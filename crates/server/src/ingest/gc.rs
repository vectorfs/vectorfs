//! 对账 GC + reindex 源内清理 (M2 T7, design.md §5.10.5).
//!
//! # The three anti-false-delete layers (GC 判据, v2.1)
//!
//! A vector is only ever eligible for reaping when ALL three hold:
//!
//! 1. **managed index** — the index's `index.json` carries the `x_vf_derived`
//!    stamp (the file layer created it). A pure-protocol user's index, however
//!    similar its rows, is never touched.
//! 2. **derived key shape** — the key parses as `{16hex}-{modality}-{seq}`
//!    ([`crate::ingest::marker::is_chunk_key`]). 裸前缀删除 (bare prefix
//!    deletes) is structurally impossible: every candidate key is parsed, and
//!    the uri grouping below works off the parsed set, never a prefix scan.
//! 3. **`x-vf-src-uri` points at a gone object** — the row's metadata joins
//!    the LIVE instance-root listing (the 真源): a chunk whose source object
//!    still exists survives, whatever its marker says.
//!
//! # Two reapers, one enumeration
//!
//! * **DELETING GC (immediate, never idle-gated)** — a Source marker in
//!   DELETING whose object is gone: delete its chunks across every managed
//!   index of the bucket, then the marker itself. Runs inside the same scan
//!   pass that (re)confirms the vanish, so `vf rm`'s 202 → poll converges
//!   within one wake + scan.
//! * **orphan sweep (periodic, idle-gated)** — every managed index of every
//!   bucket, chunks whose `x-vf-src-uri` matches NO live object. The
//!   anti-starvation discipline of an "optimize" pass: it only runs when the
//!   file layer owes no PENDING/IN_PROGRESS work. Orphan window = one scan
//!   period (`scan_interval_secs`, default 30s — surfaced in doctor's ingest
//!   probe).
//!
//! The reindex flow reuses the same enumeration to CLEAR a source's old
//! chunks before the rewrite (§5.10.5 "按旧指纹 filter 清旧块 → 重写 → 更新
//! 标记" — the clear covers ALL managed indexes of the bucket, because an
//! embedder config change derives a NEW index name while the old index keeps
//! its rows). Old-derived indexes are emptied but KEPT: deleting an index is
//! a destructive action M2 does not automate (doctor/status show them at
//! 0 rows; the operator may DeleteIndex by hand).

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use bytes::Bytes;

use crate::ingest::marker;
use crate::ops::{self, AppState};
use crate::store::{IndexMeta, Storage};

/// DeleteVectors wire limit (the ops layer rejects more; batching keeps every
/// call legal by construction, mirroring the put path's `BATCH_MAX_ROWS`).
const DELETE_BATCH: usize = 500;

/// GC 判据 layer 1: the bucket's MANAGED indexes — stamped `x_vf_derived`.
/// Deliberately bucket-wide, NOT "the current config's derived specs": an
/// embedder config change derives a different index NAME, and the previous
/// config's index (still stamped, still holding the old chunks) is exactly
/// what a reindex/GC must also cover.
pub(crate) async fn managed_indexes(storage: &Storage, vb: &str) -> Result<Vec<IndexMeta>, String> {
    storage
        .list_indexes(vb)
        .await
        .map_err(|e| e.to_string())
        .map(|ixs| {
            ixs.into_iter()
                .filter(|m| m.x_vf_derived.is_some())
                .collect()
        })
}

/// Enumerate one index's derived chunk rows grouped by `x-vf-src-uri`
/// (GC 判据 layers 2 + the metadata half of 3): keys must parse as chunk
/// keys; rows without a string `x-vf-src-uri` are not ours to touch. Works
/// against ANY index — the caller applies layer 1 by only passing managed
/// ones, which keeps the unstamped-index case unit-testable.
pub(crate) async fn chunk_keys_by_uri(
    storage: &Storage,
    vb: &str,
    ix: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let rows = storage
        .scan_vector_metadata(vb, ix)
        .await
        .map_err(|e| e.to_string())?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, meta) in rows {
        if !marker::is_chunk_key(&key) {
            continue; // layer 2: not a derived chunk key
        }
        let Some(uri) = meta
            .as_ref()
            .and_then(|m| m.get("x-vf-src-uri"))
            .and_then(|v| v.as_str())
        else {
            continue; // layer 3 (metadata half): no source attribution
        };
        out.entry(uri.to_string()).or_default().push(key);
    }
    Ok(out)
}

/// Delete chunk keys through the ops layer (`ops::delete_vectors` — same
/// handlers, same validation semantics, zero bypass; the put path's
/// `put_batch` uses the identical pattern) in ≤500-key batches.
pub(crate) async fn delete_chunk_keys(
    state: &AppState,
    vb: &str,
    ix: &str,
    keys: &[String],
) -> Result<(), String> {
    for batch in keys.chunks(DELETE_BATCH) {
        let body = serde_json::to_vec(&serde_json::json!({
            "vectorBucketName": vb,
            "indexName": ix,
            "keys": batch,
        }))
        .map_err(|e| format!("serializing delete batch: {e}"))?;
        ops::delete_vectors(State(state.clone()), Bytes::from(body))
            .await
            .map_err(|e| format!("{}: {}", e.shape, e.message))?;
    }
    Ok(())
}

/// reindex 源内清理: delete every derived chunk of the given sources across
/// ALL managed indexes of the bucket (old-derived ones included — the size
/// 64→96 shrink case is why this is a per-uri DELETE and not an upsert:
/// the extra high-ordinal keys would otherwise survive as orphans). Returns
/// the number of vector keys deleted.
pub(crate) async fn clear_source_chunks(
    state: &AppState,
    vb: &str,
    uris: &BTreeSet<String>,
) -> Result<usize, String> {
    let mut deleted = 0usize;
    for meta in managed_indexes(&state.storage, vb).await? {
        let by_uri = chunk_keys_by_uri(&state.storage, vb, &meta.index_name).await?;
        let keys: Vec<String> = by_uri
            .into_iter()
            .filter(|(uri, _)| uris.contains(uri))
            .flat_map(|(_, ks)| ks)
            .collect();
        if keys.is_empty() {
            continue;
        }
        delete_chunk_keys(state, vb, &meta.index_name, &keys).await?;
        deleted += keys.len();
    }
    Ok(deleted)
}

/// One DELETING-source reap (immediate GC; caller already verified the
/// object is really gone via the live listing). Deletes the source's chunks
/// across every managed index, then the marker. Returns the chunk-key count.
pub(crate) async fn reap_source(
    state: &AppState,
    vb: &str,
    object_key: &str,
) -> Result<usize, String> {
    let uri = marker::object_key_hash(object_key);
    let deleted = clear_source_chunks(state, vb, &BTreeSet::from([uri.clone()])).await?;
    state
        .storage
        .delete_source(vb, &uri)
        .await
        .map_err(|e| e.to_string())?;
    Ok(deleted)
}

/// 周期孤儿清扫 (idle-gated; see module doc) — ONE bucket per call
/// (design-v3.md R5: GC 作用域 = 本 daemon 持锁的桶集合, driven by the
/// caller): every managed index of the bucket, deleting derived chunks whose
/// `x-vf-src-uri` matches no LIVE object of THAT bucket's FULL listing (the
/// include/exclude claim filter never narrows this live set — 收窄不得
/// 触发 mass-flip). All three GC 判据 layers apply (`managed_indexes` =
/// layer 1, `chunk_keys_by_uri` = layers 2+3a, the live-join below =
/// layer 3b). Returns the number of vector keys deleted.
pub(crate) async fn orphan_sweep(
    state: &AppState,
    vb: &str,
    live_uris: &BTreeSet<String>,
) -> Result<usize, String> {
    let mut deleted = 0usize;
    for meta in managed_indexes(&state.storage, vb).await? {
        let by_uri = chunk_keys_by_uri(&state.storage, vb, &meta.index_name).await?;
        let keys: Vec<String> = by_uri
            .into_iter()
            .filter(|(uri, _)| !live_uris.contains(uri))
            .flat_map(|(_, ks)| ks)
            .collect();
        if keys.is_empty() {
            continue;
        }
        delete_chunk_keys(state, vb, &meta.index_name, &keys).await?;
        deleted += keys.len();
    }
    Ok(deleted)
}
