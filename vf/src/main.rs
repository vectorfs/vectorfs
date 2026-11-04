//! `vf` CLI (M1: serve + init + status + doctor + mcp, design.md §5.8).
//!
//! `vf serve` is the fully functional s3vectors server harvested from the M0
//! spike, now on the unified backend (file/memory/S3), the flag > env >
//! vf.json > default config chain (config.rs), and the single-writer lock
//! (lock.rs). `vf status` / `vf doctor` are the read-only inspection pair
//! (status.rs / doctor.rs) and `vf mcp` is the stdio MCP server for the
//! agent single-user mode (mcp.rs).
//!
//! Output contract everywhere (report.rs, §5.8): stdout = results, stderr =
//! progress, exit codes grep-compatible, abnormal exits print
//! {code, message, hint}.

use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
use vectorfs::config::{self, ServeFlags};
use vectorfs::ops::AppState;
use vectorfs::report::Failure;
use vectorfs::{backends, lock, mcp, status, store};

/// `--version` needs a `&'static str` for clap; compute once (report.rs
/// explains the VF_GIT_HASH compile-time option).
fn version_str() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(vectorfs::report::version_string)
}

#[derive(Parser, Debug)]
#[command(
    name = "vf",
    version = version_str(),
    about = "vectorfs — turn any S3 into S3 Vectors. Single binary.",
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

    /// Generate static credentials and write <data-dir>/vf.json
    Init(InitArgs),

    /// Read-only data-root overview: LOCK holder, backend, buckets/indexes,
    /// per-index vector counts, data volume — or probe a daemon with
    /// --endpoint
    ///
    /// Examples (copy-paste ready):
    ///
    ///   vf status --data-dir ./vectorfs-data
    ///
    ///   vf status --data-dir ./vectorfs-data --json | jq .
    ///
    ///   vf status --endpoint http://127.0.0.1:8787
    Status(StatusArgs),

    /// Diagnose a data root: writable/conditional-write probe, vf.json,
    /// LOCK state, per-index dataset health, disk headroom
    ///
    /// Examples:
    ///
    ///   vf doctor --data-dir ./vectorfs-data
    ///
    ///   vf doctor --data-dir ./vectorfs-data --json | jq '.overall'
    Doctor(DoctorArgs),

    /// Run the stdio MCP server (agent single-user mode: this process is
    /// the ONLY writer — it takes the same LOCK as `vf serve`)
    ///
    /// Examples (for an MCP client config):
    ///
    ///   vf mcp --data-dir ./vectorfs-data
    ///
    ///   vf mcp --data-dir ./vectorfs-data --tools ro
    Mcp(McpArgs),
}

/// `vf init` arguments.
#[derive(Parser, Debug)]
struct InitArgs {
    /// Data directory to initialize (default ./vectorfs-data)
    #[arg(long)]
    data_dir: Option<String>,

    /// Regenerate credentials even if vf.json already exists
    #[arg(long)]
    force: bool,
}

/// `vf serve` arguments. Every option here feeds the flag layer of the
/// resolution chain (flag > VF_* env > <data-dir>/vf.json > default); see
/// config.rs for the full precedence and the exact env names.
#[derive(Parser, Debug)]
struct ServeArgs {
    /// TCP port to listen on [env: VF_PORT] [default: 8787]
    #[arg(long)]
    port: Option<u16>,

    /// Data directory (file backend root + where vf.json is read from)
    /// [env: VF_DATA_DIR] [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,

    /// Backend: `file` (uses --data-dir), `memory` (volatile — CI/emulator
    /// mode, DATA IS NOT PERSISTED), or
    /// `s3+http(s)://host:port/bucket[/prefix]` (S3-compatible remote;
    /// credentials from --s3-access-key/--s3-secret-key/--s3-region or
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

    /// Static access key for SigV4 [env: VF_ACCESS_KEY or <data-dir>/vf.json]
    #[arg(long)]
    access_key: Option<String>,

    /// Static secret key for SigV4 [env: VF_SECRET_KEY or <data-dir>/vf.json]
    #[arg(long)]
    secret_key: Option<String>,

    /// Region clients must sign for; enters the credential scope and ARNs
    /// [env: VF_REGION or <data-dir>/vf.json] [default: us-east-1]
    #[arg(long)]
    region: Option<String>,

    /// One-line-JSON request/response tracing to stderr
    #[arg(long)]
    trace: bool,
}

/// `vf status` arguments (data-dir mode resolves exactly like serve's
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

/// `vf doctor` arguments.
#[derive(Parser, Debug)]
struct DoctorArgs {
    /// Data directory to diagnose [env: VF_DATA_DIR] [default: ./vectorfs-data]
    #[arg(long)]
    data_dir: Option<String>,

    /// Backend: file | memory | s3+http(s)://host:port/bucket[/prefix]
    /// [env: VF_BACKEND] [default: file]
    #[arg(long)]
    backend: Option<String>,

    /// Structured output: {schema_version, overall, checks:[…]}
    #[arg(long)]
    json: bool,
}

/// `vf mcp` arguments.
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Init(args)) => init(args),
        Some(Command::Status(args)) => status_cmd(args).await,
        Some(Command::Doctor(args)) => doctor_cmd(args).await,
        Some(Command::Mcp(args)) => mcp_cmd(args).await,
        // Bare `vf` already gets help from arg_required_else_help; this arm is
        // the explicit fallback for a None subcommand.
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
    eprintln!("vf: inspecting data root at {} …", cfg.data_dir.display());
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
    eprintln!("vf: probing data root at {} …", cfg.data_dir.display());
    match vectorfs::doctor::run(&cfg).await {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("report serializes")
                );
            } else {
                print!("{}", vectorfs::doctor::render_human(&report));
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
            "check the data root (vf doctor) and that stdin/stdout are a pipe pair",
        )
        .fail(false, 2),
    }
}

fn init(args: InitArgs) {
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
            "vf: WARNING --force regenerates credentials; clients still using the \
             previous access key will stop authenticating."
        );
    }
    match config::init(&data_dir, args.force) {
        Ok(outcome) => {
            println!("{}", config::init_report(&outcome));
        }
        Err(e) => {
            eprintln!("vf init: {e}");
            std::process::exit(1);
        }
    }
}

async fn serve(args: ServeArgs) {
    let flags = ServeFlags {
        port: args.port,
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
            eprintln!("vf: {e}");
            std::process::exit(2);
        }
    };

    // ---- backend ----
    let backend = match backends::Backend::parse(&cfg.backend_spec, &cfg.data_dir) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vf: {e}");
            std::process::exit(2);
        }
    };
    if matches!(backend, backends::Backend::Memory) {
        eprintln!(
            "vf: *** WARNING: --backend memory is VOLATILE — all buckets, indexes and \
             vectors are LOST when the process exits (CI / emulator mode). ***"
        );
    }
    let storage = match store::Storage::new(backend) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vf: backend init failed: {e}");
            std::process::exit(2);
        }
    };
    let backend = storage.backend().clone();

    // ---- single-writer lock (§5.1): acquired BEFORE the listener binds ----
    let lock_guard = match lock::acquire(&backend, storage.catalog().clone()).await {
        Ok(g) => g,
        Err(lock::LockError::Held(who)) => {
            eprintln!(
                "vf: refusing to start: another writer owns this data root ({who}).\n\
                 \x20 If that daemon is really gone, wait for its lock heartbeat to expire \
                 ({}s) and restart, or remove the stale LOCK object.",
                lock::LOCK_TTL.as_secs()
            );
            std::process::exit(3);
        }
        Err(lock::LockError::Backend(e)) => {
            eprintln!("vf: lock acquisition failed: {e}");
            std::process::exit(3);
        }
    };

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
    let state = AppState {
        storage: Arc::new(storage),
        creds: if cfg.no_auth { None } else { cfg.creds.clone() },
        region: cfg.region.clone(),
        account: cfg.account.clone(),
        trace: cfg.trace,
    };
    eprintln!(
        "vf: starting on 127.0.0.1:{} backend={backend} auth={auth_desc} \
         region={} account={} owner={} trace={}",
        cfg.port,
        cfg.region,
        cfg.account,
        lock_guard.info().owner_uuid,
        cfg.trace
    );

    let app = vectorfs::build_router(state);
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vf: bind 127.0.0.1:{} failed: {e}", cfg.port);
            lock_guard.release().await;
            std::process::exit(2);
        }
    };

    // Heartbeat: TTL/3 rewrite; loss of ownership (taken over after our TTL
    // expired) must stop the daemon — wired into graceful shutdown.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let heartbeat_lock = Arc::new(tokio::sync::Mutex::new(lock_guard));
    tokio::spawn({
        let mut shutdown_rx = shutdown_rx.clone();
        let lock = heartbeat_lock.clone();
        async move {
            let mut ticker = tokio::time::interval(lock::HEARTBEAT_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = shutdown_rx.changed() => break,
                }
                let mut guard = lock.lock().await;
                match guard.heartbeat().await {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!(
                            "vf: LOCK lost (taken over by another writer after heartbeat \
                             expiry) — shutting down."
                        );
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    Err(e) => eprintln!("vf: LOCK heartbeat error: {e} (will retry)"),
                }
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await
        .expect("server exited");

    // graceful shutdown: release the LOCK (only if still ours), drop the flock.
    eprintln!("vf: shutting down, releasing lock…");
    heartbeat_lock.lock().await.release().await;
}

/// ctrl-c / SIGTERM, or the heartbeat task signalling lock loss.
#[cfg(unix)]
async fn shutdown_signal(mut lost_lock: tokio::sync::watch::Receiver<bool>) {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
        _ = lost_lock.changed() => {},
    }
}

/// ctrl-c only (non-unix fallback).
#[cfg(not(unix))]
async fn shutdown_signal(mut lost_lock: tokio::sync::watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = lost_lock.changed() => {},
    }
}
