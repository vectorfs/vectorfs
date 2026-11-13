# 外部公允基准首跑：BEIR（SciFact/NFCorpus）+ MLDR-zh dev 采样——vectorfs 四臂检索质量

- 日期：2026-11-12。被测代码 = HEAD `501c83c`（含 R7/R8/R9 查询面：锚词门控 v3 / CCH 章题 / server orderBy），`cargo build --release`。
- 定位：**首次用外部公认基准给 vectorfs 检索质量打分**——此前质量口径只有仓内合成金标（T16 叙事集）与雷达语料九实验。本文 = BEIR 两小集（国际标准，有公开对照数字）+ MLDR-zh dev 采样（中文长文，贴生产域），四臂对照（semantic / keyword / hybrid / hybrid+rerank）。
- 环境：与 M3 bench 同机（WSL2，i5-14600KF 20 逻辑核，31 GiB RAM）；gte-multilingual-base（暖缓存）+ gte-multilingual-reranker-base（镜像预取）；onnxruntime 1.31.0 CPU EP；file 后端；daemon 端口 8788 与本机生产实例（8787）完全隔离（每次 vf 调用显式 VF_* env + 假 HOME + 空 XDG_CONFIG_HOME，§6.3 反制）。

## 0. 结论速览

| 基准（nDCG@10） | semantic | keyword | hybrid | +rerank | 公开锚点 |
|---|---|---|---|---|---|
| BEIR SciFact (300q) | 0.660 | 0.444 | 0.627 | **0.686** | mGTE dense 0.734 / Anserini BM25 0.646 |
| BEIR NFCorpus (323q) | 0.338 | 0.272 | **0.345** | 0.319 | mGTE dense 0.367 / Anserini BM25 0.325 |
| MLDR-zh dev 采样 (200q) | 0.695 | 0.730 | 0.769 | **0.833** | 不可比（采样语料；论文全量 test：D+S 0.414 / +rerank 0.448 / BM25 0.346） |

一句话：**BEIR 两集上语义臂与 mGTE 论文公开数字差 3-7 个点（512-token 切块+doc 折叠的可解释代价），keyword 臂距 Anserini BM25 差 5-20 个点（英文词法短板）；MLDR-zh 上四臂全升且排序翻转——keyword>semantic、hybrid 再抬、rerank 0.833 一骑绝尘（+6.4 点）。「恒 hybrid + 按需 rerank」的产品位形在中文长文域被外部数据背书，英文词法臂是下一条最值得修的短板。**

## 1. 硬口径（先声明）

1. **文档物化**：BEIR corpus jsonl → 每 doc 一个 txt（title 行 + 空行 + text）；MLDR-zh 原文 txt。查询/金标 = 官方 test/dev split 原样（MLDR qrels 由 dev.jsonl 内联 positives 构造，与官方 qrels 等价，grade=1；zh tsv 镜像 503）。
2. **切块**：产品默认 `token-window 512/64`（as-shipped 口径——成绩可直接解释为「产品默认配置的成绩」）。mGTE 论文为整文档 8192 ctx 嵌入，**这是与公开数字最大的口径差**。
3. **评分协议**：chunk 命中按首次出现折叠到 doc（BEIR doc 级 qrels 的标准 chunked-corpus 适配）；nDCG 线性增益 + log2 折扣（pytrec_eval 同式），抽查手算两查询一致；R@k / MRR@10 同源。
4. **四臂**：semantic / keyword / hybrid 均取 top-100；rerank 臂 = `--mode hybrid --rerank --top-k 10`（产品默认形态：候选深度=topK，R@100 由此受限为候选上限，读表注意）。
5. **索引态**：摄取后 `vectorfs optimize`（IVF_PQ + FTS icu 词典分词）。MLDR-zh 另有 ngram 消融（见 §4）。
6. **延迟**：`vf search` 客户端端到端墙钟（进程启动 + SigV4 + 回环 + 全链 + JSON 解析；`--no-text` 未抑制 metadata 携带的 chunk 全文，解析成本计入——见 §5 产品观察）。延迟非本文重点（M3 bench 已立口径），仅作参考。

## 2. BEIR 结果

### 2.1 总表

**SciFact**（5,183 docs → 24,564 chunks；300 查询，binary qrels）：

| 臂 | nDCG@10 | nDCG@100 | R@5 | R@10 | R@100 | MRR@10 |
|---|---|---|---|---|---|---|
| semantic | 0.660 | 0.686 | 0.750 | 0.827 | 0.933 | 0.623 |
| keyword | 0.444 | 0.447 | 0.493 | 0.500 | 0.507 | 0.439 |
| hybrid | 0.627 | 0.659 | 0.723 | 0.813 | 0.933 | 0.581 |
| +rerank | **0.686** | 0.686 | 0.777 | 0.783 | 0.783 | **0.669** |

**NFCorpus**（3,633 docs → 18,120 chunks；323 查询，分级 qrels 1-3）：

| 臂 | nDCG@10 | nDCG@100 | R@5 | R@10 | R@100 | MRR@10 |
|---|---|---|---|---|---|---|
| semantic | 0.338 | 0.297 | 0.681 | 0.752 | 0.858 | 0.542 |
| keyword | 0.272 | 0.205 | 0.539 | 0.557 | 0.588 | 0.454 |
| hybrid | **0.345** | 0.308 | 0.700 | 0.762 | 0.864 | **0.561** |
| +rerank | 0.319 | 0.213 | 0.715 | 0.734 | 0.734 | 0.576 |

（延迟参考，客户端口径 p50：semantic 354ms / keyword 302ms / hybrid 659ms / rerank 1524ms@k10——摄取竞争已排除，但含进程启动与 JSON 解析，非服务端口径。）

### 2.2 对照公开数字（同模型 mGTE，整文档嵌入）

| SciFact nDCG@10 | 值 | Δ(ours-论文) | NFCorpus nDCG@10 | 值 | Δ |
|---|---|---|---|---|---|
| mGTE dense（论文 Table 21） | 0.734 | **-7.4** | mGTE dense | 0.367 | **-2.9** |
| mGTE D+S | 0.735 | -10.8（our hybrid） | mGTE D+S | 0.367 | -2.2 |
| mGTE +reranker | 0.756 | -7.0（our rerank） | mGTE +reranker | 0.363 | -4.4 |
| Anserini BM25（BEIR 论文） | 0.646 | **-20.2**（our keyword） | Anserini BM25 | 0.325 | **-5.3**（our keyword） |

### 2.3 解读（机制假设，按证据强度排序）

1. **语义臂 -3~-7 点 = 切块折叠的可解释代价**：512-token 窗把 title+abstract 拆 ~4.7 chunk/doc；正解证据句所在 chunk 与同 doc 其他 chunk 平权竞争，doc 首现排名被稀释；论文整文档嵌入无此损失。R@100 两集 0.93/0.86 说明召回池在，排不出（这正是 R2/CCH 类语境前缀机制的目标病灶——章题传播在 PDF/EPUB 有题可用，txt 无结构信号是 Worst case）。
2. **keyword 臂 -5~-20 点，SciFact 重灾**：候选机制（按验证优先级）——① chunk 级 BM25 长度归一化（512-token 窗 vs Anserini 字段级索引）；② icu 词典分词无英文词干化/停用词（Lucene English analyzer 有 Porter stemming；探针：词形变化查询 top 命中不变但分值降——机制未钉死，存疑待查）；③ 无 title 字段加权。SciFact（术语密集短摘要）对 ①③ 最敏感。
3. **hybrid 一助一损**：RRF 等权融合下，弱词法臂拉低强语义臂（SciFact 0.660→0.627），两臂接近时互抬（NFCorpus 0.338→0.345）。与 R3「恒 hybrid 推荐」的预期一致——hybrid 的期望收益依赖词法臂质量。
4. **rerank 天花板 = 候选深度**：top-10 候选重排，SciFact +2.6 点（语义臂强，重排纠位），NFCorpus -2.6 点（候选池不够，R@100 0.734 vs 0.864 暴露上限）；R3a 的 `--rerank-depth` 解耦正是解此症的产品答案（148ms/pair CPU 预算内 depth 20 ≈ 3s）。

## 3. MLDR-zh dev 采样结果

语料：2,995 docs（200 qrels 正解 + 2,800 seeded 填充，SEED=20261112）→ **68,659 chunks（≈23 chunk/doc，长文段制）**；200 dev 查询（每题恰 1 正解）。IVF_PQ + FTS(icu) 已 optimize。

| 臂 | nDCG@10 | nDCG@100 | R@5 | R@10 | R@100 | MRR@10 |
|---|---|---|---|---|---|---|
| semantic | 0.695 | 0.717 | 0.755 | 0.800 | 0.890 | 0.662 |
| keyword | 0.730 | 0.736 | 0.765 | 0.770 | 0.795 | 0.718 |
| hybrid | 0.769 | 0.781 | 0.840 | 0.865 | **0.920** | 0.737 |
| +rerank | **0.833** | 0.833 | **0.855** | 0.865 | 0.865 | **0.822** |

**口径红线（先于一切解读）**：采样语料只含全量 20 万 docs 的 1.5%，缺位负例只可能推高绝对分——**绝对值与论文全量数字（BM25 0.346 / D+S 0.414 / +rerank 0.448）不可比**；公允的读法是**同语料四臂内部对照**。

四臂内部对照的读法：

1. **zh 上词法臂反超语义臂**（0.730 > 0.695）——与 mGTE 论文 zh 列一致（sparse 41.8 > dense 27.4）：中文长文的 BM25（icu 词典分词 + R1 锚词门控）是强臂，不像英文那样被词干化缺失拖垮。icu 词典分词对 CJK 的适配是及格的。
2. **hybrid 在双强臂下兑现 RRF 数学**（0.769 > 两单臂，R@100 0.920 最高）——与 BEIR 上「弱臂拉低强臂」互补，**「恒 hybrid 推荐」在中文域被外部数据背书**。
3. **rerank 是 zh 长文最大赢家**：+6.4 点（0.833）、MRR@10 0.822——cross-encoder 对长文 chunk 的语境判别力与 R2/CCH 同向；候选深度上限可见（R@100 0.865 < hybrid 0.920，top-10 候选天花板），`--rerank-depth` 解耦在 zh 域预期收益比 BEIR 更大。
4. 延迟（净测热态，客户端口径 p50）：semantic 246ms / keyword 176ms / hybrid 446ms（68.7k 行 IVF_PQ+FTS）；rerank p50 2.73s@k10。scifact 净测：semantic 362 / keyword 284 / hybrid 651ms（24.5k 行）。

## 4. 消融：fts-tokenizer ngram（CJK 词法臂）——本跑未做，裁决记录

`fts_tokenizer` **进桶指纹**（pipeline/config.rs：换分词 = reindex，ADR 0002 先例）——ngram 消融意味着 mldrzh 全量重嵌（~25k chunks ≈ 2h CPU），超出本次预算。诚记为后续项：CJK 词法臂的 ngram 收益值得单独一轮跑（toml 自注「best CJK recall」的验证），成本口径就是一次指纹翻转重嵌波。

另两个影响解读的查询面 knob（均在指纹外、本跑以默认态生效）：`anchor_gating` 默认 **true**（keyword/hybrid 臂带 R1 锚词门控跑的）；`fusion_overfetch` 默认 1（=topK 无 over-fetch，pre-R3b 行为）。

## 5. 产品观察（非基准缺陷，如实记录）

1. **批上传撞在飞 scan → 标记批量 strike 后活性停滞**：scifact 5,183 文件一次 `--async` 入队，首个 scan 窗口只消化 146 后 PENDING=5,037 冻结、无后续 scan 动作（文件全在盘、标记非终态）；指数 requeue 恢复臂（2→5→…→146）中途停摆。**serve 重启即愈**（干净重扫全量入队）。与 §6.1 beac012 同族但恢复臂未兑现到该批次——建议给 ingest 调度器加「PENDING+文件在盘=N 且 worker 空闲」的活性断言护栏。
2. **SIGTERM 关机挂死**：mldrzh 20 任务在飞时 SIGTERM，监听/flock 已释放（optimize 可跑）但进程 37 分钟不退——「≤8s drain」承诺未兑现。kill -9 崩溃重启路径验证无损（标记/索引完好续跑）。
3. **`--no-text` 未抑制 metadata 携带的 chunk 全文**（`x-vf-chunk-text`）：top-100 查询每命中 ~1KB 额外载荷与解析成本。建议 no-text 语义覆盖 metadata 文本键，或提供瘦身输出档。
4. **客户端 env 回落脚枪实录（§6.3 再度应验）**：本 bench 一次 vf 调用漏传 env 链，静默回落 `~/.config/vectorfs/config.toml` 打到 8787 生产实例——幸而目标桶不在其配置，400 被拒零污染。`--no-config` 或「有 endpoint 无凭据即显式报错」仍值得做。
5. **摄取与查询共享 gte 会话互斥锁**：20 worker 摄取满载时查询编码饿死（实测 0.8 query/min vs 空载 ~300/min）。单机单写者档可接受（本 bench 用「暂停桶段」规避），大库全量重嵌窗口期检索不可用值得写进运维文档。

## 6. 复现附录

- 数据源：BEIR zips（UKP canonical：`public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/{scifact,nfcorpus}.zip`）；MLDR-zh（HF `Shitao/MLDR` mldr-v1.0-zh corpus/dev + qrels，经 hf-mirror；1.37GB 语料单连接被限速 ~170KB/s，12 路分段并行 ~2 分钟拿下——镜像对大 LFS 的限速是按连接的）。
- 工件（全部在 `/root/bench-ir/`）：`scripts/materialize_beir.py`（jsonl→txt+金标）、`scripts/sample_mldr.py`（全 qrels 正解+seed 填充采样，SEED=20261112）、`scripts/run_queries.py`（四臂 runner，ranked.jsonl 原始排名落盘可审计）、`scripts/score.py`（nDCG/Recall/MRR）、`scripts/wait_mldrzh.sh`（排空→optimize→四臂编排，含卡死自愈）；`results/*_gold.json`（金标）、`*_ranked.jsonl`（逐查询原始排名）、`*_scores.json`。数据目录 `/root/bench-ir/data`（隔离 daemon，端口 8788）。
- 关键命令形态：`vf put <dir>/*.txt <bucket>/ --async` → `vf status` 轮询 → 停服 `vectorfs optimize --data-dir … --bucket …` → 重启 → `vf search "<q>" <bucket> --mode {semantic|keyword|hybrid} [--rerank] --top-k {100|10} --json --no-text`。

## 7. 收口

- **对外可引用的一句话成绩**（as-shipped：产品默认 512/64 切块 + gte + 恒 hybrid + 按需 rerank，CPU 档）：BEIR SciFact nDCG@10 = 0.66(semantic)/0.69(+rerank)、NFCorpus = 0.34/0.32——同模型（mGTE）论文整文档口径 0.73/0.37、Anserini BM25 0.65/0.33 之间；MLDR-zh 采样集四臂 0.70→0.83 全梯（绝对值采样口径，四臂内部对照公允）。
- **下一条最值得修的短板**：英文 keyword 臂（-5~-20 vs Anserini；候选机制：chunk 级 BM25 长度归一 / icu 无英文词干化 / 无 title 字段加权，未钉死）。修法候选（查询面/指纹外优先）：英文 analyzer 变体、fusion_overfetch 调优；涉及索引结构的（字段化 BM25）须走指纹翻转预算。
- **第二轮候选**（成本已估）：ngram 分词消融（=一次 mldrzh 全量重嵌 ≈2h）；`--rerank-depth 20/50` 在 zh 域的增益曲线（查询面，零重嵌成本，148ms/pair 线性预算）；BEIR 大集（ArguAna/Touche）验证 keyword 病灶泛化性。
- 基准工件全量保留于 `/root/bench-ir/`（语料、金标、逐查询原始排名、脚本），`results/*_scores.json` 为机器可读结论。bench daemon（8788）已停服；复跑：`vectorfs serve --data-dir /root/bench-ir/data` + `scripts/run_queries.py <bench> <bucket>` + `score.py <bench>`。
