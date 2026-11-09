//! Golden corpus for the M2-T4 pipeline kinds (design.md §5.10.2 contracts).
//!
//! Corpus policy (four classes):
//! * **normal** — small committed fixtures in `tests/golden/` (`hello.pdf`
//!   hand-assembled minimal single-page PDF, `sample.md`, `sample.txt`);
//! * **corrupt** — committed damaged bytes (`corrupt.pdf`): a PDF magic on
//!   top of garbage → must be Err, never a panic;
//! * **huge** — GENERATED in-test (a 5 MiB text document; a 300-page PDF
//!   assembled by the builder below): nothing big is committed;
//! * **truncated** — byte-prefix slices of the NORMAL fixtures at test time.
//!   Tail truncation is called out for PDF specifically: a network-interrupted
//!   upload is the common real-world corruption shape, and pdfium's actual
//!   behavior (strict xref/trailer checking) is asserted as observed, not
//!   as wished.
//!
//! Every pdf assertion runs the REAL pdfium `.so` (missing library = panic
//! with fetch guidance, not a skip — honest failure rule). Location:
//! `vf/tests/golden/` (integration-test side, keeps binary fixtures out of
//! `src/`; also proves the pub pipeline API suffices without crate internals).

use vectorfs_server::pipeline::chunk::ChunkerKind;
use vectorfs_server::pipeline::chunk::token_window::TokenWindowChunker;
use vectorfs_server::pipeline::contract::chunker_contract;
use vectorfs_server::pipeline::extract::md::MdExtractor;
use vectorfs_server::pipeline::extract::pdf_inspector::PdfInspectorExtractor;
use vectorfs_server::pipeline::extract::pdf_text::{PdfTextExtractor, locate_library};
use vectorfs_server::pipeline::extract::txt::TxtExtractor;
use vectorfs_server::pipeline::{Chunk, ChunkKind, Chunker, Extractor, LabelSpan, Modality, Part};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

const HELLO_PDF: &[u8] = include_bytes!("golden/hello.pdf");
const CORRUPT_PDF: &[u8] = include_bytes!("golden/corrupt.pdf");
const SAMPLE_MD: &[u8] = include_bytes!("golden/sample.md");
const SAMPLE_TXT: &[u8] = include_bytes!("golden/sample.txt");

/// The real pdfium-backed extractor, or an actionable panic (never a skip).
fn pdf() -> PdfTextExtractor {
    match locate_library() {
        Ok(_) => PdfTextExtractor::new(),
        Err(m) => panic!(
            "golden pdf tests need the real pdfium library and none was found: {m} — \
             run scripts/fetch_runtime_libs.sh pdfium (or set VF_PDFIUM_PATH); \
             tests refuse to skip (M2-T4 honest-failure rule)."
        ),
    }
}

/// The concatenation invariant, re-implemented test-side (the contract
/// module's checker is crate-internal): chunk[0] in full, then
/// chars(overlap..) of every following chunk, per part, must rebuild the part.
fn reconstruct(chunks: &[Chunk], overlap: usize) -> String {
    let mut acc = String::new();
    for (i, ch) in chunks.iter().enumerate() {
        if i == 0 {
            acc.push_str(&ch.text);
        } else {
            acc.extend(ch.text.chars().skip(overlap));
        }
    }
    acc
}

/// Minimal multi-page PDF builder (no external tool): N pages, each drawing
/// its own page marker (12pt) plus the 10pt body paragraph supplied by
/// `paragraph_for(i)`. Objects, exact `/Length`s and a correct xref table
/// are assembled with byte offsets — the same hand-assembly discipline as
/// the committed hello.pdf fixture.
///
/// # T12 uniquification (adversarial review of D-4)
///
/// The pre-T12 fixture drew the IDENTICAL paragraph on every page, which
/// T12's furniture rules (≥20% of pages repeating a line ⇒ furniture) would
/// later wholesale-delete — every page's body and the fixture's point with
/// it. `paragraph_for` is therefore PER-PAGE by contract: callers compose
/// [`unique_head`] (page number + rotating words) with a shared filler tail
/// for the "huge" mass, and the tests pin each probed page's unique head
/// surviving extraction so the uniqueness cannot silently rot.
///
/// # Recorded engine behaviors this shape threads (all probed, 2026-11)
///
/// * pdfium CLIPS a single overflowing `Tj` line at the page box (~156
///   chars of a long line extract) — the unique head must LEAD the
///   paragraph, never trail it;
/// * pdf-inspector 1.25.2's markdown pipeline emits an EMPTY page whenever
///   a synthetic page carries two same-size (12pt) text lines (the
///   pre-T12 fixture's own "page 0001 vanishes" quirk was exactly this:
///   only its first page kept two lines after header/footer stripping) —
///   the 10pt body font gives the pipeline the size contrast it needs,
///   which is why the marker is 12pt and the body 10pt here.
/// Per-page-unique paragraph-prefix words (letters only, rotating with the
/// page index — see [`unique_head`]).
const UNIQUE_TAIL_WORDS: [&str; 12] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima",
];

/// The per-page-unique head of a page's paragraph (probe-assertable: tests
/// recompute it with the same page index). Embeds the page number so no two
/// pages share a paragraph even after any digit-normalizing furniture rule.
fn unique_head(i: usize) -> String {
    format!(
        "golden body {:04} {} {}",
        i + 1,
        UNIQUE_TAIL_WORDS[i % UNIQUE_TAIL_WORDS.len()],
        UNIQUE_TAIL_WORDS[(i + 5) % UNIQUE_TAIL_WORDS.len()]
    )
}

fn build_multipage_pdf(pages: usize, paragraph_for: impl Fn(usize) -> String) -> Vec<u8> {
    let escape = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)")
    };
    let content = |i: usize| {
        format!(
            "BT /F1 12 Tf 72 720 Td (page {:04} of {:04}) Tj /F1 10 Tf 0 -20 Td ({}) Tj ET\n",
            i + 1,
            pages,
            escape(&paragraph_for(i))
        )
    };
    // object numbering: 1 catalog, 2 pages, 3 font, then (page, contents)
    // pairs from object 4 on.
    let mut kids = String::new();
    for i in 0..pages {
        if i > 0 {
            kids.push(' ');
        }
        kids.push_str(&format!("{} 0 R", 4 + 2 * i));
    }
    let mut bodies: Vec<Vec<u8>> = vec![
        format!("<< /Type /Catalog /Pages 2 0 R >>").into_bytes(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for i in 0..pages {
        let c = content(i);
        bodies.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << \
                 /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            )
            .into_bytes(),
        );
        bodies.push(format!("<< /Length {} >>\nstream\n{}endstream", c.len(), c).into_bytes());
    }
    assemble_xref(&bodies)
}

/// Xref-exact assembly shared by the hand-built fixtures: object bodies in
/// order (object id = index + 1), byte-exact offsets, correct trailer
/// (same hand-assembly discipline as the committed hello.pdf).
fn assemble_xref(bodies: &[Vec<u8>]) -> Vec<u8> {
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(bodies.len());
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", bodies.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            bodies.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// The T12 line-fixture builder (D-4): each page draws its own LINES
/// top-down — 12pt for the page's first line, 10pt after, 18pt leading,
/// a 40pt gap after the first line. The PROBED shape (2026-11-08) pdfium
/// extracts line-faithfully: lines verbatim in draw order, `\r\n`
/// separators, no trailing break, no blank lines for the gap. Callers
/// pass RAW text (no PDF escaping needed for the D-4 fixtures —
/// parentheses/backslashes stay out of the line texts).
fn build_multipage_pdf_lines(pages: &[Vec<String>]) -> Vec<u8> {
    let content = |lines: &[String]| {
        let mut ops = String::new();
        let mut y = 720i32;
        for (i, line) in lines.iter().enumerate() {
            let size = if i == 0 { 12 } else { 10 };
            if i == 1 {
                y -= 40; // the gap that keeps a title line visually alone
            }
            ops.push_str(&format!("BT /F1 {size} Tf 72 {y} Td ({line}) Tj ET\n"));
            y -= 18;
        }
        ops
    };
    let mut kids = String::new();
    for i in 0..pages.len() {
        if i > 0 {
            kids.push(' ');
        }
        kids.push_str(&format!("{} 0 R", 4 + 2 * i));
    }
    let mut bodies: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for (i, page_lines) in pages.iter().enumerate() {
        let c = content(page_lines);
        bodies.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << \
                 /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            )
            .into_bytes(),
        );
        bodies.push(format!("<< /Length {} >>\nstream\n{}endstream", c.len(), c).into_bytes());
    }
    assemble_xref(&bodies)
}

// ---------------------------------------------------------------------------
// normal corpus
// ---------------------------------------------------------------------------

#[test]
fn golden_pdf_normal_extracts_hello() {
    let x = pdf();
    let a = x
        .extract(HELLO_PDF, "application/pdf")
        .expect("hello.pdf must extract");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].modality, Modality::Text);
    assert!(
        a[0].text.contains("Hello vectorfs"),
        "extracted: {:?}",
        a[0].text
    );
    // determinism against the real engine
    let b = x.extract(HELLO_PDF, "application/pdf").unwrap();
    assert_eq!(a, b);
}

#[test]
fn golden_md_and_txt_normal_samples() {
    let md_parts = MdExtractor.extract(SAMPLE_MD, "text/markdown").unwrap();
    assert_eq!(md_parts.len(), 1);
    let md = &md_parts[0].text;
    // structure -> newlines; inline markup gone; code blocks kept as text
    assert!(md.starts_with("VectorFS Golden Markdown"), "{md}");
    assert!(
        md.contains("Structure section\nfirst item\nsecond item with 中文"),
        "{md}"
    );
    assert!(md.contains("[ ] unchecked task\n[x] checked task"), "{md}");
    assert!(md.contains("println!(\"golden code block\");"), "{md}");
    assert!(!md.contains("**"), "bold markers must not survive: {md}");
    assert!(
        !md.contains("https://"),
        "link targets are markup, not text: {md}"
    );

    let txt_parts = TxtExtractor.extract(SAMPLE_TXT, "text/plain").unwrap();
    assert_eq!(txt_parts.len(), 1);
    assert!(
        txt_parts[0].text.contains("CJK 汉字"),
        "{}",
        txt_parts[0].text
    );
    assert!(txt_parts[0].text.contains("héllo wörld"));
}

/// The M2 happy-path shape end to end on the committed fixtures:
/// extract(kind) -> chunk(token-window) -> lossless reconstruction.
#[test]
fn golden_extract_then_chunk_round_trips() {
    let chunker = TokenWindowChunker::new(64, 16).unwrap();
    for (bytes, mime) in [
        (HELLO_PDF, "application/pdf"),
        (SAMPLE_MD, "text/markdown"),
        (SAMPLE_TXT, "text/plain"),
    ] {
        let kind = vectorfs_server::pipeline::extract::ExtractorKind::from_config(
            "golden",
            &toml::from_str::<toml::Value>(&format!(
                "kind = \"{}\"",
                match mime {
                    "application/pdf" => "pdf-text",
                    "text/markdown" => "md",
                    _ => "txt",
                }
            ))
            .unwrap(),
            None,
        )
        .unwrap();
        let parts: Vec<Part> = kind.extract(bytes, mime).unwrap();
        let chunks = chunker.chunk(&parts).unwrap();
        assert!(!chunks.is_empty(), "{mime} produced no chunks");
        // sequential indices across the run
        for (i, ch) in chunks.iter().enumerate() {
            assert_eq!(ch.index, i as u32);
        }
        let rebuilt: String = chunks
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                if i == 0 {
                    ch.text.clone()
                } else {
                    ch.text.chars().skip(16).collect::<String>()
                }
            })
            .collect();
        // every part's text survives chunking (single-part kinds here)
        assert_eq!(rebuilt, parts[0].text, "{mime}: chunking lost text");
    }
}

// ---------------------------------------------------------------------------
// corrupt corpus
// ---------------------------------------------------------------------------

#[test]
fn golden_pdf_corrupt_is_error_not_panic() {
    let err = pdf()
        .extract(CORRUPT_PDF, "application/pdf")
        .err()
        .expect("damaged bytes claiming to be a PDF must Err");
    assert!(
        err.to_string().contains("extract failed"),
        "unexpected error shape: {err}"
    );
    // determinism of the failure path
    let again = pdf().extract(CORRUPT_PDF, "application/pdf").unwrap_err();
    assert_eq!(err, again);
}

// ---------------------------------------------------------------------------
// truncated corpus (pdf tail truncation = the common real-world case)
// ---------------------------------------------------------------------------

#[test]
fn golden_pdf_truncated_tail_is_error_not_panic() {
    let x = pdf();
    // RECORDED behavior of pdfium chromium/8066 on tail-truncated PDFs
    // (probed against this fixture): a missing TAIL of up to ~15 bytes —
    // "%%EOF\n", even the whole "startxref\n<n>\n%%EOF\n" — still loads and
    // extracts the COMPLETE text, because pdfium falls back to reconstructing
    // the cross-reference table when the trailer is damaged. Deeper cuts
    // (into the xref table or body) are load errors. Both halves are real
    // recovery semantics, asserted as observed; if a future pdfium changes
    // this, the test fails and forces a deliberate re-record.
    let len = HELLO_PDF.len();
    // tolerated tail loss: full text still extracted
    for cut in [len - 6, len - 15] {
        let parts = x
            .extract(&HELLO_PDF[..cut], "application/pdf")
            .unwrap_or_else(|e| panic!("cut={cut}: pdfium tolerated this loss before: {e}"));
        assert_eq!(parts.len(), 1, "cut={cut}");
        assert!(
            parts[0].text.contains("Hello vectorfs"),
            "cut={cut}: recovered text must still be complete"
        );
    }
    // detected truncation: load error, never a panic
    for cut in [
        len - 25,
        len * 90 / 100,
        len * 75 / 100,
        len / 2,
        len / 10,
        9,
    ] {
        let err = x
            .extract(&HELLO_PDF[..cut], "application/pdf")
            .err()
            .unwrap_or_else(|| panic!("cut={cut}: pdfium accepted a deeply truncated PDF"));
        assert!(
            err.to_string().contains("cannot open this document"),
            "cut={cut}: unexpected error text: {err}"
        );
    }
}

#[test]
fn golden_md_txt_truncated_never_panic() {
    // byte prefixes of the text fixtures: UTF-8 is self-synchronizing, so
    // some cuts stay valid text (Ok allowed — genuinely valid input), a cut
    // inside a multibyte char must Err; neither may panic.
    for cut in 1..SAMPLE_TXT.len() {
        let _ = TxtExtractor.extract(&SAMPLE_TXT[..cut], "text/plain");
    }
    for cut in 1..SAMPLE_MD.len() {
        let _ = MdExtractor.extract(&SAMPLE_MD[..cut], "text/markdown");
    }
    // the known-detectable cuts: both fixtures carry multibyte chars; a cut
    // mid-glyph is non-UTF-8 -> Err
    let md = std::str::from_utf8(SAMPLE_MD).unwrap();
    let cjk_byte = md.find('中').expect("fixture carries 中文");
    assert!(
        MdExtractor
            .extract(&SAMPLE_MD[..cjk_byte + 1], "text/markdown")
            .is_err()
    );
    let txt = std::str::from_utf8(SAMPLE_TXT).unwrap();
    let cjk_byte = txt.find('汉').expect("fixture carries 汉字");
    assert!(
        TxtExtractor
            .extract(&SAMPLE_TXT[..cjk_byte + 1], "text/plain")
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// huge corpus (generated, never committed)
// ---------------------------------------------------------------------------

#[test]
fn golden_huge_txt_5mib_chunks_losslessly() {
    // ~5 MiB deterministic prose: no RNG, no fixtures, just repetition with
    // rotating markers so windows cannot accidentally align.
    let mut text = String::with_capacity(5 * 1024 * 1024 + 256);
    let words = [
        "vectorfs",
        "golden",
        "chunking",
        "检索",
        "fingerprint",
        "rrf",
    ];
    let mut i = 0usize;
    while text.len() < 5 * 1024 * 1024 {
        text.push_str(words[i % words.len()]);
        text.push(' ');
        if i % 24 == 23 {
            text.push_str("end of sentence.\n\n");
        }
        i += 1;
    }
    assert!(text.len() >= 5 * 1024 * 1024);

    let parts = TxtExtractor.extract(text.as_bytes(), "text/plain").unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].text.len(), text.len());

    let chunker = TokenWindowChunker::new(512, 64).unwrap();
    let chunks = chunker.chunk(&parts).unwrap();
    // ~5 MiB / stride 448 -> ~11.7k chunks; assert the order of magnitude
    // and the invariant, which is the real point
    assert!(chunks.len() > 10_000, "chunk count: {}", chunks.len());
    assert_eq!(&reconstruct(&chunks, 64), &parts[0].text);
    // every chunk within budget
    assert!(chunks.iter().all(|c| c.text.chars().count() <= 512));
}

#[test]
fn golden_huge_multipage_pdf_extracts_every_page() {
    // 300 pages * ~1 KiB of drawn text per page: the "multi-page PDF
    // concatenation" huge shape, generated at test time. Each paragraph is
    // per-page-unique (furniture-rule armor — see build_multipage_pdf's
    // doc): unique head + shared filler tail for the mass.
    const PAGES: usize = 300;
    let paragraph_for = |i: usize| {
        format!(
            "{} {}",
            unique_head(i),
            "lorem ipsum dolor sit amet consectetur ".repeat(17)
        )
    };
    let pdf_bytes = build_multipage_pdf(PAGES, paragraph_for);
    assert!(pdf_bytes.len() > 200_000, "pdf size: {}", pdf_bytes.len());

    let x = pdf();
    let parts = x
        .extract(&pdf_bytes, "application/pdf")
        .expect("generated PDF must extract");
    assert_eq!(parts.len(), 1);
    let text = &parts[0].text;
    // every page's marker survived extraction (page text joined "\n\n")
    for probe in [1usize, 42, 150, PAGES] {
        assert!(
            text.contains(&format!("page {probe:04} of {PAGES:04}")),
            "page {probe} marker missing"
        );
        // the per-page-unique paragraph head survived too (uniqueness
        // pinned — a regressed shared-paragraph builder fails here, not in
        // T12). The head must LEAD the paragraph: pdfium clips an
        // overflowing Tj line at the page box (~156 chars extract)
        assert!(
            text.contains(&format!("golden body {probe:04} ")),
            "page {probe} unique body head missing"
        );
    }
    assert!(
        text.contains("\n\n"),
        "multi-page join must separate pages with a blank line"
    );
    // determinism on a large document
    let again = x.extract(&pdf_bytes, "application/pdf").unwrap();
    assert_eq!(parts, again);
}

// ---------------------------------------------------------------------------
// T12 structure labels: the D-4 counterexample corpus (adjudicated shape)
// through the REAL pdfium engine. Every gate that two adversarial-review
// rounds landed has a fixture page here — the unit matrix lives in
// src/pipeline/extract/structure.rs; this is the end-to-end pin that the
// engine's real output (drawn lines -> \r\n text) feeds those gates
// unchanged.
// ---------------------------------------------------------------------------

/// The furniture positive: a copyright strap on EVERY page head — repeated
/// on 100% of pages, in the edge window, substantial ⇒ removed + audited.
const D4_STRAP: &str = "(c) 2026 vectorfs radar monthly";

/// The furniture negative (min-length gate): 2-char ring words at page
/// heads, each on 50% of the pages — the radar four-ring-vocabulary shape
/// (评估/试验/暂缓/采纳 are 2 CJK chars; ASCII stand-ins exercise the same
/// gate through a Helvetica-only synthetic PDF, CJK needing a CID font).
const D4_RING: [&str; 2] = ["go", "no"];

/// The demotion positive (streak branch): running chapter heads, one per
/// 3-page chapter — 15% of the 20 pages (< the 20% delete branch) with a
/// 3-page streak ⇒ kept, Caption-spanned.
const D4_CHAPTERS: [&str; 6] = [
    "chapter alpha running head",
    "chapter bravo running head",
    "chapter charlie running head",
    "chapter delta running head",
    "chapter echo running head",
    "chapter foxtrot running head",
];

/// The TOC positive (v2): a TITLELESS no-leader contents page — the
/// flagship corpus's genuine shape (the radar TOC carries no 目录/
/// Contents line; the v1 title hard gate rejected every one and the
/// first flagship run came back with zero Toc spans). Entries monotonic,
/// every number inside the 21-page document (the v2 range gate).
const D4_TOC_ENTRIES: [(&str, u32); 8] = [
    ("preface of the volume", 1),
    ("glossary of terms", 2),
    ("intro for chapter alpha", 4),
    ("intro for chapter bravo", 7),
    ("intro for chapter charlie", 10),
    ("intro for chapter delta", 13),
    ("intro for chapter echo", 16),
    ("intro for chapter foxtrot", 19),
];

/// The TOC negative #1 (monotonicity gate alone): page-number-SIZED
/// amounts in a non-monotonic column — the range gate cannot catch these
/// (all ≤ 21), only the ordering evidence can.
const D4_PRICES_UNORDERED: [(&str, u32); 6] = [
    ("gadget alpha", 12),
    ("gadget bravo", 3),
    ("gadget charlie", 18),
    ("gadget delta", 7),
    ("gadget echo", 20),
    ("gadget foxtrot", 5),
];

/// The TOC negative #2 (range gate alone): an ascending-sorted price
/// column — monotonic, high ratio, exactly a TOC's shape — whose amounts
/// OVERFLOW the document's 21 pages. The v2 successor of the demoted
/// title gate's residual duty.
const D4_PRICES_SORTED: [(&str, u32); 5] = [
    ("widget alpha", 129),
    ("widget bravo", 899),
    ("widget charlie", 2499),
    ("widget delta", 3299),
    ("widget echo", 5999),
];

/// The D-4 document (v2): 21 pages — page 1 the TITLELESS TOC positive,
/// page 2 the unordered-price negative (monotonicity alone), page 3 the
/// sorted-price negative (range alone), pages 4-21 six 3-page chapters.
/// Returns the per-page LINE lists (the raw expectations for both
/// structure paths recompute from the same table — 对拍 without a second
/// source of truth).
fn d4_pages() -> Vec<Vec<String>> {
    let mut pages: Vec<Vec<String>> = Vec::new();
    for i in 0..21 {
        let mut lines = vec![D4_STRAP.to_string(), D4_RING[i % D4_RING.len()].to_string()];
        match i {
            0 => {
                // no title line — the flagship radar shape (v2)
                for (t, n) in D4_TOC_ENTRIES {
                    lines.push(format!("{t} {n}"));
                }
            }
            1 => {
                for (t, n) in D4_PRICES_UNORDERED {
                    lines.push(format!("{t} {n}"));
                }
                lines.push("price list closing prose line".to_string());
            }
            2 => {
                for (t, n) in D4_PRICES_SORTED {
                    lines.push(format!("{t} {n}"));
                }
                lines.push("second price list closing prose line".to_string());
            }
            i => {
                let chapter = D4_CHAPTERS[(i - 3) / 3];
                lines.push(chapter.to_string());
                // middle page number: unique per page, never a trailing
                // digit (a trailing page digit would make the line a
                // furniture strap — by design)
                lines.push(format!("body prose for page {i} of the document"));
            }
        }
        pages.push(lines);
    }
    pages
}

fn build_d4_structure_pdf() -> Vec<u8> {
    build_multipage_pdf_lines(&d4_pages())
}

#[test]
fn golden_t12_structure_labels_furniture_and_toc() {
    let pdf_bytes = build_d4_structure_pdf();
    let x = pdf();
    let parts = x
        .extract(&pdf_bytes, "application/pdf")
        .expect("D-4 fixture must extract");
    assert_eq!(parts.len(), 1);
    let part = &parts[0];

    // ---- furniture positive: the strap is gone everywhere, audited ----
    assert!(
        !part.text.contains(D4_STRAP),
        "the (c) strap must not survive: {:?}",
        &part.text[..80.min(part.text.len())]
    );
    assert_eq!(part.furniture_removed, 21, "one removal per page");

    // ---- page table: 1-based, byte-accurate, page slices carry their
    // own content (the offset 对拍: every offset slices out exactly its
    // page, first kept line = the ring word) ----
    assert_eq!(part.pages.len(), 21);
    assert_eq!(part.pages[0], (1, 0));
    for i in 0..21 {
        let (p, off) = part.pages[i];
        assert_eq!(p, i as u32 + 1);
        let end = part
            .pages
            .get(i + 1)
            .map(|&(_, o)| o - 2) // strip the "\n\n" join
            .unwrap_or(part.text.len());
        let page_text = &part.text[off..end];
        assert_eq!(
            page_text.lines().next(),
            Some(D4_RING[i % D4_RING.len()]),
            "page {p} must open with its ring word (min-length gate, real engine)"
        );
    }

    // ---- furniture negative: every ring word survives on its pages
    // (11 and 10 of 21 — both well past the ≥20% repeat branch) ----
    for (w, want) in [(D4_RING[0], 11), (D4_RING[1], 10)] {
        assert_eq!(
            part.text.matches(w).count(),
            want,
            "ring word {w:?} must survive all of its pages (min-length gate)"
        );
    }

    // ---- demotion: running chapter heads KEPT, Caption-spanned (6 x 3) ----
    let captions: Vec<&LabelSpan> = part
        .labels
        .iter()
        .filter(|s| s.kind == ChunkKind::Caption)
        .collect();
    assert_eq!(captions.len(), 18, "per-page Caption spans");
    for chapter in D4_CHAPTERS {
        assert_eq!(
            part.text.matches(chapter).count(),
            3,
            "running head {chapter:?} must stay in the text"
        );
    }
    assert!(
        captions
            .iter()
            .all(|s| D4_CHAPTERS.contains(&&part.text[s.range.clone()][..]))
    );

    // ---- TOC positive: exactly one Toc span, inside page 1, entries
    // kept in the text (labels, never removal) ----
    let toc: Vec<&LabelSpan> = part
        .labels
        .iter()
        .filter(|s| s.kind == ChunkKind::Toc)
        .collect();
    assert_eq!(toc.len(), 1, "labels: {:?}", part.labels);
    let toc_text = &part.text[toc[0].range.clone()];
    assert!(toc_text.contains("preface of the volume 1"), "{toc_text:?}");
    assert!(
        toc_text.contains("intro for chapter foxtrot 19"),
        "{toc_text:?}"
    );
    let page1_end = part.pages[1].1;
    assert!(
        toc[0].range.end <= page1_end,
        "the Toc span must live inside page 1"
    );

    // ---- TOC negatives: BOTH price pages stay Toc-free through the real
    // engine — page 2 (page-number-sized amounts, unordered column) is
    // rejected by MONOTONICITY alone; page 3 (sorted amounts overflowing
    // the 21-page document) is rejected by the RANGE gate alone ----
    for (page_idx, note) in [(1usize, "monotonicity"), (2, "range")] {
        let start = part.pages[page_idx].1;
        let end = part
            .pages
            .get(page_idx + 1)
            .map(|&(_, o)| o)
            .unwrap_or(part.text.len());
        assert!(
            part.labels
                .iter()
                .filter(|s| s.kind == ChunkKind::Toc)
                .all(|s| { s.range.start >= end || s.range.end <= start }),
            "price page {} must stay Toc-free ({note} gate, real engine)",
            page_idx + 1
        );
    }
    assert!(part.text.contains("gadget alpha 12"), "prices stay");
    assert!(part.text.contains("widget echo 5999"), "sorted prices stay");

    // ---- determinism with the annotated product in the equality ----
    let again = x.extract(&pdf_bytes, "application/pdf").unwrap();
    assert_eq!(parts, again);
}

/// The structure=false equivalence pin against the real .so: the raw page
/// texts joined `"\n\n"` with their `\r\n` line separators intact — the
/// pre-T12 extraction byte-for-byte — and EMPTY annotation channels.
#[test]
fn golden_t12_structure_false_is_the_pre_t12_join() {
    let pdf_bytes = build_d4_structure_pdf();
    let x = pdf().with_structure(false);
    let parts = x
        .extract(&pdf_bytes, "application/pdf")
        .expect("fixture must extract");
    assert_eq!(parts.len(), 1);
    let expected = d4_pages()
        .iter()
        .map(|lines| lines.join("\r\n"))
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(parts[0].text, expected, "pre-T12 byte-exact join");
    assert!(parts[0].labels.is_empty());
    assert!(parts[0].pages.is_empty());
    assert_eq!(parts[0].furniture_removed, 0);
    // ...and the strap IS in the raw product (no removal on this path)
    assert!(parts[0].text.contains(D4_STRAP));
}

/// T13 page coordinates 对拍 the page table through the REAL engine: every
/// chunk of the multipage fixture reports exactly the pages its chars
/// cover — re-derived here over an INDEPENDENT path (the window walk in
/// byte space: window i starts at window i-1's end minus the overlap, an
/// ASCII fixture makes char == byte) against the extractor's own
/// page-offset table. The per-page markers are the human witness (a chunk
/// whose text lists page p's marker carries p inside its interval), and
/// both window shapes — inside one page and straddling a boundary — must
/// occur, with the honest cross-page interval reported.
#[test]
fn golden_t13_page_coordinates_match_page_content() {
    // 12 pages, each: a per-page marker line + a per-page-unique body
    // (furniture-rule armor — see build_multipage_pdf's doc; nothing
    // repeats, so the structure path removes nothing and every page
    // offset is real). Body filler is per-page too, so no whole-window
    // content can repeat at a shifted offset.
    const PAGES: usize = 12;
    let paragraph_for = |i: usize| {
        format!(
            "{} with a tail of {} words to fill the window",
            unique_head(i),
            "lorem ipsum dolor sit amet"
        )
    };
    let pdf_bytes = build_multipage_pdf(PAGES, paragraph_for);
    let parts = pdf()
        .extract(&pdf_bytes, "application/pdf")
        .expect("fixture must extract");
    assert_eq!(parts.len(), 1);
    let part = &parts[0];
    assert_eq!(part.pages.len(), PAGES);
    assert_eq!(
        part.furniture_removed, 0,
        "fixture self-check: nothing removed"
    );

    // byte-domain page lookup (the independent direction: the chunker
    // worked char-domain through the conversion table)
    let page_of = |byte: usize| -> u32 {
        let idx = part.pages.partition_point(|&(_, off)| off <= byte) - 1;
        part.pages[idx].0
    };

    const SIZE: usize = 64;
    const OVERLAP: usize = 16;
    let chunker = TokenWindowChunker::new(SIZE, OVERLAP).unwrap();
    let chunks = chunker.chunk(std::slice::from_ref(part)).unwrap();
    assert!(
        chunks.len() >= PAGES,
        "{} chunks over 12 pages",
        chunks.len()
    );

    // the byte-space window walk: window i covers [pos, pos+len), the next
    // window starts at end - overlap (the pinned alignment contract)
    let mut pos = 0usize;
    let mut starts_seen = 0u32;
    let mut crossings = 0u32;
    let mut covered_pages = std::collections::BTreeSet::new();
    for ch in &chunks {
        let len = ch.text.len(); // ASCII fixture: bytes == chars
        let (want_start, want_end) = (page_of(pos), page_of(pos + len - 1));
        // D-7: the paged part reports page intervals and NO line intervals
        assert_eq!(
            ch.page,
            Some((want_start, want_end)),
            "chunk {pos}..{}: {ch:?} / pages {:?}",
            pos + len,
            part.pages
        );
        assert_eq!(ch.line, None, "paged part carries no line keys (D-7)");
        assert_eq!(ch.section, None);
        if want_start == want_end {
            starts_seen += 1;
        } else {
            crossings += 1;
        }
        // the marker witness: every page marker LISTED in this chunk's text
        // is inside its interval (markers are per-page unique, both the
        // page strap and the golden-body head)
        for p in 1..=PAGES as u32 {
            let listed = ch.text.contains(&format!("page {p:04} of"))
                || ch.text.contains(&format!("golden body {p:04}"));
            if listed {
                assert!(
                    want_start <= p && p <= want_end,
                    "chunk lists page {p} outside ({want_start},{want_end}): {ch:?}"
                );
                covered_pages.insert(p);
            }
        }
        pos = pos + len - OVERLAP;
    }
    // both window shapes occurred (inside-page AND boundary-straddling —
    // the honest overlap, not a degenerate all-one-page or all-crossing)
    assert!(starts_seen > 0, "fixture must produce inside-page windows");
    assert!(
        crossings > 0,
        "fixture must produce boundary-straddling windows"
    );
    // and the witness coverage is total: every page's marker landed in some
    // window whose interval contains it (lossless + coordinates together)
    assert_eq!(
        covered_pages.len(),
        PAGES,
        "every page marker witnessed: {covered_pages:?}"
    );
}

// ---------------------------------------------------------------------------
// chunker contract re-run through the registered config path (the ingest
// worker's construction route), on the rewritten text-splitter engine
// ---------------------------------------------------------------------------

#[test]
fn token_window_contract_through_config_path() {
    let raw =
        toml::from_str::<toml::Value>("kind = \"token-window\"\nsize = 48\noverlap = 12").unwrap();
    let kind = ChunkerKind::from_config("ingest.golden.chunk", &raw, Some(1)).unwrap();
    chunker_contract(&kind, 48, 12);
}

// ---------------------------------------------------------------------------
// pdf-inspector corpus (the second PDF engine, run over the SAME committed
// fixtures as pdf-text — corpus reuse across engines is the point: normal /
// corrupt / truncated behavior is only comparable when the bytes are
// identical). No .so needed: the pure-Rust engine runs wherever cargo test
// runs.
// ---------------------------------------------------------------------------

#[test]
fn golden_pdf_inspector_normal_extracts_hello() {
    let x = PdfInspectorExtractor::new();
    let a = x
        .extract(HELLO_PDF, "application/pdf")
        .expect("hello.pdf must extract");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].modality, Modality::Text);
    assert!(
        a[0].text.contains("Hello vectorfs"),
        "extracted: {:?}",
        a[0].text
    );
    // determinism against the real engine
    let b = x.extract(HELLO_PDF, "application/pdf").unwrap();
    assert_eq!(a, b);
    // markdown flavor on the same fixture: position-aware structure
    let md = PdfInspectorExtractor::markdown()
        .extract(HELLO_PDF, "application/pdf")
        .unwrap();
    assert_eq!(md.len(), 1);
    assert!(
        md[0].text.contains("## Hello vectorfs"),
        "markdown flavor: {:?}",
        md[0].text
    );
}

#[test]
fn golden_pdf_inspector_corrupt_is_error_not_panic() {
    let x = PdfInspectorExtractor::new();
    let err = x
        .extract(CORRUPT_PDF, "application/pdf")
        .err()
        .expect("damaged bytes claiming to be a PDF must Err");
    assert!(
        err.to_string().contains("cannot classify"),
        "unexpected error shape: {err}"
    );
    // determinism of the failure path
    let again = x.extract(CORRUPT_PDF, "application/pdf").unwrap_err();
    assert_eq!(err, again);
}

#[test]
fn golden_pdf_inspector_truncated_tail_never_panics() {
    // RECORDED behavior of pdf-inspector 1.25.2 (lopdf) on tail-truncated
    // PDFs, probed against this fixture — asserted as observed, like the
    // pdfium table above: shallow tail loss (the "%%EOF\n" and a few bytes
    // more) is TOLERATED (still classifies TextBased, still extracts the
    // full text — lopdf recovers like pdfium does), deeper cuts are
    // classification errors.
    let x = PdfInspectorExtractor::new();
    let len = HELLO_PDF.len();
    // tolerated tail loss: full text still extracted
    for cut in [len - 6, len - 15] {
        let parts = x
            .extract(&HELLO_PDF[..cut], "application/pdf")
            .unwrap_or_else(|e| panic!("cut={cut}: pdf-inspector tolerated this loss before: {e}"));
        assert_eq!(parts.len(), 1, "cut={cut}");
        assert!(
            parts[0].text.contains("Hello vectorfs"),
            "cut={cut}: recovered text must still be complete"
        );
    }
    // detected truncation: classification error, never a panic
    for cut in [
        len - 25,
        len * 90 / 100,
        len * 75 / 100,
        len / 2,
        len / 10,
        9,
    ] {
        let err = x
            .extract(&HELLO_PDF[..cut], "application/pdf")
            .err()
            .unwrap_or_else(|| panic!("cut={cut}: pdf-inspector accepted a deeply truncated PDF"));
        assert!(
            err.to_string().contains("cannot classify"),
            "cut={cut}: unexpected error text: {err}"
        );
    }
}

#[test]
fn golden_pdf_inspector_multipage_and_config_path() {
    // the shared in-test 300-page builder: the pure-Rust engine must get
    // every page's marker out of the same generated bytes pdfium does (the
    // per-page-unique short line rides along — the furniture-rule armor,
    // same as the pdfium run above)
    const PAGES: usize = 300;
    let paragraph_for = |i: usize| {
        format!(
            "{} {}",
            unique_head(i),
            "lorem ipsum dolor sit amet consectetur ".repeat(17)
        )
    };
    let pdf_bytes = build_multipage_pdf(PAGES, paragraph_for);
    let x = PdfInspectorExtractor::new();
    let parts = x
        .extract(&pdf_bytes, "application/pdf")
        .expect("generated PDF must extract");
    assert_eq!(parts.len(), 1);
    for probe in [1usize, 42, 150, PAGES] {
        assert!(
            parts[0]
                .text
                .contains(&format!("page {probe:04} of {PAGES:04}")),
            "page {probe} marker missing"
        );
        // the per-page-unique paragraph head survived the pure-Rust engine
        // too (the furniture-rule armor, same pin as the pdfium run)
        assert!(
            parts[0].text.contains(&format!("golden body {probe:04} ")),
            "page {probe} unique body head missing"
        );
    }
    // determinism on a large document
    let again = x.extract(&pdf_bytes, "application/pdf").unwrap();
    assert_eq!(parts, again);

    // and through the REGISTERED config route (the ingest worker's
    // construction path), both flavors:
    let raw = toml::from_str::<toml::Value>("kind = \"pdf-inspector\"").unwrap();
    let kind = vectorfs_server::pipeline::extract::ExtractorKind::from_config("golden", &raw, None)
        .unwrap();
    let via_enum = kind.extract(&pdf_bytes, "application/pdf").unwrap();
    assert_eq!(via_enum.len(), 1);
    assert!(via_enum[0].text.contains("page 0001 of 0300"));
    let raw_md =
        toml::from_str::<toml::Value>("kind = \"pdf-inspector\"\nmarkdown = true").unwrap();
    let kind_md =
        vectorfs_server::pipeline::extract::ExtractorKind::from_config("golden", &raw_md, None)
            .unwrap();
    let via_enum_md = kind_md.extract(&pdf_bytes, "application/pdf").unwrap();
    // RECORDED behavior of pdf-inspector 1.25.2's markdown pipeline on these
    // minimal synthetic pages, re-probed for the T12 fixture rework: the
    // pre-T12 all-12pt shape emitted only page-marker text (bodies dropped;
    // page 0001 vanished — the two-same-size-lines empty-page behavior, see
    // build_multipage_pdf's doc). With the 10pt body + per-page-unique
    // paragraphs the pages carry real content; the honest assertions stay
    // conservative (non-empty + markers through the registered route); the
    // markdown CONTENT path with real structure is covered on hello.pdf.
    assert!(!via_enum_md[0].text.is_empty());
    assert!(via_enum_md[0].text.contains("page 0002 of 0300"));
}
