//! vectorfs — single-binary, self-hosted AWS S3 Vectors compatible gateway.
//!
//! Harvested from the verified M0 spike (spike/REPORT.md, 2026-11-03): four
//! operations (CreateVectorBucket / CreateIndex / PutVectors / QueryVectors),
//! hand-rolled SigV4 verification, file + memory + S3-compatible backends
//! (lancedb + object_store, zero AWS SDK). The full 21-operation surface and
//! `vectorfs doctor` land across M1 (design.md §4 / §5.8 / §7 M1).
//!
//! Wire protocol per botocore `s3vectors/2025-07-15` model: rest-json,
//! `POST /<OperationName>`, JSON bodies; PutVectors success = 200 empty body.

pub mod backends;
pub mod config;
pub mod doctor;
pub mod filter;
pub mod ingest;
pub mod lock;
pub mod mcp;
pub mod ops;
pub mod ops_xvf;
pub mod pipeline;
pub mod preset;
pub mod status;
pub mod store;

// Shared wire core (three-crate split, design.md §0 2026-11-05): the SigV4
// signer+verifier pair, the TLS HTTP leaf, and the report vocabulary live in
// `vectorfs-common` so the `vf` client links them WITHOUT this crate. They
// are re-exported at their historical crate-root paths so every internal
// `crate::sigv4` / `crate::report` reference keeps compiling unchanged —
// the split moves code between crates, not names within this one.
pub use vectorfs_common::{report, sigv4};

/// Engine-level experiments carried over from the spike as regression tests
/// (conditional writes, merge_insert x schema evolution — design.md §5.2/§5.3).
#[cfg(test)]
mod experiments;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

/// Max accepted body. Deliberately ABOVE the AWS PutVectors payload limit of
/// 20 MiB: an oversized-but-deliverable body must reach the handler so it can
/// answer 400 ValidationException (a shape SDKs parse) instead of axum's 413
/// (no modeled shape — typed clients surface it as an opaque transport
/// error). Bodies beyond this hard cap are also answered 400 by the gate
/// middleware's read, never a bare 413.
pub const MAX_BODY: usize = 21 * 1024 * 1024;

/// Effective body-buffer cap for one path (the gate middleware and the
/// route-level `DefaultBodyLimit` must agree): `/x-vf/PutSource` accepts
/// raw source bytes up to `max_file_size_mb` headroom (appendix E.1 — the
/// 21 MiB AWS-face cap would reject legitimate files before the handler
/// could shape the error), and a `/x-vf/GetSourceData` RESPONSE can be that
/// large too (the gate buffers responses for tracing). Every other path —
/// the whole AWS face — keeps the 21 MiB cap and its documented 400-on-
/// overflow behavior.
fn body_limit_for(path: &str) -> usize {
    match path {
        "/x-vf/PutSource" | "/x-vf/GetSourceData" => ops_xvf::X_VF_MAX_BODY,
        _ => MAX_BODY,
    }
}

/// Process-start reference for /healthz `uptimeSeconds` — initialized when
/// the router is built (server start), read by every healthz hit.
static STARTED_AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn build_router(state: ops::AppState) -> Router {
    STARTED_AT.get_or_init(std::time::Instant::now);
    let authed = Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/health",
            get(health).layer(axum::extract::DefaultBodyLimit::max(MAX_BODY)),
        )
        .route("/CreateVectorBucket", post(ops::create_vector_bucket))
        .route("/CreateIndex", post(ops::create_index))
        .route("/PutVectors", post(ops::put_vectors))
        .route("/QueryVectors", post(ops::query_vectors))
        // M1 T4 vector data plane: completes the 21-operation surface
        // (design.md 附录A). All POST /<Op>, 200 with JSON (delete: empty).
        .route("/GetVectors", post(ops::get_vectors))
        .route("/ListVectors", post(ops::list_vectors))
        .route("/DeleteVectors", post(ops::delete_vectors))
        // M1 T3 management plane (design.md §4 / 附录A): all POST /<Op> …
        .route("/GetVectorBucket", post(ops::get_vector_bucket))
        .route("/DeleteVectorBucket", post(ops::delete_vector_bucket))
        .route("/ListVectorBuckets", post(ops::list_vector_buckets))
        .route("/GetIndex", post(ops::get_index))
        .route("/DeleteIndex", post(ops::delete_index))
        .route("/ListIndexes", post(ops::list_indexes))
        .route(
            "/PutVectorBucketPolicy",
            post(ops::put_vector_bucket_policy),
        )
        .route(
            "/GetVectorBucketPolicy",
            post(ops::get_vector_bucket_policy),
        )
        .route(
            "/DeleteVectorBucketPolicy",
            post(ops::delete_vector_bucket_policy),
        )
        .route(
            "/PutVectorBucketDefaultIndexMode",
            post(ops::put_vector_bucket_default_index_mode),
        )
        .route("/UpdateIndexMode", post(ops::update_index_mode))
        // … except the three /tags/{resourceArn} operations (model URIs:
        // POST/DELETE/GET /tags/{resourceArn}). Wildcard tail + manual
        // percent-decode in the handlers: boto3 sends the whole ARN as one
        // fully-encoded label (':'->%3A, '/'->%2F), other clients may send
        // the literal multi-segment form — both must route (ops.rs module doc).
        .route(
            "/tags/{*resource_arn}",
            post(ops::tag_resource)
                .delete(ops::untag_resource)
                .get(ops::list_tags_for_resource),
        )
        // ---- /x-vf file-layer superset (M2 T6, design.md 附录E) ----
        // Same gate (SigV4 with signingName s3vectors applies verbatim),
        // same rest-json error protocol; PutSource alone widens the body
        // limit (see `body_limit_for`).
        .route(
            "/x-vf/PutSource",
            post(ops_xvf::put_source)
                .layer(axum::extract::DefaultBodyLimit::max(ops_xvf::X_VF_MAX_BODY)),
        )
        .route("/x-vf/GetSource", post(ops_xvf::get_source))
        .route(
            "/x-vf/GetSourceData",
            post(ops_xvf::get_source_data)
                .layer(axum::extract::DefaultBodyLimit::max(ops_xvf::X_VF_MAX_BODY)),
        )
        .route("/x-vf/ListSources", post(ops_xvf::list_sources))
        .route("/x-vf/DeleteSource", post(ops_xvf::delete_source))
        .route("/x-vf/Search", post(ops_xvf::search))
        .route("/x-vf/Reindex", post(ops_xvf::reindex))
        .route("/x-vf/GetIngestStatus", post(ops_xvf::get_ingest_status))
        .fallback(fallback_404);
    authed
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gate_middleware,
        ))
        .with_state(state)
}

/// /healthz (M1 T6 upgrade): `{"ok": true}` is kept verbatim — liveness
/// probes keyed on it stay compatible — with `version`, `uptimeSeconds`
/// (added for `vf status --endpoint`, see STARTED_AT above) and `engines`
/// (engine observability, 2026-11-05: the extraction/embedding engine
/// versions this binary runs — additive members only, same rule as before).
async fn healthz() -> Json<serde_json::Value> {
    let uptime = STARTED_AT
        .get()
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    Json(serde_json::json!({
        "ok": true,
        "version": report::version_string(),
        "uptimeSeconds": uptime,
        "engines": pipeline::engines::engine_versions(),
    }))
}

/// GET /health (M2 T6, appendix E): the CONTAINER healthcheck probe —
/// no-auth (see `gate_middleware`), unauthenticated readers must never need
/// credentials to tell "up" from "down". `{"ok":true}` plus the version and
/// the engine table (identity members — same additive rule as /healthz's;
/// `vf status`'s single unsigned probe carries them). The uptime member
/// stays on /healthz only (telemetry, not identity), and `vf status` reads
/// BOTH (health for up/down + engines, GetIngestStatus for depth).
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "version": report::version_string(),
        "engines": pipeline::engines::engine_versions(),
    }))
}

/// Unknown path/method -> plain 404 JSON.
async fn fallback_404(uri: Uri) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "__type": "NotFoundException",
            "message": format!("no route for {}", uri.path()),
        })),
    )
        .into_response()
}

/// Headers -> JSON object preserving multi-values (M0 Task C wire observation:
/// nothing may be dropped).
fn headers_to_json(headers: &HeaderMap) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let v = String::from_utf8_lossy(value.as_bytes()).into_owned();
        m.entry(name.as_str().to_string())
            .and_modify(|e| {
                if let serde_json::Value::Array(a) = e {
                    a.push(serde_json::Value::String(v.clone()));
                }
            })
            .or_insert_with(|| serde_json::json!([v]));
    }
    serde_json::Value::Object(m)
}

fn trace_line(event: &str, mut fields: serde_json::Map<String, serde_json::Value>) {
    fields.insert("ev".into(), serde_json::json!(event));
    eprintln!("{}", serde_json::Value::Object(fields));
}

/// The single gate: (1) buffer body, (2) emit request trace, (3) verify SigV4
/// (except /healthz), (4) call inner service, (5) buffer + emit response trace.
///
/// Note: buffering the response body means the trace always shows the final
/// bytes; /healthz and 404s are traced too (they are part of the wire picture).
async fn gate_middleware(State(state): State<ops::AppState>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let request_limit = body_limit_for(parts.uri.path());
    let bytes = match axum::body::to_bytes(body, request_limit).await {
        Ok(b) => b,
        Err(e) => {
            return ops::ApiError::validation(format!("failed to read body: {e}")).into_response();
        }
    };

    if state.trace {
        let mut f = serde_json::Map::new();
        f.insert("method".into(), serde_json::json!(parts.method.as_str()));
        f.insert(
            "path".into(),
            serde_json::json!(
                parts
                    .uri
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or("")
            ),
        );
        f.insert("headers".into(), headers_to_json(&parts.headers));
        f.insert(
            "body".into(),
            serde_json::json!(String::from_utf8_lossy(&bytes).into_owned()),
        );
        trace_line("http_request", f);
    }

    if parts.uri.path() != "/healthz"
        && parts.uri.path() != "/health"
        && let Some(creds) = &state.creds
    {
        let header_pairs: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let uri = &parts.uri;
        let res = sigv4::verify(
            parts.method.as_str(),
            uri.path(),
            uri.query(),
            &header_pairs,
            &bytes,
            creds,
            &state.region,
            now,
        );
        if let Err(msg) = res {
            let resp = ops::ApiError::access_denied(msg).into_response();
            // auth failures short-circuit before `next` — trace them too so
            // every wire exchange is visible.
            let (rparts, rbody) = resp.into_parts();
            let rbytes = axum::body::to_bytes(rbody, MAX_BODY)
                .await
                .unwrap_or_default();
            if state.trace {
                let mut f = serde_json::Map::new();
                f.insert("status".into(), serde_json::json!(rparts.status.as_u16()));
                f.insert("headers".into(), headers_to_json(&rparts.headers));
                f.insert(
                    "body".into(),
                    serde_json::json!(String::from_utf8_lossy(&rbytes).into_owned()),
                );
                trace_line("http_response", f);
            }
            return Response::from_parts(rparts, Body::from(rbytes));
        }
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    let res = next.run(req).await;

    let (rparts, rbody) = res.into_parts();
    let rbytes = match axum::body::to_bytes(rbody, request_limit).await {
        Ok(b) => b,
        Err(e) => {
            return ops::ApiError::internal(format!("failed to read response body: {e}"))
                .into_response();
        }
    };

    if state.trace {
        let mut f = serde_json::Map::new();
        f.insert("status".into(), serde_json::json!(rparts.status.as_u16()));
        f.insert("headers".into(), headers_to_json(&rparts.headers));
        f.insert(
            "body".into(),
            serde_json::json!(String::from_utf8_lossy(&rbytes).into_owned()),
        );
        trace_line("http_response", f);
    }

    Response::from_parts(rparts, Body::from(rbytes))
}

// ---------------------------------------------------------------------------
// tests (in-process: tower::oneshot, no network port)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt; // oneshot

    fn test_state(dir: &std::path::Path, creds: Option<sigv4::Credentials>) -> ops::AppState {
        ops::AppState {
            storage: std::sync::Arc::new(store::Storage::file(dir)),
            creds,
            region: "us-east-1".into(),
            account: "000000000000".into(),
            trace: false,
            ingest: None,
        }
    }

    async fn send(
        router: &Router,
        req: Request<Body>,
    ) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
        let res = router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers: Vec<(String, String)> = res
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = axum::body::to_bytes(res.into_body(), MAX_BODY)
            .await
            .unwrap();
        let val = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&body).into_owned())
            })
        };
        (status, headers, val)
    }

    fn post_json(path: &str, body: &str, extra: &[(&str, &str)]) -> Request<Body> {
        let mut b = HttpRequest::post(path)
            .header("content-type", "application/json")
            .header("host", "127.0.0.1:8787");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    // ---------------- SigV4 request-level tests (auth ON) ----------------

    fn creds() -> sigv4::Credentials {
        sigv4::Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    fn signed_post(path: &str, body: &str, c: &sigv4::Credentials, date: &str) -> Request<Body> {
        let s = sigv4::signer::sign(
            "POST",
            path,
            None,
            "127.0.0.1:8787",
            body.as_bytes(),
            c,
            "us-east-1",
            "s3vectors",
            date,
            &[("content-type", "application/json")],
        );
        post_json(
            path,
            body,
            &[
                ("authorization", &s.authorization),
                ("x-amz-date", &s.x_amz_date),
            ],
        )
    }

    /// /healthz (M1 T6): `ok:true` stays byte-stable (liveness probes keyed
    /// on it keep working); version + uptimeSeconds are ADDITIVE members, and
    /// so is the engine table (engine observability, 2026-11-05).
    #[tokio::test]
    async fn healthz_keeps_ok_and_adds_version_uptime() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), None));
        let req = HttpRequest::get("/healthz").body(Body::empty()).unwrap();
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], serde_json::json!(true));
        assert!(
            body["version"].as_str().unwrap().starts_with("0."),
            "{body}"
        );
        assert!(
            body["uptimeSeconds"].as_f64().is_some_and(|u| u >= 0.0),
            "{body}"
        );
        // the engine table rides along (the SAME map the marker persists)
        assert_eq!(
            body["engines"]["pdfium"],
            serde_json::json!(pipeline::engines::PDFIUM_PIN),
            "{body}"
        );
    }

    /// /health (M2 T6, appendix E): the CONTAINER healthcheck probe —
    /// NO-AUTH even when credentials are configured (an unsigned GET must
    /// pass the same gate that 403s unsigned AWS-face calls), while the
    /// signed surface stays protected. /healthz keeps its own exemption.
    #[tokio::test]
    async fn health_is_no_auth_under_configured_creds() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        let req = HttpRequest::get("/health").body(Body::empty()).unwrap();
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], serde_json::json!(true));
        assert!(body["version"].is_string(), "{body}");
        // /health carries the engine table too (vf status's single unsigned
        // probe reports it; the container HEALTHCHECK only reads the exit
        // code, so the additive member costs nothing)
        assert!(
            body["engines"]
                .as_object()
                .is_some_and(|e| e.contains_key("pdfium")),
            "{body}"
        );
        // and the control: an unsigned AWS-face call still 403s
        let req = post_json("/PutVectors", r#"{"vectors":[]}"#, &[]);
        let (status, _, _) = send(&router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sigv4_valid_request_passes_and_creates_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        let req = signed_post(
            "/CreateVectorBucket",
            r#"{"vectorBucketName":"auth-bucket"}"#,
            &creds(),
            &sigv4::signer::now_amz_date(),
        );
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["vectorBucket"]["vectorBucketName"],
            serde_json::json!("auth-bucket")
        );
    }

    #[tokio::test]
    async fn sigv4_tampered_body_rejected_403() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        let req = signed_post(
            "/CreateVectorBucket",
            r#"{"vectorBucketName":"legit"}"#,
            &creds(),
            &sigv4::signer::now_amz_date(),
        );
        // flip one byte of the body after signing
        let (parts, body) = req.into_parts();
        let raw = axum::body::to_bytes(body, MAX_BODY).await.unwrap();
        let mut raw = raw.to_vec();
        let last = raw.len() - 3; // inside the name
        raw[last] = b'X';
        let req = Request::from_parts(parts, Body::from(raw));
        let (status, headers, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            header_of(&headers, "x-amzn-errortype"),
            Some("AccessDeniedException")
        );
        assert_eq!(body["__type"], serde_json::json!("AccessDeniedException"));
        assert!(body["message"].is_string());
    }

    #[tokio::test]
    async fn sigv4_wrong_access_key_rejected_403() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        let evil = sigv4::Credentials {
            access_key: "AKIDATTACKER".into(),
            secret_key: "nope".into(),
        };
        let req = signed_post(
            "/PutVectors",
            r#"{"vectors":[]}"#,
            &evil,
            &sigv4::signer::now_amz_date(),
        );
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["__type"], serde_json::json!("AccessDeniedException"));
    }

    #[tokio::test]
    async fn sigv4_stale_date_rejected_403() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        // signed 2h in the past
        let stale = {
            sigv4::signer::amz_date_from_epoch(
                std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600),
            )
        };
        let req = signed_post("/PutVectors", r#"{"vectors":[]}"#, &creds(), &stale);
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["__type"], serde_json::json!("AccessDeniedException"));
        assert!(body["message"].as_str().unwrap().contains("skew"));
    }

    #[tokio::test]
    async fn sigv4_missing_authorization_rejected_403() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        let req = post_json("/PutVectors", r#"{"vectors":[]}"#, &[]);
        let (status, _, _) = send(&router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
