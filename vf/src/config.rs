//! `vectorfs init` credentials + `vectorfs serve` configuration resolution
//! (M1 T2; M2 T1 swaps the file carrier vf.json -> vectorfs.toml, design.md
//! §0 config ruling — no backward compatibility, direct switch).
//!
//! Resolution order (design.md §5.8): **flag > environment variable >
//! `<data-dir>/.vectorfs/vectorfs.toml` > default**. Everything the daemon
//! needs (auth creds, region, account, backend, data dir, port) flows through
//! [`resolve`], so there is exactly one precedence to reason (and test) about.
//!
//! `<data-dir>/.vectorfs/vectorfs.toml` shape (written by `vectorfs init`;
//! lives INSIDE the reserved system prefix, design.md §5.3):
//! ```toml
//! version = 1
//! region = "us-east-1"
//! account_id = "000000000000"
//! access_key_id = "VFAK…"
//! secret_access_key = "…"
//! ```
//! The file is PLAINTEXT on purpose: v0.1 is single-binary, single-machine,
//! single-tenant; the data root itself already holds everything worth
//! protecting (vectors + metadata in the clear), so encrypting only the keys
//! inside the same directory would be security theater. Revisit when a
//! multi-tenant / hosted deployment shape appears (e.g. OS keyring, or
//! encrypt-at-rest keyed by a passphrase).

use std::path::{Path, PathBuf};

/// Base62 alphabet for generated credentials.
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// File name of the server config, inside `.vectorfs/` (design.md §5.3).
pub const CONFIG_FILE: &str = "vectorfs.toml";

/// `<data-dir>/.vectorfs/vectorfs.toml` — the ONE place server config lives.
pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(crate::backends::SYSTEM_PREFIX)
        .join(CONFIG_FILE)
}

/// Contents of `<data-dir>/.vectorfs/vectorfs.toml`. Server-side fields
/// are M2 T1; the M2 T2 plugin sections ([extractor.*], [embedder.*],
/// [ingest.*], design.md §5.10.4) live in the SAME file and are declared
/// here only so `deny_unknown_fields` (which keeps guarding every OTHER
/// key) tolerates their presence — their strict parse, with TOML line
/// attribution, is `pipeline::config::load_pipeline_config` over the raw
/// text (spans need the source, a span-free `Table` cannot carry them).
/// `init` never writes them, so serialization stays byte-identical to T1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub version: u32,
    pub region: String,
    pub account_id: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// `[extractor.<name>]` sections (raw; parsed by pipeline::config).
    #[serde(default)]
    pub extractor: Option<toml::Table>,
    /// `[embedder.<name>]` sections (raw; parsed by pipeline::config).
    #[serde(default)]
    pub embedder: Option<toml::Table>,
    /// `[ingest.<bucket>]` sections (raw; parsed by pipeline::config).
    #[serde(default)]
    pub ingest: Option<toml::Table>,
}

/// Raw `vectorfs serve` flags — Option everywhere so "not given" stays
/// distinguishable from every other precedence layer.
#[derive(Debug, Default, Clone)]
pub struct ServeFlags {
    pub port: Option<u16>,
    pub data_dir: Option<PathBuf>,
    pub backend: Option<String>,
    pub no_auth: bool,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub region: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
    pub trace: bool,
}

/// Fully resolved daemon configuration.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub port: u16,
    pub data_dir: PathBuf,
    /// Resolved backend spec string (file|memory|s3+…|file:<dir>) BEFORE
    /// Backend::parse — kept for logging.
    pub backend_spec: String,
    pub creds: Option<crate::sigv4::Credentials>,
    pub no_auth: bool,
    /// Region clients must sign for; enters the SigV4 scope and ARNs.
    pub region: String,
    /// Account id used in ARNs (12 digits).
    pub account: String,
    pub trace: bool,
    /// Raw S3 backend credentials (backend connection, NOT SigV4 auth).
    pub s3_region: String,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
}

pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_DATA_DIR: &str = "./vectorfs-data";
pub const DEFAULT_REGION: &str = "us-east-1";
pub const DEFAULT_ACCOUNT: &str = "000000000000";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Test seam: same resolution with an injected environment (edition 2024
/// makes std::env::set_var unsafe, and process-global env would race the
/// parallel test runner).
pub fn resolve_with(
    flags: &ServeFlags,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedConfig, String> {
    resolve_impl(flags, &env)
}

/// The production entry: resolution against the real process environment.
pub fn resolve(flags: &ServeFlags) -> Result<ResolvedConfig, String> {
    resolve_impl(flags, &env)
}

/// Storage-only slice of the chain (flag > `VF_*` env > default), for the
/// inspection commands `vectorfs status` / `vectorfs doctor` (M1 T6): data dir,
/// backend
/// spec and S3 connection settings — deliberately NO credential requirement,
/// these commands read the data root and never serve the wire.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    /// Backend spec BEFORE `Backend::parse` (file|memory|s3+…).
    pub backend_spec: String,
    pub s3_region: String,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
}

impl ResolvedConfig {
    /// Parse the backend spec WITH the resolved S3 connection settings.
    /// CAUTION: calling `Backend::parse` directly on `backend_spec` (as
    /// serve did before 2026-11-04) silently drops the S3 credentials —
    /// requests go out unsigned (`SkipSignature=true`), which SeaweedFS's
    /// anonymous-admin setup tolerates but real clouds (R2: 400
    /// InvalidArgument "Authorization") reject.
    pub fn backend(&self) -> Result<crate::backends::Backend, String> {
        let mut backend = crate::backends::Backend::parse(&self.backend_spec, &self.data_dir)?;
        if let crate::backends::Backend::S3(cfg) = &mut backend {
            cfg.region = self.s3_region.clone();
            cfg.access_key = self.s3_access_key.clone();
            cfg.secret_key = self.s3_secret_key.clone();
        }
        Ok(backend)
    }
}

impl StorageConfig {
    /// Parse into a [`crate::backends::Backend`], injecting the S3
    /// connection settings. CAUTION: on the `file` backend `Backend::parse`
    /// CREATES + canonicalizes the data dir — callers that must not create
    /// it (`vectorfs status` / `vectorfs doctor` probing a possibly-missing root)
    /// check
    /// existence first.
    pub fn backend(&self) -> Result<crate::backends::Backend, String> {
        let mut backend = crate::backends::Backend::parse(&self.backend_spec, &self.data_dir)?;
        if let crate::backends::Backend::S3(cfg) = &mut backend {
            cfg.region = self.s3_region.clone();
            cfg.access_key = self.s3_access_key.clone();
            cfg.secret_key = self.s3_secret_key.clone();
        }
        Ok(backend)
    }

    /// True when the resolved backend stores data under the local data dir
    /// (i.e. the "no data root" early-exit of the inspection commands
    /// applies).
    pub fn is_file_backend(&self) -> bool {
        matches!(self.backend_spec.as_str(), "file") || self.backend_spec.starts_with("file:")
    }
}

/// Test seam, same as [`resolve_with`].
pub fn resolve_storage_with(
    flags: &ServeFlags,
    env: impl Fn(&str) -> Option<String>,
) -> Result<StorageConfig, String> {
    let env = &env as &dyn Fn(&str) -> Option<String>;
    let (s3_region, s3_access_key, s3_secret_key) = s3_conn_of(flags, env);
    Ok(StorageConfig {
        data_dir: data_dir_of(flags, env),
        backend_spec: backend_spec_of(flags, env),
        s3_region,
        s3_access_key,
        s3_secret_key,
    })
}

/// The production entry: storage-only resolution against the real environment.
pub fn resolve_storage(flags: &ServeFlags) -> Result<StorageConfig, String> {
    resolve_storage_with(flags, env)
}

/// Read `<dir>/.vectorfs/vectorfs.toml` if present. Missing file -> Ok(None);
/// present but unreadable/corrupt -> Err (fail closed: silently ignoring
/// credentials the operator thinks are active is worse than refusing to
/// start). Corrupt TOML carries the parse error verbatim — toml's de::Error
/// prints the offending line/column span, which is the fastest repair hint.
pub fn read_config(dir: &Path) -> Result<Option<FileConfig>, String> {
    let path = config_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s)
            .map(Some)
            .map_err(|e| format!("corrupt {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Data dir resolution: flag (`--data-dir`) > env (`VF_DATA_DIR`) > default
/// (`./vectorfs-data`). Shared by the daemon and inspection resolutions.
fn data_dir_of(flags: &ServeFlags, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    flags
        .data_dir
        .clone()
        .or_else(|| env("VF_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
}

/// Backend spec resolution: flag (`--backend`) > env (`VF_BACKEND`) > `file`.
/// vectorfs.toml never carries a backend — it is per-machine
/// credentials/config, the backend is per-invocation.
fn backend_spec_of(flags: &ServeFlags, env: &dyn Fn(&str) -> Option<String>) -> String {
    flags
        .backend
        .clone()
        .or_else(|| env("VF_BACKEND"))
        .unwrap_or_else(|| "file".to_string())
}

/// S3 backend connection settings: flag > `VF_S3_*` env > `AWS_*` env.
fn s3_conn_of(
    flags: &ServeFlags,
    env: &dyn Fn(&str) -> Option<String>,
) -> (String, Option<String>, Option<String>) {
    let region = flags
        .s3_region
        .clone()
        .or_else(|| env("VF_S3_REGION"))
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| DEFAULT_REGION.to_string());
    let access_key = flags
        .s3_access_key
        .clone()
        .or_else(|| env("VF_S3_ACCESS_KEY"))
        .or_else(|| env("AWS_ACCESS_KEY_ID"));
    let secret_key = flags
        .s3_secret_key
        .clone()
        .or_else(|| env("VF_S3_SECRET_KEY"))
        .or_else(|| env("AWS_SECRET_ACCESS_KEY"));
    (region, access_key, secret_key)
}

/// Resolve: flag > env (`VF_*`) > `<data-dir>/.vectorfs/vectorfs.toml` >
/// default.
///
/// The data dir itself resolves flag (`--data-dir`) > env (`VF_DATA_DIR`) >
/// default (`./vectorfs-data`) FIRST, because it decides where
/// vectorfs.toml is read from. The backend flag/env is resolved before the
/// file too (vectorfs.toml never carries a backend — it is per-machine
/// credentials/config, the backend is per-invocation).
fn resolve_impl(
    flags: &ServeFlags,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedConfig, String> {
    let data_dir = data_dir_of(flags, env);

    let file_cfg = read_config(&data_dir)?;

    // --no-auth is a flag-only, explicit opt-out (never inherited from env or
    // vectorfs.toml: an environment must not silently disable verification).
    let no_auth = flags.no_auth;

    let region = flags
        .region
        .clone()
        .or_else(|| env("VF_REGION"))
        .or_else(|| file_cfg.as_ref().map(|c| c.region.clone()))
        .unwrap_or_else(|| DEFAULT_REGION.to_string());

    let account = file_cfg
        .as_ref()
        .map(|c| c.account_id.clone())
        .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());

    let (access_key, secret_key) = (
        flags
            .access_key
            .clone()
            .or_else(|| env("VF_ACCESS_KEY"))
            .or_else(|| file_cfg.as_ref().map(|c| c.access_key_id.clone())),
        flags
            .secret_key
            .clone()
            .or_else(|| env("VF_SECRET_KEY"))
            .or_else(|| file_cfg.as_ref().map(|c| c.secret_access_key.clone())),
    );
    let creds = match (access_key, secret_key) {
        (Some(ak), Some(sk)) if !ak.is_empty() && !sk.is_empty() => {
            Some(crate::sigv4::Credentials {
                access_key: ak,
                secret_key: sk,
            })
        }
        _ => None,
    };
    if !no_auth && creds.is_none() {
        return Err(format!(
            "no credentials configured and --no-auth not given.\n\
             Fix one of:\n\
             \x20 1. run `vectorfs init --data-dir {}` to generate credentials, or\n\
             \x20 2. export VF_ACCESS_KEY / VF_SECRET_KEY, or pass --access-key/--secret-key, or\n\
             \x20 3. pass --no-auth (CI/emulator mode — anyone reachable can read/write).",
            data_dir.display()
        ));
    }

    let backend_spec = backend_spec_of(flags, env);

    let port = flags
        .port
        .or_else(|| env("VF_PORT").and_then(|p| p.parse().ok()))
        .unwrap_or(DEFAULT_PORT);

    // S3 backend connection config (shared helper).
    let (s3_region, s3_access_key, s3_secret_key) = s3_conn_of(flags, env);

    Ok(ResolvedConfig {
        port,
        data_dir,
        backend_spec,
        creds,
        no_auth,
        region,
        account,
        trace: flags.trace,
        s3_region,
        s3_access_key,
        s3_secret_key,
    })
}

// ---------------------------------------------------------------------------
// key generation (`vectorfs init`)
// ---------------------------------------------------------------------------

/// OS randomness (getrandom(2)); no rand dependency (design.md §5.7 keep the
/// tree lean — getrandom is already transitive).
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    buf
}

/// Rejection-free base62: one random byte -> four base62 chars (62*4 = 248
/// leaves 8 wasted values, so 255/256 of bytes are used; rejected bytes are
/// simply skipped — no modulo bias).
fn base62(len: usize) -> String {
    let need = len / 4 + 4;
    let mut out = String::with_capacity(len + 4);
    'outer: for b in random_bytes(need) {
        let mut v = b as u16;
        for _ in 0..4 {
            if v < 248 {
                out.push(BASE62[(v % 62) as usize] as char);
                if out.len() == len {
                    break 'outer;
                }
            }
            v /= 62;
        }
    }
    out
}

/// `VFAK` + 20 base62 chars — the AWS access-key-id shape clients expect
/// (16+ chars, [A-Z0-9]+, prefix tags the issuer).
pub fn generate_access_key() -> String {
    format!("VFAK{}", base62(20))
}

/// 40 base62 chars, ~238 bits of entropy.
pub fn generate_secret_key() -> String {
    base62(40)
}

/// Result of a successful `vectorfs init`.
pub struct InitOutcome {
    pub config: FileConfig,
    pub path: PathBuf,
    pub regenerated: bool,
}

/// `vectorfs init [--data-dir <dir>] [--force]`:
/// create the dir + `.vectorfs/`, generate credentials, write
/// vectorfs.toml. Refuses to overwrite an existing vectorfs.toml unless
/// `--force` (which regenerates credentials).
pub fn init(data_dir: &Path, force: bool) -> Result<InitOutcome, String> {
    // The config lives at <data-dir>/.vectorfs/vectorfs.toml — creating its
    // parent dir covers both levels in one shot.
    std::fs::create_dir_all(data_dir.join(crate::backends::SYSTEM_PREFIX)).map_err(|e| {
        format!(
            "cannot create {}: {e}",
            data_dir.join(crate::backends::SYSTEM_PREFIX).display()
        )
    })?;
    let path = config_path(data_dir);
    if path.exists() && !force {
        return Err(format!(
            "{} already exists (refusing to overwrite). \
             Use --force to regenerate credentials (existing clients using the \
             old key will stop authenticating).",
            path.display()
        ));
    }
    let config = FileConfig {
        version: 1,
        region: DEFAULT_REGION.to_string(),
        account_id: DEFAULT_ACCOUNT.to_string(),
        access_key_id: generate_access_key(),
        secret_access_key: generate_secret_key(),
        // init writes the pure-protocol server file; plugin sections are
        // the operator's (or a preset's) later additions.
        extractor: None,
        embedder: None,
        ingest: None,
    };
    let toml_doc = toml::to_string_pretty(&config)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    std::fs::write(&path, toml_doc).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(InitOutcome {
        config,
        path,
        regenerated: force,
    })
}

/// Success printout for `vectorfs init`: credentials, next command, SDK
/// endpoint pointers (the §3 five-minute promise: copy-paste from here to
/// first query).
pub fn init_report(outcome: &InitOutcome) -> String {
    let c = &outcome.config;
    // config sits one level down now (<dir>/.vectorfs/vectorfs.toml) — the
    // report still names the DATA DIR operators pass to --data-dir.
    let dir = outcome
        .path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."));
    format!(
        "{sep}vectorfs initialized{regen}\n\
         data dir : {dir}\n\
         config   : {path}\n\
         \n\
         access key id : {ak}\n\
         secret key    : {sk}   (stored in vectorfs.toml — see note below)\n\
         \n\
         Next: start the server\n\
         \x20   vectorfs serve --data-dir {dir}\n\
         \n\
         Point your SDK at the endpoint (http://127.0.0.1:8787):\n\
         \x20   boto3: client = boto3.client(\"s3vectors\", endpoint_url=\"http://127.0.0.1:8787\",\n\
         \x20           aws_access_key_id=\"{ak}\", aws_secret_access_key=\"{sk}\", region_name=\"{region}\")\n\
         \x20   Go:    cfg.BaseEndpoint = aws.String(\"http://127.0.0.1:8787\") // + static creds, region {region}\n\
         \n\
         note: vectorfs.toml holds the secret key in PLAINTEXT (v0.1\n\
         single-machine tradeoff; the data root already stores all vectors\n\
         unencrypted).\n{sep}",
        sep = "-".repeat(62),
        regen = if outcome.regenerated {
            " (credentials REGENERATED — old access key no longer works)"
        } else {
            ""
        },
        dir = dir.display(),
        path = outcome.path.display(),
        ak = c.access_key_id,
        sk = c.secret_access_key,
        region = c.region,
    )
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shapes() {
        let ak = generate_access_key();
        assert!(ak.starts_with("VFAK") && ak.len() == 24, "{ak}");
        assert!(ak[4..].bytes().all(|b| BASE62.contains(&b)));
        let sk = generate_secret_key();
        assert_eq!(sk.len(), 40);
        assert!(sk.bytes().all(|b| BASE62.contains(&b)));
        // distinct across calls (entropy actually flowing)
        assert_ne!(ak, generate_access_key());
        assert_ne!(sk, generate_secret_key());
    }

    #[test]
    fn init_writes_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let first = init(dir.path(), false).unwrap();
        assert!(!first.regenerated);
        let old_ak = first.config.access_key_id.clone();

        // the file lands INSIDE the system prefix (design.md §5.3)
        assert_eq!(first.path, config_path(dir.path()));
        assert!(dir.path().join(".vectorfs/vectorfs.toml").is_file());

        assert!(init(dir.path(), false).is_err(), "must refuse overwrite");

        let again = init(dir.path(), true).unwrap();
        assert!(again.regenerated);
        assert_ne!(again.config.access_key_id, old_ak);

        // round-trip through the reader serve uses
        let reread = read_config(dir.path()).unwrap().unwrap();
        assert_eq!(reread.access_key_id, again.config.access_key_id);
        assert_eq!(reread.version, 1);
        assert_eq!(reread.account_id, "000000000000");
    }

    /// Fail-fast contract of the TOML carrier: corrupt syntax must report the
    /// LINE (toml de::Error's span, passed through read_config verbatim), and
    /// unknown fields must be rejected (deny_unknown_fields) instead of
    /// silently dropping credentials.
    #[test]
    fn corrupt_toml_reports_span_and_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".vectorfs")).unwrap();
        let path = config_path(dir.path());

        // syntax/type error on line 3 -> error names line 3
        std::fs::write(
            &path,
            "version = 1\nregion = \"us-east-1\"\naccount_id = 1\n",
        )
        .unwrap();
        let err = read_config(dir.path()).unwrap_err();
        assert!(err.contains("corrupt"), "{err}");
        assert!(err.contains("line 3"), "{err}");

        // unknown key -> rejected, not ignored
        std::fs::write(&path, "version = 1\nbogus_key = \"x\"\n").unwrap();
        let err = read_config(dir.path()).unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
    }

    /// M2 T2: the plugin sections share the file — FileConfig must TOLERATE
    /// them (their strict parse is pipeline::config's job, with line
    /// attribution) while every other unknown key still fails.
    #[test]
    fn read_config_tolerates_plugin_sections_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".vectorfs")).unwrap();
        let path = config_path(dir.path());

        let base = "version = 1\nregion = \"us-east-1\"\naccount_id = \"000000000000\"\n\
                    access_key_id = \"VFAKx\"\nsecret_access_key = \"s\"\n";
        std::fs::write(
            &path,
            format!(
                "{base}\n[extractor.pdf]\nkind = \"txt\"\ndpi = 300\n\n\
                 [ingest.docs]\nchunk = {{ kind = \"token-window\", size = 512, overlap = 64 }}\n"
            ),
        )
        .unwrap();
        let cfg = read_config(dir.path()).unwrap().unwrap();
        assert!(cfg.extractor.is_some() && cfg.ingest.is_some() && cfg.embedder.is_none());

        // deny_unknown_fields still guards every non-plugin key
        std::fs::write(&path, format!("{base}\n[ingsetr.d]\nx = 1\n")).unwrap();
        let err = read_config(dir.path()).unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn init_report_mentions_everything() {
        let dir = tempfile::tempdir().unwrap();
        let out = init(dir.path(), false).unwrap();
        let report = init_report(&out);
        assert!(report.contains("vectorfs serve --data-dir"));
        assert!(report.contains(".vectorfs/vectorfs.toml"));
        assert!(report.contains("endpoint_url"));
        assert!(report.contains("BaseEndpoint"));
        assert!(report.contains(&out.config.access_key_id));
    }

    fn env_map<'a>(m: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| m.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    /// The one precedence to rule them all: flag > env > vectorfs.toml >
    /// default.
    #[test]
    fn resolution_order_flag_env_file_default() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), false).unwrap();
        let file_cfg = read_config(dir.path()).unwrap().unwrap();

        let mk_flags = || ServeFlags {
            port: Some(9999),
            data_dir: Some(dir.path().to_path_buf()),
            region: Some("flag-region".into()),
            access_key: Some("flag-ak".into()),
            secret_key: Some("flag-sk".into()),
            ..Default::default()
        };
        let env = env_map(&[
            ("VF_REGION", "env-region"),
            ("VF_ACCESS_KEY", "env-ak"),
            ("VF_SECRET_KEY", "env-sk"),
            ("VF_PORT", "7777"),
        ]);

        // flag beats env and vectorfs.toml
        let c = resolve_with(&mk_flags(), &env).unwrap();
        assert_eq!(c.region, "flag-region");
        assert_eq!(c.creds.as_ref().unwrap().access_key, "flag-ak");
        assert_eq!(c.port, 9999);

        // env beats vectorfs.toml (flags absent)
        let c = resolve_with(&ServeFlags::default(), &env).unwrap();
        assert_eq!(c.region, "env-region");
        assert_eq!(c.creds.as_ref().unwrap().access_key, "env-ak");
        assert_eq!(c.port, 7777);

        // vectorfs.toml beats defaults (env absent)
        let c = resolve_with(
            &ServeFlags {
                data_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        assert_eq!(c.region, file_cfg.region);
        assert_eq!(c.account, file_cfg.account_id);
        assert_eq!(
            c.creds.as_ref().unwrap().secret_key,
            file_cfg.secret_access_key
        );
        assert_eq!(c.port, DEFAULT_PORT);
        assert_eq!(c.backend_spec, "file");
    }

    #[test]
    fn resolution_backend_and_data_dir_env() {
        // creds via env so resolution succeeds without vectorfs.toml
        let env = env_map(&[
            ("VF_BACKEND", "s3+http://127.0.0.1:9000/bk"),
            ("VF_DATA_DIR", "/tmp/env-dd"),
            ("VF_ACCESS_KEY", "env-ak"),
            ("VF_SECRET_KEY", "env-sk"),
        ]);
        let c = resolve_with(&ServeFlags::default(), env).unwrap();
        assert_eq!(c.backend_spec, "s3+http://127.0.0.1:9000/bk");
        assert_eq!(c.data_dir, PathBuf::from("/tmp/env-dd"));

        // default data dir when nothing given
        let c = resolve_with(
            &ServeFlags::default(),
            env_map(&[("VF_ACCESS_KEY", "a"), ("VF_SECRET_KEY", "s")]),
        )
        .unwrap();
        assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert_eq!(c.backend_spec, "file");
    }

    #[test]
    fn resolution_no_creds_without_no_auth_is_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_with(
            &ServeFlags {
                data_dir: Some(dir.path().to_path_buf()), // no vectorfs.toml there
                ..Default::default()
            },
            |_| None,
        )
        .unwrap_err();
        assert!(err.contains("vectorfs init"), "{err}");
        assert!(err.contains("VF_ACCESS_KEY"), "{err}");
        assert!(err.contains("--no-auth"), "{err}");

        // --no-auth skips the credential requirement entirely
        let c = resolve_with(
            &ServeFlags {
                data_dir: Some(dir.path().to_path_buf()),
                no_auth: true,
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        assert!(c.creds.is_none());
        assert!(c.no_auth);
    }
}

#[test]
fn resolved_backend_carries_s3_credentials_regression() {
    // 2026-11-04 R2 incident: serve called Backend::parse directly, so S3
    // creds from env/flags never reached the backend -> unsigned requests
    // (tolerated by SeaweedFS anonymous-admin, rejected by R2 with 400
    // InvalidArgument "Authorization"). ResolvedConfig::backend() must
    // inject them.
    let dir = tempfile::tempdir().unwrap();
    let c = resolve_with(
        &ServeFlags {
            backend: Some("s3+https://h.example/b".into()),
            no_auth: true,
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
        |k| match k {
            "VF_S3_ACCESS_KEY" => Some("ak".into()),
            "VF_S3_SECRET_KEY" => Some("sk".into()),
            "VF_S3_REGION" => Some("auto".into()),
            _ => None,
        },
    )
    .unwrap();
    match c.backend().unwrap() {
        crate::backends::Backend::S3(cfg) => {
            assert_eq!(cfg.access_key.as_deref(), Some("ak"));
            assert_eq!(cfg.secret_key.as_deref(), Some("sk"));
            assert_eq!(cfg.region, "auto");
        }
        other => panic!("expected S3 backend, got {other:?}"),
    }
}
