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
//! status — plus the T14 content-addressing pair `chunks` (a source's chunk
//! address space: ListVectors + client prefix filter + early-exit) and
//! `chunk show` (bounded neighborhood read, grep -C semantics) — and `mcp`,
//! the stdio MCP server that exposes the same verbs to
//! agents (client/src/mcp.rs). API fields are camelCase, flags kebab-case,
//! one to one (`--top-k` -> topK, `--max-results` -> maxResults).
//! `vf search` takes its query as positional TEXT (M2) or `--image <file>`
//! (M3-T4, 以图搜图: bytes+mime derived from the extension ride
//! query.image; with --image the lone positional names the bucket).

use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use vectorfs_client::{self as client, CallError, Connection};
use vectorfs_common::chunkkey;
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
    /// of the two). Bucket as a bare positional or via `--bucket`. Hits
    /// carry content-addressing coordinates (T13): pages (`p18-19`),
    /// lines (`L405-420`), container sections (`§3`) — shown in the COORD
    /// column and in --json under location.coord; page keys for paged
    /// sources, line keys for pageless ones, never both. NOTE: line
    /// coordinates address the EXTRACTED text (the md extractor strips
    /// heading markers, html drops markup), not the original file's bytes.
    /// `--source <key>` (repeatable) scopes the query to named source
    /// FILES — the daemon translates names to x-vf-src-uri (T15), no hash
    /// digging.
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
    /// List one source's CHUNK address space (T14, ADR 0005): the
    /// `{hash}-{modality}-{i}` keys are the vector world's line numbers.
    /// S3-CLI-style path: `vf chunks docs/hello.pdf`. Table: I (modality-i)
    /// / KIND / COORD / TEXT preview — the I column plus the printed hash
    /// prefix compose the chunk keys `vf chunk show` takes
    Chunks(ChunksArgs),
    /// The content-addressed read family (T14): `vf chunk show` reads a
    /// chunk and its REAL enumerated neighbors, bounded (grep -C semantics)
    Chunk(ChunkArgs),
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

    /// Structure kinds to ADMIT into the results (T12, repeatable):
    /// search excludes toc/legend chunks by default (TOC lines are
    /// page-repeated perfect lexical matches and霸榜 the default ranking);
    /// `--kind toc` 放行s them — the navigation query「哪章在哪页」.
    /// Allowed values: body, toc, caption, legend.
    #[arg(long = "kind")]
    kinds: Vec<String>,

    /// Restrict the search to one source FILE by its key (T15, repeatable
    /// for several — they become an $in). No hash digging: the daemon
    /// translates names to x-vf-src-uri on the Search face
    #[arg(long = "source")]
    sources: Vec<String>,

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

/// `vf chunks <bucket/key>` — one source's chunk address space (T14).
#[derive(Args, Debug)]
struct ChunksArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Source as `bucket/key` (S3-CLI style path) or a bare key alongside
    /// `--bucket`
    key: Option<String>,

    /// Vector bucket (explicit form; mutually exclusive with the path
    /// positional)
    #[arg(long)]
    bucket: Option<String>,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
}

/// `vf chunk …` — the content-addressed read family (T14).
#[derive(Args, Debug)]
struct ChunkArgs {
    #[command(subcommand)]
    command: ChunkCommand,
}

#[derive(Subcommand, Debug)]
enum ChunkCommand {
    /// Bounded neighborhood read of one chunk (grep -C semantics): the
    /// anchor plus its REAL enumerated neighbors, each clipped to --chars
    /// with a continuation marker. Reads the INDEXED view (the chunk text
    /// the pipeline stored — same source as search hits), never the raw
    /// file
    Show(ChunkShowArgs),
}

#[derive(Args, Debug)]
struct ChunkShowArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Vector bucket (bare name — the chunk key is its own address, there
    /// is no object key to path-split)
    bucket: String,

    /// Chunk key `{16hex}-{modality}-{i}` (from `vf chunks`)
    chunk_key: String,

    /// Neighborhood radius in address-space order, 0..=20 (0 = the anchor
    /// alone; the 1+2N ≤ 41-key window fits ONE GetVectors call under its
    /// 100-key cap) [default: 2]
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(0..=20))]
    around: u64,

    /// Chars shown per chunk; 0 = the FULL text of ONE chunk (requires
    /// --around 0 — a whole unbounded neighborhood would silently break the
    /// never-over-LLM-context promise) [default: 400]
    #[arg(long, default_value_t = 400)]
    chars: u64,

    /// Structured output (schema_version'd JSON)
    #[arg(long)]
    json: bool,
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
    // F7 (teachable clap errors): `vf get docs "key"` — two positionals
    // where the verb takes ONE `bucket/key` path — dies as a bare
    // "unexpected argument" that never mentions the path form. Intercept
    // the usage error and append the path-form hint for the key-carrying
    // verbs (get/cat/rm/chunks share the shape), and only when the
    // rejected token is a POSITIONAL (a rejected --flag keeps clap's own
    // error; the hint would be off-target there); every other parse error
    // takes clap's default rendering/exit untouched.
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let rendered = e.render().to_string();
            let positional = matches!(
                e.get(clap::error::ContextKind::InvalidArg),
                Some(clap::error::ContextValue::String(s)) if !s.starts_with('-')
            );
            let path_verb = ["get", "cat", "rm", "chunks"]
                .into_iter()
                .find(|verb| rendered.contains(&format!("vf {verb} ")));
            if e.kind() == clap::error::ErrorKind::UnknownArgument
                && positional
                && let Some(verb) = path_verb
            {
                eprintln!("{rendered}");
                eprintln!(
                    "note: {verb} takes ONE '<bucket>/<key>' path positional (or a bare key \
                     with --bucket): `vf {verb} docs/key.txt`"
                );
                std::process::exit(e.exit_code());
            }
            e.exit()
        }
    };
    match cli.command {
        Command::Put(args) => put_cmd(args).await,
        Command::Search(args) => search_cmd(args).await,
        Command::Sources(args) => match args.command {
            SourcesCommand::List(a) => sources_list_cmd(a).await,
            SourcesCommand::Status(a) => sources_status_cmd(a).await,
        },
        Command::Get(args) => get_cmd(args).await,
        Command::Cat(args) => cat_cmd(args).await,
        Command::Chunks(args) => chunks_cmd(args).await,
        Command::Chunk(args) => match args.command {
            ChunkCommand::Show(a) => chunk_show_cmd(a).await,
        },
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
    // --source: a SHALLOW wrapper (T15) — the translation's single source
    // of truth is the SERVER (ADR 0004); the CLI only spells the filter.
    // Combined with --filter via $and so both compose without key overlap.
    let filter = match (filter, source_filter_of(&args.sources)) {
        (f, None) => f,
        (None, s) => s,
        (Some(f), Some(s)) => Some(json!({ "$and": [f, s] })),
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
    if !args.kinds.is_empty() {
        // repeatable --kind ⇒ the wire's includeKinds array (the server
        // validates the closed vocabulary; unknown kinds die loud)
        body["includeKinds"] = json!(args.kinds);
    }
    let v = match conn.call_json("Search", &body).await {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    // T15: the server's empty-hit note (a src-name filter that matched no
    // indexed source — the typo signal); --json carries it as a member,
    // the human table reports it on stderr (progress/diagnostics channel)
    let note = v.get("note").and_then(Value::as_str).map(String::from);
    if args.json {
        let mut out = json!({
            "schema_version": SCHEMA_VERSION,
            "bucket": bucket,
            "query": query_echo,
            "topK": args.top_k,
            "hits": hits,
        });
        if let Some(n) = &note {
            out["note"] = json!(n);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&out).expect("serializes")
        );
        return;
    }
    if let Some(n) = &note {
        eprintln!("note: {n}");
    }
    // human table (deterministic: server order is score desc, key asc);
    // image hits have no text channel — the SOURCE column carries the 回源,
    // the COORD column the T13 addressing summary (p18-19 / L405-420 / §3,
    // empty when the hit carries none — see the verb doc for the md caveat)
    let header = format!(
        "{:<4} {:<9} {:<28} {:<22} {:<24} {:<13} {}",
        "#", "SCORE", "KEY", "INDEX", "SOURCE", "COORD", "TEXT"
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
            "{:<4} {:<9.4} {:<28} {:<22} {:<24} {:<13} {}",
            i + 1,
            h["score"].as_f64().unwrap_or(0.0),
            truncate(h["key"].as_str().unwrap_or(""), 28),
            truncate(h["index"].as_str().unwrap_or(""), 22),
            truncate_source(h["location"]["key"].as_str().unwrap_or(""), 24),
            coord_cell(&h["location"]["coord"]),
            text
        );
    }
}

/// The COORD cell from a hit's `location.coord` (T13): each unit in its
/// compact form — `p18-19` / `p7` (page), `L405-420` / `L12` (line), `§3`
/// (section) — space-joined when several coexist (a container's txt child
/// carries line + section). Empty string when the hit carries none.
fn coord_cell(coord: &serde_json::Value) -> String {
    let span = |unit: &str, v: &serde_json::Value| -> Option<String> {
        let (s, e) = (v["start"].as_u64()?, v["end"].as_u64()?);
        Some(if s == e {
            format!("{unit}{s}")
        } else {
            format!("{unit}{s}-{e}")
        })
    };
    let mut parts = Vec::new();
    if let Some(p) = span("p", &coord["page"]) {
        parts.push(p);
    }
    if let Some(l) = span("L", &coord["line"]) {
        parts.push(l);
    }
    if let Some(s) = coord["section"].as_u64() {
        parts.push(format!("§{s}"));
    }
    parts.join(" ")
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

/// The SOURCE column's truncation (T15, replacing head-only `truncate`):
/// long object keys keep their TAIL and lose their MIDDLE — the file name
/// and its nearest directories are the identifying part of a path
/// (`docs/reports/2026/…/radar-vol32.pdf` reads as `docs/re…ol32.pdf`,
/// never as `docs/reports/202…`). Char-domain, like `truncate`.
fn truncate_source(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max < 3 {
        // max < 3 cannot hold head + '…' + tail: degrade to head-truncation
        return truncate(s, max);
    }
    // tail-biased: a third of the budget to the head, the rest (minus the
    // ellipsis) to the tail
    let head = (max - 1) / 3;
    let tail = max - 1 - head;
    let mut out: String = s.chars().take(head).collect();
    out.push('…');
    out.extend(s.chars().skip(count - tail));
    out
}

/// The repeatable `--source` flags as a filter member (T15): one source
/// spells the bare-key shorthand `$eq`, several spell `$in` — the server's
/// name→hash translation (ADR 0004) consumes either; the CLI adds no
/// logic of its own.
fn source_filter_of(sources: &[String]) -> Option<Value> {
    match sources {
        [] => None,
        [one] => Some(json!({ "x-vf-src-name": one })),
        many => Some(json!({ "x-vf-src-name": { "$in": many } })),
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
        "{:<40} {:<20} {:<8} {:<6} {:<10} {}",
        "KEY", "STATUS", "CHUNKS", "FURN", "SIZE", "UPDATED"
    );
    println!("{header}");
    for s in &rows {
        // same total-chunks semantics as put's terminal line (sum over the
        // per-modality ranges, not whichever modality sorts first)
        let chunks = s["chunks"]
            .as_array()
            .map(|cs| cs.iter().filter_map(|c| c["count"].as_u64()).sum::<u64>())
            .unwrap_or(0);
        // T12 audit column: furniture lines this source's extraction
        // removed ("-" = no completing extraction / pre-T12 marker)
        let furniture = s["furnitureRemoved"]
            .as_u64()
            .map_or("-".into(), |n| n.to_string());
        println!(
            "{:<40} {:<20} {:<8} {:<6} {:<10} {}",
            truncate(s["key"].as_str().unwrap_or(""), 40),
            s["status"].as_str().unwrap_or(""),
            chunks,
            furniture,
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
// content addressing (T14, ADR 0005): vf chunks / vf chunk show
//
// The `{hash}-{modality}-{i}` chunk keys are a CONTRACT — the vector
// world's line numbers: ordered, enumerable, neighborhood-addressable.
// Both verbs ride the AWS data plane through the same signed Connection
// (`/ListVectors`, `/GetVectors` — the client's first data-plane
// consumers); the shape judgment is the ONE shared implementation in
// vectorfs-common::chunkkey (sigv4 同源一套 precedent, T14 sink).
// ---------------------------------------------------------------------------

/// The half-open lexical interval of ONE source's chunk keys:
/// [`{hash}-`, `{hash}.`). `'-' + 1 = '.'` and no byte sits between them,
/// so a key starts with `{hash}-` IFF `lower <= key < upper` — the interval
/// IS the prefix set, which is what makes page-level early-exit sound.
fn chunk_prefix_bounds(hash: &str) -> (String, String) {
    (format!("{hash}-"), format!("{hash}."))
}

/// Filter ONE key-ascending ListVectors page against the source's key
/// interval: the matched entries, and whether enumeration may STOP.
/// ListVectors walks an index in key order, so the FIRST key at/past
/// `upper` proves every later key — the rest of this page and every next
/// page — lies beyond the source: stop requesting pages (the scan costs
/// O(the source's key interval), never O(the whole index)).
fn scan_chunk_page<'a>(
    page: &'a [(String, Value)],
    lower: &str,
    upper: &str,
) -> (Vec<&'a (String, Value)>, bool) {
    let mut matched = Vec::new();
    for entry in page {
        if entry.0.as_str() >= upper {
            return (matched, true);
        }
        if entry.0.starts_with(lower) {
            matched.push(entry);
        }
    }
    (matched, false)
}

/// Address-space order: NUMERIC i (the chunker assigns ONE global index
/// per source — text windows and image atoms share the sequence, so a
/// mixed-modality source's keys INTERLEAVE in document order:
/// `text-0, text-1, image-2, text-3`). The enumeration returns keys in
/// lexical order, which is neither numeric (`-text-10` sorts before
/// `-text-2`) nor interleaved (`image-*` sorts before `text-*` wholesale)
/// — sort on the parsed ordinal. The modality tiebreak only total-orders
/// the impossible-tie case. Foreign keys that matched the hash interval
/// but do not carry the chunk-key shape (pure-protocol rows parked in the
/// same interval) are dropped here: they are not address-space members.
fn order_chunk_entries(entries: &mut Vec<(String, Value)>) {
    entries.retain(|(k, _)| chunkkey::parse_chunk_key(k).is_some());
    entries.sort_by_key(|(k, _)| {
        let p = chunkkey::parse_chunk_key(k).expect("retained above");
        (p.index, p.modality.to_string())
    });
}

/// The `--around` window, taken from the REAL enumerated order (**枚举
/// 优先**, ADR 0005): neighbors are whatever keys actually sit beside the
/// anchor after enumeration — cross-modal neighbors are returned AS-IS,
/// never skipped and never guessed. (A guessing scheme — the anchor's key
/// with i±1 — systematically misses real neighbors in mixed-modality
/// sources, where the text/image i counters interleave in document order;
/// that failure mode is exactly what enumeration-first exists to prevent.)
fn chunk_window<'a>(sorted: &'a [String], anchor: &str, around: usize) -> Option<&'a [String]> {
    let pos = sorted.iter().position(|k| k == anchor)?;
    let lo = pos.saturating_sub(around);
    let hi = (pos + around + 1).min(sorted.len());
    Some(&sorted[lo..hi])
}

/// The next key after `key` in address-space order (the truncation
/// marker's continuation pointer — the window an agent opens next).
fn next_chunk_key<'a>(sorted: &'a [String], key: &str) -> Option<&'a str> {
    let pos = sorted.iter().position(|k| k == key)?;
    sorted.get(pos + 1).map(String::as_str)
}

/// Bounded body: the first `chars` CHARS of the text (char discipline — a
/// CJK glyph is never halved) plus a truncation flag. `chars == 0` = full
/// text; the caller has already enforced the `--chars 0` × `--around > 0`
/// mutex (chunk size is a pipeline config with no upper bound — 41 × full
/// text would silently break the never-over-LLM-context promise).
fn bound_chunk_text(text: &str, chars: u64) -> (String, bool) {
    if chars == 0 || text.chars().count() as u64 <= chars {
        return (text.to_string(), false);
    }
    (text.chars().take(chars as usize).collect(), true)
}

/// The coord summary from a vector's metadata (the same `{page?, line?,
/// section?}` shape `vf search --json` reports under hit.location.coord —
/// T13): only the units that exist, mirroring the stamped
/// `x-vf-page-start/end` / `x-vf-line-start/end` / `x-vf-section` keys.
fn coord_obj(metadata: &Value) -> Value {
    let span = |start_key: &str, end_key: &str| -> Option<Value> {
        Some(json!({
            "start": metadata[start_key].as_u64()?,
            "end": metadata[end_key].as_u64()?,
        }))
    };
    let mut o = json!({});
    if let Some(p) = span("x-vf-page-start", "x-vf-page-end") {
        o["page"] = p;
    }
    if let Some(l) = span("x-vf-line-start", "x-vf-line-end") {
        o["line"] = l;
    }
    if let Some(s) = metadata["x-vf-section"].as_u64() {
        o["section"] = json!(s);
    }
    o
}

/// The bucket's derived indexes from GetIngestStatus: `(indexName,
/// embedder)` pairs in config order. Every derived index mirrors the same
/// chunk identity, and the UNION across them is the complete address space
/// — each embedder only holds the modalities its doc side accepts (a
/// text-only embedder's index has no image chunks however image-rich the
/// source is), so a single-index read would under-report mixed sources.
async fn derived_indexes(
    conn: &Connection,
    bucket: &str,
) -> Result<Vec<(String, String)>, Failure> {
    let v = match conn
        .call_json("GetIngestStatus", &json!({ "bucket": bucket }))
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(e.to_failure(conn.endpoint())),
    };
    let Some(entry) = v["buckets"].as_array().and_then(|a| a.first()) else {
        return Err(Failure::new(
            "NotFound",
            format!("bucket '{bucket}' is not an ingest bucket on this daemon"),
            "list the daemon's ingest buckets with: vf ls",
        ));
    };
    let indexes: Vec<(String, String)> = entry["derivedIndexes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|ix| {
            Some((
                ix["index"].as_str()?.to_string(),
                ix["embedder"].as_str().unwrap_or("?").to_string(),
            ))
        })
        .collect();
    if indexes.is_empty() {
        return Err(Failure::new(
            "NotFound",
            format!("bucket '{bucket}' reports no derived indexes"),
            "an ingest bucket needs at least one embedder in its [ingest.{bucket}] section",
        ));
    }
    Ok(indexes)
}

/// ONE derived index's contribution to the source's address space:
/// ListVectors pages (maxResults 1000, the protocol face's page shape)
/// filtered against the hash interval with early-exit. A NotFoundException
/// means the index exists in the plan but not on disk yet (nothing has
/// written to it) — an empty contribution, not an error.
async fn enumerate_index_chunks(
    conn: &Connection,
    bucket: &str,
    index: &str,
    hash: &str,
) -> Result<Vec<(String, Value)>, CallError> {
    let (lower, upper) = chunk_prefix_bounds(hash);
    let mut out: Vec<(String, Value)> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut body = json!({
            "vectorBucketName": bucket,
            "indexName": index,
            "maxResults": 1000,
            "returnMetadata": true,
        });
        if let Some(t) = &token {
            body["nextToken"] = json!(t);
        }
        let v = match conn.call_json_path("/ListVectors", &body).await {
            Ok(v) => v,
            Err(CallError::Http { shape, .. }) if shape == "NotFoundException" => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let page: Vec<(String, Value)> = v["vectors"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                (
                    e["key"].as_str().unwrap_or_default().to_string(),
                    e.get("metadata").cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        let (matched, stop) = scan_chunk_page(&page, &lower, &upper);
        out.extend(matched.into_iter().cloned());
        if stop {
            break;
        }
        token = v["nextToken"].as_str().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    Ok(out)
}

/// The source's COMPLETE address space: the union of every derived index's
/// enumeration (first index wins on the rare same-key overlap — the chunk
/// identity is the key, only the model metadata differs), sorted into
/// address-space order. Takes the key HASH (both callers already hold it —
/// `vf chunks` from the object key, `chunk show` from the anchor key).
async fn enumerate_source_chunks(
    conn: &Connection,
    bucket: &str,
    indexes: &[(String, String)],
    hash: &str,
    json_out: bool,
) -> Vec<(String, Value)> {
    let mut merged = std::collections::HashMap::<String, Value>::new();
    for (index, _) in indexes {
        for (k, m) in match enumerate_index_chunks(conn, bucket, index, hash).await {
            Ok(entries) => entries,
            Err(e) => die(e, conn.endpoint()),
        } {
            merged.entry(k).or_insert(m);
        }
        if !json_out {
            eprintln!("vf: enumerated {}/{}", bucket, index);
        }
    }
    let mut entries: Vec<(String, Value)> = merged.into_iter().collect();
    order_chunk_entries(&mut entries);
    entries
}

/// `vf chunks <bucket/key>` — the address-space listing: what `vf search`
/// can address, in the order the address space defines it.
async fn chunks_cmd(args: ChunksArgs) {
    let (bucket, key) = resolve_key_target(args.key.as_deref(), args.bucket.as_deref(), "chunks");
    let conn = args
        .common
        .connect()
        .unwrap_or_else(|f| f.fail(args.json, 2));
    // the source must exist (a 404 here names the real problem — an
    // unuploaded key — instead of a silently empty address space)
    let src = match conn
        .call_json("GetSource", &json!({"bucket": bucket, "key": key}))
        .await
    {
        Ok(v) => v,
        Err(e) => die(e, conn.endpoint()),
    };
    let indexes = match derived_indexes(&conn, &bucket).await {
        Ok(ix) => ix,
        Err(f) => f.fail(args.json, 1),
    };
    let hash = chunkkey::object_key_hash(&key);
    let entries = enumerate_source_chunks(&conn, &bucket, &indexes, &hash, args.json).await;
    if entries.is_empty() {
        let status = src["status"].as_str().unwrap_or("?");
        eprintln!(
            "vf: no chunks for {key} (source status: {status}) — indexing may still be in flight"
        );
    }
    // store_chunk_text = false shows up as no text anywhere; say it once on
    // stderr, both output modes (the listing is still useful — the address
    // space exists — but the read verb will refuse)
    if entries
        .iter()
        .all(|(_, m)| m["x-vf-chunk-text"].as_str().is_none())
    {
        eprintln!(
            "vf: no chunk text stored for this source (store_chunk_text = false on the daemon) — \
             `vf chunk show` will refuse to read these chunks"
        );
    }

    if args.json {
        let chunks: Vec<Value> = entries
            .iter()
            .map(|(k, m)| {
                let p = chunkkey::parse_chunk_key(k).expect("ordered entries parse");
                json!({
                    "key": k,
                    "modality": p.modality,
                    "i": p.index,
                    "kind": m["x-vf-chunk-kind"].as_str(),
                    "coord": coord_obj(m),
                    // PREVIEW only (60 chars, the search table's discipline) —
                    // the read verb is `vf chunk show`; a full-text dump of a
                    // big source's address space would bury the listing
                    "text": m["x-vf-chunk-text"].as_str().map(|t| {
                        t.replace(['\n', '\r', '\t'], " ").chars().take(60).collect::<String>()
                    }),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "bucket": bucket,
                "key": key,
                "hash": hash,
                "indexes": indexes.iter().map(|(ix, _)| ix).collect::<Vec<_>>(),
                "count": chunks.len(),
                "chunks": chunks,
            }))
            .expect("serializes")
        );
        return;
    }

    // human table (deterministic: address-space order (modality, numeric i))
    println!(
        "{key} {hash}- · {} chunks · index {}",
        entries.len(),
        indexes
            .iter()
            .map(|(ix, _)| ix.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if entries.is_empty() {
        return;
    }
    let header = format!("{:<12} {:<8} {:<13} {}", "I", "KIND", "COORD", "TEXT");
    println!("{header}");
    for (k, m) in &entries {
        let p = chunkkey::parse_chunk_key(k).expect("ordered entries parse");
        let kind = m["x-vf-chunk-kind"].as_str().unwrap_or("-");
        let coord = coord_cell(&coord_obj(m));
        let text = m["x-vf-chunk-text"]
            .as_str()
            .map(|t| t.replace(['\n', '\r', '\t'], " "))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<12} {:<8} {:<13} {}",
            format!("{}-{}", p.modality, p.index),
            kind,
            coord,
            text.chars().take(60).collect::<String>()
        );
    }
}

/// `vf chunk show <bucket> <chunk-key>` — the bounded neighborhood read.
async fn chunk_show_cmd(args: ChunkShowArgs) {
    // USAGE gates first (exit 2, before any wire traffic): the key shape and
    // the size-boundedness mutex
    let parts = chunkkey::parse_chunk_key(&args.chunk_key).unwrap_or_else(|| {
        Failure::new(
            "USAGE",
            format!("'{}' is not a chunk key", args.chunk_key),
            "chunk keys are {16-hex-object-hash}-{modality}-{i}, e.g. \
             0123456789abcdef-text-3 — list them with: vf chunks <bucket>/<key>",
        )
        .fail(args.json, 2)
    });
    if args.bucket.contains('/') {
        Failure::new(
            "USAGE",
            format!("'{}' is not a bare bucket name", args.bucket),
            "'vf chunk show' takes the BUCKET and the CHUNK KEY as two positionals: vf chunk \
             show docs 0123456789abcdef-text-3",
        )
        .fail(args.json, 2);
    }
    if args.chars == 0 && args.around > 0 {
        Failure::new(
            "USAGE",
            "--chars 0 (full text) cannot be combined with --around > 0",
            "chunk size is a pipeline config with NO upper bound — a whole neighborhood in full \
             text would break the bounded-read promise; read ONE chunk in full with \
             '--chars 0 --around 0', or keep --chars > 0 for a bounded neighborhood",
        )
        .fail(args.json, 2);
    }

    let conn = args
        .common
        .connect()
        .unwrap_or_else(|f| f.fail(args.json, 2));
    let indexes = match derived_indexes(&conn, &args.bucket).await {
        Ok(ix) => ix,
        Err(f) => f.fail(args.json, 1),
    };

    // 枚举优先: the neighborhood is defined by the REAL key order of the
    // source's address space (same enumeration `vf chunks` renders), never
    // by guessing i±1 keys around the anchor
    let entries =
        enumerate_source_chunks(&conn, &args.bucket, &indexes, parts.hash, args.json).await;
    let sorted: Vec<String> = entries.iter().map(|(k, _)| k.clone()).collect();
    let Some(window) = chunk_window(&sorted, &args.chunk_key, args.around as usize) else {
        Failure::new(
            "NotFound",
            format!(
                "chunk '{}' is not in bucket '{}''s address space",
                args.chunk_key, args.bucket
            ),
            "the key may be stale (source re-indexed) or mistyped — the address space is \
             per source file: vf chunks <bucket>/<object-key> lists the live keys",
        )
        .fail(args.json, 1);
    };
    if !args.json {
        eprintln!(
            "vf: {} chunks in the address space; window {} ..= {}",
            sorted.len(),
            window.first().map(String::as_str).unwrap_or("?"),
            window.last().map(String::as_str).unwrap_or("?")
        );
    }

    // one batched point-lookup per derived index (≤ 41 keys ≤ GetVectors'
    // 100-key cap); first index containing a key wins (the key IS the
    // chunk identity; only model metadata can differ)
    let mut by_key = std::collections::HashMap::<String, Value>::new();
    for (index, _) in &indexes {
        let body = json!({
            "vectorBucketName": args.bucket,
            "indexName": index,
            "keys": window,
            "returnMetadata": true,
            "returnData": false,
        });
        let v = match conn.call_json_path("/GetVectors", &body).await {
            Ok(v) => v,
            Err(CallError::Http { shape, .. }) if shape == "NotFoundException" => continue,
            Err(e) => die(e, conn.endpoint()),
        };
        for vec_entry in v["vectors"].as_array().cloned().unwrap_or_default() {
            if let Some(k) = vec_entry["key"].as_str() {
                by_key
                    .entry(k.to_string())
                    .or_insert_with(|| vec_entry["metadata"].clone());
            }
        }
    }

    // fail loud on store_chunk_text = false: enumerABLE but not readABLE is
    // a config state, and a silent empty body would misreport it as "the
    // chunk has no content" (the never-silent-empty ruling, ADR 0005)
    let mut blocks: Vec<(String, Value)> = Vec::with_capacity(window.len());
    for key in window {
        let Some(meta) = by_key.get(key) else {
            Failure::new(
                "NotFound",
                format!("chunk '{key}' vanished between enumeration and read"),
                "the source may have been re-indexed concurrently; retry",
            )
            .fail(args.json, 1);
        };
        if meta["x-vf-chunk-text"].as_str().is_none() {
            Failure::new(
                "NO_CHUNK_TEXT",
                format!(
                    "chunk '{key}' carries no 'x-vf-chunk-text' metadata — this index stores no \
                     chunk text"
                ),
                "the daemon's vectorfs.toml sets store_chunk_text = false for this bucket: the \
                 address space is enumerable (vf chunks) but chunks are not readable; re-index \
                 with the default store_chunk_text = true to enable bounded reads",
            )
            .fail(args.json, 1);
        }
        blocks.push((key.clone(), meta.clone()));
    }

    if args.json {
        let blocks_json: Vec<Value> = blocks
            .iter()
            .map(|(k, m)| {
                let p = chunkkey::parse_chunk_key(k).expect("window keys parse");
                let text = m["x-vf-chunk-text"].as_str().unwrap_or("");
                let (bounded, truncated) = bound_chunk_text(text, args.chars);
                json!({
                    "key": k,
                    "modality": p.modality,
                    "i": p.index,
                    "anchor": k.as_str() == args.chunk_key,
                    "kind": m["x-vf-chunk-kind"].as_str(),
                    "coord": coord_obj(m),
                    "model": m["x-vf-model"].as_str(),
                    // the text AS SHOWN (bounded); the full text only ever
                    // arrives via --chars 0 --around 0, by explicit ask
                    "text": bounded,
                    "truncated": truncated,
                    "next": if truncated { next_chunk_key(&sorted, k) } else { None },
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "bucket": args.bucket,
                "key": args.chunk_key,
                "around": args.around,
                "chars": args.chars,
                "blocks": blocks_json,
            }))
            .expect("serializes")
        );
        return;
    }

    for (n, (key, meta)) in blocks.iter().enumerate() {
        let p = chunkkey::parse_chunk_key(key).expect("window keys parse");
        let coord = coord_cell(&coord_obj(meta));
        let mut head = format!(
            "── {key}  kind={}",
            meta["x-vf-chunk-kind"].as_str().unwrap_or("-")
        );
        if !coord.is_empty() {
            head.push_str(&format!("  coord={coord}"));
        }
        head.push_str(&format!(
            "  model={}",
            meta["x-vf-model"].as_str().unwrap_or("-")
        ));
        if key.as_str() == args.chunk_key {
            head.push_str("  ← anchor");
        }
        println!("{head}");
        let text = meta["x-vf-chunk-text"].as_str().unwrap_or("");
        if p.modality == "image" && text.is_empty() {
            // cross-modal neighbors are shown as-is; an image chunk has no
            // text channel — say that instead of printing nothing
            println!("〔image chunk — no text channel〕");
        } else {
            let (bounded, truncated) = bound_chunk_text(text, args.chars);
            println!("{bounded}");
            if truncated {
                match next_chunk_key(&sorted, key) {
                    Some(next) => println!("…[truncated, next: {next}]"),
                    None => println!("…[truncated]"),
                }
            }
        }
        if n + 1 < blocks.len() {
            println!();
        }
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
        // T12 audit (durable, summed over the markers — a different reset
        // semantics than the process-lifetime stats below): printed only
        // when anything was removed; the --json face always carries it
        if b["furnitureLinesRemoved"].as_u64().is_some_and(|n| n > 0) {
            println!(
                "    furniture: {} lines removed",
                b["furnitureLinesRemoved"]
            );
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

// ---------------------------------------------------------------------------
// tests: the T14 addressing primitives (pure logic — the enumeration /
// windowing / bounding decisions, no I/O; the daemon-facing halves live in
// crates/server/tests/cli_smoke.rs against the real binaries)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn page(keys: &[&str]) -> Vec<(String, Value)> {
        keys.iter().map(|k| (k.to_string(), Value::Null)).collect()
    }

    /// The interval IS the prefix set: `lower <= k < upper` iff k starts
    /// with `{hash}-` — the property early-exit's correctness rests on.
    #[test]
    fn prefix_bounds_are_exactly_the_prefix_set() {
        let hash = "0123456789abcdef";
        let (lower, upper) = chunk_prefix_bounds(hash);
        assert_eq!(lower, "0123456789abcdef-");
        assert_eq!(upper, "0123456789abcdef.");
        for k in ["0123456789abcdef-text-0", "0123456789abcdef-image-9"] {
            assert!(
                k.starts_with(&lower) && k >= lower.as_str() && k < upper.as_str(),
                "{k}"
            );
        }
        for k in [
            "0123456789abcdee-text-0", // hash below
            "0123456789abcdef",        // bare hash, no dash
            "0123456789abcdef.",       // the upper bound itself
            "0123456789abcdef0-x-1",   // longer hash
        ] {
            assert!(!(k.starts_with(&lower) && k < upper.as_str()), "{k}");
        }
    }

    /// Page scanning: keys before the interval pass by, keys inside match,
    /// and the FIRST key at/past the upper bound stops the whole scan (no
    /// later page can contain a match — keys arrive ascending).
    #[test]
    fn page_scan_matches_and_early_exits() {
        let hash = "0123456789abcdef";
        let (lower, upper) = chunk_prefix_bounds(hash);
        // keys entirely below the interval: no match, no stop
        let below = page(&["0000000000000001-text-0", "0123456789abcdee-image-0"]);
        let (m, stop) = scan_chunk_page(&below, &lower, &upper);
        assert!(m.is_empty() && !stop, "m={m:?} stop={stop}");
        // interval keys mixed with nothing past the end: matched, keep paging
        let inside = page(&["0123456789abcdef-image-0", "0123456789abcdef-text-2"]);
        let (m, stop) = scan_chunk_page(&inside, &lower, &upper);
        assert_eq!(m.len(), 2);
        assert!(!stop, "no key reached the upper bound");
        // first key past the interval stops the scan even mid-page
        let mut mixed = page(&["0123456789abcdef-text-0", "0123456789abcdef."]);
        mixed.push(("0123456789abcdef-text-99".into(), Value::Null));
        let (m, stop) = scan_chunk_page(&mixed, &lower, &upper);
        assert_eq!(m.len(), 1);
        assert!(
            stop,
            "the first key at/past the upper bound ends enumeration"
        );
        // a page that OPENS past the interval stops at its first key
        let past = page(&["zzzzzzzzzzzzzzzz-text-0"]);
        let (_, stop) = scan_chunk_page(&past, &lower, &upper);
        assert!(stop);
    }

    /// Address-space order is the NUMERIC global i (document order): mixed
    /// modalities INTERLEAVE (`text-0, text-1, image-2, text-3` — the
    /// chunker's one counter serves text windows and image atoms alike),
    /// `-text-2` sorts before `-text-10` (lexical order inverts them), and
    /// foreign keys in the interval are not address-space members.
    #[test]
    fn entries_order_numerically_with_interleaved_modalities() {
        let hash = chunkkey::object_key_hash("docs/mixed.zip");
        let mut entries = page(&[
            &chunkkey::chunk_key(&hash, "text", 10),
            &chunkkey::chunk_key(&hash, "text", 3),
            &chunkkey::chunk_key(&hash, "image", 2),
            &chunkkey::chunk_key(&hash, "text", 0),
            &chunkkey::chunk_key(&hash, "text", 1),
            // a pure-protocol row parked inside the interval: same hash
            // prefix, not the chunk-key shape
            &format!("{hash}-custom"),
        ]);
        order_chunk_entries(&mut entries);
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                format!("{hash}-text-0").as_str(),
                format!("{hash}-text-1").as_str(),
                format!("{hash}-image-2").as_str(), // the interleave, kept
                format!("{hash}-text-3").as_str(),
                format!("{hash}-text-10").as_str(), // numeric: 3 < 10
            ],
            "i numeric (3 before 10), modalities interleaved, foreign row dropped"
        );
    }

    /// 枚举优先 (the adjudicated property): the window is cut from the REAL
    /// enumerated order — cross-modal neighbors are returned AS-IS, and the
    /// window never contains keys that are not in the enumeration (a
    /// guessing scheme — anchor key with i±1 of the same modality — would
    /// fabricate or miss the interleaved image neighbor here).
    #[test]
    fn window_is_cut_from_the_real_enumeration() {
        let hash = chunkkey::object_key_hash("docs/mixed.zip");
        let sorted: Vec<String> = [
            chunkkey::chunk_key(&hash, "text", 0),
            chunkkey::chunk_key(&hash, "text", 1),
            chunkkey::chunk_key(&hash, "image", 2),
            chunkkey::chunk_key(&hash, "text", 3),
        ]
        .into_iter()
        .collect();
        // the interleave: image-2's neighbors are text-1 and text-3 —
        // returned as-is, not skipped, not guessed into image-1/image-3
        let w = chunk_window(&sorted, &chunkkey::chunk_key(&hash, "image", 2), 1).unwrap();
        assert_eq!(
            w,
            [
                chunkkey::chunk_key(&hash, "text", 1),
                chunkkey::chunk_key(&hash, "image", 2),
                chunkkey::chunk_key(&hash, "text", 3),
            ]
        );
        // head/tail clamping: the window never runs past the enumeration
        let w = chunk_window(&sorted, &chunkkey::chunk_key(&hash, "text", 0), 2).unwrap();
        assert_eq!(w.len(), 3, "clamped at the head: {w:?}");
        let w = chunk_window(&sorted, &chunkkey::chunk_key(&hash, "text", 3), 5).unwrap();
        assert_eq!(
            w.len(),
            4,
            "clamped at the tail: the whole 4-key space {w:?}"
        );
        // --around 0: the anchor alone
        let w = chunk_window(&sorted, &chunkkey::chunk_key(&hash, "text", 1), 0).unwrap();
        assert_eq!(w, [chunkkey::chunk_key(&hash, "text", 1)]);
        // absent anchor: None (the caller's NotFound)
        assert!(chunk_window(&sorted, &format!("{hash}-text-999"), 1).is_none());
    }

    /// Bounded text: exact fit emits no marker, char (never byte) clipping,
    /// and the continuation pointer names the NEXT key in real order.
    #[test]
    fn bounded_text_clips_by_char_and_points_forward() {
        let hash = chunkkey::object_key_hash("docs/a.txt");
        let sorted: Vec<String> = [
            chunkkey::chunk_key(&hash, "text", 4),
            chunkkey::chunk_key(&hash, "text", 5),
        ]
        .into_iter()
        .collect();
        // exact fit / chars = 0 (full) / short text: never truncated
        for chars in [0u64, 5, 500] {
            let (t, trunc) = bound_chunk_text("héllo", chars);
            assert_eq!(t, "héllo");
            assert!(!trunc, "chars={chars}");
        }
        // char discipline: 6 CHARS of CJK, never 6 bytes
        let (t, trunc) = bound_chunk_text("中文段落内容很长", 3);
        assert_eq!(t, "中文段");
        assert!(trunc);
        // the marker's continuation: the next key in the enumeration
        assert_eq!(
            next_chunk_key(&sorted, &chunkkey::chunk_key(&hash, "text", 4)),
            Some(chunkkey::chunk_key(&hash, "text", 5).as_str())
        );
        assert_eq!(
            next_chunk_key(&sorted, &chunkkey::chunk_key(&hash, "text", 5)),
            None,
            "the last chunk has no continuation"
        );
    }

    /// SOURCE-column truncation (T15): tails survive, middles elide — the
    /// file-name end of a long path stays readable, short keys are
    /// untouched, and the CJK char-domain discipline holds.
    #[test]
    fn source_column_keeps_tail_elides_middle() {
        // fits: verbatim
        assert_eq!(truncate_source("docs/fox.txt", 24), "docs/fox.txt");
        // long path: head + '…' + TAIL (24 = head 7 + '…' + tail 16)
        let long = "docs/reports/2026/radar/vol32/fox.pdf";
        let t = truncate_source(long, 24);
        assert_eq!(t.chars().count(), 24, "{t}");
        assert!(t.starts_with("docs/re"), "{t}");
        assert!(t.ends_with("ol32/fox.pdf"), "{t}");
        assert!(t.contains('…'), "{t}");
        // the identifying tail is what head-truncation used to lose
        assert!(!truncate(long, 24).ends_with("fox.pdf"), "self-check");
        // char-domain: 24 CHARS of CJK, never bytes
        let cjk: String = "档".repeat(60);
        assert_eq!(truncate_source(&cjk, 24).chars().count(), 24);
        // degenerate budget: falls back to head truncation, never panics
        assert_eq!(truncate_source("abcdef", 2), "a…");
    }

    /// `--source` flags ⇒ the src-name filter member (T15): one flag is
    /// the bare-key shorthand, several are `$in` — and nothing else; the
    /// translation itself is the SERVER's (ADR 0004 single source of
    /// truth), the client only spells the filter.
    #[test]
    fn source_flags_spell_the_src_name_filter() {
        assert_eq!(source_filter_of(&[]), None);
        assert_eq!(
            source_filter_of(&["docs/fox.txt".to_string()]),
            Some(json!({"x-vf-src-name": "docs/fox.txt"}))
        );
        assert_eq!(
            source_filter_of(&["docs/a.txt".to_string(), "docs/b.txt".to_string()]),
            Some(json!({"x-vf-src-name": {"$in": ["docs/a.txt", "docs/b.txt"]}}))
        );
    }
}
