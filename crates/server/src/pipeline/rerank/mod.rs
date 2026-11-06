//! Reranker axis (M3-T11, design.md §5.10.2 Reranker row): the FOURTH
//! strategy axis and the second QUERY-side one. Contract:
//! `query × candidates -> scored` — one score per candidate, the output a
//! deterministic total order (score descending, ties broken by key
//! ascending), and **the candidates' input ORDER never influences the
//! result** (neither the scores nor the order).
//!
//! # Placement (§5.10.3 data flow: `merge -> (rerank, M3) -> 回源`)
//!
//! Rerank runs AFTER the per-route lists merge and BEFORE the wire shapes
//! hits — it re-scores the merged candidate set (already truncated to the
//! request's topK, so "the candidates" is exactly what would be returned)
//! with a cross-encoder that reads the PAIR (query, candidate chunk text)
//! jointly. This is the precision half of the single-vector-recall +
//! rerank industry shape (§0 multimodal evidence row).
//!
//! # Availability shape (the `fts` precedent, one knob)
//!
//! `[ingest.<bucket>] rerank = true` ships OFF (§7: M3 superset content
//! default-off) and means: the bucket's reranker model LOADS at serve
//! startup (dim/output probe, prefetch when online) and `/x-vf/Search`
//! accepts the per-request `rerank: true`. The table form
//! `rerank = { kind = "cross-encoder", … }` carries the model reference
//! (`model` / `path` / `offline`, the OrtConfig field set). Like `merge`
//! and `fts`, the whole knob is QUERY-SIDE ranking policy — it writes
//! NOTHING to any index and never changes chunk identity, so it stays OUT
//! of the pipeline fingerprint (the `merge-weights` ruling: "changing a
//! weight changes ranking policy, never chunk identity"; the reranker
//! model is not index-defining the way an embedder model is).
//!
//! # Registration model (§5.10.2, closed enum)
//!
//! One kind today — `cross-encoder` (an ort session over a
//! sequence-classification ONNX graph, gte-multilingual-reranker-base the
//! default model). M2's single-kind precedent is the chunker axis's
//! token-window era: `RerankerKind` has one variant, but the availability
//! string, the label table, `kind_label` and `from_config` are already the
//! four parity-pinned views a SECOND kind must update together (the test
//! below pins it; the compiler walks the exhaustive matches).
//!
//! # Determinism discipline (the M3-T2 siglip2 batch-determinism precedent)
//!
//! A cross-encoder scores each (query, doc) pair INDEPENDENTLY — attention
//! with a mask cannot mix rows — but float reduction order inside one
//! `session.run` CAN depend on the batch SHAPE. The kind therefore sorts
//! candidates BY KEY before tokenizing and packs mini-batches over the
//! sorted order: the same candidate SET produces byte-identical batch
//! shapes (hence scores) whatever order the caller presented it in. The
//! contract suite pins this end-to-end.

pub mod cross_encoder;

use super::PipelineError;

use cross_encoder::{CrossEncoderConfig, CrossEncoderReranker};

/// The available-kind list embedded in unknown-kind errors (the
/// extractor/embedder/chunker axis anchor discipline).
pub const RERANKER_KINDS: &str = "cross-encoder";

/// Every kind's config label, IN AVAILABILITY-LIST ORDER (parity-pinned by
/// the test below; a new kind lands here, in `kind_label` and in
/// `from_config` in one diff).
pub const KIND_LABELS: &[&str] = &["cross-encoder"];

/// Registered reranker kinds (the CONFIG object; see module docs). `None`
/// at the ingest-config level = the axis is off for that bucket.
#[derive(Debug, Clone, PartialEq)]
pub enum RerankerKind {
    CrossEncoder(CrossEncoderConfig),
}

impl RerankerKind {
    /// Two-stage config parse for the TABLE form
    /// (`rerank = { kind = "cross-encoder", … }`); `section` is the ingest
    /// section path for error context, `line` its TOML line (the §5.10.4
    /// attribution rule shared with the inline chunker).
    pub fn from_config(
        section: &str,
        raw: &toml::Value,
        line: Option<usize>,
    ) -> Result<Self, PipelineError> {
        let kind = super::config::probe_kind(raw)
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
        match kind.as_str() {
            "cross-encoder" => {
                let cfg: CrossEncoderConfig =
                    raw.clone().try_into().map_err(|e: toml::de::Error| {
                        super::config::stage2_error(section, Some(&kind), e.message(), line)
                    })?;
                Ok(RerankerKind::CrossEncoder(cfg))
            }
            other => Err(PipelineError::config(
                format!(
                    "[{section}]: unknown reranker kind '{other}' (available kinds: \
                     {RERANKER_KINDS})"
                ),
                line,
            )),
        }
    }

    /// The kind's REGISTRY label (the `kind_label` discipline).
    pub fn kind_label(&self) -> &'static str {
        match self {
            RerankerKind::CrossEncoder(_) => "cross-encoder",
        }
    }

    /// Human-readable kind + params (CLI summary lines; no secrets).
    pub fn describe(&self) -> String {
        match self {
            RerankerKind::CrossEncoder(c) => match &c.path {
                Some(p) => format!("cross-encoder(path={p})"),
                None => format!(
                    "cross-encoder(model={}{})",
                    cross_encoder::resolve_model_name(c),
                    if c.offline { ", offline" } else { "" }
                ),
            },
        }
    }

    /// Build the runtime object (the STARTUP gate: model resolution,
    /// prefetch-when-online, tokenizer/session load, output-shape probe).
    pub async fn load(
        &self,
        models_dir: &std::path::Path,
    ) -> Result<CrossEncoderReranker, PipelineError> {
        match self {
            RerankerKind::CrossEncoder(cfg) => CrossEncoderReranker::new(cfg, models_dir).await,
        }
    }
}

/// Fingerprint-facing serialization: the INNER config only (the kind tag is
/// a field of the config struct — the three axes' canonical flat form). The
/// reranker is query-side and deliberately NOT serialized into the
/// fingerprint today; this impl exists so the axis keeps the same
/// serialize-the-inner-config discipline the moment a stored artifact ever
/// depends on it.
impl serde::Serialize for RerankerKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RerankerKind::CrossEncoder(c) => c.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_parses_and_rejects_unknown_kind() {
        let raw: toml::Value =
            toml::from_str("kind = \"cross-encoder\"\nmodel = \"gte-multilingual-reranker-base\"")
                .unwrap();
        let k = RerankerKind::from_config("ingest.d.rerank", &raw, Some(4)).unwrap();
        assert_eq!(k.kind_label(), "cross-encoder");
        assert_eq!(
            k.describe(),
            "cross-encoder(model=gte-multilingual-reranker-base)"
        );
        // unknown kind: closed-set refusal naming the menu, with the line
        let raw: toml::Value = toml::from_str("kind = \"listwise\"").unwrap();
        let err = RerankerKind::from_config("ingest.d.rerank", &raw, Some(7)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown reranker kind 'listwise' (available kinds: cross-encoder)"),
            "{msg}"
        );
        assert!(msg.contains("at line 7"), "{msg}");
        // unknown field is shaped with kind + allowed list
        let raw: toml::Value =
            toml::from_str("kind = \"cross-encoder\"\nmodel = \"x\"\ntop_n = 50").unwrap();
        let err = RerankerKind::from_config("ingest.d.rerank", &raw, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field 'top_n' for kind 'cross-encoder'"),
            "{msg}"
        );
        assert!(msg.contains("allowed: kind, model, path, offline"), "{msg}");
    }

    /// THE parity test (the single-kind registration face, chunker-axis
    /// token-window era precedent): availability string == label table ==
    /// `kind_label` == `from_config` are views of one registry.
    #[test]
    fn kind_registry_views_agree() {
        assert_eq!(RERANKER_KINDS, KIND_LABELS.join(", "));
        for label in KIND_LABELS {
            let raw: toml::Value = toml::from_str(&format!("kind = \"{label}\"")).unwrap();
            let k = RerankerKind::from_config("x", &raw, None)
                .unwrap_or_else(|e| panic!("KIND_LABELS entry {label} must parse: {e}"));
            assert_eq!(k.kind_label(), *label);
            assert!(k.describe().starts_with(label), "{}", k.describe());
        }
        // the bare kind parses with the DEFAULT model resolved at load time
        // (the reranker DOES have a default tier — it shipped adjudicated,
        // §0 2026-11-07 — unlike siglip2's deliberate no-default era)
        let raw: toml::Value = toml::from_str("kind = \"cross-encoder\"").unwrap();
        let k = RerankerKind::from_config("x", &raw, None).unwrap();
        assert_eq!(
            k.describe(),
            "cross-encoder(model=gte-multilingual-reranker-base)"
        );
    }
}
