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
| `--top-k K` | hits to request, 1..=1000 (default 10) |
| `--filter JSON` | metadata filter, `$`-operator syntax (below) |
| `--no-text` | omit chunk text from hits |
| `--json` | schema-versioned structured output |

MCP tool (`vf mcp` or `vectorfs mcp`): `search {bucket, text, topK, filter?,
returnText}` — same semantics, camelCase.

## Query crafting

- **Query the meaning, not keywords**: the query is *embedded*, so a full
  natural-language sentence ("how do we plan capacity for next quarter")
  usually beats a keyword list ("capacity planning").
- **Multilingual by default**: the default model (`gte-multilingual-base`) is
  multilingual — a Chinese query finds English documents and vice versa.
- **Recall then dedupe**: fetch more (`--top-k 20`) and dedupe by
  `SOURCE`/`location.key` — several chunks of one file are adjacent hits, not
  distinct answers.
- Order is deterministic: score desc, then key asc. Score is a merged
  (fan-out + merge) score — higher is better; treat it as a ranking, not a
  probability.

## Filters

`--filter '{"k": "v"}'` (implicit `$eq`) or with operators:
`$eq $ne $gt $gte $lt $lte $startsWith $in $nin $exists $and $or`.

Of the file-layer provenance keys only **`x-vf-src-uri`** is filterable.
Handy pattern: scope a query to one known file by filtering on the
`x-vf-src-uri` hash taken from an earlier hit's metadata:

```bash
vf search "rotation policy" docs --filter '{"x-vf-src-uri": "9f21ab…"}'
```

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
