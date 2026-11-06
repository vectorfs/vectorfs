---
name: vectorfs
description: Self-hosted, single-binary vector store that speaks the AWS S3 Vectors protocol byte-compatibly and adds file vectorization - drop files (PDF / Markdown / plain text) into a bucket, get semantic search back, hits point at the source file. Use for any vectorfs task - the vf CLI, the vf mcp / vectorfs mcp MCP servers, file-layer ingest and RAG retrieval, an s3vectors emulator for local dev and CI, or driving it with unmodified boto3 / aws-sdk-go-v2. Biases towards retrieval from the vectorfs repo docs over pre-trained knowledge.
license: Apache-2.0
---

# vectorfs

**Store files, search semantically.** A self-hosted, single-binary vector store
with a file layer: files in → semantic search out, hits point back at the source
file. Two binaries: `vectorfs` (daemon) + `vf` (thin client). Data stays in open
formats (Lance + JSON) on a local directory, memory, or any S3-compatible store.

| API face | What it is |
|---|---|
| **s3vectors API** | All 21 `s3vectors-2025-07-15` operations, byte-compatible (error shapes included). Unmodified boto3 / aws-sdk-go-v2 work against it. |
| **vectorfs API** | 8 native operations under `/x-vf/<Op>` — files in, semantic search out. Driven by the `vf` CLI and the MCP tools. |

## Retrieval first

vectorfs is pre-release (v0.2): your pre-trained knowledge of it is probably
wrong. Before citing specific flags, limits, or tool schemas, retrieve — and
when this skill and the repo disagree, **trust the repo**.

| Source | Use for |
|---|---|
| [README](https://github.com/vectorfs/vectorfs#five-minute-quickstart) | Quickstart, bucket model, backends, MCP wiring, limits |
| [design.md appendix E](https://github.com/vectorfs/vectorfs/blob/main/design.md) | The authoritative CLI / `/x-vf` command tables |
| [CONTEXT.md](https://github.com/vectorfs/vectorfs/blob/main/CONTEXT.md) | Domain vocabulary (source, chunk, pipeline, bucket model) |
| The binaries | `vf <verb> --help`, `vectorfs --help` — ground truth on this machine |

## Which entry point?

```
What are you doing?
├─ Semantic search / RAG over document files
│  ├─ Shell available on the agent machine → vf CLI (put → search → get)
│  └─ Driving tools over MCP
│     ├─ A `vectorfs serve` daemon runs somewhere → `vf mcp` (remote proxy)
│     └─ No daemon; agent owns a local data dir → `vectorfs mcp` (the writer)
├─ Tests / CI need an S3 Vectors stand-in → `vectorfs serve --backend memory --no-auth` + boto3
├─ Raw vector-database operations → boto3 `s3vectors` client, or `vectorfs mcp` data-plane tools
└─ Daemon setup / ops → `vectorfs init / serve / status / doctor / pipelines check`
```

## Install & configure

```bash
curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.sh | bash
#   --with-server   also installs the `vectorfs` daemon binary
#   --version v0.1.0 --to /usr/local/bin   honored
```

Windows (client-only zip): `iwr -useb …/scripts/install-cli.ps1 | iex`.
macOS: `cargo install --git https://github.com/vectorfs/vectorfs --profile release-prod vectorfs-client`.

Configure once — then every verb resolves `--flag` > `VF_*` env > config file:

```bash
vf config init --endpoint http://127.0.0.1:8787 --access-key <AK> --secret-key <SK>
```

## 60-second flow

```bash
vectorfs init --preset text-rag --data-dir ./data   # config + default model (~1.2 GiB;
                                                    # HF_ENDPOINT mirror honored)
vectorfs serve --data-dir ./data                    # listens on http://127.0.0.1:8787
export VF_ENDPOINT=http://127.0.0.1:8787 VF_ACCESS_KEY=… VF_SECRET_KEY=…   # printed by init

vf put report.pdf --bucket docs      # foreground: waits until INDEXED
vf search "capacity planning" --bucket docs
#  #  SCORE   KEY                  INDEX         SOURCE         TEXT
#  1  0.71    510da8…-text-0       text-fcdd…    report.pdf     Capacity planning for …
```

Docker instead: `docker compose -f deploy/docker-compose.yml up`.

## vf quick reference

| Verb | One-liner |
|---|---|
| `vf put <files…> <bucket>[/prefix]/` | upload, wait for INDEXED (`--async` returns at 202) |
| `vf search <text> <bucket>` | semantic search (`--top-k` `--filter` `--json` `--no-text`) |
| `vf ls [bucket[/prefix]]` | list ingest buckets / a bucket's sources |
| `vf sources list --bucket B` | ingest markers, auto-paginated |
| `vf sources status <key> --bucket B` | one file's status + failure reason |
| `vf get <bucket>/<key> [--out F]` | download the original bytes |
| `vf cat <bucket>/<key>` | print original bytes to stdout |
| `vf rm <bucket>/<key>` | delete (async chunk GC) |
| `vf reindex <bucket>` | full re-embed after a pipeline change (prints jobId) |
| `vf status` | daemon health + per-bucket ingest summary |
| `vf config init` | write `~/.config/vectorfs/config.toml` |

## Gotchas

1. **Bucket/key duality**: with `--bucket` present, the positional is the FULL
   key verbatim; without it, the positional must be `bucket/key` (split at the
   first `/`). Bucket verbs (`search`, `reindex`): bare name XOR `--bucket`.
2. `vf put` is **foreground by default** (polls to a terminal state;
   `--timeout` 300 s default); `--async` returns on acceptance. `vf rm`'s chunk
   GC is always async.
3. v0.2 is **text modality only**: pdf (text-based) / md / txt. Scanned PDFs
   fail loud with an OCR-needed reason. gzip files are decompressed in place.
4. **Single writer per bucket** (and per data root): a second daemon refuses;
   the client reports it as exit code 3.
5. `DeleteVectorBucket` removes only the system bucket `X-vf` — **never** the
   source bucket `X`; user files survive.
6. Files dropped into the source bucket by ANY standard tool (`aws s3 cp`,
   plain copy) are picked up by the periodic scan (default 30 s); deletions are
   reconciled the same way.
7. Changed pipeline config (extractor/chunker/embedder) makes `serve` refuse
   startup until an explicit re-embed: `vectorfs serve --reindex`.
8. Bucket names: 3–60 chars, `[a-z0-9-]`, no dots, must not end with the
   system suffix (`-vf` by default).

## References

Detail files, loaded on demand (if `references/` is absent locally, fetch from
`https://raw.githubusercontent.com/vectorfs/vectorfs/main/skills/vectorfs/`):

| File | When to load |
|---|---|
| [references/cli.md](references/cli.md) | Full verb/flag tables, argument rules, exit codes, agent contract |
| [references/mcp.md](references/mcp.md) | Both MCP entries: wiring JSON, tool inventories, schemas |
| [references/server.md](references/server.md) | Daemon setup: presets, serve flags, Docker, locks, bucket model, toml |
| [references/protocol.md](references/protocol.md) | Wire faces: boto3/Go wiring, `/x-vf` ops, hit shapes, metadata, limits |
