//! §5.3 invariant assertions (design-v3.md G1 acceptance): the bucket-model
//! invariants the whole v3.1 axis switch rests on, pinned as tests.
//!
//! 1. 任一 vb 的全部持久自有状态恰在 `X-vf` 一个桶内；源数据恰在 `X`。
//! 2. `X-vf` 存在且含我方 marker ⇔ vb 存在（foreign marker ⇒ not ours）。
//! 3. 源桶内容物 ⊆ 用户对象（vectorfs 永不写源桶系统对象）。
//! 4. 单 vb 任一时刻至多一个写者 — pinned by lock.rs tests + the delete
//!    tail tests in store.rs (referenced, not duplicated here).
//! 5. 三后端下，桶相对键集逐字节同构 (file vs memory end-to-end; the s3
//!    half asserts the derived key/URI mapping — live-endpoint conformance
//!    is G2/PR2's matrix).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use vectorfs_server::backends::{self, Backend};
use vectorfs_server::ops::AppState;
use vectorfs_server::store::{BucketMeta, Storage};

fn fixture_dir() -> std::path::PathBuf {
    let p =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-embedder");
    assert!(
        p.join("model.onnx").is_file(),
        "tiny-embedder fixture missing"
    );
    p
}

/// The tiny pipeline config used across the ingest tests (whole-bucket
/// claim — the preset shape).
fn pipeline_toml() -> String {
    format!(
        "[extractor.plain]\nkind = \"txt\"\n\n\
         [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
         [ingest.docs]\nextract = {{ txt = \"plain\" }}\n\
         chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
         embedders = [\"tiny\"]\nscan_interval_secs = 1\n",
        fixture_dir().display().to_string()
    )
}

async fn seed(storage: &Storage, vb: &str) {
    let meta = BucketMeta {
        vector_bucket_name: vb.to_string(),
        vector_bucket_arn: format!("arn:aws:s3vectors:us-east-1:000000000000:bucket/{vb}"),
        creation_time_rfc3339: "2026-11-06T00:00:00.000Z".into(),
        creation_time_epoch: 0.0,
        deployment: storage.deployment_identity("us-east-1", "000000000000"),
        placement: storage.placement(vb),
        default_index_mode: None,
    };
    storage.create_bucket(&meta).await.unwrap();
}

/// Every object key of a bucket's store (bucket-relative), sorted.
async fn all_keys(storage: &Storage, source: bool, vb: &str) -> BTreeSet<String> {
    let store = if source {
        storage.source_store(vb)
    } else {
        storage.system_store(vb)
    };
    backends::list_all(&*store)
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.as_ref().to_string())
        .collect()
}

/// §5.3-1: after a full ingest pass, ALL persistent vectorfs state lives in
/// `docs-vf` (marker, LOCK, sources, Lance dataset) and the user data lives
/// only in `docs` — nothing else exists at the data root.
#[tokio::test]
async fn invariant_1_all_state_in_system_bucket_data_in_source() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::file(dir.path()));
    let cfg = vectorfs_server::pipeline::config::load_pipeline_config(&pipeline_toml()).unwrap();
    let core = vectorfs_server::ingest::IngestCore::load(&cfg, Path::new("/nonexistent"))
        .await
        .unwrap();
    let state = Arc::new(AppState::new(
        storage.clone(),
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        Some(Arc::new(vectorfs_server::ingest::IngestHandle::new(
            vectorfs_server::ingest::IngestShared::new(core),
        ))),
    ));
    // hold the bucket lock like serve's startup pass
    state.locks.pin("docs");
    state.locks.ensure_held("docs").await.unwrap();

    // a source object (key docs/a.txt inside the source bucket docs/)
    let user_file = dir.path().join("docs/docs/a.txt");
    std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
    std::fs::write(&user_file, "the quick brown fox jumps over the lazy dog").unwrap();

    // no scheduler: drive the scan deterministically
    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = state.ingest.clone().unwrap();
    let rt = vectorfs_server::ingest::IngestRuntime::new(
        Arc::clone(handle.shared()),
        state.clone(),
        handle.notify(),
        rx,
    );
    let _ = tx;
    rt.scan_once().await;
    rt.drain_queue().await;

    // every marker converged INDEXED
    let markers = storage.list_sources("docs").await.unwrap();
    assert_eq!(markers.len(), 1, "{markers:?}");
    assert_eq!(markers[0].status.as_str(), "INDEXED");

    // system bucket: exactly the mechanism keys (marker, LOCK, one source
    // marker, the derived index's json + Lance dataset)
    let sys = all_keys(&storage, false, "docs").await;
    assert!(
        sys.contains("bucket.json") && sys.contains("LOCK"),
        "mechanism keys: {sys:?}"
    );
    assert!(
        sys.iter().any(|k| k.starts_with("sources/")),
        "source marker: {sys:?}"
    );
    assert!(
        sys.iter()
            .any(|k| k.starts_with("indexes/") && k.ends_with("/index.json")),
        "derived index meta: {sys:?}"
    );
    assert!(
        sys.iter().any(|k| k.contains("/data/")),
        "lance dataset: {sys:?}"
    );
    // source bucket: EXACTLY the user object (§5.3-3 zero system objects)
    let src = all_keys(&storage, true, "docs").await;
    assert_eq!(src, BTreeSet::from(["docs/a.txt".to_string()]), "{src:?}");

    // the physical data root holds ONLY the bucket trees (docs/, docs-vf/)
    // — the daemon home .vectorfs/ appears only when config/flock is used,
    // which this fixture does not exercise
    let roots: BTreeSet<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        roots,
        BTreeSet::from(["docs".into(), "docs-vf".into()]),
        "{roots:?}"
    );
}

/// §5.3-2: `X-vf` + OUR marker ⇔ vb exists. A foreign deployment's marker
/// (or a marker-less `-vf` bucket) is NOT a vb of ours — GetVectorBucket
/// 404s and enumeration skips it.
#[tokio::test]
async fn invariant_2_marker_identity_is_existence() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::file(dir.path());
    let identity = storage.deployment_identity("us-east-1", "000000000000");
    seed(&storage, "docs").await;

    // ours: listed + loadable
    let listed = storage.list_buckets(&identity).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|m| m.vector_bucket_name.as_str())
            .collect::<Vec<_>>(),
        vec!["docs"]
    );
    assert!(storage.load_bucket("docs").await.unwrap().is_some());

    // a FOREIGN marker under kb-vf: exists physically, not ours
    let foreign_meta = BucketMeta {
        vector_bucket_name: "kb".into(),
        vector_bucket_arn: "arn".into(),
        creation_time_rfc3339: "t".into(),
        creation_time_epoch: 0.0,
        deployment: vectorfs_server::store::DeploymentIdentity {
            region: "eu-west-1".into(),
            account: "999999999999".into(),
            endpoint: storage.backend().endpoint_id(),
        },
        placement: storage.placement("kb"),
        default_index_mode: None,
    };
    storage.create_bucket(&foreign_meta).await.unwrap();
    // a marker-less -vf directory (somebody's real bucket)
    std::fs::create_dir_all(dir.path().join("plain-vf")).unwrap();

    let listed = storage.list_buckets(&identity).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|m| m.vector_bucket_name.as_str())
            .collect::<Vec<_>>(),
        vec!["docs"],
        "foreign markers and marker-less -vf buckets are not ours"
    );
    // the ops layer 404s the foreign bucket (require_bucket identity filter)
    let state = AppState::new(
        Arc::new(Storage::file(dir.path())),
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        None,
    );
    let resp = vectorfs_server::ops::get_vector_bucket(
        axum::extract::State(state.clone()),
        bytes::Bytes::from_static(br#"{"vectorBucketName":"kb"}"#),
    )
    .await;
    assert_eq!(resp.unwrap_err().status, axum::http::StatusCode::NOT_FOUND);
}

/// §5.3-3 (+5.3-1 memory half): after a full ingest on the MEMORY backend,
/// the source bucket key set is exactly the user objects — and (5.3-5) the
/// SYSTEM bucket key sets of file and memory are byte-identical for the
/// same operation sequence.
#[tokio::test]
async fn invariant_3_and_5_source_purity_and_cross_backend_isomorphism() {
    // ---- memory backend, same sequence as invariant_1 ----
    let mem_storage = Arc::new(Storage::memory());
    let cfg = vectorfs_server::pipeline::config::load_pipeline_config(&pipeline_toml()).unwrap();
    let core = vectorfs_server::ingest::IngestCore::load(&cfg, Path::new("/nonexistent"))
        .await
        .unwrap();
    let state = Arc::new(AppState::new(
        mem_storage.clone(),
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        Some(Arc::new(vectorfs_server::ingest::IngestHandle::new(
            vectorfs_server::ingest::IngestShared::new(core),
        ))),
    ));
    state.locks.pin("docs");
    state.locks.ensure_held("docs").await.unwrap();
    // the user object goes through the REAL PutSource path (lazy source
    // bucket creation included)
    let headers = vec![
        ("x-vf-bucket", "docs"),
        ("x-vf-file-name", "docs%2Fa.txt"), // percent-encoded non-ASCII-free key
    ];
    let mut hm = axum::http::HeaderMap::new();
    for (k, v) in headers {
        hm.insert(axum::http::HeaderName::from_static(k), v.parse().unwrap());
    }
    let resp = vectorfs_server::ops_xvf::put_source(
        axum::extract::State(state.as_ref().clone()),
        hm,
        bytes::Bytes::from_static(b"the quick brown fox jumps over the lazy dog"),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let _ = tx;
    let handle = state.ingest.clone().unwrap();
    let rt = vectorfs_server::ingest::IngestRuntime::new(
        Arc::clone(handle.shared()),
        state.clone(),
        handle.notify(),
        rx,
    );
    rt.scan_once().await;
    rt.drain_queue().await;

    let markers = mem_storage.list_sources("docs").await.unwrap();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].status.as_str(), "INDEXED");

    // §5.3-3 (memory): the source bucket holds EXACTLY the user object
    let mem_src = all_keys(&state.storage, true, "docs").await;
    assert_eq!(
        mem_src,
        BTreeSet::from(["docs/a.txt".to_string()]),
        "{mem_src:?}"
    );

    // ---- §5.3-5: system-bucket key sets byte-identical across backends ----
    // file half: the same sequence (seed via a raw file write; the ingest
    // pass then derives the identical system-bucket key set)
    let dir = tempfile::tempdir().unwrap();
    // an identical core for the file half (cores are not Clone)
    let fcfg = vectorfs_server::pipeline::config::load_pipeline_config(&pipeline_toml()).unwrap();
    let fcore = vectorfs_server::ingest::IngestCore::load(&fcfg, Path::new("/nonexistent"))
        .await
        .unwrap();
    let fstate = Arc::new(AppState::new(
        Arc::new(Storage::file(dir.path())),
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        Some(Arc::new(vectorfs_server::ingest::IngestHandle::new(
            vectorfs_server::ingest::IngestShared::new(fcore),
        ))),
    ));
    fstate.locks.pin("docs");
    fstate.locks.ensure_held("docs").await.unwrap();
    std::fs::create_dir_all(dir.path().join("docs/docs")).unwrap();
    std::fs::write(
        dir.path().join("docs/docs/a.txt"),
        "the quick brown fox jumps over the lazy dog",
    )
    .unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let _ = tx;
    let fhandle = fstate.ingest.clone().unwrap();
    let frt = vectorfs_server::ingest::IngestRuntime::new(
        Arc::clone(fhandle.shared()),
        fstate.clone(),
        fhandle.notify(),
        rx,
    );
    frt.scan_once().await;
    frt.drain_queue().await;
    let file_sys = all_keys(&fstate.storage, false, "docs").await;
    let mem_sys = all_keys(&state.storage, false, "docs").await;

    // The Lance dataset carries content-addressed file names and the LOCK a
    // writer id — normalize those two families before the byte comparison
    // (the invariant is about KEY SHAPES, not identical payload identities).
    let normalize = |keys: &BTreeSet<String>| -> BTreeSet<String> {
        keys.iter()
            .map(|k| {
                if k == "LOCK" {
                    "LOCK".to_string()
                } else if k.starts_with("sources/") {
                    "sources/<hash>.json".to_string()
                } else if let Some(rest) = k.strip_prefix("indexes/") {
                    // the derived index NAME is deterministic (config hash)
                    // but lance payload files are content/uuid-addressed:
                    // keep the structural shape, anonymize the leaf names
                    let (ix, tail) = rest.split_once('/').unwrap_or((rest, ""));
                    let _ = ix;
                    if let Some(inner) = tail.strip_prefix("data/vectors.lance/") {
                        let (dir, leaf) = inner.split_once('/').unwrap_or((inner, ""));
                        format!("indexes/<ix>/data/vectors.lance/{dir}/<file>")
                            .replace("<file>", if leaf.is_empty() { "" } else { "<file>" })
                    } else {
                        format!("indexes/<ix>/{tail}")
                    }
                } else {
                    k.clone()
                }
            })
            .collect()
    };
    assert_eq!(
        normalize(&file_sys),
        normalize(&mem_sys),
        "system-bucket key sets must be isomorphic across file/memory:\n file: {file_sys:?}\n mem:  {mem_sys:?}"
    );

    // s3 half of 5.3-5: the derived mapping is identical by construction
    // (live-endpoint conformance is G2/PR2's matrix)
    let s3cfg = backends::S3BackendConfig {
        endpoint: "http://s3.example:9000".into(),
        region: "us-east-1".into(),
        access_key: None,
        secret_key: None,
    };
    let s3 = Backend::S3(Box::new(s3cfg));
    assert_eq!(
        s3.lance_uri(&backends::system_bucket_name("docs", "-vf"), "ix"),
        "s3://docs-vf/indexes/ix/data"
    );
    assert_eq!(
        Backend::Memory.lance_uri(&backends::system_bucket_name("docs", "-vf"), "ix"),
        "memory://docs-vf/indexes/ix/data"
    );
}

/// §5.3-4 (companion, file backend): two daemons on one data root — the
/// second writer's put_vectors on a held bucket answers 409 (the per-bucket
/// single-writer invariant); a bucket only the FIRST daemon locked stays
/// writable by it and untouched by the second.
#[tokio::test]
async fn invariant_4_single_writer_per_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let d1 = AppState::new(
        Arc::new(Storage::file(dir.path())),
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        None,
    );
    seed(&d1.storage, "docs").await;
    d1.locks.pin("docs");
    d1.locks.ensure_held("docs").await.unwrap();

    // an index the first daemon created (a valid write target)
    d1.storage
        .create_index(&vectorfs_server::store::IndexMeta {
            vector_bucket_name: "docs".into(),
            index_name: "ix".into(),
            index_arn: "arn".into(),
            creation_time_rfc3339: "t".into(),
            creation_time_epoch: 0.0,
            data_type: "float32".into(),
            dimension: 2,
            distance_metric: "cosine".into(),
            non_filterable_metadata_keys: None,
            index_mode: None,
            x_vf_derived: None,
        })
        .await
        .unwrap();
    // a second daemon (fresh registry + writer id): same bucket -> the
    // single-writer 409 on a valid-shaped write
    let d2 = AppState::new(
        Arc::new(Storage::file(dir.path())),
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        None,
    );
    let body = serde_json::json!({
        "vectorBucketName": "docs",
        "indexName": "ix",
        "vectors": [{"key": "k1", "data": {"float32": [0.1, 0.2]}}],
    });
    let err = vectorfs_server::ops::put_vectors(
        axum::extract::State(d2.clone()),
        bytes::Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, axum::http::StatusCode::CONFLICT, "{err:?}");
    assert!(err.message.contains("locked by another writer"), "{err:?}");
    // ...and the FIRST daemon freely takes a DIFFERENT bucket (per-bucket
    // granularity: one contested bucket parks nothing else)
    seed(&d1.storage, "kb").await;
    d1.locks
        .ensure_held("kb")
        .await
        .expect("uncontested bucket acquires");
    assert_eq!(d1.locks.held_buckets().len(), 2);
}
