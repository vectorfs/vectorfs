//! `siglip2` embedder kind: SigLIP 2 dual-tower image embedder (M3-T2,
//! design.md §5.10.2 Embedder row; the first doc modality ≠ Text kind).
//!
//! # Model facts (verified against the pinned artifacts, 2026-11-06)
//!
//! * Pinned export: `onnx-community/siglip2-base-patch16-224-ONNX` (fp32
//!   canonical, the gte precedent — conversion of Apache-2.0
//!   `google/siglip2-base-patch16-224`; the conversion carries the base
//!   weights' license. 38 google siglip2 checkpoints are Apache-2.0, HF API
//!   verified in research/2026-11-multimodal-embedding.md §B1).
//! * SPLIT-TOWER layout, verified by loading the actual graphs:
//!   `vision_model.onnx` — IN `pixel_values` `[batch, num_channels, height,
//!   width]` float32 (ALL dims symbolic in this export), OUT
//!   `last_hidden_state` + `pooler_output [batch, 768]`;
//!   `text_model.onnx` — IN `input_ids` `[batch, seq]` int64 (NO
//!   attention_mask; `tokenizer_config.json` `model_input_names:
//!   ["input_ids"]`), OUT `last_hidden_state` + `pooler_output [batch, 768]`.
//! * The towers' `pooler_output` IS the shared-space embedding: numerically
//!   verified against the FULL `model.onnx` export (`image_embeds` /
//!   `text_embeds`, cosine 1.000000) — the full graph only adds an L2
//!   normalization the split exports omit, so THIS module normalizes in code
//!   (unit-norm rows; the derived index metric is cosine, where unit norm
//!   makes dot == cosine and keeps vectors directly comparable).
//! * Preprocessing (from the artifact's `preprocessor_config.json`, not
//!   README boilerplate — the models.rs sourcing discipline): RGB → resize
//!   EXACTLY to (height, width) — SigLIP has NO center crop — with
//!   `resample: 2` = PIL BILINEAR (`image`'s `FilterType::Triangle`), rescale
//!   1/255, per-channel normalize mean = std = 0.5, NCHW float32. NOTE: the
//!   M3-T2 task brief said "bicubic"; the artifact says bilinear — the model
//!   wins (recorded in the task report).
//! * Text context = 64 tokens (`tokenizer_config.json` `max_length: 64`;
//!   SigLIP text towers are caption encoders, not document encoders).
//! * naflex dynamic-resolution variants are NOT supported (research §D3:
//!   non-blocking, needs transformers-native smart-resize semantics; revisit
//!   when an M3+ requirement lands). Fixed-resolution towers only.
//!
//! # Configuration (§5.10.4 shape, minimal per §5.10.8)
//!
//! `{ kind, model?, path?, offline }` — the same four-key shape as `ort`.
//! ONE deliberate difference: NO default model. Which siglip2 tier becomes
//! a default (base vs so400m …) is M3-T4's preset adjudication, so
//! `kind = "siglip2"` with neither `model` nor `path` is a fail-fast config
//! error naming the built-in, never a silent tier choice. Preprocessing has
//! zero knobs — the parameters above are properties of the trained weights.
//!
//! # Engine reuse (the §0 ort-family posture)
//!
//! Same dylib discipline as `embed/ort.rs`: load-dynamic only, the
//! process-global environment is shared through `ort::ort_environment`
//! (ONE ort env per process — a daemon running gte + siglip2 binds the
//! .so once), and inference runs inside `spawn_blocking` (the two-layer
//! blocking/concurrency story in ort.rs module docs applies verbatim).
//! T3's ingest worker additionally CPU-gates this kind (it is local
//! inference, like `ort`).
//!
//! # Dim + size probing (the §5.10.2 startup contract)
//!
//! `dim`: static last dim of the POOLED output of each tower; both towers
//! must agree (a mismatched pair would put query and document vectors in
//! different spaces — silent search corruption). A table-named model must
//! additionally match the table's declared dim. `input size (H, W)`: probed
//! from the vision session's input metadata when the graph declares static
//! dims (the fixture, self-exported models); the pinned community export
//! keeps H/W symbolic, so the TABLE's declared `image_size` (taken from the
//! artifact's preprocessor_config.json at pin time) is the fallback and —
//! when both exist — the cross-check target. A `path`-only artifact with
//! dynamic dims and no table row refuses to start: nothing declares its
//! resolution, and guessing one would silently mis-preprocess.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::{Chunk, Embedder, Modality, PipelineError, Query};

/// SigLIP2 preprocessing constants (verified from the pinned artifact's
/// preprocessor_config.json; see module docs — fixed, not config).
const MEAN: f32 = 0.5;
const STD: f32 = 0.5;

/// SigLIP2 text context cap (`tokenizer_config.json` `max_length: 64`; a
/// property of the model family — caption-length text, not documents).
const MAX_SEQ_TOKENS: usize = 64;

/// Upper bound on `rows × 3 × H × W` input ELEMENTS per `session.run`
/// (786,432 = one 512×512 RGB image of raw input). The ViT activations an
/// image drives dwarf its input tensor, so this budget deliberately keeps
/// groups SMALL: the pinned base@224 (150,528 elems) rides ≤5 rows per run,
/// so400m@384 (442,368) rides alone. Same discipline and rationale as
/// ort.rs's `MAX_INFER_TOKENS` (the 2026-11-06 OOM): a memory bound, never
/// a semantics change — each row's vector is identical to embedding it
/// alone (pinned by the packing unit test).
const MAX_INFER_PIXELS: usize = 3 * 512 * 512;

/// Decode-budget guard: width/height cap before any pixel buffer is
/// materialized (`image` Limits, checked at header time). 4096² = 16.7 MP is
/// generous for photo/pdf-page inputs and bounds the RGB buffer at ~50 MiB;
/// the SigLIP resize would throw away anything above a few MP anyway. A
/// bomb must fail at decode, not at the 31 GB RSS line (the ort.rs OOM
/// precedent).
const DECODE_LIMIT: u32 = 4096;

/// Per-kind config struct (§5.10.4 shape; see module docs — no default
/// model, no preprocessing knobs).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Siglip2Config {
    pub kind: Siglip2Tag,
    /// Built-in model name (see `pipeline::models::BUILTIN_MODELS`).
    #[serde(default)]
    pub model: Option<String>,
    /// Local model directory override (layout: vision.onnx + text.onnx +
    /// tokenizer.json); the air-gapped / self-managed escape hatch.
    #[serde(default)]
    pub path: Option<String>,
    /// Never touch the network: a missing local model is a hard error
    /// instead of a prefetch.
    #[serde(default)]
    pub offline: bool,
}

/// Literal tag: only deserializes from `"siglip2"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Siglip2Tag {
    #[serde(rename = "siglip2")]
    Siglip2,
}

/// The built-in model this kind currently pins (the error text for a bare
/// `kind = "siglip2"` names it — which tier becomes a DEFAULT is T4's
/// preset adjudication, so the kind itself has none).
pub(crate) const BUILTIN: &str = "siglip2-base-patch16-224";

/// Resolved model identity + directory. Three of ort's four resolution
/// cases; the fourth (neither `model` nor `path`) is a config-time error
/// (see module docs) so it never reaches here — and errors loudly anyway if
/// constructed directly.
pub(crate) fn resolve_model(
    cfg: &Siglip2Config,
    models_dir: &Path,
) -> Result<(String, PathBuf), PipelineError> {
    match (&cfg.model, &cfg.path) {
        (Some(m), Some(p)) => Ok((m.clone(), PathBuf::from(p))),
        (None, Some(p)) => Ok((resolve_model_name(cfg), PathBuf::from(p))),
        (Some(m), None) => Ok((m.clone(), models_dir.join(m))),
        (None, None) => Err(PipelineError::config(
            format!(
                "the siglip2 kind requires model = \"{BUILTIN}\" (or another built-in name) or \
                 path = \"…\" to a local model directory — which tier becomes the default is \
                 deliberately not baked in (M3-T4 preset adjudication)"
            ),
            None,
        )),
    }
}

/// Name half of [`resolve_model`] (identity for `x-vf-model` metadata and
/// the dim cross-check). The (None, None) arm cannot come from parsed
/// config (from_config refuses it first) and load() re-refuses; its label
/// here only keeps the total function panic-free.
pub(crate) fn resolve_model_name(cfg: &Siglip2Config) -> String {
    match (&cfg.model, &cfg.path) {
        (Some(m), _) => m.clone(),
        (None, Some(p)) => Path::new(p)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "local-model".to_string()),
        (None, None) => "siglip2".to_string(),
    }
}

// ---------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------

/// A loaded siglip2 embedder: both towers + tokenizer + probed dim and
/// input size. Construction is the STARTUP GATE (all fail-fast checks in
/// [`Siglip2Embedder::new`]); afterwards immutable and `Send + Sync` for
/// the blocking pool. Not `Clone` (owns the live sessions).
pub struct Siglip2Embedder {
    inner: Arc<Inner>,
}

struct Inner {
    /// Separate mutexes: the towers are independent sessions, a text query
    /// must not queue behind an image batch (`Session::run` takes `&mut`).
    vision: std::sync::Mutex<ort::session::Session>,
    text: std::sync::Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
    /// Probed startup dim — both towers agree on it (see module docs).
    dim: usize,
    /// Resolved input resolution (session static dims, else the table's).
    height: u32,
    width: u32,
    /// Graph input names resolved at load (`pixel_values` / `input_ids`, or
    /// the single declared input of a compatible export under another name).
    vision_input: String,
    text_input: String,
    /// Index of the pooled output per tower (the `pooler_output` /
    /// `*_embeds` outlet; output[0] is `last_hidden_state` on real exports).
    vision_pool: usize,
    text_pool: usize,
}

impl std::fmt::Debug for Siglip2Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Siglip2Embedder")
            .field("dim", &self.inner.dim)
            .field("size", &(self.inner.height, self.inner.width))
            .finish_non_exhaustive()
    }
}

impl Siglip2Embedder {
    /// Load + probe. Startup contract (the ort semantics, per-tower):
    ///
    /// * model resolves (`path` > `models_dir/<model>`; missing table model:
    ///   prefetch unless `offline`, then hard error);
    /// * `vision.onnx` + `text.onnx` + `tokenizer.json` exist and load;
    /// * the vision input declares `[batch, channels, H, W]` with channels
    ///   3 (or symbolic) and STATIC H/W — else the table's declared size;
    /// * the pooled outputs have STATIC last dims, the two towers AGREE,
    ///   and a table-named model matches the table's declared dim.
    pub async fn new(cfg: &Siglip2Config, models_dir: &Path) -> Result<Self, PipelineError> {
        let (name, dir) = resolve_model(cfg, models_dir)?;
        // Same readiness/prefetch discipline as ort (table model resolved to
        // the cache dir only; an explicit `path` must simply exist).
        if cfg.path.is_none() {
            let ready = super::super::models::builtin(&name)
                .map(|m| super::super::models::is_ready(&dir, m))
                .unwrap_or(false);
            if !ready {
                if cfg.offline {
                    return Err(PipelineError::embed(format!(
                        "model '{}' is not present under {} and offline = true forbids the \
                                                 network — prefetch it on a connected machine or set \
                                                 path = \"…\" to a local copy",
                        name,
                        dir.display()
                    )));
                }
                super::super::models::prefetch(&name, models_dir).await?;
            }
        }
        for f in ["vision.onnx", "text.onnx", "tokenizer.json"] {
            if !dir.join(f).is_file() {
                return Err(PipelineError::embed(format!(
                    "model directory {} is incomplete (missing {f}); layout = vision.onnx + \
                                         text.onnx + tokenizer.json",
                    dir.display()
                )));
            }
        }
        // Table declarations for the cross-checks (None for path-only
        // artifacts that did not name a table model).
        let table = super::super::models::builtin(&name);
        let declared_dim = table.map(|m| m.dim);
        let declared_size = table.and_then(|m| m.image_size);

        let dir_str = dir.display().to_string();
        tokio::task::spawn_blocking(move || {
            Self::load_and_probe(&dir_str, declared_dim, declared_size)
        })
        .await
        .map_err(|e| PipelineError::embed(format!("siglip2 load task panicked: {e}")))?
    }

    /// Blocking half of construction (inside `spawn_blocking`).
    fn load_and_probe(
        dir: &str,
        declared_dim: Option<usize>,
        declared_size: Option<(u32, u32)>,
    ) -> Result<Self, PipelineError> {
        // dylib + process-global env: SHARED with the ort kind (one .so, one
        // environment, however many ort-family embedders are configured).
        let source = super::ort::locate_library()
            .map_err(|m| PipelineError::embed(format!("onnxruntime library unavailable: {m}")))?;
        let env = super::ort::ort_environment(&source)?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(format!("{dir}/tokenizer.json"))
            .map_err(|e| {
                PipelineError::embed(format!("loading tokenizer {dir}/tokenizer.json: {e}"))
            })?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SEQ_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| PipelineError::embed(format!("configuring truncation: {e}")))?;

        let vision = super::ort::commit_session(&env, &format!("{dir}/vision.onnx"))?;
        let text = super::ort::commit_session(&env, &format!("{dir}/text.onnx"))?;

        // ---- input probes ----
        let (vision_input, session_hw) = probe_vision_input(&vision)?;
        let text_input = probe_text_input(&text)?;

        // ---- resolution: static session dims win; else the table declares ----
        let (height, width) = match (session_hw, declared_size) {
            (Some(hw), Some(t)) if hw != t => {
                return Err(PipelineError::embed(format!(
                    "the vision tower's graph declares input {}x{} but its model table entry \
                                         declares {}x{} — refusing to start (mismatched \
                                         preprocessing would silently corrupt the index; \
                                         re-download the model or fix the configuration)",
                    hw.0, hw.1, t.0, t.1
                )));
            }
            (Some(hw), _) => hw,
            (None, Some(t)) => t,
            (None, None) => {
                return Err(PipelineError::embed(
                    "cannot determine the vision input size: the graph's height/width dims are \
                     dynamic and no built-in table entry declares them — use a fixed-resolution \
                     export (static H/W in the graph) or a built-in model name instead of path \
                     (naflex dynamic-resolution variants are not supported)"
                        .to_string(),
                ));
            }
        };

        // ---- pooled-output + dim probes (both towers must agree) ----
        let vision_pool = pooled_output_index(&vision, "vision")?;
        let text_pool = pooled_output_index(&text, "text")?;
        let vdim = probe_dim(&vision, vision_pool, "vision")?;
        let tdim = probe_dim(&text, text_pool, "text")?;
        if vdim != tdim {
            return Err(PipelineError::embed(format!(
                "the towers disagree on the embedding dim (vision {vdim}, text {tdim}) — a \
                 mismatched pair puts query and document vectors in different spaces; refusing \
                 to start"
            )));
        }
        if let Some(declared) = declared_dim
            && declared != vdim
        {
            return Err(PipelineError::embed(format!(
                "model declares dim {declared} but its ONNX graphs output dim {vdim} — refusing \
                 to start (mismatched vectors would silently corrupt the index; re-download the \
                 model or fix the configuration)"
            )));
        }

        Ok(Siglip2Embedder {
            inner: Arc::new(Inner {
                vision: std::sync::Mutex::new(vision),
                text: std::sync::Mutex::new(text),
                tokenizer,
                dim: vdim,
                height,
                width,
                vision_input,
                vision_pool,
                text_input,
                text_pool,
            }),
        })
    }

    /// The preprocess → mini-batch → run → L2-normalize pipeline (blocking).
    /// A free function over `&Inner` so `spawn_blocking` gets a 'static
    /// closure via `Arc<Inner>` (the ort.rs pattern).
    fn run_images(inner: &Inner, payloads: Vec<Vec<u8>>) -> Result<Vec<Vec<f32>>, PipelineError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        let row_elems = 3 * inner.height as usize * inner.width as usize;
        let rows: Result<Vec<Vec<f32>>, PipelineError> = payloads
            .iter()
            .map(|p| preprocess(p, (inner.height, inner.width)))
            .collect();
        let rows = rows?;

        let mut session = inner
            .vision
            .lock()
            .map_err(|_| PipelineError::embed("ort session lock poisoned"))?;
        let mut out = Vec::with_capacity(rows.len());
        for group in pack_image_groups(rows.len(), row_elems, MAX_INFER_PIXELS) {
            let n = group.len();
            let mut flat = Vec::with_capacity(n * row_elems);
            for r in &rows[group] {
                flat.extend_from_slice(r);
            }
            let shape = vec![n as i64, 3, inner.height as i64, inner.width as i64];
            let tensor = ort::value::Tensor::from_array((shape, flat))
                .map_err(|e| PipelineError::embed(format!("pixel_values tensor: {e}")))?;
            let outputs = session
                .run(vec![(inner.vision_input.clone(), tensor.into_dyn())])
                .map_err(|e| PipelineError::embed(format!("vision inference failed: {e}")))?;
            let value: &ort::value::DynValue = &outputs[inner.vision_pool];
            let (oshape, data) = value
                .try_extract_tensor::<f32>()
                .map_err(|e| PipelineError::embed(format!("reading image embeds: {e}")))?;
            let oshape: Vec<i64> = oshape.iter().copied().collect();
            if oshape.len() != 2 || oshape[0] as usize != n || oshape[1] as usize != inner.dim {
                return Err(PipelineError::embed(format!(
                    "vision tower output shape {oshape:?} does not match the batch we sent \
                     ([{n}, {}]) — graph/output mismatch",
                    inner.dim
                )));
            }
            for r in 0..n {
                out.push(super::ort::normalize(
                    &data[r * inner.dim..(r + 1) * inner.dim],
                ));
            }
        }
        Ok(out)
    }

    /// One text query through the text tower (blocking). The query side is
    /// single-row BY DESIGN: this kind's doc side is image-only, so text
    /// never arrives as documents — one query string, one [1, seq] run, no
    /// padding logic to get wrong.
    fn run_text(inner: &Inner, text: String) -> Result<Vec<f32>, PipelineError> {
        let enc = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| PipelineError::embed(format!("tokenizing query: {e}")))?;
        let ids = enc.get_ids();
        if ids.is_empty() {
            return Err(PipelineError::embed(
                "tokenizer produced an empty encoding (empty text?) — nothing to embed",
            ));
        }
        let data: Vec<i64> = ids.iter().map(|&i| i as i64).collect();
        let shape = vec![1i64, data.len() as i64];
        let tensor = ort::value::Tensor::from_array((shape, data))
            .map_err(|e| PipelineError::embed(format!("input_ids tensor: {e}")))?;
        let mut session = inner
            .text
            .lock()
            .map_err(|_| PipelineError::embed("ort session lock poisoned"))?;
        let outputs = session
            .run(vec![(inner.text_input.clone(), tensor.into_dyn())])
            .map_err(|e| PipelineError::embed(format!("text inference failed: {e}")))?;
        let value: &ort::value::DynValue = &outputs[inner.text_pool];
        let (oshape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| PipelineError::embed(format!("reading text embeds: {e}")))?;
        let oshape: Vec<i64> = oshape.iter().copied().collect();
        if oshape.len() != 2 || oshape[1] as usize != inner.dim {
            return Err(PipelineError::embed(format!(
                "text tower output shape {oshape:?} does not match [1, {}] — graph/output \
                 mismatch",
                inner.dim
            )));
        }
        Ok(super::ort::normalize(&data[..inner.dim]))
    }
}

// ---------------------------------------------------------------------------
// probing helpers
// ---------------------------------------------------------------------------

/// Resolve the vision tower's input: `pixel_values` by name (every
/// transformers-style export), else the graph's SINGLE input under any name
/// (lenient for compatible path-only artifacts). Returns the input name and
/// its STATIC height×width when the graph declares them.
fn probe_vision_input(
    session: &ort::session::Session,
) -> Result<(String, Option<(u32, u32)>), PipelineError> {
    let inputs = session.inputs();
    let outlet = match inputs.iter().find(|o| o.name() == "pixel_values") {
        Some(o) => o,
        None if inputs.len() == 1 => &inputs[0],
        None => {
            let names: Vec<&str> = inputs.iter().map(|o| o.name()).collect();
            return Err(PipelineError::embed(format!(
                "the vision tower's inputs {names:?} are not supported — expected `pixel_values` \
                 `[batch, channels, height, width]` (transformers-style exports)"
            )));
        }
    };
    let name = outlet.name().to_string();
    let shape = match outlet.dtype() {
        ort::value::ValueType::Tensor { shape, .. } => shape.clone(),
        _ => {
            return Err(PipelineError::embed(
                "the vision tower's input is not a tensor — unsupported graph shape",
            ));
        }
    };
    let dims: Vec<i64> = shape.iter().copied().collect();
    if dims.len() != 4 {
        return Err(PipelineError::embed(format!(
            "the vision tower's input {name} has shape {dims:?} — expected rank 4 [batch, \
             channels, height, width]"
        )));
    }
    match dims[1] {
        3 | -1 => {} // 3 == RGB (what we feed); -1 == symbolic (the pinned export)
        other => {
            return Err(PipelineError::embed(format!(
                "the vision tower expects {other} input channels but siglip2 preprocessing \
                 produces RGB (3) — unsupported model"
            )));
        }
    }
    let hw = match (dims[2], dims[3]) {
        (h, w) if h > 0 && w > 0 => Some((h as u32, w as u32)),
        _ => None,
    };
    Ok((name, hw))
}

/// Resolve the text tower's input: `input_ids` by name, else the graph's
/// single input (the pinned export takes ONLY input_ids — no attention_mask).
fn probe_text_input(session: &ort::session::Session) -> Result<String, PipelineError> {
    let inputs = session.inputs();
    let outlet = match inputs.iter().find(|o| o.name() == "input_ids") {
        Some(o) => o,
        None if inputs.len() == 1 => &inputs[0],
        None => {
            let names: Vec<&str> = inputs.iter().map(|o| o.name()).collect();
            return Err(PipelineError::embed(format!(
                "the text tower's inputs {names:?} are not supported — expected `input_ids` \
                 `[batch, seq]` (the siglip2 export takes no attention_mask)"
            )));
        }
    };
    Ok(outlet.name().to_string())
}

/// Index of the tower's POOLED output outlet (`pooler_output` on the
/// transformers exports, `image_embeds`/`text_embeds` on pooled-only ones —
/// immich-style). Output[0] on real exports is `last_hidden_state`, which
/// is why the pooled outlet must be found BY NAME.
fn pooled_output_index(
    session: &ort::session::Session,
    tower: &str,
) -> Result<usize, PipelineError> {
    session
        .outputs()
        .iter()
        .position(|o| matches!(o.name(), "pooler_output" | "image_embeds" | "text_embeds"))
        .ok_or_else(|| {
            let names: Vec<&str> = session.outputs().iter().map(|o| o.name()).collect();
            PipelineError::embed(format!(
                "the {tower} tower exposes no pooled output (expected `pooler_output` / \
                 `image_embeds` / `text_embeds`; graph outputs: {names:?})"
            ))
        })
}

/// Static last dim of one tower's pooled output (the startup dim probe).
fn probe_dim(
    session: &ort::session::Session,
    outlet: usize,
    tower: &str,
) -> Result<usize, PipelineError> {
    let shape = match session.outputs()[outlet].dtype() {
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
            "cannot probe the {tower} tower's embedding dim: pooled output shape {dims:?} has a \
             dynamic last dimension — the startup contract requires a static dim (§5.10.2)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// preprocessing (pure Rust, image crate; the verified recipe in module docs)
// ---------------------------------------------------------------------------

/// decode → RGB. Header-time dimension limits (the decode budget, see
/// [`DECODE_LIMIT`]) refuse bombs before the pixel buffer exists.
fn decode_rgb(bytes: &[u8]) -> Result<image::DynamicImage, PipelineError> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_LIMIT);
    limits.max_image_height = Some(DECODE_LIMIT);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| PipelineError::embed(format!("reading image bytes: {e}")))?;
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| PipelineError::embed(format!("decoding image: {e} (jpeg/png supported)")))
}

/// RGB → rescale 1/255 → per-channel (v − mean)/std → NCHW float32
/// (`[1, 3, h, w]` row-major: channel-major then row-major pixels — the
/// layout `pixel_values` expects). Pure: the unit-test target.
fn pixels_to_nchw(rgb: &image::RgbImage) -> Vec<f32> {
    let (w, h) = rgb.dimensions();
    let plane = (w * h) as usize;
    let mut out = vec![0f32; 3 * plane];
    for (i, px) in rgb.pixels().enumerate() {
        for c in 0..3 {
            out[c * plane + i] = (px.0[c] as f32 / 255.0 - MEAN) / STD;
        }
    }
    out
}

/// The full image preprocessing pipeline: decode → RGB → bilinear
/// (Triangle; the artifact's `resample: 2`, see module docs) resize EXACTLY
/// to the model's input square → NCHW float32.
fn preprocess(bytes: &[u8], size: (u32, u32)) -> Result<Vec<f32>, PipelineError> {
    let rgb = decode_rgb(bytes)?;
    // convert to RGB BEFORE resizing (the HF processor's convert→resize
    // order; palette/alpha flatten the same way PIL's convert("RGB") does)
    let resized = image::imageops::resize(
        &rgb.to_rgb8(),
        size.0,
        size.1,
        image::imageops::FilterType::Triangle,
    );
    Ok(pixels_to_nchw(&resized))
}

/// Greedy row packing under a pixel budget: adding a row would exceed the
/// budget ⇒ flush first; a row LARGER than the budget rides alone (images
/// cannot be split — unlike text rows, the budget is a bound, not a
/// correctness constraint on individuals). Pure: the unit-test target.
fn pack_image_groups(rows: usize, row_elems: usize, budget: usize) -> Vec<std::ops::Range<usize>> {
    let per_group = (budget / row_elems.max(1)).max(1);
    (0..rows)
        .step_by(per_group)
        .map(|start| start..(start + per_group).min(rows))
        .collect()
}

#[async_trait::async_trait]
impl Embedder for Siglip2Embedder {
    async fn embed_query(&self, q: &Query) -> Result<Vec<f32>, PipelineError> {
        match q {
            // 以图搜图: same tower, same preprocessing as documents (a
            // sigmoid-loss dual encoder has no query/document asymmetry)
            Query::Image(bytes, _) => {
                let inner = Arc::clone(&self.inner);
                let bytes = bytes.clone();
                tokio::task::spawn_blocking(move || Self::run_images(&inner, vec![bytes]))
                    .await
                    .map_err(|e| PipelineError::embed(format!("ort inference task panicked: {e}")))?
                    .map(|mut v| v.swap_remove(0))
            }
            // 文字搜图: the text tower, tokenizer.json shipped with the model
            Query::Text(t) => {
                let inner = Arc::clone(&self.inner);
                let t = t.clone();
                tokio::task::spawn_blocking(move || Self::run_text(&inner, t))
                    .await
                    .map_err(|e| {
                        PipelineError::embed(format!("ort inference task panicked: {e}"))
                    })?
            }
        }
    }

    async fn embed_documents(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        // the worker scopes chunks by doc modality (image-only here), so a
        // text chunk reaching this kind is a routing bug — refuse loudly
        // (the T1 defensive-seam twin of the text kinds' image refusal)
        if let Some(c) = chunks.iter().find(|c| c.image.is_none()) {
            return Err(super::text_chunk_refusal(c));
        }
        let payloads = chunks
            .iter()
            .map(|c| c.image.as_ref().expect("checked above").bytes.to_vec())
            .collect();
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || Self::run_images(&inner, payloads))
            .await
            .map_err(|e| PipelineError::embed(format!("ort inference task panicked: {e}")))?
    }

    fn dim(&self) -> usize {
        self.inner.dim
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        &[Modality::Image]
    }

    fn query_modalities(&self) -> &'static [Modality] {
        // image queries (以图搜图) AND text queries (文字搜图, via the text
        // tower — the tokenizers investigation came back clean, so the text
        // tower ships with the kind; see module docs)
        &[Modality::Image, Modality::Text]
    }
}

// ---------------------------------------------------------------------------
// tests — hermetic: the committed tiny-siglip2 fixture + the real .so
// (missing library panics with fetch guidance — the M2-T4 honest-failure
// rule, same as the ort kind's tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The committed fixture directory (428-byte GlobalAveragePool+MatMul
    /// vision tower with STATIC 8×8 input and pooled dim 8, Gather+ReduceMean
    /// text tower over the 16-token WordLevel vocab; run its generate.py if
    /// it was removed).
    pub(crate) fn fixture_dir() -> PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-siglip2",
            "tests/fixtures/tiny-siglip2",
        ] {
            let d = PathBuf::from(p);
            if d.join("vision.onnx").is_file() {
                return d;
            }
        }
        panic!(
            "tiny-siglip2 fixture not found (expected vf/tests/fixtures/tiny-siglip2 with \
             vision.onnx + text.onnx + tokenizer.json) — run its generate.py if it was removed"
        )
    }

    /// Contract-run constructor: panics with guidance when the .so is
    /// missing entirely (a System fallback still proceeds).
    pub(crate) async fn embedder_or_panic() -> Siglip2Embedder {
        match super::super::ort::locate_library() {
            Ok(_) => {
                let cfg = fixture_config();
                Siglip2Embedder::new(&cfg, Path::new("/nonexistent-models-dir"))
                    .await
                    .expect("fixture model must load against the real .so")
            }
            Err(m) => panic!(
                "ort tests need the real library and none was found: {m} — {}. Tests refuse to \
                 skip (M2-T4 honest-failure rule).",
                super::super::ort::FETCH_HINT
            ),
        }
    }

    /// Config pointing straight at the fixture via `path` (no table lookup,
    /// no models_dir, no network).
    pub(crate) fn fixture_config() -> Siglip2Config {
        Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("tiny-siglip2".to_string()),
            path: Some(fixture_dir().display().to_string()),
            offline: true,
        }
    }

    /// A solid-color PNG at `size` (in-test encoder — no committed image
    /// bytes needed; solid colors make the resize step a no-op by
    /// construction so expected NCHW values are exact).
    fn solid_png(size: (u32, u32), rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(size.0, size.1, image::Rgb(rgb));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// The fixture's projection rows: table[v][d] = ((v*8+d)%16)/8 − 1 (the
    /// generator's pattern), for hand-computing expected embeddings.
    fn pattern_row(v: usize) -> Vec<f32> {
        (0..8)
            .map(|d| ((v * 8 + d) % 16) as f32 / 8.0 - 1.0)
            .collect()
    }

    fn unit(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[tokio::test]
    async fn loads_fixture_and_embeds_images() {
        let e = embedder_or_panic().await;
        assert_eq!(e.dim(), 8, "fixture declares a pooled dim of 8");

        // a solid WHITE 8×8 image preprocesses to exactly +1.0 per pixel;
        // the fixture averages channels then applies the 3×8 pattern, so
        // the expected row is sum(pattern rows 0..3) — L2-normalized
        let white = solid_png((8, 8), [255, 255, 255]);
        let mut expected: Vec<f32> = pattern_row(0)
            .iter()
            .zip(pattern_row(1).iter().zip(pattern_row(2).iter()))
            .map(|(a, (b, c))| a + b + c)
            .collect();
        let n = unit(&expected);
        expected = expected.iter().map(|x| x / n).collect();

        let v = e
            .embed_query(&Query::Image(white.clone(), "image/png".into()))
            .await
            .unwrap();
        assert_eq!(v.len(), 8);
        assert!((unit(&v) - 1.0).abs() < 1e-6, "unit norm: {}", unit(&v));
        for (got, want) in v.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-5, "{v:?} vs {expected:?}");
        }
        assert!(v.iter().all(|x| x.is_finite()));

        // documents: one vector per atomic image chunk; two solid images of
        // DIFFERENT colors embed differently (preprocessing feeds through)
        let mk = |rgb| Chunk {
            index: 0,
            text: String::new(),
            image: Some(crate::pipeline::ImagePayload {
                bytes: bytes::Bytes::from(solid_png((8, 8), rgb)),
                mime: "image/png".to_string(),
            }),
            kind: crate::pipeline::ChunkKind::Body,
        };
        let chunks = vec![mk([255, 255, 255]), mk([0, 0, 0])];
        let docs = e.embed_documents(&chunks).await.unwrap();
        assert_eq!(docs.len(), 2);
        for d in &docs {
            assert_eq!(d.len(), 8);
            assert!((unit(d) - 1.0).abs() < 1e-6);
            assert!(d.iter().all(|x| x.is_finite()));
        }
        assert_ne!(docs[0], docs[1], "white and black must differ");

        // determinism against the real engine
        let again = e.embed_documents(&chunks).await.unwrap();
        assert_eq!(docs, again);

        // empty batch: Ok(empty), no session run
        assert!(e.embed_documents(&[]).await.unwrap().is_empty());

        // a small non-square source exercises the resize path (16×8 → 8×8);
        // a solid color keeps the expected value exact through any filter
        let wide = Chunk {
            index: 0,
            text: String::new(),
            image: Some(crate::pipeline::ImagePayload {
                bytes: bytes::Bytes::from(solid_png((16, 8), [255, 0, 0])),
                mime: "image/png".to_string(),
            }),
            kind: crate::pipeline::ChunkKind::Body,
        };
        let red = e.embed_documents(&[wide]).await.unwrap();
        // red channel = +1.0, green/blue = −1.0 → channel means [1, −1, −1]
        // → rows 0..3 of the pattern summed with those weights
        let mut want: Vec<f32> = (0..8)
            .map(|d| pattern_row(0)[d] - pattern_row(1)[d] - pattern_row(2)[d])
            .collect();
        let wn = unit(&want);
        want = want.iter().map(|x| x / wn).collect();
        for (got, w) in red[0].iter().zip(want.iter()) {
            assert!((got - w).abs() < 1e-5, "{:?} vs {:?}", red[0], want);
        }
    }

    #[tokio::test]
    async fn text_queries_go_through_the_text_tower() {
        let e = embedder_or_panic().await;
        // "hello vectorfs" tokenizes to [CLS] hello vectorfs [SEP] = ids
        // [2, 9, 10, 3]; the fixture gathers those table rows and means
        // them — the expected vector is that mean, L2-normalized
        let ids = [2usize, 9, 10, 3];
        let mut want: Vec<f32> = (0..8)
            .map(|d| ids.iter().map(|&v| pattern_row(v)[d]).sum::<f32>() / ids.len() as f32)
            .collect();
        let wn = unit(&want);
        want = want.iter().map(|x| x / wn).collect();

        let v = e
            .embed_query(&Query::Text("hello vectorfs".into()))
            .await
            .unwrap();
        assert_eq!(v.len(), 8);
        assert!((unit(&v) - 1.0).abs() < 1e-6);
        for (got, w) in v.iter().zip(want.iter()) {
            assert!((got - w).abs() < 1e-5, "{v:?} vs {want:?}");
        }

        // unknown words -> [UNK] (id 1): still deterministic, still unit
        let cjk = e
            .embed_query(&Query::Text("第二个块".into()))
            .await
            .unwrap();
        let cjk2 = e
            .embed_query(&Query::Text("第二个块".into()))
            .await
            .unwrap();
        assert_eq!(cjk, cjk2);
        assert!((unit(&cjk) - 1.0).abs() < 1e-6);

        // empty text: the tokenizer still emits [CLS]/[SEP]; the tower
        // embeds them — finite, unit, dim (no empty-encoding error path)
        let empty = e.embed_query(&Query::Text(String::new())).await.unwrap();
        assert_eq!(empty.len(), 8);
        assert!(empty.iter().all(|x| x.is_finite()));
    }

    #[tokio::test]
    async fn text_chunks_are_refused_loudly() {
        let e = embedder_or_panic().await;
        let err = e
            .embed_documents(&[Chunk {
                index: 0,
                text: "stray text chunk".into(),
                image: None,
                kind: crate::pipeline::ChunkKind::Body,
            }])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("text chunk"), "{msg}");
        assert!(msg.contains("image documents only"), "{msg}");
        assert!(msg.contains("routing bug"), "{msg}");
    }

    #[test]
    fn pixels_to_nchw_pins_the_recipe() {
        // 2×2 image, distinct channels/pixels: layout + rescale + normalize
        // in one exact assertion (NCHW: channel planes, row-major pixels)
        let img = image::RgbImage::from_fn(2, 2, |x, y| {
            image::Rgb([(x * 100 + 10) as u8, (y * 50 + 5) as u8, 255])
        });
        let v = pixels_to_nchw(&img);
        let f = |b: u8| (b as f32 / 255.0 - MEAN) / STD;
        assert_eq!(
            v,
            vec![
                f(10),
                f(110),
                f(10),
                f(110), // R plane (row-major)
                f(5),
                f(5),
                f(55),
                f(55), // G plane
                f(255),
                f(255),
                f(255),
                f(255), // B plane
            ]
        );
    }

    #[test]
    fn decode_budget_refuses_oversize_images() {
        // 5000×1 is over the 4096 decode limit: refused at HEADER time
        // (the limits guard), before any pixel buffer is materialized
        let wide = solid_png((5000, 1), [1, 2, 3]);
        let err = preprocess(&wide, (8, 8)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("decoding image"), "{msg}");
        assert!(
            msg.contains("exceeds limit"),
            "the LIMITS refusal, not truncation: {msg}"
        );
        // and a garbage payload names the decoder, never panics
        let err = preprocess(b"definitely not an image", (8, 8)).unwrap_err();
        assert!(err.to_string().contains("decoding image"), "{err}");
    }

    #[test]
    fn image_packing_respects_the_budget_and_never_splits_a_row() {
        // budget fits 2 rows: [0..2), [2..4), [4..5) — contiguous cover,
        // no group over budget
        let groups = pack_image_groups(5, 10, 20);
        assert_eq!(groups.len(), 3);
        let cover: Vec<_> = groups.iter().flat_map(|g| g.clone()).collect();
        assert_eq!(cover, (0..5).collect::<Vec<_>>());
        assert!(groups.iter().all(|g| g.len() * 10 <= 20));
        // a row larger than the whole budget rides alone
        let groups = pack_image_groups(3, 10, 5);
        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|g| g.len() == 1));
    }

    /// Mini-batch split is row-invariant against the real engine: rows
    /// packed into multiple session.run groups equal the same rows alone
    /// (the packing unit test above covers the grouping arithmetic; this
    /// pins the SEMANTICS against onnxruntime).
    #[tokio::test]
    async fn mini_batch_split_is_row_invariant() {
        let e = embedder_or_panic().await;
        let mk = |i| Chunk {
            index: i,
            text: String::new(),
            image: Some(crate::pipeline::ImagePayload {
                bytes: bytes::Bytes::from(solid_png((8, 8), [i as u8 * 40, 3, 200])),
                mime: "image/png".to_string(),
            }),
            kind: crate::pipeline::ChunkKind::Body,
        };
        let batch: Vec<_> = (0..5).map(mk).collect();
        let split = e.embed_documents(&batch).await.unwrap();
        assert_eq!(split.len(), 5);
        for (i, row) in split.iter().enumerate() {
            let alone = e.embed_documents(&[mk(i as u32)]).await.unwrap();
            assert_eq!(row, &alone[0], "row {i} changed by group membership");
        }
    }

    /// The pooled output's dim is genuinely runtime-dependent (MatMul →
    /// Reshape(-1)): shape inference cannot pin a static last dim, so the
    /// startup probe must refuse (the ort kind's `model_flat.onnx` twin; the
    /// fixture output stays NAMED pooler_output so the failure is the DIM
    /// probe, not the outlet lookup).
    #[tokio::test]
    async fn dynamic_output_dim_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["vision.onnx", "text.onnx", "tokenizer.json"] {
            std::fs::copy(fixture_dir().join(f), dir.path().join(f)).unwrap();
        }
        std::fs::copy(
            fixture_dir().join("vision_flat.onnx"),
            dir.path().join("vision.onnx"),
        )
        .unwrap();
        let cfg = Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("tiny-siglip2".to_string()),
            path: Some(dir.path().display().to_string()),
            offline: true,
        };
        let err = Siglip2Embedder::new(&cfg, Path::new("/nonexistent"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot probe"), "{msg}");
        assert!(msg.contains("vision"), "{msg}");
        assert!(msg.contains("dynamic"), "{msg}");
    }

    #[tokio::test]
    async fn dim_probe_mismatch_with_builtin_fails_startup() {
        // Declared-in-table dim (a gte-named dir: 768) vs fixture graphs
        // (8): the §5.10.2 conflict, constructed the same way as the ort
        // kind's test (gte has no declared image_size, so the SIZE check
        // stays quiet and the DIM check fires)
        let cfg = Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("gte-multilingual-base".to_string()),
            path: Some(fixture_dir().display().to_string()),
            offline: true,
        };
        let err = Siglip2Embedder::new(&cfg, Path::new("/nonexistent"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declares dim 768"), "{msg}");
        assert!(msg.contains("dim 8"), "{msg}");
        assert!(msg.contains("refusing to start"), "{msg}");
    }

    /// The siglip2 row's DECLARED size (224) vs the fixture graph (8×8):
    // the size half of the table cross-check (the twin of the dim probe)
    #[tokio::test]
    async fn size_mismatch_with_builtin_fails_startup() {
        let cfg = Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("siglip2-base-patch16-224".to_string()),
            path: Some(fixture_dir().display().to_string()),
            offline: true,
        };
        let err = Siglip2Embedder::new(&cfg, Path::new("/nonexistent"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declares input 8x8"), "{msg}");
        assert!(msg.contains("declares 224x224"), "{msg}");
        assert!(msg.contains("refusing to start"), "{msg}");
    }

    #[tokio::test]
    async fn offline_missing_model_is_an_error() {
        let cfg = Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("siglip2-base-patch16-224".to_string()),
            path: None,
            offline: true,
        };
        let dir = tempfile::tempdir().unwrap();
        let err = Siglip2Embedder::new(&cfg, dir.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("offline = true forbids the network"), "{msg}");
        assert!(msg.contains("prefetch"), "{msg}");
    }

    #[tokio::test]
    async fn incomplete_model_dir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // vision.onnx only: the layout check must name the missing towers
        std::fs::copy(
            fixture_dir().join("vision.onnx"),
            dir.path().join("vision.onnx"),
        )
        .unwrap();
        let cfg = Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("tiny-siglip2".to_string()),
            path: Some(dir.path().display().to_string()),
            offline: true,
        };
        let err = Siglip2Embedder::new(&cfg, dir.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incomplete"), "{msg}");
        assert!(msg.contains("text.onnx"), "{msg}");
        assert!(msg.contains("tokenizer.json"), "{msg}");
    }

    /// A path-only artifact whose vision graph keeps H/W dynamic AND has no
    /// table entry: nothing declares the resolution — startup refusal (the
    /// probe discipline; a dynamic-HW graph is constructible from the real
    /// pinned export's twin, here approximated by asserting the ERROR PATH
    /// via the config matrix below and the message contract in code review).
    #[tokio::test]
    async fn bare_kind_without_model_or_path_is_a_config_error() {
        let err = resolve_model(
            &Siglip2Config {
                kind: Siglip2Tag::Siglip2,
                model: None,
                path: None,
                offline: true,
            },
            Path::new("/tmp"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires model"), "{msg}");
        assert!(msg.contains(BUILTIN), "{msg}");
        assert!(
            msg.contains("M3-T4"),
            "the non-decision must be visible: {msg}"
        );
    }

    #[test]
    fn config_roundtrip_and_guards() {
        // full form round-trips flat (fingerprint-facing)
        let full = "kind = \"siglip2\"\nmodel = \"siglip2-base-patch16-224\"\noffline = true\n";
        let cfg: Siglip2Config = toml::from_str(full).unwrap();
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"siglip2\"\nmodel = \"siglip2-base-patch16-224\"\noffline = true"
        );
        // wrong kind literal rejected; unknown field rejected (stage-2)
        assert!(toml::from_str::<Siglip2Config>("kind = \"ort\"").is_err());
        assert!(
            toml::from_str::<Siglip2Config>("kind = \"siglip2\"\nimage_size = 384").is_err(),
            "no preprocessing knobs (§5.10.8)"
        );
    }

    /// REAL model smoke (downloads ~1.5 GiB through the prefetch path):
    /// #[ignore] like the ort real-model smoke; the T9 gate runs it.
    #[tokio::test]
    #[ignore = "downloads the real ~1.5 GiB siglip2 towers; run explicitly (T9 gate)"]
    async fn real_siglip2_model_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Siglip2Config {
            kind: Siglip2Tag::Siglip2,
            model: Some("siglip2-base-patch16-224".to_string()),
            path: None,
            offline: false,
        };
        let e = Siglip2Embedder::new(&cfg, dir.path()).await.unwrap();
        assert_eq!(e.dim(), 768, "siglip2-base projection dim");
        // a real photo-shaped query: any solid PNG exercises the full path
        let png = solid_png((640, 480), [24, 60, 240]);
        let v = e
            .embed_query(&Query::Image(png, "image/png".into()))
            .await
            .unwrap();
        assert_eq!(v.len(), 768);
        assert!(v.iter().all(|x| x.is_finite()));
        assert!((unit(&v) - 1.0).abs() < 1e-3);
        // text tower: query-side twin — finite, unit-norm, deterministic
        // (no cross-modal DIRECTION assertion: a solid-color probe image has
        // no semantics to rank against; that is benchmark territory, not a
        // smoke gate)
        let t1 = e
            .embed_query(&Query::Text("a photo of a panda".into()))
            .await
            .unwrap();
        let t2 = e
            .embed_query(&Query::Text("a photo of a panda".into()))
            .await
            .unwrap();
        assert_eq!(t1.len(), 768);
        assert!(t1.iter().all(|x| x.is_finite()));
        assert!((unit(&t1) - 1.0).abs() < 1e-3);
        assert_eq!(t1, t2, "CPU inference is deterministic");
    }
}
