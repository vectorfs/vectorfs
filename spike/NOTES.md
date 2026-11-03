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

## 线格式观察

（Task C 填；Task B 的 --trace 事件格式与样例见上方「Task B 工程备注」）

## 延迟矩阵

（Task C/D 填）

## 条件写矩阵

（Task C/D 填）

## 附录B对拍

（Task B/C 填）
