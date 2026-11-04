//! `vf` CLI — subcommand skeleton (M1 Task 1).
//!
//! `vf serve` is the fully functional server harvested from the M0 spike
//! (all serve flags carried over 1:1). `vf init` / `vf status` / `vf doctor`
//! are placeholders, implemented in M1 T2 / T6 (design.md §5.8).

use clap::{CommandFactory, Parser, Subcommand};
use vectorfs::ops::AppState;
use vectorfs::{s3, sigv4, store};

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

    /// Generate static credentials / write local config (TODO: M1 T2)
    Init,
    /// Show daemon / backend / bucket+index overview (TODO: M1 T6)
    Status,
    /// Probe backend connectivity & conditional-write capability (TODO: M1 T6)
    Doctor,
}

/// `vf serve` arguments — carried over 1:1 from the M0 spike server.
#[derive(Parser, Debug)]
struct ServeArgs {
    /// TCP port to listen on (env override: VF_PORT)
    #[arg(long, env = "VF_PORT", default_value_t = 8787)]
    port: u16,

    /// Data directory for the file backend
    // TODO(M1 T2): `vf init` will own config/data-dir defaults; revisit this
    // spike-era default then (kept as-is for flag compatibility).
    #[arg(long, default_value = "./spike-data")]
    data_dir: String,

    /// Backend URI: `file:<dir>` (default, --data-dir shorthand) or
    /// `s3+http://host:port/bucket[/prefix]` (S3-compatible).
    #[arg(long)]
    backend: Option<String>,

    /// S3 region (default us-east-1; MinIO/seaweedfs convention)
    #[arg(long, default_value = "us-east-1")]
    s3_region: String,

    /// S3 static access key (env override: VF_S3_ACCESS_KEY / AWS_ACCESS_KEY_ID)
    #[arg(long, env = "VF_S3_ACCESS_KEY")]
    s3_access_key: Option<String>,

    /// S3 static secret key (env override: VF_S3_SECRET_KEY / AWS_SECRET_ACCESS_KEY)
    #[arg(long, env = "VF_S3_SECRET_KEY")]
    s3_secret_key: Option<String>,

    /// Disable SigV4 verification entirely (CI mode)
    #[arg(long)]
    no_auth: bool,

    /// Static access key for SigV4 (env override: VF_ACCESS_KEY)
    #[arg(long, env = "VF_ACCESS_KEY")]
    access_key: Option<String>,

    /// Static secret key for SigV4 (env override: VF_SECRET_KEY)
    #[arg(long, env = "VF_SECRET_KEY")]
    secret_key: Option<String>,

    /// Region clients must sign for (enters credential scope + ARNs)
    #[arg(long, default_value = "us-east-1")]
    region: String,

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
        // Bare `vf` already gets help from arg_required_else_help; this arm is
        // the explicit fallback for a None subcommand.
        None => {
            Cli::command().print_help().expect("print help");
        }
        Some(Command::Init) => placeholder("init", "M1 T2"),
        Some(Command::Status) => placeholder("status", "M1 T6"),
        Some(Command::Doctor) => placeholder("doctor", "M1 T6"),
    }
}

async fn serve(args: ServeArgs) {
    let creds = match (args.access_key.as_deref(), args.secret_key.as_deref()) {
        (Some(ak), Some(sk)) if !ak.is_empty() && !sk.is_empty() => Some(sigv4::Credentials {
            access_key: ak.to_string(),
            secret_key: sk.to_string(),
        }),
        _ => None,
    };
    if args.no_auth {
        // explicit opt-out
    } else if creds.is_none() {
        eprintln!(
            "vf: WARNING no credentials configured (--access-key/--secret-key or \
             VF_ACCESS_KEY/VF_SECRET_KEY) and --no-auth not given; running WITHOUT auth"
        );
    }

    let auth_desc = if args.no_auth || creds.is_none() {
        "none".to_string()
    } else {
        format!(
            "sigv4(access_key={})",
            args.access_key.as_deref().unwrap_or("")
        )
    };

    let backend_spec = args
        .backend
        .clone()
        .unwrap_or_else(|| format!("file:{}", args.data_dir));
    let storage = if let Some(dir) = backend_spec.strip_prefix("file:") {
        store::Storage::file(std::path::PathBuf::from(dir))
    } else if let Some(mut cfg) = s3::S3BackendConfig::parse(&backend_spec) {
        cfg.region = args.s3_region.clone();
        cfg.access_key = args
            .s3_access_key
            .clone()
            .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok());
        cfg.secret_key = args
            .s3_secret_key
            .clone()
            .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok());
        match store::Storage::s3(cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("vf: S3 backend init failed: {e}");
                std::process::exit(2);
            }
        }
    } else {
        eprintln!(
            "vf: cannot parse --backend {backend_spec:?} (expected file:<dir> or s3+http://host:port/bucket[/prefix])"
        );
        std::process::exit(2);
    };
    let state = AppState {
        storage: std::sync::Arc::new(storage),
        creds: if args.no_auth { None } else { creds },
        region: args.region.clone(),
        account: "000000000000".to_string(),
        trace: args.trace,
    };

    eprintln!(
        "vf: starting on 127.0.0.1:{} backend={:?} auth={} region={} account={} trace={}",
        args.port, state.storage, auth_desc, args.region, state.account, args.trace
    );

    let app = vectorfs::build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .expect("bind 127.0.0.1 failed");
    axum::serve(listener, app).await.expect("server exited");
}
