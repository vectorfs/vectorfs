//! Shared HTTP leaf for the three networked consumers of the three-crate
//! split: the `openai` embedder kind, `pipeline::models` prefetch (§5.7:
//! "openai provider 走自研 HTTP（复用 hyper 树 ~150 行，零 reqwest）"), and —
//! since the split — the `vf` CLIENT's transport to a running daemon (the
//! client talks to the same wire the server serves, so it shares the leaf
//! instead of growing a second one).
//!
//! DIP note: this module exists so the HTTP/TLS TYPES stay confined to one
//! leaf — `openai.rs`, `pipeline::models` and the client see only
//! [`HttpClient`] and plain `http`/`bytes` types, never hyper-rustls/rustls
//! internals. reqwest (already in the SERVER's tree via lance-namespace) is
//! deliberately NOT used: it would tie the provider's client policy to the
//! engine's dependency, exactly what §5.7's "自研" ruling forbids — and it
//! would drag the engine's closure into the client crate.
//!
//! TLS: rustls 0.23 with the **aws-lc-rs** provider — the same crypto
//! object_store already binds (§8 #11; `rustls/aws_lc_rs` is enabled tree-wide
//! via hyper-rustls, and rustls's default-provider selection prefers aws-lc-rs
//! when both providers are compiled in). Root store = `webpki-roots` (MPL-2.0,
//! Cargo.toml rationale): a bundled Mozilla root set keeps slim containers
//! without ca-certificates working. The connector is `https_or_http` on
//! purpose: plain http remains reachable for loopback test fakes and in-process
//! gateways; the openai kind separately enforces https for non-loopback hosts
//! (openai.rs).

use bytes::Bytes;
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::proxy::Tunnel;
use hyper_util::rt::TokioExecutor;

/// Percent-encode a `x-vf-*` header VALUE: HTTP header values are ASCII-only,
/// so controls, space, `%` (round-trip safety) and every non-ASCII byte are
/// escaped; the server percent-decodes with the exact inverse
/// (ops_xvf::put_source). Always-encode/always-decode on both ends keeps the
/// mapping bijective for any key. Lives in common because BOTH sides of the
/// wire pair use it — the `vf` client (PutSource file names) and the server's
/// MCP tool (x-vf-file-name) encode; one function, one inverse, no drift.
pub fn encode_header_value(v: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ').add(b'%');
    percent_encoding::utf8_percent_encode(v, SET).to_string()
}

/// The shared client shape: https-or-http, http1, no pooling knobs (process
/// default pool; embedding traffic is low-volume and keep-alive reuse comes
/// free with the legacy client). The connector is [`ProxyAwareConnector`] —
/// direct TCP unless the standard proxy env vars apply.
pub type HttpClient = Client<hyper_rustls::HttpsConnector<ProxyAwareConnector>, Full<Bytes>>;

/// Build a client. One client per embedder instance keeps the connection pool
/// scoped to its endpoint; constructing is cheap.
///
/// Proxy: standard `HTTPS_PROXY`/`https_proxy` (https targets) and
/// `HTTP_PROXY`/`http_proxy` (http targets) env vars are honored via a CONNECT
/// [`Tunnel`], with `NO_PROXY`/`no_proxy` host exclusions — the same semantics
/// curl uses, resolved per destination inside [`ProxyAwareConnector`]. No env
/// vars set (the common deployment) = direct connection, byte-for-byte the
/// pre-proxy behavior. Proxy AUTH (`user:pass@host`) is not supported in M2;
/// such an env value is a hard construction error, never a silent proxy skip.
pub fn client() -> HttpClient {
    // EXPLICIT provider, never the feature-default selection: this tree's
    // rustls carries BOTH `aws-lc-rs` (hyper-rustls/object_store) and `ring`
    // (reqwest feature unification), and rustls PANICS on the ambiguity.
    // aws-lc-rs is the deliberate choice (§8 #11, same crypto as S3).
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_webpki_roots(provider)
        .expect("building the aws-lc-rs rustls ClientConfig cannot fail")
        .https_or_http()
        .enable_http1()
        .wrap_connector(ProxyAwareConnector::from_env());
    Client::builder(TokioExecutor::new()).build(https)
}

// ---------------------------------------------------------------------------
// proxy-aware transport connector
// ---------------------------------------------------------------------------

/// Transport connector (the layer UNDER TLS): plain TCP to the target, or a
/// CONNECT tunnel through an HTTP proxy when the env says so for this target.
///
/// hyper-rustls wraps this connector and does TLS itself — so for an https
/// destination through a proxy, TLS runs END-TO-END over the tunnel (the
/// proxy never sees plaintext). `http://` destinations (loopback test fakes
/// and in-process gateways only — the openai kind separately enforces https
/// off-loopback) tunnel without TLS, which is what CONNECT means for them.
#[derive(Clone, Debug)]
pub struct ProxyAwareConnector {
    http: HttpConnector,
    /// (proxy authority, no_proxy entries). `None` = env unset: zero proxy
    /// logic, the connector is a plain HttpConnector with a boxed future.
    proxy: Option<(http::uri::Authority, Vec<String>)>,
}

type BoxIoFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<
                    hyper_util::rt::TokioIo<tokio::net::TcpStream>,
                    Box<dyn std::error::Error + Send + Sync>,
                >,
            > + Send,
    >,
>;

impl tower_service::Service<http::Uri> for ProxyAwareConnector {
    type Response = hyper_util::rt::TokioIo<tokio::net::TcpStream>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    // The boxed future is what makes this connector legal for hyper-util's
    // legacy client (its Connect blanket impl requires Unpin futures; both
    // HttpConnector's and Tunnel's own futures are not).
    type Future = BoxIoFuture;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.http
            .poll_ready(cx)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    fn call(&mut self, dst: http::Uri) -> Self::Future {
        // The boxed future is 'static: every branch dials through a CLONED
        // connector (HttpConnector is cheap-clone; it only carries DNS
        // resolver + knobs) instead of borrowing `self`.
        let mut direct = self.http.clone();
        let direct = move |_dst: http::Uri| {
            Box::pin(async move {
                direct
                    .call(_dst)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
            }) as BoxIoFuture
        };
        match &self.proxy {
            // direct: same dial HttpConnector would do, error type erased
            None => direct(dst),
            // tunneled: TCP to the proxy, then CONNECT with the target
            // authority; NO_PROXY-excluded hosts fall back to direct
            Some((proxy_authority, exclusions)) => {
                let host = dst.host().unwrap_or_default().to_string();
                if no_proxy_matches(&host, exclusions) {
                    return direct(dst);
                }
                let proxy_uri = http::Uri::builder()
                    .scheme("http")
                    .authority(proxy_authority.clone())
                    .path_and_query("/")
                    .build()
                    .expect("proxy authority round-trips into a Uri");
                let mut tunnel = Tunnel::new(proxy_uri, self.http.clone());
                Box::pin(async move {
                    tunnel
                        .call(dst)
                        .await
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                })
            }
        }
    }
}

impl ProxyAwareConnector {
    /// Test seam: build with an explicit proxy + exclusions (from_env reads
    /// the process environment, which is global and racy under the parallel
    /// test runner).
    #[cfg(test)]
    pub(crate) fn proxied(authority: http::uri::Authority, exclusions: Vec<String>) -> Self {
        Self {
            http: direct_connector(),
            proxy: Some((authority, exclusions)),
        }
    }

    /// Pick up `HTTPS_PROXY`/`https_proxy` (for https targets) or
    /// `HTTP_PROXY`/`http_proxy` (for plain-http targets) plus
    /// `NO_PROXY`/`no_proxy` exclusions. One proxy for both schemes: the
    /// http-target proxy only matters for non-loopback plain http, which this
    /// leaf never makes (see module doc), so honoring it would be dead code —
    /// the https entry wins and the http entry is only read when https is
    /// absent.
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let proxy = env("HTTPS_PROXY")
            .or_else(|| env("https_proxy"))
            .or_else(|| env("HTTP_PROXY"))
            .or_else(|| env("http_proxy"))
            .map(|raw| {
                let cleaned = raw.trim_start_matches("http://").trim_end_matches('/');
                let authority = cleaned.parse::<http::uri::Authority>().unwrap_or_else(|_| {
                    panic!(
                        "invalid proxy {raw:?}: expected host:port (http scheme; proxy \
                             auth is not supported in M2)"
                    )
                });
                if authority.as_str().contains('@') {
                    panic!(
                        "proxy {raw:?} carries credentials (user:pass@) — proxy auth is not \
                         supported in M2; use an unauthenticated proxy or a mirror (HF_ENDPOINT)"
                    );
                }
                let exclusions = env("NO_PROXY")
                    .or_else(|| env("no_proxy"))
                    .map(|list| list.split(',').map(|s| s.trim().to_lowercase()).collect())
                    .unwrap_or_default();
                (authority, exclusions)
            });
        Self {
            http: direct_connector(),
            proxy,
        }
    }
}

/// The inner transport dial. Scheme enforcement is OFF: hyper-rustls forwards
/// the ORIGINAL destination URI — https scheme included — to the connector it
/// wraps (its own built-in HttpConnector gets `enforce_http(false)` for
/// exactly this reason: "HttpConnector won't enforce scheme, but
/// HttpsConnector will", connector/builder.rs). With the default
/// `enforce_http(true)` every DIRECT https request died at connect with
/// "invalid URL, scheme is not http" — model prefetch / openai egress failed
/// at connect while curl reached the same endpoint. Scheme gating is already
/// the HttpsConnector layer's job (unsupported schemes are rejected there);
/// this layer dials host:port and defers the default port (443/80) to
/// HttpConnector's own scheme handling.
fn direct_connector() -> HttpConnector {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http
}

/// curl's NO_PROXY semantics, subset: exact host match, dot-suffix match
/// (`.corp.net` or `corp.net` both cover `a.b.corp.net`), and the `*`
/// wildcard. Leading-dot entries match host suffixes without the dot.
fn no_proxy_matches(host: &str, exclusions: &[String]) -> bool {
    let host = host.to_lowercase();
    exclusions.iter().any(|e| {
        e == "*" || host == *e || {
            let bare = e.strip_prefix('.').unwrap_or(e);
            host.ends_with(bare) && host.as_bytes().get(host.len() - bare.len()) == Some(&b'.')
        }
    })
}

/// Erased response body: models.rs streams file bytes from it without naming
/// hyper-util internals.
pub type BoxedBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Follow up to `max_hops` redirects for `url`, returning the final
/// `(status, content_length, response)` with the body erased to [`BoxedBody`].
/// Hugging Face `resolve/` URLs answer 302 to a CDN, so prefetch must follow;
/// the openai embeddings endpoint does not redirect and never exercises this.
/// Non-redirect statuses pass through untouched — status interpretation is the
/// caller's policy.
pub async fn get_following_redirects(
    client: &HttpClient,
    url: &str,
    max_hops: usize,
) -> Result<(http::StatusCode, Option<u64>, http::Response<BoxedBody>), String> {
    use http_body_util::BodyExt;
    let mut current = url.to_string();
    for hop in 0..=max_hops {
        let uri: http::Uri = current
            .parse()
            .map_err(|e| format!("cannot parse URL {current:?}: {e}"))?;
        let resp = client
            .get(uri)
            .await
            .map_err(|e| format!("GET {current} failed: {e}"))?;
        let status = resp.status();
        if hop < max_hops
            && matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            // relative Location resolves against the current URL
            current = resolve_relative(&current, loc)?;
            continue;
        }
        let len = resp
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        return Ok((
            status,
            len,
            // erase the hyper error type: Box<dyn Error + Send + Sync>
            resp.map(|b| {
                b.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    .boxed()
            }),
        ));
    }
    Err(format!("too many redirects (> {max_hops}) fetching {url}"))
}

/// Resolve a (possibly relative) `Location` against `base` — enough URL
/// algebra for CDN redirects, without a url crate dependency beyond what is
/// already a direct dep (url is in-tree; hand-rolling this would be its third
/// copy in spirit — use it).
fn resolve_relative(base: &str, location: &str) -> Result<String, String> {
    let base_url = url::Url::parse(base).map_err(|e| format!("bad redirect base {base:?}: {e}"))?;
    let joined = base_url
        .join(location)
        .map_err(|e| format!("bad redirect target {location:?}: {e}"))?;
    Ok(joined.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_semantics_subset() {
        let ex = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let list = ex(&["localhost", ".corp.net", "10.0.0.0/8", "*"]);
        // exact
        assert!(no_proxy_matches("LOCALHOST", &list));
        // dot-suffix (leading dot optional)
        assert!(no_proxy_matches("a.b.corp.net", &list));
        assert!(no_proxy_matches("corp.net", &list));
        // plain suffix WITHOUT a dot boundary must NOT match (xcorp.net != corp.net)
        assert!(!no_proxy_matches("xcorp.net", &ex(&["corp.net"])));
        // non-matching entries are just strings (cidr blocks included — we do
        // not parse them, documented subset)
        assert!(!no_proxy_matches("example.com", &ex(&["10.0.0.0/8"])));
    }

    /// End-to-end through a fake HTTP proxy: the client must CONNECT through
    /// it for a non-excluded host and the request must ride the tunnel. The
    /// proxy double-roles as the origin (it answers the tunneled GET itself),
    /// which is exactly what CONNECT means for a plain-http target.
    #[tokio::test]
    async fn connector_tunnels_through_http_proxy_and_honors_no_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // fake proxy: read the CONNECT line, 200, then read one HTTP request
        // line and answer it on the same connection
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            let connect_line = head.lines().next().unwrap_or_default().to_string();
            sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            // the tunneled GET arrives on the same socket
            let mut buf2 = [0u8; 4096];
            let n2 = sock.read(&mut buf2).await.unwrap();
            let get_line = String::from_utf8_lossy(&buf2[..n2]).into_owned();
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await
                .unwrap();
            (connect_line, get_line)
        });

        let authority: http::uri::Authority =
            format!("127.0.0.1:{}", proxy_addr.port()).parse().unwrap();
        // "example.test" is NOT excluded -> tunneled
        let connector = ProxyAwareConnector::proxied(authority, vec!["localhost".into()]);
        let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider)
            .unwrap()
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
        // the "target" host need not resolve: the proxy answers everything.
        // Explicit port: hyper-util's Tunnel defaults an UNSPECIFIED CONNECT
        // port to 443 (it assumes https through the proxy); real plain-http
        // targets in this leaf are loopback-only and never tunneled, so the
        // explicit port keeps the assertion deterministic.
        let resp = client
            .get(
                "http://files.example.test:80/data.bin"
                    .parse::<http::Uri>()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), http::StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp)
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(&body[..], b"ok");
        let (connect, get) = proxy_task.await.unwrap();
        assert!(
            connect.starts_with("CONNECT files.example.test:80 "),
            "proxy saw {connect:?}"
        );
        assert!(
            get.starts_with("GET /data.bin "),
            "tunneled request {get:?}"
        );
    }

    /// REGRESSION (the 2026-11-09 deployment blocker): a DIRECT https
    /// destination must dial. hyper-rustls hands this connector the ORIGINAL
    /// https-scheme URI, so with the old default `enforce_http(true)` every
    /// direct https request died at connect ("invalid URL, scheme is not
    /// http") while curl reached the same endpoint. Only the transport dial
    /// is under test here — the listener is plain TCP and no TLS wrap
    /// happens; a completed handshake IS the assertion.
    #[tokio::test]
    async fn connector_dials_https_scheme_uri_direct() {
        use tower_service::Service;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        // a completed TCP accept is the whole assertion; drop the stream after
        let accept = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
        });

        let mut connector = ProxyAwareConnector::from_env();
        std::future::poll_fn(|cx| connector.poll_ready(cx))
            .await
            .unwrap();
        let uri: http::Uri = format!("https://127.0.0.1:{}/x.bin", port).parse().unwrap();
        let stream = connector.call(uri).await.expect("direct https dial");
        drop(stream);
        accept.await.unwrap();
    }

    #[test]
    fn relative_locations_resolve() {
        // absolute
        assert_eq!(
            resolve_relative("https://huggingface.co/a", "https://cdn.x/b").unwrap(),
            "https://cdn.x/b"
        );
        // path-relative
        assert_eq!(
            resolve_relative("https://huggingface.co/repo/resolve/f", "../g").unwrap(),
            "https://huggingface.co/repo/g"
        );
        // scheme-relative
        assert_eq!(
            resolve_relative("https://huggingface.co/a", "//cdn.x/b").unwrap(),
            "https://cdn.x/b"
        );
    }
}
