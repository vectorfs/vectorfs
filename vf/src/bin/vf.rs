//! `vf` — the vectorfs CLIENT CLI (design.md §0 command ruling + 附录E.4):
//! a thin binary that talks to a RUNNING `vectorfs serve` daemon over HTTP.
//! It never touches the engine/server modules (lancedb / the axum router) —
//! the linker drops them, keeping this binary far smaller than `vectorfs`.
//!
//! Connection (附录E.4): `--endpoint` > `VF_ENDPOINT`; `--access-key`/
//! `--secret-key` > `VF_ACCESS_KEY`/`VF_SECRET_KEY`. The daemon's own
//! vectorfs.toml credentials are NOT auto-read (the client may be remote);
//! the failure hints point at `vectorfs init`'s printed env snippet.
//! Signing rides the server's own SigV4 stack (sigv4::signer, service
//! `s3vectors`); HTTP rides the TLS-capable embed leaf, so http and https
//! endpoints both work.
//!
//! Agent contract everywhere (§5.8): stdout = results, stderr = progress,
//! `--json` carries schema_version, structured `{code, message, hint}`
//! failures, zero interaction, idempotent verbs, deterministic output.
//!
//! Verbs (附录E.4): put / search / sources list|status / get / rm / reindex /
//! status. API fields are camelCase, flags kebab-case, one to one
//! (`--top-k` -> topK, `--max-results` -> maxResults).

use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use vectorfs::client::{self, CallError, Connection};
use vectorfs::report::{Failure, SCHEMA_VERSION, human_bytes};

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
    /// Upload files into the instance root and (by default) wait for the
    /// file layer to index them (poll GetSource to a terminal state)
    Put(PutArgs),
    /// The one retrieval verb: fan-out + merge + 回源 over the bucket's
    /// embedders
    Search(SearchArgs),
    /// List ingested files / inspect one file's status
    Sources(SourcesArgs),
    /// Download a source file's original bytes
    Get(GetArgs),
    /// Delete a source file (object + async chunk GC)
    Rm(RmArgs),
    /// Trigger an explicit re-embed of a bucket (after pipeline changes)
    Reindex(ReindexArgs),
    /// /health + ingest status summary of the daemon
    Status(StatusArgs),
}

/// Connection knobs shared by every verb (flag > env, client.rs). They ride
/// the VERB (`vf put --endpoint …`, matching the M2-T1 `vf status
/// --endpoint` convention) — env vars cover the scripting case.
#[derive(Args, Debug)]
struct CommonArgs {
    /// Daemon endpoint [env: VF_ENDPOINT]
    #[arg(long)]
    endpoint: Option<String>,

    /// Static access key [env: VF_ACCESS_KEY]
    #[arg(long)]
    access_key: Option<String>,

    /// Static secret key [env: VF_SECRET_KEY]
    #[arg(long)]
    secret_key: Option<String>,

    /// Region the daemon signs for [env: VF_REGION] [default: us-east-1]
    #[arg(long)]
    region: Option<String>,
}

impl CommonArgs {
    fn connect(&self) -> Result<Connection, Failure> {
        Connection::resolve(
            self.endpoint.clone(),
            self.access_key.clone(),
            self.secret_key.clone(),
            self.region.clone(),
        )
    }
}

#[derive(Args, Debug)]
struct PutArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Files to upload (looped; --key only valid with exactly one)
    files: Vec<std::path::PathBuf>,

    /// Vector bucket (its [ingest.<bucket>] section owns the file)
    #[arg(long)]
    bucket: String,

    /// Object key (default: the file's name)
    #[arg(long)]
    key: Option<String>,

    /// Return as soon as the daemon accepts the upload (202), do not wait
    /// for indexing
    #[arg(long)]
    r#async: bool,

    /// Per-file foreground wait budget in seconds [default: 300]
    #[arg(long, default_value_t = 300)]
    timeout: u64,
}

#[derive(Args, Debug)]
struct SearchArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Query text
    text: String,

    /// Vector bucket to search
    #[arg(long)]
    bucket: String,

    /// Results to request, 1..=1000 [default: 10]
    #[arg(long, default_value_t = 10)]
    top_k: i64,

    /// Metadata filter as JSON ($-operator syntax)
    #[arg(long)]
    filter: Option<String>,

    /// Omit chunk text from the hits
    #[arg(long)]
    no_text: bool,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct SourcesArgs {
    #[command(subcommand)]
    command: SourcesCommand,
}

#[derive(Subcommand, Debug)]
enum SourcesCommand {
    /// List ingested files (marker x live join, auto-paginated)
    List(SourcesListArgs),
    /// One file's status (statusReason / failure detail visible)
    Status(SourcesStatusArgs),
}

#[derive(Args, Debug)]
struct SourcesListArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Vector bucket
    #[arg(long)]
    bucket: String,

    /// Only this status (PENDING / IN_PROGRESS / PARTIALLY_INDEXED / INDEXED
    /// / FAILED / DELETING)
    #[arg(long)]
    status: Option<String>,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct SourcesStatusArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Object key
    key: String,

    /// Vector bucket
    #[arg(long)]
    bucket: String,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct GetArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Object key
    key: String,

    /// Vector bucket
    #[arg(long)]
    bucket: String,

    /// Write to this file instead of stdout
    #[arg(long)]
    out: Option<std::path::PathBuf>,
}

#[derive(Args, Debug)]
struct RmArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Object key
    key: String,

    /// Vector bucket
    #[arg(long)]
    bucket: String,
}

#[derive(Args, Debug)]
struct ReindexArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Vector bucket
    #[arg(long)]
    bucket: String,
}

#[derive(Args, Debug)]
struct StatusArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() {
    match Cli::parse().command {
        Command::Put(args) => put_cmd(args).await,
        Command::Search(args) => search_cmd(args).await,
        Command::Sources(args) => match args.command {
            SourcesCommand::List(a) => sources_list_cmd(a).await,
            SourcesCommand::Status(a) => sources_status_cmd(a).await,
        },
        Command::Get(args) => get_cmd(args).await,
        Command::Rm(args) => rm_cmd(args).await,
        Command::Reindex(args) => reindex_cmd(args).await,
        Command::Status(args) => status_cmd(args).await,
    }
}

/// Map a wire/transport failure to the structured triple and exit 1.
fn die(e: CallError, endpoint: &str) -> ! {
    e.to_failure(endpoint).fail(false, 1)
}

// ---------------------------------------------------------------------------
// put
// ---------------------------------------------------------------------------

async fn put_cmd(args: PutArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    if args.key.is_some() && args.files.len() != 1 {
        Failure::new(
            "USAGE",
            "--key cannot be combined with multiple files",
            "drop --key (keys default to each file's name) or pass exactly one file",
        )
        .fail(false, 2);
    }
    if args.files.is_empty() {
        Failure::new("USAGE", "no files given", "pass at least one file path").fail(false, 2);
    }

    let mut any_failed = false;
    for file in &args.files {
        let bytes = match tokio::fs::read(file).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vf: cannot read {}: {e}", file.display());
                any_failed = true;
                continue;
            }
        };
        // default key = the file's NAME (aws s3 cp single-file convention);
        // directory structure is the caller's choice via --key
        let key = args.key.clone().unwrap_or_else(|| {
            file.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string())
        });

        let headers = vec![
            ("x-vf-bucket".to_string(), args.bucket.clone()),
            (
                "x-vf-file-name".to_string(),
                client::encode_header_value(&key),
            ),
        ];
        let (_, _, body) = match conn
            .call(
                "/x-vf/PutSource",
                bytes,
                &headers,
                "application/octet-stream",
            )
            .await
        {
            Ok(r) => r,
            Err(e) => die(e, conn.endpoint()),
        };
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if args.r#async {
            eprintln!("vf: {} -> accepted", file.display());
            println!("{}\t{}", v["status"].as_str().unwrap_or("PENDING"), key);
            continue;
        }

        // foreground: poll GetSource to a terminal state (one stderr line
        // per file, per 附录E.4)
        let started = Instant::now();
        let deadline = started + Duration::from_secs(args.timeout);
        let mut terminal: Option<(String, Value)> = None;
        loop {
            let s = match conn
                .call_json("GetSource", &json!({"bucket": args.bucket, "key": key}))
                .await
            {
                Ok(v) => v,
                Err(e) => die(e, conn.endpoint()),
            };
            let status = s["status"].as_str().unwrap_or_default().to_string();
            if matches!(
                status.as_str(),
                "INDEXED" | "PARTIALLY_INDEXED" | "FAILED" | "DELETING"
            ) {
                terminal = Some((status, s));
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        match terminal {
            Some((status, v)) => {
                let chunks = v["chunks"][0]["count"].as_u64().unwrap_or(0);
                let reason = v["statusReason"].as_str().unwrap_or("");
                eprintln!(
                    "vf: {} -> {} ({}, {:.1}s)",
                    key,
                    status,
                    if chunks > 0 {
                        format!("{chunks} chunks")
                    } else {
                        "no chunks".into()
                    },
                    started.elapsed().as_secs_f32()
                );
                if status == "FAILED" {
                    eprintln!("vf: statusReason: {reason}");
                    any_failed = true;
                } else if status == "PARTIALLY_INDEXED" {
                    eprintln!("vf: partial: {reason}");
                }
                println!("{status}\t{key}");
            }
            None => {
                eprintln!(
                    "vf: {key} not terminal after {}s (last status polled: PENDING/IN_PROGRESS)",
                    args.timeout
                );
                any_failed = true;
            }
        }
    }
    if any_failed {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

async fn search_cmd(args: SearchArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let filter: Option<Value> = match &args.filter {
        None => None,
        Some(f) => match serde_json::from_str(f) {
            Ok(v) => Some(v),
            Err(e) => Failure::new(
                "USAGE",
                format!("--filter is not valid JSON: {e}"),
                "pass a JSON object of $-operator filters, e.g. '{\"k\":{\"$eq\":\"v\"}}'",
            )
            .fail(args.json, 2),
        },
    };
    let mut body = json!({
        "bucket": args.bucket,
        "query": { "text": args.text },
        "topK": args.top_k,
        "returnText": !args.no_text,
    });
    if let Some(f) = filter {
        body["filter"] = f;
    }
    let v = match conn.call_json("Search", &body).await {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "bucket": args.bucket,
                "query": { "text": args.text },
                "topK": args.top_k,
                "hits": hits,
            }))
            .expect("serializes")
        );
        return;
    }
    // human table (deterministic: server order is score desc, key asc)
    let header = format!(
        "{:<4} {:<9} {:<28} {:<22} {:<24} {}",
        "#", "SCORE", "KEY", "INDEX", "SOURCE", "TEXT"
    );
    println!("{header}");
    for (i, h) in hits.iter().enumerate() {
        let text = h["content"]["text"]
            .as_str()
            .unwrap_or("")
            .replace(['\n', '\r', '\t'], " ");
        let text: String = text.chars().take(60).collect();
        println!(
            "{:<4} {:<9.4} {:<28} {:<22} {:<24} {}",
            i + 1,
            h["score"].as_f64().unwrap_or(0.0),
            truncate(h["key"].as_str().unwrap_or(""), 28),
            truncate(h["index"].as_str().unwrap_or(""), 22),
            truncate(h["location"]["key"].as_str().unwrap_or(""), 24),
            text
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

// ---------------------------------------------------------------------------
// sources list / status
// ---------------------------------------------------------------------------

async fn sources_list_cmd(args: SourcesListArgs) {
    let conn = args
        .common
        .connect()
        .unwrap_or_else(|f| f.fail(args.json, 2));
    let mut rows: Vec<Value> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut body = json!({"bucket": args.bucket, "maxResults": 1000});
        if let Some(s) = &args.status {
            body["status"] = json!(s);
        }
        if let Some(t) = &token {
            body["nextToken"] = json!(t);
        }
        let v = match conn.call_json("ListSources", &body).await {
            Ok(v) => v,
            Err(e) => die(e, conn.endpoint()),
        };
        rows.extend(v["sources"].as_array().cloned().unwrap_or_default());
        token = v["nextToken"].as_str().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "bucket": args.bucket,
                "count": rows.len(),
                "sources": rows,
            }))
            .expect("serializes")
        );
        return;
    }
    let header = format!(
        "{:<40} {:<20} {:<8} {:<10} {}",
        "KEY", "STATUS", "CHUNKS", "SIZE", "UPDATED"
    );
    println!("{header}");
    for s in &rows {
        let chunks = s["chunks"][0]["count"].as_u64().unwrap_or(0);
        println!(
            "{:<40} {:<20} {:<8} {:<10} {}",
            truncate(s["key"].as_str().unwrap_or(""), 40),
            s["status"].as_str().unwrap_or(""),
            chunks,
            s["size"]
                .as_u64()
                .map(human_bytes)
                .unwrap_or_else(|| "-".into()),
            s["updatedAt"].as_str().unwrap_or(""),
        );
    }
}

async fn sources_status_cmd(args: SourcesStatusArgs) {
    let conn = args
        .common
        .connect()
        .unwrap_or_else(|f| f.fail(args.json, 2));
    let v = match conn
        .call_json(
            "GetSource",
            &json!({"bucket": args.bucket, "key": args.key}),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "bucket": args.bucket,
                "key": args.key,
                "source": v,
            }))
            .expect("serializes")
        );
        return;
    }
    println!("key                  : {}", args.key);
    println!(
        "status               : {}",
        v["status"].as_str().unwrap_or("")
    );
    if let Some(r) = v["statusReason"].as_str() {
        println!("statusReason         : {r}");
    }
    println!("retriesExhausted     : {}", v["retriesExhausted"]);
    if let Some(m) = v["mime"].as_str() {
        println!("mime                 : {m}");
    }
    if let Some(e) = v["etag"].as_str() {
        println!("etag                 : {e}");
    }
    println!(
        "pipelineFingerprint  : {}",
        v["pipelineFingerprint"].as_str().unwrap_or("")
    );
    if let Some(chunks) = v["chunks"].as_array() {
        for c in chunks {
            println!(
                "chunks               : index {} count {}",
                c["index"], c["count"]
            );
        }
    }
    if let Some(at) = v["updatedAt"].as_str() {
        println!("updatedAt            : {at}");
    }
}

// ---------------------------------------------------------------------------
// get / rm / reindex / status
// ---------------------------------------------------------------------------

async fn get_cmd(args: GetArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let body =
        serde_json::to_vec(&json!({"bucket": args.bucket, "key": args.key})).expect("serializes");
    let (_, content_type, bytes) = match conn
        .call("/x-vf/GetSourceData", body, &[], "application/json")
        .await
    {
        Ok(r) => r,
        Err(e) => die(e, conn.endpoint()),
    };
    match &args.out {
        Some(path) => {
            if let Err(e) = tokio::fs::write(path, &bytes).await {
                Failure::new(
                    "WRITE_FAILED",
                    format!("cannot write {}: {e}", path.display()),
                    "check the destination path and permissions",
                )
                .fail(false, 1);
            }
        }
        None => {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            if let Err(e) = out.write_all(&bytes) {
                Failure::new(
                    "WRITE_FAILED",
                    format!("cannot write to stdout: {e}"),
                    "is stdout redirected to a full disk or a closed pipe?",
                )
                .fail(false, 1);
            }
            out.flush().ok();
        }
    }
    eprintln!(
        "vf: got {} ({}, {content_type})",
        args.key,
        human_bytes(bytes.len() as u64)
    );
}

async fn rm_cmd(args: RmArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let v = match conn
        .call_json(
            "DeleteSource",
            &json!({"bucket": args.bucket, "key": args.key}),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    eprintln!(
        "vf: rm {} -> {} (chunk GC is asynchronous)",
        args.key, v["status"]
    );
    println!(
        "{}\t{}",
        v["status"].as_str().unwrap_or("DELETING"),
        args.key
    );
}

async fn reindex_cmd(args: ReindexArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let v = match conn
        .call_json("Reindex", &json!({"bucket": args.bucket}))
        .await
    {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    let job = v["jobId"].as_str().unwrap_or_default();
    eprintln!(
        "vf: reindex of {} queued ({job}) — poll with: vf status",
        args.bucket
    );
    println!("{job}");
}

async fn status_cmd(args: StatusArgs) {
    let conn = args
        .common
        .connect()
        .unwrap_or_else(|f| f.fail(args.json, 2));
    let health = match conn.get_health().await {
        Ok(h) => h,
        Err(f) => f.fail(args.json, 1),
    };
    let ingest = match conn.call_json("GetIngestStatus", &json!({})).await {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "endpoint": conn.endpoint(),
                "health": health,
                "ingest": ingest,
            }))
            .expect("serializes")
        );
        return;
    }
    println!("vf status (schema v{SCHEMA_VERSION})");
    println!("  endpoint : {}", conn.endpoint());
    println!(
        "  health   : {} (version {})",
        health["ok"],
        health["version"].as_str().unwrap_or("?")
    );
    let buckets = ingest["buckets"].as_array().cloned().unwrap_or_default();
    if buckets.is_empty() {
        println!("  ingest   : none configured (pure protocol face)");
        return;
    }
    for b in &buckets {
        let counts = &b["counts"];
        println!(
            "  bucket {} : INDEXED={} PARTIAL={} PENDING={} IN_PROGRESS={} FAILED={} DELETING={}",
            b["bucket"].as_str().unwrap_or("?"),
            counts["INDEXED"],
            counts["PARTIALLY_INDEXED"],
            counts["PENDING"],
            counts["IN_PROGRESS"],
            counts["FAILED"],
            counts["DELETING"],
        );
        if let Some(at) = b["lastScanAt"].as_str() {
            println!("    lastScan : {at}");
        }
        let s = &b["stats"];
        if s.is_object() {
            println!(
                "    stats    : scanned={} new={} modified={} deleted={} failed={}",
                s["numberOfDocumentsScanned"],
                s["numberOfNewDocumentsIndexed"],
                s["numberOfModifiedDocumentsIndexed"],
                s["numberOfDocumentsDeleted"],
                s["numberOfDocumentsFailed"],
            );
        }
        println!(
            "    fingerprint : {}",
            b["pipelineFingerprint"].as_str().unwrap_or("")
        );
        for ix in b["derivedIndexes"].as_array().cloned().unwrap_or_default() {
            println!(
                "    index {} (embedder={}, model={}, dim={})",
                ix["index"].as_str().unwrap_or("?"),
                ix["embedder"].as_str().unwrap_or("?"),
                ix["model"].as_str().unwrap_or("?"),
                ix["dim"]
            );
        }
        if let Some(r) = b["reindex"].as_object() {
            println!(
                "    reindex : {} ({} of {} done)",
                r["status"].as_str().unwrap_or("?"),
                r["progress"]["done"],
                r["progress"]["total"]
            );
        }
    }
}
