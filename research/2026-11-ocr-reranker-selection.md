# 专项调研：M3 尾部两项待裁决——pdf-ocr 的 OCR 引擎选型 + Reranker 默认模型选型

- 日期：2026-11-06
- 背景：M3 执行序已到 T8+（design.md §0「M3 开工与执行序」裁决行）：`$textMatch`+tantivy / reranker / merge weighted / IVF_PQ 与 pdf-ocr。pdf-ocr 与 Reranker 均需先裁决选型再动代码（§7 M3 范围：「pdf-ocr + scanned-docs preset（OCR 引擎选型补 §5.10.2）」「Reranker」）。本文件是备用户裁决的决策文档。
- 调研纪律：与 [2026-11-multimodal-embedding.md](2026-11-multimodal-embedding.md) 同——所有论断回到一手来源（HF API license 元数据 / model card 原文 / GitHub 仓库与官方文档）；license 以 HF model tag / 官方仓库为准；标注 [来源](url)；标注"估算"的为推算值，非实测。
- 项目硬前提（选型判据，已对 design.md / crates 逐一核实）：
  1. **服务端推理全走 ort（=2.0.0-rc.15，load-dynamic）**：libonnxruntime.so 由 scripts/fetch_runtime_libs.sh pin+sha（ORT 1.31.0）；理想候选 = ONNX 模型跑既有 ort session 机制（embed/ort.rs 的 VF_ORT_PATH > runtime-libs/ > system 解析链 + spawn_blocking 纪律），零新 .so；任何新原生依赖（C/C++/Python 链接）或**第二份 ort 副本**是强减分（crates/server/Cargo.toml 对 pdf-inspector 可选特性 "ocr/render-pdfium/ocr-oar pulling ort + oar-ocr … python pulling pyo3" 的拒绝注释是既有先例）。
  2. **许可红线**：内置默认仅 permissive（Apache-2.0/MIT 等）；CC-BY-NC/无标注许可只能用户自配（deny.toml 白名单 + models.rs 红线表）。
  3. **中文质量一等公民**。
  4. **CPU 推理可行**（无 GPU 假设）；模型体积与 gte 1.2GB / siglip2 1.5GB 同量级或更小可接受。
  5. **Reranker 契约**（design.md §5.10.2 Reranker 行 + contract 断言）：cross-encoder `query × candidates → scored`；分数全序确定、tie-break by key；candidates 顺序不影响重排结果。

---

## 1. pdf-ocr：OCR 引擎选型

### 1.1 候选事实表

| 候选 | 许可 | 形态（模型/运行时） | 体积 | 中文质量证据 | CPU 延迟 | 与 ort 机制兼容性 |
|---|---|---|---|---|---|---|
| **PaddleOCR PP-OCR 系 ONNX（经我们自己的 ort）** | 仓库 Apache-2.0（[GitHub API 核实](https://api.github.com/repos/PaddlePaddle/PaddleOCR)，92.4k stars）；**HF PaddlePaddle 官方组织下 PP-OCRv3/v4/v5/v6 全部模型 repo 均 `license:apache-2.0`**（[API 搜索](https://huggingface.co/api/models?author=PaddlePaddle&search=PP-OCR&limit=50)，50 repo 逐一带 tag） | det（DB 系）+ rec（SVTR CTC 系）双 ONNX 模型；**官方直接发布 ONNX 仓库**（PP-OCRv5 mobile/server det+rec、13 语言 rec、PP-OCRv6 tiny/small/medium det+rec，均含 `inference.onnx`，[API 核实](https://huggingface.co/api/models?author=PaddlePaddle&search=onnx&limit=60)） | v5 mobile：det 4.60MB + rec 15.77MB（≈20MB 整管线）；v5 server：84.03 + 80.59MB；v6 small：9.42 + 20.18MB；v6 tiny：1.70 + 4.26MB（[API blobs 实测](https://huggingface.co/api/models/PaddlePaddle/PP-OCRv5_mobile_det_onnx?blobs=true)）——比 embedder 预算小 1–2 个量级 | **简中/繁中/英/日/拼音单 rec 模型**；printed Chinese：v5 server det Hmean **0.945**（v4 0.888）、rec **0.9013**（v4 0.8486）、端到端 +13pp（官方文档）；v6 medium det **95.1** / rec **91.5**，50 语言单 rec 模型。[来源：PP-OCRv5 文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv5/PP-OCRv5.html)、[PP-OCRv6 文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv6/PP-OCRv6.html) | **官方 ONNX Runtime CPU 实测**（Xeon 8350C，s/页）：v6 tiny **0.22** / small **0.61** / medium **3.31**（[v6 文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv6/PP-OCRv6.html)）；v5 mobile 1.75 / server 4.34（Paddle 后端，Intel 6271C，[v5 文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv5/PP-OCRv5.html)） | **满分**：纯 ONNX 文件走既有 ort session；模型可进 models.rs 预取表（HF repo + sha256 pin）；零新 .so、零新 Rust 依赖 |
| **onnxtr** | Apache-2.0（[GitHub](https://github.com/felixdittrich92/OnnxTR)） | **任务先验修正：是 Python 库不是 Rust 库**（"Python 3.11 (or higher) and pip are required"，WebFetch README 核实）；且模型是 **docTR 系**（DBNet/LinkNet/FAST det + CRNN/SAR/PARSeq rec），**不是 PaddleOCR 系** | docTR 系模型 | docTR 系英文为主，中文无专门模型 | — | **出局**：引入 Python 链接正是 Cargo.toml 既有先例明确拒绝的形态（pyo3 被禁）；先验的两个关键事实（Rust、PaddleOCR）均不成立 |
| **tesseract** | 引擎 Apache-2.0（[GitHub API](https://api.github.com/repos/tesseract-ocr/tesseract)）；traineddata 亦 Apache-2.0（[tessdata](https://api.github.com/repos/tesseract-ocr/tessdata)、[tessdata_best](https://api.github.com/repos/tesseract-ocr/tessdata_best)） | C++ 库：libtesseract + libleptonica；Rust 接法两条：`rusty-tesseract`（**CLI 子进程包装，非 FFI**——README 自述 "Brings all relevant command-line tesseract functionality to Rust"、"Partly based on the Python wrapper for tesseract"（即 pytesseract 形态），依赖节要求系统安装 Tesseract，[README 核实](https://github.com/thomasgruebl/rusty-tesseract)；MIT）或 FFI 系 `tesseract` 0.15.2 / `leptess` 0.14.0（后者 2023-02 起未更新，[crates.io](https://crates.io/api/v1/crates/leptess)） | 引擎+数据 ~50MB 级 | chi_sim traineddata 存在但**无同基准一手对比数字**（社区信号一致偏弱，Reddit/工具链共识非一手，不采为论据）；docling 的语言映射把中文场景留给 RapidOCR（见 1.2） | 纯 CPU，量级与 PP-OCR 相当或更慢 | **双输**：FFI = 新增一对未 pin 的系统 .so（违反 §5.10.7 .so 白名单纪律）；CLI = 子进程部署负担（无法 sha pin、镜像内 apt 依赖） |
| **ocrs**（Rust 纯推理库） | Apache-2.0/MIT 双许可（[GitHub](https://github.com/robertknight/ocrs)） | 自研 **rten** 运行时（非 ort；纯 Rust 无 .so 但是**第二套 ML 推理引擎 + 私有模型格式**）；0.13.1 活跃（2026-09，54 万下载，[crates.io](https://crates.io/api/v1/crates/ocrs)） | 模型 ~20MB 级（rten 格式） | **自述"currently recognizes the Latin alphabet only (eg. English)"**（README 原文核实）——中文直接出局 | 快 | 中文不满足（一票否决）；且 rten 与 ort 并存 = 双推理引擎 |
| **oar-ocr**（ocrs 后继生态） | Apache-2.0（[GitHub](https://github.com/greatv/oar-ocr)）；0.10.2 活跃（2026-10，34 万下载） | **PaddleOCR 系（PP-OCRv4/v5/v6 det + SVTR rec + 布局/表格）via ort** ✓；另带 `oar-ocr-vl`（Candle 跑 VLM，独立重件） | 与 PP-OCR 同源模型（见上行） | 与 PP-OCR 同源（简中/繁中等 12+ 语言 rec，README 核实） | 与 PP-OCR via ort 同源 | **两个硬伤**：① **`ort =2.0.0-rc.13` 精确锁**（[crates.io deps 核实](https://crates.io/api/v1/crates/oar-ocr/0.10.2/dependencies)）与我们 `=2.0.0-rc.15` 冲突 → 依赖树双 ort 副本（ort-sys 各一份、各自 dist.tsv 配对要求）；② 模型从 ModelScope 自动下载（绕开我们 HF 预取 + sha256 纪律），依赖树重（hayro/fontdb/zip/clap…）。可作**实现参考**（其 det/rec 后处理代码 Apache-2.0） |

### 1.2 竞品佐证行

| 竞品 | OCR 引擎 | 来源 |
|---|---|---|
| Apache Tika | tesseract **CLI 子进程**（"`TesseractOCRParser`, which runs the `tesseract` command-line program in a separate process"；Tess4J 仅作备选 classpath 引擎） | [官方 parser 配置文档](https://raw.githubusercontent.com/apache/tika/main/docs/modules/ROOT/pages/configuration/parsers/tesseract-ocr-parser.adoc) |
| unstructured | Tesseract（`ocr_only` 策略 "runs the document through Tesseract"；要求系统装 tesseract + tessdata） | [官方文档](https://docs.unstructured.io/open-source/core-functionality/partitioning#ocr) |
| docling（IBM） | 多引擎：**RapidOCR**（= PP-OCR 模型 ONNX via onnxruntime；空 `lang` 时默认映射 **Simplified Chinese**）+ EasyOCR + tesseract-CLI/tesserocr + ocrmac + 远端 KServe | [官方 OCR 概念页](https://docling-project.github.io/docling/concepts/OCR/) |
| RapidOCR（RapidAI，8.5k stars） | "innovatively converted the models in PaddleOCR into the highly compatible ONNX format"，`pip install rapidocr onnxruntime`，"bundled and hosted OCR model files are derived from official PaddleOCR models"（附 MODEL_LICENSES.md） | [GitHub README](https://github.com/RapidAI/RapidOCR) |
| xberg（原 Kreuzberg，9.6k stars，MIT） | **Rust 侧直接先例**：`xberg-paddle-ocr` crate 用 ort 跑 PP-OCR ONNX（README："Tesseract, PaddleOCR, Candle, or VLM backends"）；其 ort 锁 `=2.0.0-rc.12` 同样与我们的 rc.15 冲突 | [GitHub](https://github.com/xberg-io/xberg)、[crates.io deps](https://crates.io/api/v1/crates/xberg-paddle-ocr/1.3.10/dependencies) |

读法：Tika/unstructured 是 tesseract 的存量基本盘（历史包袱）；新一代（docling/RapidOCR/xberg）全数把中文场景押在 **PP-OCR 模型 + ONNX Runtime** 上——与我们 ort 路线同构。

### 1.3 推荐 / 次选 / 排除

**推荐：PaddleOCR PP-OCR 官方 ONNX（det+rec 两模型）直接跑我们的 ort session，pdf-ocr extractor 内自实现 det→rec 编排。**

为什么它在项目约束下赢（不只是质量排名）：

1. **唯一同时满足全部四个硬前提的候选**：纯 ONNX（零新 .so、零新 Rust 依赖，复用 embed/ort.rs session 机制与 fetch_runtime_libs.sh 的 ORT 1.31）+ 官方 Apache-2.0 全链（仓库/模型/ONNX 导出三者都有一手核实）+ 中文系最强梯队（简繁英日单模型、printed Chinese 0.90–0.95）+ 体积 ≈20MB（比预算小两个量级，预取负担可忽略）。
2. **官方 ONNX 导出消除了最大工程疑点**：PP-OCRv5/v6 的 `*_onnx` 仓库由 PaddlePaddle 官方组织发布、逐文件可 sha256 pin 进 models.rs（RapidOCR/oar/xberg 证明这条路在 Python/Rust 两侧都走通了）。
3. **CPU 延迟有一手 ONNX Runtime 数字**：v6 small 0.61s/页（Xeon 8350C）——扫描件摄取是后台批任务（§5.10.5 CPU 信号量门），秒级/页可接受；要更快有 tiny 0.22s，要更准有 medium 3.31s，档位齐全。
4. **排除项全是"约束出局"而非"质量出局"**，见下。

**默认档位建议：PP-OCRv5 mobile（det 4.60MB + rec 15.77MB）**。理由：v5 自 2025-05-20（PaddleOCR 3.0.0，[GitHub releases](https://api.github.com/repos/PaddlePaddle/PaddleOCR/releases)）起已有一年半生态回验（RapidOCR/docling-rapidocr/oar/xberg 在用），中文五类文字（简中/繁中/英/日/拼音）单模型；**次选/升级路径：PP-OCRv6 small**（det 9.42 + rec 20.18MB）——同机对比数字全面更优（det Hmean 84.1 vs v5 mobile 75.2；v6 文档自报同机（8350C/OpenVINO）v6 small 0.59s vs v5 mobile 0.78s，ORT 档 0.61s）且 50 语言单模型，但 2026-06 才发布（[HF repo createdAt](https://huggingface.co/api/models/PaddlePaddle/PP-OCRv6_small_det_onnx)），建议作为配置档位观察一个周期后升默认。server 档（det+rec ≈165MB）作文档化配置项，不内置预取。

**排除项与理由**：

- **tesseract**：两种接法各踩一条红线——FFI 引未 pin 的系统 .so 对（违反 §5.10.7 白名单 + fetch 脚本纪律）；CLI 子进程把部署依赖外溢到镜像/宿主（apt 包，无法 sha）。中文质量另有无同基准数字的减分（不作为主论据）。
- **ocrs**：README 自述仅支持拉丁字母——中文一等公民一票否决；rten 是第二套推理引擎。
- **onnxtr**：先验修正——Python 库 + docTR 系模型，两处关键事实均不成立；Python 链接是 Cargo.toml 注释里点名拒绝的形态。
- **oar-ocr**：方向最接近（PP-OCR via ort）但不可作依赖收编：ort `=2.0.0-rc.13` 精确锁与我们 `=2.0.0-rc.15` 构成双 ort 副本；模型走 ModelScope 自动下载绕开预取纪律。**建议的相处方式：作 det/rec 后处理（DB 后处理/unclip/CTC 解码）的 Apache-2.0 参考实现**，代码自研进 kind 模块（DIP 边界内）。

### 1.4 落地注意（pdf-ocr kind）

- **两段管线形态**：pdfium 渲染（M3-T3 pdf-pages 已交付的 render 路径复用）→ det（resize_long 960 + ImageNet 归一化 + **DBPostProcess：thresh 0.3 / box_thresh 0.6 / unclip_ratio 1.5 / max_candidates 1000**）→ crop（多边形外接/透视）→ rec（**3×48×W 动态宽**，W 到 3200；CTC 解码，字典在 HF 仓库 `inference.yml` 内置 character_dict，可随预取落盘）。预处理参数一手来源：[det inference.yml](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_det_onnx/raw/main/inference.yml)、[rec inference.yml](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_rec_onnx/raw/main/inference.yml)。
- **cls（方向分类）模型默认不装**：PP-OCRv5 官方基准明示"Document orientation classification, image correction, and text line orientation classification are not used"（[v5 文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv5/PP-OCRv5.html)）——det+rec 两模型起步，旋转文档场景作后续配置项。
- **模型目录布局扩展**：一个 kind 需两个（det/rec）onnx + 字典 yml——`BUILTIN_MODELS` 的 `ModelFile` 表加行即可，预取/原子落盘/sha256 机制零改动。
- **opset 兼容待 smoke 验证**（存疑标注）：paddle2onnx 导出常用 opset 11–18，ORT 1.31 支持至 opset 23，预期无障碍，但落地时用真模型跑一次 session smoke（同 embedder dim 探测纪律）。
- **内存 guard**：det 动态形状上限 1×3×4000×4000（inference.yml），resize_long 960 已实际约束输入；rec 侧按行批处理。摄取侧已有放大门（§5.10.3）。
- **affinity 声明**：pdf-ocr 对 `scan-dominant`/`mixed-text-scan` 声明 Preferred，`text-native` Refuses（OCR 不与 pdf-text 抢文本页）——挂进既有 probe→affinity→route 机制，`scanned-docs` preset 物化链 `[pdf-ocr]` 或 `[pdf-text, pdf-ocr]`。

---

## 2. Reranker 默认模型选型

### 2.1 候选事实表

许可/参数/体积/ONNX 文件均 HF API 一手核实（2026-11-06）。

| 模型 | 许可（HF tag） | 参数量 | 体积 | ONNX 可得性 | 中文质量证据（基准数字优先） | CPU 延迟 | 与 ort 机制兼容性 |
|---|---|---|---|---|---|---|---|
| **bge-reranker-v2-m3**（BAAI） | **apache-2.0** | 568M（API safetensors） | fp32 2165.9MB | [onnx-community 官方系转换](https://huggingface.co/api/models/onnx-community/bge-reranker-v2-m3-ONNX?blobs=true) 全变体：fp16 1083.6MB / **int8 544.3MB** / q4f16 669.6MB（86.7k 下载） | CMTEB-R **72.16**（Qwen 官方表）；C-Pack **81.83**（mxbai 官方博客，BM25 一阶段）；MIRACL 72.6 / MLDR 66.8（mGTE 论文表5） | 512tok×100 候选 ≈ 58 TFLOPs → 8 核数十秒量级（估算） | **满分**：XLM-R-large cross-encoder（`XLMRobertaForSequenceClassification`，单 logit），input_ids+mask → [B,1]，ctx 8192（config max_pos 8194 核实） |
| **gte-multilingual-reranker-base**（Alibaba） | **apache-2.0** | 306M（API safetensors） | fp16 583.6MB | [onnx-community 转换](https://huggingface.co/api/models/onnx-community/gte-multilingual-reranker-base?blobs=true) 全变体：fp16 600.0MB / **int8 325.1MB** / fp32 1199.5MB | CMTEB-R **74.08**（同表**第一**，且 MTEB-R 59.51 / MMTEB-R 59.44 / MLDR 66.33 四列全胜 bge-v2-m3）；mGTE 论文表5：Avg **67.4** vs bge-v2-m3 65.7、MLDR **78.7** vs 66.8（长文档优势），MIRACL 68.5 弱于 bge 72.6 | 同基准 ≈ 31 TFLOPs，约为 bge-v2-m3 的 54%（估算） | **满分**：`NewForSequenceClassification` cross-encoder（`logits.view(-1)`，[card](https://huggingface.co/Alibaba-NLP/gte-multilingual-reranker-base)）；**与默认文本 embedder gte-multilingual-base 同底座**（两 config 一手比对：均 12 层×768、vocab 250048、RoPE）；tokenizer 同源但**非字节相同**（tokenizer.json 17,082,756 vs 17,083,021 字节，差 265B——added_tokens 层面微差，API blobs 核实）；ctx 8192 |
| **Qwen3-Reranker-0.6B**（Qwen） | **apache-2.0** | 596M（API safetensors，BF16） | bf16 1136.4MB | [onnx-community 转换](https://huggingface.co/api/models/onnx-community/Qwen3-Reranker-0.6B-ONNX?blobs=true)**仅量化变体**：q4 949.1MB / quantized 1162.9MB（无 fp32/fp16 干净导出） | MTEB-R **65.80**（同表英文第一）、CMTEB-R 71.31、MMTEB-R **66.36**（[card](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B)）；4B 档 CMTEB-R 75.94 但 CPU 不可行 | 同上量级（估算） | **可行但形态最偏**：`Qwen3ForCausalLM` decoder-only——需 chat 模板拼 prompt + 取 "yes"/"no" token logit（词表 151,669），单次前向输出全词表；与 cross-encoder 契约形态有额外实现距离 |
| **mxbai-rerank-base-v2**（mixedbread） | **apache-2.0** | 494M（Qwen2 底座） | fp16 942.3MB | **无官方 ONNX**（HF 搜索仅自家 v1 系带 onnx tag，v2 无；自导出违反"pin canonical artifact"纪律） | C-Pack **83.70** vs bge 81.83（[自家博客](https://mixedbread.com/blog/mxbai-rerank-v2)，BM25 一阶段）；但 Mr.TyDi 28.56 **低于** bge-v2-m3 30.99（同博客）；A100 0.67s（GPU，非 CPU） | 未知（无 CPU 一手） | 同 decoder-only logit 形态 + 无官方 ONNX → 双减分 |
| mxbai-rerank-xsmall-v1 | apache-2.0 | 70.8M | fp16 135.1MB | **官方 repo 内 onnx**（fp32 271MB / int8 83.2MB，[API](https://huggingface.co/api/models/mixedbread-ai/mxbai-rerank-xsmall-v1?blobs=true)） | v1 系英文为主：同表 C-Pack 仅 large-v1 72.53（vs v2 83.70） | 最快（估算 5–10×） | 满分（cross-encoder）但中文/多语不满足 |
| **bge-reranker-base / large**（BAAI） | **mit** | 278M / 560M | fp32 1060.7 / 2135.9MB | **官方 repo 内 onnx/model.onnx**（large 为外置数据两文件，[API blobs](https://huggingface.co/api/models/BAAI/bge-reranker-large?blobs=true)） | 2023 老系（C-Pack 产物），v2-m3 card 定位"Chinese and English 轻量档"；无同基准新数字（v2-m3 官方表未列入） | base ≈ gte 同量级、large ≈ bge-v2-m3（估算） | 满分（cross-encoder）；但 ctx 512（两 config `max_position_embeddings: 514` 核实）、被自家 v2-m3 实质取代 |
| **jina-reranker-v2-base-multilingual** | **cc-by-nc-4.0** ❌（[API tag](https://huggingface.co/api/models/jinaai/jina-reranker-v2-base-multilingual)，任务先验核实成立） | 278M | bf16 ~556MB | 官方 onnx（repo 带 onnx tag） | CMTEB-R **63.37**（同表垫底） | — | **许可红线排除内置**；质量亦无竞争力 |

中文基准口径说明：Qwen 官方表（[Qwen3-Reranker-0.6B card](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B)）四列 = MTEB-R（英文检索子集）/ CMTEB-R（**中文** MTEB 检索子集）/ MMTEB-R（多语）/ MLDR（长文档），全部基于 Qwen3-Embedding-0.6B 召回 top-100 的一致管线；mxbai 博客与 mGTE 论文是不同一阶段/不同候选池的独立管线——**跨表不同比，同表内可比**。

### 2.2 质量证据（两个独立一手来源交叉）

**来源一：Qwen 官方评测**（同管线、同候选池，[card](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B)）：

| 模型 | MTEB-R | **CMTEB-R（中文）** | MMTEB-R | MLDR |
|---|---|---|---|---|
| gte-multilingual-reranker-base（0.3B） | **59.51** | **74.08** | 59.44 | **66.33** |
| bge-reranker-v2-m3（0.6B） | 57.03 | 72.16 | 58.36 | 59.51 |
| Qwen3-Reranker-0.6B | **65.80** | 71.31 | **66.36** | 67.28 |
| jina-reranker-v2-base（0.3B） | 58.22 | 63.37 | 63.73 | 39.66 |

**来源二：mGTE 论文表5**（[arXiv 2407.19669](https://arxiv.org/html/2407.19669)，EMNLP 2024 Industry，自家族候选池）：

| 模型 | Avg | MLDR | MIRACL | MKQA | BEIR |
|---|---|---|---|---|---|
| mGTE-reranker（304M） | **67.4** | **78.7** | 68.5 | 67.2 | 55.4 |
| bge-reranker-v2-m3（568M） | 65.7 | 66.8 | **72.6** | **68.7** | 54.6 |
| jina-reranker-v2（278M） | 59.4 | 53.2 | 65.8 | **68.8** | 49.7 |

交叉结论：**gte/mGTE reranker 在两个独立评测里均以约一半参数量压过或持平 bge-reranker-v2-m3**，中文（CMTEB-R 74.08 第一）与长文档（MLDR 78.7/66.33）两项领先；诚实记录其弱项——多语检索 MIRACL 落后 bge-v2-m3（68.5 vs 72.6）。英文纯语场景 Qwen3-Reranker-0.6B 最强（MTEB-R 65.80），但中文 71.31 低于 gte 74.08。

### 2.3 竞品佐证行

| 竞品/基础设施 | reranker 形态 | 来源 |
|---|---|---|
| HF text-embeddings-inference（TEI） | "Rerankers, also called cross-encoders, are **sequence classification models with a single class**"——cross-encoder 是 `/rerank` 端点的标准形态；gte-multilingual-reranker-base 的 card 直接给出 TEI 部署命令 | [TEI 文档](https://huggingface.co/docs/text-embeddings-inference/en/quick_tour)、[gte card](https://huggingface.co/Alibaba-NLP/gte-multilingual-reranker-base) |
| 生态用量（HF 下载量 API，2026-11-06） | bge-reranker-v2-m3 14.4M/月（事实标准）；bge-reranker-base 2.5M；Qwen3-Reranker-0.6B 1.3M；gte-multilingual-reranker-base 46k（用量小但论文+官方背书） | [HF API](https://huggingface.co/api/models/BAAI/bge-reranker-v2-m3) |
| 本仓既有研究 | 多模态调研已预判"单向量召回 + cross-encoder 精排"为行业共识形态（C2 节），reranker provider 与 embedding provider 同层 | [2026-11-multimodal-embedding.md §D1](2026-11-multimodal-embedding.md) |

### 2.4 推荐 / 次选 / 排除

**推荐默认：gte-multilingual-reranker-base。**

为什么它在项目约束下赢：

1. **中文第一且有两个独立一手来源**：CMTEB-R 74.08（同管线四列全胜 bge-reranker-v2-m3）；mGTE 论文 Avg 67.4 > 65.7。中文一等公民约束下它是质量最优解，不是折中。
2. **约束面最干净的候选之一**：Apache-2.0；标准 cross-encoder（与 §5.10.2 契约 `query × candidates → scored` 完全同构，TEI 生态标准形态）；onnx-community 全变体 ONNX（fp16 600MB / int8 325MB，均在体积预算内）——与已交付 embedder 完全相同的获取与推理路径。
3. **家族一致性是独有优势**：默认文本 embedder 就是 gte-multilingual-base——**同一底座（New 架构 12×768、vocab 250048）、同源分词器（tokenizer.json 仅差 265B，见 2.1 注）、同一 ONNX 转换管道（onnx-community，本项目已在生产验证）**。tokenizer 栈复用、预处理复用、运维心智一份。306M 参数在质量第一梯队里 CPU 最省（估算 ≈ bge-v2-m3 的 54% FLOPs）。
4. ctx 8192 覆盖 chunk 全长（chunker 产物上限远低于此），无截断损失。

**次选：bge-reranker-v2-m3**——生态事实标准（14.4M 下载/月、TEI/LangChain/LlamaIndex 一等公民），多语检索 MIRACL 最强（72.6），官方系 ONNX 全变体、Apache-2.0；代价是 568M 参数（CPU ≈ 2×，fp16 1.08GB 贴预算上限）。若用户更看重生态惯性/多语而非中文极致，选它不亏。

**排除项与理由**：

- **jina-reranker-v2**：CC-BY-NC-4.0 触许可红线（先验核实成立）；CMTEB-R 63.37 同表垫底，双重出局。
- **mxbai-rerank-base-v2**：无官方 ONNX（需自导出，违反 pin canonical artifact 纪律）+ decoder-only prompt 形态离契约最远；中文优势仅在自家基准管线（BM25 一阶段）成立，Mr.TyDi 反而落后 bge。
- **Qwen3-Reranker-0.6B**：英文最强但中文 71.31 < gte 74.08；ONNX 仅量化变体（无干净 fp32/fp16）；decoder-only yes/no logit 机制引入 chat 模板 + 全词表 logit 的实现距离。**留作未来档位**（若后续上 Qwen3 统一栈，与 Qwen3-Embedding 配对的价值会重新评估）。
- **bge-reranker-base/large**：2023 老系被自家 v2-m3 实质取代，ctx 512，无同基准优势数字；MIT 许可本身合规，仅"无理由选"。

### 2.5 落地注意（Reranker kind）

- **最大长度**：config knob（默认 512–1024 截断建议），query+candidate 拼接为单序列；gte reranker 硬上限 8192（config max_position_embeddings 核实）。
- **分数语义**：输出单 logit 原始分——天然全序确定；不做 sigmoid 也可比（单调变换不改变序）；contract 断言"tie-break by key"按既有裁决落实现。
- **批形态与顺序无关性**：[B, L] 定长批（pad + attention_mask，ORT 批内各序列独立计算）；为保证"candidates 顺序不影响重排结果"的 contract 断言，建议按 key 排序后组批 + 固定批大小（消除批形状差异引入的数值抖动面），实现时用 contract test 钉住。
- **并发纪律**：与 embedder 同——tokenize+session.run 进 spawn_blocking，摄取/查询侧 CPU 信号量门（embed/ort.rs 两层阻塞纪律直接复用）。
- **机制形态**：一条 query × N 候选 = N 对前向（top-k 精排，k 建议默认 50–100）；`rerank` 作为查询链可选段（preset/查询选项挂载），非摄取期组件。
- **模型预取**：fp16 600MB（默认档，对齐 embedder "fp32 canonical" 先例可议 fp32 1199.5MB——体积预算内两者皆可，裁决时定）；进 `BUILTIN_MODELS` 表 + license 审计行（models.rs 红线表格式）。

---

## 附：本文引用的一手来源索引

**OCR 侧**：PaddleOCR 仓库（[GitHub API license](https://api.github.com/repos/PaddlePaddle/PaddleOCR)）；PP-OCR 全系模型许可（[HF API PaddlePaddle org](https://huggingface.co/api/models?author=PaddlePaddle&search=PP-OCR&limit=50)）；官方 ONNX 仓库族（[HF API 搜索](https://huggingface.co/api/models?author=PaddlePaddle&search=onnx&limit=60)，含 [PP-OCRv5_mobile_det_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_det_onnx)、[PP-OCRv5_mobile_rec_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_rec_onnx)、[PP-OCRv5_server_det_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv5_server_det_onnx)、[PP-OCRv5_server_rec_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv5_server_rec_onnx)、[PP-OCRv6_small_det_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx) 等）；det/rec 预处理与后处理参数（两 `inference.yml` raw）；[PP-OCRv5 官方文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv5/PP-OCRv5.html)（精度/CPU 延迟/cls 缺省）；[PP-OCRv6 官方文档](https://www.paddleocr.ai/latest/en/version3.x/algorithm/PP-OCRv6/PP-OCRv6.html)（档位/ONNX Runtime CPU 延迟/50 语言）；PaddleOCR releases（[GitHub API](https://api.github.com/repos/PaddlePaddle/PaddleOCR/releases)：3.0.0=2025-05-20、3.7.0=2026-06-11）；[OnnxTR](https://github.com/felixdittrich92/OnnxTR)（Python/docTR 判定）；[rusty-tesseract](https://github.com/thomasgruebl/rusty-tesseract)（CLI 包装判定）、[leptess](https://crates.io/api/v1/crates/leptess)（停滞）；[ocrs](https://github.com/robertknight/ocrs)（Latin-only 引文）、[oar-ocr](https://github.com/greatv/oar-ocr)（模型族/ModelScope）+ [oar-ocr deps](https://crates.io/api/v1/crates/oar-ocr/0.10.2/dependencies)（ort rc.13 冲突）；tesseract/tessdata 许可（[GitHub API](https://api.github.com/repos/tesseract-ocr/tesseract)）；[Tika tesseract-ocr-parser 文档](https://raw.githubusercontent.com/apache/tika/main/docs/modules/ROOT/pages/configuration/parsers/tesseract-ocr-parser.adoc)；[unstructured OCR 文档](https://docs.unstructured.io/open-source/core-functionality/partitioning#ocr)；[docling OCR 概念页](https://docling-project.github.io/docling/concepts/OCR/)；[RapidOCR](https://github.com/RapidAI/RapidOCR)；[xberg](https://github.com/xberg-io/xberg) + [xberg-paddle-ocr deps](https://crates.io/api/v1/crates/xberg-paddle-ocr/1.3.10/dependencies)。

**Reranker 侧**（HF API license/params/files + model card）：[BAAI/bge-reranker-v2-m3](https://huggingface.co/api/models/BAAI/bge-reranker-v2-m3)、[BAAI/bge-reranker-base](https://huggingface.co/api/models/BAAI/bge-reranker-base?blobs=true)、[BAAI/bge-reranker-large](https://huggingface.co/api/models/BAAI/bge-reranker-large?blobs=true)、[Alibaba-NLP/gte-multilingual-reranker-base](https://huggingface.co/Alibaba-NLP/gte-multilingual-reranker-base)、[Qwen/Qwen3-Reranker-0.6B](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B)、[mixedbread-ai/mxbai-rerank-base-v2](https://huggingface.co/api/models/mixedbread-ai/mxbai-rerank-base-v2)、[mixedbread-ai/mxbai-rerank-xsmall-v1](https://huggingface.co/api/models/mixedbread-ai/mxbai-rerank-xsmall-v1?blobs=true)、[jinaai/jina-reranker-v2-base-multilingual](https://huggingface.co/api/models/jinaai/jina-reranker-v2-base-multilingual)（CC-BY-NC-4.0）；ONNX 转换：[onnx-community/bge-reranker-v2-m3-ONNX](https://huggingface.co/onnx-community/bge-reranker-v2-m3-ONNX)、[onnx-community/gte-multilingual-reranker-base](https://huggingface.co/onnx-community/gte-multilingual-reranker-base)、[onnx-community/Qwen3-Reranker-0.6B-ONNX](https://huggingface.co/onnx-community/Qwen3-Reranker-0.6B-ONNX)；基准：[Qwen3-Reranker card 评测表](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B#evaluation)、[mGTE 论文 arXiv 2407.19669](https://arxiv.org/html/2407.19669)（表5）、[mxbai-rerank-v2 博客](https://mixedbread.com/blog/mxbai-rerank-v2)（C-Pack/Mr.TyDi/延迟）；[TEI 文档](https://huggingface.co/docs/text-embeddings-inference/en/quick_tour)。
