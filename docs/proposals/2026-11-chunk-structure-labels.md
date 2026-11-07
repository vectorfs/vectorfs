# 提案：chunk 结构标签与提取期家具剔除

> 状态：proposed（2026-11-06）。来源：dogfooding 摩擦 F2/F3/F4（`research/2026-11-friction-backlog.md`），
> 机制研究 `research/2026-11-retrieval-friction-mechanisms.md` §2。
> 对应 ADR：`docs/adr/0003-chunk-structure-labels.md`；设计：`docs/design/2026-11-chunk-structure-labels.md`。

## 1. 问题

提取层把文档压平成无结构纯文本，三类污染直进向量：

- **F2 目录霸榜**：目录页是关键词密集袋，与任意主题查询语义相似度虚高——会话实证 top-5 全是
  目录块，真正答案块不在 top-20。成因是嵌入度量「共享主题/措辞」而非「答案相关性」（行业共识
  分析见 mechanisms §2.2）；
- **F3 家具混入**：页眉/页脚/页码/版权行逐页重复进入 chunk 正文与向量
  （"© Thoughtworks, Inc. 版权所有。 18"）；
- **F4 图例混排**：图表页图例数字混进文本（"62 63 54 暂缓 评估 试验 采纳"），根因 reading order。

病根：§5.10.2「一份逻辑文档一个提取产物」契约下，`pdf_text.rs` 页循环 `"\n\n"` 直拼——页界、
字号、书签在提取层丢弃，下游再无结构信号可用。

## 2. 目标 / 非目标

**目标**
1. furniture（页眉/页脚/页码/版权行）在**提取期剔除**——对检索零价值，docling/marker/MinerU/
   olmOCR 四家一致的默认策略；
2. TOC **标签化不剔除**——`x-vf-chunk-kind` 元数据标签，默认检索面可过滤（「哪章在哪页」类
   查询的合法答案，打标签放行而非打死）；
3. 双引擎（pdf-inspector / pdfium）产出**同一标签词表**——probe→affinity→route 回退链可组合
   的前提；
4. 检索策略在查询层（默认过滤 + 显式放行），检测在提取层——与 docling「检测在解析层、策略
   在导出层」分层同构。

**非目标**
- 不做多 Part 化 / 提取产物结构化重构（§5.10.2 契约不破，标注是 additive 字段）；
- 不做布局模型 / OCR（F4 的 caption 分类随 pdf-inspector markdown 模式自然获得，不单独立项）；
- 不做数值信号降权（业界无框架级实现，标签+过滤是共识路线）；
- 不做 section-path 反哺（outline → 正文 chunk 章节元数据，验证过价值但属增强项，后置）。

## 3. 方案选项

| 选项 | 评估 |
|---|---|
| **A. 提取层检测 + chunk 标签 + 查询层策略**（推荐） | 证据（页界/字号/书签）只在提取层可见，join 后不可恢复；标签以中立形式传出符合 DIP 边界（引擎细节不出 kind 模块）；Lance 侧走既有 filterable 元数据 + AllNulls 列演化，零 schema 破坏 |
| B. 查询层数值降权（term specificity/信息密度打分） | 无框架级先例，调参黑洞；治标不治本（家具文本仍在向量里） |
| C. 仅靠 $textMatch/hybrid（F1）绕过 | 词法路由缓解排序但家具/TOC 仍占词法高分（逐页重复行是完美 BM25 匹配）；不互斥，本提案与 F1 正交互补 |
| D. 提取产物多 Part 化（TOC 独立 Part） | 契约手术大（§5.10.2、chunker 单输入假设、contract 断言全动），收益与 A 相同 |

**决定性发现**：pdf-inspector 1.25.2（现依赖）已内置 `furniture.rs` 跨页家具剔除 +
`analysis.rs` TOC 行信号 + `structure_tree.rs` tagged structure 读取——现状 `markdown=false`
纯文本模式全部未激活（`extract/pdf_inspector.rs:88-112` 默认构造核验）。F3 治理成本接近
「翻默认值 + 接标签通道」。

## 4. 关键取舍

1. **furniture 默认剔除**：提取产物不再含页眉页脚原文；`vf cat` 回源原始字节不受影响。
   剔除发生在投影内，chunk 无损拼接契约（串接 ≡ 提取产物）不破——提取本来就是有损投影；
2. **TOC 默认排除出检索面 + `includeKinds` 放行**：默认行为变化（结果更准），显式参数可逆；
   备选「默认含 + excludeKinds」不采——多数查询不想要目录块，F2 会话实证即默认形态；
3. **`markdown=true` 升 pdf-inspector 默认 flavor**：输出从纯文本变 markdown（标题井号等），
   对嵌入通常无害甚至有益；属可见变化，走指纹/reindex 语义；
4. **标签词表最小化**：`body|toc|caption` 起步（closed enum，加值是 additive）；heading 等
   候选留给 section-path 增强项。

## 5. 依赖与风险

- 依赖：`Part` additive 标注字段；`Chunk.kind` + `x-vf-chunk-kind`（filterable，与
  `x-vf-modality` 同构处理）；domain.rs 键集声明、40KB 预检自动覆盖；pdfium 提取器内部改
  逐页提取（页界只在提取器内部使用，输出仍是单 Part）。
- 风险：启发式误判（furniture 误删正文——缓解：跨页重复证明需 ≥3 页 + 位置证据双门槛，
  短文档证据不足时宁可不删）；TOC 判定漂移（≥50% 行命中才整页判 toc）；存量索引需 reindex
  才获得标签（指纹变更自然触发）。
