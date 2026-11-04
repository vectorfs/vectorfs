//! `token-window` chunker kind: fixed-size sliding window with overlap.
//!
//! M2 implementation = **char-count window** (UTF-8 safe by construction:
//! windows are cut on `char` boundaries, so a CJK glyph is never halved).
//! T4 swaps the counter for `text-splitter` (real tokenization); the
//! [`Chunker`] interface and the overlap contract below are unchanged —
//! that is the whole point of the axis.

use super::{Chunk, Chunker, Part, PipelineError};

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
    /// config path (`0 < overlap < size`, `size >= 1` — a window with no
    /// forward stride would loop forever, one that never overlaps is legal
    /// but almost never intended, so both are loud config errors).
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

    /// Window starts: `0, stride, 2*stride, …` stopping after a window has
    /// covered the text tail (the final window may be short, but is ALWAYS
    /// longer than `overlap` — see the reconstruction proof in the tests).
    fn windows(&self, text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let stride = self.size - self.overlap;
        let mut out = Vec::new();
        let mut start = 0usize;
        loop {
            let end = (start + self.size).min(chars.len());
            out.push(chars[start..end].iter().collect());
            if start + self.size >= chars.len() {
                break;
            }
            start += stride;
        }
        out
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
            for text in self.windows(&part.text) {
                out.push(Chunk { index, text });
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
        let c = TokenWindowChunker::new(4, 2).unwrap();
        // shorter than one window -> single chunk
        assert_eq!(chunks_of(&c, "ab"), ["ab"]);
        // exactly one window
        assert_eq!(chunks_of(&c, "abcd"), ["abcd"]);
        // one past a window
        assert_eq!(chunks_of(&c, "abcde"), ["abcd", "cde"]);
        // exact multiple of the stride from a full window (len 8)
        assert_eq!(chunks_of(&c, "abcdefgh"), ["abcd", "cdef", "efgh"]);
        // len 7: tail window is short but > overlap
        assert_eq!(chunks_of(&c, "abcdefg"), ["abcd", "cdef", "efg"]);
        // reconstruction holds on every edge
        for text in ["ab", "abcd", "abcde", "abcdefgh", "abcdefg"] {
            let w = chunks_of(&c, text);
            assert_eq!(&reconstruct(&w, 2), text, "text={text:?} windows={w:?}");
        }
    }

    #[test]
    fn overlap_zero_forward_stride_impossible() {
        // stride = size - overlap > 0 always => windows advance; and every
        // window except the first-full ones ends at size. Quick property
        // sweep: reconstruction holds for many lengths/params.
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
                    // suffix/prefix alignment (the overlap actually
                    // overlaps), in chars not bytes
                    for pair in w.windows(2) {
                        let (prev, cur) = (&pair[0], &pair[1]);
                        let k = overlap.min(cur.chars().count());
                        let suffix: String = prev.chars().skip(prev.chars().count() - k).collect();
                        let prefix: String = cur.chars().take(k).collect();
                        assert_eq!(suffix, prefix, "prev={prev:?} cur={cur:?} k={k}");
                        // final window is always > overlap long (proof in
                        // mod docs: len - start > size - stride = overlap)
                        assert!(cur.chars().count() > overlap, "{cur:?}");
                    }
                    assert!(w[0].chars().count() <= size);
                }
            }
        }
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
}
