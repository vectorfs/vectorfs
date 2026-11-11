# 设计：叙事语料召回缺口三机制（R7/R8/R9，任务 T16–T18）

> 状态：**revised**（2026-11-11，双路独立评审 + 交叉验证后修订版；v1 见同日早稿）。
> 提案：`docs/proposals/2026-11-narrative-recall-mechanisms.md`。
> 谱系：`research/2026-11-recall-quality-optimization.md` R1–R6 延长线；任务编号续
> retrieval-quality-solution 的 T12–T15。
> 评审与处置记录：§6（两路独立评审 28 项发现、5 项关键争议断言经主会话亲自核验全部坐实）。

## 0. 事实基线（2026-11-11 会话 + 源码核验；v2 修订两处失实）

| # | 事实 | 锚点 |
|---|---|---|
| F-1 | 锚词候选要求含 ASCII 字母，纯 CJK 永不入选；引号整串同样被拦 | `lexical.rs:231`，`:22-24` 模块文档自述 |
| F-2 | df 探针缺省 = `search(..., PROBE_DF_LIMIT=64).len()`（**计数封顶 64**，cap 的既存理由只覆盖 ceiling=16）；单头锚 MUST（v2 裁决）；ceiling=16 雷达标定 | `lexical.rs:104-109/186/192`，`select_anchors` |
| F-3 | `LexicalKernel` trait 现四方法（name / search / search_gated / probe_df），boxed-future dyn-compat，扩展形态 = ADDITIVE-with-default | `lexical.rs:40-110` |
| F-4 | 桶→tokenizer 查询期可解析（`fts_tokenizer_of`，`ingest/mod.rs:3020`；optimize 侧同源） | `optimize.rs:134-135` 同默认 icu |
| F-5 | CCH 前缀 = 文件名 stem >（flat 文档的）最近前置 TOC 条目；容器子 part 无 TOC 页 ⇒ 前缀退化为常量 stem | `windowing.rs:114-132`，`chunk/mod.rs:115-119` |
| F-6 | section = 容器 walk 的 child 序（**每源独立从 1 计**）；epub spine 序 = 章节序 ⇒ section 即章节轴 | `ingest/mod.rs:923-932`，`token_window.rs:203` |
| F-7 | T13 坐标键 + kind 全 filterable；`x-vf-section` 范围过滤实测生效；镜像列写时动态演化（新标量键自动进 filterable map，无需注册） | 实测；`ops.rs:1186-1206`，`store.rs:1976-2001` |
| F-8 | `/x-vf/Search` 已有 searchMode + includeKinds + anchors 成员；CLI 已有七检索旗标；**anchors CLI `--json` 透传与 human stderr 行已在 HEAD 交付**（随 R1 提交 50f32d1） | `ops_xvf.rs:1067-1077/1220-1230/1273-1286`；`vf.rs:235-278/1097-1135` |
| F-9（v2 修订） | ~~skill 文档义务未执行、anchors 未透传~~ **两处仓库义务均已在 HEAD 清偿**（仓库内 `skills/vf-search/SKILL.md` 已记载七旗标与 anchors 面）；会话实测的失实源于**部署侧过期**（已安装的 `vf` 二进制与 `~/.zcode/skills` 副本均旧于 HEAD）——教训入 §6：观测结论必须锚定二进制版本 | `skills/vf-search/SKILL.md:34-93`；`vf.rs:1112-1114` |
| F-10 | 管线指纹只哈希**配置**（extract_routes / chunk / embedders / charset / fts_tokenizer）——纯代码行为变化不翻指纹；`EpubConfig` 现无任何 knob（"reading order is a format fact"） | `config.rs:1322-1383`，`epub.rs:456-468` |
| F-11 | lance 的 icu 分析器是**六段管线**（IcuTokenizer[词典模式] → RemoveLongFilter(40) → LowerCaser → Stemmer(English) → StopWordFilter[icu 专项 = 全语种停用词] → AsciiFoldingFilter）；ngram 档关 stem/stop 保留 lower/ascii-folding；`lance-tokenizer 14.0.2` 与 `icu_segmenter 2.4.0` 已在锁文件 | lance-index `scalar/inverted/tokenizer.rs`（registry 源码核验）；`Cargo.lock:3756/2847` |
| F-12 | 金标纪律：全合成零版权（D-4）；T9「分词差分断言」先例；`DefaultHasher` 不稳定（`ops.rs:712-713` 自述，生成器禁用）；旗舰套件 env-gated 拒绝静默 skip | `tests/golden_corpus.rs`，`flagship_recall.rs:5-7` |
| F-13 | 仓库对"行为变化无 knob ⇒ 指纹漂移"有明确先例裁决：T12 给 pdf-text 加 `structure = true`（默认开 + 恒序列化） | retrieval-quality-solution §2 T12 |

会话病灶对照（提案 §1）：FM1←F-1、FM2←叙事新维度、FM3←F-7、FM4←跨源诱饵实测、FM5→R5。

## 1. 决策记录

### D-8 R7：查询面分析器与索引内核同管线 + 头锚自校准（两阶段探针）

**D-8.1 trait 扩展**：`LexicalKernel` 增第五方法（ADDITIVE-with-default，dyn-compat 面零破坏
——`Scripted` 第二内核测试先例）：
```rust
/// Tokenization-stable terms of `query` under THIS kernel's analyzer
/// POLICY (the full filter chain, not just segmentation).
fn analyze(&self, tokenizer: FtsTokenizer, query: &str)
    -> Vec<String> { let _ = (tokenizer, query); Vec::new() }
```

**D-8.2 同管线复刻**（评审一发现 2 处置；「同上游即同分词」v1 论断作废——分段级相同不等于
token 级相同，F-11）：`LanceFts::analyze` 直接依赖 **`lance-tokenizer`（= 锁内 14.0.2 同版本
线）**，用其导出的组件组装**同一条六段管线**（icu 档全链；ngram 档按 `apply_ngram_defaults`
的等价策略）；lance 升级若改滤镜链/停用词表，版本线随 lancedb 统一移动，token 级对拍断言
（D-8.5）拦截。若实现期发现 lance-tokenizer 未导出可组装面，退级方案 = 用同版本上游 crate
（icu_segmenter 2.4 / frostem / stop-words）自复刻六段链，**token 级对拍断言不变**（机制
承诺在断言，不在依赖形态）。

**D-8.3 候选重写**：引号整串优先（任意文字，含 CJK）；其余候选 = `analyze()` 术语（去重、
case-fold；ASCII 标识符形状启发式**退役**——icu 词式切分下 camelCase/`vol-32` 形状由 analyzer
天然保留）；纯数字/单字符过滤保留。**降级层（评审二发现 7 处置）**：`analyze()` 返回空
（第二内核未实现）时回退 v1 的 ASCII-run 提取——第二内核缺省行为 = v1 等价，而非弱于 v1。

**D-8.4 头锚 v3 = 查询内 argmin-df，两阶段探针**（评审一发现 1/8 处置）：
- 探针阶段一沿用 cap 64（既有语义，band 分类够用）；**若头位候选 ≥2 个并列饱和于 cap**
  （叙事语料常态：实体 df 全部 ≥64），对并列集做**阶段二精确计数**（limit 8192 的定向探针，
  仅并列者、至多 MAX_PROBES 个）；
- **MUST = df≥1 的最稀有候选**（两阶段后仍并列 ⇒ 首现序，**规范行为**非降级）；df=0 不门
  （门到空集无意义），骑 should；
- argmin 定义域 = 探针集本身（≤ `MAX_PROBES=8`，引号优先、analyzer 术语按首现序截断）——
  **规范语义**，长查询判别实体落第 9+ 位不被探针属既定行为，金标钉长查询用例；
- ceiling=16 **全面退役**：不再做门控资格线、不再做展示带分类器（评审二发现 3 处置——展示带
  会使叙事语料 anchors 成员恒空、与 A-R7 矛盾）。成员语义见 D-8.7；
- **行为变化如实声明**（评审一发现 9）：v2 的「全结构词 ⇒ 零锚 ⇒ 不门」逃生口退役——任何含
  df≥1 候选的查询都会 MUST 门在 argmin 项（可能 df 数百）。风险面与 R1 误判同构（丢词法增强
  非丢结果），金标加结构词查询用例钉住新行为。

**D-8.5 断言**：①token 级对拍——`analyze()` 输出 = 用 lance-tokenizer 组装的索引侧分析器
对同一串的输出（逐 token 相等）；②round-trip——`analyze()` 每个术语对**建好 FTS 索引的表**
（`ensure_fts_index` 进程内，非 flat-scan——评审二发现 8）可命中；icu/ngram 两档各建一次；
③`r1_probe_calibration` v2：叙事金标上 CJK 实体查询头锚 = 期望实体 + 全饱和并列用例（两阶段
路径）+ 长查询截断用例。

**降级语义**：探针失败 → ungated + stderr 注记（v1 契约）；`fts=off` → 词法路不可用，
analyze 不调用；`analyze()` 空 → v1 回退（D-8.3）。

### D-9 R8 索引面：容器章题传播（knob 门控 + 两级退级链）

**D-9.1 指纹载体先行（评审一发现 3 / 评审二发现 1 处置）**：`EpubConfig` 增首个 knob
**`chapter-titles = true`**（默认开 + 恒序列化——F-13 的 T12 先例原样适用：章题解析是行为
变化，无 knob 则指纹漂移、静默混装，正是尾声裁决 2 消灭过的病）。升级 ⇒ epub 路由桶指纹翻转
⇒ `fingerprint_gate` 拒启 ⇒ 强制 reindex 波次（迁移说明如实写：docs 桶受影响源 = 15,428
chunk 的 epub，本地 embedder 代价可忽略）。

**D-9.2 章题溯源两级链**（评审一发现 6 处置——heading 行退级**删除**，其落点跨层且泄漏
行为面到非 epub 容器）：
- **nav/NCX 标签**（epub kind 层）：`parse_opf` 扩读 EPUB3 `properties="nav"` 与 EPUB2
  `spine@toc` 定位目录文档，解析 spine item → 标签映射（quick-xml 0.41 既有依赖，pull 模式
  仿既有代码；`resolve_href` 复用）；
- **child 文件名 stem** 退级（kind 层，零行为歧义）；无 nav/NCX 的 epub 退化为文件名——
  有界退化，如实文档化；
- 非 epub 容器（zip/eml）：`title = None` ⇒ 前缀/stamp 零变化 ⇒ 不涉指纹。
- `ChildDoc` 增 `title: Option<String>`（构造器策略：加带 title 的便捷构造器，27 处调用点
  不动或仅机械补——评审二发现 14 的影响面如实入 T18 工作量）。

**D-9.3 传播链**：walk 层 stamp `Part.title`（与 section 同循环，`ingest/mod.rs:1005-1009`
落点）；`Part` 增 `title: Option<String>`（T12 labels/pages 的 additive-but-explicit 先例，
21 处字面量逐点补）；**两个 kind 的 chunk_run 共用**（`token_window.rs:184-191` 与
`markdown.rs:185-188` 都调 `windowing::context_header`——评审一发现 10/评审二发现 6 处置）：
`context_header` 增 title 分量参数，容器 leaf（`sections` 空且 `title` 在场）用它作章节分量，
flat PDF 路径零变化。

**D-9.4 新元数据键**：`x-vf-section-title`（string；F-7 镜像列自动演化，`$eq/$startsWith`
可滤；键数 11+1=12 ≤ 50）。只在 title 可得时写入。指纹载体 = D-9.1 的 knob（键随提取行为
走，不单独入指纹）。

### D-10 R8 查询面：结构轴三原语

1. **CLI `--section <A[:B]>`**：客户端语法糖 → `x-vf-section` `$gte/$lte`（单值 `$eq`），
   与 `--filter` 经 `$and` 组合；非法形态 fail loud。
2. **CLI `--at <substr>`**（评审一发现 4 / 评审二发现 2 处置）：**要求恰好一个源围栏**
   （`--source` 单值；无源或多源 + `--at` = clap 校验错 fail loud——section 是源内序号
   F-6，裸 `$in` 会跨源串号）。实现：复用 T14 枚举（`enumerate_source_chunks`，**全 metadata
   往返**——ListVectors 无投影，措辞如实；early-exit 有界，15k chunk 实测可忽略）收集
   distinct (section, title)，标题含 substr（大小写不敏感）的 section 集编译为 `$in`，与
   `--source` 的服务端翻译经 `$and` 组合。零命中提示沿 T15 先例。**ADR 0004 自辩**：
   `--at` 是 CLI-only 语法糖，与被否决的「客户端翻译」差异 = 无翻译真值（过滤语义仍由
   服务器过滤编译器裁定）；不承诺 MCP/HTTP 直连等价面。
3. **`vf chunks <src> --sections`**：T14 枚举的折叠视图（distinct section→title 升序）——
   「地图即元数据」的读取面，零服务端变化。
4. **`/x-vf Search` 增 `orderBy: "section-asc"|"section-desc"`**：插在 rerank 之后、hits
   映射之前（`ops_xvf.rs:1813-1815` 之间——配额→截断→rerank→**排序** 的终局视图）；键序
   (section, line-start, score desc, key asc)，**缺失键 nulls-last**（评审发现 11 处置）；
   分数保留展示；与 rerank 可组合（覆盖陈列序，score 列仍是 rerank logit——措辞钉死）。
   未知值 400 `path=orderBy`（searchMode 校验先例）。ranking policy 不入指纹（T11/T12 先例）。
   conformance 面零风险（协议面对拍不覆盖 /x-vf）。CLI `--order` 透传；**MCP 面只加
   `orderBy`**（SearchParams 现落后三代，searchMode 等欠账另立 backlog，不默扩——评审二
   发现 11 处置）。

### D-11 R9：合成叙事金标集（T16）

1. `tests/narrative_corpus.rs`：确定性生成器（固定显式种子；**禁用 `DefaultHasher`** F-12）：
   3 卷 × 60 章；实体表（主角每章、配角/地名/组织中频、一次性实体低频）；**10 个植入事件**
   ——**密度差模型**（评审一发现 5 处置）：唯一出现章含查询词 1–2 次（可词法检索），复述章
   3–8 个、行文含查询词 3–6 次（密度更高——FM2 的忠实结构：场景可命中、排序被淹没）；
   生成器自检：出现章恰一、**复述章 section 号 > 出现章**、密度关系成立；跨源诱饵文档
   （结构词密集）；1 个 Latin 标识符事件（F1 回归守卫）。查询 fixture 与正解同文件固化。
2. 断言面（随 T17/T18 点亮，红→绿）：
   - **A-R7**：CJK 实体/事件查询 anchors 非空、头锚 = 期望实体；结构词查询的**新恒门行为**
     被钉住；雷达起源查询回归不变；
   - **A-R8 拆两半**：召回半——出现章在**未门控 keyword top-k 内**；位置半——orderBy/
     section 序后首中 = 出现章（复述章不冒名）；
   - **A-FM4 降级为观测断言**（评审二处置：FM4 机制本设计延后，经验缓解不作硬门）；
   - **A-差分**：D-8.5 全部。
3. INDEXED 全链门 **env-gated**（真模型，旗舰套件纪律）；真实语料不入 CI——8 条会话查询 +
   正解锚点固化为手动回放清单（附 `research/`，T18 过门项）。

## 2. 任务分解

### T16 R9 金标集（先行）
生成器 + fixture + 断言骨架（机制断言先红）+ env-gated 全链 INDEXED 门。
测试门：确定性（同种子同产物）、事件结构自检（恰一/序关系/密度关系）、诱饵与 Latin 守卫。

### T17 R7 分析器同管线
`lance-tokenizer` 直接依赖 + `analyze()` + 候选重写（含 v1 回退层）+ 头锚 v3 两阶段探针 +
anchors 成员新语义 + token 对拍/round-trip 断言 + calibration v2。
**随手术更新的既有测试（预期红→绿，非遗漏）**：`lexical.rs:421-458` 候选单测（含 :449 纯
CJK 空断言）、`ops_xvf.rs:4946-4959` e2e 零锚断言、`flagship_recall.rs:255-260`——行为变更
清单在提交说明列明。测试门：A-R7 + A-差分 + 起源查询 e2e 头锚不变（vibe df=1 仍为头锚）。

### T18 R8 结构轴
`chapter-titles` knob（指纹载体）+ nav/NCX 解析 + `ChildDoc.title`/`Part.title` 手术 +
CCH 两 kind 分量 + `x-vf-section-title` stamp + `--section`/`--at`/`vf chunks --sections`/
`orderBy`（/x-vf + CLI + MCP[仅 orderBy]）+ 迁移说明（指纹翻转、reindex 波次）+ 手动回放
清单 + **部署侧同步**（重装 `vf` 二进制与 skill 副本——F-9 教训；mcp.rs searchMode 欠账立
backlog 条目）。
测试门：A-R8 两半 + 迁移一致性（reindex 前后 chunk 集断言，T13 先例）+ 手动回放过门
（8 查询头锚非空 ×8；「灰雾 vs 魔药先后」各一条 orderBy 查询判出）。

每任务一提交、主会话复跑验收（K0 标准作业）。

## 3. 与已交付面的交互条款

- **rerank / kind 排除 / per-source 配额 / orderBy**：ranking policy 族，不入指纹；orderBy
  作用序 = 配额→截断→rerank→排序（只改陈列不改入选）；
- **`vectorfs optimize`**：analyze() 纯查询面零物理变化；title stamp 走 knob 翻转的 reindex
  不走 optimize（T13 分工）；
- **T14 `vf chunks`**：`--at` 与 `--sections` 的数据源（early-exit 复用）；
- **ADR 0004**：`--section` 纯机械编译；`--at` 的 CLI-only 自辩句已入 D-10.2；
- **ADR 0006**：knob 落在 epub 的 EpubConfig——epub 现为归文档默认路由，指纹翻转覆盖面即
  默认行为面，迁移说明按此措辞。

## 4. 实施序与守门

**序**：T16 → T17 → T18（尺子→疗法→面）；T17 依赖 T16 语料（calibration v2），差分断言
可先行。**T18 的 knob 裁决已前置进本设计**（评审二建议采纳：最脆弱环节最先钉死）。
**守门常数**：头锚 = 两阶段后 argmin（仍并列取首现序，规范行为）；df=0 不门；探针集 =
引号优先 + analyzer 首现序前 8；title 链 = nav/NCX > 文件名（heading 级已删）；orderBy 键序
(section, line-start, score desc, key asc) + nulls-last；`--at` 恰一源。

## 5. 诚实声明（边界与撤回）

1. **v1 撤回三处**：①「同上游即同分词」（分段级 ≠ token 级，F-11 六段管线）→ 同管线复刻 +
   token 对拍；②「新键+前缀 = 指纹变更」（指纹只哈希配置，F-10）→ knob 载体；③「差分断言
   拦版本漂移」（查询级 round-trip 拦不住滤镜级）→ token 级对拍；
2. **裁决收窄注记**：提案裁决「分位数自校准 + 常数兜底」在本设计细化为 **argmin（0 分位）+
   v1 降级语义**——最小值是分位数的退化形态，属合法细化，回填提案修订记录；
3. argmin-df 头锚无产品级先例（R1 同一诚实等级）：内部证据 = FM1 直证 + v2 裁决自然推广 +
   T16 金标可证伪（含全饱和并列用例）；两阶段后 df > 8192 的残余并列 = 首现序，如实声明；
4. `--at` v1 字面匹配 + 恰一源；卷级弧段围栏两步走（`--sections` 查界 + `--section` 显式）；
5. **FM2 排序病不治**（R8 立场 = 绕过：验证首次出现一条查询判出）；FM4 机制延后（A-FM4
   降观测）；FM5 归 R5；
6. 无 nav/NCX 的 epub 退化为文件名章题——有界退化，非错误。

## 6. 评审记录（2026-11-11，双路独立 + 主会话交叉验证）

**评审一（机制与证据链，13 项发现）**：blocker 1（探针 cap=64 使 argmin 在目标语料退化为
首现序——动机数字全部落入盲区）、major 4（同管线论断 / 指纹载体 / --at 跨源 / 金标低重叠
自矛盾）、major 2（退级链落点 / F-9 失实）、minor 6。
**评审二（可实施性与契约面， 15 项发现）**：major 3（指纹无实现路径 / --at 源范围 /
anchors 展示带矛盾）、minor 12（含 BUG①②已交付、Part/ChildDoc 影响面、orderBy 裁决缺
两条、MCP 面欠账、测试预期红清单、ADR 0004 方向自辩）。
**交叉撞点**（两路独立同判，可信度最高）：指纹载体缺失、--at 跨源串号、anchors 成员语义、
BUG①②已交付、Part/两 kind 手术面。
**主会话核验**（五项争议断言亲自对码）：probe cap（`lexical.rs:104-109` 坐实）、
lance-tokenizer/icu_segmenter 锁内版本（`Cargo.lock:3756/2847` 坐实）、anchors 透传在场
（`vf.rs:1112-1114` 坐实——会话用了旧二进制）、仓库 skill 已新（`skills/vf-search/SKILL.md`
坐实——过期的是安装副本）、指纹仅哈希配置（`config.rs:1322+` 坐实）。
**处置**：全部 blocker/major 已吸收进 v2（D-8.2/8.4/8.7、D-9.1/9.2、D-10.2/10.4、D-11.1，
§0 F-3/F-9/F-11 修订，任务清单与守门常数同步）；minor 逐项落入对应条款；两处 v1 事实失实
（F-9）如实留痕。**结论：裁决关闭，可进入实施。**
