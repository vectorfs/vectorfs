# 提案：词法检索与 hybrid 查询（Lance 原生 FTS）

> 状态：**已交付**（2026-11-07，M3-T9 按 ADR 0002 执行；design.md §0 T9 交付行）。本文为当时
> 设计输入，实现形态与本文三处分歧（列名 `text`、`fts` 默认关 + `fts-tokenizer` 配置键、FTS
> 索引经 `vectorfs optimize` 动词）以实现与 `docs/design/2026-11-retrieval-quality-solution.md`
> §0 对账为准。原提案状态行（proposed 2026-11-06）留档如下。
>
> 来源：dogfooding 摩擦 F1（`research/2026-11-friction-backlog.md`），
> 机制研究 `research/2026-11-retrieval-friction-mechanisms.md` §1（含外部 SOTA 与本地源码核验）。
> 对应 ADR：`docs/adr/0002-lexical-kernel-lance-native-fts.md`（accepted）；设计：`docs/design/2026-11-lexical-retrieval-hybrid.md`。

## 1. 问题

`/x-vf/Search` 只有向量单通道。嵌入把专有名词平滑成语义邻域，词法精确匹配无处发生：

- 会话实证：查「vibe coding 在哪几期出现」，嵌入检索 top-20 无一命中含该词的 chunk，被迫
  `vf get` 18MB×5 + venv + pypdf + grep——chunk 文本本来就在库里；
- 词法查询是**枚举/验证型问题**（"哪些文档含 X"），排序语义的嵌入检索原理上答不了；
- API 名、错误码、版本号等 identifier 类查询是语义检索的公认盲区（多语言 BM25 基准佐证，
  mechanisms §1.2）。

## 2. 目标 / 非目标

**目标**
1. `/x-vf/Search` 提供词法路由（BM25）与 hybrid 融合（词法 + 语义，RRF）；
2. 零新依赖：BM25 内核用 Lance 原生 FTS（锁定版本线 lancedb 0.41 / lance 14.0.2 已含，源码核验）；
3. 融合复用既有 `merge_hitlists`（RRF k=60 已实现），编排层改动最小；
4. s3vectors 协议面零变化（QueryVectors 保持纯语义）。

**非目标**
- 不做 SPLADE/学习型稀疏（多语言弱 + 第二模型，mechanisms §1.2）；
- 不做加权/分数融合（留 `Merge::Weighted` 变体，评测集就绪后开）；
- 不做 Fusion 蓝本全量查询语法（`$textMatch` 过滤算子、knn[] 多路、自定义排序——T8 后续按需）；
- 不动 M3 执行序（图像链 T1–T5 优先的裁决不变）。

## 3. 方案选项

| 选项 | 评估 |
|---|---|
| **A. Lance 原生 FTS**（推荐） | 零新依赖（tantivy 不在依赖树上）；索引挂表上，未索引行 flat 兜底 + `optimize_index` 增量并段，双索引一致性问题**按构造不存在**；BM25 分数物化返回；fuzzy 查询、FTS 前置过滤向量查询白拿。代价：分词器限 lance 菜单（无自定义钩子）；原生 FTS 比 tantivy 年轻（41M Wikipedia 压测背书） |
| B. tantivy 侧车 | 自定义分词自由（自研 CJK bigram）；但引入新依赖 + 侧车持久化/代际一致性/重建自愈一整套机制（原 D4 方案，现整套退役）；ParadeDB 先例证明可行但重复造 Lance 已内置的能力 |
| C. SPLADE 等学习型稀疏 | 英文中心多语言弱（HAKARI-Bench）；第二模型分发 + 全语料神经编码，与自托管离线定位冲突 |
| D. 不做（依赖嵌入） | 专有名词/枚举型查询原理上无解，F1 摩擦持续 |

**A 胜出的决定性事实**（本地 cargo registry 源码核验，非文档转述）：`lancedb 0.41` 的
`Index::FTS(FtsIndexBuilder)` 就是 `InvertedIndexParams` 再导出，base_tokenizer（含内置 `icu`
与 `ngram`）/stem/停用词/with_position 全配置面可用；BM25 scorer（k1/b）+ `SCORE_COL`；
`table.rs:1523-1528` 未索引行自动 flat 搜索兜底。

## 4. 关键取舍

1. **分词器**（lance 菜单内，无自研钩子——原「自研 ≤150 行 bigram」前提消失）：
   - 默认 `icu`（icu_segmenter 内置非 optional）：CJK 词典分词 + Latin 词式，中英混排最稳
     （MIRACL-zh ~0.40 档）；
   - 选项 `ngram` 2,2 全滑窗：真字符 bigram（0.54 档）但 Latin 同被 bigram 化，恰伤
     F1 的英文专有名词场景；
   - jieba/lindera：feature flag + `LANCE_LANGUAGE_MODEL_HOME` 外部模型目录，与自包含部署
     相悖，**不采**；
   - 进阶（后续）：双列双 tokenizer（icu + ngram）三路 RRF，与 fan-out/merge 同构，零机制障碍。
2. **chunk 文本升列**：FTS 建在派生表顶层 string 列；metadata blob 的 `x-vf-chunk-text` 键保留
   （回显语义不变）。列与 blob 键同受 `store_chunk_text` 开关管辖：关则词法路由不可用（fail loud）。
3. **查询面形态**：`searchMode: semantic|keyword|hybrid` 超集参数（v1），不是 Fusion 式
   `$textMatch` 过滤算子——先解决 F1，语法蓝本留给 T8+。
4. **降级语义**：配置缺失（无列/无索引/store_chunk_text=false）→ 400 fail loud；索引落后
   （未索引行）→ Lance flat 兜底自动，无感知。

## 5. 依赖与风险

- 依赖：chunk 文本列 schema 演化（AllNulls `add_columns` 已有）；Search 路由循环接纳非
  embedder 路由（唯一编排改动）；tokenizer 进管线指纹。
- 风险：原生 FTS 成熟度（缓解：守默认 block 128/v2 格式，查询面薄 trait 保换核退路）；
  `icu` 中文召回不及 bigram（缓解：`ngram` 选项 + 进阶双列形态留门）；BM25 单路分数语义
  需文档言明（无界原始分，仅单调可比）。
