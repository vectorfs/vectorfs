//! `vf status` (M1 T6, design.md §5.8): read-only inspection of a data root —
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

/// Read the LOCK object and classify it. A LOCK that exists but does not
/// parse is reported as [`LockState::Stale`] debris with no daemon details
/// (the same fail-open takeover rule lock.rs applies to a real writer).
pub async fn probe_lock(backend: &Backend, storage: &Storage) -> Result<LockProbe, String> {
    let key = backend.object_key("LOCK");
    let raw = backends::read_json::<serde_json::Value>(storage.catalog(), &key).await?;
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

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub mode: &'static str,
    /// {"kind": "file"|"memory"|"s3", "spec": "<display form>"}
    pub backend: BackendSummary,
    /// Local data dir (where vf.json / the file root live).
    pub data_root: String,
    pub lock: LockProbe,
    pub buckets: usize,
    pub indexes: usize,
    /// Sum over indexes whose count succeeded.
    pub vectors_total: u64,
    pub index_details: Vec<IndexRow>,
    pub objects: usize,
    pub size_bytes: u64,
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
    // root, and `vf status` must not materialize a data dir that isn't there.
    if cfg.is_file_backend() && !cfg.data_dir.exists() {
        return Err(Failure::new(
            "NO_DATA_ROOT",
            format!("no data root at {}", cfg.data_dir.display()),
            format!("run: vf init --data-dir {}", cfg.data_dir.display()),
        ));
    }
    let backend = cfg.backend().map_err(|e| {
        Failure::new(
            "BACKEND_INVALID",
            e,
            "backend must be file | memory | s3+http(s)://host:port/bucket[/prefix]",
        )
    })?;
    let storage = Storage::new(backend.clone()).map_err(|e| {
        Failure::new(
            "BACKEND_INIT",
            format!("cannot open backend {backend}: {e}"),
            "check the data dir / S3 endpoint and credentials (VF_S3_*, AWS_*)",
        )
    })?;

    let lock = probe_lock(&backend, &storage).await.map_err(|e| {
        Failure::new(
            "LOCK_UNREADABLE",
            format!("cannot read the LOCK object: {e}"),
            "inspect the LOCK object at the data root; if unreadable, remove it while no writer runs",
        )
    })?;

    let mut index_details = Vec::new();
    let mut vectors_total = 0u64;
    let mut bucket_count = 0usize;
    let mut failures = Vec::new();
    for bucket in storage
        .list_buckets()
        .await
        .map_err(store_failure("list buckets"))?
    {
        bucket_count += 1;
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
            "vf: warning: {} dataset(s) could not be counted: {}",
            failures.len(),
            failures.join("; ")
        );
    }

    // Whole-root volume: object sizes from list metadata only. On S3 the
    // store is rooted at the bucket, so the configured prefix scopes it.
    let prefix = match &backend {
        Backend::S3(_) => Some(backend.object_key("")),
        _ => None,
    };
    let (objects, size_bytes) = backends::prefix_stats(storage.catalog(), prefix.as_ref())
        .await
        .map_err(|e| {
            Failure::new(
                "BACKEND_LIST",
                format!("cannot enumerate the data root: {e}"),
                "check backend reachability (vf doctor probes it in depth)",
            )
        })?;

    let kind = match backend {
        Backend::File { .. } => "file",
        Backend::Memory => "memory",
        Backend::S3(_) => "s3",
    };
    Ok(StatusReport {
        schema_version: SCHEMA_VERSION,
        mode: "data-dir",
        backend: BackendSummary {
            kind,
            spec: backend.to_string(),
        },
        data_root: cfg.data_dir.display().to_string(),
        lock,
        buckets: bucket_count,
        indexes: index_details.len(),
        vectors_total,
        index_details,
        objects,
        size_bytes,
    })
}

fn store_failure(what: &'static str) -> impl Fn(crate::store::StoreError) -> Failure {
    move |e| {
        Failure::new(
            "CATALOG_READ",
            format!("cannot {what}: {e}"),
            "check backend health with: vf doctor",
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
    match (&r.lock.state, &r.lock.daemon) {
        (LockState::Free, _) => {
            let _ = writeln!(out, "  lock      : free (no writer holds this data root)");
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
                        "  lock      : {label} by pid {} (owner {}, host {}, since {}, heartbeat {}s ago)",
                        d.pid, d.owner, d.host, d.started_rfc3339, d.heartbeat_age_s
                    );
                    if matches!(state, LockState::Held) {
                        let _ = writeln!(
                            out,
                            "              port unknown — probe with: vf status --endpoint http://127.0.0.1:8787"
                        );
                    } else {
                        let _ = writeln!(
                            out,
                            "              heartbeat older than TTL ({}s) — the next writer takes over",
                            r.lock.ttl_s
                        );
                    }
                }
                None => {
                    let _ = writeln!(
                        out,
                        "  lock      : {label} but unparseable (debris; next writer takes over)"
                    );
                }
            }
        }
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
                "is a vf daemon listening there? try: vf status --data-dir …",
            )
        })?
        .map_err(|e| {
            Failure::new(
                "ENDPOINT_UNREACHABLE",
                format!("GET {url}/healthz failed: {e}"),
                "is `vf serve` running? start it with: vf serve --data-dir …",
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

    use crate::store::{BucketMeta, IndexMeta};

    fn storage_cfg(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            data_dir: dir.to_path_buf(),
            backend_spec: "file".into(),
            s3_region: "us-east-1".into(),
            s3_access_key: None,
            s3_secret_key: None,
        }
    }

    async fn seed(dir: &std::path::Path) -> Arc<Storage> {
        let storage = Storage::file(dir);
        storage
            .create_bucket(&BucketMeta {
                vector_bucket_name: "docs".into(),
                vector_bucket_arn: "arn:aws:s3vectors:us-east-1:0:bucket/docs".into(),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                default_index_mode: None,
            })
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
        assert!(err.hint.contains("vf init"), "{}", err.hint);
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
        assert!(matches!(report.lock.state, LockState::Free));
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
        let backend = storage.backend().clone();
        let key = backend.object_key("LOCK");

        // a live daemon: fresh heartbeat -> Held, with owner details
        let live = crate::lock::LockInfo {
            owner_uuid: "deadbeef".into(),
            pid: 4242,
            hostname: "probe-host".into(),
            started_at: now_epoch() - 60.0,
            heartbeat_at: now_epoch() - 2.0,
        };
        backends::put_json_overwrite(storage.catalog(), &key, &serde_json::to_vec(&live).unwrap())
            .await
            .unwrap();
        let report = gather(&storage_cfg(dir.path())).await.unwrap();
        assert!(matches!(report.lock.state, LockState::Held));
        let d = report.lock.daemon.as_ref().unwrap();
        assert_eq!(d.pid, 4242);
        assert_eq!(d.host, "probe-host");
        assert!(d.started_rfc3339.starts_with("20"), "{}", d.started_rfc3339);
        let text = render_human(&report);
        assert!(text.contains("held by pid 4242"), "{text}");
        assert!(text.contains("--endpoint"), "{text}");

        // same LOCK ten TTLs later -> Stale
        let mut stale = live.clone();
        stale.heartbeat_at = now_epoch() - 600.0;
        backends::put_json_overwrite(
            storage.catalog(),
            &key,
            &serde_json::to_vec(&stale).unwrap(),
        )
        .await
        .unwrap();
        let report = gather(&storage_cfg(dir.path())).await.unwrap();
        assert!(matches!(report.lock.state, LockState::Stale));
        assert!(render_human(&report).contains("STALE"));
    }
}
