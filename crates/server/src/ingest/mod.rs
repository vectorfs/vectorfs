//! Ingest worker — the file layer's fixed ORCHESTRATION mechanism
//! (design.md §5.10.3 data flow, §5.10.5 robustness; M2 T3 + T7 GC/reindex).
//!
//! # Module structure
//!
//! * [`marker`] — Source-marker SEMANTICS: no-queue migration rules, key
//!   hashing, chunk-key shape, cheap change signatures, scope matching.
//!   Pure functions.
//! * [`domain`] — derived-index identity (`hash(model, dim, embedder
//!   config)`) and the idempotent bucket/index creation + validation.
//! * [`gc`] — the T7 reconciliation reapers (DELETING reaps + idle-gated
//!   orphan sweep) and the reindex chunk-clearing they share.
//! * this module — the RUNTIME: [`IngestCore`] (config + loaded embedders),
//!   [`IngestRuntime`] (scheduler + workers + queue), [`IngestHandle`]
//!   (wake/drain handle the protocol face and `vectorfs serve` hold).
//!
//! # Extractor fallback chains (M2.1)
//!
//! An `extract` route may name an ORDERED chain of extractor instances
//! (`extract = { pdf = ["pdf-inspector", "pdf-text"] }`): content one
//! member cannot handle (its Err or empty output — e.g. pdf-inspector
//! meeting a scan) falls to the next member better suited to it, instead
//! of failing or being勉强处理. The chain is INGEST ORCHESTRATION on top
//! of the unchanged [`Extractor`](crate::pipeline::Extractor) trait — a
//! list, not a conditional/DAG (§5.10.8 stands); the decision core is the
//! pure [`pick_from_chain`].
//!
//! ## Smart routing (M2.1 step 2: probe → affinity → route)
//!
//! Pure order has two blind spots: a MIS-ORDERED chain is a quality
//! accident waiting to happen (an OCR-first chain would OCR native text —
//! strictly worse output the chain then accepts), and members discover
//! their suitability only by burning an attempt. Step 2 adds three
//! MECHANISM stages, zero config surface (§5.10.8 still stands — no
//! routing table, no conditionals in config; the config-ordered chain
//! itself remains the correctness backstop):
//!
//! 1. **probe** — [`pipeline::probe::probe`] classifies the bytes into a
//!    [`ContentClass`] before any extractor runs (pdf family only; probe
//!    errors read as Unknown so cross-parser recovery survives).
//! 2. **affinity** — each kind DECLARES its fit per class
//!    ([`Extractor::affinity`](crate::pipeline::Extractor::affinity));
//!    class-level and static, never a knob.
//! 3. **route** — the pure [`route`] below: first Preferred in config
//!    order, else first Capable, else all-refuse ⇒ the file FAILED with an
//!    aggregate reason (more informative than running everyone and
//!    failing). Routing only decides WHO RUNS FIRST — a routed member that
//!    still yields falls back to the remaining members in config order, so
//!    the chain remains the correctness兜底 and every M2.1 chain semantic
//!    (yield-on-Err/empty, first-win, single-member passthrough) is
//!    preserved. Routing is mechanism, not configuration: it does NOT
//!    enter the pipeline fingerprint (code-version drift semantics are
//!    §5.10.5's existing stance), and single-member chains skip the probe
//!    entirely (`members.len() > 1` is the zero-cost gate).
//!
//! ## Normalize preprocessing (M2.1 step 3, generalized ruling)
//!
//! Preprocessing is more than decompression — it is a STAGE: one fixed,
//! ORDERED table of built-in byte-level transforms
//! ([`pipeline::normalize::normalize`]; M2: gunzip) that every document —
//! and every container child, recursively — runs through BEFORE extractor
//! routing. The layering inside one CPU segment:
//!
//! ```text
//! normalize(bytes) → terminal state
//!   → short key from the POST-normalize mime → chain
//!   → probe → affinity → route (step 2, unchanged)
//!   → member: unpack? container children re-enter THIS flow (depth-guarded)
//!             : else extract (leaf, bytes unchanged since normalize)
//! ```
//!
//! * **Err is hard, and parks**: the chain's members yield on Err; the
//!   normalize stage has NO fallback — a corrupt gzip is a deterministic
//!   dead end (retrying cannot uncorrupt it), so the document parks with
//!   the reason. At CHILD level the same Err is a reason on the container's
//!   aggregate — the container keeps walking its remaining children.
//! * **Amplification gate**: the SAME `max_file_size_mb` is enforced on the
//!   POST-normalize bytes (document: park; child: skip with reason) — a
//!   decompression bomb may not smuggle past the file-size guard what the
//!   scanner would have refused outright.
//! * **Claiming cooperates**: a `.gz`/`.gzip` suffix is a compression
//!   WRAPPER, not a content type — the scan-time extension guess sees
//!   through one wrapper layer, so `notes.txt.gz` is claimed as the
//!   text/plain source it decompresses into, under the user's existing
//!   route, zero configuration.
//! * **Fingerprint: zero impact** — the stage is mechanism, not
//!   configuration (the same §5.10.5 stance that keeps probe/route out).
//!
//! ## Container unpacking (M2.1 step 3)
//!
//! A kind is either a LEAF or a CONTAINER
//! ([`Extractor::unpack`](crate::pipeline::Extractor::unpack); default
//! `Ok(None)` = leaf — the four M2.1 production kinds are all leaves, and
//! their path is byte-identical to before). After routing picks the member
//! order, each member first UNPACKS: `Some(children)` ⇒ every child
//! re-enters the USER-CONFIGURED routing — the child's POST-NORMALIZE mime
//! resolves to a chain in this SAME ingest config ([`BucketPlan::child_routes`]),
//! which then runs with the full probe → affinity → yield semantics — and
//! the winning Parts aggregate in child order (a container never digests
//! its own children; the operator's per-mime chains apply inside archives
//! automatically — the Tika CompositeParser/PackageParser shape, stronger
//! than unstructured's hardcoded delegation). `None` ⇒ the ordinary
//! `extract` leaf path. Unpack semantics mirror the chain's: an unpack Err
//! (mime gate, bad archive) is the member's YIELD signal. Children that
//! find no chain, or whose chain yields, contribute ZERO Parts and their
//! reason rides the aggregate; if NO child produces content the container
//! member itself yields with that aggregate (fail-loud survives verbatim).
//! Deterministic by construction: child order is the kind's return order
//! (e.g. spine order), the whole segment is sequential. Bomb guards are
//! MECHANISM CONSTANTS ([`MAX_CONTAINER_DEPTH`], [`MAX_CHILDREN`], and the
//! existing per-file size cap applied per child, raw AND post-normalize) —
//! a guardrail is not policy, so none of them is a knob (§5.10.8's spirit).
//!
//! # Execution model (§5.10.5)
//!
//! One SCHEDULER task per instance root: periodic scan (`scan_interval_secs`,
//! default 30) + external wake ([`IngestHandle::wake`] — T6's `vf put` /
//! PutSource immediate trigger) + shutdown, `select`ed together. It holds a
//! bounded mpsc (capacity 64) — scan-and-diff pushes jobs, a small set of
//! WORKER tasks (one per CPU) consumes them. CPU segments (extract / chunk /
//! ort embed) run through `spawn_blocking` behind a semaphore sized to the
//! CPU count; remote (openai) embeds are I/O-bound and skip the gate. Each
//! scan pass ends with the T7 GC pass ([`IngestRuntime::gc_pass`]): DELETING
//! sources reaped immediately, orphans swept only when idle.
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
pub mod gc;
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
use crate::pipeline::config::{ExtractValue, PipelineConfig};
use crate::pipeline::embed::LoadedEmbedder;
use crate::pipeline::extract::{
    ExtractorKind,
    md::{MdConfig, MdTag},
    pdf_text::{PdfTextConfig, PdfTextTag},
    txt::{TxtConfig, TxtTag},
};
use crate::pipeline::normalize::{NormalizeOutcome, normalize};
use crate::pipeline::{
    Chunk, Chunker, ContentClass, Embedder, Extractor, Fit, Merge, Modality, Part, PipelineError,
};
use crate::store::{SourceMarker, SourceStatus, Storage};

use domain::DerivedSpec;
use marker::ClaimFilter;

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
/// startup — no silent background re-embedding engine exists by design. The
/// remedy is EXPLICIT re-embedding (M2 T7): `vectorfs serve --reindex`
/// re-embeds every drifted source at startup (the flag skips this gate);
/// `vf reindex --bucket <b>` forces a re-embed of a RUNNING daemon whose
/// config did NOT change.
pub async fn fingerprint_gate(
    storage: &Storage,
    cfg: &PipelineConfig,
    data_dir: &Path,
) -> Result<(), String> {
    for (vb, _) in cfg.ingest.iter().filter(|(_, i)| !i.embedders.is_empty()) {
        // A vb whose SYSTEM bucket does not exist yet (fresh s3 deployment:
        // vectorfs.toml names [ingest.X] before CreateVectorBucket ever ran)
        // has no source markers to drift — the gate passes trivially. On the
        // file backend a missing bucket lists as empty, but S3 answers
        // ListObjects with 404 NoSuchBucket, which would otherwise refuse
        // startup on every fresh endpoint deployment (G2 s3 full-path
        // finding, 2026-11-06). The startup lock pass + scan both tolerate
        // the missing bucket already (§5.1 skip; scan's no-listing guard).
        if !storage
            .system_bucket_exists(vb)
            .await
            .map_err(|e| format!("reading source markers of bucket {vb}: {e}"))?
        {
            continue;
        }
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
                     Refusing to start. Re-embed explicitly: restart with `vectorfs serve \
                     --reindex --data-dir {}` (clears old chunks, re-embeds every drifted \
                     source at startup, then serves normally) — or, for a forced re-embed of a \
                     RUNNING daemon whose config did not change, `vf reindex --bucket {vb}`.",
                    m.object_key,
                    m.pipeline_fingerprint,
                    data_dir.display()
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
    /// In-bucket claim filter (R5): include/exclude globs; empty = whole
    /// bucket. The source bucket IS the vb's namespace — there is no
    /// cross-bucket scope anymore.
    pub claim: ClaimFilter,
    /// raw extract routing: mime short key (or `default`) -> instance name,
    /// the `auto` sentinel, or an ordered chain of instance names (M2.1;
    /// see [`crate::pipeline::config::ExtractValue`]).
    extract: BTreeMap<String, ExtractValue>,
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
    /// The ordered extractor CHAIN this bucket routes `object_key` through
    /// (M2.1 linear fallback), as (member name, kind) pairs — the name is
    /// what the aggregated statusReason quotes ("auto" names the sentinel's
    /// one-member chain, instance names name themselves). `None` means the
    /// object is NOT a source of this pipeline (unknown/absent extension
    /// with no matching extract entry): no marker, no ingest (§5.10.1:
    /// sources are the objects this pipeline claims).
    ///
    /// Every configured member resolved at PARSE time (config validation
    /// guarantees chain members are declared instances), so a missing entry
    /// here can only mean "routing changed under a running process" — the
    /// safe answer is None (not a source), same as pre-M2.1.
    fn extractor_chain_for(&self, object_key: &str) -> Option<Vec<(String, ExtractorKind)>> {
        let short = short_key_of(object_key)?;
        let value = self
            .extract
            .get(short)
            .or_else(|| self.extract.get("default"))?;
        self.resolve_chain(value, short)
    }

    /// One extract-table VALUE -> its member list (the shared body of the
    /// object- and child-level chain lookups; `short_key` is the KEY the
    /// value was found under — the `auto` sentinel resolves against it).
    fn resolve_chain(
        &self,
        value: &ExtractValue,
        short_key: &str,
    ) -> Option<Vec<(String, ExtractorKind)>> {
        match value {
            ExtractValue::Single(s) => match s.as_str() {
                // auto stays the built-in mime->kind single-member chain
                // (M3 ruling pending: whether auto itself becomes a chain
                // when pdf-ocr lands — see builtin_extractor).
                "auto" => builtin_extractor(short_key).map(|k| vec![(s.clone(), k)]),
                name => self
                    .extractors
                    .get(name)
                    .map(|k| vec![(name.to_string(), k.clone())]),
            },
            // array form: the declared order IS the try order
            ExtractValue::Chain(names) => Some(
                names
                    .iter()
                    .map(|n| self.extractors.get(n).map(|k| (n.clone(), k.clone())))
                    .collect::<Option<Vec<_>>>()?,
            ),
        }
    }

    /// The child-routing snapshot for one CPU segment (container unpacking,
    /// M2.1 step 3): the SAME extract table the document level routes
    /// through, pre-resolved per mime short key so the `spawn_blocking`
    /// segment can walk it without borrowing the plan. Children have no
    /// filename to guess an extension from — the mime IS their identity: a
    /// table mime routes by its short key, an OFF-table mime falls to the
    /// `default` route (the user's catch-all, or nothing).
    fn child_routes(&self) -> ChildRoutes {
        let mut routes = ChildRoutes::default();
        for (_, key) in crate::pipeline::extract::MIME_TO_SHORT_KEY {
            // the same specific-then-default precedence the object level
            // applies for a known extension
            let value = self
                .extract
                .get(*key)
                .or_else(|| self.extract.get("default"));
            if let Some(chain) = value.and_then(|v| self.resolve_chain(v, key)) {
                routes.per_key.insert(key, chain);
            }
        }
        // off-table mimes (and mimes under a bare `default`): `auto` cannot
        // serve them — the built-in map is keyed by short key — so a
        // default of "auto" resolves to nothing here, deliberately.
        routes.default_chain = self
            .extract
            .get("default")
            .and_then(|v| self.resolve_chain(v, "default"));
        routes
    }
}

/// Pre-resolved child routing (see [`BucketPlan::child_routes`]). Cheap by
/// construction: a handful of small member lists cloned once per extract
/// segment (only when the chunk cache is cold).
#[derive(Default)]
struct ChildRoutes {
    /// short key -> chain, for every table key that resolves (specific
    /// entry, else the default route).
    per_key: BTreeMap<&'static str, Vec<(String, ExtractorKind)>>,
    /// the default route's chain, for off-table mimes.
    default_chain: Option<Vec<(String, ExtractorKind)>>,
}

impl ChildRoutes {
    /// The chain one CHILD re-enters, by its mime: table mimes by short key
    /// (specific entry, else default — the object-level precedence), off-table
    /// mimes straight to `default`. `None` = no chain carries this child.
    fn chain_for_mime(&self, mime: &str) -> Option<&[(String, ExtractorKind)]> {
        match crate::pipeline::extract::short_key_for_mime(mime) {
            Some(key) => self.per_key.get(key).map(Vec::as_slice),
            None => self.default_chain.as_deref(),
        }
    }
}

/// Extension -> mime short key (the scan-time mime guess; the canonical mime
/// then comes from the short key, so extractor mime gates always agree).
fn short_key_of(object_key: &str) -> Option<&'static str> {
    let file = object_key.rsplit('/').next()?;
    // A compression wrapper is MECHANISM (the normalize stage), not a
    // content type: the guess sees through ONE `.gz`/`.gzip` layer, so
    // `notes.txt.gz` is claimed and routed as the text/plain source it
    // decompresses into — under the user's EXISTING txt route, zero
    // configuration. The worker's normalize stage then makes the bytes
    // match the guess before any extractor runs (a misnamed plain file
    // under `.gz` degrades gracefully: no magic ⇒ no transform ⇒ the
    // plain route still reads it).
    let file = crate::pipeline::normalize::unwrap_compression(file);
    let ext = file.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        // M2.1 step 3 TEST SCAFFOLD: the container mechanism is proven with a
        // cfg(test) kind, and a container format needs its own short key to
        // be claimable as a source at all. The production epub/html keys land
        // with the real M3 kinds + the mime-table rows they bring; this arm
        // exists only under cfg(test) and ships in no production binary.
        #[cfg(test)]
        "ctn" => Some("ctn"),
        // M3-T1 TEST SCAFFOLD, same ruling: the image modality mechanism is
        // proven with a cfg(test) kind; the production png/jpeg family keys
        // land with the real T2/T3 image kinds and their mime-table rows.
        #[cfg(test)]
        "img" => Some("img"),
        _ => crate::pipeline::extract::short_key_for_extension(&ext),
    }
}

/// The `auto` mapping's default extractor KIND per short key (kept in lock
/// step with [`MIME_TO_SHORT_KEY`]). `pdf` deliberately stays `pdf-text`
/// even though two further PDF kinds (`pdf-inspector`, and `pdf-pages` since
/// M3-T3) exist: pdfium text extraction is the battle-tested path and `auto`
/// is an OPINIONATED default — a new kind landing must not silently
/// re-route every `extract = { default = "auto" }` deployment (image-output
/// `pdf-pages` doubly so: it would change the pipeline's modality needs).
/// The `img` family (M3-T4) stays OUT of `auto` for exactly that second
/// reason: an auto arm would make every existing `auto` deployment start
/// claiming image files into pipelines whose embedders are text-only.
/// Deployments that want the classify-first, no-.so engine, the page-image
/// route or image files opt in explicitly (`extract = { pdf =
/// "inspector-instance" }`, `pdf = "pages-instance"`, `img =
/// "image-instance"`).
/// M2.1 keeps `auto` a single-member chain: whether it should become a
/// built-in chain (pdf → pdf-inspector → pdf-text, say) is an M3 ruling,
/// taken when `pdf-ocr` actually exists to chain onto.
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
// M2.1: the linear extractor fallback chain (pure decision core)
// ---------------------------------------------------------------------------

/// Does an `Ok` extraction carry content? "Non-empty" means at least one
/// Part with a content channel that is not blank: text whose trimming
/// leaves nothing counts as no content (the same bar every built-in kind's
/// own "no extractable text" refusal uses), and an image payload IS content
/// regardless of the (always empty) text channel (M3-T1: an image-only
/// extraction must WIN its chain, not yield as "empty").
fn has_extracted_content(parts: &[Part]) -> bool {
    !parts.is_empty()
        && parts
            .iter()
            .any(|p| p.image.is_some() || !p.text.trim().is_empty())
}

/// The inner message of an extractor error, without the `PipelineError`
/// Display prefix (the chain aggregate adds ONE prefix of its own; member
/// reasons must not arrive pre-prefixed or the aggregate would say "extract
/// failed" once per member).
fn inner_message(e: &PipelineError) -> String {
    match e {
        PipelineError::Extract(m) => m.clone(),
        other => other.to_string(),
    }
}

/// Pick the winning extraction off an ORDERED fallback chain (M2.1). The
/// pure decision core of the chain mechanism — unit-tested below; the
/// worker only supplies the members and executes.
///
/// `members` yields `(name, outcome)` pairs LAZILY: the iterator is only
/// advanced past a member when that member YIELDED, which is what makes
/// "first non-empty Ok wins — later members never run" true in production
/// (the worker maps `extract` over the kinds; a member after the winner is
/// never pulled, never executed).
///
/// Chain semantics (uniform rules, no new error type — an extractor's Err
/// or empty output IS its yield signal, so the [`Extractor`] trait stays
/// untouched and routing intelligence keeps living inside the kinds):
///
/// * `Ok(parts)` WINS iff [`has_extracted_content`]; an `Ok` of nothing
///   (no Parts, all-blank text) YIELDS, exactly like an Err — "cannot
///   handle this content" and "handled it into nothing" both mean "give it
///   to the next member".
/// * the FIRST win ends the chain and its Parts are THE result (no
///   cross-member merging — a merged Frankenstein document is worse than
///   either engine's honest output); the winner's NAME rides along for the
///   progress line (`INDEXED via {name}`).
/// * full exhaustion fails the chain: the message aggregates every
///   member's yield reason as `{mime短键} chain [{a→b}]: {a}: {ra}; {b}: {rb}`
///   so the fail-loud contract survives (a blank/scanned PDF with no OCR
///   relay is still a visible FAILED with every reason on record).
///   A ONE-member chain (every pre-M2.1 config, and `auto`) passes the
///   member's own message through verbatim — the wrapper describes a
///   chain, and there is nothing to describe with one member; this also
///   keeps every existing failure message byte-stable.
fn pick_from_chain<I>(short_key: &str, members: I) -> Result<(String, Vec<Part>), PipelineError>
where
    I: IntoIterator<Item = (String, Result<Vec<Part>, PipelineError>)>,
{
    let mut names: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    for (name, outcome) in members {
        names.push(name.clone());
        match outcome {
            Ok(parts) if has_extracted_content(&parts) => {
                return Ok((name, parts));
            }
            Ok(_) => reasons.push(
                "produced no extractable content (empty output — refusing to index \
                     nothing)"
                    .to_string(),
            ),
            Err(e) => reasons.push(inner_message(&e)),
        }
    }
    // exhaustion (a validated chain is never empty, so names is never empty
    // here in production; the pure function still handles it as a trivial
    // exhaustion)
    if names.len() <= 1 {
        let reason = reasons.into_iter().next().unwrap_or_default();
        return Err(PipelineError::extract(reason));
    }
    let aggregate: Vec<String> = names
        .iter()
        .zip(reasons)
        .map(|(n, r)| format!("{n}: {r}"))
        .collect();
    Err(PipelineError::extract(format!(
        "{short_key} chain [{}]: {}",
        names.join("→"),
        aggregate.join("; ")
    )))
}

// ---------------------------------------------------------------------------
// M2.1 step 2: probe → affinity → route (pure decision core)
// ---------------------------------------------------------------------------

/// What the router decided for one chain against one (possibly unknown)
/// content class. `Run`/`Order` carry the MEMBER INDICES to try, in try
/// order; `Refused` carries the aggregate refusal message (the file FAILED
/// without anyone executing — more informative than running the whole chain
/// into guaranteed failures).
#[derive(Debug, PartialEq)]
enum Routing {
    /// A member was selected by affinity (Preferred, else first Capable):
    /// run it FIRST; the remaining members follow by AFFINITY TIER (see
    /// [`chain_try_order`]). Carries the class — the tail tiering needs it,
    /// and `first` + `class` is exactly what the progress line reports.
    Run { first: usize, class: ContentClass },
    /// Unknown class (no probe for the family, or the probe itself could
    /// not parse the bytes): the full chain in config order — the pure
    /// M2.1 semantics, cross-parser recovery intact.
    Order(Vec<usize>),
    /// Every member declares [`Fit::Refuses`] for the probed class: nobody
    /// runs; the message names the class and every member's refusal reason.
    Refused(String),
}

/// The routing decision core (M2.1 step 2). Pure, deterministic, and
/// unit-tested below; the worker supplies the chain (config order) and the
/// probe's class verdict.
///
/// Deterministic pass structure over the CONFIG order (a list scan, not an
/// engine — §5.10.8's ban stands):
///
/// * `class = None` ⇒ [`Routing::Order`] with every member, config order —
///   bit-for-bit the pre-routing chain semantics.
/// * `class = Some(c)` ⇒ pass 1 takes the FIRST member whose affinity is
///   [`Fit::Preferred`] (config order is the tie-break: two equally-right
///   tools resolve to the one the operator listed first); pass 2 (no
///   Preferred anywhere) takes the FIRST [`Fit::Capable`]; all-Refuses ⇒
///   [`Routing::Refused`] with the aggregate reason (remedy phrased by the
///   class — [`ContentClass::remedy`]).
///
/// NOTE: routing accelerates and protects quality; it does not REPLACE the
/// chain. [`Routing::Run`] only reorders who is tried first — see
/// [`chain_try_order`].
fn route(
    short_key: &str,
    members: &[(String, ExtractorKind)],
    class: Option<ContentClass>,
) -> Routing {
    let Some(class) = class else {
        return Routing::Order((0..members.len()).collect());
    };
    if let Some(i) = members
        .iter()
        .position(|(_, kind)| kind.affinity(class) == Fit::Preferred)
    {
        return Routing::Run { first: i, class };
    }
    if let Some(i) = members
        .iter()
        .position(|(_, kind)| kind.affinity(class) == Fit::Capable)
    {
        return Routing::Run { first: i, class };
    }
    // everyone refuses: say so WITHOUT executing anyone — the class names
    // the content, each member's declaration says why it will not serve it,
    // and the remedy names the missing capability, phrased by the CLASS
    // (a scan needs OCR; a blank document needs nothing any extractor has).
    let refusals: Vec<String> = members
        .iter()
        .map(|(name, kind)| format!("{name} refuses ({})", kind.refusal_note(class)))
        .collect();
    Routing::Refused(format!(
        "{short_key} class={class}: {} — {}",
        refusals.join("; "),
        class.remedy()
    ))
}

/// Affinity rank for tail tiering (lower = tried earlier).
fn fit_rank(fit: Fit) -> u8 {
    match fit {
        Fit::Preferred => 0,
        Fit::Capable => 1,
        Fit::Refuses => 2,
    }
}

/// The EXECUTION order a routing verdict implies (hardening revision,
/// 2026-11-05: the tail is now AFFINITY-TIERED, superseding the original
/// "remaining members in config order" ruling — recorded in design.md §0):
///
/// * `Run { first, class }` ⇒ `first` first (the routed member), then the
///   remaining members sorted by their declared affinity for `class`
///   (Preferred, then Capable, then Refuses), CONFIG ORDER STABLE WITHIN a
///   tier. Rationale: the tail is the correctness backstop that runs only
///   if the routed member yields — a member DECLARING Refuses for this
///   class running before a Capable one burns an attempt on the most
///   likely-to-fail member and, worse, risks its degraded output WINNING
///   ("first non-empty Ok wins") over the Capable member's better one.
///   Tiering the tail extends "mis-order is a quality accident" (the
///   probe→affinity→route motivation) from who runs FIRST to the whole
///   try order. Refuses members still RUN (extract-time behavior, not the
///   class declaration, remains the last word on cross-parser recovery) —
///   just last.
/// * `Order` ⇒ verbatim (Unknown class ⇒ pure config order, byte-stable
///   pre-routing semantics).
/// * `Refused` ⇒ nobody executes; an empty order makes the (unused) chain
///   walk a trivial exhaustion instead of a panic.
fn chain_try_order(members: &[(String, ExtractorKind)], routing: &Routing) -> Vec<usize> {
    match routing {
        Routing::Run { first, class } => {
            let mut order = vec![*first];
            let mut tail: Vec<usize> = (0..members.len()).filter(|j| j != first).collect();
            // sort_by_key is stable: config order survives within a tier
            tail.sort_by_key(|j| fit_rank(members[*j].1.affinity(*class)));
            order.extend(tail);
            order
        }
        Routing::Order(order) => order.clone(),
        Routing::Refused(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// M3-T1: doc-side modality routing (§5.10.1 fan-out gate, pure decision core)
// ---------------------------------------------------------------------------

/// The doc-side modality gate of the fan-out (§5.10.1: a Part modality fans
/// out to EVERY accepting embedder — and to NOBODY when no listed embedder
/// accepts it). Pure decision core, unit-tested below; the worker parks the
/// file with the returned aggregate reason (a deterministic dead end:
/// retrying cannot change a declared modality — the same shape as the
/// all-refuse route verdict).
///
/// `None` (covered) when every modality present in the chunk set is accepted
/// by at least one fanned-out embedder, OR when the fan-out list is empty —
/// the pure-protocol face (no embedders listed) embeds nothing by design and
/// skips this gate exactly like the config-time carry-over check does
/// (§5.10.3).
///
/// The refusal message mirrors the all-refuse chain aggregate: it counts the
/// stranded chunks by modality, lists what every listed embedder DOES accept
/// (so the mismatch is visible without opening the config), and quotes the
/// modality's remedy ([`Modality::remedy`] — phrased BY the modality, the
/// [`ContentClass::remedy`] precedent).
fn modality_gap_reason(chunks: &[Chunk], fanned: &[(&String, &LoadedEmbedder)]) -> Option<String> {
    if fanned.is_empty() {
        return None;
    }
    // BTreeSet: enum order (Text < Image), deterministic message
    let uncovered: BTreeSet<Modality> = chunks
        .iter()
        .map(Chunk::modality)
        .filter(|m| !fanned.iter().any(|(_, e)| e.doc_modalities().contains(m)))
        .collect();
    if uncovered.is_empty() {
        return None;
    }
    let gaps: Vec<String> = uncovered
        .iter()
        .map(|m| {
            let n = chunks.iter().filter(|c| c.modality() == *m).count();
            format!("{n} {m} chunk(s)")
        })
        .collect();
    let roster: Vec<String> = fanned
        .iter()
        .map(|(name, e)| {
            format!(
                "{name} (doc modalities: {})",
                e.doc_modalities()
                    .iter()
                    .map(Modality::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    let remedies: Vec<&str> = uncovered.iter().map(|m| m.remedy()).collect();
    Some(format!(
        "{} with no embedder that accepts them on its doc side — listed embedder(s): {} — {}",
        gaps.join("; "),
        roster.join("; "),
        remedies.join("; ")
    ))
}

/// Per-modality chunk counts for the completion marker. `text` is always
/// present (byte-stable with the pre-image marker shape); other modalities
/// appear only when they have chunks — a `("image", 0)` row on every
/// text-only source would churn every existing marker for no information.
fn chunk_counts(chunks: Option<&[Chunk]>) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::from([("text".to_string(), 0u32)]);
    if let Some(chunks) = chunks {
        for c in chunks {
            *counts.entry(c.modality().to_string()).or_insert(0) += 1;
        }
    }
    counts
}

/// What the extract segment reports upward for the progress line: the
/// member that actually WON and the probe class, present only when routing
/// engaged (multi-member chain + a class verdict).
struct RouteNote {
    winner: String,
    class: ContentClass,
}

// ---------------------------------------------------------------------------
// M2.1 step 3: container unpacking (children re-enter USER routing)
// ---------------------------------------------------------------------------

/// Bomb guard: how deep CONTAINERS may nest. Depth counts from the document
/// (a document-level container unpacks at 0, its children walk at 1, a child
/// container at 1 unpacks grandchildren at 2); a container reached at depth
/// 2 — the THIRD container layer — Errs outright, before its children are
/// touched. Two layers cover the real world (an .eml attachment that is
/// itself an .eml; an epub never nests zip-in-zip legitimately) while making
/// zip-bomb nesting structurally impossible. A MECHANISM CONSTANT,
/// deliberately not config: a guardrail is not policy — a knob saying how
/// much bomb to tolerate would be a policy (§5.10.8's spirit).
const MAX_CONTAINER_DEPTH: u32 = 2;

/// Bomb guard: one container may unpack at most this many children. Exceeding
/// it Errs the CONTAINER (naming the entry count) — the member yields like
/// any Err and the chain may still recover; the file fails loudly if nothing
/// catches it. 256 sits far above every real-world container at hand (epub
/// spines, docx parts, eml attachments) while capping the fan-out a hostile
/// archive can demand from one document. Mechanism constant — same ruling
/// as [`MAX_CONTAINER_DEPTH`].
const MAX_CHILDREN: usize = 256;

/// One stderr line per normalize HIT (M2.1 step 3 observability):
/// `ingest: normalize: docs/a.txt.gz -> gunzip (text/plain)`. Pure
/// passthrough (nothing applied) logs nothing — the common leaf path stays
/// silent. `display` is whatever the caller already shows (object key at
/// document level, `parent!child` below it).
fn log_normalize(display: &str, outcome: &NormalizeOutcome) {
    if !outcome.applied.is_empty() {
        eprintln!(
            "ingest: normalize: {display} -> {} ({})",
            outcome.applied.join(" + "),
            outcome.mime
        );
    }
}

/// Run ONE chain member over ONE document or child (M2.1 step 3): a LEAF
/// extracts (the pre-container path, byte-for-byte); a CONTAINER unpacks and
/// re-routes every child through the USER-CONFIGURED chains
/// ([`ChildRoutes`]), aggregating the winning Parts in child order. Each
/// child is NORMALIZED first (the same fixed transform table the document
/// ran through) — its chain resolves by the POST-normalize mime.
///
/// The member outcome keeps the chain's uniform yield vocabulary: an unpack
/// Err (mime gate, bad archive), a guard trip, or "no child produced
/// content" all arrive as `Err(reason)` — [`pick_from_chain`] records them,
/// falls to the next member, and the eventual document FAILED carries the
/// child-level reasons in its aggregate.
fn run_member(
    routes: &ChildRoutes,
    max_child_bytes: u64,
    display: &str,
    name: &str,
    kind: &ExtractorKind,
    bytes: &[u8],
    mime: &str,
    depth: u32,
) -> Result<Vec<Part>, PipelineError> {
    let children = match kind.unpack(bytes, mime) {
        // leaf: plain extract, exactly the pre-container member behavior
        Ok(None) => return Extractor::extract(kind, bytes, mime),
        // container: children RE-ENTER the user's routing below
        Ok(Some(children)) => children,
        // unpack Err = the member's yield signal (mime gate / bad archive),
        // the same semantics a leaf's extract Err has
        Err(e) => return Err(e),
    };
    // ---- bomb guards (before any child work) ----
    if depth >= MAX_CONTAINER_DEPTH {
        return Err(PipelineError::extract(format!(
            "container '{name}' sits at nesting depth {} (MAX_CONTAINER_DEPTH = \
             {MAX_CONTAINER_DEPTH}): the third container layer is refused outright — a bomb \
             guard, not a policy knob",
            depth + 1
        )));
    }
    if children.len() > MAX_CHILDREN {
        return Err(PipelineError::extract(format!(
            "container '{name}' unpacked {} children (MAX_CHILDREN = {MAX_CHILDREN}) — the \
             expansion is refused; if the archive is legitimate, split it",
            children.len()
        )));
    }
    // observability: one line per expansion (stderr is this codebase's
    // progress channel; `display` is the object key at depth 0 and
    // `parent!child` below it)
    eprintln!(
        "ingest: {display} -> container({name}) x {} children",
        children.len()
    );
    let mut parts: Vec<Part> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for child in &children {
        let child_display = format!("{display}!{}", child.name);
        // per-child size cap: the SAME max_file_size_mb the object level
        // enforces — an archive may not smuggle past the file-size guard
        // what the scanner would have refused outright
        if child.bytes.len() as u64 > max_child_bytes {
            skipped.push(format!(
                "'{}': {} bytes exceeds max_file_size_mb ({} MiB) — child skipped",
                child.name,
                child.bytes.len(),
                max_child_bytes / 1024 / 1024
            ));
            continue;
        }
        // ---- child normalize (the SAME mechanism the document got) ----
        // the child re-enters routing through its POST-normalize state:
        // its chain is looked up by the derived mime, and the chain's
        // probe/extract see the transformed bytes. A normalize Err here is
        // a REASON on the container's aggregate, not a park (only the
        // document's own normalize parks) — the container keeps walking
        // its remaining children.
        let outcome = match normalize(child.bytes.clone(), &child.mime, &child.name) {
            Ok(o) => o,
            Err(e) => {
                skipped.push(format!(
                    "'{}': normalize: {}",
                    child.name,
                    inner_message(&e)
                ));
                continue;
            }
        };
        // the same cap again, on the EXPANDED size (decompression
        // amplification: the document-level rule, applied per child)
        if outcome.bytes.len() as u64 > max_child_bytes {
            skipped.push(format!(
                "'{}': normalize expanded the child to {} bytes, exceeding max_file_size_mb \
                 ({} MiB) — child skipped",
                child.name,
                outcome.bytes.len(),
                max_child_bytes / 1024 / 1024
            ));
            continue;
        }
        log_normalize(&child_display, &outcome);
        match routes.chain_for_mime(&outcome.mime) {
            None => skipped.push(format!(
                "'{}': no extractor chain for mime '{}' in this ingest config — child skipped",
                child.name, outcome.mime
            )),
            Some(chain) => match walk_child(
                routes,
                max_child_bytes,
                &child_display,
                &outcome.bytes,
                &outcome.mime,
                chain,
                depth + 1,
            ) {
                Ok(mut child_parts) => parts.append(&mut child_parts),
                Err(reason) => skipped.push(format!("'{}': {reason}", child.name)),
            },
        }
    }
    if !has_extracted_content(&parts) {
        // every child yielded or was skipped: the CONTAINER yields too, with
        // the child-level reasons on record (a skipped-only container counts
        // as empty for the same bar every kind's "nothing" refusal uses)
        return Err(PipelineError::extract(format!(
            "container '{name}': none of its {} children produced extractable content [{}]",
            children.len(),
            skipped.join("; ")
        )));
    }
    Ok(parts)
}

/// Walk ONE CHILD through its own chain (M2.1 step 3): the FULL routing the
/// document level gets — probe (multi-member chains), affinity, chain-yield —
/// recursively, depth-guarded by [`run_member`]. `Err(reason)` is this
/// child's aggregated yield reason: child-level refusals and exhaustions are
/// REASONS, not parks (the container keeps going; only the DOCUMENT-level
/// all-refuse parks, and only the document's own chain can produce it).
fn walk_child(
    routes: &ChildRoutes,
    max_child_bytes: u64,
    display: &str,
    bytes: &[u8],
    mime: &str,
    chain: &[(String, ExtractorKind)],
    depth: u32,
) -> Result<Vec<Part>, String> {
    // the child's routing short key: its mime's table key, or "default" for
    // off-table mimes (names the chain in aggregate reasons)
    let short_key = crate::pipeline::extract::short_key_for_mime(mime).unwrap_or("default");
    let class = if chain.len() > 1 {
        crate::pipeline::probe::probe(short_key, bytes)
    } else {
        None
    };
    let routing = route(short_key, chain, class);
    if let Routing::Refused(reason) = routing {
        return Err(reason);
    }
    let members = chain_try_order(chain, &routing).into_iter().map(|i| {
        let (name, kind) = &chain[i];
        (
            name.clone(),
            run_member(
                routes,
                max_child_bytes,
                display,
                name,
                kind,
                bytes,
                mime,
                depth,
            ),
        )
    });
    match pick_from_chain(short_key, members) {
        Ok((_winner, parts)) => Ok(parts),
        Err(e) => {
            // the same class annotation the document level applies: a probe
            // that engaged names the class in the failure it explains
            let message = inner_message(&e);
            Err(match class {
                Some(c) => format!("{message} (class={c})"),
                None => message,
            })
        }
    }
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
                    claim: ClaimFilter::new(&icfg.include, &icfg.exclude),
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

    /// Startup lock-skip seam (§5.1): a configured `[ingest.X]` bucket whose
    /// LOCK could not be taken is dropped from the plan set — its worker and
    /// scans do not run, everything else serves normally.
    pub fn remove_bucket(&mut self, vb: &str) {
        self.buckets.remove(vb);
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
        self.shared
            .inflight()
            .lock()
            .unwrap()
            .remove(&job.dedup_key());
    }

    // -- scan --------------------------------------------------------------

    /// One full list-and-diff pass over every configured bucket, followed by
    /// the GC pass (T7): DELETING reaps first (immediate — a `vf rm` 202 that
    /// woke this scan must converge HERE), then the idle-gated orphan sweep.
    /// v3 (R1/R5): every bucket lists ITS OWN source bucket (separate
    /// namespaces — no shared-root listing, no exclusion list: the system
    /// objects live in a different bucket by construction). Idempotent; safe
    /// to call concurrently with workers (markers are single-writer — this
    /// process holds the bucket LOCKs).
    pub async fn scan_once(&self) {
        let mut pending_work = false;
        // per-bucket live listings for the GC pass (keyed by vb)
        let mut listings: BTreeMap<String, Vec<(String, object_store::ObjectMeta)>> =
            BTreeMap::new();
        for (vb, plan) in &self.shared.core.buckets {
            // §5.1 startup skip / lock loss: an unheld bucket gets NO scan
            // decisions (marker writes are writes); the lock manager's
            // retry pass brings it back without a restart
            if !self.state.locks.is_held(vb) {
                self.shared.warn_unlocked(vb);
                continue;
            }
            self.shared.clear_unlocked_warn(vb);
            let listing = match list_source_bucket(&self.state.storage, vb).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!(
                        "ingest: scan of bucket {vb} failed (listing its source bucket): {e}"
                    );
                    // 防误删 (R5 护栏②): without THIS bucket's live listing
                    // there is no 真源 to join against — no scan decisions and
                    // NO GC of any kind may run for it this pass.
                    continue;
                }
            };
            listings.insert(vb.clone(), listing.clone());
            match self.scan_bucket(vb, plan, &listing).await {
                Ok(()) => {
                    // lastScanAt for GetIngestStatus (appendix E.2): stamp on
                    // COMPLETION — a half-failed scan must not claim freshness.
                    self.shared.record_scan(vb);
                }
                Err(e) => eprintln!("ingest: scan of bucket {vb} failed: {e}"),
            }
            // idle signal for the orphan sweep: any marker the file layer
            // still owes work on (scan_bucket may have JUST enqueued it).
            match self.state.storage.list_sources(vb).await {
                Ok(ms)
                    if ms.iter().any(|m| {
                        matches!(m.status, SourceStatus::Pending | SourceStatus::InProgress)
                    }) =>
                {
                    pending_work = true
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("ingest: idle probe of bucket {vb} failed (markers unreadable): {e}")
                }
            }
        }
        self.gc_pass(&listings, pending_work).await;
    }

    /// The T7 GC half of one scan pass (v3 rework, design-v3.md R5).
    ///
    /// * SCOPE = the buckets this daemon holds locks for ([ingest.X] pinned
    ///   ∪ lazily acquired protocol buckets still held) — orphan sweep and
    ///   any cross-bucket write NEVER touch an unlocked bucket (multi-daemon
    ///   deployments: each daemon minds its own).
    /// * `listings` carries THIS pass's per-bucket source listings (the 真源);
    ///   a configured bucket whose listing failed is not in the map and gets
    ///   no GC (the R5 no-live-listing guard).
    /// * 护栏② (zero-live): a bucket whose listing came back EMPTY while it
    ///   still has markers is treated as an anomalous listing — reap and
    ///   sweep both refuse for it, loudly (an outage must not mass-delete
    ///   chunks + trigger a re-embed storm). 护栏① (N-strike) lives in
    ///   marker::plan_absent.
    /// * DELETING reaps are NOT idle-gated (the user behind a `vf rm` 202 is
    ///   waiting), but they re-verify gone-ness against the bucket's live
    ///   set — a re-appeared object (scan_bucket already flipped the marker
    ///   back to PENDING) is never reaped on stale state.
    /// * the orphan sweep runs only when `pending_work` is false AND no job
    ///   is queued/in flight (the optimize discipline). Orphan window = one
    ///   scan period (`scan_interval_secs`, default 30s). The sweep's live
    ///   set per bucket is the FULL bucket listing — include/exclude
    ///   narrowing must not starve orphans into reaping (R5).
    async fn gc_pass(
        &self,
        listings: &BTreeMap<String, Vec<(String, object_store::ObjectMeta)>>,
        pending_work: bool,
    ) {
        // GC scope: held buckets. Configured buckets whose lock we could not
        // take at startup (or lost) are skipped here too.
        let held: BTreeSet<String> = self.state.locks.held_buckets().into_iter().collect();
        // buckets the zero-live guard refused this pass (护栏② gates the
        // orphan sweep exactly as hard as the reap — an anomalous empty
        // listing must not mass-delete chunks through either door)
        let mut guarded: BTreeSet<String> = BTreeSet::new();
        for vb in self.shared.core.buckets.keys() {
            if !held.contains(vb) {
                continue; // not ours to write (R5 scope)
            }
            let Some(live) = listings.get(vb) else {
                continue; // listing failed this pass — no 真源, no GC
            };
            let markers = match self.state.storage.list_sources(vb).await {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("ingest: gc of bucket {vb} skipped (markers unreadable): {e}");
                    continue;
                }
            };
            // 护栏②: empty listing + markers => anomalous; refuse + warn loud
            if live.is_empty() && !markers.is_empty() {
                eprintln!(
                    "ingest: gc {vb}: listing returned ZERO objects while {} marker(s) exist — \
                     treating it as an anomalous listing and REFUSING to reap or sweep (delete \
                     the markers by hand if the bucket really is empty)",
                    markers.len()
                );
                guarded.insert(vb.clone());
                continue;
            }
            let live_uris: BTreeSet<String> = live
                .iter()
                .map(|(k, _)| marker::object_key_hash(k))
                .collect();
            for m in markers {
                if m.status != SourceStatus::Deleting {
                    continue;
                }
                let uri = marker::object_key_hash(&m.object_key);
                if live_uris.contains(&uri) {
                    continue; // the object is back; scan_bucket owns the flip
                }
                match gc::reap_source(&self.state, vb, &m.object_key).await {
                    Ok(keys) => {
                        self.shared.record_deleted(vb);
                        eprintln!(
                            "ingest: gc {vb}: reaped '{}' ({keys} chunk(s) + marker)",
                            m.object_key
                        );
                    }
                    Err(e) => eprintln!(
                        "ingest: gc {vb}: reaping '{}' failed (next scan retries): {e}",
                        m.object_key
                    ),
                }
            }
        }
        let inflight_empty = self.shared.inflight().lock().unwrap().is_empty();
        if pending_work || !inflight_empty {
            return; // owed work first; orphans keep (they are only waste)
        }
        for vb in &held {
            if guarded.contains(vb) {
                continue; // zero-live guard (护栏②)
            }
            let Some(live) = listings.get(vb) else {
                continue;
            };
            // held-but-unconfigured buckets can hit the guard too: an empty
            // listing with markers is suspicious whatever wrote the markers
            if live.is_empty() {
                match self.state.storage.list_sources(vb).await {
                    Ok(ms) if !ms.is_empty() => {
                        eprintln!(
                            "ingest: gc {vb}: listing returned ZERO objects while {} marker(s) \
                             exist — orphan sweep refused this pass",
                            ms.len()
                        );
                        continue;
                    }
                    Ok(_) => {}
                    Err(_) => continue,
                }
            }
            let live_uris: BTreeSet<String> = live
                .iter()
                .map(|(k, _)| marker::object_key_hash(k))
                .collect();
            match gc::orphan_sweep(&self.state, vb, &live_uris).await {
                Ok(0) => {}
                Ok(n) => eprintln!("ingest: gc {vb}: orphan sweep deleted {n} chunk(s)"),
                Err(e) => {
                    eprintln!("ingest: gc {vb}: orphan sweep failed (next scan retries): {e}")
                }
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
        let (mut n_new, mut n_changed, mut n_requeue, mut n_refresh) = (0u32, 0u32, 0u32, 0u32);
        for (key, meta) in listing {
            // claim filter + extractor routing (R5): unclaimed keys are not
            // sources of this pipeline
            if !plan.claim.matches(key) || plan.extractor_chain_for(key).is_none() {
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
                marker::ScanPlan::Refresh(m) => {
                    // object back after a strike: persist the counter reset
                    storage
                        .save_source(vb, &key_hash, &m)
                        .await
                        .map_err(|e| e.to_string())?;
                    n_refresh += 1;
                }
                marker::ScanPlan::Settled | marker::ScanPlan::Parked => {}
            }
        }
        // vanished objects -> DELETING via the N-strike gate (R5 护栏①: an
        // object must be absent VANISH_STRIKES consecutive scans before the
        // flip; T7's GC reaps chunks + markers afterwards). Only markers the
        // CURRENT claim filter covers: an include/exclude change must not
        // mass-delete what it merely stopped claiming.
        let (mut n_gone, mut n_strike) = (0u32, 0u32);
        for m in storage.list_sources(vb).await.map_err(|e| e.to_string())? {
            if plan.claim.matches(&m.object_key) && !present.contains(&m.object_key) {
                match marker::plan_absent(m, &marker::now_rfc3339()) {
                    marker::AbsentPlan::FlipDeleting(d) => {
                        let key_hash = marker::object_key_hash(&d.object_key);
                        storage
                            .save_source(vb, &key_hash, &d)
                            .await
                            .map_err(|e| e.to_string())?;
                        n_gone += 1;
                    }
                    marker::AbsentPlan::WaitForStrike(w) => {
                        let key_hash = marker::object_key_hash(&w.object_key);
                        storage
                            .save_source(vb, &key_hash, &w)
                            .await
                            .map_err(|e| e.to_string())?;
                        n_strike += 1;
                    }
                }
            }
        }
        if n_new + n_changed + n_requeue + n_refresh + n_gone + n_strike > 0 {
            eprintln!(
                "ingest: scan {vb}: {n_new} new, {n_changed} changed, {n_requeue} requeued, \
                 {n_refresh} refreshed, {n_gone} deleting, {n_strike} strike-counted"
            );
        }
        // KB statistics (appendix E.2): sources examined by this COMPLETED
        // pass — claimed, extractor-routed, present objects.
        self.shared.record_scanned(vb, present.len() as u64);
        Ok(())
    }

    async fn enqueue(&self, job: Job) {
        // reindex quiesce barrier: a reindex pass is draining this bucket's
        // jobs before its clear+flip section — taking NEW work now would
        // both extend its wait and race its flip. Refuse: the flip turns
        // everything PENDING anyway, and the reindex's trailing wake
        // re-covers every refused source on the very next scan (unchanged
        // signature + PENDING is the scanner's own Enqueue case).
        if self.shared.reindex_barrier_held(&job.vb) {
            return;
        }
        let k = job.dedup_key();
        if !self.shared.inflight().lock().unwrap().insert(k.clone()) {
            return; // already queued or in flight
        }
        // bounded send: backpressure into the scanner is the designed
        // behavior (the queue is the shock absorber, not a buffer of the
        // whole root).
        if self.tx.send(job).await.is_err() {
            // receivers all gone (shutdown) — undo the dedup entry
            self.shared.inflight().lock().unwrap().remove(&k);
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
        // §5.1 (在飞提交前复核锁归属): a bucket whose lock this daemon lost
        // (or never acquired — startup skip) gets no writes from us. The
        // commit paths re-check again through ops::put_vectors's write lock.
        if !self.state.locks.still_ours(&job.vb) {
            eprintln!(
                "ingest: {} skipped: bucket {} is not lock-held by this daemon",
                job.object_key, job.vb
            );
            return;
        }
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
        // domain::user_object_key_candidates). The read addresses THIS
        // bucket's SOURCE store (v3: one store per bucket).
        let source = storage.source_store(&job.vb);
        let mut get = None;
        let mut hard_err = None;
        for okey in domain::user_object_key_candidates(&job.object_key) {
            match source.get(&okey).await {
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
        // routing trace of the (first) successful extract segment — rides
        // the same lifecycle as cached_chunks: set once, read at completion
        let mut route_note: Option<RouteNote> = None;
        let mut succeeded: BTreeSet<String> = BTreeSet::new();
        let mut last_err = String::new();
        let mut parked: Option<String> = None;
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match self
                .try_attempt(
                    job,
                    plan,
                    &bytes,
                    &mut cached_chunks,
                    &mut route_note,
                    &mut succeeded,
                )
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

        // per-modality counts ("text" always, "image" only when present —
        // M3-T1; byte-stable with the pre-image marker for text-only files)
        let chunks_map = Some(chunk_counts(cached_chunks.as_deref()));
        let all_done = succeeded.len() == plan.embedders.len();
        let outcome = if let Some(reason) = parked {
            Outcome::Parked(reason)
        } else if all_done {
            Outcome::Indexed {
                sig,
                hash,
                chunks: chunks_map,
                retries: attempts - 1,
                // progress-line routing note, present only when the probe
                // actually engaged (multi-member chain + class verdict):
                // "plain (class=mixed-text-scan)"
                via: route_note
                    .take()
                    .map(|n| format!("{} (class={})", n.winner, n.class)),
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
        note: &mut Option<RouteNote>,
        succeeded: &mut BTreeSet<String>,
    ) -> Result<(), AttemptErr> {
        // ---- extract + chunk (CPU segment) ----
        let chunks: &[Chunk] = match cached {
            Some(c) => c,
            None => {
                let chain = plan.extractor_chain_for(&job.object_key).ok_or_else(|| {
                    AttemptErr::Parked(format!(
                        "no extractor handles this object (routing changed after the scan) — \
                         it is not a source of this pipeline"
                    ))
                })?;
                let chunker = plan.chunker.clone();
                // the PRE-normalize state: the scan-time mime guess (which
                // already sees through compression wrappers) and the raw
                // bytes. The blocking segment normalizes this FIRST and
                // derives the routing short key from the POST-normalize
                // mime — the chain itself stays the object-key-resolved one
                // the scan claimed the source under.
                let mime = mime_for(&job.object_key).to_string();
                let bytes = bytes.clone();
                // container unpacking (M2.1 step 3): the children of any
                // container member re-enter THIS bucket's extract table —
                // snapshotted here (plus the per-child size guard) because
                // the blocking segment cannot borrow the plan
                let routes = plan.child_routes();
                let max_child_bytes = plan.max_file_size_bytes;
                let display = job.object_key.clone();
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
                    // M2.1 step 3 layering, then step 2 — all inside THIS one
                    // CPU segment, no concurrency:
                    //
                    // * NORMALIZE first (the generalized preprocessing
                    //   stage): one fixed ordered table of built-in
                    //   transforms runs over the raw bytes BEFORE anything
                    //   else — the probe, the routing short key and every
                    //   extractor see the POST-normalize state. Err is a
                    //   deterministic dead end (corrupt input the stage
                    //   refuses to guess around: retrying cannot uncorrupt
                    //   a gzip stream), so it parks with the reason instead
                    //   of burning the retry budget; the amplification gate
                    //   below applies the SAME max_file_size_mb to the
                    //   expanded bytes (a decompression bomb may not smuggle
                    //   past the file-size guard what the scanner would
                    //   have refused outright — the container-child rule,
                    //   extended to transforms).
                    // * probe: multi-member chains only (`chain.len() > 1`
                    //   is the zero-cost gate — a single member has nothing
                    //   to route, so the probe never even runs and its
                    //   byte-stable messages stay untouched); a probe that
                    //   cannot parse the bytes yields Unknown.
                    // * route: the pure affinity scan; all-Refuses parks the
                    //   file WITHOUT executing any extractor (a
                    //   deterministic dead end — retrying cannot change a
                    //   class verdict, and the aggregate reason is more
                    //   informative than a chain of guaranteed failures).
                    // * Run/Order: the routed member runs FIRST, every
                    //   remaining member keeps config order as the
                    //   correctness backstop; the member iterator stays
                    //   lazy — nothing after the winner executes.
                    // * each member UNPACKS first (M2.1 step 3): a leaf
                    //   (the default) extracts as before, byte-for-byte; a
                    //   container's children re-enter the USER-configured
                    //   chains (`run_member` — each child normalized again
                    //   inside it), so this map stays the single place a
                    //   member's outcome is produced — laziness and the
                    //   yield vocabulary carry over unchanged.
                    let outcome = normalize(bytes, &mime, &display).map_err(|e| {
                        AttemptErr::Parked(format!("normalize: {}", inner_message(&e)))
                    })?;
                    if outcome.bytes.len() as u64 > max_child_bytes {
                        return Err(AttemptErr::Parked(format!(
                            "normalize expanded the content to {} bytes, exceeding the \
                             configured max_file_size_mb ({} MiB) — the expanded size is what \
                             the pipeline would have to carry",
                            outcome.bytes.len(),
                            max_child_bytes / 1024 / 1024
                        )));
                    }
                    log_normalize(&display, &outcome);
                    // terminal state: everything downstream — probe, route,
                    // extract, unpack — sees these, never the raw bytes
                    let bytes = outcome.bytes;
                    let mime = outcome.mime;
                    // routing key from the POST-normalize mime (the scan-time
                    // guess already sees through compression wrappers the
                    // same way, so for a claimed key this agrees with it)
                    let short_key = crate::pipeline::extract::short_key_for_mime(&mime)
                        .unwrap_or("default")
                        .to_string();
                    let class = if chain.len() > 1 {
                        crate::pipeline::probe::probe(&short_key, &bytes)
                    } else {
                        None
                    };
                    let routing = route(&short_key, &chain, class);
                    if let Routing::Refused(reason) = routing {
                        return Err(AttemptErr::Parked(reason));
                    }
                    let members = chain_try_order(&chain, &routing).into_iter().map(|i| {
                        let (name, kind) = &chain[i];
                        (
                            name.clone(),
                            run_member(
                                &routes,
                                max_child_bytes,
                                &display,
                                name,
                                kind,
                                &bytes,
                                &mime,
                                0,
                            ),
                        )
                    });
                    pick_from_chain(&short_key, members)
                        .map_err(|e| {
                            // observability: the exhaustion aggregate names
                            // the class when the probe engaged (Unknown-class
                            // and single-member messages stay byte-stable —
                            // no verdict, no annotation)
                            match (class, e) {
                                (Some(c), PipelineError::Extract(m)) => {
                                    AttemptErr::Fail(format!("{m} (class={c})"))
                                }
                                (_, e) => AttemptErr::Fail(e.to_string()),
                            }
                        })
                        .and_then(|(winner, parts)| {
                            chunker
                                .chunk(&parts)
                                .map(|chunks| (winner, chunks))
                                .map_err(|e| AttemptErr::Fail(e.to_string()))
                        })
                        .map(|(winner, chunks)| {
                            // the note exists only when routing engaged
                            let traced = class.map(|c| RouteNote { winner, class: c });
                            match traced {
                                Some(t) => (Some(t), chunks),
                                None => (None, chunks),
                            }
                        })
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
                // joined: Result<Result<(Option<RouteNote>, Vec<Chunk>),
                // AttemptErr>, JoinError> (timeout layer already unwrapped)
                let (traced, chunks): (Option<RouteNote>, Vec<Chunk>) = match joined {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return Err(e),
                    Err(e) => return Err(AttemptErr::Fail(format!("extract task panicked: {e}"))),
                };
                *note = traced;
                *cached = Some(chunks);
                cached.as_deref().expect("just stored")
            }
        };

        // ---- fan-out (§5.10.1: every doc-modality-matching embedder) ----
        // Resolve the FULL fan-out list first: the modality gate below and
        // each embedder's chunk scoping read the SAME doc-modality
        // declarations.
        let mut fanned: Vec<(&String, &LoadedEmbedder)> = Vec::with_capacity(plan.embedders.len());
        for name in &plan.embedders {
            let loaded = self
                .shared
                .core
                .embedder(name)
                .ok_or_else(|| AttemptErr::Fail(format!("embedder '{name}' not loaded")))?;
            fanned.push((name, loaded));
        }
        // Modality gate (M3-T1, the routing rule of §5.10.1: a Part fans
        // out to every doc-modality-matching embedder — and to NOBODY when
        // no listed embedder accepts its modality). A gap is a loud FAILED
        // with the aggregate reason + remedy, never a silent skip or an
        // empty-yield success; deterministic, so it parks (retrying cannot
        // change a declared modality — the all-refuse route verdict shape).
        // The gate reads the FULL list, not the not-yet-succeeded subset:
        // coverage is a configured-pipeline property, and on a retry after
        // a partial success an already-succeeded embedder still counts as
        // its modality's taker.
        if let Some(reason) = modality_gap_reason(chunks, &fanned) {
            return Err(AttemptErr::Parked(reason));
        }
        for (name, loaded) in fanned {
            if succeeded.contains(name) {
                continue;
            }
            let accepts = loaded.doc_modalities();
            // Per-embedder modality scoping: an embedder sees only the
            // chunks its doc side accepts. The all-accepted fast path hands
            // the ORIGINAL slice over (today's text-only pipelines are
            // byte-identical, zero copies); a scoped path materializes the
            // subset — an embedder with nothing to accept embeds nothing,
            // writes no rows, and still counts as done.
            let scoped_storage: Option<Vec<Chunk>> =
                if chunks.iter().all(|c| accepts.contains(&c.modality())) {
                    None
                } else {
                    Some(
                        chunks
                            .iter()
                            .filter(|c| accepts.contains(&c.modality()))
                            .cloned()
                            .collect(),
                    )
                };
            let scoped: &[Chunk] = scoped_storage.as_deref().unwrap_or(chunks);
            let spec = &plan.derived[name];
            let vectors = self
                .embed_gated(loaded, scoped)
                .await
                .map_err(|e| AttemptErr::Fail(format!("embedder '{name}' failed: {e}")))?;

            let rows = build_rows(plan, job, spec, scoped, &vectors)?;
            for batch in split_batches(rows, spec.dim) {
                // domain check + commit under the write lock: the check is
                // load-then-(maybe)-create, and racing it against another
                // worker's identical create would surface as Conflict noise.
                let _guard = self.shared.write_lock().lock().await;
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

    /// CPU gate: LOCAL inference holds the thread (ort and siglip2 wrap
    /// themselves in spawn_blocking); remote HTTP embeds are I/O-bound and
    /// stay ungated.
    async fn embed_gated(
        &self,
        loaded: &LoadedEmbedder,
        chunks: &[Chunk],
    ) -> Result<Vec<Vec<f32>>, PipelineError> {
        match loaded {
            LoadedEmbedder::Ort(_) | LoadedEmbedder::Siglip2(_) => {
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

    /// Persist the final marker + one stderr progress line per file. Also the
    /// single stats/checkpoint site: KB counters advance here (new vs
    /// modified decided by the PREVIOUS marker's completion evidence) and
    /// the reindex job's done counter bumps per completion.
    async fn finish_marker(&self, job: &Job, prev: &SourceMarker, outcome: Outcome) {
        let mut m = prev.clone();
        m.updated_at = marker::now_rfc3339();
        // engine observability: completing outcomes record WHAT PRODUCED
        // them (doctor compares against the running table); failures carry
        // no evidence and no engines either
        let engines_now = Some(crate::pipeline::engines::engine_versions_owned());
        match outcome {
            Outcome::Indexed {
                sig,
                hash,
                chunks,
                retries,
                via,
            } => {
                m.status = SourceStatus::Indexed;
                m.etag_sig = sig;
                m.content_hash = hash;
                m.chunks = chunks;
                m.retries = retries;
                m.retries_exhausted = false;
                m.status_reason = None;
                m.engines = engines_now;
                m.pipeline_fingerprint = self.shared.core.buckets[&job.vb].fingerprint.clone();
                // ALL modalities, not just text (M3: image-only sources
                // would otherwise log chunks=0 next to a real image chunk —
                // their per-modality map carries text=0 by construction)
                let count = m
                    .chunks
                    .as_ref()
                    .map(|c| c.values().sum::<u32>())
                    .unwrap_or(0);
                match via {
                    // routing engaged: name the member that won and the
                    // probed class it won under (M2.1 step 2 observability)
                    Some(v) => {
                        eprintln!(
                            "ingest: {} -> INDEXED via {v} (chunks={count})",
                            job.object_key
                        )
                    }
                    // no probe verdict (single-member chain / Unknown):
                    // the pre-routing line, byte-stable
                    None => eprintln!("ingest: {} -> INDEXED (chunks={count})", job.object_key),
                }
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
                m.engines = engines_now;
                m.pipeline_fingerprint = self.shared.core.buckets[&job.vb].fingerprint.clone();
                eprintln!("ingest: {} -> PARTIALLY_INDEXED ({reason})", job.object_key);
            }
            Outcome::Failed(reason) => {
                m.status = SourceStatus::Failed;
                m.retries_exhausted = true;
                m.status_reason = Some(reason.clone());
                m.chunks = None;
                m.engines = None;
                eprintln!("ingest: {} -> FAILED ({reason})", job.object_key);
            }
            Outcome::Parked(reason) => {
                m.status = SourceStatus::Failed;
                m.retries_exhausted = true;
                m.status_reason = Some(reason.clone());
                m.chunks = None;
                m.engines = None;
                eprintln!("ingest: {} -> FAILED/parked ({reason})", job.object_key);
            }
        }
        // KB statistics (appendix E.2 field names): INDEXED/PARTIALLY count
        // as indexed work — "modified" when the previous marker carried
        // completion evidence (a real re-ingest) OR an explicit reindex job
        // is running (its flip DELIBERATELY wipes the evidence the first
        // rule reads — the rewrite is still a modified document, not a new
        // one); FAILED/parked counts as failed. PARTIALLY is not
        // double-counted as failed: its succeeded half is real indexed work
        // and the per-source statusReason already tells the story.
        match m.status {
            SourceStatus::Indexed | SourceStatus::PartiallyIndexed => {
                let modified = (!prev.content_hash.is_empty() && prev.chunks.is_some())
                    || self.shared.reindex_running(&job.vb);
                self.shared.record_indexed(&job.vb, modified);
            }
            SourceStatus::Failed => self.shared.record_failed(&job.vb),
            _ => unreachable!("finish_marker only persists terminal states"),
        }
        // reindex progress (T7): one completion = one unit of done.
        self.shared.bump_reindex(&job.vb);
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
        /// Routing trace for the progress line — `"{winner} (class={c})"`,
        /// present only when the probe engaged (M2.1 step 2). NOT marker
        /// state: statusReason stays None on success; this is stderr-only
        /// observability.
        via: Option<String>,
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
/// via `store_chunk_text = false`). Per-chunk modality (M3-T1): both the
/// metadata value and the chunk-key namespace come from the CHUNK, so image
/// atoms land in the `image` namespace (`{hash}-image-{i}`) with
/// `x-vf-modality = image` — text chunks are byte-identical to before.
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
        let modality = chunk.modality();
        let mut metadata = serde_json::json!({
            "x-vf-src-uri": job.key_hash,
            "x-vf-src-name": job.object_key,
            "x-vf-modality": modality.to_string(),
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
            key: marker::chunk_key(&job.key_hash, &modality.to_string(), chunk.index),
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

/// List ONE bucket's SOURCE store (design-v3.md R1/R5: the source bucket IS
/// the vb's namespace — system objects live in the separate `X-vf` bucket,
/// so the exclusion list of the v2.2 shared-root scan is gone by
/// construction), returning bucket-relative keys with their metadata. The
/// `/x-vf` face (ListSources' marker x live join) reuses it.
///
/// Key canonicalization (a T6-found latent bug of the T3 scanner, kept):
/// `object_store::path::Path` percent-encodes non-ASCII and AWS-discouraged
/// characters (`%{}`\\^]"'>~<#|*?` + controls) at construction, and the LOCAL
/// backend materializes that canonical form on disk — so a listed location's
/// string is CANONICAL, while every human-facing key (marker `object_key`,
/// PutSource's decoded `x-vf-file-name`, GetSource lookups through
/// [`domain::user_object_key`], which re-canonicalizes symmetrically) is the
/// RAW key. The listing is therefore DECODED back here; without it, any key
/// with such characters would phantom-appear (encoded twin) and
/// vanish-flip (raw twin) on every scan. Decode is the exact inverse of
/// `Path`'s encoding (which escapes `%` itself), so the round trip is
/// bijective.
pub(crate) async fn list_source_bucket(
    storage: &Storage,
    vb: &str,
) -> Result<Vec<(String, object_store::ObjectMeta)>, String> {
    use futures_util::StreamExt;
    let store = storage.source_store(vb);
    let mut stream = store.list(None);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|e| format!("list bucket {vb}: {e}"))?;
        let key = percent_encoding::percent_decode_str(meta.location.as_ref())
            .decode_utf8_lossy()
            .into_owned();
        if key.is_empty() {
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
    /// `queued` -> `running` -> `done`; advanced by worker completions
    /// ([`IngestShared::bump_reindex`]) and re-derived by GetIngestStatus
    /// reads (single-process, single-writer — a read is a safe checkpoint).
    pub status: &'static str,
    pub total: usize,
    pub done: usize,
}

/// KB-aligned ingestion statistics (appendix E.2, field names from Bedrock
/// KB `IngestionJobStatistics`): PROCESS-LIFETIME cumulative counters, one
/// set per bucket — they reset on restart by design (nothing persists them;
/// the markers remain the durable truth).
#[derive(Debug, Default)]
pub struct IngestStats {
    scanned: std::sync::atomic::AtomicU64,
    new_indexed: std::sync::atomic::AtomicU64,
    modified_indexed: std::sync::atomic::AtomicU64,
    deleted: std::sync::atomic::AtomicU64,
    failed: std::sync::atomic::AtomicU64,
}

/// Read-only snapshot of one bucket's [`IngestStats`] (GetIngestStatus).
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsSnapshot {
    pub scanned: u64,
    pub new_indexed: u64,
    pub modified_indexed: u64,
    pub deleted: u64,
    pub failed: u64,
}

/// The piece of the ingest runtime the PROTOCOL face (`AppState.ingest` ->
/// [`IngestHandle`]) holds: the loaded core (one `Arc` shared with the
/// runtime's workers), the last-scan clock, the reindex job registry, the
/// KB statistics, and the two coordination primitives the runtime's workers
/// and the reindex path share (commit write-lock + in-flight dedup set),
/// plus `wake()` for the immediate trigger and `shutdown()` for the drain.
pub struct IngestShared {
    core: Arc<IngestCore>,
    /// bucket -> RFC3339 of the last COMPLETED scan (GetIngestStatus).
    last_scan: std::sync::Mutex<BTreeMap<String, String>>,
    /// bucket -> latest reindex job.
    jobs: std::sync::Mutex<BTreeMap<String, ReindexJob>>,
    /// bucket -> KB statistics (appendix E.2 numberOf* block).
    stats: BTreeMap<String, IngestStats>,
    /// live dedup: queued + in-flight jobs (§5.10.5 "幂等 key 防重复入队").
    /// Shared with the reindex path: a source a worker owns right now is
    /// skipped by the flip (its current-config result is already valid).
    inflight: std::sync::Mutex<HashSet<String>>,
    /// The reindex QUIESCE BARRIER (job-level serialization, 2026-11-05 —
    /// closes the §10.8 "reindex vs in-flight worker" debt): while a bucket
    /// is held here, `enqueue` refuses its new jobs and a running
    /// [`reindex_bucket`] waits for its in-flight jobs to drain before the
    /// clear+flip section. This is what moves reindex from "skip whatever
    /// is in flight RIGHT NOW" (the old narrow-race semantics) to "run
    /// strictly BETWEEN jobs" — no worker can commit around the flip.
    reindex_barrier: std::sync::Mutex<BTreeSet<String>>,
    /// Serializes put/delete commits across workers AND the reindex
    /// clear+flip section: lance commits are optimistic
    /// (read-modify-write), so concurrent merge_inserts into ONE derived
    /// index would race and surface commit-conflict errors that the retry
    /// loop would then burn attempts on. Extraction/embedding still run in
    /// parallel — only the final commit is one-at-a-time.
    write_lock: tokio::sync::Mutex<()>,
    /// Buckets already WARNED as lock-skipped this uptime (throttle: a 1s
    /// scan interval must not print the same skip every pass; cleared when
    /// the lock recovers).
    unlocked_warned: std::sync::Mutex<BTreeSet<String>>,
}

impl IngestShared {
    pub fn new(core: IngestCore) -> Arc<IngestShared> {
        let stats = core
            .buckets
            .keys()
            .map(|b| (b.clone(), IngestStats::default()))
            .collect();
        Arc::new(IngestShared {
            core: Arc::new(core),
            last_scan: std::sync::Mutex::new(BTreeMap::new()),
            jobs: std::sync::Mutex::new(BTreeMap::new()),
            stats,
            inflight: std::sync::Mutex::new(HashSet::new()),
            reindex_barrier: std::sync::Mutex::new(BTreeSet::new()),
            write_lock: tokio::sync::Mutex::new(()),
            unlocked_warned: std::sync::Mutex::new(BTreeSet::new()),
        })
    }

    /// The commit-serialization lock (workers' put path + reindex's
    /// clear+flip section).
    pub(crate) fn write_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.write_lock
    }

    /// The queued + in-flight dedup set (runtime-internal + reindex skip).
    pub(crate) fn inflight(&self) -> &std::sync::Mutex<HashSet<String>> {
        &self.inflight
    }

    /// Is a worker currently owning `(bucket, object_key)`?
    pub fn is_inflight(&self, bucket: &str, object_key: &str) -> bool {
        self.inflight
            .lock()
            .unwrap()
            .contains(&format!("{}\u{1}{}", bucket, object_key))
    }

    /// Hold/release/read the reindex quiesce barrier for one bucket (see
    /// the field doc). Private to this module: `reindex_bucket` is the only
    /// holder; the tests reach in to pin the enqueue-refusal rule.
    fn hold_reindex_barrier(&self, bucket: &str) {
        self.reindex_barrier.lock().unwrap().insert(bucket.into());
    }

    fn release_reindex_barrier(&self, bucket: &str) {
        self.reindex_barrier.lock().unwrap().remove(bucket);
    }

    fn reindex_barrier_held(&self, bucket: &str) -> bool {
        self.reindex_barrier.lock().unwrap().contains(bucket)
    }

    /// How many queued + in-flight jobs belong to `bucket` (the drain-wait
    /// condition of the barrier).
    fn bucket_inflight_count(&self, bucket: &str) -> usize {
        let prefix = format!("{bucket}\u{1}");
        self.inflight
            .lock()
            .unwrap()
            .iter()
            .filter(|k| k.starts_with(&prefix))
            .count()
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

    /// Is `object_key` a SOURCE this bucket's pipeline claims (claim-filter
    /// match AND a routed extractor chain, R5)? PutSource refuses keys
    /// outside it — a marker for an unclaimed key would never be touched by
    /// the scanner again.
    pub fn claims_key(&self, bucket: &str, object_key: &str) -> bool {
        self.core.buckets.get(bucket).is_some_and(|p| {
            p.claim.matches(object_key) && p.extractor_chain_for(object_key).is_some()
        })
    }

    /// Warn (once) that a bucket is lock-skipped (§5.1).
    pub(crate) fn warn_unlocked(&self, bucket: &str) {
        let mut warned = self.unlocked_warned.lock().unwrap();
        if warned.insert(bucket.to_string()) {
            eprintln!(
                "ingest: bucket {bucket} scan SKIPPED — its writer lock is held elsewhere; \
                 resuming when the lock is recovered"
            );
        }
    }

    /// Clear the lock-skip warning state (the lock came back).
    pub(crate) fn clear_unlocked_warn(&self, bucket: &str) {
        self.unlocked_warned.lock().unwrap().remove(bucket);
    }

    /// Stamp a completed scan (scheduler -> GetIngestStatus).
    pub(crate) fn record_scan(&self, bucket: &str) {
        self.last_scan
            .lock()
            .unwrap()
            .insert(bucket.to_string(), marker::now_rfc3339());
    }

    // ---- KB statistics (appendix E.2 numberOf* block; T7) ----

    /// Sources examined by completed scan passes (worker -> stats).
    pub(crate) fn record_scanned(&self, bucket: &str, n: u64) {
        if let Some(s) = self.stats.get(bucket) {
            s.scanned.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// One terminal INDEXED/PARTIALLY_INDEXED completion (`modified` = the
    /// previous marker carried completion evidence).
    pub(crate) fn record_indexed(&self, bucket: &str, modified: bool) {
        if let Some(s) = self.stats.get(bucket) {
            let counter = if modified {
                &s.modified_indexed
            } else {
                &s.new_indexed
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// One FAILED/parked terminal outcome.
    pub(crate) fn record_failed(&self, bucket: &str) {
        if let Some(s) = self.stats.get(bucket) {
            s.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// One DELETING source fully reaped (chunks + marker) by GC.
    pub(crate) fn record_deleted(&self, bucket: &str) {
        if let Some(s) = self.stats.get(bucket) {
            s.deleted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Snapshot one bucket's counters (GetIngestStatus `stats` block).
    pub fn stats_of(&self, bucket: &str) -> StatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        match self.stats.get(bucket) {
            None => StatsSnapshot::default(),
            Some(s) => StatsSnapshot {
                scanned: s.scanned.load(Relaxed),
                new_indexed: s.new_indexed.load(Relaxed),
                modified_indexed: s.modified_indexed.load(Relaxed),
                deleted: s.deleted.load(Relaxed),
                failed: s.failed.load(Relaxed),
            },
        }
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

    /// Is a reindex job of one bucket outstanding (not yet done)? Drives
    /// the new-vs-modified statistics split (see `finish_marker`).
    pub(crate) fn reindex_running(&self, bucket: &str) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .get(bucket)
            .is_some_and(|j| j.status != "done")
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

    /// One more completion under the latest job of one bucket (worker ->
    /// progress; the GetIngestStatus read-side refresh reconciles the same
    /// field from markers, so the two paths converge).
    pub(crate) fn bump_reindex(&self, bucket: &str) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(bucket)
            && job.status != "done"
        {
            job.done = (job.done + 1).min(job.total);
            job.status = if job.done >= job.total {
                "done"
            } else {
                "running"
            };
        }
    }
}

// ---------------------------------------------------------------------------
// reindex mechanism (T7, §5.10.5 "按旧指纹清旧块 → 重写 → 更新标记")
// ---------------------------------------------------------------------------

/// What one [`reindex_bucket`] pass did (both call surfaces report it:
/// `/x-vf/Reindex` answers with `job_id`, `serve --reindex` logs the rest).
#[derive(Debug, Clone)]
pub struct ReindexOutcome {
    pub job_id: String,
    /// sources flipped to PENDING under the current fingerprint.
    pub flipped: usize,
    /// old chunk keys deleted across the bucket's managed indexes.
    pub cleared: usize,
}

/// The reindex MECHANISM, shared by `/x-vf/Reindex` (forced re-embed of a
/// running daemon) and `vectorfs serve --reindex` (the startup-flag path
/// that resolves the fingerprint-gate deadlock): for one bucket —
///
/// 0. **静默屏障** (job-level serialization, 2026-11-05 — closes the §10.8
///    "reindex vs in-flight worker" debt) — hold the per-bucket quiesce
///    barrier (new enqueues for the bucket are refused while held) and wait
///    for the bucket's queued + in-flight jobs to DRAIN before enumerating.
///    The wait does NOT hold the write lock (workers need that lock to
///    commit their batches — a barrier holding it would deadlock them);
///    waiting on the in-flight set alone is sufficient because `enqueue`
///    cannot add to it while the barrier stands. Bounded at 90s: on timeout
///    the pass degrades to the pre-barrier semantics (skip whatever is
///    still in flight) — strictly no worse than the old behavior, and
///    loudly logged. `serve --reindex` runs before workers spawn, so its
///    barrier passes trivially.
/// 1. **清旧块** — delete every derived chunk of the target sources across ALL
///    of the bucket's MANAGED indexes, old-derived ones included (an embedder
///    config change derives a new index NAME; the previous index keeps its
///    rows). Per-uri DELETE, not upsert: a chunk-count SHRINK (size 64→96)
///    must leave zero high-ordinal residue. Old-derived indexes are emptied
///    but KEPT — deleting an index is destructive and stays the operator's
///    (DeleteIndex) call in M2; doctor/status show them at 0 rows.
/// 2. **重写** — flip every non-DELETING marker to PENDING, wipe its
///    completion evidence, stamp the CURRENT fingerprint; the caller wakes
///    the scheduler and the normal scan-and-diff re-embeds (chunk keys carry
///    the object-key hash + new ordinals, so reruns converge by upsert; the
///    clear above guarantees no orphans).
/// 3. **job registry** — T6's entry (total = flipped sources; workers bump
///    `done` per completion).
///
/// With the barrier drained, the target enumeration sees a QUIESCENT bucket:
/// nothing is in flight, so the old skip-in-flight filter is a no-op on the
/// happy path (it remains for the timeout degradation).
pub async fn reindex_bucket(
    state: &AppState,
    shared: &Arc<IngestShared>,
    bucket: &str,
) -> Result<ReindexOutcome, String> {
    let fingerprint = shared
        .fingerprint_of(bucket)
        .ok_or_else(|| format!("bucket {bucket:?} is not ingest-configured"))?;
    // ---- 0. quiesce barrier: run strictly BETWEEN this bucket's jobs ----
    shared.hold_reindex_barrier(bucket);
    let drain = drain_bucket_inflight(shared, bucket).await;
    let result = reindex_bucket_quiesced(state, shared, bucket, &fingerprint).await;
    shared.release_reindex_barrier(bucket);
    if !drain {
        eprintln!(
            "ingest: reindex {bucket}: quiesce barrier timed out with jobs still in flight — \
             proceeding with the legacy skip-in-flight semantics (their clear lands on the next \
             reindex; the §10.8 debt's OLD shape, degraded loudly)"
        );
    }
    result
}

/// The barrier's bounded drain wait. `false` = timed out (caller degrades
/// to the legacy semantics and says so on stderr).
async fn drain_bucket_inflight(shared: &Arc<IngestShared>, bucket: &str) -> bool {
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(90);
    const POLL: std::time::Duration = std::time::Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + BUDGET;
    while shared.bucket_inflight_count(bucket) > 0 {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
    true
}

/// The clear+flip section proper (steps 1–3 above), run with the bucket
/// quiesced (or degraded after a timeout).
async fn reindex_bucket_quiesced(
    state: &AppState,
    shared: &Arc<IngestShared>,
    bucket: &str,
    fingerprint: &str,
) -> Result<ReindexOutcome, String> {
    let markers = state
        .storage
        .list_sources(bucket)
        .await
        .map_err(|e| e.to_string())?;
    let targets: Vec<SourceMarker> = markers
        .into_iter()
        .filter(|m| {
            m.status != SourceStatus::Deleting && !shared.is_inflight(bucket, &m.object_key)
        })
        .collect();
    if targets.is_empty() {
        // nothing to rewrite; still hand the caller a (trivially done) job id
        let job_id = shared.register_reindex(bucket, 0);
        return Ok(ReindexOutcome {
            job_id,
            flipped: 0,
            cleared: 0,
        });
    }
    let uris: BTreeSet<String> = targets
        .iter()
        .map(|m| marker::object_key_hash(&m.object_key))
        .collect();
    // one write-lock section around clear + flip: the workers' per-batch
    // commits cannot interleave INSIDE this section.
    let _guard = shared.write_lock().lock().await;
    let cleared = gc::clear_source_chunks(state, bucket, &uris).await?;
    let now = marker::now_rfc3339();
    for m in &targets {
        // PENDING, current fingerprint, evidence cleared; the last-observed
        // signature is KEPT — unchanged sig + PENDING is the scanner's
        // Enqueue case (crash-residue shape), so the very next scan rewakes
        // these sources even without an explicit wake.
        let pending = SourceMarker {
            object_key: m.object_key.clone(),
            etag_sig: m.etag_sig.clone(),
            content_hash: String::new(),
            pipeline_fingerprint: fingerprint.to_string(),
            status: SourceStatus::Pending,
            status_reason: None,
            retries: 0,
            retries_exhausted: false,
            misses: 0,
            updated_at: now.clone(),
            chunks: None,
            engines: None,
        };
        state
            .storage
            .save_source(bucket, &marker::object_key_hash(&m.object_key), &pending)
            .await
            .map_err(|e| e.to_string())?;
    }
    drop(_guard);
    let flipped = targets.len();
    let job_id = shared.register_reindex(bucket, flipped);
    eprintln!(
        "ingest: reindex {bucket}: {flipped} source(s) -> PENDING, {cleared} old chunk(s) \
         cleared (job {job_id})"
    );
    Ok(ReindexOutcome {
        job_id,
        flipped,
        cleared,
    })
}

/// `vectorfs serve --reindex` selection rule: re-index ONLY the buckets the
/// fingerprint gate would have refused (§主会话裁决 — "对该根所有指纹不符的桶
/// 执行重嵌"). `Ok(None)` = this bucket is already consistent.
pub async fn reindex_bucket_if_stale(
    state: &AppState,
    shared: &Arc<IngestShared>,
    bucket: &str,
) -> Result<Option<ReindexOutcome>, String> {
    let current = shared
        .fingerprint_of(bucket)
        .ok_or_else(|| format!("bucket {bucket:?} is not ingest-configured"))?;
    let markers = state
        .storage
        .list_sources(bucket)
        .await
        .map_err(|e| e.to_string())?;
    let stale = markers
        .iter()
        .any(|m| m.status != SourceStatus::Deleting && m.pipeline_fingerprint != current);
    if !stale {
        return Ok(None);
    }
    reindex_bucket(state, shared, bucket).await.map(Some)
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
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n{}",
            fixture_dir().display().to_string(),
            extra
        );
        let cfg = crate::pipeline::config::load_pipeline_config(&text).unwrap();
        (text, cfg)
    }

    fn test_state(dir: &Path) -> Arc<AppState> {
        Arc::new(AppState::new(
            Arc::new(Storage::file(dir)),
            None,
            "us-east-1".into(),
            "000000000000".into(),
            false,
            None,
        ))
    }

    /// Pin + acquire the writer locks for every configured bucket — the test
    /// counterpart of serve's startup pass (§5.1; jobs refuse unlocked
    /// buckets by design, and GC only covers held buckets).
    async fn hold_locks(state: &AppState, buckets: impl Iterator<Item = String>) {
        for vb in buckets {
            state.locks.pin(&vb);
            state.locks.ensure_held(&vb).await.unwrap();
        }
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
        hold_locks(&state, core.buckets.keys().cloned()).await;
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
            .unwrap_or_else(|| panic!("marker for {key} not found"))
    }

    #[tokio::test]
    async fn full_chain_indexed_queryable_and_side_effect_free() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        // user objects land in the instance root, inside the scope
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        // v3: user data lives ONLY in the source bucket; the system bucket
        // (docs-vf) and the daemon home (.vectorfs) are the other two trees
        let user_files: Vec<String> = outside
            .iter()
            .filter(|p| !p.contains("/.vectorfs") && !p.contains("/docs-vf/"))
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
        hold_locks(&state, core.buckets.keys().cloned()).await;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            handle.notify(),
            shutdown_rx,
        );
        rt.spawn(&handle);

        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        hold_locks(&state, core.buckets.keys().cloned()).await;
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let rt = IngestRuntime::new(
            IngestShared::new(core),
            state.clone(),
            Arc::new(Notify::new()),
            rx,
        );

        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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

        // deletion => the vanish N-strike gate (v3 R5: TWO consecutive
        // absent scans) then the reap — the flip+GC convergence the `vf rm`
        // 202 poll contract observes, one strike later. A non-source file
        // keeps the listing non-empty (the zero-live guard 护栏② would
        // otherwise refuse the reap of a bucket that emptied completely).
        std::fs::write(file.with_file_name("keep.png"), b"not a source").unwrap();
        std::fs::remove_file(&file).unwrap();
        rt.scan_once().await; // strike 1: counted, no flip
        let struck = state
            .storage
            .load_source("docs", &marker::object_key_hash("docs/a.txt"))
            .await
            .unwrap();
        assert_eq!(
            struck.as_ref().unwrap().misses,
            1,
            "first absence counts a strike"
        );
        rt.scan_once().await; // strike 2: flip + reap in the same pass
        let gone = state
            .storage
            .load_source("docs", &marker::object_key_hash("docs/a.txt"))
            .await
            .unwrap();
        assert!(gone.is_none(), "marker reaped with the chunks: {gone:?}");
        let count = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        assert_eq!(count, 0, "every chunk of the gone source deleted");
    }

    #[tokio::test]
    async fn unicode_and_special_char_keys_scan_without_phantoms() {
        // T6-found latent bug: object_store's Path canonical form percent-
        // encodes non-ASCII (and `%#*?` etc.) — the scanner must compare
        // DECODED keys against markers, or every such key phantom-creates an
        // encoded-twin marker and vanish-flips its real one each scan.
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
            misses: 0,
            updated_at: "t".into(),
            chunks: Some(BTreeMap::from([("text".to_string(), 1)])),
            engines: None,
        };
        state
            .storage
            .save_source("docs", &marker::object_key_hash("docs/a.txt"), &stale)
            .await
            .unwrap();

        let (_, cfg) = tiny_config("");
        let err = fingerprint_gate(&state.storage, &cfg, Path::new("/tmp/vf-gate"))
            .await
            .unwrap_err();
        assert!(err.contains("fingerprint mismatch"), "{err}");
        assert!(
            err.contains("vectorfs serve --reindex --data-dir /tmp/vf-gate"),
            "the startup remedy must be the flag: {err}"
        );
        assert!(
            err.contains("vf reindex --bucket docs"),
            "the running-daemon remedy must be named: {err}"
        );
        assert!(err.contains("docs/a.txt"), "{err}");

        // after wiping the markers the gate passes
        state
            .storage
            .delete_source("docs", &marker::object_key_hash("docs/a.txt"))
            .await
            .unwrap();
        assert!(
            fingerprint_gate(&state.storage, &cfg, Path::new("/tmp"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn in_progress_crash_residue_recovers_without_lock() {
        // an IN_PROGRESS marker whose object is UNCHANGED (the kill -9 shape)
        // is re-enqueued by the next scan and converges
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
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

    // ---- T7: reindex / GC / stats ------------------------------------------

    /// tiny-embedder pipeline with a PARAMETERIZED chunk window and model
    /// alias: the model name moves the derived key (→ a different derived
    /// index NAME) and the chunker size moves the fingerprint, while the
    /// fixture path keeps both configs loading the same 8-dim artifact.
    fn sized_config(chunk_size: usize, model: &str) -> PipelineConfig {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"{model}\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\" }}\n\
             chunk = {{ kind = \"token-window\", size = {chunk_size}, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n",
            fixture_dir().display().to_string(),
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// Un-spawned runtime + handle over a caller-supplied config (the
    /// reindex tests reconfigure the SAME data dir under a second pipeline).
    async fn runtime_for(
        dir: &Path,
        cfg: &PipelineConfig,
    ) -> (Arc<IngestRuntime>, Arc<IngestHandle>, Arc<AppState>) {
        let core = IngestCore::load(cfg, Path::new("/nonexistent-models"))
            .await
            .expect("fixture embedder loads");
        let state = test_state(dir);
        hold_locks(&state, core.buckets.keys().cloned()).await;
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            Arc::new(Notify::new()),
            rx,
        );
        (rt, handle, state)
    }

    /// A body long enough that size 32 and size 96 windows split it very
    /// differently (the SHRINK case: fewer chunks after reindex).
    const LONG_BODY: &str = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda \
         mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega alpha beta gamma delta \
         epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon \
         phi chi psi omega alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";

    /// keys of one uri across EVERY index of the bucket (residue check).
    async fn keys_of_uri(state: &AppState, vb: &str, uri: &str) -> usize {
        let mut n = 0;
        for meta in state.storage.list_indexes(vb).await.unwrap() {
            n += state
                .storage
                .scan_vector_metadata(vb, &meta.index_name)
                .await
                .unwrap()
                .into_iter()
                .filter(|(k, m)| {
                    marker::is_chunk_key(k)
                        && m.as_ref()
                            .and_then(|m| m.get("x-vf-src-uri"))
                            .and_then(|v| v.as_str())
                            == Some(uri)
                })
                .count();
        }
        n
    }

    /// The acceptance run of §5.10.5 "清旧块 → 重写 → 更新标记": ingest under
    /// size 32, reconfigure to size 96 (+ a new model alias ⇒ a NEW derived
    /// index name), watch the gate refuse, reindex, converge with zero
    /// residue, and see the gate pass again.
    #[tokio::test]
    async fn reindex_full_flow_shrink_zero_residue_and_gate_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), LONG_BODY).unwrap();

        // (1) ingest under the OLD pipeline (size 32 / model tiny-fixture)
        let old_cfg = sized_config(32, "tiny-fixture");
        let (rt1, _h1, state) = runtime_for(dir.path(), &old_cfg).await;
        rt1.scan_once().await;
        rt1.drain_queue().await;
        let m1 = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(m1.status, SourceStatus::Indexed);
        // engine observability: a completing marker records the engine table
        // that produced it (doctor compares against the running one)
        assert_eq!(
            m1.engines.as_ref().unwrap(),
            &crate::pipeline::engines::engine_versions_owned()
        );
        let old_chunks = m1.chunks.as_ref().unwrap()["text"];
        assert!(old_chunks >= 8, "32-window over a long body: {old_chunks}");
        let old_spec = rt1.shared.core.buckets["docs"].derived["tiny"].clone();
        assert_eq!(
            state
                .storage
                .count_vectors("docs", &old_spec.index_name)
                .await
                .unwrap(),
            old_chunks as usize
        );

        // (2) reconfigure: the gate refuses and names BOTH remedies
        let new_cfg = sized_config(96, "tiny-v2");
        let err = fingerprint_gate(&state.storage, &new_cfg, dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("--reindex"), "{err}");

        // (3) serve --reindex selection + mechanism (pre-spawn shape); the
        // old "daemon" releases its bucket locks first (same-process restart)
        state.locks.release_all().await;
        let (rt2, handle2, state2) = runtime_for(dir.path(), &new_cfg).await;
        let outcome = reindex_bucket_if_stale(&state2, handle2.shared(), "docs")
            .await
            .unwrap()
            .expect("stale bucket reindexes");
        assert_eq!(outcome.flipped, 1);
        assert_eq!(outcome.cleared, old_chunks as usize, "every old chunk gone");
        // a consistent bucket is a no-op
        assert!(
            reindex_bucket_if_stale(&state2, handle2.shared(), "docs")
                .await
                .unwrap()
                .is_none()
        );

        // markers flipped PENDING under the NEW fingerprint
        let flipped = status_of(&state2, "docs", "docs/a.txt").await;
        assert_eq!(flipped.status, SourceStatus::Pending);
        assert_eq!(
            flipped.pipeline_fingerprint,
            new_cfg.fingerprint("docs").unwrap()
        );
        assert!(flipped.chunks.is_none(), "completion evidence cleared");
        assert!(
            flipped.engines.is_none(),
            "engine evidence cleared with the rest"
        );

        // (4) rewrite: scan-and-diff re-embeds under the new pipeline
        rt2.scan_once().await;
        rt2.drain_queue().await;
        let m2 = status_of(&state2, "docs", "docs/a.txt").await;
        assert_eq!(m2.status, SourceStatus::Indexed);
        let new_chunks = m2.chunks.as_ref().unwrap()["text"];
        assert!(
            new_chunks < old_chunks,
            "96-window SHRINKS the chunk set: {new_chunks} vs {old_chunks}"
        );
        let new_spec = rt2.shared.core.buckets["docs"].derived["tiny"].clone();
        assert_ne!(
            new_spec.index_name, old_spec.index_name,
            "model alias moves the derived key"
        );

        // (5) zero residue: the new index holds exactly the new chunks; the
        // OLD derived index still EXISTS, emptied (kept for the operator)
        assert_eq!(
            state2
                .storage
                .count_vectors("docs", &new_spec.index_name)
                .await
                .unwrap(),
            new_chunks as usize
        );
        assert!(
            state2
                .storage
                .load_index("docs", &old_spec.index_name)
                .await
                .unwrap()
                .is_some(),
            "old derived index is KEPT (M2 never auto-deletes)"
        );
        assert_eq!(
            state2
                .storage
                .count_vectors("docs", &old_spec.index_name)
                .await
                .unwrap(),
            0,
            "…but emptied"
        );
        let uri = marker::object_key_hash("docs/a.txt");
        assert_eq!(
            keys_of_uri(&state2, "docs", &uri).await,
            new_chunks as usize,
            "no chunk of this source lives anywhere else"
        );

        // (6) the gate now passes and the job registry shows real progress
        assert!(
            fingerprint_gate(&state2.storage, &new_cfg, dir.path())
                .await
                .is_ok()
        );
        let job = handle2.shared().reindex_job("docs").unwrap();
        assert_eq!(job.status, "done");
        assert_eq!(job.done, 1);
        assert_eq!(job.total, 1);

        // (7) KB statistics: initial ingest was "new", the reindex rewrite
        // "modified"
        let stats = handle2.shared().stats_of("docs");
        assert!(stats.scanned >= 1, "{stats:?}");
        assert_eq!(
            stats.new_indexed, 0,
            "this process never saw a first ingest"
        );
        assert_eq!(stats.modified_indexed, 1, "{stats:?}");
        assert_eq!(stats.failed, 0, "{stats:?}");
    }

    /// The quiesce barrier's enqueue rule (job-level serialization, 2026-11-05):
    /// while a bucket's barrier is held, `enqueue` refuses its NEW jobs — no
    /// dedup entry, no channel send. A refused source needs no repair: the
    /// reindex flip turns it PENDING and the trailing wake re-covers it.
    #[tokio::test]
    async fn reindex_barrier_refuses_new_enqueues() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), LONG_BODY).unwrap();

        let cfg = sized_config(32, "tiny-fixture");
        let (rt, handle, _state) = runtime_for(dir.path(), &cfg).await;
        handle.shared().hold_reindex_barrier("docs");
        let job = Job {
            vb: "docs".into(),
            object_key: "docs/a.txt".into(),
            key_hash: marker::object_key_hash("docs/a.txt"),
        };
        rt.enqueue(job).await;
        assert!(
            !handle.shared().is_inflight("docs", "docs/a.txt"),
            "a barrier-held bucket must not accept new jobs"
        );
        assert_eq!(
            handle.shared().bucket_inflight_count("docs"),
            0,
            "and nothing of it may linger queued"
        );
        // release, and the same enqueue goes through (the refusal is the
        // barrier's, not a broken queue)
        handle.shared().release_reindex_barrier("docs");
        let job = Job {
            vb: "docs".into(),
            object_key: "docs/a.txt".into(),
            key_hash: marker::object_key_hash("docs/a.txt"),
        };
        rt.enqueue(job).await;
        assert!(handle.shared().is_inflight("docs", "docs/a.txt"));
        rt.drain_queue().await;
    }

    /// The barrier's DRAIN rule — the race the §10.8 debt described, made
    /// deterministic with the drain_queue harness: a job enqueued BEFORE the
    /// reindex pass must BLOCK the pass (pre-barrier, the enumeration would
    /// have skipped it as in-flight); once the job drains, the pass proceeds
    /// and flips that SAME source — proof the clear+flip section runs
    /// strictly between jobs, never around one.
    #[tokio::test]
    async fn reindex_barrier_waits_for_inflight_then_flips_it_too() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), LONG_BODY).unwrap();

        let cfg = sized_config(32, "tiny-fixture");
        let (rt, handle, state) = runtime_for(dir.path(), &cfg).await;
        // one job sits QUEUED (= in the in-flight set; no worker spawned —
        // the marker stays PENDING until process_job flips IN_PROGRESS)
        rt.scan_once().await;
        assert!(handle.shared().is_inflight("docs", "docs/a.txt"));

        // the reindex pass starts and must STALL on the barrier's drain wait
        // (spawned over cloned Arcs — the harness keeps its own for asserts)
        let state_c = state.clone();
        let shared_c = handle.shared().clone();
        let mut pass =
            tokio::spawn(async move { reindex_bucket(&state_c, &shared_c, "docs").await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut pass)
                .await
                .is_err(),
            "reindex must not complete while a job of this bucket is in flight"
        );
        // the barrier is held for the duration (new work refused)
        assert!(handle.shared().reindex_barrier_held("docs"));

        // the job drains: the barrier releases, the pass completes, and the
        // SAME source is flipped (the pre-barrier pass would have SKIPPED it
        // as in-flight and reported flipped == 0)
        rt.drain_queue().await;
        let outcome = pass.await.unwrap().expect("reindex completes after drain");
        assert_eq!(outcome.flipped, 1, "the drained source is a flip target");
        let flipped = status_of(&state, "docs", "docs/a.txt").await;
        assert_eq!(flipped.status, SourceStatus::Pending);
        assert_eq!(
            flipped.pipeline_fingerprint,
            cfg.fingerprint("docs").unwrap()
        );
        // barrier released afterwards (the bucket accepts work again)
        assert!(!handle.shared().reindex_barrier_held("docs"));
    }

    /// Orphan sweep: chunks whose marker is GONE (not merely DELETING) are
    /// reaped by the idle-gated periodic sweep — but only when the file
    /// layer owes no other work (the optimize discipline).
    #[tokio::test]
    async fn orphan_sweep_reaps_markerless_chunks_when_idle_only() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), LONG_BODY).unwrap();
        std::fs::write(docs.join("b.txt"), "a second, shorter body").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        let total = state
            .storage
            .count_vectors("docs", &spec.index_name)
            .await
            .unwrap();
        // vanish a.txt AND its marker in one go (crash / operator wipe):
        // nothing is DELETING — only the orphan sweep can collect it
        std::fs::remove_file(docs.join("a.txt")).unwrap();
        state
            .storage
            .delete_source("docs", &marker::object_key_hash("docs/a.txt"))
            .await
            .unwrap();
        // a new owed source at the same moment: the sweep must WAIT
        std::fs::write(docs.join("c.txt"), "owed work blocks the janitor").unwrap();
        rt.scan_once().await;
        assert_eq!(
            state
                .storage
                .count_vectors("docs", &spec.index_name)
                .await
                .unwrap(),
            total,
            "sweep skipped while c.txt is PENDING/in flight"
        );
        rt.drain_queue().await; // c.txt INDEXED
        rt.scan_once().await; // now idle -> sweep
        let b_chunks = status_of(&state, "docs", "docs/b.txt")
            .await
            .chunks
            .unwrap()["text"] as usize;
        let c_chunks = status_of(&state, "docs", "docs/c.txt")
            .await
            .chunks
            .unwrap()["text"] as usize;
        assert_eq!(
            state
                .storage
                .count_vectors("docs", &spec.index_name)
                .await
                .unwrap(),
            b_chunks + c_chunks,
            "orphan chunks reaped exactly (b + c remain)"
        );
        let c_chunks = status_of(&state, "docs", "docs/c.txt")
            .await
            .chunks
            .unwrap()["text"] as usize;
        assert_eq!(
            keys_of_uri(&state, "docs", &marker::object_key_hash("docs/c.txt")).await,
            c_chunks
        );
    }

    /// 防误删 (GC 判据 hard gate): inside a MANAGED index a non-derived-format
    /// key survives; chunk-format keys with source attribution survive in a
    /// USER-CREATED (unstamped) index — even when their uri points at a
    /// vanished object.
    #[tokio::test]
    async fn gc_spares_foreign_keys_and_unstamped_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = test_runtime(dir.path(), "").await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), LONG_BODY).unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        let spec = rt.shared.core.buckets["docs"].derived["tiny"].clone();
        let gone_uri = marker::object_key_hash("docs/a.txt");
        let ghost_uri = marker::object_key_hash("docs/ghost.txt");

        // a USER-CREATED index (no x_vf_derived stamp) holding a
        // chunk-format key whose uri points at a vanished object
        state
            .storage
            .create_index(&crate::store::IndexMeta {
                vector_bucket_name: "docs".into(),
                index_name: "user-ix".into(),
                index_arn: "arn".into(),
                creation_time_rfc3339: "t".into(),
                creation_time_epoch: 0.0,
                data_type: "float32".into(),
                dimension: 8,
                distance_metric: "cosine".into(),
                non_filterable_metadata_keys: None,
                index_mode: None,
                x_vf_derived: None,
            })
            .await
            .unwrap();
        let row = |key: String, uri: String| crate::store::VectorRow {
            key,
            vector: vec![0.5; 8],
            metadata_json: Some(format!(r#"{{"x-vf-src-uri":"{uri}"}}"#)),
            filterable: BTreeMap::from([(
                "x-vf-src-uri".to_string(),
                crate::store::MetaMirror::Str(uri),
            )]),
        };
        state
            .storage
            .upsert_vectors(
                "docs",
                "user-ix",
                8,
                vec![row(
                    marker::chunk_key(&ghost_uri, "text", 0),
                    gone_uri.clone(),
                )],
            )
            .await
            .unwrap();
        // …and a NON-derived-format key living INSIDE the managed index
        state
            .storage
            .upsert_vectors(
                "docs",
                &spec.index_name,
                8,
                vec![row("user-row-1".to_string(), gone_uri.clone())],
            )
            .await
            .unwrap();

        // the object vanishes: its OWN chunks are reaped (after the v3
        // N-strike gate — two consecutive absent scans), the two foreign
        // rows survive. keep.png keeps the listing non-empty (护栏② would
        // refuse a fully-emptied bucket's reap).
        std::fs::write(docs.join("keep.png"), b"not a source").unwrap();
        std::fs::remove_file(docs.join("a.txt")).unwrap();
        rt.scan_once().await; // strike 1
        rt.scan_once().await; // strike 2: flip + reap

        let derived_rows = state
            .storage
            .scan_vector_metadata("docs", &spec.index_name)
            .await
            .unwrap();
        assert_eq!(
            derived_rows.len(),
            1,
            "only user-row-1 remains: {derived_rows:?}"
        );
        assert_eq!(derived_rows[0].0, "user-row-1");
        let user_rows = state
            .storage
            .scan_vector_metadata("docs", "user-ix")
            .await
            .unwrap();
        assert_eq!(
            user_rows.len(),
            1,
            "unstamped index untouched: {user_rows:?}"
        );
        assert!(marker::is_chunk_key(&user_rows[0].0));

        // the DELETING reap advanced the KB deleted counter
        let stats = rt.shared.stats_of("docs");
        assert_eq!(stats.deleted, 1, "{stats:?}");
    }

    // ---- M2.1: extractor fallback chains -----------------------------------

    /// The committed hello fixture (same bytes the extract/golden suites
    /// use; chain tests exercise the runner, not engine behavior).
    const HELLO_PDF: &[u8] = include_bytes!("../../tests/golden/hello.pdf");

    /// Construct a registered ExtractorKind the way the worker does.
    fn kind_of(section: &str, body: &str) -> ExtractorKind {
        ExtractorKind::from_config(
            section,
            &toml::from_str::<toml::Value>(body).expect("test kind body"),
            None,
        )
        .expect("test kind parses")
    }

    #[test]
    fn chain_first_winner_never_pulls_the_second_member() {
        // laziness IS "the second member never runs": the counter only
        // moves when pick_from_chain PULLS an item, and pulling is what
        // executes a member in the worker (extract runs inside the map)
        let mut pulled = 0usize;
        let members = [
            ("first".to_string(), Ok(vec![Part::text("winner content")])),
            (
                "second".to_string(),
                Err(PipelineError::extract("must never be computed")),
            ),
        ]
        .into_iter()
        .inspect(|_| pulled += 1);
        let (winner, parts) = pick_from_chain("txt", members).expect("first member wins");
        assert_eq!(parts, vec![Part::text("winner content")]);
        assert_eq!(
            winner, "first",
            "the winner's name rides along (progress line)"
        );
        assert_eq!(pulled, 1, "the chain advanced past the winner");
    }

    #[test]
    fn chain_err_and_every_empty_ok_shape_yield_to_the_next_member() {
        // Err yields
        let members = vec![
            (
                "a".to_string(),
                Err(PipelineError::extract("cannot handle this content")),
            ),
            ("b".to_string(), Ok(vec![Part::text("b wins")])),
        ];
        let (winner, parts) = pick_from_chain("txt", members).expect("b wins");
        assert_eq!(parts, vec![Part::text("b wins")]);
        assert_eq!(winner, "b");
        // ...and so does every empty shape of Ok (no Parts, empty text,
        // whitespace-only text — the same "nothing" bar the kinds' own
        // refusals use)
        for empty in [
            Vec::<Part>::new(),
            vec![Part::text("")],
            vec![Part::text(" \n\t ")],
        ] {
            let members = vec![
                ("a".to_string(), Ok(empty.clone())),
                ("b".to_string(), Ok(vec![Part::text("b wins")])),
            ];
            let (winner, parts) = pick_from_chain("txt", members)
                .unwrap_or_else(|e| panic!("empty shape {empty:?} must yield: {e}"));
            assert_eq!(parts, vec![Part::text("b wins")]);
            assert_eq!(winner, "b");
        }
    }

    #[test]
    fn chain_exhausted_aggregates_every_member_reason() {
        let members = vec![
            (
                "insp".to_string(),
                Err(PipelineError::extract(
                    "pdf-inspector classification: page 1: scanned",
                )),
            ),
            (
                "plain".to_string(),
                Err(PipelineError::extract("no extractable text")),
            ),
        ];
        let err = pick_from_chain("pdf", members).unwrap_err();
        assert_eq!(
            err.to_string(),
            "extract failed: pdf chain [insp→plain]: insp: pdf-inspector classification: \
             page 1: scanned; plain: no extractable text"
        );
    }

    #[test]
    fn chain_single_member_passes_the_message_through_verbatim() {
        // a one-member chain (every pre-M2.1 route, and `auto`) keeps the
        // member's own message byte-stable — no wrapper to describe a chain
        let members = vec![("only".to_string(), Err(PipelineError::extract("boom")))];
        assert_eq!(
            pick_from_chain("txt", members).unwrap_err().to_string(),
            "extract failed: boom"
        );
        let members = vec![("only".to_string(), Ok(Vec::<Part>::new()))];
        let msg = pick_from_chain("txt", members).unwrap_err().to_string();
        assert!(msg.contains("produced no extractable content"), "{msg}");
        assert!(!msg.contains("chain"), "no wrapper on one member: {msg}");
    }

    // ---- M3-T1: doc-side modality routing (gate + counts, pure cores) ----

    #[test]
    fn image_parts_count_as_extracted_content() {
        // an image-only extraction must WIN its chain (an image Part's text
        // channel is empty by construction — the pre-image content bar
        // would have yielded it as "empty")
        assert!(has_extracted_content(&[Part::image(
            "image/png",
            &b"\x89PNG\r\n\x1a\n"[..]
        )]));
        // mixed: the image alone rescues an otherwise-blank extraction
        assert!(has_extracted_content(&[
            Part::text("  \n\t "),
            Part::image("image/png", &b"\x89PNG\r\n\x1a\n"[..])
        ]));
        // and the old bars stand: empty/blank text alone is still nothing
        assert!(!has_extracted_content(&[]));
        assert!(!has_extracted_content(&[Part::text("")]));
        assert!(!has_extracted_content(&[Part::text(" \n\t ")]));
    }

    #[test]
    fn chunk_counts_per_modality_text_always_image_only_when_present() {
        let text = |i: u32| Chunk {
            index: i,
            text: format!("t{i}"),
            image: None,
        };
        let image = |i: u32| Chunk {
            index: i,
            text: String::new(),
            image: Some(crate::pipeline::ImagePayload {
                bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
                mime: "image/png".to_string(),
            }),
        };
        // the pre-image marker shape, byte-stable: text-only file
        assert_eq!(
            chunk_counts(Some(&[text(0), text(1)])),
            BTreeMap::from([("text".to_string(), 2u32)])
        );
        // no chunks at all (never-completed path): still just text=0
        assert_eq!(
            chunk_counts(None),
            BTreeMap::from([("text".to_string(), 0u32)])
        );
        // mixed: image appears ONLY when nonzero
        assert_eq!(
            chunk_counts(Some(&[text(0), image(1), image(2)])),
            BTreeMap::from([("text".to_string(), 1u32), ("image".to_string(), 2u32)])
        );
    }

    /// The gate against the REAL loaded tiny embedder (the .so fixture the
    /// module banner promises; both M2 kinds declare &[Text], so the tiny
    /// instance IS the "text-only world" the M3-T1 gate must refuse).
    #[tokio::test]
    async fn modality_gap_reason_shapes_the_loud_refusal() {
        let (_text, cfg) = tiny_config("");
        let core = IngestCore::load(&cfg, Path::new("/nonexistent-models"))
            .await
            .expect("fixture embedder loads");
        let tiny = core.embedder("tiny").expect("tiny declared");
        let name = "tiny".to_string();
        let fanned = vec![(&name, tiny)];

        let text = |i: u32| Chunk {
            index: i,
            text: format!("t{i}"),
            image: None,
        };
        let image = |i: u32| Chunk {
            index: i,
            text: String::new(),
            image: Some(crate::pipeline::ImagePayload {
                bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
                mime: "image/png".to_string(),
            }),
        };

        // text-only chunks: covered (the M2 world — gate silent)
        assert_eq!(modality_gap_reason(&[text(0), text(1)], &fanned), None);
        // empty chunk set: nothing to strand
        assert_eq!(modality_gap_reason(&[], &fanned), None);
        // the pure-protocol face (no embedders listed): the gate skips,
        // same carve-out as the config-time carry-over check
        assert_eq!(modality_gap_reason(&[image(0)], &[]), None);

        // an image chunk with a text-only fan-out: the loud aggregate —
        // counts by modality, the roster of what IS accepted, and the
        // remedy phrased by the modality
        let reason =
            modality_gap_reason(&[text(0), image(1), image(2)], &fanned).expect("image stranded");
        assert!(reason.contains("2 image chunk(s)"), "{reason}");
        assert!(
            reason.contains("no embedder that accepts them on its doc side"),
            "{reason}"
        );
        assert!(reason.contains("tiny (doc modalities: text)"), "{reason}");
        assert!(
            reason.contains("add an embedder that accepts image documents"),
            "{reason}"
        );
        // deterministic (BTreeSet ordering: no roster/aggregate flapping)
        assert_eq!(
            modality_gap_reason(&[text(0), image(1), image(2)], &fanned),
            Some(reason.clone())
        );
    }

    #[test]
    fn chain_over_real_kinds_winner_parts_equal_direct_extraction() {
        // in-process over registered kinds: the chain winner's Parts are
        // EXACTLY the direct extraction's (no cross-member merging)
        let pdf_text = kind_of("t", "kind = \"pdf-text\"");
        let insp = kind_of("t", "kind = \"pdf-inspector\"");
        let direct = Extractor::extract(&pdf_text, HELLO_PDF, "application/pdf").unwrap();
        let members = vec![
            (
                "plain".to_string(),
                Extractor::extract(&pdf_text, HELLO_PDF, "application/pdf"),
            ),
            (
                "insp".to_string(),
                Extractor::extract(&insp, HELLO_PDF, "application/pdf"),
            ),
        ];
        let (winner, parts) = pick_from_chain("pdf", members).unwrap();
        assert_eq!(parts, direct);
        assert_eq!(winner, "plain");
    }

    /// tiny-embedder pipeline with a parameterized `pdf` extract route over
    /// two declared PDF instances (the chain e2e fixture).
    fn pdf_route_config(route: &str) -> PipelineConfig {
        let text = format!(
            "[extractor.insp]\nkind = \"pdf-inspector\"\n\n\
             [extractor.plain]\nkind = \"pdf-text\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = \
             true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ pdf = {route} }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\nretries = 0\n",
            fixture_dir().display().to_string(),
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// THE cross-parser recovery run (golden-corpus-adjacent, real engines),
    /// now under M2.1 step 2 routing: the mixed text+image PDF probes
    /// mixed-text-scan, pdf-inspector REFUSES that class (its classification
    /// gate) while pdf-text is Capable — the router selects plain directly,
    /// the inspector never executes, and the file INDEXES where the
    /// single-member insp route stays FAILED. Pre-routing this same file
    /// recovered by the inspector's Err yielding (same winner, one wasted
    /// extraction); routing reaches it without the wasted attempt — and the
    /// stderr line names the routing ("INDEXED via plain (class=...)"; run
    /// with --nocapture to see it).
    #[tokio::test]
    async fn chain_recovers_across_parsers_where_the_first_member_yields() {
        let bytes = crate::pipeline::extract::pdf_inspector::tests::mixed_text_image_pdf();

        // (1) chain ["insp", "plain"]: mixed class routes to plain (Capable;
        // insp refuses) — INDEXED via the pdfium half of the divergence
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("mixed.pdf"), &bytes).unwrap();
        let (rt, _h, state) =
            runtime_for(dir.path(), &pdf_route_config("[\"insp\", \"plain\"]")).await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/mixed.pdf").await;
        assert_eq!(m.status, SourceStatus::Indexed);
        let chunks = m.chunks.as_ref().unwrap()["text"];
        assert!(chunks >= 1, "the pdfium half of the divergence: {m:?}");
        // the winner's Parts walked the ordinary path: vectors landed
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        assert_eq!(
            state
                .storage
                .count_vectors("docs", &spec.index_name)
                .await
                .unwrap() as u32,
            chunks
        );

        // (1b) the REVERSED chain over a TEXT-NATIVE PDF: both members are
        // Preferred for text-native, the tie resolves to CONFIG ORDER —
        // [plain, insp] runs plain first, [insp, plain] runs insp first
        // (pinned precisely at the router in the route unit tests); the
        // file indexes either way.
        let dir_tb = tempfile::tempdir().unwrap();
        let docs_tb = dir_tb.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs_tb).unwrap();
        std::fs::write(docs_tb.join("hello.pdf"), HELLO_PDF).unwrap();
        let (rt_tb, _h_tb, state_tb) =
            runtime_for(dir_tb.path(), &pdf_route_config("[\"plain\", \"insp\"]")).await;
        rt_tb.scan_once().await;
        rt_tb.drain_queue().await;
        assert_eq!(
            status_of(&state_tb, "docs", "docs/hello.pdf").await.status,
            SourceStatus::Indexed
        );

        // (2) the SAME mixed bytes under the single-member "insp" route:
        // FAILED, fail-loud intact (no relay configured), reason verbatim —
        // and NO routing vocabulary: a single member never probes
        // (members.len() > 1 gate), so no class annotation, no wrapper
        let dir2 = tempfile::tempdir().unwrap();
        let docs2 = dir2.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs2).unwrap();
        std::fs::write(docs2.join("mixed.pdf"), &bytes).unwrap();
        state.locks.release_all().await; // same-process restart
        let (rt2, _h2, state2) = runtime_for(dir2.path(), &pdf_route_config("\"insp\"")).await;
        rt2.scan_once().await;
        rt2.drain_queue().await;
        let m2 = status_of(&state2, "docs", "docs/mixed.pdf").await;
        assert_eq!(m2.status, SourceStatus::Failed);
        assert!(m2.retries_exhausted);
        let reason = m2.status_reason.expect("reason");
        assert!(reason.contains("scanned"), "{reason}");
        assert!(
            !reason.contains("chain"),
            "one member, no wrapper: {reason}"
        );
        assert!(
            !reason.contains("class=") && !reason.contains("refuses ("),
            "single-member chains never probe/route: {reason}"
        );
    }

    /// Full exhaustion on the UNKNOWN-class path (M2.1 chain semantics,
    /// byte-stable): corrupt bytes defeat the probe itself (detect errs ⇒
    /// Unknown ⇒ full config order), then BOTH members genuinely run and
    /// yield — the file FAILED with every member's reason on record and no
    /// routing vocabulary anywhere in the message.
    #[tokio::test]
    async fn chain_exhausted_failed_lists_every_member_reason() {
        let mut corrupt = b"%PDF-1.7\n".to_vec();
        corrupt.extend_from_slice(&[0x00, 0xff, 0x13, 0x37, 0xde, 0xad, 0xbe, 0xef]);
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("corrupt.pdf"), &corrupt).unwrap();
        let (rt, _h, state) =
            runtime_for(dir.path(), &pdf_route_config("[\"insp\", \"plain\"]")).await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/corrupt.pdf").await;
        assert_eq!(m.status, SourceStatus::Failed);
        assert!(m.retries_exhausted);
        let reason = m.status_reason.expect("reason");
        assert!(reason.contains("pdf chain [insp→plain]"), "{reason}");
        assert!(reason.contains("insp: "), "{reason}");
        assert!(reason.contains("plain: "), "{reason}");
        // Unknown class: no annotation — the pre-routing aggregate verbatim
        assert!(!reason.contains("class="), "{reason}");
        // nothing was indexed — the derived index was never even created
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        assert!(
            state
                .storage
                .load_index("docs", &spec.index_name)
                .await
                .unwrap()
                .is_none(),
            "a fully-exhausted chain must not create or write any index"
        );
    }

    /// The all-Refuses terminal (M2.1 step 2): an image-only PDF probes
    /// scan-dominant and NO member of ["insp", "plain"] will serve it (the
    /// inspector's classification gate; pdf-text has no text layer to
    /// read). Nobody executes — the file parks immediately with the
    /// aggregate routing reason naming the class, every member's refusal,
    /// and the missing capability. More informative than running the chain
    /// into two guaranteed failures (the pre-routing behavior for the same
    /// file was the chain-exhaustion aggregate above's shape).
    #[tokio::test]
    async fn routing_all_refuse_parks_without_executing_any_member() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("scan.pdf"),
            crate::pipeline::extract::pdf_inspector::tests::image_only_pdf(),
        )
        .unwrap();
        let (rt, _h, state) =
            runtime_for(dir.path(), &pdf_route_config("[\"insp\", \"plain\"]")).await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/scan.pdf").await;
        assert_eq!(m.status, SourceStatus::Failed);
        assert!(m.retries_exhausted, "all-refuse is a deterministic park");
        let reason = m.status_reason.expect("reason");
        assert!(reason.contains("pdf class=scan-dominant"), "{reason}");
        assert!(reason.contains("insp refuses ("), "{reason}");
        assert!(reason.contains("plain refuses ("), "{reason}");
        assert!(reason.contains("OCR"), "{reason}");
        assert!(
            reason.contains("add an OCR-capable extractor"),
            "the remedy must name the missing capability: {reason}"
        );
        // nothing was indexed — no member ever ran, no index was touched
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        assert!(
            state
                .storage
                .load_index("docs", &spec.index_name)
                .await
                .unwrap()
                .is_none(),
            "an all-refusing chain must not create or write any index"
        );
    }

    // ---- M2.1 step 2: the pure router (probe → affinity → route) ----------

    /// A two-PDF-kind chain, config order parameterized (inspector first,
    /// pdf-text second).
    fn pdf_members(first: &str, second: &str) -> Vec<(String, ExtractorKind)> {
        vec![
            (first.to_string(), kind_of("t", "kind = \"pdf-inspector\"")),
            (second.to_string(), kind_of("t", "kind = \"pdf-text\"")),
        ]
    }

    #[test]
    fn route_takes_first_preferred_with_config_order_tie_break() {
        // text-native: BOTH pdf kinds are Preferred — whichever sits FIRST
        // in config order wins the tie (the operator's order is the
        // tie-break, exactly like the linear chain it degenerates to)
        let insp_first = pdf_members("insp", "plain");
        assert_eq!(
            route("pdf", &insp_first, Some(ContentClass::TextNative)),
            Routing::Run {
                first: 0,
                class: ContentClass::TextNative
            }
        );
        // the REVERSED config (plain first) also routes to ITS first
        // member — the choice follows config order, not kind identity
        // (this is what proves the tie rule, not a lucky fixed order)
        let plain_first = vec![
            ("plain".to_string(), kind_of("t", "kind = \"pdf-text\"")),
            ("insp".to_string(), kind_of("t", "kind = \"pdf-inspector\"")),
        ];
        assert_eq!(
            route("pdf", &plain_first, Some(ContentClass::TextNative)),
            Routing::Run {
                first: 0,
                class: ContentClass::TextNative
            }
        );
    }

    #[test]
    fn route_falls_back_to_first_capable_when_no_preferred() {
        // mixed-text-scan: insp refuses, plain is Capable ⇒ plain is
        // selected even though it sits SECOND in config order — the
        // mis-order quality protection this step exists for
        let members = pdf_members("insp", "plain");
        assert_eq!(
            route("pdf", &members, Some(ContentClass::MixedTextScan)),
            Routing::Run {
                first: 1,
                class: ContentClass::MixedTextScan
            }
        );
        // scan-dominant: everyone refuses
        assert!(matches!(
            route("pdf", &members, Some(ContentClass::ScanDominant)),
            Routing::Refused(_)
        ));
    }

    #[test]
    fn route_unknown_class_is_the_full_config_order() {
        // Unknown (no probe verdict): pure M2.1 semantics, every member in
        // config order
        let members = pdf_members("insp", "plain");
        assert_eq!(route("pdf", &members, None), Routing::Order(vec![0, 1]));
    }

    #[test]
    fn route_all_refuse_message_names_class_members_and_remedy() {
        let members = pdf_members("insp", "plain");
        let Routing::Refused(reason) = route("pdf", &members, Some(ContentClass::ScanDominant))
        else {
            panic!("scan-dominant must be refused by both members");
        };
        assert!(reason.starts_with("pdf class=scan-dominant:"), "{reason}");
        assert!(
            reason.contains("insp refuses (its whole-document"),
            "{reason}"
        );
        assert!(
            reason.contains("plain refuses (a scan has no text layer"),
            "{reason}"
        );
        // the remedy is phrased by the CLASS: scan classes name the missing
        // OCR capability
        assert!(reason.contains("add an OCR-capable extractor"), "{reason}");
    }

    #[test]
    fn route_all_refuse_on_no_text_names_the_blank_document_remedy() {
        // no-text is a DIFFERENT dead end than a scan: blank pages have
        // nothing to extract AND nothing to OCR — pointing the operator at
        // OCR (the pre-hardening hardwired remedy) would be wrong advice
        let members = pdf_members("insp", "plain");
        let Routing::Refused(reason) = route("pdf", &members, Some(ContentClass::NoText)) else {
            panic!("no-text must be refused by both members");
        };
        assert!(reason.starts_with("pdf class=no-text:"), "{reason}");
        assert!(
            reason.contains("no text layer and no raster content"),
            "{reason}"
        );
        assert!(
            !reason.contains("OCR-capable"),
            "the blank-document remedy must not pitch OCR: {reason}"
        );
    }

    #[test]
    fn chain_try_order_routes_first_then_affinity_tiers_the_tail() {
        // Run{first, class}: the routed member first, the REST by affinity
        // tier (Preferred > Capable > Refuses), config order STABLE within
        // a tier — the tail is still the cross-parser backstop (a
        // Refuses-class member still runs, just last: extract-time behavior
        // remains the last word)
        //
        // [insp, plain] under mixed-text-scan: plain (Capable) is routed;
        // insp (Refuses) is the tiered tail
        let members = pdf_members("insp", "plain");
        assert_eq!(
            chain_try_order(
                &members,
                &Routing::Run {
                    first: 1,
                    class: ContentClass::MixedTextScan
                }
            ),
            vec![1, 0]
        );
        // under text-native (plain routed at 1, insp ALSO Preferred): both
        // tail-less tier-0s keep config order — [0] stays after [1] only by
        // config position, which is the stable-sort guarantee
        assert_eq!(
            chain_try_order(
                &members,
                &Routing::Run {
                    first: 1,
                    class: ContentClass::TextNative
                }
            ),
            vec![1, 0]
        );
        // a THREE-member chain shows the tiering: [insp, md, plain] under
        // mixed-text-scan routes plain (the only Capable); insp and md both
        // Refuse — the tail keeps config order WITHIN the single tier
        let mixed = vec![
            ("insp".to_string(), kind_of("t", "kind = \"pdf-inspector\"")),
            ("md".to_string(), kind_of("t", "kind = \"md\"")),
            ("plain".to_string(), kind_of("t", "kind = \"pdf-text\"")),
        ];
        assert_eq!(
            chain_try_order(
                &mixed,
                &Routing::Run {
                    first: 2,
                    class: ContentClass::MixedTextScan
                }
            ),
            vec![2, 0, 1]
        );
        // CROSS-tier tail: [ctn (Capable — the cfg(test) container declares
        // no affinity, so the default is Capable everywhere), md (Refuses),
        // plain (Capable)] routes ctn (first Capable); the tail's plain
        // (Capable, tier 1) outranks md (Refuses, tier 2) DESPITE md's
        // earlier config position — the burn-a-guaranteed-failure-later
        // rule this revision exists for
        let ctn = crate::pipeline::extract::ExtractorKind::TestContainer(
            crate::pipeline::extract::TestContainerSpec::one(
                "application/x-test",
                b"payload",
                vec![],
            ),
        );
        let cross_tier = vec![
            ("ctn".to_string(), ctn),
            ("md".to_string(), kind_of("t", "kind = \"md\"")),
            ("plain".to_string(), kind_of("t", "kind = \"pdf-text\"")),
        ];
        assert_eq!(
            chain_try_order(
                &cross_tier,
                &Routing::Run {
                    first: 0,
                    class: ContentClass::MixedTextScan
                }
            ),
            vec![0, 2, 1]
        );
        // Order: verbatim (Unknown class = pure config order, byte-stable)
        assert_eq!(
            chain_try_order(&members, &Routing::Order(vec![0, 1])),
            vec![0, 1]
        );
        // Refused: nobody executes
        assert_eq!(
            chain_try_order(&members, &Routing::Refused("x".into())),
            Vec::<usize>::new()
        );
    }

    /// The `auto` mapping's view of the registry (plugin-axis hardening):
    /// every family the mime table declares must resolve under `auto` — a
    /// table row without a [`builtin_extractor`] arm would silently strand
    /// every `extract = { default = "auto" }` deployment for that family.
    /// EXEMPTION (M3-T4): `img` is the one deliberate exception — pulling
    /// image files into every existing `auto` deployment would change their
    /// modality needs overnight (pdf-pages stayed out of `auto` for the
    /// same reason); operators opt in with `extract = { img = "…" }`.
    /// (The cfg(test) `ctn` scaffold key is also exempt: it is scanner-side
    /// claiming only, never an `auto` family.)
    #[test]
    fn auto_mapping_serves_every_mime_family() {
        for (_mime, key) in crate::pipeline::extract::MIME_TO_SHORT_KEY {
            if *key == "img" {
                assert!(
                    builtin_extractor(key).is_none(),
                    "img must stay OUT of auto (the pdf-pages ruling): an auto arm would \
                     silently re-route every default = \"auto\" deployment into image ingesting"
                );
                continue;
            }
            assert!(
                builtin_extractor(key).is_some(),
                "auto has no built-in extractor for family {key} — every MIME_TO_SHORT_KEY \
                 row needs a builtin_extractor arm"
            );
        }
    }

    /// Backward compatibility of the affinity DEFAULT (the mechanism that
    /// keeps every pre-declaration implementation — contract kinds, future
    /// third-party kinds — chain-compatible): an implementation that does
    /// not override [`Extractor::affinity`] is Capable for every class, so
    /// routing degenerates to pure config order: no Preferred exists (pass
    /// 1 finds nothing), the first Capable is member 0, and Run(0)'s try
    /// order IS the config order (pinned in chain_try_order above). The
    /// default itself is pinned in pipeline::tests.
    #[test]
    fn undeclared_affinity_degenerates_routing_to_pure_config_order() {
        struct Undeclared;
        impl Extractor for Undeclared {
            fn extract(&self, _: &[u8], _: &str) -> Result<Vec<Part>, PipelineError> {
                unreachable!("affinity-only test double")
            }
            // NO affinity override: the trait default is the test subject
        }
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Undeclared.affinity(class), Fit::Capable, "{class}");
        }
        // ...and all-Capable ⇒ first-Capable = index 0 ⇒ [0, 1, …]: the
        // member sequence equals the config order (Run{first: 0} over
        // all-Capable members: a single-tier tail keeps config order, so
        // tiering never perturbs the degenerate case)
        let all_capable = vec![
            (
                "a".to_string(),
                crate::pipeline::extract::ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/x-test",
                    b"p",
                    vec![],
                )),
            ),
            (
                "b".to_string(),
                crate::pipeline::extract::ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/x-test",
                    b"q",
                    vec![],
                )),
            ),
        ];
        assert_eq!(
            chain_try_order(
                &all_capable,
                &Routing::Run {
                    first: 0,
                    class: ContentClass::ScanDominant
                }
            ),
            vec![0, 1]
        );
    }

    // ---- M2.1 step 3: normalize stage + container unpacking ---------------

    use crate::pipeline::ChildDoc;
    use crate::pipeline::extract::TestContainerSpec;

    /// Real gzip bytes (the same library the GzNormalizer decodes with).
    fn gz(payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap()
    }

    /// Every stored chunk text of one index (the x-vf-chunk-text metadata
    /// the default config keeps) — the assertion surface for "which KIND
    /// actually produced this content".
    async fn chunk_texts(state: &AppState, vb: &str, index: &str) -> Vec<String> {
        state
            .storage
            .scan_vectors(vb, index, None)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|v| {
                v.metadata
                    .as_ref()
                    .and_then(|m| m.get("x-vf-chunk-text"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .collect()
    }

    /// runtime_for + a PRE-Arc core patch: the container tests inject the
    /// cfg(test) TestContainer kind into the loaded plan here (production
    /// configs can never name it — from_config has no arm; the TOML declares
    /// placeholder instances so the extract table validates, and the patch
    /// overwrites their KINDS with scripted containers).
    async fn runtime_patched(
        dir: &Path,
        cfg: &PipelineConfig,
        patch: impl FnOnce(&mut BucketPlan),
    ) -> (Arc<IngestRuntime>, Arc<IngestHandle>, Arc<AppState>) {
        let mut core = IngestCore::load(cfg, Path::new("/nonexistent-models"))
            .await
            .expect("fixture embedder loads");
        patch(core.buckets.get_mut("docs").expect("docs bucket"));
        let state = test_state(dir);
        hold_locks(&state, core.buckets.keys().cloned()).await;
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let handle = Arc::new(IngestHandle::new(IngestShared::new(core)));
        let rt = IngestRuntime::new(
            Arc::clone(handle.shared()),
            state.clone(),
            Arc::new(Notify::new()),
            rx,
        );
        (rt, handle, state)
    }

    /// The container e2e fixture: tiny embedder + a parameterized extract
    /// table (plus any extra ingest keys, e.g. `max_file_size_mb`/`retries`).
    /// Declares `plain` (txt), `m` (md) and `ctn`/`inner` placeholders so
    /// routes like `ctn = "ctn"` / `default = "inner"` validate.
    fn ctn_config(extract_entries: &str, extra: &str) -> PipelineConfig {
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [extractor.m]\nkind = \"md\"\n\n\
             [extractor.ctn]\nkind = \"txt\"\n\n\
             [extractor.inner]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ {extract_entries} }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1\n{extra}",
            fixture_dir().display().to_string(),
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// THE normalize mechanism e2e (M2.1 step 3): a `.txt.gz` document is
    /// claimed as the text/plain source it decompresses into (the scan-time
    /// guess sees through the wrapper), the worker's normalize stage gunzips
    /// it BEFORE routing, the user's plain txt chain extracts the
    /// DECOMPRESSED text, and the file INDEXES — the real GzNormalizer over
    /// the real full chain, not a paper mechanism. Run with --nocapture to
    /// see `normalize: docs/a.txt.gz -> gunzip (text/plain)` and the INDEXED
    /// line.
    #[tokio::test]
    async fn gz_document_full_chain_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = runtime_for(dir.path(), &tiny_config("").1).await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        let body = "the quick brown fox jumps over the lazy dog again and again";
        std::fs::write(docs.join("a.txt.gz"), gz(body.as_bytes())).unwrap();

        rt.scan_once().await;
        rt.drain_queue().await;

        // claimed and indexed as ONE text/plain source
        let ms = markers(&state, "docs").await;
        assert_eq!(ms.len(), 1, ".txt.gz is a source: {ms:?}");
        let m = status_of(&state, "docs", "docs/a.txt.gz").await;
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
        let chunks = m.chunks.as_ref().unwrap()["text"];
        assert!(chunks >= 2, "69-char body over a 32 window: {m:?}");
        // the vectors are the DECOMPRESSED text's (what the txt chain saw),
        // and the content hash stays the hash of the RAW stored bytes
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        let texts = chunk_texts(&state, "docs", &spec.index_name).await;
        assert_eq!(texts.len() as u32, chunks);
        assert!(
            texts.iter().any(|t| t.contains("quick brown fox")),
            "decompressed content indexed: {texts:?}"
        );
        let raw = std::fs::read(docs.join("a.txt.gz")).unwrap();
        assert_eq!(m.content_hash, marker::content_hash_of(&raw));
        // a misnamed plain file under .gz degrades gracefully: no magic ⇒ no
        // transform ⇒ the plain route still reads it
        std::fs::write(docs.join("b.txt.gz"), b"plain text in a misnamed wrapper").unwrap();
        rt.scan_once().await;
        rt.drain_queue().await;
        assert_eq!(
            status_of(&state, "docs", "docs/b.txt.gz").await.status,
            SourceStatus::Indexed
        );
    }

    /// A corrupt gzip is a HARD normalize failure (no fallback member exists
    /// — module docs): the document parks with the reason naming the hint,
    /// no retry budget is burned, and a later scan leaves it untouched.
    #[tokio::test]
    async fn corrupt_gz_document_parks_with_hint_name_reason() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) = runtime_for(dir.path(), &tiny_config("retries = 3\n").1).await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        let mut bad = vec![0x1f, 0x8b, 0x08, 0x00];
        bad.extend_from_slice(b"definitely not a deflate stream");
        std::fs::write(docs.join("bad.txt.gz"), &bad).unwrap();

        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/bad.txt.gz").await;
        assert_eq!(m.status, SourceStatus::Failed);
        assert!(
            m.retries_exhausted,
            "deterministic park, not retry-then-fail"
        );
        let reason = m.status_reason.expect("reason");
        assert!(reason.contains("normalize:"), "{reason}");
        assert!(reason.contains("gunzip"), "{reason}");
        assert!(reason.contains("bad.txt.gz"), "the hint name: {reason}");
        // nothing was indexed
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        assert!(
            state
                .storage
                .load_index("docs", &spec.index_name)
                .await
                .unwrap()
                .is_none()
        );
        // parked: the rescan does not re-enqueue it
        rt.scan_once().await;
        rt.drain_queue().await;
        let m2 = status_of(&state, "docs", "docs/bad.txt.gz").await;
        assert_eq!(m2.updated_at, m.updated_at, "untouched by the rescan");
    }

    /// The amplification gate: `max_file_size_mb` applies to the
    /// POST-normalize bytes too — a tiny gz that expands past the cap parks
    /// (deterministically) instead of handing the pipeline 100x the guard.
    #[tokio::test]
    async fn gz_expansion_over_size_cap_parks() {
        let dir = tempfile::tempdir().unwrap();
        let (rt, _h, state) =
            runtime_for(dir.path(), &tiny_config("max_file_size_mb = 1\n").1).await;
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        // ~1.5 MiB of highly compressible text: raw gz stays far under the
        // 1 MiB raw gate, the expansion blows past it
        let body = "word ".repeat(320 * 1024);
        std::fs::write(docs.join("bomb.txt.gz"), gz(body.as_bytes())).unwrap();
        assert!(
            std::fs::metadata(docs.join("bomb.txt.gz")).unwrap().len() < 1024 * 1024,
            "the raw file passes the ordinary size gate"
        );

        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/bomb.txt.gz").await;
        assert_eq!(m.status, SourceStatus::Failed);
        let reason = m.status_reason.expect("reason");
        assert!(
            reason.contains("normalize expanded the content to"),
            "{reason}"
        );
        assert!(reason.contains("max_file_size_mb"), "{reason}");
    }

    /// THE configuration-coherence proof (the step's core selling point): a
    /// container's children re-enter the USER-CONFIGURED routing — the md
    /// child lands on the configured `m` (md kind) instance, the txt child
    /// on `plain`, and the aggregated chunks are EXACTLY what those two
    /// configured instances produce over the children in order. A container
    /// that digested its own children (the unstructured anti-shape) would
    /// produce different bytes: the md kind strips the heading marker.
    #[tokio::test]
    async fn container_children_reenter_user_configured_chains_e2e() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        let payload = b"CONTAINER-PAYLOAD";
        std::fs::write(docs.join("book.ctn"), payload).unwrap();

        let cfg = ctn_config("txt = \"plain\", md = \"m\", ctn = \"ctn\"", "");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    payload,
                    vec![
                        ChildDoc::new(
                            "ch1/chapter.txt",
                            "text/plain",
                            "first chapter body, plain words",
                        ),
                        ChildDoc::new(
                            "ch1/chapter.md",
                            "text/markdown",
                            "# Heading Two\n\nsecond chapter prose",
                        ),
                    ],
                )),
            );
        })
        .await;

        rt.scan_once().await;
        rt.drain_queue().await;

        let m = status_of(&state, "docs", "docs/book.ctn").await;
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");

        // expected = exactly what the USER'S configured instances produce:
        // plain(txt) over child 1 ++ m(md) over child 2, in child order,
        // through the configured chunker
        let plan = &rt.shared.core.buckets["docs"];
        let txt = kind_of("t", "kind = \"txt\"");
        let md = kind_of("t", "kind = \"md\"");
        let mut expected_parts =
            Extractor::extract(&txt, b"first chapter body, plain words", "text/plain").unwrap();
        expected_parts.extend(
            Extractor::extract(
                &md,
                b"# Heading Two\n\nsecond chapter prose",
                "text/markdown",
            )
            .unwrap(),
        );
        let expected: Vec<String> = Chunker::chunk(&plan.chunker, &expected_parts)
            .unwrap()
            .into_iter()
            .map(|c| c.text)
            .collect();
        let got = chunk_texts(&state, "docs", &plan.derived["tiny"].index_name).await;
        assert_eq!(
            got.len() as u32,
            m.chunks.as_ref().unwrap()["text"],
            "marker count matches stored vectors"
        );
        assert_eq!(
            got, expected,
            "children were routed by THEIR mimes through the configured instances"
        );
        // the md discrimination made explicit: a self-digesting (txt-only)
        // container would have kept the heading marker
        assert!(
            !got.iter().any(|t| t.contains('#')),
            "the md kind processed the md child: {got:?}"
        );
    }

    /// Child-level normalize: a container child that is ITSELF gzipped
    /// decompresses BEFORE its chain lookup — the derived text/plain mime
    /// routes it to the user's txt chain. Without the child normalize this
    /// child has no chain for `application/gzip` and would be skipped.
    #[tokio::test]
    async fn container_child_normalize_runs_before_child_routing() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        let payload = b"GZ-CHILD-CONTAINER";
        std::fs::write(docs.join("book.ctn"), payload).unwrap();

        let cfg = ctn_config("txt = \"plain\", ctn = \"ctn\"", "");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    payload,
                    vec![ChildDoc::new(
                        "logs/app.txt.gz",
                        "application/gzip",
                        gz(
                            b"gz child body content, decompressed by the child normalize"
                                .as_slice(),
                        ),
                    )],
                )),
            );
        })
        .await;

        rt.scan_once().await;
        rt.drain_queue().await;
        assert_eq!(
            status_of(&state, "docs", "docs/book.ctn").await.status,
            SourceStatus::Indexed
        );
        let plan = &rt.shared.core.buckets["docs"];
        let texts = chunk_texts(&state, "docs", &plan.derived["tiny"].index_name).await;
        assert!(
            texts.iter().any(|t| t.contains("gz child body content")),
            "{texts:?}"
        );
    }

    // ---- M3-T1: image modality through the REAL worker ----

    /// A valid 1x1 PNG (67 bytes) — the e2e image source. Only its magic is
    /// ever sniffed in T1 (no decoder exists yet; the sniff guard and the
    /// modality gate are the mechanism under test).
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// The image e2e fixture: tiny (text-only) embedder + an `img` placeholder
    /// extractor instance routed under the cfg(test) `img` short key (the
    /// runtime patch swaps the placeholder for the scripted image kind).
    fn img_config() -> PipelineConfig {
        let text = format!(
            "[extractor.img]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = \
             true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ img = \"img\" }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\"]\nscan_interval_secs = 1",
            fixture_dir().display().to_string(),
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// THE M3-T1 mechanism e2e: a PNG source is claimed, extracted as an
    /// image Part by the cfg(test) image kind (image-only extractions carry
    /// content and WIN the chain), atomically chunked by the axis bypass
    /// (sniff guard passes a real PNG) — and then REFUSED LOUDLY by the
    /// doc-modality gate: the only listed embedder is text-only, so the
    /// file lands FAILED with the aggregate reason + remedy, no retry
    /// budget burned, no vectors written, no derived index created. Note
    /// the config-time carry-over check CANNOT catch this (the placeholder
    /// kind declares text; the runtime output is image) — exactly why the
    /// runtime gate exists.
    #[tokio::test]
    async fn image_source_with_no_taker_fails_loud_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("photo.img"), TINY_PNG).unwrap();

        let cfg = img_config();
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "img".to_string(),
                crate::pipeline::extract::ExtractorKind::TestImage(
                    crate::pipeline::extract::TestImageSpec {
                        mime: "image/png".to_string(),
                    },
                ),
            );
        })
        .await;

        rt.scan_once().await;
        rt.drain_queue().await;

        // claimed as a source (the cfg(test) img short key) and FAILED —
        // parked, not retried: the reason is the modality gate's aggregate
        let m = status_of(&state, "docs", "docs/photo.img").await;
        assert_eq!(m.status, SourceStatus::Failed, "{m:?}");
        assert!(m.retries_exhausted, "a parked gap is deterministic: {m:?}");
        let reason = m
            .status_reason
            .clone()
            .expect("the gate reason on the marker");
        assert!(reason.contains("1 image chunk(s)"), "{reason}");
        assert!(
            reason.contains("no embedder that accepts them on its doc side"),
            "{reason}"
        );
        assert!(reason.contains("tiny (doc modalities: text)"), "{reason}");
        assert!(
            reason.contains("add an embedder that accepts image documents"),
            "the guidance must ride along: {reason}"
        );
        // loud, not silent: no completion evidence, nothing embedded, no
        // derived index created for the gated file
        assert_eq!(m.chunks, None, "failed sources carry no chunk evidence");
        assert!(m.content_hash.is_empty(), "{m:?}");
        let spec = rt.shared.core.buckets["docs"].derived["tiny"].clone();
        assert!(
            state
                .storage
                .load_index("docs", &spec.index_name)
                .await
                .unwrap()
                .is_none(),
            "no index may be created for a gated source"
        );
    }

    /// The bypass's sniff guard runs inside the REAL worker too: a garbage
    /// ".img" file (no image magic) fails as a CHUNK error — with the
    /// no-magic message, NOT the missing-embedder remedy (which would
    /// misdirect on a corrupt file).
    #[tokio::test]
    async fn garbage_image_source_fails_on_the_sniff_not_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("garbage.img"), b"definitely not an image").unwrap();

        let cfg = img_config();
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "img".to_string(),
                crate::pipeline::extract::ExtractorKind::TestImage(
                    crate::pipeline::extract::TestImageSpec {
                        mime: "image/png".to_string(),
                    },
                ),
            );
        })
        .await;

        rt.scan_once().await;
        rt.drain_queue().await;

        let m = status_of(&state, "docs", "docs/garbage.img").await;
        assert_eq!(m.status, SourceStatus::Failed, "{m:?}");
        let reason = m
            .status_reason
            .clone()
            .expect("chunk refusal on the marker");
        assert!(
            reason.contains("no known image-container magic"),
            "{reason}"
        );
        assert!(
            !reason.contains("no embedder that accepts"),
            "garbage is a chunk refusal, not a missing-embedder gap: {reason}"
        );
    }

    // ---- M3-T2: image sources EMBED through the real siglip2 kind ----

    /// The siglip2 fixture dir (tiny towers + WordLevel tokenizer; the same
    /// hermetic discipline as the ort tiny-embedder fixture).
    fn siglip2_fixture_dir() -> std::path::PathBuf {
        for p in [
            "vf/tests/fixtures/tiny-siglip2",
            "tests/fixtures/tiny-siglip2",
        ] {
            let d = std::path::PathBuf::from(p);
            if d.join("vision.onnx").is_file() {
                return d;
            }
        }
        panic!("tiny-siglip2 fixture not found — run its generate.py");
    }

    /// A GENUINELY decodable small PNG (in-test encoder). T1's TINY_PNG
    /// only ever had its magic SNIFFED — its deflate stream is not valid
    /// for a real decoder, and the siglip2 kind decodes for preprocessing.
    fn valid_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(12, 9, image::Rgb([30, 180, 90]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// The image-EMBEDDING e2e config: an `img` placeholder extractor (the
    /// runtime patch swaps it for the cfg(test) image kind) + the siglip2
    /// fixture embedder via `path` (no network, no models dir). The tiny
    /// text embedder rides along because the config-time modality
    /// carry-over check (correctly) refuses a text-outputting placeholder
    /// extractor with no text-accepting embedder listed — which also makes
    /// every run of this shape prove per-embedder modality SCOPING.
    fn sig_config(extract_entries: &str) -> PipelineConfig {
        let text = format!(
            "[extractor.img]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = \
             true\n\n\
             [embedder.sig]\nkind = \"siglip2\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ {extract_entries} }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\", \"sig\"]\nscan_interval_secs = 1",
            fixture_dir().display().to_string(),
            siglip2_fixture_dir().display().to_string(),
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// The cfg(test) image-kind patch (the T1 twin of every patched test).
    fn patch_img_extractor(plan: &mut BucketPlan) {
        plan.extractors.insert(
            "img".to_string(),
            crate::pipeline::extract::ExtractorKind::TestImage(
                crate::pipeline::extract::TestImageSpec {
                    mime: "image/png".to_string(),
                },
            ),
        );
    }

    /// THE M3-T2 e2e (the T1 gate's green twin): a PNG source is claimed,
    /// extracted as an image Part, atomically chunked — and EMBEDDED by the
    /// real siglip2 kind over the real .so: INDEXED, one vector in the sig
    /// derived index, `x-vf-modality = image`, the `image` chunk-key
    /// namespace, the fixture's 8-dim unit-norm vector on the row — while
    /// the text embedder, scoped to zero chunks, writes NOTHING (no index
    /// of its own is ever created).
    #[tokio::test]
    async fn image_source_embeds_through_siglip2() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("photo.img"), valid_png()).unwrap();

        let cfg = sig_config("img = \"img\"");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, patch_img_extractor).await;

        rt.scan_once().await;
        rt.drain_queue().await;

        let m = status_of(&state, "docs", "docs/photo.img").await;
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
        assert_eq!(
            m.chunks.as_ref().unwrap().get("image"),
            Some(&1u32),
            "one atomic image chunk: {:?}",
            m.chunks
        );

        // the sig derived index holds exactly one IMAGE row with the full
        // metadata contract and a unit-norm 8-dim vector
        let spec = rt.shared.core.buckets["docs"].derived["sig"].clone();
        let rows = state
            .storage
            .scan_vectors("docs", &spec.index_name, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one image atom in the derived index");
        let row = &rows[0];
        assert!(
            row.key.ends_with("-image-0"),
            "image atoms live in the image key namespace: {}",
            row.key
        );
        let meta = row.metadata.as_ref().unwrap();
        assert_eq!(meta["x-vf-modality"], "image");
        assert_eq!(meta["x-vf-src-name"], "docs/photo.img");
        assert_eq!(
            meta["x-vf-model"], "tiny-siglip2",
            "path-only identity is the directory name"
        );
        assert_eq!(row.vector.len(), 8, "fixture dim");
        assert!(row.vector.iter().all(|x| x.is_finite()));
        let norm: f32 = row.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit norm: {norm}");

        // per-embedder modality scoping, the zero-chunk half: the text
        // embedder saw no acceptable chunk, so it wrote nothing and its
        // derived index was never created
        let tiny_spec = rt.shared.core.buckets["docs"].derived["tiny"].clone();
        assert!(
            state
                .storage
                .load_index("docs", &tiny_spec.index_name)
                .await
                .unwrap()
                .is_none(),
            "the text embedder must not materialize an index for an image-only source"
        );
    }

    /// A garbage image payload still fails AT DECODE inside the real kind
    /// (not the gate, not silently): the sniff guard passes a real PNG
    /// header but a TRUNCATED png body must fail loud as an embed error.
    #[tokio::test]
    async fn truncated_png_fails_at_siglip2_decode() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // magic sniff needs the header; the decoder needs the IDAT — cut both
        std::fs::write(docs.join("cut.img"), &valid_png()[..24]).unwrap();

        let cfg = sig_config("img = \"img\"");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, patch_img_extractor).await;

        rt.scan_once().await;
        rt.drain_queue().await;

        // the text embedder SUCCEEDED (its scoped set was empty), so the
        // marker is PARTIALLY indexed with the sig failure as the reason —
        // the loud-embed-failure shape, never a silent skip
        let m = status_of(&state, "docs", "docs/cut.img").await;
        assert_eq!(m.status, SourceStatus::PartiallyIndexed, "{m:?}");
        assert!(
            m.retries_exhausted,
            "decode failure is deterministic: {m:?}"
        );
        let reason = m.status_reason.clone().expect("reason");
        assert!(reason.contains("decoding image"), "{reason}");
    }

    /// Mixed-modality source set over DUAL embedders (the §5.10.1 fan-out
    /// through real kinds): text files embed through the tiny ort kind,
    /// image files through siglip2 — each derived index receives ONLY its
    /// own modality's rows, both sources INDEXED, each row's
    /// `x-vf-modality`/`x-vf-model` naming the embedder that produced it.
    #[tokio::test]
    async fn mixed_sources_fan_out_to_dual_embedders() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        let body = "the quick brown fox jumps over the lazy dog again and again";
        std::fs::write(docs.join("note.txt"), body).unwrap();
        std::fs::write(docs.join("photo.img"), valid_png()).unwrap();

        // text (tiny ort fixture) + image (siglip2 fixture) embedders, both
        // listed; the fan-out scopes chunks by each one's doc modalities
        let text = format!(
            "[extractor.plain]\nkind = \"txt\"\n\n\
             [extractor.img]\nkind = \"txt\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = \
             true\n\n\
             [embedder.sig]\nkind = \"siglip2\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ txt = \"plain\", img = \"img\" \
             }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\", \"sig\"]\nscan_interval_secs = 1",
            fixture_dir().display().to_string(),
            siglip2_fixture_dir().display().to_string(),
        );
        let cfg = crate::pipeline::config::load_pipeline_config(&text).unwrap();
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, patch_img_extractor).await;

        rt.scan_once().await;
        rt.drain_queue().await;

        // both sources INDEXED (the T1 aggregate gate cannot fire: every
        // emitted modality has a taker)
        for key in ["docs/note.txt", "docs/photo.img"] {
            let m = status_of(&state, "docs", key).await;
            assert_eq!(m.status, SourceStatus::Indexed, "{key}: {m:?}");
        }

        // tiny's index: ONLY text rows; sig's: ONLY the image row
        let tiny_spec = rt.shared.core.buckets["docs"].derived["tiny"].clone();
        let sig_spec = rt.shared.core.buckets["docs"].derived["sig"].clone();
        let tiny_rows = state
            .storage
            .scan_vectors("docs", &tiny_spec.index_name, None)
            .await
            .unwrap();
        let sig_rows = state
            .storage
            .scan_vectors("docs", &sig_spec.index_name, None)
            .await
            .unwrap();
        assert!(!tiny_rows.is_empty(), "the text source embedded");
        assert_eq!(sig_rows.len(), 1, "one image atom");
        for row in &tiny_rows {
            assert!(
                !row.key.contains("-image-"),
                "no image atoms in the text embedder's index: {}",
                row.key
            );
            assert_eq!(row.metadata.as_ref().unwrap()["x-vf-modality"], "text");
            assert_eq!(row.metadata.as_ref().unwrap()["x-vf-model"], "tiny-fixture");
        }
        let sig_row = &sig_rows[0];
        assert!(sig_row.key.ends_with("-image-0"), "{}", sig_row.key);
        let meta = sig_row.metadata.as_ref().unwrap();
        assert_eq!(meta["x-vf-modality"], "image");
        assert_eq!(meta["x-vf-src-name"], "docs/photo.img");
        assert_eq!(meta["x-vf-model"], "tiny-siglip2");
        assert_eq!(sig_row.vector.len(), 8);
        // dims differ per index (8-dim fixture text vs 8-dim fixture image —
        // both fixtures are 8; the SEPARATION is the assertion, and each
        // index only ever saw its own embedder's vectors)
        assert_eq!(tiny_rows[0].vector.len(), 8, "text index keeps its own dim");
    }

    // ---- M3-T3: pdf-pages — real PDF pages through the real worker ----

    /// The pdf-pages e2e config fixture: a `pages` (pdf-pages, default dpi)
    /// and a `plain` (pdf-text) instance over BOTH fixture embedders (the
    /// carry-over check needs an image-accepting embedder for `pages` and a
    /// text-accepting one for `plain`), with the `pdf` route parameterized —
    /// `"pages"`, `["plain", "pages"]`, … (the routing-chain fixture shape).
    fn pdf_pages_config(route: &str) -> PipelineConfig {
        let text = format!(
            "[extractor.pages]\nkind = \"pdf-pages\"\n\n\
             [extractor.plain]\nkind = \"pdf-text\"\n\n\
             [embedder.tiny]\nkind = \"ort\"\nmodel = \"tiny-fixture\"\npath = {:?}\noffline = \
             true\n\n\
             [embedder.sig]\nkind = \"siglip2\"\npath = {:?}\noffline = true\n\n\
             [ingest.docs]\ninclude = [\"docs/**\"]\nextract = {{ pdf = {route} }}\n\
             chunk = {{ kind = \"token-window\", size = 32, overlap = 8 }}\n\
             embedders = [\"tiny\", \"sig\"]\nscan_interval_secs = 1",
            fixture_dir().display().to_string(),
            siglip2_fixture_dir().display().to_string(),
        );
        crate::pipeline::config::load_pipeline_config(&text).unwrap()
    }

    /// THE M3-T3 e2e (the image chain closed end to end over a REAL PDF): a
    /// 2-page mixed document renders page-by-page through the real pdfium
    /// .so (the production kind, no test scaffold anywhere in the path),
    /// every page becomes one atomic image chunk, siglip2 embeds them —
    /// INDEXED with image=2, the `-image-N` key namespace numbered BY PAGE
    /// ORDER (…-image-0, …-image-1), x-vf-modality=image on each row, and
    /// the two rows' vectors DISTINCT (page 1 draws text, page 2 a raster —
    /// the tiny fixture maps different pixels to different vectors, pinned
    /// in its own suite). The text embedder rides along scoped to zero
    /// chunks and materializes nothing.
    #[tokio::test]
    async fn pdf_pages_render_indexes_through_siglip2() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("mixed.pdf"),
            crate::pipeline::extract::pdf_inspector::tests::mixed_text_image_pdf(),
        )
        .unwrap();

        let (rt, _h, state) = runtime_for(dir.path(), &pdf_pages_config("\"pages\"")).await;
        rt.scan_once().await;
        rt.drain_queue().await;

        let m = status_of(&state, "docs", "docs/mixed.pdf").await;
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
        assert_eq!(
            m.chunks.as_ref().unwrap().get("image"),
            Some(&2u32),
            "one atomic chunk per page: {:?}",
            m.chunks
        );
        assert_eq!(
            m.chunks.as_ref().unwrap().get("text"),
            Some(&0u32),
            "no text chunks from an image-only extraction: {:?}",
            m.chunks
        );

        // the sig index: exactly two image rows, key namespace numbered in
        // page order, the full metadata contract, distinct page vectors
        let spec = rt.shared.core.buckets["docs"].derived["sig"].clone();
        let rows = state
            .storage
            .scan_vectors("docs", &spec.index_name, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "one vector per rendered page");
        let mut keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        keys.sort_unstable();
        let prefix = keys[0].trim_end_matches("-image-0").to_string();
        assert_eq!(
            keys,
            vec![
                format!("{prefix}-image-0").as_str(),
                format!("{prefix}-image-1").as_str()
            ],
            "keys number the pages in order from 0: {:?}",
            keys
        );
        for row in &rows {
            let meta = row.metadata.as_ref().unwrap();
            assert_eq!(meta["x-vf-modality"], "image");
            assert_eq!(meta["x-vf-src-name"], "docs/mixed.pdf");
            assert_eq!(meta["x-vf-model"], "tiny-siglip2");
            assert_eq!(row.vector.len(), 8, "fixture dim");
            assert!(row.vector.iter().all(|x| x.is_finite()));
        }
        assert_ne!(
            rows[0].vector, rows[1].vector,
            "the two pages embed to different vectors (page order carries content)"
        );

        // the text embedder saw no acceptable chunk: no index of its own
        let tiny_spec = rt.shared.core.buckets["docs"].derived["tiny"].clone();
        assert!(
            state
                .storage
                .load_index("docs", &tiny_spec.index_name)
                .await
                .unwrap()
                .is_none(),
            "a text embedder must not materialize an index for an image-only source"
        );
    }

    /// The mixed-chain routing behavior (probe → affinity → route over
    /// real PDFs, the three-stage mechanism with the image kind in the
    /// chain): a text-native PDF routes to the TEXT member even when
    /// pdf-pages sits FIRST in config (Preferred beats config order — the
    /// mis-order-is-a-quality-accident protection), while an image-only
    /// (scan-dominant) PDF under the SAME chain routes to pdf-pages
    /// (Preferred there; pdf-text refuses the class). Both files INDEX,
    /// each with its own modality's chunks.
    #[tokio::test]
    async fn pdf_pages_chain_routes_by_content_class() {
        // (a) text-native hello.pdf under the MIS-ORDERED chain
        // [pages, plain]: plain is Preferred for text-native, pages only
        // Capable — the router must run plain FIRST regardless of config
        // order, and the file indexes as TEXT
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("hello.pdf"), HELLO_PDF).unwrap();
        let (rt, _h, state) =
            runtime_for(dir.path(), &pdf_pages_config("[\"pages\", \"plain\"]")).await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/hello.pdf").await;
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
        let chunks = m.chunks.as_ref().unwrap();
        assert!(
            chunks.get("text").is_some_and(|n| *n >= 1),
            "text-native routes to the text member: {chunks:?}"
        );
        assert_eq!(chunks.get("image"), None, "no image chunks: {chunks:?}");
        // ...and only the text index has rows (the pdfium page renderer
        // never executed)
        let tiny_spec = rt.shared.core.buckets["docs"].derived["tiny"].clone();
        let sig_spec = rt.shared.core.buckets["docs"].derived["sig"].clone();
        assert!(
            state
                .storage
                .load_index("docs", &sig_spec.index_name)
                .await
                .unwrap()
                .is_none(),
            "pdf-pages must not have run for a text-native PDF"
        );
        assert_eq!(
            state
                .storage
                .count_vectors("docs", &tiny_spec.index_name)
                .await
                .unwrap() as u32,
            chunks["text"]
        );

        // (b) image-only (scan-dominant) PDF under the SAME chain: pdf-pages
        // is Preferred, pdf-text Refuses — the router runs pages first and
        // it WINS (pdfium has no text layer to read; rendering is the only
        // honest product)
        let dir2 = tempfile::tempdir().unwrap();
        let docs2 = dir2.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs2).unwrap();
        std::fs::write(
            docs2.join("scan.pdf"),
            crate::pipeline::extract::pdf_inspector::tests::image_only_pdf(),
        )
        .unwrap();
        let (rt2, _h2, state2) =
            runtime_for(dir2.path(), &pdf_pages_config("[\"pages\", \"plain\"]")).await;
        rt2.scan_once().await;
        rt2.drain_queue().await;
        let m2 = status_of(&state2, "docs", "docs/scan.pdf").await;
        assert_eq!(m2.status, SourceStatus::Indexed, "{m2:?}");
        assert_eq!(
            m2.chunks.as_ref().unwrap().get("image"),
            Some(&1u32),
            "the scan rendered: {:?}",
            m2.chunks
        );
        let sig2 = rt2.shared.core.buckets["docs"].derived["sig"].clone();
        assert_eq!(
            state2
                .storage
                .count_vectors("docs", &sig2.index_name)
                .await
                .unwrap(),
            1,
            "one vector for the one rendered page"
        );

        // (c) the REVERSED chain [plain, pages] over the same scan: the
        // router's verdict must NOT follow config order here either (plain
        // Refuses scan-dominant) — pages still runs first, same INDEXED
        // image outcome (config order is only the WITHIN-TIER tie-break)
        let dir3 = tempfile::tempdir().unwrap();
        let docs3 = dir3.path().join("docs").join("docs");
        std::fs::create_dir_all(&docs3).unwrap();
        std::fs::write(
            docs3.join("scan.pdf"),
            crate::pipeline::extract::pdf_inspector::tests::image_only_pdf(),
        )
        .unwrap();
        let (rt3, _h3, state3) =
            runtime_for(dir3.path(), &pdf_pages_config("[\"plain\", \"pages\"]")).await;
        rt3.scan_once().await;
        rt3.drain_queue().await;
        let m3 = status_of(&state3, "docs", "docs/scan.pdf").await;
        assert_eq!(m3.status, SourceStatus::Indexed, "{m3:?}");
        assert_eq!(
            m3.chunks.as_ref().unwrap().get("image"),
            Some(&1u32),
            "reversed config order must not change the scan's route: {:?}",
            m3.chunks
        );
    }

    /// The child three-state vocabulary on the failure side: a child with NO
    /// chain for its mime is skipped with a reason; a child whose (single)
    /// chain exhausts contributes its verbatim reason; when NO child
    /// produces content the container yields and the document FAILED
    /// carries both reasons in the aggregate.
    #[tokio::test]
    async fn container_children_all_failing_aggregate_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        let payload = b"EMPTY-CONTAINER";
        std::fs::write(docs.join("book.ctn"), payload).unwrap();

        let cfg = ctn_config("txt = \"plain\", ctn = \"ctn\"", "retries = 0\n");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    payload,
                    vec![
                        // off-table mime, no default configured: 无承接
                        ChildDoc::new("pic/logo.png", "image/png", b"\x89PNG-ish".as_slice()),
                        // invalid UTF-8 under a single-member txt chain: 耗尽
                        ChildDoc::new(
                            "bad/note.txt",
                            "text/plain",
                            b"\xff\xfe not utf8".as_slice(),
                        ),
                    ],
                )),
            );
        })
        .await;

        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/book.ctn").await;
        assert_eq!(m.status, SourceStatus::Failed);
        let reason = m.status_reason.expect("reason");
        assert!(reason.contains("none of its 2 children"), "{reason}");
        assert!(
            reason.contains("no extractor chain for mime 'image/png'"),
            "{reason}"
        );
        assert!(reason.contains("not valid UTF-8"), "{reason}");
        // nothing was indexed
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        assert!(
            state
                .storage
                .load_index("docs", &spec.index_name)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Depth guard, both sides of the line: a container INSIDE a container
    /// (two layers) walks its grandchildren to leaves and INDEXES; a THIRD
    /// container layer Errs outright — the depth reason rides the aggregate
    /// up to the document's FAILED.
    #[tokio::test]
    async fn container_nested_two_levels_ok_third_refused() {
        // ---- two levels: doc.ctn -> ctn -> default(inner) -> leaf.txt ----
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("book.ctn"), b"OUTER".as_slice()).unwrap();
        let cfg = ctn_config("ctn = \"ctn\", txt = \"plain\", default = \"inner\"", "");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    b"OUTER",
                    vec![ChildDoc::new(
                        "mid",
                        "application/octet-stream",
                        b"MID".as_slice(),
                    )],
                )),
            );
            plan.extractors.insert(
                "inner".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    b"MID",
                    vec![ChildDoc::new(
                        "leaf.txt",
                        "text/plain",
                        b"deep leaf text body".as_slice(),
                    )],
                )),
            );
        })
        .await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/book.ctn").await;
        assert_eq!(m.status, SourceStatus::Indexed, "two layers are allowed");
        let plan = &rt.shared.core.buckets["docs"];
        let texts = chunk_texts(&state, "docs", &plan.derived["tiny"].index_name).await;
        assert!(
            texts.iter().any(|t| t.contains("deep leaf text body")),
            "{texts:?}"
        );

        // ---- three layers: the third container Errs before its children --
        let dir3 = tempfile::tempdir().unwrap();
        let docs3 = dir3.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs3).unwrap();
        std::fs::write(docs3.join("book.ctn"), b"OUTER".as_slice()).unwrap();
        let cfg3 = ctn_config(
            "ctn = \"ctn\", txt = \"plain\", default = \"inner\"",
            "retries = 0\n",
        );
        let (rt3, _h3, state3) = runtime_patched(dir3.path(), &cfg3, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    b"OUTER",
                    vec![ChildDoc::new(
                        "mid",
                        "application/octet-stream",
                        b"MID".as_slice(),
                    )],
                )),
            );
            // one instance, two script entries: MID -> [DEEP], DEEP -> [leaf]
            plan.extractors.insert(
                "inner".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::scripted(
                    "application/octet-stream",
                    vec![
                        (
                            b"MID".to_vec(),
                            vec![ChildDoc::new(
                                "deep",
                                "application/octet-stream",
                                b"DEEP".as_slice(),
                            )],
                        ),
                        (
                            b"DEEP".to_vec(),
                            vec![ChildDoc::new(
                                "leaf.txt",
                                "text/plain",
                                b"never reached".as_slice(),
                            )],
                        ),
                    ],
                )),
            );
        })
        .await;
        rt3.scan_once().await;
        rt3.drain_queue().await;
        let m3 = status_of(&state3, "docs", "docs/book.ctn").await;
        assert_eq!(m3.status, SourceStatus::Failed);
        let reason = m3.status_reason.expect("reason");
        assert!(
            reason.contains("nesting depth 3"),
            "the third layer refusal names its depth: {reason}"
        );
        assert!(reason.contains("MAX_CONTAINER_DEPTH = 2"), "{reason}");
    }

    /// Child-count guard: 257 children Err the container BEFORE any child
    /// work — the reason names the entry count and the constant.
    #[tokio::test]
    async fn container_children_limit_errs() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("many.ctn"), b"MANY".as_slice()).unwrap();
        let children: Vec<ChildDoc> = (0..=256)
            .map(|i| {
                ChildDoc::new(
                    format!("e{i:03}.txt"),
                    "text/plain",
                    format!("entry number {i}").into_bytes(),
                )
            })
            .collect();
        let cfg = ctn_config("txt = \"plain\", ctn = \"ctn\"", "retries = 0\n");
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    b"MANY",
                    children,
                )),
            );
        })
        .await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/many.ctn").await;
        assert_eq!(m.status, SourceStatus::Failed);
        let reason = m.status_reason.expect("reason");
        assert!(
            reason.contains("unpacked 257 children (MAX_CHILDREN = 256)"),
            "{reason}"
        );
        // the guard fires BEFORE any child work: not one vector landed
        let spec = &rt.shared.core.buckets["docs"].derived["tiny"];
        assert!(
            state
                .storage
                .load_index("docs", &spec.index_name)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Per-child size guard, raw and expanded: an oversize child (or a gz
    /// child that EXPANDS past the cap) is skipped with a reason while its
    /// in-limit sibling still indexes; a container of ONLY oversize
    /// children yields with the skip reason on record.
    #[tokio::test]
    async fn container_oversize_child_skipped_but_sibling_indexes() {
        let mk = |payload: &[u8], children| {
            let cfg = ctn_config(
                "txt = \"plain\", ctn = \"ctn\"",
                "max_file_size_mb = 1\nretries = 0\n",
            );
            (payload.to_vec(), cfg, children)
        };
        let big: Vec<u8> = vec![b'x'; 2 * 1024 * 1024];
        let gz_big = gz(&vec![b'y'; 2 * 1024 * 1024]);

        // (a) raw-oversize + ok sibling -> INDEXED, only the sibling's text
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("book.ctn"), b"PAY-A".as_slice()).unwrap();
        let (payload, cfg, _) = mk(
            b"PAY-A",
            vec![
                ChildDoc::new("huge.bin", "text/plain", big.clone()),
                ChildDoc::new("ok.txt", "text/plain", "the in-limit sibling body"),
            ],
        );
        let (rt, _h, state) = runtime_patched(dir.path(), &cfg, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    &payload,
                    vec![
                        ChildDoc::new("huge.bin", "text/plain", big.clone()),
                        ChildDoc::new("ok.txt", "text/plain", "the in-limit sibling body"),
                    ],
                )),
            );
        })
        .await;
        rt.scan_once().await;
        rt.drain_queue().await;
        let m = status_of(&state, "docs", "docs/book.ctn").await;
        assert_eq!(m.status, SourceStatus::Indexed, "{m:?}");
        let plan = &rt.shared.core.buckets["docs"];
        let texts = chunk_texts(&state, "docs", &plan.derived["tiny"].index_name).await;
        assert!(
            texts.iter().any(|t| t.contains("in-limit sibling")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("xxxx")),
            "the oversize child contributed nothing: {texts:?}"
        );

        // (b) a gz child expanding past the cap: skipped at the EXPANDED
        // size, sibling still indexes (decompression may not smuggle past
        // the file-size guard)
        let dir_b = tempfile::tempdir().unwrap();
        let docs_b = dir_b.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs_b).unwrap();
        std::fs::write(docs_b.join("book.ctn"), b"PAY-B".as_slice()).unwrap();
        let (payload_b, cfg_b, _) = mk(b"PAY-B", vec![]);
        let (rt_b, _h_b, state_b) = runtime_patched(dir_b.path(), &cfg_b, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    &payload_b,
                    vec![
                        ChildDoc::new("bomb.txt.gz", "application/gzip", gz_big.clone()),
                        ChildDoc::new("ok.txt", "text/plain", "sibling beside the bomb"),
                    ],
                )),
            );
        })
        .await;
        rt_b.scan_once().await;
        rt_b.drain_queue().await;
        let m_b = status_of(&state_b, "docs", "docs/book.ctn").await;
        assert_eq!(m_b.status, SourceStatus::Indexed, "{m_b:?}");
        let plan_b = &rt_b.shared.core.buckets["docs"];
        let texts_b = chunk_texts(&state_b, "docs", &plan_b.derived["tiny"].index_name).await;
        assert!(
            !texts_b.iter().any(|t| t.contains("yyyy")),
            "the expanded-past-cap child contributed nothing: {texts_b:?}"
        );

        // (c) ONLY the oversize child: the container yields, the reason
        // names the skip
        let dir_c = tempfile::tempdir().unwrap();
        let docs_c = dir_c.path().join("docs").join("docs"); // v3: inside the source bucket dir (keys keep the docs/… spelling)
        std::fs::create_dir_all(&docs_c).unwrap();
        std::fs::write(docs_c.join("book.ctn"), b"PAY-C".as_slice()).unwrap();
        let (payload_c, cfg_c, _) = mk(b"PAY-C", vec![]);
        let (rt_c, _h_c, state_c) = runtime_patched(dir_c.path(), &cfg_c, |plan| {
            plan.extractors.insert(
                "ctn".to_string(),
                ExtractorKind::TestContainer(TestContainerSpec::one(
                    "application/octet-stream",
                    &payload_c,
                    vec![ChildDoc::new("huge.bin", "text/plain", big.clone())],
                )),
            );
        })
        .await;
        rt_c.scan_once().await;
        rt_c.drain_queue().await;
        let m_c = status_of(&state_c, "docs", "docs/book.ctn").await;
        assert_eq!(m_c.status, SourceStatus::Failed);
        let reason = m_c.status_reason.expect("reason");
        assert!(
            reason.contains("'huge.bin': 2097152 bytes exceeds max_file_size_mb"),
            "{reason}"
        );
    }
}
