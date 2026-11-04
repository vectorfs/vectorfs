# 专项调研：原生多模态嵌入（multimodal embedding）技术路线

- 日期：2026-11-04
- 背景：vectorfs 上层"文件即向量化"已裁决；文本路线已定（默认 bge-m3 / Qwen3-Embedding-0.6B，ort+fastembed 本地推理，LangChain 式 provider trait）。本文件专项回答：图片（及扫描件 PDF、图表）如何与文本进同一向量空间被检索。
- 调研纪律：所有论断回到一手来源（arXiv 原文、HF model card / HF API license 元数据、官方文档、官方博客）；license 以 HF model card / 官方仓库为准。标注 [来源](url)；标注"估算"的为推算值，非实测。

---

## A. 技术路线图谱

### A1. 双编码器对比学习（CLIP 系）

**原理**：图像编码器（ViT/ResNet）+ 文本编码器各自编码后经投影到共享 d 维空间，对比损失（softmax InfoNCE 或 sigmoid loss）拉近正样本图文对。查询文本与库内图像各出**一个向量**，cosine/点积检索。

**代表模型**：
- CLIP（OpenAI，4 亿图文对，arXiv [2103.00020](https://arxiv.org/abs/2103.00020)）；SigLIP（sigmoid 逐对损失替代 softmax，arXiv [2303.15343](https://arxiv.org/abs/2303.15343)）。
- **SigLIP 2**（Google DeepMind，2025-02）：WebLI 多语言数据 + captioning（LocCa 式）+ 自监督（self-distillation、masked prediction）+ 在线数据策划统一配方；发布 ViT-B(86M)/L(303M)/So400m(400M)/g(约 1B) 四档，并有 **naflex** 变体（原生宽高比 + 多分辨率）。[来源：arXiv 2502.14786](https://arxiv.org/abs/2502.14786)。HF 上 google 名下 38 个 checkpoint **全部 Apache-2.0**（HF API 逐一核实）[来源](https://huggingface.co/api/models?search=siglip2&author=google&limit=50)。
- **EVA-CLIP**（BAAI）：8B/18B 放大版，LAION-2B+COYO 训练，Apache-2.0。[来源：HF BAAI/EVA-CLIP-8B license 核实](https://huggingface.co/api/models/BAAI/EVA-CLIP-8B)，[arXiv 2402.04252](https://arxiv.org/abs/2402.04252)。
- **Chinese-CLIP**（OFA-Sys）：约 2 亿中文图文对训练，发布 RN50(77M)/ViT-B-16(188M)/ViT-L-14(406M)/ViT-L-14@336(407M)/ViT-H-14(958M) 五档。[来源：GitHub](https://github.com/OFA-Sys/Chinese-CLIP)、[arXiv 2211.01335](https://arxiv.org/abs/2211.01335)。**注意：HF 权重仓库无 license 标注（API 核实无 license tag），代码 MIT**——内置分发有法律模糊。
- **AltCLIP**（BAAI）：CLIP 蒸馏进多语文本编码器的中英双语版，license = creativeml-openrail-m。[来源：HF API](https://huggingface.co/api/models/BAAI/AltCLIP)。

**质量特征**：零样本分类/自然图像检索是基本盘；**页级/文档/密集文本检索很弱**——ColPali 论文 ViDoRe 页级检索中 SigLIP 平均 51.3、Jina-CLIP 17.7、nomic-vision 12.9（nDCG@5），而 ColPali 81.3 [来源：arXiv 2407.01449 v2 表2](https://arxiv.org/html/2407.01449v2)。组合推理（属性绑定、计数）弱于 MLLM 路线。

**推理成本**：最低。ViT-B/16@224 约 17.5 GFLOPs/图；SigLIP2 so400m@384 约百 GFLOPs 级（估算：576 patch + 1152 宽 27 深）——CPU 单图 0.1–1 秒量级（估算）。

**协议契合度**：**完美**。双塔各自产出单 float32 向量、cosine 度量，与 S3 Vectors"每 key 单向量"完全同构；文本查询侧也是单向量。

### A2. MLLM 统一嵌入（把多模态 LLM 当 embedder）

**原理**：取 MLLM（Qwen2-VL / LLaVA-NeXT / Phi-3.5-V 等）最后一个 token 的 last hidden state（经 L2 归一化）作为整个输入（文本/图像/图文交错/视频）的**单一向量**；配 instruction prompt 与对比学习训练。组合推理、指令跟随、文档页级理解远强于 CLIP。

**代表模型（按时间线）**：
- **E5-V**（2024-07，arXiv [2407.12580](https://arxiv.org/abs/2407.12580)）：LLaVA-LLaMA-3-8B（8.36B，[HF](https://huggingface.co/royokong/e5-v)），仅用文本 NLI 对比训练即桥接模态；prompt "Summary above image in one word:"；组合推理（SugarCrepe）强，但**检索基准弱**：MMEB 13.3、UMRB 42.52（见 C 节）。代码 Apache-2.0 [来源](https://github.com/kongds/E5-V)。
- **VLM2Vec**（2024-10，arXiv [2410.05160](https://arxiv.org/abs/2410.05160)）：提出 **MMEB** 基准（分类/VQA/检索/grounding 四元任务）；Phi-3.5-V LoRA 版 MMEB **60.1** vs CLIP 37.8。V2（2025-07，arXiv [2507.04590](https://arxiv.org/abs/2507.04590)）扩展为 **MMEB-V2**（78 任务，新增视觉文档检索、视频检索等）；Qwen2-VL 底座的 V1 版 MMEB 60.1/65.8。[来源：GitHub](https://github.com/TIGER-AI-Lab/VLM2Vec)。VLM2Vec-Full Apache-2.0（HF API 核实）。
- **GME**（Alibaba，2024-12，arXiv [2412.16855](https://arxiv.org/abs/2412.16855)）：Qwen2-VL-2B/7B 底座，合成多模态数据训练，统一文本/图像/图文对/视频嵌入；UMRB 2B=64.45、7B=67.44。**Apache-2.0**（HF API 核实 [gme-Qwen2-VL-2B](https://huggingface.co/api/models/Alibaba-NLP/gme-Qwen2-VL-2B-Instruct)），中文强（C-MTEB T2Retrieval MRR@10 93.4，[model card](https://huggingface.co/Alibaba-NLP/gme-Qwen2-VL-2B-Instruct)）。
- **DSE**（2024，arXiv [2406.11251](https://arxiv.org/abs/2406.11251)）：Document Screenshot Embedding，Qwen2-VL 直接把文档截图编成单向量，ViDoRe v1 达 85.8 nDCG@5（2B 版，[model card](https://huggingface.co/MrLight/dse-qwen2-2b-mrl-v1)），支持 MRL（1536→512 截断），Apache-2.0。文档场景单向量路线的标志工作。
- **MM-Embed**（NVIDIA，2024-11，arXiv [2411.02571](https://arxiv.org/abs/2411.02571)）：8.18B，M-BEIR 52.7（超 UniIR 48.9），instruction tuning + modality-aware hard negative。**CC-BY-NC-4.0**（HF API 核实）——不可内置。[来源](https://huggingface.co/api/models/nvidia/MM-Embed)。
- **BGE-VL**（BAAI，2024-12–2025）：MegaPairs 合成数据（ACL 2025 Oral，[GitHub](https://github.com/VectorSpaceLab/MegaPairs)）；BGE-VL-base/large = CLIP ViT-B/L 微调（MIT，512 维）；BGE-VL-MLLM-S1/S2 = LLaVA-NeXT(Mistral-7B) 7.57B（MIT，[HF API](https://huggingface.co/api/models/BAAI/BGE-VL-MLLM-S2)），零样本 MMEB SOTA 声称。[来源：bge-model.com](https://bge-model.com/bge/bge_vl.html)。
- **UniME**（DeepGlint，2025-04，arXiv [2504.17432](https://arxiv.org/abs/2504.17432)，ACM MM 2025）：两阶段（文本判别知识蒸馏→组合增强），曾登 MMEB 榜首（2025-05 截图）；代码 MIT，HF 权重 gated（401）。续作 UniME-V2（arXiv [2510.13515](https://arxiv.org/abs/2510.13515)）。
- **jina-embeddings-v4**（2025-06）：Qwen2.5-VL-3B 底座 3.75B，单向量 2048（MRL 128–2048）+ **late-interaction 多向量**（128 维）+ 32K 上下文，task/prompt 指令制。**license 实为 Qwen Research License**（model card 明确"correct license is the Qwen Research License, derived from Qwen2.5-VL-3B"，最初误标 CC-BY-NC）——**不可内置商用**。[来源：model card](https://huggingface.co/jinaai/jina-embeddings-v4)。
- **Qwen3-VL-Embedding 2B/8B**（2026，arXiv [2601.04720](https://arxiv.org/abs/2601.04720)）：当前 MMEB-V2 开源最强梯队——2B 73.2 / 8B 77.8（8B：image 80.1、video 67.1、VisDoc 82.4）；文本/图像/截图/视频/混合输入统一；**MRL 64–2048（2B）/64–4096（8B）+ 输出量化**；32K 上下文；instruction 感知（指令带来 1–5% 提升）；配套 Qwen3-VL-Reranker 2B/8B 组成两段式；vLLM ≥0.14 支持；**Apache-2.0**（HF API 核实，[2B card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)、[GitHub](https://github.com/QwenLM/Qwen3-VL-Embedding)）。

**质量特征**：组合推理、指令化查询、文档页/截图/表格类任务显著强于 CLIP 系（见 C 节量化）。**推理成本**：2B 级 MLLM 单图 prefill 约 3–6 TFLOPs（估算：2×参数×~800 视觉 token），CPU 上**每图十秒到分钟级**（估算，8 核 int8），GPU 上十毫秒级——CPU 默认部署不现实，适合 GPU 用户/远程 API。

**协议契合度**：单向量，契合；但注意 (a) instruction/prompt 需在查询与入库两侧一致；(b) 维度大（1536–4096），可 MRL 截断；(c) 无 sparse/多向量需求。

### A3. 模态对齐伴生编码器（companion vision encoder）

**原理**：冻结既有文本嵌入模型，仅训练一个视觉编码器投影进**该文本模型的向量空间**（LiT 反向）——不动文本索引，事后补图像能力。

**代表**：**nomic-embed-vision-v1 / v1.5**（无 v2）。v1.5 与 nomic-embed-text-v1.5 **共享嵌入空间**，768 维；ImageNet 0-shot 71.0、Datacomp 56.8、MTEB 62.28（超 CLIP B/16 的 43.82）；文本查询加 `search_query:` 前缀。**license 已从 CC-BY-NC-4.0 重授为 Apache-2.0**。[来源：model card](https://huggingface.co/nomic-ai/nomic-embed-vision-v1.5)、[官方公告](https://www.nomic.ai/news/nomic-embed-vision)、[license 讨论](https://huggingface.co/nomic-ai/nomic-embed-vision-v1.5/discussions/3)、[arXiv 2406.18587](https://arxiv.org/html/2406.18587v1)。ONNX 有 Xenova 转换（含量化 + WebGPU）。

**适用条件**：仅当文本侧已用对应文本模型（nomic-embed-text-v1.5）时才有意义；文档/图表弱；无 MRL。对 vectorfs（文本默认 bge-m3）**不匹配**，除非整体切到 nomic 文本栈。

### A4. Late-interaction 多向量（ColPali 系）

**原理**：文档页转图 → VLM 输出 **patch 级多向量集合**（每页约 750–1030 个 128 维向量）→ 查询词向量与页向量做 ColBERT 式 **MaxSim** 求和。细粒度匹配能力强，是视觉文档检索 top-1 精度天花板。

**代表**：ColPali（PaliGemma-3B 底座，arXiv [2407.01449](https://arxiv.org/abs/2407.01449)）、ColQwen2（Qwen2-VL-2B 底座）、ColQwen2.5（Qwen2.5-VL-3B 底座）、ColSmol（PaliGemma 256M/500M）。license 分层：**ColPali/ColSmol 的 PaliGemma 底座是 Gemma license（限制性），adapter MIT/Apache**；**ColQwen2 的 Qwen2-VL 底座 Apache-2.0 + adapter MIT（HF 标签 apache-2.0，可商用）**；ColQwen2.5 HF 标签 MIT，Qwen2.5-VL 底座 Apache-2.0（API 核实）。[来源：colqwen2-v1.0 card](https://huggingface.co/vidore/colqwen2-v1.0)、[colqwen2.5-v0.2 API](https://huggingface.co/api/models/vidore/colqwen2.5-v0.2)、[Qwen2.5-VL-7B API](https://huggingface.co/api/models/Qwen/Qwen2.5-VL-7B-Instruct)、[colpali repo（代码 Apache-2.0）](https://github.com/illuin-tech/colpali)。

**协议契合判断（明确回答）**：**塞不进 S3 Vectors 协议面**。S3 Vectors 的向量 = 每 key 恰好一个 float32 数组（PutVectors `vectors[].data.float32`），index 的 dimension 在创建时固定 1–4096；MaxSim 需要"每 key N×128 矩阵 + 查询侧多向量"，该形态**不存在任何表达**（协议面连查询侧多向量入口都没有）。只能走 **x-vf-\* 超集 + 独立存储/算子**：Lance 原生支持 multivector 列（list\<fixed_size_list\<f32\>\>），LanceDB 1.29+ 提供 multivector search（cosine，接受查询向量矩阵）[来源：LanceDB docs](https://docs.lancedb.com/search/multivector-search)、[HF MultiVectorEncoder 博客](https://huggingface.co/blog/multi-vector-encoder)。且 MaxSim 无标准 ANN 加速，实践中是"单向量粗召回 + 多向量精排"两段式（见 C2 HEAVEN 证据）——**双引擎复杂度评估见 D1，初判 v1 不做**。

### A5. 音频/视频长尾（简述）

- **CLAP**（LAION）：音频-文本对比双编码器，laion/clap-htsat-fused Apache-2.0（HF API 核实）。音频对音频/文本查询。
- **TwelveLabs Marengo Embed API**（商业）：视频原生 embedding；2.x 为 1024 维，3.0 降为 512 维（宣称比 Nova 3072 维省 6x 存储），3.5 按模态 token 计费；可用于 Bedrock/OpenSearch。[来源：官方博客](https://www.twelvelabs.io/blog/marengo-3-0)、[docs](https://docs.twelvelabs.io/docs/guides/create-embeddings/query)。
- 开源侧视频嵌入：Qwen3-VL-Embedding（视频输入原生支持）与 Nova Multimodal Embeddings（30 秒分段）是 2026 年的新选项（见 B）。

---

## B. 可本地部署模型清单

### B1. 开源模型总表

参数量/体积来自 HF API 或 model card；"磁盘"按 fp16≈2 bytes/param 估算（另注明）；license 以 HF model card/API tag 为准。

| 模型 | 参数量 / 磁盘(约) | 维度 | MRL | 分辨率/上下文 | License | ONNX/GGUF | CPU 可行性(估) |
|---|---|---|---|---|---|---|---|
| **SigLIP2-base (naflex/224–512)** | 86M / ~170MB fp16 | 768 | 无 | 固定 224–512 或 naflex 动态 | **Apache-2.0**（38 ckpt 全系） | 社区 ONNX（immich-app so400m 版、onnx-community gopt 版、guruansh naflex 版） | 好，<0.3s/图 |
| **SigLIP2-so400m-patch16-384** | 400M / ~800MB | **1152**（config 核实） | 无 | 384（naflex 动态） | Apache-2.0 | 同上（immich-app 生产在用） | 中，~0.3–1s/图 |
| **SigLIP2-giant-opt-patch16-384** | ~1.1B / ~2.2GB | 1536（config 核实） | 无 | 256/384 | Apache-2.0 | onnx-community/siglip2-giant-opt-patch16-384-ONNX | 弱，秒级 |
| **Chinese-CLIP ViT-B/16** | 188M / ~380MB | 512 | 无 | 224 | **无 license 标注**（代码 MIT）— 分发风险 | 社区 ONNX（Xenova 镜像） | 好 |
| **Chinese-CLIP ViT-H/14** | 958M / ~1.9GB | 1024 | 无 | 224 | 同上 | 无官方 | 弱 |
| **EVA-CLIP-8B** | 8B / ~16GB | 未核实（非推荐项） | 无 | 224 | Apache-2.0 | 无现成 | 不可（CPU） |
| **jina-clip-v1** | 223M / ~450MB fp16 | 768 | 无 | 224（tile） | **Apache-2.0** | **官方 ONNX**（card 标注） | 好 |
| **jina-clip-v2** | 865M / ~1.7GB | 1024 | **是（64–1024）** | 512×512 | **CC-BY-NC-4.0** — 不可内置 | 社区（Replicate 等） | 中 |
| **nomic-embed-vision-v1.5** | 93M(vision塔) / ~190MB | 768（=nomic-text-v1.5 空间） | 无 | 448 | **Apache-2.0**（重授） | Xenova ONNX（含量化/WebGPU） | 好 |
| **BGE-VL-base / large** | 150M / 428M | 512（base, card 实测）/ 768（large, CLIP-L 惯例） | 无 | CLIP 224 | **MIT** | 无官方 | 好/中 |
| **BGE-VL-MLLM-S2** | 7.57B / ~15GB | 4096（LLaVA-NeXT hidden，Mistral-7B 底座） | 无 | 动态 | **MIT** | 无 | 不可（CPU） |
| **GME-Qwen2-VL-2B** | 2.2B / ~4.4GB | 1536 | 无（card 未见 MRL） | 动态（Qwen2-VL 原生） | **Apache-2.0** | 无官方（fastembed/vLLM 均在诉求中，见其 issue） | 差，十秒级/图 |
| **GME-Qwen2-VL-7B** | 8.3B / ~17GB | 3584（Qwen2-VL-7B hidden） | 无 | 动态 | Apache-2.0 | 无 | 不可 |
| **DSE-Qwen2-2B-MRL** | 2.2B / ~4.4GB | 1536 | **是（→512）** | 动态（建议 resize 680×670 控显存） | **Apache-2.0** | 无官方 | 差 |
| **E5-V** | 8.36B / ~16GB | 4096（LLaMA-3 hidden） | 无 | 336 | GitHub Apache-2.0（HF 无 tag） | 无 | 不可；且检索质量弱 |
| **VLM2Vec-Full** | 4.15B / ~8.3GB | 3072（Phi-3.5 hidden） | 无 | 336 | Apache-2.0 | 无 | 不可 |
| **MM-Embed** | 8.18B / ~16GB | 4096 | 无 | 动态 | **CC-BY-NC-4.0** — 排除 | 无 | 不可 |
| **jina-embeddings-v4** | 3.75B / ~7.5GB | 2048（单向量）+128（多向量） | **是（128–2048）** | 32K / 动态 | **Qwen Research License** — 排除内置 | 官方 GGUF repo（jina-embeddings-v4-gguf） | 差（GGUF/CPU 勉强） |
| **UniME**（Phi3.5-V-4.2B/LLaVA-7B） | 4–8B | 底座 hidden | 无 | 336 | 代码 MIT；HF 权重 gated | 无 | 不可 |
| **Qwen3-VL-Embedding-2B** | 2.13B / 4.26GB(BF16) | 2048 | **是（64–2048）+ int8/binary 输出量化** | 32K、30+ 语言 | **Apache-2.0** | 无官方 ONNX；**社区 GGUF 28 个 + WebGPU Space 实测跑通** | 差（CPU 每图十秒级）；GPU 佳 |
| **Qwen3-VL-Embedding-8B** | 8B / ~16GB | 4096 | 是（64–4096） | 32K | Apache-2.0 | 同上 | 不可（CPU） |
| **CLAP（音频）** | ~150M | 512 | 无 | 音频 | Apache-2.0 | 社区 | 好 |

来源：SigLIP2 各档 [arXiv 2502.14786](https://arxiv.org/abs/2502.14786)、[HF API 全量 license](https://huggingface.co/api/models?search=siglip2&author=google&limit=50)、so400m config projection 1152（`config.json` 实测）、giant-opt config projection 1536（实测）；Chinese-CLIP [GitHub](https://github.com/OFA-Sys/Chinese-CLIP)/[HF](https://huggingface.co/OFA-Sys/chinese-clip-vit-base-patch16)（无 license tag，API 核实）；EVA-CLIP [API](https://huggingface.co/api/models/BAAI/EVA-CLIP-8B)；jina-clip-v1 [card](https://huggingface.co/jinaai/jina-clip-v1)（Apache-2.0、ONNX、MSCOCO R@1 0.411）与 [jina.ai 规格页](https://jina.ai/models/jina-clip-v1)（768 维、223M）；jina-clip-v2 [card](https://huggingface.co/jinaai/jina-clip-v2)（CC-BY-NC-4.0、865M、MRL 64–1024、[arXiv 2412.08802](https://arxiv.org/html/2412.08802v1)）；nomic-vision [card](https://huggingface.co/nomic-ai/nomic-embed-vision-v1.5)；BGE-VL [API](https://huggingface.co/api/models/BAAI/BGE-VL-MLLM-S2)/[bge-model.com](https://bge-model.com/bge/bge_vl.html)；GME [API](https://huggingface.co/api/models/Alibaba-NLP/gme-Qwen2-VL-2B-Instruct)；DSE [card](https://huggingface.co/MrLight/dse-qwen2-2b-mrl-v1)；E5-V [GitHub](https://github.com/kongds/E5-V)；VLM2Vec [API](https://huggingface.co/api/models/TIGER-Lab/VLM2Vec-Full)；MM-Embed [API](https://huggingface.co/api/models/nvidia/MM-Embed)；jina-v4 [card](https://huggingface.co/jinaai/jina-embeddings-v4)；Qwen3-VL-Embedding-2B [card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)/[GitHub](https://github.com/QwenLM/Qwen3-VL-Embedding)；CLAP [API](https://huggingface.co/api/models/laion/clap-htsat-fused)。

### B2. 商业 API 对照简表

| 服务 | 模态 | 维度 | MRL | 定价要点 | 备注 |
|---|---|---|---|---|---|
| **Cohere Embed v4**（2025-04） | 文本+图像+PDF 混合 | 256/512/1024/**1536**(默认) | 是 | 文本 ~$0.12/M tok、图像 ~$0.47/M tok（二手汇总，官方定价页为准）；128K 上下文 | [官方博客](https://cohere.com/blog/embed-4)、[docs](https://docs.cohere.com/docs/cohere-embed)。**Embed 5**（2026-09-30）新增 768/2048 维、int8/binary 量化、Pro/Fast 两档（[博客](https://cohere.com/blog/embed-5)） |
| **Voyage voyage-multimodal-3 / 3.5** | 文本+图像（3.5 加视频） | 3: 1024 固定；3.5: 256/512/1024/2048 | 3.5 有 | $0.12/M 文本 token；像素计价（560px=1 token）；200M token 免费 | [docs](https://docs.voyageai.com/docs/multimodal-embeddings)、[pricing](https://docs.voyageai.com/docs/pricing) |
| **Amazon Titan Multimodal Embeddings G1** | 文本+图像 | 1024/384/256 | 否（需归一化修正因子） | ~$0.00006/1k 输入（二手，官方页为准） | 上一代；官方 KB 文档仍列为多模态选项 [来源](https://docs.aws.amazon.com/bedrock/latest/userguide/kb-how-data.html) |
| **Amazon Nova Multimodal Embeddings**（nova-2-multimodal-embeddings-v1） | **文本+文档+图像+视频+音频** 统一 | 3072/1024/384/256 | **是（MRL）** | 见 Bedrock 定价页 | 文本 8K tok/~200 语言；视频音频 30s 分段；**与 S3 Vectors 原生集成（发布即支持 OpenSearch 与 S3 Vectors）**。[来源：AWS 博客](https://aws.amazon.com/blogs/aws/amazon-nova-multimodal-embeddings-now-available-in-amazon-bedrock) |
| **TwelveLabs Embed（Marengo 3.5）** | 视频+图像+音频 | 512（3.0 起） | — | 按模态 token 计费 | 视频场景专用。[来源](https://www.twelvelabs.io/blog/marengo-3.5) |
| **OpenAI** | **无多模态嵌入 API**（仅 text-embedding-3 系） | — | — | — | [官方 embeddings 文档](https://developers.openai.com/api/docs/guides/embeddings)、[社区确认](https://community.openai.com/t/get-embeddings-for-images/524241) |

### B3. CJK / 中文跨模态质量证据

- **Chinese-CLIP**：2 亿中文图文对原生训练；零样本 MUGE T2I R@1 63.0、Flickr30K-CN 71.2、COCO-CN 69.2（[card 表](https://huggingface.co/OFA-Sys/chinese-clip-vit-base-patch16)，大尺寸档更高）。中文跨模态检索的老牌选择。
- **SigLIP 2**：WebLI 多语言 + 论文明确"much better multilingual understanding and improved fairness"（[arXiv 2502.14786](https://arxiv.org/abs/2502.14786)）；英文以外语言（含中文）比 SigLIP1 明显提升，但无中文专门基准数字。
- **GME**：model card 附完整 C-MTEB（T2Retrieval MRR@10 93.391 等），zh tag；中文文档/图文场景有直接证据（[card](https://huggingface.co/Alibaba-NLP/gme-Qwen2-VL-2B-Instruct)）。
- **jina-clip-v2**：89 语言（[card](https://huggingface.co/jinaai/jina-clip-v2)）——多语言最强双编码器，但 CC-BY-NC。
- **Qwen3-VL-Embedding**：30+ 语言；官方建议 instruction 用英文写（[card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)）。
- 结论：**中文场景 Apache 系里 SigLIP2（通用）+ GME/Qwen3-VL-Embedding（文档/图文，重）覆盖；Chinese-CLIP 质量好但权重许可不明**。

---

## C. 质量证据与"单向量死锁"重审

### C1. 三大基准上的量化对比（2024→2026）

基准说明：MMEB（VLM2Vec 提出，20 任务，[arXiv 2410.05160](https://arxiv.org/abs/2410.05160)）→ MMEB-V2（78 任务，加视觉文档/视频，[arXiv 2507.04590](https://arxiv.org/abs/2507.04590)）→ MMEB-V3（omni，含音频/agent，见 [VLM2Vec repo](https://github.com/TIGER-AI-Lab/VLM2Vec)）；M-BEIR（UniIR 提出，MM-Embed 的主战场，[arXiv 2411.02571](https://arxiv.org/abs/2411.02571)）。注意：MMTEB 通常指**文本**多语言基准（Qwen3-VL-Embedding card 里的 MMTEB 即文本侧成绩），多模态侧看 MMEB 系。

**MMEB overall（单向量，跨四元任务）**[来源：VLM2Vec 论文表2](https://arxiv.org/html/2410.05160v2)：

| 模型 | Overall |
|---|---|
| CLIP（双编码器） | 37.8 |
| E5-V（MLLM 零训练式） | 13.3 |
| UniIR（CLIP 分数融合） | 42.8 |
| VLM2Vec LoRA（Phi-3.5-V） | **60.1** |
| VLM2Vec v1 Qwen2VL 版 | 60.1 / 65.8（[GitHub](https://github.com/TIGER-AI-Lab/VLM2Vec)） |
| Qwen3-VL-Embedding-2B / 8B（MMEB-V2, 2026） | **73.2 / 77.8**（[card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)、[GitHub](https://github.com/QwenLM/Qwen3-VL-Embedding)） |

**UMRB（通用多模态检索，GME 论文表3）**[来源](https://arxiv.org/html/2412.16855v1)：GME-7B **67.44** > GME-2B 64.45 > DSE 50.04 > CLIP-SF 43.66 > E5-V 42.52。

**文档类（ViDoRe v1, nDCG@5）**[来源：ColPali 论文表2](https://arxiv.org/html/2407.01449v2)：

| 路线 | 平均 |
|---|---|
| ColPali（late-interaction, 3B） | **81.3** |
| DSE（**单向量** MLLM 2B, [card](https://huggingface.co/MrLight/dse-qwen2-2b-mrl-v1)） | **85.8** |
| 最佳 OCR 管线（Unstructured+captioning+BGE-M3） | 67.0（BM25 65.1） |
| SigLIP（页级双编码器） | 51.4（Jina-CLIP 17.7、nomic-vision 12.9） |

关键读数：(1) **OCR/文本抽取路线在视觉文档上平均落后 ~14 点**；(2) **2B 单向量 DSE 在 ViDoRe v1 已反超 3B 的 ColPali**——"单向量天花板"在 2024 末已被打破；(3) 但新一代多向量（ColQwen2/2.5）在更难的整页文档基准上重新拉开差距（下表）。

**单向量 vs 多向量（2025-10 HEAVEN 论文，4 个多页文档基准平均 R@1/R@3）**[来源：arXiv 2510.22215](https://arxiv.org/html/2510.22215v1)：

| 模型 | Avg R@1 | Avg R@3 | FLOPs/query |
|---|---|---|---|
| DSE（单向量 2B） | 60.52 | 77.83 | **0.156B** |
| ColPali（多向量） | 63.79 | 81.44 | 447.6B |
| ColQwen2 | 68.43 | 84.99 | 304.8B |
| ColQwen2.5 | **69.33** | **85.86** | 304.8B |
| HEAVEN（混合：单向量召回+多向量精排） | 69.24 | 85.23 | 0.549B（-99.82%） |

即：top-1 上多向量领先单向量约 **7–9 点**；但同论文 ViMDoc 上 **R@1 差 22.5% 而 R@200 只差 0.63%**，且单向量查询 FLOPs 少 ~2000 倍。

**自然图像 vs 文档的分裂**：MMEB（自然图像为主）上单向量 MLLM 已是绝对主流、无 late-interaction 竞争压力；差距集中在**密集文本/表格/图表页**（VisDoc：Qwen3-VL-Embedding-8B 82.4 已很强，但仍配 Reranker 精排，[GitHub](https://github.com/QwenLM/Qwen3-VL-Embedding)）。

### C2. "单向量视觉检索质量死锁"重审结论

前会话结论"单 dense 向量做视觉检索有质量天花板"需要**分场景修订**：

1. **CLIP 时代成立**：页级/文档级 51.4 vs 81.3（ColPali 论文），双编码器确有天花板（全局 pooled 向量丢失布局/细粒度文本）。
2. **MLLM 统一嵌入已显著缩小差距**：DSE 85.8 vs ColPali 81.3（ViDoRe v1，2B 单向量反超）；MMEB 上 VLM2Vec→Qwen3-VL-Embedding 60.1→77.8。2B 级单向量在文档页检索已是可用质量。
3. **多向量的残余优势在 top-1 精排**（7–9 点 R@1），且**召回层面几乎无差**（R@200 差 0.63%）。行业解法不是"上多向量索引"而是**单向量召回 + cross-encoder/生成式 reranker 精排**（Qwen3-VL-Reranker、HEAVEN 同思路）——这与 S3 Vectors 协议面兼容。
4. **场景判定**：
   - **单向量够用**：自然图像检索（图库、产品图）、文档页粗召回（配合 rerank）、与文本混合的通用语义检索。
   - **必须视觉路线（单向量或 late-interaction）**：扫描件、表格/图表密集、幻灯片——OCR 路线平均 -14 点。
   - **必须 late-interaction（或精排）**：要求 top-1 极致精度的多页复杂文档问答（HEAVEN 的 7–9 点就是这条线）。
   - **OCR/文本抽取仍是合理路线**：文本为主的文档（Wiki-SS/NQ 上 OCR+E5 47.6 ≈ DSE 46.2 top-1，[DSE 论文表1](https://arxiv.org/html/2406.11251v2)）——且能与既有 bge-m3 文本索引同空间、零新增模型。
5. **结论**：死锁论在"CLIP+单向量"组合上成立；在"MLLM+单向量(+rerank)"组合上**不成立**，单向量是 2026 年的主流工程形态，late-interaction 退居精排/特定高精度场景。

### C3. 扫描件 PDF 三条实践路线对比

| 路线 | 证据 | 优 | 劣 |
|---|---|---|---|
| a) 渲染成图→多模态嵌入（pdfium→image） | DSE：SlideVQA-open nDCG@10 **75.3** vs OCR+文本 E5 59.3；BM25 55.8（[DSE 表1](https://arxiv.org/html/2406.11251v2)）；ViDoRe 85.8 | 表格/图表/版面信息保留；单向量契合协议 | 需 MLLM 级模型（2B+）；CPU 慢；大图显存 |
| b) OCR→文本嵌入 | ColPali 论文：最佳 OCR 管线 67.0 vs 视觉 81.3；但文本为主文档上 E5≈DSE | 复用 bge-m3 文本索引与空间；CPU 快、模型零新增；文本可高亮/引用 | 视觉密集文档 -14 点；OCR 质量依赖 |
| c) ColPali 页级多向量 | ColQwen2.5 R@1 69.3 最高；HEAVEN 证明精排价值 | top-1 最高 | 协议面外（D1）；无 ANN；许可/工程成本 |

实践共识（ColPali 论文 + Bedrock KB 双证据）：**混合**——文本可抽取就走文本嵌入（便宜、可与纯文本语料同索引），图像/扫描页走视觉嵌入，密集文档高精度场景加 rerank。

### C4. Bedrock KB 多模态文档处理（官方参照）

- 官方文档：KB 摄入时"images are embedded as visual vectors while text is embedded as text vectors, enabling search across both modalities"；多模态数据推荐 **Titan Multimodal Embeddings G1 或 Cohere Embed v3**。[来源：kb-how-data](https://docs.aws.amazon.com/bedrock/latest/userguide/kb-how-data.html)。
- 两种处理路径：**Nova Multimodal Embeddings**（原生视觉/音频相似度，不做文本转换，支持图像查询）或 **Bedrock Data Automation**（音频 ASR、视频场景+转录、图像 OCR/视觉内容抽取→文本表示）。[来源：kb-multimodal](https://docs.aws.amazon.com/bedrock/latest/userguide/kb-multimodal.html)。
- metadata 官方保留键（回源约定）：`x-amz-bedrock-kb-source-uri`（源文档 S3 URI）、`x-amz-bedrock-kb-chunk-id`、`x-amz-bedrock-kb-data-source-id` 等，检索用 equals/startsWith 过滤。[来源](https://docs.aws.amazon.com/bedrock/latest/userguide/kb-test-neptune.html)（另见本仓库 repositioning 调研 §B 引用）。
- AWS 官方 `s3vectors-embed-cli`（awslabs，Apache-2.0）：**每文件单 embedding、官方明说 "Document chunking is not currently supported"**（仅 Nova 异步模式分段）；自动写 `S3VECTORS-EMBED-SRC-LOCATION` / `S3VECTORS-EMBED-SRC-CONTENT` metadata。[来源](https://github.com/awslabs/s3vectors-embed-cli)——**官方自己也是"单文件单向量+metadata 回源"形态，无 late-interaction**，可作为 vectorfs 上层设计的合法性背书。

---

## D. 对 vectorfs 的落地碰撞

### D1. 协议面：late-interaction 不可表达（确认）+ 双引擎评估

**确认**：S3 Vectors wire 面 `PutVectors` 的每个 vector 是 `{key, data:{float32:[...]}, metadata}`，index dimension 创建时固定（1–4096，本仓 design.md §4.3/§5.3 已核实）；每 key 恰一个向量 ⇒ **ColPali 形态（每 key N×128 矩阵 + 查询多向量 + MaxSim）在协议面无表达**，连查询入口都不存在。

若要做，只能 x-vf-\* 超集：`x-vf-multivector` 存储格式（Lance 原生 `list<fixed_size_list<f32,128>>` 列，LanceDB 1.29+ 已支持 multivector search，[docs](https://docs.lancedb.com/search/multivector-search)）+ 自定义 MaxSim 算子 + 独立索引生命周期 + CLI/MCP 面。

**复杂度评估：不值（v1/v2 均不建议）**。理由：(1) MaxSim 无 ANN 加速，生产形态必然退化为"单向量召回+多向量精排"两段式（HEAVEN、Qwen3-VL Reranker 路线）——那精排本身用 cross-encoder 就够，不需要多向量索引；(2) 双引擎 = 双倍 schema 演化、写入路径、查询路径、conformance；(3) 权重许可参差（ColPali/ColSmol 底座 Gemma license）；(4) 官方参照（Bedrock KB、s3vectors-embed-cli）均无此形态。**替代**：预留 reranker provider 接口（与 embedding provider 同层），文档高精度场景走"单向量召回 + rerank"。

### D2. ort/ONNX 可得性与 CPU 延迟量级

- **CLIP/SigLIP 系：成熟**。Xenova/transformers.js 生态有全套 ONNX（fp16/int8/q4/uint8，vision_model 与 text_model 分文件，[API 实证](https://huggingface.co/api/models/Xenova/siglip-base-patch16-224)）；SigLIP2 有 onnx-community gopt 版、immich-app so400m 版（Immich 生产在用）、naflex 社区版（[搜索实证](https://huggingface.co/api/models?search=siglip2&library=onnx)）。**fastembed-rs（项目文本路线已选）本身支持 ImageEmbedding**：`Qdrant/clip-ViT-B-32-vision`（512 维）+ 文本塔成对使用（[crates.io](https://crates.io/crates/fastembed)、[支持模型表](https://qdrant.github.io/fastembed/examples/Supported_Models)）——图像侧可与文本侧同一技术栈落地，仅换模型。
- **MLLM 系（Qwen2-VL 2B 底座）：可行但重**。onnx-community/Qwen2-VL-2B-Instruct 有官方转换（onnxruntime-genai 格式，[HF](https://huggingface.co/onnx-community/Qwen2-VL-2B-Instruct)）——但那是生成式导出；embedding 只需 last-hidden 前向，需自行导出（transformers→ONNX，Qwen2-VL 的动态分辨率 vision preprocessing 是主要工程量）。GGUF：llama.cpp 已支持 Qwen2-VL（主模型+mmproj 视觉编码器分文件，[社区记录](https://www.reddit.com/r/LocalLLaMA/comments/1he4ars/llamacpp_now_supports_qwen2vl)、[教程](https://dev.to/mrzaizai2k/run-qwen2-vl-on-cpu-using-gguf-model-llamacpp-bli)）；jina-embeddings-v4 有官方 GGUF repo；Qwen3-VL-Embedding-2B 有 28 个社区量化 + WebGPU in-browser 实测（[card](https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B)）。llama.cpp `--embedding` 模式取 last token pooling 与 MLLM 嵌合法一致，但 vision 前向走 mmproj、需自证一致性。
- **CPU 延迟量级（估算，非实测）**：SigLIP2 base @384 ≈ 0.1–0.3s/图；so400m @384 ≈ 0.3–1s/图（百 GFLOPs 级）；2B MLLM 单图 prefill ≈ 3–6 TFLOPs ⇒ 8 核 CPU（数十 GFLOPS 有效算力）**每图十秒到分钟级**，GPU 毫秒级。结论：**双编码器可做 CPU 默认；MLLM 统一嵌入定位为 GPU 用户/远程 provider**。

### D3. Rust 图像预处理成熟度

- `image` crate 0.25.10（2026-09-17 发版，活跃维护，MIT/Apache-2.0 双许可）：PNG/JPEG/WebP/GIF/TIFF/AVIF 解码 + `imageops::resize`（含 Lanczos3 等 filter）+ DynamicImage 统一缓冲。[来源：docs.rs](https://docs.rs/image/latest/image/)。
- CLIP 式预处理（resize→center-crop→RGB→per-channel normalize）为手写矩阵运算（配 ndarray），无生态障碍；SigLIP2 **naflex**（原生宽高比 + smart resize + padding）需按 transformers 语义自行实现（约百行，非阻塞项）。pdfium 渲染 PDF 页：`pdfium-render` crate 成熟（文本路线调研已覆盖 PDF 解析侧）。**判断：无技术风险**。

### D4. 混合语料索引策略（数据模型建议）

事实约束：不同模型 = 不同向量空间，**即使维度凑齐也不能同 index 混存**（cosine 空间不可比）；S3 Vectors 一个 index 一个固定 dimension/metric。跨 index 查询在 AWS 面不存在——**必须应用层 fan-out 合并，或走 x-vf 超集 op**（与已规划的 M2 hybrid/RRF 超集是同一个机制，正好复用）。

建议数据模型：

```
vector bucket: vf-{user-bucket}
  index: txt-bge-m3        # dimension=1024, cosine；文本 chunk
  index: img-siglip2       # dimension=1152, cosine；图像/页图
  （可选）index: doc-qwen3vl # dimension=2048/1024(MRL)，GPU 用户
```

- 每 index 的 metadata 统一回源键（对齐 AWS 保留键风格，与 s3vectors-embed-cli 的 SRC-LOCATION 同构）：
  - `x-vf-src-uri`（源文件 URI/路径）、`x-vf-src-page`（PDF 页码）、`x-vf-src-bbox`（图内区域，可选）
  - `x-vf-modality`（text/image/page）、`x-vf-embed-model`、`x-vf-embed-dim`、`x-vf-content-hash`
- 查询入口：`vf query "..."` 默认双路（文本 query 同时 embed 成 bge-m3 查 txt-* 与 SigLIP2 查 img-*）+ RRF 合并——即 M2 的 x-vf hybrid op 首个用例；图像 query（以图搜图）走 img-* 单路。
- 同一源文件的文本 chunk 与页图在两个 index 用相同 `x-vf-src-uri` 关联，回源时合并。
- **替代形态**（值得列为 v2 选项）：全面切换到 Qwen3-VL-Embedding-2B 单模型——文本/图像/文档页/视频同一空间同一 index（MRL 可选维度），混合索引问题消失；代价是 CPU 不可行、必须 GPU/远程。此时 provider trait 设计应在第一天就抽象 `embed_query(MultimodalInput) -> Vec<f32>`（MultimodalInput = Text|Image|Mixed），避免文本/图像两套 trait。

### D5. 模型分发：体积预算与许可红线

- **内置默认必须 permissive**：SigLIP2 全系 Apache-2.0（38 ckpt 实证）✅；jina-clip-v1 Apache-2.0 ✅；nomic-embed-vision-v1.5 Apache-2.0（重授后）✅；GME/Qwen3-VL-Embedding Apache-2.0 ✅但 4GB+ 不适合内置。
- **不能内置（CC-BY-NC / 研究许可）**：jina-clip-v2（CC-BY-NC-4.0）、jina-embeddings-v4（**Qwen Research License**，非 CC-BY-NC 但同样限商用）、MM-Embed（CC-BY-NC-4.0）。可作用户自配 provider。
- **许可模糊**：Chinese-CLIP 权重无 license 标注（代码 MIT）——不内置，文档标注"用户自担"；E5-V HF 无 tag（GitHub Apache-2.0）。
- 体积预算参考（fp16）：SigLIP2-base ~170MB、so400m ~800MB、jina-clip-v1 ~450MB；int8 量化再减半。与文本路线 bge-m3（fp16 ~1.1GB）同量级，分发机制可复用（模型下载缓存目录）。

### D6. 推荐初判（四维打分：质量/成本/协议契合/license，1–5）

| 路线 | 质量 | 成本(CPU 推理+体积) | 协议契合 | license | 定位 |
|---|---|---|---|---|---|
| 双编码器 SigLIP2-so400m（+ bge-m3 文本） | 3.5（自然图好、文档弱） | **4.5**（<1s/图, ~800MB） | **5** | **5** | **v1 图像侧默认** |
| 双编码器 jina-clip-v1 | 3（英文好） | 4.5 | 5 | 5 | 英文场景备选/次默认 |
| Chinese-CLIP ViT-H | 3.5（中文专门） | 4 | 5 | **2**（无标注） | 用户自配（中文） |
| nomic-embed-vision-v1.5 | 3 | 4.5 | 5 | 5 | 仅当文本栈=nomic |
| MLLM 统一嵌入 Qwen3-VL-Embedding-2B | **4.5**（MMEB-V2 73.2, VisDoc 79.2） | **2**（CPU 十秒级/图, 4.3GB） | 4（单向量+MRL，instruction 需约定） | 5 | **v2 目标 / GPU provider** |
| GME-Qwen2-VL-2B | 4（UMRB 64.45, 中文强） | 2 | 4 | 5 | 同上备选（中文更稳） |
| DSE-Qwen2-2B-MRL | 4（文档专精 ViDoRe 85.8） | 2 | 4 | 5 | 扫描件 PDF 专用 provider |
| ColPali/ColQwen2 late-interaction | 5（top-1 最高） | 1.5 | **1**（协议面外） | 3（ColQwen2 底座 Apache） | **不做**（用 rerank 替代） |

**结论建议**：
1. **v1：双编码器起步**——文本 bge-m3 + 图像 SigLIP2（英文/通用）或 jina-clip-v1；中文密集场景文档化 Chinese-CLIP 自配。fastembed-rs 的 ImageEmbedding 已验证技术栈，CPU 友好，license 全绿。OCR-文本路线保留（扫描件先 OCR 入 txt index，作为无 GPU 时的降级路径——文本密集文档上质量不输视觉路线）。
2. **v1.x：加 reranker provider**（Qwen3-VL-Reranker-2B 或文本 cross-encoder），对齐"单向量召回+精排"的行业共识形态，替代 late-interaction。
3. **v2：评估 Qwen3-VL-Embedding-2B 统一嵌入**作为 GPU/远程 provider 的默认多模态模型——单空间单索引消灭混合索引复杂度，MRL 64–2048 灵活，Apache-2.0；provider trait 从第一天就按多模态输入设计。
4. **ColPali 不做**：协议面不可表达是硬约束，双引擎复杂度与收益不成比例，官方参照（Bedrock KB、embed-cli）亦无此形态；精排需求由 reranker 承接。

---

## 附：本文引用的一手来源索引

arXiv：CLIP [2103.00020](https://arxiv.org/abs/2103.00020)、SigLIP [2303.15343](https://arxiv.org/abs/2303.15343)、SigLIP2 [2502.14786](https://arxiv.org/abs/2502.14786)、EVA-CLIP [2402.04252](https://arxiv.org/abs/2402.04252)、Chinese-CLIP [2211.01335](https://arxiv.org/abs/2211.01335)、E5-V [2407.12580](https://arxiv.org/abs/2407.12580)、VLM2Vec [2410.05160](https://arxiv.org/abs/2410.05160)、VLM2Vec-V2 [2507.04590](https://arxiv.org/abs/2507.04590)、GME [2412.16855](https://arxiv.org/abs/2412.16855)、DSE [2406.11251](https://arxiv.org/abs/2406.11251)、MM-Embed [2411.02571](https://arxiv.org/abs/2411.02571)、UniME [2504.17432](https://arxiv.org/abs/2504.17432)、UniME-V2 [2510.13515](https://arxiv.org/abs/2510.13515)、ColPali [2407.01449](https://arxiv.org/abs/2407.01449)、jina-clip 技术报告 [2405.20204](https://arxiv.org/abs/2405.20204)、jina-clip-v2 [2412.08802](https://arxiv.org/html/2412.08802v1)、nomic-vision [2406.18587](https://arxiv.org/html/2406.18587v1)、HEAVEN [2510.22215](https://arxiv.org/html/2510.22215v1)、Qwen3-VL-Embedding [2601.04720](https://arxiv.org/abs/2601.04720)。
HF model card/API：见正文各条目。官方文档/博客：Cohere、Voyage、AWS（Bedrock KB/Nova/Titan）、TwelveLabs、OpenAI、Qwen GitHub、LanceDB docs。
