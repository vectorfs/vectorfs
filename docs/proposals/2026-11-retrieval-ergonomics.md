# 提案：检索工效薄层（源名过滤 + chunk 读动词）

> 状态：proposed（2026-11-06）。来源：dogfooding 摩擦 F5/F6（`research/2026-11-friction-backlog.md`），
> 机制研究 `research/2026-11-retrieval-friction-mechanisms.md` §3。
> 对应 ADR：`docs/adr/0004-superset-face-source-scoping.md`；设计：`docs/design/2026-11-retrieval-ergonomics.md`。

## 1. 问题

检索的「收窄到一文件」与「读懂一命中」两步在当前工具面上断层：

- **F5 按源文件过滤只认 `x-vf-src-uri` 哈希**：想只在 vol 32 内搜，须先 `--json` 挖哈希再拼
  filter；默认表格输出不显示哈希。根因：`x-vf-src-name`（人类可读名）被声明 non-filterable
  （AWS 兼容约束），对它过滤返回 400——但**哈希是名字的确定性函数**（`object_key_hash`），
  翻译在机制上是平凡的；
- **F6 命中文本预览 ~40 字符**，无法判断相关性，每条命中都要追加命令确认；想读完整 chunk
  及上下文，只能 `vf cat` 整文件（2.7MB PDF）。而 chunk key 形如 `{hash}-text-{i}`——**相邻
  块客户端可算**，`x-vf-chunk-text` 就在 metadata 里，协议面 GetVectors 本可取回。

## 2. 目标 / 非目标

**目标**
1. `/x-vf/Search` 接受 `x-vf-src-name` 过滤（`$eq/$in`），服务端机械翻译为 `x-vf-src-uri`；
2. CLI `vf search --source <key>` 一等参数；命中表格输出加源标识列；
3. CLI `vf chunk show <chunk-key>` 读完整 chunk 及相邻块（`--around N`）、可控预览长度
   （`--chars N`，顺带解决 F9 一半）；
4. 协议面（QueryVectors）non-filterable 拒绝语义零变化；零服务端 schema 变更。

**非目标**
- 不做 `x-vf-src-name` 的 `$startsWith`/glob（哈希破坏前缀结构，翻译覆盖不了；未来需要时走
  派生表 server-internal 列，另行提案）；
- 不做服务端 chunk 读动词（GetVectors 已够，客户端拼装即可）；
- F7（CLI 误用报错指引）、F9（分数语义文档）作为 rider 随本组顺手清偿，不单独立项。

## 3. 方案选项

| 选项 | 评估 |
|---|---|
| **A. 超集面服务端翻译 + 纯 CLI 动词**（推荐） | F5：`object_key_hash` 确定性函数，`/x-vf/Search` 一处翻译，两面孔一套存储；F6：chunk key 结构客户端可算，GetVectors 批量取回，零服务端改动 |
| B. `x-vf-src-name` 转可过滤（改 nonFilterableMetadataKeys 声明） | 动协议面语义：QueryVectors 对该键的 400 拒绝消失，AWS 对标差异——出局 |
| C. 派生表加 src-name Lance 原生列（server-internal） | 能支撑 `$startsWith`/glob，但当前无此需求；F5 首切不需要，留作演进 |
| D. 客户端翻译（CLI 把 name 哈希后再发 filter） | 零服务端改动，但 MCP/HTTP 直连用户享受不到；翻译逻辑藏进客户端不利一致性 |

## 4. 关键取舍

1. 翻译放**服务端 `/x-vf/Search`**（不是客户端）：MCP、CLI、HTTP 直连三个面一次受益，
   翻译规则单一事实源；
2. 翻译**仅覆盖语义可保证的算子**（`$eq/$in`；标量 shorthand 归 `$eq`），其余算子遇
   `x-vf-src-name` 返回 400 并指引（"use x-vf-src-uri"）——fail loud 不半译；
3. `vf chunk show` 是 CLI 动词不是 API 操作：chunk key 的 `{hash}-text-{i}` 结构是服务端
   约定，客户端据此算 `i±1` 批量 GetVectors——服务端零新增面。

## 5. 依赖与风险

- 依赖：无新依赖、无 schema 变更、无指纹变化（查询面/CLI 层改动）。
- 风险：chunk key 结构 `{hash}-{modality}-{i}` 成为客户端契约（M3 image 模态 `-image-{i}`
   同构，已核验 `ingest/mod.rs:2271-2274` 注释）——需在 key 结构变更时保持兼容意识；
   翻译后的 `x-vf-src-uri` 过滤仍受既有 100 约束上限管辖（`$in` 长度计入 constraints）。
