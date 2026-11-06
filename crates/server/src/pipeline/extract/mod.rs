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
//! two-kinds-one-key precedent extended to a text/image pair), `image`
//! (M3-T4, the image FILE kind: one verbatim image Part behind the new
//! `img` short key — the first family key carried by TWO mime rows,
//! jpeg + png), and `docx` + `html` (M3-T6, the text face: the Word
//! document kind behind its own `docx` key — a LEAF per the §0 criterion,
//! see extract/docx.rs — and the HTML document kind behind `html`,
//! html5ever under the hood per the selection note in extract/html.rs; the
//! html chain is also T7's epub-chapter route by construction, zero
//! special-casing either side).
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
pub mod html;
pub mod image;
pub mod md;
pub mod pdf_inspector;
pub mod pdf_pages;
pub mod pdf_text;
pub mod txt;

use super::{ChildDoc, ContentClass, Extractor, Fit, Modality, Part, PipelineError};
use docx::{DocxConfig, DocxExtractor};
use html::{HtmlConfig, HtmlExtractor};
use image::{ImageConfig, ImageExtractor};
use md::{MdConfig, MdExtractor};
use pdf_inspector::{PdfInspectorConfig, PdfInspectorExtractor};
use pdf_pages::{PdfPagesConfig, PdfPagesExtractor};
use pdf_text::{PdfTextConfig, PdfTextExtractor};
use txt::{TxtConfig, TxtExtractor};

/// Canonical mime -> config short key, one row per declared mime. ORDER
/// MATTERS only for human eyes — EXCEPT that a family with SEVERAL mimes
/// (`img`: jpeg + png, M3-T4) must list its CANONICAL representative first:
/// [`mime_for_short_key`] returns the first row of the key, and that mime is
/// what the reporting face (`GetSource`/`GetSourceData`) shows for the
/// whole family (routing itself is per-object and extension-exact). The
/// table is the single mapping both directions consult.
pub const MIME_TO_SHORT_KEY: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
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
pub const EXTRACTOR_KINDS: &str = "txt, pdf-text, md, pdf-inspector, pdf-pages, image, docx, html";

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
    "image",
    "docx",
    "html",
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
    ("md", "md"),
    ("markdown", "md"),
    ("txt", "txt"),
    ("text", "txt"),
];

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
    /// document; the same chain will serve T7's epub chapters by plain
    /// child re-routing, no special-casing on either side.
    Html(HtmlConfig),
    /// Scripted CONTAINER kind for the M2.1 step 3 mechanism tests — see
    /// [`TestContainerSpec`]. `cfg(test)` only: the PRODUCTION enum stays at
    /// leaf kinds only (seven since M3-T6's `docx`/`html`; the first
    /// production container kind — epub — is T7 work), and
    /// no test kind is parseable from config (`from_config` has no arm —
    /// tests inject the variant into a loaded plan directly).
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
            | ExtractorKind::Html(_) => &[Modality::Text],
            ExtractorKind::PdfPages(_) | ExtractorKind::Image(_) => &[Modality::Image],
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
            ExtractorKind::Image(_) => "image",
            ExtractorKind::Docx(_) => "docx",
            ExtractorKind::Html(_) => "html",
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
            // configless: the kind IS the extractor (validate-decode, then
            // one verbatim image Part — see extract/image.rs)
            ExtractorKind::Image(_) => ImageExtractor.extract(bytes, mime),
            // configless (M3-T6): the format intelligence IS the kind
            ExtractorKind::Docx(_) => DocxExtractor.extract(bytes, mime),
            ExtractorKind::Html(_) => HtmlExtractor.extract(bytes, mime),
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
            // undeclared (the trait default, spelled out): a container that
            // declares no fit is Capable everywhere — routing degenerates to
            // config order around it, exactly like any undeclared leaf.
            #[cfg(test)]
            ExtractorKind::TestContainer(_) => Fit::Capable,
            #[cfg(test)]
            ExtractorKind::TestImage(_) => Fit::Capable,
        }
    }

    /// Container-role dispatch (M2.1 step 3): the four PRODUCTION kinds are
    /// leaves (the trait default, spelled out at the dispatch site so the
    /// closed set stays honest — a new production kind lands here with the
    /// compiler walking it); the test container runs its script. (The
    /// underscore names silence the unused-parameter lint that the leaf-only
    /// production build would otherwise raise — only the cfg(test) arm reads
    /// them.)
    fn unpack(&self, _bytes: &[u8], _mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        match self {
            ExtractorKind::Txt(_)
            | ExtractorKind::PdfText(_)
            | ExtractorKind::Md(_)
            | ExtractorKind::PdfInspector(_)
            | ExtractorKind::PdfPages(_)
            | ExtractorKind::Image(_)
            | ExtractorKind::Docx(_)
            | ExtractorKind::Html(_) => Ok(None),
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
            ExtractorKind::Image(c) => c.serialize(serializer),
            ExtractorKind::Docx(c) => c.serialize(serializer),
            ExtractorKind::Html(c) => c.serialize(serializer),
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
        // unknown extensions claim nothing
        assert_eq!(short_key_for_extension("epub"), None);
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
        // kinds (docx/html) are undeclared for the same reason — the
        // ContentClass vocabulary is PDF-shaped and their sources probe
        // Unknown.
        let img = kind("image");
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Extractor::affinity(&img, class), Fit::Capable, "{class}");
        }
        for s in ["docx", "html"] {
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
        // M2.1 step 3 zero-impact pin: every PRODUCTION kind's unpack is the
        // trait-default leaf verdict `Ok(None)` — the dispatch spells the
        // default out for the closed set, and this test is what keeps a
        // future kind from silently landing as a container (a container
        // variant must update this list DELIBERATELY, with its own unpack).
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

    #[test]
    fn from_config_unknown_kind_lists_available() {
        // epub is T7's container kind — unknown TODAY, and the error must
        // list every kind that DOES exist (docx/html included since M3-T6)
        let raw: toml::Value = toml::from_str("kind = \"epub\"").unwrap();
        let err = ExtractorKind::from_config("extractor.d", &raw, Some(3)).unwrap_err();
        assert!(
            err.to_string().contains("unknown extractor kind 'epub'"),
            "{err}"
        );
        assert!(
            err.to_string().contains(
                "available kinds: txt, pdf-text, md, pdf-inspector, pdf-pages, image, \
                           docx, html"
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
            // dpi is LEGAL on pdf-pages — the unknown-field probe needs a
            // field this kind does not have
            ("pdf-pages", "format = \"jpeg\""),
            // the image kind is knob-free (preprocessing belongs to the
            // embedder); the M3-T6 text-face kinds are knob-free too (the
            // format intelligence IS the kind)
            ("image", "dpi = 72"),
            ("docx", "dpi = 300"),
            ("html", "dpi = 72"),
        ] {
            let raw: toml::Value =
                toml::from_str(&format!("kind = \"{kind_str}\"\n{extra}")).unwrap();
            let err = ExtractorKind::from_config("extractor.x", &raw, Some(1)).unwrap_err();
            assert!(
                err.to_string().contains("unknown field 'dpi'")
                    || err.to_string().contains("unknown field 'gfm'")
                    || err.to_string().contains("unknown field 'format'"),
                "{kind_str}: {err}"
            );
        }
    }
}
