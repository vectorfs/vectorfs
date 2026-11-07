//! Plugin contract tests (§5.10.2 minimum assertion list): the LSP defense
//! line and the future third-party / wasm admission gate.
//!
//! These are PUBlic assertion suites, not just our own tests: any future
//! implementation (community kind, wasm plugin in v2) runs the SAME suite to
//! be admitted — `#[cfg(test)]` below runs it against the built-ins
//! (`txt`, `pdf-text`, `md`, `pdf-inspector`, `pdf-pages`, `docx` and `html`
//! (M3-T6), `image` through the image-extractor twin, `token-window` and
//! `markdown` (M3-T6), the real `ort` kind over the committed tiny ONNX
//! fixture + the real .so, the real `openai` kind over an in-process fake
//! provider, the real `siglip2` kind over its committed tiny fixture + the
//! real .so via the image-route twin) and a local mock embedder kept as the
//! LSP canary for third-party implementations.
//!
//! Assertions panic on violation (assertion-suite convention); callers wrap
//! them in whatever test harness they use.
//!
//! Note on M2.1 fallback chains: an ordered `extract` chain is INGEST
//! ORCHESTRATION layered on top of these contracts (the worker tries
//! members in order; see `ingest::pick_from_chain`) — the trait and this
//! suite are untouched by it, which is the point: a kind admitted here
//! composes into any chain with zero framework changes.

use super::{
    ChunkKind, Chunker, Embedder, Extractor, Hit, HitList, LabelSpan, Merge, Modality, Part, Query,
};

// ---------------------------------------------------------------------------
// Extractor
// ---------------------------------------------------------------------------

/// Run the extractor contract against `x`, using `good` = (bytes, mime) as
/// the known-good sample.
///
/// * **Determinism**: two calls with the same input produce identical
///   output.
/// * **Wrong mime** (`application/octet-stream`): Err, never a guess.
/// * **Empty mime** (`""`): TOLERATED without panic — "unspecified" means
///   the bytes are the truth (the uniform gate every built-in kind opens
///   with, `pipeline::gate`); the kind may still Err on the bytes.
/// * **Empty file**: Err (uniform empty-source handling — see txt.rs for
///   the rationale; every registered kind must match it).
/// * **Truncated bytes** (first half of `good`): must not panic, and must
///   be Err whenever the truncation is detectable. Kinds with
///   self-describing binary framing (pdf) always detect it; `txt` detects
///   it when the cut splits a UTF-8 sequence — the built-in test sample is
///   crafted so it does. An Ok on truncation is only acceptable when the
///   truncated bytes are genuinely valid input for the kind.
/// * **Closed modality set**: every produced Part's modality is a variant
///   of [`Modality`] (trivially true in Rust — the closed enum IS the
///   guarantee; asserted anyway so the invariant is executable
///   documentation).
pub fn extractor_contract(x: &dyn Extractor, good: (&[u8], &str)) {
    let (bytes, mime) = good;

    // determinism
    let a = x.extract(bytes, mime).expect("good sample must extract");
    let b = x
        .extract(bytes, mime)
        .expect("good sample must extract (2nd)");
    assert_eq!(a, b, "extractor is not deterministic on its good sample");

    // non-empty output: a GOOD sample yields extracted content —
    // `Ok(vec![Part::text("")])` would silently index nothing and still pass
    // every other assertion (gap found by the 2026-11-05 pdf-inspector
    // plugin-soundness audit, closed here)
    assert!(
        !a.is_empty() && a.iter().any(|p| !p.text.is_empty()),
        "good sample must produce at least one non-empty text part"
    );

    // modality closed set (executable documentation; Rust enforces it)
    for part in &a {
        assert!(matches!(part.modality, Modality::Text));
    }

    // wrong mime -> Err
    assert!(
        x.extract(bytes, "application/octet-stream").is_err(),
        "wrong mime must Err, not guess"
    );

    // empty mime ("unspecified") -> tolerated, never a panic: the shared
    // gate's uniform rule (pipeline::gate) — the kind proceeds on the bytes;
    // Err on the bytes themselves is fine, a surprise rejection of the
    // UNSPECIFIED mime is not
    let _ = x.extract(bytes, "");

    // empty file -> Err (uniform contract; see txt.rs rationale)
    assert!(
        x.extract(b"", mime).is_err(),
        "empty input must Err, not return an empty Vec"
    );

    // truncated input: never panic (any panic below fails the suite), Err
    // when detectable
    let truncated = &bytes[..bytes.len() / 2];
    match x.extract(truncated, mime) {
        Err(_) => {}
        Ok(parts) => {
            for part in &parts {
                assert!(matches!(part.modality, Modality::Text));
            }
            // allowed only if the truncated bytes are valid for the kind
            // (txt with an ASCII-clean cut, e.g.)
        }
    }
}

// ---------------------------------------------------------------------------
// Chunker
// ---------------------------------------------------------------------------

/// Run the extractor contract against an IMAGE-output `x` (M3-T3: the twin
/// [`extractor_contract`] assumes text output — it asserts a non-empty TEXT
/// part and Text modality, so an image kind cannot run it; the same shape as
/// `embedder_contract` vs `image_embedder_contract`). `good` = (bytes, mime)
/// is the known-good sample.
///
/// Everything `extractor_contract` asserts, through the image channel:
///
/// * **Determinism**: two calls with the same input produce identical output
///   (byte-identical payloads — the encoder is part of the extractor).
/// * **Non-empty output with content**: at least one part carries an image
///   payload (the image twin of the non-empty-text-part assertion — a
///   `Vec::new()` Ok would silently index nothing).
/// * **Every part is `Modality::Image` with an EMPTY text channel**, the
///   payload's declared mime honest against the bytes' sniffed magic
///   ([`crate::pipeline::ImagePayload::validate`] — the same guard the
///   chunker bypass runs; a kind that emits garbage or a wrong mime must
///   fail HERE, not inside an embedder's preprocessing).
/// * **Wrong mime** (`application/octet-stream`): Err, never a guess.
/// * **Empty mime** (`""`): tolerated without panic.
/// * **Empty file**: Err (uniform empty-source handling).
/// * **Truncated bytes** (first half of `good`): never panic; PDF framing
///   is self-describing, so an Ok is only acceptable when the truncated
///   bytes are genuinely valid input for the kind (the truncation halves of
///   image fixtures Err in practice).
pub fn image_extractor_contract(x: &dyn Extractor, good: (&[u8], &str)) {
    let (bytes, mime) = good;

    // determinism (encoder included)
    let a = x.extract(bytes, mime).expect("good sample must extract");
    let b = x
        .extract(bytes, mime)
        .expect("good sample must extract (2nd)");
    assert_eq!(
        a, b,
        "image extractor is not deterministic on its good sample"
    );

    // non-empty output carrying image content
    assert!(
        !a.is_empty() && a.iter().any(|p| p.image.is_some()),
        "good sample must produce at least one image part"
    );

    // modality + channel pairing + bytes-vs-mime honesty per part
    for part in &a {
        assert!(matches!(part.modality, Modality::Image));
        assert!(part.text.is_empty(), "image parts carry no text");
        part.image
            .as_ref()
            .expect("image modality implies a payload")
            .validate()
            .expect("declared mime must match the sniffed container magic");
    }

    // wrong mime -> Err
    assert!(
        x.extract(bytes, "application/octet-stream").is_err(),
        "wrong mime must Err, not guess"
    );

    // empty mime ("unspecified") -> tolerated, never a panic
    let _ = x.extract(bytes, "");

    // empty file -> Err (uniform contract)
    assert!(
        x.extract(b"", mime).is_err(),
        "empty input must Err, not return an empty Vec"
    );

    // truncated input: never panic; Err unless genuinely valid
    let truncated = &bytes[..bytes.len() / 2];
    if let Ok(parts) = x.extract(truncated, mime) {
        for part in &parts {
            assert!(matches!(part.modality, Modality::Image));
        }
    }
}

/// Run the chunker contract against `c`, given its window parameters
/// `(size, overlap)` — the overlap-semantics assertions need the DECLARED
/// overlap (an empirically-inferred one is wrong on repetitive text: the
/// max-suffix-match over-finds, e.g. "aaaa…" windows).
///
/// Assertions (the concatenation invariant, spelled out):
/// 1. calling twice yields identical output (determinism);
/// 2. chunk indices are sequential from 0;
/// 3. adjacent chunks overlap exactly as declared: the last `overlap` CHARS
///    of chunk[i-1] equal the first `overlap` chars of chunk[i];
/// 4. **de-overlapped concatenation reconstructs the input text**: chunk[0]
///    in full, then chars(overlap..) of every following chunk (per part);
///    this is the property that makes chunk text lossless w.r.t. the
///    source, and — because equality is on Strings — it doubles as the
///    CJK/no-mid-glyph-cut proof;
/// 5. every chunk is within the size budget (chars);
/// 6. edge coverage: input shorter than the window, exactly one window,
///    exact multiples, and one-past-everything, plus CJK/mixed samples;
/// 7. empty inputs (no parts / empty text) yield no chunks and no panic.
pub fn chunker_contract(c: &dyn Chunker, size: usize, overlap: usize) {
    assert!(size > 0 && overlap < size, "fixture chunker params");

    let samples: Vec<String> = vec![
        String::new(),
        "x".repeat(1),
        "x".repeat(size.saturating_sub(1)), // shorter than one window
        "x".repeat(size),                   // exactly one window
        "x".repeat(size + 1),               // one past
        "x".repeat(size * 3),               // many
        "x".repeat(size * 3 + overlap + 1), // tail past an exact boundary
        "汉字".repeat(size),                // CJK, 3 bytes/char
        "a汉b汉c汉".repeat(size / 2 + 1),   // mixed, multibyte mid-text
    ];

    for text in &samples {
        let parts = vec![Part::text(text.clone())];
        let chunks = c.chunk(&parts).expect("chunking must not fail");
        assert_window_invariants(&chunks, text, size, overlap);

        // determinism
        let again = c.chunk(&parts).expect("chunking must not fail (2nd)");
        assert_eq!(chunks, again, "chunker is not deterministic");
    }

    // multi-part: indices continue sequentially; each part reconstructs
    let parts = vec![
        Part::text("a".repeat(size * 2)),
        Part::text("b".repeat(size)),
    ];
    let chunks = c.chunk(&parts).expect("multi-part chunking");
    let idx: Vec<u32> = chunks.iter().map(|ch| ch.index).collect();
    assert_eq!(idx, (0..chunks.len() as u32).collect::<Vec<_>>());
    let first_n = chunks
        .iter()
        .filter(|ch| ch.text.chars().all(|c| c == 'a') && !ch.text.is_empty())
        .count();
    let last = chunks
        .last()
        .expect("second part produces at least one chunk");
    assert!(last.text.chars().all(|c| c == 'b'));
    assert!(first_n >= 1);

    // empty inputs
    assert!(c.chunk(&[]).expect("no parts").is_empty());
    assert!(
        c.chunk(&[Part::text("")])
            .expect("empty text part")
            .is_empty()
    );
}

/// The per-text invariant core (3)+(4)+(5)+(2), shared by the contract and
/// the token-window unit tests' edge sweep.
fn assert_window_invariants(chunks: &[super::Chunk], text: &str, size: usize, overlap: usize) {
    if text.is_empty() {
        assert!(chunks.is_empty(), "empty text must produce no chunks");
        return;
    }
    assert!(!chunks.is_empty(), "non-empty text produced no chunks");
    // sequential indices
    let idx: Vec<u32> = chunks.iter().map(|ch| ch.index).collect();
    assert_eq!(idx, (0..chunks.len() as u32).collect::<Vec<_>>());

    // reconstruction: chunk[0] in full, chars(overlap..) for the rest
    let mut recon = String::new();
    for (i, ch) in chunks.iter().enumerate() {
        assert!(
            !ch.text.is_empty(),
            "empty chunk {i} — a 0-char chunk is not a chunk"
        );
        if i == 0 {
            recon.push_str(&ch.text);
        } else {
            // adjacent overlap alignment (chars, not bytes — CJK safety)
            let prev = &chunks[i - 1].text;
            let k = overlap.min(ch.text.chars().count());
            let suffix: String = prev.chars().skip(prev.chars().count() - k).collect();
            let prefix: String = ch.text.chars().take(k).collect();
            assert_eq!(suffix, prefix, "chunks {}/{} misaligned", i - 1, i);
            recon.extend(ch.text.chars().skip(overlap));
        }
        assert!(
            ch.text.chars().count() <= size,
            "chunk {i} exceeds window size"
        );
    }
    assert_eq!(&recon, text, "de-overlapped concatenation != input");
}

/// Run the chunker contract's T12 LABEL extension against `c` — the
/// closing of the gap the adversarial review found in the pre-T12 suite:
/// [`chunker_contract`] only ever constructs unlabeled parts, so a chunker
/// that IGNORED `Part::labels`/`Part::pages` entirely passed vacuously (the
/// LSP defense line was blind on the new axis). The fixture part carries
/// BOTH new channels; assertions, in the order the D-1 ruling demands:
///
/// 1. **forced window-breaking takes effect**: some window ends exactly at
///    each non-body span edge, and the SAME text chunked label-free breaks
///    nowhere near them (the contrast proves the bend comes from the
///    labels, not from a lucky cut-lattice coincidence);
/// 2. **the invariant battery still holds on the labeled output** — the
///    anti-「平接」 pin at contract level: a chunker that abuts windows at
///    the span edge (no overlap reach-back) drops `overlap` chars per
///    boundary from the reconstruction and fails here;
/// 3. **kind determination, all states**: a window wholly inside one kind's
///    span ⇒ that kind (pure Toc and pure Caption windows must both
///    exist); any window crossing a span edge — the mandated overlap
///    reach-back (o|T), or straddling two different spans (T|C) — ⇒ Body
///    (recall protection: 宁可漏排不可误排); unlabeled text ⇒ Body;
/// 4. **`pages` rides along inertly**: T13 consumes it, not the chunker —
///    identical chunk output with and without the page table;
/// 5. determinism on the labeled path, as everywhere else.
///
/// Fixture shape: `b1 = size + overlap + 1` unlabeled chars, then a Toc
/// span and a Caption span of `size + overlap + 2` chars each (long enough
/// that a window fits wholly inside — the pure-kind states are reachable),
/// then an unlabeled tail of the same length (the last span edge is never
/// swallowed by the tail window). Separator-free runs put the cut oracle on
/// its pure char-lattice fallback, so the geometry is deterministic for
/// both kinds regardless of their oracle.
pub fn chunker_contract_labels(c: &dyn Chunker, size: usize, overlap: usize) {
    assert!(size > 0 && overlap < size, "fixture chunker params");
    let b1 = size + overlap + 1;
    let seg = size + overlap + 2;
    let b2 = b1 + seg;
    let b3 = b2 + seg;
    let text = format!(
        "{}{}{}{}",
        "o".repeat(b1),
        "T".repeat(seg),
        "C".repeat(seg),
        "o".repeat(seg)
    );
    // byte ranges == char ranges (ASCII fixture); non-body spans only —
    // body is expressed by the absence of labels, the producer-side shape
    let labels = vec![
        LabelSpan {
            range: b1..b2,
            kind: ChunkKind::Toc,
        },
        LabelSpan {
            range: b2..b3,
            kind: ChunkKind::Caption,
        },
    ];
    // a realistic page table over the same text — must be inert (T13's)
    let pages = vec![(1u32, 0usize), (2, b1), (3, b2), (4, b3)];

    let labeled = Part {
        modality: Modality::Text,
        text: text.clone(),
        image: None,
        labels,
        pages,
    };
    let chunks = c
        .chunk(std::slice::from_ref(&labeled))
        .expect("labeled chunking");
    // (2) the full battery: alignment, lossless reconstruction, budgets,
    // sequential indices — under forced endpoints
    assert_window_invariants(&chunks, &text, size, overlap);

    // (1) forced breaking: window ends (derived chunk-side: each window
    // starts at the previous end - overlap) sit exactly on the span edges
    let mut ends = Vec::new();
    let mut start = 0usize;
    for ch in &chunks {
        let len = ch.text.chars().count();
        ends.push(start + len);
        start += len - overlap;
    }
    for (name, b) in [
        ("toc-open", b1),
        ("toc-close/caption-open", b2),
        ("caption-close", b3),
    ] {
        assert!(
            ends.contains(&b),
            "forced endpoint {name} {b} must bend some window end: {ends:?}"
        );
    }
    // the contrast: the same text without labels breaks at none of them
    let plain = c
        .chunk(&[Part::text(text.clone())])
        .expect("plain chunking");
    let mut plain_ends = Vec::new();
    let mut start = 0usize;
    for ch in &plain {
        let len = ch.text.chars().count();
        plain_ends.push(start + len);
        start += len - overlap;
    }
    for b in [b1, b2, b3] {
        assert!(
            !plain_ends.contains(&b),
            "boundary {b} bent the UNLABELED walk too — fixture broken: {plain_ends:?}"
        );
    }

    // (3) kind determination — every state present and correctly arbitrated
    let is_all = |ch: &super::Chunk, x: char| ch.text.chars().all(|c| c == x);
    assert!(
        chunks
            .iter()
            .any(|ch| is_all(ch, 'T') && ch.kind == ChunkKind::Toc),
        "a pure toc-span window must be judged Toc: {:?}",
        chunks.iter().map(|c| (&c.text, c.kind)).collect::<Vec<_>>()
    );
    assert!(
        chunks
            .iter()
            .any(|ch| is_all(ch, 'C') && ch.kind == ChunkKind::Caption),
        "a pure caption-span window must be judged Caption"
    );
    for (a, b) in [('o', 'T'), ('T', 'C'), ('C', 'o')] {
        assert!(
            chunks.iter().any(|ch| {
                ch.text.contains(a) && ch.text.contains(b) && ch.kind == ChunkKind::Body
            }),
            "a window crossing the {a}|{b} edge must arbitrate to Body (recall protection)"
        );
    }
    assert!(
        chunks
            .iter()
            .any(|ch| is_all(ch, 'o') && ch.kind == ChunkKind::Body),
        "unlabeled windows must stay Body"
    );

    // (4) pages are inert for the chunker: same chunks with them dropped
    let mut no_pages = labeled.clone();
    no_pages.pages = Vec::new();
    assert_eq!(
        c.chunk(&[no_pages]).expect("pages-free chunking"),
        chunks,
        "the page table must not change chunk output (T13's channel)"
    );
    // (5) determinism on the labeled path
    assert_eq!(c.chunk(&[labeled]).expect("labeled chunking (2nd)"), chunks);
}

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

/// Run the embedder contract against `e` (async: the query/document routes
/// are `async_trait` methods; await this in a test runtime or the future
/// admission harness).
///
/// * both routes return vectors of dimension exactly `e.dim()`;
/// * the query route called twice still reports `dim()` dimensions (the
///   contract pins DIMENSION identity, not bit-identical outputs — GPU
///   nondeterminism is legal, shape flapping is not);
/// * no NaN/Inf anywhere;
/// * `embed_documents(&[])` is `Ok(vec![])` — no panic, no phantom vector;
/// * modality declarations are non-empty on both sides (an embedder that
///   accepts nothing cannot be routed to).
pub async fn embedder_contract<E: Embedder + ?Sized>(e: &E) {
    let dim = e.dim();
    assert!(dim > 0, "declared dim must be positive");

    let q = Query::Text("contract probe".to_string());
    let v1 = e.embed_query(&q).await.expect("embed_query must succeed");
    assert_eq!(v1.len(), dim, "query route dim != declared dim");
    assert_finite(&v1, "query route");

    let v2 = e
        .embed_query(&q)
        .await
        .expect("embed_query (2nd) must succeed");
    assert_eq!(v2.len(), dim, "query route dim flaps across calls");

    let chunks = vec![
        super::Chunk {
            index: 0,
            text: "first chunk".to_string(),
            image: None,
            kind: ChunkKind::Body,
        },
        super::Chunk {
            index: 1,
            text: "第二个块".to_string(),
            image: None,
            kind: ChunkKind::Body,
        },
    ];
    let docs = e
        .embed_documents(&chunks)
        .await
        .expect("embed_documents must succeed");
    assert_eq!(docs.len(), chunks.len(), "one vector per chunk");
    for (i, v) in docs.iter().enumerate() {
        assert_eq!(v.len(), dim, "document route dim != declared (chunk {i})");
        assert_finite(v, "document route");
    }

    let empty = e
        .embed_documents(&[])
        .await
        .expect("empty batch must be Ok");
    assert!(empty.is_empty(), "empty batch must return no vectors");

    assert!(
        !e.doc_modalities().is_empty(),
        "embedder must accept at least one doc modality"
    );
    assert!(
        !e.query_modalities().is_empty(),
        "embedder must accept at least one query modality"
    );
}

fn assert_finite(v: &[f32], route: &str) {
    for (i, x) in v.iter().enumerate() {
        assert!(
            x.is_finite(),
            "{route} produced non-finite value at index {i}: {x}"
        );
    }
}

/// Run the embedder contract against an IMAGE-document embedder (M3-T2:
/// the `embedder_contract` twin for kinds whose doc side is not text —
/// siglip2 today). `png` is a known-good sample image (any valid PNG the
/// caller controls).
///
/// Everything `embedder_contract` asserts, through the image routes:
///
/// * image query twice → vectors of dimension exactly `e.dim()` (dimension
///   identity, not bit-identical outputs);
/// * image document batch → one vector per chunk, each `dim()`-dimensional,
///   no NaN/Inf; determinism (CPU inference must be reproducible);
/// * `embed_documents(&[])` is `Ok(vec![])`;
/// * doc modality declares [`Modality::Image`]; query declares
///   [`Modality::Image`] and, when it also declares Text (siglip2: the text
///   tower ships with the kind), a TEXT query embeds at `dim()` too.
pub async fn image_embedder_contract<E: Embedder + ?Sized>(e: &E, png: &[u8]) {
    let dim = e.dim();
    assert!(dim > 0, "declared dim must be positive");
    assert!(
        e.doc_modalities().contains(&Modality::Image),
        "an image-document embedder must declare Image on its doc side"
    );
    assert!(
        e.query_modalities().contains(&Modality::Image),
        "an image embedder without an image query side cannot serve 以图搜图"
    );

    let q = Query::Image(png.to_vec(), "image/png".to_string());
    let v1 = e.embed_query(&q).await.expect("image query must succeed");
    assert_eq!(v1.len(), dim, "image query dim != declared dim");
    assert_finite(&v1, "image query route");

    let v2 = e
        .embed_query(&q)
        .await
        .expect("image query (2nd) must succeed");
    assert_eq!(v2.len(), dim, "image query dim flaps across calls");

    let chunks = vec![
        super::Chunk {
            index: 0,
            text: String::new(),
            image: Some(super::ImagePayload {
                bytes: bytes::Bytes::copy_from_slice(png),
                mime: "image/png".to_string(),
            }),
            kind: ChunkKind::Body,
        },
        super::Chunk {
            index: 1,
            text: String::new(),
            image: Some(super::ImagePayload {
                bytes: bytes::Bytes::copy_from_slice(png),
                mime: "image/png".to_string(),
            }),
            kind: ChunkKind::Body,
        },
    ];
    let docs = e
        .embed_documents(&chunks)
        .await
        .expect("image documents must embed");
    assert_eq!(docs.len(), chunks.len(), "one vector per image chunk");
    for (i, v) in docs.iter().enumerate() {
        assert_eq!(v.len(), dim, "image doc dim != declared (chunk {i})");
        assert_finite(v, "image document route");
    }
    // determinism on CPU inference (stronger than the contract's dim-only
    // requirement, same pin as the mock embedder below)
    let again = e.embed_documents(&chunks).await.unwrap();
    assert_eq!(docs, again, "CPU image inference must be reproducible");

    let empty = e
        .embed_documents(&[])
        .await
        .expect("empty batch must be Ok");
    assert!(empty.is_empty(), "empty batch must return no vectors");

    // the text-query half when the query side declares Text (siglip2's text
    // tower): 文字搜图 through the same declared dim
    if e.query_modalities().contains(&Modality::Text) {
        let t = e
            .embed_query(&Query::Text("contract probe".to_string()))
            .await
            .expect("a Text-declaring query side must embed text queries");
        assert_eq!(t.len(), dim, "text query dim != declared dim");
        assert_finite(&t, "text query route");
    }
}

// ---------------------------------------------------------------------------
// Merge (query option)
// ---------------------------------------------------------------------------

/// Run the merge contract against [`merge_hitlists`] (the pure function; no
/// implementation slot to inject — alternatives like `weighted` get their
/// own run when they land in M3).
///
/// * single list: IDENTITY — same rows, same order, same scores;
/// * output is a deterministic total order: scores non-increasing,
///   equal-score runs ordered by key ascending (tie-break by key);
/// * key union preserved, no duplicates;
/// * empty input -> empty output.
pub fn merge_contract() {
    // identity on a single list
    let one: HitList = vec![
        Hit {
            key: "a".into(),
            score: 0.9,
        },
        Hit {
            key: "b".into(),
            score: 0.5000001,
        },
        Hit {
            key: "c".into(),
            score: -1.0,
        },
    ];
    let out = super::merge_hitlists(Merge::Rrf, &[one.clone()]);
    assert_eq!(out.len(), one.len());
    for (i, (got, want)) in out.iter().zip(&one).enumerate() {
        assert_eq!(got.key, want.key, "single list: order must be kept ({i})");
        assert_eq!(
            got.score.to_bits(),
            want.score.to_bits(),
            "single list: raw scores must be preserved ({i})"
        );
    }

    // fusion + tie-break + total order
    let l1: HitList = vec![
        Hit {
            key: "shared".into(),
            score: 9.0,
        },
        Hit {
            key: "only1".into(),
            score: 8.0,
        },
    ];
    let l2: HitList = vec![
        Hit {
            key: "only2".into(),
            score: 7.0,
        },
        Hit {
            key: "shared".into(),
            score: 6.0,
        },
    ];
    let out = super::merge_hitlists(Merge::Rrf, &[l1, l2]);
    // union, no duplicates
    let mut keys: Vec<&str> = out.iter().map(|h| h.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["only1", "only2", "shared"]);
    // 'shared' (rank1+rank2) outranks only2 (rank1) outranks only1 (rank2)
    assert_eq!(out[0].key, "shared");
    assert_eq!(out[1].key, "only2");
    assert_eq!(out[2].key, "only1");
    assert_total_order(&out);

    // constructed tie: rank-1 in disjoint lists -> equal fused scores ->
    // key ascending decides
    let tie = super::merge_hitlists(
        Merge::Rrf,
        &[
            vec![Hit {
                key: "zz".into(),
                score: 1.0,
            }],
            vec![Hit {
                key: "aa".into(),
                score: 1.0,
            }],
        ],
    );
    assert_eq!(tie[0].score.to_bits(), tie[1].score.to_bits());
    assert_eq!(tie[0].key, "aa", "tie must break by key ascending");
    assert_eq!(tie[1].key, "zz");

    // empty inputs
    assert!(super::merge_hitlists(Merge::Rrf, &[]).is_empty());
    assert!(super::merge_hitlists(Merge::Rrf, &[vec![], vec![]]).is_empty());
}

/// The M3-T8 weighted run of the merge contract against
/// [`super::merge_hitlists_weighted`] — same assertions the rrf run makes
/// (§5.10.2 contract minimums: single-route identity, deterministic total
/// order with tie-break by key, union preserved, empty -> empty), plus the
/// weighted-specific property: uniform/absent weights are BIT-identical to
/// rrf (the additive red line in one assert).
pub fn weighted_merge_contract() {
    use std::collections::BTreeMap;

    // identity on a single list (weight present but meaningless)
    let one: HitList = vec![
        Hit {
            key: "a".into(),
            score: 0.9,
        },
        Hit {
            key: "b".into(),
            score: 0.5000001,
        },
    ];
    let out =
        super::merge_hitlists_weighted(&[("e", &one)], &BTreeMap::from([("e".to_string(), 5.0)]));
    assert_eq!(out.len(), one.len());
    for (got, want) in out.iter().zip(&one) {
        assert_eq!(got.key, want.key);
        assert_eq!(got.score.to_bits(), want.score.to_bits());
    }

    // fusion + total order + union (asymmetric weights exercising the
    // per-name lookup and the neutral 1.0 default)
    let l1: HitList = vec![
        Hit {
            key: "shared".into(),
            score: 9.0,
        },
        Hit {
            key: "only1".into(),
            score: 8.0,
        },
    ];
    let l2: HitList = vec![
        Hit {
            key: "only2".into(),
            score: 7.0,
        },
        Hit {
            key: "shared".into(),
            score: 6.0,
        },
    ];
    let out = super::merge_hitlists_weighted(
        &[("e1", &l1), ("e2", &l2)],
        &BTreeMap::from([("e1".to_string(), 3.0)]), // e2 defaults to 1.0
    );
    let mut keys: Vec<&str> = out.iter().map(|h| h.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["only1", "only2", "shared"]);
    // shared = 3/61 + 1/62 ≈ .0653; only1 = 3/62 ≈ .0484; only2 = 1/61 ≈
    // .0164 — that order (the 3x weight lifts e1's rank-2 over e2's rank-1)
    assert_eq!(
        out.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["shared", "only1", "only2"]
    );
    assert_total_order(&out);

    // the additive red line: uniform weights (explicit 1.0 or absent) are
    // byte-identical to the rrf fusion
    let rrf = super::merge_hitlists(Merge::Rrf, &[l1.clone(), l2.clone()]);
    for weights in [
        BTreeMap::from([("e1".to_string(), 1.0), ("e2".to_string(), 1.0)]),
        BTreeMap::new(),
    ] {
        let wgt = super::merge_hitlists_weighted(&[("e1", &l1), ("e2", &l2)], &weights);
        assert_eq!(wgt.len(), rrf.len());
        for (r, g) in rrf.iter().zip(&wgt) {
            assert_eq!(r.key, g.key);
            assert_eq!(r.score.to_bits(), g.score.to_bits());
        }
    }

    // constructed tie: symmetric weights, rank-1 in disjoint lists -> equal
    // scores -> key ascending decides
    let lz: HitList = vec![Hit {
        key: "zz".into(),
        score: 1.0,
    }];
    let la: HitList = vec![Hit {
        key: "aa".into(),
        score: 1.0,
    }];
    let tie = super::merge_hitlists_weighted(
        &[("e1", &lz), ("e2", &la)],
        &BTreeMap::from([("e1".to_string(), 2.0), ("e2".to_string(), 2.0)]),
    );
    assert_eq!(tie[0].score.to_bits(), tie[1].score.to_bits());
    assert_eq!(tie[0].key, "aa");
    assert_eq!(tie[1].key, "zz");

    // empty inputs
    let empty: HitList = Vec::new();
    assert!(super::merge_hitlists_weighted(&[], &BTreeMap::new()).is_empty());
    assert!(
        super::merge_hitlists_weighted(&[("e1", &empty), ("e2", &empty)], &BTreeMap::new())
            .is_empty()
    );
}

fn assert_total_order(hits: &[Hit]) {
    for w in hits.windows(2) {
        let ordered = w[0].score > w[1].score
            || (w[0].score.to_bits() == w[1].score.to_bits() && w[0].key < w[1].key);
        assert!(ordered, "output is not totally ordered: {hits:?}");
    }
}

// ---------------------------------------------------------------------------
// Reranker (M3-T11, §5.10.2 "Reranker（M3 补）" contract minimums)
// ---------------------------------------------------------------------------

/// Run the reranker contract against `r` (async: the axis method is an
/// `async_trait` method). §5.10.2's minimum assertions for the fourth
/// axis, made executable:
///
/// * **分数全序确定** (deterministic total order): scores non-increasing,
///   equal-score runs ordered by key ascending — the `merge_contract`
///   tie-break discipline, checked by the same helper;
/// * **tie-break by key**: duplicates that SCORE identically (an empty
///   second member, e.g.) still come back in a fixed key order;
/// * **candidates 顺序不影响重排结果** (order independence — the axis's
///   headline numeric-determinism clause): the same candidate SET in a
///   PERMUTED order produces the byte-identical output (same keys, same
///   scores, same order), twice over (determinism of the determinism);
/// * **rerank re-orders, never filters**: one hit per input candidate,
///   the union preserved, no duplicates invented;
/// * no NaN/Inf scores; empty input → empty output.
pub async fn reranker_contract<R: super::Reranker + ?Sized>(r: &R) {
    // a candidate set with a real score spread + a forced near-tie shape
    // (two empty texts score identically; the tie-break must order them)
    let cands: Vec<super::RerankCandidate> = [
        ("k-d", "the quick brown fox embeds"),
        ("k-a", "alpha contract probe"),
        ("k-e", ""),
        ("k-b", "beta second chunk embeds the fox"),
        ("k-c", ""),
        ("k-f", "vectorfs"),
    ]
    .iter()
    .map(|(k, t)| super::RerankCandidate::new(*k, *t))
    .collect();

    let out = r.rerank("contract probe", &cands).await.expect("rerank");
    // re-orders, never filters: same key set
    let mut keys: Vec<&str> = out.iter().map(|h| h.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["k-a", "k-b", "k-c", "k-d", "k-e", "k-f"]);
    // deterministic total order (score desc, tie-break by key asc)
    assert_total_order(&out);
    for h in &out {
        assert!(h.score.is_finite(), "non-finite rerank score: {h:?}");
    }

    // ORDER INDEPENDENCE (the axis's own clause): permuted input ->
    // byte-identical output. Attention-masked rows are independent, but
    // float reduction order varies with batch SHAPE — implementations
    // must pin the batch composition to the candidate SET (the built-in
    // key-sorts before packing), and this assertion is the pin.
    let mut perm: Vec<super::RerankCandidate> = cands.clone();
    perm.reverse();
    let out_rev = r.rerank("contract probe", &perm).await.expect("rerank rev");
    assert_eq!(out, out_rev, "reversed candidates changed the result");
    let rot = vec![
        cands[3].clone(),
        cands[0].clone(),
        cands[5].clone(),
        cands[2].clone(),
        cands[4].clone(),
        cands[1].clone(),
    ];
    let out_rot = r.rerank("contract probe", &rot).await.expect("rerank rot");
    assert_eq!(out, out_rot, "rotated candidates changed the result");
    // and determinism of the determinism: same call again, same output
    let again = r
        .rerank("contract probe", &cands)
        .await
        .expect("rerank 2nd");
    assert_eq!(out, again);

    // a different QUERY is allowed to change the scores (it must not
    // change the key set)
    let other_q = r
        .rerank("a very different query", &cands)
        .await
        .expect("rerank q2");
    let mut keys2: Vec<&str> = other_q.iter().map(|h| h.key.as_str()).collect();
    keys2.sort_unstable();
    assert_eq!(keys, keys2, "query changes scores, never the key set");
    assert_total_order(&other_q);

    // empty input -> empty output, no panic
    assert!(r.rerank("q", &[]).await.expect("empty rerank").is_empty());
    // single candidate: passthrough shape
    let one = r
        .rerank("q", &[super::RerankCandidate::new("only", "one doc")])
        .await
        .expect("single rerank");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].key, "only");
    assert!(one[0].score.is_finite());
}

// ---------------------------------------------------------------------------
// built-in runs (the LSP defense line: a regression in a built-in kind
// trips here, not in a user's ingest)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::chunk::markdown::MarkdownChunker;
    use super::super::chunk::token_window::TokenWindowChunker;
    use super::super::embed::openai;
    use super::super::embed::ort;
    use super::super::embed::siglip2;
    use super::super::extract::md::MdExtractor;
    use super::super::extract::pdf_inspector::PdfInspectorExtractor;
    use super::super::extract::pdf_text;
    use super::super::extract::txt::TxtExtractor;
    use super::super::{Chunk, PipelineError};
    use super::*;
    use sha2::Digest;

    #[test]
    fn txt_passes_extractor_contract() {
        // good sample crafted so its byte-half splits the multibyte 'ö' —
        // truncation is then detectable and must Err (see contract docs)
        let good = "hello wörld — truncated cut lands mid-glyph hére";
        extractor_contract(&TxtExtractor, (good.as_bytes(), "text/plain"));
    }

    #[test]
    fn pdf_text_passes_extractor_contract() {
        // real .so required: the locator panics with fetch guidance rather
        // than skipping (honest failure, M2-T4 rule)
        let x = pdf_text::tests::extractor_or_panic();
        extractor_contract(&x, (pdf_text::tests::HELLO_PDF_MINIMAL, "application/pdf"));
    }

    #[test]
    fn md_passes_extractor_contract() {
        // CJK-dense so the byte-half cut is guaranteed to split a multibyte
        // char -> non-UTF-8 -> detectable truncation -> must Err
        let good = "# 标题\n\n汉字汉字汉字汉字汉字汉字汉字汉字";
        extractor_contract(&MdExtractor, (good.as_bytes(), "text/markdown"));
    }

    #[test]
    fn pdf_inspector_passes_extractor_contract() {
        // pure-Rust engine: no .so locator, nothing to fetch — the same
        // suite, zero framework changes (the reuse claim, executed)
        extractor_contract(
            &PdfInspectorExtractor::new(),
            (
                super::super::extract::pdf_inspector::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
            ),
        );
    }

    #[test]
    fn pdf_pages_passes_image_extractor_contract() {
        // real .so required (the pdf-text rule: locator panics with fetch
        // guidance rather than skipping); the IMAGE twin because every Part
        // this kind emits is Modality::Image with a PNG payload
        let x = super::super::extract::pdf_pages::tests::extractor_or_panic(150);
        image_extractor_contract(
            &x,
            (
                super::super::extract::pdf_pages::tests::HELLO_PDF_MINIMAL,
                "application/pdf",
            ),
        );
    }

    #[test]
    fn image_kind_passes_image_extractor_contract() {
        // the M3-T4 image FILE kind through the same twin (configless, no
        // .so — the pure-Rust image crate); the sample is an in-test PNG so
        // the truncation half is genuinely undecodable
        image_extractor_contract(
            &super::super::extract::image::ImageExtractor,
            (
                super::super::extract::image::tests::static_png(),
                "image/png",
            ),
        );
    }

    #[test]
    fn docx_kind_passes_extractor_contract() {
        // M3-T6: pure-Rust kinds (zip + quick-xml, no .so) through the
        // standard suite; the sample is CJK-dense so the truncation half
        // cuts the zip's central directory — detected, Err
        extractor_contract(
            &super::super::extract::docx::DocxExtractor,
            (
                super::super::extract::docx::tests::minimal_docx(&[
                    "汉字段落内容一，足够长以覆盖截断。",
                    "second paragraph with latin text",
                ])
                .as_slice(),
                super::super::extract::docx::tests::DOCX_MIME,
            ),
        );
    }

    #[test]
    fn html_kind_passes_extractor_contract() {
        // M3-T6: html5ever recovery makes truncated halves legal HTML with
        // partial text (the contract's Ok-on-truncation allowance — HTML5
        // has no framing to detect), and the tag-soup body exercises the
        // parser's recovery paths inside a contract run
        extractor_contract(
            &super::super::extract::html::HtmlExtractor,
            (
                "<html><head><title>t</title></head><body><h1>标题</h1><script>if (a<b) {}\
                 </script><p>汉字正文段落，含<b>加粗</b>与 AT&T。</p></body></html>"
                    .as_bytes(),
                "text/html",
            ),
        );
    }

    #[test]
    fn token_window_passes_chunker_contract() {
        let c = TokenWindowChunker::new(8, 3).unwrap();
        chunker_contract(&c, 8, 3);
        chunker_contract_labels(&c, 8, 3);
        let c = TokenWindowChunker::new(4, 2).unwrap();
        chunker_contract(&c, 4, 2);
        chunker_contract_labels(&c, 4, 2);
    }

    #[test]
    fn markdown_chunker_passes_chunker_contract() {
        // M3-T6: the axis's second kind through the SAME suite — the
        // concatenation/overlap/budget invariants are kind-independent
        // (the shared windowing recurrence), so the markdown oracle must
        // satisfy them on the contract's plain/CJK samples too; the T12
        // label extension rides along: forced endpoints and kind judgment
        // live in the shared windowing seam, so the second kind must pass
        // them with only its own cut oracle differing
        let c = MarkdownChunker::new(8, 3).unwrap();
        chunker_contract(&c, 8, 3);
        chunker_contract_labels(&c, 8, 3);
        let c = MarkdownChunker::new(4, 2).unwrap();
        chunker_contract(&c, 4, 2);
        chunker_contract_labels(&c, 4, 2);
    }

    #[tokio::test]
    async fn ort_kind_passes_embedder_contract() {
        // the REAL ort kind against the committed tiny fixture + the real
        // .so (locator panics with fetch guidance if missing — same
        // honest-failure rule as pdfium)
        let e = ort::tests::embedder_or_panic().await;
        embedder_contract(&e).await;
    }

    #[tokio::test]
    async fn openai_kind_passes_embedder_contract() {
        // the REAL openai kind against an in-process fake provider
        let (base, _fake) = openai::tests::fake_openai().await;
        let e = super::super::embed::openai::OpenAiEmbedder::new(&openai::tests::fake_config(base))
            .unwrap();
        embedder_contract(&e).await;
    }

    /// b1 (2026-11-07 §0 裁决): the openai kind with `multimodal = true`
    /// is a UNIFIED embedder — it passes BOTH contract suites against the
    /// same instance: the text routes (`embedder_contract`) AND the image
    /// routes (`image_embedder_contract`). That bilateral pass is the
    /// admission-level statement of the unified route (doc side [Text,
    /// Image], query side [Text, Image] — text AND image documents embed,
    /// text AND image queries embed, one declared dim for all four).
    #[tokio::test]
    async fn openai_multimodal_kind_passes_both_embedder_contracts() {
        let (base, _fake) = openai::tests::fake_openai().await;
        let e = super::super::embed::openai::OpenAiEmbedder::new(
            &openai::tests::fake_multimodal_config(base, None, None),
        )
        .unwrap();
        // the unified modality matrix, pinned as the admission view
        assert_eq!(e.doc_modalities(), &[Modality::Text, Modality::Image]);
        assert_eq!(e.query_modalities(), &[Modality::Text, Modality::Image]);
        embedder_contract(&e).await;
        image_embedder_contract(&e, &contract_png()).await;
    }

    /// A solid-color PNG encoded in-test (the contract's known-good image
    /// sample — no committed image bytes needed).
    fn contract_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(16, 12, image::Rgb([200, 30, 90]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn siglip2_kind_passes_image_embedder_contract() {
        // the REAL siglip2 kind against the committed tiny fixture + the
        // real .so — through the IMAGE routes (the text-contract twin above
        // assumes text documents; siglip2 refuses them loudly by design)
        let e = siglip2::tests::embedder_or_panic().await;
        image_embedder_contract(&e, &contract_png()).await;
        // modality declarations, pinned as the admission-suite view (doc =
        // Image; query = Image + Text — the text tower ships with the kind)
        assert_eq!(e.doc_modalities(), &[Modality::Image]);
        assert_eq!(e.query_modalities(), &[Modality::Image, Modality::Text]);
    }

    /// Deterministic pseudo-reranker: score = sha256(key) -> f64 — a THIRD
    /// grade of test double alongside the real kind: the LSP canary for
    /// third-party rerankers (the ShaEmbedder precedent), proving the
    /// contract suite passes an implementation with QUITE different score
    /// semantics (query-independent, key-only) while demanding exactly the
    /// axis's order/tie/union discipline.
    struct ShaReranker;

    #[async_trait::async_trait]
    impl crate::pipeline::Reranker for ShaReranker {
        async fn rerank(
            &self,
            _query: &str,
            candidates: &[crate::pipeline::RerankCandidate],
        ) -> Result<Vec<Hit>, PipelineError> {
            use sha2::Digest;
            let mut hits: Vec<Hit> = candidates
                .iter()
                .map(|c| {
                    let b = sha2::Sha256::digest(c.key.as_bytes());
                    let n = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                    Hit {
                        key: c.key.clone(),
                        score: (n % 10_000) as f64 / 10_000.0,
                    }
                })
                .collect();
            hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
            Ok(hits)
        }
    }

    #[tokio::test]
    async fn mock_reranker_passes_reranker_contract() {
        reranker_contract(&ShaReranker).await;
    }

    #[tokio::test]
    async fn cross_encoder_kind_passes_reranker_contract() {
        // the REAL cross-encoder kind against the committed tiny fixture +
        // the real .so (locator panics with fetch guidance if missing)
        let r = crate::pipeline::rerank::cross_encoder::tests::reranker_or_panic().await;
        reranker_contract(&r).await;
    }

    /// Deterministic pseudo-embedding: sha256(dim_index + text) -> one f32
    /// in [-1, 1]. Kept alongside the real kinds: it is the LSP canary for
    /// THIRD-PARTY implementations (the admission gate future kinds/wasm
    /// plugins must pass) — the real kinds do not make it redundant.
    struct ShaEmbedder {
        dim: usize,
    }

    fn pseudo(text: &str, slot: usize, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|i| {
                let mut h = sha2::Sha256::new();
                h.update(i.to_le_bytes());
                h.update((slot as u32).to_le_bytes());
                h.update(text.as_bytes());
                let b = h.finalize();
                let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                n as f32 / u32::MAX as f32 * 2.0 - 1.0
            })
            .collect()
    }

    #[async_trait::async_trait]
    impl Embedder for ShaEmbedder {
        async fn embed_query(&self, q: &Query) -> Result<Vec<f32>, PipelineError> {
            // a THIRD-PARTY canary: text-only implementations refuse
            // non-text queries loudly (the M3-T1 seam — same refusal shape
            // as the built-in kinds, via the shared helper)
            let Query::Text(t) = q else {
                return Err(crate::pipeline::embed::query_modality_refusal(q));
            };
            Ok(pseudo(t, 0, self.dim))
        }

        async fn embed_documents(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError> {
            Ok(chunks
                .iter()
                .map(|c| pseudo(&c.text, 1, self.dim))
                .collect())
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn doc_modalities(&self) -> &'static [Modality] {
            &[Modality::Text]
        }

        fn query_modalities(&self) -> &'static [Modality] {
            &[Modality::Text]
        }
    }

    #[tokio::test]
    async fn mock_embedder_passes_embedder_contract() {
        embedder_contract(&ShaEmbedder { dim: 8 }).await;
        // mock sanity: deterministic by construction (stronger than the
        // contract's dim-only requirement — a real GPU kind may flake)
        let a = pseudo("x", 0, 4);
        let b = pseudo("x", 0, 4);
        assert_eq!(a, b);
        assert!(a.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn merge_function_passes_merge_contract() {
        merge_contract();
    }

    #[test]
    fn weighted_merge_function_passes_merge_contract() {
        weighted_merge_contract();
    }
}
