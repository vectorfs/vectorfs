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

### `vf search <TEXT> [BUCKET]` / `vf search --image <PATH> [BUCKET]`

The one retrieval verb: fan-out + merge + source attribution over the bucket's
embedders. Query = text positional **XOR** `--image` (以图搜图: the image runs
through the bucket's image-capable embedders; `.jpg/.jpeg/.png`, the daemon
cross-checks the real container).

| Flag | Meaning |
|---|---|
| `--image PATH` | image query (XOR the text positional; bucket stays positional) |
| `--mode M` | `semantic` (default) \| `keyword` (BM25 lexical — needs `[ingest] fts = true` + `vectorfs optimize`; exact terms/proper nouns, the semantic blind spot) \| `hybrid` (**recommended** — both arms fused; exact terms / proper nouns / error codes are keyword/hybrid's strength and a pure-semantic blind spot, per the recall-quality research) |
| `--rerank` | cross-encoder re-scoring of the merged top-K (needs `[ingest] rerank = true`; semantic/hybrid only — keyword refuses: BM25 order IS that mode's contract) |
| `--rerank-depth N` | rerank candidate depth (R3a): re-score the first N merged candidates, still return `--top-k`. 1..=200 and ≥ top-k, needs `--rerank`; default = top-k (no change). CPU budget: the cross-encoder scores ~148 ms/pair on CPU — depth 20 ≈ 3 s, depth 50 ≈ 7.4 s (linear in depth) |
| `--per-source N` | per-source quota (R4): at most N hits per source file, applied to the merged ranking before top-K truncation (rank order kept). 1..=100, 0/absent = off. If the quota thins the set below top-k, fewer hits come back — skipped same-source hits are NOT backfilled (backfill would defeat the quota) |
| `--bucket B` | explicit form (XOR the bare-name positional) |
| `--top-k K` | 1..=1000, default 10 |
| `--source KEY` | restrict to one source file by key, repeatable for several (T15) — the server translates names to `x-vf-src-uri` hashes, no hash digging |
| `--filter JSON` | metadata filter, `$`-operator syntax (below); invalid JSON = usage error |
| `--no-text` | omit chunk text from hits |
| `--json` | schema-versioned structured output |

Human output columns: `#  SCORE  KEY  INDEX  SOURCE  COORD  TEXT` (score 4
decimals; TEXT is the first 60 chars of the chunk; long SOURCE keys keep
their tail, middle elided). Deterministic order: score desc,
key asc. Scores are orderings, not calibrated values (different modes/embedders
are not comparable). Result-set governance: `toc` and `legend` chunks are
EXCLUDED by default (TOC lines are page-repeated perfect matches; legends
are pathologically short BM25 docs) — `--kind toc` (repeatable; the wire's
`includeKinds`) admits them back for navigation queries like「哪章在哪页」.

Filter operators (bare value = implicit `$eq`): `$eq $ne $gt $gte $lt $lte
$startsWith $in $nin $exists $and $or`. Example:
`--filter '{"kind": {"$eq": "doc"}, "len": {"$gte": 5}}'`. Filterable
file-layer keys: `x-vf-src-uri` (everywhere) and `x-vf-chunk-kind`; on this
Search face `x-vf-src-name` also filters with `$eq`/`$in` (server-side
translated to the src-uri hash — QueryVectors rejects that key; other
operators on it are refused with a pointer to `x-vf-src-uri`, see
protocol.md). A zero-hit query whose src-name filter matched no indexed
source reports it in a `note` (check the key spelling with `vf ls`).

#### Anchor gating (R1, lexical-arm precision, default on for fts buckets)

Before the lexical arm of a `keyword`/`hybrid` text query runs, the daemon
analyzes the query: its Latin identifier runs and quoted spans (≤8 terms) are
probed against the FTS index by POSTINGS COUNT (df, capped at 64 — one small
single-term FTS query per term, 5-25 ms/probe measured, ≤8 probes ≈ 40-200 ms
added latency). Terms whose df is at or under a calibrated ceiling (16) are
anchors; the RAREST anchor becomes the MUST constraint of a Boolean FTS query
— chunks missing it leave the **lexical arm only** (fellow anchors and the
whole query stay the ranking body). Pure-CJK queries analyze to zero
candidates and ride the ungated behavior.

- **Misjudgment cost, stated plainly**: the gate only ever TIGHTENS the
  lexical arm — the semantic arm is untouched, so a misjudged anchor costs
  the lexical *enhancement*, never a result. Worst case is the pre-hybrid
  ranking, not a lost answer.
- **Verdict visibility**: the response carries an `anchors` member beside
  `hits` exactly when the analysis ran — the verdict list, rarest-first:
  the FIRST entry is the term the lexical arm was gated on, further
  entries are anchors riding the ranking body (`["Vibe"]` = gated on
  Vibe); `[]` = analyzed, nothing is rare enough (the ungated behavior);
  absent = not applicable (semantic mode, fts off, or the knob off). Human
  mode: one stderr diagnostics line.
- **Knob**: `[ingest.<bucket>] anchor-gating = false` restores the ungated
  lexical arm (query-face policy, outside the pipeline fingerprint — no
  reindex). Inert on `fts = false` buckets.
- **Calibration status (measured)**: the ceiling is calibrated against the
  docs radar corpus (`lexical::ANCHOR_DF_CEILING`'s table in the server
  source: vibe df 1 anchors; coding 35 / ThoughtWorks 36 / 雷达 109 /
  技术 336 do not). The v1 score-based probe (top-1 BM25) was measured to
  invert the df ordering (tf/chunk-length effects) and retired; the count
  probe reads df directly. Re-tune via the `r1_probe_calibration`
  acceptance test on your own corpus.

### `vf chunks <KEY>`

List one source's CHUNK address space (T14, ADR 0005): the
`{hash}-{modality}-{i}` keys are the vector world's line numbers — ordered
(document order, one global `i` per source: text windows and image atoms
share the sequence), enumerable, neighborhood-addressable. `vf chunks
docs/hello.pdf` (S3-CLI path form) or `docs/hello.pdf --bucket docs`.

Table: `I  KIND  COORD  TEXT` — I is `{modality}-{i}` (compose the full key
with the hash prefix printed above the table), KIND the `x-vf-chunk-kind`,
COORD the T13 summary (`p18-19` / `L405-420` / `§3`), TEXT a 60-char
preview. `--json`. The read verb for the bodies is `vf chunk show`.

### `vf chunk show <BUCKET> <CHUNK-KEY>`

Bounded neighborhood read (grep -C semantics): the anchor chunk plus its
`--around N` real enumerated neighbors — cross-modal neighbors (a jpg page
between text chunks) are returned as-is, never skipped. Reads the INDEXED
view (the stored `x-vf-chunk-text`, same source as search hits), not the
raw file.

| Flag | Meaning |
|---|---|
| `--around N` | neighborhood radius, 0..=20, default 2 (0 = anchor alone; the ≤41-key window fits one GetVectors call) |
| `--chars B` | chars shown per chunk, default 400; truncated blocks end with `…[truncated, next: <chunk-key>]` — the key to open next |
| `--json` | schema-versioned structured output (blocks with key/kind/coord/model/text/truncated/next) |

`--chars 0` (full text of ONE chunk) requires `--around 0` — a full-text
neighborhood has no size bound (USAGE error otherwise). If the index was
built with `store_chunk_text = false`, the listing still works but this
verb FAILS LOUD (`NO_CHUNK_TEXT`) pointing at that config — never a silent
empty body.

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
