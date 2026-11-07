# 设计：内容寻址与有界上下文读取

> 状态：proposed（2026-11-06）。提案：`docs/proposals/2026-11-content-addressing.md`；
> ADR：`docs/adr/0005-content-addressing-bounded-reads.md`。
> 核验背景：chunk key 形状与解析规则已成文于 `ingest/marker.rs:22-45`（`{16hex}-{modality}-{i}`，
> 三段/16 hex/字母数字模态/全数字序号）；GetVectors keys 限额 1–100（`ops.rs:1420`）；
> `returnMetadata` 返回完整 metadata blob（`ops.rs:1400-1402`，含 `x-vf-chunk-text`）；
> 客户端无既有 GetVectors 消费（`chunk show` 为首个，走 `call_json`）。

## 1. 坐标系统（coordinate vocabulary）

按格式最小化的**数值坐标键集**（全部 filterable，§5.10.1 键集增补，与 `x-vf-modality` 同构待遇）：

| 键 | 类型 | 单位存在的格式 | 来源 |
|---|---|---|---|
| `x-vf-page-start` / `x-vf-page-end` | number | pdf（文本块与页图像块统一） | extract：页偏移表 |
| `x-vf-line-start` / `x-vf-line-end` | number | txt / md / 容器子文档 | chunker：行计数 |
| `x-vf-section` | number | 容器子文档（epub spine 序、zip 子项序） | 容器编排：child ordinal |

- **不做笛卡尔积**：pdf 不打行、txt 不打页；键只在单位存在时写入，`$exists` 语义自然区分；
- **1-based 物理页索引**（非印刷 label——雷达类前置目录致 label 偏移；`FPDF_GetPageLabel`
  后置增强）；`line` 以 `Part::text` 的 `\n` 计数（提取器负责 `\r\n` 归一）；`section` 为
  容器内 child 的 1-based 序号；
- **跨窗如实重叠**：chunk 横跨单位边界时 `[start, end]` 如实取触碰区间，不去重。

## 2. 三源坐标（无第四处猜测）

1. **extract 源（页偏移表）**：与结构标签设计共享 Part 手术——pdfium 页循环 join 时天然知道
   各页起点；pdf-inspector 逐页产物同理。`Part` 增 additive 字段
   `pages: Vec<(u16, usize)>`（页号 → `Part::text` **字节**偏移；chunker 消费时统一换算到
   char 域——token_window 以 char 索引切片是刻意的 CJK 防线，换算义务写死在 chunker 侧）。
2. **chunker 源（行计数，免费）**：token_window 全文在手，窗口起点前的 `\n` 计数即行号——
   零提取器改动。
3. **容器编排源（section）**：M3 T7 epub/zip 落地时，child 摄取路径 stamp 子文档序号；
   键集先行占位，机制常数随 T7 定。

## 3. chunk 序号契约（向量世界的行号）

`{hash}-{modality}-{i}` 声明为**契约**（marker.rs 形状判据升格为契约条款）：

- `i` 随文档序**单调递增、连续无空洞**（token_window 逐窗产出天然保证——契约断言进测试门）；
- **枚举**：`vf chunks docs <src-key>` = ListVectors 分页 + 客户端 key 前缀过滤（`{hash}-`），
  按 `i` 排序输出（`grep -n` 的列表形态）；
- **邻域**：`i±N` 客户端可算（ADR-0004 已声明同款 key 结构约束，此处升格为正式契约）。

## 4. 有界读取原语（grep -C 语义）

```
vf chunk show docs <chunk-key> [--around N] [--chars B] [--kind-filter ...]
```

- `--around N`（缺省 2，上限 20 ⇒ 单次 1+2×20=41 ≤ GetVectors 100 限额，不分批）；
- `--chars B`（缺省 400/块，`0` = 全文）：**预算硬界**——超出截断并以 `…[truncated, next:
  {chunk-key}]` 标记续窗入口，「永不超 LLM 上下文」是构造属性；
- 读取的是**索引所见视图**（chunk 文本 = 管线产物，含家具剔除/标签后的内容）——agent 读到的
  与检索命中同源，无二次对齐；
- 输出含每块 metadata 摘要行（`x-vf-chunk-kind`、坐标键）——与结构标签、本设计坐标键互见。

MCP 侧不加新工具：`get_source_data` 全文语义保持；邻域读取经 `search`（命中即锚点）+
GetVectors 组合可达（skill 文档给 pattern）。

## 5. 查询面

- **坐标过滤**：坐标键是 filterable 数值，既有 `$gte/$lte` 直接可用——
  `vf search "容量规划" docs --filter '{"x-vf-page-start":{"$gte":10,"$lte":50}}'`
  （「只搜第 10–50 页」）；MCP/HTTP 同享；
- **命中呈现**：`hit.location` 增可选 `coord`（`{page?:{start,end}, line?:{start,end},
  section?:n}`，响应构建时从 metadata 摘出）；CLI 表格有坐标则显示（`p18-19` / `L405-420` /
  `§3`）；协议面响应零变化（QueryVectors 不摘 coord——metadata verbatim 已含）。

## 6. v2 演进钩子（本期不做，形态预留）

- **服务端原文窗口读**（`GetSourceContext {unit, start, end, maxChars}`）：读 raw 视图而非
  管线视图（chunk 邻域读不到的原文细节，如被剔除的页眉上下文）；触发条件 = raw 视图需求实证；
  新增第 9 个超集 op 需独立裁决；
- **印刷页码 label**（`FPDF_GetPageLabel`）、**字符级偏移**、**heading-path 坐标**（md 章节）：
  键集与坐标词汇表按 additive 演进。

## 7. 测试门

1. **页坐标正确性**：合成多页 PDF（golden 家具语料复用）——每 chunk 的 page-start/end 与
   pdfium 页数一致；跨页 chunk 区间如实重叠；
2. **行坐标正确性**：txt fixture——窗口行号与 `nl` 对拍；
3. **序号契约**：单 source 的 chunk 枚举连续无空洞、单调；reindex 前后 `i` 稳定（同指纹）；
4. **邻域读取**：`--around/--chars` 边界（越界跳过、截断标记、续窗 key 正确）；预算硬界
   （总输出 ≤ Σ chars 构造断言）；
5. **坐标过滤**：页范围收窄端到端（范围外零命中）；`$exists` 区分有无坐标格式；
6. **协议面零变化**：QueryVectors conformance 不回归；坐标键为 filterable 数值（黑名单制
   校验下协议面可直接过滤——与 kind 键同待遇）。
