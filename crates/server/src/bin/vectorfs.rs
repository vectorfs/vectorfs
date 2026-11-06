//! `vectorfs` — the SERVER CLI (design.md §0 command ruling + 附录E.4:
//! init/serve/status/doctor/mcp; optimize/pipelines land with later tasks —
//! no empty stubs).
//!
//! `vectorfs serve` is the fully functional s3vectors server harvested from
//! the M0 spike, now on the unified backend (file/memory/S3), the flag > env
//! > `.vectorfs/vectorfs.toml` > default config chain (config.rs), and the
//! single-writer lock (lock.rs). `vectorfs status` / `vectorfs doctor` are
//! the read-only inspection pair (status.rs / doctor.rs) and
//! `vectorfs mcp` is the stdio MCP server for the agent single-user mode
//! (mcp.rs). The thin HTTP CLIENT lives in the separate `vf` binary
//! (src/bin/vf.rs) — engine/server modules stay unlinked there.
//!
//! Output contract everywhere (report.rs, §5.8): stdout = results, stderr =
//! progress, exit codes grep-compatible, abnormal exits print
//! {code, message, hint}.

use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
use vectorfs_server::config::{self, ServeFlags};
use vectorfs_server::ops::AppState;
use vectorfs_server::report::Failure;
use vectorfs_server::{backends, lock, mcp, status};

/// `--version` needs a `&'static str` for clap; compute once (report.rs
/// explains the VF_GIT_HASH compile-time option).
fn version_str() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(vectorfs_server::report::version_string)
}

#[derive(Parser, Debug)]
#[command(
    name = "vectorfs",
    version = version_str(),
    about = "vectorfs — turn any S3 into S3 Vectors. Server binary (the thin client is `vf`).",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the s3vectors-compatible HTTP server
    Serve(ServeArgs),

    /// Generate static credentials and write <data-dir>/.vectorfs/vectorfs.toml
    Init(InitArgs),

    /// Read-only data-root overview: LOCK holder, backend, buckets/indexes,
    /// per-index vector counts, data volume — or probe a daemon with
    /// --endpoint
    ///
    /// Examples (copy-paste ready):
    ///
    ///   vectorfs status --data-dir ./vectorfs-data
    ///
    ///   vectorfs status --data-dir ./vectorfs-data --json | jq .
    ///
    ///   vectorfs status --endpoint http://127.0.0.1:8787
    Status(StatusArgs),

    /// Diagnose a data root: writable/conditional-write probe,
    /// vectorfs.toml, LOCK state, per-index dataset health, disk headroom
    ///
    /// Examples:
    ///
    ///   vectorfs doctor --data-dir ./vectorfs-data
    ///
    ///   vectorfs doctor --data-dir ./vectorfs-data --json | jq '.overall'
    Doctor(DoctorArgs),

    /// Run the stdio MCP server (agent single-user mode: this process is
    /// the ONLY writer — it takes the same LOCK as `vectorfs serve`)
    ///
    /// Examples (for an MCP client config):
    ///
    ///   vectorfs mcp --data-dir ./vectorfs-data
    ///
    ///   vectorfs mcp --data-dir ./vectorfs-data --tools ro
    Mcp(McpArgs),

    /// Inspect / validate the file-layer pipeline configuration
    /// (M2 T2: the [extractor.*] / [embedder.*] / [ingest.*] sections of
    /// vectorfs.toml)
    ///
    /// Examples:
    ///
    ///   vectorfs pipelines check --data-dir ./vectorfs-data
    ///
    ///   vectorfs pipelines check --config ./vectorfs.toml
    Pipelines(PipelinesArgs),
}

/// `vectorfs init` arguments.
#[derive(Parser, Debug)]
struct InitArgs {
    /// Data directory to initialize (default ./vectorfs-data)
    #[arg(long)]
    data_dir: Option<String>,

    /// Configuration preset to materialize (design.md §5.10.4): dev = pure
    /// protocol face (default, identical to the pre-preset init);
    /// text-rag = gte text pipeline + model PREFETCH (~1.17 GiB, stderr
    /// progress, HF_ENDPOINT mirror honored); air-gapped = offline ort
    /// embedder, no download — supply the model directory yourself;
    /// multimodal = gte + siglip2 dual embedders (text AND image queries,
    /// prefetches both models, ~2.7 GiB); image-search = siglip2 only
    /// (以图搜图, prefetches ~1.5 GiB)
    #[arg(long)]
    preset: Option<vectorfs_server::preset::Preset>,

    /// Report-only hint: the endpoint the daemon will serve on (host or
    /// host:port). Shapes the printed serve command + client env snippet;
    /// never written to vectorfs.toml (the bind address is a serve-time
    /// flag, §5.10.7 env-override boundary)
    #[arg(long)]
    listen: Option<String>,

    /// Report-only hint: backend spec echoed into the printed serve
    /// command. Backends are per-invocation and never written to
    /// vectorfs.toml (config.rs resolution chain owns the real behavior)
    #[arg(long)]
    backend: Option<String>,

    /// Models cache root (container deployments: the /vectorfs/models
    /// VOLUME, §5.10.7 — the model-carrying presets record it as
    /// `models_dir`, air-gapped bakes it into the embedder `path`; bare
    /// metal leaves it unset and the <data-dir>/.vectorfs/models default
    /// applies)
    #[arg(long)]
    models_dir: Option<String>,

    /// Regenerate credentials even if vectorfs.toml already exists
    #[arg(long)]
    force: bool,
}

/// `vectorfs serve` arguments. Every option here feeds the flag layer of the
/// resolution chain (flag > VF_* env > <data-dir>/.vectorfs/vectorfs.toml >
/// default); see config.rs for the full precedence and the exact env names.
#[derive(Parser, Debug)]
struct ServeArgs {
    /// TCP port to listen on [env: VF_PORT] [default: 8787]
    #[arg(long)]
    port: Option<u16>,

    /// Bind address for the HTTP listener: `host` or `host:port` (an
    /// embedded port wins over --port/VF_PORT). Default 127.0.0.1 — set
    /// 0.0.0.0 in containers so `-p` port mapping reaches the daemon (the
    /// image does this via VF_LISTEN, design.md §5.10.7)
    /// [env: VF_LISTEN]
    #[arg(long)]
    listen: Option<String>,

    /// Data directory (file backend root + where .vectorfs/vectorfs.toml is
    /// read from) [env: VF_DATA_DIR] [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,

    /// Backend: `file` (uses --data-dir as the bucket root), `memory`
    /// (volatile — CI/emulator mode, DATA IS NOT PERSISTED), or
    /// `s3+http(s)://host:port` (S3-compatible ENDPOINT; buckets are
    /// addressed per vector bucket — source bucket X + system bucket X-vf.
    /// Credentials from --s3-access-key/--s3-secret-key/--s3-region or
    /// VF_S3_ACCESS_KEY/VF_S3_SECRET_KEY/VF_S3_REGION, falling back to
    /// AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY/AWS_REGION)
    /// [env: VF_BACKEND] [default: file]
    #[arg(long)]
    backend: Option<String>,

    /// S3 backend region (MinIO/seaweedfs convention: us-east-1)
    #[arg(long)]
    s3_region: Option<String>,

    /// S3 backend static access key [env: VF_S3_ACCESS_KEY or AWS_ACCESS_KEY_ID]
    #[arg(long)]
    s3_access_key: Option<String>,

    /// S3 backend static secret key [env: VF_S3_SECRET_KEY or AWS_SECRET_ACCESS_KEY]
    #[arg(long)]
    s3_secret_key: Option<String>,

    /// Disable SigV4 verification entirely (CI mode; also skips the
    /// credential requirement — anyone reachable can read/write)
    #[arg(long)]
    no_auth: bool,

    /// Static access key for SigV4 [env: VF_ACCESS_KEY or <data-dir>/.vectorfs/vectorfs.toml]
    #[arg(long)]
    access_key: Option<String>,

    /// Static secret key for SigV4 [env: VF_SECRET_KEY or <data-dir>/.vectorfs/vectorfs.toml]
    #[arg(long)]
    secret_key: Option<String>,

    /// Region clients must sign for; enters the credential scope and ARNs
    /// [env: VF_REGION or <data-dir>/.vectorfs/vectorfs.toml] [default: us-east-1]
    #[arg(long)]
    region: Option<String>,

    /// One-line-JSON request/response tracing to stderr
    #[arg(long)]
    trace: bool,

    /// Skip the pipeline-fingerprint gate and RE-INDEX at startup: every
    /// ingest bucket holding fingerprint-drifted markers gets its old chunks
    /// cleared and its sources re-embedded (mechanism = /x-vf/Reindex), then
    /// the daemon serves normally with ingest active. The remedy for
    /// "changed vectorfs.toml, serve now refuses to start"
    #[arg(long)]
    reindex: bool,
}

/// `vectorfs status` arguments (data-dir mode resolves exactly like serve's
/// storage slice; see config::resolve_storage).
#[derive(Parser, Debug)]
struct StatusArgs {
    /// Data directory to inspect [env: VF_DATA_DIR] [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,

    /// Backend: file | memory | s3+http(s)://host:port/bucket[/prefix]
    /// [env: VF_BACKEND] [default: file]
    #[arg(long)]
    backend: Option<String>,

    /// Probe a RUNNING daemon instead of the local data root: GETs
    /// <endpoint>/healthz and prints what it reports (http only)
    #[arg(long, conflicts_with = "data_dir")]
    endpoint: Option<String>,

    /// Structured output (schema_version'd JSON) instead of the human table
    #[arg(long)]
    json: bool,
}

/// `vectorfs doctor` arguments.
#[derive(Parser, Debug)]
struct DoctorArgs {
    /// Data directory to diagnose [env: VF_DATA_DIR] [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,

    /// Backend: file | memory | s3+http(s)://host:port
    /// [env: VF_BACKEND] [default: file]
    #[arg(long)]
    backend: Option<String>,

    /// Structured output: {schema_version, overall, checks:[…]}
    #[arg(long)]
    json: bool,
}

/// `vectorfs mcp` arguments.
#[derive(Parser, Debug)]
struct McpArgs {
    /// Data directory to serve exclusively [env: VF_DATA_DIR]
    /// [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,

    /// Tool set: `ro` = list/query/get only; `rw` adds put/delete
    /// [default: rw]
    #[arg(long, default_value = "rw")]
    tools: mcp::ToolsMode,
}

/// `vectorfs pipelines …` arguments (`check` for now; more verbs land with
/// the T3+ runtime registry).
#[derive(Parser, Debug)]
struct PipelinesArgs {
    #[command(subcommand)]
    command: PipelinesCommand,
}

#[derive(Subcommand, Debug)]
enum PipelinesCommand {
    /// Validate the pipeline sections of vectorfs.toml and print a summary
    /// (instances, ingest sections, fingerprints). Exit 0 = valid,
    /// 1 = validation failure (line-numbered), 2 = usage/IO.
    Check(PipelinesCheckArgs),
}

/// `vectorfs pipelines check` arguments.
#[derive(Parser, Debug)]
struct PipelinesCheckArgs {
    /// Explicit config file to check (instead of <data-dir>/.vectorfs/
    /// vectorfs.toml; the data dir STILL resolves the models cache for the
    /// availability gate)
    #[arg(long)]
    config: Option<String>,

    /// Data directory: config location when --config is absent, and the
    /// models-cache root for the model availability gate
    /// [env: VF_DATA_DIR] [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Init(args)) => init(args).await,
        Some(Command::Status(args)) => status_cmd(args).await,
        Some(Command::Doctor(args)) => doctor_cmd(args).await,
        Some(Command::Mcp(args)) => mcp_cmd(args).await,
        Some(Command::Pipelines(args)) => pipelines_cmd(args),
        // Bare `vectorfs` already gets help from arg_required_else_help; this
        // arm is the explicit fallback for a None subcommand.
        None => {
            Cli::command().print_help().expect("print help");
        }
    }
}

/// Storage-only flags for the inspection commands (status/doctor/mcp): only
/// the flag layer — the env (`VF_DATA_DIR` / `VF_BACKEND`) and default
/// layers are applied by config::resolve_storage.
fn storage_flags(data_dir: &Option<String>, backend: &Option<String>) -> ServeFlags {
    ServeFlags {
        data_dir: data_dir.as_deref().map(std::path::PathBuf::from),
        backend: backend.clone(),
        ..Default::default()
    }
}

async fn status_cmd(args: StatusArgs) {
    if let Some(url) = &args.endpoint {
        match status::endpoint_probe(url).await {
            Ok(report) => {
                if args.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).expect("report serializes")
                    );
                } else {
                    print!("{}", status::render_endpoint_human(&report));
                }
            }
            Err(f) => f.fail(args.json, 1),
        }
        return;
    }
    let flags = storage_flags(&args.data_dir, &args.backend);
    let cfg = config::resolve_storage(&flags).unwrap_or_else(|e| {
        Failure::new("CONFIG", e, "fix the data dir / backend flags").fail(args.json, 2)
    });
    eprintln!(
        "vectorfs: inspecting data root at {} …",
        cfg.data_dir.display()
    );
    match status::gather(&cfg).await {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("report serializes")
                );
            } else {
                print!("{}", status::render_human(&report));
            }
        }
        Err(f) => f.fail(args.json, 1),
    }
}

async fn doctor_cmd(args: DoctorArgs) {
    let flags = storage_flags(&args.data_dir, &args.backend);
    let cfg = config::resolve_storage(&flags).unwrap_or_else(|e| {
        Failure::new("CONFIG", e, "fix the data dir / backend flags").fail(args.json, 2)
    });
    eprintln!(
        "vectorfs: probing data root at {} …",
        cfg.data_dir.display()
    );
    match vectorfs_server::doctor::run(&cfg).await {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("report serializes")
                );
            } else {
                print!("{}", vectorfs_server::doctor::render_human(&report));
            }
            if report.any_fail() {
                std::process::exit(1);
            }
        }
        Err(f) => f.fail(args.json, 1),
    }
}

async fn mcp_cmd(args: McpArgs) {
    let flags = storage_flags(&args.data_dir, &None);
    let cfg = config::resolve_storage(&flags)
        .unwrap_or_else(|e| Failure::new("CONFIG", e, "fix the data dir flag").fail(false, 2));
    match mcp::run(&cfg, args.tools).await {
        mcp::McpExit::Ok => {}
        mcp::McpExit::Held(who, hint) => Failure::new(
            "DATA_ROOT_HELD",
            format!("refusing to start: another writer owns this data root ({who})"),
            hint,
        )
        .fail(false, 3),
        mcp::McpExit::Failed(msg) => Failure::new(
            "MCP_START_FAILED",
            msg,
            "check the data root (vectorfs doctor) and that stdin/stdout are a pipe pair",
        )
        .fail(false, 2),
    }
}

/// `vectorfs pipelines check`: exit 0 = valid (stdout summary: parsed
/// instances, ingest sections, fingerprints), 1 = validation failure (each
/// error one stderr line, TOML-line attributed), 2 = usage/IO (clap usage
/// errors already exit 2).
fn pipelines_cmd(args: PipelinesArgs) {
    let PipelinesCommand::Check(check) = args.command;
    // File resolution: --config explicit; otherwise the same data-dir chain
    // serve uses (flag > VF_DATA_DIR > default), pointing at
    // <dir>/.vectorfs/vectorfs.toml.
    let path = match check.config {
        Some(f) => std::path::PathBuf::from(f),
        None => {
            let data_dir = check
                .data_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var("VF_DATA_DIR")
                        .ok()
                        .map(std::path::PathBuf::from)
                })
                .unwrap_or_else(|| std::path::PathBuf::from(config::DEFAULT_DATA_DIR));
            config::config_path(&data_dir)
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            // A missing file is a USAGE error here (the operator asked to
            // check a specific file) — the opposite of serve, where missing
            // means the pure-protocol face and is fine.
            eprintln!(
                "vectorfs pipelines check: cannot read {}: {e}",
                path.display()
            );
            std::process::exit(2);
        }
    };
    // data dir for the models cache (the availability gate resolves
    // config models_dir > <data-dir>/.vectorfs/models): same chain as the
    // no-flag path; with --config the file's own directory context is not
    // knowable, so flag/env/default apply.
    let data_dir = check
        .data_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("VF_DATA_DIR")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| std::path::PathBuf::from(config::DEFAULT_DATA_DIR));
    match vectorfs_server::pipeline::config::load_pipeline_config(&text) {
        Ok(cfg) => {
            // availability gate (fs pass over the models cache): an offline
            // ort config whose model is missing must fail the check, not the
            // first embed call (§5.10.3 fail-fast with line attribution)
            if let Err(e) = cfg.validate_model_availability(&data_dir) {
                eprintln!("vectorfs pipelines check: {e}");
                std::process::exit(1);
            }
            println!("vectorfs pipelines check: {} — OK", path.display());
            print_pipeline_summary(&cfg);
            // warnings (stderr): advisory, never exit-affecting — e.g. the
            // §5.10.5 license note for models outside the built-in table
            for w in &cfg.warnings {
                eprintln!("vectorfs pipelines check: warning: {w}");
            }
        }
        Err(e) => {
            eprintln!("vectorfs pipelines check: {e}");
            std::process::exit(1);
        }
    }
}

/// Deterministic (BTreeMap-ordered) stdout summary of a parsed pipeline
/// config. stdout = results, stderr = progress/errors (report.rs contract).
fn print_pipeline_summary(cfg: &vectorfs_server::pipeline::config::PipelineConfig) {
    if cfg.extractors.is_empty() && cfg.embedders.is_empty() && cfg.ingest.is_empty() {
        println!("no pipeline sections (pure protocol face; nothing to do)");
        return;
    }
    for (name, kind) in &cfg.extractors {
        println!("extractor {name}: kind={}", kind.describe());
    }
    if cfg.embedders.is_empty() {
        println!("embedders: (none — the pure protocol face)");
    }
    for (name, kind) in &cfg.embedders {
        println!("embedder {name}: kind={}", kind.describe());
    }
    if let Some(dir) = &cfg.models_dir {
        println!("models_dir: {}", dir.display());
    }
    for (bucket, ingest) in &cfg.ingest {
        let claim = if ingest.include.is_empty() && ingest.exclude.is_empty() {
            "<whole bucket>".to_string()
        } else {
            format!("include={:?} exclude={:?}", ingest.include, ingest.exclude)
        };
        // extract routes, BTreeMap order; the array form shows the chain
        // (order = try order), the string form shows verbatim
        let routes = if ingest.extract.is_empty() {
            "(none)".to_string()
        } else {
            ingest
                .extract
                .iter()
                .map(|(key, value)| format!("{key} -> {}", value.describe()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let embedders = if ingest.embedders.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", ingest.embedders.join(", "))
        };
        println!(
            "ingest {bucket}: chunker={} claim={claim} extract: {routes} \
             embedders={embedders} merge={} fingerprint={}",
            ingest.chunk.describe(),
            ingest.merge,
            cfg.fingerprint(bucket).expect("bucket present"),
        );
    }
}

/// `vectorfs init`: materialize credentials (+ preset pipeline sections,
/// §5.10.4) and — for the model-carrying presets (text-rag, multimodal,
/// image-search) — PREFETCH every model the preset names (progress/sha256/
/// tmp+rename/HF_ENDPOINT mirror: pipeline::models). Async solely for those
/// downloads; everything else is local file IO.
async fn init(args: InitArgs) {
    let data_dir = args
        .data_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("VF_DATA_DIR")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| std::path::PathBuf::from(config::DEFAULT_DATA_DIR));
    if args.force {
        eprintln!(
            "vectorfs: WARNING --force regenerates credentials; clients still using the \
             previous access key will stop authenticating."
        );
    }
    let opts = config::InitOptions {
        preset: args.preset.unwrap_or_default(),
        listen: args.listen,
        backend: args.backend,
        models_dir: args.models_dir.map(std::path::PathBuf::from),
    };
    let outcome = match config::init_with(&data_dir, args.force, &opts) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vectorfs init: {e}");
            std::process::exit(1);
        }
    };
    // model-carrying presets: prefetch every model the preset names into
    // the resolved cache dir BEFORE printing the report (the §3 "model
    // ready" five-minute promise happens here, at init, not at first
    // serve). air-gapped deliberately downloads nothing (offline
    // semantics); dev has no models at all. The list is the preset's own
    // declaration (preset.prefetch_models) so a new tier wiring an embedder
    // decides its prefetch in the same place.
    let models = outcome.preset.prefetch_models().to_vec();
    if !models.is_empty() {
        eprintln!(
            "vectorfs init: preset {}: prefetching {} model(s) into {} …",
            outcome.preset,
            models.len(),
            outcome.models_dir.display()
        );
        for name in &models {
            match vectorfs_server::pipeline::models::prefetch(name, &outcome.models_dir).await {
                Ok(dir) => {
                    eprintln!(
                        "vectorfs init: model ready at {} (sha256-verified, stamp .complete)",
                        dir.display()
                    );
                }
                Err(e) => {
                    // the config IS materialized: serve retries the prefetch
                    // at embedder load (ort.rs / siglip2.rs), so the
                    // deployment stays fixable without re-init
                    eprintln!("vectorfs init: model prefetch FAILED ({name}): {e}");
                    eprintln!(
                        "vectorfs init: hint: the config was written; fix the network (or set \
                         HF_ENDPOINT to a mirror) and re-run with --force, or let `vectorfs \
                         serve` retry the prefetch at startup"
                    );
                    std::process::exit(1);
                }
            }
        }
    }
    println!("{}", config::init_report(&outcome));
}

async fn serve(args: ServeArgs) {
    let flags = ServeFlags {
        port: args.port,
        listen: args.listen,
        data_dir: args.data_dir.map(std::path::PathBuf::from),
        backend: args.backend,
        no_auth: args.no_auth,
        access_key: args.access_key,
        secret_key: args.secret_key,
        region: args.region,
        s3_region: args.s3_region,
        s3_access_key: args.s3_access_key,
        s3_secret_key: args.s3_secret_key,
        trace: args.trace,
    };
    let cfg = match config::resolve(&flags) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vectorfs: {e}");
            std::process::exit(2);
        }
    };

    // ---- pipeline config fail-fast (M2 T2, design.md §5.10.3) ----
    // The plugin sections share vectorfs.toml with the T1 server fields
    // (resolve already parsed its FileConfig slice; the sections were
    // tolerated there). Re-read the RAW TEXT here: line attribution needs
    // the source, and a bad pipeline config must refuse startup BEFORE any
    // backend/lock is touched. A MISSING file is the pure-protocol face —
    // serve starts normally with no file layer. The PARSED config is kept:
    // ingest sections (if any) drive the file-layer runtime below.
    let mut pipeline_cfg = None;
    match std::fs::read_to_string(config::config_path(&cfg.data_dir)) {
        Ok(text) => match vectorfs_server::pipeline::config::load_pipeline_config(&text) {
            Err(e) => {
                eprintln!("vectorfs: pipeline config invalid: {e}");
                eprintln!(
                    "vectorfs: hint: fix {} (or run `vectorfs pipelines check --data-dir {}`)",
                    config::config_path(&cfg.data_dir).display(),
                    cfg.data_dir.display()
                );
                std::process::exit(2);
            }
            // availability gate: a model an embedder needs at startup must
            // exist where its config says it will be found
            Ok(parsed) => {
                if let Err(e) = parsed.validate_model_availability(&cfg.data_dir) {
                    eprintln!("vectorfs: pipeline config invalid: {e}");
                    std::process::exit(2);
                }
                pipeline_cfg = Some(parsed);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!(
                "vectorfs: cannot read {}: {e}",
                config::config_path(&cfg.data_dir).display()
            );
            std::process::exit(2);
        }
    }

    // ---- backend ----
    // ResolvedConfig::backend() (NOT raw Backend::parse): injects the S3
    // connection settings — see the doc comment there for the unsigned-
    // request bug this guards against.
    let backend = match cfg.backend() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vectorfs: {e}");
            std::process::exit(2);
        }
    };
    if matches!(backend, backends::Backend::Memory) {
        eprintln!(
            "vectorfs: *** WARNING: --backend memory is VOLATILE — all buckets, indexes and \
             vectors are LOST when the process exits (CI / emulator mode). ***"
        );
    }
    let storage = match cfg.storage() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vectorfs: backend init failed: {e}");
            std::process::exit(2);
        }
    };
    let backend = storage.backend().clone();

    let storage_arc = Arc::new(storage);

    // ---- local flock (design-v3.md §5.1, unchanged mechanism) ----
    // Taken BEFORE any bucket LOCK: a second daemon on this machine must be
    // refused while it still holds NOTHING (per-bucket LOCKs taken before a
    // failing flock would linger fresh for a full TTL on exit paths that
    // never reach the heartbeat loop's cleanup).
    let _flock = match lock::take_flock(&backend) {
        Ok(g) => Some(g),
        Err(lock::LockError::Held(who)) => {
            eprintln!(
                "vectorfs: refusing to start: another writer owns this data root ({who}).\n\
                 \x20 If that daemon is really gone, stop it or remove the stale flock file."
            );
            std::process::exit(3);
        }
        Err(lock::LockError::Backend(_)) => None, // memory/s3: no local home
    };

    // ---- ingest runtime, pre-lock half (M2 T3, design.md §5.10.5) ----
    // With ingest sections configured: (1) the FINGERPRINT gate reads every
    // source marker and refuses startup on any config-drifted one (the
    // remedy is explicit re-embedding — `serve --reindex` at startup or `vf
    // reindex` against a running daemon; there is deliberately no background
    // re-embedding engine); (2) embedders LOAD (dim probe, credentials,
    // model prefetch — potentially slow, so it happens while nobody waits on
    // us for the lock). No ingest config = the pure-protocol face, zero
    // extra tasks, behavior unchanged.
    let mut ingest_core = None;
    // Per-bucket lock registry (v3 §5.1): built AFTER storage, BEFORE the
    // ingest buckets pin their locks.
    let locks = lock::LockManager::new(storage_arc.clone());
    if let Some(pcfg) = pipeline_cfg.as_ref().filter(|c| !c.ingest.is_empty()) {
        if args.reindex {
            eprintln!(
                "vectorfs: --reindex: skipping the fingerprint gate; drifted buckets re-embed \
                 at startup"
            );
        } else if let Err(e) =
            vectorfs_server::ingest::fingerprint_gate(&storage_arc, pcfg, &cfg.data_dir).await
        {
            eprintln!("vectorfs: refusing to start: {e}");
            std::process::exit(2);
        }
        let models_dir = pcfg.models_dir.clone().unwrap_or_else(|| {
            vectorfs_server::pipeline::models::default_models_dir(&cfg.data_dir)
        });
        match vectorfs_server::ingest::IngestCore::load(pcfg, &models_dir).await {
            Ok(core) => {
                // startup lock pass (§5.1): a bucket whose LOCK cannot be
                // taken is PINNED but skipped (its scans and jobs refuse
                // unlocked buckets) — one contested bucket must not take
                // the whole daemon down. The heartbeat loop re-tries the
                // lock each TTL/3, so a stale lock self-heals.
                for vb in core.describe_buckets().into_iter().map(|(vb, _, _)| vb) {
                    locks.pin(&vb);
                    if let Err(e) = locks.ensure_held(&vb).await {
                        eprintln!(
                            "vectorfs: ingest {vb}: SKIPPED for now — cannot take its writer \
                             lock ({}); scans resume when the other writer releases or its \
                             heartbeat expires ({}s)",
                            match e {
                                lock::LockError::Held(who) => who,
                                lock::LockError::Backend(e) => e,
                            },
                            lock::LOCK_TTL.as_secs()
                        );
                    }
                }
                for (vb, embedders, scan) in core.describe_buckets() {
                    eprintln!(
                        "vectorfs: ingest {vb}: {embedders} embedder(s), scan every {scan}s \
                         (fingerprints ok)"
                    );
                }
                ingest_core = Some(core);
            }
            Err(e) => {
                eprintln!("vectorfs: ingest pipeline failed to load: {e}");
                eprintln!(
                    "vectorfs: hint: `vectorfs pipelines check --data-dir {}` names the exact \
                     line",
                    cfg.data_dir.display()
                );
                std::process::exit(2);
            }
        }
    }

    // ---- HTTP ----
    let auth_desc = if cfg.no_auth {
        "none (--no-auth)".to_string()
    } else {
        // resolve() guarantees creds exist unless --no-auth
        format!(
            "sigv4(access_key={})",
            cfg.creds
                .as_ref()
                .map(|c| c.access_key.as_str())
                .unwrap_or("")
        )
    };
    // Ingest wiring (M2 T3): the HANDLE exists iff the runtime core was
    // built — it goes INTO the AppState (T6's immediate-trigger + /x-vf
    // surface over the SAME shared core); the scheduler + workers then
    // spawn off that same state (per-bucket LOCKs held, so first-write
    // domain creation is race-free by ownership).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = ingest_core.map(|core| {
        Arc::new(vectorfs_server::ingest::IngestHandle::new(
            vectorfs_server::ingest::IngestShared::new(core),
        ))
    });
    let state = AppState::with_locks(
        storage_arc.clone(),
        locks.clone(),
        if cfg.no_auth { None } else { cfg.creds.clone() },
        cfg.region.clone(),
        cfg.account.clone(),
        cfg.trace,
        handle.clone(),
    );
    let ingest_drain = handle.clone();
    // ---- `serve --reindex` flip (T7): BEFORE any worker exists ----
    // The startup-flag resolution of the gate/reindex deadlock (主会话
    // 裁决 2026-11-05): re-index every bucket the gate would have refused —
    // clear old chunks + flip markers under the lock — then hand the work to
    // the normal scheduler/workers below (progress via GetIngestStatus's
    // reindex job). Running it pre-spawn makes it race-free by construction.
    let mut reindexed_buckets = 0usize;
    if args.reindex
        && let Some(handle) = handle.as_ref()
    {
        for vb in handle.shared().bucket_names() {
            match vectorfs_server::ingest::reindex_bucket_if_stale(&state, handle.shared(), &vb)
                .await
            {
                Ok(Some(outcome)) => {
                    reindexed_buckets += 1;
                    eprintln!(
                        "vectorfs: --reindex {vb}: {} source(s) -> PENDING, {} old chunk(s) \
                         cleared (job {})",
                        outcome.flipped, outcome.cleared, outcome.job_id
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("vectorfs: --reindex of {vb} failed: {e}");
                    locks.release_all().await;
                    std::process::exit(2);
                }
            }
        }
    }
    if let Some(handle) = handle {
        let runtime = vectorfs_server::ingest::IngestRuntime::new(
            Arc::clone(handle.shared()),
            Arc::new(state.clone()),
            handle.notify(),
            shutdown_rx.clone(),
        );
        runtime.spawn(&handle);
        if reindexed_buckets > 0 {
            // the scheduler's first tick is skipped by design; poke it so
            // the reindex rewrite starts immediately
            handle.wake();
        }
        eprintln!("vectorfs: ingest worker running (SIGTERM drains in-flight files, ≤8s)");
    }
    eprintln!(
        "vectorfs: starting on {}:{} backend={backend} auth={auth_desc} \
         region={} account={} bucket_suffix={} writer={} trace={}",
        cfg.bind_host,
        cfg.port,
        cfg.region,
        cfg.account,
        storage_arc.bucket_suffix(),
        locks.writer_id(),
        cfg.trace
    );

    let app = vectorfs_server::build_router(state);
    let listener = match tokio::net::TcpListener::bind((cfg.bind_host.as_str(), cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vectorfs: bind {}:{} failed: {e}", cfg.bind_host, cfg.port);
            locks.release_all().await;
            std::process::exit(2);
        }
    };

    // Heartbeat + idle release (§5.1): every held bucket lock heartbeats in
    // parallel each TTL/3; losing one bucket's lock parks ONLY that bucket
    // (the manager logs it — the daemon keeps serving the rest).

    locks.start(shutdown_rx.clone());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server exited");

    // Graceful shutdown sequence: signal every background task (ingest
    // scheduler/workers stop taking NEW files and finish the current one),
    // drain them inside the 8s budget (Docker SIGKILLs at 10s), then release
    // the LOCK (only if still ours) and drop the flock.
    eprintln!("vectorfs: shutting down, releasing bucket locks…");
    let _ = shutdown_tx.send(true);
    if let Some(ingest) = ingest_drain.as_ref() {
        ingest
            .shutdown(vectorfs_server::ingest::SHUTDOWN_BUDGET)
            .await;
    }
    locks.release_all().await;
}

/// ctrl-c / SIGTERM (per-bucket lock loss no longer stops the daemon —
/// §5.1 parks only the affected bucket).
#[cfg(unix)]
async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
    }
}

/// ctrl-c only (non-unix fallback).
#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await
}
