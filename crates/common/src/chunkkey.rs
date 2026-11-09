//! The chunk-key CONTRACT (ADR 0005 / design 2026-11-content-addressing §3,
//! T14): `{16hex}-{modality}-{i}` is not a private storage detail — it is
//! the vector world's line number, an ORDERED, CONTIGUOUS, ENUMERABLE,
//! NEIGHBORHOOD-ADDRESSABLE document address space. The server mints the
//! keys at ingest ([`chunk_key`], GC-reaps with [`is_chunk_key`]); the
//! CLIENT enumerates them (`vf chunks` prefix scan) and windows them
//! (`vf chunk show --around`) — which is why this lives in common (T14
//! sink: the sigv4 同源一套 precedent — one shape judgment, both binaries;
//! never a client-side copy).
//!
//! Consequences (ADR 0005): any evolution of this shape must keep
//! [`parse_chunk_key`] compatible — the client's addressing verbs depend on
//! the three-segment grammar, and `i` remains a per-modality u32 counter
//! that is monotonic and gap-free per source (the chunker's windowing
//! guarantees it; the ingest tests pin it).

use sha2::{Digest, Sha256};

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

/// The parsed chunk key: borrowed views of the three contract segments.
/// `index` is the NUMERIC ordinal (the key spells it in digits; lexical
/// order is NOT numeric order — `-text-10` sorts before `-text-2` — so every
/// address-space consumer must sort by the parsed number, never the string).
#[derive(Debug, PartialEq, Eq)]
pub struct ChunkKeyParts<'a> {
    pub hash: &'a str,
    pub modality: &'a str,
    pub index: u32,
}

/// Parse a key as the chunk-key shape `{16hex}-{modality}-{ordinal}` — the
/// inverse of [`chunk_key`] and the ONE shape judgment both binaries share.
///
/// Server use: GC 判据 layer 2 (§5.10.5 anti-false-delete) — see
/// [`is_chunk_key`]. Client use (T14): numeric `i` ordering, anchor-key
/// validation, and the prefix enumeration's address-space membership test.
/// Returns `None` for everything else (a pure-protocol user's key must be
/// shaped EXACTLY like ours to enter the address space at all).
pub fn parse_chunk_key(key: &str) -> Option<ChunkKeyParts<'_>> {
    let mut parts = key.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(hash), Some(modality), Some(seq), None) => {
            if hash.len() == KEY_HASH_HEX_LEN
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && !modality.is_empty()
                && modality.bytes().all(|b| b.is_ascii_alphanumeric())
                && !seq.is_empty()
                && seq.bytes().all(|b| b.is_ascii_digit())
            {
                Some(ChunkKeyParts {
                    hash,
                    modality,
                    index: seq.parse().ok()?,
                })
            } else {
                None
            }
        }
        _ => None,
    }
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
    parse_chunk_key(key).is_some()
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

// ---------------------------------------------------------------------------
// tests (moved from server ingest/marker.rs when the key family sank into
// common — T14; the marker-side migration tests stayed there)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The CLIENT half of the contract (T14): parse inverts mint, and the
    /// ordinal comes back as a NUMBER — the lexical-order trap
    /// (`-text-10` < `-text-2` as strings) is exactly why consumers sort on
    /// `index`, never on the key string.
    #[test]
    fn parse_round_trips_and_orders_numerically() {
        let h = object_key_hash("docs/hello.txt");
        let minted = chunk_key(&h, "text", 42);
        let p = parse_chunk_key(&minted).expect("ours always parse");
        assert_eq!(
            p,
            ChunkKeyParts {
                hash: h.as_str(),
                modality: "text",
                index: 42,
            }
        );
        let zero_image = chunk_key(&h, "image", 0);
        assert_eq!(parse_chunk_key(&zero_image).unwrap().index, 0);
        // the lexical trap, pinned: numeric 2 < 10 but lexical "text-10" <
        // "text-2" — only the parsed number orders the address space
        let k2 = chunk_key(&h, "text", 2);
        let k10 = chunk_key(&h, "text", 10);
        let (two, ten) = (
            parse_chunk_key(&k2).unwrap(),
            parse_chunk_key(&k10).unwrap(),
        );
        assert!(two.index < ten.index);
        assert!(
            k10.as_str() < k2.as_str(),
            "self-check: the KEY STRINGS are lexically inverted"
        );
        // ordinal wider than u32 is not a shape failure in the grammar, but
        // it cannot be a real chunk (counters are u32) — None, not a panic
        assert!(parse_chunk_key(&format!("{h}-text-99999999999")).is_none());
    }
}
