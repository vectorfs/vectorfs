//! G2 s3 conformance (design-v3.md §3-R8, §9 risk row 1, §10-G2) against a
//! LIVE S3-compatible endpoint, in two layers:
//!
//! 1. [`s3admin_four_verbs_conformance_matrix`] — the raw admin face
//!    (s3admin.rs): create/head/list/delete + the CreateBucket IDEMPOTENCY
//!    FORK (R8: the caller converges on "create then HeadBucket + marker",
//!    never on the backend's error-code split) + the NAME-RULE rows
//!    (underscore / invalid name) + delete-404 tolerance.
//! 2. [`s3_backend_full_daemon_path`] — the whole v3 bucket-pair lifecycle
//!    through a REAL serve process on the s3 backend: init → serve →
//!    CreateVectorBucket (dual-bucket assertion: `X-vf` exists, `X` does
//!    NOT — lazy, D4) → PutSource (lazy source-bucket creation on the real
//!    endpoint) → scan ingest (tiny fixture embedder) → QueryVectors →
//!    DeleteVectorBucket 删净 (`X-vf` gone, `X` and its user object
//!    untouched).
//!
//! Env (the experiments.rs convention): run with
//! `VF_S3CP_URI='s3+http://127.0.0.1:19000' cargo test -p vectorfs-server
//! --test s3_conformance -- --ignored`
//! plus optional `VF_S3_ACCESS_KEY`/`VF_S3_SECRET_KEY` (anonymous when
//! unset — SeaweedFS `weed server -s3` without IAM). Optional knobs:
//! `VF_S3CP_UNDERSCORE=accept|reject` (default **reject**: probed
//! SeaweedFS 4.50 answers 400 InvalidBucketName on underscores — the
//! design-v3.md §2 row "仅 SeaweedFS 默认宽容" records the older 3.x
//! behavior; MinIO/R2/AWS reject everywhere) and `VF_S3CP_LIST_403=1`
//! (only provable where the endpoint enforces IAM — an anonymous endpoint
//! ignores signatures entirely, so the wrong-credential ListBuckets row is
//! opt-in for MinIO/R2 CI runs).

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vectorfs_server::backends::S3BackendConfig;
use vectorfs_server::s3admin::{CreateOutcome, S3Admin};
use vectorfs_server::store::Storage;

// object_store 0.14: `get` rides ObjectStoreExt, `list` the base trait
use object_store::ObjectStore as _;
use object_store::ObjectStoreExt as _;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/server/../../ is the repo root")
        .to_path_buf()
}

fn ort_lib() -> PathBuf {
    let lib = if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    let p = repo_root().join("runtime-libs").join(lib);
    assert!(
        p.is_file(),
        "s3 full-path test needs the real ONNX Runtime at {} — run \
         scripts/fetch_runtime_libs.sh onnxruntime; tests refuse to skip",
        p.display()
    );
    p
}

fn fixture_dir() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-embedder");
    assert!(
        p.join("model.onnx").is_file(),
        "tiny-embedder fixture missing"
    );
    p
}

fn s3_config() -> S3BackendConfig {
    let uri = std::env::var("VF_S3CP_URI").expect("VF_S3CP_URI=s3+http://host:port");
    let cfg = S3BackendConfig::parse(&uri).expect("parse s3+ endpoint uri");
    S3BackendConfig {
        access_key: std::env::var("VF_S3_ACCESS_KEY").ok().or(cfg.access_key),
        secret_key: std::env::var("VF_S3_SECRET_KEY").ok().or(cfg.secret_key),
        ..cfg
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// layer 1: the raw admin face (s3admin.rs, R8)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs live S3 endpoint; set VF_S3CP_URI (see module doc)"]
fn s3admin_four_verbs_conformance_matrix() {
    let cfg = s3_config();
    rt().block_on(async move {
        let admin = S3Admin::new(&cfg);
        let run = std::process::id();
        let bucket = format!("vfconf{run}");

        // clean slate (a previous run's debris must not fake a fork result)
        let _ = admin.delete_bucket(&bucket).await;

        // head on a missing bucket is Ok(false) — 404 is an answer, not an
        // error (probe callers branch on existence, doctor on failure)
        assert!(
            !admin.head_bucket(&bucket).await.unwrap(),
            "HeadBucket(missing) = Ok(false)"
        );

        // create -> Created
        assert_eq!(
            admin.create_bucket(&bucket).await.unwrap(),
            CreateOutcome::Created
        );

        // THE IDEMPOTENCY FORK (§9 row 1): re-create of an owned bucket
        // diverges per backend (us-east-1 200 / AWS 409
        // BucketAlreadyOwnedByYou / MinIO 409). Whatever the endpoint
        // answers, S3Admin folds it to AlreadyExists and the lifecycle
        // judges from the marker — assert the FOLD, log the raw shape.
        let again = admin.create_bucket(&bucket).await;
        match again {
            Ok(CreateOutcome::AlreadyExists) => {
                eprintln!("conformance: CreateBucket(existing) -> 409 (folded to AlreadyExists)")
            }
            Ok(CreateOutcome::Created) => {
                // a 200-on-recreate endpoint (classic us-east-1): the fold
                // must NOT lie — HeadBucket still tells the truth, which is
                // all the R8 converge rule needs
                eprintln!(
                    "conformance: CreateBucket(existing) -> 200 (us-east-1 style); converging \
                     via HeadBucket"
                );
                assert!(admin.head_bucket(&bucket).await.unwrap());
            }
            Err(e) => panic!("CreateBucket(existing) surfaced as an error (must fold): {e}"),
        }

        // head + list membership
        assert!(admin.head_bucket(&bucket).await.unwrap());
        let names = admin.list_bucket_names().await.unwrap();
        assert!(
            names.contains(&bucket),
            "ListBuckets must carry the created bucket: {names:?}"
        );

        // delete -> gone from head AND list; deleting a missing bucket is
        // success (the replay/cleanup path may race us — s3admin.rs doc)
        admin.delete_bucket(&bucket).await.unwrap();
        assert!(!admin.head_bucket(&bucket).await.unwrap());
        let names = admin.list_bucket_names().await.unwrap();
        assert!(
            !names.contains(&bucket),
            "ListBuckets after delete must not carry it: {names:?}"
        );
        admin.delete_bucket(&bucket).await.unwrap();

        // NAME-RULE row (§2): underscores. Default expectation = reject
        // (probed: SeaweedFS 4.50 and MinIO both 400; the §2 "seaweed
        // lenient" row is the 3.x behavior). VF_S3CP_UNDERSCORE=accept
        // records a lenient backend.
        let under = format!("vf_conf_{run}");
        match std::env::var("VF_S3CP_UNDERSCORE").ok().as_deref() {
            Some("accept") => {
                let outcome = admin.create_bucket(&under).await.unwrap();
                eprintln!("conformance: underscore name accepted ({outcome:?})");
                admin.delete_bucket(&under).await.unwrap();
            }
            _ => {
                let e = admin.create_bucket(&under).await.unwrap_err();
                assert_eq!(
                    e.status,
                    Some(400),
                    "underscore bucket name must be rejected: {e}"
                );
            }
        }

        // NAME-RULE row: a plainly invalid name surfaces as AdminError with
        // its HTTP status (store.rs maps this shape to ValidationException
        // with the bucket_suffix hint, §5.2 last-but-one row)
        let e = admin.create_bucket("VFConf!!").await.unwrap_err();
        assert_eq!(e.status, Some(400), "InvalidBucketName shape: {e}");

        // PERMISSION-ERROR row (§9 ListBuckets risk): only provable where
        // IAM exists. An anonymous endpoint (SeaweedFS without IAM) ignores
        // signatures entirely, so wrong credentials still answer 200 — the
        // row is opt-in via VF_S3CP_LIST_403=1 for MinIO/R2 CI runs.
        if std::env::var("VF_S3CP_LIST_403").ok().as_deref() == Some("1") {
            let bad = S3BackendConfig {
                access_key: Some("vf-wrong-key".into()),
                secret_key: Some("vf-wrong-secret".into()),
                ..cfg.clone()
            };
            let e = S3Admin::new(&bad)
                .list_bucket_names()
                .await
                .expect_err("wrong credentials must fail ListBuckets");
            assert_eq!(e.status, Some(403), "AccessDenied shape: {e}");
        } else {
            eprintln!(
                "conformance: ListBuckets-permission row skipped (anonymous endpoint; set \
                 VF_S3CP_LIST_403=1 against an IAM-enforcing endpoint)"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// layer 2: the full daemon path on the s3 backend (§10-G2 全路径)
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn spawn(data_dir: &Path, port: u16, backend_spec: &str) -> Daemon {
        let log = data_dir.parent().unwrap().join("s3-fullpath-serve.log");
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
            .env("VF_BACKEND", backend_spec)
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

/// wait_healthy with the daemon's log dumped on timeout (the s3 backend can
/// fail to start in interesting remote ways the healthz poll hides).
fn wait_healthy_or_dump(daemon: &Daemon, port: u16, deadline: Instant) {
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
    daemon.dump_log_tail("never healthy");
    panic!("serve never became healthy on port {port}");
}

/// (status, x-amzn-errortype, body) of one request (the shared raw-HTTP
/// shape; extra headers for the /x-vf file face).
fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Option<String>, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: vf-test\r\ncontent-length: {}\r\n\
         connection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).expect("write head");
    s.write_all(body).expect("write body");
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
    let resp_body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, errortype, resp_body)
}

fn post_json(port: u16, path: &str, body: &str) -> (u16, Option<String>, String) {
    request(
        port,
        "POST",
        path,
        &[("content-type", "application/json")],
        body.as_bytes(),
    )
}

#[test]
#[ignore = "needs live S3 endpoint; set VF_S3CP_URI (see module doc)"]
fn s3_backend_full_daemon_path() {
    let cfg = s3_config();
    let backend_spec = format!("s3+{}", cfg.endpoint);
    let run = std::process::id();
    let vb = format!("s3fp{run}");
    let sys = format!("{vb}-vf");
    let doc = b"the quick brown fox jumps over the lazy dog";

    // ---- clean slate on the shared endpoint (rerun safety) ----
    rt().block_on({
        let cfg = cfg.clone();
        let vb = vb.clone();
        async move {
            let admin = S3Admin::new(&cfg);
            let _ = admin.delete_bucket(&vb).await;
            let _ = admin.delete_bucket(&format!("{vb}-vf")).await;
        }
    });

    // ---- init: the daemon home + the tiny ingest pipeline ----
    let tmp = tempfile::tempdir().expect("tmp");
    let home = tmp.path().join("home");
    let home_dot = home.join(".vectorfs");
    std::fs::create_dir_all(&home_dot).unwrap();
    std::fs::write(
        home_dot.join("vectorfs.toml"),
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

[ingest.{vb}]
extract = {{ txt = "plain" }}
chunk = {{ kind = "token-window", size = 16, overlap = 4 }}
embedders = ["tiny"]
scan_interval_secs = 1
retries = 2
index-prefix = "docs"
"#,
            models = home_dot.join("models").display(),
            fixture = fixture_dir().display(),
            vb = vb,
        ),
    )
    .unwrap();

    let port = free_port();
    let mut daemon = Daemon::spawn(&home, port, &backend_spec);
    wait_healthy_or_dump(&daemon, port, Instant::now() + Duration::from_secs(60));

    // ---- CreateVectorBucket: the DUAL-BUCKET assertion on the real endpoint ----
    // (G2: `X-vf` created, `X` NOT — lazy source, D4; the s3 half PR1 never
    // actually ran)
    let (st, et, body) = post_json(
        port,
        "/CreateVectorBucket",
        &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
    );
    assert_eq!(st, 200, "create: {et:?} {body}");
    assert!(body.contains("\"vectorBucketArn\""), "{body}");

    rt().block_on({
        let cfg = cfg.clone();
        let (vb, sys) = (vb.clone(), sys.clone());
        async move {
            let admin = S3Admin::new(&cfg);
            let names = admin.list_bucket_names().await.unwrap();
            assert!(
                names.contains(&sys),
                "system bucket {sys} must exist on the endpoint: {names:?}"
            );
            assert!(
                !names.contains(&vb),
                "source bucket {vb} must NOT exist yet (lazy, D4): {names:?}"
            );
            assert!(admin.head_bucket(&sys).await.unwrap());
            assert!(!admin.head_bucket(&vb).await.unwrap());
        }
    });

    // ---- PutSource: lazily CREATES the source bucket on the real endpoint ----
    let (st, et, body) = request(
        port,
        "POST",
        "/x-vf/PutSource",
        &[
            ("content-type", "text/plain"),
            ("x-vf-bucket", &vb),
            // percent-encoded key (the /x-vf face rule, ops_xvf.rs)
            ("x-vf-file-name", "docs%2Fa.txt"),
        ],
        doc,
    );
    assert_eq!(st, 202, "PutSource: {et:?} {body}");

    // the source bucket now exists and holds EXACTLY the user object
    // (§5.3-3: zero system objects in the source bucket — verified through
    // the raw object store, not our daemon's view)
    rt().block_on({
        let cfg = cfg.clone();
        let vb = vb.clone();
        let sys = sys.clone();
        async move {
            let admin = S3Admin::new(&cfg);
            let names = admin.list_bucket_names().await.unwrap();
            assert!(
                names.contains(&vb) && names.contains(&sys),
                "dual buckets after PutSource: {names:?}"
            );
            let store = cfg.build_bucket_store(&vb).unwrap();
            let got = store
                .get(&object_store::path::Path::from("docs/a.txt"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(&got[..], &doc[..], "user object bytes round-trip");
            use futures_util::TryStreamExt as _;
            let keys: Vec<String> = store
                .list(None)
                .map_ok(|m| m.location.to_string())
                .try_collect()
                .await
                .unwrap();
            assert_eq!(
                keys,
                vec!["docs/a.txt".to_string()],
                "source purity: {keys:?}"
            );
        }
    });

    // ---- scan ingest: poll the source markers until INDEXED ----
    // (startup could not lock the not-yet-existing bucket — §5.1 skip; the
    // Create op's require_write_lock recovers it and the next scan picks the
    // file up, so the budget covers one heartbeat round: ~TTL/3 + scans)
    let deadline = Instant::now() + Duration::from_secs(180);
    let vb_probe = vb.clone();
    let (index_name, dim) = rt().block_on({
        let storage = Storage::new(
            vectorfs_server::backends::Backend::S3(Box::new(cfg.clone())),
            "-vf",
        )
        .unwrap();
        async move {
            loop {
                let markers = storage.list_sources(&vb_probe).await.unwrap();
                if markers.len() == 1 && markers[0].status.as_str() == "INDEXED" {
                    let chunks = markers[0]
                        .chunks
                        .as_ref()
                        .and_then(|c| c.get("text").copied())
                        .unwrap_or(0);
                    assert!(chunks > 0, "INDEXED marker carries chunk counts");
                    let indexes = storage.list_indexes(&vb_probe).await.unwrap();
                    let derived = indexes
                        .iter()
                        .find(|m| m.x_vf_derived.is_some())
                        .unwrap_or_else(|| panic!("derived index after ingest: {indexes:?}"));
                    return (derived.index_name.clone(), derived.dimension);
                }
                assert!(
                    Instant::now() < deadline,
                    "source never converged INDEXED (markers: {markers:?})"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    assert_eq!(dim, 8, "tiny fixture dimension");
    eprintln!("s3 full-path: ingested, derived index {index_name}");

    // ---- QueryVectors through the wire face ----
    let (st, et, body) = post_json(
        port,
        "/QueryVectors",
        &format!(
            r#"{{"vectorBucketName":"{vb}","indexName":"{index_name}","queryVector":{{"float32":[1.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}},"topK":3,"returnDistance":true,"returnMetadata":true}}"#
        ),
    );
    assert_eq!(st, 200, "query: {et:?} {body}");
    assert!(body.contains("\"vectors\""), "{body}");
    assert!(
        body.contains("docs/a.txt"),
        "the hit must join back to the source object: {body}"
    );

    // ---- Delete: non-empty 409 first (AWS 实测语义), then 删净 ----
    let (st, et, body) = post_json(
        port,
        "/DeleteVectorBucket",
        &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
    );
    assert_eq!(st, 409, "delete of an indexed bucket: {body}");
    assert_eq!(et.as_deref(), Some("ConflictException"), "{body}");
    assert!(body.contains("indexes"), "{body}");

    let (st, et, body) = post_json(
        port,
        "/DeleteIndex",
        &format!(r#"{{"vectorBucketName":"{vb}","indexName":"{index_name}"}}"#),
    );
    assert_eq!(st, 200, "delete derived index: {et:?} {body}");

    let (st, et, body) = post_json(
        port,
        "/DeleteVectorBucket",
        &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
    );
    assert_eq!(st, 200, "delete bucket: {et:?} {body}");

    // ---- 删净 assertion: system bucket GONE, source bucket + user object
    // SURVIVE (D4: the source bucket is the user's, vectorfs never deletes) ----
    rt().block_on({
        let cfg = cfg.clone();
        let (vb, sys) = (vb.clone(), sys.clone());
        async move {
            let admin = S3Admin::new(&cfg);
            let names = admin.list_bucket_names().await.unwrap();
            assert!(
                !names.contains(&sys),
                "system bucket raw-deleted by DeleteVectorBucket: {names:?}"
            );
            assert!(
                names.contains(&vb),
                "source bucket survives (D4, never deleted): {names:?}"
            );
            assert!(!admin.head_bucket(&sys).await.unwrap());
            let store = cfg.build_bucket_store(&vb).unwrap();
            let got = store
                .get(&object_store::path::Path::from("docs/a.txt"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(&got[..], &doc[..], "user data survived the vb delete");
        }
    });

    // the daemon itself stays healthy and consistent afterwards
    wait_healthy(port, Instant::now() + Duration::from_secs(5));
    let (st, _, body) = post_json(
        port,
        "/GetVectorBucket",
        &format!(r#"{{"vectorBucketName":"{vb}"}}"#),
    );
    assert_eq!(st, 404, "deleted vb is gone: {body}");
    daemon.kill9();
}
