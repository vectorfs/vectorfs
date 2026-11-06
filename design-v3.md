# vectorfs v3.1 桶模型重构设计：双桶映射（source bucket + 系统桶）

> 状态：**已实施**（PR1-3 完成，2026-11-06；G1/G2/G4 自证，实施裁决回填 §14）。三路独立审查（协议/存储并发/代码可行性，2026-11-06）发现已全部采纳修订。
> 基线：design.md v2.2（2026-11-05）。开发期无向后兼容（2026-11-04 既有裁决沿用）。
> 本文替代 design.md §5.1（锁粒度）、§5.3（布局）的桶模型部分，修订 §4.4/§5.10.3/§5.10.4/§5.10.7 相关行；不涉及的部分（HTTP 栈、Lance schema、指纹、限额、agent 契约）原样有效。
> v3.0→v3.1 修订记录见 §13。

## 0. 裁决记录

| # | 裁决 | 来源 |
|---|---|---|
| D1 | vector bucket `X` ⇔ 物理桶对：**源桶 `X`（用户域）+ 系统桶 `X-vf`（vectorfs 域）**；废弃"单实例根装全部逻辑桶"布局与"物理桶/逻辑桶"双义词汇 | 用户裁决 2026-11-06（否决 opaque-placement 提案） |
| D2 | 系统桶**根即系统区**：`X-vf` 内不再有 `.vectorfs/` 内层前缀 | 用户裁决 2026-11-06 |
| D3 | 桶后缀可配置（`bucket_suffix`），**默认 `-vf`**；仅文件配置，装时刻常量 | 用户裁决 2026-11-06 |
| D4 | 源桶惰性创建；DeleteVectorBucket 删净 `X-vf` 含桶，源桶永不删 | 用户放行 2026-11-06 |
| D5 | vb 名 ≤ 63−len(bucket_suffix)，类 S3 命名但**严于通用桶规则（无点）**，不得以当前后缀结尾 | 用户放行 2026-11-06，v3.1 修正算术与措辞 |
| D6 | 锁粒度从 daemon 迁到**桶**；daemon 级 LOCK 退役 | 设计推导，v3.1 补齐收尾语义（§5.1） |
| D7 | Create/Delete 的崩溃重放采用**空桶收养**规则，无 tombstone | v3.1 审查修订（§5.2） |
| D8 | 所有权判据 = 部署身份（region+account+endpoint），非 daemon 实例 UUID | v3.1 审查修订（R2） |

## 1. 病根（现行 v2.2 的三处偏离）

1. **文件命名空间挂在物理根上，不挂在桶上**（§5.3：`<root>/` 是用户文件命名空间，`scope` 从共享根认领）⇒ 同一文件可被多桶认领、`claims_key` 校验、跨桶 scope 重叠无人管。
2. **"桶"一词双义**：VF_BACKEND 的 S3 桶（部署面）vs API 的 vector bucket（数据面）。
3. **落点是 daemon 级单点**：一个实例根装下所有桶，无桶级落点概念；AWS 把落点整体藏进服务端（用户面不可见不可配）。

**病根一句话**：现行设计把"部署单元（实例根）""数据面桶（vector bucket）""文件命名空间（根）"三个角色压进了一个概念。

## 2. 外部事实基线（三路审查复核版）

| 事实 | 判定 | 出处 |
|---|---|---|
| vector bucket 名 3–63、小写/数字/连字符（**不允许点**）、首尾字母数字——严于通用 S3 桶规则 | 真 | AWS API 参考（Length Constraints）+ user guide |
| S3 Vectors **不暴露落点**：向量数据由服务托管写入 S3，用户面不可见、不可配；wire API（CreateVectorBucket）无任何放置参数 | 真（v3.0 曾误引不存在的 `s3Prefix` 字段，已纠正） | CreateVectorBucket 响应仅 `{vectorBucketArn}`；VectorBucket 类型成员无落点字段 |
| 下划线桶名：AWS S3 / R2 / **MinIO** / **SeaweedFS** 均拒绝（MinIO 无放宽开关）；**Ceph 默认拒绝**（需 `rgw_relaxed_s3_bucket_names` opt-in）。**PR2 实测回填（2026-11-06）**：SeaweedFS **4.50** 对下划线桶名实测返回 400 InvalidBucketName——本表原行"仅 SeaweedFS 默认宽容"过时（审查 A 引的是源码 IsValidBucketName 的 lax 注释，属 3.x 旧况；**以 4.50 实测为准**，目标后端全集拒下划线，R6 单一规则再无例外） | 真 | S3 命名规则、R2 文档、minio#9088、Ceph bucketops、SeaweedFS **4.50 实测**（PR2 dockerized conformance） |
| DeleteVectorBucket 对非空桶（含 index）返回 **ConflictException**（409） | 真 | AWS API 参考 + 实测记录；与 design.md 附录A（Delete 特有错误=Conflict）一致 |
| CreateVectorBucket 对已存在名字返回 **ConflictException**（含自有桶重放） | 真 | AWS API 参考 |
| Ceph RGW S3Vectors：默认 vector bucket 落在与 S3 桶分离的命名空间；opt-in 1:1 落进**预建**同名桶，落点**创建时钉死** | 真 | Ceph docs |
| S3 Vectors 2025-07 预览、2025-12 GA；协议基线 `s3vectors-2025-07-15` 不变 | 真 | AWS What's New |

## 3. 核心裁决（R1–R10）

**R1 命名映射**：vector bucket `X` ⇔ 源桶 `X` + 系统桶 `X + bucket_suffix`（默认 `X-vf`）。三后端同构：s3 = 真 S3 桶对；file = `<data-dir>/X/` + `<data-dir>/X-vf/`；memory = 共享 InMemory 键空间内 `X/…` 与 `X-vf/…`。

vb 名规则：`^[a-z0-9][a-z0-9-]*[a-z0-9]$`，长度 3..=63−len(suffix)（默认 `-vf` ⇒ **60**），不得以当前后缀结尾（防 `X-vf-vf` 递归歧义；AWS 允许此类名，属登记偏离）。index 名规则不变。

**R2 所有权 marker（D8）**：`X-vf/bucket.json` 记 **deployment identity（region + account + endpoint）**、创建时间、placement。**"我方" = marker 的 deployment identity 与本 daemon 配置一致**——daemon 实例 UUID 只用作 LOCK 的 writer id（现任者标识），与所有权解耦。无状态再部署（容器重建/换机/丢本地卷）凭同一 deployment identity 即可认领全部既有系统桶。`X` 源桶永不需要 marker——`X-vf` 存在且带我方 marker ⇔ vb 存在；无 `X-vf` 的 `X` 桶只是普通用户桶，零误判。

**R3 生命周期**：
- `CreateVectorBucket(X)`：raw CreateBucket 建 `X-vf` → 写 marker + LOCK 对象。**不建、不要求源桶 `X`**。
- **创建重放 = ConflictException（对齐 AWS，含自有桶）**；message/hint 区分两种情形：marker 在（我方存量，hint=先 Delete 或直接使用）vs 无 marker。**空桶收养规则（D7）**：`X-vf` 存在但**空、无 marker、无 LOCK** ⇒ 判定为崩溃残留，收养（写 marker+LOCK）——这是 Create 中途崩溃（CreateBucket 成功、marker 未写）与 Delete 尾窗崩溃的收敛路径，无需 tombstone；收养空的外人 `-vf` 桶的风险接受并文档明示（桶为空，无数据可损）。非空无 marker ⇒ ConflictException fail-loud。
- 源桶惰性创建：首个 PutSource/`vf put` 时 HeadBucket 探测，无则 raw CreateBucket；**CreateBucket 败于他人建桶 ⇒ adopt**（良性竞态）。存在即接管（adopt），零拷贝零搬移。
- `DeleteVectorBucket(X)`：桶内有 index ⇒ **ConflictException**（AWS 实测语义，v3.0 误写 ValidationException 已纠正）；持锁后按**规定删除序**：清空 marker/LOCK 之外全部对象 → 删 marker → 删 LOCK → 立即 raw DeleteBucket；DeleteBucket 失败（BucketNotEmpty/NoSuchBucket）⇒ 重读重评的收敛循环。**Delete 尾窗豁免**：重放 Delete 时若 LOCK 缺失且桶内仅剩删除残留，允许无锁判定+清尾（§5.3-4 的显式豁免）。**源桶 `X` 永不删**。

**R4 wire/API 面零变化**：21 op 原样。超集仅加一个只读回显：GetVectorBucket / ListVectorBuckets 响应成员 `x-vf-placement`，取值 `s3://X-vf@endpoint` | `file://<data-dir>/X-vf` | `memory://X-vf`。正当性 = **自托管透明性**（`aws s3 ls` 读出拓扑；§8 已有论据），不对标 AWS（AWS 面完全不暴露落点）。PutSource 落源桶，GetSourceData 透传读源桶。

**R5 文件层换轴**：`scope`/`claims_key` 整体删除——源桶整体就是该 vb 的命名空间，桶间零共享是物理事实。扫描 = 整个源桶分页 list-and-diff（源桶内无任何系统对象需要排除）；可选 `include`/`exclude` glob 仅是桶内认领过滤（省缺 = 整桶）。**GC/对账作用域 = 本 daemon 持锁的桶集合**（`[ingest.X]` ∪ 惰性获取且仍持有）——orphan sweep 与任何跨桶写以此为界，绝不触碰未持锁的桶（多 daemon 各管各的）。**孤儿判定的 live 集恒为整桶列表**（include/exclude 收窄不得触发 mass-flip——仅作用于认领与 vanish-flip）。**GC 双护栏**：①对象须连续 2 个扫描周期缺席（N-strike）才 flip DELETING；②`live==0 && 标记数>0` ⇒ 拒绝 reap + 响亮告警（防异常空 listing 造成全量误删 chunk + 重嵌风暴）。指纹、无队列、幂等 key、批量纪律原样（指纹本就不含 scope，删除无 reindex 冲击）。

**R6 配置面**：
```toml
# vectorfs.toml（daemon 级，唯一配置文件；位置不变 <data-dir>/.vectorfs/vectorfs.toml）
bucket_suffix = "-vf"              # 省缺即此值。校验：^[a-z0-9][a-z0-9-]*[a-z0-9]$，len 2..=16
                                   # （单一可移植规则：连字符全域合法；_ 与 . 在 AWS S3/R2/MinIO 非法，
                                   #  Ceph 默认亦非法——不放行）
                                   # 仅文件配置（无 env/flag）：命名空间身份常量，非调优旋钮。
                                   # 装时刻常量：改后缀 = 换命名空间，旧系统桶全体从枚举消失（惰性数据
                                   # 不删）；换回即恢复。不做漂移检测引擎。

[ingest.docs]                      # vb "docs" ⇔ 桶 docs（源）+ docs-vf（系统）
extract = { pdf = ["insp", "pdf"], default = "auto" }
chunk = { kind = "token-window", size = 512, overlap = 64 }
embedders = ["text"]
merge = "rrf"
# include = ["**.pdf"]             # 可选桶内认领过滤；省缺 = 整桶
# exclude = ["**.tmp"]
```
- `VF_BACKEND` 收敛为 **endpoint 型**：`file`（省缺）/ `memory` / `s3+http(s)://host:port`。带桶路径段 ⇒ 启动 fail-fast 报 v3 形态。
- `--data-dir` 语义：daemon 本地家（`<data-dir>/.vectorfs/`：vectorfs.toml + models）；file 后端时**兼任桶根**（`X/`、`X-vf/` 与 `.vectorfs/` 并列）。
- `[ingest.X]` 删 `scope` 字段（两段式严格解析硬拒旧配置，fail-fast 带 TOML 行号——符合预期）。字段解析地 = `pipeline/config.rs`（非 server config.rs，见 §11）。

**R7 锁：单写者粒度 daemon → 桶（D6，收尾语义见 §5.1）**。

**R8 raw S3 管理面（新增底座，唯一真实成本）**：object_store 0.14 无桶管理 API（已核实）。自研最小签名出站模块（新 `crates/server/src/s3admin.rs`）：`create_bucket` / `head_bucket` / `delete_bucket` / `list_bucket_names`。**已核实可复用**：`crates/common/src/sigv4.rs` 纯函数（verify 收元组、signer 本就是 vf 客户端生产签名器、service 参数化）；HTTP 走 `crates/common/src/http.rs`（hyper-util + rustls 叶子，服务端已直依）。按 M0 纪律做后端 conformance：SeaweedFS / R2 / MinIO / Ceph 各验 create/head/list/delete + 权限错误 + 名字规则差异 + CreateBucket 幂等性分岔（us-east-1 200 vs 409 BucketAlreadyOwnedByYou vs MinIO 409——代码按"创建后 HeadBucket+marker 判定"收敛，不依赖错误码分岔）。

**R9 ListVectorBuckets 枚举 = 后缀扫描，无中心 registry**：s3 = ListBuckets 过滤 `ends_with(suffix)` + 逐候选 HeadBucket marker 校验（无 marker 的他人 `-vf` 桶跳过）；file = read_dir 同规则；**memory = 同一后缀扫描实现（键空间前缀 list），不用进程内注册表**（消除第二份真值）。nextToken 跨调用期间他 daemon 建删桶 ⇒ 尽力而为分页，文档化。支持不相交 daemon 集。

**R10 一切不列明的原样保留**：Lance schema/条件写矩阵/flat-first、管线指纹与 reindex、毒丸防护、错误模型与 agent 契约、SigV4 验签、preset 物化、models 目录、MCP 工具集、CLI 命令形状、Docker healthcheck。缓存键控补一句：**缓存键须含桶/数据集身份**（Lance 数据文件 per-dataset UUID 命名，实测无碰撞，但显式化防实现走样）。

## 4. 布局 v3

```
S3 endpoint（或 <data-dir>/ 作桶根，或 memory）         <data-dir>/.vectorfs/（daemon 本地家）
├── docs/                          ← 源桶：纯用户命名空间        ├── vectorfs.toml
│   ├── manual.pdf                   （零系统对象；               ├── models/
│   └── inbox/report.docx              扫描=整桶）                └── flock（本地同机双启锁）
└── docs-vf/                       ← 系统桶：根即系统区（D2）
    ├── LOCK                       ← 条件写+心跳+接管（机制原样，粒度=桶）
    ├── bucket.json                ← 所有权 marker（deployment identity）+ placement
    ├── policy.json  tags.json
    ├── sources/<keyHash>.json
    └── indexes/<ix>/{index.json, tags.json, data/}   # Lance dataset 原样
```

保留字：① file 后端 data-dir 根级 `.vectorfs/`（daemon 家）；② vb 名不得以后缀结尾；③ 系统桶根级 `LOCK`/`bucket.json`/`policy.json`/`tags.json`/`sources`/`indexes` 全为机制固定位，用户输入到不了桶根，无碰撞面。源桶无任何保留字。

系统相对键全部桶根相对化（`LOCK`、`bucket.json`、`sources/…`、`indexes/…`）。`SYSTEM_PREFIX` 常量从对象键路径退役，仅剩 daemon 本地家三处：`config.rs`（配置路径）、`pipeline/models.rs:133`（默认 models 目录）、`backends.rs::flock_path`。

**memory 后端重构**（审查 B-F9）：Lance dataset URI 的 host 段从固定 `vf/` 镜像前缀改为**桶名**（`memory://docs-vf/indexes/…` ⇒ map 键恰为桶相对键），删除 `vf/` 镜像前缀与 `delete_index` 的镜像特判——三后端桶相对键集逐字节同构（§5.3-5）方才成立。

## 5. 一致性与并发

### 5.1 锁模型（D6 完整版）

- **每活动 vb 一把 LOCK**：`X-vf/LOCK`，条件写创建 + 心跳续期 TTL + writer id（daemon 实例 UUID）+ 接管 put-if-match——机制原样，粒度换轴。
- **获取时机**：启动为全部 `[ingest.X]` 对应 vb 取锁并起 ingest worker；**启动取锁失败 = 只跳过该桶 worker + stderr 告警 + doctor 红**，其余桶正常服务，协议面对该桶照常 ConflictException（不拖死整个 daemon）。协议面操作惰性取锁：首次触碰某 vb 时获取，**空闲释放**（TTL 内无该 vb 操作即释放；长持仅限 `[ingest]` 归属桶）——防纯协议网关 daemon 永久压制他人桶。
- **失去某桶锁 ⇒ 仅该桶泊车**：该桶全部写路径立即 409/503，其它桶不受影响；在飞 ingest 作业**提交前强制复核锁归属**；心跳循环并行化（串行 100 桶 × ~150ms ≈ 15s 逼近 TTL/3 预算）。多把锁不被最弱一把绑架。
- **本地 flock 原样**（锁 daemon 本地家）。同机多 daemon：file 后端桶在 data-dir 之下，同 data-dir ⇒ 共享桶集 ⇒ flock 正确拒绝；不同 data-dir ⇒ 桶集天然不相交，零冲突。多 daemon 部署文档写明：**纯协议面多 daemon 需外部按桶分流**（惰性锁是执行面不是声明面）。
- **MCP 模式**：与协议面同规（惰性取锁、空闲释放）；`--tools ro` 免写锁（读走 Lance manifest 快照）。
- **创建竞态**：两 daemon 同时 CreateVectorBucket(X) ⇒ raw CreateBucket 全局唯一，一胜一败；败方按空桶收养/ConflictException 规则收敛（§5.2）。
- **登记偏离**：多 daemon 同 endpoint 拓扑下，任意协议面 op 可返回 ConflictException（携带 writer id/起始时间）——AWS 单写者场景不出现此错误（§7 偏离行）。

### 5.2 竞态与不变式

| 场景 | 行为 |
|---|---|
| CreateVectorBucket 时 `X-vf` 已存在（非空、无 marker） | ConflictException，hint 指向 bucket_suffix（撞名出路） |
| CreateVectorBucket 时 `X-vf` 已存在（marker 在） | ConflictException（对齐 AWS；hint=存量桶可直接使用或先 Delete） |
| Create 中途崩溃（CreateBucket 成功、marker 未写） | 重放 ⇒ 空桶收养（D7）：写 marker+LOCK，幂等收敛 |
| Delete 中途崩溃（marker 已删、DeleteBucket 未发） | 重放 Delete ⇒ 无锁清尾（空桶直删）；重放 Create ⇒ 空桶收养——两个方向都收敛，无 tombstone |
| Delete 尾窗他 daemon 惰性取锁 | LOCK 删除与 raw DeleteBucket 之间的窗口由删除序压缩（清空→删 marker→删 LOCK→立即 DeleteBucket）；DeleteBucket 收 BucketNotEmpty ⇒ 收敛循环重读重评；取锁者在垂死桶内的操作全部 404（marker 已删），且空闲释放最终放手 |
| PutSource 时源桶不存在 | 惰性 CreateBucket；败于他人建桶 ⇒ adopt |
| 他人桶恰好以 `-vf` 结尾（无 marker） | 枚举 marker 校验跳过；Create 同名 vb 按非空无 marker ⇒ ConflictException |
| 后缀配置漂移 | 装时刻常量语义，文档写明（§R6） |
| raw CreateBucket 收 InvalidBucketName | ValidationException，hint 指向 bucket_suffix |
| GC 遇异常 listing（成功但空/漏列） | N-strike(2) + live==0&&标记>0 拒 reap 双护栏（R5） |

### 5.3 不变式（审查与测试的锚点）

1. 任一 vb 的全部**持久**自有状态恰在 `X-vf` 一个桶内；源数据恰在 `X` 一个桶内（本地缓存页是派生副本，不算第三处）。
2. `X-vf` 存在且含我方 marker ⇔ vb 存在（"我方"= deployment identity 匹配；崩溃残留按 §5.2 收养规则收敛后成立）。
3. 源桶内容物 ⊆ 用户对象（vectorfs 永不写源桶系统对象；G1 加直接断言）。
4. 单 vb 任一时刻至多一个写者（**显式豁免**：Delete 尾窗按 §5.2 规定删除序与无锁清尾规约）。
5. 三后端下，桶相对键集逐字节同构（memory 的 `vf/` 镜像前缀随本设计删除；G1 按三后端断言）。

## 6. 接触面变更规格

| 面 | 变更 | 不变 |
|---|---|---|
| wire（21 op） | 零 | 全部 |
| 超集 | `x-vf-placement` 回显；PutSource 落源桶 + 惰性建桶 | 8 op 形状、x-vf-src-*、错误契约 |
| 存储 | `Backend::S3` endpoint 型（去 bucket/prefix）；per-vb store 句柄（源桶 store + 系统桶 store）；`user_object_key(vb, key)` 按桶寻址；系统键桶根相对；memory `vf/` 镜像删除 | object_store 路线、条件写矩阵 |
| 配置 | `bucket_suffix`（pipeline/config.rs + config.rs 两处解析链）；VF_BACKEND endpoint 型；ingest 去 scope 增 include/exclude；vb 名校验收紧 | 解析链 flag > VF_* > toml > 默认 |
| ingest/GC | 整源桶扫描；GC 作用域=持锁集；N-strike+零活护栏；排除清单随桶动态化（file 后端桶目录互为邻居） | 周期 30s、无队列、指纹、幂等 key |
| CLI/MCP | `vf put f.pdf docs/pre/` → `s3://docs/pre/f.pdf`；MCP 锁语义 §5.1 | 命令面、agent 契约 |
| status/doctor | status.rs：LOCK 探针换 per-vb、体积核算按桶对、backend 文案；doctor per-vb 探针（双桶可达、marker 归属、LOCK 持有者） | 既有探针 |
| Docker/compose | `VF_BACKEND=s3+http://minio:9000`（注释示例更新）；注记改"每 endpoint 按桶划分" | 卷、preset、healthcheck |
| workflows | ci.yml/release.yml **无**带桶路径用法（已核实），无需改；发布冒烟增双桶断言；`scripts/cli_smoke.sh` 去 scope + 双桶断言 | 其余 CI 面原样 |

## 7. 与 AWS 对齐表修订（替换 design.md §4.4 的 bucket 行）

| AWS 概念 | v3 实现 | 说明 |
|---|---|---|
| vector bucket | 源桶 `X` + 系统桶 `X-vf`（命名约定即放置，零配置） | 名 3..=63−len(suffix)、类 S3 命名无点、非后缀尾；ARN 自造原样 |
| 落点不透明（服务托管，用户面不可见不可配） | `x-vf-placement` 超集回显 | **自托管透明性**是差异化选择，非对齐项（§4.8-②出口合规） |
| CreateVectorBucket 无放置参数、重放=Conflict | 同（空桶收养是崩溃收敛内部规则，不改变对外行为） | |
| DeleteVectorBucket 非空桶=ConflictException | 同 | v3.0 误写 ValidationException 已纠正 |

limits 对照表偏离行（合计三条）：①vb 名上限 63−len(bucket_suffix)（默认 60）；②vb 名不得以 bucket_suffix 结尾（AWS 允许）；③多 daemon 拓扑下协议面 op 可返回 ConflictException（AWS 不出现）。

## 8. 被否替代方案（记录否决理由，防回潮）

| 方案 | 否决理由 |
|---|---|
| opaque per-bucket 落点（目录式，AWS 同构） | 治混淆不治隔离；自托管世界透明性/可运维性 > 协议纯粹性 |
| 1:1 同名真桶（无后缀分离） | 源/系统所有权无从切分：Delete 两难、存量同名桶收编歧义 |
| prefix 模式（同根桶内前缀） | 双路径双测试面；真撞桶数上限再裁决 |
| 共享根 + scope 互斥校验 | 保留"根是命名空间"病根 |
| daemon 级 LOCK 叠加桶级 LOCK | 无新保证，纯复杂度 |
| 后缀走 env/flag | 身份常量进 env 易被顺手翻动 |
| 中心 registry 对象 | 中心写点破坏不相交 daemon 集 |
| 本地 intent journal（tombstone）做崩溃收敛 | 空桶收养规则（D7）已覆盖两个崩溃窗口，更少状态 |
| 所有权锚定 daemon 本地持久 UUID | 破坏 S3 后端无状态再部署；换机即失全部系统桶 |
| GC 真源单押 list 可用性、无护栏 | 异常空 listing ⇒ 全量误删 chunk + 重嵌风暴 |

## 9. 风险登记

| 风险 | 处置 |
|---|---|
| raw 管理面逐后端差异（CreateBucket 幂等分岔：us-east-1 200 / 409 BucketAlreadyOwnedByYou / MinIO 409） | R8 conformance 矩阵逐后端钉死；代码按"创建后 HeadBucket+marker 判定"收敛。**PR2 实测回填**：SeaweedFS 实测=**409 BucketAlreadyOwnedByYou**（4.50，自有桶重建；s3admin 折叠为 AlreadyExists） |
| 桶数上限（AWS S3 账号软限 1000；每 vb ≤2 桶） | 文档注明主战场=自托管端点；惰性源桶把纯协议用户降为 1 桶 |
| ListBuckets 权限（部分后端需显式授权） | doctor 探针覆盖；文档部署前置条件。**PR2 实测回填**：SeaweedFS 4.50 桶列表**账号作用域**——匿名端点（未配 IAM）ListBuckets **恒返回空**，不报错也不见桶 ⇒ **R9 s3 枚举需凭据**：部署必须配置 IAM 用户/凭据（README/compose 部署前置条件），匿名端点 `vf` 枚举=空列表且 marker 校验无从发生 |
| ListBuckets 全端点扫描 + 逐候选 HeadBucket 的往返放大（500 桶 ⇒ 500+ 次远端往返） | ListVectorBuckets 上限 500/页本就存在；文档注明大端点部署的枚举成本 |
| `_`/`.` 后缀回潮 | §2 事实行 + R6 单一可移植校验规则双重钉死 |

## 10. 验收门

1. **G1 存储轴（file/memory 全量）**：workspace 编译绿；file+memory 全量测试绿（含重锚后的 ingest/ops/pipeline 测试）；§5.3-1..5 不变式断言测试（三后端逐字节键同构、源桶零系统对象、marker 身份判定）；s3admin 签名向量单测。
2. **G2 生命周期与 s3**：Create/adopt/惰性源桶/删净全路径；**崩溃重放矩阵**（kill -9 切点：CreateBucket 后 marker 前、删 marker 后 DeleteBucket 前）；双 daemon 不相交桶集 + 锁冲突 409 测试；s3 conformance（dockerized SeaweedFS + MinIO，双桶断言）。
3. **G3 文件层**：整桶扫描/GC 换轴全量测试；include/exclude 收窄不 mass-flip 断言；**空 listing 故障注入**测试（N-strike 护栏）；memory 桶间隔离断言。
4. **G4 文档与发布面**：design.md 合并、README、deploy、cli_smoke.sh、发布冒烟（ubuntu:20.04 容器双桶全链路）。

## 11. 迁移清单（代码接触面，v3.1 按审查 C 补全）

| 文件/目录 | 动作 |
|---|---|
| `crates/server/src/s3admin.rs`（新） | raw Create/Head/Delete/ListBuckets，sigv4 出站签名（复用 common/sigv4.rs + common/http.rs，已核实零新依赖） |
| `crates/server/src/backends.rs` | Backend::S3 endpoint 化；per-vb store 构造；系统键桶根相对；memory `vf/` 镜像删除（含 store.rs delete_index 特判清理） |
| `crates/server/src/store.rs` | per-vb 句柄；create/delete_bucket 生命周期 R3（含删除序）；ListVectorBuckets 枚举 R9 |
| `crates/server/src/lock.rs` | per-bucket LOCK（多 guard 注册表、每 guard 心跳、空闲释放）；serve 接线 = `bin/vectorfs.rs:750/:870-916`；mcp 接线 = `mcp.rs:787/:875-938`；失去单桶锁 ⇒ 仅该桶泊车 |
| `crates/server/src/config.rs` | bucket_suffix；VF_BACKEND endpoint 解析；vb 名校验 |
| `crates/server/src/pipeline/`（config.rs 等，scope 真正解析地） | scope 删除；include/exclude 新字段；配置校验迁移 |
| `crates/server/src/ops.rs` | Create/DeleteVectorBucket 生命周期；x-vf-placement；锁获取时机 |
| `crates/server/src/ops_xvf.rs` | PutSource 落源桶 + 惰性建桶；GetSourceData 源桶透传 |
| `crates/server/src/ingest/*` | scope/claims_key 删除；整桶扫描；排除清单随桶动态化；GC 作用域=持锁集；N-strike+零活护栏 |
| `crates/server/src/status.rs` | LOCK 探针 per-vb；体积核算按桶对；backend 错误文案 |
| `crates/server/src/mcp.rs` | 锁接线 per-vb；工具描述去 "data root" 根假设 |
| `crates/server/src/doctor.rs` | per-vb 探针；backend 帮助文案 |
| `crates/server/src/experiments.rs` | S3BackendConfig 结构体字面量 + `VF_S3CP_URI` 桶路径改 endpoint 形态（PR1 编译闭包内） |
| `crates/server/src/bin/vectorfs.rs` | 锁接线重做；--backend 帮助文本；init 物化模板 |
| `crates/server/src/preset.rs` | 模板去 scope |
| `crates/server/tests/*`（~150+ 用例持轴假设） | 全面重锚（scope fixture、`.vectorfs/…` 键断言、LOCK 物理路径断言 cli_smoke.rs:751） |
| `scripts/cli_smoke.sh` | 去 scope（:70-76 现含 `scope = "docs/**"`，启动即 fail-fast）；双桶断言 |
| `deploy/docker-compose.yml`、`README.md` | VF_BACKEND 注释示例；桶模型叙述 |
| `.github/workflows/{ci,release}.yml` | 无带桶路径用法（已核实），仅发布冒烟增双桶断言 |
| `crates/client/*`、`conformance/`（boto3/go 对拍） | 零布局假设，无需改（wire 面不变则继续绿）；G2 增双桶场景脚本 |

## 12. PR 序（v3.1 按编译闭包重切，子代理实施顺序，串行）

> v3.0 原切法（PR1 不动 ops/ingest）被审查 C 证伪：`Backend` 形态变更的编译闭包横穿 bin/ingest/domain/experiments，且布局一换 ingest 扫描会把 `X-vf/sources/*.json` 当用户文件。重切如下，**每个 PR 结束时 workspace 编译绿 + 该 PR 声明的测试面全绿**：

1. **PR1 全轴换（file/memory 收敛）**：§11 全部代码面一次性换轴（s3admin + backends/store/lock/config/pipeline/ops/ops_xvf/ingest/status/mcp/doctor/experiments/bin/preset + tests 重锚 + cli_smoke.sh）；验收 = G1（workspace 编译绿、file+memory 全量测试绿、不变式断言）。
2. **PR2 生命周期边缘 + s3 conformance**：崩溃重放矩阵、双 daemon 测试、SeaweedFS/MinIO dockerized conformance、release 冒烟双桶断言；验收 = G2。
3. **PR3 文档与发布面**：design.md 合并（§7 行替换 + 偏离行）、README、compose、§0 裁决记录回填；验收 = G4。

## 13. v3.0 → v3.1 修订记录（三路审查发现采纳）

| 审查 | 发现 | 修订 |
|---|---|---|
| A-F1 | `s3Prefix` 字段在 s3vectors-2025-07-15 模型中不存在（v3.0 误引） | §2 改为"落点不透明"；R4/§7 正当性改挂自托管透明性 |
| A-F2 | Delete 非空桶 AWS 返回 Conflict 非 Validation | R3 纠正 |
| A-F3 | Create 重放 AWS 返回 Conflict，v3.0 误读基线 §5.1 | R3 改 Conflict + hint 分型；与 D7 空桶收养并存（收养是崩溃收敛内部规则） |
| A-F4 | MinIO 拒下划线、Ceph 默认拒（v3.0 称"宽容"为假） | §2 纠正；R6 校验收紧为单一可移植规则 |
| A-F5/F8 | 63−3=60 非 59；vb 命名严于通用 S3（无点） | R1/D5 纠正 |
| B-F1 | LOCK 在被删桶内，Delete 尾窗破坏单写者 | R3 规定删除序 + 无锁清尾；§5.3-4 显式豁免 |
| B-F2 | 无 marker 崩溃窗口产生 Conflict 死端 | D7 空桶收养 |
| B-F3 | 所有权锚本地 UUID 破坏无状态再部署 | D8 deployment identity |
| B-F4 | 惰性锁持有至停机压制他人桶；启动部分失败未定 | §5.1 空闲释放 + 跳过不拖死 |
| B-F5 | 多锁接管栅栏未指定 | §5.1 按桶泊车 + 提交前复核 + 心跳并行 |
| B-F8 | GC/orphan sweep 会跨锁写他 daemon 桶 | R5 作用域=持锁集 |
| B-F9 | memory 注册表自违 R9；`vf/` 镜像破同构 | R9/§4 memory 重构 |
| B-F10 | 异常空 listing ⇒ 全量误删 chunk | R5 双护栏 |
| B-F11 | include/exclude 收窄不得触发 mass-flip；nextToken 稳定性 | R5 显式化；R9 尽力而为分页 |
| C-F1..F4 | 清单漏 pipeline/、status.rs、mcp.rs、experiments.rs | §11 补全 |
| C-F5 | PR1 编译闭包不成立 | §12 重切 |
| C-F6/F7/F9/F10 | workflows 事实纠正；cli_smoke.sh 遗漏；SYSTEM_PREFIX 三处残留；memory 排除面 | §6/§11/§4 对应修正 |

## 14. 实施裁决记录（2026-11-06，回填规格：PR1 + PR2）

1. **R6 后缀正则自纠**：字面 `^[a-z0-9]…` 拒绝默认值 `-vf`（首字符连字符）。实施定稿：首字符 `[a-z0-9-]`、结尾必须字母数字（保证 `X+suffix` 恒为合法桶名），len 2..=16——本表为准，§R6 字面正则作废。
2. **D7 收养条件精化**："空、无 marker、无 LOCK" 放宽为"**仅含机制键**（LOCK/policy/tags/sources 之下）且无 marker"——惰性 pin 锁可在 marker 写入前合法物化 `X-vf`+LOCK，字面"空"会误伤合法时序；indexes/ 或未知对象仍 Conflict。
3. **启动锁跳过自愈**：heartbeat 轮每 TTL/3 重试 pinned 桶，垂死锁过期后 worker 自动恢复，无需重启。
4. **s3 create 顺序**：raw-create 先行、后探测（R8 "创建后 HeadBucket+marker 判定" 的字面收敛；先探测在真实端点必败）。
5. **锁实现级修复**：空闲释放同时删 LOCK 对象（否则留 fresh-heartbeat 孤儿锁钉桶一整个 TTL）；同 manager 并发首次取锁互认（owner_uuid 相同返回视图 guard）。
6. **flock 时序**：实际调序为 flock 最先（原代码注释称 FIRST 但实现在锁 pin 之后）。
7. **cli_smoke 断言排序**：`LC_ALL=C sort`（UTF-8 collation 下 `.vectorfs` 排序位置不同）。

PR2（生命周期边缘 + s3 conformance）：

8. **fingerprint_gate 对尚无系统桶的 vb 短路通过**：s3 新部署先在 vectorfs.toml 名 `[ingest.X]` 后跑 CreateVectorBucket 时，ListObjects 对不存在桶答 **404 NoSuchBucket**（file 后端同态=空目录列表）——不短路则 init→serve 在全新端点**必然拒启**。实施：`system_bucket_exists(vb)` 探测不存在 ⇒ 该桶无 marker 可漂移，gate 平凡放行（启动锁跳过与扫描 no-listing 守卫本就容忍缺失桶，§5.1）。
9. **`VF_TEST_CRASH` 崩溃注入缝**（store.rs，**test-only**）：`VF_TEST_CRASH=<切点名>` 命中即进程硬 abort——真崩溃语义（无优雅停机、无 LOCK 释放），在存储上留下恰为 §5.2 行 3/4 的现场供重放矩阵消费（tests/lifecycle_crash_replay.rs）。两个切点名：`create_after_raw_create`（raw CreateBucket 已成、marker 未写）与 `delete_after_marker`（marker+LOCK 已删、raw DeleteBucket 未发）。生产不设此 env 时成本 = 每次生命周期 op 一次 env 查找（create/delete 是低频管理动词，非每请求路径）。
10. **MinIO 无社区发行版**：docker hub pull denied（与 spike/REPORT.md 的 MinIO 社区版归档记录一致）⇒ R8 conformance 矩阵 **MinIO 行待 CI 端点**（矩阵行保留不删；G2 以 dockerized SeaweedFS 4.50 首行落地，PR3 起进 ci.yml）。
