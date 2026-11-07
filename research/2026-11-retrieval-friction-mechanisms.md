# 检索质量摩擦的机制分析（2026-11-06）

> 姊妹篇：`2026-11-friction-backlog.md`（摩擦清单 F1–F9 与会话实证）。本文是深度分析：每项摩擦
> 先钉死仓库内部机制事实（代码/设计文档，本会话逐处核验），再叠外部证据（官方文档/论文/源码，
> 两路调研代理一手核对，来源随文），最后给机制级方案与待裁决点。纪律：**从机制上补齐缺口，
> 不打补丁**；不重开 M3 执行序裁决（2026-11-06）；不动 s3vectors 兼容面。

---

## 0. 病根总述

dogfooding 会话（技术雷达 5 期 PDF 语义问答）暴露的九项摩擦收敛为**两个交界处的信息丢失**：

1. **extract→chunk 交界**：§5.10.2 契约「一份逻辑文档、一个提取产物」+ `pdf_text.rs` 页循环
   `"\n\n"` 直拼——页边界、字号、书签在提取层全部丢弃，下游再无结构信号可用（F2 目录霸榜 /
   F3 页眉页脚混入 / F4 图例混排的共同病根）。
2. **query 交界**：`/x-vf/Search` 只有向量单通道——嵌入把专有名词平滑成语义邻域，词法精确匹配
   无处发生（F1），命中的验证与放大也无工具（F5/F6）。

一句话：**文档结构在入口被压平，查询词法在出口缺席**。两者都该按机制补，而不是在检索结果上打补丁降权。

---

## 1. F1：词法通道与 hybrid 融合（归宿 = M3 T8 `$textMatch`，本文为其设计输入）

### 1.1 仓库内部事实（已核验）

| 事实 | 位置 |
|---|---|
| RRF 融合已实现：`1/(60+rank)` 累加、tie-break key asc、单路恒等直通 | `pipeline/mod.rs:632,644`（`RRF_K=60`，注释引 Cormack 2009） |
| fan-out 结构天然接受新路由：Search 按 embedder 展开 HitList 列表再 merge | `ops_xvf.rs:762-867` |
| `Merge` 枚举闭集，`weighted`/`first` 注释明言 M3 加变体 | `pipeline/mod.rs:599-619` |
| T8 已规划：`$textMatch`+tantivy；**CJK 零词典 bigram tokenizer，自研 ≤150 行，禁引 lindera/gse**；稀疏检索不走向量列（float32 定长协议装不下） | design.md §0 2026-11-06 行、依赖表「tantivy + CJK bigram 推迟 M3」、附录C Fusion 蓝本 |
| 现依赖：`lancedb 0.41` + `lance 14.0.2` | `crates/server/Cargo.toml` |

### 1.2 外部证据

**RRF(k=60) 是无评测集时的业界共识默认**（四家默认值一致）：

| 系统 | 默认融合 | 备注 |
|---|---|---|
| Elasticsearch | RRF retriever，`rank_constant=60` | 官方理由：无需调参、各路相关性指标无需可比 |
| OpenSearch | 归一化默认，2.19+ RRF 处理器（k=60） | 实测 RRF 比归一化融合 NDCG@10 低 3.86%，换延迟更低、抗漂移、免重校准 |
| Milvus | RRFRanker k=60；全文检索 BM25 内建为 schema 级 Function | WeightedRanker 走类 arctan 归一 |
| Qdrant | RRF 为「安全默认」，v1.17+ weighted RRF | 官方决策表：有评测集→调权重；无→RRF。明确「α 加权原始分数不可靠」 |

k=60 出处：Cormack/Clarke/Buettcher-Simcha SIGIR 2009，经验平滑常数（抑制头部名次独大），非理论推导。
归一化问题：BM25 无界（常见 0–30+）vs cosine [-1,1]，任何分数融合必须先归一化或改用名次——RRF 用名次绕开。

来源：[ES RRF 文档](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/reciprocal-rank-fusion) ·
[OpenSearch RRF 博客 2025-02](https://opensearch.org/blog/introducing-reciprocal-rank-fusion-hybrid-search) ·
[Qdrant Hybrid Queries](https://qdrant.tech/documentation/search/hybrid-queries/) ·
[Milvus Full Text Search](https://milvus.io/docs/full-text-search.md) ·
[Weaviate 融合深潜（relativeScoreFusion 反例：调优后 +6% recall）](https://weaviate.io/blog/hybrid-search-fusion-algorithms) ·
[ParadeDB Hybrid Manual](https://www.paradedb.com/blog/hybrid-search-in-postgresql-the-missing-manual) ·
[Lee SIGIR 1997（CombMNZ 最优的分布融合基线）](https://dl.acm.org/doi/pdf/10.1145/258525.258587)

**CJK 分词的定量裁决**（MIRACL-zh，同库同 BM25 只换 tokenizer）：词式切分 **0.012**（98% 查询空结果）→
jieba 0.40 → ES cjk 二元 0.49–0.52 → **字符 bigram 0.54** → 多语言 BPE 0.53。
来源：[One Analyzer, Every Language（2026-09，一手核对数字）](https://www.kshivendu.dev/blog/token-search-multilingual)。
⇒ **仓库既有裁决（零词典 bigram）被数据正面验证：不仅是省依赖，是得分更高的选择**。统一 BPE
（复用 gte 自身 tokenizer，中文 0.53 胜 jieba、与嵌入侧子词对齐）可作实验性第三路 analyzer。

**tantivy 生态**：0.27.0（2026-10-17）持续发版，MIT，16.4k stars；ParadeDB pg_search 即「tantivy 作
BM25 侧车」的生产先例。索引大小与 Lucene 相差 ±1%，查询期 mmap、写入期内存可用
`with_memory_budget` 控制。来源：[tantivy releases](https://github.com/quickwit-oss/tantivy/releases)、
[size 对比 issue#465](https://github.com/quickwit-oss/tantivy/issues/465)、
[内存 issue#666](https://github.com/quickwit-oss/tantivy/issues/666)。

**⚠️ 战术级发现（2026-11-06 本地源码复核，改写 D4）：Lance 原生 FTS 已在 vectorfs 锁定的版本线内**。
lancedb 0.41 / lance 14.0.2 的依赖树**不含 tantivy**（lancedb、lance、lance-index 的 Cargo.toml 零命中）；
`Index::FTS(FtsIndexBuilder)` 即 `InvertedIndexParams` 再导出（base_tokenizer / language / stem /
停用词 / ngram / with_position 全配置面）；`full_text_search(FullTextSearchQuery)` 查询，BM25 打分
（k1/b 标准实现，`SCORE_COL` 物化分数）；未索引行查询自动 flat 兜底 + `optimize_index` 增量并段
——lancedb#2998 弃 tantivy 的「与数据同步更好」按构造成立。lancedb#2998 与
[Lance 倒排索引设计](https://github.com/lancedb/lance/issues/1195)、
[41M Wikipedia 压测](https://www.lancedb.com/blog/feature-full-text-search)是上游背景；
**本地事实以 registry 源码核验为准**（`lance-index-14.0.2/src/scalar/inverted/`、
`lancedb-0.41.0/src/index.rs:80-105`、`table.rs:1523-1528`）。

**S3 Vectors 无 hybrid，官方两处明确**（2026-11 复核）：主文档把 hybrid 指给 OpenSearch；Bedrock KB
集成页 Limitations 首条 "Semantic search only"。AWS 自己的参考架构（S3 Vectors 存向量 + OpenSearch
做 BM25 + Lambda 侧 RRF）就是「侧车索引 + 查询层融合」——与 vectorfs 的分层（协议面 QueryVectors 纯语义
+ `/x-vf/Search` 超集 hybrid）完全同构。来源：[S3 Vectors 用户指南](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors.html)、
[Bedrock KB 集成](https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors-bedrock-kb.html)。

**SPLADE 不做（短期明确）**：学习型稀疏英文中心（HAKARI-Bench 551 多语言任务绝对分低，作者明言
应固定语言比较）、需第二模型 + 全语料神经编码，与自托管离线定位冲突；Milvus 亿级系统也选经典 BM25
内建。来源：[HAKARI-Bench](https://arxiv.org/html/2606.22778v1)。

### 1.3 对 T8 的机制建议（2026-11-06 修订：直用 Lance 原生 FTS，弃 tantivy 侧车）

1. **BM25 内核 = Lance 原生 FTS，不引 tantivy，不建侧车**。锁定版本线已含全部所需（§1.2 复核）：
   索引挂表上，未索引行 flat 兜底 + `optimize_index` 增量并段——原侧车方案的双索引一致性、
   `X-vf/indexes/` 持久放置、代际元数据、重建自愈**整套机制不再需要**。「内核抽象」降级为
   `/x-vf/` 查询面的薄 trait（查询面隔离本来就想要），不再是侧车换核的预留。
2. **前置：chunk 文本升列**。FTS 建在派生表顶层 string 列上——`x-vf-chunk-text` 目前住在 metadata
   blob（nonFilterable 无镜像列），需 schema 加列（server-internal，协议面读 key/metadata/vector
   不可见）；走既有 schema 演化 + 指纹/reindex 语义。metadata blob 内的 `x-vf-chunk-text` 键保留
   （MCP/GetVectors 回显语义不变），列是并行的索引侧真身。
3. **分词器 = lance 菜单内选择，无自定义钩子**（原「自研 ≤150 行 bigram」裁决的前提消失，D5 改判）：
   - 默认 **`icu`**（内置 icu_segmenter，非 optional）：CJK 词典分词 + Latin 词式，中英混排最稳，
     ~jieba 档（MIRACL-zh 0.40 档）；
   - 选项 **`ngram` 2,2 全滑窗**（`prefix_only=false`）：真字符 bigram（0.54 档）但 Latin 同被
     bigram 化——英文精确词退化配对袋，恰伤 F1 的专有名词场景；
   - jieba/lindera 需 feature flag + `LANCE_LANGUAGE_MODEL_HOME` 外部模型目录，与自包含部署相悖，
     **维持禁引**；
   - 进阶形态（T8 首切不做、机制上零障碍）：两列两 tokenizer（icu 列 + ngram 列）→ 三路 RRF，
     与现有 fan-out/merge 同构。
   tokenizer 选择进管线指纹（换 tokenizer = 换索引 = reindex，与既有指纹语义一致）。
4. **融合在查询层**：fan-out 加词法路由（词法路由不是 embedder——Search 的路由循环需接纳非 embedder
   路由，这是 T8 唯一的编排改动），两路各取 N ≥ 2×top-k，RRF(k=60) 后截断；`searchMode:
   semantic|keyword|hybrid` 超集参数。白拿能力一并接：fuzzy 查询（编辑距离）、FTS 作向量查询
   前置过滤（`base.full_text_search` 与 vector query 可组合）。演进钩子三条全留：weighted RRF
   （Qdrant v1.17 形状 `1/(k+(rank+1)/w−1)`）、relativeScoreFusion（Weaviate 形状）、未来稀疏向量
   只换检索路不动融合层。
5. **协议面零变化**：QueryVectors 保持纯语义——S3 Vectors 无 hybrid 且 2026-11 未变，兼容面加
   hybrid 反而制造不可对标差异。
6. **成熟度守则**：block_size 守默认 128（256/v3 格式官方标注实验性）；JSON tokenizer 有已知 panic
   bug（lance#8812）——我们只用 text 粒度，不触；原生 FTS 比 tantivy 年轻，41M Wikipedia 压测背书，
   且查询面薄 trait 保住将来换核退路。

### 1.4 边界

本节为 T8 设计输入，不触发提前执行（F1 的会话证据确认 T8 必要性，但 T6/T7 无依赖变化）。

---

## 2. F2/F3/F4：文档结构识别（新机制提案，本 backlog 唯一需要从零设计的项）

### 2.1 仓库内部事实（已核验）

| 事实 | 位置 |
|---|---|
| 提取契约：一份逻辑文档一个提取产物，页级切块是 chunker 的事；页文本 `"\n\n"` 直拼成单 Part | `extract/pdf_text.rs:46-48` |
| `Part{text,image}`、`Chunk` 均无结构通道 | `pipeline/mod.rs:228,358` |
| chunk metadata 键集（§5.10.1）在 `build_rows` 统一注入，40KB 预检 | `ingest/mod.rs:2275-2308` |
| Lance metadata 走动态镜像列：scalar filterable 键建列，nonFilterable/list 键 blob-only；新键 AllNulls `add_columns` 自动演化，同 key 标量类型必须永久稳定 | `store.rs:33-53,1149,1417` |
| `x-vf-src-name`/`x-vf-chunk-text` 声明 non-filterable（协议面过滤即 400） | `ingest/domain.rs:172-175` |
| **pdf-inspector 提取器默认 `markdown=false`**——crate 内置的家具剔除（`furniture.rs`）与 TOC 行信号（`analysis.rs`）只在 markdown 管线激活 | `extract/pdf_inspector.rs:88-112` |
| 依赖 pdf-inspector 1.25.2；pdfium 经 pdfium-render 0.9.5 | `Cargo.toml`、引擎状态页 |

### 2.2 外部证据

**布局感知解析器对照**（结构治理的行业标准形态）：

| 系统 | 页眉页脚 | TOC | 机制形态 |
|---|---|---|---|
| docling（IBM） | 识别为 furniture 层，默认导出只取 body 层 | `DOCUMENT_INDEX` 标签、**默认保留**，用户手动删 | 双轴：语义 label × 内容层（body/furniture/…），检测在解析层、策略在导出层 |
| marker | `PageHeader/Footer` 默认 `ignore_for_output`，显式选项保留 | `TableOfContents` 独立块类型 + `DocumentTOCProcessor` 从标题**生成导航元数据** | 检测在布局层、剔除是默认策略 |
| MinerU | YOLO 布局 `abandon` 类直接丢弃 | — | 检测即丢弃（模型路线） |
| unstructured | hi_res 分类 `Header/Footer/PageNumber`；fast 靠后处理去重 | 无专项 | 按类别过滤是标准用法 |
| Bedrock KB | 无任何 TOC/boilerplate 专项 | 无 | 路线 = 结构化 markdown + `document-page-number` 元数据，治理留给解析质量 |

来源：[docling-core labels/content_layer](https://github.com/docling-project/docling-core/blob/main/docling_core/types/doc/labels.py)、
[docling 技术报告](https://arxiv.org/html/2408.09869v1)、[marker pagefooter 默认剔除](https://github.com/datalab-to/marker/blob/master/marker/schema/blocks/pagefooter.py)、
[unstructured 元素类别](https://docs.unstructured.io/concepts/document-elements)、[AWS kb-chunking](https://docs.aws.amazon.com/bedrock/latest/userguide/kb-chunking.html)。

**页眉页脚检测三家算法对照**（纯文本侧标准算法 = 跨页首/尾区重复一致性）：

| 维度 | marker | Haystack | pdf-inspector（现依赖！） |
|---|---|---|---|
| 候选集 | 每页首/尾块 | 每页前后 300 字符 | 行级（x/y/字号），文档级 + 短文档双策略 |
| 判定 | 频次 ≥20% 页数或连击 ≥3，模糊匹配 >90 | 首尾页集合最长公共 n-gram | 跨页重复证明（≥3 页）；短文档 gap ≥1.8× 中位行距 + ≤2 行 + ≤90 字符 |

来源：[marker ignoretext.py](https://github.com/datalab-to/marker/blob/master/marker/processors/ignoretext.py)、
[Haystack document_cleaner.py](https://github.com/deepset-ai/haystack/blob/main/haystack/components/preprocessors/document_cleaner.py)、
[pdf-inspector furniture.rs v1.25.2](https://github.com/firecrawl/pdf-inspector/blob/v1.25.2/src/markdown/furniture.rs)。

**TOC 污染治理共识**：业界没有「数值信号降权」的框架级实现，共识是 **ingest 侧打结构标签 + 检索侧
metadata 过滤/惩罚**；TOC 的正确归宿是导航元数据而非语料（LlamaIndex 实践、marker DocumentTOCProcessor、
PageIndex 建议对结构性样板页打 content-type 惩罚）。病因理论：embedding 相似度度量「共享主题/措辞」
而非「答案相关性」，关键词密集的目录块天然像一切主题查询。来源：
[LlamaIndex TOC→节区元数据](https://blog.stackademic.com/automating-table-of-content-extraction-and-filtering-in-papers-with-llamaindex-7fc6a7cd3aae)、
[PageIndex](https://medium.com/@vaibhav-p-dixit/pageindex-in-rag-the-upgrade-your-retrieval-pipeline-desperately-needs-c6899b81cbb8)、
[语义相似度反向答案分析](https://hackernoon.com/why-semantic-similarity-can-give-ai-the-exact-opposite-answer)。

**vectorfs 现有依赖里已埋好的信号源**（零新依赖）：

- pdf-inspector 1.25.2：`markdown/furniture.rs` 家具剔除；`analysis.rs::is_toc_entry_line`（行尾 ≤4 位
  页码 + 点导符）与 `is_toc_marker_heading`（"Contents/目录" 标题行）——现状只在 markdown 模式激活；
  `structure_tree.rs::extract_structure_elements_mem` 读 /StructTreeRoot，`role ∈ {TOC, TOCI, Index,
  H1-H6, P, …}`，无 tagged tree 时优雅返回空。来源：[v1.25.2 源码](https://github.com/firecrawl/pdf-inspector/blob/v1.25.2/src/structure_tree.rs)。
- pdfium-render 0.9.5：`PdfBookmark`（title + dest 页码，outline 完整可读）；逐字符
  `scaled_font_size()/font_name()/font_weight()`；`PdfPageStructureTree` 是 stub 但 C API 全量
  FFI 可达。来源：[bookmark.rs](https://github.com/ajrcarey/pdfium-render/blob/master/src/pdf/document/bookmark.rs)、
  [char.rs](https://github.com/ajrcarey/pdfium-render/blob/master/src/pdf/document/page/text/char.rs)。
- tagged structure 覆盖率：完全 PDF/UA 合规仅 ~1–2%，但「至少一个 tag」约 2/3–4/5——**高置信信号
  但非默认路径**，缺失必须回退启发式。来源：[PDF Trends 2026](https://medium.com/@pdf4wcag/pdf-trends-2026-structure-tagging-and-conformance-of-pdfs-8039cf53b7bc)。

### 2.3 机制提案：证据在 extract，标签到 chunk，策略在检索

**分层原则与行业同构**（docling 的「检测在解析层、策略在导出层」）：vectorfs 的对应物是——证据采集
必须在 extract（页界/字号/书签只在那一层可见，join 之后不可恢复），分类结果以**中立标签**传出
（DIP 边界：引擎细节不出 kind 模块），剔除/降权策略在 chunk 与检索层。

1. **`Part` 增加可选结构标注**（additive；`Part::image` 先例）：extract 层写入 per-part 或 per-行段
   标签；Chunker 把标签继承到 chunk（跨结构边界的混合窗口取主导标签或标签集）。**不建议在 chunk 层
   重新猜测**——那是把证据丢掉再猜回来。
2. **新 metadata 键 `x-vf-chunk-kind`**：utf8 字符串枚举（`body|toc|header|footer|caption|…`，缺省
   `body`），与 `x-vf-modality` 同构；进 filterable 集合（短字符串，2KB filterable 预算无压力）。
   **用字符串不用 int**：镜像列同 key 类型必须永久稳定（store.rs 明文警告），int 编码在加标签值时
   有类型漂移/语义漂移双重风险。§5.10.1 键集显式增补 + domain.rs 两处声明同步 + 40KB 预检自动覆盖。
   Lance 侧走既有 AllNulls `add_columns` 演化——**旧索引零破坏**。
3. **策略分两档**（对齐全行业默认）：**furniture（页眉/页脚/页码/版权行）默认从提取产物剔除**——对
   检索零价值，docling/marker/MinerU/olmOCR 四家一致；**TOC 标签化不剔除**——`kind=toc` 保留可检索
   （「哪一章在哪页」类查询的合法答案），但默认检索面可过滤，检索策略二选一由裁决定：
   (a) `/x-vf/Search` 默认降权 kind=toc（RRF 前对向量名次乘惩罚，或路由内 rank 偏移）；
   (b) 默认过滤 + `includeKinds` 超集参数显式放行。倾向 (b)：过滤比降权可解释、可逆、零打分语义污染；
   降权留给 M3 reranker（其时惩罚信号天然就位）。
4. **三轨道检测，按置信度组合，双引擎产出同一标签词表**（可组合的前提，也符合 contract 的确定性断言）：
   - **轨道 1 tagged structure**（高置信，有则优先）：pdf-inspector `extract_structure_elements_mem`
     的 TOC/TOCI/Index role 直接映射 kind；
   - **轨道 2 outline**（pdfium）：bookmarks dest 页码集合定位 TOC 实体页；outline 另可生成
     section-path 元数据反哺正文 chunk（marker/LlamaIndex 已验证价值，**增强项后置**）；
   - **轨道 3 启发式**（兜底，双引擎通用）：pdf-inspector 路线**先做**——`markdown=true` 即激活
     furniture 剔除 + TOC 行信号（现状 `markdown=false` 是 F3 的直接成因之一，接近「翻默认值+接标签」
     的成本）；pdfium 路线补「逐页提取保留页界 → 跨页重复剔除（marker 参数）→ TOC 行占比判页
     （≥50% 行命中即整页 toc，中文需扩全角点导符）」。
5. **图例混排（F4）明确后置**：根因是 reading order，纯文本引擎无解；pdf-inspector markdown 模式的
   caption/表格分类是近期最优路径，随轨道 3 成熟度推进，不单独立项。

### 2.4 契约影响与待裁决点

- §5.10.2「一个提取产物」契约**不需要破**：结构标注是 Part 的 additive 字段，不是多 Part 化；
  分块无损拼接不变量也不受影响——契约管「chunk 串接 ≡ 提取产物」，而提取本身一直是有损投影
  （pdfium 文本本来就是页面对象的投影），furniture 剔除发生在投影内，层级清晰。
- **需要用户裁决**：(D1) furniture 默认剔除是否接受「提取产物不再含页眉页脚原文」（回源原文不受
  影响——`vf cat` 永远是原始字节，只有索引产物变化）；(D2) TOC 检索策略选默认过滤+显式放行（倾向）
  还是降权；(D3) `markdown=true` 是否升为 pdf-inspector kind 的默认 flavor（行为变化：输出从纯文本
  变 markdown，chunk 文本会带标题井号——对嵌入通常无害甚至有益，但属可见变化，需走指纹/reindex
  语义）。

---

## 3. F5/F6：超集面薄层（机制清晰、零协议影响、不依赖 M3）

### F5 按源文件（人类可读名）过滤

机制：`object_key_hash` 是确定性函数（`ingest/marker.rs`）——`/x-vf/Search` 收到
`{"x-vf-src-name": k}` 时服务端机械翻译为 `{"x-vf-src-uri": {"$eq": hash(k)}}` 再走既有 filter 编译。
协议面 QueryVectors 的 non-filterable 拒绝（400）原样不动——两面孔、一套存储。局限：翻译只覆盖
`$eq/$in`；`$startsWith`/glob 需要名字的 Lance 原生列（镜像列机制对 nonFilterable 键不建列——若未来
需要，走「派生表 server-internal 列」而非改 nonFilterableMetadataKeys 声明）。CLI 加 `--source <key>`
包裹。成本：服务端一处翻译 + CLI 一个 flag。

### F6 chunk 级读动词

机制：chunk key 形如 `{hash}-text-{i}`——**相邻块客户端可算**（`i±1`）；`x-vf-chunk-text` 已在
metadata 内，协议面 GetVectors（returnMetadata）即可取回。⇒ `vf chunk show <key> [--around N]
[--chars N]` 是纯 CLI 薄层，零服务端改动。顺带解决 F9 的一半（预览长度 `--chars`）。

---

## 4. 已归宿项与 polish（不展开）

- **F8 页级读取**：`location.page` 已标注 M3+（`ops_xvf.rs:704`），pdf-pages extractor（M3 T3）
  渲染页图像是其载体；`vf read --pages` 届时顺理成章。本次不新增机制。
- **F7 CLI 报错可教性**：语法本体是已裁决的 S3-CLI 风格（design.md §0 2026-11-05）；缺口仅是
  `vf get docs "key"` 双位置参数误用时 clap 错误未提示 `docs/key` 路径形态——错误模板加一行用法
  提示即可，随相邻提交清偿。
- **F9 分数语义文档**：RRF 融合分仅 rank 语义已在 `merge_hitlists` 注释言明；用户面（vf-search
  skill / README）补「分数读法」一节。

---

## 5. 汇总：机制落点与建议时机

| 摩擦 | 机制 | 落点 | 依赖 | 建议时机 |
|---|---|---|---|---|
| F1 | 词法路由 + RRF 融合（内核 = Lance 原生 FTS，查询面薄 trait 保换核退路） | T8 `$textMatch` | chunk 文本升顶层列；Search 路由循环接纳非 embedder 路由；tokenizer 进指纹 | M3 T8（本文 §1 为设计输入） |
| F2 | `x-vf-chunk-kind` 标签 + 检索策略 | Part 标注 → chunker 继承 → build_rows 注入 | §5.10.1 键集增补裁决 | 裁决后可做，与 M3 无执行序冲突（ingest 侧独立） |
| F3 | furniture 剔除（轨道 3 先行：markdown=true + 标签接线） | extract 层 | D1/D3 裁决 | 同上；成本接近「翻默认值」 |
| F4 | caption/表格分类 | pdf-inspector markdown 模式 | F3 成熟 | 后置，不单独立项 |
| F5 | name→hash 翻译 | `/x-vf/Search` 一处 | 无 | 独立小任务，随时 |
| F6 | `vf chunk show` | 纯 CLI | 无 | 独立小任务，随时 |
| F7/F9 | 错误模板/文档 | CLI/skill | 无 | 随相邻提交 |

**待裁决清单**（本文产出，供用户定夺）：
- **D1** furniture 默认剔除（索引产物不再含页眉页脚；`vf cat` 回源原文不变）
- **D2** TOC 检索策略：默认过滤 + `includeKinds` 放行（本文倾向）vs RRF 前降权
- **D3** pdf-inspector 默认 flavor 升 `markdown=true`（可见输出变化，走指纹/reindex 语义）
- **D4**（T8 设计时，2026-11-06 修订）BM25 内核**直用 Lance 原生 FTS**（锁定版本线已含，tantivy
  不进树、侧车不建）；前置为 chunk 文本升顶层列 + tokenizer 进指纹
- **D5**（T8 设计时，随 D4 改写）分词器在 lance 菜单内选：默认 `icu`（内置词典、中英混排稳）vs
  选项 `ngram 2,2`（真 bigram 得分最高但 Latin 同被 bigram 化）；原「自研 ≤150 行 bigram」因
  lance 无自定义 tokenizer 钩子而选项消失
