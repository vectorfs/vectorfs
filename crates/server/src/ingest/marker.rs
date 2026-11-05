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

/// Hex chars of sha256(object key) used as the marker FILE name base and as
/// `x-vf-src-uri` / chunk-key prefix.
///
/// 16 hex = 64 bits. Birthday-collision odds for N sources are ~N²/2^65:
/// even a million sources give ~2.7e-8 — and a collision would need TWO
/// colliding keys ALIVE in the same bucket to misbehave (the marker of one
/// would shadow the other). File-name safety was the other constraint: hex
/// needs no escaping on every object-store backend.
pub const KEY_HASH_HEX_LEN: usize = 16;

/// Marker/chunk key base: first [`KEY_HASH_HEX_LEN`] hex of sha256(key).
pub fn object_key_hash(object_key: &str) -> String {
    let digest = Sha256::digest(object_key.as_bytes());
    hex(&digest)[..KEY_HASH_HEX_LEN].to_string()
}

/// Chunk key (§5.10.1): `{keyHash16}-{modality}-{index}` — short, stable, and
/// containing the OBJECT key hash so two same-content files never stomp each
/// other's chunks (they are two Sources).
pub fn chunk_key(key_hash: &str, modality: &str, index: u32) -> String {
    format!("{key_hash}-{modality}-{index}")
}

/// Does `key` carry the DERIVED chunk-key shape `{16hex}-{modality}-{seq}`?
/// GC 判据 layer 2 (§5.10.5 anti-false-delete): the reaper only considers
/// vectors whose key parses as (exactly three `-`-free segments) a 16-hex
/// object-key hash, an alphanumeric modality token ("text" in M2; "image" /
/// "audio" later), and an all-digit ordinal. Keys minted by [`chunk_key`]
/// always satisfy this; a pure-protocol user's key must be shaped EXACTLY
/// like ours to be eligible at all — and even then layers 1 (managed-index
/// stamp) and 3 (`x-vf-src-uri` joins a vanished object) still gate the
/// delete. 裸前缀删除 is what this function exists to prevent.
pub fn is_chunk_key(key: &str) -> bool {
    let mut parts = key.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(hash), Some(modality), Some(seq), None) => {
            hash.len() == KEY_HASH_HEX_LEN
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && !modality.is_empty()
                && modality.bytes().all(|b| b.is_ascii_alphanumeric())
                && !seq.is_empty()
                && seq.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    }
}

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
// scope matching (§5.10.4: "scope = 前缀/glob; 省略 = 整根")
// ---------------------------------------------------------------------------

/// Compiled ingest scope. `None` = the whole instance root.
///
/// Matching rules (deliberately tiny — a full glob engine would be policy
/// surface no user asked for):
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

    /// Does an instance-root-relative object key fall inside this scope?
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

/// Segment-recursion glob: `**` consumes any number of segments (but a
/// TRAILING `**` requires at least one — `a/**` ≠ `a`), `*` stays inside its
/// segment, everything else is literal.
fn match_segments(pat: &[&str], key: &[&str]) -> bool {
    match pat.split_first() {
        None => key.is_empty(),
        Some((&"**", rest)) => {
            if rest.is_empty() {
                return !key.is_empty();
            }
            (0..=key.len()).any(|i| match_segments(rest, &key[i..]))
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
            updated_at: now.to_string(),
            chunks: None,
            engines: None,
        });
    };
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
        m.updated_at = now.to_string();
        return ScanPlan::Update(m);
    }
    if m.etag_sig == sig {
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

/// Scanner decision for a marker whose object is GONE from the listing: flip
/// to DELETING (this task only sets the state; T7's GC reaps the chunks and
/// the marker itself). `None` = already DELETING.
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
            updated_at: "t0".into(),
            chunks: None,
            engines: None,
        }
    }

    fn completed(status: SourceStatus) -> SourceMarker {
        let mut m = marker(status);
        m.chunks = Some(BTreeMap::from([("text".to_string(), 5)]));
        m
    }

    const SIG: &str = "size=10;mtime_ns=1";
    const SIG2: &str = "size=11;mtime_ns=2";

    #[test]
    fn key_hash_is_stable_short_hex() {
        let h = object_key_hash("docs/hello.txt");
        assert_eq!(h.len(), KEY_HASH_HEX_LEN);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(h, object_key_hash("docs/hello.txt"));
        assert_ne!(h, object_key_hash("docs/hello.md"));
        // chunk keys are the same base + modality + ordinal
        assert_eq!(chunk_key(&h, "text", 0), format!("{h}-text-0"));
        assert_eq!(chunk_key(&h, "text", 12), format!("{h}-text-12"));
    }

    #[test]
    fn chunk_key_shape_recognition() {
        let h = object_key_hash("docs/hello.txt");
        // ours always match, any modality token / ordinal width
        for k in [
            chunk_key(&h, "text", 0),
            chunk_key(&h, "image", 12345),
            chunk_key(&h, "audio", 7),
        ] {
            assert!(is_chunk_key(&k), "{k}");
        }
        // lookalikes that must NOT pass (GC 判据 layer 2)
        assert!(!is_chunk_key("custom-key-1"), "user-shaped key");
        assert!(!is_chunk_key(&format!("{}-text-0", &h[..15])), "short hash");
        assert!(!is_chunk_key(&format!("{h}-text-1x")), "ordinal not digits");
        assert!(!is_chunk_key(&format!("{h}-te-xt-1")), "extra segment");
        assert!(!is_chunk_key(&format!("{h}--1")), "empty modality");
        assert!(!is_chunk_key(&format!("{h}-text-")), "empty ordinal");
        assert!(
            !is_chunk_key(&format!("{}-text-1", "z".repeat(16))),
            "hash not hex"
        );
        assert!(!is_chunk_key("k1"), "plain protocol key");
        assert!(!is_chunk_key(""), "empty");
    }

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
        assert!(whole.matches(".vectorfs/anything")); // exclusion is the SCAN's job, not scope's
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
        assert!(empty.matches("anything"), "blank scope = whole root");
    }
}
