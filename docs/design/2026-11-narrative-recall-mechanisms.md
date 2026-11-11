# 设计：叙事语料召回缺口三机制（R7/R8/R9，任务 T16–T18）

> 状态：**proposed**（2026-11-11）。提案：`docs/proposals/2026-11-narrative-recall-mechanisms.md`。
> 谱系：`research/2026-11-recall-quality-optimization.md` R1–R6 延长线；任务编号续
> retrieval-quality-solution 的 T12–T15。
> 评审记录：占位——独立评审与交叉验证结论回填于 §6。

## 0. 事实基线（2026-11-11 会话 + 源码核验）

| # | 事实 | 锚点 |
|---|---|---|
| F-1 | 锚词候选要求含 ASCII 字母，纯 CJK 永不入选；引号整串同样被拦 | `lexical.rs:231`（`push` 闭包），`:22-24` 模块文档自述 |
| F-2 | 锚词机制三件套：df 探针（count，cap 64）、单头锚 MUST（v2 裁决）、ceiling=16 雷达标定 | `lexical.rs:186/192/200/207`，`select_anchors` v2 裁决 |
| F-3 | `LexicalKernel` trait 现三方法（search / search_gated / probe_df），扩展形态 = ADDITIVE-with-default | `lexical.rs:40-60`，模块文档 :30-34 |
| F-4 | 桶→tokenizer 映射在查询期可解析（`[ingest.X] fts-tokenizer`，daemon 可读；optimize 侧同源） | `optimize.rs:134-135`，preset.rs 注释 |
| F-5 | CCH 前缀 = 文件名 stem >（flat 文档的）最近前置 TOC 条目；容器子 part 无 TOC 页 ⇒ `Part.sections` 空 ⇒ 前缀退化为常量 stem | `windowing.rs:114-132`，`chunk/mod.rs:115-119`（`ChunkCtx` 仅 `source_stem`） |
| F-6 | 容器 walk 给 child part stamp `section`（child 序）；epub spine 顺序 = 章节顺序 ⇒ **section 即章节轴** | `token_window.rs:203`，`extract/epub.rs` spine_children |
| F-7 | T13 坐标键（page×2/line×2/section）+ kind 全 filterable；`x-vf-section` 范围过滤实测生效 | 实测 `--filter '{"x-vf-section":{"$lte":20}}'`；retrieval-quality-solution §2 T13 |
| F-8 | `/x-vf/Search` 已有 searchMode（semantic/keyword/hybrid）+ includeKinds + anchors 响应成员；CLI 已有 `--mode/--rerank/--rerank-depth/--per-source/--kind/--source` | `ops_xvf.rs:137-139/1071-1074/1220-1230/1273-1281`；`client/src/bin/vf.rs:235-278` |
| F-9 | skill 文档与实现脱节：过滤句失实 + 七旗标未记载（T15.4 义务未执行）；`anchors` 未透传 CLI `--json` | `/root/.zcode/skills/vf-search/SKILL.md`；会话实测 `--json` 无 anchors |
| F-10 | 指纹门拒启 ⇒ 指纹变更 = reindex 语义；ranking policy（rerank/kind 排除/配额）不入指纹先例 | `ingest/mod.rs:228-241`；T11/T12 先例 |
| F-11 | 金标纪律：全合成零版权（D-4）；T9 有「分词差分断言」先例可挂 | `tests/golden_corpus.rs`，retrieval-quality-solution §1 D-2/D-3/D-4 |

会话病灶对照（proposal §1）：FM1←F-1、FM2←叙事语料新维度、FM3←F-7（面在而轴不成）、
FM4←跨源诱饵实测（「廷根 恶灵 案件」命中《大模型基础》p14，rank 2）、FM5（分数带压缩）→
R5 模型轴的叙事场景列，不在本设计内。

## 1. 决策记录

### D-8 R7：查询面分析器与索引内核同源 + 头锚自校准

**机制**
1. `LexicalKernel` 增第四方法（ADDITIVE-with-default，缺省实现 = 返回空 vec = 第二内核零改动
   编译、行为同现状）：
   ```rust
   /// Tokenization-stable terms of `query` under THIS kernel's analyzer
   /// policy — the same upstream the index's FTS tokenizer derives from.
   fn analyze(&self, tokenizer: FtsTokenizer, query: &str)
       -> Vec<String> { let _ = (tokenizer, query); Vec::new() }
   ```
2. `LanceFts` 实现按 `FtsTokenizer` 分派（tokenizer 由查询面从桶配置解析，解析序
   config > 默认 `icu`——与 `optimize`/指纹同源，F-4）：
   - `Icu` → `icu_segmenter`（**与 lancedb 锁定版本线对齐**进 server 依赖树；lance 的 icu
     tokenizer 即此 crate 的封装——同上游即同分词，稳定性由构造成立）；
   - `Ngram(2,2)` → 内部纯函数（全滑窗 bigram，~30 行，策略是纯函数无外部状态）。
3. `anchor_candidates` 重写：**引号整串优先**（任意文字，含 CJK——「用户主张判别性」语义
   对文字本就不该有偏置）；其余候选 = `analyze()` 术语（去重、case-fold；ASCII 标识符形状
   启发式退役——icu 对 Latin 词式切分、camelCase 整词保留，v1 想保的形状由 analyzer 天然
   给出）。纯数字/单字符过滤保留（语料结构噪声，v1 理由仍成立）。
4. **头锚 v3 = 查询内 argmin-df**：对候选（≤ `MAX_PROBES` 个，按探针预算）逐一 df 探针，
   **MUST = df ≥ 1 的最稀有候选**（并列取首现序）。ceiling=16 降级为「anchor 展示带」分类
   器与探针优先序提示，**不再是门控资格线**。理由（会话病根直证）：叙事语料判别词 df 整体
   高位（克莱恩 df≈15k、廷根≈2k、赞格威尔≈百级），绝对阈值下实体查询永远零锚——而 argmin
   语义恰是 v2「single rarest anchor」裁决的自然推广（改的只是「最稀有」的求法：绝对带内取
   最小 → 查询内取最小）。df=0 候选不入 MUST（门到空集无意义），仍骑 should。
5. 差分断言（挂 T9 分词差分先例）：`analyze()` 每个术语对实际 FTS 索引 round-trip 可命中
   （造词 corpus 上断言）；版本漂移致分词漂移 = 测试红。`r1_probe_calibration` 扩 v2：叙事
   语料（T16 金标）上断言 CJK 实体查询 anchors 非空且头锚 = 期望实体。

**降级语义**（沿 v1 契约）：探针失败 → ungated + stderr 注记；`fts = off` 桶 → 词法路本就
不可用，analyze 不调用；`analyze` 返回空（未来第二内核未实现）→ v1 等价行为（无 CJK 锚，
Latin 引号串仍可用）。

### D-9 R8 索引面：容器章题传播（CCH 容器补全 + section-title 键）

**机制**
1. `ChildDoc` 增 `title: Option<String>`（additive）。`spine_children` 解析目录标签：
   **EPUB3 nav.xhtml / EPUB2 toc.ncx 的 spine item 标签**优先；退级：child 文本首个
   heading 形态行（html 提取后的首个非空行，≤80 字符且非正文句式）；再退级：child 文件名
   stem。有界退化（无 nav 无 heading 的 epub 退化为文件名），行为如实文档化。
2. 容器 walk stamp 新元数据键 **`x-vf-section-title`**（string，镜像列进 pushdown——
   `$eq/$startsWith` 可滤，T13 坐标键同批先例；键数核算 +1，远离 AWS 50 上限）。只在
   title 可得时写入（与 T13「单位存在才写」同哲学）。
3. CCH 容器补全：`context_header` 的章节分量，容器 leaf part（`sections` 空）用
   `part.title`；flat PDF 路径（nearest-preceding-TOC-entry）**零变化**。手术落点：
   `token_window` 建 `Chunk` 时把 part-level title 传入（`ChunkCtx` 是 source 级，title 是
   part 级——经 dispatch 的既有 part 循环传参，不扩 `ChunkCtx`）。
4. **迁移语义**：新元数据键 + 嵌入前缀变化 = 指纹变更 ⇒ reindex（一次波次合并两处代价，
   尾声裁决 2 先例措辞）；docs 桶受影响源 = 15,428 chunk 的 epub（本地 embedder，代价可忽略，
   如实入迁移说明）。

### D-10 R8 查询面：结构轴三原语

1. **CLI `--section <A[:B]>`**：客户端语法糖，编译为 `x-vf-section` 的 `$gte/$lte`（单值
   = `$eq`），与 `--filter` 经 `$and` 组合（`--source` 先例：客户端拼写，真值在服务器，
   ADR 0004 措辞）。非法形态（非正整数/A>B）= clap/校验错 fail loud。
2. **CLI `--at <substr>`**：客户端解析——经 T14 枚举机制（ListVectors 前缀 + early-exit，
   `--no-text` 等价面）收集 distinct (section, section-title)，**标题含 substr（大小写
   不敏感）的 section 集**编译为 `$in` 过滤。**v1 语义如实声明**：字面匹配，不做「卷→
   范围」扩展（过度聪明；弧段围栏 = `vf chunks --sections` 查界 + `--section` 显式范围）。
   零命中且含 `--at` 时提示「title 未匹配任何 section」（T15 空命中提示先例）。
3. **`vf chunks <src> --sections`**：T14 枚举动词的折叠视图（distinct section→title 按
   section 升序）——**TOC 地图即元数据**的读取面，零服务端变化。
4. **`/x-vf Search` 增 `orderBy: "section-asc" | "section-desc"`**：终局命中集（配额/
   截断之后）后置排序，键序 (section, line-start, score desc, key asc)；分数保留展示。
   **措辞钉死「扫描序视图」**——不改变任何路的排序语义，语义排序期望由 rerank 面
   承担。未知值 400 `path=orderBy`（searchMode 校验 `ops_xvf.rs:1273-1281` 先例）。
   ranking policy 不入指纹（T11/T12 先例，F-10）。CLI `--order section[:asc|desc]` 透传。

### D-11 R9：合成叙事金标集（T16）

1. `tests/narrative_corpus.rs`：**确定性生成器**（固定种子，全合成，D-4 纪律）：3 卷 ×
   60 章；实体表（主角高频 ~每章、配角/地名/组织中频、一次性实体低频）；**10 个植入事件**
   ——每个事件有唯一出现章（行文刻意换词，与自然查询低词面重叠）+ 3–8 个复述章（复述行文
   含查询词更密——FM2 的结构化复刻）；**跨源诱饵文档**（结构词密集，FM4 复刻）；1 个
   Latin 标识符事件（F1 场景回归守卫）。查询 fixture（含正解 section）与生成器同文件固化。
2. 断言面（随 T17/T18 逐项点亮，红→绿纪律）：
   - **A-R7**：CJK 实体/事件查询 `anchors` 非空、头锚 = 期望实体；门控后诱饵/洪泛块出局
     词法路；雷达起源查询回归不变（现有 flagship 套件全绿）；
   - **A-R8**：事件定位（keyword + `--section`/orderBy）首中 = 唯一出现章，复述章不冒名；
   - **A-FM4**：hybrid 下跨源诱饵不进 top-k；
   - **A-差分**：D-8.5 round-trip 断言。
3. **真实语料不入 CI**（版权纪律）：《诡秘之主》8 条会话查询 + 已知正解（§15 加入值夜者、
   §102 小丑线索占卜、§210–214 大战、§476 复述陷阱段等）固化为**手动回放清单**附于
   `research/`（T18 过门项）。

## 2. 任务分解

### T16 R9 金标集（先行——尺子先于疗法）
生成器 + fixture + 断言骨架（机制断言先红）。测试门：生成器确定性（同种子同产物）、
事件唯一性（出现章恰一、复述章行文密度 > 出现章——生成器自检断言）、金标 corpus 经
ingest 全链 INDEXED。

### T17 R7 分析器同源
trait `analyze()` + icu/ngram 双实现 + `anchor_candidates` 重写 + 头锚 v3 + 差分断言 +
calibration v2 + **BUG②**（anchors CLI `--json` 透传，缺则补线）。
测试门：A-R7 + A-差分 + flagship 回归全绿；纯 CJK 查询不再零锚（金标断言由红转绿）。

### T18 R8 结构轴
NCX/nav 解析 + `ChildDoc.title` + walk stamp `x-vf-section-title` + 镜像列 + CCH 容器
分量 + `--section`/`--at`/`vf chunks --sections`/`orderBy`(+CLI 透传) + **BUG①**
（vf-search skill 过滤句与七旗标补记载；mcp.rs 描述同步——T15.4 义务清偿）+ 手动回放清单。
测试门：A-R8 + A-FM4 + 迁移语义（reindex 前后 chunk 集一致性断言，T13 先例）+ 手动回放
过门（8 查询：FM1 锚非空 ×8；「灰雾 vs 魔药先后」经 orderBy 各一条查询判出）。

每任务一提交、主会话复跑验收（K0 标准作业）。

## 3. 与已交付面的交互条款

- **rerank / kind 排除 / per-source 配额**：orderBy 同类——ranking policy 不入指纹
  （T11/T12/T9 先例链）；orderBy 作用于终局集，与配额/截断的先后 = 配额→截断→排序
  （排序不改变「谁入选」，只改变「怎么陈列」）；
- **`vectorfs optimize`**：analyze() 纯查询面，FTS 索引物理层零变化；D-9 的 title stamp
  走 reindex 不走 optimize（T13 分工措辞）；
- **T14 `vf chunks`**：`--at` 与 `--sections` 的数据源（early-exit 前缀枚举复用）；
- **ADR 0004**：`--section` 语法糖的「客户端拼写、服务器真值」措辞义务；
- **指纹**：T18 一批 reindex 合并 D-9 两处代价（尾声裁决 2 波次合并先例）。

## 4. 实施序与守门

**序**：T16 → T17 → T18（尺子→疗法→面）。
**守门常数**（机制常数，非配置）：头锚 = df≥1 的 argmin（并列取首现序）；df=0 不门；
探针预算 MAX_PROBES=8 不变；title 退级链 nav > 首行 > 文件名；orderBy 键序
(section, line-start, score desc, key asc)；`--at` 字面匹配不扩展。

## 5. 诚实声明（边界与撤回）

1. argmin-df 头锚**无产品级外部先例**（与 R1 的 df 阈值同一诚实等级）——内部证据：会话
   FM1 直证 + v2 裁决的自然推广 + T16 金标可证伪；
2. `--at` v1 是字面匹配，卷级弧段围栏需两步（查界 + 显式范围）——不做过聪明的范围推断；
3. **FM2 的排序病不治**（复述段仍可能排在场景段前）——R8 的立场是绕过：验证首次出现变成
   一条位置序查询；正面治疗超出零 LLM 纪律可达域；
4. FM4 融合准入规则**延后**（proposal §3）——待 R9 数据地基就绪独立裁决；
5. FM5 分数带压缩不在本设计（R5 模型轴排期不变，R9 为其备好叙事场景列）。

## 6. 评审记录（占位，评审后回填）

- 独立评审一（机制与证据链）：待回填
- 独立评审二（可行性与契约面）：待回填
- 交叉验证与处置：待回填
