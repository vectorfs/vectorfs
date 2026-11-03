# M0 Spike NOTES

数据为主。日期：2026-11-03。crate：`spike/`（bin `vf-spike`）。环境见同目录 ENV.md。

---

## 依赖核查（Task A）

### 版本锚点

- `lancedb 0.41.0`（crates.io 该 minor 仅此一版，即当前最新 0.x）
- 链路：`lancedb 0.41.0 → lance 14.0.2 / lance-* 14.0.2 → object_store 0.14.2`；`datafusion 54.1.0`（lancedb/lance 必带）
- rustc 1.99.1 ≥ lancedb MSRV 1.94，编译无版本问题
- 全树唯一 crate 数：**456**；最终树（见下）**0 个 aws-\* crate**

### arrow 去重（design.md §5.7 第一周验证项）——结论：无多版本

`cargo tree -d` 输出中 **无任何 arrow-\* 条目**。逐一核对全树唯一版本：

```
arrow / arrow-array / arrow-data / arrow-schema 及其余 10 个 arrow-* == v58.4.1（各恰好一份）
```

`cargo tree -i arrow-array` 谁在拉：`arrow 58.4.1 ← datafusion 54.1.0`（lance/lancedb 侧）与 lancedb 直接依赖 arrow-array ^58 —— 两路汇合统一到 58.4.1。**双 arrow 版本风险：当前锁文件下不存在。** 附加观察：lance 系有 `lance-arrow-scalar/stats v58.0.0`（以 arrow 大版本编号的 lance crate），版本号易误读，非 arrow 本体。

### `cargo tree -d` 其余重复/意外

共 38 组重复，无致命项：

| 项 | 版本 | 来源/备注 |
|---|---|---|
| **object_store ×2** | **0.13.2 + 0.14.2** | **最大意外**：`datafusion 54.1.0` 拉 0.13.2（datafusion-catalog/-common/-datasource/-execution/-physical-* 等），lance 全家 + lancedb 用 0.14.2。两版本类型不互通，但互不相交（datafusion 内部用旧版），编译/运行无冲突；lancedb 升级 datafusion 后有望合并 |
| hashbrown ×4 | 0.14.5/0.15.5/0.16.1/0.17.1 | 常规生态分裂 |
| getrandom ×3 / phf ×2 / rand ×2 | — | 常规 |
| serde_json | 仅 1.0.154 一版 | tree -d 报的是**特性分裂**：lancedb 开 `preserve_order+raw_value+indexmap`，其余默认——版本无分裂 |
| serde_core | 仅 1.0.229 一版 | 同上，特性分裂 |
| zstd ×2 / syn ×2 / regex ×2 / prost ×2 / snafu ×2(0.8/0.9) 等 | — | 常规小分裂 |
| tokio | 仅 1.54.1 一版 | 特性并集统一，无版本分裂 |
| http / hyper | 仅 http 1.5.0、hyper 1.12.0 一版 | 无旧版混入 |

**重量级事实**：datafusion 54 是全树最大子图（约 40 个 datafusion-\* crate + parquet），由 lancedb 必带——不可避，属引擎成本。

### S3/MinIO 支持：lancedb `aws` feature 的真实代价（Task D 关键输入）

查证（crates.io sparse index features2 + docs.rs/lancedb/0.41.0 + 本地 registry 源码 grep）：

- lancedb 0.41 默认 feature 为**空**（object_store 仅 `fs`，即 file 后端）。
- S3 = feature `aws`：`["dep:aws-smithy-types", "lance/aws", "lance-io/aws", "object_store/aws", "lance-namespace-impls/dir-aws"]`。
- **实测开启该 feature 的树**（本任务先建后撤，证据存档）：`lance-io/aws` = `dep:aws-config + aws-credential-types + opendal(services-s3) + object_store_opendal`，即
  `aws-config 1.12.0 → aws-runtime → aws-sigv4 1.5.3 + aws-sdk-sso 1.111.0 + aws-sdk-ssooidc 1.113.0 + aws-sdk-sts 1.116.0 …` 共 **22 个 aws-\* crate + opendal 0.60.5 + object_store_opendal 0.60.5**，全树 531 crates。
- **这违反「服务器零 AWS SDK」红线**，故最终 Cargo.toml **不开** `aws` feature。
- **零 SDK 备选路线（Task D 首选，待实现验证）**：直接依赖 `object_store = { version = "0.14", features = ["aws"] }` —— object_store 自研 S3 协议客户端（仅 reqwest + aws-lc-rs 加密库，**不含 aws-config**；可用 feature `ring` 换掉 aws-lc-rs），经 `lance_io::object_store::ObjectStoreProvider` trait（`async fn new_store(base_path, &ObjectStoreParams)`，本地源已核实存在）注入自建 store。需注意 lancedb 0.41 未 re-export 该 trait，要加 `lance-io = "14.0.2"` 直接依赖。
- 若 Task D 走 provider 路线受阻，再升级裁决是否接受 aws-config 链（设计例外）。

### 连 MinIO 的 storage_options 键（查证记录，未实现）

lance-io aws provider 以 `AmazonS3ConfigKey::from_str` 解析键名（lance-io/src/object_store/providers/aws.rs:644），object_store 0.14.2 接受带/不带 `aws_` 前缀两种写法。MinIO 需要：

```text
endpoint        (或 aws_endpoint；如 http://127.0.0.1:9000)
access_key_id   (或 aws_access_key_id)
secret_access_key (或 aws_secret_access_key)
region          (或 aws_region；MinIO 惯例 us-east-1)
aws_allow_http  = "true"   ← 自定义 endpoint 走 http 时必须（lance 测试代码即用此键）
可选: aws_session_token / aws_skip_signature / aws_virtual_hosted_style_request(默认 path-style，MinIO 友好)
```

出处：object_store-0.14.2/src/aws/builder.rs `AmazonS3ConfigKey` 枚举文档 + lance-io aws.rs 测试（`("aws_endpoint", uri), ("aws_allow_http","true")`）。

### 冒烟（真实输出）

```
$ ./target/debug/vf-spike --port 18787 &   # --port 生效
$ curl -s http://127.0.0.1:18787/healthz
{"ok":true}
$ VF_PORT=18788 ./target/debug/vf-spike &  # 环境变量覆盖生效
$ curl -s http://127.0.0.1:18788/healthz
{"ok":true}
$ ./target/release/vf-spike --port 18789 & # release 同样通过
{"ok":true}
```

---

## 体积（Task A）

```
$ ls -lh target/debug/vf-spike target/release/vf-spike
-rwxr-xr-x 48M target/debug/vf-spike      (49,317,608 B，dev 默认含 debuginfo)
-rwxr-xr-x 3.5M target/release/vf-spike   ( 3,605,552 B，≈3.4 MiB)
```

- release profile 保持 cargo 默认（对照记录）：`opt-level=3, debug=false, lto=false, codegen-units=16, panic=unwind, strip=none, incremental=false`（未做 §5.7 产品级 lto=fat/cgu=1/strip）。
- 对照组：lancedb-aws-feature 变体的 release 二进制 3,605,608 B —— 与无 aws 变体差 56 字节（噪声级）。
- **重要诚实声明（下界！）**：Task A 的 main.rs **尚未调用任何 lancedb API**（/healthz only），链接器按未用代码丢弃了 lancedb/datafusion 全家。3.4 MiB 只证明 axum+tokio+clap 底座很轻；**真实引擎体积须等 Task D store.rs 接入 lancedb 后重测**。≤120MB 预算判断在彼时做，本数字仅供参考下界。
- 编译耗时（WSL2 本机，冷缓存）：debug 全量 11m43s；撤 aws feature 后增量 6m04s；release 增量 7m21s；`target/` 目录 16G（已进 .gitignore）。

---

## Task B 工程备注（最小 s3vectors server：4 操作 + SigV4 验签 + lancedb file 后端）

日期：2026-11-03。产物：`vf-spike`（axum + 自研 SigV4 + lancedb 0.41.0）。`cargo test` 17/17 绿（0.07s）。代码位置：`spike/src/{main,ops,store,sigv4}.rs`。

### botocore 模型提取（字段权威，逐字对齐）

四个操作的 wire 结论（develop 分支 + 本机安装 botocore 1.43.123 双重核实，两者一致）：

- 路由：全部 `POST /<OperationName>`，rest-json，bucket/index 定位字段在 body。
- `CreateVectorBucketOutput` required = **仅 `vectorBucketArn`**；`CreateIndexOutput` = **仅 `indexArn`**。但 AWS CLI 参考文档仍展示 `{vectorBucket:{...}}` / `{index:{...}}` 嵌套对象（docs.aws.amazon.com CLI v2，2026-11-03 查）。**协议漂移实例（design 风险 #3）**：模型已改 ARN-only、文档未跟。处置：**双发**（`vectorBucketArn` + `vectorBucket` / `indexArn` + `index`）——SDK 忽略未知成员，新老客户端通吃。
- `PutVectorsOutput` members={} → 200 **空 body、无 content-type**（实测 curl）。
- `QueryVectorsOutput` required = `vectors` + `distanceMetric`（echo index 配置）；`QueryOutputVector` required 仅 `key`，`distance`/`metadata` 可选。
- `Timestamp`（rest-json）= epoch 秒浮点（如 `1793711700.1363378`）。盘上 bucket.json/index.json 存 RFC3339 + epoch 双份（任务要求 RFC3339，回读线格式需要 epoch）。
- 限额：`PutVectorsInputList` 1–500；`VectorKey` 1–1024；`VectorBucketName`/`IndexName` 3–63；`Dimension` 1–4096；`TopK` min 1 **模型无 max**（10000 上限来自任务/design，照执行）；`nonFilterableMetadataKeys` 1–10。
- 错误码（模型 `error` trait）：NotFound=404、Conflict=409、AccessDenied=403、InternalServer=500、Validation（无 trait，按 400）。错误发射：`X-Amzn-Errortype: <Shape>` 头 + body `{"__type","message"}` 双发，curl 实测可见。

### lancedb 0.41 API 摩擦点（实际使用的形态）

1. **upsert = merge_insert 链**：`table.merge_insert(&["key"])` 返回 `MergeInsertBuilder`，链 `.when_matched_update_all(None)` + `.when_not_matched_insert_all()`，然后 `execute(Box<dyn RecordBatchReader + Send>)`。注意三点：
   - `execute(mut self, ...)` **消费** builder，且 `when_*` 返回 `&mut Self`——必须先 `let mut m = ...; m.when_...(); m.execute(...)` 两步写（doc 示例同款）。
   - `execute` 的参数是 **arrow 的** `arrow_array::RecordBatchReader` trait；lancedb 自家 `lancedb::arrow::SimpleRecordBatchReader` 实现的是 lancedb 自己的同名 trait，**不满足**该 bound（编译期踩过）。正确姿势：`lancedb::arrow::arrow_array::RecordBatchIterator::new(iter_of_Ok_batch, schema)`（item 类型 `Result<RecordBatch, ArrowError>`）。
   - merge_insert 在 `create_empty_table`（0 行）建的表上**直接可用**，无需先 add 一批。
2. **建表**：`connect(path).execute().await` → `conn.create_empty_table(name, SchemaRef).execute().await`。每 index 独立 lance root（`indexes/<ix>/data/`，表名固定 `vectors`）+ 每操作新建 Connection（无缓存）——spike 简化，产品化时上缓存。
3. **查询**：`table.query().limit(k).nearest_to(Vec<f32>)?.distance_type(DistanceType::Cosine|L2).execute().await` → `SendableRecordBatchStream`（需 `futures_util::StreamExt` 逐 batch 收）。`nearest_to` 返回 `Result<VectorQuery>`（错误要处理）。`_distance` 列 = Float32。
4. **lancedb re-export arrow 全家**（`lancedb::arrow::{arrow_array, arrow_schema, ...}`）——下游不声明直接 arrow 依赖即保证同版本。arrow 58 的 `StringArray` 不再支持 `FromIterator<&str>`，要 `map(|x| Some(...))`。
5. **axum 相关**：handler 中持有 `&dyn Fn + Send`（不带 Sync）跨 await → future 非 Send → `Handler` 不满足（报错极晦涩，`#[axum::debug_handler]` 一击定位）。trait object 参数写 `+ Send + Sync` 即解。

### 距离语义实测（lancedb 0.41 / lance 14.0.2）

- **cosine distance = 1 − cosine相似度**（非角度、非 2−2sim）。测试断言：q=[1,0,0,0]：v=[1,0,0,0]→0.0；v=[1,1,0,0]→0.29289321881345254（=1−1/√2）；v=[−1,0,0,0]→2.0。范围 [0,2]。**与 AWS 假设一致但待 Task C 对拍确认**。
- **euclidean：lancedb flat 扫描的 `_distance` 是平方 L2（sql2），不含末尾 sqrt**——lance-linalg `l2.rs` 参考实现注释明说 "we skip the final square root step"，doc 例 [1,2]vs[3,4]→8。实测 25≠5 抓到。处置：查询侧对 euclidean 结果补 `sqrt()`（保序变换，KNN 次序与 tie 不变），对齐"AWS euclidean = 真 L2"的假设（待对拍）。**产品实现若走 lancedb 原生距离必须记得同样处理，或改自算**。
- 带 filter 的查询走全表扫 + Rust 自算距离（预过滤 = ENHANCED 语义），Rust 侧公式与上述对齐（cosine=1−sim，euclidean=sqrt(Σ(x−y)²)），测试各自钉死数值。
- 排序：结果统一 (distance asc, key asc)；无 filter 路径先 `limit(topK)` 再排序——**topK 边界外的并列 tie 行可能被截掉**（lancedb 内部截断次序未定义），spike 接受，产品用全量排序或 distance_range。

### SigV4 自研验签（~350 行，零 aws-* 依赖）

- 依赖增量：`sha2 0.10.9 / hmac 0.12.1 / hex 0.4.3 / futures-util 0.3.34`——全部是锁文件里已有版本（零新传递子图）；tokio 加 `fs` feature；dev-deps `tower 0.5(util)/tempfile 3.27`。
- canonical 规则：非 S3 通用编码（保留 unreserved + `/`）；query 严格编码排序（四操作恒空）；payload hash 优先取 `x-amz-content-sha256` 头（天然覆盖 UNSIGNED-PAYLOAD 形态），否则 body sha256；常量时间比较（XOR 折叠自写）。约束：service 必须精确 `s3vectors`、region 必须等于服务端配置、X-Amz-Date 偏差 ≤15min、credential scope date 必须与 X-Amz-Date 一致、SignedHeaders 必含 host+x-amz-date。失败一律 `AccessDeniedException` 403。
- **跨实现验证**：测试内 Rust 签名器（`sigv4::signer`，#[cfg(test)]）之外，用独立 Python 签名器（按 AWS General Reference 另写，`/root/tmp-taskb/smoke.py`，不进仓库）冒烟对拍：4 操作 200、篡改签名 403、无 Authorization 403——两套独立实现签出的请求 Rust 验签均通过。
- `--no-auth` 或未配凭证 → 跳过验签（启动行有 WARNING）。`/healthz` 恒免验签（非 AWS 面）。

### --trace 线格式（Task C 输入）

- 每请求两行 JSON 事件到 stderr：`{"ev":"http_request","method","path","headers":{名:[值]},"body":"..."}` + `{"ev":"http_response","status","headers","body"}`；多值头保留数组形态，**任何头不丢**；403（验签失败）也补齐了 response trace（初版漏掉，已修）。
- 启动行：`vf-spike: starting on 127.0.0.1:{port} data_dir=... auth=... region=... account=... trace=...`。
- 实测一轮 4 操作 + 篡改 + 无签名 = 12 行 trace（6 req + 6 resp），含 `x-amzn-errortype` 响应头。

### 体积重测（Task A 遗留问题：lancedb 真正链接后）

```
debug:            2,138,607,432 B (~2.0 GiB, dev 默认含 debuginfo)
release(默认):      326,366,168 B (312 MiB)   ← 引擎(datafusion 54+lance 14)已实际链接
release(strip 后):  235,612,760 B (225 MiB)
```

Task A 的 3.5MB 是链接器丢弃引擎的下界；**真实引擎体积 312MiB（默认 profile）远超 §5.7 的 120MB 警戒/150MB 硬顶**。Task D 待办：lto=fat/cgu=1/strip 产品 profile 重测 + datafusion feature 裁剪评估；若仍超标需在 §5.7 记录裁决（预算上调或依赖瘦身），架构不变。

### 遗留 TODO（AWS 语义不确定项，Task C 对拍清单）

1. 错误 message 原文（现为描述性替身）；ValidationException `fieldList[{path,message}]` 未实现。
2. malformed JSON body 真实错误形状（spike → ValidationException 400；真实服务可能是 Serialization 族）。
3. PutVectors 同批重复 key 语义（spike: 后者胜出）；省略 metadata 是否保留旧值（附录B-Q5；spike: 整行替换 = NULL）。
4. queryVector 零向量在 cosine 下是否拒绝（spike: 拒绝，同 put 规则）。
5. cosine distance 公式（假设 1−sim）与 euclidean（假设真 L2）对真实 AWS 对拍。
6. topK 上限 10000（模型无 max，来自文档/任务）；分页 nextToken 未实现（topK>100 场景 Task C 才需要）。
7. bucket/index name 是否接受 `.`（ARN pattern 允许 `[a-z0-9-.]`；spike 只放行 `[a-z0-9-]`）。
8. CreateIndex/PutVectors/QueryVectors 的 `vectorBucketArn`/`indexArn` 定位方式未实现（spike 只认 name）。
9. SigV4 细节：`Date` 头 fallback 未支持（要求 X-Amz-Date）；header 值折叠（多值 join 分隔符/引号）未对拍；错误未细分（InvalidClientTokenId/SignatureDoesNotMatch 等折叠为 AccessDeniedException）。
10. body >21MiB 时真实行为（spike: 400 ValidationException）。
11. merge_insert × 动态 schema 演化叠加（附录B-Q10，metadata 现为 JSON blob 列，无动态列——M1 再做）。
12. filter 类型不匹配行为（spike: 不匹配而非报错）；`$ne/$gt/...` 等其余算子未实现（报 400 "unsupported in spike"）。

---

## Task C 工程备注（官方 AWS SDK 直连验收 + 线格式抓包对拍）

日期：2026-11-03。**M0 正式验收门达成**：boto3 1.43.123（Python 3.13.5）与 aws-sdk-go-v2 `service/s3vectors` v1.17.1（go 1.24.4）**未经修改直连**，4 操作 + 错误路径 9 步全 PASS，**验签开启**（`--access-key test --secret-key testsecret`，未用 `--no-auth`）。

### 交付物

- `spike/conformance/boto3_check.py`：9 步符合性检查（--endpoint/--access-key/--secret-key/--region/--prefix，prefix 默认随机），每步 PASS/FAIL，任一 FAIL 退出码 1。不绑定 server 生命周期；改 --endpoint 即可打真实 AWS（无 delete 操作故不清理）。
- `spike/conformance/go/`（module `vf-conformance`）：`s3vectors@v1.17.1` + `config` + `credentials`，flag 同上，步骤 1–9 与 boto3 对齐，错误断言 `errors.As(&types.XxxException{})`；二进制 `vf-conformance` 不提交（.gitignore 已忽略）。
- 抓包：`--trace` 全程开启，trace 存 `/tmp/vf-trace.log`（1 启动行 + 23 req + 23 resp：curl healthz×1、boto3×11、go×11）。

### spike/src 改动清单

**零改动。** 两 SDK 首轮全绿，未出现需要动 `spike/src/*.rs` 的兼容问题。`cargo test` 17/17 绿（0.17s，与 Task B 相同）。

### Go SDK v1.17.1 API 形态备注（schema-serde 新路径，B8 相关）

- `types.VectorData` 是 union 接口，float32 分支为 `&types.VectorDataMemberFloat32{Value: []float32}`。
- `PutInputVector.Metadata` / `QueryVectorsInput.Filter` 是 `document.Interface`：发送用 `s3vectors/document.NewLazyDocument(goValue)`；回读用 `UnmarshalSmithyDocument(&map[string]any)`，数字回读为 `smithy-go/document.Number`（字符串形态，需自行 ParseFloat 比较）。
- 与传统 codegen 的差异：`ReturnDistance`/`ReturnMetadata` 是裸 `bool`（非 `*bool`），`QueryOutputVector.Distance` 是 `*float32`（非 `*float64`）。


---

## 线格式观察（Task C，--trace 抓包 + 独立复算对拍）

抓包环境：server `--trace`（验签开启）；boto3 1.43.123 与 Go SDK s3vectors v1.17.1 各跑 9 步脚本。签名验证方法：用 trace 里的 Authorization/SignedHeaders/body **独立重算**整套 SigV4（Python），对每条请求比对 Signature —— 这是判定 payload 形态的硬证据（UNSIGNED-PAYLOAD / aws-chunked 假设下重算不可能匹配）。

### 1. SigV4 实际形态（两 SDK 对比）

| 项 | boto3 1.43.123 | aws-sdk-go-v2（core v1.47.5 / s3vectors v1.17.1） |
|---|---|---|
| `x-amz-content-sha256` 头 | **不发**（非 S3 服务；全 11 请求均无） | **不发**（全 11 请求均无） |
| payload hash 形态 | hex sha256(body) 进 canonical request（**signed payload**）——复算 10/10 正确 SK 请求全匹配 + 错 SK 请求用错 SK 复算匹配 | 同左，signed payload |
| UNSIGNED-PAYLOAD / aws-chunked | **均未出现** | **均未出现** |
| SignedHeaders | `content-type;host;x-amz-date`（3 个；boto3 会签 content-type；amz-sdk-* 头**发送但不签**） | `amz-sdk-invocation-id;amz-sdk-request;content-length;content-type;host;x-amz-date`（**6 个**：Go 签 content-length 和 amz-sdk-* 头） |
| Authorization 格式 | `AWS4-HMAC-SHA256 Credential=test/20261103/us-east-1/s3vectors/aws4_request, SignedHeaders=…, Signature=…`（标准单头，两 SDK 完全同构） | 同左 |
| X-Amz-Date | `20261103T132811Z`（ISO8601 basic，秒精度） | 同左（`20261103T132816Z`） |
| Content-Type | `application/json`，**无 charset** | `application/json`，**无 charset** |
| User-Agent | `Boto3/1.43.123 md/Botocore#1.43.123 ua/2.1 os/linux#6.18.54.1-… md/arch#x86_64 lang/python#3.13.5 md/pyimpl#…` | `aws-sdk-go-v2/1.47.5 ua/2.1 os/linux lang/go#1.24.4 md/GOOS#linux md/GOARCH#amd64 api/s3vectors#1.17.1 m/E,e` |
| 其他头 | `accept-encoding: identity`、`amz-sdk-invocation-id: <uuid>`、`amz-sdk-request: attempt=1`、content-length | `accept-encoding: gzip`、`amz-sdk-invocation-id: <uuid>`、`amz-sdk-request: attempt=1; max=3`、content-length |
| X-Amz-Security-Token | 不发（static AK/SK 无 session token） | 不发 |

对 server 验签器的含义：**不能假定 SignedHeaders 恒为 host+x-amz-date** —— 必须按请求声明的 SignedHeaders 列表重建 canonical headers（Task B 的实现正是这么做的，两 SDK 11+11 全通过；Go 签 content-length/amz-sdk-* 头也正确处理）。

### 2. 请求 body 序列化差异（同语义、不同字节）

- boto3：`", "` / `": "` 带空格分隔，float 打成 `1.0`；Go：紧凑无空格、**成员按字母序**（`{"indexName":…,"queryVector":…,"topK":…,"vectorBucketName":…}`）、float32 `1` 打成整数。
- server 侧 serde 按 member 名取值，顺序/空白/整数-浮点形态均无关紧要（本轮全过即证据）。
- metadata document：boto3 dict 直出；Go `NewLazyDocument(map)`。未 set 的成员省略。

### 3. 两 SDK 对我们响应的接受面

- **Create* 双发（顶层 ARN + 嵌套 vectorBucket/index 对象）**：boto3 实际解析字段 = `['vectorBucketArn']` / `['indexArn']`；Go populated members = `[VectorBucketArn]` / `[IndexArn]`——两 SDK 对未知成员（含其中 epoch-float creationTime）**静默忽略、不报错**。双发策略被证实无害。
- **QueryVectors**：`vectors[].{key,distance,metadata}` + `distanceMetric` 全被两边解析；metadata document 双向 roundtrip（string/number/bool 三类型全对；Go 数字回读为 document.Number 需转换比较）。
- **PutVectors 成功响应**：200 + 空 body + 应用层零头（无 content-type；trace 里 response headers = `{}`）——两 SDK 均接受（空输出 shape 无解析要求）。
- **响应头极简**（无 x-amzn-RequestId/Date 等应用层头）：两 SDK 均不要求；真实 AWS 会发 x-amzn-RequestId 等，M1 拟真可加，非兼容必需。

### 4. 错误响应映射证据（头+body 双发）

本轮覆盖 409 Conflict / 404 NotFound / 400 Validation / 403 AccessDenied 各两条（两 SDK 各一）：

- wire 形态（server 发出）：`X-Amzn-Errortype: <裸形状名>` 头 + body `{"__type":"<裸形状名>","message":"…"}`。
- boto3：`e.response["Error"]["Code"]` 全等于形状名；机制（botocore 1.43 源）：rest-json 解析器先取 body `__type`，再用 `x-amzn-errortype` 头**覆盖**（`RestJSONParser._inject_error_code`），`:`/`#` namespace 前缀会被剥离。
- Go：`errors.As(&types.XxxException{})` 全命中，`ErrorCode()` = 形状名，`Message` 正确透出；机制（smithy-go v1.28.7 `transport/http/protocol/internal/json/error.go`）：`ResolveProtocolErrorType` **头优先**、body `__type` 兜底、body `code` 第三，`SanitizeErrorCode` 剥 `:`/`#`。
- 结论：**头赢、body 兜底**，双发对两 SDK 均是冗余安全；裸形状名（无 namespace）两边原生支持。

### 5. creationTime epoch float 的接受性

- 类型化路径本轮**无法直接实证**：两 SDK 的 Create* 输出模型都只有 ARN 成员（模型漂移，见 Task B 备注），creationTime 只存在于我们双发的嵌套对象里 → 作为未知成员被忽略（忽略本身即一种"接受"：不因 epoch float 报解析错）。
- 解析器直测（补证据）：botocore `parse_timestamp(1793712491.7435482)` → `datetime`（微秒保留）；smithy-go v1.28.7 json `ShapeDeserializer.ReadTime` 默认 `epoch-seconds` → `smithytime.ParseEpochSeconds` → `time.Time` UTC（毫秒截断），实测 `1793712491.7435482 → 2026-11-03T13:28:11.743Z`。即 **epoch float 是 rest-json Timestamp 的两 SDK 原生默认格式**，未来 Get* 类操作回读无风险。

## 延迟矩阵

（Task C/D 填）

## 条件写矩阵

（Task C/D 填）

## 附录B对拍（Task C：B1/B2/B3(部分)/B7/B8；「客户端实证」= 可由 boto3/Go SDK 行为直接回答，「待AWS实测」= 需真实端点）

- **B1 错误 wire 实际形态**：
  - 客户端实证：头（`X-Amzn-Errortype`）+ body（`__type` + `message`）双发被两 SDK 正确映射为类型化异常——409→`client.exceptions.ConflictException` / 404→`NotFoundException` / 400→`ValidationException` / 403→`AccessDeniedException` 全命中，HTTP 状态码同步；裸形状名（无 namespace）对两者原生可用（botocore `_inject_error_code` 用头覆盖并剥 `:`/`#`；smithy-go `ResolveProtocolErrorType` 头优先 + `SanitizeErrorCode`）。头/body 之一丢失也各能兜底（两边都有两级取码路径）。
  - 待AWS实测：真实端点 `__type` 是否带 namespace（`com.aws…#Shape`）、ValidationException 线上是否恒 400、message 原文与 fieldList 形态。
- **B2 SigV4 payload 形态**：✅ 已闭环（纯客户端行为，无需 AWS）：两 SDK 对 s3vectors **均不发 `x-amz-content-sha256` 头**（那是 S3 专属），payload hash 以 **hex sha256(body) 进 canonical request**（signed payload）——用 trace 独立复算 22/22 条签名全匹配证明；**无 UNSIGNED-PAYLOAD、无 aws-chunked**。差异在签名头集合：boto3 只签 3 头（含 content-type），Go 签 6 头（含 content-length 与 amz-sdk-*）。server 验签须按请求 SignedHeaders 逐头重建（已满足）。
- **B3 Document 边界（部分）**：
  - 客户端实证：顶层混合类型 `{string, number, bool}`（{"tag":"demo","n":7,"ok":true}）两 SDK marshal→server 存储→回读 unmarshal 全程无损；Go 对 nil/空 document 的线形态：成员未 set→省略、`NewLazyDocument(map{})`→`"metadata":{}`、`NewLazyDocument(nil)`→`"metadata":null`（server 三态均接受，null 按"无 metadata"存 NULL）。
  - 待AWS实测：嵌套对象/混合数组/超大数字被拒时的错误形态（ValidationException vs Serialization 族）——需要真实端点发坏数据回读。
- **B7（部分）cosine distance / charset / 空 body 头**：
  - 客户端实证：① Content-Type 精确值 = `application/json` **无 charset**（两 SDK 请求一致；我们响应也不带 charset，均被接受）。② PutVectors 成功 = 200 空 body 且应用层无 content-type，两 SDK 均正常返回空输出。③ cosine distance 数值与 `1 − cos_sim` 假设一致：d(同向量)=0.0、d([1,0,0,0] vs [1,1,0,0])=0.2928932 = 1−1/√2（f32 精度）；tie 时我们按 (distance, key) 排序（本轮 topK 截断处无歧义断言）。
  - 待AWS实测：AWS 的确切公式与 tie 次序声明、AWS 空 body 响应的真实头集合（x-amzn-RequestId 等）。
- **B8 Go SDK v1.17.x schema-serde 与旧版差异**：✅ 已闭环（客户端实证）：s3vectors **自 v1.0.0 起就是 document.Interface + union（VectorData 接口）的新路径 API**，不存在"旧路径"版本可比；v1.0.0 与 v1.17.1 对 nil/空 document 序列化**字节级一致**（省略 / `{}` / `null` 三态同形，离线抓包对比）。结论：B8 对 s3vectors 无风险，问题关闭。
- 范围外（本任务未触碰，M1 或对 AWS 实测再答）：B4 filter 语义、B5 PutVectors 覆盖语义（本轮仅钉死 server 自身行为 = 整行替换、无 legacy 残留；AWS 对拍待实测）、B6 nextToken、B9 tags ARN 编码、B10 merge-insert 叠加。
