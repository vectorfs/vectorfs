//! Chunker axis: closed-enum registration + dispatch (M2 T2).
//!
//! Available kinds (M2): `token-window`. Markdown chunker lands in M3.

pub mod token_window;

use super::{Chunk, Chunker, Part, PipelineError};
use token_window::{TokenWindowChunker, TokenWindowConfig};

/// Registered chunker kinds (same registration pattern as
/// [`crate::pipeline::extract::ExtractorKind`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkerKind {
    TokenWindow(TokenWindowChunker),
}

impl ChunkerKind {
    /// Two-stage config parse. `section` is the config path for error
    /// context — chunkers are configured INLINE in an ingest section, so it
    /// reads e.g. `ingest.docs.chunk`; `line` is the ingest section's TOML
    /// line (per-kind values are detached from the document by then; §5.10.4
    /// attributes stage-2 errors to the owning section).
    pub fn from_config(
        section: &str,
        raw: &toml::Value,
        line: Option<usize>,
    ) -> Result<Self, PipelineError> {
        let kind = super::config::probe_kind(raw)
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
        match kind.as_str() {
            "token-window" => {
                let cfg: TokenWindowConfig =
                    raw.clone().try_into().map_err(|e: toml::de::Error| {
                        super::config::stage2_error(section, Some(&kind), e.message(), line)
                    })?;
                // window sanity is STARTUP validation (§5.10.3), reported at
                // the config line that carries the bad numbers
                TokenWindowChunker::new(cfg.size, cfg.overlap)
                    .map(ChunkerKind::TokenWindow)
                    .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))
            }
            other => Err(PipelineError::config(
                format!(
                    "[{section}]: unknown chunker kind '{other}' (available kinds: token-window)"
                ),
                line,
            )),
        }
    }

    /// Human-readable kind + params (CLI summary lines).
    pub fn describe(&self) -> String {
        match self {
            ChunkerKind::TokenWindow(c) => {
                format!("token-window(size={}, overlap={})", c.size, c.overlap)
            }
        }
    }
}

/// Dispatch: the enum IS the chunker.
impl Chunker for ChunkerKind {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError> {
        match self {
            ChunkerKind::TokenWindow(c) => c.chunk(parts),
        }
    }
}

/// Fingerprint-facing serialization: inner config only (the kind tag is a
/// field of the config struct).
impl serde::Serialize for ChunkerKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // serialize the VALIDATED parameters, not the live chunker
            ChunkerKind::TokenWindow(c) => TokenWindowConfig::from(c).serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(size: usize, overlap: usize) -> toml::Value {
        toml::from_str(&format!(
            "kind = \"token-window\"\nsize = {size}\noverlap = {overlap}"
        ))
        .unwrap()
    }

    #[test]
    fn from_config_accepts_valid_window() {
        let k = ChunkerKind::from_config("ingest.d.chunk", &win(512, 64), Some(2)).unwrap();
        assert_eq!(k.describe(), "token-window(size=512, overlap=64)");
        assert!(k.chunk(&[Part::text("hello")]).unwrap().len() == 1);
    }

    #[test]
    fn from_config_rejects_bad_window_with_line() {
        for (size, overlap) in [(64, 64usize), (512, 0), (512, 600), (0, 0)] {
            let err = ChunkerKind::from_config("ingest.d.chunk", &win(size, overlap), Some(5))
                .unwrap_err();
            assert!(
                err.to_string().contains("overlap") || err.to_string().contains("size"),
                "{err}"
            );
            assert!(err.to_string().contains("at line 5"), "{err}");
        }
    }

    #[test]
    fn from_config_unknown_kind() {
        let raw: toml::Value = toml::from_str("kind = \"markdown\"").unwrap();
        let err = ChunkerKind::from_config("ingest.d.chunk", &raw, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown chunker kind 'markdown' (available kinds: token-window)"),
            "{err}"
        );
    }
}
