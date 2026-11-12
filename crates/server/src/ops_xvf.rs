//! `/x-vf/*` — the file-layer SUPERSET operations (M2 T6, design.md 附录E:
//! the single authoritative list; §5.8/§5.10.6 only carry pointers).
//!
//! Eight operations under an isolated `POST /x-vf/<Op>` namespace so the AWS
//! face (21 ops, 附录A) — and any operation AWS ever adds — can never
//! collide. Everything else is deliberately ISOMORPHIC to the AWS face:
//! same SigV4 gate (signingName `s3vectors`, the `gate_middleware` in lib.rs
//! applies unchanged), same rest-json error protocol (double emission of
//! `X-Amzn-Errortype` + body `__type`, reused [`crate::ops::ApiError`]), same
//! opaque base64 nextToken pagination. The one deliberate wire difference:
//! **asynchronous operations answer HTTP 202 and are observed by polling**
//! (appendix E.1, aligned with the Bedrock KB `IngestKnowledgeBaseDocuments`
//! contract): PutSource / DeleteSource / Reindex.
//!
//! Domain rule shared by every bucket-taking op here: the bucket must be an
//! INGEST-CONFIGURED bucket (`[ingest.<bucket>]` in vectorfs.toml, present as
//! [`crate::ops::AppState::ingest`]) — the `/x-vf` face manages the file
//! layer, and an unconfigured bucket gets a 400 ValidationException whose
//! message points at the config, not a bare 404 (the operator's fix is a
//! config edit, not a resource lookup).
//!
//! Field-level decisions recorded per appendix E.2 (rationale at each site):
//! * PutSource bucket transport — `x-vf-bucket` HEADER (see `put_source`).
//! * GetSource on a markerless-but-present object — `{"status":"PENDING"}`
//!   (see `get_source`).
//! * M2 buffering: request and response bodies are buffered whole (max
//!   210 MiB on PutSource/GetSourceData, 21 MiB elsewhere); streaming lands
//!   with the M3 envelope work.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use crate::ops::{ApiError, AppState};
use crate::pipeline::{
    Embedder as _, HitList, Merge, Modality, Query, Reranker as _, merge_hitlists,
    merge_hitlists_weighted,
};
use crate::store::{QueryRoute, SourceMarker, SourceStatus};

/// Hard body cap for `/x-vf/PutSource` (and the gate's read of a
/// `/x-vf/GetSourceData` response): `max_file_size_mb` defaults to 200 MiB
/// with config headroom, so the wire accepts 210 MiB before the ingest-side
/// poison guard makes its own decision. Tradeoff (M2): the whole body is
/// buffered in memory — a single in-flight 200 MiB upload is the accepted
/// cost of the single-binary first cut; the multipart write path keeps the
/// OBJECT STORE traffic chunked either way.
pub const X_VF_MAX_BODY: usize = 210 * 1024 * 1024;

/// Bodies above this ride the object-store multipart API (appendix E.1:
/// ">100MB 走分段上传" — 16 MiB is the S3-standard part-5×-threshold that
/// keeps every backend happy without tuning).
const MULTIPART_THRESHOLD: usize = 16 * 1024 * 1024;

/// Fixed part size for the multipart path (S3 minimum 5 MiB except the last
/// part; 8 MiB keeps part counts low for the 210 MiB cap).
const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// request shapes (camelCase per appendix E.1's naming convention)
// ---------------------------------------------------------------------------

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::validation(format!("invalid request body: {e}")))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketKeyInput {
    bucket: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListSourcesInput {
    bucket: String,
    status: Option<String>,
    max_results: Option<i64>,
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQueryInput {
    /// Text query (M2). Exactly one of `text` / `image` must be present —
    /// a query is ONE modality (audio arrives in M4 and is a
    /// ValidationException until then, appendix E.2).
    text: Option<String>,
    /// Image query (M3-T4, 以图搜图): `{bytes, mime}` — see [`SearchImageInput`].
    image: Option<SearchImageInput>,
    audio: Option<serde_json::Value>,
}

/// The image member of a Search query (M3-T4). Field shape, appendix E.2
/// leaves the byte transport open — decided as an OBJECT carrying both the
/// base64 bytes and the declared mime, because:
///
/// * JSON has no native bytes; base64 is this op's existing binary-in-string
///   convention (the opaque `nextToken` cursor is base64 of the same engine);
/// * the DECLARED mime rides along (the CLI derives it from the file
///   extension) so the server can run the same declared-vs-sniffed-magic
///   honesty check [`crate::pipeline::ImagePayload::validate`] imposes on
///   the ingest side — a mismatched label is a client bug that must fail
///   HERE, with a field-addressable path, not inside an embedder's
///   preprocessing.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchImageInput {
    /// base64 (STANDARD alphabet) container bytes of the query image.
    bytes: String,
    /// Declared container mime — must be the bytes' CANONICAL container
    /// mime (`image/jpeg` / `image/png`).
    mime: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchInput {
    bucket: String,
    query: Option<SearchQueryInput>,
    top_k: Option<i64>,
    filter: Option<serde_json::Value>,
    return_text: Option<bool>,
    /// Retrieval mode (M3-T9, ADR 0002): `semantic` (default — the
    /// pre-T9 behavior, old clients unchanged), `keyword` (pure BM25 via
    /// the lexical kernel), `hybrid` (semantic fan-out + BM25 fused by the
    /// bucket's merge strategy). Unknown values are ValidationExceptions.
    search_mode: Option<String>,
    /// Cross-encoder rerank (M3-T11): re-score the merged candidate set
    /// (already truncated to topK) with the bucket's configured reranker
    /// and return hits in rerank order. Requires the bucket's
    /// `[ingest.<bucket>] rerank = true` (availability: the model loads at
    /// serve startup), a TEXT query (the cross-encoder reads the pair) and
    /// a non-keyword mode. Default absent/false = the pre-T11 behavior.
    rerank: Option<bool>,
    /// Rerank candidate DEPTH (R3a, decoupled from the return depth): the
    /// cross-encoder re-scores the first `rerankDepth` merged candidates and
    /// the response still returns `topK`. Absent = topK (the pre-R3a
    /// behavior, old clients zero-change). 1..=200, and >= topK (rerank
    /// must re-score at least what would be returned); a depth without
    /// `rerank: true` is a dead knob and refused. CPU honesty: the
    /// cross-encoder scores ~148 ms/pair on CPU (research/2026-11-m3-bench
    /// §2) — depth 20 ≈ 3 s, depth 50 ≈ 7.4 s; the latency is linear in
    /// the depth.
    rerank_depth: Option<i64>,
    /// Per-source quota (R4, grouping): after the merge order settles and
    /// BEFORE any truncation, at most `perSource` hits per source file
    /// (`x-vf-src-uri`) survive — walked in fusion-rank order, bucket-full
    /// entries skipped (保序). 1..=100; absent/0 = off (the pre-R4
    /// behavior). If the quota leaves fewer than topK hits, fewer are
    /// returned — skipped same-source hits are NOT backfilled (backfilling
    /// would defeat the quota). Applies before a rerankDepth truncation
    /// too: the cross-encoder sees the quota'd candidate set (a same-source
    /// crowd must not eat the re-scoring budget).
    per_source: Option<i64>,
    /// Structure kinds to ADMIT beyond the default visibility (T12, D-5):
    /// Search excludes `toc` and `legend` chunks by default (F2/F4
    /// governance — TOC lines are perfect BM25 matches and霸榜; legends
    /// amplify pathologically short docs); listing a kind here放行s it
    /// (e.g. `includeKinds: ["toc"]` for the navigation query「哪章在哪页」).
    /// Values are validated against the closed `x-vf-chunk-kind`
    /// vocabulary (`body|toc|caption|legend`) — unknown kinds are
    /// ValidationExceptions, never silent no-matches.
    include_kinds: Option<Vec<String>>,
    /// Terminal view ordering (T18, R8 structure axis, D-10.4):
    /// `section-asc` / `section-desc` re-order the FINAL hit set by
    /// (section, line-start, score desc, key asc), missing keys
    /// nulls-last — applied AFTER quota/truncation/rerank (presentation
    /// only, membership untouched; the `score` column keeps its meaning).
    /// Absent = the retrieval order (the pre-T18 behavior). Unknown
    /// values are ValidationExceptions at `path=orderBy` (the searchMode
    /// precedent).
    order_by: Option<String>,
}

/// The parsed `searchMode` (see [`SearchInput::search_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Semantic,
    Keyword,
    Hybrid,
}

/// The parsed `orderBy` (T18, D-10.4): the section axis's two directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderBy {
    SectionAsc,
    SectionDesc,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetIngestStatusInput {
    bucket: Option<String>,
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// The ingest handle for a bucket-taking `/x-vf` op: present + configured, or
/// a 400 ValidationException whose message IS the fix (appendix E.2 guidance
/// rule — the file layer is config-scoped, there is nothing to 404 into).
fn require_ingest<'a>(
    state: &'a AppState,
    bucket: &str,
) -> Result<&'a Arc<crate::ingest::IngestHandle>, ApiError> {
    let Some(handle) = state.ingest.as_ref() else {
        return Err(ApiError::validation(format!(
            "bucket {bucket:?}: this server runs the pure protocol face (no [ingest.*] sections \
             in vectorfs.toml) — /x-vf/* operations manage the file layer. Add an \
             [ingest.{bucket}] section (extract / chunk / embedders) to \
             <data-dir>/.vectorfs/vectorfs.toml and restart `vectorfs serve`, or use the \
             S3 Vectors API (PutVectors/QueryVectors) instead"
        )));
    };
    if !handle.shared().has_bucket(bucket) {
        return Err(ApiError::validation(format!(
            "bucket {bucket:?} has no [ingest.{bucket}] section in vectorfs.toml — /x-vf/* \
             operations only manage ingest-configured buckets. Add the section and restart \
             `vectorfs serve`"
        )));
    }
    Ok(handle)
}

/// object_store error -> ApiError (NotFound mapped by the callers that expect
/// it; everything else is a 500 — the wire never leaks engine error text
/// shapes it cannot parse).
fn store_err(e: object_store::Error) -> ApiError {
    ApiError::internal(format!("object store: {e}"))
}

/// Bucket-relative key of a USER object (design-v3.md R1/R5: the SOURCE
/// bucket IS the vb's namespace — keys address the bucket's store directly).
fn user_key(_state: &AppState, key: &str) -> object_store::path::Path {
    object_store::path::Path::from(key)
}

/// The bucket's SOURCE store (where PutSource writes / GetSourceData reads).
fn source_store(state: &AppState, bucket: &str) -> Arc<dyn object_store::ObjectStore> {
    state.storage.source_store(bucket)
}

/// Marker-file key hash of an instance-root object key.
fn key_hash(key: &str) -> String {
    crate::ingest::marker::object_key_hash(key)
}

/// HEAD a user object: Ok(None) = gone. This is the LIVE existence check
/// GetSource/ListSources join against (markers can lag the listing). Tries
/// the canonical key form first, then the raw form (externally placed
/// files — see `domain::user_object_key_candidates`).
async fn head_user_object(
    state: &AppState,
    bucket: &str,
    key: &str,
) -> Result<Option<object_store::ObjectMeta>, ApiError> {
    use object_store::ObjectStoreExt as _;
    for okey in crate::ingest::domain::user_object_key_candidates(key) {
        match source_store(state, bucket).head(&okey).await {
            Ok(meta) => return Ok(Some(meta)),
            Err(object_store::Error::NotFound { .. }) => continue,
            Err(e) => return Err(store_err(e)),
        }
    }
    Ok(None)
}

/// Write one user object into the bucket's SOURCE store (PutSource, R3 lazy
/// bucket creation handled by the caller): plain put below the multipart
/// threshold, WriteMultipart above it. Whole-body in memory either way
/// (M2 tradeoff, see [`X_VF_MAX_BODY`]).
async fn write_user_object(
    state: &AppState,
    bucket: &str,
    key: &str,
    body: Bytes,
) -> Result<(), ApiError> {
    use object_store::{ObjectStoreExt as _, PutPayload};
    let okey = user_key(state, key);
    let catalog = source_store(state, bucket);
    if body.len() > MULTIPART_THRESHOLD {
        let upload = catalog.put_multipart(&okey).await.map_err(store_err)?;
        let mut writer =
            object_store::WriteMultipart::new_with_chunk_size(upload, MULTIPART_PART_SIZE);
        writer.write(&body);
        writer.finish().await.map_err(store_err)?;
    } else {
        catalog
            .put(&okey, PutPayload::from(body))
            .await
            .map_err(store_err)?;
    }
    Ok(())
}

/// Read one user object whole (GetSourceData). Same candidate-key rule as
/// [`head_user_object`].
async fn read_user_object(
    state: &AppState,
    bucket: &str,
    key: &str,
) -> Result<Option<Bytes>, ApiError> {
    use object_store::ObjectStoreExt as _;
    for okey in crate::ingest::domain::user_object_key_candidates(key) {
        match source_store(state, bucket).get(&okey).await {
            Ok(get) => return Ok(Some(get.bytes().await.map_err(store_err)?)),
            Err(object_store::Error::NotFound { .. }) => continue,
            Err(e) => return Err(store_err(e)),
        }
    }
    Ok(None)
}

/// Delete one user object; a missing object is SUCCESS (S3 delete semantics —
/// the caller decides what "already gone" means). Deletes under BOTH key
/// forms (an external raw-name twin must not survive a canonical-form
/// delete).
async fn delete_user_object(state: &AppState, bucket: &str, key: &str) -> Result<(), ApiError> {
    use object_store::ObjectStoreExt as _;
    for okey in crate::ingest::domain::user_object_key_candidates(key) {
        match source_store(state, bucket).delete(&okey).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => continue,
            Err(e) => return Err(store_err(e)),
        }
    }
    Ok(())
}

/// etag wire value of a live object: the backend's real etag when it lists
/// one (S3), else the cheap pseudo-signature the scanner diffs by
/// (`size=…;mtime_ns=…`) — same honesty level as the marker's `etag_sig`.
fn etag_of(meta: &object_store::ObjectMeta) -> String {
    meta.e_tag
        .clone()
        .unwrap_or_else(|| crate::ingest::marker::signature_of(meta))
}

/// `chunks[{index,count}]` (appendix E.2): the marker stores per-modality
/// COUNTS; chunk ordinals are assigned sequentially across a source's parts,
/// so entry i's `index` is the running offset of modality i's chunk range
/// (text-only M2: a single `{index:0,count:N}`).
fn chunks_json(chunks: &Option<BTreeMap<String, u32>>) -> Option<Vec<serde_json::Value>> {
    let map = chunks.as_ref()?;
    let mut out = Vec::with_capacity(map.len());
    let mut offset = 0u32;
    for count in map.values() {
        out.push(json!({ "index": offset, "count": count }));
        offset += count;
    }
    if out.is_empty() { None } else { Some(out) }
}

/// One source-status JSON row (GetSource response body / ListSources entry
/// core). `meta` is the LIVE object when it exists (mime/etag); absent meta
/// leaves those members out (a DELETING row, or a scan-not-yet window).
fn source_status_json(
    key: &str,
    m: &SourceMarker,
    meta: Option<&object_store::ObjectMeta>,
) -> serde_json::Value {
    let mut o = json!({
        "status": m.status.as_str(),
        "pipelineFingerprint": m.pipeline_fingerprint,
        "retriesExhausted": m.retries_exhausted,
        "updatedAt": m.updated_at,
    });
    if let Some(reason) = &m.status_reason {
        o["statusReason"] = json!(reason);
    }
    if let Some(meta) = meta {
        o["mime"] = json!(crate::ingest::mime_for(key));
        o["etag"] = json!(etag_of(meta));
        o["size"] = json!(meta.size);
    }
    if let Some(chunks) = chunks_json(&m.chunks) {
        o["chunks"] = json!(chunks);
    }
    // T12 audit: the extraction's furniture tally (Some(0) = extracted,
    // removed nothing; absent = no completing extraction / pre-T12 marker)
    if let Some(n) = m.furniture_removed {
        o["furnitureRemoved"] = json!(n);
    }
    o
}

/// A fresh PENDING marker (the shape the scanner's `ScanPlan::New` writes;
/// PutSource pre-seeds it so `vf put` status is observable even before the
/// first scan ticks).
fn pending_marker(key: &str, sig: &str, fingerprint: &str) -> SourceMarker {
    SourceMarker {
        object_key: key.to_string(),
        etag_sig: sig.to_string(),
        content_hash: String::new(),
        pipeline_fingerprint: fingerprint.to_string(),
        status: SourceStatus::Pending,
        status_reason: None,
        retries: 0,
        retries_exhausted: false,
        misses: 0,
        updated_at: crate::ingest::marker::now_rfc3339(),
        chunks: None,
        engines: None,
        furniture_removed: None,
    }
}

// ---------------------------------------------------------------------------
// pagination (same opaque-cursor contract as the AWS face)
// ---------------------------------------------------------------------------

fn encode_token(last_key: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(json!({ "k": last_key }).to_string())
}

fn decode_token(token: &str) -> Result<String, ApiError> {
    let bad = || ApiError::validation_field("nextToken", "invalid nextToken");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(token)
        .map_err(|_| bad())?;
    let v: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| bad())?;
    v.get("k")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(bad)
}

// ---------------------------------------------------------------------------
// PutSource
// ---------------------------------------------------------------------------

/// `POST /x-vf/PutSource` (appendix E.2): convenience wrapper over "S3 PutObject
/// into the instance root + immediate ingest trigger". Body = RAW BYTES
/// (`Content-Type: application/octet-stream`); metadata rides headers:
/// `x-vf-file-name` (required, percent-encoded when non-ASCII — HTTP header
/// values are ASCII-only) and `x-vf-content-type` (optional, advisory for
/// M2: extractor routing is extension-based, so the canonical mime stays the
/// extension's).
///
/// BUCKET TRANSPORT (decision, appendix E leaves it open): the `x-vf-bucket`
/// HEADER, not a query parameter. Rationale: query strings enter the SigV4
/// canonical request as a second signed surface every hand-rolled client must
/// get byte-exact, while headers already flow through the signer's
/// SignedHeaders path this codebase exercises everywhere else; it also keeps
/// the op free of the `?bucket=` encoding questions multi-byte bucket names
/// would raise on the query route.
///
/// 202 `{key, status:"PENDING"}` — asynchronous contract: the file layer's
/// marker/scan machinery (not this handler) does the extraction/embedding;
/// poll GetSource. A PENDING marker is pre-seeded here (idempotent: an
/// existing marker is left alone — the scan's own diff decides what a
/// re-put means) so `vf put`'s foreground wait has something to observe
/// immediately.
pub async fn put_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    use object_store::ObjectStoreExt as _;

    // One x-vf-* string header: present, ASCII, percent-decoded (the client
    // contract encodes non-ASCII values — HTTP header values are ASCII-only).
    fn header_str(headers: &HeaderMap, name: &'static str) -> Result<String, ApiError> {
        let raw = headers
            .get(name)
            .ok_or_else(|| ApiError::validation_field(name, format!("{name} header is required")))?
            .to_str()
            .map_err(|_| {
                ApiError::validation_field(
                    name,
                    format!("{name} must be ASCII (percent-encode non-ASCII values)"),
                )
            })?;
        percent_encoding::percent_decode_str(raw)
            .decode_utf8()
            .map(|c| c.into_owned())
            .map_err(|_| {
                ApiError::validation_field(
                    name,
                    format!("{name} is not valid UTF-8 after percent-decoding"),
                )
            })
    }

    let bucket = header_str(&headers, "x-vf-bucket")?;
    let key = header_str(&headers, "x-vf-file-name")?;
    if let Some(ct) = headers.get("x-vf-content-type")
        && let Err(e) = ct.to_str()
    {
        return Err(ApiError::validation_field(
            "x-vf-content-type",
            format!("x-vf-content-type must be ASCII: {e}"),
        ));
    }
    if key.is_empty() || key.len() > 1024 {
        return Err(ApiError::validation_field(
            "x-vf-file-name",
            "key must be 1-1024 characters",
        ));
    }

    let handle = require_ingest(&state, &bucket)?;
    // the key must be a SOURCE this pipeline claims (include/exclude claim
    // filter + extractor routing, R5): otherwise the marker below would sit
    // forever un-scanned.
    if !handle.shared().claims_key(&bucket, &key) {
        return Err(ApiError::validation(format!(
            "key {key:?} is outside bucket {bucket:?}'s ingest claim filter or has no routed \
             extractor — the file layer would never ingest it. Check the section's \
             `include`/`exclude` and `extract` routing in vectorfs.toml"
        )));
    }

    // writer lock (lazy), then the R3 lazy SOURCE-bucket creation: the first
    // PutSource materializes bucket `X` when it does not exist yet (adoption
    // on a lost create race is benign, §5.2).
    if let Err(e) = state.locks.ensure_held(&bucket).await {
        return Err(match e {
            crate::lock::LockError::Held(who) => ApiError::conflict(format!(
                "vector bucket {bucket} is locked by another writer ({who})"
            )),
            crate::lock::LockError::Backend(e) => ApiError::internal(e),
        });
    }
    state.storage.ensure_source_bucket(&bucket).await?;
    write_user_object(&state, &bucket, &key, body).await?;
    let meta = source_store(&state, &bucket)
        .head(&user_key(&state, &key))
        .await
        .map_err(store_err)?;
    let sig = crate::ingest::marker::signature_of(&meta);

    // pre-seed the marker ONLY when absent (first PutSource of this key):
    // an existing marker's history (completion evidence, retries) belongs to
    // the scanner's diff, not to this write.
    let hash = key_hash(&key);
    if state.storage.load_source(&bucket, &hash).await?.is_none() {
        let fingerprint = handle
            .shared()
            .fingerprint_of(&bucket)
            .expect("configured bucket");
        state
            .storage
            .save_source(&bucket, &hash, &pending_marker(&key, &sig, &fingerprint))
            .await?;
    }

    handle.wake(); // immediate scan trigger (coalesced by the scheduler)
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "key": key, "status": "PENDING" })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GetSource / GetSourceData
// ---------------------------------------------------------------------------

/// `POST /x-vf/GetSource` (appendix E.2) -> `{status, statusReason?,
/// retriesExhausted?, mime, etag, pipelineFingerprint, chunks[{index,count}]}`.
///
/// NO-MARKER SEMANTICS (decision, appendix E leaves it open): an object that
/// EXISTS live but has no marker is a legal transient (the scan has not
/// reached it — e.g. right after PutSource's 202 or an S3-direct PutObject),
/// and answers `{"status":"PENDING", …}` with the CURRENT pipeline
/// fingerprint and live mime/etag. 404 NotFoundException is reserved for
/// "the object does not exist", with one carve-out: a DELETING marker with a
/// gone object still answers 200 DELETING — that is exactly the state the
/// DeleteSource 202 told the caller to poll (GC/T7 reaps the marker later).
pub async fn get_source(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: BucketKeyInput = parse_body(&body)?;
    let handle = require_ingest(&state, &input.bucket)?;
    let hash = key_hash(&input.key);
    let marker = state.storage.load_source(&input.bucket, &hash).await?;
    match head_user_object(&state, &input.bucket, &input.key).await? {
        Some(meta) => {
            let m = marker.unwrap_or_else(|| {
                // live but unmarked: report the pending transient under the
                // CURRENT fingerprint (what the next scan will ingest it under)
                let fp = handle
                    .shared()
                    .fingerprint_of(&input.bucket)
                    .expect("configured bucket");
                pending_marker(&input.key, &crate::ingest::marker::signature_of(&meta), &fp)
            });
            Ok(Json(source_status_json(&input.key, &m, Some(&meta))).into_response())
        }
        None => match marker {
            Some(m) if m.status == SourceStatus::Deleting => {
                Ok(Json(source_status_json(&input.key, &m, None)).into_response())
            }
            Some(_) | None => Err(ApiError::not_found(format!(
                "source {:?} does not exist in bucket {:?}",
                input.key, input.bucket
            ))),
        },
    }
}

/// `POST /x-vf/GetSourceData` (appendix E.2): transparent byte passthrough of
/// the instance-root object (`vf get`'s backing op; equivalent to a direct
/// GetObject on the underlying bucket). 200 + bytes + the canonical mime for
/// the key's extension; 404 when the object is gone.
pub async fn get_source_data(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: BucketKeyInput = parse_body(&body)?;
    require_ingest(&state, &input.bucket)?;
    match read_user_object(&state, &input.bucket, &input.key).await? {
        Some(bytes) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, crate::ingest::mime_for(&input.key))
            .body(Body::from(bytes))
            .expect("static response")),
        None => Err(ApiError::not_found(format!(
            "source {:?} does not exist in bucket {:?}",
            input.key, input.bucket
        ))),
    }
}

// ---------------------------------------------------------------------------
// ListSources
// ---------------------------------------------------------------------------

/// `POST /x-vf/ListSources` (appendix E.2): markers x LIVE listing join.
/// The MARKER set is primary (it is the file layer's bookkeeping — objects
/// the pipeline does not claim never appear); the join fills live
/// etag/mime/size. DELETING markers are listed (the DeleteSource poll
/// contract). Pagination = opaque base64 keyset cursor by `key`
/// (`{"k":<last>}`), maxResults 1-1000 default 500 — the same shape as the
/// AWS face's name-keyed pages.
pub async fn list_sources(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: ListSourcesInput = parse_body(&body)?;
    require_ingest(&state, &input.bucket)?;

    let max = match input.max_results {
        None => 500,
        Some(n) if (1..=1000).contains(&n) => n as usize,
        Some(n) => {
            return Err(ApiError::validation_field(
                "maxResults",
                format!("maxResults must be between 1 and 1000, got {n}"),
            ));
        }
    };
    let after = match input.next_token.as_deref() {
        None | Some("") => None,
        Some(t) => Some(decode_token(t)?),
    };
    let status_filter = match input.status.as_deref() {
        None | Some("") => None,
        Some(s) => match parse_status(s) {
            Some(_) => Some(s.to_string()),
            None => {
                return Err(ApiError::validation_field(
                    "status",
                    format!(
                        "status must be one of PENDING, IN_PROGRESS, PARTIALLY_INDEXED, INDEXED, \
                         FAILED, DELETING, got {s:?}"
                    ),
                ));
            }
        },
    };

    // markers (object_key-sorted by the store) x live listing (one pass,
    // over THIS bucket's source store — v3 buckets are separate namespaces)
    let markers = state.storage.list_sources(&input.bucket).await?;
    let listing: HashMap<String, object_store::ObjectMeta> =
        crate::ingest::list_source_bucket(&state.storage, &input.bucket)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .collect();

    let mut sources = Vec::new();
    let mut last_emitted: Option<String> = None;
    let mut token = None;
    for m in &markers {
        if let Some(want) = &status_filter
            && m.status.as_str() != want.as_str()
        {
            continue;
        }
        if let Some(a) = &after
            && m.object_key <= *a
        {
            continue;
        }
        if sources.len() == max {
            // one item left over -> a next page exists (lookahead semantics)
            token = last_emitted.clone();
            break;
        }
        let mut entry = source_status_json(&m.object_key, m, listing.get(&m.object_key));
        entry["key"] = json!(m.object_key);
        sources.push(entry);
        last_emitted = Some(m.object_key.clone());
    }

    let mut resp = json!({ "sources": sources });
    if let Some(t) = token {
        resp["nextToken"] = json!(encode_token(&t));
    }
    Ok(Json(resp).into_response())
}

fn parse_status(s: &str) -> Option<SourceStatus> {
    match s {
        "PENDING" => Some(SourceStatus::Pending),
        "IN_PROGRESS" => Some(SourceStatus::InProgress),
        "PARTIALLY_INDEXED" => Some(SourceStatus::PartiallyIndexed),
        "INDEXED" => Some(SourceStatus::Indexed),
        "FAILED" => Some(SourceStatus::Failed),
        "DELETING" => Some(SourceStatus::Deleting),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// DeleteSource
// ---------------------------------------------------------------------------

/// `POST /x-vf/DeleteSource` (appendix E.2) -> 202 `{status:"DELETING"}`.
/// = delete the object (object_store) + flip the marker to DELETING + wake.
/// The chunk GC itself is T7's reaper; this op only establishes the state it
/// consumes. Idempotent exactly like S3 delete: an already-gone object still
/// answers 202 (with the marker flipped for good measure).
pub async fn delete_source(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: BucketKeyInput = parse_body(&body)?;
    let handle = require_ingest(&state, &input.bucket)?;
    if let Err(e) = state.locks.ensure_held(&input.bucket).await {
        return Err(match e {
            crate::lock::LockError::Held(who) => ApiError::conflict(format!(
                "vector bucket {} is locked by another writer ({who})",
                input.bucket
            )),
            crate::lock::LockError::Backend(e) => ApiError::internal(e),
        });
    }
    delete_user_object(&state, &input.bucket, &input.key).await?;
    let hash = key_hash(&input.key);
    if let Some(m) = state.storage.load_source(&input.bucket, &hash).await?
        && let Some(mut d) =
            crate::ingest::marker::plan_vanished(m, &crate::ingest::marker::now_rfc3339())
    {
        // clear the completion evidence: GC'd chunks must never be
        // mistaken for a re-put skip (same rule as the scanner's
        // object-reappears path)
        d.content_hash.clear();
        d.chunks = None;
        state.storage.save_source(&input.bucket, &hash, &d).await?;
    }
    handle.wake(); // the woken scan reaps the DELETING marker in the SAME pass (T7 GC)
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "DELETING" }))).into_response())
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Decode + honesty-check one image query member (M3-T4): base64 → bytes →
/// sniffed container magic, all fail-fast with field-addressable
/// ValidationExceptions. Returns `(bytes, mime)` for [`Query::Image`] — the
/// mime is the sniffed CANONICAL one (the declared label was only consulted
/// to be cross-checked, the exact [`crate::pipeline::ImagePayload::validate`]
/// discipline the ingest-side chunker bypass runs).
fn decode_image_query(img: &SearchImageInput) -> Result<(Vec<u8>, String), ApiError> {
    let bad = |msg: String| ApiError::validation_field("query.image", msg);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(img.bytes.as_bytes())
        .map_err(|e| bad(format!("bytes is not valid base64: {e}")))?;
    if bytes.is_empty() {
        return Err(bad(
            "bytes is empty — an image query needs the image's container bytes \
                        (jpeg/png)"
                .to_string(),
        ));
    }
    let sniffed = image::guess_format(&bytes)
        .map_err(|_| {
            bad(
                "the bytes carry no known image-container magic (jpeg/png) — not an image"
                    .to_string(),
            )
        })?
        .to_mime_type()
        .to_string();
    if sniffed != img.mime {
        return Err(bad(format!(
            "mime mismatch: the bytes sniff as {sniffed} but the query declares '{}' — use \
             the container's canonical mime (the declared mime drives embedder preprocessing)",
            img.mime
        )));
    }
    Ok((bytes, sniffed))
}

/// The kinds Search excludes unless [`SearchInput::include_kinds`] admits
/// them (T12, D-5): TOC lines are page-repeated perfect lexical matches
/// (F2's霸榜) and legend blocks are pathologically short BM25 docs — both
/// stay INDEXED and retrievable, just out of the DEFAULT result set (the
/// superset parameter放行s them; ranking policy, never chunk identity, so
/// this list lives on the query face and touches no fingerprint).
const SEARCH_DEFAULT_EXCLUDED_KINDS: [&str; 2] = ["toc", "legend"];

// ---------------------------------------------------------------------------
// src-name translation (T15, ADR 0004)
// ---------------------------------------------------------------------------

/// The superset face's source-NAME filter key (T15, ADR 0004): conditions
/// on it are rewritten into [`SRC_URI_KEY`] hashes BEFORE filter compile.
/// The key stays non-filterable everywhere else — the protocol face's
/// QueryVectors 400 rejection is untouched, the translation lives ONLY on
/// `/x-vf/Search`.
const SRC_NAME_KEY: &str = "x-vf-src-name";

/// What [`SRC_NAME_KEY`] conditions translate into: the 16-hex
/// `object_key_hash` of the object key, stamped as `x-vf-src-uri` at ingest
/// (the GC join key — filterable by protocol declaration).
const SRC_URI_KEY: &str = "x-vf-src-uri";

/// What the rewrite layer observed. `translated` drives the empty-hit note
/// (a zero-hit search with a src-name filter must not read as "nothing
/// relevant"); `constraints` (each `$eq` 1, each `$in` element 1 — the AWS
/// accounting `filter.rs` caps) and `sample` drive the constraints-cap
/// error's name echo.
#[derive(Default)]
struct SrcNameRewrite {
    translated: bool,
    constraints: usize,
    sample: Vec<String>,
}

impl SrcNameRewrite {
    fn record(&mut self, name: &str) {
        self.translated = true;
        self.constraints += 1;
        if self.sample.len() < 3 {
            self.sample.push(name.to_string());
        }
    }
}

/// Rewrite every `x-vf-src-name` condition into an `x-vf-src-uri` hash
/// condition (T15, ADR 0004) — the serde_json layer, because the compiled
/// `Filter` AST is private with no public rewrite surface. `$eq` (and the
/// bare-value shorthand) and `$in` (element-wise) translate;
/// [`vectorfs_common::chunkkey::object_key_hash`] is a pure function of the
/// object key, so the translation is semantics-preserving. Every OTHER
/// operator on the key is a field-addressable 400 pointing at `x-vf-src-uri`
/// (fail loud: hashing destroys prefix/list structure, and quiet acceptance
/// would smuggle a filterable key past the protocol declaration). `$and`/
/// `$or` arrays recurse; structurally foreign shapes pass through for
/// `Filter::compile` to report — this layer's only opinions are about the
/// src-name key.
fn rewrite_src_name_filter(
    v: &serde_json::Value,
) -> Result<(serde_json::Value, SrcNameRewrite), ApiError> {
    let mut info = SrcNameRewrite::default();
    let out = rewrite_src_name(v, &mut info)?;
    Ok((out, info))
}

fn rewrite_src_name(
    v: &serde_json::Value,
    info: &mut SrcNameRewrite,
) -> Result<serde_json::Value, ApiError> {
    let serde_json::Value::Object(map) = v else {
        return Ok(v.clone()); // non-objects are compile's structural errors
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (k, cond) in map {
        match k.as_str() {
            "$and" | "$or" => match cond {
                serde_json::Value::Array(items) => {
                    // recurse: a name nested under $and/$or translates too
                    let rewritten = items
                        .iter()
                        .map(|it| rewrite_src_name(it, info))
                        .collect::<Result<Vec<_>, _>>()?;
                    out.insert(k.clone(), serde_json::Value::Array(rewritten));
                }
                _ => {
                    out.insert(k.clone(), cond.clone()); // compile reports
                }
            },
            SRC_NAME_KEY => {
                // both keys in ONE object would collapse on insert (the
                // translated entry stomping the literal one) — refuse the
                // combination loudly instead of silently dropping a leg
                if map.contains_key(SRC_URI_KEY) {
                    return Err(ApiError::validation_field(
                        "filter",
                        "x-vf-src-name and x-vf-src-uri cannot appear in the same filter \
                         object — combine them with $and",
                    ));
                }
                let translated = rewrite_src_name_cond(cond, info)?;
                out.insert(SRC_URI_KEY.to_string(), translated);
            }
            _ => {
                out.insert(k.clone(), cond.clone());
            }
        }
    }
    Ok(serde_json::Value::Object(out))
}

/// One `x-vf-src-name` condition (the field's operand): translate to the
/// equivalent `x-vf-src-uri` condition, or refuse with guidance.
fn rewrite_src_name_cond(
    cond: &serde_json::Value,
    info: &mut SrcNameRewrite,
) -> Result<serde_json::Value, ApiError> {
    // bare string == implicit $eq (the filter grammar's shorthand)
    if let serde_json::Value::String(name) = cond {
        info.record(name);
        return Ok(json!(vectorfs_common::chunkkey::object_key_hash(name)));
    }
    let serde_json::Value::Object(ops) = cond else {
        return Err(ApiError::validation_field(
            "filter",
            format!(
                "x-vf-src-name condition must be a string (implicit $eq) or a $-operator \
                 object, got {}",
                json_type_name(cond)
            ),
        ));
    };
    let hash = vectorfs_common::chunkkey::object_key_hash;
    let mut out = serde_json::Map::new();
    for (op, arg) in ops {
        match op.as_str() {
            "$eq" => {
                let name = src_name_string_arg(arg)?;
                info.record(&name);
                out.insert("$eq".into(), json!(hash(&name)));
            }
            "$in" => {
                let serde_json::Value::Array(items) = arg else {
                    return Err(ApiError::validation_field(
                        "filter",
                        "filter.x-vf-src-name.$in requires an array of source-name strings",
                    ));
                };
                if items.is_empty() {
                    return Err(ApiError::validation_field(
                        "filter",
                        "filter.x-vf-src-name.$in array must not be empty",
                    ));
                }
                let names = items
                    .iter()
                    .map(src_name_string_arg)
                    .collect::<Result<Vec<_>, _>>()?;
                for n in &names {
                    info.record(n);
                }
                out.insert(
                    "$in".into(),
                    serde_json::Value::Array(names.iter().map(|n| json!(hash(n))).collect()),
                );
            }
            other => {
                return Err(ApiError::validation_field(
                    "filter",
                    format!(
                        "operator {other:?} does not support x-vf-src-name (source-name) \
                         filtering — only $eq (or a bare value) and $in translate to \
                         x-vf-src-uri; use x-vf-src-uri for other operators"
                    ),
                ));
            }
        }
    }
    if out.is_empty() {
        return Err(ApiError::validation_field(
            "filter",
            "filter.x-vf-src-name condition object must not be empty (supported operators: \
             $eq, $in)",
        ));
    }
    Ok(serde_json::Value::Object(out))
}

/// A src-name condition value must be a STRING (it is an object key):
/// numbers/booleans can never match a stored name, so compile would accept
/// them into a silent no-match — fail loud instead (the F5 discipline).
fn src_name_string_arg(v: &serde_json::Value) -> Result<String, ApiError> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        other => Err(ApiError::validation_field(
            "filter",
            format!(
                "x-vf-src-name values must be strings (they are object keys), got {}",
                json_type_name(other)
            ),
        )),
    }
}

/// JSON type name for the rewrite layer's own error messages (filter.rs's
/// `json_type` is private to that module).
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// The source-grouping key of one merged hit (R4): the stored
/// `x-vf-src-uri` from the attribution map's metadata — the same hash the
/// GC joins on and the src-name translation targets. `None` when the hit
/// carries no source hash (not reachable for file-layer chunks; callers
/// treat such a hit as its own bucket so the quota never bites malformed
/// metadata).
fn src_uri_of<'a>(
    attribution: &'a HashMap<String, (String, Option<f64>, Option<serde_json::Value>)>,
    key: &str,
) -> Option<&'a str> {
    attribution
        .get(key)
        .and_then(|(_, _, m)| m.as_ref())
        .and_then(|m| m.get("x-vf-src-uri"))
        .and_then(serde_json::Value::as_str)
}

/// Compile the Search filter: the user's `$`-operator filter, conjoined
/// with the default kind exclusion unless `include_kinds` admits every
/// excluded kind (T12).
///
/// **Negative-form predicate (the m3 trap)**: the exclusion compiles to
/// `x-vf-chunk-kind $nin [...]`, whose semantics on BOTH execution paths
/// treat a MISSING key as matching — memory `eval_cond` (`!stored
/// .is_some_and(..)`) and SQL pushdown (`col IS NULL OR col NOT IN ..`).
/// A bare `!=`/`NOT IN` would let SQL three-valued logic drop every
/// NULL row — which is exactly the LEGACY pre-T12 population; 存量 null 行
/// 默认可见 is pinned by test.
///
/// `include_kinds` values are validated against the closed kind
/// vocabulary — an unknown kind is a field-addressable ValidationException
/// (the m5 discipline), never a silent no-match.
///
/// T15: before compiling, the user filter passes the src-name rewrite
/// ([`rewrite_src_name_filter`]) — `/x-vf/Search` accepts `x-vf-src-name`
/// `$eq`/`$in` conditions and translates them to `x-vf-src-uri` hashes.
/// The returned [`SrcNameRewrite`] tells the caller whether translation
/// was active (the empty-hit note's trigger).
fn search_filter(
    user: &Option<serde_json::Value>,
    include_kinds: &Option<Vec<String>>,
) -> Result<(Option<crate::filter::Filter>, SrcNameRewrite), ApiError> {
    // validate + resolve the admitted set first (unknown kinds fail loud
    // even when the exclusion would end up empty — a typo'd kind name must
    // not silently pass as "no-op")
    let mut admitted: Vec<&str> = Vec::new();
    if let Some(kinds) = include_kinds {
        for k in kinds {
            if crate::pipeline::ChunkKind::parse(k).is_none() {
                return Err(ApiError::validation_field(
                    "includeKinds",
                    format!(
                        "unknown includeKinds entry {k:?} (allowed kinds: body, toc, caption, \
                         legend — the x-vf-chunk-kind vocabulary)"
                    ),
                ));
            }
            admitted.push(k.as_str());
        }
    }
    let excluded: Vec<&str> = SEARCH_DEFAULT_EXCLUDED_KINDS
        .into_iter()
        .filter(|k| !admitted.contains(k))
        .collect();
    // T15 rewrite FIRST — the translation is what the user wrote, the kind
    // injection below composes with the TRANSLATED filter
    let (user, src_names) = match user {
        None | Some(serde_json::Value::Null) => (None, SrcNameRewrite::default()),
        Some(f) => {
            let (rewritten, info) = rewrite_src_name_filter(f)?;
            (Some(rewritten), info)
        }
    };
    // everything admitted: no injection, the user filter stands alone
    // (byte-identical compile + error path of the pre-T12 behavior for
    // src-name-free filters; with src-name conditions the rewrite has
    // already applied by design)
    if excluded.is_empty() {
        return Ok((
            match user {
                None => None,
                Some(f) => Some(compile_filter(&f, &src_names)?),
            },
            src_names,
        ));
    }
    // conjoin at the JSON level ($and composes with any user grammar):
    // `user AND kind $nin excluded` — one compiled filter both routes run
    let injection = serde_json::json!({
        "x-vf-chunk-kind": { "$nin": excluded },
    });
    let combined = match user {
        None => injection,
        Some(f) => serde_json::json!({ "$and": [f, injection] }),
    };
    Ok((Some(compile_filter(&combined, &src_names)?), src_names))
}

/// Compile + error mapping: ValidationException(path = filter), and when
/// the 100-constraint cap ([`crate::filter::MAX_CONSTRAINTS`]) fires with
/// translated source names aboard, the message echoes a sample of the
/// NAMES the user wrote (a `$in` of 150 keys busts the cap; the error must
/// teach which filter element did it — never leave the user staring at
/// hash values they never typed). The `"the maximum per query is"` probe
/// matches the cap error's phrasing in `filter.rs` — same crate, one
/// definition site away.
fn compile_filter(
    f: &serde_json::Value,
    src: &SrcNameRewrite,
) -> Result<crate::filter::Filter, ApiError> {
    crate::filter::Filter::compile(f).map_err(|e| {
        let msg = if src.constraints > 0 && e.0.contains("the maximum per query is") {
            format!(
                "{} — the x-vf-src-name filter contributes {} of them (translated from names \
                 like {}); narrow the source list, or filter x-vf-src-uri hashes directly",
                e.0,
                src.constraints,
                src.sample
                    .iter()
                    .map(|n| format!("{n:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            e.0
        };
        ApiError::validation_field("filter", msg)
    })
}

/// `POST /x-vf/Search` (appendix E.2) — the flagship superset op: embed the
/// query once per QUERY-MODALITY-matching embedder, query each derived index
/// (topK, `$`-short-name filter, pre-filter pushdown — the same
/// [`crate::filter`] engine QueryVectors runs), merge with the bucket's
/// configured strategy (M2: rrf), and emit hits shaped like KB's
/// KnowledgeBaseRetrievalResult plus the fan-out fields
/// (`index`/`key`/`distance`).
///
/// src-name translation (T15, ADR 0004): this face accepts
/// `x-vf-src-name` `$eq`/`$in` (bare value = `$eq`) conditions and
/// translates them to `x-vf-src-uri` hashes before compile — the ONE
/// superset extension to the filter grammar; every other operator on the
/// key is a 400 pointing at `x-vf-src-uri`. The protocol face
/// (QueryVectors) rejects the same key as non-filterable, unchanged.
///
/// Query modalities (appendix E.2 `query{text|image|audio(M4)}`): text since
/// M2; IMAGE since M3-T4 — `{bytes: base64, mime}` (see
/// [`SearchImageInput`]), routed to embedders whose query side declares
/// Image (siglip2's vision tower). A TEXT query still fans out to every
/// text-query embedder — including siglip2's IMAGE index, 文字搜图 (the
/// dual-tower default route; the merge treats it like any second list).
///
/// `searchMode` (M3-T9, ADR 0002; default `semantic` — absent = old
/// behavior, old clients zero-change):
/// * `semantic` — the pre-T9 fan-out above;
/// * `keyword` — pure BM25: per route, the lexical kernel
///   ([`crate::lexical`], Lance native FTS) runs `query.text` against the
///   derived table's `text` column; lists merge with the bucket's strategy
///   exactly like semantic routes. Requires a TEXT query (an image has no
///   lexical side) and the bucket's `fts = true` (`[ingest.<bucket>]`); a
///   table that predates the lexical layer fails LOUD with the
///   `vectorfs optimize` fix (never a silent empty result — that is the F1
///   friction this mode exists to kill);
/// * `hybrid` — BOTH arms per route: the semantic list plus the BM25 list,
///   all lists fused by the same bucket strategy (rrf, or `weighted` where
///   BOTH arms of a route share the embedder instance's weight).
///
/// `rerank: true` (M3-T11, per-request; default absent = the pre-T11
/// behavior): after the merge, re-score the topK candidates with the
/// bucket's cross-encoder (`[ingest.<bucket>] rerank = true` gates the
/// model's load at serve startup; the §5.10.3 data flow's merge → rerank →
/// 回源). Needs a TEXT query and a non-keyword mode (keyword's BM25
/// ranking IS its semantics); hits come back in rerank order with the
/// score field carrying the RAW cross-encoder logit, and `distance` is
/// omitted (the order no longer derives from it).
///
/// `rerankDepth` (R3a, recall-quality first batch): decouples the re-scoring
/// depth from the return depth — the cross-encoder scores the first
/// `rerankDepth` merged candidates, topK still come back. Absent = topK
/// (zero behavior change); 1..=200 and >= topK, both enforced loudly; a
/// depth without `rerank: true` is a dead knob and refused. The arms fetch
/// `min(max(topK, rerankDepth) × fusion-overfetch, 1000)` so the depth has
/// a fusion output deep enough to re-score at the default multiplier; the
/// bucket's `fusion-overfetch` (R3b) composes on top for still-deeper
/// pools.
/// CPU honesty: ~148 ms/pair (research/2026-11-m3-bench.md §2) — depth 20
/// ≈ 3 s, depth 50 ≈ 7.4 s.
///
/// `perSource` (R4): per-source quota on the merged order, applied BEFORE
/// any truncation (rerank included — the cross-encoder sees the quota'd
/// candidate set). At most N hits per `x-vf-src-uri` survive, walked in
/// fusion order; fewer than topK are returned when the quota thins the
/// set (no backfill — backfilling would defeat the quota). Absent/0 = off.
///
/// `orderBy` (T18, R8 structure axis): terminal view ordering of the FINAL
/// hit set — `"section-asc"`/`"section-desc"` sort by the chapter ordinal
/// `x-vf-section` (key sequence section, `x-vf-line-start`, score desc,
/// key asc; a MISSING section sorts NULLS-LAST in both directions).
/// Applied after quota → truncation → rerank: presentation only, the
/// membership and every hit's `score` keep their meaning (composed with
/// rerank, the score column still carries the rerank logit). Absent = the
/// retrieval order; unknown values are refused at `path=orderBy` (the
/// searchMode discipline). A ranking policy, NOT in the fingerprint.
///
/// Anchor gating (R1→T17, on by DEFAULT for lexical faces of fts buckets —
/// bucket knob `anchor-gating = true|false`): before the lexical arm runs,
/// the query's quoted spans (any script, whole) and analyzer terms (the
/// kernel's SAME pipeline as the index — CJK included, the R1 blind spot)
/// are probed against the FTS index by POSTINGS COUNT (≤8 small
/// single-term FTS queries at the cap, plus stage-two exact counts when
/// the head ties at the cap — see `lexical::analyze_anchors`); the
/// RAREST df≥1 candidate (the argmin, ties first-seen) becomes the MUST
/// constraint of a Boolean FTS query — chunks missing it leave the LEXICAL
/// arm only (the semantic arm is untouched: a misjudged anchor costs the
/// lexical enhancement, never a result). Queries whose every candidate is
/// corpus-absent (df=0) ride the ungated behavior; all-structure-word
/// queries GATE anyway (T17 恒门 — the df-ceiling escape hatch is
/// retired); a probe failure degrades to the ungated behavior with a
/// stderr note. The response carries an `anchors` member beside `hits`
/// exactly when the analysis ran: the probed candidates that exist in the
/// corpus, df ASCENDING — the FIRST entry is the term the lexical arm was
/// gated on, whatever its df absolute value; further entries ride the
/// ranking body (empty = every candidate df=0, nothing gates).
///
/// Hit shape, field by field (appendix E.2):
/// * `content{text?, type: "TEXT"|"IMAGE"}` — `type` follows the CHUNK's
///   modality (`x-vf-modality`; image chunks answer IMAGE whatever the
///   query's modality was); `text` = the stored `x-vf-chunk-text`, omitted
///   when `returnText=false`, `store_chunk_text=false`, or the chunk is an
///   image (image chunks store no text — the source file is the content);
/// * `score` — the merged score. Single list: passthrough of the route's
///   own score — semantic `1/(1+distance)` (monotone in distance, so
///   thresholds stay meaningful), keyword the kernel's RAW BM25
///   (corpus-statistics-dependent: comparable within one index's ranking,
///   never a calibrated relevance across corpora or queries — treat it as
///   an ordering, not a number). Multiple lists: the rrf sum, or with
///   `merge = "weighted"` (M3-T8) the per-embedder-instance weight times
///   each rank contribution — RANK-based either way, never raw scores
///   (different embedders/kernels are dimensionally incomparable; see
///   `pipeline::merge_hitlists_weighted`);
/// * `distance` — emitted only for single-LIST semantic queries (appendix
///   E.2: "单路查询时有意义"; keyword hits have no vector distance and a
///   fused hit has no single distance);
/// * `index`/`key` — derived index name + chunk key; attribution is the
///   FIRST route (config order) containing the key when several do;
/// * `location{type:"source", key, coord?}` — `key` = the full object key
///   (`x-vf-src-name`) — the回源 join; `coord` (T13, D-6/D-7) is the
///   addressing summary distilled from the stored coordinate keys —
///   `{page?:{start,end}, line?:{start,end}, section?}`, a member present
///   only when the chunk carries that unit (pdf ⇒ page, pageless ⇒ line,
///   container children ⇒ section), `coord` itself omitted when the chunk
///   carries none. The appendix E.2 `page?` hook this supersedes;
/// * `metadata` — the chunk's stored metadata verbatim (the protocol face
///   needs no coord member: QueryVectors returns this same metadata, which
///   already carries the `x-vf-*` coordinate keys).
///
/// Response-level member (T15): `note` appears BESIDE `hits` — never on
/// hits — exactly when a translated src-name filter is active and the hit
/// set is empty (a wrong name hashes to a value no indexed source
/// carries; the note points at key verification, 打错字 ≠ 没搜到).
pub async fn search(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: SearchInput = parse_body(&body)?;
    let handle = require_ingest(&state, &input.bucket)?;
    let shared = handle.shared();

    let mode = match input.search_mode.as_deref() {
        None | Some("semantic") => SearchMode::Semantic,
        Some("keyword") => SearchMode::Keyword,
        Some("hybrid") => SearchMode::Hybrid,
        Some(other) => {
            return Err(ApiError::validation_field(
                "searchMode",
                format!(
                    "unknown searchMode {other:?} (allowed: semantic, keyword, hybrid) — \
                     semantic is the default"
                ),
            ));
        }
    };

    // T18 orderBy (D-10.4): closed vocabulary, 400 at the field (the
    // searchMode discipline — an unknown value dies at the field that
    // carries it, never a silent no-op ordering)
    let order_by = match input.order_by.as_deref() {
        None => None,
        Some("section-asc") => Some(OrderBy::SectionAsc),
        Some("section-desc") => Some(OrderBy::SectionDesc),
        Some(other) => {
            return Err(ApiError::validation_field(
                "orderBy",
                format!(
                    "unknown orderBy {other:?} (allowed: section-asc, section-desc) — absent \
                     keeps the retrieval order"
                ),
            ));
        }
    };

    let query = input.query.ok_or_else(|| {
        ApiError::validation("query is required (query.text or query.image; audio arrives in M4)")
    })?;
    if query.audio.is_some() {
        return Err(ApiError::validation_field(
            "query",
            "audio queries are not supported yet (query.audio arrives in M4); use query.text \
             or query.image",
        ));
    }
    // exactly one modality per query (a Query IS one modality — the
    // pipeline's query enum has no mixed variant, and guessing a precedence
    // between two given members would silently drop the other)
    let query = match (query.text, query.image) {
        (Some(_), Some(_)) => {
            return Err(ApiError::validation_field(
                "query",
                "pass exactly one of query.text / query.image — a search query is one \
                 modality",
            ));
        }
        (None, None) => {
            return Err(ApiError::validation_field(
                "query",
                "query must carry query.text (a non-empty string) or query.image \
                 ({bytes, mime})",
            ));
        }
        (Some(t), None) => {
            if t.trim().is_empty() {
                return Err(ApiError::validation_field(
                    "query.text",
                    "query.text must be a non-empty string",
                ));
            }
            Query::Text(t)
        }
        (None, Some(img)) => {
            // keyword/hybrid are lexical faces: they need TEXT (an image
            // query has no tokens to match)
            if mode != SearchMode::Semantic {
                return Err(ApiError::validation_field(
                    "searchMode",
                    format!(
                        "searchMode {:?} needs query.text — the lexical arm matches tokens; \
                         use semantic for image queries",
                        input.search_mode.as_deref().unwrap_or("semantic")
                    ),
                ));
            }
            let (bytes, mime) = decode_image_query(&img)?;
            Query::Image(bytes, mime)
        }
    };
    let query_modality = match &query {
        Query::Text(_) => Modality::Text,
        Query::Image(_, _) => Modality::Image,
    };

    // the lexical face is opt-in per bucket (§7: M3 superset content ships
    // OFF); refusing here — not returning empty hits — makes the config
    // gap a one-line fix instead of a mystery
    if mode != SearchMode::Semantic && shared.fts_of(&input.bucket) != Some(true) {
        return Err(ApiError::validation_field(
            "searchMode",
            format!(
                "bucket {:?} has the lexical face disabled — set `fts = true` under \
                 [ingest.{}] in vectorfs.toml and run `vectorfs optimize` to build the FTS \
                 index (restart the daemon for the config change)",
                input.bucket, input.bucket
            ),
        ));
    }

    // rerank (M3-T11) applicability — three loud refusals, each a one-line
    // fix, none a silent half-feature (the searchMode discipline):
    // * the bucket must carry a loaded reranker (availability knob, §7);
    // * the query must be TEXT (a text cross-encoder has no image arm);
    // * the mode must not be keyword (BM25's ranking semantics IS that
    //   mode's contract — silently re-sorting exact-term hits by neural
    //   relevance would defeat the point of asking for keyword).
    let reranking = input.rerank.unwrap_or(false);
    let reranker = if reranking {
        let Some(r) = shared.reranker_of(&input.bucket) else {
            return Err(ApiError::validation_field(
                "rerank",
                format!(
                    "bucket {:?} has no reranker configured — add `rerank = true` under \
                     [ingest.{}] in vectorfs.toml (loads the gte cross-encoder at startup) and \
                     restart the daemon",
                    input.bucket, input.bucket
                ),
            ));
        };
        if !matches!(query, Query::Text(_)) {
            return Err(ApiError::validation_field(
                "rerank",
                "rerank needs query.text — the cross-encoder scores (query text, chunk text) \
                 pairs; use rerank with a text query",
            ));
        }
        if mode == SearchMode::Keyword {
            return Err(ApiError::validation_field(
                "rerank",
                "rerank does not apply to searchMode \"keyword\" — pure BM25's ranking is that \
                 mode's semantics; use semantic or hybrid for cross-encoder precision ordering",
            ));
        }
        Some(r)
    } else {
        None
    };

    let top_k = match input.top_k {
        None => 10,
        Some(k) if (1..=1000).contains(&k) => k as usize,
        Some(k) => {
            return Err(ApiError::validation_field(
                "topK",
                format!("topK must be between 1 and 1000, got {k}"),
            ));
        }
    };

    // rerankDepth (R3a): decouple the re-scoring depth from the return
    // depth. Absent = topK (zero behavior change); explicit values must be
    // usable — a depth without rerank is a dead knob and a depth below topK
    // would SHRINK the result set (both die loud, the fts-tokenizer-on-
    // fts-off discipline: bad values die at the field that carries them).
    let rerank_depth = match input.rerank_depth {
        None => top_k,
        Some(d) if (1..=200).contains(&d) => d as usize,
        Some(d) => {
            return Err(ApiError::validation_field(
                "rerankDepth",
                format!("rerankDepth must be between 1 and 200, got {d}"),
            ));
        }
    };
    if input.rerank_depth.is_some() {
        if !reranking {
            return Err(ApiError::validation_field(
                "rerankDepth",
                "rerankDepth is set but rerank is not true — the depth only sizes the \
                 cross-encoder's candidate set; pass rerank: true or drop rerankDepth",
            ));
        }
        if rerank_depth < top_k {
            return Err(ApiError::validation_field(
                "rerankDepth",
                format!(
                    "rerankDepth must be >= topK (rerank re-scores at least what would be \
                     returned), got rerankDepth {rerank_depth} < topK {top_k}"
                ),
            ));
        }
    }

    // perSource (R4): the per-source quota. Absent/0 = off (zero behavior
    // change); 1..=100 = at most N hits per source file in the final order.
    let per_source = match input.per_source {
        None | Some(0) => None,
        Some(n) if (1..=100).contains(&n) => Some(n as usize),
        Some(n) => {
            return Err(ApiError::validation_field(
                "perSource",
                format!("perSource must be between 0 and 100 (0 = off), got {n}"),
            ));
        }
    };
    let return_text = input.return_text.unwrap_or(true);

    // filter: `$` short-name syntax, same compile + non-filterable rejection
    // as QueryVectors (appendix E.5: the two official grammars stay as-is)
    // — PLUS the T15 superset extension: `x-vf-src-name` `$eq`/`$in`
    // conditions translate to `x-vf-src-uri` hashes before compile (the
    // protocol face rejects the same key; the two faces differ by design,
    // ADR 0004). T12 kind governance composes INTO the same compiled
    // filter (see [`search_filter`]): one filter object is what BOTH
    // routes consume — the semantic routes' pushdown and the lexical
    // kernel's pre/post filter — so the default exclusion covers every
    // searchMode at once.
    let (compiled, src_names) = search_filter(&input.filter, &input.include_kinds)?;

    let names = shared.embedder_names(&input.bucket).unwrap_or_default();
    if names.is_empty() {
        return Err(ApiError::validation(format!(
            "bucket {:?} lists no embedders — Search fans out over the [ingest.{}] embedders; \
             add at least one to vectorfs.toml",
            input.bucket, input.bucket
        )));
    }

    // fan-out: query-modality-matching embedders, config order. A TEXT
    // query fans out to every text-query embedder — which, since siglip2's
    // query side declares [Image, Text], INCLUDES the image index (文字搜图,
    // the research-file default dual route); an IMAGE query fans out to the
    // image-query embedders only (a text tower cannot embed bytes).
    // M3-T9: hybrid appends the BM25 list of the SAME route after its
    // semantic list — one route contributes two lists (both named by the
    // embedder instance, so merge-weights weigh a route's both arms alike).
    //
    // R3b fusion over-fetch: each ARM fetches min(max(topK, rerankDepth) ×
    // the bucket's `fusion-overfetch`, 1000) candidates instead of topK —
    // a deeper candidate pool behind the same merge (a rank-15-both-arms
    // chunk can now out-fuse a rank-1-one-arm one). The `max(topK,
    // rerankDepth)` base is the R3a linkage: a rerankDepth deeper than
    // topK must have a fusion output deep enough to re-score, even at the
    // default multiplier. The merge truncates back below; multiplier 1
    // (the config default) makes fetch_k == max(topK, rerankDepth) — the
    // exact pre-R3b behavior for every depth-less request. A single-list
    // result is order-identical either way (a deeper fetch only APPENDS to
    // a list whose ranking is already total), so the knob bites fused
    // faces only.
    let fetch_k = (rerank_depth.max(top_k)
        * shared.fusion_overfetch_of(&input.bucket).unwrap_or(1) as usize)
        .min(1000);
    let lexical = crate::lexical::kernel();
    // R1/T17 anchor gating (recall-quality roadmap, v3 two-stage count
    // probe): on lexical faces of an fts bucket with `anchor-gating` on
    // (the default), the query is analyzed ONCE — its quoted spans and
    // same-pipeline analyzer terms (the bucket's tokenizer, CJK included)
    // are probed against the first route's derived index by POSTINGS
    // COUNT (≤ lexical::MAX_PROBES small FTS queries at the cap; a
    // head-position saturation tie escalates to exact counts, D-8.4);
    // every df≥1 candidate is an anchor and the RAREST (verdict head, the
    // argmin) becomes the MUST constraint of the gated Boolean FTS query.
    // `None` = analysis not applicable (semantic mode / fts off / knob
    // off) or degraded (probe error — current behavior, one stderr note);
    // `Some(vec![])` = analyzed, no anchor exists (every candidate df=0 —
    // the ungated behavior IS the answer). The SEMANTIC arm never sees
    // the gate: a misjudged anchor costs the lexical ENHANCEMENT, never a
    // result.
    let anchor_gating_on = mode != SearchMode::Semantic
        && shared.fts_of(&input.bucket) == Some(true)
        && shared.anchor_gating_of(&input.bucket) != Some(false);
    let mut anchors: Option<Vec<String>> = None;
    let mut anchors_settled = false;
    let query_text = match &query {
        Query::Text(t) => Some(t.as_str()),
        Query::Image(_, _) => None,
    };
    let mut lists: Vec<HitList> = Vec::new();
    // route identity parallel to `lists` (M3-T8): the embedder INSTANCE name
    // of each list — the key merge-weights are looked up by
    let mut routes: Vec<String> = Vec::new();
    let mut matched_any = false;
    // key -> (index name, distance?, stored metadata); first route wins.
    // distance is None for keys whose only evidence is a BM25 hit (keyword
    // mode / a hybrid-only key) — the wire emits it for single-LIST
    // semantic queries only anyway.
    let mut attribution: HashMap<String, (String, Option<f64>, Option<serde_json::Value>)> =
        HashMap::new();
    for name in &names {
        let Some(loaded) = shared.embedder(name) else {
            continue;
        };
        if !loaded.query_modalities().contains(&query_modality) {
            continue; // not a route for THIS query's modality
        }
        matched_any = true;
        let Some(spec) = shared
            .derived_specs(&input.bucket)
            .unwrap_or_default()
            .into_iter()
            .find(|s| &s.embedder == name)
        else {
            continue;
        };
        // not-yet-created derived index (nothing ever ingested): the route
        // contributes an empty list rather than a 500 on a missing table
        let Some(meta) = state
            .storage
            .load_index(&input.bucket, &spec.index_name)
            .await?
        else {
            lists.push(Vec::new());
            routes.push(name.clone());
            continue;
        };
        if let Some(nf) = meta.non_filterable_metadata_keys.as_deref()
            && let Some(cf) = &compiled
        {
            for field in cf.fields() {
                if nf.iter().any(|k| k == field) {
                    return Err(ApiError::validation_field(
                        "filter",
                        format!(
                            "metadata key {field:?} is declared non-filterable for this index \
                             and cannot be used in a query filter"
                        ),
                    ));
                }
            }
        }
        if mode != SearchMode::Keyword {
            let qvec = loaded.embed_query(&query).await.map_err(|e| {
                ApiError::internal(format!("embedder '{name}' failed to embed the query: {e}"))
            })?;
            if qvec.len() != meta.dimension as usize {
                return Err(ApiError::internal(format!(
                    "embedder '{name}' produced a {}-dim query vector but derived index {} has \
                     dimension {} — the pipeline config and the index drifted; run reindex",
                    qvec.len(),
                    spec.index_name,
                    meta.dimension
                )));
            }
            let outcome = state
                .storage
                .query_vectors(
                    &input.bucket,
                    &spec.index_name,
                    meta.dimension,
                    &meta.distance_metric,
                    &qvec,
                    fetch_k,
                    compiled.as_ref(),
                    QueryRoute::Pushdown,
                    None,
                )
                .await?;
            let list: HitList = outcome
                .hits
                .iter()
                .map(|h| {
                    attribution.entry(h.key.clone()).or_insert_with(|| {
                        (
                            spec.index_name.clone(),
                            Some(h.distance as f64),
                            h.metadata.clone(),
                        )
                    });
                    crate::pipeline::Hit {
                        key: h.key.clone(),
                        score: 1.0 / (1.0 + h.distance as f64),
                    }
                })
                .collect();
            lists.push(list);
            routes.push(name.clone());
        }
        if mode != SearchMode::Semantic
            && let Some(text) = query_text
        {
            // R1: settle the anchor analysis at the FIRST lexical arm this
            // query runs (against that route's index — every route's
            // derived table carries the same chunk texts, so the IDF proxy
            // is route-independent); later routes reuse the verdict.
            if !anchors_settled {
                anchors_settled = true;
                if anchor_gating_on {
                    // T17 (D-8.2/D-8.3): the bucket's CONFIGURED tokenizer
                    // (F-4: query-time resolvable; the icu default mirrors
                    // what an unconfigured optimize builds) drives the
                    // analyze() half of the candidate extraction — the
                    // same pipeline the index was built with.
                    let tokenizer = shared
                        .fts_tokenizer_of(&input.bucket)
                        .unwrap_or(crate::pipeline::config::FtsTokenizer::Icu);
                    match crate::lexical::analyze_anchors(
                        lexical.as_ref(),
                        &state.storage,
                        &input.bucket,
                        &spec.index_name,
                        tokenizer,
                        text,
                    )
                    .await
                    {
                        Ok(a) => anchors = Some(a),
                        Err(e) => {
                            // probe failure (FTS face missing, pre-T9
                            // table, …): degrade to the ungated behavior —
                            // the note rides stderr (daemon log), the wire
                            // shape stays the pre-R1 one
                            eprintln!(
                                "search: anchor analysis failed for bucket {:?} index {:?} — \
                                 gating degraded to the ungated lexical query: {e}",
                                input.bucket, spec.index_name
                            );
                        }
                    }
                }
            }
            // the lexical arm rides the lexical-kernel seam (ADR 0002's
            // swap path); StoreError::Validation (pre-T9 table) surfaces as
            // a ValidationException carrying the optimize fix. With
            // anchors: search_gated — the MUST constraint is the verdict's
            // HEAD (the single rarest anchor, the v2 gate ruling measured
            // out of the f1 corpus: AND-of-several evicts correct chunks
            // whose fellow rare terms straddle chunk boundaries), the
            // original query stays the ranking body (R1).
            let hits = match anchors.as_ref().filter(|a| !a.is_empty()) {
                Some(gate) => {
                    lexical
                        .search_gated(
                            &state.storage,
                            &input.bucket,
                            &spec.index_name,
                            std::slice::from_ref(&gate[0]),
                            text,
                            fetch_k,
                            compiled.as_ref(),
                        )
                        .await?
                }
                None => {
                    lexical
                        .search(
                            &state.storage,
                            &input.bucket,
                            &spec.index_name,
                            text,
                            fetch_k,
                            compiled.as_ref(),
                        )
                        .await?
                }
            };
            let list: HitList = hits
                .iter()
                .map(|h| {
                    attribution
                        .entry(h.key.clone())
                        .or_insert_with(|| (spec.index_name.clone(), None, h.metadata.clone()));
                    crate::pipeline::Hit {
                        key: h.key.clone(),
                        score: h.score,
                    }
                })
                .collect();
            lists.push(list);
            routes.push(name.clone());
        }
    }
    if !matched_any {
        return Err(ApiError::validation(format!(
            "no embedder of bucket {:?} accepts {} queries — check the [embedder.*] query \
             modalities in vectorfs.toml (the siglip2 kind accepts image AND text queries)",
            input.bucket, query_modality
        )));
    }

    let merge = shared.merge_of(&input.bucket).unwrap_or(Merge::Rrf);
    let single_list = lists.len() == 1;
    let mut merged = match merge {
        Merge::Rrf => merge_hitlists(Merge::Rrf, &lists),
        // weighted (M3-T8): per-embedder-instance weights on each rank
        // contribution (pipeline::merge_hitlists_weighted documents why the
        // fusion is RANK-based — raw distances of different embedders are
        // dimensionally incomparable). Weightless instances count 1.0. A
        // hybrid route's semantic and BM25 lists share the instance name,
        // so its weight weighs both arms alike.
        Merge::Weighted => {
            let empty = std::collections::BTreeMap::new();
            let weights = shared.merge_weights_of(&input.bucket).unwrap_or(&empty);
            let named: Vec<(&str, &HitList)> = routes
                .iter()
                .map(String::as_str)
                .zip(lists.iter())
                .collect();
            merge_hitlists_weighted(&named, weights)
        }
    };
    // ---- R4 per-source quota: merge order settled, truncation NOT yet —
    // the quota walks the FULL merged ranking in fusion order and keeps at
    // most `perSource` entries per source file (bucket-full entries are
    // skipped, order preserved). Deliberately BEFORE any truncation (a
    // same-source crowd must not eat the rerank budget either) and with NO
    // backfill: if the quota leaves fewer than topK hits, fewer are
    // returned — refilling the gaps with the skipped same-source hits would
    // defeat the quota's whole point (the textbook-794-chunks crowding
    // shape, research/2026-11-recall-quality-optimization.md R4).
    if let Some(n) = per_source {
        let mut per_src: HashMap<String, usize> = HashMap::new();
        merged.retain(|h| {
            // the grouping key is the attribution map's source hash (the
            // same `x-vf-src-uri` the GC joins on); a hit without one (not
            // possible for file-layer chunks, but honestly) is its own
            // bucket — the quota never bites malformed metadata
            let key = src_uri_of(&attribution, &h.key)
                .unwrap_or(h.key.as_str())
                .to_string();
            let count = per_src.entry(key).or_insert(0);
            if *count >= n {
                false
            } else {
                *count += 1;
                true
            }
        });
    }
    merged.truncate(rerank_depth.max(top_k)); // fusions can exceed topK across route unions; R3a: a rerankDepth deeper than topK widens the re-scoring pool

    // ---- rerank (M3-T11 + R3a depth): merge → quota → truncate(depth) →
    // rerank → truncate(topK) → 回源 (§5.10.3 data flow). The candidates
    // are the (quota'd) merged head — topK of them by default, or
    // rerankDepth when the request decouples the depths (each carries its
    // stored x-vf-chunk-text, empty for image chunks / text-less buckets —
    // scored, never dropped). The reranker's output replaces the merged
    // order AND the scores (raw cross-encoder logits, monotone in
    // relevance); `distance` stops being emitted below because the order
    // no longer derives from it. When depth > topK the re-scored tail is
    // truncated away here — the response still returns topK.
    if let Some(reranker) = reranker {
        let query_text = match &query {
            Query::Text(t) => t.as_str(),
            Query::Image(_, _) => unreachable!("validated above: rerank needs a text query"),
        };
        let candidates: Vec<crate::pipeline::RerankCandidate> = merged
            .iter()
            .map(|h| crate::pipeline::RerankCandidate {
                key: h.key.clone(),
                text: attribution
                    .get(&h.key)
                    .and_then(|(_, _, m)| m.as_ref())
                    .and_then(|m| m.get("x-vf-chunk-text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect();
        merged = reranker
            .rerank(query_text, &candidates)
            .await
            .map_err(|e| {
                ApiError::internal(format!(
                    "reranker of bucket {:?} failed to score the merged candidates: {e}",
                    input.bucket
                ))
            })?;
        // R3a: the depths are decoupled on the WAY IN only — re-score
        // rerankDepth candidates, still RETURN topK (a no-op at the
        // default depth == topK).
        merged.truncate(top_k);
    }

    // ---- T18 orderBy (R8 structure axis, D-10.4): the terminal view
    // ordering — 配额 → 截断 → rerank → 排序 (§3's order of record; only
    // the PRESENTATION moves, never the membership). Key sequence:
    // (section, line-start, score desc, key asc), with a MISSING key
    // sorting NULLS-LAST in both directions (a hit without a section is
    // not "chapter zero" — it is a differently-shaped source, and pinning
    // it last keeps container-only orderings readable in mixed buckets).
    // `section-desc` flips ONLY the section key; the tie sequence
    // (line-start asc, score desc, key asc) is stable in both directions
    // — determinism over symmetry. Composes with rerank: applied AFTER
    // it, the presented order is the section order even though each
    // hit's `score` stays the RERANK LOGIT (the score column is what
    // produced the membership; the orderBy column is how you read it).
    // Ranking policy, deliberately NOT in the fingerprint (the T11/T12
    // rerank precedent — §3).
    if let Some(direction) = order_by {
        let descending = matches!(direction, OrderBy::SectionDesc);
        // per-key extraction, once per comparison arm: the stored
        // metadata is the single source (the coord summary below reads
        // the same keys)
        let section_of = |h: &crate::pipeline::Hit| -> Option<u64> {
            attribution
                .get(&h.key)
                .and_then(|(_, _, m)| m.as_ref())
                .and_then(|m| m.get("x-vf-section"))
                .and_then(serde_json::Value::as_u64)
        };
        let line_of = |h: &crate::pipeline::Hit| -> Option<u64> {
            attribution
                .get(&h.key)
                .and_then(|(_, _, m)| m.as_ref())
                .and_then(|m| m.get("x-vf-line-start"))
                .and_then(serde_json::Value::as_u64)
        };
        // nulls-last comparator for one Option<u64> key: None ranks
        // after EVERY Some, in both directions
        let opt_cmp = |x: Option<u64>, y: Option<u64>, flip: bool| -> std::cmp::Ordering {
            use std::cmp::Ordering;
            match (x, y) {
                (Some(x), Some(y)) if flip => y.cmp(&x),
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        };
        merged.sort_by(|a, b| {
            opt_cmp(section_of(a), section_of(b), descending)
                .then_with(|| opt_cmp(line_of(a), line_of(b), false))
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.key.cmp(&b.key))
        });
    }

    let hits: Vec<serde_json::Value> = merged
        .iter()
        .map(|h| {
            let (index, distance, metadata) = attribution
                .get(&h.key)
                .cloned()
                .expect("every merged key came from some route");
            // content.type follows the CHUNK's modality (appendix E.2's
            // mapping: page-image/image → IMAGE) — an image query (or a
            // text query fanned out onto an image index) returns image
            // chunks, and the display enum must say so
            let is_image = metadata
                .as_ref()
                .and_then(|m| m.get("x-vf-modality"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|m| m == "image");
            let mut content = json!({ "type": if is_image { "IMAGE" } else { "TEXT" } });
            // empty text adds nothing and image chunks HAVE none (their
            // stored x-vf-chunk-text is the empty string): omit the member
            // — text chunks are never empty, so this only shapes image hits
            if return_text
                && let Some(t) = metadata
                    .as_ref()
                    .and_then(|m| m.get("x-vf-chunk-text"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|t| !t.is_empty())
            {
                content["text"] = json!(t);
            }
            let mut o = json!({
                "content": content,
                "score": h.score,
                "index": index,
                "key": h.key,
            });
            if single_list && mode == SearchMode::Semantic && !reranking {
                // suppressed under rerank (M3-T11): the hit's order no
                // longer derives from the vector distance, and a stale
                // distance would mislead score/threshold reasoning
                o["distance"] = json!(distance);
            }
            if let Some(m) = &metadata {
                let mut location = json!({
                    "type": "source",
                    "key": m.get("x-vf-src-name").cloned().unwrap_or(serde_json::Value::Null),
                });
                // T13 coord summary (D-6): distilled from the STORED
                // coordinate keys, a member only when the chunk carries
                // that unit; `coord` itself only when any member exists.
                // Both pair members are read together or not at all (the
                // stamper always writes start+end in one insert).
                let interval = |start: &str, end: &str| -> Option<serde_json::Value> {
                    Some(json!({
                        "start": m.get(start)?.as_u64()?,
                        "end": m.get(end)?.as_u64()?,
                    }))
                };
                let mut coord = serde_json::Map::new();
                if let Some(page) = interval("x-vf-page-start", "x-vf-page-end") {
                    coord.insert("page".into(), page);
                }
                if let Some(line) = interval("x-vf-line-start", "x-vf-line-end") {
                    coord.insert("line".into(), line);
                }
                if let Some(section) = m.get("x-vf-section").and_then(serde_json::Value::as_u64) {
                    coord.insert("section".into(), json!(section));
                }
                if !coord.is_empty() {
                    location["coord"] = serde_json::Value::Object(coord);
                }
                o["location"] = location;
                o["metadata"] = m.clone();
            }
            o
        })
        .collect();
    // T15 empty-hit note (adversarial-review disposition): a src-name
    // filter that yields ZERO hits is indistinguishable on the wire from a
    // typo'd key — the wrong name hashes to a value no indexed source
    // carries, and the silent empty list reads as "nothing relevant".
    // One restrained member, only when translation was active AND the hit
    // set is empty (打错字 ≠ 没搜到).
    let mut body = serde_json::Map::with_capacity(3);
    body.insert("hits".into(), json!(hits));
    if hits.is_empty() && src_names.translated {
        body.insert(
            "note".into(),
            json!(
                "x-vf-src-name filter matched no indexed sources — verify the exact key \
                 (e.g. `vf ls`, ListSources); a source whose chunks simply did not match \
                 the query also yields zero hits"
            ),
        );
    }
    // R1/T17 observability, the restrained response-face form: `anchors`
    // appears BESIDE `hits` exactly when the anchor analysis RAN — the
    // VERDICT list, df ASCENDING (v3, D-8.7): the FIRST entry is the MUST
    // term the lexical arm was gated on (the argmin-df candidate, its df
    // absolute value no longer a display filter — narrative corpora gate
    // on df-hundreds entities and the member must say so), further
    // entries are probed candidates riding the ranking body; an empty
    // array = analyzed, every candidate is corpus-absent (df=0 — the
    // ungated behavior). Absent = the analysis was not applicable
    // (semantic mode, fts off, `anchor-gating = false`) or degraded
    // (probe failure — see the stderr note); never a hit-level member.
    if let Some(a) = &anchors {
        body.insert("anchors".into(), json!(a));
    }
    Ok(Json(serde_json::Value::Object(body)).into_response())
}

// ---------------------------------------------------------------------------
// Reindex
// ---------------------------------------------------------------------------

/// `POST /x-vf/Reindex` (appendix E.2) -> 202 `{jobId}`.
///
/// The FULL mechanism lives in [`crate::ingest::reindex_bucket`] (T7; the
/// same code `vectorfs serve --reindex` runs at startup): clear every
/// target source's old chunks across ALL of the bucket's managed indexes
/// (old-derived ones included — a shrink reindex leaves zero residue), flip
/// the markers to PENDING under the CURRENT fingerprint, register a job,
/// wake. This op is the forced re-embed of a RUNNING daemon (config
/// unchanged); config-change drift is the startup flag's business.
pub async fn reindex(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ReindexInput {
        bucket: String,
    }
    let input: ReindexInput = parse_body(&body)?;
    let handle = require_ingest(&state, &input.bucket)?;
    if let Err(e) = state.locks.ensure_held(&input.bucket).await {
        return Err(match e {
            crate::lock::LockError::Held(who) => ApiError::conflict(format!(
                "vector bucket {} is locked by another writer ({who})",
                input.bucket
            )),
            crate::lock::LockError::Backend(e) => ApiError::internal(e),
        });
    }
    let outcome = crate::ingest::reindex_bucket(&state, handle.shared(), &input.bucket)
        .await
        .map_err(ApiError::internal)?;
    handle.wake();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "jobId": outcome.job_id })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GetIngestStatus
// ---------------------------------------------------------------------------

/// `POST /x-vf/GetIngestStatus` (appendix E.2): per-bucket marker counts
/// (all six states), pendingCount (PENDING+IN_PROGRESS — the work the file
/// layer still owes), the `numberOf*` statistics block (KB
/// IngestionJobStatistics field names; process-lifetime cumulative),
/// lastScanAt, the active pipeline fingerprint, the derived-index table,
/// the latest reindex job with live progress, and `inFlight` — the live
/// per-job progress register (2026-11-11 摄取进度可见性: stage + per-stage/
/// attempt clocks + children/chunks/batches counters; process-memory, empty
/// after restart by design). Omitted bucket = every configured bucket.
pub async fn get_ingest_status(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: GetIngestStatusInput = parse_body(&body)?;
    let Some(handle) = state.ingest.as_ref() else {
        // pure protocol face: the honest aggregate is an empty table
        return Ok(Json(json!({ "buckets": [] })).into_response());
    };
    let shared = handle.shared();

    let buckets = match input.bucket.as_deref() {
        None | Some("") => shared.bucket_names(),
        Some(b) => {
            require_ingest(&state, b)?;
            vec![b.to_string()]
        }
    };

    let mut out = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        let markers = state.storage.list_sources(&bucket).await?;
        let mut counts: BTreeMap<&str, u64> = BTreeMap::from([
            ("PENDING", 0),
            ("IN_PROGRESS", 0),
            ("PARTIALLY_INDEXED", 0),
            ("INDEXED", 0),
            ("FAILED", 0),
            ("DELETING", 0),
        ]);
        let mut pending_count = 0u64;
        let mut done = 0usize; // reindex progress: terminal markers
        // T12 audit aggregate: furniture lines removed across the bucket's
        // DURABLE markers (not the process-lifetime stats block — this
        // survives restarts like the markers it sums; Some(0) sources
        // contribute 0, pre-T12/failed markers contribute nothing)
        let mut furniture_lines: u64 = 0;
        for m in &markers {
            *counts.entry(m.status.as_str()).or_insert(0) += 1;
            furniture_lines += m.furniture_removed.unwrap_or(0) as u64;
            match m.status {
                SourceStatus::Pending | SourceStatus::InProgress => pending_count += 1,
                SourceStatus::Indexed | SourceStatus::PartiallyIndexed | SourceStatus::Failed => {
                    done += 1
                }
                SourceStatus::Deleting => {}
            }
        }
        let fingerprint = shared.fingerprint_of(&bucket).expect("configured bucket");
        let derived: Vec<serde_json::Value> = shared
            .derived_specs(&bucket)
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                json!({
                    "index": s.index_name,
                    "embedder": s.embedder,
                    "model": s.model,
                    "dim": s.dim,
                })
            })
            .collect();
        let mut entry = json!({
            "bucket": bucket,
            "counts": counts,
            "pendingCount": pending_count,
            // T12 audit (durable): furniture lines this bucket's
            // extractions removed — 静默行为变可审计行为
            "furnitureLinesRemoved": furniture_lines,
            // KB IngestionJobStatistics field names (appendix E.2/E.5):
            // process-lifetime cumulative counters (reset on restart).
            "stats": {
                "numberOfDocumentsScanned": shared.stats_of(&bucket).scanned,
                "numberOfNewDocumentsIndexed": shared.stats_of(&bucket).new_indexed,
                "numberOfModifiedDocumentsIndexed": shared.stats_of(&bucket).modified_indexed,
                "numberOfDocumentsDeleted": shared.stats_of(&bucket).deleted,
                "numberOfDocumentsFailed": shared.stats_of(&bucket).failed,
            },
            "pipelineFingerprint": fingerprint,
            "derivedIndexes": derived,
        });
        if let Some(at) = shared.last_scan_at(&bucket) {
            entry["lastScanAt"] = json!(at);
        }
        // InFlight register (additive): live per-job progress — always an
        // array (empty when nothing runs). Optional counters appear only
        // once the stage that owns them has begun; RFC3339 rides the same
        // marker::now_rfc3339 convention as every other timestamp here.
        let in_flight: Vec<serde_json::Value> = shared
            .in_flight_of(&bucket)
            .into_iter()
            .map(|j| {
                let mut o = json!({
                    "key": j.key,
                    "stage": j.stage.to_string(),
                    "stageStartedAt": j.stage_started_at,
                    "attemptStartedAt": j.attempt_started_at,
                });
                if j.children_total.is_some() {
                    o["childrenTotal"] = json!(j.children_total);
                    o["childrenDone"] = json!(j.children_done);
                }
                if j.chunks_total.is_some() {
                    o["chunksTotal"] = json!(j.chunks_total);
                    o["chunksDone"] = json!(j.chunks_done);
                    o["batchesTotal"] = json!(j.batches_total);
                    o["batchesDone"] = json!(j.batches_done);
                }
                o
            })
            .collect();
        entry["inFlight"] = json!(in_flight);
        if shared.reindex_job(&bucket).is_some() {
            // read = checkpoint: advance the cached job state from live markers
            shared.refresh_reindex(&bucket, done);
            let job = shared.reindex_job(&bucket).expect("just checked");
            entry["reindex"] = json!({
                "jobId": job.job_id,
                "status": job.status,
                "progress": { "done": job.done, "total": job.total },
            });
        }
        out.push(entry);
    }
    Ok(Json(json!({ "buckets": out })).into_response())
}

// ---------------------------------------------------------------------------
// tests (in-process wire tests: signed requests through the real gate)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{IngestCore, IngestHandle, IngestRuntime, IngestShared};
    use crate::pipeline::config::load_pipeline_config;
    use axum::Router;
    use axum::http::Request;
    use tower::ServiceExt;

    use tokio::sync::Notify;

    fn creds() -> crate::sigv4::Credentials {
        crate::sigv4::Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    fn fixture_dir() -> std::path::PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-embedder",
            "tests/fixtures/tiny-embedder",
        ] {
            let d = std::path::PathBuf::from(p);
            if d.join("model.onnx").is_file() {
                return d;
            }
        }
        panic!("tiny-embedder fixture not found under vf/tests/fixtures/");
    }

    /// The committed tiny-SIGLIP2 fixture (the siglip2 kind's own suite
    /// runs it; the M3-T4 image-search wire tests do too).
    fn siglip2_fixture_dir() -> std::path::PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-siglip2",
            "tests/fixtures/tiny-siglip2",
        ] {
            let d = std::path::PathBuf::from(p);
            if d.join("vision.onnx").is_file() {
                return d;
            }
        }
        panic!("tiny-siglip2 fixture not found under vf/tests/fixtures/");
    }

    /// tiny-embedder pipeline: txt -> 32/8 token window, scope docs/** (the
    /// same hermetic shape ingest/mod.rs tests use).
    fn tiny_config() -> crate::pipeline::config::PipelineConfig {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n",
            fixture_dir().display().to_string()
        );
        load_pipeline_config(&text).unwrap()
    }

    /// [`tiny_config`] + the lexical face ON (M3-T9): `fts = true`, default
    /// icu tokenizer; `weighted` appends the M3-T8 weighted-merge shape the
    /// hybrid-weighted test needs.
    fn tiny_fts_config(weighted: bool) -> crate::pipeline::config::PipelineConfig {
        let merge = if weighted {
            "merge = \"weighted\"\nmerge-weights = { tiny = 2.5 }\n"
        } else {
            ""
        };
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n{merge}fts = true\n",
            fixture_dir().display().to_string()
        );
        load_pipeline_config(&text).unwrap()
    }

    /// The M3-T4 image pipeline: image FILE extractor + tiny siglip2 — the
    /// in-tree twin of the `image-search` preset shape (img route only).
    fn image_config() -> crate::pipeline::config::PipelineConfig {
        let text = format!(
            "[extractor.imgs]\nkind = \"image\"\n\n\
             [embedder.clip]\nkind = \"siglip2\"\nmodel = \"tiny-siglip2\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ img = \"imgs\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"clip\"]\nscan_interval_secs = 1\n",
            siglip2_fixture_dir().display().to_string()
        );
        load_pipeline_config(&text).unwrap()
    }

    /// Full hermetic stack: router with SigV4 ON + the ingest handle wired,
    /// plus the (un-spawned) runtime the test drives deterministically.
    struct Fixture {
        router: Router,
        rt: Arc<IngestRuntime>,
        handle: Arc<IngestHandle>,
        state: Arc<AppState>,
        dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        fixture_with(tiny_config()).await
    }

    async fn fixture_with(cfg: crate::pipeline::config::PipelineConfig) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let core = IngestCore::load(&cfg, std::path::Path::new("/nonexistent-models"))
            .await
            .expect("fixture embedder loads");
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let state = Arc::new(AppState::new(
            Arc::new(crate::store::Storage::file(dir.path())),
            Some(creds()),
            "us-east-1".into(),
            "000000000000".into(),
            false,
            Some(handle.clone()),
        ));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            Arc::new(Notify::new()),
            rx,
        );
        Fixture {
            router: crate::build_router(state.as_ref().clone()),
            rt,
            handle,
            state,
            dir,
        }
    }

    /// Marker lookup through the fixture's storage (tests observe the file
    /// layer's own bookkeeping, not a wire echo).
    async fn marker_of(f: &Fixture, key: &str) -> Option<SourceMarker> {
        f.state
            .storage
            .load_source("docs", &key_hash(key))
            .await
            .unwrap()
    }

    /// Signed POST (through the real gate — proves SigV4 covers /x-vf) with
    /// arbitrary extra headers (x-vf-* metadata) and a raw-or-JSON body.
    fn signed_post(path: &str, body: Vec<u8>, extra: &[(&str, String)]) -> Request<Body> {
        let content_type = if path == "/x-vf/PutSource" {
            "application/octet-stream"
        } else {
            "application/json"
        };
        let mut headers: Vec<(String, String)> = vec![("content-type".into(), content_type.into())];
        for (k, v) in extra {
            headers.push(((*k).to_string(), v.clone()));
        }
        let ref_pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let s = crate::sigv4::signer::sign(
            "POST",
            path,
            None,
            "127.0.0.1:8787",
            &body,
            &creds(),
            "us-east-1",
            "s3vectors",
            &crate::sigv4::signer::now_amz_date(),
            &ref_pairs,
        );
        let mut b = Request::post(path).header("host", "127.0.0.1:8787");
        for &(k, v) in &ref_pairs {
            b = b.header(k, v);
        }
        b.header("authorization", &s.authorization)
            .header("x-amz-date", &s.x_amz_date)
            .body(Body::from(body))
            .unwrap()
    }

    async fn call_json(
        f: &Fixture,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        let req = signed_post(path, serde_json::to_vec(&body).unwrap(), &[]);
        send(f, req).await
    }

    /// Percent-encode a header value the way the client contract says
    /// (controls, space, '%' and all non-ASCII escaped).
    fn enc(v: &str) -> String {
        const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ').add(b'%');
        percent_encoding::utf8_percent_encode(v, SET).to_string()
    }

    async fn put_source_call(
        f: &Fixture,
        bucket: &str,
        key: &str,
        bytes: Vec<u8>,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        let req = signed_post(
            "/x-vf/PutSource",
            bytes,
            &[
                ("x-vf-bucket", bucket.to_string()),
                ("x-vf-file-name", enc(key)),
            ],
        );
        send(f, req).await
    }

    async fn send(
        f: &Fixture,
        req: Request<Body>,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        let res = f.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers: Vec<(String, String)> = res
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let bytes = axum::body::to_bytes(res.into_body(), X_VF_MAX_BODY)
            .await
            .unwrap();
        let val = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
            })
        };
        (status, headers, val)
    }

    fn errortype(headers: &[(String, String)]) -> Option<&str> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("x-amzn-errortype"))
            .map(|(_, v)| v.as_str())
    }

    async fn drive(f: &Fixture) {
        f.rt.scan_once().await;
        f.rt.drain_queue().await;
    }

    // ---------------- PutSource ----------------

    #[tokio::test]
    async fn put_source_202_seeds_marker_and_wakes_scan() {
        let f = fixture().await;
        let (st, _, b) = put_source_call(
            &f,
            "docs",
            "docs/a.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        assert_eq!(b["key"], json!("docs/a.txt"));
        assert_eq!(b["status"], json!("PENDING"));

        // marker pre-seeded PENDING (the poll contract's observable)
        let marker = marker_of(&f, "docs/a.txt").await.expect("marker seeded");
        assert_eq!(marker.status, SourceStatus::Pending);

        // deterministic convergence through the REAL scan path (wake was
        // called by the handler; the un-spawned runtime is driven by hand)
        drive(&f).await;
        let marker = marker_of(&f, "docs/a.txt").await.unwrap();
        assert_eq!(marker.status, SourceStatus::Indexed);
    }

    #[tokio::test]
    async fn put_source_large_body_rides_multipart() {
        let f = fixture().await;
        // 17 MiB > the 16 MiB threshold: exercises put_multipart + WriteMultipart
        let big = vec![b'x'; 17 * 1024 * 1024];
        let (st, _, b) = put_source_call(&f, "docs", "docs/big.txt", big.clone()).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        // the object exists WHOLE (GetSourceData round-trip below reads it
        // back byte-for-byte via the normal get path)
        let (st, _, body) = call_json(
            &f,
            "/x-vf/GetSourceData",
            json!({"bucket":"docs","key":"docs/big.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body.as_str().unwrap().len(), big.len());
    }

    #[tokio::test]
    async fn put_source_errors_are_shaped_and_double_emitted() {
        let f = fixture().await;
        // unknown bucket -> 400 ValidationException, header + body __type
        let (st, h, b) = put_source_call(&f, "nope", "docs/a.txt", b"z".to_vec()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["__type"], json!("ValidationException"));
        assert!(
            b["message"].as_str().unwrap().contains("[ingest.nope]"),
            "{b}"
        );

        // missing key header -> 400 with fieldList addressing the header
        let req = signed_post(
            "/x-vf/PutSource",
            b"z".to_vec(),
            &[("x-vf-bucket", "docs".to_string())],
        );
        let (st, _, b) = send(&f, req).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("x-vf-file-name"));

        // no signature at all -> 403 through the SAME gate the AWS face uses
        let req = Request::post("/x-vf/PutSource")
            .header("host", "127.0.0.1:8787")
            .header("content-type", "application/octet-stream")
            .body(Body::from(b"z".to_vec()))
            .unwrap();
        let (st, _, b) = send(&f, req).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(b["__type"], json!("AccessDeniedException"));

        // key outside the pipeline's claim filter -> 400 with the config pointer
        let (st, _, b) = put_source_call(&f, "docs", "notes.txt", b"z".to_vec()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("claim filter"),
            "{b}"
        );
    }

    // ---------------- GetSource / GetSourceData ----------------

    #[tokio::test]
    async fn get_source_pending_transient_then_indexed_then_404() {
        let f = fixture().await;
        put_source_call(&f, "docs", "docs/a.txt", b"hello wire world".to_vec()).await;

        // live object, no terminal state yet: the PENDING transient branch
        let (st, _, b) = call_json(
            &f,
            "/x-vf/GetSource",
            json!({"bucket":"docs","key":"docs/a.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["status"], json!("PENDING"));
        assert!(b.get("chunks").is_none(), "{b}");
        assert!(b["mime"].is_string() && b["etag"].is_string(), "{b}");
        assert!(b["pipelineFingerprint"].is_string(), "{b}");

        drive(&f).await;
        let (st, _, b) = call_json(
            &f,
            "/x-vf/GetSource",
            json!({"bucket":"docs","key":"docs/a.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["status"], json!("INDEXED"));
        assert_eq!(b["chunks"][0]["index"], json!(0));
        assert!(b["chunks"][0]["count"].as_u64().unwrap() >= 1, "{b}");
        assert_eq!(b["mime"], json!("text/plain"));
        assert_eq!(b["retriesExhausted"], json!(false));

        // gone object, no marker -> 404 NotFoundException
        let (st, h, b) = call_json(
            &f,
            "/x-vf/GetSource",
            json!({"bucket":"docs","key":"docs/never-was.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    #[tokio::test]
    async fn get_source_data_passthrough_roundtrip() {
        let f = fixture().await;
        let payload = "汉字内容 round trip \u{1F600}".as_bytes().to_vec();
        put_source_call(&f, "docs", "docs/cn.txt", payload.clone()).await;
        let req = signed_post(
            "/x-vf/GetSourceData",
            serde_json::to_vec(&json!({"bucket":"docs","key":"docs/cn.txt"})).unwrap(),
            &[],
        );
        let res = f.router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain")
        );
        let bytes = axum::body::to_bytes(res.into_body(), X_VF_MAX_BODY)
            .await
            .unwrap();
        assert_eq!(&bytes[..], &payload[..]);

        // 404 same shape as the AWS face
        let (st, h, _) = call_json(
            &f,
            "/x-vf/GetSourceData",
            json!({"bucket":"docs","key":"docs/gone.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    // ---------------- ListSources ----------------

    #[tokio::test]
    async fn list_sources_join_filter_and_pagination() {
        let f = fixture().await;
        for i in 0..3 {
            put_source_call(
                &f,
                "docs",
                &format!("docs/f{i}.txt"),
                format!("file number {i} with some content to embed").into_bytes(),
            )
            .await;
        }
        drive(&f).await;

        // page 1: maxResults=2 forces a nextToken
        let (st, _, b) = call_json(
            &f,
            "/x-vf/ListSources",
            json!({"bucket":"docs","maxResults":2}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let page1 = b["sources"].as_array().unwrap().clone();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0]["key"], json!("docs/f0.txt"), "marker-sorted");
        let token = b["nextToken"].as_str().expect("next page").to_string();

        // page 2: the remaining one, no follow-on token
        let (st, _, b) = call_json(
            &f,
            "/x-vf/ListSources",
            json!({"bucket":"docs","maxResults":2,"nextToken":token}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["sources"].as_array().unwrap().len(), 1);
        assert!(b.get("nextToken").is_none(), "{b}");

        // status filter
        let (st, _, b) = call_json(
            &f,
            "/x-vf/ListSources",
            json!({"bucket":"docs","status":"INDEXED"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["sources"].as_array().unwrap().len(), 3);
        // bad status enum -> fieldList-addressable 400
        let (st, _, b) = call_json(
            &f,
            "/x-vf/ListSources",
            json!({"bucket":"docs","status":"BOGUS"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("status"));

        // DELETING rows stay listed (DeleteSource poll contract)
        call_json(
            &f,
            "/x-vf/DeleteSource",
            json!({"bucket":"docs","key":"docs/f1.txt"}),
        )
        .await;
        let (st, _, b) = call_json(
            &f,
            "/x-vf/ListSources",
            json!({"bucket":"docs","status":"DELETING"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let rows = b["sources"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["key"], json!("docs/f1.txt"));
        assert!(rows[0].get("etag").is_none(), "live join is gone");
    }

    // ---------------- DeleteSource ----------------

    #[tokio::test]
    async fn delete_source_202_deleting_and_idempotent() {
        let f = fixture().await;
        put_source_call(&f, "docs", "docs/a.txt", b"to be deleted".to_vec()).await;
        drive(&f).await;

        let (st, _, b) = call_json(
            &f,
            "/x-vf/DeleteSource",
            json!({"bucket":"docs","key":"docs/a.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        assert_eq!(b["status"], json!("DELETING"));

        // object gone; the DELETING marker is what GetSource polls
        let (st, _, b) = call_json(
            &f,
            "/x-vf/GetSource",
            json!({"bucket":"docs","key":"docs/a.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["status"], json!("DELETING"));

        // second delete: idempotent 202 (S3 equivalence)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/DeleteSource",
            json!({"bucket":"docs","key":"docs/a.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    }

    // ---------------- Search ----------------

    #[tokio::test]
    async fn search_hit_shape_filter_and_return_text() {
        let f = fixture().await;
        put_source_call(
            &f,
            "docs",
            "docs/fox.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        put_source_call(
            &f,
            "docs",
            "docs/sea.txt",
            b"whales and dolphins swim in the deep blue ocean".to_vec(),
        )
        .await;
        drive(&f).await;

        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        let hit = &hits[0];
        // field-by-field against appendix E.2
        assert_eq!(hit["content"]["type"], json!("TEXT"), "{hit}");
        assert!(
            hit["content"]["text"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "{hit}"
        );
        assert!(hit["score"].as_f64().is_some(), "{hit}");
        assert!(
            hit["distance"].as_f64().is_some(),
            "single route carries distance: {hit}"
        );
        assert!(
            hit["index"]
                .as_str()
                .is_some_and(|s| s.starts_with("tiny-")),
            "{hit}"
        );
        assert!(hit["key"].as_str().unwrap().contains("-text-"), "{hit}");
        assert_eq!(hit["location"]["type"], json!("source"), "{hit}");
        assert!(
            hit["location"]["key"]
                .as_str()
                .is_some_and(|k| k.starts_with("docs/")),
            "{hit}"
        );
        assert!(hit["metadata"].is_object(), "{hit}");
        // determinism: score desc, tie-break key asc
        for w in hits.windows(2) {
            let ok = w[0]["score"].as_f64().unwrap() > w[1]["score"].as_f64().unwrap()
                || (w[0]["score"] == w[1]["score"]
                    && w[0]["key"].as_str().unwrap() < w[1]["key"].as_str().unwrap());
            assert!(ok, "not ordered: {hits:?}");
        }

        // filter: the filterable join key (x-vf-src-uri) narrows to one file
        let fox_hash = crate::ingest::marker::object_key_hash("docs/fox.txt");
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-uri":{"$in":[fox_hash]}}, "topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"] == json!("docs/fox.txt")),
            "{b}"
        );

        // T15: the same name now TRANSLATES on the superset face — the
        // shorthand narrows to fox exactly like the x-vf-src-uri $in above
        // (the pre-T15 400 is now the PROTOCOL face's contract, pinned in
        // `search_src_name_translation_and_two_face_rejection` below)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":"docs/fox.txt"},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"] == json!("docs/fox.txt")),
            "{b}"
        );
        assert!(b.get("note").is_none(), "hits present: no note: {b}");

        // returnText=false omits content.text (type stays)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":1,"returnText":false}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert!(
            b["hits"][0]["content"].get("text").is_none(),
            "{}",
            b["hits"][0]
        );
        assert_eq!(b["hits"][0]["content"]["type"], json!("TEXT"));

        // ---- image-query error face (M3-T4): the additive member and its
        // fail-fast refusals, all field-addressable where a field is at
        // fault ----
        // the member is an OBJECT {bytes, mime} — a bare string is a type
        // error at the body parse (same shape every malformed body takes)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":"base64"},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("invalid request body"),
            "{b}"
        );
        // exactly one modality: text AND image, and NEITHER
        for q in [
            json!({"text": "fox", "image": {"bytes": "", "mime": "image/png"}}),
            json!({}),
        ] {
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":q,"topK":1}),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
            assert!(
                b["message"]
                    .as_str()
                    .unwrap()
                    .contains("one of query.text / query.image")
                    || b["message"]
                        .as_str()
                        .unwrap()
                        .contains("query.text (a non-empty string) or query.image"),
                "{b}"
            );
        }
        // audio stays M4
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"audio":"x"},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("arrives in M4"),
            "{b}"
        );
        // garbage base64 / empty bytes / non-image bytes / mime mismatch:
        // all ValidationException with fieldList path query.image
        let png = crate::pipeline::extract::image::tests::solid_png((8, 8), [7, 7, 7]);
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        // (WIRE bytes exactly as sent, mime, the phrase the refusal carries)
        // — only the last three ride the encoder; the first must arrive as
        // literal invalid base64
        let cases: Vec<(String, &str, &str)> = vec![
            ("%%%%".into(), "image/png", "not valid base64"),
            (b64(b""), "image/png", "bytes is empty"),
            (
                b64(b"not an image at all"),
                "image/png",
                "no known image-container magic",
            ),
            (b64(&png), "image/jpeg", "sniff as image/png"),
        ];
        for (bytes, mime, why) in cases {
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"image":{"bytes": bytes, "mime": mime}},"topK":1}),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{why}: {b}");
            assert!(b["message"].as_str().unwrap().contains(why), "{why}: {b}");
            assert_eq!(b["fieldList"][0]["path"], json!("query.image"), "{why}");
        }
        // a WELL-FORMED image query against a text-only pipeline (the tiny
        // ort fixture): no route takes it — the honest aggregate names the
        // modality and the remedy
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":{"bytes": b64(&png), "mime": "image/png"}},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        let msg = b["message"].as_str().unwrap();
        assert!(msg.contains("accepts image queries"), "{msg}");
        assert!(msg.contains("siglip2"), "{msg}");

        // topK bounds
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"x"},"topK":0}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("topK"));
    }

    /// T15 src-name translation (ADR 0004), end to end through the real
    /// compile + both retrieval routes: the three translatable spellings
    /// ($eq explicit, the bare-value shorthand, $in multi-source), $and/$or
    /// recursion, the untranslatable-operator 400 (path=filter, guidance
    /// pointing at x-vf-src-uri), and the TWO-FACE pin — the very same
    /// filter on the protocol face (QueryVectors) stays a non-filterable
    /// 400. Plus the rewrite layer's own guards (name+uri collision,
    /// non-string values) and the empty-hit note both ways.
    #[tokio::test]
    async fn search_src_name_translation_and_two_face_rejection() {
        let f = fixture().await;
        put_source_call(
            &f,
            "docs",
            "docs/fox.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        put_source_call(
            &f,
            "docs",
            "docs/sea.txt",
            b"whales and dolphins swim in the deep blue ocean".to_vec(),
        )
        .await;
        drive(&f).await;

        // ---- $eq (explicit operator): narrows to fox, exactly what the
        // equivalent x-vf-src-uri filter does ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":{"$eq":"docs/sea.txt"}},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"] == json!("docs/sea.txt")),
            "{b}"
        );

        // ---- $in multi-source: BOTH files, and only those two ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":{"$in":["docs/fox.txt","docs/sea.txt"]}},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let mut seen: Vec<String> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["location"]["key"].as_str().map(String::from))
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen, vec!["docs/fox.txt", "docs/sea.txt"], "{b}");

        // ---- recursion: a name nested under $and AND under $or translates
        // (the rewrite drills the boolean operators; the second $and leg is
        // a trivially-true exists on a key every chunk carries) ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"$and":[
                       {"$or":[{"x-vf-src-name":{"$eq":"docs/fox.txt"}}]},
                       {"x-vf-modality":{"$exists":true}}
                   ]},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert!(
            b["hits"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["location"]["key"] == json!("docs/fox.txt")),
            "{b}"
        );

        // ---- untranslatable operators: 400, path=filter, guidance naming
        // the escape hatch (each on its own, and mixed into $eq's object) ----
        for filter in [
            json!({"x-vf-src-name":{"$ne":"docs/fox.txt"}}),
            json!({"x-vf-src-name":{"$startsWith":"docs/"}}),
            json!({"x-vf-src-name":{"$eq":"docs/fox.txt","$ne":"docs/sea.txt"}}),
        ] {
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"text":"the"},"filter":filter,"topK":1}),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
            assert_eq!(b["fieldList"][0]["path"], json!("filter"), "{b}");
            let msg = b["message"].as_str().unwrap();
            assert!(
                msg.contains("does not support x-vf-src-name") && msg.contains("use x-vf-src-uri"),
                "{msg}"
            );
        }

        // ---- the rewrite layer's own guards ----
        // name + uri in one object would silently collapse: loud 400
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":"docs/fox.txt",
                             "x-vf-src-uri":"0123456789abcdef"},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("cannot appear in the same filter"),
            "{b}"
        );
        // a non-string name compiles into a silent no-match; fail loud
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":7},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        let msg = b["message"].as_str().unwrap();
        assert!(msg.contains("must be a string"), "{msg}");
        // ...and inside an operator the value-level wording carries
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":{"$in":["docs/a.txt", 7]}},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("must be strings"),
            "{b}"
        );

        // ---- empty-hit note: PRESENT when a translated filter yields zero
        // hits (a typo'd name hashes to a value nothing carries — 打错字 ≠
        // 没搜到), ABSENT for a zero-hit query with NO src-name filter ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":"docs/typo-misspelled.txt"},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["hits"].as_array().unwrap().len(), 0, "{b}");
        let note = b["note"].as_str().expect("the note rides the response");
        assert!(note.contains("matched no indexed sources"), "{note}");
        // no translation active: zero hits stay a bare empty list
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-uri":"0123456789abcdef"},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["hits"].as_array().unwrap().len(), 0, "{b}");
        assert!(b.get("note").is_none(), "no src-name filter: no note: {b}");

        // ---- the TWO-FACE pin: the identical filter on the PROTOCOL face
        // (QueryVectors) stays the non-filterable 400 — the translation is
        // the superset face's alone (ADR 0004) ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let index = b["hits"][0]["index"].as_str().unwrap().to_string();
        let meta = f
            .state
            .storage
            .load_index("docs", &index)
            .await
            .unwrap()
            .expect("derived index exists");
        let qvec = vec![0.1f64; meta.dimension as usize];
        let (st, headers, b) = call_json(
            &f,
            "/QueryVectors",
            json!({"vectorBucketName":"docs","indexName":index,
                   "topK":1,"queryVector":{"float32":qvec},
                   "filter":{"x-vf-src-name":"docs/fox.txt"}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&headers), Some("ValidationException"), "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("non-filterable"),
            "{b}"
        );
    }

    /// T15 constraints-cap name echo: a src-name `$in` busting the
    /// 100-constraint limit (filter.rs MAX_CONSTRAINTS — translated or
    /// not, each element counts one) fails with a message that points at
    /// the NAMES the user wrote, not at hashes they never typed.
    #[tokio::test]
    async fn search_src_name_constraints_cap_echoes_names() {
        let f = fixture().await;
        put_source_call(
            &f,
            "docs",
            "docs/a.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        drive(&f).await;

        let names: Vec<String> = (0..150).map(|i| format!("docs/src-{i:03}.txt")).collect();
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"the"},
                   "filter":{"x-vf-src-name":{"$in":names}},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("filter"), "{b}");
        let msg = b["message"].as_str().unwrap();
        // the generic cap accounting, the src-name attribution, and the
        // sample of the USER'S spellings (never a hash)
        assert!(msg.contains("constraints"), "{msg}");
        assert!(msg.contains("x-vf-src-name filter contributes"), "{msg}");
        assert!(msg.contains("\"docs/src-000.txt\""), "{msg}");
        assert!(
            !msg.contains(&crate::ingest::marker::object_key_hash("docs/src-000.txt")),
            "the echo must speak names, not hashes: {msg}"
        );
    }

    /// T13 LINE coordinates end to end on a pageless source (the `nl` 对拍
    /// spirit): a known-lines txt is ingested through the real worker, and
    /// every hit's `location.coord.line` (a) matches its stored metadata
    /// keys, (b) numbers within the file's real line count, and (c) the
    /// window holding line 3's unique phrase reports an interval CONTAINING
    /// 3 — what `nl` would number it. Page/section members stay absent
    /// (D-7: one addressing unit per source).
    #[tokio::test]
    async fn search_coord_line_summary_txt() {
        let f = fixture().await;
        // five known lines; the needle phrase rides line 3
        let text = "line one alpha words\n\
                    line two beta words follow here\n\
                    line three gamma carries the needle phrase\n\
                    line four delta words continue on\n\
                    line five epsilon the closing words\n";
        put_source_call(&f, "docs", "docs/lines.txt", text.as_bytes().to_vec()).await;
        drive(&f).await;

        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"words"},"topK":50}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(hits.len() >= 2, "32/8 windows over 5 lines: {b}");
        for h in hits {
            let coord = &h["location"]["coord"];
            let (ls, le) = (
                coord["line"]["start"].as_u64().expect("line.start"),
                coord["line"]["end"].as_u64().expect("line.end"),
            );
            // (a) the summary mirrors the stored metadata verbatim
            let m = &h["metadata"];
            assert_eq!(m["x-vf-line-start"].as_u64(), Some(ls), "{h}");
            assert_eq!(m["x-vf-line-end"].as_u64(), Some(le), "{h}");
            // (b) within the file's 5 lines — the end may exceed the last
            // content line by ONE: a window ending in '\n' honestly
            // includes the line the break OPENS (the pinned end
            // semantics; the window "…closing words\n" reports (5,6))
            assert!(1 <= ls && ls <= le && le <= 6, "({ls},{le}): {h}");
            // (d) the D-7 fork: no page, no section member at all
            assert!(coord.get("page").is_none(), "{h}");
            assert!(coord.get("section").is_none(), "{h}");
            assert!(m.get("x-vf-page-start").is_none(), "{h}");
        }
        // (c) the nl 对拍 witness: the needle window covers line 3
        let holder = hits
            .iter()
            .find(|h| {
                h["content"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("needle"))
            })
            .expect("some hit covers line 3");
        let (ls, le) = (
            holder["location"]["coord"]["line"]["start"]
                .as_u64()
                .unwrap(),
            holder["location"]["coord"]["line"]["end"].as_u64().unwrap(),
        );
        assert!(
            ls <= 3 && 3 <= le,
            "({ls},{le}) must contain line 3: {holder}"
        );
    }

    /// The T18 container pipeline: a cfg(test) scripted container (the
    /// ingest suite's discipline) whose children CARRY TITLES, beside a
    /// flat txt source — the section axis and its title companion, end to
    /// end through the real worker.
    async fn titled_container_fixture() -> Fixture {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [extractor.ctn]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\", ctn = \"ctn\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n",
            fixture_dir().display().to_string()
        );
        let cfg = crate::pipeline::config::load_pipeline_config(&text).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut core = IngestCore::load(&cfg, std::path::Path::new("/nonexistent-models"))
            .await
            .expect("fixture embedder loads");
        // the placeholder extractor becomes the scripted container (the
        // ingest suite's runtime_patched shape, through BucketPlan's
        // cfg(test) injection hook — the plan's own fields stay private)
        use crate::pipeline::ChildDoc;
        use crate::pipeline::extract::ExtractorKind;
        const PAYLOAD: &[u8] = b"TITLED-BOOK";
        core.buckets
            .get_mut("docs")
            .expect("docs bucket")
            .set_extractor(
                "ctn",
                ExtractorKind::TestContainer(crate::pipeline::extract::TestContainerSpec::one(
                    "application/octet-stream",
                    PAYLOAD,
                    vec![
                        ChildDoc::new_titled(
                            "ch1.xhtml",
                            "text/plain",
                            "first chapter body words alpha beta gamma",
                            "第一章　灰雾渡口",
                        ),
                        ChildDoc::new_titled(
                            "ch2.xhtml",
                            "text/plain",
                            "second chapter body words delta epsilon zeta",
                            "第二章　魔药铺",
                        ),
                        ChildDoc::new(
                            "ch3.xhtml",
                            "text/plain",
                            "third chapter body words eta theta iota",
                        ),
                    ],
                )),
            );
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let state = Arc::new(AppState::new(
            Arc::new(crate::store::Storage::file(dir.path())),
            Some(creds()),
            "us-east-1".into(),
            "000000000000".into(),
            false,
            Some(handle.clone()),
        ));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            Arc::new(Notify::new()),
            rx,
        );
        Fixture {
            router: crate::build_router(state.as_ref().clone()),
            rt,
            handle,
            state,
            dir,
        }
    }

    /// T18 R8 structure axis, end to end (D-9.4 + D-10.4): a titled
    /// container + a flat txt share one index —
    /// * `x-vf-section-title` is FILTERABLE ($eq/$startsWith, the mirror
    ///   column's automatic evolution — F-7, no registration);
    /// * `orderBy: section-asc|section-desc` re-orders the final view by
    ///   (section, line, score desc, key asc) with MISSING sections
    ///   NULLS-LAST (the flat source's hits), scores retained;
    /// * an unknown orderBy is a 400 at path=orderBy (the searchMode
    ///   discipline).
    #[tokio::test]
    async fn search_section_axis_title_filters_and_order_by() {
        let f = titled_container_fixture().await;
        const PAYLOAD: &[u8] = b"TITLED-BOOK";
        put_source_call(&f, "docs", "docs/book.ctn", PAYLOAD.to_vec()).await;
        put_source_call(
            &f,
            "docs",
            "docs/flat.txt",
            b"flat source body words kappa lambda mu".to_vec(),
        )
        .await;
        drive(&f).await;

        // ---- D-9.4: the title companion is filterable ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"words"},"topK":50,
                   "filter":{"x-vf-section-title":{"$eq":"第二章　魔药铺"}}}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "the titled section matches: {b}");
        for h in hits {
            assert_eq!(
                h["metadata"]["x-vf-section"],
                json!(2),
                "$eq on the title narrows to section 2: {h}"
            );
        }
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"words"},"topK":50,
                   "filter":{"x-vf-section-title":{"$startsWith":"第"}}}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        {
            let hits = b["hits"].as_array().unwrap();
            assert!(
                !hits.is_empty(),
                "$startsWith 第 matches the titled sections: {b}"
            );
            for h in hits {
                // the two titled sections (1, 2) — never the untitled 3,
                // never the flat source's key-absent rows
                let sec = h["metadata"]["x-vf-section"].as_u64();
                assert!(
                    sec == Some(1) || sec == Some(2),
                    "$startsWith keeps only the titled rows: {h}"
                );
            }
        }

        // ---- D-10.4: orderBy section-asc — container sections ascending,
        // the flat (section-less) source NULLS-LAST, scores retained ----
        let sections_of = |b: &serde_json::Value| -> Vec<Option<u64>> {
            b["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["metadata"]["x-vf-section"].as_u64())
                .collect()
        };
        let (st, _, asc) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"words"},"topK":50,"orderBy":"section-asc"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{asc}");
        let secs = sections_of(&asc);
        assert!(secs.len() >= 4, "container + flat hits: {secs:?}");
        {
            // nulls last: every None trails every Some
            let first_none = secs.iter().position(|s| s.is_none());
            let last_some = secs.iter().rposition(|s| s.is_some());
            assert!(
                first_none.is_some_and(|n| last_some.is_some_and(|s| n > s)),
                "nulls-last: {secs:?}"
            );
            // and the Some prefix is ascending
            let somes: Vec<u64> = secs.iter().flatten().copied().collect();
            let mut sorted = somes.clone();
            sorted.sort_unstable();
            assert_eq!(somes, sorted, "section-asc: {secs:?}");
            // scores retained
            for h in asc["hits"].as_array().unwrap() {
                assert!(h["score"].as_f64().is_some(), "{h}");
            }
        }
        let (st, _, desc) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"words"},"topK":50,"orderBy":"section-desc"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{desc}");
        let secs = sections_of(&desc);
        {
            let somes: Vec<u64> = secs.iter().flatten().copied().collect();
            let mut sorted = somes.clone();
            sorted.sort_unstable();
            sorted.reverse();
            assert_eq!(
                somes, sorted,
                "section-desc flips the section leg: {secs:?}"
            );
            let first_none = secs.iter().position(|s| s.is_none());
            let last_some = secs.iter().rposition(|s| s.is_some());
            assert!(
                first_none.is_some_and(|n| last_some.is_some_and(|s| n > s)),
                "nulls-last holds under desc too: {secs:?}"
            );
        }

        // ---- the closed vocabulary: unknown value dies at path=orderBy ----
        let (st, h, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"words"},"topK":5,"orderBy":"chapter-asc"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("orderBy"), "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown orderBy"),
            "{b}"
        );
        let _ = h;
    }

    /// The pdf+txt pipeline for the PAGE-coordinate e2e (the real pdfium
    /// `.so` through the whole worker — the same dependency the golden
    /// corpus already makes mandatory for this suite).
    fn pdf_txt_config() -> crate::pipeline::config::PipelineConfig {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [extractor.pdfs]\nkind = \"pdf-text\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\", pdf = \"pdfs\" }}\n\
             chunk = {{ kind = \"token-window\", size = 64, overlap = 16 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n",
            fixture_dir().display().to_string()
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// T13 PAGE coordinates end to end (pdfium, real worker) + the range
    /// and existence FILTERS over them: a 5-page PDF and a txt share one
    /// index; the pdf hits carry page intervals (no line keys — D-7), the
    /// txt hits line intervals; `$gte/$lte` narrows to a page window and
    /// out-of-range is ZERO hits; `$exists` separates the two formats.
    #[tokio::test]
    async fn search_coord_page_range_and_exists_filters() {
        let f = fixture_with(pdf_txt_config()).await;
        // 5 pages, each unique (no furniture repetition, no TOC shape —
        // no trailing digit columns), each long enough to straddle windows
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)")
        };
        let pages: Vec<Vec<String>> = (0..5)
            .map(|i| {
                vec![
                    esc(&format!(
                        "coord header unique words for page {i} of the set"
                    )),
                    esc(&format!(
                        "body prose {i} lorem ipsum dolor sit amet consectetur adipiscing"
                    )),
                    esc(&format!(
                        "closing prose {i} quis nostrud exercitation ullamco"
                    )),
                ]
            })
            .collect();
        let pdf_bytes = crate::pipeline::extract::pdf_inspector::tests::build_t12_pdf(pages);
        put_source_call(&f, "docs", "docs/paper.pdf", pdf_bytes).await;
        put_source_call(
            &f,
            "docs",
            "docs/plain.txt",
            b"plain prose words in the text file companion\n".to_vec(),
        )
        .await;
        drive(&f).await;
        // both sources landed
        let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::OK, "{b}");

        let search = |filter: serde_json::Value| {
            let f = &f;
            async move {
                let (st, _, b) = call_json(
                    f,
                    "/x-vf/Search",
                    json!({"bucket":"docs","query":{"text":"prose"},"topK":100,"filter":filter}),
                )
                .await;
                assert_eq!(st, StatusCode::OK, "{b}");
                b
            }
        };

        // ---- unfiltered: both formats, each with ITS unit (D-7) ----
        let b = search(json!({})).await;
        let hits = b["hits"].as_array().unwrap();
        assert!(hits.len() >= 3, "{b}");
        let mut pdf_pages_seen = std::collections::BTreeSet::new();
        for h in hits {
            let key = h["location"]["key"].as_str().unwrap();
            let coord = &h["location"]["coord"];
            let m = &h["metadata"];
            if key.ends_with(".pdf") {
                let (ps, pe) = (
                    coord["page"]["start"].as_u64().expect("pdf page.start"),
                    coord["page"]["end"].as_u64().expect("pdf page.end"),
                );
                assert_eq!(m["x-vf-page-start"].as_u64(), Some(ps), "{h}");
                assert_eq!(m["x-vf-page-end"].as_u64(), Some(pe), "{h}");
                assert!(1 <= ps && ps <= pe && pe <= 5, "({ps},{pe}): {h}");
                assert!(
                    coord.get("line").is_none(),
                    "pdf carries no line keys (D-7): {h}"
                );
                assert!(m.get("x-vf-line-start").is_none(), "{h}");
                pdf_pages_seen.insert(ps);
            } else {
                assert!(
                    coord["line"]["start"].as_u64().is_some(),
                    "txt carries line keys: {h}"
                );
                assert!(coord.get("page").is_none(), "{h}");
            }
        }
        assert!(
            pdf_pages_seen.len() >= 3,
            "windows must land on several pages: {pdf_pages_seen:?} in {b}"
        );

        // ---- $gte/$lte page window: only chunks OPENING on pages 3..4 ----
        let b = search(json!({"x-vf-page-start": {"$gte": 3, "$lte": 4}})).await;
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        for h in hits {
            assert!(
                h["location"]["key"].as_str().unwrap().ends_with(".pdf"),
                "{h}"
            );
            let ps = h["location"]["coord"]["page"]["start"].as_u64().unwrap();
            assert!((3..=4).contains(&ps), "{h}");
        }
        // out-of-range: ZERO hits, not an error, not a silent all
        let b = search(json!({"x-vf-page-start": {"$gte": 99}})).await;
        assert_eq!(b["hits"].as_array().unwrap().len(), 0, "{b}");

        // ---- $exists on the page key: true for THIS corpus's pdf rows
        // (pdf-text multi-page parts) and false for txt — but per the
        // 2026-11-09 裁决 3 weakening this is a corpus fact, NOT a format
        // law: an OCR'd (pdf-ocr) single-page part carries page keys too,
        // so page-key presence no longer uniquely discriminates multi-page
        // PDF extraction — format questions belong to modality semantics
        // (x-vf-modality / the source mime), see chunk::windowing::Coords
        let b = search(json!({"x-vf-page-start": {"$exists": true}})).await;
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"].as_str().unwrap().ends_with(".pdf")),
            "{b}"
        );
        let b = search(json!({"x-vf-page-start": {"$exists": false}})).await;
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"].as_str().unwrap().ends_with(".txt")),
            "{b}"
        );
    }

    /// T12 kind governance over the wire (D-5 + the protocol m3/m5
    /// duties): three populations share one index — a stamped `body` row,
    /// a stamped `toc` row, and a LEGACY row with NO kind key (the pre-T12
    /// population) — and the faces must treat them as:
    ///
    /// * **Search default**: toc hidden, body AND the legacy NULL row
    ///   visible — the negative-form predicate is the whole point (a bare
    ///   `!=` would let SQL three-valued logic drop every null row);
    /// * **includeKinds**: the superset放行 — the navigation query
    ///   (「哪章在哪页」) reaches the toc chunk;
    /// * **unknown kinds**: field-addressable ValidationException (m5),
    ///   never a silent no-match;
    /// * **QueryVectors (protocol)**: NO implicit exclusion — unfiltered
    ///   returns the toc row, and the kind key filters explicitly.
    #[tokio::test]
    async fn search_kind_governance_null_rows_and_include_kinds() {
        // fts ON (superset of the plain fixture): the kind exclusion must
        // hold on the LEXIAL arm too — one injected filter, every route
        let f = fixture_with(tiny_fts_config(false)).await;
        // one real ingest so the derived index exists (dim + mirror
        // columns materialize through the normal path)
        put_source_call(
            &f,
            "docs",
            "docs/fox.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        drive(&f).await;
        let (st, _, status) =
            call_json(&f, "/x-vf/GetIngestStatus", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::OK, "{status}");
        let ix = &status["buckets"][0]["derivedIndexes"][0];
        let index = ix["index"].as_str().unwrap().to_string();
        let dim = ix["dim"].as_u64().unwrap() as usize;

        // the three populations, written through the PROTOCOL face (the
        // exact rows a mixed pre/post-T12 index holds)
        // every row carries chunk TEXT with the query term so the LEXICAL
        // arm can match them too (the kind governance must hold on both
        // routes; a textless row is invisible to BM25 by construction)
        let row = |key: &str, extra: serde_json::Value| {
            let mut meta = json!({"x-vf-chunk-text": "the fox governance fixture row"});
            if let (serde_json::Value::Object(dst), serde_json::Value::Object(src)) =
                (&mut meta, extra)
            {
                for (k, v) in src {
                    dst.insert(k, v);
                }
            }
            json!({
                "key": key,
                "data": { "float32": vec![0.5f32; dim] },
                "metadata": meta,
            })
        };
        let body = serde_json::json!({
            "vectorBucketName": "docs",
            "indexName": index,
            "vectors": [
                row("kind-body-row", json!({"x-vf-chunk-kind": "body"})),
                row("kind-toc-row", json!({"x-vf-chunk-kind": "toc"})),
                row("legacy-null-row", json!({"legacy": true})),
            ],
        });
        let resp = crate::ops::put_vectors(
            State(f.state.as_ref().clone()),
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await
        .expect("put rows");
        let _ = resp;

        let keys_of = |b: &serde_json::Value| -> Vec<String> {
            b["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["key"].as_str().unwrap_or("").to_string())
                .collect()
        };

        // ---- Search default: toc hidden, body + the LEGACY NULL row
        // visible (m3: 存量 null 行默认可见) ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":50}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let keys = keys_of(&b);
        assert!(
            keys.contains(&"legacy-null-row".to_string()),
            "the pre-T12 null-kind row must stay visible: {keys:?}"
        );
        assert!(keys.contains(&"kind-body-row".to_string()), "{keys:?}");
        assert!(
            !keys.contains(&"kind-toc-row".to_string()),
            "toc is excluded from the default result set: {keys:?}"
        );
        // attribution stays visible on the admitted rows (kind in metadata)
        assert!(
            b["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["key"] == json!("kind-body-row")
                    && h["metadata"]["x-vf-chunk-kind"] == json!("body")),
            "{b}"
        );

        // ---- the LEXICAL arm carries the same exclusion (the injection
        // composes into the ONE filter both routes run): keyword mode ----
        for mode in ["keyword", "hybrid"] {
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"text":"fox"},"topK":50,
                       "searchMode": mode}),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{mode}: {b}");
            let keys = keys_of(&b);
            assert!(
                keys.contains(&"legacy-null-row".to_string()),
                "{mode}: null row visible: {keys:?}"
            );
            assert!(
                !keys.contains(&"kind-toc-row".to_string()),
                "{mode}: toc excluded: {keys:?}"
            );
            // ...and includeKinds放行s on the lexical arm as well
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"text":"fox"},"topK":50,
                       "searchMode": mode, "includeKinds":["toc"]}),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{mode}: {b}");
            assert!(
                keys_of(&b).contains(&"kind-toc-row".to_string()),
                "{mode}: includeKinds admits toc: {:?}",
                keys_of(&b)
            );
        }

        // ---- includeKinds放行: the navigation query reaches the toc chunk ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":50,
                   "includeKinds":["toc"]}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let keys = keys_of(&b);
        assert!(
            keys.contains(&"kind-toc-row".to_string()),
            "includeKinds admits the toc row: {keys:?}"
        );
        assert!(keys.contains(&"legacy-null-row".to_string()), "{keys:?}");
        // admitting BOTH excluded kinds empties the injection entirely (the
        // user filter path stands alone, byte-identical)
        let (st, _, b2) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":50,
                   "includeKinds":["toc","legend"]}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b2}");
        assert_eq!(
            keys_of(&b2),
            keys,
            "both admitted == toc-only admitted here"
        );
        // the kind exclusion composes WITH a user filter (AND semantics)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":50,
                   "filter":{"legacy":{"$eq":true}}}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let keys = keys_of(&b);
        assert_eq!(keys, vec!["legacy-null-row".to_string()], "{keys:?}");

        // ---- unknown kinds: field-addressable 400 (m5), not a silent
        // no-match ----
        let (st, h, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5,
                   "includeKinds":["header"]}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("unknown includeKinds entry"),
            "{b}"
        );
        assert_eq!(b["fieldList"][0]["path"], json!("includeKinds"), "{b}");

        // ---- protocol face (QueryVectors): NO implicit exclusion — the
        // toc row is unfiltered-visible AND explicitly filterable ----
        let qbody = serde_json::json!({
            "vectorBucketName": "docs",
            "indexName": index,
            "queryVector": {"float32": vec![0.5f64; dim]},
            "topK": 100,
            "returnMetadata": true,
        });
        let resp = crate::ops::query_vectors(
            State(f.state.as_ref().clone()),
            Bytes::from(serde_json::to_vec(&qbody).unwrap()),
        )
        .await
        .expect("query ok");
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let qkeys: Vec<&str> = v["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["key"].as_str().unwrap())
            .collect();
        assert!(
            qkeys.contains(&"kind-toc-row"),
            "QueryVectors never implicitly excludes: {qkeys:?}"
        );
        // ...and the kind key is a first-class filterable on the protocol
        // face (explicit user behavior)
        let mut filtered = qbody.clone();
        filtered["filter"] = json!({"x-vf-chunk-kind": {"$eq": "toc"}});
        let resp = crate::ops::query_vectors(
            State(f.state.as_ref().clone()),
            Bytes::from(serde_json::to_vec(&filtered).unwrap()),
        )
        .await
        .expect("query ok");
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let got: Vec<&str> = v["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["key"].as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["kind-toc-row"], "explicit kind filter: {got:?}");
    }

    /// tiny-embedder pipeline with the PDF route (structure detection ON —
    /// the T12 adjudicated default): the extractor's furniture tally must
    /// flow to the durable marker, the per-source wire row, and the
    /// per-bucket GetIngestStatus aggregate (slice 2's audit channel
    /// consumed).
    #[tokio::test]
    async fn ingest_furniture_tally_flows_to_marker_and_status() {
        let text = format!(
            "[extractor.pdf]\nkind = \"pdf-text\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ pdf = \"pdf\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n",
            fixture_dir().display().to_string()
        );
        let f = fixture_with(load_pipeline_config(&text).unwrap()).await;
        // 4 pages, every page head the same strap: the furniture gate
        // (100% of pages, substance gates pass) removes 4 lines
        let pages: Vec<Vec<String>> = (0..4usize)
            .map(|i| {
                vec![
                    "copyright 2026 vectorfs radar".to_string(),
                    format!("unique prose {i} for the page"),
                    format!("second prose {i} line here"),
                ]
            })
            .collect();
        let pdf_bytes = crate::pipeline::extract::pdf_inspector::tests::build_t12_pdf(pages);
        put_source_call(&f, "docs", "docs/radar.pdf", pdf_bytes).await;
        drive(&f).await;
        let m = marker_of(&f, "docs/radar.pdf").await.expect("marker");
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
        assert_eq!(m.furniture_removed, Some(4), "the strap left every page");

        // per-source wire row (ListSources / vf ls's FURN column source)
        let (st, _, b) = call_json(&f, "/x-vf/ListSources", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["sources"][0]["furnitureRemoved"], json!(4), "{b}");

        // per-bucket durable aggregate (GetIngestStatus / vf status)
        let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["buckets"][0]["furnitureLinesRemoved"], json!(4), "{b}");
    }

    /// InFlight register over the WIRE (2026-11-11 摄取进度可见性): with the
    /// worker parked at its first put (the fixture holds the commit lock —
    /// the stage flips just before the acquire, so the state is stable), a
    /// >1000-chunk source's GetIngestStatus entry carries `inFlight` with
    /// the additive field set — stage/chunks per sub-batch, RFC3339 clocks,
    /// children absent for a plain txt — and the array is EMPTY at the
    /// terminal state.
    #[tokio::test]
    async fn get_ingest_status_reports_inflight_progress_and_clears() {
        let f = fixture().await;
        put_source_call(&f, "docs", "docs/big.txt", b"word ".repeat(7000)).await;

        f.rt.scan_once().await; // enqueue; the drain runs parked below
        let guard = f.handle.shared().write_lock().lock().await;
        let drain = tokio::spawn({
            let rt = f.rt.clone();
            async move { rt.drain_queue().await }
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        let entry = loop {
            let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({"bucket":"docs"})).await;
            assert_eq!(st, StatusCode::OK, "{b}");
            let j = &b["buckets"][0]["inFlight"];
            if j.as_array().is_some_and(|a| a.len() == 1)
                && j[0]["stage"] == json!("writing")
                && j[0]["chunksDone"] == json!(500)
            {
                break j[0].clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the worker never pinned at put #1: {b}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        };
        assert_eq!(entry["key"], json!("docs/big.txt"));
        assert_eq!(
            entry["batchesDone"],
            json!(0),
            "put #1 cannot run under our lock"
        );
        let total = entry["chunksTotal"].as_u64().unwrap();
        assert!(total > 1000, "three sub-batches: {total}");
        assert_eq!(entry["batchesTotal"], json!(total.div_ceil(500)));
        // RFC3339 clocks (the ops epoch formatter's fixed ...Z shape)
        for k in ["stageStartedAt", "attemptStartedAt"] {
            let s = entry[k].as_str().unwrap();
            assert_eq!(s.len(), 24, "{k}: {s}");
            assert!(s.ends_with('Z') && s.contains('T'), "{k}: {s}");
        }
        assert!(
            entry.get("childrenTotal").is_none(),
            "a plain txt carries no children keys"
        );
        drop(guard);
        drain.await.unwrap();

        // terminal: the register is empty on the wire (the array member
        // itself stays — always-present additive field)
        let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["buckets"][0]["inFlight"], json!([]), "{b}");
        let m = marker_of(&f, "docs/big.txt").await.unwrap();
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
    }

    /// M3-T4 以图搜图 over the wire, in-process: the image FILE extractor +
    /// tiny-siglip2 pipeline — put two PNG sources, then (a) an image query
    /// of the SAME bytes hits its own source first (回源 + IMAGE content
    /// type), (b) a TEXT query reaches the IMAGE index (文字搜图 — siglip2's
    /// query side declares [Image, Text], the fan-out must actually happen).
    #[tokio::test]
    async fn search_image_query_round_trip_and_text_to_image() {
        let f = fixture_with(image_config()).await;
        let red = crate::pipeline::extract::image::tests::solid_png((8, 8), [255, 0, 0]);
        let blue = crate::pipeline::extract::image::tests::solid_png((8, 8), [0, 0, 255]);
        put_source_call(&f, "docs", "docs/red.png", red.clone()).await;
        put_source_call(&f, "docs", "docs/blue.png", blue.clone()).await;
        drive(&f).await;
        for key in ["docs/red.png", "docs/blue.png"] {
            let m = marker_of(&f, key).await.unwrap();
            assert_eq!(m.status, SourceStatus::Indexed, "{key}");
            assert_eq!(m.chunks.as_ref().unwrap()["image"], 1, "{key}");
        }

        let b64 = |b: &[u8]| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b)
        };

        // (a) image query with red's exact bytes: rank 1 is red itself —
        // same bytes, same preprocessing, distance ~0 on the single route
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":{"bytes": b64(&red), "mime": "image/png"}},"topK":2}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "both sources rank: {b}");
        let top = &hits[0];
        assert_eq!(top["location"]["key"], json!("docs/red.png"), "{b}");
        assert_eq!(top["content"]["type"], json!("IMAGE"), "{b}");
        assert!(
            top["content"].get("text").is_none(),
            "image chunks carry no text channel"
        );
        assert!(top["key"].as_str().unwrap().contains("-image-"), "{b}");
        assert!(top["index"].as_str().unwrap().starts_with("clip-"), "{b}");
        assert_eq!(top["metadata"]["x-vf-modality"], json!("image"), "{b}");
        assert!(
            top["score"].as_f64().unwrap() > 0.99,
            "identical bytes must land distance ~0 (single-route passthrough): {b}"
        );
        // the other source ranks second — attribution + merge stay sane
        assert_eq!(hits[1]["location"]["key"], json!("docs/blue.png"), "{b}");

        // (b) 文字搜图: a TEXT query fans out onto the IMAGE index (siglip2
        // declares Text on its query side) and returns image chunks — the
        // dual-tower default route, asserted REAL, not just declared
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":2}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(
            !hits.is_empty(),
            "the text query must reach the image index: {b}"
        );
        for h in hits {
            assert_eq!(h["content"]["type"], json!("IMAGE"), "{b}");
            assert_eq!(h["metadata"]["x-vf-modality"], json!("image"), "{b}");
        }
    }

    // -----------------------------------------------------------------------
    // b1 (2026-11-07 §0 裁决): the openai kind's multimodal messages dialect
    // over a fake vLLM-dialect endpoint — content-seeded vectors make
    // identity observable (same content in ⇒ same vector out, doc and query
    // sides alike, because both carry the same default system instruction).
    // -----------------------------------------------------------------------

    /// The unified-embedder pipeline: txt + image extractors, ONE
    /// `[embedder.unified] kind = "openai" multimodal = true` pointed at a
    /// seeded fake dialect server.
    async fn unified_config() -> (
        crate::pipeline::config::PipelineConfig,
        crate::pipeline::embed::openai::tests::Fake,
    ) {
        let (base, fake) = crate::pipeline::embed::openai::tests::fake_openai_with(Some(8)).await;
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [extractor.imgs]\nkind = \"image\"\n\n\
             [embedder.unified]\nkind = \"openai\"\nendpoint = \"{base}\"\n\
             model = \"fake-qwen3vl\"\ndim = 8\napi_key = \"test-key\"\n\
             multimodal = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\n\
             extract = {{ txt = \"plain\", img = \"imgs\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"unified\"]\nscan_interval_secs = 1\n",
        );
        (load_pipeline_config(&text).unwrap(), fake)
    }

    /// b1 e2e #1 — txt+jpg mixed source → ONE unified embedder → ONE index:
    /// both modalities' chunks land in the SAME derived index (x-vf-modality
    /// each its own), the ingest wire is the messages dialect end to end
    /// (image chunk = data:image/jpeg data URL, text chunk = text part), and
    /// BOTH query modalities hit that one index — a text query returns TEXT
    /// and IMAGE rows from it, an image query round-trips to its own source.
    #[tokio::test]
    async fn unified_multimodal_openai_txt_jpg_single_index() {
        let (cfg, fake) = unified_config().await;
        let f = fixture_with(cfg).await;
        let fox_text = "the quick brown fox";
        let red = crate::pipeline::extract::image::tests::solid_jpeg((16, 12), [255, 0, 0]);
        put_source_call(&f, "docs", "docs/fox.txt", fox_text.as_bytes().to_vec()).await;
        put_source_call(&f, "docs", "docs/red.jpg", red.clone()).await;
        drive(&f).await;
        for (key, modality) in [("docs/fox.txt", "text"), ("docs/red.jpg", "image")] {
            let m = marker_of(&f, key).await.unwrap();
            assert_eq!(m.status, SourceStatus::Indexed, "{key}: {m:?}");
            assert_eq!(m.chunks.as_ref().unwrap()[modality], 1, "{key}");
        }
        // ingest wire: two requests, BOTH messages dialect, one per chunk
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "one request per chunk: {seen:?}");
            for r in seen.iter() {
                assert!(r.body.get("input").is_none(), "dialect only: {}", r.body);
                assert!(r.body["messages"].is_array(), "{}", r.body);
            }
            let text_req = &seen[0].body;
            assert_eq!(
                text_req["messages"][1]["content"],
                json!([{ "type": "text", "text": fox_text }])
            );
            let img_req = &seen[1].body;
            let url = img_req["messages"][1]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap();
            assert!(
                url.starts_with("data:image/jpeg;base64,"),
                "the jpg chunk rides its declared mime: {url}"
            );
        }
        // ONE derived index holds BOTH modalities' rows
        let indexes = f.state.storage.list_indexes("docs").await.unwrap();
        let derived: Vec<&str> = indexes
            .iter()
            .map(|m| m.index_name.as_str())
            .filter(|n| n.starts_with("unified-"))
            .collect();
        assert_eq!(derived.len(), 1, "{indexes:?}");
        assert_eq!(
            f.state
                .storage
                .count_vectors("docs", derived[0])
                .await
                .unwrap(),
            2,
            "text chunk + image chunk, same index"
        );

        let b64 = |b: &[u8]| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b)
        };
        // text query (the exact chunk text): top hit = the TEXT chunk, and
        // the IMAGE chunk ranks from the SAME index (cross-modal rows in one
        // list — the unified-space shape)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":fox_text},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "{b}");
        assert_eq!(hits[0]["content"]["type"], json!("TEXT"), "{b}");
        assert!(hits[0]["key"].as_str().unwrap().contains("-text-"), "{b}");
        assert_eq!(hits[0]["metadata"]["x-vf-modality"], json!("text"), "{b}");
        assert!(hits[0]["score"].as_f64().unwrap() > 0.99, "identity: {b}");
        assert_eq!(hits[1]["content"]["type"], json!("IMAGE"), "{b}");
        assert_eq!(hits[1]["metadata"]["x-vf-modality"], json!("image"), "{b}");
        for h in hits {
            assert_eq!(h["index"], json!(derived[0]), "single index: {b}");
        }

        // image query (red's exact bytes): round-trips to its own source —
        // an IMAGE row from the SAME index, rank 1, distance ~0
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":{"bytes": b64(&red), "mime": "image/jpeg"}},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "{b}");
        assert_eq!(hits[0]["content"]["type"], json!("IMAGE"), "{b}");
        assert_eq!(hits[0]["location"]["key"], json!("docs/red.jpg"), "{b}");
        assert_eq!(hits[0]["index"], json!(derived[0]), "{b}");
        assert!(hits[0]["score"].as_f64().unwrap() > 0.99, "identity: {b}");
        // and the TEXT row ranks from the same index (以图搜文 through the
        // unified space — the capability the dual-tower form cannot have)
        assert_eq!(hits[1]["content"]["type"], json!("TEXT"), "{b}");
        assert_eq!(hits[1]["location"]["key"], json!("docs/fox.txt"), "{b}");
    }

    /// b1 e2e #2 — coexistence fan-out: the unified openai embedder AND
    /// siglip2 side by side. The image chunk fans out to BOTH doc sides (two
    /// derived indexes, one row each); an image query fans out to both query
    /// sides (both declare Image) and merges; a text query fans out to the
    /// unified index AND siglip2's text tower.
    #[tokio::test]
    async fn unified_openai_and_siglip2_coexist_fanout() {
        let (base, fake) = crate::pipeline::embed::openai::tests::fake_openai_with(Some(8)).await;
        let text = format!(
            "[extractor.imgs]\nkind = \"image\"\n\n\
             [embedder.unified]\nkind = \"openai\"\nendpoint = \"{base}\"\n\
             model = \"fake-qwen3vl\"\ndim = 8\napi_key = \"test-key\"\n\
             multimodal = true\n\n\
             [embedder.clip]\nkind = \"siglip2\"\nmodel = \"tiny-siglip2\"\npath = {:?}\n\
             offline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\n\
             extract = {{ img = \"imgs\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"unified\", \"clip\"]\nscan_interval_secs = 1\n",
            siglip2_fixture_dir().display().to_string()
        );
        let cfg = load_pipeline_config(&text).unwrap();
        let f = fixture_with(cfg).await;
        let red = crate::pipeline::extract::image::tests::solid_png((8, 8), [255, 0, 0]);
        put_source_call(&f, "docs", "docs/red.png", red.clone()).await;
        drive(&f).await;
        let m = marker_of(&f, "docs/red.png").await.unwrap();
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");

        // the image chunk reached BOTH doc sides: two derived indexes, one
        // row each (store-level — the wire attribution only names route one)
        let indexes = f.state.storage.list_indexes("docs").await.unwrap();
        let names: Vec<&str> = indexes.iter().map(|m| m.index_name.as_str()).collect();
        let unified_ix = names
            .iter()
            .find(|n| n.starts_with("unified-"))
            .expect("{names:?}");
        let clip_ix = names
            .iter()
            .find(|n| n.starts_with("clip-"))
            .expect("{names:?}");
        for ix in [unified_ix, clip_ix] {
            assert_eq!(
                f.state.storage.count_vectors("docs", ix).await.unwrap(),
                1,
                "the image chunk embedded into {ix}"
            );
        }
        // the unified route rode the messages dialect; the clip route never
        // touched the fake (it is a local ort session)
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "one doc request through the fake: {seen:?}");
            assert!(seen[0].body["messages"].is_array());
        }

        let b64 = |b: &[u8]| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b)
        };
        // image query: fans out to BOTH query sides (both declare Image),
        // merges (rrf over the same key in two lists), red top
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":{"bytes": b64(&red), "mime": "image/png"}},"topK":5}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert_eq!(hits[0]["location"]["key"], json!("docs/red.png"), "{b}");
        assert_eq!(
            hits[0]["index"],
            json!(unified_ix),
            "attribution = first route: {b}"
        );

        // text query: fans out to the unified index's text route AND
        // siglip2's text tower (文字搜图) — both lists exist and merge
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":5}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "both routes answered: {b}");
        for h in hits {
            assert_eq!(h["content"]["type"], json!("IMAGE"), "{b}");
        }
    }

    /// M3-T8 merge weighted over the wire, in-process: a DUAL-embedder
    /// bucket (txt->tiny text index + img->clip image index, `merge =
    /// "weighted"` with tiny=3.0 / clip=1.0). A TEXT query fans out to BOTH
    /// routes (clip's query side declares Text), and the merged order+scores
    /// must equal the weighted rank fusion computed INDEPENDENTLY from the
    /// two routes' own engine-direct rankings (the oracle). The same fixture
    /// under the default rrf must equal the uniform fusion — the regression
    /// half.
    #[tokio::test]
    async fn search_weighted_merge_matches_independent_fusion() {
        let dual_config = |merge_lines: &str| {
            format!(
                "[extractor.plain]\nkind = \"txt\"\n\n\
                 [extractor.imgs]\nkind = \"image\"\n\n\
                 [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\n\
                 offline = true\n\n\
                 [embedder.clip]\nkind = \"siglip2\"\nmodel = \"tiny-siglip2\"\npath = {:?}\n\
                 offline = true\n\n\
                 [ingest.docs]\ninclude = [\"docs/**\"]\n\
                 extract = {{ txt = \"plain\", img = \"imgs\" }}\n\
                 chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
                 embedders = [\"tiny\", \"clip\"]\n{merge_lines}scan_interval_secs = 1\n",
                fixture_dir().display().to_string(),
                siglip2_fixture_dir().display().to_string()
            )
        };

        for (merge_lines, w_tiny, w_clip) in [
            (
                "merge = \"weighted\"\nmerge-weights = { tiny = 3.0, clip = 1.0 }\n",
                3.0,
                1.0,
            ),
            ("", 1.0, 1.0), // absent merge = rrf default (regression half)
        ] {
            let cfg = load_pipeline_config(&dual_config(merge_lines)).unwrap();
            let f = fixture_with(cfg).await;
            put_source_call(
                &f,
                "docs",
                "docs/a.txt",
                b"alpha body of the first".to_vec(),
            )
            .await;
            put_source_call(
                &f,
                "docs",
                "docs/b.txt",
                b"beta body of the second".to_vec(),
            )
            .await;
            let red = crate::pipeline::extract::image::tests::solid_png((8, 8), [255, 0, 0]);
            let blue = crate::pipeline::extract::image::tests::solid_png((8, 8), [0, 0, 255]);
            put_source_call(&f, "docs", "docs/red.png", red).await;
            put_source_call(&f, "docs", "docs/blue.png", blue).await;
            drive(&f).await;
            for key in ["docs/a.txt", "docs/b.txt", "docs/red.png", "docs/blue.png"] {
                assert_eq!(
                    marker_of(&f, key).await.unwrap().status,
                    SourceStatus::Indexed,
                    "{key} ({merge_lines:?})"
                );
            }

            // ---- the oracle: each route's ranking, engine-direct ----
            let text = "hello weighted vectorfs";
            let mut fused: Vec<(String, f64)> = Vec::new();
            let specs = f.handle.shared().derived_specs("docs").unwrap();
            for (name, weight) in [("tiny", w_tiny), ("clip", w_clip)] {
                let spec = specs.iter().find(|s| s.embedder == name).unwrap();
                let loaded = f.handle.shared().embedder(name).unwrap();
                let qv = loaded
                    .embed_query(&Query::Text(text.to_string()))
                    .await
                    .unwrap();
                let out = f
                    .state
                    .storage
                    .query_vectors(
                        "docs",
                        &spec.index_name,
                        spec.dim as i32,
                        "cosine",
                        &qv,
                        10,
                        None,
                        crate::store::QueryRoute::Pushdown,
                        None,
                    )
                    .await
                    .unwrap();
                assert!(!out.hits.is_empty(), "{name} route has hits");
                for (rank0, h) in out.hits.iter().enumerate() {
                    fused.push((h.key.clone(), weight / (60.0 + rank0 as f64 + 1.0)));
                }
            }
            fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            fused.truncate(10);

            // ---- the wire: Search must reproduce the oracle exactly ----
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"text": text},"topK":10}),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{b}");
            let hits = b["hits"].as_array().unwrap();
            assert_eq!(hits.len(), fused.len(), "{b}");
            let mut indexes = std::collections::BTreeSet::new();
            for (h, (key, score)) in hits.iter().zip(&fused) {
                assert_eq!(h["key"].as_str().unwrap(), key, "{b}");
                assert_eq!(
                    &h["score"].as_f64().unwrap(),
                    score,
                    "exact fused score: {b}"
                );
                indexes.insert(h["index"].as_str().unwrap().split('-').next().unwrap());
            }
            // both routes really contributed (a merge over one list is no
            // merge at all)
            assert_eq!(
                indexes,
                std::collections::BTreeSet::from(["tiny", "clip"]),
                "fan-out over both embedders: {b}"
            );
            // no `distance` on multi-route hits (appendix E.2)
            assert!(hits.iter().all(|h| h.get("distance").is_none()), "{b}");
        }
    }

    // ---------------- Search rerank (M3-T11) ----------------

    /// The committed tiny-RERANKER fixture (the cross-encoder kind's own
    /// suite runs it; these wire tests do too).
    fn reranker_fixture_dir() -> std::path::PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-reranker",
            "tests/fixtures/tiny-reranker",
        ] {
            let d = std::path::PathBuf::from(p);
            if d.join("model.onnx").is_file() {
                return d;
            }
        }
        panic!("tiny-reranker fixture not found under tests/fixtures/");
    }

    /// tiny text pipeline + the tiny cross-encoder reranker (path-based,
    /// hermetic). `extra` splices further ingest lines (fts for hybrid).
    fn tiny_rerank_config(extra: &str) -> crate::pipeline::config::PipelineConfig {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n\
             rerank = {{ kind = \"cross-encoder\", model = \"tiny-reranker-fixture\", path = \
             {:?}, offline = true }}\n{extra}",
            fixture_dir().display().to_string(),
            reranker_fixture_dir().display().to_string()
        );
        load_pipeline_config(&text).unwrap()
    }

    /// e2e: Search merges, then reranks the topK candidates with the tiny
    /// cross-encoder (logit = sum of (token_id+1) over the masked pair — an
    /// EXACT oracle), then shapes hits. Pinned:
    /// * order = rerank score desc, ties by key asc (the two identical
    ///   docs tie and fall to the chunk-key order);
    /// * `score` carries the raw logit (exact values asserted);
    /// * `distance` is omitted (the order no longer derives from it);
    /// * hit count is unchanged (rerank re-orders, never filters);
    /// * rerank ABSENT on a rerank-CONFIGURED bucket is byte-identical to
    ///   the same bucket without the rerank line at all (the additive red
    ///   line: availability never changes the off path).
    #[tokio::test]
    async fn search_rerank_reorders_with_exact_scores_and_ties_break_by_key() {
        let f = fixture_with(tiny_rerank_config("")).await;
        // one 32-char window each: "the fox"(59) < "contract probe
        // chunk"(51)? — the sums are asserted exactly below; identical
        // c1/c2 texts force the tie
        put_source_call(&f, "docs", "docs/a.txt", b"the fox".to_vec()).await;
        put_source_call(
            &f,
            "docs",
            "docs/b.txt",
            b"fox embeds the quick fox".to_vec(),
        )
        .await;
        put_source_call(&f, "docs", "docs/c1.txt", b"contract probe chunk".to_vec()).await;
        put_source_call(&f, "docs", "docs/c2.txt", b"contract probe chunk".to_vec()).await;
        drive(&f).await;
        for key in ["docs/a.txt", "docs/b.txt", "docs/c1.txt", "docs/c2.txt"] {
            assert_eq!(
                marker_of(&f, key).await.unwrap().status,
                SourceStatus::Indexed,
                "{key}"
            );
        }

        // ---- rerank ON: the tiny cross-encoder's exact logits ----
        // pair template [CLS] query [SEP] doc [SEP]; logit = Σ(id+1):
        //   query "hello vectorfs" prefix = (2+1)+(9+1)+(10+1)+(3+1) = 28
        //   b "fox embeds the quick fox" = 28 + (15+16+12+13+15) + 4 = 103
        //   a "the fox"                 = 28 + (12+15) + 4 = 59
        //   c1/c2 "contract probe chunk"= 28 + (5+6+8) + 4 = 51  (tie)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":10,"rerank":true}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 4, "rerank never filters: {b}");
        let src = |h: &serde_json::Value| h["location"]["key"].as_str().unwrap().to_string();
        assert_eq!(src(&hits[0]), "docs/b.txt", "{b}");
        assert_eq!(src(&hits[1]), "docs/a.txt", "{b}");
        assert_eq!(hits[0]["score"].as_f64().unwrap(), 103.0, "{b}");
        assert_eq!(hits[1]["score"].as_f64().unwrap(), 59.0, "{b}");
        // the forced tie: identical scores, key ascending decides
        assert_eq!(
            hits[2]["score"].as_f64().unwrap(),
            hits[3]["score"].as_f64().unwrap(),
            "identical texts must tie exactly: {b}"
        );
        assert_eq!(hits[2]["score"].as_f64().unwrap(), 51.0, "{b}");
        assert!(
            hits[2]["key"].as_str().unwrap() < hits[3]["key"].as_str().unwrap(),
            "tie-break by key ascending: {b}"
        );
        // distance omitted under rerank (order no longer derives from it)
        assert!(hits.iter().all(|h| h.get("distance").is_none()), "{b}");
        // total order over the wire
        for w in hits.windows(2) {
            let ok = w[0]["score"].as_f64().unwrap() > w[1]["score"].as_f64().unwrap()
                || (w[0]["score"] == w[1]["score"]
                    && w[0]["key"].as_str().unwrap() < w[1]["key"].as_str().unwrap());
            assert!(ok, "not rerank-ordered: {b}");
        }

        // ---- rerank ABSENT on the SAME (rerank-configured) bucket: the
        // response is byte-identical to a bucket WITHOUT the rerank line —
        // availability changes nothing about the off path ----
        let (_, _, off) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":10}),
        )
        .await;
        let plain = fixture().await;
        for (key, body) in [
            ("docs/a.txt", b"the fox".to_vec()),
            ("docs/b.txt", b"fox embeds the quick fox".to_vec()),
            ("docs/c1.txt", b"contract probe chunk".to_vec()),
            ("docs/c2.txt", b"contract probe chunk".to_vec()),
        ] {
            put_source_call(&plain, "docs", key, body).await;
        }
        drive(&plain).await;
        let (_, _, plain_b) = call_json(
            &plain,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":10}),
        )
        .await;
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            serde_json::to_string(&plain_b).unwrap(),
            "rerank-configured bucket, rerank off: byte-identical to no-rerank bucket"
        );

        // ---- explicit rerank: false is the same off path ----
        let (_, _, off2) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":10,"rerank":false}),
        )
        .await;
        assert_eq!(off, off2);
    }

    /// The rerank applicability refusals (each a one-line fix, none a
    /// silent half-feature): a bucket without a reranker, keyword mode
    /// (BM25's ranking IS that mode's semantics), and an IMAGE query (the
    /// cross-encoder reads text pairs). Hybrid + rerank works (fuse, then
    /// re-score) — 200 with rerank-ordered hits.
    #[tokio::test]
    async fn search_rerank_applicability_refusals_and_hybrid() {
        // (a) no reranker configured: the config gap is a 400 naming the fix
        let plain = fixture().await;
        let (st, _, b) = call_json(
            &plain,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5,"rerank":true}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        let msg = b["message"].as_str().unwrap();
        assert!(msg.contains("no reranker configured"), "{b}");
        assert!(msg.contains("rerank = true"), "{b}");
        assert!(msg.contains("[ingest.docs]"), "{b}");

        // fts + rerank bucket for the remaining arms
        let f = fixture_with(tiny_rerank_config("fts = true\n")).await;
        put_source_call(
            &f,
            "docs",
            "docs/a.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        put_source_call(
            &f,
            "docs",
            "docs/sea.txt",
            b"whales and dolphins swim in the deep blue ocean".to_vec(),
        )
        .await;
        drive(&f).await;

        // (b) keyword + rerank: refused — BM25's ranking is that mode's
        // own semantics
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5,
                   "searchMode":"keyword","rerank":true}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("does not apply to searchMode"),
            "{b}"
        );
        assert!(b["message"].as_str().unwrap().contains("keyword"), "{b}");

        // (c) image query + rerank: refused — needs query.text. Uses the
        // image pipeline WITH a reranker so the refusal that fires is the
        // modality one (not the availability one).
        let img_cfg = {
            let mut text = format!(
                "[extractor.imgs]\nkind = \"image\"\n\n\
                 [embedder.clip]\nkind = \"siglip2\"\nmodel = \"tiny-siglip2\"\npath = {:?}\n\
                 offline = true\n\n\
                 [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ img = \"imgs\" }}\n\
                 chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
                 embedders = [\"clip\"]\nscan_interval_secs = 1\n\
                 rerank = {{ kind = \"cross-encoder\", model = \"tiny-reranker-fixture\", \
                 path = {:?}, offline = true }}\n",
                siglip2_fixture_dir().display().to_string(),
                reranker_fixture_dir().display().to_string()
            );
            text.push_str("fts = true\n"); // not needed, keeps one splice point
            load_pipeline_config(&text).unwrap()
        };
        let img_f = fixture_with(img_cfg).await;
        let png = crate::pipeline::extract::image::tests::solid_png((8, 8), [1, 2, 3]);
        let b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&png)
        };
        let (st, _, b) = call_json(
            &img_f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":{"bytes": b64, "mime": "image/png"}},
                   "topK":5,"rerank":true}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("rerank needs query.text"),
            "{b}"
        );

        // (d) hybrid + rerank: fuse, then re-score — 200, rerank-ordered,
        // distance omitted
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5,
                   "searchMode":"hybrid","rerank":true}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        for w in hits.windows(2) {
            let ok = w[0]["score"].as_f64().unwrap() > w[1]["score"].as_f64().unwrap()
                || (w[0]["score"] == w[1]["score"]
                    && w[0]["key"].as_str().unwrap() < w[1]["key"].as_str().unwrap());
            assert!(ok, "hybrid rerank not totally ordered: {b}");
        }
        assert!(hits.iter().all(|h| h.get("distance").is_none()), "{b}");
    }

    // ---------------- Search rerankDepth / perSource / fusion-overfetch
    // (recall-quality first batch: R3a + R4 + R3b, research/2026-11-recall-
    // quality-optimization.md §3) ----------------

    /// R3a: the re-scoring depth decouples from the return depth. The
    /// validation face first (bounds, dead knob, depth < topK — all
    /// field-addressable ValidationExceptions, keyword's rerank refusal
    /// untouched), then the behavior: the exact-logit corpus above — with
    /// topK=1 and NO depth the cross-encoder re-scores exactly the merged
    /// head (one candidate, itself); with rerankDepth=4 it re-scores all
    /// four docs and the max-logit doc (b, 103) takes the single returned
    /// slot even though it was NOT the semantic head. Default absent =
    /// depth topK = the pre-R3a behavior (byte-identical).
    #[tokio::test]
    async fn search_rerank_depth_decouples_candidate_and_return_depth() {
        // fts on: the keyword-refusal arm below needs the lexical face
        let f = fixture_with(tiny_rerank_config("fts = true\n")).await;
        for (key, body) in [
            ("docs/a.txt", b"the fox".to_vec()),
            ("docs/b.txt", b"fox embeds the quick fox".to_vec()),
            ("docs/c1.txt", b"contract probe chunk".to_vec()),
            ("docs/c2.txt", b"contract probe chunk".to_vec()),
        ] {
            put_source_call(&f, "docs", key, body).await;
        }
        drive(&f).await;

        // ---- validation face ----
        // bounds: 0 and 201 are outside 1..=200 (rerank: true so the
        // dead-knob arm cannot fire first)
        for bad in [0i64, 201] {
            let (st, _, b) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":5,
                       "rerank":true,"rerankDepth":bad}),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {b}");
            assert_eq!(
                b["fieldList"][0]["path"],
                json!("rerankDepth"),
                "{bad}: {b}"
            );
            assert!(
                b["message"].as_str().unwrap().contains("between 1 and 200"),
                "{bad}: {b}"
            );
        }
        // dead knob: a depth without rerank is refused (the fts-tokenizer
        // discipline — never a silently-ignored value)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":5,
                   "rerankDepth":5}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("rerankDepth"), "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("rerank is not true"),
            "{b}"
        );
        // depth < topK would SHRINK the result set — refused
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":10,
                   "rerank":true,"rerankDepth":5}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("rerankDepth"), "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("must be >= topK"),
            "{b}"
        );
        // keyword still refuses rerank itself, depth or no depth
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5,
                   "searchMode":"keyword","rerank":true,"rerankDepth":5}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("does not apply to searchMode"),
            "{b}"
        );

        // ---- behavior ----
        // the semantic head first (rerank OFF): whoever leads the merged
        // order is what a depth-less topK=1 re-scores
        let (_, _, plain) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":4}),
        )
        .await;
        let semantic_head = plain["hits"][0]["key"].as_str().unwrap().to_string();
        let b_hash = crate::ingest::marker::object_key_hash("docs/b.txt");
        let b_key = plain["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap())
            .find(|k| k.starts_with(&b_hash))
            .expect("docs/b.txt has a hit")
            .to_string();
        assert_ne!(
            semantic_head, b_key,
            "precondition: the max-logit doc is not the semantic head — tune the corpus"
        );

        // topK=1, NO depth: the one candidate is the merged head itself
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":1,
                   "rerank":true}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["hits"].as_array().unwrap().len(), 1, "{b}");
        assert_eq!(b["hits"][0]["key"].as_str().unwrap(), semantic_head, "{b}");

        // topK=1, rerankDepth=4: all four candidates re-scored, b's 103
        // logit takes the single returned slot — the return depth stays 1
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":1,
                   "rerank":true,"rerankDepth":4}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["hits"].as_array().unwrap().len(), 1, "{b}");
        assert_eq!(b["hits"][0]["key"].as_str().unwrap(), b_key, "{b}");
        assert_eq!(b["hits"][0]["score"].as_f64().unwrap(), 103.0, "{b}");

        // topK=2, rerankDepth=4: four re-scored, TWO returned (b first)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"hello vectorfs"},"topK":2,
                   "rerank":true,"rerankDepth":4}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["hits"].as_array().unwrap().len(), 2, "{b}");
        assert_eq!(b["hits"][0]["key"].as_str().unwrap(), b_key, "{b}");
    }

    /// R4: the per-source quota (grouping). A fat multi-chunk document
    /// crowds the merged top-10 (the textbook-794-chunks shape); `perSource`
    /// caps every source's share — walked in fusion order (the survivors
    /// are a SUBSEQUENCE of the unquota'd order), counts respected at 1
    /// and 2, and a quota that thins the set returns FEWER than topK
    /// honestly (no backfill of the skipped same-source hits). Absent and
    /// explicit 0 are byte-identical (the default = off).
    #[tokio::test]
    async fn search_per_source_quota_groups_merged_order() {
        let f = fixture_with(tiny_fts_config(false)).await;
        // the fat doc: ~5 windows, every one carries "fox" — near-duplicate
        // chunks cluster in embedding space and crowd the top-10 (kept
        // small enough that ALL chunks fit one top-10 fetch, so the quota's
        // diversity math is over the whole corpus)
        let fat = ["the fox network signal route "; 4].concat();
        put_source_call(&f, "docs", "docs/fat.txt", fat.into_bytes()).await;
        for (i, body) in [
            (1, b"the fox rides alone tonight".to_vec()),
            (2, b"a fox sleeps under the bridge".to_vec()),
            (3, b"fox watchers report movement".to_vec()),
        ] {
            put_source_call(&f, "docs", &format!("docs/s{i}.txt"), body).await;
        }
        drive(&f).await;

        let quotaless = |per_source: Option<i64>| {
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10,
                   "perSource": per_source})
        };
        let (st, _, base) = call_json(&f, "/x-vf/Search", quotaless(None)).await;
        assert_eq!(st, StatusCode::OK, "{base}");
        let src = |h: &serde_json::Value| h["location"]["key"].as_str().unwrap().to_string();
        let hits = base["hits"].as_array().unwrap();
        // precondition: the fat doc really crowds (>= 3 of its chunks in
        // the top-10 — otherwise this corpus no longer exercises R4)
        let fat_count = hits.iter().filter(|h| src(h) == "docs/fat.txt").count();
        assert!(fat_count >= 3, "crowding precondition: {base}");

        // ---- perSource = 1: every source at most once, survivors keep
        // the fusion order (a subsequence of the quotaless ranking) ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10,"perSource":1}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        let mut per_src: std::collections::BTreeMap<String, usize> = Default::default();
        for h in hits {
            *per_src.entry(src(h)).or_insert(0) += 1;
        }
        assert!(
            per_src.values().all(|c| c <= &1),
            "perSource=1 caps every source: {per_src:?} — {b}"
        );
        assert_eq!(per_src.get("docs/fat.txt"), Some(&1), "{b}");
        assert!(hits.len() >= 3, "cross-source diversity: {b}");
        // order preservation: each survivor's position in the quotaless
        // baseline strictly increases
        let base_order: Vec<String> = base["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap().to_string())
            .collect();
        let mut prev: Option<usize> = None;
        for h in hits {
            let key = h["key"].as_str().unwrap();
            let pos = base_order
                .iter()
                .position(|k| k == key)
                .expect("survivors come from the quotaless ranking");
            assert!(
                prev.is_none_or(|p| p < pos),
                "quota keeps fusion order: {key} at {pos} after {prev:?}"
            );
            prev = Some(pos);
        }

        // ---- perSource = 2 ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10,"perSource":2}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let mut per_src: std::collections::BTreeMap<String, usize> = Default::default();
        for h in b["hits"].as_array().unwrap() {
            *per_src.entry(src(h)).or_insert(0) += 1;
        }
        assert!(
            per_src.values().all(|c| c <= &2),
            "perSource=2 caps every source: {per_src:?} — {b}"
        );
        assert_eq!(per_src.get("docs/fat.txt"), Some(&2), "{b}");

        // ---- honest shortfall: only TWO sources match this query —
        // perSource=1 returns 2 hits, NOT 10 (the skipped fat chunks are
        // never backfilled) ----
        put_source_call(
            &f,
            "docs",
            "docs/lone.txt",
            b"a fox and nothing else".to_vec(),
        )
        .await;
        drive(&f).await;
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10,
                   "filter":{"x-vf-src-name":{"$in":["docs/fat.txt","docs/lone.txt"]}},
                   "perSource":1}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "two sources x quota 1, no backfill: {b}");
        assert_eq!(src(&hits[0]), "docs/fat.txt", "{b}");
        assert_eq!(src(&hits[1]), "docs/lone.txt", "{b}");

        // ---- validation: 101 is outside 1..=100 ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":5,"perSource":101}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("perSource"), "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("between 0 and 100"),
            "{b}"
        );

        // ---- default = off: absent and explicit 0 byte-identical ----
        let (_, _, zero) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10,"perSource":0}),
        )
        .await;
        // (the baseline above predates docs/lone.txt — recompute it)
        let (_, _, base2) = call_json(&f, "/x-vf/Search", quotaless(None)).await;
        assert_eq!(
            serde_json::to_string(&base2).unwrap(),
            serde_json::to_string(&zero).unwrap(),
            "perSource 0 == absent (off)"
        );
    }

    /// R4 × rerank ordering: the quota runs BEFORE the rerankDepth
    /// truncation, so the cross-encoder re-scores the QUOTA'D candidate
    /// set. Observable: with perSource=1 the surviving chunk of the fat
    /// source is the one the FUSION admitted (its rank-first chunk), not
    /// the one the reranker would have crowned — pin both orders at
    /// runtime and assert they disagree (precondition), then that the
    /// quota's choice wins.
    #[tokio::test]
    async fn search_per_source_quota_runs_before_rerank() {
        let f = fixture_with(tiny_rerank_config("fts = true\n")).await;
        // the fat source: the FIRST window carries the fixture vocab's
        // LOW-id words ("contract probe chunk" — the exact-logit corpus's
        // 51-scorers) and the later windows carry the HIGH-id phrase
        // ("fox embeds the quick fox" — the 103-scorer), so the fusion
        // order (semantic all-tie → key asc → text-0) and the rerank order
        // (logit sums → the later windows) genuinely disagree
        let fat = format!(
            "{}{}",
            "contract probe chunk ",
            " fox embeds the quick fox fox embeds the quick fox fox embeds the quick fox"
        );
        put_source_call(&f, "docs", "docs/fat.txt", fat.into_bytes()).await;
        put_source_call(
            &f,
            "docs",
            "docs/b.txt",
            b"the quick brown fox jumps over the lazy dog".to_vec(),
        )
        .await;
        put_source_call(
            &f,
            "docs",
            "docs/c.txt",
            b"whales and dolphins swim in the deep blue ocean".to_vec(),
        )
        .await;
        drive(&f).await;

        // fusion order (rerank OFF): the fat source's rank-first chunk
        let (_, _, plain) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10}),
        )
        .await;
        let fusion_first = plain["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["location"]["key"] == json!("docs/fat.txt"))
            .map(|h| h["key"].as_str().unwrap().to_string())
            .expect("the fat source has hits");

        // rerank order (quota OFF): the fat chunk the cross-encoder ranks
        // first — computed through the SAME loaded reranker the wire uses
        let reranker = f.handle.shared().reranker_of("docs").unwrap();
        let fat_chunks: Vec<crate::pipeline::RerankCandidate> = plain["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["location"]["key"] == json!("docs/fat.txt"))
            .map(|h| {
                let text = h["content"]["text"].as_str().unwrap_or("");
                crate::pipeline::RerankCandidate {
                    key: h["key"].as_str().unwrap().to_string(),
                    text: text.to_string(),
                }
            })
            .collect();
        assert!(fat_chunks.len() >= 2, "the fat source is multi-chunk");
        let reranked = reranker.rerank("fox", &fat_chunks).await.unwrap();
        let rerank_first = reranked[0].key.clone();
        assert!(
            fat_chunks.iter().any(|c| c.key == fusion_first),
            "both orders over the same chunk set"
        );
        assert_ne!(
            fusion_first, rerank_first,
            "precondition: fusion-first != rerank-first — tune the corpus"
        );

        // rerank + perSource=1: exactly ONE fat chunk survives, and it is
        // the fusion-admitted one — had the quota run AFTER the rerank,
        // the survivor would be the reranker's pick instead
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"fox"},"topK":10,
                   "rerank":true,"perSource":1}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let fat_hits: Vec<&serde_json::Value> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["location"]["key"] == json!("docs/fat.txt"))
            .collect();
        assert_eq!(fat_hits.len(), 1, "quota: one fat chunk — {b}");
        assert_eq!(
            fat_hits[0]["key"].as_str().unwrap(),
            fusion_first,
            "the fusion-admitted chunk survives the rerank, not the reranker's pick: {b}"
        );
        // b/c still there (the quota thins, the companions ride along)
        assert_eq!(b["hits"].as_array().unwrap().len(), 3, "{b}");
    }

    /// tiny fts pipeline + an extra splice line for the ingest section
    /// (fusion-overfetch tests, R3b).
    fn tiny_fts_extra_config(extra: &str) -> crate::pipeline::config::PipelineConfig {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\nfts = true\n{extra}",
            fixture_dir().display().to_string()
        );
        load_pipeline_config(&text).unwrap()
    }

    /// The R1 anchor corpus at wire scale: the needle's anchor terms are
    /// df-small (the needle's own chunks) while the filler words are
    /// df-huge (700 filler lines ≈ 2000 chunks — filler counts read as
    /// the probe cap). One needle source carries both anchors at high tf;
    /// one TOC-shaped source is dense with the query's structure words
    /// and carries NO needle anchor (the experiment-G flooding shape in
    /// miniature).
    async fn anchor_fixture(extra: &str) -> Fixture {
        let f = fixture_with(tiny_fts_extra_config(extra)).await;
        // ~600 chunks of filler (token-window 32/8: ~24-token stride)
        let mut filler = String::with_capacity(48 * 1024);
        for i in 0..700 {
            filler.push_str(&format!(
                "filler line number {i} of the anchor test corpus\n"
            ));
        }
        put_source_call(&f, "docs", "docs/filler.txt", filler.into_bytes()).await;
        // the TOC-shaped flooder: structure words, high tf, NO anchor
        let toc = "\u{6280}\u{672f}\u{96f7}\u{8fbe} \u{6280}\u{672f}\u{96f7}\u{8fbe} \
                   \u{5e73}\u{53f0} \u{5de5}\u{5177} \u{8bed}\u{8a00} \u{5e73}\u{53f0} \
                   \u{5de5}\u{5177} \u{8bed}\u{8a00} \u{6280}\u{672f}\u{96f7}\u{8fbe} \
                   \u{6280}\u{672f}\u{96f7}\u{8fbe}";
        put_source_call(&f, "docs", "docs/toc.txt", toc.as_bytes().to_vec()).await;
        // the needle: both anchors at tf=2 inside a structure-word frame
        put_source_call(
            &f,
            "docs",
            "docs/needle.txt",
            "vibe vibe coding coding \u{81ea}\u{6ee1}\u{4e8e} AI \u{751f}\u{6210}\u{7684}\
             \u{4ee3}\u{7801} \u{7684}\u{91c7}\u{7528}\u{4e0e}\u{53d8}\u{5316}"
                .as_bytes()
                .to_vec(),
        )
        .await;
        drive(&f).await;
        let spec = f
            .handle
            .shared()
            .derived_specs("docs")
            .unwrap()
            .pop()
            .unwrap();
        f.state
            .storage
            .ensure_fts_index("docs", &spec.index_name, "icu")
            .await
            .unwrap();
        f
    }

    /// R1→T17 wire: the origin-query FORM through `/x-vf/Search` keyword
    /// mode — the response carries `anchors` beside `hits` exactly when
    /// the analysis ran (the verdict = every df≥1 candidate, df ascending,
    /// analyzer spellings: coding stems to `code`), the gate (verdict head
    /// = argmin-df) leaves only head-carrying chunks on the lexical arm,
    /// pure-CJK queries analyze to NON-empty anchors now (the analyzer
    /// terms — the R1 blind spot fixed; the 恒门: structure-word queries
    /// gate too), and semantic mode never runs the analysis.
    #[tokio::test]
    async fn anchor_gating_wire_gates_lexical_arm_and_reports_anchors() {
        let f = anchor_fixture("").await;

        // the origin form: the T17 verdict — analyzer terms in the index's
        // own spellings (ThoughtWorks→thoughtwork is df=0 here: the corpus
        // never spells it, so it drops; coding→code), df ASCENDING:
        // vibe/code/变化 are df=1 (needle's single vibe-carrying chunk),
        // 技术/雷达 df=2 (the TOC's two chunks) — the head (vibe) is the
        // gate, exactly the R1 acceptance shape
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs",
                   "query":{"text":"ThoughtWorks \u{6280}\u{672f}\u{96f7}\u{8fbe}\u{4e2d} vibe \
                    coding \u{6280}\u{672f}\u{6709}\u{54ea}\u{4e9b}\u{53d8}\u{5316}"},
                   "topK":10,"searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["anchors"],
            json!(["vibe", "code", "变化", "技术", "雷达"]),
            "the analysis verdict rides the response: every df>=1 candidate, \
             df ascending, analyzer spellings (coding->code)"
        );
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "the needle survives the gate: {b}");
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"] == json!("docs/needle.txt")),
            "only head-carrying chunks return (the head is vibe): {b}"
        );

        // pure CJK: NON-empty anchors — the T17 fix (v1 analyzed pure-CJK
        // to zero candidates). The verdict is df-ascending over the
        // analyzer's CJK terms, the head (技术, first-seen among the TOC's
        // df ties) gates the lexical arm onto the TOC flooder — the 恒门:
        // structure words gate on their argmin, no zero-anchor escape
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"\u{6280}\u{672f}\u{96f7}\u{8fbe} \u{5e73}\
                    \u{53f0} \u{5de5}\u{5177}"},"topK":10,"searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["anchors"],
            json!(["平台", "工具", "技术", "雷达"]),
            "CJK terms analyze to candidates — df ascending: 平台/工具 live in \
             one TOC chunk (df=1, first-seen order), 技术/雷达 straddle both (df=2)"
        );
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0]["location"]["key"], json!("docs/toc.txt"), "{b}");

        // semantic mode: no analysis, no member (the old wire shape)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"vibe coding \u{53d8}\u{5316}"},
                   "topK":5,"searchMode":"semantic"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert!(b.get("anchors").is_none(), "semantic never analyzes: {b}");

        // HYBRID on an fts bucket: the recommended mode rides the SAME
        // gated lexical arm — the anchors member carries the verdict beside
        // the FUSED hits, and the needle's gated lexical rank-1 contribution
        // surfaces it in the fused head (the semantic arm legitimately
        // contributes its own candidates too — that is what fusion means)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs",
                   "query":{"text":"ThoughtWorks \u{6280}\u{672f}\u{96f7}\u{8fbe}\u{4e2d} vibe \
                    coding \u{6280}\u{672f}\u{6709}\u{54ea}\u{4e9b}\u{53d8}\u{5316}"},
                   "topK":10,"searchMode":"hybrid"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["anchors"],
            json!(["vibe", "code", "变化", "技术", "雷达"]),
            "hybrid analyzes and gates exactly like keyword mode: {b}"
        );
        let hits = b["hits"].as_array().unwrap();
        let needle_rank = hits
            .iter()
            .position(|h| h["location"]["key"] == json!("docs/needle.txt"));
        assert!(
            needle_rank.is_some_and(|r| r < 5),
            "the gated lexical rank-1 contribution surfaces the needle in the \
             fused head: {b}"
        );
    }

    /// R1 wire, the off switch: `anchor-gating = false` runs NO analysis
    /// (no `anchors` member) and the lexical arm stays ungated — the TOC
    /// flooder rides its structure-word density back into the keyword
    /// results, exactly the pre-R1 behavior.
    #[tokio::test]
    async fn anchor_gating_off_wire_keeps_the_ungated_lexical_arm() {
        let f = anchor_fixture("anchor-gating = false\n").await;

        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs",
                   "query":{"text":"ThoughtWorks \u{6280}\u{672f}\u{96f7}\u{8fbe}\u{4e2d} vibe \
                    coding \u{6280}\u{672f}\u{6709}\u{54ea}\u{4e9b}\u{53d8}\u{5316}"},
                   "topK":10,"searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert!(b.get("anchors").is_none(), "the knob skips the analysis");
        let keys: Vec<serde_json::Value> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["location"]["key"].clone())
            .collect();
        assert!(
            keys.contains(&json!("docs/toc.txt")),
            "ungated: the structure-word flooder is back: {b}"
        );
    }

    /// R3b: `fusion-overfetch` widens every retrieval arm (fetch
    /// min(max(topK, rerankDepth) × mult, 1000) instead of topK). The
    /// deep case is asserted EXACTLY against an independent oracle (both
    /// arms engine-direct at the deep fetch, rrf-fused) — safe to pin
    /// because the deep fetch covers the whole corpus, so neither arm is
    /// ever cut at a tie boundary (the tiny embedder is CLS-constant:
    /// every distance ties at 0, and the ENGINE's pick among tied rows at
    /// a limit cut is not stable across calls — a shallow exact-order
    /// oracle would be flaky by construction, so the shallow side is
    /// asserted through the mechanism instead: a doc whose BM25 rank sits
    /// beyond the shallow cut can only carry its second rrf contribution
    /// when the arm fetched deep enough). A single-arm keyword query is
    /// byte-identical under any multiplier (a deeper fetch only appends
    /// to a list whose ranking is already total).
    #[tokio::test]
    async fn search_fusion_overfetch_widens_the_arms_and_changes_the_fusion() {
        // the corpus: four single-chunk docs (each ≤ one 32-char window),
        // matching the query term with DISTINCT multiplicity — BM25 ranks
        // them strictly (no ties, a deterministic arm at any cut) while
        // the degenerate semantic arm ties everything at distance 0 (its
        // full-fetch order is chunk-key asc, equally deterministic)
        let corpus: Vec<(String, String)> = (1..=4)
            .map(|i| {
                (
                    format!("docs/p{i}.txt"),
                    format!("{}{}", "alpha ".repeat(i), "tail"),
                )
            })
            .collect();
        let query = "alpha";
        let top_k = 3usize;

        // the deep oracle: both arms engine-direct at a fetch that covers
        // the whole corpus, fused with the same rrf the wire uses
        async fn deep_oracle(f: &Fixture, query: &str, fetch: usize, top_k: usize) -> HitList {
            let spec = f
                .handle
                .shared()
                .derived_specs("docs")
                .unwrap()
                .pop()
                .unwrap();
            let loaded = f.handle.shared().embedder("tiny").unwrap();
            let qv = loaded
                .embed_query(&Query::Text(query.to_string()))
                .await
                .unwrap();
            let sem = f
                .state
                .storage
                .query_vectors(
                    "docs",
                    &spec.index_name,
                    spec.dim as i32,
                    "cosine",
                    &qv,
                    fetch,
                    None,
                    crate::store::QueryRoute::Pushdown,
                    None,
                )
                .await
                .unwrap();
            let bm = crate::lexical::kernel()
                .search(
                    &f.state.storage,
                    "docs",
                    &spec.index_name,
                    query,
                    fetch,
                    None,
                )
                .await
                .unwrap();
            let sem_list: HitList = sem
                .hits
                .iter()
                .map(|h| crate::pipeline::Hit {
                    key: h.key.clone(),
                    score: 1.0 / (1.0 + h.distance as f64),
                })
                .collect();
            let bm_list: HitList = bm
                .iter()
                .map(|h| crate::pipeline::Hit {
                    key: h.key.clone(),
                    score: h.score,
                })
                .collect();
            let mut fused = merge_hitlists(Merge::Rrf, &[sem_list, bm_list]);
            fused.truncate(top_k);
            fused
        }

        // ---- the BM25 order, through the wire's own keyword face (full
        // depth: topK = corpus size, no cut, strictly ordered scores) ----
        let f1 = fixture_with(tiny_fts_extra_config("")).await;
        for (key, body) in &corpus {
            put_source_call(&f1, "docs", key.as_str(), body.clone().into_bytes()).await;
        }
        drive(&f1).await;
        let (st, _, b) = call_json(
            &f1,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":query},"topK":8,
                   "searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let bm_order: Vec<String> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(bm_order.len(), 4, "{b}");
        // strict BM25 scores: no adjacent tie (the arm is deterministic
        // at any fetch cut)
        for w in b["hits"].as_array().unwrap().windows(2) {
            assert!(
                w[0]["score"].as_f64().unwrap() > w[1]["score"].as_f64().unwrap(),
                "strict bm25 order: {b}"
            );
        }

        // ---- multiplier 3: fetch == 9 >= corpus — exact oracle equality
        // (both arms full-depth, deterministic) ----
        let f3 = fixture_with(tiny_fts_extra_config("fusion-overfetch = 3\n")).await;
        for (key, body) in &corpus {
            put_source_call(&f3, "docs", key.as_str(), body.clone().into_bytes()).await;
        }
        drive(&f3).await;
        let oracle = deep_oracle(&f3, query, top_k * 3, top_k).await;
        let (st, _, deep) = call_json(
            &f3,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":query},"topK":top_k,
                   "searchMode":"hybrid"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{deep}");
        let deep_keys: Vec<&str> = deep["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap())
            .collect();
        let oracle_keys: Vec<&str> = oracle.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(deep_keys, oracle_keys, "deep fetch == oracle: {deep}");

        // ---- the knob's bite (mechanism-level, robust to the semantic
        // arm's unstable tie-pick): find a deep-head doc whose BM25 rank
        // is BEYOND the shallow fetch cut ----
        let deep_late_bm: Vec<&str> = deep_keys
            .iter()
            .filter(|k| bm_order.iter().position(|b| b == *k).unwrap() >= top_k)
            .copied()
            .collect();
        assert!(
            !deep_late_bm.is_empty(),
            "precondition: the deep head contains a doc whose bm25 rank is beyond the \
             shallow cut — tune the doc names (keys order the tied semantic arm): \
             bm={bm_order:?} deep={deep_keys:?}"
        );
        let late = deep_late_bm[0];
        let late_deep_score = deep["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["key"].as_str().unwrap() == late)
            .unwrap()["score"]
            .as_f64()
            .unwrap();

        // shallow (default multiplier): the bm arm cut at topK=3 CANNOT
        // contribute to `late` — if it appears at all it is the semantic
        // pick alone (a single rrf term, strictly less than the deep
        // two-term sum); absent-or-cheaper is the honest observable
        let (st, _, shallow) = call_json(
            &f1,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":query},"topK":top_k,
                   "searchMode":"hybrid"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{shallow}");
        let shallow_hit = shallow["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["key"].as_str().unwrap() == late);
        let shallow_late_score = shallow_hit.map(|h| h["score"].as_f64().unwrap());
        assert!(
            shallow_late_score.is_none_or(|s| s < late_deep_score),
            "the bm arm fetched deep in the deep case only: late {late} shallow \
             {shallow_late_score:?} deep {late_deep_score} — {shallow}"
        );

        // ---- single-arm keyword: the multiplier changes nothing (a
        // deeper fetch only APPENDS; truncate(topK) restores the prefix) —
        // the zero-surprise property for non-fused faces ----
        let (_, _, kw_shallow) = call_json(
            &f1,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":query},"topK":3,
                   "searchMode":"keyword"}),
        )
        .await;
        let (_, _, kw_deep) = call_json(
            &f3,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":query},"topK":3,
                   "searchMode":"keyword"}),
        )
        .await;
        assert_eq!(
            serde_json::to_string(&kw_shallow).unwrap(),
            serde_json::to_string(&kw_deep).unwrap(),
            "single-arm faces are multiplier-blind: {kw_shallow} vs {kw_deep}"
        );
    }

    // ---------------- Search searchMode (M3-T9) ----------------

    /// The F1 friction corpus (research/2026-11-friction-backlog.md): a
    /// COINED word that appears in exactly one chunk of one document, buried
    /// in enough sibling material that embedding retrieval cannot see it —
    /// the exact shape that forced 18MB×5 manual greps in the field.
    fn f1_corpus() -> Vec<(&'static str, String)> {
        let filler = |seed: usize| {
            format!(
                "topic {seed} discusses capacity planning queues and backpressure \
                 across distributed schedulers with retries and idempotent handlers \
                 covering cold starts warm pools and autoscaling limits while \
                 reviewers debate budgets cost envelopes and regional placement \
                 strategies for long running batch and streaming workloads alike "
            )
            .repeat(2)
        };
        let mut docs = vec![
            ("docs/plan.txt", filler(1)),
            ("docs/queue.txt", filler(2)),
            ("docs/cost.txt", filler(3)),
            ("docs/regions.txt", filler(4)),
            ("docs/retry.txt", filler(5)),
        ];
        // THE needle doc: the coined term appears exactly once, mid-text
        docs.push((
            "docs/needle.txt",
            format!(
                "{} the zxqwibble migration gate owns every rollout decision {}",
                filler(6),
                filler(7)
            ),
        ));
        docs
    }

    /// THE F1 acceptance (M3-T9's reason to exist): semantic top-20 MISSES
    /// the coined-term chunk; keyword mode puts it at TOP-1 with the right
    /// document attribution; hybrid (rrf) surfaces it in the fused head.
    /// Runs the documented flow: ingest → `vectorfs optimize`'s engine call
    /// (FTS build) → search in all three modes.
    #[tokio::test]
    async fn f1_coined_term_semantic_misses_keyword_hits_hybrid_fuses() {
        let f = fixture_with(tiny_fts_config(false)).await;
        for (key, body) in f1_corpus() {
            put_source_call(&f, "docs", key, body.into_bytes()).await;
        }
        drive(&f).await;

        // the operator verb's engine call (what `vectorfs optimize` runs)
        let spec = f
            .handle
            .shared()
            .derived_specs("docs")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            f.state
                .storage
                .ensure_fts_index("docs", &spec.index_name, "icu")
                .await
                .unwrap(),
            crate::store::FtsIndexOutcome::Built {
                rows: f
                    .state
                    .storage
                    .count_vectors("docs", &spec.index_name)
                    .await
                    .unwrap(),
                backfilled: 0
            },
            "fresh T9 ingest: every row carried its text at write time"
        );

        // ---- keyword: the coined term is a top-1 hit, attributed right ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"zxqwibble"},"topK":10,
                   "searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "{b}");
        assert_eq!(hits[0]["location"]["key"], json!("docs/needle.txt"), "{b}");
        assert!(
            hits[0]["content"]["text"]
                .as_str()
                .unwrap()
                .contains("zxqwibble"),
            "{b}"
        );
        assert!(hits[0]["score"].as_f64().unwrap() > 0.0, "raw BM25: {b}");
        assert!(
            hits.iter().all(|h| h.get("distance").is_none()),
            "keyword hits carry no vector distance: {b}"
        );
        let needle_key = hits[0]["key"].as_str().unwrap().to_string();

        // ---- semantic: top-20 does NOT contain the coined chunk (F1) ----
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"zxqwibble migration gate"},
                   "topK":20}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let semantic_keys: Vec<&str> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap())
            .collect();
        assert_eq!(semantic_keys.len(), 20, "{b}");
        assert!(
            !semantic_keys.contains(&needle_key.as_str()),
            "the tiny embedder cannot see the coined term — F1's shape: {semantic_keys:?}"
        );

        // ---- hybrid (rrf): BM25 rank-1 contribution fuses it into the head ----
        // (fixture df diagnostics — a calibration input recorded in
        // lexical::select_anchors' history table: all three terms of the
        // query are df=1 in THREE DIFFERENT chunks (the coined sentence
        // straddles the 32-token window), which is exactly the shape that
        // ruled out AND-of-all-anchors gating — the v2 gate takes the
        // verdict HEAD. NOTE the probe strings below are the RAW words;
        // under T17 the candidates carry the analyzer's STEMMED spellings
        // (zxqwibble→zxqwibbl, migration→migrat) — the raw probe still
        // counts the same postings (the query-side stemmer maps both
        // spellings onto the same index token); the COUNT is unaffected by
        // gating)
        for t in ["zxqwibble", "migration", "gate"] {
            let (_, _, pb) = call_json(
                &f,
                "/x-vf/Search",
                json!({"bucket":"docs","query":{"text":t},"topK":64,
                       "searchMode":"keyword"}),
            )
            .await;
            eprintln!(
                "f1 fixture df[{t}] = {} (top-1 score {:?})",
                pb["hits"].as_array().map(Vec::len).unwrap_or(0),
                pb["hits"][0]["score"].as_f64()
            );
        }
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"zxqwibble migration gate"},
                   "topK":10,"searchMode":"hybrid"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        // the T17 verdict on this corpus: all three terms are df=1
        // (probed in the analyzer's STEMMED spellings — the index's own
        // tokens), rarest-first with ties in query order — the GATE is the
        // head (zxqwibbl, first-seen among the ties), the fellow anchors
        // ride `should`
        assert_eq!(b["anchors"], json!(["zxqwibbl", "migrat", "gate"]), "{b}");
        let hybrid_keys: Vec<&str> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap())
            .collect();
        let rank = hybrid_keys
            .iter()
            .position(|k| *k == needle_key.as_str())
            .unwrap_or_else(|| panic!("hybrid must surface the needle: {hybrid_keys:?}"));
        assert!(rank < 10, "well inside the fused head: {hybrid_keys:?}");
        // single-route hybrid is a REAL fusion (2 lists) — no passthrough
        // distance, and the score is an rrf sum
        assert!(
            b["hits"].as_array().unwrap()[rank]
                .get("distance")
                .is_none()
        );

        // ---- old-client zero change: absent == explicit "semantic" ----
        let (st, _, explicit) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"capacity planning"},
                   "topK":10,"searchMode":"semantic"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _, implicit) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"capacity planning"},"topK":10}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(explicit, implicit, "searchMode is additive");
    }

    /// CJK through the default icu tokenizer: a Chinese word retrieves its
    /// document in keyword mode (the center-of-gravity corpus, ADR 0002's
    /// default-tokenizer rationale).
    #[tokio::test]
    async fn keyword_mode_cjk_icu_default_hits() {
        let f = fixture_with(tiny_fts_config(false)).await;
        put_source_call(
            &f,
            "docs",
            "docs/zh.txt",
            "这是一份中文文档。北京是这座城市章节的主题，全文围绕北京的历史与规划展开。"
                .as_bytes()
                .to_vec(),
        )
        .await;
        put_source_call(
            &f,
            "docs",
            "docs/en.txt",
            b"an english language document about cities and history".to_vec(),
        )
        .await;
        drive(&f).await;
        let spec = f
            .handle
            .shared()
            .derived_specs("docs")
            .unwrap()
            .pop()
            .unwrap();
        f.state
            .storage
            .ensure_fts_index("docs", &spec.index_name, "icu")
            .await
            .unwrap();

        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"北京"},"topK":5,
                   "searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let hits = b["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "the Chinese word lands: {b}");
        assert_eq!(hits[0]["location"]["key"], json!("docs/zh.txt"), "{b}");
        // and it is SELECTIVE — every hit is the Chinese document, never the
        // english one (the doc chunked into several pieces; both carrying
        // 北京 legitimately match)
        assert!(
            hits.iter()
                .all(|h| h["location"]["key"] == json!("docs/zh.txt")),
            "{b}"
        );
    }

    /// Hybrid under `merge = "weighted"` (M3-T8's weights, naturally
    /// applying to the M3-T9 two-arm fan-out): the lexical arm's find
    /// survives the weighted fusion. `anchor-gating = false` ON PURPOSE:
    /// this test pins the WEIGHTED-MERGE contract decoupled from gate
    /// policy. (Under the v2 count probe this corpus's verdict is
    /// [zxqwibble, migration, gate] — all df=1 — with the head zxqwibble
    /// gating; the f1 acceptance test pins that path.) The gate's own
    /// wire contract is pinned by the anchor_gating_* tests below.
    #[tokio::test]
    async fn hybrid_weighted_merge_keeps_the_lexical_find() {
        let f = fixture_with(tiny_fts_extra_config(
            "merge = \"weighted\"\nmerge-weights = { tiny = 2.5 }\nanchor-gating = false\n",
        ))
        .await;
        for (key, body) in f1_corpus() {
            put_source_call(&f, "docs", key, body.into_bytes()).await;
        }
        drive(&f).await;
        let spec = f
            .handle
            .shared()
            .derived_specs("docs")
            .unwrap()
            .pop()
            .unwrap();
        f.state
            .storage
            .ensure_fts_index("docs", &spec.index_name, "icu")
            .await
            .unwrap();

        // learn the needle key via keyword mode (BM25 rank 1)
        let (_, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"zxqwibble"},"topK":5,
                   "searchMode":"keyword"}),
        )
        .await;
        let needle_key = b["hits"][0]["key"].as_str().unwrap().to_string();

        // weighted hybrid: both arms of the route carry tiny's 2.5 weight
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"zxqwibble migration gate"},
                   "topK":10,"searchMode":"hybrid"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let keys: Vec<&str> = b["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["key"].as_str().unwrap())
            .collect();
        let rank = keys
            .iter()
            .position(|k| *k == needle_key.as_str())
            .unwrap_or_else(|| panic!("weighted hybrid must keep the needle: {keys:?}"));
        assert!(rank < 5, "the 2.5-weighted lexical rank-1 leads: {keys:?}");
    }

    /// The gate faces: unknown mode, keyword+image, and the fts-off bucket
    /// all refuse with field-addressable ValidationExceptions.
    #[tokio::test]
    async fn search_mode_gates_refuse_loudly() {
        // fts OFF bucket (the §7 default): keyword demands the config line
        let f = fixture().await;
        put_source_call(&f, "docs", "docs/a.txt", b"some body text".to_vec()).await;
        drive(&f).await;
        let (st, headers, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"body"},"searchMode":"keyword"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&headers), Some("ValidationException"));
        assert!(b["message"].as_str().unwrap().contains("fts = true"), "{b}");

        // unknown mode: closed set
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"body"},"searchMode":"lexical"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"]
                .as_str()
                .unwrap()
                .contains("unknown searchMode"),
            "{b}"
        );

        // keyword + image: the lexical arm needs text
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"image":{"bytes":"AAAA","mime":"image/png"}},
                   "searchMode":"hybrid"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("needs query.text"),
            "{b}"
        );
    }

    // ---------------- Reindex + GetIngestStatus ----------------

    #[tokio::test]
    async fn reindex_flips_markers_and_tracks_progress() {
        let f = fixture().await;
        put_source_call(&f, "docs", "docs/a.txt", b"first body".to_vec()).await;
        put_source_call(&f, "docs", "docs/b.txt", b"second body".to_vec()).await;
        drive(&f).await;
        for key in ["docs/a.txt", "docs/b.txt"] {
            assert_eq!(
                marker_of(&f, key).await.unwrap().status,
                SourceStatus::Indexed
            );
        }
        let spec = f
            .handle
            .shared()
            .derived_specs("docs")
            .unwrap()
            .pop()
            .unwrap();
        let count_before = f
            .state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();

        let (st, _, b) = call_json(&f, "/x-vf/Reindex", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        let job_id = b["jobId"].as_str().expect("jobId").to_string();
        assert_eq!(job_id.len(), 32, "uuid-style hex: {job_id}");
        assert!(job_id.bytes().all(|c| c.is_ascii_hexdigit()));

        // T7: the old chunks were CLEARED before the rewrite started
        assert_eq!(
            f.state
                .storage
                .count_vectors("docs", &spec.index_name)
                .await
                .unwrap(),
            0,
            "清旧块: old chunks deleted at flip time"
        );

        // every marker flipped: PENDING, evidence cleared, CURRENT fingerprint
        let fp = f.handle.shared().fingerprint_of("docs").unwrap();
        for key in ["docs/a.txt", "docs/b.txt"] {
            let m = marker_of(&f, key).await.unwrap();
            assert_eq!(m.status, SourceStatus::Pending, "{key}");
            assert!(m.chunks.is_none(), "evidence cleared");
            assert_eq!(m.pipeline_fingerprint, fp);
        }

        // progress surfaces via GetIngestStatus (running at 0/2)
        let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let entry = &b["buckets"][0];
        assert_eq!(entry["bucket"], json!("docs"));
        assert_eq!(entry["counts"]["PENDING"], json!(2));
        assert_eq!(entry["counts"]["INDEXED"], json!(0));
        assert_eq!(entry["pendingCount"], json!(2));
        assert!(entry["lastScanAt"].is_string(), "a scan completed");
        assert_eq!(entry["pipelineFingerprint"], json!(fp));
        let derived = entry["derivedIndexes"].as_array().unwrap();
        assert_eq!(derived.len(), 1);
        assert!(derived[0]["index"].as_str().unwrap().starts_with("tiny-"));
        assert_eq!(derived[0]["embedder"], json!("tiny"));
        assert_eq!(derived[0]["model"], json!("tiny-fixture"));
        assert_eq!(derived[0]["dim"], json!(8));
        assert_eq!(entry["reindex"]["jobId"], json!(job_id));
        assert_eq!(entry["reindex"]["status"], json!("running"));
        assert_eq!(entry["reindex"]["progress"], json!({"done":0,"total":2}));
        // stats block present (KB IngestionJobStatistics field names)
        assert_eq!(entry["stats"]["numberOfNewDocumentsIndexed"], json!(2));
        assert_eq!(
            entry["stats"]["numberOfDocumentsScanned"].as_u64().unwrap(),
            2
        );

        // converge: the wake'd scan re-ingests everything
        drive(&f).await;
        let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let entry = &b["buckets"][0];
        assert_eq!(entry["counts"]["INDEXED"], json!(2));
        assert_eq!(entry["reindex"]["status"], json!("done"));
        assert_eq!(entry["reindex"]["progress"], json!({"done":2,"total":2}));
        // rewrite converged to the SAME chunk count, zero residue
        assert_eq!(
            f.state
                .storage
                .count_vectors("docs", &spec.index_name)
                .await
                .unwrap(),
            count_before
        );
        // reindex rewrites count as MODIFIED, not new
        assert_eq!(entry["stats"]["numberOfModifiedDocumentsIndexed"], json!(2));
        assert_eq!(entry["stats"]["numberOfNewDocumentsIndexed"], json!(2));
    }

    /// T7 GC over the wire: DeleteSource 202 → the woken scan reaps chunks +
    /// marker in the SAME pass; ListSources forgets the source and the KB
    /// deleted counter advances.
    #[tokio::test]
    async fn delete_source_202_converges_to_reaped() {
        let f = fixture().await;
        put_source_call(&f, "docs", "docs/keep.txt", b"the keeper body".to_vec()).await;
        put_source_call(&f, "docs", "docs/gone.txt", b"the doomed body".to_vec()).await;
        drive(&f).await;
        let spec = f
            .handle
            .shared()
            .derived_specs("docs")
            .unwrap()
            .pop()
            .unwrap();
        let total = f
            .state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        let gone_chunks = marker_of(&f, "docs/gone.txt")
            .await
            .unwrap()
            .chunks
            .unwrap()["text"] as usize;
        assert!(total > gone_chunks);

        let (st, _, b) = call_json(
            &f,
            "/x-vf/DeleteSource",
            json!({"bucket":"docs","key":"docs/gone.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        assert_eq!(b["status"], json!("DELETING"));

        // the handler's wake is inert on the un-spawned fixture; drive the
        // scan it requested — the DELETING reap happens in THIS pass
        drive(&f).await;

        let (st, _, b) = call_json(&f, "/x-vf/ListSources", json!({"bucket":"docs"})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let keys: Vec<&str> = b["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["key"].as_str())
            .collect();
        assert_eq!(keys, vec!["docs/keep.txt"], "gone.txt fully reaped: {b}");
        assert!(
            marker_of(&f, "docs/gone.txt").await.is_none(),
            "marker deleted with the chunks"
        );
        assert_eq!(
            f.state
                .storage
                .count_vectors("docs", &spec.index_name)
                .await
                .unwrap(),
            total - gone_chunks,
            "vector count reduced exactly"
        );
        // the 404 the poll contract converges to
        let (st, h, _) = call_json(
            &f,
            "/x-vf/GetSource",
            json!({"bucket":"docs","key":"docs/gone.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        let (st, _, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({})).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["buckets"][0]["stats"]["numberOfDocumentsDeleted"],
            json!(1),
            "{}",
            b
        );
    }

    #[tokio::test]
    async fn get_ingest_status_unknown_bucket_400_and_empty_face() {
        let f = fixture().await;
        let (st, h, b) = call_json(&f, "/x-vf/GetIngestStatus", json!({"bucket":"nope"})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));

        // pure protocol face (no ingest sections): the aggregate is []
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            Arc::new(crate::store::Storage::file(dir.path())),
            None,
            "us-east-1".into(),
            "000000000000".into(),
            false,
            None,
        );
        let router = crate::build_router(state);
        let req = Request::post("/x-vf/GetIngestStatus")
            .header("host", "127.0.0.1:8787")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["buckets"], json!([]));
    }

    /// `vf put`'s foreground wait, drive the actual no-marker S3-direct
    /// shape: an object written straight to the instance root (no PutSource)
    /// appears to GetSource as the PENDING transient.
    #[tokio::test]
    async fn s3_direct_object_reports_pending() {
        let f = fixture().await;
        // v3: a "direct s3 put" of key docs/direct.txt lands inside the
        // source bucket dir, under the docs/ key prefix
        std::fs::create_dir_all(f.dir.path().join("docs").join("docs")).unwrap();
        std::fs::write(f.dir.path().join("docs/docs/direct.txt"), b"raw s3 put").unwrap();
        let (st, _, b) = call_json(
            &f,
            "/x-vf/GetSource",
            json!({"bucket":"docs","key":"docs/direct.txt"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["status"], json!("PENDING"));
    }
}
