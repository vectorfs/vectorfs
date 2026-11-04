//! Storage facade over the unified backend (M1 T2; design.md §5.2/§5.3).
//!
//! Two responsibilities:
//! - catalog: `buckets/<name>/bucket.json`,
//!   `buckets/<vb>/indexes/<ix>/index.json` — ALL via object_store
//!   (`get` → NotFound → 404; create = `put_opts(PutMode::Create)`, race-free
//!   on file/memory/S3). The file backend no longer touches std::fs for
//!   catalog objects (only backend setup in backends.rs creates/canonicalizes
//!   the data dir itself).
//! - engine: lancedb (the ONLY module that touches lancedb API, design.md
//!   §5.7). One Lance table per index at `<index>/data` (file:
//!   `file://<root>/buckets/<vb>/indexes/<ix>/data`, memory:
//!   `memory://vf/…`, S3: `s3://<bucket>[/<prefix>]/…`), opened through the
//!   backend's shared `Session` (per-registry object stores). A fresh
//!   `Connection` per operation remains a spike simplification — no connection
//!   cache; M0 REPORT 遗留 3, TODO for a later M1 task.
//!
//! Lance schema: `key: utf8 not null`, `vector: fixed_size_list<f32, dim>` (not null),
//! `metadata_json: utf8 nullable` (metadata blob — no dynamic columns yet; the
//! AllNulls-evolution route is proven in `experiments.rs`, full integration is
//! M1 per design.md §5.3).

use std::sync::Arc;

use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
};
// Arrow numeric type marker used by `as_primitive`.
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_schema::{ArrowError, DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{Connection, DistanceType, Table};

use crate::backends::{self, Backend};

pub(crate) const TABLE_NAME: &str = "vectors";

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BucketMeta {
    pub vector_bucket_name: String,
    pub vector_bucket_arn: String,
    /// RFC3339 (UTC), per task spec for the on-disk file.
    pub creation_time_rfc3339: String,
    /// Unix epoch seconds (fractional) — wire format for rest-json timestamps.
    pub creation_time_epoch: f64,
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
}

#[derive(Debug)]
pub enum StoreError {
    /// bucket/index already exists -> ConflictException
    Conflict(String),
    /// anything else -> InternalServerException
    Internal(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict(m) | StoreError::Internal(m) => write!(f, "{m}"),
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
        upsert_table(&table, dim, rows).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn query_vectors(
        &self,
        vb: &str,
        ix: &str,
        dim: i32,
        metric: &str,
        query: &[f32],
        top_k: usize,
        filter: Option<&(dyn Fn(&serde_json::Value) -> bool + Send + Sync)>,
    ) -> Result<Vec<QueryHit>, StoreError> {
        let table = self.open_table(vb, ix).await?;
        query_table(&table, dim, metric, query, top_k, filter).await
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

pub struct VectorRow {
    pub key: String,
    pub vector: Vec<f32>,
    /// Pre-serialized metadata JSON; None = no metadata stored.
    pub metadata_json: Option<String>,
}

/// Upsert rows keyed on `key` (replace whole row, AWS PutVectors semantics).
/// lancedb 0.41 API actually used: `Table::merge_insert(&["key"])`
/// `.when_matched_update_all(None)` + `.when_not_matched_insert_all()`
/// `.execute(Box<dyn RecordBatchReader + Send>)`.
async fn upsert_table(table: &Table, dim: i32, rows: Vec<VectorRow>) -> Result<(), StoreError> {
    let schema = arrow_schema(dim);

    let keys: StringArray = rows.iter().map(|r| Some(r.key.as_str())).collect();
    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|r| Some(r.vector.iter().map(|v| Some(*v)).collect::<Vec<_>>())),
        dim,
    );
    let metas: StringArray = rows.iter().map(|r| r.metadata_json.as_deref()).collect();

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(keys) as Arc<dyn Array>,
            Arc::new(vectors) as Arc<dyn Array>,
            Arc::new(metas) as Arc<dyn Array>,
        ],
    )
    .map_err(internal)?;

    // arrow's RecordBatchIterator satisfies the `Box<dyn RecordBatchReader + Send>`
    // that MergeInsertBuilder::execute expects in 0.41.
    let reader = RecordBatchIterator::new(vec![Ok::<_, ArrowError>(batch)].into_iter(), schema);
    let mut merge = table.merge_insert(&["key"]);
    merge
        .when_matched_update_all(None)
        .when_not_matched_insert_all();
    merge.execute(Box::new(reader)).await.map_err(internal)?;
    Ok(())
}

pub struct QueryHit {
    pub key: String,
    pub distance: f32,
    pub metadata: Option<serde_json::Value>,
}

/// Flat exact KNN.
/// - no filter: lancedb vector search (`query().nearest_to(v).distance_type(t).limit(k)`)
/// - with filter: full scan + Rust-side distance + in-memory filter
///   (pre-filter semantics, ENHANCED-always per design.md §4.4), then sort/truncate.
///   TODO(M1): push the filter down as a Lance scalar predicate (M0 REPORT
///   遗留 1 — spike path is full-scan + in-memory filter; slow at 1M rows).
///
/// Both paths end sorted by (distance asc, key asc).
#[allow(clippy::too_many_arguments)]
async fn query_table(
    table: &Table,
    dim: i32,
    metric: &str,
    query: &[f32],
    top_k: usize,
    filter: Option<&(dyn Fn(&serde_json::Value) -> bool + Send + Sync)>,
) -> Result<Vec<QueryHit>, StoreError> {
    let mut hits: Vec<QueryHit> = Vec::new();

    match filter {
        None => {
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
        }
        Some(f) => {
            let stream = table.query().execute().await.map_err(internal)?;
            let mut stream = stream;
            use futures_util::StreamExt;
            let mut rows: Vec<QueryHit> = Vec::new();
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(internal)?;
                rows.extend(hits_from_batch_with_distance(&batch, dim, metric, query)?);
            }
            hits = rows
                .into_iter()
                .filter(|h| h.metadata.as_ref().map(f).unwrap_or(false))
                .collect();
        }
    }

    hits.sort_by(|a, b| {
        a.distance
            .partial_cmp(&b.distance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
    hits.truncate(top_k);
    Ok(hits)
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
