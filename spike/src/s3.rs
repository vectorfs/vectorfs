//! Task D: S3-compatible backend injected WITHOUT the AWS SDK.
//!
//! Route (verified against local registry sources, see NOTES.md):
//! 1. `object_store` crate's own S3 client (`object_store::aws::AmazonS3`,
//!    reqwest + aws-lc-rs only — no aws-config chain).
//! 2. Wrapped in a custom `lance_io::object_store::ObjectStoreProvider`
//!    registered for scheme "s3" on an `ObjectStoreRegistry`.
//! 3. Registry goes into a custom `lance::Session`, handed to
//!    `lancedb::ConnectBuilder::session()`; storage options (endpoint, static
//!    creds, allow_http) flow via `ConnectBuilder::storage_options()` into
//!    `ObjectStoreParams::storage_options_accessor`, which `new_store` reads.
//! 4. Lance manifest commits on the "s3" scheme use
//!    `ConditionalPutCommitHandler` = `PutMode::Create` (If-None-Match:*).
//!
//! Catalog JSON (bucket.json / index.json) also goes through object_store with
//! `PutMode::Create` — same conditional-write primitive (design.md §5.0).

use std::collections::HashMap;
use std::sync::Arc;

use lance_io::object_store::providers::ObjectStoreProvider;
use lance_io::object_store::{ObjectStore, ObjectStoreParams};
use lancedb::{Session, Table};
use url::Url;

/// Cloud block size lance's own s3 provider uses (64KB; const is private there).
const CLOUD_BLOCK_SIZE: usize = 64 * 1024;

/// Provider that builds an `object_store::aws::AmazonS3` from the storage
/// options (endpoint / static creds / region / allow_http) carried in
/// `ObjectStoreParams`.
#[derive(Debug, Default)]
pub struct StaticS3Provider;

#[async_trait::async_trait]
impl ObjectStoreProvider for StaticS3Provider {
    async fn new_store(&self, base_path: Url, params: &ObjectStoreParams) -> lance::Result<ObjectStore> {
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
        let store: Arc<dyn object_store::ObjectStore> = Arc::new(builder.build().map_err(lance_err)?);

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

    /// Build the shared lancedb Session whose registry knows our s3 provider.
    pub fn build_session() -> Arc<Session> {
        // Cache sizes mirror lance::Session::default() (6GiB index / 1GiB
        // metadata budget as *cache budget*, not RSS cap).
        let registry = lance_io::object_store::ObjectStoreRegistry::default();
        registry.insert("s3", Arc::new(StaticS3Provider));
        Arc::new(Session::new(
            6 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
            Arc::new(registry),
        ))
    }

    /// Direct object_store client for catalog JSON (separate from lance).
    pub fn build_catalog_store(
        &self,
    ) -> object_store::Result<Arc<dyn object_store::ObjectStore>> {
        use object_store::aws::AmazonS3Builder;
        let mut builder = AmazonS3Builder::new()
            .with_url(format!("s3://{}", self.bucket))
            .with_config(object_store::aws::AmazonS3ConfigKey::Endpoint, &self.endpoint)
            .with_config(object_store::aws::AmazonS3ConfigKey::Region, &self.region)
            .with_allow_http(self.endpoint.starts_with("http://"));
        if let (Some(ak), Some(sk)) = (&self.access_key, &self.secret_key) {
            builder = builder
                .with_config(object_store::aws::AmazonS3ConfigKey::AccessKeyId, ak)
                .with_config(object_store::aws::AmazonS3ConfigKey::SecretAccessKey, sk);
        } else {
            builder = builder.with_config(
                object_store::aws::AmazonS3ConfigKey::SkipSignature,
                "true",
            );
        }
        Ok(Arc::new(builder.build()?))
    }

    /// Open a lance connection to an index root with the custom session.
    pub async fn connect(
        &self,
        session: &Arc<Session>,
        vb: &str,
        ix: &str,
    ) -> lancedb::Result<lancedb::Connection> {
        lancedb::connect(&self.lance_uri(vb, ix))
            .session(session.clone())
            .storage_options(self.storage_options())
            .execute()
            .await
    }
}

/// Open the `vectors` table of an index (S3 path).
pub async fn open_table(cfg: &S3BackendConfig, session: &Arc<Session>, vb: &str, ix: &str) -> Result<Table, String> {
    let conn = cfg.connect(session, vb, ix).await.map_err(|e| e.to_string())?;
    conn.open_table(crate::store::TABLE_NAME)
        .execute()
        .await
        .map_err(|e| e.to_string())
}
