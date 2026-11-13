# 提案：外部 IR 金标资产固化（B0）——外部基准成为常驻证据门

> 状态：**proposed**（2026-11-13）。
> 来源：`research/2026-11-external-ir-bench.md` 首跑（BEIR SciFact/NFCorpus + MLDR-zh dev 采样
> 四臂成绩）+ `research/2026-11-ir-bench-optimization-roadmap.md` B0 项。
> 定位：**enabling**——B1-B4 一切调参/机制提案的前置；无它，后续每条曲线都是一次性观测。

## 1. 问题

R6 教义（Qdrant 决策表：「无评测集 → 手调权重不太可能稳定胜过默认」）在仓内的兑现物是合成
金标（T16 叙事集、雷达九实验）。2026-11-12 首跑证明**外部公允金标可以低成本常驻**：三基准
数据 + 物化/采样/runner/scorer 五脚本 + 逐查询原始排名全部落盘于 `/root/bench-ir/`，但它是
会话工件——机器上无版本管理、无复现 README、无基线锚定。下一次 knob 改动（B2-B4 任何一项）
若无同口径 before/after，疗效主张不可辩护。

## 2. 目标 / 非目标

**目标**
1. 基准脚本进仓（落点建议 `research/bench-ir/`，与数据源清单、复现口径 README 同目录）：
   `materialize_beir.py` / `sample_mldr.py` / `run_queries.py` / `score.py` / `wait_mldrzh.sh`；
2. 三基准基线分（`*_scores.json` + `*_latency.json`）进版本管理作为**回归基线**——后续任何
   检索面改动的 CI 语义是「与基线的差值可解释」；
3. README 钉死口径（口径即仪器）：title 行物化、chunk→doc 首现折叠、线性增益 nDCG
   （pytrec_eval 同式）、rerank 臂候选=topK、`anchor_gating`/`fusion_overfetch` 默认态声明、
   MLDR-zh 采样协议（全 qrels 正解 + seeded 填充，SEED 固定）与「绝对值不可对外比」红线。

**非目标**
- 不把语料本体（约 2GB）进仓——README 给数据源 URL + sha256 + 物化命令；
- 不建自动化 CI job（首轮手工复现即可；频率待 B2 跑完后按迭代节奏裁决）；
- 不扩基准集（ArguAna/Touche 等泛化验证是独立后续项，见 roadmap §7）。

## 3. 方案选项

| 选项 | 裁决 |
|---|---|
| **A. `research/bench-ir/`（研究工件区，与 m3-bench 复现附录同族）** | **采**——研究性质、非产品面，research/ 先例（m3-bench.md §8 已有全量复现附录文化） |
| B. `scripts/bench-ir/`（与 cli_smoke.sh 同级，CI 化暗示强） | 否——CI 化超出本轮目标，先落工件再谈自动化 |
| C. 独立仓 | 否——与 HEAD 二进制强耦合（查询面行为即被测物），同仓才能锚定版本 |

## 4. 关键取舍

1. **基线分进仓 ≠ 数字承诺**：README 明示基线锚定「HEAD 501c83c + 本口径」双条件，口径漂移
   （如 rerank 臂改 depth）须重跑全臂而非只跑变动臂；
2. **评分器自证**：score.py 的 nDCG 手算抽查记录（qid 3 rank-5 → 0.3869 = 1/log₂6）写进
   README，防后来者重写评分器时无声漂移；
3. **MLDR-zh 采样红线成文**：绝对分只许四臂内部对照——防「对着缺位负例调参过拟合」。

## 5. 依赖与风险

- 依赖：无产品代码改动；纯搬移 + 文档；
- 风险：①脚本含本机绝对路径（/root/bench-ir）——参数化后进仓；②hf-mirror 大 LFS 单连接
  限速（~170KB/s）——README 记 12 路分段并行方案（首跑实测 2 分钟）；③qrels tsv 镜像 503
  ——dev.jsonl 内联 positives 等价构造已验证，作为官方路径写死。
