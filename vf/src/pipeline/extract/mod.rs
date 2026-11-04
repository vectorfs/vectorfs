//! Extractor axis: closed-enum registration + dispatch (M2 T2).
//!
//! `dyn`-free by design (§5.10.2 v2.1): the enum below is the registry.
//! Adding a kind = adding a variant + a `from_config` arm; the compiler then
//! walks every match site in this tree. Available kinds (M2): `txt`.

pub mod txt;

use super::{Extractor, Modality, Part, PipelineError};
use txt::{TxtConfig, TxtExtractor};

/// Registered extractor kinds. Exhaustive match everywhere = zero vtable;
/// `dyn` stays reserved for the v2 wasm slot and the contract tests.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractorKind {
    /// UTF-8 text passthrough (`extract/txt.rs`).
    Txt(TxtConfig),
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
            other => Err(PipelineError::config(
                format!("[{section}]: unknown extractor kind '{other}' (available kinds: txt)"),
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
            ExtractorKind::Txt(_) => &[Modality::Text],
        }
    }

    /// Human-readable kind + params (CLI summary lines).
    pub fn describe(&self) -> String {
        match self {
            ExtractorKind::Txt(_) => "txt".to_string(),
        }
    }
}

/// Dispatch: the enum IS the extractor (no Box, no vtable).
impl Extractor for ExtractorKind {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        match self {
            ExtractorKind::Txt(_) => TxtExtractor.extract(bytes, mime),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt_value() -> toml::Value {
        toml::from_str("kind = \"txt\"").unwrap()
    }

    #[test]
    fn from_config_accepts_txt() {
        let k = ExtractorKind::from_config("extractor.t", &txt_value(), Some(1)).unwrap();
        assert!(matches!(k, ExtractorKind::Txt(_)));
        assert_eq!(k.output_modalities(), &[Modality::Text]);
        assert_eq!(k.describe(), "txt");
        // the enum itself dispatches (zero vtable)
        assert_eq!(
            Extractor::extract(&k, b"hi", "text/plain").unwrap().len(),
            1
        );
    }

    #[test]
    fn from_config_unknown_kind_lists_available() {
        let raw: toml::Value = toml::from_str("kind = \"pdf-text\"").unwrap();
        let err = ExtractorKind::from_config("extractor.pdf", &raw, Some(3)).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown extractor kind 'pdf-text'"),
            "{err}"
        );
        assert!(err.to_string().contains("available kinds: txt"), "{err}");
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
    fn serialize_is_inner_config() {
        let k = ExtractorKind::from_config("e", &txt_value(), None).unwrap();
        let s = toml::to_string(&k).unwrap();
        assert_eq!(s.trim(), "kind = \"txt\"");
    }
}
