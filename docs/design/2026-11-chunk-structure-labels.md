# 设计：chunk 结构标签与提取期家具剔除

> 状态：proposed（2026-11-06）。提案：`docs/proposals/2026-11-chunk-structure-labels.md`；
> ADR：`docs/adr/0003-chunk-structure-labels.md`。ingest 侧独立，与 M3 图像链无执行序冲突。
> 分层原则（与 docling「检测在解析层、策略在导出层」同构）：**证据在 extract，标签到 chunk，
> 策略在检索**。

## 1. 数据模型

### 1.1 `ChunkKind`（closed enum，加值 additive）

```rust
pub enum ChunkKind { Body, Toc, Caption }   // Display: "body" | "toc" | "caption"
```

词表最小化起步（F2/F3 治理所需）；heading/list-item 等候选留给 section-path 增强项。

### 1.2 `Part` additive 标注（`Part::image` 先例模式，现有构造零改动）

```rust
pub struct LabelSpan { pub range: std::ops::Range<usize>, pub kind: ChunkKind } // 字节区间，锚定 Part::text
pub struct Part { /* 现有字段不动 */ pub labels: Vec<LabelSpan> }   // 缺省空 = 全 body
```

- **furniture 不占标签**：剔除发生在提取产物内（文本直接消失），无标签即无块——标签只标
  **保留**的非正文文本（TOC/caption）；
- 区间锚定 `Part::text` 字节偏移（提取器自产自销，无跨层偏移漂移）。

### 1.3 `Chunk.kind` 与 metadata

- `Chunk` 增 `kind: ChunkKind`（缺省 `Body`）；chunker 对每个窗口统计标签区间覆盖率，
  **非 body 覆盖 ≥ 50% 才改标**（机制常数，防边界窗口抖动），否则 `Body`；
- `build_rows` 注入 **`x-vf-chunk-kind`**（utf8 字符串，**filterable**——不入
  `non_filterable_metadata_keys`，与 `x-vf-modality` 同构；40KB 预检自动覆盖，增量一个短串）；
- 字符串枚举而非 int：镜像列同 key 标量类型必须永久稳定（`store.rs:33-53` 明文警告），int
  编码在加值时有类型/语义双漂移风险；
- Lance 演化：新 filterable 键走动态镜像列 AllNulls `add_columns`（`store.rs:1417` 机制），
  旧索引零破坏。

## 2. 检测轨道（双引擎同一标签词表）

### 2.1 pdf-inspector 路线（先行，成本≈翻默认值+接线）

- **`markdown = true` 升默认 flavor**（D3）：激活 crate 内置 `furniture.rs` 跨页家具剔除
  （≥3 页重复证明 + 短文档位置证据双策略）与标题层级；纯文本模式这些机制全部旁路——
  这是 F3 的直接成因（`extract/pdf_inspector.rs:88-112` 默认构造核验）；
- **TOC 行标注**：`analysis.rs` 的 `is_toc_entry_line` 信号（行尾 ≤4 位页码 + 点导符，扩全角
  点/省略号导符）逐行判定，连续 TOC 行段输出为 `LabelSpan{kind: Toc}`；
- **tagged structure 轨道**（高置信，有则优先）：`extract_structure_elements_mem` 的
  `TOC/TOCI/Index` role 直接映射 `Toc`；无 tagged tree 返回空，优雅降级到行级信号。
  覆盖率限制（完全 PDF/UA ~1–2%，有任何 tag ~50–80%）已知，非默认路径。

### 2.2 pdfium 路线（`pdf_text.rs` 内部改造，输出契约不变）

- **逐页提取**（页界只在提取器内部存在）：页循环内做**跨页首/尾行重复检测**——归一化
  （去行首尾数字/空白）后，行在 ≥20% 页面重复或连击 ≥3 即判 furniture 剔除
  （marker `IgnoreTextProcessor` 参数档）；短文档（<3 页）证据不足**宁可不删**；
- **TOC 整页判定**：页内 TOC 行占比 ≥50% 判 `Toc` 页，输出该页文本的 `LabelSpan`；
- **outline 辅助**（有书签时）：`PdfDocument::bookmarks()` dest 页码集合与 TOC 页判定
  互相印证（dest 密集页加分）；outline → section-path 反哺为后续增强，本期不做；
- 输出仍是**单 Part**（§5.10.2 契约不破）：页界用于检测，不进产物结构。

### 2.3 图例混排（F4）

不单独立项：pdf-inspector markdown 模式的 caption/表格分类随 §2.1 自然获得（caption 映射
`ChunkKind::Caption`）；pdfium 纯文本引擎无 reading order 能力，明确不治。

## 3. 查询面策略

`/x-vf/Search` 与词法路由（若 T8 已落）**默认排除 `kind = toc`**：

- 超集参数 **`includeKinds: ["toc", ...]`** 显式放行；CLI `--kind toc`（可重复）；
- 默认排除作用于语义与词法两路（TOC 行是逐页重复的完美 BM25 匹配，词法路由不排除会复刻
  F2 霸榜）；
- `QueryVectors`（协议面）**零变化**：metadata 过滤是用户显式行为，协议面不做隐式排除；
- 响应 `metadata` 原样含 `x-vf-chunk-kind`，命中归因可见。

## 4. 指纹与迁移

- 提取器行为变化（markdown 默认值、家具剔除、TOC 标注）计入**管线指纹**（extract 链
  配置指纹含 kind 配置与序的既有语义自然覆盖 flavor 变更）；指纹变更 ⇒ `vf reindex`
  显式重建；
- 存量索引升级后：`x-vf-chunk-kind` 列全 null ⇒ 查询面过滤**把 null 视为 body 不排除**
  （旧数据兼容），reindex 后获得真实标签。

## 5. 测试门

1. **golden 家具语料**：带页眉页脚的合成 PDF——提取产物不含家具行（断言文本），chunk 正文
   无 "© … 版权所有" 类重复行；
2. **TOC 语料**（雷达 PDF 即真实 fixture）：目录页 chunk `kind=toc`；正文 chunk `body`；
   默认检索不返回 toc 块，`includeKinds:["toc"]` 返回且可答「哪章在哪页」；
3. **误判守门**：正文首行与页眉同文（如章节标题横排）不被误删——跨页重复 + 位置双门槛
   断言；短文档（2 页）零剔除；
4. **契约不回归**：chunk 串接 ≡ 提取产物（lossless 拼接断言对含标签 Part 同样成立）；
   extract contract 四 kind 断言全过；
5. **双引擎词表一致**：同一 PDF 经 pdf-inspector 与 pdfium 两链，标签词表一致（route
   回退可组合前提）；
6. **协议面零变化**：QueryVectors conformance 不回归；`x-vf-chunk-kind` 过滤在协议面
   可用（filterable 键，与 modality 同待遇）。
