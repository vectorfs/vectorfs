# Wire faces: s3vectors API + vectorfs API

One daemon, two contracts. Auth is SigV4 (signing name `s3vectors`) on both;
`--no-auth` disables verification for CI. Errors match AWS exactly —
`ValidationException` 400, `NotFoundException` 404, `ConflictException` 409,
`ServiceQuotaExceededException` 402, throttling 429 — emitted as both the
`X-Amzn-Errortype` header and body `__type`.

## boto3 against the s3vectors face

```python
import boto3
client = boto3.client("s3vectors", endpoint_url="http://127.0.0.1:8787",
                      aws_access_key_id="...", aws_secret_access_key="...",
                      region_name="us-east-1")
client.create_vector_bucket(vectorBucketName="docs")
client.create_index(vectorBucketName="docs", indexName="chunks",
                    dataType="float32", dimension=3, distanceMetric="cosine")
client.put_vectors(vectorBucketName="docs", indexName="chunks", vectors=[
    {"key": "a", "data": {"float32": [1.0, 0.0, 0.0]},
     "metadata": {"kind": "doc", "len": 4}},
])
res = client.query_vectors(vectorBucketName="docs", indexName="chunks",
                           queryVector={"float32": [1.0, 0.0, 0.0]}, topK=2,
                           filter={"kind": {"$eq": "doc"}, "len": {"$gte": 5}},
                           returnDistance=True, returnMetadata=True)
```

aws-sdk-go-v2: `s3vectors.NewFromConfig(cfg, func(o *s3vectors.Options) {
o.BaseEndpoint = aws.String("http://127.0.0.1:8787") })`.

To find the file layer's derived index: `client.list_indexes(
vectorBucketName="docs")` → index named `text-…`; query it with a vector from
the same embedding model (`gte-multilingual-base`, 768-dim by default) and
`returnMetadata=True` — hit metadata carries the provenance keys below.

## The vectorfs API: 8 operations under `/x-vf/<Op>`

Same SigV4, same error model; camelCase fields ↔ kebab-case CLI flags;
**async verbs return HTTP 202 + status polling**.

| Op | Input → Output |
|---|---|
| `PutSource` | bucket, key (header), body = raw bytes → 202 `{key, status: "PENDING"}`; lazily creates the source bucket; >100 MB multipart |
| `GetSource` | bucket, key → `{status, statusReason?, retriesExhausted?, mime, etag, pipelineFingerprint, chunks[]}`; states PENDING / IN_PROGRESS / PARTIALLY_INDEXED / INDEXED / FAILED / DELETING |
| `GetSourceData` | bucket, key → the original byte stream |
| `ListSources` | bucket, `status?`, `maxResults` 1–1000 (default 500), `nextToken` → sources[] |
| `DeleteSource` | bucket, key → 202 `{status: "DELETING"}` (async chunk GC) |
| `Search` | bucket, `query{text}`, `topK` 1–1000, `filter?`, `returnText?`, `searchMode?` (semantic default \| keyword \| hybrid — recommended), `rerank?`, `rerankDepth?` (1..=200, ≥ topK; default = topK), `perSource?` (0..=100, 0 = off) → hits[] (below) — the flagship op: fan-out + merge (+ per-source quota) + optional cross-encoder rerank + source attribution |
| `Reindex` | bucket → 202 `{jobId}`; progress via GetIngestStatus |
| `GetIngestStatus` | bucket? → status counts + scan statistics + reindex progress |

`Search` hit shape: `content{text, type: "TEXT"}`, `score` (merged), optional
`distance`, `index` + `key` (derived index + chunk key),
`location{type: "source", key, page?}`, `metadata`.

## Provenance metadata (`x-vf-*`, on every chunk vector)

| Key | Budget | Filterable |
|---|---|---|
| `x-vf-src-uri` | ≤ 100 B (object-key hash) | **yes** |
| `x-vf-src-name` | full object key | no |
| `x-vf-model` | model id | no |
| `x-vf-modality` | e.g. `text` | no |
| `x-vf-chunk-text` | the chunk (non-filterable) | no — omit per ingest section with `store_chunk_text = false` |

## Limits (structural enforced exactly as AWS; quotas NOT enforced)

- PutVectors ≤ 500 vectors, body ≤ 20 MiB; GetVectors keys ≤ 100;
  DeleteVectors keys ≤ 500
- Index dimension 1–4096, fixed at create; metadata ≤ 50 keys / ≤ 40 KB
  (filterable keys ≤ 2 KB); nonFilterableMetadataKeys 1–10, immutable
- QueryVectors topK ≤ 10,000 (100/page + nextToken); filter constraints ≤ 100
- ListVectors maxResults 1–1000 (default 500); List* 1–500 + prefix
- Segments per index 1–16 (0–15 paired); cosine rejects zero vectors;
  NaN/Inf rejected
- QPS / throughput / total-vector quotas: **not enforced** locally
