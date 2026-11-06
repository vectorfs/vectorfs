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
//!         or the arithmetic fallback s_i + size.
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

/// Char-index ranges of the exact-overlap windows over a `char_len`-long
/// text, preferring `cuts` (ascending CHAR indices — the oracle's chunk
/// ends) inside the budget. See the module doc for the invariant proofs.
pub(super) fn ranges(
    size: usize,
    overlap: usize,
    char_len: usize,
    cuts: &[usize],
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut s = 0usize;
    loop {
        if char_len - s <= size {
            // everything left fits one window — the tail window. Its
            // length is > overlap whenever a window preceded it (the
            // previous end left >= 1 char uncovered), satisfying the
            // contract's last-window rule.
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
        out.push((s, end));
        s = end - overlap;
    }
}
