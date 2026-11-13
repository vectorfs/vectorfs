# 提案：英文 analyzer（stemming/停用词）——`fts-tokenizer` 新枚举（B3）

> 状态：**proposed**（2026-11-13，**立项门 = B1-P1 判决词干化主导**）。
> 来源：`research/2026-11-external-ir-bench.md` F1——BEIR keyword 臂距 Anserini
> SciFact -20.2 / NFCorpus -5.3；候选机制之一是 icu 链无英文词干化（M1）。
> 定位：索引面修复，**本路线图唯一需要指纹翻转波次的项**；B1 判决书是立项前提。

## 1. 问题

若 B1-P1 判决 M1（词干化缺失）主导：英文查询的屈折形（developments/properties/cells）与
索引原形（development/property/cell）在 FTS 里互不匹配——这是 analyzer 能力缺失，查询面
无解（T17 同源纪律：查询侧与索引侧同一分词管线，单侧 stemming 只会让查询词漂离索引词表）。
修复必须动 `fts-tokenizer` 的索引侧行为 ⇒ 按 ADR 0002 进桶指纹 ⇒ reindex 波次。

## 2. 目标 / 非目标

**目标**
1. `fts-tokenizer` 增枚举值（命名建议 `icu-en-stem`：icu 六段链 + Latin 词 Porter 还原 +
   英文停用词表），**CJK 路径零变化**——zh 回归断言（mldrzh keyword 臂 B0 基线 ±0）进测试；
2. BEIR 两集 keyword/hybrid 臂复测：目标 keyword SciFact 0.444 → 向 Anserini 0.646 收敛
   过半（保守 +8~12 点；Anserini 另有字段化加成，不追平承诺）；
3. 迁移波次照 T18 模板：新枚举序列化进指纹 ⇒ 升级即指纹门拒启 ⇒ `serve --reindex`；
   override compose 片段 + 迁移说明（受影响源/chunk 数/重嵌时长如实入文）。

**非目标**
- 不改默认值（`icu` 仍是默认枚举——新值 opt-in，存量部署零波次，除非用户自选）；
- 不做 ngram CJK 消融（另一轴，roadmap §4 已记后续项）;
- 不做字段化 BM25/title 加权（M3 若是主因，由 B4 量尺另立机制，不走本提案）。

## 3. 方案选项

| 选项 | 裁决 |
|---|---|
| **A. lance-tokenizer 14.0.2 版本线内扩展**（上游 tokenizer 管线组合出 en-stem 变体，同 R7 同源纪律） | **采**——与索引内核同上游的既定路线；round-trip 差分断言（T9 先例）钉住 |
| B. 查询侧 expansion（屈折展开查询词） | 否——违反 T17 同源管线；展开词与索引词表正交漂移，且长尾屈折不可枚举 |
| C. 双列双 tokenizer（icu + en-stem 并存三路） | 否——提案 0002 已留「进阶」门，本提案不扩面；单列换枚举已覆盖病灶 |

## 4. 关键取舍

1. **opt-in 而非默认翻转**：本提案的波次只落在**自选该枚举的桶**——比 T18（默认 knob 翻转
   强制全量）温和；代价是英文收益不会自动到达存量部署（文档引导）；
2. **停用词表的最小面**：只配英文闭式词表（analyzer 内过滤，非新持久化结构）——zh 无停用词
   概念（词典分词自带），路径隔离；
3. **性能预算**：Porter 还原是纯函数级成本，FTS 索引构建时长预期变化 <5%——B0 复测列记录。

## 5. 依赖与风险

- 依赖：B1-P1 判决书（M1 主导）；lance-tokenizer 上游能力 spike（若 14.0.2 无 en-stem
  组合件，评估上游最小补丁 vs 本地 tokenizer 包装——供应链校验成本如实记录）；
- 风险：①Porter 过度还原伤专有名词（SciFact 术语密集）——曲线说话，若 NFCorpus/SciFact
  分歧则域分档；②zh 混排文档（中英夹杂）里 en-stem 误伤 CJK 邻接 Latin——CJK/Latin 判界
  进差分断言；③波次纪律执行成本——照 T18 实战工件模板，不新发明流程。
