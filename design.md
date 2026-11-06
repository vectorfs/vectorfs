# vectorfs 设计文档 v3

- 日期：2026-11-06（v1.0 = 2026-11-03；v2.1 五路审查修订、v2.2 存储布局统一 = 2026-11-04；v3 桶模型重构 = 2026-11-06，见 §0 末行与 [design-v3.md](design-v3.md)）
- 状态：重定位后设计基线。协议面（§4）与引擎（§5.1–5.6）继承 v1.0；文件层与插件系统见 §5.10。变更以 §0 裁决记录为唯一入口；正文术语冲突时**以 §0 最新裁决为准**。
- 文档纪律（G6）：本文件是唯一长期设计文档；spike 结束后按实测结论修订并裁剪，文档总量恒 ≤ 代码量。

## 0. 裁决记录（2026-11-03 首拍 / 2026-11-04 重定位增补）

| 事项 | 裁决 | 备注 |
|---|---|---|
| 方向 | **不重写存储后端**（用户原话："我们不可能重写整个 S3 后端"）。vectorfs = s3vectors 协议层 + 向量引擎网关，持久层 = 可插拔对象存储（S3 兼容远端 / 本地目录 / 内存） | 起因：验证③发现 SeaweedFS 4.52（2026-11-02）已发布 s3vectors 网关、Ceph RGW 已合并主干、Ozone 在途——"唯一开源实现"身位已失；"后端无关薄网关 + 本地模拟器"位仍无人占 |
| K0 治理 | **不做范围冻结**；分步执行、每步验证（门槛见 §7/§10）；借助 coding agent 推进 | 负面清单（§2）与"每步发布即验收"（G7）仍然有效 |
| 命名 | **保留 vectorfs** | GitHub org 已占位（今日创建）；crates.io/PyPI/npm 全空（2026-11-03 撞名审查）；"fs"语义税由 README 第一句承担 |
| 阿里别名层 | **废弃**，只实现 AWS 面 | 理由见 §4.7 与附录C |
| 三验证 | ①协议面 PASS（21 操作、rest-json、SigV4 已核实）②落地帖**押后至 v0.1 发布时以 Show HN 发出**（2026-11-04 裁决：仅 spike 阶段发帖损伤首曝与信誉；需求验证职能由 ③ 的结构性证据承接）③竞争部分翻车（见附录D），触发本日方向改写 | 详细竞争快照存附录D，未来会话不必重查 |
| M0 spike | **2026-11-03 完成，验收门 PASS**：boto3 与 Go SDK v1.17.1 直连 file:// 与 S3 后端各 9/9（SigV4 验签开启）；零 AWS SDK 的 S3 注入路线走通；数据见 `spike/REPORT.md` | 本文件已按其修订：体积预算 155/175、后端矩阵实测化、写路径三步序、optimize 升 M1 必做 |
| **重定位（2026-11-04）** | **两层定位定稿**：底层 = S3 Vectors 兼容的独立开源实现（后端无关，协议面 §4 不变）；上层 = 文件即向量化（存储→解析→分块→嵌入→查询回源）。依据 `research/2026-11-repositioning-feasibility.md`：协议兼容已成 table stakes（SeaweedFS 4.52 全量网关 + conformance harness、Ceph #66066），「兼容 + 文件管线 + 回源」组合经逐项排查为空位 | 原裁决"不重写存储后端"继续有效；"独立"= 不绑定特定存储平台，非自研存储引擎 |
| 负面清单修订 | **推翻 #3「不自带嵌入」**→「不做模型托管/训练平台；内置可插拔嵌入管线」。理由：无嵌入管线则竞争不过存储系内建实现（用户裁决）。#5（写入时实体抽取）保留；#8（插件商店）保留并与插件化架构明确区分 | 宪法修订依 §10.3 记录于此 |
| 多模态证据关 | **通过，结论改写**：「单向量视觉检索死锁」仅 CLIP 时代成立；MLLM 单向量已反超（DSE 85.8 > ColPali 81.3，ViDoRe v1），多向量残余优势仅 top-1 精排（R@1 差 7–9 点，R@200 差 0.63%）⇒ 行业形态「单向量召回 + rerank」与协议面兼容；ColPali 双引擎**不做** | `research/2026-11-multimodal-embedding.md`；M3+ 原门槛条款解除 |
| 命令面 | 服务端 `vectorfs` / 客户端 `vf`：同 crate 双薄 bin，单发布物 | vf 撞名 fish VirtualFish 已知，文档注明 |
| 配置 | **TOML**：实例根 `vectorfs.toml`（吸收 vf.json 职责；**不做 vf.json 兼容读取**——开发阶段直接切换，2026-11-04 裁决），解析链不变 flag > `VF_*` env > 文件 > 默认 | serde deny_unknown_fields，fail-fast |
| 插件系统 | **五轴一模式**（Extractor/Chunker/Embedder/Merge/Reranker），机制进代码、策略进配置（§5.10）；拓扑引擎/DAG/条件表达式明确不做 | |
| 部署 | Docker 一等公民：ghcr multi-arch 镜像 + compose；模型走卷不进镜像层（§5.10.7） | |
| **v2.1 审查修订（2026-11-04）** | 五路子代理独立审查（简洁性/健壮性/协议演化/实用性/工程，记录 §10.6）后修订：**管线指纹 + 显式 `vf reindex`**（修配置漂移静默污染——两位审查员独立命中）；**embedder 模态自声明集合路由**（废模态槽位，同模态多实例合法）；**无独立队列**（pending = 无完成标记的 Source）；**四轴 + 查询选项**（merge 降级）；**M2 收窄为 text 模态 MVP**（图像/docx/OCR → M3）；**openai 走自研 HTTP、不引 fastembed/reqwest**；**init 预取模型**；**GC 派生 key 判据防误删**；概念口径修正"2 新实体 + 4 策略轴" | 方向不变：机制更少了（删队列、删后台迁移），不是更多 |
| 远期规划增补（2026-11-04） | **Web UI、多写架构、插件商店进远期规划（§7 M5+，门槛制）**：Web UI = 只读管理台 + 检索 playground（**chat UI 仍不做**，负面清单 #4 维持）；多写架构 = §5.1 单写者的远期演进；插件商店 = wasm 槽实装 + 第二维护者之后 | 三者均为"信号触发 + 用户裁决"项，不因技术可行而提前 |
| 音视频入规划（2026-11-04） | **语音/视频升格为正式 M4 里程碑**（图片已在 M3）；**视频不设独立模态**——video extractor 解复用为帧(image)+音轨(audio)+可选 ASR 转写(text)，复用既有模态与路由，机制零改动；新增件仅 audio/video extractor + clap embedder；ASR→text 为内容检索主路线 | 依据 `research/2026-11-multimodal-embedding.md`（CLAP Apache-2.0/512 维、Nova 30s 分段、TwelveLabs、Qwen3-VL 视频原生） |
| 命令与 API 面定稿（2026-11-04） | CLI 双命令全表 + 文件层 **8 个 `/x-vf/<Op>` 超集操作**定稿（**附录E = 唯一权威清单**，§5.8/§5.10.6 只留指针）；§4.8 超集通道增补第 ③ 条（独立操作命名空间，前缀隔离防 AWS 撞名）；chunk 文本默认回存（`x-vf-chunk-text`，对齐官方 embed-cli 先例）使 Search 自洽；reindex 由 daemon 执行、vf 仅触发 | PutSource 字节流+元信息头；Search topK 1–1000、filter 复用 AWS 语法 |
| KB 对齐（2026-11-04） | 附录E 按 Bedrock KB API 官方契约修订（`research/2026-11-bedrock-kb-api.md`）：**采纳** 202+状态轮询异步契约、文档状态枚举命名（PENDING/IN_PROGRESS/PARTIALLY_INDEXED/INDEXED/FAILED/DELETING + statusReason）、IngestionJobStatistics 字段名、KnowledgeBaseRetrievalResult 响应形状（content/location/metadata/score）；**偏离** 仅两处且均有据：topK（锚定协议面）、$ filter 语法（KB 与 S3 Vectors 官方本就两套）；对照表见附录E.5 | RetrieveAndGenerate 不做（无 LLM 面） |
| S3 直传裁决（2026-11-04） | 文件上传**复用用户自己的 S3 API**（外部源 `s3-prefix`，M3 提前自 M4+；本地等价 `dir` 监听）——daemon 不在上传路径上，标准工具直达；copy-in（`vf put`）保留为 M2 统一形态与自包含实例根场景；**自建 S3 对象面（PutObject/GetObject 等）明确不做**——第二协议面 + 双份 conformance 负担，负面清单 #1 邻域 | KB DataSource 模型正式采纳（E.5 对照已更新）；外部真源 = 外部列表，补齐对账真源缺口 |
| **存储布局统一（2026-11-04，v2.2）** | **文件即普通对象**：上传直接落实例根普通键（无包装、无副本、无内容寻址源仓库），vectorfs 自有数据全收进 `.vectorfs/` 单一保留前缀（.git 心智）；copy-in/外部源双形态**取消**，统一为 scope 摄取模型（真源 = 实例根列表，`vf put`/`vf rm` = S3 语义便捷包装）；chunk key 改含**对象键**（防同内容文件互踩），内容哈希仅用于"etag 变但内容未变跳过重嵌" | M2 范围反降（无需建源仓库）；`.vectorfs/` 为保留前缀，一实例根一个 |
| 默认模型裁决（2026-11-04） | M2 文本默认 = **gte-multilingual-base**（305M、Apache-2.0、中文良好、下载轻——实用性审查 P1-5 采纳）；bge-m3 一行换（更强、1.1GB）。覆盖 multimodal 报告头部所述"bge-m3 已定"的时点快照 | 报告不回改；本行为准 |
| pdf-inspector kind 增补（2026-11-05，用户裁决） | 新增第四个 M2 extractor kind `pdf-inspector`（firecrawl/pdf-inspector 1.25，MIT，纯 Rust lopdf 系），**双重目的**：PDF 提取的免 .so 替选（扫描件分类 fail-loud + markdown 选项）+ 插件体系"第三方准入"实测。**auto 映射不漂移**（pdf→pdf-text 仍默认——pdfium 是久经考验路径，新增项不改变既有语义）。体积代价 +6.4MiB（159→166MiB，硬顶 175 余 9MiB）——M3 图像模态（image 编解码器）落地前须做**体积专项裁决**（feature 门控 kind / 拆 bin / 接受扩线）。审计结论"体系基本完善"与缺口清单记 §10.8 续 | 零核心改动即接入成功（两段式/枚举/contract/DIP 全复用）；contract 框架据审计补非空文本断言 |
| **Extractor 回退链（2026-11-05，用户裁决）** | `extract` 的每 mime 短键值从单实例名扩为**有序线性链**（字符串或数组形）：按序尝试，Err 或空产出 ⇒ 退让到下一成员，首个非空 Ok 胜出（不跨成员合并），全链耗尽 ⇒ FAILED 聚合各成员原因。**§5.10.8 禁令继续有效**——线性列表不是分支/条件/DAG（与 embedders 列表即 fan-out 同构）；内容路由智能住在 kind 内部（pdf-inspector 的分类 Err 即退让信号），机制层零路由引擎。**trait/contract 零改动**（链是 ingest 编排层机制）。指纹含链组成与序（`"x" ≡ ["x"]` 等价、序变即变）。`auto` 不动（M3 pdf-ocr 落地时再裁决是否升为内建链）。多值语义按产物性质分：extractor 多值 = 回退链（首个胜），embedder 多值 = fan-out（全写） | 实证：混合文字+图片页 PDF，链 `[pdf-inspector, pdf-text]` INDEXED、单 pdf-inspector FAILED——"不能处理交给能处理的"成立；任务书原设想的截断恢复差异样本经实测不存在（两引擎截断行为一致），已换真实差异样本 |
| **Extractor 智能路由（2026-11-05，用户裁决「再好好设计」）** | M2.1 第二步：链前加**三段机制**（probe → affinity → route），全部进代码，**零配置面增长**（§5.10.8 红线不越——无条件表达式/DAG/配置路由表；配置路由表方案已否决）。(1) **ContentClass** 封闭枚举（text-native/scan-dominant/mixed-text-scan/no-text，pipeline/mod.rs，与 Modality 同居，可跨族加值）；(2) **Fit 三级封闭词汇** + `Extractor::affinity(class)` **带默认 Capable**（未声明 kind 永不 Refuse ⇒ 路由退化为纯链序，向后兼容，trait/contract 套件不破）；kind 各自模块声明类级 fit（配置旋钮不改变路由）；(3) **probe**（`pipeline/probe.rs`，pdf 族调 pdf-inspector detect，probe Err ⇒ None=Unknown——保住跨解析器恢复；其他族 None）；(4) **route** 纯函数：配置序首个 Preferred（tie 按配置序）⇒ 否则首个 Capable ⇒ 全 Refuse ⇒ FAILED 聚合拒绝原因（不执行任何成员）。**路由只决定先跑谁**：选定成员 Err/空产出仍按链退让到剩余成员（配置序）——路由是加速与质量防护，链是正确性兜底；单成员链不跑 probe（`members.len() > 1` 零开销门）；路由不进指纹（机制非配置，§5.10.5 漂移语义覆盖）；成功 stderr 行 `INDEXED via {成员} (class=…)`，失败聚合含 class | 实测映射（pdf-inspector 1.25.2）：TextBased→text-native；Scanned→scan-dominant；**ImageBased→mixed-text-scan**（提交的混合样本单 Tj 页实测落 ImageBased 且 pdfium 可提其文字页——设计稿的 Scanned/ImageBased→scan-dominant 若照抄会退化为全拒，已按实测修正）；Mixed→mixed-text-scan；全页 `no_text` ⇒ no-text；损坏 ⇒ Unknown。实证：混合样本 + 链 `[insp, plain]` ⇒ `INDEXED via plain (class=mixed-text-scan)`（inspector 不执行）；image-only + 同链 ⇒ `pdf class=scan-dominant: insp refuses (…); plain refuses (…) — add an OCR-capable extractor`；两 Preferred 并存（text-native 的两 PDF kind）⇒ 配置序首个胜 |
| **normalize 预处理阶段 + 容器角色（2026-11-05，用户裁决「不止解压缩，还可能有其他预处理步骤」）** | 机制泛化为**固定有序内建变换表**，每级（文档与子文档递归）在 extractor 路由**之前**执行。变换三形态：Same 直通 / Transformed 原位（M2 交付 **GzNormalizer**：flate2 树内既有零新 crate，magic 驱动 + mime 从剥包装名推断）/ Expanded（**专走容器角色**，表内出现即响亮 Err）。**判据：机械恒对→normalizer（机制不可配）；格式智力/用户可选→kind**（容器角色：`Extractor::unpack` 默认叶子，kind 要么叶子要么容器永不兼得，容器永不自消化子文档——**子文档重回用户配置的路由**，配置贯通：用户配的 html 链自动适用于 epub 章节；竞品收敛佐证：Tika CompositeParser/PackageParser、unstructured partition_epub、docling EPUB backend 同形态）。**「可配置预处理器轴」明确不建**（无用户证据，v2.1 砍五轴同一纪律；M4+ 出证据再裁）。护栏（机制常数）：MAX_CONTAINER_DEPTH=2 / MAX_CHILDREN=256 / 子字节与 normalize 放大后文档级均过 max_file_size 门（解压炸弹不得走私）+ gz 4GiB 后备顶。指纹零变化（机制非配置）。**M3 范围增补**：epub（容器 kind，OPF spine 定序=格式智力）+ charset→UTF-8 normalizer + eml/mime-multipart 拆分 + 通用 zip 展开 | e2e 实证：`a.txt.gz` 全链 INDEXED（normalize→txt 链）；cfg(test) 容器吐 text/plain 子文档⇒自动经用户配置实例（chunk 集与直配逐字节相等）；嵌套两层允许第三层拒；>256 children 拒；超限 child 跳过但兄弟续摄 |
| **三 crate 拆分（2026-11-05，用户裁决「服务端大一些问题不大，客户端可以小一些」）** | 单 crate 双 bin 拆为三（目录 `crates/{server,client,common}`）：`vectorfs-server`（bin 名 `vectorfs`——引擎+协议面+全部插件 kind）、`vectorfs-client`（bin 名 `vf`——client 传输+CLI，依赖清单即「客户端小」的结构保证，不再靠链接器裁剪运气）、`vectorfs-common`（**准入判据：服务端与客户端都需要的才入**——sigv4 签验同源一套规范化（by construction 属性随迁，禁复制）、代理感知 TLS http 叶、report/版本烘焙、头值百分编码 wire 对）。包名按可发布 crates.io 定形（含 repository/description 元数据），**`cargo publish` 本身另行显式裁决，本次不执行**。**体积门禁分线**：服务端 155/175 不变（拆后实测 166MiB，±1 噪声）；客户端独立设线 **warn>10 / fail>15 MiB**（拆后实测 6MiB，≈2.5x 余量——门禁的意义是防依赖静默回蚀）。服务端以 crate 根 re-export 保持 `crate::sigv4`/`crate::report` 与 `pipeline::embed::http` 壳路径，历史内部引用零改动。**体积专项部分提前消解**：「拆 bin」选项落地后 M3 服务端余量矛盾缓和，feature 门控/扩线等其余选项 M3 图像编解码器落地时再裁 | 实证：`vf --version` ≡ `vectorfs --version`；workspace 全测 304 项绿（cli_smoke 真 daemon e2e 不变——bin 名不变、conformance/发布/镜像面零影响）；deps_guard/machete/deny 全绿；ort/pdfium 的 .so 定位器补 `../../` 前缀适配新目录深度；cargo-machete 清白确认服务端裁掉 hmac/tower-service/hyper-rustls/rustls 四项直依 |
| **插件轴硬化：注册三视图奇偶 + 网关统一（2026-11-05，M3 前地基加固）** | 闭枚举注册的手工同步点收拢为「锚 + 奇偶测试」：`kind_label()` 穷举 match（编译器强制）为身份锚，`KIND_LABELS` 表 + `EXTRACTOR_KINDS` 串 + `from_config` 三视图由 `kind_registry_views_agree` 锁死；扩展名视图改表驱动（`EXTENSION_ALIASES` 常量表，废弃手写 match），`MIME_TO_SHORT_KEY` 每族行 ⇔ 别名行 ⇔ `builtin_extractor` 三视图由 `extension_alias_view_covers_every_family` + `auto_mapping_serves_every_mime_family` 锚定（cfg(test) `ctn` 脚手架豁免）。四 kind 手写的空文件/错 mime 网关收敛为 `pipeline/gate` 共享助手（`Accept::Exact`/`Family` 两形态），**消息模板字节级保留**（statusReason 已持久化面不漂移）；「空 mime=未指定容忍」的现行统一语义补进 contract 断言。零行为变化（除修复族前缀语义外）——4 kind 时代的手工同步成本在 M3 的 8-10 kind 时代会成倍放大，本次消解 | 实证：279 单测 + 13 golden 全绿；网关消息与既有测试逐字节相等；contract 四 kind 全过（含新空 mime 断言） |
| **路由尾序 affinity 分层 + 补救语按类解耦（2026-11-05，用户裁决）** | **修订** M2.1 智能路由裁决的「选定成员退让后剩余成员按配置序」条款为「剩余成员按 affinity 分层（Preferred>Capable>Refuses，同层内配置序稳定）」——`Routing::Run` 增携 class，`chain_try_order` 尾部分层。动机与三段机制同源：Refuses 成员先于 Capable 执行 = 烧一次必败尝试，更有「低质输出先胜」（首个非空 Ok 胜）的质量风险；「错序是质量事故」的立论从「谁先跑」延伸到整个尝试序。Unknown（无 probe 判定）路径逐字节不变。全拒补救语从 route() 硬编码 PDF OCR 指引改为 **`ContentClass::remedy()` 按类表述**：scan 类指 OCR、**no-text 明确「无文字层亦无栅格，OCR 也救不了」**（原硬编码文案对空白文档是错误指引）；补救语随类走意味着未来非 PDF 族不再继承 PDF 专属提示 | 实证：`chain_try_order_routes_first_then_affinity_tiers_the_tail`（同层稳定 + 跨层重排，cfg(test) 容器充当第二个 Capable）；`route_all_refuse_on_no_text_names_the_blank_document_remedy`（断言 no-text 不再提 OCR）；混合样本 e2e 回归不破（INDEXED via plain 不变）；280 单测全绿 |
| **/x-vf/Reindex 作业级串行——静默屏障（2026-11-05，用户裁决；清偿 §10.8 留账）** | reindex_bucket 增第 0 步**静默屏障**（per-bucket quiesce barrier）：置屏障 → `enqueue` 对该桶拒收新作业（被拒源由 flip 转 PENDING + reindex 尾随 wake 自动收敛，零遗漏）→ **不持 write_lock** 有界等待（90s/100ms 轮询）该桶 in-flight 清空 → 枚举+锁内 clear+flip（原机制不动）。屏障等待不持锁（worker 提交需 write_lock，持锁等待成死锁环）；超时**响亮降级**为旧「跳过在飞」语义（严格不劣于 §10.8 原状并记 stderr）；`serve --reindex` 在 workers 孵化前运行天然无争用。语义从「跳过当下在飞的源」升级为「严格在作业之间运行」——清旧块不再可能围绕一个迟到的 worker 提交进行 | 实证（drain_queue 确定性复现竞态形状）：`reindex_barrier_waits_for_inflight_then_flips_it_too`（在飞 job 阻塞 reindex、drain 后**同一源也被翻转**——旧行为会跳过它 flipped=0）；`reindex_barrier_refuses_new_enqueues`（屏障期拒收、撤屏障后同 enqueue 通过）；既有 reindex 全链/gate 恢复/ops_xvf 集成测试不回退 |
| **引擎版本可观测（2026-11-05，用户裁决；接受新增持久字段）** | 「引擎升级=操作员显式重嵌」既定立场补上**可见的一半**：`pipeline::engines` 表（pdfium=runtime_lib_pins 的 PDFIUM_RELEASE、onnxruntime=ORT_RELEASE+ort crate 版本+API 版本、pdf-inspector=Cargo.lock 版本；**parity 测试**对脚本与 lockfile 锁死，改版本不更新表=CI 红）暴露于 /healthz 与 /health（additive 成员，与 version 同属身份信息；uptime 仍限 healthz）与 `vf status`/`vectorfs status --endpoint`；`SourceMarker` 增 `engines` 字段（serde default，旧 marker=None=前观测时代**永不告警**），INDEXED/PARTIAL 完成时写入、flip/失败时清空；`doctor` 的 ingest 检查按桶计数引擎漂移源并给「引擎升级不改指纹，重嵌是操作员显式决定」的补救语（Warn 级，混合代次可接受则不动作）。**不进指纹**（指纹漂移拒启动，引擎漂移只警告——两级严重度刻意分离）。PR 三问记录：新增持久字段有用户裁决 | 实证：schema 互读测试（旧 JSON→None、新字段往返、None 不序列化）；healthz/health additive 断言；reindex 全链断言 INDEXED 载表/flip 清空；真 daemon 冒烟（healthz+health+vf status 三面输出）；单包构建暴露并修复 client tokio feature 缺声明（fs/time——整仓特征合曾掩蔽，恰证「依赖清单即客户端保证」） |
| **vf 命令面借鉴 S3 CLI 惯用形态（2026-11-05，用户裁决「轻量包 A+B+C」）** | 对照 aws s3 / mc / s5cmd / rclone 后采纳三条，**明确不采纳** cp/mv 统一（稀释 put 的摄取等待独有语义）与 sync/rm 递归/alias set（M3+ 出需求再裁，sync 与 scope 扫摄取语义重叠）：(A) **`bucket/key` 路径位置参数**贯穿 get/cat/rm（put 用尾随 `/` 的目标位，aws-cp 式 `docs/pre/`⇒key `pre/<文件名>`；search/reindex 用裸桶名位参）——**flag 优先的向后兼容**：`--bucket` 在场时位置参数一律是完整 key（含 `/` 的 key 以 flag 形态寻址，首个 `/` 前为桶的歧义由显式 flag 消解；开发期实测互斥方案会破坏既有 `get docs/fox.txt --bucket docs` 合法形态而否决）；put 的目标位与 --bucket 互斥为 USAGE 错误；(B) **`vf ls`**：无参=列摄取桶（`aws s3 ls` 形，复用 GetIngestStatus）、带桶=列已摄取文件（与 sources list 同渲染，`--status` 过滤，prefix 客户端过滤——wire 无 prefix 参数）；(C) **`vf cat`**=get 的 stdout 直出（无 --out，能写文件的 cat 只是可发现性更差的 get）。差异动词保持一等公民不动：put 等待 INDEXED、search 回源、reindex | 实证：`cli_s3_style_forms_smoke` 真 daemon 全形态（ls 三形/put 目标位/cat/get 路径/search/reindex 位参/USAGE 错误形状/flag 形态含 `/` key 向后兼容）；既有 `cli_full_command_smoke` 全形态不回退；docker 实机走查（无 scope 配置）路径形态全通；附录E.4 表已按新形态改写 |
| **桶模型重构（2026-11-06，v3；全文见 [design-v3.md](design-v3.md)）** | **双桶映射定稿**：vector bucket `X` ⇔ 源桶 `X`（用户域，纯用户命名空间）+ 系统桶 `X-vf`（vectorfs 域，根即系统区）；**废弃**"单实例根装全部逻辑桶"布局、scope/claims_key 与"物理桶/逻辑桶"双义词汇（病根 = 部署单元/数据面桶/文件命名空间三角色压进一个概念）。**D1-D8**：D1 双桶对（命名约定即放置，零配置）；D2 系统桶根即系统区；D3 桶后缀可配（`bucket_suffix`，默认 `-vf`，仅文件配置、装时刻常量）；D4 源桶惰性创建、DeleteVectorBucket 删净 `X-vf`、源桶永不删；D5 vb 名 ≤63−len(suffix)（默认 60）、类 S3 命名**无点**、不得以后缀结尾；D6 锁粒度 daemon→桶；D7 崩溃重放=空桶收养（无 tombstone）；D8 所有权=部署身份（region+account+endpoint），非实例 UUID。R1-R10、布局、竞态矩阵、验收门与实施裁决（PR1-3 已完成）见 design-v3.md | 本文件 §5.1（锁粒度）/§5.3（布局）按其替换，§4.4/§5.10.3/§5.10.4/§5.10.7 对应行已修订；wire 面 21 op 零变化，超集仅加 `x-vf-placement` 回显；`VF_BACKEND` 收敛 endpoint 形态（`file`/`memory`/`s3+http(s)://host:port`，带桶路径段 fail-fast） |
| **两层 API 定名（2026-11-06，用户裁决；全文见 [CONTEXT.md](CONTEXT.md) 与 [ADR 0001](docs/adr/0001-two-layer-api-naming.md)）** | 底层名 = **s3vectors**（S3 Vectors 兼容协议面，21 op），高层名 = **vectorfs API**（文件即向量化面：`/x-vf/` 超集 + `vf` CLI）；六个落选名否决理由入 ADR 0001（vf API 不自释、vectorized/vectorization 语法反向且撞 Cloudflare Vectorize、file vectorization 长且个性不足、file2vec slogan 造词当 API 面名不稳、filevec 撞 Rust file-vec crate、file/sources API 撞后端词汇且盖不住 Search）。**定名不触碰任何既有面**：wire `/x-vf/`、CLI `vf`、元数据 `x-vf-*`、桶后缀 `-vf` 全不变 | CONTEXT.md 领域术语表建为词汇单源（两层定位/桶模型/文件层/后端四组词条，歧义裁决折叠进 _Avoid_）；「文件即向量化」定为口号（英文 one-liner 同日裁决用 search semantically，§1 已同步） |
| **服务端体积门禁撤销硬顶（2026-11-06，用户裁决）** | 服务端 `vectorfs` bin **尽量小但不设硬上限**——模型下载本身 GB 级（gte 305M / bge-m3 1.1GB），强制限制 vectorfs 二进制大小意义不大。**「尽量小」纪律不变**：default-features = false 姿势、cargo-machete / `cargo tree -d` 裁剪审计照旧——撤的是门禁不是纪律。175MB 降为非阻断信号线（155 旧警戒退役：实测 166MB 常驻超标已成噪声）；**客户端 10/15MB 双线不动**（客户端小是拆分裁决的结构保证，与服务端本裁决无关）。M3 图像编解码器（`image` crate）随本裁决直接进树，原「体积专项裁决（feature 门控 / 拆 bin / 接受扩线）」**销案** | 实证：size_guard.sh 改造后真二进制跑通（server 166MB 显示 no hard cap、client 7MB 双线内、exit 0）；M2 门槛⑥ / §10.8 / §0 三 crate 行的 155/175 历史记录不回改，本行为准 |
| **M3 开工与执行序（2026-11-06，用户裁决「按上表推进」）** | M3 执行序定稿：**图像链闭环先行**——T1 机械地基（Modality::Image / Part 图像载荷 / Query::Image / chunker 旁路 / 路由接线 / image crate 进树，无模型）→ T2 siglip2 embedder（ort+预处理+模态声明+tiny fixture hermetic）→ T3 pdf-pages extractor（pdfium render）→ T4 以图搜图查询 + multimodal/image-search preset → T5 真模型 e2e 验收门；其后 T6 text 面（docx/html extractor + markdown chunker，html 为 epub 前置）→ T7 容器/normalize 增补（epub/zip/charset/eml）→ T8+ 查询超集（$textMatch+tantivy / reranker / merge weighted / IVF_PQ）与 pdf-ocr（先 OCR 引擎选型裁决）。text 链提前交错的替代方案已提出、用户裁定**不采**。每步一提交、主会话复跑验收才进下一步（K0 标准作业） | M3 前置挂账同日清偿：G3 验收门补全（a628496——四项断言一次钉住即过，实现与 design-v3 语义零偏差）；体积专项销案（本表上一行）；§0 回填定名裁决（b8f23e9）；cli_smoke 密闭性缺陷顺手修复（058b7a8——vf 无凭据探针端点取 env、凭据回落真实 config.toml 的混合来源 403，空 XDG_CONFIG_HOME 隔离，默认 HOME 11/11 套件全绿） |
| **M3 图像模态链交付（2026-11-06，T1-T5 五连提交，验收门 PASS with notes）** | 图像链按执行序闭环：**T1** 机械地基 d3a90a1（Modality::Image / Part 图像载荷 / Query::Image / chunker 旁路图像原子入索引 / 模态集合路由接线 / image crate 进树+magic 嗅探校验，additive 红线全守）；**T2** siglip2 双塔 7390954（vision 预处理纯 Rust + ort load-dynamic 全复用；text 塔复用 M2 既有 tokenizers **零新依赖**；doc=[Image]、query=[Image,Text]；BUILTIN_MODELS 增 siglip2-base）；**T3** pdf-pages f7180a2（pdfium image_latest 逐页渲染、dpi 旋钮入指纹、512 页护栏、PNG 编码实测胜出、scan-dominant=Preferred）；**T4** 用户面 ecfab9a（image extractor 入 img 族、/x-vf/Search `query.image` additive、`vf search --image`、multimodal+image-search presets+预取接线）；**T5** 验收门（真模型六样本三类路由行如实、同图 top1 1.0000 回源、文字搜图为真、boto3 直查 x-vf-modality=image、dpi 指纹拒启→`--reindex` 恢复、双 SDK 32+32、docker 实机 healthy、CLI 全形态、门禁全家绿；显示面 chunks 计数三处全模态求和修复 4425e1b）。**默认档位 siglip2-base-patch16-224 为主会话暂定**（M2 gte 先例同构——轻量优先；so400m 一行换，preset 注释有指引），待用户确认或推翻 | 量级记录：put→INDEXED 0.5s/样本；以图搜图 91-114ms；文本查询 51-122ms；server 167.2MiB（无硬顶）、vf 8.5MiB（双线内）、镜像 489MB；观察项：base 档文字搜图排序质量一般（so400m 属后续质量线）、MCP search 图像查询待一行接入、engines.rs dead_code 警告在基线内 |
| **M3-T6 text 面交付（2026-11-06，18d0c1a，验收过）** | `docx`（leaf kind：word/document.xml 是内部序列化非可路由文档 ⇒ 格式智力归 kind，quick-xml 严格 XML + zip 256 条目/256MiB 边读边查炸弹门）、`html`（leaf kind）、`markdown` chunker（chunker 轴第二 kind，注册机制补齐四视图+奇偶锚；窗口递提为轴共享模块，两 kind 一次实现）。**html 解析器裁决：html5ever**——quick-xml 宽容模式在 14 形状对抗基准两例取消资格（script 体内 `<` 静默吞余页、裸 `&` 硬错——embeddings 索引幻觉半页的最恶劣形态），html5ever 14/14（WHATWG 合规）；代价 9 个纯 Rust crate + 手写最小 Rc-DOM TreeSink（不取 markup5ever_rcdom——unofficial 且钉旧版）。CJK 规则：两侧皆 CJK 的空白串整体丢弃。markdown chunker 启用 text-splitter 既有 markdown feature **零新 crate**。新依赖 zip 9.4（T7 地基）+quick-xml（object_store 已锁同版本）+html5ever；auto 不认领 docx/html 族（豁免先例延续） | 实证：443/0 全量；tag_soup 基准测试钉住；docx/html e2e CJK 全链存活、title/script/style 零泄漏；markdown 标题切分过真 worker；体积 +0.98MiB |
| **M3-T7 容器与 normalize 增补（2026-11-06，2505c56，验收过；本行回填其 §0 指纹语义裁决）** | 三容器 kind + charset normalizer 落地：**epub**（格式智力 = OPF spine 定序；unpack 出 spine 序 XHTML 子文档走用户 html 链——§0「配置贯通」闭环；非 spine 资源零产出；10 损坏形状响亮）、**zip**（条目 mime 经唯一表按名推断，未知扩展 octet-stream 透传不预截断——机制 skip 策略住在 worker，kind 复制它即越层）、**eml**（mailparse 拆 multipart，alternative 双 body 都产出，part charset 解码属 kind 非 normalizer——格式元数据）。**charset→UTF-8 normalizer**：BOM+严格 UTF-8 校验机械判定 + 配置 fallback 转码（chardetng 式猜测明确不采），仅 text/* 生效，表序 gz 后（x.txt.gz 装 GBK 组合证明）。**§0「指纹零变化」语义修正（本行即裁决）**：normalize 裁决原文的「不可配/指纹零变化」指**表成员与顺序**（编译期固定，轴禁令完整）；内建变换可携标量参数（dpi 先例移植到 stage）——charset_fallback 落 ingest section，**入指纹**（改 chunk 字节），默认值省略 ⇒ pre-T7 部署指纹逐字节不变。**默认 fallback = gbk**（中文部署主导遗留编码；纯 ASCII 是 GBK/1252 唯一交集且永不触达旋钮；西文一行 windows-1252）；cp936 是 Windows 名非 WHATWG 标签，诚实拒绝 | 实证：479/0 全量；epub e2e 断言 chunk 序=spine 序（fixture 刻意乱序）且非 spine 资源零泄漏；混合 zip（txt+md+docx）各走各链；eml plain/html 双链；GBK fixture 真字节转码 INDEXED 零替换符；指纹三态测试；zip-in-zip 一层过三层拒；体积 server 168MiB；盲区记录：无 BOM UTF-16 机械不可判定 |
| **词法内核改判：Lance 原生 FTS，tantivy 侧车退役（2026-11-06，用户裁决；全文见 [ADR 0002](docs/adr/0002-lexical-kernel-lance-native-fts.md)）** | M3 词法检索（$textMatch）的 BM25 内核**直用 Lance 原生 FTS**——锁定版本线 lancedb 0.41/lance 14.0.2 已含（依赖树无 tantivy，registry 源码核验），`Index::FTS` + `full_text_search` + BM25 分数物化；tantivy 侧车方案（X-vf/indexes/ 持久放置、代际元数据、重建自愈）**整套退役**——索引挂派生表，双索引一致性问题按构造不存在，未索引行 flat 兜底 + optimize_index 增量并段。**原「CJK 零词典 bigram 自研 ≤150 行」裁决前提消失**：分词在 lance 菜单内选（默认 icu，选项 ngram 2,2；tokenizer 进指纹；jieba/lindera 维持禁引）。前置：chunk 文本升派生表顶层 string 列；`/x-vf/Search` 增 searchMode 超集参数；内核薄 trait 隔离保留换核退路；协议面 QueryVectors 保持纯语义。block_size 守默认 128；lance#8812 JSON tokenizer panic 不触 | 源于用户质询「直接使用 Lance 原生 FTS？」；机制研究 research/2026-11-retrieval-friction-mechanisms.md §1 核验背书；真实语料 F1 摩擦证据（friction-backlog：专有名词检索 top-20 零命中被迫 18MB×5 手工 grep）。本表 2026-11-05 行及 §5.7/§5.10.2/§7 的 tantivy 字样已按本行改写 |
| **M3-T8 merge weighted + IVF_PQ 交付（2026-11-06，验收过）** | 查询二件套落地。**merge weighted**：语义裁决 = **rank-based 加权融合**（`w_i/(60+rank_i)`，per-embedder 实例权重）——不同 embedder 的 distance 量纲不可比（模型空间 × 度量），min-max 归一化是「把偶然的 top-k 分布当可比语义」（对 topK/离群点敏感），配权乘原始 distance 更是给不可比量纲乘系数；rank 是唯一免量纲的跨源通货，权重语义即「更信该 embedder 的排序」。全 1.0 权重与 rrf **逐位相同**（IEEE 1.0·x=x，契约断言钉死）。配置面 `[ingest.X] merge = "weighted"` + `merge-weights = { 实例 = 正有限数 }`（kebab，对齐 index-prefix）；校验全响亮：weighted 无 weights 拒、rrf 带 weights 拒（死旋钮）、零/负/NaN 拒（mute 该走 embedders 移除）、未知实例名拒；未配权实例计 1.0（查询模态过滤意味着非每实例都参与每次查询）；merge/merge-weights 均**查询侧、不入指纹**（改排序策略不改 chunk 身份，既有 merge 裁决同款延伸，指纹三态测试钉住）。**IVF_PQ**：触发面裁决 = **附录 E.4 既有 `vectorfs optimize [--bucket] [--index]`**（M1「显式管理动词非自动后台」先例 + 附录表唯一权威已预留此动词），**零新配置键**（索引物理形态非管线语义：无指纹面、无 drift 面、无协议面变更），daemon 永不自动建索引——未跑动词前查询路径逐字节 = pre-T8 flat 精确。训练门 256 行（PQ 256 质心）：以下**响亮拒绝**（BelowGate——扫全桶=SKIPPED 行继续、点名目标=exit 1，拒绝优于静默 flat 假装有索引）；已有向量索引时动词 = **折入增量**（lancedb optimize(Index)，不重训；查询在折入前仍正确——引擎并行扫已索引+未索引数据，store 测试钉 delta 行可查）；参数全用引擎默认（nparts 按行数自动、nsubvectors=dim/16、8bit），旋钮零暴露（出用户证据再加）；重训（分布漂移后）暂无入口，evidence-gated。单写者纪律：动词持本地 flock（同 serve/mcp；memory/s3 无本地锁则显式警告放行）；s6 §6「`vf optimize`」笔误修正为服务端 `vectorfs optimize`（附录 E.4 为准）。附录 E.4「Lance 压缩」半边**不在本次**：碎片压缩无用户证据且改写数据文件是不同风险级，证据到再加同动词后 | 实测：1M×768 file 后端 release、引擎默认参数（无旋钮）——ingest 32.5s、train 36.7s、flat P50/P99 1191.5/1287.5ms vs IVF_PQ 12.7/15.4ms = **P50 94x**（§6 flat 痛点的兑现数字）；recall@10：均匀随机向量 0.047（距离集中使 PQ 排序成噪声，nprobes 20→512 仅 3/50→2/50——数据病理非接线 bug，50k 探针接线验证 1.0），**同规模聚簇（嵌入形）探针 = 1.0 @ P50 12.0ms**；真实部署查嵌入输出（聚簇）不查噪声，病态数据要 nprobes/refine 旋钮即证据门后续；数字与口径全录 store.rs 头注「IVF_PQ 对照」；共存契约 store 测试钉死：建索引后 upsert 的 delta 行 rank-1 可查、删除行查询与 scan_vector_metadata（GC join）双消失、重跑=Folded；既有全部测试数据量 < 256 = 旧路径零变化

## 1. 定位

**vectorfs：单二进制、自托管、Docker 友好的「S3 Vectors 兼容向量存储 + 文件即向量化」——文件放进来，语义检索出结果；指向本地目录/内存时是即开即用的 s3vectors 开发与 CI 模拟器。**

- 英文一句话（README/发布物用）：*Store files, search semantically — S3 Vectors-compatible, single binary, Docker-friendly.*
- 两层结构：底层 = 后端无关的 S3 Vectors 兼容实现（§4，21 op 双 SDK conformance 全绿）；上层 = 文件层（§5.10）：`Source → Extract → Chunk → Embed → PutVectors`，查询经 metadata 回源原文件，RAG 形态。**上层是下层的第一个内部客户**（摄取经 ops 层写入，不旁路）。
- 内置可插拔嵌入管线（本地 ort：gte-multilingual-base 文本默认（换 bge-m3 一行）+ SigLIP2 图像默认（M3）；openai 兼容远端 provider）；不做模型托管平台。

### 1.1 邻居地图（2026-11-04 快照）

| 邻居 | 形态 | 与我们的关系 |
|---|---|---|
| AWS S3 Vectors（GA 2025-12，31 region） | 云托管，协议出处 | 协议面基准；其 embed-cli（官方明说不支持 chunking）/ Bedrock KB（`x-amz-bedrock-kb-source-uri` 回源）验证了"文件→向量→回源"形态，但锁 AWS——留下开放位 |
| SeaweedFS 4.52+ / Ceph RGW(#66066) | 存储平台**内置** s3vectors，绑定自家存储，明确不做 embedding 生成 | 底层名义竞争、实际**可宿主**（M0 实证 9/9）；上层管线是差异化空位 |
| floci / moto / winterbaume | 模拟器（内存态 wire mock） | 与"真实引擎"错位；memory:// 后端对位 LocalStack 的 s3vectors 空缺 |
| Bedrock KB / Cloudflare AutoRAG / AnythingLLM 类 | 上层管线，各锁自家云/协议/UI | 上层对照物：我们 = 开放协议 + 可插拔嵌入 + 回源任意存储；不做 chat UI 与之错位 |
| Turbopuffer / LanceDB | 同为对象存储向量阵营（Lance 系） | 互补大于竞争；LanceDB 官方网关仍是最高哨戒（风险 #1） |
| MinIO / R2 / AWS S3 / Garage | 纯对象存储 | **宿主**（MinIO 社区版已归档，存量可宿主，附录D） |

### 1.2 为什么这个位置成立（v2.0 论据）

1. **协议兼容窗口关闭 ⇒ 差异化上移**：SeaweedFS/Ceph 已内建协议面；「兼容 + 文件管线 + 回源」组合经逐项排查无人占据（`research/2026-11-repositioning-feasibility.md` §B2）。
2. **部件齐全且许可干净**：ort、text-splitter、pdfium、image 生态成熟；内置默认模型许可干净（gte Apache-2.0 / bge-m3 MIT / SigLIP2 Apache-2.0），CC-BY-NC 红线已查明（§5.10.5）。
3. **回源有官方惯例**：metadata 对齐 `x-amz-bedrock-kb-*` 风格，纯 AWS 协议面即可消费已摄取数据（`returnMetadata`）。
4. **技术栈严丝合缝**：Lance 对象存储原生；object_store 三后端同路径；单写者模型使 ingest 无竞态。
5. **史实先例**（v1.0 保留）：平台内建协议功能从不消灭"后端无关的专用轻量层"。

## 2. 永不做（负面清单，进 README）

1. **不自建对象存储后端、不自研存储引擎**——持久层永远是既有对象存储或本地目录（本日方向裁决的宪法条款）
2. 自建版本控制（documentfs D1/D4 教训）
3. 模型托管/训练平台（**2026-11-04 修订**：原"不自带嵌入"已推翻——内置**可插拔**嵌入管线是产品核心；hosting 他人模型的推理/训练服务仍然不做）
4. 应用层 chat UI（2026-11-04 注：远期 Web UI 仅限**只读管理台 + 检索 playground**，见 §7 M5+；chat 仍不做）
5. 写入时实体抽取管线
6. 内核态实现
7. 多写者分布式协调（v1–v3 单写者；2026-11-04 进远期规划，触发条件见 §7 M5+）
8. 插件商店（插件**化架构** ≠ 插件商店：前者是内部 trait + 注册表 + 配置选择，v1 即有（§5.10）；后者是外部生态分发机制，2026-11-04 进远期规划，前置与触发见 §7 M5+）
9. 兼容 AWS 控制台 UI
10. 阿里原生 API 面（别名层已废弃，§4.7）

## 3. 用户与验收

| 用户 | 场景 | 到达路径 |
|---|---|---|
| 用 Open WebUI / LlamaIndex / LangChain s3vectors 集成的开发者 | 本地跑通、不花钱不注册 | `vectorfs serve` + `AWS_ENDPOINT_URL` 环境变量 |
| 在 CI 里测 s3vectors 代码的团队 | 无网络依赖、秒级启停、确定性 | memory:// 后端 + testcontainers/裸进程 |
| 自托管 MinIO / SeaweedFS / Ceph 的用户 | 数据留在自己的存储里 | `vectorfs serve --backend s3://…` |
| 边缘 / air-gapped | 单二进制拷入即用；本地 ort 模型离线可用 | file:// 后端 + ort embedder |
| RAG / 语义检索开发者（不想自建管线） | 文件放入即向量化，`vf search` 返回带出处的 chunk | `vectorfs init --preset text-rag` + `vf put` / `vf search`（multimodal 档 M3） |
| Docker / compose 用户 | 一条命令起服务，卷挂载持久化 | ghcr 镜像 + `VF_*` env 覆盖 |

- **核心验收（M0 起）**：官方 AWS SDK（boto3 + aws-sdk-go-v2，M1 加 Python/Go 双版本交叉）未经修改直连全部操作通过。
- **文件层验收（M2 起）**：**模型就绪后**，preset 物化零额外配置，`vf put` 一个 PDF → `vf search` 回源全链路 ≤ 5 分钟（模型预取发生在 `vectorfs init`，含进度与镜像配置）；同一数据 boto3 `QueryVectors` 能查到并拿到回源 metadata；SDK 直查的接入条件（查询向量须同源 embedding 模型 + 指向派生 index）写进 README。
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

- 模式一 **static**（默认）：`vectorfs init` 生成 access key/secret（打印一次），客户端按普通 AK/SK 配置；服务端验签（canonical request + HMAC-SHA256 链）。
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
| vector bucket | **源桶 `X` + 系统桶 `X-vf`**（v3 双桶，design-v3.md §7：命名约定即放置，零配置；所有权 marker=`X-vf/bucket.json`，记部署身份 D8） | 名 3..=63−len(bucket_suffix)（默认 60）、类 S3 命名**无点**、不得以当前后缀结尾；ARN 自造 `arn:aws:s3vectors:{region}:{account}:bucket/…`（region/account 可配，默认 us-east-1/000000000000）原样；落点经 `x-vf-placement` 超集回显（自托管透明性，非对齐项） |
| index | `<root>/buckets/<vb>/indexes/<ix>/` Lance dataset | 创建时定 dataType=float32 / dimension 1–4096 / metric ∈ {cosine, euclidean}，**不可改**（无 UpdateIndex，符合 AWS） |
| vector = {key, data.float32, metadata} | Lance 行：`key: utf8`(主键), `vector: fixed_size_list<f32, dim>`, `metadata: struct<…>`（动态字段，见 §5.3） | key 1–1024 字符、大小写敏感、任意 UTF-8、每索引唯一；**PutVectors = upsert**（整行替换，metadata 整体替换——*spike* 对拍 AWS 确认"省略 metadata 是否保留旧值"，附录B-Q5） |
| indexMode CLASSIC/ENHANCED | **恒 ENHANCED 行为**（预过滤：先过滤后 KNN） | UpdateIndexMode = 200 空响应；defaultIndexMode 存储并回显；这是本地实现的天然优势（无后置过滤的召回损失），文档明示 |
| metadataConfiguration.nonFilterableMetadataKeys | 记录于 index 元数据；列出的键存 JSON blob 不建标量列 | 1–10 个键，键名 1–63，声明后不可改回（对齐 AWS） |
| bucket policy | `policy.json` 存取回显 | **不实施**（本地单租户）；文档明示；M2 可选最小 Allow/Deny 按 AK principal |
| encryptionConfiguration / KMS | 接受、回显、忽略 | 静态加密是后端对象存储的职责；KMS 错误永不触发 |
| tags | `tags.json`（bucket 与 index ARN 均支持） | TagKey 1–128 / TagValue 0–256 |
| 一致性 | 写后立读强一致（§6） | 对齐 AWS "strongly consistent" 声明 |

### 4.5 限额执行策略（结构限制照抄，配额不抄）

**照抄并校验**（超限报 ValidationException / 对应错误）：PutVectors ≤500 向量且 body ≤20MiB；GetVectors ≤100 keys；DeleteVectors ≤500 keys；dimension 1–4096；metadata 每向量 ≤40KB / ≤50 键 / filterable ≤2KB / non-filterable 键 ≤10；topK ≤10,000（每页 100 + nextToken）；filter 每查询 ≤100 条约束（ENHANCED；我们恒 ENHANCED 故恒适用）；ListVectors maxResults 1–1000（默认 500，累计 >1MB 截断）；ListVectorBuckets/ListIndexes maxResults 1–500 + prefix；segmentCount 1–16 / segmentIndex 0–15（成对出现）。
**不执行**（本地无意义）：QPS/吞吐配额、20 亿向量、10,000 bucket/index 上限。文档以一张"limits 对照表"写明哪些相同、哪些放宽。

**v3 桶模型偏离行（三条，design-v3.md §7）**：①vb 名上限 63−len(bucket_suffix)（默认 `-vf` ⇒ 60，AWS=63）；②vb 名不得以 bucket_suffix 结尾（AWS 允许；防 `X-vf-vf` 递归歧义）；③多 daemon 同 endpoint 拓扑下协议面 op 可返回 ConflictException（AWS 单写者场景不出现）。

**向量校验**：cosine 拒零向量；两 metric 均拒 NaN/Infinity；JSON 数值入站折叠 float32（对齐 AWS 折转语义）。

### 4.6 过滤器

算子（AWS 已核实全集）：`$eq $ne $gt $gte $lt $lte $startsWith $in $nin $exists $and $or`（**无 `$nor`**——那是阿里原生 API 的）。顶层简写隐式 `$eq`；同字段多算子并列 = AND。`$startsWith` 在 AWS 仅 ENHANCED 可用——我们恒可用。metadata 值类型：string / number / boolean / list（list 与 `$eq/$in` 交集=任一元素匹配）。
*spike* 对拍项：顶层多键隐式 AND（附录B-Q4）、类型不匹配行为（Validation vs 空结果）、对 nonFilterable 键下过滤算子的错误形态。

### 4.7 阿里云面：废弃别名层，只取蓝本

阿里兼容的方向是"AWS SDK 可连阿里"（其服务端接受 AWS 客户端），而**不是**其原生 API（`POST /?queryVectorsFusion`、OSS V4 签名、三级域名 `{bucket}-{uid}.{region}.oss-vectors.aliyuncs.com` 定位）与 AWS 面互通。为阿里线格式做别名层 = 双倍协议面 + 零新用户，废弃。
保留的是 **Fusion 语义作为超集蓝本**（多路 knn、RRF/加权 retriever、$textMatch、自定义排序、分页——形态见附录C），进 M3 超集设计。

### 4.8 超集策略（差异化，但绝不污染兼容面）

- **原则**：AWS SDK 走到的路径 100% 与 AWS 语义同构；超集能力只经 ①第一方 CLI/MCP ②既有 op 的 `x-vf-*` 扩展 body 字段（AWS SDK 类型化客户端发不出也收不坏——未知响应成员被各 SDK 忽略）③ `/x-vf/<Op>` **独立操作命名空间**（仅文件层 8 op，见附录E——前缀隔离保证与 AWS 未来新 op 零碰撞）提供。metadata 值域中的 `x-vf-src-*` 保留键属数据面注释（官方先例 `x-amz-bedrock-kb-*`），非 wire 污染。
- M3 超集内容（Fusion 蓝本裁剪）：`$textMatch` 全文（CJK 零词典 bigram tokenizer，自研 ≤150 行，禁引 lindera/gse）；hybrid 多路检索 + RRF/加权合并；自定义排序；（评估）`ip` 距离。默认关闭，显式开启。（merge 的 rrf 基础形态随 M2 文件层先行，见 §5.10.2）

## 5. 架构

### 5.0 组件总览

```
vf CLI ──HTTP/SigV4──►┐    AWS SDK / MCP 客户端
                      ▼
┌─────────────────────────────────────────────┐
│ vectorfs daemon（单二进制, 单写者）            │
│  HTTP 层 → SigV4 验签 → 路由(21 + x-vf-*)     │
│  → 校验(限额/形状) → ops 层                   │
│    查询: Lance scan/KNN(预过滤)               │
│    写入: Lance merge-insert 原子提交           │
│    目录: catalog JSON 对象(条件写)             │
│  ── 文件层（上层；经 ops 层写入，不旁路）──      │
│    ingest worker: Source → Extract → Chunk    │
│      → Embed → put_vectors(x-vf-src-*)        │
│    Source 标记(无队列) + 周期扫描对账            │
│    search: fan-out → merge(rrf) → 回源         │
│    插件注册表: 四轴 + merge 选项（§5.10）        │
│  可选本地缓存(manifest 版本键控)                │
└──────────────┬──────────────────────────────┘
               │ object_store 抽象
  file:// │ memory:// │ s3://(aws/r2/seaweedfs; M3 验证 ceph/garage)
```

### 5.1 进程与锁模型

- 双薄 bin：服务端 `vectorfs` + 客户端 `vf`（同 crate 共享 lib，§5.8）。**写者粒度 = 桶**（v3/D6，替换 v2"每实例根一个写者"）：每活动 vector bucket 一把 `X-vf/LOCK`，daemon 级 LOCK 退役。
- 锁：本地 flock（锁 daemon 本地家 `<data-dir>/.vectorfs/`，同机双启）+ **per-bucket LOCK 对象** `X-vf/LOCK`（条件写创建 + 心跳续期 TTL，心跳携带 writer id=daemon 实例 UUID；防跨机双写）。接管 = 条件写 put-if-match（旧 token 为匹配值）——过期本身即死亡判据，**不依赖跨机不可实现的"确认旧进程死"**（v2.1 修订，与 §5.10.5 一致）。收尾语义全文见 **design-v3.md §5.1**：启动为 `[ingest.X]` 取锁（失败仅跳过该桶+告警，不拖死 daemon）、协议面惰性取锁+空闲释放、失去某桶锁仅该桶泊车、心跳并行化。
- **崩溃恢复 = 客户端重试**：所有写操作 key 寻址、幂等（upsert/delete 天然幂等；bucket/index 创建冲突报 ConflictException 亦幂等）。**无 intent journal**——继承设计的"CAS parquet → SQLite intent → Lance∥tantivy → done 四步写序"整体废弃，理由：那套设计为多存储组件一致性而生，本架构里 **Lance manifest 提交就是唯一事务边界**（原子、版本化、可重放）。
- 多实例：v1 不支持多写者（**桶级单写者**）；不相交桶集可多 daemon 并存，纯协议面多 daemon 需外部按桶分流（design-v3.md §5.1）；读者恒读最新 manifest（Lance 快照读），跨 daemon 只读访问允许但不在支持矩阵内。

### 5.2 存储后端抽象

- **接入路线定稿（M0 实证，零 AWS SDK）**：object_store 0.14（aws feature）自建客户端 → 实现 `lance_io::ObjectStoreProvider` → 自定义 lancedb `Session` 注入。例外记录：需直接依赖 lance-io（lancedb 未 re-export provider trait）。代价：aws feature 硬绑 aws-lc-rs（cmake 构建依赖），体积 +67.7MiB——接受（风险 #11）。
- v0.1 矩阵：`file://`（默认，零配置）、`memory://`（CI/测试）、`s3://`——**已实测：SeaweedFS 4.52（M0，9/9+延迟+条件写）与 Cloudflare R2（2026-11-04，双 SDK 32+32 conformance+条件写+doctor 探针，region=auto）**；AWS S3 / MinIO 存量——If-None-Match:\* 文档核实，未实测。R2 首测暴露真 bug：serve 绕过 `StorageConfig::backend()` 致 S3 凭证未注入、请求未签名（SeaweedFS 匿名 Admin 掩蔽此问题，R2 以 400 InvalidArgument 揭露）——已修复并加回归测试；教训：**匿名宽容后端会掩蔽签名路径缺陷，后端 conformance 必须含一个强制鉴权的真实云**。R2 的 `use_constant_size_upload_parts` 已按 endpoint 自动启用。后续验证扩展（M3，§7）：Ceph RGW、Garage/Silo。
- **条件写矩阵（M0 结论）**：LocalFS / SeaweedFS 实测，AWS / R2 / MinIO 文档核实——全部支持 If-None-Match:\*；Lance manifest 提交即走此路径 ⇒ **提交原子性在全部目标后端成立**；LOCK 对象降级保留为防御性设计，非必需。

### 5.3 数据布局与 Lance schema（v3：双桶布局，替换 v2.2 实例根布局；全文见 design-v3.md §4）

**v3 双桶**：vector bucket `X` ⇔ 源桶 `X` + 系统桶 `X + bucket_suffix`（默认 `X-vf`，D3）。**用户文件 = 源桶内普通对象**，任何 S3 工具直达读写删，无包装无副本（源桶零系统对象，扫描=整桶）；vectorfs 全部持久自有状态收进系统桶**根即系统区**（D2，无内层前缀）。三后端桶相对键集逐字节同构：s3 = 真 S3 桶对；file = `<data-dir>/X/` + `<data-dir>/X-vf/`；memory = 共享键空间内 `X/…` 与 `X-vf/…`。

```
S3 endpoint（或 <data-dir>/ 作桶根，或 memory）      <data-dir>/.vectorfs/（daemon 本地家）
├── docs/                     ← 源桶：纯用户命名空间   ├── vectorfs.toml（配置+凭证）
│   ├── manual.pdf              零系统对象；扫描=整桶  ├── models/
│   └── inbox/report.docx       （S3 API 直传→下周期  └── flock（本地同机双启锁）
│                                被摄取/GC）
└── docs-vf/                  ← 系统桶：根即系统区（D2）
    ├── LOCK                  ← per-bucket 写者锁（条件写+心跳，writer id）
    ├── bucket.json           ← 所有权 marker（部署身份 region+account+endpoint，D8）
    ├── policy.json  tags.json
    ├── sources/<keyHash>.json    # 逐文件标记：对象键、etag、内容哈希、管线指纹、状态机
    └── indexes/<ix>/{index.json, tags.json, data/}   # Lance dataset 原样
```

保留字收敛：系统桶根级 `LOCK`/`bucket.json`/`policy.json`/`tags.json`/`sources`/`indexes` 为机制固定位（用户输入到不了桶根）；file 后端 data-dir 根级 `.vectorfs/`（daemon 家）；vb 名不得以后缀结尾。源桶无任何保留字。
- Lance schema：`key: utf8 not null`、`vector: fixed_size_list<float32, dim>`、`metadata: struct<动态字段>` + `metadata_json: utf8`（nonFilterable 键的原文）。
- **动态 schema 演化（M0 闭环，附录B-Q10）**：批内出现新 metadata 键 → `add_columns().transform(AllNulls(仅新子字段))` 演化 → merge_insert，三步在同一请求内完成。实证：跳过演化直接提交会被干净拒绝（fail-closed，无静默损坏）；演化后旧行 NULL、新行带值。值类型冲突（同键先 number 后 string）→ ValidationException。
- 一代一表原则保留的简化版：index 创建参数即"代"；嵌入模型版本由 index 派生键隔离（换模型 ⇒ 派生新 index，§5.10.1）。

### 5.4 写路径

1. 校验（限额、形状、向量合法性、metadata 键类型冲突）。
2. 单次 Lance `merge_insert("key")` 原子提交（upsert）或 delete-merge（DeleteVectors，≤500 keys）。
3. 200 空 body。
- 批量纪律：一请求一提交（严禁逐条 add——继承教训）；merge-insert 内部即批。
- **M0 实测注记**：表 >50 万行后每批提交线性变慢（100k→1M：269→333ms@file、292→408ms@s3）——`optimize`（fragment > 32 且空闲时压缩，管理命令非自动后台任务）由 M2 **提前至 M1 必做**。

### 5.5 查询路径

- **flat-first**：< 100,000 向量不建 ANN 索引，全表精确扫描（arrow 并行 + 预过滤 = 召回 100%，确定性排序 tie-break by key）。>10 万后由 `vectorfs optimize`（附录 E.4 服务端管理动词，M3-T8 交付）显式建 IVF_PQ（Lance 原生），查询自动走索引；训练门 256 行，行数不足响亮拒绝（详见 §0 M3-T8 行）。
- ENHANCED 语义 = 标量谓词下推预过滤 → KNN（Lance scan filter + vector search 一体）。
- **分页 token**：opaque base64 `{manifest_version, last_key/offset, segment}`——manifest 版本入 token = **快照隔离**（翻页期间写入不改变结果集）。
- segment：`hash(key) % segmentCount` 分片（可并行，语义与 AWS 一致：segments 划分 keyspace）。
- 距离语义：cosine = 1−相似度（M0 钉死）；euclidean 注意 lancedb 原生返回**平方 L2**，查询侧须补 sqrt（M0 发现，保序）；`returnDistance/returnMetadata` 默认 false。
- **M0 实测注记**：flat 全表扫 1M×768 = 398ms(file)/972ms(s3)——1M 规模需 IVF_PQ（M3 规划）；spike 的 filter 走全表扫+内存过滤（1M 时 1.0–1.4s），产品实现谓词下推后过滤查询将显著下降。

### 5.6 缓存

- manifest 不可变 → 数据页本地缓存以 `{manifest 版本, 文件, 偏移}` 键控，天然一致；远端后端时缓存显著加速重复查询（*spike* 量化）。
- 缓存目录默认 `<root>/.vectorfs/cache/`（本地缓存，不落文件命名空间；file 后端时退化为 OS 页缓存即够）。
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
| ~~tantivy~~ | **永不引入**（2026-11-06 改判，ADR 0002）：词法内核直用 Lance 原生 FTS（锁定版本线 lancedb 0.41/lance 14.0.2 已含） | v1 过滤由 Lance 标量谓词承担；M3 词法检索复用树内 Lance，零新依赖；arrow 去重风险彻底不引入 |
| CLI/MCP | clap + rmcp(3.x stable)；**双薄 bin**：`vectorfs`（服务端）/`vf`（客户端） | 同 crate 共享 lib，单发布物 |
| 配置 | toml + serde（`vectorfs.toml`，deny_unknown_fields；不做 vf.json 兼容） | 解析链 flag > `VF_*` env > 文件 > 默认 |
| 文件层 | pdfium-render（.so load-dynamic）、image（default-features=false：png/jpeg/webp）、text-splitter（char/tokenizers 计数，禁 tiktoken 联网特性）、ort（.so load-dynamic，**不引 fastembed**——自管模型清单与 load-dynamic 冲突）；openai provider 走**自研 HTTP**（复用 hyper 树 ~150 行，与自研 SigV4 同先例，零 reqwest）；音视频（M4）：symphonia（纯 Rust 音频解码）、ffmpeg（.so load-dynamic，解复用） | v2.1：deny.toml ban openssl-sys；.so pin 版本+SHA；CI 加 ldd + 小模型真实 embed 冒烟 |
| 出网 | 协议面：无。`openai` embedder 显式配置才出网 | 2026-11-04 修订（原"无出网"仅适用纯协议面） |
| 其余 | cargo-deny（拒 GPL/AGPL/CC-BY-NC）+ cargo-machete + `cargo tree -d` 进 CI；release-prod profile 强制 lto=**thin**+cgu=1+strip（2026-11-04 裁决：fat 冷链 ~25-35min 被 CI 托管 runner ~30min 回收窗口四连杀；thin 冷构建 ~12min，体积 137→155MiB——踩 155 警戒线、仍低于硬顶 20MiB；fat 可由自建者以 `CARGO_PROFILE_RELEASE_PROD_LTO=fat` 覆盖）；datafusion 为不可裁剪的引擎固定成本；**体积分线（2026-11-06 用户裁决修订）**：服务端 `vectorfs` bin **不设硬顶，175MB 仅作非阻断信号线**（模型下载本身 GB 级，强制限制二进制大小意义不大；「尽量小」纪律不变——default-features = false 姿势与裁剪审计照旧；155 旧警戒线退役，实测 166MB 已成常驻噪声）；客户端 `vf` bin **≤10MB 警戒 / 15MB 硬顶不变**（客户端小是三 crate 拆分裁决的结构保证） | CI：ubuntu x64+arm64 + macos check + size-guard（双 bin 各按各线）；发布冒烟在 ubuntu:20.04 容器跑全链路 |

### 5.8 客户端面（薄，v1）

- 双命令面与 `/x-vf/*` API 的完整清单见 **附录E**（唯一权威）：服务端 `vectorfs`（init/serve/status/doctor/optimize/pipelines/mcp）+ 客户端 `vf`（put/search/sources/get/rm/reindex/status）+ `GET /health` 探活。
- MCP（rmcp server，单写者模式同锁）：既有 6 个数据面工具（list buckets/indexes / query / get / put / delete），M2 增 `search` / `put_source` / `list_sources`（附录E.3）；**agent 一等契约**：结构化错误 `{code, exit, message, hint}`（hint=给 agent 的行动指令）、退出码 grep 兼容、stdout=结果 / stderr=进度、`--json` 带 schema_version、零交互、幂等、确定性输出（tie-break by key）。
- 纯 SDK 数据面照旧：boto3/Go 用户不经 CLI（21 op 不变）；第一方 CLI/MCP、既有 op 的 `x-vf-*` 扩展字段、`/x-vf/<Op>` 命名空间是超集仅有的三个出口（§4.8）。

### 5.9 继承设计处置表

| 继承项 | 处置 | 理由 |
|---|---|---|
| Rust 单二进制 / Linux x64+arm64 首发 / macOS CI 绿灯 | 保留 | |
| Lance 基座 / flat-first / 批量提交 / fragment>32 才 optimize / RSS-CPU watchdog | 保留 | watchdog 简化为 daemon 自监控 |
| flock 拉起协议 | 保留（本地层）+ LOCK 对象（跨机层） | |
| 四步写序 + SQLite intent journal | **废弃** | §5.1：Lance 原子提交即事务边界 |
| rusqlite | **移出 v1** | catalog 即对象 |
| ~~tantivy + CJK bigram tokenizer~~ | **作废**（ADR 0002）：lance 无自定义 tokenizer 钩子，分词在 lance 菜单内选——默认 `icu`（内置词典、中英混排稳），选项 `ngram 2,2`（真 bigram、中文最优但 Latin 同被 bigram 化）；tokenizer 进管线指纹；jieba/lindera 外部模型目录**维持禁引** | 「CJK 零词典 bigram 自研 ≤150 行」原裁决前提消失，随侧车方案一并退役 |
| ort load-dynamic / models.toml / .so 内嵌释放管线 | **解冻进 M2 文件层**（2026-11-04 裁决）：ort/pdfium 走 load-dynamic .so；模型清单并入 `vectorfs.toml` 的 `[embedder.*]` | §5.10 |
| agent 接口契约 / `vf` 短名 / 结构化错误 | 保留 | `vf` 在 cargo/brew/apt 可用（fish 用户的 VirtualFish 冲突已知，文档注明）；命令角色改为客户端 CLI（服务端命令为 `vectorfs`） |

### 5.10 文件层与插件系统（2026-11-04 新增；v2.1 按五路独立审查修订，记录见 §10.6）

#### 5.10.0 公理

1. **机制进代码，策略进配置**：编排算法固定（全量测试覆盖）；会变的（提取器、块参数、模型、合并策略）进 TOML。配置里出现的每个概念必须对应"用户真的会换的东西"。
2. **上层是下层的第一个内部客户**：ingest worker 进程内直调 ops 层（同 mcp.rs 路径，不走 HTTP）——限额/校验语义自动适用（500/批、40KB metadata、批量切分规则见 5.10.3），conformance 套件同时保护内部路径。AWS SDK 客户端可查询已摄取向量，但**实用前提是查询向量出自同源 embedding 模型**（README 写明接入条件，不夸大）。
3. **正确性靠指纹，不靠纪律**（v2.1）：完成标记与 index 元数据记录**管线指纹** = hash(extractor + chunker + embedder 实例配置)；配置变更 ⇒ 启动 fail-fast，提示显式 `vf reindex`——不做隐式后台迁移引擎。

#### 5.10.1 概念（2 个新实体 + 4 个策略轴 + 1 个摄取配置节 + Source 标记附属状态）

| 概念 | 定义 | 存储 |
|---|---|---|
| **Source** | **源桶里的普通对象**（标准 S3 API 直达；`vf put` / PutSource 只是便捷包装：落源桶普通键 + 即时触发摄取）。v3 起源桶整体即该 vb 的命名空间（桶间零共享是物理事实）；桶内认领可选 `include`/`exclude` glob 过滤，省缺 = 整桶 | 对象本体在源桶 `X`；标记在 `X-vf/sources/`（对象键、etag、内容哈希、指纹、状态机） |
| **Embedder** | 内容→向量引擎；实例声明 {dim, doc/query 模态}；**一个实例 1:1 一个 index**，index 派生键 = hash(model, dim, 配置)——同名实例改 model 也派生新 index，杜绝静默混写 | index 元数据 + vectorfs.toml |
| Chunk | **vector 的文件层视图**（非新实体）：key = f(**对象键**, 模态, 序号)（保持短）——**含对象键防同内容文件互踩**（两个相同内容的文件是两个 Source 各自成 chunk；内容哈希用于 etag 变更后"内容未变跳过重嵌"）。metadata 带 `x-vf-src-*` 回源键——**完整键集**：`x-vf-src-uri`（对象键短哈希 ≤100B，filterable，**GC join 键**）/ `x-vf-src-name`（对象键全文，non-filterable）/ `x-vf-src-page`（页/帧/段号，M3）/ `x-vf-modality` / `x-vf-model` / `x-vf-chunk-text`（non-filterable，**默认存、可关**——Search 直接返回文本，对齐官方 embed-cli 的 SRC-CONTENT 先例） | Lance index 行 |
| `[ingest.<bucket>]` | 每 bucket 的摄取配置节（缺省 ⇒ 纯协议面，文件层不介入） | vectorfs.toml |

路由规则（机制，非配置）：Part 模态 → **所有** doc-模态匹配的 embedder 实例（同模态多实例 = fan-out 全写；查询侧对称 fan-out 后合并）——不需要"模态槽位"配置项。模态为封闭枚举（`text` / `page-image` / `image` / `audio` / …，增量加值）；**视频不设独立模态**——video extractor 解复用为帧（image）+ 音轨（audio）+ 可选 ASR 转写（text），三种既有模态全复用，路由机制零改动。稀疏检索不走向量列：由 M3 `$textMatch`（**Lance 原生 FTS，ADR 0002——tantivy 侧车 2026-11-06 退役**）承担，规避 sparse 向量在 float32 定长协议下的表达问题。

#### 5.10.2 插件系统：一个模式，四个策略轴 + 查询选项

统一注册模式（**Rust 形态定稿 v2.1**）：配置层两段式——先取 `kind` 字符串、再反序列化到 per-kind config struct（各自 deny_unknown_fields，报错带 TOML 行号；注意 serde 内部标签枚举不支持 deny_unknown_fields，不可单枚举一步到位）；运行层 `enum` 分发（kind 集封闭、exhaustive match、零 vtable），`dyn` 只留给 v2 wasm 槽。**DIP 边界**：ort/pdfium/HTTP 类型只准出现在各 kind 模块内，核心只见 trait 与统一 `PipelineError`。

| 轴 | 侧 | 契约（v2.1 修正签名） | 内建 kind（按里程碑标注） |
|---|---|---|---|
| Extractor | 摄取 | `{bytes, mime} → Vec<Part(模态)>` + `affinity(ContentClass) → Fit{Preferred,Capable,Refuses}`（默认 Capable，未声明即纯链序）+ `unpack → Option<Vec<ChildDoc>>`（默认 None=叶子；容器角色：子文档重回 normalize+路由，永不自消化）；按 mime 配置有序链（多成员先 probe→route，§5.10.3/§0 智能路由裁决） | pdf-text / md / txt（M2）；pdf-inspector（M2.1 增补 2026-11-05，见 §0：纯 Rust lopdf 系、扫描件分类 fail-loud、markdown 输出选项——插件体系验证试金石，auto 默认不漂移仍 pdf-text）；pdf-pages(dpi) / pdf-ocr(lang) / docx / html / image（M3）；audio(分段/VAD) / video(解复用→帧+音轨+可选 ASR)（M4） |
| Chunker | 摄取，仅 text 模态（图像类原子入索引） | `&[TextPart] → Vec<Chunk>`；模态分派在机制层 | token-window(size, overlap)（M2）；markdown（M3） |
| Embedder | 摄取+查询 | `embed_query(Query) → Vec<f32>`（`Query = enum {Text, Image, Audio}`）+ `embed_documents(&[Chunk])`；声明 {dim, doc/query 模态}；ort dim 启动探测、openai dim 配置声明（不出网探测） | ort / openai（M2）；qwen3vl 统一评估（M3）；clap 声学（M4）；远端视频（Nova/TwelveLabs）作 provider 扩展 |
| Reranker（M3） | 查询 | `query × candidates → scored` | cross-encoder |

查询选项（非轴）：`merge = rrf | weighted | first`——纯函数 `fn(&[HitList]) -> Vec<Hit>`，全序确定、tie-break by key。

contract test 最低断言（四轴各一份 + 查询选项；LSP 防线 + 未来第三方/wasm 准入门槛）：
- **Extractor**：同输入两次输出全等（确定性）；错 mime/空文件/截断字节 → Err 不 panic；产物模态 ∈ 封闭枚举。
- **Chunker**：拼接不变量（块串接 ≡ 原文，overlap 计入）；overlap < size 启动校验；CJK 不截半个字符。
- **Embedder**：两路调用 dim 恒等且 = 声明值；无 NaN/Inf；空输入不 panic；探测 dim ≠ 声明 ⇒ 启动失败。
- **Reranker**（M3 补）：分数全序确定、tie-break by key；candidates 顺序不影响重排结果。
- **Merge（查询选项）**：单路输入恒等；输出全序确定。

#### 5.10.3 数据流（固定机制）

```
ingest: Source → normalize（固定有序内建变换表：Same/Transformed/Expanded 三形态，
        每级递归先于路由；M2=[gz]，M3 增 charset/eml/zip）
        → extractor 链（按 normalize 后 mime 短键取有序链；多成员时先 probe 内容分类
        → 按各 kind 的 affinity 声明路由：配置序首个 Preferred ⇒ 首个 Capable
        ⇒ 全拒即 FAILED 聚合原因；选定成员 Err/空产出仍按链退让——
        路由是加速与质量防护，链是正确性兜底；分类未知/单成员 = 纯链序；
        auto = 内置 mime→kind 单射）
        → Parts → 每模态路由到全部 doc-模态匹配的 embedder
        → chunker（仅 text 模态）→ embed → 分批 put_vectors(x-vf-src-*)
**顺序裁决（normalize 为什么在 route 前，2026-11-05 复核）**：不变式 = **normalize 是每级第一个触碰字节的东西，路由三段（短键/probe/affinity）永远只见 normalize 后状态**；normalize 之前只有扫描层按文件名的廉价认领（提示而非变换，magic 校验兜住谎报文件名）。五条强制理由：①**短键身份依赖 normalize**——`.txt.gz` 的可路由身份是 text/plain，route 先行则按 application/gzip 查链无承接即被跳过，gz 透明性的全部价值依赖路由看穿包装；②**probe 只对真实内容有意义**——`scan.pdf.gz` 先解压才能分类 text-native/scan-dominant 选对提取器，route 先行则 probe 吃压缩字节得 Unknown，智能路由对一切包装文件静默退化纯顺序；③**放大门必须在扩张点**——解压炸弹守卫贴着变换出口（normalize 后、任何下游之前）；④**确定性死端先泊车**——损坏 gz 后置会被翻译成"成员退让"，链逐个白跑同错且语义错（损坏不是退让）；⑤**成员/子文档同一性**——每成员惰性变换重复解压且引入成员间字节不一致裂缝；一处变换一处门一行日志，全员见同一终态。被否决的替代位：route 原始 mime（丢①②）、每成员惰性变换（破⑤）、route→normalize→再 route 的双相循环（为省一次**有界**解压引入再路由循环=迷你拓扑引擎，§5.10.8 恰禁；且未认领包装已由扫描层文件名剥包在源头过滤，唯一残余浪费是谎报文件名——有界且响亮失败）。"某些变换只对某些提取器有意义"的忧虑由判据本身隔离：那是格式智力，归 kind 内部，不是 normalizer。

query: 输入 → query 模态匹配的各 embedder → embed_query → 各查各 index（filter 可带）
       → merge → (rerank, M3) → 回源（x-vf-src-*）
```

- **批量切分**（公理 2 的落地）：批 = min(500 向量, 20MiB − metadata 估重)，顺序提交；崩溃重跑以幂等 key 收敛。
- **摄取触发（统一模型；v3 换轴，源桶即数据源）**：周期增量 list-and-diff（扫描各**源桶**整桶，可选 include/exclude 认领过滤；默认 30s 可配）+ `vf put`/PutSource 即时触发。**真源 = 源桶对象列表**——S3 API 直接上传/覆盖/删除的对象在下周期被摄取/更新/GC（KB ingestion job 同款扫描模型；S3 事件通知留作加速项）。daemon 不在上传路径上。GC/orphan sweep 作用域 = 本 daemon 持锁的桶集合，绝不触碰未持锁的桶（design-v3.md R5）。
- **首次 put 幂等建域**：自动创建配置声明的 bucket 与派生 index（存在则校验 dim/metric/派生键，匹配复用、不符报错）。
- **加载期 fail-fast**（数个 if，不是引擎）：extractor 输出模态无 embedder 承接 ⇒ 拒启；index 派生键与配置不符 ⇒ 提示 `vf reindex`；overlap ≥ size ⇒ 拒启；报错带 TOML 行号。

#### 5.10.4 配置形态（vectorfs.toml，一屏为限）

```toml
bucket_suffix = "-vf"                  # 桶后缀（v3 新增；省缺即此值。仅文件配置、装时刻常量，
                                      # 校验/漂移语义见 design-v3.md R6）

[embedder.text]                       # 全局具名实例；一实例 = 一 index
kind = "ort"
model = "gte-multilingual-base"       # M2 默认：305M、中文良好、下载轻；换 bge-m3 是一行
[embedder.remote]
kind = "openai"
endpoint = "https://api.bigmodel.cn/v1"
model = "embedding-3"
dim = 2048                            # 远端 dim 配置声明
[embedder.local-only]                 # air-gapped 变体：本地路径 + offline
kind = "ort"
path = "/models/bge-m3"
offline = true

[extractor.pdf]
kind = "pdf-text"

[ingest.docs]                         # vb "docs" ⇔ 桶 docs（源）+ docs-vf（系统）
extract = { pdf = ["insp", "pdf"], default = "auto" }
                                     # mime 短键 → extractor 实例名或有序回退链
                                     # （数组：Err/空产出 ⇒ 退让下一成员，首个非空 Ok 胜；
                                     #  "auto" 只可独占整值；多值语义详见 §5.10.3/§0 回退链裁决）
chunk = { kind = "token-window", size = 512, overlap = 64 }
embedders = ["text"]                  # 实例集合；同模态多实例 = fan-out
merge = "rrf"
index-prefix = "docs"                 # 派生 index 名前缀（可选）
# include = ["**.pdf"]                # 可选桶内认领过滤（v3 替换 scope）；省缺 = 整桶
# exclude = ["**.tmp"]
```

preset = 内建 TOML 模板；`vectorfs init --preset X` **预取模型**（进度条、sha256 校验、tmp+rename 原子落盘、`HF_ENDPOINT` 镜像可配）并物化配置。M2 交付三档：`dev`（无模型纯协议面）/ `text-rag` / `air-gapped`；M3-T4 增 `multimodal`（gte+siglip2 双塔，pdf 两段链 pdf-text→pdf-pages 按 probe→affinity 分流，img 走 image kind）与 `image-search`（siglip2 单塔，仅 img 路由——pdf 页图属 multimodal/scanned-docs 语义，刻意不含）；`scanned-docs`（OCR）属 T8+。

#### 5.10.5 健壮性（v2.1 修订）

- **无队列设计**：不建独立任务队列——待摄取 = "无标记，或 etag/内容哈希与标记不符 ⇒ 置 PENDING"（FAILED 且 retriesExhausted 者除外），worker 周期扫描 + `vf put` 唤醒；队列损坏/积压/重复入队问题类别整体消失（幂等 key 化解重复）。执行模型：每摄取桶一个 tokio 任务持有有界 mpsc（~64）；CPU 段（extract/chunk/embed）经 spawn_blocking 进信号量限并发（=CPU 核数）的阻塞池；模型惰性加载用 OnceLock；SIGTERM 复用 watch channel——停取新任务、完成当前文件后退出（停机超时 < Docker 默认 10s）。
- **指纹与 reindex**：完成标记记录**管线指纹**（= hash(extractor+chunker+embedder 实例配置)）；配置变更 ⇒ 启动 fail-fast，`vf reindex` 显式执行（按旧指纹 filter 清旧块 → 重写 → 更新标记）；无后台隐式迁移。index 元数据记录 **index 派生键**（= hash(model, dim, embedder 配置)），每批写入前校验，不符即泊车报错。两个哈希各司其职：管线指纹管"块集身份"，派生键管"index 身份"。
- **毒丸防护**：文件大小上限（配置，默认 200MB）、extractor 单文件超时、重试 N 次（默认 3）+ 指数退避 → `FAILED` + `retriesExhausted`（不再自动重试，`statusReason` 与 `vf sources status` 可见——状态命名对齐 KB，附录E.2）；OOM 防护 = 大小上限 + RSS 自监控（§5.9）。
- **GC 判据（防误删，v2.1）**：只对 ingest 管理的 index（元数据有标记）执行，且匹配 **chunk key 派生格式**与 `x-vf-src-uri`——禁止裸前缀过滤（防误删恰好使用 x-vf-* 键的纯协议用户向量）；对账走 ListVectors+segments（非 Query）；GC 限速 + 空闲窗口（复用 optimize 纪律）；孤儿窗口 = 对账周期（默认值文档明示）。**真源 = 源桶对象列表**（源桶零系统对象，无需排除前缀）：对象经任何途径被删 ⇒ 下周期 GC 其 chunk + 标记；`vf rm` = 删对象（经 object_store）+ 立即触发 GC（异步完成，202 DELETING 状态可见——与 S3 直接删除最终等价）。
- **openai provider**：429/超时指数退避；批内任一失败整批重试（幂等 key 保安全）；持续失败 ⇒ 泊车（协议面不受影响）。
- **LOCK 接管竞态（v2.1）**：心跳携带 writer id；接管用条件写 put-if-match（旧 token 为匹配值），不依赖跨机不可实现的"确认旧进程死"。
- **metadata 预算**：`x-vf-src-uri` 恒为 ≤100B **对象键**短哈希（原对象键全文放 non-filterable 的 `x-vf-src-name`）；摄取前预检（超长拒收 fail-loud）；回源键全声明 filterable；预算表进 README。
- **前向兼容**：catalog/index JSON 解析对未知字段宽容 + `schemaVersion`（保 §6 可退出性）。开发阶段不做任何向后兼容（vf.json 读取、旧布局迁移、旧命令别名均无——2026-11-04 裁决）。
- **许可红线**：内置默认仅 permissive（gte-multilingual-base Apache-2.0、bge-m3 MIT、SigLIP2 Apache-2.0、jina-clip-v1 Apache-2.0）；jina-clip-v2（CC-BY-NC）、jina-embeddings-v4（Qwen Research License）、MM-Embed（CC-BY-NC）、Chinese-CLIP（权重无标注）只可用户自配；cargo-deny 拦截。

#### 5.10.6 命令面与 API 面

- 服务端 `vectorfs` 与客户端 `vf` 的**完整命令表、8 个 `/x-vf/<Op>` 超集操作、MCP 工具增补，统一见附录E**（唯一权威清单，避免双处维护）。
- 要点：`reindex` 由 daemon 内 ingest worker 执行（单写者语义），`vf reindex` 仅为触发端；`GET /health` 为 no-auth 探活端点（容器 healthcheck 用）。

#### 5.10.7 Docker 部署

ghcr multi-arch（amd64/arm64 对齐发布矩阵）debian-slim non-root，内含双命令；卷 `/vectorfs/data` + `/vectorfs/models`（模型不进镜像层；air-gapped 自焙衍生镜像，文档给 Dockerfile.example）；`VF_PRESET` env + 入口脚本首启物化 TOML 进数据卷；**env 覆盖收敛为标量项**（端点/凭证/preset/后端），复杂结构改 TOML——边界写明；healthcheck = `GET /health`；SIGTERM 优雅停机（5.10.5）；**每 endpoint 按桶划分（v3）**：单 daemon 管全部桶仍是默认部署形态（单容器）；不相交桶集可多 daemon（per-bucket LOCK，§5.1；S3 后端可无状态重启）；纯协议面多 daemon 需外部按桶分流（惰性锁是执行面不是声明面）；`--backend memory --no-auth` 容器 = testcontainers 秒级 s3vectors 模拟器；`deploy/docker-compose.yml` 进仓库；K8s 文档化不发 chart。发布物基线：tarball 在 ubuntu:20.04 容器构建（glibc 2.31 对齐既有冒烟）；.so（ort/pdfium）pin 版本 + SHA 随 sha256sums；CI 用仓内 ~1MB 小 ONNX fixture 保 hermetic，真模型测试每周档。

#### 5.10.8 明确不做

分支声明 / 条件表达式 / 拓扑引擎（多路 fan-out 只是 embedders 列表里多一个名字；**extractor 有序回退链同理**——2026-11-05 裁决：线性列表非 DAG，内容路由智能住在 kind 内部，机制层零路由引擎）；任意 DAG；独立任务队列；后台隐式重嵌引擎（重嵌 = 显式 `vf reindex`）；模态槽位配置（路由由 embedder 模态声明推导）；**自建 S3 对象面**（PutObject/GetObject 等对象 API 不实现——标准工具直接操作源桶，vectorfs 只读扫描）；**内容寻址源仓库/文件副本**（v2.2：文件即普通对象，不做第二份存储）；插件商店；v1 不做 wasm runtime 与进程外插件。

## 6. 一致性模型（卖点之一）

- **写后立读强一致**：Lance manifest 原子切换 + S3 系对象存储读写一致 ⇒ 上一毫秒 PutVectors、下一毫秒 QueryVectors 必见。对齐 AWS 官方表述 "Writes … are strongly consistent"，且**无任何最终一致窗口**（对比：阿里 Standard 模式写后约 2–3 秒可查——我们的本地实现天然免疫）。
- 单写者 ⇒ 写全序串行化。
- 分页 = manifest 快照隔离（token 携带版本）。
- **数据可退出性承诺**：系统桶 `X-vf` = 开放格式（Lance + JSON），daemon 消失数据仍可被任何 Lance 工具读取——写进 README，是对用户的承诺也是项目的优雅退出路径。

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
门槛（发布即验收，G7）：conformance 全绿 + 发布物就绪（二进制 + README + 5 分钟路径）+ **Show HN 落地帖发出（带可用 v0.1；不做无产物预热帖）**。发布后观察窗 2–4 周（§9 指标）。

### M2 — v0.2 文件层 MVP：text 模态 ✅ 完成（2026-11-05，验收门 PASS——证据见 §10 第 8 条执行实录）

范围（v2.1 收窄——五路审查 P0：组合面过载）：`vectorfs.toml`（不做 vf.json 兼容，直接切换）+ 双命令 bin + **`.vectorfs/` 布局**（§5.3 v2.2）；四轴插件骨架（两段式配置 + enum 分发）+ contract test；**scope 扫描摄取**（周期 list-and-diff + 标记；`vf put` = 落普通对象 + 即时触发）+ ingest worker（pdf-text/md/txt + token-window + ort[gte 默认]/openai）；`vf` CLI（put/search/get/rm/sources/reindex）；merge rrf；Docker 镜像 + compose + 三 preset（dev/text-rag/air-gapped，init 预取模型）。
门槛（发布即验收）逐条证据：**①preset 零配置全链 35 秒**（serve+1.2GB 模型加载 20s + put PDF/中文 txt 各 0.5s 至 INDEXED + 中英双语语义检索正确回源；门槛 ≤5 分钟）；**②boto3 直查**已摄取向量拿到全套 `x-vf-src-*`（真实 gte 768 维派生 index，`$eq x-vf-src-uri` 过滤可用）；**③双 SDK conformance 不回退**（boto3+Go 各 32/32，对 release-prod 二进制）；**④GC 路径测试**（防误删两测 + fs 直删下周期收敛 + vf rm 319ms 收敛，进常规测试套件）；**⑤healthcheck**（容器 40s healthy/0 failing）；**⑥体积 159 MiB——超 155 警戒线 4 MiB、硬顶 175 未破**（openai embedder 的 TLS 路径成为 serve 真实引用后不再被 thin-LTO 裁掉，+3.1MiB 功能固有成本；M1 已有"踩警戒线仍接受"先例 §0/§5.7，此处如实记录，自建者可 `CARGO_PROFILE_RELEASE_PROD_LTO=fat` 压回）。
（图像模态 pdf-pages+siglip2、docx/html、markdown chunker、OCR、reranker、$textMatch、IVF_PQ → M3）

### M3 — v0.3 图像模态 / 质量 / 规模 / 超集（图像模态链已闭环：T1-T5 于 2026-11-06 过验收门，证据见 §0「M3 图像模态链交付」行）

范围：image 模态（pdf-pages + siglip2 + multimodal/image-search preset，以图搜图查询输入）；S3 事件通知加速（可选，扫描模型之上的低延迟增强）；pdf-ocr + scanned-docs preset（OCR 引擎选型补 §5.10.2）；docx/html extractor + markdown chunker；**epub 容器 kind**（unpack OPF spine 定序→XHTML 子文档走 html 链——机制已就绪，2026-11-05 裁决）**+ normalize 表增补**（charset→UTF-8、eml/mime-multipart 拆分、通用 zip 展开）；Reranker；$textMatch 走 **Lance 原生 FTS**（tantivy 侧车退役，ADR 0002——前置：chunk 文本升派生表顶层 string 列、`/x-vf/Search` 增 `searchMode: semantic|keyword|hybrid` 超集参数、内核薄 trait 隔离保留换核退路；协议面 QueryVectors 保持纯语义）；merge weighted；IVF_PQ；统一多模态 embedder 评估（Qwen3-VL 作 GPU/远端 provider）；后端矩阵扩展（Ceph/Garage）；bench 报告；策略最小实施（可选）。

### M4 — v0.4 音视频模态（门槛：M3 过门）

范围：
- `audio` extractor：symphonia 解码（纯 Rust）；分段 = 固定窗/VAD（Nova 惯例 30s 段）；**可选 ASR 转写（whisper，ort/GGUF）→ text Parts 进现有文本管线**——音视频内容检索的主路线（"用文字查视频说了什么"），复用文本管线（默认 gte，可换 bge-m3）与回源机制，质量与语言覆盖最成熟。
- `video` extractor：ffmpeg（.so load-dynamic）解复用 → 场景切换/固定间隔关键帧（image Parts）+ 音轨（audio Parts，转 audio 管线含 ASR）；chunk key 序号 = 音频段号/视频帧号（派生规则不变）。
- `clap` embedder kind：声学相似（以音搜音/音乐/环境音），Apache-2.0、512 维，协议契合同 SigLIP 双塔（doc=audio，query=text/audio）。
- 远端视频 embedding（Nova Multimodal / TwelveLabs Marengo）作 openai 形态 provider 扩展；Qwen3-VL-Embedding 视频原生输入作远期统一选项。
- preset：`media-search`。
依据：`research/2026-11-multimodal-embedding.md`（CLAP license/维度核实；Nova 30s 分段 + MRL；TwelveLabs 按模态计费）。
门槛：样例音视频摄取 → `vf search`（文本查内容经 ASR、以音搜音经 CLAP）回源可用；conformance 不回退；ffmpeg .so 走既有 pin+SHA 分发纪律。

### M5+ — 门槛制扩张（每项须外部信号 + 用户裁决，不因"建得起"而建）

- BYOC 同步（本地↔云 S3 Vectors 复制）——信号：用户要求迁移路径。
- wasm 插件槽实装（`kind="wasm"`）——信号：第三方要写提取器/embedder 的实际需求。
- **Web UI**（只读管理台：bucket/Source/摄取状态浏览 + 检索 playground；chat UI 不做）——信号：用户证据 + 第二维护者（UI 是长期维护负担，单人不开）。
- **多写架构**（§5.1 演进：多 writer 提交仲裁/锁协议升级；在此之前单写者是简化宪法——ingest 无竞态、GC 无协调皆依赖它）——信号：规模化部署的真实瓶颈（多机写入需求、单点吞吐投诉）。
- **插件商店**（外部生态分发；前置 = wasm 槽已实装且有第三方插件实际供给）——信号：第二维护者出现（G5）。

## 8. 风险登记册

| # | 风险 | 概率 | 冲击 | 缓解 | 触发器（→ 动作） |
|---|---|---|---|---|---|
| 1 | **LanceDB 公司出官方 s3vectors 网关**（天然主人，两家竞品都用 Lance，路径现成） | 中 | 极高 | 无法防；差异化转"后端无关+模拟器+超集"或转贡献者 | 其 blog/release 官宣 → 一周内重定位裁决 |
| 2 | SeaweedFS 天级迭代，补齐单进程简单模式/超集/embedding 管线 | 中高 | 高 | 我们=后端无关（可跑 MinIO/R2/S3）+文件层差异化首发 | 其发布 simple mode 且补管线 → 触发差异化重评（上层是现差异位） |
| 3 | 协议漂移（季度级：2026-06 分页、2026-09 prefilter） | 高 | 中 | CI 每月 diff botocore 模型，21 op 兼容矩阵文档化 | 破坏性变更 → 7 天适配窗口承诺 |
| 4 | 条件写在部分 S3 后端缺失 → Lance 提交退化/慢 | **低**（M0：全部目标后端支持 If-None-Match:*） | 中 | LOCK 对象单写者串行化降级（防御性保留，§5.2） | 新后端接入时先跑条件写测试 |
| 11 | aws-lc-rs（object_store aws 硬绑）：C 供应链 + cmake 构建依赖进 CI | 低 | 低 | 锁版本；CI 预装 cmake；关注上游 ring feature 恢复后切换 | 构建链断供 → 评估 ring 路线 |
| 5 | 远端 S3 延迟伤叙事 | 中 | 中 | file/memory 保底 dev/CI；manifest 键控缓存 | spike 延迟超标 → 叙事降级为"同机房 MinIO 位" |
| 6 | 单人断代（生存悖论） | 高 | 高 | K0 分步门槛 + 可退出性格局（Lance 开放格式）+ M1 起每季评估寻找共维者 | 6 个月无第二贡献者 → 维护模式 + README 寻接管 |
| 7 | merge-insert × schema 演化边角（并发批新键、类型漂移） | 中 | 中 | 写者串行天然规避并发演化；类型冲突预检 | spike Q10 失败 → metadata 全 blob + 建列惰性化 |
| 8 | lancedb 0.41 API 漂移 / 双 arrow | 中 | 低 | 锁 minor；`cargo tree -d` 进 CI；API 只经内部一个 crate | 依赖冲突 → 隔离 crate 收敛 |
| 9 | SigV4/pagination 细节与真实 SDK 行为出入 | 中 | 中 | M0 抓包三 SDK；conformance 套件即回归防线 | 新 SDK 大版 → 套件补跑 |
| 10 | "vectorfs" 名称语义税（服务而非 fs） | 确定 | 低 | README 第一句定位纠偏；发布物副标题承担 | — |
| 12 | 上层范围蔓延（解析/嵌入/对账各自皆无底洞） | 高 | 高 | 四轴封闭 + 机制固定 + §10 三问 + 每步验收门；**M2 已收窄为 text 单模态 MVP（v2.1）** | 新模态/新 kind 无用户证据 → PR 模板拒绝 |
| 13 | 模型许可红线（jina-clip-v2 CC-BY-NC、jina-v4 Qwen Research、MM-Embed NC、Chinese-CLIP 无标注） | 中 | 高 | 内置默认仅 permissive（gte Apache-2.0/bge-m3 MIT/SigLIP2 Apache）；cargo-deny 拦截；许可清单文档化（§5.10.5） | 新内置模型引入前 license 审查步骤 |
| 14 | CPU 嵌入延迟拖垮体验 | 中 | 中 | SigLIP2 <1s/图、bge-m3 批异步；MLLM 类只作远端/GPU provider；队列泊车不伤协议面 | bench 超标 → preset 降级或默认远端 |
| 15 | Docker 单写者被误解（同桶集起多容器；v3 起锁粒度=桶，不相交桶集多 daemon 合法） | 中 | 中 | per-bucket LOCK 对象 TTL 心跳（§5.1）+ 启动锁冲突报错清晰 + compose 注释与 FAQ | 用户报告锁问题 → 文档补 |
| 16 | AWS 官方补 chunking/hybrid 挤压上层 | 中 | 中 | 官方天然绑 Bedrock；我们=本地/任意 S3/离线/可插拔 | embed-cli 与 S3 Vectors changelog 月度哨 |

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
- awslabs/s3vectors-embed-cli 是否加 chunking、S3 Vectors 是否加 hybrid/全文（风险 #16 上层挤压哨）
- Qwen3-VL-Embedding / 开源多模态嵌入生态动向（统一 embedder 路线成熟度，M3 依据）
- 自仓 stars/issues/下载量；落地帖渠道的累积反馈

## 10. K0 治理记录（本日裁决）

- 模式：**solo，不范围冻结，分步执行、每步验证**；coding agent 为主要实现力。
- 纪律对冲（documentfs 元教训——"因为建得起所以建了"）：
  1. 每个里程碑以 §7 验收门为硬边界，过门才进下一步；门内包含用户人工审查。
  2. 新组件三问（进 PR 模板）：差异化必需还是"AI 让它便宜"？无用户证据是否新增持久状态？文档是否超代码量？
  3. 负面清单（§2）为宪法，改动须显式裁决记录于本文档 §0。
  4. conformance 套件先于功能实现（TDD 于协议面：测试=官方 SDK）。
  5. 2026-11-03 M0 执行实录：子代理模式——主会话分派 5 个任务（环境/脚手架/实现/SDK 验收/S3+性能）、逐任务亲自复跑验收（不采信代理自述：测试、双 SDK roundtrip、S3 后端、体积、MinIO 归档均独立复现）、逐任务提交（b5ea181→…）。此为 K0"借助 coding agent 推进"的标准作业形态。
  6. 2026-11-04 重定位会话：两份调研落 `research/`（重定位可行性、多模态嵌入，均一手来源）；设计三轮迭代——策略清单（太平庸）→ pipeline-DAG（否决：为三种实际形态造通用引擎，~9 概念超标）→ 机制/策略分离 + 五轴插件 + 4 新概念（定稿 §5.10；后续口径演进：v2.1 降为四轴+merge 查询选项，v2.2 概念口径 = 2 实体 + 4 轴 + 1 配置节 + Source 标记附属状态）。v2.0 定稿后经**五路子代理独立审查**（简洁性 7/10、健壮性 7/10、协议演化 6.5/10、实用性 6/10、工程 7/10），P0 全部采纳并修订为 v2.1（§0 增补行）。审查的关键收敛发现：配置漂移导致 chunk key 语义破裂（两位审查员独立命中）、换模型隔离仅靠 dim 不可变不成立、模态槽表达力不足、M2 组合面过载。
  7. **优先级纪律（v2.1 增）**：botocore 月度 diff 触发的协议适配（风险 #3 的 7 天窗口承诺）**优先于一切文件层工作**——协议面 conformance 是项目立身资产，上层是增值层，不可反噬。
  8. **2026-11-05 M2 执行实录（§10.8）**：主会话-子代理标准作业（同 #5 形态），T0–T10 十一个任务逐个「任务书→子代理→**主会话亲自复跑验收**→提交」；每任务一 commit（0fb3a46 之后 12 个）。执行序调整为 **T4→T5→T3**（任务书允许无依赖重排：T3 的 kill -9/全链验收需要真实 extractor+embedder 就位，tiny-embedder 仓内 fixture 使全链 hermetic）。主会话复跑共抓出并自修三个子代理未报的问题：①ort config `model` 字段默认 gte 致 §5.10.4 air-gapped 形态（path-only）必然拒启——model 改 Option 四象限解析；②HF blob URL 漏 `resolve/<ref>` 段（hermetic 假 server 同形路径未暴露）；③T8 提交误纳 target-docker/ staging 目录（amend 修正 + gitignore）。裁决一处设计缺口：「配置变更 ⇒ 启动 fail-fast」与「vf reindex 需运行中 daemon」死锁——增 `vectorfs serve --reindex` 启动旗标（跳指纹门、启动即重嵌、不加新命令，附录E.4 未列 flag 不违）。新增超设计计划但必要的一项：**HTTP 叶子标准代理支持**（HTTPS_PROXY/HTTP_PROXY/NO_PROXY，hyper-util Tunnel CONNECT）——企业网络 HF 下载/openai 出网是真实部署形态，T9 总门环境（仅代理出网）实测暴露。体积终值 159 MiB（超 155 警戒 4 MiB 的成因与处置见 §7 M2 门槛⑥）。遗留如实记录：`/x-vf/Reindex` 与同源 in-flight worker 间极窄竞态（write_lock+in-flight 跳过已收窄，作业级串行留后续）；PutSource 全程内存缓冲（≤210MiB，流式验签留后续）；onnxruntime 无 macOS-x64 官方资产（fetch 脚本 fail-loud 指引自建）。

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
- Fusion 语义蓝本（M3 超集裁剪源）：显式 schema（字段 ≤100、向量 ≤3 路）；`knn[]` 多路（各带 filter/topK≤500/numCandidates≤1000/boost）；`retriever` 复合（rrf{k,windowSize,weight} / weight{normalizer}，复合不可嵌套）；`$textMatch`(operator/minShouldMatch/boost)、`$textMatchPhrase`；`sort[]` ≤3（含 _score）；`limit` ≤500；纯标量查询才可翻页；`ip` 距离。阿里 Standard 限额参照：TopK ≤500、QPS 100、写后 2–3 秒可查（我们强一致即优势点）。

## 附录D：竞争格局快照（2026-11-03，验证③产出）

| 项目 | 状态（2026-11-03） | 形态/引擎 | 对我们的含义 |
|---|---|---|---|
| SeaweedFS | **4.52 已发布**（2026-11-02）：S3 Vectors 网关、boto3 conformance harness、两周 60+ PR、35.9k★ | 分布式 FS（master/volume/filer + vector server），Lance | 主要威胁与**潜在宿主**；盯 simple-mode 与超集动作 |
| Ceph RGW | s3vector 合并主干（2026-09-17，+29.7k 行），未进稳定版 | 整套 Ceph，内嵌 lancedb-c；官方明示不做混合检索/低延迟 | 宿主+验证品类；其"不做 hybrid"留给我们超集位 |
| Apache Ozone | roadmap HDDS-14967（~2.2.0） | Hadoop 系 | 观察项 |
| LocalStack | 无 s3vectors；Community 版 2026-03 转 inactive | 模拟器 | dev/CI 位空缺=我们 memory:// 后端的定位空间 |
| MinIO / Qdrant / Cloudflare / TurboPuffer | 零相关动向 | — | MinIO=宿主；无新竞争者 |
| LanceDB 公司 | 无兼容产品动向 | 引擎 | **最高哨戒风险**（风险 #1）：两大竞品都用 Lance，官方网关路径现成 |
| 应用侧 | Open WebUI 已内置 s3vector backend（社区维护，boto3 端点可覆写）；LlamaIndex 3 天/LangChain 9 天出集成；Bedrock KB GA 绑定 | — | 需求侧强兑现 |
| 社区情绪 | HN 高热帖抱怨 post-filter/topK/无 hybrid；明确 self-host 呼声（"cloud is someone else's computer"） | — | 超集+自托管叙事的原始证据 |

（快照来源与链接见 2026-11-03 会话调研记录；下次复查按 §9 月度节奏。）

**2026-11-03 增补**：MinIO 社区版归档（repo 2026-02 archived，末版 2025-10，下载通道下线；AGPL 线由 Silo 分支续命）——"简单自托管对象存储"位现真空，流向 SeaweedFS/Garage/Silo。对 vectorfs 双重含义：①宿主格局变动（README/文档的宿主列表口径随之调整）；②"向量界 MinIO"类比的心智位反而更空——名字的出处退场，位子还在。另：SeaweedFS 4.52 的 S3 面已实测作为 vectorfs 宿主（M0，9/9）——"竞争者即宿主"判断获实证。

## 附录E：CLI 命令面与 x-vf 超集 API（文件层，M2 起；唯一权威清单）

### E.1 Wire 约定

- 命名空间：`POST /x-vf/<OperationName>`——独立前缀，与 AWS 面 21 op（附录A）**零碰撞**（AWS 未来加同名 op 也不冲突）。
- 签名/错误/分页与 AWS 面完全同构：SigV4（signingName=s3vectors）、rest-json、`X-Amzn-Errortype` + body `__type` 双发、nextToken 分页。vf CLI 复用同一自研签名栈。
- **异步操作 = HTTP 202 + 状态轮询**（对齐 KB `IngestKnowledgeBaseDocuments` 官方契约）：PutSource / DeleteSource / Reindex 均 202 入队即返回，结果经 GetSource / GetIngestStatus 可见。
- 大文件：PutSource 用原始字节 body（`Content-Type: application/octet-stream`，元信息走 `x-vf-file-name`/`x-vf-content-type` 头）；>100MB 走分段上传（multipart，后端矩阵已验证）。
- 命名映射约定：API 字段 camelCase（topK/maxResults），CLI flag kebab-case（--top-k/--max-results），一一对应。

### E.2 超集操作（8 个）

| 操作 | Method+URI | 关键参数 → 响应 | 说明 |
|---|---|---|---|
| PutSource | POST /x-vf/PutSource | bucket, key(头)；body = 原始字节 → **202** `{key, status:"PENDING"}` | **便捷包装**（v2.2）：对象落源桶普通键 + 即时触发摄取（源桶不存在则惰性建桶，v3/D4）；S3 API 直传的对象由周期扫描摄取（§5.10.3），不经此 op；对齐 KB `IngestKnowledgeBaseDocuments` 契约 |
| GetSource | POST /x-vf/GetSource | bucket, key → `{status, statusReason?, retriesExhausted?, mime, etag, pipelineFingerprint, chunks[{index, count}]}` | 状态机对齐 KB 文档状态命名：`PENDING / IN_PROGRESS / PARTIALLY_INDEXED / INDEXED / FAILED / DELETING`；失败原因逐文件可见（`statusReason`，KB 同名字段） |
| GetSourceData | POST /x-vf/GetSourceData | bucket, key → 原始字节流（透传读源桶对象） | `vf get` 底层（回源下载；s3 后端也可直接 GetObject） |
| ListSources | POST /x-vf/ListSources | bucket, status?, maxResults(1–1000, 默认 500), nextToken → sources[] | 标记 × 实时列表 join；`vf sources list` 底层 |
| DeleteSource | POST /x-vf/DeleteSource | bucket, key → **202** `{status:"DELETING"}` | = 删对象（经 object_store）+ 异步 GC chunk——与 S3 直接删除等价的便捷包装；对齐 KB `DeleteKnowledgeBaseDocuments` 的 202+轮询 |
| **Search** | POST /x-vf/Search | bucket, query{text\|image\|audio(M4)}, topK(1–1000), filter?（**$ 短名语法**，§4.6 同款）, returnText?, searchMode?(semantic\|keyword\|hybrid, M3-T9) → hits[] | **旗舰超集 op**：fan-out + merge + 回源；hits 形状对齐 KB Retrieve 的 KnowledgeBaseRetrievalResult（下）。`query.image`（M3-T4 定形）：`{bytes: base64(STANDARD), mime}`，服务端按 magic 嗅探与声明 mime 交叉校验（同 `ImagePayload::validate` 纪律）；query 与 image 模态互斥、audio 属 M4。`searchMode`（M3-T9, ADR 0002）：默认 semantic（缺省=旧行为，老客户端零变化）；keyword=纯 BM25（Lance 原生 FTS，内核走薄 trait 隔离）；hybrid=语义 fan-out+BM25 两路共入 bucket 的 merge（rrf/weighted）；keyword/hybrid 需 TEXT query + `[ingest.X] fts = true`（§7 默认关）+ `vectorfs optimize` 建索引（列缺失/未建时前 flat 兜底、无列响亮拒）；协议面 QueryVectors 保持纯语义 |
| Reindex | POST /x-vf/Reindex | bucket → **202** `{jobId}` | 显式重嵌（指纹变更后），进度经 GetIngestStatus；KB 无对应 op（其 chunking 不可变，换管线=重建 KB） |
| GetIngestStatus | POST /x-vf/GetIngestStatus | bucket? → 状态计数 + **统计块（字段名对齐 KB IngestionJobStatistics）**：numberOfDocumentsScanned / numberOfNewDocumentsIndexed / numberOfModifiedDocumentsIndexed / numberOfDocumentsDeleted / numberOfDocumentsFailed + pendingCount；最近对账、active 管线指纹、reindex{jobId, status, progress} | `vf status` 底层 |

**Search hit 形状**（对齐 KnowledgeBaseRetrievalResult，保留双 index fan-out 所需字段；`content.type` 为 KB 展示枚举，与内部模态映射：page-image/image→IMAGE、video→拆解为帧/音轨后各自映射）：

```
hits[] = { content{text, type: TEXT|IMAGE|AUDIO|VIDEO},   // KB content.type 同款枚举（去 ROW）
           score,                                            // 合并分（rrf/weighted）
           distance?,                                        // 单路查询时有意义
           index, key,                                       // vectorfs 特有：派生 index 名 + chunk key
           location{type:"source", key, page?},              // KB location 同位（回源 = 源桶对象键）
           metadata }
```

AWS 面 21 op 不受影响——文件层经它们写入（公理 2 §5.10.0），纯协议客户端零感知。

### E.3 MCP 工具增补（M2）

既有 6 数据面工具（list_vector_buckets / list_indexes / query_vectors / get_vectors / put_vectors / delete_vectors）之上增：`search`（同 Search）、`put_source`（本地路径读入+上传）、`list_sources`。`--tools ro` 下仅 search/list 只读可用。

### E.4 CLI 命令表

**服务端 `vectorfs`**（daemon 本地执行，`--data-dir` 即 daemon 本地家；公共 flag：`--data-dir`）：

| 命令 | 作用 |
|---|---|
| `init [--preset dev\|text-rag\|air-gapped] [--listen] [--backend …]` | 物化 vectorfs.toml + **预取模型**（进度/sha256/镜像）+ 生成凭证，打印客户端 env 片段 |
| `serve [--listen] [--no-auth]` | daemon：21+x-vf HTTP、MCP 入口、ingest worker、对账器 |
| `status` | 本地概览：锁持有、后端连通、bucket/index/Source 计数 |
| `doctor` | 探针：后端条件写、multipart、模型就绪、磁盘余量 |
| `optimize [--bucket] [--index]` | IVF_PQ 建索引/折入增量（M3-T8 交付；训练门 256 行响亮拒绝）；Lance 碎片压缩 = 证据门待加 |
| `pipelines check [--config <f>]` | 配置校验（解析/模态匹配/指纹）——CI 可跑，exit code 语义 |
| `pipelines list` | 已物化 pipeline、派生 index 与指纹 |
| `mcp [--tools ro\|rw]` | stdio MCP server（单写者模式，同锁） |

**客户端 `vf`**（HTTP 远程；端点/凭证：`--endpoint` 或 `VF_ENDPOINT` + `VF_*` env）。**S3-CLI 形态（2026-11-05 裁决）**：`bucket/key` 路径位置参数与 `--bucket` flag 并行——flag 在场时位置参数一律是完整 key（含 `/` 的 key 以 flag 形态寻址，向后兼容优先）；路径形态在无 flag 时生效，首个 `/` 前为桶：

| 命令 | 作用 |
|---|---|
| `put <file…> <bucket>/[<prefix>/]` 或 `--bucket <b> [--key <k>]` `[--async]` | 落源桶普通对象 + 即时触发摄取；默认前台 + stderr 进度；尾随 `/` 的末位参数是 aws-cp 式目标位（`docs/pre/` ⇒ key `pre/<文件名>`） |
| `search "<text>" [<b>] 或 --bucket <b> [--top-k N] [--filter JSON] [--json]`（M3+ 增 `--image <f>`，M4 增 `--audio <f>`） | 唯一检索动词：fan-out + merge + 回源 |
| `ls [<bucket>[/prefix>]] [--status S]` | S3-CLI 列表形态：无参=列摄取桶（`aws s3 ls` 形）、带桶=列已摄取文件（prefix 客户端过滤）；与 `sources list` 同渲染 |
| `sources list --bucket <b> [--status S]` | 列已摄取文件（标记 × 列表 join，分页）——动词形态，`ls` 为其简写 |
| `sources status <key> --bucket <b>` | 单文件状态（statusReason / 失败详情可见） |
| `get <bucket>/<key>` 或 `<key> --bucket <b>` `[--out <f>]` | 下载原文件（回源；等价于直接 GetObject） |
| `cat <bucket>/<key>` | get 的 stdout 直出形态（S3-CLI 惯用） |
| `rm <bucket>/<key>` 或 `<key> --bucket <b>` | 删对象 + GC（与 S3 直接删除等价的便捷包装） |
| `reindex [<b>] 或 --bucket <b>` | 触发显式重嵌（daemon 内执行） |
| `status` | `/health` + GetIngestStatus 汇总 |

通用契约（§5.8 agent 一等契约）：`--json` 带 schema_version、结构化错误 `{code, exit, message, hint}`、stdout=结果 / stderr=进度、零交互、幂等、确定性输出。

### E.5 与 Bedrock KB API 的对照（借鉴边界，2026-11-04）

完整调研：`research/2026-11-bedrock-kb-api.md`（官方文档逐字核对）。原则：**采纳其契约形状与状态语义，偏离处一律因"锚定自家协议面"**。

| Bedrock KB | vectorfs | 取舍 |
|---|---|---|
| KnowledgeBase 实体 | bucket + `[ingest.<bucket>]` | 不引入独立 KB 实体——bucket 即边界 |
| DataSource（S3 周期扫描拉取） | 整源桶摄取模型（v3 换轴）：源桶即数据源，文件=普通对象，扫描+标记 | **采纳且更彻底**：KB 的 DataSource 配置消失——**源桶本身**就是数据源（.git 心智） |
| IngestKnowledgeBaseDocuments（202 + documentDetails 状态） | PutSource（202 + 状态轮询） | **采纳**：入队即返回、逐文档状态 |
| IngestionJob 统计（8 个 numberOf\* 计数器） | GetIngestStatus 统计块 | **采纳适用子集（5 个 numberOf\*）** |
| merge 选项 | rrf / weighted / first | 超集（M2 rrf，M3 weighted） |
| Retrieve / KnowledgeBaseRetrievalResult | Search hits | **采纳形状**（content/location/metadata/score）；增 index/key/distance（双 index fan-out 所需） |
| numberOfResults(1–100, 默认 5) | topK(1–1000) | **偏离**：与协议面 QueryVectors 的 topK 命名一致优先 |
| overrideSearchType: HYBRID/SEMANTIC | merge 查询选项（rrf/weighted/first） | KB 在 S3 Vectors 后端仅 SEMANTIC；merge 粒度更细 |
| retrievalFilter（13 个长名算子 union 结构） | $ 短名算子（附录A 同款） | **偏离**：官方本就两套语法，锚定 S3 Vectors 面；KB 用户按 KB 文档的 FilterAttribute↔$ 算子换算 |
| chunkingConfiguration（创建后不可变，无 reindex op） | chunker 轴可变 + 显式 Reindex | 我们更灵活（KB 换管线=重建 KB） |
| dataDeletionPolicy: RETAIN/DELETE | 恒 DELETE | 单机自管无 RETAIN 场景 |
| DeleteKnowledgeBaseDocuments（202+轮询） | DeleteSource（202 + DELETING 轮询） | **采纳** |
| 文档状态 12 值枚举 + statusReason | 6 值子集 + statusReason + retriesExhausted | **采纳命名**，取适用子集（PARTIALLY_INDEXED 对应多模态分支部分失败） |
| RetrieveAndGenerate | **不做** | 无 LLM 面（负面清单 #4 同族） |
