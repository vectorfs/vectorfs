//! `vectorfs status` (M1 T6, design.md §5.8): read-only inspection of a data root —
//! NO daemon parameters required, NO lock taken, NO writes performed.
//!
//! Two modes:
//! - **data-dir mode** (default): resolve the data root exactly like serve
//!   (`--data-dir` > `VF_DATA_DIR` > `./vectorfs-data`; backend `--backend` >
//!   `VF_BACKEND` > `file`), peek at the LOCK object (held / stale / free —
//!   the same TTL logic as lock.rs, but purely observational), then summarize
//!   the catalog: backend kind, bucket/index counts, per-index dimension /
//!   metric / vector count (lance `count_rows`, read-only) and the total
//!   object-store volume of the data root (object sizes from list metadata).
//! - **`--endpoint URL` mode**: GET `<URL>/healthz` over hyper's client
//!   (http only — no TLS provider is linked) and show what the daemon
//!   reports. This is the mode that answers "is the daemon up", because the
//!   LOCK object carries no port.
//!
//! Output contract (report.rs): stdout = human table or `--json` structured
//! object with `schema_version`; stderr = progress; exit 0 normal, non-zero
//! with `{code, message, hint}` on stderr otherwise.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use crate::backends::{self, Backend};
use crate::config::StorageConfig;
use crate::lock::{LOCK_TTL, LockInfo};
use crate::report::{Failure, SCHEMA_VERSION};
use crate::store::Storage;

/// LOCK observation for status/doctor — never acquires anything.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    /// A live writer holds it (heartbeat newer than TTL).
    Held,
    /// Left behind by a dead writer (heartbeat older than TTL); the next
    /// writer takes over automatically.
    Stale,
    /// No LOCK object at all.
    Free,
}

#[derive(Debug, Serialize)]
pub struct DaemonInfo {
    pub owner: String,
    pub pid: u32,
    pub host: String,
    /// epoch seconds
    pub started_at: f64,
    pub started_rfc3339: String,
    pub heartbeat_age_s: u64,
}

#[derive(Debug, Serialize)]
pub struct LockProbe {
    pub state: LockState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonInfo>,
    /// the TTL the state was judged against (lock.rs LOCK_TTL)
    pub ttl_s: u64,
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Read ONE bucket's LOCK object and classify it (design-v3.md §5.1: locks
/// are per vb, at `X-vf/LOCK`). A LOCK that exists but does not parse is
/// reported as [`LockState::Stale`] debris with no daemon details (the same
/// fail-open takeover rule lock.rs applies to a real writer).
pub async fn probe_lock(storage: &Storage, vb: &str) -> Result<LockProbe, String> {
    let key = object_store::path::Path::from("LOCK");
    let raw = backends::read_json::<serde_json::Value>(&*storage.system_store(vb), &key).await?;
    let ttl_s = LOCK_TTL.as_secs();
    match raw {
        None => Ok(LockProbe {
            state: LockState::Free,
            daemon: None,
            ttl_s,
        }),
        Some(v) => {
            let age = v
                .get("heartbeat_at")
                .and_then(|h| h.as_f64())
                .map(|h| (now_epoch() - h).max(0.0))
                .unwrap_or(f64::INFINITY);
            let info: Option<LockInfo> = serde_json::from_value(v).ok();
            let state = if age < LOCK_TTL.as_secs_f64() {
                LockState::Held
            } else {
                LockState::Stale
            };
            let daemon = info.map(|i| DaemonInfo {
                owner: i.owner_uuid,
                pid: i.pid,
                host: i.hostname,
                started_at: i.started_at,
                started_rfc3339: crate::ops::epoch_to_rfc3339(i.started_at),
                heartbeat_age_s: age as u64,
            });
            Ok(LockProbe {
                state,
                daemon,
                ttl_s,
            })
        }
    }
}

/// One bucket row of the per-vb lock table.
#[derive(Debug, Serialize)]
pub struct BucketLockRow {
    pub bucket: String,
    #[serde(flatten)]
    pub probe: LockProbe,
}

/// One index row of the status table.
#[derive(Debug, Serialize)]
pub struct IndexRow {
    pub bucket: String,
    pub index: String,
    pub dimension: i32,
    pub distance_metric: String,
    pub data_type: String,
    /// Lance `count_rows` — `None` when the dataset could not be opened
    /// (the row then carries `error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vectors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One DERIVED (x_vf_derived-stamped) index of an ingest bucket — the local
/// ingest block's row (T7). Read from the catalog stamp, never from a
/// config-derived spec: status is read-only and must not load embedders.
#[derive(Debug, Serialize)]
pub struct DerivedIndexRow {
    pub index: String,
    pub embedder: String,
    pub model: String,
    pub dim: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vectors: Option<u64>,
}

/// One ingest bucket's local summary (T7): active fingerprint, marker
/// status counts, derived indexes.
#[derive(Debug, Serialize)]
pub struct IngestBucketRow {
    pub bucket: String,
    pub fingerprint: String,
    /// marker status name -> count (the six SourceStatus states).
    pub sources: BTreeMap<String, u64>,
    pub derived: Vec<DerivedIndexRow>,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub mode: &'static str,
    /// {"kind": "file"|"memory"|"s3", "spec": "<display form>"}
    pub backend: BackendSummary,
    /// Local data dir (bucket root + the `.vectorfs/` daemon home).
    pub data_root: String,
    /// Per-vb LOCK probes (v3 §5.1: one lock per bucket at `X-vf/LOCK`);
    /// empty when no buckets exist.
    pub locks: Vec<BucketLockRow>,
    pub buckets: usize,
    pub indexes: usize,
    /// Sum over indexes whose count succeeded.
    pub vectors_total: u64,
    pub index_details: Vec<IndexRow>,
    pub objects: usize,
    pub size_bytes: u64,
    /// File-layer ingest summary (T7): one row per [ingest.<bucket>]
    /// section; EMPTY when the config has no ingest sections (pure protocol
    /// face) or is unparseable (doctor reports that case — status stays
    /// read-only and quiet beyond a stderr warning).
    pub ingest: Vec<IngestBucketRow>,
}

#[derive(Debug, Serialize)]
pub struct BackendSummary {
    pub kind: &'static str,
    pub spec: String,
}

/// Gather the data-dir-mode report. Purely read-only: the LOCK is observed,
/// never taken; per-index counts go through lance's read path.
pub async fn gather(cfg: &StorageConfig) -> Result<StatusReport, Failure> {
    // Existence BEFORE Backend::parse: parse creates + canonicalizes the file
    // root, and `vectorfs status` must not materialize a data dir that isn't there.
    if cfg.is_file_backend() && !cfg.data_dir.exists() {
        return Err(Failure::new(
            "NO_DATA_ROOT",
            format!("no data root at {}", cfg.data_dir.display()),
            format!("run: vectorfs init --data-dir {}", cfg.data_dir.display()),
        ));
    }
    let backend = cfg.backend().map_err(|e| {
        Failure::new(
            "BACKEND_INVALID",
            e,
            "backend must be file | memory | s3+http(s)://host:port",
        )
    })?;
    let storage = cfg.storage().map_err(|e| {
        Failure::new(
            "BACKEND_INIT",
            format!("cannot open backend {backend}: {e}"),
            "check the data dir / S3 endpoint and credentials (VF_S3_*, AWS_*)",
        )
    })?;
    // deployment identity from the config file (region/account) + backend
    let (region, account) = crate::config::read_config(&cfg.data_dir)
        .ok()
        .flatten()
        .map(|c| (c.region, c.account_id))
        .unwrap_or_else(|| {
            (
                crate::config::DEFAULT_REGION.to_string(),
                crate::config::DEFAULT_ACCOUNT.to_string(),
            )
        });
    let identity = storage.deployment_identity(&region, &account);

    let mut index_details = Vec::new();
    let mut vectors_total = 0u64;
    let mut bucket_count = 0usize;
    let mut failures = Vec::new();
    let mut locks = Vec::new();
    for bucket in storage
        .list_buckets(&identity)
        .await
        .map_err(store_failure("list buckets"))?
    {
        bucket_count += 1;
        locks.push(BucketLockRow {
            bucket: bucket.vector_bucket_name.clone(),
            probe: probe_lock(&storage, &bucket.vector_bucket_name)
                .await
                .map_err(|e| {
                    Failure::new(
                        "LOCK_UNREADABLE",
                        format!(
                            "cannot read the LOCK of bucket {}: {e}",
                            bucket.vector_bucket_name
                        ),
                        "inspect the LOCK object in the bucket's system bucket; if unreadable, \
                         remove it while no writer runs",
                    )
                })?,
        });
        let indexes = storage
            .list_indexes(&bucket.vector_bucket_name)
            .await
            .map_err(store_failure("list indexes"))?;
        for meta in indexes {
            match storage
                .count_vectors(&meta.vector_bucket_name, &meta.index_name)
                .await
            {
                Ok(n) => {
                    vectors_total += n as u64;
                    index_details.push(IndexRow {
                        bucket: meta.vector_bucket_name.clone(),
                        index: meta.index_name.clone(),
                        dimension: meta.dimension,
                        distance_metric: meta.distance_metric.clone(),
                        data_type: meta.data_type.clone(),
                        vectors: Some(n as u64),
                        error: None,
                    });
                }
                Err(e) => {
                    failures.push(format!(
                        "{}/{}: {e}",
                        meta.vector_bucket_name, meta.index_name
                    ));
                    index_details.push(IndexRow {
                        bucket: meta.vector_bucket_name.clone(),
                        index: meta.index_name.clone(),
                        dimension: meta.dimension,
                        distance_metric: meta.distance_metric.clone(),
                        data_type: meta.data_type.clone(),
                        vectors: None,
                        error: Some(e.to_string()),
                    });
                }
            }
        }
    }
    if !failures.is_empty() {
        // stderr = progress/diagnostics; the rows themselves carry the error.
        eprintln!(
            "vectorfs: warning: {} dataset(s) could not be counted: {}",
            failures.len(),
            failures.join("; ")
        );
    }

    // Volume accounting per BUCKET PAIR (design-v3.md §6 status row): both
    // the source bucket (user data) and the system bucket (catalog + Lance
    // datasets) of every vb, from list metadata only.
    let mut objects = 0usize;
    let mut size_bytes = 0u64;
    for row in &locks {
        let vb = &row.bucket;
        for stats in [
            backends::prefix_stats(&*storage.source_store(vb), None).await,
            backends::prefix_stats(&*storage.system_store(vb), None).await,
        ] {
            let (o, sz) = stats.map_err(|e| {
                Failure::new(
                    "BACKEND_LIST",
                    format!("cannot enumerate bucket {vb}: {e}"),
                    "check backend reachability (vectorfs doctor probes it in depth)",
                )
            })?;
            objects += o;
            size_bytes += sz;
        }
    }

    let kind = match backend {
        Backend::File { .. } => "file",
        Backend::Memory => "memory",
        Backend::S3(_) => "s3",
    };
    let ingest = gather_ingest(cfg, &storage).await;
    Ok(StatusReport {
        schema_version: SCHEMA_VERSION,
        mode: "data-dir",
        backend: BackendSummary {
            kind,
            spec: backend.to_string(),
        },
        data_root: cfg.data_dir.display().to_string(),
        locks,
        buckets: bucket_count,
        indexes: index_details.len(),
        vectors_total,
        index_details,
        objects,
        size_bytes,
        ingest,
    })
}

/// The local ingest block (T7): buckets + source counts + derived indexes.
/// Derived indexes come from the CATALOG's `x_vf_derived` stamps (not from
/// config-derived specs — computing those needs loaded embedders, and status
/// never loads models); the fingerprint is pure TOML. Read-only throughout.
async fn gather_ingest(cfg: &StorageConfig, storage: &Storage) -> Vec<IngestBucketRow> {
    let text = match std::fs::read_to_string(crate::config::config_path(&cfg.data_dir)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("vectorfs: warning: cannot read pipeline config: {e}");
            return Vec::new();
        }
    };
    let pcfg = match crate::pipeline::config::load_pipeline_config(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "vectorfs: warning: pipeline config invalid (vectorfs doctor probes it): {e}"
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for vb in pcfg.ingest.keys() {
        let fingerprint = pcfg.fingerprint(vb).expect("bucket in config");
        let mut sources: BTreeMap<String, u64> = [
            "PENDING",
            "IN_PROGRESS",
            "PARTIALLY_INDEXED",
            "INDEXED",
            "FAILED",
            "DELETING",
        ]
        .iter()
        .map(|s| (s.to_string(), 0))
        .collect();
        if let Ok(markers) = storage.list_sources(vb).await {
            for m in markers {
                *sources.entry(m.status.as_str().to_string()).or_insert(0) += 1;
            }
        }
        let mut derived = Vec::new();
        if let Ok(indexes) = storage.list_indexes(vb).await {
            for meta in indexes.into_iter().filter(|m| m.x_vf_derived.is_some()) {
                let stamp = meta.x_vf_derived.as_ref().expect("just filtered");
                derived.push(DerivedIndexRow {
                    index: meta.index_name.clone(),
                    embedder: stamp.embedder.clone(),
                    model: stamp.model.clone(),
                    dim: stamp.dim,
                    vectors: storage
                        .count_vectors(vb, &meta.index_name)
                        .await
                        .ok()
                        .map(|n| n as u64),
                });
            }
        }
        out.push(IngestBucketRow {
            bucket: vb.clone(),
            fingerprint,
            sources,
            derived,
        });
    }
    out
}

fn store_failure(what: &'static str) -> impl Fn(crate::store::StoreError) -> Failure {
    move |e| {
        Failure::new(
            "CATALOG_READ",
            format!("cannot {what}: {e}"),
            "check backend health with: vectorfs doctor",
        )
    }
}

/// Human-readable rendering (stdout). Field order is the report's; no
/// timestamps beyond what the data carries.
pub fn render_human(r: &StatusReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "vectorfs status (schema v{})", r.schema_version);
    let _ = writeln!(out, "  data root : {}", r.data_root);
    let _ = writeln!(out, "  backend   : {} ({})", r.backend.spec, r.backend.kind);
    // per-vb LOCK lines (v3 §5.1): each row names its bucket
    for row in &r.locks {
        match (&row.probe.state, &row.probe.daemon) {
            (LockState::Free, _) => {
                let _ = writeln!(out, "  lock/{:<8}: free", row.bucket);
            }
            (state, daemon) => {
                let label = match state {
                    LockState::Held => "held",
                    LockState::Stale => "STALE",
                    LockState::Free => unreachable!("matched above"),
                };
                match daemon {
                    Some(d) => {
                        let _ = writeln!(
                            out,
                            "  lock/{:<8}: {label} by pid {} (owner {}…, host {}, heartbeat {}s ago)",
                            row.bucket,
                            d.pid,
                            &d.owner[..8.min(d.owner.len())],
                            d.host,
                            d.heartbeat_age_s
                        );
                        if matches!(state, LockState::Stale) {
                            let _ = writeln!(
                                out,
                                "              heartbeat older than TTL ({}s) — the next writer takes over",
                                row.probe.ttl_s
                            );
                        }
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "  lock/{:<8}: {label} but unparseable (debris; next writer takes over)",
                            row.bucket
                        );
                    }
                }
            }
        }
    }
    if r.locks.is_empty() {
        let _ = writeln!(out, "  locks     : none (no vector buckets)");
    }
    let _ = writeln!(out, "  buckets   : {}", r.buckets);
    let _ = writeln!(
        out,
        "  indexes   : {} ({} vectors)",
        r.indexes, r.vectors_total
    );
    let _ = writeln!(
        out,
        "  data      : {} across {} objects",
        crate::report::human_bytes(r.size_bytes),
        r.objects
    );
    if !r.index_details.is_empty() {
        let _ = writeln!(out, "  indexes:");
        let _ = writeln!(
            out,
            "    {:<34} {:>5}  {:<9} {:>8}",
            "bucket/index", "dim", "metric", "vectors"
        );
        for row in &r.index_details {
            let vectors = match (row.vectors, &row.error) {
                (Some(n), _) => n.to_string(),
                (None, Some(e)) => format!("ERROR: {e}"),
                (None, None) => "?".to_string(),
            };
            let _ = writeln!(
                out,
                "    {:<34} {:>5}  {:<9} {:>8}",
                format!("{}/{}", row.bucket, row.index),
                row.dimension,
                row.distance_metric,
                vectors
            );
        }
    }
    if !r.ingest.is_empty() {
        let _ = writeln!(out, "  ingest:");
        for row in &r.ingest {
            let _ = writeln!(
                out,
                "    {} : fingerprint {}… sources {}",
                row.bucket,
                &row.fingerprint[..8],
                row.sources.values().sum::<u64>()
            );
            let _ = writeln!(
                out,
                "      INDEXED={} PARTIAL={} PENDING={} IN_PROGRESS={} FAILED={} DELETING={}",
                row.sources["INDEXED"],
                row.sources["PARTIALLY_INDEXED"],
                row.sources["PENDING"],
                row.sources["IN_PROGRESS"],
                row.sources["FAILED"],
                row.sources["DELETING"],
            );
            for ix in &row.derived {
                let vectors = ix
                    .vectors
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into());
                let _ = writeln!(
                    out,
                    "      index {} (embedder={}, model={}, dim={}, vectors={})",
                    ix.index, ix.embedder, ix.model, ix.dim, vectors
                );
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// --endpoint mode
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct EndpointReport {
    pub schema_version: u32,
    pub mode: &'static str,
    pub endpoint: String,
    pub http_status: u16,
    /// The /healthz JSON body as returned by the daemon ({"ok":true,
    /// "version":…, "uptimeSeconds":…}); non-JSON bodies are wrapped raw.
    pub health: serde_json::Value,
}

/// GET `<url>/healthz` with a 5s budget. http only: no TLS provider is
/// linked into the binary (design.md §5.7 no-outbound posture) — probing a
/// remote https endpoint is out of scope for a local-daemon liveness check.
pub async fn endpoint_probe(url: &str) -> Result<EndpointReport, Failure> {
    use http_body_util::BodyExt;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let uri: http::Uri = url.parse().map_err(|e| {
        Failure::new(
            "ENDPOINT_INVALID",
            format!("cannot parse endpoint {url:?}: {e}"),
            "use the form http://127.0.0.1:8787",
        )
    })?;
    if uri.scheme_str() == Some("https") {
        return Err(Failure::new(
            "HTTPS_UNSUPPORTED",
            "the built-in probe links no TLS stack; https endpoints are unsupported",
            "probe a local http:// endpoint, or use: curl -s <url>/healthz",
        ));
    }
    if uri.host().is_none() {
        return Err(Failure::new(
            "ENDPOINT_INVALID",
            format!("endpoint {url:?} carries no host"),
            "use the form http://127.0.0.1:8787",
        ));
    }
    // normalize the path: exactly one /healthz, with or without a trailing
    // slash on the base URL
    let base = uri.path().trim_end_matches('/');
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(
        format!("{base}/healthz")
            .parse::<http::uri::PathAndQuery>()
            .expect("valid path"),
    );
    let health_uri = http::Uri::from_parts(parts).expect("reassembly preserves validity");

    let client: Client<_, http_body_util::Full<bytes::Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let resp = tokio::time::timeout(Duration::from_secs(5), client.get(health_uri))
        .await
        .map_err(|_| {
            Failure::new(
                "ENDPOINT_TIMEOUT",
                format!("GET {url}/healthz timed out after 5s"),
                "is a vectorfs daemon listening there? try: vectorfs status --data-dir …",
            )
        })?
        .map_err(|e| {
            Failure::new(
                "ENDPOINT_UNREACHABLE",
                format!("GET {url}/healthz failed: {e}"),
                "is `vectorfs serve` running? start it with: vectorfs serve --data-dir …",
            )
        })?;
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| {
            Failure::new(
                "ENDPOINT_READ",
                format!("reading the healthz body failed: {e}"),
                "retry; if it persists the daemon is unhealthy",
            )
        })?
        .to_bytes();
    let health = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(_) => serde_json::json!({
            "raw": truncate_safe(&String::from_utf8_lossy(&body), 512),
        }),
    };
    if !status.is_success() {
        return Err(Failure::new(
            "ENDPOINT_HTTP_ERROR",
            format!("GET {url}/healthz answered HTTP {status}"),
            "the daemon is reachable but unhealthy — check its stderr log",
        ));
    }
    Ok(EndpointReport {
        schema_version: SCHEMA_VERSION,
        mode: "endpoint",
        endpoint: url.to_string(),
        http_status: status.as_u16(),
        health,
    })
}

pub fn render_endpoint_human(r: &EndpointReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "vectorfs status (schema v{})", r.schema_version);
    let _ = writeln!(out, "  endpoint : {}", r.endpoint);
    let _ = writeln!(out, "  healthz  : HTTP {}", r.http_status);
    for key in ["ok", "version", "uptimeSeconds"] {
        if let Some(v) = r.health.get(key) {
            let _ = writeln!(out, "  {key:<9}: {v}");
        }
    }
    // engine observability (2026-11-05): the extraction/embedding engine
    // versions the daemon runs — one line each, sorted (the JSON object is
    // a BTreeMap, so health.get preserves that order)
    if let Some(engines) = r.health.get("engines").and_then(|e| e.as_object()) {
        let _ = writeln!(out, "  engines  :");
        for (name, version) in engines {
            // bare string rendering (a Value's Display keeps JSON quotes)
            let v = version
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| version.to_string());
            let _ = writeln!(out, "    {name:<9} {v}");
        }
    }
    out
}

/// Lossy truncation for raw (non-JSON) healthz bodies in diagnostics
/// (char-boundary safe).
fn truncate_safe(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::store::{DerivedIndexInfo, IndexMeta};

    fn storage_cfg(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            data_dir: dir.to_path_buf(),
            backend_spec: "file".into(),
            s3_region: "us-east-1".into(),
            s3_access_key: None,
            s3_secret_key: None,
            bucket_suffix: crate::backends::DEFAULT_BUCKET_SUFFIX.into(),
        }
    }

    async fn seed(dir: &std::path::Path) -> Arc<Storage> {
        let storage = Storage::file(dir);
        storage
            .create_bucket(&crate::store::test_bucket_meta(&storage, "docs"))
            .await
            .unwrap();
        for (ix, dim) in [("chunks", 4i32), ("titles", 2i32)] {
            storage
                .create_index(&IndexMeta {
                    vector_bucket_name: "docs".into(),
                    index_name: ix.into(),
                    index_arn: format!("arn:aws:s3vectors:us-east-1:0:bucket/docs/index/{ix}"),
                    creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
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
            storage
                .upsert_vectors(
                    "docs",
                    ix,
                    dim,
                    vec![crate::store::VectorRow {
                        key: "k1".into(),
                        vector: vec![1.0; dim as usize],
                        metadata_json: None,
                        filterable: Default::default(),
                    }],
                )
                .await
                .unwrap();
        }
        Arc::new(storage)
    }

    #[tokio::test]
    async fn status_no_data_root_is_actionable() {
        let cfg = storage_cfg(std::path::Path::new("/nonexistent/vf-status-tmp"));
        let err = gather(&cfg).await.unwrap_err();
        assert_eq!(err.code, "NO_DATA_ROOT");
        assert!(err.hint.contains("vectorfs init"), "{}", err.hint);
    }

    #[tokio::test]
    async fn status_summarizes_catalog_and_free_lock() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path()).await;
        let report = gather(&storage_cfg(dir.path())).await.unwrap();
        assert_eq!(report.backend.kind, "file");
        assert_eq!(report.buckets, 1);
        assert_eq!(report.indexes, 2);
        assert_eq!(report.vectors_total, 2);
        assert!(report.size_bytes > 0, "lance data must account for bytes");
        assert!(report.objects > 0);
        assert!(
            report
                .locks
                .iter()
                .all(|r| matches!(r.probe.state, LockState::Free))
        );
        // deterministic row order: (bucket, index) ascending
        let names: Vec<(String, String)> = report
            .index_details
            .iter()
            .map(|r| (r.bucket.clone(), r.index.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("docs".into(), "chunks".into()),
                ("docs".into(), "titles".into())
            ]
        );
        // human rendering carries the load-bearing fields
        let text = render_human(&report);
        assert!(text.contains("docs/chunks"), "{text}");
        assert!(text.contains("free"), "{text}");
        assert!(text.contains("cosine"), "{text}");
        // json mode carries the schema version
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn status_reports_held_and_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let storage = seed(dir.path()).await;
        // v3: the LOCK lives per bucket, at docs-vf/LOCK
        let key = object_store::path::Path::from("LOCK");
        let lock_store = storage.system_store("docs");

        // a live daemon: fresh heartbeat -> Held, with owner details
        let live = crate::lock::LockInfo {
            owner_uuid: "deadbeef".into(),
            pid: 4242,
            hostname: "probe-host".into(),
            started_at: now_epoch() - 60.0,
            heartbeat_at: now_epoch() - 2.0,
        };
        backends::put_json_overwrite(&*lock_store, &key, &serde_json::to_vec(&live).unwrap())
            .await
            .unwrap();
        let report = gather(&storage_cfg(dir.path())).await.unwrap();
        let row = report.locks.iter().find(|r| r.bucket == "docs").unwrap();
        assert!(matches!(row.probe.state, LockState::Held));
        let d = row.probe.daemon.as_ref().unwrap();
        assert_eq!(d.pid, 4242);
        assert_eq!(d.host, "probe-host");
        assert!(d.started_rfc3339.starts_with("20"), "{}", d.started_rfc3339);
        let text = render_human(&report);
        assert!(text.contains("held by pid 4242"), "{text}");

        // same LOCK ten TTLs later -> Stale
        let mut stale = live.clone();
        stale.heartbeat_at = now_epoch() - 600.0;
        backends::put_json_overwrite(&*lock_store, &key, &serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();
        let report = gather(&storage_cfg(dir.path())).await.unwrap();
        let row = report.locks.iter().find(|r| r.bucket == "docs").unwrap();
        assert!(matches!(row.probe.state, LockState::Stale));
        assert!(render_human(&report).contains("STALE"));
    }

    /// T7: the local ingest block — buckets + source counts + derived
    /// (stamped) indexes, all read from the catalog without loading any
    /// model. A bucket whose derived index is a config-change LEFTOVER shows
    /// up here too (0-row old index visible to the operator).
    #[tokio::test]
    async fn status_local_ingest_block_lists_buckets_counts_and_derived() {
        let dir = tempfile::tempdir().unwrap();
        // a valid pipeline config (fingerprint is pure TOML — no model load)
        let fixture_dir = [
            "vf/tests/fixtures/tiny-embedder",
            "tests/fixtures/tiny-embedder",
        ]
        .iter()
        .map(|p| p.to_string())
        .find(|p| std::path::Path::new(p).join("model.onnx").is_file())
        .expect("tiny-embedder fixture");
        std::fs::create_dir_all(dir.path().join(".vectorfs")).unwrap();
        std::fs::write(
            dir.path().join(".vectorfs/vectorfs.toml"),
            format!(
                "[extractor.plain]\nkind = \"txt\"\n\n\
                 [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {fixture_dir:?}\n\
                 offline = true\n\n\
                 [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
                 chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
                 embedders = [\"tiny\"]\n"
            ),
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.path().join(".vectorfs/vectorfs.toml")).unwrap();
        let pcfg = crate::pipeline::config::load_pipeline_config(&text).unwrap();

        let storage = seed(dir.path()).await;
        // a stamped derived index + a stale marker
        storage
            .create_index(&IndexMeta {
                vector_bucket_name: "docs".into(),
                index_name: "tiny-abcd1234".into(),
                index_arn: "arn".into(),
                creation_time_rfc3339: "t".into(),
                creation_time_epoch: 0.0,
                data_type: "float32".into(),
                dimension: 8,
                distance_metric: "cosine".into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
                x_vf_derived: Some(DerivedIndexInfo {
                    key: "k".repeat(64),
                    embedder: "tiny".into(),
                    model: "tiny-fixture".into(),
                    dim: 8,
                }),
            })
            .await
            .unwrap();
        storage
            .upsert_vectors(
                "docs",
                "tiny-abcd1234",
                8,
                vec![crate::store::VectorRow {
                    key: "k1".into(),
                    vector: vec![1.0; 8],
                    metadata_json: None,
                    filterable: Default::default(),
                }],
            )
            .await
            .unwrap();
        storage
            .save_source(
                "docs",
                &crate::ingest::marker::object_key_hash("docs/a.txt"),
                &crate::store::SourceMarker {
                    object_key: "docs/a.txt".into(),
                    etag_sig: "s".into(),
                    content_hash: "c".into(),
                    pipeline_fingerprint: pcfg.fingerprint("docs").unwrap(),
                    status: crate::store::SourceStatus::Indexed,
                    status_reason: None,
                    retries: 0,
                    retries_exhausted: false,
                    misses: 0,
                    updated_at: "t".into(),
                    chunks: None,
                    engines: None,
                },
            )
            .await
            .unwrap();

        let report = gather(&storage_cfg(dir.path())).await.unwrap();
        assert_eq!(report.ingest.len(), 1);
        let row = &report.ingest[0];
        assert_eq!(row.bucket, "docs");
        assert_eq!(row.fingerprint, pcfg.fingerprint("docs").unwrap());
        assert_eq!(row.sources["INDEXED"], 1);
        // the UNSTAMPED seed indexes (chunks/titles) are NOT derived rows;
        // the stamped one is, with its live vector count
        assert_eq!(row.derived.len(), 1);
        assert_eq!(row.derived[0].index, "tiny-abcd1234");
        assert_eq!(row.derived[0].model, "tiny-fixture");
        assert_eq!(row.derived[0].dim, 8);
        assert_eq!(row.derived[0].vectors, Some(1));
        // human rendering carries the block
        let text = render_human(&report);
        assert!(text.contains("ingest:"), "{text}");
        assert!(text.contains("INDEXED=1 PARTIAL=0"), "{text}");
        assert!(text.contains("tiny-abcd1234"), "{text}");
        // json carries it too
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ingest"][0]["bucket"], "docs");
    }
}
