# 设计：检索工效薄层（源名过滤 + chunk 读动词）

> 状态：proposed（2026-11-06）。提案：`docs/proposals/2026-11-retrieval-ergonomics.md`；
> ADR：`docs/adr/0004-superset-face-source-scoping.md`。查询面/CLI 层改动，零 schema、零指纹、
> 不依赖 M3 执行序。

## 1. `/x-vf/Search`：`x-vf-src-name` 过滤翻译

### 1.1 翻译规则（服务端，编译前重写）

Search filter 编译前（`ops_xvf.rs:745` 之前）对 AST 做一次字段重写：

| 用户 filter | 翻译为 | 说明 |
|---|---|---|
| `{"x-vf-src-name": "docs/fox.txt"}` | `{"x-vf-src-uri": {"$eq": object_key_hash("docs/fox.txt")}}` | 标量 shorthand 归 `$eq` |
| `{"x-vf-src-name": {"$eq": k}}` | `{"x-vf-src-uri": {"$eq": hash(k)}}` | |
| `{"x-vf-src-name": {"$in": [k1, k2]}}` | `{"x-vf-src-uri": {"$in": [hash(k1), hash(k2)]}}` | 翻译后计入既有 100 constraints 上限 |
| `{"x-vf-src-name": {"$ne/$startsWith/$gt/…": …}}` | **400** ValidationException，message 指引「该算子不支持源名，用 `x-vf-src-uri`」 | fail loud 不半译 |

- **为什么成立**：`object_key_hash` 是对象键的确定性函数（`ingest/marker.rs`）——名字与哈希
  一一对应，翻译语义无损；
- **`$and/$or` 嵌套**：重写递归下钻，遇不可译算子整支 400（与 filter pushdown 的「不可译
  跳过」语义相反——此处是**用户意图完整性**优先，宁拒不错）；
- **协议面零变化**：翻译只在 `/x-vf/Search` 入口；QueryVectors 对 non-filterable 键的 400
  拒绝原样（index meta 的 `nonFilterableMetadataKeys` 声明不动）；
- **MCP**：`search` 工具同享（同一 handler）。

### 1.2 CLI

```
vf search "vibe coding" docs --source radar/tr_technology_radar_vol_32_cn.pdf
```

`--source <key>`（可重复 ⇒ `$in`）：客户端展开为 `{"x-vf-src-name": …}` filter——与 wire
翻译单一事实源在服务端，CLI 只做参数→filter 的浅包装。

### 1.3 命中输出

默认表格增加**源标识列**（object key 尾段，超宽省略中段）；`--json` 已有完整
`location.key`，不变。

## 2. `vf chunk show`：chunk 级读动词（纯 CLI）

```
vf chunk show docs 510da8…-text-3 --around 2 --chars 600
```

### 2.1 机制

- **相邻块客户端可算**：chunk key 形如 `{hash}-{modality}-{i}`（`ingest/mod.rs:2271-2274`
  注释核验；image 模态 M3 同构 `-image-{i}`）——解析尾序号，`i±1..i±N` 重建键集；
- **取回**：协议面 `GetVectors`（`returnMetadata = true`）批量取回（每调用 ≤100 向量，
  协议限额内；`--around N` 上限 20 防滥用）；
- **渲染**：按 i 序输出 `[i-2] … [i] … [i+2]`，每块 `--chars` 截断（缺省 400，全文本
  `--chars 0`）；metadata 键值随行显示（`x-vf-chunk-kind` 等标签可见——与结构标签设计衔接）。

### 2.2 边界

- 不可达序号（越界/未存）静默跳过（GetVectors 键缺失语义）；
- 块键非法（不匹配 `{hash}-{modality}-{i}`）⇒ USAGE 错误；
- **不做**服务端 chunk 读 API：GetVectors 语义已完备，客户端拼装即可（ADR 0004 记录）。

## 3. Riders（随本组顺手清偿）

- **F7**：`vf get` 双位置参数误用（`vf get docs "key"`）的 clap 错误追加一行用法提示
  （「path form: vf get docs/key」）——错误模板改动，既有 USAGE 测试同步；
- **F9**：vf-search skill / README 补「分数语义」节（semantic `1/(1+distance)`、hybrid RRF
  名次分、keyword BM25 无界分——与词法设计 §4.3 的文档承诺同源）。

## 4. 测试门

1. 翻译：`$eq`/shorthand/`$in`/嵌套 `$and` 命中正确源；不可译算子 400 且 message 含指引；
   翻译后 constraints 计数正确；
2. 协议面：QueryVectors 对 `x-vf-src-name` 仍 400「non-filterable」（不回归）；
3. CLI：`--source` 端到端（真 daemon）；`vf chunk show` 相邻块序正确、截断/全本双形态、
   越界跳过、非法键 USAGE；
4. 输出列：源标识列渲染与省略形态 golden 断言。
