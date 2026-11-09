//! Source-marker SEMANTICS (design.md §5.10.1 / §5.10.5): the no-queue
//! migration rules, key hashing, cheap change signatures and scope matching.
//!
//! The PERSISTED data model lives in [`crate::store::SourceMarker`] (a catalog
//! object like BucketMeta/IndexMeta); this module owns the pure decision
//! logic the scanner and job runner consult. Everything here is
//! side-effect-free so every migration edge is unit-testable without I/O.

use object_store::ObjectMeta;
use sha2::{Digest, Sha256};

use crate::store::{SourceMarker, SourceStatus};

// The chunk-key family (`object_key_hash` / `chunk_key` / `is_chunk_key`,
// `KEY_HASH_HEX_LEN`) sank into vectorfs-common (T14, ADR 0005): the client's
// addressing verbs (`vf chunks` prefix enumeration, `vf chunk show --around`)
// need the SAME shape judgment, and sigv4 is the standing precedent for
// one-source shared wire logic. Re-exported so this crate's historical
// `marker::…` call sites keep working — one implementation, two binaries.
pub use vectorfs_common::chunkkey::{KEY_HASH_HEX_LEN, chunk_key, is_chunk_key, object_key_hash};

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// sha256 of object bytes, full hex (the marker `content_hash`).
pub fn content_hash_of(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// cheap change signature
// ---------------------------------------------------------------------------

/// The CHEAP listing-level signature of one object: what the scanner diffs
/// without reading bytes (§5.10.5 "etag/内容哈希与标记不符").
///
/// * real etag when the backend lists one (S3 does; note S3 multipart etags
///   are not content hashes — that is fine, this is only the change SIGNAL);
/// * otherwise a size+mtime PSEUDO-signature — the file/memory backends have
///   no etag. (size, mtime_ns) is the conventional identity pair: a rewrite
///   that keeps both is invisible to the scanner until the next tick, but
///   the definitive direction is covered by the CONTENT hash inside the job
///   (metadata-only rewrites must not re-embed, §5.10.5), and a missed
///   change is picked up on the object's next mtime/size change.
pub fn signature_of(meta: &ObjectMeta) -> String {
    signature_from_parts(
        meta.e_tag.as_deref(),
        meta.size,
        meta.last_modified.timestamp_nanos_opt().unwrap_or_default(),
    )
}

/// The pure half of [`signature_of`], split out so the format is testable
/// without constructing an `ObjectMeta` (whose timestamp type would drag
/// chrono into direct use — it stays object_store's business).
pub fn signature_from_parts(e_tag: Option<&str>, size: u64, mtime_ns: i64) -> String {
    match e_tag {
        Some(etag) => format!("etag={etag}"),
        None => format!("size={size};mtime_ns={mtime_ns}"),
    }
}

// ---------------------------------------------------------------------------
// claim filtering (design-v3.md §3-R5: include/exclude glob 桶内认领过滤;
// 省缺 = 整桶 — the source bucket IS the namespace)
// ---------------------------------------------------------------------------

/// Compiled glob pattern (the §5.10.4 matcher, repurposed as an in-bucket
/// claim filter). Matching rules (deliberately tiny — a full glob engine
/// would be policy surface no user asked for):
/// * no metacharacter (`docs`): plain PREFIX match;
/// * otherwise segment glob: `*` matches within one `/`-segment, `**`
///   matches across segments (`docs/**` = strictly everything under `docs/`,
///   not `docs` itself);
/// * no other metacharacters.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    raw: Option<String>,
}

impl Scope {
    pub fn new(raw: Option<&str>) -> Scope {
        Scope {
            raw: raw
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string()),
        }
    }

    /// Does a bucket-relative object key match this pattern?
    pub fn matches(&self, key: &str) -> bool {
        let Some(pattern) = &self.raw else {
            return true;
        };
        if !pattern.contains('*') {
            // prefix form: "docs" matches "docs/a.txt" AND "docs-x/b" — the
            // design calls this 前缀 (prefix), not directory.
            return key.starts_with(pattern.as_str());
        }
        match_segments(
            &pattern.split('/').collect::<Vec<_>>(),
            &key.split('/').collect::<Vec<_>>(),
        )
    }
}

/// One bucket's claim filter (R5): `(include empty OR any include matches)
/// AND no exclude matches`. Affects claiming and vanish-flips ONLY — the GC
/// orphan live-set is ALWAYS the full bucket listing (include/exclude
/// narrowing must never trigger a mass-flip).
#[derive(Debug, Clone, Default)]
pub struct ClaimFilter {
    pub include: Vec<Scope>,
    pub exclude: Vec<Scope>,
}

impl ClaimFilter {
    pub fn new(include: &[String], exclude: &[String]) -> ClaimFilter {
        ClaimFilter {
            include: include
                .iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| Scope::new(Some(s)))
                .collect(),
            exclude: exclude
                .iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| Scope::new(Some(s)))
                .collect(),
        }
    }

    /// Empty filter (no include/exclude keys): claims the whole bucket.
    pub fn whole_bucket() -> ClaimFilter {
        ClaimFilter::default()
    }

    /// Is a bucket-relative key claimed by this filter?
    pub fn matches(&self, key: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|p| p.matches(key)))
            && !self.exclude.iter().any(|p| p.matches(key))
    }
}

/// Segment-recursion glob: a bare `**` segment consumes any number of
/// segments (but a TRAILING `**` requires at least one — `a/**` ≠ `a`); a
/// segment EMBEDDING `**` with literal parts (`**.pdf`, `docs**`) matches
/// any ONE segment satisfying the single-segment glob while allowing any
/// number of segments before it (the design-v3.md R6 example spells
/// include = ["**.pdf"]); `*` stays inside its segment; everything else is
/// literal.
fn match_segments(pat: &[&str], key: &[&str]) -> bool {
    match pat.split_first() {
        None => key.is_empty(),
        Some((&"**", rest)) => {
            if rest.is_empty() {
                return !key.is_empty();
            }
            (0..=key.len()).any(|i| match_segments(rest, &key[i..]))
        }
        Some((p, rest)) if p.contains("**") => {
            // embedded cross-segment wildcard: p may consume any number of
            // LEADING segments; one of them must satisfy the segment glob
            (0..key.len()).any(|i| seg_glob(p, key[i]) && match_segments(rest, &key[i + 1..]))
        }
        Some((p, rest)) => match key.split_first() {
            None => false,
            Some((&k, krest)) => seg_glob(p, k) && match_segments(rest, krest),
        },
    }
}

/// Single-segment glob: `*` = any run of non-`/` characters.
fn seg_glob(pat: &str, s: &str) -> bool {
    // classic two-pointer glob; no recursion needed for one metachar
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    let pb = pat.as_bytes();
    let sb = s.as_bytes();
    while si < sb.len() {
        if pi < pb.len() && (pb[pi] == b'?' || pb[pi] == sb[si]) {
            pi += 1;
            si += 1;
        } else if pi < pb.len() && pb[pi] == b'*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < pb.len() && pb[pi] == b'*' {
        pi += 1;
    }
    pi == pb.len()
}

// ---------------------------------------------------------------------------
// migration rules (the no-queue state machine, §5.10.5)
// ---------------------------------------------------------------------------

/// Terminal-for-auto-retry statuses: the scanner will never re-enqueue these
/// on its own. `PARTIALLY_INDEXED` counts as terminal by design — its failed
/// embedder already burned the retry budget inside the job; a scan loop that
/// re-attempted it every 30s against a permanently-dead provider would fight
/// the parking semantics (the remedy is `vf reindex`, T7).
pub fn is_terminal(m: &SourceMarker) -> bool {
    matches!(
        m.status,
        SourceStatus::Indexed | SourceStatus::PartiallyIndexed
    ) || (m.status == SourceStatus::Failed && m.retries_exhausted)
}

/// Scanner decision for an object PRESENT in the listing.
#[derive(Debug, PartialEq)]
pub enum ScanPlan {
    /// First sighting: persist this fresh PENDING marker and enqueue.
    New(SourceMarker),
    /// Signature changed on a non-completed marker: persist the PENDING
    /// rewrite (retries reset) and enqueue.
    Update(SourceMarker),
    /// Enqueue WITHOUT touching the marker:
    /// * unchanged signature + non-terminal marker — IN_PROGRESS crash
    ///   residue or a PENDING whose queue slot was lost; re-enqueueing is
    ///   the lock-free recovery (§5.10.5), put_vectors upsert makes it safe;
    /// * changed signature + COMPLETED marker — the job itself flips the
    ///   marker through IN_PROGRESS; rewriting it to PENDING here would
    ///   erase the completion evidence (`status`+`chunks`) the
    ///   content-hash skip needs to restore INDEXED vs PARTIALLY_INDEXED.
    Enqueue,
    /// Completed marker, unchanged signature: nothing to do.
    Settled,
    /// Marker carried N-strike residue (misses > 0) and the object is BACK:
    /// persist the reset counter WITHOUT enqueueing (a stale strike count
    /// must not make the next absence flip early — R5).
    Refresh(SourceMarker),
    /// FAILED + retries_exhausted: never auto-retried, whatever the
    /// signature (§5.10.5 — the operator's remedy is `vf reindex`, T7, or
    /// deleting the marker).
    Parked,
}

/// Evaluate one present object against its (possibly absent) marker.
pub fn plan_present(
    existing: Option<SourceMarker>,
    object_key: &str,
    sig: &str,
    fingerprint: &str,
    now: &str,
) -> ScanPlan {
    let Some(mut m) = existing else {
        return ScanPlan::New(SourceMarker {
            object_key: object_key.to_string(),
            etag_sig: sig.to_string(),
            content_hash: String::new(),
            pipeline_fingerprint: fingerprint.to_string(),
            status: SourceStatus::Pending,
            status_reason: None,
            retries: 0,
            retries_exhausted: false,
            misses: 0,
            updated_at: now.to_string(),
            chunks: None,
            engines: None,
            furniture_removed: None,
        });
    };
    // any present sighting resets the N-strike counter (R5): a stale count
    // from an old absence must not make the NEXT vanish flip after one
    // strike — and the reset must PERSIST (Refresh) when nothing else would
    // rewrite the marker
    let had_misses = m.misses > 0;
    m.misses = 0;
    if m.status == SourceStatus::Failed && m.retries_exhausted {
        return ScanPlan::Parked;
    }
    if m.status == SourceStatus::Deleting {
        // the object REAPPEARED after being marked DELETING: treat as a fresh
        // source. chunks/content_hash are cleared — GC (T7) may already have
        // deleted the old rows, so "same content, skip re-embed" must not
        // fire against a possibly-gone chunk set.
        m.status = SourceStatus::Pending;
        m.etag_sig = sig.to_string();
        m.content_hash = String::new();
        m.chunks = None;
        m.retries = 0;
        m.retries_exhausted = false;
        m.misses = 0;
        m.updated_at = now.to_string();
        return ScanPlan::Update(m);
    }
    if m.etag_sig == sig {
        if had_misses {
            return ScanPlan::Refresh(m);
        }
        return if is_terminal(&m) {
            ScanPlan::Settled
        } else {
            ScanPlan::Enqueue
        };
    }
    if is_terminal(&m) {
        // completed content, new cheap signature: enqueue; the job's
        // content-hash comparison decides re-embed vs signature-refresh.
        ScanPlan::Enqueue
    } else {
        m.status = SourceStatus::Pending;
        m.etag_sig = sig.to_string();
        m.retries = 0;
        m.retries_exhausted = false;
        m.status_reason = None;
        m.updated_at = now.to_string();
        ScanPlan::Update(m)
    }
}

/// Scanner decision for a marker whose object is ABSENT from the listing —
/// the N-strike reaper gate (design-v3.md R5: an object must be gone for
/// VANISH_STRIKES consecutive scans before the DELETING flip; a transient
/// empty/partial listing must not reap chunks).
#[derive(Debug, PartialEq)]
pub enum AbsentPlan {
    /// Strike counted, not yet at the threshold: persist the incremented
    /// counter, no flip, no enqueue.
    WaitForStrike(SourceMarker),
    /// Threshold reached: flip to DELETING (GC owns the chunks + marker).
    FlipDeleting(SourceMarker),
}

/// Consecutive absent scans required before the DELETING flip (R5: N=2).
pub const VANISH_STRIKES: u32 = 2;

pub fn plan_absent(existing: SourceMarker, now: &str) -> AbsentPlan {
    let mut m = existing;
    if m.status == SourceStatus::Deleting {
        return AbsentPlan::FlipDeleting(m); // already flipped; GC owns it
    }
    m.misses += 1;
    m.updated_at = now.to_string();
    if m.misses >= VANISH_STRIKES {
        m.status = SourceStatus::Deleting;
        AbsentPlan::FlipDeleting(m)
    } else {
        AbsentPlan::WaitForStrike(m)
    }
}

/// The DeleteSource path's IMMEDIATE flip (the 202 contract polls DELETING —
/// it must not wait out the strikes; the op itself is the definitive signal,
/// and the GC still re-verifies gone-ness before reaping).
pub fn plan_vanished(existing: SourceMarker, now: &str) -> Option<SourceMarker> {
    if existing.status == SourceStatus::Deleting {
        return None;
    }
    let mut m = existing;
    m.status = SourceStatus::Deleting;
    m.updated_at = now.to_string();
    Some(m)
}

/// Job-side skip test (§5.10.5 content-hash skip): the cheap signature moved
/// but the bytes are IDENTICAL to the last completed ingest — refresh the
/// signature, do not re-embed. Requires evidence of a prior completion
/// (`chunks` present, hence a non-empty content_hash comparison is
/// meaningful); FAILED/DELETING markers never carry chunks.
pub fn content_unchanged(prev: &SourceMarker, bytes_hash: &str) -> bool {
    !prev.content_hash.is_empty() && prev.content_hash == bytes_hash && prev.chunks.is_some()
}

/// RFC3339 now (reuses the ops epoch formatter — no chrono dependency).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    crate::ops::epoch_to_rfc3339(secs)
}

// ---------------------------------------------------------------------------
// tests: every migration edge, the skip rules and the scope matcher
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn marker(status: SourceStatus) -> SourceMarker {
        SourceMarker {
            object_key: "docs/a.txt".into(),
            etag_sig: "size=10;mtime_ns=1".into(),
            content_hash: "c".repeat(32),
            pipeline_fingerprint: "fp".into(),
            status,
            status_reason: None,
            retries: 1,
            retries_exhausted: false,
            misses: 0,
            updated_at: "t0".into(),
            chunks: None,
            engines: None,
            furniture_removed: None,
        }
    }

    fn completed(status: SourceStatus) -> SourceMarker {
        let mut m = marker(status);
        m.chunks = Some(BTreeMap::from([("text".to_string(), 5)]));
        m
    }

    const SIG: &str = "size=10;mtime_ns=1";
    const SIG2: &str = "size=11;mtime_ns=2";

    // (the key-family tests — hash stability, chunk-key shape recognition —
    // moved to vectorfs-common's chunkkey module with the implementation,
    // T14 sink; this module keeps the migration-edge tests below)

    #[test]
    fn scan_edges_no_marker_is_new_pending() {
        let plan = plan_present(None, "docs/a.txt", SIG, "fp", "t1");
        let ScanPlan::New(m) = plan else {
            panic!("expected New, got {plan:?}")
        };
        assert_eq!(m.status, SourceStatus::Pending);
        assert_eq!(m.etag_sig, SIG);
        assert_eq!(m.pipeline_fingerprint, "fp");
        assert_eq!(m.chunks, None);
        assert_eq!(content_hash_of(b"abc"), content_hash_of(b"abc"));
        assert_ne!(content_hash_of(b"abc"), content_hash_of(b"abd"));
    }

    #[test]
    fn scan_edges_unchanged() {
        // terminal + unchanged -> settled (nothing to do)
        for status in [SourceStatus::Indexed, SourceStatus::PartiallyIndexed] {
            let plan = plan_present(Some(completed(status)), "docs/a.txt", SIG, "fp", "t1");
            assert_eq!(plan, ScanPlan::Settled, "{status:?}");
        }
        // FAILED+exhausted + unchanged -> parked
        let mut f = marker(SourceStatus::Failed);
        f.retries_exhausted = true;
        assert_eq!(
            plan_present(Some(f.clone()), "docs/a.txt", SIG, "fp", "t1"),
            ScanPlan::Parked
        );
        // non-terminal + unchanged -> requeue only (crash residue recovery)
        for status in [
            SourceStatus::Pending,
            SourceStatus::InProgress,
            SourceStatus::Failed, // not exhausted: crashed mid-retry
        ] {
            let plan = plan_present(Some(marker(status)), "docs/a.txt", SIG, "fp", "t1");
            assert_eq!(plan, ScanPlan::Enqueue, "{status:?}");
        }
    }

    #[test]
    fn scan_edges_changed() {
        // completed + changed -> enqueue only (job owns the flip; the
        // completion evidence must survive for the content-hash skip)
        for status in [SourceStatus::Indexed, SourceStatus::PartiallyIndexed] {
            let plan = plan_present(Some(completed(status)), "docs/a.txt", SIG2, "fp", "t1");
            assert_eq!(plan, ScanPlan::Enqueue, "{status:?}");
        }
        // FAILED+exhausted + changed -> still parked (never auto-retry)
        let mut f = marker(SourceStatus::Failed);
        f.retries_exhausted = true;
        assert_eq!(
            plan_present(Some(f), "docs/a.txt", SIG2, "fp", "t1"),
            ScanPlan::Parked
        );
        // non-terminal + changed -> PENDING rewrite with retries reset
        for status in [SourceStatus::Pending, SourceStatus::InProgress] {
            let plan = plan_present(Some(marker(status)), "docs/a.txt", SIG2, "fp", "t1");
            let ScanPlan::Update(m) = plan else {
                panic!("expected Update for {status:?}")
            };
            assert_eq!(m.status, SourceStatus::Pending);
            assert_eq!(m.etag_sig, SIG2);
            assert_eq!(m.retries, 0);
            assert!(!m.retries_exhausted);
            assert_eq!(m.updated_at, "t1");
        }
    }

    #[test]
    fn scan_edges_deleting_object_reappears_as_fresh() {
        let mut m = completed(SourceStatus::Indexed);
        m.status = SourceStatus::Deleting;
        m.chunks = Some(BTreeMap::from([("text".to_string(), 5)]));
        let plan = plan_present(Some(m), "docs/a.txt", SIG2, "fp", "t1");
        let ScanPlan::Update(m) = plan else {
            panic!("expected Update, got {plan:?}")
        };
        assert_eq!(m.status, SourceStatus::Pending);
        assert_eq!(m.chunks, None, "old chunks may be GC'd — must not skip");
        assert_eq!(m.content_hash, "");
    }

    #[test]
    fn vanished_flips_to_deleting_once() {
        let m = completed(SourceStatus::Indexed);
        let Some(d) = plan_vanished(m.clone(), "t1") else {
            panic!("expected a DELETING marker")
        };
        assert_eq!(d.status, SourceStatus::Deleting);
        assert_eq!(d.updated_at, "t1");
        // already deleting -> None (idempotent)
        assert_eq!(plan_vanished(d, "t2"), None);
        // FAILED markers also flip (the object is gone; GC owns everything)
        let mut f = marker(SourceStatus::Failed);
        f.retries_exhausted = true;
        assert_eq!(
            plan_vanished(f, "t2").unwrap().status,
            SourceStatus::Deleting
        );
    }

    #[test]
    fn content_skip_requires_completion_evidence() {
        let done = completed(SourceStatus::Indexed);
        assert!(content_unchanged(&done, &done.content_hash));
        assert!(!content_unchanged(&done, &"d".repeat(32)));
        // no chunks (PENDING / FAILED / never completed) -> never skip
        let mut m = completed(SourceStatus::Indexed);
        m.chunks = None;
        assert!(!content_unchanged(&m, &m.content_hash));
        let mut m = completed(SourceStatus::Indexed);
        m.content_hash = String::new();
        assert!(!content_unchanged(&m, ""));
    }

    #[test]
    fn signature_prefers_etag_else_size_mtime() {
        assert_eq!(
            signature_from_parts(None, 42, 1_700_000_000_123_456_789),
            "size=42;mtime_ns=1700000000123456789"
        );
        assert_eq!(
            signature_from_parts(Some("\"abc\""), 42, 1),
            "etag=\"abc\"",
            "a listed etag always wins over the pseudo-signature"
        );
    }

    #[test]
    fn scope_matching_rules() {
        let whole = Scope::new(None);
        assert!(whole.matches("anything"));
        assert!(whole.matches("docs/a.txt"));

        let prefix = Scope::new(Some("docs"));
        assert!(prefix.matches("docs/a.txt"));
        assert!(
            prefix.matches("docs-deep/b.txt"),
            "prefix form, not directory"
        );
        assert!(!prefix.matches("other/a.txt"));

        let glob = Scope::new(Some("docs/**"));
        assert!(glob.matches("docs/a.txt"));
        assert!(glob.matches("docs/sub/deep/a.txt"));
        assert!(
            !glob.matches("docs"),
            "trailing ** needs at least one segment"
        );
        assert!(!glob.matches("docsx/a.txt"));

        let star = Scope::new(Some("docs/*.txt"));
        assert!(star.matches("docs/a.txt"));
        assert!(!star.matches("docs/sub/a.txt"), "* stays inside a segment");
        assert!(!star.matches("docs/a.md"));

        let mid = Scope::new(Some("docs/*/keep/**"));
        assert!(mid.matches("docs/x/keep/a.txt"));
        assert!(mid.matches("docs/x/keep/d/e.txt"));
        assert!(!mid.matches("docs/x/other/a.txt"));

        let empty = Scope::new(Some("  "));
        assert!(empty.matches("anything"), "blank pattern matches all");
    }

    /// Claim filter rules (design-v3.md R5): include absent = whole bucket;
    /// exclude wins over include; narrowing NEVER touches the GC live-set
    /// (which joins against the FULL listing — asserted in ingest tests).
    #[test]
    fn claim_filter_rules() {
        let whole = ClaimFilter::whole_bucket();
        assert!(whole.matches("docs/a.txt"));
        assert!(whole.matches("anything/at/all"));

        let inc = ClaimFilter::new(&["**.pdf".to_string()], &[]);
        assert!(inc.matches("docs/a.pdf"));
        assert!(inc.matches("b.pdf"));
        assert!(!inc.matches("docs/a.txt"));

        let multi = ClaimFilter::new(
            &["docs/**".to_string(), "notes/**".to_string()],
            &["**.tmp".to_string()],
        );
        assert!(multi.matches("docs/a.txt"));
        assert!(multi.matches("notes/b.txt"));
        assert!(!multi.matches("other/a.txt"));
        assert!(!multi.matches("docs/a.tmp"), "exclude wins");

        // blank entries are ignored, not empty-string patterns
        let blanks = ClaimFilter::new(&["  ".to_string()], &["".to_string()]);
        assert!(blanks.matches("x"), "blank = whole bucket");
    }

    /// N-strike vanish gate (R5): one absent scan is a strike, two flips;
    /// a present sighting resets the counter.
    #[test]
    fn absent_needs_two_strikes_and_resets_on_return() {
        let m = completed(SourceStatus::Indexed);
        // strike 1: wait
        let AbsentPlan::WaitForStrike(w1) = plan_absent(m.clone(), "t1") else {
            panic!("first absence must wait")
        };
        assert_eq!(w1.misses, 1);
        assert_eq!(w1.status, SourceStatus::Indexed);
        // strike 2: flip
        let AbsentPlan::FlipDeleting(d) = plan_absent(w1.clone(), "t2") else {
            panic!("second absence must flip")
        };
        assert_eq!(d.status, SourceStatus::Deleting);
        // already DELETING stays DELETING (idempotent for the GC pass)
        assert!(matches!(
            plan_absent(d.clone(), "t3"),
            AbsentPlan::FlipDeleting(_)
        ));
        // object returns after one strike: Refresh persists the reset (the
        // next absence then needs two strikes again)
        let ScanPlan::Refresh(r) = plan_present(Some(w1), "docs/a.txt", SIG, "fp", "t4") else {
            panic!("return-after-strike must persist the reset")
        };
        assert_eq!(r.misses, 0);
        let AbsentPlan::WaitForStrike(w2) = plan_absent(r, "t5") else {
            panic!("fresh absence must wait")
        };
        assert_eq!(w2.misses, 1);
    }
}
