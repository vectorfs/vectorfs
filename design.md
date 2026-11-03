# vectorfs 设计文档 v1.0

- 日期：2026-11-03
- 状态：M0 spike 前设计基线。§5（架构）与 §7（里程碑）中标注 *spike* 的决策以实测为准。
- 文档纪律（G6）：本文件是唯一长期设计文档；spike 结束后按实测结论修订并裁剪，文档总量恒 ≤ 代码量。

## 0. 裁决记录（2026-11-03，本日拍板）

| 事项 | 裁决 | 备注 |
|---|---|---|
| 方向 | **不重写存储后端**（用户原话："我们不可能重写整个 S3 后端"）。vectorfs = s3vectors 协议层 + 向量引擎网关，持久层 = 可插拔对象存储（S3 兼容远端 / 本地目录 / 内存） | 起因：验证③发现 SeaweedFS 4.52（2026-11-02）已发布 s3vectors 网关、Ceph RGW 已合并主干、Ozone 在途——"唯一开源实现"身位已失；"后端无关薄网关 + 本地模拟器"位仍无人占 |
| K0 治理 | **不做范围冻结**；分步执行、每步验证（门槛见 §7/§10）；借助 coding agent 推进 | 负面清单（§2）与"每步发布即验收"（G7）仍然有效 |
| 命名 | **保留 vectorfs** | GitHub org 已占位（今日创建）；crates.io/PyPI/npm 全空（2026-11-03 撞名审查）；"fs"语义税由 README 第一句承担 |
| 阿里别名层 | **废弃**，只实现 AWS 面 | 理由见 §4.7 与附录C |
| 三验证 | ①协议面 PASS（21 操作、rest-json、SigV4 已核实）②落地帖待发（稿已备）③竞争部分翻车（见附录D），触发本日方向改写 | 详细竞争快照存附录D，未来会话不必重查 |
| M0 spike | **2026-11-03 完成，验收门 PASS**：boto3 与 Go SDK v1.17.1 直连 file:// 与 S3 后端各 9/9（SigV4 验签开启）；零 AWS SDK 的 S3 注入路线走通；数据见 `spike/REPORT.md` | 本文件已按其修订：体积预算 155/175、后端矩阵实测化、写路径三步序、optimize 升 M1 必做 |

## 1. 定位

**vectorfs：单二进制、自托管、AWS S3 Vectors 兼容的向量存储网关——把任何 S3 兼容对象存储变成 Vector Bucket；指向本地目录/内存时即开即用的开发与 CI 模拟器。**

- 英文一句话（README/发布物用）：*Turn any S3 into S3 Vectors. Single binary. Point your AWS SDK at it.*
- v1 不含任何嵌入模型：存的是客户端发来的向量（协议服务的天然边界）。

### 1.1 邻居地图（2026-11 快照）

| 邻居 | 形态 | 与我们的关系 |
|---|---|---|
| AWS S3 Vectors / 阿里 OSS 向量 Bucket | 云托管服务 | 协议出处；我们是其自托管/离线形态 |
| SeaweedFS 4.52+ / Ceph RGW(master) / Ozone(roadmap) | 平台**内置** s3vectors，绑定自家存储 | 名义竞争，实际**可宿主**——**已实证（M0）**：vectorfs 跑在其 S3 面上 9/9 兼容；差异=后端无关、单二进制、秒级启动、超集 |
| MinIO / R2 / AWS S3 | 纯对象存储 | **宿主**。注：MinIO 社区版已归档（2026-02 repo archived，末版 2025-10，dl 下线）——存量部署仍可宿主，新装机流向 SeaweedFS/Garage/Silo（附录D） |
| LocalStack | 模拟器（无 s3vectors；社区版 2026-03 转 inactive） | 我们的 file:// 与 memory:// 后端 = 其 s3vectors 空位的替代 |
| pgvector / Qdrant 等 | 专用向量库，自有 API | 不同协议面；用户以"应用已按 s3vectors 编写"为准选择我们 |

### 1.2 为什么这个位置成立

1. **史实先例**：SeaweedFS 与 Ceph RGW 提供 S3 兼容多年，MinIO 仍赢得"简单自托管 S3"心智——平台加协议功能从不消灭"后端无关的专用轻量层"。
2. **结构性空位**（2026-11-03 核实）：现有开源实现无一满足 {单二进制, 秒级启动, 不绑定特定存储, dev/CI 模拟器形态}。
3. **需求侧已兑现**：Open WebUI 已内置 s3vector backend（boto3 端点可覆写，`AWS_ENDPOINT_URL` 即可指向我们）；LlamaIndex/LangChain 在 AWS 预览后 3–9 天内出集成；Bedrock KB GA 即绑定（预览期 25 万 index / 400 亿向量）。
4. **技术栈严丝合缝**：Lance 本就是对象存储原生向量格式；object_store 抽象使 file/memory/s3 三后端共享同一路径。

## 2. 永不做（负面清单，进 README）

1. **不自建对象存储后端、不自研存储引擎**——持久层永远是既有对象存储或本地目录（本日方向裁决的宪法条款）
2. 自建版本控制（documentfs D1/D4 教训）
3. 嵌入模型托管平台（v1 不自带任何嵌入）
4. 应用层 chat UI
5. 写入时实体抽取管线
6. 内核态实现
7. 多写者分布式协调（v1 单写者；见 §5.1）
8. 插件商店（除非第二维护者出现）
9. 兼容 AWS 控制台 UI
10. 阿里原生 API 面（别名层已废弃，§4.7）

## 3. 用户与验收

| 用户 | 场景 | 到达路径 |
|---|---|---|
| 用 Open WebUI / LlamaIndex / LangChain s3vectors 集成的开发者 | 本地跑通、不花钱不注册 | `vf serve` + `AWS_ENDPOINT_URL` 环境变量 |
| 在 CI 里测 s3vectors 代码的团队 | 无网络依赖、秒级启停、确定性 | memory:// 后端 + testcontainers/裸进程 |
| 自托管 MinIO / SeaweedFS / Ceph 的用户 | 数据留在自己的存储里 | `vf serve --backend s3://…` |
| 边缘 / air-gapped | 单二进制拷入即用 | file:// 后端 |

- **核心验收（M0 起）**：官方 AWS SDK（boto3 + aws-sdk-go-v2，M1 加 Python/Go 双版本交叉）未经修改直连全部操作通过。
- **G2 依赖链**：核心价值路径上的全部依赖 = 一个二进制 + 用户已有的 S3/SDK。不允许"先安装另一个系统"（file:// 为默认后端）。
- 上手承诺：从下载二进制到 SDK 查询成功 ≤ 5 分钟（文档以此写 acceptance）。

## 4. 协议兼容规格（AWS 面为准）

基线：`s3vectors-2025-07-15`（botocore develop 分支模型，同步于 2026-09-30；与 AWS CLI 参考双确认）。**恰好 21 个操作**，全集见附录A。

### 4.1 Wire 协议（已核实）

- rest-json，**path-dispatch**：每个操作 `POST /<OperationName>`（如 `POST /QueryVectors`），bucket/index 定位字段在 body。例外：`GET /tags/{resourceArn}`、`POST /tags/{resourceArn}`、`DELETE /tags/{resourceArn}`。
- 请求/响应 `Content-Type: application/json`；PutVectors / DeleteIndex / 管理类操作成功 = **HTTP 200 空 body**。
- 端点形态（我们模拟的对象）：`https://s3vectors.{region}.amazonaws.com`，host dispatch 无关紧要——本地监听任意 host。
- SigV4，signingName = `s3vectors`，区域进入 credential scope。

### 4.2 认证（服务端）

- 模式一 **static**（默认）：`vf init` 生成 access key/secret（打印一次），客户端按普通 AK/SK 配置；服务端验签（canonical request + HMAC-SHA256 链）。
- 模式二 **none**（`--no-auth`，CI 用）：跳过验签。
- 验签规则：接受 signed payload 与 `UNSIGNED-PAYLOAD`（*spike* 对 boto3/Go SDK 抓包确认实际发送形态，附录B-Q2）；X-Amz-Date 偏差窗口 ±15min；service 名须为 `s3vectors`、region 须匹配配置（默认宽容任意 region 字符串）；X-Amz-Security-Token 若配置了则须匹配否则忽略。presigned URL 不支持（SDK 对这些操作不用）。
- 实现选型（*spike*）：验签器自研（~300 行，零 AWS SDK 依赖）vs 引 `aws-sigv4` crate——以依赖体积与可测性定，倾向自研。

### 4.3 错误模型（已核实，模型实证）

- 形状名精确对齐：`ValidationException`(400)、`NotFoundException`(404)、`ConflictException`(409)、`AccessDeniedException`(403)、`ServiceQuotaExceededException`(**402**)、`TooManyRequestsException`(429)、`RequestTimeoutException`(408)、`InternalServerException`(500)、`ServiceUnavailableException`(503)、KMS×4(400，**永不触发**，见 §4.4)。**不存在** ResourceNotFoundException / ThrottlingException（前会话记录有误）。
- 发射方式按 Smithy restJson1：`X-Amzn-Errortype` 响应头 + body `__type`（形状名）与 `message` **双发**，最大化各代客户端兼容（*spike* 用三个 SDK 实测回读）。
- ValidationException 带 `fieldList[{path, message}]`（序列化校验失败时填充）。

### 4.4 语义映射表（AWS 概念 → vectorfs 实现）

| AWS 概念 | v1 实现 | 说明 |
|---|---|---|
| vector bucket | `<root>/buckets/<name>/bucket.json` | 名 3–63；ARN 自造 `arn:aws:s3vectors:{region}:{account}:bucket/…`（region/account 可配，默认 us-east-1/000000000000） |
| index | `<root>/buckets/<vb>/indexes/<ix>/` Lance dataset | 创建时定 dataType=float32 / dimension 1–4096 / metric ∈ {cosine, euclidean}，**不可改**（无 UpdateIndex，符合 AWS） |
| vector = {key, data.float32, metadata} | Lance 行：`key: utf8`(主键), `vector: fixed_size_list<f32, dim>`, `metadata: struct<…>`（动态字段，见 §5.3） | key 1–1024 字符、大小写敏感、任意 UTF-8、每索引唯一；**PutVectors = upsert**（整行替换，metadata 整体替换——*spike* 对拍 AWS 确认"省略 metadata 是否保留旧值"，附录B-Q5） |
| indexMode CLASSIC/ENHANCED | **恒 ENHANCED 行为**（预过滤：先过滤后 KNN） | UpdateIndexMode = 200 空响应；defaultIndexMode 存储并回显；这是本地实现的天然优势（无后置过滤的召回损失），文档明示 |
| metadataConfiguration.nonFilterableMetadataKeys | 记录于 index 元数据；列出的键存 JSON blob 不建标量列 | 1–10 个键，键名 1–63，声明后不可改回（对齐 AWS） |
| bucket policy | `policy.json` 存取回显 | **不实施**（本地单租户）；文档明示；M2 可选最小 Allow/Deny 按 AK principal |
| encryptionConfiguration / KMS | 接受、回显、忽略 | 静态加密是后端对象存储的职责；KMS 错误永不触发 |
| tags | `tags.json`（bucket 与 index ARN 均支持） | TagKey 1–128 / TagValue 0–256 |
| 一致性 | 写后立读强一致（§6） | 对齐 AWS "strongly consistent" 声明 |

### 4.5 限额执行策略（结构限制照抄，配额不抄）

**照抄并校验**（超限报 ValidationException / 对应错误）：PutVectors ≤500 向量且 body ≤20MiB；GetVectors ≤100 keys；DeleteVectors ≤500 keys；dimension 1–4096；metadata 每向量 ≤40KB / ≤50 键 / filterable ≤2KB / non-filterable 键 ≤10；topK ≤10,000（每页 100 + nextToken）；ListVectors maxResults 1–1000（默认 500，累计 >1MB 截断）；ListVectorBuckets/ListIndexes maxResults 1–500 + prefix；segmentCount 1–16 / segmentIndex 0–15（成对出现）。
**不执行**（本地无意义）：QPS/吞吐配额、20 亿向量、10,000 bucket/index 上限。文档以一张"limits 对照表"写明哪些相同、哪些放宽。

**向量校验**：cosine 拒零向量；两 metric 均拒 NaN/Infinity；JSON 数值入站折叠 float32（对齐 AWS 折转语义）。

### 4.6 过滤器

算子（AWS 已核实全集）：`$eq $ne $gt $gte $lt $lte $startsWith $in $nin $exists $and $or`（**无 `$nor`**——那是阿里原生 API 的）。顶层简写隐式 `$eq`；同字段多算子并列 = AND。`$startsWith` 在 AWS 仅 ENHANCED 可用——我们恒可用。metadata 值类型：string / number / boolean / list（list 与 `$eq/$in` 交集=任一元素匹配）。
*spike* 对拍项：顶层多键隐式 AND（附录B-Q4）、类型不匹配行为（Validation vs 空结果）、对 nonFilterable 键下过滤算子的错误形态。

### 4.7 阿里云面：废弃别名层，只取蓝本

阿里兼容的方向是"AWS SDK 可连阿里"（其服务端接受 AWS 客户端），而**不是**其原生 API（`POST /?queryVectorsFusion`、OSS V4 签名、三级域名 `{bucket}-{uid}.{region}.oss-vectors.aliyuncs.com` 定位）与 AWS 面互通。为阿里线格式做别名层 = 双倍协议面 + 零新用户，废弃。
保留的是 **Fusion 语义作为超集蓝本**（多路 knn、RRF/加权 retriever、$textMatch、自定义排序、分页——形态见附录C），进 M2 超集设计。

### 4.8 超集策略（差异化，但绝不污染兼容面）

- **原则**：AWS SDK 走到的路径 100% 与 AWS 语义同构；超集能力只经 ①第一方 CLI/MCP ②裸 HTTP 的扩展 body 字段（`x-vf-*` 前缀，AWS SDK 类型化客户端发不出也收不坏——未知响应成员被各 SDK 忽略）提供。
- M2 超集内容（Fusion 蓝本裁剪）：`$textMatch` 全文（CJK 零词典 bigram tokenizer，自研 ≤150 行，禁引 lindera/gse）；hybrid 多路检索 + RRF/加权合并；自定义排序；（评估）`ip` 距离。默认关闭，显式开启。

## 5. 架构

### 5.0 组件总览

```
AWS SDK / CLI(vf) / MCP 客户端
        │ HTTP (SigV4)
┌───────▼───────────────────────────┐
│ vf daemon（单二进制, 单写者）        │
│  HTTP 层 → SigV4 验签 → 路由(21)    │
│  → 校验(限额/形状) → 引擎层          │
│    查询: Lance scan/KNN(预过滤)     │
│    写入: Lance merge-insert 原子提交 │
│    目录: catalog JSON 对象(条件写)   │
│  可选本地缓存(manifest 版本键控)      │
└───────┬───────────────────────────┘
        │ object_store 抽象
  file:// │ memory:// │ s3://(minio/aws/r2; v0.2 验证 seaweedfs/ceph)
```

### 5.1 进程与锁模型

- 单二进制 `vf`（多调用：`vf serve` 等）。**每个数据根一个写者 daemon**。
- 锁：本地 flock（同机双启）+ `<root>/LOCK` 对象（条件写创建 + 心跳续期 TTL；防跨机双写）。拿不到锁 = 陈旧 daemon 检测/接管协议（启动时 LOCK 心跳过期可安全接管，需先确认旧进程死）。
- **崩溃恢复 = 客户端重试**：所有写操作 key 寻址、幂等（upsert/delete 天然幂等；bucket/index 创建冲突报 ConflictException 亦幂等）。**无 intent journal**——继承设计的"CAS parquet → SQLite intent → Lance∥tantivy → done 四步写序"整体废弃，理由：那套设计为多存储组件一致性而生，本架构里 **Lance manifest 提交就是唯一事务边界**（原子、版本化、可重放）。
- 多实例：v1 不支持多写者；读者恒读最新 manifest（Lance 快照读），跨 daemon 只读访问允许但不在支持矩阵内。

### 5.2 存储后端抽象

- **接入路线定稿（M0 实证，零 AWS SDK）**：object_store 0.14（aws feature）自建客户端 → 实现 `lance_io::ObjectStoreProvider` → 自定义 lancedb `Session` 注入。例外记录：需直接依赖 lance-io（lancedb 未 re-export provider trait）。代价：aws feature 硬绑 aws-lc-rs（cmake 构建依赖），体积 +67.7MiB——接受（风险 #11）。
- v0.1 矩阵：`file://`（默认，零配置）、`memory://`（CI/测试）、`s3://`（**SeaweedFS 4.52 已实测 9/9 + 条件写 + 延迟**；AWS S3 / R2 / MinIO 存量——If-None-Match:\* 文档核实均支持，M1 在真实云 S3 补实测 conformance）。v0.2 验证扩展：Ceph RGW、Garage/Silo。
- **条件写矩阵（M0 结论）**：LocalFS / SeaweedFS 实测，AWS / R2 / MinIO 文档核实——全部支持 If-None-Match:\*；Lance manifest 提交即走此路径 ⇒ **提交原子性在全部目标后端成立**；LOCK 对象降级保留为防御性设计，非必需。

### 5.3 数据布局与 Lance schema

```
<root>/
  LOCK                       # 写者锁（条件写+心跳）
  buckets/<vectorBucketName>/
    bucket.json              # 元数据+defaultIndexMode+encryption回显
    policy.json  tags.json   # 可选
    indexes/<indexName>/
      index.json             # dimension/metric/dataType/nonFilterable/创建时间/indexMode回显
      tags.json              # index ARN 的 tags
      data/                  # Lance dataset（目录名固定，不用 "lance" 默认名避免歧义）
```
- Lance schema：`key: utf8 not null`、`vector: fixed_size_list<float32, dim>`、`metadata: struct<动态字段>` + `metadata_json: utf8`（nonFilterable 键的原文）。
- **动态 schema 演化（M0 闭环，附录B-Q10）**：批内出现新 metadata 键 → `add_columns().transform(AllNulls(仅新子字段))` 演化 → merge_insert，三步在同一请求内完成。实证：跳过演化直接提交会被干净拒绝（fail-closed，无静默损坏）；演化后旧行 NULL、新行带值。值类型冲突（同键先 number 后 string）→ ValidationException。
- 一代一表原则保留的简化版：index 创建参数即"代"；无嵌入模型版本问题（不自带嵌入）。

### 5.4 写路径

1. 校验（限额、形状、向量合法性、metadata 键类型冲突）。
2. 单次 Lance `merge_insert("key")` 原子提交（upsert）或 delete-merge（DeleteVectors，≤500 keys）。
3. 200 空 body。
- 批量纪律：一请求一提交（严禁逐条 add——继承教训）；merge-insert 内部即批。
- **M0 实测注记**：表 >50 万行后每批提交线性变慢（100k→1M：269→333ms@file、292→408ms@s3）——`optimize`（fragment > 32 且空闲时压缩，管理命令非自动后台任务）由 M2 **提前至 M1 必做**。

### 5.5 查询路径

- **flat-first**：< 100,000 向量不建 ANN 索引，全表精确扫描（arrow 并行 + 预过滤 = 召回 100%，确定性排序 tie-break by key）。>10 万后由 `vf optimize` 建 IVF_PQ（Lance 原生），查询自动走索引。
- ENHANCED 语义 = 标量谓词下推预过滤 → KNN（Lance scan filter + vector search 一体）。
- **分页 token**：opaque base64 `{manifest_version, last_key/offset, segment}`——manifest 版本入 token = **快照隔离**（翻页期间写入不改变结果集）。
- segment：`hash(key) % segmentCount` 分片（可并行，语义与 AWS 一致：segments 划分 keyspace）。
- 距离语义：cosine = 1−相似度（M0 钉死）；euclidean 注意 lancedb 原生返回**平方 L2**，查询侧须补 sqrt（M0 发现，保序）；`returnDistance/returnMetadata` 默认 false。
- **M0 实测注记**：flat 全表扫 1M×768 = 398ms(file)/972ms(s3)——1M 规模需 IVF_PQ（M2 已规划）；spike 的 filter 走全表扫+内存过滤（1M 时 1.0–1.4s），产品实现谓词下推后过滤查询将显著下降。

### 5.6 缓存

- manifest 不可变 → 数据页本地缓存以 `{manifest 版本, 文件, 偏移}` 键控，天然一致；远端后端时缓存显著加速重复查询（*spike* 量化）。
- 缓存目录默认 `<root>/.cache/`（file 后端时退化为 OS 页缓存即够）。
- **M0 实测注记**：S3 后端与 file 的查询延迟比随规模放大（100k≈2.6x、1M≈2.4x，每查询全量拉取所致）——manifest 键控缓存在 S3 后端是**一级优化项**，非可选。另：每操作新开 lance Connection 是已识别的优化点（连接缓存）。

### 5.7 HTTP/依赖栈（v1）

| 件 | 选型 | 备注 |
|---|---|---|
| HTTP server | axum（M0 定稿：底座仅 3.4MiB，hyper 对照无意义） | 21 条平铺路由，无中间件复杂度 |
| SigV4 验签 | **自研定稿**（~350 行；双 SDK 22/22 签名验签通过；S3 出站同样经 object_store 自研签名，全链零 AWS SDK） | lancedb aws feature = 22 个 aws-\* crate + opendal，禁用 |
| 序列化 | serde + serde_json | filter/metadata 为自由 JSON Document |
| 引擎 | lancedb 0.41（锁 minor，API 只经一个内部 crate 出现） | 双 arrow 版本风险 M0 第一周 `cargo tree -d` 验证 |
| object_store | 随 lancedb | s3 特性按需开 |
| ~~rusqlite~~ | **移出 v1** | catalog 即对象存储里的 JSON；无本地持久状态（G4 赢） |
| ~~tantivy~~ | **推迟到 M2** | v1 过滤由 Lance 标量谓词承担；全文只在超集需要；顺带推迟 arrow 去重风险 |
| CLI/MCP | clap + rmcp(3.x stable) | |
| 出网 | 无（server 不发起出网请求；S3 后端除外） | |
| 其余 | cargo-deny（拒 GPL/AGPL/CC-BY-NC）+ cargo-machete + `cargo tree -d` 进 CI；release-prod profile 强制 lto=fat+cgu=1+strip（M0 实测 379→164.5MiB，datafusion 为不可裁剪的引擎固定成本）；体积 **≤155MB 警戒 / 175MB 硬顶**（S3 客户端 +67.7MiB 已含；内嵌 .so 白名单仅 v2 客户端的 ort/pdfium） | CI：ubuntu x64+arm64 + macos check + size-guard；发布冒烟在 ubuntu:20.04 容器跑全链路 |

### 5.8 客户端面（薄，v1）

- CLI：`vf init`（生成凭证/写配置）、`vf serve`（--backend/--listen/--no-auth）、`vf status`（daemon/后端/桶索引概览）、`vf doctor`（后端连通与条件写能力探测）。**不做** put/query 的 CLI 重实现——数据面走 SDK（这是"薄面"的边界；M2 超集时 CLI 增加 `vf search --hybrid` 之类第一方入口）。
- MCP：rmcp server，工具面 = list buckets/indexes / query / get / put / delete；**agent 一等契约**沿用：结构化错误 `{code, exit, message, hint}`（hint=给 agent 的行动指令）、退出码 grep 兼容、stdout=结果 / stderr=进度、`--json` 带 schema_version、零交互、幂等、确定性输出（tie-break by key）。

### 5.9 继承设计处置表

| 继承项 | 处置 | 理由 |
|---|---|---|
| Rust 单二进制 / Linux x64+arm64 首发 / macOS CI 绿灯 | 保留 | |
| Lance 基座 / flat-first / 批量提交 / fragment>32 才 optimize / RSS-CPU watchdog | 保留 | watchdog 简化为 daemon 自监控 |
| flock 拉起协议 | 保留（本地层）+ LOCK 对象（跨机层） | |
| 四步写序 + SQLite intent journal | **废弃** | §5.1：Lance 原子提交即事务边界 |
| rusqlite | **移出 v1** | catalog 即对象 |
| tantivy + CJK bigram tokenizer | **推迟 M2** | 仅超集 $textMatch 需要 |
| ort load-dynamic / models.toml / .so 内嵌释放管线 | **冻结到 v2 第一方客户端** | v1 无嵌入 |
| agent 接口契约 / `vf` 短名 / 结构化错误 | 保留 | `vf` 在 cargo/brew/apt 可用（fish 用户的 VirtualFish 冲突已知，文档注明） |

## 6. 一致性模型（卖点之一）

- **写后立读强一致**：Lance manifest 原子切换 + S3 系对象存储读写一致 ⇒ 上一毫秒 PutVectors、下一毫秒 QueryVectors 必见。对齐 AWS 官方表述 "Writes … are strongly consistent"，且**无任何最终一致窗口**（对比：阿里 Standard 模式写后约 2–3 秒可查——我们的本地实现天然免疫）。
- 单写者 ⇒ 写全序串行化。
- 分页 = manifest 快照隔离（token 携带版本）。
- **数据可退出性承诺**：数据根 = 开放格式（Lance + JSON），daemon 消失数据仍可被任何 Lance 工具读取——写进 README，是对用户的承诺也是项目的优雅退出路径。

## 7. 里程碑与验证门（K0：分步执行，每步过门才进下一步）

### M0 — 协议符合性 spike ✅ 完成（2026-11-03，throwaway crate `spike/`，不进产品）

**结果：验收门 PASS**——双官方 SDK 直连 file:// 与 S3(SeaweedFS 4.52) 后端各 9/9（验签开启）；必答 1-6 全部回答，数据见 `spike/REPORT.md`。门槛实测：写批 P50 285–408ms（<500 PASS）、查询 P99@10万 file 49.7ms / s3 128.3ms（<300 PASS）。进 M1 的遗留 5 项：谓词下推、optimize、连接缓存、真实云 S3 conformance、aws-lc-rs/cmake 进 CI。以下为原任务书：

范围：最小 server（CreateVectorBucket / CreateIndex / PutVectors / QueryVectors 四操作）+ SigV4 验签 + object_store 双后端（file + minio 容器）。
验收：**boto3 与 aws-sdk-go-v2 未经修改直连跑通四操作 roundtrip（验签开启状态）**。
必答问题（产出物 = 报告 + 数据表）：
1. 附录B 全部 9+ 项协议模糊点（对三 SDK 抓包回读）。
2. 条件写能力矩阵（AWS/MinIO/R2/SeaweedFS/本地）。
3. Lance-over-S3 延迟矩阵：1 万/10 万/100 万向量 × {写入（500/批）、P50/P99 查询、预过滤查询} × {file, minio 同机, minio 容器网络, R2}。
4. merge-insert × 动态 schema 演化叠加行为；struct 标量过滤谓词形态。
5. `cargo tree -d` arrow 去重；release 二进制体积。
6. axum vs hyper 体积差；自研 vs aws-sigv4 依赖差。
门槛：SDK 打通 + 延迟可接受（同机 minio 写入 < 500ms/批、查询 P99 < 300ms@10 万向量——不达标则回到"file 优先、s3 为增强"的叙事降级点，架构不变）。

### M1 — v0.1 发布（M0 后 4–6 周）

范围：21 操作全实现（§4 全表）；file/memory/s3(MinIO/AWS/R2) 后端；SigV4 static + --no-auth；CLI 四命令；MCP server；符合性测试套件（boto3 + Go SDK 双客户端，CI 跑全操作矩阵 + 错误码断言）；README（负面清单 + limits 对照表 + 5 分钟上手）；GitHub Actions 全矩阵 + ubuntu:20.04 冒烟 + size-guard。
门槛（发布即验收，G7）：conformance 全绿 + 发布物就绪 + 落地帖发出。发布后观察窗 2–4 周（§9 指标）。

### M2 — v0.2 超集与规模（门槛：M1 外部信号 ≥ 基线，见 §9）

范围：$textMatch + hybrid/RRF（tantivy 本地缓存索引，数据源仍是 Lance）；IVF_PQ + `vf optimize`；后端矩阵扩展验证（SeaweedFS/Ceph）；bench 报告；策略最小实施（可选）。

### M3+ — 门槛制扩张（每项须外部信号 + 用户裁决，不因"建得起"而建）

- BYOC 同步（本地↔云 S3 Vectors 复制）——信号：用户要求迁移路径。
- 第一方语义客户端（嵌入，ort 管线已预研）——**必须先过前会话多模态证据关**（单向量视觉检索质量死锁结论）。
- 插件系统——信号：第二维护者出现（G5）。

## 8. 风险登记册

| # | 风险 | 概率 | 冲击 | 缓解 | 触发器（→ 动作） |
|---|---|---|---|---|---|
| 1 | **LanceDB 公司出官方 s3vectors 网关**（天然主人，两家竞品都用 Lance，路径现成） | 中 | 极高 | 无法防；差异化转"后端无关+模拟器+超集"或转贡献者 | 其 blog/release 官宣 → 一周内重定位裁决 |
| 2 | SeaweedFS 天级迭代，补齐单进程简单模式/超集 | 中高 | 高 | 我们=后端无关（可跑 MinIO/R2/S3）+模拟器位+超集首发 | 其发布 simple mode 且 hybrid → 评估 niche down |
| 3 | 协议漂移（季度级：2026-06 分页、2026-09 prefilter） | 高 | 中 | CI 每月 diff botocore 模型，21 op 兼容矩阵文档化 | 破坏性变更 → 7 天适配窗口承诺 |
| 4 | 条件写在部分 S3 后端缺失 → Lance 提交退化/慢 | **低**（M0：全部目标后端支持 If-None-Match:*） | 中 | LOCK 对象单写者串行化降级（防御性保留，§5.2） | 新后端接入时先跑条件写测试 |
| 11 | aws-lc-rs（object_store aws 硬绑）：C 供应链 + cmake 构建依赖进 CI | 低 | 低 | 锁版本；CI 预装 cmake；关注上游 ring feature 恢复后切换 | 构建链断供 → 评估 ring 路线 |
| 5 | 远端 S3 延迟伤叙事 | 中 | 中 | file/memory 保底 dev/CI；manifest 键控缓存 | spike 延迟超标 → 叙事降级为"同机房 MinIO 位" |
| 6 | 单人断代（生存悖论） | 高 | 高 | K0 分步门槛 + 可退出性格局（Lance 开放格式）+ M1 起每季评估寻找共维者 | 6 个月无第二贡献者 → 维护模式 + README 寻接管 |
| 7 | merge-insert × schema 演化边角（并发批新键、类型漂移） | 中 | 中 | 写者串行天然规避并发演化；类型冲突预检 | spike Q10 失败 → metadata 全 blob + 建列惰性化 |
| 8 | lancedb 0.41 API 漂移 / 双 arrow | 中 | 低 | 锁 minor；`cargo tree -d` 进 CI；API 只经内部一个 crate | 依赖冲突 → 隔离 crate 收敛 |
| 9 | SigV4/pagination 细节与真实 SDK 行为出入 | 中 | 中 | M0 抓包三 SDK；conformance 套件即回归防线 | 新 SDK 大版 → 套件补跑 |
| 10 | "vectorfs" 名称语义税（服务而非 fs） | 确定 | 低 | README 第一句定位纠偏；发布物副标题承担 | — |

## 9. Kill criteria 与先行指标

**Kill / 降级判据**（任一触发 → 对应动作，不再自动续命）：
1. LanceDB 官方 s3vectors 网关 GA → 停止功能开发；评估转上游贡献者或退出。
2. M1 发布后 3 个月：< 100 stars 且 < 3 个外部用户 issue → 降级维护模式（bug + 协议追新 only）。
3. 6 个月无第二贡献者且无付费/机构信号 → 维护模式 + README 声明寻求接管者。
4. M0 spike 关键失败（SDK 无法打通，或 minio 同机写入 > 1s/批）→ 回到 file-only 架构决策点再裁决。
5. AWS 停滞**不是** kill 信号（存量 s3vectors 客户端仍需自托管位）。

**先行指标监测**（每月一次，≤30 分钟，记入 `docs/indicators-YYYY-MM.md` 或 issue）：
- SeaweedFS releases（vector 相关）、Ceph 稳定版是否含 s3vector、Ozone 2.2.0
- LanceDB 公司 blog/releases（风险 #1 主哨）
- LocalStack 是否复活/补 s3vectors
- botocore s3vectors 模型 diff（协议漂移）
- 自仓 stars/issues/下载量；落地帖渠道的累积反馈

## 10. K0 治理记录（本日裁决）

- 模式：**solo，不范围冻结，分步执行、每步验证**；coding agent 为主要实现力。
- 纪律对冲（documentfs 元教训——"因为建得起所以建了"）：
  1. 每个里程碑以 §7 验收门为硬边界，过门才进下一步；门内包含用户人工审查。
  2. 新组件三问（进 PR 模板）：差异化必需还是"AI 让它便宜"？无用户证据是否新增持久状态？文档是否超代码量？
  3. 负面清单（§2）为宪法，改动须显式裁决记录于本文档 §0。
  4. conformance 套件先于功能实现（TDD 于协议面：测试=官方 SDK）。
  5. 2026-11-03 M0 执行实录：子代理模式——主会话分派 5 个任务（环境/脚手架/实现/SDK 验收/S3+性能）、逐任务亲自复跑验收（不采信代理自述：测试、双 SDK roundtrip、S3 后端、体积、MinIO 归档均独立复现）、逐任务提交（b5ea181→…）。此为 K0"借助 coding agent 推进"的标准作业形态。

---

## 附录A：操作全集（2026-11-03 botocore 模型核实版，恰 21 个）

全部 `POST /<Op>`，除标注外；错误列省略公共前缀（Validation/ServiceUnavailable/TooManyRequests/InternalServer/AccessDenied/RequestTimeout/NotFound）。

| 操作 | Method+URI | 特有错误 | 关键参数/响应 |
|---|---|---|---|
| CreateVectorBucket | POST /CreateVectorBucket | Conflict, ServiceQuotaExceeded | vectorBucketName(3-63), encryptionConfiguration?, tags? → VectorBucket |
| DeleteVectorBucket | POST /DeleteVectorBucket | Conflict | 定位：vectorBucketName/Arn |
| GetVectorBucket | POST /GetVectorBucket | — | → VectorBucket(含 defaultIndexMode) |
| ListVectorBuckets | POST /ListVectorBuckets | — | maxResults(1-500), nextToken, prefix → vectorBuckets[] |
| CreateIndex | POST /CreateIndex | Conflict, ServiceQuotaExceeded | indexName, dataType=float32, dimension(1-4096), distanceMetric∈{cosine,euclidean}, metadataConfiguration?, tags? → Index |
| GetIndex | POST /GetIndex | — | → Index(含 indexMode 回显) |
| DeleteIndex | POST /DeleteIndex | — | |
| ListIndexes | POST /ListIndexes | — | by bucket; maxResults/nextToken/prefix → indexes[] |
| PutVectors | POST /PutVectors | KMS×4, ServiceQuotaExceeded | vectors[](1-500, key+data.float32+metadata) → 200 空 body |
| GetVectors | POST /GetVectors | KMS×4 | keys[](1-100), returnMetadata → vectors[] |
| ListVectors | POST /ListVectors | — | maxResults(1-1000 默认500), nextToken, segmentCount(1-16)+segmentIndex(0-15) |
| DeleteVectors | POST /DeleteVectors | KMS×4 | keys[](1-500) |
| QueryVectors | POST /QueryVectors | KMS×4 | topK(1-10000, required), queryVector, filter?, returnDistance?, returnMetadata?, nextToken?, queryMode? → vectors[{key,distance,metadata?}](每页≤100), distanceMetric 回显 |
| PutVectorBucketPolicy | POST /PutVectorBucketPolicy | — | policy JSON 原文 |
| GetVectorBucketPolicy | POST /GetVectorBucketPolicy | — | |
| DeleteVectorBucketPolicy | POST /DeleteVectorBucketPolicy | — | |
| TagResource | **POST /tags/{resourceArn}** | Conflict | tags ≤10 |
| UntagResource | **DELETE /tags/{resourceArn}** | Conflict | TagKeys |
| ListTagsForResource | **GET /tags/{resourceArn}** | — | → tags |
| PutVectorBucketDefaultIndexMode | POST /PutVectorBucketDefaultIndexMode | — | defaultIndexMode∈{CLASSIC,ENHANCED}；仅影响新建索引 |
| UpdateIndexMode | POST /UpdateIndexMode | — | indexMode∈{CLASSIC,ENHANCED}；我们=恒 ENHANCED 行为 |

## 附录B：协议模糊点（M0 对拍结果 + 原始清单）

**M0 对拍结论**：B2 ✅（两 SDK 均 signed payload、无 x-amz-content-sha256 头；boto3 签 3 头、Go 签 6 头——验签须按请求声明列表重建）、B8 ✅（问题消解）、B10 ✅（三步序闭环，§5.3）；B1 部分（头+body 双发被两 SDK 类型化解析，AWS 端原文待实测）、B3/B7 部分（document 三态/无 charset/空 body/cosine=1−sim 已实证）；**B4/B5/B6/B9 需真实 AWS 端点，M1 或有凭证机会时补**。原文如下：

1. 错误 wire 实际形态：真实端点头/body 组合、`__type` 是否带 namespace、ValidationException 线上恒 400？（对策：双发 + 三 SDK 回读断言。）
2. SigV4 payload：boto3/Go SDK 对该服务实际发 signed payload 还是 UNSIGNED-PAYLOAD？aws-chunked？
3. Document 边界：嵌套对象/混合数组/超大数字被拒的形态（Validation vs Serialization）。
4. 顶层多键 filter 是否隐式 AND；类型不匹配的行为；对 nonFilterable 键过滤的错误消息原文。
5. PutVectors 覆盖同 key：metadata 整体替换还是合并？省略 metadata 保留旧值否？
6. nextToken：空串 vs 省略、有效期、跨 segment 行为；topK=10000 的分页算法。
7. cosine `distance` 的确切定义（1−sim？）；tie 的排序；Content-Type charset；空 body 响应头。
8. Go SDK v1.17.x schema-serde 新路径与旧版在 nil list/map 序列化的 wire 差异。
9. GET/DELETE /tags/{arn} 的 ARN URL 编码要求。
10. （工程项）Lance merge-insert × 动态 schema 演化叠加；struct 谓词过滤的写法与性能。

## 附录C：阿里 Fusion 蓝本摘要（别名层废弃说明）

- 兼容声明方向：AWS SDK → 阿里端点（`{bucket}-{uid}.{region}.oss-vectors.aliyuncs.com`，三级域名定位，body 内 bucket 名须与域名一致否则 400）。其原生 API（`POST /?queryVectorsFusion`，OSS V4 HMAC 签名）为独立线格式——与 AWS 面不互通，实现双面=双倍维护零增量用户，故废弃。
- 16 个 AWS 操作在阿里有原生对应（Create↔Put 等）；阿里特有：PutVectorIndexFusion、QueryVectorsFusion、Logging×3；AWS 特有（阿里兼容面 405/忽略）：Tag×3、UpdateIndexMode、DefaultIndexMode。
- Fusion 语义蓝本（M2 超集裁剪源）：显式 schema（字段 ≤100、向量 ≤3 路）；`knn[]` 多路（各带 filter/topK≤500/numCandidates≤1000/boost）；`retriever` 复合（rrf{k,windowSize,weight} / weight{normalizer}，复合不可嵌套）；`$textMatch`(operator/minShouldMatch/boost)、`$textMatchPhrase`；`sort[]` ≤3（含 _score）；`limit` ≤500；纯标量查询才可翻页；`ip` 距离。阿里 Standard 限额参照：TopK ≤500、QPS 100、写后 2–3 秒可查（我们强一致即优势点）。

## 附录D：竞争格局快照（2026-11-03，验证③产出）

| 项目 | 状态（2026-11-03） | 形态/引擎 | 对我们的含义 |
|---|---|---|---|
| SeaweedFS | **4.52 已发布**（2026-11-02）：S3 Vectors 网关、boto3 conformance harness、两周 60+ PR、35.9k★ | 分布式 FS（master/volume/filer + vector server），Lance | 主要威胁与**潜在宿主**；盯 simple-mode 与超集动作 |
| Ceph RGW | s3vector 合并主干（2026-09-17，+29.7k 行），未进稳定版 | 整套 Ceph，内嵌 lancedb-c；官方明示不做混合检索/低延迟 | 宿主+验证品类；其"不做 hybrid"留给我们超集位 |
| Apache Ozone | roadmap HDDS-14967（~2.2.0） | Hadoop 系 | 观察项 |
| LocalStack | 无 s3vectors；Community 版 2026-03 转 inactive | 模拟器 | dev/CI 位空缺=我们 memory:// 后端的定位空间 |
| MinIO / Qdrant / Cloudflare / TurboPuffer | 零相关动向 | — | MinIO=宿主；无新竞争者 |
| LanceDB 公司 | 无兼容产品动向 | 引擎 | **最高哨戒风险**（风险#8-1）：两大竞品都用 Lance，官方网关路径现成 |
| 应用侧 | Open WebUI 已内置 s3vector backend（社区维护，boto3 端点可覆写）；LlamaIndex 3 天/LangChain 9 天出集成；Bedrock KB GA 绑定 | — | 需求侧强兑现 |
| 社区情绪 | HN 高热帖抱怨 post-filter/topK/无 hybrid；明确 self-host 呼声（"cloud is someone else's computer"） | — | 超集+自托管叙事的原始证据 |

（快照来源与链接见 2026-11-03 会话调研记录；下次复查按 §9 月度节奏。）

**2026-11-03 增补**：MinIO 社区版归档（repo 2026-02 archived，末版 2025-10，下载通道下线；AGPL 线由 Silo 分支续命）——"简单自托管对象存储"位现真空，流向 SeaweedFS/Garage/Silo。对 vectorfs 双重含义：①宿主格局变动（README/文档的宿主列表口径随之调整）；②"向量界 MinIO"类比的心智位反而更空——名字的出处退场，位子还在。另：SeaweedFS 4.52 的 S3 面已实测作为 vectorfs 宿主（M0，9/9）——"竞争者即宿主"判断获实证。
