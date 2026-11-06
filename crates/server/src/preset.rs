//! `vectorfs init --preset` templates (M2-T8, design.md §5.10.4 "preset =
//! 内建 TOML 模板" + 附录E.4 init row).
//!
//! A preset is a static TOML template APPENDED to the generated server
//! fields (config.rs keeps owning credential generation + file writing).
//! Three tiers ship in M2 (§5.10.4): `dev` (pure protocol face, no pipeline
//! sections — byte-identical to the pre-preset init), `text-rag` (the gte
//! text pipeline; init PREFETCHES the model, ~1.17 GiB, stderr progress —
//! see pipeline::models) and `air-gapped` (offline `path` embedder, NO
//! download by construction).
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
//! * `--models-dir` (container path, §5.10.7): text-rag writes it as the
//!   top-level `models_dir` key (the Docker /vectorfs/models VOLUME);
//!   air-gapped bakes it into the embedder `path`. Bare-metal init does not
//!   pass it — text-rag then uses the `<data-dir>/.vectorfs/models`
//!   default, air-gapped defaults to the Docker volume path because that is
//!   the deployment shape the preset exists for (the printed guidance tells
//!   bare-metal operators to edit `path`).
//! * `--listen` / `--backend` affect the REPORT TEXT only — the backend is
//!   per-invocation by design (config.rs: vectorfs.toml never carries a
//!   backend) and the listen address is a serve-time flag; neither belongs
//!   in vectorfs.toml.

use std::path::Path;

/// The M2 preset tiers (clap ValueEnum so `--preset` accepts the kebab-case
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
}

impl Preset {
    /// The M2 default text model (§0 ruling 2026-11-04: 305M, Apache-2.0,
    /// good CJK, light download; bge-m3 is a one-line swap).
    pub const DEFAULT_MODEL: &'static str = "gte-multilingual-base";

    /// Does this preset's embedder expect the model on disk at init time
    /// (no download)? Air-gapped = true by definition (§5.10.4 offline).
    pub fn offline(&self) -> bool {
        matches!(self, Preset::AirGapped)
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
    /// (air-gapped embedder `path`); the top-level `models_dir` KEY for
    /// text-rag is written by config.rs through `FileConfig` so it stays a
    /// typed field, and only when `--models-dir` was explicit (container
    /// deployments, §5.10.7; bare metal keeps the derived default).
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
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Preset::Dev => "dev",
            Preset::TextRag => "text-rag",
            Preset::AirGapped => "air-gapped",
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

    use crate::pipeline::config::load_pipeline_config;

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
        assert!(Preset::AirGapped.offline());
        assert!(!Preset::TextRag.offline());
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
}
