//! `vectorfs init` credentials + `vectorfs serve` configuration resolution
//! (M1 T2; M2 T1 swaps the file carrier vf.json -> vectorfs.toml, design.md
//! §0 config ruling — no backward compatibility, direct switch; M2 T8 adds
//! preset materialization, §5.10.4 — pipeline templates live in
//! [`crate::preset`], this module keeps owning credentials + the file).
//!
//! Resolution order (design.md §5.8): **flag > environment variable >
//! `<data-dir>/.vectorfs/vectorfs.toml` > default**. Everything the daemon
//! needs (auth creds, region, account, backend, data dir, bind address,
//! port) flows through [`resolve`], so there is exactly one precedence to
//! reason (and test) about.
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

use crate::preset::Preset;

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
/// `init` writes them as TEXT through the preset templates (preset.rs) —
/// these typed fields stay None in the serialized SERVER part, which keeps
/// its T1 byte shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub version: u32,
    pub region: String,
    pub account_id: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// `bucket_suffix` (design-v3.md §3-R6/D3): vector bucket `X` ⇔ source
    /// bucket `X` + system bucket `X<suffix>`. Default `-vf`; file-only
    /// (no env/flag — a namespace identity constant, not a tuning knob);
    /// install-time constant (changing it switches namespaces — old system
    /// buckets vanish from enumeration until it changes back). Validated
    /// by [`crate::backends::validate_bucket_suffix`] at resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket_suffix: Option<String>,
    /// `[extractor.<name>]` sections (raw; parsed by pipeline::config).
    #[serde(default)]
    pub extractor: Option<toml::Table>,
    /// `[embedder.<name>]` sections (raw; parsed by pipeline::config).
    #[serde(default)]
    pub embedder: Option<toml::Table>,
    /// `[ingest.<bucket>]` sections (raw; parsed by pipeline::config).
    #[serde(default)]
    pub ingest: Option<toml::Table>,
    /// M2 T5: models cache root override, consumed by pipeline::models (the
    /// `<data-dir>/.vectorfs/models` default applies when absent). Tolerated
    /// here for the same reason as the sections above; never written by
    /// `init` (T1 files keep their byte shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<String>,
}

/// Raw `vectorfs serve` flags — Option everywhere so "not given" stays
/// distinguishable from every other precedence layer.
#[derive(Debug, Default, Clone)]
pub struct ServeFlags {
    pub port: Option<u16>,
    /// Bind address for the HTTP listener: `host` or `host:port`
    /// (`--listen` / `VF_LISTEN`). The port part, when present, WINS over
    /// `--port`/`VF_PORT` (it is the more specific spelling); the host part
    /// defaults to loopback — the daemon is a local data-root owner, and
    /// 0.0.0.0 is a CONTAINER decision (Dockerfile sets `VF_LISTEN=0.0.0.0`,
    /// §5.10.7, so `-p` port mapping reaches the process).
    pub listen: Option<String>,
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
    /// Host the HTTP listener binds to (from `--listen`/`VF_LISTEN`,
    /// default 127.0.0.1 — see [`ServeFlags::listen`]).
    pub bind_host: String,
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
    /// `bucket_suffix` resolved from vectorfs.toml (default `-vf`; R6 —
    /// file-only constant, validated at resolution).
    pub bucket_suffix: String,
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
    /// `bucket_suffix` from vectorfs.toml (default `-vf`, R6).
    pub bucket_suffix: String,
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

    /// Open the [`crate::store::Storage`] this config resolves to (backend
    /// + bucket_suffix, v3 R1/R6).
    pub fn storage(&self) -> Result<crate::store::Storage, String> {
        crate::store::Storage::new(self.backend()?, &self.bucket_suffix)
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

    /// Open the [`crate::store::Storage`] this config resolves to (backend
    /// + bucket_suffix, v3 R1/R6).
    pub fn storage(&self) -> Result<crate::store::Storage, String> {
        crate::store::Storage::new(self.backend()?, &self.bucket_suffix)
    }
}

/// Test seam, same as [`resolve_with`].
pub fn resolve_storage_with(
    flags: &ServeFlags,
    env: impl Fn(&str) -> Option<String>,
) -> Result<StorageConfig, String> {
    let env = &env as &dyn Fn(&str) -> Option<String>;
    let (s3_region, s3_access_key, s3_secret_key) = s3_conn_of(flags, env);
    let data_dir = data_dir_of(flags, env);
    let bucket_suffix = suffix_of(&data_dir)?;
    Ok(StorageConfig {
        data_dir,
        backend_spec: backend_spec_of(flags, env),
        s3_region,
        s3_access_key,
        s3_secret_key,
        bucket_suffix,
    })
}

/// `bucket_suffix` from `<data-dir>/.vectorfs/vectorfs.toml` (default
/// `-vf`). Missing file = default; a PRESENT but invalid suffix fails fast
/// (R6: it is a namespace identity constant — a typo silently renaming
/// every system bucket is the worse failure).
fn suffix_of(data_dir: &Path) -> Result<String, String> {
    match read_config(data_dir)? {
        None => Ok(crate::backends::DEFAULT_BUCKET_SUFFIX.to_string()),
        Some(c) => match c.bucket_suffix {
            None => Ok(crate::backends::DEFAULT_BUCKET_SUFFIX.to_string()),
            Some(s) => crate::backends::validate_bucket_suffix(&s)
                .map_err(|e| format!("corrupt {}: {e}", config_path(data_dir).display()))
                .map(|()| s),
        },
    }
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

/// Listener address resolution: flag (`--listen`) > env (`VF_LISTEN`) >
/// loopback. Accepts `host` or `host:port`; an embedded port is returned
/// separately (and wins over `--port`/`VF_PORT` — the more specific
/// spelling). IPv6 literals are deliberately NOT special-cased: brackets in
/// a bind spec are a niche this daemon does not serve, and silently
/// mis-splitting `[::1]:8787` would be worse than refusing to guess.
fn listen_of(flags: &ServeFlags, env: &dyn Fn(&str) -> Option<String>) -> (String, Option<u16>) {
    let raw = flags.listen.clone().or_else(|| env("VF_LISTEN"));
    match raw {
        None => ("127.0.0.1".to_string(), None),
        Some(spec) => match spec.rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(p) if !host.is_empty() => (host.to_string(), Some(p)),
                // trailing colon or non-numeric tail (e.g. an unbracketed
                // IPv6 literal): treat the WHOLE spec as the host, keep the
                // existing port chain
                _ => (spec.clone(), None),
            },
            None => (spec, None),
        },
    }
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

    let (bind_host, listen_port) = listen_of(flags, env);
    let port = listen_port
        .or(flags.port)
        .or_else(|| env("VF_PORT").and_then(|p| p.parse().ok()))
        .unwrap_or(DEFAULT_PORT);

    // S3 backend connection config (shared helper).
    let (s3_region, s3_access_key, s3_secret_key) = s3_conn_of(flags, env);

    // bucket_suffix (R6): file-only constant, validated fail-fast.
    let bucket_suffix = match file_cfg.as_ref().and_then(|c| c.bucket_suffix.clone()) {
        None => crate::backends::DEFAULT_BUCKET_SUFFIX.to_string(),
        Some(s) => {
            crate::backends::validate_bucket_suffix(&s)
                .map_err(|e| format!("corrupt {}: {e}", config_path(&data_dir).display()))?;
            s
        }
    };

    Ok(ResolvedConfig {
        port,
        bind_host,
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
        bucket_suffix,
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

/// `vectorfs init` knobs (M2-T8): the preset to materialize plus the two
/// REPORT-ONLY knobs `--listen` / `--backend` (they shape the printed
/// serve/env hints and are deliberately NOT written to vectorfs.toml — the
/// backend is per-invocation, §5.10.7 "env 覆盖收敛为标量项", and the bind
/// address is a serve-time flag). `models_dir` is the CONTAINER volume path
/// (§5.10.7): the model-carrying presets (text-rag, multimodal, image-search)
/// record it as the top-level `models_dir` key, air-gapped bakes it into the
/// embedder `path`; bare-metal init leaves it unset and the derived default
/// applies.
#[derive(Debug, Default, Clone)]
pub struct InitOptions {
    pub preset: Preset,
    /// Report-only endpoint text (`host` or `host:port`).
    pub listen: Option<String>,
    /// Report-only serve hint (backend spec).
    pub backend: Option<String>,
    /// Models cache override (container `/vectorfs/models` volume).
    pub models_dir: Option<PathBuf>,
}

/// Result of a successful `vectorfs init`.
pub struct InitOutcome {
    pub config: FileConfig,
    pub path: PathBuf,
    pub regenerated: bool,
    /// Which preset was materialized (drives the report + the caller's
    /// prefetch decision).
    pub preset: Preset,
    /// Resolved models cache root (explicit `--models-dir` or the
    /// `<data-dir>/.vectorfs/models` default) — where a text-rag prefetch
    /// lands / where air-gapped expects the model directory.
    pub models_dir: PathBuf,
    /// Client-facing endpoint for the printed env snippet (from
    /// `--listen`, default `http://127.0.0.1:8787`).
    pub endpoint: String,
    /// Report-only `--backend` echo for the printed serve command (never
    /// written to vectorfs.toml — backends are per-invocation).
    pub serve_hint_backend: Option<String>,
}

/// `vectorfs init [--data-dir <dir>] [--force]` — the pre-preset shape
/// (identical to `init_with` with the default `dev` preset): create the dir
/// + `.vectorfs/`, generate credentials, write vectorfs.toml. Refuses to
/// overwrite an existing vectorfs.toml unless `--force` (which regenerates
/// credentials).
pub fn init(data_dir: &Path, force: bool) -> Result<InitOutcome, String> {
    init_with(data_dir, force, &InitOptions::default())
}

/// `vectorfs init` with a preset (§5.10.4, 附录E.4): server fields +
/// generated credentials FIRST (typed serialization — byte-identical to the
/// pre-preset server file), then the preset's pipeline template appended as
/// text. The written file is immediately re-validated through
/// `load_pipeline_config` — a broken template must fail HERE (the authoring
/// site), never at the first `serve`. The model AVAILABILITY gate is
/// deliberately NOT run by init: air-gapped is expected to fail it until
/// the operator supplies the model (that fail-fast is `pipelines check`'s
/// job), and the model-carrying presets (text-rag, multimodal, image-search)
/// satisfy it via the init-time prefetch the CLI performs next.
pub fn init_with(data_dir: &Path, force: bool, opts: &InitOptions) -> Result<InitOutcome, String> {
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
             old access key will stop authenticating).",
            path.display()
        ));
    }
    let models_dir = opts
        .preset
        .models_cache_dir(data_dir, opts.models_dir.as_deref());
    let config = FileConfig {
        version: 1,
        region: DEFAULT_REGION.to_string(),
        account_id: DEFAULT_ACCOUNT.to_string(),
        access_key_id: generate_access_key(),
        secret_access_key: generate_secret_key(),
        // bucket_suffix stays ABSENT (the `-vf` default applies; it is a
        // namespace identity constant, not something init should surprise
        // operators with)
        bucket_suffix: None,
        // raw section tables stay None: the preset template is appended as
        // TEXT below (typed round-tripping of the template through
        // FileConfig would reorder and re-quote it for zero benefit — the
        // §5.10.4 comparison shape is the point).
        extractor: None,
        embedder: None,
        ingest: None,
        // top-level models_dir key: ONLY the model-carrying presets' container
        // path needs it (text-rag / multimodal / image-search — all resolve
        // models through the cache root; air-gapped embeds the path; dev has
        // no models at all)
        models_dir: match (opts.preset, &opts.models_dir) {
            (Preset::TextRag | Preset::Multimodal | Preset::ImageSearch, Some(dir)) => {
                Some(dir.display().to_string())
            }
            _ => None,
        },
    };
    let mut toml_doc = toml::to_string_pretty(&config)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    let template = opts.preset.template(&models_dir);
    if !template.is_empty() {
        toml_doc.push('\n');
        toml_doc.push_str(&template);
    }
    // parse-level self-validation (see fn docs): the template must satisfy
    // the same two-stage parser serve runs, including the modality
    // carry-over and derived-name gates
    if let Err(e) = crate::pipeline::config::load_pipeline_config(&toml_doc) {
        return Err(format!(
            "internal error: preset '{}' produced an invalid config: {e} \
             (this is a vectorfs bug — please report it)",
            opts.preset
        ));
    }
    std::fs::write(&path, toml_doc).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(InitOutcome {
        config,
        path,
        regenerated: force,
        preset: opts.preset,
        models_dir,
        endpoint: endpoint_hint(opts.listen.as_deref()),
        serve_hint_backend: opts.backend.clone(),
    })
}

/// Endpoint for the report/env snippet: `--listen host[:port]` > the
/// loopback default. The port defaults to 8787 even when the daemon will
/// later serve elsewhere (`--port`) — the hint documents the COMMON shape.
fn endpoint_hint(listen: Option<&str>) -> String {
    match listen {
        Some(spec) => {
            // reuse the serve-side split so the hint and the real bind agree
            let (host, port) = listen_of(
                &ServeFlags {
                    listen: Some(spec.to_string()),
                    ..Default::default()
                },
                &|_| None,
            );
            format!("http://{host}:{}", port.unwrap_or(DEFAULT_PORT))
        }
        None => format!("http://127.0.0.1:{DEFAULT_PORT}"),
    }
}

/// Success printout for `vectorfs init`: credentials, next command, client
/// env snippet + SDK pointers (the §3 five-minute promise: copy-paste from
/// here to first query), preset-specific guidance.
pub fn init_report(outcome: &InitOutcome) -> String {
    let c = &outcome.config;
    // config sits one level down now (<dir>/.vectorfs/vectorfs.toml) — the
    // report still names the DATA DIR operators pass to --data-dir.
    let dir = outcome
        .path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."));
    let ep = &outcome.endpoint;
    // serve hint: --listen/--backend are echoed when the operator gave them
    // (they are per-invocation flags, never written to vectorfs.toml)
    let mut serve_cmd = format!("vectorfs serve --data-dir {dir}", dir = dir.display());
    if let Some(backend) = &outcome.serve_hint_backend {
        serve_cmd.push_str(&format!(" --backend {backend}"));
    }
    let serve_hint = if outcome.endpoint != format!("http://127.0.0.1:{DEFAULT_PORT}") {
        // --listen was explicit: echo it so the printed command reproduces
        // the advertised endpoint
        let host = outcome
            .endpoint
            .trim_start_matches("http://")
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_default();
        format!("{serve_cmd} --listen {host}")
    } else {
        serve_cmd
    };
    format!(
        "{sep}vectorfs initialized ({preset}){regen}\n\
         data dir : {dir}\n\
         config   : {path}\n\
         \n\
         access key id : {ak}\n\
         secret key    : {sk}   (stored in vectorfs.toml — see note below)\n\
         \n\
         Next: start the server\n\
         \x20   {serve_hint}\n\
         \n\
         Client env (vf CLI + any SigV4 client):\n\
         \x20   export VF_ENDPOINT=\"{ep}\"\n\
         \x20   export VF_ACCESS_KEY=\"{ak}\"\n\
         \x20   export VF_SECRET_KEY=\"{sk}\"\n\
         \n\
         Point your SDK at the endpoint ({ep}):\n\
         \x20   boto3: client = boto3.client(\"s3vectors\", endpoint_url=\"{ep}\",\n\
         \x20           aws_access_key_id=\"{ak}\", aws_secret_access_key=\"{sk}\", region_name=\"{region}\")\n\
         \x20   Go:    cfg.BaseEndpoint = aws.String(\"{ep}\") // + static creds, region {region}\n\
         {preset_note}\
         note: vectorfs.toml holds the secret key in PLAINTEXT (v0.1\n\
         single-machine tradeoff; the data root already stores all vectors\n\
         unencrypted).\n{sep}",
        sep = "-".repeat(62),
        preset = outcome.preset,
        regen = if outcome.regenerated {
            " — credentials REGENERATED, old access key no longer works"
        } else {
            ""
        },
        dir = dir.display(),
        path = outcome.path.display(),
        ak = c.access_key_id,
        sk = c.secret_access_key,
        region = c.region,
        serve_hint = serve_hint,
        ep = ep,
        preset_note = preset_note(outcome),
    )
}

/// Preset-specific guidance block for [`init_report`] (empty for dev — the
/// pure protocol face needs nothing beyond the credentials).
fn preset_note(outcome: &InitOutcome) -> String {
    match outcome.preset {
        Preset::Dev => String::new(),
        Preset::TextRag => format!(
            "\n\
             File layer (text-rag): put a file, search by meaning:\n\
             \x20   vf put a.pdf --bucket docs\n\
             \x20   vf search \"<your query>\" --bucket docs\n\
             \n"
        ),
        Preset::AirGapped => format!(
            "\n\
             Air-gapped preset: NO download is performed (offline = true).\n\
             Place the model directory at\n\
             \x20   {model_dir}   (model.onnx + tokenizer.json)\n\
             Docker: mount it under the /vectorfs/models volume; bare metal:\n\
             copy the directory there or edit `path` in vectorfs.toml.\n\
             `vectorfs pipelines check` stays red until the model is in place.\n\
             \n",
            model_dir = outcome.models_dir.join(Preset::DEFAULT_MODEL).display()
        ),
        Preset::Multimodal => "\n\
             File layer (multimodal): text and images in ONE bucket:\n\
             \x20   vf put a.pdf docs/    (text-native -> text index)\n\
             \x20   vf put scan.pdf docs/  (scan-dominant -> page images -> image index)\n\
             \x20   vf put photo.png docs/\n\
             \x20   vf search \"<text query>\" docs        (searches BOTH indexes)\n\
             \x20   vf search --image photo.png docs      (以图搜图)\n\
             \n"
        .to_string(),
        Preset::ImageSearch => "\n\
             File layer (image-search): 以图搜图 over the images you put:\n\
             \x20   vf put photo.png docs/\n\
             \x20   vf search --image photo.png docs\n\
             Text queries also work (siglip2's text tower); non-image files\n\
             are not claimed in this preset.\n\
             \n"
        .to_string(),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

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
        // M2 T8: the client env snippet + endpoint are part of the
        // five-minute promise now
        assert!(report.contains("export VF_ENDPOINT=\"http://127.0.0.1:8787\""));
        assert!(report.contains("export VF_ACCESS_KEY="));
        assert!(report.contains("export VF_SECRET_KEY="));
        assert!(report.contains("(dev)"));
    }

    /// --listen / --backend shape the REPORT only; vectorfs.toml keeps its
    /// T1 byte shape (no backend, no listen — per-invocation flags).
    #[test]
    fn init_with_listen_and_backend_are_report_only() {
        let dir = tempfile::tempdir().unwrap();
        let out = init_with(
            dir.path(),
            false,
            &InitOptions {
                preset: crate::preset::Preset::Dev,
                listen: Some("0.0.0.0:9431".into()),
                backend: Some("s3+http://minio:9000/bk".into()),
                models_dir: None,
            },
        )
        .unwrap();
        let report = init_report(&out);
        assert!(report.contains("http://0.0.0.0:9431"), "{report}");
        assert!(
            report.contains("--backend s3+http://minio:9000/bk"),
            "{report}"
        );
        assert!(report.contains("--listen 0.0.0.0"), "{report}");
        // the FILE carries neither knob
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(!text.contains("backend"));
        assert!(!text.contains("listen"));
    }

    /// Bind-address resolution (§5.10.7 container need): `--listen` /
    /// `VF_LISTEN` accept host or host:port; the embedded port wins over
    /// --port/VF_PORT; default stays loopback.
    #[test]
    fn listen_resolution_forms() {
        let mk = |listen: Option<String>| ServeFlags {
            listen,
            ..Default::default()
        };
        let env = env_map(&[]);
        // default: loopback, port chain untouched
        let (host, port) = listen_of(&mk(None), &env);
        assert_eq!((host.as_str(), port), ("127.0.0.1", None));
        // host only
        let (host, port) = listen_of(&mk(Some("0.0.0.0".into())), &env);
        assert_eq!((host.as_str(), port), ("0.0.0.0", None));
        // host:port — port surfaces separately
        let (host, port) = listen_of(&mk(Some("0.0.0.0:9000".into())), &env);
        assert_eq!((host.as_str(), port), ("0.0.0.0", Some(9000)));
        // env layer feeds it too, flag beats env
        let (host, _) = listen_of(
            &mk(Some("10.0.0.1".into())),
            &env_map(&[("VF_LISTEN", "10.9.9.9")]),
        );
        assert_eq!(host, "10.0.0.1");

        // full resolution: embedded listen port beats --port and VF_PORT;
        // host-only listen defers to the port chain
        let c = resolve_with(
            &ServeFlags {
                listen: Some("0.0.0.0:9100".into()),
                port: Some(8888),
                no_auth: true,
                data_dir: Some(PathBuf::from("/tmp/x")),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        assert_eq!((c.bind_host.as_str(), c.port), ("0.0.0.0", 9100));
        let c = resolve_with(
            &ServeFlags {
                listen: Some("0.0.0.0".into()),
                port: Some(8888),
                no_auth: true,
                data_dir: Some(PathBuf::from("/tmp/x")),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        assert_eq!((c.bind_host.as_str(), c.port), ("0.0.0.0", 8888));
        let c = resolve_with(
            &ServeFlags {
                no_auth: true,
                data_dir: Some(PathBuf::from("/tmp/x")),
                ..Default::default()
            },
            &env_map(&[("VF_LISTEN", "0.0.0.0"), ("VF_PORT", "7777")]),
        )
        .unwrap();
        assert_eq!((c.bind_host.as_str(), c.port), ("0.0.0.0", 7777));
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
            backend: Some("s3+https://h.example".into()),
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
