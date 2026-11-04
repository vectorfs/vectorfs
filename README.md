# vectorfs

**Turn any S3 into S3 Vectors. Single binary. Point your AWS SDK at it.**

- Single static binary — no daemon fleet, no containers, starts in seconds.
- Any S3-compatible object store as persistence; a local directory or memory works too (dev/CI emulator mode).
- Official AWS SDKs (boto3, aws-sdk-go-v2) connect unmodified, SigV4-verified,
  strongly consistent read-after-write. No bundled embeddings — it stores the vectors your client sends.

[![CI](https://github.com/OWNER/vectorfs/actions/workflows/ci.yml/badge.svg)](https://github.com/OWNER/vectorfs/actions/workflows/ci.yml)
<!-- TODO(repo public): replace OWNER with the GitHub org once the repo is public -->

> **Status: pre-release.** Protocol face complete: all 21 `s3vectors-2025-07-15`
> operations conformance-tested through boto3 and aws-sdk-go-v2 (32 steps each,
> green in CI). Superset features (hybrid search, full-text) in progress — see
> [design.md](design.md).

## Five-minute quickstart

Download a release tarball for Linux x86_64/aarch64 from
[Releases](https://github.com/OWNER/vectorfs/releases) (published on `v*` tags with
`sha256sums.txt`), or build from source (Rust 1.94+, C toolchain, cmake, protoc):

```bash
cargo build --profile release-prod --workspace   # -> target/release-prod/vf
```

Initialize a data root and start the server:

```bash
vf init --data-dir ./vectorfs-data     # generates + prints the access key / secret
vf serve --data-dir ./vectorfs-data    # listens on http://127.0.0.1:8787
```

Point boto3 at it (credentials from `vf init`; every command here was executed against a fresh `vf init`):

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
    QueryVector:      &types.VectorDataMemberFloat32{Value: []float32{1, 0, 0}},
    TopK:             aws.Int32(2),
    ReturnDistance:   true,
})
```

## Backends

- `file` (default) — data root is a local directory; zero configuration.
- `memory` — volatile, for CI and emulator use: `vf serve --backend memory --no-auth`
  (no SigV4 verification, no persistence; start/stop in seconds, deterministic).
- `s3` — any S3-compatible endpoint: `vf serve --backend 's3+http://host:8333/bucket[/prefix]'`
  with `--s3-access-key/--s3-secret-key/--s3-region` (or `VF_S3_*`/`AWS_*` env).
  Verified on SeaweedFS 4.52 (full conformance + conditional writes). AWS S3, R2
  and MinIO support `If-None-Match:*` per their docs; real-endpoint conformance
  runs are pending cloud credentials.

## What works / what's different

- All 21 operations of the `s3vectors-2025-07-15` API, including pagination,
  segments, tags, policies (stored + echoed, not enforced) and index modes.
- Every query is filter-before-KNN: both CLASSIC and ENHANCED indexes behave as
  ENHANCED — no post-filter recall loss.
- Strong read-after-write: Lance manifest commits are the atomicity boundary; a vector
  written one millisecond ago is immediately visible, and pagination reads a snapshot
  (writes don't shift results mid-page).
- Exit path: the data root is open formats (Lance + JSON catalog). If the daemon
  disappears, any Lance tooling can still read your vectors.

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
| ListVectors maxResults | 1–1000 (default 500) | same (enforced) |
| ListVectorBuckets/ListIndexes maxResults | 1–500 + prefix | same (enforced) |
| Segment count/index | 1–16 / 0–15, paired | same (enforced) |
| Vector values | cosine rejects zero vectors; NaN/Inf rejected | same (enforced) |
| QPS / throughput quotas | yes | **not enforced** |
| Total vectors / bucket+index count caps | 2B / 10,000 | **not enforced** |

## MCP

`vf mcp` is a stdio MCP server for agent single-user mode — the only writer of its data root (same lock as `vf serve`):

```json
{"vectorfs": {"command": "vf", "args": ["mcp", "--data-dir", "/path/to/vectorfs-data"]}}
```

Add `"--tools", "ro"` for a read-only toolset. Tools: `list_vector_buckets`,
`list_indexes`, `query_vectors`, `get_vectors`, `put_vectors`, `delete_vectors`.

## Non-goals

- No own object-storage backend or storage engine — persistence is always an
  existing object store or a local directory.
- No homegrown versioning, embedding-model hosting, chat UI, write-time entity
  extraction, kernel-mode implementation, plugin marketplace, AWS-console UI
  clone, non-AWS native API faces.
- No distributed multi-writer coordination (v1: one writer per data root).

## Compatibility notes

- Error shapes match AWS exactly (`ValidationException` 400, `NotFoundException` 404,
  `ConflictException` 409, `ServiceQuotaExceededException` 402, throttling 429, …),
  emitted as both the `X-Amzn-Errortype` header and body `__type` for maximum
  client compatibility.
- Known pending items: a few wire-level details (error body nuances, ARN
  encoding in tag paths) are SDK-round-trip-verified but not yet cross-checked
  against the real AWS endpoint — tracked in [design.md](design.md) appendix B.

## Docs

- [design.md](design.md) — positioning, protocol spec, architecture, milestones.
- [spike/REPORT.md](spike/REPORT.md) — M0 measurements (latency, binary size);
  [conformance/](conformance/) — the 32-step dual-SDK conformance suite.

License: Apache-2.0 (see [LICENSE](LICENSE)).
