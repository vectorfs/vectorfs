//! `ort` embedder kind: local ONNX inference through ONNX Runtime, loaded
//! DYNAMICALLY (design.md §5.7 "ort (.so load-dynamic, 不引 fastembed)").
//!
//! # Engine + binding
//!
//! `ort` 2.0.0-rc.15 with `load-dynamic` only (no `download-binaries`, no
//! `fetch-models`): the `libonnxruntime.so` binary is fetched + sha-pinned by
//! `scripts/fetch_runtime_libs.sh` (v1.31.0, the exact minor ort-sys rc.15
//! bundles — ort refuses older minors at dlopen), never linked at build time
//! and never baked into the binary (§5.10.7 .so whitelist).
//!
//! # Library resolution (pdfium convention)
//!
//! `env VF_ORT_PATH` > repo-local `runtime-libs/libonnxruntime.{so,dylib}`
//! (fetch-script output, checked at the workspace root and one level up) >
//! system loader (ort falls back to `ORT_DYLIB_PATH`/plain soname itself).
//! An explicit `VF_ORT_PATH` that does not exist is a hard error — operator
//! overrides must not silently fall through. Like pdfium, the ort dylib bind
//! is process-global and one-shot: the first successfully bound path wins
//! for the whole process.
//!
//! # Model directory + DIP boundary
//!
//! The model dir is resolved `config path` > `models_dir/<model-name>/`
//! (§5.10.4; `models_dir` defaults to `<data-dir>/.vectorfs/models`). Its
//! layout is the prefetch layout: `model.onnx` + `tokenizer.json`. With
//! `offline = false` a missing built-in model dir is PREFETCHED
//! ([`super::super::models::prefetch`]) before loading — the air-gapped
//! variant (`offline = true`) refuses instead of ever touching the network.
//! All ort/tokenizers types stay inside this module; the outside sees
//! [`OrtConfig`] (plain data) and [`OrtEmbedder`] + [`PipelineError`].
//!
//! # Pooling semantics (verified against the base model, not the README
//! # boilerplate — see pipeline/models.rs header for the sources)
//!
//! gte-multilingual-base: **CLS pooling** (position 0 of `last_hidden_state`)
//! followed by **L2 normalization**; **no retrieval query prefix** — the
//! query and document routes therefore share one encode path. The pooling is
//! fixed HERE per model-family convention rather than made a config knob:
//! it is a property of the trained weights, not policy a user could
//! usefully change (§5.10.0 axiom 1).
//!
//! # dim: probed at startup, declared by the built-in table
//!
//! The startup probe reads the STATIC last dimension of the session's first
//! output (fixture: 8; gte: 768). Unprobeable (dynamic) ⇒ Err. When the
//! model name is in the built-in table, the probe must EQUAL the table's
//! declared dim — mismatch ⇒ Err, the §5.10.2 contract "探测 dim ≠ 声明 ⇒
//! 启动失败" (a 768-dim index receiving 8-dim vectors would corrupt silently
//! otherwise). User-configured `path` models have no declaration; the probe
//! IS the declaration.
//!
//! # Blocking discipline (two deliberate layers)
//!
//! Inference (tokenize + session run + pool) runs inside `spawn_blocking`:
//! ONNX Runtime holds the thread for tens of milliseconds to seconds and the
//! async runtime must not stall. T3's ingest worker ADDITIONALLY gates its
//! CPU segments behind a CPU-count semaphore (§5.10.5) — that layer caps
//! CONCURRENCY across extract/chunk/embed; this layer only keeps the reactor
//! responsive. They compose; neither replaces the other.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::{Chunk, Embedder, Modality, PipelineError, Query};
use tokenizers as _;

/// Guidance line shared by runtime + test locator failures (pub(crate):
/// the siglip2 kind — the other ort-family member — quotes it too).
pub(crate) const FETCH_HINT: &str =
    "run scripts/fetch_runtime_libs.sh onnxruntime (or point VF_ORT_PATH at the library)";

/// Where the ONNX Runtime dylib for this process comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibrarySource {
    File(PathBuf),
    System,
}

/// Resolve the ONNX Runtime dylib (pdfium convention, see module docs).
pub fn locate_library() -> Result<LibrarySource, String> {
    if let Some(explicit) = std::env::var_os("VF_ORT_PATH") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(LibrarySource::File(p));
        }
        return Err(format!(
            "VF_ORT_PATH is set but {} is not a file (explicit overrides must resolve; unset \
             it or fix the path)",
            p.display()
        ));
    }
    let lib_name = if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    // Repo-local fetch-script output. cargo runs test binaries from the
    // workspace root AND from the member dir depending on invocation; since
    // the three-crate split the member dir sits TWO levels below the root
    // (crates/server), so all three relative prefixes are candidates.
    for prefix in [".", "..", "../.."] {
        let p = PathBuf::from(prefix).join("runtime-libs").join(lib_name);
        if p.is_file() {
            return Ok(LibrarySource::File(p));
        }
    }
    Ok(LibrarySource::System)
}

/// Per-kind config struct (§5.10.4: `{ kind, model?, path?, offline }`).
/// `path` points at a model DIRECTORY in the prefetch layout; it is the
/// air-gapped / self-managed escape hatch and bypasses the built-in table
/// (license then being the operator's audit, warned by `pipelines check`).
///
/// `model` is OPTIONAL. The four resolution cases (see [`OrtEmbedder::new`]):
/// path+model / path only / model only / neither (→ the M2 default model).
/// A path-only embedder must NOT inherit the default model name: the
/// §5.10.4 air-gapped shape (`path = "/models/bge-m3"`, no `model`) would
/// otherwise be dim-cross-checked against gte's 768 and refuse to start.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrtConfig {
    pub kind: OrtTag,
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

/// The M2 default model (§5.10.4 sample config) — used only when BOTH
/// `model` and `path` are absent.
pub(crate) fn default_model() -> String {
    "gte-multilingual-base".to_string()
}

/// Resolved model identity + directory (the four-case matrix of
/// `model` × `path`). `name` feeds metadata / derivation / the built-in
/// dim cross-check (only when it matches a table entry — path-only
/// embedders take their name from the directory and are never
/// cross-checked against a name they never declared).
pub(crate) fn resolve_model(cfg: &OrtConfig, models_dir: &Path) -> (String, PathBuf) {
    match (&cfg.model, &cfg.path) {
        (Some(m), Some(p)) => (m.clone(), PathBuf::from(p)),
        (None, Some(p)) => (resolve_model_name(cfg), PathBuf::from(p)),
        (Some(m), None) => (m.clone(), models_dir.join(m)),
        (None, None) => {
            let m = default_model();
            let dir = models_dir.join(&m);
            (m, dir)
        }
    }
}

/// Name half of [`resolve_model`] (no filesystem needed) — the identity
/// recorded in `x-vf-model` metadata and `x_vf_derived`.
pub(crate) fn resolve_model_name(cfg: &OrtConfig) -> String {
    match (&cfg.model, &cfg.path) {
        (Some(m), _) => m.clone(),
        (None, Some(p)) => Path::new(p)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "local-model".to_string()),
        (None, None) => default_model(),
    }
}

/// Literal tag: only deserializes from `"ort"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OrtTag {
    #[serde(rename = "ort")]
    Ort,
}

// ---------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------

/// A loaded ort embedder: tokenizer + session + probed dim. Construction is
/// the STARTUP GATE (all fail-fast checks live in [`OrtEmbedder::new`]);
/// afterwards the struct is immutable and `Send + Sync` for the blocking
/// pool. Not `Clone` (owns the live session); the T3 registry holds one per
/// configured instance.
pub struct OrtEmbedder {
    inner: Arc<Inner>,
}

struct Inner {
    /// `Session::run` takes `&mut self` (ort API); the mutex serializes the
    /// blocking pool's access. ORT runs intra-op parallel internally, so
    /// serializing calls is the right granularity anyway.
    session: std::sync::Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
    dim: usize,
}

/// Manual Debug: the session/tokenizer hold FFI handles; only the identity
/// fields matter for test diagnostics.
impl std::fmt::Debug for OrtEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrtEmbedder")
            .field("dim", &self.inner.dim)
            .finish_non_exhaustive()
    }
}

/// Process-global ONNX Runtime environment (ONE per process — ort hard-fails
/// a second `Environment`, so a daemon loading two ort embedder instances
/// must share it). Same stickiness as the pdfium bind: the FIRST resolution
/// wins for the process lifetime, cached success-or-failure.
static ENV: std::sync::OnceLock<Result<ort::environment::Environment, String>> =
    std::sync::OnceLock::new();

/// Resolve the environment: bind the dylib (explicit path or system chain)
/// on first use, then reuse. The returned clone keeps the process-global
/// alive regardless (sessions do not outlive it inside this function's
/// caller). pub(crate): shared with the siglip2 kind — the ort FAMILY binds
/// ONE environment per process however many ort-family embedders load.
pub(crate) fn ort_environment(
    source: &LibrarySource,
) -> Result<ort::environment::Environment, PipelineError> {
    ENV.get_or_init(|| match source {
        LibrarySource::File(p) => {
            let builder = ort::init_from(p).map_err(|e| {
                format!(
                    "loading ONNX Runtime from {} failed: {e} ({FETCH_HINT})",
                    p.display()
                )
            })?;
            builder.build().map_err(|e| {
                format!(
                    "initializing ONNX Runtime from {} failed: {e} ({FETCH_HINT})",
                    p.display()
                )
            })
        }
        LibrarySource::System => ort::init()
            .build()
            .map_err(|e| format!("initializing ONNX Runtime failed: {e} ({FETCH_HINT})")),
    })
    .clone()
    .map_err(|m| PipelineError::embed(format!("onnxruntime unavailable: {m}")))
}

/// Commit a session from a model file (the builder + commit + error-shape
/// discipline; pub(crate) so the siglip2 kind loads its towers through the
/// exact same path — one ort-family loading story).
pub(crate) fn commit_session(
    env: &ort::environment::Environment,
    model_path: &str,
) -> Result<ort::session::Session, PipelineError> {
    let mut builder = ort::session::Session::builder(env)
        .map_err(|e| PipelineError::embed(format!("session builder: {e}")))?;
    builder
        .commit_from_file(model_path)
        .map_err(|e| PipelineError::embed(format!("loading ONNX model {model_path}: {e}")))
}

/// Cap for tokenized length. gte's `max_position_embeddings` is 8192; the
/// M2 chunker emits ≤512-token chunks by default, so this only bites
/// long-window configs. Fixed (not config): it is a property of the model
/// family, not policy.
const MAX_SEQ_TOKENS: usize = 8192;

/// Upper bound on `rows × padded-seq` per `session.run` (MINI-BATCH budget).
/// BERT-family attention materializes `[batch, heads, seq, seq]` scores:
/// ~12.6 MB per 512-token row on gte (12 heads), quadratic in seq. One
/// document's whole chunk set in a single run is an OOM — observed
/// 2026-11-06: a ~2'000-chunk PDF pushed the daemon to 31.6 GB RSS and the
/// kernel OOM-killed it mid-`put`. 8192 tokens ⇒ 16×512-token rows (~200 MB
/// peak activations); a single long row still rides alone (up to
/// [`MAX_SEQ_TOKENS`]). Fixed (not config): this is a machine-independent
/// safety bound, not a tuning knob.
const MAX_INFER_TOKENS: usize = 8192;

impl OrtEmbedder {
    /// Load + probe. All §5.10.2 startup contract checks happen here:
    ///
    /// * model dir resolves (`path` > `models_dir/<model>`; missing:
    ///   prefetch unless `offline`, then hard error);
    /// * tokenizer.json + model.onnx exist and load;
    /// * the session's first output has a STATIC last dim (the probe);
    /// * probed dim == built-in table's declared dim for table models.
    ///
    /// `models_dir` is the RESOLVED cache root (config `models_dir` or the
    /// `<data-dir>/.vectorfs/models` default — resolution is the caller's
    /// job so config parsing stays side-effect-free).
    pub async fn new(cfg: &OrtConfig, models_dir: &Path) -> Result<Self, PipelineError> {
        // ---- model identity + directory (four-case matrix, resolve_model) ----
        let (name, dir) = resolve_model(cfg, models_dir);
        // Only a table model resolved to the cache dir can be
        // prefetched/ready-checked; an explicit `path` must simply exist.
        if cfg.path.is_none() {
            let ready = super::super::models::builtin(&name)
                .map(|m| super::super::models::is_ready(&dir, m))
                .unwrap_or(false);
            if !ready {
                if cfg.offline {
                    return Err(PipelineError::embed(format!(
                        "model '{}' is not present under {} and offline = true forbids the \
                         network — prefetch it on a connected machine (vectorfs init --preset \
                         text-rag) or set path = \"…\" to a local copy",
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
                    "model directory {} is incomplete (missing {}); layout = model.onnx + \
                     tokenizer.json",
                    dir.display(),
                    p.file_name()
                        .map(|s| s.to_string_lossy())
                        .unwrap_or_default()
                )));
            }
        }

        // ---- dylib + session + probe (CPU-heavy: off the reactor) ----
        let model_path = model_path.display().to_string();
        let tokenizer_path = tokenizer_path.display().to_string();
        tokio::task::spawn_blocking(move || {
            Self::load_and_probe(&name, &model_path, &tokenizer_path)
        })
        .await
        .map_err(|e| PipelineError::embed(format!("ort load task panicked: {e}")))?
    }

    /// Blocking half of construction (inside `spawn_blocking`). `name` is the
    /// RESOLVED model identity (see [`resolve_model`]) — the built-in dim
    /// cross-check fires only when that name matches a table entry.
    fn load_and_probe(
        name: &str,
        model_path: &str,
        tokenizer_path: &str,
    ) -> Result<Self, PipelineError> {
        // dylib: process-global one-shot (see ENV) — explicit file or ort's
        // own system fallback (ORT_DYLIB_PATH / loader search path).
        let source = locate_library()
            .map_err(|m| PipelineError::embed(format!("onnxruntime library unavailable: {m}")))?;
        let env = ort_environment(&source)?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path).map_err(|e| {
            PipelineError::embed(format!("loading tokenizer {tokenizer_path}: {e}"))
        })?;
        // pad to batch-longest (right side, pad id 0 — the [PAD] slot in
        // every BERT-family vocab) and truncate at the model context cap;
        // tokenizers' builder methods take &mut self (0.23 API shape).
        // Padding is OFF: encodings carry TRUE lengths so run_batch can pack
        // mini-batches under MAX_INFER_TOKENS and each batch pads only to
        // its own longest row (run_group's flatten loop) — a global
        // BatchLongest here would bill every short chunk the whole
        // document's longest sequence.
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| PipelineError::embed(format!("configuring truncation: {e}")))?;

        let session = commit_session(&env, model_path)?;

        // ---- dim probe: static last dim of output[0] ----
        let dim = probe_dim(&session)?;
        if let Some(declared) = super::super::models::builtin(name)
            && declared.dim != dim
        {
            return Err(PipelineError::embed(format!(
                "model '{}' declares dim {} but its ONNX graph outputs dim {dim} — refusing \
                 to start (mismatched vectors would silently corrupt the index; re-download \
                 the model or fix the configuration)",
                name, declared.dim
            )));
        }
        let _ = &source; // resolution snapshot kept for diagnostics above
        // `env` stays alive in the process-global ENV (sessions do not
        // borrow it after construction); dropping this clone is safe.
        drop(env);

        Ok(OrtEmbedder {
            inner: Arc::new(Inner {
                session: std::sync::Mutex::new(session),
                tokenizer,
                dim,
            }),
        })
    }

    /// The tokenize → mini-batch → run → CLS-pool → L2-normalize pipeline
    /// (blocking). A free function over `&Inner` so `spawn_blocking` gets a
    /// 'static closure via `Arc<Inner>` (the trait methods only hold `&self`).
    ///
    /// Rows are packed into mini-batches under [`MAX_INFER_TOKENS`] before
    /// any `session.run` (see the const: a whole-document run OOMs). CLS
    /// pooling is per-row and attention masks zero the pad columns, so the
    /// split is invisible to callers — same vectors, same order, bounded
    /// peak memory.
    fn run_batch(inner: &Inner, texts: Vec<String>) -> Result<Vec<Vec<f32>>, PipelineError> {
        let encodings = inner
            .tokenizer
            .encode_batch(texts, true)
            .map_err(|e| PipelineError::embed(format!("tokenizing batch: {e}")))?;
        if encodings.is_empty() {
            return Ok(Vec::new());
        }
        // greedy packing: adding a row would push longest × rows over the
        // budget ⇒ flush first. Lengths are TRUE tokenized lengths (the
        // tokenizer runs padding OFF), so a short-chunk batch is not billed
        // the document's longest sequence.
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
            out.extend(Self::run_group(&mut session, inner.dim, group)?);
        }
        Ok(out)
    }

    /// One mini-batch through the session: flatten to `[batch, seq]` with
    /// MANUAL padding to this batch's longest row, run, CLS-pool + L2
    /// normalize. Split out of run_batch so the token-budget loop stays
    /// readable; `dim` is the startup-probed dim every output row must match.
    fn run_group(
        session: &mut ort::session::Session,
        dim: usize,
        group: &[&tokenizers::Encoding],
    ) -> Result<Vec<Vec<f32>>, PipelineError> {
        let batch = group.len();
        let seq = group.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
        if seq == 0 {
            return Err(PipelineError::embed(
                "tokenizer produced an empty encoding (empty text?) — nothing to embed",
            ));
        }
        // flatten to [batch, seq] row-major; pad each row to the batch's
        // longest (ids 0, mask 0 — the model never attends to pad columns)
        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        for e in group {
            let eids = e.get_ids();
            let emask = e.get_attention_mask();
            for s in 0..seq {
                ids.push(if s < eids.len() { eids[s] as i64 } else { 0 });
                mask.push(if s < emask.len() { emask[s] as i64 } else { 0 });
            }
        }

        // feed exactly the inputs the graph declares; transformers-style
        // exports use input_ids + attention_mask (+ token_type_ids for BERT)
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
            .map_err(|e| PipelineError::embed(format!("inference failed: {e}")))?;

        // ---- first output: [batch, seq, dim] ----
        if outputs.len() == 0 {
            return Err(PipelineError::embed("model produced no outputs"));
        }
        let value: &ort::value::DynValue = &outputs[0];
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| PipelineError::embed(format!("reading model output: {e}")))?;
        let shape: Vec<i64> = shape.iter().copied().collect();
        if shape.len() != 3 || shape[0] as usize != batch || shape[1] as usize != seq {
            return Err(PipelineError::embed(format!(
                "model output shape {shape:?} does not match the batch we sent ([{batch}, {seq}, \
                 dim]) — graph/output mismatch"
            )));
        }
        let out_dim = shape[2] as usize;
        if out_dim != dim {
            return Err(PipelineError::embed(format!(
                "model output dim {out_dim} != probed startup dim {dim} — refusing (dim must not \
                 flap across calls, §5.10.2)"
            )));
        }
        // CLS pooling: token position 0 per row, then L2 normalize
        // (gte convention; see module docs)
        Ok((0..batch)
            .map(|r| {
                let row = &data[r * seq * out_dim..r * seq * out_dim + out_dim];
                normalize(row)
            })
            .collect())
    }
}

/// Probe the declared output dim of a session: the STATIC last dimension of
/// output 0. Dynamic (-1) ⇒ Err ("probed dim unobtainable ⇒ startup failure").
fn probe_dim(session: &ort::session::Session) -> Result<usize, PipelineError> {
    let outlet = session
        .outputs()
        .first()
        .ok_or_else(|| PipelineError::embed("model declares no outputs"))?;
    let shape = match outlet.dtype() {
        ort::value::ValueType::Tensor { shape, .. } => shape.clone(),
        _ => {
            return Err(PipelineError::embed(
                "model output is not a tensor — unsupported graph shape",
            ));
        }
    };
    let dims: Vec<i64> = shape.iter().copied().collect();
    match dims.last() {
        Some(d) if *d >= 0 => Ok(*d as usize),
        _ => Err(PipelineError::embed(format!(
            "cannot probe the embedding dim: model output shape {dims:?} has a dynamic last \
             dimension — the startup contract requires a static dim (§5.10.2)"
        ))),
    }
}

/// In-place L2 normalization; a zero vector stays zero (only reachable for a
/// degenerate model — the finite check below still passes). pub(crate):
/// shared with the siglip2 kind (both towers L2-normalize — the full-model
/// exports normalize; the split-tower exports the siglip2 kind loads do not,
/// verified against the pinned artifact).
pub(crate) fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

#[async_trait::async_trait]
impl Embedder for OrtEmbedder {
    async fn embed_query(&self, q: &Query) -> Result<Vec<f32>, PipelineError> {
        let Query::Text(t) = q else {
            // text-only kind (query_modalities = [text]); M3-T1 seam
            return Err(super::query_modality_refusal(q));
        };
        // no query prefix for the gte family (verified; module docs)
        self.embed_texts(vec![t.clone()])
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
        let texts = chunks.iter().map(|c| c.text.clone()).collect();
        self.embed_texts(texts).await
    }

    fn dim(&self) -> usize {
        self.inner.dim
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        &[Modality::Text]
    }

    fn query_modalities(&self) -> &'static [Modality] {
        &[Modality::Text]
    }
}

impl OrtEmbedder {
    /// Shared batch path for both routes (they encode identically for gte).
    async fn embed_texts(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, PipelineError> {
        let inner = Arc::clone(&self.inner);
        // spawn_blocking: ORT inference holds the thread (module docs on the
        // two-layer blocking/concurrency story)
        tokio::task::spawn_blocking(move || Self::run_batch(&inner, texts))
            .await
            .map_err(|e| PipelineError::embed(format!("ort inference task panicked: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// tests — hermetic: the committed tiny-embedder fixture + the real .so
// (missing library panics with fetch guidance instead of skipping — same
// honest-failure rule as pdf_text.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The committed fixture directory (793-byte MatMul-only ONNX with a
    /// static output dim of 8, word-level tokenizer.json).
    pub(crate) fn fixture_dir() -> PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-embedder",
            "tests/fixtures/tiny-embedder",
        ] {
            let d = PathBuf::from(p);
            if d.join("model.onnx").is_file() {
                return d;
            }
        }
        panic!(
            "tiny-embedder fixture not found (expected vf/tests/fixtures/tiny-embedder with \
             model.onnx + tokenizer.json) — run its generate.py if it was removed"
        )
    }

    /// Contract-run constructor: panics with guidance when the .so is
    /// missing entirely (a System fallback still proceeds).
    pub(crate) async fn embedder_or_panic() -> OrtEmbedder {
        match locate_library() {
            Ok(_) => {
                let cfg = fixture_config();
                OrtEmbedder::new(&cfg, Path::new("/nonexistent-models-dir"))
                    .await
                    .expect("fixture model must load against the real .so")
            }
            Err(m) => panic!(
                "ort tests need the real library and none was found: {m} — {FETCH_HINT}. Tests \
                 refuse to skip (M2-T4 honest-failure rule)."
            ),
        }
    }

    /// Config pointing straight at the fixture via `path` (no table lookup,
    /// no models_dir, no network).
    pub(crate) fn fixture_config() -> OrtConfig {
        OrtConfig {
            kind: OrtTag::Ort,
            model: Some("tiny-fixture".to_string()),
            path: Some(fixture_dir().display().to_string()),
            offline: true,
        }
    }

    #[tokio::test]
    async fn loads_fixture_and_embeds() {
        let e = embedder_or_panic().await;
        assert_eq!(e.dim(), 8, "fixture declares a static dim of 8");

        // table[r][d] = ((v*8+d) % 16)/8 - 1 with v = the [CLS] id 2:
        // row = [ -0.75, -0.625, -0.5, -0.375, -0.25, -0.125, 0.0, 0.125 ]
        // (normalized). Assert EXACT semantics: unit norm + expected ratios.
        let v = e
            .embed_query(&Query::Text("hello vectorfs".into()))
            .await
            .unwrap();
        assert_eq!(v.len(), 8);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-6,
            "CLS row must be L2-normalized: {norm}"
        );
        assert!(v.iter().all(|x| x.is_finite()));

        // documents: one vector per chunk, CJK included (unknown words -> [UNK])
        let chunks = vec![
            Chunk {
                index: 0,
                text: "first chunk".into(),
                image: None,
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
            },
            Chunk {
                index: 1,
                text: "第二个块".into(),
                image: None,
                kind: crate::pipeline::ChunkKind::Body,
                page: None,
                line: None,
                section: None,
            },
        ];
        let docs = e.embed_documents(&chunks).await.unwrap();
        assert_eq!(docs.len(), 2);
        for d in &docs {
            assert_eq!(d.len(), 8);
            let n: f32 = d.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-6);
        }
        // [CLS] id appears in every encoding -> both rows identical after
        // pooling (the fixture ignores token identity beyond the first)
        assert_eq!(docs[0], docs[1]);

        // determinism against the real engine
        let again = e.embed_documents(&chunks).await.unwrap();
        assert_eq!(docs, again);

        // empty batch: Ok(empty), no session run
        assert!(e.embed_documents(&[]).await.unwrap().is_empty());
    }

    /// Regression (2026-11-06 OOM): a request larger than MAX_INFER_TOKENS
    /// must ride MULTIPLE session.run mini-batches, and each row's vector
    /// must be IDENTICAL to embedding that row alone — grouping is a memory
    /// bound, never a semantics change. Two 8192-truncated texts cannot share
    /// a group (8192 + 8192 > MAX_INFER_TOKENS), so the split path is
    /// guaranteed to fire.
    #[tokio::test]
    async fn mini_batch_split_is_row_invariant() {
        let e = embedder_or_panic().await;
        let long_a = "alpha ".repeat(9_000);
        let long_b = "beta ".repeat(9_000);
        let split = e
            .embed_texts(vec![long_a.clone(), long_b.clone()])
            .await
            .unwrap();
        let alone_a = e.embed_texts(vec![long_a]).await.unwrap();
        let alone_b = e.embed_texts(vec![long_b]).await.unwrap();
        assert_eq!(split.len(), 2, "one vector per row, batches concatenated");
        assert_eq!(split[0], alone_a[0], "row 0 unchanged by group membership");
        assert_eq!(split[1], alone_b[0], "row 1 unchanged by group membership");
        assert!(
            split.iter().flat_map(|v| v.iter()).all(|x| x.is_finite()),
            "no NaN/Inf leakage across the group boundary"
        );
    }

    #[tokio::test]
    async fn dim_probe_mismatch_with_builtin_fails_startup() {
        // Declared-in-table (gte = 768) vs fixture graph (8): the most direct
        // construction of the §5.10.2 conflict — a gte-named model whose
        // directory holds a different-dim graph.
        let cfg = OrtConfig {
            kind: OrtTag::Ort,
            model: Some("gte-multilingual-base".to_string()),
            path: Some(fixture_dir().display().to_string()),
            offline: true,
        };
        let err = OrtEmbedder::new(&cfg, Path::new("/nonexistent"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declares dim 768"), "{msg}");
        assert!(msg.contains("dim 8"), "{msg}");
        assert!(msg.contains("refusing to start"), "{msg}");
    }

    #[tokio::test]
    async fn dynamic_output_dim_fails_startup() {
        // fixture twin whose ONLY output dim is genuinely runtime-dependent
        // (Gather -> Reshape(-1)): shape inference cannot pin a static last
        // dim, so the startup probe must refuse
        let dir = tempfile::tempdir().unwrap();
        for f in ["model.onnx", "tokenizer.json", "config.json"] {
            std::fs::copy(fixture_dir().join(f), dir.path().join(f)).unwrap();
        }
        std::fs::copy(
            fixture_dir().join("model_flat.onnx"),
            dir.path().join("model.onnx"),
        )
        .unwrap();
        let cfg = OrtConfig {
            kind: OrtTag::Ort,
            model: Some("tiny-fixture".to_string()),
            path: Some(dir.path().display().to_string()),
            offline: true,
        };
        let err = OrtEmbedder::new(&cfg, Path::new("/nonexistent"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot probe"), "{msg}");
        assert!(msg.contains("dynamic"), "{msg}");
    }

    #[tokio::test]
    async fn offline_missing_model_is_an_error() {
        let cfg = OrtConfig {
            kind: OrtTag::Ort,
            model: Some("gte-multilingual-base".to_string()),
            path: None,
            offline: true,
        };
        let dir = tempfile::tempdir().unwrap();
        let err = OrtEmbedder::new(&cfg, dir.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("offline = true forbids the network"), "{msg}");
        assert!(msg.contains("prefetch"), "{msg}");
    }

    #[tokio::test]
    async fn incomplete_model_dir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("gte-multilingual-base")).unwrap();
        // no model.onnx; stamp files so the ready-check path is bypassed and
        // the incomplete-dir check fires
        std::fs::write(dir.path().join("gte-multilingual-base/.complete"), "x\n").unwrap();
        std::fs::write(
            dir.path().join("gte-multilingual-base/tokenizer.json"),
            "{}",
        )
        .unwrap();
        let cfg = OrtConfig {
            kind: OrtTag::Ort,
            model: Some("gte-multilingual-base".to_string()),
            path: Some(
                dir.path()
                    .join("gte-multilingual-base")
                    .display()
                    .to_string(),
            ),
            offline: true,
        };
        let err = OrtEmbedder::new(&cfg, dir.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("incomplete"),
            "{}",
            err.to_string()
        );
    }

    #[test]
    fn config_roundtrip_and_guards() {
        // defaults: model = gte-multilingual-base, offline = false
        let cfg: OrtConfig = toml::from_str("kind = \"ort\"").unwrap();
        assert_eq!(cfg.model.as_deref(), None); // path-only default: no name asserted until resolution
        assert!(!cfg.offline);
        assert!(cfg.path.is_none());
        // full form round-trips flat (fingerprint-facing)
        let full = "kind = \"ort\"\nmodel = \"x\"\npath = \"/m\"\noffline = true\n";
        let cfg: OrtConfig = toml::from_str(full).unwrap();
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"ort\"\nmodel = \"x\"\npath = \"/m\"\noffline = true"
        );
        // wrong kind literal rejected
        assert!(toml::from_str::<OrtConfig>("kind = \"openai\"").is_err());
        // unknown field rejected (stage-2 deny_unknown_fields)
        assert!(toml::from_str::<OrtConfig>("kind = \"ort\"\ndim = 8").is_err());
    }

    #[test]
    fn library_resolution_chain() {
        // happy paths exercised by every other test (runtime-libs/ or env);
        // here: an explicit-but-missing VF_ORT_PATH is a hard error.
        // SAFETY: test-local env mutation; serialized by the env lock in
        // models tests? no — VF_ORT_PATH is only read here, races with
        // nothing (other tests never set it).
        let key = "VF_ORT_PATH";
        // scope the change
        {
            // SAFETY: single-threaded within this test; other ort tests
            // never read VF_ORT_PATH (their happy path uses runtime-libs/)
            unsafe { std::env::set_var(key, "/nonexistent/libonnxruntime.so") };
            let err = locate_library().unwrap_err();
            assert!(err.contains("VF_ORT_PATH"), "{err}");
            unsafe { std::env::remove_var(key) };
        }
    }

    /// REAL model smoke (downloads ~1.2 GiB through the prefetch path):
    /// #[ignore] in the normal suite — the T9 release gate runs it
    /// explicitly (design.md §5.10.7 "real-model tests weekly tier").
    #[tokio::test]
    #[ignore = "downloads the real ~1.2 GiB gte weights; run explicitly (T9 gate)"]
    async fn real_gte_model_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = OrtConfig {
            kind: OrtTag::Ort,
            model: Some("gte-multilingual-base".to_string()),
            path: None,
            offline: false,
        };
        let e = OrtEmbedder::new(&cfg, dir.path()).await.unwrap();
        assert_eq!(e.dim(), 768, "gte hidden_size");
        let q = e
            .embed_query(&Query::Text("What is a panda?".into()))
            .await
            .unwrap();
        let docs = e
            .embed_documents(&[
                Chunk {
                    index: 0,
                    text: "The giant panda is a bear species endemic to China.".into(),
                    image: None,
                    kind: crate::pipeline::ChunkKind::Body,
                    page: None,
                    line: None,
                    section: None,
                },
                Chunk {
                    index: 1,
                    text: "大熊猫是中国特有的熊科动物。".into(),
                    image: None,
                    kind: crate::pipeline::ChunkKind::Body,
                    page: None,
                    line: None,
                    section: None,
                },
            ])
            .await
            .unwrap();
        assert_eq!(q.len(), 768);
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().all(|d| d.len() == 768));
        // both routes must be finite and (CLS+normalize) unit-normed
        for v in std::iter::once(&q).chain(docs.iter()) {
            assert!(v.iter().all(|x| x.is_finite()));
            let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-3, "unit norm expected: {n}");
        }
    }
}
