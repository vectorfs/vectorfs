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

## 线格式观察

（Task B 填）

## 延迟矩阵

（Task C/D 填）

## 条件写矩阵

（Task C/D 填）

## 附录B对拍

（Task B/C 填）
