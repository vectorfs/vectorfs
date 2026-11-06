//! Built-in model catalog + prefetch infrastructure (M2-T5, design.md
//! §5.10.4 "preset 预取模型（进度条、sha256 校验、tmp+rename 原子落盘、HF_ENDPOINT 镜像可配）").
//!
//! # What lives here
//!
//! * [`BUILTIN_MODELS`] — the built-in model table. A model reaches the table
//!   only after the §5.10.5 LICENSE RED LINE check: permissive licenses only.
//! * [`prefetch`] — download a built-in model into `models_dir/<name>/`,
//!   sha256-verified, atomically (per-file tmp+rename; directory readiness
//!   gated on a `.complete` stamp written last).
//! * [`default_models_dir`] — `<data-dir>/.vectorfs/models`: the cache stays
//!   inside the `.vectorfs/` RESERVED PREFIX, so the ingest scope scan
//!   (§5.10.4: "`.vectorfs/` 恒排除") can never mistake model files for user
//!   documents. Docker deployments materialize a vectorfs.toml whose
//!   `models_dir` points at the `/vectorfs/models` VOLUME instead (§5.10.7:
//!   models live in a volume, never in the image layer) — that is why
//!   `models_dir` is a top-level CONFIG key, not a derived constant.
//!
//! # License red line (§5.10.5) — enforced by review, documented here
//!
//! Built-in defaults are PERMISSIVE-licensed only. EXPLICITLY REJECTED for the
//! built-in table (user-self-configured `path` is not restricted, only warned
//! about by `pipelines check`):
//!
//! | model | license | verdict |
//! |---|---|---|
//! | jina-clip-v2 | CC-BY-NC-4.0 | rejected (non-commercial) |
//! | jina-embeddings-v4 | Qwen Research License | rejected (non-commercial) |
//! | MM-Embed | CC-BY-NC-4.0 | rejected (non-commercial) |
//! | Chinese-CLIP | weights unlicensed | rejected (no grant) |
//!
//! Accepted precedents: gte-multilingual-base Apache-2.0 (below),
//! bge-m3 MIT, jina-clip-v1 Apache-2.0 (future candidates — each gets its
//! own table row + license audit when its milestone lands); SigLIP2
//! Apache-2.0 LANDED as the M3-T2 siglip2 kind's model (below; the 38
//! google checkpoints are all Apache-2.0, HF API verified in
//! research/2026-11-multimodal-embedding.md §B1); PP-OCRv5 mobile
//! Apache-2.0 LANDED as the M3-T10 pdf-ocr kind's det+rec pair (below —
//! official PaddlePaddle ONNX repos, HF tags verified 2026-11-07 in
//! research/2026-11-ocr-reranker-selection.md).
//!
//! # gte-multilingual-base facts (verified 2026-11-05, step-0 research)
//!
//! * HF ONNX export: `onnx-community/gte-multilingual-base` (conversion of
//!   Apache-2.0 `Alibaba-NLP/gte-multilingual-base`; the conversion carries
//!   the base weights' license).
//! * dim = 768 (`config.json` `hidden_size`).
//! * Pooling: **CLS** — position 0 of `last_hidden_state`, then **L2
//!   normalize**. Authoritative sources agree: the base repo's
//!   `1_Pooling/config.json` (`pooling_mode_cls_token: true`) and Alibaba's
//!   own `scripts/gte_embedding.py` (`last_hidden_state[:, 0, :]` +
//!   `F.normalize`). The onnx-community README's `pooling: 'mean'` snippet is
//!   generic transformers.js boilerplate, NOT model guidance — ignored.
//! * NO retrieval query prefix: the official `gte_embedding.py` and the
//!   sentence-transformers `modules.json` (no prompt module) encode queries
//!   and documents identically. The README's "recommended query instruction"
//!   line is likewise template boilerplate.
//! * Weights file: the fp32 `onnx/model.onnx` is pinned (canonical artifact,
//!   zero quantization surprises baked into a default; 1.17 GiB). The repo
//!   also ships fp16 (599 MiB) / int8 (325 MiB) variants users may download
//!   by hand and point `path` at — self-configured, quality-informed choice.

use std::path::{Path, PathBuf};

use sha2::Digest;

use super::PipelineError;

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

/// One file of a built-in model: remote path inside the HF repo, local file
/// name under `models_dir/<name>/`, and the pinned sha256.
pub struct ModelFile {
    /// Path inside the HF repo (e.g. `onnx/model.onnx`).
    pub remote: &'static str,
    /// File name under the model's local directory (flat layout: the ort kind
    /// loads `<dir>/model.onnx` + `<dir>/tokenizer.json`).
    pub local: &'static str,
    /// Pinned sha256 (lowercase hex) — the trust root, same discipline as the
    /// .so pins (§5.10.7).
    pub sha256: &'static str,
    /// Per-file HF repo override: `None` = the row's `hf_repo`. Multi-repo
    /// models keep ONE row + one local directory (M3-T10: PP-OCRv5 det and
    /// rec are SIBLING repos — `PP-OCRv5_mobile_{det,rec}_onnx` — but the
    /// pdf-ocr kind loads one flat `det.onnx + rec.onnx + dict.yml` dir).
    pub repo: Option<&'static str>,
}

/// A built-in model entry. `dim` is the DECLARED dimension the ort kind's
/// startup probe must agree with (§5.10.2: "探测 dim ≠ 声明 ⇒ 启动失败");
/// `image_size` is the DECLARED vision-tower input resolution for image
/// models (the siglip2 kind's probe fallback + cross-check target — the
/// pinned community export keeps H/W symbolic in the graph, so the ARTIFACT
/// declares what the graph does not).
pub struct BuiltinModel {
    /// Config-facing name (`[embedder.*] model = "…"` / prefetch argument).
    pub name: &'static str,
    /// HF repo id (mirror-friendly via `HF_ENDPOINT`).
    pub hf_repo: &'static str,
    pub files: &'static [ModelFile],
    pub dim: usize,
    /// SPDX id recorded at audit time (red line above).
    pub license: &'static str,
    /// Declared vision input (height, width) — image models only (the
    /// siglip2 kind; None for text models).
    pub image_size: Option<(u32, u32)>,
}

/// The built-in model table. Adding a row = a license audit (§5.10.5 red
/// line) + verified file set; nothing else in the codebase hardcodes model
/// names.
pub static BUILTIN_MODELS: &[BuiltinModel] = &[
    BuiltinModel {
        name: "gte-multilingual-base",
        hf_repo: "onnx-community/gte-multilingual-base",
        files: &[
            ModelFile {
                remote: "onnx/model.onnx",
                local: "model.onnx",
                sha256: "5b9f03fdc40350a78fa064b4cfb6bf9a229a7c40aa87736f537e3ebd00aa2b86",
                repo: None,
            },
            ModelFile {
                remote: "tokenizer.json",
                local: "tokenizer.json",
                sha256: "3a56def25aa40facc030ea8b0b87f3688e4b3c39eb8b45d5702b3a1300fe2a20",
                repo: None,
            },
            ModelFile {
                remote: "config.json",
                local: "config.json",
                sha256: "6ef2538d4286a7cd18d05225f659d8a1bceca7adb01c186868e53dbd4f822e17",
                repo: None,
            },
        ],
        dim: 768,
        license: "Apache-2.0",
        image_size: None,
    },
    // SigLIP2 base, split-tower fp32 canonical (M3-T2). Facts verified
    // 2026-11-06 against the actual artifacts (embed/siglip2.rs module docs
    // carry the full story): split files vision_model/text_model (the FULL
    // model.onnx needs image+text inputs together — unusable for one-modality
    // runs); text input is input_ids ONLY; pooler_output IS the shared-space
    // embedding (cos 1.000000 vs the full model's embeds, which only add the
    // L2 norm); preprocessor mean=std=0.5, resample=2 (bilinear), exact
    // 224×224 resize, no crop; text context 64 (tokenizer_config max_length).
    // sha256s are the HF LFS content hashes (the same source the gte pins
    // used — API `?blobs=true` lfs.sha256 == file-content sha256, verified
    // against the gte pins at pin time).
    BuiltinModel {
        name: "siglip2-base-patch16-224",
        hf_repo: "onnx-community/siglip2-base-patch16-224-ONNX",
        files: &[
            ModelFile {
                remote: "onnx/vision_model.onnx",
                local: "vision.onnx",
                sha256: "c0573e3f4140c3a7c4e9cc5912bd6b26a033b46a6a8e8af26cbea262b163bcad",
                repo: None,
            },
            ModelFile {
                remote: "onnx/text_model.onnx",
                local: "text.onnx",
                sha256: "baf12d941beabafafb14f7b4adb38dc15be18681b964a84410ec53d9d65e6293",
                repo: None,
            },
            ModelFile {
                remote: "tokenizer.json",
                local: "tokenizer.json",
                sha256: "cb9140fae3ac5122c972d37adf83e1248471a38147ad76f8215c8872c6fd8322",
                repo: None,
            },
            ModelFile {
                remote: "config.json",
                local: "config.json",
                sha256: "e43a9f7692d3819886a82cb2097048258d444f123c67d37ec825f9345b019cf2",
                repo: None,
            },
        ],
        dim: 768,
        license: "Apache-2.0",
        // from the artifact's preprocessor_config.json size (pin-time fact)
        image_size: Some((224, 224)),
    },
    // PP-OCRv5 mobile, the pdf-ocr kind's det+rec pair (M3-T10, the §0
    // 2026-11-07 adjudication). Facts verified 2026-11-07 against the
    // actual artifacts: BOTH repos official PaddlePaddle, Apache-2.0
    // (HF tags); det 4,826,518 B + rec 16,534,782 B (4.60 + 15.77 MiB);
    // det input `x` [1,3,H,W] dynamic 32..4000, BGR + ImageNet
    // normalize, DBPostProcess thresh 0.3 / box_thresh 0.6 / unclip 1.5 /
    // max_candidates 1000 (det inference.yml); rec input `x` [b,3,48,W]
    // dynamic W 160..3200, `(v/255−0.5)/0.5` BGR, and the dictionary
    // EMBEDDED in the rec repo's inference.yml (`PostProcess.
    // character_dict`, 18,383 entries incl. 简中/繁中/英/日/拼音 — the
    // rec file's 148,345 B IS the dict file; stored here as dict.yml).
    // sha256s: LFS content hashes for the onnx pair (HF API ?blobs=true),
    // content hash for the non-LFS yml (computed at pin time). The dim
    // field is unused by an OCR model (no embedding vector); the class
    // count the rec graph must agree with is probed by the kind.
    BuiltinModel {
        name: "ppocr-v5-mobile",
        hf_repo: "PaddlePaddle/PP-OCRv5_mobile_det_onnx",
        files: &[
            ModelFile {
                remote: "inference.onnx",
                local: "det.onnx",
                sha256: "a431985659dc921974177a95adcfbb90fd9e51989a5e04d70d0b75f597b6e61d",
                repo: None,
            },
            ModelFile {
                remote: "inference.onnx",
                local: "rec.onnx",
                sha256: "da72dc72ca4dc220df0dfde68c1dedc31c58d3e76a25871122e5056227d50092",
                repo: Some("PaddlePaddle/PP-OCRv5_mobile_rec_onnx"),
            },
            ModelFile {
                remote: "inference.yml",
                local: "dict.yml",
                sha256: "5dfeb2777f6d0db8177d8128a8acfcf6e6276dc4ac73ea3bf0dc06d6a5e85d8e",
                repo: Some("PaddlePaddle/PP-OCRv5_mobile_rec_onnx"),
            },
        ],
        dim: 0,
        license: "Apache-2.0",
        image_size: None,
    },
];

/// Look up a built-in model by config name.
pub fn builtin(name: &str) -> Option<&'static BuiltinModel> {
    BUILTIN_MODELS.iter().find(|m| m.name == name)
}

/// The names of every built-in model (error text).
pub fn builtin_names() -> Vec<&'static str> {
    BUILTIN_MODELS.iter().map(|m| m.name).collect()
}

/// Default models cache: `<data-dir>/.vectorfs/models` — inside the reserved
/// `.vectorfs/` prefix so scope scans exclude it by construction (§5.10.4).
pub fn default_models_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(crate::backends::SYSTEM_PREFIX).join("models")
}

/// HF base URL: `HF_ENDPOINT` mirror override (e.g. hf-mirror.com) or the
/// official origin.
fn hf_endpoint() -> String {
    std::env::var("HF_ENDPOINT")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_string())
}

/// The stamp file marking a COMPLETE, verified model directory.
const STAMP: &str = ".complete";

/// Expected stamp contents: one `sha256  local` line per file, table order.
fn stamp_manifest(model: &BuiltinModel) -> String {
    model
        .files
        .iter()
        .map(|f| format!("{}  {}\n", f.sha256, f.local))
        .collect()
}

/// Is the model directory COMPLETE? Readiness = a `.complete` stamp whose
/// manifest matches the CURRENT table pins AND every local file present.
/// Because the stamp is written last (after every file was downloaded and
/// sha-verified through tmp+rename), its existence can never be observed for
/// a half-downloaded directory; pin bumps change the manifest string and
/// automatically invalidate older stamps.
pub fn is_ready(dir: &Path, model: &BuiltinModel) -> bool {
    let stamp_ok = std::fs::read_to_string(dir.join(STAMP))
        .map(|s| s == stamp_manifest(model))
        .unwrap_or(false);
    stamp_ok && model.files.iter().all(|f| dir.join(f.local).is_file())
}

/// sha256 of a local file (streamed; models are GiB-scale).
fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Random tmp suffix from the OS RNG (getrandom is already a direct dep).
fn tmp_suffix() -> Result<String, PipelineError> {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).map_err(|e| PipelineError::io(format!("OS rng failed: {e}")))?;
    Ok(hex(&b))
}

// ---------------------------------------------------------------------------
// prefetch
// ---------------------------------------------------------------------------

/// Prefetch a BUILT-IN model (by name) into `models_dir/<name>/`; see
/// [`prefetch_model`] for the semantics.
pub async fn prefetch(name: &str, models_dir: &Path) -> Result<PathBuf, PipelineError> {
    let model = builtin(name).ok_or_else(|| {
        PipelineError::config(
            format!(
                "unknown model '{name}' (built-in models: {}; user-supplied models go through \
                 [embedder.*] path = …, not prefetch)",
                builtin_names().join(", ")
            ),
            None,
        )
    })?;
    prefetch_model(model, models_dir).await
}

/// Prefetch one model entry into `models_dir/<name>/` and return that
/// directory. Split out of [`prefetch`] so the hermetic tests can drive the
/// FULL machinery (idempotence, stamp, tmp sweep) against an in-process fake
/// HF with test-computed pins.
///
/// * **Idempotent**: a ready directory (stamp + files, [`is_ready`]) is
///   returned without touching the network.
/// * **Atomic per file**: bytes stream into `<local>.tmp-<rand>`, sha256 is
///   verified, THEN the file is renamed into place — an interrupted download
///   leaves only a tmp leftover, never a truncated "real" file.
/// * **Atomic per model**: the `.complete` stamp is written (tmp+rename) only
///   after every file passed, so half-fetched directories are never ready.
/// * **Partial resume**: files already present with a MATCHING sha are kept
///   (re-running after a mid-model crash redownloads only the rest); a
///   mismatched existing file is re-fetched.
/// * **Progress** goes to stderr (stdout stays machine-readable): received /
///   total bytes and a percentage when Content-Length is present.
/// * **Mirror**: `HF_ENDPOINT` replaces `https://huggingface.co`.
///
/// Failures (404, sha mismatch, broken connection) delete the tmp file and
/// return Err — the directory stays not-ready and a retry starts clean.
pub async fn prefetch_model(
    model: &BuiltinModel,
    models_dir: &Path,
) -> Result<PathBuf, PipelineError> {
    let dir = models_dir.join(model.name);
    std::fs::create_dir_all(&dir)?;

    if is_ready(&dir, model) {
        eprintln!("model {}: already at {} — skip", model.name, dir.display());
        return Ok(dir);
    }

    // sweep tmp leftovers from earlier interrupted runs: every tmp path we
    // ever create contains ".tmp-" and no legitimate file does
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            if entry.file_name().to_string_lossy().contains(".tmp-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    let client = super::embed::http::client();
    let base = hf_endpoint();

    for file in model.files {
        let target = dir.join(file.local);
        if target.is_file() && matches!(sha256_file(&target), Ok(s) if s == file.sha256) {
            eprintln!(
                "model {}/{}: present and sha256-verified — keep",
                model.name, file.local
            );
            continue;
        }
        // HF blob URL shape: /<repo>/resolve/<ref>/<path>. The ref is pinned
        // `main` (the only branch the built-in table's sha256s are taken
        // against; a moving tag would detach the pin). The repo is the
        // row's, unless the file overrides it (the multi-repo OCR pair).
        let url = format!(
            "{}/{}/resolve/main/{}",
            base,
            file.repo.unwrap_or(model.hf_repo),
            file.remote
        );
        let (status, total, resp) = super::embed::http::get_following_redirects(&client, &url, 5)
            .await
            .map_err(PipelineError::io)?;
        if !status.is_success() {
            return Err(PipelineError::io(format!(
                "fetching {url} answered HTTP {status} (mirror = HF_ENDPOINT, currently {base})"
            )));
        }
        download_to(&url, total, resp, &target, file.sha256).await?;
        eprintln!(
            "model {}/{}: downloaded, sha256 verified",
            model.name, file.local
        );
    }

    // stamp LAST: existence == every file verified under the current pins
    let stamp_tmp = dir.join(format!("{STAMP}.tmp-{}", tmp_suffix()?));
    std::fs::write(&stamp_tmp, stamp_manifest(model))?;
    std::fs::rename(&stamp_tmp, dir.join(STAMP))?;
    eprintln!("model {}: ready at {}", model.name, dir.display());
    Ok(dir)
}

/// Stream one response body to `target.tmp-<rand>`, verifying sha256 before
/// the final rename. On any failure the tmp file is deleted and the target
/// name is never created (half-downloads can never masquerade as ready).
async fn download_to(
    url: &str,
    total: Option<u64>,
    resp: http::Response<super::embed::http::BoxedBody>,
    target: &Path,
    want_sha: &str,
) -> Result<(), PipelineError> {
    use http_body_util::BodyExt;
    use std::io::Write;

    let fname = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!("{fname}.tmp-{}", tmp_suffix()?));
    let mut out = std::fs::File::create(&tmp)
        .map_err(|e| PipelineError::io(format!("create {}: {e}", tmp.display())))?;
    let mut hasher = sha2::Sha256::new();
    let mut body = resp.into_body();
    let mut received: u64 = 0;
    let mut last_report: u64 = 0;
    loop {
        let frame = tokio::select! {
            f = body.frame() => f,
            // stall cap per attempt: 30 min covers a 1.2 GiB fetch at
            // ~700 KB/s; hanging forever serves nobody
            _ = tokio::time::sleep(std::time::Duration::from_secs(1800)) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(PipelineError::io(format!(
                    "downloading {url} stalled (no bytes for 30min; got {received} so far)"
                )));
            }
        };
        let Some(frame) = frame else { break };
        let bytes = frame
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                PipelineError::io(format!("connection broke mid-download of {url}: {e}"))
            })?
            .into_data()
            .map_err(|_| {
                // a non-data frame (trailers): download bodies carry none we
                // care about, but never corrupt the file over one
                let _ = std::fs::remove_file(&tmp);
                PipelineError::io(format!("unexpected non-data frame from {url}"))
            })?;
        hasher.update(&bytes);
        out.write_all(&bytes).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            PipelineError::io(format!("write {}: {e}", tmp.display()))
        })?;
        received += bytes.len() as u64;
        // progress every 1 MiB (stderr; plain text, agent-parseable)
        if received - last_report >= 1024 * 1024 {
            last_report = received;
            match total {
                Some(t) if t > 0 => eprintln!(
                    "  {}: {} / {} bytes ({:.1}%)",
                    target.display(),
                    received,
                    t,
                    received as f64 * 100.0 / t as f64
                ),
                _ => eprintln!("  {}: {} bytes", target.display(), received),
            }
        }
    }
    let got = hex(&hasher.finalize());
    if got != want_sha {
        let _ = std::fs::remove_file(&tmp);
        return Err(PipelineError::io(format!(
            "sha256 mismatch for {url}: got {got}, pinned {want_sha} — refusing to install \
             (inconsistent mirror or changed upstream; re-pin only after review)"
        )));
    }
    // durability before publish: rename after fsync is crash-atomic on POSIX
    out.sync_all().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        PipelineError::io(format!("fsync {}: {e}", tmp.display()))
    })?;
    drop(out);
    std::fs::rename(&tmp, target)
        .map_err(|e| PipelineError::io(format!("rename into {}: {e}", target.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// tests — hermetic: an in-process axum server stands in for HF (HF_ENDPOINT)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use axum::extract::Path as AxPath;
    use axum::http::StatusCode as AxStatus;
    use axum::routing::get;
    use sha2::Sha256;

    /// Deterministic bytes the fake HF serves per repo path. model.onnx is
    /// >1 MiB so the progress path (stderr) actually fires.
    pub(crate) fn fake_file(path: &str) -> Vec<u8> {
        match path {
            "onnx/model.onnx" => {
                let mut v = Vec::with_capacity(1_100_000);
                for i in 0..1_100_000u32 {
                    v.push((i % 251) as u8);
                }
                v
            }
            "tokenizer.json" => b"{\"vocab\": \"tiny\"}".to_vec(),
            "config.json" => b"{\"hidden_size\": 8}".to_vec(),
            "onnx/model-corrupted.onnx" => b"these bytes match no pin".to_vec(),
            _ => Vec::new(),
        }
    }

    fn sha_of(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex(&h.finalize())
    }

    /// Fake HF serving [fake_file] bytes at `/<repo>/resolve/<ref>/<path>`.
    /// `hits` counts download requests (idempotence assertions).
    pub(crate) async fn fake_hf() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        async fn serve_file(
            axum::extract::State(hits): axum::extract::State<
                std::sync::Arc<std::sync::atomic::AtomicUsize>,
            >,
            AxPath(path): AxPath<String>,
        ) -> (AxStatus, Vec<u8>) {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // the wire path is /<repo>/resolve/<ref>/<file> — drop the ref
            // segment and serve by <file>
            let after = path.rsplit("/resolve/").next().unwrap_or(&path);
            let file = after.split_once('/').map(|(_, f)| f).unwrap_or(after);
            (AxStatus::OK, fake_file(file))
        }
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/{*_}", get(serve_file))
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    /// A test model entry mirroring the gte file set, with pins computed from
    /// the fake bytes. `Box::leak` makes the &'static the table type demands.
    fn fake_model(repo: &'static str) -> &'static BuiltinModel {
        Box::leak(Box::new(BuiltinModel {
            name: "fake-gte",
            hf_repo: repo,
            files: Box::leak(
                [
                    ("onnx/model.onnx", "model.onnx"),
                    ("tokenizer.json", "tokenizer.json"),
                    ("config.json", "config.json"),
                ]
                .iter()
                .map(|(remote, local)| ModelFile {
                    remote,
                    local,
                    sha256: Box::leak(sha_of(&fake_file(remote)).into_boxed_str()),
                    repo: None,
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            ),
            dim: 8,
            license: "test-only",
            image_size: None,
        }))
    }

    /// env-var tests run in one process: serialize the HF_ENDPOINT flips.
    /// The guard is DELIBERATELY held across awaits — it must span the whole
    /// async test body so no parallel test observes a half-flipped env; the
    /// lock is never contended from an async context that could deadlock
    /// (only these tests take it, and each holds it exclusively).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK spans the async body on purpose (see its doc)
    async fn prefetch_downloads_verifies_and_is_idempotent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (base, hits) = fake_hf().await;
        unsafe { std::env::set_var("HF_ENDPOINT", &base) };
        let model = fake_model("vectorfs-test/fake");
        let dir = tempfile::tempdir().unwrap();

        // 1st run: everything downloads
        let out = prefetch_model(model, dir.path()).await.unwrap();
        assert_eq!(out, dir.path().join("fake-gte"));
        assert!(is_ready(&out, model), "stamped + all files");
        assert_eq!(
            std::fs::read(out.join("model.onnx")).unwrap(),
            fake_file("onnx/model.onnx")
        );
        let after_first = hits.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_first, 3, "one GET per file");

        // 2nd run: stamp short-circuits, zero requests
        prefetch_model(model, dir.path()).await.unwrap();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            after_first,
            "ready dir must not hit the network"
        );

        // no tmp residue anywhere
        for e in std::fs::read_dir(&out).unwrap().flatten() {
            assert!(
                !e.file_name().to_string_lossy().contains(".tmp-"),
                "residue: {:?}",
                e.file_name()
            );
        }

        // partial-resume: delete one file + stamp -> only that file refetches
        std::fs::remove_file(out.join("tokenizer.json")).unwrap();
        std::fs::remove_file(out.join(".complete")).unwrap();
        prefetch_model(model, dir.path()).await.unwrap();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            after_first + 1,
            "only the missing file is refetched"
        );
        assert!(is_ready(&out, model));
        unsafe { std::env::remove_var("HF_ENDPOINT") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK spans the async body on purpose (see its doc)
    async fn sha_mismatch_errs_and_leaves_nothing_ready() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (base, _hits) = fake_hf().await;
        unsafe { std::env::set_var("HF_ENDPOINT", &base) };
        // pin says "model.onnx" but repo path serves the corrupted variant
        let model: &'static BuiltinModel = Box::leak(Box::new(BuiltinModel {
            name: "badsha",
            hf_repo: "vectorfs-test/fake",
            files: Box::leak(
                vec![ModelFile {
                    remote: "onnx/model-corrupted.onnx",
                    local: "model.onnx",
                    sha256: Box::leak(sha_of(b"expected-but-different").into_boxed_str()),
                    repo: None,
                }]
                .into_boxed_slice(),
            ),
            dim: 8,
            license: "test-only",
            image_size: None,
        }));
        let dir = tempfile::tempdir().unwrap();
        let err = prefetch_model(model, dir.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sha256 mismatch"), "{msg}");
        let out = dir.path().join("badsha");
        // no final file, no stamp -> not ready; a retry starts clean
        assert!(
            !out.join("model.onnx").exists(),
            "mismatch must not install"
        );
        assert!(!out.join(".complete").exists());
        assert!(!is_ready(&out, model));
        let residue: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(residue.is_empty(), "tmp must be cleaned: {residue:?}");
        unsafe { std::env::remove_var("HF_ENDPOINT") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK spans the async body on purpose (see its doc)
    async fn interrupted_download_is_not_ready() {
        let _guard = ENV_LOCK.lock().unwrap();
        // raw server that drops the connection after a few body bytes with a
        // LARGER Content-Length advertised (the mid-stream disconnect shape)
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // consume the request head first so hyper is in response state
            let mut buf = [0u8; 4096];
            let _n = sock.read(&mut buf).await.unwrap_or(0);
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&[0u8; 10]).await.unwrap();
            sock.shutdown().await.unwrap();
        });
        unsafe { std::env::set_var("HF_ENDPOINT", format!("http://{addr}")) };
        let model: &'static BuiltinModel = Box::leak(Box::new(BuiltinModel {
            name: "broken",
            hf_repo: "vectorfs-test/fake",
            files: Box::leak(
                vec![ModelFile {
                    remote: "onnx/model.onnx",
                    local: "model.onnx",
                    sha256: "0".repeat(64).leak(),
                    repo: None,
                }]
                .into_boxed_slice(),
            ),
            dim: 8,
            license: "test-only",
            image_size: None,
        }));
        let dir = tempfile::tempdir().unwrap();
        let err = prefetch_model(model, dir.path()).await.unwrap_err();
        // whether the break surfaces at the transport or body layer, the
        // outcome contract is: Err + nothing installed + not ready
        assert!(!err.to_string().is_empty());
        let out = dir.path().join("broken");
        assert!(!out.join("model.onnx").exists());
        assert!(!is_ready(&out, model));
        unsafe { std::env::remove_var("HF_ENDPOINT") };
    }

    #[tokio::test]
    async fn unknown_model_is_a_config_error() {
        let err = prefetch("no-such-model", Path::new("/tmp/x"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown model 'no-such-model'"), "{msg}");
        assert!(msg.contains("gte-multilingual-base"), "{msg}");
    }

    #[test]
    fn builtin_table_shape() {
        let m = builtin("gte-multilingual-base").expect("default model present");
        assert_eq!(m.dim, 768);
        assert_eq!(m.license, "Apache-2.0");
        assert_eq!(m.image_size, None, "text model declares no image size");
        let locals: Vec<_> = m.files.iter().map(|f| f.local).collect();
        assert_eq!(locals, ["model.onnx", "tokenizer.json", "config.json"]);
        for f in m.files {
            assert_eq!(f.sha256.len(), 64);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        // the siglip2 row (M3-T2): split towers + tokenizer, declared dim
        // and the 224×224 the artifact's preprocessor pins
        let s = builtin("siglip2-base-patch16-224").expect("siglip2 model present");
        assert_eq!(s.dim, 768);
        assert_eq!(s.license, "Apache-2.0");
        assert_eq!(s.image_size, Some((224, 224)));
        let locals: Vec<_> = s.files.iter().map(|f| f.local).collect();
        assert_eq!(
            locals,
            ["vision.onnx", "text.onnx", "tokenizer.json", "config.json"]
        );
        for f in s.files {
            assert_eq!(f.sha256.len(), 64);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        // the ppocr row (M3-T10): the det+rec pair from TWO sibling repos
        // into one flat directory, plus the rec dict yml
        let o = builtin("ppocr-v5-mobile").expect("ppocr model present");
        assert_eq!(o.license, "Apache-2.0");
        assert_eq!(o.image_size, None);
        let rows: Vec<(&str, &str, &str)> = o
            .files
            .iter()
            .map(|f| (f.local, f.remote, f.repo.unwrap_or(o.hf_repo)))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "det.onnx",
                    "inference.onnx",
                    "PaddlePaddle/PP-OCRv5_mobile_det_onnx"
                ),
                (
                    "rec.onnx",
                    "inference.onnx",
                    "PaddlePaddle/PP-OCRv5_mobile_rec_onnx"
                ),
                (
                    "dict.yml",
                    "inference.yml",
                    "PaddlePaddle/PP-OCRv5_mobile_rec_onnx"
                ),
            ]
        );
        for f in o.files {
            assert_eq!(f.sha256.len(), 64);
            assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        // default models dir stays inside the reserved prefix
        assert_eq!(
            default_models_dir(Path::new("/data")),
            Path::new("/data/.vectorfs/models")
        );
        // stamp invalidation: manifest mismatch (pin bump) => not ready
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("x");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(STAMP), "stale\n").unwrap();
        for f in m.files {
            std::fs::write(d.join(f.local), b"x").unwrap();
        }
        assert!(!is_ready(&d, m), "stale stamp must not count as ready");
    }

    /// END-TO-END prefetch against the REAL pins + REAL network (~1.2 GiB
    /// download): #[ignore] like the ort real-model smoke; run explicitly at
    /// the T9 gate.
    #[tokio::test]
    #[ignore = "downloads the real ~1.2 GiB gte weights; run explicitly (T9 gate)"]
    async fn prefetch_real_table_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let out = prefetch("gte-multilingual-base", dir.path()).await.unwrap();
        assert!(is_ready(&out, builtin("gte-multilingual-base").unwrap()));
        // idempotent second call skips via the stamp (no re-hash of GiB files)
        let again = prefetch("gte-multilingual-base", dir.path()).await.unwrap();
        assert_eq!(out, again);
    }
}
