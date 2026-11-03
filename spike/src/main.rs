//! vectorfs M0 spike server — Task B: minimal s3vectors-compatible server.
//! 4 operations (CreateVectorBucket / CreateIndex / PutVectors / QueryVectors),
//! hand-rolled SigV4 verification, file backend via lancedb.
//!
//! Wire protocol per botocore s3vectors 2025-07-15 model: rest-json,
//! `POST /<OperationName>`, JSON bodies; PutVectors success = 200 empty body.

mod ops;
mod s3;
mod sigv4;
mod store;

#[cfg(test)]
mod taskd_tests;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use ops::AppState;
use serde_json::json;

/// Max accepted body (AWS PutVectors limit is 500 vectors / 20 MiB).
const MAX_BODY: usize = 21 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "vf-spike", version, about = "vectorfs M0 spike server (throwaway)")]
struct Args {
    /// TCP port to listen on (env override: VF_PORT)
    #[arg(long, env = "VF_PORT", default_value_t = 8787)]
    port: u16,

    /// Data directory for the file backend
    #[arg(long, default_value = "./spike-data")]
    data_dir: String,

    /// Backend URI: `file:<dir>` (default, --data-dir shorthand) or
    /// `s3+http://host:port/bucket[/prefix]` (S3-compatible, Task D).
    #[arg(long)]
    backend: Option<String>,

    /// S3 region (default us-east-1; MinIO/seaweedfs convention)
    #[arg(long, default_value = "us-east-1")]
    s3_region: String,

    /// S3 static access key (env override: VF_S3_ACCESS_KEY / AWS_ACCESS_KEY_ID)
    #[arg(long, env = "VF_S3_ACCESS_KEY")]
    s3_access_key: Option<String>,

    /// S3 static secret key (env override: VF_S3_SECRET_KEY / AWS_SECRET_ACCESS_KEY)
    #[arg(long, env = "VF_S3_SECRET_KEY")]
    s3_secret_key: Option<String>,

    /// Disable SigV4 verification entirely (CI mode)
    #[arg(long)]
    no_auth: bool,

    /// Static access key for SigV4 (env override: VF_ACCESS_KEY)
    #[arg(long, env = "VF_ACCESS_KEY")]
    access_key: Option<String>,

    /// Static secret key for SigV4 (env override: VF_SECRET_KEY)
    #[arg(long, env = "VF_SECRET_KEY")]
    secret_key: Option<String>,

    /// Region clients must sign for (enters credential scope + ARNs)
    #[arg(long, default_value = "us-east-1")]
    region: String,

    /// One-line-JSON request/response tracing to stderr
    #[arg(long)]
    trace: bool,
}

pub fn build_router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/healthz", get(healthz))
        .route("/CreateVectorBucket", post(ops::create_vector_bucket))
        .route("/CreateIndex", post(ops::create_index))
        .route("/PutVectors", post(ops::put_vectors))
        .route("/QueryVectors", post(ops::query_vectors))
        .fallback(fallback_404);
    authed
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gate_middleware,
        ))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let creds = match (
        args.access_key.as_deref(),
        args.secret_key.as_deref(),
    ) {
        (Some(ak), Some(sk)) if !ak.is_empty() && !sk.is_empty() => {
            Some(sigv4::Credentials {
                access_key: ak.to_string(),
                secret_key: sk.to_string(),
            })
        }
        _ => None,
    };
    if args.no_auth {
        // explicit opt-out
    } else if creds.is_none() {
        eprintln!(
            "vf-spike: WARNING no credentials configured (--access-key/--secret-key or \
             VF_ACCESS_KEY/VF_SECRET_KEY) and --no-auth not given; running WITHOUT auth"
        );
    }

    let auth_desc = if args.no_auth || creds.is_none() {
        "none".to_string()
    } else {
        format!("sigv4(access_key={})", args.access_key.as_deref().unwrap_or(""))
    };

    let backend_spec = args.backend.clone().unwrap_or_else(|| format!("file:{}", args.data_dir));
    let storage = if let Some(dir) = backend_spec.strip_prefix("file:") {
        store::Storage::file(std::path::PathBuf::from(dir))
    } else if let Some(mut cfg) = s3::S3BackendConfig::parse(&backend_spec) {
        cfg.region = args.s3_region.clone();
        cfg.access_key = args
            .s3_access_key
            .clone()
            .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok());
        cfg.secret_key = args
            .s3_secret_key
            .clone()
            .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok());
        match store::Storage::s3(cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("vf-spike: S3 backend init failed: {e}");
                std::process::exit(2);
            }
        }
    } else {
        eprintln!(
            "vf-spike: cannot parse --backend {backend_spec:?} (expected file:<dir> or s3+http://host:port/bucket[/prefix])"
        );
        std::process::exit(2);
    };
    let state = AppState {
        storage: std::sync::Arc::new(storage),
        creds: if args.no_auth { None } else { creds },
        region: args.region.clone(),
        account: "000000000000".to_string(),
        trace: args.trace,
    };

    eprintln!(
        "vf-spike: starting on 127.0.0.1:{} backend={:?} auth={} region={} account={} trace={}",
        args.port,
        state.storage,
        auth_desc,
        args.region,
        state.account,
        args.trace
    );

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .expect("bind 127.0.0.1 failed");
    axum::serve(listener, app).await.expect("server exited");
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

/// Unknown path/method -> plain 404 JSON (task spec: simple is fine).
async fn fallback_404(uri: Uri) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "__type": "NotFoundException",
            "message": format!("no route for {}", uri.path()),
        })),
    )
        .into_response()
}

/// Headers -> JSON object preserving multi-values (Task C wire observation:
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
    fields.insert("ev".into(), json!(event));
    eprintln!("{}", serde_json::Value::Object(fields));
}

/// The single gate: (1) buffer body, (2) emit request trace, (3) verify SigV4
/// (except /healthz), (4) call inner service, (5) buffer + emit response trace.
///
/// Note: buffering the response body means the trace always shows the final
/// bytes; /healthz and 404s are traced too (they are part of the wire picture).
async fn gate_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return ops::ApiError::validation(format!("failed to read body: {e}")).into_response()
        }
    };

    if state.trace {
        let mut f = serde_json::Map::new();
        f.insert("method".into(), json!(parts.method.as_str()));
        f.insert(
            "path".into(),
            json!(parts.uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("")),
        );
        f.insert("headers".into(), headers_to_json(&parts.headers));
        f.insert(
            "body".into(),
            json!(String::from_utf8_lossy(&bytes).into_owned()),
        );
        trace_line("http_request", f);
    }

    if parts.uri.path() != "/healthz" {
        if let Some(creds) = &state.creds {
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
                // Task C sees every wire exchange.
                let (rparts, rbody) = resp.into_parts();
                let rbytes = axum::body::to_bytes(rbody, MAX_BODY)
                    .await
                    .unwrap_or_default();
                if state.trace {
                    let mut f = serde_json::Map::new();
                    f.insert("status".into(), json!(rparts.status.as_u16()));
                    f.insert("headers".into(), headers_to_json(&rparts.headers));
                    f.insert(
                        "body".into(),
                        json!(String::from_utf8_lossy(&rbytes).into_owned()),
                    );
                    trace_line("http_response", f);
                }
                return Response::from_parts(rparts, Body::from(rbytes));
            }
        }
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    let res = next.run(req).await;

    let (rparts, rbody) = res.into_parts();
    let rbytes = match axum::body::to_bytes(rbody, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return ops::ApiError::internal(format!("failed to read response body: {e}"))
                .into_response()
        }
    };

    if state.trace {
        let mut f = serde_json::Map::new();
        f.insert("status".into(), json!(rparts.status.as_u16()));
        f.insert("headers".into(), headers_to_json(&rparts.headers));
        f.insert(
            "body".into(),
            json!(String::from_utf8_lossy(&rbytes).into_owned()),
        );
        trace_line("http_response", f);
    }

    Response::from_parts(rparts, Body::from(rbytes))
}

// ---------------------------------------------------------------------------
// tests (bin crate -> all integration tests live in-crate; tower::oneshot, no
// network port)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt; // oneshot

    fn test_state(dir: &std::path::Path, creds: Option<sigv4::Credentials>) -> AppState {
        AppState {
            storage: std::sync::Arc::new(store::Storage::file(dir)),
            creds,
            region: "us-east-1".into(),
            account: "000000000000".into(),
            trace: false,
        }
    }

    async fn send(router: &Router, req: Request<Body>) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
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
            json!("auth-bucket")
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
        assert_eq!(header_of(&headers, "x-amzn-errortype"), Some("AccessDeniedException"));
        assert_eq!(body["__type"], json!("AccessDeniedException"));
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
        assert_eq!(body["__type"], json!("AccessDeniedException"));
    }

    #[tokio::test]
    async fn sigv4_stale_date_rejected_403() {
        let dir = tempfile::tempdir().unwrap();
        let router = build_router(test_state(dir.path(), Some(creds())));
        // signed 2h in the past
        let stale = {
            let now = sigv4::signer::amz_date_from_epoch(
                std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600),
            );
            now
        };
        let req = signed_post("/PutVectors", r#"{"vectors":[]}"#, &creds(), &stale);
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["__type"], json!("AccessDeniedException"));
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
