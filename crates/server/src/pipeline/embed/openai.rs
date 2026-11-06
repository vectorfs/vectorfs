//! `openai` embedder kind: remote embeddings over a hand-rolled HTTP client
//! (design.md §5.7: "openai provider 走自研 HTTP（复用 hyper 树 ~150 行，零
//! reqwest）"; §5.10.5 robustness rules).
//!
//! # Transport
//!
//! hyper-util legacy client + hyper-rustls/rustls (aws-lc-rs provider) — the
//! same crates already in the tree for object_store; the connector lives in
//! [`super::http`]. HTTPS is REQUIRED for non-loopback endpoints (both real
//! providers — api.openai.com, api.bigmodel.cn — are https; a plaintext
//! bearer token crossing the internet is not acceptable). Plain http is
//! allowed for loopback hosts only, which is exactly what the in-process
//! test fake (and an on-box gateway) needs.
//!
//! # dim: DECLARED, never probed (§5.10.2)
//!
//! `dim` is a required config field. Probing would need a network round trip
//! at startup — wrong for an air-gapped check (`pipelines check` parses this
//! config offline and must stay network-free) and wrong at serve time (the
//! provider's dim is a stable, documented property; a probe would only add a
//! startup-order dependency on a remote's uptime). A response whose vectors
//! do not match the declared dim fails the batch — surfaced as an Err, never
//! silently truncated.
//!
//! # Robustness (§5.10.5 "openai provider")
//!
//! * **Retry**: 429 / 5xx / timeout / connection errors retry with
//!   exponential backoff (base × 2^attempt), at most 3 retries per attempt
//!   group. Other 4xx (auth, bad model name) fail FAST — retrying a
//!   permission error is noise.
//! * **Batch semantics**: any failure inside a batch retries the WHOLE batch
//!   — embeddings are a pure function of the input texts, so a re-POST is
//!   idempotent from the index's point of view (§5.10.5: "批内任一失败整批
//!   重试（幂等 key 保安全）"). Retries exhausted ⇒ Err.
//! * **api key resolution**: config `api_key` > `VF_OPENAI_API_KEY` >
//!   `OPENAI_API_KEY`; missing ⇒ construction Err with guidance (NOT a
//!   config parse error: `pipelines check` must pass without credentials).
//! * **Batch size** (`POOL_SIZE`, 64) is a mechanism constant, not config
//!   (§5.10.0 axiom 1: no user would usefully change it; providers cap
//!   request payloads anyway).
//!
//! # Wire format
//!
//! `POST {endpoint}/embeddings` with `{"model": …, "input": [texts…]}`
//! (OpenAI-compatible: api.openai.com, BigModel's v1, vLLM/TEI gateways) and
//! `Authorization: Bearer <key>`. Response `data[]` carries
//! `{index, embedding}` pairs — real providers may return them OUT OF ORDER,
//! so vectors are realigned by `index` and the index set is validated to be
//! exactly `0..n`.

use std::time::Duration;

use super::super::{Chunk, Embedder, Modality, PipelineError, Query};
use super::http::HttpClient;

/// Requests per POST batch. A mechanism constant (module docs).
pub const POOL_SIZE: usize = 64;

/// Attempt budget per batch: 1 try + 3 retries (§5.10.5).
const MAX_RETRIES: u32 = 3;

/// Backoff base for the first retry; doubles each attempt. Production
/// default 500ms (total worst-case wait before giving up ≈ 3.5s of backoff
/// plus request time). The field (not just the constant) exists so tests
/// inject ~1ms instead of sleeping seconds.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Per-attempt request timeout. Generous: embeddings of a 64-text batch can
/// take tens of seconds on loaded remotes; the RETRY layer, not a tight
/// timeout, is the resilience mechanism.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-kind config struct (§5.10.4: `{ kind, endpoint, model, dim, api_key? }`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiConfig {
    pub kind: OpenAiTag,
    /// Base URL, e.g. `https://api.bigmodel.cn/v1` (no trailing path needed).
    pub endpoint: String,
    pub model: String,
    /// DECLARED output dimensionality (never probed over the network).
    pub dim: usize,
    /// Bearer token; when absent the env chain applies at construction.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Literal tag: only deserializes from `"openai"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenAiTag {
    #[serde(rename = "openai")]
    OpenAi,
}

// ---------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------

/// A constructed openai embedder: config + resolved credentials + one shared
/// client (connection pooling per endpoint comes with the hyper-util client).
pub struct OpenAiEmbedder {
    client: HttpClient,
    endpoint: String,
    model: String,
    dim: usize,
    api_key: String,
    /// Test seam: production uses [`DEFAULT_BACKOFF_BASE`].
    backoff_base: Duration,
}

/// Manual Debug: the client is a connection pool; identity fields only (and
/// never the api_key — Debug output goes to test failures/logs).
impl std::fmt::Debug for OpenAiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedder")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dim", &self.dim)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl OpenAiEmbedder {
    /// Construction (the credential + endpoint gates; `pipelines check` never
    /// gets here — config parsing is credential-free by design).
    pub fn new(cfg: &OpenAiConfig) -> Result<Self, PipelineError> {
        if cfg.dim == 0 {
            return Err(PipelineError::embed(
                "openai embedder: dim must be > 0 (it is the DECLARED output dimensionality — \
                 there is deliberately no network probe)",
            ));
        }
        let uri: http::Uri = cfg.endpoint.parse().map_err(|e| {
            PipelineError::embed(format!(
                "openai embedder: cannot parse endpoint {:?}: {e}",
                cfg.endpoint
            ))
        })?;
        match uri.scheme_str() {
            Some("https") => {}
            Some("http") => {
                let host = uri.host().unwrap_or_default();
                let loopback = host == "localhost"
                    || host == "127.0.0.1"
                    || host == "[::1]"
                    || host.starts_with("127.");
                if !loopback {
                    return Err(PipelineError::embed(format!(
                        "openai embedder: endpoint {host:?} is plain http — https is required \
                         for non-loopback hosts (the bearer token must not cross the network in \
                         the clear; loopback http is kept for on-box gateways and tests)"
                    )));
                }
            }
            other => {
                return Err(PipelineError::embed(format!(
                    "openai embedder: endpoint scheme must be http(s), got {other:?}"
                )));
            }
        }
        let api_key = cfg
            .api_key
            .clone()
            .or_else(|| env_nonempty("VF_OPENAI_API_KEY"))
            .or_else(|| env_nonempty("OPENAI_API_KEY"))
            .ok_or_else(|| {
                PipelineError::embed(
                    "openai embedder: no api key (set [embedder.<name>].api_key, or export \
                     VF_OPENAI_API_KEY / OPENAI_API_KEY)",
                )
            })?;
        Ok(OpenAiEmbedder {
            client: super::http::client(),
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            dim: cfg.dim,
            api_key,
            backoff_base: DEFAULT_BACKOFF_BASE,
        })
    }

    /// Test-only backoff shrink (real backoff values are tested by
    /// construction; a full-timing test would flake on CI load).
    #[cfg(test)]
    pub(crate) fn with_backoff_base(mut self, base: Duration) -> Self {
        self.backoff_base = base;
        self
    }

    /// POST one batch of texts, returning one vector per text (realigned by
    /// the response's `index`). Retry policy in the module docs.
    async fn post_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, PipelineError> {
        let url = format!("{}/embeddings", self.endpoint);
        let body = serde_json::to_vec(&Body {
            model: &self.model,
            input: texts,
        })
        .map_err(|e| PipelineError::embed(format!("serializing request: {e}")))?;

        let mut attempt: u32 = 0;
        loop {
            use http_body_util::BodyExt;
            let req = http::Request::post(&url)
                .header(
                    http::header::AUTHORIZATION,
                    format!("Bearer {}", self.api_key),
                )
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(http_body_util::Full::new(bytes::Bytes::from(body.clone())))
                .expect("static parts build a valid request");

            let outcome =
                match tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(req)).await {
                    Err(_) => Err("request timed out".to_string()), // retryable
                    Ok(Err(e)) => Err(format!("transport error: {e}")), // retryable
                    Ok(Ok(resp)) => Ok(resp),
                };
            match outcome {
                Ok(resp) => {
                    let status = resp.status();
                    let bytes = resp
                        .into_body()
                        .collect()
                        .await
                        .map_err(|e| PipelineError::embed(format!("reading response body: {e}")))?
                        .to_bytes();
                    if status.is_success() {
                        let resp: Resp = serde_json::from_slice(&bytes).map_err(|e| {
                            PipelineError::embed(format!(
                                "provider answered HTTP {status} with an unparsable body: {e}"
                            ))
                        })?;
                        return realign(resp.data, texts.len(), self.dim);
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if !retryable {
                        return Err(PipelineError::embed(format!(
                            "provider rejected the batch: HTTP {status} — {}",
                            String::from_utf8_lossy(&bytes)
                        )));
                    }
                    if attempt >= MAX_RETRIES {
                        return Err(PipelineError::embed(format!(
                            "provider still answers HTTP {status} after {MAX_RETRIES} retries"
                        )));
                    }
                }
                Err(why) => {
                    if attempt >= MAX_RETRIES {
                        return Err(PipelineError::embed(format!(
                            "batch failed after {MAX_RETRIES} retries (last: {why})"
                        )));
                    }
                }
            }
            // exponential backoff: base * 2^attempt (500ms, 1s, 2s in prod)
            tokio::time::sleep(self.backoff_base * (1u32 << attempt)).await;
            attempt += 1;
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

// ---- wire types (shared by post_batch and the realign validation) ----

#[derive(serde::Serialize)]
struct Body<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(serde::Deserialize)]
struct Resp {
    data: Vec<Item>,
}

#[derive(serde::Deserialize)]
struct Item {
    index: usize,
    embedding: Vec<f32>,
}

impl Realigned for Item {
    fn index(&self) -> usize {
        self.index
    }
    fn embedding(&self) -> Vec<f32> {
        self.embedding.clone()
    }
}

/// Realign `data[]` by `index` and enforce the response contract: exactly
/// `n` items, index set exactly `0..n`, every vector of the DECLARED dim and
/// finite. Any violation fails the batch (module docs: never truncate).
fn realign(
    items: Vec<impl Realigned>,
    n: usize,
    dim: usize,
) -> Result<Vec<Vec<f32>>, PipelineError> {
    let mut slots: Vec<Option<Vec<f32>>> = (0..n).map(|_| None).collect();
    for item in items {
        let idx = item.index();
        let v = item.embedding();
        if idx >= n {
            return Err(PipelineError::embed(format!(
                "provider returned index {idx} for a {n}-item batch"
            )));
        }
        if slots[idx].is_some() {
            return Err(PipelineError::embed(format!(
                "provider returned index {idx} twice"
            )));
        }
        if v.len() != dim {
            return Err(PipelineError::embed(format!(
                "provider returned dim {} for item {idx}, config declares {dim} — the index \
                 schema and the provider disagree; fix the config dim",
                v.len()
            )));
        }
        if v.iter().any(|x| !x.is_finite()) {
            return Err(PipelineError::embed(format!(
                "provider returned a non-finite value for item {idx}"
            )));
        }
        slots[idx] = Some(v);
    }
    slots
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            s.ok_or_else(|| {
                PipelineError::embed(format!(
                    "provider omitted index {i} of a {n}-item batch (partial batches are \
                     rejected, not patched)"
                ))
            })
        })
        .collect()
}

/// Internal accessor trait so `realign` reads the same wire struct in prod
/// and tests without duplicating the validation logic.
trait Realigned {
    fn index(&self) -> usize;
    fn embedding(&self) -> Vec<f32>;
}

#[async_trait::async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed_query(&self, q: &Query) -> Result<Vec<f32>, PipelineError> {
        let Query::Text(t) = q else {
            // text-only kind (query_modalities = [text]); M3-T1 seam
            return Err(super::query_modality_refusal(q));
        };
        self.post_batch(std::slice::from_ref(t))
            .await
            .map(|mut v| v.swap_remove(0))
    }

    async fn embed_documents(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(c) = chunks.iter().find(|c| c.image.is_some()) {
            return Err(super::image_chunk_refusal(c));
        }
        let mut out = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(POOL_SIZE) {
            let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
            out.extend(self.post_batch(&texts).await?);
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        &[Modality::Text]
    }

    fn query_modalities(&self) -> &'static [Modality] {
        &[Modality::Text]
    }
}

// ---------------------------------------------------------------------------
// tests — hermetic: in-process axum fake answering /embeddings
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode as AxStatus;
    use serde_json::json;

    /// What the fake captured for one request (asserted by the tests).
    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    pub(crate) struct SeenRequest {
        pub auth: Option<String>,
        pub model: String,
        pub input: Vec<String>,
    }

    /// Fake server state shared with the handlers.
    #[derive(Clone)]
    pub(crate) struct Fake {
        pub seen: std::sync::Arc<std::sync::Mutex<Vec<SeenRequest>>>,
        /// scripts: each incoming request consumes the NEXT status; an empty
        /// list answers 200 forever.
        pub statuses: std::sync::Arc<std::sync::Mutex<Vec<u16>>>,
    }

    /// Spawn the fake provider; returns (base_url, state).
    pub(crate) async fn fake_openai() -> (String, Fake) {
        let fake = Fake {
            seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let state = fake.clone();
        let app = axum::Router::new()
            .route(
                "/v1/embeddings",
                axum::routing::post(
                    move |State(st): State<Fake>, req: axum::extract::Request| async move {
                        use http_body_util::BodyExt;
                        let auth = req
                            .headers()
                            .get(http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        let body = req.into_body().collect().await.expect("body").to_bytes();
                        let v: serde_json::Value =
                            serde_json::from_slice(&body).expect("request must be JSON");
                        st.seen.lock().unwrap().push(SeenRequest {
                            auth,
                            model: v["model"].as_str().unwrap_or_default().to_string(),
                            input: v["input"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .map(|x| x.as_str().unwrap_or_default().to_string())
                                        .collect()
                                })
                                .unwrap_or_default(),
                        });
                        let status = st.statuses.lock().unwrap().pop().unwrap_or(200);
                        if status != 200 {
                            return (
                                AxStatus::from_u16(status).unwrap(),
                                axum::Json(json!({"error": {"message": "scripted failure"}})),
                            );
                        }
                        // canned vectors: embedding of input i = [i as f32;
                        // dim], DELIBERATELY shuffled by index — the embedder
                        // must realign by index, not by array order.
                        let n = st.seen.lock().unwrap().last().unwrap().input.len();
                        let dim = 4usize;
                        let mut data: Vec<serde_json::Value> = (0..n)
                            .map(|i| {
                                json!({
                                    "index": i,
                                    "embedding": vec![i as f32; dim],
                                })
                            })
                            .collect();
                        // rotate: index 0 last
                        if n > 1 {
                            data.rotate_right(1);
                        }
                        (
                            AxStatus::OK,
                            axum::Json(json!({ "data": data, "model": "fake" })),
                        )
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/v1"), fake)
    }

    pub(crate) fn fake_config(endpoint: String) -> OpenAiConfig {
        OpenAiConfig {
            kind: OpenAiTag::OpenAi,
            endpoint,
            model: "embedding-fake".to_string(),
            dim: 4,
            api_key: Some("test-key".to_string()),
        }
    }

    impl Realigned for (usize, Vec<f32>) {
        fn index(&self) -> usize {
            self.0
        }
        fn embedding(&self) -> Vec<f32> {
            self.1.clone()
        }
    }

    #[test]
    fn realign_validates_and_sorts() {
        // shuffled input realigns by index
        let items = vec![(2usize, vec![0.2; 4]), (0, vec![0.0; 4]), (1, vec![0.1; 4])];
        let out = realign(items, 3, 4).unwrap();
        assert_eq!(
            out,
            vec![vec![0.0; 4], vec![0.1; 4], vec![0.2; 4]],
            "index realignment"
        );
        // missing index -> Err
        let err = realign(vec![(0, vec![0.0; 4])], 2, 4).unwrap_err();
        assert!(err.to_string().contains("omitted index 1"), "{err}");
        // duplicate -> Err
        let err = realign(vec![(0, vec![0.0; 4]), (0, vec![0.0; 4])], 2, 4).unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
        // wrong dim -> Err (declared dim contract)
        let err = realign(vec![(0, vec![0.0; 7])], 1, 4).unwrap_err();
        assert!(err.to_string().contains("config declares 4"), "{err}");
        // non-finite -> Err
        let err = realign(vec![(0, vec![f32::NAN; 4])], 1, 4).unwrap_err();
        assert!(err.to_string().contains("non-finite"), "{err}");
    }

    #[tokio::test]
    async fn request_shape_and_index_alignment() {
        let (base, fake) = fake_openai().await;
        let e = OpenAiEmbedder::new(&fake_config(base)).unwrap();
        let docs: Vec<Chunk> = ["alpha", "beta", "gamma"]
            .into_iter()
            .enumerate()
            .map(|(i, t)| Chunk {
                index: i as u32,
                text: t.to_string(),
                image: None,
            })
            .collect();
        let out = e.embed_documents(&docs).await.unwrap();
        // shuffled response realigned by index
        assert_eq!(
            out,
            vec![vec![0.0; 4], vec![1.0; 4], vec![2.0; 4]],
            "canned vectors must realign by index"
        );
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].auth.as_deref(), Some("Bearer test-key"));
            assert_eq!(seen[0].model, "embedding-fake");
            assert_eq!(seen[0].input, vec!["alpha", "beta", "gamma"]);
        }
        // query route: same wire shape
        let v = e.embed_query(&Query::Text("qq".into())).await.unwrap();
        assert_eq!(v, vec![0.0; 4]);
        assert_eq!(fake.seen.lock().unwrap().len(), 2);
        // empty batch: no request at all
        assert!(e.embed_documents(&[]).await.unwrap().is_empty());
        assert_eq!(fake.seen.lock().unwrap().len(), 2);
    }

    /// M3-T1: both text-only kinds refuse non-text inputs LOUDLY — an image
    /// query errors (never an empty/garbage vector), and an image chunk on
    /// the doc side errors (never the empty text channel embedded as if it
    /// were content). The openai kind is the hermetic proof; ort shares the
    /// exact refusal helpers.
    #[tokio::test]
    async fn image_inputs_are_refused_loudly_not_silently_embedded() {
        let (base, fake) = fake_openai().await;
        let e = OpenAiEmbedder::new(&fake_config(base)).unwrap();
        // query side: text-only query_modalities => Err, message says the
        // embedder does not accept image queries and points at the fix
        let err = e
            .embed_query(&Query::Image(
                vec![0x89, 0x50, 0x4E, 0x47],
                "image/png".to_string(),
            ))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("does not accept an image query"), "{msg}");
        assert!(msg.contains("text-only"), "{msg}");
        assert!(msg.contains("image-capable embedder"), "{msg}");
        // doc side: an image chunk is a routing bug, refused before any
        // request leaves (nothing hits the wire)
        let err = e
            .embed_documents(&[Chunk {
                index: 0,
                text: String::new(),
                image: Some(crate::pipeline::ImagePayload {
                    bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
                    mime: "image/png".to_string(),
                }),
            }])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("chunk 0 is an image chunk"), "{msg}");
        assert!(msg.contains("text documents only"), "{msg}");
        assert!(fake.seen.lock().unwrap().is_empty(), "no HTTP call");
    }

    #[tokio::test]
    async fn batches_are_pooled_at_64() {
        let (base, fake) = fake_openai().await;
        let e = OpenAiEmbedder::new(&fake_config(base)).unwrap();
        let chunks: Vec<Chunk> = (0..65)
            .map(|i| Chunk {
                index: i,
                text: format!("t{i}"),
                image: None,
            })
            .collect();
        let out = e.embed_documents(&chunks).await.unwrap();
        assert_eq!(out.len(), 65);
        // 65 inputs / POOL_SIZE 64 = 2 batches, second has exactly 1 text
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "pool_size=64 must split 65 into 64+1");
        assert_eq!(seen[0].input.len(), 64);
        assert_eq!(seen[1].input.len(), 1);
        assert_eq!(seen[1].input[0], "t64");
    }

    #[tokio::test]
    async fn retries_on_429_and_5xx_then_succeeds() {
        let (base, fake) = fake_openai().await;
        // pop() consumes from the END, so the first two attempts answer 429
        // then 500 (order irrelevant — both retryable), then 200 forever
        *fake.statuses.lock().unwrap() = vec![500, 429];
        let e = OpenAiEmbedder::new(&fake_config(base))
            .unwrap()
            .with_backoff_base(Duration::from_millis(1));
        let out = e
            .embed_documents(&[Chunk {
                index: 0,
                text: "x".into(),
                image: None,
            }])
            .await
            .unwrap();
        assert_eq!(out, vec![vec![0.0; 4]]);
        assert_eq!(
            fake.seen.lock().unwrap().len(),
            3,
            "2 failures + 1 success = 3 requests"
        );
    }

    #[tokio::test]
    async fn retries_exhausted_is_an_error() {
        let (base, fake) = fake_openai().await;
        *fake.statuses.lock().unwrap() = vec![500, 500, 500, 500]; // pops 500 first
        let e = OpenAiEmbedder::new(&fake_config(base))
            .unwrap()
            .with_backoff_base(Duration::from_millis(1));
        let err = e
            .embed_documents(&[Chunk {
                index: 0,
                text: "x".into(),
                image: None,
            }])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("after 3 retries"), "{msg}");
        assert_eq!(fake.seen.lock().unwrap().len(), 4, "1 try + 3 retries");
    }

    #[tokio::test]
    async fn non_retryable_4xx_fails_fast() {
        let (base, fake) = fake_openai().await;
        *fake.statuses.lock().unwrap() = vec![401];
        let e = OpenAiEmbedder::new(&fake_config(base))
            .unwrap()
            .with_backoff_base(Duration::from_millis(1));
        let err = e
            .embed_documents(&[Chunk {
                index: 0,
                text: "x".into(),
                image: None,
            }])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 401"), "{}", err);
        assert_eq!(fake.seen.lock().unwrap().len(), 1, "no retries on 4xx");
    }

    #[test]
    fn https_required_off_loopback() {
        let err =
            OpenAiEmbedder::new(&fake_config("http://api.example.com/v1".into())).unwrap_err();
        assert!(err.to_string().contains("https is required"), "{err}");
        // https anywhere: fine at construction (no request is made)
        assert!(OpenAiEmbedder::new(&fake_config("https://api.bigmodel.cn/v1".into())).is_ok());
        // loopback http: allowed (tests + on-box gateways)
        assert!(OpenAiEmbedder::new(&fake_config("http://127.0.0.1:9/v1".into())).is_ok());
        // dim must be positive
        let mut cfg = fake_config("https://x.example/v1".into());
        cfg.dim = 0;
        let err = OpenAiEmbedder::new(&cfg).unwrap_err();
        assert!(err.to_string().contains("dim must be > 0"), "{err}");
    }

    /// Env-var tests share one process: serialize credential tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn api_key_resolution_chain_and_missing_error() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg_no_key = OpenAiConfig {
            api_key: None,
            ..fake_config("https://api.example.com/v1".into())
        };
        // config wins over env
        unsafe { std::env::set_var("VF_OPENAI_API_KEY", "from-vf") };
        unsafe { std::env::set_var("OPENAI_API_KEY", "from-openai") };
        let e = OpenAiEmbedder::new(&cfg_no_key).unwrap();
        assert_eq!(e.api_key, "from-vf");
        // VF_ > OPENAI_
        unsafe { std::env::remove_var("VF_OPENAI_API_KEY") };
        let e = OpenAiEmbedder::new(&cfg_no_key).unwrap();
        assert_eq!(e.api_key, "from-openai");
        // missing -> actionable Err
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let err = OpenAiEmbedder::new(&cfg_no_key).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no api key"), "{msg}");
        assert!(msg.contains("VF_OPENAI_API_KEY"), "{msg}");
        // restore a key so other tests in this process are unaffected
        unsafe { std::env::set_var("OPENAI_API_KEY", "from-openai") };
    }

    #[test]
    fn config_roundtrip_and_guards() {
        let cfg: OpenAiConfig = toml::from_str(
            "kind = \"openai\"\nendpoint = \"https://api.bigmodel.cn/v1\"\nmodel = \
             \"embedding-3\"\ndim = 2048\n",
        )
        .unwrap();
        assert_eq!(cfg.dim, 2048);
        assert!(cfg.api_key.is_none());
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"openai\"\nendpoint = \"https://api.bigmodel.cn/v1\"\nmodel = \
             \"embedding-3\"\ndim = 2048"
        );
        // dim is REQUIRED: no default, no probe
        let err = toml::from_str::<OpenAiConfig>(
            "kind = \"openai\"\nendpoint = \"https://x/v1\"\nmodel = \"m\"",
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing field `dim`"), "{err}");
        // unknown field rejected
        assert!(
            toml::from_str::<OpenAiConfig>(
                "kind = \"openai\"\nendpoint = \"https://x/v1\"\nmodel = \"m\"\ndim = \
                 4\npool_size = 8"
            )
            .is_err()
        );
        // wrong kind literal rejected
        assert!(toml::from_str::<OpenAiConfig>("kind = \"ort\"").is_err());
    }
}
