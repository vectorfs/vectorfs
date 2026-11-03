//! Storage backend (M0 Task B: file; Task D: + S3-compatible via object_store).
//! - catalog: `buckets/<name>/bucket.json`, `buckets/<vb>/indexes/<ix>/index.json`
//!   (file: O_EXCL create; S3: `PutMode::Create` conditional put — both are
//!   race-free create-if-not-exists);
//! - engine: lancedb (the ONLY module that touches lancedb API, design.md §5.7).
//!   One Lance table per index at `<index>/data/vectors.lance` (file) or
//!   `s3://<bucket>[/<prefix>]/buckets/<vb>/indexes/<ix>/data` (S3), opened via a
//!   fresh `Connection` per operation (spike simplification — no connection cache).
//!
//! Lance schema: `key: utf8 not null`, `vector: fixed_size_list<f32, dim>` (not null),
//! `metadata_json: utf8 nullable` (metadata blob — no dynamic columns in the spike).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
};
// Arrow numeric type marker used by `as_primitive`.
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_schema::{ArrowError, DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, DistanceType, Table};

use crate::s3::S3BackendConfig;

use object_store::ObjectStoreExt as _; // get/delete/put_opts on Arc<dyn ObjectStore>

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
// backend selection (Task D)
// ---------------------------------------------------------------------------

/// A configured storage backend. All catalog + engine operations go through
/// here; the S3 variant carries the object_store client (catalog) and the
/// custom-registry lancedb Session (engine).
pub enum Storage {
    File(PathBuf),
    S3 {
        cfg: S3BackendConfig,
        session: Arc<lancedb::Session>,
        catalog: Arc<dyn object_store::ObjectStore>,
    },
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Storage::File(p) => write!(f, "file:{}", p.display()),
            Storage::S3 { cfg, .. } => write!(f, "s3:{}/{}{}", cfg.bucket, cfg.prefix, cfg.endpoint),
        }
    }
}

impl Storage {
    pub fn file(root: impl Into<PathBuf>) -> Self {
        Storage::File(root.into())
    }

    pub fn s3(cfg: S3BackendConfig) -> Result<Self, String> {
        let catalog = cfg.build_catalog_store().map_err(|e| e.to_string())?;
        Ok(Storage::S3 {
            cfg,
            session: S3BackendConfig::build_session(),
            catalog,
        })
    }

    // ---- catalog ----

    pub async fn load_bucket(&self, name: &str) -> Result<Option<BucketMeta>, StoreError> {
        match self {
            Storage::File(root) => load_bucket_file(root, name).await,
            Storage::S3 { cfg, catalog, .. } => {
                let key = cfg.catalog_key(&format!("buckets/{name}/bucket.json"));
                read_json_s3(catalog, &key).await
            }
        }
    }

    pub async fn load_index(&self, vb: &str, ix: &str) -> Result<Option<IndexMeta>, StoreError> {
        match self {
            Storage::File(root) => load_index_file(root, vb, ix).await,
            Storage::S3 { cfg, catalog, .. } => {
                let key = cfg.catalog_key(&format!("buckets/{vb}/indexes/{ix}/index.json"));
                read_json_s3(catalog, &key).await
            }
        }
    }

    pub async fn create_bucket(&self, meta: &BucketMeta) -> Result<(), StoreError> {
        match self {
            Storage::File(root) => create_bucket_file(root, meta).await,
            Storage::S3 { cfg, catalog, .. } => {
                let key = cfg.catalog_key(&format!(
                    "buckets/{}/bucket.json",
                    meta.vector_bucket_name
                ));
                put_json_create_s3(catalog, &key, &serde_json::to_vec_pretty(meta).map_err(internal)?)
                    .await
            }
        }
    }

    pub async fn create_index(&self, meta: &IndexMeta) -> Result<(), StoreError> {
        match self {
            Storage::File(root) => create_index_file(root, meta).await,
            Storage::S3 { cfg, catalog, .. } => {
                let key = cfg.catalog_key(&format!(
                    "buckets/{}/indexes/{}/index.json",
                    meta.vector_bucket_name, meta.index_name
                ));
                put_json_create_s3(catalog, &key, &serde_json::to_vec_pretty(meta).map_err(internal)?)
                    .await?;
                if let Err(e) = self.create_lance_table(meta).await {
                    let _ = catalog.delete(&key).await; // best-effort rollback
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    // ---- engine ----

    async fn open_table(&self, vb: &str, ix: &str) -> Result<Table, StoreError> {
        match self {
            Storage::File(root) => open_table_file(root, vb, ix).await,
            Storage::S3 { cfg, session, .. } => {
                crate::s3::open_table(cfg, session, vb, ix)
                    .await
                    .map_err(internal)
            }
        }
    }

    async fn create_lance_table(&self, meta: &IndexMeta) -> Result<(), StoreError> {
        match self {
            Storage::File(root) => create_lance_table_file(root, meta).await,
            Storage::S3 { cfg, session, .. } => {
                let conn = cfg
                    .connect(session, &meta.vector_bucket_name, &meta.index_name)
                    .await
                    .map_err(internal)?;
                conn.create_empty_table(TABLE_NAME, arrow_schema(meta.dimension))
                    .execute()
                    .await
                    .map_err(internal)?;
                Ok(())
            }
        }
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
// paths (file backend)
// ---------------------------------------------------------------------------

pub fn bucket_dir(root: &Path, name: &str) -> PathBuf {
    root.join("buckets").join(name)
}

pub fn index_dir(root: &Path, vb: &str, ix: &str) -> PathBuf {
    bucket_dir(root, vb).join("indexes").join(ix)
}

fn lance_root(root: &Path, vb: &str, ix: &str) -> PathBuf {
    index_dir(root, vb, ix).join("data")
}

// ---------------------------------------------------------------------------
// catalog JSON — file backend
// ---------------------------------------------------------------------------

async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(internal),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(internal(e)),
    }
}

async fn load_bucket_file(root: &Path, name: &str) -> Result<Option<BucketMeta>, StoreError> {
    read_json(&bucket_dir(root, name).join("bucket.json")).await
}

async fn load_index_file(root: &Path, vb: &str, ix: &str) -> Result<Option<IndexMeta>, StoreError> {
    read_json(&index_dir(root, vb, ix).join("index.json")).await
}

/// Create bucket.json exclusively. Existing file -> Conflict.
async fn create_bucket_file(root: &Path, meta: &BucketMeta) -> Result<(), StoreError> {
    let dir = bucket_dir(root, &meta.vector_bucket_name);
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let path = dir.join("bucket.json");
    let json = serde_json::to_vec_pretty(meta).map_err(internal)?;
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
    {
        Ok(mut f) => {
            use tokio::io::AsyncWriteExt;
            f.write_all(&json).await.map_err(internal)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(StoreError::Conflict(
            format!("vector bucket {} already exists", meta.vector_bucket_name),
        )),
        Err(e) => Err(internal(e)),
    }
}

/// Create index.json exclusively + the empty Lance table.
/// On Lance failure the index.json is removed again (best effort) so the
/// catalog never advertises an index without a dataset.
async fn create_index_file(root: &Path, meta: &IndexMeta) -> Result<(), StoreError> {
    let dir = index_dir(root, &meta.vector_bucket_name, &meta.index_name);
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let json = serde_json::to_vec_pretty(meta).map_err(internal)?;
    let path = dir.join("index.json");
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
    {
        Ok(mut f) => {
            use tokio::io::AsyncWriteExt;
            f.write_all(&json).await.map_err(internal)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(StoreError::Conflict(format!(
                "index {} already exists",
                meta.index_name
            )))
        }
        Err(e) => return Err(internal(e)),
    }
    if let Err(e) = create_lance_table_file(root, meta).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// catalog JSON — S3 backend (object_store, conditional put)
// ---------------------------------------------------------------------------

async fn read_json_s3<T: serde::de::DeserializeOwned>(
    store: &Arc<dyn object_store::ObjectStore>,
    key: &object_store::path::Path,
) -> Result<Option<T>, StoreError> {
    match store.get(key).await {
        Ok(res) => {
            let bytes = res.bytes().await.map_err(internal)?;
            serde_json::from_slice(&bytes).map(Some).map_err(internal)
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(internal(e)),
    }
}

/// Conditional create via `PutMode::Create` (If-None-Match:* on S3-compatible
/// stores). Existing object -> Conflict.
async fn put_json_create_s3(
    store: &Arc<dyn object_store::ObjectStore>,
    key: &object_store::path::Path,
    json: &[u8],
) -> Result<(), StoreError> {
    use object_store::{PutMode, PutOptions};
    store
        .put_opts(key, json.to_vec().into(), PutOptions { mode: PutMode::Create, ..Default::default() })
        .await
        .map(|_| ())
        .map_err(|e| match e {
            object_store::Error::AlreadyExists { .. } => StoreError::Conflict(format!(
                "object {} already exists",
                key.as_ref()
            )),
            other => internal(other),
        })
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

async fn open_table_file(root: &Path, vb: &str, ix: &str) -> Result<Table, StoreError> {
    let conn = connect(lance_root(root, vb, ix).to_str().expect("utf8 path"))
        .execute()
        .await
        .map_err(internal)?;
    conn.open_table(TABLE_NAME).execute().await.map_err(internal)
}

async fn create_lance_table_file(root: &Path, meta: &IndexMeta) -> Result<(), StoreError> {
    let conn = connect(lance_root(root, &meta.vector_bucket_name, &meta.index_name).to_str().expect("utf8 path"))
        .execute()
        .await
        .map_err(internal)?;
    conn.create_empty_table(TABLE_NAME, arrow_schema(meta.dimension))
        .execute()
        .await
        .map_err(internal)?;
    Ok(())
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
async fn upsert_table(
    table: &Table,
    dim: i32,
    rows: Vec<VectorRow>,
) -> Result<(), StoreError> {
    let schema = arrow_schema(dim);

    let keys: StringArray = rows.iter().map(|r| Some(r.key.as_str())).collect();
    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|r| Some(r.vector.iter().map(|v| Some(*v)).collect::<Vec<_>>())),
        dim,
    );
    let metas: StringArray = rows
        .iter()
        .map(|r| r.metadata_json.as_deref())
        .collect();

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
    let reader = RecordBatchIterator::new(
        vec![Ok::<_, ArrowError>(batch)].into_iter(),
        schema,
    );
    let mut merge = table.merge_insert(&["key"]);
    merge
        .when_matched_update_all(None)
        .when_not_matched_insert_all();
    merge
        .execute(Box::new(reader))
        .await
        .map_err(internal)?;
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
