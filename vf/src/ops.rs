//! S3 Vectors operation handlers (wire layer). M1 T3 grew the surface from
//! the spike's four operations to eighteen; M1 T4 completes the full
//! twenty-one with the vector data plane: GetVectors / ListVectors /
//! DeleteVectors plus the full PutVectors limit set. Field names are verbatim
//! from the botocore `s3vectors/2025-07-15` model (service-2.json, develop
//! branch, synced 2026-09-30 — NOTES.md has the extraction).
//!
//! M1 T4 wire facts re-verified live against the model (local botocore
//! 1.43.123 + develop branch curl'd 2026-11-04):
//! - `GetOutputVector`/`ListOutputVector` members are exactly
//!   {key (required), data?, metadata?} — there is NO `status` member
//!   anywhere in the model (the task brief anticipated an OK/NOT_FOUND
//!   status; the model wins). Missing GetVectors keys are therefore simply
//!   omitted from `vectors`, never reported per-key — callers diff requested
//!   vs returned keys (AWS user guide + API reference agree).
//! - Both GetVectorsInput and ListVectorsInput carry `returnData` alongside
//!   `returnMetadata` (both default false); each gates its response member.
//! - ListVectors segment pairing (segmentCount ⇔ segmentIndex) is expressed
//!   only in member documentation, not as a model validator — enforced here
//!   as ValidationException, including segmentIndex < segmentCount.
//! - Limit set per AWS "Limitations and restrictions" (user guide): payload
//!   ≤20 MiB, metadata ≤40 KB per vector (filterable + non-filterable) with
//!   ≤50 keys, filterable portion ≤2 KB per vector (not per value), keys
//!   1-100 (get) / 1-500 (delete, put) of length 1-1024.
//!
//! Locators: every management/vector operation accepts name or ARN
//! (model members vectorBucketName|vectorBucketArn, indexName+vectorBucketName
//! |indexArn). Name+ARN both given and inconsistent -> ValidationException.
//! TODO(AWS semantics): the both-given behavior is unverified against real
//! AWS (it may silently prefer one); flagged at each check site.
//!
//! /tags/{resourceArn} wire form (empirically captured against boto3
//! 1.43.123, 2026-11-04): boto3 percent-encodes the WHOLE ARN into one URI
//! label (`:`->%3A, `/`->%2F), so the route accepts a wildcard tail and
//! percent-decodes; the literal multi-segment form
//! (`/tags/arn:aws:...:bucket/vb`) is accepted too for other clients.
//! `tagKeys` serializes as repeated query params (`tagKeys=a&tagKeys=b`).
//!
//! Error wire protocol (design.md §4.3): HTTP status +
//! `X-Amzn-Errortype: <Shape>` header + body `{"__type":"<Shape>","message":"..."}`
//! (both emitted). Shapes: ValidationException=400, NotFoundException=404,
//! ConflictException=409, AccessDeniedException=403, InternalServerException=500.
//! ValidationException carries an optional `fieldList[{path,message}]`
//! (model member) for field-addressable failures (maxResults, enums).
//!
//! TODO(AWS semantics, unverified — see spike/NOTES.md「遗留 TODO」):
//! - exact AWS error message texts (we use descriptive stand-ins);
//! - malformed-JSON request body: real service likely 400 SerializationException-ish,
//!   we map it to ValidationException;
//! - CreateVectorBucket/CreateIndex response: model says {vectorBucketArn}/{indexArn}
//!   only; AWS CLI docs show {vectorBucket}/{index}; we double-emit (SDKs
//!   ignore unknown members);
//! - DeleteVectorBucket on a non-empty bucket: we return 409 ConflictException
//!   (S3 semantic convention; the model admits it) — exact AWS text unverified;
//! - GetVectorBucket never emits `encryptionConfiguration`: CreateVectorBucket
//!   does not store one yet (M1 T-scope), the member stays absent when unset.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use crate::store::{self, BucketMeta, IndexMeta, StoreError, VectorRow};

/// Shared server state (cloned into handlers).
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<store::Storage>,
    pub creds: Option<crate::sigv4::Credentials>,
    pub region: String,
    pub account: String,
    pub trace: bool,
}

// ---------------------------------------------------------------------------
// error protocol
// ---------------------------------------------------------------------------

pub struct ApiError {
    pub status: StatusCode,
    pub shape: &'static str,
    pub message: String,
    /// ValidationException.fieldList[{path,message}] (model member): set for
    /// field-addressable failures (maxResults range, enum values, ...).
    pub field_list: Option<Vec<FieldError>>,
}

#[derive(serde::Serialize)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

impl ApiError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            shape: "ValidationException",
            message: msg.into(),
            field_list: None,
        }
    }
    /// ValidationException carrying a single fieldList entry pointing at `path`.
    pub fn validation_field(path: &str, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self {
            status: StatusCode::BAD_REQUEST,
            shape: "ValidationException",
            message: msg.clone(),
            field_list: Some(vec![FieldError {
                path: path.to_string(),
                message: msg,
            }]),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            shape: "NotFoundException",
            message: msg.into(),
            field_list: None,
        }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            shape: "ConflictException",
            message: msg.into(),
            field_list: None,
        }
    }
    pub fn access_denied(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            shape: "AccessDeniedException",
            message: msg.into(),
            field_list: None,
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            shape: "InternalServerException",
            message: msg.into(),
            field_list: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "__type": self.shape,
            "message": self.message,
        });
        if let Some(fields) = &self.field_list {
            body["fieldList"] = json!(fields);
        }
        (
            self.status,
            [(
                header::HeaderName::from_static("x-amzn-errortype"),
                self.shape,
            )],
            Json(body),
        )
            .into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Conflict(m) => ApiError::conflict(m),
            StoreError::Internal(m) => ApiError::internal(m),
        }
    }
}

/// 200 with empty body (PutVectors-style success; no content-type on empty body).
fn empty_ok() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .expect("static response")
}

fn arn_bucket(state: &AppState, name: &str) -> String {
    format!(
        "arn:aws:s3vectors:{}:{}:bucket/{}",
        state.region, state.account, name
    )
}

fn arn_index(state: &AppState, vb: &str, ix: &str) -> String {
    format!("{}/index/{}", arn_bucket(state, vb), ix)
}

/// epoch seconds + RFC3339 (UTC) for the same instant.
fn now() -> (f64, String) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before 1970");
    let secs = d.as_secs_f64();
    (secs, epoch_to_rfc3339(secs))
}

/// days->civil (Howard Hinnant); no chrono dependency.
fn epoch_to_rfc3339(secs: f64) -> String {
    let millis = (secs.fract() * 1000.0).round().rem_euclid(1000.0) as u32;
    let total = secs.floor() as i64;
    let days = total.div_euclid(86400);
    let sod = total.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        millis
    )
}

// ---------------------------------------------------------------------------
// request shapes (serde: camelCase == botocore member names)
// ---------------------------------------------------------------------------

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::validation(format!("invalid request body: {e}")))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateVectorBucketInput {
    vector_bucket_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NonFilterableKeys {
    non_filterable_metadata_keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateIndexInput {
    vector_bucket_name: Option<String>,
    vector_bucket_arn: Option<String>,
    index_name: String,
    data_type: String,
    dimension: i64,
    distance_metric: String,
    metadata_configuration: Option<NonFilterableKeys>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VectorDataInput {
    float32: Option<Vec<f64>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutInputVector {
    key: String,
    data: VectorDataInput,
    metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutVectorsInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
    vectors: Vec<PutInputVector>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryVectorsInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
    top_k: Option<i64>,
    query_vector: VectorDataInput,
    filter: Option<serde_json::Value>,
    return_distance: Option<bool>,
    return_metadata: Option<bool>,
    // `queryMode` (model member, IndexMode enum) is accepted and ignored:
    // runtime behavior is always pre-filter/ENHANCED (design.md §4.4).
    #[allow(dead_code)]
    query_mode: Option<String>,
}

/// GetVectorsInput (model): keys[1-100], returnData?/returnMetadata?.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetVectorsInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
    keys: Vec<String>,
    return_data: Option<bool>,
    return_metadata: Option<bool>,
}

/// DeleteVectorsInput (model): keys[1-500].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteVectorsInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
    keys: Vec<String>,
}

/// ListVectorsInput (model): maxResults[1-1000] (default 500), nextToken?,
/// segmentCount[1-16] + segmentIndex[0-15] (must appear as a pair —
/// documentation-level constraint in the model, enforced here),
/// returnData?/returnMetadata?.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListVectorsInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
    max_results: Option<i64>,
    next_token: Option<String>,
    segment_count: Option<i64>,
    segment_index: Option<i64>,
    return_data: Option<bool>,
    return_metadata: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketLocatorInput {
    vector_bucket_name: Option<String>,
    vector_bucket_arn: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListVectorBucketsInput {
    max_results: Option<i64>,
    next_token: Option<String>,
    prefix: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListIndexesInput {
    vector_bucket_name: Option<String>,
    vector_bucket_arn: Option<String>,
    max_results: Option<i64>,
    next_token: Option<String>,
    prefix: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexLocatorInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutVectorBucketPolicyInput {
    vector_bucket_name: Option<String>,
    vector_bucket_arn: Option<String>,
    policy: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutVectorBucketDefaultIndexModeInput {
    vector_bucket_name: Option<String>,
    vector_bucket_arn: Option<String>,
    default_index_mode: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateIndexModeInput {
    vector_bucket_name: Option<String>,
    index_name: Option<String>,
    index_arn: Option<String>,
    index_mode: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TagResourceInput {
    tags: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// validation helpers
// ---------------------------------------------------------------------------

/// 3-63 chars, [a-z0-9-], must start/end alphanumeric (AWS bucket/index name rule;
/// model itself only encodes min/max 3/63 — TODO: verify whether dots are accepted).
fn validate_name(kind: &str, name: &str) -> Result<(), ApiError> {
    let ok = (3..=63).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(ApiError::validation(format!(
            "{kind} {name:?} is invalid: must be 3-63 characters, lowercase letters, digits and \
             hyphens, starting and ending with an alphanumeric character"
        )))
    }
}

/// f64 (JSON) -> f32 with AWS fold semantics: reject NaN/Inf before AND after cast
/// (large finite f64s overflow to f32 infinity).
fn to_f32(v: f64) -> Result<f32, ApiError> {
    if !v.is_finite() {
        return Err(ApiError::validation(
            "vector data must contain only finite floating point values (NaN/Infinity not allowed)",
        ));
    }
    let f = v as f32;
    if !f.is_finite() {
        return Err(ApiError::validation(
            "vector value out of float32 range (folds to Infinity)",
        ));
    }
    Ok(f)
}

fn check_vector(raw: &[f64], dim: i64, metric: &str, what: &str) -> Result<Vec<f32>, ApiError> {
    if raw.len() as i64 != dim {
        return Err(ApiError::validation(format!(
            "{what} has dimension {} but index dimension is {dim}",
            raw.len()
        )));
    }
    let out: Vec<f32> = raw.iter().map(|v| to_f32(*v)).collect::<Result<_, _>>()?;
    if metric == "cosine" && out.iter().all(|v| *v == 0.0) {
        return Err(ApiError::validation(format!(
            "{what} is a zero vector; zero vectors are not allowed for the cosine distance metric"
        )));
    }
    Ok(out)
}

/// Validate a request key list: 1..=max entries (model list min/max) and
/// VectorKey length 1-1024 for each entry.
fn validate_key_list(keys: &[String], what: &str, max: usize) -> Result<(), ApiError> {
    if keys.is_empty() || keys.len() > max {
        return Err(ApiError::validation_field(
            what,
            format!(
                "{what} must contain between 1 and {max} items, got {}",
                keys.len()
            ),
        ));
    }
    for k in keys {
        if k.is_empty() || k.len() > 1024 {
            return Err(ApiError::validation_field(
                what,
                format!("vector key {:?} must be 1-1024 characters", k),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PutVectors metadata limits (design.md §4.5; AWS user guide "Limitations and
// restrictions" + "Metadata filtering", sizes as compact-JSON UTF-8 bytes)
// ---------------------------------------------------------------------------

const METADATA_MAX_BYTES: usize = 40 * 1024; // total (filterable + non-filterable) per vector
const METADATA_MAX_KEYS: usize = 50;
const METADATA_FILTERABLE_MAX_BYTES: usize = 2 * 1024; // filterable portion per vector

/// One JSON-type name for error messages.
fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "object",
    }
}

/// Metadata value types: string / number / boolean, or a flat list of those
/// primitives (the exact set AWS documents as supported; nested objects and
/// nulls are rejected with a message naming the legal types).
fn validate_metadata_value(key: &str, v: &serde_json::Value) -> Result<(), ApiError> {
    const LEGAL: &str = "string, number, boolean, or a list of those values";
    match v {
        serde_json::Value::String(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_) => Ok(()),
        serde_json::Value::Array(items) => {
            for item in items {
                if !matches!(
                    item,
                    serde_json::Value::String(_)
                        | serde_json::Value::Number(_)
                        | serde_json::Value::Bool(_)
                ) {
                    return Err(ApiError::validation(format!(
                        "metadata[{key:?}] list elements must be {LEGAL}, got {}",
                        json_type(item)
                    )));
                }
            }
            Ok(())
        }
        other => Err(ApiError::validation(format!(
            "metadata[{key:?}] must be {LEGAL}, got {}",
            json_type(other)
        ))),
    }
}

/// Full per-vector metadata validation. `non_filterable` is the index's
/// declared `nonFilterableMetadataKeys`: keys in that set are exempt from the
/// 2 KB filterable cap (they ride the 40 KB total as blobs — our storage
/// already keeps every value in `metadata_json` and reads it back whole, so
/// "blob storage" is the natural state). AWS's 2 KB limit is documented as
/// "Filterable metadata per vector" — the combined filterable portion, which
/// subsumes any per-value reading.
fn validate_metadata(
    meta: &serde_json::Value,
    non_filterable: Option<&[String]>,
) -> Result<(), ApiError> {
    let serde_json::Value::Object(map) = meta else {
        return Err(ApiError::validation(format!(
            "metadata must be a JSON object of key-value pairs, got {}",
            json_type(meta)
        )));
    };
    if map.len() > METADATA_MAX_KEYS {
        return Err(ApiError::validation(format!(
            "metadata must contain at most {METADATA_MAX_KEYS} keys, got {}",
            map.len()
        )));
    }
    let total = serde_json::to_vec(meta)
        .map_err(|e| ApiError::internal(format!("metadata serialization: {e}")))?;
    if total.len() > METADATA_MAX_BYTES {
        return Err(ApiError::validation(format!(
            "metadata is {} bytes; total metadata per vector is limited to 40 KB \
             (filterable + non-filterable)",
            total.len()
        )));
    }
    let mut filterable = serde_json::Map::new();
    for (k, v) in map {
        validate_metadata_value(k, v)?;
        if !non_filterable.is_some_and(|ks| ks.iter().any(|nf| nf == k)) {
            filterable.insert(k.clone(), v.clone());
        }
    }
    if !filterable.is_empty() {
        let f = serde_json::to_vec(&serde_json::Value::Object(filterable))
            .map_err(|e| ApiError::internal(format!("metadata serialization: {e}")))?;
        if f.len() > METADATA_FILTERABLE_MAX_BYTES {
            return Err(ApiError::validation(format!(
                "filterable metadata is {} bytes; filterable metadata per vector is limited \
                 to 2 KB (store larger values under keys declared in the index's \
                 metadataConfiguration.nonFilterableMetadataKeys)",
                f.len()
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ListVectors segments & page budget
// ---------------------------------------------------------------------------

/// Approximate page budget for ListVectors (AWS: "If the processed dataset
/// size exceeds 1 MB before reaching the maxResults value, the operation
/// stops and returns the vectors that are retrieved up to that point, along
/// with a nextToken"). Measured as the cumulative compact-JSON size of the
/// emitted vector entries — an approximation of "processed dataset size";
/// TODO(AWS 对拍): exact accounting AWS applies (does it count the request
/// projection? delimiters?) is unverified.
const LIST_PAGE_BUDGET: usize = 1024 * 1024;

/// FNV-1a 64-bit over the key's UTF-8 bytes, reduced modulo segmentCount —
/// the deterministic ListVectors segment hash. Properties we pin in tests:
/// one key always lands in the same segment (pure function of the bytes),
/// and the segments over 0..segmentCount partition the key space exactly
/// (union = full set, pairwise disjoint) — so parallel workers neither
/// duplicate nor drop keys. FNV-1a specifically: no dependencies, byte-exact
/// specification, stable across releases/processes (unlike a randomly-seeded
/// hash — note Rust's own DefaultHasher is NOT stable and must not be used).
fn segment_of(key: &str, segment_count: i64) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    (h % segment_count as u64) as i64
}

/// Locate bucket -> index, mapping misses to 404 before any engine work.
async fn resolve_index(state: &AppState, vb: &str, ix: &str) -> Result<IndexMeta, ApiError> {
    if state.storage.load_bucket(vb).await?.is_none() {
        return Err(ApiError::not_found(format!(
            "vector bucket {vb} does not exist"
        )));
    }
    match state.storage.load_index(vb, ix).await? {
        None => Err(ApiError::not_found(format!(
            "index {ix} does not exist in vector bucket {vb}"
        ))),
        Some(meta) => Ok(meta),
    }
}

// ---------------------------------------------------------------------------
// ARN parsing & locators (M1 T3)
// ---------------------------------------------------------------------------

/// Parsed `arn:aws[-partition]:s3vectors:{region}:{account}:bucket/{vb}[/index/{ix}]`.
struct ParsedArn {
    bucket: String,
    index: Option<String>,
}

/// Parse and validate an s3vectors ARN against the server configuration.
///
/// Strictness (deliberate, M1 T3): service MUST be `s3vectors` and region /
/// account MUST equal the server config — every ARN a client holds was minted
/// by this server (or by real AWS), so a mismatch means the client is pointed
/// at the wrong endpoint and failing early beats a confusing 404. The
/// partition accepts any `aws*` form (`aws`, `aws-cn`, `aws-us-gov`).
/// Names are NOT run through [`validate_name`]: the model's ARN pattern is
/// wider than the name pattern (dots allowed), so ARN names go straight to
/// catalog lookup -> 404 when unknown.
/// TODO(AWS 对拍): real AWS behavior on region-mismatched ARNs (404 vs 400)
/// is unverified.
fn parse_arn(state: &AppState, arn: &str) -> Result<ParsedArn, ApiError> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    let invalid = || ApiError::validation(format!("malformed s3vectors ARN {arn:?}"));
    if parts.len() != 6
        || parts[0] != "arn"
        || !parts[1].starts_with("aws")
        || parts[2] != "s3vectors"
        || parts[3].is_empty()
        || parts[4].is_empty()
    {
        return Err(invalid());
    }
    if parts[3] != state.region {
        return Err(ApiError::validation(format!(
            "ARN region {:?} does not match this server's region {:?} — is the client \
             pointed at the wrong endpoint?",
            parts[3], state.region
        )));
    }
    if parts[4] != state.account {
        return Err(ApiError::validation(format!(
            "ARN account {:?} does not match this server's account {:?}",
            parts[4], state.account
        )));
    }
    let rsegs: Vec<&str> = parts[5].split('/').collect();
    match rsegs.as_slice() {
        ["bucket", vb] if !vb.is_empty() => Ok(ParsedArn {
            bucket: (*vb).to_string(),
            index: None,
        }),
        ["bucket", vb, "index", ix] if !vb.is_empty() && !ix.is_empty() => Ok(ParsedArn {
            bucket: (*vb).to_string(),
            index: Some((*ix).to_string()),
        }),
        _ => Err(invalid()),
    }
}

/// Bucket locator: `vectorBucketName` and/or `vectorBucketArn` -> bucket name.
/// Neither given -> ValidationException; both given and disagreeing ->
/// ValidationException.
/// TODO(AWS 对拍): real AWS may prefer one locator over erroring; unverified.
fn resolve_bucket_locator(
    state: &AppState,
    name: Option<&str>,
    arn: Option<&str>,
) -> Result<String, ApiError> {
    match (name, arn) {
        (None, None) => Err(ApiError::validation(
            "one of vectorBucketName or vectorBucketArn is required",
        )),
        (Some(n), Some(a)) => {
            let parsed = parse_arn(state, a)?;
            if parsed.bucket != n {
                Err(ApiError::validation(format!(
                    "vectorBucketName {n:?} does not match vectorBucketArn {a:?}"
                )))
            } else {
                Ok(n.to_string())
            }
        }
        (Some(n), None) => Ok(n.to_string()),
        (None, Some(a)) => Ok(parse_arn(state, a)?.bucket),
    }
}

/// Index locator: `vectorBucketName`+`indexName` or `indexArn` ->
/// (bucket, index). Cross-checks any redundant members for consistency.
/// TODO(AWS 对拍): same both-given caveat as [`resolve_bucket_locator`].
fn resolve_index_locator(
    state: &AppState,
    vb: Option<&str>,
    ix: Option<&str>,
    ix_arn: Option<&str>,
) -> Result<(String, String), ApiError> {
    match ix_arn {
        None => match (vb, ix) {
            (Some(v), Some(i)) => Ok((v.to_string(), i.to_string())),
            _ => Err(ApiError::validation(
                "either indexArn or vectorBucketName+indexName is required",
            )),
        },
        Some(a) => {
            let parsed = parse_arn(state, a)?;
            let Some(a_ix) = parsed.index else {
                return Err(ApiError::validation(format!(
                    "indexArn {a:?} does not address an index (missing /index/<name>)"
                )));
            };
            if let Some(v) = vb
                && *v != parsed.bucket
            {
                return Err(ApiError::validation(format!(
                    "vectorBucketName {v:?} does not match indexArn {a:?}"
                )));
            }
            if let Some(i) = ix
                && *i != a_ix
            {
                return Err(ApiError::validation(format!(
                    "indexName {i:?} does not match indexArn {a:?}"
                )));
            }
            Ok((parsed.bucket, a_ix))
        }
    }
}

/// Load a bucket meta or 404 (post-locator).
async fn require_bucket(state: &AppState, name: &str) -> Result<BucketMeta, ApiError> {
    state
        .storage
        .load_bucket(name)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("vector bucket {name} does not exist")))
}

// ---------------------------------------------------------------------------
// pagination (M1 T3: ListVectorBuckets / ListIndexes)
// ---------------------------------------------------------------------------

/// Opaque base64 cursor `{\"l\": <last emitted name>}`. The page semantics it
/// encodes: "all items with name > l" — stable across pages for inserts made
/// behind the cursor, and cheap to recompute because listing re-enumerates the
/// catalog each call (single-writer, design.md §5.1 — no snapshot needed for
/// management objects; AWS itself documents list calls as eventually
/// consistent anyway).
fn encode_page_token(last_name: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(json!({ "l": last_name }).to_string())
}

fn decode_page_token(token: &str) -> Result<String, ApiError> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(token)
        .map_err(|_| ApiError::validation_field("nextToken", "invalid nextToken"))?;
    let v: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|_| ApiError::validation_field("nextToken", "invalid nextToken"))?;
    v.get("l")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ApiError::validation_field("nextToken", "invalid nextToken"))
}

/// Shared list-page logic: validate maxResults (1-500, default 500) +
/// nextToken, filter by prefix + cursor, sort order comes from the caller
/// (store lists are name-sorted), emit a follow-on token iff items remain.
fn paginate<T, F>(
    items: Vec<T>,
    max_results: Option<i64>,
    next_token: Option<&str>,
    name_of: F,
) -> Result<(Vec<T>, Option<String>), ApiError>
where
    F: Fn(&T) -> &str,
{
    let max = match max_results {
        None => 500,
        Some(n) if (1..=500).contains(&n) => n as usize,
        Some(n) => {
            return Err(ApiError::validation_field(
                "maxResults",
                format!("maxResults must be between 1 and 500, got {n}"),
            ));
        }
    };
    let after = match next_token {
        None | Some("") => None,
        Some(t) => Some(decode_page_token(t)?),
    };
    let mut page: Vec<T> = items
        .into_iter()
        .filter(|i| after.as_deref().is_none_or(|a| name_of(i) > a))
        .take(max + 1) // one lookahead decides whether a next page exists
        .collect();
    let has_more = page.len() > max;
    page.truncate(max);
    let token = match (has_more, page.last()) {
        (true, Some(last)) => Some(encode_page_token(name_of(last))),
        _ => None,
    };
    Ok((page, token))
}

/// Validate a list `prefix` (model: 1-63 chars when present; "" = absent).
fn validate_prefix(prefix: Option<&str>) -> Result<Option<&str>, ApiError> {
    match prefix {
        None | Some("") => Ok(None),
        Some(p) if p.len() <= 63 => Ok(Some(p)),
        Some(p) => Err(ApiError::validation_field(
            "prefix",
            format!("prefix must be 1-63 characters, got {}", p.len()),
        )),
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

pub async fn create_vector_bucket(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: CreateVectorBucketInput = parse_body(&body)?;
    validate_name("vectorBucketName", &input.vector_bucket_name)?;
    let (epoch, rfc3339) = now();
    let meta = BucketMeta {
        vector_bucket_arn: arn_bucket(&state, &input.vector_bucket_name),
        vector_bucket_name: input.vector_bucket_name.clone(),
        creation_time_rfc3339: rfc3339,
        creation_time_epoch: epoch,
        default_index_mode: None,
    };
    state.storage.create_bucket(&meta).await?;

    // Model (botocore 2025-07-15): required member `vectorBucketArn` only.
    // AWS CLI reference documents `vectorBucket` — double-emit, SDKs ignore extras.
    let resp = serde_json::json!({
        "vectorBucketArn": meta.vector_bucket_arn,
        "vectorBucket": {
            "vectorBucketName": meta.vector_bucket_name,
            "vectorBucketArn": meta.vector_bucket_arn,
            "creationTime": epoch,
        }
    });
    Ok(Json(resp).into_response())
}

pub async fn create_index(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: CreateIndexInput = parse_body(&body)?;
    validate_name("indexName", &input.index_name)?;
    if input.data_type != "float32" {
        return Err(ApiError::validation(format!(
            "unsupported dataType {:?} (only float32)",
            input.data_type
        )));
    }
    if !(1..=4096).contains(&input.dimension) {
        return Err(ApiError::validation(format!(
            "dimension must be between 1 and 4096, got {}",
            input.dimension
        )));
    }
    if !matches!(input.distance_metric.as_str(), "cosine" | "euclidean") {
        return Err(ApiError::validation(format!(
            "distanceMetric must be cosine or euclidean, got {:?}",
            input.distance_metric
        )));
    }
    if let Some(cfg) = &input.metadata_configuration {
        let ks = &cfg.non_filterable_metadata_keys;
        if ks.is_empty() || ks.len() > 10 {
            return Err(ApiError::validation(
                "metadataConfiguration.nonFilterableMetadataKeys must contain 1-10 keys",
            ));
        }
        for k in ks {
            if k.is_empty() || k.len() > 63 {
                return Err(ApiError::validation(format!(
                    "non-filterable metadata key {k:?} must be 1-63 characters"
                )));
            }
        }
    }

    let vb = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    let bucket_meta = require_bucket(&state, &vb).await?;

    let (epoch, rfc3339) = now();
    let meta = IndexMeta {
        vector_bucket_name: vb.clone(),
        index_name: input.index_name.clone(),
        index_arn: arn_index(&state, &vb, &input.index_name),
        creation_time_rfc3339: rfc3339,
        creation_time_epoch: epoch,
        data_type: "float32".into(),
        dimension: input.dimension as i32,
        distance_metric: input.distance_metric.clone(),
        non_filterable_metadata_keys: input
            .metadata_configuration
            .map(|c| c.non_filterable_metadata_keys),
        // Echo-only knob (§4.4): inherits the bucket default; ENHANCED when
        // unset (matches AWS's post-2026-09-30 default and our always-prefilter
        // runtime). Runtime behavior is identical for both values.
        index_mode: Some(
            bucket_meta
                .default_index_mode
                .clone()
                .unwrap_or_else(|| "ENHANCED".into()),
        ),
    };
    state.storage.create_index(&meta).await?;

    let resp = serde_json::json!({
        "indexArn": meta.index_arn,          // model-required member
        "index": index_full_json(&meta),     // documented shape; SDKs ignore extras
    });
    Ok(Json(resp).into_response())
}

/// Full `Index` shape (model: name/arn/creationTime/dataType/dimension/
/// distanceMetric required + metadataConfiguration/indexMode when set).
fn index_full_json(meta: &IndexMeta) -> serde_json::Value {
    let mut o = json!({
        "vectorBucketName": meta.vector_bucket_name,
        "indexName": meta.index_name,
        "indexArn": meta.index_arn,
        "creationTime": meta.creation_time_epoch,
        "dataType": meta.data_type,
        "dimension": meta.dimension,
        "distanceMetric": meta.distance_metric,
    });
    if let Some(ks) = &meta.non_filterable_metadata_keys {
        o["metadataConfiguration"] = json!({ "nonFilterableMetadataKeys": ks });
    }
    if let Some(mode) = &meta.index_mode {
        o["indexMode"] = json!(mode);
    }
    o
}

pub async fn put_vectors(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: PutVectorsInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    let meta = resolve_index(&state, &vb, &ix).await?;

    // Payload cap (AWS: "Request payload size: Up to 20 MiB"). Checked after
    // the body is fully buffered: the gate middleware + DefaultBodyLimit sit
    // at 21 MiB so an oversized-but-deliverable body reaches HERE and gets a
    // 400 ValidationException instead of axum's 413. Rationale (botocore
    // checked 2026-11-04): rest-json error parsing keys off the
    // X-Amzn-Errortype/__type shape, and 413 has no modeled shape here — a
    // typed SDK surfaces it as an unparsed transport error rather than
    // client.exceptions.ValidationException.
    if body.len() > 20 * 1024 * 1024 {
        return Err(ApiError::validation(format!(
            "request body is {} bytes; the maximum request payload size is 20 MiB",
            body.len()
        )));
    }

    if input.vectors.is_empty() || input.vectors.len() > 500 {
        return Err(ApiError::validation(format!(
            "vectors must contain between 1 and 500 items, got {}",
            input.vectors.len()
        )));
    }

    // Same-batch duplicate keys: the LAST occurrence wins (TODO(AWS 对拍):
    // real-service behavior for duplicate keys in one PutVectors batch is
    // undocumented; merge_insert sources must be key-unique, so we resolve
    // duplicates before the engine ever sees them).
    let mut rows: Vec<VectorRow> = Vec::with_capacity(input.vectors.len());
    let mut index_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for v in input.vectors {
        if v.key.is_empty() || v.key.len() > 1024 {
            return Err(ApiError::validation(format!(
                "vector key {:?} must be 1-1024 characters",
                v.key
            )));
        }
        let Some(raw) = v.data.float32 else {
            return Err(ApiError::validation(format!(
                "vectors[{:?}].data.float32 is required",
                v.key
            )));
        };
        let vec = check_vector(&raw, meta.dimension as i64, &meta.distance_metric, "vector")?;
        // metadata: whole-document replacement on upsert (AWS semantics assumed;
        // omitted metadata stores NULL — TODO: confirm against real service).
        let metadata_json = match &v.metadata {
            None | Some(serde_json::Value::Null) => None,
            Some(m) => {
                validate_metadata(m, meta.non_filterable_metadata_keys.as_deref())?;
                Some(m.to_string())
            }
        };
        let row = VectorRow {
            key: v.key.clone(),
            vector: vec,
            metadata_json,
        };
        match index_of.get(&v.key) {
            Some(&i) => rows[i] = row, // later duplicate replaces the earlier row
            None => {
                index_of.insert(v.key, rows.len());
                rows.push(row);
            }
        }
    }

    state
        .storage
        .upsert_vectors(&vb, &ix, meta.dimension, rows)
        .await?;
    Ok(empty_ok())
}

pub async fn query_vectors(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: QueryVectorsInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    let meta = resolve_index(&state, &vb, &ix).await?;

    let Some(top_k) = input.top_k else {
        return Err(ApiError::validation("topK is required"));
    };
    if !(1..=10_000).contains(&top_k) {
        return Err(ApiError::validation(format!(
            "topK must be between 1 and 10000, got {top_k}"
        )));
    }
    let Some(raw_q) = input.query_vector.float32 else {
        return Err(ApiError::validation("queryVector.float32 is required"));
    };
    // TODO: zero query vector under cosine — assumed rejected like put vectors
    // (real AWS behavior unverified).
    let q = check_vector(
        &raw_q,
        meta.dimension as i64,
        &meta.distance_metric,
        "queryVector",
    )?;

    // filter: top-level multi-key implicit AND + $eq + $and; anything else rejected.
    // TODO(M1): full AWS operator set (design.md §4.6) — subset kept from spike.
    let compiled_filter = match &input.filter {
        None | Some(serde_json::Value::Null) => None,
        Some(f) => {
            validate_filter_supported(f)?;
            Some(f.clone())
        }
    };
    let filter_fn = compiled_filter
        .as_ref()
        .map(|f| eval_filter_closure(f.clone()));

    let hits = state
        .storage
        .query_vectors(
            &vb,
            &ix,
            meta.dimension,
            &meta.distance_metric,
            &q,
            top_k as usize,
            filter_fn.as_deref(),
        )
        .await?;

    let return_distance = input.return_distance.unwrap_or(false);
    let return_metadata = input.return_metadata.unwrap_or(false);

    let vectors: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|h| {
            let mut o = serde_json::json!({ "key": h.key });
            if return_distance {
                o["distance"] = serde_json::json!(h.distance as f64);
            }
            if return_metadata && let Some(m) = h.metadata {
                o["metadata"] = m;
            }
            o
        })
        .collect();

    let resp = serde_json::json!({
        "vectors": vectors,
        "distanceMetric": meta.distance_metric,
    });
    Ok(Json(resp).into_response())
}

// ---------------------------------------------------------------------------
// vector data plane (M1 T4): GetVectors / DeleteVectors / ListVectors
// ---------------------------------------------------------------------------

/// Serialize a stored vector for Get/List output: {key, data?{float32},
/// metadata?} — members gated by returnData/returnMetadata, absent members
/// omitted (model: only `key` is required on GetOutputVector/ListOutputVector).
fn vector_entry_json(
    v: &crate::store::FoundVector,
    return_data: bool,
    return_metadata: bool,
) -> serde_json::Value {
    let mut o = json!({ "key": v.key });
    if return_data {
        // f32 values widen to f64 for JSON — matches the AWS fold semantics
        // already applied at ingest (float32 stored, float32 echoed).
        o["data"] = json!({ "float32": v.vector });
    }
    if return_metadata && let Some(m) = &v.metadata {
        o["metadata"] = m.clone();
    }
    o
}

/// GetVectors: batch point-lookup, keys[1-100]. Response contains one entry
/// per FOUND key (missing keys are omitted — the model has no per-entry
/// status member; verified 2026-11-04, see module doc), in request order,
/// deduplicated. TODO(AWS 对拍): response ordering is undocumented; we
/// preserve the caller's key order as the deterministic choice.
pub async fn get_vectors(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: GetVectorsInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    resolve_index(&state, &vb, &ix).await?;
    validate_key_list(&input.keys, "keys", 100)?;

    let found = state.storage.get_vectors(&vb, &ix, &input.keys).await?;
    let mut by_key = std::collections::HashMap::with_capacity(found.len());
    for f in found {
        by_key.insert(f.key.clone(), f);
    }
    let return_data = input.return_data.unwrap_or(false);
    let return_metadata = input.return_metadata.unwrap_or(false);
    let mut seen = std::collections::HashSet::new();
    let vectors: Vec<serde_json::Value> = input
        .keys
        .iter()
        .filter(|k| seen.insert((*k).clone()))
        .filter_map(|k| by_key.get(k))
        .map(|v| vector_entry_json(v, return_data, return_metadata))
        .collect();
    Ok(Json(json!({ "vectors": vectors })).into_response())
}

/// DeleteVectors: keys[1-500] -> 200 empty. Keys are bound into an
/// arrow/datafusion IN predicate in the store (never SQL text) — arbitrary
/// UTF-8 keys cannot inject or escape. Absent keys are idempotent no-ops;
/// a later PutVectors of a deleted key rebuilds the row normally (merge_insert
/// insert path).
pub async fn delete_vectors(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: DeleteVectorsInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    resolve_index(&state, &vb, &ix).await?;
    validate_key_list(&input.keys, "keys", 500)?;

    state.storage.delete_vectors(&vb, &ix, &input.keys).await?;
    Ok(empty_ok())
}

/// ListVectors: key-ascending pagination (maxResults 1-1000, default 500)
/// with optional parallel segments (segmentCount 1-16 + segmentIndex 0-15,
/// strictly paired, segmentIndex < segmentCount) and an approximate 1 MiB
/// page budget. nextToken = base64 cursor {last_key}; the next page scans
/// `key > last_key`, re-applies the segment filter, and continues.
pub async fn list_vectors(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: ListVectorsInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    resolve_index(&state, &vb, &ix).await?;

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
        Some(t) => Some(decode_page_token(t)?),
    };
    // Pairing is documentation-level in the model ("If you specify a
    // segmentCount value, you must also specify segmentIndex" and vice
    // versa) — there is no validator trait, so the check lives here.
    let segment = match (input.segment_count, input.segment_index) {
        (None, None) => None,
        (Some(_), None) => {
            return Err(ApiError::validation_field(
                "segmentIndex",
                "segmentIndex is required when segmentCount is specified",
            ));
        }
        (None, Some(_)) => {
            return Err(ApiError::validation_field(
                "segmentCount",
                "segmentCount is required when segmentIndex is specified",
            ));
        }
        (Some(c), Some(i)) => {
            if !(1..=16).contains(&c) {
                return Err(ApiError::validation_field(
                    "segmentCount",
                    format!("segmentCount must be between 1 and 16, got {c}"),
                ));
            }
            if !(0..=15).contains(&i) || i >= c {
                return Err(ApiError::validation_field(
                    "segmentIndex",
                    format!(
                        "segmentIndex must be between 0 and segmentCount-1 ({}), got {i}",
                        c - 1
                    ),
                ));
            }
            Some((c, i))
        }
    };

    let return_data = input.return_data.unwrap_or(false);
    let return_metadata = input.return_metadata.unwrap_or(false);

    let scanned = state
        .storage
        .scan_vectors(&vb, &ix, after.as_deref())
        .await?;
    let candidates: Vec<&crate::store::FoundVector> = scanned
        .iter()
        .filter(|v| segment.is_none_or(|(c, i)| segment_of(&v.key, c) == i))
        .collect();

    // Walk in key order; stop at maxResults or when the NEXT complete entry
    // would push the page past the 1 MiB budget (never mid-vector: the
    // entry being cut is simply left for the next page). A token is emitted
    // iff an item was actually left behind.
    let mut vectors: Vec<serde_json::Value> = Vec::new();
    let mut last_key: Option<String> = None;
    let mut total = 0usize;
    let mut token = None;
    for v in &candidates {
        if vectors.len() >= max {
            token = last_key.clone();
            break;
        }
        let entry = vector_entry_json(v, return_data, return_metadata);
        let size = entry.to_string().len(); // compact JSON bytes — see LIST_PAGE_BUDGET note
        if !vectors.is_empty() && total + size > LIST_PAGE_BUDGET {
            token = last_key.clone();
            break;
        }
        total += size;
        last_key = Some(v.key.clone());
        vectors.push(entry);
    }

    let mut resp = json!({ "vectors": vectors });
    if let Some(t) = token {
        resp["nextToken"] = json!(encode_page_token(&t));
    }
    Ok(Json(resp).into_response())
}

// ---------------------------------------------------------------------------
// management plane (M1 T3): buckets
// ---------------------------------------------------------------------------

/// GetVectorBucket -> {vectorBucket: VectorBucket} (model-required member).
pub async fn get_vector_bucket(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: BucketLocatorInput = parse_body(&body)?;
    let name = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    let meta = require_bucket(&state, &name).await?;
    Ok(Json(json!({ "vectorBucket": bucket_full_json(&meta) })).into_response())
}

/// Full `VectorBucket` shape (model: name/arn/creationTime required +
/// defaultIndexMode when set; encryptionConfiguration is never emitted —
/// CreateVectorBucket does not store one yet, see module TODO).
fn bucket_full_json(meta: &BucketMeta) -> serde_json::Value {
    let mut o = json!({
        "vectorBucketName": meta.vector_bucket_name,
        "vectorBucketArn": meta.vector_bucket_arn,
        "creationTime": meta.creation_time_epoch,
    });
    if let Some(mode) = &meta.default_index_mode {
        o["defaultIndexMode"] = json!(mode);
    }
    o
}

/// DeleteVectorBucket -> 200 empty. Non-empty bucket (any index objects)
/// -> 409 ConflictException (S3 semantic convention for delete-non-empty;
/// the model admits ConflictException). TODO(AWS 对拍): exact AWS text +
/// whether AWS cleans up or refuses; unverified against the real service.
pub async fn delete_vector_bucket(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: BucketLocatorInput = parse_body(&body)?;
    let name = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    require_bucket(&state, &name).await?;
    if state.storage.bucket_has_indexes(&name).await? {
        return Err(ApiError::conflict(format!(
            "vector bucket {name} still contains vector indexes; delete them first"
        )));
    }
    // bucket.json + optional policy.json/tags.json (index metadata dirs are
    // empty by the check above — nothing else may live under buckets/<name>/).
    state.storage.delete_bucket(&name).await?;
    Ok(empty_ok())
}

/// ListVectorBuckets -> {vectorBuckets: [VectorBucketSummary], nextToken?}.
pub async fn list_vector_buckets(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: ListVectorBucketsInput = parse_body(&body)?;
    let prefix = validate_prefix(input.prefix.as_deref())?;
    let all = state.storage.list_buckets().await?;
    let filtered: Vec<BucketMeta> = match prefix {
        Some(p) => all
            .into_iter()
            .filter(|m| m.vector_bucket_name.starts_with(p))
            .collect(),
        None => all,
    };
    let (page, next) = paginate(
        filtered,
        input.max_results,
        input.next_token.as_deref(),
        |m| &m.vector_bucket_name,
    )?;
    let buckets: Vec<serde_json::Value> = page
        .iter()
        .map(|m| {
            // VectorBucketSummary: name/arn/creationTime only (model).
            json!({
                "vectorBucketName": m.vector_bucket_name,
                "vectorBucketArn": m.vector_bucket_arn,
                "creationTime": m.creation_time_epoch,
            })
        })
        .collect();
    let mut resp = json!({ "vectorBuckets": buckets });
    if let Some(t) = next {
        resp["nextToken"] = json!(t);
    }
    Ok(Json(resp).into_response())
}

// ---------------------------------------------------------------------------
// management plane (M1 T3): indexes
// ---------------------------------------------------------------------------

/// GetIndex -> {index: Index} (model-required member), full Index shape.
pub async fn get_index(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: IndexLocatorInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    let meta = resolve_index(&state, &vb, &ix).await?;
    Ok(Json(json!({ "index": index_full_json(&meta) })).into_response())
}

/// DeleteIndex -> 200 empty: index.json + tags.json + every Lance dataset
/// object under `indexes/<ix>/data/` (§5.3). Idempotent-404 on missing.
pub async fn delete_index(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: IndexLocatorInput = parse_body(&body)?;
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    resolve_index(&state, &vb, &ix).await?;
    state.storage.delete_index(&vb, &ix).await?;
    Ok(empty_ok())
}

/// ListIndexes -> {indexes: [IndexSummary], nextToken?}, by bucket locator.
pub async fn list_indexes(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: ListIndexesInput = parse_body(&body)?;
    let vb = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    require_bucket(&state, &vb).await?;
    let prefix = validate_prefix(input.prefix.as_deref())?;
    let all = state.storage.list_indexes(&vb).await?;
    let filtered: Vec<IndexMeta> = match prefix {
        Some(p) => all
            .into_iter()
            .filter(|m| m.index_name.starts_with(p))
            .collect(),
        None => all,
    };
    let (page, next) = paginate(
        filtered,
        input.max_results,
        input.next_token.as_deref(),
        |m| &m.index_name,
    )?;
    let indexes: Vec<serde_json::Value> = page
        .iter()
        .map(|m| {
            // IndexSummary: bucketName/indexName/arn/creationTime only (model).
            json!({
                "vectorBucketName": m.vector_bucket_name,
                "indexName": m.index_name,
                "indexArn": m.index_arn,
                "creationTime": m.creation_time_epoch,
            })
        })
        .collect();
    let mut resp = json!({ "indexes": indexes });
    if let Some(t) = next {
        resp["nextToken"] = json!(t);
    }
    Ok(Json(resp).into_response())
}

// ---------------------------------------------------------------------------
// management plane (M1 T3): bucket policy (§4.4: store & echo, NEVER enforce)
// ---------------------------------------------------------------------------

/// PutVectorBucketPolicy: idempotent overwrite, policy stored as the raw
/// bytes received. boto3 sends `policy` as a JSON *string* (model shape
/// VectorBucketPolicy is `string`); a JSON object is accepted too for bare
/// HTTP clients (canonicalized to compact JSON). Byte-faithful echo on Get.
pub async fn put_vector_bucket_policy(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: PutVectorBucketPolicyInput = parse_body(&body)?;
    let name = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    require_bucket(&state, &name).await?;
    let raw = match input.policy {
        serde_json::Value::String(s) => {
            // must be a JSON document (AWS validates policy syntax; we check
            // parseability only — no structural or enforcement validation)
            serde_json::from_str::<serde_json::Value>(&s).map_err(|e| {
                ApiError::validation_field("policy", format!("policy is not valid JSON: {e}"))
            })?;
            s
        }
        serde_json::Value::Object(_) => input.policy.to_string(),
        other => {
            return Err(ApiError::validation_field(
                "policy",
                format!("policy must be a JSON document (string or object), got {other:?}"),
            ));
        }
    };
    state.storage.put_policy(&name, &raw).await?;
    Ok(empty_ok())
}

/// GetVectorBucketPolicy -> {policy: <string>} (model: policy is a string).
/// No policy stored -> 404 NotFoundException (model lists it).
pub async fn get_vector_bucket_policy(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: BucketLocatorInput = parse_body(&body)?;
    let name = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    require_bucket(&state, &name).await?;
    match state.storage.load_policy(&name).await? {
        Some(policy) => Ok(Json(json!({ "policy": policy })).into_response()),
        None => Err(ApiError::not_found(format!(
            "vector bucket {name} has no policy"
        ))),
    }
}

/// DeleteVectorBucketPolicy -> 200 empty; idempotent (missing policy on an
/// existing bucket is NOT an error — S3 DeleteBucketPolicy convention).
pub async fn delete_vector_bucket_policy(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: BucketLocatorInput = parse_body(&body)?;
    let name = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    require_bucket(&state, &name).await?;
    state.storage.delete_policy(&name).await?;
    Ok(empty_ok())
}

// ---------------------------------------------------------------------------
// management plane (M1 T3): tags — POST/DELETE/GET /tags/{resourceArn}
// ---------------------------------------------------------------------------

/// Extract the resourceArn from `/tags/{arn}`. The tail is percent-decoded
/// RFC3986-style ('+' stays literal — path, not form). Two wire forms are
/// accepted (route is a wildcard):
/// - boto3: one fully-encoded label `/tags/arn%3Aaws%3A…%2Fvb` (captured
///   empirically, see module doc);
/// - literal multi-segment `/tags/arn:aws:…:bucket/vb`.
fn resource_arn_from_uri(uri: &Uri) -> Result<String, ApiError> {
    let tail = uri
        .path()
        .strip_prefix("/tags/")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::validation("resourceArn is required in the /tags/ path"))?;
    percent_encoding::percent_decode_str(tail)
        .decode_utf8()
        .map(|c| c.into_owned())
        .map_err(|_| ApiError::validation("resourceArn is not valid UTF-8"))
}

/// Decode one query pair component: form convention ('+' = space — what the
/// Go SDK's QueryEscape emits; boto3 always sends %20 so both agree there)
/// plus %XX otherwise.
fn query_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

/// All values of a repeated query key (`tagKeys=a&tagKeys=b` — the form
/// boto3 emits; a single `tagKeys=a,b` also splits on commas for tolerance).
fn query_values(raw: Option<&str>, want: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(q) = raw else {
        return out;
    };
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if query_decode(k) == want {
            out.extend(query_decode(v).split(',').map(str::to_string));
        }
    }
    out
}

/// Validate a tags map (TagKey 1-128, TagValue 0-256, values must be strings;
/// charset pattern in the model is NOT enforced — TODO(AWS 对拍)).
fn validate_tags(tags: &serde_json::Map<String, serde_json::Value>) -> Result<(), ApiError> {
    for (k, v) in tags {
        if k.is_empty() || k.len() > 128 {
            return Err(ApiError::validation_field(
                "tags",
                format!("tag key {k:?} must be 1-128 characters"),
            ));
        }
        let serde_json::Value::String(v) = v else {
            return Err(ApiError::validation_field(
                "tags",
                format!("tag {k:?} value must be a string"),
            ));
        };
        if v.len() > 256 {
            return Err(ApiError::validation_field(
                "tags",
                format!("tag {k:?} value must be 0-256 characters"),
            ));
        }
    }
    Ok(())
}

/// ARN -> existing (bucket, Option<index>), 404 on either miss.
async fn require_tag_target(
    state: &AppState,
    arn: &str,
) -> Result<(String, Option<String>), ApiError> {
    let parsed = parse_arn(state, arn)?;
    require_bucket(state, &parsed.bucket).await?;
    if let Some(ix) = &parsed.index
        && state
            .storage
            .load_index(&parsed.bucket, ix)
            .await?
            .is_none()
    {
        return Err(ApiError::not_found(format!(
            "index {ix} does not exist in vector bucket {}",
            parsed.bucket
        )));
    }
    Ok((parsed.bucket, parsed.index))
}

/// TagResource `POST /tags/{resourceArn}`: merge tags (existing keys
/// overwritten, others kept), cap 10 pairs total per resource.
pub async fn tag_resource(
    State(state): State<AppState>,
    uri: Uri,
    body: Bytes,
) -> Result<Response, ApiError> {
    let arn = resource_arn_from_uri(&uri)?;
    let input: TagResourceInput = parse_body(&body)?;
    let (vb, ix) = require_tag_target(&state, &arn).await?;
    validate_tags(&input.tags)?;
    let mut tags = state.storage.load_tags(&vb, ix.as_deref()).await?;
    for (k, v) in input.tags {
        tags.insert(k, v);
    }
    if tags.len() > 10 {
        return Err(ApiError::validation_field(
            "tags",
            format!("a resource supports at most 10 tags, got {}", tags.len()),
        ));
    }
    state.storage.put_tags(&vb, ix.as_deref(), &tags).await?;
    Ok(empty_ok())
}

/// UntagResource `DELETE /tags/{resourceArn}?tagKeys=k1&k2…` (tagKeys in
/// query). Removing absent keys is a no-op (idempotent).
pub async fn untag_resource(State(state): State<AppState>, uri: Uri) -> Result<Response, ApiError> {
    let arn = resource_arn_from_uri(&uri)?;
    let keys = query_values(uri.query(), "tagKeys");
    if keys.is_empty() {
        return Err(ApiError::validation_field(
            "tagKeys",
            "tagKeys is required (repeated query parameters)",
        ));
    }
    let (vb, ix) = require_tag_target(&state, &arn).await?;
    let mut tags = state.storage.load_tags(&vb, ix.as_deref()).await?;
    for k in keys {
        tags.remove(&k);
    }
    state.storage.put_tags(&vb, ix.as_deref(), &tags).await?;
    Ok(empty_ok())
}

/// ListTagsForResource `GET /tags/{resourceArn}` -> {tags: {...}}; {} when
/// none stored (tags is a model-required member).
pub async fn list_tags_for_resource(
    State(state): State<AppState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let arn = resource_arn_from_uri(&uri)?;
    let (vb, ix) = require_tag_target(&state, &arn).await?;
    let tags = state.storage.load_tags(&vb, ix.as_deref()).await?;
    Ok(Json(json!({ "tags": tags })).into_response())
}

// ---------------------------------------------------------------------------
// management plane (M1 T3): indexMode knobs (echo-only, §4.4 — runtime is
// always pre-filter/ENHANCED regardless of the stored value)
// ---------------------------------------------------------------------------

/// PutVectorBucketDefaultIndexMode: write bucket.json defaultIndexMode;
/// affects only the indexMode echoed for indexes created afterwards.
pub async fn put_vector_bucket_default_index_mode(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: PutVectorBucketDefaultIndexModeInput = parse_body(&body)?;
    if !matches!(input.default_index_mode.as_str(), "CLASSIC" | "ENHANCED") {
        return Err(ApiError::validation_field(
            "defaultIndexMode",
            format!(
                "defaultIndexMode must be CLASSIC or ENHANCED, got {:?}",
                input.default_index_mode
            ),
        ));
    }
    let name = resolve_bucket_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.vector_bucket_arn.as_deref(),
    )?;
    require_bucket(&state, &name).await?;
    state
        .storage
        .set_default_index_mode(&name, &input.default_index_mode)
        .await?;
    Ok(empty_ok())
}

/// UpdateIndexMode: write index.json indexMode. Query behavior is unchanged
/// (always pre-filter); only the echoed value moves.
pub async fn update_index_mode(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: UpdateIndexModeInput = parse_body(&body)?;
    if !matches!(input.index_mode.as_str(), "CLASSIC" | "ENHANCED") {
        return Err(ApiError::validation_field(
            "indexMode",
            format!(
                "indexMode must be CLASSIC or ENHANCED, got {:?}",
                input.index_mode
            ),
        ));
    }
    let (vb, ix) = resolve_index_locator(
        &state,
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
        input.index_arn.as_deref(),
    )?;
    resolve_index(&state, &vb, &ix).await?;
    state
        .storage
        .set_index_mode(&vb, &ix, &input.index_mode)
        .await?;
    Ok(empty_ok())
}

// ---------------------------------------------------------------------------
// filter subset
// ---------------------------------------------------------------------------

/// Reject operators outside the spike subset: {$eq, $and} + top-level implicit AND
/// + implicit $eq shorthand. Everything else -> ValidationException "unsupported in spike".
fn validate_filter_supported(f: &serde_json::Value) -> Result<(), ApiError> {
    let serde_json::Value::Object(map) = f else {
        return Err(ApiError::validation(
            "filter must be a JSON object (unsupported in spike)",
        ));
    };
    for (k, v) in map {
        match k.as_str() {
            "$and" => {
                let serde_json::Value::Array(items) = v else {
                    return Err(ApiError::validation("$and must be an array"));
                };
                if items.is_empty() {
                    return Err(ApiError::validation("$and must not be empty"));
                }
                for item in items {
                    validate_filter_supported(item)?;
                }
            }
            other if other.starts_with('$') => {
                return Err(ApiError::validation(format!(
                    "filter operator {other} is unsupported in spike (supported: $eq, $and)"
                )));
            }
            _field => match v {
                serde_json::Value::Object(cond) => {
                    if cond.is_empty() || cond.len() > 1 {
                        return Err(ApiError::validation(format!(
                            "field condition for {k:?} must contain exactly one supported \
                             operator (spike: $eq)"
                        )));
                    }
                    for (op, _) in cond {
                        if op != "$eq" {
                            return Err(ApiError::validation(format!(
                                "filter operator {op} is unsupported in spike \
                                 (supported: $eq, $and)"
                            )));
                        }
                    }
                }
                _scalar => {} // implicit $eq shorthand
            },
        }
    }
    Ok(())
}

/// Evaluate the validated filter subset against a vector's metadata document.
pub fn eval_filter(f: &serde_json::Value, meta: &serde_json::Value) -> bool {
    let serde_json::Value::Object(map) = f else {
        return false;
    };
    map.iter().all(|(k, v)| match k.as_str() {
        "$and" => match v {
            serde_json::Value::Array(items) => items.iter().all(|i| eval_filter(i, meta)),
            _ => false,
        },
        field => {
            let stored = meta.get(field);
            match v {
                serde_json::Value::Object(cond) => cond
                    .get("$eq")
                    .map(|want| eq_match(want, stored))
                    .unwrap_or(false),
                want => eq_match(want, stored),
            }
        }
    })
}

fn eval_filter_closure(
    f: serde_json::Value,
) -> Box<dyn Fn(&serde_json::Value) -> bool + Send + Sync> {
    // cheap per-row re-evaluation of the (small) filter tree — carried over
    // spike simplicity (see store::query_table TODO: predicate pushdown).
    Box::new(move |meta| eval_filter(&f, meta))
}

/// $eq semantics: numbers compare as f64 (1 == 1.0); bool/string/null strict;
/// stored list matches if ANY element equals the filter value; type mismatch -> no match.
fn eq_match(want: &serde_json::Value, stored: Option<&serde_json::Value>) -> bool {
    match stored {
        None | Some(serde_json::Value::Null) => matches!(want, serde_json::Value::Null),
        Some(serde_json::Value::Array(items)) => items.iter().any(|i| eq_match(want, Some(i))),
        Some(serde_json::Value::Number(w)) => match want {
            serde_json::Value::Number(n) => n.as_f64().zip(w.as_f64()).is_some_and(|(a, b)| a == b),
            _ => false,
        },
        Some(serde_json::Value::String(s)) => {
            matches!(want, serde_json::Value::String(x) if x == s)
        }
        Some(serde_json::Value::Bool(b)) => matches!(want, serde_json::Value::Bool(x) if x == b),
        Some(serde_json::Value::Object(_)) => want == stored.unwrap(),
    }
}

// ---------------------------------------------------------------------------
// integration tests: axum + tower::oneshot, --no-auth mode (no network port)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    fn state(dir: &std::path::Path) -> AppState {
        AppState {
            storage: std::sync::Arc::new(store::Storage::file(dir)),
            creds: None, // --no-auth
            region: "us-east-1".into(),
            account: "000000000000".into(),
            trace: false,
        }
    }

    /// `--backend memory` state: volatile store, no filesystem at all.
    fn memory_state() -> AppState {
        AppState {
            storage: std::sync::Arc::new(store::Storage::memory()),
            creds: None,
            region: "us-east-1".into(),
            account: "000000000000".into(),
            trace: false,
        }
    }

    /// The memory backend must run the four operations end to end (it is the
    /// T7 CI mode — no external dependencies, design.md §3/§5.2). This also
    /// proves the shared-InMemory provider keeps datasets alive across the
    /// per-operation lancedb Connections.
    #[tokio::test]
    async fn memory_backend_four_op_roundtrip() {
        let r = crate::build_router(memory_state());

        let (st, _, b) = call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"memvb"}"#).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"memvb","indexName":"memix","dataType":"float32",
                "dimension":2,"distanceMetric":"cosine"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        let (st, _, b) = call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"memvb","indexName":"memix","vectors":[
                {"key":"a","data":{"float32":[1,0]},"metadata":{"t":"x"}},
                {"key":"b","data":{"float32":[0,1]}}
            ]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"memvb","indexName":"memix","topK":2,
                "queryVector":{"float32":[1,0]},"returnDistance":true,"returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0]["key"], json!("a"));
        assert!((vs[0]["distance"].as_f64().unwrap()).abs() < 1e-6);
        assert_eq!(vs[0]["metadata"]["t"], json!("x"));
        assert_eq!(vs[1]["key"], json!("b"));

        // duplicate create still conflicts through the conditional put
        let (st, h, _) = call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"memvb"}"#).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(errortype(&h), Some("ConflictException"));

        // M1 T4 data plane through the memory engine (expr-bound key
        // predicates ride the same InMemory object-store session)
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            r#"{"vectorBucketName":"memvb","indexName":"memix","keys":["a","zz"],
                "returnData":true,"returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1, "missing key omitted: {b}");
        assert_eq!(vs[0]["key"], json!("a"));
        assert_eq!(vs[0]["data"]["float32"], json!([1.0, 0.0]));
        assert_eq!(vs[0]["metadata"]["t"], json!("x"));

        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"memvb","indexName":"memix","segmentCount":2,
                "segmentIndex":0}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert!(b["vectors"].as_array().unwrap().len() <= 2);

        let (st, _, b) = call(
            &r,
            "/DeleteVectors",
            r#"{"vectorBucketName":"memvb","indexName":"memix","keys":["a"]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"memvb","indexName":"memix"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0]["key"], json!("b"));
    }

    async fn call(
        router: &axum::Router,
        path: &str,
        body: &str,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        call_meth(router, "POST", path, body).await
    }

    /// Method-general call (GET/DELETE for /tags/…, POST for the rest).
    #[allow(clippy::similar_names)] // req builder + result tuple
    async fn call_meth(
        router: &axum::Router,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("host", "127.0.0.1:8787")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers: Vec<(String, String)> = res
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let bytes = axum::body::to_bytes(res.into_body(), 32 * 1024 * 1024)
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

    #[tokio::test]
    async fn full_roundtrip_cosine() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));

        // create bucket
        let (st, _, b) = call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"vb1"}"#).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["vectorBucketArn"],
            json!("arn:aws:s3vectors:us-east-1:000000000000:bucket/vb1")
        );
        assert_eq!(b["vectorBucket"]["vectorBucketName"], json!("vb1"));
        assert!(b["vectorBucket"]["creationTime"].is_f64());

        // duplicate -> 409
        let (st, h, b) = call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"vb1"}"#).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(errortype(&h), Some("ConflictException"));
        assert_eq!(b["__type"], json!("ConflictException"));

        // invalid name -> 400
        let (st, h, b) = call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"x"}"#).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["__type"], json!("ValidationException"));

        // create index dim=4 cosine
        let (st, _, b) = call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","dataType":"float32",
                "dimension":4,"distanceMetric":"cosine"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["indexArn"],
            json!("arn:aws:s3vectors:us-east-1:000000000000:bucket/vb1/index/ix1")
        );
        assert_eq!(b["index"]["dimension"], json!(4));
        assert_eq!(b["index"]["distanceMetric"], json!("cosine"));

        // index into missing bucket -> 404
        let (st, h, b) = call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"nope","indexName":"ix1","dataType":"float32",
                "dimension":4,"distanceMetric":"cosine"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // put 3 vectors with metadata
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","vectors":[
                {"key":"k1","data":{"float32":[1,0,0,0]},"metadata":{"genre":"a","n":1}},
                {"key":"k2","data":{"float32":[0,1,0,0]},"metadata":{"genre":"b","n":2}},
                {"key":"k3","data":{"float32":[0,0,1,0]}}
            ]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null); // empty body

        // upsert: replace k1's vector and metadata (merge_insert update_all proof)
        let (st, _, _) = call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","vectors":[
                {"key":"k1","data":{"float32":[0,0,0,1]},"metadata":{"genre":"z"}}
            ]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // query [0,0,0,1] topK=2: k1 (identical) then the remaining three are all
        // cosine distance 1.0 -> tie broken by key: k2 < k3.
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","topK":2,
                "queryVector":{"float32":[0,0,0,1]},"returnDistance":true,"returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["distanceMetric"], json!("cosine"));
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0]["key"], json!("k1"));
        assert!((vs[0]["distance"].as_f64().unwrap() - 0.0).abs() < 1e-6);
        assert_eq!(vs[0]["metadata"]["genre"], json!("z")); // upsert replaced metadata
        assert_eq!(vs[1]["key"], json!("k2"));
        assert!((vs[1]["distance"].as_f64().unwrap() - 1.0).abs() < 1e-6);

        // defaults: distance/metadata omitted
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","topK":1,
                "queryVector":{"float32":[0,0,0,1]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v0 = &b["vectors"][0];
        assert_eq!(v0["key"], json!("k1"));
        assert!(v0.get("distance").is_none(), "{v0}");
        assert!(v0.get("metadata").is_none(), "{v0}");

        // distance omitted when returnDistance=false even if metadata on
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","topK":1,
                "queryVector":{"float32":[0,0,0,1]},"returnDistance":false,"returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(b["vectors"][0].get("distance").is_none());
        assert_eq!(b["vectors"][0]["metadata"]["genre"], json!("z"));

        // dimension mismatch -> 400
        let (st, h, _) = call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","vectors":[
                {"key":"bad","data":{"float32":[1,0,0,0,0]}}]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(errortype(&h), Some("ValidationException"));

        // zero vector under cosine -> 400
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","vectors":[
                {"key":"zero","data":{"float32":[0,0,0,0]}}]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");

        // >500 vectors -> 400 (send 501 minimal vectors)
        let many: Vec<String> = (0..501)
            .map(|i| format!(r#"{{"key":"m{}","data":{{"float32":[1,0,0,0]}}}}"#, i))
            .collect();
        let body = format!(
            r#"{{"vectorBucketName":"vb1","indexName":"ix1","vectors":[{}]}}"#,
            many.join(",")
        );
        let (st, _, _) = call(&r, "/PutVectors", &body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // query missing index -> 404
        let (st, h, _) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vb1","indexName":"nope","topK":1,
                "queryVector":{"float32":[1,0,0,0]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // topK out of range / missing -> 400
        let (st, _, _) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1","topK":10001,
                "queryVector":{"float32":[1,0,0,0]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _, _) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vb1","indexName":"ix1",
                "queryVector":{"float32":[1,0,0,0]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // unknown route -> simple 404
        let (st, _, _) = call(&r, "/BogusOp", "{}").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// Pin the actual cosine formula: lancedb cosine distance == 1 - cos_sim.
    /// Query [1,0,0,0] vs stored [1,1,0,0]: sim = 1/sqrt(2) -> dist ~= 0.2928932.
    #[tokio::test]
    async fn cosine_distance_formula() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"vbc"}"#).await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"vbc","indexName":"ixc","dataType":"float32",
                "dimension":4,"distanceMetric":"cosine"}"#,
        )
        .await;
        call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vbc","indexName":"ixc","vectors":[
                {"key":"u","data":{"float32":[1,1,0,0]}},
                {"key":"neg","data":{"float32":[-1,0,0,0]}}
            ]}"#,
        )
        .await;
        let (_, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbc","indexName":"ixc","topK":2,
                "queryVector":{"float32":[1,0,0,0]},"returnDistance":true}"#,
        )
        .await;
        let vs = b["vectors"].as_array().unwrap();
        let by_key: std::collections::HashMap<&str, f64> = vs
            .iter()
            .map(|v| (v["key"].as_str().unwrap(), v["distance"].as_f64().unwrap()))
            .collect();
        let want = 1.0 - 1.0f64 / 2.0f64.sqrt(); // = 0.29289321881345254
        assert!(
            (by_key["u"] - want).abs() < 1e-6,
            "u: {} want {want}",
            by_key["u"]
        );
        // opposite direction: sim = -1 -> distance = 2
        assert!((by_key["neg"] - 2.0).abs() < 1e-6, "neg: {}", by_key["neg"]);
    }

    /// Pin euclidean = L2 with the sqrt fix-up (lancedb returns squared L2;
    /// see store::query_table). 3-4-5 triangle -> distance exactly 5.
    #[tokio::test]
    async fn euclidean_distance_formula() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"vbe"}"#).await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"vbe","indexName":"ixe","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;
        call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vbe","indexName":"ixe","vectors":[
                {"key":"a","data":{"float32":[3,4]}},
                {"key":"b","data":{"float32":[0,0]}}
            ]}"#,
        )
        .await;
        let (_, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbe","indexName":"ixe","topK":2,
                "queryVector":{"float32":[0,0]},"returnDistance":true}"#,
        )
        .await;
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs[0]["key"], json!("b"));
        assert!((vs[0]["distance"].as_f64().unwrap() - 0.0).abs() < 1e-6);
        assert!((vs[1]["distance"].as_f64().unwrap() - 5.0).abs() < 1e-5); // 3-4-5
    }

    /// Filter subset: implicit AND + $eq + $and; unsupported ops -> 400.
    #[tokio::test]
    async fn filter_subset() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"vbf"}"#).await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;
        call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","vectors":[
                {"key":"a","data":{"float32":[0,0]},"metadata":{"genre":"jazz","n":1}},
                {"key":"b","data":{"float32":[1,0]},"metadata":{"genre":"rock","n":2}},
                {"key":"c","data":{"float32":[2,0]},"metadata":{"genre":"jazz","n":2,"tags":["x","y"]}},
                {"key":"d","data":{"float32":[3,0]}}
            ]}"#,
        )
        .await;

        // $eq single
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},"filter":{"genre":"jazz"},"returnDistance":true,
                "returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0]["key"], json!("a"));
        assert!((vs[0]["distance"].as_f64().unwrap()).abs() < 1e-6);
        assert_eq!(vs[1]["key"], json!("c"));
        assert!((vs[1]["distance"].as_f64().unwrap() - 2.0).abs() < 1e-6); // Rust path L2

        // multi-key implicit AND + numeric $eq (1 == 1.0)
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},
                "filter":{"genre":"jazz","n":{"$eq":1.0}}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0]["key"], json!("a"));

        // $and explicit
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},
                "filter":{"$and":[{"genre":"rock"},{"n":2}]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0]["key"], json!("b"));

        // list metadata: any-element match
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},"filter":{"tags":"y"}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0]["key"], json!("c"));

        // filter excludes vectors without metadata
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},"filter":{"genre":{"$eq":"jazz"}},
                "returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b["vectors"].as_array().unwrap().len(), 2);

        // unsupported operator -> ValidationException with "unsupported in spike"
        let (st, h, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},"filter":{"n":{"$gt":1}}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        let msg = b["message"].as_str().unwrap();
        assert!(msg.contains("unsupported in spike"), "{msg}");

        // unsupported $or
        let (st, _, _) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":10,
                "queryVector":{"float32":[0,0]},"filter":{"$or":[{"genre":"jazz"}]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // filter honoring topK truncation (pre-filter then topK)
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"vbf","indexName":"ixf","topK":1,
                "queryVector":{"float32":[0,0]},"filter":{"n":2},"returnDistance":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0]["key"], json!("b")); // b dist 1.0 < c dist 2.0
    }

    // ------------------------------------------------------------------
    // M1 T3: management plane
    // ------------------------------------------------------------------

    /// Percent-encode an ARN the way boto3 does (':'->%3A, '/'->%2F — the
    /// empirically captured single-label form).
    fn enc_arn(arn: &str) -> String {
        arn.replace(':', "%3A").replace('/', "%2F")
    }

    #[tokio::test]
    async fn get_and_delete_vector_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"mgvb"}"#).await;

        // happy: by name — full VectorBucket shape, defaultIndexMode absent
        let (st, _, b) = call(&r, "/GetVectorBucket", r#"{"vectorBucketName":"mgvb"}"#).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vb = &b["vectorBucket"];
        assert_eq!(vb["vectorBucketName"], json!("mgvb"));
        assert_eq!(
            vb["vectorBucketArn"],
            json!("arn:aws:s3vectors:us-east-1:000000000000:bucket/mgvb")
        );
        assert!(vb["creationTime"].is_f64());
        assert!(vb.get("defaultIndexMode").is_none(), "{vb}");
        assert!(vb.get("encryptionConfiguration").is_none());

        // happy: by ARN
        let (st, _, b2) = call(
            &r,
            "/GetVectorBucket",
            r#"{"vectorBucketArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/mgvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b2["vectorBucket"]["vectorBucketName"], json!("mgvb"));

        // missing -> 404
        let (st, h, _) = call(&r, "/GetVectorBucket", r#"{"vectorBucketName":"nope"}"#).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // name+ARN disagree -> ValidationException (TODO(AWS 对拍) semantics)
        let (st, h, _) = call(
            &r,
            "/GetVectorBucket",
            r#"{"vectorBucketName":"other",
                "vectorBucketArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/mgvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(errortype(&h), Some("ValidationException"));

        // no locator -> ValidationException
        let (st, _, _) = call(&r, "/GetVectorBucket", "{}").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // malformed ARN -> ValidationException
        let (st, _, _) = call(
            &r,
            "/GetVectorBucket",
            r#"{"vectorBucketArn":"arn:aws:s3:vb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // ARN with foreign region/account -> ValidationException (strict check)
        let (st, _, _) = call(
            &r,
            "/GetVectorBucket",
            r#"{"vectorBucketArn":"arn:aws:s3vectors:eu-west-1:000000000000:bucket/mgvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // delete non-empty bucket -> 409 (S3 convention; TODO(AWS 对拍))
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"mgvb","indexName":"mgix","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;
        let (st, h, b) = call(&r, "/DeleteVectorBucket", r#"{"vectorBucketName":"mgvb"}"#).await;
        assert_eq!(st, StatusCode::CONFLICT, "{b}");
        assert_eq!(errortype(&h), Some("ConflictException"));

        // index removed -> bucket deletes; 200 empty body
        call(
            &r,
            "/DeleteIndex",
            r#"{"vectorBucketName":"mgvb","indexName":"mgix"}"#,
        )
        .await;
        let (st, _, b) = call(&r, "/DeleteVectorBucket", r#"{"vectorBucketName":"mgvb"}"#).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);

        // gone -> 404; delete missing -> 404
        let (st, _, _) = call(&r, "/GetVectorBucket", r#"{"vectorBucketName":"mgvb"}"#).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, h, _) = call(&r, "/DeleteVectorBucket", r#"{"vectorBucketName":"mgvb"}"#).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    #[tokio::test]
    async fn list_vector_buckets_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        for n in ["mgl-a", "mgl-b", "mgl-c"] {
            call(
                &r,
                "/CreateVectorBucket",
                &format!(r#"{{"vectorBucketName":"{n}"}}"#),
            )
            .await;
        }
        call(
            &r,
            "/CreateVectorBucket",
            r#"{"vectorBucketName":"mglz-d"}"#,
        )
        .await; // prefix decoy

        // page 1: prefix filter + maxResults=2 -> a, b + nextToken
        let (st, _, b) = call(
            &r,
            "/ListVectorBuckets",
            r#"{"maxResults":2,"prefix":"mgl-"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let names: Vec<&str> = b["vectorBuckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["vectorBucketName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mgl-a", "mgl-b"], "sorted by name");
        assert!(b["vectorBuckets"][0].get("dimension").is_none()); // summary shape
        let token = b["nextToken"]
            .as_str()
            .expect("nextToken on partial page")
            .to_string();

        // page 2: -> c, no nextToken (termination)
        let (st, _, b) = call(
            &r,
            "/ListVectorBuckets",
            &format!(r#"{{"maxResults":2,"prefix":"mgl-","nextToken":"{token}"}}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let names: Vec<&str> = b["vectorBuckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["vectorBucketName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mgl-c"]);
        assert!(b.get("nextToken").is_none(), "no nextToken on last page");

        // full list without prefix (default maxResults=500): 4 buckets, sorted
        let (st, _, b) = call(&r, "/ListVectorBuckets", "{}").await;
        assert_eq!(st, StatusCode::OK);
        let names: Vec<&str> = b["vectorBuckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["vectorBucketName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mgl-a", "mgl-b", "mgl-c", "mglz-d"]);

        // maxResults out of range -> ValidationException with fieldList
        for bad in [0, 501, -1] {
            let (st, h, b) = call(
                &r,
                "/ListVectorBuckets",
                &format!(r#"{{"maxResults":{bad}}}"#),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
            assert_eq!(errortype(&h), Some("ValidationException"));
            assert_eq!(b["fieldList"][0]["path"], json!("maxResults"), "{b}");
        }

        // garbage nextToken -> ValidationException
        let (st, h, _) = call(
            &r,
            "/ListVectorBuckets",
            r#"{"nextToken":"!!not-base64!!"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(errortype(&h), Some("ValidationException"));

        // prefix over 63 chars -> ValidationException (model max)
        let long = "x".repeat(64);
        let (st, _, _) = call(
            &r,
            "/ListVectorBuckets",
            &format!(r#"{{"prefix":"{long}"}}"#),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_delete_list_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"ixvb"}"#).await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"ix-a","dataType":"float32",
                "dimension":3,"distanceMetric":"cosine",
                "metadataConfiguration":{"nonFilterableMetadataKeys":["blob"]}}"#,
        )
        .await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"ix-b","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;

        // GetIndex: full Index shape incl metadataConfiguration + indexMode
        let (st, _, b) = call(
            &r,
            "/GetIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"ix-a"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let ix = &b["index"];
        assert_eq!(ix["vectorBucketName"], json!("ixvb"));
        assert_eq!(ix["indexName"], json!("ix-a"));
        assert_eq!(
            ix["indexArn"],
            json!("arn:aws:s3vectors:us-east-1:000000000000:bucket/ixvb/index/ix-a")
        );
        assert!(ix["creationTime"].is_f64());
        assert_eq!(ix["dataType"], json!("float32"));
        assert_eq!(ix["dimension"], json!(3));
        assert_eq!(ix["distanceMetric"], json!("cosine"));
        assert_eq!(ix["indexMode"], json!("ENHANCED")); // create-time default
        assert_eq!(
            ix["metadataConfiguration"]["nonFilterableMetadataKeys"],
            json!(["blob"])
        );

        // GetIndex by indexArn
        let (st, _, b) = call(
            &r,
            "/GetIndex",
            r#"{"indexArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/ixvb/index/ix-b"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b["index"]["indexName"], json!("ix-b"));
        assert!(b["index"].get("metadataConfiguration").is_none());

        // GetIndex missing -> 404
        let (st, h, _) = call(
            &r,
            "/GetIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"zz"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // ListIndexes (by name and by ARN): summaries, name-sorted
        let (st, _, b) = call(&r, "/ListIndexes", r#"{"vectorBucketName":"ixvb"}"#).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let names: Vec<&str> = b["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["indexName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["ix-a", "ix-b"]);
        assert!(b["indexes"][0].get("dimension").is_none()); // summary shape

        let (st, _, b) = call(
            &r,
            "/ListIndexes",
            r#"{"vectorBucketArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/ixvb",
                "prefix":"ix-b"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let names: Vec<&str> = b["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["indexName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["ix-b"]);

        // ListIndexes on missing bucket -> 404
        let (st, h, _) = call(&r, "/ListIndexes", r#"{"vectorBucketName":"nope"}"#).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // DeleteIndex -> 200 empty; GetIndex -> 404; re-create same name OK
        // (proves index.json AND the lance dataset were both removed)
        let (st, _, b) = call(
            &r,
            "/DeleteIndex",
            r#"{"indexArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/ixvb/index/ix-a"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);
        let (st, _, _) = call(
            &r,
            "/GetIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"ix-a"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _, _) = call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"ix-a","dataType":"float32",
                "dimension":3,"distanceMetric":"cosine"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // DeleteIndex missing -> 404
        let (st, h, _) = call(
            &r,
            "/DeleteIndex",
            r#"{"vectorBucketName":"ixvb","indexName":"zz"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    /// DeleteIndex must also remove the engine dataset on the memory backend,
    /// where lance objects live under a `vf/` prefix on the shared map — the
    /// recreate-same-name round trip fails with 409 if any object survives.
    #[tokio::test]
    async fn memory_backend_delete_index_and_recreate() {
        let r = crate::build_router(memory_state());
        call(
            &r,
            "/CreateVectorBucket",
            r#"{"vectorBucketName":"memvb2"}"#,
        )
        .await;
        let mk = r#"{"vectorBucketName":"memvb2","indexName":"memix2","dataType":"float32",
            "dimension":2,"distanceMetric":"cosine"}"#;
        call(&r, "/CreateIndex", mk).await;
        call(
            &r,
            "/PutVectors",
            r#"{"vectorBucketName":"memvb2","indexName":"memix2","vectors":[
                {"key":"a","data":{"float32":[1,0]}}]}"#,
        )
        .await;
        let (st, _, _) = call(
            &r,
            "/DeleteIndex",
            r#"{"vectorBucketName":"memvb2","indexName":"memix2"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _, b) = call(&r, "/CreateIndex", mk).await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"memvb2","indexName":"memix2","topK":1,
                "queryVector":{"float32":[1,0]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["vectors"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn policy_store_and_echo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"pvb"}"#).await;

        // no policy yet -> 404
        let (st, h, _) = call(
            &r,
            "/GetVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // put (string form, non-canonical spacing) -> byte-faithful echo
        let doc = r#"{"Version": "2012-10-17", "Statement": [ {"Effect": "Allow"} ]}"#;
        let (st, _, b) = call(
            &r,
            "/PutVectorBucketPolicy",
            &format!(
                r#"{{"vectorBucketName":"pvb","policy":{}}}"#,
                serde_json::json!(doc)
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);
        let (st, _, b) = call(
            &r,
            "/GetVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["policy"], json!(doc), "policy echoed byte-faithfully");

        // put (object form) -> canonicalized string echo, same JSON document
        let (st, _, _) = call(
            &r,
            "/PutVectorBucketPolicy",
            r#"{"vectorBucketArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/pvb",
                "policy":{"Version":"2012-10-17","Statement":[]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _, b) = call(
            &r,
            "/GetVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(b["policy"].as_str().unwrap()).unwrap(),
            json!({"Version":"2012-10-17","Statement":[]})
        );

        // policy not valid JSON -> ValidationException (fieldList path policy)
        let (st, h, b) = call(
            &r,
            "/PutVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb","policy":"not json"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("policy"));

        // policy wrong type -> ValidationException
        let (st, _, _) = call(
            &r,
            "/PutVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb","policy":42}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // policy on missing bucket -> 404
        let (st, h, _) = call(
            &r,
            "/PutVectorBucketPolicy",
            r#"{"vectorBucketName":"nope","policy":"{}"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));

        // delete -> 200; get -> 404; delete again -> 200 (idempotent)
        let (st, _, _) = call(
            &r,
            "/DeleteVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _, _) = call(
            &r,
            "/GetVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _, _) = call(
            &r,
            "/DeleteVectorBucketPolicy",
            r#"{"vectorBucketName":"pvb"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn tags_resource_both_path_forms() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"tvb"}"#).await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"tvb","indexName":"tix","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;
        let arn_b = "arn:aws:s3vectors:us-east-1:000000000000:bucket/tvb";
        let arn_i = "arn:aws:s3vectors:us-east-1:000000000000:bucket/tvb/index/tix";

        // TagResource: boto3 form (whole ARN percent-encoded as one label)
        let (st, _, b) = call_meth(
            &r,
            "POST",
            &format!("/tags/{}", enc_arn(arn_b)),
            r#"{"tags":{"env":"dev"}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);

        // TagResource: literal multi-segment form (other clients)
        let (st, _, _) = call_meth(
            &r,
            "POST",
            &format!("/tags/{arn_b}"),
            r#"{"tags":{"team":"infra"}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // ListTagsForResource: encoded path sees the MERGE of both writes
        let (st, _, b) = call_meth(&r, "GET", &format!("/tags/{}", enc_arn(arn_b)), "").await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["tags"], json!({"env": "dev", "team": "infra"}));

        // ListTagsForResource: literal path, same state
        let (st, _, b) = call_meth(&r, "GET", &format!("/tags/{arn_b}"), "").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b["tags"], json!({"env": "dev", "team": "infra"}));

        // index ARN tags (independent of bucket tags), with a punctuation-heavy key
        call_meth(
            &r,
            "POST",
            &format!("/tags/{}", enc_arn(arn_i)),
            r#"{"tags":{"a":"1","b":"2","sp ace+x/y:z":"kept"}}"#,
        )
        .await;
        let (st, _, b) = call_meth(&r, "GET", &format!("/tags/{}", enc_arn(arn_i)), "").await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["tags"], json!({"a":"1","b":"2","sp ace+x/y:z":"kept"}));
        // bucket tags unaffected by index tags
        let (_, _, b) = call_meth(&r, "GET", &format!("/tags/{}", enc_arn(arn_b)), "").await;
        assert_eq!(b["tags"].as_object().unwrap().len(), 2);

        // UntagResource: tagKeys as repeated query params, values encoded
        // (boto3 form: `tagKeys=a&tagKeys=sp%20ace%2Bx%2Fy%3Az`)
        let (st, _, b) = call_meth(
            &r,
            "DELETE",
            &format!(
                "/tags/{}?tagKeys=a&tagKeys=sp%20ace%2Bx%2Fy%3Az",
                enc_arn(arn_i)
            ),
            "",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);
        let (_, _, b) = call_meth(&r, "GET", &format!("/tags/{}", enc_arn(arn_i)), "").await;
        assert_eq!(
            b["tags"],
            json!({"b": "2"}),
            "untag removed a + punct key, kept b"
        );

        // untag without tagKeys -> ValidationException
        let (st, h, _) = call_meth(&r, "DELETE", &format!("/tags/{}", enc_arn(arn_i)), "").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(errortype(&h), Some("ValidationException"));

        // tags on unknown resources -> 404 (bucket and index forms)
        let ghost_b = enc_arn("arn:aws:s3vectors:us-east-1:000000000000:bucket/ghost");
        let (st, h, _) = call_meth(
            &r,
            "POST",
            &format!("/tags/{ghost_b}"),
            r#"{"tags":{"a":"b"}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
        let ghost_i = enc_arn("arn:aws:s3vectors:us-east-1:000000000000:bucket/tvb/index/ghost");
        let (st, _, _) = call_meth(&r, "GET", &format!("/tags/{ghost_i}"), "").await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // >10 pairs -> ValidationException (fill bucket to 10 first)
        let mut full = serde_json::Map::new();
        for i in 0..8 {
            full.insert(format!("k{i}"), json!(format!("v{i}")));
        }
        let body = format!(r#"{{"tags":{}}}"#, serde_json::Value::Object(full));
        let (st, _, _) = call_meth(&r, "POST", &format!("/tags/{}", enc_arn(arn_b)), &body).await;
        assert_eq!(st, StatusCode::OK); // 2 existing + 8 = 10
        let (st, h, b) = call_meth(
            &r,
            "POST",
            &format!("/tags/{}", enc_arn(arn_b)),
            r#"{"tags":{"overflow":"x"}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("tags"));

        // key 129 chars / value 257 chars / non-string value -> ValidationException
        let long_key = "k".repeat(129);
        let (st, _, _) = call_meth(
            &r,
            "POST",
            &format!("/tags/{}", enc_arn(arn_i)),
            &format!(r#"{{"tags":{{"{long_key}":"v"}}}}"#),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let long_val = "v".repeat(257);
        let (st, _, _) = call_meth(
            &r,
            "POST",
            &format!("/tags/{}", enc_arn(arn_i)),
            &format!(r#"{{"tags":{{"k":"{long_val}"}}}}"#),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _, _) = call_meth(
            &r,
            "POST",
            &format!("/tags/{}", enc_arn(arn_i)),
            r#"{"tags":{"k":42}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // bare /tags/ (no ARN at all) falls through to the 404 fallback —
        // axum's wildcard needs a non-empty tail; nothing is addressed.
        let (st, h, _) = call_meth(&r, "GET", "/tags/", "").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), None, "plain fallback 404, no error shape");
    }

    #[tokio::test]
    async fn index_mode_knobs() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"mvb"}"#).await;

        // invalid enum -> ValidationException with fieldList path (both ops)
        let (st, h, b) = call(
            &r,
            "/PutVectorBucketDefaultIndexMode",
            r#"{"vectorBucketName":"mvb","defaultIndexMode":"TURBO"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("defaultIndexMode"));
        let (st, h, b) = call(
            &r,
            "/UpdateIndexMode",
            r#"{"vectorBucketName":"mvb","indexName":"zz","indexMode":"TURBO"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("indexMode"));

        // missing bucket / index -> 404 (enum validated first? bucket check after)
        let (st, _, _) = call(
            &r,
            "/PutVectorBucketDefaultIndexMode",
            r#"{"vectorBucketName":"nope","defaultIndexMode":"CLASSIC"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // set CLASSIC -> GetVectorBucket echoes it
        let (st, _, b) = call(
            &r,
            "/PutVectorBucketDefaultIndexMode",
            r#"{"vectorBucketName":"mvb","defaultIndexMode":"CLASSIC"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);
        let (_, _, b) = call(&r, "/GetVectorBucket", r#"{"vectorBucketName":"mvb"}"#).await;
        assert_eq!(b["vectorBucket"]["defaultIndexMode"], json!("CLASSIC"));

        // new index inherits CLASSIC (echo-only; runtime stays pre-filter)
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"mvb","indexName":"mix","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;
        let (_, _, b) = call(
            &r,
            "/GetIndex",
            r#"{"vectorBucketName":"mvb","indexName":"mix"}"#,
        )
        .await;
        assert_eq!(b["index"]["indexMode"], json!("CLASSIC"));

        // UpdateIndexMode by indexArn -> ENHANCED; GetIndex echoes
        let (st, _, b) = call(
            &r,
            "/UpdateIndexMode",
            r#"{"indexArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/mvb/index/mix",
                "indexMode":"ENHANCED"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (_, _, b) = call(
            &r,
            "/GetIndex",
            r#"{"vectorBucketName":"mvb","indexName":"mix"}"#,
        )
        .await;
        assert_eq!(b["index"]["indexMode"], json!("ENHANCED"));

        // UpdateIndexMode on missing index -> 404
        let (st, _, _) = call(
            &r,
            "/UpdateIndexMode",
            r#"{"vectorBucketName":"mvb","indexName":"zz","indexMode":"CLASSIC"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // bucket default does NOT rewrite existing indexes
        call(
            &r,
            "/PutVectorBucketDefaultIndexMode",
            r#"{"vectorBucketName":"mvb","defaultIndexMode":"CLASSIC"}"#,
        )
        .await;
        let (_, _, b) = call(
            &r,
            "/GetIndex",
            r#"{"vectorBucketName":"mvb","indexName":"mix"}"#,
        )
        .await;
        assert_eq!(b["index"]["indexMode"], json!("ENHANCED"));
    }

    /// Vector ops accept the indexArn locator too (model member): PutVectors
    /// by ARN + QueryVectors by name must meet; name/ARN mismatch -> 400.
    #[tokio::test]
    async fn vector_ops_accept_index_arn_locator() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"avb"}"#).await;
        call(
            &r,
            "/CreateIndex",
            r#"{"vectorBucketName":"avb","indexName":"aix","dataType":"float32",
                "dimension":2,"distanceMetric":"euclidean"}"#,
        )
        .await;

        let (st, _, b) = call(
            &r,
            "/PutVectors",
            r#"{"indexArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/avb/index/aix",
                "vectors":[{"key":"k","data":{"float32":[1,2]}}]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"avb","indexName":"aix","topK":1,
                "queryVector":{"float32":[1,2]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["vectors"][0]["key"], json!("k"));

        // consistent name+ARN pair works
        let (st, _, _) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"avb","indexName":"aix",
                "indexArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/avb/index/aix",
                "topK":1,"queryVector":{"float32":[1,2]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // disagreeing pair -> ValidationException (TODO(AWS 对拍))
        let (st, h, _) = call(
            &r,
            "/QueryVectors",
            r#"{"vectorBucketName":"avb","indexName":"other",
                "indexArn":"arn:aws:s3vectors:us-east-1:000000000000:bucket/avb/index/aix",
                "topK":1,"queryVector":{"float32":[1,2]}}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(errortype(&h), Some("ValidationException"));

        // neither locator -> ValidationException
        let (st, _, _) = call(
            &r,
            "/PutVectors",
            r#"{"vectors":[{"key":"k","data":{"float32":[1,2]}}]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    // ------------------------------------------------------------------
    // M1 T4: vector data plane (GetVectors / ListVectors / DeleteVectors +
    // PutVectors full limits)
    // ------------------------------------------------------------------

    /// Request body from a JSON value (needed whenever keys contain quotes,
    /// backslashes or non-ASCII — raw string literals get unreadable).
    fn jbody(v: serde_json::Value) -> String {
        v.to_string()
    }

    /// Create bucket + index helper for the T4 tests.
    async fn mkindex(router: &axum::Router, vb: &str, ix: &str, dim: i64, metric: &str) {
        let (st, _, b) = call(
            router,
            "/CreateIndex",
            &jbody(json!({
                "vectorBucketName": vb, "indexName": ix, "dataType": "float32",
                "dimension": dim, "distanceMetric": metric,
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
    }

    #[tokio::test]
    async fn get_vectors_found_missing_and_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"gvb"}"#).await;
        mkindex(&r, "gvb", "gix", 2, "euclidean").await;
        let hostile = "sp\"'\\菊"; // quotes + backslash + non-ASCII
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "gvb", "indexName": "gix",
                "vectors": [
                    {"key": "a", "data": {"float32": [1, 2]},
                     "metadata": {"tag": "one", "n": 3, "ok": true, "tags": ["x", "y"]}},
                    {"key": "b", "data": {"float32": [3, 4]}},
                    {"key": hostile, "data": {"float32": [5, 6]},
                     "metadata": {"emoji": "🎯"}}
                ]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // mixed found/missing: response holds only the FOUND keys, in request
        // order, deduplicated; NO status member (model has none — module doc).
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({
                "vectorBucketName": "gvb", "indexName": "gix",
                "keys": ["b", "ghost", "a", hostile, "b"],
                "returnData": true, "returnMetadata": true,
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 3, "missing key omitted, dup key deduped: {b}");
        assert_eq!(vs[0]["key"], json!("b")); // request order, not key order
        assert_eq!(vs[1]["key"], json!("a"));
        assert_eq!(vs[2]["key"], json!(hostile));
        assert_eq!(vs[0].get("metadata"), None); // b stored without metadata
        assert_eq!(vs[1]["metadata"]["tag"], json!("one"));
        assert_eq!(vs[1]["metadata"]["tags"], json!(["x", "y"]));
        assert_eq!(vs[1]["metadata"]["n"], json!(3));
        assert_eq!(vs[1]["data"]["float32"], json!([1.0, 2.0]));
        assert_eq!(vs[2]["metadata"]["emoji"], json!("🎯"));
        assert!(vs.iter().all(|v| v.get("status").is_none()));

        // defaults: data/metadata omitted entirely
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            r#"{"vectorBucketName":"gvb","indexName":"gix","keys":["a"]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let v0 = &b["vectors"][0];
        assert_eq!(v0["key"], json!("a"));
        assert!(v0.get("data").is_none(), "{v0}");
        assert!(v0.get("metadata").is_none(), "{v0}");

        // returnData=false explicit: same omission
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            r#"{"vectorBucketName":"gvb","indexName":"gix","keys":["a"],
                "returnData":false,"returnMetadata":false}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(b["vectors"][0].get("data").is_none());

        // all keys missing -> 200 with empty vectors (member is required)
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            r#"{"vectorBucketName":"gvb","indexName":"gix","keys":["nope1","nope2"]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["vectors"].as_array().unwrap().len(), 0);

        // exactly 100 keys (model max) -> 200; 101 -> ValidationException
        let hundred: Vec<serde_json::Value> = (0..100)
            .map(|i| json!({"key": format!("m{i:03}"), "data": {"float32": [0, 1]}}))
            .collect();
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({"vectorBucketName": "gvb", "indexName": "gix", "vectors": hundred})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let keys100: Vec<String> = (0..100).map(|i| format!("m{i:03}")).collect();
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({"vectorBucketName": "gvb", "indexName": "gix", "keys": keys100})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["vectors"].as_array().unwrap().len(), 100);
        let mut keys101 = keys100.clone();
        keys101.push("one-too-many".into());
        let (st, h, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({"vectorBucketName": "gvb", "indexName": "gix", "keys": keys101})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("keys"));

        // empty keys / missing keys / oversized key -> 400
        let (st, _, _) = call(
            &r,
            "/GetVectors",
            r#"{"vectorBucketName":"gvb","indexName":"gix","keys":[]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let long_key = "k".repeat(1025);
        let (st, _, _) = call(
            &r,
            "/GetVectors",
            &jbody(json!({"vectorBucketName": "gvb", "indexName": "gix", "keys": [long_key]})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // missing index -> 404 (locator resolves, catalog misses)
        let (st, h, _) = call(
            &r,
            "/GetVectors",
            r#"{"vectorBucketName":"gvb","indexName":"zz","keys":["a"]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    #[tokio::test]
    async fn delete_vectors_special_keys_and_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"dvb"}"#).await;
        mkindex(&r, "dvb", "dix", 2, "euclidean").await;
        // keys chosen to break naive SQL string interpolation: quotes,
        // backslash, unicode, emoji, and a classic injection payload. All are
        // bound as arrow literals — none of this ever reaches SQL text.
        let keys = [
            "plain",
            "it's",
            "a\"b",
            "back\\slash",
            "菊🍒",
            "'); DROP TABLE vectors;--",
        ];
        let vectors: Vec<serde_json::Value> = keys
            .iter()
            .map(|k| json!({"key": k, "data": {"float32": [1, 0]}}))
            .collect();
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({"vectorBucketName": "dvb", "indexName": "dix", "vectors": vectors})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // delete the hostile subset -> 200 empty; "plain" survives
        let (st, _, b) = call(
            &r,
            "/DeleteVectors",
            &jbody(json!({
                "vectorBucketName": "dvb", "indexName": "dix", "keys": keys[1..]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b, serde_json::Value::Null);
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({
                "vectorBucketName": "dvb", "indexName": "dix",
                "keys": keys, "returnMetadata": true
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 1, "only plain survives: {b}");
        assert_eq!(vs[0]["key"], json!("plain"));

        // idempotent: deleting absent keys again is 200, not an error
        let (st, _, _) = call(
            &r,
            "/DeleteVectors",
            &jbody(json!({"vectorBucketName": "dvb", "indexName": "dix", "keys": keys[1..]})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // delete -> put same key = normal rebuild (merge_insert insert path)
        let rebuilt = "a\"b";
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "dvb", "indexName": "dix",
                "vectors": [{"key": rebuilt, "data": {"float32": [7, 8]},
                             "metadata": {"round": 2}}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({
                "vectorBucketName": "dvb", "indexName": "dix", "keys": [rebuilt],
                "returnData": true, "returnMetadata": true
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["vectors"][0]["data"]["float32"], json!([7.0, 8.0]));
        assert_eq!(b["vectors"][0]["metadata"]["round"], json!(2));

        // boundaries: exactly 500 keys (mostly nonexistent) -> 200; 501 -> 400
        let keys500: Vec<String> = (0..500).map(|i| format!("ghost-{i}")).collect();
        let (st, _, b) = call(
            &r,
            "/DeleteVectors",
            &jbody(json!({"vectorBucketName": "dvb", "indexName": "dix", "keys": keys500})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let mut keys501: Vec<String> = (0..500).map(|i| format!("ghost-{i}")).collect();
        keys501.push("plain".into()); // 500 phantoms + one real = 501
        let (st, h, b) = call(
            &r,
            "/DeleteVectors",
            &jbody(json!({"vectorBucketName": "dvb", "indexName": "dix", "keys": keys501})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("keys"));
        let (st, _, _) = call(
            &r,
            "/DeleteVectors",
            r#"{"vectorBucketName":"dvb","indexName":"dix","keys":[]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // missing index -> 404
        let (st, h, _) = call(
            &r,
            "/DeleteVectors",
            r#"{"vectorBucketName":"dvb","indexName":"zz","keys":["a"]}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    /// Walk ListVectors pages to the end; returns (keys in order, pages).
    async fn list_all(r: &axum::Router, base: serde_json::Value) -> (Vec<String>, usize) {
        let mut seen: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut req = base.clone();
            if let Some(t) = &token {
                req["nextToken"] = json!(t);
            }
            let (st, _, b) = call(r, "/ListVectors", &jbody(req)).await;
            assert_eq!(st, StatusCode::OK, "{b}");
            for v in b["vectors"].as_array().unwrap() {
                seen.push(v["key"].as_str().unwrap().to_string());
            }
            pages += 1;
            match b.get("nextToken").and_then(|t| t.as_str()) {
                Some(t) => token = Some(t.to_string()),
                None => return (seen, pages),
            }
        }
    }

    #[tokio::test]
    async fn list_vectors_pagination_and_segments() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"lvb"}"#).await;
        mkindex(&r, "lvb", "lix", 2, "euclidean").await;
        let vectors: Vec<serde_json::Value> = (0..10)
            .map(|i| {
                json!({"key": format!("k{i:02}"), "data": {"float32": [i, 0]},
                       "metadata": {"n": i}})
            })
            .collect();
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({"vectorBucketName": "lvb", "indexName": "lix", "vectors": vectors})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // default: all 10 keys ascending, no data/metadata members
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert_eq!(vs.len(), 10);
        let names: Vec<String> = vs
            .iter()
            .map(|v| v["key"].as_str().unwrap().to_string())
            .collect();
        let want: Vec<String> = (0..10).map(|i| format!("k{i:02}")).collect();
        assert_eq!(names, want, "key-ascending order");
        assert!(vs[0].get("data").is_none() && vs[0].get("metadata").is_none());
        assert!(b.get("nextToken").is_none(), "single page, no token");

        // returnData + returnMetadata members
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix","maxResults":1,
                "returnData":true,"returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let v0 = &b["vectors"][0];
        assert_eq!(v0["key"], json!("k00"));
        assert_eq!(v0["data"]["float32"], json!([0.0, 0.0]));
        assert_eq!(v0["metadata"]["n"], json!(0));
        assert!(b.get("nextToken").is_some());

        // multi-page walk: 3,3,3,1 — order preserved across pages
        let (seen, pages) = list_all(
            &r,
            json!({"vectorBucketName": "lvb", "indexName": "lix", "maxResults": 3}),
        )
        .await;
        assert_eq!(pages, 4, "pages of 3,3,3,1");
        assert_eq!(
            seen,
            (0..10).map(|i| format!("k{i:02}")).collect::<Vec<_>>()
        );

        // segments: count=3 -> three pairwise-disjoint walks whose union is
        // the full key set; repeating a segment is byte-stable. FNV-1a goldens
        // (computed independently of the Rust impl): "k00" -> seg 0, "k05" ->
        // seg 0 of 3 but seg 1 of 2 — the partition is hash-driven, not key order.
        let mut union: Vec<String> = Vec::new();
        for seg in 0..3 {
            let (part, _) = list_all(
                &r,
                json!({"vectorBucketName": "lvb", "indexName": "lix", "maxResults": 2,
                       "segmentCount": 3, "segmentIndex": seg}),
            )
            .await;
            assert!(
                part.iter().all(|k| !union.contains(k)),
                "segment {seg} overlaps earlier segments"
            );
            if seg == 0 {
                assert!(
                    part.contains(&"k00".to_string()),
                    "FNV-1a golden: k00 in seg0"
                );
                assert!(!part.contains(&"k07".to_string()));
            }
            union.extend(part);
        }
        union.sort();
        assert_eq!(
            union,
            (0..10).map(|i| format!("k{i:02}")).collect::<Vec<_>>(),
            "segment union == full key set"
        );
        let (again, _) = list_all(
            &r,
            json!({"vectorBucketName": "lvb", "indexName": "lix",
                   "segmentCount": 3, "segmentIndex": 0}),
        )
        .await;
        let (first, _) = list_all(
            &r,
            json!({"vectorBucketName": "lvb", "indexName": "lix",
                   "segmentCount": 3, "segmentIndex": 0}),
        )
        .await;
        assert_eq!(first, again, "same key always lands in the same segment");

        // pairing: either member alone -> ValidationException
        let (st, h, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix","segmentCount":2}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert_eq!(b["fieldList"][0]["path"], json!("segmentIndex"));
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix","segmentIndex":1}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("segmentCount"));

        // ranges: count 17 / index 16 / index >= count / count 0 -> 400
        for (count, index) in [(17, 0), (16, 16), (2, 2), (0, 0)] {
            let body = jbody(json!({
                "vectorBucketName": "lvb", "indexName": "lix",
                "segmentCount": count, "segmentIndex": index,
            }));
            let (st, _, b) = call(&r, "/ListVectors", &body).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{body} {b}");
        }

        // maxResults range + garbage token
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix","maxResults":0}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["fieldList"][0]["path"], json!("maxResults"));
        let (st, _, _) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix","maxResults":1001}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _, _) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix","nextToken":"@@not-base64@@"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // empty index -> 200 with empty vectors
        mkindex(&r, "lvb", "lix2", 2, "euclidean").await;
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"lix2"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["vectors"].as_array().unwrap().len(), 0);
        assert!(b.get("nextToken").is_none());

        // missing index -> 404
        let (st, h, _) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"lvb","indexName":"zz"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(errortype(&h), Some("NotFoundException"));
    }

    /// 1 MiB page budget: 30 vectors x ~39 KB non-filterable metadata exceed
    /// the budget mid-walk; the page must cut at the last COMPLETE vector and
    /// hand out a nextToken (AWS: "processed dataset size exceeds 1 MB" —
    /// approximation documented at LIST_PAGE_BUDGET).
    #[tokio::test]
    async fn list_vectors_1mib_page_budget() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"mbvb"}"#).await;
        let (st, _, b) = call(
            &r,
            "/CreateIndex",
            &jbody(json!({
                "vectorBucketName": "mbvb", "indexName": "mbix", "dataType": "float32",
                "dimension": 2, "distanceMetric": "euclidean",
                "metadataConfiguration": {"nonFilterableMetadataKeys": ["blob"]},
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let blob = "x".repeat(39_000);
        let vectors: Vec<serde_json::Value> = (0..30)
            .map(|i| {
                json!({"key": format!("big-{i:02}"), "data": {"float32": [1, 0]},
                       "metadata": {"blob": blob}})
            })
            .collect();
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({"vectorBucketName": "mbvb", "indexName": "mbix", "vectors": vectors})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // page 1: truncated below 30 entries, token present, under budget
        let (st, _, b) = call(
            &r,
            "/ListVectors",
            r#"{"vectorBucketName":"mbvb","indexName":"mbix",
                "returnMetadata":true}"#,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let vs = b["vectors"].as_array().unwrap();
        assert!(
            vs.len() < 30 && vs.len() >= 26,
            "truncated at a complete vector: {} entries",
            vs.len()
        );
        assert!(
            b.get("nextToken").is_some(),
            "budget truncation hands out a token"
        );
        assert!(
            serde_json::to_vec(&b).unwrap().len() <= 1024 * 1024,
            "emitted page stays within the 1 MiB budget"
        );
        assert_eq!(vs.last().unwrap()["key"], json!("big-25")); // 26 full entries fit

        // the walk still covers everything, in order
        let (seen, pages) = list_all(
            &r,
            json!({"vectorBucketName": "mbvb", "indexName": "mbix", "returnMetadata": true}),
        )
        .await;
        assert_eq!(pages, 2, "second page holds the remaining 4");
        assert_eq!(
            seen,
            (0..30).map(|i| format!("big-{i:02}")).collect::<Vec<_>>()
        );
    }

    /// PutVectors full limit set: 20 MiB payload, 40 KB metadata, 50 keys,
    /// 2 KB filterable (with the non-filterable exemption), value types,
    /// same-batch duplicate keys (last wins).
    #[tokio::test]
    async fn put_vectors_full_limit_set() {
        let dir = tempfile::tempdir().unwrap();
        let r = crate::build_router(state(dir.path()));
        call(&r, "/CreateVectorBucket", r#"{"vectorBucketName":"pvb"}"#).await;
        mkindex(&r, "pvb", "sml", 2, "euclidean").await; // plain index
        let (st, _, b) = call(
            &r,
            "/CreateIndex",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "big", "dataType": "float32",
                "dimension": 1024, "distanceMetric": "euclidean",
                "metadataConfiguration": {"nonFilterableMetadataKeys": ["blob"]},
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/CreateIndex",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "blob2", "dataType": "float32",
                "dimension": 2, "distanceMetric": "euclidean",
                "metadataConfiguration": {"nonFilterableMetadataKeys": ["blob"]},
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // (a) 20 MiB payload cap. 500 vectors x (39 KB blob + dim-1024
        // vector) ~= 21.6 MB: each vector is individually VALID, only the
        // payload rule trips. Body sits under the 21 MiB transport cap so the
        // handler — not the body limit — answers.
        let vec1024 = format!("[{}]", vec!["0.5"; 1024].join(","));
        let blob = "x".repeat(39_000);
        let mut items = String::new();
        for i in 0..500 {
            if i > 0 {
                items.push(',');
            }
            items.push_str(&format!(
                r#"{{"key":"m{:03}","data":{{"float32":{}}},"metadata":{{"blob":"{}"}}}}"#,
                i, vec1024, blob
            ));
        }
        let over = format!(r#"{{"vectorBucketName":"pvb","indexName":"big","vectors":[{items}]}}"#);
        assert!(
            over.len() > 20 * 1024 * 1024 && over.len() < 21 * 1024 * 1024,
            "test body must be between 20 and 21 MiB, got {}",
            over.len()
        );
        let (st, h, b) = call(&r, "/PutVectors", &over).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert!(b["message"].as_str().unwrap().contains("20 MiB"), "{b}");
        // 480 of the same vectors ~= 20.7 MB: under the cap -> 200
        let under: String = {
            let mut s = String::new();
            for i in 0..480 {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    r#"{{"key":"u{:03}","data":{{"float32":{}}},"metadata":{{"blob":"{}"}}}}"#,
                    i, vec1024, blob
                ));
            }
            format!(r#"{{"vectorBucketName":"pvb","indexName":"big","vectors":[{s}]}}"#)
        };
        assert!(under.len() < 20 * 1024 * 1024, "{}", under.len());
        let (st, _, b) = call(&r, "/PutVectors", &under).await;
        assert_eq!(st, StatusCode::OK, "{b}");

        // (b) 40 KB total metadata cap (blob-declared key: only the total trips)
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "blob2",
                "vectors": [{"key": "fat", "data": {"float32": [0, 1]},
                             "metadata": {"blob": "x".repeat(41_000)}}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(b["message"].as_str().unwrap().contains("40 KB"), "{b}");

        // (c) 51 keys -> rejection
        let mut many = serde_json::Map::new();
        for i in 0..51 {
            many.insert(format!("k{i:02}"), json!(i));
        }
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "sml",
                "vectors": [{"key": "k", "data": {"float32": [0, 1]},
                             "metadata": many}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert!(b["message"].as_str().unwrap().contains("50"), "{b}");

        // (d) filterable 2 KB per vector: over on a plain index, exempt under
        // a declared non-filterable key of the same size.
        let big_value = "v".repeat(2_100);
        let (st, h, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "sml",
                "vectors": [{"key": "f", "data": {"float32": [0, 1]},
                             "metadata": {"s": big_value}}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(errortype(&h), Some("ValidationException"));
        assert!(b["message"].as_str().unwrap().contains("2 KB"), "{b}");
        let (st, _, b) = call(
            &r,
            "/CreateIndex",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "blb2", "dataType": "float32",
                "dimension": 2, "distanceMetric": "euclidean",
                "metadataConfiguration": {"nonFilterableMetadataKeys": ["doc"]},
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "blb2",
                "vectors": [{"key": "f", "data": {"float32": [0, 1]},
                             "metadata": {"doc": big_value}}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b} (declared non-filterable: exempt)");
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({"vectorBucketName": "pvb", "indexName": "blb2",
                          "keys": ["f"], "returnMetadata": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["vectors"][0]["metadata"]["doc"].as_str().unwrap().len(),
            2_100
        );

        // (e) value types: nested object / null / list-of-object rejected with
        // the legal types named; flat list round-trips.
        for bad in [
            json!({"nested": {"a": 1}}),
            json!({"nul": null}),
            json!({"arr": [{"x": 1}]}),
            json!({"arr2": [[1]]}),
        ] {
            let (st, _, b) = call(
                &r,
                "/PutVectors",
                &jbody(json!({
                    "vectorBucketName": "pvb", "indexName": "sml",
                    "vectors": [{"key": "t", "data": {"float32": [0, 1]}, "metadata": bad}]
                })),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{bad} {b}");
            let msg = b["message"].as_str().unwrap();
            assert!(
                msg.contains("string, number, boolean"),
                "message names the legal types: {msg}"
            );
        }
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "sml",
                "vectors": [{"key": "t", "data": {"float32": [0, 1]},
                             "metadata": {"s": "str", "n": 1.5, "b": false,
                                          "l": ["a", 2, true]}}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({"vectorBucketName": "pvb", "indexName": "sml",
                          "keys": ["t"], "returnMetadata": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["vectors"][0]["metadata"],
            json!({"s": "str", "n": 1.5, "b": false, "l": ["a", 2, true]})
        );

        // (f) same-batch duplicate keys: last occurrence wins
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "sml",
                "vectors": [
                    {"key": "d", "data": {"float32": [0, 0]}, "metadata": {"v": 1}},
                    {"key": "d", "data": {"float32": [1, 1]}, "metadata": {"v": 2}}
                ]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        let (st, _, b) = call(
            &r,
            "/GetVectors",
            &jbody(json!({"vectorBucketName": "pvb", "indexName": "sml",
                          "keys": ["d"], "returnData": true, "returnMetadata": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(
            b["vectors"][0]["metadata"]["v"],
            json!(2),
            "last write wins"
        );
        assert_eq!(b["vectors"][0]["data"]["float32"], json!([1.0, 1.0]));

        // empty metadata object is legal
        let (st, _, b) = call(
            &r,
            "/PutVectors",
            &jbody(json!({
                "vectorBucketName": "pvb", "indexName": "sml",
                "vectors": [{"key": "e", "data": {"float32": [0, 1]}, "metadata": {}}]
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
    }
}
