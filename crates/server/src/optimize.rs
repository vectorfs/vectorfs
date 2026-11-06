//! `vectorfs optimize` — the explicit index-management verb (M3-T8; design
//! appendix E.4 reserves exactly this face: "Lance 压缩 / IVF_PQ（M3）").
//!
//! Scope ruling (recorded in design.md §0): this verb delivers the IVF_PQ
//! half — build (train) the IVF_PQ vector index of an index, or fold the
//! rows written since the last pass into an existing one. Fragment
//! COMPACTION (the "Lance 压缩" half of the appendix line) stays
//! unimplemented: no user evidence of fragment pain at tested scales, and
//! compaction rewrites data files — a different risk class than building a
//! side index. Add it behind the same verb when evidence arrives.
//!
//! M3-T9 (ADR 0002) adds the LEXICAL half on the same verb: for buckets
//! with `fts = true`, migrate each derived table (add the `text` column on
//! pre-T9 tables, backfill NULLs from the metadata blob) and build/fold
//! the FTS inverted index with the bucket's tokenizer. Why this verb and
//! not table creation: (a) T8's discipline — the daemon never builds
//! indexes on its own, and the ingest worker IS the daemon; (b) FTS is
//! config-carrying (tokenizer), so creation must be gated on the bucket's
//! `[ingest.<bucket>]` config which only this sweep reads; (c) unlike
//! IVF_PQ there is NO training gate — an inverted index builds at any row
//! count, and until the verb runs keyword queries stay CORRECT on lance's
//! flat BM25 scan (the index buys speed, not correctness).
//!
//! Discipline (the M1 `optimize` ruling, design.md §6): explicit management
//! verb, NEVER a background task — the daemon never builds indexes on its
//! own. IVF_PQ is an index PHYSICAL form, not pipeline semantics: zero
//! config keys (nothing in vectorfs.toml, nothing in the pipeline
//! fingerprint), zero protocol-face surface. Until the verb runs, every
//! query stays on the flat exact path — byte-identical to pre-T8. The FTS
//! half is the deliberate exception on the config axis (an
//! availability-carrying knob, `[ingest.X] fts` + `fts-tokenizer`, the
//! latter in the pipeline fingerprint — 换分词 = reindex) while keeping the
//! verb discipline: same flock, same explicit run, zero background work.
//!
//! Single-writer: the bin takes the data root's local flock (the same one
//! `vectorfs serve`/`mcp` hold) for the duration; on backends without a
//! local home (memory/s3) it proceeds with an explicit warning — the
//! maintenance window is then the operator's guarantee.

use std::collections::BTreeMap;

use crate::pipeline::config::FtsTokenizer;
use crate::store::{DeploymentIdentity, FtsIndexOutcome, Storage, VectorIndexOutcome};

/// One index's result line.
#[derive(Debug, PartialEq)]
pub struct OptimizeLine {
    pub bucket: String,
    pub index: String,
    pub outcome: VectorIndexOutcome,
    /// The lexical pass (M3-T9): `Some` iff this target is a DERIVED index
    /// of an `fts = true` bucket — its migration (column + backfill) and
    /// FTS build/fold ran this pass.
    pub fts: Option<FtsIndexOutcome>,
    /// The tokenizer the FTS pass used (rendered on build lines; meaningful
    /// only when `fts.is_some()`).
    pub tokenizer: FtsTokenizer,
}

/// What `run` did. The bin turns this into stdout lines + an exit code:
/// an explicitly NAMED target below the training gate fails the run (the
/// operator asked for THAT index; the refusal IS the answer), while a sweep
/// merely skips young buckets loudly and continues.
#[derive(Debug)]
pub struct OptimizeReport {
    pub lines: Vec<OptimizeLine>,
}

impl OptimizeReport {
    /// True when a below-gate refusal must fail an EXPLICITLY targeted run.
    pub fn named_below_gate(&self, bucket: Option<&str>, index: Option<&str>) -> bool {
        index.is_some()
            && self.lines.iter().any(|l| {
                matches!(l.outcome, VectorIndexOutcome::BelowGate { .. })
                    && bucket.is_some_and(|b| b == l.bucket)
                    && index.is_some_and(|i| i == l.index)
            })
    }

    /// Human-readable result lines (stdout — results, not progress).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            let target = format!("{}/{}", l.bucket, l.index);
            match l.outcome {
                VectorIndexOutcome::Trained { rows } => out.push_str(&format!(
                    "{target}: ivf_pq trained on {rows} rows — queries now use the index \
                     (ANN; recall < exact)\n"
                )),
                VectorIndexOutcome::Folded { rows } => out.push_str(&format!(
                    "{target}: folded new rows into the existing ivf_pq index ({rows} rows \
                     total)\n"
                )),
                VectorIndexOutcome::BelowGate { rows } => out.push_str(&format!(
                    "{target}: SKIPPED — {rows} rows < {} (IVF_PQ training gate; queries stay \
                     flat exact)\n",
                    crate::store::IVF_PQ_MIN_TRAIN_ROWS
                )),
            }
            if let Some(fts) = l.fts {
                match fts {
                    crate::store::FtsIndexOutcome::Built { rows, backfilled } => {
                        out.push_str(&format!(
                            "{target}: fts index built on {rows} rows (tokenizer {}; {backfilled} \
                             chunk texts backfilled) — keyword/hybrid search now uses the \
                             index\n",
                            l.tokenizer
                        ))
                    }
                    crate::store::FtsIndexOutcome::Folded { rows, backfilled } => {
                        out.push_str(&format!(
                            "{target}: folded new rows into the existing fts index ({rows} rows \
                             total; {backfilled} chunk texts backfilled)\n"
                        ))
                    }
                    crate::store::FtsIndexOutcome::NoTable => out.push_str(&format!(
                        "{target}: fts SKIPPED — the derived table does not exist yet (nothing \
                         ingested); keyword search will work after the first ingest + optimize\n"
                    )),
                }
            }
        }
        out
    }
}

/// Run the pass. `identity` is the deployment identity (region/account
/// from the config file, exactly how status/doctor resolve it — the R9
/// ownership filter needs it). `bucket`/`index` narrow the target set
/// (appendix E.4 `optimize [--bucket] [--index]`; `--index` requires
/// `--bucket` — index names are per-bucket). Unknown names fail loudly with
/// what DOES exist.
///
/// `fts` (M3-T9) maps FTS-ENABLED bucket names to their tokenizer (the
/// bin's read of `[ingest.<bucket>] fts/fts-tokenizer`); buckets absent
/// from the map keep the pure-T8 pass. The lexical half only touches
/// DERIVED indexes (`x_vf_derived` stamped) — protocol-face indexes never
/// carry the ingest pipeline's chunk semantics.
pub async fn run(
    storage: &Storage,
    identity: &DeploymentIdentity,
    bucket: Option<&str>,
    index: Option<&str>,
    fts: &BTreeMap<String, FtsTokenizer>,
) -> Result<OptimizeReport, vectorfs_common::report::Failure> {
    if index.is_some() && bucket.is_none() {
        return Err(vectorfs_common::report::Failure::new(
            "USAGE",
            "--index requires --bucket (index names are scoped to a vector bucket)",
            "pass --bucket <vb> --index <name>, or drop --index to optimize the whole bucket",
        ));
    }

    // target enumeration: [(vb, index)] — sorted (list_buckets/list_indexes
    // already sort), so output is deterministic
    let mut targets: Vec<(String, String)> = Vec::new();
    let buckets = storage.list_buckets(identity).await.map_err(store_err)?;
    if let Some(want) = bucket {
        let meta = buckets
            .iter()
            .find(|b| b.vector_bucket_name == want)
            .ok_or_else(|| {
                vectorfs_common::report::Failure::new(
                    "NOT_FOUND",
                    format!("vector bucket '{want}' does not exist"),
                    format!(
                        "existing vector buckets: {}",
                        names_or_empty(
                            &buckets
                                .iter()
                                .map(|b| &b.vector_bucket_name)
                                .collect::<Vec<_>>()
                        )
                    ),
                )
            })?;
        let indexes = storage
            .list_indexes(&meta.vector_bucket_name)
            .await
            .map_err(store_err)?;
        if let Some(want_ix) = index {
            if !indexes.iter().any(|m| m.index_name == want_ix) {
                return Err(vectorfs_common::report::Failure::new(
                    "NOT_FOUND",
                    format!("index '{want}/{want_ix}' does not exist"),
                    format!(
                        "indexes of {want}: {}",
                        names_or_empty(&indexes.iter().map(|m| &m.index_name).collect::<Vec<_>>())
                    ),
                ));
            }
            targets.push((want.to_string(), want_ix.to_string()));
        } else {
            for m in &indexes {
                targets.push((want.to_string(), m.index_name.clone()));
            }
        }
    } else {
        for b in &buckets {
            let indexes = storage
                .list_indexes(&b.vector_bucket_name)
                .await
                .map_err(store_err)?;
            for m in &indexes {
                targets.push((b.vector_bucket_name.clone(), m.index_name.clone()));
            }
        }
    }
    if targets.is_empty() {
        return Err(vectorfs_common::report::Failure::new(
            "NOT_FOUND",
            "no indexes to optimize",
            "ingest something first (or check --bucket: young deployments have no indexes yet)",
        ));
    }

    let mut lines = Vec::with_capacity(targets.len());
    for (vb, ix) in targets {
        // the index's own metric rides into the IVF_PQ distance type so
        // query and index agree (derived: cosine; protocol face: either)
        let meta = storage
            .load_index(&vb, &ix)
            .await
            .map_err(store_err)?
            .ok_or_else(|| {
                vectorfs_common::report::Failure::new(
                    "INTERNAL",
                    format!("catalog says index {vb}/{ix} exists but its index.json is unreadable"),
                    "run `vectorfs doctor` against this data root",
                )
            })?;
        let outcome = storage
            .optimize_vector_index(&vb, &ix, &meta.distance_metric)
            .await
            .map_err(|e| store_err_ctx(e, &vb, &ix))?;
        // lexical half (M3-T9): derived indexes of fts-enabled buckets only
        let tokenizer = fts.get(&vb).copied().unwrap_or(FtsTokenizer::Icu);
        let fts_outcome = if fts.get(&vb).is_some() && meta.x_vf_derived.is_some() {
            Some(
                storage
                    .ensure_fts_index(&vb, &ix, &tokenizer.to_string())
                    .await
                    .map_err(|e| store_err_ctx(e, &vb, &ix))?,
            )
        } else {
            None
        };
        lines.push(OptimizeLine {
            bucket: vb,
            index: ix,
            outcome,
            fts: fts_outcome,
            tokenizer,
        });
    }
    Ok(OptimizeReport { lines })
}

fn store_err(e: crate::store::StoreError) -> vectorfs_common::report::Failure {
    vectorfs_common::report::Failure::new(
        "BACKEND",
        e.to_string(),
        "check the data root (vectorfs doctor) and that no daemon holds it",
    )
}

fn store_err_ctx(
    e: crate::store::StoreError,
    vb: &str,
    ix: &str,
) -> vectorfs_common::report::Failure {
    vectorfs_common::report::Failure::new(
        "BACKEND",
        format!("optimizing {vb}/{ix}: {e}"),
        "this index is left as-is; the other targets still run — fix the cause and re-run",
    )
}

fn names_or_empty(names: &[&String]) -> String {
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{BucketMeta, IndexMeta};
    use std::collections::BTreeMap;

    /// Same shape as store::tests::test_bucket_meta — the deployment
    /// identity MUST be the storage's own or list_buckets' ownership filter
    /// (R9) skips the bucket.
    fn bucket_meta(storage: &Storage, vb: &str) -> BucketMeta {
        BucketMeta {
            vector_bucket_name: vb.to_string(),
            vector_bucket_arn: format!("arn:aws:s3vectors:us-east-1:0:bucket/{vb}"),
            creation_time_rfc3339: "2026-11-06T00:00:00.000Z".into(),
            creation_time_epoch: 0.0,
            deployment: storage.deployment_identity("us-east-1", "000000000000"),
            placement: storage.placement(vb),
            default_index_mode: None,
        }
    }

    fn index_meta(vb: &str, ix: &str, dim: i32) -> IndexMeta {
        IndexMeta {
            vector_bucket_name: vb.to_string(),
            index_name: ix.to_string(),
            index_arn: format!("arn:aws:s3vectors:us-east-1:0:bucket/{vb}/index/{ix}"),
            creation_time_rfc3339: "2026-11-06T00:00:00.000Z".into(),
            creation_time_epoch: 0.0,
            data_type: "float32".into(),
            dimension: dim,
            distance_metric: "cosine".into(),
            non_filterable_metadata_keys: None,
            index_mode: None,
            x_vf_derived: None,
        }
    }

    /// A 300-row dataset (above the training gate) under one index — rows
    /// only, no metadata fuss.
    async fn seeded(dim: i32, n: usize) -> Storage {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::file(dir.path());
        std::mem::forget(dir);
        storage
            .create_bucket(&bucket_meta(&storage, "vb"))
            .await
            .unwrap();
        storage
            .create_index(&index_meta("vb", "ix", dim))
            .await
            .unwrap();
        let mut r = (0..n).map(|_| 0.5f32).collect::<Vec<f32>>();
        r.resize(dim as usize, 0.5);
        for b in 0..n.div_ceil(100) {
            let rows: Vec<crate::store::VectorRow> = (0..100)
                .map(|i| {
                    let key = format!("k{:05}", b * 100 + i);
                    crate::store::VectorRow {
                        key,
                        vector: r.clone(),
                        metadata_json: None,
                        filterable: BTreeMap::new(),
                        text: None,
                    }
                })
                .collect();
            storage.upsert_vectors("vb", "ix", dim, rows).await.unwrap();
        }
        storage
    }

    #[tokio::test]
    async fn sweep_trains_and_named_unknowns_fail_loudly() {
        let storage = seeded(16, 300).await;
        let no_fts = BTreeMap::new();

        // unknown bucket / index: NOT_FOUND naming what exists
        let id = storage.deployment_identity("us-east-1", "000000000000");
        let err = run(&storage, &id, Some("nope"), None, &no_fts)
            .await
            .unwrap_err();
        assert!(err.message.contains("'nope' does not exist"), "{err:?}");
        let err = run(&storage, &id, Some("vb"), Some("ghost"), &no_fts)
            .await
            .unwrap_err();
        assert!(err.message.contains("'vb/ghost' does not exist"), "{err:?}");
        // --index without --bucket is ambiguous: refused
        let err = run(&storage, &id, None, Some("ix"), &no_fts)
            .await
            .unwrap_err();
        assert!(err.message.contains("--index requires --bucket"));

        // named target: trains
        let report = run(&storage, &id, Some("vb"), Some("ix"), &no_fts)
            .await
            .unwrap();
        assert_eq!(
            report.lines,
            vec![OptimizeLine {
                bucket: "vb".into(),
                index: "ix".into(),
                outcome: VectorIndexOutcome::Trained { rows: 300 },
                fts: None, // protocol-shaped index (no x_vf_derived): no FTS
                tokenizer: FtsTokenizer::Icu,
            }]
        );
        assert!(!report.named_below_gate(Some("vb"), Some("ix")));
        assert!(report.render().contains("ivf_pq trained on 300 rows"));

        // re-run folds
        let report = run(&storage, &id, Some("vb"), Some("ix"), &no_fts)
            .await
            .unwrap();
        assert_eq!(
            report.lines[0].outcome,
            VectorIndexOutcome::Folded { rows: 300 }
        );
    }

    #[tokio::test]
    async fn named_below_gate_fails_but_sweep_skips() {
        let storage = seeded(16, 100).await; // below gate
        let id = storage.deployment_identity("us-east-1", "000000000000");
        let no_fts = BTreeMap::new();

        // sweep (no flags): loud SKIPPED line, not an error
        let report = run(&storage, &id, None, None, &no_fts).await.unwrap();
        assert_eq!(
            report.lines[0].outcome,
            VectorIndexOutcome::BelowGate { rows: 100 }
        );
        assert!(
            !report.named_below_gate(None, None),
            "sweep never fails on the gate"
        );
        assert!(report.render().contains("SKIPPED — 100 rows < 256"));

        // named target below gate: the refusal IS the answer
        let report = run(&storage, &id, Some("vb"), Some("ix"), &no_fts)
            .await
            .unwrap();
        assert!(report.named_below_gate(Some("vb"), Some("ix")));
    }

    // ---- M3-T9: the lexical half rides the same verb ----

    /// A DERIVED-shaped index (x_vf_derived stamped) with chunk-text rows —
    /// the FTS pass's actual customer.
    async fn seeded_derived(dim: i32, texts: &[&str]) -> Storage {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::file(dir.path());
        std::mem::forget(dir);
        storage
            .create_bucket(&bucket_meta(&storage, "vb"))
            .await
            .unwrap();
        let mut meta = index_meta("vb", "ix", dim);
        meta.x_vf_derived = Some(crate::store::DerivedIndexInfo {
            key: "0".repeat(64),
            embedder: "tiny".into(),
            model: "tiny-fixture".into(),
            dim: dim as usize,
        });
        storage.create_index(&meta).await.unwrap();
        let rows: Vec<crate::store::VectorRow> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut blob = serde_json::json!({ "x-vf-src-uri": "h1" });
                blob["x-vf-chunk-text"] = serde_json::json!(t);
                crate::store::VectorRow {
                    key: format!("k{i:03}"),
                    vector: vec![0.5; dim as usize],
                    metadata_json: Some(blob.to_string()),
                    filterable: BTreeMap::new(),
                    text: Some(t.to_string()),
                }
            })
            .collect();
        storage.upsert_vectors("vb", "ix", dim, rows).await.unwrap();
        storage
    }

    #[tokio::test]
    async fn fts_pass_builds_and_folds_derived_indexes_only() {
        let storage = seeded_derived(8, &["hello world", "goodbye moon"]).await;
        let id = storage.deployment_identity("us-east-1", "000000000000");
        let mut fts = BTreeMap::new();
        fts.insert("vb".to_string(), FtsTokenizer::Icu);

        let report = run(&storage, &id, Some("vb"), Some("ix"), &fts)
            .await
            .unwrap();
        assert_eq!(
            report.lines[0].fts,
            Some(FtsIndexOutcome::Built {
                rows: 2,
                backfilled: 0
            })
        );
        assert!(
            report
                .render()
                .contains("fts index built on 2 rows (tokenizer icu")
        );

        // keyword search works through the built index
        let hits = storage
            .keyword_search("vb", "ix", "hello", 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "k000");

        // re-run folds (no retrain) and stays idempotent
        let report = run(&storage, &id, Some("vb"), Some("ix"), &fts)
            .await
            .unwrap();
        assert_eq!(
            report.lines[0].fts,
            Some(FtsIndexOutcome::Folded {
                rows: 2,
                backfilled: 0
            })
        );
    }

    /// Young-bucket edges stay loud and harmless: a derived index that was
    /// never materialized (nothing ingested) reports the NoTable skip, and
    /// an EMPTY table (created, zero rows) builds a zero-row FTS index
    /// without failing the sweep — the next ingest + optimize pass fills it.
    #[tokio::test]
    async fn fts_pass_no_table_skips_and_empty_table_builds() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::file(dir.path());
        std::mem::forget(dir);
        storage
            .create_bucket(&bucket_meta(&storage, "vb"))
            .await
            .unwrap();
        let mut meta = index_meta("vb", "ghost", 8);
        meta.x_vf_derived = Some(crate::store::DerivedIndexInfo {
            key: "0".repeat(64),
            embedder: "tiny".into(),
            model: "tiny-fixture".into(),
            dim: 8,
        });

        // never-materialized (no index.json, no table): loud NoTable skip
        assert_eq!(
            storage
                .ensure_fts_index("vb", "ghost", "icu")
                .await
                .unwrap(),
            FtsIndexOutcome::NoTable
        );

        // empty table (created, zero rows): builds a zero-row index fine
        storage.create_index(&meta).await.unwrap();
        assert_eq!(
            storage
                .ensure_fts_index("vb", "ghost", "icu")
                .await
                .unwrap(),
            FtsIndexOutcome::Built {
                rows: 0,
                backfilled: 0
            }
        );
        // and keyword search on the empty indexed table is an honest empty
        let hits = storage
            .keyword_search("vb", "ghost", "anything", 10, None)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
