# MCP servers (two entries, one division of labor)

| | `vf mcp` | `vectorfs mcp` |
|---|---|---|
| Runs on | the **agent's** machine | the **storage** machine |
| Mode | remote proxy: every tool call goes to a `vectorfs serve` daemon over the same signed HTTP as the CLI (SigV4 enforced daemon-side); holds no lock | agent **single-user mode**: this process IS the only writer — takes the same locks as `vectorfs serve` |
| Needs | `vf config init` done (or `--endpoint`/`VF_*`) | `--data-dir` (env `VF_DATA_DIR`, default `./vectorfs-data`) |
| serverInfo name | `vf` | `vectorfs` |
| Ingest scheduler | n/a (the daemon has it) | **none** — `put_source` lands the object + PENDING marker; a later `vectorfs serve` scan does extraction/embedding |

## Wiring

```json
{"vf": {"command": "vf", "args": ["mcp"]}}
```

```json
{"vectorfs": {"command": "vectorfs", "args": ["mcp", "--data-dir", "/path/to/data"]}}
```

Both take `--tools ro|rw` (default `rw`). stdout is protocol-only — never wrap
them with anything that prints to stdout.

## `vf mcp` — file-layer tools (9)

| Tool | Params | Notes |
|---|---|---|
| `search` | `bucket`*, `text`*, `topK` (1..=1000, default 10), `filter`?, (`returnText`, default true) | the RAG entry: merged, source-attributed hits with `content.text` and `location.key` |
| `put_source` | `bucket`*, `path` XOR `content`, `key`?, `waitSeconds` (default 0) | `path` = local file (key defaults to its name); `content` = inline text, then `key` REQUIRED. Returns on acceptance (202-style); `waitSeconds > 0` polls GetSource to a terminal state, timeout returns with a note |
| `delete_source` | `bucket`*, `key` | async chunk GC |
| `reindex` | `bucket` | returns the jobId |
| `list_buckets` | — | |
| `list_sources` | `bucket`*, `prefix`?, `status`? | auto-paginated |
| `get_source` | `bucket`*, `key` | status/mime/etag/chunks |
| `get_source_data` | `bucket`*, `key` | the whole original file, lossy UTF-8 text (not for binary formats) |
| `status` | — | daemon health + ingest summary |

`--tools ro` hides `put_source` / `delete_source` / `reindex` from tools/list
and rejects them on tools/call.

## `vectorfs mcp` — data plane + file layer (9)

s3vectors data-plane tools (these stay server-side; `vf mcp` does not have
them):

| Tool | Key params | Notes |
|---|---|---|
| `list_vector_buckets` | `maxResults`?, `nextToken`? | |
| `list_indexes` | `bucket`* | enriched with dimension / distanceMetric / dataType / nonFilterableMetadataKeys |
| `query_vectors` | `bucket`*, `index`*, `queryVector` (len = dim), `topK` 1..=10000, `filter`?, `returnMetadata`?, `nextToken`? | paged 100/page; order distance asc, key asc |
| `get_vectors` | `bucket`*, `index`*, `keys` (1..=100) | |
| `put_vectors` | `bucket`*, `index`*, `vectors` (1..=500) | upsert = whole-row replace; metadata ≤ 50 keys / ≤ 40 KB |
| `delete_vectors` | `bucket`*, `index`*, `keys` (1..=500) | idempotent |

File-layer tools: `search` (same schema as above), `list_sources`
(`bucket`*, `prefix`?, `status`?), and `put_source` — server-side form is
**`path` only** (a local file; no `content`, no `waitSeconds`), plus optional
`key`.

`--tools ro` disables `put_vectors` / `delete_vectors` / `put_source` (and
takes no bucket locks at all).

## Result & error contract

Every tool returns a JSON string carrying `schema_version` (currently 1).
Failures are tool errors shaped `{code, message, hint}` with the same mapping
as the CLI (`Validation` / `NotFound` / `AccessDenied` / `Conflict` /
`ServerError` / `ENDPOINT_UNREACHABLE`). A `search` or `put_source` against a
bucket without an `[ingest.<bucket>]` section is a Validation error — see
server.md for the config.

Startup behavior: `vf mcp` probes `/health` and warns (does not exit) if the
daemon is down; tool calls then fail with `ENDPOINT_UNREACHABLE`.
