//! `vectorfs doctor` (M1 T6, design.md §5.8): environment diagnostics for one data
//! root. Every check reports PASS / WARN / FAIL, and every non-PASS carries
//! an **executable** hint ("run: vectorfs init …", "restart the writer …") per the
//! agent-first output contract (report.rs). Any FAIL exits 1.
//!
//! Checks (in run order):
//! 1. `backend-write` — data root writable (file) or S3 reachable + the
//!    conditional-write probe the whole catalog rests on: `PutMode::Create`
//!    of a uniquely-named probe object, then delete. On the file backend
//!    `PutMode::Create` is O_EXCL — the exact primitive bucket/index/LOCK
//!    creation uses; on S3 it is If-None-Match:* (design.md §5.2).
//! 2. `config` — `<data-dir>/.vectorfs/vectorfs.toml` exists and its
//!    credential fields are complete.
//! 3. `lock` — who holds the LOCK (live daemon), stale debris, or nobody.
//!    A live holder is healthy (it IS the writer); stale debris is a WARN —
//!    the next writer takes over automatically after the TTL.
//! 4. `indexes` — every index's Lance dataset opens and `count_rows` works.
//! 5. `disk-free` — statvfs(2) on the data root (file backend); < 1 GiB
//!    warns. Not applicable to s3/memory (reported as such).
//!
//! A MISSING data root (file backend) is not a check row: it short-circuits
//! to `{code: NO_DATA_ROOT, hint: run: vectorfs init …}` + exit 1 — there is
//! nothing else to probe.

use serde::Serialize;

use crate::config::StorageConfig;
use crate::report::{Failure, SCHEMA_VERSION, human_bytes};
use crate::status::{LockState, probe_lock};
use crate::store::Storage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    /// Executable action (always set for warn/fail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            status: CheckStatus::Pass,
            detail: detail.into(),
            hint: None,
        }
    }
    fn not_pass(
        name: &'static str,
        status: CheckStatus,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Check {
            name,
            status,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    /// fail > warn > pass; matches the exit code decision (fail -> 1).
    pub overall: CheckStatus,
    pub data_root: String,
    pub backend: String,
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn any_fail(&self) -> bool {
        self.overall == CheckStatus::Fail
    }
}

/// Run all checks. `Err` is reserved for conditions that make checking
/// impossible at all (missing data root, unusable backend).
pub async fn run(cfg: &StorageConfig) -> Result<DoctorReport, Failure> {
    // Existence BEFORE Backend::parse (which would create the dir) — and
    // per the task contract this is the one early-exit case.
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

    let mut checks = Vec::new();
    checks.push(check_backend_write(&backend, &storage).await);
    checks.push(check_config(cfg).await);
    checks.push(check_lock(&backend, &storage).await);
    checks.push(check_indexes(&storage).await);
    checks.push(check_disk_free(&backend, cfg));

    let overall = if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        CheckStatus::Fail
    } else if checks.iter().any(|c| c.status == CheckStatus::Warn) {
        CheckStatus::Warn
    } else {
        CheckStatus::Pass
    };
    Ok(DoctorReport {
        schema_version: SCHEMA_VERSION,
        overall,
        data_root: cfg.data_dir.display().to_string(),
        backend: backend.to_string(),
        checks,
    })
}

/// 8 hex chars of OS randomness for the probe object name (getrandom is
/// already a dependency — same source as the LOCK owner id).
fn probe_suffix() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("OS randomness unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn check_backend_write(backend: &crate::backends::Backend, storage: &Storage) -> Check {
    let key = backend.object_key(&format!(".vf-doctor-probe-{}", probe_suffix()));
    let create = crate::backends::put_json_create(storage.catalog(), &key, b"probe");
    match create.await {
        Ok(true) => {
            if let Err(e) = crate::backends::delete_object(storage.catalog(), &key).await {
                return Check::not_pass(
                    "backend-write",
                    CheckStatus::Warn,
                    format!("probe object created but not deletable: {e}"),
                    "remove the .vectorfs/.vf-doctor-probe-* object at the data root manually",
                );
            }
            let what = match backend {
                crate::backends::Backend::File { .. } => {
                    "file backend: conditional create (O_EXCL) + delete of the probe object"
                }
                crate::backends::Backend::Memory => {
                    "memory backend: conditional create + delete of the probe object"
                }
                crate::backends::Backend::S3(_) => {
                    "s3 backend: reachable, conditional create (If-None-Match:*) + delete of the probe object"
                }
            };
            Check::pass("backend-write", what)
        }
        Ok(false) => Check::not_pass(
            "backend-write",
            CheckStatus::Fail,
            "conditional create reported the probe object as already existing (impossible for a fresh random name)",
            "retry vectorfs doctor; if it persists the backend's conditional writes are broken",
        ),
        Err(e) => {
            let hint = match backend {
                crate::backends::Backend::S3(_) => {
                    "check the endpoint/credentials (VF_S3_*, AWS_*) and that the bucket exists"
                        .to_string()
                }
                _ => format!(
                    "check permissions on the data root: chmod u+w {}",
                    cfg_dir_of(backend).display()
                ),
            };
            Check::not_pass(
                "backend-write",
                CheckStatus::Fail,
                format!("probe write failed: {e}"),
                hint,
            )
        }
    }
}

fn cfg_dir_of(backend: &crate::backends::Backend) -> std::path::PathBuf {
    match backend {
        crate::backends::Backend::File { root } => root.clone(),
        _ => std::path::PathBuf::from("."),
    }
}

async fn check_config(cfg: &StorageConfig) -> Check {
    match crate::config::read_config(&cfg.data_dir) {
        Err(e) => Check::not_pass(
            "config",
            CheckStatus::Fail,
            format!("vectorfs.toml unreadable: {e}"),
            format!(
                "fix or regenerate it (regenerating rotates credentials): mv {} /tmp && vectorfs init --data-dir {}",
                crate::config::config_path(&cfg.data_dir).display(),
                cfg.data_dir.display()
            ),
        ),
        Ok(None) => Check::not_pass(
            "config",
            CheckStatus::Fail,
            format!(
                "no {} found",
                crate::config::config_path(&cfg.data_dir).display()
            ),
            format!("run: vectorfs init --data-dir {}", cfg.data_dir.display()),
        ),
        Ok(Some(c)) => {
            if c.access_key_id.is_empty() || c.secret_access_key.is_empty() {
                return Check::not_pass(
                    "config",
                    CheckStatus::Fail,
                    "vectorfs.toml has empty access_key_id / secret_access_key".to_string(),
                    format!(
                        "regenerate credentials: vectorfs init --force --data-dir {} (rotates the key)",
                        cfg.data_dir.display()
                    ),
                );
            }
            let masked = if c.access_key_id.len() > 8 {
                format!(
                    "{}…{}",
                    &c.access_key_id[..4],
                    &c.access_key_id[c.access_key_id.len() - 4..]
                )
            } else {
                c.access_key_id.clone()
            };
            Check::pass(
                "config",
                format!(
                    "vectorfs.toml present (region {}, account {}, access key {masked})",
                    c.region, c.account_id
                ),
            )
        }
    }
}

async fn check_lock(backend: &crate::backends::Backend, storage: &Storage) -> Check {
    match probe_lock(backend, storage).await {
        Err(e) => Check::not_pass(
            "lock",
            CheckStatus::Fail,
            format!("cannot read the LOCK object: {e}"),
            "remove the LOCK object at the data root while no writer runs",
        ),
        Ok(probe) => match (&probe.state, &probe.daemon) {
            (LockState::Free, _) => Check::pass("lock", "no writer holds this data root"),
            (LockState::Held, Some(d)) => Check::pass(
                "lock",
                format!(
                    "held by a live daemon (pid {}, host {}, heartbeat {}s ago < TTL {}s)",
                    d.pid, d.host, d.heartbeat_age_s, probe.ttl_s
                ),
            ),
            (LockState::Stale, Some(d)) => Check::not_pass(
                "lock",
                CheckStatus::Warn,
                format!(
                    "stale LOCK: heartbeat {}s old (owner pid {}, host {}) — its writer is gone",
                    d.heartbeat_age_s, d.pid, d.host
                ),
                "restart takes over after TTL automatically: vectorfs serve / vectorfs mcp (or remove the LOCK object while nothing runs)",
            ),
            (_, None) => Check::not_pass(
                "lock",
                CheckStatus::Warn,
                "LOCK object present but unparseable (crashed-write debris)",
                "the next writer takes over automatically; to clean up now, remove the LOCK object while nothing runs",
            ),
        },
    }
}

async fn check_indexes(storage: &Storage) -> Check {
    let buckets = match storage.list_buckets().await {
        Ok(b) => b,
        Err(e) => {
            return Check::not_pass(
                "indexes",
                CheckStatus::Fail,
                format!("cannot enumerate buckets: {e}"),
                "check backend health (vectorfs doctor's backend-write row)",
            );
        }
    };
    let mut total = 0usize;
    let mut vectors = 0u64;
    let mut broken: Vec<String> = Vec::new();
    let mut examined = 0usize;
    for bucket in &buckets {
        let indexes = match storage.list_indexes(&bucket.vector_bucket_name).await {
            Ok(ix) => ix,
            Err(e) => {
                broken.push(format!("{}/: {e}", bucket.vector_bucket_name));
                continue;
            }
        };
        for meta in &indexes {
            examined += 1;
            match storage
                .count_vectors(&meta.vector_bucket_name, &meta.index_name)
                .await
            {
                Ok(n) => {
                    total += 1;
                    vectors += n as u64;
                }
                Err(e) => broken.push(format!(
                    "{}/{}: {e}",
                    meta.vector_bucket_name, meta.index_name
                )),
            }
        }
    }
    if !broken.is_empty() {
        return Check::not_pass(
            "indexes",
            CheckStatus::Fail,
            format!(
                "{}/{} datasets open; failing: {}",
                total,
                total + broken.len(),
                broken.join("; ")
            ),
            "inspect the failing dataset(s); if corrupt, delete and recreate the index (DeleteIndex + CreateIndex via the SDK)",
        );
    }
    if examined == 0 {
        return Check::pass("indexes", "no indexes yet (none to check)");
    }
    Check::pass(
        "indexes",
        format!("{total}/{examined} datasets open, {vectors} vectors total"),
    )
}

/// 1 GiB warning threshold for the file backend's free space.
const DISK_WARN: u64 = 1024 * 1024 * 1024;

fn check_disk_free(backend: &crate::backends::Backend, cfg: &StorageConfig) -> Check {
    match backend {
        crate::backends::Backend::File { root } => match rustix::fs::statvfs(root) {
            Ok(st) => {
                let avail = st.f_bavail * st.f_frsize;
                if avail < DISK_WARN {
                    Check::not_pass(
                        "disk-free",
                        CheckStatus::Warn,
                        format!(
                            "only {} available at {}",
                            human_bytes(avail),
                            root.display()
                        ),
                        format!(
                            "free space or move the data root (current: {})",
                            cfg.data_dir.display()
                        ),
                    )
                } else {
                    Check::pass(
                        "disk-free",
                        format!("{} available at {}", human_bytes(avail), root.display()),
                    )
                }
            }
            Err(e) => Check::not_pass(
                "disk-free",
                CheckStatus::Fail,
                format!("statvfs({}) failed: {e}", root.display()),
                "check the mount; vf needs a readable+writable filesystem",
            ),
        },
        _ => Check::pass("disk-free", "n/a (remote/volatile backend)"),
    }
}

/// Human rendering (stdout).
pub fn render_human(r: &DoctorReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "vectorfs doctor (schema v{}) — data root {}, backend {}",
        r.schema_version, r.data_root, r.backend
    );
    for c in &r.checks {
        let tag = match c.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        let _ = writeln!(out, "  [{tag}] {}: {}", c.name, c.detail);
        if let Some(hint) = &c.hint {
            let _ = writeln!(out, "         hint: {hint}");
        }
    }
    let overall = match r.overall {
        CheckStatus::Pass => "PASS",
        CheckStatus::Warn => "WARN",
        CheckStatus::Fail => "FAIL",
    };
    let _ = writeln!(out, "overall: {overall}");
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{BucketMeta, IndexMeta};

    fn cfg(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            data_dir: dir.to_path_buf(),
            backend_spec: "file".into(),
            s3_region: "us-east-1".into(),
            s3_access_key: None,
            s3_secret_key: None,
        }
    }

    #[tokio::test]
    async fn no_data_root_short_circuits_with_init_hint() {
        let err = run(&cfg(std::path::Path::new("/nonexistent/vf-doctor-tmp")))
            .await
            .unwrap_err();
        assert_eq!(err.code, "NO_DATA_ROOT");
        assert!(err.hint.contains("vectorfs init"), "{}", err.hint);
    }

    #[tokio::test]
    async fn healthy_data_root_all_pass() {
        let dir = tempfile::tempdir().unwrap();
        crate::config::init(dir.path(), false).unwrap();
        let storage = Storage::file(dir.path());
        storage
            .create_bucket(&BucketMeta {
                vector_bucket_name: "vb".into(),
                vector_bucket_arn: "arn:aws:s3vectors:us-east-1:0:bucket/vb".into(),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                default_index_mode: None,
            })
            .await
            .unwrap();
        storage
            .create_index(&IndexMeta {
                vector_bucket_name: "vb".into(),
                index_name: "ix".into(),
                index_arn: "arn:aws:s3vectors:us-east-1:0:bucket/vb/index/ix".into(),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                data_type: "float32".into(),
                dimension: 2,
                distance_metric: "euclidean".into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
            })
            .await
            .unwrap();

        let report = run(&cfg(dir.path())).await.unwrap();
        for c in &report.checks {
            assert_eq!(c.status, CheckStatus::Pass, "{c:?}");
        }
        assert_eq!(report.overall, CheckStatus::Pass);
        assert!(!report.any_fail());
        let text = render_human(&report);
        assert!(text.contains("[PASS] config"), "{text}");
        assert!(text.contains("1/1 datasets open"), "{text}");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert_eq!(json["overall"], "pass");
    }

    #[tokio::test]
    async fn missing_config_fails_with_run_init_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap(); // data root, no vectorfs.toml
        let report = run(&cfg(dir.path())).await.unwrap();
        assert!(report.any_fail());
        let config = report.checks.iter().find(|c| c.name == "config").unwrap();
        assert_eq!(config.status, CheckStatus::Fail);
        assert!(
            config.hint.as_deref().unwrap().contains("vectorfs init"),
            "{config:?}"
        );
        let text = render_human(&report);
        assert!(text.contains("[FAIL] config"), "{text}");
        assert!(text.contains("hint: run: vectorfs init"), "{text}");
        assert!(text.contains("overall: FAIL"), "{text}");
    }

    #[tokio::test]
    async fn stale_lock_warns_and_live_lock_passes() {
        let dir = tempfile::tempdir().unwrap();
        crate::config::init(dir.path(), false).unwrap();
        let storage = Storage::file(dir.path());
        let backend = storage.backend().clone();
        let key = backend.object_key("LOCK");

        let stale = crate::lock::LockInfo {
            owner_uuid: "cafe".into(),
            pid: 999,
            hostname: "gone".into(),
            started_at: 0.0,
            heartbeat_at: 500.0, // epoch — ancient
        };
        crate::backends::put_json_overwrite(
            storage.catalog(),
            &key,
            &serde_json::to_vec(&stale).unwrap(),
        )
        .await
        .unwrap();
        let report = run(&cfg(dir.path())).await.unwrap();
        assert!(!report.any_fail(), "{report:?}"); // stale is a WARN, not a FAIL
        assert_eq!(report.overall, CheckStatus::Warn);
        let lock = report.checks.iter().find(|c| c.name == "lock").unwrap();
        assert_eq!(lock.status, CheckStatus::Warn);
        assert!(
            lock.hint.as_deref().unwrap().contains("takes over"),
            "{lock:?}"
        );

        // a fresh heartbeat flips it to PASS
        let mut live = stale;
        live.heartbeat_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        crate::backends::put_json_overwrite(
            storage.catalog(),
            &key,
            &serde_json::to_vec(&live).unwrap(),
        )
        .await
        .unwrap();
        let report = run(&cfg(dir.path())).await.unwrap();
        let lock = report.checks.iter().find(|c| c.name == "lock").unwrap();
        assert_eq!(lock.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn broken_index_dataset_fails_with_recreate_hint() {
        let dir = tempfile::tempdir().unwrap();
        crate::config::init(dir.path(), false).unwrap();
        let storage = Storage::file(dir.path());
        storage
            .create_bucket(&BucketMeta {
                vector_bucket_name: "vb".into(),
                vector_bucket_arn: "arn:aws:s3vectors:us-east-1:0:bucket/vb".into(),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                default_index_mode: None,
            })
            .await
            .unwrap();
        storage
            .create_index(&IndexMeta {
                vector_bucket_name: "vb".into(),
                index_name: "ix".into(),
                index_arn: "arn:aws:s3vectors:us-east-1:0:bucket/vb/index/ix".into(),
                creation_time_rfc3339: "2026-11-04T00:00:00.000Z".into(),
                creation_time_epoch: 0.0,
                data_type: "float32".into(),
                dimension: 2,
                distance_metric: "euclidean".into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
            })
            .await
            .unwrap();
        // corrupt the dataset: wipe the lance dataset dir, keep index.json
        // (lance keeps the table under data/<table>/ — remove the whole data
        // path so reopening the index cannot succeed)
        let data = dir.path().join(".vectorfs/buckets/vb/indexes/ix/data");
        std::fs::remove_dir_all(&data).unwrap();
        std::fs::write(&data, b"not a dataset").unwrap();

        let report = run(&cfg(dir.path())).await.unwrap();
        assert!(report.any_fail());
        let indexes = report.checks.iter().find(|c| c.name == "indexes").unwrap();
        assert_eq!(indexes.status, CheckStatus::Fail);
        assert!(
            indexes.hint.as_deref().unwrap().contains("DeleteIndex"),
            "{indexes:?}"
        );
    }
}
