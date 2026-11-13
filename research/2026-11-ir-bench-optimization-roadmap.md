# 外部基准驱动的下一轮优化提案（2026-11-13）

> 来源：`2026-11-external-ir-bench.md` 首跑成绩（BEIR SciFact/NFCorpus + MLDR-zh dev 采样，四臂）+
> 基准过程暴露的 5 条产品观察。本文收敛为机制路线图供裁决，格式沿用
> `2026-11-recall-quality-optimization.md`（R1-R6）先例：每项给疗效证据 / 成本 / 风险 / 诚实声明。
> **各提案项已分案入 `docs/proposals/`**（proposed 状态，待裁决）：
> B0 `2026-11-external-ir-gold-harness.md` ｜ B1 `2026-11-en-keyword-pathology-spike.md` ｜
> B2 `2026-11-query-face-knob-sweep.md` ｜ B3 `2026-11-en-analyzer-stemming.md`（立项门=B1-P1）｜
> B4 `2026-11-semantic-dilution-rulers.md` ｜ B5 `2026-11-bench-guardrails.md`。

## 0. 证据底座（本提案与 R1-R6 轮的本质差异）

R1-R6 的证据是**仓内合成金标 + 雷达语料九实验**；本轮多了一个可重跑的**外部公允金标资产**：
`/root/bench-ir/`（三基准语料 + 金标 + 逐查询原始排名 + runner/scorer 脚本，daemon 一条命令可复活）。
按 R6 教义（Qdrant 决策表：无评测集不调参），**以下一切调参类提案都以这套资产为证据门**——
每个 knob 改动前后在同语料上重跑四臂，数字说话。

四条已被外部数据钉死的事实（提案的全部依据）：

| # | 事实 | 数字 |
|---|---|---|
| F1 | 英文 keyword 臂距 Anserini BM25 差距大 | SciFact **-20.2**（0.444 vs 0.646）/ NFCorpus -5.3（0.272 vs 0.325） |
| F2 | 语义臂切块稀释：召回在、排位被稀释 | SciFact R@100 0.933 但 nDCG@10 0.660（论文整文档 0.734）；NFCorpus 同形（0.858/0.338/0.367） |
| F3 | zh 域 rerank 增益最大且候选深度封顶 | mldrzh rerank@depth10 0.833 但 R@100 0.865 < hybrid 0.920（候选池里有货没被重排） |
| F4 | zh 词法臂是强臂、hybrid 双强下兑现 RRF | mldrzh keyword 0.730 > semantic 0.695；hybrid 0.769；BEIR 上弱臂拉低强臂（SciFact 0.660→0.627） |

## 1. 提案（按 证据强度×成本 排序）

### B1 英文 keyword 病灶解剖 spike——一切词法修复的前置证据门

- **机制**：在保留的 BEIR 语料上做三组受控探针，把 F1 的 -20 点拆解到机制：
  ① **词干化探针**：取 20 条查询的屈折词（developments/properties/cells），手工还原为基础形重查
  keyword 臂——若 nDCG 显著回升 ⇒ 缺英文 stemming 是主因；
  ② **长度归一探针**：同一查询在「chunk 级 BM25」vs「把 doc 全文拼成单 chunk 重嵌对照桶」对照
  （一个 24.5k chunk 桶，重嵌 ~25min）——若回升 ⇒ chunk 级 b 参数/长度归一是主因；
  ③ **title 加权探针**：Anserini 的 SciFact 优势配置含字段化索引；量一下正解 doc 的 title 命中占比。
- **证据**：F1；mGTE 论文 zh 列证明 icu 词典分词对 CJK 及格（zh keyword 是强臂）——**病灶是英文特有**，
  修复必须带 zh 不回归断言。
- **成本**：①③ 纯查询面几小时；② 一次 25min 小桶重嵌。零产品代码。
- **产出**：机制判决书 + B3/B4 的路由（查询面可修 or 必须指纹翻转）。

### B2 查询面红利收割：rerank-depth 曲线 + fusion_overfetch + merge-weights——零重嵌，先跑先得

- **机制**：三个**已落地、指纹外**的查询面 knob（R3a `--rerank-depth`、R3b `fusion_overfetch`、
  `merge-weights`）全部跑外部金标曲线：
  - rerank-depth {10, 20, 50} × 三基准——F3 的 0.92-0.865=5.5 点 R@100 缺口是现成的上探空间；
  - fusion_overfetch {1, 2, 4}——F4 的「弱臂稀释」是否被 over-fetch 缓解（SciFact hybrid 0.627 的回升空间）；
  - merge-weights 若暴露静态权重档，测 dense:lexical 加权 RRF vs 等权——**只采纳静态测得值，不做自动调权**
    （三集上自动调权=过拟合脚枪）。
- **成本**：~2-3h 基准机时（148ms/pair 线性预算：depth 50 × 823 查询 ≈ 1.7h），零产品风险、零重嵌。
- **预期（待测假设，不背书）**：mldrzh depth 20 若吃下缺口一半，nDCG 0.833→0.85+；SciFact hybrid
  over-fetch 若 +2 点则「hybrid 推荐」在英文域也站住。
- **落点**：测得值直接进文档配方（「质量敏感路径 depth 20 ≈ 3s」），不动默认值除非曲线陡。

### B3 英文 analyzer（stemming/停用词）——索引面，指纹翻转预算，B1 判决后立项

- **机制**：lance-tokenizer 14.0.2 的 icu 链加英文处理变体（Porter/停用词），作为 `fts-tokenizer`
  新枚举值——按 ADR 0002 进指纹，换分词 = reindex 波次（T18 先例：docs 桶 37k chunks ≈2h）。
- **路由条件**：仅当 B1① 判定 stemming 主导（若长度归一/title 主导则本项不立项，转 B4）。
- **风险**：zh 不回归（枚举值只对 Latin 词形做还原，CJK 路径零变化，断言进测试）；fingerprint 翻转
  波次的迁移纪律照 T18 模板（override compose 片段 + 指纹门拒启 + serve --reindex）。
- **预期**：SciFact keyword 0.444 → Anserini 0.646 之间取值，保守 +8~12 点（Anserini 还有字段化加成）。

### B4 语义臂长文稀释的两个量尺——ablation 先于机制

- **量尺一（doc-oracle）**：rerank-depth 100 于三基准（B2 顺带产出）= 候选池排位上限——直接量化
  「F2 缺口里多少是首段排位问题 vs 候选池根本没有」。
- **量尺二（chunk 尺寸轴）**：scifact 桶一次 `token-window 1024/128` 重嵌（~25min）跑四臂——量化
  切块粒度对 F2 的贡献。**只量不改**：默认 512/64 是回源定位的产品契约（T13 坐标粒度），
  改默认需单独裁决 + 全量重嵌波次，本轮只拿数字。
- **诚实声明**：gte 支持 8192 ctx，论文口径就是整文档——但整文档嵌入违背「命中回源到 chunk」的
  产品语义（明确不做，见 §3）。

### B5 产品护栏四件套（非检索质量，基准过程实录的缺陷）

| 项 | 观察（external-ir-bench §5） | 护栏 |
|---|---|---|
| 调度器活性 | 批上传撞在飞 scan → 5k 标记 strike 后冻结，重启才愈 | 「PENDING+文件在盘=N 且 worker 空闲 K tick」活性断言测试 |
| SIGTERM 挂死 | 在飞嵌入时 drain 37min 不退（承诺 ≤8s） | drain 看门狗：超时告警+强退；空闲态 3s 退（已实录）进回归测试 |
| `--no-text` | metadata 仍带 chunk 全文（top-100 ≈100KB/查） | no-text 覆盖 `x-vf-chunk-text` 或加 `--slim` 档 |
| env 回落脚枪 | 本轮实录：漏 env 打到 8787 生产实例（400 被拒零污染） | 「有 endpoint 无凭据」显式报错或 `--no-config`（§6.3 老账） |

### B0 外部金标资产固化（enabling，半天的工程）

- `/root/bench-ir/scripts` 五个脚本 + 数据源清单搬进仓（建议 `research/bench-ir/` + README
  复现口径），`*_scores.json` 纳入版本管理作为**回归基线**——B2/B3/B4 的每张曲线都以此为 before/after。
- daemon 复活命令与四臂口径写死在 README（口径即仪器：title 行物化、chunk→doc 首现折叠、
  线性增益 nDCG、rerank 臂候选=topK）。

## 2. 建议落地序（裁决请求）

| 序 | 项 | 成本 | 外部证据指向 | 阻塞关系 |
|---|---|---|---|---|
| 0 | B0 金标固化 | 半天工程 | — | B1-B4 的前置 |
| 1 | B1 病灶 spike | 数小时+25min 重嵌 | F1 | 路由 B3/B4 |
| 2 | B2 查询面曲线 | 2-3h 机时，零风险 | F3/F4 | 无 |
| 3 | B4 两个量尺 | 25min 重嵌 + B2 顺带 | F2 | 无 |
| 4 | B3 英文 analyzer | spike + 指纹翻转波次 | F1（B1 判决后） | 依赖 B1① |
| 5 | B5 护栏四件套 | 各半天内 | §5 观察 | 无（随时插队） |

B2/B4 可与 B1 并行（互不依赖）；B3 是唯一需要迁移波次的项，立项与否由 B1 判决书决定。

## 3. 明确不做

- **整文档 8192 嵌入追平论文口径**：违背 chunk 回源产品契约；F2 的缺口用 B4 量尺说话，不偷换粒度。
- **自动/在线调权的 RRF 或路由器**：无先例 + 三集过拟合风险（R1 轮裁决沿用）；只取静态测得值。
- **LLM 任何环节**（零 LLM 纪律不变）。
- **改默认 chunk 尺寸**：T13 坐标粒度是契约；1024/128 只作 ablation 数字。
- **为 MLDR-zh 采样集的绝对分做任何优化**：采样口径只许四臂内部对照（external-ir-bench §3 红线）——
  防止「调参对着采样语料的缺位负例过拟合」。

## 4. 预期总账（全部为待测假设，B0 跑完即验）

| 轴 | 现状 | 一轮后可辩护的目标 |
|---|---|---|
| BEIR keyword | 0.444 / 0.272 | 主因修复后向 Anserini（0.646/0.325）收敛过半 |
| BEIR hybrid | 0.627 / 0.345 | over-fetch/加权静态值 +2~4 点（追平或超过 semantic 臂） |
| MLDR-zh rerank | 0.833 @depth10 | depth 曲线后 0.85+（R@100 缺口 5.5 点的回收） |
| 回归纪律 | 仓内金标 | 外部金标基线进仓，每 knob 曲线 before/after 留痕 |
