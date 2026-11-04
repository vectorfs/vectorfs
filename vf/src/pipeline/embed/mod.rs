//! Embedder axis: registration SKELETON only (M2 T2).
//!
//! **No embedder kinds are registered yet** — `ort` and `openai` land in
//! M2-T5 (design.md §7). The trait itself lives in
//! [`crate::pipeline`] (it is axis-shared, not per-kind); this module hosts
//! the registration entry and the closed dispatch enum so the pattern —
//! identical to [`crate::pipeline::extract`] / [`crate::pipeline::chunk`] —
//! is in place for T5 to fill:
//!
//! * T5 adds `embed/ort.rs` + `embed/openai.rs`, turns
//!   [`EmbedderKind`] into `enum { Ort(OrtConfig), Openai(OpenaiConfig) }`,
//!   and wires per-kind config structs per §5.10.4
//!   (`kind/model/path/offline` for ort; `kind/endpoint/model/dim` for
//!   openai), each `deny_unknown_fields`.
//! * Until then, ANY `[embedder.<name>]` section fails at load with the
//!   line number and this module's "no kinds yet" message — the honest
//!   fail-fast answer, better than silently ignoring the section.
//!
//! DIP boundary (pipeline/mod.rs): ort session / HTTP client types stay
//! inside the per-kind modules added in T5.

use super::{Chunk, Embedder, Modality, PipelineError, Query};

/// Registered embedder kinds. UNINHABITED in M2-T2 (see module docs): there
/// is nothing to construct, and every match over it is vacuously exhaustive
/// — the moment T5 adds the first variant, the compiler lists every site
/// that must handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {}

impl EmbedderKind {
    /// Two-stage config parse; with an empty registry the dispatch always
    /// answers "no kinds yet". Kept (not stubbed away) so the config
    /// framework — including the per-kind `deny_unknown_fields` struct
    /// plumbing — is exercised end-to-end by `[embedder.*]` sections today.
    pub fn from_config(
        section: &str,
        raw: &toml::Value,
        line: Option<usize>,
    ) -> Result<Self, PipelineError> {
        let kind = super::config::probe_kind(raw)
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
        Err(PipelineError::config(
            format!(
                "[{section}]: unknown embedder kind '{kind}' (no embedder kinds registered \
                 yet; ort/openai land in M2-T5)"
            ),
            line,
        ))
    }

    /// Human-readable kind + params (CLI summary lines); no kinds yet.
    pub fn describe(&self) -> String {
        match *self {}
    }
}

/// Dispatch: vacuously exhaustive until T5 adds variants.
#[async_trait::async_trait]
impl Embedder for EmbedderKind {
    async fn embed_query(&self, _q: &Query) -> Result<Vec<f32>, PipelineError> {
        match *self {}
    }

    async fn embed_documents(&self, _chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError> {
        match *self {}
    }

    fn dim(&self) -> usize {
        match *self {}
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        match *self {}
    }

    fn query_modalities(&self) -> &'static [Modality] {
        match *self {}
    }
}

/// Fingerprint-facing serialization: nothing to serialize until T5.
impl serde::Serialize for EmbedderKind {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_embedder_section_fails_with_t5_pointer() {
        let raw: toml::Value =
            toml::from_str("kind = \"ort\"\nmodel = \"gte-multilingual-base\"").unwrap();
        let err = EmbedderKind::from_config("embedder.text", &raw, Some(7)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[embedder.text]"), "{msg}");
        assert!(msg.contains("unknown embedder kind 'ort'"), "{msg}");
        assert!(msg.contains("M2-T5"), "{msg}");
        assert!(msg.contains("at line 7"), "{msg}");
    }

    #[test]
    fn missing_kind_reported() {
        let raw: toml::Value = toml::from_str("model = \"x\"").unwrap();
        let err = EmbedderKind::from_config("embedder.e", &raw, None).unwrap_err();
        assert!(err.to_string().contains("missing field 'kind'"), "{err}");
    }
}
