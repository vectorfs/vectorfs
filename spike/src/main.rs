//! vectorfs M0 spike server — throwaway quality bar (buildable + measurable).
//!
//! Minimal axum server: `GET /healthz -> {"ok":true}` on 127.0.0.1:PORT.
//! Port: `--port` flag, `VF_PORT` env, default 8787.

mod ops;
mod sigv4;
mod store;

use axum::{routing::get, Json, Router};
use clap::Parser;
use serde_json::json;

#[derive(Parser, Debug)]
#[command(name = "vf-spike", version, about = "vectorfs M0 spike server (throwaway)")]
struct Args {
    /// TCP port to listen on (env override: VF_PORT)
    #[arg(long, env = "VF_PORT", default_value_t = 8787)]
    port: u16,

    /// Placeholder (not implemented yet): disable SigV4 auth checks
    #[arg(long)]
    no_auth: bool,

    /// Placeholder (not implemented yet): data directory for the file backend
    #[arg(long, default_value = "./spike-data")]
    data_dir: String,

    /// Placeholder (not implemented yet): verbose request tracing
    #[arg(long)]
    trace: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Placeholder flags parsed but unused for now — spike Task A scope.
    let _ = (&args.no_auth, &args.data_dir, &args.trace);
    eprintln!(
        "vf-spike: starting on 127.0.0.1:{} (no_auth={} data_dir={} trace={})",
        args.port, args.no_auth, args.data_dir, args.trace
    );

    let app = Router::new().route("/healthz", get(healthz));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .expect("bind 127.0.0.1 failed");
    axum::serve(listener, app).await.expect("server exited");
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}
