//! The lexical-search kernel seam (M3-T9, ADR 0002: "内核以 `/x-vf/` 查询面
//! 薄 trait 隔离，保留未来换核退路").
//!
//! Everything under `/x-vf/` that needs BM25 goes through
//! [`LexicalKernel`] — ONE trait, today exactly ONE production implementation
//! ([`LanceFts`], Lance's native FTS over the derived table's `text` column).
//! The point is the swap path, not abstraction for its own sake: if Lance's
//! FTS evolves incompatibly (or a future ruling goes back to a sidecar), the
//! `/x-vf` query face keeps its shape and only this module's binding changes.
//! The PROTOCOL face (QueryVectors) never touches this trait — it stays
//! pure-vector by the same ADR ruling.
//!
//! R1 anchor gating (recall-quality roadmap) lives here too: the
//! [`anchor_candidates`]/[`select_anchors`] analyzer splits a query into
//! {锚词 (rare-df constraints) / 语境词} — probed by POSTINGS COUNT
//! ([`LexicalKernel::probe_df`], v2: the v1 top-1-BM25 score was measured
//! to invert the df ordering) — and [`LexicalKernel::search_gated`] turns
//! that split into a Boolean{must, should} query: the MUST side is the
//! SINGLE RAREST anchor (the v2 gate ruling — see [`select_anchors`]),
//! the SHOULD side keeps the whole query's ranking.
//!
//! T17 (narrative-recall design §1 D-8) rebuilt the candidate side on the
//! kernel's OWN analyzer: [`LexicalKernel::analyze`] is the SAME pipeline
//! the FTS index was built with (assembled from `lance-tokenizer`, the
//! very crate `lance-index` builds the index analyzer from — token-level
//!差分断言 in the tests below guard the mirror), so candidates are
//! analyzer-accurate for ANY script — CJK included, the R1 blind spot
//! (F-1). Quoted spans stay first and whole (any text, CJK included); the
//! head anchor is the query's argmin-df term under a two-stage probe
//! (cap 64 first, exact counts for cap-saturated head ties — D-8.4), and
//! the 恒门 ruling applies: any query with a df≥1 candidate gates, even
//! all-structure-word queries (v2's zero-anchor escape is retired).
//!
//! Deliberately NOT here: index management (`vectorfs optimize` builds/folds
//! the FTS index through `store::Storage::ensure_fts_index` directly — the
//! verb is an engine-side maintenance action, not a query-face concern).
//!
//! The boxed-future signature is what keeps the trait dyn-compatible
//! (`Arc<dyn LexicalKernel>`); native async-fn-in-trait is not yet
//! dyn-compatible. The seam grew from ONE method to FIVE, each addition
//! ADDITIVE with a default implementation (R1 ×3, T17 ×1): a second
//! kernel keeps compiling unchanged and simply rides the defaults (for
//! [`LexicalKernel::analyze`] that default is "no terms", which the
//! candidate extractor degrades on to the v1 shape — the second kernel's
//! behavior stays v1-equivalent, never worse).

use crate::filter::Filter;
use crate::pipeline::config::FtsTokenizer;
use crate::store::{KeywordHit, Storage, StoreError};

/// A lexical (BM25) kernel bound behind the `/x-vf/` query face.
pub trait LexicalKernel: Send + Sync + 'static {
    /// Kernel identity (observability/tests: error context, doctor lines).
    fn name(&self) -> &'static str;

    /// BM25 search over one derived index's chunk-text column.
    ///
    /// `filter` is the SAME `$`-operator filter the semantic routes compile
    /// (QueryVectors grammar); kernels apply it as a pre-/post-filter around
    /// their ranking, mirroring the vector routes' semantics. Results are
    /// (score desc, key asc) with raw kernel scores — see the implementor
    /// docs for score-comparability caveats.
    fn search<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        query: &'a str,
        top_k: usize,
        filter: Option<&'a Filter>,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>>;

    /// Anchor-gated BM25 search (R1): every term of `must` is a CONSTRAINT
    /// (chunks missing any of them are out), while `should` — the original
    /// query string — stays the ranking body. ADDITIVE with a default that
    /// IGNORES the gate and runs the plain [`LexicalKernel::search`]:
    /// kernels without a structured-query face (the scripted test kernel)
    /// keep compiling and keep their behavior; only kernels that override
    /// this (LanceFts) actually gate.
    fn search_gated<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        must: &'a [String],
        should: &'a str,
        top_k: usize,
        filter: Option<&'a Filter>,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
        let _ = must; // the gate is advisory: a kernel without a structured
        // query face answers the UNGATED query (the additive contract)
        self.search(storage, vb, ix, should, top_k, filter)
    }

    /// df probe (R1 v2, adjudicated 2026-11-11): the POSTINGS COUNT of a
    /// single term against one derived index — how many chunks carry the
    /// term, capped at [`PROBE_DF_LIMIT`] (df above the cap reads AS the
    /// cap: the stage-one judgment only needs the band, and the cap keeps
    /// the probe one small FTS query; a cap-saturated TIE at the head
    /// triggers the stage-two exact count — see [`analyze_anchors`]). The
    /// count is the corpus statistic the anchor judgment reads; v1's
    /// top-1-BM25 score was measured to INVERT the df ordering
    /// (tf-saturation and chunk-length effects — ThoughtWorks df 36
    /// outscoring vibe df 1), which no score floor could split. `Ok(0)` =
    /// the term has no postings (absent terms can never gate — the
    /// must-clause would zero the result set); `Err` = probing unavailable
    /// (caller degrades to the ungated behavior). ADDITIVE with a default
    /// that counts [`LexicalKernel::search`] at the cap: any kernel
    /// answers probes for free.
    fn probe_df<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        term: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            Ok(self
                .search(storage, vb, ix, term, PROBE_DF_LIMIT, None)
                .await?
                .len() as u64)
        })
    }

    /// Tokenization-stable terms of `query` under THIS kernel's analyzer
    /// POLICY — the FULL filter chain the index built with, not just
    /// segmentation (T17, D-8.1). The returned terms are what the kernel's
    /// index would tokenize `query` into: whatever survives
    /// segmentation + length/stop/stem/fold filters. The anchor analysis
    /// feeds them to [`probe_df`] — same-pipeline terms are the only ones
    /// whose probe round-trips (v1's ASCII-run heuristic worked for Latin
    /// by accident and never for CJK, F-1). ADDITIVE with a default that
    /// returns NO terms: a kernel without an analyzer face keeps
    /// compiling and [`anchor_candidates`] degrades to the v1 extraction
    /// (the second kernel's behavior stays v1-equivalent, D-8.3).
    fn analyze(&self, tokenizer: FtsTokenizer, query: &str) -> Vec<String> {
        let _ = (tokenizer, query);
        Vec::new()
    }
}

/// The Lance native FTS kernel (ADR 0002 ruling): `Index::FTS` inverted
/// index + `full_text_search` BM25 over the derived table's `text` column,
/// engine-side — flat-scanned when no index exists yet (correct, slower),
/// indexed after `vectorfs optimize` built one. Tokenizer choice lives in
/// the `[ingest.<bucket>]` config and rides the optimize verb, not this
/// query path (queries tokenize with whatever the index was built with —
/// [`LanceFts::analyze`] mirrors that exact pipeline, see [`fts_analyzer`]).
pub struct LanceFts;

impl LexicalKernel for LanceFts {
    fn name(&self) -> &'static str {
        "lance-fts"
    }

    fn search<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        query: &'a str,
        top_k: usize,
        filter: Option<&'a Filter>,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
        Box::pin(async move { storage.keyword_search(vb, ix, query, top_k, filter).await })
    }

    fn search_gated<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        must: &'a [String],
        should: &'a str,
        top_k: usize,
        filter: Option<&'a Filter>,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
        Box::pin(async move {
            storage
                .keyword_search_gated(vb, ix, must, should, top_k, filter)
                .await
        })
    }

    fn analyze(&self, tokenizer: FtsTokenizer, query: &str) -> Vec<String> {
        // the assembly cannot fail for the menu `Storage::ensure_fts_index`
        // spells (ngram 2,2 passes its constructor's validation); the empty
        // return on a hypothetical failure IS the D-8.3 degrade signal
        // (candidates fall back to the v1 extraction)
        match fts_analyzer(tokenizer) {
            Ok(mut analyzer) => {
                let mut stream = analyzer.token_stream(query);
                let mut terms = Vec::new();
                while stream.advance() {
                    terms.push(stream.token().text.clone());
                }
                terms
            }
            Err(e) => {
                eprintln!("lance-fts: analyzer assembly for {tokenizer} failed: {e}");
                Vec::new()
            }
        }
    }
}

/// Assemble the analyzer chain `Storage::ensure_fts_index` builds FTS
/// indexes with (T17, D-8.2) — the SAME six stages, in the SAME order,
/// from the SAME crate (`lance-tokenizer`, what `lance-index`'s
/// `InvertedIndexParams::build` assembles the index analyzer from):
///
/// | stage | icu (default) | ngram |
/// |---|---|---|
/// | 1 segmentation | `IcuTokenizer::default()` (dictionary mode) | `NgramTokenizer(2, 2, prefix_only=false)` |
/// | 2 length | `RemoveLongFilter(40)` | same |
/// | 3 case | `LowerCaser` | same |
/// | 4 stem | `Stemmer(English)` | OFF (`apply_ngram_defaults`) |
/// | 5 stop | `StopWordFilter::all()` (the icu 专项: ALL languages' stop words) | OFF |
/// | 6 fold | `AsciiFoldingFilter` | same |
///
/// The ngram column is lance's own `apply_ngram_defaults` (stem/stop off so
/// every substring stays searchable); the 2,2 bounds and every ON flag are
/// `FtsIndexBuilder`'s defaults, i.e. exactly what the optimize verb writes.
/// The stage ORDER is load-bearing and deliberately mirrors `build()`
/// verbatim — e.g. stemming runs BEFORE stop-word removal (a stemmed
/// stopword usually escapes the list) — which is why the tests assemble the
/// chain INDEPENDENTLY from this table and assert token-level equality
/// (`analyze_matches_the_independently_assembled_index_pipeline`): "same
/// version line" is a dependency fact, "same tokens" is the contract.
fn fts_analyzer(tokenizer: FtsTokenizer) -> Result<lance_tokenizer::TextAnalyzer, String> {
    use lance_tokenizer::{
        AsciiFoldingFilter, IcuTokenizer, Language, LowerCaser, NgramTokenizer, RemoveLongFilter,
        Stemmer, StopWordFilter,
    };
    let ngram = || NgramTokenizer::new(2, 2, false).map_err(|e| e.to_string());
    let builder = match tokenizer {
        FtsTokenizer::Icu => lance_tokenizer::TextAnalyzer::builder(IcuTokenizer::default())
            .dynamic()
            .filter_dynamic(RemoveLongFilter::limit(40))
            .filter_dynamic(LowerCaser)
            .filter_dynamic(Stemmer::new(Language::English))
            .filter_dynamic(StopWordFilter::all())
            .filter_dynamic(AsciiFoldingFilter),
        FtsTokenizer::Ngram => lance_tokenizer::TextAnalyzer::builder(ngram()?)
            .dynamic()
            .filter_dynamic(RemoveLongFilter::limit(40))
            .filter_dynamic(LowerCaser)
            .filter_dynamic(AsciiFoldingFilter),
    };
    Ok(builder.build())
}

// ---------------------------------------------------------------------------
// R1/T17 anchor analysis — query → {锚词 (gate) / 语境词 (rank)}
// ---------------------------------------------------------------------------

/// The stage-one df probe caps at this many hits: one small FTS query per
/// term, count = min(df, cap). The cap decides the stage-one band — any
/// candidate reading the cap MIGHT be the argmin (narrative corpora's
/// 常态: every entity df ≥ 64), which is exactly the tie
/// [`analyze_anchors`]'s stage two resolves. Measured 5-25 ms per probe
/// indexed (2026-11-11, docs corpus, 6 probes in 84-87 ms warm —
/// `r1_probe_calibration`); the cap bounds the probe's row decode.
pub const PROBE_DF_LIMIT: usize = 64;

/// The stage-two df probe caps at this many hits (D-8.4): when ≥2 head
/// candidates tie AT the stage-one cap, EACH tied term gets one
/// directed exact count at this limit (a deeper FTS query, only for the
/// tied set — at most [`MAX_PROBES`] of them). Residual ties beyond this
/// limit stay first-seen ties — declared honest, not hidden (design §5.3:
/// 两阶段后 df > 8192 的残余并列 = 首现序).
pub const EXACT_DF_LIMIT: usize = 8192;

/// At most this many candidates get probed (each probe is one FTS query
/// limited to [`PROBE_DF_LIMIT`] hits — 5-25 ms each measured, see
/// [`PROBE_DF_LIMIT`]); the cap bounds the analysis latency budget. The
/// head-anchor argmin's domain is this probe set (quoted spans first,
/// analyzer terms in first-seen order) — a discriminative entity sitting
/// 9th+ in a long query is never probed: 规范语义, pinned by the
/// narrative corpus's long-query case, not an accident.
pub const MAX_PROBES: usize = 8;

/// Anchor candidates of a query (T17 v3, D-8.3):
///
/// * QUOTED spans first, their whole content one candidate — ANY text,
///   CJK included (`"潮汐灯"` gates as its term bag — 优先整串, never
///   split into words);
/// * then the kernel's [`LexicalKernel::analyze`] TERMS of the whole
///   query — the same-pipeline tokens, so CJK dictionary words, stemmed
///   Latin and ascii-folded accents are candidates exactly as the index
///   sees them (the v1 ASCII-run shape heuristic is RETIRED: under icu
///   word segmentation camelCase/`vol-32` shapes survive as whole tokens
///   by themselves).
///
/// Kept from v1: the junk filter (candidates must be ≥2 chars, carry at
/// least one alphanumeric char — the ngram bigramizer would otherwise
/// nominate punctuation — and not be pure numerals: years/versions are
/// corpus-structure noise) and the case-fold dedup (first spelling wins).
///
/// DEGRADE LAYER (D-8.3): `analyze()` returning EMPTY — a second kernel
/// riding the trait default — falls back to v1's whole extraction below:
/// a kernel without an analyzer face behaves EXACTLY as before the
/// rewrite, never worse.
pub fn anchor_candidates(
    kernel: &dyn LexicalKernel,
    tokenizer: FtsTokenizer,
    query: &str,
) -> Vec<String> {
    let terms = kernel.analyze(tokenizer, query);
    if terms.is_empty() {
        return anchor_candidates_v1(query);
    }
    let mut candidates: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut push = |c: String, candidates: &mut Vec<String>| {
        if c.chars().count() < 2
            || !c.chars().any(|ch| ch.is_alphanumeric())
            || c.chars().all(|ch| ch.is_numeric())
        {
            return;
        }
        if seen.insert(c.to_lowercase()) {
            candidates.push(c);
        }
    };

    // 1) quoted spans (ASCII double quotes): whole content, any script,
    //    priority order
    let mut in_quotes = false;
    let mut quoted = String::new();
    for ch in query.chars() {
        if ch == '"' {
            if in_quotes {
                push(std::mem::take(&mut quoted), &mut candidates);
            }
            in_quotes = !in_quotes;
        } else if in_quotes {
            quoted.push(ch);
        }
    }

    // 2) analyzer terms, first-seen order (the argmin's probe domain)
    for term in terms {
        push(term, &mut candidates);
    }
    candidates.truncate(MAX_PROBES);
    candidates
}

/// The v1 extractor (R1, retired to fallback duty by T17): quoted spans
/// (ASCII-lettered) then Latin identifier RUNS — maximal ASCII
/// alphanumeric spans keeping intra-word `-`/`.`/`_` connectors. Its
/// blind spot (F-1) is the reason v3 exists: pure-CJK runs are never
/// candidates, because an ASCII-shape heuristic cannot know the index's
/// CJK segmentation. Kept verbatim so the degrade layer is v1-EQUIVALENT,
/// not v1-inspired.
fn anchor_candidates_v1(query: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut push = |c: String, candidates: &mut Vec<String>| {
        if c.chars().count() < 2 || !c.chars().any(|ch| ch.is_ascii_alphabetic()) {
            return;
        }
        if seen.insert(c.to_lowercase()) {
            candidates.push(c);
        }
    };

    // 1) quoted spans (ASCII double quotes): whole content, priority order
    let mut remainder = String::with_capacity(query.len());
    let mut in_quotes = false;
    let mut quoted = String::new();
    for ch in query.chars() {
        if ch == '"' {
            if in_quotes {
                push(std::mem::take(&mut quoted), &mut candidates);
            }
            in_quotes = !in_quotes;
        } else if in_quotes {
            quoted.push(ch);
        } else {
            remainder.push(ch);
        }
    }

    // 2) identifier runs over the remainder: ASCII alphanumeric spans with
    //    intra-word `-._` connectors. A connector enters the run only when
    //    flanked by alphanumerics on BOTH sides (prev: the run is non-empty
    //    and cannot end in a connector; next: checked directly) — so runs
    //    never start or end with a connector (`-vibe` drops the prefix,
    //    `vibe-` flushes as `vibe`).
    let mut run = String::new();
    let mut flush = |run: &mut String, candidates: &mut Vec<String>| {
        if !run.is_empty() {
            push(std::mem::take(run), candidates);
        }
        run.clear();
    };
    let chars: Vec<char> = remainder.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        let continues_run = ch.is_ascii_alphanumeric()
            || (matches!(ch, '-' | '.' | '_')
                && !run.is_empty()
                && chars
                    .get(i + 1)
                    .is_some_and(|next| next.is_ascii_alphanumeric()));
        if continues_run {
            run.push(ch);
        } else {
            flush(&mut run, &mut candidates);
        }
    }
    flush(&mut run, &mut candidates);
    candidates.truncate(MAX_PROBES);
    candidates
}

/// Anchor selection over probed candidates (T17 v3, D-8.4): every
/// df≥1 candidate, df ASCENDING, ties first-seen — the HEAD is the MUST
/// term (the argmin-df gate). Zero-count terms are dropped (an absent
/// term's must-clause would zero the result set — df=0 never gates, it
/// rides `should`).
///
/// THE GATE RULING (v2, measured 2026-11-11; unchanged by v3): the
/// Boolean MUST clause is the verdict's HEAD alone. The
/// AND-of-all-anchors shape was measured to evict correct-answer chunks
/// whose fellow rare terms STRADDLE chunk boundaries: on the f1
/// coined-term fixture `zxqwibble`/`migration`/`gate` are EACH df=1 in
/// three DIFFERENT 32-token windows (AND = zero hits, the needle loses
/// its lexical rank); the single-rarest-anchor must is the shape the
/// acceptance counterfactual validated (must=[vibe] returns EXACTLY the
/// correct chunk) and it cannot evict a chunk that contains the corpus's
/// rarest query term by construction.
///
/// HONEST HISTORY — the retired v2 ceiling (calibration table MEASURED
/// 2026-11-11 via [`LexicalKernel::probe_df`], re-runnable as
/// `r1_probe_calibration`; docs radar corpus, 1,570 text chunks, icu):
///
/// | term | df | verdict at ceiling 16 |
/// |---|---|---|
/// | `vibe` | 1 | ANCHOR — the rarest term in the corpus |
/// | `自满` | 12 | (CJK — not a v1 candidate; the rare band's edge) |
/// | `coding` | 35 | not |
/// | `ThoughtWorks` | 36 | not — the Latin structure word (every footer) |
/// | `雷达` | 109 | not — structure word |
/// | `技术` | 336 | not — ubiquitous |
///
/// v3 (D-8.4) RETIRED the ceiling — both roles. As a gate eligibility
/// line it was corpus-scale-blind (a count, not a fraction: the same 16
/// that splits a 1.5k-chunk docs corpus calls EVERY entity rare on a
/// 180-chapter novel, and rare-nothing on a 100k-chunk archive); as a
/// display band it made the `anchors` member sit EMPTY on exactly the
/// narrative corpora the mechanism exists for (A-R7's contradiction). The
/// argmin-df head keeps the ceiling's real function — the rarest term
/// gates — without inventing a threshold; the behavior change is
/// deliberate and pinned: an all-structure-word query now gates too
/// (恒门 — v2's "all-structure ⇒ zero anchors ⇒ ungated" escape hatch is
/// retired; the gate costs lexical ENHANCEMENT precision, never a
/// semantic result).
///
/// Pure function on purpose: the unit-test oracle feeds calibrated probe
/// counts (the measured table above) without needing a live index.
pub fn select_anchors(counted: &[(String, u64)]) -> Vec<String> {
    let mut hits: Vec<(u64, usize, &String)> = counted
        .iter()
        .enumerate()
        .filter_map(|(i, (term, df))| (*df >= 1).then_some((*df, i, term)))
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, _, term)| term.clone()).collect()
}

/// The full query-time analysis (T17 v3, D-8.4 two-stage): candidates →
/// df probes → anchors, through the kernel's probe/search faces.
///
/// * STAGE ONE: every candidate probed at [`PROBE_DF_LIMIT`] (the v2
///   behavior; the band is enough when the head is under the cap);
/// * STAGE TWO: when ≥2 head candidates tie AT the cap (the narrative
///   常态 — every entity df ≥ 64, so the argmin is invisible to stage
///   one), each tied term gets ONE directed exact count at
///   [`EXACT_DF_LIMIT`]. Stage two can only re-rank the tied set among
///   itself (an exact count of a cap-saturated term stays ≥ cap), so the
///   stage-one order below the cap is untouched.
///
/// `Err` propagates (a missing FTS face / pre-T9 table — the caller
/// degrades to the ungated behavior and notes it); `Ok(vec![])` =
/// analyzed, no df≥1 candidate — every candidate is corpus-absent, the
/// ungated behavior IS the answer (the 恒门 retirement means
/// all-structure-word queries reach a NON-empty verdict whenever the
/// structure words exist in the corpus, which they do by definition).
/// Probe latency budget: ≤ [`MAX_PROBES`] stage-one queries (5-25 ms
/// each measured) + ≤ tied-set-size stage-two queries at the deeper
/// limit, run only on the saturation-tie path.
pub async fn analyze_anchors(
    kernel: &dyn LexicalKernel,
    storage: &Storage,
    vb: &str,
    ix: &str,
    tokenizer: FtsTokenizer,
    query: &str,
) -> Result<Vec<String>, StoreError> {
    let candidates = anchor_candidates(kernel, tokenizer, query);
    let mut counted: Vec<(String, u64)> = Vec::with_capacity(candidates.len());
    for term in &candidates {
        counted.push((term.clone(), kernel.probe_df(storage, vb, ix, term).await?));
    }
    // stage two (D-8.4): the head's argmin is only UNDECIDABLE when the
    // minimum stage-one count IS the cap (every sub-cap count is exact);
    // then every cap-saturated candidate (they are all tied at the head)
    // gets one directed exact count
    let head_tied_at_cap = counted
        .iter()
        .filter(|(_, df)| *df >= 1)
        .map(|(_, df)| *df)
        .min()
        == Some(PROBE_DF_LIMIT as u64);
    if head_tied_at_cap {
        for (term, df) in counted.iter_mut() {
            if *df == PROBE_DF_LIMIT as u64 {
                *df = kernel
                    .search(storage, vb, ix, term, EXACT_DF_LIMIT, None)
                    .await?
                    .len() as u64;
            }
        }
    }
    Ok(select_anchors(&counted))
}

/// The kernel binding used by the `/x-vf` query face: one place to swap the
/// implementation (a future ruling changes exactly this function). The
/// kernel is stateless — it borrows the caller's storage — so there is
/// nothing to configure or inject at startup.
pub fn kernel() -> std::sync::Arc<dyn LexicalKernel> {
    std::sync::Arc::new(LanceFts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam itself: a scripted second kernel proves the trait is all the
    /// query face needs (the swap path is real, not decorative) — it answers
    /// from a fixed list without touching the engine. It does NOT override
    /// the R1 additions, so it also pins the additive contract: a second
    /// kernel compiles unchanged and rides the defaults.
    struct Scripted(Vec<KeywordHit>);

    impl LexicalKernel for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn search<'a>(
            &'a self,
            _storage: &'a Storage,
            _vb: &'a str,
            _ix: &'a str,
            _query: &'a str,
            _top_k: usize,
            _filter: Option<&'a Filter>,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
            Box::pin(async { Ok(self.0.clone()) })
        }
    }

    #[tokio::test]
    async fn the_seam_is_dyn_dispatchable_with_two_impls() {
        let k: std::sync::Arc<dyn LexicalKernel> = kernel();
        assert_eq!(k.name(), "lance-fts");
        // a second impl rides the same Arc<dyn> face (no engine involved)
        let want = crate::store::KeywordHit {
            key: "k".into(),
            score: 1.5,
            metadata: None,
        };
        let scripted: std::sync::Arc<dyn LexicalKernel> = std::sync::Arc::new(Scripted(vec![want]));
        assert_eq!(scripted.name(), "scripted");
        // dispatch through the trait object against a memory storage that is
        // never opened (the scripted kernel ignores it)
        let storage = crate::store::Storage::memory();
        let got = scripted
            .search(&storage, "vb", "ix", "q", 10, None)
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "k");
        // R1 additive defaults: search_gated IGNORES the gate (answers the
        // plain query), probe_df counts the search answer at PROBE_DF_LIMIT
        // — a kernel without a structured face degrades, never breaks
        let gated = scripted
            .search_gated(&storage, "vb", "ix", &["never".into()], "q", 10, None)
            .await
            .unwrap();
        assert_eq!(gated.len(), 1, "default ignores `must` — same answer");
        let probe = scripted.probe_df(&storage, "vb", "ix", "q").await.unwrap();
        assert_eq!(probe, 1, "default counts the plain search's hits");
        // T17 additive default: analyze answers NO terms — the D-8.3
        // degrade signal; the candidate extractor falls back to the v1
        // extraction (quoted + Latin runs, CJK stays out), so the second
        // kernel's candidate behavior is v1-EQUIVALENT, never worse
        assert!(
            scripted
                .analyze(FtsTokenizer::Icu, "vibe coding")
                .is_empty(),
            "default analyze = no terms"
        );
        assert_eq!(
            anchor_candidates(
                scripted.as_ref(),
                FtsTokenizer::Icu,
                "find \"vibe coding\" mentions 技术"
            ),
            vec![
                "vibe coding".to_string(),
                "find".to_string(),
                "mentions".to_string()
            ],
            "a default-analyze kernel rides the v1 extraction verbatim"
        );
    }

    // ---- T17 anchor analysis: candidate extraction (analyzer terms) ----

    #[test]
    fn candidates_are_analyzer_terms_with_quoted_spans_first() {
        // the origin query through the REAL analyzer chain (icu): Latin
        // lowercases AND STEMS (ThoughtWorks→thoughtwork, coding→code —
        // the index's own tokens, what a probe must carry), CJK
        // dictionary-segments into words (技术/雷达/变化 — the F-1 blind
        // spot fixed), the single char 中 drops at the candidate filter
        assert_eq!(
            anchor_candidates(
                &LanceFts,
                FtsTokenizer::Icu,
                "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化"
            ),
            vec!["thoughtwork", "技术", "雷达", "vibe", "code", "变化"]
        );
        // identifier shapes now come from the analyzer, not a heuristic:
        // icu SPLITS vf-search at the hyphen, KEEPS config.toml whole
        // (dot inside one segment), stems uses→use, and the trailing
        // underscore survives (flags_) — shapes the v1 ASCII-run
        // heuristic approximated, the pipeline states
        assert_eq!(
            anchor_candidates(
                &LanceFts,
                FtsTokenizer::Icu,
                "vf-search uses config.toml and -v flags_"
            ),
            vec!["vf", "search", "use", "config.toml", "flags_"]
        );
        // quoted content is one candidate, priority order — the whole span
        // precedes every analyzer term, and its constituent terms still
        // follow (the phrase AND its words are all probeable)
        assert_eq!(
            anchor_candidates(
                &LanceFts,
                FtsTokenizer::Icu,
                "find \"vibe coding\" mentions"
            ),
            vec!["vibe coding", "find", "vibe", "code", "mention"]
        );
        // duplicates fold case-insensitively (analyzer output is already
        // lowercased — the fold still matters for quoted spans); pure
        // numerals and single chars never qualify (the kept v1 filters)
        assert_eq!(
            anchor_candidates(&LanceFts, FtsTokenizer::Icu, "Vibe vibe coding 2026 32 a I"),
            vec!["vibe", "code"]
        );
        // pure CJK: candidates EXIST — v1's structural empty set (F-1,
        // pinned as the v1 record in tests/narrative_corpus.rs before
        // this rewrite) is exactly what T17 removes
        assert_eq!(
            anchor_candidates(&LanceFts, FtsTokenizer::Icu, "技术雷达有哪些变化"),
            vec!["技术", "雷达", "变化"]
        );
        // accents fold (AsciiFoldingFilter): café/résumé/naïve/straße
        // probe as cafe/resume/naiv/strasse — what the index tokenized
        assert_eq!(
            anchor_candidates(&LanceFts, FtsTokenizer::Icu, "café résumé naïve straße"),
            vec!["cafe", "resume", "naiv", "strasse"]
        );
    }

    #[test]
    fn candidates_are_capped_at_max_probes_in_priority_order() {
        let query = "\"quoted one\" alpha beta gamma delta epsilon zeta eta theta";
        let got = anchor_candidates(&LanceFts, FtsTokenizer::Icu, query);
        assert_eq!(got.len(), MAX_PROBES);
        assert_eq!(got[0], "quoted one", "quoted spans keep probe priority");
        // analyzer terms fill in first-seen order (quoted→quot, one is a
        // kept pronoun in lance's stop list); zeta/eta/theta sit past
        // MAX_PROBES and are never probed (规范语义 — D-8.4)
        assert_eq!(
            got[1..],
            ["quot", "one", "alpha", "beta", "gamma", "delta", "epsilon"]
        );
    }

    // ---- T17 anchor analysis: selection over probe counts (argmin-df) ----

    #[test]
    fn selection_orders_the_calibrated_oracle_rarest_first() {
        // THE R1 oracle — the MEASURED docs-corpus df table (see
        // select_anchors' history table, re-runnable as
        // r1_probe_calibration): the origin query's candidates count
        // vibe=1 / coding=35 / ThoughtWorks=36 — v3 keeps EVERY df≥1 term
        // (the ceiling is retired), rarest first; the head (vibe) is the
        // counterfactual-validated MUST term, exactly as under v2
        let counted = vec![
            ("thoughtwork".to_string(), 36),
            ("vibe".to_string(), 1),
            ("code".to_string(), 35),
        ];
        assert_eq!(
            select_anchors(&counted),
            vec!["vibe", "code", "thoughtwork"],
            "the measured origin-query verdict: every df≥1 candidate, rarest first"
        );
        // count ascending (rarest = head = gate), ties first-seen — the
        // NORMATIVE residual-tie behavior (two-stage counts that still
        // tie resolve here, by design not by degrade)
        let rare = vec![
            ("zxqwibbl".to_string(), 1),
            ("gate".to_string(), 1),
            ("migrat".to_string(), 3),
        ];
        assert_eq!(
            select_anchors(&rare),
            vec!["zxqwibbl", "gate", "migrat"],
            "rarest first; ties keep first-seen order"
        );
        // reordering must not move the ORDERING semantics (count sort
        // wins over first-seen; ties stay in query order)
        let reordered = vec![
            ("migrat".to_string(), 3),
            ("gate".to_string(), 1),
            ("zxqwibbl".to_string(), 1),
        ];
        assert_eq!(
            select_anchors(&reordered),
            vec!["gate", "zxqwibbl", "migrat"]
        );
        // THE 恒门 PIN (D-8.4): the measured structure words 雷达 109 /
        // 技术 336 / ThoughtWorks 36 / coding 35 are ALL in the verdict
        // now — df ASC, no ceiling, no display band; a query built from
        // them gates on its own argmin instead of escaping ungated
        let structure = vec![
            ("技术".to_string(), 336),
            ("雷达".to_string(), 109),
            ("thoughtwork".to_string(), 36),
            ("code".to_string(), 35),
        ];
        assert_eq!(
            select_anchors(&structure),
            vec!["code", "thoughtwork", "雷达", "技术"],
            "structure words anchor too — the df absolute value is no filter"
        );
    }

    #[test]
    fn selection_drops_absent_terms_and_keeps_every_present_one() {
        // absent (0-count) terms can never gate — a must-clause over a
        // term with zero postings would return NOTHING (df=0 rides should)
        let counted = vec![("zxqwibbl".to_string(), 0), ("vibe".to_string(), 1)];
        assert_eq!(select_anchors(&counted), vec!["vibe"]);
        // a query whose every token is rare: the verdict lists them ALL —
        // the MAX_ANCHORS observability cap retired with the ceiling (the
        // member's v3 semantics is "the probed candidates, df-ascending",
        // D-8.7); the GATE is still the head alone
        let all_rare: Vec<(String, u64)> = ["aa1", "bb2", "cc3", "dd4", "ee5", "ff6"]
            .iter()
            .enumerate()
            .map(|(i, t)| (t.to_string(), (i + 1) as u64))
            .collect();
        let got = select_anchors(&all_rare);
        assert_eq!(got.len(), 6, "no observability cap: every df≥1 candidate");
        assert_eq!(
            got[0], "aa1",
            "rarest (count 1) is the head = the gate term"
        );
    }

    /// A probe-mock kernel: fixed term→df answers (0 = corpus-absent),
    /// Err on demand — [`analyze_anchors`] driven with the MEASURED
    /// docs-corpus counts (the `r1_probe_calibration` table) without a
    /// live index. The ANALYZER delegates to the real LanceFts chain
    /// (tokenization is real; only the postings answers are scripted),
    /// and `search` answers `df` placeholder hits capped at top_k —
    /// exactly the count semantics both probe stages read (stage one
    /// min(df, PROBE_DF_LIMIT), stage two min(df, EXACT_DF_LIMIT)), so
    /// the two-stage escalation is drivable from df tables alone.
    struct Probed {
        dfs: Vec<(String, u64)>,
        fail_on: Option<&'static str>,
    }

    impl Probed {
        fn df_of(&self, term: &str) -> u64 {
            self.dfs
                .iter()
                .find(|(t, _)| t == term)
                .map(|(_, d)| *d)
                .unwrap_or(0)
        }
    }

    impl LexicalKernel for Probed {
        fn name(&self) -> &'static str {
            "probed"
        }
        fn analyze(&self, tokenizer: FtsTokenizer, query: &str) -> Vec<String> {
            LanceFts.analyze(tokenizer, query)
        }
        fn search<'a>(
            &'a self,
            _storage: &'a Storage,
            _vb: &'a str,
            _ix: &'a str,
            query: &'a str,
            top_k: usize,
            _filter: Option<&'a Filter>,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
            Box::pin(async move {
                if Some(query) == self.fail_on {
                    return Err(StoreError::Internal(format!("probe of {query:?} failed")));
                }
                Ok((0..self.df_of(query).min(top_k as u64))
                    .map(|_| KeywordHit {
                        key: "k".into(),
                        score: 1.0,
                        metadata: None,
                    })
                    .collect())
            })
        }
    }

    #[tokio::test]
    async fn analysis_through_a_mock_probe_reproduces_the_measured_verdict() {
        let storage = crate::store::Storage::memory();
        // the docs-corpus measurement (select_anchors' history table):
        // the origin query's candidates count vibe=1 / coding=35 /
        // ThoughtWorks=36 — the v3 verdict keeps all three, rarest first;
        // the head (vibe) is the counterfactual-validated MUST term; the
        // corpus-absent 自满 never reaches the verdict
        let kernel = Probed {
            dfs: vec![
                ("thoughtwork".into(), 36),
                ("vibe".into(), 1),
                ("code".into(), 35),
                ("自满".into(), 0),
            ],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            FtsTokenizer::Icu,
            "ThoughtWorks 技术雷达中 vibe coding 技术有哪些变化",
        )
        .await
        .unwrap();
        assert_eq!(
            got,
            vec!["vibe", "code", "thoughtwork"],
            "measured verdict: every df≥1 candidate, rarest first"
        );

        // the f1 fixture's measured shape (all three df=1 in different
        // chunks), candidates in the analyzer's STEMMED spellings
        // (zxqwibble→zxqwibbl, migration→migrat — the index's own
        // tokens): the gate takes the HEAD (zxqwibbl, first-seen among
        // ties), which is what keeps the coined-term needle in its
        // lexical arm (see select_anchors' gate ruling)
        let kernel = Probed {
            dfs: vec![
                ("zxqwibbl".into(), 1),
                ("migrat".into(), 1),
                ("gate".into(), 1),
            ],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            FtsTokenizer::Icu,
            "the zxqwibble migration gate",
        )
        .await
        .unwrap();
        assert_eq!(got, vec!["zxqwibbl", "migrat", "gate"]);

        // THE 恒门 PIN through the full driver: a structure-word query
        // GATES (radar 109 / tech 336 anchor, head = the argmin) — v2's
        // over-ceiling empty verdict is retired (D-8.4)
        let kernel = Probed {
            dfs: vec![("radar".into(), 109), ("tech".into(), 336)],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            FtsTokenizer::Icu,
            "radar tech",
        )
        .await
        .unwrap();
        assert_eq!(
            got,
            vec!["radar", "tech"],
            "structure words gate on their argmin: {got:?}"
        );

        // probe FAILURE propagates as Err — the caller (ops_xvf) degrades
        // to the ungated behavior on this exact signal
        let kernel = Probed {
            dfs: vec![("vibe".into(), 1)],
            fail_on: Some("vibe"),
        };
        assert!(matches!(
            analyze_anchors(
                &kernel,
                &storage,
                "vb",
                "ix",
                FtsTokenizer::Icu,
                "vibe coding"
            )
            .await,
            Err(StoreError::Internal(_))
        ));
    }

    /// D-8.4 stage two: the quoted spans pin the candidate ORDER exactly
    /// (their dictionary-segmented constituents are single chars and drop
    /// at the candidate filter), so a three-quoted-term query drives the
    /// escalation with a known probe set — the narrative-corpus tie shape
    /// in miniature.
    #[tokio::test]
    async fn analysis_resolves_head_saturation_ties_with_stage_two_exact_counts() {
        let storage = crate::store::Storage::memory();
        // stage one is BLIND here: 72/100/300 all read as the cap 64 (a
        // ≥2-way tie AT the head) — stage two's exact counts alone decide
        // the argmin (乙词 72), reordering the verdict against the
        // candidate order
        let kernel = Probed {
            dfs: vec![
                ("甲词".into(), 100),
                ("乙词".into(), 72),
                ("丙词".into(), 300),
            ],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            FtsTokenizer::Icu,
            "\"甲词\"到\"乙词\"经过\"丙词\"",
        )
        .await
        .unwrap();
        assert_eq!(
            got,
            vec!["乙词", "甲词", "丙词"],
            "stage-two exact counts reorder the saturated tie: 72 < 100 < 300"
        );

        // residual tie (both exact counts equal, under EXACT_DF_LIMIT):
        // first-seen wins — the NORMATIVE resolution, not a degrade
        let kernel = Probed {
            dfs: vec![("丁词".into(), 5000), ("戊词".into(), 5000)],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            FtsTokenizer::Icu,
            "\"丁词\"与\"戊词\"",
        )
        .await
        .unwrap();
        assert_eq!(
            got,
            vec!["丁词", "戊词"],
            "a tie stage two cannot split keeps first-seen order"
        );

        // a sub-cap minimum is already EXACT — the escalation must not
        // fire (only the tied-at-cap set pays the deeper probes): 己词 3
        // decides the head at stage one, 庚词 stays capped at 64 in the
        // verdict's tail
        let kernel = Probed {
            dfs: vec![("己词".into(), 3), ("庚词".into(), 200)],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            FtsTokenizer::Icu,
            "\"己词\"与\"庚词\"",
        )
        .await
        .unwrap();
        assert_eq!(
            got,
            vec!["己词", "庚词"],
            "sub-cap head: no escalation, the capped term rides the tail"
        );
    }

    /// D-8.5①: `LanceFts::analyze` must emit EXACTLY what the index-side
    /// analyzer emits — proven by assembling the six-stage chain HERE,
    /// independently from the design's stage table (F-11), and comparing
    /// token by token. The mirror is only as strong as it is independent:
    /// this test re-derives the assembly (ORDER included — stem BEFORE
    /// stop, fold LAST, RemoveLongFilter first among the filters) rather
    /// than calling the implementation's builder, so a future edit that
    /// reorders, adds or omits a stage breaks HERE before it silently
    /// breaks a probe. lance upgrading its chain moves both sides — the
    /// token-level差分 still holds, and the round-trip test below is the
    /// behavioral backstop.
    #[test]
    fn analyze_matches_the_independently_assembled_index_pipeline() {
        use lance_tokenizer::{
            AsciiFoldingFilter, IcuTokenizer, Language, LowerCaser, NgramTokenizer,
            RemoveLongFilter, Stemmer, StopWordFilter, TextAnalyzer,
        };
        fn reference(tokenizer: FtsTokenizer, s: &str) -> Vec<String> {
            // F-11 verbatim: icu = the full six stages; ngram = lance's
            // apply_ngram_defaults (stem/stop OFF — every substring stays
            // searchable), both ending in the ascii fold
            let builder = match tokenizer {
                FtsTokenizer::Icu => TextAnalyzer::builder(IcuTokenizer::default())
                    .dynamic()
                    .filter_dynamic(RemoveLongFilter::limit(40))
                    .filter_dynamic(LowerCaser)
                    .filter_dynamic(Stemmer::new(Language::English))
                    .filter_dynamic(StopWordFilter::all()),
                FtsTokenizer::Ngram => TextAnalyzer::builder(
                    NgramTokenizer::new(2, 2, false).expect("2,2 is a valid ngram"),
                )
                .dynamic()
                .filter_dynamic(RemoveLongFilter::limit(40))
                .filter_dynamic(LowerCaser),
            };
            let mut analyzer = builder.filter_dynamic(AsciiFoldingFilter).build();
            let mut stream = analyzer.token_stream(s);
            let mut out = Vec::new();
            while stream.advance() {
                out.push(stream.token().text.clone());
            }
            out
        }
        // the battery: the origin query, narrative fixture shapes (coined
        // CJK names segment per-char under the dictionary; quoted spans
        // are candidate-shape, not analyzer-shape), accent folding, a
        // stopword-heavy Latin run (stem-before-stop is observable: a
        // stemmed stopword escapes the list), a >40-byte token
        // (RemoveLongFilter), and mixed punctuation
        let battery = [
            "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化",
            "沈观澜最早点亮\"潮汐灯\"是在哪一章",
            "从\"百步桥\"到\"乌篷巷\",要经过\"栖霞圩\"吗",
            "café résumé naïve straße ÄÖÜ",
            "the zxqwibble migration gate and running was",
            "技术雷达有哪些变化",
            "夜读手记·其一",
            "short词 next to averyveryveryveryveryveryveryverylongtoken",
            "VF_TOKENIZER_14 mirror check 42",
            "北京是一座大城市 the city",
        ];
        for s in battery {
            assert_eq!(
                LanceFts.analyze(FtsTokenizer::Icu, s),
                reference(FtsTokenizer::Icu, s),
                "icu token-level差分: {s:?}"
            );
            assert_eq!(
                LanceFts.analyze(FtsTokenizer::Ngram, s),
                reference(FtsTokenizer::Ngram, s),
                "ngram token-level差分: {s:?}"
            );
        }
    }

    /// D-8.5②: every analyze() term of a query whose text is IN the
    /// corpus must be retrievable against a REAL `ensure_fts_index`-built
    /// table (the optimize verb's engine call — the INDEXED path, not the
    /// flat scan), for BOTH tokenizer档. This is the behavioral half of
    /// the差分: if the query-side chain drifted from the index-side chain
    /// (a missing fold, a reordered stem), some term would tokenize
    /// differently on one side and its probe would miss.
    ///
    /// HONEST BOUNDARY: retrievability runs the term through the index's
    /// QUERY-side analyzer once more, and a fold-LAST chain is not
    /// re-analysis-idempotent for every accented stem — `résumé`/`straße`
    /// index to `resume`/`strasse` (the accent blocks the stemmer, the
    /// fold then lands) but probing the FOLDED form re-stems it
    /// (`resum`/`strass`) and misses. That is lance's own pipeline
    /// property (both sides share it), not a mirror drift — the差分
    /// battery above covers accented input for mirror-fidelity; this
    /// battery uses re-analysis-stable terms. In the mechanism such a
    /// candidate probes df=0 and rides `should` (never gates), the
    /// honest degrade.
    #[tokio::test]
    async fn analyze_terms_round_trip_through_built_fts_indexes() {
        use crate::store::{FtsIndexOutcome, IndexMeta, MetaMirror, VectorRow};
        // the corpus carries the query strings themselves (each query is
        // one chunk's text), so every analyzed term has a postings entry
        // by construction — a miss means the two sides tokenized it
        // differently, which is exactly the failure this test exists to
        // catch
        let texts = [
            "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化",
            "沈观澜最早点亮潮汐灯是在哪一章",
            "北京是一座大城市 the city",
            "the zxqwibble migration gate",
            "向量数据库与全文检索的混合引擎 vectorfs",
        ];
        async fn indexed_corpus(name: &str, tokenizer: FtsTokenizer, texts: &[&str]) -> Storage {
            let dir = tempfile::tempdir().unwrap();
            let storage = Storage::file(dir.path());
            // leak the tempdir (store.rs tests' discipline: datasets
            // outlive the test body, the OS reclaims on exit)
            std::mem::forget(dir);
            storage
                .create_bucket(&crate::store::test_bucket_meta(&storage, "vb"))
                .await
                .unwrap();
            storage
                .create_index(&IndexMeta {
                    vector_bucket_name: "vb".into(),
                    index_name: name.into(),
                    index_arn: format!("arn:aws:s3vectors:us-east-1:0:bucket/vb/index/{name}"),
                    creation_time_rfc3339: "2026-11-12T00:00:00.000Z".into(),
                    creation_time_epoch: 0.0,
                    data_type: "float32".into(),
                    dimension: 4,
                    distance_metric: "cosine".into(),
                    non_filterable_metadata_keys: None,
                    index_mode: None,
                    x_vf_derived: None,
                })
                .await
                .unwrap();
            let dim = 4;
            let rows: Vec<VectorRow> = texts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let meta = serde_json::json!({
                        "x-vf-chunk-text": t,
                        "x-vf-src-name": format!("docs/{i}.txt"),
                    });
                    let mut filterable = std::collections::BTreeMap::new();
                    filterable.insert(
                        "x-vf-src-name".into(),
                        MetaMirror::Str(format!("docs/{i}.txt")),
                    );
                    VectorRow {
                        key: format!("k{i:03}"),
                        vector: (0..dim).map(|k| (i + k) as f32 / 64.0).collect(),
                        metadata_json: Some(meta.to_string()),
                        filterable,
                        text: Some(t.to_string()),
                    }
                })
                .collect();
            storage
                .upsert_vectors("vb", name, dim as i32, rows)
                .await
                .unwrap();
            let outcome = storage
                .ensure_fts_index("vb", name, tokenizer.to_string().as_str())
                .await
                .unwrap();
            assert!(
                matches!(outcome, FtsIndexOutcome::Built { .. }),
                "the indexed path: the FTS index exists before any probe ({outcome:?})"
            );
            storage
        }
        for tokenizer in [FtsTokenizer::Icu, FtsTokenizer::Ngram] {
            let storage = indexed_corpus("ixrt", tokenizer, &texts).await;
            for q in texts {
                let terms = LanceFts.analyze(tokenizer, q);
                assert!(
                    !terms.is_empty(),
                    "{tokenizer}: {q:?} must analyze to terms"
                );
                for term in &terms {
                    let hits = storage
                        .keyword_search("vb", "ixrt", term, 10, None)
                        .await
                        .unwrap();
                    assert!(
                        !hits.is_empty(),
                        "{tokenizer} round-trip: term {term:?} of {q:?} must hit the \
                         ensure_fts_index-built table (the two analyzer sides drifted)"
                    );
                }
            }
        }
    }
}
