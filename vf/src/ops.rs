//! S3 Vectors operation handlers (wire layer) for the four operations
//! implemented so far (21-operation surface completes across M1, design.md §4).
//! Carried over from the M0 spike, adapted to the product crate.
//! Field names are verbatim from the botocore `s3vectors/2025-07-15` model
//! (service-2.json, develop branch, synced 2026-09-30 — NOTES.md has the extraction).
//!
//! Error wire protocol (design.md §4.3): HTTP status +
//! `X-Amzn-Errortype: <Shape>` header + body `{"__type":"<Shape>","message":"..."}`
//! (both emitted). Shapes: ValidationException=400, NotFoundException=404,
//! ConflictException=409, AccessDeniedException=403, InternalServerException=500.
//!
//! TODO(AWS semantics, unverified — see spike/NOTES.md「遗留 TODO」):
//! - exact AWS error message texts (we use descriptive stand-ins);
//! - malformed-JSON request body: real service likely 400 SerializationException-ish,
//!   we map it to ValidationException;
//! - CreateVectorBucket/CreateIndex response: model says {vectorBucketArn}/{indexArn}
//!   only; AWS CLI docs show {vectorBucket}/{index}; we double-emit (SDKs
//!   ignore unknown members).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;

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
}

impl ApiError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            shape: "ValidationException",
            message: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            shape: "NotFoundException",
            message: msg.into(),
        }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            shape: "ConflictException",
            message: msg.into(),
        }
    }
    pub fn access_denied(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            shape: "AccessDeniedException",
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            shape: "InternalServerException",
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "__type": self.shape,
            "message": self.message,
        });
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
    vector_bucket_name: String,
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
    vector_bucket_name: String,
    index_name: String,
    vectors: Vec<PutInputVector>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryVectorsInput {
    vector_bucket_name: String,
    index_name: String,
    top_k: Option<i64>,
    query_vector: VectorDataInput,
    filter: Option<serde_json::Value>,
    return_distance: Option<bool>,
    return_metadata: Option<bool>,
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

    if state
        .storage
        .load_bucket(&input.vector_bucket_name)
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!(
            "vector bucket {} does not exist",
            input.vector_bucket_name
        )));
    }

    let (epoch, rfc3339) = now();
    let meta = IndexMeta {
        vector_bucket_name: input.vector_bucket_name.clone(),
        index_name: input.index_name.clone(),
        index_arn: arn_index(&state, &input.vector_bucket_name, &input.index_name),
        creation_time_rfc3339: rfc3339,
        creation_time_epoch: epoch,
        data_type: "float32".into(),
        dimension: input.dimension as i32,
        distance_metric: input.distance_metric.clone(),
        non_filterable_metadata_keys: input
            .metadata_configuration
            .map(|c| c.non_filterable_metadata_keys),
    };
    state.storage.create_index(&meta).await?;

    let mut index_json = serde_json::json!({
        "vectorBucketName": meta.vector_bucket_name,
        "indexName": meta.index_name,
        "indexArn": meta.index_arn,
        "creationTime": epoch,
        "dataType": "float32",
        "dimension": meta.dimension,
        "distanceMetric": meta.distance_metric,
    });
    if let Some(ks) = &meta.non_filterable_metadata_keys {
        index_json["metadataConfiguration"] = serde_json::json!({
            "nonFilterableMetadataKeys": ks,
        });
    }
    let resp = serde_json::json!({
        "indexArn": meta.index_arn, // model-required member
        "index": index_json,        // documented shape; SDKs ignore extras
    });
    Ok(Json(resp).into_response())
}

pub async fn put_vectors(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let input: PutVectorsInput = parse_body(&body)?;
    let meta = resolve_index(&state, &input.vector_bucket_name, &input.index_name).await?;

    if input.vectors.is_empty() || input.vectors.len() > 500 {
        return Err(ApiError::validation(format!(
            "vectors must contain between 1 and 500 items, got {}",
            input.vectors.len()
        )));
    }

    let mut rows = Vec::with_capacity(input.vectors.len());
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
            Some(m) => Some(m.to_string()),
        };
        rows.push(VectorRow {
            key: v.key,
            vector: vec,
            metadata_json,
        });
    }

    state
        .storage
        .upsert_vectors(
            &input.vector_bucket_name,
            &input.index_name,
            meta.dimension,
            rows,
        )
        .await?;
    Ok(empty_ok())
}

pub async fn query_vectors(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input: QueryVectorsInput = parse_body(&body)?;
    let meta = resolve_index(&state, &input.vector_bucket_name, &input.index_name).await?;

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
            &input.vector_bucket_name,
            &input.index_name,
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
    }

    async fn call(
        router: &axum::Router,
        path: &str,
        body: &str,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        let req = Request::post(path)
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
}
