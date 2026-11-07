---
status: accepted
---

# 词法检索内核采用 Lance 原生 FTS（tantivy 侧车不建）

**已执行**（M3-T9，2026-11-07 交付；design.md §0 2026-11-06 裁决行 + 2026-11-07 T9 交付行）。
实现形态为权威，与本 ADR 起草稿的三处分歧（均实现方更优或等效）：列名 `text`（非 `chunk_text`）；
`fts` 默认关 + `fts-tokenizer` 配置键（非 `lexical` 单键）；FTS 索引经 `vectorfs optimize` 动词
创建（非 ensure_domain 惰性建——守「daemon 从不自行建索引」纪律）。四路审查结论与完整对账见
`docs/design/2026-11-retrieval-quality-solution.md` §0。

词法检索（M3 T8 `$textMatch`）的 BM25 内核**直用 Lance 原生 FTS**：锁定版本线 lancedb 0.41 / lance 14.0.2 已含（依赖树无 tantivy，registry 源码核验 2026-11-06），`Index::FTS` 全配置面 + `full_text_search` 查询 + BM25 分数物化。tantivy 侧车方案（含 `X-vf/indexes/` 持久放置、代际元数据、重建自愈）整套退役——索引挂在派生表上，未索引行 flat 兜底 + `optimize_index` 增量并段，双索引一致性问题按构造不存在。方向源于用户质询「直接使用 Lance 原生 FTS？」（2026-11-06），机制研究 `research/2026-11-retrieval-friction-mechanisms.md` §1 核验背书。

## Considered Options

- **tantivy 侧车**——自定义分词自由（自研 CJK bigram）、ParadeDB 生产先例；但新依赖 + 一整套装载/一致性/换代机制，且重复 Lance 已内置能力，出局
- **SPLADE/学习型稀疏**——英文中心多语言弱（HAKARI-Bench）、第二模型分发与全语料神经编码，与自托管离线定位冲突，出局
- **延迟不做**——F1 摩擦（专有名词/枚举型查询原理上非嵌入所长）持续，出局

## Consequences

- 原「CJK 零词典 bigram 自研 ≤150 行、禁引 lindera/gse」裁决**前提消失**（lance 无自定义 tokenizer 钩子）：分词器在 lance 菜单内选——默认 `icu`（内置词典、中英混排稳），选项 `ngram 2,2`（真 bigram、中文最优但 Latin 同被 bigram 化）；jieba/lindera 需外部模型目录，维持禁引
- 前置：chunk 文本升派生表顶层 string 列（metadata blob 键保留作回显）；tokenizer 进管线指纹（换分词 = reindex）
- `/x-vf/Search` 增 `searchMode: semantic|keyword|hybrid` 超集参数；协议面 QueryVectors 保持纯语义（S3 Vectors 无 hybrid，2026-11 双处官方文档确认）
- 内核以 `/x-vf/` 查询面薄 trait 隔离，保留未来换核（含 Lance 原生 FTS 演进或回退 tantivy）退路
- 成熟度守则：block_size 守默认 128（256/v3 实验性）；JSON tokenizer 已知 panic（lance#8812）不触
