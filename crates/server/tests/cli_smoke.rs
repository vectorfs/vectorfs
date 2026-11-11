//! M2 T6 CLI integration smoke: a REAL `vectorfs serve` daemon (SigV4 auth
//! ON, tiny-embedder fixture pipeline) exercised end to end through the REAL
//! `vf` client binary (from the sibling vectorfs-client crate — see
//! [`vf_bin`]) — every 附录E.4 verb:
//!
//!   put (foreground wait to INDEXED, and --async), get (byte round-trip),
//!   sources list / sources status, search (hit + location 回源), rm (202
//!   DELETING), reindex (jobId), jobs (the InFlight live-progress table),
//!   status (/health + GetIngestStatus) — plus the FAILED exit-1 path
//!   (non-UTF-8 source) and the no-credentials structured error.
//!
//! Hermetic: committed tiny-embedder fixture + repo-local libonnxruntime
//! (VF_ORT_PATH), tmp data root, free TCP port; no network beyond loopback.

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const AK: &str = "VFTESTKEY";
const SK: &str = "vf-test-secret";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/server/../../ is the repo root")
        .to_path_buf()
}

/// The `vf` client binary lives in the vectorfs-CLIENT crate since the
/// three-crate split, so this test crate has no `CARGO_BIN_EXE_vf`. Both
/// bins land in the SAME target/<profile>/ directory when the workspace
/// builds, so derive the client's path from this crate's own `vectorfs`
/// binary — and fail loud (not skip) if a package-scoped run left it unbuilt.
fn vf_bin() -> PathBuf {
    let vf = PathBuf::from(env!("CARGO_BIN_EXE_vectorfs"))
        .parent()
        .expect("bin dir")
        .join("vf");
    assert!(
        vf.is_file(),
        "vf client binary missing at {} — build the workspace: cargo test --workspace",
        vf.display()
    );
    vf
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
        "CLI smoke needs the real ONNX Runtime at {} — run \
         scripts/fetch_runtime_libs.sh onnxruntime; tests refuse to skip",
        p.display()
    );
    p
}

fn fixture_dir() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-embedder");
    assert!(p.join("model.onnx").is_file(), "fixture missing");
    p
}

fn config_toml(data_dir: &Path) -> String {
    config_toml_scoped(data_dir, true)
}

/// The S3-forms smoke's variant: NO `scope` (the whole instance root, the
/// preset/docker shape) — so `bucket/key` paths land wherever the operator
/// puts them, which is what the destination-form semantics assert.
fn config_toml_unscoped(data_dir: &Path) -> String {
    config_toml_scoped(data_dir, false)
}

fn config_toml_scoped(data_dir: &Path, scoped: bool) -> String {
    // v3 (R5): the claim filter replaces scope — keys under docs/ only
    let scope = if scoped {
        "include = [\"docs/**\"]\n"
    } else {
        ""
    };
    format!(
        r#"version = 1
region = "us-east-1"
account_id = "000000000000"
access_key_id = "{AK}"
secret_access_key = "{SK}"
models_dir = "{models}"

[extractor.plain]
kind = "txt"

[embedder.tiny]
kind = "ort"
model = "tiny-fixture"
path = "{fixture}"
offline = true

[ingest.docs]
{scope}extract = {{ txt = "plain" }}
chunk = {{ kind = "token-window", size = 32, overlap = 8 }}
embedders = ["tiny"]
scan_interval_secs = 1
"#,
        models = data_dir.join(".vectorfs/models").display(),
        fixture = fixture_dir().display(),
    )
}

// ---------------------------------------------------------------------------
// daemon + client control
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn spawn(data_dir: &Path, port: u16, log_name: &str) -> Daemon {
        Daemon::spawn_with(data_dir, port, log_name, &[])
    }

    fn spawn_with(data_dir: &Path, port: u16, log_name: &str, extra: &[&str]) -> Daemon {
        let log = data_dir.parent().unwrap().join(log_name);
        let stderr = std::fs::File::create(&log).expect("create log file");
        let child = Command::new(env!("CARGO_BIN_EXE_vectorfs"))
            .args([
                "serve",
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--port",
                &port.to_string(),
            ])
            .args(extra)
            .env("VF_ORT_PATH", ort_lib())
            .stdout(Stdio::from(stderr.try_clone().expect("clone log")))
            .stderr(stderr)
            .spawn()
            .expect("spawn vectorfs serve");
        Daemon { child, log }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Wait for the process to exit on its own; returns (exit code, log).
    fn wait_exit(&mut self, deadline: Instant) -> (Option<i32>, String) {
        loop {
            match self.child.try_wait().expect("poll child") {
                Some(status) => {
                    let mut buf = String::new();
                    if let Ok(mut f) = std::fs::File::open(&self.log) {
                        let _ = f.read_to_string(&mut buf);
                    }
                    return (status.code(), buf);
                }
                None if Instant::now() >= deadline => {
                    self.kill();
                    panic!("serve did not exit before the deadline");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn dump_log_tail(&self, context: &str) {
        eprintln!("--- serve log tail ({context}, {}):", self.log.display());
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let mut buf = String::new();
            let _ = f.read_to_string(&mut buf);
            let lines: Vec<&str> = buf.lines().collect();
            for line in lines.iter().rev().take(30).rev() {
                eprintln!("    {line}");
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Raw-HTTP /health poll — the no-auth probe itself is part of the smoke.
fn wait_healthy(port: u16, deadline: Instant) {
    while Instant::now() < deadline {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"GET /health HTTP/1.1\r\nhost: l\r\nconnection: close\r\n\r\n");
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

/// An EMPTY XDG_CONFIG_HOME for [`vf`]: the config-file layer must find
/// nothing. Without this the strip_creds probe mixes sources (endpoint from
/// env, credentials falling through to a developer's real
/// ~/.config/vectorfs/config.toml → 403 against the test daemon's own AK
/// namespace). CI never sees it — it has no config; dev machines do.
fn empty_config_home() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("vf-cli-smoke-no-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("create empty XDG_CONFIG_HOME");
        d
    })
    .clone()
}

/// One `vf` invocation with the endpoint/credentials environment set.
fn vf(port: u16, args: &[&str], strip_creds: bool) -> Output {
    let mut cmd = Command::new(vf_bin());
    cmd.args(args)
        .env("VF_ENDPOINT", format!("http://127.0.0.1:{port}"))
        .env("VF_ACCESS_KEY", AK)
        .env("VF_SECRET_KEY", SK)
        .env("XDG_CONFIG_HOME", empty_config_home());
    if strip_creds {
        cmd.env_remove("VF_ACCESS_KEY").env_remove("VF_SECRET_KEY");
    }
    cmd.output().expect("run vf")
}

/// The config.toml layer's shape: vf with an ISOLATED HOME (so the config
/// file lives under the test's tmp, and a developer's real one cannot leak
/// in) and every VF_* var scrubbed — only flags and the file may carry
/// connection state.
fn vf_env_home(home: &Path, args: &[&str]) -> Output {
    Command::new(vf_bin())
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("VF_ENDPOINT")
        .env_remove("VF_ACCESS_KEY")
        .env_remove("VF_SECRET_KEY")
        .env_remove("VF_REGION")
        .output()
        .expect("run vf")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn wait_vf_indexed(port: u16, key: &str, deadline: Instant) {
    while Instant::now() < deadline {
        let o = vf(port, &["sources", "status", key, "--bucket", "docs"], false);
        if out(&o).contains("status               : INDEXED") {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("{key} did not reach INDEXED");
}

// ---------------------------------------------------------------------------
// the smoke
// ---------------------------------------------------------------------------

#[test]
fn cli_full_command_smoke() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&data_dir),
    )
    .unwrap();

    // upload corpus
    let fox = work.join("fox.txt");
    std::fs::write(&fox, "the quick brown fox jumps over the lazy dog").unwrap();
    let sea = work.join("sea.txt");
    std::fs::write(&sea, "whales and dolphins swim in the deep blue ocean").unwrap();

    let port = free_port();
    let daemon = Daemon::spawn(&data_dir, port, "serve.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));

    // ---- put, foreground: waits for INDEXED (exit 0, stdout result) ----
    let o = vf(
        port,
        &[
            "put",
            fox.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/fox.txt",
            "--timeout",
            "120",
        ],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(out(&o).starts_with("INDEXED\tdocs/fox.txt"), "{}", out(&o));
    assert!(err(&o).contains("docs/fox.txt -> INDEXED"), "{}", err(&o));

    // ---- put --async: 202-style immediate return, scan finishes it ----
    let o = vf(
        port,
        &[
            "put",
            sea.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/sea.txt",
            "--async",
        ],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(out(&o).starts_with("PENDING\tdocs/sea.txt"), "{}", out(&o));
    wait_vf_indexed(
        port,
        "docs/sea.txt",
        Instant::now() + Duration::from_secs(120),
    );

    // ---- get: byte-for-byte round trip through GetSourceData ----
    let roundtrip = work.join("roundtrip.txt");
    let o = vf(
        port,
        &[
            "get",
            "docs/fox.txt",
            "--bucket",
            "docs",
            "--out",
            roundtrip.to_str().unwrap(),
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    assert!(err(&o).contains("vf: got docs/fox.txt"), "{}", err(&o));
    assert_eq!(
        std::fs::read(&roundtrip).unwrap(),
        std::fs::read(&fox).unwrap(),
        "byte round trip"
    );

    // ---- F7: the two-positional misuse (`vf get docs "key"` — space
    // where the path form wants a '/') is a clap usage error that now
    // TEACHES the path form instead of leaving the user at --help ----
    let o = vf(port, &["get", "docs", "fox.txt"], false);
    assert_eq!(o.status.code(), Some(2), "stderr:\n{}", err(&o));
    assert!(
        err(&o).contains("unexpected argument"),
        "stderr:\n{}",
        err(&o)
    );
    assert!(
        err(&o).contains("note: get takes ONE '<bucket>/<key>' path positional"),
        "stderr:\n{}",
        err(&o)
    );
    // (the legitimate single-positional path form itself is exercised by
    // the S3-forms smoke below; this fixture's keys carry a docs/ prefix
    // INSIDE the bucket, so `docs/fox.txt` would split to key fox.txt)

    // ---- sources list / status ----
    let o = vf(port, &["sources", "list", "--bucket", "docs"], false);
    assert!(o.status.success(), "stdout:\n{}", out(&o));
    assert!(
        out(&o).contains("docs/fox.txt") && out(&o).contains("docs/sea.txt"),
        "{}",
        out(&o)
    );
    assert!(out(&o).contains("INDEXED"), "{}", out(&o));
    let o = vf(
        port,
        &["sources", "list", "--bucket", "docs", "--json"],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["count"], 2);

    let o = vf(
        port,
        &["sources", "status", "docs/fox.txt", "--bucket", "docs"],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("INDEXED"), "{}", out(&o));
    assert!(out(&o).contains("chunks"), "{}", out(&o));

    // ---- search: hits with location pointing back at the source ----
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--top-k",
            "3",
        ],
        false,
    );
    assert!(o.status.success(), "stdout:\n{}", out(&o));
    assert!(out(&o).contains("docs/fox.txt"), "{}", out(&o));
    // T13: the table carries the COORD column, and a txt (pageless) corpus
    // shows LINE cells on its rows (the p/L/§ compact forms; the md-source
    // product-domain caveat lives in the verb docs)
    assert!(out(&o).contains("COORD"), "header: {}", out(&o));
    assert!(
        out(&o).contains("L1"),
        "txt hits carry line coords: {}",
        out(&o)
    );
    assert!(
        !out(&o).contains(" p1 "),
        "page coords must stay absent for a pageless corpus: {}",
        out(&o)
    );

    // T12: repeatable --kind passes through to the wire's includeKinds (a
    // txt corpus has no toc chunks - the pin is the flag plumbing + the
    // wire accepting the admitted vocabulary, exit 0)
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--top-k",
            "3",
            "--kind",
            "toc",
            "--kind",
            "legend",
        ],
        false,
    );
    assert!(o.status.success(), "stdout:\n{}", out(&o));

    // (the tiny fixture embeds only a 16-word vocab — for OOV terms like
    // this query several chunks tie at distance 0.0 and the tie breaks by
    // key ascending, so the ASSERTION TARGET is the hit SHAPE, the 回源
    // location, and the fox document being PRESENT — never which tied
    // chunk lands first; T12 note: the always-on kind-exclusion filter
    // puts every Search on the filtered retrieval path, whose tie paging
    // is deterministic per query shape but not identical to the unfiltered
    // one this test was originally pinned against)
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--top-k",
            "3",
            "--json",
        ],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["schema_version"], 1, "{}", out(&o));
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{}", out(&o));
    assert!(
        hits.iter().any(|h| h["location"]["key"] == "docs/fox.txt"),
        "{}",
        out(&o)
    );
    assert_eq!(hits[0]["content"]["type"], "TEXT", "{}", out(&o));
    assert!(hits[0]["content"]["text"].is_string(), "{}", out(&o));
    assert!(hits[0]["score"].as_f64().is_some(), "{}", out(&o));
    assert!(
        hits[0]["index"].as_str().unwrap().starts_with("tiny-"),
        "{}",
        out(&o)
    );

    // ---- T15: --source scopes by NAME — the server translates to the
    // x-vf-src-uri hash (no hash digging), and a name that matches no
    // indexed source surfaces the note instead of a silent empty list ----
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--source",
            "docs/fox.txt",
            "--json",
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{}", out(&o));
    assert!(
        hits.iter().all(|h| h["location"]["key"] == "docs/fox.txt"),
        "every hit from the named source: {}",
        out(&o)
    );
    assert!(
        v.get("note").is_none(),
        "hits present: no note: {}",
        out(&o)
    );
    // repeatable --source ⇒ $in: both sources come back
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--source",
            "docs/fox.txt",
            "--source",
            "docs/sea.txt",
            "--json",
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let mut seen: Vec<String> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["location"]["key"].as_str().map(str::to_string))
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen, vec!["docs/fox.txt", "docs/sea.txt"], "{}", out(&o));
    // a typo'd name: zero hits + the note rides the envelope (and the
    // human table prints it on stderr)
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--source",
            "docs/foxx.txt",
            "--json",
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["hits"].as_array().unwrap().len(), 0, "{}", out(&o));
    assert!(
        v["note"]
            .as_str()
            .unwrap()
            .contains("matched no indexed sources"),
        "{}",
        out(&o)
    );
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
            "docs",
            "--source",
            "docs/foxx.txt",
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    assert!(
        err(&o).contains("matched no indexed sources"),
        "human mode carries the note on stderr: {}",
        err(&o)
    );

    // ---- rm: 202 DELETING, idempotent ----
    let o = vf(port, &["rm", "docs/sea.txt", "--bucket", "docs"], false);
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(out(&o).starts_with("DELETING\tdocs/sea.txt"), "{}", out(&o));
    let o = vf(port, &["rm", "docs/sea.txt", "--bucket", "docs"], false);
    assert!(o.status.success(), "second rm is idempotent: {}", out(&o));

    // ---- T7 GC convergence: within a couple of scan periods the source is
    // fully reaped (chunks + marker) — `vf sources list` forgets it and the
    // KB deleted counter advances (202 DELETING -> 404) ----
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let o = vf(
            port,
            &["sources", "list", "--bucket", "docs", "--json"],
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
        if v["count"] == 1 && !out(&o).contains("sea.txt") {
            break;
        }
        assert!(Instant::now() < deadline, "sea.txt not reaped: {}", out(&o));
        std::thread::sleep(Duration::from_millis(300));
    }
    let o = vf(port, &["status", "--json"], false);
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(
        v["ingest"]["buckets"][0]["stats"]["numberOfDocumentsDeleted"],
        1,
        "{}",
        out(&o)
    );

    // ---- reindex: jobId printed ----
    let o = vf(port, &["reindex", "--bucket", "docs"], false);
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    let job = out(&o).trim().to_string();
    assert_eq!(job.len(), 32, "uuid hex: {job}");
    assert!(job.bytes().all(|b| b.is_ascii_hexdigit()), "{job}");
    // the flipped marker re-converges to INDEXED
    wait_vf_indexed(
        port,
        "docs/fox.txt",
        Instant::now() + Duration::from_secs(120),
    );

    // ---- status: health + ingest summary ----
    let o = vf(port, &["status"], false);
    assert!(o.status.success(), "stdout:\n{}", out(&o));
    assert!(out(&o).contains("health"), "{}", out(&o));
    assert!(out(&o).contains("bucket docs"), "{}", out(&o));
    let o = vf(port, &["status", "--json"], false);
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["health"]["ok"], true);
    assert_eq!(v["ingest"]["buckets"][0]["bucket"], "docs");
    assert_eq!(v["ingest"]["buckets"][0]["counts"]["INDEXED"], 1);

    // ---- FAILED path: corrupt gzip -> exit 1 + statusReason ----
    // (M3-T7 update: the non-UTF-8 trigger this block used before — a
    // \xff\xfe… file — now rides the charset normalizer's BOM
    // identification and INDEXES; a corrupt gzip is the deterministic
    // normalize dead end that still parks, same CLI contract.)
    let bad = work.join("bad.txt.gz");
    let mut gz_noise = vec![0x1f, 0x8b, 0x08, 0x00];
    gz_noise.extend_from_slice(b"definitely not a deflate stream");
    std::fs::write(&bad, &gz_noise).unwrap();
    let o = vf(
        port,
        &[
            "put",
            bad.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/bad.txt.gz",
            "--timeout",
            "60",
        ],
        false,
    );
    assert_eq!(o.status.code(), Some(1), "FAILED exits 1: {}", out(&o));
    assert!(
        out(&o).starts_with("FAILED\tdocs/bad.txt.gz"),
        "{}",
        out(&o)
    );
    assert!(err(&o).contains("statusReason"), "{}", err(&o));

    // ---- no credentials: health-only degradation (M2 T8 container
    // HEALTHCHECK contract): `vf status` probes the unsigned GET /health and
    // exits 0 when the daemon is up — the image's HEALTHCHECK runs exactly
    // this shape with no VF_* env (§5.10.7). Signed DEPTH verbs (put/search/
    // …) still hard-fail on missing credentials (their own tests above).
    let o = vf(port, &["status"], true);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("health-only probe"), "{}", out(&o));
    assert!(out(&o).contains("health"), "{}", out(&o));

    // ---- config.toml layer (2026-11-05 ruling): `vf config init` writes
    // the file seeded from flags, a signed verb then resolves with NO
    // flags and NO VF_* env (isolated HOME — the file is the ONLY
    // connection state), and re-init without --force refuses ----
    let cfg_home = work.join("cfg-home");
    std::fs::create_dir_all(&cfg_home).unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let o = vf_env_home(
        &cfg_home,
        &[
            "config",
            "init",
            "--endpoint",
            &endpoint,
            "--access-key",
            AK,
            "--secret-key",
            SK,
        ],
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    let cfg_path = cfg_home.join(".config/vectorfs/config.toml");
    let cfg_text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        cfg_text.contains(&format!("endpoint = \"{endpoint}\"")),
        "{cfg_text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = cfg_path.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "secret-bearing config must be 0600");
    }
    let o = vf_env_home(&cfg_home, &["sources", "list", "--bucket", "docs"]);
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(out(&o).contains("fox.txt"), "{}", out(&o));
    let o = vf_env_home(&cfg_home, &["config", "init"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("CONFIG_EXISTS"), "{}", err(&o));

    daemon.dump_log_tail("end of smoke (informational)");
}

/// The S3-CLI-style argument forms (2026-11-05 adjudication: bucket/key
/// PATHS alongside the --bucket flags, `ls`/`cat` shorthands — the aws s3 /
/// mc / s5cmd shapes): every form is exercised against a REAL daemon, plus
/// the USAGE error shapes for mixing the two spellings.
#[test]
fn cli_s3_style_forms_smoke() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml_unscoped(&data_dir),
    )
    .unwrap();

    let fox = work.join("fox.txt");
    std::fs::write(&fox, "the quick brown fox jumps over the lazy dog").unwrap();

    let port = free_port();
    let daemon = Daemon::spawn(&data_dir, port, "serve.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));

    // ---- ls with no argument: the bucket inventory (aws `s3 ls` shape) ----
    let o = vf(port, &["ls"], false);
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    assert!(out(&o).contains("docs"), "{}", out(&o));
    assert!(out(&o).contains("INDEXED=0"), "{}", out(&o));
    let o = vf(port, &["ls", "--json"], false);
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["buckets"][0]["bucket"], "docs", "{}", out(&o));

    // ---- put with a `bucket[/prefix]/` destination: key = pre/<name> ----
    let o = vf(
        port,
        &[
            "put",
            fox.to_str().unwrap(),
            "docs/stories/",
            "--timeout",
            "120",
        ],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(
        out(&o).starts_with("INDEXED\tstories/fox.txt"),
        "destination prefix becomes the key: {}",
        out(&o)
    );

    // ---- ls <bucket>: the sources table (shares sources list rendering) ----
    let o = vf(port, &["ls", "docs"], false);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("stories/fox.txt"), "{}", out(&o));
    assert!(out(&o).contains("KEY"), "{}", out(&o));
    // ---- ls <bucket/prefix>: client-side key filter ----
    let o = vf(port, &["ls", "docs/stories"], false);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("stories/fox.txt"), "{}", out(&o));
    let o = vf(port, &["ls", "docs/nowhere"], false);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!out(&o).contains("fox.txt"), "prefix filters: {}", out(&o));

    // ---- get/cat path form + cat's stdout bytes ----
    let roundtrip = work.join("rt.txt");
    let o = vf(
        port,
        &[
            "get",
            "docs/stories/fox.txt",
            "--out",
            roundtrip.to_str().unwrap(),
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    assert!(err(&o).contains("vf: got stories/fox.txt"), "{}", err(&o));
    assert_eq!(
        std::fs::read(&roundtrip).unwrap(),
        std::fs::read(&fox).unwrap(),
        "path-form get round trip"
    );
    let o = vf(port, &["cat", "docs/stories/fox.txt"], false);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "the quick brown fox jumps over the lazy dog");

    // ---- the PRE-S3-forms contract survives: with --bucket present the
    // positional is the full key VERBATIM, even when it contains '/' (a
    // `bucket/key`-shaped key stays addressable through the flag form) ----
    let back = work.join("back.txt");
    let o = vf(
        port,
        &[
            "get",
            "stories/fox.txt",
            "--bucket",
            "docs",
            "--out",
            back.to_str().unwrap(),
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    assert!(err(&o).contains("vf: got stories/fox.txt"), "{}", err(&o));

    // ---- search / reindex positional bucket ----
    let o = vf(
        port,
        &["search", "quick brown fox", "docs", "--top-k", "1"],
        false,
    );
    assert!(o.status.success(), "stdout:\n{}", out(&o));
    assert!(out(&o).contains("stories/fox.txt"), "{}", out(&o));
    let o = vf(port, &["reindex", "docs"], false);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o).trim().len(), 32, "jobId hex: {}", out(&o));
    wait_vf_indexed(
        port,
        "stories/fox.txt",
        Instant::now() + Duration::from_secs(120),
    );

    // ---- rm path form ----
    let o = vf(port, &["rm", "docs/stories/fox.txt"], false);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).starts_with("DELETING\tstories/fox.txt"),
        "{}",
        out(&o)
    );

    // ---- USAGE error shapes (the put destination never mixes with
    // --bucket; a keyless bare positional is refused with a repair hint) ----
    let o = vf(port, &["get", "fox.txt"], false);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("no '/' to split"), "{}", err(&o));
    let o = vf(
        port,
        &["put", fox.to_str().unwrap(), "docs/", "--bucket", "docs"],
        false,
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("second one"), "{}", err(&o));

    daemon.dump_log_tail("end of s3-forms smoke (informational)");
}

/// SIGKILL leaves the LOCK object behind (a killed process cannot release
/// it); the documented operator remedy is removing it while nothing runs.
/// The reindex flow restarts daemons on ONE data root, so it applies that
/// remedy between kills instead of waiting out the 60s TTL.
fn remove_stale_lock(data_dir: &Path) {
    // v3: the per-bucket LOCK of the reindex flow's bucket
    let lock = data_dir.join("docs-vf/LOCK");
    if lock.exists() {
        std::fs::remove_file(&lock).expect("remove stale LOCK");
    }
}

/// T7 acceptance run (real daemon, real client): chunk-size config change →
/// plain serve REFUSES (message names `--reindex`) → `serve --reindex`
/// clears + re-embeds everything → a subsequent PLAIN serve starts clean.
#[test]
fn cli_serve_reindex_flow() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&data_dir),
    )
    .unwrap();
    let long = work.join("long.txt");
    std::fs::write(
        &long,
        "alpha beta gamma delta epsilon zeta eta theta ".repeat(12),
    )
    .unwrap();

    // (1) ingest under size 32
    let port = free_port();
    let mut daemon = Daemon::spawn(&data_dir, port, "reindex-serve1.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));
    let o = vf(
        port,
        &[
            "put",
            long.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/long.txt",
            "--timeout",
            "120",
        ],
        false,
    );
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    daemon.kill();
    remove_stale_lock(&data_dir);

    // (2) change the chunker: size 32 -> 96 (fingerprint moves)
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&data_dir).replace("size = 32", "size = 96"),
    )
    .unwrap();

    // (3) plain serve refuses, naming --reindex
    let port = free_port();
    let mut refused = Daemon::spawn(&data_dir, port, "reindex-refused.log");
    let (code, log) = refused.wait_exit(Instant::now() + Duration::from_secs(30));
    assert_eq!(code, Some(2), "gate exits 2:\n{log}");
    assert!(log.contains("fingerprint mismatch"), "{log}");
    assert!(log.contains("serve --reindex"), "{log}");

    // (4) serve --reindex: starts, clears, re-embeds, serves normally
    let port = free_port();
    let mut daemon = Daemon::spawn_with(&data_dir, port, "reindex-serve2.log", &["--reindex"]);
    wait_healthy(port, Instant::now() + Duration::from_secs(60));
    wait_vf_indexed(
        port,
        "docs/long.txt",
        Instant::now() + Duration::from_secs(120),
    );
    // the reindex job reports done and the rewrite counted as modified
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let o = vf(port, &["status", "--json"], false);
        let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
        let b = &v["ingest"]["buckets"][0];
        if b["reindex"]["status"] == "done"
            && b["stats"]["numberOfModifiedDocumentsIndexed"]
                .as_u64()
                .unwrap_or(0)
                >= 1
        {
            assert_eq!(b["counts"]["INDEXED"], 1, "{b}");
            assert_eq!(b["stats"]["numberOfNewDocumentsIndexed"], 0, "{b}");
            break;
        }
        assert!(Instant::now() < deadline, "reindex job not done: {b}");
        std::thread::sleep(Duration::from_millis(300));
    }
    daemon.kill();
    remove_stale_lock(&data_dir);

    // (5) a PLAIN serve now passes the gate
    let port = free_port();
    let mut daemon = Daemon::spawn(&data_dir, port, "reindex-serve3.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));
    let o = vf(port, &["sources", "list", "--bucket", "docs"], false);
    assert!(
        o.status.success() && out(&o).contains("docs/long.txt"),
        "{}",
        out(&o)
    );
    daemon.kill();
}

// ---------------------------------------------------------------------------
// M3-T4: 以图搜图 over the REAL daemon + REAL vf — the image FILE extractor
// + tiny-siglip2 pipeline (the image-search preset shape), put PNG sources,
// `vf search --image` same-image hit, text query onto the image index, and
// the CLI error faces.
// ---------------------------------------------------------------------------

/// tiny-siglip2 fixture (the siglip2 kind's committed hermetic model).
fn siglip2_fixture_dir() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-siglip2");
    assert!(
        p.join("vision.onnx").is_file(),
        "fixture missing: {}",
        p.display()
    );
    p
}

fn image_config_toml(data_dir: &Path) -> String {
    format!(
        r#"version = 1
region = "us-east-1"
account_id = "000000000000"
access_key_id = "{AK}"
secret_access_key = "{SK}"
models_dir = "{models}"

[extractor.imgs]
kind = "image"

[embedder.clip]
kind = "siglip2"
model = "tiny-siglip2"
path = "{fixture}"
offline = true

[ingest.docs]
include = ["docs/**"]
extract = {{ img = "imgs" }}
chunk = {{ kind = "token-window", size = 32, overlap = 8 }}
embedders = ["clip"]
scan_interval_secs = 1
"#,
        models = data_dir.join(".vectorfs/models").display(),
        fixture = siglip2_fixture_dir().display(),
    )
}

/// Solid-color 8x8 PNGs (deterministic hand-encoded fixtures — the CLI
/// reads a PATH, so the test writes these to the work dir).
const RED_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x02, 0x00, 0x00, 0x00, 0x4b, 0x6d, 0x29,
    0xdc, 0x00, 0x00, 0x00, 0x11, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0x80,
    0x15, 0x31, 0x0c, 0x2d, 0x09, 0x00, 0x28, 0xff, 0x3f, 0xc1, 0xce, 0x77, 0xc8, 0x4f, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
const BLUE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x02, 0x00, 0x00, 0x00, 0x4b, 0x6d, 0x29,
    0xdc, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x60, 0xf8, 0x8f,
    0x03, 0x0d, 0x29, 0x09, 0x00, 0xa9, 0x70, 0x3f, 0xc1, 0xfc, 0xcb, 0x45, 0xfc, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn cli_image_search_smoke() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        image_config_toml(&data_dir),
    )
    .unwrap();

    let red = work.join("red.png");
    std::fs::write(&red, RED_PNG).unwrap();
    let blue = work.join("blue.png");
    std::fs::write(&blue, BLUE_PNG).unwrap();

    let port = free_port();
    let daemon = Daemon::spawn(&data_dir, port, "image-serve.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));

    // ---- put two PNG sources: the image FILE route indexes each as one
    // atomic image chunk ----
    for (path, key) in [(&red, "docs/red.png"), (&blue, "docs/blue.png")] {
        let o = vf(
            port,
            &[
                "put",
                path.to_str().unwrap(),
                "--bucket",
                "docs",
                "--key",
                key,
                "--timeout",
                "120",
            ],
            false,
        );
        assert!(
            o.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            out(&o),
            err(&o)
        );
        assert!(
            out(&o).starts_with(&format!("INDEXED\t{key}")),
            "{}",
            out(&o)
        );
    }

    // daemon stderr's progress line counts ALL modalities (M3-T5 fix): an
    // image-only source logs chunks=1 — the pre-multimodal "text entry only"
    // read underreported them as chunks=0
    {
        let log = std::fs::read_to_string(&daemon.log).unwrap_or_default();
        for key in ["docs/red.png", "docs/blue.png"] {
            assert!(
                log.contains(&format!("ingest: {key} -> INDEXED (chunks=1)")),
                "progress line must count image chunks for {key}:\n{log}"
            );
        }
    }

    // ---- 以图搜图: querying with red's own file hits red first (same
    // bytes -> same preprocessing -> distance ~0), 回源 in the table ----
    let o = vf(
        port,
        &[
            "search",
            "--image",
            red.to_str().unwrap(),
            "docs",
            "--top-k",
            "2",
        ],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(out(&o).contains("docs/red.png"), "{}", out(&o));
    assert!(
        out(&o).contains("docs/blue.png"),
        "both sources rank: {}",
        out(&o)
    );

    // the --json contract: IMAGE content type, image chunk key, the image
    // query echoed by local identity (path + mime), single-route distance
    let o = vf(
        port,
        &[
            "search",
            "--image",
            red.to_str().unwrap(),
            "docs",
            "--top-k",
            "1",
            "--json",
        ],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["schema_version"], 1, "{}", out(&o));
    assert_eq!(v["query"]["image"]["mime"], "image/png", "{}", out(&o));
    assert!(
        v["query"]["image"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("red.png")),
        "{}",
        out(&o)
    );
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{}", out(&o));
    let hit = &hits[0];
    assert_eq!(hit["location"]["key"], "docs/red.png", "{}", out(&o));
    assert_eq!(hit["content"]["type"], "IMAGE", "{}", out(&o));
    assert!(hit["content"].get("text").is_none(), "{}", out(&o));
    assert!(
        hit["key"].as_str().unwrap().contains("-image-"),
        "{}",
        out(&o)
    );
    assert_eq!(hit["metadata"]["x-vf-modality"], "image", "{}", out(&o));
    assert!(
        hit["score"].as_f64().unwrap() > 0.99,
        "identical bytes => distance ~0: {}",
        out(&o)
    );

    // ---- 文字搜图: a TEXT query reaches the IMAGE index through siglip2's
    // text tower (the declared [Image, Text] query side, exercised for
    // real) and returns image chunks ----
    let o = vf(
        port,
        &["search", "hello vectorfs", "docs", "--top-k", "2", "--json"],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    assert!(
        !hits.is_empty(),
        "text query must hit the image index: {}",
        out(&o)
    );
    for h in &hits {
        assert_eq!(h["content"]["type"], "IMAGE", "{}", out(&o));
        assert_eq!(h["metadata"]["x-vf-modality"], "image", "{}", out(&o));
    }

    // ---- CLI error faces (structured, exit-coded) ----
    // unsupported extension: usage error BEFORE any network
    let gif = work.join("photo.gif");
    std::fs::write(&gif, b"GIF89a").unwrap();
    let o = vf(
        port,
        &["search", "--image", gif.to_str().unwrap(), "docs"],
        false,
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(
        err(&o).contains("not a supported image extension"),
        "{}",
        err(&o)
    );
    // missing file: read error, exit 1
    let o = vf(
        port,
        &[
            "search",
            "--image",
            work.join("nope.png").to_str().unwrap(),
            "docs",
        ],
        false,
    );
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("READ_FAILED"), "{}", err(&o));
    assert!(err(&o).contains("cannot read"), "{}", err(&o));
    // text + --image: mutually exclusive usage error (two positionals
    // beside --image are an unambiguous text query AND an image query)
    let o = vf(
        port,
        &[
            "search",
            "some text",
            "docs",
            "--image",
            red.to_str().unwrap(),
        ],
        false,
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("mutually exclusive"), "{}", err(&o));
    // no query at all: usage error naming both forms
    let o = vf(port, &["search", "--bucket", "docs"], false);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("no query given"), "{}", err(&o));

    daemon.dump_log_tail("end of image smoke (informational)");
}

// ---------------------------------------------------------------------------
// T14 content addressing: `vf chunks` / `vf chunk show` over the REAL daemon
// + REAL vf — pagination (a >1000-chunk source walks multiple ListVectors
// pages), the client hash-interval filter + early-exit, NUMERIC i ordering,
// the bounded neighborhood read (window / truncation marker / USAGE gates /
// fail-loud store_chunk_text), and the mixed-modality (txt+jpg zip) union
// enumeration with cross-modal neighbors returned as-is.
// ---------------------------------------------------------------------------

/// The T14 smoke's config: the plain docs bucket PLUS a `notext` bucket with
/// store_chunk_text = false (the fail-loud fixture — its address space is
/// enumerable but not readable).
fn chunk_config_toml(data_dir: &Path) -> String {
    format!(
        r#"version = 1
region = "us-east-1"
account_id = "000000000000"
access_key_id = "{AK}"
secret_access_key = "{SK}"
models_dir = "{models}"

[extractor.plain]
kind = "txt"

[embedder.tiny]
kind = "ort"
model = "tiny-fixture"
path = "{fixture}"
offline = true

[ingest.docs]
include = ["docs/**"]
extract = {{ txt = "plain" }}
chunk = {{ kind = "token-window", size = 32, overlap = 8 }}
embedders = ["tiny"]
scan_interval_secs = 1

[ingest.notext]
include = ["notext/**"]
extract = {{ txt = "plain" }}
chunk = {{ kind = "token-window", size = 32, overlap = 8 }}
embedders = ["tiny"]
store_chunk_text = false
scan_interval_secs = 1
"#,
        models = data_dir.join(".vectorfs/models").display(),
        fixture = fixture_dir().display(),
    )
}

/// Poll `vf sources status` to INDEXED for an arbitrary bucket (the shared
/// wait_vf_indexed helper is docs-hardcoded).
fn wait_vf_indexed_in(port: u16, bucket: &str, key: &str, deadline: Instant) {
    while Instant::now() < deadline {
        let o = vf(port, &["sources", "status", key, "--bucket", bucket], false);
        if out(&o).contains("status               : INDEXED") {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("{key} did not reach INDEXED in {bucket}");
}

#[test]
fn cli_chunk_addressing_smoke() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        chunk_config_toml(&data_dir),
    )
    .unwrap();

    // the PAGINATION source: token-window 32/8 strides ~24 chars, so ~40k
    // chars make ~1600 chunks — beyond the client's 1000/page ListVectors
    // request, the multi-page walk is exercised for real
    let mut big = String::new();
    for n in 0..700 {
        big.push_str(&format!(
            "the counting sentence number {n} carries plenty of ordinary words and ends here "
        ));
    }
    assert!(big.chars().count() > 39_000, "self-check: ~40k chars");
    let big_path = work.join("big.txt");
    std::fs::write(&big_path, &big).unwrap();
    // a SECOND source in the same bucket/index: the hash-interval filter
    // must keep its chunks out of big.txt's enumeration
    let fox = work.join("fox.txt");
    std::fs::write(&fox, "the quick brown fox jumps over the lazy dog").unwrap();
    // the fail-loud fixture source (notext bucket)
    let small = work.join("small.txt");
    std::fs::write(&small, "a modest paragraph of text for the no-text bucket").unwrap();

    let port = free_port();
    let daemon = Daemon::spawn(&data_dir, port, "chunk-serve.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));

    for (path, key, bucket) in [
        (&big_path, "docs/big.txt", "docs"),
        (&fox, "docs/fox.txt", "docs"),
        (&small, "notext/small.txt", "notext"),
    ] {
        let o = vf(
            port,
            &[
                "put",
                path.to_str().unwrap(),
                "--bucket",
                bucket,
                "--key",
                key,
                "--timeout",
                "300",
            ],
            false,
        );
        assert!(
            o.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            out(&o),
            err(&o)
        );
        wait_vf_indexed_in(port, bucket, key, Instant::now() + Duration::from_secs(300));
    }

    // ---- vf chunks: pagination + hash-interval filter + NUMERIC order ----
    // (the scoped bucket stores keys WITH the docs/ prefix — the flag form
    // addresses the full key verbatim, exactly like the get/cat contract)
    let o = vf(
        port,
        &["chunks", "docs/big.txt", "--bucket", "docs", "--json"],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["schema_version"], 1, "{}", out(&o));
    let hash = v["hash"].as_str().unwrap().to_string();
    let chunks = v["chunks"].as_array().cloned().unwrap();
    assert!(
        chunks.len() > 1000,
        "{} chunks — the >1000/page walk must paginate, got {}",
        chunks.len(),
        chunks.len()
    );
    // every key sits inside the source's hash interval (the fox.txt source
    // shares the index — its keys must be filtered OUT client-side)
    for c in &chunks {
        let k = c["key"].as_str().unwrap();
        assert!(k.starts_with(&format!("{hash}-")), "{k} outside {hash}-");
    }
    // the count agrees with the marker's own accounting (sources status)
    let o = vf(
        port,
        &[
            "sources",
            "status",
            "docs/big.txt",
            "--bucket",
            "docs",
            "--json",
        ],
        false,
    );
    let sv: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let marker_chunks: u64 = sv["source"]["chunks"]
        .as_array()
        .map(|cs| cs.iter().filter_map(|c| c["count"].as_u64()).sum())
        .unwrap_or(0);
    assert_eq!(
        chunks.len() as u64,
        marker_chunks,
        "enumeration count == marker count (hash filter lost or duplicated chunks)"
    );
    // NUMERIC i order + the 序号契约 (contiguous, gap-free, monotonic): the
    // text modality must enumerate 0..N-1 in numeric order — lexical order
    // would put text-10 right after text-1
    let is: Vec<u64> = chunks.iter().map(|c| c["i"].as_u64().unwrap()).collect();
    for (n, i) in is.iter().enumerate() {
        assert_eq!(
            *i, n as u64,
            "i must be 0..N-1 in numeric order (gap/dup at {n})"
        );
    }

    // human table: the preamble carries the hash prefix (with it, the I
    // column composes the keys `vf chunk show` takes), rows show KIND /
    // COORD (a txt corpus is pageless ⇒ LINE coords) / TEXT preview
    let o = vf(port, &["chunks", "docs/big.txt", "--bucket", "docs"], false);
    assert!(o.status.success(), "stdout:\n{}", out(&o));
    let table = out(&o);
    assert!(table.contains(&format!("{hash}-")), "{}", table);
    assert!(
        table.lines().count() > 100,
        "the whole address space is listed: {} rows",
        table.lines().count()
    );
    let header = table.lines().nth(1).unwrap_or_default();
    assert!(
        header.contains("I") && header.contains("KIND"),
        "header: {header}"
    );
    assert!(table.contains("text-0"), "{}", header);
    assert!(table.contains("body"), "kind column: {header}");
    assert!(table.contains("L1"), "txt rows carry line coords: {header}");
    // numeric-vs-lexical in the RENDERED table: text-10 must come after
    // text-9 (lexical order would print text-10 before text-2)
    let p9 = table.find("text-9").expect("text-9 row");
    let p10 = table.find("text-10").expect("text-10 row");
    assert!(
        p9 < p10,
        "numeric order in the table: text-9 before text-10"
    );
    let p2 = table.find("text-2").expect("text-2 row");
    assert!(
        p2 < p10,
        "numeric order in the table: text-2 before text-10"
    );

    // ---- vf chunk show: the bounded neighborhood read ----
    let anchor = format!("{hash}-text-5");
    // default --around 2: 5 blocks, anchor marked, headers carry the
    // metadata summary (kind / coord / model)
    let o = vf(port, &["chunk", "show", "docs", &anchor], false);
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    let shown = out(&o);
    assert!(shown.contains(&format!("── {anchor}")), "{}", shown);
    assert_eq!(shown.matches("← anchor").count(), 1, "{}", shown);
    for k in 3..=7 {
        assert!(shown.contains(&format!("{hash}-text-{k}")), "{}", shown);
    }
    assert!(
        !shown.contains(&format!("{hash}-text-2")),
        "outside the window: {}",
        shown
    );
    assert!(
        !shown.contains(&format!("{hash}-text-8")),
        "outside the window: {}",
        shown
    );
    assert!(shown.contains("kind=body"), "{}", shown);
    assert!(shown.contains("model=tiny-fixture"), "{}", shown);
    assert!(
        shown.contains("coord=L"),
        "txt coords in block headers: {}",
        shown
    );
    // no truncation at the default 400 chars (chunks here are ~32 chars)
    assert!(!shown.contains("[truncated"), "{}", shown);

    // --around 0: the anchor alone
    let o = vf(
        port,
        &["chunk", "show", "docs", &anchor, "--around", "0"],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let shown = out(&o);
    assert!(shown.contains(&format!("── {anchor}")), "{}", shown);
    assert!(!shown.contains(&format!("{hash}-text-4")), "{}", shown);
    assert!(!shown.contains(&format!("{hash}-text-6")), "{}", shown);

    // --chars 20: every block truncates, each marker pointing at the NEXT
    // key in address-space order — the LAST window block (text-7) points
    // PAST the window (text-8): the continuation window entry
    let o = vf(
        port,
        &["chunk", "show", "docs", &anchor, "--chars", "20"],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let shown = out(&o);
    assert!(
        shown.contains(&format!("…[truncated, next: {hash}-text-4]")),
        "first truncated block points forward: {}",
        shown
    );
    assert!(
        shown.contains(&format!("…[truncated, next: {hash}-text-8]")),
        "the window's last block points PAST the window: {}",
        shown
    );
    assert!(
        !shown.contains(&format!("…[truncated, next: {hash}-text-7]")),
        "a block's continuation is the NEXT key, never itself: {}",
        shown
    );

    // --chars 0 --around 0: the one legal full-text form — no marker
    let o = vf(
        port,
        &[
            "chunk", "show", "docs", &anchor, "--chars", "0", "--around", "0",
        ],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    assert!(!out(&o).contains("[truncated"), "{}", out(&o));

    // ---- the USAGE gates (exit 2) ----
    // --chars 0 with a neighborhood: the bounded-read mutex (chunk size is
    // an unbounded pipeline config; 41x full text would break the promise)
    let o = vf(
        port,
        &["chunk", "show", "docs", &anchor, "--chars", "0"],
        false,
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("--chars 0"), "{}", err(&o));
    // --around beyond the 20 cap (41-key window <= GetVectors' 100 keys)
    let o = vf(
        port,
        &["chunk", "show", "docs", &anchor, "--around", "21"],
        false,
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    // not a chunk key
    let o = vf(port, &["chunk", "show", "docs", "not-a-chunk-key"], false);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("is not a chunk key"), "{}", err(&o));
    // bucket/key path confusion: chunk show takes TWO positionals
    let o = vf(port, &["chunk", "show", "docs/fox.txt", &anchor], false);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("bare bucket name"), "{}", err(&o));

    // ---- the NotFound faces (exit 1) ----
    // a well-shaped key that is not in the address space
    let o = vf(
        port,
        &["chunk", "show", "docs", &format!("{hash}-text-99999")],
        false,
    );
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("not in bucket"), "{}", err(&o));
    // an unuploaded source key names the real problem (GetSource 404),
    // not a silently empty address space
    let o = vf(port, &["chunks", "nope.txt", "--bucket", "docs"], false);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("NotFound"), "{}", err(&o));

    // ---- store_chunk_text = false: enumerable but NOT readable ----
    // the listing works (the address space exists) with a stderr note
    let o = vf(
        port,
        &["chunks", "notext/small.txt", "--bucket", "notext", "--json"],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(err(&o).contains("store_chunk_text"), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let notext_chunks = v["chunks"].as_array().cloned().unwrap();
    assert!(!notext_chunks.is_empty(), "{}", out(&o));
    assert!(
        notext_chunks.iter().all(|c| c["text"].is_null()),
        "no text previews: {}",
        out(&o)
    );
    // the read FAILS LOUD, pointing at the config
    let notext_key = notext_chunks[0]["key"].as_str().unwrap().to_string();
    let o = vf(port, &["chunk", "show", "notext", &notext_key], false);
    assert_eq!(
        o.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert!(err(&o).contains("NO_CHUNK_TEXT"), "{}", err(&o));
    assert!(err(&o).contains("store_chunk_text"), "{}", err(&o));

    daemon.dump_log_tail("end of chunk smoke (informational)");
}

/// The mixed-modality config (T14): zip container + txt and image child
/// chains, TWO embedders — tiny (text doc side) and clip/siglip2 (image doc
/// side). The source's chunks therefore SPLIT ACROSS TWO derived indexes
/// (text-* in tiny's, image-* in clip's): the union enumeration is the only
/// way the address space is complete.
fn mixed_config_toml(data_dir: &Path) -> String {
    format!(
        r#"version = 1
region = "us-east-1"
account_id = "000000000000"
access_key_id = "{AK}"
secret_access_key = "{SK}"
models_dir = "{models}"

[extractor.plain]
kind = "txt"

[extractor.zipper]
kind = "zip"

[extractor.imgs]
kind = "image"

[embedder.tiny]
kind = "ort"
model = "tiny-fixture"
path = "{tiny}"
offline = true

[embedder.clip]
kind = "siglip2"
model = "tiny-siglip2"
path = "{clip}"
offline = true

[ingest.docs]
include = ["docs/**"]
extract = {{ zip = "zipper", txt = "plain", img = "imgs" }}
chunk = {{ kind = "token-window", size = 32, overlap = 8 }}
embedders = ["tiny", "clip"]
scan_interval_secs = 1
"#,
        models = data_dir.join(".vectorfs/models").display(),
        tiny = fixture_dir().display(),
        clip = siglip2_fixture_dir().display(),
    )
}

/// The T14 interleave e2e: a txt+png zip container source indexes as
/// INTERLEAVED text/image chunks (the vector world's mixed document), the
/// two modalities land in TWO different derived indexes, and:
/// * `vf chunks` enumerates the UNION across both indexes (complete address
///   space — a single-index read would miss half of it);
/// * `vf chunk show --around` at the modality seam returns the CROSS-MODAL
///   neighbor as-is (枚举优先: neighbors are the real enumerated keys, not
///   guessed i±1 keys).
#[test]
fn cli_mixed_modality_chunks_smoke() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        mixed_config_toml(&data_dir),
    )
    .unwrap();

    // the mixed source: one txt child (several text chunks) + one png child
    // (one image atom), zipped in test with the same crate the extractor
    // reads (the extract/zip.rs discipline — no committed binary blobs)
    let mut text_child = String::new();
    for n in 0..10 {
        text_child.push_str(&format!(
            "chapter paragraph {n} with several plain words inside it "
        ));
    }
    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(&mut zip_buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("chapter.txt", opts).unwrap();
        zip.write_all(text_child.as_bytes()).unwrap();
        zip.start_file("figure.png", opts).unwrap();
        zip.write_all(RED_PNG).unwrap();
        zip.finish().unwrap();
    }
    let mixed = work.join("mixed.zip");
    std::fs::write(&mixed, zip_buf.into_inner()).unwrap();

    let port = free_port();
    let daemon = Daemon::spawn(&data_dir, port, "mixed-serve.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));

    let o = vf(
        port,
        &[
            "put",
            mixed.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/mixed.zip",
            "--timeout",
            "300",
        ],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    wait_vf_indexed_in(
        port,
        "docs",
        "docs/mixed.zip",
        Instant::now() + Duration::from_secs(300),
    );

    // ---- the UNION enumeration: both modalities, both indexes ----
    let o = vf(
        port,
        &["chunks", "docs/mixed.zip", "--bucket", "docs", "--json"],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let hash = v["hash"].as_str().unwrap().to_string();
    let chunks = v["chunks"].as_array().cloned().unwrap();
    let modalities: Vec<&str> = chunks
        .iter()
        .map(|c| c["modality"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        modalities.contains(&"image"),
        "the image half lives in clip's index — the union must carry it: {:?}",
        modalities
    );
    assert!(
        modalities.contains(&"text"),
        "the text half lives in tiny's index: {:?}",
        modalities
    );
    // the count agrees with the marker's cross-modality accounting
    let o = vf(
        port,
        &[
            "sources",
            "status",
            "docs/mixed.zip",
            "--bucket",
            "docs",
            "--json",
        ],
        false,
    );
    let sv: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let marker_chunks: u64 = sv["source"]["chunks"]
        .as_array()
        .map(|cs| cs.iter().filter_map(|c| c["count"].as_u64()).sum())
        .unwrap_or(0);
    assert_eq!(
        chunks.len() as u64,
        marker_chunks,
        "union count == marker count (a single-index enumeration under-counts)"
    );
    // address-space order: the NUMERIC global i — the chunker assigns ONE
    // index sequence per source (text windows and image atoms share it), so
    // the enumeration must be 0..N-1 gap-free in document order: the txt
    // child's windows first, the png child's atom LAST (zip order)
    let mut seen: Vec<(String, u64)> = Vec::new();
    for c in &chunks {
        seen.push((
            c["modality"].as_str().unwrap_or("?").to_string(),
            c["i"].as_u64().unwrap(),
        ));
    }
    for (n, (_, i)) in seen.iter().enumerate() {
        assert_eq!(
            *i, n as u64,
            "the global i must be 0..N-1 in order: {seen:?}"
        );
    }
    let last_i = seen.len() as u64 - 1;
    assert_eq!(
        seen.last().map(|(m, _)| m.as_str()),
        Some("image"),
        "the png child is the LAST chunk (zip order preserved): {seen:?}"
    );
    let last_text_i = last_i - 1;

    // ---- 枚举优先 at the interleave: the LAST text chunk's AFTER neighbor
    // in the real enumeration is the IMAGE atom — returned AS-IS, not
    // skipped, not guessed into another text key ----
    let last_text = format!("{hash}-text-{last_text_i}");
    let o = vf(
        port,
        &["chunk", "show", "docs", &last_text, "--around", "1"],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    let shown = out(&o);
    let image_key = format!("{hash}-image-{last_i}");
    assert!(
        shown.contains(&format!("── {image_key}")),
        "the cross-modal AFTER neighbor is shown as-is: {}",
        shown
    );
    assert!(
        shown.contains("〔image chunk — no text channel〕"),
        "the image block says what it is instead of an empty body: {}",
        shown
    );
    assert!(shown.contains("← anchor"), "{}", shown);
    // exactly three blocks: [text-{last-2}, text-{last-1}=anchor, image-{last}]
    assert_eq!(
        shown.matches("── ").count(),
        3,
        "window = anchor + 1 before + 1 after: {}",
        shown
    );

    // anchoring ON the image atom: its BEFORE neighbor crosses back into
    // text (and the window clamps at the tail — nothing after it)
    let o = vf(
        port,
        &["chunk", "show", "docs", &image_key, "--around", "1"],
        false,
    );
    assert!(o.status.success(), "{}", err(&o));
    let shown = out(&o);
    assert!(
        shown.contains(&format!("── {last_text}")),
        "the cross-modal BEFORE neighbor: {}",
        shown
    );
    assert_eq!(
        shown.matches("── ").count(),
        2,
        "clamped at the tail (nothing exists after the last chunk): {}",
        shown
    );

    daemon.dump_log_tail("end of mixed-modality smoke (informational)");
}

// ---------------------------------------------------------------------------
// vf jobs (the InFlight register: live per-job ingest progress)
// ---------------------------------------------------------------------------

/// `vf jobs` end to end against the real daemon: the DETERMINISTIC face is
/// the envelope + empty state (`no in-flight jobs`, --json count 0, exit 0,
/// both bucket-scoped and aggregate); the mid-flight table is sampled
/// OPPORTUNISTICALLY — a big multi-sub-batch source is `put --async`'d and
/// `vf jobs --json` polled while the worker grinds. The deterministic
/// mid-flight semantics (chunksDone advancing per sub-batch, terminal
/// clear) are pinned by the in-process tests (ingest mod + ops_xvf); this
/// smoke adds the real binaries, the signed wire and the human table.
#[test]
fn cli_jobs_live_progress_and_empty_state() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("work");
    let data_dir = work.join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&data_dir),
    )
    .unwrap();

    // ~52 KB of text over the 32-char/8-overlap window ⇒ ~2.1k chunks ⇒
    // 5 sub-batches: the job runs for seconds, widening the sampling window
    let novel = work.join("novel.txt");
    std::fs::write(&novel, "word ".repeat(10_500)).unwrap();

    let port = free_port();
    let daemon = Daemon::spawn(&data_dir, port, "serve-jobs.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(60));

    // ---- the empty state BEFORE any upload (deterministic) ----
    let o = vf(port, &["jobs"], false);
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );
    assert_eq!(out(&o), "no in-flight jobs\n");
    let o = vf(port, &["jobs", "--json"], false);
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["count"], 0);
    assert_eq!(v["jobs"].as_array().map(Vec::len), Some(0));

    // ---- upload --async and sample the live table (opportunistic) ----
    let o = vf(
        port,
        &[
            "put",
            novel.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/novel.txt",
            "--async",
        ],
        false,
    );
    assert!(
        o.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out(&o),
        err(&o)
    );

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut caught: Option<serde_json::Value> = None;
    while Instant::now() < deadline {
        let o = vf(port, &["jobs", "--json"], false);
        if !o.status.success() {
            continue; // transient mid-poll failures are retried until deadline
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&out(&o)) else {
            continue;
        };
        if v["count"].as_u64().unwrap_or(0) >= 1 {
            caught = v["jobs"][0].clone().into();
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    if let Some(j) = &caught {
        assert_eq!(j["bucket"].as_str(), Some("docs"), "{j}");
        assert_eq!(j["key"].as_str(), Some("docs/novel.txt"), "{j}");
        let stage = j["stage"].as_str().unwrap_or_default();
        assert!(
            [
                "unpacking",
                "extracting",
                "chunking",
                "embedding",
                "writing"
            ]
            .contains(&stage),
            "{j}"
        );
        if let (Some(d), Some(t)) = (j["chunksDone"].as_u64(), j["chunksTotal"].as_u64()) {
            assert!(d <= t, "{j}");
        }
        // the HUMAN table while still in flight: header + one data row (if
        // the job is STILL running — a re-check through --json gates the row
        // assert so the smoke cannot flake on a fast finisher)
        let o = vf(port, &["jobs", "docs"], false);
        assert!(
            o.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            out(&o),
            err(&o)
        );
        let table = out(&o);
        assert!(table.contains("KEY") && table.contains("STAGE"), "{table}");
        assert!(
            table.contains("ELAPSED") && table.contains("DETAIL"),
            "{table}"
        );
        let o = vf(port, &["jobs", "--json"], false);
        let still: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
        if still["count"].as_u64().unwrap_or(0) >= 1 {
            assert!(
                table.contains("docs/novel.txt"),
                "the live row renders: {table}"
            );
            assert!(
                table.contains(stage) || still["jobs"][0]["stage"] != j["stage"],
                "stage cell renders (or advanced between the two reads): {table}"
            );
        }
    } else {
        eprintln!(
            "vf-jobs smoke: the job finished before a live sample landed \
                   (the deterministic mid-flight semantics live in the in-process tests)"
        );
    }

    // ---- terminal: the register is empty in EVERY form (deterministic) ----
    wait_vf_indexed(
        port,
        "docs/novel.txt",
        Instant::now() + Duration::from_secs(120),
    );
    let o = vf(port, &["jobs"], false);
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    assert_eq!(out(&o), "no in-flight jobs\n");
    let o = vf(port, &["jobs", "docs", "--json"], false);
    assert!(o.status.success(), "stderr:\n{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["count"], 0);

    daemon.dump_log_tail("end of vf jobs smoke (informational)");
}
