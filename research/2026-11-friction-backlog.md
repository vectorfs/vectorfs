# 使用摩擦 backlog（2026-11-06）

> 来源：一次真实 dogfooding 会话——用 `vf search`/`vf get` 回答「ThoughtWorks 技术雷达 vol 28–32 中
> Vibe Coding 的变化」（语料：5 期中文版雷达 PDF，`docs` 桶，INDEXED=6）。每条摩擦附会话级实证。
> 定位：**摩擦 → 机制归宿映射**。多数摩擦在 design.md 已有规划归宿（M3 T8 `$textMatch`+tantivy、
> `location.page`、`merge weighted`）；本 backlog 的作用是（a）为已规划项补充真实使用证据，
> （b）登记**无归宿的新缺口**，供机制研究（见 `2026-11-retrieval-friction-mechanisms.md`）与后续里程碑裁决。
> 纪律：不重开 M3 执行序裁决（2026-11-06「text 链不提前交错」仍然有效）。

## 摩擦总表

| # | 摩擦 | 会话实证 | 机制归宿 | 状态 |
|---|---|---|---|---|
| F1 | 精确词/专有名词检索缺失（"vibe coding" 在哪几期出现） | 嵌入检索 top-20 无一命中含该词的 chunk；被迫 `vf get` 18MB×5 + venv + pypdf + grep | **已交付（2026-11-07，M3-T9，ADR 0002 accepted）**：Lance 原生 FTS + searchMode + fts/fts-tokenizer 配置，1M 实测 43–54x，造词 e2e + 分词差分断言过门 | 证据补充：真实语料首日即命中此缺口 |
| F2 | 目录/TOC chunk 霸榜 | ~~查询「Vibe Coding 评估变化」top-5 全是目录块~~ **记录纠偏（2026-11-07 审查实证）**：枚举型查询（「语言和框架有哪些条目」）top-5 全目录块——F2 方向成立；原查询当日污染主体实为**图例块**（F4 实体），指认错位已修正 | **新缺口**：结构化 chunk 分类（chunk kind 标签）+ 检索侧策略 | 机制定稿（solution.md §1 D-1 页级判定+强制断窗，T12） |
| F3 | 页眉页脚/页码/版权行混进 chunk | "© Thoughtworks, Inc. 版权所有。 18" 等逐页重复行进入 chunk 正文与向量 | **新缺口**：与 F2 同根（跨页重复行检测；pdfium 提取器页循环内可见页结构，当前页文本 `\n\n` 直拼后丢弃页界） | 机制提案已出（mechanisms §2.3 轨道 3，D1/D3 待裁决；pdf-inspector 家具剔除现成未启用） |
| F4 | 图表页布局残留 | chunk 混入环形图图例数字「62 63 54 暂缓 评估 试验 采纳」 | **新缺口**：与 F2/F3 同根（结构识别的第三形态） | 随 F3 后置（reading order 根因，不单独立项） |
| F5 | 按源文件过滤只认 `x-vf-src-uri` 哈希 | 想只在 vol 32 内搜：需先 `--json` 挖哈希再拼 filter；默认表格输出不显示哈希 | **新缺口（薄）**：`/x-vf/Search` 超集面做 name→hash 翻译——`object_key_hash` 是确定性函数，服务端可机械翻译 `{"x-vf-src-name": k}` → `{"x-vf-src-uri": hash(k)}`；协议面（QueryVectors）不动，non-filterable 拒绝语义不变 | 机制明确，工作量小 |
| F6 | 命中文本预览 ~40 字符，无法判断相关性 | 每次命中都要追加命令确认；最终靠下载原文解决 | **新缺口（薄）**：chunk 级读动词——chunk key `{hash}-text-{i}` 的相邻块客户端可算（`i±1`）；`x-vf-chunk-text` 已在 metadata 内，协议面 GetVectors 就能取回 ⇒ `vf chunk show <key>` 是纯 CLI 薄层 | 机制明确，零服务端改动 |
| F7 | CLI `vf get` 误用形态报错无指引 | `vf get docs "radar/…"`（空格分隔双位置参数）报 "unexpected argument"，未提示 `docs/key` 路径形态；被迫读 `--help` | 语法本体**已裁决**（design.md §0 2026-11-05「vf 命令面借鉴 S3 CLI 惯用形态」：get/cat/rm 用路径位参，search/ls 用裸桶位参） | 缺口仅在错误消息可教性（clap 建议层） |
| F8 | 命中→原文缺中间粒度 | 想读「第 18–19 页」，`vf cat` 只能整文件（2.7MB PDF） | **已规划**：`location.page` 标注 M3+（ops_xvf.rs Search 文档注释）；pdf-pages extractor（M3 T3）渲染页图像 | 依赖 M3 T3 |
| F9 | 分数无标定 | 0.78 vs 0.83 不知好坏；RRF 融合分仅 rank 语义已在代码注释言明，但用户面无文档 | 文档缺口（vf-search skill / README 补「分数语义」节） | 文档 polish |

## 分组与优先级建议

- **机制研究组（F2/F3/F4，同一病根）**：提取/分块层的**文档结构识别**缺失——提取器只输出纯文本
  Part，无任何结构信号（页界、重复家具、导航结构）进入下游。深入分析见
  `research/2026-11-retrieval-friction-mechanisms.md`。这是本 backlog 唯一需要新机制设计的项。
- **超集薄层组（F5/F6）**：机制清晰、工作量小、不动协议面、不依赖 M3 执行序——可作独立小任务
  （服务端 F5 一处翻译 + 客户端 F6 一个动词）随时插入。
- **已规划佐证组（F1/F8）**：不新增机制；本次会话作为真实使用证据归档。F1 证据链（专有名词是
  语义检索盲区、BM25 侧车可完全消除本次绕路）供 M3 T8 设计时直接引用。
- **polish 组（F7/F9）**：错误消息可教性 + 文档，随任意相邻提交顺手清偿。

## 与既有裁决的边界

1. **不重开 M3 执行序**：F1 证据不支持提前 T8（本次绕路的根因之一是语料特征——专有名词精确匹配
   本就是 $textMatch 的分内事，T6/T7 无依赖关系变化）。
2. **不动协议面**：F5 的 name 过滤严格限定在 `/x-vf/` 超集面；QueryVectors 对 non-filterable key
   的 400 拒绝语义（AWS 兼容）原样保留。
3. **chunk 元数据键集是加法**：F2 若落地为 `x-vf-chunk-kind` 标签，属 §5.10.1 键集的显式增补，
   需同步 domain.rs 两处 `non_filterable_metadata_keys` 声明与 40KB 预检——新键计入预算。
4. **提取契约（lossless 拼接不变量）不破**：F3 若做家具剥离，剥离动作与「chunk 串接 ≡ 提取产物」
   契约的层级关系（提取有损投影 vs 分块无损拼接）需在机制研究里先行裁决。
