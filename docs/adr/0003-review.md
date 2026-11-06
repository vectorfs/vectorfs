---
status: final（独立审查完成 2026-11-07，待用户终裁）
target: docs/adr/0003-chunk-structure-labels.md（proposed）
---

# ADR 0003 独立审查：chunk 结构标签（提取期家具剔除 + `x-vf-chunk-kind`）

## 0. 审查范围与方法

- **对象**：ADR 0003 全文；其依据 `research/2026-11-retrieval-friction-mechanisms.md` §2、
  `research/2026-11-friction-backlog.md` F2/F3/F4；`design.md` §0（pdf-inspector 增补行、智能路由行、
  插件轴硬化行、M3 执行序行）与 §5.10.2/§5.10.8；姊妹文档 `docs/proposals/2026-11-chunk-structure-labels.md`
  与 `docs/design/2026-11-chunk-structure-labels.md`。
- **代码核验**（一手，非转述）：`crates/server/src/pipeline/extract/pdf_text.rs`、
  `extract/pdf_inspector.rs`、`pipeline/mod.rs`（Part/Chunk/Modality/ContentClass/Extractor trait）、
  `pipeline/config.rs`（pipeline_fingerprint 构成）、`pipeline/chunk/`（拼接不变量）、
  `ingest/mod.rs`（build_rows + 40KB 预检）、`ingest/domain.rs`（non_filterable 声明）、
  `filter.rs`（`$ne` 对缺失键的语义、pushdown NULL 三值逻辑）、`store.rs`（AllNulls 镜像列演化、
  FTS prefilter）、`ops.rs`/`ops_xvf.rs`（QueryVectors/Search 的 filter 面与 searchMode 先例）、
  `preset.rs`（pdf-inspector 是否进默认档）。
- **依赖源码核验**：pdf-inspector **1.25.2** registry 源码（`MarkdownOptions` 全字段与默认值、
  `markdown/furniture.rs` 实际阈值、`markdown/analysis.rs` TOC 函数可见性与匹配集）。
- 未运行雷达语料实测（阈值经验行为不在本次审查范围，见 S4）。

## 1. Verdict

**needs revision**——架构方向与分层原则核验**成立**（reject 级问题为零；filterable 键、AllNulls
演化、null=body 语义、auto 不漂移、DIP 边界逐条对上代码），但存在 **2 个 blocker**，属 ADR 文本与
迁移设计缺陷，修订 ADR 即可解除，无需推翻方案骨架：

- **B1（迁移/指纹缺口，阻塞 D1）**：pdfium 路线（`pdf-text`，**auto 与全部 preset 的默认引擎**）的
  家具剔除与 TOC 标注是纯机制改动——`PdfTextConfig` 零旋钮，`pipeline_fingerprint`
  （`pipeline/config.rs:1092`）只序列化 {extractors 配置, extract_routes, chunk, embedders,
  charset_fallback, fts_tokenizer}，**不含任何机制版本盐**。 ⇒ 升级二进制后指纹逐字节不变：存量
  marker 仍 INDEXED 且指纹匹配，**没有任何东西触发 reindex**；新摄取的文件产出无家具/带标签 chunk，
  旧文件保留家具/无标签 chunk——同一 index 内语料语义静默混装，直接违反 §5.10.0 公理 3
  「正确性靠指纹，不靠纪律」。ADR Consequences 的迁移故事（「存量索引列 null 视为 body 不排除，
  **reindex 后**获得真实标签」）对默认路径**无从兑现**——reindex 只由指纹变更触发，而这条路线上
  指纹不变。ADR 只为 pdf-inspector 侧写明了指纹通道（markdown 旋钮翻转，见核验表 F6），默认路径
  悬空。**修法三选一**（ADR 需明确写出）：(i) 给提取行为引入一次性机制版本盐进指纹（最贴
  charset_fallback「改 chunk 字节 ⇒ 入指纹」的 T7 先例精神）；(ii) 家具剔除做成 ingest 级显式旋钮
  （默认 strip，入指纹，三态省略规则同 fts_tokenizer——pre-ADR 部署指纹逐字节不变）；(iii) 降级为
  文档声明「升级须手动 reindex」+ doctor 提示（最弱，与指纹纪律张力最大）。倾向 (ii)：语义最诚实，
  且给 D1 留了 per-bucket 关闭口（见 D1 选项）。
- **B2（守门常数事实错误，阻塞 D1/D3 裁决面）**：ADR Consequences 把误判守门写成单一「机制常数」：
  「家具剔除需跨页重复证明（**≥20% 页或连击 ≥3**）+ **短文档（<3 页）宁可不删**」。这是 pdfium
  路线计划的 marker 参数档；而 pdf-inspector 轨道（D3 翻默认即激活）的真实行为是
  `furniture.rs:464` `threshold = max(3, 30%页)`（**30% 且无连击规则**），且短文档**并非不删**——
  `<3` 页时走位置证据支线（行距隔离 ≥1.8× + 首尾 ≤2 行 + ≤90 字符即删，`furniture.rs:40-43`）。
  即：用户按 ADR 文本裁决 D1 时以为「短文档永不被删」，实际 D3 默认翻转后 pdf-inspector 轨道会删。
  research §2.2 的对照表本身是对的，ADR 收敛时把两条轨道压成了一套常数。**修法**：ADR 分轨道如实
  写两套常数；或者（更强）承诺统一——把 pdf-inspector 轨道的 `strip_headers_footers` 行为约束进
  我们自己的判定层（crate 阈值不可配，统一只能靠我们在外层重判，成本反噬「翻默认值近乎免费」的
  立论，不建议）——至少要让终裁文本与真实守门一致。

无 blocker 级架构问题；其余发现如下（should-fix 项建议随 ADR 修订一并落，note 项随实施处理）。

## 2. 分维发现表

### 维度 1：设计正确性（分层不变式 / Part 形态 / markdown 影响面）

| # | 发现 | severity | 建议 |
|---|---|---|---|
| 1.1 | **层级关系成立**。不变式链：chunk 串接 ≡ **提取产物**（chunker 契约，`chunk/token_window.rs:17` 注释与 windowing 不变量测试钉住）；提取产物 ≅ 源文档本来就是有损投影（pdfium 文本即页对象投影）。家具剔除发生在投影内 ⇒ 不变式按定义保住；标签是 `Part` 上的 additive `Vec<LabelSpan>`（字节区间，不动文本），同样不触拼接不变量。ADR 一句话（「剔除发生在提取投影内」）+ research §2.4 展开论证，说得清、成立。 | — | ADR 可补一行显式两段式表述（不变式绑定的是 chunker↔extractor 出口，不是 extractor↔源字节），防后人误读「无损」二字。 |
| 1.2 | **与 T1 Part 形态无冲突**：`Part{modality, text, image: Option<ImagePayload>}`（`pipeline/mod.rs:228`）加 `labels` 与 `image` 先例同构；`Chunk` 加 `kind` 同理。代价是全仓 Part/Chunk 字面量机械补字段（T1 已演练过一次）。 | note | 实施时给 `Part::labels` 配 `Default::default()` 或构造函数，缩小学动面。 |
| 1.3 | **markdown=true 影响面被 ADR 低估**。`process_pdf_mem` = `PdfOptions::default()` ⇒ MarkdownOptions 全默认开启：`detect_headers/lists/code`、`remove_page_numbers=true`、`format_urls`（URL 重写为 `[t](url)`）、`fix_hyphenation=true`（**拼接跨行断词，改写文本内容**）、`detect_bold/italic/underline`（`**`/`<u>` 标记）、`strip_headers_footers=true`。D3 不是「标题井号」一个变化，是一整组文本变换电池；「对嵌入通常无害甚至有益」对中文语料 + gte 未验证（`<u>`/`**` 噪声、断词拼接副作用）。有利面：`include_images=false`、`include_page_numbers=false` 默认关，无占位符噪声。 | should-fix | D3 裁决文本如实列出变换电池；实施时用真语料做一次廉价 A/B（同 PDF 两 flavor 的检索对比），异常再考虑经 `process_pdf_mem_with_options` 收窄 MarkdownOptions（crate 通道现成，见 2.4/S2）。 |
| 1.4 | **auto 不漂移裁决未被破**（核验）：auto pdf→pdf-text（§0 2026-11-05 行）；`preset.rs` 全文无 pdf-inspector（仅 README 提及）⇒ D3 翻默认的影响面 = 显式配置 pdf-inspector 的实例，默认档零感知。 | — | — |
| 1.5 | **F3 在查询层无解的论据 ADR 没有写透**（这是 D1 最硬的正当性）：token-window 跨页切窗时家具行与正文**同窗交错**，chunk 级标签无法把「chunk 里的家具部分」从嵌入里摘出去——标签只能整 chunk 处置，家具噪声仍进向量并扭曲邻近正文窗的表示。这是「F3 必须在提取期治」的机制理由，比「四家行业默认」更强。 | note | ADR Considered Options 或 D1 论证补此条。 |

### 维度 2：§5.10.8 红线（闭枚举归属 / includeKinds / 与智能路由的关系）

| # | 发现 | severity | 建议 |
|---|---|---|---|
| 2.1 | **`ChunkKind` 闭枚举 = 机制面，合规**。与 `Modality`/`ContentClass` 同居 pipeline/mod.rs、加值 additive、用户不可配——完全复刻既有两枚举的先例（`ContentClass` 注释明言「closed enum, zero config surface (§5.10.8 stands)」）。metadata 值用字符串不用 int 的论证（镜像列同 key 标量类型永久稳定，`store.rs:52` 明文）核验成立。 | — | — |
| 2.2 | **includeKinds 不越红线**。它是 `/x-vf/Search` 的查询期超集参数（wire 字段 + CLI flag），与 `searchMode`（M3-T9 已交付先例）、`filter`、`topK` 同类；红线禁的是**摄取编排**的分支/DAG/路由表，查询期过滤定值不属此列。语义上它是 `filter` 的语法糖（可机械编译为 `x-vf-chunk-kind $in`/`$nin`），无新机制。 | — | 实施建议直接编译进既有 filter 再走 QueryVectors 同一编译管线，零新执行路径。 |
| 2.3 | **默认排除 toc 是「策略进代码」，与 §5.10.0 公理 1 有张力但有先例**：`searchMode` 默认 semantic 同为查询期固定默认 + 逐查询显式改写。区别是 searchMode 不改变「返回什么」只改「怎么查」，默认排除改变结果集。若未来出现「目录即语料」的桶（导航索引场景），无 per-bucket 默认开关——但按证据门纪律（无用户证据不加旋钮），先固定默认 + includeKinds 逃生口是对的。 | note | ADR 显式记一句「沿用 searchMode 先例：查询期默认 + 显式放行，per-bucket 旋钮证据门后置」。 |
| 2.4 | **与智能路由无概念纠缠**。ContentClass 分类**源字节**（提取前、路由输入）；ChunkKind 分类**提取产物内容**（提取后、检索过滤输入）。时间轴、消费者、词表全部不相交；probe→affinity→route 不看 ChunkKind，ChunkKind 不影响路由序。唯一交点是「回退链可换成员 ⇒ 标签须同一词表」——这正是 ADR 词表约束的动因，属维度 4。 | — | 文档注意 `includeKinds` 与插件轴 `kind = "pdf-text"` 的第三重「kind」撞名（见 4.4/N1）。 |

### 维度 3：协议/存储面（metadataConfiguration / AWS 面 / 40KB）

| # | 发现 | severity | 建议 |
|---|---|---|---|
| 3.1 | **filterable 新键零 metadataConfiguration 牵动，兼容成立**（核验）。ingest 派生 index 的 `nonFilterableMetadataKeys=[x-vf-src-name, x-vf-chunk-text]`（`domain.rs:173`）；`x-vf-chunk-kind` 走 filterable ⇒ **不需要**动这份声明（`x-vf-modality`/`x-vf-model` 同为 filterable 且不在表中）。注意：research 两文反复出现的「domain.rs 两处 non_filterable_metadata_keys 声明同步」对 filterable 键是**过时/误导**表述——ADR 正文没抄这句话（只说「40KB 预检自动覆盖」，正确），但实施者若照 research 做会白找两处不存在的同步点。 | note | 实施以 ADR 为准；research 文档不回改（快照纪律），本审查留档纠偏。 |
| 3.2 | **旧索引行为核验成立**。新 filterable 标量键 → 首次新写入触发 `add_columns(AllNulls)` 演化（`store.rs:49-55` + 写路径 1219-1223 三步序），旧行 null、零破坏。查询面「null 视为 body 不排除」在**两条执行路径**上都核验为真：内存路径 `eval_cond` 的 `$ne` 对缺失键 = `!false` = true（`filter.rs:438`）；pushdown 路径显式生成 `col IS NULL OR col != v` 避 SQL 三值逻辑坑（`filter.rs:551-553`）；无镜像列的键 pushdown 跳过、精确后滤兜底（`filter.rs:519-528`）。 | — | — |
| 3.3 | **AWS 面语义无回归**：QueryVectors 对未知 **filterable** 键不拒（只拒 nonFilterable 声明键，`ops.rs:1300-1309`）——`$eq "toc"` 旧索引得空集、`$ne` 得全量，与 AWS「过滤是用户显式行为」一致；GetVectors returnMetadata 原样回显新键。协议用户也可 PutVectors 自写 `x-vf-chunk-kind` 任意值——闭枚举只是 ingest 侧词表，协议面值域自由（与 `x-vf-*` 其他键同立场）；未知值在默认排除下被视为非 toc 保留，无害。 | — | README 键集表（§5.10.5 预算表）增补该键时顺带写明词表与值域自由。 |
| 3.4 | **40KB 预检影响可忽略**：增量 `"x-vf-chunk-kind":"body"` ≈ 26 字节/行，预检对象是整 metadata 对象（`build_rows` `ingest/mod.rs:2344-2352`）自动覆盖；filterable ≤2KB/键的值域 ~7 字节，无压力。 | — | — |
| 3.5 | **默认排除与用户显式 filter 的合成规则未定义**（should-fix）。若实现为「注入 `$nin` 与用户 filter AND 合并」，则 `filter: {"x-vf-chunk-kind": "toc"}` + 默认排除 = 恒假 = 永远空结果——用户按文档过滤 toc 却拿不到，除非同时知道要传 includeKinds。设计文档（design 稿 §3）未写交互。 | should-fix | 定一条并在 ADR/附录E 写明，推荐：**用户 filter 显式引用 `x-vf-chunk-kind` 时默认排除让位**（用户接管语义），includeKinds 仅作为无引用时的放行口；实现即一行前置判断。 |

### 维度 4：双引擎一致性（词表 vs 判定 / 路由可换成员 × 标签持久化）

| # | 发现 | severity | 建议 |
|---|---|---|---|
| 4.1 | **两轨道阈值/策略实质分歧**（B2 的根，此处看后果）：pdfium 计划 ≥20% 或连击 ≥3、<3 页不删；pdf-inspector 实际 max(3, 30%)、短文档位置证据可删。同一 PDF 换引擎 ⇒ 家具删留不同、TOC 判页不同 ⇒ chunk 文本与标签都不同。路由链按 ContentClass **逐文档换成员**（text-native→insp，mixed→pdfium 退让），而 chunk kind 持久化进 metadata ⇒ **同桶语料带两套判定语义**，用户对 filter 结果的预期无法一致。指纹含链组成与序（成员序变即变）但不含成员内部阈值 ⇒ 升级 pdf-inspector 小版本同样可漂移标签（engines 表只 Warn，重嵌是操作员决定——与 §0 引擎可观测裁决一致，可接受，但 ADR 应言明标签继承此立场）。 | should-fix（B2 修订后残余） | 接受分歧但**显式记录**：「词表一致 ≠ 判定一致」写进 ADR；docs 给阈值对照表；把「跨引擎判定收敛」留给后续证据（如同语料双链标注差异测量）。 |
| 4.2 | **设计稿测试门 #5 按字面不可过**：「同一 PDF 经 pdf-inspector 与 pdfium 两链，标签词表一致」——若理解为「同 PDF 同标签」，按 4.1 构造性不成立（两引擎文本都不相同，标签必然有差）。 | should-fix | 改写为可断言形式：词表枚举一致（两链产出的 kind 值 ∈ 同一闭集）+ 各自黄金语料上的召回断言；删去「同 PDF 同标签」的暗示。 |
| 4.3 | **TOC 标注通道两边都要自建，成本立论偏乐观**（核验 crate）：`is_toc_entry_line`/`is_toc_marker_heading` 是 `pub(crate)`（`markdown/analysis.rs:181,203`），vectorfs **不可调用**；且点导符只认 ASCII `.` ≥3、标题只认英文 "contents"/"table of contents"——**中文雷达 PDF（本案证据语料）的 `……`/`………` 全角导符与「目录」标题均不命中**。 ⇒ pdf-inspector 轨道的 TOC LabelSpan 也是我们自己的行级扫描（design 稿已提「扩全角点导符」，等于承认自建）；crate 免费的只有家具剔除。 | should-fix | ADR/设计稿把「翻默认值+接标签」的成本表述改为「家具近乎免费、TOC 两引擎均自建词法（含全角导符与『目录』标题匹配）」；两引擎共用一个行级 TOC 判定模块（放 pipeline 共享层，DIP 边界内）以缩小 4.1 分歧面。 |

### 维度 5：实施代价与收益（证据强度 / 改动面 / 更小第一步）

| # | 发现 | severity | 建议 |
|---|---|---|---|
| 5.1 | **收益证据真实但单会话单语料**：F2（top-5 全目录、答案块不在 top-20）与 F3（版权行进 chunk）均一次 dogfooding、5 份中文雷达 PDF。机制方向有行业对照背书（docling/marker/MinerU/unstructured 四家），但**阈值（判页 ≥50%、覆盖率 ≥50%）零标定**——误标 toc 的代价是默认检索**静默召回损失**（body 被排除），不可观测除非 includeKinds 探测。 | should-fix | 终裁通过后加一道**测量门**：雷达语料全量标注 diff（多少 chunk 得非 body 标签、人工抽查误判率）作为默认排除开启的前置；把「机制常数」的第一次校准钉在真实证据语料上。 |
| 5.2 | **改动面清点**（两 extractor + Part/Chunk + chunker 覆盖统计 + build_rows + Search/CLI + 测试）：pdfium 侧跨页重复检测与 TOC 判页是新启发式代码（本 ADR 最大的单块新逻辑）；pdf-inspector 侧翻默认 + 自建 TOC 扫描；chunker 两 kind 各接标签覆盖率；Search 面 includeKinds + 默认排除（含 BM25 臂——`keyword_search` 已收 filter 做 prefilter，`store.rs:1506-1509`，通道现成）。 | — | — |
| 5.3 | **存在更小的第一步，且切分面干净**：F2（证据最硬、痛感最尖）只需 **toc 标签 + 默认排除**；家具剔除（F3）是独立可后的半边——尤其 pdfium 侧跨页去重是全部新逻辑里最细的一块，而 pdf-inspector 侧家具近乎免费但默认配置根本不用它。切法：**Phase 1 = 双引擎 TOC 标注 + `x-vf-chunk-kind` + Search 排除**（拿到 F2 收益，B1 迁移问题同款存在需先解）；**Phase 2 = 家具剔除**（pdfium 机制 + D3 翻默认）。Phase 1 不含 D1/D3，终裁可拆。 | note（属裁决选项，非缺陷） | 作为 D1 问题内的分阶段选项呈报（见 §3 D1-c）。 |

### 维度 6：D1/D2/D3 子裁决选项梳理

并入 §3 三问清单。

## 3. 给用户的三问裁决清单

> 每问选项按推荐序排列；推荐均为审查意见，终裁权在用户。B1/B2 修订进 ADR 前任何「通过」都不生效。

### D1：furniture（页眉/页脚/页码/版权行）默认从提取产物剔除？

前提事实：`vf cat` 永远回源原始字节，变化只及索引产物；提取产物本就是有损投影（1.1）；
F3 在查询层无解（1.5——家具与正文同窗交错，标签摘不出向量里的噪声）；行业四家默认剔除。

- **D1-a（推荐）：默认剔除，双引擎各自机制实现，守门常数分轨道如实写（B2 修订后）**
  利：对齐行业默认；F3 真治（向量干净）；零配置面增长。弊：有损投影多损一层（不可逆，除非
  reindex 回旧版）；两轨道判定分歧（4.1）造成桶内语义不完全一致；误删风险由守门常数承担
  （pdfium 侧 <3 页不删、≥20%/连击 ≥3；pdf-inspector 侧 max(3,30%) + 短文档位置证据——需接受）。
  **配套必选一个 B1 修法**（推荐机制旋钮 `furniture = "strip"`，默认值入指纹三态省略）。
- **D1-b：标注不剔除**（词表扩 `header|footer`，Search 默认一并排除，includeKinds 放行）
  利：提取产物保真、查询层可逆；与 D2 同构，机制单一。弊：**治不了 F3 的本体**——家具仍进嵌入
  并污染同窗正文向量（1.5），只 hides 症状；词表变宽；行业无此默认（独走）。
- **D1-c：分阶段——本期只做 TOC 标签（D2 相关半边），家具剔除后置一期**
  利：先拿 F2（最硬证据）；pdfium 跨页去重（最细的新逻辑）延后；D1 裁决也随之后置，可带测量数据
  再裁。弊：F3 症状留场；两期两次指纹变更/两次 reindex（若 B1 走旋钮法可合并为一次）。

### D2：toc 的检索策略？

前提事实：F2 = 目录块与任意主题查询相似度虚高，会话实证 top-5 霸榜；「哪章在哪页」是合法查询
（故不采默认剔除）；T8/T9 后 BM25 臂同受 TOC 污染（逐页重复行是完美 BM25 匹配）。

- **D2-a（推荐）：`/x-vf/Search` 默认排除 toc + 超集参数 `includeKinds` 显式放行（编译进既有
  filter 管线，2.2）**
  利：可解释、可逆、零打分语义污染（RRF rank 纯净性保住）；与 filter 语法糖同构无新机制；
  旧索引 null=body 核验无恙（3.2）。弊：默认结果集变化（老客户端感知「结果变了」——但这正是
  F2 的修复本身）；需定 3.5 的合成规则；误标 toc 即默认静默丢失（靠 S4 测量门对冲）。
- **D2-b：只打标签，不设默认排除**（用户自行 `filter: {"x-vf-chunk-kind": {"$ne":"toc"}}`）
  利：Search 面零改动、零默认行为变化；最小机制。弊：F2 对所有不写 filter 的用户原样留存——
  本 ADR 的主要收益不兑现；标签成为「建得起」而非「用得上」的设施。
- **D2-c：RRF 前降权**（研究文已出局，列此存档）：无框架先例、污染 rank 语义、与 reranker
  （M3 已选型 gte-reranker）职责重叠——不采。

### D3：pdf-inspector 默认 flavor 升 `markdown=true`？

前提事实：影响面仅显式配置该 kind 的实例（preset/auto 均不含，1.4）；markdown 序列化进指纹
（`config_roundtrip` 测试钉住默认值可序列化）⇒ 翻默认 = 省缺配置的桶指纹变更 = fail-fast 引导
reindex（迁移语义现成）；显式 `markdown=false` 的桶行为与指纹双不变；markdown=true 是一组变换电池
（1.3），且**默认自带 `strip_headers_footers=true` 与 `remove_page_numbers=true`**——D3 内嵌了
pdf-inspector 轨道的 D1。**crate 有独立旋钮** `MarkdownOptions.strip_headers_footers`
（`process_pdf_mem_with_options` 公开，核验）⇒ D1/D3 技术上可解耦，只是 ADR 未提。

- **D3-a（推荐，若 D1 采 a）：默认 markdown=true，全默认接受**（家具剔除随 crate 默认开启）
  利：一行翻默认拿到结构化输出 + 家具剔除 + 标题层级；迁移走指纹现成语义；配置面零增长。
  弊：接受 1.3 全部变换（断词拼接/URL 重写/强调标记）与 crate 阈值（30%/短文档位置证据）——
  即接受 B2 修订后的轨道事实。
- **D3-b（若 D1 被拒或后置）：默认 markdown=true，但经 `_with_options` 固定
  `strip_headers_footers=false`**（家具留给 D1 裁决/后置）
  利：结构收益与家具政策解耦，D1 可独立终裁。弊：引入我们侧一个隐藏固定值（应随 D1 旋钮一起
  物化进配置与指纹，否则又是一个指纹不可见的行为开关——B1 同款风险）；「翻默认值近乎免费」
  的表述进一步失真。
- **D3-c：维持 markdown=false 默认**（结构化输出留显式 opt-in）
  利：零可见变化、零裁决耦合。弊：pdf-inspector 轨道失去本 ADR 的一切结构收益；F4（caption 分类）
  唯一载道延后；「插件试金石」kind 的差异化特性长期藏灰。

## 4. 附：核验事实锚点（ADR 断言 ↔ 代码/源码）

| ADR 断言 | 核验 | 锚点 |
|---|---|---|
| 家具剔除「全行业默认」 | 成立 | research §2.2 四家对照（docling/marker/MinerU/unstructured），来源可溯 |
| filterable 键 + AllNulls 演化旧索引零破坏 | 成立 | `store.rs:49-55`、写路径演化三步序 |
| 存量 null 视为 body 不排除 | 成立（双路径） | `filter.rs:438`（内存 `$ne` 缺失=true）、`filter.rs:551`（pushdown `IS NULL OR !=`） |
| QueryVectors 零变化 / 未知 filterable 键不拒 | 成立 | `ops.rs:1300`（仅拒 nonFilterable 声明键） |
| 40KB 预检自动覆盖 | 成立 | `ingest/mod.rs:2344` 预检整 metadata 对象 |
| pdf-inspector 家具/TOC 信号「现状纯文本模式旁路」 | 成立 | `extract/pdf_inspector.rs:169-181`（markdown 分支才走 `process_pdf_mem`） |
| 「翻默认值+接标签」成本量级 | **偏乐观** | TOC 函数 `pub(crate)` 且 ASCII/英文 only（4.3）；家具阈值 max(3,30%) ≠ ADR 常数（B2） |
| markdown 翻默认走指纹/reindex | 成立 | `PdfInspectorConfig` 默认值可序列化断言（`config_roundtrip`）；Fp 含 extractors（F6） |
| auto 不漂移 | 成立 | §0 2026-11-05 行 + `preset.rs` 无 pdf-inspector |
| 拼接不变式不破 | 成立 | 不变式绑定 chunker↔提取产物（1.1）；标签不动文本 |
| pdfium 路线行为变化有迁移通道 | **不成立（B1）** | `PdfTextConfig` 零旋钮；`pipeline_fingerprint` 无机制盐 |
