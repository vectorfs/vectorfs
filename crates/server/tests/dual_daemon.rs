//! G2 dual-daemon semantics (design-v3.md §5.1, §10-G2), as REAL processes:
//!
//! 1. **Disjoint bucket sets** (always runs, file backend): two serve
//!    processes on different data-dirs — on the file backend the data-dir IS
//!    the bucket root (R6), so different roots are disjoint bucket sets by
//!    construction ("不同 data-dir ⇒ 桶集天然不相交，零冲突"). Each daemon
//!    creates and serves its own vb; the OTHER daemon's vb 404s (a different
//!    root is a different namespace, not a shared one).
//! 2. **Same data-dir ⇒ flock refusal** (always runs): the local flock
//!    (§5.1, `<data-dir>/.vectorfs/flock`) rejects the second daemon at
//!    startup (exit 3) while the first keeps serving.
//! 3. **Same-bucket lock conflict 409** (`#[ignore]`, needs `VF_S3CP_URI`):
//!    the file backend cannot host a shared-namespace pair (same root =
//!    flock), so the contested-bucket case rides the s3 backend: two daemons
//!    on one endpoint, different data-dirs. The second daemon's WRITE on the
//!    first's held bucket answers 409 ConflictException carrying the
//!    occupant (§5.1 登记偏离行), its own bucket works, and READS of the
//!    contested bucket stay open.
//!
//! The s3 row follows experiments.rs' env convention: run with
//! `VF_S3CP_URI='s3+http://127.0.0.1:19000' cargo test -- --ignored`
//! (SeaweedFS `weed server -s3` or any S3-compatible endpoint; creds via
//! optional VF_S3_ACCESS_KEY/VF_S3_SECRET_KEY, anonymous otherwise).

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn spawn(data_dir: &Path, port: u16, log_name: &str) -> Daemon {
        Self::spawn_with(data_dir, port, log_name, &[])
    }

    /// `extra_env`: (name, value) pairs — e.g. VF_BACKEND for the s3 rows.
    fn spawn_with(
        data_dir: &Path,
        port: u16,
        log_name: &str,
        extra_env: &[(&str, &str)],
    ) -> Daemon {
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
            .envs(
                extra_env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string())),
            )
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
            for line in lines.iter().rev().take(30).rev() {
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("serve never became healthy on port {port}");
}

/// (status, x-amzn-errortype, body) of one JSON POST (same shape as
/// lifecycle_crash_replay.rs).
fn post_json(port: u16, path: &str, body: &str) -> (u16, Option<String>, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: vf-test\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).expect("write request");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).expect("read response");
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    // match the header name case-insensitively, keep the value's case
    let errortype = text.lines().find_map(|l| {
        let lower = l.to_ascii_lowercase();
        lower.strip_prefix("x-amzn-errortype:").map(|_| {
            l["x-amzn-errortype:".len()..]
                .trim()
                .trim_end_matches(',')
                .to_string()
        })
    });
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, errortype, body)
}

// ---------------------------------------------------------------------------
// file backend: disjoint bucket sets (§5.1 "不同 data-dir ⇒ 桶集天然不相交")
// ---------------------------------------------------------------------------

#[test]
fn dual_file_daemons_disjoint_bucket_sets_both_serve() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("daemon-a");
    let root_b = tmp.path().join("daemon-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    let (port_a, port_b) = (free_port(), free_port());

    // two SIMULTANEOUS daemons, each its own bucket root (file backend)
    let mut da = Daemon::spawn(&root_a, port_a, "daemon-a.log");
    let mut db = Daemon::spawn(&root_b, port_b, "daemon-b.log");
    wait_healthy(port_a, Instant::now() + Duration::from_secs(30));
    wait_healthy(port_b, Instant::now() + Duration::from_secs(30));

    // each creates and serves its OWN vb
    let (st, et, body) = post_json(
        port_a,
        "/CreateVectorBucket",
        r#"{"vectorBucketName":"alpha"}"#,
    );
    assert_eq!(st, 200, "daemon A creates alpha: {et:?} {body}");
    let (st, et, body) = post_json(
        port_b,
        "/CreateVectorBucket",
        r#"{"vectorBucketName":"beta"}"#,
    );
    assert_eq!(st, 200, "daemon B creates beta: {et:?} {body}");

    // cross reads: the OTHER daemon's vb does not exist in my namespace
    let (st, _, _) = post_json(port_a, "/GetVectorBucket", r#"{"vectorBucketName":"beta"}"#);
    assert_eq!(st, 404, "beta is not in daemon A's root");
    let (st, _, _) = post_json(
        port_b,
        "/GetVectorBucket",
        r#"{"vectorBucketName":"alpha"}"#,
    );
    assert_eq!(st, 404, "alpha is not in daemon B's root");
    // own reads fine
    let (st, _, body) = post_json(
        port_a,
        "/GetVectorBucket",
        r#"{"vectorBucketName":"alpha"}"#,
    );
    assert_eq!(st, 200, "{body}");
    let (st, _, body) = post_json(port_b, "/GetVectorBucket", r#"{"vectorBucketName":"beta"}"#);
    assert_eq!(st, 200, "{body}");

    // physical isolation: each root holds ONLY its own bucket pair's system
    // bucket; no source bucket anywhere (lazy, D4)
    assert!(root_a.join("alpha-vf/bucket.json").is_file());
    assert!(root_b.join("beta-vf/bucket.json").is_file());
    assert!(!root_a.join("alpha").exists());
    assert!(!root_b.join("beta").exists());

    // both daemons stay healthy to the end (zero cross-talk)
    wait_healthy(port_a, Instant::now() + Duration::from_secs(5));
    wait_healthy(port_b, Instant::now() + Duration::from_secs(5));
    da.kill9();
    db.kill9();
}

// ---------------------------------------------------------------------------
// file backend: same data-dir ⇒ flock refusal (§5.1)
// ---------------------------------------------------------------------------

#[test]
fn second_daemon_same_data_dir_refused_by_flock() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let port = free_port();

    let mut d1 = Daemon::spawn(&root, port, "flock-first.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(30));

    // the second daemon on the SAME data-dir must exit 3 (flock held), and
    // quickly — the flock is taken before anything else (bin/vectorfs.rs
    // "Taken BEFORE any bucket LOCK")
    let mut d2 = Daemon::spawn(&root, free_port(), "flock-second.log");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut code = None;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = d2.child.try_wait() {
            code = status.code();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let code = code.unwrap_or_else(|| {
        d2.dump_log_tail("second daemon did not exit");
        panic!("second daemon on the same data-dir never exited (flock not enforced?)");
    });
    assert_eq!(code, 3, "flock refusal exit code (log below)");
    d2.dump_log_tail("flock refusal");
    let log = std::fs::read_to_string(tmp.path().join("flock-second.log")).unwrap();
    assert!(
        log.contains("refusing to start"),
        "the refusal must name the occupant: {log}"
    );

    // ...and the FIRST daemon is untouched (the loser held nothing)
    wait_healthy(port, Instant::now() + Duration::from_secs(5));
    let (st, _, _) = post_json(
        port,
        "/CreateVectorBucket",
        r#"{"vectorBucketName":"solo"}"#,
    );
    assert_eq!(st, 200, "the first daemon still writes");
    d1.kill9();
}

// ---------------------------------------------------------------------------
// s3 backend: same-bucket cross-process lock conflict (§5.1) — env-gated
// ---------------------------------------------------------------------------

/// The VF_S3CP_* construction (experiments.rs convention).
fn s3_config() -> vectorfs_server::backends::S3BackendConfig {
    let uri = std::env::var("VF_S3CP_URI").expect("VF_S3CP_URI=s3+http://host:port");
    let cfg =
        vectorfs_server::backends::S3BackendConfig::parse(&uri).expect("parse s3+ endpoint uri");
    vectorfs_server::backends::S3BackendConfig {
        access_key: std::env::var("VF_S3_ACCESS_KEY").ok().or(cfg.access_key),
        secret_key: std::env::var("VF_S3_SECRET_KEY").ok().or(cfg.secret_key),
        ..cfg
    }
}

#[test]
#[ignore = "needs live S3 endpoint; set VF_S3CP_URI (see module doc)"]
fn same_bucket_two_daemons_cross_process_lock_conflict_409() {
    let cfg = s3_config();
    let backend_spec = format!("s3+{}", cfg.endpoint);

    // unique-per-run names on the SHARED endpoint (reruns must not trip over
    // last run's markers); pre-clean any residue via the raw admin face
    let run = std::process::id();
    let shared = format!("dd{run}"); // D1's bucket, lock held by D1
    let own2 = format!("do{run}"); // D2's own, uncontested
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on({
        let cfg = cfg.clone();
        let shared = shared.clone();
        let own2 = own2.clone();
        async move {
            let storage = vectorfs_server::store::Storage::new(
                vectorfs_server::backends::Backend::S3(Box::new(cfg)),
                "-vf",
            )
            .unwrap();
            for vb in [&shared, &own2] {
                storage.delete_bucket_raw(&format!("{vb}-vf")).await;
                storage.delete_bucket_raw(vb).await;
            }
        }
    });

    let tmp = tempfile::tempdir().expect("tmp");
    let home_a = tmp.path().join("home-a"); // s3 backend: data-dir is ONLY the daemon home
    let home_b = tmp.path().join("home-b");
    std::fs::create_dir_all(&home_a).unwrap();
    std::fs::create_dir_all(&home_b).unwrap();
    let (port_a, port_b) = (free_port(), free_port());

    let mut d1 = Daemon::spawn_with(
        &home_a,
        port_a,
        "s3-d1.log",
        &[("VF_BACKEND", &backend_spec)],
    );
    let mut d2 = Daemon::spawn_with(
        &home_b,
        port_b,
        "s3-d2.log",
        &[("VF_BACKEND", &backend_spec)],
    );
    wait_healthy(port_a, Instant::now() + Duration::from_secs(30));
    wait_healthy(port_b, Instant::now() + Duration::from_secs(30));

    // D1 creates `shared` — the create path takes the lazy bucket lock
    // (ops.rs create_vector_bucket -> require_write_lock), so D1 OWNS it
    // from here on (fresh heartbeat; idle release only after a full TTL).
    let (st, et, body) = post_json(
        port_a,
        "/CreateVectorBucket",
        &format!(r#"{{"vectorBucketName":"{shared}"}}"#),
    );
    assert_eq!(st, 200, "D1 creates {shared}: {et:?} {body}");

    // D2 writes into D1's bucket: 409 ConflictException CARRYING THE
    // OCCUPANT (§5.1 登记偏离: writer id/起始时间 ride the message)
    let (st, et, body) = post_json(
        port_b,
        "/CreateIndex",
        &format!(
            r#"{{"vectorBucketName":"{shared}","indexName":"lockix","dataType":"float32","dimension":2,"distanceMetric":"cosine"}}"#
        ),
    );
    assert_eq!(st, 409, "D2's write on the held bucket: {body}");
    assert_eq!(et.as_deref(), Some("ConflictException"), "{body}");
    assert!(
        body.contains("locked by another writer"),
        "conflict typing: {body}"
    );
    assert!(
        body.contains("pid="),
        "the occupant description rides the conflict (§5.1): {body}"
    );

    // D2's READ of the contested bucket stays open (only write paths park)
    let (st, _, body) = post_json(
        port_b,
        "/GetVectorBucket",
        &format!(r#"{{"vectorBucketName":"{shared}"}}"#),
    );
    assert_eq!(st, 200, "reads are lock-free: {body}");

    // one contested bucket parks NOTHING on D2: its OWN bucket works
    let (st, et, body) = post_json(
        port_b,
        "/CreateVectorBucket",
        &format!(r#"{{"vectorBucketName":"{own2}"}}"#),
    );
    assert_eq!(st, 200, "D2 creates its own {own2}: {et:?} {body}");

    // and D1 keeps writing `shared` (the incumbent is unaffected)
    let (st, et, body) = post_json(
        port_a,
        "/CreateIndex",
        &format!(
            r#"{{"vectorBucketName":"{shared}","indexName":"lockix","dataType":"float32","dimension":2,"distanceMetric":"cosine"}}"#
        ),
    );
    assert_eq!(st, 200, "D1 (the owner) writes freely: {et:?} {body}");

    // enumeration from EITHER daemon sees the whole shared topology (same
    // deployment identity — R9 suffix scan, no registry)
    for (p, who) in [(port_a, "D1"), (port_b, "D2")] {
        let (st, _, body) = post_json(p, "/ListVectorBuckets", "{}");
        assert_eq!(st, 200);
        assert!(
            body.contains(&shared) && body.contains(&own2),
            "{who} sees the shared endpoint topology: {body}"
        );
    }

    // cleanup: delete the index first (non-empty Delete = 409, AWS 实测语义),
    // then the buckets — proving BOTH daemons can still drive their own
    let (st, _, _) = post_json(
        port_a,
        "/DeleteIndex",
        &format!(r#"{{"vectorBucketName":"{shared}","indexName":"lockix"}}"#),
    );
    assert_eq!(st, 200);
    let (st, et, body) = post_json(
        port_a,
        "/DeleteVectorBucket",
        &format!(r#"{{"vectorBucketName":"{shared}"}}"#),
    );
    assert_eq!(st, 200, "D1 deletes its bucket: {et:?} {body}");
    let (st, et, body) = post_json(
        port_b,
        "/DeleteVectorBucket",
        &format!(r#"{{"vectorBucketName":"{own2}"}}"#),
    );
    assert_eq!(st, 200, "D2 deletes its bucket: {et:?} {body}");

    d1.kill9();
    d2.kill9();

    // endpoint hygiene: nothing named dd*/do* remains (system buckets gone;
    // source buckets were never created — lazy, D4)
    rt.block_on(async move {
        let storage = vectorfs_server::store::Storage::new(
            vectorfs_server::backends::Backend::S3(Box::new(cfg)),
            "-vf",
        )
        .unwrap();
        let names = storage.suffix_scan_candidates().await.unwrap();
        let leftover: Vec<String> = names
            .iter()
            .filter(|n| n.starts_with(&shared) || n.starts_with(&own2))
            .cloned()
            .collect();
        assert_eq!(
            leftover,
            Vec::<String>::new(),
            "rerun-safe cleanup: {names:?}"
        );
    });
}
