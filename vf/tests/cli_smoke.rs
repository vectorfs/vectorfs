//! M2 T6 CLI integration smoke: a REAL `vectorfs serve` daemon (SigV4 auth
//! ON, tiny-embedder fixture pipeline) exercised end to end through the REAL
//! `vf` client binary (`CARGO_BIN_EXE_vf`) — every 附录E.4 verb:
//!
//!   put (foreground wait to INDEXED, and --async), get (byte round-trip),
//!   sources list / sources status, search (hit + location 回源), rm (202
//!   DELETING), reindex (jobId), status (/health + GetIngestStatus) — plus
//!   the FAILED exit-1 path (non-UTF-8 source) and the no-credentials
//!   structured error.
//!
//! Hermetic: committed tiny-embedder fixture + repo-local libonnxruntime
//! (VF_ORT_PATH), tmp data root, free TCP port; no network beyond loopback.

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const AK: &str = "VFTESTKEY";
const SK: &str = "vf-test-secret";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vf/../ is the repo root")
        .to_path_buf()
}

fn ort_lib() -> PathBuf {
    let p = repo_root().join("runtime-libs/libonnxruntime.so");
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
scope = "docs/**"
extract = {{ txt = "plain" }}
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

/// One `vf` invocation with the endpoint/credentials environment set.
fn vf(port: u16, args: &[&str], strip_creds: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vf"));
    cmd.args(args)
        .env("VF_ENDPOINT", format!("http://127.0.0.1:{port}"))
        .env("VF_ACCESS_KEY", AK)
        .env("VF_SECRET_KEY", SK);
    if strip_creds {
        cmd.env_remove("VF_ACCESS_KEY").env_remove("VF_SECRET_KEY");
    }
    cmd.output().expect("run vf")
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

    // (the tiny fixture embeds only a 16-word vocab — "quick brown fox" is
    // the discriminative query; the assertion target is the hit SHAPE and
    // the 回源 location, not the toy model's ranking quality)
    let o = vf(
        port,
        &[
            "search",
            "quick brown fox",
            "--bucket",
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
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{}", out(&o));
    assert_eq!(hits[0]["location"]["key"], "docs/fox.txt", "{}", out(&o));
    assert_eq!(hits[0]["content"]["type"], "TEXT", "{}", out(&o));
    assert!(hits[0]["content"]["text"].is_string(), "{}", out(&o));
    assert!(hits[0]["score"].as_f64().is_some(), "{}", out(&o));
    assert!(
        hits[0]["index"].as_str().unwrap().starts_with("tiny-"),
        "{}",
        out(&o)
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

    // ---- FAILED path: non-UTF-8 source -> exit 1 + statusReason ----
    let bad = work.join("bad.txt");
    std::fs::write(&bad, b"\xff\xfe not utf8 \xff").unwrap();
    let o = vf(
        port,
        &[
            "put",
            bad.to_str().unwrap(),
            "--bucket",
            "docs",
            "--key",
            "docs/bad.txt",
            "--timeout",
            "60",
        ],
        false,
    );
    assert_eq!(o.status.code(), Some(1), "FAILED exits 1: {}", out(&o));
    assert!(out(&o).starts_with("FAILED\tdocs/bad.txt"), "{}", out(&o));
    assert!(err(&o).contains("statusReason"), "{}", err(&o));

    // ---- structured usage error: no credentials ----
    let o = vf(port, &["status"], true);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("NO_CREDENTIALS"), "{}", err(&o));

    daemon.dump_log_tail("end of smoke (informational)");
}
