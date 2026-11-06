//! Extractor axis: closed-enum registration + dispatch (M2 T2 skeleton,
//! kinds landed through M2 T4; `pdf-pages` in M3-T3).
//!
//! `dyn`-free by design (§5.10.2 v2.1): the enum below is the registry.
//! Adding a kind = adding a variant + a `from_config` arm; the compiler then
//! walks every match site in this tree. Available kinds: `txt`,
//! `pdf-text`, `md`, `pdf-inspector` (the first kind added AFTER the M2
//! set — the closed-set mechanism's first live exercise; two PDF kinds
//! coexist behind the one `pdf` short key, selection is explicit),
//! `pdf-pages` (M3-T3, the third PDF kind and the first IMAGE-output kind:
//! pdfium renders each page to a PNG Part — same short key again, the
//! two-kinds-one-key precedent extended to a text/image pair), `pdf-ocr`
//! (M3-T10, the fourth PDF kind: PaddleOCR PP-OCRv5 mobile det+rec over
//! the tree's own ort sessions — renders each page at its own dpi, OCRs
//! det→rec, and emits one TEXT Part per page in page order; the scan route
//! of the `scanned-docs` preset's `[pdf-text, pdf-ocr]` chain),
//! `image`
//! (M3-T4, the image FILE kind: one verbatim image Part behind the new
//! `img` short key — the first family key carried by TWO mime rows,
//! jpeg + png), `docx` + `html` (M3-T6, the text face: the Word
//! document kind behind its own `docx` key — a LEAF per the §0 criterion,
//! see extract/docx.rs — and the HTML document kind behind `html`,
//! html5ever under the hood per the selection note in extract/html.rs; the
//! html chain is also T7's epub-chapter route by construction, zero
//! special-casing either side), and `epub` + `zip` + `eml` (M3-T7, the
//! first PRODUCTION container kinds: `unpack` is their one product path,
//! children re-enter user routing — epub's format intelligence is the OPF
//! spine ordering, zip's is the extension→mime inference, eml's is the
//! MIME-multipart walk; see each kind module for its ruling).
//! Since M2.1 step 2 every kind also declares its per-[`ContentClass`]
//! affinity (kind module `affinity`/`refusal_note` free functions, dispatched
//! exhaustively below) — the ingest router consumes those declarations to
//! order extract chains by content class.
//!
//! # mime <-> short-key table (T4)
//!
//! Ingest sections route by mime SHORT KEY (`extract = { pdf = "pdf" }`,
//! §5.10.4); the `auto` mapping T3 installs resolves real mime types through
//! the one table below so kind names, config keys and mime gates can never
//! drift apart (single source of truth, pub for the T3 ingest worker).

pub mod docx;
pub mod eml;
pub mod epub;
pub mod html;
pub mod image;
pub mod md;
pub mod pdf_inspector;
pub mod pdf_ocr;
pub mod pdf_pages;
pub mod pdf_text;
pub mod txt;
pub mod zip;

use super::{ChildDoc, ContentClass, Extractor, Fit, Modality, Part, PipelineError};
use docx::{DocxConfig, DocxExtractor};
use eml::{EmlConfig, EmlExtractor};
use epub::{EpubConfig, EpubExtractor};
use html::{HtmlConfig, HtmlExtractor};
use image::{ImageConfig, ImageExtractor};
use md::{MdConfig, MdExtractor};
use pdf_inspector::{PdfInspectorConfig, PdfInspectorExtractor};
use pdf_ocr::{PdfOcrConfig, PdfOcrExtractor};
use pdf_pages::{PdfPagesConfig, PdfPagesExtractor};
use pdf_text::{PdfTextConfig, PdfTextExtractor};
use txt::{TxtConfig, TxtExtractor};
use zip::{ZipConfig, ZipExtractor};

/// Canonical mime -> config short key, one row per declared mime. ORDER
/// MATTERS only for human eyes — EXCEPT that a family with SEVERAL mimes
/// (`img`: jpeg + png, M3-T4) must list its CANONICAL representative first:
/// [`mime_for_short_key`] returns the first row of the key, and that mime is
/// what the reporting face (`GetSource`/`GetSourceData`) shows for the
/// whole family (routing itself is per-object and extension-exact). The
/// table is the single mapping both directions consult.
///
/// The M3-T7 rows (`epub`/`zip`/`eml`) are CONTAINER families: their kinds
/// `unpack`, and `auto` deliberately does NOT claim them (the
/// img/docx/html exemption ruling — an auto arm would make every
/// `default = "auto"` deployment start expanding archives overnight).
pub const MIME_TO_SHORT_KEY: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/epub+zip", "epub"),
    ("application/zip", "zip"),
    ("message/rfc822", "eml"),
    ("image/jpeg", "img"),
    ("image/png", "img"),
    ("text/html", "html"),
    ("text/markdown", "md"),
    ("text/plain", "txt"),
];

/// The available-kind list embedded in unknown-kind errors. Derived view of
/// [`KIND_LABELS`] (the parity test pins the two together — a new kind that
/// updates the enum but forgets the string fails there, not in a user's
/// error message).
pub const EXTRACTOR_KINDS: &str =
    "txt, pdf-text, md, pdf-inspector, pdf-pages, pdf-ocr, image, docx, html, epub, zip, eml";

/// Every production kind's config label, IN AVAILABILITY-LIST ORDER. This is
/// the registry's identity anchor: `kind_label` (the exhaustive match the
/// compiler walks on a new variant) must agree with this table row for row
/// (the `kind_registry_views_agree` test walks it). A new kind lands here,
/// in `kind_label`, and in `from_config` in one diff — everything else in
/// this module is exhaustive dispatch the compiler already checks.
pub const KIND_LABELS: &[&str] = &[
    "txt",
    "pdf-text",
    "md",
    "pdf-inspector",
    "pdf-pages",
    "pdf-ocr",
    "image",
    "docx",
    "html",
    "epub",
    "zip",
    "eml",
];

/// mime (parameters like `; charset=utf-8` tolerated) -> short key.
pub fn short_key_for_mime(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or("").trim();
    MIME_TO_SHORT_KEY
        .iter()
        .find(|(m, _)| *m == base)
        .map(|(_, key)| *key)
}

/// short key -> canonical mime (the inverse, MANY-TO-ONE since M3-T4: the
/// `img` family lists jpeg + png, and this returns the family's canonical
/// representative — the FIRST row of the key, the honest answer a
/// short-key-only consumer can give; per-object mimes come from the
/// extension/bytes at the caller that has them. `default`/`auto` are
/// ingest RESERVED words, never kind keys, and are absent here on purpose).
pub fn mime_for_short_key(key: &str) -> Option<&'static str> {
    MIME_TO_SHORT_KEY
        .iter()
        .find(|(_, k)| *k == key)
        .map(|(mime, _)| *mime)
}

/// file-name EXTENSION -> short key, the extension-side view of the same
/// one table. Two consumers must never drift apart, which is why this
/// lives HERE and not in each caller: the ingest scanner routes an object
/// key by its extension (after the normalize stage's
/// [`unwrap_compression`](crate::pipeline::normalize::unwrap_compression)
/// sees through one compression wrapper), and the gzip normalizer infers
/// the POST-decompression mime from the wrapper-stripped hint name —
/// `x.txt.gz` must claim and route as the same text/plain source it
/// decompresses into. Extension ALIASES live beside their canonical key
/// (`markdown`/`text`); test scaffolding keys (the cfg(test) `ctn`) stay
/// in the scanner, which owns claiming. Since the plugin-axis hardening
/// the lookup is TABLE-driven (like the normalizer TABLE) instead of a
/// hand-written match: one const table, and the
/// `mime_table_extension_and_auto_views_agree` test pins every family row
/// of [`MIME_TO_SHORT_KEY`] to an alias row here AND to a
/// [`builtin_extractor`] arm — three views, one anchor, zero drift.
pub const EXTENSION_ALIASES: &[(&str, &str)] = &[
    ("pdf", "pdf"),
    // the img family (M3-T4): all three spellings route to the one short
    // key — the extractor sniffs the container, the extension only claims
    ("jpg", "img"),
    ("jpeg", "img"),
    ("png", "img"),
    // the M3-T6 text face: one spelling for docx, both spellings for html
    ("docx", "docx"),
    ("html", "html"),
    ("htm", "html"),
    // the M3-T7 container families: one spelling each (an archive IS its
    // extension's format — claiming is all the extension does; expansion
    // policy is the kind's, see extract/{epub,zip,eml}.rs)
    ("epub", "epub"),
    ("zip", "zip"),
    ("eml", "eml"),
    ("md", "md"),
    ("markdown", "md"),
    ("txt", "txt"),
    ("text", "txt"),
];

// ---------------------------------------------------------------------------
// shared container plumbing (M3-T7): the constants + capped read loop the
// zip-based container kinds (epub/zip) share — the docx.rs discipline
// (entry-count bomb BEFORE any read; decompression ceiling WHILE reading)
// factored once for its second and third consumers.
// ---------------------------------------------------------------------------

/// Entry-count bomb guard for container kinds: checked against the zip's
/// entry listing BEFORE any entry is read (the docx MAX_ZIP_ENTRIES form,
/// which is itself the ingest worker's MAX_CHILDREN=256 form — same
/// number, same discipline, kind-side because the listing is read here).
pub(crate) const CONTAINER_MAX_ENTRIES: usize = 256;

/// Per-entry decompression ceiling while READING (the docx MAX_XML_BYTES
/// form: zip headers lie about sizes, so the cap bounds what a bomb can
/// make the process materialize BEFORE the worker's per-child
/// max_file_size_mb gate sees the child). 256 MiB is 2x the default cap's
/// headroom — mechanism constant, not policy (the §5.10.8 spirit).
pub(crate) const ENTRY_DECOMPRESSION_CEILING: u64 = 256 * 1024 * 1024;

/// Read one open zip entry to end, refusing past `ceiling` OUTPUT bytes
/// (the amplification gate at the expansion point, §5.10.3 ruling ③).
/// `kind` is the registry label and `name` the entry path, for the loud
/// error. Generic over [`std::io::Read`] (a `ZipFile` is one; the generic
/// also keeps THIS module's local `pub mod zip` kind module from shadowing
/// the crate path), chunk-fed so the cap is enforced DURING the read, not
/// after it.
pub(crate) fn read_entry_capped(
    entry: &mut impl std::io::Read,
    ceiling: u64,
    kind: &str,
    name: &str,
) -> Result<Vec<u8>, PipelineError> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut read: u64 = 0;
    loop {
        let n = entry.read(&mut chunk).map_err(|e| {
            PipelineError::extract(format!(
                "{kind}: reading '{name}' failed (corrupt deflate stream): {e}"
            ))
        })?;
        if n == 0 {
            return Ok(out);
        }
        read += n as u64;
        if read > ceiling {
            return Err(PipelineError::extract(format!(
                "{kind}: '{name}' decompresses past {ceiling} bytes — a decompression bomb is \
                 refused at the expansion point (§5.10.3); if this is a legitimate entry, it is \
                 too large for one child"
            )));
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

/// Display name -> canonical mime through the ONE extension table; unknown
/// or absent extension ⇒ `application/octet-stream` — the honest "cannot
/// tell" (the gzip normalizer's `infer_mime`, factored for its siblings:
/// the zip kind's per-entry inference and the eml kind's attachment
/// octet-stream cross-check ride the same single source the scanner
/// routes by).
pub(crate) fn infer_mime_from_name(name: &str) -> String {
    let file = name.rsplit('/').next().unwrap_or(name);
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    ext.as_deref()
        .and_then(short_key_for_extension)
        .and_then(mime_for_short_key)
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// Extension lookup over [`EXTENSION_ALIASES`] (exact, case-normalized by
/// the caller).
pub fn short_key_for_extension(ext: &str) -> Option<&'static str> {
    EXTENSION_ALIASES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, key)| *key)
}

/// Registered extractor kinds. Exhaustive match everywhere = zero vtable;
/// `dyn` stays reserved for the v2 wasm slot and the contract tests.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractorKind {
    /// UTF-8 text passthrough (`extract/txt.rs`).
    Txt(TxtConfig),
    /// pdfium whole-document text (`extract/pdf_text.rs`).
    PdfText(PdfTextConfig),
    /// CommonMark to plain text (`extract/md.rs`).
    Md(MdConfig),
    /// lopdf-based classify-then-extract PDF kind, no native library
    /// (`extract/pdf_inspector.rs`).
    PdfInspector(PdfInspectorConfig),
    /// pdfium page-to-PNG renderer: one IMAGE Part per page, page order
    /// preserved (`extract/pdf_pages.rs`, M3-T3 — the image chain's supply
    /// side; the third kind behind the `pdf` short key, the first whose
    /// output modality is not Text).
    PdfPages(PdfPagesConfig),
    /// PaddleOCR PP-OCRv5 mobile det+rec over the tree's own ort sessions:
    /// every page renders (own dpi knob, default 300) through det → DB
    /// boxes → rec → CTC lines, ONE Text Part per page in page order
    /// (`extract/pdf_ocr/`, M3-T10 — the OCR route for scanned PDFs; the
    /// FOURTH kind behind the `pdf` short key, scan-dominant Preferred).
    PdfOcr(PdfOcrConfig),
    /// Image FILE as a source: validate-decode then ONE image Part with the
    /// original bytes verbatim (`extract/image.rs`, M3-T4 — the user-facing
    /// supply side of the image chain, behind the `img` short key).
    Image(ImageConfig),
    /// Word document file: unzip to `word/document.xml`, pull-event XML
    /// text extraction, ONE text Part of `"\n\n"`-joined paragraphs
    /// (`extract/docx.rs`, M3-T6). LEAF kind per the §0 criterion — the
    /// internal XML is not an independently routable user document.
    Docx(DocxConfig),
    /// HTML document file: html5ever (WHATWG-compliant) parse, strip
    /// script/style/head noise, block-level line breaks, ONE text Part
    /// (`extract/html.rs`, M3-T6). LEAF kind — one html file IS one
    /// document; the same chain serves T7's epub chapters by plain
    /// child re-routing, no special-casing on either side.
    Html(HtmlConfig),
    /// EPUB book file: the FIRST production CONTAINER kind (M3-T7,
    /// `extract/epub.rs`). `unpack` walks container.xml → OPF → spine and
    /// produces spine-ordered XHTML chapter children (`text/html`); the
    /// reading order is the kind's format intelligence — zip entry order is
    /// NOT it. Non-spine resources produce nothing. `unpack` is the kind's
    /// one product path (`extract` refuses loudly — a container never
    /// extracts).
    Epub(EpubConfig),
    /// Generic zip archive: CONTAINER kind (M3-T7, `extract/zip.rs`).
    /// `unpack` produces one child per file entry with the mime inferred
    /// from the entry-name extension through the one table; unknown
    /// extensions ride as honest `application/octet-stream` (downstream
    /// routing decides — module docs for the adjudication).
    Zip(ZipConfig),
    /// RFC 5322 email message: CONTAINER kind (M3-T7, `extract/eml.rs`).
    /// `unpack` walks the MIME part tree: text bodies by their part mime,
    /// attachments by Content-Type (octet-stream cross-checked against the
    /// filename extension), embedded messages as re-routable
    /// `message/rfc822` children.
    Eml(EmlConfig),
    /// Scripted CONTAINER kind for the M2.1 step 3 mechanism tests — see
    /// [`TestContainerSpec`]. `cfg(test)` only: the PRODUCTION registry
    /// gained its first real container kinds in M3-T7 (`Epub`/`Zip`/`Eml`
    /// above — the role this scaffold proved); no test kind is parseable
    /// from config (`from_config` has no arm — tests inject the variant
    /// into a loaded plan directly).
    #[cfg(test)]
    TestContainer(TestContainerSpec),
    /// Scripted IMAGE kind for the M3-T1 modality-mechanism tests (the
    /// real image/pdf-pages extractor kinds are T2/T3). Same discipline as
    /// [`Self::TestContainer`]: cfg(test) only, no `from_config` arm, tests
    /// inject the variant into a loaded plan.
    #[cfg(test)]
    TestImage(TestImageSpec),
}

/// The cfg(test) spec of the scripted image kind: one image Part over the
/// input bytes, declared as `mime` (the M3-T1 worker e2e fixture — it
/// proves image extractions carry content, atom into chunks, and hit the
/// doc-modality gate, without any real image kind existing yet).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct TestImageSpec {
    /// The mime the extracted Part declares (must be the bytes' canonical
    /// container mime or the chunker bypass's sniff guard refuses it).
    pub mime: String,
}

#[cfg(test)]
impl TestImageSpec {
    /// One image Part carrying the WHOLE input as the payload.
    fn extract(&self, bytes: &[u8]) -> Result<Vec<Part>, PipelineError> {
        Ok(vec![Part::image(self.mime.clone(), bytes.to_vec())])
    }
}

/// The programmable proof kind for the container-unpacking mechanism
/// (cfg(test) only, the T2/T3 mock-in-crate discipline): `unpack` runs a
/// scripted table of exact payload bytes → programmed children, so tests
/// can hand the GENERIC worker path whatever shape they need to prove —
/// txt children, an md child routed by ITS mime, a nested container, an
/// over-limit child count, oversize children…
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct TestContainerSpec {
    /// The mime this container accepts; any other mime ⇒ Err (the shared
    /// mime-gate semantics: a container reached with foreign content yields
    /// the chain to the next member).
    pub mime: String,
    /// payload bytes → children, first match wins; a payload with no
    /// matching entry ⇒ Err (unknown container bytes).
    pub script: Vec<(Vec<u8>, Vec<ChildDoc>)>,
}

#[cfg(test)]
impl TestContainerSpec {
    /// One-entry script: `payload` unpacks to `children` (nesting tests use
    /// [`TestContainerSpec::scripted`] — distinct payloads must unpack to
    /// distinct children).
    pub fn one(mime: &str, payload: &[u8], children: Vec<ChildDoc>) -> Self {
        TestContainerSpec::scripted(mime, vec![(payload.to_vec(), children)])
    }

    /// Multi-entry script: `(payload, children)` pairs matched by exact
    /// bytes (the nesting fixture: outer payload → container child whose
    /// bytes are the inner payload).
    pub fn scripted(mime: &str, script: Vec<(Vec<u8>, Vec<ChildDoc>)>) -> Self {
        TestContainerSpec {
            mime: mime.to_string(),
            script,
        }
    }

    /// The kind's whole behavior: mime gate, then the script lookup.
    fn unpack(&self, bytes: &[u8], mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        if mime != self.mime {
            return Err(PipelineError::extract(format!(
                "test-container cannot handle mime '{mime}' (accepts only '{}'); this is an \
                 extract-routing bug, not a bad file",
                self.mime
            )));
        }
        match self
            .script
            .iter()
            .find(|(payload, _)| payload.as_slice() == bytes)
        {
            Some((_, children)) => Ok(Some(children.clone())),
            None => Err(PipelineError::extract(
                "test-container: payload matches no script entry (unknown container bytes)",
            )),
        }
    }
}

impl ExtractorKind {
    /// Two-stage config parse (§5.10.2): stage 1 reads the `kind` string,
    /// stage 2 deserializes the per-kind struct with `deny_unknown_fields`.
    /// `section` is the config path for error context (e.g.
    /// `extractor.pdf`), `line` the 1-based TOML line of the section.
    pub fn from_config(
        section: &str,
        raw: &toml::Value,
        line: Option<usize>,
    ) -> Result<Self, PipelineError> {
        let kind = super::config::probe_kind(raw)
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
        match kind.as_str() {
            "txt" => {
                let cfg: TxtConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Txt(cfg))
            }
            "pdf-text" => {
                let cfg: PdfTextConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::PdfText(cfg))
            }
            "md" => {
                let cfg: MdConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Md(cfg))
            }
            "pdf-inspector" => {
                let cfg: PdfInspectorConfig =
                    raw.clone().try_into().map_err(|e: toml::de::Error| {
                        super::config::stage2_error(section, Some(&kind), e.message(), line)
                    })?;
                Ok(ExtractorKind::PdfInspector(cfg))
            }
            "pdf-pages" => {
                let cfg: PdfPagesConfig =
                    raw.clone().try_into().map_err(|e: toml::de::Error| {
                        super::config::stage2_error(section, Some(&kind), e.message(), line)
                    })?;
                // dpi domain is STARTUP validation (the token-window
                // precedent): 10..=600 dots per inch, fail-fast at the
                // config line that carries the bad number
                cfg.validate()
                    .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
                Ok(ExtractorKind::PdfPages(cfg))
            }
            "pdf-ocr" => {
                let cfg: PdfOcrConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                // same dpi domain discipline as pdf-pages (the T3 constants
                // verbatim — same risk class, same numbers)
                cfg.validate()
                    .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
                Ok(ExtractorKind::PdfOcr(cfg))
            }
            "image" => {
                let cfg: ImageConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Image(cfg))
            }
            "docx" => {
                let cfg: DocxConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Docx(cfg))
            }
            "html" => {
                let cfg: HtmlConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Html(cfg))
            }
            "epub" => {
                let cfg: EpubConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Epub(cfg))
            }
            "zip" => {
                let cfg: ZipConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Zip(cfg))
            }
            "eml" => {
                let cfg: EmlConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(ExtractorKind::Eml(cfg))
            }
            other => Err(PipelineError::config(
                format!(
                    "[{section}]: unknown extractor kind '{other}' (available kinds: \
                     {EXTRACTOR_KINDS})"
                ),
                line,
            )),
        }
    }

    /// Modalities this kind can emit, for the config-time modality
    /// carry-over check (§5.10.3: an extractor output with no embedder to
    /// carry it refuses to start). Kept beside the dispatch so a new kind
    /// and its modality declaration land in the same diff.
    pub fn output_modalities(&self) -> &'static [Modality] {
        match self {
            // The text kinds (§5.10.1). pdf-pages is the first Image-output
            // kind — the config-time modality carry-over check (below in
            // config.rs) therefore demands an image-accepting embedder on
            // any ingest that routes to it.
            ExtractorKind::Txt(_)
            | ExtractorKind::PdfText(_)
            | ExtractorKind::Md(_)
            | ExtractorKind::PdfInspector(_)
            | ExtractorKind::Docx(_)
            | ExtractorKind::Html(_)
            | ExtractorKind::PdfOcr(_) => &[Modality::Text],
            ExtractorKind::PdfPages(_) | ExtractorKind::Image(_) => &[Modality::Image],
            // Container kinds (M3-T7): children re-enter user routing, so
            // the OUTPUT modality is whatever the user's chains produce —
            // unknowable at config time. The declaration is best-effort
            // (Text: the ordinary corpus of chapters/bodies/attachments),
            // exactly like the cfg(test) TestContainer's; the RUNTIME
            // modality gate is the real bar (the M3-T1 precedent: a
            // placeholder-declares-text runtime-emits-image shape is caught
            // there, loudly, with the remedy).
            ExtractorKind::Epub(_) | ExtractorKind::Zip(_) | ExtractorKind::Eml(_) => {
                &[Modality::Text]
            }
            // its children re-enter user routing; the tests' children are
            // text (txt/md chains)
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => &[Modality::Text],
            // the cfg(test) image kind emits exactly what M3's real image
            // kinds will (the declaration the M3-T1 worker tests rely on)
            #[cfg(test)]
            ExtractorKind::TestImage(_) => &[Modality::Image],
        }
    }

    /// The kind's REGISTRY label — the `kind = "…"` config string, one per
    /// variant, exhaustively matched so a new kind MUST land here (the
    /// compiler walks it; [`KIND_LABELS`] and the parity test then force the
    /// table views to agree). Unlike [`Self::describe`] this is knob-free:
    /// it names the KIND, not the configuration.
    pub fn kind_label(&self) -> &'static str {
        match self {
            ExtractorKind::Txt(_) => "txt",
            ExtractorKind::PdfText(_) => "pdf-text",
            ExtractorKind::Md(_) => "md",
            ExtractorKind::PdfInspector(_) => "pdf-inspector",
            ExtractorKind::PdfPages(_) => "pdf-pages",
            ExtractorKind::PdfOcr(_) => "pdf-ocr",
            ExtractorKind::Image(_) => "image",
            ExtractorKind::Docx(_) => "docx",
            ExtractorKind::Html(_) => "html",
            ExtractorKind::Epub(_) => "epub",
            ExtractorKind::Zip(_) => "zip",
            ExtractorKind::Eml(_) => "eml",
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => "test-container",
            #[cfg(test)]
            ExtractorKind::TestImage(_) => "test-image",
        }
    }

    /// Human-readable kind + params (CLI summary lines).
    pub fn describe(&self) -> String {
        match self {
            ExtractorKind::Txt(_) => "txt".to_string(),
            ExtractorKind::PdfText(_) => "pdf-text".to_string(),
            ExtractorKind::Md(_) => "md".to_string(),
            ExtractorKind::Image(_) => "image".to_string(),
            // knob-free kinds (M3-T6): the label is the whole summary
            ExtractorKind::Docx(_) => "docx".to_string(),
            ExtractorKind::Html(_) => "html".to_string(),
            // knob-free kinds (M3-T7), same discipline: the label is the
            // whole summary
            ExtractorKind::Epub(_) => "epub".to_string(),
            ExtractorKind::Zip(_) => "zip".to_string(),
            ExtractorKind::Eml(_) => "eml".to_string(),
            // the knob shows only when it departs from the pdf-text-aligned
            // default, keeping summary lines short
            ExtractorKind::PdfInspector(cfg) => {
                if cfg.markdown {
                    "pdf-inspector(markdown=true)".to_string()
                } else {
                    "pdf-inspector".to_string()
                }
            }
            // same discipline: bare label at the default dpi
            ExtractorKind::PdfPages(cfg) => {
                if cfg.dpi == pdf_pages::DEFAULT_DPI {
                    "pdf-pages".to_string()
                } else {
                    format!("pdf-pages(dpi={})", cfg.dpi)
                }
            }
            // same discipline at the OCR default (300); a `path` model
            // directory is load-relevant and always shown (the embedder
            // summary precedent)
            ExtractorKind::PdfOcr(cfg) => {
                let mut s = String::from("pdf-ocr");
                if cfg.dpi != pdf_ocr::DEFAULT_DPI {
                    s.push_str(&format!("(dpi={}", cfg.dpi));
                    if let Some(p) = &cfg.path {
                        s.push_str(&format!(", path={p}"));
                    }
                    s.push(')');
                } else if let Some(p) = &cfg.path {
                    s.push_str(&format!("(path={p})"));
                }
                s
            }
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => "test-container".to_string(),
            #[cfg(test)]
            ExtractorKind::TestImage(_) => "test-image".to_string(),
        }
    }
}

/// Dispatch: the enum IS the extractor (no Box, no vtable). The pdf-text arm
/// snapshots the library resolution per call (cheap path checks); the
/// process-global pdfium bind happens at most once regardless (pdf_text.rs).
impl Extractor for ExtractorKind {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        match self {
            ExtractorKind::Txt(_) => TxtExtractor.extract(bytes, mime),
            ExtractorKind::PdfText(_) => PdfTextExtractor::new().extract(bytes, mime),
            ExtractorKind::Md(_) => MdExtractor.extract(bytes, mime),
            ExtractorKind::PdfInspector(cfg) => {
                let x = if cfg.markdown {
                    PdfInspectorExtractor::markdown()
                } else {
                    PdfInspectorExtractor::new()
                };
                x.extract(bytes, mime)
            }
            // like the pdf-text arm: snapshot the library resolution per
            // call (cheap path checks); the pdfium bind itself is
            // process-global (pdf_text::bind_pdfium)
            ExtractorKind::PdfPages(cfg) => PdfPagesExtractor::new(cfg).extract(bytes, mime),
            // the OCR pair: model dir + pdfium + two ort sessions, all
            // lazy on first use (see the kind module docs); an unhydrated
            // bare-config instance refuses loudly instead of guessing a
            // models cache
            ExtractorKind::PdfOcr(cfg) => PdfOcrExtractor::new(cfg)?.extract(bytes, mime),
            // configless: the kind IS the extractor (validate-decode, then
            // one verbatim image Part — see extract/image.rs)
            ExtractorKind::Image(_) => ImageExtractor.extract(bytes, mime),
            // configless (M3-T6): the format intelligence IS the kind
            ExtractorKind::Docx(_) => DocxExtractor.extract(bytes, mime),
            ExtractorKind::Html(_) => HtmlExtractor.extract(bytes, mime),
            // the container kinds (M3-T7): the module extractors carry the
            // loud role-separation refusal (a container NEVER extracts —
            // the worker short-circuits extract for kinds whose unpack
            // yields children, so reaching extract is a routing bug, and
            // it must fail visibly instead of guessing)
            ExtractorKind::Epub(_) => EpubExtractor.extract(bytes, mime),
            ExtractorKind::Zip(_) => ZipExtractor.extract(bytes, mime),
            ExtractorKind::Eml(_) => EmlExtractor.extract(bytes, mime),
            // role separation made loud: a container NEVER extracts — the
            // worker short-circuits extract for kinds whose unpack yields
            // children, so reaching this arm is a mechanism bug, and it
            // must fail visibly instead of guessing.
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => Err(PipelineError::extract(
                "test-container is a container kind: it unpacks, never extracts — the worker \
                 re-routes its children; extract() here is a routing bug",
            )),
            // the scripted image kind: one atomic image Part over the bytes
            #[cfg(test)]
            ExtractorKind::TestImage(spec) => spec.extract(bytes),
        }
    }

    /// Affinity dispatch (M2.1 step 2): each arm delegates to its kind
    /// module's class-level declaration — a NEW variant lands here with the
    /// compiler walking it (same discipline as `output_modalities`), so a
    /// kind cannot be routed without declaring its fit. The declarations are
    /// free functions in the kind modules because affinity is CLASS-level:
    /// no instance state participates (the pdf-text arm deliberately does
    /// NOT construct an extractor — library resolution is deployment state,
    /// routing must not touch it).
    fn affinity(&self, class: ContentClass) -> Fit {
        match self {
            ExtractorKind::Txt(_) => txt::affinity(class),
            ExtractorKind::PdfText(_) => pdf_text::affinity(class),
            ExtractorKind::Md(_) => md::affinity(class),
            ExtractorKind::PdfInspector(_) => pdf_inspector::affinity(class),
            ExtractorKind::PdfPages(_) => pdf_pages::affinity(class),
            // the OCR declaration: scan-dominant is Preferred territory
            // (OCR is the main route for scans); mixed/text-native Capable
            // (a text member outranks OCR where a text layer exists);
            // no-text Refused (blank pages carry nothing to OCR)
            ExtractorKind::PdfOcr(_) => pdf_ocr::affinity(class),
            // undeclared (the trait default, spelled out): the image kind
            // declares no fit — its family has no classification probe, so
            // no class ever routes it; reached through its own `img` route
            // or a default chain, where config order is the whole truth
            // (extract/image.rs module docs)
            ExtractorKind::Image(_) => Fit::Capable,
            // undeclared, same ruling (M3-T6): the ContentClass vocabulary
            // is PDF-shaped — a docx/html source probes as Unknown and the
            // router never consults affinity for it (the module docs of
            // both kinds)
            ExtractorKind::Docx(_) => Fit::Capable,
            ExtractorKind::Html(_) => Fit::Capable,
            // undeclared, same ruling (M3-T7): epub/zip/eml sources probe
            // Unknown (no family probe exists) — config order is the whole
            // truth around them (each kind module's docs)
            ExtractorKind::Epub(_) => Fit::Capable,
            ExtractorKind::Zip(_) => Fit::Capable,
            ExtractorKind::Eml(_) => Fit::Capable,
            // undeclared (the trait default, spelled out): a container that
            // declares no fit is Capable everywhere — routing degenerates to
            // config order around it, exactly like any undeclared leaf.
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => Fit::Capable,
            #[cfg(test)]
            ExtractorKind::TestImage(_) => Fit::Capable,
        }
    }

    /// Container-role dispatch (M2.1 step 3): the LEAF kinds (the trait
    /// default, spelled out at the dispatch site so the closed set stays
    /// honest — a new production kind lands here with the compiler walking
    /// it), the three M3-T7 PRODUCTION containers (epub/zip/eml — the role
    /// the cfg(test) scaffold proved), and the test container's script.
    fn unpack(&self, _bytes: &[u8], _mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        match self {
            ExtractorKind::Txt(_)
            | ExtractorKind::PdfText(_)
            | ExtractorKind::Md(_)
            | ExtractorKind::PdfInspector(_)
            | ExtractorKind::PdfPages(_)
            | ExtractorKind::PdfOcr(_)
            | ExtractorKind::Image(_)
            | ExtractorKind::Docx(_)
            | ExtractorKind::Html(_) => Ok(None),
            // the production containers: expansion is their one product
            // path (each module's docs carry its ruling)
            ExtractorKind::Epub(_) => EpubExtractor.unpack(_bytes, _mime),
            ExtractorKind::Zip(_) => ZipExtractor.unpack(_bytes, _mime),
            ExtractorKind::Eml(_) => EmlExtractor.unpack(_bytes, _mime),
            #[cfg(test)]
            ExtractorKind::TestContainer(spec) => spec.unpack(_bytes, _mime),
            // the scripted image kind is a leaf
            #[cfg(test)]
            ExtractorKind::TestImage(_) => Ok(None),
        }
    }
}

impl ExtractorKind {
    /// One-line reason the ingest router quotes when this kind Refuses a
    /// probed class (the all-refuse aggregate message). Dispatches to the
    /// kind modules' declarations next to [`affinity`](Extractor::affinity);
    /// callers only invoke it on Refuses verdicts.
    pub fn refusal_note(&self, class: ContentClass) -> String {
        match self {
            ExtractorKind::Txt(_) => txt::refusal_note(class),
            ExtractorKind::PdfText(_) => pdf_text::refusal_note(class),
            ExtractorKind::Md(_) => md::refusal_note(class),
            ExtractorKind::PdfInspector(_) => pdf_inspector::refusal_note(class),
            ExtractorKind::PdfPages(_) => pdf_pages::refusal_note(class),
            ExtractorKind::PdfOcr(_) => pdf_ocr::refusal_note(class),
            // unreachable in practice (the image kind is Capable for every
            // class, so the router never quotes a refusal)
            ExtractorKind::Image(_) => {
                "an image kind (no affinity declaration — never refuses)".to_string()
            }
            // same unreachability (M3-T6: both kinds are Capable everywhere)
            ExtractorKind::Docx(_) => {
                "a docx kind (no affinity declaration — never refuses)".to_string()
            }
            ExtractorKind::Html(_) => {
                "an html kind (no affinity declaration — never refuses)".to_string()
            }
            // same unreachability (M3-T7: all three containers are Capable
            // everywhere)
            ExtractorKind::Epub(_) => {
                "an epub kind (no affinity declaration — never refuses)".to_string()
            }
            ExtractorKind::Zip(_) => {
                "a zip kind (no affinity declaration — never refuses)".to_string()
            }
            ExtractorKind::Eml(_) => {
                "an eml kind (no affinity declaration — never refuses)".to_string()
            }
            // unreachable in practice (the test container is Capable for
            // every class, so the router never quotes a refusal)
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => {
                "a scripted test container (no affinity declaration)".to_string()
            }
            // same unreachability (the scripted image kind is Capable
            // everywhere)
            #[cfg(test)]
            ExtractorKind::TestImage(_) => {
                "a scripted image kind (no affinity declaration)".to_string()
            }
        }
    }
}

/// Fingerprint-facing serialization: the INNER config only — the kind tag is
/// already a field of every per-kind config struct, so the canonical form
/// stays flat (`kind = "txt"`, …) and a fingerprint never double-encodes it.
impl serde::Serialize for ExtractorKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ExtractorKind::Txt(c) => c.serialize(serializer),
            ExtractorKind::PdfText(c) => c.serialize(serializer),
            ExtractorKind::Md(c) => c.serialize(serializer),
            ExtractorKind::PdfInspector(c) => c.serialize(serializer),
            ExtractorKind::PdfPages(c) => c.serialize(serializer),
            ExtractorKind::PdfOcr(c) => c.serialize(serializer),
            ExtractorKind::Image(c) => c.serialize(serializer),
            ExtractorKind::Docx(c) => c.serialize(serializer),
            ExtractorKind::Html(c) => c.serialize(serializer),
            ExtractorKind::Epub(c) => c.serialize(serializer),
            ExtractorKind::Zip(c) => c.serialize(serializer),
            ExtractorKind::Eml(c) => c.serialize(serializer),
            // fingerprints serialize CONFIG-loaded kinds only; the test kinds
            // never enter a PipelineConfig (from_config has no arm), so these
            // arms exist purely to keep the closed set exhaustive.
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => serializer.serialize_str("test-container"),
            #[cfg(test)]
            ExtractorKind::TestImage(_) => serializer.serialize_str("test-image"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_short_key_table_round_trips() {
        for (mime, key) in MIME_TO_SHORT_KEY {
            assert_eq!(short_key_for_mime(mime), Some(*key), "{mime}");
            // MANY-TO-ONE since M3-T4 (the img family lists jpeg + png):
            // the inverse returns the family's canonical representative —
            // the FIRST row of the key — so the honest per-row assertion is
            // "the key resolves back to a mime OF THE SAME FAMILY"
            let canonical = mime_for_short_key(key).expect("key resolves");
            assert!(
                MIME_TO_SHORT_KEY
                    .iter()
                    .any(|(m, k)| *m == canonical && *k == *key),
                "{mime}: inverse of {key} left the family ({canonical})"
            );
        }
        // the family canonical is the FIRST row (the reporting face's
        // answer for a short-key-only consumer — pinned so reordering the
        // table cannot silently change GetSource's mime)
        assert_eq!(mime_for_short_key("img"), Some("image/jpeg"));
        // parameters tolerated (uniformly, image rows included); unknown
        // mimes have no short key; the ingest reserved words are NOT kind
        assert_eq!(short_key_for_mime("text/plain; charset=utf-8"), Some("txt"));
        assert_eq!(short_key_for_mime("image/png; name=x"), Some("img"));
        assert_eq!(short_key_for_mime("application/octet-stream"), None);
        assert_eq!(mime_for_short_key("default"), None);
        assert_eq!(mime_for_short_key("auto"), None);
    }

    #[test]
    fn extension_alias_view_covers_every_family() {
        // every family row of the mime table has at least one alias mapping
        // to it, and every alias routes back INTO its family (M3-T4: a
        // family may carry several mimes — jpg/jpeg/png all land in img, and
        // the canonical-mime round trip stays inside the family):
        // x.txt.gz / x.text.gz / x.md.gz all claim as the source they
        // decompress into (the scanner + normalizer contract)
        for (_mime, key) in MIME_TO_SHORT_KEY {
            let aliases = EXTENSION_ALIASES
                .iter()
                .filter(|(_, k)| *k == *key)
                .collect::<Vec<_>>();
            assert!(!aliases.is_empty(), "family {key} has no extension alias");
            for (ext, _) in aliases {
                assert_eq!(short_key_for_extension(ext), Some(*key), "{ext}");
                let canonical = mime_for_short_key(short_key_for_extension(ext).unwrap())
                    .expect("key resolves");
                let in_family = MIME_TO_SHORT_KEY
                    .iter()
                    .any(|(m, k)| *m == canonical && *k == *key);
                assert!(in_family, "{ext} routes outside the {key} family");
            }
        }
        // the img family's full extension set (all three spellings claim)
        for ext in ["jpg", "jpeg", "png"] {
            assert_eq!(short_key_for_extension(ext), Some("img"), "{ext}");
        }
        // the M3-T6 text face's extension set (both html spellings claim)
        assert_eq!(short_key_for_extension("docx"), Some("docx"));
        assert_eq!(short_key_for_extension("html"), Some("html"));
        assert_eq!(short_key_for_extension("htm"), Some("html"));
        // the M3-T7 container families claim (T7 landed: epub is no longer
        // the unknown-extension example it was in M3-T6)
        assert_eq!(short_key_for_extension("epub"), Some("epub"));
        assert_eq!(short_key_for_extension("zip"), Some("zip"));
        assert_eq!(short_key_for_extension("eml"), Some("eml"));
        // unknown extensions claim nothing
        assert_eq!(short_key_for_extension("mobi"), None);
        assert_eq!(short_key_for_extension("exe"), None);
        assert_eq!(short_key_for_extension(""), None);
    }

    #[test]
    fn kind_registry_views_agree() {
        // THE parity test (plugin-axis hardening): the availability string,
        // the label table, the exhaustive kind_label match and from_config
        // are four views of one registry — a new kind that updates some but
        // not all of them fails HERE instead of drifting in production
        // error messages.
        assert_eq!(EXTRACTOR_KINDS, KIND_LABELS.join(", "));
        for label in KIND_LABELS {
            // config round trip: label parses, names its variant, and the
            // variant names the label back (no .so touched — parsing and
            // labeling are library-free)
            let raw: toml::Value = toml::from_str(&format!("kind = \"{label}\"")).unwrap();
            let k = ExtractorKind::from_config("x", &raw, None).unwrap_or_else(|e| {
                panic!("KIND_LABELS entry {label} must parse: {e}");
            });
            assert_eq!(k.kind_label(), *label);
            // the DEFAULT config's describe is the bare label too (knob
            // suffixes appear only when knobs depart from defaults)
            assert_eq!(k.describe(), *label);
        }
    }

    #[test]
    fn from_config_accepts_every_kind() {
        // (kind string, good sample bytes, good sample mime, declared output
        // modality, expected canonical TOML of the parsed enum) — the two
        // pdfium kinds run the real .so (missing library fails the test
        // honestly, pdf_text.rs). The canonical-form column exists because
        // kinds with knobs (pdf-inspector, pdf-pages) serialize their
        // defaulted fields too — the fingerprint form is per-kind, not
        // uniformly `kind = "…"`.
        //
        // This sweep walks the LEAF kinds (it asserts extract() output); the
        // M3-T7 container kinds are swept by
        // `container_kinds_dispatch_unpack_through_the_enum` (their extract
        // deliberately refuses — the role separation).
        type Case = (
            &'static str,
            &'static [u8],
            &'static str,
            &'static [Modality],
            &'static str,
        );
        let cases: &[Case] = &[
            (
                "txt",
                b"hi",
                "text/plain",
                &[Modality::Text],
                "kind = \"txt\"",
            ),
            (
                "pdf-text",
                pdf_text::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
                &[Modality::Text],
                "kind = \"pdf-text\"",
            ),
            (
                "md",
                b"# hi",
                "text/markdown",
                &[Modality::Text],
                "kind = \"md\"",
            ),
            (
                "pdf-inspector",
                pdf_inspector::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
                &[Modality::Text],
                "kind = \"pdf-inspector\"\nmarkdown = false",
            ),
            (
                "pdf-pages",
                pdf_pages::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
                &[Modality::Image],
                "kind = \"pdf-pages\"\ndpi = 150",
            ),
            (
                "image",
                image::tests::static_png(),
                "image/png",
                &[Modality::Image],
                "kind = \"image\"",
            ),
            (
                "docx",
                docx::tests::static_docx(),
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                &[Modality::Text],
                "kind = \"docx\"",
            ),
            (
                "html",
                html::tests::static_html(),
                "text/html",
                &[Modality::Text],
                "kind = \"html\"",
            ),
        ];
        for (kind_str, bytes, mime, modalities, canonical) in cases {
            let raw: toml::Value = toml::from_str(&format!("kind = \"{kind_str}\"")).unwrap();
            let k = ExtractorKind::from_config("extractor.x", &raw, Some(1)).unwrap();
            assert_eq!(k.describe(), *kind_str);
            assert_eq!(k.output_modalities(), *modalities);
            // the enum itself dispatches (zero vtable), through the mime the
            // short-key table maps for this kind
            let kind_mime = mime_for_short_key(short_key_for_mime(mime).unwrap()).unwrap();
            let parts = Extractor::extract(&k, bytes, kind_mime)
                .unwrap_or_else(|e| panic!("{kind_str} must dispatch through the enum: {e}"));
            // every sample here is the single-page fixture: exactly one Part
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0].modality, modalities[0]);
            // fingerprints serialize flat: inner config only
            assert_eq!(toml::to_string(&k).unwrap().trim(), *canonical);
        }
    }

    #[test]
    fn affinity_dispatch_reaches_every_kind_declaration() {
        // the enum delegates to the kind modules' class-level declarations —
        // spot-check one discriminating cell per kind (the full matrices are
        // pinned in each kind module's own tests)
        let kind = |s: &str| {
            let raw: toml::Value = toml::from_str(&format!("kind = \"{s}\"")).unwrap();
            ExtractorKind::from_config("extractor.x", &raw, None).unwrap()
        };
        let pdf_text = kind("pdf-text");
        assert_eq!(
            Extractor::affinity(&pdf_text, ContentClass::TextNative),
            Fit::Preferred
        );
        assert_eq!(
            Extractor::affinity(&pdf_text, ContentClass::MixedTextScan),
            Fit::Capable
        );
        assert_eq!(
            Extractor::affinity(&pdf_text, ContentClass::ScanDominant),
            Fit::Refuses
        );
        // inspector: text-native preferred, everything else refused
        let insp = kind("pdf-inspector");
        assert_eq!(
            Extractor::affinity(&insp, ContentClass::TextNative),
            Fit::Preferred
        );
        assert_eq!(
            Extractor::affinity(&insp, ContentClass::MixedTextScan),
            Fit::Refuses
        );
        // pdf-pages: the image mirror — scan-dominant is its Preferred,
        // text-native/mixed stay Capable (the text route outranks page
        // images there), no-text is refused
        let pages = kind("pdf-pages");
        assert_eq!(
            Extractor::affinity(&pages, ContentClass::ScanDominant),
            Fit::Preferred
        );
        assert_eq!(
            Extractor::affinity(&pages, ContentClass::TextNative),
            Fit::Capable
        );
        assert_eq!(
            Extractor::affinity(&pages, ContentClass::NoText),
            Fit::Refuses
        );
        // pdf-ocr (M3-T10): the OCR mirror — scan-dominant Preferred (OCR
        // is the scan route), mixed/text-native Capable (a text member
        // outranks OCR where a text layer exists), no-text refused (the
        // ContentClass::remedy no-text vocabulary: OCR included can't help)
        let ocr = kind("pdf-ocr");
        assert_eq!(
            Extractor::affinity(&ocr, ContentClass::ScanDominant),
            Fit::Preferred
        );
        assert_eq!(
            Extractor::affinity(&ocr, ContentClass::MixedTextScan),
            Fit::Capable
        );
        assert_eq!(
            Extractor::affinity(&ocr, ContentClass::TextNative),
            Fit::Capable
        );
        assert_eq!(
            Extractor::affinity(&ocr, ContentClass::NoText),
            Fit::Refuses
        );
        assert!(
            ocr.refusal_note(ContentClass::NoText)
                .contains("nothing to OCR"),
            "the pdf-ocr no-text refusal names the blank-page reality"
        );
        // text family: text-native preferred only
        for s in ["txt", "md"] {
            let k = kind(s);
            assert_eq!(
                Extractor::affinity(&k, ContentClass::TextNative),
                Fit::Preferred
            );
            assert_eq!(Extractor::affinity(&k, ContentClass::NoText), Fit::Refuses);
        }
        // image: the UNDECLARED default — Capable for every class (its
        // family has no probe, so no class ever routes it; the dispatch
        // spells the trait default out, pinned here). The M3-T6 text-face
        // kinds (docx/html) and the M3-T7 containers (epub/zip/eml) are
        // undeclared for the same reason — the ContentClass vocabulary is
        // PDF-shaped and their sources probe Unknown.
        let img = kind("image");
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Extractor::affinity(&img, class), Fit::Capable, "{class}");
        }
        for s in ["docx", "html", "epub", "zip", "eml"] {
            let k = kind(s);
            assert_eq!(
                Extractor::affinity(&k, ContentClass::TextNative),
                Fit::Capable,
                "{s}"
            );
        }
        // refusal notes exist for every refusing cell (the router quotes them)
        assert!(
            pdf_text
                .refusal_note(ContentClass::ScanDominant)
                .contains("OCR")
        );
        assert!(
            insp.refusal_note(ContentClass::ScanDominant)
                .contains("classification gate")
        );
        assert!(
            pages.refusal_note(ContentClass::NoText).contains("blank"),
            "the pdf-pages no-text refusal names the blank-page reality"
        );
    }

    #[test]
    fn production_kinds_are_leaves_unpack_none() {
        // M2.1 step 3 zero-impact pin: every LEAF production kind's unpack
        // is the trait-default verdict `Ok(None)` — the dispatch spells the
        // default out for the closed set, and this test is what keeps a
        // future kind from silently landing as a container (a container
        // variant must land in its own dispatch arm + the container sweep
        // below, DELIBERATELY). The M3-T7 container kinds (epub/zip/eml)
        // are covered by `container_kinds_dispatch_unpack_through_the_enum`.
        let cases: &[(&str, &[u8], &str)] = &[
            ("txt", b"plain text", "text/plain"),
            (
                "pdf-text",
                pdf_text::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
            ),
            ("md", b"# heading", "text/markdown"),
            (
                "pdf-inspector",
                pdf_inspector::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
            ),
            (
                "pdf-pages",
                pdf_pages::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
            ),
            // the OCR kind needs its model directory at extract time (its
            // own module tests carry that half); the leaf pin only needs
            // the UNPACK verdict, which never touches models
            (
                "pdf-ocr",
                pdf_pages::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
            ),
            ("image", image::tests::static_png(), "image/png"),
            ("docx", docx::tests::static_docx(), docx::tests::DOCX_MIME),
            ("html", html::tests::static_html(), "text/html"),
        ];
        for (kind_str, bytes, mime) in cases {
            let raw: toml::Value = toml::from_str(&format!("kind = \"{kind_str}\"")).unwrap();
            let k = ExtractorKind::from_config("extractor.x", &raw, None).unwrap();
            assert_eq!(
                Extractor::unpack(&k, bytes, mime),
                Ok(None),
                "{kind_str} must be a leaf"
            );
        }
    }

    /// The container sweep (the M3-T7 mirror of the leaf pin above): all
    /// three production containers dispatch through the ENUM to their
    /// module unpacks — the verdict is `Ok(Some(children))` (never the leaf
    /// `Ok(None)`), the children carry real bytes, and the extract() arm is
    /// the loud role-separation refusal (never a silent Ok).
    #[test]
    fn container_kinds_dispatch_unpack_through_the_enum() {
        let epub_bytes = epub::tests::build_epub(&["ch2", "ch3", "ch1"], &[]);
        let zip_bytes = zip::tests::build_zip(&[("a.txt", b"body".as_slice())]);
        let eml_bytes = b"From: a@b.c\r\nSubject: s\r\n\r\nplain body";
        for (kind_str, bytes, mime, want_len) in [
            ("epub", epub_bytes.as_slice(), epub::EPUB_MIME, 3usize),
            ("zip", zip_bytes.as_slice(), zip::ZIP_MIME, 1),
            ("eml", eml_bytes, eml::EML_MIME, 1),
        ] {
            let raw: toml::Value = toml::from_str(&format!("kind = \"{kind_str}\"")).unwrap();
            let k = ExtractorKind::from_config("extractor.x", &raw, None).unwrap();
            let children = Extractor::unpack(&k, bytes, mime)
                .unwrap_or_else(|e| panic!("{kind_str} dispatches: {e}"))
                .unwrap_or_else(|| panic!("{kind_str} must be a container"));
            assert_eq!(children.len(), want_len, "{kind_str}");
            assert!(
                children.iter().all(|c| !c.bytes.is_empty()),
                "{kind_str} children carry bytes"
            );
            // and the extract() arm refuses loudly (the role separation)
            let err = Extractor::extract(&k, bytes, mime).unwrap_err();
            assert!(
                err.to_string().contains("container kind: it unpacks"),
                "{kind_str}: {err}"
            );
        }
    }

    #[test]
    fn from_config_unknown_kind_lists_available() {
        // mobi is a real-world format with NO kind — the error must list
        // every kind that DOES exist (the container trio included since
        // M3-T7; epub was the M3-T6-era example and has since landed)
        let raw: toml::Value = toml::from_str("kind = \"mobi\"").unwrap();
        let err = ExtractorKind::from_config("extractor.d", &raw, Some(3)).unwrap_err();
        assert!(
            err.to_string().contains("unknown extractor kind 'mobi'"),
            "{err}"
        );
        assert!(
            err.to_string().contains(
                "available kinds: txt, pdf-text, md, pdf-inspector, pdf-pages, pdf-ocr, image, \
                           docx, html, epub, zip, eml"
            ),
            "{err}"
        );
        assert!(err.to_string().contains("at line 3"), "{err}");
    }

    #[test]
    fn from_config_missing_or_mistyped_kind() {
        let empty: toml::Value = toml::from_str("").unwrap();
        let err = ExtractorKind::from_config("extractor.x", &empty, None).unwrap_err();
        assert!(err.to_string().contains("missing field 'kind'"), "{err}");

        let bad: toml::Value = toml::from_str("kind = 7").unwrap();
        let err = ExtractorKind::from_config("extractor.x", &bad, None).unwrap_err();
        assert!(err.to_string().contains("expected a string"), "{err}");
    }

    #[test]
    fn from_config_rejects_out_of_domain_dpi_with_line() {
        // the dpi domain is startup validation, attributed to the config
        // line that carries the bad number (§5.10.3 fail-fast)
        let raw: toml::Value = toml::from_str("kind = \"pdf-pages\"\ndpi = 5000").unwrap();
        let err = ExtractorKind::from_config("extractor.p", &raw, Some(4)).unwrap_err();
        assert!(err.to_string().contains("[extractor.p]"), "{err}");
        assert!(
            err.to_string()
                .contains("outside the accepted range 10..=600"),
            "{err}"
        );
        assert!(err.to_string().contains("dots per inch"), "{err}");
        assert!(err.to_string().contains("at line 4"), "{err}");
        // the in-domain knob parses (and departs from the default in
        // describe — the summary-line discipline)
        let raw: toml::Value = toml::from_str("kind = \"pdf-pages\"\ndpi = 300").unwrap();
        let k = ExtractorKind::from_config("extractor.p", &raw, None).unwrap();
        assert_eq!(k.describe(), "pdf-pages(dpi=300)");
    }

    #[test]
    fn unknown_fields_rejected_per_kind() {
        for (kind_str, extra) in [
            ("txt", "dpi = 1"),
            ("pdf-text", "dpi = 300"),
            ("md", "gfm = true"),
            ("pdf-inspector", "dpi = 300"),
            // dpi is LEGAL on pdf-pages/pdf-ocr — the unknown-field probe
            // needs a field these kinds do not have (pdf-ocr's lang knob is
            // deliberately absent: the PP-OCRv5 rec model is one model for
            // 简中/繁中/英/日/拼音, §5.10.8 minimalism)
            ("pdf-pages", "format = \"jpeg\""),
            ("pdf-ocr", "lang = \"ch\""),
            // the image kind is knob-free (preprocessing belongs to the
            // embedder); the M3-T6 text-face kinds are knob-free too (the
            // format intelligence IS the kind); the M3-T7 container kinds
            // are knob-free for the container-role reason (their expansion
            // policy is a format fact, their routing is the user's)
            ("image", "dpi = 72"),
            ("docx", "dpi = 300"),
            ("html", "dpi = 72"),
            ("epub", "dpi = 300"),
            ("zip", "dpi = 72"),
            ("eml", "dpi = 300"),
        ] {
            let raw: toml::Value =
                toml::from_str(&format!("kind = \"{kind_str}\"\n{extra}")).unwrap();
            let err = ExtractorKind::from_config("extractor.x", &raw, Some(1)).unwrap_err();
            assert!(
                err.to_string().contains("unknown field 'dpi'")
                    || err.to_string().contains("unknown field 'gfm'")
                    || err.to_string().contains("unknown field 'format'")
                    || err.to_string().contains("unknown field 'lang'"),
                "{kind_str}: {err}"
            );
        }
    }
}
