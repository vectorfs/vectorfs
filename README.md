# vectorfs

**Store files, search by meaning — S3 Vectors-compatible, single binary, Docker-friendly.**

单二进制、自托管的「S3 Vectors 兼容向量存储 + 文件即向量化」：文件放进来，语义检索出结果；
指向本地目录/内存时是即开即用的 s3vectors 开发与 CI 模拟器。

- Two binaries, one tarball: `vectorfs` (server) + `vf` (client).
- All 21 `s3vectors-2025-07-15` operations, verified through unmodified boto3 and
  aws-sdk-go-v2 (32-step conformance each, green in CI), SigV4-authenticated,
  strongly consistent read-after-write.
- **v0.2 file layer (text modality)**: drop files into the instance root, a built-in
  pipeline extracts → chunks → embeds them locally (default model
  [`gte-multilingual-base`](https://huggingface.co/Alibaba-NLP/gte-multilingual-base),
  Apache-2.0), and `vf search` returns hits that point back at the source file.
- Any S3-compatible object store as persistence; a local directory or memory works
  too (dev/CI emulator mode). No own storage engine — your data stays in open
  formats (Lance + JSON) in your own store.

[![CI](https://github.com/vectorfs/vectorfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vectorfs/vectorfs/actions/workflows/ci.yml)

> **Status: pre-release (v0.2).** Protocol face complete; file layer MVP for text
> (PDF / Markdown / plain text) shipped. Image modality, OCR, docx/html extractors,
> hybrid full-text search → v0.3 (see [design.md](design.md) §7).

## Five-minute quickstart

Install the `vf` CLI (default `~/.local/bin`; checksum-verified against the
release's `sha256sums.txt`):

```bash
curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.sh | bash
# or: install-cli.sh --with-server   (also installs the `vectorfs` daemon binary)
# or: install-cli.sh --version v0.1.0 --to /usr/local/bin
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

### Text-RAG mode (files in, semantic search out)

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

Files placed by ANY standard tool (aws s3 cp on the s3 backend, plain file copy on
the file backend) are picked up by the periodic scan (default 30 s) — the instance
root IS the data source; deletions are reconciled the same way (chunks GC'd).
`vf` verbs: `put` (`--async`), `search`, `sources list|status`, `get`, `rm`,
`reindex`, `status`. Everything is also on the wire as eight `/x-vf/<Op>`
operations (design.md appendix E) — same SigV4, same error shapes.

Querying through a plain AWS SDK works too (same embedding model required — the
query vector must come from `gte-multilingual-base` for the default preset):

```python
client.list_indexes(vectorBucketName="docs")           # -> derived index "text-…"
client.query_vectors(..., queryVector={"float32": q}, returnMetadata=True)
# hit metadata carries the provenance keys:
#   x-vf-src-name (file key)  x-vf-src-uri (short hash, filterable)
#   x-vf-model                x-vf-chunk-text
```

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

## Backends

- `file` (default) — the instance root is a local directory; zero configuration.
  Your files live there as plain files; vectorfs keeps everything it owns under
  the reserved `.vectorfs/` prefix (git-style mental model).
- `memory` — volatile, for CI and emulator use: `vectorfs serve --backend memory
  --no-auth` (no SigV4, no persistence; start/stop in seconds, deterministic).
- `s3` — any S3-compatible endpoint: `vectorfs serve --backend
  's3+http://host:8333/bucket[/prefix]'` with `--s3-access-key/--s3-secret-key/
  --s3-region` (or `VF_S3_*`/`AWS_*` env). Verified end-to-end on SeaweedFS 4.52
  and Cloudflare R2 (dual-SDK conformance, conditional writes, multipart).

## The file layer in brief

- Presets: `dev` (protocol only), `text-rag` (default model, zero extra config),
  `air-gapped` (local model path, `offline = true`, nothing ever leaves the network).
  `vectorfs init --preset X` materializes `.vectorfs/vectorfs.toml` and prefetches
  models when needed (`HF_ENDPOINT` mirror respected; sha256-verified, atomic).
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
  plain protocol-face vectors are never touched (tested).

## Docker

```bash
docker compose -f deploy/docker-compose.yml up        # data + models volumes, healthcheck on /health
```

Image: `ghcr.io/vectorfs/vectorfs` (multi-arch linux/amd64+arm64, debian-slim,
non-root, binaries + pinned onnxruntime/pdfium `.so`; models stay on the volume,
never in image layers). `VF_PRESET=text-rag` materializes the config on first
boot. An `emulator` compose profile runs the volatile `memory --no-auth` variant —
a seconds-to-start s3vectors stand-in for CI/testcontainers. One container per
instance root (single-writer LOCK semantics).

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

`vectorfs mcp` is a stdio MCP server for agent single-user mode — the only writer of
its data root (same lock as `vectorfs serve`):

```json
{"vectorfs": {"command": "vectorfs", "args": ["mcp", "--data-dir", "/path/to/data"]}}
```

Add `"--tools", "ro"` for a read-only toolset. Data-plane tools:
`list_vector_buckets`, `list_indexes`, `query_vectors`, `get_vectors`, `put_vectors`,
`delete_vectors`; file-layer tools: `search`, `list_sources`, `put_source`
(`put_source` is rw-only).

## Non-goals

- No own object-storage backend or storage engine — persistence is always an
  existing object store or a local directory. Files are stored as ordinary
  objects; vectorfs adds no second copy and no content-addressed store.
- No homegrown versioning, embedding-model hosting/training platform, chat UI,
  write-time entity extraction, kernel-mode implementation, plugin marketplace,
  AWS-console UI clone, non-AWS native API faces.
- No distributed multi-writer coordination (one writer per instance root; the LOCK
  object heartbeats and takeovers follow TTL semantics).

## Compatibility notes

- Error shapes match AWS exactly (`ValidationException` 400, `NotFoundException` 404,
  `ConflictException` 409, `ServiceQuotaExceededException` 402, throttling 429, …),
  emitted as both the `X-Amzn-Errortype` header and body `__type` for maximum
  client compatibility.
- The eight file-layer operations live under `/x-vf/<Op>` — an isolated namespace
  so AWS surface and superset can never collide; same SigV4, same error model,
  202 + status-polling for async verbs (design.md appendix E).
- `vf` shares its name with fish's VirtualFish — if you use both, alias as needed.
- Egress: none for the protocol face. The file layer reaches out only for model
  download (init/prefetch; `HF_ENDPOINT` mirror, standard `HTTPS_PROXY`/`NO_PROXY`
  env honored) or an explicitly configured OpenAI-compatible embedder endpoint.

## Docs

- [design.md](design.md) — positioning, protocol spec, architecture, milestones,
  and appendix E (the CLI/`/x-vf` command tables — the authoritative list).
- [spike/REPORT.md](spike/REPORT.md) — M0 measurements (latency, binary size);
  [conformance/](conformance/) — the 32-step dual-SDK conformance suite.

License: Apache-2.0 (see [LICENSE](LICENSE)).
