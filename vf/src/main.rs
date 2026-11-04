//! `vf` CLI (M1: serve + init; status/doctor land in M1 T6, design.md §5.8).
//!
//! `vf serve` is the fully functional s3vectors server harvested from the M0
//! spike, now on the unified backend (file/memory/S3), the flag > env >
//! vf.json > default config chain (config.rs), and the single-writer lock
//! (lock.rs).

use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
use vectorfs::config::{self, ServeFlags};
use vectorfs::ops::AppState;
use vectorfs::{backends, lock, store};

#[derive(Parser, Debug)]
#[command(
    name = "vf",
    version,
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
    /// Show daemon / backend / bucket+index overview (TODO: M1 T6)
    Status,
    /// Probe backend connectivity & conditional-write capability (TODO: M1 T6)
    Doctor,
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

fn placeholder(name: &str, task: &str) -> ! {
    eprintln!("vf {name}: not implemented yet ({task})");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Init(args)) => init(args),
        // Bare `vf` already gets help from arg_required_else_help; this arm is
        // the explicit fallback for a None subcommand.
        None => {
            Cli::command().print_help().expect("print help");
        }
        Some(Command::Status) => placeholder("status", "M1 T6"),
        Some(Command::Doctor) => placeholder("doctor", "M1 T6"),
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
