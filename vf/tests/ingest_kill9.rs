//! M2 T3 kill -9 convergence test: a REAL daemon process (`CARGO_BIN_EXE_-
//! vectorfs`), real files, a real SIGKILL mid-ingest, a real restart — and
//! the no-queue design must converge to exactly-once chunks (design.md
//! §5.10.5: pending-ness is re-derived from the object listing; put_vectors
//! upsert keys make every rerun converge).
//!
//! Hermetic: the committed tiny-embedder fixture (dim=8 ONNX + tokenizer)
//! and the repo-local libonnxruntime — zero network. Both missing = panic
//! with fetch guidance, never a skip (the M2 honest-failure rule).
//!
//! Timeline:
//!   1. materialize a data root (vectorfs.toml: txt -> tiny ort embedder,
//!      16/4 token window so each file yields ~100+ chunks, 1s scan period);
//!   2. serve, drop ~150 docs into `docs/`;
//!   3. poll marker files until ≥1 INDEXED while others are still
//!      PENDING/IN_PROGRESS (the mid-flight window), then kill -9;
//!   4. restart (the LOCK object holds for its TTL after the kill — the
//!      retry loop models exactly the deployment reality of §5.1 takeover);
//!   5. poll until every marker is INDEXED;
//!   6. assert via the STORE (the ops layer's engine face): vector count ==
//!      the markers' summed chunk counts and every key unique — no
//!      duplicates, no losses.

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// environment
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vf/../ is the repo root")
        .to_path_buf()
}

fn ort_lib() -> PathBuf {
    // platform library name (CI fetches the matching asset per runner OS)
    let lib = if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    let p = repo_root().join("runtime-libs").join(lib);
    assert!(
        p.is_file(),
        "kill -9 test needs the real ONNX Runtime at {} — run \
         scripts/fetch_runtime_libs.sh onnxruntime; tests refuse to skip",
        p.display()
    );
    p
}

fn fixture_dir() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-embedder");
    assert!(
        p.join("model.onnx").is_file(),
        "tiny-embedder fixture missing under {}",
        p.display()
    );
    p
}

/// One deterministic ~1.5 KiB document (content varies per file so chunk
/// counts are not all identical).
fn doc_body(i: usize) -> String {
    let mut s = format!("document {i}: ");
    while s.len() < 1500 {
        s.push_str(&format!(
            "the quick brown fox {i} jumps over the lazy dog. "
        ));
    }
    s
}

fn config_toml(data_dir: &Path) -> String {
    format!(
        r#"version = 1
region = "us-east-1"
account_id = "000000000000"
access_key_id = "VFTESTKEY"
secret_access_key = "vf-test-secret"
models_dir = "{models}"

[extractor.plain]
kind = "txt"

[embedder.tiny]
kind = "ort"
model = "tiny-fixture"
path = "{fixture}"
offline = true

[ingest.docs]
scope = "docs/**"
extract = {{ txt = "plain" }}
chunk = {{ kind = "token-window", size = 16, overlap = 4 }}
embedders = ["tiny"]
scan_interval_secs = 1
retries = 2
index-prefix = "docs"
"#,
        models = data_dir.join(".vectorfs/models").display(),
        fixture = fixture_dir().display(),
    )
}

// ---------------------------------------------------------------------------
// daemon control
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn spawn(data_dir: &Path, port: u16, log_name: &str) -> Daemon {
        let log = data_dir.parent().unwrap().join(log_name);
        let stderr = std::fs::File::create(&log).expect("create log file");
        let child = Command::new(env!("CARGO_BIN_EXE_vectorfs"))
            .args([
                "serve",
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--port",
                &port.to_string(),
                "--no-auth",
            ])
            .env("VF_ORT_PATH", ort_lib())
            .stdout(Stdio::from(stderr.try_clone().expect("clone log")))
            .stderr(stderr)
            .spawn()
            .expect("spawn vectorfs serve");
        Daemon { child, log }
    }

    fn kill9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn dump_log_tail(&self, context: &str) {
        eprintln!("--- serve log tail ({context}, {}):", self.log.display());
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let mut buf = String::new();
            let _ = f.read_to_string(&mut buf);
            let lines: Vec<&str> = buf.lines().collect();
            for line in lines.iter().rev().take(40).rev() {
                eprintln!("    {line}");
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill9();
    }
}

/// A free TCP port (bind :0, read it, drop — the tiny race is irrelevant
/// for a test-only port).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Raw-HTTP /healthz poll (no client dependency; the route is no-auth).
fn wait_healthy(port: u16, deadline: Instant) {
    while Instant::now() < deadline {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(
                b"GET /healthz HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
            );
            let mut buf = [0u8; 512];
            if let Ok(n) = s.read(&mut buf)
                && std::str::from_utf8(&buf[..n]).is_ok_and(|r| r.contains("\"ok\":true"))
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("serve never became healthy on port {port}");
}

// ---------------------------------------------------------------------------
// marker observations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mark {
    Pending,
    InProgress,
    Indexed,
    Other,
}

fn marker_states(data_dir: &Path) -> Vec<(String, Mark, Option<u32>)> {
    let dir = data_dir.join(".vectorfs/buckets/docs/sources");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let status = v["status"].as_str().unwrap_or_default().to_string();
        let mark = match status.as_str() {
            "PENDING" => Mark::Pending,
            "IN_PROGRESS" => Mark::InProgress,
            "INDEXED" => Mark::Indexed,
            _ => Mark::Other,
        };
        let chunks = v["chunks"]["text"].as_u64().map(|c| c as u32);
        out.push((
            v["object_key"].as_str().unwrap_or("").to_string(),
            mark,
            chunks,
        ));
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

#[test]
fn kill9_mid_ingest_restart_converges_exactly_once() {
    // Enough files/chunks that the mid-flight window is comfortable on a
    // 20-core box while the whole run stays well under the CI budget.
    const N: usize = 150;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("data");
    let data_dir = work.clone(); // serve's --data-dir == the instance root
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&data_dir),
    )
    .unwrap();

    let port = free_port();

    // ---- round 1: serve, flood files, kill -9 mid-ingest ----
    let mut d1 = Daemon::spawn(&data_dir, port, "serve-round1.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(30));

    let docs = data_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    for i in 0..N {
        std::fs::write(docs.join(format!("doc-{i:03}.txt")), doc_body(i)).unwrap();
    }

    // poll for the kill window: at least one file DONE while others are not
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut killed_mid_flight = false;
    while Instant::now() < deadline {
        let states = marker_states(&data_dir);
        let done = states
            .iter()
            .filter(|(_, m, _)| *m == Mark::Indexed)
            .count();
        let open = states
            .iter()
            .any(|(_, m, _)| *m == Mark::Pending || *m == Mark::InProgress);
        if done >= 1 && open {
            killed_mid_flight = true;
            eprintln!("kill -9 at: {done} INDEXED, work still open");
            break;
        }
        if states.len() == N && !open {
            break; // finished too fast to catch mid-flight; still exercise the kill
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    d1.kill9();
    let states = marker_states(&data_dir);
    eprintln!(
        "after kill -9: {} markers, {} INDEXED",
        states.len(),
        states
            .iter()
            .filter(|(_, m, _)| *m == Mark::Indexed)
            .count()
    );
    // The window MUST have been caught with this file/chunk volume; if it
    // ever flakes on slower machines, raise N — do not weaken the assert.
    if !killed_mid_flight {
        d1.dump_log_tail("round 1");
        panic!("ingest finished before the kill -9 landed (raise N or chunk volume)");
    }
    assert!(states.iter().any(|(_, m, _)| *m == Mark::Indexed));

    // ---- round 2: restart (LOCK TTL may hold us off briefly — retry) ----
    let mut d2 = None;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let mut d = Daemon::spawn(&data_dir, port, "serve-round2.log");
        // give it a few seconds: exit 3 (lock held) is quick, healthy is quick
        let probe = Instant::now() + Duration::from_secs(10);
        let mut healthy = false;
        while Instant::now() < probe {
            if let Ok(Some(status)) = d.child.try_wait() {
                eprintln!("restart attempt exited with {status} (lock TTL?) — retrying");
                break;
            }
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                let _ =
                    s.write_all(b"GET /healthz HTTP/1.1\r\nhost: l\r\nconnection: close\r\n\r\n");
                let mut buf = [0u8; 512];
                if let Ok(n) = s.read(&mut buf)
                    && std::str::from_utf8(&buf[..n]).is_ok_and(|r| r.contains("\"ok\":true"))
                {
                    healthy = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if healthy {
            d2 = Some(d);
            break;
        }
        d.kill9();
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut d2 = match d2 {
        Some(d) => d,
        None => {
            let ghost = Daemon {
                child: Command::new("true").spawn().unwrap(),
                log: data_dir.parent().unwrap().join("serve-round2.log"),
            };
            ghost.dump_log_tail("failed restarts");
            panic!("serve never restarted (LOCK TTL window exceeded 120s?)");
        }
    };

    // ---- converge: every marker INDEXED ----
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let states = marker_states(&data_dir);
        let done = states
            .iter()
            .filter(|(_, m, _)| *m == Mark::Indexed)
            .count();
        if states.len() == N && done == N {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not all {N} INDEXED after restart: {}/{} (states: {:?})",
            done,
            states.len(),
            states
                .iter()
                .map(|(k, m, _)| (k.clone(), *m))
                .collect::<Vec<_>>()
        );
        let _ = &d2;
        std::thread::sleep(Duration::from_millis(200));
    }

    // ---- exactly-once: store count == summed marker chunks, keys unique ----
    let states = marker_states(&data_dir);
    let expected: u32 = states
        .iter()
        .map(|(_, _, c)| c.expect("INDEXED markers carry chunk counts"))
        .sum();
    assert!(expected > 0);
    d2.kill9(); // stop the writer before opening the store read-only

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let storage = vectorfs::store::Storage::file(&data_dir);
        // find the derived index (the one stamped x_vf_derived)
        let indexes = storage.list_indexes("docs").await.unwrap();
        assert!(!indexes.is_empty(), "derived index exists: {indexes:?}");
        let derived = indexes
            .iter()
            .find(|m| m.x_vf_derived.is_some())
            .expect("an ingest-derived index");
        assert!(
            derived.index_name.starts_with("docs-"),
            "{}",
            derived.index_name
        );
        assert_eq!(derived.dimension, 8);
        let count = storage
            .count_vectors("docs", &derived.index_name)
            .await
            .unwrap();
        assert_eq!(
            count as u32, expected,
            "vector count must equal the markers' summed chunk counts (no dup, no loss)"
        );
        // and the keys are unique (an upsert replay bug would still pass the
        // count but never duplicate keys — assert the stronger property)
        let rows = storage
            .scan_vectors("docs", &derived.index_name, None)
            .await
            .unwrap();
        assert_eq!(rows.len() as u32, expected);
        let mut keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len() as u32, expected, "duplicate chunk keys present");
        // spot-check the chunk key format and metadata join key
        let hash = {
            use sha2::Digest;
            let d = sha2::Sha256::digest(b"docs/doc-000.txt");
            let mut h = String::new();
            for b in d.iter().take(8) {
                h.push_str(&format!("{b:02x}"));
            }
            h
        };
        let first = rows
            .iter()
            .find(|r| r.key.starts_with(&hash))
            .expect("doc-000 chunks present");
        assert!(first.key.contains("-text-"), "{}", first.key);
        let meta = first.metadata.as_ref().expect("chunk metadata");
        assert_eq!(meta["x-vf-src-name"], "docs/doc-000.txt");
        assert_eq!(meta["x-vf-src-uri"].as_str().unwrap(), hash);
        assert_eq!(meta["x-vf-modality"], "text");
    });
}
