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

use vectorfs::pipeline::chunk::ChunkerKind;
use vectorfs::pipeline::chunk::token_window::TokenWindowChunker;
use vectorfs::pipeline::contract::chunker_contract;
use vectorfs::pipeline::extract::md::MdExtractor;
use vectorfs::pipeline::extract::pdf_text::{PdfTextExtractor, locate_library};
use vectorfs::pipeline::extract::txt::TxtExtractor;
use vectorfs::pipeline::{Chunk, Chunker, Extractor, Modality, Part};

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
/// the same paragraph plus its own page marker. Objects, exact `/Length`s
/// and a correct xref table are assembled with byte offsets — the same
/// hand-assembly discipline as the committed hello.pdf fixture.
fn build_multipage_pdf(pages: usize, paragraph: &str) -> Vec<u8> {
    let escape = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)")
    };
    let content = |i: usize| {
        format!(
            "BT /F1 12 Tf 72 720 Td (page {:04} of {:04}) Tj 0 -20 Td ({}) Tj ET\n",
            i + 1,
            pages,
            escape(paragraph)
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
        let kind = vectorfs::pipeline::extract::ExtractorKind::from_config(
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
    // concatenation" huge shape, generated at test time.
    const PAGES: usize = 300;
    let paragraph = std::iter::repeat("lorem ipsum dolor sit amet consectetur ")
        .take(18)
        .collect::<String>();
    let pdf_bytes = build_multipage_pdf(PAGES, &paragraph);
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
