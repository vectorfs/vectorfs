---
status: accepted
---

# 超集面源名过滤：服务端 name→hash 翻译，协议面语义不动

**已执行**（T15，2026-11-09，9172ecd）：编译前 JSON 层翻译（`$eq`/shorthand/`$in` + 递归
`$and/$or`），不可译算子 400 path=filter；QueryVectors non-filterable 拒绝同 filter 显式钉住
不回归；空命中 note + constraints 触顶回指用户所写名字；CLI `--source`。`vf chunk show` 归属
ADR 0005（T14 交付）。

> **修订（2026-11-07）**：`vf chunk show` 的规格归属移入 ADR 0005（内容寻址组，T14）——本 ADR
> 收窄为源名过滤（F5）+ riders（F7/F9/文档义务）；新 400 错误的 fieldList path 归属（filter/
> searchMode）与 MCP/skill 文档更新义务并入 T15。错误 path 先例与约束计数核验见四路审查。
> 完整任务定义见 `docs/design/2026-11-retrieval-quality-solution.md` §2-T15。

按源文件（人类可读名）收窄检索的能力放在 `/x-vf/Search` 超集面：filter 编译前对 `x-vf-src-name` 字段做服务端重写——`$eq`（含标量 shorthand）与 `$in` 翻译为 `x-vf-src-uri` 的确定性哈希（`object_key_hash` 是对象键的纯函数，语义无损），其余算子 400 fail loud 并指引。协议面 QueryVectors 对 non-filterable 键的 400 拒绝原样（`nonFilterableMetadataKeys` 声明不动）——两面孔、一套存储，翻译规则单一事实源在服务端（CLI `--source` / MCP / HTTP 直连三面一次受益）。2026-11-06。

## Considered Options

- **`x-vf-src-name` 转可过滤**（改 nonFilterableMetadataKeys 声明）——协议面 400 拒绝语义消失，制造 AWS 对标差异，出局
- **派生表加 src-name Lance 原生列**——可支撑 `$startsWith`/glob，但当前无需求；留作演进（届时列走 server-internal schema，仍不动协议声明）
- **客户端翻译**（CLI 哈希后发 filter）——零服务端改动但 MCP/HTTP 直连不受益，翻译逻辑藏进客户端，不采
- **服务端 chunk 读动词**——GetVectors 语义已完备（metadata 含 chunk 文本），客户端拼装即可，不采

## Consequences

- `$startsWith`/glob 源名过滤明确不支持（哈希破坏前缀结构）；翻译后过滤计入既有 100 constraints 上限
- chunk key 结构 `{hash}-{modality}-{i}` 成为客户端契约（M3 image 模态同构）——key 结构演化需保持该解析兼容
- `vf search --source <key>`（可重复 ⇒ `$in`）与命中表格源标识列为伴生 CLI 面
- F7（`vf get` 误用报错加路径形态提示）、F9（分数语义文档）作为 riders 随本组清偿
