# vectorfs

**Store files, search semantically — S3 Vectors-compatible, single binary, Docker-friendly.**

[中文文档](README_ZH.md)

Self-hosted, single-binary **"S3 Vectors-compatible vector store + file vectorization"**:
drop files in, get semantic search out. Point it at a local directory or memory and it
doubles as an instant s3vectors development and CI emulator.

- Two binaries, one tarball: `vectorfs` (server) + `vf` (client).
- Any S3-compatible object store as persistence; a local directory or memory works too
  (dev/CI emulator mode). No own storage engine — your data stays in open formats
  (Lance + JSON) in your own store.

[![CI](https://github.com/vectorfs/vectorfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vectorfs/vectorfs/actions/workflows/ci.yml)

## Two API faces

One daemon, two wire contracts. The naming is the map:

| | **s3vectors API** | **vectorfs API** |
|---|---|---|
| What | All 21 `s3vectors-2025-07-15` operations — the AWS S3 Vectors protocol, byte-compatible (error shapes included) | 8 native operations under `/x-vf/<Op>` — files in, semantic search out, hits point back at the source file |
| Verified by | Unmodified boto3 and aws-sdk-go-v2 (32-step conformance each, green in CI) | `vf` CLI and MCP file-layer tools |
| Auth | SigV4, strongly consistent read-after-write | Same SigV4, same error model, 202 + status-polling for async verbs |

The capability behind the vectorfs API — extract → chunk → embed, with reconciliation
and GC — is implemented by the **file layer** (see [below](#the-file-layer-in-brief)).

> **Status: pre-release (v0.2).** The s3vectors API face is complete; the vectorfs API
> ships its text-modality MVP (PDF / Markdown / plain text). Image modality, OCR,
> docx/html extractors, hybrid full-text search → v0.3 (see [design.md](design.md) §7).

Vocabulary is governed by [CONTEXT.md](CONTEXT.md); the two-face naming decision is
[ADR 0001](docs/adr/0001-two-layer-api-naming.md).

## Five-minute quickstart

Install the `vf` CLI (default `~/.local/bin`; checksum-verified against the release's
`sha256sums.txt`):

```bash
curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.sh | bash
# or: install-cli.sh --with-server   (also installs the `vectorfs` daemon binary)
# or: install-cli.sh --version v0.1.0 --to /usr/local/bin
```

Windows (native, client-only zip — the daemon stays a linux binary; same
checksum verification):

```powershell
iwr -useb https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.ps1 | iex
```

Then configure once — `vf config init` writes `~/.config/vectorfs/config.toml`
(connection knobs resolve `--flag` > `VF_*` env > that file):

```bash
vf config init --endpoint http://127.0.0.1:8787 --access-key <AK> --secret-key <SK>
```

Manually: download a release tarball for Linux x86_64/aarch64 from
[Releases](https://github.com/vectorfs/vectorfs/releases) (published on `v*` tags with
`sha256sums.txt`; contains `vectorfs` + `vf`), or build from source (Rust 1.94+,
C toolchain, cmake, protoc):

```bash
cargo build --profile release-prod --workspace   # -> target/release-prod/{vectorfs,vf}
```

### File vectorization (text modality)

```bash
vectorfs init --preset text-rag --data-dir ./data   # materializes config, downloads the
                                                    # default model (~1.2 GiB, progress on
                                                    # stderr; HF_ENDPOINT mirror supported)
vectorfs serve --data-dir ./data                    # listens on http://127.0.0.1:8787

export VF_ENDPOINT=http://127.0.0.1:8787 VF_ACCESS_KEY=… VF_SECRET_KEY=…   # printed by init

vf put report.pdf --bucket docs          # foreground until INDEXED
vf search "capacity planning" --bucket docs
#  #  SCORE   KEY                  INDEX         SOURCE         TEXT
#  1  0.71    510da8…-text-0       text-fcdd…    report.pdf     Capacity planning for …
```

Files placed by ANY standard tool (aws s3 cp into the source bucket on the s3
backend, plain file copy on the file backend) are picked up by the periodic scan
(default 30 s) — the source bucket IS the data source; deletions are reconciled
the same way (chunks GC'd).

`vf` verbs: `put` (`--async`), `search`, `sources list|status`, `get`, `rm`,
`reindex`, `status`. Everything is also on the wire as the vectorfs API's eight
operations under `/x-vf/<Op>` (design.md appendix E) — same SigV4, same error shapes.

Querying through a plain AWS SDK works too (same embedding model required — the
query vector must come from `gte-multilingual-base` for the default preset):

```python
client.list_indexes(vectorBucketName="docs")           # -> derived index "text-…"
client.query_vectors(..., queryVector={"float32": q}, returnMetadata=True)
# hit metadata carries the provenance keys:
#   x-vf-src-name (file key)  x-vf-src-uri (short hash, filterable)
#   x-vf-model                x-vf-chunk-text
```

### Unified multimodal embedding (remote, b1)

The `openai` embedder kind speaks a multimodal `messages` dialect (the
vLLM/IONOS extension of `/v1/embeddings`) behind a config gate: with
`multimodal = true` it embeds **text AND images in one space** — doc side
`[text, image]`, query side `[text, image]`, one index, one score. The
reference model is **Qwen3-VL-Embedding** (Apache-2.0, MMEB-V2 73.2/77.8,
MRL 64..=2048/4096); there is deliberately **no CPU path** (no official
ONNX, prefill is TFLOPs-scale per image — research/2026-11-m3-unified-embedder-eval.md).

Self-host on a GPU box with vLLM (≥0.14, pooling runner):

```bash
vllm serve Qwen/Qwen3-VL-Embedding-2B --runner pooling    # 2B: ~2.13 GiB weights;
                                                          # 8B: 8.14 GiB, 4096-dim
```

then in vectorfs.toml (loopback http is allowed by the openai kind for
on-box gateways):

```toml
[embedder.unified]
kind = "openai"
endpoint = "http://127.0.0.1:8000/v1"     # the vLLM server
model = "Qwen/Qwen3-VL-Embedding-2B"
dim = 2048
api_key = "EMPTY"                          # vLLM ignores it; any non-empty string
multimodal = true
# query_instruction = "Find the matching document"   # optional, per side; the
# doc_instruction  = "Represent the document."       # official default is used
# dimensions = 1024                                  # MRL truncation (must equal dim)
```

or a managed endpoint (IONOS AI Model Hub serves `Qwen/Qwen3-VL-Embedding-8B`
with the same dialect):

```toml
[embedder.unified]
kind = "openai"
endpoint = "https://openai.inference.de-txl.ionos.com/v1"
model = "Qwen/Qwen3-VL-Embedding-8B"
dim = 4096
api_key = "<your IONOS key>"
multimodal = true
```

Wire facts (verified against both stacks): every entry — text included —
goes `messages` (`{role: system, content: [instruction]}` + `{role: user,
content: [image data-URL?, text?]}`, one message per request), `dimensions`
rides both wire forms, images are base64 data URLs capped at 20 MiB raw.
**Pin your serving stack**: vLLM's pre-processing differs measurably from
the official `qwen_vl_utils` examples, and the endpoint+model pair is part
of the pipeline fingerprint — changing either requires `--reindex`, by
design. Cost note: at full 2048-dim a text-dominant corpus pays ~2.7x the
vector bytes of the 768-dim default; MRL `dimensions = 1024` brings it to
~1.3x. TEI does not serve this model (open feature request).

### Protocol-only mode (SDK emulator)

```bash
vectorfs init --data-dir ./data && vectorfs serve --data-dir ./data
```

Point boto3 at it (credentials from `vectorfs init`; every command here was executed
against a fresh init):

```python
import boto3

client = boto3.client("s3vectors", endpoint_url="http://127.0.0.1:8787",
                      aws_access_key_id="...", aws_secret_access_key="...",
                      region_name="us-east-1")
client.create_vector_bucket(vectorBucketName="docs")
client.create_index(vectorBucketName="docs", indexName="chunks",
                    dataType="float32", dimension=3, distanceMetric="cosine")
client.put_vectors(vectorBucketName="docs", indexName="chunks", vectors=[
    {"key": "a", "data": {"float32": [1.0, 0.0, 0.0]}, "metadata": {"kind": "doc", "len": 4}},
    {"key": "b", "data": {"float32": [0.0, 1.0, 0.0]}, "metadata": {"kind": "doc", "len": 9}},
])
res = client.query_vectors(vectorBucketName="docs", indexName="chunks",
                           queryVector={"float32": [1.0, 0.0, 0.0]}, topK=2,
                           filter={"kind": {"$eq": "doc"}, "len": {"$gte": 5}},
                           returnDistance=True, returnMetadata=True)
print(res["vectors"])   # -> [{'key': 'b', 'distance': 1.0, 'metadata': {...}}]
```

The same query through aws-sdk-go-v2:

```go
client := s3vectors.NewFromConfig(cfg, func(o *s3vectors.Options) {
    o.BaseEndpoint = aws.String("http://127.0.0.1:8787") })
res, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
    VectorBucketName: aws.String("docs"),
    IndexName:        aws.String("chunks"),
    QueryVector:      &s3vectors.VectorDataMemberFloat32{Value: []float32{1, 0, 0}},
    TopK:             aws.Int32(2),
    ReturnDistance:   true,
})
```

## Bucket model

Every vector bucket `X` is a **pair of physical buckets** — the naming
convention *is* the placement, zero configuration (design-v3.md §4):

```
endpoint bucket root                <data-dir>/.vectorfs/ (daemon home)
(or <data-dir>/ on the file          ├── vectorfs.toml   config + credentials
 backend; one key space on memory)   ├── models/
├── docs/     ← source bucket:       └── flock           same-host double-start lock
│   └── a.pdf     pure user namespace — plain objects,
│                 zero system objects; the periodic scan
│                 watches the WHOLE bucket
└── docs-vf/  ← system bucket: its root IS the system area
    ├── LOCK        per-bucket writer lock (TTL heartbeat, writer id)
    ├── bucket.json ownership marker (deployment identity)
    ├── sources/…   per-file ingest markers
    └── indexes/<ix>/…  index metadata + Lance datasets
```

- The `-vf` suffix is the default and configurable once per deployment
  (`bucket_suffix` in `vectorfs.toml` — file-only knob, install-time constant;
  changing it selects a different namespace, it never migrates data).
- The source bucket is created **lazily** by the first `vf put` / PutSource;
  `DeleteVectorBucket` removes `X-vf` entirely and **never deletes** `X` —
  your files survive a vector bucket deletion.
- Vector bucket names: 3–60 chars (63 minus the suffix), lowercase/digits/
  hyphens, no dots (stricter than general S3 bucket names), and the name must
  not end with the configured suffix.
- Single writer **per bucket**. One daemon managing every bucket is the
  default deployment; multiple daemons may share one endpoint only over
  **disjoint bucket sets**, and a pure protocol-face fan-out in front of one
  bucket needs external bucket-aware routing (the lazy locks are the
  enforcement, not the declaration).

## Backends

- `file` (default) — the data dir plays bucket root: each vector bucket is the
  directory pair `X/` + `X-vf/`; your files are plain files inside `X/`; the
  daemon home `.vectorfs/` (vectorfs.toml + models) sits beside them. Zero
  configuration.
- `memory` — volatile, for CI and emulator use: `vectorfs serve --backend memory
  --no-auth` (no SigV4, no persistence; start/stop in seconds, deterministic).
- `s3` — any S3-compatible endpoint, in **endpoint form** (no bucket path —
  buckets come from the dual-bucket model above): `vectorfs serve --backend
  's3+http://host:8333'` with `--s3-access-key/--s3-secret-key/--s3-region`
  (or `VF_S3_*`/`AWS_*` env). Credentials are required for bucket operations —
  SeaweedFS scopes ListBuckets to the account, so an anonymous endpoint simply
  enumerates as empty. Verified end-to-end on SeaweedFS 4.52 and Cloudflare R2
  (dual-SDK conformance, conditional writes, multipart); the raw bucket admin
  verbs (create/head/list/delete) run a dockerized-SeaweedFS conformance suite
  in CI.

## The file layer in brief

The file layer is the subsystem implementing the vectorfs API: scan, pipeline,
reconciliation, GC.

- Presets: `dev` (protocol only), `text-rag` (default model, zero extra config),
  `air-gapped` (local model path, `offline = true`, nothing ever leaves the
  network). `vectorfs init --preset X` materializes `.vectorfs/vectorfs.toml`
  and prefetches models when needed (`HF_ENDPOINT` mirror respected;
  sha256-verified, atomic).
- Pipeline = **normalize** (built-in ordered transforms before routing — gzip
  files are decompressed in place today; charset recoding / email & archive
  container splitting land with v0.3) → extractor (pdf-text via pdfium /
  `pdf-inspector` [pure-Rust, scans fail loud with an OCR-needed reason,
  optional position-aware Markdown] / md / txt; per-mime values may be an
  **ordered fallback chain**, and multi-member chains are **content-routed** —
  PDFs are classified first and the member that declares the best fit for the
  content class runs, e.g. scanned → an OCR-capable extractor rather than
  trial-and-error; container kinds unpack into child documents that re-enter
  the same routing — EPUB lands on this mechanism in v0.3) → chunker
  (token-window, char-counted, CJK-safe) → embedder (local `ort`
  [onnxruntime, load-dynamic] or any OpenAI-compatible `/embeddings` endpoint).
  Config in `.vectorfs/vectorfs.toml`; `vectorfs pipelines check` validates it
  with TOML line numbers.
- Correctness is fingerprinted, not hoped for: changing extractor/chunker/embedder
  config refuses startup until you re-embed explicitly (`vectorfs serve --reindex`).
- GC only ever deletes vectors in ingest-managed indexes whose keys match the
  derived chunk format and whose `x-vf-src-uri` resolves to a deleted object —
  plain s3vectors API vectors are never touched (tested).

## Docker

```bash
docker compose -f deploy/docker-compose.yml up        # data + models volumes, healthcheck on /health
```

Local builds: `scripts/docker_build.sh` (or `scripts/docker_build_cn.sh` on
mainland networks — Docker Hub / Debian / crates.io / GitHub-release mirrors
and an `HF_ENDPOINT=hf-mirror.com` default, all env-overridable; see its
header) stages prebuilt binaries and builds the host arch.

Image: `ghcr.io/vectorfs/vectorfs` (multi-arch linux/amd64+arm64, debian-slim,
non-root, binaries + pinned onnxruntime/pdfium `.so`; models stay on the volume,
never in image layers). `VF_PRESET=text-rag` materializes the config on first
boot. An `emulator` compose profile runs the volatile `memory --no-auth`
variant — a seconds-to-start s3vectors stand-in for CI/testcontainers. Locking
is **per bucket** (a TTL-heartbeat LOCK object inside each `X-vf`): one
container managing every bucket is the default deployment — do NOT scale this
service over the same bucket set; daemons may share an endpoint only over
disjoint bucket sets (pure protocol-face fan-out needs external bucket-aware
routing).

## Limits: AWS vs local

Structural limits are enforced exactly as AWS does (same error shapes);
multi-tenant quotas are not enforced locally.

| Limit | AWS | vectorfs |
|---|---|---|
| Vectors per PutVectors | ≤ 500, body ≤ 20 MiB | same (enforced) |
| Keys per GetVectors / DeleteVectors | ≤ 100 / ≤ 500 | same (enforced) |
| Index dimension | 1–4096, fixed at create | same (enforced) |
| Metadata per vector | ≤ 50 keys, ≤ 40 KB; filterable keys ≤ 2 KB | same (enforced) |
| nonFilterableMetadataKeys | 1–10 keys, immutable | same (enforced) |
| QueryVectors topK | ≤ 10,000 (100/page + nextToken) | same (enforced) |
| Filter constraints per query | ≤ 100 (ENHANCED indexes) | same (enforced) |
| ListVectors maxResults | 1–1000 (default 500) | same (enforced) |
| ListVectorBuckets/ListIndexes maxResults | 1–500 + prefix | same (enforced) |
| Segment count/index | 1–16 / 0–15, paired | same (enforced) |
| Vector values | cosine rejects zero vectors; NaN/Inf rejected | same (enforced) |
| QPS / throughput quotas | yes | **not enforced** |
| Total vectors / bucket+index count caps | 2B / 10,000 | **not enforced** |

File-layer provenance metadata budget (per chunk vector, `x-vf-*` keys):
`x-vf-src-uri` ≤ 100 B (object-key hash, filterable), `x-vf-src-name` full key
(non-filterable), `x-vf-model`, `x-vf-modality`, `x-vf-chunk-text` (non-filterable,
stored by default — disable per ingest section with `store_chunk_text = false`).

## MCP

`vectorfs mcp` is a stdio MCP server for agent single-user mode — the only writer
of its buckets (same per-bucket locks as `vectorfs serve`):

```json
{"vectorfs": {"command": "vectorfs", "args": ["mcp", "--data-dir", "/path/to/data"]}}
```

Add `"--tools", "ro"` for a read-only toolset. s3vectors API data-plane tools:
`list_vector_buckets`, `list_indexes`, `query_vectors`, `get_vectors`, `put_vectors`,
`delete_vectors`; vectorfs API tools: `search`, `list_sources`, `put_source`
(`put_source` is rw-only).

### `vf mcp` — agent remote mode

`vf mcp` is the second MCP entry: a stdio server run on the AGENT's machine
that proxies every tool call to a `vectorfs serve` daemon over the same signed
HTTP as the vf verbs (endpoint/credentials resolve from `--endpoint` / `VF_*`
env / `~/.config/vectorfs/config.toml` — usually no args once `vf config init`
ran). It holds no lock; the daemon's SigV4 gate sees every call:

```json
{"vf": {"command": "vf", "args": ["mcp"]}}
```

Tools (the file layer): `search` (the RAG entry — merged, source-attributed
hits carrying `content.text` and `location.key`), `list_buckets`,
`list_sources`, `get_source`, `get_source_data` (the whole original file),
`status`; with `--tools rw` (the default) also `put_source` (a local file via
`path` OR inline text via `content`, returning immediately — `waitSeconds`
for a foreground wait), `delete_source`, `reindex`. Division of labor:
`vectorfs mcp` IS the writer on local storage; `vf mcp` drives a remote
daemon — the s3vectors data-plane tools stay server-side.

### Agent Skill

[skills/vectorfs/](skills/vectorfs/) is the official Agent Skill for coding
agents (Claude Code, Codex, ZCode, …): a `SKILL.md` decision tree plus
reference files for the `vf` CLI, both MCP entries, daemon ops, and the wire
faces. One-line install into any agent's skills directory:

```bash
curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/skills/vectorfs/install.sh | bash
# or: … | bash -s -- --to ~/.zcode/skills    (any agent's skills root)
```

A focused companion, [skills/vf-search/](skills/vf-search/), teaches just the
search flow — query crafting, metadata filters, hit → full document — for
agents whose users only ever search.

## Non-goals

- No own object-storage backend or storage engine — persistence is always an
  existing object store or a local directory. Files are stored as ordinary
  objects; vectorfs adds no second copy and no content-addressed store.
- No homegrown versioning, embedding-model hosting/training platform, chat UI,
  write-time entity extraction, kernel-mode implementation, plugin marketplace,
  AWS-console UI clone, non-AWS native API faces.
- No distributed multi-writer coordination (one writer per bucket; each bucket's
  LOCK object heartbeats and takeovers follow TTL semantics).

## Compatibility notes

- Error shapes match AWS exactly (`ValidationException` 400, `NotFoundException` 404,
  `ConflictException` 409, `ServiceQuotaExceededException` 402, throttling 429, …),
  emitted as both the `X-Amzn-Errortype` header and body `__type` for maximum
  client compatibility.
- The vectorfs API's eight operations live under `/x-vf/<Op>` — an isolated
  namespace so the AWS surface and the native face can never collide; same SigV4,
  same error model, 202 + status-polling for async verbs (design.md appendix E).
- `vf` shares its name with fish's VirtualFish — if you use both, alias as needed.
- Egress: none for the s3vectors API face. The file layer reaches out only for
  model download (init/prefetch; `HF_ENDPOINT` mirror, standard `HTTPS_PROXY`/
  `NO_PROXY` env honored) or an explicitly configured OpenAI-compatible embedder
  endpoint.

## Docs

- [design.md](design.md) — positioning, protocol spec, architecture, milestones,
  and appendix E (the CLI/`/x-vf` command tables — the authoritative list).
- [design-v3.md](design-v3.md) — the v3 bucket model: dual-bucket mapping,
  lifecycle/crash rules, per-bucket locks, invariants, implementation log.
- [CONTEXT.md](CONTEXT.md) — the ubiquitous language: two API faces, bucket
  model, file layer, backends ([ADR 0001](docs/adr/0001-two-layer-api-naming.md)
  records the naming decision).
- [skills/vectorfs/](skills/vectorfs/) — the official Agent Skill (SKILL.md +
  references): install into Claude Code / Codex / any agent that loads skills;
  [skills/vf-search/](skills/vf-search/) is its focused search-only companion.
- [spike/REPORT.md](spike/REPORT.md) — M0 measurements (latency, binary size);
  [conformance/](conformance/) — the 32-step dual-SDK conformance suite.

License: Apache-2.0 (see [LICENSE](LICENSE)).
