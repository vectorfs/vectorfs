//! Shared HTTP leaf for the two networked M2-T5 components: the `openai`
//! embedder kind and `pipeline::models` prefetch (§5.7: "openai provider 走
//! 自研 HTTP（复用 hyper 树 ~150 行，零 reqwest）").
//!
//! DIP note: this module exists so the HTTP/TLS TYPES stay confined to one
//! leaf — `openai.rs` and `pipeline::models` see only [`HttpClient`] and plain
//! `http`/`bytes` types, never hyper-rustls/rustls internals. reqwest (already
//! in the tree via lance-namespace) is deliberately NOT used: it would tie the
//! provider's client policy to the engine's dependency, exactly what §5.7's
//! "自研" ruling forbids.
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
use hyper_util::rt::TokioExecutor;

/// The shared client shape: https-or-http, http1, no pooling knobs (process
/// default pool; embedding traffic is low-volume and keep-alive reuse comes
/// free with the legacy client).
pub type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Build a client. One client per embedder instance keeps the connection pool
/// scoped to its endpoint; constructing is cheap.
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
        .build();
    Client::builder(TokioExecutor::new()).build(https)
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
