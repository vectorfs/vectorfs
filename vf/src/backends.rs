//! Unified storage backend abstraction (design.md §5.2): one `Backend` enum
//! for `file` / `memory` / `s3`, each producing
//! (a) an `Arc<dyn object_store::ObjectStore>` used for ALL catalog JSON
//!     (bucket.json / index.json / policy.json / tags.json) and the LOCK
//!     object — reads are `get` (NotFound -> 404), creations are
//!     `put_opts(PutMode::Create)` (race-free create-if-not-exists on every
//!     target backend: O_EXCL on LocalFileSystem, If-None-Match:* on S3,
//!     map-insert on InMemory), overwrites are `PutMode::Overwrite` (or
//!     `PutMode::Update(etag)` where a CAS is needed — see lock.rs);
//! (b) a lancedb `Session` whose `ObjectStoreRegistry` routes Lance dataset
//!     URIs through the same object_store abstraction.
//!
//! Injection route (M0-verified, zero AWS SDK; previously s3.rs, absorbed
//! here so all three backends live in one module):
//! 1. `object_store` crate's own S3 client (`object_store::aws::AmazonS3`,
//!    reqwest + aws-lc-rs only — no aws-config chain).
//! 2. Providers wrapped into lance_io `ObjectStoreProvider`s registered on an
//!    `ObjectStoreRegistry`; the registry goes into a custom `lance::Session`,
//!    handed to `lancedb::ConnectBuilder::session()`. Storage options flow via
//!    `ConnectBuilder::storage_options()` into `ObjectStoreParams`.
//! 3. Lance manifest commits on the "s3" scheme use
//!    `ConditionalPutCommitHandler` = `PutMode::Create` (If-None-Match:*).
//!
//! Backend-specific notes:
//! - file: the registry is lance-io's OWN default `FileStoreProvider`
//!   (lance's optimized local code paths — direct IO probing, local dir
//!   operations). We deliberately do NOT replace it with a plain
//!   LocalFileSystem provider: that would give up lance's local fast paths
//!   for zero gain. The catalog side still goes through object_store's
//!   `LocalFileSystem` (prefix-rooted), which is a different, tiny overhead
//!   path. Net difference vs the old spike: engine opens `file://` URIs via
//!   the session instead of bare paths — same provider underneath.
//! - memory: lance-io's default memory provider hands every `new_store` call a
//!   FRESH `InMemory` map; our registry cache holds only WEAK references, so
//!   per-operation Connections (current store.rs design) could let the map be
//!   dropped between operations, silently losing datasets. We therefore
//!   register `SharedMemoryProvider`, which returns ONE `Arc<InMemory>`
//!   owned by this `Backend` for the whole daemon lifetime. The same instance
//!   also serves the catalog, so `vf status`-style tooling sees one coherent
//!   store. The `memory` backend is volatile by definition (CI/emulator mode).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lance_io::object_store::providers::ObjectStoreProvider;
use lance_io::object_store::{ObjectStore, ObjectStoreParams, ObjectStoreRegistry};
use lancedb::Session;
use url::Url;

/// Cloud block size lance's own s3 provider uses (64KB; const is private there).
const CLOUD_BLOCK_SIZE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// S3 provider (from the M0 spike, unchanged in substance)
// ---------------------------------------------------------------------------

/// Provider that builds an `object_store::aws::AmazonS3` from the storage
/// options (endpoint / static creds / region / allow_http) carried in
/// `ObjectStoreParams`.
#[derive(Debug, Default)]
pub struct StaticS3Provider;

#[async_trait::async_trait]
impl ObjectStoreProvider for StaticS3Provider {
    async fn new_store(
        &self,
        base_path: Url,
        params: &ObjectStoreParams,
    ) -> lance::Result<ObjectStore> {
        use object_store::aws::AmazonS3Builder;

        let opts = params.storage_options().cloned().unwrap_or_default();
        let mut builder = AmazonS3Builder::new().with_url(base_path.to_string());
        for (k, v) in &opts {
            // Unknown keys (e.g. lance-internal ones) are skipped, matching
            // lance-io's own provider behavior of only forwarding s3 keys.
            if let Ok(key) = k.parse::<object_store::aws::AmazonS3ConfigKey>() {
                builder = builder.with_config(key, v.clone());
            }
        }
        let store: Arc<dyn object_store::ObjectStore> =
            Arc::new(builder.build().map_err(lance_err)?);

        // Public constructor (struct literals are not possible from outside
        // lance-io: private fields). NOTE: it derives the metrics prefix from a
        // default registry that has no "s3" provider without lance-io's aws
        // feature, so it logs a guess warning — value is identical ("s3$bucket").
        Ok(ObjectStore::new(
            store,
            base_path.clone(),
            Some(CLOUD_BLOCK_SIZE),
            None,
            false, // use_constant_size_upload_parts (only Cloudflare R2 needs true)
            true,  // list_is_lexically_ordered (true for object stores)
            lance_io::object_store::DEFAULT_CLOUD_IO_PARALLELISM,
            lance_io::object_store::DEFAULT_DOWNLOAD_RETRY_COUNT,
            Some(&opts_for_prefix(&opts)),
        ))
    }
}

/// Storage options must not contain base-scoped keys when hashed for the
/// registry cache; passing the raw map matches what lance-io's own provider
/// does (it forwards the raw options too).
fn opts_for_prefix(opts: &HashMap<String, String>) -> HashMap<String, String> {
    opts.clone()
}

/// object_store::Error -> lance::Error (trait signature requires lance's Result).
fn lance_err(e: object_store::Error) -> lance::Error {
    lance::Error::invalid_input_source(Box::new(e))
}

/// Parsed `--backend s3+http://host:port/bucket[/prefix]` configuration.
#[derive(Debug, Clone)]
pub struct S3BackendConfig {
    pub endpoint: String,
    pub bucket: String,
    /// Optional key prefix inside the bucket ("" = bucket root).
    pub prefix: String,
    pub region: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
}

impl S3BackendConfig {
    /// Parse `s3+<endpoint-url>/<bucket>[/prefix]`.
    pub fn parse(spec: &str) -> Option<Self> {
        let rest = spec.strip_prefix("s3+")?;
        let url = Url::parse(rest).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let endpoint = format!("{}://{}", url.scheme(), url.authority());
        let path = url.path().trim_start_matches('/');
        let (bucket, prefix) = match path.split_once('/') {
            Some((b, p)) => (b.to_string(), p.trim_matches('/').to_string()),
            None => (path.to_string(), String::new()),
        };
        if bucket.is_empty() {
            return None;
        }
        Some(Self {
            endpoint,
            bucket,
            prefix,
            region: "us-east-1".to_string(), // MinIO/seaweedfs convention; overridden by caller
            access_key: None,
            secret_key: None,
        })
    }

    /// Storage options consumed by our provider (object_store config keys).
    pub fn storage_options(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("endpoint".to_string(), self.endpoint.clone());
        m.insert("region".to_string(), self.region.clone());
        m.insert("skip_signature".to_string(), "false".to_string());
        match (&self.access_key, &self.secret_key) {
            (Some(ak), Some(sk)) => {
                m.insert("access_key_id".to_string(), ak.clone());
                m.insert("secret_access_key".to_string(), sk.clone());
            }
            _ => {
                // anonymous (e.g. seaweedfs embedded filer without IAM)
                m.insert("skip_signature".to_string(), "true".to_string());
            }
        }
        if self.endpoint.starts_with("http://") {
            m.insert("allow_http".to_string(), "true".to_string());
        }
        m
    }

    /// Lance dataset URI for an index's data dir.
    pub fn lance_uri(&self, vb: &str, ix: &str) -> String {
        match self.prefix.is_empty() {
            true => format!("s3://{}/buckets/{}/indexes/{}/data", self.bucket, vb, ix),
            false => format!(
                "s3://{}/{}/buckets/{}/indexes/{}/data",
                self.bucket, self.prefix, vb, ix
            ),
        }
    }

    /// Object key (relative to bucket) for a catalog JSON object.
    pub fn catalog_key(&self, rel: &str) -> object_store::path::Path {
        let full = if self.prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_matches('/'), rel)
        };
        object_store::path::Path::from(full)
    }

    /// Direct object_store client for catalog JSON (separate from lance).
    pub fn build_catalog_store(&self) -> object_store::Result<Arc<dyn object_store::ObjectStore>> {
        use object_store::aws::AmazonS3Builder;
        let mut builder = AmazonS3Builder::new()
            .with_url(format!("s3://{}", self.bucket))
            .with_config(
                object_store::aws::AmazonS3ConfigKey::Endpoint,
                &self.endpoint,
            )
            .with_config(object_store::aws::AmazonS3ConfigKey::Region, &self.region)
            .with_allow_http(self.endpoint.starts_with("http://"));
        if let (Some(ak), Some(sk)) = (&self.access_key, &self.secret_key) {
            builder = builder
                .with_config(object_store::aws::AmazonS3ConfigKey::AccessKeyId, ak)
                .with_config(object_store::aws::AmazonS3ConfigKey::SecretAccessKey, sk);
        } else {
            builder =
                builder.with_config(object_store::aws::AmazonS3ConfigKey::SkipSignature, "true");
        }
        Ok(Arc::new(builder.build()?))
    }
}

// ---------------------------------------------------------------------------
// memory provider
// ---------------------------------------------------------------------------

/// Memory provider that hands out ONE shared `InMemory` map for the lifetime
/// of the owning `Backend` (see module doc for why lance's default — a fresh
/// map per `new_store` — is wrong for our per-operation connections).
#[derive(Debug)]
pub struct SharedMemoryProvider {
    inner: Arc<object_store::memory::InMemory>,
}

#[async_trait::async_trait]
impl ObjectStoreProvider for SharedMemoryProvider {
    async fn new_store(
        &self,
        base_path: Url,
        params: &ObjectStoreParams,
    ) -> lance::Result<ObjectStore> {
        let opts = params.storage_options().cloned().unwrap_or_default();
        Ok(ObjectStore::new(
            self.inner.clone(),
            base_path.clone(),
            None,
            None,
            false,
            true, // btree map -> lexically ordered
            lance_io::object_store::DEFAULT_CLOUD_IO_PARALLELISM,
            lance_io::object_store::DEFAULT_DOWNLOAD_RETRY_COUNT,
            Some(&opts),
        ))
    }

    /// Mirrors lance-io's own memory provider: the URL domain (we always use
    /// the constant "vf") plus the path form the object key.
    fn extract_path(&self, url: &Url) -> lance::Result<object_store::path::Path> {
        let mut output = String::new();
        if let Some(domain) = url.domain() {
            output.push_str(domain);
        }
        output.push_str(url.path());
        // The in-memory store uses the Path directly as a key with no HTTP
        // layer, so there is no re-encoding step and thus no double-encoding
        // to avoid (same reasoning as lance-io's memory provider).
        Ok(object_store::path::Path::from(output))
    }

    fn calculate_object_store_prefix(
        &self,
        _url: &Url,
        _storage_options: Option<&HashMap<String, String>>,
    ) -> lance::Result<String> {
        Ok("memory".to_string())
    }
}

/// Domain component of memory:// dataset URIs. Constant so every URI spells
/// the same namespace (`memory://vf/buckets/<vb>/indexes/<ix>/data`).
const MEMORY_HOST: &str = "vf";

// ---------------------------------------------------------------------------
// Backend enum
// ---------------------------------------------------------------------------

/// A configured storage backend. Everything above this type (catalog, engine,
/// LOCK) is backend-agnostic.
#[derive(Debug, Clone)]
pub enum Backend {
    /// Local directory root (default). `root` is canonicalized to an absolute
    /// path so `file://` engine URIs parse.
    File { root: PathBuf },
    /// Volatile process-local store (CI / emulator mode).
    Memory,
    /// S3-compatible remote via `s3+http(s)://host:port/bucket[/prefix]`.
    S3(Box<S3BackendConfig>),
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::File { root } => write!(f, "file:{}", root.display()),
            Backend::Memory => write!(f, "memory"),
            Backend::S3(cfg) => write!(f, "s3:{}/{}{}", cfg.bucket, cfg.prefix, cfg.endpoint),
        }
    }
}

impl Backend {
    /// Parse a `--backend` spec. Three forms (design.md §5.2 / M1 T2):
    /// - `file` → the `--data-dir` directory (spike-era `file:<dir>` also
    ///   accepted for compatibility),
    /// - `memory` → volatile store,
    /// - `s3+http(s)://host:port/bucket[/prefix]` → S3-compatible remote.
    ///
    /// The file root is created + canonicalized HERE (not in `build`) so every
    /// derived artifact — `file://` engine URIs, the prefix-rooted catalog
    /// store, the local flock path — spells the same absolute directory.
    pub fn parse(spec: &str, data_dir: &std::path::Path) -> Result<Self, String> {
        match spec {
            "file" => Ok(Backend::File {
                root: file_root(data_dir)?,
            }),
            "memory" => Ok(Backend::Memory),
            _ if spec.starts_with("file:") => Ok(Backend::File {
                root: file_root(std::path::Path::new(spec.strip_prefix("file:").unwrap()))?,
            }),
            _ if spec.starts_with("s3+") => S3BackendConfig::parse(spec)
                .map(|cfg| Backend::S3(Box::new(cfg)))
                .ok_or_else(|| {
                    format!(
                        "invalid s3 backend {spec:?} \
                         (expected s3+http://host:port/bucket[/prefix])"
                    )
                }),
            other => Err(format!(
                "cannot parse --backend {other:?} \
                 (expected file | memory | s3+http://host:port/bucket[/prefix])"
            )),
        }
    }

    /// Lance dataset URI for one index's data dir (§5.3 layout).
    pub fn lance_uri(&self, vb: &str, ix: &str) -> String {
        match self {
            Backend::File { root } => {
                format!("file://{}/buckets/{vb}/indexes/{ix}/data", root.display())
            }
            Backend::Memory => format!("memory://{MEMORY_HOST}/buckets/{vb}/indexes/{ix}/data"),
            Backend::S3(cfg) => cfg.lance_uri(vb, ix),
        }
    }

    /// Object-store key for a catalog/LOCK object relative to the backend root.
    pub fn object_key(&self, rel: &str) -> object_store::path::Path {
        match self {
            // The file catalog store is prefix-rooted at `root`; memory keys
            // are plain relative paths on the shared map.
            Backend::File { .. } | Backend::Memory => object_store::path::Path::from(rel),
            Backend::S3(cfg) => cfg.catalog_key(rel),
        }
    }

    /// Storage options to pass to `ConnectBuilder` for engine connections
    /// (S3 only; file/memory need none).
    pub fn storage_options(&self) -> Option<HashMap<String, String>> {
        match self {
            Backend::S3(cfg) => Some(cfg.storage_options()),
            _ => None,
        }
    }

    /// Idempotently normalize a File root (create + canonicalize); other
    /// variants pass through. `parse` already does this — `Storage::new` calls
    /// it again so hand-built `Backend::File` values are safe too.
    pub fn normalized(self) -> Result<Self, String> {
        match self {
            Backend::File { root } => Ok(Backend::File {
                root: file_root(&root)?,
            }),
            other => Ok(other),
        }
    }

    /// Local flock file path (file backend only — same-machine double-start
    /// guard; see lock.rs). A separate dotfile, NOT the LOCK object: the LOCK
    /// object is rewritten by put/delete, which would swap the inode out from
    /// under the flock.
    pub fn flock_path(&self) -> Option<std::path::PathBuf> {
        match self {
            Backend::File { root } => Some(root.join(".vf.flock")),
            _ => None,
        }
    }

    /// Build (a) the object store serving catalog + LOCK objects and (b) the
    /// lancedb session whose registry serves engine datasets — the two halves
    /// of the unified backend.
    pub fn build(&self) -> Result<BackendHandles, String> {
        // The registry starts from lance-io's defaults: their "file" provider
        // (optimized local code paths — see module doc) plus their default
        // "memory" provider, which we immediately replace below.
        let registry = ObjectStoreRegistry::default();
        let catalog: Arc<dyn object_store::ObjectStore> = match self {
            Backend::File { root } => Arc::new(
                object_store::local::LocalFileSystem::new_with_prefix(root)
                    .map_err(|e| e.to_string())?,
            ),
            Backend::Memory => {
                let mem = Arc::new(object_store::memory::InMemory::new());
                registry.insert(
                    "memory",
                    Arc::new(SharedMemoryProvider { inner: mem.clone() }),
                );
                mem
            }
            Backend::S3(cfg) => {
                registry.insert("s3", Arc::new(StaticS3Provider));
                cfg.build_catalog_store().map_err(|e| e.to_string())?
            }
        };
        // Cache sizes mirror lance::Session::default() (6GiB index / 1GiB
        // metadata budget as *cache budget*, not RSS cap).
        let session = Arc::new(Session::new(
            6 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
            Arc::new(registry),
        ));
        Ok(BackendHandles { catalog, session })
    }
}

/// Create + canonicalize a file-backend root (see `Backend::parse`).
fn file_root(p: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(p)
        .map_err(|e| format!("cannot create data dir {}: {e}", p.display()))?;
    std::fs::canonicalize(p).map_err(|e| format!("cannot canonicalize {}: {e}", p.display()))
}

/// The two handles every backend yields (see [`Backend::build`]).
pub struct BackendHandles {
    /// Object store for catalog JSON + LOCK objects.
    pub catalog: Arc<dyn object_store::ObjectStore>,
    /// lancedb session whose registry routes dataset URIs.
    pub session: Arc<Session>,
}

// ---------------------------------------------------------------------------
// catalog JSON primitives (backend-agnostic, used by store.rs and lock.rs)
// ---------------------------------------------------------------------------

/// Read + parse a JSON object; NotFound maps to Ok(None) (wire: 404).
pub async fn read_json<T: serde::de::DeserializeOwned>(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
) -> Result<Option<T>, String> {
    use object_store::ObjectStoreExt as _;
    match store.get(key).await {
        Ok(res) => {
            let bytes = res.bytes().await.map_err(|e| e.to_string())?;
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("corrupt object {}: {e}", key.as_ref()))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(format!("read {}: {e}", key.as_ref())),
    }
}

/// Conditional create (`PutMode::Create`); Ok(false) = already exists.
/// This is THE race-free create primitive shared by catalog objects, the LOCK
/// object and (on s3) Lance manifest commits (design.md §5.2).
pub async fn put_json_create(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
    json: &[u8],
) -> Result<bool, String> {
    use object_store::{PutMode, PutOptions};
    match store
        .put_opts(
            key,
            json.to_vec().into(),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
        Err(e) => Err(format!("create {}: {e}", key.as_ref())),
    }
}

/// Unconditional overwrite (`PutMode::Overwrite`).
pub async fn put_json_overwrite(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
    json: &[u8],
) -> Result<(), String> {
    use object_store::{PutMode, PutOptions};
    store
        .put_opts(
            key,
            json.to_vec().into(),
            PutOptions {
                mode: PutMode::Overwrite,
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("overwrite {}: {e}", key.as_ref()))
}

/// Read raw bytes + e_tag (for CAS sequences — LOCK takeover/heartbeat).
pub async fn get_with_etag(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
) -> Result<Option<(bytes::Bytes, String)>, String> {
    use object_store::ObjectStoreExt as _;
    match store.get(key).await {
        Ok(res) => {
            let etag = res.meta.e_tag.clone().unwrap_or_default();
            let bytes = res.bytes().await.map_err(|e| e.to_string())?;
            Ok(Some((bytes, etag)))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(format!("read {}: {e}", key.as_ref())),
    }
}

/// etag-guarded conditional overwrite (`PutMode::Update`). Ok(false) = etag
/// mismatch (someone else won the race). NOT supported by LocalFileSystem —
/// callers on the file backend must use the delete+create fallback (lock.rs).
pub async fn put_json_cas(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
    json: &[u8],
    etag: &str,
) -> Result<bool, String> {
    use object_store::{PutMode, PutOptions, UpdateVersion};
    match store
        .put_opts(
            key,
            json.to_vec().into(),
            PutOptions {
                mode: PutMode::Update(UpdateVersion {
                    e_tag: Some(etag.to_string()),
                    version: None,
                }),
                ..Default::default()
            },
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(object_store::Error::Precondition { .. }) => Ok(false),
        Err(e) => Err(format!("cas-write {}: {e}", key.as_ref())),
    }
}

/// Best-effort delete.
pub async fn delete_object(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
) -> Result<(), String> {
    use object_store::ObjectStoreExt as _;
    store
        .delete(key)
        .await
        .map(|_| ())
        .map_err(|e| format!("delete {}: {e}", key.as_ref()))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_specs() {
        let dd = tempfile::tempdir().unwrap();
        let dd = dd.path();
        assert!(matches!(
            Backend::parse("file", dd).unwrap(),
            Backend::File { root } if root == dd.canonicalize().unwrap()
        ));
        let other = tempfile::tempdir().unwrap();
        assert!(matches!(
            Backend::parse(&format!("file:{}", other.path().display()), dd).unwrap(),
            Backend::File { root } if root == other.path().canonicalize().unwrap()
        ));
        assert!(matches!(
            Backend::parse("memory", dd).unwrap(),
            Backend::Memory
        ));
        let Backend::S3(cfg) = Backend::parse("s3+http://h:9000/b/p", dd).unwrap() else {
            panic!("s3");
        };
        assert_eq!(cfg.endpoint, "http://h:9000");
        assert_eq!(cfg.bucket, "b");
        assert_eq!(cfg.prefix, "p");
        assert!(Backend::parse("ftp://x", dd).is_err());
        assert!(Backend::parse("s3+http://h:9000", dd).is_err()); // no bucket
    }

    #[test]
    fn backend_uris_and_keys() {
        let dd = tempfile::tempdir().unwrap();
        let f = Backend::parse("file", dd.path()).unwrap();
        let abs = format!("{}", dd.path().canonicalize().unwrap().display());
        assert_eq!(
            f.lance_uri("vb", "ix"),
            format!("file://{abs}/buckets/vb/indexes/ix/data")
        );
        assert_eq!(f.object_key("LOCK").as_ref(), "LOCK");
        let m = Backend::parse("memory", dd.path()).unwrap();
        assert_eq!(
            m.lance_uri("vb", "ix"),
            "memory://vf/buckets/vb/indexes/ix/data"
        );
        let s = Backend::parse("s3+http://h:9000/b/pre", dd.path()).unwrap();
        assert_eq!(
            s.lance_uri("vb", "ix"),
            "s3://b/pre/buckets/vb/indexes/ix/data"
        );
        assert_eq!(s.object_key("LOCK").as_ref(), "pre/LOCK");
    }

    #[tokio::test]
    async fn catalog_primitives_roundtrip_and_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let handles = Backend::File {
            root: dir.path().to_path_buf(),
        }
        .build()
        .unwrap();
        let key = object_store::path::Path::from("x/y.json");

        assert_eq!(
            read_json::<serde_json::Value>(&*handles.catalog, &key)
                .await
                .unwrap(),
            None
        );
        assert!(
            put_json_create(&*handles.catalog, &key, br#"{"a":1}"#)
                .await
                .unwrap()
        );
        assert!(
            !put_json_create(&*handles.catalog, &key, br#"{"a":2}"#)
                .await
                .unwrap(),
            "second conditional create must lose"
        );
        let v: serde_json::Value = read_json(&*handles.catalog, &key).await.unwrap().unwrap();
        assert_eq!(v["a"], 1);
        put_json_overwrite(&*handles.catalog, &key, br#"{"a":3}"#)
            .await
            .unwrap();
        let v: serde_json::Value = read_json(&*handles.catalog, &key).await.unwrap().unwrap();
        assert_eq!(v["a"], 3);
        delete_object(&*handles.catalog, &key).await.unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&*handles.catalog, &key)
                .await
                .unwrap(),
            None
        );
    }

    /// CAS semantics on the memory store (the S3 path uses the same
    /// If-Match primitive and needs a live server — see experiments.rs).
    #[tokio::test]
    async fn cas_overwrite_memory() {
        let handles = Backend::Memory.build().unwrap();
        let key = object_store::path::Path::from("cas.json");
        put_json_create(&*handles.catalog, &key, b"v1")
            .await
            .unwrap();
        let (_, etag) = get_with_etag(&*handles.catalog, &key)
            .await
            .unwrap()
            .unwrap();
        assert!(
            put_json_cas(&*handles.catalog, &key, b"v2", &etag)
                .await
                .unwrap(),
            "matching etag must win"
        );
        assert!(
            !put_json_cas(&*handles.catalog, &key, b"v3", "\"stale\"")
                .await
                .unwrap(),
            "stale etag must lose"
        );
        let (bytes, _) = get_with_etag(&*handles.catalog, &key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&bytes[..], b"v2");
    }
}
