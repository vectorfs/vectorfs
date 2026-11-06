//! Extractor axis: closed-enum registration + dispatch (M2 T2 skeleton,
//! kinds landed through M2 T4).
//!
//! `dyn`-free by design (§5.10.2 v2.1): the enum below is the registry.
//! Adding a kind = adding a variant + a `from_config` arm; the compiler then
//! walks every match site in this tree. Available kinds: `txt`,
//! `pdf-text`, `md`, `pdf-inspector` (the first kind added AFTER the M2
//! set — the closed-set mechanism's first live exercise; two PDF kinds
//! coexist behind the one `pdf` short key, selection is explicit).
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

pub mod md;
pub mod pdf_inspector;
pub mod pdf_text;
pub mod txt;

use super::{ChildDoc, ContentClass, Extractor, Fit, Modality, Part, PipelineError};
use md::{MdConfig, MdExtractor};
use pdf_inspector::{PdfInspectorConfig, PdfInspectorExtractor};
use pdf_text::{PdfTextConfig, PdfTextExtractor};
use txt::{TxtConfig, TxtExtractor};

/// Canonical mime -> config short key, one row per M2 kind. ORDER MATTERS
/// only for human eyes (lookups are exact); the table is the single mapping
/// both directions consult.
pub const MIME_TO_SHORT_KEY: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    ("text/markdown", "md"),
    ("text/plain", "txt"),
];

/// The available-kind list embedded in unknown-kind errors. Derived view of
/// [`KIND_LABELS`] (the parity test pins the two together — a new kind that
/// updates the enum but forgets the string fails there, not in a user's
/// error message).
pub const EXTRACTOR_KINDS: &str = "txt, pdf-text, md, pdf-inspector";

/// Every production kind's config label, IN AVAILABILITY-LIST ORDER. This is
/// the registry's identity anchor: `kind_label` (the exhaustive match the
/// compiler walks on a new variant) must agree with this table row for row
/// (the `kind_registry_views_agree` test walks it). A new kind lands here,
/// in `kind_label`, and in `from_config` in one diff — everything else in
/// this module is exhaustive dispatch the compiler already checks.
pub const KIND_LABELS: &[&str] = &["txt", "pdf-text", "md", "pdf-inspector"];

/// mime (parameters like `; charset=utf-8` tolerated) -> short key.
pub fn short_key_for_mime(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or("").trim();
    MIME_TO_SHORT_KEY
        .iter()
        .find(|(m, _)| *m == base)
        .map(|(_, key)| *key)
}

/// short key -> canonical mime (the inverse; `default`/`auto` are ingest
/// RESERVED words, never kind keys, and are absent here on purpose).
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
    /// Scripted CONTAINER kind for the M2.1 step 3 mechanism tests — see
    /// [`TestContainerSpec`]. `cfg(test)` only: the PRODUCTION enum stays at
    /// four leaf kinds (epub/docx/eml and the zip dependency are M3), and
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
            // Every built-in kind so far emits Text only (§5.10.1).
            ExtractorKind::Txt(_)
            | ExtractorKind::PdfText(_)
            | ExtractorKind::Md(_)
            | ExtractorKind::PdfInspector(_) => &[Modality::Text],
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
            // the knob shows only when it departs from the pdf-text-aligned
            // default, keeping summary lines short
            ExtractorKind::PdfInspector(cfg) => {
                if cfg.markdown {
                    "pdf-inspector(markdown=true)".to_string()
                } else {
                    "pdf-inspector".to_string()
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
            | ExtractorKind::PdfInspector(_) => Ok(None),
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
            assert_eq!(mime_for_short_key(key), Some(*mime), "{key}");
        }
        // parameters tolerated; unknown mimes have no short key; the ingest
        // reserved words are NOT kind keys
        assert_eq!(short_key_for_mime("text/plain; charset=utf-8"), Some("txt"));
        assert_eq!(short_key_for_mime("application/octet-stream"), None);
        assert_eq!(mime_for_short_key("default"), None);
        assert_eq!(mime_for_short_key("auto"), None);
    }

    #[test]
    fn extension_alias_view_covers_every_family() {
        // every family row of the mime table has at least one alias mapping
        // to it, and every alias routes back to its family's canonical mime:
        // x.txt.gz / x.text.gz / x.md.gz all claim as the source they
        // decompress into (the scanner + normalizer contract)
        for (mime, key) in MIME_TO_SHORT_KEY {
            let aliases = EXTENSION_ALIASES
                .iter()
                .filter(|(_, k)| *k == *key)
                .collect::<Vec<_>>();
            assert!(!aliases.is_empty(), "family {key} has no extension alias");
            for (ext, _) in aliases {
                assert_eq!(short_key_for_extension(ext), Some(*key), "{ext}");
                assert_eq!(
                    mime_for_short_key(short_key_for_extension(ext).unwrap()),
                    Some(*mime)
                );
            }
        }
        // unknown extensions claim nothing
        assert_eq!(short_key_for_extension("docx"), None);
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
        // (kind string, good sample bytes, good sample mime, expected
        // canonical TOML of the parsed enum) — pdf-text runs the real .so
        // (missing library fails the test honestly, pdf_text.rs). The
        // canonical-form column exists because kinds with knobs
        // (pdf-inspector) serialize their defaulted fields too — the
        // fingerprint form is per-kind, not uniformly `kind = "…"`.
        let cases: &[(&str, &[u8], &str, &str)] = &[
            ("txt", b"hi", "text/plain", "kind = \"txt\""),
            (
                "pdf-text",
                pdf_text::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
                "kind = \"pdf-text\"",
            ),
            ("md", b"# hi", "text/markdown", "kind = \"md\""),
            (
                "pdf-inspector",
                pdf_inspector::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
                "kind = \"pdf-inspector\"\nmarkdown = false",
            ),
        ];
        for (kind_str, bytes, mime, canonical) in cases {
            let raw: toml::Value = toml::from_str(&format!("kind = \"{kind_str}\"")).unwrap();
            let k = ExtractorKind::from_config("extractor.x", &raw, Some(1)).unwrap();
            assert_eq!(k.describe(), *kind_str);
            assert_eq!(k.output_modalities(), &[Modality::Text]);
            // the enum itself dispatches (zero vtable), through the mime the
            // short-key table maps for this kind
            let kind_mime = mime_for_short_key(short_key_for_mime(mime).unwrap()).unwrap();
            let parts = Extractor::extract(&k, bytes, kind_mime)
                .unwrap_or_else(|e| panic!("{kind_str} must dispatch through the enum: {e}"));
            assert_eq!(parts.len(), 1);
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
        // text family: text-native preferred only
        for s in ["txt", "md"] {
            let k = kind(s);
            assert_eq!(
                Extractor::affinity(&k, ContentClass::TextNative),
                Fit::Preferred
            );
            assert_eq!(Extractor::affinity(&k, ContentClass::NoText), Fit::Refuses);
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
        let raw: toml::Value = toml::from_str("kind = \"docx\"").unwrap();
        let err = ExtractorKind::from_config("extractor.d", &raw, Some(3)).unwrap_err();
        assert!(
            err.to_string().contains("unknown extractor kind 'docx'"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("available kinds: txt, pdf-text, md, pdf-inspector"),
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
    fn unknown_fields_rejected_per_kind() {
        for (kind_str, extra) in [
            ("txt", "dpi = 1"),
            ("pdf-text", "dpi = 300"),
            ("md", "gfm = true"),
            ("pdf-inspector", "dpi = 300"),
        ] {
            let raw: toml::Value =
                toml::from_str(&format!("kind = \"{kind_str}\"\n{extra}")).unwrap();
            let err = ExtractorKind::from_config("extractor.x", &raw, Some(1)).unwrap_err();
            assert!(
                err.to_string().contains("unknown field 'dpi'")
                    || err.to_string().contains("unknown field 'gfm'"),
                "{kind_str}: {err}"
            );
        }
    }
}
