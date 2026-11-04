//! Ingest worker — the file layer's fixed ORCHESTRATION mechanism
//! (design.md §5.10.3 data flow, §5.10.5 robustness; M2 T3).
//!
//! # Module structure
//!
//! * [`marker`] — Source-marker SEMANTICS: no-queue migration rules, key
//!   hashing, cheap change signatures, scope matching. Pure functions.
//! * [`domain`] — derived-index identity (`hash(model, dim, embedder
//!   config)`) and the idempotent bucket/index creation + validation.
//! * this module — the RUNTIME: [`IngestCore`] (config + loaded embedders),
//!   [`IngestRuntime`] (scheduler + workers + queue), [`IngestHandle`]
//!   (wake/drain handle the protocol face and `vectorfs serve` hold).
//!
//! # Execution model (§5.10.5)
//!
//! One SCHEDULER task per instance root: periodic scan (`scan_interval_secs`,
//! default 30) + external wake ([`IngestHandle::wake`] — T6's `vf put` /
//! PutSource immediate trigger) + shutdown, `select`ed together. It holds a
//! bounded mpsc (capacity 64) — scan-and-diff pushes jobs, a small set of
//! WORKER tasks (one per CPU) consumes them. CPU segments (extract / chunk /
//! ort embed) run through `spawn_blocking` behind a semaphore sized to the
//! CPU count; remote (openai) embeds are I/O-bound and skip the gate.
//!
//! # No queue (§5.10.5)
//!
//! There is no task store to corrupt: pending-ness is DERIVED — "no marker,
//! or signature mismatch" — recomputed from the real object listing every
//! scan. A crashed IN_PROGRESS is simply re-enqueued by the next scan
//! (etag unchanged + non-terminal), and put_vectors upsert keys make every
//! rerun converge. Dedup is a live in-flight SET, not a queue.
//!
//! # Shutdown
//!
//! The serve shutdown watch stops the scheduler and makes workers stop
//! TAKING jobs; the in-flight file finishes; [`IngestHandle::shutdown`]
//! joins with a hard budget (8s — inside Docker's 10s SIGKILL grace).

pub mod domain;
pub mod marker;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};

use crate::ops::{self, AppState};
use crate::pipeline::chunk::ChunkerKind;
use crate::pipeline::config::PipelineConfig;
use crate::pipeline::embed::LoadedEmbedder;
use crate::pipeline::extract::{
    ExtractorKind,
    md::{MdConfig, MdTag},
    pdf_text::{PdfTextConfig, PdfTextTag},
    txt::{TxtConfig, TxtTag},
};
use crate::pipeline::{Chunk, Chunker, Embedder, Extractor, Merge, Modality, PipelineError};
use crate::store::{SourceMarker, SourceStatus, Storage};

use domain::DerivedSpec;
use marker::Scope;

/// Bounded queue depth (§5.10.5 "~64").
const QUEUE_DEPTH: usize = 64;

/// Retry backoff base; doubles per attempt (0.5s, 1s, 2s with retries = 3).
const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Hard cap on the graceful-drain wait (Docker's SIGKILL lands at 10s).
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(8);

/// PutVectors wire limits the batch splitter aims to stay under (the ops
/// layer enforces the real 500/20MiB — this estimate just keeps batches from
/// ever reaching them; 1 MiB headroom absorbs JSON structure variance).
const BATCH_MAX_ROWS: usize = 500;
const BATCH_BUDGET_BYTES: usize = 19 * 1024 * 1024;

// ---------------------------------------------------------------------------
// startup gates (serve calls these BEFORE spawning anything)
// ---------------------------------------------------------------------------

/// Fingerprint fail-fast (§5.10.5): every marker of every configured ingest
/// bucket must carry the CURRENT pipeline fingerprint. Any mismatch refuses
/// startup — no silent background re-embedding engine exists by design; the
/// remedy is an explicit `vf reindex` (T7 lands the command; the text names
/// it now so dev-stage config changes have one canonical answer).
pub async fn fingerprint_gate(storage: &Storage, cfg: &PipelineConfig) -> Result<(), String> {
    for (vb, _) in cfg.ingest.iter().filter(|(_, i)| !i.embedders.is_empty()) {
        let current = cfg.fingerprint(vb).expect("bucket present in config");
        let markers = storage
            .list_sources(vb)
            .await
            .map_err(|e| format!("reading source markers of bucket {vb}: {e}"))?;
        for m in markers {
            if m.pipeline_fingerprint != current {
                return Err(format!(
                    "pipeline fingerprint mismatch in bucket '{vb}': source '{}' was ingested \
                     under fingerprint {} but the current configuration derives {current} \
                     (changing extractor/chunker/embedder config invalidates existing chunks). \
                     Refusing to start. Run `vf reindex` to re-embed explicitly, or delete the \
                     marker files under .vectorfs/buckets/{vb}/sources/ if the old chunks are \
                     no longer wanted.",
                    m.object_key, m.pipeline_fingerprint
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// per-bucket plan
// ---------------------------------------------------------------------------

/// Everything one ingest bucket needs at runtime, pre-resolved once.
pub(crate) struct BucketPlan {
    pub fingerprint: String,
    pub scope: Scope,
    /// raw extract routing: mime short key (or `default`) -> instance name
    /// or the `auto` sentinel.
    extract: BTreeMap<String, String>,
    /// declared extractor instances (name -> kind).
    extractors: BTreeMap<String, ExtractorKind>,
    chunker: ChunkerKind,
    /// fan-out list, config order.
    pub(crate) embedders: Vec<String>,
    /// embedder name -> derived index spec.
    pub derived: BTreeMap<String, DerivedSpec>,
    /// Query-side merge strategy for the `/x-vf/Search` fan-out (T6): the
    /// plan carries it so the protocol face can merge with the SAME policy
    /// the bucket was configured with (§5.10.4 `merge`).
    pub(crate) merge: Merge,
    max_file_size_bytes: u64,
    retries: u32,
    extract_timeout: Duration,
    store_chunk_text: bool,
    scan_interval_secs: u64,
}

impl BucketPlan {
    /// The extractor this bucket routes `object_key` to, or None — None
    /// means the object is NOT a source of this pipeline (unknown/absent
    /// extension with no matching extract entry): no marker, no ingest
    /// (§5.10.1: sources are the objects this pipeline claims).
    fn extractor_for(&self, object_key: &str) -> Option<ExtractorKind> {
        let short = short_key_of(object_key)?;
        let instance = self
            .extract
            .get(short)
            .or_else(|| self.extract.get("default"))?;
        match instance.as_str() {
            "auto" => builtin_extractor(short),
            name => self.extractors.get(name).cloned(),
        }
    }
}

/// Extension -> mime short key (the scan-time mime guess; the canonical mime
/// then comes from the short key, so extractor mime gates always agree).
fn short_key_of(object_key: &str) -> Option<&'static str> {
    let file = object_key.rsplit('/').next()?;
    let ext = file.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Some("pdf"),
        "txt" | "text" => Some("txt"),
        "md" | "markdown" => Some("md"),
        _ => None,
    }
}

/// The `auto` mapping's default extractor KIND per short key (kept in lock
/// step with [`MIME_TO_SHORT_KEY`]).
fn builtin_extractor(short_key: &str) -> Option<ExtractorKind> {
    match short_key {
        "pdf" => Some(ExtractorKind::PdfText(PdfTextConfig {
            kind: PdfTextTag::PdfText,
        })),
        "md" => Some(ExtractorKind::Md(MdConfig { kind: MdTag::Md })),
        "txt" => Some(ExtractorKind::Txt(TxtConfig { kind: TxtTag::Txt })),
        _ => None,
    }
}

/// Canonical mime for the routing table (extractors compare against these).
/// `pub(crate)`: the `/x-vf` face reports the same mime a source would be
/// routed under (GetSource/GetSourceData — T6).
pub(crate) fn mime_for(object_key: &str) -> &'static str {
    short_key_of(object_key)
        .and_then(crate::pipeline::extract::mime_for_short_key)
        .unwrap_or("application/octet-stream")
}

// ---------------------------------------------------------------------------
// loaded core (constructed once, pre-lock at serve startup)
// ---------------------------------------------------------------------------

/// Config + loaded embedders, before any task exists. Loading an embedder is
/// the STARTUP gate for its kind (ort dim probe, openai credentials) and may
/// prefetch a missing model (network) — deliberately done while no worker
/// tasks exist.
pub struct IngestCore {
    /// pub(crate): the serve binary summarizes via [`IngestCore::describe_buckets`].
    pub(crate) buckets: BTreeMap<String, BucketPlan>,
    embedders: BTreeMap<String, LoadedEmbedder>,
    cpu: Arc<Semaphore>,
}

impl IngestCore {
    /// Load every embedder referenced by an ingest section and pre-resolve
    /// the per-bucket plans (fingerprints, scopes, derived-index specs).
    pub async fn load(
        cfg: &PipelineConfig,
        models_dir: &Path,
    ) -> Result<IngestCore, PipelineError> {
        let referenced: BTreeSet<String> = cfg
            .ingest
            .values()
            .flat_map(|i| i.embedders.iter().cloned())
            .collect();
        let mut embedders = BTreeMap::new();
        for name in &referenced {
            let kind = cfg.embedders.get(name).ok_or_else(|| {
                PipelineError::config(format!("[embedder.{name}]: not declared"), None)
            })?;
            let loaded = kind.load(models_dir).await?;
            embedders.insert(name.clone(), loaded);
        }

        let mut buckets = BTreeMap::new();
        for (vb, icfg) in &cfg.ingest {
            let mut derived = BTreeMap::new();
            for name in &icfg.embedders {
                let kind = &cfg.embedders[name];
                let dim = embedders[name].dim();
                derived.insert(
                    name.clone(),
                    domain::spec_for(name, kind, dim, icfg.index_prefix.as_deref()),
                );
            }
            buckets.insert(
                vb.clone(),
                BucketPlan {
                    fingerprint: cfg.fingerprint(vb).expect("bucket in config"),
                    scope: Scope::new(icfg.scope.as_deref()),
                    extract: icfg.extract.clone(),
                    extractors: cfg.extractors.clone(),
                    chunker: icfg.chunk.clone(),
                    embedders: icfg.embedders.clone(),
                    derived,
                    merge: icfg.merge,
                    max_file_size_bytes: icfg.max_file_size_mb * 1024 * 1024,
                    retries: icfg.retries,
                    extract_timeout: Duration::from_secs(icfg.extract_timeout_secs),
                    store_chunk_text: icfg.store_chunk_text,
                    scan_interval_secs: icfg.scan_interval_secs as u64,
                },
            );
        }

        let permits = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Ok(IngestCore {
            buckets,
            embedders,
            cpu: Arc::new(Semaphore::new(permits)),
        })
    }

    pub fn embedder(&self, name: &str) -> Option<&LoadedEmbedder> {
        self.embedders.get(name)
    }

    /// Per-bucket startup summary: (bucket, embedder count, scan seconds).
    /// BucketPlan itself stays crate-private; the serve log only needs this.
    pub fn describe_buckets(&self) -> Vec<(String, usize, u64)> {
        self.buckets
            .iter()
            .map(|(vb, p)| (vb.clone(), p.embedders.len(), p.scan_interval_secs))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// runtime (scheduler + workers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Job {
    vb: String,
    object_key: String,
    key_hash: String,
}

impl Job {
    fn dedup_key(&self) -> String {
        format!("{}\u{1}{}", self.vb, self.object_key)
    }
}

/// How one job attempt failed (drives the retry vs park decision).
enum AttemptErr {
    /// Deterministic dead end — retrying cannot help (size cap, derived-key
    /// mismatch, metadata budget). FAILED + retries_exhausted immediately.
    Parked(String),
    /// Ordinary failure — retry with backoff, then FAILED/EXHAUSTED (or
    /// PARTIALLY_INDEXED when some embedders already landed).
    Fail(String),
}

/// The live ingest runtime. Cheap to keep alive; the tasks it spawns hold
/// `Arc` clones, so dropping the outer value does not stop them (drain via
/// [`IngestHandle::shutdown`]).
pub struct IngestRuntime {
    /// The SAME shared core the protocol face sees through
    /// [`IngestHandle::shared`] (one `Arc<IngestCore>` for the whole process:
    /// embedders are loaded exactly once, serve's handlers and this
    /// runtime's workers call the same sessions).
    shared: Arc<IngestShared>,
    state: Arc<AppState>,
    notify: Arc<Notify>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    tx: mpsc::Sender<Job>,
    /// The single mpsc receiver behind an async Mutex in the PASSED-BATON
    /// pattern: a worker takes the lock and waits in `recv()`; the moment a
    /// job arrives the guard drops (block-scoped) and the worker processes
    /// UNLOCKED while the next worker picks up the baton and waits. All
    /// workers can hold jobs concurrently; exactly one ever waits.
    rx: Arc<Mutex<mpsc::Receiver<Job>>>,
    /// live dedup: queued + in-flight jobs (§5.10.5 "幂等 key 防重复入队").
    inflight: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Serializes the put path across workers: lance commits are
    /// optimistic (read-modify-write), so concurrent merge_inserts into ONE
    /// derived index would race and surface commit-conflict errors that the
    /// retry loop would then burn attempts on. Extraction/embedding still
    /// run in parallel — only the final commit is one-at-a-time.
    write_lock: tokio::sync::Mutex<()>,
}

impl IngestRuntime {
    pub fn new(
        shared: Arc<IngestShared>,
        state: Arc<AppState>,
        notify: Arc<Notify>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Arc<IngestRuntime> {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        Arc::new(IngestRuntime {
            shared,
            state,
            notify,
            shutdown,
            tx,
            rx: Arc::new(Mutex::new(rx)),
            inflight: Arc::new(std::sync::Mutex::new(HashSet::new())),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Scheduler + worker tasks. `handle` collects the JoinHandles for the
    /// SIGTERM drain. Worker count = CPU count: the semaphore already caps
    /// CPU segments; extra workers would only parallelize I/O-bound
    /// (openai) jobs, which the CPU gate deliberately does not cover.
    pub fn spawn(self: &Arc<Self>, handle: &IngestHandle) {
        // scan period: the MINIMUM across buckets — one scan sweeps EVERY
        // bucket off ONE shared root listing, so the cheapest schedule is a
        // single ticker at the fastest configured period.
        let scan_interval = Duration::from_secs(
            self.shared
                .core
                .buckets
                .values()
                .map(|p| p.scan_interval_secs)
                .min()
                .unwrap_or(30),
        );

        // scheduler
        {
            let rt = Arc::clone(self);
            let notify = Arc::clone(&self.notify);
            let mut shutdown = self.shutdown.clone();
            let task = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(scan_interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ticker.tick().await; // first tick fires immediately; skip it
                loop {
                    tokio::select! {
                        _ = ticker.tick() => rt.scan_once().await,
                        _ = notify.notified() => rt.scan_once().await,
                        _ = shutdown.changed() => break,
                    }
                }
            });
            handle.register(task);
        }
        // workers
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        for _ in 0..workers {
            let rt = Arc::clone(self);
            let mut shutdown = self.shutdown.clone();
            let rx = Arc::clone(&self.rx);
            let task = tokio::spawn(async move {
                loop {
                    // baton pass: the lock is held ONLY around recv(); a
                    // shutdown win drops the (not-yet-acquired or waiting)
                    // lock future and breaks without taking a new job
                    let job = {
                        tokio::select! {
                            j = async { rx.lock().await.recv().await } => j,
                            _ = shutdown.changed() => break,
                        }
                    };
                    match job {
                        Some(j) => {
                            rt.process_job(&j).await;
                            rt.finish(&j);
                        }
                        None => break,
                    }
                }
            });
            handle.register(task);
        }
    }

    fn finish(&self, job: &Job) {
        self.inflight.lock().unwrap().remove(&job.dedup_key());
    }

    // -- scan --------------------------------------------------------------

    /// One full list-and-diff pass over every configured bucket. Idempotent;
    /// safe to call concurrently with workers (markers are single-writer —
    /// this process holds the LOCK).
    pub async fn scan_once(&self) {
        let listing = match list_root(&self.state.storage).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ingest: scan failed (listing the instance root): {e}");
                return;
            }
        };
        for (vb, plan) in &self.shared.core.buckets {
            match self.scan_bucket(vb, plan, &listing).await {
                Ok(()) => {
                    // lastScanAt for GetIngestStatus (appendix E.2): stamp on
                    // COMPLETION — a half-failed scan must not claim freshness.
                    self.shared.record_scan(vb);
                }
                Err(e) => eprintln!("ingest: scan of bucket {vb} failed: {e}"),
            }
        }
    }

    async fn scan_bucket(
        &self,
        vb: &str,
        plan: &BucketPlan,
        listing: &[(String, object_store::ObjectMeta)],
    ) -> Result<(), String> {
        let storage = &self.state.storage;
        let mut present = HashSet::new();
        let (mut n_new, mut n_changed, mut n_requeue) = (0u32, 0u32, 0u32);
        for (key, meta) in listing {
            if !plan.scope.matches(key) || plan.extractor_for(key).is_none() {
                continue;
            }
            present.insert(key.clone());
            let key_hash = marker::object_key_hash(key);
            let sig = marker::signature_of(meta);
            let existing = storage
                .load_source(vb, &key_hash)
                .await
                .map_err(|e| e.to_string())?;
            let now = marker::now_rfc3339();
            match marker::plan_present(existing, key, &sig, &plan.fingerprint, &now) {
                marker::ScanPlan::New(m) => {
                    storage
                        .save_source(vb, &key_hash, &m)
                        .await
                        .map_err(|e| e.to_string())?;
                    n_new += 1;
                    self.enqueue(Job {
                        vb: vb.to_string(),
                        object_key: key.clone(),
                        key_hash,
                    })
                    .await;
                }
                marker::ScanPlan::Update(m) => {
                    storage
                        .save_source(vb, &key_hash, &m)
                        .await
                        .map_err(|e| e.to_string())?;
                    n_changed += 1;
                    self.enqueue(Job {
                        vb: vb.to_string(),
                        object_key: key.clone(),
                        key_hash,
                    })
                    .await;
                }
                marker::ScanPlan::Enqueue => {
                    n_requeue += 1;
                    self.enqueue(Job {
                        vb: vb.to_string(),
                        object_key: key.clone(),
                        key_hash,
                    })
                    .await;
                }
                marker::ScanPlan::Settled | marker::ScanPlan::Parked => {}
            }
        }
        // vanished objects -> DELETING (T7's GC reaps chunks + markers; this
        // task only sets the state). Only markers inside the CURRENT scope:
        // a scope change must not mass-delete what it merely stopped covering.
        let mut n_gone = 0u32;
        for m in storage.list_sources(vb).await.map_err(|e| e.to_string())? {
            if plan.scope.matches(&m.object_key) && !present.contains(&m.object_key) {
                if let Some(d) = marker::plan_vanished(m, &marker::now_rfc3339()) {
                    let key_hash = marker::object_key_hash(&d.object_key);
                    storage
                        .save_source(vb, &key_hash, &d)
                        .await
                        .map_err(|e| e.to_string())?;
                    n_gone += 1;
                }
            }
        }
        if n_new + n_changed + n_requeue + n_gone > 0 {
            eprintln!(
                "ingest: scan {vb}: {n_new} new, {n_changed} changed, {n_requeue} requeued, \
                 {n_gone} deleting"
            );
        }
        Ok(())
    }

    async fn enqueue(&self, job: Job) {
        let k = job.dedup_key();
        if !self.inflight.lock().unwrap().insert(k.clone()) {
            return; // already queued or in flight
        }
        // bounded send: backpressure into the scanner is the designed
        // behavior (the queue is the shock absorber, not a buffer of the
        // whole root).
        if self.tx.send(job).await.is_err() {
            // receivers all gone (shutdown) — undo the dedup entry
            self.inflight.lock().unwrap().remove(&k);
        }
    }

    // -- jobs ----------------------------------------------------------------

    /// Test/deterministic path: drain everything currently queued, one job
    /// at a time (no workers spawned). Production uses the spawned workers;
    /// both call the same [`IngestRuntime::process_job`].
    pub async fn drain_queue(&self) {
        loop {
            // try_recv, not recv: the runtime itself owns a Sender (self.tx),
            // so the channel never CLOSES — "empty" is the drain condition
            // after a completed scan, not Disconnected.
            let job = self.rx.lock().await.try_recv();
            match job {
                Ok(j) => {
                    self.process_job(&j).await;
                    self.finish(&j);
                }
                Err(mpsc::error::TryRecvError::Empty)
                | Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    async fn process_job(&self, job: &Job) {
        let Some(plan) = self.shared.core.buckets.get(&job.vb) else {
            return;
        };
        let storage = &self.state.storage;

        // ---- load marker, flip IN_PROGRESS ----
        let prev = match storage.load_source(&job.vb, &job.key_hash).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                // marker vanished between scan and job (operator wiped it):
                // a fresh PENDING is exactly the scan's own New plan
                match marker::plan_present(
                    None,
                    &job.object_key,
                    "",
                    &plan.fingerprint,
                    &marker::now_rfc3339(),
                ) {
                    marker::ScanPlan::New(m) => m,
                    _ => unreachable!("no-marker plan is always New"),
                }
            }
            Err(e) => {
                eprintln!("ingest: {}: cannot read marker: {e}", job.object_key);
                return;
            }
        };
        let mut m = prev.clone();
        m.status = SourceStatus::InProgress;
        m.updated_at = marker::now_rfc3339();
        if let Err(e) = storage.save_source(&job.vb, &job.key_hash, &m).await {
            eprintln!("ingest: {}: cannot persist marker: {e}", job.object_key);
            return;
        }

        // ---- read the object (size-gated) ----
        // candidate keys: canonical first, raw second — externally placed
        // files keep their raw name on the file backend (T6 finding, see
        // domain::user_object_key_candidates)
        let backend = storage.backend().clone();
        let mut get = None;
        let mut hard_err = None;
        for okey in domain::user_object_key_candidates(&backend, &job.object_key) {
            match storage.catalog().get(&okey).await {
                Ok(g) => {
                    get = Some(g);
                    break;
                }
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(e) => {
                    hard_err = Some(e);
                    break;
                }
            }
        }
        let get = match get {
            Some(g) => g,
            None => {
                if let Some(e) = hard_err {
                    eprintln!(
                        "ingest: {}: read failed: {e} (next scan retries)",
                        job.object_key
                    );
                    return;
                }
                // vanished between scan and job — same as the scanner's
                // vanished path
                if let Some(d) = marker::plan_vanished(prev.clone(), &marker::now_rfc3339()) {
                    let _ = storage.save_source(&job.vb, &job.key_hash, &d).await;
                    eprintln!("ingest: {} -> DELETING (object gone)", job.object_key);
                }
                return;
            }
        };
        let sig = marker::signature_of(&get.meta);
        if get.meta.size > plan.max_file_size_bytes {
            // deterministic poison: retrying cannot shrink the file
            self.finish_marker(
                job,
                &prev,
                Outcome::Parked(format!(
                    "file size {} bytes exceeds the configured max_file_size_mb ({} MiB) — \
                     enlarge the limit or split the file",
                    get.meta.size,
                    plan.max_file_size_bytes / 1024 / 1024
                )),
            )
            .await;
            return;
        }
        let bytes = match get.bytes().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "ingest: {}: body read failed: {e} (next scan retries)",
                    job.object_key
                );
                return;
            }
        };

        // ---- content-hash skip (§5.10.5): metadata-only rewrite ----
        let hash = marker::content_hash_of(&bytes);
        if marker::content_unchanged(&prev, &hash) {
            let mut m = prev.clone();
            m.etag_sig = sig;
            m.updated_at = marker::now_rfc3339();
            // prev.status is INDEXED or PARTIALLY_INDEXED (content_unchanged
            // demands chunks) — restored verbatim, no re-embed happened.
            let _ = storage.save_source(&job.vb, &job.key_hash, &m).await;
            eprintln!(
                "ingest: {} -> {} (content unchanged; signature refreshed, no re-embed)",
                job.object_key, m.status
            );
            return;
        }

        // ---- attempts ----
        let mut cached_chunks: Option<Vec<Chunk>> = None;
        let mut succeeded: BTreeSet<String> = BTreeSet::new();
        let mut last_err = String::new();
        let mut parked: Option<String> = None;
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match self
                .try_attempt(job, plan, &bytes, &mut cached_chunks, &mut succeeded)
                .await
            {
                Ok(()) => {
                    last_err.clear();
                    break;
                }
                Err(AttemptErr::Parked(reason)) => {
                    parked = Some(reason);
                    break;
                }
                Err(AttemptErr::Fail(e)) => {
                    last_err = e;
                    if attempts > plan.retries {
                        break;
                    }
                    let backoff = BACKOFF_BASE * (1u32 << (attempts - 1).min(16));
                    tokio::time::sleep(backoff).await;
                }
            }
        }

        let chunk_count = cached_chunks.as_ref().map(|c| c.len()).unwrap_or(0);
        let chunks_map = Some(BTreeMap::from([("text".to_string(), chunk_count as u32)]));
        let all_done = succeeded.len() == plan.embedders.len();
        let outcome = if let Some(reason) = parked {
            Outcome::Parked(reason)
        } else if all_done {
            Outcome::Indexed {
                sig,
                hash,
                chunks: chunks_map,
                retries: attempts - 1,
            }
        } else if !succeeded.is_empty() {
            let failed: Vec<&str> = plan
                .embedders
                .iter()
                .filter(|n| !succeeded.contains(*n))
                .map(String::as_str)
                .collect();
            Outcome::Partial {
                sig,
                hash,
                chunks: chunks_map,
                reason: format!(
                    "embedder(s) {} failed after {} attempt(s): {last_err}",
                    failed.join(", "),
                    attempts
                ),
            }
        } else {
            Outcome::Failed(format!("failed after {} attempt(s): {last_err}", attempts))
        };
        self.finish_marker(job, &prev, outcome).await;
    }

    /// One attempt: extract+chunk (cached across attempts), then fan the
    /// text chunks out to every not-yet-succeeded embedder, writing each
    /// derived index in ops-layer batches.
    async fn try_attempt(
        &self,
        job: &Job,
        plan: &BucketPlan,
        bytes: &Bytes,
        cached: &mut Option<Vec<Chunk>>,
        succeeded: &mut BTreeSet<String>,
    ) -> Result<(), AttemptErr> {
        // ---- extract + chunk (CPU segment) ----
        let chunks: &[Chunk] = match cached {
            Some(c) => c,
            None => {
                let extractor = plan.extractor_for(&job.object_key).ok_or_else(|| {
                    AttemptErr::Parked(format!(
                        "no extractor handles this object (routing changed after the scan) — \
                         it is not a source of this pipeline"
                    ))
                })?;
                let chunker = plan.chunker.clone();
                let mime = mime_for(&job.object_key).to_string();
                let bytes = bytes.clone();
                let permit = self
                    .shared
                    .core
                    .cpu
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| AttemptErr::Fail(format!("cpu semaphore closed: {e}")))?;
                let mut work = tokio::task::spawn_blocking(move || {
                    let _hold = permit;
                    extractor
                        .extract(&bytes, &mime)
                        .and_then(|parts| chunker.chunk(&parts))
                });
                // wall-clock cap around the extract call (§5.10.5 poison
                // guard). On timeout the handle is ABORTED (not dropped:
                // dropping detaches) — a blocked extractor thread eventually
                // observes the cancel at its next boundary and releases the
                // CPU permit.
                let joined = match tokio::time::timeout(plan.extract_timeout, &mut work).await {
                    Ok(j) => j,
                    Err(_elapsed) => {
                        work.abort();
                        return Err(AttemptErr::Fail(format!(
                            "extractor timed out after {}s (extract_timeout_secs)",
                            plan.extract_timeout.as_secs()
                        )));
                    }
                };
                // joined: Result<Result<Vec<Chunk>, PipelineError>, JoinError>
                // (timeout layer already unwrapped by the match above)
                let chunks: Vec<Chunk> = match joined {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => return Err(AttemptErr::Fail(e.to_string())),
                    Err(e) => return Err(AttemptErr::Fail(format!("extract task panicked: {e}"))),
                };
                *cached = Some(chunks);
                cached.as_deref().expect("just stored")
            }
        };

        // ---- fan-out (§5.10.1: every doc-modality-matching embedder) ----
        for name in &plan.embedders {
            if succeeded.contains(name) {
                continue;
            }
            let loaded = self
                .shared
                .core
                .embedder(name)
                .ok_or_else(|| AttemptErr::Fail(format!("embedder '{name}' not loaded")))?;
            let spec = &plan.derived[name];
            let vectors = self
                .embed_gated(loaded, chunks)
                .await
                .map_err(|e| AttemptErr::Fail(format!("embedder '{name}' failed: {e}")))?;

            let rows = build_rows(plan, job, spec, chunks, &vectors)?;
            for batch in split_batches(rows, spec.dim) {
                // domain check + commit under the write lock: the check is
                // load-then-(maybe)-create, and racing it against another
                // worker's identical create would surface as Conflict noise.
                let _guard = self.write_lock.lock().await;
                // per-batch domain check (§5.10.5): the protocol face can
                // drop/recreate indexes between scans; ours is recreated
                // idempotently, a FOREIGN one under the name parks the file.
                match domain::ensure_domain(
                    &self.state.storage,
                    &job.vb,
                    spec,
                    &self.state.region,
                    &self.state.account,
                )
                .await
                {
                    Ok(()) => {}
                    Err(domain::DomainError::Mismatch(m)) => return Err(AttemptErr::Parked(m)),
                    Err(domain::DomainError::Io(m)) => {
                        return Err(AttemptErr::Fail(format!("domain check: {m}")));
                    }
                }
                self.put_batch(&job.vb, &spec.index_name, batch)
                    .await
                    .map_err(|e| {
                        AttemptErr::Fail(format!(
                            "put_vectors into {}/{} rejected: {e}",
                            job.vb, spec.index_name
                        ))
                    })?;
            }
            succeeded.insert(name.clone());
        }
        Ok(())
    }

    /// CPU gate: LOCAL inference holds the thread (ort wraps itself in
    /// spawn_blocking); remote HTTP embeds are I/O-bound and stay ungated.
    async fn embed_gated(
        &self,
        loaded: &LoadedEmbedder,
        chunks: &[Chunk],
    ) -> Result<Vec<Vec<f32>>, PipelineError> {
        match loaded {
            LoadedEmbedder::Ort(_) => {
                let permit = self
                    .shared
                    .core
                    .cpu
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| PipelineError::io(format!("cpu semaphore closed: {e}")))?;
                let out = loaded.embed_documents(chunks).await;
                drop(permit);
                out
            }
            LoadedEmbedder::OpenAi(_) => loaded.embed_documents(chunks).await,
        }
    }

    /// One ops-layer PutVectors batch (the mcp.rs in-process path — same
    /// handlers, same 500/20MiB/40KB validation semantics, zero bypass).
    async fn put_batch(
        &self,
        vb: &str,
        index_name: &str,
        rows: Vec<BatchRow>,
    ) -> Result<(), String> {
        let vectors: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "key": r.key,
                    "data": { "float32": r.vector },
                    "metadata": r.metadata,
                })
            })
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({
            "vectorBucketName": vb,
            "indexName": index_name,
            "vectors": vectors,
        }))
        .map_err(|e| format!("serializing batch: {e}"))?;
        let resp = ops::put_vectors(State(self.state.as_ref().clone()), Bytes::from(body))
            .await
            .map_err(|e| format!("{}: {}", e.shape, e.message))?;
        let _ = resp; // 200 empty on success
        Ok(())
    }

    /// Persist the final marker + one stderr progress line per file.
    async fn finish_marker(&self, job: &Job, prev: &SourceMarker, outcome: Outcome) {
        let mut m = prev.clone();
        m.updated_at = marker::now_rfc3339();
        match outcome {
            Outcome::Indexed {
                sig,
                hash,
                chunks,
                retries,
            } => {
                m.status = SourceStatus::Indexed;
                m.etag_sig = sig;
                m.content_hash = hash;
                m.chunks = chunks;
                m.retries = retries;
                m.retries_exhausted = false;
                m.status_reason = None;
                m.pipeline_fingerprint = self.shared.core.buckets[&job.vb].fingerprint.clone();
                eprintln!(
                    "ingest: {} -> INDEXED (chunks={})",
                    job.object_key,
                    m.chunks
                        .as_ref()
                        .and_then(|c| c.get("text").copied())
                        .unwrap_or(0)
                );
            }
            Outcome::Partial {
                sig,
                hash,
                chunks,
                reason,
            } => {
                m.status = SourceStatus::PartiallyIndexed;
                m.etag_sig = sig;
                m.content_hash = hash;
                m.chunks = chunks;
                m.retries_exhausted = true; // the retry budget IS spent
                m.status_reason = Some(reason.clone());
                m.pipeline_fingerprint = self.shared.core.buckets[&job.vb].fingerprint.clone();
                eprintln!("ingest: {} -> PARTIALLY_INDEXED ({reason})", job.object_key);
            }
            Outcome::Failed(reason) => {
                m.status = SourceStatus::Failed;
                m.retries_exhausted = true;
                m.status_reason = Some(reason.clone());
                m.chunks = None;
                eprintln!("ingest: {} -> FAILED ({reason})", job.object_key);
            }
            Outcome::Parked(reason) => {
                m.status = SourceStatus::Failed;
                m.retries_exhausted = true;
                m.status_reason = Some(reason.clone());
                m.chunks = None;
                eprintln!("ingest: {} -> FAILED/parked ({reason})", job.object_key);
            }
        }
        if let Err(e) = self
            .state
            .storage
            .save_source(&job.vb, &job.key_hash, &m)
            .await
        {
            eprintln!(
                "ingest: {}: cannot persist final marker: {e}",
                job.object_key
            );
        }
    }
}

/// Job outcome → marker. `sig`/`hash` ride along for the states that record
/// them; `prev` (the pre-IN_PROGRESS marker) is the base.
enum Outcome {
    Indexed {
        sig: String,
        hash: String,
        chunks: Option<BTreeMap<String, u32>>,
        retries: u32,
    },
    Partial {
        sig: String,
        hash: String,
        chunks: Option<BTreeMap<String, u32>>,
        reason: String,
    },
    Failed(String),
    Parked(String),
}

// ---------------------------------------------------------------------------
// rows & batching
// ---------------------------------------------------------------------------

struct BatchRow {
    key: String,
    vector: Vec<f32>,
    metadata: serde_json::Value,
}

/// Chunk metadata (§5.10.1 complete key set): `x-vf-src-uri` (key hash,
/// filterable, the GC join key), `x-vf-src-name` (full key, non-filterable),
/// `x-vf-modality`, `x-vf-model`, `x-vf-chunk-text` (default on, droppable
/// via `store_chunk_text = false`).
fn build_rows(
    plan: &BucketPlan,
    job: &Job,
    spec: &DerivedSpec,
    chunks: &[Chunk],
    vectors: &[Vec<f32>],
) -> Result<Vec<BatchRow>, AttemptErr> {
    assert_eq!(chunks.len(), vectors.len(), "one vector per chunk");
    let mut rows = Vec::with_capacity(chunks.len());
    for (chunk, vector) in chunks.iter().zip(vectors) {
        let mut metadata = serde_json::json!({
            "x-vf-src-uri": job.key_hash,
            "x-vf-src-name": job.object_key,
            "x-vf-modality": Modality::Text.to_string(),
            "x-vf-model": spec.model,
        });
        if plan.store_chunk_text {
            metadata
                .as_object_mut()
                .expect("object literal")
                .insert("x-vf-chunk-text".into(), serde_json::json!(chunk.text));
        }
        // metadata budget preflight (§5.10.5 fail-loud precheck): refuse the
        // FILE, not a mystery 400 from the ops layer mid-batch
        let encoded = serde_json::to_vec(&metadata)
            .map_err(|e| AttemptErr::Fail(format!("serializing chunk metadata: {e}")))?;
        if encoded.len() > 40 * 1024 {
            return Err(AttemptErr::Parked(format!(
                "chunk metadata is {} bytes (over the 40 KB per-vector budget) — reduce the \
                 chunker window size or disable store_chunk_text",
                encoded.len()
            )));
        }
        rows.push(BatchRow {
            key: marker::chunk_key(&job.key_hash, "text", chunk.index),
            vector: vector.clone(),
            metadata,
        });
    }
    Ok(rows)
}

/// Batch = min(500 rows, 19 MiB estimated JSON). The estimate counts keys +
/// metadata + a worst-case 13 bytes per float (sign/digits/exponent/comma);
/// the ops layer enforces the REAL limits — this only keeps batches from
/// tripping them.
fn split_batches(rows: Vec<BatchRow>, dim: usize) -> Vec<Vec<BatchRow>> {
    let per_vector_json = dim.saturating_mul(13) + 64;
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut cur_est = 0usize;
    for row in rows {
        let est = row.key.len() + per_vector_json + row.metadata.to_string().len();
        if !cur.is_empty() && (cur.len() >= BATCH_MAX_ROWS || cur_est + est > BATCH_BUDGET_BYTES) {
            out.push(std::mem::take(&mut cur));
            cur_est = 0;
        }
        cur_est += est;
        cur.push(row);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// instance-root listing
// ---------------------------------------------------------------------------

/// List the WHOLE instance root (user namespace), returning
/// root-relative keys with their metadata. Excludes the reserved system
/// prefix (`.vectorfs/`, plus the memory backend's `vf/.vectorfs/` lance
/// mirror) — model caches live under it too, so the exclusion is what keeps
/// scans off them (§5.10.4 "`.vectorfs/` 恒排除").
async fn list_root(storage: &Storage) -> Result<Vec<(String, object_store::ObjectMeta)>, String> {
    list_root_at(storage.catalog(), storage.backend()).await
}

/// The backend-parameterized half of [`list_root`] — the `/x-vf` face
/// (ListSources' marker x live join, T6) reuses it against the same catalog.
///
/// Key canonicalization (a T6-found latent bug of the T3 scanner):
/// `object_store::path::Path` percent-encodes non-ASCII and AWS-discouraged
/// characters (`%{}\^]"'>~<#|*?` + controls) at construction, and the LOCAL
/// backend materializes that canonical form on disk — so a listed location's
/// string is CANONICAL, while every human-facing key (marker `object_key`,
/// PutSource's decoded `x-vf-file-name`, GetSource lookups through
/// [`domain::user_object_key`], which re-canonicalizes symmetrically) is the
/// RAW key. The listing is therefore DECODED back here; without it, any key
/// with such characters would phantom-appear (encoded twin) and
/// vanish-flip (raw twin) on every scan. Decode is the exact inverse of
/// `Path`'s encoding (which escapes `%` itself), so the round trip is
/// bijective.
pub(crate) async fn list_root_at(
    catalog: &Arc<dyn object_store::ObjectStore>,
    backend: &crate::backends::Backend,
) -> Result<Vec<(String, object_store::ObjectMeta)>, String> {
    use futures_util::StreamExt;
    let mut stream = catalog.list(None);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|e| format!("list: {e}"))?;
        let raw = meta.location.as_ref().to_string();
        // S3: the catalog store is rooted at the BUCKET, so keys carry the
        // configured prefix; strip it and skip anything outside it (other
        // tenants of the same bucket are not this instance root).
        let key = match backend {
            crate::backends::Backend::S3(cfg) => {
                let Some(k) = raw.strip_prefix(&format!("{}/", cfg.prefix.trim_matches('/')))
                else {
                    continue;
                };
                k.to_string()
            }
            _ => raw,
        };
        let key = percent_encoding::percent_decode_str(&key)
            .decode_utf8_lossy()
            .into_owned();
        if key.starts_with(crate::backends::SYSTEM_PREFIX)
            || key.starts_with(&format!(
                "{}/{}",
                crate::backends::MEMORY_HOST,
                crate::backends::SYSTEM_PREFIX
            ))
            || key.is_empty()
        {
            continue;
        }
        out.push((key, meta));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

// ---------------------------------------------------------------------------
// shared state (runtime <-> protocol face) + handle (wake + drain)
// ---------------------------------------------------------------------------

/// One reindex job (appendix E.2 GetIngestStatus `reindex{jobId,status,
/// progress}`): the registry keeps the LATEST job per bucket — a re-reindex
/// while one is outstanding simply replaces the entry (the marker flip is
/// idempotent; progress is recomputed from the markers either way).
#[derive(Debug, Clone)]
pub struct ReindexJob {
    pub job_id: String,
    /// `queued` -> `running` -> `done`; advanced lazily by GetIngestStatus
    /// reads (single-process, single-writer — a read is a safe checkpoint).
    pub status: &'static str,
    pub total: usize,
    pub done: usize,
}

/// The piece of the ingest runtime the PROTOCOL face (`AppState.ingest` ->
/// [`IngestHandle`]) holds: the loaded core (one `Arc` shared with the
/// runtime's workers), the last-scan clock and the reindex job registry,
/// plus `wake()` for the immediate trigger and `shutdown()` for the drain.
pub struct IngestShared {
    core: Arc<IngestCore>,
    /// bucket -> RFC3339 of the last COMPLETED scan (GetIngestStatus).
    last_scan: std::sync::Mutex<BTreeMap<String, String>>,
    /// bucket -> latest reindex job.
    jobs: std::sync::Mutex<BTreeMap<String, ReindexJob>>,
}

impl IngestShared {
    pub fn new(core: IngestCore) -> Arc<IngestShared> {
        Arc::new(IngestShared {
            core: Arc::new(core),
            last_scan: std::sync::Mutex::new(BTreeMap::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    /// The configured ingest buckets, name-sorted (BTreeMap order).
    pub fn bucket_names(&self) -> Vec<String> {
        self.core.buckets.keys().cloned().collect()
    }

    pub fn has_bucket(&self, bucket: &str) -> bool {
        self.core.buckets.contains_key(bucket)
    }

    /// Current pipeline fingerprint of one bucket (`None` = unconfigured).
    pub fn fingerprint_of(&self, bucket: &str) -> Option<String> {
        self.core.buckets.get(bucket).map(|p| p.fingerprint.clone())
    }

    /// Fan-out list (config order) of one bucket's embedder instance names.
    pub fn embedder_names(&self, bucket: &str) -> Option<Vec<String>> {
        self.core.buckets.get(bucket).map(|p| p.embedders.clone())
    }

    /// Query-side merge strategy of one bucket (`/x-vf/Search`).
    pub fn merge_of(&self, bucket: &str) -> Option<Merge> {
        self.core.buckets.get(bucket).map(|p| p.merge)
    }

    /// Derived-index specs of one bucket, config embedder order.
    pub fn derived_specs(&self, bucket: &str) -> Option<Vec<domain::DerivedSpec>> {
        let plan = self.core.buckets.get(bucket)?;
        Some(
            plan.embedders
                .iter()
                .map(|n| plan.derived[n].clone())
                .collect(),
        )
    }

    /// A loaded embedder instance by name (the query side of `/x-vf/Search`).
    pub fn embedder(&self, name: &str) -> Option<&LoadedEmbedder> {
        self.core.embedder(name)
    }

    /// Is `object_key` a SOURCE of this bucket's pipeline (scope match AND a
    /// routed extractor)? PutSource refuses keys outside it — a marker for an
    /// unscoped key would never be touched by the scanner again.
    pub fn claims_key(&self, bucket: &str, object_key: &str) -> bool {
        self.core
            .buckets
            .get(bucket)
            .is_some_and(|p| p.scope.matches(object_key) && p.extractor_for(object_key).is_some())
    }

    /// Stamp a completed scan (scheduler -> GetIngestStatus).
    pub(crate) fn record_scan(&self, bucket: &str) {
        self.last_scan
            .lock()
            .unwrap()
            .insert(bucket.to_string(), marker::now_rfc3339());
    }

    /// lastScanAt of one bucket (RFC3339), if a scan ever completed.
    pub fn last_scan_at(&self, bucket: &str) -> Option<String> {
        self.last_scan.lock().unwrap().get(bucket).cloned()
    }

    /// Register a fresh reindex job for `bucket` (Reindex handler): returns
    /// the generated jobId (uuid-style hex).
    pub fn register_reindex(&self, bucket: &str, total: usize) -> String {
        let job_id = random_hex(16);
        self.jobs.lock().unwrap().insert(
            bucket.to_string(),
            ReindexJob {
                job_id: job_id.clone(),
                status: "queued",
                total,
                done: 0,
            },
        );
        job_id
    }

    /// Snapshot the latest job of one bucket.
    pub fn reindex_job(&self, bucket: &str) -> Option<ReindexJob> {
        self.jobs.lock().unwrap().get(bucket).cloned()
    }

    /// Advance the latest job of one bucket from live marker counts
    /// (GetIngestStatus is the single reader/checkpointer): `done` = markers
    /// in a terminal state (INDEXED / PARTIALLY_INDEXED / FAILED). A
    /// zero-marker job (empty/deleting-only bucket) is trivially done.
    pub fn refresh_reindex(&self, bucket: &str, done: usize) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(bucket) {
            job.done = done;
            job.status = if done >= job.total { "done" } else { "running" };
        }
    }
}

/// 32 hex chars from the OS RNG (getrandom is already a direct dep; the same
/// entropy source the LOCK owner id uses). Job ids are opaque handles, not
/// secrets — collision odds at 128 bits are irrelevant here.
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The piece of the ingest runtime the PROTOCOL face holds: `wake()` for the
/// immediate trigger (T6 `vf put` / PutSource) and `shutdown()` for the
/// SIGTERM drain, over the shared [`IngestShared`]. Cheap and inert until
/// tasks are registered.
pub struct IngestHandle {
    notify: Arc<Notify>,
    shared: Arc<IngestShared>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl IngestHandle {
    pub fn new(shared: Arc<IngestShared>) -> IngestHandle {
        IngestHandle {
            notify: Arc::new(Notify::new()),
            shared,
            tasks: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The scheduler's wake channel (shared with the runtime).
    pub fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// The shared core/registry face (`/x-vf/*` handlers).
    pub fn shared(&self) -> &Arc<IngestShared> {
        &self.shared
    }

    /// Immediate scan trigger: the scheduler wakes and runs one
    /// scan-and-diff pass right away (coalesced if already scanning).
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }

    pub(crate) fn register(&self, task: tokio::task::JoinHandle<()>) {
        self.tasks.lock().unwrap().push(task);
    }

    /// Drain: the shutdown watch already told the tasks to stop taking new
    /// work; join what finishes within `timeout`, abort the rest.
    pub async fn shutdown(&self, timeout: Duration) {
        let mut tasks: Vec<_> = std::mem::take(&mut *self.tasks.lock().unwrap());
        if tasks.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut finished = 0usize;
        while finished < tasks.len() {
            let Some(remain) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
                break;
            };
            // JoinHandle is Unpin: await through &mut so an Elapsed timeout
            // leaves us owning (and able to abort) the straggler.
            if tokio::time::timeout(remain, &mut tasks[finished])
                .await
                .is_err()
            {
                break; // budget spent
            }
            finished += 1;
        }
        let aborted = tasks.len() - finished;
        for h in tasks.iter_mut().skip(finished) {
            h.abort();
        }
        if aborted > 0 {
            eprintln!(
                "ingest: drained {finished} task(s); {aborted} aborted after the {timeout:?} \
                 shutdown budget"
            );
        } else {
            eprintln!("ingest: drained {finished} task(s)");
        }
    }
}

// ---------------------------------------------------------------------------
// tests (hermetic: tiny-embedder fixture + real .so; no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ops;

    /// tiny-embedder fixture directory (same one the ort kind tests use).
    fn fixture_dir() -> std::path::PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-embedder",
            "tests/fixtures/tiny-embedder",
        ] {
            let d = std::path::PathBuf::from(p);
            if d.join("model.onnx").is_file() {
                return d;
            }
        }
        panic!("tiny-embedder fixture not found under vf/tests/fixtures/");
    }

    /// A pipeline config: txt extractor -> tiny ort embedder, small window.
    fn tiny_config(extra: &str) -> (String, PipelineConfig) {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\nscope = \"docs/**\"\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n{}",
            fixture_dir().display().to_string(),
            extra
        );
        let cfg = crate::pipeline::config::load_pipeline_config(&text).unwrap();
        (text, cfg)
    }

    fn test_state(dir: &Path) -> Arc<AppState> {
        Arc::new(AppState {
            storage: Arc::new(Storage::file(dir)),
            creds: None,
            region: "us-east-1".into(),
            account: "000000000000".into(),
            trace: false,
            ingest: None,
        })
    }

    /// Runtime in test mode: no spawned tasks; the test drives
    /// scan_once + drain_queue deterministically. The handle over the same
    /// shared core is returned for the /x-vf face (T6).
    async fn test_runtime(
        dir: &Path,
        extra: &str,
    ) -> (Arc<IngestRuntime>, Arc<IngestHandle>, Arc<AppState>) {
        let (_, cfg) = tiny_config(extra);
        let core = IngestCore::load(&cfg, Path::new("/nonexistent-models"))
            .await
            .expect("fixture embedder loads");
        let state = test_state(dir);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            Arc::new(Notify::new()),
            shutdown_rx,
        );
        (rt, handle, state)
    }

    async fn markers(state: &AppState, vb: &str) -> Vec<SourceMarker> {
        state.storage.list_sources(vb).await.unwrap()
    }

    async fn status_of(state: &AppState, vb: &str, key: &str) -> SourceMarker {
        state
            .storage
            .load_source(vb, &marker::object_key_hash(key))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("marker for {key}"))
    }

    #[tokio::test]
    async fn full_chain_indexed_queryable_and_side_effect_free() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        // user objects land in the instance root, inside the scope
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("a.txt"),
            "the quick brown fox jumps over the lazy dog again and again",
        )
        .unwrap();
        std::fs::write(docs.join("b.txt"), "汉字测试文档，用于验证全链路摄取。").unwrap();
        // outside scope: never ingested, never marked
        std::fs::write(dir.path().join("notes.txt"), "out of scope").unwrap();
        // inside scope but no extractor: not a source (no marker, no ingest)
        std::fs::write(docs.join("pic.png"), b"\x89PNG-not-really").unwrap();

        rt.scan_once().await;
        rt.drain_queue().await;

        let ms = markers(&state, "docs").await;
        assert_eq!(
            ms.len(),
            2,
            "only in-scope extractor-handled objects: {ms:?}"
        );
        let a = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(a.status, SourceStatus::Indexed);
        assert!(a.content_hash.len() == 64);
        let b = status_of(&state, "docs", "docs/b.txt").await;
        assert_eq!(b.status, SourceStatus::Indexed);
        let a_chunks = a.chunks.as_ref().unwrap()["text"];
        let b_chunks = b.chunks.as_ref().unwrap()["text"];
        assert!(a_chunks >= 2, "69 chars over a 32-char window: {a_chunks}");
        assert!(b_chunks >= 1);

        // derived index meta is correct and stamped
        let core_cfg = tiny_config("").1;
        let plan = &rt.shared.core.buckets["docs"];
        let spec = &plan.derived["tiny"];
        assert_eq!(spec.dim, 8, "tiny fixture");
        let meta = state
            .storage
            .load_index("docs", &spec.index_name)
            .await
            .unwrap()
            .expect("derived index exists");
        assert!(meta.index_name.starts_with("tiny-"), "{}", meta.index_name);
        assert_eq!(meta.dimension, 8);
        assert_eq!(meta.distance_metric, "cosine");
        assert_eq!(
            meta.x_vf_derived.as_ref().unwrap().key,
            spec.key,
            "full derived key recorded"
        );
        // the pipeline config's own fingerprint matches the marker's
        assert_eq!(
            a.pipeline_fingerprint,
            core_cfg.fingerprint("docs").unwrap()
        );

        // ops-layer QueryVectors sees the x-vf-src-* metadata (filterable uri)
        let qvec: Vec<f64> = vec![0.5, -0.5, 0.25, 0.125, 0.0, 1.0, -1.0, 0.75];
        let body = serde_json::json!({
            "vectorBucketName": "docs",
            "indexName": spec.index_name,
            "topK": 100,
            "queryVector": { "float32": qvec },
            "returnMetadata": true,
        });
        let resp = ops::query_vectors(
            State(state.as_ref().clone()),
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await
        .expect("query ok");
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let vectors = v["vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), (a_chunks + b_chunks) as usize);
        for hit in vectors {
            let meta = &hit["metadata"];
            assert_eq!(meta["x-vf-modality"], "text");
            assert!(meta["x-vf-src-name"].as_str().unwrap().starts_with("docs/"));
            assert_eq!(
                meta["x-vf-src-uri"].as_str().unwrap().len(),
                marker::KEY_HASH_HEX_LEN
            );
            // resolved identity: an explicit model name wins over the path
            assert_eq!(meta["x-vf-model"].as_str().unwrap(), "tiny-fixture");
            assert!(meta["x-vf-chunk-text"].is_string(), "default on");
        }
        // keys follow the chunk-key format
        let a_hash = marker::object_key_hash("docs/a.txt");
        assert!(
            vectors
                .iter()
                .all(|h| h["key"].as_str().unwrap().contains("-text-"))
        );
        assert!(
            vectors
                .iter()
                .any(|h| h["key"].as_str().unwrap().starts_with(&a_hash))
        );

        // count via the store equals the markers' chunk sums (no dup/loss)
        let count = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        assert_eq!(count as u32, a_chunks + b_chunks);

        // `.vectorfs/` 外无产物: only the user's own files exist outside it
        let mut outside: Vec<String> = Vec::new();
        fn walk(p: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path.display().to_string());
                }
            }
        }
        walk(dir.path(), &mut outside);
        let user_files: Vec<String> = outside
            .iter()
            .filter(|p| !p.contains("/.vectorfs"))
            .cloned()
            .collect();
        assert_eq!(
            user_files.len(),
            4,
            "3 user docs + notes.txt stay; nothing else appears: {user_files:?}"
        );
    }

    #[tokio::test]
    async fn scheduler_smoke_background_convergence() {
        // the PRODUCTION path: spawned scheduler + workers with a 1s period
        let dir = tempfile::tempdir().unwrap();
        let (_, cfg) = tiny_config("");
        let core = IngestCore::load(&cfg, Path::new("/nonexistent-models"))
            .await
            .unwrap();
        let state = test_state(dir.path());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            handle.notify(),
            shutdown_rx,
        );
        rt.spawn(&handle);

        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("s.txt"), "scheduled scan should pick this up").unwrap();

        // poll markers until INDEXED (scan interval 1s); the marker appears
        // only after the first scan, so absence is a poll-again, not a panic
        async fn poll_indexed(state: &AppState, key: &str, secs: u64, what: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(secs);
            loop {
                let m = state
                    .storage
                    .load_source("docs", &marker::object_key_hash(key))
                    .await
                    .unwrap();
                if m.as_ref()
                    .is_some_and(|m| m.status == SourceStatus::Indexed)
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{what} did not converge: {m:?}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        poll_indexed(&state, "docs/s.txt", 30, "scheduled scan").await;
        // wake() works too: add a file, poke, expect it indexed promptly
        std::fs::write(docs.join("t.txt"), "woken ingest").unwrap();
        handle.wake();
        poll_indexed(&state, "docs/t.txt", 15, "wake").await;
        // SIGTERM-style drain: watch fires, tasks stop, shutdown joins in budget
        shutdown_tx.send(true).expect("send shutdown");
        handle.shutdown(SHUTDOWN_BUDGET).await;
    }

    #[tokio::test]
    async fn non_utf8_file_fails_with_reason_and_no_retry() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "retries = 0\n").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("bad.txt"), b"\xff\xfe\xfa not utf8 \xff").unwrap();

        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/bad.txt").await;
        assert_eq!(m.status, SourceStatus::Failed);
        assert!(m.retries_exhausted);
        let reason = m.status_reason.expect("reason");
        assert!(reason.contains("UTF-8"), "{reason}");

        // a later scan does NOT re-enqueue it (parked)
        rt.scan_once().await;
        rt.drain_queue().await;
        let m2 = status_of(&state, "docs", "docs/bad.txt").await;
        assert_eq!(m2.status, SourceStatus::Failed);
        assert_eq!(m2.updated_at, m.updated_at, "untouched by the rescan");
    }

    #[tokio::test]
    async fn dead_remote_embedder_fans_out_to_partial() {
        // dual-embedder fan-out: tiny ort (works) + openai at a dead loopback
        // port (fails fast) => PARTIALLY_INDEXED with the reason naming it
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [embedder.dead]\nkind = \"openai\"\nendpoint = \"http://127.0.0.1:9/v1\"\n\
             model = \"embedding-3\"\ndim = 8\napi_key = \"test-key\"\n\n\
             [ingest.docs]\nextract = {{ default = \"auto\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\", \"dead\"]\nretries = 0\nscan_interval_secs = 1\n",
            fixture_dir().display().to_string()
        );
        let cfg = crate::pipeline::config::load_pipeline_config(&text).unwrap();
        let core = IngestCore::load(&cfg, Path::new("/nonexistent-models"))
            .await
            .unwrap();
        let state = test_state(dir.path());
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let rt = IngestRuntime::new(
            IngestShared::new(core),
            state.clone(),
            Arc::new(Notify::new()),
            rx,
        );

        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "fan out to two embedders, one dead").unwrap();

        rt.scan_once().await;
        rt.drain_queue().await;

        let m = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m.status, SourceStatus::PartiallyIndexed);
        let reason = m.status_reason.expect("reason");
        assert!(reason.contains("dead"), "{reason}");
        assert!(m.chunks.is_some(), "the succeeded half is recorded");

        // the tiny embedder's index DID get its vectors
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        let count = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        assert_eq!(count as u32, m.chunks.as_ref().unwrap()["text"]);
        // ...and PARTIALLY is terminal for auto-retry: rescan leaves it alone
        rt.scan_once().await;
        rt.drain_queue().await;
        assert_eq!(
            status_of(&state, "docs", "docs/a.txt").await.status,
            SourceStatus::PartiallyIndexed
        );
    }

    #[tokio::test]
    async fn content_hash_skip_beats_metadata_only_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let file = docs.join("a.txt");
        std::fs::write(&file, "identical bytes will survive a rewrite").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        let m1 = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m1.status, SourceStatus::Indexed);
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        let count1 = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();

        // metadata-only rewrite: SAME bytes, new mtime => new signature
        std::fs::write(&file, "identical bytes will survive a rewrite").unwrap();
        let mtime = std::time::SystemTime::now() + Duration::from_secs(3600);
        let f = std::fs::File::options().write(true).open(&file).unwrap();
        f.set_modified(mtime).unwrap();
        drop(f);

        rt.scan_once().await;
        rt.drain_queue().await;
        let m2 = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m2.status, SourceStatus::Indexed, "skip restores INDEXED");
        assert_ne!(m2.etag_sig, m1.etag_sig, "signature was refreshed");
        assert_eq!(m2.content_hash, m1.content_hash, "same content");
        let count2 = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        assert_eq!(
            count1, count2,
            "no re-embed happened (upsert would still converge, but the point is zero work)"
        );
    }

    #[tokio::test]
    async fn changed_content_reingests_and_deletion_flips_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let file = docs.join("a.txt");
        std::fs::write(&file, "first version of the content").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        let m1 = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m1.status, SourceStatus::Indexed);

        // real content change => re-embed (chunks count may change)
        std::fs::write(&file, "second version, different and longer content here").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        let m2 = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m2.status, SourceStatus::Indexed);
        assert_ne!(m2.content_hash, m1.content_hash);
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        let count = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        assert_eq!(
            count as u32,
            m2.chunks.as_ref().unwrap()["text"],
            "converged"
        );

        // deletion => DELETING (state only; GC is T7)
        std::fs::remove_file(&file).unwrap();
        rt.scan_once().await;
        let m3 = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m3.status, SourceStatus::Deleting);
    }

    #[tokio::test]
    async fn unicode_and_special_char_keys_scan_without_phantoms() {
        // T6-found latent bug: object_store's Path canonical form percent-
        // encodes non-ASCII (and `%#*?` etc.) — the scanner must compare
        // DECODED keys against markers, or every such key phantom-creates an
        // encoded-twin marker and vanish-flips its real one each scan.
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("汉字 文档#1.txt"), "unicode content here").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/汉字 文档#1.txt").await;
        assert_eq!(m.status, SourceStatus::Indexed);

        // rescan: exactly ONE marker, still INDEXED (no encoded twin, no
        // DELETING flip)
        rt.scan_once().await;
        rt.drain_queue().await;
        let ms = markers(&state, "docs").await;
        assert_eq!(ms.len(), 1, "phantom marker appeared: {ms:?}");
        assert_eq!(ms[0].object_key, "docs/汉字 文档#1.txt");
        assert_eq!(ms[0].status, SourceStatus::Indexed);
    }

    #[tokio::test]
    async fn oversize_file_parks_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "max_file_size_mb = 1\n").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // a sparse file reports > 1 MiB without materializing it
        let file = docs.join("big.txt");
        let f = std::fs::File::create(&file).unwrap();
        f.set_len(2 * 1024 * 1024 + 1).unwrap();
        drop(f);

        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/big.txt").await;
        assert_eq!(m.status, SourceStatus::Failed);
        assert!(m.retries_exhausted);
        assert!(
            m.status_reason
                .as_deref()
                .unwrap()
                .contains("max_file_size_mb"),
            "{:?}",
            m.status_reason
        );
    }

    #[tokio::test]
    async fn fingerprint_gate_refuses_stale_markers() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        // marker written under an OLD fingerprint (simulate: write directly)
        let stale = SourceMarker {
            object_key: "docs/a.txt".into(),
            etag_sig: "s".into(),
            content_hash: "c".into(),
            pipeline_fingerprint: "deadbeefdeadbeef".into(),
            status: SourceStatus::Indexed,
            status_reason: None,
            retries: 0,
            retries_exhausted: false,
            updated_at: "t".into(),
            chunks: Some(BTreeMap::from([("text".to_string(), 1)])),
        };
        state
            .storage
            .save_source("docs", &marker::object_key_hash("docs/a.txt"), &stale)
            .await
            .unwrap();

        let (_, cfg) = tiny_config("");
        let err = fingerprint_gate(&state.storage, &cfg).await.unwrap_err();
        assert!(err.contains("fingerprint mismatch"), "{err}");
        assert!(
            err.contains("vf reindex"),
            "the remedy must be explicit: {err}"
        );
        assert!(err.contains("docs/a.txt"), "{err}");

        // after wiping the markers the gate passes
        state
            .storage
            .delete_source("docs", &marker::object_key_hash("docs/a.txt"))
            .await
            .unwrap();
        assert!(fingerprint_gate(&state.storage, &cfg).await.is_ok());
    }

    #[tokio::test]
    async fn in_progress_crash_residue_recovers_without_lock() {
        // an IN_PROGRESS marker whose object is UNCHANGED (the kill -9 shape)
        // is re-enqueued by the next scan and converges
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("a.txt"),
            "content that was mid-flight when we died",
        )
        .unwrap();

        // scan creates PENDING; simulate the crash BEFORE the job runs
        rt.scan_once().await;
        let mut m = status_of(&state, "docs", "docs/a.txt").await;
        m.status = SourceStatus::InProgress;
        state
            .storage
            .save_source("docs", &marker::object_key_hash("docs/a.txt"), &m)
            .await
            .unwrap();
        // next scan: unchanged sig + non-terminal -> re-enqueued
        rt.scan_once().await;
        rt.drain_queue().await;
        assert_eq!(
            status_of(&state, "docs", "docs/a.txt").await.status,
            SourceStatus::Indexed
        );
    }

    #[tokio::test]
    async fn batch_splitting_respects_both_limits() {
        let mk = |i: usize, meta_len: usize| BatchRow {
            key: format!("k{i}"),
            vector: vec![0.5; 8],
            metadata: serde_json::from_str(&format!(
                "{{\"x-vf-src-uri\":\"{}\"}}",
                "u".repeat(meta_len)
            ))
            .unwrap(),
        };
        // 500-row cap
        let batches = split_batches((0..1200).map(|i| mk(i, 8)).collect(), 8);
        assert_eq!(
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
            vec![500, 500, 200]
        );
        // byte cap: rows of ~12 MiB estimated JSON cannot pair up (the
        // 19 MiB budget fits at most one)
        let big = (0..5).map(|i| mk(i, 12 * 1024 * 1024)).collect::<Vec<_>>();
        let batches = split_batches(big, 8);
        assert!(
            batches.iter().all(|b| b.len() <= 1),
            "{:?}",
            batches.iter().map(|b| b.len()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn store_chunk_text_false_omits_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "store_chunk_text = false\n").await;
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), "no chunk text should be stored for me").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        assert_eq!(
            status_of(&state, "docs", "docs/a.txt").await.status,
            SourceStatus::Indexed
        );
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        // scan the table for any x-vf-chunk-text metadata
        let found = state
            .storage
            .scan_vectors("docs", &spec.index_name, None)
            .await
            .unwrap();
        assert!(!found.is_empty());
        for v in &found {
            let meta = v.metadata.as_ref().expect("metadata present");
            assert!(meta.get("x-vf-chunk-text").is_none(), "{meta:?}");
            assert!(meta.get("x-vf-src-uri").is_some());
        }
    }
}
