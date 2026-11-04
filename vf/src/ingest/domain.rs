//! Derived-index identity + idempotent domain creation (design.md §5.10.5:
//! "两个哈希各司其职：管线指纹管『块集身份』，派生键管『index 身份』").
//!
//! The derived key = `hash(model, dim, embedder instance config)` — full
//! sha256 hex over the canonical TOML of the embedder's flat config plus the
//! RESOLVED dim (for ort that is the startup probe, so swapping a same-named
//! model artifact for a different-shape one derives a different index
//! instead of silently mixing vector widths). The derived index NAME appends
//! the first 8 hex to a human prefix: `{index-prefix | embedder-name}-{8}`.
//! Name bases are charset/length-validated at CONFIG parse time
//! (pipeline/config.rs name gates), so every name minted here is
//! CreateIndex-legal by construction.

use sha2::{Digest, Sha256};

use crate::backends::Backend;
use crate::store::{BucketMeta, DerivedIndexInfo, IndexMeta, Storage};

use crate::pipeline::embed::EmbedderKind;

/// Everything the ingest worker needs to know about ONE embedder instance's
/// derived index.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedSpec {
    /// Legal, creatable index name (`{base}-{key8}`).
    pub index_name: String,
    /// Full sha256-hex derived key (the identity compared per batch).
    pub key: String,
    /// Embedder instance name (config key).
    pub embedder: String,
    /// Model identity (ort: `path` when set, else model name; openai: model).
    pub model: String,
    /// Resolved output dimensionality (ort: probed; openai: declared).
    pub dim: usize,
}

/// Hex chars of the derived key that go into the index NAME (64-bit prefix;
/// name collisions would need two configs hashing alike in 8 hex — and even
/// then the per-batch full-key check parks the writer before mixing).
pub const DERIVED_NAME_HEX_LEN: usize = 8;

/// Compute the derived key: sha256 over canonical TOML of
/// `{embedder = <flat config>, dim = N}`. BTreeMap-free plain-data config
/// (strings/ints only) serializes deterministically; `dim` is included
/// EXPLICITLY even though the openai config carries it — for ort it is the
/// probed value and must move the key when the artifact's shape changes.
pub fn derived_key(kind: &EmbedderKind, dim: usize) -> String {
    #[derive(serde::Serialize)]
    struct D<'a> {
        embedder: &'a EmbedderKind,
        dim: usize,
    }
    let canonical = toml::to_string(&D {
        embedder: kind,
        dim,
    })
    .expect("canonical TOML of plain embedder configs");
    let digest = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Model identity string recorded in `x_vf_derived` and `x-vf-model`
/// metadata. Delegates to the ort module's resolution matrix: for a
/// path-only embedder the identity is the directory name (the artifact under
/// `path` is what embeds; no built-in name was ever declared).
pub fn model_of(kind: &EmbedderKind) -> String {
    match kind {
        EmbedderKind::Ort(c) => super::super::pipeline::embed::ort::resolve_model_name(c),
        EmbedderKind::OpenAi(c) => c.model.clone(),
    }
}

/// Build the full spec for one embedder instance.
pub fn spec_for(
    name: &str,
    kind: &EmbedderKind,
    dim: usize,
    index_prefix: Option<&str>,
) -> DerivedSpec {
    let key = derived_key(kind, dim);
    let base = index_prefix.unwrap_or(name);
    DerivedSpec {
        index_name: format!("{base}-{}", &key[..DERIVED_NAME_HEX_LEN]),
        key,
        embedder: name.to_string(),
        model: model_of(kind),
        dim,
    }
}

// ---------------------------------------------------------------------------
// idempotent domain (§5.10.3 "首次写幂等建域")
// ---------------------------------------------------------------------------

/// Why an ensure/validate pass refused. `Mismatch` parks the FILE (per-batch
/// check) or refuses STARTUP (first-write domain setup) with an explicit
/// message; `Io` is transient storage trouble.
#[derive(Debug)]
pub enum DomainError {
    Mismatch(String),
    Io(String),
}

/// Ensure the ingest bucket + ONE derived index exist and match the spec.
///
/// * bucket missing → create (idempotent; a concurrent creation is a win);
/// * index missing → create with `dataType=float32`, `metric=cosine`, the
///   embedder's dim, `nonFilterableMetadataKeys=[x-vf-src-name,
///   x-vf-chunk-text]` (both always DECLARED; `store_chunk_text` only
///   controls whether the chunk-text KEY is written on rows) and the
///   `x_vf_derived` stamp;
/// * index present → validate dim / metric / derived key: mismatched dims or
///   a foreign/undecorated index under the derived name is an error (never a
///   silent reuse — that is how 768-d vectors end up in an 8-d index).
///
/// Called at startup for every ingest bucket AND before each put batch (the
/// protocol face can delete/recreate indexes between scans; recreating ours
/// is the same idempotent path).
pub async fn ensure_domain(
    storage: &Storage,
    vb: &str,
    spec: &DerivedSpec,
    region: &str,
    account: &str,
) -> Result<(), DomainError> {
    let io = |e: crate::store::StoreError| DomainError::Io(e.to_string());

    // ---- bucket ----
    match storage.load_bucket(vb).await.map_err(io)? {
        Some(_) => {}
        None => {
            let (epoch, rfc3339) = now_pair();
            let meta = BucketMeta {
                vector_bucket_name: vb.to_string(),
                vector_bucket_arn: format!("arn:aws:s3vectors:{region}:{account}:bucket/{vb}"),
                creation_time_rfc3339: rfc3339,
                creation_time_epoch: epoch,
                default_index_mode: None,
            };
            // put_json_create under the single-writer LOCK: Conflict here
            // means a concurrent creator won — reload and continue.
            if let Err(e) = storage.create_bucket(&meta).await {
                match e {
                    crate::store::StoreError::Conflict(_) => {}
                    other => return Err(DomainError::Io(other.to_string())),
                }
            }
        }
    }

    // ---- index ----
    match storage.load_index(vb, &spec.index_name).await.map_err(io)? {
        None => {
            let (epoch, rfc3339) = now_pair();
            let meta = IndexMeta {
                vector_bucket_name: vb.to_string(),
                index_name: spec.index_name.clone(),
                index_arn: format!(
                    "arn:aws:s3vectors:{region}:{account}:bucket/{vb}/index/{}",
                    spec.index_name
                ),
                creation_time_rfc3339: rfc3339,
                creation_time_epoch: epoch,
                data_type: "float32".to_string(),
                dimension: spec.dim as i32,
                distance_metric: "cosine".to_string(),
                non_filterable_metadata_keys: Some(vec![
                    "x-vf-src-name".to_string(),
                    "x-vf-chunk-text".to_string(),
                ]),
                index_mode: None,
                x_vf_derived: Some(DerivedIndexInfo {
                    key: spec.key.clone(),
                    embedder: spec.embedder.clone(),
                    model: spec.model.clone(),
                    dim: spec.dim,
                }),
            };
            if let Err(e) = storage.create_index(&meta).await {
                // someone else created it between load and create — fall
                // through to validation, which must agree with our spec
                let created_by_other = matches!(e, crate::store::StoreError::Conflict(_));
                if !created_by_other
                    || storage
                        .load_index(vb, &spec.index_name)
                        .await
                        .map_err(io)?
                        .is_none()
                {
                    return Err(DomainError::Io(e.to_string()));
                }
            }
            Ok(())
        }
        Some(meta) => validate_existing(vb, spec, &meta),
    }
}

/// The present-index half of the contract: same dims, cosine, and OUR
/// derived key — else a loud refusal naming both sides.
fn validate_existing(vb: &str, spec: &DerivedSpec, meta: &IndexMeta) -> Result<(), DomainError> {
    let want_dim = spec.dim as i32;
    if meta.dimension != want_dim {
        return Err(DomainError::Mismatch(format!(
            "index {vb}/{} exists with dimension {} but embedder '{}' derives dim {want_dim} — \
             refusing to mix vector widths (drop the index or run `vf reindex`)",
            spec.index_name, meta.dimension, spec.embedder
        )));
    }
    if meta.distance_metric != "cosine" {
        return Err(DomainError::Mismatch(format!(
            "index {vb}/{} exists with metric {} but derived indexes are created cosine — \
             refusing to write (drop the index or run `vf reindex`)",
            spec.index_name, meta.distance_metric
        )));
    }
    match &meta.x_vf_derived {
        Some(d) if d.key == spec.key => Ok(()),
        Some(d) => Err(DomainError::Mismatch(format!(
            "index {vb}/{} is derived from a DIFFERENT embedder configuration (its key starts \
             {}, this config derives {}) — run `vf reindex` after changing pipeline config, or \
             drop the index",
            spec.index_name,
            &d.key[..DERIVED_NAME_HEX_LEN],
            &spec.key[..DERIVED_NAME_HEX_LEN]
        ))),
        None => Err(DomainError::Mismatch(format!(
            "index {vb}/{} already exists WITHOUT an ingest derivation stamp — it is not managed \
             by the file layer; refusing to write into it (choose another index-prefix/embedder \
             name or drop the index)",
            spec.index_name
        ))),
    }
}

fn now_pair() -> (f64, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    (secs, crate::ops::epoch_to_rfc3339(secs))
}

/// Instance-root-relative key of a USER object (Source bytes live in the
/// user namespace; the `.vectorfs/` funnel in `Backend::object_key` is for
/// system objects only). S3 keys additionally carry the configured bucket
/// prefix.
pub fn user_object_key(backend: &Backend, key: &str) -> object_store::path::Path {
    match backend {
        Backend::S3(cfg) => cfg.catalog_key(key),
        Backend::File { .. } | Backend::Memory => object_store::path::Path::from(key),
    }
}

/// READ/DELETE key candidates of a user object, canonical form first.
///
/// Why two (T6 finding, file backend): `object_store::path::Path::from`
/// CANONICALIZES (percent-encodes non-ASCII and `%{}\^]"'>~<#|*?`), and the
/// local backend materializes that form — so objects WE write live under
/// canonical names. But an EXTERNALLY placed file (cp/mv into the instance
/// root — the §5.10.4 data-source model) keeps its RAW name, and
/// `LocalFileSystem::list` reports raw names. Reads/deletes therefore try
/// the canonical key first, then the raw form (built via `from_url_path`,
/// which validates without re-encoding). A key containing a literal `%`
/// stays canonical-only: the percent-decoding construction would be
/// ambiguous there (documented limitation; such keys also cannot round-trip
/// a listing decode).
pub fn user_object_key_candidates(backend: &Backend, key: &str) -> Vec<object_store::path::Path> {
    let primary = user_object_key(backend, key);
    if key.contains('%') {
        return vec![primary];
    }
    match object_store::path::Path::from_url_path(key) {
        Ok(alt) if alt != primary => vec![primary, alt],
        _ => vec![primary],
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ort_cfg(model: &str) -> EmbedderKind {
        EmbedderKind::from_config(
            "embedder.t",
            &toml::Value::from(model_kind_table(model)),
            None,
        )
        .unwrap()
    }

    fn model_kind_table(model: &str) -> toml::value::Table {
        let mut t = toml::value::Table::new();
        t.insert("kind".into(), "ort".into());
        t.insert("model".into(), model.into());
        t
    }

    #[test]
    fn derived_key_moves_with_every_identity_input() {
        let base = derived_key(&ort_cfg("gte-multilingual-base"), 768);
        assert_eq!(base.len(), 64);
        assert!(base.bytes().all(|b| b.is_ascii_hexdigit()));
        // same inputs -> same key (deterministic)
        assert_eq!(base, derived_key(&ort_cfg("gte-multilingual-base"), 768));
        // dim moves it (same-named artifact with a different shape)
        assert_ne!(base, derived_key(&ort_cfg("gte-multilingual-base"), 8));
        // model moves it
        assert_ne!(base, derived_key(&ort_cfg("bge-m3"), 768));
        // path moves it (self-managed artifact bypasses the pins)
        let mut t = model_kind_table("gte-multilingual-base");
        t.insert("path".into(), "/models/gte-fp16".into());
        let with_path =
            EmbedderKind::from_config("embedder.t", &toml::Value::from(t), None).unwrap();
        assert_ne!(base, derived_key(&with_path, 768));
        // openai vs ort never collide
        let mut t = toml::value::Table::new();
        t.insert("kind".into(), "openai".into());
        t.insert("endpoint".into(), "https://e/v1".into());
        t.insert("model".into(), "embedding-3".into());
        t.insert("dim".into(), 768.into());
        let oa = EmbedderKind::from_config("embedder.r", &toml::Value::from(t), None).unwrap();
        assert_ne!(base, derived_key(&oa, 768));
    }

    #[test]
    fn spec_names_and_models() {
        let kind = ort_cfg("gte-multilingual-base");
        let s = spec_for("text", &kind, 768, None);
        assert_eq!(s.index_name, format!("text-{}", &s.key[..8]));
        assert_eq!(s.model, "gte-multilingual-base");
        assert_eq!(s.dim, 768);
        // prefix wins over the instance name
        let p = spec_for("text", &kind, 768, Some("docs"));
        assert_eq!(p.index_name, format!("docs-{}", &p.key[..8]));
        assert_eq!(p.key, s.key, "the prefix is NOT part of index identity");
        // path models: an explicit model name is the identity; a path-only
        // embedder takes its name from the directory (ort::resolve_model_name)
        let mut t = model_kind_table("whatever");
        t.insert("path".into(), "/models/x".into());
        let kp = EmbedderKind::from_config("e", &toml::Value::from(t), None).unwrap();
        assert_eq!(model_of(&kp), "whatever");
        let mut t2 = toml::value::Table::new();
        t2.insert("kind".into(), "ort".into());
        t2.insert("path".into(), "/models/bge-m3".into());
        let kpo = EmbedderKind::from_config("e", &toml::Value::from(t2), None).unwrap();
        assert_eq!(model_of(&kpo), "bge-m3");
    }

    #[tokio::test]
    async fn ensure_domain_creates_validates_and_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::file(dir.path());
        let kind = ort_cfg("gte-multilingual-base");
        let spec = spec_for("text", &kind, 768, Some("docs"));

        // creates bucket + index with the stamp
        ensure_domain(&s, "docs", &spec, "us-east-1", "000000000000")
            .await
            .unwrap();
        let meta = s
            .load_index("docs", &spec.index_name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.dimension, 768);
        assert_eq!(meta.distance_metric, "cosine");
        assert_eq!(
            meta.non_filterable_metadata_keys,
            Some(vec![
                "x-vf-src-name".to_string(),
                "x-vf-chunk-text".to_string()
            ])
        );
        let d = meta.x_vf_derived.as_ref().unwrap();
        assert_eq!(d.key, spec.key);
        assert_eq!(d.embedder, "text");
        assert_eq!(d.model, "gte-multilingual-base");
        assert_eq!(d.dim, 768);
        // bucket really exists (ops resolve would 404 otherwise)
        assert!(s.load_bucket("docs").await.unwrap().is_some());

        // idempotent: same spec passes again
        ensure_domain(&s, "docs", &spec, "us-east-1", "000000000000")
            .await
            .unwrap();

        // dim mismatch refuses. NOTE: a config whose dim CHANGES derives a
        // different key and therefore a different index NAME — this check
        // fires only when an index under the SAME name carries the right
        // stamp but the wrong width (hand-crafted or foreign write):
        // simulate exactly that.
        let mut tampered = meta.clone();
        tampered.dimension = 999;
        s.delete_index("docs", &spec.index_name).await.unwrap();
        s.create_index(&tampered).await.unwrap();
        match ensure_domain(&s, "docs", &spec, "us-east-1", "000000000000").await {
            Err(DomainError::Mismatch(m)) => assert!(m.contains("dimension"), "{m}"),
            other => panic!("expected mismatch, got {other:?}"),
        }

        // a foreign (non-derived) index squatting on the derived name refuses
        let foreign = IndexMeta {
            vector_bucket_name: "docs".into(),
            index_name: spec.index_name.clone(),
            index_arn: "arn".into(),
            creation_time_rfc3339: "t".into(),
            creation_time_epoch: 0.0,
            data_type: "float32".into(),
            dimension: 768,
            distance_metric: "cosine".into(),
            non_filterable_metadata_keys: None,
            index_mode: None,
            x_vf_derived: None,
        };
        s.delete_index("docs", &spec.index_name).await.unwrap();
        s.create_index(&foreign).await.unwrap();
        match ensure_domain(&s, "docs", &spec, "us-east-1", "000000000000").await {
            Err(DomainError::Mismatch(m)) => {
                assert!(m.contains("derivation stamp"), "{m}")
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
        // ...and the "right key, wrong embedder stamp" shape refuses too
        let mut wrong_key = foreign;
        wrong_key.x_vf_derived = Some(DerivedIndexInfo {
            key: "f".repeat(64),
            embedder: "other".into(),
            model: "m".into(),
            dim: 768,
        });
        s.delete_index("docs", &spec.index_name).await.unwrap();
        s.create_index(&wrong_key).await.unwrap();
        match ensure_domain(&s, "docs", &spec, "us-east-1", "000000000000").await {
            Err(DomainError::Mismatch(m)) => assert!(m.contains("vf reindex"), "{m}"),
            other => panic!("expected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn user_object_key_maps_per_backend() {
        use crate::backends::S3BackendConfig;
        assert_eq!(
            user_object_key(&Backend::File { root: "/d".into() }, "docs/a.txt").as_ref(),
            "docs/a.txt"
        );
        assert_eq!(
            user_object_key(&Backend::Memory, "docs/a.txt").as_ref(),
            "docs/a.txt"
        );
        let cfg = S3BackendConfig {
            endpoint: "http://x".into(),
            bucket: "b".into(),
            prefix: "pre".into(),
            region: "us-east-1".into(),
            access_key: None,
            secret_key: None,
        };
        assert_eq!(
            user_object_key(&Backend::S3(Box::new(cfg)), "docs/a.txt").as_ref(),
            "pre/docs/a.txt"
        );
    }

    /// The canonical/raw candidate pair (T6 finding): Path::from encodes
    /// non-ASCII + AWS-discouraged ASCII, so reads must also try the raw
    /// form; a literal '%' key stays canonical-only (decode ambiguity).
    #[test]
    fn user_object_key_candidates_canonical_then_raw() {
        let plain = user_object_key_candidates(&Backend::Memory, "docs/a.txt");
        assert_eq!(plain.len(), 1, "ASCII-plain keys have one form");

        let uni = user_object_key_candidates(&Backend::Memory, "docs/汉字 文档#1.txt");
        assert_eq!(uni.len(), 2, "canonical + raw");
        assert!(uni[0].as_ref().contains("%E6%B1%89"), "{}", uni[0].as_ref());
        assert_eq!(uni[1].as_ref(), "docs/汉字 文档#1.txt", "raw form second");

        let pct = user_object_key_candidates(&Backend::Memory, "docs/100%done.txt");
        assert_eq!(pct.len(), 1, "literal '%' never falls back to decoding");
    }
}
