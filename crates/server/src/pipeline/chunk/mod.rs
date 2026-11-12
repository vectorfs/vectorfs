//! Chunker axis: closed-enum registration + dispatch (M2 T2; the axis's
//! registration mechanism completed in M3-T6 when `markdown` became the
//! SECOND kind — the availability string, the label table, `kind_label`
//! and `from_config` are four views of one registry, parity-pinned, the
//! same anchor discipline the extractor/embedder axes already had).
//!
//! Available kinds: `token-window` (M2), `markdown` (M3-T6 —
//! heading-boundary-first cuts through text-splitter's MarkdownSplitter,
//! same exact-overlap window recurrence via the shared [`windowing`]
//! module).
//!
//! # Modality dispatch lives HERE, in the axis (§5.10.2, M3-T1)
//!
//! The chunker axis serves TEXT only; image parts become ATOMIC chunks —
//! one chunk per image part, carrying the payload — and never reach a kind.
//! Both rules are mechanism, not kind policy, so they live in the
//! [`Chunker`](super::Chunker) impl on [`ChunkerKind`]: every kind gets the
//! bypass for free, and the global chunk-index sequence (text windows and
//! image atoms share ONE counter across all parts) is assigned in one
//! place.
//!
//! # The context-header axis entry (R2 CCH, recall roadmap)
//!
//! The ingest worker enters through [`ChunkerKind::chunk_source`], the
//! SOURCE-aware form of the same walk: when the chunker's
//! `context-header = "toc-path"` knob is on, every TEXT window carries a
//! heading-path [`Chunk::embed_prefix`] (filename stem > document title >
//! nearest preceding TOC chapter for the window's start page —
//! [`windowing::context_header`]) computed from the part's TOC-derived
//! `sections` map. The prefix feeds the EMBEDDING INPUT ONLY (see
//! [`Chunk::embed_prefix`]'s hard boundary); the plain trait path and the
//! knob's default `none` never emit one.

pub mod markdown;
pub mod token_window;
pub mod windowing;

use super::{Chunk, Chunker, Modality, Part, PipelineError};
use markdown::{MarkdownChunker, MarkdownConfig};
use token_window::{TokenWindowChunker, TokenWindowConfig};

/// The available-kind list embedded in unknown-kind errors. Derived view of
/// [`KIND_LABELS`] (the parity test pins the two together — a new kind that
/// updates the enum but forgets the string fails there, not in a user's
/// error message).
pub const CHUNKER_KINDS: &str = "token-window, markdown";

/// Every kind's config label, IN AVAILABILITY-LIST ORDER — the axis twin of
/// the extractor axis's identity anchor: `kind_label` (the exhaustive match
/// the compiler walks on a new variant) must agree with this table row for
/// row (the `kind_registry_views_agree` test walks it). A new kind lands
/// here, in `kind_label`, and in `from_config` in one diff — everything
/// else in this module is exhaustive dispatch the compiler already checks.
pub const KIND_LABELS: &[&str] = &["token-window", "markdown"];

/// Registered chunker kinds (same registration pattern as
/// [`crate::pipeline::extract::ExtractorKind`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkerKind {
    TokenWindow(TokenWindowChunker),
    Markdown(MarkdownChunker),
}

/// The `context-header` knob's closed vocabulary (R2 CCH, recall roadmap):
/// whether the chunker stamps each window's embedding input with a
/// heading-path prefix (`文件名（去扩展）> 文档标题（可得则用）> 章节路径` —
/// [`windowing::context_header`]). Config form is the chunk table's
/// `context-header = "toc-path" | "none"` (kebab-case like every ingest
/// knob).
///
/// **This knob is chunk-identity-relevant and ENTERS the pipeline
/// fingerprint** (the `structure` knob precedent): turning it on changes
/// the EMBEDDING INPUT, hence the vectors, hence every chunk's identity in
/// the index — enabling it must demand the explicit re-embed of a
/// fingerprint flip. The default `none` keeps pre-R2 fingerprints
/// byte-stable (elided at the default, the charset three-state rule).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextHeader {
    /// No prefix — every chunk embeds its text alone (the pre-R2 path,
    /// byte-identical).
    #[default]
    #[serde(rename = "none")]
    None,
    /// Prefix from the TOC-derived chapter map: filename stem + document
    /// title (first-line heuristic, when short enough) + the nearest
    /// preceding TOC entry for the window's start page. Parts without a
    /// map (non-PDF sources, `structure = false`) degrade to
    /// filename-only prefixes.
    #[serde(rename = "toc-path")]
    TocPath,
}

impl ContextHeader {
    /// `true` at the default — the fingerprint elides the field there (the
    /// charset/fts-tokenizer three-state precedent: identical behavior
    /// must fingerprint identically, and enabling the header genuinely
    /// moves it).
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, ContextHeader::None)
    }
}

/// The per-SOURCE chunking context the ingest worker hands the axis
/// (R2 CCH): the source identity the header's filename component needs.
/// Plain borrowed data; the kinds read it, never own it.
///
/// v1 carries the STEM only. The adjudication's optional document-title
/// component (首行或卷号行，可得则用) is DELIBERATELY NOT implemented:
/// pdfium's `.all()` text exposes no font-size/layout signal, and the
/// first-line heuristic measures as noise on the evidence corpora (the
/// D-4 fixture's first kept line is a 2-char ring word; a txt source's
/// first line is usually prose) — not obtainable without layout, so not
/// attempted (如实声明).
#[derive(Debug, Clone, Copy)]
pub struct ChunkCtx<'a> {
    /// The source's FILE NAME without extension (e.g. `vol32` for
    /// `radar/vol32.pdf`) — the header's always-present first component.
    pub source_stem: &'a str,
}

impl ChunkerKind {
    /// Two-stage config parse. `section` is the config path for error
    /// context — chunkers are configured INLINE in an ingest section, so it
    /// reads e.g. `ingest.docs.chunk`; `line` is the ingest section's TOML
    /// line (per-kind values are detached from the document by then; §5.10.4
    /// attributes stage-2 errors to the owning section).
    pub fn from_config(
        section: &str,
        raw: &toml::Value,
        line: Option<usize>,
    ) -> Result<Self, PipelineError> {
        let kind = super::config::probe_kind(raw)
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;
        match kind.as_str() {
            "token-window" => {
                let cfg: TokenWindowConfig =
                    raw.clone().try_into().map_err(|e: toml::de::Error| {
                        super::config::stage2_error(section, Some(&kind), e.message(), line)
                    })?;
                // window sanity is STARTUP validation (§5.10.3), reported at
                // the config line that carries the bad numbers
                TokenWindowChunker::new(cfg.size, cfg.overlap)
                    .map(|c| c.with_context_header(cfg.context_header))
                    .map(ChunkerKind::TokenWindow)
                    .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))
            }
            "markdown" => {
                let cfg: MarkdownConfig =
                    raw.clone().try_into().map_err(|e: toml::de::Error| {
                        super::config::stage2_error(section, Some(&kind), e.message(), line)
                    })?;
                // same window sanity, same startup discipline (the shared
                // recurrence depends on 0 < overlap < size)
                MarkdownChunker::new(cfg.size, cfg.overlap)
                    .map(|c| c.with_context_header(cfg.context_header))
                    .map(ChunkerKind::Markdown)
                    .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))
            }
            other => Err(PipelineError::config(
                format!(
                    "[{section}]: unknown chunker kind '{other}' (available kinds: \
                     {CHUNKER_KINDS})"
                ),
                line,
            )),
        }
    }

    /// The kind's REGISTRY label — the `kind = "…"` config string, one per
    /// variant, exhaustively matched so a new kind MUST land here (the
    /// compiler walks it; [`KIND_LABELS`] and the parity test then force
    /// the table views to agree). Unlike [`Self::describe`] this is knob-free.
    pub fn kind_label(&self) -> &'static str {
        match self {
            ChunkerKind::TokenWindow(_) => "token-window",
            ChunkerKind::Markdown(_) => "markdown",
        }
    }

    /// Human-readable kind + params (CLI summary lines).
    pub fn describe(&self) -> String {
        match self {
            ChunkerKind::TokenWindow(c) => {
                let mut s = format!("token-window(size={}, overlap={})", c.size, c.overlap);
                if c.context_header == ContextHeader::TocPath {
                    s.push_str(", context-header=toc-path");
                }
                s
            }
            ChunkerKind::Markdown(c) => {
                let mut s = format!("markdown(size={}, overlap={})", c.size, c.overlap);
                if c.context_header == ContextHeader::TocPath {
                    s.push_str(", context-header=toc-path");
                }
                s
            }
        }
    }

    /// The SOURCE-aware chunking entry (R2 CCH): the ingest worker's path.
    /// Like the trait's [`Chunker::chunk`] (same modality split, same
    /// global index sequence) but carrying the source identity the
    /// `context-header = "toc-path"` mode needs: when the mode is on and a
    /// stem is given, every TEXT window is stamped with its
    /// [`Chunk::embed_prefix`]; otherwise (mode off, or no source — the
    /// plain trait path and the contract doubles) no prefix ever
    /// materializes — the header's FIRST component is the filename, a
    /// prefix without it would be a different identity than production
    /// ever sees.
    ///
    pub fn chunk_source(
        &self,
        parts: &[Part],
        source_stem: Option<&str>,
    ) -> Result<Vec<Chunk>, PipelineError> {
        let ctx = source_stem.map(|stem| ChunkCtx { source_stem: stem });
        self.dispatch(parts, ctx.as_ref())
    }

    /// The shared modality-split walk (the axis mechanism, parameterized
    /// by the optional context — `None` is the plain trait path).
    fn dispatch(
        &self,
        parts: &[Part],
        ctx: Option<&ChunkCtx<'_>>,
    ) -> Result<Vec<Chunk>, PipelineError> {
        // Walk the parts in order, chunking each maximal run of TEXT parts
        // through the kind (a sub-slice, so a text-only input is ONE call —
        // byte-identical to calling the kind directly) and materializing
        // every image part as ONE atomic chunk at its position. Indices are
        // (re)assigned GLOBALLY here — the "sequential across all parts"
        // invariant is the axis's guarantee now, independent of how any
        // kind numbers its own output.
        let chunk_text_run = |run: &[Part]| -> Result<Vec<Chunk>, PipelineError> {
            match self {
                ChunkerKind::TokenWindow(c) => c.chunk_run(run, ctx),
                ChunkerKind::Markdown(c) => c.chunk_run(run, ctx),
            }
        };
        let mut out: Vec<Chunk> = Vec::new();
        let mut next_index: u32 = 0;
        let mut run_start = 0usize;
        let flush_text_run = |run: &[Part],
                              out: &mut Vec<Chunk>,
                              next_index: &mut u32|
         -> Result<(), PipelineError> {
            for mut ch in chunk_text_run(run)? {
                ch.index = *next_index;
                *next_index += 1;
                out.push(ch);
            }
            Ok(())
        };
        for (i, part) in parts.iter().enumerate() {
            match part.modality {
                Modality::Text => {}
                Modality::Image => {
                    // the payload pairing is a constructor invariant;
                    // a hand-built part without it is a mechanism bug
                    // and must fail visibly, not chunk "nothing"
                    let payload = part.image.clone().ok_or_else(|| {
                        PipelineError::chunk(format!(
                            "part {} declares modality image but carries no image \
                             payload — an image part without bytes cannot be chunked",
                            i
                        ))
                    })?;
                    // bytes-vs-mime honesty at the seam (garbage or a
                    // wrong declaration refuses HERE, with the file
                    // FAILED loudly — never a silent empty-ish chunk)
                    payload.validate()?;
                    flush_text_run(&parts[run_start..i], &mut out, &mut next_index)?;
                    out.push(Chunk {
                        index: next_index,
                        text: String::new(),
                        image: Some(payload),
                        // an image atom carries no text, hence no text
                        // structure to label — Body is the only honest
                        // kind (T12; the modality channel, not ChunkKind,
                        // is what routes image chunks)
                        kind: super::ChunkKind::Body,
                        // T13 addressing: an atom has no text window, so
                        // LINE coordinates are None by construction. PAGE
                        // coordinates would come from the part's page
                        // table — an atom covers every page its table
                        // names (a whole-page render names one) — but NO
                        // image extractor emits the table today (pdf-pages
                        // renders page images without a Part.pages entry;
                        // declared limitation, this path stays None until
                        // one lands). SECTION rides the orchestration
                        // channel like any text part (a container's image
                        // child is still a child).
                        page: part
                            .pages
                            .first()
                            .zip(part.pages.last())
                            .map(|(f, l)| (f.0, l.0)),
                        line: None,
                        section: part.section,
                        // the T18 chapter title rides the orchestration
                        // channel beside the ordinal (a container's image
                        // child is still a child)
                        title: part.title.clone(),
                        // an image atom has no text to prefix (R2 CCH)
                        embed_prefix: None,
                    });
                    next_index += 1;
                    run_start = i + 1;
                }
            }
        }
        flush_text_run(&parts[run_start..], &mut out, &mut next_index)?;
        Ok(out)
    }
}

/// Dispatch: the enum IS the chunker — and the enum layer is where the
/// §5.10.2 modality split happens (see module docs). The plain trait path
/// is the context-FREE form of the shared walk (no source identity ⇒ no
/// context header ever — see [`ChunkerKind::chunk_source`]); the ingest
/// worker enters through the source-aware path.
impl Chunker for ChunkerKind {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError> {
        self.dispatch(parts, None)
    }
}

/// Fingerprint-facing serialization: inner config only (the kind tag is a
/// field of the config struct — kind and knobs enter the fingerprint
/// naturally, no axis-level changes).
impl serde::Serialize for ChunkerKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // serialize the VALIDATED parameters, not the live chunker
            ChunkerKind::TokenWindow(c) => TokenWindowConfig::from(c).serialize(serializer),
            ChunkerKind::Markdown(c) => MarkdownConfig::from(c).serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(size: usize, overlap: usize) -> toml::Value {
        toml::from_str(&format!(
            "kind = \"token-window\"\nsize = {size}\noverlap = {overlap}"
        ))
        .unwrap()
    }

    #[test]
    fn from_config_accepts_valid_window() {
        let k = ChunkerKind::from_config("ingest.d.chunk", &win(512, 64), Some(2)).unwrap();
        assert_eq!(k.describe(), "token-window(size=512, overlap=64)");
        assert!(k.chunk(&[Part::text("hello")]).unwrap().len() == 1);
    }

    #[test]
    fn from_config_rejects_bad_window_with_line() {
        for (size, overlap) in [(64, 64usize), (512, 0), (512, 600), (0, 0)] {
            let err = ChunkerKind::from_config("ingest.d.chunk", &win(size, overlap), Some(5))
                .unwrap_err();
            assert!(
                err.to_string().contains("overlap") || err.to_string().contains("size"),
                "{err}"
            );
            assert!(err.to_string().contains("at line 5"), "{err}");
        }
    }

    #[test]
    fn from_config_unknown_kind() {
        let raw: toml::Value = toml::from_str("kind = \"sentence\"").unwrap();
        let err = ChunkerKind::from_config("ingest.d.chunk", &raw, None).unwrap_err();
        assert!(
            err.to_string().contains(
                "unknown chunker kind 'sentence' (available kinds: token-window, \
                           markdown)"
            ),
            "{err}"
        );
    }

    /// THE parity test (the extractor/embedder axis anchor, landed here with
    /// the axis's second kind): the availability string, the label table,
    /// the exhaustive `kind_label` match and `from_config` are four views of
    /// one registry — a new kind that updates some but not all of them
    /// fails HERE instead of drifting in production error messages.
    #[test]
    fn kind_registry_views_agree() {
        assert_eq!(CHUNKER_KINDS, KIND_LABELS.join(", "));
        for label in KIND_LABELS {
            let raw: toml::Value =
                toml::from_str(&format!("kind = \"{label}\"\nsize = 32\noverlap = 8")).unwrap();
            let k = ChunkerKind::from_config("x", &raw, None).unwrap_or_else(|e| {
                panic!("KIND_LABELS entry {label} must parse: {e}");
            });
            assert_eq!(k.kind_label(), *label);
            // describe carries the label as its head (knobs follow)
            assert!(k.describe().starts_with(label), "{}", k.describe());
        }
        // markdown parses with its own knobs and reports them
        let raw: toml::Value =
            toml::from_str("kind = \"markdown\"\nsize = 256\noverlap = 32").unwrap();
        let k = ChunkerKind::from_config("x", &raw, None).unwrap();
        assert_eq!(k.describe(), "markdown(size=256, overlap=32)");
        // unknown fields rejected per kind
        let raw: toml::Value =
            toml::from_str("kind = \"markdown\"\nsize = 8\noverlap = 2\nsep = \"\\n\"").unwrap();
        let err = ChunkerKind::from_config("ingest.d.chunk", &raw, Some(1)).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown field 'sep' for kind 'markdown'"),
            "{err}"
        );
        // and its window sanity carries the line
        let raw: toml::Value =
            toml::from_str("kind = \"markdown\"\nsize = 8\noverlap = 8").unwrap();
        let err = ChunkerKind::from_config("ingest.d.chunk", &raw, Some(7)).unwrap_err();
        assert!(
            err.to_string()
                .contains("markdown chunker requires 0 < overlap < size"),
            "{err}"
        );
        assert!(err.to_string().contains("at line 7"), "{err}");
    }

    // ---- M3-T1: the axis-level modality split (image atoms, one index
    // sequence across all parts) ----

    fn kind(size: usize, overlap: usize) -> ChunkerKind {
        ChunkerKind::from_config("t", &win(size, overlap), None).unwrap()
    }

    fn png_part(mime: &str) -> Part {
        Part::image(mime, &b"\x89PNG\r\n\x1a\nnot-decoded-here"[..])
    }

    /// A GENUINELY decodable 1x1 red RGBA PNG (every chunk CRC-checked;
    /// IDAT inflates to `00 ff 00 00 ff`). T1's first cut had a broken
    /// deflate stream — only ever magic-sniffed, so nothing caught it
    /// until T2's decoder-bearing tests (fixed per that finding).
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn tiny_png_fixture_genuinely_decodes() {
        // guard: this const is shared fixture material; a broken deflate
        // stream once slipped through because only the magic was sniffed
        // (nothing decoded it). Keep the decode asserted so it can't rot.
        let img = image::load_from_memory(TINY_PNG).expect("TINY_PNG must really decode");
        assert_eq!((img.width(), img.height()), (1, 1));
    }

    #[test]
    fn image_parts_bypass_the_kind_and_take_one_atomic_chunk_each() {
        let k = kind(8, 2);
        // mixed parts: [text, image, image, text] — the image parts occupy
        // their position in the ONE index sequence; text runs chunk through
        // the kind and renumber globally (never restart at 0)
        let parts = vec![
            Part::text("abcdefgh"), // 8 chars, one window
            png_part("image/png"),
            png_part("image/png"),
            Part::text("ijkl"), // tail run
        ];
        let out = k.chunk(&parts).unwrap();
        let idx: Vec<u32> = out.iter().map(|c| c.index).collect();
        assert_eq!(idx, vec![0, 1, 2, 3], "one global sequence: {out:?}");
        assert_eq!(out[0].text, "abcdefgh");
        assert_eq!(out[0].modality(), Modality::Text);
        for ch in &out[1..3] {
            assert_eq!(ch.modality(), Modality::Image);
            assert_eq!(ch.text, "");
            assert_eq!(
                &ch.image.as_ref().unwrap().bytes[..],
                b"\x89PNG\r\n\x1a\nnot-decoded-here"
            );
        }
        assert_eq!(out[3].text, "ijkl");
        // image-only extraction: still chunked (has_extracted_content's
        // image arm keeps such an extraction from yielding), atomic 1:1
        let out = k
            .chunk(&[png_part("image/png"), png_part("image/png")])
            .unwrap();
        assert_eq!(out.iter().map(|c| c.index).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn mixed_indices_stay_sequential_across_multi_window_runs() {
        // text long enough for SEVERAL windows before AND after the image:
        // the post-image run must continue the global counter (a per-run
        // restart would collide chunk keys)
        let k = kind(4, 2);
        let head = "abcdefgh"; // -> windows at 0..=3 chars: [abcd, cdef, efgh]
        let tail = "ijklmn"; // -> [ijkl, klmn]
        let parts = vec![Part::text(head), png_part("image/png"), Part::text(tail)];
        let out = k.chunk(&parts).unwrap();
        let idx: Vec<u32> = out.iter().map(|c| c.index).collect();
        assert_eq!(idx, (0..out.len() as u32).collect::<Vec<_>>(), "{out:?}");
        // head windows, then the image, then tail windows — in part order
        assert_eq!(
            out.iter()
                .map(|c| (c.modality(), c.text.clone()))
                .collect::<Vec<_>>(),
            vec![
                (Modality::Text, "abcd".to_string()),
                (Modality::Text, "cdef".to_string()),
                (Modality::Text, "efgh".to_string()),
                (Modality::Image, String::new()),
                (Modality::Text, "ijkl".to_string()),
                (Modality::Text, "klmn".to_string()),
            ]
        );
        // determinism: same parts, same chunks (byte-for-byte)
        assert_eq!(k.chunk(&parts).unwrap(), out);
    }

    #[test]
    fn text_only_input_is_byte_identical_to_the_kind_alone() {
        // the bypass must be invisible for text-only parts: same windows,
        // same indices, as calling the kind directly
        let k = kind(8, 2);
        let parts = vec![
            Part::text("one two three"),
            Part::text(""),
            Part::text("four"),
        ];
        let via_axis = k.chunk(&parts).unwrap();
        let direct = match &k {
            ChunkerKind::TokenWindow(c) => c.chunk(&parts).unwrap(),
            ChunkerKind::Markdown(_) => unreachable!("kind() builds token-window"),
        };
        assert_eq!(via_axis, direct);
    }

    /// T13: the image atom's addressing — line None BY CONSTRUCTION (no
    /// text window), page None while no image extractor emits a page table
    /// (pdf-pages renders page images without one — declared limitation at
    /// the bypass), and the section channel riding along like any part's
    /// (a container's image child is still a child). A hypothetical future
    /// table names the atom's whole page span.
    #[test]
    fn image_atoms_carry_no_text_coordinates_but_keep_their_section() {
        let k = kind(8, 2);
        let mut part = png_part("image/png");
        part.section = Some(3);
        let out = k.chunk(&[part, Part::text("tail text")]).unwrap();
        assert_eq!(out[0].modality(), Modality::Image);
        assert_eq!(out[0].page, None, "no extractor page table today");
        assert_eq!(out[0].line, None, "an atom has no text window");
        assert_eq!(out[0].section, Some(3), "orchestration channel rides along");
        // the text windows after the atom keep their own (pageless) line keys
        assert_eq!(out[1].page, None);
        assert_eq!(out[1].line, Some((1, 1)));
        // and a future page-bearing image part would name its whole span:
        // one page table entry ⇒ the single page, two ⇒ first..last
        let mut paged = png_part("image/png");
        paged.pages = vec![(7, 0)];
        let out = k.chunk(&[paged]).unwrap();
        assert_eq!(out[0].page, Some((7, 7)));
        let mut two = png_part("image/png");
        two.pages = vec![(2, 0), (3, 0)];
        let out = k.chunk(&[two]).unwrap();
        assert_eq!(out[0].page, Some((2, 3)));
    }

    #[test]
    fn image_part_without_payload_fails_loudly() {
        let k = kind(8, 2);
        let mut part = png_part("image/png");
        part.image = None; // a hand-built inconsistency, not a real state
        let err = k.chunk(&[part]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("declares modality image but carries no image"),
            "{msg}"
        );
        assert!(msg.starts_with("chunk failed"), "{msg}");
    }

    #[test]
    fn bypass_refuses_garbage_bytes_and_wrong_declared_mime() {
        let k = kind(8, 2);
        // garbage magic: named as no-known-magic (NOT as a missing embedder
        // — that remedy would misdirect on a corrupt file)
        let err = k
            .chunk(&[Part::image("image/png", &b"plainly not an image"[..])])
            .unwrap_err();
        assert!(
            err.to_string().contains("no known image-container magic"),
            "{err}"
        );
        // magic present, declared mime disagrees: names both values
        let err = k
            .chunk(&[Part::image("image/jpeg", &b"\x89PNG\r\n\x1a\n"[..])])
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sniff as image/png"), "{msg}");
        assert!(msg.contains("image/jpeg"), "{msg}");
        // the real 1x1 PNG passes its canonical mime
        assert!(k.chunk(&[Part::image("image/png", TINY_PNG)]).is_ok());
    }

    // ---- R2 CCH: the context-header axis (chunk_source + the knob) ----

    fn cch_kind(kind: &str, header: &str) -> ChunkerKind {
        let raw: toml::Value = toml::from_str(&format!(
            "kind = \"{kind}\"\nsize = 8\noverlap = 2\ncontext-header = \"{header}\""
        ))
        .unwrap();
        ChunkerKind::from_config("t", &raw, None).unwrap()
    }

    /// A paged part with a chapter map (the pdf-text structure shape): two
    /// "pages" of body text plus the (page, title) entries a TOC judgment
    /// would have produced. Returns (part, expected page->title lookup).
    fn paged_part_with_map() -> Part {
        // 2 pages x 20 chars (ASCII: byte == char); page 2 opens at char 20
        let mut p = Part::text("aaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbb");
        p.pages = vec![(1, 0), (2, 20)];
        p.sections = vec![
            (1, "序章".to_string()),
            (2, "第二章 方法".to_string()),
            (5, "附录".to_string()),
        ];
        p
    }

    #[test]
    fn context_header_knob_parses_describes_and_fingerprints() {
        // parse: both values, both kinds; unknown value is loud with the menu
        for kind in ["token-window", "markdown"] {
            let on = cch_kind(kind, "toc-path");
            let off = cch_kind(kind, "none");
            assert!(
                matches!(on, ChunkerKind::TokenWindow(c) if c.context_header == ContextHeader::TocPath)
                    || matches!(on, ChunkerKind::Markdown(c) if c.context_header == ContextHeader::TocPath)
            );
            assert!(format!("{off:?}").contains("None"));
            // describe shows the knob ONLY when it departs from the default
            assert!(
                on.describe().contains("context-header=toc-path"),
                "{}",
                on.describe()
            );
            assert!(
                !off.describe().contains("context-header"),
                "{}",
                off.describe()
            );
        }
        let raw: toml::Value = toml::from_str(
            "kind = \"token-window\"\nsize = 8\noverlap = 2\ncontext-header = \"heading-path\"",
        )
        .unwrap();
        let err = ChunkerKind::from_config("ingest.d.chunk", &raw, Some(6)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant `heading-path`, expected `none` or `toc-path`"),
            "{msg}"
        );
        assert!(msg.contains("[ingest.d.chunk]"), "{msg}");
        assert!(msg.contains("at line 6"), "{msg}");
        // fingerprint: the knob serializes elided at the default (pre-R2
        // fingerprints byte-stable) and PRESENT when on (改嵌入输入 = 改块身份
        // = 入指纹 — enabling demands the reindex of a fingerprint flip)
        let plain: toml::Value =
            toml::from_str("kind = \"token-window\"\nsize = 8\noverlap = 2").unwrap();
        let explicit_none = cch_kind("token-window", "none");
        let on = cch_kind("token-window", "toc-path");
        assert_eq!(
            toml::to_string(&plain_serde(&explicit_none)).unwrap(),
            toml::to_string(&plain_serde(&cch_kind_from(&plain))).unwrap(),
            "absent and explicit-none serialize identically (elided at the default)"
        );
        assert!(
            toml::to_string(&plain_serde(&on))
                .unwrap()
                .contains("context-header = \"toc-path\""),
            "the on-mode serializes into the fingerprint form"
        );
        assert_ne!(
            toml::to_string(&plain_serde(&on)).unwrap(),
            toml::to_string(&plain_serde(&explicit_none)).unwrap()
        );
        // a non-string value is serde's typed refusal (loud, section- and
        // line-attributed through the shared stage-2 shaper — the axis rule)
        let raw: toml::Value =
            toml::from_str("kind = \"markdown\"\nsize = 8\noverlap = 2\ncontext-header = 42")
                .unwrap();
        let err = ChunkerKind::from_config("ingest.d.chunk", &raw, Some(2)).unwrap_err();
        assert!(err.to_string().contains("[ingest.d.chunk]"), "{}", err);
        assert!(err.to_string().contains("at line 2"), "{}", err);
    }

    /// helper: serialize the INNER config (the fingerprint's view of a kind)
    fn plain_serde(k: &ChunkerKind) -> toml::Value {
        match k {
            ChunkerKind::TokenWindow(c) => {
                toml::Value::try_from(token_window::TokenWindowConfig::from(c)).unwrap()
            }
            ChunkerKind::Markdown(c) => {
                toml::Value::try_from(markdown::MarkdownConfig::from(c)).unwrap()
            }
        }
    }

    fn cch_kind_from(raw: &toml::Value) -> ChunkerKind {
        ChunkerKind::from_config("t", raw, None).unwrap()
    }

    /// The SOURCE-AWARE entry stamps prefixes; the plain trait path never
    /// does; mode none never does; the image atom never does (no text
    /// window). Every prefix's shape is pinned against the map: windows on
    /// page 1 carry 序章, page 2 carries 第二章 方法 (the nearest PRECEDING
    /// entry — 附录 targets page 5 and is future), and the composition is
    /// stem > title > chapter.
    #[test]
    fn chunk_source_stamps_prefixes_by_nearest_preceding_chapter() {
        let on = cch_kind("token-window", "toc-path");
        let part = paged_part_with_map();
        // window size 8 over 40 chars -> windows live inside one page each
        // except the straddlers; assert per-window by the page the window
        // STARTS on (the adjudicated lookup key)
        let chunks = on
            .chunk_source(std::slice::from_ref(&part), Some("golden-radar"))
            .unwrap();
        assert!(chunks.iter().all(|c| c.embed_prefix.is_some()));
        for c in &chunks {
            let prefix = c.embed_prefix.as_ref().unwrap();
            let page = c.page.unwrap().0;
            let chapter = if page == 1 {
                "序章"
            } else {
                "第二章 方法"
            };
            assert_eq!(prefix, &format!("golden-radar > {chapter}"), "{c:?}");
        }
        // mode none through the SAME source-aware entry: byte-old path
        let off = cch_kind("token-window", "none");
        let plain = off
            .chunk_source(&[paged_part_with_map()], Some("golden-radar"))
            .unwrap();
        assert!(plain.iter().all(|c| c.embed_prefix.is_none()), "{plain:?}");
        // mode ON through the PLAIN trait path: no prefix either (the
        // header's first component is the filename — a prefix without it
        // would be an identity production never sees)
        let trait_path = on.chunk(&[paged_part_with_map()]).unwrap();
        assert!(
            trait_path.iter().all(|c| c.embed_prefix.is_none()),
            "{trait_path:?}"
        );
        // and chunk_source with NO stem on mode on: still none
        let no_stem = on.chunk_source(&[paged_part_with_map()], None).unwrap();
        assert!(no_stem.iter().all(|c| c.embed_prefix.is_none()));
        // the image atom: prefix-free whatever the mode (mixed parts, the
        // atom between two text runs)
        let k = cch_kind("token-window", "toc-path");
        let mut atom = png_part("image/png");
        atom.section = None;
        let out = k
            .chunk_source(&[paged_part_with_map(), atom], Some("s"))
            .unwrap();
        assert!(
            out.iter()
                .any(|c| c.image.is_some() && c.embed_prefix.is_none())
        );
    }

    /// The map-less degradation, pinned: a part with NO sections (txt/md
    /// sources, structure=false) gets FILENAME-ONLY prefixes on mode on —
    /// the adjudicated non-PDF shape — and the straddling-window rule keys
    /// on the START page only (a window whose first char is on page 1
    /// carries page 1's chapter even when it reaches into page 2).
    #[test]
    fn mapless_parts_degrade_to_filename_only() {
        let on = cch_kind("token-window", "toc-path");
        let chunks = on
            .chunk_source(&[Part::text("plain prose here")], Some("notes"))
            .unwrap();
        assert!(
            chunks
                .iter()
                .all(|c| c.embed_prefix.as_deref() == Some("notes"))
        );
        // start-page rule: page 1's first chapter covers a window that
        // opens on page 1 even if the recurrence ends it on page 2
        let k = cch_kind("token-window", "toc-path");
        let mut p = Part::text("aaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbb");
        p.pages = vec![(1, 0), (2, 20)];
        p.sections = vec![(1, "一章".to_string()), (2, "二章".to_string())];
        let chunks = k.chunk_source(&[p], Some("doc")).unwrap();
        for c in &chunks {
            let start_page = c.page.unwrap().0;
            let chapter = if start_page == 1 { "一章" } else { "二章" };
            assert_eq!(
                c.embed_prefix.as_deref(),
                Some(format!("doc > {chapter}").as_str()),
                "{c:?}"
            );
        }
    }

    /// T18 (R8, D-9.3): the CONTAINER-LEAF arm — a sectioned, pageless
    /// part whose walk stamped a chapter title carries `stem > title` as
    /// its prefix (the epub chapter shape); the same-shaped part WITHOUT a
    /// title stays filename-only (the pre-T18 shape), and the FLAT PDF
    /// path (sections present, no title) keeps the TOC-page lookup
    /// byte-for-byte — the zero-change rule for the R2 corpus.
    #[test]
    fn container_leaf_titles_stamp_the_chapter_component() {
        let on = cch_kind("token-window", "toc-path");
        // the container leaf: pageless, mapless, titled
        let mut leaf = Part::text("chapter body prose long enough to window across");
        leaf.section = Some(3);
        leaf.title = Some("第七章　灰雾渡口".to_string());
        let chunks = on
            .chunk_source(std::slice::from_ref(&leaf), Some("novel"))
            .unwrap();
        assert!(
            chunks.iter().all(|c| c.embed_prefix.is_some()),
            "{chunks:?}"
        );
        for c in &chunks {
            assert_eq!(
                c.embed_prefix.as_deref(),
                Some("novel > 第七章　灰雾渡口"),
                "{c:?}"
            );
            // the title rides the chunk's orchestration channel too (the
            // x-vf-section-title stamper's input)
            assert_eq!(c.title.as_deref(), Some("第七章　灰雾渡口"), "{c:?}");
        }
        // the UNTITLED sibling: same shape, no title — filename-only (the
        // zip/eml container shape, and every pre-T18 container)
        let mut untitled = Part::text("chapter body prose long enough to window across");
        untitled.section = Some(4);
        let chunks = on
            .chunk_source(std::slice::from_ref(&untitled), Some("novel"))
            .unwrap();
        assert!(
            chunks
                .iter()
                .all(|c| c.embed_prefix.as_deref() == Some("novel") && c.title.is_none()),
            "{chunks:?}"
        );
        // the flat PDF path: sections present, title absent — the TOC-page
        // lookup answers EXACTLY as before (pinned against the same map:
        // page-1 windows carry 序章)
        let flat = paged_part_with_map();
        let pinned = on
            .chunk_source(std::slice::from_ref(&flat), Some("golden-radar"))
            .unwrap();
        for c in &pinned {
            let page = c.page.unwrap().0;
            let chapter = if page == 1 {
                "序章"
            } else {
                "第二章 方法"
            };
            assert_eq!(
                c.embed_prefix.as_deref(),
                Some(format!("golden-radar > {chapter}").as_str()),
                "{c:?}"
            );
        }
        // mode none: the title arm composes with the mode gate — off is
        // still off (no prefix ever, whatever the title)
        let off = cch_kind("token-window", "none");
        let chunks = off
            .chunk_source(std::slice::from_ref(&leaf), Some("novel"))
            .unwrap();
        assert!(
            chunks.iter().all(|c| c.embed_prefix.is_none()),
            "{chunks:?}"
        );
    }
}
