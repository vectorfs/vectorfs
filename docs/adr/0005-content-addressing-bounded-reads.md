---
status: proposed
---

# 内容寻址与有界读取：坐标键集 + chunk 序号契约 + grep -C 式邻域读

> **修订（2026-11-07，专项审查后）**：分叉判据落定为 `Part.pages` 在场与否（页键无行键/
> 行键无页键，容器子文档行键+section 叠加）——原「三源无第四处猜测」在分叉处恰有未写判定；
> pdf-inspector 侧页界**仅 markdown flavor 可得**（`process_pdf_mem` 单串丢页界，换轨=指纹
> 变更=reindex；plain flavor 无逐页 API，不产 page 键）；页号 u32；`store_chunk_text=false`
> 时 `vf chunk show` fail loud；枚举 early-exit + `object_key_hash` 下沉 common；跨模态邻域
> 契约即刻生效（M3 image 已在产）；`location.coord` 落实并超集附录 E.2 既有 `page?` 钩子。
> 完整修订见 `docs/design/2026-11-retrieval-quality-solution.md` §1/§2-T13/T14（冲突以彼为准）。

检索命中后的 agent 回路需要「精确定位 + 局部上下文」，现状只有不可寻址的命中与全文透传两个极端——大 PDF/EPUB 全文可超 LLM 上下文，全文读取不该是默认动作。机制三件套：**(1) 坐标键集** `x-vf-page-start/end`（pdf）、`x-vf-line-start/end`（txt/md/容器子文档）、`x-vf-section`（容器序号），全部 filterable 数值，`$gte/$lte` 支持范围收窄；**(2) chunk 序号契约**——`{hash}-{modality}-{i}` 升格为有序、连续、可枚举、可邻域寻址的文档地址空间（向量世界的行号）；**(3) 有界读取**——`vf chunk show --around N --chars B` 客户端邻域读（grep -C 语义），预算硬界 + 截断标记 + 续窗指引。坐标三源：extract 页偏移表（与结构标签共享 Part 手术）、chunker 行计数（免费）、容器编排 section（T7 `walk_child` 已在产，补 stamp）。物理页索引先行，印刷 label 后置。2026-11-06 用户设计拔高（「像 grep 定位到行、只读相邻几行，而非全文件」）。

## Considered Options

- **服务端原文窗口 op（GetSourceContext）**——能读 raw 视图（含被剔除家具），但新增第 9 个超集 op + 定页重提取管线；chunk 邻域已覆盖 agent 回路（索引所见视图与命中同源），需求实证再上，留 v2 钩子
- **仅页码键、不契约化序号**——txt/epub 无坐标、`chunk show` 无契约根基，治标
- **依赖 M3 T3 页图像**——页归属被图像模态（本务是渲染嵌入）阻塞，文本块坐标与图像无关
- **字符级偏移**——粒度溢出 chunk 语义，v2 按需

## Consequences

- chunk key 的 `{hash}-{modality}-{i}` 结构成为正式客户端契约（枚举靠 ListVectors + 前缀过滤、邻域靠 `i±N` 客户端可算）——key 结构演化须保持该解析兼容（与 ADR-0004 约束同源、此处升格）
- 坐标键只在单位存在时写入（`$exists` 区分格式）；跨单位边界的 chunk 区间如实重叠不去重；存量索引 reindex 后获得坐标（与结构标签同批迁移）
- 命中呈现：`/x-vf/Search` 的 `hit.location.coord` 摘要 + CLI 坐标列；协议面零变化（metadata verbatim 已含坐标键，filterable 数值按黑名单制校验直接可过滤）
- 读取的是索引所见视图（管线产物）而非原文——agent 读到与检索命中同源；raw 视图留 v2
- Part 手术与结构标签设计共享（页偏移表与 labels 同一 additive 字段族）；chunker 行计数与字节↔char 换算义务（CJK 防线）写死在 chunker 侧
