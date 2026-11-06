//! `pdf-text` extractor kind: whole-document text out of a PDF via pdfium.
//!
//! # Engine + binding (design.md §5.7 "pdfium-render (.so load-dynamic)")
//!
//! `pdfium-render` 0.9 binds the pdfium C++ library DYNAMICALLY through
//! [`Pdfium::bind_to_library`] (takes the library FILE path — pdfium-render
//! 0.9.5 `pdfium.rs:173`; there is no `bind_to_dynamic_library`, that name is
//! the design-doc shorthand for this API). The pinned binary is
//! bblanchon/pdfium-binaries `chromium/8066`, fetched by
//! `scripts/fetch_runtime_libs.sh` into the gitignored `runtime-libs/`
//! directory; the bindings feature set is gated to `pdfium_7881` (newest gate
//! ≤ 8066) so we never bind a symbol the pinned .so lacks.
//!
//! # DIP boundary (enforced by review, §5.10.2)
//!
//! Every pdfium-render type stays inside this module. The outside world sees
//! only [`PdfTextExtractor`] (plain data) and [`PipelineError`].
//!
//! # Process-global bind (pdfium-render constraint, not a choice)
//!
//! pdfium-render stores the FFI bindings in a process-global `OnceLock` and
//! refuses a second bind (`PdfiumLibraryBindingsAlreadyInitialized`); it also
//! calls `FPDF_InitLibrary` exactly once. We mirror that shape: a static
//! `OnceLock` caches ONE bind attempt — the first extractor call wins, a
//! failed bind is sticky for the process (restart after fixing the
//! environment), and the `thread_safe` feature makes the cached handle sound
//! to share across the `spawn_blocking` ingest pool (§5.10.5).
//!
//! # Library resolution (mirrors the test locator; §5.10.7 pin discipline)
//!
//! `env VF_PDFIUM_PATH` > `runtime-libs/<lib>` (repo-local fetch-script
//! output; checked at the workspace root and one level up, covering both
//! cargo test working directories) > system loader (`dlopen` search path).
//! An explicit `VF_PDFIUM_PATH` that does not exist is a hard error — an
//! operator override must not silently fall through to some other copy.
//!
//! # Contract behavior
//!
//! * empty input -> Err (uniform empty-source rule, see txt.rs);
//! * mime gate: exactly `application/pdf` (or empty = unknown, T3 sniffs);
//! * corrupt/truncated bytes -> pdfium load error translated to Err, never a
//!   panic (golden corpus asserts this against the real .so);
//! * a PDF whose pages yield no text (scanned/image-only) -> Err pointing at
//!   the M3 `pdf-ocr` kind: silently indexing an empty document is worse
//!   than an honest failure;
//! * page texts are joined `"\n\n"` into ONE Text Part (§5.10.2: one
//!   logical document, one extraction product; page-level chunking is the
//!   chunker's job).
//!
//! # Chain semantics (M2.1)
//!
//! Under an `extract` fallback chain this kind's Err or empty output IS
//! its yield signal: the ingest worker hands the bytes to the next chain
//! member instead of failing the file (and this kind is itself a common
//! LAST member — the pdfium recovery path after a stricter first member
//! yields). Standalone (single-member route) the same Err stays a plain,
//! visible FAILED.

use std::path::PathBuf;
use std::sync::OnceLock;

use pdfium_render::prelude::*;

use super::super::gate;
use super::{ContentClass, Extractor, Fit, Modality, Part, PipelineError};

/// Guidance line shared by the runtime bind error and the test locator so
/// both point at the same fix (library file name is platform-dependent:
/// libpdfium.so / libpdfium.dylib).
const FETCH_HINT: &str =
    "run scripts/fetch_runtime_libs.sh pdfium (or point VF_PDFIUM_PATH at the pdfium library)";

/// Where the pdfium library for this process comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibrarySource {
    /// An explicit file path (env override or the repo-local `runtime-libs/`
    /// copy installed by the fetch script).
    File(PathBuf),
    /// No repo-local copy: defer to the system dynamic loader.
    System,
}

/// Resolve the pdfium library for this process (public: the golden-corpus
/// tests and the contract runs share this locator so their failure mode is
/// the same actionable panic as the runtime error).
pub fn locate_library() -> Result<LibrarySource, String> {
    if let Some(explicit) = std::env::var_os("VF_PDFIUM_PATH") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(LibrarySource::File(p));
        }
        return Err(format!(
            "VF_PDFIUM_PATH is set but {} is not a file (explicit overrides must resolve; \
             unset it or fix the path)",
            p.display()
        ));
    }
    // Repo-local fetch-script output. cargo runs test binaries from the
    // workspace root AND from the member dir depending on invocation; since
    // the three-crate split the member dir sits TWO levels below the root
    // (crates/server), so all three relative prefixes are candidates. The
    // file name follows the platform (libpdfium.so on Linux,
    // libpdfium.dylib on macOS — pdfium-render's own
    // `pdfium_platform_library_name`).
    let lib_name = Pdfium::pdfium_platform_library_name();
    for prefix in [".", "..", "../.."] {
        let p = PathBuf::from(prefix).join("runtime-libs").join(&lib_name);
        if p.is_file() {
            return Ok(LibrarySource::File(p));
        }
    }
    Ok(LibrarySource::System)
}

/// Runtime instance: the resolved library source, bound lazily on first use
/// (binding is process-global anyway; lazy keeps config parsing and
/// `pipelines check` .so-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfTextExtractor {
    source: LibrarySource,
}

impl Default for PdfTextExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-global pdfium handle shared by every pdfium-backed kind
/// (`pdf-text` here, `pdf-pages` in its sibling module): bind on first use,
/// cache the attempt (success or failure) for the process lifetime. The
/// visibility is scoped to the extract kind modules — the DIP boundary
/// (pdfium-render types never leave `extract/pdf_*.rs`) holds; a pdfium
/// swap still touches only these two files.
pub(in crate::pipeline::extract) fn bind_pdfium(
    source: &LibrarySource,
) -> Result<&'static Pdfium, PipelineError> {
    static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();
    PDFIUM
        .get_or_init(|| match source {
            // bind_to_library yields the raw bindings; Pdfium::new wraps
            // them (and runs the one-shot FPDF_InitLibrary).
            LibrarySource::File(p) => Pdfium::bind_to_library(p)
                .map(Pdfium::new)
                .map_err(|e| format!("bind to {} failed: {e}", p.display())),
            LibrarySource::System => Pdfium::bind_to_system_library()
                .map(Pdfium::new)
                .map_err(|e| format!("no libpdfium found via the system loader: {e}")),
        })
        .as_ref()
        .map_err(|m| {
            PipelineError::extract(format!("pdfium library unavailable: {m} ({FETCH_HINT})"))
        })
}

impl PdfTextExtractor {
    /// Default constructor: apply the documented resolution chain. The path
    /// snapshot is taken HERE (deterministic per process); a resolution
    /// failure is deferred to the first `extract` as an Err — construction
    /// stays infallible so config parsing never needs the binary present.
    pub fn new() -> Self {
        let source = locate_library().unwrap_or(LibrarySource::System);
        PdfTextExtractor { source }
    }

    /// Explicit library path (tests, or a future config knob): bypasses the
    /// chain. The pdfium-render global-bind one-shot still applies — the
    /// FIRST bound path wins for the whole process.
    pub fn with_library(path: impl Into<PathBuf>) -> Self {
        PdfTextExtractor {
            source: LibrarySource::File(path.into()),
        }
    }

    /// The process-global pdfium handle (see [`bind_pdfium`]).
    fn pdfium(&self) -> Result<&'static Pdfium, PipelineError> {
        bind_pdfium(&self.source)
    }
}

impl Extractor for PdfTextExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        // the shared input gates (messages byte-identical to the hand-rolled
        // copies this replaced — see pipeline/gate.rs)
        gate::refuse_empty_input("pdf-text", bytes)?;
        gate::refuse_foreign_mime("pdf-text", mime, gate::Accept::Exact("application/pdf"))?;
        let pdfium = self.pdfium()?;
        // load_pdf_from_byte_vec takes ownership (pdfium keeps a handle to the
        // buffer), so one copy is inherent to the API.
        let document = pdfium
            .load_pdf_from_byte_vec(bytes.to_vec(), None)
            .map_err(|e| {
                PipelineError::extract(format!(
                    "pdfium cannot open this document (corrupt, encrypted, or not a PDF): {e}"
                ))
            })?;
        let pages = document.pages();
        // PdfPages::len() is pdfium-render's PdfPageIndex = c_int
        let page_count = pages.len() as usize;
        let mut texts = Vec::with_capacity(page_count);
        for page in pages.iter() {
            let text = page
                .text()
                .map_err(|e| PipelineError::extract(format!("pdfium text page error: {e}")))?
                .all();
            texts.push(text);
        }
        if page_count == 0 {
            return Err(PipelineError::extract(
                "document has no pages — nothing to extract",
            ));
        }
        let text = texts.join("\n\n");
        if text.trim().is_empty() {
            return Err(PipelineError::extract(
                "no extractable text (a scanned/image-only PDF needs the pdf-ocr kind, M3) — \
                 refusing to index an empty document",
            ));
        }
        Ok(vec![Part {
            modality: Modality::Text,
            text,
            image: None,
        }])
    }
}

// ---------------------------------------------------------------------------
// affinity (M2.1 step 2): the kind's class-level routing declaration
// ---------------------------------------------------------------------------

/// Class-level affinity (see [`super::super::Extractor::affinity`]):
/// * text-native documents are this kind's exact use case — `Preferred`;
/// * mixed text+scan documents stay `Capable` — pdfium extracts the text
///   pages (partial output), which IS this kind's M2.1 chain behavior; an
///   OCR member routed first would still beat it via Preferred, an
///   inspector member refuses the class outright;
/// * scans and no-text documents are `Refuses`: pdfium would find nothing
///   (its own "no extractable text" refusal), so declaring it spares the
///   attempt and lets the router fail the file with a class-accurate reason.
///
/// Static and knob-independent (this kind has no knobs; the library source
/// is deployment state, never routing).
pub fn affinity(class: super::ContentClass) -> super::Fit {
    match class {
        ContentClass::TextNative => Fit::Preferred,
        ContentClass::MixedTextScan => Fit::Capable,
        ContentClass::ScanDominant | ContentClass::NoText => Fit::Refuses,
    }
}

/// One-line reason the router quotes when this kind Refuses a class (only
/// called on the Refuses arm of [`affinity`]).
pub fn refusal_note(class: super::ContentClass) -> String {
    match class {
        ContentClass::ScanDominant => {
            "a scan has no text layer to extract — OCR territory".to_string()
        }
        ContentClass::NoText => "the document has no text layer at all".to_string(),
        other => unreachable!("pdf-text is {other:?}, never refuses it"),
    }
}

/// Per-kind config struct: `pdf-text` has no knobs in M2 (the .so location
/// is environment/deployment state, §5.10.7, not pipeline policy — it must
/// not change chunk identity, so it does not belong in the fingerprint).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfTextConfig {
    pub kind: PdfTextTag,
}

/// Literal tag: only deserializes from `"pdf-text"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PdfTextTag {
    #[serde(rename = "pdf-text")]
    PdfText,
}

// ---------------------------------------------------------------------------
// tests — all of these run against the REAL .so; when it is missing they
// panic with the fetch hint instead of skipping (honest failure: a silently
// skipped pdf test suite is a green lie).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Shared fixture: the committed minimal single-page PDF whose content
    /// stream draws "Hello vectorfs" (tests/golden/hello.pdf; the golden
    /// corpus includes the same file and builds larger kin in-test).
    pub(crate) const HELLO_PDF_MINIMAL: &[u8] = include_bytes!("../../../tests/golden/hello.pdf");

    /// Contract-run constructor: panics with actionable guidance when the
    /// library cannot be located at all (a `System` fallback still proceeds
    /// — a bind failure then surfaces as a loud test failure, not a skip).
    pub(crate) fn extractor_or_panic() -> PdfTextExtractor {
        match locate_library() {
            Ok(_) => PdfTextExtractor::new(),
            Err(m) => panic!(
                "pdfium tests need the real library and none was found: {m} — {FETCH_HINT}. \
                 Tests refuse to skip (M2-T4 honest-failure rule)."
            ),
        }
    }

    #[test]
    fn extracts_text_from_real_pdf() {
        let parts = extractor_or_panic()
            .extract(HELLO_PDF_MINIMAL, "application/pdf")
            .expect("fixture PDF must extract");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].modality, Modality::Text);
        assert!(
            parts[0].text.contains("Hello vectorfs"),
            "extracted text lacks the drawn string: {:?}",
            parts[0].text
        );
        // determinism against the real engine
        let again = PdfTextExtractor::new()
            .extract(HELLO_PDF_MINIMAL, "application/pdf")
            .unwrap();
        assert_eq!(parts, again);
    }

    #[test]
    fn empty_wrong_mime_and_corrupt_are_errors() {
        let x = extractor_or_panic();
        // empty input
        let err = x.extract(b"", "application/pdf").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        // wrong mime
        let err = x
            .extract(HELLO_PDF_MINIMAL, "application/octet-stream")
            .unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        // empty mime = unknown, accepted
        assert!(x.extract(HELLO_PDF_MINIMAL, "").is_ok());
        // corrupt bytes with a PDF magic prefix: pdfium must Err, not panic
        let corrupt = {
            let mut v = b"%PDF-1.7\n".to_vec();
            v.extend_from_slice(&[0x00, 0xff, 0x13, 0x37, 0xde, 0xad, 0xbe, 0xef]);
            v
        };
        let err = x.extract(&corrupt, "application/pdf").unwrap_err();
        assert!(matches!(err, PipelineError::Extract(_)), "{err}");
    }

    #[test]
    fn affinity_declares_text_extraction_territory() {
        assert_eq!(affinity(ContentClass::TextNative), Fit::Preferred);
        assert_eq!(affinity(ContentClass::MixedTextScan), Fit::Capable);
        for class in [ContentClass::ScanDominant, ContentClass::NoText] {
            assert_eq!(affinity(class), Fit::Refuses, "{class}");
            assert!(!refusal_note(class).is_empty(), "{class}");
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: PdfTextConfig = toml::from_str("kind = \"pdf-text\"").unwrap();
        assert_eq!(cfg.kind, PdfTextTag::PdfText);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"pdf-text\"");
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch)
        assert!(toml::from_str::<PdfTextConfig>("kind = \"txt\"").is_err());
        // unknown field rejected
        assert!(toml::from_str::<PdfTextConfig>("kind = \"pdf-text\"\ndpi = 300").is_err());
    }
}
