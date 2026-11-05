//! Single-writer lock (design.md §5.1): every data root has at most one
//! writer daemon.
//!
//! Two layers:
//! 1. **Local flock** (file backend only): `<root>/.vectorfs/flock`, exclusive
//!    + non-blocking — same-machine double starts fail instantly with a pids
//!    level error. The flock file is separate from the LOCK object because
//!    the LOCK object is rewritten via put/delete, which swaps the inode out
//!    from under an flock held on it.
//! 2. **LOCK object** `<root>/.vectorfs/LOCK` (all backends): JSON
//!    `{owner_uuid, pid, hostname, started_at, heartbeat_at}`.
//!
//! Acquire: not present -> conditional create (`PutMode::Create`, the same
//! race-free primitive as catalog creations). Present with a heartbeat newer
//! than TTL -> refuse (print occupant). Present with a heartbeat older than
//! TTL -> takeover:
//!   - memory / S3: `PutMode::Update(etag-of-what-we-read)` — a true CAS
//!     (If-Match); if another daemon took over in between, our write fails
//!     with Precondition and we re-evaluate.
//!   - file: LocalFileSystem does not implement `PutMode::Update`, so we use
//!     delete + conditional-create. RACE ARGUMENT: on one machine the flock
//!     already serializes every contender (the delete/create dance cannot
//!     interleave with another local process). Across machines sharing the
//!     directory, two simultaneous takers could in the worst case delete each
//!     other's fresh LOCK and both end up "owning" it — accepted: (a) the
//!     primary defense is the heartbeat TTL (a takeover only happens after
//!     the old owner was silent for a full TTL), (b) the loser's next
//!     heartbeat re-reads the LOCK, notices a foreign owner_uuid, and shuts
//!     itself down, so the window of true double-ownership is bounded by one
//!     heartbeat interval (TTL/3) and requires an already-dead predecessor.
//!
//! Heartbeat: every TTL/3, read -> verify owner_uuid is still ours -> rewrite
//! heartbeat_at (CAS on memory/S3, overwrite on file where flock decides).
//! Losing the ownership check or the CAS means we were taken over: the daemon
//! must stop writing — the caller wires this to graceful shutdown.
//!
//! Release (ctrl-c / SIGTERM / graceful shutdown): best-effort delete, only
//! when the LOCK still names us (never delete a successor's lock).

use std::sync::Arc;
use std::time::Duration;

use crate::backends::{self, Backend};

/// LOCK freshness window. A daemon whose heartbeat is older than this is
/// considered dead and may be taken over.
pub const LOCK_TTL: Duration = Duration::from_secs(60);
/// Heartbeat cadence (TTL/3 by design.md §5.1).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct LockInfo {
    pub owner_uuid: String,
    pub pid: u32,
    pub hostname: String,
    /// epoch seconds
    pub started_at: f64,
    /// epoch seconds of the last heartbeat
    pub heartbeat_at: f64,
}

impl LockInfo {
    /// Age of the last heartbeat (saturating on clock skew backwards).
    pub fn heartbeat_age(&self, now: f64) -> Duration {
        Duration::from_secs_f64((now - self.heartbeat_at).max(0.0))
    }

    pub fn describe(&self) -> String {
        format!(
            "owner={} pid={} host={} started={} heartbeat={}s ago",
            self.owner_uuid,
            self.pid,
            self.hostname,
            self.started_at,
            self.heartbeat_age(now_epoch()).as_secs()
        )
    }
}

#[derive(Debug)]
pub enum LockError {
    /// LOCK exists with a fresh heartbeat (or the local flock is held).
    Held(String),
    /// Storage / OS failure while acquiring.
    Backend(String),
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 128-bit random owner id (hex) — identifies THIS daemon instance across
/// takeovers (release/heartbeat must only act on our own LOCK).
fn new_owner_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS randomness unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Held local flock. Dropping releases it (close(2) semantics) — the field is
/// never read on purpose, holding the fd IS the lock.
struct FlockGuard {
    _file: std::fs::File,
}

/// Take an exclusive non-blocking flock; `Err(Held)` if another process on
/// this machine holds it.
fn take_flock(path: &std::path::Path) -> Result<FlockGuard, LockError> {
    use std::os::unix::io::AsFd as _;
    // The flock lives inside `.vectorfs/` now; open(2) creates files, not
    // directories — materialize the system dir before the first-ever lock.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LockError::Backend(format!("create {}: {e}", parent.display())))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| LockError::Backend(format!("open flock {}: {e}", path.display())))?;
    match rustix::fs::flock(
        file.as_fd(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    ) {
        Ok(()) => Ok(FlockGuard { _file: file }),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(LockError::Held(format!(
            "another vectorfs daemon on this machine holds the local lock {} (flock)",
            path.display()
        ))),
        Err(e) => Err(LockError::Backend(format!("flock {}: {e}", path.display()))),
    }
}

/// The held writer lock: LOCK object (+ local flock on the file backend).
pub struct LockGuard {
    store: Arc<dyn object_store::ObjectStore>,
    key: object_store::path::Path,
    info: LockInfo,
    /// memory/S3 support `PutMode::Update`; file does not (see module doc).
    cas_capable: bool,
    /// Kept alive for the daemon lifetime; Drop releases the flock.
    _flock: Option<FlockGuard>,
}

impl LockGuard {
    pub fn info(&self) -> &LockInfo {
        &self.info
    }

    /// Refresh heartbeat_at. `Ok(false)` = we no longer own the LOCK (taken
    /// over after our TTL expired) — the caller must stop the daemon.
    pub async fn heartbeat(&mut self) -> Result<bool, String> {
        match backends::get_with_etag(&*self.store, &self.key).await? {
            None => Ok(false), // deleted from under us
            Some((bytes, etag)) => {
                let current: LockInfo = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(_) => return Ok(false), // not our (readable) lock anymore
                };
                if current.owner_uuid != self.info.owner_uuid {
                    return Ok(false);
                }
                self.info.started_at = current.started_at; // keep original start
                self.info.heartbeat_at = now_epoch();
                let json =
                    serde_json::to_vec(&self.info).map_err(|e| format!("serialize LOCK: {e}"))?;
                if self.cas_capable {
                    backends::put_json_cas(&*self.store, &self.key, &json, &etag).await
                } else {
                    // file backend: unconditional overwrite is safe here — the
                    // flock means nobody else on this machine can be holding
                    // the LOCK, and ownership was just verified above.
                    backends::put_json_overwrite(&*self.store, &self.key, &json)
                        .await
                        .map(|_| true)
                }
            }
        }
    }

    /// Best-effort release on shutdown: delete the LOCK only while it still
    /// names us (never delete a successor's).
    pub async fn release(&self) {
        match backends::read_json::<LockInfo>(&*self.store, &self.key).await {
            Ok(Some(current)) if current.owner_uuid == self.info.owner_uuid => {
                let _ = backends::delete_object(&*self.store, &self.key).await;
            }
            _ => {}
        }
    }
}

/// Acquire the writer lock with the default TTL (60s).
pub async fn acquire(
    backend: &Backend,
    store: Arc<dyn object_store::ObjectStore>,
) -> Result<LockGuard, LockError> {
    acquire_with_ttl(backend, store, LOCK_TTL).await
}

/// Acquire with an explicit TTL (tests use short windows).
pub async fn acquire_with_ttl(
    backend: &Backend,
    store: Arc<dyn object_store::ObjectStore>,
    ttl: Duration,
) -> Result<LockGuard, LockError> {
    let flock = match backend.flock_path() {
        // file backend: double protection — flock + LOCK object (design §5.1).
        Some(path) => Some(take_flock(&path)?),
        None => None, // memory/S3 skip the local flock, the LOCK object is the lock
    };
    let key = backend.object_key("LOCK");
    let info = LockInfo {
        owner_uuid: new_owner_uuid(),
        pid: std::process::id(),
        hostname: hostname(),
        started_at: now_epoch(),
        heartbeat_at: now_epoch(),
    };
    let cas_capable = !matches!(backend, Backend::File { .. });
    let json = serde_json::to_vec(&info).map_err(|e| LockError::Backend(e.to_string()))?;

    // Bounded retry loop: each iteration re-reads, so races against other
    // contenders converge (3 rounds is far beyond anything realistic).
    for _ in 0..3 {
        match backends::get_with_etag(&*store, &key).await {
            Ok(None) => {
                if backends::put_json_create(&*store, &key, &json)
                    .await
                    .map_err(LockError::Backend)?
                {
                    return Ok(LockGuard {
                        store,
                        key,
                        info,
                        cas_capable,
                        _flock: flock,
                    });
                }
                // raced: someone created between our read and create — re-read
            }
            Ok(Some((bytes, etag))) => {
                // Unparseable LOCK: a crashed/partial write can never beat
                // again — treat as expired debris and take over.
                let current: LockInfo = serde_json::from_slice(&bytes)
                    .map_err(|_| LockError::Backend("corrupt LOCK object (unparseable)".into()))?;
                if current.heartbeat_age(now_epoch()) < ttl {
                    return Err(LockError::Held(current.describe()));
                }
                // expired -> takeover
                let taken = if cas_capable {
                    backends::put_json_cas(&*store, &key, &json, &etag)
                        .await
                        .map_err(LockError::Backend)?
                } else {
                    // file: delete + conditional create (see module doc for the
                    // race argument; flock serializes same-machine contenders).
                    backends::delete_object(&*store, &key)
                        .await
                        .map_err(LockError::Backend)?;
                    backends::put_json_create(&*store, &key, &json)
                        .await
                        .map_err(LockError::Backend)?
                };
                if taken {
                    return Ok(LockGuard {
                        store,
                        key,
                        info,
                        cas_capable,
                        _flock: flock,
                    });
                }
                // lost the takeover race — re-read and re-evaluate
            }
            Err(e) => return Err(LockError::Backend(e)),
        }
    }
    Err(LockError::Backend(
        "LOCK acquisition did not converge after 3 rounds".into(),
    ))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_file(dir: &std::path::Path) -> crate::store::Storage {
        crate::store::Storage::file(dir)
    }

    #[tokio::test]
    async fn lock_second_acquire_rejected_while_alive() {
        let dir = tempfile::tempdir().unwrap();
        let s1 = storage_file(dir.path());
        let mut g1 = acquire(s1.backend(), s1.catalog().clone()).await.unwrap();

        // second daemon, same data dir: refused by BOTH layers — flock first
        let s2 = storage_file(dir.path());
        match acquire(s2.backend(), s2.catalog().clone()).await {
            Err(LockError::Held(msg)) => assert!(msg.contains("flock"), "{msg}"),
            Err(other) => panic!("expected Held, got {other:?}"),
            Ok(_) => panic!("expected Held, got a lock"),
        }

        // heartbeat keeps us alive and reports ownership
        assert!(g1.heartbeat().await.unwrap());
        assert!(g1.info().heartbeat_age(now_epoch()) < Duration::from_secs(5));

        // graceful release frees the lock for the next daemon: release()
        // removes the LOCK object, dropping the guard releases the flock
        g1.release().await;
        drop(g1);
        let _g2 = acquire(s2.backend(), s2.catalog().clone())
            .await
            .expect("re-acquire after release");
    }

    #[tokio::test]
    async fn lock_expired_heartbeat_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let s = storage_file(dir.path());
        let backend = s.backend().clone();
        let key = backend.object_key("LOCK");

        // a daemon that died silently: LOCK left behind, heartbeat 10x TTL old
        let stale = LockInfo {
            owner_uuid: new_owner_uuid(),
            pid: 4242,
            hostname: "dead-host".into(),
            started_at: now_epoch() - 1000.0,
            heartbeat_at: now_epoch() - 1000.0,
        };
        backends::put_json_overwrite(s.catalog(), &key, &serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();

        let mut g = acquire(&backend, s.catalog().clone())
            .await
            .expect("takeover");
        assert_ne!(g.info().owner_uuid, stale.owner_uuid);
        assert_eq!(g.info().pid, std::process::id());
        assert!(g.heartbeat().await.unwrap());

        // memory backend: same protocol, CAS takeover
        let sm = crate::store::Storage::memory();
        let bm = sm.backend().clone();
        let km = bm.object_key("LOCK");
        let mut very_stale = stale.clone();
        very_stale.heartbeat_at = now_epoch() - 3600.0;
        backends::put_json_overwrite(sm.catalog(), &km, &serde_json::to_vec(&very_stale).unwrap())
            .await
            .unwrap();
        let mut gm = acquire(&bm, sm.catalog().clone())
            .await
            .expect("memory takeover");
        assert!(gm.heartbeat().await.unwrap());
        gm.release().await;
    }

    #[tokio::test]
    async fn lock_live_heartbeat_rejects_with_occupant_info() {
        // memory backend (no flock) so the LOCK object is the ONLY guard here
        let s = crate::store::Storage::memory();
        let backend = s.backend().clone();
        let g = acquire(&backend, s.catalog().clone()).await.unwrap();

        match acquire(&backend, s.catalog().clone()).await {
            Err(LockError::Held(msg)) => {
                assert!(msg.contains("pid="), "{msg}");
                assert!(msg.contains(g.info().owner_uuid.as_str()), "{msg}");
            }
            Err(other) => panic!("expected Held, got {other:?}"),
            Ok(_) => panic!("expected Held, got a lock"),
        }
        g.release().await;
    }

    /// Concurrent creation of the same bucket: exactly one conditional write
    /// wins (the primitive LOCK and the catalog both rest on).
    #[tokio::test]
    async fn concurrent_bucket_create_exactly_one_wins() {
        let s = crate::store::Storage::memory();
        let key = s.backend().object_key("buckets/race/bucket.json");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = s.catalog().clone();
                let key = key.clone();
                tokio::spawn(async move {
                    backends::put_json_create(&*store, &key, b"{\"winner\":true}")
                        .await
                        .unwrap()
                })
            })
            .collect();
        let results: Vec<bool> = futures_util::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|w| **w).count(),
            1,
            "exactly one creator wins: {results:?}"
        );
    }
}
