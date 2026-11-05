//! `pdf-inspector` extractor kind: lopdf-based PDF classification + text
//! extraction via the `pdf-inspector` crate (firecrawl, MIT).
//!
//! # Why a second PDF kind (differentiation from `pdf-text`)
//!
//! * **Classification gate, fail-loud**: this kind runs the crate's detector
//!   (`detect_pdf_mem`) FIRST and refuses — with the per-page OCR REASON in
//!   the error — exactly the documents text extraction cannot serve
//!   honestly: scans (`scanned`), invisible text layers over images
//!   (`invisible_text_layer`), garbled/broken font encodings
//!   (`suspected_garbled_text`), vector-outline text (`vector_text`) and
//!   no-text pages (`no_text`). `pdf-text` only discovers most of these
//!   AFTER a full pdfium parse (and its empty-text refusal is a weaker,
//!   later signal). Fail-loud beats silently indexing an empty or wrong
//!   document; the error points at the planned M3 `pdf-ocr` kind.
//! * **Zero native libraries**: pdf-inspector is pure Rust (lopdf chain);
//!   no `libpdfium.so` to fetch/pin/ship. Deployments without the runtime
//!   libs directory get a working PDF extractor — the ops differentiation
//!   point. Trade-off (recorded honestly): lopdf is a parser, not a
//!   renderer; fidelity on exotic PDFs is below pdfium's, which is why the
//!   `auto` mapping keeps `pdf` -> `pdf-text` (see ingest/mod.rs).
//! * **Optional position-aware Markdown** (`markdown = true`): the crate's
//!   full pipeline produces heading/paragraph-aware Markdown, which RAG
//!   chunking can split on structure instead of raw position. Default
//!   `false` keeps the plain-text output shape aligned with `pdf-text`.
//!
//! # DIP boundary (§5.10.2, enforced by review)
//!
//! Every `pdf_inspector` type stays inside this module. The outside world
//! sees only [`PdfInspectorExtractor`] (plain `bool` data) and
//! [`PipelineError`].
//!
//! # Engine behavior (probed against pdf-inspector 1.25.2, asserted below)
//!
//! * `detect_pdf_mem` is DetectOnly-mode: it returns the classification
//!   (`pdf_type`, per-page `ocr_reasons_by_page`) WITHOUT extracting text.
//!   `has_encoding_issues` is always `false` in that mode — the garbled-text
//!   signal arrives through the OCR reasons instead, which is why the gate
//!   below reads reasons, not that flag.
//! * plain text (`markdown = false`): `extractor::extract_text_mem` — the
//!   crate's whole-document `extract_text` entry. NOTE: the crate exposes
//!   per-page output only as Markdown (`extract_pages_markdown_mem`); there
//!   is no per-page plain-text API, so the page join inside the string is
//!   the crate's own (pages concatenated, each page's text typically ending
//!   `\n`). The output SHAPE still matches `pdf-text`: ONE Text Part per
//!   document.
//! * markdown (`markdown = true`): `process_pdf_mem` (full pipeline). Its
//!   own `pages_needing_ocr` can flag pages the detect gate passed (a more
//!   conservative extraction-time unreliability signal — probed: it flags
//!   the hello.pdf fixture while detect does not); the gate deliberately
//!   uses the DETECT-stage classification only, so both modes accept and
//!   reject the same documents.
//! * corrupt/truncated bytes: `detect_pdf_mem` errs (`Invalid PDF
//!   structure`), translated to `PipelineError::Extract` — never a panic.
//!   Tolerated tail loss mirrors pdfium's recovery semantics (small tail
//!   cuts still classify as TextBased; the golden corpus asserts this as
//!   observed).
//! * encrypted PDFs err (`Encrypted`); empty/non-PDF bytes err (`Not a
//!   PDF: …`).
//!
//! # Chain semantics (M2.1)
//!
//! Under an `extract` fallback chain this kind's Err IS its yield signal:
//! the ingest worker hands the bytes to the next chain member (a future
//! `pdf-ocr`) instead of failing the file. Standalone (single-member
//! route) the same Err stays a plain, visible FAILED.

use pdf_inspector::{PdfType, detect_pdf_mem, process_pdf_mem};

use super::{Extractor, Modality, Part, PipelineError};

/// Guidance line every OCR-gate error carries: the reason is a
/// pdf-inspector OCR-reason constant VALUE (`scanned`,
/// `invisible_text_layer`, `suspected_garbled_text`, `vector_text`,
/// `no_text` — `OCR_REASON_*` in the crate root); the fix is the planned
/// OCR kind, not this one.
const OCR_GUIDANCE: &str = "this pdf-inspector instance extracts text-layer \
     PDFs only; scanned documents, invisible text layers and garbled \
     encodings need the pdf-ocr kind (M3) — refusing to index an empty or \
     wrong document";

/// Runtime instance: the only knob is the output flavor. Plain data — the
/// enum dispatch constructs it per call at zero cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PdfInspectorExtractor {
    /// Position-aware Markdown output instead of plain text.
    markdown: bool,
}

impl PdfInspectorExtractor {
    /// Plain-text flavor (the default, aligned with `pdf-text`).
    pub fn new() -> Self {
        PdfInspectorExtractor { markdown: false }
    }

    /// Markdown flavor (`markdown = true`).
    pub fn markdown() -> Self {
        PdfInspectorExtractor { markdown: true }
    }
}

/// Per-kind config struct (`deny_unknown_fields`): `markdown` defaults to
/// `false` (plain text, the pdf-text-aligned shape). The knob changes chunk
/// identity, so it serializes into the fingerprint even when defaulted.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfInspectorConfig {
    pub kind: PdfInspectorTag,
    /// Emit the crate's position-aware Markdown instead of plain text.
    #[serde(default)]
    pub markdown: bool,
}

/// Literal tag: only deserializes from `"pdf-inspector"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PdfInspectorTag {
    #[serde(rename = "pdf-inspector")]
    PdfInspector,
}

/// Classification gate shared by both flavors: run the detector, then refuse
/// any document with OCR-needing pages. Returns the detector result so the
/// caller's error text can quote the per-page reasons.
fn classify_or_refuse(bytes: &[u8]) -> Result<(), PipelineError> {
    let detected = detect_pdf_mem(bytes).map_err(|e| {
        PipelineError::extract(format!(
            "pdf-inspector cannot classify this document (corrupt, encrypted, or not a \
             PDF): {e}"
        ))
    })?;
    // pdf_type Scanned/ImageBased with an EMPTY page list would still be a
    // document extraction cannot serve; check both (belt and braces — probed
    // shapes always populate the list, but the enum is the stronger signal).
    let ocr_pages = &detected.pages_needing_ocr;
    if matches!(detected.pdf_type, PdfType::Scanned | PdfType::ImageBased) || !ocr_pages.is_empty()
    {
        // reasons are the crate's OCR_REASON_* constant values; quote them
        // per page so the operator sees WHY (scanned vs invisible layer vs
        // garbled) without opening the file.
        let detail = if detected.ocr_reasons_by_page.is_empty() {
            format!("pdf_type {:?}", detected.pdf_type)
        } else {
            detected
                .ocr_reasons_by_page
                .iter()
                .map(|r| format!("page {}: {}", r.page, r.reasons.join(",")))
                .collect::<Vec<_>>()
                .join("; ")
        };
        return Err(PipelineError::extract(format!(
            "pdf-inspector classification: {detail} needs OCR — {OCR_GUIDANCE}"
        )));
    }
    Ok(())
}

impl Extractor for PdfInspectorExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        if bytes.is_empty() {
            return Err(PipelineError::extract(
                "empty input (0 bytes): the pdf-inspector kind refuses empty files — the \
                 ingest worker marks the source FAILED instead of silently indexing nothing",
            ));
        }
        if !mime.is_empty() && mime != "application/pdf" {
            return Err(PipelineError::extract(format!(
                "pdf-inspector kind cannot handle mime '{mime}' (accepts application/pdf or \
                 empty/unknown); this is an extract-routing bug, not a bad file"
            )));
        }
        classify_or_refuse(bytes)?;
        let text = if self.markdown {
            // Full pipeline: detect (again, cheap) + extract + Markdown. The
            // gate already ran; the result's own conservative OCR flags are
            // deliberately NOT re-checked (see module docs).
            let result = process_pdf_mem(bytes).map_err(|e| {
                PipelineError::extract(format!("pdf-inspector markdown extraction failed: {e}"))
            })?;
            result.markdown.unwrap_or_default()
        } else {
            pdf_inspector::extractor::extract_text_mem(bytes).map_err(|e| {
                PipelineError::extract(format!("pdf-inspector text extraction failed: {e}"))
            })?
        };
        if text.trim().is_empty() {
            return Err(PipelineError::extract(
                "no extractable text (classification passed but extraction yielded nothing) — \
                 refusing to index an empty document; if the file is a scan, the pdf-ocr kind \
                 (M3) is the right tool",
            ));
        }
        Ok(vec![Part {
            modality: Modality::Text,
            text,
        }])
    }
}

// ---------------------------------------------------------------------------
// tests — pure Rust engine, no .so locator needed (the operational contrast
// with pdf_text.rs's extractor_or_panic is the point).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The committed minimal fixture (same file the pdf-text suite and the
    /// golden corpus use — corpus reuse across engines).
    pub(crate) const HELLO_PDF_MINIMAL: &[u8] = include_bytes!("../../../tests/golden/hello.pdf");

    /// Minimal single-page PDF whose page carries NO text and NO image (an
    /// empty content stream): the detector classifies it Scanned with the
    /// `no_text` OCR reason (probed) — the in-test stand-in for a scan.
    fn no_text_page_pdf() -> Vec<u8> {
        build_pdf(&[b"<< /Length 0 >>\nstream\n\nendstream".to_vec()])
    }

    /// Minimal single-page PDF whose page draws ONLY a full-page image
    /// XObject (uncompressed 1x1 RGB): the detector classifies it Scanned
    /// with the `scanned` OCR reason (probed) — a genuine raster-scan shape,
    /// built in-test, nothing committed. `pub(crate)`: the M2.1 ingest
    /// chain tests reuse it as the ALL-members-yield specimen.
    pub(crate) fn image_only_pdf() -> Vec<u8> {
        let img: &[u8] = &[0x80, 0x80, 0x80];
        let image_obj = format!(
            "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB \
             /BitsPerComponent 8 /Length {} >>\nstream\n",
            img.len()
        )
        .into_bytes();
        let content = b"q 612 0 0 792 0 0 cm /Im0 Do Q\n";
        build_pdf(&[
            {
                // image object: dict + raw samples + stream close
                let mut v = image_obj;
                v.extend_from_slice(img);
                v.extend_from_slice(b"\nendstream");
                v
            },
            format!(
                "<< /Length {} >>\nstream\n{}endstream",
                content.len(),
                String::from_utf8_lossy(content)
            )
            .into_bytes(),
        ])
    }

    /// Shared minimal-PDF builder: objects are 1 catalog, 2 pages, 3 the
    /// single page (contents = extra objects 4.., image drawn via /Im0 when
    /// `extra` starts with an image object — the caller passes fully-formed
    /// body bytes). Byte offsets and the xref table are computed exactly
    /// (same hand-assembly discipline as tests/golden_corpus.rs).
    fn build_pdf(extra: &[Vec<u8>]) -> Vec<u8> {
        let has_image = !extra.is_empty() && extra[0].starts_with(b"<< /Type /XObject");
        let resources = if has_image {
            "<< /XObject << /Im0 4 0 R >> >>"
        } else {
            "<< >>"
        };
        // content object id: 4 when an image takes 4, else 4 for content
        let content_id = if has_image { 5 } else { 4 };
        let mut bodies: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources {resources} \
                 /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        ];
        bodies.extend_from_slice(extra);
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

    /// Two-page PDF: page 1 draws text, page 2 draws ONLY a full-page image
    /// XObject — the RECORDED cross-engine divergence specimen (probed
    /// 2026-11-05): this kind classifies the document as needing OCR and
    /// refuses, while pdfium (`pdf-text`) still extracts page 1's text.
    /// That asymmetry is exactly what the M2.1 fallback chain recovers
    /// (`extract = { pdf = ["inspector", "pdf-text"] }`); the ingest chain
    /// tests consume this builder. (Tail-truncation shapes do NOT diverge
    /// on the pinned engines — both tolerate ~21 bytes of tail loss, both
    /// refuse deeper cuts — which is why the divergence fixture is the
    /// mixed text+image page, not a truncation.)
    pub(crate) fn mixed_text_image_pdf() -> Vec<u8> {
        let img: &[u8] = &[0x80, 0x80, 0x80];
        let c1 = b"BT /F1 12 Tf 72 720 Td (Hello from the text page) Tj ET\n";
        let c2 = b"q 612 0 0 792 0 0 cm /Im0 Do Q\n";
        // objects: 1 catalog, 2 pages, 3 font, 4 image, then (page,
        // contents) pairs 5..8
        let bodies: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [5 0 R 7 0 R] /Count 2 >>".to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            {
                let mut v = format!(
                    "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace \
                     /DeviceRGB /BitsPerComponent 8 /Length {} >>\nstream\n",
                    img.len()
                )
                .into_bytes();
                v.extend_from_slice(img);
                v.extend_from_slice(b"\nendstream");
                v
            },
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << \
              /F1 3 0 R >> >> /Contents 6 0 R >>"
                .to_vec(),
            format!(
                "<< /Length {} >>\nstream\n{}endstream",
                c1.len(),
                String::from_utf8_lossy(c1)
            )
            .into_bytes(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << \
              /Im0 4 0 R >> >> /Contents 8 0 R >>"
                .to_vec(),
            format!(
                "<< /Length {} >>\nstream\n{}endstream",
                c2.len(),
                String::from_utf8_lossy(c2)
            )
            .into_bytes(),
        ];
        assemble(&bodies)
    }

    /// xref-exact assembly for fully-formed object bodies the single-page
    /// [`build_pdf`] helper cannot express (same hand-assembly discipline).
    fn assemble(bodies: &[Vec<u8>]) -> Vec<u8> {
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
            &format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
                bodies.len() + 1
            )
            .into_bytes(),
        );
        out
    }

    #[test]
    fn extracts_text_from_real_pdf() {
        let parts = PdfInspectorExtractor::new()
            .extract(HELLO_PDF_MINIMAL, "application/pdf")
            .expect("fixture PDF must extract");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].modality, Modality::Text);
        assert!(
            parts[0].text.contains("Hello vectorfs"),
            "extracted text lacks the drawn string: {:?}",
            parts[0].text
        );
        // determinism against the real engine (pure parser: trivially true,
        // asserted anyway — the contract demands it)
        let again = PdfInspectorExtractor::new()
            .extract(HELLO_PDF_MINIMAL, "application/pdf")
            .unwrap();
        assert_eq!(parts, again);
    }

    #[test]
    fn markdown_flavor_emits_position_aware_markdown() {
        let parts = PdfInspectorExtractor::markdown()
            .extract(HELLO_PDF_MINIMAL, "application/pdf")
            .expect("fixture PDF must extract as markdown");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].modality, Modality::Text);
        // probed output of process_pdf_mem on this fixture: the single line
        // becomes a level-2 heading ("## Hello vectorfs\n") — the
        // position/size-aware structure plain text cannot carry
        assert!(
            parts[0].text.contains("## Hello vectorfs"),
            "markdown flavor lost the heading structure: {:?}",
            parts[0].text
        );
        // and the two flavors differ on the same input
        let plain = PdfInspectorExtractor::new()
            .extract(HELLO_PDF_MINIMAL, "application/pdf")
            .unwrap();
        assert_ne!(plain[0].text, parts[0].text);
    }

    #[test]
    fn scanned_documents_are_refused_with_ocr_reason() {
        // raster-only page: the detector's `scanned` reason (a real
        // full-page-image classification, built in-test)
        let err = PdfInspectorExtractor::new()
            .extract(&image_only_pdf(), "application/pdf")
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, PipelineError::Extract(_)), "{err}");
        assert!(msg.contains("scanned"), "reason constant missing: {msg}");
        assert!(msg.contains("pdf-ocr"), "guidance missing: {msg}");
        assert!(msg.contains("page 1"), "per-page detail missing: {msg}");
        // markdown flavor refuses identically (same gate)
        let err = PdfInspectorExtractor::markdown()
            .extract(&image_only_pdf(), "application/pdf")
            .unwrap_err();
        assert!(err.to_string().contains("scanned"), "{err}");
        // no-text page (blank): the `no_text` reason — recorded honestly:
        // this is the closest in-test stand-in for a scan WITHOUT an image
        // sample; `invisible_text_layer` / `suspected_garbled_text` /
        // `vector_text` have no cheap in-test construction and stay uncovered
        // here (the gate treats every reason uniformly)
        let err = PdfInspectorExtractor::new()
            .extract(&no_text_page_pdf(), "application/pdf")
            .unwrap_err();
        assert!(err.to_string().contains("no_text"), "{err}");
    }

    #[test]
    fn mixed_text_image_divergence_specimen_refuses_here() {
        // the inspector half of the recorded cross-engine divergence (the
        // pdfium half — page 1's text extracted — is asserted in the M2.1
        // ingest chain e2e, where the real .so is honest-failed): this kind
        // refuses the whole document because ONE page needs OCR
        let err = PdfInspectorExtractor::new()
            .extract(&mixed_text_image_pdf(), "application/pdf")
            .unwrap_err();
        assert!(matches!(err, PipelineError::Extract(_)), "{err}");
        assert!(err.to_string().contains("scanned"), "{err}");
    }

    #[test]
    fn empty_wrong_mime_and_corrupt_are_errors() {
        let x = PdfInspectorExtractor::new();
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
        // corrupt bytes with a PDF magic prefix: must Err, not panic
        let corrupt = {
            let mut v = b"%PDF-1.7\n".to_vec();
            v.extend_from_slice(&[0x00, 0xff, 0x13, 0x37, 0xde, 0xad, 0xbe, 0xef]);
            v
        };
        let err = x.extract(&corrupt, "application/pdf").unwrap_err();
        assert!(matches!(err, PipelineError::Extract(_)), "{err}");
        // non-PDF bytes at all
        let err = x
            .extract(b"plain text, no magic", "application/pdf")
            .unwrap_err();
        assert!(err.to_string().contains("cannot classify"), "{err}");
    }

    #[test]
    fn config_roundtrip() {
        // default: markdown absent -> false
        let cfg: PdfInspectorConfig = toml::from_str("kind = \"pdf-inspector\"").unwrap();
        assert!(!cfg.markdown);
        // ...and the DEFAULT serializes into the fingerprint (knob changes
        // chunk identity, so its value — not its presence — is canonical)
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"pdf-inspector\"\nmarkdown = false"
        );
        // explicit markdown
        let cfg: PdfInspectorConfig =
            toml::from_str("kind = \"pdf-inspector\"\nmarkdown = true").unwrap();
        assert!(cfg.markdown);
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"pdf-inspector\"\nmarkdown = true"
        );
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch)
        assert!(toml::from_str::<PdfInspectorConfig>("kind = \"pdf-text\"").is_err());
        // non-bool markdown rejected by serde's typed parse
        assert!(
            toml::from_str::<PdfInspectorConfig>("kind = \"pdf-inspector\"\nmarkdown = \"yes\"")
                .is_err()
        );
        // unknown field rejected
        assert!(
            toml::from_str::<PdfInspectorConfig>("kind = \"pdf-inspector\"\ndpi = 300").is_err()
        );
    }
}
