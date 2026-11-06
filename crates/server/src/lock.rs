//! Per-bucket single-writer lock (design-v3.md §5.1/D6): every vector
//! bucket `X` has at most one writer, guarded by the LOCK object at
//! `X-vf/LOCK`. The v2.2 daemon-level LOCK is retired — losing one bucket's
//! lock parks ONLY that bucket.
//!
//! Two layers:
//! 1. **Local flock** (file backend only): `<data-dir>/.vectorfs/flock`,
//!    exclusive + non-blocking — same-machine double starts fail instantly
//!    with a pids level error. It guards the daemon's LOCAL home (one
//!    flock per daemon process, taken once at serve/mcp startup — NOT per
//!    bucket). The flock file is separate from the LOCK object because
//!    the LOCK object is rewritten via put/delete, which swaps the inode
//!    out from under an flock held on it.
//! 2. **LOCK object `X-vf/LOCK`** (all backends, one per active vb): JSON
//!    `{owner_uuid, pid, hostname, started_at, heartbeat_at}`. The writer
//!    id (daemon instance UUID) is the INCUMBENT marker only — ownership
//!    of buckets is the deployment identity on `bucket.json` (R2/D8).
//!
//! Acquire (per bucket): not present -> conditional create (`PutMode::
//! Create`, the same race-free primitive as catalog creations). Present
//! with a heartbeat newer than TTL -> refuse (report occupant). Present
//! with a heartbeat older than TTL -> takeover:
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
//!     heartbeat re-reads the LOCK, notices a foreign owner_uuid, and parks
//!     that bucket, so the window of true double-ownership is bounded by one
//!     heartbeat interval (TTL/3) and requires an already-dead predecessor.
//!
//! Heartbeat: every TTL/3, in PARALLEL across guards (a serial 100-bucket
//! walk at ~150ms/request would eat the whole interval); a lost/foreign
//! LOCK parks exactly that bucket (its write paths 409, everything else
//! keeps serving).
//!
//! Idle release (§5.1): protocol-face locks are LAZY — first touch of a vb
//! acquires it, and a guard with no activity for a full TTL is released so
//! a pure gateway daemon cannot pin buckets forever. Long-hold is reserved
//! for `[ingest.X]` buckets (pinned at startup).
//!
//! Release (ctrl-c / SIGTERM / graceful shutdown): best-effort delete, only
//! when the LOCK still names us (never delete a successor's).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::backends::{self, Backend};
use crate::store::Storage;

/// LOCK freshness window. A daemon whose heartbeat is older than this is
/// considered dead and may be taken over.
pub const LOCK_TTL: Duration = Duration::from_secs(60);
/// Heartbeat cadence (TTL/3 by design-v3.md §5.1).
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
    /// LOCK exists with a fresh heartbeat, the bucket is parked (its lock
    /// was lost), or the local flock is held.
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

/// 128-bit random writer id (hex) — identifies THIS daemon instance across
/// takeovers (release/heartbeat must only act on our own LOCK). Deliberately
/// NOT the ownership identity (R2/D8): a redeploy gets a fresh writer id and
/// still owns its deployment's buckets.
fn new_owner_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS randomness unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Held local flock. Dropping releases it (close(2) semantics) — the field is
/// never read on purpose, holding the fd IS the lock.
pub struct FlockGuard {
    _file: std::fs::File,
}

/// Take the daemon-home flock (file backend only — same-machine double-start
/// guard). serve / mcp call this ONCE at startup; in-process test fixtures do
/// not (the flock guards distinct processes, not distinct Storage handles).
pub fn take_flock(backend: &Backend) -> Result<FlockGuard, LockError> {
    let Some(path) = backend.flock_path() else {
        // memory/S3 skip the local flock: the LOCK objects are the locks
        return Err(LockError::Backend(
            "take_flock: backend has no local home (memory/s3)".into(),
        ));
    };
    take_flock_at(&path)
}

fn take_flock_at(path: &std::path::Path) -> Result<FlockGuard, LockError> {
    use std::os::unix::io::AsFd as _;
    // The flock lives inside the daemon home `<data-dir>/.vectorfs/`;
    // open(2) creates files, not directories — materialize the home dir
    // before the first-ever lock.
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

/// The held writer lock for ONE bucket: LOCK object in `X-vf/`.
pub struct LockGuard {
    store: Arc<dyn object_store::ObjectStore>,
    key: object_store::path::Path,
    info: LockInfo,
    /// memory/S3 support `PutMode::Update`; file does not (see module doc).
    cas_capable: bool,
}

impl LockGuard {
    pub fn info(&self) -> &LockInfo {
        &self.info
    }

    /// Refresh heartbeat_at. `Ok(false)` = we no longer own the LOCK (taken
    /// over after our TTL expired) — the caller must park this bucket.
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

/// Acquire one bucket's LOCK with the default TTL (60s). The store is the
/// bucket's SYSTEM store; the key is the bucket-relative `LOCK`.
async fn acquire_guard(
    store: Arc<dyn object_store::ObjectStore>,
    key: object_store::path::Path,
    writer_id: &str,
    cas_capable: bool,
) -> Result<LockGuard, LockError> {
    let info = LockInfo {
        owner_uuid: writer_id.to_string(),
        pid: std::process::id(),
        hostname: hostname(),
        started_at: now_epoch(),
        heartbeat_at: now_epoch(),
    };
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
                    });
                }
                // raced: someone created between our read and create — re-read
            }
            Ok(Some((bytes, etag))) => {
                // Unparseable LOCK: a crashed/partial write can never beat
                // again — treat as expired debris and take over.
                let current: LockInfo = serde_json::from_slice(&bytes)
                    .map_err(|_| LockError::Backend("corrupt LOCK object (unparseable)".into()))?;
                if current.heartbeat_age(now_epoch()) < LOCK_TTL {
                    if current.owner_uuid == writer_id {
                        // OURS: a concurrent ensure_held of the SAME manager
                        // won the create while this task was also probing
                        // (two first-touch writes racing on one bucket).
                        // Return a VIEW guard of the winner's LOCK — the
                        // registry insert then keeps exactly one owner, and
                        // the loser's guard is dropped untouched.
                        return Ok(LockGuard {
                            store,
                            key,
                            info: current,
                            cas_capable,
                        });
                    }
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
// LockManager: the daemon-wide registry of per-bucket guards
// ---------------------------------------------------------------------------

/// Registry + heartbeat bookkeeping for every bucket lock this daemon holds
/// (§5.1). Cheap to keep alive; [`LockManager::start`] runs the parallel
/// heartbeat + idle-release loop until the shutdown watch fires.
pub struct LockManager {
    storage: Arc<Storage>,
    writer_id: String,
    inner: std::sync::Mutex<Inner>,
}

struct Inner {
    /// vb -> guard behind its own async mutex (heartbeat vs ops vs release).
    guards: BTreeMap<String, Arc<tokio::sync::Mutex<LockGuard>>>,
    /// Buckets whose lock we LOST (heartbeat false / deleted underneath):
    /// every write path on them answers 409 until process restart.
    parked: BTreeSet<String>,
    /// Long-hold set: the `[ingest.X]` buckets (never idle-released).
    pinned: BTreeSet<String>,
    /// vb -> last activity (protocol op or acquire); drives idle release.
    last_use: BTreeMap<String, std::time::Instant>,
}

impl LockManager {
    pub fn new(storage: Arc<Storage>) -> Arc<LockManager> {
        Arc::new(LockManager {
            storage,
            writer_id: new_owner_uuid(),
            inner: std::sync::Mutex::new(Inner {
                guards: BTreeMap::new(),
                parked: BTreeSet::new(),
                pinned: BTreeSet::new(),
                last_use: BTreeMap::new(),
            }),
        })
    }

    /// This daemon's writer id (LOCK incumbent marker, R2).
    pub fn writer_id(&self) -> &str {
        &self.writer_id
    }

    /// Mark a bucket as long-hold (`[ingest.X]`): pinned guards are never
    /// idle-released (§5.1).
    pub fn pin(&self, vb: &str) {
        self.inner.lock().unwrap().pinned.insert(vb.to_string());
    }

    /// Record protocol-face activity on a bucket (feeds idle release).
    pub fn note_activity(&self, vb: &str) {
        self.inner
            .lock()
            .unwrap()
            .last_use
            .insert(vb.to_string(), std::time::Instant::now());
    }

    /// Is the bucket's lock currently held and healthy (not parked)?
    pub fn is_held(&self, vb: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.guards.contains_key(vb) && !inner.parked.contains(vb)
    }

    /// Buckets whose locks we currently hold (GC / orphan-sweep scope, R5:
    /// 本 daemon 持锁的桶集合).
    pub fn held_buckets(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .guards
            .keys()
            .filter(|vb| !inner.parked.contains(*vb))
            .cloned()
            .collect()
    }

    /// Lazily acquire the bucket's lock (protocol-face write paths). Held
    /// guards pass through; a parked bucket refuses without re-acquiring (a
    /// takeover means someone else owns it now).
    pub async fn ensure_held(&self, vb: &str) -> Result<(), LockError> {
        self.note_activity(vb);
        {
            let inner = self.inner.lock().unwrap();
            if inner.parked.contains(vb) {
                return Err(LockError::Held(format!(
                    "bucket {vb} is parked on this daemon (its lock was lost to another \
                     writer) — restart the daemon to re-acquire"
                )));
            }
            if inner.guards.contains_key(vb) {
                return Ok(());
            }
        }
        let store = self.storage.system_store(vb);
        let cas_capable = !matches!(self.storage.backend(), Backend::File { .. });
        let guard = acquire_guard(
            store,
            object_store::path::Path::from("LOCK"),
            &self.writer_id,
            cas_capable,
        )
        .await?;
        let mut inner = self.inner.lock().unwrap();
        // a concurrent ensure_held may have won; drop ours if so (its LOCK
        // create lost the race, so this is rare and benign)
        if let Some(existing) = inner.guards.get(vb) {
            let _ = existing; // first holder keeps it
            drop(guard); // no release(): the object names the winner
            return Ok(());
        }
        inner
            .guards
            .insert(vb.to_string(), Arc::new(tokio::sync::Mutex::new(guard)));
        Ok(())
    }

    /// Commit-time re-verification (§5.1 在飞提交前复核归属): in-flight ingest
    /// jobs call this before writing batches; a bucket that lost its lock
    /// (or never had it) makes the caller skip the commit.
    pub fn still_ours(&self, vb: &str) -> bool {
        self.is_held(vb)
    }

    /// Release one bucket's lock (shutdown / delete-tail): removes the guard
    /// and deletes the LOCK object iff it still names us.
    pub async fn release(&self, vb: &str) {
        let guard = self.inner.lock().unwrap().guards.remove(vb);
        if let Some(g) = guard {
            g.lock().await.release().await;
        }
    }

    /// Drop the guard WITHOUT touching the LOCK object — for the Delete
    /// tail (store::delete_bucket deleted the LOCK itself as part of the
    /// prescribed order; a release() here would be a no-op anyway, but the
    /// registry entry must go before the next heartbeat runs).
    pub fn drop_guard(&self, vb: &str) {
        self.inner.lock().unwrap().guards.remove(vb);
        self.inner.lock().unwrap().parked.remove(vb);
    }

    /// Release every held lock (graceful shutdown).
    pub async fn release_all(&self) {
        let guards: Vec<Arc<tokio::sync::Mutex<LockGuard>>> = {
            let inner = self.inner.lock().unwrap();
            inner.guards.values().cloned().collect()
            // NOTE: keep the map — in-flight ops then fail loudly instead of
            // silently re-acquiring during shutdown
        };
        for g in guards {
            g.lock().await.release().await;
        }
    }

    /// Heartbeat + idle-release loop (serve / mcp spawn this). Every guard
    /// heartbeats IN PARALLEL (§5.1); a lost/foreign LOCK parks exactly that
    /// bucket. Guards idle for a full TTL that are NOT pinned get released.
    pub fn start(self: &Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = shutdown.changed() => break,
                }
                this.heartbeat_round().await;
                this.idle_release_round().await;
            }
        });
    }

    /// One parallel heartbeat round over every held guard, plus the
    /// retry pass for pinned buckets we do NOT hold yet (§5.1 startup skip:
    /// a lock held by a dying daemon goes stale after the TTL; the pinned
    /// bucket then recovers without a daemon restart — B-F4's skip must not
    /// strand the bucket for the daemon's whole uptime).
    pub async fn heartbeat_round(&self) {
        self.retry_pinned().await;
        let guards: Vec<(String, Arc<tokio::sync::Mutex<LockGuard>>)> = {
            let inner = self.inner.lock().unwrap();
            inner
                .guards
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        let mut results = futures_util::future::join_all(guards.iter().map(|(vb, g)| {
            let vb = vb.clone();
            async move {
                let mut guard = g.lock().await;
                (vb, guard.heartbeat().await)
            }
        }))
        .await;
        let mut lost = Vec::new();
        for (vb, res) in results.drain(..) {
            match res {
                Ok(true) => {}
                Ok(false) => lost.push((vb, "taken over by another writer".to_string())),
                Err(e) => eprintln!("vectorfs: lock heartbeat error on {vb}: {e} (will retry)"),
            }
        }
        if !lost.is_empty() {
            let mut inner = self.inner.lock().unwrap();
            for (vb, why) in lost {
                eprintln!(
                    "vectorfs: LOCK on bucket {vb} lost ({why}) — parking ONLY this bucket; \
                     its write paths answer 409 until restart"
                );
                inner.guards.remove(&vb);
                inner.parked.insert(vb);
            }
        }
    }

    /// Re-try the per-bucket lock for pinned buckets not currently held
    /// (startup-skip recovery, see [`LockManager::heartbeat_round`]).
    async fn retry_pinned(&self) {
        let want: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            inner
                .pinned
                .iter()
                .filter(|vb| !inner.guards.contains_key(*vb) && !inner.parked.contains(*vb))
                .cloned()
                .collect()
        };
        for vb in want {
            if let Ok(()) = self.ensure_held(&vb).await {
                eprintln!(
                    "vectorfs: lock on bucket {vb} recovered — its worker resumes on the next scan"
                );
            } // still held elsewhere / unreachable: keep waiting
        }
    }

    /// Release non-pinned guards idle for a full TTL (§5.1 lazy locks):
    /// drop the guard AND delete the LOCK object while it still names us —
    /// leaving a fresh-heartbeat object behind would pin the bucket against
    /// every other writer for a full TTL after this daemon stopped caring.
    async fn idle_release_round(&self) {
        let expired: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            inner
                .guards
                .keys()
                .filter(|vb| !inner.pinned.contains(*vb))
                .filter(|vb| {
                    inner
                        .last_use
                        .get(*vb)
                        .is_some_and(|t| t.elapsed() >= LOCK_TTL)
                })
                .cloned()
                .collect()
        };
        for vb in expired {
            eprintln!("vectorfs: lock on {vb} idle for a full TTL — releasing (lazy lock)");
            let guard = self.inner.lock().unwrap().guards.remove(&vb);
            if let Some(g) = guard {
                g.lock().await.release().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_file(dir: &std::path::Path) -> Arc<LockManager> {
        LockManager::new(Arc::new(crate::store::Storage::file(dir)))
    }

    #[test]
    fn flock_second_taker_rejected_while_alive() {
        let dir = tempfile::tempdir().unwrap();
        let backend = crate::store::Storage::file(dir.path()).backend().clone();
        let _g1 = take_flock(&backend).expect("first flock");
        match take_flock(&backend) {
            Err(LockError::Held(msg)) => assert!(msg.contains("flock"), "{msg}"),
            Err(other) => panic!("expected Held, got {other:?}"),
            Ok(_) => panic!("expected Held, got a lock"),
        }
        drop(_g1);
        let _g2 = take_flock(&backend).expect("re-acquire after release");
    }

    #[tokio::test]
    async fn bucket_lock_second_acquire_rejected_while_alive() {
        let dir = tempfile::tempdir().unwrap();
        let m1 = manager_file(dir.path());
        m1.ensure_held("docs").await.unwrap();
        assert!(m1.is_held("docs"));

        // a SECOND daemon (fresh manager, fresh writer id) on the same data
        // dir: refused by the fresh LOCK (flock not involved in-process)
        let m2 = manager_file(dir.path());
        match m2.ensure_held("docs").await {
            Err(LockError::Held(msg)) => assert!(msg.contains("pid="), "{msg}"),
            other => panic!("expected Held, got {other:?}"),
        }
        // ...and the refusal parks nothing: another bucket still acquires
        m2.ensure_held("kb").await.unwrap();

        // heartbeat keeps us alive; graceful release frees the lock
        m1.heartbeat_round().await;
        assert!(m1.is_held("docs"));
        m1.release("docs").await;
        assert!(!m1.is_held("docs"));
        m2.ensure_held("docs")
            .await
            .expect("re-acquire after release");
    }

    #[tokio::test]
    async fn bucket_lock_expired_heartbeat_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::store::Storage::file(dir.path());
        let store = storage.system_store("docs");
        let key = object_store::path::Path::from("LOCK");

        // a daemon that died silently: LOCK left behind, heartbeat 10x TTL old
        let stale = LockInfo {
            owner_uuid: new_owner_uuid(),
            pid: 4242,
            hostname: "dead-host".into(),
            started_at: now_epoch() - 1000.0,
            heartbeat_at: now_epoch() - 1000.0,
        };
        backends::put_json_overwrite(&*store, &key, &serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();

        let m = manager_file(dir.path());
        m.ensure_held("docs").await.expect("takeover");
        m.heartbeat_round().await;
        assert!(m.is_held("docs"));
        assert_ne!(m.writer_id(), stale.owner_uuid);

        // memory backend: same protocol, CAS takeover
        let sm = crate::store::Storage::memory();
        let mm = LockManager::new(Arc::new(sm));
        let mstore = mm.storage.system_store("docs");
        let mut very_stale = stale.clone();
        very_stale.heartbeat_at = now_epoch() - 3600.0;
        backends::put_json_overwrite(&*mstore, &key, &serde_json::to_vec(&very_stale).unwrap())
            .await
            .unwrap();
        mm.ensure_held("docs").await.expect("memory takeover");
        mm.heartbeat_round().await;
        assert!(mm.is_held("docs"));
        mm.release_all().await;
    }

    /// Losing one bucket's lock parks only that bucket (§5.1).
    #[tokio::test]
    async fn losing_one_bucket_parks_only_that_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager_file(dir.path());
        m.ensure_held("docs").await.unwrap();
        m.ensure_held("kb").await.unwrap();
        // simulate takeover: a foreign writer overwrites docs' LOCK
        let foreign = LockInfo {
            owner_uuid: new_owner_uuid(),
            pid: 1,
            hostname: "other".into(),
            started_at: now_epoch(),
            heartbeat_at: now_epoch(),
        };
        let store = m.storage.system_store("docs");
        backends::put_json_overwrite(
            &*store,
            &object_store::path::Path::from("LOCK"),
            &serde_json::to_vec(&foreign).unwrap(),
        )
        .await
        .unwrap();
        m.heartbeat_round().await;
        assert!(!m.is_held("docs"), "docs must be parked");
        assert!(m.is_held("kb"), "kb unaffected");
        // parked bucket's write path refuses (the 409 the ops layer maps)
        assert!(m.ensure_held("docs").await.is_err());
        assert_eq!(m.held_buckets(), vec!["kb".to_string()]);
    }

    /// Idle release lets go of non-pinned guards after a TTL of inactivity;
    /// pinned ([ingest.X]) guards stay.
    #[tokio::test]
    async fn idle_release_respects_pinned_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager_file(dir.path());
        m.ensure_held("docs").await.unwrap();
        m.ensure_held("kb").await.unwrap();
        m.pin("kb");
        // backdate both buckets' last activity beyond the TTL
        {
            let mut inner = m.inner.lock().unwrap();
            for vb in ["docs", "kb"] {
                inner.last_use.insert(
                    vb.to_string(),
                    std::time::Instant::now() - LOCK_TTL - Duration::from_secs(1),
                );
            }
        }
        m.idle_release_round().await;
        assert!(!m.is_held("docs"), "idle protocol lock released");
        assert!(m.is_held("kb"), "pinned ingest lock kept");
        // the released lock's OBJECT must be gone too (a fresh-heartbeat
        // orphan would pin the bucket against other writers for a full TTL)
        assert!(
            crate::backends::read_json::<LockInfo>(
                &*m.storage.system_store("docs"),
                &object_store::path::Path::from("LOCK")
            )
            .await
            .unwrap()
            .is_none(),
            "idle release deletes the LOCK object"
        );
    }

    /// Two concurrent first-touch acquires of the same bucket on ONE
    /// manager: both must succeed (the loser views the winner's LOCK —
    /// a same-daemon race must not 409 itself).
    #[tokio::test]
    async fn concurrent_same_manager_acquire_does_not_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager_file(dir.path());
        let (a, b) = tokio::join!(m.ensure_held("docs"), m.ensure_held("docs"));
        a.expect("first concurrent acquire");
        b.expect("second concurrent acquire");
        assert!(m.is_held("docs"));
        assert_eq!(m.held_buckets(), vec!["docs".to_string()]);
    }

    /// Concurrent creation of the same object: exactly one conditional write
    /// wins (the primitive LOCK and the catalog both rest on).
    #[tokio::test]
    async fn concurrent_bucket_create_exactly_one_wins() {
        let s = crate::store::Storage::memory();
        let store = s.system_store("race");
        let key = object_store::path::Path::from("buckets.json");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
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
