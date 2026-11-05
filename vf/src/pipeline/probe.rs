//! Content-classification probe (M2.1 step 2: probe → affinity → route).
//!
//! The FIRST half of smart routing: turn one source's raw bytes into a
//! [`ContentClass`] BEFORE any extractor runs, so the router can order the
//! chain by DECLARED fit instead of discovering each member's suitability by
//! burning an attempt on it. This closes the two blind spots of the pure
//! linear chain:
//!
//! * **Mis-order is a quality accident**: a chain that puts an OCR member
//!   before a text member would OCR native text — strictly worse output that
//!   the chain would then ACCEPT (first non-empty Ok wins). A class probe
//!   makes the order content-derived, not config-accidental.
//! * **Classification knowledge was thrown away**: `pdf-inspector`'s detect
//!   stage already KNEW a document was a scan, but as an extractor its verdict
//!   was stringified into an Err the mechanism could not see — the chain could
//!   only blind-try. The probe lifts that verdict into a machine value.
//!
//! # Contract
//!
//! [`probe`] is a pure function of `(short_key, bytes)`; `None` means
//! UNKNOWN — deliberately conflating "family has no probe" (md/txt today)
//! with "the pdf probe errored" (corrupt/encrypted/not-parseable). Unknown is
//! not a failure: the router falls back to the config-ordered full chain,
//! which preserves the M2.1 cross-parser recovery property (lopdf refusing
//! bytes pdfium can still open must not park the file — the probe's Err is
//! exactly that shape, so it MUST read as Unknown, not as a verdict).
//!
//! # DIP boundary
//!
//! The `pdf_inspector` crate's types appear here and in
//! `extract/pdf_inspector.rs` only; the rest of the pipeline sees
//! [`ContentClass`]. Swapping the probe engine touches this module alone.
//!
//! # Probed mapping reality (pdf-inspector 1.25.2, asserted below)
//!
//! | specimen | `pdf_type` | pages/reasons | class |
//! |---|---|---|---|
//! | hello.pdf (drawn text) | TextBased | — | text-native |
//! | full-page image only | Scanned | 1: scanned | scan-dominant |
//! | text page + image page (1 text op) | **ImageBased** | 1: scanned, 2: scanned | mixed-text-scan |
//! | text-heavy page + image page | Mixed | 2: scanned | mixed-text-scan |
//! | blank page (empty content) | Scanned | 1: no_text | no-text |
//! | corrupt bytes | Err | — | None (unknown) |
//!
//! Two mapping rulings fall out of the probe (recorded honestly, not from
//! the crate's docstrings):
//!
//! * `ImageBased` maps to [`MixedTextScan`], not ScanDominant: the
//!   classifier emits it for documents whose text pages sit below its
//!   per-page text-op threshold — pdfium still extracts real text from them
//!   (the committed mixed specimen IS this shape), so pdf-text must stay
//!   Capable there. `Scanned` alone is the unambiguous raster verdict.
//! * an all-pages-`no_text` document maps to [`NoText`] regardless of
//!   `pdf_type`: blank pages have nothing to extract AND nothing to OCR —
//!   a different failure message than a scan's.

use pdf_inspector::{PdfType, detect_pdf_mem};

use super::ContentClass;

/// Classify one source's bytes for routing. `None` = Unknown (no probe for
/// the family, or the probe itself cannot parse the bytes) — the router then
/// keeps the pure config order (M2.1 semantics).
///
/// Sync + allocation-light on purpose: the worker calls it INSIDE the same
/// `spawn_blocking` CPU segment that runs extraction, before routing — no
/// separate task, no concurrency (§5.10.5), and single-member chains skip it
/// entirely (`members.len() > 1` is the worker's zero-cost gate).
pub fn probe(short_key: &str, bytes: &[u8]) -> Option<ContentClass> {
    if short_key != "pdf" {
        // No probe exists for other families in M2 (md/txt are text-native
        // by construction — decoding IS the classification). M3 image-family
        // probes plug in here as new short-key arms.
        return None;
    }
    // probe Err (corrupt / encrypted / not a PDF) IS Unknown: a verdict here
    // would park bytes another parser might still recover (cross-parser
    // recovery, M2.1's flagship property, must survive routing).
    let detected = detect_pdf_mem(bytes).ok()?;

    // no-text correction FIRST (it overrides the pdf_type verdict): a
    // document whose every page's only reason is `no_text` is blank — no text
    // layer to extract, no raster to OCR. Reasons are the crate's
    // OCR_REASON_* constant VALUES; compared literally, no re-derivation.
    if detected.page_count > 0 {
        let all_pages_blank = (1..=detected.page_count).all(|page| {
            detected.ocr_reasons_by_page.iter().any(|r| {
                r.page == page
                    && r.reasons.len() == 1
                    && r.reasons[0] == pdf_inspector::OCR_REASON_NO_TEXT
            })
        });
        if all_pages_blank {
            return Some(ContentClass::NoText);
        }
    }

    match detected.pdf_type {
        PdfType::TextBased => Some(ContentClass::TextNative),
        PdfType::Scanned => Some(ContentClass::ScanDominant),
        // ImageBased: text pages below the classifier's per-page threshold —
        // pdfium still extracts them (probed on the committed specimen), so
        // this is mixed-family, NOT unambiguous-scan. See module docs.
        PdfType::ImageBased | PdfType::Mixed => Some(ContentClass::MixedTextScan),
    }
}

// ---------------------------------------------------------------------------
// tests — hermetic: committed fixture + in-test assembled PDFs (same
// hand-assembly discipline as extract/pdf_inspector.rs; the classifier is
// pure Rust, no .so needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Corrupt bytes with a PDF magic prefix (the parse-failure specimen the
    /// extractor suites use): the probe must answer Unknown, never panic.
    fn corrupt_pdf() -> Vec<u8> {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(&[0x00, 0xff, 0x13, 0x37, 0xde, 0xad, 0xbe, 0xef]);
        v
    }

    #[test]
    fn classifies_each_content_shape() {
        // text-native: the committed hello fixture (drawn text, TextBased)
        assert_eq!(
            probe(
                "pdf",
                crate::pipeline::extract::pdf_inspector::tests::HELLO_PDF_MINIMAL
            ),
            Some(ContentClass::TextNative)
        );
        // scan-dominant: full-page raster image (Scanned + `scanned` reason)
        assert_eq!(
            probe(
                "pdf",
                &crate::pipeline::extract::pdf_inspector::tests::image_only_pdf()
            ),
            Some(ContentClass::ScanDominant)
        );
        // mixed-text-scan: the committed cross-engine divergence specimen
        // (probes ImageBased — text page under the classifier's op
        // threshold, page of raster; see module docs) ...
        assert_eq!(
            probe(
                "pdf",
                &crate::pipeline::extract::pdf_inspector::tests::mixed_text_image_pdf()
            ),
            Some(ContentClass::MixedTextScan)
        );
        // ... and the text-heavy variant the classifier names Mixed
        assert_eq!(
            probe(
                "pdf",
                &crate::pipeline::extract::pdf_inspector::tests::text_heavy_mixed_pdf()
            ),
            Some(ContentClass::MixedTextScan)
        );
        // no-text: blank page (empty content stream; probes Scanned with the
        // `no_text` reason on every page — corrected to NoText)
        assert_eq!(
            probe(
                "pdf",
                &crate::pipeline::extract::pdf_inspector::tests::no_text_page_pdf()
            ),
            Some(ContentClass::NoText)
        );
    }

    #[test]
    fn probe_error_is_unknown_not_a_verdict() {
        // corrupt bytes: detect errs -> None (Unknown). This is the property
        // that keeps cross-parser recovery alive under routing: bytes lopdf
        // cannot even CLASSIFY still fall to the config-ordered chain where
        // pdfium may open them.
        assert_eq!(probe("pdf", &corrupt_pdf()), None);
        // non-PDF bytes with a pdf short key: equally Unknown
        assert_eq!(probe("pdf", b"plain text, no magic"), None);
    }

    #[test]
    fn families_without_probes_read_unknown() {
        // md/txt (and anything unknown): no probe exists, Unknown by
        // construction — even bytes that happen to BE a PDF.
        for key in ["md", "txt", "default"] {
            assert_eq!(
                probe(
                    key,
                    crate::pipeline::extract::pdf_inspector::tests::HELLO_PDF_MINIMAL
                ),
                None,
                "short key {key} has no probe in M2"
            );
        }
    }
}
