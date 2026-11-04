//! `vf` CLIENT transport (M2 T6, appendix E.1/E.4): the signed-HTTP slice
//! the `vf` binary talks to a running `vectorfs serve` daemon with.
//!
//! Deliberately reuses the SERVER's own primitives — the SigV4 signer
//! ([`crate::sigv4::signer`], the same canonicalization the gate verifies
//! against, so the pair stays byte-exact by construction) and the TLS-capable
//! HTTP leaf ([`crate::pipeline::embed::http`], aws-lc-rs rustls — local
//! `http://` and remote `https://` endpoints both work). No engine/server
//! module is referenced, so the `vf` binary still links none of lancedb/axum.
//!
//! Connection resolution (appendix E.4): `--endpoint` > `VF_ENDPOINT`;
//! `--access-key`/`--secret-key` > `VF_ACCESS_KEY`/`VF_SECRET_KEY`. The
//! SERVER's vectorfs.toml credentials are deliberately NOT auto-read — the
//! client may run on a different machine; the hint points at
//! `vectorfs init`'s printed env snippet instead.
//!
//! Error contract (§5.8): every failure surfaces as a report::Failure
//! `{code, message, hint}` — transport trouble as `ENDPOINT_UNREACHABLE`,
//! wire errors carrying the server's own error shape name as `code`.

use bytes::Bytes;
use http_body_util::BodyExt as _;
use http_body_util::Full;

use crate::pipeline::embed::http;
use crate::report::Failure;
use crate::sigv4::{Credentials, signer};

/// Percent-encode a `x-vf-*` header VALUE: HTTP header values are ASCII-only,
/// so controls, space, `%` (round-trip safety) and every non-ASCII byte are
/// escaped; the server percent-decodes with the exact inverse
/// (ops_xvf::put_source). Always-encode/always-decode on both ends keeps the
/// mapping bijective for any key.
pub fn encode_header_value(v: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ').add(b'%');
    percent_encoding::utf8_percent_encode(v, SET).to_string()
}

/// What a signed call came back with when it did not succeed.
#[derive(Debug)]
pub enum CallError {
    /// The server answered an HTTP error status: its `__type` shape
    /// (ValidationException, NotFoundException, …) and message.
    Http {
        status: u16,
        shape: String,
        message: String,
    },
    /// The request never completed (connect/reset/timeout/parse).
    Transport(String),
}

impl CallError {
    /// Map to the CLI's structured failure triple. Exit codes: 1 — the
    /// command saw a well-formed negative answer; usage/config errors (2)
    /// happen earlier, at argument resolution.
    pub fn to_failure(&self, endpoint: &str) -> Failure {
        match self {
            CallError::Http {
                status,
                shape,
                message,
            } => {
                // stable short code derived from the server's error shape
                // (ValidationException -> Validation); a closed set because
                // Failure::code is &'static str
                let (code, hint): (&'static str, &'static str) = match shape.as_str() {
                    "ValidationException" => (
                        "Validation",
                        "fix the arguments or the pipeline config the message names \
                         (see `vectorfs pipelines check`)",
                    ),
                    "NotFoundException" => {
                        ("NotFound", "verify the key/bucket with: vf sources list")
                    }
                    "AccessDeniedException" => (
                        "AccessDenied",
                        "check VF_ACCESS_KEY/VF_SECRET_KEY against the daemon's vectorfs.toml \
                         (printed by `vectorfs init`)",
                    ),
                    "ConflictException" => (
                        "Conflict",
                        "the resource state changed; re-check and retry (writes are idempotent)",
                    ),
                    _ => (
                        "ServerError",
                        "check the daemon's stderr log; vf status probes it",
                    ),
                };
                Failure::new(code, format!("HTTP {status} {shape}: {message}"), hint)
            }
            CallError::Transport(msg) => Failure::new(
                "ENDPOINT_UNREACHABLE",
                format!("request to {endpoint} failed: {msg}"),
                "is `vectorfs serve` running at that endpoint? try: vf status --endpoint …",
            ),
        }
    }
}

/// A resolved daemon connection: endpoint + credentials + region + one
/// shared HTTP client (pooled keep-alive).
pub struct Connection {
    endpoint: String,
    authority: String,
    region: String,
    creds: Credentials,
    client: http::HttpClient,
}

impl Connection {
    /// flag > env resolution for the four client knobs; every miss is a
    /// structured usage failure (exit 2 at the call site).
    pub fn resolve(
        endpoint: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        region: Option<String>,
    ) -> Result<Connection, Failure> {
        let endpoint = endpoint
            .or_else(|| env_nonempty("VF_ENDPOINT"))
            .ok_or_else(|| {
                Failure::new(
                    "NO_ENDPOINT",
                    "no endpoint given and VF_ENDPOINT is unset",
                    "pass --endpoint http://127.0.0.1:8787 (or export VF_ENDPOINT; the daemon \
                     prints its endpoint at startup)",
                )
            })?;
        let access_key = access_key
            .or_else(|| env_nonempty("VF_ACCESS_KEY"))
            .ok_or_else(|| {
                Failure::new(
                    "NO_CREDENTIALS",
                    "no access key given and VF_ACCESS_KEY is unset",
                    "pass --access-key (or export VF_ACCESS_KEY/VF_SECRET_KEY — the values are \
                     printed by `vectorfs init` / stored in the daemon's vectorfs.toml)",
                )
            })?;
        let secret_key = secret_key
            .or_else(|| env_nonempty("VF_SECRET_KEY"))
            .ok_or_else(|| {
                Failure::new(
                    "NO_CREDENTIALS",
                    "no secret key given and VF_SECRET_KEY is unset",
                    "pass --secret-key (or export VF_ACCESS_KEY/VF_SECRET_KEY — the values are \
                     printed by `vectorfs init` / stored in the daemon's vectorfs.toml)",
                )
            })?;
        let region = region
            .or_else(|| env_nonempty("VF_REGION"))
            .unwrap_or_else(|| "us-east-1".to_string());

        let (endpoint, authority) = parse_endpoint(endpoint)?;
        Ok(Connection {
            endpoint,
            authority,
            region,
            creds: Credentials {
                access_key,
                secret_key,
            },
            client: http::client(),
        })
    }

    /// Credential-free constructor for the HEALTH-ONLY probe path: endpoint
    /// resolution exactly like [`Connection::resolve`], dummy credentials.
    /// ONLY `get_health` (unsigned GET /health) may be called on the result
    /// — signed calls would send nonsense. Exists so `vf status --endpoint
    /// …` works inside a container HEALTHCHECK where no VF_ACCESS_KEY is
    /// present (the daemon's credentials live in the data volume's
    /// vectorfs.toml, not the probe's environment; §5.10.7).
    pub fn resolve_probe(endpoint: Option<String>) -> Result<Connection, Failure> {
        let endpoint = endpoint
            .or_else(|| env_nonempty("VF_ENDPOINT"))
            .ok_or_else(|| {
                Failure::new(
                    "NO_ENDPOINT",
                    "no endpoint given and VF_ENDPOINT is unset",
                    "pass --endpoint http://127.0.0.1:8787 (or export VF_ENDPOINT)",
                )
            })?;
        let (endpoint, authority) = parse_endpoint(endpoint)?;
        Ok(Connection {
            endpoint,
            authority,
            region: "us-east-1".to_string(),
            creds: Credentials {
                access_key: String::new(),
                secret_key: String::new(),
            },
            client: http::client(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// One signed `POST /<op>` round trip. `extra_headers` ride the signature
    /// (SignedHeaders) — the x-vf-* metadata of PutSource. Returns
    /// `(status, content-type, body)` on any HTTP answer; HTTP >= 400 is an
    /// `Err(CallError::Http)` with the server's error shape.
    pub async fn call(
        &self,
        op_path: &str,
        body: Vec<u8>,
        extra_headers: &[(String, String)],
        content_type: &str,
    ) -> Result<(u16, String, Bytes), CallError> {
        let mut headers: Vec<(String, String)> =
            vec![("content-type".to_string(), content_type.to_string())];
        headers.extend(extra_headers.iter().cloned());
        let ref_pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let signed = signer::sign(
            "POST",
            op_path,
            None,
            &self.authority,
            &body,
            &self.creds,
            &self.region,
            "s3vectors",
            &signer::now_amz_date(),
            &ref_pairs,
        );

        let url = format!("{}{}", self.endpoint, op_path);
        let mut req = ::http::Request::builder()
            .method("POST")
            .uri(&url)
            .header("x-amz-date", &signed.x_amz_date)
            .header("authorization", &signed.authorization);
        for &(k, v) in &ref_pairs {
            req = req.header(k, v);
        }
        let req = req
            .body(Full::new(Bytes::from(body)))
            .expect("static request parts");

        // generous per-request ceiling: a 200 MiB PutSource on a slow link is
        // legitimate; polling loops apply their own, tighter deadlines.
        let resp = match tokio::time::timeout(
            std::time::Duration::from_secs(600),
            self.client.request(req),
        )
        .await
        {
            Ok(r) => r.map_err(|e| CallError::Transport(e.to_string()))?,
            Err(_) => {
                return Err(CallError::Transport("timed out after 600s".to_string()));
            }
        };
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| CallError::Transport(e.to_string()))?
            .to_bytes();
        if status >= 400 {
            let (shape, message) = parse_error_body(&body);
            return Err(CallError::Http {
                status,
                shape,
                message,
            });
        }
        Ok((status, content_type, body))
    }

    /// Signed JSON call: body serialized, response parsed as JSON (empty
    /// bodies — the AWS face's delete-style 200s — come back as Null).
    pub async fn call_json(
        &self,
        op: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let path = format!("/x-vf/{op}");
        let (_, _, bytes) = self
            .call(
                &path,
                serde_json::to_vec(body).expect("serializes"),
                &[],
                "application/json",
            )
            .await?;
        if bytes.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| CallError::Transport(format!("response is not JSON: {e}")))
    }

    /// Unsigned `GET /health` (no-auth by design, appendix E). 5s budget.
    pub async fn get_health(&self) -> Result<serde_json::Value, Failure> {
        let url = format!("{}/health", self.endpoint);
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.client
                .get(url.parse::<::http::Uri>().expect("valid uri")),
        )
        .await
        .map_err(|_| {
            Failure::new(
                "ENDPOINT_TIMEOUT",
                format!("GET {url} timed out after 5s"),
                "is a vectorfs daemon listening there? (vf status probes /health)",
            )
        })?
        .map_err(|e| {
            Failure::new(
                "ENDPOINT_UNREACHABLE",
                format!("GET {url} failed: {e}"),
                "is `vectorfs serve` running at that endpoint?",
            )
        })?;
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| {
                Failure::new(
                    "ENDPOINT_READ",
                    format!("reading the health body failed: {e}"),
                    "retry; if it persists the daemon is unhealthy",
                )
            })?
            .to_bytes();
        if !status.is_success() {
            return Err(Failure::new(
                "ENDPOINT_HTTP_ERROR",
                format!("GET {url} answered HTTP {status}"),
                "the daemon is reachable but unhealthy — check its stderr log",
            ));
        }
        serde_json::from_slice(&body).map_err(|_| {
            Failure::new(
                "ENDPOINT_READ",
                "the /health body is not JSON".to_string(),
                "is another service bound to that port?",
            )
        })
    }
}

/// Parse an error body into (shape, message): the double-emitted rest-json
/// form `{"__type": "...", "message": "..."}` with graceful fallbacks.
fn parse_error_body(body: &[u8]) -> (String, String) {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) => {
            let shape = v
                .get("__type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("InternalServerException")
                .to_string();
            let message = v
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            (shape, message)
        }
        Err(_) => (
            "InternalServerException".to_string(),
            String::from_utf8_lossy(body).into_owned(),
        ),
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Endpoint string -> `(normalized endpoint, authority)`; the shared URI
/// validation of [`Connection::resolve`] and [`Connection::resolve_probe`]
/// (scheme allow-list, authority present, base-path normalization to
/// exactly one '/' between base and op paths).
fn parse_endpoint(raw: String) -> Result<(String, String), Failure> {
    let uri: ::http::Uri = raw.parse().map_err(|e| {
        Failure::new(
            "ENDPOINT_INVALID",
            format!("cannot parse endpoint {raw:?}: {e}"),
            "use the form http://127.0.0.1:8787 (or https://host for TLS)",
        )
    })?;
    let scheme = uri.scheme_str().unwrap_or("http");
    if scheme != "http" && scheme != "https" {
        return Err(Failure::new(
            "ENDPOINT_INVALID",
            format!("unsupported endpoint scheme {scheme:?}"),
            "use http:// or https://",
        ));
    }
    let authority = uri
        .authority()
        .map(|a| a.as_str().to_string())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            Failure::new(
                "ENDPOINT_INVALID",
                format!("endpoint {raw:?} carries no host"),
                "use the form http://127.0.0.1:8787",
            )
        })?;
    let base = uri.path().trim_end_matches('/');
    Ok((format!("{scheme}://{authority}{base}"), authority))
}

// ---------------------------------------------------------------------------
// tests (in-process: a real axum router + a REAL TCP round trip through the
// hyper client + signer — no network beyond 127.0.0.1)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire agreement between this client and the server gate: sign -> send
    /// over a real socket -> verify. Uses the plain AWS-face router (the
    /// /x-vf surface is covered end-to-end by tests/cli_smoke.rs with the
    /// real binaries); here the point is the SIGNATURE plumbing.
    #[tokio::test]
    async fn signed_roundtrip_over_real_socket() {
        use axum::Json;
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/x-vf/Ping",
            post(|body: bytes::Bytes| async move {
                let echoed: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                Json(serde_json::json!({ "echo": echoed }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let conn = Connection::resolve(
            Some(format!("http://{addr}")),
            Some("AKIDEXAMPLE".into()),
            Some("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
            Some("us-east-1".into()),
        )
        .unwrap();
        let v = conn
            .call_json("Ping", &serde_json::json!({"hello":"world"}))
            .await
            .expect("call ok");
        assert_eq!(v["echo"]["hello"], serde_json::json!("world"));
        server.abort();
    }

    #[test]
    fn header_value_encoding_round_trips() {
        assert_eq!(encode_header_value("docs/plain.txt"), "docs/plain.txt");
        let tricky = "docs/文档 file%100.txt";
        let enc = encode_header_value(tricky);
        assert!(enc.is_ascii(), "{enc}");
        assert!(enc.split('%').count() >= 3, "{enc}");
        let dec = percent_encoding::percent_decode_str(&enc)
            .decode_utf8()
            .unwrap()
            .into_owned();
        assert_eq!(dec, tricky);
    }
}
