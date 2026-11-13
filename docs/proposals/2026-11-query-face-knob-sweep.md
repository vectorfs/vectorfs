# 提案：查询面旋钮外部金标曲线（B2）——rerank-depth / fusion-overfetch / weighted merge

> 状态：**proposed**（2026-11-13）。
> 来源：`research/2026-11-external-ir-bench.md` F3/F4——mldrzh rerank@depth10 0.833 但
> R@100 0.865 < hybrid 0.920（**候选池里有货没被重排，缺口 5.5 点**）；SciFact hybrid
> 0.627 < semantic 0.660（**弱词法臂下等权 RRF 拉低强语义臂**）。
> 定位：三个**已落地、指纹外**的查询面 knob 首次上外部金标——零重嵌、零产品风险、立即可跑。

## 1. 问题

R3a（`--rerank-depth` 解耦）、R3b（`fusion-overfetch`）、M3-T8（`merge = "weighted"` +
`merge-weights`，`ops_xvf.rs:5882` 测试钉住词法臂可加权）都已进 HEAD，但**全部停留在默认态
（depth=topK、overfetch=1、rrf 等权）**——没有一张外部数据曲线告诉用户（和我们）什么时候值得
付出 CPU 预算。首跑的 F3/F4 正是这两个机制的靶点：候选深度封顶与弱臂稀释。

## 2. 目标 / 非目标

**目标**
1. **rerank-depth 曲线**：{10, 20, 50} × 三基准 × hybrid+rerank——量化 mldrzh 的 5.5 点
   R@100 缺口回收曲线 + BEIR 上「rerank 增益依赖语义臂强度」假设的深度维验证；
2. **fusion-overfetch 曲线**：{1, 2, 4} × 三基准 × hybrid——弱臂稀释是否被 over-fetch 缓解
   （SciFact 0.627 的回升空间）；
3. **weighted merge 静态档**：lexical:dense 加权（如 2:1 / 1:2 两档）× SciFact/mldrzh——
   等权 RRF 的双向损益是否存在静态加权甜点；**只测静态档**。
4. 产出：三组曲线表进 B0 基线工件；若 depth 20 曲线陡，文档配方升级（「质量敏感路径
   depth 20 ≈ 3s」从估算变成实测背书）。

**非目标**
- 不改任何默认值（曲线是证据，默认值变更独立裁决）；
- 不做自动/在线调权（roadmap §3 明确不做：三集过拟合脚枪）；
- 不动 reranker 模型轴（R5 另裁）。

## 3. 方案选项

| 选项 | 裁决 |
|---|---|
| **A. 纯基准机时（现有 knob × 现有索引）** | **采**——零产品改动；depth 50 × 823 查询 × 148ms/pair ≈ 1.7h，全曲线 ~3h 机时 |
| B. 顺带改默认（曲线跑完即升 depth 默认） | 否——默认值变更是用户可见行为变化，须单独裁决 + 文档迁移说明 |
| C. 等 B1 判决后一起跑 | 否——B2 与 B1 无依赖（B1 治词法臂内容，B2 治融合/深度策略），并行 |

## 4. 关键取舍

1. **CPU 诚实预算**：depth 50 = 7.4s/查询（148ms/pair 线性）——曲线表自带延迟列，交互级/
   批处理级判词照 M3 bench 口径；
2. **rerank 臂候选深度的口径注记**：首跑 rerank 臂 R@100=候选上限是 depth=10 的伪上限——
   B2 曲线天然修正该口径，基线表加脚注；
3. **weighted merge 的键名纪律**：实例命名以 config.rs 校验面为准（已知实例名才能进
   merge-weights），曲线表记录确切 TOML 形态。

## 5. 依赖与风险

- 依赖：B0（同口径重跑）；bench daemon 复活（一条命令）；
- 风险：①三基准曲线结论互相矛盾（如 over-fetch 在 BEIR 增益、在 mldrzh 减益）——按域分档
  记录结论，不强求全局默认；②加权档过拟合三集——只报测得值与「未见集泛化未验证」声明。
