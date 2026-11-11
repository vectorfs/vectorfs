---
name: vf-search
description: Semantic file search over a vectorfs deployment — find documents by meaning, not keywords. Use when searching a document/knowledge base backed by vectorfs, via the vf CLI (`vf search`) or the `search` MCP tool of `vf mcp` / `vectorfs mcp`: crafting queries, metadata filters, interpreting source-attributed hits, and pulling the full original file behind a hit. Not for installing or operating vectorfs itself — that is the `vectorfs` skill.
license: Apache-2.0
---

# vf-search

`vf search` is the retrieval verb: your query text is embedded by the same
model that indexed the files, matched against every chunk, merged and ranked —
each hit is a passage attributed to its source file. One command:

```bash
vf search "capacity planning" docs
#  #  SCORE   KEY                  INDEX         SOURCE         TEXT
#  1  0.71    510da8…-text-0       text-fcdd…    report.pdf     Capacity planning for …
```

## Preconditions (30 seconds)

- A `vectorfs serve` daemon is running and `vf` is configured — check with
  `vf status`. Not configured yet: `vf config init --endpoint … --access-key …
  --secret-key …` (resolution: `--flag` > `VF_*` env > config file).
- The bucket is claimed by the file layer and its files are indexed:
  `vf ls docs --status PENDING` shows what is still in flight.
- No deployment exists at all? Setting one up is the `vectorfs` skill's job.

## Search

CLI (bucket as a bare positional, XOR `--bucket`):

| Flag | Meaning |
|---|---|
| `--mode M` | `semantic` (default) \| `keyword` (pure BM25) \| `hybrid` (**recommended** — both arms fused; needs the bucket's `fts = true`, which the `text-rag` preset ships) |
| `--rerank` | cross-encoder re-scoring of the merged top-K (needs `[ingest] rerank = true`; semantic/hybrid only — keyword refuses) |
| `--rerank-depth N` | rerank candidate depth: re-score the first N merged candidates, still return `--top-k` (1..=200, ≥ top-k, needs `--rerank`, default = top-k). CPU budget: ~148 ms/pair on CPU — depth 20 ≈ 3 s, depth 50 ≈ 7.4 s |
| `--per-source N` | per-source quota: at most N hits per source file in the merged order, before top-K truncation (1..=100, default off). Fewer than top-k hits come back when the quota thins the set — no backfill |
| `--top-k K` | hits to request, 1..=1000 (default 10) |
| `--source KEY` | restrict to one source file by its key (repeatable = several files) — the server translates names to the `x-vf-src-uri` hash, no digging |
| `--kind K` | admit an excluded structure kind (repeatable): `toc` and `legend` chunks are out of the default result set; `--kind toc` brings them back (allowed: body, toc, caption, legend) |
| `--filter JSON` | metadata filter, `$`-operator syntax (below) |
| `--no-text` | omit chunk text from hits |
| `--json` | schema-versioned structured output |

MCP tool (`vf mcp` or `vectorfs mcp`): `search {bucket, text, topK, filter?,
returnText}` — same semantics, camelCase.

### Anchor gating (R1→T17 — automatic, default on for fts buckets)

On `keyword`/`hybrid` text queries the daemon first probes the query's
quoted spans (any script, whole) and analyzer terms — the SAME tokenization
pipeline the FTS index was built with, so CJK dictionary words, stemmed
Latin and folded accents are candidates exactly as the index sees them —
by POSTINGS COUNT (≤8 small single-term FTS queries at the cap, plus exact
counts when the head ties at the cap); the RAREST df≥1 candidate (argmin,
ties first-seen) gates the **lexical arm** (chunks missing it leave it; the
whole query stays the ranking body). CJK queries gate like any other;
only a query whose every candidate is corpus-absent (df=0) rides the
ungated behavior.

- **Misjudgment cost**: the gate only tightens the lexical arm — the
  semantic arm is untouched. A misjudged anchor costs the lexical
  *enhancement*, never a result; worst case is the ungated ranking.
- The response tells you what happened: an `anchors` array beside `hits`
  — the probed candidates that exist in the corpus, df ascending; the
  FIRST term is what gated, whatever its df (e.g. `["vibe", "code", …]`
  — the pipeline's lowercase/stemmed spellings); `[]` = every candidate
  df=0; absent = analysis not applicable. The human CLI prints one stderr
  line.
- Off switch (server config): `[ingest.<bucket>] anchor-gating = false` —
  query-face policy, no reindex.

## Query crafting

- **Prefer `--mode hybrid`** (the recommended everyday mode): exact terms,
  proper nouns, and error codes are keyword/hybrid's strength and a
  pure-semantic blind spot — measured evidence, not taste. Pure `semantic`
  shines for paraphrase/meaning queries; pure `keyword` when you want BM25's
  exact-term contract untouched.
- **Query the meaning, not keywords**: the query is *embedded*, so a full
  natural-language sentence ("how do we plan capacity for next quarter")
  usually beats a keyword list ("capacity planning").
- **Multilingual by default**: the default model (`gte-multilingual-base`) is
  multilingual — a Chinese query finds English documents and vice versa.
- **Recall then dedupe**: fetch more (`--top-k 20`) and dedupe by
  `SOURCE`/`location.key` — several chunks of one file are adjacent hits, not
  distinct answers. `--per-source 2` enforces the spread server-side when one
  fat document crowds the head (fewer hits return, honestly).
- Order is deterministic: score desc, then key asc. Score is a merged
  (fan-out + merge) score — higher is better; treat it as a ranking, not a
  probability.

## Filters

`--filter '{"k": "v"}'` (implicit `$eq`) or with operators:
`$eq $ne $gt $gte $lt $lte $startsWith $in $nin $exists $and $or`.

File-layer keys: **`x-vf-src-uri`** (the source's content hash) is
filterable. On the search face, **`x-vf-src-name`** — the source file's
key, human-readable — also filters (implicit `$eq`, `$eq`, `$in`; other
operators are refused with a pointer to `x-vf-src-uri`): the server
translates names to hashes. Scoping to known files needs no hash digging
anymore:

```bash
vf search "rotation policy" docs --source docs/report.pdf
vf search "评估" docs --filter '{"x-vf-src-name": {"$in": ["radar/v31.pdf", "radar/v32.pdf"]}}'
```

Structure: `x-vf-chunk-kind` (`body` / `toc` / `caption` / `legend`)
filters explicitly, and `toc` + `legend` are excluded from results by
default (TOC lines are perfect lexical matches that霸榜; legend blocks are
pathologically short) — `--kind` (or `includeKinds` on the wire) admits
them. A zero-hit search whose src-name filter matched no indexed source
says so in a `note` — check the key spelling (`vf ls`) before concluding
the corpus lacks the answer.

## From hit to full document

A hit carries the passage (`content.text` / the TEXT column) plus its source:
`SOURCE` column = `location.key` = the source object key; `KEY` = the chunk
key; `INDEX` = the derived index. To read the whole original file behind a
hit:

```bash
vf cat docs/report.pdf        # original bytes to stdout
vf get docs/report.pdf --out report.pdf
```

MCP: `get_source_data {bucket, key}` — the entire original file (lossy UTF-8
text; not for binary formats).

## Troubleshooting

| Symptom | Meaning / fix |
|---|---|
| Validation error naming the bucket | bucket has no `[ingest.<bucket>]` section — server-side config, see the `vectorfs` skill |
| Zero hits on a fresh upload | indexing still in flight (periodic scan, default 30 s) — `vf ls docs --status PENDING`; `vf put` foreground already waits |
| A source stays FAILED | `vf sources status <key> --bucket docs` shows `statusReason` (e.g. a scanned PDF fails loud: OCR lands in v0.3) |
| `ENDPOINT_UNREACHABLE` | daemon down or `vf` misconfigured — `vf status` |

## See also

The full `vectorfs` skill (same `skills/` root): install/setup, every verb,
both MCP entries, daemon ops, and the wire faces.
