---
status: accepted
---

# EPUB 默认认领 + 容器护栏改膨胀比判据

两项用户裁决（2026-11-11，起于真实语料入库：两本长篇小说 1560/1426 章撞
`CONTAINER_MAX_ENTRIES=256` 护栏）：

**① epub 进默认支持**：epub 是**文档格式**（一本书 = 一份文档，与 pdf 同类），归文档默认——
`builtin_extractor` 的 auto 认领集（原仅 pdf/md/txt）纳入 epub，text-rag preset 的 extract map
显式带 `epub = "epub"`。T7 豁免裁决对 **zip/eml（任意档案）维持不变**——任意档案的展开
fan-out 仍是操作者声明。auto 认领集变化使 auto 部署解析路由漂移 ⇒ 指纹门 ⇒ 显式重嵌
（auto 指纹语义，尾巴裁决 A）；显式配置部署零影响。

**② 容器护栏改判据**：条目数只是炸弹的弱代理，**膨胀比才是本质**——`CONTAINER_MAX_ENTRIES`
256 → **4096**（有界放宽，长篇小说合法），新增 `CONTAINER_EXPANSION_RATIO = 256`（子项解压
字节流式累加 ≤ 容器字节 × 256，超即拒）；per-entry 256MiB 上限不动；ingest 侧 worker
`MAX_CHILDREN` 同步 4096（提取器与摄取层两常数必须联动，否则提取放行而摄取丢子文档）。

## Considered Options（护栏）

- **维持 256**——两本合法小说 FAIL 留档，长篇小说单源入库不支持，出局
- **常数提到 4096 即止**——放行小说，但纯计数仍拦不住"少条目巨膨胀"炸弹（如单条目解压
  数 GiB——per-entry 上限部分兜底，聚aggregate维度仍缺），不完整
- **膨胀比 + 放宽计数（采纳）**——两条弱代理合璧：计数 4096 拦"海量小条目"型，膨胀比 256×
  拦"巨膨胀"型；真实书籍（1560 章 ×~6KB ≈ 1:1）远低于双上限
- **配置轴化 max-children**——护栏值变操作者声明，把安全边界外包给配置，不采

## Consequences

- text-rag 新部署开箱认领 epub（解包 → OPF spine 定序 → XHTML 子文档重入用户路由）；
  **章节（text/html）走 `default` 路由**——txt 的 text/\* 族门放行原文 XHTML 入索引；
  要标签剥离的净文本章节，操作者加显式 `html = "html"` 实例（模板注释注明）
- **`default = "auto"` 部署认领并解包 epub，但章节因 html 仍豁免而无链**——source 以
  「无子内容产出 + 逐章 no-chain 理由」响亮失败（可操作：加 html 路由即通）。这是裁决拆分
  （epub 进、html 不进）的固有语义
- zip/eml 维持显式认领（操作者声明）；img/docx/html 的 auto 豁免维持（T7 原裁决，未被本裁决触碰）
- 容器错误消息分双分支（条目过多 / 膨胀比超限），各带两数字；膨胀比计费在读取 chunk 循环内
  流式进行（越限即拒，非整条目读完才判）
- auto 部署一次性指纹漂移（迁移注记入 preset 文档）；显式部署零影响
