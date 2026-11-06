//! The lexical-search kernel seam (M3-T9, ADR 0002: "内核以 `/x-vf/` 查询面
//! 薄 trait 隔离，保留未来换核退路").
//!
//! Everything under `/x-vf/` that needs BM25 goes through
//! [`LexicalKernel`] — ONE trait, today exactly ONE production implementation
//! ([`LanceFts`], Lance's native FTS over the derived table's `text` column).
//! The point is the swap path, not abstraction for its own sake: if Lance's
//! FTS evolves incompatibly (or a future ruling goes back to a sidecar), the
//! `/x-vf` query face keeps its shape and only this module's binding changes.
//! The PROTOCOL face (QueryVectors) never touches this trait — it stays
//! pure-vector by the same ADR ruling.
//!
//! Deliberately NOT here: index management (`vectorfs optimize` builds/folds
//! the FTS index through `store::Storage::ensure_fts_index` directly — the
//! verb is an engine-side maintenance action, not a query-face concern).
//!
//! The boxed-future signature is what keeps the trait dyn-compatible
//! (`Arc<dyn LexicalKernel>`); native async-fn-in-trait is not yet
//! dyn-compatible, and this seam is exactly one method wide.

use crate::filter::Filter;
use crate::store::{KeywordHit, Storage, StoreError};

/// A lexical (BM25) kernel bound behind the `/x-vf/` query face.
pub trait LexicalKernel: Send + Sync + 'static {
    /// Kernel identity (observability/tests: error context, doctor lines).
    fn name(&self) -> &'static str;

    /// BM25 search over one derived index's chunk-text column.
    ///
    /// `filter` is the SAME `$`-operator filter the semantic routes compile
    /// (QueryVectors grammar); kernels apply it as a pre-/post-filter around
    /// their ranking, mirroring the vector routes' semantics. Results are
    /// (score desc, key asc) with raw kernel scores — see the implementor
    /// docs for score-comparability caveats.
    fn search<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        query: &'a str,
        top_k: usize,
        filter: Option<&'a Filter>,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>>;
}

/// The Lance native FTS kernel (ADR 0002 ruling): `Index::FTS` inverted
/// index + `full_text_search` BM25 over the derived table's `text` column,
/// engine-side — flat-scanned when no index exists yet (correct, slower),
/// indexed after `vectorfs optimize` built one. Tokenizer choice lives in
/// the `[ingest.<bucket>]` config and rides the optimize verb, not this
/// query path (queries tokenize with whatever the index was built with).
pub struct LanceFts;

impl LexicalKernel for LanceFts {
    fn name(&self) -> &'static str {
        "lance-fts"
    }

    fn search<'a>(
        &'a self,
        storage: &'a Storage,
        vb: &'a str,
        ix: &'a str,
        query: &'a str,
        top_k: usize,
        filter: Option<&'a Filter>,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
        Box::pin(async move { storage.keyword_search(vb, ix, query, top_k, filter).await })
    }
}

/// The kernel binding used by the `/x-vf` query face: one place to swap the
/// implementation (a future ruling changes exactly this function). The
/// kernel is stateless — it borrows the caller's storage — so there is
/// nothing to configure or inject at startup.
pub fn kernel() -> std::sync::Arc<dyn LexicalKernel> {
    std::sync::Arc::new(LanceFts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam itself: a scripted second kernel proves the trait is all the
    /// query face needs (the swap path is real, not decorative) — it answers
    /// from a fixed list without touching the engine.
    struct Scripted(Vec<KeywordHit>);

    impl LexicalKernel for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn search<'a>(
            &'a self,
            _storage: &'a Storage,
            _vb: &'a str,
            _ix: &'a str,
            _query: &'a str,
            _top_k: usize,
            _filter: Option<&'a Filter>,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<KeywordHit>, StoreError>> {
            Box::pin(async { Ok(self.0.clone()) })
        }
    }

    #[tokio::test]
    async fn the_seam_is_dyn_dispatchable_with_two_impls() {
        let k: std::sync::Arc<dyn LexicalKernel> = kernel();
        assert_eq!(k.name(), "lance-fts");
        // a second impl rides the same Arc<dyn> face (no engine involved)
        let want = crate::store::KeywordHit {
            key: "k".into(),
            score: 1.5,
            metadata: None,
        };
        let scripted: std::sync::Arc<dyn LexicalKernel> = std::sync::Arc::new(Scripted(vec![want]));
        assert_eq!(scripted.name(), "scripted");
        // dispatch through the trait object against a memory storage that is
        // never opened (the scripted kernel ignores it)
        let storage = crate::store::Storage::memory();
        let got = scripted
            .search(&storage, "vb", "ix", "q", 10, None)
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "k");
    }
}
