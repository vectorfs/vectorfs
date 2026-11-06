//! G2 crash-replay matrix (design-v3.md §5.2 rows 3-4, §10-G2): REAL serve
//! processes hard-crashed at the two lifecycle cut points, then a REAL replay
//! daemon must converge — no tombstone (D7), from BOTH replay directions:
//!
//! * cut A `create_after_raw_create` — raw CreateBucket of `X-vf` succeeded,
//!   the ownership marker was never written (§5.2 row 3). On disk: `X-vf/`
//!   exists, no `bucket.json`, no `LOCK`, and NO source bucket `X` (D4 —
//!   Create never touches it). Replay Create ⇒ ADOPTION (D7); replay Delete ⇒
//!   lockless tail cleanup (§5.3-4) — both converge.
//! * cut B `delete_after_marker` — marker + LOCK already deleted, raw
//!   DeleteBucket never sent (§5.2 row 4). On disk: `X-vf/` exists, empty.
//!   Replay Delete ⇒ 无锁清尾; replay Create ⇒ adoption — both converge.
//!
//! The cut points are hit via the `VF_TEST_CRASH` seam in store.rs
//! (`Storage::crash_point`) — the process ABORTS (SIGABRT: no graceful
//! shutdown, no LOCK release) at the exact statement, the same hard-crash
//! semantics a kill -9 has for objects-on-storage. This is the process-level
//! version the spec demands ("kill -9 切点…重放断言收敛"); an externally
//! timed kill cannot land inside the sub-millisecond window between mkdir
//! and marker write, so the seam is the only honest way to make the cut
//! deterministic.
//!
//! Hermetic file backend (the lifecycle code is backend-generic; the s3
//! half of G2 runs the same lifecycle against a live endpoint in
//! tests/s3_conformance.rs).

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// daemon control (the ingest_kill9.rs shape, minus the ingest fixtures —
// this matrix never configures [ingest.X], so no embedder/ORT is needed)
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    /// `crash`: value for VF_TEST_CRASH (None = a normal, well-behaved
    /// daemon — the replay daemon).
    fn spawn(data_dir: &Path, port: u16, log_name: &str, crash: Option<&str>) -> Daemon {
        let log = data_dir.parent().unwrap().join(log_name);
        let stderr = std::fs::File::create(&log).expect("create log file");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vectorfs"));
        cmd.args([
            "serve",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--no-auth",
        ])
        .stdout(Stdio::from(stderr.try_clone().expect("clone log")))
        .stderr(stderr);
        if let Some(point) = crash {
            cmd.env("VF_TEST_CRASH", point);
        }
        let child = cmd.spawn().expect("spawn vectorfs serve");
        Daemon { child, log }
    }

    fn kill9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Wait for the process to die on its own (the crash path) and prove it
    /// died HARD (a signal — SIGABRT 6 here — not a graceful exit).
    fn wait_crashed(&mut self, context: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                if status.success() {
                    self.dump_log_tail(context);
                    panic!("{context}: expected a hard crash, exited {status}");
                }
                return;
            }
            if Instant::now() >= deadline {
                self.dump_log_tail(context);
                panic!("{context}: daemon never crashed");
            }
            std::thread::sleep(Duration::from_millis(20));
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

/// Raw-HTTP poll of the no-auth /healthz route (same shape as
/// ingest_kill9.rs).
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

/// One JSON POST over a fresh connection. Ok = (status, errortype header,
/// body). A hard crash mid-handler surfaces as an io error (connection
/// reset) OR a clean EOF with zero response bytes (status 0) — both are the
/// cut-point calls' expected outcome.
fn post_json(port: u16, path: &str, body: &str) -> std::io::Result<(u16, Option<String>, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: vf-test\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
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
    Ok((status, errortype, body))
}

// ---------------------------------------------------------------------------
// crash-state assertions (§5.2 on-storage residue, verbatim)
// ---------------------------------------------------------------------------

/// Cut-point residue: `X-vf` present, marker absent, LOCK absent, source
/// bucket `X` absent (Create never creates it — D4; Delete never deletes it).
fn assert_crash_residue(data_dir: &Path, vb: &str, context: &str) {
    let sys = data_dir.join(format!("{vb}-vf"));
    assert!(
        sys.is_dir(),
        "{context}: {vb}-vf must exist after the crash"
    );
    assert!(
        !sys.join("bucket.json").exists(),
        "{context}: marker must NOT exist (crash before it was written / after it was deleted)"
    );
    assert!(
        !sys.join("LOCK").exists(),
        "{context}: LOCK must not exist (cut A never wrote it; cut B deleted it in-order)"
    );
    assert!(
        !data_dir.join(vb).exists(),
        "{context}: source bucket {vb} must never have been created (D4)"
    );
}

/// The expected outcome of a request whose handler hit the crash seam: an
/// io error (reset) or an empty response (EOF, status 0). Anything else
/// means the daemon answered — the cut was missed.
fn expect_no_response(res: std::io::Result<(u16, Option<String>, String)>, context: &str) {
    match res {
        Err(_) => {}
        Ok((0, _, _)) => {}
        Ok(other) => panic!("{context}: daemon answered {other:?} — the crash seam never fired"),
    }
}

fn create_vb(port: u16, vb: &str) -> (u16, Option<String>, String) {
    post_json(
        port,
        "/CreateVectorBucket",
        &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
    )
    .expect("CreateVectorBucket request")
}

fn delete_vb(port: u16, vb: &str) -> (u16, Option<String>, String) {
    post_json(
        port,
        "/DeleteVectorBucket",
        &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
    )
    .expect("DeleteVectorBucket request")
}

// ---------------------------------------------------------------------------
// the matrix
// ---------------------------------------------------------------------------

#[test]
fn crash_replay_matrix_converges_both_directions() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("data");
    let data_dir = work.clone();
    std::fs::create_dir_all(&data_dir).unwrap();
    let port = free_port();

    // ---- cut A: crash between raw CreateBucket and the marker write ----
    // cra -> replay Create (D7 adoption); crx -> replay Delete (tail clean)
    for vb in ["cra", "crx"] {
        let mut d = Daemon::spawn(
            &data_dir,
            port,
            &format!("cutA-{vb}.log"),
            Some("create_after_raw_create"),
        );
        wait_healthy(port, Instant::now() + Duration::from_secs(30));
        let res = post_json(
            port,
            "/CreateVectorBucket",
            &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
        );
        expect_no_response(res, &format!("cut A {vb}"));
        d.wait_crashed(&format!("cut A {vb}"));
        assert_crash_residue(&data_dir, vb, &format!("cut A {vb}"));
    }

    // ---- cut B: crash after marker+LOCK deletion, before raw DeleteBucket ----
    // the SAME crashing daemon creates normally first (the crash env only
    // fires at the delete cut point), then dies mid-Delete:
    // dlt -> replay Delete; dlx -> replay Create (both directions converge)
    for vb in ["dlt", "dlx"] {
        let mut d = Daemon::spawn(
            &data_dir,
            port,
            &format!("cutB-{vb}.log"),
            Some("delete_after_marker"),
        );
        wait_healthy(port, Instant::now() + Duration::from_secs(30));
        let (st, et, body) = create_vb(port, vb);
        assert_eq!(st, 200, "create under cut-B env: {et:?} {body}");
        let res = post_json(
            port,
            "/DeleteVectorBucket",
            &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
        );
        expect_no_response(res, &format!("cut B {vb}"));
        d.wait_crashed(&format!("cut B {vb}"));
        assert_crash_residue(&data_dir, vb, &format!("cut B {vb}"));
    }

    // ---- replay daemon: one normal process replays all four states ----
    let mut replayer = Daemon::spawn(&data_dir, port, "replay.log", None);
    wait_healthy(port, Instant::now() + Duration::from_secs(30));

    // cut A x replay-Create => ADOPTION (§5.2 row 3: 幂等收敛, D7)
    let (st, et, body) = create_vb(port, "cra");
    assert_eq!(st, 200, "replay Create on cut-A residue: {et:?} {body}");
    assert!(
        body.contains("\"vectorBucketArn\""),
        "adoption must answer the full create shape: {body}"
    );
    assert!(
        data_dir.join("cra-vf/bucket.json").is_file(),
        "adoption wrote the ownership marker"
    );

    // cut A x replay-Delete => lockless tail cleanup (§5.3-4 exemption: the
    // empty bucket is deletion residue, not a vb — Delete finishes it)
    let (st, et, body) = delete_vb(port, "crx");
    assert_eq!(st, 200, "replay Delete on cut-A residue: {et:?} {body}");
    assert!(
        !data_dir.join("crx-vf").exists(),
        "tail cleanup removed the residue bucket"
    );

    // cut B x replay-Delete => 无锁清尾 (§5.2 row 4's primary direction)
    let (st, et, body) = delete_vb(port, "dlt");
    assert_eq!(st, 200, "replay Delete on cut-B residue: {et:?} {body}");
    assert!(
        !data_dir.join("dlt-vf").exists(),
        "system bucket raw-deleted"
    );

    // cut B x replay-Create => adoption (§5.2 row 4: 两个方向都收敛)
    let (st, et, body) = create_vb(port, "dlx");
    assert_eq!(st, 200, "replay Create on cut-B residue: {et:?} {body}");
    assert!(data_dir.join("dlx-vf/bucket.json").is_file());

    // enumeration: exactly the two adopted vbs (R9 suffix scan + marker)
    let (st, _, body) = post_json(port, "/ListVectorBuckets", "{}").unwrap();
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mut names: Vec<String> = v["vectorBuckets"]
        .as_array()
        .expect("vectorBuckets array")
        .iter()
        .map(|b| {
            b["vectorBucketName"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["cra".to_string(), "dlx".to_string()], "{body}");
    // the R4 superset echo rides the summary
    assert!(
        body.contains("\"x-vf-placement\""),
        "ListVectorBuckets carries x-vf-placement: {body}"
    );

    // replay Create on an ADOPTED bucket is a plain replay-Conflict (A-F3:
    // AWS replay semantics, hint names the existing bucket)
    let (st, et, body) = create_vb(port, "cra");
    assert_eq!(st, 409, "{body}");
    assert_eq!(et.as_deref(), Some("ConflictException"));
    assert!(
        body.contains("already exists"),
        "conflict hint for our own存量桶: {body}"
    );

    // ---- final sweep: only the daemon home + the two live system buckets; ----
    // NO source bucket was ever created anywhere (D4 — nothing did PutSource)
    replayer.kill9();
    let mut roots: Vec<String> = std::fs::read_dir(&data_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    roots.sort();
    assert_eq!(
        roots,
        vec![".vectorfs".to_string(), "cra-vf".into(), "dlx-vf".into()],
        "data root after the matrix"
    );
}
