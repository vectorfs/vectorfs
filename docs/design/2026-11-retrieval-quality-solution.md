# 检索质量与寻址：完整解决方案（M3.5 提案，T12–T15）

> 状态：proposed（2026-11-07）。本文是四组文档（`docs/{proposals,design}/2026-11-*` × 4 + ADR
> 0002–0005）经四路独立审查（协议/检索质量/代码可行性/内容寻址专项，2026-11-06~07）后的
> **收敛权威版**：交付基线对账 + D-1..D-7 决策落定 + 任务分解。与既有分组文档冲突时**以本文为准**。
> 证据链：`research/2026-11-friction-backlog.md`（摩擦 F1–F9）、
> `research/2026-11-retrieval-friction-mechanisms.md`（机制研究）、四路审查报告（会话存档）。

## 0. 交付基线对账（2026-11-07，并行会话 M3 已过门）

| 本方案原项 | 现状 | 处置 |
|---|---|---|
| F1 词法检索/hybrid | **已交付**（M3-T9，ADR 0002 执行）：派生表顶层 `text` 列（put_vectors 单源双写，blob 键保留回显）、`fts`（默认关）+ `fts-tokenizer`（icu 默认/ngram 2,2，换档入指纹）、`searchMode: semantic\|keyword\|hybrid`、hybrid 复用 rrf/weighted、内核薄 trait `LexicalKernel`、FTS 索引挂 `vectorfs optimize` 动词、1M 实测 43–54x、造词 e2e + 分词差分断言 | ADR 0002 → accepted（实现形态为权威）；lexical 组提案/设计加「已交付」横幅并记录三处实现分歧（列名 `text` 非 `chunk_text`；fts 默认关非 icu-on；索引经 optimize 非惰性 ensure——**optimize 路线更优**：守「daemon 从不自行建索引」纪律，审查一的次序 major 就此消解） |
| F8 页归属 | 未交付（`location.page?` 仍为 M3+ 注释；`x-vf-src-page` 未 stamp） | 归入 T13 |
| F2/F3/F4 结构 | 未交付 | T12（按 D-1 重设计） |
| F5/F6/F7/F9 | 未交付（src-name 过滤仍 400） | T14/T15 |
| 交互面 | reranker（T11，merge→truncate→rerank，rerank 不入指纹先例）；markdown chunker（T6，**heading 边界优先断窗**——TOC 断窗的同构先例，windowing 共享模块）；epub/zip 容器（T7，`walk_child` child 序循环在产——section 坐标源落点存在）；pdf_pages（T3，页渲染图像原子——image 原子天然携带页号）；IVF_PQ/weighted（T8，`optimize` 动词） | 各任务交互条款见 §3 |

## 1. 决策记录（D-1..D-7 落定，全部有审查证据链）

**D-1 TOC 治理 = 页级判定 + 强制断窗 + 三重守门**（原「窗口 50% 字节覆盖率」方案被检索质量路
证伪；首轮 D-1 又被对抗性审查证伪——**旗舰语料自身 5/46 页越崖**：雷达总览页 p9（97% 行命中）、
象限版页 p29/p39（73%/77%，恰为 F2 枚举查询的标准答案页）被裸谓词误判 TOC，默认排除即正文
整页静默蒸发。修复经实验验证：三重守门下 5 假阳性全灭、全部真目录保留）：
- **行谓词（strict 变体）**：尾随 1–4 位数字**前须 ≥1 个非数字字符**（裸版号行 `41 42`、
  `57. D2` 类不命中——实测雷达零假阳性）；导符任意（含空格/无导符）；「目录/Contents/Table of
  Contents」标题行加分；
- **页码列单调性 = 第二证据**：整页命中行的尾随数字列须**单调不减**（真目录页码有序；一览页/
  价格表/SKU 表的尾随数字是条目序号或金额，非单调）——实测 5 假阳性全灭、真目录 p2/p4–p7
  全保留；
- **无 TOC 标题行 ⇒ 放行不判**（页占比再高也不强制判 TOC，只打行级标签供 includeKinds）；
- **页级判定（v2，2026-11-08 活语料修订）**：strict 行占比 ≥50% + 单调性 + **范围合理门**
  （命中页码 ≤ 文档总页数）⇒ 判 TOC，**行级 span 输出**（只标达标行）。~~标题行硬门~~被旗舰
  语料证伪：雷达真目录页无「目录/Contents」标题，v1 全语料零判出（治疗落空——D-4 硬条件拦下）；
  探针证实 5 个实测假阳性页由占比门独立承重（strict 谓词下 hits=0 或 ratio ≤3%），范围门为纯
  增益防线（排序价格表金额溢出页数上界）。标题词表存档 `structure.rs` 模块文档，消费者出现可
  复挂。窄残差（小金额升序列在大文档内）如实声明并单测钉住——爆炸半径 = 行级 span；
- **页级判定（v1 存档）**：标题行在场 + strict 行占比 ≥50% + 单调性 ⇒ 整页判 TOC（span 全页）；
  页内家具同行剔除；
- **强制断窗 = 端点强制（断窗递推规范，对抗性审查 TOP1 三分支穷举后裁决）**：`windowing::ranges`
  增 `forced` 端点集——窗口**端点**不得越过 span 边界，但 overlap 递推 `s'=e-overlap` **照常
  跨界**（三分支穷举：阻断 overlap = 重建 skip 跳过未发射文本 = **违反 lossless 契约**，除非修订
  §5.10.2 本身；朴素 clamp 在边界落于 `(s, s+overlap]` 时零/负步长 = **死循环**；端点强制是唯一
  保四不变量的分支）。后果如实陈述（**撤回「独占窗口/无阈值抖动」的过强保证**）：TOC 污染上界
  = overlap 字符（页尾泄入 body 窗的量有界）；**页 ≥ size 时窗口纯净**（幻灯片类小页语料窗口横
  跨 2–4 页，kind 判定退回歧义仲裁：窗口所跨页 kind 集合有歧义 ⇒ `body`——宁可漏排不可误排，
  recall 保护）；`B ≤ s+overlap` 退化情形跳段处理（不 clamp）；markdown chunker 的 heading 切点
  是 PREFER 非同构（其 overlap 自由跨界，旗舰测试自证），仅作参照；
- **页级判定输出行级 span**（对抗性审查 TOP3）：页判 toc 时只标注**达标的 TOC 行**（非整页字节
  单值化——混页（目录+版权+图例）保住 legend/caption 身份，D-5 排除链不失效）；
- **卷末索引**（无页码形态）v1 明确不治，v1.1 独立谓词族——范围诚实声明。

**D-2** lexical 不入 `derived_key`，换代只走管线指纹——**T9 已按此实现**（`fts-tokenizer` 入指纹、
省略保旧指纹），消解协议路 M1。

**D-3** icu 默认维持（T9 已定，分词差分断言在测）；MIRACL-zh 证据降格为「方向性参考」记录，
「icu≈jieba 0.40 档」表述废弃（检索质量路实证 ICU 词典弱于 jieba 档：`暂缓→暂|缓`）。

**D-4** golden 语料 = `build_multipage_pdf`（`tests/golden_corpus.rs:70-129`）扩展：每页画重复
页眉行（furniture）、无导符目录页（TOC）、图例页（legend）、**一览页/价格表页/环词分组页三类
反例 fixture**（对抗性审查：缺这三类则测试门是绿灯谎言——TOC 假阳性与家具误删恰恰发生在这些
形态）——全合成，零版权材料。**T12 过门硬条件升级**：旗舰语料（docs 桶 5 期雷达 + 教科书，
INDEXED 现成）**零整页误判 + 零词表级误删**——过不了这道门不上生产。

**D-5** 词表扩为 `body|toc|caption|legend`；**legend 默认排除**（图例块已居语义路 rank 2，词法路
必放大——BM25 极短文档病态）；pdf-inspector markdown 的 caption 分类尽力标注，pdfium 路线文档
声明限制；reranker 为二道防线（候选经 kind 排除已在 merge 前净化）。

**D-6** `location.coord{page?,line?,section?}` = 附录 E.2 既有 `page?` 钩子的**落实与超集**
（supersedes 单值形态）；E.2 hit 形状与 E.4 命令表更新义务随 T13/T14 履行。

**D-7** 分叉判据 = **`Part.pages` 在场与否**（在场 ⇒ 页键无行键；缺席 ⇒ 行键）；容器子文档 =
行键 + section 叠加（编排源）；md 源的 line 属提取产物域（md.rs 剥标题改行结构），CLI 注记。

## 2. 任务分解

### T12 chunk 结构标签（ingest 侧，与 T13 共享 Part 手术）

**机制**（修订结构标签设计 §1/§2，以本文为准）：
1. `ChunkKind{Body,Toc,Caption,Legend}`（kebab-case 字符串入 metadata）；`x-vf-chunk-kind`
   filterable、缺省显式 `"body"`；
2. `Part` additive 双字段：`labels: Vec<LabelSpan{字节区间,kind}>` + `pages: Vec<(u32,usize)>`
   （T13 共用；字节锚定，**chunker 统一换算 char 域**——windowing 的 CJK 防线）；
3. pdfium（`pdf-text`）：**首个配置 knob `structure = true`**（进指纹——家具剔除/TOC 判定是
   行为变化，无 knob 则指纹漂移，审查一 major）；页循环内跨页重复家具剔除——**守门对齐
   furniture.rs 全套**（对抗性审查证伪一句话规则：裸「≥20% 页重复或连击 ≥3」会把雷达四环词
   `评估×22页/试验×20/暂缓×11/采纳×9` 与四象限名整词表删掉）：页边位置守卫（pdfium `.all()`
   无 Y 坐标，以页首/尾 N 行窗口近似）+ 最小行长 + 字母含量 + <3 页宁可不删；**跨页表头/版页
   标题类重复降级不删除**（打 `caption` 标签，可检索可排除，保住可逆性）；`furniture_removed`
   计数进 ingest status（静默行为变可审计行为）+ D-1 页级 TOC 判定 + 页偏移表；
4. pdf-inspector：markdown 产物上**自实现** TOC 行启发式（`is_toc_entry_line`/家具函数均
   `pub(crate)` 不可复用——审查一 major；TOC 行在 markdown 输出保留为纯文本，路径成立）；
   caption 分类尽力 → `Caption/Legend`；
5. chunker：端点强制断窗（`windowing::ranges` 增 `forced` 参数 + 退化跳段，§1-D-1 断窗递推
   规范）+ 窗口 kind = 纯净页主导 / 跨页歧义 ⇒ body；**chunker contract 套件必须跑带
   labels/pages 的 fixture**（现有套件只构造无标注 Part，断窗路径否则空洞通过——LSP 防线
   失明）；`golden_corpus::build_multipage_pdf` **段落唯一化改造**（现每页同段——家具规则会把
   段落与页标整删，`golden_huge_multipage_pdf_extracts_every_page` 转红；D-4 是重造非扩展）；
6. 查询面：`/x-vf/Search` 默认排除 `toc|legend`——**否定形态谓词**（`kind IS NULL OR kind NOT
   IN ('toc','legend')`，防 SQL 三值逻辑把 null 行全排除——协议路 m3）；`includeKinds` 放行；
   CLI `--kind`（可重复）；kind 排除与 rerank 一样**不入指纹**（ranking policy ≠ chunk identity，
   T11 先例）；
7. 协议面措辞统一为「**零破坏性变化**（无 op/schema/拒绝语义变化；`x-vf-chunk-kind` additive
   可见/可过滤，`x-vf-modality` 同构先例）」——消解 m1 款张力。

**升级影响（对抗性审查 #3 + 契约路 TOP2 处置，诚实披露）**：`structure = true` 默认开 = 全员
指纹变更 = 升级即 `fingerprint_gate` 拒启（`ingest/mod.rs:228-241`）⇒ 存量 PDF 桶全部
`vectorfs serve --reindex`——**整桶翻转**（含 txt/docx 等提取行为未变的 source 一并重嵌，
`ingest/mod.rs:2989-2997`；openai embedder 部署 = 真实 API 账单）+ 强制断窗致 chunk 数**永久
膨胀**（实测上界雷达 +14% / 教科书 +15%）+ **ADR 0003 的 markdown=true flavor 变更叠加同一波
迁移**（两处代价合并进同一份迁移说明）。缓解三件随任务交付：错误信息指路完整；
`vf reindex --dry-run`（受影响 bucket/文件数/预估嵌入调用数——doctor 预检同源）；膨胀率写进
迁移说明。**裁决点（实施前定，三选一）**：默认 true + 恒序列化 + 迁移章节（推荐——提取质量
是本任务存在理由，默认关则治理对存量用户永不生效；注意 pdf_inspector 先例是「值即规范」恒
序列化，pre-T12 指纹**不可复现**，elide 分支会击穿「指纹管块集身份」语义故不可取）/ 默认
false 换档期（T9 fts 哲学，升级零成本）/ `structure = "legacy"` 一次性兼容档。**迁移注记**：
默认排除 toc/legend 使升级后「哪章在哪页」类查询默认零命中（需 includeKinds）——结果集内容
变化写入 release note（「零破坏性」指 wire/schema/拒绝语义，不含默认结果集内容）。**auto 路线指纹缺口（切片 3 补记，
预先存在的结构性属性）**：`extract = { default = "auto" }` 桶的指纹只记哨兵串 `"auto"`，不含 builtin
pdf-text 的解析配置 ⇒ 该形态部署的 `structure = true` 行为变化**不触发** fingerprint 拒启——显式
`[extractor.*] kind = "pdf-text"` 实例（含全部 preset）指纹必变、拒启/reindex 语义成立，auto 桶需
**手动 `vectorfs serve --reindex`** 才获得家具剔除与标签（否则新摄取文件带标签、存量文件无标签的
混装持续）。修法（auto 路线解析结果入指纹）是指纹结构变更，留待独立裁决，本切片不动。

**测试门**：无导符目录页语料（D-4 合成）TOC 页判出 + 断窗纯度；**存量 null 行默认可见**断言
（防三值逻辑回归）；家具不误删（跨页表头已知误伤面文档化 + 短文档双引擎行为统一：pdfium
不删、pdf-inspector 位置策略删——**收敛为两路都删或都不删**，本任务裁决：都删（证据足），
<3 页双路都不删）；双引擎同词表；lossless 契约不回归；QueryVectors conformance 不回归；
**旗舰语料零整页误判 + 零词表级误删**（D-4 过门硬条件）。

### T13 内容寻址坐标（三源 + 通道 + 响应面）

**机制**（修订内容寻址设计 §1/§2/§5，以本文为准）：
1. 坐标键（page×2 + line×2 + section = **5 键**，+ `x-vf-chunk-kind` 同批）：`x-vf-page-start/end`、
   `x-vf-line-start/end`、`x-vf-section`，全 filterable 数值；只在单位存在时写入（D-7 判据）；
   页号 **u32**（content-addressing.md 的 u16 表述以本文为准）；跨窗区间如实重叠；metadata 键数
   核算封口：现 5 键 + 新至多 4 = 9 ≤ 50（AWS 上限，契约路已验证不触顶）；
2. 三源：extract（页偏移表，T12 手术共用；**pdf-inspector 仅 markdown flavor 可产页界**——
   `process_pdf_mem` 单串丢页界，`include_page_numbers` 标记会**注入 `<!-- Page N -->` 进
   `Part::text`（进 chunk/嵌入/FTS 的文本污染，须后剥离且剥离后页偏移重算）**或换逐页 API，
   换轨 = 指纹变更 = reindex，明示；plain flavor 无逐页 API，不产 page 键，按 flavor 分档承诺
   ——审查四 blocker 修正）；chunker（行计数免费，`\n` char/字节域同构无陷阱）；编排
   （`walk_child` child 序 → section，T7 已在产，落点补 stamp）；`\r\n` 归一义务在两 pdf 引擎内
   实现（txt 透传即精确；pdfium `.all()` 无字号/坐标，家具位置守卫用页首/尾 N 行窗口近似、
   精确位置证据需 char-level API——实现注记）；
3. 通道：`Part.pages/labels` → chunker（char 换算 + 窗口坐标计算）→ `Chunk` 坐标字段 →
   `build_rows` 注入（与 kind 同批）；
4. 响应面：`hit.location.coord{page:{start,end},line:{start,end},section}`（E.2 落实，D-6）；
   CLI 坐标列（`p18-19`/`L405-420`/`§3`；md 加产物域注记）；
5. **迁移语义**：kind/坐标来自提取产物 ⇒ 变更走 **reindex**（指纹变更），不是 optimize
   （optimize 只回填既有 metadata 派生物，如 T9 的 text 列）——与 T9 迁移语义的分工写明。

**测试门**：页对拍（`build_multipage_pdf` 页标记）；`nl` 对拍 txt；reindex 前后 `i` 稳定
（pdfium .so 版本不入指纹的既有边缘披露）；范围过滤端到端；`$exists` 区分。

### T14 寻址读取动词（客户端为主）

1. `vf chunks docs <src>`：ListVectors 前缀枚举（`{hash}-`）+ **客户端 early-exit**（页首键
   前缀失配即停——无服务端前缀谓词，成本 O(哈希区间)，非 O(全索引)）；`object_key_hash` 下沉
   `common`（客户端复刻或下沉，二选一裁决：**下沉 common**——sigv4 同源一套的先例）；按 `i`
   排序输出，含坐标/kind 列；
2. `vf chunk show <key> [--around N=2,≤20] [--chars B=400] [--kind …]`：GetVectors ≤100 限额内；
   **`store_chunk_text=false` ⇒ fail loud** 指向该配置（禁静默空体——审查四 major）；
   预算硬界 + `…[truncated, next: {key}]` 续窗标记；**`--chars 0`（全文）与 `--around >0`
   互斥**（对抗性审查：size 是无上限管线配置，41×全文会静默击穿「永不超上下文」承诺），
   全局输出预算上限构造性兜底；
3. **跨模态邻域契约（枚举优先）**：邻域**按全局 `i` 枚举**（`vf chunks` 前缀枚举得真实键序，
   非按键的模态段猜键——**交织序今天就存在**：容器 child parts 按 child 序聚合（txt+jpg 混合
   zip 的 text/image i 交织），猜键系统性漏真实邻居，契约路次级 2 修正）；跨模态邻居如实返回
   （非静默跳过）；
4. E.4 命令表更新义务。

**测试门**：真源枚举（833-chunk PDF，含 image 源混合）；预算硬界构造断言；越界/非法键/跨模态
行为。

### T15 工效收尾（查询/CLI/文档）

1. `/x-vf/Search` src-name 翻译：`$eq`（含 shorthand）/`$in` → `x-vf-src-uri` 哈希（编译前
   JSON 层重写——filter compile 现位于 `ops_xvf.rs:977-985`，重写插其前；M3 后行号已漂移，
   旧锚 :745 作废）；递归 `$and`；不可译算子 400 **path=filter** 附指引；keyword 不可用类 400
   **path=searchMode**（对齐 searchMode 校验 `ops_xvf.rs:837-849` 先例——协议路 m5）；
   **空命中提示**（对抗性审查：不存在键名 → 哈希零命中 → 静默空结果；翻译后零命中且 filter
   含 src-name 时响应附提示「name 未匹配任何已索引源」——打错字≠没搜到）；constraints 触顶
   （100 上限，`filter.rs:61`）错误信息**指回用户所写名字**而非哈希值；
2. CLI `--source <key>`（可重复 ⇒ `$in`）；SOURCE 列尾段省略策略（列已存在，改截断——审查一
   minor 修正）；
3. F7：`vf get` 双位置参数误用的 clap 错误加路径形态提示；F9：分数语义文档；
4. **文档义务**（协议路 m4 + 契约路次级 3）：vf-search skill 的「only x-vf-src-uri filterable」
   句与挖哈希 pattern 更新；`crates/client mcp.rs` search filter 描述补键可用性两面差异注记；
   **server 端 `vectorfs mcp` 同进程暴露 `query_vectors`（协议语义：src-uri-only、无 kind 排除）
   与 `search`（x-vf 语义：src-name 翻译、默认排 toc/legend）**——两个工具对同一索引给不同
   结果集，工具描述里必须写明语义归属，防 agent 混用；
5. 测试门：翻译端到端 + 协议面 400 不回归 + constraints 计数。

## 3. 与已交付面的交互条款

- **reranker（T11）**：kind 排除发生在 merge 前 ⇒ rerank 候选天然净化；「keyword+rerank 拒绝」
  语义不动（BM25 序即契约）；kind 排除同 rerank/merge-weights 不入指纹（ranking policy 先例）；
- **`vectorfs optimize`**：只管索引物理层回填（T9 text 列先例）；kind/坐标需重提取 ⇒ reindex
  语义，两动词分工文档化；
- **image 模态**：pdf_pages 原子 = 页渲染 ⇒ T13 page 键对 `-image-{i}` 同样 stamp（extractor
  天然知道页号）；`vf chunks` 枚举跨模态并集；
- **markdown chunker**：heading 断窗先例 = T12 强制断窗的实现参照（`windowing` 共享模块扩展）；
  md 源的 line 坐标属产物域（注记）。

## 4. 实施序与守门

**序**：T12 → T13（共享 Part 手术，一次 reindex 窗口）→ T14（依赖 T13 坐标列展示，chunk show
本体无依赖可并行）→ T15（独立，随时）。每任务一提交、主会话复跑验收（K0 标准作业）。

**守门常数**（机制常数，非配置）：家具剔除 ≥20% 页重复或连击 ≥3、<3 页双路不删；TOC 页占比
≥50%、导符任意含空格；断窗 span 边界强制；kind 主导无阈值；邻域 ≤20；枚举 early-exit。

**里程碑命名**：M3.5（检索质量与寻址）——命名与排期待用户裁决（M3 已过门，M4 音视频未被
本方案阻塞）。
