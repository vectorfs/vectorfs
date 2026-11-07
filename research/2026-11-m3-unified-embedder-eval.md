# 专项调研：M3 §7 收官项——统一多模态 embedder 评估（Qwen3-VL 作 GPU/远端 provider）

- 日期：2026-11-07。基准 = HEAD `89ffb22`（M3 T1-T11 全部落地，双塔 + rerank 已交付）。
- 背景：design.md §7 M3 范围最后一项「统一多模态 embedder 评估（Qwen3-VL 作 GPU/远端 provider）」；§5.10.2 Embedder 行预告「qwen3vl 统一评估（M3）」。当前多模态检索形态 = **双塔分立**（`[embedder.text]` gte-multilingual-base + `[embedder.image]` siglip2-base，查询侧文字 query 双路 fan-out→RRF merge，图像 query 单路以图搜图）。本文件回答：**统一 embedder（单模型图文同空间）对本项目是否值得引入、以什么形态引入**。备用户裁决的决策文档，零代码改动。
- 调研纪律：与 [2026-11-multimodal-embedding.md](2026-11-multimodal-embedding.md) 同——所有论断回到一手来源（HF API license 元数据 / model card 原文 / GitHub 仓库与官方文档 / 本仓代码）；license 以 HF tag / card 原文为准；标注 [来源](url)；标注"估算"的为推算值。引用既有研究档案结论时注明章节**不重查**：multimodal 档案记作「多模态档案 §X」，[2026-11-m3-bench.md](2026-11-m3-bench.md) 记作「M3 bench §X」。
- 项目硬前提（判据，design.md / crates 逐一核实）：
  1. **无 GPU 假设**：服务端推理全走 ort CPU EP（§5.7；runtime-libs pin ORT 1.31.0，无 CUDA EP .so 管理链）；M3 bench 全部数字为 20 逻辑核 CPU 实测。
  2. **出网白名单**：仅 `openai` embedder 显式配置才出网（§5.7 出网行）；air-gapped preset 是一等交付面。
  3. **许可红线**：内置默认仅 permissive；CC-BY-NC/研究许可/无标注只能用户自配（§5.10.5 + models.rs 红线表）。
  4. **闭集 kind 纪律**：embedder kind 是封闭 enum 穷举分发（embed/mod.rs），新增 kind 触碰注册表四视图 + 模态矩阵 + contract/parity 测试。
  5. **机制层已模态无关**：v2.1 裁决的「embedder 模态自声明集合路由」+ 查询 fan-out（§5.10.3）意味着**机制层对"doc 侧声明 [Text, Image] 的 embedder"零改动可用**——统一路线的落点只在 kind 层。

---

## 0. 结论速览

| 判题 | 结论 |
|---|---|
| 统一 embedder 质量是否够格 | **是**：Qwen3-VL-Embedding-2B MMEB-V2 73.2 / 8B 77.8（开源第一梯队，card 实证）；文本侧 MMTEB 63.87 不劣于 bge-m3（59.56） |
| CPU 自托管可行吗 | **不可行**（量化结论）：2B MLLM 单图 prefill 3–6 TFLOPs ⇒ CPU 每图十秒到分钟级（多模态档案 §D2 估算，本轮无新反证）；无官方 ONNX，唯一社区导出为 0 下载 genai 组件拆分格式，**走不了我们 ort 路径** |
| 形态 b「零产品代码」成立吗 | **不成立（代码核查更正）**：既有 `openai` kind 双侧均 [Text]、wire 只发 `{"input":[texts]}`——图像根本到不了端点；"零代码远端"只覆盖纯文本嵌入，统一收益为零 |
| M3 写代码吗 | **不写**：双塔+rerank 已是交付形态且交互级可用（M3 bench §0）；统一路线的证据窗口在 M4（视频模态原生支持） |
| 推荐形态 | **a + b 文档化**：M3 收官零代码；把「远端统一 embedder」文档化为既定形态（vLLM/IONOS 配方），把 **b1（openai kind 的 messages 多模态输入扩展，估 200–400 行）列为 M4 候选工件**；**c（内置 qwen3vl kind）否决**（三重硬冲突） |

---

## 1. 候选现状核查（2026-11 时点，全部一手）

### 1.1 事实表

体积为 HF API safetensors 实测总量；"统一空间"= 单模型文本/图像/截图/视频同空间；ONNX 列回答"能否走我们 ort（单 session + 自管预处理）"。

| 候选 | 许可（HF tag/card 原文核实） | 参数量 / 权重 | 单模型图文同空间 | 维度 / MRL | ONNX（走我们 ort） | GPU 推理栈 | CPU 可行性 |
|---|---|---|---|---|---|---|---|
| **Qwen3-VL-Embedding-2B** | **Apache-2.0**（[API](https://huggingface.co/api/models/Qwen/Qwen3-VL-Embedding-2B)） | 2B（28 层）/ safetensors **2.13GB** | 是：text/image/screenshot/video/混合输入统一（card Highlights） | 2048；MRL **64–2048** + 输出量化（int8/binary） | **无官方导出**；onnx-community 仅有 Instruct 系（非 Embedding）；唯一社区导出 [PIA-SPACE-LAB/...-2B-onnx](https://huggingface.co/api/models/PIA-SPACE-LAB/Qwen3-VL-Embedding-2B-onnx) 为 Transformer+Vision 组件拆分（genai 形态）、**0 下载**（2026-04 建） | **vLLM ≥0.14**（官方 GitHub，`--runner pooling`）/ SGLang；TEI 无（开放 feature request） | 不可行（§1.3 量化） |
| **Qwen3-VL-Embedding-8B** | **Apache-2.0**（[API](https://huggingface.co/api/models/Qwen/Qwen3-VL-Embedding-8B)） | 8B（36 层）/ safetensors **8.14GB** | 同上 | 4096；MRL **64–4096** | 同上（社区 FP8/GGUF/MLX 若干，无可用 ONNX） | 同上 | 不可行 |
| **MrLight/dse-qwen2-2b-mrl-v1**（DSE） | **Apache-2.0**（[API](https://huggingface.co/api/models/MrLight/dse-qwen2-2b-mrl-v1)；本轮复核，author 名下 DSE 线仅此一档，无 qwen2.5-vl 后继） | 2.2B / ~4.4GB（多模态档案 §B1 引 card） | 部分：**doc 侧=文档截图专精**；text query 也需附最小占位图（vLLM 文档明说 "requires a placeholder image of the minimum image size for text query embeddings"，[来源](https://docs.vllm.ai/en/latest/models/pooling_models/embed)）——查询侧协议别扭 | 1536；MRL →512（档案 §B1） | 无官方（档案 §B1） | vLLM（`--runner pooling` + dse 专用 jinja，vLLM 文档实证） | 不可行 |
| **jina-embeddings-v4** | **红线成立但名义更正**：HF tag 已无 license；card License 节原文 "initially released under cc-by-nc-4.0 due to an error. The correct license is the **Qwen Research License**"（[card](https://huggingface.co/jina-embeddings-v4)，本轮逐字核实，与档案 §A2 一致） | 3.75B（safetensors 实测 3,754,885,248） | 是（visual-document-retrieval 定位：密集文档强，多模态档案 §B1） | 2048 单向量（+128 多向量）；MRL 128–2048 | 无（官方仅 GGUF repo） | vLLM（card 自标 vllm tag） | 不可行；且**不可内置**（研究许可） |
| **nvidia/MM-Embed** | **CC-BY-NC-4.0**（[API](https://huggingface.co/api/models/nvidia/MM-Embed)，本轮复核 tag 仍在） | 8.18B（safetensors 实测 8,175,507,456） | 是（instruction tuning，M-BEIR 52.7，档案 §A2） | 4096 | 无 | 自行 transformers | 不可行；**不可内置** |
| （参照）**gme-Qwen2-VL-2B** | **Apache-2.0**（[API](https://huggingface.co/api/models/Alibaba-NLP/gme-Qwen2-VL-2B-Instruct)，本轮复核；中文强，档案 §B3） | 2.2B | 是 | 1536；无 MRL（档案 §B1） | 无 | vLLM 社区诉求中（档案 §B1） | 不可行 |

读法：**统一路线的可用候选实际只有 Qwen3-VL-Embedding 一家**——它是唯一同时满足「Apache-2.0 + 官方 vLLM 一等支持 + MRL + instruction 感知 + 图文截图视频全模态」的模型；DSE 是文档截图专精的窄形态（查询侧占位图别扭）；jina-v4/MM-Embed 许可红线（后者还 8B 重）；GME 被 Qwen card 同表复评全面反超（MMEB-V2 55.3 vs 73.2，见 §2.1）。

### 1.2 Qwen3-VL-Embedding 关键规格（card + GitHub 一手）

- **官方仅 2B/8B 两档**（[Qwen org 搜索](https://huggingface.co/api/models?author=Qwen&search=Qwen3-VL-Embedding)实证；底座 Qwen3-VL-2B/8B-Instruct，8B card `base_model` tag 核实）。32K 上下文、30+ 语言、**指令建议用英文写**（card Note：instruction 带来 1–5% 提升）。
- **架构 = 双塔式独立编码**：取 `[EOS]` token 最后层 hidden state、L2 归一化（GitHub Architecture 节 + `scripts/qwen3_vl_embedding.py` 实读：instruction 走 system prompt、末 token pooling、F.normalize）。**协议契合**：每输入恰一个 float 向量、cosine——与 S3 Vectors 每 key 单向量完全同构（多模态档案 §A2 同判）。
- **MRL**：card 声明 64–2048（2B）/64–4096（8B）；官方脚本只输出全维，截断是后处理（vLLM 以请求参数 `dimensions` 承接，vLLM 文档核实）。**输出量化**（int8/binary）为 card 声明的输出后处理支持项。
- **GGUF/CPU 侧生态**：本轮 [HF 搜索](https://huggingface.co/api/models?search=Qwen3-VL-Embedding&limit=100)实证 **32 个社区 GGUF 量化** + MLX/FP8 若干 + llama.cpp 支持讨论（[ggml-org#19516](https://github.com/ggml-org/llama.cpp/discussions/19516)）——比档案 §B1 时点（28 个）又增；但 GGUF/mmproj 路径需自证嵌入一致性（档案 §D2），非 ort 路径。

### 1.3 推理栈与 CPU 量化结论

- **ONNX/ort 路径 = 无**。判据：(a) 无官方/社区可用导出（唯一 ONNX repo 是 genai 组件拆分形态、0 下载、2026-04 后无回验）；(b) 即便自行导出，Qwen3-VL 动态分辨率视觉预处理 + chat template + EOS pooling + instruction 注入全部自实现（siglip2 kind 的 naflex 预处理已 1190 行，这里量级更大）；(c) **根本瓶颈是算力不是格式**——2B MLLM 单图 prefill ≈3–6 TFLOPs，8 核 CPU 每图十秒到分钟级（多模态档案 §D2 估算；M3 bench 实测同机 CPU 档：siglip2 单图 92ms、2B MLLM 按 ~4 个数量级差推算即分钟级）。**结论：CPU 默认部署不现实，维持档案定位「GPU 用户/远程 provider」**。
- **GPU 自托管 = vLLM**（官方指定，≥0.14.0，GitHub README 核实）。关键事实：vLLM 对 `Qwen3VLForConditionalGeneration` 的支持是 **pooling 原生**（vLLM 文档模型表列 `Qwen/Qwen3-VL-Embedding-2B etc. (T+I+V), LoRA+PP`）；注意 vLLM 与官方 qwen_vl_utils 的视频预处理"slightly different results"（vLLM 文档 Qwen3-VL 注记）——**服务栈一旦选定须钉死进配置**（见 §3.2 落地注意）。**TEI 不支持**（text-embeddings-inference 仓库为开放 feature request；文本系 Qwen3-Embedding 自 v1.7.3 支持）。
- **托管端点已存在且 wire 已定型**：IONOS AI Model Hub 托管 Qwen3-VL-Embedding-8B，**OpenAI 兼容 `POST /v1/embeddings`**，文本走 `input`、图像走 **`messages` 内容分片扩展**（与 vLLM 同方言，[docs](https://docs.ionos.com/cloud/ai/ai-model-hub/models/embedding-models/qwen3-vl-embedding-8b)，请求体逐字核实见 §3.2）；DashScope Model Studio 嵌入 API 覆盖文本/图像/视频输入（[官方文档索引](https://www.alibabacloud.com/help/en/model-studio/)级信号，wire 未逐字核验）。

---

## 2. 收益-成本

### 2.1 收益：统一空间 vs 双塔 RRF 的实证差距

无公开基准直接对比「双索引 RRF 合并」与「单空间」（如实声明）；以下用可分解证据链回答。

**质量证据（MMEB-V2，Qwen card 同表，2026-01 复评口径）**[来源：card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)：

| 模型 | 规模 | Image RET（文搜图 12 数据集） | VisDoc Overall（视觉文档 24 数据集） | All（78 任务） |
|---|---|---|---|---|
| VLM2Vec / VLM2Vec-V2 | 2B | 65.0 / 69.5 | 44.0 / 69.2 | 47.7 / 59.2 |
| GME-2B / GME-7B | 2B/7B | 66.9 / 71.2 | 76.8 / 79.3 | 55.3 / 59.1 |
| **Qwen3-VL-Embedding-2B** | 2B | **74.8** | **79.2** | **73.2** |
| **Qwen3-VL-Embedding-8B** | 8B | **80.0** | **82.4** | **77.8**（开源第一，arXiv 摘要：2026-01-08 时点 ranking first） |

对照我们双塔的两条腿：

1. **文字搜图路线的上限 = siglip2 文本塔**。双塔中文本 query 命中图像索引，走的是 SigLIP 文本编码器——页级/文档级检索 SigLIP 平均 51.4（ViDoRe v1）vs 单向量 MLLM 路线 85.8（DSE）（多模态档案 §C1 引 ColPali 论文表 2）；MMEB 上 CLIP 系 37.8 vs MLLM 统一嵌入 60.1→77.8（档案 §C1）。**差距集中在视觉密集文档**（扫描件/表格/图表页）；自然图像检索 CLIP 系本就强——**统一路线的收益主要落在文档/截图/指令化查询场景，不在自然图库场景**（档案 §C2 场景判定同向）。
2. **扫描件 OCR 路线 -14 点**：最佳 OCR 管线 67.0 vs 视觉路线 81.3（ViDoRe v1，档案 §C1）；Qwen3-VL-2B VisDoc 79.2 意味着统一路线在 scanned-docs 语义上有代差。但我们 M3 刚交付 pdf-ocr + scanned-docs preset（无 GPU 场景的正确降级路径，档案 §C3 判定"文本为主文档上 OCR≈视觉"），**两路线是硬件条件分流而非质量互斥**。
3. **RRF 只用秩不用分**是我们自己的有据设计裁决（pipeline/mod.rs weighted merge 注释明说跨模型 raw score 不可比，`Σ 1/(60+rank)`，RRF_K=60）——统一空间给回**单一校准分数**，top-k 截断不再依赖两路秩的运气；且 rerank（文本 cross-encoder）之上仍可叠加（Qwen 官方两段式：Embedding 召回 + Reranker 精排，GitHub Reranker 表：Reranker-2B 检索 75.2 > Embedding-2B 73.4）。
4. **能力差（非质量差，双塔无法补）**：统一空间天然支持**以图搜文**（图像 query 命中文本 chunk——当前形态图像 query 只能命中图像索引，ops_xvf.rs 路由实证）与**图文混合查询**；M4 规划的视频模态它原生覆盖（T+I+V，vs CLAP+帧抽取双件）。
5. **文本侧不劣化**（若整桶切统一）：MMTEB card 表 2B 63.87 / Retri 67.12，高于 BGE-M3（59.56/54.60）与 gte-Qwen2-1.5B-instruct（59.45/60.78），近 Qwen3-Embedding-0.6B（64.33/64.64）。注意：**我们默认 gte-multilingual-base 不在此表，无直接 C-MTEB 对照**——替换默认文本塔是另一个独立裁决，本报告不裁。

### 2.2 成本

| 维度 | 双塔现状（实测） | 统一 Qwen3-VL-2B |
|---|---|---|
| 模型分发 | gte 1.2GB + siglip2 1.5GB（M3 bench §1.1），CPU 可跑 | **2.13GB 起（8B 8.14GB）** + GPU 或远端依赖；CPU 不可跑（§1.3） |
| 查询延迟 | 文字搜图双路 49–67ms P50 热态；以图搜图 92ms（M3 bench §2，本机全链） | 远端 = 网络 RTT + 端点编码（估算 100–500ms 级，未实测）；本地 GPU = 毫秒级（档案 §D2）但要求 vLLM 级栈在场 |
| 摄取吞吐 | jpg <0.5s/份、批量修复后并发 ≤ 串行（M3 bench §6.1） | 图像每份都过 2B MLLM prefill——远端批量吞吐受端点限速；成本随图像数线性 |
| 硬件前提 | 无（CPU） | **GPU（vLLM 自托管）或商业端点（IONOS 等）**——与自托管离线定位构成张力，但张力形态 = §5.7 已让渡的「显式配置出网」（openai kind 先例），非新增类别 |
| 向量存储 | text chunk 768d×4B=3KiB（gte）+ image chunk 768d×4B=3KiB（siglip2） | 每 chunk 2048d×4B=**8KiB**（MRL 1024 → 4KiB）——**文本主导语料向量字节 ×2.7（全维）/×1.3（MRL 1024）**；换来单索引与跨模态可比分 |
| 运维面 | 两模型两索引，全本地 | 单模型单索引，但多一个外部依赖（端点可用性/凭证/限速重试——openai kind 的 429/整批重试机制可复用） |

**成本结论**：统一路线的代价不在模型体积（与双塔同数量级），在**推理前提**（GPU/远端）与**文本侧存储放大**；收益集中在**视觉密集文档质量（+14 点级）、以图搜文/混合查询能力、单索引单分数空间**。按 M3 bench 的判词口径：双塔在 CPU 档已交互级交付，统一路线在无 GPU 用户处**零可用**——它是「另一档硬件条件下的更优解」，不是当前形态的缺陷修复。

---

## 3. 形态选项评估

### 3.1 a. 不引入（维持双塔，等 M4+ 证据）

- **支持**：双塔 + OCR 降级 + rerank 已交付且全形态交互级（M3 bench §0）；统一路线的质量增益场景（视觉密集文档）我们已有 OCR 管线承接（-14 点但可用）；无 GPU 用户（产品基本盘）完全无法消费统一路线；§5.10.2 的 M4 行（clap/远端视频）才是下一个模态决策点，届时视频原生支持（T+I+V）是统一路线的最强论据。
- **反对**：以图搜文能力缺口（图像 query 搜不到文本 chunk）在当前形态下**永远补不上**；文档密集场景的代差已实证（§2.1）。
- **判词**：作为 M3 收官动作完全成立；但「不引入」宜表述为「不在 M3 写代码」，而非「路线否决」。

### 3.2 b. 远端 provider 形态——「零产品代码」命题的代码核查更正

**代码事实（本轮逐一核实）**：既有 `openai` kind 是**纯文本** kind——

- `embed/mod.rs` L181–192：`EmbedderKind::OpenAi` 的 doc_modalities = `&[Modality::Text]`、query_modalities = `&[Modality::Text]`（与 ort 同）；图像 chunk 到达即 `image_chunk_refusal` 响亮失败（路由 bug 防线）。
- `openai.rs` 模块文档 L44：wire = `POST {endpoint}/embeddings` + `{"model": …, "input": [texts…]}`——**没有图像通道**。

而统一 embedder 的远端 wire 是 **OpenAI 扩展方言**（vLLM 文档 + IONOS 文档逐字核实）：

```json
POST /v1/embeddings
{ "model": "Qwen/Qwen3-VL-Embedding-8B",
  "messages": [{ "role": "user", "content": [
      {"type": "image_url", "image_url": {"url": "data:image/png;base64,…"}},
      {"type": "text", "text": "…"} ]}] }
```

（纯文本时兼容 `input`；服务端需 `--runner pooling --chat-template …`，MRL 走 `dimensions` 参数。）

**因此**：形态 b 拆成两半——

- **b0「零代码、纯文档」**：今天把 vLLM/IONOS 端点配进 `[embedder.remote] kind="openai"`，**只能当又一个纯文本 embedder 用**（统一收益为零，只等于换了个文本模型）。零产品代码为真、统一语义为假。
- **b1「openai kind 多模态输入扩展」**：给 openai kind 增加该 messages 方言（doc 侧 [Text, Image]、query 侧 [Text, Image]、base64 编码、instruction/dim 进配置）。**复用**：HTTP/重试/整批幂等/https 强制/dim 声明机制全部现成（openai.rs 773 行不动）；**新增**：payload 构造 + 模态矩阵翻转 + 配置字段（instruction、可选 `image_query`）+ contract 测试。量级估算 **200–400 行**（对照：siglip2 kind 含 naflex 预处理 + ort 会话共 1190 行，此处无预处理无推理，只有 JSON 形状）。无新 .so、无 GPU、无新依赖；机制层（模态自声明路由、查询 fan-out、单 list merge 恒等、rerank 叠加）**零改动**——v2.1 的模态路由裁决正是为此预留的。
- **落地注意（若 M4 采纳 b1）**：① instruction 必须入库/查询两侧一致且**进管线指纹**（配置即指纹，机制已有）；② MRL 截断维度 = 声明 dim，请求 `dimensions` 或客户端截断+重归一化，实现期定；③ 服务栈差异（vLLM vs qwen_vl_utils 预处理微差）⇒ 端点/model 组合进指纹语义（同 model 不同栈视为不同 embedder 实例）；④ Ollama 形态（社区有 `qwen3-vl-embedding:2b` 集成信号）wire 未核验，不进文档。

### 3.3 c. 内置 qwen3vl kind（GPU 用户本地）

三重硬冲突，**否决**：

1. **无 ort 路径**：无官方/可用 ONNX（§1.3）；自行导出 = 动态分辨率预处理 + chat template + EOS pooling 全自研且要自证与官方一致——kind 体量数倍于 siglip2（1190 行），且导出物无法 sha256 预取管理（无官方 artifact）。
2. **§5.7 无 GPU 假设冲突**：产品推理栈是 ort CPU EP（pin ORT 1.31.0，runtime-libs 单 .so 管理）；GPU ort = 第二份 CUDA EP 运行库 + .so 白名单破坏；GPU bf16 2B（2.13GB 权重 + KV/激活）起步即 8GB 级 VRAM（估算）——「GPU 用户本地」是一个**我们没有服务面/测试面/CI 面的新硬件档位**，一个 kind 换来一整条支持矩阵。
3. **Python 栈排除**：真要做 GPU 本地只有 vLLM/SGLang 形态——那是产品外部的用户自建服务，恰好就是形态 b 的远端端点（loopback http 已被 openai kind 允许，openai.rs 明示 "loopback http is kept for on-box gateways"）。**c 的合理内核就是 b**。

### 3.4 三形态对照

| | a 不引入（M3 零代码） | b0 零代码文档化 | b1 openai 扩展（M4 候选） | c 内置 kind |
|---|---|---|---|---|
| 产品代码 | 0 | 0 | ~200–400 行（估） | 数千行级 + 预取/导出自证 |
| 统一收益 | 0 | **0（仅文本可用）** | 全量（文搜图/图搜文/混合） | 全量 |
| 新硬件/依赖 | 无 | 无 | 无（复用出网白名单） | GPU 档位 + 双运行库风险 |
| 离线定位冲突 | 无 | 无 | 无（opt-in，同既有 openai） | **正面冲突** |
| 质量兑现 | 双塔现状 | 不变 | MMEB-V2 73.2/77.8（远端） | 同左（本地 GPU） |

---

## 4. 裁决建议

**建议采纳：a + b 的组合——M3 零代码收官；统一 embedder 的既定形态裁决为「远端 provider（b1 方言扩展）」，工件排 M4；c 否决。** 数据依据回放：

1. **质量侧已够格、门槛不在模型**：2B 即 MMEB-V2 73.2 开源第一梯队、Apache-2.0、MRL、协议同构（§1、§2.1）——「值不值得引入」的答案是**值得，但只对有 GPU/愿意出网的用户**，且其收益主体（视觉密集文档、以图搜文、视频）恰好是 M4 的决策域。
2. **「零产品代码」命题不成立是本轮最重要的代码核查更正**（§3.2）：统一语义要求 openai kind 学 messages 方言（估 200–400 行）。这不是拒绝 b 的理由，而是把 b 从「文档化即可」修正为「一个有界、零依赖、机制层零改动的小工件」——**不足以在 M3 收官日插单，恰好适合 M4 排期**。
3. **c 在本项目结构内不可实现**（无 ort 路径 + 无 GPU 档位 + Python 栈违反 §5.7 依赖纪律），其合理内核（loopback vLLM 网关）已被 b 覆盖。
4. **M3 交付形态无需回撤**：双塔 + OCR 降级 + rerank 在 CPU 档交互级可用且刚过收官 bench；统一路线是硬件条件的升档选项，不是缺陷修复（§2.2 结论）。

**建议的 §5.10.2 Embedder 行文案**（供裁决）：`ort / openai（M2）；siglip2（M3）；qwen3vl 统一评估完毕（2026-11）——不设内置 kind（无 ort 路径/CPU 不可行，research/2026-11-m3-unified-embedder-eval.md）；统一多模态 embedder 的既定形态 = 远端 provider（openai kind 的 vLLM messages 多模态方言扩展，b1，M4 候选工件）；clap 声学（M4）；远端视频（Nova/TwelveLabs）作 provider 扩展`。

**M4 重评触发器**（写明判据，避免重开泛泛调研）：① 视频模态选型时统一模型（T+I+V 原生）vs CLAP+帧抽取的直接对比；② 以图搜文/混合查询的真实用户需求信号；③ b1 落地后 scanned-docs 语义档（OCR 路线）与统一路线的同语料对照实测（ViDoRe 类 -14 点代差在本仓语料上的复现度）。

---

## 附：本文引用的一手来源索引

- HF API（license tag / safetensors / 文件清单，本轮逐一核实）：[Qwen3-VL-Embedding-2B](https://huggingface.co/api/models/Qwen/Qwen3-VL-Embedding-2B)、[Qwen3-VL-Embedding-8B](https://huggingface.co/api/models/Qwen/Qwen3-VL-Embedding-8B)、[Qwen org 型号清点](https://huggingface.co/api/models?author=Qwen&search=Qwen3-VL-Embedding)、[MrLight/dse-qwen2-2b-mrl-v1](https://huggingface.co/api/models/MrLight/dse-qwen2-2b-mrl-v1)、[jina-embeddings-v4](https://huggingface.co/api/models/jinaai/jina-embeddings-v4)、[MM-Embed](https://huggingface.co/api/models/nvidia/MM-Embed)、[gme-Qwen2-VL-2B](https://huggingface.co/api/models/Alibaba-NLP/gme-Qwen2-VL-2B-Instruct)、[社区 ONNX 导出](https://huggingface.co/api/models/PIA-SPACE-LAB/Qwen3-VL-Embedding-2B-onnx)、[onnx-community Qwen3-VL 检索](https://huggingface.co/api/models?author=onnx-community&search=Qwen3-VL)、[变体全量搜索（GGUF 32 档实证）](https://huggingface.co/api/models?search=Qwen3-VL-Embedding&limit=100)。
- model card / 官方脚本（逐字/逐行核实）：[Qwen3-VL-Embedding-2B card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)（规格表/MMEB-V2/MMTEB/指令注意）、[jina-v4 card License 节](https://huggingface.co/jinaai/jina-embeddings-v4)、[`scripts/qwen3_vl_embedding.py`](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B/raw/main/scripts/qwen3_vl_embedding.py)（system-prompt instruction + EOS pooling + L2 归一化）。
- GitHub：[QwenLM/Qwen3-VL-Embedding README](https://github.com/QwenLM/Qwen3-VL-Embedding)（vLLM ≥0.14、架构节、Reranker 对照表、输入规格）、[llama.cpp #19516](https://github.com/ggml-org/llama.cpp/discussions/19516)、[text-embeddings-inference](https://github.com/huggingface/text-embeddings-inference)（TEI 支持面）。
- 官方文档：[vLLM pooling/embeddings](https://docs.vllm.ai/en/latest/models/pooling_models/embed)（Qwen3VL 原生支持、messages 多模态方言、`--runner pooling`/chat-template、DSE 占位图注意、`dimensions` 截断、Qwen3-VL 预处理微差注记）、[IONOS AI Model Hub Qwen3-VL-Embedding-8B](https://docs.ionos.com/cloud/ai/ai-model-hub/models/embedding-models/qwen3-vl-embedding-8b)（OpenAI 兼容 + messages 图像输入请求体）、[Alibaba Cloud Model Studio](https://www.alibabacloud.com/help/en/model-studio/)。
- arXiv：[Qwen3-VL-Embedding 技术报告 2601.04720](https://arxiv.org/abs/2601.04720)（2B/8B、32K、MMEB-V2 77.8 第一）。
- 本仓（代码事实，本轮核实）：`crates/server/src/pipeline/embed/mod.rs`（kind 注册表/模态矩阵/闭集纪律）、`embed/openai.rs`（纯文本 wire/dim 声明/loopback http）、`pipeline/mod.rs`（RRF_K=60、秩融合裁决注释）、`ops_xvf.rs`（查询 fan-out 与图像 query 单路、rerank 需文本 query）、`preset.rs`（multimodal 双塔形态）；design.md §5.7/§5.10.2/§5.10.5/§7。
- 既有档案（引用不重查）：[2026-11-multimodal-embedding.md](2026-11-multimodal-embedding.md) §A2（MLLM 路线与 jina-v4 许可）、§B1（模型总表）、§B3（中文证据）、§C1（ViDoRe/MMEB 数字）、§C2（单向量裁决）、§C3（扫描件三路线）、§D2（CPU 延迟估算）、§D6（四维打分）；[2026-11-m3-bench.md](2026-11-m3-bench.md) §0/§1.1/§2/§6.1；[2026-11-ocr-reranker-selection.md](2026-11-ocr-reranker-selection.md) §2（reranker 裁决先例）。
