//! `cross-encoder` reranker kind: one ort session over a
//! sequence-classification ONNX graph (M3-T11; design.md §5.10.2 Reranker
//! row, kind = cross-encoder).
//!
//! # Model facts (verified 2026-11-07 against the actual artifacts; the
//! # research file's selection table is the adjudication record)
//!
//! Default model `gte-multilingual-reranker-base` = the onnx-community
//! conversion of Alibaba-NLP/gte-multilingual-reranker-base (Apache-2.0;
//! the conversion carries the base weights' license — the gte embedder
//! row's reasoning). The fp32 `onnx/model.onnx` is the pinned canonical
//! artifact (1,257,814,850 B): IR 10 / opset 14, inputs
//! `input_ids` + `attention_mask` + `token_type_ids` (all int64
//! `[batch, seq]`), ONE output `logits` float32 `[batch, 1]` — one raw
//! relevance logit per (query, doc) pair. `type_vocab_size: 1` in the
//! config means token_type_ids are structurally zeros; the graph keeps the
//! input and this kind feeds zeros (the shared ort.rs input-feeding
//! convention).
//!
//! **Input template — the model card's own usage, not a guess**: the pair
//! is passed to the TOKENIZER as `(query, passage)` —
//! `tokenizer([["q","p"],…], padding=True, truncation=True)` then
//! `logits.view(-1)` — i.e. the pair template the tokenizer.json itself
//! carries (`<s> query </s> </s> passage </s>` for the XLM-R family), NOT
//! string concatenation and NOT a chat template. This kind therefore calls
//! `encode_batch` over `(query, text)` DUAL inputs; the score is the RAW
//! logit (the card applies no sigmoid; sigmoid is monotone and would not
//! change the order anyway).
//!
//! # fp32 vs int8 (the default-tier adjudication, measured 2026-11-07)
//!
//! The repo ships fp16/int8/q4 variants in the same directory. Measured on
//! 12 zh/en query-doc pairs (this machine, ORT 1.31 CPU): int8 perturbs
//! scores by up to ~1.0 logit and flips the internal order of the
//! non-relevant tail (Kendall tau 0.67 on 2 of 3 query sets) while keeping
//! every relevant-vs-irrelevant verdict; fp32 is 17.5 ms/pair vs int8
//! 8.1. The default pins **fp32** — the same canonical-artifact ruling as
//! the gte embedder and the siglip2 towers (zero quantization surprises
//! baked into a default), and a reranker's entire job is FINE-GRAINED
//! ORDER, so quantization-induced order noise eats its core value. int8
//! (325 MiB) stays a self-managed variant: download by hand, point `path`
//! at it (the embedder row's documented policy).
//!
//! # Blocking discipline
//!
//! Same two layers as the embedders: tokenize + `session.run` inside
//! `spawn_blocking` (ORT holds the thread for tens of ms per pair batch);
//! the rerank call happens on the QUERY path where the request itself is
//! the concurrency cap (no ingest worker semaphore applies — same as
//! `embed_query`).
//!
//! # Determinism (the axis contract's order-independence clause)
//!
//! Rows of an attention-masked batch are mathematically independent, but
//! float reduction order can vary with the batch SHAPE — so the candidate
//! list is sorted BY KEY before tokenization and the greedy mini-batch
//! packing walks that sorted order. The same SET of candidates therefore
//! produces the identical batch shapes (identical scores, identical
//! output order) whatever order the caller presented — pinned by the
//! contract suite and the kind's own tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::{Hit, PipelineError, RerankCandidate, Reranker};
use tokenizers as _;

/// The built-in default model (§0 adjudication 2026-11-07: CMTEB-R 74.08
/// first, same base family + tokenizer source as the gte embedder,
/// Apache-2.0, onnx-community pipeline already proven in this repo).
pub const BUILTIN_MODEL: &str = "gte-multilingual-reranker-base";

/// Cap for the tokenized PAIR length. The model's
/// `max_position_embeddings` is 8192 (config.json); the card's own example
/// truncates at 512, and this project's default chunker emits ≤512-token
/// windows, so the cap only bites long-window configs + long queries.
/// Fixed (not config): a property of the model family, like the ort
/// embedder's MAX_SEQ_TOKENS.
const MAX_SEQ_TOKENS: usize = 8192;

/// Upper bound on `rows × padded-seq` per `session.run` (the ort.rs
/// mini-batch budget and its OOM story, copied verbatim in spirit:
/// BERT-family attention materializes `[batch, heads, seq, seq]` scores).
const MAX_INFER_TOKENS: usize = 8192;

/// Pad id for the XLM-R/gte family: `<pad>` is vocab id 1 (config.json
/// `pad_token_id: 1` — NOT the BERT-family 0 the ort embedder's table
/// notes quote). Inert under attention_mask=0 either way; kept honest for
/// the artifact at hand.
const PAD_ID: i64 = 1;

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// Per-kind config struct — deliberately the OrtConfig field set (same
/// resolution matrix, same air-gapped shape); a cross-encoder differs in
/// GRAPH shape and call pattern, not in what an operator configures.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossEncoderConfig {
    pub kind: CrossEncoderTag,
    /// Built-in model name (see `pipeline::models::BUILTIN_MODELS`).
    #[serde(default)]
    pub model: Option<String>,
    /// Local model directory override (layout: model.onnx + tokenizer.json).
    #[serde(default)]
    pub path: Option<String>,
    /// Never touch the network: a missing local model is a hard error
    /// instead of a prefetch.
    #[serde(default)]
    pub offline: bool,
}

/// Literal tag: only deserializes from `"cross-encoder"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CrossEncoderTag {
    #[serde(rename = "cross-encoder")]
    CrossEncoder,
}

/// Name half of model resolution (the ort.rs four-case matrix).
pub(crate) fn resolve_model_name(cfg: &CrossEncoderConfig) -> String {
    match (&cfg.model, &cfg.path) {
        (Some(m), _) => m.clone(),
        (None, Some(p)) => Path::new(p)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "local-model".to_string()),
        (None, None) => BUILTIN_MODEL.to_string(),
    }
}

/// Directory half (the ort.rs four-case matrix; `models_dir` is the
/// RESOLVED cache root).
fn resolve_model_dir(cfg: &CrossEncoderConfig, models_dir: &Path) -> PathBuf {
    match (&cfg.model, &cfg.path) {
        (_, Some(p)) => PathBuf::from(p),
        (Some(m), None) => models_dir.join(m),
        (None, None) => models_dir.join(BUILTIN_MODEL),
    }
}

// ---------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------

/// A loaded cross-encoder reranker: tokenizer + session. Construction is
/// the STARTUP GATE; afterwards immutable and `Send + Sync` for the
/// blocking pool (the OrtEmbedder shape).
pub struct CrossEncoderReranker {
    inner: Arc<Inner>,
}

struct Inner {
    /// `Session::run` takes `&mut self`; the mutex serializes the blocking
    /// pool's access (ORT parallelizes intra-op anyway — the ort.rs note).
    session: std::sync::Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
}

impl std::fmt::Debug for CrossEncoderReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossEncoderReranker")
            .finish_non_exhaustive()
    }
}

impl CrossEncoderReranker {
    /// Load + probe. Startup contract (the embedder discipline):
    /// * model dir resolves (`path` > `models_dir/<model>`; a table-model
    ///   dir missing + online ⇒ prefetch, `offline = true` ⇒ hard error);
    /// * `model.onnx` + `tokenizer.json` exist and load;
    /// * the graph's output 0 has static rank 2 with last dim 1 (one logit
    ///   per pair — the sequence-classification shape; anything else is
    ///   not a reranker graph and refuses to start).
    pub async fn new(cfg: &CrossEncoderConfig, models_dir: &Path) -> Result<Self, PipelineError> {
        let name = resolve_model_name(cfg);
        let dir = resolve_model_dir(cfg, models_dir);
        if cfg.path.is_none() {
            let ready = super::super::models::builtin(&name)
                .map(|m| super::super::models::is_ready(&dir, m))
                .unwrap_or(false);
            if !ready {
                if cfg.offline {
                    return Err(PipelineError::embed(format!(
                        "reranker model '{}' is not present under {} and offline = true forbids \
                         the network — prefetch it on a connected machine or set path = \"…\" to \
                         a local copy",
                        name,
                        dir.display()
                    )));
                }
                super::super::models::prefetch(&name, models_dir).await?;
            }
        }
        let model_path = dir.join("model.onnx");
        let tokenizer_path = dir.join("tokenizer.json");
        for p in [&model_path, &tokenizer_path] {
            if !p.is_file() {
                return Err(PipelineError::embed(format!(
                    "reranker model directory {} is incomplete (missing {}); layout = model.onnx \
                     + tokenizer.json",
                    dir.display(),
                    p.file_name()
                        .map(|s| s.to_string_lossy())
                        .unwrap_or_default()
                )));
            }
        }

        let model_path = model_path.display().to_string();
        let tokenizer_path = tokenizer_path.display().to_string();
        tokio::task::spawn_blocking(move || Self::load_and_probe(&model_path, &tokenizer_path))
            .await
            .map_err(|e| PipelineError::embed(format!("cross-encoder load task panicked: {e}")))?
    }

    /// Blocking half of construction (inside `spawn_blocking`).
    fn load_and_probe(model_path: &str, tokenizer_path: &str) -> Result<Self, PipelineError> {
        // dylib + environment: process-global one-shot, shared with the ort
        // family's sessions (embed::ort owns the stickiness).
        let source = crate::pipeline::embed::ort::locate_library()
            .map_err(|m| PipelineError::embed(format!("onnxruntime library unavailable: {m}")))?;
        let env = crate::pipeline::embed::ort::ort_environment(&source)?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path).map_err(|e| {
            PipelineError::embed(format!("loading tokenizer {tokenizer_path}: {e}"))
        })?;
        // padding OFF (encodings carry TRUE lengths; run_scores pads each
        // mini-batch to its own longest row), truncation at the family cap.
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| PipelineError::embed(format!("configuring truncation: {e}")))?;

        let session = crate::pipeline::embed::ort::commit_session(&env, model_path)?;

        // ---- output probe: `logits` [batch, 1] (one logit per pair) ----
        let outlet = session
            .outputs()
            .first()
            .ok_or_else(|| PipelineError::embed("reranker model declares no outputs"))?;
        let dims = match outlet.dtype() {
            ort::value::ValueType::Tensor { shape, .. } => {
                shape.iter().copied().collect::<Vec<i64>>()
            }
            _ => {
                return Err(PipelineError::embed(
                    "reranker model output is not a tensor — unsupported graph shape",
                ));
            }
        };
        // accept [batch, 1] (the pinned artifact) and [batch] (a squeezed
        // export still yields one logit per row); anything else refuses.
        let ok = match dims.as_slice() {
            [_, d] if *d == 1 => true,
            [_] => true,
            _ => false,
        };
        if !ok {
            return Err(PipelineError::embed(format!(
                "reranker model output shape {dims:?} is not one logit per pair (expected \
                 [batch, 1]) — this does not look like a sequence-classification reranker graph"
            )));
        }
        drop(env);

        Ok(CrossEncoderReranker {
            inner: Arc::new(Inner {
                session: std::sync::Mutex::new(session),
                tokenizer,
            }),
        })
    }

    /// Score one query against the given texts (blocking). The candidate
    /// list MUST already be key-sorted (the determinism contract; the axis
    /// dispatch sorts). Returns one RAW logit per input, input order.
    fn run_scores(
        inner: &Inner,
        query: &str,
        texts: Vec<String>,
    ) -> Result<Vec<f64>, PipelineError> {
        // the PAIR template: (query, passage) dual encoding — the model
        // card's own usage (module docs), the tokenizer.json's pair
        // post-processor does the joining
        let pairs: Vec<(String, String)> =
            texts.into_iter().map(|t| (query.to_string(), t)).collect();
        let encodings = inner
            .tokenizer
            .encode_batch(pairs, true)
            .map_err(|e| PipelineError::embed(format!("tokenizing rerank pairs: {e}")))?;
        if encodings.is_empty() {
            return Ok(Vec::new());
        }

        // greedy mini-batch packing under MAX_INFER_TOKENS (the ort.rs
        // budget loop, over the KEY-SORTED order)
        let mut groups: Vec<Vec<&tokenizers::Encoding>> = Vec::new();
        let mut cur: Vec<&tokenizers::Encoding> = Vec::new();
        let mut cur_longest = 0usize;
        for e in &encodings {
            let len = e.get_ids().len();
            if !cur.is_empty() && cur_longest.max(len) * (cur.len() + 1) > MAX_INFER_TOKENS {
                groups.push(std::mem::take(&mut cur));
                cur_longest = 0;
            }
            cur_longest = cur_longest.max(len);
            cur.push(e);
        }
        if !cur.is_empty() {
            groups.push(cur);
        }

        let mut session = inner
            .session
            .lock()
            .map_err(|_| PipelineError::embed("ort session lock poisoned"))?;
        let mut out = Vec::with_capacity(encodings.len());
        for group in &groups {
            out.extend(Self::run_group(&mut session, group)?);
        }
        Ok(out)
    }

    /// One mini-batch through the session: manual padding to the batch's
    /// longest row, run, read `logits` [batch, 1] -> one f64 per row.
    fn run_group(
        session: &mut ort::session::Session,
        group: &[&tokenizers::Encoding],
    ) -> Result<Vec<f64>, PipelineError> {
        let batch = group.len();
        let seq = group.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
        if seq == 0 {
            return Err(PipelineError::embed(
                "tokenizer produced an empty encoding (empty query AND text?) — nothing to score",
            ));
        }
        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        for e in group {
            let eids = e.get_ids();
            let emask = e.get_attention_mask();
            for s in 0..seq {
                ids.push(if s < eids.len() {
                    eids[s] as i64
                } else {
                    PAD_ID
                });
                mask.push(if s < emask.len() { emask[s] as i64 } else { 0 });
            }
        }
        // feed exactly the inputs the graph declares (the ort.rs
        // convention: input_ids / attention_mask / token_type_ids)
        let mut inputs: Vec<(String, ort::value::DynValue)> = Vec::new();
        for outlet in session.inputs() {
            let name = outlet.name().to_string();
            let shape2 = vec![batch as i64, seq as i64];
            let tensor = match name.as_str() {
                "input_ids" => ort::value::Tensor::from_array((shape2, ids.clone()))
                    .map_err(|e| PipelineError::embed(format!("input_ids tensor: {e}")))?,
                "attention_mask" => ort::value::Tensor::from_array((shape2, mask.clone()))
                    .map_err(|e| PipelineError::embed(format!("attention_mask tensor: {e}")))?,
                other if other.contains("token_type") => {
                    ort::value::Tensor::from_array((shape2, vec![0i64; batch * seq]))
                        .map_err(|e| PipelineError::embed(format!("token_type_ids tensor: {e}")))?
                }
                other => {
                    return Err(PipelineError::embed(format!(
                        "ONNX graph input '{other}' is not supported (transformers exports use \
                         input_ids / attention_mask / token_type_ids"
                    )));
                }
            };
            inputs.push((name, tensor.into_dyn()));
        }
        let outputs = session
            .run(inputs)
            .map_err(|e| PipelineError::embed(format!("rerank inference failed: {e}")))?;

        let value: &ort::value::DynValue = &outputs[0];
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| PipelineError::embed(format!("reading reranker output: {e}")))?;
        let shape: Vec<i64> = shape.iter().copied().collect();
        // [batch, 1] or [batch] — one logit per row either way (the probe
        // pinned one of these two at startup)
        let stride = if shape.len() == 2 {
            shape[1] as usize
        } else {
            1
        };
        if shape.is_empty() || shape[0] as usize != batch || (shape.len() == 2 && stride != 1) {
            return Err(PipelineError::embed(format!(
                "reranker output shape {shape:?} does not match the batch we sent ({batch} pairs, \
                 1 logit each) — graph/output mismatch"
            )));
        }
        Ok((0..batch).map(|r| data[r * stride] as f64).collect())
    }
}

#[async_trait::async_trait]
impl Reranker for CrossEncoderReranker {
    /// The axis contract in one method: key-sort -> score -> total order.
    /// Sorting the candidates BY KEY before scoring is what makes the
    /// output independent of the input order (batch shapes become a
    /// function of the SET, module docs); the final sort is the same
    /// `total_cmp + key` tie-break `merge_hitlists` uses.
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
    ) -> Result<Vec<Hit>, PipelineError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // key-sort (stable over an already-unique-key list): the scoring
        // order is a pure function of the candidate SET
        let mut sorted: Vec<&RerankCandidate> = candidates.iter().collect();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));

        let inner = Arc::clone(&self.inner);
        let texts: Vec<String> = sorted.iter().map(|c| c.text.clone()).collect();
        let query = query.to_string();
        let scores = tokio::task::spawn_blocking(move || Self::run_scores(&inner, &query, texts))
            .await
            .map_err(|e| PipelineError::embed(format!("cross-encoder task panicked: {e}")))??;

        let mut hits: Vec<Hit> = sorted
            .iter()
            .zip(scores)
            .map(|(c, score)| Hit {
                key: c.key.clone(),
                score,
            })
            .collect();
        // score descending, tie-break by key ascending (the merge
        // convention; total_cmp gives the total order over f64)
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        Ok(hits)
    }
}

// ---------------------------------------------------------------------------
// tests — hermetic: the committed tiny-reranker fixture + the real .so
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The committed fixture directory (Gather+ReduceSum ONNX: one logit
    /// per pair = sum of (token_id+1) over the MASKED positions; word-level
    /// tokenizer.json with a PAIR post-processor).
    pub(crate) fn fixture_dir() -> PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-reranker",
            "tests/fixtures/tiny-reranker",
        ] {
            let d = PathBuf::from(p);
            if d.join("model.onnx").is_file() {
                return d;
            }
        }
        panic!(
            "tiny-reranker fixture not found (expected tests/fixtures/tiny-reranker with \
             model.onnx + tokenizer.json) — run its generate.py if it was removed"
        )
    }

    fn fixture_config() -> CrossEncoderConfig {
        CrossEncoderConfig {
            kind: CrossEncoderTag::CrossEncoder,
            model: Some("tiny-reranker-fixture".to_string()),
            path: Some(fixture_dir().display().to_string()),
            offline: true,
        }
    }

    pub(crate) async fn reranker_or_panic() -> CrossEncoderReranker {
        match crate::pipeline::embed::ort::locate_library() {
            Ok(_) => CrossEncoderReranker::new(&fixture_config(), Path::new("/nonexistent"))
                .await
                .expect("fixture reranker must load against the real .so"),
            Err(m) => panic!(
                "ort tests need the real library and none was found: {m} — {}. Tests refuse to \
                 skip (M2-T4 honest-failure rule).",
                crate::pipeline::embed::ort::FETCH_HINT
            ),
        }
    }

    fn cands(pairs: &[(&str, &str)]) -> Vec<RerankCandidate> {
        pairs
            .iter()
            .map(|(k, t)| RerankCandidate::new(*k, *t))
            .collect()
    }

    #[tokio::test]
    async fn loads_fixture_and_scores_pairs() {
        let r = reranker_or_panic().await;
        let out = r
            .rerank(
                "hello vectorfs",
                &cands(&[("k1", "first chunk"), ("k2", "second chunk")]),
            )
            .await
            .unwrap();
        assert_eq!(out.len(), 2, "rerank re-orders, never filters");
        assert!(out.iter().all(|h| h.score.is_finite()));
        // the fixture's logit = sum of (id+1) over the pair sequence
        // ([CLS]=2, words, [SEP]=3): a longer doc sums higher
        let (lo, hi) = if out[0].key == "k2" {
            (out[1].clone(), out[0].clone())
        } else {
            (out[0].clone(), out[1].clone())
        };
        assert!(hi.score > lo.score, "{out:?}");
        // determinism: same call, byte-identical output
        let again = r
            .rerank(
                "hello vectorfs",
                &cands(&[("k1", "first chunk"), ("k2", "second chunk")]),
            )
            .await
            .unwrap();
        assert_eq!(out, again);
    }

    /// THE order-independence pin (the axis contract): the candidate ORDER
    /// must not change scores OR output. Same set, three permutations
    /// (including reverse) — outputs byte-identical.
    #[tokio::test]
    async fn candidate_order_never_changes_the_result() {
        let r = reranker_or_panic().await;
        let set = [
            ("k-a", "alpha doc"),
            ("k-b", "beta doc with more tokens here"),
            ("k-c", "gamma"),
            ("k-d", "the quick brown fox embeds"),
            ("k-e", ""),
        ];
        let base = r.rerank("hello vectorfs", &cands(&set)).await.unwrap();
        let mut rev = set.to_vec();
        rev.reverse();
        let out_rev = r.rerank("hello vectorfs", &cands(&rev)).await.unwrap();
        assert_eq!(base, out_rev, "reversed input must reproduce the output");
        let rot = [set[2], set[0], set[4], set[3], set[1]];
        let out_rot = r.rerank("hello vectorfs", &cands(&rot)).await.unwrap();
        assert_eq!(base, out_rot, "rotated input must reproduce the output");
        // total order: score descending, equal-score runs key-sorted
        for w in base.windows(2) {
            assert!(
                w[0].score > w[1].score || (w[0].score == w[1].score && w[0].key < w[1].key),
                "not totally ordered: {base:?}"
            );
        }
    }

    #[tokio::test]
    async fn empty_candidates_and_single_candidate() {
        let r = reranker_or_panic().await;
        assert!(r.rerank("q", &[]).await.unwrap().is_empty());
        let one = r.rerank("q", &cands(&[("only", "one doc")])).await.unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].key, "only");
    }

    /// Bad-output-shape twin (the ort.rs model_flat discipline): a graph
    /// whose output is NOT one-logit-per-pair refuses at STARTUP. The
    /// tiny-EMBEDDER fixture (`[batch, seq, 8]` hidden states) is exactly
    /// such a graph — the sharpest real-file probe available.
    #[tokio::test]
    async fn wrong_output_shape_fails_startup() {
        let mut embedder_fixture = None;
        for p in [
            "vf/tests/fixtures/tiny-embedder",
            "tests/fixtures/tiny-embedder",
        ] {
            let d = PathBuf::from(p);
            if d.join("model.onnx").is_file() {
                embedder_fixture = Some(d);
                break;
            }
        }
        let embedder_fixture =
            embedder_fixture.unwrap_or_else(|| panic!("tiny-embedder fixture not found"));
        let cfg = CrossEncoderConfig {
            kind: CrossEncoderTag::CrossEncoder,
            model: Some("tiny-reranker-fixture".to_string()),
            path: Some(embedder_fixture.display().to_string()),
            offline: true,
        };
        let err = CrossEncoderReranker::new(&cfg, Path::new("/nonexistent"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("one logit per pair"), "{msg}");
        assert!(msg.contains("[batch, 1]"), "{msg}");
    }

    #[tokio::test]
    async fn offline_missing_model_is_an_error() {
        let cfg = CrossEncoderConfig {
            kind: CrossEncoderTag::CrossEncoder,
            model: Some(BUILTIN_MODEL.to_string()),
            path: None,
            offline: true,
        };
        let dir = tempfile::tempdir().unwrap();
        let err = CrossEncoderReranker::new(&cfg, dir.path())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("offline = true forbids the network"), "{msg}");
        assert!(msg.contains(BUILTIN_MODEL), "{msg}");
    }

    #[test]
    fn config_roundtrip_and_guards() {
        // bare kind: defaults to the built-in reranker model, online
        let cfg: CrossEncoderConfig = toml::from_str("kind = \"cross-encoder\"").unwrap();
        assert_eq!(cfg.model, None);
        assert_eq!(resolve_model_name(&cfg), BUILTIN_MODEL);
        assert!(!cfg.offline);
        // full form round-trips flat
        let full = "kind = \"cross-encoder\"\nmodel = \"m\"\npath = \"/p\"\noffline = true\n";
        let cfg: CrossEncoderConfig = toml::from_str(full).unwrap();
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"cross-encoder\"\nmodel = \"m\"\npath = \"/p\"\noffline = true"
        );
        // wrong kind literal / unknown field rejected
        assert!(toml::from_str::<CrossEncoderConfig>("kind = \"ort\"").is_err());
        assert!(
            toml::from_str::<CrossEncoderConfig>("kind = \"cross-encoder\"\ntop_n = 5").is_err()
        );
    }

    /// REAL model smoke (downloads ~1.17 GiB through the prefetch path):
    /// #[ignore] like the ort real-model smoke — run explicitly at gates.
    /// The quality acceptance: relevant > irrelevant on zh/en pairs, the
    /// input-pair template producing scores consistent with the model card
    /// example (raw logits in a ~[-4, +4] band, not garbage magnitudes).
    #[tokio::test]
    #[ignore = "downloads the real ~1.17 GiB gte reranker weights; run explicitly"]
    async fn real_gte_reranker_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = CrossEncoderConfig {
            kind: CrossEncoderTag::CrossEncoder,
            model: Some(BUILTIN_MODEL.to_string()),
            path: None,
            offline: false,
        };
        let r = CrossEncoderReranker::new(&cfg, dir.path()).await.unwrap();
        let zh = r
            .rerank(
                "大熊猫的栖息地在哪里",
                &cands(&[
                    ("hit", "大熊猫主要栖息在中国四川、陕西和甘肃的山区竹林中。"),
                    ("miss", "北京是中国的首都，拥有三千多年的建城历史。"),
                ]),
            )
            .await
            .unwrap();
        // sample output for the record (visible with --nocapture): the raw
        // logits through THIS kind's tokenize -> ort -> logit path
        println!("smoke zh 大熊猫的栖息地在哪里 -> {zh:?}");
        assert_eq!(zh.len(), 2);
        assert_eq!(zh[0].key, "hit", "relevant zh doc must outrank the decoy");
        assert!(zh[0].score > 0.0, "card-scale raw logit: {zh:?}");
        assert!(zh[1].score < zh[0].score);
        let en = r
            .rerank(
                "What is the capital of China?",
                &cands(&[
                    (
                        "miss",
                        "The giant panda is a bear species endemic to China.",
                    ),
                    (
                        "hit",
                        "Beijing is the capital of the People's Republic of China.",
                    ),
                ]),
            )
            .await
            .unwrap();
        println!("smoke en What is the capital of China? -> {en:?}");
        assert_eq!(en[0].key, "hit", "relevant en doc must outrank the decoy");
        // order independence against the real engine too
        let en_rev = r
            .rerank(
                "What is the capital of China?",
                &cands(&[
                    (
                        "hit",
                        "Beijing is the capital of the People's Republic of China.",
                    ),
                    (
                        "miss",
                        "The giant panda is a bear species endemic to China.",
                    ),
                ]),
            )
            .await
            .unwrap();
        assert_eq!(en, en_rev);
    }
}
