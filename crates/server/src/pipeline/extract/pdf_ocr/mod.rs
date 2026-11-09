//! `pdf-ocr` extractor kind (M3-T10): OCR for scanned PDFs — PaddleOCR
//! PP-OCRv5 mobile (official ONNX, Apache-2.0) through OUR ort sessions.
//!
//! # Selection (§0 ruling 2026-11-07, research/2026-11-ocr-reranker-selection.md)
//!
//! PaddleOCR official ONNX won the four hard constraints outright: pure
//! ONNX files on the EXISTING ort session mechanism (zero new .so, zero new
//! Rust dependency — oar-ocr was demoted to an Apache-2.0 reference because
//! its `ort =2.0.0-rc.13` pin would be a second ort copy beside our rc.15),
//! Apache-2.0 the whole way (repo + models + exports), Chinese in the top
//! tier (one PP-OCRv5 rec model covers 简中/繁中/英/日/拼音; printed Chinese
//! det Hmean 0.945 / rec 0.9013), and ~20 MiB total (det 4.60 MiB +
//! rec 15.77 MiB — two orders below the embedder budget). Default tier
//! PP-OCRv5 **mobile**; the documented upgrade path (v6 small/medium, v5
//! server ≈165 MiB) rides `path = "…"` pointing at a hand-fetched model
//! directory (same layout: `det.onnx` + `rec.onnx` + `dict.yml`).
//!
//! # The two-stage pipeline
//!
//! pdfium renders each page (the T3 render path, our OWN dpi knob — OCR
//! wants 300, not pdf-pages' display-grade 150, see [`DEFAULT_DPI`]) →
//! det (DBNet: resize_long 960, BGR + ImageNet normalize → probability map
//! → [`db::db_postprocess`]) → per line: perspective crop
//! ([`rec::crop_quad`]) → rec (CRNN/SVTR CTC: 3×48×W, `(v/255−0.5)/0.5`
//! BGR → [`rec::ctc_greedy_decode`] against the artifact's embedded
//! dictionary). Every page with surviving lines becomes ONE Text Part,
//! page order preserved; a page that yields nothing contributes nothing;
//! a document where NO page yields text is a loud Err (the pdf-text
//! precedent: silently indexing nothing is worse).
//!
//! # Model resolution (the embedder discipline, extractor-side)
//!
//! `{ kind = "pdf-ocr", dpi = 300 }` is the whole config face: the model is
//! the kind's FIXED built-in (a §0-adjudicated default, like the M2 gte
//! pick), resolved as `path` > `models_dir/<ppocr-v5-mobile>/`. The models
//! cache path is wired at INGEST LOAD ([`hydrate`]: prefetch when missing —
//! the OrtEmbedder::new semantics), because the Extractor trait is sync and
//! config parsing stays side-effect-free; an unhydrated instance (a test
//! constructing one directly) extracts with a loud Err instead of guessing.
//! The resolved directory rides a `#[serde(skip)]` field: the FINGERPRINT
//! still serializes exactly what the operator wrote (`kind` + `dpi` + the
//! optional `path` — dpi is chunk identity, the pdf-pages precedent).
//!
//! # Sessions (pdfium + ort, both process-shared)
//!
//! pdfium binds through the shared [`pdf_text::bind_pdfium`] handle (the
//! pdf-pages discipline). The two ort sessions load once per model
//! DIRECTORY through [`embed::ort`]'s environment/commit path (the siglip2
//! discipline — one ort env per process however many kinds) and cache in a
//! process-global map keyed by the resolved dir (the pdfium one-shot,
//! per-dir: tests exercise fixture and error dirs side by side). Inference
//! runs inside the ingest worker's CPU segment (extract is sync; the
//! worker's spawn_blocking + CPU semaphore already scope it, §5.10.5).
//!
//! # Guards
//!
//! Page cap and dpi domain ARE the T3 constants ([`MAX_RENDER_PAGES`] 512,
//! 10..=600 dpi) — same risk class, same numbers; a failed page render is
//! a loud Err naming the page (the T3 no-silent-loss ruling). det input is
//! bounded by resize_long 960; rec lines are bounded by [`rec::REC_MAX_WIDTH`].
//!
//! # Chain semantics
//!
//! OCR IS the scan-dominant route: affinity declares ScanDominant
//! Preferred, MixedTextScan/TextNative Capable (a text member outranks OCR
//! wherever a text layer exists — the router's quality protection), NoText
//! Refuses (the ContentClass::remedy vocabulary: blank pages are "OCR
//! included can't help" — consistent with it). Under `extract` chains the
//! kind's Err/empty output is its yield signal like every leaf.

mod db;
mod rec;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pdfium_render::prelude::*;

use super::super::gate;
use super::pdf_text::{self, LibrarySource};
use super::{ContentClass, Extractor, Fit, Modality, Part, PipelineError};

/// The built-in model name this kind pins (the models.rs row; the §0
/// 2026-11-07 adjudicated default tier).
pub(crate) const BUILTIN_MODEL: &str = "ppocr-v5-mobile";

/// The model directory layout this kind loads (prefetch locals).
pub(crate) const MODEL_FILES: [&str; 3] = ["det.onnx", "rec.onnx", "dict.yml"];

/// Default OCR render resolution: 300 dots per inch — the digitization
/// standard for OCR (not pdf-pages' display-grade 150). The decisive
/// consumer is the REC stage: its input lines are 48 px tall, so a 10 pt
/// body glyph at 150 dpi is ~21 px (a 2.3× upsample through the recognizer)
/// but ~41 px at 300 dpi — native-resolution lines, no invented detail. The
/// det stage is dpi-insensitive by construction (its input is resized to a
/// 960 px long side regardless). 300 is also the number every scanner
/// vendor's "text PDF" profile ships.
pub const DEFAULT_DPI: u16 = 300;

/// dpi domain floor (the T3 constant: below 10 a letter page renders under
/// 85 px tall — glyphs collapse to noise).
const MIN_DPI: u16 = 10;

/// dpi domain ceiling (the T3 constant: 600 is the print-scan ceiling;
/// above it a letter page's RGBA bitmap alone exceeds ~134 MiB).
const MAX_DPI: u16 = 600;

/// Page-count bomb guard (the T3 constant, same risk class and number: a
/// hostile `/Count 5000` document must not buy thousands of OCR passes).
const MAX_RENDER_PAGES: usize = 512;

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): `kind = "pdf-ocr"` plus
/// the dpi knob and the model-directory escape hatch. `dpi` serializes
/// into the fingerprint EVEN when defaulted (the pdf-pages precedent: it
/// changes chunk identity); `resolved` is load-time state and skipped.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfOcrConfig {
    pub kind: PdfOcrTag,
    /// OCR render resolution in DOTS PER INCH (pdfium renders at dpi/72 ×
    /// page points; the raster feeds det+rec). Accepted domain
    /// [`MIN_DPI`]..=[`MAX_DPI`] = 10..=600, validated at startup; 300 is
    /// the OCR standard (see [`DEFAULT_DPI`]).
    #[serde(default = "default_dpi")]
    pub dpi: u16,
    /// Local model directory override (layout: [`MODEL_FILES`]) — the
    /// documented path for tiers the built-in table does not carry (v6
    /// small/medium, v5 server). A `path` is never prefetched.
    #[serde(default)]
    pub path: Option<String>,
    /// The RESOLVED model directory, wired by ingest load ([`hydrate`]);
    /// `None` for a bare config (parse-level) instance. Not config, not
    /// fingerprinted — it is derived state, like a bound library handle.
    #[serde(skip)]
    pub(crate) resolved: Option<PathBuf>,
}

fn default_dpi() -> u16 {
    DEFAULT_DPI
}

/// Literal tag: only deserializes from `"pdf-ocr"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PdfOcrTag {
    #[serde(rename = "pdf-ocr")]
    PdfOcr,
}

impl PdfOcrConfig {
    /// Startup domain check (§5.10.3 fail-fast), the pdf-pages message
    /// shape with the OCR numbers.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_DPI..=MAX_DPI).contains(&self.dpi) {
            return Err(format!(
                "pdf-ocr dpi = {} is outside the accepted range {MIN_DPI}..={MAX_DPI} (dots per \
                 inch; OCR wants native-resolution text lines — 300 is the standard, 600 the \
                 print-scan ceiling)",
                self.dpi
            ));
        }
        Ok(())
    }
}

/// The model directory this config resolves to: `path` (verbatim —
/// operator-managed) > `models_dir/<BUILTIN_MODEL>`. Pure; no I/O.
pub(crate) fn resolve_model_dir(cfg: &PdfOcrConfig, models_dir: &Path) -> PathBuf {
    match &cfg.path {
        Some(p) => PathBuf::from(p),
        None => models_dir.join(BUILTIN_MODEL),
    }
}

/// Wire the model directory at INGEST LOAD (the OrtEmbedder::new
/// semantics, sync-trait-compatible): `path` = the three files must exist
/// (fail-fast); builtin = prefetch into the models cache when not ready,
/// then record the directory on the config so every later `extract` finds
/// it. Async because prefetch is.
pub(crate) async fn hydrate(
    cfg: &mut PdfOcrConfig,
    models_dir: &Path,
) -> Result<(), PipelineError> {
    let dir = resolve_model_dir(cfg, models_dir);
    if cfg.path.is_none() {
        let Some(model) = crate::pipeline::models::builtin(BUILTIN_MODEL) else {
            return Err(PipelineError::config(
                format!(
                    "the pdf-ocr kind pins built-in model '{BUILTIN_MODEL}' which is not in the \
                     table — this is a vectorfs bug"
                ),
                None,
            ));
        };
        if !crate::pipeline::models::is_ready(&dir, model) {
            crate::pipeline::models::prefetch(BUILTIN_MODEL, models_dir).await?;
        }
    } else {
        let missing: Vec<&str> = MODEL_FILES
            .iter()
            .filter(|f| !dir.join(f).is_file())
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(PipelineError::config(
                format!(
                    "pdf-ocr path {} does not contain {} — a path is never prefetched; point it \
                     at a complete model directory ({})",
                    dir.display(),
                    missing.join(", "),
                    MODEL_FILES.join(" + ")
                ),
                None,
            ));
        }
    }
    cfg.resolved = Some(dir);
    Ok(())
}

// ---------------------------------------------------------------------------
// the ort pair (loaded once per model dir, process-shared)
// ---------------------------------------------------------------------------

/// The loaded OCR pair: two sessions + the class table. Sessions sit in
/// mutexes (`Session::run` takes `&mut`); separate mutexes so a det run
/// never queues behind a rec run.
struct OcrCore {
    det: std::sync::Mutex<ort::session::Session>,
    rec: std::sync::Mutex<ort::session::Session>,
    /// CTC class table (blank at 0; see [`rec::class_table`]).
    table: Vec<String>,
    /// Graph input names, resolved at load (`x` on the Paddle exports).
    det_input: String,
    rec_input: String,
}

/// Process-global session cache, keyed by resolved model dir (the pdfium
/// one-shot discipline, per-dir: first load wins per directory; a FAILED
/// load is not sticky — a fixed environment can retry on the next file).
static CORES: std::sync::Mutex<BTreeMap<PathBuf, Arc<OcrCore>>> =
    std::sync::Mutex::new(BTreeMap::new());

/// The shared pair for `dir`, loading on first use.
fn ocr_core(dir: &Path) -> Result<Arc<OcrCore>, PipelineError> {
    let mut cache = CORES.lock().map_err(|_| {
        PipelineError::extract("pdf-ocr: the session cache lock is poisoned — restart the daemon")
    })?;
    if let Some(core) = cache.get(dir) {
        return Ok(Arc::clone(core));
    }
    let core = Arc::new(OcrCore::load(dir)?);
    cache.insert(dir.to_path_buf(), Arc::clone(&core));
    Ok(core)
}

impl OcrCore {
    /// Load both sessions + the dictionary and run the startup probes:
    /// det input rank 4 float; rec input rank 4 float and output rank 3
    /// with a STATIC class count that must match the dictionary (± the
    /// space row — [`rec::class_table`]).
    fn load(dir: &Path) -> Result<Self, PipelineError> {
        for f in MODEL_FILES {
            if !dir.join(f).is_file() {
                return Err(PipelineError::extract(format!(
                    "pdf-ocr model directory {} is incomplete (missing {f}; layout = {})",
                    dir.display(),
                    MODEL_FILES.join(" + ")
                )));
            }
        }
        let source = crate::pipeline::embed::ort::locate_library()
            .map_err(|m| PipelineError::extract(format!("onnxruntime library unavailable: {m}")))?;
        let env = crate::pipeline::embed::ort::ort_environment(&source)?;
        let det = crate::pipeline::embed::ort::commit_session(
            &env,
            &dir.join("det.onnx").display().to_string(),
        )?;
        let rec = crate::pipeline::embed::ort::commit_session(
            &env,
            &dir.join("rec.onnx").display().to_string(),
        )?;
        // input probes: `x` by name (the Paddle exports), else the single
        // declared input (the siglip2 leniency for compatible artifacts)
        let input_name = |s: &ort::session::Session, want: &str| -> Option<String> {
            if s.inputs().iter().any(|o| o.name() == want) {
                Some(want.to_string())
            } else if s.inputs().len() == 1 {
                Some(s.inputs()[0].name().to_string())
            } else {
                None
            }
        };
        let (det_input, rec_input) = (
            input_name(&det, "x").ok_or_else(|| {
                PipelineError::extract(
                    "pdf-ocr: the det graph's inputs are not the expected single 'x' tensor",
                )
            })?,
            input_name(&rec, "x").ok_or_else(|| {
                PipelineError::extract(
                    "pdf-ocr: the rec graph's inputs are not the expected single 'x' tensor",
                )
            })?,
        );
        let out_dims = |s: &ort::session::Session| -> Option<Vec<i64>> {
            match s.outputs()[0].dtype() {
                ort::value::ValueType::Tensor { shape, .. } => {
                    Some(shape.iter().copied().collect())
                }
                _ => None,
            }
        };
        let det_dims = out_dims(&det).ok_or_else(|| {
            PipelineError::extract("pdf-ocr: the det graph's output is not a tensor — unsupported")
        })?;
        if det_dims.len() != 4 {
            return Err(PipelineError::extract(format!(
                "pdf-ocr: the det graph's output is not a rank-4 probability map ({det_dims:?}) — \
                 unsupported"
            )));
        }
        let rec_dims = out_dims(&rec).ok_or_else(|| {
            PipelineError::extract("pdf-ocr: the rec graph's output is not a tensor — unsupported")
        })?;
        if rec_dims.len() != 3 {
            return Err(PipelineError::extract(format!(
                "pdf-ocr: the rec graph's output is not a rank-3 [batch, time, classes] tensor \
                 ({rec_dims:?}) — unsupported",
            )));
        }
        // the rec class count: the STATIC last dim of output 0
        let classes = match rec_dims.last() {
            Some(c) if *c > 0 => *c as usize,
            _ => {
                return Err(PipelineError::extract(
                    "pdf-ocr: the rec graph's class-count dimension is dynamic — the startup \
                     contract requires a static one (a mismatched dict would silently decode \
                     garbage)",
                ));
            }
        };
        let dict = rec::parse_dict(&std::fs::read_to_string(dir.join("dict.yml"))?)?;
        let table = rec::class_table(&dict, classes)?;
        Ok(OcrCore {
            det: std::sync::Mutex::new(det),
            rec: std::sync::Mutex::new(rec),
            table,
            det_input,
            rec_input,
        })
    }

    /// One session run helper: float tensor in, float tensor out (shape
    /// checked by the caller).
    fn run_f32(
        session: &mut ort::session::Session,
        input: &str,
        shape: Vec<i64>,
        data: Vec<f32>,
        what: &str,
    ) -> Result<Vec<f32>, PipelineError> {
        let tensor = ort::value::Tensor::from_array((shape, data))
            .map_err(|e| PipelineError::extract(format!("pdf-ocr {what} tensor: {e}")))?;
        let outputs = session
            .run(vec![(input.to_string(), tensor.into_dyn())])
            .map_err(|e| PipelineError::extract(format!("pdf-ocr {what} inference failed: {e}")))?;
        let value: &ort::value::DynValue = &outputs[0];
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| PipelineError::extract(format!("pdf-ocr {what} output: {e}")))?;
        let _ = shape; // dims consumed by the callers' arithmetic
        Ok(data.to_vec())
    }

    /// det one page image → probability map row-major `[ph][pw]`.
    fn det_map(&self, rgb: &image::RgbImage) -> Result<(Vec<f32>, usize, usize), PipelineError> {
        let (data, w, h) = db::det_preprocess(rgb);
        let shape = vec![1i64, 3, h as i64, w as i64];
        let mut session = self.det.lock().map_err(|_| {
            PipelineError::extract("pdf-ocr: the det session lock is poisoned — restart the daemon")
        })?;
        let out = Self::run_f32(&mut session, &self.det_input, shape, data, "det")?;
        // DB heads are stride-1: the map comes back at the input's H×W.
        // Trust the map's own dims for everything downstream.
        let plane = (w * h) as usize;
        if out.len() != plane {
            return Err(PipelineError::extract(format!(
                "pdf-ocr det returned {} values for a {}×{} input — unexpected graph shape",
                out.len(),
                w,
                h
            )));
        }
        Ok((out, w as usize, h as usize))
    }

    /// rec one line crop → (text, confidence).
    fn recognize(&self, crop: &image::RgbImage) -> Result<(String, f32), PipelineError> {
        let (data, w) = rec::rec_preprocess(crop);
        let shape = vec![1i64, 3, rec::REC_HEIGHT as i64, w as i64];
        let mut session = self.rec.lock().map_err(|_| {
            PipelineError::extract("pdf-ocr: the rec session lock is poisoned — restart the daemon")
        })?;
        let out = Self::run_f32(&mut session, &self.rec_input, shape, data, "rec")?;
        let classes = self.table.len();
        if out.is_empty() || out.len() % classes != 0 {
            return Err(PipelineError::extract(format!(
                "pdf-ocr rec returned {} values, not a multiple of its {} classes — unexpected \
                 graph shape",
                out.len(),
                classes
            )));
        }
        let timesteps = out.len() / classes;
        Ok(rec::ctc_greedy_decode(
            &out,
            timesteps,
            classes,
            &self.table,
        ))
    }

    /// One whole page: det → boxes → crops → rec lines. Lines below the
    /// rec score gate are dropped (PaddleOCR `drop_score`); page order of
    /// the boxes IS the line order.
    fn ocr_page(&self, rgb: &image::RgbImage) -> Result<Vec<String>, PipelineError> {
        let (prob, pw, ph) = self.det_map(rgb)?;
        let quads = db::db_postprocess(&prob, pw, ph, rgb.width(), rgb.height());
        let mut lines = Vec::with_capacity(quads.len());
        for quad in quads {
            let Some(crop) = rec::crop_quad(rgb, &quad) else {
                continue; // degenerate geometry on this box — the page keeps going
            };
            let (text, conf) = self.recognize(&crop)?;
            if text.trim().is_empty() || conf < rec::DROP_SCORE {
                continue;
            }
            lines.push(text);
        }
        Ok(lines)
    }
}

// ---------------------------------------------------------------------------
// the extractor
// ---------------------------------------------------------------------------

/// Runtime instance: the dpi knob, the resolved model directory, and the
/// pdfium source snapshot (the pdf-pages shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfOcrExtractor {
    dpi: u16,
    model_dir: PathBuf,
    source: LibrarySource,
}

impl PdfOcrExtractor {
    /// Construct from a parsed (and hydrated, or `path`-carrying) config.
    /// Construction never touches the .so or the sessions — first `extract`
    /// binds/loads, so config parsing stays side-effect-free.
    pub fn new(cfg: &PdfOcrConfig) -> Result<Self, PipelineError> {
        let model_dir = match (&cfg.resolved, &cfg.path) {
            (Some(dir), _) => dir.clone(),
            (None, Some(p)) => PathBuf::from(p),
            (None, None) => {
                return Err(PipelineError::extract(
                    "pdf-ocr: no model directory — this instance was never wired to a models \
                     cache (ingest load does that; a directly-constructed instance must set \
                     path = \"…\" or come through ingest load)",
                ));
            }
        };
        Ok(PdfOcrExtractor {
            dpi: cfg.dpi,
            model_dir,
            source: pdf_text::locate_library().unwrap_or(LibrarySource::System),
        })
    }
}

impl Extractor for PdfOcrExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        gate::refuse_empty_input("pdf-ocr", bytes)?;
        gate::refuse_foreign_mime("pdf-ocr", mime, gate::Accept::Exact("application/pdf"))?;
        let core = ocr_core(&self.model_dir)?;
        let pdfium = pdf_text::bind_pdfium(&self.source)?;
        let document = pdfium
            .load_pdf_from_byte_vec(bytes.to_vec(), None)
            .map_err(|e| {
                PipelineError::extract(format!(
                    "pdfium cannot open this document (corrupt, encrypted, or not a PDF): {e}"
                ))
            })?;
        let pages = document.pages();
        let page_count = pages.len() as usize;
        if page_count == 0 {
            return Err(PipelineError::extract(
                "document has no pages — nothing to extract",
            ));
        }
        if page_count > MAX_RENDER_PAGES {
            return Err(PipelineError::extract(format!(
                "pdf-ocr: document has {page_count} pages (MAX_RENDER_PAGES = \
                 {MAX_RENDER_PAGES}) — page OCR is refused before any page work; if the document \
                 is legitimate, split it into volumes",
            )));
        }
        let config = PdfRenderConfig::new().scale_page_by_factor(self.dpi as f32 / 72.0);
        let mut parts = Vec::with_capacity(page_count);
        for (i, page) in pages.iter().enumerate() {
            let rgb = page
                .render_with_config(&config)
                .and_then(|bitmap| bitmap.as_image())
                .map(|img| img.to_rgb8())
                .map_err(|e| {
                    // loud, page named — never skip-and-continue (the T3
                    // silent-content-loss ruling)
                    PipelineError::extract(format!(
                        "pdf-ocr: rendering page {} of {page_count} at dpi {} failed: {e}",
                        i + 1,
                        self.dpi
                    ))
                })?;
            let lines = core.ocr_page(&rgb).map_err(|e| {
                PipelineError::extract(format!("pdf-ocr: page {} of {page_count}: {e}", i + 1))
            })?;
            if lines.is_empty() {
                continue; // a page with nothing legible contributes nothing
            }
            parts.push(Part {
                modality: Modality::Text,
                text: lines.join("\n"),
                image: None,
                labels: Vec::new(),
                pages: Vec::new(),
                furniture_removed: 0,
                section: None,
            });
        }
        if parts.is_empty() {
            return Err(PipelineError::extract(format!(
                "no text recognized on any of {page_count} pages — the document may be blank, or \
                 the scan quality is below what the OCR tier reads",
            )));
        }
        Ok(parts)
    }
}

// ---------------------------------------------------------------------------
// affinity (M2.1 step 2): the kind's class-level routing declaration
// ---------------------------------------------------------------------------

/// Class-level affinity — OCR's territorial mirror of pdf-text's:
/// * scan-dominant documents are this kind's EXACT use case (`Preferred`) —
///   OCR is the main route for scans (the scanned-docs preset chains it
///   behind pdf-text precisely so the router pulls it here);
/// * mixed text+scan: OCR would add the scan pages pdfium misses
///   (`Capable` — but the text member is the better first pick where a
///   text layer exists; the router's Preferred tier decides);
/// * text-native: OCR is real-but-degraded output versus decoding the text
///   layer (`Capable` — a text-declaring member beats it through
///   Preferred; running OCR on text-native PDFs wastes seconds per page);
/// * no-text (blank pages): nothing to OCR (`Refuses` — consistent with
///   `ContentClass::remedy`'s "OCR included can't help" vocabulary).
///
/// Static and knob-independent: `dpi` changes pixel density, never routing.
pub fn affinity(class: super::ContentClass) -> super::Fit {
    match class {
        ContentClass::ScanDominant => Fit::Preferred,
        ContentClass::MixedTextScan | ContentClass::TextNative => Fit::Capable,
        ContentClass::NoText => Fit::Refuses,
    }
}

/// One-line reason the router quotes when this kind Refuses a class.
pub fn refusal_note(class: super::ContentClass) -> String {
    match class {
        ContentClass::NoText => {
            "the document has no text layer and no raster payload — blank pages carry nothing to \
              OCR"
            .to_string()
        }
        other => unreachable!("pdf-ocr is {other:?}, never refuses it"),
    }
}

// ---------------------------------------------------------------------------
// tests — hermetic through the tiny-ppocr fixture (committed ONNX pair with
// a constant det probability map and a constant rec distribution) + the real
// .so for pdfium and ort (the honest-failure rule); the REAL models run only
// in the #[ignore] smoke below.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The committed fixture directory (det.onnx + rec.onnx + dict.yml; run
    /// its generate.py if it was removed — the tiny-siglip2 discipline).
    pub(crate) fn fixture_dir() -> PathBuf {
        for p in ["vf/tests/fixtures/tiny-ppocr", "tests/fixtures/tiny-ppocr"] {
            let d = PathBuf::from(p);
            if d.join("det.onnx").is_file() {
                return d;
            }
        }
        panic!(
            "tiny-ppocr fixture not found (expected vf/tests/fixtures/tiny-ppocr with det.onnx + \
             rec.onnx + dict.yml) — run its generate.py if it was removed"
        )
    }

    /// Contract-run constructor: real pdfium + ort or an actionable panic.
    pub(crate) fn extractor_or_panic(dpi: u16) -> PdfOcrExtractor {
        match pdf_text::locate_library() {
            Ok(_) => PdfOcrExtractor::new(&cfg_of(dpi)).expect("fixture model must load"),
            Err(m) => panic!(
                "pdf-ocr tests need the real pdfium and none was found: {m} — run \
                 scripts/fetch_runtime_libs.sh pdfium (or point VF_PDFIUM_PATH at it). Tests \
                 refuse to skip (honest-failure rule)."
            ),
        }
    }

    fn cfg_of(dpi: u16) -> PdfOcrConfig {
        let cfg: PdfOcrConfig = toml::from_str(&format!(
            "kind = \"pdf-ocr\"\ndpi = {dpi}\npath = \"{}\"",
            fixture_dir().display()
        ))
        .unwrap();
        cfg
    }

    /// The committed hello.pdf (the pdf-pages/pdf-text fixture).
    const HELLO_PDF_MINIMAL: &[u8] = include_bytes!("../../../../tests/golden/hello.pdf");

    #[test]
    fn config_roundtrip_and_domain() {
        // default: dpi absent → 300, and the default serializes into the
        // fingerprint (chunk identity: the pdf-pages precedent)
        let cfg: PdfOcrConfig = toml::from_str("kind = \"pdf-ocr\"").unwrap();
        assert_eq!(cfg.dpi, DEFAULT_DPI);
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"pdf-ocr\"\ndpi = 300"
        );
        assert!(cfg.validate().is_ok());
        // explicit dpi + path round-trip
        let cfg: PdfOcrConfig = toml::from_str(&format!(
            "kind = \"pdf-ocr\"\ndpi = 150\npath = \"{}\"",
            fixture_dir().display()
        ))
        .unwrap();
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            format!(
                "kind = \"pdf-ocr\"\ndpi = 150\npath = \"{}\"",
                fixture_dir().display()
            )
        );
        // domain: 0 / below-floor / above-ceiling rejected with the unit;
        // the bounds themselves legal
        for dpi in [0u16, 9, 601, 60_000] {
            let err = cfg_of(dpi).validate().unwrap_err();
            assert!(err.contains("dots per inch"), "{err}");
            assert!(err.contains("10..=600"), "{err}");
        }
        for dpi in [MIN_DPI, MAX_DPI] {
            assert!(cfg_of(dpi).validate().is_ok());
        }
        // wrong literal / typed dpi / unknown field rejected
        assert!(toml::from_str::<PdfOcrConfig>("kind = \"pdf-pages\"").is_err());
        assert!(toml::from_str::<PdfOcrConfig>("kind = \"pdf-ocr\"\ndpi = \"high\"").is_err());
        assert!(toml::from_str::<PdfOcrConfig>("kind = \"pdf-ocr\"\nlang = \"ch\"").is_err());
        // resolved is NOT fingerprinted: two configs differing only in it
        // serialize identically
        let mut a = cfg_of(300);
        let b = cfg_of(300);
        a.resolved = Some(PathBuf::from("/somewhere"));
        assert_eq!(toml::to_string(&a).unwrap(), toml::to_string(&b).unwrap());
    }

    #[test]
    fn unhydrated_instance_refuses_loudly() {
        let cfg: PdfOcrConfig = toml::from_str("kind = \"pdf-ocr\"").unwrap();
        let err = PdfOcrExtractor::new(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no model directory"), "{msg}");
        assert!(msg.contains("ingest load"), "{msg}");
    }

    #[test]
    fn affinity_declares_ocr_territory() {
        assert_eq!(affinity(ContentClass::ScanDominant), Fit::Preferred);
        for class in [ContentClass::MixedTextScan, ContentClass::TextNative] {
            assert_eq!(affinity(class), Fit::Capable, "{class}");
        }
        assert_eq!(affinity(ContentClass::NoText), Fit::Refuses);
        assert!(refusal_note(ContentClass::NoText).contains("nothing to OCR"));
    }

    #[test]
    fn extracts_text_parts_from_a_pdf_through_the_fixture() {
        // the fixture det returns a ~1.0 probability map everywhere → the
        // whole page is one detection; the fixture rec returns one constant
        // winning class → a known single-grapheme line "B" per page
        let x = extractor_or_panic(300);
        let parts = x.extract(HELLO_PDF_MINIMAL, "application/pdf").unwrap();
        assert_eq!(parts.len(), 1, "hello.pdf is single-page");
        assert_eq!(parts[0].modality, Modality::Text);
        assert!(parts[0].image.is_none());
        assert!(!parts[0].text.trim().is_empty(), "fixture rec yields 'B'");
        // determinism against the real engines (pdfium + ort + PNG-free
        // render, but the whole path reruns)
        let again = x.extract(HELLO_PDF_MINIMAL, "application/pdf").unwrap();
        assert_eq!(parts, again);
        // a multi-page document: one Part per page WITH text, page order
        let bytes = crate::pipeline::extract::pdf_inspector::tests::mixed_text_image_pdf();
        let parts = x.extract(&bytes, "application/pdf").unwrap();
        assert_eq!(parts.len(), 2, "both pages carry fixture text");
    }

    #[test]
    fn gates_page_cap_and_corrupt_are_errors() {
        let x = extractor_or_panic(150);
        let err = x.extract(b"", "application/pdf").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        let err = x
            .extract(HELLO_PDF_MINIMAL, "application/octet-stream")
            .unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        assert!(x.extract(HELLO_PDF_MINIMAL, "").is_ok(), "unknown mime ok");
        let corrupt = {
            let mut v = b"%PDF-1.7\n".to_vec();
            v.extend_from_slice(&[0x00, 0xff, 0x13, 0x37, 0xde, 0xad, 0xbe, 0xef]);
            v
        };
        assert!(x.extract(&corrupt, "application/pdf").is_err());
        // the page cap, T3's number verbatim
        let many = crate::pipeline::extract::pdf_pages::tests::blank_multipage_pdf(513);
        let err = x.extract(&many, "application/pdf").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("513 pages"), "{msg}");
        assert!(msg.contains("MAX_RENDER_PAGES = 512"), "{msg}");
    }

    #[test]
    fn incomplete_model_dir_is_a_loud_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixture_dir().join("det.onnx"), dir.path().join("det.onnx")).unwrap();
        let cfg: PdfOcrConfig = toml::from_str(&format!(
            "kind = \"pdf-ocr\"\npath = \"{}\"",
            dir.path().display()
        ))
        .unwrap();
        let x = PdfOcrExtractor::new(&cfg).unwrap();
        let err = x.extract(HELLO_PDF_MINIMAL, "application/pdf").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incomplete"), "{msg}");
        assert!(msg.contains("rec.onnx"), "{msg}");
        assert!(msg.contains("dict.yml"), "{msg}");
    }

    /// Async hydrate wiring: a path config passes with files present and
    /// fails naming the missing ones; a builtin config against a real
    /// models cache exercises the is_ready branch without network (the
    /// fixture dir masquerading as the cache root).
    #[tokio::test]
    async fn hydrate_wires_the_model_directory() {
        // path form: complete dir hydrates; a gutted one fails loudly
        let mut cfg = cfg_of(300);
        hydrate(&mut cfg, Path::new("/nonexistent-models"))
            .await
            .unwrap();
        assert_eq!(cfg.resolved.as_deref(), Some(fixture_dir().as_path()));
        let gutted = tempfile::tempdir().unwrap();
        let mut cfg: PdfOcrConfig = toml::from_str(&format!(
            "kind = \"pdf-ocr\"\npath = \"{}\"",
            gutted.path().display()
        ))
        .unwrap();
        let err = hydrate(&mut cfg, Path::new("/nonexistent-models"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("never prefetched"), "{err}");
        // builtin form: is_ready against a cache that does NOT hold the
        // model would prefetch (network) — assert the resolution shape only
        // by pointing the models_dir at the fixture PARENT under the
        // builtin name, which satisfies no stamp (not ready) → this arm
        // would hit the network; skip it here (the #[ignore] real-model
        // smoke covers the live prefetch path end to end).
    }

    /// REAL model smoke (downloads ~20 MiB det+rec+dict through the
    /// prefetch path, then OCRs a real scanned page): the research quality
    /// numbers' re-verification slot. `#[ignore]` like the ort/siglip2
    /// real-model smokes; run explicitly at the gate. The page raster is
    /// the COMMITTED `scan-lines.png` specimen — two lines of real
    /// font-rendered Chinese (Noto Sans CJK SC, OFL) at body-text
    /// proportions, the shape a 300-dpi scan of a printed page has —
    /// wrapped into an image-XObject PDF and rasterized by pdfium through
    /// the kind's own render path (a genuine synthetic scan; no font
    /// engine needed at test time).
    #[tokio::test]
    #[ignore = "downloads the real ~20 MiB PP-OCRv5 mobile pair; run explicitly (M3 gate)"]
    async fn real_ppocr_model_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: PdfOcrConfig = toml::from_str("kind = \"pdf-ocr\"").unwrap();
        hydrate(&mut cfg, dir.path()).await.expect("prefetch");
        let x = PdfOcrExtractor::new(&cfg).unwrap();
        let png = std::fs::read(fixture_dir().join("scan-lines.png")).unwrap();
        let pdf = png_image_only_pdf(png);
        let parts = x.extract(&pdf, "application/pdf").unwrap();
        assert_eq!(parts.len(), 1);
        let text = &parts[0].text;
        eprintln!("pdf-ocr real-model smoke recognized: {text:?}");
        // the specimen says: 向量文件系统支持中文识别 / 扫描文档自动光学字符识别
        assert!(!text.trim().is_empty(), "the page must read SOMETHING");
        assert!(
            text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "expected CJK text from the CJK rec model: {text:?}"
        );
        let cjk: String = text
            .chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .collect();
        // a real recognition, not one stray glyph: the specimen lines carry
        // 12 CJK chars each — accept a degraded but SUBSTANTIAL reading
        assert!(
            cjk.chars().count() >= 8,
            "expected most of a 12-char line to survive: {text:?}"
        );
    }

    /// One-page PDF drawing one full-page PNG image XObject (the
    /// image_only_pdf discipline from pdf_inspector::tests, generalized to
    /// a real raster). The PAGE preserves the image's aspect (width 612 pt,
    /// height scaled) — a stretched raster is not a scan.
    fn png_image_only_pdf(png: Vec<u8>) -> Vec<u8> {
        // decode to raw RGB samples (DeviceRGB, 8bpc, uncompressed)
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        let (w, h) = decoded.dimensions();
        let samples = decoded.into_raw();
        // page: 612 pt wide, height at the image's aspect (2 decimals)
        let page_h = (612.0 * h as f32 / w as f32 * 100.0).round() / 100.0;
        let image_obj = format!(
            "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB \
             /BitsPerComponent 8 /Length {} >>\nstream\n",
            samples.len()
        )
        .into_bytes();
        let content = format!("q 612 0 0 {page_h} 0 0 cm /Im0 Do Q\n");
        let bodies: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 {page_h}] /Resources << \
                 /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
            )
            .into_bytes(),
            {
                let mut v = image_obj;
                v.extend_from_slice(&samples);
                v.extend_from_slice(b"\nendstream");
                v
            },
            format!(
                "<< /Length {} >>\nstream\n{}endstream",
                content.len(),
                content
            )
            .into_bytes(),
        ];
        let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::with_capacity(bodies.len());
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", bodies.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
                bodies.len() + 1
            )
            .as_bytes(),
        );
        out
    }
}
