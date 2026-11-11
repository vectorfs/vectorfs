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
//! # Kinds (M2/M3)
//!
//! * `ort` — local ONNX Runtime inference (dynamic .so; gte family defaults).
//! * `openai` — remote OpenAI-compatible /embeddings API over hand-rolled
//!   HTTP.
//! * `siglip2` — local ONNX Runtime image embedder, SigLIP 2 split towers
//!   (embed/siglip2.rs, M3-T2: doc side Image, query side Image + Text —
//!   the first doc modality ≠ Text kind).

pub mod http;
pub mod openai;
pub mod ort;
pub mod siglip2;

use super::{Chunk, Modality, PipelineError, Query};
use openai::{OpenAiConfig, OpenAiEmbedder};
use ort::{OrtConfig, OrtEmbedder};
use siglip2::{Siglip2Config, Siglip2Embedder};

/// The loud refusal every text-only kind returns from `embed_query` for a
/// non-text query (M3-T1): the query-side twin of the ingest worker's
/// doc-modality gate. Called from the `else` arms of the kinds' `Text`
/// matches — the message must say THIS embedder does not take image
/// queries, because silently embedding nothing (or the empty text channel)
/// would corrupt the search fan-out.
pub(crate) fn query_modality_refusal(q: &Query) -> PipelineError {
    PipelineError::embed(format!(
        "this embedder does not accept {} — its query side is text-only; an image query needs \
         an image-capable embedder kind (the siglip2 kind accepts image queries)",
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

/// The loud refusal an image-only kind returns from `embed_documents` when
/// a text chunk reaches it (M3-T2, the twin of [`image_chunk_refusal`]):
/// the worker scopes chunks by doc modality, so a text chunk at the siglip2
/// kind is a routing bug — and there is no honest image to preprocess.
pub(crate) fn text_chunk_refusal(chunk: &Chunk) -> PipelineError {
    PipelineError::embed(format!(
        "chunk {} is a text chunk but this embedder embeds image documents only — the ingest \
         router scopes chunks by doc modality, so a text chunk here is a routing bug (a text \
         chunk has no image payload to preprocess; failing loudly beats embedding garbage)",
        chunk.index
    ))
}

/// The text one chunk's EMBEDDING INPUT (R2 CCH): the heading-path prefix,
/// a newline, then the chunk's ORIGINAL text — or the text alone when no
/// prefix was stamped. This is the ONE composition site of the R2 hard
/// boundary: every text embedder kind feeds its model exactly this, while
/// [`Chunk::text`], the stored `x-vf-chunk-text` and the FTS index's text
/// column stay the original everywhere else. The lexical arm's separation
/// is load-bearing, not incidental: R1's anchor gating reads df statistics
/// off the FTS index, and a chapter title repeated into every chunk of
/// its chapter would inflate exactly the postings counts that ceiling is
/// calibrated against — the prefix must never reach BM25.
pub(crate) fn embed_input(c: &Chunk) -> String {
    match &c.embed_prefix {
        Some(prefix) => format!("{prefix}\n{}", c.text),
        None => c.text.clone(),
    }
}

/// The available-kind list embedded in unknown-kind errors.
pub const EMBEDDER_KINDS: &str = "ort, openai, siglip2";

/// Every kind's config label, IN AVAILABILITY-LIST ORDER — the embedder
/// axis twin of the extractor axis's `KIND_LABELS` anchor: `kind_label`
/// (the exhaustive match the compiler walks on a new variant) and the
/// parity test below pin the three views together (enum variants /
/// `from_config` arms / this table + [`EMBEDDER_KINDS`]).
pub const KIND_LABELS: &[&str] = &["ort", "openai", "siglip2"];

/// Registered embedder kinds (the CONFIG object; see module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum EmbedderKind {
    /// Local ONNX inference (embed/ort.rs).
    Ort(OrtConfig),
    /// Remote OpenAI-compatible provider (embed/openai.rs).
    OpenAi(OpenAiConfig),
    /// Local SigLIP 2 image embedder, split towers (embed/siglip2.rs).
    Siglip2(Siglip2Config),
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
                // b1 cross-field coherence, fail-fast at PARSE time with the
                // section line (the siglip2 no-default precedent — these are
                // pure config facts, knowable before any credential exists):
                // * dimensions is the MRL truncation REQUESTED from the
                //   provider; dim is the DECLARED output. One number, not
                //   two — a mismatch would fail every realign at runtime.
                // * the instruction fields need the messages dialect; the
                //   legacy input wire has no instruction channel to carry
                //   them (silently ignoring a configured field is worse).
                if let Some(d) = cfg.dimensions.filter(|&d| d != cfg.dim) {
                    return Err(PipelineError::config(
                        format!(
                            "[{section}]: dimensions = {d} must EQUAL dim = {} — dimensions \
                             is the MRL truncation requested from the provider and dim is \
                             the declared output dimensionality; set BOTH to the truncated \
                             size (or drop dimensions for the model's native output)",
                            cfg.dim
                        ),
                        line,
                    ));
                }
                if !cfg.multimodal
                    && (cfg.query_instruction.is_some() || cfg.doc_instruction.is_some())
                {
                    return Err(PipelineError::config(
                        format!(
                            "[{section}]: query_instruction / doc_instruction require \
                             multimodal = true — the legacy {{input}} wire has no instruction \
                             channel; the messages dialect carries instructions as the system \
                             message"
                        ),
                        line,
                    ));
                }
                Ok(EmbedderKind::OpenAi(cfg))
            }
            "siglip2" => {
                let cfg: Siglip2Config = raw.clone().try_into().map_err(|e: toml::de::Error| {
                    super::config::stage2_error(section, Some(&kind), e.message(), line)
                })?;
                // fail-fast at PARSE time: the siglip2 kind deliberately has
                // no default tier (which model becomes the preset default is
                // M3-T4's adjudication), so a bare kind = "siglip2" must
                // name the built-in rather than silently pick one
                if cfg.model.is_none() && cfg.path.is_none() {
                    return Err(PipelineError::config(
                        format!(
                            "[{section}]: the siglip2 kind requires model = \"{}\" (or another \
                             built-in name) or path = \"…\" — which tier becomes the default is \
                             deliberately not baked in (M3-T4 preset adjudication)",
                            siglip2::BUILTIN
                        ),
                        line,
                    ));
                }
                Ok(EmbedderKind::Siglip2(cfg))
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

    /// The kind's REGISTRY label — the `kind = "…"` config string, one per
    /// variant, exhaustively matched so a new kind MUST land here (the
    /// extractor-axis `kind_label` discipline; the parity test below pins
    /// the table views to it).
    pub fn kind_label(&self) -> &'static str {
        match self {
            EmbedderKind::Ort(_) => "ort",
            EmbedderKind::OpenAi(_) => "openai",
            EmbedderKind::Siglip2(_) => "siglip2",
        }
    }

    /// Modalities accepted on the DOCUMENT side — drives the §5.10.3
    /// modality carry-over check at config time. The M2 kinds are text-only;
    /// siglip2 (M3-T2) is the first image-document kind; the openai kind
    /// with `multimodal = true` (b1) is the first kind accepting BOTH — a
    /// unified embedder whose gate must stay config-driven so pre-b1
    /// deployments keep their text/text matrix byte-identically.
    pub fn doc_modalities(&self) -> &'static [Modality] {
        match self {
            EmbedderKind::Ort(_) => &[Modality::Text],
            EmbedderKind::OpenAi(c) => {
                if c.multimodal {
                    &[Modality::Text, Modality::Image]
                } else {
                    &[Modality::Text]
                }
            }
            EmbedderKind::Siglip2(_) => &[Modality::Image],
        }
    }

    /// Modalities accepted on the QUERY side.
    pub fn query_modalities(&self) -> &'static [Modality] {
        match self {
            EmbedderKind::Ort(_) => &[Modality::Text],
            EmbedderKind::OpenAi(c) => {
                if c.multimodal {
                    &[Modality::Text, Modality::Image]
                } else {
                    &[Modality::Text]
                }
            }
            // 以图搜图 (vision tower) + 文字搜图 (text tower; the tokenizers
            // investigation came back clean — see siglip2.rs module docs)
            EmbedderKind::Siglip2(_) => &[Modality::Image, Modality::Text],
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
                "openai(model={}, dim={}, endpoint={}{})",
                c.model,
                c.dim,
                c.endpoint,
                if c.multimodal { ", multimodal" } else { "" }
            ),
            EmbedderKind::Siglip2(c) => match &c.path {
                Some(p) => format!("siglip2(path={p})"),
                None => {
                    format!(
                        "siglip2(model={}{})",
                        siglip2::resolve_model_name(c),
                        if c.offline { ", offline" } else { "" }
                    )
                }
            },
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
            EmbedderKind::Siglip2(cfg) => Ok(LoadedEmbedder::Siglip2(
                Siglip2Embedder::new(cfg, models_dir).await?,
            )),
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
    Siglip2(Siglip2Embedder),
}

impl LoadedEmbedder {
    /// Same summary as the config object (status lines after load).
    pub fn describe(&self) -> String {
        match self {
            LoadedEmbedder::Ort(_) => "ort".to_string(),
            LoadedEmbedder::OpenAi(_) => "openai".to_string(),
            LoadedEmbedder::Siglip2(_) => "siglip2".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl super::Embedder for LoadedEmbedder {
    async fn embed_query(&self, q: &super::Query) -> Result<Vec<f32>, PipelineError> {
        match self {
            LoadedEmbedder::Ort(e) => e.embed_query(q).await,
            LoadedEmbedder::OpenAi(e) => e.embed_query(q).await,
            LoadedEmbedder::Siglip2(e) => e.embed_query(q).await,
        }
    }

    async fn embed_documents(
        &self,
        chunks: &[super::Chunk],
    ) -> Result<Vec<Vec<f32>>, PipelineError> {
        match self {
            LoadedEmbedder::Ort(e) => e.embed_documents(chunks).await,
            LoadedEmbedder::OpenAi(e) => e.embed_documents(chunks).await,
            LoadedEmbedder::Siglip2(e) => e.embed_documents(chunks).await,
        }
    }

    fn dim(&self) -> usize {
        match self {
            LoadedEmbedder::Ort(e) => e.dim(),
            LoadedEmbedder::OpenAi(e) => e.dim(),
            LoadedEmbedder::Siglip2(e) => e.dim(),
        }
    }

    fn doc_modalities(&self) -> &'static [Modality] {
        match self {
            LoadedEmbedder::Ort(e) => e.doc_modalities(),
            LoadedEmbedder::OpenAi(e) => e.doc_modalities(),
            LoadedEmbedder::Siglip2(e) => e.doc_modalities(),
        }
    }

    fn query_modalities(&self) -> &'static [Modality] {
        match self {
            LoadedEmbedder::Ort(e) => e.query_modalities(),
            LoadedEmbedder::OpenAi(e) => e.query_modalities(),
            LoadedEmbedder::Siglip2(e) => e.query_modalities(),
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
            EmbedderKind::Siglip2(c) => c.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R2 CCH: the ONE composition site — the embedding input is prefix +
    /// "\n" + text when a prefix was stamped, the text alone otherwise.
    /// Everything else in the system (storage, FTS, returned text) reads
    /// `Chunk::text` directly and never sees the prefix.
    #[test]
    fn embed_input_composes_prefix_newline_text() {
        let plain = super::Chunk {
            index: 0,
            text: "正文块".to_string(),
            image: None,
            kind: crate::pipeline::ChunkKind::Body,
            page: None,
            line: None,
            section: None,
            embed_prefix: None,
        };
        assert_eq!(embed_input(&plain), "正文块");
        let prefixed = super::Chunk {
            embed_prefix: Some("vol32 > 试验".to_string()),
            text: plain.text.clone(),
            ..plain.clone()
        };
        assert_eq!(embed_input(&prefixed), "vol32 > 试验\n正文块");
        // empty prefix never occurs (the chunker stamps None, not ""), but
        // the composition stays total if one ever did
        let empty = super::Chunk {
            embed_prefix: Some(String::new()),
            ..plain.clone()
        };
        assert_eq!(embed_input(&empty), "\n正文块");
    }

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
        assert!(
            msg.contains("available kinds: ort, openai, siglip2"),
            "{msg}"
        );
        assert!(msg.contains("at line 3"), "{msg}");
    }

    /// THE embedder-axis parity test (plugin-axis hardening, the extractor
    /// axis's `kind_registry_views_agree` twin): the availability string,
    /// the label table, the exhaustive `kind_label` match and `from_config`
    /// are four views of one registry — a new kind that updates some but not
    /// all of them fails HERE instead of drifting in production error
    /// messages. Modality declarations ride along: the view that ROUTES
    /// (doc/query modalities) is pinned next to the identity views.
    #[test]
    fn kind_registry_views_agree() {
        assert_eq!(EMBEDDER_KINDS, KIND_LABELS.join(", "));
        let minimal = |label: &str| -> toml::Value {
            // the least config each kind parses with (siglip2 has no default
            // tier, so its minimal form names the built-in; openai has
            // required endpoint/model/dim)
            match label {
                "siglip2" => toml::from_str(&format!(
                    "kind = \"{label}\"\nmodel = \"{}\"",
                    siglip2::BUILTIN
                ))
                .unwrap(),
                "openai" => toml::from_str(
                    "kind = \"openai\"\nendpoint = \"https://e/v1\"\nmodel = \"m\"\ndim = 8",
                )
                .unwrap(),
                other => toml::from_str(&format!("kind = \"{other}\"")).unwrap(),
            }
        };
        for label in KIND_LABELS {
            let k = EmbedderKind::from_config("x", &minimal(label), None)
                .unwrap_or_else(|e| panic!("KIND_LABELS entry {label} must parse: {e}"));
            assert_eq!(k.kind_label(), *label);
            assert!(k.describe().starts_with(label), "{}", k.describe());
        }
        // the closed modality matrix (the routing view): text kinds stay
        // text/text; siglip2 is image-doc, image+text-query
        let ort = EmbedderKind::from_config("x", &minimal("ort"), None).unwrap();
        assert_eq!(ort.doc_modalities(), &[Modality::Text]);
        assert_eq!(ort.query_modalities(), &[Modality::Text]);
        let sig = EmbedderKind::from_config("x", &minimal("siglip2"), None).unwrap();
        assert_eq!(sig.doc_modalities(), &[Modality::Image]);
        assert_eq!(sig.query_modalities(), &[Modality::Image, Modality::Text]);
    }

    /// The siglip2 no-default-tier rule is a PARSE-time error with the
    /// section line (fail-fast, §5.10.3) — not a load-time surprise.
    #[test]
    fn bare_siglip2_without_model_or_path_is_shaped_with_line() {
        let raw: toml::Value = toml::from_str("kind = \"siglip2\"").unwrap();
        let err = EmbedderKind::from_config("embedder.img", &raw, Some(5)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[embedder.img]"), "{msg}");
        assert!(msg.contains("requires model"), "{msg}");
        assert!(msg.contains(siglip2::BUILTIN), "{msg}");
        assert!(msg.contains("M3-T4"), "{msg}");
        assert!(msg.contains("at line 5"), "{msg}");
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

    /// b1: the gate flips the openai kind's modality matrix — config-gated,
    /// bilateral, and visible in `describe` (the `pipelines check` summary
    /// face). Off = the pre-b1 text/text matrix, pinned right next to it.
    #[test]
    fn openai_multimodal_gate_flips_the_modality_matrix_and_describe() {
        let raw = |extra: &str| {
            toml::from_str(&format!(
                "kind = \"openai\"\nendpoint = \"https://e/v1\"\nmodel = \"m\"\ndim = 8\n{extra}"
            ))
            .unwrap()
        };
        // off (the additive red line): text/text, no multimodal in describe
        let off = EmbedderKind::from_config("e", &raw(""), None).unwrap();
        assert_eq!(off.doc_modalities(), &[Modality::Text]);
        assert_eq!(off.query_modalities(), &[Modality::Text]);
        assert_eq!(
            off.describe(),
            "openai(model=m, dim=8, endpoint=https://e/v1)"
        );
        // on: BOTH sides [Text, Image] — a unified embedder
        let on = EmbedderKind::from_config("e", &raw("multimodal = true"), None).unwrap();
        assert_eq!(on.doc_modalities(), &[Modality::Text, Modality::Image]);
        assert_eq!(on.query_modalities(), &[Modality::Text, Modality::Image]);
        assert_eq!(
            on.describe(),
            "openai(model=m, dim=8, endpoint=https://e/v1, multimodal)"
        );
        // the fingerprint view: the gate moves the serialized form (a
        // re-gate IS a reindex — the vectors change), and the b1 fields
        // absent keep the flat pre-b1 form (pinned above)
        assert!(toml::to_string(&on).unwrap().contains("multimodal = true"));
    }

    /// b1 parse-time coherence, fail-fast with the section line: dimensions
    /// must equal dim (one number, not two), and the instruction fields
    /// require the messages dialect (the legacy input wire has no
    /// instruction channel).
    #[test]
    fn openai_b1_cross_field_rules_are_shaped_with_line() {
        let raw = |extra: &str| {
            toml::from_str(&format!(
                "kind = \"openai\"\nendpoint = \"https://e/v1\"\nmodel = \"m\"\ndim = 8\n{extra}"
            ))
            .unwrap()
        };
        let err = EmbedderKind::from_config("e", &raw("dimensions = 4"), Some(9)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("dimensions = 4 must EQUAL dim = 8"), "{msg}");
        assert!(msg.contains("set BOTH to the truncated size"), "{msg}");
        assert!(msg.contains("at line 9"), "{msg}");
        // equal is fine (the MRL form): parses
        assert!(EmbedderKind::from_config("e", &raw("dimensions = 8"), None).is_ok());
        // instructions without the gate: refused, naming the fix
        let err =
            EmbedderKind::from_config("e", &raw("query_instruction = \"find charts\""), Some(4))
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("require multimodal = true"), "{msg}");
        assert!(msg.contains("no instruction channel"), "{msg}");
        assert!(msg.contains("at line 4"), "{msg}");
        let err =
            EmbedderKind::from_config("e", &raw("doc_instruction = \"x\""), None).unwrap_err();
        assert!(err.to_string().contains("require multimodal = true"));
        // with the gate: both parse
        assert!(
            EmbedderKind::from_config(
                "e",
                &raw("multimodal = true\nquery_instruction = \"q\"\ndoc_instruction = \"d\""),
                None
            )
            .is_ok()
        );
    }
}
