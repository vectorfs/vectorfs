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

use vectorfs_server::pipeline::extract::pdf_text::{PdfTextExtractor, locate_library};
use vectorfs_server::pipeline::{ChunkKind, Extractor, Part};

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

        // (2) vol32's false-positive pages must be clean
        if vol == 32 {
            for p in VOL32_FP_PAGES {
                assert!(
                    !judged.contains(p),
                    "vol32 p{p}: whole-page TOC misjudgment returned (triple gate regressed)"
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
