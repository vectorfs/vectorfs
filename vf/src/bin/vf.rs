//! `vf` — the vectorfs CLIENT CLI (design.md §0 command ruling + 附录E.4):
//! a thin binary that talks to a RUNNING `vectorfs serve` daemon over HTTP.
//! It never touches the engine/server modules (lancedb / the axum router) —
//! the linker drops them, keeping this binary far smaller than `vectorfs`.
//!
//! Endpoint comes from `--endpoint` or `VF_ENDPOINT`; the client credential
//! surface (`VF_ACCESS_KEY` / `VF_SECRET_KEY`, per-command flags) is reserved
//! for the T6 client verbs (put/search/get/rm/sources/reindex, 附录E.4) —
//! deliberately NOT stubbed here.
//!
//! M2-T1 ships exactly one verb: `status` (the /healthz probe shared with
//! `vectorfs status --endpoint`).

use clap::{Parser, Subcommand};
use vectorfs::report::Failure;
use vectorfs::status;

fn version_str() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(vectorfs::report::version_string)
}

#[derive(Parser, Debug)]
#[command(
    name = "vf",
    version = version_str(),
    about = "vf — the vectorfs client. Talks to a remote `vectorfs serve` daemon \
             over HTTP (endpoint from --endpoint or VF_ENDPOINT).",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Probe the daemon behind --endpoint: GETs <url>/healthz and prints what
    /// it reports (http only)
    ///
    /// Example:
    ///
    ///   vf status --endpoint http://127.0.0.1:8787
    Status(StatusArgs),
}

/// `vf status` arguments.
#[derive(Parser, Debug)]
struct StatusArgs {
    /// Daemon endpoint to probe [env: VF_ENDPOINT]
    #[arg(long)]
    endpoint: Option<String>,

    /// Structured output (schema_version'd JSON) instead of the human table
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() {
    match Cli::parse().command {
        Command::Status(args) => status_cmd(args).await,
    }
}

async fn status_cmd(args: StatusArgs) {
    // Manual env layer (same discipline as config.rs: the clap "env" feature
    // stays out): flag --endpoint > VF_ENDPOINT > actionable error.
    let url = args
        .endpoint
        .clone()
        .or_else(|| std::env::var("VF_ENDPOINT").ok().filter(|v| !v.is_empty()));
    let Some(url) = url else {
        Failure::new(
            "NO_ENDPOINT",
            "no endpoint given and VF_ENDPOINT is unset",
            "pass --endpoint http://127.0.0.1:8787 (or export VF_ENDPOINT; the daemon is \
             started with: vectorfs serve)",
        )
        .fail(args.json, 2);
    };
    match status::endpoint_probe(&url).await {
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
}
