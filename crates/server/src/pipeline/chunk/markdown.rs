//! `markdown` chunker kind (M3-T6, the chunker axis's second kind):
//! heading-boundary-first windowing over markdown text, exact char overlap.
//!
//! # Same discipline as token-window, different oracle
//!
//! The kind consumes text-splitter's `MarkdownSplitter` (the crate's
//! `markdown` feature — pulldown-cmark 0.13, the exact dependency the md
//! extractor already declares; enabling it added ZERO new crates) as a
//! CUT-POINT ORACLE only: MarkdownSplitter's semantic hierarchy tops out at
//! heading levels (H1..H6 are split positions, headings attach to the
//! FOLLOWING chunk), then paragraphs/blocks, then the generic prose levels.
//! Windows therefore prefer to end just before a heading — a chunk boundary
//! at a section edge — and headings stay attached to their section's
//! content.
//!
//! The OVERLAP is not the library's: exactly as token-window (and per its
//! module docs), the window recurrence is the axis-shared LOCAL
//! implementation ([`super::windowing`]) — exact per-char overlap, lossless
//! reconstruction, CJK-safe slicing, `size` as the ceiling. MarkdownSplitter
//! has no overlap support at all, which only underlines that windowing was
//! never its job.
//!
//! # What "markdown semantics" buys over token-window
//!
//! token-window's oracle ends windows at paragraph/sentence boundaries but
//! knows nothing about `#` headings — on a long markdown document its
//! windows cut wherever the budget runs out, tearing sections mid-body.
//! This kind's cuts place every boundary at a HEADING edge when one fits
//! the budget (the splitter's highest level), so a section's body stays in
//! one window whenever it fits — the retrieval-quality property the e2e
//! asserts (same-heading content in one chunk, different headings never
//! sharing a chunk when a heading boundary fits the budget).
//!
//! # Inputs are ALREADY plain text
//!
//! The md extractor emits plain text (`#` headings stripped — see
//! extract/md.rs), so heading-aware cuts need the MARKDOWN SOURCE, not the
//! extractor's output. The kind therefore suits pipelines whose extractor
//! preserves markdown (txt-sourced `.md` files routed through a
//! passthrough, or a future md-passthrough kind); on plain prose it
//! degenerates gracefully to token-window's behavior (no headings → no
//! heading cuts → the splitter's lower levels). That is the honest
//! semantics of a CHUNKER knob: the user declares which structure their
//! text carries.

use super::windowing;
use super::{Chunk, Chunker, Part, PipelineError};
use text_splitter::MarkdownSplitter;

/// Runtime instance: the window knobs (same shape as token-window — `size`
/// is the char ceiling, `overlap` the exact char overlap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownChunker {
    pub size: usize,
    pub overlap: usize,
}

/// Per-kind config struct (`deny_unknown_fields`; the typed `kind` tag is
/// the allowed-list source and part of the fingerprint).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkdownConfig {
    pub kind: MarkdownTag,
    pub size: usize,
    pub overlap: usize,
}

/// Literal tag: only deserializes from `"markdown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MarkdownTag {
    #[serde(rename = "markdown")]
    Markdown,
}

impl From<&MarkdownChunker> for MarkdownConfig {
    fn from(c: &MarkdownChunker) -> Self {
        MarkdownConfig {
            kind: MarkdownTag::Markdown,
            size: c.size,
            overlap: c.overlap,
        }
    }
}

impl MarkdownChunker {
    /// Programmatic constructor with the same startup validation as
    /// token-window (`0 < overlap < size` — a window with no forward
    /// stride would loop forever; the shared recurrence depends on it).
    pub fn new(size: usize, overlap: usize) -> Result<Self, PipelineError> {
        if size < 1 {
            return Err(PipelineError::config(
                format!("markdown chunker size must be >= 1 (got {size})"),
                None,
            ));
        }
        if overlap == 0 || overlap >= size {
            return Err(PipelineError::config(
                format!(
                    "markdown chunker requires 0 < overlap < size (got size = {size}, overlap = \
                     {overlap}); overlap >= size means zero forward stride"
                ),
                None,
            ));
        }
        Ok(MarkdownChunker { size, overlap })
    }

    /// Semantic cut candidates for one part's text: the CHAR indices where
    /// MarkdownSplitter (char-budgeted to `size`) ends a chunk, ascending —
    /// heading edges are the splitter's highest level, so they are exactly
    /// what its chunk ends prefer (module docs).
    fn semantic_cuts(text: &str, size: usize) -> Vec<usize> {
        MarkdownSplitter::new(size)
            .chunk_char_indices(text)
            .map(|c| c.char_offset + c.chunk.chars().count())
            .collect()
    }
}

impl Chunker for MarkdownChunker {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError> {
        let mut out = Vec::new();
        let mut index: u32 = 0;
        for part in parts {
            // Defensive: the axis feeds Text parts only (image parts become
            // atomic chunks in the axis dispatch); a routing bug must skip,
            // not corrupt.
            if part.modality != super::super::Modality::Text {
                continue;
            }
            if part.text.is_empty() {
                continue;
            }
            let chars: Vec<char> = part.text.chars().collect();
            let cuts = Self::semantic_cuts(&part.text, self.size);
            // T12 label consumption — identical wiring to token-window (the
            // shared windowing seam is the point: both kinds derive forced
            // endpoints and window kinds from the same LabelView, only
            // their cut ORACLE differs)
            let label_view = windowing::LabelView::new(&part.text, &part.labels);
            let forced = label_view.forced_endpoints();
            let ranges = windowing::ranges(self.size, self.overlap, chars.len(), &cuts, &forced);
            // T13 coordinates — identical wiring to token-window (the
            // shared windowing seam is the point: both kinds derive forced
            // endpoints, window kinds AND addressing coordinates from the
            // same tables, only their cut ORACLE differs); the D-7-refined
            // dual channel rides the part's real_lines declaration
            let coords = windowing::Coords::new(&part.text, &part.pages, part.real_lines);
            for (s, e) in ranges {
                let (page, line) = coords.window(s, e);
                out.push(Chunk {
                    index,
                    text: chars[s..e].iter().collect(),
                    image: None,
                    kind: label_view.window_kind(s, e),
                    page,
                    line,
                    section: part.section,
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

    fn chunks_of(c: &MarkdownChunker, text: &str) -> Vec<String> {
        c.chunk(&[Part {
            modality: Modality::Text,
            text: text.to_string(),
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
            real_lines: false,
            furniture_removed: 0,
            section: None,
        }])
        .unwrap()
        .into_iter()
        .map(|ch| ch.text)
        .collect()
    }

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

    /// THE heading-boundary assertion the e2e rides on (unit-level, exact):
    /// under a size that fits one section but not two, every window ends at
    /// a heading edge — content under one heading shares a chunk, content
    /// under different headings never does.
    #[test]
    fn heading_boundaries_win_over_budget_exhaustion() {
        // sections of 40 body chars + heading line, size 64: one section
        // fits, two never do — every cut lands on a heading edge
        let section = |h: usize, name: &str| {
            format!("# Heading {name}\nbody text {h:02} aaaa bbbb cccc dddd\n\n")
        };
        let mut text = String::new();
        text.push_str(&section(0, "one"));
        text.push_str(&section(1, "two"));
        text.push_str(&section(2, "three"));
        let c = MarkdownChunker::new(64, 8).unwrap();
        let w = chunks_of(&c, &text);
        assert!(w.len() >= 3, "three sections cannot share windows: {w:?}");
        // each chunk carries exactly ONE section's heading (heading + its
        // body stay together; the next heading starts the next chunk)
        for (i, win) in w.iter().enumerate() {
            let headings = win.matches("Heading").count();
            assert!(
                headings <= 1,
                "chunk {i} spans {headings} headings: {win:?}"
            );
        }
        // sections never SPLIT: each chunk that starts mid-section (overlap
        // reach-back) still ends before the next heading — the strong claim
        // is the first chunk: it must contain heading one AND its body
        assert!(w[0].contains("Heading one"), "{:?}", w[0]);
        assert!(w[0].contains("body text 00"), "{:?}", w[0]);
        assert!(!w[0].contains("Heading two"), "{:?}", w[0]);
        // and the invariants hold (the axis-shared recurrence)
        assert_eq!(&reconstruct(&w, 8), &text);
    }

    /// No headings ⇒ degenerate to prose behavior: the invariants still
    /// hold end to end (the contract suite sweeps this too).
    #[test]
    fn plain_prose_degenerates_gracefully() {
        let c = MarkdownChunker::new(16, 4).unwrap();
        let text = "word word word\n\npara two here\nitem one\nitem two\nend";
        let w = chunks_of(&c, text);
        assert_eq!(&reconstruct(&w, 4), text);
        assert_eq!(chunks_of(&c, text), w, "determinism");
    }

    #[test]
    fn validation_rejects_degenerate_windows() {
        assert!(MarkdownChunker::new(0, 0).is_err());
        assert!(MarkdownChunker::new(1, 1).is_err());
        assert!(MarkdownChunker::new(512, 0).is_err());
        assert!(MarkdownChunker::new(64, 64).is_err());
        assert!(MarkdownChunker::new(64, 63).is_ok());
    }

    #[test]
    fn cjk_sections_reconstruct_exactly() {
        let c = MarkdownChunker::new(24, 6).unwrap();
        let text = "# 标题一\n\n汉字内容第一段，包含足够多的字符以跨窗口。\n\n# 标题二\n\n第二段内容同样较长，验证重叠窗口下的拼接不变量成立。";
        let w = chunks_of(&c, text);
        assert!(w.len() > 1);
        assert_eq!(reconstruct(&w, 6), text, "no mid-glyph cuts, lossless");
    }

    #[test]
    fn indices_sequential_across_parts_and_empty_inputs() {
        let c = MarkdownChunker::new(16, 4).unwrap();
        let parts = vec![Part::text("# a\nbody one two"), Part::text("# b\nbody")];
        let out = c.chunk(&parts).unwrap();
        let idx: Vec<u32> = out.iter().map(|ch| ch.index).collect();
        assert_eq!(idx, (0..out.len() as u32).collect::<Vec<_>>());
        assert!(c.chunk(&[]).unwrap().is_empty());
        assert!(c.chunk(&[Part::text("")]).unwrap().is_empty());
    }

    /// T13 coordinates through the second kind (the shared windowing seam:
    /// markdown windows carry the SAME addressing table token-window uses,
    /// only the cut oracle differs). A pageless part ⇒ line keys on every
    /// window, re-derived independently from the char-range walk; the
    /// section channel passes through untouched.
    #[test]
    fn line_coordinates_and_section_through_the_shared_seam() {
        let text = "# Heading one\nbody text under one\n\n# Heading two\nbody text under two\n";
        let c = MarkdownChunker::new(24, 6).unwrap();
        let part = Part {
            modality: Modality::Text,
            text: text.to_string(),
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
            real_lines: false,
            furniture_removed: 0,
            section: Some(2),
        };
        let out = c.chunk(&[part]).unwrap();
        assert!(out.len() >= 2, "{out:?}");
        let chars: Vec<char> = text.chars().collect();
        let line_at =
            |x: usize| -> u32 { 1 + chars[..x].iter().filter(|&&c| c == '\n').count() as u32 };
        let mut start = 0usize;
        for ch in &out {
            let len = ch.text.chars().count();
            let (s, e) = (start, start + len);
            assert_eq!(ch.line, Some((line_at(s), line_at(e))), "{ch:?} [{s},{e})");
            assert_eq!(ch.page, None, "pageless part, no page keys (D-7)");
            assert_eq!(ch.section, Some(2), "orchestration channel passes through");
            start = e - 6;
        }
        // the heading-one body sits on line 2 (line 1 is the heading
        // itself): a phrase can be TORN across a cut, so the witness is
        // positional — the window whose char span holds the phrase's first
        // char must report an interval containing line 2
        let pos = chars
            .iter()
            .position(|&c| c == 'u') // "under one"
            .expect("fixture carries the phrase");
        let mut start = 0usize;
        let mut holder: Option<&Chunk> = None;
        for ch in &out {
            let len = ch.text.chars().count();
            if start <= pos && pos < start + len {
                holder = Some(ch);
                break;
            }
            start = start + len - 6;
        }
        let holder = holder.expect("some window covers the heading-one body");
        let (ls, le) = holder.line.unwrap();
        assert!(
            ls <= 2 && 2 <= le,
            "({ls},{le}) must contain line 2: {holder:?}"
        );
    }
}
