//! Embedder axis: closed-enum registration + dispatch, filled in M2-T5
//! (design.md §5.10.2 Embedder row; §5.10.4 config shapes).
//!
//! # Two objects, by design
//!
//! * [`EmbedderKind`] is the CONFIG object: plain data, `Clone`, serializes
//!   flat into pipeline fingerprints, and lives inside `PipelineConfig`
//!   (which `pipelines check` and the serve gate parse — parsing must be
//!   network-free, .so-free and credential-free).
//! * [`LoadedEmbedder`] is the RUNTIME object produced by
//!   [`EmbedderKind::load`]: real tokenizer/session/HTTP client. It is NOT
//!   `Clone` and not serializable — loading is where the startup contract is
//!   enforced (ort dim probe, api-key resolution, endpoint scheme) and where
//!   side effects are allowed (missing-model prefetch when not offline).
//!   T3's ingest worker owns one `LoadedEmbedder` per configured instance.
//!
//! Both are closed enums dispatched by exhaustive match — zero vtables; `dyn`
//! remains reserved for the v2 wasm slot and the contract tests.
//!
//! # DIP boundary
//!
//! ort/tokenizers/HTTP types stay inside `ort.rs` / `openai.rs` /
//! [`http`]. This module sees only the two config structs and the two
//! runtime structs.
//!
//! # Kinds (M2)
//!
//! * `ort` — local ONNX Runtime inference (dynamic .so; gte family defaults).
//! * `openai` — remote OpenAI-compatible /embeddings API over hand-rolled
//!   HTTP.

pub mod http;
pub mod openai;
pub mod ort;

use super::{Chunk, Modality, PipelineError, Query};
use openai::{OpenAiConfig, OpenAiEmbedder};
use ort::{OrtConfig, OrtEmbedder};

/// The loud refusal every text-only kind returns from `embed_query` for a
/// non-text query (M3-T1): the query-side twin of the ingest worker's
/// doc-modality gate. Called from the `else` arms of the kinds' `Text`
/// matches — the message must say THIS embedder does not take image
/// queries, because silently embedding nothing (or the empty text channel)
/// would corrupt the search fan-out.
pub(crate) fn query_modality_refusal(q: &Query) -> PipelineError {
    PipelineError::embed(format!(
        "this embedder does not accept {} — its query side is text-only; an image query needs \
         an image-capable embedder kind (planned: siglip2, M3-T2)",
        q.describe()
    ))
}

/// The loud refusal a text-only kind returns from `embed_documents` when an
/// image chunk reaches it (M3-T1 defensive seam, the token-window
/// defensive-skip discipline): the ingest router fans image chunks to
/// doc-modality-matching embedders only, so an image chunk here is a
/// routing bug — and embedding its empty text channel would silently
/// produce garbage vectors, which is worse than failing.
pub(crate) fn image_chunk_refusal(chunk: &Chunk) -> PipelineError {
    PipelineError::embed(format!(
        "chunk {} is an image chunk but this embedder embeds text documents only — the ingest \
         router must fan image chunks to an image-capable embedder; an image chunk reaching a \
         text-only kind is a routing bug, and embedding its empty text silently would be worse",
        chunk.index
    ))
}

/// The available-kind list embedded in unknown-kind errors.
pub const EMBEDDER_KINDS: &str = "ort, openai";

/// Registered embedder kinds (the CONFIG object; see module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum EmbedderKind {
    /// Local ONNX inference (embed/ort.rs).
    Ort(OrtConfig),
    /// Remote OpenAI-compatible provider (embed/openai.rs).
    OpenAi(OpenAiConfig),
}

impl EmbedderKind {
    /// Two-stage config parse (§5.10.2): stage 1 reads the `kind` string,
    /// stage 2 deserializes the per-kind struct with `deny_unknown_fields`
    /// (unknown-field errors get section context + line via
    /// `config::stage2_error`).
    pub fn from_config(
        section: &str,
        raw: &toml::Value,
        line: Option<usize>,
    ) -> Result<Self, PipelineError> {
        let kind = super::config::probe_kind(raw)
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
        match kind.as_str() {
            "ort" => {
                let cfg: OrtConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(EmbedderKind::Ort(cfg))
            }
            "openai" => {
                let cfg: OpenAiConfig = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                Ok(EmbedderKind::OpenAi(cfg))
            }
            other => Err(PipelineError::config(
                format!(
                    "[{section}]: unknown embedder kind '{other}' (available kinds: \
                     {EMBEDDER_KINDS})"
                ),
                line,
            )),
        }
    }

    /// Modalities accepted on the DOCUMENT side — drives the §5.10.3
    /// modality carry-over check at config time. Both M2 kinds are text-only;
    /// M3's image/audio kinds add values next to these arms.
    pub fn doc_modalities(&self) -> &'static [Modality] {
        match self {
            EmbedderKind::Ort(_) | EmbedderKind::OpenAi(_) => &[Modality::Text],
        }
    }

    /// Modalities accepted on the QUERY side.
    pub fn query_modalities(&self) -> &'static [Modality] {
        match self {
            EmbedderKind::Ort(_) | EmbedderKind::OpenAi(_) => &[Modality::Text],
        }
    }

    /// Human-readable kind + params (CLI summary lines). NOTE: deliberately
    /// excludes `api_key` (secrets do not belong in summaries) — the same
    /// rule `describe()` output will follow for every future kind. The model
    /// identity shown is the RESOLVED name (ort::resolve_model_name): the
    /// artifact under `path` is what embeds, so a path-only embedder is
    /// described by its directory, never by the default name.
    pub fn describe(&self) -> String {
        match self {
            EmbedderKind::Ort(c) => match &c.path {
                Some(p) => format!("ort(path={p})"),
                None => {
                    format!(
                        "ort(model={}{})",
                        ort::resolve_model_name(c),
                        if c.offline { ", offline" } else { "" }
                    )
                }
            },
            EmbedderKind::OpenAi(c) => format!(
                "openai(model={}, dim={}, endpoint={})",
                c.model, c.dim, c.endpoint
            ),
        }
    }

    /// Build the runtime object (the STARTUP gate: dim probe for ort,
    /// credential + scheme checks for openai, model prefetch when allowed).
    /// `models_dir` is the resolved cache root (config `models_dir` or the
    /// `<data-dir>/.vectorfs/models` default).
    pub async fn load(
        &self,
        models_dir: &std::path::Path,
    ) -> Result<LoadedEmbedder, PipelineError> {
        match self {
            EmbedderKind::Ort(cfg) => Ok(LoadedEmbedder::Ort(
                OrtEmbedder::new(cfg, models_dir).await?,
            )),
            EmbedderKind::OpenAi(cfg) => Ok(LoadedEmbedder::OpenAi(OpenAiEmbedder::new(cfg)?)),
        }
    }
}

/// The runtime object: one loaded embedder instance (see module docs).
/// Dispatches by exhaustive match — no `dyn`, no Box.
// clippy: the OpenAi variant embeds the HTTP client (a few hundred bytes);
// there is one LoadedEmbedder per configured instance, never in a hot
// collection, so boxing a variant would only add indirection.
#[allow(clippy::large_enum_variant)]
pub enum LoadedEmbedder {
    Ort(OrtEmbedder),
    OpenAi(OpenAiEmbedder),
}

impl LoadedEmbedder {
    /// Same summary as the config object (status lines after load).
    pub fn describe(&self) -> String {
        match self {
            LoadedEmbedder::Ort(_) => "ort".to_string(),
            LoadedEmbedder::OpenAi(_) => "openai".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl super::Embedder for LoadedEmbedder {
    async fn embed_query(&self, q: &super::Query) -> Result<Vec<f32>, PipelineError> {
        match self {
            LoadedEmbedder::Ort(e) => e.embed_query(q).await,
            LoadedEmbedder::OpenAi(e) => e.embed_query(q).await,
        }
    }

    async fn embed_documents(
        &self,
        chunks: &[super::Chunk],
    ) -> Result<Vec<Vec<f32>>, PipelineError> {
        match self {
            LoadedEmbedder::Ort(e) => e.embed_documents(chunks).await,
            LoadedEmbedder::OpenAi(e) => e.embed_documents(chunks).await,
        }
    }

    fn dim(&self) -> usize {
        match self {
            LoadedEmbedder::Ort(e) => e.dim(),
            LoadedEmbedder::OpenAi(e) => e.dim(),
        }
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        match self {
            LoadedEmbedder::Ort(e) => e.doc_modalities(),
            LoadedEmbedder::OpenAi(e) => e.doc_modalities(),
        }
    }

    fn query_modalities(&self) -> &'static [Modality] {
        match self {
            LoadedEmbedder::Ort(e) => e.query_modalities(),
            LoadedEmbedder::OpenAi(e) => e.query_modalities(),
        }
    }
}

/// Fingerprint-facing serialization: the INNER config only — the kind tag is
/// already a field of every per-kind config struct, keeping the canonical
/// form flat (`kind = "ort"`, …) like the extractor/chunker axes.
impl serde::Serialize for EmbedderKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            EmbedderKind::Ort(c) => c.serialize(serializer),
            EmbedderKind::OpenAi(c) => c.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_parses_both_kinds() {
        let raw: toml::Value =
            toml::from_str("kind = \"ort\"\nmodel = \"gte-multilingual-base\"").unwrap();
        let k = EmbedderKind::from_config("embedder.text", &raw, Some(7)).unwrap();
        match &k {
            EmbedderKind::Ort(c) => {
                assert_eq!(c.model.as_deref(), Some("gte-multilingual-base"));
                assert!(!c.offline);
            }
            other => panic!("wrong kind: {other:?}"),
        }
        assert_eq!(k.doc_modalities(), &[Modality::Text]);
        assert_eq!(k.query_modalities(), &[Modality::Text]);
        assert_eq!(
            k.describe(),
            "ort(model=gte-multilingual-base)",
            "no-path describe omits empty suffix"
        );

        let raw: toml::Value = toml::from_str(
            "kind = \"openai\"\nendpoint = \"https://api.bigmodel.cn/v1\"\nmodel = \
             \"embedding-3\"\ndim = 2048",
        )
        .unwrap();
        let k = EmbedderKind::from_config("embedder.remote", &raw, Some(1)).unwrap();
        assert!(k.describe().contains("dim=2048"));
        assert!(!k.describe().contains("api_key"), "no secrets in describe");
    }

    #[test]
    fn from_config_unknown_kind_lists_available() {
        let raw: toml::Value = toml::from_str("kind = \"fastembed\"").unwrap();
        let err = EmbedderKind::from_config("embedder.x", &raw, Some(3)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown embedder kind 'fastembed'"), "{msg}");
        assert!(msg.contains("available kinds: ort, openai"), "{msg}");
        assert!(msg.contains("at line 3"), "{msg}");
    }

    #[test]
    fn from_config_unknown_field_is_shaped_with_line() {
        let raw: toml::Value =
            toml::from_str("kind = \"ort\"\nmodel = \"x\"\npool_size = 4").unwrap();
        let err = EmbedderKind::from_config("embedder.t", &raw, Some(2)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[embedder.t]: unknown field 'pool_size' for kind 'ort'"),
            "{msg}"
        );
        assert!(msg.contains("allowed: kind, model, path, offline"), "{msg}");
        assert!(msg.contains("at line 2"), "{msg}");
    }

    #[test]
    fn missing_kind_reported() {
        let raw: toml::Value = toml::from_str("model = \"x\"").unwrap();
        let err = EmbedderKind::from_config("embedder.e", &raw, None).unwrap_err();
        assert!(err.to_string().contains("missing field 'kind'"), "{err}");
    }

    #[test]
    fn fingerprint_serialization_is_flat() {
        let raw: toml::Value = toml::from_str(
            "kind = \"openai\"\nendpoint = \"https://e/v1\"\
             \nmodel = \"m\"\ndim = 8",
        )
        .unwrap();
        let k = EmbedderKind::from_config("e", &raw, None).unwrap();
        assert_eq!(
            toml::to_string(&k).unwrap().trim(),
            "kind = \"openai\"\nendpoint = \"https://e/v1\"\nmodel = \"m\"\ndim = 8"
        );
    }
}
