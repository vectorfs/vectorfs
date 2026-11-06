# vf CLI reference

`vf` talks to a remote `vectorfs serve` daemon over signed HTTP. It is built
for agents: stdout carries results, stderr carries progress, `--json` is
schema-versioned, zero interactivity, idempotent.

## Connection: `--flag` > `VF_*` env > config file

| Flag | Env | Config key (`~/.config/vectorfs/config.toml`) |
|---|---|---|
| `--endpoint` | `VF_ENDPOINT` | `endpoint` |
| `--access-key` | `VF_ACCESS_KEY` | `access_key` |
| `--secret-key` | `VF_SECRET_KEY` | `secret_key` |
| `--region` (default `us-east-1`) | `VF_REGION` | `region` |

Every verb takes all four flags (they are not global). The config file lives at
`$XDG_CONFIG_HOME/vectorfs/config.toml` else `~/.config/vectorfs/config.toml`
(Windows: `%USERPROFILE%\.config\…`); exactly these 4 keys, unknown keys are
rejected, written 0600. `vf config init` refuses to overwrite an existing file
(`--force` regenerates) and prints the written path to stdout.

## Bucket/key argument resolution

- `--bucket` **present** ⇒ the positional is the **full key verbatim** (may
  contain `/`).
- `--bucket` **absent** ⇒ the positional must be `bucket/key`, split at the
  **first** `/`.
- Bucket verbs (`search`, `reindex`): bare bucket name positional **XOR**
  `--bucket` — giving both is a usage error (exit 2).
- `put`: a **last positional ending in `/`** is the destination
  `bucket[/prefix]/` (aws-cp style), not a file: `vf put a.txt docs/pre/` ⇒
  key `pre/a.txt`. `--bucket` and a destination positional are mutually
  exclusive; `--key` only with exactly one file.

## Verbs

### `vf put [FILES]… [dest]/`

Upload and (default) wait for the file layer to index each file — polls
GetSource every 500 ms to a terminal state (`INDEXED`, `PARTIALLY_INDEXED`,
`FAILED`, `DELETING`).

| Flag | Meaning |
|---|---|
| `--bucket B` | explicit bucket (XOR destination positional) |
| `--key K` | object key (single file only; default: file name, under a destination prefix) |
| `--async` | return as soon as the daemon accepts (202); do not wait |
| `--timeout S` | per-file foreground wait budget (default 300) |

stdout: one `STATUS<TAB>key` line per file; progress on stderr. Exit 1 if any
file failed. Failed file? `vf sources status <key> --bucket B` shows
`statusReason`.

### `vf search <TEXT> [BUCKET]`

The one retrieval verb: fan-out + merge + source attribution over the bucket's
embedders.

| Flag | Meaning |
|---|---|
| `--bucket B` | explicit form (XOR the bare-name positional) |
| `--top-k K` | 1..=1000, default 10 |
| `--filter JSON` | metadata filter, `$`-operator syntax (below); invalid JSON = usage error |
| `--no-text` | omit chunk text from hits |
| `--json` | schema-versioned structured output |

Human output columns: `#  SCORE  KEY  INDEX  SOURCE  TEXT` (score 4 decimals;
TEXT is the first 60 chars of the chunk). Deterministic order: score desc,
key asc.

Filter operators (bare value = implicit `$eq`): `$eq $ne $gt $gte $lt $lte
$startsWith $in $nin $exists $and $or`. Example:
`--filter '{"kind": {"$eq": "doc"}, "len": {"$gte": 5}}'`. Note: only
filterable metadata participates — of the file-layer keys that is `x-vf-src-uri`
only (see protocol.md).

### `vf ls [PATH]`

No argument: list the ingest buckets (`INDEXED=  PARTIAL=  PENDING=  FAILED=`
counts). `vf ls docs` / `vf ls docs/pre`: list a bucket's sources
(prefix-filtered client-side). `--status` filters
(PENDING / IN_PROGRESS / PARTIALLY_INDEXED / INDEXED / FAILED / DELETING);
`--json`.

### `vf sources list --bucket B` / `vf sources status <KEY> --bucket B`

`list`: marker × live join, auto-paginated (1000/page until exhausted);
`--status` filter; `--json`. Table: `KEY STATUS CHUNKS SIZE UPDATED`.
`status`: one file's `status`, `statusReason`, `retriesExhausted`, `mime`,
`etag`, `pipelineFingerprint`, `chunks`, `updatedAt`.

### `vf get <KEY> [--out F]` / `vf cat <KEY>`

Download a source's original bytes. `get` writes to `--out` or stdout (progress
line on stderr); `cat` is the always-stdout form (no `--out`).

### `vf rm <KEY>`

Delete a source: object deleted, chunks GC'd asynchronously. stdout
`STATUS<TAB>key`.

### `vf reindex [BUCKET]`

Explicit full re-embed of a bucket (after pipeline config changes). stdout is
the jobId only. Progress: `vf status` / `vf sources list`.

### `vf status`

`/health` + GetIngestStatus summary: engines, per-bucket counts/stats/
fingerprints, derived indexes (index/embedder/model/dim), reindex progress.
Without credentials it degrades to a health-only probe.

### `vf mcp` / `vf config init`

See mcp.md; config init above.

## Agent output contract

- stdout = results, stderr = progress/diagnostics. Never prompt.
- `--json` output carries `schema_version` (currently 1).
- Errors: structured `{code, message, hint}` on stderr (JSON object with
  `--json`). Mapping: `ValidationException`→`Validation`,
  `NotFoundException`→`NotFound`, `AccessDeniedException`→`AccessDenied`,
  `ConflictException`→`Conflict`, anything else→`ServerError`,
  transport failure→`ENDPOINT_UNREACHABLE`.
- Exit codes: `0` success · `1` command failed (wire/transport or a failed
  file) · `2` usage/config error · `3` data root held by another writer.

## Compatibility notes

- `vf` shares its name with fish's VirtualFish — alias if you use both.
- Windows: `vf.exe` from the client-only zip (`install-cli.ps1`).
