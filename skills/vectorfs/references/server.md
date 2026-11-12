# Daemon: setup & operations

## `vectorfs init` — materialize config

```bash
vectorfs init --preset text-rag --data-dir ./data
```

Writes `<data-dir>/.vectorfs/vectorfs.toml` (config + generated static
credentials) and prefetches models when the preset needs them. Prints the
serve command and the client env snippet.

| Preset | What |
|---|---|
| `dev` | pure protocol face — no pipeline sections (the default) |
| `text-rag` | pdf/md/txt extractors + token-window chunker + local `ort` embedder (`gte-multilingual-base`, 768-dim); init downloads the model (~1.2 GiB, progress on stderr, `HF_ENDPOINT` mirror + `HTTPS_PROXY`/`NO_PROXY` honored, sha256-verified, atomic) |
| `air-gapped` | offline: local model `path`, `offline = true`, nothing ever leaves the network — the operator supplies the model directory |

Other init flags: `--listen` / `--backend` are report-only hints shaping the
printed serve command; `--models-dir` (container volume deployments);
`--force` regenerates credentials.

## `vectorfs serve` — run the daemon

| Flag | Default | Notes |
|---|---|---|
| `--port` (`VF_PORT`) | 8787 | |
| `--listen` (`VF_LISTEN`) | 127.0.0.1 | set `0.0.0.0` in containers so `-p` mapping reaches it |
| `--data-dir` (`VF_DATA_DIR`) | `./vectorfs-data` | file-backend root + where vectorfs.toml is read |
| `--backend` (`VF_BACKEND`) | `file` | `file` \| `memory` (volatile CI/emulator) \| `s3+http(s)://host:port` (endpoint form; creds via `--s3-access-key/--s3-secret-key/--s3-region` or `VF_S3_*`, falling back to `AWS_*`) |
| `--no-auth` | off | disables SigV4 entirely (CI mode — anyone reachable can read/write) |
| `--trace` | off | one-line-JSON request/response tracing to stderr |
| `--reindex` | off | skip the fingerprint gate and re-index at startup (the remedy for "changed vectorfs.toml, serve refuses to start") |

## Docker

```bash
docker compose -f deploy/docker-compose.yml up
```

Image `ghcr.io/vectorfs/vectorfs` (multi-arch debian-slim, non-root, pinned
`.so`s; models stay on the volume). `VF_PRESET=text-rag` materializes config
on first boot. An `emulator` compose profile runs the volatile
`memory --no-auth` variant — a seconds-to-start s3vectors stand-in for CI /
testcontainers.

## Bucket model (naming IS placement)

Every vector bucket `X` is a pair of physical buckets: source bucket `X`
(pure user namespace — plain objects, zero system objects) + system bucket
`X-vf` (locks, ingest markers, derived Lance indexes). The daemon home is
`.vectorfs/` beside them (vectorfs.toml, models, same-host double-start lock).

- Source bucket is created lazily by the first `vf put` / PutSource;
  `DeleteVectorBucket` removes only `X-vf`, never `X`.
- Names: 3–60 chars (63 minus the suffix), `[a-z0-9-]`, alphanumeric edges,
  no dots, must not end with the suffix. `bucket_suffix` in vectorfs.toml is
  an install-time constant (default `-vf`); changing it selects a different
  namespace, it never migrates data.
- **Single writer per bucket** (TTL-heartbeat LOCK in `X-vf`) and per data
  root (flock). One daemon managing every bucket is the default deployment;
  multiple daemons may share one endpoint only over disjoint bucket sets.

## vectorfs.toml (pipeline side)

`[extractor.*]` (ordered fallback chains, content-routed), `[embedder.*]` (fan-out),
`[ingest.<bucket>]` (which buckets the file layer claims; `include`/`exclude`
filters; `store_chunk_text = false` to not store chunk text in metadata).
Validate with `vectorfs pipelines check` (exit 0 valid / 1 validation failure
with TOML line numbers / 2 usage).

Query-side `[ingest.<bucket>]` knobs (ranking/retrieval policy — they never
change chunk identity, so they do NOT trip the fingerprint gate):
`fts` (lexical face switch), `rerank` (cross-encoder availability),
`fusion-overfetch = N` (1..=10, default 1 — each retrieval arm of Search
fetches `min(max(topK, rerankDepth) × N, 1000)` candidates before the merge;
deeper pools give the rrf fusion and a deep rerank more to work with. The
`text-rag` preset ships `fts = true` — hybrid is the recommended everyday
mode). Exception: `fts-tokenizer` DOES trip the gate (it changes how the
FTS index tokenizes — 换分词 = reindex).

Index-side chunk-table knob: `context-header = "toc-path" | "none"`
(default `none`) inside `chunk = { … }` (R2 CCH, recall-quality roadmap).
`toc-path` prefixes each chunk's EMBEDDING INPUT with its heading path —
`文件名（去扩展）> 章节路径` — where the chapter comes from the PDF's
TOC-derived section map (the nearest preceding TOC entry for the chunk's
start page; non-PDF sources and TOC-less documents degrade to
filename-only). T18 adds the container arm: a container LEAF part (an
epub chapter) names its stamped chapter title directly — `文件名 >
章题` — from the epub `chapter-titles` knob's two-level chain (nav/NCX
label > child file-name stem; that knob lives on `[extractor.<name>]
kind = "epub"`, defaults true, and DOES trip the fingerprint gate — see
below). Two rules are load-bearing:

- **改嵌入输入 = 改块身份 = reindex**: enabling the knob changes every
  vector, so it ENTERS the pipeline fingerprint and the gate demands an
  explicit re-embed (`vectorfs serve --reindex` after flipping it; the
  default `none` keeps pre-R2 fingerprints byte-stable — flipping to
  `toc-path` triggers the same full-re-embed window as R2 prescribes).
- **The prefix never reaches storage or the lexical face**: stored/returned
  `x-vf-chunk-text`, the FTS index and BM25 all read the ORIGINAL chunk
  text. The separation protects R1's anchor-gating df statistics — a
  chapter title repeated into every chunk of its chapter would inflate
  exactly the postings counts the anchor ceiling is calibrated against.
  The reranker reads the stored original too (prefixing its input is out
  of v1's scope — the query face cannot reconstruct the prefix anyway).

**Fingerprint gate**: changing extractor/chunker/embedder config makes serve
refuse startup until an explicit re-embed — `vectorfs serve --reindex`.

## File-layer facts (v0.2)

- Modality: text only — pdf (text-based; pdfium `pdf-text` + pure-Rust
  `pdf-inspector`; scanned PDFs fail loud with an OCR-needed reason), md, txt.
  gzip files are decompressed in place during normalize. Image/OCR/docx/html
  → v0.3.
- Chunker: token-window (512 / overlap 64), char-counted, CJK-safe. Opt-in
  heading-path embedding prefix via `chunk.context-header = "toc-path"`
  (R2 CCH — see the knob notes above; embedding input only, never stored
  text).
- Default model `gte-multilingual-base` (768-dim). Query vectors must come
  from the same model — including raw `query_vectors` calls via an SDK.
- Periodic scan of source buckets: default 30 s; deletions reconciled (chunk
  GC only ever touches ingest-managed indexes whose keys match the derived
  chunk format — plain s3vectors API vectors are never GC'd).

## Ops verbs

- `vectorfs status [--data-dir D | --endpoint URL]` — data-root overview
  (LOCK holder, buckets/indexes, counts, volume) or daemon probe; `--json`.
- `vectorfs doctor --data-dir D` — writable/conditional-write probe, toml,
  LOCK state, per-index dataset health, disk headroom; `--json` prints
  `{overall, checks[]}`.
- `vectorfs pipelines check` — see above.
