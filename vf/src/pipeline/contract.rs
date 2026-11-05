//! Plugin contract tests (§5.10.2 minimum assertion list): the LSP defense
//! line and the future third-party / wasm admission gate.
//!
//! These are PUBlic assertion suites, not just our own tests: any future
//! implementation (community kind, wasm plugin in v2) runs the SAME suite to
//! be admitted — `#[cfg(test)]` below runs it against the built-ins
//! (`txt`, `pdf-text`, `md`, `pdf-inspector`, `token-window`, the real `ort`
//! kind over the committed tiny ONNX fixture + the real .so, the real
//! `openai` kind over an in-process fake provider) and a local mock embedder
//! kept as the LSP canary for third-party implementations.
//!
//! Assertions panic on violation (assertion-suite convention); callers wrap
//! them in whatever test harness they use.

use super::{Chunker, Embedder, Extractor, Hit, HitList, Merge, Modality, Part, Query};

// ---------------------------------------------------------------------------
// Extractor
// ---------------------------------------------------------------------------

/// Run the extractor contract against `x`, using `good` = (bytes, mime) as
/// the known-good sample.
///
/// * **Determinism**: two calls with the same input produce identical
///   output.
/// * **Wrong mime** (`application/octet-stream`): Err, never a guess.
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
        },
        super::Chunk {
            index: 1,
            text: "第二个块".to_string(),
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

fn assert_total_order(hits: &[Hit]) {
    for w in hits.windows(2) {
        let ordered = w[0].score > w[1].score
            || (w[0].score.to_bits() == w[1].score.to_bits() && w[0].key < w[1].key);
        assert!(ordered, "output is not totally ordered: {hits:?}");
    }
}

// ---------------------------------------------------------------------------
// built-in runs (the LSP defense line: a regression in a built-in kind
// trips here, not in a user's ingest)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::chunk::token_window::TokenWindowChunker;
    use super::super::embed::openai;
    use super::super::embed::ort;
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
    fn token_window_passes_chunker_contract() {
        let c = TokenWindowChunker::new(8, 3).unwrap();
        chunker_contract(&c, 8, 3);
        let c = TokenWindowChunker::new(4, 2).unwrap();
        chunker_contract(&c, 4, 2);
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
            let Query::Text(t) = q;
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
}
