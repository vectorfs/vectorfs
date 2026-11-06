//! `pdf-pages` extractor kind (M3-T3): pdfium renders every PDF page into a
//! raster bitmap — ONE image [`Part`] per page, page order preserved. This is
//! the supply side of the image chain (§5.10.1): a scanned document becomes
//! page images the siglip2 embedder (M3-T2) turns into vectors, closing
//! "PDF → 页图 → embedding" end to end.
//!
//! # Engine + binding (shared with `pdf-text`)
//!
//! Same pdfium binary, same DYNAMIC bind, same one process-global handle:
//! the library locator ([`pdf_text::locate_library`]) and the
//! process-global [`bind`](pdf_text::bind_pdfium) live in the `pdf_text`
//! module and are reused here — pdfium-render refuses a second bind anyway,
//! so two kinds sharing one handle is not a choice but a constraint, and the
//! sharing stays inside `extract/pdf_*.rs` (the DIP boundary: a pdfium swap
//! touches only these two files).
//!
//! # Render path
//!
//! * `dpi` (dots per inch, the ONLY knob) → pdfium render scale
//!   `dpi / 72` (PDF user space is 72 pt/inch; a letter page 612×792 pt at
//!   the default 150 dpi renders exactly 1275×1650 px). Default 150: the
//!   balanced digitization standard — 300 would quadruple bytes for a
//!   vision tower that resizes to a few hundred px anyway, 72 already
//!   legible for page thumbnails.
//! * each page renders through `PdfPage::render_with_config`
//!   (white background, annotations on — pdfium-render's defaults) into a
//!   `PdfBitmap`, converts to a `DynamicImage` (`PdfBitmap::as_image`, the
//!   `image_latest` feature riding the SAME image 0.25 crate the tree
//!   already declares), encodes to PNG bytes, and becomes one
//!   [`Part::image`] in PAGE ORDER — the chunker bypass numbers them
//!   `-image-0`, `-image-1`, … so page N's chunk key is stable.
//!
//! # Encoding ruling: PNG, lossless (measured, see tests)
//!
//! Page images feed a vision embedder whose preprocessing resizes/crops
//! anyway; JPEG's lossy artifacts would (a) compound with that resize and
//! (b) make re-encodes of the same source nondeterministic across encoder
//! versions — PNG keeps the pipeline's determinism contract honest
//! (`extractor_contract`: same bytes in, same Parts out). Size was measured
//! on the committed fixtures (see `render_size_reference` in the tests,
//! 2026-11-06, 1275×1650 px pages): PNG is SMALLER than JPEG on the
//! text/line-art pages that dominate real PDFs (54,488 vs 60,923 bytes on
//! the text page; deflate loves flat white + glyph edges); JPEG only wins
//! on photographic scans, where the siglip2 resize destroys more
//! information than the codec saves. PNG it is — revisit only with measured
//! evidence.
//!
//! # Guards (the MAX_CHILDREN discipline, same risk class)
//!
//! * [`MAX_RENDER_PAGES`]: pages are Parts (atomic chunks → vector rows),
//!   not ChildDocs, but the bomb shape is the same — a hostile `/Count 5000`
//!   document demanding thousands of bitmap renders from one file. The cap
//!   fires BEFORE any page work (load → count → refuse), loud, naming the
//!   count and the constant. Mechanism constant, deliberately NOT config
//!   (a knob saying how much bomb to tolerate would be a policy,
//!   §5.10.8's spirit) — same ruling as MAX_CONTAINER_DEPTH/MAX_CHILDREN.
//! * [`MAX_DPI`]/[`MIN_DPI`]: the dpi domain, validated at config parse
//!   (startup fail-fast, §5.10.3). The ceiling bounds the per-page bitmap
//!   allocation (600 dpi on a letter page ≈ 5100×6600 px ≈ 134 MiB RGBA);
//!   note the LAST word on embedding stays with the embedder's own decode
//!   budget (siglip2 refuses > 4096 px per side — letter pages above ~372
//!   dpi already exceed it; that gate fails loud per embedder, which is the
//!   modality-scoped place for it).
//!
//! # Failed-page semantics: loud Err, never skip-and-continue
//!
//! A page that fails to render Errs the WHOLE extraction with the page
//! number named. The T7 container precedent ("oversize child skipped, its
//! sibling still indexes", `run_member`) does NOT transfer: container
//! children are independently-routable documents whose skip reasons ride
//! the container's aggregate — and on total failure still fail loud;
//! extractor Parts have NO warning channel, so a skipped page inside a
//! successful extraction would be SILENT content loss (queries against
//! page N return nothing, nobody learns why). This also matches the
//! in-family twin exactly: `pdf-text`'s per-page loop Errs on the first
//! `page.text()` failure, never skips. And an Err is the chain's yield
//! signal — under `pdf = ["pdf-pages", "pdf-text"]` a render failure hands
//! the bytes to the next member instead of parking the file.
//!
//! # Contract behavior
//!
//! * empty input -> Err (uniform empty-source rule, gate.rs);
//! * mime gate: exactly `application/pdf` (or empty = unknown);
//! * corrupt/truncated bytes -> pdfium load error translated to Err, never
//!   a panic;
//! * zero-page document -> Err (same stance as pdf-text);
//! * every produced Part is `Modality::Image` with a PNG payload whose
//!   magic the chunker-bypass sniff guard accepts (canonical
//!   `image/png`).
//!
//! # Chain semantics (M2.1)
//!
//! Under an `extract` fallback chain this kind's Err or empty output IS its
//! yield signal, same vocabulary as every leaf. Its affinity declaration
//! (`Preferred` for scan-dominant, `Capable` for mixed/text-native,
//! `Refuses` for no-text) is what the router consumes — see [`affinity`].

use pdfium_render::prelude::*;

use super::super::gate;
use super::pdf_text::{self, LibrarySource};
use super::{ContentClass, Extractor, Fit, Part, PipelineError};

/// Default render resolution: 150 dots per inch (the balanced digitization
/// standard — legible text, moderate bytes; see module docs).
pub const DEFAULT_DPI: u16 = 150;

/// Accepted dpi floor: below 10 a letter page renders under 85 px tall —
/// glyphs collapse to noise and the vectors carry nothing.
const MIN_DPI: u16 = 10;

/// Accepted dpi ceiling: 600 is the print-scan fidelity ceiling; above it a
/// letter page's RGBA bitmap alone exceeds ~134 MiB and every vision
/// embedder in tree refuses the decode anyway (siglip2 DECODE_LIMIT = 4096
/// px/side). Bounds the allocation, not the quality argument.
const MAX_DPI: u16 = 600;

/// Page-count bomb guard: at most this many pages render per document —
/// checked BEFORE any page work (the MAX_CHILDREN form; see module docs).
/// 512 = 2× MAX_CHILDREN: a page Part is heavier than an average child doc
/// (one bitmap + one atomic vector row each), but the codebase's own golden
/// corpus treats a 300-page PDF as the legitimate "huge" shape, so 256 would
/// refuse honest documents this project itself generates.
const MAX_RENDER_PAGES: usize = 512;

/// The declared container mime of every part this kind emits (PNG encoding,
/// see the encoding ruling in the module docs).
pub(crate) const PAGE_MIME: &str = "image/png";

/// Runtime instance: the dpi knob plus the library-source snapshot (the
/// pdfium handle itself is process-global either way, see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfPagesExtractor {
    dpi: u16,
    source: LibrarySource,
}

impl PdfPagesExtractor {
    /// Construct from a parsed config: snapshot the library resolution NOW
    /// (deterministic per process); bind failures stay deferred to the first
    /// `extract` as an Err — construction never needs the .so present.
    pub fn new(cfg: &super::PdfPagesConfig) -> Self {
        PdfPagesExtractor {
            dpi: cfg.dpi,
            source: pdf_text::locate_library().unwrap_or(LibrarySource::System),
        }
    }
}

impl Extractor for PdfPagesExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("pdf-pages", bytes)?;
        gate::refuse_foreign_mime("pdf-pages", mime, gate::Accept::Exact("application/pdf"))?;
        let pdfium = pdf_text::bind_pdfium(&self.source)?;
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
        if page_count == 0 {
            return Err(PipelineError::extract(
                "document has no pages — nothing to extract",
            ));
        }
        // bomb guard BEFORE any page work (the child-count guard's form):
        // refuse the document outright, naming the count and the constant
        if page_count > MAX_RENDER_PAGES {
            return Err(PipelineError::extract(format!(
                "pdf-pages: document has {page_count} pages (MAX_RENDER_PAGES = \
                 {MAX_RENDER_PAGES}) — page rendering is refused before any page work; if the \
                 document is legitimate, split it into volumes",
            )));
        }
        // PDF user space is 72 pt/inch: dpi → scale factor
        let config = PdfRenderConfig::new().scale_page_by_factor(self.dpi as f32 / 72.0);
        let mut parts = Vec::with_capacity(page_count);
        for (i, page) in pages.iter().enumerate() {
            let png = page
                .render_with_config(&config)
                .and_then(|bitmap| bitmap.as_image())
                .and_then(|img| {
                    let mut buf = std::io::Cursor::new(Vec::new());
                    img.write_to(&mut buf, image::ImageFormat::Png)
                        .map_err(|_| PdfiumError::ImageError)?;
                    Ok(buf.into_inner())
                })
                .map_err(|e| {
                    // loud, page named — never skip-and-continue (module
                    // docs: a skipped page would be silent content loss)
                    PipelineError::extract(format!(
                        "pdf-pages: rendering page {} of {page_count} at dpi {} failed: {e}",
                        i + 1,
                        self.dpi
                    ))
                })?;
            parts.push(Part::image(PAGE_MIME, png));
        }
        Ok(parts)
    }
}

// ---------------------------------------------------------------------------
// affinity (M2.1 step 2): the kind's class-level routing declaration
// ---------------------------------------------------------------------------

/// Class-level affinity (see [`super::super::Extractor::affinity`]) — the
/// image-chain mirror of `pdf-text`'s text declaration:
/// * scan-dominant documents are this kind's EXACT use case — the raster IS
///   the content, page images are the only honest product (`Preferred`);
/// * mixed text+scan: renders every page faithfully (`Capable` — real
///   output, but a text member is `Preferred` where the text layer matters,
///   and a future OCR member would add what rendering alone cannot);
/// * text-native: page images are USEFUL (以图搜图 over pages, figures) but
///   strictly secondary to decoding the text layer (`Capable` — a
///   text-declaring member beats it through the Preferred tier, exactly the
///   quality-protection the router exists for);
/// * no-text (blank pages: no text layer, no raster payload): rendering
///   blank pages produces blank bitmaps — nothing to index (`Refuses`).
///
/// Static and knob-independent: `dpi` changes pixel density, never routing.
pub fn affinity(class: super::ContentClass) -> super::Fit {
    match class {
        ContentClass::ScanDominant => Fit::Preferred,
        ContentClass::MixedTextScan | ContentClass::TextNative => Fit::Capable,
        ContentClass::NoText => Fit::Refuses,
    }
}

/// One-line reason the router quotes when this kind Refuses a class (only
/// called on the Refuses arm of [`affinity`]).
pub fn refusal_note(class: super::ContentClass) -> String {
    match class {
        ContentClass::NoText => {
            "the document has no text layer and no raster payload — blank pages would render \
              to blank bitmaps"
                .to_string()
        }
        other => unreachable!("pdf-pages is {other:?}, never refuses it"),
    }
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): `kind = "pdf-pages"` plus
/// the one knob. `dpi` serializes into the fingerprint EVEN when defaulted
/// (the pdf-inspector `markdown` precedent: the knob changes chunk identity —
/// a page at 150 dpi and the same page at 300 dpi are different image bytes
/// — so its VALUE, not its presence, is canonical).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfPagesConfig {
    pub kind: PdfPagesTag,
    /// Render resolution in DOTS PER INCH (PDF user space is 72 pt/inch, so
    /// the bitmap scales as `dpi / 72` × page points; letter page at the
    /// default 150 dpi = 1275×1650 px). Accepted domain
    /// [`MIN_DPI`]..=[`MAX_DPI`] = 10..=600, validated at startup.
    #[serde(default = "default_dpi")]
    pub dpi: u16,
}

fn default_dpi() -> u16 {
    DEFAULT_DPI
}

/// Literal tag: only deserializes from `"pdf-pages"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PdfPagesTag {
    #[serde(rename = "pdf-pages")]
    PdfPages,
}

impl PdfPagesConfig {
    /// Startup domain check (§5.10.3 fail-fast): the message carries the
    /// unit and the reasoning so the operator can pick a legal value without
    /// opening the docs.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_DPI..=MAX_DPI).contains(&self.dpi) {
            return Err(format!(
                "pdf-pages dpi = {} is outside the accepted range {MIN_DPI}..={MAX_DPI} (dots \
                 per inch; the page bitmap scales as dpi/72 x page points — 150 is the balanced \
                 default, 600 the print-scan ceiling)",
                self.dpi
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// tests — all render through the REAL .so (honest-failure rule, pdf_text.rs);
// fixtures are corpus reuse: the committed hello.pdf, the pdf-inspector
// in-test builders (2-page mixed, no .so dependency of their own)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pipeline::Modality;
    use image::GenericImageView;

    /// The committed single-page fixture (same file the pdf-text and
    /// pdf-inspector suites run).
    pub(crate) const HELLO_PDF_MINIMAL: &[u8] = include_bytes!("../../../tests/golden/hello.pdf");

    /// Contract-run constructor: real .so or an actionable panic (the
    /// pdf_text::tests::extractor_or_panic twin).
    pub(crate) fn extractor_or_panic(dpi: u16) -> PdfPagesExtractor {
        match pdf_text::locate_library() {
            Ok(_) => PdfPagesExtractor::new(&cfg_of(dpi)),
            Err(m) => panic!(
                "pdf-pages tests need the real pdfium library and none was found: {m} — run \
                 scripts/fetch_runtime_libs.sh pdfium (or point VF_PDFIUM_PATH at it). Tests \
                 refuse to skip (honest-failure rule)."
            ),
        }
    }

    fn cfg_of(dpi: u16) -> super::PdfPagesConfig {
        let cfg: super::PdfPagesConfig =
            toml::from_str(&format!("kind = \"pdf-pages\"\ndpi = {dpi}")).unwrap();
        cfg
    }

    /// xref-exact N-page PDF with a SHARED empty content stream (the
    /// hand-assembly discipline of pdf_inspector::tests; sharing one content
    /// object keeps the builder O(N) bytes — the page-count guard tests need
    /// shape validity, not per-page content). `pub(crate)`: the pdf-ocr
    /// kind's page-cap test reuses the same specimen (same guard, same
    /// shape).
    pub(crate) fn blank_multipage_pdf(pages: usize) -> Vec<u8> {
        let kids = (0..pages)
            .map(|i| format!("{} 0 R", 4 + i))
            .collect::<Vec<_>>()
            .join(" ");
        let bodies: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").into_bytes(),
            b"<< /Length 0 >>\nstream\n\nendstream".to_vec(),
        ]
        .into_iter()
        .chain((0..pages).map(|_| {
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents 3 0 \
              R >>"
                .to_vec()
        }))
        .collect();
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

    /// Every part of a good extraction: image modality, PNG payload, sniff
    /// honesty (the chunker-bypass guard's own check), and decodable pixels.
    fn assert_valid_page_parts(parts: &[Part]) {
        assert!(!parts.is_empty(), "an image kind never yields empty Ok");
        for p in parts {
            assert_eq!(p.modality, Modality::Image);
            assert_eq!(p.text, "", "image parts carry an empty text channel");
            let payload = p.image.as_ref().expect("image part carries payload");
            assert_eq!(payload.mime, "image/png");
            payload
                .validate()
                .expect("declared mime matches the sniffed magic");
            assert!(
                image::load_from_memory(&payload.bytes).is_ok(),
                "payload must be a decodable PNG"
            );
        }
    }

    #[test]
    fn renders_every_page_as_an_image_part_in_page_order() {
        // the 2-page cross-engine divergence specimen: page 1 draws text,
        // page 2 a full-page image XObject — distinct rendered pixels keep
        // the part ORDER observable (part 0 is page 1, not a reshuffle)
        let bytes = crate::pipeline::extract::pdf_inspector::tests::mixed_text_image_pdf();
        let x = extractor_or_panic(150);
        let parts = x.extract(&bytes, "application/pdf").expect("2 pages");
        assert_eq!(parts.len(), 2, "one Part per page");
        assert_valid_page_parts(&parts);
        // page order: the text page and the near-blank raster page cannot
        // render to identical bytes
        assert_ne!(
            parts[0].image.as_ref().unwrap().bytes,
            parts[1].image.as_ref().unwrap().bytes,
            "part 0 must be page 1 (distinct content)"
        );
        // determinism against the real engine (PNG encode included)
        let again = x.extract(&bytes, "application/pdf").unwrap();
        assert_eq!(parts, again);
        // single-page committed fixture: exactly one part
        let one = x.extract(HELLO_PDF_MINIMAL, "application/pdf").unwrap();
        assert_eq!(one.len(), 1);
        assert_valid_page_parts(&one);
    }

    #[test]
    fn dpi_scales_pixel_dimensions() {
        // hello.pdf's MediaBox is 612x792 pt; scale = dpi/72 -> exact px at
        // 72 and 144 dpi (no rounding ambiguity)
        for (dpi, (w, h)) in [(72u16, (612u32, 792u32)), (144, (1224, 1584))] {
            let parts = extractor_or_panic(dpi)
                .extract(HELLO_PDF_MINIMAL, "application/pdf")
                .unwrap();
            let img = image::load_from_memory(&parts[0].image.as_ref().unwrap().bytes).unwrap();
            assert_eq!(
                img.dimensions(),
                (w, h),
                "dpi {dpi}: {w}x{h} expected (612x792 pt at dpi/72)"
            );
        }
    }

    #[test]
    fn page_cap_fires_before_any_page_work() {
        // the builder's shape is VALID for pdfium (3 pages render fine —
        // the refusal below is the CAP, not a parse failure) ...
        let x = extractor_or_panic(72);
        let small = x
            .extract(&blank_multipage_pdf(3), "application/pdf")
            .expect("3-page shape is valid");
        assert_eq!(small.len(), 3);
        // ... and 513 pages refuse BEFORE any rendering: the message names
        // the count and the constant (the MAX_CHILDREN form)
        let err = x
            .extract(
                &blank_multipage_pdf(MAX_RENDER_PAGES + 1),
                "application/pdf",
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("513 pages"), "{msg}");
        assert!(msg.contains("MAX_RENDER_PAGES = 512"), "{msg}");
        assert!(matches!(err, PipelineError::Extract(_)), "{err}");
    }

    #[test]
    fn empty_wrong_mime_and_corrupt_are_errors() {
        let x = extractor_or_panic(150);
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
        // zero-page document: same stance as pdf-text
        let err = x
            .extract(&blank_multipage_pdf(0), "application/pdf")
            .unwrap_err();
        assert!(err.to_string().contains("no pages"), "{err}");
    }

    #[test]
    fn affinity_declares_render_territory() {
        assert_eq!(affinity(ContentClass::ScanDominant), Fit::Preferred);
        for class in [ContentClass::MixedTextScan, ContentClass::TextNative] {
            assert_eq!(affinity(class), Fit::Capable, "{class}");
        }
        assert_eq!(affinity(ContentClass::NoText), Fit::Refuses);
        assert!(
            refusal_note(ContentClass::NoText).contains("blank"),
            "not empty either"
        );
    }

    #[test]
    fn config_roundtrip_and_domain() {
        // default: dpi absent -> 150, and the DEFAULT serializes into the
        // fingerprint (chunk identity: the pdf-inspector markdown precedent)
        let cfg: super::PdfPagesConfig = toml::from_str("kind = \"pdf-pages\"").unwrap();
        assert_eq!(cfg.dpi, DEFAULT_DPI);
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"pdf-pages\"\ndpi = 150"
        );
        assert!(cfg.validate().is_ok());
        // explicit dpi round-trips
        let cfg: super::PdfPagesConfig = toml::from_str("kind = \"pdf-pages\"\ndpi = 300").unwrap();
        assert_eq!(cfg.dpi, 300);
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"pdf-pages\"\ndpi = 300"
        );
        // domain: 0 / below-floor / above-ceiling rejected with the unit in
        // the message; both bounds themselves legal
        for dpi in [0u16, 9, 601, 60_000] {
            let cfg = cfg_of(dpi);
            let err = cfg.validate().unwrap_err();
            assert!(err.contains("dots per inch"), "{err}");
            assert!(err.contains("10..=600"), "{err}");
        }
        for dpi in [MIN_DPI, MAX_DPI] {
            assert!(cfg_of(dpi).validate().is_ok());
        }
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch);
        // non-integer dpi rejected by serde's typed parse; unknown field too
        assert!(toml::from_str::<super::PdfPagesConfig>("kind = \"pdf-text\"").is_err());
        assert!(
            toml::from_str::<super::PdfPagesConfig>("kind = \"pdf-pages\"\ndpi = \"high\"")
                .is_err()
        );
        assert!(
            toml::from_str::<super::PdfPagesConfig>("kind = \"pdf-pages\"\nformat = \"jpeg\"")
                .is_err()
        );
    }

    /// Measured encoding reference (the PNG-vs-JPEG ruling's evidence, run
    /// with --nocapture when re-adjudicating): the 2-page mixed specimen at
    /// the default dpi, PNG vs JPEG (the encoder's default quality) byte
    /// sizes per page. Text/line-art pages — the dominant real-PDF shape —
    /// deflate smaller as PNG than JPEG's DCT guess; recorded 2026-11-06:
    /// page 1 (text) 54,488 vs 60,923, page 2 (near-blank raster) 46,322 vs
    /// 58,585 — PNG ~16% smaller total AND lossless.
    #[test]
    fn render_size_reference() {
        let x = extractor_or_panic(150);
        let mixed = crate::pipeline::extract::pdf_inspector::tests::mixed_text_image_pdf();
        let png_parts = x.extract(&mixed, "application/pdf").unwrap();
        let mut png_total = 0usize;
        let mut jpeg_total = 0usize;
        for (i, p) in png_parts.iter().enumerate() {
            let img = image::load_from_memory(&p.image.as_ref().unwrap().bytes).unwrap();
            let mut jpg = std::io::Cursor::new(Vec::new());
            img.write_to(&mut jpg, image::ImageFormat::Jpeg).unwrap();
            let png_len = p.image.as_ref().unwrap().bytes.len();
            let jpeg_len = jpg.into_inner().len();
            eprintln!(
                "pdf-pages encode reference: page {} png={png_len} jpeg={jpeg_len} ({})",
                i + 1,
                img.dimensions().0
            );
            png_total += png_len;
            jpeg_total += jpeg_len;
        }
        eprintln!("pdf-pages encode reference: totals png={png_total} jpeg={jpeg_total}");
        // decode both once to prove either container would carry the pixels
        assert!(png_total > 0 && jpeg_total > 0);
    }
}
