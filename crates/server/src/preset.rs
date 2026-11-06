//! `vectorfs init --preset` templates (M2-T8, design.md §5.10.4 "preset =
//! 内建 TOML 模板" + 附录E.4 init row).
//!
//! A preset is a static TOML template APPENDED to the generated server
//! fields (config.rs keeps owning credential generation + file writing).
//! Six tiers ship (§5.10.4): `dev` (pure protocol face, no pipeline
//! sections — byte-identical to the pre-preset init), `text-rag` (the gte
//! text pipeline; init PREFETCHES the model, ~1.17 GiB, stderr progress —
//! see pipeline::models), `air-gapped` (offline `path` embedder, NO
//! download by construction), the two M3-T4 image tiers: `multimodal`
//! (gte text + siglip2 image DUAL embedders — one bucket, text and image
//! queries both served) and `image-search` (siglip2 only, 以图搜图 over
//! the images you put), and `scanned-docs` (M3-T10: gte + the pdf chain
//! `[pdf-text, pdf-ocr]` — the probe→affinity router sends text-native
//! PDFs to the text layer and scan-dominant ones through OCR, mixed
//! documents fall to the text member; one text index either way).
//!
//! # Why text templates instead of typed structs
//!
//! The template IS the operator-facing documentation of what a pipeline
//! config looks like (§5.10.4 "一屏为限"); materializing the exact §5.10.4
//! shape keeps the written file and the design doc line-for-line
//! comparable. The bytes are immediately re-validated by
//! `pipeline::config::load_pipeline_config` inside `init` itself (and by
//! unit tests), so a template regression fails at the authoring site, not
//! at the first `serve`.
//!
//! # Parameterization
//!
//! * `--models-dir` (container path, §5.10.7): model-carrying presets
//!   (text-rag, multimodal, image-search) write it as the top-level
//!   `models_dir` key (the Docker /vectorfs/models VOLUME); air-gapped
//!   bakes it into the embedder `path`. Bare-metal init does not pass it —
//!   the presets then use the `<data-dir>/.vectorfs/models` default,
//!   air-gapped defaults to the Docker volume path because that is the
//!   deployment shape the preset exists for (the printed guidance tells
//!   bare-metal operators to edit `path`).
//! * `--listen` / `--backend` affect the REPORT TEXT only — the backend is
//!   per-invocation by design (config.rs: vectorfs.toml never carries a
//!   backend) and the listen address is a serve-time flag; neither belongs
//!   in vectorfs.toml.

use std::path::Path;

/// The preset tiers (clap ValueEnum so `--preset` accepts the kebab-case
/// spellings; `dev` is also the DEFAULT when --preset is absent, keeping
/// pre-preset `vectorfs init` invocations byte-compatible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Preset {
    /// Pure protocol face: NO pipeline sections (the pre-preset init).
    #[default]
    Dev,
    /// gte text pipeline: pdf/md/txt extractors + token-window + ort
    /// embedder; init prefetches the model.
    TextRag,
    /// Offline text pipeline: local `path` embedder, no network, model
    /// supplied by the operator.
    AirGapped,
    /// gte (text) + siglip2 (image) dual embedders (M3-T4): pdf routes
    /// text-native to pdf-text and scan-dominant to pdf-pages (the
    /// probe→affinity three-way split), md/txt stay the standard text
    /// chain, image files go to the image index; init prefetches BOTH
    /// models.
    Multimodal,
    /// siglip2 image pipeline only (M3-T4, 以图搜图): image files in,
    /// image (and text) queries out; init prefetches the model.
    ImageSearch,
    /// Text pipeline + OCR for scanned PDFs (M3-T10, design.md §5.10.4
    /// "scanned-docs 随 M3 图像模态与 OCR 交付"): gte carries the chunks
    /// whether they came from a text layer or through the OCR hop; the
    /// pdf route is the chain `[pdf-text, pdf-ocr]` (text-native → text,
    /// scan-dominant → OCR — mixed documents auto-split to the text
    /// member); init prefetches BOTH models (gte + PP-OCRv5 mobile ~20 MiB).
    ScannedDocs,
}

impl Preset {
    /// The M2 default text model (§0 ruling 2026-11-04: 305M, Apache-2.0,
    /// good CJK, light download; bge-m3 is a one-line swap).
    pub const DEFAULT_MODEL: &'static str = "gte-multilingual-base";

    /// The M3-T4 default image model (main-session provisional ruling, the
    /// M2 gte precedent: base tier, Apache-2.0, in the built-in table;
    /// so400m is a one-line swap once a table row + license audit land).
    pub const DEFAULT_IMAGE_MODEL: &'static str = "siglip2-base-patch16-224";

    /// The M3-T10 default OCR model (the §0 2026-11-07 adjudication:
    /// PP-OCRv5 mobile, det 4.60 MiB + rec 15.77 MiB, Apache-2.0 — pinned
    /// by the pdf-ocr kind; the const lives there, one source of truth).
    pub const DEFAULT_OCR_MODEL: &'static str = crate::pipeline::extract::pdf_ocr::BUILTIN_MODEL;

    /// Does this preset's embedder expect the model on disk at init time
    /// (no download)? Air-gapped = true by definition (§5.10.4 offline).
    pub fn offline(&self) -> bool {
        matches!(self, Preset::AirGapped)
    }

    /// Built-in models this preset's init-time prefetch downloads, in
    /// order (empty = none). Drives the `vectorfs init` progress lines and
    /// the five-minute "model ready at init" promise (§5.10.4).
    pub fn prefetch_models(&self) -> &'static [&'static str] {
        match self {
            Preset::Dev | Preset::AirGapped => &[],
            Preset::TextRag => &[Self::DEFAULT_MODEL],
            Preset::Multimodal => &[Self::DEFAULT_MODEL, Self::DEFAULT_IMAGE_MODEL],
            Preset::ImageSearch => &[Self::DEFAULT_IMAGE_MODEL],
            Preset::ScannedDocs => &[Self::DEFAULT_MODEL, Self::DEFAULT_OCR_MODEL],
        }
    }

    /// Model cache directory the init-time prefetch / guidance refers to.
    /// `explicit` is the `--models-dir` flag (container volume); without it
    /// the bare-metal default `<data-dir>/.vectorfs/models` applies
    /// (pipeline::models::default_models_dir — the daemon home, outside
    /// every bucket's namespace by construction, design-v3.md §4).
    pub fn models_cache_dir(&self, data_dir: &Path, explicit: Option<&Path>) -> std::path::PathBuf {
        explicit
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::pipeline::models::default_models_dir(data_dir))
    }

    /// The pipeline TOML appended after the generated server fields. Empty
    /// for `dev` — the file stays byte-identical to the pre-preset init.
    ///
    /// `models_dir` is the RESOLVED cache root ([`Preset::models_cache_dir`]
    /// output) — used only where the template must embed an absolute path
    /// (air-gapped embedder `path`); the top-level `models_dir` KEY for the
    /// model-carrying presets is written by config.rs through `FileConfig`
    /// so it stays a typed field, and only when `--models-dir` was explicit
    /// (container deployments, §5.10.7; bare metal keeps the derived
    /// default).
    pub fn template(&self, models_dir: &Path) -> String {
        match self {
            Preset::Dev => String::new(),
            Preset::TextRag => format!(
                "\
# text-rag preset (design.md §5.10.4): files in -> chunks -> gte vectors.
[embedder.text]
kind = \"ort\"
model = \"{model}\"

[extractor.pdf]
kind = \"pdf-text\"

[extractor.md]
kind = \"md\"

[extractor.t]
kind = \"txt\"

[ingest.docs]
# include/exclude omitted = the whole source bucket (design-v3.md R5)
extract = {{ pdf = \"pdf\", md = \"md\", default = \"t\" }}
chunk = {{ kind = \"token-window\", size = 512, overlap = 64 }}
embedders = [\"text\"]
merge = \"rrf\"
# Lexical search (exact-term/BM25 via `vf search --mode keyword|hybrid`) is
# OFF by default (M3 superset content, §7). To enable (then run
# `vectorfs optimize` to build the FTS index):
#   fts = true
#   # fts-tokenizer = \"icu\"    # default: dictionary zh/en word segmentation
#   # fts-tokenizer = \"ngram\"  # 2,2 bigram: best CJK recall, but Latin words
#   #                         # are bigrammed too (substring matches fire)
# Cross-encoder rerank (`vf search --rerank`: re-scores the topK hits with
# gte-multilingual-reranker-base) is OFF by default (§7). To enable (loads
# the ~1.17 GiB model at serve startup):
#   rerank = true
",
                model = Self::DEFAULT_MODEL
            ),
            Preset::AirGapped => format!(
                "\
# air-gapped preset (design.md §5.10.4): offline ort embedder, NO download.
[embedder.local]
kind = \"ort\"
path = \"{model_dir}\"
offline = true

[extractor.pdf]
kind = \"pdf-text\"

[extractor.md]
kind = \"md\"

[extractor.t]
kind = \"txt\"

[ingest.docs]
# include/exclude omitted = the whole source bucket (design-v3.md R5)
extract = {{ pdf = \"pdf\", md = \"md\", default = \"t\" }}
chunk = {{ kind = \"token-window\", size = 512, overlap = 64 }}
embedders = [\"local\"]
merge = \"rrf\"
",
                model_dir = models_dir.join(Self::DEFAULT_MODEL).display()
            ),
            Preset::Multimodal => format!(
                "\
# multimodal preset (design.md §5.10.4, M3-T4): text + image in ONE bucket —
# gte carries text chunks, siglip2 carries image chunks; queries fan out to both.
[embedder.text]
kind = \"ort\"
model = \"{model}\"

# image model: base tier (main-session provisional default, the M2 gte precedent);
# a stronger tier (so400m@384 …) is a one-line model = swap once it lands in the
# built-in table (pipeline::models license audit first).
[embedder.image]
kind = \"siglip2\"
model = \"{image_model}\"

[extractor.pdf]
kind = \"pdf-text\"

[extractor.pages]
kind = \"pdf-pages\"

[extractor.md]
kind = \"md\"

[extractor.t]
kind = \"txt\"

[extractor.imgs]
kind = \"image\"

[ingest.docs]
# include/exclude omitted = the whole source bucket (design-v3.md R5).
# pdf is a two-member chain: the probe→affinity router sends text-native
# documents to pdf-text and scan-dominant ones to pdf-pages (page images ->
# the image index); md/txt/default stay the standard text chain; img files
# go straight to the image index.
extract = {{ pdf = [\"pdf\", \"pages\"], md = \"md\", img = \"imgs\", default = \"t\" }}
chunk = {{ kind = \"token-window\", size = 512, overlap = 64 }}
embedders = [\"text\", \"image\"]
merge = \"rrf\"
# Lexical search (exact-term/BM25 via `vf search --mode keyword|hybrid`) is
# OFF by default (M3 superset content, §7). To enable (then run
# `vectorfs optimize` to build the FTS index):
#   fts = true
#   # fts-tokenizer = \"icu\"    # default: dictionary zh/en word segmentation
#   # fts-tokenizer = \"ngram\"  # 2,2 bigram: best CJK recall, but Latin words
#   #                         # are bigrammed too (substring matches fire)
# Cross-encoder rerank (`vf search --rerank`: re-scores the topK hits with
# gte-multilingual-reranker-base) is OFF by default (§7). To enable (loads
# the ~1.17 GiB model at serve startup):
#   rerank = true
",
                model = Self::DEFAULT_MODEL,
                image_model = Self::DEFAULT_IMAGE_MODEL
            ),
            Preset::ImageSearch => format!(
                "\
# image-search preset (design.md §5.10.4, M3-T4): 以图搜图 only — images in,
# `vf search --image photo.png` (and text queries, through siglip2's text
# tower) out. Image files ONLY: no pdf/md/txt routes, so non-image files are
# simply not claimed (add routes by hand for a mixed bucket — or take the
# multimodal preset). PDF page images stay out deliberately: page rasters are
# the multimodal/scanned-docs story (T8+ OCR), not the query-by-photo one.
[embedder.image]
kind = \"siglip2\"
# base tier (main-session provisional default); so400m@384 … = one-line swap
# once it lands in the built-in table (license audit first).
model = \"{image_model}\"

[extractor.imgs]
kind = \"image\"

[ingest.docs]
# include/exclude omitted = the whole source bucket (design-v3.md R5)
extract = {{ img = \"imgs\" }}
chunk = {{ kind = \"token-window\", size = 512, overlap = 64 }}
embedders = [\"image\"]
merge = \"rrf\"
",
                image_model = Self::DEFAULT_IMAGE_MODEL
            ),
            Preset::ScannedDocs => format!(
                "\
# scanned-docs preset (design.md §5.10.4, M3-T10): text pipeline + OCR for
# scanned PDFs — gte carries the chunks whether they came from a text layer
# or through the OCR hop (PaddleOCR PP-OCRv5 mobile, det+rec ~20 MiB, on
# this server's own onnxruntime).
[embedder.text]
kind = \"ort\"
model = \"{model}\"

[extractor.pdf]
kind = \"pdf-text\"

# the OCR member (dpi 300 = the OCR digitization standard: the recognizer
# wants native-resolution 48-px text lines, not display-grade pages)
[extractor.ocr]
kind = \"pdf-ocr\"

[extractor.md]
kind = \"md\"

[extractor.t]
kind = \"txt\"

[ingest.docs]
# include/exclude omitted = the whole source bucket (design-v3.md R5).
# pdf is a two-member chain: the probe->affinity router sends text-native
# documents to pdf-text and scan-dominant ones to pdf-ocr (mixed documents
# fall to pdf-text, whose text layer outranks OCR); md/txt/default stay the
# standard text chain.
extract = {{ pdf = [\"pdf\", \"ocr\"], md = \"md\", default = \"t\" }}
chunk = {{ kind = \"token-window\", size = 512, overlap = 64 }}
embedders = [\"text\"]
merge = \"rrf\"
# OCR is seconds-per-page CPU work (PP-OCRv5 mobile ~= 0.6-2 s/page): the
# default 120 s extract timeout fits only ~60-200 page scans; 600 s raises
# the ceiling for fatter volumes (chunker/embedding stay as fast as ever).
extract_timeout_secs = 600
# Lexical search (exact-term/BM25 via `vf search --mode keyword|hybrid`) is
# OFF by default (M3 superset content, §7). To enable (then run
# `vectorfs optimize` to build the FTS index):
#   fts = true
#   # fts-tokenizer = \"icu\"    # default: dictionary zh/en word segmentation
#   # fts-tokenizer = \"ngram\"  # 2,2 bigram: best CJK recall, but Latin words
#   #                         are bigrammed too (substring matches fire)
# Cross-encoder rerank (`vf search --rerank`: re-scores the topK hits with
# gte-multilingual-reranker-base) is OFF by default (§7). To enable (loads
# the ~1.17 GiB model at serve startup):
#   rerank = true
",
                model = Self::DEFAULT_MODEL
            ),
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Preset::Dev => "dev",
            Preset::TextRag => "text-rag",
            Preset::AirGapped => "air-gapped",
            Preset::Multimodal => "multimodal",
            Preset::ImageSearch => "image-search",
            Preset::ScannedDocs => "scanned-docs",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// tests — every template must satisfy the SAME gates serve applies
// (load_pipeline_config + validate_model_availability with a temp cache),
// and the text-rag template FINGERPRINT must be stable (acceptance: "text-rag
// 模板指纹稳定" guards silent template drift).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pipeline::config::{ExtractValue, load_pipeline_config};

    /// Materialize a preset into a temp data dir and return the file text
    /// (exactly what config::init_with writes).
    fn materialize(preset: Preset, models_dir: Option<&Path>) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let outcome = crate::config::init_with(
            dir.path(),
            false,
            &crate::config::InitOptions {
                preset,
                listen: Some("0.0.0.0:9000".into()),
                backend: Some("memory".into()),
                models_dir: models_dir.map(Path::to_path_buf),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&outcome.path).unwrap();
        (dir, text)
    }

    #[test]
    fn dev_is_the_pure_protocol_face() {
        let (dir, text) = materialize(Preset::Dev, None);
        // no pipeline sections at all: the section axes are absent
        for key in ["[embedder", "[extractor", "[ingest"] {
            assert!(!text.contains(key), "dev preset must not write {key}");
        }
        // parse-level check passes and yields the empty (pure protocol) config
        let cfg = load_pipeline_config(&text).unwrap();
        assert!(cfg.extractors.is_empty() && cfg.embedders.is_empty() && cfg.ingest.is_empty());
        // server-file side still round-trips through the strict reader
        let reread = crate::config::read_config(dir.path()).unwrap().unwrap();
        assert_eq!(reread.version, 1);
        assert!(reread.embedder.is_none() && reread.ingest.is_none());
    }

    #[test]
    fn text_rag_parses_and_passes_the_availability_gate() {
        let (dir, text) = materialize(Preset::TextRag, None);
        let cfg = load_pipeline_config(&text).unwrap();
        assert_eq!(cfg.embedders.len(), 1);
        assert_eq!(
            cfg.embedders["text"].describe(),
            "ort(model=gte-multilingual-base)"
        );
        assert_eq!(cfg.extractors.len(), 3);
        let ingest = &cfg.ingest["docs"];
        assert!(
            ingest.include.is_empty() && ingest.exclude.is_empty(),
            "whole bucket"
        );
        assert_eq!(ingest.embedders, ["text"]);
        // online + empty cache: OK — load-time prefetch is allowed (§5.10.4)
        cfg.validate_model_availability(dir.path()).unwrap();
        // no --models-dir passed: the top-level key must NOT be written
        // (bare metal keeps the <data-dir>/.vectorfs/models default)
        assert!(!text.contains("models_dir"));
    }

    #[test]
    fn text_rag_fingerprint_is_stable() {
        // two materializations parse to the SAME fingerprint: the template is
        // a constant, so any drift here means the parse or template went
        // nondeterministic — the exact regression the acceptance pins.
        let (_, t1) = materialize(Preset::TextRag, None);
        let (_, t2) = materialize(Preset::TextRag, None);
        let f1 = load_pipeline_config(&t1)
            .unwrap()
            .fingerprint("docs")
            .unwrap();
        let f2 = load_pipeline_config(&t2)
            .unwrap()
            .fingerprint("docs")
            .unwrap();
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), crate::pipeline::config::FINGERPRINT_HEX_LEN);
        // and the fingerprinted instances are exactly the referenced ones
        // (pdf/md/t all named in `extract`, embedder `text`)
        assert!(t1.contains("embedders = [\"text\"]"));
    }

    #[test]
    fn text_rag_writes_models_dir_only_when_explicit() {
        let vol = tempfile::tempdir().unwrap();
        let (_, text) = materialize(Preset::TextRag, Some(vol.path()));
        assert!(text.contains(&format!("models_dir = \"{}\"", vol.path().display())));
        let cfg = load_pipeline_config(&text).unwrap();
        assert_eq!(cfg.models_dir.as_deref(), Some(vol.path()));
    }

    #[test]
    fn air_gapped_passes_check_with_a_local_model_dir() {
        let cache = tempfile::tempdir().unwrap();
        // craft the offline model directory the availability gate demands
        // (the tiny-embedder fixture bytes are irrelevant here — only
        // presence of model.onnx + tokenizer.json is checked at parse time)
        let model = cache.path().join(Preset::DEFAULT_MODEL);
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("model.onnx"), b"onnx").unwrap();
        std::fs::write(model.join("tokenizer.json"), b"tok").unwrap();

        let (data, text) = materialize(Preset::AirGapped, Some(cache.path()));
        assert!(text.contains("offline = true"));
        let cfg = load_pipeline_config(&text).unwrap();
        assert_eq!(
            cfg.embedders["local"].describe(),
            format!("ort(path={})", model.display())
        );
        assert_eq!(cfg.ingest["docs"].embedders, ["local"]);
        // data dir is irrelevant for a path embedder — the check passes
        cfg.validate_model_availability(data.path()).unwrap();
    }

    #[test]
    fn air_gapped_without_a_model_fails_fast_with_the_path() {
        // the §5.10.4 offline contract: no download, the availability gate
        // points at the exact directory to populate (the `path` branch of
        // validate_model_availability — a path is never prefetched)
        let cache = tempfile::tempdir().unwrap();
        let (data, text) = materialize(Preset::AirGapped, Some(cache.path()));
        let cfg = load_pipeline_config(&text).unwrap();
        let err = cfg.validate_model_availability(data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[embedder.local]"), "{msg}");
        assert!(
            msg.contains(
                cache
                    .path()
                    .join(Preset::DEFAULT_MODEL)
                    .display()
                    .to_string()
                    .as_str()
            ),
            "{msg}"
        );
        assert!(msg.contains("model.onnx"), "{msg}");
        assert!(msg.contains("a path is never prefetched"), "{msg}");
    }

    #[test]
    fn default_preset_is_dev_and_names_roundtrip() {
        assert_eq!(Preset::default(), Preset::Dev);
        assert_eq!(Preset::Dev.to_string(), "dev");
        assert_eq!(Preset::TextRag.to_string(), "text-rag");
        assert_eq!(Preset::AirGapped.to_string(), "air-gapped");
        assert_eq!(Preset::Multimodal.to_string(), "multimodal");
        assert_eq!(Preset::ImageSearch.to_string(), "image-search");
        assert_eq!(Preset::ScannedDocs.to_string(), "scanned-docs");
        assert!(Preset::AirGapped.offline());
        assert!(!Preset::TextRag.offline());
        assert!(!Preset::Multimodal.offline());
        assert!(!Preset::ImageSearch.offline());
        assert!(!Preset::ScannedDocs.offline());
        // models cache resolution: explicit wins, default stays in the
        // reserved prefix
        assert_eq!(
            Preset::TextRag
                .models_cache_dir(Path::new("/data"), Some(Path::new("/vectorfs/models"))),
            Path::new("/vectorfs/models")
        );
        assert_eq!(
            Preset::TextRag.models_cache_dir(Path::new("/data"), None),
            Path::new("/data/.vectorfs/models")
        );
    }

    // ---- M3-T4 image tiers ----

    #[test]
    fn multimodal_parses_and_passes_the_availability_gate() {
        let (dir, text) = materialize(Preset::Multimodal, None);
        let cfg = load_pipeline_config(&text).unwrap();
        // the two embedders, described by their default models
        assert_eq!(cfg.embedders.len(), 2);
        assert_eq!(
            cfg.embedders["text"].describe(),
            "ort(model=gte-multilingual-base)"
        );
        assert_eq!(
            cfg.embedders["image"].describe(),
            format!("siglip2(model={})", Preset::DEFAULT_IMAGE_MODEL)
        );
        // five extractor instances: pdf/pdf-pages/md/txt/image
        assert_eq!(cfg.extractors.len(), 5);
        let ingest = &cfg.ingest["docs"];
        assert_eq!(ingest.embedders, ["text", "image"]);
        // the pdf route is the TWO-member chain, in order; img routes to
        // the image kind; default catches the text families
        assert_eq!(
            ingest.extract["pdf"],
            ExtractValue::Chain(vec!["pdf".into(), "pages".into()])
        );
        assert_eq!(ingest.extract["img"], ExtractValue::Single("imgs".into()));
        assert_eq!(ingest.extract["default"], ExtractValue::Single("t".into()));
        // modality carry-over passes BY PARSE (pdf-pages + image emit
        // Image; the siglip2 instance carries it — that IS the check)
        // online + empty cache: OK — load-time prefetch is allowed (§5.10.4)
        cfg.validate_model_availability(dir.path()).unwrap();
        // both default models named in the template; the so400m swap note
        // rides beside the image model (the tier decision is documented
        // where the operator would change it)
        assert!(text.contains(Preset::DEFAULT_MODEL), "{text}");
        assert!(text.contains(Preset::DEFAULT_IMAGE_MODEL), "{text}");
        assert!(text.contains("one-line"), "{text}");
        // no --models-dir passed: the top-level key must NOT be written
        assert!(!text.contains("models_dir"));
    }

    #[test]
    fn image_search_parses_and_passes_the_availability_gate() {
        let (dir, text) = materialize(Preset::ImageSearch, None);
        let cfg = load_pipeline_config(&text).unwrap();
        // ONE embedder, ONE extractor, one img route — the 以图搜图 shape
        assert_eq!(cfg.embedders.len(), 1);
        assert_eq!(
            cfg.embedders["image"].describe(),
            format!("siglip2(model={})", Preset::DEFAULT_IMAGE_MODEL)
        );
        assert_eq!(cfg.extractors.len(), 1);
        let ingest = &cfg.ingest["docs"];
        assert_eq!(ingest.embedders, ["image"]);
        assert_eq!(ingest.extract["img"], ExtractValue::Single("imgs".into()));
        // deliberately NO pdf/md/txt/default routes: non-image files are
        // not claimed at all (the template says so, the config agrees)
        for key in ["pdf", "md", "txt", "default"] {
            assert!(
                !ingest.extract.contains_key(key),
                "route {key} must be absent"
            );
        }
        assert!(!text.contains("pdf-pages"), "{text}");
        cfg.validate_model_availability(dir.path()).unwrap();
        // --models-dir explicit: the container-volume key is written (the
        // text-rag rule, extended to the image tiers)
        let vol = tempfile::tempdir().unwrap();
        let (_, text) = materialize(Preset::ImageSearch, Some(vol.path()));
        assert!(text.contains(&format!("models_dir = \"{}\"", vol.path().display())));
    }

    #[test]
    fn image_tier_fingerprints_are_stable() {
        // the text-rag stability acceptance, extended to both image tiers
        for preset in [Preset::Multimodal, Preset::ImageSearch] {
            let (_, t1) = materialize(preset, None);
            let (_, t2) = materialize(preset, None);
            let f1 = load_pipeline_config(&t1)
                .unwrap()
                .fingerprint("docs")
                .unwrap();
            let f2 = load_pipeline_config(&t2)
                .unwrap()
                .fingerprint("docs")
                .unwrap();
            assert_eq!(f1, f2, "{preset}");
            assert_eq!(f1.len(), crate::pipeline::config::FINGERPRINT_HEX_LEN);
            // and the tiers differ from each other AND from text-rag
            // (same bucket name, different pipelines)
            let (_, tr) = materialize(Preset::TextRag, None);
            let fr = load_pipeline_config(&tr)
                .unwrap()
                .fingerprint("docs")
                .unwrap();
            assert_ne!(f1, fr, "{preset} must not share text-rag's fingerprint");
        }
        let (_, m) = materialize(Preset::Multimodal, None);
        let fm = load_pipeline_config(&m)
            .unwrap()
            .fingerprint("docs")
            .unwrap();
        let (_, s) = materialize(Preset::ImageSearch, None);
        let fs = load_pipeline_config(&s)
            .unwrap()
            .fingerprint("docs")
            .unwrap();
        assert_ne!(fm, fs);
    }

    // ---- M3-T10 scanned-docs tier ----

    #[test]
    fn scanned_docs_parses_and_passes_the_availability_gate() {
        let (dir, text) = materialize(Preset::ScannedDocs, None);
        let cfg = load_pipeline_config(&text).unwrap();
        // one gte embedder, four extractor instances (pdf/ocr/md/t)
        assert_eq!(cfg.embedders.len(), 1);
        assert_eq!(
            cfg.embedders["text"].describe(),
            "ort(model=gte-multilingual-base)"
        );
        assert_eq!(cfg.extractors.len(), 4);
        assert_eq!(
            cfg.extractors["ocr"].describe(),
            "pdf-ocr",
            "default dpi: the bare label"
        );
        let ingest = &cfg.ingest["docs"];
        assert_eq!(ingest.embedders, ["text"]);
        // the pdf route is the [pdf-text, pdf-ocr] chain; md/default as
        // text-rag; the OCR-appropriate extract timeout is materialized
        assert_eq!(
            ingest.extract["pdf"],
            ExtractValue::Chain(vec!["pdf".into(), "ocr".into()])
        );
        assert_eq!(ingest.extract["default"], ExtractValue::Single("t".into()));
        assert_eq!(ingest.extract_timeout_secs, 600);
        // online + empty cache: OK — load-time prefetch is allowed
        cfg.validate_model_availability(dir.path()).unwrap();
        // no --models-dir passed: the top-level key must NOT be written
        assert!(!text.contains("models_dir"), "{text}");
        // --models-dir explicit: the container-volume key IS written (the
        // model-carrying-tier rule)
        let vol = tempfile::tempdir().unwrap();
        let (_, text) = materialize(Preset::ScannedDocs, Some(vol.path()));
        assert!(text.contains(&format!("models_dir = \"{}\"", vol.path().display())));
    }

    #[test]
    fn scanned_docs_fingerprint_is_stable_and_distinct() {
        // the text-rag stability acceptance, extended: stable across
        // materializations, and distinct from text-rag AND multimodal
        // (same bucket name, different pipelines)
        let (_, t1) = materialize(Preset::ScannedDocs, None);
        let (_, t2) = materialize(Preset::ScannedDocs, None);
        let f1 = load_pipeline_config(&t1)
            .unwrap()
            .fingerprint("docs")
            .unwrap();
        let f2 = load_pipeline_config(&t2)
            .unwrap()
            .fingerprint("docs")
            .unwrap();
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), crate::pipeline::config::FINGERPRINT_HEX_LEN);
        for other in [Preset::TextRag, Preset::Multimodal] {
            let (_, to) = materialize(other, None);
            let fo = load_pipeline_config(&to)
                .unwrap()
                .fingerprint("docs")
                .unwrap();
            assert_ne!(f1, fo, "scanned-docs must not share {other}'s fingerprint");
        }
    }

    /// The prefetch table the init CLI walks (its own view of the tiers —
    /// a preset shipping an embedder whose model nobody prefetches would
    /// break the five-minute promise silently). The scanned-docs tier's
    /// OCR model rides this table even though no EMBEDDER names it — its
    /// extractor does.
    #[test]
    fn prefetch_models_cover_every_nonoffline_embedder() {
        assert!(Preset::Dev.prefetch_models().is_empty());
        assert!(Preset::AirGapped.prefetch_models().is_empty());
        assert_eq!(Preset::TextRag.prefetch_models(), [Preset::DEFAULT_MODEL]);
        assert_eq!(
            Preset::Multimodal.prefetch_models(),
            [Preset::DEFAULT_MODEL, Preset::DEFAULT_IMAGE_MODEL]
        );
        assert_eq!(
            Preset::ImageSearch.prefetch_models(),
            [Preset::DEFAULT_IMAGE_MODEL]
        );
        assert_eq!(
            Preset::ScannedDocs.prefetch_models(),
            [Preset::DEFAULT_MODEL, Preset::DEFAULT_OCR_MODEL]
        );
        // every named model is a built-in table entry (prefetch can only
        // fetch built-ins)
        for preset in [
            Preset::TextRag,
            Preset::Multimodal,
            Preset::ImageSearch,
            Preset::ScannedDocs,
        ] {
            for name in preset.prefetch_models() {
                assert!(
                    crate::pipeline::models::builtin(name).is_some(),
                    "{preset}: {name} is not a built-in model"
                );
            }
        }
    }
}
