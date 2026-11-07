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
    /// compiler walks it; [`KIND_LABELS`] and the parity test then force the
    /// table views to agree). Unlike [`Self::describe`] this is knob-free.
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
                format!("token-window(size={}, overlap={})", c.size, c.overlap)
            }
            ChunkerKind::Markdown(c) => {
                format!("markdown(size={}, overlap={})", c.size, c.overlap)
            }
        }
    }
}

/// Dispatch: the enum IS the chunker — and the enum layer is where the
/// §5.10.2 modality split happens (see module docs). The text-run walking
/// and the image bypass below are AXIS mechanism shared by every kind;
/// each kind only ever sees a maximal run of Text parts.
impl Chunker for ChunkerKind {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError> {
        // Walk the parts in order, chunking each maximal run of TEXT parts
        // through the kind (a sub-slice, so a text-only input is ONE call —
        // byte-identical to calling the kind directly) and materializing
        // every image part as ONE atomic chunk at its position. Indices are
        // (re)assigned GLOBALLY here — the "sequential across all parts"
        // invariant is the axis's guarantee now, independent of how any
        // kind numbers its own output.
        let chunk_text_run = |run: &[Part]| -> Result<Vec<Chunk>, PipelineError> {
            match self {
                ChunkerKind::TokenWindow(c) => c.chunk(run),
                ChunkerKind::Markdown(c) => c.chunk(run),
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
}
