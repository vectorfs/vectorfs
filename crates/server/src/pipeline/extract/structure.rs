//! T12 structure-label heuristics: the line-shape EVIDENCE layer shared by
//! the two PDF extractor kinds (retrieval-quality solution §1 D-1/D-5, ADR
//! 0003 rev). 分层原则「证据在 extract、标签到 chunk、策略在检索」puts the
//! detection here, inside the extract axis, where both engines can feed the
//! SAME predicates — the review-4.3 duty that the label vocabulary
//! ([`ChunkKind`]) never drifts between routes (the fallback chain swaps
//! pdfium/pdf-inspector per document; one vocabulary must survive the swap).
//!
//! Two consumers, two entry points, one shared predicate core:
//!
//! * **pdf-text** (pdfium, [`assemble`]): the whole-document page loop.
//!   pdfium's `.all()` emits page text with `\r\n` line separators and NO
//!   positional metadata (no Y coordinates), so the page-head/tail position
//!   guard is approximated by a line-index window (D-1's recorded
//!   limitation). [`assemble`] normalizes, removes cross-page furniture
//!   under the full gate set, judges TOC pages under the v2 gate set
//!   (below), and
//!   emits the page-offset table + byte-anchored labels + the removal
//!   audit count in one pass.
//! * **pdf-inspector** (markdown flavor, [`markdown_labels`]): the crate's
//!   markdown product is ONE string with no page boundaries, so the
//!   page-level gates adapt to line-RUN gates (see that function). The
//!   plain flavor has no line structure at all (probed 2026-11-08:
//!   `extract_text_mem` concatenates positioned runs without separators)
//!   and gets no labels — the pre-T12 bypass stance of that flavor.
//!
//! # The furniture gate (D-1, two adversarial-review rounds)
//!
//! A REPEATED line is furniture only when EVERY gate passes — the bare
//! "repeats on ≥20% of pages" rule was falsified on the flagship corpus
//! (it would delete the radar's four-ring vocabulary 评估/试验/暂缓/采纳
//! and the quadrant names wholesale):
//!
//! 1. **position**: the line sits in the page-head or page-tail window of
//!    [`FURNITURE_EDGE_LINES`] lines (pdfium `.all()` has no Y coordinates
//!    — the line-index approximation of "top/bottom of page");
//! 2. **substance**: the normalized line is ≥ [`FURNITURE_MIN_LINE_CHARS`]
//!    chars and carries ≥1 alphanumeric (this is what saves the 2-char
//!    ring words — 评估 is 2 chars and survives);
//! 3. **cross-page repetition**: identical AFTER normalization (trim +
//!    strip the leading/trailing digit runs — page numbers must not break
//!    identity) on ≥20% of pages ([`FURNITURE_PAGE_PERCENT`]) OR on a
//!    streak of [`FURNITURE_MIN_STREAK`] consecutive pages;
//! 4. **short-document abstention**: fewer than [`FURNITURE_MIN_PAGES`]
//!    pages and NOTHING is removed (evidence too thin — 宁可不删).
//!
//! A line qualifying only through the STREAK branch (a running chapter
//! title repeats inside its chapter's page range, not document-wide) is
//! NOT deleted: it is DEMOTED to a [`ChunkKind::Caption`] span — kept in
//! the text, searchable, excludable (可逆性, the D-1 carve-out for
//! 页首/尾标题类重复). Lines qualifying through the ≥20% branch are removed
//! and counted into the `furniture_removed` audit channel.
//!
//! # The TOC gate, v2 (D-1 门 v2, evidence-driven revision 2026-11-08)
//!
//! A page is judged TOC when EVERY gate passes:
//!
//! 1. **strict line predicate** ([`toc_entry_page`]): a trailing 1–4 digit
//!    run that is its OWN token (a leader char before it) with ≥1
//!    non-digit character before it past any leaders — kills bare
//!    version-number pairs (`41 42`), glued codes (`57. D2`) and decimal
//!    prices (`苹果 5.5`); leaders are arbitrary incl. whitespace-only
//!    (无导符目录行) and full-width (`……`/`・・`);
//! 2. **hit ratio ≥ [`TOC_MIN_LINE_RATIO_PERCENT`]** of the page's content
//!    lines;
//! 3. **monotonic page column** (second evidence): the hit lines' page
//!    numbers non-decreasing in line order — true TOCs are ordered;
//!    overview/price/SKU pages carry enumeration indices or amounts and
//!    are not (the gate that killed the flagship corpus's 5 naive
//!    false-positive pages);
//! 4. **range plausibility** (v2, the title gate's successor): the hit
//!    column's MAXIMUM ≤ the document's total page count — a real TOC's
//!    page numbers are bounded by the document it indexes; sorted price
//!    tables and ranked lists whose trailing numbers are amounts usually
//!    overflow that bound.
//!
//! The v1 **title-line hard gate** (「目录/Contents/Table of Contents」must
//! be present) was FALSIFIED by the flagship live corpus (2026-11-08
//! first run): the radar's genuine TOC pages carry NO title line at all
//! (vol32 p2: 10/10 content lines predicate hits, monotonic column
//! 1..39, max ≤ the 46-page document — rejected solely for the missing
//! title; every volume in the corpus fared the same, zero Toc spans
//! anywhere). v2 demotes the title line to an OPTIONAL ENHANCEMENT
//! signal — present ⇒ diagnostic value for a future ingest-status
//! consumer, never required — and hands its residual-misjudgment duty to
//! the range gate, which is both cheaper and evidence-shaped (the
//! amounts-overflow-the-page-count class it must catch is exactly the
//! class the title gate was adjudicated against). Line-level span output
//! is the structural reason the demotion is safe: a misjudged line costs
//! its own exclusion, never a whole page (v1's whole-page blast radius
//! was the original motivation for the title gate).
//!
//! **Declared narrow residual (v2)**: a page/run whose trailing numbers
//! are all ≤ the page count AND monotonic AND ≥50% of content — e.g. a
//! small-amount SORTED price table in a large document, or a ranked list
//! ending in small indices — is judged TOC. Blast radius = those lines
//! (line-level spans, 宁可漏排不可误排 at the window kind); accepted
//! honestly rather than re-hardening the gate that the live corpus just
//! falsified.
//!
//! # The chapter map (R2 CCH, 2026-11-11)
//!
//! The judged hits are also consumed, not merely labeled: [`assemble`]
//! upgrades them into `Assembled::sections` — `(target page, entry title)`
//! pairs in TOC-appearance order, the document-level SECTION MAP the
//! chunker's `context-header = "toc-path"` axis reads to prefix each
//! chunk's embedding input with its chapter context (「TOC 被消费而非被
//! 索引」, the PageIndex/docling precedent — the map rides the Part as an
//! annotation channel and never touches the text). Only the pdfium path
//! produces it: the page table it presupposes exists there alone
//! (pdf-inspector's markdown flavor is pageless — its TOC RUNS label fine
//! but cannot ground a page-keyed map; v1 derives the map from TOC alone,
//! honestly declared).
//!
//! # Honest scope (v1 declarations)
//!
//! * 卷末索引 (end-of-volume indexes without page columns) — v1 不治, v1.1
//!   gets its own predicate family (D-1's range declaration);
//! * glued page numbers (`绪论12`, no separator at all) are NOT covered by
//!   the strict predicate — rejecting `57. D2`-shaped codes requires the
//!   separator, and 宁可漏排不可误排 takes the loss;
//! * ASCII digits only — full-width page digits are not treated as a page
//!   column (not observed on the evidence corpus).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::super::{ChunkKind, LabelSpan};

// ---------------------------------------------------------------------------
// mechanism constants (守门常数 — adjudicated, NOT configuration; relaxing
// any of these is rework by the two adversarial-review rounds, D-1/§4)
// ---------------------------------------------------------------------------

/// Page-head and page-tail line window for the furniture position guard:
/// a repeated line must sit within the first/last 3 lines of its page
/// (pdfium `.all()` has no Y coordinates; 3 lines ≈ a header/footer band).
pub(in crate::pipeline::extract) const FURNITURE_EDGE_LINES: usize = 3;

/// Minimum normalized line length for furniture candidacy: 2-char status
/// words (the radar ring vocabulary 评估/试验/暂缓/采纳) stay in the text.
pub(in crate::pipeline::extract) const FURNITURE_MIN_LINE_CHARS: usize = 4;

/// Minimum page count before ANY furniture verdict: below 3 pages the
/// cross-page evidence is too thin — 宁可不删 (D-1: <3 页双路不删).
pub(in crate::pipeline::extract) const FURNITURE_MIN_PAGES: usize = 3;

/// Cross-page repetition fraction (percent) that proves document-global
/// furniture (© lines, ISSN straps): repetition on ≥20% of pages deletes.
pub(in crate::pipeline::extract) const FURNITURE_PAGE_PERCENT: usize = 20;

/// Consecutive-page streak that proves a RUNNING title (连击 ≥3): qualifies
/// a line for the Caption demotion when the ≥20% branch did not.
pub(in crate::pipeline::extract) const FURNITURE_MIN_STREAK: usize = 3;

/// TOC hit ratio over a page's content (non-blank) lines: ≥50% required on
/// top of the v2 gate set (D-1: strict 行占比 ≥50% 才判 TOC 页).
pub(in crate::pipeline::extract) const TOC_MIN_LINE_RATIO_PERCENT: usize = 50;

/// Maximum digits in a TOC page column entry (1–4; longer runs are amounts
/// or IDs, not page numbers).
pub(in crate::pipeline::extract) const TOC_PAGE_NUM_MAX_DIGITS: usize = 4;

// ---------------------------------------------------------------------------
// the strict TOC line predicate (shared core — both engines, one vocabulary)
// ---------------------------------------------------------------------------

/// Leader characters tolerated between a TOC title and its page number:
/// whitespace (incl. full-width U+3000), ASCII/full-width dots, middle
/// dots and ellipses (导符任意含空格——D-1; the dot-leader AND the
/// no-leader/space-only 无导符 shapes both pass).
fn is_leader_char(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | '．' | '·' | '・' | '…')
}

/// The strict TOC-entry predicate (D-1): does `line` end in a 1–4 digit
/// page column that is its OWN token, with ≥1 non-digit character before
/// it past any leaders? Returns the page number when it does.
///
/// Rejected shapes (each one an adjudicated counterexample):
/// * `41 42` / `41 42 43` — bare version/index pairs (the char before the
///   trailing run, past the leaders, is a digit);
/// * `57. D2` — a glued alphanumeric code (no leader directly before the
///   digit run; the trailing 2 belongs to the token `D2`);
/// * `苹果 5.5` — decimal prices (the `5` before the dot ends the title
///   side after leader-stripping);
/// * `评估×22页` — glued unit counts (no trailing digit run at all);
/// * `12` — a bare page number with no title side.
pub(in crate::pipeline::extract) fn toc_entry_page(line: &str) -> Option<u32> {
    toc_entry(line).map(|(_, page)| page)
}

/// The R2 CCH upgrade of [`toc_entry_page`]: the same strict predicate,
/// returning BOTH halves of the entry — `(title, page)` — because the
/// section map (below) consumes the TITLE side, not just the column. The
/// title is the line minus its trailing leaders and digit run, trimmed of
/// leading leaders (indentation is not part of a chapter's name);
/// non-empty by the predicate itself.
pub(in crate::pipeline::extract) fn toc_entry(line: &str) -> Option<(&str, u32)> {
    let trimmed = line.trim_end();
    // the trailing ASCII-digit run
    let digits = trimmed
        .bytes()
        .rev()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 || digits > TOC_PAGE_NUM_MAX_DIGITS {
        return None;
    }
    let split = trimmed.len() - digits;
    let num: u32 = trimmed[split..].parse().ok()?;
    let title_side = &trimmed[..split];
    // the page number must be its own token: the char directly before the
    // run is a leader (space/dot/ellipsis) — this is what kills `57. D2`
    // (the run belongs to a word) while accepting `绪论 1` and `绪论 … 1`
    match title_side.chars().next_back() {
        Some(c) if is_leader_char(c) => {}
        _ => return None,
    }
    // ≥1 non-digit character before the page number, past any leaders:
    // `41 42` reduces to `41` (ends in a digit) and fails here
    let title = title_side.trim_end_matches(is_leader_char);
    if title.is_empty() || title.ends_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((title.trim_start_matches(is_leader_char), num))
}

/// The furniture normalization key: trim, strip ONE leading and ONE
/// trailing ASCII-digit run (page numbers must not break cross-page line
/// identity — `Golden Radar Page 3` and `Golden Radar Page 4` are the same
/// strap), trim again. `None` when the normalized line fails the substance
/// gates (too short, or nothing alphanumeric) — such lines are never
/// furniture candidates (the 2-char ring words exit here).
pub(in crate::pipeline::extract) fn furniture_key(line: &str) -> Option<String> {
    let stripped = line
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start()
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end();
    if stripped.chars().count() < FURNITURE_MIN_LINE_CHARS {
        return None;
    }
    if !stripped.chars().any(|c| c.is_alphanumeric()) {
        return None;
    }
    Some(stripped.to_string())
}

/// Is `line`'s index within the page-head/tail position window?
fn in_edge_window(line_idx: usize, line_count: usize) -> bool {
    line_idx < FURNITURE_EDGE_LINES || line_idx + FURNITURE_EDGE_LINES >= line_count
}

// ---------------------------------------------------------------------------
// pdfium path: the whole-document assembly (furniture + TOC + page table)
// ---------------------------------------------------------------------------

/// One line's furniture verdict inside a page (the cross-page evidence
/// resolved; see the module doc for the gate order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineFate {
    /// No verdict — the line stays, unlabeled.
    Keep,
    /// Document-global furniture (≥20% of pages): the line is REMOVED from
    /// the product and counted.
    Remove,
    /// Running-title repeat (streak-only qualification): the line STAYS,
    /// demoted to a Caption span (可检索可排除, D-1's reversibility carve-out).
    Demote,
}

/// [`assemble`]'s product: the extraction OUTPUT channels the pdfium page
/// loop hands to its `Part`.
pub(in crate::pipeline::extract) struct Assembled {
    pub text: String,
    pub labels: Vec<LabelSpan>,
    /// (1-based page number → BYTE offset into `text` where the page's
    /// kept text begins) — the T12/T13 shared surgery channel.
    pub pages: Vec<(u32, usize)>,
    /// Audit count of REMOVED furniture lines (the ingest-status channel;
    /// this slice produces it, slice 3 consumes it).
    pub furniture_removed: usize,
    /// CHAPTER-SECTION map (R2 CCH): `(target page, entry title)` pairs in
    /// TOC-appearance order — the TOC judgment's hits upgraded from line
    /// labels into a document-level map (页码 → 最近前序条目标题 at lookup
    /// time; the per-page column monotonicity the gate already proved is
    /// what makes "nearest preceding entry" well-defined). Empty when no
    /// page judged TOC — the context-header axis then degrades to
    /// filename-only prefixes for this document.
    pub sections: Vec<(u32, String)>,
}

/// The pdfium page loop's structure-aware assembly (T12): normalize each
/// page's raw text (`\r\n` → `\n` — the line-detection premise), resolve
/// furniture fates across pages, judge each page under the TOC v2 gates,
/// then join kept lines (page `"\n"` inside, `"\n\n"` between) while
/// recording the page-offset table and byte-anchored label spans.
///
/// Pure function over the per-page raw texts — every gate is unit-tested
/// here without the engine; the golden corpus pins the same gates through
/// the real `.so`.
pub(in crate::pipeline::extract) fn assemble(raw_pages: &[&str]) -> Assembled {
    let normalized: Vec<String> = raw_pages
        .iter()
        .map(|p| p.replace("\r\n", "\n").replace('\r', "\n"))
        .collect();
    let lines: Vec<Vec<&str>> = normalized.iter().map(|p| p.split('\n').collect()).collect();
    let fates = furniture_fates(&lines);
    // per-page TOC judgment over the KEPT lines (furniture is invisible to
    // the gate: D-1's 页内家具同行剔除 — a removed strap cannot vote). Each
    // hit carries its TARGET page (the R2 upgrade: the hits feed the
    // section map, which needs the column, not just the line indices)
    let toc_hits: Vec<Vec<(usize, u32)>> = lines
        .iter()
        .zip(&fates)
        .map(|(page, fates)| judge_toc_page(page, fates, lines.len() as u32).unwrap_or_default())
        .collect();
    // the document-level CHAPTER map (R2 CCH): every judged entry in
    // appearance order — (target page, title) — the raw material the
    // chunker's nearest-preceding lookup consumes (monotonic within each
    // judged page by the gate; appearance order across pages)
    let sections: Vec<(u32, String)> = lines
        .iter()
        .zip(&toc_hits)
        .flat_map(|(page, hits)| {
            hits.iter().filter_map(|&(l, target)| {
                toc_entry(page[l]).map(|(title, _)| (target, title.to_string()))
            })
        })
        .collect();

    let mut text = String::new();
    let mut labels: Vec<LabelSpan> = Vec::new();
    let mut pages: Vec<(u32, usize)> = Vec::with_capacity(lines.len());
    let mut furniture_removed = 0usize;
    for (p, page_lines) in lines.iter().enumerate() {
        if p > 0 {
            text.push_str("\n\n");
        }
        pages.push(((p + 1) as u32, text.len()));
        // the open TOC run: (span start byte, last hit line idx, span end
        // byte) — consecutive hit lines merge into one span (the '\n'
        // between them is inside the span); any gap flushes it
        let mut run: Option<(usize, usize, usize)> = None;
        let mut first_kept = true;
        for (l, line) in page_lines.iter().enumerate() {
            match fates[p][l] {
                LineFate::Remove => {
                    furniture_removed += 1;
                    continue;
                }
                LineFate::Keep | LineFate::Demote => {}
            }
            if !first_kept {
                text.push('\n');
            }
            first_kept = false;
            let start = text.len();
            text.push_str(line);
            let end = text.len();
            if fates[p][l] == LineFate::Demote {
                labels.push(LabelSpan {
                    range: start..end,
                    kind: ChunkKind::Caption,
                });
            }
            if toc_hits[p].iter().any(|&(i, _)| i == l) {
                run = match run {
                    // contiguous by ORIGINAL line index: extend through
                    // the '\n' just pushed (it lies inside the span)
                    Some((rs, last_idx, _)) if last_idx + 1 == l => Some((rs, l, end)),
                    Some((rs, _, re)) => {
                        labels.push(LabelSpan {
                            range: rs..re,
                            kind: ChunkKind::Toc,
                        });
                        Some((start, l, end))
                    }
                    None => Some((start, l, end)),
                };
            }
        }
        if let Some((rs, _, re)) = run {
            labels.push(LabelSpan {
                range: rs..re,
                kind: ChunkKind::Toc,
            });
        }
    }
    // spans are emitted page-ordered but a run flushes after page-tail
    // captions — sort by start for a tidy, deterministic product
    labels.sort_by_key(|s| s.range.start);
    Assembled {
        text,
        labels,
        pages,
        furniture_removed,
        sections,
    }
}

/// Resolve every page-edge line's furniture fate from the cross-page
/// evidence (the four-gate adjudication; see module docs).
fn furniture_fates(pages: &[Vec<&str>]) -> Vec<Vec<LineFate>> {
    let mut fates: Vec<Vec<LineFate>> = pages
        .iter()
        .map(|lines| vec![LineFate::Keep; lines.len()])
        .collect();
    // short-document abstention: <3 pages ⇒ no verdict at all
    if pages.len() < FURNITURE_MIN_PAGES {
        return fates;
    }
    // keys per page per line (None outside the substance gates)
    let keys: Vec<Vec<Option<String>>> = pages
        .iter()
        .map(|lines| lines.iter().map(|l| furniture_key(l)).collect())
        .collect();
    // cross-page evidence from PAGE-EDGE positions only: key → page set
    let mut evidence: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (p, page_keys) in keys.iter().enumerate() {
        for (l, key) in page_keys.iter().enumerate() {
            if let Some(k) = key.as_ref().filter(|_| in_edge_window(l, pages[p].len())) {
                evidence.entry(k.to_string()).or_default().insert(p);
            }
        }
    }
    // the class of each qualifying key: the ≥20% branch deletes; the
    // streak-only branch demotes (running titles keep their text)
    let mut class_of: BTreeMap<&str, LineFate> = BTreeMap::new();
    for (k, page_set) in &evidence {
        // 跨页重复: repetition means ≥2 DISTINCT pages first (a line seen
        // on one page never repeated at all — the floor only bites on
        // tiny documents, where 20% of the page count rounds below one
        // page), THEN the ≥20% fraction
        let global =
            page_set.len() >= 2 && page_set.len() * 100 >= pages.len() * FURNITURE_PAGE_PERCENT;
        // longest run of CONSECUTIVE page indices (连击): BTreeSet iterates
        // ascending, so a run extends while the next page is +1
        let mut streak = 0usize;
        let mut cur = 0usize;
        let mut prev: Option<usize> = None;
        for &p in page_set {
            cur = if prev.is_some_and(|q| p == q + 1) {
                cur + 1
            } else {
                1
            };
            streak = streak.max(cur);
            prev = Some(p);
        }
        let fate = if global {
            LineFate::Remove
        } else if streak >= FURNITURE_MIN_STREAK {
            LineFate::Demote
        } else {
            continue;
        };
        class_of.insert(k, fate);
    }
    for (p, page_keys) in keys.iter().enumerate() {
        for (l, key) in page_keys.iter().enumerate() {
            let fate = key
                .as_ref()
                .filter(|_| in_edge_window(l, pages[p].len()))
                .and_then(|k| class_of.get(k.as_str()).copied());
            if let Some(fate) = fate {
                fates[p][l] = fate;
            }
        }
    }
    fates
}

/// The per-page TOC gate v2 (D-1 门 v2): over the page's KEPT, non-blank
/// lines — hit ratio ≥ 50%, the hit lines' page column monotonic
/// non-decreasing, and the column's maximum within the document's page
/// count (range plausibility, the demoted title gate's successor — see
/// the module docs for the falsification record and the declared narrow
/// residual). Returns the hits as `(line index, target page)` pairs
/// (line-level spans, never whole-page bytes; the target pages ride along
/// since R2 — the section map consumes them) when the page is judged TOC;
/// `None` = 放行不判.
fn judge_toc_page(
    lines: &[&str],
    fates: &[LineFate],
    total_pages: u32,
) -> Option<Vec<(usize, u32)>> {
    let mut content = 0usize;
    let mut hits: Vec<(usize, u32)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if fates.get(i) == Some(&LineFate::Remove) {
            continue; // removed furniture is invisible to the gate
        }
        if line.trim().is_empty() {
            continue; // blank lines vote in neither the ratio nor the gate
        }
        content += 1;
        if let Some((_, n)) = toc_entry(line) {
            hits.push((i, n));
        }
    }
    if content == 0 {
        return None;
    }
    if hits.len() * 100 < content * TOC_MIN_LINE_RATIO_PERCENT {
        return None;
    }
    if !hits.windows(2).all(|w| w[0].1 <= w[1].1) {
        return None; // the column is not ordered — overview/price shape
    }
    // range plausibility: a real TOC points INSIDE its own document; the
    // amounts of a sorted price table overflow the page count — the
    // v2 successor of the title gate's residual duty
    if hits.iter().any(|&(_, n)| n > total_pages) {
        return None;
    }
    Some(hits)
}

// ---------------------------------------------------------------------------
// pdf-inspector path: line-run labels over the markdown product
// ---------------------------------------------------------------------------

/// Caption prefix families mirrored from the crate's `pub(crate)`
/// `markdown::classify::is_caption_line` (registry 1.25.2): a line that
/// STARTS with one of these is a figure/table caption or source citation.
/// The crate emits such lines as plain text (probed: convert.rs renders
/// them with only a paragraph break — no marker survives into the
/// markdown), so the lexical line shape IS the output-side signal.
const CAPTION_ALWAYS_PREFIXES: &[&str] = &[
    "Figura ",
    "Fig. ",
    "Fig ",
    "Tabela ",
    "Source:",
    "Fonte:",
    "Source ",
    "Fonte ",
    "Note:",
    "Nota:",
    "Chart ",
    "Gráfico ",
    "Graph ",
    "Diagram ",
    "Image ",
    "Imagem ",
    "Photo ",
    "Foto ",
];

/// Caption line-shape predicate (the re-implemented crate evidence): the
/// always-prefixes above, plus `Figure `/`Table ` only when a digit, `(`,
/// or `#` follows — the crate's own distinction of "Table 3" (caption)
/// from "Table of Contents" (heading).
fn is_caption_line(line: &str) -> bool {
    let trimmed = line.trim();
    if CAPTION_ALWAYS_PREFIXES
        .iter()
        .any(|p| trimmed.starts_with(p))
    {
        return true;
    }
    let lower = trimmed.to_lowercase();
    for p in ["figure ", "table "] {
        let shaped = lower.strip_prefix(p).is_some_and(|rest| {
            rest.trim_start()
                .starts_with(|c: char| c.is_ascii_digit() || c == '(' || c == '#')
        });
        if shaped {
            return true;
        }
    }
    lower.starts_with("source:")
}

/// The markdown flavor's label scan (T12): the crate's product is ONE
/// string without page boundaries, so the PAGE gates adapt to RUN gates —
///
/// * **run** = a maximal sequence of predicate-hit lines where only blank
///   lines may intervene (the crate sometimes merges entries into one
///   paragraph — then the merged line is a 1-line run and still judged);
/// * a run becomes a Toc span when it has ≥2 hits, its page column is
///   monotonic non-decreasing, and the column's maximum is within
///   `total_pages` (the v2 range gate — same rule set as the pdfium page
///   gate; the title-line arming of v1 is demoted here the same way, per
///   the flagship falsification, module docs);
/// * caption-shaped lines OUTSIDE Toc spans get Caption spans (a
///   table-of-tables entry "Table 3 断窗机制 12" is TOC, not caption —
///   the scan order encodes the priority).
///
/// **Legend is not produced on this path** (D-5 尽力条款 declared): the
/// crate has no legend classification and the markdown product exposes no
/// legend-side signal (probed — registry `markdown/` carries none).
pub(in crate::pipeline::extract) fn markdown_labels(
    text: &str,
    total_pages: u32,
) -> Vec<LabelSpan> {
    // line table with byte ranges (start..end excludes the '\n')
    let lines: Vec<(usize, usize, &str)> = {
        let mut v = Vec::new();
        let mut pos = 0usize;
        for line in text.split('\n') {
            v.push((pos, pos + line.len(), line));
            pos += line.len() + 1;
        }
        v
    };
    // TOC runs: hit lines bridged by blanks, gated by run length ≥2 +
    // monotonic page column + range plausibility (the v2 evidences in
    // their page-free form)
    let mut toc: Vec<LabelSpan> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    let flush = |run: &mut Vec<usize>, toc: &mut Vec<LabelSpan>| {
        if run.len() >= 2 {
            let nums: Vec<u32> = run
                .iter()
                .map(|&i| toc_entry_page(lines[i].2).expect("hit line"))
                .collect();
            let monotonic = nums.windows(2).all(|w| w[0] <= w[1]);
            let in_range = nums.iter().all(|&n| n <= total_pages);
            if monotonic && in_range {
                let (s, _, _) = lines[run[0]];
                let (_, e, _) = lines[*run.last().unwrap()];
                toc.push(LabelSpan {
                    range: s..e,
                    kind: ChunkKind::Toc,
                });
            }
        }
        run.clear();
    };
    for (i, (_, _, line)) in lines.iter().enumerate() {
        match toc_entry_page(line) {
            Some(_) => run.push(i),
            // blanks bridge entries (the crate sometimes paragraph-breaks
            // a TOC into separate blocks)
            None if line.trim().is_empty() => {}
            None => flush(&mut run, &mut toc),
        }
    }
    flush(&mut run, &mut toc);
    // caption scan over lines OUTSIDE Toc spans (a table-of-tables entry
    // is TOC, not caption — the scan order encodes the priority)
    let mut out = toc;
    for (s, e, line) in &lines {
        if is_caption_line(line)
            && !out
                .iter()
                .any(|t| t.kind == ChunkKind::Toc && t.range.start <= *s && *e <= t.range.end)
        {
            out.push(LabelSpan {
                range: *s..*e,
                kind: ChunkKind::Caption,
            });
        }
    }
    out.sort_by_key(|s| s.range.start);
    out
}

// ---------------------------------------------------------------------------
// tests — the pure-function gate matrix (no engine, no .so: the same
// gates run through the real pdfium in tests/golden_corpus.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A page from lines (the pdfium shape after `\r\n` normalization).
    fn page(lines: &[&str]) -> String {
        lines.join("\n")
    }

    // ---- the strict TOC predicate (the five adjudicated families) ----

    #[test]
    fn toc_predicate_accepts_leader_and_no_leader_shapes() {
        // 无导符 (space-only) — the shape that broke the crate's
        // dot-leader predicate on the radar corpus
        assert_eq!(toc_entry_page("第一章 检索质量方案 12"), Some(12));
        assert_eq!(toc_entry_page("Methods and Materials 3"), Some(3));
        // indented entries keep working (leading whitespace is title side)
        assert_eq!(toc_entry_page("    1.1 研究背景 8"), Some(8));
        // ASCII dot leaders, any length
        assert_eq!(
            toc_entry_page("第一章 绪论 .................... 1"),
            Some(1)
        );
        // full-width leaders: ellipsis, middle dots, full-width space
        assert_eq!(toc_entry_page("第二章 方法 …… 12"), Some(12));
        assert_eq!(toc_entry_page("第三章 结果 ・・・・ 8"), Some(8));
        assert_eq!(toc_entry_page("第四章 讨论　5"), Some(5));
        // repeated page numbers are legal (two entries onto one page)
        assert_eq!(toc_entry_page("附录 A 术语表 40"), Some(40));
        // 4-digit pages are the documented ceiling…
        assert_eq!(toc_entry_page("very long manual 1234"), Some(1234));
        // …5 digits is an amount/ID, not a page
        assert_eq!(toc_entry_page("invoice line 54321"), None);
    }

    #[test]
    fn toc_predicate_rejects_the_counterexample_families() {
        // bare version-number / index pairs (裸版号行)
        assert_eq!(toc_entry_page("41 42"), None);
        assert_eq!(toc_entry_page("41 42 43"), None);
        assert_eq!(toc_entry_page("2.0.1"), None);
        // glued alphanumeric codes — the trailing 2 belongs to `D2`
        assert_eq!(toc_entry_page("57. D2"), None);
        assert_eq!(toc_entry_page("issue A7 item B3"), None);
        // decimal prices (价格表行): the digit left of the dot ends the
        // title side after leader-stripping
        assert_eq!(toc_entry_page("苹果 iPhone 5.5"), None);
        assert_eq!(toc_entry_page("香蕉 3.2 元"), None); // no trailing digits at all
        // bare page numbers with no title side
        assert_eq!(toc_entry_page("12"), None);
        assert_eq!(toc_entry_page("   "), None);
        // glued unit counts (环词分组行): the trailing run is a word, not
        // a page column — the radar's 评估×22页 family
        assert_eq!(toc_entry_page("评估×22页"), None);
        assert_eq!(toc_entry_page("试验×20"), None);
        assert_eq!(toc_entry_page("采纳 9条"), None);
        // section numbers with decimal refs as the last column
        assert_eq!(toc_entry_page("结果与讨论 3.2"), None);
        // plain prose / headings with no trailing digits
        assert_eq!(toc_entry_page("第一章 绪论"), None);
        assert_eq!(toc_entry_page("目录"), None);
        // NOTE (documented acceptance): `gadget alpha 5999` IS
        // predicate-shaped — integer prices with space separators hit;
        // the MONOTONICITY gate is what rejects price pages (see
        // toc_page_gate below), per D-1's evidence split
        assert_eq!(toc_entry_page("gadget alpha 5999"), Some(5999));
    }

    /// The R2 title side (the section map's material): leaders, the page
    /// number, and leading indentation drop; the chapter's NAME survives
    /// verbatim, CJK included.
    #[test]
    fn toc_entry_returns_the_title_side_for_the_section_map() {
        assert_eq!(
            toc_entry("第一章 检索质量方案 12"),
            Some(("第一章 检索质量方案", 12))
        );
        assert_eq!(
            toc_entry("第一章 绪论 .................... 1"),
            Some(("第一章 绪论", 1))
        );
        assert_eq!(toc_entry("第二章 方法 …… 12"), Some(("第二章 方法", 12)));
        // indentation is not part of a chapter's name
        assert_eq!(toc_entry("    1.1 研究背景 8"), Some(("1.1 研究背景", 8)));
        assert_eq!(
            toc_entry("Methods and Materials 3"),
            Some(("Methods and Materials", 3))
        );
        assert_eq!(toc_entry("12"), None);
    }

    // ---- the per-page TOC gate, v2 (D-1 门 v2) ----

    #[test]
    fn toc_page_gate_v2_matrix() {
        let all_keep = |n: usize| vec![LineFate::Keep; n];
        const TOTAL: u32 = 46; // the flagship vol32 shape
        // the v2 HEADLINE positive: a TITLELESS genuine TOC page — the
        // radar shape that falsified the v1 title hard gate (vol32 p2:
        // no 目录/Contents line anywhere, 10/10 hits, column 1..39)
        let radar = [
            "关于技术雷达 3",
            "雷达一览 4",
            "贡献者 5",
            "制作团队人员 6",
            "本期主题 7",
            "采纳 10",
            "试验 18",
            "评估 27",
            "暂缓 34",
            "工具 39",
        ];
        let fates = all_keep(radar.len());
        assert_eq!(
            judge_toc_page(&radar, &fates, TOTAL),
            Some(vec![
                (0, 3),
                (1, 4),
                (2, 5),
                (3, 6),
                (4, 7),
                (5, 10),
                (6, 18),
                (7, 27),
                (8, 34),
                (9, 39)
            ])
        );
        // a titled TOC judges identically — the title line is INERT under
        // v2 (present ⇒ diagnostic value only, never a requirement)
        let titled = [
            "目录",
            "第一章 绪论 1",
            "第二章 方法 3",
            "第三章 结果 3",
            "第四章 讨论 7",
        ];
        let fates = all_keep(titled.len());
        assert_eq!(
            judge_toc_page(&titled, &fates, TOTAL),
            Some(vec![(1, 1), (2, 3), (3, 3), (4, 7)])
        );
        let titled = ["TABLE OF CONTENTS", "introduction 1", "methods 3"];
        let fates = all_keep(titled.len());
        assert_eq!(
            judge_toc_page(&titled, &fates, TOTAL),
            Some(vec![(1, 1), (2, 3)])
        );
        // equal page numbers stay legal (non-DECREASING, not increasing)
        let flat = ["one 3", "two 3", "three 3"];
        let fates = all_keep(flat.len());
        assert_eq!(
            judge_toc_page(&flat, &fates, TOTAL),
            Some(vec![(0, 3), (1, 3), (2, 3)])
        );
        // non-monotonic column (一览页/计数页): rejected — the flagship's
        // naive-predicate false positives all carry unordered columns
        let overview = ["评估 22", "试验 20", "暂缓 11", "采纳 9"];
        let fates = all_keep(overview.len());
        assert_eq!(judge_toc_page(&overview, &fates, TOTAL), None);
        // RANGE gate (the v2 title-successor): a SORTED price table —
        // monotonic column, 100% ratio, titleless — whose amounts OVERFLOW
        // the document's page count is rejected by the bound alone
        let sorted_prices = [
            "gadget alpha 129",
            "gadget bravo 899",
            "gadget charlie 2499",
            "gadget delta 5999",
        ];
        let fates = all_keep(sorted_prices.len());
        assert_eq!(judge_toc_page(&sorted_prices, &fates, TOTAL), None);
        // ratio < 50%: a lone hit among body lines is not a TOC page
        let sparse = [
            "第一章 绪论 1",
            "ordinary prose line two",
            "ordinary prose line three",
        ];
        let fates = all_keep(sparse.len());
        assert_eq!(judge_toc_page(&sparse, &fates, TOTAL), None);
        // blank lines vote in neither direction (ratio denominator is
        // CONTENT lines)
        let with_blanks = ["", "第一章 绪论 1", "", "第二章 方法 2", ""];
        let fates = all_keep(with_blanks.len());
        assert_eq!(
            judge_toc_page(&with_blanks, &fates, TOTAL),
            Some(vec![(1, 1), (3, 2)])
        );
        // removed furniture is invisible: the strap does not vote
        let with_strap = ["© 2026 vectorfs", "第一章 绪论 1", "第二章 方法 2"];
        let fates = vec![LineFate::Remove, LineFate::Keep, LineFate::Keep];
        assert_eq!(
            judge_toc_page(&with_strap, &fates, TOTAL),
            Some(vec![(1, 1), (2, 2)])
        );
    }

    /// The v2 DECLARED NARROW RESIDUAL, pinned honestly: a page whose
    /// trailing numbers are small enough to sit inside the page count AND
    /// sorted AND ≥50% of content — e.g. a small-amount ascending price
    /// list, or a ranked list with tiny indices — is judged TOC. The
    /// blast radius is its own lines (line-level spans; the window kind
    /// arbitrates 宁可漏排不可误排), the cost the flagship falsification
    /// imposed for treating real titleless TOCs.
    #[test]
    fn toc_page_gate_v2_narrow_residual_is_declared_not_hidden() {
        let all_keep = |n: usize| vec![LineFate::Keep; n];
        let ranked_small = ["item alpha 3", "item bravo 8", "item charlie 21"];
        let fates = all_keep(ranked_small.len());
        // every number ≤ the 46-page bound: judged (a real TOC is
        // indistinguishable from this shape at the line level)
        assert_eq!(
            judge_toc_page(&ranked_small, &fates, 46),
            Some(vec![(0, 3), (1, 8), (2, 21)])
        );
        // the SAME lines in a 2-page document: the bound tightens and the
        // residual disappears — the range gate scales with the document
        assert_eq!(judge_toc_page(&ranked_small, &fates, 2), None);
    }

    // ---- the furniture gate matrix ----

    #[test]
    fn furniture_gate_matrix() {
        // © strap on EVERY page head AND tail: 100% ≥ 20% ⇒ removed,
        // audited — both edge occurrences count
        let mut pages = Vec::new();
        for i in 0..10 {
            pages.push(page(&[
                "© 2026 vectorfs monthly",
                &format!("评估 prose {i} body line one"),
                &format!("second prose {i} body line two"),
                "© 2026 vectorfs monthly", // same strap at the tail too
            ]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 20, "both edge occurrences x 10 pages");
        assert!(!a.text.contains("© 2026 vectorfs"));
        // kept pages still carry the per-page prose
        assert!(a.text.contains("评估 prose 0 body line one"));
        assert!(a.text.contains("second prose 9 body line two"));

        // the ring words (评估/试验/暂缓/采纳) at page heads on ≥20% of
        // pages: 2 chars < FURNITURE_MIN_LINE_CHARS ⇒ NEVER removed — the
        // adversarial-review counterexample that shaped the substance gate
        let mut pages = Vec::new();
        let ring = ["评估", "试验", "暂缓", "采纳"];
        for i in 0..10 {
            pages.push(page(&[
                ring[i % ring.len()],
                &format!("chapter {i} prose line"),
            ]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 0);
        for w in ring {
            assert!(a.text.contains(w), "ring word {w} must survive");
        }

        // a MID-page repeated line never enters evidence (position guard):
        // 7 lines/page ⇒ index 3 is outside both edge windows; the window
        // lines stay unique per page so ONLY the guard is under test
        let mut pages = Vec::new();
        for i in 0..10 {
            pages.push(page(&[
                &format!("head {i} line"),
                &format!("filler {i} one"),
                &format!("filler {i} two"),
                "repeated mid-page table header row", // index 3: mid-page
                &format!("filler {i} three"),
                &format!("filler {i} four"),
                &format!("tail {i} line"),
            ]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 0);
        assert!(a.text.contains("repeated mid-page table header row"));

        // <3 pages: nothing is removed even at 100% repetition
        let two = [
            page(&["© 2026 vectorfs", "body one"]),
            page(&["© 2026 vectorfs", "body two"]),
        ];
        let refs: Vec<&str> = two.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 0);
        assert_eq!(a.text.matches("© 2026 vectorfs").count(), 2);

        // running chapter titles: 3 consecutive pages of 20 (15% < 20%) ⇒
        // streak branch ⇒ DEMOTED to Caption, text kept; the front matter
        // strap on the remaining 14 pages (70% ≥ 20%) IS removed — both
        // branches of the repetition evidence in one document
        let mut pages = Vec::new();
        for i in 0..20 {
            let title = if (2..5).contains(&i) {
                "第三章 强制断窗"
            } else if (5..8).contains(&i) {
                "第四章 单调性证据"
            } else {
                "序章导览" // 14 of 20 pages — the global-furniture arm
            };
            pages.push(page(&[title, &format!("chapter {i} body line")]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 14, "only the 序章导览 strap");
        assert!(!a.text.contains("序章导览"));
        assert_eq!(a.text.matches("第三章 强制断窗").count(), 3, "kept");
        let captions: Vec<&str> = a
            .labels
            .iter()
            .filter(|s| s.kind == ChunkKind::Caption)
            .map(|s| &a.text[s.range.clone()])
            .collect();
        assert_eq!(captions.len(), 6, "3 pages x 2 running titles");
        assert!(
            captions
                .iter()
                .all(|t| *t == "第三章 强制断窗" || *t == "第四章 单调性证据")
        );

        // digit normalization: EDGE digit runs strip (identity survives
        // page numbers), MIDDLE digits stay (lines stay distinct)
        let mut pages = Vec::new();
        for i in 0..5 {
            pages.push(page(&[
                // middle digit differs per page — nothing is removed
                &format!("golden radar page {i} of 5"),
                &format!("prose {i} unique"),
            ]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 0);
        assert!(a.text.contains("golden radar page 0 of 5"));

        let mut pages = Vec::new();
        for i in 0..5 {
            pages.push(page(&[
                // trailing digits strip → identical key on every page
                &format!("vectorfs monthly 2026-{i:02}"),
                &format!("prose {i} unique"),
            ]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        let a = assemble(&refs);
        assert_eq!(a.furniture_removed, 5);
        assert!(!a.text.contains("vectorfs monthly"));
    }

    // ---- assemble: page table + TOC spans (byte-anchored) ----

    #[test]
    fn assemble_records_page_offsets_and_toc_spans() {
        // entries point INSIDE this 3-page document (the v2 range gate:
        // a TOC's numbers are bounded by its own document's page count)
        let toc_page = page(&["目录", "第一章 绪论 1", "第二章 方法 2", "第三章 结果 3"]);
        let body1 = page(&["body one line", "body one more"]);
        let body2 = page(&["body two line"]);
        let a = assemble(&[toc_page.as_str(), body1.as_str(), body2.as_str()]);
        // page table: 1-based, byte-exact starts, strictly increasing
        assert_eq!(a.pages.len(), 3);
        assert_eq!(a.pages[0], (1, 0));
        assert!(a.pages[1].1 > 0 && a.pages[2].1 > a.pages[1].1);
        // the offsets really point at the pages (对拍 via slicing)
        assert_eq!(&a.text[a.pages[1].1..a.pages[1].1 + 4], "body");
        assert_eq!(&a.text[a.pages[2].1..a.pages[2].1 + 4], "body");
        // one merged TOC span over the three hit lines (contiguous)
        assert_eq!(a.labels.len(), 1);
        let span = &a.labels[0];
        assert_eq!(span.kind, ChunkKind::Toc);
        assert_eq!(
            &a.text[span.range.clone()],
            "第一章 绪论 1\n第二章 方法 2\n第三章 结果 3"
        );
        // CJK byte anchoring: span offsets sit on char boundaries (the
        // slice above already proves it — no panic, exact text)
        // R2 CCH: the SAME hits upgraded into the chapter map — target
        // pages from the column, titles from the title side (leaders and
        // the page number gone, CJK preserved verbatim)
        assert_eq!(
            a.sections,
            vec![
                (1u32, "第一章 绪论".to_string()),
                (2, "第二章 方法".to_string()),
                (3, "第三章 结果".to_string()),
            ]
        );
        // determinism: same inputs, same product
        let again = assemble(&[toc_page.as_str(), body1.as_str(), body2.as_str()]);
        assert_eq!(again.text, a.text);
        assert_eq!(again.labels, a.labels);
        assert_eq!(again.pages, a.pages);
        assert_eq!(again.sections, a.sections);
    }

    // ---- R2 CCH: the chapter map (TOC hits → 页码→标题) ----

    /// The section map's build rules, pinned: entries accumulate in
    /// APPEARANCE order across every judged page (a TOC split over two
    /// pages keeps its listing order); titles drop leaders/indent/page
    /// numbers (`绪论 .................... 12` ⇒ `绪论`); repeated target
    /// pages are legal; a no-TOC document maps to EMPTY (the
    /// filename-only degradation the context-header axis takes).
    #[test]
    fn section_map_accumulates_judged_entries_in_appearance_order() {
        // a two-page TOC continuation + a body page, entries pointing
        // inside the 5-page document (the v2 range gate's bound)
        let toc1 = page(&[
            "目录上篇",
            "绪论 .................... 1",
            "    1.1 研究背景 2", // indentation is not part of the title
        ]);
        let toc2 = page(&[
            "目录下篇", // a non-hit line: ratio still ≥50% (2/3)
            "第二章 方法 2",
            "附录 5",
        ]);
        // body pages carry per-page-unique lines (a repeated line on ≥20%
        // of pages would be furniture — irrelevant to the map, but the
        // fixture stays honest about the whole assembly)
        let bodies: Vec<String> = (3..=5)
            .map(|p| page(&[&format!("正文第 {p} 页一行")]))
            .collect();
        let body_refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
        let a = assemble(&[
            toc1.as_str(),
            toc2.as_str(),
            body_refs[0],
            body_refs[1],
            body_refs[2],
        ]);
        assert_eq!(
            a.sections,
            vec![
                (1u32, "绪论".to_string()),
                (2, "1.1 研究背景".to_string()),
                (2, "第二章 方法".to_string()), // repeated target: legal
                (5, "附录".to_string()),
            ],
            "appearance order across judged pages; leaders/indent stripped"
        );
        // a document with NO judged TOC page: empty map, the degradation
        let mut pages = Vec::new();
        for i in 0..4 {
            pages.push(page(&[&format!("正文第 {} 行", i + 1)]));
        }
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        assert!(assemble(&refs).sections.is_empty());
    }

    #[test]
    fn assemble_normalizes_crlf_and_keeps_short_docs_verbatim() {
        // \r\n is the pdfium line separator; structure assembly normalizes
        let raw = "line one\r\nline two";
        let a = assemble(&[raw]);
        assert_eq!(a.text, "line one\nline two");
        assert_eq!(a.pages, vec![(1, 0)]);
        assert!(a.labels.is_empty());
        assert_eq!(a.furniture_removed, 0);
        // a lone \r also normalizes (defensive; PDFs use \r\n)
        assert_eq!(assemble(&["a\rb"]).text, "a\nb");
    }

    // ---- the markdown path (pdf-inspector) ----

    #[test]
    fn markdown_labels_judges_toc_runs_under_the_v2_gates() {
        const TOTAL: u32 = 46;
        // the probed crate shape: entries preserved as plain lines —
        // leader and no-leader mixed; a heading title rides along inertly
        let md = "Contents\n\
                  \n\
                  Introduction .................... 1\n\
                  Methods and materials 3\n\
                  Results overview ................ 3\n\
                  \n\
                  Chapter head one\n\
                  \n\
                  body prose follows\n";
        let labels = markdown_labels(md, TOTAL);
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert_eq!(labels[0].kind, ChunkKind::Toc);
        assert_eq!(
            &md[labels[0].range.clone()],
            "Introduction .................... 1\nMethods and materials 3\n\
             Results overview ................ 3"
        );
        // the v2 HEADLINE: a TITLELESS run judges on its own evidence —
        // the radar shape the v1 title arming rejected wholesale
        let md = "Introduction .... 1\nMethods and materials 3\n";
        let labels = markdown_labels(md, TOTAL);
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert_eq!(labels[0].kind, ChunkKind::Toc);
        // a single stray hit line is not a TOC (run ≥ 2)
        let md = "one stray shaped line 42\n";
        assert!(markdown_labels(md, TOTAL).is_empty());
        // non-monotonic run (the price-page negative): column NOT
        // monotonic ⇒ rejected
        let md = "gadget alpha 5999\ngadget bravo 129\ngadget charlie 4499\n";
        assert!(markdown_labels(md, TOTAL).is_empty());
        // RANGE gate (v2): a SORTED run whose numbers overflow the page
        // count is rejected by the bound alone
        let md = "gadget alpha 129\ngadget bravo 899\ngadget charlie 5999\n";
        assert!(markdown_labels(md, TOTAL).is_empty());
        // ...and the same run inside a document large enough to hold
        // those numbers is the declared narrow residual: judged
        let labels = markdown_labels(md, 6_000);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].kind, ChunkKind::Toc);
    }

    #[test]
    fn markdown_labels_captions_the_prefix_families() {
        // the mirrored crate evidence: figure/table/source/note prefixes
        let md = "## Results\n\nbody line one\n\nFigure 3.2 Accuracy across phases\n\n\
                  body line two\n\nTable 1 (ablation) results\n\nSource: internal bench\n\n\
                  note in lowercase body\n";
        let labels = markdown_labels(md, 46);
        let caption_texts: Vec<&str> = labels.iter().map(|s| &md[s.range.clone()]).collect();
        assert!(labels.iter().all(|s| s.kind == ChunkKind::Caption));
        assert!(caption_texts.contains(&"Figure 3.2 Accuracy across phases"));
        assert!(caption_texts.contains(&"Table 1 (ablation) results"));
        assert!(caption_texts.contains(&"Source: internal bench"));
        // "Table of Contents" (no digit after the prefix) is NOT a caption
        // — the crate's own distinction, kept verbatim
        let labels = markdown_labels("Table of Contents\n", 46);
        assert!(labels.is_empty());
        // a table-of-tables TOC entry wins over the caption reading (scan
        // order encodes the priority: TOC first, caption skips covered)
        let md = "Table 3 断窗机制 12\nTable 4 页偏移表 15\n";
        let labels = markdown_labels(md, 46);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].kind, ChunkKind::Toc);
        assert_eq!(
            &md[labels[0].range.clone()],
            "Table 3 断窗机制 12\nTable 4 页偏移表 15"
        );
    }
}
