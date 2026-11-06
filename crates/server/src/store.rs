//! Storage facade over the unified bucket-pair backend (design-v3.md §3/§4;
//! M1 T2 base, v3.1 全轴换).
//!
//! Two responsibilities:
//! - catalog: `X-vf/bucket.json`, `X-vf/indexes/<ix>/index.json`,
//!   `X-vf/sources/<keyHash>.json`, `X-vf/{policy,tags}.json` — ALL via
//!   per-bucket object_store handles (`get` → NotFound → 404; create =
//!   `put_opts(PutMode::Create)`, race-free on file/memory/S3). System keys
//!   are BUCKET-ROOT-relative (D2 — no `.vectorfs/` inner prefix); the
//!   source bucket `X` holds ONLY user objects (§5.3-3).
//! - engine: lancedb (the ONLY module that touches lancedb API, design.md
//!   §5.7). One Lance table per index at `indexes/<ix>/data` inside the
//!   system bucket (file: `file://<root>/X-vf/indexes/…`, memory:
//!   `memory://X-vf/indexes/…`, s3: `s3://X-vf/indexes/…`), opened through
//!   the backend's shared `Session`. See the M1 T4/T5 notes below for the
//!   vector-surface and filter-engine paths (unchanged by v3).
//!
//! v3 lifecycle (§3-R3): `create_bucket` implements the CreateVectorBucket
//! semantics — raw CreateBucket for `X-vf`, conflict-typed hints, the
//! EMPTY-BUCKET ADOPTION rule (D7: empty + no marker + no LOCK = crash
//! residue, adopt), never creating or requiring the source bucket `X`.
//! `delete_bucket` implements the prescribed deletion order (清空 → 删
//! marker → 删 LOCK → DeleteBucket) with the BucketNotEmpty converging loop;
//! the source bucket is NEVER deleted (D4). `list_buckets` is the R9
//! suffix-scan enumeration (no central registry): s3 = ListBuckets + marker
//! 校验, file = read_dir, memory = shared key-space prefixes.
//!
//! Ownership (§3-R2): `bucket.json` carries the DEPLOYMENT identity
//! (region+account+endpoint) + placement. "我方" = identity matches; the
//! daemon instance UUID lives only in LOCK writer ids and is decoupled from
//! ownership.
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
//! itself is the per-key type registry (single writer per bucket, design-v3
//! §5.1, makes the read-classify-evolve sequence race-free).
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
//!
//! IVF_PQ 对照 (M3-T8, 2026-11-06, `cargo test --release -p
//! vectorfs-server --lib perf_ivf_pq_vs_flat_1m -- --ignored --nocapture`,
//! engine defaults — no knobs): 1,000,000 unit vectors dim=768 cosine, file
//! backend, topK=10. ingest 32.5s, IVF_PQ train 36.7s, then 30 queries ×
//! 30 iters per phase (P50/P99 ms):
//!
//! | phase | P50    | P99    | recall@10 |
//! |-------|--------|--------|-----------|
//! | flat (pre-index, exact) | 1191.5 | 1287.5 | 1.0 |
//! | ivf_pq (engine defaults) | 12.7 | 15.4 | 0.047 |
//!
//! **94x P50 — the number that justifies M3's IVF_PQ scope.** The recall
//! caveat is a DATA property, pinned by a same-scale contrast probe
//! (deleted after the run; numbers recorded here): on CLUSTERED vectors
//! (1000 centroids + noise — the shape real embedding corpora have) the
//! SAME engine defaults scored recall@10 = 100/100 = 1.0 at 1M×768 with
//! P50 12.0ms. Uniform-random 768-d vectors concentrate distances
//! (max-min gaps shrink below PQ quantization error), so ANN ranking there
//! is noise — raising nprobes 20→512 moved recall 3/50→2/50 only. Real
//! deployments query embedding output (clustered), not noise; operators
//! needing tuned recall on pathological data get the nprobes/refine knobs
//! as the evidence-gated follow-up.

use std::collections::BTreeMap;
use std::collections::HashMap;
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

// ---------------------------------------------------------------------------
// deployment identity + ownership marker (design-v3.md §3-R2/D8)
// ---------------------------------------------------------------------------

/// WHO owns a system bucket: the DEPLOYMENT (region+account+endpoint), not
/// any daemon instance. A stateless redeploy (container rebuild, new
/// machine, lost local volume) presents the same identity and re-claims
/// every existing system bucket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeploymentIdentity {
    pub region: String,
    pub account: String,
    pub endpoint: String,
}

/// The ownership marker / bucket catalog entry `X-vf/bucket.json`. R2: the
/// deployment identity + placement ARE the marker payload.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BucketMeta {
    pub vector_bucket_name: String,
    pub vector_bucket_arn: String,
    /// RFC3339 (UTC), per task spec for the on-disk file.
    pub creation_time_rfc3339: String,
    /// Unix epoch seconds (fractional) — wire format for rest-json timestamps.
    pub creation_time_epoch: f64,
    /// Ownership: the deployment this bucket belongs to (R2/D8).
    pub deployment: DeploymentIdentity,
    /// `x-vf-placement` echo (R4): `s3://X-vf@endpoint` | `file://<dir>/X-vf`
    /// | `memory://X-vf`.
    pub placement: String,
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
    /// File-layer derivation stamp (M2 T3, design.md §5.10.5): present iff
    /// this index was CREATED by the ingest worker as an embedder-derived
    /// index. The derived key (`hash(model, dim, embedder config)`) is the
    /// index's IDENTITY — the per-batch write path re-validates it and GC
    /// (T7) only touches indexes carrying this stamp. Serde is deliberately
    /// tolerant in BOTH directions: pre-T3 index.json files parse without
    /// it (`default`), and unknown future fields are ignored (module-level
    /// forward-compat rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_vf_derived: Option<DerivedIndexInfo>,
}

/// The `x_vf_derived` payload (see `IndexMeta.x_vf_derived`).
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, PartialEq)]
pub struct DerivedIndexInfo {
    /// Full sha256-hex derived key `hash(model, dim, embedder config)`.
    pub key: String,
    /// Embedder INSTANCE name the index is fanned out from.
    pub embedder: String,
    /// Model identity string (ort: model name or path; openai: model).
    pub model: String,
    /// Declared/probed output dimensionality.
    pub dim: usize,
}

// ---------------------------------------------------------------------------
// Source markers (M2 T3, design.md §5.10.1 / §5.10.5)
// ---------------------------------------------------------------------------

/// Source lifecycle state, KB-aligned naming (appendix E.2). The transition
/// RULES live in `crate::ingest::marker` (the mechanism layer); this is the
/// persisted data model only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SourceStatus {
    /// Known to the scanner, not yet picked up (or re-detected as changed).
    #[serde(rename = "PENDING")]
    Pending,
    /// A worker owns it right now.
    #[serde(rename = "IN_PROGRESS")]
    InProgress,
    /// Fan-out completed for SOME embedders; the rest are parked with a
    /// statusReason.
    #[serde(rename = "PARTIALLY_INDEXED")]
    PartiallyIndexed,
    /// Every listed embedder's chunks are in its derived index.
    #[serde(rename = "INDEXED")]
    Indexed,
    /// Attempts exhausted (poison); never auto-retried.
    #[serde(rename = "FAILED")]
    Failed,
    /// The object vanished from the source bucket; GC (T7) owns the rest.
    #[serde(rename = "DELETING")]
    Deleting,
}

impl SourceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceStatus::Pending => "PENDING",
            SourceStatus::InProgress => "IN_PROGRESS",
            SourceStatus::PartiallyIndexed => "PARTIALLY_INDEXED",
            SourceStatus::Indexed => "INDEXED",
            SourceStatus::Failed => "FAILED",
            SourceStatus::Deleting => "DELETING",
        }
    }
}

impl std::fmt::Display for SourceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One Source marker: the no-queue ingest bookkeeping (§5.10.5 — "待摄取 =
/// 无标记，或签名与标记不符"). Persisted at `X-vf/sources/<keyHash>.json`
/// via the bucket's system store.
///
/// `etag_sig` is the CHEAP change signal (real etag when the backend lists
/// one, else a size+mtime pseudo-signature — see `ingest::marker::signature_of`);
/// `content_hash` is the DEFINITIVE sha256 of the bytes, computed inside the
/// job and used for the metadata-only-rewrite skip. `misses` (v3.1 R5) is
/// the N-strike counter: consecutive scans the object was absent before the
/// DELETING flip (a transient empty/partial listing must not reap chunks).
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, PartialEq)]
pub struct SourceMarker {
    /// Full object key relative to the SOURCE bucket root (S3-semantic key).
    pub object_key: String,
    /// Cheap listing signature at the last observed state.
    pub etag_sig: String,
    /// sha256 hex of the object bytes at the last completed ingest.
    #[serde(default)]
    pub content_hash: String,
    /// Pipeline fingerprint of the ingest section that owns this marker.
    pub pipeline_fingerprint: String,
    pub status: SourceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub retries_exhausted: bool,
    /// Consecutive scans the object was ABSENT (N-strike, R5); reset to 0 by
    /// any present sighting.
    #[serde(default)]
    pub misses: u32,
    /// RFC3339 (UTC) of the last marker write.
    pub updated_at: String,
    /// Per-modality chunk counts of the last (partial or full) completion —
    /// the raw material for GetSource's `chunks[{index,count}]` (appendix
    /// E.2; the wire adapter lands with T6). Absent until something lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<BTreeMap<String, u32>>,
    /// Engine versions of the stack that produced this completion (engine
    /// observability, 2026-11-05): engine upgrades change extraction output
    /// WITHOUT changing the pipeline fingerprint, and the re-embed ruling
    /// keeps that the operator's explicit call — this field is what makes
    /// the drift visible (doctor compares it against the running
    /// [`crate::pipeline::engines`] table). `None` = pre-observability
    /// markers (never warned on) or a non-completing outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<BTreeMap<String, String>>,
}

#[derive(Debug)]
pub enum StoreError {
    /// bucket/index already exists -> ConflictException
    Conflict(String),
    /// request-shape problem detected below the wire layer (metadata type
    /// drift, stale/unknown QueryVectors page version, invalid
    /// bucket_suffix shapes) -> ValidationException
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

/// Backend-agnostic storage over the bucket-pair backend: one system-store
/// handle + one source-store handle per vector bucket, both flowing from one
/// [`Backend`].
pub struct Storage {
    backend: Backend,
    bucket_suffix: String,
    session: Arc<lancedb::Session>,
    /// The memory backend's ONE shared key space (bucket handles are
    /// PrefixStore views over it; R9 enumeration reads it directly).
    memory: Option<Arc<object_store::memory::InMemory>>,
    /// Raw bucket admin (s3 backend only; file/memory admin is local).
    admin: Option<crate::s3admin::S3Admin>,
    /// Per-bucket store-handle caches (source `X` / system `X-vf`
    /// separately — R3: touching the SYSTEM half must never materialize the
    /// SOURCE bucket, which is created lazily by the first PutSource).
    /// Constructing a file handle materializes that one bucket directory.
    source_stores: std::sync::Mutex<HashMap<String, Arc<dyn object_store::ObjectStore>>>,
    system_stores: std::sync::Mutex<HashMap<String, Arc<dyn object_store::ObjectStore>>>,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.backend)
    }
}

/// State of a MARKER-LESS system bucket for the Delete-tail replay
/// (design-v3.md §5.2 row 4 / §5.3-4 exemption).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailState {
    /// No system bucket at all.
    Missing,
    /// Bucket holds only deletion residue (policy.json / tags.json /
    /// sources/*) or nothing; `fresh_lock` = a live foreign LOCK object is
    /// present (its holder is mid-operation on the dying bucket — the
    /// lockless tail cleanup must wait for that lock to go stale).
    Residue { fresh_lock: bool },
    /// Objects beyond residue (indexes or foreign data): not a vb of ours,
    /// and not cleanup-able.
    Occupied,
}

impl Storage {
    /// Build from a parsed backend (file/memory/s3) + the configured
    /// `bucket_suffix` (validated here — a bad suffix refuses every
    /// backend, fail-fast).
    pub fn new(backend: Backend, bucket_suffix: &str) -> Result<Self, String> {
        backends::validate_bucket_suffix(bucket_suffix)?;
        let backend = backend.normalized()?;
        let handles = backend.build()?;
        let admin = match &backend {
            Backend::S3(cfg) => Some(crate::s3admin::S3Admin::new(cfg)),
            _ => None,
        };
        Ok(Storage {
            backend,
            bucket_suffix: bucket_suffix.to_string(),
            session: handles.session,
            memory: handles.memory,
            admin,
            source_stores: std::sync::Mutex::new(HashMap::new()),
            system_stores: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// File backend rooted at `dir` (creates + canonicalizes it), default
    /// `bucket_suffix`.
    pub fn file(root: impl Into<std::path::PathBuf>) -> Self {
        Storage::new(
            Backend::File { root: root.into() },
            backends::DEFAULT_BUCKET_SUFFIX,
        )
        .expect("file backend setup cannot fail (dir is created)")
    }

    /// Volatile in-process backend (CI / emulator mode), default suffix.
    pub fn memory() -> Self {
        Storage::new(Backend::Memory, backends::DEFAULT_BUCKET_SUFFIX)
            .expect("memory backend setup cannot fail")
    }

    /// The backend (for LOCK placement + logging).
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The lancedb session shared by every index connection.
    pub fn session(&self) -> &Arc<lancedb::Session> {
        &self.session
    }

    /// The memory backend's shared key space (None on file/s3) — the doctor
    /// and other direct probes open scratch buckets through it.
    pub fn memory_handle(&self) -> Option<&Arc<object_store::memory::InMemory>> {
        self.memory.as_ref()
    }

    /// Raw-delete a PHYSICAL bucket by name (doctor probe cleanup; file:
    /// remove_dir_all, memory: no-op, s3: DeleteBucket best effort).
    pub async fn delete_bucket_raw(&self, bucket: &str) {
        match &self.backend {
            Backend::File { root } => {
                let _ = std::fs::remove_dir_all(root.join(bucket));
            }
            Backend::Memory => {}
            Backend::S3(_) => {
                if let Some(admin) = &self.admin {
                    let _ = admin.delete_bucket(bucket).await;
                }
            }
        }
    }

    /// Configured `bucket_suffix` (R6: an install-time constant).
    pub fn bucket_suffix(&self) -> &str {
        &self.bucket_suffix
    }

    /// Physical system bucket name of vb `X`: `X + bucket_suffix` (R1).
    pub fn sys_bucket(&self, vb: &str) -> String {
        backends::system_bucket_name(vb, &self.bucket_suffix)
    }

    /// `x-vf-placement` echo (R4) of one vb's system bucket.
    pub fn placement(&self, vb: &str) -> String {
        let sys = self.sys_bucket(vb);
        match &self.backend {
            Backend::File { root } => format!("file://{}/{sys}", root.display()),
            Backend::Memory => format!("memory://{sys}"),
            Backend::S3(cfg) => format!("s3://{sys}@{}", cfg.endpoint),
        }
    }

    /// This daemon's deployment identity (R2/D8) — what ownership markers
    /// compare against.
    pub fn deployment_identity(&self, region: &str, account: &str) -> DeploymentIdentity {
        DeploymentIdentity {
            region: region.to_string(),
            account: account.to_string(),
            endpoint: self.backend.endpoint_id(),
        }
    }

    /// The source-bucket object store of vb `X` (pure user namespace).
    /// Constructing the handle lazily CREATES the bucket on the file backend
    /// (R3: the source bucket may be created at any time — adoption rules
    /// make that safe); s3 requires [`Storage::ensure_source_bucket`] first.
    pub fn source_store(&self, vb: &str) -> Arc<dyn object_store::ObjectStore> {
        let mut cache = self.source_stores.lock().unwrap();
        if let Some(s) = cache.get(vb) {
            return s.clone();
        }
        // user_object_key (R1): bucket-relative — the store IS the bucket
        let store = backends::open_bucket_store(&self.backend, vb, self.memory.as_ref())
            .expect("bucket store construction cannot fail (dirs are created)");
        cache.insert(vb.to_string(), store.clone());
        store
    }

    /// The system-bucket object store of vb `X` (bucket-root-relative
    /// system keys: `LOCK`, `bucket.json`, `sources/…`, `indexes/…`).
    /// Same lazy-creation note as [`Storage::source_store`] (it materializes
    /// `X-vf`, never `X`); read paths that must not materialize anything
    /// check [`Storage::system_bucket_exists`] first.
    pub fn system_store(&self, vb: &str) -> Arc<dyn object_store::ObjectStore> {
        let mut cache = self.system_stores.lock().unwrap();
        if let Some(s) = cache.get(vb) {
            return s.clone();
        }
        let sys = self.sys_bucket(vb);
        let store = backends::open_bucket_store(&self.backend, &sys, self.memory.as_ref())
            .expect("bucket store construction cannot fail (dirs are created)");
        cache.insert(vb.to_string(), store.clone());
        store
    }

    /// Drop the cached handles after the system bucket was RAW-DELETED
    /// (delete_bucket): a stale LocalFileSystem prefix would resurrect an
    /// empty directory on the next use.
    fn evict_stores(&self, vb: &str) {
        self.system_stores.lock().unwrap().remove(vb);
        self.source_stores.lock().unwrap().remove(vb);
    }

    // ---- raw bucket admin (R3/R9; s3 = s3admin, file/memory = local) ----

    /// Does the SYSTEM bucket physically exist? (file: directory present;
    /// memory: any key under the prefix; s3: HeadBucket.)
    pub async fn system_bucket_exists(&self, vb: &str) -> Result<bool, StoreError> {
        self.raw_bucket_exists(&self.sys_bucket(vb)).await
    }

    async fn raw_bucket_exists(&self, bucket: &str) -> Result<bool, StoreError> {
        match &self.backend {
            Backend::File { root } => Ok(root.join(bucket).is_dir()),
            Backend::Memory => {
                let Some(mem) = &self.memory else {
                    return Ok(false);
                };
                let prefix = object_store::path::Path::from(bucket.to_string());
                Ok(!backends::list_prefix(mem.as_ref(), &prefix)
                    .await
                    .map_err(internal)?
                    .is_empty())
            }
            Backend::S3(_) => {
                let admin = self.admin.as_ref().expect("s3 backend carries an admin");
                admin
                    .head_bucket(bucket)
                    .await
                    .map_err(|e| internal(e.to_string()))
            }
        }
    }

    /// Ensure the SOURCE bucket `X` exists (R3 lazy creation, D4): no-op
    /// when present; created when missing (s3: raw CreateBucket — losing a
    /// create race to someone else's identical bucket is benign adoption).
    pub async fn ensure_source_bucket(&self, vb: &str) -> Result<(), StoreError> {
        match &self.backend {
            Backend::File { .. } | Backend::Memory => {
                let _ = self.source_store(vb); // materializes file dirs
                Ok(())
            }
            Backend::S3(_) => {
                let admin = self.admin.as_ref().expect("s3 backend carries an admin");
                if !admin
                    .head_bucket(vb)
                    .await
                    .map_err(|e| internal(e.to_string()))?
                {
                    // Both Created and AlreadyExists are fine: existing =
                    // adoption (良性竞态, §5.2).
                    admin
                        .create_bucket(vb)
                        .await
                        .map_err(|e| map_admin_create_err(e, vb))?;
                }
                Ok(())
            }
        }
    }

    /// Every candidate vb name from the R9 suffix scan, WITHOUT the marker
    /// check (callers filter by ownership; enumeration never invents a
    /// second source of truth).
    pub async fn suffix_scan_candidates(&self) -> Result<Vec<String>, StoreError> {
        let suffix = self.bucket_suffix.as_str();
        let mut candidates: Vec<String> = match &self.backend {
            Backend::File { root } => std::fs::read_dir(root)
                .map_err(|e| internal(format!("read {}: {e}", root.display())))?
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(suffix))
                .map(|n| n.trim_end_matches(suffix).to_string())
                .collect(),
            Backend::Memory => {
                let Some(mem) = &self.memory else {
                    return Ok(Vec::new());
                };
                let mut firsts = std::collections::BTreeSet::new();
                for key in backends::list_all(mem.as_ref()).await.map_err(internal)? {
                    if let Some(seg) = key.as_ref().split('/').next() {
                        firsts.insert(seg.to_string());
                    }
                }
                firsts
                    .into_iter()
                    .filter(|n| n.ends_with(suffix))
                    .map(|n| n.trim_end_matches(suffix).to_string())
                    .collect()
            }
            Backend::S3(_) => {
                let admin = self.admin.as_ref().expect("s3 backend carries an admin");
                admin
                    .list_bucket_names()
                    .await
                    .map_err(|e| internal(e.to_string()))?
                    .into_iter()
                    .filter(|n| n.ends_with(suffix))
                    .map(|n| n.trim_end_matches(suffix).to_string())
                    .collect()
            }
        };
        candidates.sort();
        candidates.dedup();
        Ok(candidates)
    }

    // ---- catalog (object_store everywhere; §5.0 “目录: catalog JSON 对象(条件写)”) ----

    /// Read `X-vf/bucket.json`; Ok(None) = no marker (no such vb, or crash
    /// residue). Read-only: does NOT materialize a missing file-backend
    /// bucket directory.
    pub async fn load_bucket(&self, name: &str) -> Result<Option<BucketMeta>, StoreError> {
        if let Backend::File { root } = &self.backend
            && !root.join(self.sys_bucket(name)).is_dir()
        {
            return Ok(None); // read path must not materialize bucket dirs
        }
        backends::read_json(&*self.system_store(name), &Self::MARKER_KEY.into())
            .await
            .map_err(internal)
    }

    /// `bucket.json` — the marker object key (bucket-root-relative).
    const MARKER_KEY: &'static str = "bucket.json";

    /// The LOCK object key (bucket-root-relative; lock.rs).
    const LOCK_KEY: &'static str = "LOCK";

    /// G2 crash-injection seam (design-v3.md §5.2 rows 3-4, §10-G2): when
    /// `VF_TEST_CRASH` names a lifecycle cut point, the process aborts RIGHT
    /// THERE — a real hard crash (no graceful-shutdown handlers, no LOCK
    /// release) that leaves exactly the §5.2 on-storage state for the replay
    /// matrix (tests/lifecycle_crash_replay.rs). The two named points:
    /// `create_after_raw_create` (raw CreateBucket done, marker not written)
    /// and `delete_after_marker` (marker + LOCK deleted, raw DeleteBucket not
    /// sent). Test-only: unset in production this is one env lookup per
    /// lifecycle op (create/delete are rare admin verbs, not per-request).
    fn crash_point(name: &str) {
        if std::env::var("VF_TEST_CRASH").ok().as_deref() == Some(name) {
            eprintln!("vectorfs: VF_TEST_CRASH={name} — aborting at the cut point");
            std::process::abort();
        }
    }

    /// CreateVectorBucket lifecycle (R3): raw-create `X-vf`, adopt crash
    /// residue, write the ownership marker. NEVER touches the source bucket
    /// `X` (lazy, D4).
    ///
    /// * marker present ⇒ Conflict (replay = ConflictException, A-F3; hint:
    ///   存量桶可直接使用或先 Delete);
    /// * non-marker objects present (a foreign `-vf` bucket in use) ⇒
    ///   Conflict with the bucket_suffix hint (§5.2 row 1);
    /// * empty + no marker + no LOCK ⇒ ADOPT (D7 — Create-crash / Delete-
    ///   tail-crash residue), which is exactly the plain create path;
    /// * raw CreateBucket losing a race (s3) ⇒ same adoption logic on the
    ///   next marker write (conditional create decides).
    pub async fn create_bucket(&self, meta: &BucketMeta) -> Result<(), StoreError> {
        let vb = meta.vector_bucket_name.as_str();
        let sys = self.sys_bucket(vb);

        // ---- raw create FIRST on s3 (R8: converge on "create then probe",
        // never on the error-code split — and real endpoints ERROR on
        // listing/getting from a bucket that does not exist yet, so the
        // probes below must not run first; file/memory: the store
        // construction below materializes the bucket) ----
        if let Backend::S3(_) = &self.backend {
            let admin = self.admin.as_ref().expect("s3 backend carries an admin");
            admin
                .create_bucket(&sys)
                .await
                .map_err(|e| map_admin_create_err(e, vb))?;
        }

        let store = self.system_store(vb); // materializes the bucket (file)

        // §5.2 row 3 cut point: raw CreateBucket succeeded, marker not yet
        // written (G2 replay matrix; see crash_point doc).
        Self::crash_point("create_after_raw_create");

        // ---- state probe: marker / foreign objects ----
        if backends::read_json::<BucketMeta>(&*store, &Self::MARKER_KEY.into())
            .await
            .map_err(internal)?
            .is_some()
        {
            return Err(StoreError::Conflict(format!(
                "vector bucket {vb} already exists (its system bucket {sys} carries an \
                 ownership marker) — use it directly, or DeleteVectorBucket first if you mean \
                 to replace it"
            )));
        }
        // "Empty" for ADOPTION purposes (D7) = no FOREIGN DATA: our own
        // mechanism keys may legitimately precede the marker — the lazy
        // per-bucket LOCK (§5.1 startup pin) and scan-time source markers
        // both materialize `X-vf` before the first ensure-domain creates
        // the vb. Anything beyond the mechanism set (indexes/, unknown
        // objects) is a bucket in actual use -> Conflict (§5.2 row 1).
        let foreign = backends::list_all(&*store)
            .await
            .map_err(internal)?
            .into_iter()
            .any(|k| {
                let k = k.as_ref();
                let mechanism = k == Self::LOCK_KEY
                    || k == "policy.json"
                    || k == "tags.json"
                    || k.starts_with("sources/");
                !mechanism && k != Self::MARKER_KEY
            });
        if foreign {
            return Err(StoreError::Conflict(format!(
                "a foreign bucket named {sys} already exists and is not empty — pick another \
                 vector bucket name, or change bucket_suffix (currently {:?}) so vectorfs \
                 system buckets stop colliding with it",
                self.bucket_suffix
            )));
        }

        // ---- marker (conditional: a racing creator that wrote its own
        // marker first makes ours lose — that is the conflict case again) ----
        let json = serde_json::to_vec_pretty(meta).map_err(internal)?;
        if !backends::put_json_create(&*store, &Self::MARKER_KEY.into(), &json)
            .await
            .map_err(internal)?
        {
            return Err(StoreError::Conflict(format!(
                "vector bucket {vb} already exists (marker written concurrently)"
            )));
        }
        Ok(())
    }

    /// DeleteVectorBucket lifecycle (R3), prescribed order: 清空 marker/LOCK
    /// 之外全部对象 → 删 marker → 删 LOCK → raw DeleteBucket. BucketNotEmpty
    /// from a racing writer re-enters the loop (bounded). The SOURCE bucket
    /// is never touched (D4). Caller has verified no indexes + holds the
    /// bucket lock.
    pub async fn delete_bucket(&self, name: &str) -> Result<(), StoreError> {
        let sys = self.sys_bucket(name);
        for round in 0..3 {
            let store = self.system_store(name);
            let keys = backends::list_all(&*store).await.map_err(internal)?;
            for key in keys {
                // deletion order inside one round: residue first, then the
                // marker, then the LOCK (LOCK last-but-one keeps the
                // takeover guard alive as long as possible)
                if key.as_ref() == Self::MARKER_KEY || key.as_ref() == Self::LOCK_KEY {
                    continue;
                }
                backends::delete_object(&*store, &key)
                    .await
                    .map_err(internal)?;
            }
            backends::delete_object(&*store, &Self::MARKER_KEY.into())
                .await
                .map_err(internal)?;
            backends::delete_object(&*store, &Self::LOCK_KEY.into())
                .await
                .map_err(internal)?;

            // §5.2 row 4 cut point: marker + LOCK deleted, raw DeleteBucket
            // not yet sent (G2 replay matrix; see crash_point doc).
            Self::crash_point("delete_after_marker");

            // ---- raw DeleteBucket ----
            match &self.backend {
                Backend::File { root } => {
                    let dir = root.join(&sys);
                    if dir.is_dir() {
                        // best-effort: a racing writer may hold a file open;
                        // the next Delete replay re-enters the loop
                        if let Err(e) = std::fs::remove_dir_all(&dir) {
                            if round == 2 {
                                return Err(internal(format!("remove {sys}: {e}")));
                            }
                            continue;
                        }
                    }
                    self.evict_stores(name);
                    return Ok(());
                }
                Backend::Memory => {
                    self.evict_stores(name);
                    return Ok(()); // keys deleted above; no bucket object to remove
                }
                Backend::S3(_) => {
                    let admin = self.admin.as_ref().expect("s3 backend carries an admin");
                    match admin.delete_bucket(&sys).await {
                        Ok(()) => {
                            self.evict_stores(name);
                            return Ok(());
                        }
                        // BucketNotEmpty: a racing writer put an object back —
                        // re-read, re-evaluate (§5.2 row 5), bounded rounds
                        Err(e) if e.status == Some(409) && round < 2 => continue,
                        Err(e) => return Err(internal(e.to_string())),
                    }
                }
            }
        }
        Err(StoreError::Internal(format!(
            "delete of system bucket {sys} did not converge"
        )))
    }

    /// Classify a marker-less system bucket (Delete-tail replay probe).
    pub async fn system_bucket_tail(&self, vb: &str) -> Result<TailState, StoreError> {
        if !self.system_bucket_exists(vb).await? {
            return Ok(TailState::Missing);
        }
        let store = self.system_store(vb);
        let keys = backends::list_all(&*store).await.map_err(internal)?;
        let is_residue =
            |k: &str| k == "policy.json" || k == "tags.json" || k.starts_with("sources/");
        if keys
            .iter()
            .any(|k| !is_residue(k.as_ref()) && k.as_ref() != "LOCK")
        {
            return Ok(TailState::Occupied);
        }
        let fresh_lock = match backends::read_json::<crate::lock::LockInfo>(&*store, &"LOCK".into())
            .await
            .map_err(internal)?
        {
            Some(info) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                info.heartbeat_age(now) < crate::lock::LOCK_TTL
            }
            None => false,
        };
        Ok(TailState::Residue { fresh_lock })
    }

    /// Lockless Delete-tail cleanup (§5.3-4 exemption): remove the residue
    /// objects + stale LOCK, then raw-delete the bucket. Caller verified
    /// [`TailState::Residue`] with no fresh lock.
    pub async fn delete_bucket_tail(&self, vb: &str) -> Result<(), StoreError> {
        self.delete_bucket(vb).await
    }

    pub async fn load_index(&self, vb: &str, ix: &str) -> Result<Option<IndexMeta>, StoreError> {
        backends::read_json(
            &*self.system_store(vb),
            &format!("indexes/{ix}/index.json").into(),
        )
        .await
        .map_err(internal)
    }

    pub async fn create_index(&self, meta: &IndexMeta) -> Result<(), StoreError> {
        let key = object_store::path::Path::from(format!("indexes/{}/index.json", meta.index_name));
        let json = serde_json::to_vec_pretty(meta).map_err(internal)?;
        if !backends::put_json_create(&*self.system_store(&meta.vector_bucket_name), &key, &json)
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
                let _ =
                    backends::delete_object(&*self.system_store(&meta.vector_bucket_name), &key)
                        .await;
                return Err(e);
            }
        };
        if let Err(e) = conn
            .create_empty_table(TABLE_NAME, arrow_schema(meta.dimension))
            .execute()
            .await
        {
            let _ =
                backends::delete_object(&*self.system_store(&meta.vector_bucket_name), &key).await; // best-effort rollback
            return Err(internal(e));
        }
        Ok(())
    }

    // ---- Source markers (M2 T3; catalog objects like the metas above) ----

    fn source_key(&self, key_hash: &str) -> object_store::path::Path {
        object_store::path::Path::from(format!("sources/{key_hash}.json"))
    }

    /// Load one Source marker; Ok(None) = no marker (the no-queue "pending"
    /// signal, §5.10.5).
    pub async fn load_source(
        &self,
        vb: &str,
        key_hash: &str,
    ) -> Result<Option<SourceMarker>, StoreError> {
        backends::read_json(&*self.system_store(vb), &self.source_key(key_hash))
            .await
            .map_err(internal)
    }

    /// Unconditional marker overwrite (the single-writer LOCK serializes
    /// writers; markers are never concurrently written by design).
    pub async fn save_source(
        &self,
        vb: &str,
        key_hash: &str,
        marker: &SourceMarker,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_vec_pretty(marker).map_err(internal)?;
        backends::put_json_overwrite(&*self.system_store(vb), &self.source_key(key_hash), &json)
            .await
            .map_err(internal)
    }

    /// Every Source marker of one bucket. Suffix-parsed like list_buckets.
    pub async fn list_sources(&self, vb: &str) -> Result<Vec<SourceMarker>, StoreError> {
        let mut out = Vec::new();
        let prefix = object_store::path::Path::from("sources");
        for key in backends::list_prefix(&*self.system_store(vb), &prefix)
            .await
            .map_err(internal)?
        {
            // …/sources/<keyHash>.json
            let segs = key.as_ref().split('/').collect::<Vec<_>>();
            if segs.len() >= 2
                && segs[segs.len() - 2] == "sources"
                && segs.last().is_some_and(|s| s.ends_with(".json"))
                && let Some(m) = backends::read_json::<SourceMarker>(&*self.system_store(vb), &key)
                    .await
                    .map_err(internal)?
            {
                out.push(m);
            }
        }
        out.sort_by(|a, b| a.object_key.cmp(&b.object_key));
        Ok(out)
    }

    /// Remove one marker (idempotent; the delete path rolls T7's GC).
    pub async fn delete_source(&self, vb: &str, key_hash: &str) -> Result<(), StoreError> {
        backends::delete_object(&*self.system_store(vb), &self.source_key(key_hash))
            .await
            .map_err(internal)
    }

    // ---- catalog: management-plane enumeration & removal (M1 T3, R9) ----

    /// All bucket metas of THIS deployment, sorted by name (ListVectorBuckets
    /// ordering). R9 suffix scan + ownership marker check: a `-vf` bucket
    /// without our marker is somebody else's (or debris) and is skipped.
    pub async fn list_buckets(
        &self,
        identity: &DeploymentIdentity,
    ) -> Result<Vec<BucketMeta>, StoreError> {
        let mut out = Vec::new();
        for vb in self.suffix_scan_candidates().await? {
            if let Some(meta) = self.load_bucket(&vb).await?
                && &meta.deployment == identity
            {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| a.vector_bucket_name.cmp(&b.vector_bucket_name));
        Ok(out)
    }

    /// All index metas of one bucket, sorted by name (ListIndexes ordering).
    pub async fn list_indexes(&self, vb: &str) -> Result<Vec<IndexMeta>, StoreError> {
        let mut out = Vec::new();
        let prefix = object_store::path::Path::from("indexes");
        for key in backends::list_prefix(&*self.system_store(vb), &prefix)
            .await
            .map_err(internal)?
        {
            // …/indexes/<ix>/index.json
            let segs = key.as_ref().split('/').collect::<Vec<_>>();
            if segs.len() == 3
                && segs.last() == Some(&"index.json")
                && let Some(meta) = backends::read_json::<IndexMeta>(&*self.system_store(vb), &key)
                    .await
                    .map_err(internal)?
            {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| a.index_name.cmp(&b.index_name));
        Ok(out)
    }

    /// True if ANY object lives under `indexes/` — the DeleteVectorBucket
    /// non-empty guard. (The bucket lock serializes list-then-delete,
    /// design-v3.md §5.1.)
    pub async fn bucket_has_indexes(&self, vb: &str) -> Result<bool, StoreError> {
        Ok(!backends::list_prefix(
            &*self.system_store(vb),
            &object_store::path::Path::from("indexes"),
        )
        .await
        .map_err(internal)?
        .is_empty())
    }

    /// Remove an index: every object under `indexes/<ix>/` — index.json,
    /// tags.json AND the Lance dataset under `data/` — one sweep on the
    /// system bucket's store. (The v2.2 memory `vf/` mirror sweep is gone:
    /// v3 memory dataset keys ARE bucket-relative, §5.3-5.)
    pub async fn delete_index(&self, vb: &str, ix: &str) -> Result<(), StoreError> {
        let base = object_store::path::Path::from(format!("indexes/{ix}"));
        for key in backends::list_prefix(&*self.system_store(vb), &base)
            .await
            .map_err(internal)?
        {
            backends::delete_object(&*self.system_store(vb), &key)
                .await
                .map_err(internal)?;
        }
        Ok(())
    }

    // ---- policy (§4.4: store & echo, never enforce) ----

    pub async fn load_policy(&self, vb: &str) -> Result<Option<String>, StoreError> {
        match backends::get_with_etag(&*self.system_store(vb), &"policy.json".into())
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
            &*self.system_store(vb),
            &"policy.json".into(),
            raw.as_bytes(),
        )
        .await
        .map_err(internal)
    }

    pub async fn delete_policy(&self, vb: &str) -> Result<(), StoreError> {
        backends::delete_object(&*self.system_store(vb), &"policy.json".into())
            .await
            .map_err(internal)
    }

    // ---- tags (bucket ARN and index ARN both address tags.json, §5.3) ----

    fn tags_key(&self, ix: Option<&str>) -> object_store::path::Path {
        object_store::path::Path::from(match ix {
            None => "tags.json".to_string(),
            Some(ix) => format!("indexes/{ix}/tags.json"),
        })
    }

    pub async fn load_tags(
        &self,
        vb: &str,
        ix: Option<&str>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, StoreError> {
        Ok(
            backends::read_json(&*self.system_store(vb), &self.tags_key(ix))
                .await
                .map_err(internal)?
                .unwrap_or_else(serde_json::Map::new),
        )
    }

    pub async fn put_tags(
        &self,
        vb: &str,
        ix: Option<&str>,
        tags: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), StoreError> {
        let json =
            serde_json::to_vec(&serde_json::Value::Object(tags.clone())).map_err(internal)?;
        backends::put_json_overwrite(&*self.system_store(vb), &self.tags_key(ix), &json)
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
        backends::put_json_overwrite(&*self.system_store(name), &Self::MARKER_KEY.into(), &json)
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
            &*self.system_store(vb),
            &object_store::path::Path::from(format!("indexes/{ix}/index.json")),
            &json,
        )
        .await
        .map_err(internal)
    }

    // ---- engine ----

    /// Open a lancedb connection to an index's data dir through the shared
    /// session (registry-routed store for file/memory/s3 alike).
    async fn connect_index(&self, vb: &str, ix: &str) -> Result<Connection, StoreError> {
        let uri = self.backend.lance_uri(&self.sys_bucket(vb), ix);
        let mut builder = lancedb::connect(&uri).session(self.session.clone());
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

    /// Lean full-table enumeration for the file layer's reconciliation (M2
    /// T7, design.md §5.10.5 "对账走 ListVectors+segments（非 Query）"):
    /// one column-projected scan returning `(key, metadata)` for EVERY row —
    /// no vectors, no KNN. This is the engine-side equivalent of paging
    /// ListVectors for keys and then batching GetVectors for metadata, minus
    /// the vector-column traffic both of those drag along (40 KB chunk-text
    /// rows make that non-trivial); the GC and the reindex source-clear both
    /// group these rows by `x-vf-src-uri`. Key order is the scan's native
    /// order (callers only aggregate, never paginate).
    pub async fn scan_vector_metadata(
        &self,
        vb: &str,
        ix: &str,
    ) -> Result<Vec<(String, Option<serde_json::Value>)>, StoreError> {
        let table = self.open_table(vb, ix).await?;
        let stream = table
            .query()
            .select(lancedb::query::Select::columns(&["key", "metadata_json"]))
            .execute()
            .await
            .map_err(internal)?;
        let mut out = Vec::new();
        let mut stream = stream;
        use futures_util::StreamExt;
        while let Some(batch) = stream.next().await {
            let batch = batch.map_err(internal)?;
            let Some(keys) = batch.column_by_name("key") else {
                return Err(StoreError::Internal("missing key column".into()));
            };
            let keys = keys
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| StoreError::Internal("key column not utf8".into()))?;
            let metas = batch
                .column_by_name("metadata_json")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            for i in 0..batch.num_rows() {
                out.push((keys.value(i).to_string(), metadata_at(metas, i)));
            }
        }
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

    /// Explicitly build (or fold new rows into) the IVF_PQ vector index of
    /// one index — the engine side of `vectorfs optimize` (M3-T8). NEVER
    /// called by the daemon on its own: IVF_PQ is an index PHYSICAL FORM,
    /// not pipeline semantics — no config key, no fingerprint surface, no
    /// background task. Until an operator runs the verb, queries stay on the
    /// flat exact path (byte-identical to pre-T8; every existing test lives
    /// here).
    ///
    /// Behavior:
    /// * rows < [`IVF_PQ_MIN_TRAIN_ROWS`] → [`VectorIndexOutcome::BelowGate`]
    ///   — LOUD skip, never a silent flat fallback masquerading as an index
    ///   (the caller turns this into a visible SKIPPED line / exit code);
    /// * no vector index yet → train+build IVF_PQ over `vector` with the
    ///   ENGINE defaults (num_partitions auto-sized by row count,
    ///   num_sub_vectors = dim/16 heuristic, 8-bit codes — deliberately zero
    ///   knobs; expose any only with user evidence). The index's distance
    ///   type is taken from the index's own metric so query and index agree;
    /// * vector index present → fold the rows written since the last pass
    ///   into the existing index (lancedb `optimize(Index)`), no retrain.
    ///   Queries in the interim were still CORRECT — lancedb scans indexed
    ///   and unindexed data in parallel — just slower on the delta. A full
    ///   retrain after distribution drift is an evidence-gated knob for now.
    ///
    /// Approximation note: once an index exists, KNN over that table is
    /// ANN (recall < 1.0 by construction of IVF+PQ). That is the trade the
    /// operator opted into by running the verb at 1M-scale.
    pub async fn optimize_vector_index(
        &self,
        vb: &str,
        ix: &str,
        metric: &str,
    ) -> Result<VectorIndexOutcome, StoreError> {
        // a missing table (index created, nothing ever ingested) is the
        // zero-row case, not an error — same BelowGate verdict
        let table = match self.open_table(vb, ix).await {
            Ok(t) => t,
            Err(e) => {
                return if e.to_string().contains("Table not found")
                    || e.to_string().contains("was not found")
                {
                    Ok(VectorIndexOutcome::BelowGate { rows: 0 })
                } else {
                    Err(e)
                };
            }
        };
        let rows = table.count_rows(None).await.map_err(internal)?;
        if (rows as usize) < IVF_PQ_MIN_TRAIN_ROWS {
            return Ok(VectorIndexOutcome::BelowGate { rows });
        }
        // vector column already indexed? (only WE build on "vector"; scalar
        // mirror columns, if any ever exist, carry metadata names)
        let indexed = table
            .list_indices()
            .await
            .map_err(internal)?
            .iter()
            .any(|c| c.columns.iter().any(|col| col == "vector"));
        if indexed {
            table
                .optimize(lancedb::table::OptimizeAction::Index(
                    lancedb::table::OptimizeOptions::default(),
                ))
                .await
                .map_err(internal)?;
            Ok(VectorIndexOutcome::Folded { rows })
        } else {
            table
                .create_index(
                    &["vector"],
                    lancedb::index::Index::IvfPq(
                        lancedb::index::vector::IvfPqIndexBuilder::default()
                            .distance_type(distance_type(metric)),
                    ),
                )
                .execute()
                .await
                .map_err(internal)?;
            Ok(VectorIndexOutcome::Trained { rows })
        }
    }
}

/// s3admin create errors → StoreError (§5.2): InvalidBucketName/400 becomes
/// a ValidationException pointing at `bucket_suffix`; everything else is
/// internal.
fn map_admin_create_err(e: crate::s3admin::AdminError, vb: &str) -> StoreError {
    match e.status {
        Some(400) => StoreError::Validation(format!(
            "creating the system bucket for {vb} was rejected by the endpoint ({}): the name \
             derived from bucket_suffix may be illegal there — adjust bucket_suffix or the \
             bucket name",
            e.message
        )),
        _ => internal(e.to_string()),
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

/// Outcome of one explicit IVF_PQ pass ([`Storage::optimize_vector_index`],
/// the engine side of `vectorfs optimize`, M3-T8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorIndexOutcome {
    /// Trained + built IVF_PQ over `rows` rows (no vector index before).
    Trained { rows: usize },
    /// An existing vector index absorbed the delta written since the last
    /// pass (no retrain — see `optimize_vector_index` docs).
    Folded { rows: usize },
    /// `rows` < [`IVF_PQ_MIN_TRAIN_ROWS`]: refused, loudly — IVF centroids
    /// and PQ codebooks need enough rows to train on, and a silent flat
    /// fallback would let an operator believe they have an index.
    BelowGate { rows: usize },
}

/// Minimum rows before an IVF_PQ index can be trained (M3-T8 gate): the PQ
/// stage quantizes each sub-vector to one of 256 (2^8) centroids — k-means
/// with more centroids than training rows does not converge to anything
/// meaningful. Below this the index is refused (queries stay flat exact,
/// which at these sizes is fast anyway — §6's flat cost line only bites
/// around 1M rows).
pub const IVF_PQ_MIN_TRAIN_ROWS: usize = 256;

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

/// Test fixture meta with the v3 marker fields (default suffix `-vf`,
/// deployment identity of a stock daemon) — shared by every crate test
/// module that seeds a bucket.
#[cfg(test)]
pub(crate) fn test_bucket_meta(storage: &Storage, vb: &str) -> BucketMeta {
    BucketMeta {
        vector_bucket_name: vb.to_string(),
        vector_bucket_arn: format!("arn:aws:s3vectors:us-east-1:0:bucket/{vb}"),
        creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
        creation_time_epoch: 0.0,
        deployment: storage.deployment_identity("us-east-1", "000000000000"),
        placement: storage.placement(vb),
        default_index_mode: None,
    }
}

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
        setup_metric(name, batches, dim, "euclidean").await
    }

    /// `setup` with an explicit distance metric — the IVF_PQ tests use
    /// `cosine`, the metric every derived index carries.
    async fn setup_metric(
        name: &str,
        batches: Vec<Vec<VectorRow>>,
        dim: i32,
        metric: &str,
    ) -> Storage {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::file(dir.path());
        // leak the tempdir: keep the dataset alive for the test body (tests
        // are short-lived; the OS reclaims on exit)
        std::mem::forget(dir);
        storage
            .create_bucket(&test_bucket_meta(&storage, "vb"))
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
                distance_metric: metric.into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
                x_vf_derived: None,
            })
            .await
            .unwrap();
        for b in batches {
            storage.upsert_vectors("vb", name, dim, b).await.unwrap();
        }
        storage
    }

    /// A row whose vector is GIVEN (not LCG-derived) — the IVF_PQ tests'
    /// needle rows.
    fn row_with(key: &str, vector: Vec<f32>, meta: serde_json::Value) -> VectorRow {
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
            key: key.to_string(),
            vector,
            metadata_json: if meta.is_null() {
                None
            } else {
                Some(meta.to_string())
            },
            filterable,
        }
    }

    // ---- IVF_PQ (M3-T8): explicit index physical form ----

    /// The training gate: below [`IVF_PQ_MIN_TRAIN_ROWS`] the pass refuses
    /// (BelowGate) and builds NOTHING — no silent flat fallback pretending
    /// to be an index, and no half-trained centroid file on disk.
    #[tokio::test]
    async fn ivf_pq_training_gate_refuses_small_datasets() {
        let dim = 16;
        let ds: Vec<Vec<VectorRow>> = vec![consistency_dataset(100, dim)];
        let storage = setup_metric("ixsmall", ds, dim, "cosine").await;
        assert_eq!(
            storage
                .optimize_vector_index("vb", "ixsmall", "cosine")
                .await
                .unwrap(),
            VectorIndexOutcome::BelowGate { rows: 100 }
        );
        // nothing was built
        let table = storage.open_table("vb", "ixsmall").await.unwrap();
        assert!(
            table.list_indices().await.unwrap().is_empty(),
            "below-gate datasets must not carry an index"
        );
        // queries still work (flat exact — the pre-T8 path unchanged)
        let q: Vec<f32> = (0..dim).map(|d| 0.1 + d as f32 * 0.05).collect();
        let out = storage
            .query_vectors(
                "vb",
                "ixsmall",
                dim,
                "cosine",
                &q,
                5,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        assert_eq!(out.hits.len(), 5);
    }

    /// The full coexistence contract: build → query over the index → WRITE
    /// after the build (delta rows must still surface — lancedb scans
    /// indexed + unindexed data in parallel) → DELETE (index + deletion
    /// vectors agree) → the GC join path (scan_vector_metadata) is untouched
    /// → re-run folds instead of retraining.
    #[tokio::test]
    async fn ivf_pq_build_query_delta_delete_coexistence() {
        let dim = 16;
        let ds = consistency_dataset(300, dim);
        let storage = setup_metric("ixivf", vec![ds], dim, "cosine").await;

        // build: Trained, one vector index on "vector"
        assert_eq!(
            storage
                .optimize_vector_index("vb", "ixivf", "cosine")
                .await
                .unwrap(),
            VectorIndexOutcome::Trained { rows: 300 }
        );
        let table = storage.open_table("vb", "ixivf").await.unwrap();
        let indices = table.list_indices().await.unwrap();
        assert!(
            indices
                .iter()
                .any(|c| c.columns.iter().any(|col| col == "vector")),
            "a vector index exists: {indices:?}"
        );

        // queries run over the indexed table: order + truncation still hold
        let q: Vec<f32> = (0..dim).map(|d| 0.1 + d as f32 * 0.05).collect();
        let out = storage
            .query_vectors(
                "vb",
                "ixivf",
                dim,
                "cosine",
                &q,
                10,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        assert_eq!(out.hits.len(), 10);
        for w in out.hits.windows(2) {
            assert!(w[0].distance <= w[1].distance, "ordered: {:?}", out.hits);
        }
        // filtered query through the indexed table (pushdown + post-filter)
        let filter = Filter::compile(&json!({"ok": true})).unwrap();
        let out = storage
            .query_vectors(
                "vb",
                "ixivf",
                dim,
                "cosine",
                &q,
                10,
                Some(&filter),
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        assert!(
            !out.hits.is_empty()
                && out
                    .hits
                    .iter()
                    .all(|h| h.metadata.as_ref().unwrap()["ok"] == json!(true))
        );

        // WRITE after the build: the needle (vector == query) is a DELTA row
        // — it must still surface at rank 1 with distance ~0. This is the
        // correctness pin for "ingest continues after optimize".
        let needle = row_with("needle", q.clone(), json!({"i": 999999, "tag": "needle"}));
        storage
            .upsert_vectors("vb", "ixivf", dim, vec![needle])
            .await
            .unwrap();
        let out = storage
            .query_vectors(
                "vb",
                "ixivf",
                dim,
                "cosine",
                &q,
                10,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            out.hits[0].key,
            "needle",
            "delta row is searchable: {:?}",
            &out.hits[..3]
        );
        assert!(
            out.hits[0].distance < 1e-6,
            "identical vector: {}",
            out.hits[0].distance
        );

        // DELETE the needle + five more: gone from queries AND from the GC
        // join path (scan_vector_metadata), with the index present
        let doomed: Vec<String> = (0..5).map(|i| format!("k{:04}", i)).collect();
        let mut gone = vec!["needle".to_string()];
        gone.extend(doomed.clone());
        storage.delete_vectors("vb", "ixivf", &gone).await.unwrap();
        let out = storage
            .query_vectors(
                "vb",
                "ixivf",
                dim,
                "cosine",
                &q,
                300,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        let keys: Vec<&str> = out.hits.iter().map(|h| h.key.as_str()).collect();
        assert!(!keys.contains(&"needle"), "deleted delta row is gone");
        for k in &doomed {
            assert!(!keys.contains(&k.as_str()), "deleted {k} is gone");
        }
        assert_eq!(out.hits.len(), 295);
        let scanned = storage.scan_vector_metadata("vb", "ixivf").await.unwrap();
        assert_eq!(scanned.len(), 295, "the GC join sees exactly the live rows");
        assert!(
            scanned.iter().all(|(k, _)| k != "needle"),
            "no deleted key in the scan"
        );

        // re-run: FOLD into the existing index (no retrain), idempotent verb
        assert_eq!(
            storage
                .optimize_vector_index("vb", "ixivf", "cosine")
                .await
                .unwrap(),
            VectorIndexOutcome::Folded { rows: 295 }
        );
        // and the folded index still answers correctly
        let out = storage
            .query_vectors(
                "vb",
                "ixivf",
                dim,
                "cosine",
                &q,
                10,
                None,
                QueryRoute::Pushdown,
                None,
            )
            .await
            .unwrap();
        assert_eq!(out.hits.len(), 10);
        assert!(out.hits.iter().all(|h| h.key != "needle"));
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

    /// 1M×768 IVF_PQ vs flat reference (M3-T8 delivery numbers; run manually
    /// — RELEASE profile, the M1 perf precedent is debug-only for its own
    /// numbers, this one is too big for that):
    ///
    ///   cargo test --release -p vectorfs-server --lib perf_ivf_pq_vs_flat_1m -- --ignored --nocapture
    ///
    /// 口径: engine-direct `Storage::query_vectors` (no HTTP/SigV4 stack),
    /// file backend on a temp dir, 1,000,000 random unit vectors dim=768
    /// cosine (the derived-index metric), ingested in 500 batches of 2000,
    /// topK=10, N=30 timed queries per phase (first discarded as warm-up).
    /// Phase 1 (flat) runs BEFORE any index exists — the pre-T8 default
    /// path; phase 2 after `optimize_vector_index` trains IVF_PQ (engine
    /// defaults: num_partitions auto, num_sub_vectors=dim/16, 8-bit). The
    /// phase-1 top-10s are kept as the ground truth for phase-2 recall@10.
    /// Last run's numbers live in the module header ("IVF_PQ 对照" note) and
    /// the M3-T8 delivery report — including the recall caveat: on this
    /// UNIFORM-RANDOM dataset recall@10 is ~0.05 (distance concentration
    /// makes PQ ranking noise on structure-free vectors; nprobes does not
    /// fix it), while a same-scale CLUSTERED probe (embedding-like) scored
    /// 1.0 with the same engine defaults.
    #[tokio::test]
    #[ignore = "manual perf run: 1M×768 ingest + IVF_PQ train (~minutes, ~3GB disk)"]
    async fn perf_ivf_pq_vs_flat_1m() {
        let dim: i32 = 768;
        let total = 1_000_000usize;
        let batch = 2_000usize;

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::file(dir.path());
        std::mem::forget(dir);
        storage
            .create_bucket(&test_bucket_meta(&storage, "vb"))
            .await
            .unwrap();
        storage
            .create_index(&IndexMeta {
                vector_bucket_name: "vb".into(),
                index_name: "ix1m".into(),
                index_arn: "arn:aws:s3vectors:us-east-1:0:bucket/vb/index/ix1m".into(),
                creation_time_rfc3339: "2026-11-06T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                data_type: "float32".into(),
                dimension: dim,
                distance_metric: "cosine".into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
                x_vf_derived: None,
            })
            .await
            .unwrap();

        // unit vectors: random direction, normalized once — cosine-friendly
        // and no single dominant axis
        let mut rng = lcg(20261106);
        let unit = |rng: &mut dyn FnMut() -> u64| -> Vec<f32> {
            let v: Vec<f32> = (0..dim)
                .map(|_| (rng() % 20_000) as f32 / 10_000.0 - 1.0)
                .collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
            v.into_iter().map(|x| x / norm).collect()
        };

        let t0 = std::time::Instant::now();
        for b in 0..total.div_ceil(batch) {
            let rows: Vec<VectorRow> = (0..batch)
                .map(|i| VectorRow {
                    key: format!("k{:08}", b * batch + i),
                    vector: unit(&mut rng),
                    metadata_json: None,
                    filterable: BTreeMap::new(),
                })
                .collect();
            storage
                .upsert_vectors("vb", "ix1m", dim, rows)
                .await
                .unwrap();
            if b % 100 == 0 {
                eprintln!(
                    "ingest {}/{} ({:.0}s)",
                    b * batch,
                    total,
                    t0.elapsed().as_secs()
                );
            }
        }
        eprintln!(
            "ingest: {total} rows dim={dim} in {:.1}s",
            t0.elapsed().as_secs_f64()
        );

        // 30 fixed query vectors (their flat top-10s are the recall truth)
        let mut qrng = lcg(99);
        let queries: Vec<Vec<f32>> = (0..30).map(|_| unit(&mut qrng)).collect();
        let iters = 30usize;

        // ---- phase 1: FLAT (no index; the pre-T8 default path) ----
        let mut flat_ms = Vec::with_capacity(queries.len() * iters);
        let mut truth: Vec<Vec<String>> = Vec::with_capacity(queries.len());
        for (qi, q) in queries.iter().enumerate() {
            for it in 0..=iters {
                let t = std::time::Instant::now();
                let out = storage
                    .query_vectors(
                        "vb",
                        "ix1m",
                        dim,
                        "cosine",
                        q,
                        10,
                        None,
                        QueryRoute::Pushdown,
                        None,
                    )
                    .await
                    .unwrap();
                let dt = t.elapsed().as_secs_f64() * 1000.0;
                if it > 0 {
                    flat_ms.push(dt);
                }
                if it == 0 {
                    // the first (warm-up) call doubles as the recall truth
                    truth.push(out.hits.iter().map(|h| h.key.clone()).collect());
                    let _ = qi;
                }
            }
        }

        // ---- build IVF_PQ (the explicit verb's engine call) ----
        let t = std::time::Instant::now();
        let outcome = storage
            .optimize_vector_index("vb", "ix1m", "cosine")
            .await
            .unwrap();
        eprintln!(
            "ivf_pq train: {outcome:?} in {:.1}s",
            t.elapsed().as_secs_f64()
        );

        // ---- phase 2: INDEXED ----
        let mut idx_ms = Vec::with_capacity(iters);
        let mut recall_hits = 0usize;
        let mut recall_total = 0usize;
        for (qi, q) in queries.iter().enumerate() {
            for it in 0..=iters {
                let t = std::time::Instant::now();
                let out = storage
                    .query_vectors(
                        "vb",
                        "ix1m",
                        dim,
                        "cosine",
                        q,
                        10,
                        None,
                        QueryRoute::Pushdown,
                        None,
                    )
                    .await
                    .unwrap();
                let dt = t.elapsed().as_secs_f64() * 1000.0;
                if it > 0 {
                    idx_ms.push(dt);
                }
                if it == 0 {
                    let got: Vec<&str> = out.hits.iter().map(|h| h.key.as_str()).collect();
                    recall_total += truth[qi].len();
                    recall_hits += truth[qi]
                        .iter()
                        .filter(|k| got.contains(&k.as_str()))
                        .count();
                }
            }
        }
        flat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        idx_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |v: &[f64], p: f64| v[((v.len() as f64 - 1.0) * p).round() as usize];
        eprintln!(
            "flat  P50 {:>7.1} ms  P99 {:>7.1} ms   ({} calls)",
            q(&flat_ms, 0.50),
            q(&flat_ms, 0.99),
            flat_ms.len()
        );
        eprintln!(
            "ivf_pq P50 {:>7.1} ms  P99 {:>7.1} ms   ({} calls)",
            q(&idx_ms, 0.50),
            q(&idx_ms, 0.99),
            idx_ms.len()
        );
        eprintln!(
            "speedup P50 {:.1}x / P99 {:.1}x — recall@10 {}/{} = {:.3}",
            q(&flat_ms, 0.50) / q(&idx_ms, 0.50),
            q(&flat_ms, 0.99) / q(&idx_ms, 0.99),
            recall_hits,
            recall_total,
            recall_hits as f64 / recall_total as f64
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
        assert!(matches!(&e, Err(StoreError::Validation(_))), "{e:?}");
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

    // ---- M2 T3: Source markers + derived-index meta tolerance ----

    fn marker(object_key: &str, status: SourceStatus) -> SourceMarker {
        SourceMarker {
            object_key: object_key.to_string(),
            etag_sig: "size=1;mtime_ns=2".into(),
            content_hash: "ab".repeat(32),
            pipeline_fingerprint: "fp".into(),
            status,
            status_reason: None,
            retries: 0,
            retries_exhausted: false,
            misses: 0,
            updated_at: "2026-11-05T00:00:00.000Z".into(),
            chunks: Some(BTreeMap::from([("text".to_string(), 3)])),
            engines: None,
        }
    }

    /// Engine-observability schema interop (2026-11-05): a marker JSON from
    /// BEFORE the `engines` field deserializes with `engines == None`
    /// (pre-observability markers never warn in doctor), and a marker WITH
    /// the field round-trips it byte-equal.
    #[test]
    fn source_marker_engines_schema_interop() {
        let pre = serde_json::json!({
            "object_key": "docs/a.txt",
            "etag_sig": "size=1;mtime_ns=2",
            "pipeline_fingerprint": "fp",
            "status": "INDEXED",
            "updated_at": "2026-11-01T00:00:00.000Z",
        });
        let m: SourceMarker = serde_json::from_value(pre).expect("pre-observability marker");
        assert_eq!(m.status, SourceStatus::Indexed);
        assert!(m.engines.is_none(), "old markers read as None, never warn");
        assert_eq!(m.misses, 0, "pre-v3.1 markers read as zero strikes");

        let with = serde_json::json!({
            "object_key": "docs/b.txt",
            "etag_sig": "size=1;mtime_ns=3",
            "pipeline_fingerprint": "fp",
            "status": "INDEXED",
            "updated_at": "2026-11-05T00:00:00.000Z",
            "engines": { "pdfium": "chromium/8066", "pdf-inspector": "1.25.2" },
        });
        let m: SourceMarker = serde_json::from_value(with.clone()).unwrap();
        assert_eq!(m.engines.as_ref().unwrap()["pdfium"], "chromium/8066");
        // and serializes the field back (skip_serializing_if only omits None)
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["engines"], with["engines"]);
        // a None engine set is OMITTED from the JSON (byte-stable for
        // old readers)
        let mut none_marker = m.clone();
        none_marker.engines = None;
        assert!(
            serde_json::to_value(&none_marker)
                .unwrap()
                .get("engines")
                .is_none()
        );
    }

    #[tokio::test]
    async fn source_marker_round_trip_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::file(dir.path());
        let meta = test_bucket_meta(&s, "vb");
        s.create_bucket(&meta).await.unwrap();
        // absent -> None
        assert!(s.load_source("vb", "deadbeef").await.unwrap().is_none());
        // save two, load both back byte-equal on the fields that matter
        let m1 = marker("docs/a.txt", SourceStatus::Pending);
        let m2 = marker("docs/b.txt", SourceStatus::Indexed);
        s.save_source("vb", "hash0000000000001", &m1).await.unwrap();
        s.save_source("vb", "hash0000000000002", &m2).await.unwrap();
        assert_eq!(
            s.load_source("vb", "hash0000000000001").await.unwrap(),
            Some(m1.clone())
        );
        let listed = s.list_sources("vb").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].object_key, "docs/a.txt"); // sorted by object key
        // overwrite + delete + idempotent re-delete
        let mut m1b = m1.clone();
        m1b.status = SourceStatus::Indexed;
        s.save_source("vb", "hash0000000000001", &m1b)
            .await
            .unwrap();
        assert_eq!(
            s.load_source("vb", "hash0000000000001")
                .await
                .unwrap()
                .unwrap()
                .status,
            SourceStatus::Indexed
        );
        s.delete_source("vb", "hash0000000000002").await.unwrap();
        s.delete_source("vb", "hash0000000000002").await.unwrap();
        assert_eq!(s.list_sources("vb").await.unwrap().len(), 1);
        // markers live in the SYSTEM bucket, never in the source namespace
        // (§5.3-1/§5.3-3): <data>/vb-vf/sources exists, and the source
        // bucket was never even materialized (lazy, D4)
        assert!(dir.path().join("vb-vf/sources").is_dir());
        assert!(
            !dir.path().join("vb").exists(),
            "source bucket must hold zero system objects (it is never created by marker ops)"
        );
    }

    #[test]
    fn index_meta_tolerates_missing_and_unknown_derived_field() {
        // pre-T3 shape: no x_vf_derived — parses with None (old index.json)
        let old = r#"{
            "vector_bucket_name": "vb", "index_name": "ix",
            "index_arn": "arn", "creation_time_rfc3339": "t",
            "creation_time_epoch": 1.0, "data_type": "float32",
            "dimension": 8, "distance_metric": "cosine"
        }"#;
        let meta: IndexMeta = serde_json::from_str(old).unwrap();
        assert!(meta.x_vf_derived.is_none());
        // unknown FUTURE fields are ignored (forward-compat, §5.10.5):
        // splice a new member INSIDE the object
        let future = old.replace(
            "\"distance_metric\": \"cosine\"",
            "\"distance_metric\": \"cosine\", \"x_vf_next_big_thing\": {}",
        );
        let meta: IndexMeta = serde_json::from_str(&future).unwrap();
        assert!(meta.x_vf_derived.is_none());
        // derived stamp round-trips
        let with = old.replace(
            "\"distance_metric\": \"cosine\"",
            &format!(
                "\"distance_metric\": \"cosine\", \"x_vf_derived\": {{\"key\":\"{}\",\
                 \"embedder\":\"e\",\"model\":\"m\",\"dim\":8}}",
                "0".repeat(64)
            ),
        );
        let meta: IndexMeta = serde_json::from_str(&with).unwrap();
        let d = meta.x_vf_derived.unwrap();
        assert_eq!(d.dim, 8);
        assert_eq!(d.key.len(), 64);
    }

    #[test]
    fn source_status_wire_names_are_kb_aligned() {
        for (s, want) in [
            (SourceStatus::Pending, "PENDING"),
            (SourceStatus::InProgress, "IN_PROGRESS"),
            (SourceStatus::PartiallyIndexed, "PARTIALLY_INDEXED"),
            (SourceStatus::Indexed, "INDEXED"),
            (SourceStatus::Failed, "FAILED"),
            (SourceStatus::Deleting, "DELETING"),
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(want));
            assert_eq!(
                serde_json::from_value::<SourceStatus>(json!(want)).unwrap(),
                s
            );
            assert_eq!(s.to_string(), want);
        }
    }

    // ---- v3 lifecycle + enumeration (design-v3.md §3-R3/R9) ----

    /// Create replay = Conflict with the 存量 hint; a foreign non-empty
    /// system bucket = Conflict with the bucket_suffix hint; empty residue
    /// is adopted (D7).
    #[tokio::test]
    async fn bucket_lifecycle_conflict_adoption_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::file(dir.path());
        let meta = test_bucket_meta(&s, "docs");
        s.create_bucket(&meta).await.unwrap();
        assert!(dir.path().join("docs-vf/bucket.json").is_file());

        // replay -> Conflict, hint names the existing bucket
        match s.create_bucket(&meta).await {
            Err(StoreError::Conflict(m)) => assert!(m.contains("already exists"), "{m}"),
            other => panic!("expected conflict, got {other:?}"),
        }

        // foreign non-empty system bucket (no marker) -> Conflict with the
        // bucket_suffix hint
        let foreign = test_bucket_meta(&s, "wiki");
        let sys_store = s.system_store("wiki"); // materializes wiki-vf
        backends::put_json_overwrite(&*sys_store, &"someone-elses-object".into(), b"data")
            .await
            .unwrap();
        match s.create_bucket(&foreign).await {
            Err(StoreError::Conflict(m)) => assert!(m.contains("bucket_suffix"), "{m}"),
            other => panic!("expected conflict, got {other:?}"),
        }

        // empty residue (no marker, no objects) -> adopted (D7)
        let residue = test_bucket_meta(&s, "logs");
        let _ = s.system_store("logs"); // materialize the empty dir
        s.create_bucket(&residue).await.unwrap();
        assert!(s.load_bucket("logs").await.unwrap().is_some());

        // delete order converges and removes the physical bucket; source
        // bucket dir is never created by create/delete (lazy, D4)
        s.delete_bucket("docs").await.unwrap();
        assert!(s.load_bucket("docs").await.unwrap().is_none());
        assert!(!dir.path().join("docs-vf").exists());
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|n| n == "docs"),
            "source bucket never created (entries: {entries:?})"
        );
    }

    /// R9 enumeration is a suffix scan + ownership marker check: foreign
    /// markers and marker-less `-vf` buckets are skipped.
    #[tokio::test]
    async fn list_buckets_is_suffix_scan_with_marker_check() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::file(dir.path());
        let identity = s.deployment_identity("us-east-1", "000000000000");
        assert!(s.list_buckets(&identity).await.unwrap().is_empty());

        s.create_bucket(&test_bucket_meta(&s, "docs"))
            .await
            .unwrap();
        s.create_bucket(&test_bucket_meta(&s, "kb")).await.unwrap();

        // a foreign-owned marker (different deployment identity) is skipped
        let mut foreign = test_bucket_meta(&s, "other");
        foreign.deployment = DeploymentIdentity {
            region: "eu-west-1".into(),
            account: "999999999999".into(),
            endpoint: s.backend().endpoint_id(),
        };
        s.create_bucket(&foreign).await.unwrap();

        // a marker-less `-vf` directory (somebody's real bucket) is skipped
        let _ = s.system_store("plain"); // materializes plain-vf/ only

        let listed = s.list_buckets(&identity).await.unwrap();
        let names: Vec<&str> = listed
            .iter()
            .map(|m| m.vector_bucket_name.as_str())
            .collect();
        assert_eq!(names, vec!["docs", "kb"], "{names:?}");

        // the placement echo is materialized per backend
        assert_eq!(
            listed[0].placement,
            s.placement("docs"),
            "placement is recorded on the marker"
        );
    }
}
