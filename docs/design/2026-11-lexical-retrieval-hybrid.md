# 设计：词法检索与 hybrid 查询（Lance 原生 FTS）

> 状态：proposed（2026-11-06）。提案：`docs/proposals/2026-11-lexical-retrieval-hybrid.md`；
> ADR：`docs/adr/0002-lexical-kernel-lance-native-fts.md`。里程碑归宿 M3 T8；本文为 T8 的
> 实施级设计输入。代码锚点均为本会话核验（lancedb 0.41 / lance 14.0.2 registry 源码 + 仓库源码）。

## 1. 数据模型：chunk 文本升列

派生索引表新增顶层列 **`chunk_text: LargeString`**（ nullable）：

- **为什么顶层列**：FTS 建列索引（`Index::FTS` on `["chunk_text"]`）；metadata blob 内的
  `x-vf-chunk-text` 键不满足建列前提（blob-only，无镜像列），且 lance 的 JSON tokenizer 是
  `path,str,value` 三元组语法，不适用纯文本查询（`lancedb/src/index.rs:80-87` 文档注释核验）；
- **命名**：无 `x-vf-` 前缀——该前缀是 metadata 键约定（ADR 0001），列名与 metadata 键是两个
  命名空间；下划线避免 SQL 路径的连字符引号问题；
- **写入**：`build_rows`（`ingest/mod.rs:2275`）在 `store_chunk_text = true` 时同时写 blob 键
  与列（同批同值，单一来源派生）；`= false` 时列写 null；
- **演化**：走既有 AllNulls `add_columns` 自动演化（`store.rs` `evolve_metadata` 同机制，
  对顶层列同理）；旧表升级后列全 null，`optimize`/查询不受影响，**无 FTS 索引时词法路由
  fail loud**（见 §5）；
- **协议面不可见**：QueryVectors/GetVectors 投影 key/metadata/vector，不选额外列——
  协议语义零变化。

## 2. FTS 索引生命周期

1. **创建**：`ensure_domain`（`ingest/domain.rs:122`）建索引时（或列首次演化后）追加
   `create_index(&["chunk_text"], Index::FTS(params))`；幂等（已存在则跳过，沿用 load-check
   模式）。params：`base_tokenizer` 由桶配置解析（§3），`lower_case` 默认 true，
   `block_size` 守默认 128（256/v3 实验性不启用），其余默认。
2. **增量**：ingest 批提交后 Lance 自动跟踪未索引行；**查询不漏**（未索引行 flat 兜底，
   `lancedb table.rs:1523-1528` 核验）；`optimize_index` 挂到既有周期扫描 tick，阈值门控
   （`unindexed_rows > max(1000, 10% total)` 才 optimize，避免小批抖动）。
3. **删除**：DeleteVectorBucket 删系统桶 → 表与 FTS 索引连坐消失（design-v3 不变量 1）；
   GC 的行级删除（`x-vf-src-uri` join）由 Lance 索引维护语义覆盖。
4. **换代**：tokenizer 变更 ⇒ 指纹变更 ⇒ 既有 `vf reindex` 显式重建语义管辖（删旧 FTS 索引、
   重建列值与索引）。

## 3. 配置与指纹

`[ingest.<bucket>]` 新增：

```toml
[ingest.docs]
lexical = "icu"          # 或 "ngram"；缺省 "icu"；显式 "off" 关闭词法路由
```

- **归轴**：lexical 属于派生索引（DerivedSpec）属性，与 embedder/model/dim 同级登记；
- **指纹**：进管线指纹（`pipeline/config.rs` fingerprint 序列化）——换 tokenizer = 换索引代
  = reindex 语义，防「向量新、词法旧分词」的半新半旧；
- **合法值**：`icu | ngram | off`（lance 菜单子集；jieba/lindera 明确不采——外部模型目录
  与自包含部署相悖）。`ngram` 映射 `min_ngram_length=2, max_ngram_length=2, prefix_only=false`。

## 4. 查询面：`/x-vf/Search`

### 4.1 wire

```json
{"bucket":"docs","query":{"text":"vibe coding"},"searchMode":"hybrid","topK":10}
```

`searchMode`：`semantic`（缺省，现行为不变）| `keyword` | `hybrid`。MCP `search` 工具透传
`searchMode`（camelCase）。

### 4.2 编排（fan-out 扩展）

Search 的路由循环（`ops_xvf.rs:762-807`）目前按 embedder 展开。词法路由是**非 embedder 路由**：

```
routes = embedder_routes (现有)          // semantic 路由，模态匹配后逐路查询
       + lexical_route (若 searchMode ∈ {keyword, hybrid} 且 lexical != off)
```

- `keyword`：仅词法路由（单路）；
- `hybrid`：全部 semantic 路由 + 词法路由，`merge_hitlists(Merge::Rrf)`（现有实现，k=60）；
- 词法路由查询：`table.query().only_if(<filter pushdown 翻译>).full_text_search(
  FullTextSearchQuery::new(text).with_limit(N)).column("chunk_text")`，`N = max(topK, 2×topK)`、
  上限 1000；filter 复用既有编译（`x-vf-chunk-kind` 默认排除策略同样作用于词法路由，见
  结构标签设计）；
- 命中形状：`index` 字段 = 派生索引名 + `:fts` 后缀（可归因）；`key` = chunk key（与语义
  路由同键空间，RRF 融合按 key 聚合——现有机制零改动）。

### 4.3 分数语义（文档化承诺）

| 模式 | score | distance |
|---|---|---|
| semantic 单路（现状） | `1/(1+distance)` 直通 | 有 |
| keyword 单路 | BM25 原始分（无界，仅单调可比） | 无 |
| hybrid | RRF 和 `Σ 1/(60+rank)`（仅名次语义） | 无 |

### 4.4 降级与报错

| 情形 | 行为 |
|---|---|
| `store_chunk_text = false` 或列缺失或 FTS 索引未建，请求 `keyword`/`hybrid` | 400 ValidationException，message 指引（开启 store_chunk_text / 等待索引 / lexical 配置） |
| 索引存在但有未索引行 | Lance flat 兜底，自动，无标记 |
| `lexical = "off"`，请求 `keyword`/`hybrid` | 400（配置明确关闭） |

fail loud 而非静默降级：词法是用户显式请求的模式，静默回退语义检索会答非所问。

## 5. 测试门

1. 单路 keyword：golden 语料（含 CJK/EN 混排 chunk）精确词命中 top-1；BM25 分数单调；
2. hybrid：语义盲区词（会话实证的 "vibe coding" 语料回归）——词法路由补进 top-k；
   RRF 融合确定性（score desc, key asc）不回归；
3. 生命周期：列演化后旧表 fail loud → reindex 后 keyword 可用；tokenizer 变更触发指纹 diff；
   optimize 阈值门控（小批不触发，大批触发）；
4. 协议面：QueryVectors 全 21 op 行为零变化（conformance 套件不回归）；
5. `lexical = "ngram"`：中文 bigram 召回优于 icu 的语料断言（雷达 PDF 目录/正文可作 fixture）。

## 6. 明确不做（本设计内）

`$textMatch` 过滤算子语法、knn[] 多路、自定义排序、weighted 融合（`Merge::Weighted` 变体位
已留）、fuzzy 暴露到 wire（服务端能力已具备，暴露随需求）——T8 后续按需裁决。
