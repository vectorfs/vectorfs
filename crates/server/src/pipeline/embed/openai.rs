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
//!
//! # Multimodal messages dialect (b1, 2026-11-07 design.md §0 裁决)
//!
//! `multimodal = true` turns this kind into a UNIFIED embedder (doc side
//! [Text, Image], query side [Text, Image]) over the vLLM/IONOS messages
//! extension of the same endpoint (research/2026-11-m3-unified-embedder-
//! eval.md §3.2, wire verified verbatim against both):
//!
//! ```json
//! { "model": "…",
//!   "messages": [
//!     { "role": "system", "content": [ { "type": "text", "text": "…" } ] },
//!     { "role": "user", "content": [
//!       { "type": "image_url", "image_url": { "url": "data:image/png;base64,…" } },
//!       { "type": "text", "text": "…" } ] } ],
//!   "dimensions": 1024 }
//! ```
//!
//! * **one message per request**: vLLM treats the whole `messages` list as a
//!   SINGLE prompt ("you can replace `inputs` with a list of `messages` …
//!   which will be treated as a single prompt", vLLM embedding docs) — a
//!   multi-message request would be a multi-TURN conversation yielding ONE
//!   embedding of the concatenation, so batching N inputs into one request is
//!   not expressible in this dialect. Each chunk/query therefore rides its
//!   own POST, through the SAME retry/backoff loop (embeddings are a pure
//!   function of the message, so a re-POST stays idempotent for the index —
//!   the §5.10.5 invariant, adapted to endpoint-defined request granularity).
//! * **everything goes messages when the gate is on** — pure text entries
//!   included: the instruction rides a system message (only expressible in
//!   `messages`), and vLLM applies the chat template on the messages path but
//!   NOT on the `input` path, so mixing the two forms per entry would build
//!   embeddings from different prompt constructions inside one index (the
//!   §3.2 service-stack-consistency warning). IONOS documents text through
//!   `messages` in its own multimodal example; vLLM documents it generally.
//! * **instruction** (query/doc sides independently configurable) follows the
//!   official `scripts/qwen3_vl_embedding.py`: a system message, defaulting
//!   to "Represent the user's input.", punctuation-completed when the
//!   operator's string lacks a trailing one.
//! * **dimensions** (MRL truncation) is independent of the gate and rides
//!   BOTH wire forms (vLLM documents it beside `input` and beside
//!   `messages`); it must equal `dim` — the declared output dimensionality is
//!   one number, not two.
//! * **image budget**: images ride base64 data URLs (×4/3 on the wire); a
//!   payload over [`MAX_IMAGE_BYTES`] is refused before any HTTP (the
//!   OpenAI 20-MiB-per-image convention, the max_file_size discipline at the
//!   provider boundary).
//!
//! **The additive red line**: with the new fields unset (every pre-b1
//! config), the wire is byte-for-byte `{"model": …, "input": [texts…]}` and
//! the modality matrix stays [Text]/[Text] — pinned by tests, not assumed.

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

/// The system instruction sent when the operator configures none — the
/// official qwen3_vl_embedding.py default (both sides; the script always
/// attaches a system message).
pub const DEFAULT_INSTRUCTION: &str = "Represent the user's input.";

/// Raw image-payload ceiling for the messages dialect (base64 inflates ×4/3
/// on the wire, so the request body for a payload at the ceiling is
/// ~26.7 MiB). A mechanism constant in the `max_file_size_mb` family: the
/// OpenAI image-input convention is 20 MiB per image, gateway body caps
/// cluster around it, and a 2B MLLM has no use for more pixels than that —
/// refuse loudly BEFORE any HTTP rather than letting the provider 400 an
/// opaque multi-MiB body.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Per-kind config struct (§5.10.4: `{ kind, endpoint, model, dim, api_key? }`).
///
/// The b1 fields (`multimodal`, `query_instruction`, `doc_instruction`,
/// `dimensions`) are all `skip`-at-default so a pre-b1 config serializes —
/// fingerprint included — byte-identically to before (the additive red line).
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// b1 gate: switch the wire to the vLLM/IONOS `messages` dialect and the
    /// modality matrix to [Text, Image] on both sides (a unified embedder —
    /// Qwen3-VL-Embedding behind vLLM `--runner pooling` or IONOS). Off =
    /// the legacy pure-text `{"input": [texts]}` wire, byte-identical to
    /// pre-b1 deployments.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multimodal: bool,
    /// Query-side system instruction (Qwen3-VL-Embedding is
    /// instruction-aware; the card reports 1–5% from instructions, English
    /// recommended). Absent = the official default instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_instruction: Option<String>,
    /// Document-side system instruction (query and doc sides may differ —
    /// the official script takes the instruction per input, and its MMEB
    /// usage passes one on queries while documents ride the default).
    /// Absent = the official default instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_instruction: Option<String>,
    /// MRL truncation size requested from the provider via the
    /// `dimensions` request parameter (64..=model max per model card; the
    /// endpoint validates the range). Must EQUAL `dim`: the declared output
    /// dimensionality and the requested truncation are one number, not two.
    /// Independent of `multimodal` (rides both wire forms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
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
    /// b1: the messages-dialect gate + the two RESOLVED system instructions
    /// (punctuation-completed at construction; wire-ready strings).
    multimodal: bool,
    query_system: String,
    doc_system: String,
    dimensions: Option<usize>,
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
            .field("multimodal", &self.multimodal)
            .finish()
    }
}

/// The official script's instruction normalization: trim, then append `.` if
/// the last character is not punctuation (ASCII or the common CJK
/// terminators — the script's `unicodedata.category(...).startswith('P')`
/// check, covering what real instructions end with).
fn complete_instruction(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(last) = trimmed.chars().next_back() else {
        return DEFAULT_INSTRUCTION.to_string();
    };
    if last.is_ascii_punctuation() || "。！？；：…）」』”".contains(last) {
        trimmed.to_string()
    } else {
        format!("{trimmed}.")
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
            multimodal: cfg.multimodal,
            query_system: cfg
                .query_instruction
                .as_deref()
                .map(complete_instruction)
                .unwrap_or_else(|| DEFAULT_INSTRUCTION.to_string()),
            doc_system: cfg
                .doc_instruction
                .as_deref()
                .map(complete_instruction)
                .unwrap_or_else(|| DEFAULT_INSTRUCTION.to_string()),
            dimensions: cfg.dimensions,
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
        let body = serde_json::to_vec(&Body {
            model: &self.model,
            input: texts,
            dimensions: self.dimensions,
        })
        .map_err(|e| PipelineError::embed(format!("serializing request: {e}")))?;
        let resp = self.post_json(&body).await?;
        realign(resp.data, texts.len(), self.dim)
    }

    /// POST ONE messages-dialect entry (a chunk or a query), returning its
    /// single vector. One message per request — the endpoint semantics the
    /// module docs pin (vLLM: the whole `messages` list is a single prompt);
    /// the retry loop is the shared [`post_json`] one.
    async fn post_message(&self, body: &[u8]) -> Result<Vec<f32>, PipelineError> {
        let resp = self.post_json(body).await?;
        let mut v = realign(resp.data, 1, self.dim)?;
        Ok(v.swap_remove(0))
    }

    /// The shared POST + retry core (module docs §Robustness): 429/5xx/
    /// timeout/transport retry with exponential backoff, other 4xx fail
    /// fast, `MAX_RETRIES` then Err. Response `data[]` is returned raw —
    /// callers realign for their batch size.
    async fn post_json(&self, body: &[u8]) -> Result<Resp, PipelineError> {
        let url = format!("{}/embeddings", self.endpoint);
        let mut attempt: u32 = 0;
        loop {
            use http_body_util::BodyExt;
            let req = http::Request::post(&url)
                .header(
                    http::header::AUTHORIZATION,
                    format!("Bearer {}", self.api_key),
                )
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(http_body_util::Full::new(bytes::Bytes::from(body.to_vec())))
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
                        return Ok(resp);
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

    /// Serialize ONE messages-dialect request body: the system message
    /// (instruction) + a user message whose content parts are the image (as a
    /// base64 data URL, first — both verified examples order it first) and/or
    /// the text. Budget/refusal gates run BEFORE any bytes hit the wire.
    fn messages_body(
        &self,
        system: &str,
        image: Option<(&[u8], &str)>,
        text: Option<&str>,
    ) -> Result<Vec<u8>, PipelineError> {
        let mut parts: Vec<WirePart> = Vec::with_capacity(2);
        if let Some((bytes, mime)) = image {
            if bytes.is_empty() {
                return Err(PipelineError::embed(
                    "openai embedder: refusing to embed an EMPTY image payload — the messages \
                     dialect has no honest embedding of zero bytes (a routing bug upstream of \
                     this kind)",
                ));
            }
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(PipelineError::embed(format!(
                    "openai embedder: image payload is {} bytes — the multimodal messages \
                     dialect carries images as base64 data URLs (×4/3 on the wire) and refuses \
                     anything over {} MiB before any HTTP; downscale the image before ingesting \
                     (the image extractor passes bytes through verbatim)",
                    bytes.len(),
                    MAX_IMAGE_BYTES / (1024 * 1024)
                )));
            }
            use base64::Engine as _;
            let url = format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            parts.push(WirePart::ImageUrl {
                image_url: WireImageUrl { url },
            });
        }
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            parts.push(WirePart::Text {
                text: t.to_string(),
            });
        }
        if parts.is_empty() {
            return Err(PipelineError::embed(
                "openai embedder: messages request would carry no content (neither an image \
                 payload nor non-empty text) — an empty entry has no honest embedding",
            ));
        }
        serde_json::to_vec(&MessagesBody {
            model: &self.model,
            messages: vec![
                WireMessage {
                    role: "system",
                    content: vec![WirePart::Text {
                        text: system.to_string(),
                    }],
                },
                WireMessage {
                    role: "user",
                    content: parts,
                },
            ],
            dimensions: self.dimensions,
        })
        .map_err(|e| PipelineError::embed(format!("serializing request: {e}")))
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
    /// MRL truncation, only when configured (skip-at-None keeps the legacy
    /// wire byte-identical for every pre-b1 config).
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

/// One messages-dialect request (module docs: the b1 wire, field order
/// matching the verified vLLM/IONOS samples — model, messages, dimensions).
#[derive(serde::Serialize)]
struct MessagesBody<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(serde::Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<WirePart>,
}

/// One content part, internally tagged by `type` — serializes to exactly
/// `{"type":"text","text":…}` / `{"type":"image_url","image_url":{"url":…}}`
/// (the verified dialect shape).
#[derive(serde::Serialize)]
#[serde(tag = "type")]
enum WirePart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
}

#[derive(serde::Serialize)]
struct WireImageUrl {
    /// `data:<mime>;base64,<payload>`
    url: String,
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
        match q {
            Query::Text(t) => {
                if self.multimodal {
                    // text entries go messages too when the gate is on (the
                    // dialect-consistency rule in the module docs)
                    let body = self.messages_body(&self.query_system, None, Some(t))?;
                    self.post_message(&body).await
                } else {
                    // the legacy route, byte-identical to pre-b1
                    self.post_batch(std::slice::from_ref(t))
                        .await
                        .map(|mut v| v.swap_remove(0))
                }
            }
            Query::Image(bytes, mime) => {
                if !self.multimodal {
                    // text-only kind (query_modalities = [text]); M3-T1 seam
                    return Err(super::query_modality_refusal(q));
                }
                let body = self.messages_body(&self.query_system, Some((bytes, mime)), None)?;
                self.post_message(&body).await
            }
        }
    }

    async fn embed_documents(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        if self.multimodal {
            // one message per chunk (the endpoint semantics: a messages list
            // is ONE prompt — see module docs), each through the shared
            // retry loop; order is preserved one-to-one with the chunks
            let mut out = Vec::with_capacity(chunks.len());
            for c in chunks {
                let body = match &c.image {
                    Some(img) => self.messages_body(
                        &self.doc_system,
                        Some((img.bytes.as_ref(), img.mime.as_str())),
                        None,
                    )?,
                    // R2 CCH: text members embed the composed prefix + text
                    // (the one composition site — embed::embed_input)
                    None => {
                        let input = super::embed_input(c);
                        self.messages_body(&self.doc_system, None, Some(&input))?
                    }
                };
                out.push(self.post_message(&body).await?);
            }
            return Ok(out);
        }
        if let Some(c) = chunks.iter().find(|c| c.image.is_some()) {
            return Err(super::image_chunk_refusal(c));
        }
        let mut out = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(POOL_SIZE) {
            // R2 CCH: the legacy batch wire carries the composed inputs too
            let texts: Vec<String> = batch.iter().map(super::embed_input).collect();
            out.extend(self.post_batch(&texts).await?);
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        if self.multimodal {
            &[Modality::Text, Modality::Image]
        } else {
            &[Modality::Text]
        }
    }

    fn query_modalities(&self) -> &'static [Modality] {
        if self.multimodal {
            &[Modality::Text, Modality::Image]
        } else {
            &[Modality::Text]
        }
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
    /// `raw`/`body` carry the request VERBATIM (bytes + parsed view) — the
    /// b1 dialect tests assert field-by-field against them, and the additive
    /// red-line test asserts byte-for-byte.
    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    pub(crate) struct SeenRequest {
        pub auth: Option<String>,
        pub raw: Vec<u8>,
        pub body: serde_json::Value,
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
        /// When Some(dim): answer with CONTENT-SEEDED vectors (a
        /// deterministic pseudo-embedding of the whole request body) instead
        /// of the canned index vectors — the e2e suites' identity oracle
        /// (same content in ⇒ same vector out, across doc and query sides).
        pub seeded_dim: Option<usize>,
    }

    /// A deterministic pseudo-embedding of `seed` (sha256-chained blocks
    /// into [-1, 1] f32s) — identity-preserving: equal seeds ⇒ equal
    /// vectors, different seeds ⇒ (overwhelmingly) different vectors.
    pub(crate) fn seeded_vector(seed: &str, dim: usize) -> Vec<f32> {
        use sha2::Digest;
        let mut block = sha2::Sha256::digest(seed.as_bytes()).to_vec();
        let mut out: Vec<f32> = Vec::with_capacity(dim);
        while out.len() < dim {
            for b in &block {
                if out.len() < dim {
                    out.push(*b as f32 / 255.0 * 2.0 - 1.0);
                }
            }
            block = sha2::Sha256::digest(&block).to_vec();
        }
        out
    }

    /// Spawn the fake provider; returns (base_url, state).
    pub(crate) async fn fake_openai() -> (String, Fake) {
        fake_openai_with(None).await
    }

    /// [`fake_openai`] with the content-seeded embedding mode (see
    /// [`Fake::seeded_dim`]).
    pub(crate) async fn fake_openai_with(seeded_dim: Option<usize>) -> (String, Fake) {
        let fake = Fake {
            seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            statuses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            seeded_dim,
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
                            raw: body.to_vec(),
                            body: v.clone(),
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
                        // content-seeded mode: the whole request body is the
                        // seed (input form: one vector per input string, so
                        // text entries stay independently addressable)
                        if let Some(dim) = st.seeded_dim {
                            let data: Vec<serde_json::Value> = if v["input"].is_array() {
                                v["input"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .enumerate()
                                    .map(|(i, s)| {
                                        json!({
                                            "index": i,
                                            "embedding": seeded_vector(
                                                s.as_str().unwrap_or_default(), dim),
                                        })
                                    })
                                    .collect()
                            } else {
                                // messages dialect: one prompt per request
                                vec![json!({
                                    "index": 0,
                                    "embedding": seeded_vector(&v.to_string(), dim),
                                })]
                            };
                            return (
                                AxStatus::OK,
                                axum::Json(json!({ "data": data, "model": "fake" })),
                            );
                        }
                        // canned vectors: embedding of input i = [i as f32;
                        // dim], DELIBERATELY shuffled by index — the embedder
                        // must realign by index, not by array order. A
                        // messages-dialect request is a batch of ONE (index
                        // 0), regardless of the empty `input` field.
                        let n = if v["input"].is_array() {
                            v["input"].as_array().unwrap().len()
                        } else {
                            1
                        };
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
            multimodal: false,
            query_instruction: None,
            doc_instruction: None,
            dimensions: None,
        }
    }

    /// [`fake_config`] + the b1 gate on, with per-side instructions.
    pub(crate) fn fake_multimodal_config(
        endpoint: String,
        query_instruction: Option<&str>,
        doc_instruction: Option<&str>,
    ) -> OpenAiConfig {
        OpenAiConfig {
            multimodal: true,
            query_instruction: query_instruction.map(str::to_string),
            doc_instruction: doc_instruction.map(str::to_string),
            ..fake_config(endpoint)
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
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
                title: None,
                embed_prefix: None,
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
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
                title: None,
                embed_prefix: None,
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
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
                title: None,
                embed_prefix: None,
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
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
                title: None,
                embed_prefix: None,
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
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
                title: None,
                embed_prefix: None,
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
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
                title: None,
                embed_prefix: None,
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
             \"embedding-3\"\ndim = 2048",
            "every b1 field must ELIDE at its default — pre-b1 configs keep \
             serializing (fingerprint included) byte-identically"
        );
        assert!(!cfg.multimodal);
        assert!(cfg.query_instruction.is_none());
        assert!(cfg.doc_instruction.is_none());
        assert!(cfg.dimensions.is_none());
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
        // the b1 fields parse, and round-trip into the serialized form
        let cfg: OpenAiConfig = toml::from_str(
            "kind = \"openai\"\nendpoint = \"https://x/v1\"\nmodel = \"m\"\ndim = 8\n\
             multimodal = true\nquery_instruction = \"find charts\"\n\
             doc_instruction = \"Represent the document.\"\ndimensions = 8",
        )
        .unwrap();
        assert!(cfg.multimodal);
        assert_eq!(cfg.query_instruction.as_deref(), Some("find charts"));
        assert_eq!(
            cfg.doc_instruction.as_deref(),
            Some("Represent the document.")
        );
        assert_eq!(cfg.dimensions, Some(8));
        let out = toml::to_string(&cfg).unwrap();
        assert!(out.contains("multimodal = true"), "{out}");
        assert!(out.contains("dimensions = 8"), "{out}");
    }

    // -----------------------------------------------------------------------
    // b1: the multimodal messages dialect
    // -----------------------------------------------------------------------

    /// A 1x1 red PNG (real container bytes; the wire must carry them
    /// base64'd verbatim).
    fn tiny_png() -> Vec<u8> {
        crate::pipeline::extract::image::tests::solid_png((4, 4), [255, 0, 0])
    }

    fn image_chunk(index: u32, bytes: Vec<u8>, mime: &str) -> Chunk {
        Chunk {
            index,
            text: String::new(),
            image: Some(crate::pipeline::ImagePayload {
                bytes: bytes::Bytes::from(bytes),
                mime: mime.to_string(),
            }),
            kind: crate::pipeline::ChunkKind::Body,
            page: None,
            line: None,
            section: None,
            title: None,
            embed_prefix: None,
        }
    }

    fn text_chunk(index: u32, text: &str) -> Chunk {
        Chunk {
            index,
            text: text.to_string(),
            image: None,
            kind: crate::pipeline::ChunkKind::Body,
            page: None,
            line: None,
            section: None,
            title: None,
            embed_prefix: None,
        }
    }

    /// THE additive red-line pin: with the b1 fields unset, the wire bytes
    /// are EXACTLY the legacy object — no reordered keys, no new fields, no
    /// `dimensions` — byte for byte (and the modality matrix stays
    /// text/text; every pre-b1 deployment keeps its index and fingerprint).
    #[tokio::test]
    async fn legacy_wire_is_byte_identical_when_gate_unset() {
        let (base, fake) = fake_openai().await;
        let e = OpenAiEmbedder::new(&fake_config(base)).unwrap();
        assert_eq!(e.doc_modalities(), &[Modality::Text]);
        assert_eq!(e.query_modalities(), &[Modality::Text]);
        let out = e
            .embed_documents(&[text_chunk(0, "alpha"), text_chunk(1, "beta")])
            .await
            .unwrap();
        assert_eq!(out, vec![vec![0.0; 4], vec![1.0; 4]]);
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(
                seen[0].raw,
                serde_json::to_vec(&json!({"model": "embedding-fake", "input": ["alpha", "beta"]}))
                    .unwrap(),
                "byte-for-byte legacy wire (the pre-b1 shape)"
            );
        }
        // the query route keeps the same legacy shape
        e.embed_query(&Query::Text("qq".into())).await.unwrap();
        let seen = fake.seen.lock().unwrap();
        assert_eq!(
            seen[1].raw,
            serde_json::to_vec(&json!({"model": "embedding-fake", "input": ["qq"]})).unwrap(),
            "byte-for-byte legacy wire on the query route"
        );
    }

    /// R2 CCH on the WIRE: the composed input (prefix + "\n" + text) is
    /// what the provider receives — the legacy batch wire carries it in
    /// `input`, the multimodal messages dialect carries it as the user
    /// message's text member. `Chunk::text` itself is untouched (the
    /// storage/FTS side never sees the prefix — pinned at the ingest and
    /// store layers; this is the embed-side third of that boundary).
    #[tokio::test]
    async fn embed_documents_wire_carries_the_composed_prefix_input() {
        let (base, fake) = fake_openai().await;
        let prefixed = |i: u32, prefix: &str, text: &str| Chunk {
            index: i,
            text: text.to_string(),
            image: None,
            kind: crate::pipeline::ChunkKind::Body,
            page: None,
            line: None,
            section: None,
            title: None,
            embed_prefix: Some(prefix.to_string()),
        };
        // legacy batch: input = ["vol32 > 试验\n正文一", "notes\n正文二"]
        let e = OpenAiEmbedder::new(&fake_config(base)).unwrap();
        e.embed_documents(&[
            prefixed(0, "vol32 > 试验", "正文一"),
            prefixed(1, "notes", "正文二"),
        ])
        .await
        .unwrap();
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(
                seen[0].input,
                vec![
                    "vol32 > 试验\n正文一".to_string(),
                    "notes\n正文二".to_string()
                ],
                "the batch wire carries prefix + newline + text"
            );
        }
        // multimodal messages: the TEXT member is the composed input
        let (base, fake) = fake_openai().await;
        let cfg = fake_multimodal_config(base, None, None);
        let e = OpenAiEmbedder::new(&cfg).unwrap();
        e.embed_documents(&[prefixed(0, "vol32 > 评估", "正文")])
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap();
        let user_text = seen[0].body["messages"]
            .as_array()
            .and_then(|m| m.iter().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_array())
            .and_then(|c| c.first())
            .and_then(|c| c["text"].as_str());
        assert_eq!(
            user_text,
            Some("vol32 > 评估\n正文"),
            "the messages dialect carries the composed input too: {}",
            seen[0].body
        );
    }

    /// The messages dialect, asserted FIELD BY FIELD against the verified
    /// vLLM/IONOS shape: system message carries the instruction (query side
    /// for queries, doc side for documents), the user message carries the
    /// image data URL (image first) or the text, `dimensions` rides along,
    /// and every entry — text included — goes messages.
    #[tokio::test]
    async fn messages_dialect_wire_shape_field_by_field() {
        let (base, fake) = fake_openai().await;
        let cfg = fake_multimodal_config(
            base,
            Some("Find the matching document"),
            Some("Represent the document for retrieval"),
        );
        let e = OpenAiEmbedder::new(&cfg).unwrap();
        assert_eq!(e.doc_modalities(), &[Modality::Text, Modality::Image]);
        assert_eq!(e.query_modalities(), &[Modality::Text, Modality::Image]);

        let png = tiny_png();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);

        // mixed batch: [text, image] — one request per chunk, order kept;
        // every messages request is a batch of one, so the canned vector of
        // index 0 comes back per chunk
        let out = e
            .embed_documents(&[
                text_chunk(0, "hello chunk"),
                image_chunk(1, png.clone(), "image/png"),
            ])
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.0; 4], "one vector per chunk, in order");
        assert_eq!(out[1], vec![0.0; 4]);

        // text query through messages with the QUERY instruction
        e.embed_query(&Query::Text("q-text".into())).await.unwrap();
        // image query through messages with the QUERY instruction
        e.embed_query(&Query::Image(png.clone(), "image/png".to_string()))
            .await
            .unwrap();

        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "one request per chunk + one per query");
        for r in seen.iter() {
            assert_eq!(r.model, "embedding-fake");
            assert!(
                r.body.get("input").is_none(),
                "the gate routes EVERYTHING through messages: {}",
                r.body
            );
            assert_eq!(r.body["dimensions"], json!(null), "dimensions unset here");
        }
        let doc_body = &seen[0].body;
        assert_eq!(
            doc_body["messages"][0],
            json!({"role": "system", "content": [{"type": "text", "text": "Represent the \
              document for retrieval."}]}),
            "doc system message: configured instruction, punctuation-completed"
        );
        assert_eq!(
            doc_body["messages"][1],
            json!({"role": "user", "content": [{"type": "text", "text": "hello chunk"}]}),
            "a text entry goes messages too (dialect consistency)"
        );
        let img_body = &seen[1].body;
        assert_eq!(
            img_body["messages"][0]["content"][0]["text"],
            json!("Represent the document for retrieval.")
        );
        assert_eq!(
            img_body["messages"][1]["role"],
            json!("user"),
            "image chunk: one user message"
        );
        let parts = img_body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "image-only chunk carries no text part");
        assert_eq!(parts[0]["type"], json!("image_url"));
        assert_eq!(
            parts[0]["image_url"]["url"],
            json!(format!("data:image/png;base64,{b64}")),
            "data URL: declared mime + base64 of the payload bytes, verbatim"
        );
        let q_text_body = &seen[2].body;
        assert_eq!(
            q_text_body["messages"][0]["content"][0]["text"],
            json!("Find the matching document."),
            "query side carries the QUERY instruction (punctuation-completed)"
        );
        assert_eq!(
            q_text_body["messages"][1]["content"],
            json!([{"type": "text", "text": "q-text"}])
        );
        let q_img_body = &seen[3].body;
        assert_eq!(
            q_img_body["messages"][0]["content"][0]["text"],
            json!("Find the matching document.")
        );
        assert_eq!(
            q_img_body["messages"][1]["content"][0]["image_url"]["url"],
            json!(format!("data:image/png;base64,{b64}")),
            "image query: same data-URL shape as the doc side"
        );
        assert_eq!(
            q_img_body["messages"][1]["content"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    /// No instruction configured ⇒ the official default rides the system
    /// message on BOTH sides (the qwen3_vl_embedding.py behavior); an
    /// instruction already ending in punctuation (ASCII or CJK) is left
    /// alone; whitespace is trimmed.
    #[tokio::test]
    async fn instructions_default_and_punctuation_completion() {
        let (base, fake) = fake_openai().await;
        let cfg = fake_multimodal_config(base, None, None);
        let e = OpenAiEmbedder::new(&cfg).unwrap();
        e.embed_query(&Query::Text("t".into())).await.unwrap();
        e.embed_documents(&[text_chunk(0, "d")]).await.unwrap();
        {
            let seen = fake.seen.lock().unwrap();
            for r in seen.iter().take(2) {
                assert_eq!(
                    r.body["messages"][0]["content"][0]["text"],
                    json!(DEFAULT_INSTRUCTION),
                    "absent instruction = the official default: {}",
                    r.body
                );
            }
        }
        // completion matrix, as a unit on the resolver itself
        assert_eq!(complete_instruction("find cats"), "find cats.");
        assert_eq!(complete_instruction("  find cats  "), "find cats.");
        assert_eq!(complete_instruction("Find it!"), "Find it!");
        assert_eq!(complete_instruction("以图搜文档"), "以图搜文档.");
        assert_eq!(complete_instruction("找出图表。"), "找出图表。");
    }

    /// `dimensions` (MRL truncation) rides BOTH dialects — beside `input`
    /// on the legacy wire (vLLM documents exactly that curl) and beside
    /// `messages` on the dialect wire — and elides when unset (the red-line
    /// test above pins that half).
    #[tokio::test]
    async fn dimensions_rides_both_wire_forms() {
        let (base, fake) = fake_openai().await;
        let mut cfg = fake_config(base.clone());
        cfg.dimensions = Some(4); // == dim (4): the one-number rule
        let e = OpenAiEmbedder::new(&cfg).unwrap();
        e.embed_query(&Query::Text("legacy".into())).await.unwrap();
        {
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen[0].body["dimensions"], json!(4));
            assert_eq!(
                seen[0].raw,
                serde_json::to_vec(&json!({
                    "model": "embedding-fake",
                    "input": ["legacy"],
                    "dimensions": 4
                }))
                .unwrap(),
                "legacy wire + dimensions, exact bytes"
            );
        }
        let mut mm = fake_multimodal_config(base, None, None);
        mm.dimensions = Some(4);
        let e = OpenAiEmbedder::new(&mm).unwrap();
        e.embed_documents(&[text_chunk(0, "x")]).await.unwrap();
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen[1].body["dimensions"], json!(4));
        assert!(
            serde_json::to_string(&seen[1].body)
                .unwrap()
                .contains("\"dimensions\":4")
                && seen[1].raw.windows(8).any(|w| w == b"messages"),
            "messages body carries dimensions too"
        );
    }

    /// The image budget: a payload over [`MAX_IMAGE_BYTES`] is refused
    /// BEFORE any HTTP leaves (the max_file_size discipline at the provider
    /// boundary), with the size and the ceiling in the message.
    #[tokio::test]
    async fn image_budget_refused_before_http() {
        let (base, fake) = fake_openai().await;
        let e = OpenAiEmbedder::new(&fake_multimodal_config(base, None, None)).unwrap();
        let huge = vec![0u8; MAX_IMAGE_BYTES + 1];
        let err = e
            .embed_documents(&[image_chunk(0, huge, "image/png")])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refuses anything over 20 MiB"), "{msg}");
        assert!(msg.contains("downscale"), "{msg}");
        // and the empty-payload defensive refusal
        let err = e
            .embed_documents(&[image_chunk(0, Vec::new(), "image/png")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("EMPTY image payload"), "{}", err);
        assert!(
            fake.seen.lock().unwrap().is_empty(),
            "no HTTP call for budget refusals"
        );
    }

    /// The retry/idempotency discipline is THE SAME loop in the messages
    /// dialect: 429/5xx retried with backoff, success after failures, and
    /// a re-POST is safe (the body is a pure function of the chunk).
    #[tokio::test]
    async fn messages_dialect_retries_on_5xx_then_succeeds() {
        let (base, fake) = fake_openai().await;
        *fake.statuses.lock().unwrap() = vec![500, 429];
        let e = OpenAiEmbedder::new(&fake_multimodal_config(base, None, None))
            .unwrap()
            .with_backoff_base(Duration::from_millis(1));
        let v = e
            .embed_query(&Query::Image(tiny_png(), "image/png".to_string()))
            .await
            .unwrap();
        assert_eq!(v, vec![0.0; 4]);
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "2 failures + 1 success");
        // every attempt POSTed the IDENTICAL body (idempotent re-POST)
        assert_eq!(seen[0].raw, seen[1].raw);
        assert_eq!(seen[1].raw, seen[2].raw);
        assert!(seen[0].raw.windows(8).any(|w| w == b"messages"));
    }

    /// The modality matrix flip is the GATE's, and only the gate's: off ⇒
    /// the M3-T1 refusals still fire verbatim for image queries and image
    /// chunks (the pre-b1 behavior, unchanged); on ⇒ both sides accept
    /// both modalities.
    #[tokio::test]
    async fn modality_matrix_flips_with_the_gate() {
        let (base, fake) = fake_openai().await;
        // off: image query refused loudly (the M3-T1 seam text)
        let e = OpenAiEmbedder::new(&fake_config(base.clone())).unwrap();
        let err = e
            .embed_query(&Query::Image(vec![0x89, 0x50], "image/png".to_string()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("does not accept an image query"),
            "{err}"
        );
        let err = e
            .embed_documents(&[image_chunk(0, tiny_png(), "image/png")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is an image chunk"), "{err}");
        assert!(fake.seen.lock().unwrap().is_empty());
        // on: both sides, both modalities
        let e = OpenAiEmbedder::new(&fake_multimodal_config(base, None, None)).unwrap();
        assert_eq!(e.doc_modalities(), &[Modality::Text, Modality::Image]);
        assert_eq!(e.query_modalities(), &[Modality::Text, Modality::Image]);
    }

    /// Real-endpoint smoke (b1): driven by env — set
    /// `VF_TEST_QWEN3VL_ENDPOINT` (e.g. `http://127.0.0.1:8000/v1`, a
    /// `vllm serve Qwen/Qwen3-VL-Embedding-2B --runner pooling` on the box)
    /// plus `VF_OPENAI_API_KEY` when the endpoint needs one, and optionally
    /// `VF_TEST_QWEN3VL_MODEL` (default `Qwen/Qwen3-VL-Embedding-2B`) /
    /// `VF_TEST_QWEN3VL_DIM` (default 2048). Without the env, the test is
    /// skipped with a one-line reason — it must NEVER fake a network.
    #[tokio::test]
    #[ignore = "real endpoint required: set VF_TEST_QWEN3VL_ENDPOINT (vLLM --runner pooling or IONOS-style gateway)"]
    async fn real_endpoint_multimodal_smoke() {
        let Some(endpoint) = env_nonempty("VF_TEST_QWEN3VL_ENDPOINT") else {
            eprintln!(
                "skipping: VF_TEST_QWEN3VL_ENDPOINT not set — this smoke needs a real \
                 Qwen3-VL-Embedding endpoint (vllm serve Qwen/Qwen3-VL-Embedding-2B \
                 --runner pooling, or a managed one); no network is faked"
            );
            return;
        };
        let model = env_nonempty("VF_TEST_QWEN3VL_MODEL")
            .unwrap_or_else(|| "Qwen/Qwen3-VL-Embedding-2B".to_string());
        let dim = env_nonempty("VF_TEST_QWEN3VL_DIM")
            .and_then(|d| d.parse().ok())
            .unwrap_or(2048);
        let cfg = OpenAiConfig {
            kind: OpenAiTag::OpenAi,
            endpoint,
            model,
            dim,
            api_key: env_nonempty("VF_OPENAI_API_KEY"),
            multimodal: true,
            query_instruction: Some("Find the matching document".to_string()),
            doc_instruction: None,
            dimensions: None,
        };
        let e = OpenAiEmbedder::new(&cfg).unwrap();
        let t = e
            .embed_query(&Query::Text("a unified embedder smoke".into()))
            .await
            .unwrap();
        assert_eq!(t.len(), dim);
        assert!(t.iter().all(|x| x.is_finite()));
        let img = crate::pipeline::extract::image::tests::solid_png((64, 64), [10, 200, 30]);
        let v = e
            .embed_documents(&[image_chunk(0, img, "image/png")])
            .await
            .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), dim);
        assert!(v[0].iter().all(|x| x.is_finite()));
    }
}
