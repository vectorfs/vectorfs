//! G3 文件层验收门 (design-v3.md §10-G3, R5/§5.2): the file-layer assertions
//! no PR claimed — pinned here as tests.
//!
//! 1. **include/exclude 收窄不 mass-flip** (R5: "include/exclude 收窄不得
//!    触发 mass-flip——仅作用于认领与 vanish-flip"): narrowing the claim
//!    filter leaves already-INDEXED sources byte-untouched (no PENDING
//!    rewrite, no strike, no DELETING flip), and the GC orphan live-set
//!    stays the FULL bucket listing (narrowing must not starve orphans into
//!    reaping the narrowed-out source's chunks).
//! 2. **空 listing 故障注入 (N-strike 护栏)** (R5 双护栏, §5.2 last row): a
//!    backend list that succeeds but returns EMPTY (outage / permission
//!    shape) must not mass-delete — 护栏① (N-strike) refuses the flip on the
//!    first absent scan, 护栏② (live==0 && markers>0) refuses reap AND sweep
//!    no matter how many strikes accumulate.
//! 3. **memory 桶间隔离** (R1: memory = one shared key space, per-bucket
//!    `X`/`X-vf` PrefixStore views): two vector buckets ingest side by side
//!    with zero cross-visibility — even the SAME object key in both buckets
//!    yields two independent markers/indexes.
//! 4. **GC 换轴: 作用域 = 持锁集** (R5/B-F8): scan decisions and both GC
//!    doors (reap + orphan sweep) never touch a bucket whose LOCK this
//!    daemon does not hold; 整桶扫描 has no exclusion list — source-bucket
//!    keys shaped like system keys are ordinary user files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use vectorfs_server::backends;
use vectorfs_server::ingest::{IngestCore, IngestHandle, IngestRuntime, IngestShared};
use vectorfs_server::ops::AppState;
use vectorfs_server::pipeline::config::{PipelineConfig, load_pipeline_config};
use vectorfs_server::store::{
    BucketMeta, DerivedIndexInfo, IndexMeta, MetaMirror, SourceMarker, SourceStatus, Storage,
    VectorRow,
};

// ---------------------------------------------------------------------------
// harness (the bucket_model_invariants.rs shape: deterministic scan_once +
// drain_queue, locks held like serve's startup pass)
// ---------------------------------------------------------------------------

fn fixture_dir() -> std::path::PathBuf {
    let p =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-embedder");
    assert!(
        p.join("model.onnx").is_file(),
        "tiny-embedder fixture missing"
    );
    p
}

/// The pipeline preamble (txt extractor -> tiny ort embedder); `sections`
/// carries the full `[ingest.X]` bodies.
fn config_text(sections: &str) -> String {
    format!(
        "[extractor.plain]\nkind = \"txt\"\n\n\
         [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
         {sections}",
        fixture_dir().display().to_string(),
    )
}

/// One `[ingest.<vb>]` section; `claim_fields` splices in the claim-filter
/// lines (`include = [...]` / `exclude = [...]`) — empty = whole bucket.
fn ingest_section(vb: &str, claim_fields: &str) -> String {
    format!(
        "[ingest.{vb}]\n{claim_fields}\
         extract = {{ txt = \"plain\" }}\n\
         chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
         embedders = [\"tiny\"]\nscan_interval_secs = 1\n\n"
    )
}

/// Test-mode runtime over an EXISTING storage: no spawned tasks, the test
/// drives `scan_once` + `drain_queue` (same construction order as
/// bucket_model_invariants.rs).
async fn runtime_over(
    storage: Arc<Storage>,
    text: &str,
) -> (
    Arc<IngestRuntime>,
    Arc<IngestHandle>,
    Arc<AppState>,
    PipelineConfig,
) {
    let cfg = load_pipeline_config(text).unwrap();
    let core = IngestCore::load(&cfg, Path::new("/nonexistent-models"))
        .await
        .expect("fixture embedder loads");
    let state = Arc::new(AppState::new(
        storage,
        None,
        "us-east-1".into(),
        "000000000000".into(),
        false,
        Some(Arc::new(IngestHandle::new(IngestShared::new(core)))),
    ));
    let handle = state.ingest.clone().unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let rt = IngestRuntime::new(
        Arc::clone(handle.shared()),
        state.clone(),
        handle.notify(),
        rx,
    );
    (rt, handle, state, cfg)
}

/// Pin + acquire the writer locks for every configured bucket — serve's
/// startup pass (§5.1). Tests asserting the UNHELD skip pass hold a subset
/// by hand instead.
async fn hold_locks(state: &AppState, cfg: &PipelineConfig) {
    for vb in cfg.ingest.keys() {
        state.locks.pin(vb);
        state.locks.ensure_held(vb).await.unwrap();
    }
}

/// First 16 hex of sha256(key) — the marker file name base / chunk uri
/// (`ingest::marker::object_key_hash`).
fn key_hash(key: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(key.as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A derived-shaped chunk key (`ingest::marker::chunk_key`).
fn chunk_key(uri: &str, seq: u32) -> String {
    format!("{uri}-text-{seq}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

async fn marker_of(storage: &Storage, vb: &str, key: &str) -> Option<SourceMarker> {
    storage.load_source(vb, &key_hash(key)).await.unwrap()
}

async fn expect_status(storage: &Storage, vb: &str, key: &str, want: SourceStatus) -> SourceMarker {
    let m = marker_of(storage, vb, key)
        .await
        .unwrap_or_else(|| panic!("marker for {vb}:{key} not found"));
    assert_eq!(m.status, want, "{vb}:{key}: {m:?}");
    m
}

/// The (single) ingest-derived index of a bucket (the x_vf_derived stamp).
async fn derived_index(storage: &Storage, vb: &str) -> String {
    storage
        .list_indexes(vb)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.x_vf_derived.is_some())
        .unwrap_or_else(|| panic!("no derived index for {vb}"))
        .index_name
}

async fn vec_count(storage: &Storage, vb: &str, ix: &str) -> usize {
    storage.count_vectors(vb, ix).await.unwrap()
}

/// Every object key of one bucket's store (bucket-relative), sorted.
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

/// One orphan-shaped row (chunk-format key + `x-vf-src-uri` attribution)
/// planted directly through the store — the probe the orphan sweep hunts.
fn orphan_row(uri: &str, seq: u32) -> VectorRow {
    VectorRow {
        key: chunk_key(uri, seq),
        vector: vec![0.5; 8],
        metadata_json: Some(format!(r#"{{"x-vf-src-uri":"{uri}"}}"#)),
        filterable: BTreeMap::from([(
            "x-vf-src-uri".to_string(),
            MetaMirror::Str(uri.to_string()),
        )]),
    }
}

// ---------------------------------------------------------------------------
// G3-1: include/exclude 收窄不 mass-flip (R5)
// ---------------------------------------------------------------------------

/// Narrowing `include` must leave an already-INDEXED source that falls out
/// of the filter byte-untouched (INDEXED, misses 0, same updated_at — no
/// PENDING rewrite, no strike, no flip), while the idle orphan sweep —
/// PROVEN to run by a planted orphan — keeps the narrowed-out source's
/// chunks alive (the sweep's live set is the FULL bucket listing, R5:
/// "孤儿判定的 live 集恒为整桶列表").
#[tokio::test]
async fn include_narrowing_never_flips_indexed_sources_or_reaps_their_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::file(dir.path()));

    // (1) wide config: docs/** claims both files; ingest to INDEXED
    let wide = config_text(&ingest_section("docs", "include = [\"docs/**\"]\n"));
    let (rt, _h, state, cfg_wide) = runtime_over(storage.clone(), &wide).await;
    hold_locks(&state, &cfg_wide).await;
    let docs = dir.path().join("docs").join("docs");
    std::fs::create_dir_all(docs.join("keep")).unwrap();
    std::fs::write(
        docs.join("a.txt"),
        "the document that will fall out of the claim filter",
    )
    .unwrap();
    std::fs::write(
        docs.join("keep").join("b.txt"),
        "the document that stays claimed after the narrowing",
    )
    .unwrap();
    rt.scan_once().await;
    rt.drain_queue().await;

    let a1 = expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    let b1 = expect_status(&storage, "docs", "docs/keep/b.txt", SourceStatus::Indexed).await;
    let ix = derived_index(&storage, "docs").await;
    let a_chunks = a1.chunks.as_ref().unwrap()["text"] as usize;
    let b_chunks = b1.chunks.as_ref().unwrap()["text"] as usize;
    assert!(a_chunks >= 1 && b_chunks >= 1);
    // a planted ORPHAN (uri of an object that never existed): the sweep's
    // canary — it must disappear on the next idle pass, proving the sweep
    // actually ran when the narrowed-out chunks survive it
    storage
        .upsert_vectors(
            "docs",
            &ix,
            8,
            vec![orphan_row(&key_hash("docs/ghost.txt"), 0)],
        )
        .await
        .unwrap();
    assert_eq!(
        vec_count(&storage, "docs", &ix).await,
        a_chunks + b_chunks + 1
    );

    // (2) restart under the NARROWED config (docs/keep/** only). Premise
    // pin: include/exclude are claim-only fields — the fingerprint must not
    // move (config.rs "deliberately EXCLUDED"), or the markers would be
    // stale garbage rather than settled truth
    state.locks.release_all().await;
    let narrow = config_text(&ingest_section("docs", "include = [\"docs/keep/**\"]\n"));
    let (rt2, handle2, state2, cfg_narrow) = runtime_over(storage.clone(), &narrow).await;
    hold_locks(&state2, &cfg_narrow).await;
    assert_eq!(
        cfg_narrow.fingerprint("docs").unwrap(),
        cfg_wide.fingerprint("docs").unwrap(),
        "narrowing include/exclude is claim-only — the fingerprint must not move"
    );
    assert!(!handle2.shared().claims_key("docs", "docs/a.txt"));
    assert!(handle2.shared().claims_key("docs", "docs/keep/b.txt"));

    // (3) two full passes under the narrowed filter: pass 1 sweeps the
    // planted orphan (idle: nothing pending), pass 2 would flip any marker
    // that (incorrectly) collected a strike on pass 1
    rt2.scan_once().await;
    rt2.drain_queue().await;
    rt2.scan_once().await;
    rt2.drain_queue().await;

    // the canary is gone => the orphan sweep RAN in these passes
    assert_eq!(
        vec_count(&storage, "docs", &ix).await,
        a_chunks + b_chunks,
        "the planted orphan was swept (canary) while the narrowed-out source's \
         chunks survived: the sweep's live set is the FULL listing, not the claim"
    );
    // the narrowed-out marker is byte-untouched: still INDEXED, never
    // struck, never flipped, never rewritten
    let a2 = expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    assert_eq!(a2.misses, 0, "narrowing must not count strikes: {a2:?}");
    assert_eq!(
        a2.updated_at, a1.updated_at,
        "the marker must be untouched by the narrowed scans"
    );
    assert_eq!(a2.chunks, a1.chunks);
    assert_eq!(a2.content_hash, a1.content_hash);
    expect_status(&storage, "docs", "docs/keep/b.txt", SourceStatus::Indexed).await;

    // (4) the narrowed filter still takes NEW work in both directions:
    // keep/c.txt is claimed and indexed, docs/d.txt is not a source at all
    std::fs::write(
        docs.join("keep").join("c.txt"),
        "new file inside the narrowed claim",
    )
    .unwrap();
    std::fs::write(docs.join("d.txt"), "new file outside the narrowed claim").unwrap();
    rt2.scan_once().await;
    rt2.drain_queue().await;
    let c = expect_status(&storage, "docs", "docs/keep/c.txt", SourceStatus::Indexed).await;
    let c_chunks = c.chunks.as_ref().unwrap()["text"] as usize;
    assert!(
        marker_of(&storage, "docs", "docs/d.txt").await.is_none(),
        "an unclaimed new file must never be marked"
    );
    assert_eq!(
        vec_count(&storage, "docs", &ix).await,
        a_chunks + b_chunks + c_chunks
    );
}

/// The vanish-flip half of the same rule: a marker the CURRENT filter no
/// longer claims is outside the vanish-flip scope — even with the object
/// REALLY deleted and the strikes long past, the marker must not flip to
/// DELETING, while a still-claimed sibling object deleted in the same pass
/// goes through the full strike -> flip -> reap cycle (the control that
/// proves the machinery was live, so the difference is the claim filter).
#[tokio::test]
async fn narrowed_out_marker_never_vanish_flips_even_when_the_object_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::file(dir.path()));

    let wide = config_text(&ingest_section("docs", "include = [\"docs/**\"]\n"));
    let (rt, _h, state, cfg_wide) = runtime_over(storage.clone(), &wide).await;
    hold_locks(&state, &cfg_wide).await;
    let docs = dir.path().join("docs").join("docs");
    std::fs::create_dir_all(docs.join("keep")).unwrap();
    std::fs::write(
        docs.join("a.txt"),
        "will fall out of the claim and then be deleted",
    )
    .unwrap();
    std::fs::write(
        docs.join("keep").join("b.txt"),
        "stays claimed; deleted to drive the control reap",
    )
    .unwrap();
    // keeps the listing non-empty after both deletions (护栏② would
    // otherwise refuse the control reap and dull the contrast)
    std::fs::write(docs.join("keep").join("c.txt"), "stays on disk").unwrap();
    rt.scan_once().await;
    rt.drain_queue().await;
    for k in ["docs/a.txt", "docs/keep/b.txt", "docs/keep/c.txt"] {
        expect_status(&storage, "docs", k, SourceStatus::Indexed).await;
    }
    let a1 = marker_of(&storage, "docs", "docs/a.txt").await.unwrap();

    // narrowed config; delete BOTH the narrowed-out and the claimed object
    state.locks.release_all().await;
    let narrow = config_text(&ingest_section("docs", "include = [\"docs/keep/**\"]\n"));
    let (rt2, _h2, state2, cfg_narrow) = runtime_over(storage.clone(), &narrow).await;
    hold_locks(&state2, &cfg_narrow).await;
    std::fs::remove_file(docs.join("a.txt")).unwrap();
    std::fs::remove_file(docs.join("keep").join("b.txt")).unwrap();

    // three passes — one beyond the VANISH_STRIKES threshold
    for _ in 0..3 {
        rt2.scan_once().await;
        rt2.drain_queue().await;
    }

    // control: the CLAIMED deleted object went strike -> flip -> reap
    assert!(
        marker_of(&storage, "docs", "docs/keep/b.txt")
            .await
            .is_none(),
        "the claimed vanished object must be reaped (the machinery is live)"
    );
    // the G3 assertion: the narrowed-out marker never even struck
    let a2 = expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    assert_eq!(a2.misses, 0, "unclaimed markers take no strikes: {a2:?}");
    assert_eq!(a2.updated_at, a1.updated_at, "untouched by every pass");
}

// ---------------------------------------------------------------------------
// G3-2: 空 listing 故障注入 (R5 双护栏, §5.2 "GC 遇异常 listing")
// ---------------------------------------------------------------------------

/// A list that SUCCEEDS but returns EMPTY while markers exist is the
/// outage/permission shape (§5.2 last row). Injected on the file backend by
/// emptying the source bucket directory (the PR2-recorded file-backend
/// homology of a broken listing). Both guardrails must hold:
/// * 护栏① N-strike: the first empty pass counts strikes, flips nothing;
/// * 护栏② zero-live: even after the strikes flip markers to DELETING, reap
///   AND sweep refuse (live==0 && markers>0) — no chunk is ever deleted,
///   through any number of passes;
/// and the outage's end converges: the objects return, the markers
/// re-embed, the counts re-derive.
#[tokio::test]
async fn empty_listing_outage_never_mass_deletes_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::file(dir.path()));

    let text = config_text(&ingest_section("docs", "include = [\"docs/**\"]\n"));
    let (rt, handle, state, cfg) = runtime_over(storage.clone(), &text).await;
    hold_locks(&state, &cfg).await;
    let docs = dir.path().join("docs").join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), "first document caught by the outage").unwrap();
    std::fs::write(docs.join("b.txt"), "second document caught by the outage").unwrap();
    rt.scan_once().await;
    rt.drain_queue().await;
    let a = expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    let b = expect_status(&storage, "docs", "docs/b.txt", SourceStatus::Indexed).await;
    let ix = derived_index(&storage, "docs").await;
    let total = (a.chunks.as_ref().unwrap()["text"] + b.chunks.as_ref().unwrap()["text"]) as usize;
    assert!(total > 0);

    // ---- the outage: every object vanishes from the listing at once ----
    std::fs::remove_file(docs.join("a.txt")).unwrap();
    std::fs::remove_file(docs.join("b.txt")).unwrap();

    // pass 1: strikes counted, nothing flipped (护栏① — one absent scan is
    // not a vanish), nothing deleted (护栏② gates the same pass's sweep)
    rt.scan_once().await;
    rt.drain_queue().await;
    let a1 = expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    assert_eq!(a1.misses, 1, "one empty pass = exactly one strike");
    assert_eq!(vec_count(&storage, "docs", &ix).await, total);

    // pass 2: the N-strike threshold flips the markers DELETING (by design,
    // N=2) — but 护栏② refuses the reap: markers stay, chunks stay
    rt.scan_once().await;
    rt.drain_queue().await;
    expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Deleting).await;
    expect_status(&storage, "docs", "docs/b.txt", SourceStatus::Deleting).await;
    assert_eq!(
        vec_count(&storage, "docs", &ix).await,
        total,
        "an empty listing must never reap chunks (live==0 && markers>0)"
    );

    // pass 3: the refusal persists — a stuck outage must not wear the
    // guard down
    rt.scan_once().await;
    rt.drain_queue().await;
    expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Deleting).await;
    assert_eq!(vec_count(&storage, "docs", &ix).await, total);
    assert_eq!(
        handle.shared().stats_of("docs").deleted,
        0,
        "not one source may be counted deleted through the outage"
    );

    // ---- the outage ends: objects return, everything re-converges ----
    std::fs::write(
        docs.join("a.txt"),
        "first document restored after the outage",
    )
    .unwrap();
    std::fs::write(
        docs.join("b.txt"),
        "second document restored after the outage",
    )
    .unwrap();
    rt.scan_once().await;
    rt.drain_queue().await;
    let a2 = expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    let b2 = expect_status(&storage, "docs", "docs/b.txt", SourceStatus::Indexed).await;
    let healed =
        (a2.chunks.as_ref().unwrap()["text"] + b2.chunks.as_ref().unwrap()["text"]) as usize;
    assert_eq!(vec_count(&storage, "docs", &ix).await, healed);
}

// ---------------------------------------------------------------------------
// G3-3: memory 桶间隔离 (R1: one shared key space, per-bucket views)
// ---------------------------------------------------------------------------

/// Two vector buckets ingesting side by side on the MEMORY backend share
/// ONE `InMemory` map — the only thing keeping them apart is the bucket
/// prefix discipline. Assert it end-to-end: the SAME object key in both
/// buckets produces two independent markers (different content hashes, no
/// key-hash collision), each bucket's source/system key sets are exactly
/// its own, the raw shared map carries nothing outside the four bucket
/// prefixes, R9 enumeration lists both, and each derived index holds only
/// its own bucket's chunks.
#[tokio::test]
async fn memory_backend_two_buckets_are_fully_isolated() {
    let storage = Arc::new(Storage::memory());

    // ownership markers (R9 enumeration reads them)
    for vb in ["docs", "notes"] {
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

    let text = &format!(
        "{}{}",
        ingest_section("docs", ""),
        ingest_section("notes", "")
    );
    let (rt, _h, state, cfg) = runtime_over(storage.clone(), &config_text(text)).await;
    hold_locks(&state, &cfg).await;

    // the SAME key in both buckets with DIFFERENT bytes (the sharpest
    // collision probe), plus one docs-only key — all through the REAL
    // PutSource path (lazy source-bucket creation included)
    let put = |bucket: &str, name: &str, body: &'static str| {
        let state = state.clone();
        let mut hm = axum::http::HeaderMap::new();
        hm.insert(
            axum::http::HeaderName::from_static("x-vf-bucket"),
            bucket.parse().unwrap(),
        );
        hm.insert(
            axum::http::HeaderName::from_static("x-vf-file-name"),
            name.parse().unwrap(),
        );
        async move {
            let resp = vectorfs_server::ops_xvf::put_source(
                axum::extract::State(state.as_ref().clone()),
                hm,
                bytes::Bytes::from_static(body.as_bytes()),
            )
            .await
            .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
        }
    };
    let docs_body = "the docs copy of the shared key, with its own bytes";
    let notes_body = "the notes copy of the shared key, deliberately different";
    put("docs", "shared%2Fa.txt", docs_body).await;
    put(
        "docs",
        "docs%2Fonly.txt",
        "a document that only exists in docs",
    )
    .await;
    put("notes", "shared%2Fa.txt", notes_body).await;

    rt.scan_once().await;
    rt.drain_queue().await;

    // markers: exactly each bucket's own, all INDEXED, same key => two
    // independent markers with the content hashes of their OWN bytes
    let docs_markers = storage.list_sources("docs").await.unwrap();
    let notes_markers = storage.list_sources("notes").await.unwrap();
    assert_eq!(
        docs_markers
            .iter()
            .map(|m| m.object_key.as_str())
            .collect::<Vec<_>>(),
        vec!["docs/only.txt", "shared/a.txt"],
        "{docs_markers:?}"
    );
    assert_eq!(
        notes_markers
            .iter()
            .map(|m| m.object_key.as_str())
            .collect::<Vec<_>>(),
        vec!["shared/a.txt"],
        "{notes_markers:?}"
    );
    for m in docs_markers.iter().chain(notes_markers.iter()) {
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
    }
    let dm = marker_of(&storage, "docs", "shared/a.txt").await.unwrap();
    let nm = marker_of(&storage, "notes", "shared/a.txt").await.unwrap();
    assert_eq!(dm.content_hash, sha256_hex(docs_body.as_bytes()));
    assert_eq!(nm.content_hash, sha256_hex(notes_body.as_bytes()));
    assert_ne!(
        dm.content_hash, nm.content_hash,
        "the same object key in two buckets must never share a marker"
    );

    // source buckets: each store lists EXACTLY its own objects
    assert_eq!(
        all_keys(&storage, true, "docs").await,
        BTreeSet::from(["docs/only.txt".to_string(), "shared/a.txt".to_string()])
    );
    assert_eq!(
        all_keys(&storage, true, "notes").await,
        BTreeSet::from(["shared/a.txt".to_string()])
    );

    // system buckets: each carrying its own mechanism objects. The marker
    // of the SHARED key exists under BOTH -vf buckets as the SAME
    // bucket-relative name (`sources/<sameHash>.json`) — two independent
    // copies is itself the no-collision proof: one shared namespace would
    // have kept only one marker (and the content-hash asserts above would
    // have failed). Cross-leak check: notes carries NOTHING of docs-only.
    let dsys = all_keys(&storage, false, "docs").await;
    let nsys = all_keys(&storage, false, "notes").await;
    let shared_hash = key_hash("shared/a.txt");
    let docs_only_hash = key_hash("docs/only.txt");
    for (sys, vb) in [(&dsys, "docs-vf"), (&nsys, "notes-vf")] {
        assert!(sys.contains("bucket.json"), "{vb}: {sys:?}");
        assert!(sys.contains("LOCK"), "{vb}: {sys:?}");
        assert!(
            sys.contains(format!("sources/{shared_hash}.json").as_str()),
            "{vb}: {sys:?}"
        );
        assert!(
            sys.iter().any(|k| k.starts_with("indexes/")),
            "{vb}: {sys:?}"
        );
    }
    assert!(
        !nsys.contains(format!("sources/{docs_only_hash}.json").as_str()),
        "docs-only state leaked into notes' system bucket: {nsys:?}"
    );

    // the raw shared map: every key lives under exactly one of the four
    // bucket prefixes (R1) — nothing naked, nothing under the retired `vf/`
    // mirror prefix — and the shared key's marker exists as TWO distinct
    // physical keys (one per system-bucket prefix)
    let mem = storage.memory_handle().expect("memory backend handle");
    let raw: BTreeSet<String> = backends::list_all(mem.as_ref())
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.as_ref().to_string())
        .collect();
    assert!(!raw.is_empty());
    for k in &raw {
        let first = k.split('/').next().unwrap_or_default();
        assert!(
            ["docs", "docs-vf", "notes", "notes-vf"].contains(&first),
            "naked or mirrored key in the shared space: {k:?}"
        );
        assert!(
            !k.starts_with("vf/"),
            "the v2.2 mirror prefix is gone: {k:?}"
        );
    }
    for phys in [
        format!("docs-vf/sources/{shared_hash}.json"),
        format!("notes-vf/sources/{shared_hash}.json"),
    ] {
        assert!(raw.contains(&phys), "missing independent copy: {phys}");
    }

    // R9 enumeration (suffix scan over the shared key space): exactly both
    let identity = storage.deployment_identity("us-east-1", "000000000000");
    let listed = storage.list_buckets(&identity).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|m| m.vector_bucket_name.as_str())
            .collect::<Vec<_>>(),
        vec!["docs", "notes"]
    );

    // derived indexes: same embedder config => same derived index NAME in
    // both buckets — each bucket's dataset holds exactly its own chunks
    let dix = derived_index(&storage, "docs").await;
    let nix = derived_index(&storage, "notes").await;
    assert_eq!(
        dix, nix,
        "same config => same derived name; isolation is by bucket"
    );
    let dsum: u32 = storage
        .list_sources("docs")
        .await
        .unwrap()
        .iter()
        .map(|m| m.chunks.as_ref().unwrap()["text"])
        .sum();
    let nsum: u32 = storage
        .list_sources("notes")
        .await
        .unwrap()
        .iter()
        .map(|m| m.chunks.as_ref().unwrap()["text"])
        .sum();
    assert_eq!(vec_count(&storage, "docs", &dix).await, dsum as usize);
    assert_eq!(vec_count(&storage, "notes", &nix).await, nsum as usize);
    // and the rows' uri attribution stays inside each bucket's own hashes
    let duri_set: BTreeSet<String> = ["docs/only.txt", "shared/a.txt"]
        .iter()
        .map(|k| key_hash(k))
        .collect();
    for (k, meta) in storage.scan_vector_metadata("docs", &dix).await.unwrap() {
        let uri = meta
            .as_ref()
            .and_then(|m| m.get("x-vf-src-uri"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("no uri on {k}"));
        assert!(duri_set.contains(uri), "foreign uri {uri} in docs: {k}");
    }
    for (k, meta) in storage.scan_vector_metadata("notes", &nix).await.unwrap() {
        let uri = meta
            .as_ref()
            .and_then(|m| m.get("x-vf-src-uri"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("no uri on {k}"));
        assert_eq!(uri, &shared_hash, "foreign uri in notes: {k}");
    }
}

// ---------------------------------------------------------------------------
// G3-4: GC 换轴 — 作用域 = 本 daemon 持锁的桶集合 (R5/B-F8) + 整桶扫描
// ---------------------------------------------------------------------------

/// Two CONFIGURED buckets, one LOCK held: the unheld bucket gets no scan
/// decisions (its files are never marked) and no GC of any kind — a
/// hand-planted DELETING marker and an orphan-shaped chunk row in its
/// managed index survive every pass — while the held bucket's object
/// deleted in the same passes goes through the full strike -> flip -> reap
/// cycle. The contrast proves the scope (not inactivity) is what protects
/// the unheld bucket: a multi-daemon deployment's buckets are never touched
/// by someone else's GC (B-F8).
#[tokio::test]
async fn gc_scope_is_exactly_the_held_lock_set() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::file(dir.path()));

    let text = &format!(
        "{}{}",
        ingest_section("docs", ""),
        ingest_section("notes", "")
    );
    let (rt, _h, state, cfg) = runtime_over(storage.clone(), &config_text(text)).await;
    assert_eq!(cfg.ingest.len(), 2, "both buckets are configured");
    // serve's STARTUP shape for a lock we could not take (§5.1): pin+hold
    // docs only — notes is configured but its lock was never acquired
    state.locks.pin("docs");
    state.locks.ensure_held("docs").await.unwrap();
    assert_eq!(state.locks.held_buckets(), vec!["docs"]);

    let docs = dir.path().join("docs").join("docs");
    let notes = dir.path().join("notes").join("notes");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(docs.join("a.txt"), "the held bucket's doomed document").unwrap();
    std::fs::write(docs.join("keep.txt"), "keeps the held listing non-empty").unwrap();
    std::fs::write(notes.join("n.txt"), "the unheld bucket's file").unwrap();

    rt.scan_once().await;
    rt.drain_queue().await;
    // held bucket: fully ingested; unheld bucket: NOT EVEN MARKED
    expect_status(&storage, "docs", "docs/a.txt", SourceStatus::Indexed).await;
    expect_status(&storage, "docs", "docs/keep.txt", SourceStatus::Indexed).await;
    assert!(
        marker_of(&storage, "notes", "notes/n.txt").await.is_none(),
        "an unheld bucket gets no scan decisions (§5.1 startup skip)"
    );

    // plant GC-bait in the UNHELD bucket: a DELETING marker whose object is
    // gone, plus a managed index holding an orphan-shaped chunk row
    let ghost = "notes/ghost.txt";
    storage
        .save_source(
            "notes",
            &key_hash(ghost),
            &SourceMarker {
                object_key: ghost.to_string(),
                etag_sig: "size=1;mtime_ns=1".into(),
                content_hash: "c".repeat(64),
                pipeline_fingerprint: "fp".into(),
                status: SourceStatus::Deleting,
                status_reason: None,
                retries: 0,
                retries_exhausted: false,
                misses: 9,
                updated_at: "t0".into(),
                chunks: Some(BTreeMap::from([("text".to_string(), 1)])),
                engines: None,
            },
        )
        .await
        .unwrap();
    let ghost_ix = "notes-ghost-ix";
    storage
        .create_index(&IndexMeta {
            vector_bucket_name: "notes".into(),
            index_name: ghost_ix.into(),
            index_arn: "arn".into(),
            creation_time_rfc3339: "t".into(),
            creation_time_epoch: 0.0,
            data_type: "float32".into(),
            dimension: 8,
            distance_metric: "cosine".into(),
            non_filterable_metadata_keys: None,
            index_mode: None,
            x_vf_derived: Some(DerivedIndexInfo {
                key: "k".repeat(64),
                embedder: "tiny".into(),
                model: "tiny-fixture".into(),
                dim: 8,
            }),
        })
        .await
        .unwrap();
    let ghost_uri = key_hash(ghost);
    storage
        .upsert_vectors("notes", ghost_ix, 8, vec![orphan_row(&ghost_uri, 0)])
        .await
        .unwrap();

    // the held bucket's object vanishes; two passes take it through
    // strike -> flip -> reap (the control that these passes DO run GC)
    std::fs::remove_file(docs.join("a.txt")).unwrap();
    for _ in 0..2 {
        rt.scan_once().await;
        rt.drain_queue().await;
    }
    assert!(
        marker_of(&storage, "docs", "docs/a.txt").await.is_none(),
        "held bucket: flip + reap ran (the GC passes are live)"
    );

    // the unheld bucket: NOTHING was touched — the DELETING marker survives
    // unreaped, the orphan row survives unswept, the file stays unmarked
    let gm = marker_of(&storage, "notes", ghost).await;
    let gm = gm.unwrap_or_else(|| panic!("GC reaped a bucket whose lock we do not hold"));
    assert_eq!(gm.status, SourceStatus::Deleting);
    assert_eq!(gm.misses, 9, "not even a strike-counter rewrite: {gm:?}");
    assert_eq!(gm.updated_at, "t0");
    assert_eq!(
        storage
            .scan_vector_metadata("notes", ghost_ix)
            .await
            .unwrap()
            .len(),
        1,
        "the orphan sweep must not touch an unheld bucket"
    );
    assert!(marker_of(&storage, "notes", "notes/n.txt").await.is_none());
}

/// 整桶扫描 has NO exclusion list anymore (v3 R5): the system objects live
/// in the OTHER bucket by construction, so source-bucket keys shaped like
/// system keys (`sources/…`, `indexes/…`, even a `LOCK.txt` at the bucket
/// root) are ordinary user files — claimed, indexed, and never confused
/// with the system bucket's mechanism objects.
#[tokio::test]
async fn whole_bucket_scan_treats_system_shaped_keys_as_user_files() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::file(dir.path()));

    // whole-bucket claim (no include/exclude — 省缺 = 整桶)
    let (rt, _h, state, cfg) =
        runtime_over(storage.clone(), &config_text(&ingest_section("docs", ""))).await;
    hold_locks(&state, &cfg).await;

    let bucket = dir.path().join("docs");
    for (rel, body) in [
        (
            "sources/keep.txt",
            "a user file under a system-shaped prefix",
        ),
        (
            "indexes/ix/data/keep.txt",
            "a user file under the lance-shaped prefix",
        ),
        ("LOCK.txt", "a user file named like the LOCK object"),
        ("plain.txt", "an ordinary root-level file"),
    ] {
        let p = bucket.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    rt.scan_once().await;
    rt.drain_queue().await;

    for key in [
        "sources/keep.txt",
        "indexes/ix/data/keep.txt",
        "LOCK.txt",
        "plain.txt",
    ] {
        expect_status(&storage, "docs", key, SourceStatus::Indexed).await;
    }
    let markers = storage.list_sources("docs").await.unwrap();
    assert_eq!(markers.len(), 4, "{markers:?}");
    let ix = derived_index(&storage, "docs").await;
    let sum: u32 = markers
        .iter()
        .map(|m| m.chunks.as_ref().unwrap()["text"])
        .sum();
    assert_eq!(vec_count(&storage, "docs", &ix).await, sum as usize);

    // the source bucket holds EXACTLY the four user files — and the system
    // bucket's mechanism keys stay where they belong (the other bucket)
    assert_eq!(
        all_keys(&storage, true, "docs").await,
        BTreeSet::from([
            "LOCK.txt".to_string(),
            "indexes/ix/data/keep.txt".to_string(),
            "plain.txt".to_string(),
            "sources/keep.txt".to_string(),
        ])
    );
    let sys = all_keys(&storage, false, "docs").await;
    assert!(sys.contains("LOCK"), "{sys:?}");
    assert!(
        sys.contains("bucket.json"),
        "the marker json lives in the SYSTEM bucket, whatever the source holds"
    );
}
