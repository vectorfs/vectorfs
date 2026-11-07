//! `token-window` chunker kind: fixed-size sliding window with overlap,
//! M2-T4 edition — `text-splitter` under the hood.
//!
//! # What text-splitter provides, and what it must NOT provide
//!
//! `text-splitter` (0.33, featureless = pure char counting, no tokenizer
//! models — §5.7 "char 计数, 禁 tiktoken 联网特性") recursively splits text at
//! the LARGEST semantic unit that fits a char budget: paragraphs, then
//! sentences, words, graphemes, characters. We use it as a **semantic
//! cut-point oracle**: given the part text, its chunk ends are
//! human-sensible places to end a window.
//!
//! We deliberately do NOT use its `with_overlap`: text-splitter's overlap is
//! separator-level APPROXIMATE (it shrinks or drops the overlap when no
//! valid boundary exists, and trims whitespace), so its raw output violates
//! the T2 chunker contract — exact per-character overlap alignment and the
//! lossless concatenation invariant. The contract is the hard gate
//! (§5.10.2: "拼接不变量(块串接 ≡ 原文, overlap 计入)"), so the overlap itself
//! is implemented LOCALLY, exactly, by slicing the ORIGINAL text — since
//! M3-T6 (the markdown kind, the axis's second) the recurrence lives in the
//! axis-shared [`super::windowing`] module with its invariant proofs; this
//! kind supplies the generic-prose oracle, the markdown kind the
//! heading-aware one.
//!
//! What changed vs the T2 arithmetic windows: windows may now end EARLIER
//! than `s + size` (at a paragraph/sentence/word boundary) and therefore
//! can be shorter than `size`; `size` stays the ceiling and `overlap` stays
//! the exact char overlap — the contract semantics are unchanged, the
//! boundaries are just less likely to tear a sentence in half.

use super::{Chunk, Chunker, Part, PipelineError};
use text_splitter::TextSplitter;

/// Runtime instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenWindowChunker {
    pub size: usize,
    pub overlap: usize,
}

/// Per-kind config struct (`deny_unknown_fields`; the typed `kind` tag is
/// the allowed-list source and part of the fingerprint).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenWindowConfig {
    pub kind: TokenWindowTag,
    pub size: usize,
    pub overlap: usize,
}

/// Literal tag: only deserializes from `"token-window"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TokenWindowTag {
    #[serde(rename = "token-window")]
    TokenWindow,
}

impl From<&TokenWindowChunker> for TokenWindowConfig {
    fn from(c: &TokenWindowChunker) -> Self {
        TokenWindowConfig {
            kind: TokenWindowTag::TokenWindow,
            size: c.size,
            overlap: c.overlap,
        }
    }
}

impl TokenWindowChunker {
    /// Programmatic constructor with the same startup validation as the
    /// config path (`0 < overlap < size`, `size >= 2` implied — a window
    /// with no forward stride would loop forever, one that never overlaps is
    /// legal but almost never intended, so both are loud config errors).
    pub fn new(size: usize, overlap: usize) -> Result<Self, PipelineError> {
        if size < 1 {
            return Err(PipelineError::config(
                format!("token-window size must be >= 1 (got {size})"),
                None,
            ));
        }
        if overlap == 0 || overlap >= size {
            return Err(PipelineError::config(
                format!(
                    "token-window requires 0 < overlap < size (got size = {size}, overlap = \
                     {overlap}); overlap >= size means zero forward stride"
                ),
                None,
            ));
        }
        Ok(TokenWindowChunker { size, overlap })
    }

    /// Semantic cut candidates for one part's text: the CHAR indices where
    /// text-splitter (char-budgeted to `size`) ends a chunk, ascending.
    /// `chunk_char_indices` reports the chunk's own char offset + length, so
    /// a cut is `char_offset + char_len` — a char boundary of the original
    /// by construction, with no byte→char conversion table needed.
    fn semantic_cuts(text: &str, size: usize) -> Vec<usize> {
        TextSplitter::new(size)
            .chunk_char_indices(text)
            .map(|c| c.char_offset + c.chunk.chars().count())
            .collect()
    }
}

impl Chunker for TokenWindowChunker {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError> {
        let mut out = Vec::new();
        let mut index: u32 = 0;
        for part in parts {
            // Defensive: the mechanism feeds Text parts only (image-class
            // parts go to their embedders atomically, §5.10.2); a future
            // routing bug must skip, not corrupt.
            if part.modality != super::super::Modality::Text {
                continue;
            }
            // empty part text -> no windows (a 0-char chunk is not a chunk)
            if part.text.is_empty() {
                continue;
            }
            // Materialize the char sequence ONCE per part: window slicing is
            // then pure indexing — every window is a whole number of chars,
            // CJK glyphs can never be halved.
            let chars: Vec<char> = part.text.chars().collect();
            let cuts = Self::semantic_cuts(&part.text, self.size);
            // T12: consume the part's structural labels — byte-anchored
            // spans convert ONCE to the char domain (LabelView owns the
            // CJK-safe prefix table), non-body span edges become forced
            // window endpoints (D-1 endpoint forcing), and each emitted
            // window carries its kind judgment (pure span ⇒ its kind,
            // crossing/ambiguous ⇒ Body). Empty labels = the pre-T12 path:
            // no forced endpoints, every window Body — byte-identical
            // output modulo the uniformly-defaulted kind field.
            let label_view = super::windowing::LabelView::new(&part.text, &part.labels);
            let forced = label_view.forced_endpoints();
            let ranges =
                super::windowing::ranges(self.size, self.overlap, chars.len(), &cuts, &forced);
            for (s, e) in ranges {
                out.push(Chunk {
                    index,
                    text: chars[s..e].iter().collect(),
                    // text windows never carry the image channel (the axis
                    // dispatch materializes image chunks; see chunk/mod.rs)
                    image: None,
                    kind: label_view.window_kind(s, e),
                });
                index += 1;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Modality;

    fn chunks_of(c: &TokenWindowChunker, text: &str) -> Vec<String> {
        c.chunk(&[Part {
            modality: Modality::Text,
            text: text.to_string(),
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
        }])
        .unwrap()
        .into_iter()
        .map(|ch| ch.text)
        .collect()
    }

    /// The reconstruction identity the contract asserts: de-overlapped
    /// concatenation of consecutive windows == original text. Overlap is
    /// counted in CHARS (byte-slicing would cut mid-glyph on CJK).
    fn reconstruct(windows: &[String], overlap: usize) -> String {
        let mut acc = String::new();
        for (i, w) in windows.iter().enumerate() {
            if i == 0 {
                acc.push_str(w);
            } else {
                acc.extend(w.chars().skip(overlap));
            }
        }
        acc
    }

    #[test]
    fn validation_rejects_degenerate_windows() {
        assert!(TokenWindowChunker::new(0, 0).is_err());
        assert!(TokenWindowChunker::new(1, 1).is_err());
        assert!(TokenWindowChunker::new(512, 0).is_err());
        assert!(TokenWindowChunker::new(64, 64).is_err());
        assert!(TokenWindowChunker::new(64, 63).is_ok());
        assert!(TokenWindowChunker::new(1, 0).is_err()); // overlap must be > 0
    }

    #[test]
    fn edge_shorter_exactly_multiple_longer() {
        // On whitespace-free text the semantic cuts ARE the char-level
        // fallbacks, so the T2 arithmetic edge behavior carries over exactly.
        let c = TokenWindowChunker::new(4, 2).unwrap();
        assert_eq!(chunks_of(&c, "ab"), ["ab"]);
        assert_eq!(chunks_of(&c, "abcd"), ["abcd"]);
        assert_eq!(chunks_of(&c, "abcde"), ["abcd", "cde"]);
        assert_eq!(chunks_of(&c, "abcdefgh"), ["abcd", "cdef", "efgh"]);
        assert_eq!(chunks_of(&c, "abcdefg"), ["abcd", "cdef", "efg"]);
        for text in ["ab", "abcd", "abcde", "abcdefgh", "abcdefg"] {
            let w = chunks_of(&c, text);
            assert_eq!(&reconstruct(&w, 2), text, "text={text:?} windows={w:?}");
        }
    }

    /// Semantic-end preference, demonstrated deterministically: paragraphs
    /// of 34 chars (+ "\n\n") under size 40 make text-splitter cut at every
    /// paragraph end (two paragraphs never fit one 40-char chunk), so window
    /// 0 ends at char 34 — NOT at the arithmetic fallback 40. Later windows
    /// mix semantic ends with arithmetic fallbacks (cuts are 36 apart, so
    /// some (s+overlap, s+size] ranges contain none); every window still
    /// obeys the invariant and the budget.
    #[test]
    fn semantic_ends_preferred_and_invariant_holds_on_prose() {
        let overlap = 8usize;
        let c = TokenWindowChunker::new(40, overlap).unwrap();
        let para = |i: usize| format!("paragraph number {i:02} body text ends");
        let mut text = String::new();
        for i in 0..6 {
            text.push_str(&para(i));
            text.push_str("\n\n");
        }
        assert_eq!(para(0).chars().count(), 34, "fixture self-check");
        let w = chunks_of(&c, &text);
        assert!(w.len() > 1, "sample must span several windows: {w:?}");
        // window 0 ends at the first paragraph boundary, not at char 40
        assert_eq!(
            w[0].chars().count(),
            34,
            "semantic cut must beat the arithmetic fallback: {w:?}"
        );
        assert_eq!(&reconstruct(&w, overlap), &text);
        let mut e = 0usize;
        for (i, win) in w.iter().enumerate() {
            let s = if i == 0 { 0 } else { e - overlap };
            e = s + win.chars().count();
            assert!(win.chars().count() <= 40, "window {i} over budget: {win:?}");
            if i > 0 {
                let prev = &w[i - 1];
                let k = overlap.min(win.chars().count());
                let suffix: String = prev.chars().skip(prev.chars().count() - k).collect();
                let prefix: String = win.chars().take(k).collect();
                assert_eq!(suffix, prefix, "pair {}/{}/", i - 1, i);
                assert!(
                    win.chars().count() > overlap,
                    "window {i} too short: {win:?}"
                );
            }
        }
    }

    /// Property sweep across sizes/overlaps/lengths (superset of the T2
    /// sweep): reconstruction + alignment + budgets on char-only text, and
    /// determinism by double-run.
    #[test]
    fn overlap_zero_forward_stride_impossible() {
        for size in [2usize, 3, 5, 8] {
            for overlap in 1..size {
                let c = TokenWindowChunker::new(size, overlap).unwrap();
                for len in 0..=size * 3 + 2 {
                    let text: String = std::iter::repeat('x').take(len).collect();
                    let w = chunks_of(&c, &text);
                    if len == 0 {
                        assert!(w.is_empty());
                        continue;
                    }
                    assert_eq!(&reconstruct(&w, overlap), &text);
                    for pair in w.windows(2) {
                        let (prev, cur) = (&pair[0], &pair[1]);
                        let k = overlap.min(cur.chars().count());
                        let suffix: String = prev.chars().skip(prev.chars().count() - k).collect();
                        let prefix: String = cur.chars().take(k).collect();
                        assert_eq!(suffix, prefix, "prev={prev:?} cur={cur:?} k={k}");
                        // every window is strictly longer than the overlap
                        // (the contract's prev-suffix and last-window rules)
                        assert!(cur.chars().count() > overlap, "{cur:?}");
                    }
                    assert!(w[0].chars().count() <= size);
                    // determinism
                    assert_eq!(chunks_of(&c, &text), w);
                }
            }
        }
    }

    /// The whitespace-trimming trap that ruled out text-splitter's native
    /// overlap: prose with spaces/newlines between words must still
    /// reconstruct EXACTLY (text-splitter's own chunk output trims those).
    #[test]
    fn whitespace_survives_reconstruction() {
        let c = TokenWindowChunker::new(16, 4).unwrap();
        let text = "word word word\n\npara two here\nitem one\nitem two\nend";
        let w = chunks_of(&c, text);
        assert_eq!(&reconstruct(&w, 4), text);
        // determinism on prose too
        assert_eq!(chunks_of(&c, text), w);
    }

    #[test]
    fn cjk_never_split_mid_glyph() {
        let c = TokenWindowChunker::new(4, 2).unwrap();
        let text = "汉字汉字汉字汉字汉字"; // 10 CJK chars, 3 bytes each
        let w = chunks_of(&c, text);
        assert_eq!(w.len(), 4, "{w:?}");
        for (i, win) in w.iter().enumerate() {
            // every window is whole glyphs and within the char budget
            assert!(win.chars().all(|ch| ch.to_string().bytes().len() == 3));
            assert!(win.chars().count() <= 4, "win {i}: {win:?}");
        }
        assert_eq!(&reconstruct(&w, 2), text);
        // mixed content with multibyte exactly on a boundary
        let mixed = "a汉b汉c汉d汉e汉f";
        let w = chunks_of(&c, mixed);
        assert_eq!(&reconstruct(&w, 2), mixed);
    }

    #[test]
    fn indices_sequential_across_parts() {
        let c = TokenWindowChunker::new(4, 2).unwrap();
        let parts = vec![Part::text("abcdef"), Part::text("ghij")];
        let out = c.chunk(&parts).unwrap();
        let idx: Vec<u32> = out.iter().map(|ch| ch.index).collect();
        assert_eq!(idx, (0..out.len() as u32).collect::<Vec<_>>());
        // "abcdef" (6 chars, stride 2) -> 2 windows; "ghij" -> 1 window
        assert_eq!(out.len(), 3);
        let part1: Vec<String> = out[0..2].iter().map(|c| c.text.clone()).collect();
        assert_eq!(&reconstruct(&part1, 2), "abcdef");
        assert_eq!(out[2].text, "ghij");
    }

    #[test]
    fn empty_inputs_no_chunks() {
        let c = TokenWindowChunker::new(4, 2).unwrap();
        assert!(c.chunk(&[]).unwrap().is_empty());
        assert!(c.chunk(&[Part::text("")]).unwrap().is_empty());
    }

    /// Large-input sanity (the golden corpus pushes this to 5 MiB): the
    /// cut-oracle + slice loop stays linear and the invariant holds end to
    /// end.
    #[test]
    fn large_prose_text_reconstructs() {
        let c = TokenWindowChunker::new(512, 64).unwrap();
        let para = "the quick brown fox jumps over the lazy dog. ".repeat(24); // ~1 KiB
        let text = para.repeat(64); // ~64 KiB
        let w = chunks_of(&c, &text);
        assert!(w.len() > 64);
        assert_eq!(&reconstruct(&w, 64), &text);
    }

    /// T12 label path through the kind, on MULTIBYTE text — the byte→char
    /// conversion duty exercised where it lives (LabelView is unit-tested
    /// in windowing; this pins the wiring: byte-anchored spans over CJK
    /// produce char-domain forced endpoints and per-window kinds, while the
    /// overlap alignment/reconstruction invariants hold unchanged).
    #[test]
    fn labeled_cjk_text_chunks_with_forced_breaks_and_kinds() {
        use crate::pipeline::LabelSpan;
        let c = TokenWindowChunker::new(4, 2).unwrap();
        // 12 CJK chars, 3 bytes each: body run, a toc run in the middle
        // (chars 4..8 = bytes 12..24), legend-like tail (unlabeled)
        let text = "正文正文目录目录图例图例";
        let toc = text.char_indices().nth(4).unwrap().0..text.char_indices().nth(8).unwrap().0;
        assert_eq!(toc, 12..24, "fixture self-check");
        let part = Part {
            modality: Modality::Text,
            text: text.to_string(),
            image: None,
            labels: vec![LabelSpan {
                range: toc,
                kind: crate::pipeline::ChunkKind::Toc,
            }],
            pages: vec![(1, 0), (2, 24)],
        };
        let out = c.chunk(&[part]).unwrap();
        // the invariant battery still holds (forced endpoints never bend it)
        let windows: Vec<String> = out.iter().map(|ch| ch.text.clone()).collect();
        assert_eq!(&reconstruct(&windows, 2), text);
        for pair in windows.windows(2) {
            let (prev, cur) = (&pair[0], &pair[1]);
            let k = 2.min(cur.chars().count());
            let suffix: String = prev.chars().skip(prev.chars().count() - k).collect();
            let prefix: String = cur.chars().take(k).collect();
            assert_eq!(suffix, prefix);
        }
        // forced break at the span edges: some window ends exactly at char
        // 4 and some at char 8 (off the unlabeled stride lattice). Ends are
        // derived chunk-side: each window starts at prev end - overlap.
        let mut ends = Vec::new();
        let mut start = 0usize;
        for ch in &out {
            let len = ch.text.chars().count();
            ends.push(start + len);
            start = start + len - 2;
        }
        assert!(
            ends.contains(&4),
            "span open forces an end: {ends:?} / {out:?}"
        );
        assert!(
            ends.contains(&8),
            "span close forces an end: {ends:?} / {out:?}"
        );
        // kind judgment: a window wholly inside the toc run is Toc; any
        // window crossing an edge (the overlap reach-back) is Body — and
        // pages ride along inertly (consumed by T13, not this kind)
        assert!(
            out.iter()
                .any(|ch| ch.text.chars().all(|x| x == '目' || x == '录')
                    && ch.kind == crate::pipeline::ChunkKind::Toc),
            "pure toc window must exist: {out:?}"
        );
        assert!(
            out.iter().any(|ch| ch.text.contains('目')
                && ch.text.contains('正')
                && ch.kind == crate::pipeline::ChunkKind::Body),
            "edge-crossing window must arbitrate to Body: {out:?}"
        );
    }
}
