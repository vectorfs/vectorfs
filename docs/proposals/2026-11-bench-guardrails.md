# 提案：基准过程实录的护栏四件套（B5）——活性断言 / drain 看门狗 / no-text 瘦身 / env 回落显式化

> 状态：**proposed**（2026-11-13）。
> 来源：`research/2026-11-external-ir-bench.md` §5 产品观察——外部基准首跑过程本身成了
> 一次高强度 dogfooding（8.8k+3k 文件批摄取、多次停服 optimize/重启、823 查询），
> 四条缺陷全程可复现。非检索质量问题，故独立成案、随时插队。

## 1. 问题（四条实录）

| # | 观察 | 实录（2026-11-12 bench） |
|---|---|---|
| G1 调度器活性停滞 | 批上传撞在飞 scan → 5,183 文件中 5,037 标记被 strike 后**冻结无 scan 动作**（文件全在盘、标记非终态）；指数 requeue 恢复臂（2→5→…→146）中途停摆；**serve 重启即愈** | scifact 桶实测；与 §6.1 beac012 同族但恢复臂未兑现到该批次 |
| G2 SIGTERM 挂死 | 在飞嵌入时 SIGTERM：监听/flock 已释放、进程 37 分钟不退（承诺「≤8s drain」）；空闲态 SIGTERM 3s 干净退出 | mldrzh 20 任务在飞实测；kill -9 崩溃重启路径验证无损 |
| G3 `--no-text` 不瘦身 | metadata 仍带 `x-vf-chunk-text` 全文：top-100 查询 ≈100KB/查额外载荷与解析成本 | 四臂 runner 全程背负 |
| G4 env 回落脚枪 | vf 漏传 VF_* 静默回落 `~/.config/vectorfs/config.toml` 打到**另一个生产实例**（8787）——幸而目标桶不在其配置，400 被拒零污染 | §6.3 老账第三次实录；本次 bench 一次、M3 bench 一次 |

## 2. 目标 / 非目标

**目标**
1. **G1 活性断言护栏**：`PENDING+文件在盘 = N 且 worker 空闲持续 K tick` 判定停滞 → 自动
   重新入队（或至少 stderr 响亮告警）；回归测试复刻 bench 竞态（批上传 × 在飞 scan 交错）；
2. **G2 drain 看门狗**：SIGTERM drain 超时（建议 >8s 阈值告警、>60s 自伤退出）+ 空闲态 3s 退出
   进回归测试；「≤8s」承诺改写为「空闲 ≤8s / 在飞有界看门狗」；
3. **G3 no-text 覆盖 metadata 文本键**（或新增 `--slim`）：`x-vf-chunk-text` 随 no-text 一并
   省略——CLI/MCP/--json 三面同语义；
4. **G4 env 回落显式化**：`--no-config` 旗标 + 「env 有 endpoint 而无凭据」降级为显式错误
   （§6.3 老提案正式立项）。

**非目标**
- 不改调度器架构（单写者 + 周期扫描模型不变——G1 只补活性兜底）；
- 不动 metadata 键契约（`x-vf-chunk-text` 照写，只是查询输出面瘦身）；
- 不做多实例路由/实例别名（G4 只堵静默回落，不新增选址智能）。

## 3. 方案选项

| 缺口 | 选项 | 裁决 |
|---|---|---|
| G1 | **A. scan 循环内活性判据 + 自愈 re-enqueue**（复用 beac012 Refresh 臂通道） | **采**——自愈优于告警；判据保守（K≥3 tick）防误触发 |
| | B. 只加告警不自愈 | 否——bench 里是「全量冻结直到人肉重启」，告警不解决夜里跑批场景 |
| G2 | **A. 看门狗线程：drain 超时强退** | **采**——崩溃重启路径已被 lifecycle_crash_replay 背书，强退是安全操作 |
| | B. 只修文档措辞 | 否——37 分钟不退在运维上等同挂死 |
| G3 | **A. no-text 语义扩展覆盖 metadata 文本键** | **采**——语义即名：no-text 就是不要 text；键级 opt-out（`--with-chunk-text`）留逃生 |
| | B. 新增 --slim 独立旗标 | 否——两旗语重叠，用户面临三选一矩阵 |
| G4 | **A. `--no-config` + 半配置显式报错** | **采**——§6.3 原方子；显式旗标满足脚本场景（CI/多实例） |

## 4. 关键取舍

1. G1 自愈判据的**误触发代价**：K tick 无进展也可能是合法长任务（OCR 600s 超时预算）——判据
   加「worker 空闲」前置条件（在飞任务≥1 时不判停滞），长任务豁免；
2. G2 强退阈值取 60s：覆盖最大合法单任务尾延（extract_timeout 600s 是单文件预算，drain 等的
   是批尾；60s 批尾 + kill -9 崩溃恢复，语义损失为零）；
3. G3 是 wire 行为变化：schema_version'd JSON 的键集变化照 CLI 契约记录（v0.2 内，与
   anchors 空列表语义化先例同类——minor 清理档）；
4. G4 显式报错可能伤「config.toml 只存 endpoint」的既有用户——报错文案指向两条出路
   （补 env 凭据 / --no-config 显式选 config），不是硬断。

## 5. 依赖与风险

- 依赖：无跨提案依赖；G1 回归测试需要竞态注入工具（批上传 × scan 交错）——ingest_kill9.rs
  先例可扩；
- 风险：①G1 自愈与 strike 计数交互（误 strike 的合法标记被反复重入队）——重入队幂等
  （内容寻址键），最坏是重复处理成本；②G2 强退打断半提交 Lance 写——事务性由 store 层
  保证（crash replay 测试域）；③G4 报错面波及 cli_smoke（其用空 XDG_CONFIG_HOME 隔离——
  正是显式旗标的受益者，测试更新为 --no-config 形态）。
