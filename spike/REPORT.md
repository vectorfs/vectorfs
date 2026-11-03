# M0 Spike Report（最终交付物）

- 日期：2026-11-03。crate：`spike/`（bin `vf-spike`）。数据与过程证据：`NOTES.md`（Task A-D 全部段落）。
- **M0 验收门状态：PASS。** boto3 1.43.123 与 aws-sdk-go-v2 s3vectors v1.17.1 未经修改直连，file:// 与 S3 后端（SeaweedFS 4.52 同机）各 9/9 步骤全 PASS，SigV4 验签开启。`cargo test` 19 passed + 1 ignored（ignored = 需活 S3 服务的条件写测试，对 SeaweedFS 实跑 PASS）。
- 重要环境事实：**MinIO 已无社区发行版**（dl.min.io 全线 410 归档；Docker Hub 匿名拉取 2026 起实质关闭；本机 Docker Desktop daemon 死锁无法修复）——任务书"minio 实测"行全部以 **SeaweedFS 4.52 单二进制**（`weed server -s3`，同机 localhost）替代完成，如实标注。详见 NOTES「Task D 工程备注」。

## 必答 1：附录B 协议模糊点（对拍）

Task C 已闭环 B1（错误 wire 双发、两 SDK 解析路径）、B2（SigV4 signed payload、SignedHeaders 集合差异）、B3 部分（document 三态）、B7 部分（cosine=1−sim、Content-Type 无 charset、200 空 body）、B8（Go schema-serde 新路径自 v1.0 即如此，问题消解）。本任务新增：

- **B10（必答 4 重叠）**：merge_insert × schema 演化——见必答 4，闭环。
- B4/B5/B6/B9 仍需真实 AWS 端点（无凭证，M1 或对拍机会再答）；spike 自身行为已钉死在测试里。

## 必答 2：条件写能力矩阵

机制链条（源码核实）：Lance manifest 提交（s3 scheme）= `ConditionalPutCommitHandler` → `put_opts(PutMode::Create)` → HTTP `If-None-Match:*`；412/Precondition → object_store `AlreadyExists` → lance `CommitConflict`。**该矩阵直接决定各后端 Lance 提交原子性与 catalog 创建竞态安全。**

| 后端 | If-None-Match:* 条件写 | 证据 | 对 Lance 提交的意义 |
|---|---|---|---|
| 本地目录（object_store LocalFileSystem） | **支持** | 实测（`taskd_tests::conditional_put_local`，cargo test 常绿项）：首写 OK，重写 AlreadyExists（O_EXCL 语义） | file:// 后端提交原子（ lance 对 file 另有 rename 快路径，此为兜底验证） |
| SeaweedFS 4.52（s3 网关） | **支持** | 实测（`taskd_tests::conditional_put_s3` + `--ignored` 实跑）：首写 OK，重写 AlreadyExists | manifest 提交原子 = 多次 PutVectors 的版本链安全；S3 后端端到端 9/9 即含此路径 |
| AWS S3 | **支持** | 文档核实未实测：2024-08 起 PutObject/CompleteMultipartUpload 支持；已存在=412 PreconditionFailed；并发竞态=409 ConditionalRequestConflict（需重试） | 原子；409 竞态窗口需 lance 侧重试（已有 CommitConflict 处理） |
| Cloudflare R2 | **支持** | 文档核实未实测：PutObject 条件头"原子评估于已提交桶状态"，412 | 原子；R2 需 `use_constant_size_upload_parts`（provider 已留开关） |
| MinIO | **支持（声明）** | 文档核实未实测：object_store 0.14 文档明言 "minio supports conditional put using standard If-Match and If-None-Match headers"；社区版已归档无法实测 | 预期原子；M1 兼容矩阵实测项 |
| memory://（lance 内建） | n/a（进程内） | — | 单写者进程内安全 |

**结论**：设计 §5.2 的担忧（条件写缺口→LOCK 降级）在全部目标后端上未成立；LOCK 对象降级路径保留为防御性设计但非当前必需。

## 必答 3：Lance-over-S3 延迟矩阵

测量法：release 二进制；boto3 直打 API（SigV4 开启）；加载 = PutVectors 500/批（每批一次 lance merge_insert 提交）；查询 = 200 次 topK=10 无过滤 + 50 次 filter `{"i":{"$eq":存在值}}`；客户端计时，P50/P99（ms）。dim=768 float32 随机向量 + metadata `{"i":n}`。s3 = SeaweedFS 4.52 同机 localhost（与 file:// 同一块物理盘，差值即 HTTP+网关开销，非网络）。

| 规模 | 后端 | 批 P50/P99 | 加载 vec/s | 查询 P50/P99 | 过滤查询 P50/P99 |
|---|---|---|---|---|---|
| 10k | file | 264 / 279 | 1,522 | 8.5 / 11.8 | 13.2 / 15.4 |
| 10k | s3(同机) | 285 / 308 | 1,424 | 22.3 / 27.8 | 26.3 / 32.0 |
| 100k | file | 269 / 291 | 1,500 | 44.7 / 49.7 | 112.5 / 120.6 |
| 100k | s3(同机) | 292 / 312 | 1,397 | 117.1 / 128.3 | 170.6 / 192.7 |
| 1M | file | 333 / 403 | 1,256 | 397.9 / 431.4 | 1,019 / 1,062 |
| 1M | s3(同机) | 408 / 550 | 1,061 | 971.6 / 1,054 | 1,359 / 1,473 |

（R2/远端网络延迟未测：无凭证。minio 容器网络列未测：MinIO 不可得。）

**对 M0 门槛（同机 s3 写 <500ms/批、查询 P99<300ms@10万）**：
- 写入：10k/100k 批 P50 285-292ms **PASS**；1M P50 408ms PASS 但 P99 550ms 略超 500——表到百万行后每批提交开始付 fragment 代价（1M 时 file 也从 269→333ms），指向 §5.4 的 `optimize`（fragment>32 压缩）在产品期必要，spike 未实现。
- 查询：@10万 P99 file 49.7ms / s3 128.3ms，**双双 PASS**（门槛 300ms）。
- 1M 查询 398ms(file)/972ms(s3)：flat 全表扫 3GB 的必然代价，与 §5.5 预言一致（>10 万建 IVF_PQ 是 M2 已规划项）；s3 与 file 的比值在 1M 时放大到 ~2.4x，因为每次查询全量拉数据——**manifest 键控缓存（§5.6）在 S3 后端的价值随规模上升**，量化数据在此。
- 过滤查询 = spike 的全表扫+内存过滤路径（非谓词下推）；产品实现谓词下推后该列应显著下降。如实标注。

## 必答 4：merge_insert × 动态 schema 演化（附录B-Q10）

实验（`taskd_tests::merge_insert_with_schema_evolution`，lancedb 0.41/lance 14.0.2，file 后端）：

1. 表 schema `metadata: struct{i:int64}`，批1（{i}）merge_insert 正常。
2. 批2 出现新键 tag（struct{i,tag}）**直接 merge_insert → 干净拒绝**（`Append with different schema: metadata had mismatched children ... unexpected=[metadata.tag]`）——fail-closed，无静默数据损坏。
3. 演化 API 排雷（真实错误记录在 NOTES）：字段名带点（"metadata.tag"）被拒；AllNulls 传含旧子字段完整 struct 被拒（"Column i already exists"）。**正确姿势：`add_columns().transform(AllNulls(schema{metadata: struct{仅新子字段}}))`。**
4. 演化后批2 merge_insert 正常；读回旧行 tag=NULL、新行带值。
5. **裁决：演化不破坏 merge_insert，条件是"先演化后提交"**。产品写路径（§5.3）落实为：PutVectors 检测批内新 metadata 键 → AllNulls 演化 → merge_insert，三步在一个请求内完成。风险#7 的并发演化场景由单写者（§5.1）天然规避。struct 标量谓词过滤：spike 未建 metadata 列（JSON blob），谓词形态留 M1（与 §5.3 动态列方案一起做）。

## 必答 5：体积裁决（Task B 遗留 312MiB 超预算）

| 变体 | 体积 | 说明 |
|---|---|---|
| Task B release 默认 profile（file-only） | 312 MiB | datafusion54+lance14 全链接 |
| Task D release 默认 profile（+S3 客户端） | 379 MiB（397,246,352 B） | object_store aws + aws-lc-rs + reqwest 链，**S3 支持 = +67.7 MiB** |
| Task D **release-prod**（lto=fat + cgu=1 + strip） | **164.5 MiB**（172,523,448 B） | §5.7 产品 profile，-57%；编译 ~25min |

- **feature 裁剪不可达**：datafusion 54 在 lancedb 0.41 与 lance 14.0.2 均为**非可选**依赖（两 crate 的 Cargo.toml 均无对应 feature）；lance 自身 default features 全关（lancedb 以 default-features=false 引入）后仍是这个量级。「去 datafusion」只能换引擎或等上游重构，当前代际无路径。
- **裁决建议**：预算从 150MB 硬顶**放宽至 175MB**（release-prod 实测 164.5，留 ~6% 余量），警戒线从 120 提到 155。辩护：单二进制内含完整向量引擎（datafusion+arrow+lance），可查参照 SeaweedFS `weed` 单二进制 149MB（本任务实测下载物，同为"存储引擎全家桶"形态）；150→175 只是把"引擎不可避成本"显式化，非范围膨胀。维持 150 的替代路径 = 砍 S3 客户端（-67.7MiB → ~97MiB @prod profile）**不可取**（S3 后端是核心卖点）。panic=abort 未启用（对 spike 无意义，产品期再评估崩溃语义）。
- size-guard CI 动作不变，只改阈值数字。

## 必答 6：axum vs hyper 与自研 vs aws-sigv4

- **axum 已用且够轻**：Task A 下界实测 axum+tokio+clap 底座仅 3.4MiB（引擎未链接时）——axum 相对裸 hyper 的边际体积在 MiB 量级，对 164.5MiB 总量是噪声；21 条平铺路由无中间件复杂度（§5.7 判断成立），不另做 hyper 对照。
- **自研 SigV4 胜出**：验签 ~350 行零 aws-* 依赖（sha2/hmac/hex 均为锁内已有），双 SDK 22/22 签名请求验签通过。反面证据链完整：lancedb aws feature = 22 个 aws-* crate + opendal（违反红线）；即便只引 aws-sigv4 也会带入 smithy 底座并开启"接受 AWS SDK 链"的第一步。**S3 客户端同理走 object_store 自研签名**（本次验证可用），全链零 AWS SDK 成立。

## design.md 修订建议清单

1. **§5.7 体积条款**：`≤120MB 警戒 / 150MB 硬顶` → `≤155MB 警戒 / 175MB 硬顶`；补一行"产品 profile 必须 lto=fat+strip（实测 379→164.5MiB）"；补"S3 客户端(object_store+aws-lc-rs)成本 +67.7MiB，接受"。
2. **§5.2 后端矩阵**：MinIO 行加注"社区版 2025-归档、2026 无二进制发行；对象 = 其历史版本或商业版"；SeaweedFS 由 v0.2 提前到 v0.1 已验证（本次 9/9+延迟+条件写全过）；条件写矩阵结论落入正文（AWS/MinIO/R2/SeaweedFS/LocalFS 全支持 If-None-Match:*，LOCK 降级路径保留为防御）。
3. **§5.2/ENV**：注入路线定稿为产品路线：object_store(aws) + `ObjectStoreProvider` + `Session` 注入（本次代码为原型）；需直接依赖 lance-io（lancedb 未 re-export provider trait）——记入"API 只经一个内部 crate"的例外清单。
4. **§5.4 写路径**：加"批内新 metadata 键 → 先 AllNulls schema 演化 → merge_insert"三步序（Q10 实验结论）；加"表 >50 万行后批提交开始线性变慢（实测 269→333ms@file, 292→408ms@s3），`optimize` 压缩是产品期必做项而非可选"。
5. **§5.5/§5.6**：1M 规模 s3/file 查询比 2.4x 的实测数据支持"manifest 键控缓存在 S3 后端为一级优化"的定位；过滤查询列补注"谓词下推后过滤列数字将大幅下降（spike 为全表扫+内存过滤）"。
6. **§7 M0 门槛**：写门槛细化为"P50<500ms（P99 可到 550@1M，配合 optimize 收敛）"；查询门槛维持 300ms@10万并补"1M 需 IVF_PQ（M2），flat 扫描 1M 实测 398-1054ms 为已知上界"。
7. **§8 风险#4**（条件写缺失→降级）：概率从"中"降为"低"（全后端实测/文档支持）；风险登记新增"object_store aws feature 硬绑 aws-lc-rs（cmake 构建依赖 + C 供应链）"低风险项。
8. **§1.1/附录D 竞争表**：SeaweedFS 行加"2026-11 已实测其 S3 面作为 vectorfs 宿主（9/9 兼容）"——竞争者同时是宿主的判断被本次验证。

## 遗留风险（进 M1 前须处置）

1. **过滤谓词未下推**：spike 过滤=全表扫+内存过滤，1M 时 1.0-1.4s；产品需谓词下推（§5.5 已规划）。
2. **fragment 增长未治理**：1M 批提交变慢 +23%（file）/+41%（s3）vs 100k；`optimize` 未实现（§5.4）。
3. **每操作新开 lance Connection**（含 S3 session 复用但连接层重建）：产品期连接缓存是明确优化点（spike 简化，已在 NOTES 标注）。
4. **SeaweedFS 替身风险**：MinIO/AWS/R2 的条件写与延迟未实测（仅文档核实）；M1 conformance 需在至少一个真实云 S3 上补一轮。
5. **aws-lc-rs 供应链**：C 代码 + cmake 构建依赖进 CI；可评估上游 ring feature 恢复后切换。
