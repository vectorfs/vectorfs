//! The axis-shared exact-overlap window recurrence (hoisted from
//! token_window.rs when the markdown kind became the chunker axis's second
//! kind, M3-T6): the WINDOW SEMANTICS are the axis's local guarantee, never
//! outsourced to a library's approximate overlap.
//!
//! Both chunker kinds use a semantic split library (text-splitter) as a
//! CUT-POINT ORACLE only — given the part text, where do human-sensible
//! chunks end? — and then slice the ORIGINAL text themselves:
//!
//! ```text
//! windows are text[s_0..e_0], text[s_1..e_1], … with
//!   s_0 = 0,  e_last = len (full coverage),
//!   s_i = e_(i-1) - overlap  (exact char overlap with the previous window),
//!   e_i = the largest semantic cut in (s_i + overlap, s_i + size]
//!         or the arithmetic fallback s_i + size,
//!         never past the nearest forced endpoint beyond s_i + overlap
//!         (T12 endpoint forcing — see below).
//! ```
//!
//! Given those definitions, with all windows being slices of one original
//! `char` sequence:
//!
//! * **overlap alignment**: the last `overlap` chars of window i-1 are
//!   `text[e_(i-1)-overlap .. e_(i-1)]`, which IS the first `overlap` chars
//!   of window i — byte-for-byte, by construction;
//! * **reconstruction**: `window_0 + Σ chars(overlap..) of window_i`
//!   = the original text — chunking is lossless w.r.t. the extractor's
//!   output, whitespace included (the oracle's trimming never touches us:
//!   only its cut positions are borrowed);
//! * **CJK safety**: slices are taken on `char` boundaries of the original
//!   (the String-equality reconstruction doubles as the no-mid-glyph
//!   proof);
//! * **size budget**: every window ≤ `size` chars; the tail window is
//!   emitted when the remaining text fits one window;
//! * **termination/progress**: every non-final end satisfies
//!   `e > s + overlap`, so the next start `s' = e - overlap > s` strictly
//!   advances (`overlap < size` is startup-validated by each kind).
//!
//! # Forced endpoints (T12, solution D-1's endpoint-forcing adjudication)
//!
//! `forced` carries the CHAR indices where a structural label span (TOC,
//! caption, legend — never body) opens or closes. A window END must not run
//! past such an endpoint, so a labeled segment's content stops leaking into
//! the next window's fresh body; the mandated overlap reach-back keeps
//! crossing freely (bounded pollution, D-1: "TOC 污染上界 = overlap 字符").
//! An empty `forced` slice is the label-free path — byte-identical to the
//! pre-T12 recurrence (the pre-existing tests pin that equivalence).
//!
//! The three branches two rounds of adversarial review exhausted over the
//! boundary B nearest a window in progress, and why the third is the only
//! one that survives:
//!
//! 1. **Block the overlap at B** (next window starts AT B, no reach-back):
//!    the de-overlapped reconstruction then skips `text[B .. B+overlap)` —
//!    unemitted text, a direct violation of the lossless contract (§5.10.2
//!    itself would have to be revised). Rejected.
//! 2. **Naive clamp** (`end = min(candidate, next forced boundary after
//!    s)`): when B lands in `(s, s+overlap]` — which is not a corner case
//!    but the STEADY state after any window that ended exactly ON a
//!    boundary, since its successor starts at `B - overlap` and re-finds B
//!    as its nearest boundary — the stride `e - overlap` is zero or
//!    negative: the walk re-emits the same window forever. Rejected.
//! 3. **Endpoint forcing with frontier consumption** (implemented): the
//!    clamp candidate is the nearest forced endpoint STRICTLY BEYOND the
//!    coverage frontier `s + overlap`. An endpoint at or behind the
//!    frontier has been consumed by an earlier emission (the frontier is
//!    exactly what the previous window covered past `s`); clamping to it
//!    would re-cut covered text into the zero-stride death of branch 2.
//!    Instead the walk SKIPS the degenerate segment: the end lands past B
//!    (into the first valid interval beyond it), leaving the successor's
//!    mandated reach-back as the only crossing — D-1's degenerate-segment
//!    skip, never a clamped zero/negative stride.
//!
//! The four invariants under a non-empty `forced`, proof sketch by branch:
//!
//! * **alignment**: forced endpoints only ever participate in choosing
//!   `e`; the start recurrence `s_i = e_(i-1) - overlap` is untouched, so
//!   the module-level alignment proof applies verbatim;
//! * **reconstruction**: follows from alignment by the module-level proof
//!   (this is also the pin against the "abut at the boundary" shape: a
//!   successor starting AT B instead of `B - overlap` leaves
//!   `text[B .. B+overlap)` unemitted and fails reconstruction);
//! * **progress**: `end = min(candidate, B)` where `candidate ≥
//!   s+overlap+1` (the oracle window) and `B > s+overlap` (frontier
//!   consumption) — both arms strictly exceed `s+overlap`, so `s' =
//!   end - overlap > s` unconditionally, for every `forced` input
//!   (unsorted, duplicated, zero, or past `char_len` entries included:
//!   they are only ever candidates in a min-scan, never stride inputs);
//! * **budget**: `end ≤ candidate ≤ s + size`, and the tail rule emits
//!   `(s, char_len)` only when `char_len - s ≤ size`.

/// The R2 CCH context header of one window (the axis's one prefix rule,
/// shared by both kinds — only their cut ORACLE differs): the
/// heading-path prefix that joins the chunk's EMBEDDING INPUT when the
/// chunker's `context-header = "toc-path"` mode is on.
///
/// Components, joined `" > "`:
///
/// 1. the source FILE NAME STEM (always — the non-PDF sources' only
///    component; `ChunkerKind::chunk_source` passes it);
/// 2. the CHAPTER COMPONENT, by source shape:
///    * a CONTAINER LEAF part (T18, D-9.3) — `sections` empty and the
///      walk stamped a `title` — names its chapter DIRECTLY (the epub
///      nav/NCX/stem chain's label); the component is the title itself,
///      page lookup never applies (container children are pageless);
///    * a flat/PDF part — the NEAREST PRECEDING TOC entry for the
///      window's START page: the last map entry (appearance order) whose
///      target page ≤ the window's start page. A v1 declaration: the
///      path is the FLAT nearest entry, not a reconstructed hierarchy
///      (TOC lines carry no reliable depth signal; indent analysis is
///      out of scope), and windows before the first entry's page — the
///      TOC pages themselves, typically — carry no chapter component at
///      all. The adjudication's optional document-title component is
///      v1-NOT-IMPLEMENTED (see `ChunkCtx`'s declaration: no layout
///      signal ⇒ not obtainable). A paged part inside a titled container
///      keeps this arm (its `sections` is its own document's TOC map —
///      the flat-PDF behavior is the zero-change rule).
///
/// `None` when the mode is off (the pre-R2 path — no prefix ever
/// materializes). The product is the ONLY string the embed axis may
/// prepend; it never touches the chunk's text (the R2 hard boundary).
pub(super) fn context_header(
    mode: super::ContextHeader,
    ctx: &super::ChunkCtx<'_>,
    sections: &[(u32, String)],
    start_page: Option<u32>,
    title: Option<&str>,
) -> Option<String> {
    if mode == super::ContextHeader::None {
        return None;
    }
    let mut components: Vec<&str> = vec![ctx.source_stem];
    if sections.is_empty() {
        // the container-leaf arm (T18): the stamped chapter title IS the
        // chapter component — an untitled container child degrades to the
        // stem-only prefix exactly as before
        if let Some(t) = title {
            components.push(t);
        }
    } else if let Some(page) = start_page {
        // nearest preceding entry: the LAST in-appearance entry whose
        // target page is at/below the window's start page
        if let Some((_, toc_title)) = sections.iter().rfind(|(tp, _)| *tp <= page) {
            components.push(toc_title);
        }
    }
    Some(components.join(" > "))
}

/// Char-index ranges of the exact-overlap windows over a `char_len`-long
/// text, preferring `cuts` (ascending CHAR indices — the oracle's chunk
/// ends) inside the budget and never ending a window past a `forced`
/// endpoint (`forced` need not be sorted or deduped — the clamp is a
/// min-scan; see the module doc for the endpoint adjudication and the
/// invariant proofs).
pub(super) fn ranges(
    size: usize,
    overlap: usize,
    char_len: usize,
    cuts: &[usize],
    forced: &[usize],
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut s = 0usize;
    loop {
        if char_len - s <= size {
            // everything left fits one window — the tail window. Its
            // length is > overlap whenever a window preceded it (the
            // previous end left >= 1 char uncovered), satisfying the
            // contract's last-window rule. A forced endpoint inside the
            // tail stretch is NOT split around: the successor's mandated
            // reach-back would re-cross it anyway (branch 1 of the module
            // doc's adjudication), so the tail window carries the crossing
            // and the kind judgment arbitrates it (ambiguous ⇒ body).
            out.push((s, char_len));
            return out;
        }
        // Non-final end: prefer the largest SEMANTIC cut inside the budget
        // that still leaves more than `overlap` chars of fresh content
        // (see module docs for why each bound matters).
        let hi = s + size;
        let lo = s + overlap + 1;
        let end = cuts
            .iter()
            .rev()
            .find(|&&c| c >= lo && c <= hi)
            .copied()
            .unwrap_or(hi);
        // Endpoint forcing (T12, D-1): clamp to the nearest forced
        // endpoint STRICTLY beyond the coverage frontier s + overlap. An
        // endpoint at/below the frontier is consumed — clamping to it is
        // the zero/negative-stride death of branch 2 — so the walk skips
        // the degenerate segment and ends past it instead. The min-scan
        // (rather than a sorted-prefix assumption) keeps this robust to
        // any caller-side ordering/duplication of the endpoint set.
        let end = match forced.iter().copied().filter(|&b| b > s + overlap).min() {
            Some(b) if b < end => b,
            _ => end,
        };
        out.push((s, end));
        s = end - overlap;
    }
}

// ---------------------------------------------------------------------------
// the T12 label view (byte-anchored labels → char-domain recurrence input)
// ---------------------------------------------------------------------------

/// Byte offset of each char's start (`boundaries[0] = 0` always) — the
/// byte→char conversion base shared by [`LabelView`] (T12 labels) and
/// [`Coords`] (T13 page offsets): extractor annotations are BYTE-anchored,
/// the window recurrence is CHAR-indexed, and the conversion duty is the
/// chunker's (D-1: "字节锚定，chunker 统一换算 char 域——windowing 的 CJK 防线").
fn char_boundaries(text: &str) -> Vec<usize> {
    text.char_indices().map(|(b, _)| b).collect()
}

/// Char index of the char CONTAINING `byte` (largest boundary ≤ byte), the
/// floor direction of the conversion. Never panics: `boundaries[0] = 0`
/// keeps partition_point ≥ 1. Callers clamp to their own char_len.
fn floor_char(boundaries: &[usize], byte: usize) -> usize {
    boundaries.partition_point(|&b| b <= byte) - 1
}

/// One part's structural labels converted ONCE into the CHAR domain the
/// window recurrence works in (T12). [`LabelSpan`] offsets are BYTE offsets
/// into `Part::text`; windows are CHAR indices. The conversion duty is the
/// chunker's by D-1 ("字节锚定，chunker 统一换算 char 域——windowing 的 CJK
/// 防线") and it lives HERE, shared by both kinds, so neither can slice on a
/// raw byte offset. `\n`-anchored spans over ASCII text convert as the
/// identity (single-byte chars); multibyte content converts through the
/// `char_indices` prefix table below — one uniform path, no special cases.
pub(super) struct LabelView {
    /// Char-domain spans, converted and health-checked (empty/inverted
    /// spans dropped). Kinds may include Body — inert for forced endpoints
    /// (body is the default, its edges are not structure) but consulted by
    /// the window-kind judgment, where a Body span containing the window is
    /// the same verdict as the fallback.
    spans: Vec<(std::ops::Range<usize>, super::super::ChunkKind)>,
}

impl LabelView {
    /// Convert `labels` (byte ranges over `text`) to char ranges. Spans are
    /// clamped to the text: a start floors into the containing char, an end
    /// ceils to the next char boundary, so a mid-glyph offset from a
    /// malformed producer degrades to the nearest representable span (never
    /// panics, never splits a glyph). Empty/inverted results are dropped.
    pub(super) fn new(text: &str, labels: &[super::super::LabelSpan]) -> Self {
        if labels.is_empty() {
            // the pre-T12 shape: every window unforced and Body — the fast
            // path every existing source takes until extractor kinds gain
            // label emission
            return LabelView { spans: Vec::new() };
        }
        // Prefix table: `boundaries[i]` = byte offset where char i starts
        // (boundaries[0] = 0 always). byte→char is then one binary search.
        let boundaries = char_boundaries(text);
        let char_len = boundaries.len();
        let ceil_char = |byte: usize| -> usize {
            // char index of the first char STARTING at/after `byte`
            boundaries.partition_point(|&b| b < byte)
        };
        let mut spans = Vec::with_capacity(labels.len());
        for label in labels {
            let start = floor_char(&boundaries, label.range.start).min(char_len);
            let end = ceil_char(label.range.end).min(char_len);
            if start < end {
                spans.push((start..end, label.kind));
            }
        }
        LabelView { spans }
    }

    /// The forced-endpoint set for [`ranges`]: the CHAR indices where
    /// NON-body spans open or close, sorted and deduped. Body spans
    /// contribute nothing — body is the default kind and its boundaries are
    /// not structure (D-1: forced断窗 exists to keep TOC/caption/legend
    /// content out of body windows, not to shard body text).
    pub(super) fn forced_endpoints(&self) -> Vec<usize> {
        let mut ends: Vec<usize> = self
            .spans
            .iter()
            .filter(|(_, k)| *k != super::super::ChunkKind::Body)
            .flat_map(|(r, _)| [r.start, r.end])
            .collect();
        ends.sort_unstable();
        ends.dedup();
        ends
    }

    /// Window-kind judgment (T12): a window that falls ENTIRELY inside one
    /// or more label spans of a SINGLE kind is that kind; anything else —
    /// no containing span (unlabeled), a window crossing a span edge (the
    /// mandated overlap reach-back), or containing spans that disagree
    /// (nested contradictory labels) — is [`ChunkKind::Body`]. The
    /// fallback direction is deliberate: 宁可漏排不可误排 (recall protection,
    /// D-1) — an under-excluded window costs a candidate hit, a
    /// mis-excluded window silently evaporates content from default search.
    pub(super) fn window_kind(&self, s: usize, e: usize) -> super::super::ChunkKind {
        let mut agreed: Option<super::super::ChunkKind> = None;
        for (r, k) in &self.spans {
            if r.start <= s && e <= r.end {
                match agreed {
                    None => agreed = Some(*k),
                    Some(prev) if prev == *k => {}
                    // containing spans disagree — ambiguous, recall-safe
                    // verdict (see doc comment)
                    Some(_) => return super::super::ChunkKind::Body,
                }
            }
        }
        agreed.unwrap_or(super::super::ChunkKind::Body)
    }
}

// ---------------------------------------------------------------------------
// the T13 coordinate table (page offsets / newline lattice, char domain)
// ---------------------------------------------------------------------------

/// One part's ADDRESSING coordinates, precomputed once in the CHAR domain
/// the recurrence works in (T13, ADR 0005): the page interval a window
/// covers (from the extractor's byte-anchored [`super::super::Part::pages`]
/// table), the line interval (from the `'\n'` lattice), or both.
///
/// The **D-7 fork is decided here, per part** (refined 2026-11-09, 尾巴裁决
/// 3): the page/line MUTUAL EXCLUSION is justified only where the part's
/// lines are extraction PSEUDO-structure — a multi-page part whose `'\n'`
/// boundaries are the text-layer join's artifact (pdf-text). A part that
/// carries a page table AND declares its lines REAL structure
/// ([`Part::real_lines`](super::super::Part::real_lines) — pdf-ocr: each
/// single-page part's lines are the recognition engine's own explicit
/// outputs) reports BOTH the page interval and the line interval (页键 +
/// 行键并存). A pageless part reports LINE intervals only (无页概念格式 ⇒
/// 行键), `real_lines` or not — there is no page channel to open.
///
/// **`$exists` discriminator weakened accordingly** (the adjudication's
/// documentation duty): a present page key no longer uniquely identifies
/// a multi-page PDF row — an OCR'd single-page part carries page keys too.
/// Format discrimination should use modality semantics (the source's
/// mime/`x-vf-modality`), not page-key presence.
///
/// Page offsets convert byte→char through the same prefix table as
/// [`LabelView`] (the extractor anchors in bytes, the recurrence indexes
/// chars); newline counting needs no conversion at all — `'\n'` is a
/// single-byte char, so the newline count of any char prefix equals that
/// of its byte prefix (the char/byte isomorphism the T13 design verified,
/// 无陷阱).
///
/// One INCLUSIVE 1-based addressing interval (a page pair or a line pair
/// — both ends are the unit's numbers, never offsets).
type Interval = (u32, u32);

pub(super) enum Coords {
    /// The part carries a page-offset table whose lines are extraction
    /// pseudo-structure (multi-page pdf-text): `(char index where the
    /// page's kept text begins, 1-based page number)`, sorted — by char
    /// index, ties by page number, so the LAST entry at one offset names
    /// the HIGHEST page (an empty page shares its offset with the
    /// successor; a window touching that boundary honestly reports the
    /// higher page). Page keys only (the T13 D-7 fork).
    Paged(Vec<(usize, u32)>),
    /// The part carries a page-offset table AND declared its lines real
    /// structure (`real_lines`, 2026-11-09 裁决 3 — pdf-ocr's single-page
    /// parts): BOTH channels — the page table above plus the `'\n'`
    /// lattice below, so a window reports its page interval AND its line
    /// interval (页键 + 行键并存).
    PagedRealLines(Vec<(usize, u32)>, Vec<usize>),
    /// Pageless part: the char indices of every `'\n'`, ascending.
    Lined(Vec<usize>),
}

impl Coords {
    /// Build the table for one part: `pages` empty ⇒ the Lined fork (D-7,
    /// whatever `real_lines` says — no page channel exists); pages present
    /// ∧ `real_lines` ⇒ the dual channel (裁决 3); pages present ∧
    /// pseudo-structure lines ⇒ page keys only (the T13 fork).
    pub(super) fn new(text: &str, pages: &[(u32, usize)], real_lines: bool) -> Self {
        // the '\n' lattice in the CHAR domain (the recurrence indexes
        // chars; '\n' is single-byte, so counting is byte/char isomorphic,
        // but the STORED positions must be char indices)
        let newlines = || -> Vec<usize> {
            text.chars()
                .enumerate()
                .filter(|&(_, c)| c == '\n')
                .map(|(i, _)| i)
                .collect()
        };
        if pages.is_empty() {
            return Coords::Lined(newlines());
        }
        // page offsets are BYTES, the recurrence is CHARS — convert through
        // the shared prefix table (floor into the containing char: a
        // malformed mid-glyph offset degrades to the nearest representable
        // page start, the LabelView clamping discipline). An offset AT or
        // PAST the text end clamps to char_len — a position no window char
        // reaches, so an empty trailing page never wins a window (a
        // malformed past-the-end table entry is inert, not the owner of the
        // last char). Then sort so the lookups below can binary-search
        // (ties keep the higher page last, by the tuple sort).
        let boundaries = char_boundaries(text);
        let char_len = boundaries.len();
        let mut starts: Vec<(usize, u32)> = pages
            .iter()
            .map(|&(page, byte)| {
                let c = if byte >= text.len() {
                    char_len
                } else {
                    floor_char(&boundaries, byte)
                };
                (c, page)
            })
            .collect();
        starts.sort_unstable();
        if real_lines {
            Coords::PagedRealLines(starts, newlines())
        } else {
            Coords::Paged(starts)
        }
    }

    /// The window `[s, e)`'s coordinates: `(page?, line?)` — both ends
    /// INCLUSIVE in the emitted unit. Exactly one arm is `Some` on the two
    /// single-channel forks (paged-pseudo-structure: page only; pageless:
    /// line only), BOTH on the dual fork (`real_lines`, 裁决 3).
    ///
    /// **Page semantics**: start = the page containing the window's FIRST
    /// char, end = the page containing its LAST char (`e - 1` — a window
    /// ending exactly at the next page's opening offset has not read that
    /// page). A straddling window reports both pages, and the mandated
    /// overlap makes consecutive windows' intervals overlap HONESTLY
    /// (跨窗区间如实重叠, D-7 — no dedup).
    ///
    /// **Line semantics**: `line_at(x) = 1 + '\n' count strictly before x`.
    /// Start = `line_at(s)`; END = `line_at(e)` — the exclusive boundary,
    /// deliberately not `line_at(e-1)`: a window ending in a `'\n'` carries
    /// that line break, and its interval honestly includes the line the
    /// break OPENS (the editor-selection convention; the boundary unit
    /// tests pin both edges).
    pub(super) fn window(&self, s: usize, e: usize) -> (Option<Interval>, Option<Interval>) {
        debug_assert!(e > s, "windows are non-empty by the recurrence");
        // page of char x: the LAST page whose start ≤ x. A malformed table
        // whose first entry starts past x degrades to that first page
        // (saturating idx) — deterministic, never panics; in-tree producers
        // always open at 0.
        let page_at = |starts: &[(usize, u32)], x: usize| -> u32 {
            let idx = starts.partition_point(|&(c, _)| c <= x);
            starts[idx.saturating_sub(1)].1
        };
        let line_at = |newlines: &[usize], x: usize| -> u32 {
            1 + newlines.partition_point(|&p| p < x) as u32
        };
        match self {
            Coords::Paged(starts) => (Some((page_at(starts, s), page_at(starts, e - 1))), None),
            Coords::PagedRealLines(starts, newlines) => (
                Some((page_at(starts, s), page_at(starts, e - 1))),
                Some((line_at(newlines, s), line_at(newlines, e))),
            ),
            Coords::Lined(newlines) => (None, Some((line_at(newlines, s), line_at(newlines, e)))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic printable text of `len` chars (no separators ⇒ the
    /// oracle's cuts are the pure char-level lattice, so window arithmetic
    /// is fully predictable in the traces below).
    fn text_of(len: usize) -> String {
        (0..len)
            .map(|i| char::from_u32(33 + (i % 94) as u32).unwrap())
            .collect()
    }

    /// The four invariants, checked over the returned ranges: alignment
    /// (last `overlap` chars of i-1 == first `overlap` of i), lossless
    /// reconstruction (chunk[0] + Σ chars(overlap..)), budget (≤ size), and
    /// progress (every non-final end > s + overlap; final window ends at
    /// char_len). The shared checker of the three forced-path regressions —
    /// each test below kills a different wrong branch, this keeps all of
    /// them honest about the other invariants.
    fn assert_invariants(windows: &[(usize, usize)], text: &str, size: usize, overlap: usize) {
        assert!(!windows.is_empty());
        let chars: Vec<char> = text.chars().collect();
        let mut recon = String::new();
        for (i, &(s, e)) in windows.iter().enumerate() {
            assert!(e > s, "window {i} empty");
            assert!(e - s <= size, "window {i} over budget: {} > {size}", e - s);
            if i == 0 {
                assert_eq!(s, 0, "first window starts at 0");
                recon.push_str(&chars[s..e].iter().collect::<String>());
            } else {
                let (ps, pe) = windows[i - 1];
                assert_eq!(
                    s,
                    pe - overlap,
                    "window {i} start must be prev end - overlap"
                );
                // alignment: byte-for-byte via the original char sequence
                assert_eq!(
                    chars[pe - overlap..pe],
                    chars[s..s + overlap],
                    "pair {}/{} misaligned",
                    i - 1,
                    i
                );
                // progress (every NON-FINAL end — the final window is never
                // a predecessor): strict stride, the anti-degenerate pin
                assert!(
                    pe > ps + overlap,
                    "progress: non-final end {} must exceed start {} + overlap",
                    pe,
                    ps
                );
                recon.extend(chars[s + overlap..e].iter());
            }
        }
        assert_eq!(windows.last().unwrap().1, chars.len(), "full coverage end");
        assert_eq!(&recon, text, "de-overlapped concatenation != input");
    }

    /// Branch-3 regression (a): the END honors the forced boundary while
    /// the overlap reach-back CROSSES it — alignment intact. A window must
    /// end exactly at B, and its successor must start at B - overlap (not
    /// at B — that is the rejected "block the overlap" branch 1, which
    /// would break both alignment and losslessness).
    #[test]
    fn forced_boundary_bends_the_end_but_not_the_overlap_recursion() {
        let size = 10;
        let overlap = 3;
        let text = text_of(50);
        let chars: Vec<char> = text.chars().collect();
        let b = 14usize; // NOT on the no-forced end lattice (10, 17, 20, …)
        let windows = ranges(size, overlap, chars.len(), &[], &[b]);
        // without forcing, no end lands on b (the contrast that proves the
        // bend comes from `forced`, not the arithmetic)
        let unforced = ranges(size, overlap, chars.len(), &[], &[]);
        assert!(
            !unforced.iter().any(|&(_, e)| e == b),
            "fixture self-check: b must be off the unforced lattice: {unforced:?}"
        );
        assert!(
            windows.iter().any(|&(_, e)| e == b),
            "some window must end exactly at the forced boundary: {windows:?}"
        );
        // the successor of the boundary window starts at b - overlap:
        // the reach-back crosses B (branch 1 rejected), the END does not
        // (branch 2's death avoided), alignment holds
        let at = windows.iter().position(|&(_, e)| e == b).unwrap();
        let (ns, _) = windows[at + 1];
        assert_eq!(ns, b - overlap, "overlap recursion must cross the boundary");
        assert_invariants(&windows, &text, size, overlap);
    }

    /// Branch-3 regression (b): the reconstruction invariant SURVIVES the
    /// forced segment boundary — the anti-「平接」 pin. Two boundaries (a
    /// labeled segment [B1, B2)) bend two ends; the de-overlapped
    /// concatenation must still equal the input exactly. An implementation
    /// that abuts windows at the boundary (successor starting AT B1 with no
    /// reach-back) silently drops text[B1 .. B1+overlap) and fails here.
    #[test]
    fn reconstruction_survives_forced_segment_boundaries() {
        let size = 10;
        let overlap = 4;
        let text = text_of(45);
        let chars: Vec<char> = text.chars().collect();
        let (b1, b2) = (17usize, 31usize); // a labeled segment in the middle
        let windows = ranges(size, overlap, chars.len(), &[], &[b1, b2]);
        assert!(windows.iter().any(|&(_, e)| e == b1), "{windows:?}");
        assert!(windows.iter().any(|&(_, e)| e == b2), "{windows:?}");
        assert_invariants(&windows, &text, size, overlap);
        // and with cuts in play too (the oracle + forcing compose): the
        // D-1 endpoint rule itself, stated exactly — an end may cross a
        // boundary only from BEHIND the coverage frontier (the mandated
        // reach-back); from ahead of it, the end bends to ≤ boundary.
        // Interior semantic cuts inside the segment stay legal (only the
        // segment EDGES are forced); what is forbidden is a fresh end
        // stepping over an unconsumed boundary.
        let cuts = [10, 20, 30, 40];
        let windows = ranges(size, overlap, chars.len(), &cuts, &[b1, b2]);
        assert_invariants(&windows, &text, size, overlap);
        for &(s, e) in &windows {
            for &b in &[b1, b2] {
                assert!(
                    e <= b || b <= s + overlap,
                    "end {e} crossed unconsumed boundary {b} (window start {s}, frontier {}): \
                     {windows:?}",
                    s + overlap
                );
            }
        }
    }

    /// Branch-3 regression (c): the degenerate boundary B ≤ s + overlap
    /// TERMINATES — the anti-dead-loop pin. Two shapes that kill the naive
    /// clamp: a boundary within the FIRST window's overlap reach (B ≤
    /// overlap at s = 0), and a boundary a previous window ended exactly ON
    /// (its successor starts at B - overlap and re-finds B as nearest —
    /// naive clamp strides zero forever). Both must terminate AND keep all
    /// four invariants.
    #[test]
    fn degenerate_boundary_at_or_behind_the_frontier_terminates() {
        let size = 10;
        let overlap = 4;
        // shape 1: B = overlap exactly (the first clamp candidate is at
        // the frontier itself — consumed, not clamped)
        let text = text_of(40);
        let chars: Vec<char> = text.chars().collect();
        for b in [2usize, 4, 5] {
            let windows = ranges(size, overlap, chars.len(), &[], &[b]);
            assert_invariants(&windows, &text, size, overlap);
        }
        // shape 2: the boundary sits exactly on the natural first end
        // (size); window 0 ends ON it, the successor starts at
        // size - overlap and re-finds it — the recorded naive-clamp death.
        // Also stress it with a second boundary just past the first
        // window's arithmetic reach, plus duplicates and an unsorted input
        // (the min-scan must not care).
        for forced in [
            vec![size],
            vec![size, size + 1],
            vec![size, size + overlap],
            vec![0, size, size, size + 2, 40],
            vec![size + 30, size], // unsorted on purpose
        ] {
            let windows = ranges(size, overlap, chars.len(), &[], &forced);
            assert_invariants(&windows, &text, size, overlap);
            // the boundary ON the natural lattice is still honored: some
            // window ends exactly at `size`
            assert!(
                windows.iter().any(|&(_, e)| e == size),
                "lattice boundary must still bend an end: {forced:?} -> {windows:?}"
            );
        }
    }

    // ---- LabelView: byte→char conversion, forced-endpoint derivation,
    // window-kind judgment ----

    use super::super::super::{ChunkKind, LabelSpan};

    fn span(bytes: std::ops::Range<usize>, kind: ChunkKind) -> LabelSpan {
        LabelSpan { range: bytes, kind }
    }

    #[test]
    fn label_view_converts_bytes_to_chars_through_the_prefix_table() {
        // CJK content: every 汉 is 3 bytes, so byte offsets ≠ char offsets
        // except at the very start — the exact trap the conversion exists
        // for (D-1's windowing CJK 防线)
        let text = "ab汉字cd"; // chars: a0 b1 汉2 字3 c4 d5; bytes 0,1,2,5,8,9
        let view = LabelView::new(text, &[span(2..8, ChunkKind::Toc)]); // 汉字
        assert_eq!(view.forced_endpoints(), [2, 4], "char-domain endpoints");
        assert_eq!(view.window_kind(2, 3), ChunkKind::Toc);
        assert_eq!(view.window_kind(2, 4), ChunkKind::Toc);
        assert_eq!(view.window_kind(0, 2), ChunkKind::Body);
        assert_eq!(
            view.window_kind(3, 5),
            ChunkKind::Body,
            "crosses the span edge"
        );
        // a mid-glyph byte offset degrades to the nearest representable
        // span (floor start, ceil end) — never panics, never splits a glyph
        let view = LabelView::new(text, &[span(3..7, ChunkKind::Legend)]);
        assert_eq!(view.forced_endpoints(), [2, 4]);
        // out-of-range offsets clamp to the text bounds; empty/inverted
        // spans are dropped entirely (the inverted end is built from a
        // variable on purpose: a literal reversed range is a clippy deny)
        let inverted_end = 2usize;
        let view = LabelView::new(
            text,
            &[
                span(0..999, ChunkKind::Toc),
                span(4..inverted_end, ChunkKind::Legend),
                span(5..5, ChunkKind::Caption),
            ],
        );
        assert_eq!(view.forced_endpoints(), [0, 6], "clamped to whole text");
        assert_eq!(view.window_kind(0, 6), ChunkKind::Toc);
        // the label-free fast path: no spans, no endpoints, everything Body
        let view = LabelView::new(text, &[]);
        assert!(view.forced_endpoints().is_empty());
        assert_eq!(view.window_kind(0, 6), ChunkKind::Body);
    }

    #[test]
    fn label_view_kind_judgment_and_endpoint_derivation_rules() {
        let text = "0123456789"; // ASCII: byte == char
        // body spans are INERT for forced endpoints (body is the default —
        // its edges are not structure) …
        let view = LabelView::new(text, &[span(2..5, ChunkKind::Body)]);
        assert!(view.forced_endpoints().is_empty());
        // … but a window fully inside a body span judges Body (same as the
        // fallback — one uniform rule, no body special case)
        assert_eq!(view.window_kind(2, 5), ChunkKind::Body);
        // non-body spans contribute BOTH edges, sorted+deduped
        let view = LabelView::new(
            text,
            &[span(2..5, ChunkKind::Toc), span(5..8, ChunkKind::Caption)],
        );
        assert_eq!(view.forced_endpoints(), [2, 5, 8]);
        // the three judgment states: pure span ⇒ its kind; edge-crossing ⇒
        // Body (recall protection); adjacent spans of different kinds both
        // judged pure in their interiors, ambiguous on the shared edge
        assert_eq!(view.window_kind(3, 4), ChunkKind::Toc, "pure toc");
        assert_eq!(view.window_kind(6, 7), ChunkKind::Caption, "pure caption");
        assert_eq!(view.window_kind(1, 3), ChunkKind::Body, "crosses toc edge");
        assert_eq!(
            view.window_kind(4, 6),
            ChunkKind::Body,
            "crosses both edges"
        );
        assert_eq!(view.window_kind(0, 1), ChunkKind::Body, "unlabeled");
        // nested DISAGREEING spans are ambiguous ⇒ Body (the conservative
        // reading of contradictory producer output)
        let view = LabelView::new(
            text,
            &[span(2..8, ChunkKind::Toc), span(4..6, ChunkKind::Legend)],
        );
        assert_eq!(view.forced_endpoints(), [2, 4, 6, 8]);
        assert_eq!(view.window_kind(2, 8), ChunkKind::Toc, "outer containment");
        assert_eq!(
            view.window_kind(4, 6),
            ChunkKind::Body,
            "nested disagreement"
        );
    }

    // ---- Coords: the T13 coordinate table (page intervals, line
    // intervals, the D-7 fork) ----

    #[test]
    fn coords_page_intervals_cover_windows_exactly() {
        // 3 CJK-char pages of 2 chars each (3 bytes/char: page offsets are
        // 0, 6, 12 — byte≠char everywhere except the start, exercising the
        // conversion) + a trailing single-char page
        let text = "一二三四五六七"; // chars 0..6, bytes 0..=18
        let pages = [(1u32, 0usize), (2, 6), (3, 12), (4, 18)];
        let coords = Coords::new(text, &pages, false);
        // window wholly inside page 1: chars [0, 2)
        assert_eq!(coords.window(0, 2), (Some((1, 1)), None));
        // straddling the 1/2 boundary: chars [1, 3) — HONEST overlap, both
        // pages reported (跨窗区间如实重叠)
        assert_eq!(coords.window(1, 3), (Some((1, 2)), None));
        // window ending exactly AT page 3's opening (char 4 = its first
        // char... end-1 = 3, still page 2): [2, 4) covers page 2 only
        assert_eq!(coords.window(2, 4), (Some((2, 2)), None));
        // [3, 5) ends at char 4 = page 3's first char ⇒ (2, 3)
        assert_eq!(coords.window(3, 5), (Some((2, 3)), None));
        // the tail page: [5, 7) — last char 6 is page 4's only char
        assert_eq!(coords.window(5, 7), (Some((3, 4)), None));
        // whole text
        assert_eq!(coords.window(0, 7), (Some((1, 4)), None));
    }

    #[test]
    fn coords_page_table_degeneracies_are_honest_not_panicking() {
        let text = "abcdefghij"; // ASCII: byte == char
        // an EMPTY page shares its offset with the successor (a page whose
        // every line was furniture): the HIGHER page owns the shared
        // offset, and any interval reaching it spans the empty page
        let coords = Coords::new(text, &[(1, 0), (2, 4), (3, 4), (4, 8)], false);
        assert_eq!(coords.window(2, 4), (Some((1, 1)), None));
        assert_eq!(
            coords.window(3, 5),
            (Some((1, 3)), None),
            "reaching the shared offset spans the empty page"
        );
        assert_eq!(
            coords.window(4, 6),
            (Some((3, 3)), None),
            "higher page owns the shared offset"
        );
        assert_eq!(coords.window(4, 9), (Some((3, 4)), None));
        // past-the-end / mid-glyph offsets clamp and floor (never panic,
        // deterministic): a table whose tail offset is past the text end
        // never wins a window (no char maps to it)
        let coords = Coords::new(text, &[(1, 0), (2, 999)], false);
        assert_eq!(coords.window(0, 10), (Some((1, 1)), None));
        // unsorted input with duplicate pages sorts into the same answers
        let a = Coords::new(text, &[(3, 6), (1, 0), (2, 3)], false);
        let b = Coords::new(text, &[(1, 0), (2, 3), (3, 6)], false);
        for (s, e) in [(0, 3), (2, 5), (4, 8), (7, 10)] {
            assert_eq!(a.window(s, e), b.window(s, e), "sorting is internal");
        }
    }

    #[test]
    fn coords_line_intervals_pin_both_newline_boundaries() {
        // chars: a0 b1 \n2 c3 d4 \n5 e6 f7 — three lines
        let text = "ab\ncd\nef";
        let coords = Coords::new(text, &[], false);
        // window exactly one line: [3, 5) = "cd" ⇒ (2, 2)
        assert_eq!(coords.window(3, 5), (None, Some((2, 2))));
        // window OPENING right after a '\n' (the s-at-line-start boundary):
        // start counts only the break BEFORE it — line 2, not 3
        assert_eq!(coords.window(3, 4), (None, Some((2, 2))));
        // window ENDING in a '\n': [3, 6) = "cd\n" ⇒ end = line_at(6) = 3
        // — the window carries the break, its interval includes the line
        // the break OPENS (the pinned end semantics)
        assert_eq!(coords.window(3, 6), (None, Some((2, 3))));
        // first window from the very start
        assert_eq!(coords.window(0, 2), (None, Some((1, 1))));
        // whole text: starts line 1, ends at line_at(8) = 3
        assert_eq!(coords.window(0, 8), (None, Some((1, 3))));
        // no newlines at all: every window is line 1
        let coords = Coords::new("abcdef", &[], false);
        assert_eq!(coords.window(0, 6), (None, Some((1, 1))));
        // CJK: '\n' counting is char/byte isomorphic — 汉字 are 3 bytes
        // each, the '\n' positions in the CHAR domain are what line_at
        // reads (a window's lines are identical under either anchoring)
        let text = "汉字\n第二行\n第三行";
        let coords = Coords::new(text, &[], false);
        assert_eq!(
            coords.window(0, 3),
            (None, Some((1, 2))),
            "ends in the first break"
        );
        assert_eq!(
            coords.window(3, 7),
            (None, Some((2, 3))),
            "carries the second break"
        );
        assert_eq!(
            coords.window(3, 6),
            (None, Some((2, 2))),
            "inside line 2, no break"
        );
        assert_eq!(coords.window(0, text.chars().count()), (None, Some((1, 3))));
    }

    /// The D-7 fork's three states, one per part (refined 2026-11-09,
    /// 裁决 3): a paged part with PSEUDO-structure lines (multi-page
    /// pdf-text) reports page keys and never line keys; a pageless part
    /// reports line keys and never page keys (whatever `real_lines` says
    /// — there is no page channel to open); a paged part that DECLARED its
    /// lines real structure (pdf-ocr's single-page parts) reports BOTH.
    #[test]
    fn coords_d7_fork_three_states_dual_pages_or_lines() {
        // state 1: pages present, lines are extraction pseudo-structure —
        // page keys only, whatever the window shape (the T13 fork, held)
        let paged = Coords::new("page one\n\nmore", &[(1, 0), (2, 10)], false);
        for (s, e) in [(0, 4), (7, 12), (0, 15)] {
            let (page, line) = paged.window(s, e);
            assert!(page.is_some(), "paged part reports pages");
            assert_eq!(
                line, None,
                "pseudo-structure lines: NO line keys (D-7): ({s},{e})"
            );
        }
        // state 2: pageless — line keys only, real_lines irrelevant
        for real_lines in [false, true] {
            let pageless = Coords::new("l1\nl2\nl3", &[], real_lines);
            for (s, e) in [(0, 3), (2, 6), (0, 8)] {
                let (page, line) = pageless.window(s, e);
                assert_eq!(page, None, "pageless part reports NO pages (D-7)");
                assert!(line.is_some(), "pageless part reports lines");
            }
        }
        // state 3: pages present AND real_lines — BOTH channels on every
        // window (页键+行键并存, the pdf-ocr shape: one part IS one page, its
        // lines the recognition engine's own outputs)
        let ocr_page = Coords::new("line one\nline two\nline three", &[(7, 0)], true);
        for (s, e) in [(0, 8), (9, 17), (0, 26)] {
            let (page, line) = ocr_page.window(s, e);
            let (p, l) = (page.expect("dual: page key"), line.expect("dual: line key"));
            assert_eq!(p, (7, 7), "the one-page part's page interval: ({s},{e})");
            assert!(
                l.0 >= 1 && l.0 <= l.1 && l.1 <= 3,
                "sane line interval {l:?}"
            );
        }
        // the line channel keeps the pinned boundary semantics on the dual
        // fork too: [0,9) = "line one\n" ends IN the break ⇒ interval
        // (1,2) — carries the line the break OPENS
        assert_eq!(ocr_page.window(0, 9), (Some((7, 7)), Some((1, 2))));
        // [9,17) = "line two" ⇒ (2,2); the whole part ⇒ (1,3)
        assert_eq!(ocr_page.window(9, 17), (Some((7, 7)), Some((2, 2))));
        assert_eq!(ocr_page.window(0, 26), (Some((7, 7)), Some((1, 3))));
        // a MULTI-page real_lines part (not a producer today, but the
        // channel is per WINDOW): straddling pages report both pages AND
        // the honest line interval — chars a0 b1 \n2 c3 d4 \n5 e6 f7,
        // page 2 opens at char 4
        let dual = Coords::new("ab\ncd\nef", &[(1, 0), (2, 4)], true);
        assert_eq!(dual.window(0, 8), (Some((1, 2)), Some((1, 3))));
        assert_eq!(dual.window(0, 2), (Some((1, 1)), Some((1, 1))));
        assert_eq!(dual.window(3, 5), (Some((1, 2)), Some((2, 2))));
    }
}
