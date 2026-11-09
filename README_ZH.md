# vectorfs

**存文件，按语义找——S3 Vectors 兼容，单二进制，Docker 友好。**

[English](README.md)

自托管、单二进制的「S3 Vectors 兼容向量存储 + 文件即向量化」：文件放进来，
语义检索出结果；指向本地目录/内存时是即开即用的 s3vectors 开发与 CI 模拟器。

- 一个 tarball 两个二进制：`vectorfs`（服务端）+ `vf`（客户端）。
- 持久化落在任意 S3 兼容对象存储；本地目录或内存也可以（开发/CI 模拟器
  模式）。没有自研存储引擎——数据始终以开放格式（Lance + JSON）留在你自己的
  存储里。

[![CI](https://github.com/vectorfs/vectorfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vectorfs/vectorfs/actions/workflows/ci.yml)

## 两张 API 面

一个守护进程，两份 wire 合约。命名即地图：

| | **s3vectors API** | **vectorfs API** |
|---|---|---|
| 是什么 | `s3vectors-2025-07-15` 全部 21 个操作——AWS S3 Vectors 协议，逐字节兼容（含错误形状） | `/x-vf/<Op>` 下 8 个原生操作——文件进来，语义检索出结果，命中回指源文件 |
| 由谁验证 | 未经改动的 boto3 与 aws-sdk-go-v2（各 32 步 conformance，CI 全绿） | `vf` CLI 与 MCP 文件层工具 |
| 鉴权 | SigV4，强一致读后写 | 同一套 SigV4，同一套错误模型，异步动词 202 + 状态轮询 |

vectorfs API 背后的能力——解析 → 分块 → 嵌入，外加对账与 GC——由**文件层**
实现（见[下文](#文件层简述)）。

> **状态：预发布。** s3vectors API 面已完备；vectorfs API 交付文本 + 图像双模态
> （PDF 含扫描件 OCR、Markdown、纯文本、docx/html、epub/zip/eml 容器）、混合全文
> 检索（Lance 原生 FTS 上的 BM25）与重排、结构感知提取（页眉页脚剔除 + 目录/图例
> 标签）、内容寻址（页/行坐标、grep 式邻域读取）。音频/视频模态 → M4（见
> [design.md](design.md) §7）。

术语以 [CONTEXT.md](CONTEXT.md) 为准；两张 API 面的命名裁决见
[ADR 0001](docs/adr/0001-two-layer-api-naming.md)。

## 五分钟上手

安装 `vf` CLI（默认 `~/.local/bin`；对发布物的 `sha256sums.txt` 强制校验）：

```bash
curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.sh | bash
# 或: install-cli.sh --with-server   （同时安装 `vectorfs` 守护进程二进制）
# 或: install-cli.sh --version v0.1.0 --to /usr/local/bin
```

Windows（原生、仅客户端 zip——守护进程仍是 linux 二进制；同样强制校验和）：

```powershell
iwr -useb https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.ps1 | iex
```

然后配置一次——`vf config init` 写入 `~/.config/vectorfs/config.toml`
（连接参数优先级 `--flag` > `VF_*` 环境变量 > 该文件）：

```bash
vf config init --endpoint http://127.0.0.1:8787 --access-key <AK> --secret-key <SK>
```

手动方式：从 [Releases](https://github.com/vectorfs/vectorfs/releases) 下载
Linux x86_64/aarch64 的 tarball（`v*` 标签发布，附 `sha256sums.txt`；含
`vectorfs` + `vf`），或源码构建（Rust 1.94+、C 工具链、cmake、protoc）：

```bash
cargo build --profile release-prod --workspace   # -> target/release-prod/{vectorfs,vf}
```

### 文件向量化（文本模态）

```bash
vectorfs init --preset text-rag --data-dir ./data   # 落实配置，下载默认模型
                                                    # （约 1.2 GiB，进度打在 stderr；
                                                    # 支持 HF_ENDPOINT 镜像）
vectorfs serve --data-dir ./data                    # 监听 http://127.0.0.1:8787

export VF_ENDPOINT=http://127.0.0.1:8787 VF_ACCESS_KEY=… VF_SECRET_KEY=…   # init 时打印

vf put report.pdf --bucket docs          # 前台等待直到 INDEXED
vf search "capacity planning" --bucket docs
#  #  SCORE   KEY                  INDEX         SOURCE         TEXT
#  1  0.71    510da8…-text-0       text-fcdd…    report.pdf     Capacity planning for …
```

任何标准工具放入的文件（s3 后端上 aws s3 cp 进源桶，file 后端上直接拷贝）都会被
周期扫描接走（默认 30 秒）——源桶即数据源；删除同样会被对账（chunk 随之 GC）。

`vf` 动词：`put`（`--async`）、`search`（`--mode semantic|keyword|hybrid`、
`--source`、`--kind`、`--rerank`、坐标范围 `--filter`）、`chunks`（枚举单文件的
地址空间）、`chunk show`（grep 式按预算读命中邻域）、`sources list|status`、
`get`、`rm`、`reindex`、`status`。一切也都以 vectorfs API 的操作形式在 wire 上
（`/x-vf/<Op>`，design.md 附录 E）——同一套 SigV4，同一套错误形状。

结构感知检索开箱即用：页眉页脚进不了 chunk；目录与图例 chunk 保持索引但默认不进
结果（`--kind toc` 放行——「工具章节在第几页」是合法查询）；每个命中携带地址
（`p18-19` / `L405-420` / `§3`）——用 `--filter '{"x-vf-page-start":{"$gte":10,"$lte":50}}'`
收窄范围，再 `vf chunk show <key> --around 3` 读邻域而非整文件。

通过普通 AWS SDK 查询也可以（要求同一嵌入模型——默认 preset 下查询向量必须来自
`gte-multilingual-base`）：

```python
client.list_indexes(vectorBucketName="docs")           # -> 派生索引 "text-…"
client.query_vectors(..., queryVector={"float32": q}, returnMetadata=True)
# 命中元数据携带溯源键：
#   x-vf-src-name（文件键）    x-vf-src-uri（短哈希，可过滤）
#   x-vf-model                 x-vf-chunk-text
#   x-vf-chunk-kind（body/toc/caption/legend，可过滤）
#   x-vf-page-start/end · x-vf-line-start/end · x-vf-section（数值，可过滤）
```

### 仅协议模式（SDK 模拟器）

```bash
vectorfs init --data-dir ./data && vectorfs serve --data-dir ./data
```

把 boto3 指过来（凭据来自 `vectorfs init`；此处每条命令都在全新 init 上实际执行过）：

```python
import boto3

client = boto3.client("s3vectors", endpoint_url="http://127.0.0.1:8787",
                      aws_access_key_id="...", aws_secret_access_key="...",
                      region_name="us-east-1")
client.create_vector_bucket(vectorBucketName="docs")
client.create_index(vectorBucketName="docs", indexName="chunks",
                    dataType="float32", dimension=3, distanceMetric="cosine")
client.put_vectors(vectorBucketName="docs", indexName="chunks", vectors=[
    {"key": "a", "data": {"float32": [1.0, 0.0, 0.0]}, "metadata": {"kind": "doc", "len": 4}},
    {"key": "b", "data": {"float32": [0.0, 1.0, 0.0]}, "metadata": {"kind": "doc", "len": 9}},
])
res = client.query_vectors(vectorBucketName="docs", indexName="chunks",
                           queryVector={"float32": [1.0, 0.0, 0.0]}, topK=2,
                           filter={"kind": {"$eq": "doc"}, "len": {"$gte": 5}},
                           returnDistance=True, returnMetadata=True)
print(res["vectors"])   # -> [{'key': 'b', 'distance': 1.0, 'metadata': {...}}]
```

同一条查询走 aws-sdk-go-v2：

```go
client := s3vectors.NewFromConfig(cfg, func(o *s3vectors.Options) {
    o.BaseEndpoint = aws.String("http://127.0.0.1:8787") })
res, err := client.QueryVectors(ctx, &s3vectors.QueryVectorsInput{
    VectorBucketName: aws.String("docs"),
    IndexName:        aws.String("chunks"),
    QueryVector:      &s3vectors.VectorDataMemberFloat32{Value: []float32{1, 0, 0}},
    TopK:             aws.Int32(2),
    ReturnDistance:   true,
})
```

## 桶模型

每个向量桶 `X` 是**一对物理桶**——命名约定即摆放，零配置（design-v3.md §4）：

```
endpoint bucket root                <data-dir>/.vectorfs/（守护进程主目录）
（file 后端即 <data-dir>/；           ├── vectorfs.toml   配置 + 凭据
 memory 后端单一键空间）              ├── models/
├── docs/     ← 源桶：               └── flock           同机二次启动锁
│   └── a.pdf     纯用户命名空间——普通对象，
│                 零系统对象；周期扫描盯全桶
└── docs-vf/  ← 系统桶：根即系统区
    ├── LOCK        桶写者锁（TTL 心跳 + 写者 id）
    ├── bucket.json 归属标记（部署身份）
    ├── sources/…   每文件摄取标记
    └── indexes/<ix>/…  索引元数据 + Lance 数据集
```

- `-vf` 后缀是默认值，每部署配置一次（`vectorfs.toml` 的 `bucket_suffix`——
  仅 file 后端可调，安装时常量；改它等于选择另一个命名空间，绝不迁移数据）。
- 源桶由第一次 `vf put` / PutSource **惰性创建**；`DeleteVectorBucket` 整体移除
  `X-vf` 且**永不删除** `X`——删向量桶不伤你的文件。
- 向量桶名：3–60 字符（63 减后缀），小写字母/数字/连字符，不允许点号（比通用
  S3 桶名更严），且不得以配置的后缀结尾。
- **每桶单写者**。一个守护进程管所有桶是默认部署；多守护进程共享同一 endpoint
  只能在**互不相交的桶集**上，纯协议面扇出到一个桶前需要外部按桶路由（惰性锁
  是执行机制，不是声明）。

## 后端

- `file`（默认）——data dir 即桶根：每个向量桶是目录对 `X/` + `X-vf/`；你的文件
  就是 `X/` 里的普通文件；守护进程主目录 `.vectorfs/`（vectorfs.toml + 模型）
  待在旁边。零配置。
- `memory`——易失，CI 与模拟器用：`vectorfs serve --backend memory --no-auth`
  （无 SigV4、无持久化；秒级启停，确定性）。
- `s3`——任意 S3 兼容端点，**endpoint 形态**（不带桶路径——桶来自上面的双桶
  模型）：`vectorfs serve --backend 's3+http://host:8333'`，配
  `--s3-access-key/--s3-secret-key/--s3-region`（或 `VF_S3_*`/`AWS_*` 环境变量）。
  桶操作必须有凭据——SeaweedFS 把 ListBuckets 限定在账号内，匿名端点只会枚举出
  空。已在 SeaweedFS 4.52 与 Cloudflare R2 上端到端验证（双 SDK conformance、
  条件写、multipart）；裸桶管理动词（create/head/list/delete）在 CI 里跑
  dockerized-SeaweedFS conformance 套件。

## 文件层简述

文件层是实现 vectorfs API 的子系统：扫描、管线、对账、GC。

- 预设：`dev`（仅协议）、`text-rag`（默认模型，零额外配置）、`air-gapped`
  （本地模型路径，`offline = true`，任何流量不出网）。`vectorfs init --preset X`
  落实 `.vectorfs/vectorfs.toml`，按需预取模型（尊重 `HF_ENDPOINT` 镜像；
  sha256 校验，原子落盘）。
- 管线 = **normalize**（路由前的内建有序变换——当前就地解压 gzip 文件；字符集
  转码 / 邮件与归档容器拆分随 v0.3 落地）→ 解析器（pdf-text 走 pdfium /
  `pdf-inspector`【纯 Rust，扫描件响亮报错并给出需要 OCR 的原因，可选位置感知
  Markdown】/ md / txt；每种 mime 的值可以是**有序回退链**，多成员链**按内容
  路由**——PDF 先分类，声明与内容类别最匹配的成员执行，例如扫描件 → 能 OCR 的
  解析器而非逐个试错；容器类拆出子文档重入同一路由——EPUB 在 v0.3 落在这套
  机制上）→ 分块器（token 窗口、按字符计数、CJK 安全）→ 嵌入器（本地 `ort`
  【onnxruntime，load-dynamic】或任意 OpenAI 兼容 `/embeddings` 端点）。配置在
  `.vectorfs/vectorfs.toml`；`vectorfs pipelines check` 校验并给出 TOML 行号。
- 正确性是指纹担保的，不是碰运气：改了解析器/分块器/嵌入器配置，启动会被拒绝，
  直到你显式重嵌（`vectorfs serve --reindex`）。
- GC 只删摄取管理的索引里、key 匹配派生 chunk 格式且 `x-vf-src-uri` 解析到已删
  对象的向量——s3vectors API 面写入的普通向量永不被触碰（有测试）。

## Docker

```bash
docker compose -f deploy/docker-compose.yml up        # data + models 卷，/health 健康检查
```

本地构建：`scripts/docker_build.sh`（大陆网络用 `scripts/docker_build_cn.sh`——
Docker Hub / Debian / crates.io / GitHub release 镜像 + 默认
`HF_ENDPOINT=hf-mirror.com`，均可环境变量覆盖；见脚本头注释）预置二进制并构建
宿主架构。

镜像：`ghcr.io/vectorfs/vectorfs`（多架构 linux/amd64+arm64，debian-slim，
非 root，二进制 + 锁定版本的 onnxruntime/pdfium `.so`；模型留在卷上，绝不进
镜像层）。`VF_PRESET=text-rag` 首次启动时落实配置。`emulator` compose profile
跑易失的 `memory --no-auth` 变体——秒级启动的 s3vectors 替身，供
CI/testcontainers 用。加锁**按桶**（每个 `X-vf` 里一个 TTL 心跳 LOCK 对象）：
一个容器管所有桶是默认部署——**不要**在同一桶集上横向扩这个服务；多守护进程
共享 endpoint 只能在互不相交的桶集上（纯协议面扇出需要外部按桶路由）。

## 限额：AWS vs 本地

结构性限额与 AWS 完全一致地强制执行（错误形状相同）；多租户配额本地不执行。

| 限额 | AWS | vectorfs |
|---|---|---|
| 每次 PutVectors 向量数 | ≤ 500，body ≤ 20 MiB | 相同（强制） |
| GetVectors / DeleteVectors 键数 | ≤ 100 / ≤ 500 | 相同（强制） |
| 索引维度 | 1–4096，创建时定死 | 相同（强制） |
| 每向量元数据 | ≤ 50 键，≤ 40 KB；可过滤键 ≤ 2 KB | 相同（强制） |
| nonFilterableMetadataKeys | 1–10 键，不可变 | 相同（强制） |
| QueryVectors topK | ≤ 10,000（100/页 + nextToken） | 相同（强制） |
| 每查询过滤约束数 | ≤ 100（ENHANCED 索引） | 相同（强制） |
| ListVectors maxResults | 1–1000（默认 500） | 相同（强制） |
| ListVectorBuckets/ListIndexes maxResults | 1–500 + prefix | 相同（强制） |
| 每索引 segment 数 | 1–16 / 0–15，配对 | 相同（强制） |
| 向量值 | cosine 拒绝零向量；拒绝 NaN/Inf | 相同（强制） |
| QPS / 吞吐配额 | 有 | **不执行** |
| 总向量数 / 桶+索引数上限 | 2B / 10,000 | **不执行** |

文件层溯源元数据预算（每 chunk 向量，`x-vf-*` 键）：`x-vf-src-uri` ≤ 100 B
（对象键哈希，可过滤）、`x-vf-src-name` 完整键（不可过滤）、`x-vf-model`、
`x-vf-modality`、`x-vf-chunk-text`（不可过滤，默认回存——按 ingest 节
`store_chunk_text = false` 关闭）。

## MCP

`vectorfs mcp` 是 stdio MCP 服务器，面向 agent 单用户模式——其桶的唯一写者
（与 `vectorfs serve` 共用按桶锁）：

```json
{"vectorfs": {"command": "vectorfs", "args": ["mcp", "--data-dir", "/path/to/data"]}}
```

加 `"--tools", "ro"` 得到只读工具集。s3vectors API 数据面工具：
`list_vector_buckets`、`list_indexes`、`query_vectors`、`get_vectors`、
`put_vectors`、`delete_vectors`；vectorfs API 工具：`search`、`list_sources`、
`put_source`（`put_source` 仅 rw）。

### `vf mcp`——agent 远程模式

`vf mcp` 是第二个 MCP 入口：跑在 **agent 本机** 的 stdio 服务器，把每个工具
调用经与 vf 动词相同的签名 HTTP 代理到 `vectorfs serve` daemon（端点/凭据按
`--endpoint` / `VF_*` 环境变量 / `~/.config/vectorfs/config.toml` 解析——
`vf config init` 之后通常无需任何参数）。它不持锁；daemon 的 SigV4 闸门看得
见每一次调用：

```json
{"vf": {"command": "vf", "args": ["mcp"]}}
```

工具（文件层，两个 MCP 入口同形）：`search`（RAG 入口——融合排序、带回源的
命中，带 `content.text` 与 `location.key`）、`list_buckets`、`list_sources`、
`get_source`、`get_source_data`（取整篇原文）、`status`；`--tools rw`（默认）
再加 `put_source`（`path` 上传本地文件 **或** `content` 内联文本，默认立即
返回——`waitSeconds` 可选前台等待）、`delete_source`、`reindex`。分工：
`vectorfs mcp` 是本地存储上的写者本体；`vf mcp` 驱动远端 daemon——s3vectors
数据面工具留在服务端。

### Agent Skill

[skills/vectorfs/](skills/vectorfs/) 是面向 coding agent（Claude Code、
Codex、ZCode……）的官方 Agent Skill：`SKILL.md` 决策树 + 参考文件（vf CLI、
两个 MCP 入口、daemon 运维、两张 wire 面）。一行装进任一 agent 的 skills
目录：

```bash
curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/skills/vectorfs/install.sh | bash
# 或: … | bash -s -- --to ~/.zcode/skills    （任一 agent 的 skills 根目录）
```

聚焦的伴生 skill [skills/vf-search/](skills/vf-search/) 只教检索这一件事
——查询构造、元数据过滤、命中→回源取全文——面向用户只做搜索的 agent。

## 非目标

- 不自研对象存储后端或存储引擎——持久化永远是既有对象存储或本地目录。文件以
  普通对象存放；vectorfs 不做第二份存储，也不做内容寻址仓库。
- 不自研版本管理、嵌入模型托管/训练平台、聊天 UI、写入时实体抽取、内核态实现、
  插件商店、AWS 控制台 UI 克隆、非 AWS 原生 API 面。
- 不做分布式多写者协调（每桶一写者；各桶 LOCK 对象按 TTL 语义心跳与接管）。

## 兼容性说明

- 错误形状与 AWS 完全一致（`ValidationException` 400、`NotFoundException` 404、
  `ConflictException` 409、`ServiceQuotaExceededException` 402、限流 429……），
  同时以 `X-Amzn-Errortype` 头和 body `__type` 双发，最大化客户端兼容。
- vectorfs API 的 8 个操作位于 `/x-vf/<Op>`——独立命名空间，AWS 面与原生面
  永不互撞；同一套 SigV4、同一套错误模型，异步动词 202 + 状态轮询
  （design.md 附录 E）。
- `vf` 与 fish 的 VirtualFish 同名——两者都用的话请自行 alias。
- 出网：s3vectors API 面零出网。文件层只为模型下载（init/预取；`HF_ENDPOINT`
  镜像，遵守标准 `HTTPS_PROXY`/`NO_PROXY` 环境变量）或显式配置的 OpenAI 兼容
  嵌入端点出网。

## 文档

- [design.md](design.md)——定位、协议规格、架构、里程碑，及附录 E
  （CLI/`/x-vf` 命令全表——唯一权威清单）。
- [design-v3.md](design-v3.md)——v3 桶模型：双桶映射、生命周期/崩溃规则、
  按桶锁、不变量、实现日志。
- [CONTEXT.md](CONTEXT.md)——领域术语表：两张 API 面、桶模型、文件层、后端
  （命名裁决见 [ADR 0001](docs/adr/0001-two-layer-api-naming.md)）。
- [skills/vectorfs/](skills/vectorfs/)——官方 Agent Skill（SKILL.md +
  references）：装进 Claude Code / Codex / 任何支持 skills 的 agent；
  [skills/vf-search/](skills/vf-search/) 是其聚焦检索的伴生 skill。
- [spike/REPORT.md](spike/REPORT.md)——M0 实测（延迟、二进制体积）；
  [conformance/](conformance/)——双 SDK 32 步 conformance 套件。

许可：Apache-2.0（见 [LICENSE](LICENSE)）。
