//! `docx` extractor kind (M3-T6): a WordprocessingML document file becomes
//! ONE text [`Part`] — the paragraphs of `word/document.xml`, in document
//! order, joined `"\n\n"`.
//!
//! # Leaf kind — the §0 criterion, decided BEFORE the implementation
//!
//! The §0 normalize/container ruling (design.md §0, 2026-11-05:
//! 「判据：机械恒对→normalizer（机制不可配）；格式智力/用户可选→kind」,
//! 容器角色「子文档重回用户配置的路由，容器永不自消化子文档」) asks one
//! question of every format: are the INSIDES independently routable USER
//! documents? For docx the answer is NO: `word/document.xml` is an INTERNAL
//! serialization detail of ONE document (the OOXML spec calls the package a
//! document, not a container of documents; headers, styles and the manifest
//! are parts OF it, none makes sense alone). The §5.10.2 vocabulary for
//! that shape is a LEAF kind that self-digests the format intelligence —
//! the zip/XML knowledge lives here, inside the kind. Contrast the T7 epub
//! CONTAINER kind: an epub's XHTML chapters ARE independently routable
//! documents (each carries `text/html` semantics a user's html chain
//! expresses an opinion about), so epub `unpack`s and its children RE-ENTER
//! user routing — the same §0 text, the opposite verdict, on document
//! shape alone.
//!
//! # One Part, paragraphs joined `"\n\n"` — the Part-formation ruling
//!
//! Both existing multi-block text kinds emit ONE Part (pdf-text joins page
//! texts `"\n\n"`, md joins blocks `"\n"`), and the reason is chunker
//! semantics, not style: the token-window recurrence windows ONE char
//! sequence with semantic cuts at `"\n\n"` paragraph boundaries, so a
//! single Part lets windows SPAN paragraphs with the configured overlap.
//! One-Part-per-paragraph would instead hand the chunker N independent
//! sequences — every short paragraph would become its own undersized
//! chunk with ZERO cross-paragraph overlap, silently degrading retrieval
//! context (and the markdown chunker's heading cut points need contiguous
//! section text for the same reason). Paragraph structure is preserved
//! by the `"\n\n"` separators the cut oracles already understand.
//!
//! # Extraction shape (what the WordprocessingML reader keeps)
//!
//! * `w:p` delimits paragraphs (the module-doc separator above);
//! * `w:t` contributes text (0.41's reader splits entities into separate
//!   `GeneralRef` events — text events themselves never contain `&`); the
//!   five predefined entities plus numeric character references resolve,
//!   anything else is a corrupt file and errs loudly (strict-XML posture);
//! * `w:tab` → `'\t'`, `w:br`/`w:cr` → `'\n'` (line breaks inside a
//!   paragraph stay inside it);
//! * `w:instrText` (field CODES — TOC/PAGEREF instructions) is skipped: it
//!   is machinery, not content;
//! * `w:delText` is naturally excluded (only `w:t` is read — tracked-changes
//!   DELETED text does not re-enter the corpus);
//! * scope is the MAIN BODY only: `word/document.xml`. Headers/footers/
//!   footnotes (`header*.xml`, `footnotes.xml`) are boilerplate that repeats
//!   on every page — indexing them would multiply boilerplate over content;
//!   a kind that wants them is a future knob's problem, not a guess here.
//!
//! # Guards (the MAX_CHILDREN container-constant discipline)
//!
//! * [`MAX_ZIP_ENTRIES`] = 256: the MAX_CHILDREN form — an entry-count
//!   bomb (a zip of a million names) refuses BEFORE any entry is read.
//! * [`MAX_XML_BYTES`] = 256 MiB decompressed: the amplification gate AT
//!   THE EXPANSION POINT (§5.10.3 ruling ③ — the decompression bomb guard
//!   sits at the transform's exit, before any downstream allocation). A
//!   real document.xml is single-digit MiB; 256 MiB is 2× the default
//!   `max_file_size_mb` headroom while bounding the zip bomb.
//! * corrupt zip / not-a-zip / missing `word/document.xml` / XML that does
//!   not parse ⇒ loud Err (a truncated upload must FAIL the source, never
//!   index a silent half document); zero extractable text ⇒ Err (the
//!   uniform empty-output refusal every text kind shares).
//!
//! # Affinity: deliberately undeclared
//!
//! The [`ContentClass`] vocabulary is PDF-shaped (its probe covers the pdf
//! family only) — a docx source probes as Unknown and the router never
//! consults affinity for it (the image-kind precedent, M3-T4: families
//! without a probe declare nothing; config order is the whole truth).
//!
//! # Contract behavior
//!
//! Empty input → Err; wrong mime → Err (exact gate on the OOXML mime);
//! empty mime tolerated; truncated bytes → Err (zip framing is
//! self-describing); output is exactly ONE `Modality::Text` Part.

use super::super::gate;
use super::{Extractor, Modality, Part, PipelineError};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::io::Read;

/// The one OOXML part this kind reads (module docs: main body only).
const DOCUMENT_XML: &str = "word/document.xml";

/// Entry-count bomb guard (the MAX_CHILDREN form — see module docs).
const MAX_ZIP_ENTRIES: usize = 256;

/// Decompressed-size ceiling for `word/document.xml` (the amplification
/// gate at the expansion point, §5.10.3 ruling ③ — see module docs).
const MAX_XML_BYTES: u64 = 256 * 1024 * 1024;

/// Runtime instance (configless — the kind has no knobs; the format
/// intelligence IS the kind, §0's criterion).
#[derive(Debug, Clone, Copy, Default)]
pub struct DocxExtractor;

impl Extractor for DocxExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("docx", bytes)?;
        gate::refuse_foreign_mime(
            "docx",
            mime,
            gate::Accept::Exact(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
        )?;
        let xml = read_document_xml(bytes)?;
        let text = document_xml_to_text(&xml)?;
        if text.trim().is_empty() {
            return Err(PipelineError::extract(
                "docx: the document contains no extractable text (no w:p/w:t content in \
                 word/document.xml) — nothing to index",
            ));
        }
        Ok(vec![Part {
            modality: Modality::Text,
            text,
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
        }])
    }
}

/// Unzip `bytes` and return the raw `word/document.xml` content. The entry
/// count is checked before ANY entry is opened (the MAX_CHILDREN form);
/// the decompressed size is capped while reading (the amplification gate).
fn read_document_xml(bytes: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        PipelineError::extract(format!(
            "docx: cannot open as a zip archive (corrupt upload, truncated transfer, or not a \
             .docx file): {e}"
        ))
    })?;
    if zip.len() > MAX_ZIP_ENTRIES {
        return Err(PipelineError::extract(format!(
            "docx: the package lists {} entries (MAX_ZIP_ENTRIES = {MAX_ZIP_ENTRIES}) — refused \
             before reading any entry; if this is a legitimate document, split it",
            zip.len()
        )));
    }
    let mut entry = zip.by_name(DOCUMENT_XML).map_err(|_| {
        PipelineError::extract(format!(
            "docx: the package has no '{DOCUMENT_XML}' part — a docx without a main \
                 document body is not a document this pipeline can index"
        ))
    })?;
    // entry sizes in a zip header are advisory (a bomb lies about them) —
    // cap while READING, not before
    let mut out = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut read: u64 = 0;
    loop {
        let n = entry.read(&mut chunk).map_err(|e| {
            PipelineError::extract(format!(
                "docx: reading '{DOCUMENT_XML}' failed (corrupt deflate stream): {e}"
            ))
        })?;
        if n == 0 {
            return Ok(out);
        }
        read += n as u64;
        if read > MAX_XML_BYTES {
            return Err(PipelineError::extract(format!(
                "docx: '{DOCUMENT_XML}' decompresses past {MAX_XML_BYTES} bytes — a \
                 decompression bomb is refused at the expansion point (§5.10.3); if this is a \
                 legitimate document, split it"
            )));
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

/// Pull-event walk of document.xml (module docs: the extraction shape).
/// quick-xml's reader defaults keep end-name checking ON: mismatched tags
/// are a corrupt document.xml and err loudly here.
fn document_xml_to_text(xml: &[u8]) -> Result<String, PipelineError> {
    let xml = std::str::from_utf8(xml).map_err(|e| {
        PipelineError::extract(format!(
            "docx: word/document.xml is not valid UTF-8 (OOXML requires UTF-8): {e}"
        ))
    })?;
    let mut reader = Reader::from_str(xml);
    let mut paragraphs: Vec<String> = Vec::new();
    let mut paragraph = String::new();
    let mut depth_w_p = 0usize; // inside <w:p> (a textbox nests w:p in a run)
    let mut depth_instr = 0usize; // inside <w:instrText> (field codes: skip)
    let mut buf = Vec::new();
    // WordprocessingML carries its prefix on every element (w:p) or none at
    // all; the LOCAL name is what identifies the element, and no two
    // namespaces in document.xml use the local names below — byte equality
    // against both spellings is exact AND allocation-free in this hot loop
    let is =
        |qname: &[u8], local: &[u8]| qname == local || qname.strip_prefix(b"w:") == Some(local);
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|e| {
            PipelineError::extract(format!(
                "docx: word/document.xml does not parse as XML (corrupt document): {e}"
            ))
        })?;
        match event {
            Event::Start(e) => {
                let name = e.name().into_inner();
                if is(name, b"p") {
                    depth_w_p += 1;
                } else if is(name, b"instrText") {
                    depth_instr += 1;
                }
            }
            Event::End(e) => {
                let name = e.name().into_inner();
                if is(name, b"p") {
                    // a paragraph closes: flush if it carried text (empty
                    // paragraphs are layout, not content)
                    if depth_w_p == 1 && !paragraph.is_empty() {
                        paragraphs.push(std::mem::take(&mut paragraph));
                    }
                    depth_w_p = depth_w_p.saturating_sub(1);
                } else if is(name, b"instrText") {
                    depth_instr = depth_instr.saturating_sub(1);
                }
            }
            Event::Empty(e) => {
                if depth_w_p > 0 && depth_instr == 0 {
                    let name = e.name().into_inner();
                    if is(name, b"tab") {
                        paragraph.push('\t');
                    } else if is(name, b"br") || is(name, b"cr") {
                        paragraph.push('\n');
                    }
                }
            }
            Event::Text(t) => {
                if depth_w_p > 0 && depth_instr == 0 {
                    let text = t.decode().map_err(|e| {
                        PipelineError::extract(format!(
                            "docx: text node in word/document.xml does not decode: {e}"
                        ))
                    })?;
                    paragraph.push_str(&text);
                }
            }
            // 0.41: entities are separate events; the reader's Text events
            // never contain '&'. Resolve the five predefined entities and
            // numeric character references; a NAMED entity beyond those is
            // invalid in strict OOXML XML and errs loudly.
            Event::GeneralRef(r) => {
                if depth_w_p > 0 && depth_instr == 0 {
                    let reference = format!("&{};", String::from_utf8_lossy(&r));
                    let resolved = quick_xml::escape::unescape(&reference).map_err(|e| {
                        PipelineError::extract(format!(
                            "docx: word/document.xml references entity '{reference}', which is \
                             not one of the predefined XML entities (OOXML is strict XML): {e}"
                        ))
                    })?;
                    paragraph.push_str(&resolved);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    // the final paragraph (a document whose body never closes w:p —
    // tolerate-and-keep rather than drop content)
    if !paragraph.is_empty() {
        paragraphs.push(paragraph);
    }
    Ok(paragraphs.join("\n\n"))
}

// ---------------------------------------------------------------------------
// config (no knobs — the tag is the whole section)
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): the typed `kind` tag
/// doubles as the allowed-field list in unknown-key errors and as the
/// fingerprint payload (canonical form `kind = "docx"`, knob-free).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocxConfig {
    pub kind: DocxTag,
}

/// Literal tag: only deserializes from `"docx"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DocxTag {
    #[serde(rename = "docx")]
    Docx,
}

// ---------------------------------------------------------------------------
// tests — in-test docx builders (the pdf-pages discipline: no committed
// binary blobs in unit tests; the zip structure is built with the same zip
// crate, solid paragraphs keep every assertion exact). The COMMITTED
// e2e fixture lives under tests/fixtures/text-face/ (a real .docx written
// once, read by the worker-level tests).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// The kind's canonical mime (the registry's docx row), shared with the
    /// mod.rs registry-sweep tables so the long literal lives once.
    pub(crate) const DOCX_MIME: &str =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

    /// A leaked `&'static` docx for case tables that demand 'static bytes
    /// (the mod.rs registry sweep; the image kind's `Box::leak` precedent).
    pub(crate) fn static_docx() -> &'static [u8] {
        Box::leak(minimal_docx(&["reg", "istry"]).into_boxed_slice())
    }

    /// Build a real minimal .docx (OOXML package: [Content_Types].xml +
    /// word/document.xml, deflate entries) around `paragraphs`.
    pub(crate) fn minimal_docx(paragraphs: &[&str]) -> Vec<u8> {
        let body = paragraphs
            .iter()
            .map(|p| {
                format!(
                    "<w:p><w:r><w:t>{}</w:t></w:r></w:p>",
                    p.replace('&', "&amp;").replace('<', "&lt;")
                )
            })
            .collect::<String>();
        let document = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
             <w:body>{body}</w:body></w:document>"
        );
        let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
             <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
             <Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.\
             openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(content_types.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn paragraphs_become_one_text_part_double_newline_joined() {
        let docx = minimal_docx(&["第一段：汉字内容。", "Second paragraph with detail."]);
        let parts = DocxExtractor
            .extract(
                &docx,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap();
        assert_eq!(parts.len(), 1, "ONE Part (the Part-formation ruling)");
        assert_eq!(parts[0].modality, Modality::Text);
        assert_eq!(
            parts[0].text,
            "第一段：汉字内容。\n\nSecond paragraph with detail."
        );
        // determinism (the contract's first assertion)
        assert_eq!(
            DocxExtractor
                .extract(
                    &docx,
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                )
                .unwrap(),
            parts
        );
    }

    #[test]
    fn runs_tabs_breaks_and_entities_assemble_per_paragraph() {
        // one paragraph of three runs with a tab and an entity; a second
        // paragraph with a <w:br/> line break inside it
        let document = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
            <w:body>\
              <w:p>\
                <w:r><w:t> caf&#233;</w:t></w:r>\
                <w:r><w:tab/></w:r>\
                <w:r><w:t>a &amp; b</w:t></w:r>\
              </w:p>\
              <w:p><w:r><w:t>line1</w:t></w:r><w:r><w:br/></w:r><w:r><w:t>line2</w:t></w:r></w:p>\
            </w:body></w:document>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        let docx = zip.finish().unwrap().into_inner();
        let parts = DocxExtractor
            .extract(
                &docx,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap();
        assert_eq!(parts[0].text, " café\ta & b\n\nline1\nline2");
    }

    #[test]
    fn instr_text_field_codes_are_skipped() {
        let document = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
            <w:body>\
              <w:p>\
                <w:r><w:instrText> TOC \\o \"1-3\" </w:instrText></w:r>\
                <w:r><w:t>real content</w:t></w:r>\
              </w:p>\
            </w:body></w:document>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        let docx = zip.finish().unwrap().into_inner();
        let parts = DocxExtractor
            .extract(
                &docx,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap();
        assert_eq!(parts[0].text, "real content", "field codes are machinery");
    }

    #[test]
    fn corrupt_not_docx_and_truncated_are_loud_errors() {
        // not a zip at all
        let err = DocxExtractor
            .extract(
                b"plainly not a zip",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot open as a zip archive"),
            "{err}"
        );
        // a zip WITHOUT word/document.xml (an xlsx shape, say)
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("xl/workbook.xml", opts).unwrap();
        zip.write_all(b"<workbook/>").unwrap();
        let not_docx = zip.finish().unwrap().into_inner();
        let err = DocxExtractor
            .extract(
                &not_docx,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("no 'word/document.xml' part"),
            "{err}"
        );
        // truncated REAL docx: byte-half cuts the zip's central directory
        let docx = minimal_docx(&["some content here"]);
        let err = DocxExtractor
            .extract(
                &docx[..docx.len() / 2],
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot open as a zip archive"),
            "truncated zip framing must be detected: {err}"
        );
        // document.xml that does not parse (mismatched tags: reader default
        // end-name checking catches it)
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(b"<w:document><w:body><w:p>broken</w:x></w:body>")
            .unwrap();
        let broken = zip.finish().unwrap().into_inner();
        let err = DocxExtractor
            .extract(
                &broken,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        assert!(err.to_string().contains("does not parse as XML"), "{err}");
        // empty input + wrong mime (the uniform gates)
        let err = DocxExtractor
            .extract(
                b"",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        let docx = minimal_docx(&["x"]);
        let err = DocxExtractor.extract(&docx, "application/pdf").unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        assert!(
            DocxExtractor.extract(&docx, "").is_ok(),
            "empty mime tolerated"
        );
    }

    #[test]
    fn no_extractable_text_is_an_error() {
        // a well-formed document.xml whose body has no w:t (a page of empty
        // paragraphs is layout, not content)
        let document = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
            <w:body><w:p/><w:p><w:r><w:br/></w:r></w:p></w:body></w:document>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        let docx = zip.finish().unwrap().into_inner();
        let err = DocxExtractor
            .extract(
                &docx,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        assert!(err.to_string().contains("no extractable text"), "{err}");
    }

    #[test]
    fn entry_count_bomb_refused_before_any_read() {
        // MAX_ZIP_ENTRIES+1 entries of nothing: the count guard fires on
        // the package listing, before any entry decompresses
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for i in 0..=MAX_ZIP_ENTRIES {
            zip.start_file(format!("junk/{i}"), opts).unwrap();
            zip.write_all(b"x").unwrap();
        }
        let bomb = zip.finish().unwrap().into_inner();
        let err = DocxExtractor
            .extract(
                &bomb,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("MAX_ZIP_ENTRIES = {MAX_ZIP_ENTRIES}")),
            "{msg}"
        );
    }

    #[test]
    fn affinity_is_the_undeclared_default() {
        // no declaration (module docs): Capable for EVERY class — the
        // ContentClass vocabulary is PDF-shaped, a docx source probes
        // Unknown and the router never consults this
        use crate::pipeline::{ContentClass, Fit};
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Extractor::affinity(&DocxExtractor, class), Fit::Capable);
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: DocxConfig = toml::from_str("kind = \"docx\"").unwrap();
        assert_eq!(cfg.kind, DocxTag::Docx);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"docx\"");
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch);
        // unknown field rejected (no knobs)
        assert!(toml::from_str::<DocxConfig>("kind = \"txt\"").is_err());
        assert!(toml::from_str::<DocxConfig>("kind = \"docx\"\ndpi = 300").is_err());
    }
}
