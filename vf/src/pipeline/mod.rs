//! File-layer pipeline plugin system (design.md §5.10; skeleton in M2-T2,
//! extractor/chunker kinds in T4, embedder kinds + model prefetch in T5).
//!
//! Three strategy axes live behind traits ([`Extractor`], [`Chunker`],
//! [`Embedder`]) plus the query-side merge option ([`merge_hitlists`]);
//! the ORCHESTRATION is fixed mechanism in code, the POLICY (which kinds,
//! which parameters) is TOML (§5.10.0 axiom 1).
//!
//! # Registration model (§5.10.2, Rust form, v2.1)
//!
//! * **Config layer, two-stage**: first read the `kind` STRING from an
//!   instance section, then deserialize the section into the per-kind config
//!   struct (each `deny_unknown_fields`). This is deliberate: serde's
//!   internally-tagged enums do not support `deny_unknown_fields`, so a
//!   single-enum one-shot parse cannot give the fail-fast unknown-key error
//!   the design demands.
//! * **Runtime layer, closed enum dispatch**: each axis has a `*Kind` enum
//!   (`extract::ExtractorKind`, `chunk::ChunkerKind`,
//!   `embed::EmbedderKind`). Adding a kind = adding a variant; every match
//!   is exhaustive, there are ZERO vtables on the hot path. `dyn` is
//!   reserved for the v2 wasm slot (§5.10.8: not in v1) and the contract
//!   tests, which must accept ANY implementation.
//!
//! # DIP boundary (machine-enforced by review, stated here)
//!
//! Engine-native types — ort sessions, pdfium handles, HTTP clients — may
//! only appear INSIDE a kind module (`extract/txt.rs`,
//! `chunk/token_window.rs`, `embed/ort.rs`, `embed/openai.rs` + the shared
//! `embed/http.rs` leaf).
//! This module tree's core (traits, errors, merge, config) sees only the
//! traits and [`PipelineError`]; swapping pdfium for another PDF engine must
//! not touch anything outside `extract/pdf_*.rs`.
//!
//! # Modality is a closed enum (§5.10.1)
//!
//! [`Modality`] deliberately has ONE variant in M2 (`Text`). Adding values
//! in M3+ (`PageImage`, `Image`, `Audio`) is a DESIGNED-FOR event: the
//! compiler walks every exhaustive match and the ingest router fans Parts
//! out by modality with zero routing-config changes. Video is NOT a modality
//! (video extractors demux into the three existing ones).

pub mod chunk;
pub mod config;
pub mod contract;
pub mod embed;
pub mod extract;
pub mod models;

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// data model
// ---------------------------------------------------------------------------

/// Content modality of a [`Part`] / capability declaration of an
/// [`Embedder`]. Closed enum by design (see module docs); `M3+ adds values`
/// is in-design behavior, not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Modality {
    /// Plain UTF-8 text (the only M2 modality).
    Text,
}

impl std::fmt::Display for Modality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Modality::Text => f.write_str("text"),
        }
    }
}

/// One extracted payload. M2 carries text only; the struct grows image/audio
/// payloads in M3/M4 alongside the new [`Modality`] values (additive).
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub modality: Modality,
    pub text: String,
}

impl Part {
    /// Convenience constructor for the M2 text-only shape.
    pub fn text(s: impl Into<String>) -> Self {
        Part {
            modality: Modality::Text,
            text: s.into(),
        }
    }
}

/// A chunker product: the file-layer view of one future vector row.
/// `index` is assigned sequentially by the mechanism across all parts of one
/// source, so chunk keys stay stable and ordered.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub index: u32,
    pub text: String,
}

/// Query-side input. `Image`/`Audio` variants land with their modality
/// (M3/M4); the enum shape is already the router's dispatch point.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Text(String),
}

// ---------------------------------------------------------------------------
// unified error
// ---------------------------------------------------------------------------

/// The ONE error type crossing the pipeline seams (DIP boundary: kind
/// modules translate their native errors into this, never leak them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// Configuration / registration failure. `line` is the 1-based TOML line
    /// the error is attributed to (fail-fast errors must carry it,
    /// design.md §5.10.3), when a span was available.
    Config {
        message: String,
        line: Option<usize>,
    },
    /// Extraction failed (bad bytes, wrong mime, decoder error).
    Extract(String),
    /// Chunking failed.
    Chunk(String),
    /// Embedding failed (model load, inference, provider).
    Embed(String),
    /// Underlying storage/transport I/O failed.
    Io(String),
}

impl PipelineError {
    /// Config error with a known TOML line.
    pub fn config(message: impl Into<String>, line: Option<usize>) -> Self {
        PipelineError::Config {
            message: message.into(),
            line,
        }
    }

    /// Extract error (no line: these are runtime, not config).
    pub fn extract(message: impl Into<String>) -> Self {
        PipelineError::Extract(message.into())
    }

    /// Chunk error.
    pub fn chunk(message: impl Into<String>) -> Self {
        PipelineError::Chunk(message.into())
    }

    /// Embed error.
    pub fn embed(message: impl Into<String>) -> Self {
        PipelineError::Embed(message.into())
    }

    /// Io error.
    pub fn io(message: impl Into<String>) -> Self {
        PipelineError::Io(message.into())
    }
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // One actionable line; `at line N` is the repair hint the
            // fail-fast contract (§5.10.3) requires.
            PipelineError::Config { message, line } => match line {
                Some(n) => write!(f, "{message} at line {n}"),
                None => write!(f, "{message}"),
            },
            PipelineError::Extract(m) => write!(f, "extract failed: {m}"),
            PipelineError::Chunk(m) => write!(f, "chunk failed: {m}"),
            PipelineError::Embed(m) => write!(f, "embed failed: {m}"),
            PipelineError::Io(m) => write!(f, "io failed: {m}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<std::io::Error> for PipelineError {
    fn from(e: std::io::Error) -> Self {
        PipelineError::io(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// the three strategy-axis traits (§5.10.2 signatures, v2.1)
// ---------------------------------------------------------------------------

/// `{bytes, mime} -> Vec<Part>`: turn one Source object's raw bytes into
/// extracted payloads.
///
/// Sync on purpose: extraction is a CPU segment; the ingest worker calls it
/// inside `spawn_blocking` (§5.10.5), and an async signature would only add
/// ceremony every kind would immediately wrap away.
pub trait Extractor: Send + Sync {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError>;
}

/// `&[Part] -> Vec<Chunk>`: split extracted payloads into embeddable chunks.
///
/// The mechanism layer guarantees only TEXT parts are fed (image-class parts
/// go to their embedders atomically, §5.10.2); implementations still skip
/// non-Text parts defensively — a routing bug must not corrupt chunk text.
pub trait Chunker: Send + Sync {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError>;
}

/// Content -> vector engine, both ingestion and query sides. Declares its
/// `dim` and the modalities it accepts; one configured instance maps 1:1 to
/// one derived index (§5.10.1).
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_query(&self, q: &Query) -> Result<Vec<f32>, PipelineError>;
    async fn embed_documents(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError>;
    /// Declared output dimensionality (ort: probed at startup; openai:
    /// declared in config, never probed over the network).
    fn dim(&self) -> usize;
    /// Modalities this instance accepts on the DOCUMENT side (ingestion
    /// routing fans a Part out to every doc-modality-matching embedder).
    fn doc_modalities(&self) -> &'static [Modality];
    /// Modalities accepted on the QUERY side.
    fn query_modalities(&self) -> &'static [Modality];
}

// ---------------------------------------------------------------------------
// query-side merge (pure function; a query OPTION, not an axis)
// ---------------------------------------------------------------------------

/// One merged query result row. Minimal in M2 (key + fused score); T3's
/// hybrid search may add source/modality attribution additively.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub key: String,
    pub score: f64,
}

/// Per-embedder-instance result list fed to [`merge_hitlists`]. Callers
/// guarantee keys are unique WITHIN one list (one list = one index's
/// ranking); the same key MAY appear across lists — that is the point.
pub type HitList = Vec<Hit>;

/// Query-result merge strategy. Closed set like everything else in the
/// plugin system: M2 registers exactly `rrf`; `weighted` / `first` land in
/// M3 (§7 M3 fusion scope) by ADDING variants + parse arms — a config saying
/// `merge = "weighted"` in M2 fails loudly instead of half-working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// Reciprocal Rank Fusion.
    Rrf,
}

impl Merge {
    /// Parse the config string form. M2 accepts exactly `"rrf"`.
    pub fn parse(s: &str) -> Result<Merge, String> {
        match s {
            "rrf" => Ok(Merge::Rrf),
            other => Err(format!(
                "unknown merge strategy '{other}' (allowed: rrf; weighted/first land in M3)"
            )),
        }
    }
}

impl std::fmt::Display for Merge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Merge::Rrf => f.write_str("rrf"),
        }
    }
}

/// RRF smoothing constant, the literature-default k = 60 (Cormack et al.
/// 2009). Damps rank-1 dominance so one list cannot crown an unconfirmed
/// key; exact value is convention, not tuning.
pub const RRF_K: u64 = 60;

/// Fuse per-list rankings into one total order.
///
/// * **Single list**: passthrough identity — same rows, same order, same
///   scores. With nothing to fuse, RRF would only destroy score semantics
///   (1/(k+r) is comparable ACROSS lists, meaningless as an absolute
///   relevance), so the raw scores stay actionable for thresholds/rerankers.
/// * **Multiple lists**: classic RRF — each key accumulates
///   `1/(RRF_K + rank)` per list (rank 1 = best), output sorted by score
///   descending, ties broken by key ascending. The tie-break makes the
///   order a pure function of the input: deterministic and reproducible.
pub fn merge_hitlists(strategy: Merge, lists: &[HitList]) -> HitList {
    match strategy {
        Merge::Rrf => {
            if lists.len() == 1 {
                return lists[0].clone();
            }
            let mut acc: BTreeMap<String, f64> = BTreeMap::new();
            for list in lists {
                for (rank0, hit) in list.iter().enumerate() {
                    *acc.entry(hit.key.clone()).or_insert(0.0) +=
                        1.0 / (RRF_K as f64 + rank0 as f64 + 1.0);
                }
            }
            let mut merged: HitList = acc
                .into_iter()
                .map(|(key, score)| Hit { key, score })
                .collect();
            // total_cmp: a total order over f64 (no NaN by construction —
            // sums of positive rationals), so sort is fully deterministic.
            merged.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
            merged
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(pairs: &[(&str, f64)]) -> HitList {
        pairs
            .iter()
            .map(|(k, s)| Hit {
                key: (*k).to_string(),
                score: *s,
            })
            .collect()
    }

    #[test]
    fn merge_single_list_is_identity() {
        let one = hits(&[("a", 0.9), ("b", 0.5), ("c", 0.1)]);
        let out = merge_hitlists(Merge::Rrf, &[one.clone()]);
        assert_eq!(out, one, "single list must keep order AND raw scores");
        // byte-identical scores, not just order
        assert_eq!(out[0].score.to_bits(), 0.9f64.to_bits());
    }

    #[test]
    fn merge_rrf_fuses_and_orders() {
        let l1 = hits(&[("a", 9.0), ("b", 8.0)]);
        let l2 = hits(&[("c", 7.0), ("a", 6.0)]);
        let out = merge_hitlists(Merge::Rrf, &[l1, l2]);
        // a = 1/61 + 1/62 beats b = 1/62 and c = 1/61
        assert_eq!(out[0].key, "a");
        assert_eq!(
            out.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        let expected = 1.0 / 61.0 + 1.0 / 62.0;
        assert!((out[0].score - expected).abs() < 1e-15);
        // union preserved, no duplicates
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn merge_tie_breaks_by_key_deterministically() {
        // a is rank-1 of list1, b is rank-1 of list2: identical fused scores.
        let l1 = hits(&[("b", 1.0), ("x", 0.5)]);
        let l2 = hits(&[("a", 1.0), ("y", 0.5)]);
        let out = merge_hitlists(Merge::Rrf, &[l1.clone(), l2.clone()]);
        assert_eq!(out[0].score.to_bits(), out[1].score.to_bits(), "tie setup");
        assert!(out[0].key < out[1].key, "ties break by key ascending");
        // total order: non-increasing scores, equal-score runs key-sorted
        for w in out.windows(2) {
            let ok = w[0].score > w[1].score || (w[0].score == w[1].score && w[0].key < w[1].key);
            assert!(ok, "not totally ordered: {out:?}");
        }
        // determinism: same input, same output
        assert_eq!(merge_hitlists(Merge::Rrf, &[l1, l2]), out);
    }

    #[test]
    fn merge_edges() {
        assert!(merge_hitlists(Merge::Rrf, &[]).is_empty());
        assert!(merge_hitlists(Merge::Rrf, &[HitList::new(), HitList::new()]).is_empty());
        // same key ranked 1st in one list, 2nd in the other: contributions
        // accumulate
        let out = merge_hitlists(
            Merge::Rrf,
            &[hits(&[("k", 1.0)]), hits(&[("j", 0.9), ("k", 0.5)])],
        );
        assert_eq!(out.len(), 2);
        assert!((out[0].score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-15);
        // single-list passthrough keeps even a duplicated key as-is
        // (keys unique per list is the CALLER contract; merge does not
        // second-guess one index's ranking)
        let dup = hits(&[("k", 1.0), ("k", 0.5)]);
        assert_eq!(merge_hitlists(Merge::Rrf, &[dup.clone()]), dup);
    }

    #[test]
    fn merge_parse_closed_set() {
        assert_eq!(Merge::parse("rrf"), Ok(Merge::Rrf));
        let err = Merge::parse("weighted").unwrap_err();
        assert!(err.contains("unknown merge strategy 'weighted'"), "{err}");
        assert!(err.contains("M3"), "{err}");
    }

    #[test]
    fn pipeline_error_display_carries_line() {
        let e = PipelineError::config("[ingest.d]: bad thing", Some(14));
        assert_eq!(e.to_string(), "[ingest.d]: bad thing at line 14");
        let e = PipelineError::config("[ingest.d]: bad thing", None);
        assert_eq!(e.to_string(), "[ingest.d]: bad thing");
        assert!(
            PipelineError::extract("boom")
                .to_string()
                .starts_with("extract failed")
        );
    }
}
