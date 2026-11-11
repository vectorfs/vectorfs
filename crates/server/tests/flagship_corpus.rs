//! T12 旗舰活语料过门（D-4 硬条件：零整页误判 + 零词表级误删）。
//!
//! Env-gated like `s3_conformance`: set `VF_FLAGSHIP_PDF_DIR` to a directory
//! holding the corpus this gate is named after (the ThoughtWorks 技术雷达
//! Chinese editions, `tr_technology_radar_vol_28..32_cn.pdf`) and run with
//! `-- --ignored`. The corpus is COPYRIGHTED material — it lives outside the
//! tree (dogfooding `docs` bucket) and is fetched ad hoc:
//!
//! ```sh
//! mkdir -p /tmp/flagship && cd /tmp/flagship
//! for v in 28 29 30 31 32; do
//!   vf get "docs/radar/tr_technology_radar_vol_${v}_cn.pdf" --out "vol${v}.pdf"
//! done
//! VF_FLAGSHIP_PDF_DIR=/tmp/flagship \
//!   cargo test -p vectorfs-server --test flagship_corpus -- --ignored --nocapture
//! ```
//!
//! The assertions encode the adversarial-review measurements (2026-11-07,
//! hostile-lens experiments on this very corpus):
//!
//! 1. **零词表级误删** — furniture removal must NOT delete the radar's four
//!    ring words (评估/试验/暂缓/采纳) or quadrant names (工具/平台/技术/
//!    语言和框架): a naive ≥20%-repeat rule deleted exactly these (the
//!    corpus's core vocabulary).
//! 2. **零整页误判** — vol32's measured false-positive pages under the NAIVE
//!    predicate (p9 radar overview 97%, p12/p20 quadrant listings, p29/p39
//!    quadrant entry pages — the standard answer pages for F2's enumeration
//!    queries) must carry ZERO Toc spans under the triple gate.
//! 3. **治理有效** — at least one genuine TOC page must be judged and
//!    labeled somewhere in the corpus (a gate that rejects everything would
//!    pass (2) vacuously).
//!
//! Diagnostics: every volume prints its page-level Toc judgments, so a
//! failure shows the full picture, not a bare assert.

use std::path::PathBuf;

use vectorfs_server::pipeline::chunk::token_window::TokenWindowChunker;
use vectorfs_server::pipeline::extract::pdf_text::{PdfTextExtractor, locate_library};
use vectorfs_server::pipeline::{ChunkKind, Chunker, Extractor, Part};

fn pdf() -> PdfTextExtractor {
    match locate_library() {
        Ok(_) => PdfTextExtractor::new(), // structure=true, the T12 adjudicated default
        Err(m) => panic!(
            "flagship gate needs the real pdfium library and none was found: {m} — \
             run scripts/fetch_runtime_libs.sh pdfium (or set VF_PDFIUM_PATH)"
        ),
    }
}

/// 1-based page covering `byte` (the span's opening), via the page table.
fn page_of(pages: &[(u32, usize)], total: usize, byte: usize) -> u32 {
    let idx = pages.partition_point(|&(_, off)| off <= byte) - 1;
    let _ = total;
    pages[idx].0
}

fn toc_pages(part: &Part) -> Vec<u32> {
    let mut out: Vec<u32> = part
        .labels
        .iter()
        .filter(|l| l.kind == ChunkKind::Toc)
        .map(|l| page_of(&part.pages, part.text.len(), l.range.start))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[test]
#[ignore = "needs the flagship corpus on disk; set VF_FLAGSHIP_PDF_DIR (see module doc)"]
fn flagship_zero_whole_page_misjudgment_zero_vocabulary_deletion() {
    let dir = std::env::var("VF_FLAGSHIP_PDF_DIR")
        .expect("VF_FLAGSHIP_PDF_DIR must point at the radar corpus directory (see module doc)");
    let dir = PathBuf::from(dir);
    let extractor = pdf();

    // 1. the core vocabulary a naive furniture rule deleted wholesale
    const VOCAB: &[&str] = &[
        "评估",
        "试验",
        "暂缓",
        "采纳", // ring words
        "工具",
        "平台",
        "技术",
        "语言和框架", // quadrant names
    ];
    // 2. vol32's measured naive-predicate false-positive pages (1-based)
    const VOL32_FP_PAGES: &[u32] = &[9, 12, 20, 29, 39];
    let mut any_toc_labeled = false;

    for vol in [28u32, 29, 30, 31, 32] {
        let path = dir.join(format!("vol{vol}.pdf"));
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let parts = extractor
            .extract(&bytes, "application/pdf")
            .unwrap_or_else(|e| panic!("vol{vol} extract: {e}"));
        // the pipeline contract: one text Part per document
        let text_part = parts
            .iter()
            .find(|p| !p.text.is_empty())
            .unwrap_or_else(|| panic!("vol{vol}: no text part"));

        // (1) vocabulary survival — zero vocabulary-level deletion
        for word in VOCAB {
            assert!(
                text_part.text.contains(word),
                "vol{vol}: furniture removal deleted core vocabulary {word:?}"
            );
        }

        let judged = toc_pages(text_part);
        eprintln!(
            "vol{vol}: pages={:?} furniture_removed={} toc_spans={}",
            judged,
            text_part.furniture_removed,
            text_part
                .labels
                .iter()
                .filter(|l| l.kind == ChunkKind::Toc)
                .count(),
        );
        any_toc_labeled |= !judged.is_empty();

        // R2 CCH (旗舰门 addition): the judged TOC hits must ALSO have
        // upgraded into the chapter map (`Part::sections`). The MEASURED
        // vol32 shape (this gate's own run): p2's ten entries top out in
        // the four QUADRANT names — 技术平台工具语言和框架 — so a chunk in
        // the Vibe Coding pages (p18-19) maps to 技术 (p12): exactly the
        // context the research doc's R2 example prescribed. Every target
        // inside the document, titles non-empty, quadrants present.
        if vol == 32 {
            assert!(
                !text_part.sections.is_empty(),
                "vol32: the judged TOC produced no section map (the R2 upgrade lost the hits)"
            );
            let page_count = text_part.pages.len() as u32;
            for (target, title) in &text_part.sections {
                assert!(
                    *target >= 1 && *target <= page_count,
                    "vol32: section entry ({target}, {title:?}) points outside the {page_count}-page document"
                );
                assert!(!title.trim().is_empty(), "vol32: empty section title");
            }
            for quadrant in ["技术", "平台", "工具", "语言和框架"] {
                assert!(
                    text_part.sections.iter().any(|(_, t)| t.contains(quadrant)),
                    "vol32: quadrant {quadrant:?} missing from the section titles: {:?}",
                    text_part.sections
                );
            }
            eprintln!(
                "vol32 section map ({} entries): {:?}",
                text_part.sections.len(),
                text_part.sections
            );
        }

        // (2) vol32's false-positive pages must be clean
        if vol == 32 {
            for p in VOL32_FP_PAGES {
                assert!(
                    !judged.contains(p),
                    "vol32 p{p}: whole-page TOC misjudgment returned (triple gate regressed)"
                );
            }
            // T13 (标签与坐标互证): vol32's genuine TOC spans all live on
            // page 2 — and the CHUNKS those spans address carry the page
            // coordinate. The label says "this is TOC"; the coordinate
            // independently says "this is page 2" — if either regressed
            // (span drifting across a page edge, page table mis-anchored),
            // the mutual pin fails. Two forms:
            // * every chunk INTERSECTING the span reports a page interval
            //   CONTAINING 2 (the span's content is addressed to page 2 —
            //   window shape irrelevant);
            // * at a window small enough to sit INSIDE the 71-char span,
            //   pure-toc windows exist and each reports exactly (2, 2).
            let toc_spans: Vec<&vectorfs_server::pipeline::LabelSpan> = text_part
                .labels
                .iter()
                .filter(|l| l.kind == ChunkKind::Toc)
                .collect();
            assert!(
                !toc_spans.is_empty(),
                "vol32 must carry its genuine TOC spans (the corpus's measured shape)"
            );
            // the span in the CHAR domain (labels are byte-anchored, the
            // chunk walk below is char-indexed — the test-side twin of the
            // chunker's own conversion duty)
            let boundaries: Vec<usize> = text_part.text.char_indices().map(|(b, _)| b).collect();
            let to_char = |byte: usize| boundaries.partition_point(|&b| b < byte);
            for span in &toc_spans {
                assert_eq!(
                    page_of(&text_part.pages, text_part.text.len(), span.range.start),
                    2,
                    "a vol32 Toc span left page 2: {span:?}"
                );
                let span_chars = to_char(span.range.start)..to_char(span.range.end);
                let chunker = TokenWindowChunker::new(512, 64).unwrap();
                let chunks = chunker
                    .chunk(std::slice::from_ref(text_part))
                    .expect("vol32 chunks");
                let mut pos = 0usize;
                let mut intersecting = 0usize;
                for c in &chunks {
                    let len = c.text.chars().count();
                    let (s, e) = (pos, pos + len);
                    if s < span_chars.end && span_chars.start < e {
                        intersecting += 1;
                        let (ps, pe) = c.page.expect("paged corpus chunk");
                        assert!(
                            ps <= 2 && 2 <= pe,
                            "a chunk addressing the toc span omits page 2: ({ps},{pe}) {c:?}"
                        );
                    }
                    pos = e - 64;
                }
                assert!(
                    intersecting > 0,
                    "the toc span must be covered by some window (lossless)"
                );
                // the pure form: windows small enough to sit inside the
                // span judge Toc and carry exactly (2, 2)
                let small = TokenWindowChunker::new(32, 8).unwrap();
                let chunks = small
                    .chunk(std::slice::from_ref(text_part))
                    .expect("vol32 small-window chunks");
                let toc_chunks: Vec<_> =
                    chunks.iter().filter(|c| c.kind == ChunkKind::Toc).collect();
                assert!(
                    !toc_chunks.is_empty(),
                    "a window inside the {}-char span must judge pure toc",
                    span_chars.len()
                );
                for c in &toc_chunks {
                    assert_eq!(
                        c.page,
                        Some((2, 2)),
                        "a toc-kind window outside page 2: {c:?}"
                    );
                }
                eprintln!(
                    "vol32: toc span {} chars; {} windows address it (all contain p2); {} pure-toc windows, all (2,2)",
                    span_chars.len(),
                    intersecting,
                    toc_chunks.len()
                );
            }
        }
    }

    // (3) the gate still catches real TOC pages somewhere in the corpus
    assert!(
        any_toc_labeled,
        "no Toc span anywhere in the corpus — the gate rejects everything (treatment vacuous)"
    );
}
