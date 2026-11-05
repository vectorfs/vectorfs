//! Extractor axis: closed-enum registration + dispatch (M2 T2 skeleton,
//! kinds landed through M2 T4).
//!
//! `dyn`-free by design (§5.10.2 v2.1): the enum below is the registry.
//! Adding a kind = adding a variant + a `from_config` arm; the compiler then
//! walks every match site in this tree. Available kinds: `txt`,
//! `pdf-text`, `md`, `pdf-inspector` (the first kind added AFTER the M2
//! set — the closed-set mechanism's first live exercise; two PDF kinds
//! coexist behind the one `pdf` short key, selection is explicit).
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

use super::{Extractor, Modality, Part, PipelineError};
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

/// The available-kind list embedded in unknown-kind errors (kept next to the
/// enum so a new variant and its error line land in the same diff).
pub const EXTRACTOR_KINDS: &str = "txt, pdf-text, md, pdf-inspector";

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
