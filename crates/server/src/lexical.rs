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
//! the SHOULD side keeps the whole query's ranking. The analyzer
//! deliberately works only on tokenization-STABLE shapes (Latin
//! identifier runs, quoted strings); pure-CJK queries analyze to zero
//! anchors and ride the ungated behavior (rebuilding the index analyzer's
//! exact CJK segmentation at query time is a later concern).
//!
//! Deliberately NOT here: index management (`vectorfs optimize` builds/folds
//! the FTS index through `store::Storage::ensure_fts_index` directly — the
//! verb is an engine-side maintenance action, not a query-face concern).
//!
//! The boxed-future signature is what keeps the trait dyn-compatible
//! (`Arc<dyn LexicalKernel>`); native async-fn-in-trait is not yet
//! dyn-compatible. The seam grew from ONE method to THREE, each addition
//! ADDITIVE with a default implementation (R1): a second kernel keeps
//! compiling unchanged and simply rides the defaults.

use crate::filter::Filter;
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
    /// cap: the anchor judgment only needs "≤ ceiling", and the cap keeps
    /// the probe one small FTS query). The count is the corpus statistic
    /// the anchor judgment reads; v1's top-1-BM25 score was measured to
    /// INVERT the df ordering (tf-saturation and chunk-length effects —
    /// ThoughtWorks df 36 outscoring vibe df 1), which no score floor
    /// could split. `Ok(0)` = the term has no postings (absent terms can
    /// never gate — the must-clause would zero the result set); `Err` =
    /// probing unavailable (caller degrades to the ungated behavior).
    /// ADDITIVE with a default that counts [`LexicalKernel::search`] at
    /// the cap: any kernel answers probes for free.
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
}

/// The Lance native FTS kernel (ADR 0002 ruling): `Index::FTS` inverted
/// index + `full_text_search` BM25 over the derived table's `text` column,
/// engine-side — flat-scanned when no index exists yet (correct, slower),
/// indexed after `vectorfs optimize` built one. Tokenizer choice lives in
/// the `[ingest.<bucket>]` config and rides the optimize verb, not this
/// query path (queries tokenize with whatever the index was built with).
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
}

// ---------------------------------------------------------------------------
// R1 anchor analysis — query → {锚词 (gate) / 语境词 (rank)}
// ---------------------------------------------------------------------------

/// A candidate becomes an anchor when its postings count (df, capped at
/// [`PROBE_DF_LIMIT`]) is at or under this ceiling — rare ⇒
/// discriminative.
///
/// CALIBRATION ORACLE — MEASURED 2026-11-11 via the count probe
/// ([`LexicalKernel::probe_df`]; re-runnable as `r1_probe_calibration` in
/// tests/flagship_recall.rs), docs radar corpus (5 期雷达 + 教科书, 1,570
/// text chunks, token-window 512/64, icu):
///
/// | term | df | verdict at ceiling 16 |
/// |---|---|---|
/// | `vibe` | 1 | ANCHOR — the rarest term in the corpus |
/// | `自满` | 12 | (CJK — never a v1 candidate; the rare band's edge) |
/// | `coding` | 35 | not |
/// | `ThoughtWorks` | 36 | not — the Latin structure word (every footer) |
/// | `雷达` | 109 | not — structure word |
/// | `技术` | 336 | not — ubiquitous |
///
/// The ceiling sits in the measured empty band [12, 35): comfortably
/// above every rare signal term (vibe 1, 自满 12), comfortably below the
/// corpus's Latin structure floor (ThoughtWorks 36, coding 35) with ≥2×
/// margin. The v1 SCORE probe put `ThoughtWorks` (6.81) ABOVE `vibe`
/// (5.44) — tf-saturation and chunk-length effects invert the df order —
/// which no score floor could split; the COUNT is the df order by
/// construction. Threshold follow-ups (per-corpus ceiling, ceiling as a
/// corpus-size fraction) are R6 golden-set work
/// (research/2026-11-recall-quality-optimization.md R1/R6).
pub const ANCHOR_DF_CEILING: u64 = 16;

/// The df probe caps at this many hits: one small FTS query per term,
/// count = min(df, cap). The cap only needs to clear
/// [`ANCHOR_DF_CEILING`] ("≤ ceiling" is decidable at the cap); it bounds
/// the probe's row decode.
pub const PROBE_DF_LIMIT: usize = 64;

/// At most this many anchors enter the verdict list (R1: 防查询全锚 — a
/// query whose every token is rare reports the RAREST few; the rest ride
/// `should`). Since the v2 ruling the MUST clause is the verdict's HEAD
/// (the single rarest anchor) — see [`select_anchors`] — so this cap is
/// observability hygiene for the `anchors` response member, not a gate
/// width.
pub const MAX_ANCHORS: usize = 4;

/// At most this many candidates get probed (each probe is one FTS query
/// limited to [`PROBE_DF_LIMIT`] hits — measured 5-25 ms each indexed
/// (2026-11-11, docs corpus, 6 probes in 84-87 ms warm — r1_probe_calibration);
/// the cap bounds
/// the analysis latency budget).
pub const MAX_PROBES: usize = 8;

/// Tokenization-STABLE anchor candidates of a query (R1 v1):
///
/// * QUOTED spans first, their whole content one candidate (`"vibe coding"`
///   gates as the phrase's term bag — 优先整串, never split into words);
/// * then Latin/alphanumeric identifier RUNS from the remainder — maximal
///   spans of ASCII letters/digits, keeping intra-word `-`/`.`/`_` when
///   flanked by alphanumerics (identifier shapes: `vectorfs`,
///   `ThoughtWorks` camelCase stays whole, `vol-32`, `config.toml`).
///   These survive the index's tokenizer round-trip whatever its CJK
///   policy is — that stability is the v1 scope ruling.
///
/// Deliberately NOT candidates: pure-CJK runs (graceful degradation — the
/// exact analyzer-accurate segmentation of the index is not rebuilt at
/// query time in v1), pure numerals (`2026`, `32` — corpus-structure noise:
/// years and versions, never discriminative), single ASCII chars, runs
/// shorter than 2. Duplicates fold case-insensitively, first spelling wins
/// (the icu tokenizer lowercases Latin, so `Vibe` and `vibe` are the same
/// token to the index).
pub fn anchor_candidates(query: &str) -> Vec<String> {
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

/// Anchor selection over probed candidates (v2, count form): keep the
/// terms whose df is 1..=[`ANCHOR_DF_CEILING`], RAREST FIRST (count
/// ascending, ties keep first-seen order), capped at [`MAX_ANCHORS`].
/// Zero-count terms (corpus-absent) can never gate — the must-clause
/// would zero the result set — and are dropped here.
///
/// THE v2 GATE RULING (measured, 2026-11-11): the caller builds the
/// Boolean MUST clause from the verdict's HEAD — the single rarest
/// anchor — and every further verdict term rides `should`. The
/// AND-of-all-anchors shape was measured to evict correct-answer chunks
/// whose fellow rare terms STRADDLE chunk boundaries: on the f1
/// coined-term fixture `zxqwibble`/`migration`/`gate` are EACH df=1 in
/// three DIFFERENT 32-token windows (AND = zero hits, the needle loses
/// its lexical rank); on the radar corpus any ceiling hearing `coding`
/// (35) also hears `ThoughtWorks` (36), and the needle carries the
/// former but not the latter. The single-rarest-anchor must is the shape
/// the acceptance counterfactual validated (must=[vibe] returns EXACTLY
/// the correct chunk) and it cannot evict a chunk that contains the
/// corpus's rarest query term by construction.
///
/// Pure function on purpose: the unit-test oracle feeds calibrated probe
/// counts (the docs-corpus measurement in [`ANCHOR_DF_CEILING`]'s docs)
/// without needing a live index.
pub fn select_anchors(counted: &[(String, u64)]) -> Vec<String> {
    let mut hits: Vec<(u64, usize, &String)> = counted
        .iter()
        .enumerate()
        .filter_map(|(i, (term, df))| {
            (*df >= 1 && *df <= ANCHOR_DF_CEILING).then_some((*df, i, term))
        })
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    hits.into_iter()
        .take(MAX_ANCHORS)
        .map(|(_, _, term)| term.clone())
        .collect()
}

/// The full query-time analysis (R1 v2): candidates → df probes →
/// anchors, through the kernel's [`LexicalKernel::probe_df`] face. `Err`
/// propagates (a missing FTS face / pre-T9 table — the caller degrades to
/// the ungated behavior and notes it); `Ok(vec![])` = analyzed with zero
/// anchors (pure CJK / all structure words — the ungated behavior IS the
/// answer). Probe latency budget: ≤ [`MAX_PROBES`] small FTS queries —
/// 5-25 ms each measured (see [`PROBE_DF_LIMIT`]'s measurement note),
/// ≈ 40-200 ms for a MAX_PROBES-sized analysis.
pub async fn analyze_anchors(
    kernel: &dyn LexicalKernel,
    storage: &Storage,
    vb: &str,
    ix: &str,
    query: &str,
) -> Result<Vec<String>, StoreError> {
    let candidates = anchor_candidates(query);
    let mut counted: Vec<(String, u64)> = Vec::with_capacity(candidates.len());
    for term in &candidates {
        counted.push((term.clone(), kernel.probe_df(storage, vb, ix, term).await?));
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
    }

    // ---- R1 anchor analysis: candidate extraction ----

    #[test]
    fn candidates_are_latin_identifier_runs_and_quoted_spans() {
        // the origin query: Latin runs whole (camelCase included), CJK is
        // not a candidate shape
        assert_eq!(
            anchor_candidates("ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化"),
            vec!["ThoughtWorks", "Vibe", "Coding"]
        );
        // identifier shapes: hyphen/dot-joined stay one run, prefixes and
        // suffixes fall off (`-v` single char, `flags_` trailing connector);
        // every plain Latin word IS a candidate — including stopword shapes
        // like "and", which the PROBE stage (df ceiling) rejects, not the
        // extractor
        assert_eq!(
            anchor_candidates("vf-search uses config.toml and -v flags_"),
            vec!["vf-search", "uses", "config.toml", "and", "flags"]
        );
        // quoted content is one candidate, priority order (first)
        assert_eq!(
            anchor_candidates("find \"vibe coding\" mentions"),
            vec!["vibe coding", "find", "mentions"]
        );
        // duplicates fold case-insensitively (first spelling wins); pure
        // numerals and single chars never qualify
        assert_eq!(
            anchor_candidates("Vibe vibe coding 2026 32 a I"),
            vec!["Vibe", "coding"]
        );
        // pure CJK: zero candidates — graceful degradation is structural
        assert!(anchor_candidates("技术雷达有哪些变化").is_empty());
    }

    #[test]
    fn candidates_are_capped_at_max_probes_in_priority_order() {
        let query = "\"quoted one\" alpha beta gamma delta epsilon zeta eta theta";
        let got = anchor_candidates(query);
        assert_eq!(got.len(), MAX_PROBES);
        assert_eq!(got[0], "quoted one", "quoted spans keep probe priority");
    }

    // ---- R1 anchor analysis: selection over probe counts ----

    #[test]
    fn selection_uses_the_calibrated_oracle_from_the_docs_corpus() {
        // THE R1 v2 oracle — the MEASURED docs-corpus df table (see
        // ANCHOR_DF_CEILING docs, re-runnable as r1_probe_calibration):
        // the origin query's candidates count vibe=1 / coding=35 /
        // ThoughtWorks=36 — only vibe clears the ceiling, and the verdict
        // orders RAREST FIRST (the head is the MUST term of the v2 gate)
        let counted = vec![
            ("ThoughtWorks".to_string(), 36),
            ("vibe".to_string(), 1),
            ("coding".to_string(), 35),
        ];
        assert_eq!(
            select_anchors(&counted),
            vec!["vibe"],
            "the measured origin-query verdict: the rarest term alone"
        );
        // the rare band the ceiling admits and the order it reports:
        // count ascending (rarest = head = gate), ties first-seen
        let rare = vec![
            ("zxqwibble".to_string(), 1),
            ("gate".to_string(), 1),
            ("migration".to_string(), 3),
        ];
        assert_eq!(
            select_anchors(&rare),
            vec!["zxqwibble", "gate", "migration"],
            "rarest first; ties keep first-seen order"
        );
        // reordering must not move the ORDERING semantics (count sort
        // wins over first-seen; ties stay in query order)
        let reordered = vec![
            ("migration".to_string(), 3),
            ("gate".to_string(), 1),
            ("zxqwibble".to_string(), 1),
        ];
        assert_eq!(
            select_anchors(&reordered),
            vec!["gate", "zxqwibble", "migration"]
        );
        // the structure words stay out: the measured 雷达 109 / 技术 336
        // / ThoughtWorks 36 / coding 35 are all over the ceiling
        let structure = vec![
            ("技术".to_string(), 336),
            ("雷达".to_string(), 109),
            ("ThoughtWorks".to_string(), 36),
            ("coding".to_string(), 35),
        ];
        assert!(select_anchors(&structure).is_empty());
    }

    #[test]
    fn selection_drops_absent_terms_and_caps_all_anchor_queries() {
        // absent (0-count) terms can never gate — a must-clause over a term
        // with zero postings would return NOTHING
        let counted = vec![("zxqwibble".to_string(), 0), ("vibe".to_string(), 1)];
        assert_eq!(select_anchors(&counted), vec!["vibe"]);
        // a query whose every token is rare: the verdict lists the rarest
        // MAX_ANCHORS (observability cap), the GATE still takes the head
        let all_rare: Vec<(String, u64)> = ["aa1", "bb2", "cc3", "dd4", "ee5", "ff6"]
            .iter()
            .enumerate()
            .map(|(i, t)| (t.to_string(), (i + 1) as u64))
            .collect();
        let got = select_anchors(&all_rare);
        assert_eq!(got.len(), MAX_ANCHORS);
        assert_eq!(
            got[0], "aa1",
            "rarest (count 1) is the head = the gate term"
        );
        // a pure structure-word query selects nothing: the ungated
        // behavior IS the answer
        let structure: Vec<(String, u64)> = [("雷达".to_string(), 109)].into_iter().collect();
        assert!(select_anchors(&structure).is_empty());
    }

    /// A probe-mock kernel: fixed term→df answers (0 = corpus-absent), Err
    /// on demand — [`analyze_anchors`] driven with the MEASURED
    /// docs-corpus counts (the `r1_probe_calibration` table) without a
    /// live index.
    struct Probed {
        dfs: Vec<(String, u64)>,
        fail_on: Option<&'static str>,
    }

    impl LexicalKernel for Probed {
        fn name(&self) -> &'static str {
            "probed"
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
            Box::pin(async { Ok(Vec::new()) })
        }
        fn probe_df<'a>(
            &'a self,
            _storage: &'a Storage,
            _vb: &'a str,
            _ix: &'a str,
            term: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<u64, StoreError>> {
            Box::pin(async move {
                if Some(term) == self.fail_on {
                    return Err(StoreError::Internal(format!("probe of {term:?} failed")));
                }
                Ok(self
                    .dfs
                    .iter()
                    .find(|(t, _)| t == term)
                    .map(|(_, d)| *d)
                    .unwrap_or(0))
            })
        }
    }

    #[tokio::test]
    async fn analysis_through_a_mock_probe_reproduces_the_measured_verdict() {
        let storage = crate::store::Storage::memory();
        // the docs-corpus measurement (ANCHOR_DF_CEILING docs): the origin
        // query's candidates count vibe=1 / coding=35 / ThoughtWorks=36 —
        // the verdict is exactly [vibe], whose head is the counterfactual-
        // validated MUST term; a corpus-absent term never reaches it
        let kernel = Probed {
            dfs: vec![
                ("ThoughtWorks".into(), 36),
                ("vibe".into(), 1),
                ("coding".into(), 35),
                ("zxqwibble".into(), 0),
            ],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            "ThoughtWorks 技术雷达中 vibe coding 技术有哪些变化",
        )
        .await
        .unwrap();
        assert_eq!(got, vec!["vibe"], "measured verdict: the rarest term alone");

        // the f1 fixture's measured shape (all three df=1 in different
        // chunks): the verdict lists them, head-first — the gate takes the
        // HEAD (zxqwibble, first-seen among ties), which is what keeps the
        // coined-term needle in its lexical arm (see select_anchors' v2
        // gate ruling)
        let kernel = Probed {
            dfs: vec![
                ("zxqwibble".into(), 1),
                ("migration".into(), 1),
                ("gate".into(), 1),
            ],
            fail_on: None,
        };
        let got = analyze_anchors(
            &kernel,
            &storage,
            "vb",
            "ix",
            "the zxqwibble migration gate",
        )
        .await
        .unwrap();
        assert_eq!(got, vec!["zxqwibble", "migration", "gate"]);

        // a pure structure-word query rides the ungated behavior
        let kernel = Probed {
            dfs: vec![("radar".into(), 109), ("tech".into(), 336)],
            fail_on: None,
        };
        let got = analyze_anchors(&kernel, &storage, "vb", "ix", "radar tech")
            .await
            .unwrap();
        assert!(
            got.is_empty(),
            "over-ceiling counts select nothing: {got:?}"
        );

        // probe FAILURE propagates as Err — the caller (ops_xvf) degrades
        // to the ungated behavior on this exact signal
        let kernel = Probed {
            dfs: vec![("vibe".into(), 1)],
            fail_on: Some("vibe"),
        };
        assert!(matches!(
            analyze_anchors(&kernel, &storage, "vb", "ix", "vibe coding").await,
            Err(StoreError::Internal(_))
        ));
    }
}
