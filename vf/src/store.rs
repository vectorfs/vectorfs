//! Storage facade over the unified backend (M1 T2; design.md §5.2/§5.3).
//!
//! Two responsibilities:
//! - catalog: `buckets/<name>/bucket.json`,
//!   `buckets/<vb>/indexes/<ix>/index.json` — ALL via object_store
//!   (`get` → NotFound → 404; create = `put_opts(PutMode::Create)`, race-free
//!   on file/memory/S3). The `.vectorfs/` system prefix is applied centrally
//!   by `Backend::object_key` (backends.rs), so the keys below are
//!   system-relative. The file backend no longer touches std::fs for
//!   catalog objects (only backend setup in backends.rs creates/canonicalizes
//!   the data dir itself). M1 T3 adds the management-plane half of the
//!   catalog: enumeration (list), removal (bucket/index delete incl. the
//!   Lance dataset objects), policy.json (raw bytes, store & echo only,
//!   design.md §4.4), tags.json (bucket and index ARN scopes) and the
//!   echo-only indexMode/defaultIndexMode knobs.
//! - engine: lancedb (the ONLY module that touches lancedb API, design.md
//!   §5.7). One Lance table per index at `<index>/data` (file:
//!   `file://<root>/.vectorfs/buckets/<vb>/indexes/<ix>/data`, memory:
//!   `memory://vf/.vectorfs/…`, S3:
//!   `s3://<bucket>[/<prefix>]/.vectorfs/…`), opened through the
//!   backend's shared `Session` (per-registry object stores). A fresh
//!   `Connection` per operation remains a spike simplification — no connection
//!   cache; M0 REPORT 遗留 3, TODO for a later M1 task. M1 T4 adds the
//!   vector-surface engine paths: key point-lookup (GetVectors), key-ordered
//!   cursor scan (ListVectors) and key-bound delete (DeleteVectors) — all via
//!   DataFusion expression predicates (see `keys_in`), never SQL strings.
//!   M1 T5 adds the filter engine integration: dynamic typed mirror columns
//!   and dual query routes (see below).
//!
//! Lance schema: `key: utf8 not null`, `vector: fixed_size_list<f32, dim>`
//! (not null), `metadata_json: utf8 nullable` — the COMPLETE original metadata
//! document — plus (M1 T5) `metadata: struct<…nullable typed children>`: one
//! dynamic child per scalar filterable key ("mirror columns", design.md §5.3).
//!
//! Why a mirror alongside the authoritative blob: the blob keeps the wire
//! output byte-stable (JSON number formatting `7` vs `7.0`, heterogeneous
//! lists like `["a", 2, true]` that no single arrow column type can hold, key
//! order), while the typed mirror exists purely so filter predicates can be
//! pushed down into the engine (rows failing the predicate never have their
//! vector bytes read — M0 REPORT 遗留 1). Reads therefore NEVER consult the
//! mirror: GetVectors/ListVectors/QueryVectors output comes from
//! `metadata_json` alone, so the wire surface is identical with or without
//! mirrors. List-valued keys and keys declared in the index's
//! nonFilterableMetadataKeys are never mirrored (blob-only).
//!
//! Mirror lifecycle on the write path (three-step sequence, M0 Q10-proven):
//! new scalar key in a batch → `add_columns(AllNulls(metadata: struct{new
//! children}))` evolution → reopen → `merge_insert` with the full target
//! schema. A key's scalar type must stay stable across the whole index:
//! number→string (or scalar↔list) drift is a ValidationException — the schema
//! itself is the per-key type registry (single writer, design.md §5.1, makes
//! the read-classify-evolve sequence race-free).
//!
//! # 性能对照（M1 T5 交付数据，predicate pushdown vs 纯内存路径）
//!
//! 2026-11-04, `cargo test --release` (standard profile, NOT release-prod),
//! file backend / tmp dir, 100,000 vectors dim=8 euclidean, metadata
//! `{"i":n,"mod":n%100,"tag":"t{n%50}"}` ingested once (200 merge_insert
//! batches of 500, the boto3 batch shape; 2.9s release / 23.4s debug),
//! topK=10, 30 timed iterations per cell (first discarded), P50/P99 ms:
//!
//! | filter (approx. hits)      | pushdown P50/P99 | memory P50/P99 | P50 speedup |
//! |----------------------------|------------------|----------------|-------------|
//! | eq-high  tag=t7   (~2,000) |  32.4 /  34.7    |  77.5 /  84.6  |   2.40x     |
//! | eq-low   i=4242   (1)      |  24.7 /  25.7    |  73.5 /  84.8  |   2.97x     |
//! | range    i≥99000  (~1,000) |  24.7 /  26.4    |  74.3 /  80.4  |   3.01x     |
//! | in-list  mod∈0..50 (~49.5k)|  33.7 /  35.4    |  97.3 / 109.4  |   2.89x     |
//! | startsWith tag^t4 (~22,000)|  34.7 /  36.1    |  82.6 /  97.6  |   2.38x     |
//!
//! Reproduce: `cargo test --lib perf_filter_pushdown_vs_memory -- --ignored
//! --nocapture` (口径: engine-direct `Storage::query_vectors` calls — no
//! HTTP/SigV4 stack; the memory column is the spike-era path, M0 REPORT
//! 遗留 1, kept as the reference route and A/B control). Debug-profile
//! ratios were 1.9–2.6x on the same dataset.

use std::collections::BTreeMap;
use std::sync::Arc;

use lancedb::arrow::arrow_array::{
    Array, BooleanArray, FixedSizeListArray, Float32Array, Float64Array, RecordBatch,
    RecordBatchIterator, StringArray, StructArray,
};
// Arrow numeric type marker used by `as_primitive`.
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_schema::{ArrowError, DataType, Field, Fields, Schema};
// DataFusion expression builders (lancedb re-exports): the ONLY route used for
// key predicates — values become ScalarValue literals serialized as substrait,
// never SQL text, so arbitrary UTF-8 keys cannot inject or escape (M1 T4:
// GetVectors / ListVectors / DeleteVectors all share this discipline; M1 T5
// extends it to metadata predicates via crate::filter).
use lancedb::expr::{col, is_in, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{Connection, DistanceType, Table};

use crate::backends::{self, Backend};
use crate::filter::{Filter, MetaCol};

pub(crate) const TABLE_NAME: &str = "vectors";

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BucketMeta {
    pub vector_bucket_name: String,
    pub vector_bucket_arn: String,
    /// RFC3339 (UTC), per task spec for the on-disk file.
    pub creation_time_rfc3339: String,
    /// Unix epoch seconds (fractional) — wire format for rest-json timestamps.
    pub creation_time_epoch: f64,
    /// `defaultIndexMode` (M1 T3): stored & echoed by PutVectorBucketDefault-
    /// IndexMode; consumed at CreateIndex time. Echo-only — runtime query
    /// behavior is always pre-filter/ENHANCED (design.md §4.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_index_mode: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct IndexMeta {
    pub vector_bucket_name: String,
    pub index_name: String,
    pub index_arn: String,
    pub creation_time_rfc3339: String,
    pub creation_time_epoch: f64,
    pub data_type: String,
    pub dimension: i32,
    /// "cosine" | "euclidean"
    pub distance_metric: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_filterable_metadata_keys: Option<Vec<String>>,
    /// `indexMode` (M1 T3): copied from the bucket's defaultIndexMode at
    /// create time (ENHANCED when unset), rewritten by UpdateIndexMode.
    /// Echo-only — see BucketMeta::default_index_mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_mode: Option<String>,
}

#[derive(Debug)]
pub enum StoreError {
    /// bucket/index already exists -> ConflictException
    Conflict(String),
    /// request-shape problem detected below the wire layer (metadata type
    /// drift, stale/unknown QueryVectors page version) -> ValidationException
    Validation(String),
    /// anything else -> InternalServerException
    Internal(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict(m) | StoreError::Internal(m) | StoreError::Validation(m) => {
                write!(f, "{m}")
            }
        }
    }
}

fn internal(e: impl std::fmt::Display) -> StoreError {
    StoreError::Internal(e.to_string())
}

// ---------------------------------------------------------------------------
// storage
// ---------------------------------------------------------------------------

/// Backend-agnostic storage: catalog objects + Lance datasets both flow from
/// one [`Backend`].
pub struct Storage {
    backend: Backend,
    catalog: Arc<dyn object_store::ObjectStore>,
    session: Arc<lancedb::Session>,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.backend)
    }
}

impl Storage {
    /// Build from a parsed backend (file/memory/s3). The file root is
    /// normalized (created + canonicalized) here if it was not already.
    pub fn new(backend: Backend) -> Result<Self, String> {
        let backend = backend.normalized()?;
        let handles = backend.build()?;
        Ok(Storage {
            backend,
            catalog: handles.catalog,
            session: handles.session,
        })
    }

    /// File backend rooted at `dir` (creates + canonicalizes it).
    pub fn file(root: impl Into<std::path::PathBuf>) -> Self {
        Storage::new(Backend::File { root: root.into() })
            .expect("file backend setup cannot fail (dir is created)")
    }

    /// Volatile in-process backend (CI / emulator mode).
    pub fn memory() -> Self {
        Storage::new(Backend::Memory).expect("memory backend setup cannot fail")
    }

    /// S3 backend (M0-verified injection route).
    pub fn s3(cfg: backends::S3BackendConfig) -> Result<Self, String> {
        Storage::new(Backend::S3(Box::new(cfg)))
    }

    /// The backend (for LOCK placement + logging).
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Catalog + LOCK object store (shared with lock.rs).
    pub fn catalog(&self) -> &Arc<dyn object_store::ObjectStore> {
        &self.catalog
    }

    // ---- catalog (object_store everywhere; §5.0 “目录: catalog JSON 对象(条件写)”) ----

    pub async fn load_bucket(&self, name: &str) -> Result<Option<BucketMeta>, StoreError> {
        backends::read_json(
            &*self.catalog,
            &self.key(&format!("buckets/{name}/bucket.json")),
        )
        .await
        .map_err(internal)
    }

    pub async fn load_index(&self, vb: &str, ix: &str) -> Result<Option<IndexMeta>, StoreError> {
        backends::read_json(
            &*self.catalog,
            &self.key(&format!("buckets/{vb}/indexes/{ix}/index.json")),
        )
        .await
        .map_err(internal)
    }

    pub async fn create_bucket(&self, meta: &BucketMeta) -> Result<(), StoreError> {
        let key = self.key(&format!("buckets/{}/bucket.json", meta.vector_bucket_name));
        let json = serde_json::to_vec_pretty(meta).map_err(internal)?;
        if !backends::put_json_create(&*self.catalog, &key, &json)
            .await
            .map_err(internal)?
        {
            return Err(StoreError::Conflict(format!(
                "vector bucket {} already exists",
                meta.vector_bucket_name
            )));
        }
        Ok(())
    }

    /// Create index.json (conditional) + the empty Lance table. On Lance
    /// failure the index.json is deleted again (best effort) so the catalog
    /// never advertises an index without a dataset.
    pub async fn create_index(&self, meta: &IndexMeta) -> Result<(), StoreError> {
        let key = self.key(&format!(
            "buckets/{}/indexes/{}/index.json",
            meta.vector_bucket_name, meta.index_name
        ));
        let json = serde_json::to_vec_pretty(meta).map_err(internal)?;
        if !backends::put_json_create(&*self.catalog, &key, &json)
            .await
            .map_err(internal)?
        {
            return Err(StoreError::Conflict(format!(
                "index {} already exists",
                meta.index_name
            )));
        }
        let conn = match self
            .connect_index(&meta.vector_bucket_name, &meta.index_name)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let _ = backends::delete_object(&*self.catalog, &key).await;
                return Err(e);
            }
        };
        if let Err(e) = conn
            .create_empty_table(TABLE_NAME, arrow_schema(meta.dimension))
            .execute()
            .await
        {
            let _ = backends::delete_object(&*self.catalog, &key).await; // best-effort rollback
            return Err(internal(e));
        }
        Ok(())
    }

    fn key(&self, rel: &str) -> object_store::path::Path {
        self.backend.object_key(rel)
    }

    // ---- catalog: management-plane enumeration & removal (M1 T3, design.md §5.3) ----

    /// All bucket metas, sorted by name (ListVectorBuckets ordering).
    /// Key parsing is suffix-based: on the s3 backend every key additionally
    /// carries the configured bucket prefix, so fixed positions would drift.
    pub async fn list_buckets(&self) -> Result<Vec<BucketMeta>, StoreError> {
        let mut out = Vec::new();
        for key in backends::list_prefix(&*self.catalog, &self.key("buckets"))
            .await
            .map_err(internal)?
        {
            // …/buckets/<vb>/bucket.json — ignore everything else (indexes/, tags…)
            let segs = key.as_ref().split('/').collect::<Vec<_>>();
            if segs.len() >= 2 && segs.last() == Some(&"bucket.json") {
                let name = segs[segs.len() - 2];
                if let Some(meta) = backends::read_json::<BucketMeta>(
                    &*self.catalog,
                    &self.key(&format!("buckets/{name}/bucket.json")),
                )
                .await
                .map_err(internal)?
                {
                    out.push(meta);
                }
            }
        }
        out.sort_by(|a, b| a.vector_bucket_name.cmp(&b.vector_bucket_name));
        Ok(out)
    }

    /// All index metas of one bucket, sorted by name (ListIndexes ordering).
    pub async fn list_indexes(&self, vb: &str) -> Result<Vec<IndexMeta>, StoreError> {
        let mut out = Vec::new();
        for key in
            backends::list_prefix(&*self.catalog, &self.key(&format!("buckets/{vb}/indexes")))
                .await
                .map_err(internal)?
        {
            // …/indexes/<ix>/index.json (see list_buckets for the suffix parse)
            let segs = key.as_ref().split('/').collect::<Vec<_>>();
            if segs.len() >= 3
                && segs.last() == Some(&"index.json")
                && segs[segs.len() - 3] == "indexes"
            {
                let ix = segs[segs.len() - 2];
                let rel = format!("buckets/{vb}/indexes/{ix}/index.json");
                if let Some(meta) =
                    backends::read_json::<IndexMeta>(&*self.catalog, &self.key(&rel))
                        .await
                        .map_err(internal)?
                {
                    out.push(meta);
                }
            }
        }
        out.sort_by(|a, b| a.index_name.cmp(&b.index_name));
        Ok(out)
    }

    /// True if ANY object lives under `buckets/<vb>/indexes/` — the
    /// DeleteVectorBucket non-empty guard. (Single-writer lock makes the
    /// list-then-delete sequence race-free, design.md §5.1.)
    pub async fn bucket_has_indexes(&self, vb: &str) -> Result<bool, StoreError> {
        Ok(
            !backends::list_prefix(&*self.catalog, &self.key(&format!("buckets/{vb}/indexes")))
                .await
                .map_err(internal)?
                .is_empty(),
        )
    }

    /// Remove a bucket's catalog objects (bucket.json + optional policy.json /
    /// tags.json). Caller must have verified the bucket has no indexes.
    pub async fn delete_bucket(&self, name: &str) -> Result<(), StoreError> {
        for rel in [
            format!("buckets/{name}/bucket.json"),
            format!("buckets/{name}/policy.json"),
            format!("buckets/{name}/tags.json"),
        ] {
            backends::delete_object(&*self.catalog, &self.key(&rel))
                .await
                .map_err(internal)?;
        }
        Ok(())
    }

    /// Remove an index: every object under the index's system-prefix dir
    /// (`.vectorfs/buckets/<vb>/indexes/<ix>/` — index.json, tags.json AND
    /// the Lance dataset under `data/`, which shares the catalog object store
    /// on file/s3 backends). The memory backend's engine datasets live under
    /// the `vf/` URL-host prefix on the same shared map (backends.rs
    /// SharedMemoryProvider), i.e. at `vf/.vectorfs/…` — a key shape the
    /// object_key funnel cannot express, so that mirror prefix is swept
    /// explicitly; a no-op on file/s3 where it never exists.
    pub async fn delete_index(&self, vb: &str, ix: &str) -> Result<(), StoreError> {
        let catalog_base = self.key(&format!("buckets/{vb}/indexes/{ix}"));
        let memory_base =
            object_store::path::Path::from(format!("{}/{catalog_base}", backends::MEMORY_HOST));
        for base in [catalog_base, memory_base] {
            for key in backends::list_prefix(&*self.catalog, &base)
                .await
                .map_err(internal)?
            {
                backends::delete_object(&*self.catalog, &key)
                    .await
                    .map_err(internal)?;
            }
        }
        Ok(())
    }

    // ---- policy (§4.4: store & echo, never enforce) ----

    pub async fn load_policy(&self, vb: &str) -> Result<Option<String>, StoreError> {
        match backends::get_with_etag(
            &*self.catalog,
            &self.key(&format!("buckets/{vb}/policy.json")),
        )
        .await
        .map_err(internal)?
        {
            None => Ok(None),
            Some((bytes, _etag)) => Ok(Some(
                String::from_utf8(bytes.to_vec()).map_err(|e| internal(format!("policy: {e}")))?,
            )),
        }
    }

    /// Idempotent overwrite; raw bytes as received (byte-faithful echo).
    pub async fn put_policy(&self, vb: &str, raw: &str) -> Result<(), StoreError> {
        backends::put_json_overwrite(
            &*self.catalog,
            &self.key(&format!("buckets/{vb}/policy.json")),
            raw.as_bytes(),
        )
        .await
        .map_err(internal)
    }

    pub async fn delete_policy(&self, vb: &str) -> Result<(), StoreError> {
        backends::delete_object(
            &*self.catalog,
            &self.key(&format!("buckets/{vb}/policy.json")),
        )
        .await
        .map_err(internal)
    }

    // ---- tags (bucket ARN and index ARN both address tags.json, §5.3) ----

    fn tags_key(&self, vb: &str, ix: Option<&str>) -> object_store::path::Path {
        self.key(&match ix {
            None => format!("buckets/{vb}/tags.json"),
            Some(ix) => format!("buckets/{vb}/indexes/{ix}/tags.json"),
        })
    }

    pub async fn load_tags(
        &self,
        vb: &str,
        ix: Option<&str>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, StoreError> {
        Ok(backends::read_json(&*self.catalog, &self.tags_key(vb, ix))
            .await
            .map_err(internal)?
            .unwrap_or_else(serde_json::Map::new))
    }

    pub async fn put_tags(
        &self,
        vb: &str,
        ix: Option<&str>,
        tags: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), StoreError> {
        let json =
            serde_json::to_vec(&serde_json::Value::Object(tags.clone())).map_err(internal)?;
        backends::put_json_overwrite(&*self.catalog, &self.tags_key(vb, ix), &json)
            .await
            .map_err(internal)
    }

    // ---- indexMode / defaultIndexMode (echo-only knobs, §4.4) ----

    pub async fn set_default_index_mode(&self, name: &str, mode: &str) -> Result<(), StoreError> {
        let mut meta = self
            .load_bucket(name)
            .await?
            .ok_or_else(|| StoreError::Internal(format!("bucket {name} vanished mid-write")))?;
        meta.default_index_mode = Some(mode.to_string());
        let json = serde_json::to_vec_pretty(&meta).map_err(internal)?;
        backends::put_json_overwrite(
            &*self.catalog,
            &self.key(&format!("buckets/{name}/bucket.json")),
            &json,
        )
        .await
        .map_err(internal)
    }

    pub async fn set_index_mode(&self, vb: &str, ix: &str, mode: &str) -> Result<(), StoreError> {
        let mut meta = self
            .load_index(vb, ix)
            .await?
            .ok_or_else(|| StoreError::Internal(format!("index {vb}/{ix} vanished mid-write")))?;
        meta.index_mode = Some(mode.to_string());
        let json = serde_json::to_vec_pretty(&meta).map_err(internal)?;
        backends::put_json_overwrite(
            &*self.catalog,
            &self.key(&format!("buckets/{vb}/indexes/{ix}/index.json")),
            &json,
        )
        .await
        .map_err(internal)
    }

    // ---- engine ----

    /// Open a lancedb connection to an index's data dir through the shared
    /// session (registry-routed store for file/memory/s3 alike).
    async fn connect_index(&self, vb: &str, ix: &str) -> Result<Connection, StoreError> {
        let mut builder =
            lancedb::connect(&self.backend.lance_uri(vb, ix)).session(self.session.clone());
        if let Some(opts) = self.backend.storage_options() {
            builder = builder.storage_options(opts);
        }
        builder.execute().await.map_err(internal)
    }

    async fn open_table(&self, vb: &str, ix: &str) -> Result<Table, StoreError> {
        self.connect_index(vb, ix)
            .await?
            .open_table(TABLE_NAME)
            .execute()
            .await
            .map_err(internal)
    }

    pub async fn upsert_vectors(
        &self,
        vb: &str,
        ix: &str,
        dim: i32,
        rows: Vec<VectorRow>,
    ) -> Result<(), StoreError> {
        let table = self.open_table(vb, ix).await?;
        // M1 T5 three-step sequence (design.md §5.3 / M0 Q10): drift-check the
        // batch against the live mirror columns, evolve for genuinely new
        // scalar keys, REOPEN (stale handles keep the pre-evolution schema),
        // then merge_insert under the full target schema.
        let existing = mirror_columns(table.schema().await.map_err(internal)?.as_ref());
        let needed = classify_batch(&existing, &rows)?;
        let table = if needed.is_empty() {
            table
        } else {
            evolve_metadata(&table, &needed).await?;
            self.open_table(vb, ix).await?
        };
        upsert_table(&table, dim, rows).await
    }

    /// QueryVectors engine entry (M1 T5): flat exact KNN under a metadata
    /// filter, pre-filter semantics (ENHANCED always, design.md §4.4).
    ///
    /// `route` selects the filter execution path — [`QueryRoute::Pushdown`]
    /// translates the filter into an engine-side predicate over the mirror
    /// columns (falling back to the in-memory path when nothing is
    /// translatable), [`QueryRoute::Memory`] forces the spike-era full-scan +
    /// in-memory evaluation. Both paths produce IDENTICAL results (pinned by
    /// `query_routes_agree`); the memory route stays public as the reference
    /// implementation and A/B baseline (see `perf_filter_pushdown_vs_memory`).
    ///
    /// `as_of` pins the dataset version (QueryVectors pagination snapshot:
    /// the page token carries the version; lance time-travel makes paging
    /// immune to writes made between pages). Returns the executed version so
    /// the caller can mint the next token.
    ///
    /// Results are sorted by (distance asc, key asc) and truncated to top_k —
    /// deterministic pagination offsets need a total order.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_vectors(
        &self,
        vb: &str,
        ix: &str,
        dim: i32,
        metric: &str,
        query: &[f32],
        top_k: usize,
        filter: Option<&Filter>,
        route: QueryRoute,
        as_of: Option<u64>,
    ) -> Result<QueryOutcome, StoreError> {
        let table = self.open_table(vb, ix).await?;
        if let Some(v) = as_of {
            // unknown/stale version -> the index was recreated under the same
            // name; treat the token as invalid (ValidationException)
            table
                .checkout(v)
                .await
                .map_err(|e| StoreError::Validation(format!("invalid nextToken: {e}")))?;
        }
        let version = table.version().await.map_err(internal)?;
        let cols = mirror_columns(table.schema().await.map_err(internal)?.as_ref());
        let hits = query_table(&table, dim, metric, query, top_k, filter, route, &cols).await?;
        Ok(QueryOutcome { hits, version })
    }

    // ---- engine: M1 T4 vector-surface reads/writes ----

    /// Batch point lookup by primary key (GetVectors, ≤100 keys): ONE scan
    /// with an arrow-bound IN predicate. Keys are bound as DataFusion
    /// `ScalarValue` literals — no SQL string is ever built, so keys
    /// containing quotes / backslashes / unicode / emoji cannot inject.
    pub async fn get_vectors(
        &self,
        vb: &str,
        ix: &str,
        keys: &[String],
    ) -> Result<Vec<FoundVector>, StoreError> {
        let table = self.open_table(vb, ix).await?;
        let pred = keys_in(keys);
        let stream = table
            .query()
            .only_if_expr(pred)
            .execute()
            .await
            .map_err(internal)?;
        let mut out = Vec::new();
        let mut stream = stream;
        use futures_util::StreamExt;
        while let Some(batch) = stream.next().await {
            out.extend(found_from_batch(&batch.map_err(internal)?)?);
        }
        Ok(out)
    }

    /// Key-ordered scan with an optional `key > after` cursor bound
    /// (ListVectors pagination). The comparison is arrow/datafusion UTF-8
    /// byte order — identical to Rust `str` ordering, so the cursor written
    /// by the ops layer round-trips across engines. TODO(M1): the scan is
    /// materialized and sorted Rust-side (spike-consistent; predicate
    /// pushdown + engine-side ORDER BY is the M0 REPORT 遗留 1 follow-up).
    pub async fn scan_vectors(
        &self,
        vb: &str,
        ix: &str,
        after: Option<&str>,
    ) -> Result<Vec<FoundVector>, StoreError> {
        let table = self.open_table(vb, ix).await?;
        let mut query = table.query();
        if let Some(a) = after {
            query = query.only_if_expr(col("key").gt(lit(a.to_string())));
        }
        let stream = query.execute().await.map_err(internal)?;
        let mut out = Vec::new();
        let mut stream = stream;
        use futures_util::StreamExt;
        while let Some(batch) = stream.next().await {
            out.extend(found_from_batch(&batch.map_err(internal)?)?);
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Delete rows by primary key (DeleteVectors, ≤500 keys) through the
    /// same arrow-bound predicate (`Predicate::Expr` → substrait; the SQL
    /// string route is deliberately unused). Deleting absent keys is a no-op
    /// — idempotent success, AWS semantics.
    pub async fn delete_vectors(
        &self,
        vb: &str,
        ix: &str,
        keys: &[String],
    ) -> Result<(), StoreError> {
        let table = self.open_table(vb, ix).await?;
        table.delete(&keys_in(keys)).await.map_err(internal)?;
        Ok(())
    }

    /// Read-only row count of one index's dataset — the `vectorfs status` /
    /// `vectorfs doctor` inspection path (M1 T6). Never opens a write path.
    pub async fn count_vectors(&self, vb: &str, ix: &str) -> Result<usize, StoreError> {
        let table = self.open_table(vb, ix).await?;
        table.count_rows(None).await.map_err(internal)
    }
}

// ---------------------------------------------------------------------------
// lancedb
// ---------------------------------------------------------------------------

fn arrow_schema(dim: i32) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            false,
        ),
        Field::new("metadata_json", DataType::Utf8, true),
    ]))
}

fn distance_type(metric: &str) -> DistanceType {
    match metric {
        "cosine" => DistanceType::Cosine,
        _ => DistanceType::L2, // "euclidean"
    }
}

/// A vector row as prepared by the ops layer for one PutVectors batch.
pub struct VectorRow {
    pub key: String,
    pub vector: Vec<f32>,
    /// Pre-serialized metadata JSON — the COMPLETE document (filterable and
    /// non-filterable keys alike); None = no metadata stored. Authoritative
    /// for all read-back paths (see module doc).
    pub metadata_json: Option<String>,
    /// Filterable-key kind map for THIS row (M1 T5): ops classifies every
    /// filterable key of the document; scalar values become mirror-column
    /// values, `List` marks list-valued keys (never mirrored, blob-only —
    /// but still drift-checked against any existing mirror column).
    pub filterable: BTreeMap<String, MetaMirror>,
}

/// What a filterable metadata key looks like in one row (see VectorRow).
#[derive(Debug, Clone, PartialEq)]
pub enum MetaMirror {
    Num(f64),
    Str(String),
    Bool(bool),
    /// List value (any elements) — blob-only, never a mirror column.
    List,
}

impl MetaMirror {
    fn kind(&self) -> Option<MetaCol> {
        match self {
            MetaMirror::Num(_) => Some(MetaCol::F64),
            MetaMirror::Str(_) => Some(MetaCol::Utf8),
            MetaMirror::Bool(_) => Some(MetaCol::Bool),
            MetaMirror::List => None,
        }
    }
}

fn kind_data_type(k: MetaCol) -> DataType {
    match k {
        MetaCol::F64 => DataType::Float64,
        MetaCol::Utf8 => DataType::Utf8,
        MetaCol::Bool => DataType::Boolean,
    }
}

/// Mirror columns currently present in the table schema: field name -> kind.
/// Absent `metadata` struct or non-scalar child types map to nothing (keys
/// under those children are simply not pushdown-able).
fn mirror_columns(schema: &Schema) -> BTreeMap<String, MetaCol> {
    let mut out = BTreeMap::new();
    if let Some(f) = schema.field_with_name("metadata").ok()
        && let DataType::Struct(children) = f.data_type()
    {
        for c in children {
            let kind = match c.data_type() {
                DataType::Float64 => Some(MetaCol::F64),
                DataType::Utf8 => Some(MetaCol::Utf8),
                DataType::Boolean => Some(MetaCol::Bool),
                _ => None,
            };
            if let Some(k) = kind {
                out.insert(c.name().clone(), k);
            }
        }
    }
    out
}

/// Drift-check a batch against the live mirror columns; returns the NEW
/// scalar keys that require schema evolution. Same-key scalar type drift
/// (number vs string vs boolean vs list) is a ValidationException —
/// fail-closed, no silent recast; TODO(AWS 对拍): whether the real service
/// accepts per-key mixed types.
fn classify_batch(
    existing: &BTreeMap<String, MetaCol>,
    rows: &[VectorRow],
) -> Result<BTreeMap<String, MetaCol>, StoreError> {
    let mut needed: BTreeMap<String, MetaCol> = BTreeMap::new();
    for r in rows {
        for (k, m) in &r.filterable {
            // effective registry = existing columns ∪ keys earlier in THIS
            // batch (in-batch drift is caught the same way as cross-batch)
            let known = existing.get(k).or_else(|| needed.get(k));
            let want = m.kind();
            match known {
                Some(&have) => {
                    if want != Some(have) {
                        return Err(StoreError::Validation(format!(
                            "metadata key {k:?} is stored as {} in this index but this request \
                             uses {} — a metadata key's type cannot change within an index",
                            col_type_name(have),
                            mirror_type_name(m)
                        )));
                    }
                    // already mirrored (or mirrored by an earlier row); nothing new
                }
                None => {
                    if let Some(kind) = want {
                        needed.insert(k.clone(), kind);
                    }
                    // list value for an unknown key: blob-only, nothing to do
                }
            }
        }
    }
    Ok(needed)
}

/// AllNulls evolution with ONLY the new struct children — the exact shape M0
/// Q10 proved (a full struct including existing children is rejected with
/// "Column ... already exists"; a flat "metadata.k" name is rejected too).
async fn evolve_metadata(
    table: &Table,
    needed: &BTreeMap<String, MetaCol>,
) -> Result<(), StoreError> {
    let children = Fields::from(
        needed
            .iter()
            .map(|(k, kind)| Field::new(k.clone(), kind_data_type(*kind), true))
            .collect::<Vec<_>>(),
    );
    let evolved = Arc::new(Schema::new(vec![Field::new(
        "metadata",
        DataType::Struct(children),
        true,
    )]));
    table
        .add_columns()
        .transform(lance::dataset::NewColumnTransform::AllNulls(evolved))
        .execute()
        .await
        .map_err(internal)?;
    Ok(())
}

/// Upsert rows keyed on `key` (replace whole row, AWS PutVectors semantics).
/// lancedb 0.41 API actually used: `Table::merge_insert(&["key"])`
/// `.when_matched_update_all(None)` + `.when_not_matched_insert_all()`
/// `.execute(Box<dyn RecordBatchReader + Send>)`. The table schema must
/// already carry every mirror column the batch needs (see
/// [`Storage::upsert_vectors`]).
async fn upsert_table(table: &Table, dim: i32, rows: Vec<VectorRow>) -> Result<(), StoreError> {
    let cols = mirror_columns(table.schema().await.map_err(internal)?.as_ref());

    let keys: StringArray = rows.iter().map(|r| Some(r.key.as_str())).collect();
    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|r| Some(r.vector.iter().map(|v| Some(*v)).collect::<Vec<_>>())),
        dim,
    );
    let metas: StringArray = rows.iter().map(|r| r.metadata_json.as_deref()).collect();

    let mut fields = vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            false,
        ),
        Field::new("metadata_json", DataType::Utf8, true),
    ];
    let mut arrays: Vec<Arc<dyn Array>> = vec![
        Arc::new(keys) as Arc<dyn Array>,
        Arc::new(vectors) as Arc<dyn Array>,
        Arc::new(metas) as Arc<dyn Array>,
    ];
    if !cols.is_empty() {
        // struct child arrays in schema order; each row contributes its value
        // for the child (typed per the schema) or NULL
        let mut child_arrays: Vec<Arc<dyn Array>> = Vec::with_capacity(cols.len());
        let mut child_fields: Vec<Field> = Vec::with_capacity(cols.len());
        for (name, kind) in &cols {
            match kind {
                MetaCol::F64 => {
                    let a: Float64Array = rows
                        .iter()
                        .map(|r| mirror_num(&r.filterable, name))
                        .collect();
                    child_arrays.push(Arc::new(a));
                }
                MetaCol::Utf8 => {
                    let a: StringArray = rows
                        .iter()
                        .map(|r| mirror_str(&r.filterable, name))
                        .collect();
                    child_arrays.push(Arc::new(a));
                }
                MetaCol::Bool => {
                    let a: BooleanArray = rows
                        .iter()
                        .map(|r| mirror_bool(&r.filterable, name))
                        .collect();
                    child_arrays.push(Arc::new(a));
                }
            }
            child_fields.push(Field::new(name.clone(), kind_data_type(*kind), true));
        }
        fields.push(Field::new(
            "metadata",
            DataType::Struct(Fields::from(child_fields.clone())),
            true,
        ));
        arrays.push(Arc::new(StructArray::new(
            Fields::from(child_fields),
            child_arrays,
            None,
        )));
    }
    let batch_schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(batch_schema.clone(), arrays).map_err(internal)?;

    // arrow's RecordBatchIterator satisfies the `Box<dyn RecordBatchReader + Send>`
    // that MergeInsertBuilder::execute expects in 0.41.
    let reader = RecordBatchIterator::new(vec![Ok::<_, ArrowError>(batch)], batch_schema);
    let mut merge = table.merge_insert(&["key"]);
    merge
        .when_matched_update_all(None)
        .when_not_matched_insert_all();
    merge.execute(Box::new(reader)).await.map_err(internal)?;
    Ok(())
}

fn mirror_num(m: &BTreeMap<String, MetaMirror>, k: &str) -> Option<f64> {
    match m.get(k) {
        Some(MetaMirror::Num(n)) => Some(*n),
        _ => None,
    }
}

fn mirror_str<'a>(m: &'a BTreeMap<String, MetaMirror>, k: &str) -> Option<&'a str> {
    match m.get(k) {
        Some(MetaMirror::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn mirror_bool(m: &BTreeMap<String, MetaMirror>, k: &str) -> Option<bool> {
    match m.get(k) {
        Some(MetaMirror::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn col_type_name(k: MetaCol) -> &'static str {
    match k {
        MetaCol::F64 => "number",
        MetaCol::Utf8 => "string",
        MetaCol::Bool => "boolean",
    }
}

fn mirror_type_name(m: &MetaMirror) -> &'static str {
    match m {
        MetaMirror::Num(_) => "number",
        MetaMirror::Str(_) => "string",
        MetaMirror::Bool(_) => "boolean",
        MetaMirror::List => "list",
    }
}

#[derive(Debug)]
pub struct QueryHit {
    pub key: String,
    pub distance: f32,
    pub metadata: Option<serde_json::Value>,
}

/// A stored vector as returned by the point-lookup/scan paths (GetVectors /
/// ListVectors): full row, distance computed by the caller if it wants one.
pub struct FoundVector {
    pub key: String,
    pub vector: Vec<f32>,
    pub metadata: Option<serde_json::Value>,
}

/// `key IN (lit(k1), …, lit(kn))` — the arrow/datafusion-bound key predicate
/// shared by get/delete. Injection safety: values ride as typed literals in
/// the serialized expression plan, never as SQL text (see module doc).
fn keys_in(keys: &[String]) -> lancedb::expr::DfExpr {
    is_in(
        col("key"),
        keys.iter().map(|k| lit(k.clone())).collect::<Vec<_>>(),
    )
}

/// Read whole rows (key, vector, metadata) from a scan batch.
fn found_from_batch(batch: &RecordBatch) -> Result<Vec<FoundVector>, StoreError> {
    let Some(keys) = batch.column_by_name("key") else {
        return Err(StoreError::Internal("missing key column".into()));
    };
    let keys = keys
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| StoreError::Internal("key column not utf8".into()))?;
    let Some(vectors) = batch.column_by_name("vector") else {
        return Err(StoreError::Internal("missing vector column".into()));
    };
    let vectors = vectors
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .ok_or_else(|| StoreError::Internal("vector column not fixed_size_list".into()))?;
    let metas = batch
        .column_by_name("metadata_json")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let v = vectors.value(i);
        let v = v
            .as_any()
            .downcast_ref::<Float32Array>()
            .ok_or_else(|| StoreError::Internal("vector values not float32".into()))?;
        out.push(FoundVector {
            key: keys.value(i).to_string(),
            vector: v.values().to_vec(),
            metadata: metadata_at(metas, i),
        });
    }
    Ok(out)
}

/// Filter execution route for QueryVectors (M1 T5). Both routes produce
/// IDENTICAL result sets (pinned by `query_routes_agree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRoute {
    /// Translate the filter into an engine-side predicate over the mirror
    /// columns (hybrid: untranslatable parts stay in the exact Rust
    /// post-filter). Falls back to the memory path when nothing translates.
    Pushdown,
    /// Spike-era reference path: full scan + in-memory filter evaluation.
    /// Kept public as the correctness baseline and the perf A/B control.
    Memory,
}

/// QueryVectors outcome: the (distance, key)-ordered hits plus the executed
/// dataset version (pagination tokens carry it; see `query_vectors`).
#[derive(Debug)]
pub struct QueryOutcome {
    pub hits: Vec<QueryHit>,
    pub version: u64,
}

/// Over-fetch margin for the pushdown route: the engine truncates its KNN at
/// `limit` with ITS tie-break; our contract orders ties by key. Fetching a
/// slack of extra rows lets the Rust re-sort apply the key tie-break for tie
/// groups up to this size at the truncation boundary. (The memory route never
/// truncates early so it is exact; AWS's own tie behavior is undocumented —
/// TODO(AWS 对拍).)
const TIE_SLACK: usize = 32;

/// Flat exact KNN (M1 T5; pre-filter semantics — ENHANCED always, §4.4).
/// - no filter: lancedb vector search (`query().nearest_to(v).distance_type(t).limit(k)`)
/// - with filter, Pushdown route: engine scalar predicate (mirror columns) +
///   KNN over the survivors (`only_if_expr` + `nearest_to`, lance prefilter),
///   then the exact in-memory post-filter over the metadata blob (hybrid —
///   the pushed predicate is only a necessary condition, see filter.rs).
/// - with filter, Memory route (or untranslatable filter): full scan +
///   Rust-side distance + in-memory filter, the spike path (M0 REPORT 遗留 1
///   reference).
///
/// All filtered paths end sorted by (distance asc, key asc) and truncated to
/// top_k — the deterministic total order QueryVectors pagination slices.
#[allow(clippy::too_many_arguments)]
async fn query_table(
    table: &Table,
    dim: i32,
    metric: &str,
    query: &[f32],
    top_k: usize,
    filter: Option<&Filter>,
    route: QueryRoute,
    cols: &BTreeMap<String, MetaCol>,
) -> Result<Vec<QueryHit>, StoreError> {
    let mut hits: Vec<QueryHit> = Vec::new();

    let pred = match (route, filter) {
        (QueryRoute::Pushdown, Some(f)) => f.pushdown(cols),
        _ => None,
    };

    match (filter, pred) {
        (None, _) => {
            let stream = table
                .query()
                .limit(top_k)
                .nearest_to(query.to_vec())
                .map_err(internal)?
                .distance_type(distance_type(metric))
                .execute()
                .await
                .map_err(internal)?;
            let mut stream = stream;
            use futures_util::StreamExt;
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(internal)?;
                hits.extend(hits_from_batch(&batch)?);
            }
            // lancedb 0.41 L2 skips the final sqrt (squared distance, see
            // lance-linalg l2.rs reference test); AWS euclidean is true L2 —
            // sqrt is order-preserving so KNN order/ties are unaffected.
            if metric == "euclidean" {
                for h in &mut hits {
                    h.distance = h.distance.sqrt();
                }
            }
            hits.sort_by(hit_order);
            return Ok(hits);
        }
        (Some(f), Some(pred)) => {
            // pushdown route: engine prefilter KNN over mirror columns
            let stream = table
                .query()
                .only_if_expr(pred)
                .nearest_to(query.to_vec())
                .map_err(internal)?
                .distance_type(distance_type(metric))
                .limit(top_k + TIE_SLACK)
                .execute()
                .await
                .map_err(internal)?;
            let mut stream = stream;
            use futures_util::StreamExt;
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(internal)?;
                hits.extend(hits_from_batch(&batch)?);
            }
            if metric == "euclidean" {
                for h in &mut hits {
                    h.distance = h.distance.sqrt();
                }
            }
            // exact post-filter (blob is authoritative; the pushed predicate
            // may have over-matched for untranslatable parts of the filter)
            hits.retain(|h| f.eval(h.metadata.as_ref().unwrap_or(&serde_json::Value::Null)));
        }
        (Some(f), None) => {
            // memory route (or nothing translatable): full scan, Rust distance
            let stream = table.query().execute().await.map_err(internal)?;
            let mut stream = stream;
            use futures_util::StreamExt;
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(internal)?;
                hits.extend(hits_from_batch_with_distance(&batch, dim, metric, query)?);
            }
            hits.retain(|h| f.eval(h.metadata.as_ref().unwrap_or(&serde_json::Value::Null)));
        }
    }

    hits.sort_by(hit_order);
    hits.truncate(top_k);
    Ok(hits)
}

fn hit_order(a: &QueryHit, b: &QueryHit) -> std::cmp::Ordering {
    a.distance
        .partial_cmp(&b.distance)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.key.cmp(&b.key))
}

/// Read (key, _distance, metadata) from a batch that carries `_distance`.
fn hits_from_batch(batch: &RecordBatch) -> Result<Vec<QueryHit>, StoreError> {
    let Some(keys) = batch.column_by_name("key") else {
        return Err(StoreError::Internal("missing key column".into()));
    };
    let keys = keys
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| StoreError::Internal("key column not utf8".into()))?;
    let Some(dist) = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
    else {
        return Err(StoreError::Internal("missing _distance column".into()));
    };
    let metas = batch
        .column_by_name("metadata_json")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(QueryHit {
            key: keys.value(i).to_string(),
            distance: dist.value(i),
            metadata: metadata_at(metas, i),
        });
    }
    Ok(out)
}

/// Read (key, vector, metadata) and compute the distance in Rust (filtered path).
fn hits_from_batch_with_distance(
    batch: &RecordBatch,
    dim: i32,
    metric: &str,
    query: &[f32],
) -> Result<Vec<QueryHit>, StoreError> {
    let Some(keys) = batch.column_by_name("key") else {
        return Err(StoreError::Internal("missing key column".into()));
    };
    let keys = keys
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| StoreError::Internal("key column not utf8".into()))?;
    let Some(vectors) = batch.column_by_name("vector") else {
        return Err(StoreError::Internal("missing vector column".into()));
    };
    let vectors = vectors
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .ok_or_else(|| StoreError::Internal("vector column not fixed_size_list".into()))?;
    let metas = batch
        .column_by_name("metadata_json")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let v = vectors.value(i);
        let v = v
            .as_any()
            .downcast_ref::<Float32Array>()
            .ok_or_else(|| StoreError::Internal("vector values not float32".into()))?;
        let vals = &v.values()[0..dim as usize];
        out.push(QueryHit {
            key: keys.value(i).to_string(),
            distance: rust_distance(vals, query, metric),
            metadata: metadata_at(metas, i),
        });
    }
    Ok(out)
}

fn metadata_at(col: Option<&StringArray>, i: usize) -> Option<serde_json::Value> {
    col.and_then(|c| {
        if c.is_null(i) {
            None
        } else {
            serde_json::from_str(c.value(i)).ok()
        }
    })
}

/// Rust-side distance for the filtered path. Mirrors lancedb semantics:
/// cosine = 1 - cos_sim (range [0,2]), euclidean = L2.
fn rust_distance(a: &[f32], b: &[f32], metric: &str) -> f32 {
    match metric {
        "cosine" => {
            let mut dot = 0f32;
            let (mut na, mut nb) = (0f32, 0f32);
            for i in 0..a.len() {
                dot += a[i] * b[i];
                na += a[i] * a[i];
                nb += b[i] * b[i];
            }
            1.0 - dot / (na.sqrt() * nb.sqrt())
        }
        _ => a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            .sqrt(),
    }
}

// ---------------------------------------------------------------------------
// tests (M1 T5): dual-route consistency + pushdown-vs-memory performance
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;

    /// Tiny deterministic LCG (no rand dep): stable datasets across runs.
    fn lcg(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s >> 11
        }
    }

    fn row(key: String, n: u64, dim: i32, meta: serde_json::Value) -> VectorRow {
        let mut r = lcg(n + 1);
        let vec: Vec<f32> = (0..dim).map(|_| (r() % 1000) as f32 / 1000.0).collect();
        let mut filterable = BTreeMap::new();
        if let serde_json::Value::Object(map) = &meta {
            for (k, v) in map {
                let mv = match v {
                    serde_json::Value::String(s) => MetaMirror::Str(s.clone()),
                    serde_json::Value::Number(num) => {
                        MetaMirror::Num(num.as_f64().expect("finite"))
                    }
                    serde_json::Value::Bool(b) => MetaMirror::Bool(*b),
                    serde_json::Value::Array(_) => MetaMirror::List,
                    _ => unreachable!("test data"),
                };
                filterable.insert(k.clone(), mv);
            }
        }
        VectorRow {
            key,
            vector: vec,
            metadata_json: if meta.is_null() {
                None
            } else {
                Some(meta.to_string())
            },
            filterable,
        }
    }

    /// Consistency dataset: every metadata shape the filter engine knows —
    /// scalars of all three kinds, lists (homogeneous + heterogeneous), rows
    /// with no metadata at all, keys that exist on only SOME rows.
    fn consistency_dataset(n: usize, dim: i32) -> Vec<VectorRow> {
        (0..n)
            .map(|i| {
                let i = i as u64;
                let key = format!("k{:04}", i);
                let meta = if i.is_multiple_of(11) {
                    serde_json::Value::Null // no metadata
                } else if i.is_multiple_of(7) {
                    json!({"only_list": ["x", "y"]}) // never mirrored
                } else {
                    let mut m = json!({
                        "i": i, "mod": i % 13, "tag": format!("t{}", i % 5), "ok": i.is_multiple_of(2),
                    });
                    if i.is_multiple_of(3) {
                        m["tags"] = json!(["a", i % 100, true]); // hetero list
                    }
                    m
                };
                row(key, i, dim, meta)
            })
            .collect()
    }

    async fn setup(name: &str, batches: Vec<Vec<VectorRow>>, dim: i32) -> Storage {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::file(dir.path());
        // leak the tempdir: keep the dataset alive for the test body (tests
        // are short-lived; the OS reclaims on exit)
        std::mem::forget(dir);
        storage
            .create_bucket(&BucketMeta {
                vector_bucket_name: "vb".into(),
                vector_bucket_arn: "arn:aws:s3vectors:us-east-1:0:bucket/vb".to_string(),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                default_index_mode: None,
            })
            .await
            .unwrap();
        storage
            .create_index(&IndexMeta {
                vector_bucket_name: "vb".into(),
                index_name: name.into(),
                index_arn: format!("arn:aws:s3vectors:us-east-1:0:bucket/vb/index/{name}"),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                data_type: "float32".into(),
                dimension: dim,
                distance_metric: "euclidean".into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
            })
            .await
            .unwrap();
        for b in batches {
            storage.upsert_vectors("vb", name, dim, b).await.unwrap();
        }
        storage
    }

    /// THE dual-path contract (task pin): the same assertion set must produce
    /// the same results on the pushdown route and the pure in-memory route.
    /// Exercises every operator, partially-translatable filters (list keys,
    /// type-mismatched literals), untranslatable $or branches, exists on both
    /// mirrored and unmirrored keys, and KNN ordering/truncation under topK.
    #[tokio::test]
    async fn query_routes_agree() {
        let dim = 4;
        let n = 444;
        // split into two batches so schema evolution + a second merge under
        // an EVOLVED schema are both on the code path
        let ds = consistency_dataset(n, dim);
        let (b1, b2): (Vec<VectorRow>, Vec<VectorRow>) =
            ds.into_iter()
                .enumerate()
                .fold((Vec::new(), Vec::new()), |(mut a, mut b), (i, r)| {
                    if i < 100 {
                        a.push(r);
                    } else {
                        b.push(r);
                    }
                    (a, b)
                });
        let storage = setup("ixc", vec![b1, b2], dim).await;
        let query: Vec<f32> = vec![0.5, 0.25, 0.75, 0.125];

        let filters = vec![
            r#"{"i": 42}"#,
            r#"{"i": {"$eq": 42.0}}"#,
            r#"{"i": {"$eq": "42"}}"#, // type mismatch
            r#"{"i": {"$ne": 42}}"#,   // missing rows match
            r#"{"i": {"$gt": 400}}"#,
            r#"{"i": {"$gte": 400}}"#,
            r#"{"mod": {"$lt": 3}}"#,
            r#"{"mod": {"$lte": 3}}"#,
            r#"{"i": {"$gte": 100, "$lt": 150}}"#, // same-field range
            r#"{"tag": {"$startsWith": "t"}}"#,
            r#"{"tag": {"$startsWith": "t1"}}"#,
            r#"{"tag": {"$startsWith": "zzz"}}"#,
            r#"{"tag": 7}"#, // mismatch shorthand
            r#"{"ok": true}"#,
            r#"{"ok": {"$ne": false}}"#,
            r#"{"tags": "a"}"#, // list any-element
            r#"{"tags": {"$in": [99, 98]}}"#,
            r#"{"tags": {"$nin": ["a"]}}"#,
            r#"{"only_list": "x"}"#, // unmirrored key
            r#"{"only_list": {"$exists": true}}"#,
            r#"{"ghost": {"$exists": false}}"#,
            r#"{"i": {"$exists": true}}"#,
            r#"{"mod": {"$in": [0, 1, 2, "3"]}}"#, // mixed-type $in
            r#"{"mod": {"$nin": [0, 1, 2, "3"]}}"#,
            r#"{"$and": [{"i": {"$gt": 200}}, {"tag": {"$startsWith": "t2"}}]}"#,
            r#"{"$or": [{"i": {"$lt": 10}}, {"ok": false}]}"#,
            // $or with an untranslatable branch (list key) -> whole $or stays
            // in the exact post-filter; the other conjunct still pushes down
            r#"{"$or": [{"tags": "a"}, {"i": {"$lt": 5}}], "tag": "t0"}"#,
            r#"{"$and": [{"$or": [{"i": 1}, {"i": 2}]}, {"ok": true}]}"#,
            r#"{}"#,
        ];

        for (fi, f) in filters.iter().enumerate() {
            let filter = Filter::compile(&serde_json::from_str::<serde_json::Value>(f).unwrap())
                .unwrap_or_else(|e| panic!("filter {f}: {e}"));
            for top_k in [5usize, 600] {
                let pd = storage
                    .query_vectors(
                        "vb",
                        "ixc",
                        dim,
                        "euclidean",
                        &query,
                        top_k,
                        Some(&filter),
                        QueryRoute::Pushdown,
                        None,
                    )
                    .await
                    .unwrap_or_else(|e| panic!("pushdown {f}: {e}"));
                let mem = storage
                    .query_vectors(
                        "vb",
                        "ixc",
                        dim,
                        "euclidean",
                        &query,
                        top_k,
                        Some(&filter),
                        QueryRoute::Memory,
                        None,
                    )
                    .await
                    .unwrap_or_else(|e| panic!("memory {f}: {e}"));
                let fmt = |hs: &[QueryHit]| {
                    hs.iter()
                        .map(|h| format!("{}:{:.4}", h.key, h.distance))
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    fmt(&pd.hits),
                    fmt(&mem.hits),
                    "route disagreement: filter={f} topK={top_k} case#{fi}"
                );
                assert!(pd.hits.len() <= top_k);
            }
        }

        // no-filter sanity: routes coincide too
        let pd = storage
            .query_vectors(
                "vb",
                "ixc",
                dim,
                "euclidean",
                &query,
                7,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        let mem = storage
            .query_vectors(
                "vb",
                "ixc",
                dim,
                "euclidean",
                &query,
                7,
                None,
                QueryRoute::Memory,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            pd.hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            mem.hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        );
    }

    /// QueryVectors pagination snapshot at the storage layer: `as_of` pins the
    /// dataset version (lance time-travel) so later writes are invisible to
    /// follow-up pages; an unknown version maps to StoreError::Validation
    /// (the ops layer surfaces it as an invalid nextToken).
    #[tokio::test]
    async fn query_version_pinning() {
        let dim = 4;
        let ds = consistency_dataset(60, dim);
        let storage = setup("ixv", vec![ds], dim).await;
        let query: Vec<f32> = vec![0.5, 0.25, 0.75, 0.125];

        let v1 = storage
            .query_vectors(
                "vb",
                "ixv",
                dim,
                "euclidean",
                &query,
                10,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap()
            .version;
        // write behind the cursor
        storage
            .upsert_vectors(
                "vb",
                "ixv",
                dim,
                vec![row("zz99".into(), 9999, dim, json!({"i": 9999}))],
            )
            .await
            .unwrap();
        let pinned = storage
            .query_vectors(
                "vb",
                "ixv",
                dim,
                "euclidean",
                &query,
                1000,
                None,
                QueryRoute::Pushdown,
                Some(v1),
            )
            .await
            .unwrap();
        assert_eq!(pinned.version, v1);
        assert!(
            !pinned.hits.iter().any(|h| h.key == "zz99"),
            "pinned query must not see later writes"
        );
        // the LIVE read does see it
        let live = storage
            .query_vectors(
                "vb",
                "ixv",
                dim,
                "euclidean",
                &query,
                1000,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        assert!(live.hits.iter().any(|h| h.key == "zz99"));
        assert!(live.version > v1);
        // unknown version -> Validation (stale nextToken)
        let err = storage
            .query_vectors(
                "vb",
                "ixv",
                dim,
                "euclidean",
                &query,
                5,
                None,
                QueryRoute::Pushdown,
                Some(999_999),
            )
            .await;
        assert!(
            matches!(&err, Err(StoreError::Validation(m)) if m.contains("nextToken")),
            "{err:?}"
        );
    }

    /// Metadata mirror evolution at the storage layer: two-step write with
    /// new keys appearing mid-batch, and the drift registry catching
    /// number->string and scalar->list across batches (the HTTP face of this
    /// is ops::tests::metadata_evolution_and_type_drift).
    #[tokio::test]
    async fn mirror_evolution_and_drift() {
        let dim = 2;
        let b1 = vec![row("a".into(), 1, dim, json!({"i": 1}))];
        let storage = setup("ixe2", vec![b1], dim).await;
        // three new keys at once
        let b2 = vec![row(
            "b".into(),
            2,
            dim,
            json!({"i": 2, "s": "x", "f": 0.5, "ok": true, "l": [1]}),
        )];
        storage.upsert_vectors("vb", "ixe2", dim, b2).await.unwrap();
        // drift: i number -> string
        let e = storage
            .upsert_vectors(
                "vb",
                "ixe2",
                dim,
                vec![row("c".into(), 3, dim, json!({"i": "3"}))],
            )
            .await;
        assert!(
            matches!(&e, Err(StoreError::Validation(m)) if m.contains("\"i\"")),
            "{e:?}"
        );
        // drift: s string -> list
        let e = storage
            .upsert_vectors(
                "vb",
                "ixe2",
                dim,
                vec![row("c".into(), 3, dim, json!({"s": ["x"]}))],
            )
            .await;
        assert!(matches!(e, Err(StoreError::Validation(_))), "{e:?}");
        // list stays list: fine
        storage
            .upsert_vectors(
                "vb",
                "ixe2",
                dim,
                vec![row("c".into(), 3, dim, json!({"l": ["y", 2]}))],
            )
            .await
            .unwrap();
    }

    /// Pushdown-vs-memory performance reference (M1 T5 交付数据; run manually):
    ///
    ///   cargo test --lib perf_filter_pushdown_vs_memory -- --ignored --nocapture
    ///
    /// 口径: engine-direct (Storage::query_vectors, no HTTP/SigV4 stack),
    /// file backend on a temp dir, 100,000 vectors dim=8 euclidean,
    /// metadata {"i":n,"mod":n%100,"tag":"t{n%50}"} ingested once via 200
    /// merge_insert batches of 500 (the boto3 batch shape), topK=10,
    /// N=30 timed iterations per filter per route (first iteration discarded
    /// as warm-up), P50/P99 in milliseconds. Last run's numbers are recorded
    /// in the module comment above (see "性能对照" in store.rs header notes)
    /// and in the M1 T5 delivery report.
    #[tokio::test]
    #[ignore = "manual perf run: ~2min (100k ingest + 300 timed queries)"]
    async fn perf_filter_pushdown_vs_memory() {
        let dim = 8;
        let total = 100_000usize;
        let batch = 500usize;
        let mut batches: Vec<Vec<VectorRow>> = Vec::with_capacity(total.div_ceil(batch));
        let mut cur: Vec<VectorRow> = Vec::with_capacity(batch);
        for n in 0..total {
            let n = n as u64;
            let meta = json!({"i": n, "mod": n % 100, "tag": format!("t{}", n % 50)});
            cur.push(row(format!("k{:06}", n), n, dim, meta));
            if cur.len() == batch {
                batches.push(std::mem::replace(&mut cur, Vec::with_capacity(batch)));
            }
        }
        if !cur.is_empty() {
            batches.push(cur);
        }
        let t0 = std::time::Instant::now();
        let storage = setup("ixperf", batches, dim).await;
        eprintln!(
            "ingest: {} rows in {:.1}s",
            total,
            t0.elapsed().as_secs_f64()
        );

        let big_in_list = format!(
            "[{}]",
            (0..50).map(|m| m.to_string()).collect::<Vec<_>>().join(",")
        );
        let shapes: Vec<(&str, String)> = vec![
            ("eq-high  tag=t7 (2000)", r#"{"tag": "t7"}"#.into()),
            ("eq-low   i=4242 (1)", r#"{"i": 4242}"#.into()),
            (
                "range    i>=99000 (1000)",
                r#"{"i": {"$gte": 99000}}"#.into(),
            ),
            (
                "in-list  mod in 0..50 (~49500)",
                format!(r#"{{"mod": {{"$in": {big_in_list}}}}}"#),
            ),
            (
                "startsWith tag^t4 (~22000)",
                r#"{"tag": {"$startsWith": "t4"}}"#.into(),
            ),
        ];

        let query: Vec<f32> = (0..dim).map(|d| 0.1 + d as f32 * 0.05).collect();
        let iters = 30usize;
        eprintln!(
            "{:<28} {:>16} {:>16}   speedup(P50)",
            "filter", "pushdown P50/P99", "memory P50/P99"
        );
        for (name, f) in &shapes {
            let filter = Filter::compile(&serde_json::from_str::<serde_json::Value>(f).unwrap())
                .expect(name);
            let mut pd_ms = Vec::with_capacity(iters);
            let mut mem_ms = Vec::with_capacity(iters);
            for route_times in [
                (&mut pd_ms, QueryRoute::Pushdown),
                (&mut mem_ms, QueryRoute::Memory),
            ] {
                let (times, route) = route_times;
                for it in 0..=iters {
                    let t = std::time::Instant::now();
                    let out = storage
                        .query_vectors(
                            "vb",
                            "ixperf",
                            dim,
                            "euclidean",
                            &query,
                            10,
                            Some(&filter),
                            route,
                            None,
                        )
                        .await
                        .unwrap();
                    let dt = t.elapsed().as_secs_f64() * 1000.0;
                    if it > 0 {
                        times.push(dt); // discard warm-up
                    }
                    assert_eq!(out.hits.len(), out.hits.len().min(10));
                }
                times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            }
            let p = |v: &[f64]| {
                let q = |p: f64| v[((v.len() as f64 - 1.0) * p).round() as usize];
                (q(0.50), q(0.99))
            };
            let (pm, p99) = p(&pd_ms);
            let (mm, m99) = p(&mem_ms);
            eprintln!(
                "{:<28} {:>7.1} / {:>6.1} ms {:>7.1} / {:>6.1} ms   {:.2}x",
                name,
                pm,
                p99,
                mm,
                m99,
                mm / pm
            );
        }
        eprintln!(
            "(perf_filter_pushdown_vs_memory, {} vectors dim={} file backend)",
            total, dim
        );
    }

    use serde_json::json;
}
