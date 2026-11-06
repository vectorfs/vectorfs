//! Unified storage backend abstraction (design-v3.md §1/§3-R1): one
//! `Backend` enum for `file` / `memory` / `s3`, addressing a BUCKET PAIR per
//! vector bucket — source bucket `X` (pure user namespace) + system bucket
//! `X-vf` (vectorfs-owned, root IS the system area, D2). The backend spec
//! is ENDPOINT-shaped (no bucket/prefix): buckets are addressed per vector
//! bucket through [`Storage`] (store.rs), which resolves
//! `system_bucket_name(vb, bucket_suffix)` and opens one object-store handle
//! per bucket.
//!
//! Per-bucket stores:
//! - file: `<data-dir>/X/` + `<data-dir>/X-vf/` — prefix-rooted
//!   `LocalFileSystem` each (the data dir also holds the daemon's local
//!   home `<data-dir>/.vectorfs/`, see [`SYSTEM_PREFIX`]);
//! - memory: ONE shared `InMemory` key space for the whole daemon; bucket
//!   handles are `PrefixStore` views rooted at `X` / `X-vf`, so bucket
//!   relative key sets are byte-identical across backends (§5.3-5);
//! - s3: one `AmazonS3` client per physical bucket (endpoint + creds come
//!   from the endpoint-shaped backend config).
//!
//! Lance datasets live under `indexes/<ix>/data` INSIDE the system bucket;
//! the memory URI's host segment IS the bucket name
//! (`memory://docs-vf/indexes/…` ⇒ map key exactly `docs-vf/indexes/…` —
//! the `vf/` mirror prefix of v2.2 is deleted, §4 memory rework).
//!
//! Injection route (M0-verified, zero AWS SDK; unchanged from v2.2):
//! object_store's own S3 client wrapped into lance-io providers on an
//! `ObjectStoreRegistry` inside a custom `lance::Session`, handed to
//! `lancedb::ConnectBuilder::session()`. Lance manifest commits on the
//! "s3" scheme use `ConditionalPutCommitHandler` = `PutMode::Create`.
//!
//! Raw bucket ADMIN (create/head/delete/list-buckets) has no object_store
//! API — that is `s3admin.rs` (design-v3.md §3-R8); the file/memory halves
//! of the same four verbs are local operations implemented in store.rs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use lance_io::object_store::providers::ObjectStoreProvider;
use lance_io::object_store::{ObjectStore, ObjectStoreParams, ObjectStoreRegistry};
use lancedb::Session;
use url::Url;

/// Cloud block size lance's own s3 provider uses (64KB; const is private there).
const CLOUD_BLOCK_SIZE: usize = 64 * 1024;

/// Reserved name of the daemon's LOCAL home directory
/// `<data-dir>/.vectorfs/` (design-v3.md §4): vectorfs.toml, models cache,
/// flock. v3 RETIRES it from object-key paths — system objects are
/// bucket-root-relative inside `X-vf`; only these three local-home uses
/// remain (config.rs, pipeline/models.rs, [`Backend::flock_path`]).
pub const SYSTEM_PREFIX: &str = ".vectorfs";

/// Default `bucket_suffix` (design-v3.md §0-D3): vb `X` ⇔ source bucket `X`
/// + system bucket `X-vf`.
pub const DEFAULT_BUCKET_SUFFIX: &str = "-vf";

/// Physical system bucket name of vector bucket `vb` (R1):
/// `X` + `bucket_suffix`.
pub fn system_bucket_name(vb: &str, suffix: &str) -> String {
    format!("{vb}{suffix}")
}

/// Validate a configured `bucket_suffix` (R6): lowercase/digits/hyphens only,
/// length 2..=16, MUST end alphanumeric, and a LEADING HYPHEN IS ALLOWED.
/// Note: the spec's literal regex `^[a-z0-9][a-z0-9-]*[a-z0-9]$` rejects its
/// own default `-vf` (first char `-`) — the default being normative, the
/// leading class is widened to `[a-z0-9-]` (registered deviation, final
/// report). Ending alphanumeric keeps every concatenated system-bucket name
/// (`X` + suffix) bucket-legal. `_` and `.` stay illegal — not portable
/// across AWS S3/R2/MinIO and default Ceph (§2 fact table).
pub fn validate_bucket_suffix(suffix: &str) -> Result<(), String> {
    let ok = (2..=16).contains(&suffix.len())
        && suffix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && suffix
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bucket_suffix {suffix:?} is invalid: must be 2-16 characters, lowercase letters, \
             digits and hyphens (a leading hyphen is fine — the \"-vf\" default has one), \
             ending with an alphanumeric character (_ and . are not portable across S3 \
             implementations and are rejected)"
        ))
    }
}

/// Validate a vector bucket NAME for a given suffix (R1/D5): class-S3
/// charset without dots, length 3..=63−len(suffix), and NOT ending with the
/// suffix itself (prevents `X-vf-vf` recursion ambiguity; AWS allows such
/// names — registered deviation, design-v3.md §7).
pub fn validate_vector_bucket_name(name: &str, suffix: &str) -> Result<(), String> {
    let max = 63usize.saturating_sub(suffix.len());
    let charset = (3..=max).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let ok = charset
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && !name.ends_with(suffix);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "vectorBucketName {name:?} is invalid: must be 3-{max} characters, lowercase \
             letters, digits and hyphens, starting and ending with an alphanumeric character, \
             and must not end with the configured bucket_suffix {suffix:?}"
        ))
    }
}

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

        // Cloudflare R2 rejects variable-size upload parts (only the LAST part
        // may differ) — Lance multipart writes must use constant part sizes
        // there (M0 REPORT 必答2 matrix). Detected from the endpoint host;
        // force-enable via `use_constant_size_upload_parts=true` storage option.
        let constant_parts = opts
            .get("endpoint")
            .is_some_and(|e| e.contains("r2.cloudflarestorage.com"))
            || opts
                .get("use_constant_size_upload_parts")
                .is_some_and(|v| v == "true" || v == "1");

        // Public constructor (struct literals are not possible from outside
        // lance-io: private fields). NOTE: it derives the metrics prefix from a
        // default registry that has no "s3" provider without lance-io's aws
        // feature, so it logs a guess warning — value is identical ("s3$bucket").
        Ok(ObjectStore::new(
            store,
            base_path.clone(),
            Some(CLOUD_BLOCK_SIZE),
            None,
            constant_parts,
            true, // list_is_lexically_ordered (true for object stores)
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

/// Parsed `--backend s3+http(s)://host:port` configuration (design-v3.md
/// §3-R6: ENDPOINT-shaped — no bucket, no prefix; buckets are addressed per
/// vector bucket). Bucket management (create/head/delete/list) is
/// [`crate::s3admin::S3Admin`], built from the same config.
#[derive(Debug, Clone)]
pub struct S3BackendConfig {
    pub endpoint: String,
    pub region: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
}

impl S3BackendConfig {
    /// Parse `s3+<endpoint-url>` — scheme http/https, authority host:port,
    /// and NO path (a bucket path segment is the retired v2.2 form: None
    /// here, which `Backend::parse` turns into the v3-shape error).
    pub fn parse(spec: &str) -> Option<Self> {
        let rest = spec.strip_prefix("s3+")?;
        let url = Url::parse(rest).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        if !url.path().is_empty() && url.path() != "/" {
            return None; // bucket-path form (v2.2) — rejected, endpoint only
        }
        let endpoint = format!("{}://{}", url.scheme(), url.authority());
        Some(Self {
            endpoint,
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

    /// Direct object_store client for ONE physical bucket (catalog JSON,
    /// source objects — everything except Lance datasets, which route
    /// through the session registry).
    pub fn build_bucket_store(
        &self,
        bucket: &str,
    ) -> object_store::Result<Arc<dyn object_store::ObjectStore>> {
        use object_store::aws::AmazonS3Builder;
        let mut builder = AmazonS3Builder::new()
            .with_url(format!("s3://{bucket}"))
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
/// of the owning `Backend` (see module doc). The URI's host segment is the
/// BUCKET NAME (v3): `memory://docs-vf/indexes/…` resolves to the map key
/// `docs-vf/indexes/…` — the same key the bucket's PrefixStore handle
/// produces, so catalog and engine share one coherent key space per bucket.
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

    /// Mirrors lance-io's own memory provider: the URL domain (here: the
    /// bucket name) plus the path form the object key.
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

// ---------------------------------------------------------------------------
// Backend enum
// ---------------------------------------------------------------------------

/// A configured storage backend. Everything above this type (catalog, engine,
/// LOCK) is backend-agnostic.
#[derive(Debug, Clone)]
pub enum Backend {
    /// Local directory root (default) — the data dir doubles as the bucket
    /// root: `X/`, `X-vf/` and the daemon home `.vectorfs/` are siblings.
    /// `root` is canonicalized to an absolute path so `file://` engine URIs
    /// parse.
    File { root: PathBuf },
    /// Volatile process-local store (CI / emulator mode).
    Memory,
    /// S3-compatible remote via `s3+http(s)://host:port` (endpoint-shaped,
    /// design-v3.md R6).
    S3(Box<S3BackendConfig>),
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::File { root } => write!(f, "file:{}", root.display()),
            Backend::Memory => write!(f, "memory"),
            Backend::S3(cfg) => write!(f, "s3:{}", cfg.endpoint),
        }
    }
}

impl Backend {
    /// Parse a `--backend` spec (design-v3.md §3-R6). Three forms:
    /// - `file` → the `--data-dir` directory (spike-era `file:<dir>` also
    ///   accepted for compatibility),
    /// - `memory` → volatile store,
    /// - `s3+http(s)://host:port` → S3-compatible endpoint. A BUCKET PATH
    ///   segment (`s3+http://h/bucket`) is the retired v2.2 form and fails
    ///   fast naming the v3 shape.
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
            _ if spec.starts_with("s3+") => {
                if S3BackendConfig::parse(spec).is_none() {
                    return Err(format!(
                        "invalid s3 backend {spec:?} — v3 backend specs are ENDPOINT-shaped: \
                         s3+http://host:port (buckets are addressed per vector bucket; \
                         s3+http://host:port/bucket was the v2.2 form and is no longer accepted)"
                    ));
                }
                S3BackendConfig::parse(spec)
                    .map(|cfg| Backend::S3(Box::new(cfg)))
                    .ok_or_else(|| format!("invalid s3 backend {spec:?}"))
            }
            other => Err(format!(
                "cannot parse --backend {other:?} \
                 (expected file | memory | s3+http://host:port)"
            )),
        }
    }

    /// Lance dataset URI for one index's data dir inside the SYSTEM bucket
    /// `sys_bucket` (e.g. `docs-vf`). Memory's host segment IS the bucket
    /// name so the dataset lands on the shared map at exactly the
    /// bucket-relative key (§4 memory rework, §5.3-5 isomorphism).
    pub fn lance_uri(&self, sys_bucket: &str, ix: &str) -> String {
        match self {
            Backend::File { root } => {
                format!("file://{}/{sys_bucket}/indexes/{ix}/data", root.display())
            }
            Backend::Memory => format!("memory://{sys_bucket}/indexes/{ix}/data"),
            Backend::S3(_) => format!("s3://{sys_bucket}/indexes/{ix}/data"),
        }
    }

    /// Deployment identity endpoint component (design-v3.md §0-D8): the
    /// placement a daemon serves. Ownership markers compare this plus
    /// region+account — NOT the daemon instance UUID.
    pub fn endpoint_id(&self) -> String {
        match self {
            Backend::File { root } => format!("file://{}", root.display()),
            Backend::Memory => "memory".to_string(),
            Backend::S3(cfg) => cfg.endpoint.clone(),
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
    /// guard, locks the daemon's LOCAL home; see lock.rs): always
    /// `<data-dir>/.vectorfs/flock` (`root` == the data dir on this
    /// backend). A separate file, NOT the LOCK object: the LOCK object is
    /// rewritten by put/delete, which would swap the inode out from under
    /// the flock.
    pub fn flock_path(&self) -> Option<std::path::PathBuf> {
        match self {
            Backend::File { root } => Some(root.join(SYSTEM_PREFIX).join("flock")),
            _ => None,
        }
    }

    /// Physical directory of `bucket` under a file root (helper for
    /// existence probes; the store constructors below materialize it).
    pub fn file_bucket_root(&self, bucket: &str) -> Option<PathBuf> {
        match self {
            Backend::File { root } => Some(root.join(bucket)),
            _ => None,
        }
    }

    /// Build the lancedb `Session` whose registry serves engine datasets for
    /// every bucket, plus (memory backend) the ONE shared `InMemory` map
    /// that all per-bucket PrefixStore handles view (§3-R1).
    pub fn build(&self) -> Result<BackendHandles, String> {
        // The registry starts from lance-io's defaults: their "file" provider
        // (optimized local code paths — see module doc) plus their default
        // "memory" provider, which we immediately replace below.
        let registry = ObjectStoreRegistry::default();
        let memory = match self {
            Backend::Memory => {
                let mem = Arc::new(object_store::memory::InMemory::new());
                registry.insert(
                    "memory",
                    Arc::new(SharedMemoryProvider { inner: mem.clone() }),
                );
                Some(mem)
            }
            Backend::S3(_) => {
                registry.insert("s3", Arc::new(StaticS3Provider));
                None
            }
            Backend::File { .. } => None,
        };
        // Cache sizes mirror lance::Session::default() (6GiB index / 1GiB
        // metadata budget as *cache budget*, not RSS cap).
        let session = Arc::new(Session::new(
            6 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
            Arc::new(registry),
        ));
        Ok(BackendHandles { session, memory })
    }
}

/// Open the object-store handle for ONE physical bucket (design-v3.md §3-R1).
///
/// - file: `<root>/<bucket>/` is created (materializing the directory IS
///   creating the bucket on this backend) and prefix-rooted LocalFileSystem
///   is returned;
/// - memory: a `PrefixStore` view over the shared map — keys are
///   bucket-relative, the shared key space carries `<bucket>/…`;
/// - s3: a per-bucket `AmazonS3` client. The bucket is NOT created here —
///   creation is the R3 lifecycle (store.rs / s3admin.rs).
pub fn open_bucket_store(
    backend: &Backend,
    bucket: &str,
    memory: Option<&Arc<object_store::memory::InMemory>>,
) -> Result<Arc<dyn object_store::ObjectStore>, String> {
    match backend {
        Backend::File { root } => {
            let dir = root.join(bucket);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("create bucket dir {}: {e}", dir.display()))?;
            Ok(Arc::new(
                object_store::local::LocalFileSystem::new_with_prefix(&dir)
                    .map_err(|e| e.to_string())?,
            ))
        }
        Backend::Memory => {
            let mem = memory.ok_or_else(|| {
                "memory bucket store requested before the shared map was built".to_string()
            })?;
            Ok(Arc::new(object_store::prefix::PrefixStore::new(
                mem.clone(),
                object_store::path::Path::from(bucket),
            )))
        }
        Backend::S3(cfg) => cfg
            .build_bucket_store(bucket)
            .map_err(|e| format!("open s3 bucket {bucket}: {e}")),
    }
}

/// Create + canonicalize a file-backend root (see `Backend::parse`).
fn file_root(p: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(p)
        .map_err(|e| format!("cannot create data dir {}: {e}", p.display()))?;
    std::fs::canonicalize(p).map_err(|e| format!("cannot canonicalize {}: {e}", p.display()))
}

/// What [`Backend::build`] yields: the engine session shared by every
/// bucket, plus the memory backend's single shared map.
pub struct BackendHandles {
    /// lancedb session whose registry routes dataset URIs.
    pub session: Arc<Session>,
    /// The memory backend's ONE shared `InMemory` (None on file/s3).
    pub memory: Option<Arc<object_store::memory::InMemory>>,
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
/// This is THE race-free create primitive shared by catalog objects and the
/// LOCK object (design-v3.md §5.1 — same mechanism, granularity = bucket).
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

/// Best-effort delete; a missing object is success (idempotent delete —
/// LocalFileSystem's raw `delete` errors on ENOENT, S3/InMemory do not).
pub async fn delete_object(
    store: &dyn object_store::ObjectStore,
    key: &object_store::path::Path,
) -> Result<(), String> {
    use object_store::ObjectStoreExt as _;
    match store.delete(key).await {
        Ok(()) => Ok(()),
        Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(format!("delete {}: {e}", key.as_ref())),
    }
}

/// List all object keys under a prefix (bucket-relative, on one bucket's
/// store). Works on every backend (LocalFileSystem / InMemory / S3 all
/// implement `list`; on S3 this is a ListObjectsV2 prefix scan).
pub async fn list_prefix(
    store: &dyn object_store::ObjectStore,
    prefix: &object_store::path::Path,
) -> Result<Vec<object_store::path::Path>, String> {
    use futures_util::StreamExt;
    let mut stream = store.list(Some(prefix));
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|e| format!("list {}: {e}", prefix.as_ref()))?;
        out.push(meta.location);
    }
    out.sort();
    Ok(out)
}

/// List EVERY key of a store (no prefix) — the memory backend's R9
/// enumeration reads the whole shared key space this way.
pub async fn list_all(
    store: &dyn object_store::ObjectStore,
) -> Result<Vec<object_store::path::Path>, String> {
    use futures_util::StreamExt;
    let mut stream = store.list(None);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|e| format!("list: {e}"))?;
        out.push(meta.location);
    }
    out.sort();
    Ok(out)
}

/// (object count, total byte size) under a prefix — `vectorfs status`
/// volume accounting (M1 T6), now per bucket (design-v3.md §6 status row).
/// Sizes come from list metadata only; no object body is ever fetched.
pub async fn prefix_stats(
    store: &dyn object_store::ObjectStore,
    prefix: Option<&object_store::path::Path>,
) -> Result<(usize, u64), String> {
    use futures_util::StreamExt;
    let mut stream = store.list(prefix);
    let (mut objects, mut bytes) = (0usize, 0u64);
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|e| {
            format!(
                "list {}: {e}",
                prefix.map(|p| p.as_ref().to_string()).unwrap_or_default()
            )
        })?;
        objects += 1;
        bytes += meta.size;
    }
    Ok((objects, bytes))
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
        let Backend::S3(cfg) = Backend::parse("s3+http://h:9000", dd).unwrap() else {
            panic!("s3");
        };
        assert_eq!(cfg.endpoint, "http://h:9000");
        // v2.2 bucket-path form fails fast naming the v3 shape
        let err = Backend::parse("s3+http://h:9000/b/p", dd).unwrap_err();
        assert!(err.contains("ENDPOINT-shaped"), "{err}");
        assert!(Backend::parse("ftp://x", dd).is_err());
    }

    #[test]
    fn backend_uris_and_names() {
        let dd = tempfile::tempdir().unwrap();
        let f = Backend::parse("file", dd.path()).unwrap();
        let abs = format!("{}", dd.path().canonicalize().unwrap().display());
        // system bucket names are spelled by the caller (store.rs maps
        // vb -> X-vf via bucket_suffix); the URI host/path below must make
        // dataset keys land bucket-relative
        assert_eq!(
            f.lance_uri("docs-vf", "ix"),
            format!("file://{abs}/docs-vf/indexes/ix/data")
        );
        assert_eq!(
            f.flock_path().unwrap(),
            dd.path().canonicalize().unwrap().join(".vectorfs/flock")
        );
        assert_eq!(f.endpoint_id(), format!("file://{abs}"));
        let m = Backend::parse("memory", dd.path()).unwrap();
        assert_eq!(
            m.lance_uri("docs-vf", "ix"),
            "memory://docs-vf/indexes/ix/data"
        );
        assert_eq!(m.endpoint_id(), "memory");
        let s = Backend::parse("s3+http://h:9000", dd.path()).unwrap();
        assert_eq!(s.lance_uri("docs-vf", "ix"), "s3://docs-vf/indexes/ix/data");
        assert_eq!(s.endpoint_id(), "http://h:9000");
    }

    /// bucket_suffix / vb-name gates (R1/R6/D5).
    #[test]
    fn suffix_and_vb_name_gates() {
        assert_eq!(DEFAULT_BUCKET_SUFFIX, "-vf");
        assert!(validate_bucket_suffix("-vf").is_ok());
        assert!(validate_bucket_suffix("x2").is_ok());
        assert!(validate_bucket_suffix("-system-x9").is_ok());
        assert!(
            validate_bucket_suffix("vf").is_ok(),
            "no leading hyphen needed"
        );
        for bad in ["", "v", "-vf-", "_x", "a.b", "-VF", "vf-", &"x".repeat(17)] {
            assert!(validate_bucket_suffix(bad).is_err(), "{bad:?}");
        }
        // vb names: charset, 3..=63-len(suffix), not suffix-tailed
        assert!(validate_vector_bucket_name("docs", "-vf").is_ok());
        assert!(validate_vector_bucket_name(&"d".repeat(60), "-vf").is_ok());
        assert!(validate_vector_bucket_name(&"d".repeat(61), "-vf").is_err());
        assert!(validate_vector_bucket_name("ab", "-vf").is_err());
        assert!(validate_vector_bucket_name("has.dot", "-vf").is_err());
        assert!(validate_vector_bucket_name("docs-vf", "-vf").is_err());
        assert!(validate_vector_bucket_name("-docs", "-vf").is_err());
        // shorter suffix widens the name budget: 61 legal at "-v" (63-2)
        assert!(validate_vector_bucket_name(&"d".repeat(61), "-v").is_ok());
        assert_eq!(system_bucket_name("docs", "-vf"), "docs-vf");
    }

    /// Per-bucket stores round-trip bucket-relative keys, and the memory
    /// shared key space carries the `<bucket>/` prefix exactly once
    /// (§5.3-5 isomorphism prerequisite).
    #[tokio::test]
    async fn per_bucket_stores_key_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let froot = Backend::File { root: root.clone() };
        let _ = froot.build().unwrap();
        let docs = open_bucket_store(&froot, "docs", None).unwrap();
        let sys = open_bucket_store(&froot, "docs-vf", None).unwrap();
        let key = object_store::path::Path::from("indexes/ix/index.json");
        assert!(put_json_create(&*sys, &key, b"{}").await.unwrap());
        assert!(
            read_json::<serde_json::Value>(&*docs, &key)
                .await
                .unwrap()
                .is_none(),
            "buckets are physically separate directories"
        );
        assert!(root.join("docs-vf/indexes/ix/index.json").is_file());
        assert!(!root.join("docs/indexes").exists());

        let m = Backend::Memory.build().unwrap();
        let mem = m.memory.as_ref().unwrap();
        let mdocs = open_bucket_store(&Backend::Memory, "docs", Some(mem)).unwrap();
        let msys = open_bucket_store(&Backend::Memory, "docs-vf", Some(mem)).unwrap();
        assert!(put_json_create(&*msys, &key, b"{\"a\":1}").await.unwrap());
        // the raw shared map key carries the bucket prefix exactly once and
        // is invisible to the SOURCE bucket's view
        let raw = object_store::path::Path::from("docs-vf/indexes/ix/index.json");
        assert_eq!(
            read_json::<serde_json::Value>(mem.as_ref(), &raw)
                .await
                .unwrap()
                .unwrap()["a"],
            1
        );
        assert!(
            read_json::<serde_json::Value>(&*mdocs, &key)
                .await
                .unwrap()
                .is_none(),
            "bucket views are disjoint key spaces"
        );
        // and a second system bucket is disjoint from the first
        let other = open_bucket_store(&Backend::Memory, "kb-vf", Some(mem)).unwrap();
        assert!(
            read_json::<serde_json::Value>(&*other, &key)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn catalog_primitives_roundtrip_and_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::File {
            root: dir.path().to_path_buf(),
        };
        let _ = backend.build().unwrap();
        let store = open_bucket_store(&backend, "vb-vf", None).unwrap();
        let key = object_store::path::Path::from("x/y.json");

        assert_eq!(
            read_json::<serde_json::Value>(&*store, &key).await.unwrap(),
            None
        );
        assert!(put_json_create(&*store, &key, br#"{"a":1}"#).await.unwrap());
        assert!(
            !put_json_create(&*store, &key, br#"{"a":2}"#).await.unwrap(),
            "second conditional create must lose"
        );
        let v: serde_json::Value = read_json(&*store, &key).await.unwrap().unwrap();
        assert_eq!(v["a"], 1);
        put_json_overwrite(&*store, &key, br#"{"a":3}"#)
            .await
            .unwrap();
        let v: serde_json::Value = read_json(&*store, &key).await.unwrap().unwrap();
        assert_eq!(v["a"], 3);
        delete_object(&*store, &key).await.unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&*store, &key).await.unwrap(),
            None
        );
    }

    /// CAS semantics on the memory store (the S3 path uses the same
    /// If-Match primitive and needs a live server — see experiments.rs).
    #[tokio::test]
    async fn cas_overwrite_memory() {
        let m = Backend::Memory.build().unwrap();
        let store = open_bucket_store(&Backend::Memory, "vb-vf", m.memory.as_ref()).unwrap();
        let key = object_store::path::Path::from("cas.json");
        put_json_create(&*store, &key, b"v1").await.unwrap();
        let (_, etag) = get_with_etag(&*store, &key).await.unwrap().unwrap();
        assert!(
            put_json_cas(&*store, &key, b"v2", &etag).await.unwrap(),
            "matching etag must win"
        );
        assert!(
            !put_json_cas(&*store, &key, b"v3", "\"stale\"")
                .await
                .unwrap(),
            "stale etag must lose"
        );
        let (bytes, _) = get_with_etag(&*store, &key).await.unwrap().unwrap();
        assert_eq!(&bytes[..], b"v2");
    }
}
