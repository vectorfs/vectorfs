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
//! Discipline (the M1 `optimize` ruling, design.md §6): explicit management
//! verb, NEVER a background task — the daemon never builds indexes on its
//! own. IVF_PQ is an index PHYSICAL form, not pipeline semantics: zero
//! config keys (nothing in vectorfs.toml, nothing in the pipeline
//! fingerprint), zero protocol-face surface. Until the verb runs, every
//! query stays on the flat exact path — byte-identical to pre-T8.
//!
//! Single-writer: the bin takes the data root's local flock (the same one
//! `vectorfs serve`/`mcp` hold) for the duration; on backends without a
//! local home (memory/s3) it proceeds with an explicit warning — the
//! maintenance window is then the operator's guarantee.

use crate::store::{DeploymentIdentity, Storage, VectorIndexOutcome};

/// One index's result line.
#[derive(Debug, PartialEq)]
pub struct OptimizeLine {
    pub bucket: String,
    pub index: String,
    pub outcome: VectorIndexOutcome,
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
pub async fn run(
    storage: &Storage,
    identity: &DeploymentIdentity,
    bucket: Option<&str>,
    index: Option<&str>,
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
        lines.push(OptimizeLine {
            bucket: vb,
            index: ix,
            outcome,
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

        // unknown bucket / index: NOT_FOUND naming what exists
        let id = storage.deployment_identity("us-east-1", "000000000000");
        let err = run(&storage, &id, Some("nope"), None).await.unwrap_err();
        assert!(err.message.contains("'nope' does not exist"), "{err:?}");
        let err = run(&storage, &id, Some("vb"), Some("ghost"))
            .await
            .unwrap_err();
        assert!(err.message.contains("'vb/ghost' does not exist"), "{err:?}");
        // --index without --bucket is ambiguous: refused
        let err = run(&storage, &id, None, Some("ix")).await.unwrap_err();
        assert!(err.message.contains("--index requires --bucket"));

        // named target: trains
        let report = run(&storage, &id, Some("vb"), Some("ix")).await.unwrap();
        assert_eq!(
            report.lines,
            vec![OptimizeLine {
                bucket: "vb".into(),
                index: "ix".into(),
                outcome: VectorIndexOutcome::Trained { rows: 300 }
            }]
        );
        assert!(!report.named_below_gate(Some("vb"), Some("ix")));
        assert!(report.render().contains("ivf_pq trained on 300 rows"));

        // re-run folds
        let report = run(&storage, &id, Some("vb"), Some("ix")).await.unwrap();
        assert_eq!(
            report.lines[0].outcome,
            VectorIndexOutcome::Folded { rows: 300 }
        );
    }

    #[tokio::test]
    async fn named_below_gate_fails_but_sweep_skips() {
        let storage = seeded(16, 100).await; // below gate
        let id = storage.deployment_identity("us-east-1", "000000000000");

        // sweep (no flags): loud SKIPPED line, not an error
        let report = run(&storage, &id, None, None).await.unwrap();
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
        let report = run(&storage, &id, Some("vb"), Some("ix")).await.unwrap();
        assert!(report.named_below_gate(Some("vb"), Some("ix")));
    }
}
