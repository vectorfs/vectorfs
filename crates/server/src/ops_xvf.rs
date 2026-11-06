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
use crate::pipeline::{Embedder as _, HitList, Merge, Modality, Query, merge_hitlists};
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

/// `POST /x-vf/Search` (appendix E.2) — the flagship superset op: embed the
/// query once per QUERY-MODALITY-matching embedder, query each derived index
/// (topK, `$`-short-name filter, pre-filter pushdown — the same
/// [`crate::filter`] engine QueryVectors runs), merge with the bucket's
/// configured strategy (M2: rrf), and emit hits shaped like KB's
/// KnowledgeBaseRetrievalResult plus the fan-out fields
/// (`index`/`key`/`distance`).
///
/// Query modalities (appendix E.2 `query{text|image|audio(M4)}`): text since
/// M2; IMAGE since M3-T4 — `{bytes: base64, mime}` (see
/// [`SearchImageInput`]), routed to embedders whose query side declares
/// Image (siglip2's vision tower). A TEXT query still fans out to every
/// text-query embedder — including siglip2's IMAGE index, 文字搜图 (the
/// dual-tower default route; the merge treats it like any second list).
///
/// Hit shape, field by field (appendix E.2):
/// * `content{text?, type: "TEXT"|"IMAGE"}` — `type` follows the CHUNK's
///   modality (`x-vf-modality`; image chunks answer IMAGE whatever the
///   query's modality was); `text` = the stored `x-vf-chunk-text`, omitted
///   when `returnText=false`, `store_chunk_text=false`, or the chunk is an
///   image (image chunks store no text — the source file is the content);
/// * `score` — the merged score. Single route: passthrough of
///   `1/(1+distance)` (monotone in distance, so thresholds stay meaningful —
///   rrf scores only make sense across lists). Multiple routes: the rrf sum;
/// * `distance` — emitted only for single-route queries (appendix E.2:
///   "单路查询时有意义"; a merged hit has no single distance);
/// * `index`/`key` — derived index name + chunk key; attribution is the
///   FIRST route (config order) containing the key when several do;
/// * `location{type:"source", key}` — `key` = the full object key
///   (`x-vf-src-name`) — the回源 join. `page?` is M3+ (pdf page images);
/// * `metadata` — the chunk's stored metadata verbatim.
pub async fn search(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: SearchInput = parse_body(&body)?;
    let handle = require_ingest(&state, &input.bucket)?;
    let shared = handle.shared();

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
            let (bytes, mime) = decode_image_query(&img)?;
            Query::Image(bytes, mime)
        }
    };
    let query_modality = match &query {
        Query::Text(_) => Modality::Text,
        Query::Image(_, _) => Modality::Image,
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
    let return_text = input.return_text.unwrap_or(true);

    // filter: `$` short-name syntax, same compile + non-filterable rejection
    // as QueryVectors (appendix E.5: the two official grammars stay as-is).
    let compiled = match &input.filter {
        None | Some(serde_json::Value::Null) => None,
        Some(f) => Some(
            crate::filter::Filter::compile(f)
                .map_err(|e| ApiError::validation_field("filter", e.0))?,
        ),
    };

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
    let mut lists: Vec<HitList> = Vec::new();
    let mut matched_any = false;
    // key -> (index name, distance, stored metadata); first route wins
    let mut attribution: HashMap<String, (String, f64, Option<serde_json::Value>)> = HashMap::new();
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
                top_k,
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
                        h.distance as f64,
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
    }
    if !matched_any {
        return Err(ApiError::validation(format!(
            "no embedder of bucket {:?} accepts {} queries — check the [embedder.*] query \
             modalities in vectorfs.toml (the siglip2 kind accepts image AND text queries)",
            input.bucket, query_modality
        )));
    }

    let merge = shared.merge_of(&input.bucket).unwrap_or(Merge::Rrf);
    let single_route = lists.len() == 1;
    let mut merged = merge_hitlists(merge, &lists);
    merged.truncate(top_k); // rrf unions can exceed topK across routes

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
            if single_route {
                o["distance"] = json!(distance);
            }
            if let Some(m) = &metadata {
                o["location"] = json!({
                    "type": "source",
                    "key": m.get("x-vf-src-name").cloned().unwrap_or(serde_json::Value::Null),
                });
                o["metadata"] = m.clone();
            }
            o
        })
        .collect();
    Ok(Json(json!({ "hits": hits })).into_response())
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
/// lastScanAt, the active pipeline fingerprint, the derived-index table and
/// the latest reindex job with live progress. Omitted bucket = every
/// configured bucket.
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
        for m in &markers {
            *counts.entry(m.status.as_str()).or_insert(0) += 1;
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

        // filter on a non-filterable key -> 400 (QueryVectors semantics)
        let (st, _, b) = call_json(
            &f,
            "/x-vf/Search",
            json!({"bucket":"docs","query":{"text":"x"},
                   "filter":{"x-vf-src-name":"docs/fox.txt"},"topK":1}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["message"].as_str().unwrap().contains("non-filterable"),
            "{b}"
        );

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
