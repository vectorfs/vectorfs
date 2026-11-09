---
status: accepted
---

# chunk 结构标签：提取期家具剔除 + `x-vf-chunk-kind` 标签化 TOC

**已执行**（T12 三切片，2026-11-08~09：c1d63c7 地基 / d82a339 提取器 / 961c4cb 盖章+查询面）。
实施对裁决的三处证据驱动修订（均记录于 solution.md §1）：断窗递推经三分支穷举定稿为**端点强制
+ overlap 照常跨界**（撤回「独占窗口」过强保证）；TOC 门 v2（标题硬门被旗舰活语料证伪——真目录
页无标题，范围合理门接防残差职责）；词表 `body|toc|caption|legend` 落地。旗舰门（5 期雷达真语料）
零整页误判 + 零词表级误删过门。**升级注意**：`structure = true` 默认开 = 显式 pdf-text 配置桶全员
指纹变更拒启 ⇒ reindex；auto 哨兵指纹缺口见 solution.md §2-T12 迁移说明。

> **修订（2026-11-07，四路审查后）**：TOC 机制按 D-1 重设计为「**页级判定 + 强制断窗**」
> （原窗口 50% 字节覆盖率被旗舰语料实证击穿——混合窗口目录覆盖率仅 6.6%，无导符目录行
> 击穿点导符谓词）；词表扩为 `body|toc|caption|legend`（D-5，图例块默认排除）；
> pdf-text 增首个配置 knob `structure`（家具剔除/TOC 判定入指纹）；pdf-inspector 的
> `is_toc_entry_line`/家具函数系 `pub(crate)` 不可复用，改自实现；卷末索引（无页码形态）
> v1 明确不治。协议面措辞统一「零破坏性变化」。完整修订见
> `docs/design/2026-11-retrieval-quality-solution.md` §1/§2-T12（与本文冲突时以彼为准）。

文档结构识别按「证据在 extract、标签到 chunk、策略在检索」分层落机制：furniture（页眉/页脚/页码/版权行）在提取产物内**默认剔除**（全行业默认：docling furniture 层 / marker ignore_for_output / MinerU abandon）；TOC **标签化不剔除**——新 filterable 元数据键 `x-vf-chunk-kind`（utf8 闭枚举，缺省 body），`/x-vf/Search` 默认排除 toc/legend、超集参数 `includeKinds` 显式放行。检测双引擎同一词表：pdf-inspector 走 `markdown=true`（激活 crate 内置 furniture 剔除 + TOC 行信号，现状纯文本模式全部旁路）+ tagged structure 高置信轨道；pdfium 页循环内跨页重复剔除 + TOC 整页判定。§5.10.2 单 Part 契约与 chunk 无损拼接不变量不破（标注是 Part 的 additive 字段；剔除发生在提取投影内）。2026-11-06 机制研究背书（`research/2026-11-retrieval-friction-mechanisms.md` §2）。

## Considered Options

- **查询层数值降权**（term specificity/信息密度打分）——无框架级先例、调参黑洞、家具仍在向量里，出局
- **仅靠 hybrid 词法路由**——逐页重复行是完美 BM25 匹配，不排 TOC 会复刻霸榜；与本案正交互补，不构成替代，出局
- **提取产物多 Part 化**（TOC 独立 Part）——契约手术大（§5.10.2、chunker 单输入、contract 断言全动）而收益相同，出局
- **TOC 默认剔除**（docling 社区手动删的路线）——「哪章在哪页」是合法查询，打死丢能力；标签 + 默认过滤可逆，不采

## Consequences

- 提取产物不再含页眉页脚原文（`vf cat` 回源原始字节不受影响）；存量索引 `x-vf-chunk-kind` 列 null 视为 body 不排除，reindex 后获得真实标签
- pdf-inspector 默认 flavor 变 `markdown=true`（可见输出变化，指纹变更 ⇒ 走既有 reindex 语义）
- 元数据键集 §5.10.1 显式增补 `x-vf-chunk-kind`（filterable，与 `x-vf-modality` 同构）；40KB 预检自动覆盖；Lance 镜像列 AllNulls 演化零破坏
- 协议面 QueryVectors 零变化（不做隐式 toc 排除——metadata 过滤是用户显式行为）
- 误判守门为机制常数（2026-11-07 对抗性审查后升级）：家具剔除需跨页重复证明 + **页边位置守卫 + 最小行长 + 字母含量**（对齐 furniture.rs 全套；跨页表头降级 caption 标签不删除；furniture_removed 计数可审计）+ 短文档（<3 页）宁可不删；TOC 判页需 **strict 行谓词（页码前 ≥1 非数字字符）+ 行占比 ≥50% + 页码列单调性 + TOC 标题行在场**（四重缺一不可——裸谓词在旗舰语料上把象限一览页误判 TOC 致整页静默蒸发）；~~非 body 覆盖 ≥50% 才改标 chunk~~（已被 D-1 页级判定 + 强制断窗取代，见 solution.md §1）
- section-path 反哺（outline → 正文 chunk 章节元数据）留作后续增强，本期不做
