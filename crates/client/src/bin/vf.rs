//! `vf` — the vectorfs CLIENT CLI (design.md §0 command ruling + 附录E.4):
//! a thin binary that talks to a RUNNING `vectorfs serve` daemon over HTTP.
//! It never touches the engine/server modules (lancedb / the axum router) —
//! the linker drops them, keeping this binary far smaller than `vectorfs`.
//!
//! Connection (附录E.4 + the 2026-11-05 config ruling): `--endpoint` >
//! `VF_ENDPOINT` > `~/.config/vectorfs/config.toml` (same chain for the
//! other knobs; `vf config init` writes that file, seeded from flags/env).
//! The daemon's own vectorfs.toml credentials are NOT auto-read (the client
//! may be remote); the failure hints point at `vectorfs init`'s printed env
//! snippet.
//! Signing rides the server's own SigV4 stack (sigv4::signer, service
//! `s3vectors`); HTTP rides the TLS-capable embed leaf, so http and https
//! endpoints both work.
//!
//! Agent contract everywhere (§5.8): stdout = results, stderr = progress,
//! `--json` carries schema_version, structured `{code, message, hint}`
//! failures, zero interaction, idempotent verbs, deterministic output.
//!
//! Verbs (附录E.4): put / search / sources list|status / get / rm / reindex /
//! status — plus `mcp`, the stdio MCP server that exposes the same verbs to
//! agents (client/src/mcp.rs). API fields are camelCase, flags kebab-case,
//! one to one (`--top-k` -> topK, `--max-results` -> maxResults).
//! `vf search` takes its query as positional TEXT (M2) or `--image <file>`
//! (M3-T4, 以图搜图: bytes+mime derived from the extension ride
//! query.image; with --image the lone positional names the bucket).

use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use vectorfs_client::{self as client, CallError, Connection};
use vectorfs_common::report::{Failure, SCHEMA_VERSION, human_bytes};

fn version_str() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(vectorfs_common::report::version_string)
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
    /// file layer to index them (poll GetSource to a terminal state).
    /// S3-CLI-style destination: the LAST positional ending in `/` names the
    /// bucket[/prefix] (`vf put a.txt docs/`, `vf put a.txt docs/pre/` ->
    /// key `pre/a.txt`); `--bucket`/`--key` remain the explicit form
    Put(PutArgs),
    /// The one retrieval verb: fan-out + merge + 回源 over the bucket's
    /// embedders. Query = positional TEXT (`vf search "q" docs`) or a local
    /// image file (`vf search --image photo.png docs`, 以图搜图 — exactly one
    /// of the two). Bucket as a bare positional or via `--bucket`
    Search(SearchArgs),
    /// List ingested files / inspect one file's status (the verb forms; the
    /// `vf ls` shorthand covers listing)
    Sources(SourcesArgs),
    /// Download a source file's original bytes. S3-CLI-style path:
    /// `vf get docs/hello.pdf` (first `/` splits bucket from key);
    /// `--bucket` + bare key remains the explicit form
    Get(GetArgs),
    /// Print a source file's bytes to stdout (the `get` stdout form, S3-CLI
    /// convention): `vf cat docs/hello.pdf`
    Cat(CatArgs),
    /// Delete a source file (object + async chunk GC). S3-CLI-style path:
    /// `vf rm docs/sample.md`; `--bucket` + bare key remains the explicit
    /// form
    Rm(RmArgs),
    /// List ingested buckets / a bucket's sources, S3-CLI style: no argument
    /// = the ingest buckets (`aws s3 ls` shape); `vf ls docs` / `vf ls
    /// docs/pre` = sources (prefix filtered client-side); `--status` filters
    Ls(LsArgs),
    /// Trigger an explicit re-embed of a bucket (after pipeline changes).
    /// Bucket as a bare positional (`vf reindex docs`) or via `--bucket`
    Reindex(ReindexArgs),
    /// /health + ingest status summary of the daemon
    Status(StatusArgs),
    /// Run the stdio MCP server (agent remote mode: every tool call proxies
    /// to the daemon over the same signed HTTP as the verbs above — vf
    /// holds no lock, the daemon owns the data)
    ///
    /// Examples (for an MCP client config):
    ///
    ///   vf mcp
    ///
    ///   vf mcp --tools ro
    Mcp(McpArgs),
    /// Manage `~/.config/vectorfs/config.toml` — the file layer of the
    /// connection chain (flag > env > file)
    Config(ConfigArgs),
}

/// Connection knobs shared by every verb (flag > env > config.toml,
/// client.rs / client config.rs). They ride the VERB (`vf put --endpoint …`,
/// matching the M2-T1 `vf status --endpoint` convention) — env vars cover
/// the scripting case, `vf config init` the interactive one.
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

    /// Files to upload — a LAST positional ending in `/` is the DESTINATION
    /// bucket[/prefix], not a file (`vf put a.txt docs/`, `vf put a.txt
    /// docs/pre/`); looped; --key only valid with exactly one
    files: Vec<std::path::PathBuf>,

    /// Vector bucket (its [ingest.<bucket>] section owns the file); omit
    /// when a `bucket[/prefix]/` destination positional is given
    #[arg(long)]
    bucket: Option<String>,

    /// Object key (default: the file's name, prefixed by a destination
    /// prefix if one was given)
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

    /// Query text (exactly one of this and --image; with --image the lone
    /// positional is the BUCKET instead)
    text: Option<String>,

    /// Query IMAGE (以图搜图): a local jpeg/png file — the search uses its
    /// pixels, matching the bucket's image index (siglip2). With --image,
    /// `vf search --image <file> <bucket>` (the lone positional names the
    /// bucket)
    #[arg(long)]
    image: Option<std::path::PathBuf>,

    /// Vector bucket to search (bare name positional, S3-CLI style)
    bucket_pos: Option<String>,

    /// Vector bucket to search (explicit form; mutually exclusive with the
    /// positional)
    #[arg(long)]
    bucket: Option<String>,

    /// Results to request, 1..=1000 [default: 10]
    #[arg(long, default_value_t = 10)]
    top_k: i64,

    /// Metadata filter as JSON ($-operator syntax)
    #[arg(long)]
    filter: Option<String>,

    /// Omit chunk text from the hits
    #[arg(long)]
    no_text: bool,

    /// Retrieval mode (M3-T9): semantic (default — embedding fan-out),
    /// keyword (pure BM25 full-text match; needs the bucket's fts = true
    /// and `vectorfs optimize`), hybrid (both arms fused by the bucket's
    /// merge strategy). Text queries only.
    #[arg(long, default_value_t = SearchModeArg::Semantic)]
    mode: SearchModeArg,

    /// Cross-encoder rerank (M3-T11): re-score the topK hits with the
    /// bucket's gte reranker and return them in rerank order (the raw
    /// logit as the score; distance is omitted). Needs the bucket's
    /// `rerank = true` in vectorfs.toml and a text query; not applicable
    /// to --mode keyword.
    #[arg(long)]
    rerank: bool,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

/// The `--mode` enum (M3-T9): clap's ValueEnum turns an invalid value into
/// a USAGE error (exit 2) — the client-side error face of the wire's
/// searchMode validation.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum SearchModeArg {
    Semantic,
    Keyword,
    Hybrid,
}

impl std::fmt::Display for SearchModeArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire())
    }
}

impl SearchModeArg {
    fn wire(&self) -> &'static str {
        match self {
            SearchModeArg::Semantic => "semantic",
            SearchModeArg::Keyword => "keyword",
            SearchModeArg::Hybrid => "hybrid",
        }
    }
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

    /// Source as `bucket/key` (S3-CLI style path) or a bare key alongside
    /// `--bucket`
    key: Option<String>,

    /// Vector bucket (explicit form; mutually exclusive with the path
    /// positional)
    #[arg(long)]
    bucket: Option<String>,

    /// Write to this file instead of stdout
    #[arg(long)]
    out: Option<std::path::PathBuf>,
}

/// `vf cat <bucket/key>` — the get stdout form, spelled like every S3 CLI.
#[derive(Args, Debug)]
struct CatArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Source as `bucket/key` (S3-CLI style path) or a bare key alongside
    /// `--bucket`
    key: Option<String>,

    /// Vector bucket (explicit form; mutually exclusive with the path
    /// positional)
    #[arg(long)]
    bucket: Option<String>,
}

#[derive(Args, Debug)]
struct RmArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Source as `bucket/key` (S3-CLI style path) or a bare key alongside
    /// `--bucket`
    key: Option<String>,

    /// Vector bucket (explicit form; mutually exclusive with the path
    /// positional)
    #[arg(long)]
    bucket: Option<String>,
}

/// `vf ls [bucket[/prefix]]` — the S3-CLI listing shape over the ingest
/// face: no argument lists the daemon's ingest buckets; `docs` / `docs/`
/// lists its sources; `docs/pre` filters by key prefix (client-side).
#[derive(Args, Debug)]
struct LsArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// bucket[/prefix] — omit to list the ingest buckets themselves
    path: Option<String>,

    /// Only this status (PENDING / IN_PROGRESS / PARTIALLY_INDEXED / INDEXED
    /// / FAILED / DELETING)
    #[arg(long)]
    status: Option<String>,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ReindexArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Vector bucket (bare name positional, S3-CLI style)
    bucket_pos: Option<String>,

    /// Vector bucket (explicit form; mutually exclusive with the positional)
    #[arg(long)]
    bucket: Option<String>,
}

#[derive(Args, Debug)]
struct StatusArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

/// `vf mcp` — the agent-facing stdio MCP server. Connection knobs resolve
/// exactly like every verb (flag > env > config.toml), so an MCP client
/// config usually needs no args at all once `vf config init` ran.
#[derive(Args, Debug)]
struct McpArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Tool set: `ro` = search/list/get only; `rw` adds
    /// put/delete/reindex [default: rw]
    #[arg(long, default_value = "rw")]
    tools: client::mcp::ToolsMode,
}

/// `vf config init` — write `~/.config/vectorfs/config.toml` (XDG-aware)
/// seeded from the CommonArgs flags over the VF_* env (`vf config init` with
/// VF_ENDPOINT exported bakes that endpoint in). Absent knobs land as
/// commented placeholders. The file is 0600 (it carries the secret key) and
/// an existing one is refused without --force.
#[derive(Args, Debug)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Create the config file from current flags/env (refuses to overwrite;
    /// --force regenerates)
    Init(ConfigInitArgs),
}

#[derive(Args, Debug)]
struct ConfigInitArgs {
    /// Seed values — the same knobs every verb takes (flag > env per knob)
    #[command(flatten)]
    common: CommonArgs,

    /// Overwrite an existing config file
    #[arg(long)]
    force: bool,
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
        Command::Cat(args) => cat_cmd(args).await,
        Command::Rm(args) => rm_cmd(args).await,
        Command::Ls(args) => ls_cmd(args).await,
        Command::Reindex(args) => reindex_cmd(args).await,
        Command::Status(args) => status_cmd(args).await,
        Command::Mcp(args) => mcp_cmd(args).await,
        Command::Config(args) => match args.command {
            ConfigCommand::Init(a) => config_init_cmd(a),
        },
    }
}

// ---------------------------------------------------------------------------
// S3-CLI-style argument resolution (2026-11-05 adjudication: bucket/key
// PATHS alongside the --bucket flag forms — the borrowed conventions are
// `aws s3`/`mc`/`s5cmd` shapes; the flags stay for scripts and muscle
// memory, and mixing the two forms for the same target is a USAGE error,
// never a silent precedence)
// ---------------------------------------------------------------------------

/// Split a `bucket/key…` path at the FIRST `/` (S3 semantics: the bucket
/// name never contains one). `None` = not a path (no `/`, or an empty side).
fn split_bucket_path(path: &str) -> Option<(&str, &str)> {
    let (bucket, key) = path.split_once('/')?;
    (!bucket.is_empty() && !key.is_empty()).then(|| (bucket, key))
}

/// Resolve a key-carrying verb's target from its positional (+ optional
/// `--bucket` flag). Precedence, chosen for BACKWARD COMPATIBILITY with the
/// documented flag form (keys may legally contain `/` — e.g. `get
/// docs/fox.txt --bucket docs` addresses the KEY `docs/fox.txt`):
///
/// * `--bucket` present ⇒ the positional is the FULL KEY, verbatim, whatever
///   it contains (the explicit flag wins; this is the pre-S3-forms contract).
/// * `--bucket` absent ⇒ the positional must be a `bucket/key` PATH, split
///   at the FIRST `/` — keys that THEMSELVES look like `bucket/key` are
///   addressable through the flag form, by this same precedence.
fn resolve_key_target(path: Option<&str>, flag: Option<&str>, verb: &str) -> (String, String) {
    let Some(p) = path else {
        Failure::new(
            "USAGE",
            format!("no target given for {verb}"),
            format!("use '{verb} <bucket>/<key>' or a bare key with --bucket"),
        )
        .fail(false, 2)
    };
    match flag {
        // explicit form (pre-S3-forms contract, unchanged): the positional
        // is the key verbatim — `vf get docs/fox.txt --bucket docs`
        Some(b) => (b.to_string(), p.to_string()),
        // path form: `vf get docs/hello.pdf`
        None => match split_bucket_path(p) {
            Some((b, k)) => (b.to_string(), k.to_string()),
            None => Failure::new(
                "USAGE",
                format!("'{p}' is not a bucket/key path (no '/' to split at)"),
                format!(
                    "use '{verb} <bucket>/<key>', or pass the key with --bucket (the flag form \
                     also addresses keys that themselves contain '/')"
                ),
            )
            .fail(false, 2),
        },
    }
}

/// Resolve a bucket-only target (search/reindex): a bare-name positional
/// (trailing `/` tolerated) or `--bucket`; exactly one form.
fn resolve_bucket_target(pos: Option<&str>, flag: Option<&str>, verb: &str) -> String {
    match (pos, flag) {
        (Some(p), None) => p.trim_end_matches('/').to_string(),
        (None, Some(b)) => b.to_string(),
        (Some(_), Some(_)) => Failure::new(
            "USAGE",
            "positional bucket and --bucket are mutually exclusive",
            format!("pass the bucket once: '{verb} <bucket>' or --bucket <bucket>"),
        )
        .fail(false, 2),
        (None, None) => Failure::new(
            "USAGE",
            format!("no bucket given for {verb}"),
            format!("use '{verb} <bucket>' or --bucket <bucket>"),
        )
        .fail(false, 2),
    }
}

/// Map a wire/transport failure to the structured triple and exit 1.
fn die(e: CallError, endpoint: &str) -> ! {
    e.to_failure(endpoint).fail(false, 1)
}

// ---------------------------------------------------------------------------
// config init
// ---------------------------------------------------------------------------

/// Write the config file (stdout = the path — the result; the next-step
/// hint rides stderr with the other progress lines).
fn config_init_cmd(args: ConfigInitArgs) {
    let seeds = client::config::seed(
        args.common.endpoint.clone(),
        args.common.access_key.clone(),
        args.common.secret_key.clone(),
        args.common.region.clone(),
    );
    let path = client::config::init(&seeds, args.force).unwrap_or_else(|f| f.fail(false, 2));
    println!("{}", path.display());
    match seeds.endpoint {
        Some(_) => eprintln!("connection knobs now resolve from the file; next: vf status"),
        None => eprintln!("set `endpoint` in the file, then: vf status"),
    }
}

// ---------------------------------------------------------------------------
// put
// ---------------------------------------------------------------------------

async fn put_cmd(args: PutArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    // S3-CLI-style destination (2026-11-05): a LAST positional ending in `/`
    // is `bucket[/prefix]/`, not a file — `vf put a.txt docs/` (key a.txt),
    // `vf put a.txt docs/pre/` (key pre/a.txt); `aws s3 cp`'s trailing-slash
    // convention, adapted to vf's single implicit instance root (no s3://).
    let mut files = args.files.clone();
    let dest = files
        .last()
        .and_then(|p| p.to_str())
        .filter(|p| p.ends_with('/'))
        .map(str::to_string);
    if dest.is_some() {
        files.pop(); // the destination is not a file to read
    }
    let (dest_bucket, dest_prefix) = match &dest {
        None => (None, None),
        // strip the trailing '/', then the first `/` splits bucket/prefix
        Some(d) => {
            let bare = d.trim_end_matches('/');
            match bare.split_once('/') {
                Some((b, prefix)) => (Some(b.to_string()), Some(prefix.to_string())),
                None => (Some(bare.to_string()), None),
            }
        }
    };
    // the two bucket spellings are mutually exclusive (a second opinion on
    // the same fact is a bug, not a precedence question)
    if args.bucket.is_some() && dest_bucket.is_some() {
        Failure::new(
            "USAGE",
            "a bucket[/prefix]/ destination already names its bucket — --bucket would be a \
             second one",
            "pick one form: 'vf put <files…> <bucket>/[<prefix>/]' or --bucket <bucket>",
        )
        .fail(false, 2);
    }
    // --key names THE key; a destination PREFIX would rename it under itself
    // (pre/ + --key k = k or pre/k? both defensible = ambiguous = refused)
    if args.key.is_some() && dest_prefix.is_some() {
        Failure::new(
            "USAGE",
            "--key cannot be combined with a destination prefix",
            "drop --key (keys default to <prefix>/<file name>) or drop the prefix from the \
             destination",
        )
        .fail(false, 2);
    }
    let bucket = args
        .bucket
        .clone()
        .or_else(|| dest_bucket.clone())
        .unwrap_or_else(|| {
            Failure::new(
                "USAGE",
                "no bucket given",
                "use 'vf put <files…> <bucket>/[<prefix>/]' or --bucket <bucket>",
            )
            .fail(false, 2)
        });
    if args.key.is_some() && files.len() != 1 {
        Failure::new(
            "USAGE",
            "--key cannot be combined with multiple files",
            "drop --key (keys default to each file's name) or pass exactly one file",
        )
        .fail(false, 2);
    }
    if files.is_empty() {
        Failure::new(
            "USAGE",
            "no files given",
            "pass at least one file path before the <bucket>/[<prefix>/] destination",
        )
        .fail(false, 2);
    }

    let mut any_failed = false;
    for file in &files {
        let bytes = match tokio::fs::read(file).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vf: cannot read {}: {e}", file.display());
                any_failed = true;
                continue;
            }
        };
        // default key = the file's NAME (aws s3 cp single-file convention),
        // under the destination prefix when one was given; --key overrides
        // the name entirely (prefix + --key was refused above)
        let key = args.key.clone().unwrap_or_else(|| {
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string());
            match &dest_prefix {
                Some(prefix) => format!("{prefix}/{name}"),
                None => name,
            }
        });

        let headers = vec![
            ("x-vf-bucket".to_string(), bucket.clone()),
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
                .call_json("GetSource", &json!({"bucket": bucket, "key": key}))
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
                // sum EVERY modality's range (appendix E.2 chunks[{index,
                // count}]) — chunks[0] alone reads whichever modality sorts
                // first (BTreeMap order), undercounting mixed-modality
                // sources and reading "text=0" for image-only ones
                let chunks = v["chunks"]
                    .as_array()
                    .map(|cs| cs.iter().filter_map(|c| c["count"].as_u64()).sum::<u64>())
                    .unwrap_or(0);
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

/// Extension -> canonical container mime for `--image` (the img family's
/// three spellings; the server re-checks the label against the sniffed
/// magic, so this is the FAST fail — the wrong label never leaves the box).
fn image_mime_of(path: &std::path::Path) -> Result<&'static str, Failure> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "png" => Ok("image/png"),
        other => Err(Failure::new(
            "USAGE",
            format!("--image: '.{other}' is not a supported image extension"),
            "supported: .jpg / .jpeg / .png (the daemon cross-checks the file's \
             actual container; a renamed file with a listed extension still works)",
        )),
    }
}

async fn search_cmd(args: SearchArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    // --image MODE reinterprets the LONE positional: `vf search --image
    // p.png docs` reads docs as the BUCKET (the S3-CLI intuition — with no
    // text query there is nothing else the positional could be). Two
    // positionals beside --image stay a text+image conflict, refused below.
    let (text, bucket_pos) = match (
        args.text.clone(),
        args.bucket_pos.clone(),
        args.image.is_some(),
    ) {
        (Some(t), None, true) => (None, Some(t)),
        (t, bp, _) => (t, bp),
    };
    let bucket = resolve_bucket_target(bucket_pos.as_deref(), args.bucket.as_deref(), "search");
    // exactly one query modality: the positional text or --image (both is
    // ambiguous — the wire takes exactly one; neither is no query at all)
    let query_member = match (&text, &args.image) {
        (Some(_), Some(_)) => Failure::new(
            "USAGE",
            "a positional query text and --image are mutually exclusive",
            "pass either 'vf search \"text\" <bucket>' or 'vf search --image <file> <bucket>'",
        )
        .fail(args.json, 2),
        (None, None) => Failure::new(
            "USAGE",
            "no query given",
            "pass query text positionally or --image <file> (以图搜图)",
        )
        .fail(args.json, 2),
        (Some(t), None) => {
            if t.trim().is_empty() {
                Failure::new(
                    "USAGE",
                    "query text must be a non-empty string",
                    "pass the text positionally: vf search \"your query\" <bucket>",
                )
                .fail(args.json, 2);
            }
            json!({ "text": t })
        }
        (None, Some(path)) => {
            let mime = image_mime_of(path).unwrap_or_else(|f| f.fail(args.json, 2));
            let bytes = match tokio::fs::read(path).await {
                Ok(b) => b,
                Err(e) => Failure::new(
                    "READ_FAILED",
                    format!("cannot read {}: {e}", path.display()),
                    "check the path and permissions; --image takes a local jpeg/png file",
                )
                .fail(args.json, 1),
            };
            if bytes.is_empty() {
                Failure::new(
                    "READ_FAILED",
                    format!("{} is empty (0 bytes)", path.display()),
                    "pass a real image file — an empty file has nothing to search with",
                )
                .fail(args.json, 1);
            }
            use base64::Engine as _;
            json!({ "image": { "bytes": base64::engine::general_purpose::STANDARD.encode(bytes), "mime": mime } })
        }
    };
    // the --json envelope's query echo: the text verbatim, the image by its
    // local identity (path + derived mime — echoing megabytes of base64
    // would bury the hits)
    let query_echo = match (&text, &args.image) {
        (Some(t), _) => json!({ "text": t }),
        (None, Some(path)) => {
            json!({ "image": { "path": path.display().to_string(), "mime": image_mime_of(path).ok() } })
        }
        _ => unreachable!("resolved above"),
    };
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
        "bucket": bucket,
        "query": query_member,
        "topK": args.top_k,
        "returnText": !args.no_text,
        "searchMode": args.mode.wire(),
        "rerank": args.rerank,
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
                "bucket": bucket,
                "query": query_echo,
                "topK": args.top_k,
                "hits": hits,
            }))
            .expect("serializes")
        );
        return;
    }
    // human table (deterministic: server order is score desc, key asc);
    // image hits have no text channel — the SOURCE column carries the 回源
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
        let text = if h["content"]["type"] == json!("IMAGE") {
            "〔image〕".to_string()
        } else {
            text
        };
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
    list_sources(&conn, &args.bucket, args.status.as_deref(), None, args.json).await;
}

/// The sources-listing core shared by `sources list` and the `ls` shorthand:
/// paginate ListSources (status filter server-side, key prefix CLIENT-side —
/// the wire has no prefix parameter in M2), then the table or JSON view.
async fn list_sources(
    conn: &Connection,
    bucket: &str,
    status: Option<&str>,
    prefix: Option<&str>,
    json_out: bool,
) {
    let mut rows: Vec<Value> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut body = json!({"bucket": bucket, "maxResults": 1000});
        if let Some(s) = status {
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
    if let Some(prefix) = prefix {
        rows.retain(|s| s["key"].as_str().is_some_and(|k| k.starts_with(prefix)));
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "bucket": bucket,
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
        // same total-chunks semantics as put's terminal line (sum over the
        // per-modality ranges, not whichever modality sorts first)
        let chunks = s["chunks"]
            .as_array()
            .map(|cs| cs.iter().filter_map(|c| c["count"].as_u64()).sum::<u64>())
            .unwrap_or(0);
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

/// `vf ls [bucket[/prefix]]` — the S3-CLI listing shape: no argument lists
/// the daemon's ingest buckets (`aws s3 ls` analog over GetIngestStatus);
/// with a bucket (optional `/`-terminated) it lists that bucket's sources
/// (`ls` shares `sources list`'s rendering and --status filter); a prefix
/// past the bucket filters keys client-side.
async fn ls_cmd(args: LsArgs) {
    let conn = args
        .common
        .connect()
        .unwrap_or_else(|f| f.fail(args.json, 2));
    let Some(path) = args.path else {
        // bucket inventory: same GetIngestStatus call `vf status` uses
        let v = match conn.call_json("GetIngestStatus", &json!({})).await {
            Ok(v) => v,
            Err(e) => die(e, conn.endpoint()),
        };
        let buckets = v["buckets"].as_array().cloned().unwrap_or_default();
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "count": buckets.len(),
                    "buckets": buckets,
                }))
                .expect("serializes")
            );
            return;
        }
        for b in &buckets {
            let c = &b["counts"];
            println!(
                "{:<24} INDEXED={} PARTIAL={} PENDING={} FAILED={}",
                b["bucket"].as_str().unwrap_or("?"),
                c["INDEXED"],
                c["PARTIALLY_INDEXED"],
                c["PENDING"],
                c["FAILED"],
            );
        }
        return;
    };
    // bucket[/prefix]: the first `/` splits bucket from key prefix
    let bare = path.trim_end_matches('/');
    let (bucket, prefix) = match bare.split_once('/') {
        Some((b, p)) => (b, Some(p)),
        None => (bare, None),
    };
    if bucket.is_empty() {
        Failure::new(
            "USAGE",
            "empty bucket name",
            "use 'vf ls <bucket>[/<prefix>]'",
        )
        .fail(args.json, 2);
    }
    list_sources(&conn, bucket, args.status.as_deref(), prefix, args.json).await;
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
// get / cat / rm / reindex / status
// ---------------------------------------------------------------------------

async fn get_cmd(args: GetArgs) {
    let (bucket, key) = resolve_key_target(args.key.as_deref(), args.bucket.as_deref(), "get");
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let body = serde_json::to_vec(&json!({"bucket": bucket, "key": key})).expect("serializes");
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
        key,
        human_bytes(bytes.len() as u64)
    );
}

/// `vf cat <bucket/key>` — the get stdout form (no --out knob by design: a
/// cat that can also write files is just get with worse discoverability).
async fn cat_cmd(args: CatArgs) {
    let (bucket, key) = resolve_key_target(args.key.as_deref(), args.bucket.as_deref(), "cat");
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let body = serde_json::to_vec(&json!({"bucket": bucket, "key": key})).expect("serializes");
    let (_, _content_type, bytes) = match conn
        .call("/x-vf/GetSourceData", body, &[], "application/json")
        .await
    {
        Ok(r) => r,
        Err(e) => die(e, conn.endpoint()),
    };
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

async fn rm_cmd(args: RmArgs) {
    let (bucket, key) = resolve_key_target(args.key.as_deref(), args.bucket.as_deref(), "rm");
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let v = match conn
        .call_json("DeleteSource", &json!({"bucket": bucket, "key": key}))
        .await
    {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    eprintln!(
        "vf: rm {} -> {} (chunk GC is asynchronous)",
        key, v["status"]
    );
    println!("{}\t{}", v["status"].as_str().unwrap_or("DELETING"), key);
}

async fn reindex_cmd(args: ReindexArgs) {
    let bucket = resolve_bucket_target(
        args.bucket_pos.as_deref(),
        args.bucket.as_deref(),
        "reindex",
    );
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    let v = match conn.call_json("Reindex", &json!({"bucket": bucket})).await {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    let job = v["jobId"].as_str().unwrap_or_default();
    eprintln!("vf: reindex of {bucket} queued ({job}) — poll with: vf status");
    println!("{job}");
}

/// Engine versions under the daemon (engine observability, 2026-11-05): one
/// line per engine from the health body's `engines` object, absent silently
/// on older daemons that do not report it (the client never assumes the
/// server's vintage).
fn print_engines(health: &Value) {
    if let Some(engines) = health.get("engines").and_then(|e| e.as_object()) {
        println!("  engines  :");
        for (name, version) in engines {
            // bare string rendering (a Value's Display keeps JSON quotes)
            let v = version
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| version.to_string());
            println!("    {name:<9} {v}");
        }
    }
}

async fn status_cmd(args: StatusArgs) {
    // Health-only degradation (container HEALTHCHECK shape, §5.10.7): with
    // no credentials resolvable the full GetIngestStatus depth is
    // impossible, but "is the daemon up" must stay answerable — the image's
    // HEALTHCHECK runs `vf status --endpoint …` with NO VF_* env (the
    // daemon's credentials live in the data volume's vectorfs.toml). Fall
    // back to the unsigned GET /health probe and exit on its verdict.
    let conn = match args.common.connect() {
        Ok(c) => c,
        Err(f) if f.code == "NO_CREDENTIALS" => {
            let probe = Connection::resolve_probe(args.common.endpoint.clone())
                .unwrap_or_else(|f| f.fail(args.json, 2));
            let health = match probe.get_health().await {
                Ok(h) => h,
                Err(f) => f.fail(args.json, 1),
            };
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": SCHEMA_VERSION,
                        "endpoint": probe.endpoint(),
                        "health": health,
                        "ingest": null,
                        "note": "health-only probe: no credentials (VF_ACCESS_KEY unset)",
                    }))
                    .expect("serializes")
                );
            } else {
                println!("vf status (schema v{SCHEMA_VERSION})");
                println!("  endpoint : {}", probe.endpoint());
                println!(
                    "  health   : {} (version {})",
                    health["ok"],
                    health["version"].as_str().unwrap_or("?")
                );
                print_engines(&health);
                println!("  ingest   : (no credentials — health-only probe)");
            }
            return;
        }
        Err(f) => f.fail(args.json, 2),
    };
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
    print_engines(&health);
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

// ---------------------------------------------------------------------------
// mcp
// ---------------------------------------------------------------------------

/// `vf mcp` — the stdio MCP server for agent remote mode (design.md §5.8):
/// tool calls proxy to the daemon through the same signed Connection as
/// every verb above; this process holds no lock. stdout is the protocol
/// channel — all diagnostics ride stderr (mcp::run's contract).
async fn mcp_cmd(args: McpArgs) {
    let conn = args.common.connect().unwrap_or_else(|f| f.fail(false, 2));
    match client::mcp::run(conn, args.tools).await {
        client::mcp::McpExit::Ok => {}
        client::mcp::McpExit::Failed(msg) => Failure::new(
            "MCP_START_FAILED",
            msg,
            "check the endpoint (vf status) and that stdin/stdout are a pipe pair",
        )
        .fail(false, 2),
    }
}
