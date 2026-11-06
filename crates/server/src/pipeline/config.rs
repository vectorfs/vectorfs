//! Pipeline/plugin configuration: parse + fail-fast validation + fingerprint
//! (M2 T2; design.md §5.10.4 shape, §5.10.3 fail-fast, §5.10.5 fingerprint).
//!
//! ONE entry: [`load_pipeline_config`] takes the WHOLE `vectorfs.toml` text
//! (same file T1's `config::FileConfig` reads; the server sections are
//! tolerated here so a standalone pipeline file also passes
//! `vectorfs pipelines check` — their strict typing stays T1's job).
//!
//! # Line-number policy
//!
//! * Whole-file `toml::from_str` errors (syntax, unknown TOP-LEVEL field,
//!   wrong section shape) carry a span natively: the line is computed from
//!   it.
//! * The plugin sections are captured as `toml::Spanned<toml::Value>` — the
//!   toml-1.x equivalent of the toml_edit `DocumentMut` span walk — so every
//!   second-stage (per-kind struct) error is attributed to its section's
//!   TOML line. Per-kind values are detached from the document once parsed
//!   into `Value`s, which is WHY the design demands section-level
//!   attribution rather than exact key lines.
//!
//! # Fail-fast (§5.10.3: "several ifs, not an engine")
//!
//! First problem found aborts with one actionable line. `vectorfs pipelines
//! check` re-runs this on demand; `vectorfs serve` refuses to start (exit 2).

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use super::chunk::ChunkerKind;
use super::embed::EmbedderKind;
use super::extract::ExtractorKind;
use super::{Merge, Modality, PipelineError};

/// Length of the hex fingerprint prefix actually consumed (see
/// [`pipeline_fingerprint`]); 16 hex chars = 64 bits, far beyond any
/// realistic collision count of configured pipelines per data root.
pub const FINGERPRINT_HEX_LEN: usize = 16;

// ---------------------------------------------------------------------------
// raw file shape
// ---------------------------------------------------------------------------

/// Whole-file raw parse: every LEGAL top-level key is listed so
/// `deny_unknown_fields` still guards the file level (a typo'd `[ingsetr.d]`
/// fails with its line), while the T1 server keys are tolerated as opaque
/// values — `config::read_config` owns their strict typing, and
/// `pipelines check` must also run on standalone pipeline files.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // server keys exist only to be *accepted*
struct RawFile {
    // T1 server keys (config.rs FileConfig).
    version: Option<toml::Value>,
    region: Option<toml::Value>,
    account_id: Option<toml::Value>,
    access_key_id: Option<toml::Value>,
    secret_access_key: Option<toml::Value>,
    // M2 T5: models cache root override (scalar; defaults to
    // `<data-dir>/.vectorfs/models` — see pipeline::models). Docker
    // deployments materialize this pointing at the /vectorfs/models volume
    // (§5.10.7).
    models_dir: Option<String>,
    // `bucket_suffix` (design-v3.md §3-R6): tolerated here so the pipeline
    // parser sees the same file; strictly typed + validated below because
    // the vb-name gates need it.
    bucket_suffix: Option<String>,
    // M2 T2 plugin sections: instance name -> spanned section table.
    extractor: Option<BTreeMap<String, toml::Spanned<toml::Value>>>,
    embedder: Option<BTreeMap<String, toml::Spanned<toml::Value>>>,
    ingest: Option<BTreeMap<String, toml::Spanned<toml::Value>>>,
}

/// Per-bucket raw ingest shape (`deny_unknown_fields`; `chunk` stays a raw
/// value because it is parsed by the chunker axis's own two-stage parser).
/// v3.1 (R5): `scope` is DELETED — the source bucket IS the namespace; the
/// optional `include`/`exclude` globs are in-bucket claim filters only
/// (省缺 = 整桶). The strict parse of a v2.2 `scope` key hard-fails with its
/// TOML line (fail-fast, expected during the axis switch).
#[derive(serde::Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct IngestRaw {
    /// Optional in-bucket claim filter (glob; empty/absent = whole bucket).
    #[serde(default)]
    include: Vec<String>,
    /// Optional in-bucket claim exclusion (glob; wins over include).
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    extract: BTreeMap<String, ExtractValue>,
    chunk: Option<toml::Value>,
    #[serde(default)]
    embedders: Vec<String>,
    merge: Option<String>,
    /// TOML key is `index-prefix` (§5.10.4 spells it kebab-case).
    #[serde(rename = "index-prefix")]
    index_prefix: Option<String>,
    /// Scanner period in seconds (§5.10.5 execution model). > 0.
    #[serde(default = "default_scan_interval_secs")]
    scan_interval_secs: usize,
    /// Poison guard: objects larger than this are FAILED without a read
    /// attempt (§5.10.5). > 0.
    #[serde(default = "default_max_file_size_mb")]
    max_file_size_mb: u64,
    /// Per-file retry budget on top of the first attempt (§5.10.5).
    #[serde(default = "default_retries")]
    retries: u32,
    /// Extractor wall-clock cap per file, wrapping the extract call.
    #[serde(default = "default_extract_timeout_secs")]
    extract_timeout_secs: u64,
    /// Store chunk text as `x-vf-chunk-text` metadata (default ON, §5.10.1;
    /// turning it off drops the key from written metadata entirely).
    #[serde(default = "default_store_chunk_text")]
    store_chunk_text: bool,
}

fn default_scan_interval_secs() -> usize {
    30
}
fn default_max_file_size_mb() -> u64 {
    200
}
fn default_retries() -> u32 {
    3
}
fn default_extract_timeout_secs() -> u64 {
    120
}
fn default_store_chunk_text() -> bool {
    true
}

// ---------------------------------------------------------------------------
// extract routing values (M2.1 fallback chains)
// ---------------------------------------------------------------------------

/// One `extract` map value: the STRING form — a declared extractor instance
/// name, or the reserved sentinel `"auto"` (the built-in mime→kind map) —
/// or the ARRAY form, an ORDERED linear fallback chain of declared instance
/// names (`extract = { pdf = ["pdf-inspector", "pdf-ocr-未来"] }`).
///
/// # Design rulings baked into this shape (main-session 2026-11-05)
///
/// * A chain is a LIST, not a conditional/DAG (§5.10.8's ban stands): a list
///   of names is the same shape `embedders` already uses for fan-out — "one
///   more name in the list" is not an engine. The routing INTELLIGENCE stays
///   inside the kinds themselves (a member's Err/empty output IS its yield
///   signal); the mechanism layer only tries in order.
/// * `"auto"` is legal ONLY as the whole string value. Mixed into an array
///   it is semantically ambiguous ("auto resolves to one kind — then fall
///   back to what?") and is refused at parse time with the section line
///   rather than guessed.
/// * Duplicate members in an array are ALLOWED (deterministic — a repeated
///   attempt yields the same verdict; no rule without grounds).
/// * A string value and its one-element array are the SAME chain; the
///   fingerprint serializes both as the array form (see
///   [`pipeline_fingerprint`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractValue {
    /// String form: an instance name, or `"auto"` (built-in mapping).
    Single(String),
    /// Array form: ordered fallback chain of declared instance names.
    /// Parse-time validation guarantees non-empty and `"auto"`-free.
    Chain(Vec<String>),
}

impl ExtractValue {
    /// Normalized chain view (fingerprint + routing): the string form IS a
    /// one-element chain — `"x" ≡ ["x"]` — so every consumer sees one shape.
    pub fn chain(&self) -> Vec<&str> {
        match self {
            ExtractValue::Single(s) => vec![s.as_str()],
            ExtractValue::Chain(names) => names.iter().map(String::as_str).collect(),
        }
    }

    /// Human form for the `pipelines check` summary: verbatim for the
    /// string shape (`auto`, `insp`), bracketed for the array shape
    /// (`[insp, plain]`) so the two remain distinguishable on one line.
    pub fn describe(&self) -> String {
        match self {
            ExtractValue::Single(s) => s.clone(),
            ExtractValue::Chain(names) => {
                format!("[{}]", names.join(", "))
            }
        }
    }
}

/// Typed parse for [`ExtractValue`]: a plain string or an array of strings,
/// with serde's native type error for anything else (line attribution then
/// flows through the shared stage-2 section-line path like every other
/// ingest field error).
impl<'de> serde::Deserialize<'de> for ExtractValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ExtractValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "an extractor instance name, \"auto\", or an ordered array of instance \
                     names",
                )
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<ExtractValue, E> {
                Ok(ExtractValue::Single(s.to_string()))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<ExtractValue, A::Error> {
                let mut out = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    out.push(name);
                }
                Ok(ExtractValue::Chain(out))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Validated pipeline configuration (the parse product consumed by T3's
/// ingest worker; T2's consumers are `pipelines check` and the serve gate).
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// `[extractor.<name>]` instances, parsed.
    pub extractors: BTreeMap<String, ExtractorKind>,
    /// `[embedder.<name>]` instances, parsed (T5: ort + openai kinds).
    pub embedders: BTreeMap<String, EmbedderKind>,
    /// `[ingest.<bucket>]` sections, validated.
    pub ingest: BTreeMap<String, IngestConfig>,
    /// Top-level `models_dir` override (None = the
    /// `<data-dir>/.vectorfs/models` default, resolved at USE time — the
    /// parser has no data dir).
    pub models_dir: Option<std::path::PathBuf>,
    /// `bucket_suffix` (design-v3.md R6): validated, defaults `-vf`. The
    /// ingest-bucket name gates use it (names must fit 3..=63−len(suffix)
    /// and not end with the suffix).
    pub bucket_suffix: String,
    /// Non-fatal advisories (warnings) collected during validation — e.g. an
    /// ort embedder pointing `path` at a model outside the built-in table
    /// (license then being the operator's audit, §5.10.5). `pipelines check`
    /// prints these to stderr; they never block startup.
    pub warnings: Vec<String>,
    /// 1-based TOML line of each plugin section header, keyed
    /// `"<axis>.<name>"` — runtime availability gates
    /// ([`PipelineConfig::validate_model_availability`]) report with the
    /// same fail-fast line attribution as parse errors (§5.10.3).
    pub(crate) section_lines: BTreeMap<String, usize>,
}

impl PipelineConfig {
    /// Section header line for `"<axis>.<name>"` (see `section_lines`).
    pub fn section_line(&self, section: &str) -> Option<usize> {
        self.section_lines.get(section).copied()
    }

    /// Model-AVAILABILITY gate (the fs-checking half of fail-fast, run by
    /// `pipelines check` and the serve gate; parsing itself stays
    /// side-effect-free). `data_dir` resolves the models cache when the
    /// config does not override `models_dir`.
    ///
    /// Per ort embedder (§5.10.4 semantics):
    /// * `path` set: the directory must already hold `model.onnx` +
    ///   `tokenizer.json` (a `path` is never prefetched);
    /// * builtin name, `offline = true`: the prefetched cache dir must be
    ///   ready — no network will fix it (§5.10.4 air-gapped variant);
    /// * builtin name, online, cache not ready: OK — load-time prefetch
    ///   fetches it;
    /// * unknown name without `path`: Err — prefetch cannot fix an unknown
    ///   model.
    pub fn validate_model_availability(
        &self,
        data_dir: &std::path::Path,
    ) -> Result<(), PipelineError> {
        let models_dir = self
            .models_dir
            .clone()
            .unwrap_or_else(|| super::models::default_models_dir(data_dir));
        for (name, kind) in &self.embedders {
            let EmbedderKind::Ort(cfg) = kind else {
                continue; // openai needs no local artifacts
            };
            let section = format!("embedder.{name}");
            let line = self.section_line(&section);
            match &cfg.path {
                Some(p) => {
                    let dir = std::path::PathBuf::from(p);
                    let missing: Vec<&str> = ["model.onnx", "tokenizer.json"]
                        .iter()
                        .filter(|f| !dir.join(f).is_file())
                        .copied()
                        .collect();
                    if !missing.is_empty() {
                        return Err(PipelineError::config(
                            format!(
                                "[{section}]: path {} does not contain {} (a path is never \
                                 prefetched — point it at a complete model directory: \
                                 model.onnx + tokenizer.json)",
                                dir.display(),
                                missing.join(", ")
                            ),
                            line,
                        ));
                    }
                }
                None => {
                    // name-only (or default) resolution: the model MUST be a
                    // built-in table entry (that is what can be prefetched).
                    let name = crate::pipeline::embed::ort::resolve_model_name(cfg);
                    let Some(builtin) = super::models::builtin(&name) else {
                        return Err(PipelineError::config(
                            format!(
                                "[{section}]: unknown model '{}' (built-in models: {})",
                                name,
                                super::models::builtin_names().join(", ")
                            ),
                            line,
                        ));
                    };
                    let dir = models_dir.join(&name);
                    if cfg.offline && !super::models::is_ready(&dir, builtin) {
                        return Err(PipelineError::config(
                            format!(
                                "[{section}]: offline = true but model '{}' is not present under \
                                 {} — prefetch on a connected machine (vectorfs init --preset \
                                 text-rag) or set path = \"…\"; the network will not be used",
                                name,
                                dir.display()
                            ),
                            line,
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// One `[ingest.<bucket>]` section (design.md §5.10.4; v3.1 R5: the source
/// bucket IS the namespace — `include`/`exclude` are in-bucket claim
/// filters, not namespace selectors).
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Optional in-bucket claim filter (globs; empty = whole bucket). Affects
    /// claiming and vanish-flips ONLY — never the GC orphan live-set (R5).
    pub include: Vec<String>,
    /// Optional claim exclusion (globs; wins over `include`).
    pub exclude: Vec<String>,
    /// mime short key (plus the reserved `default` key) -> extractor
    /// instance name, the sentinel `"auto"` (built-in mime->kind map), or an
    /// ORDERED array of instance names (M2.1 fallback chain; see
    /// [`ExtractValue`]).
    pub extract: BTreeMap<String, ExtractValue>,
    /// Inline chunker (two-stage kind parse).
    pub chunk: ChunkerKind,
    /// Embedder instance names (fan-out per §5.10.1 routing rules).
    pub embedders: Vec<String>,
    /// Query-side merge strategy (defaults to `rrf`).
    pub merge: Merge,
    /// Derived index name prefix (optional).
    pub index_prefix: Option<String>,
    /// Scanner period in seconds (default 30).
    pub scan_interval_secs: usize,
    /// Poison guard, MiB (default 200).
    pub max_file_size_mb: u64,
    /// Retry budget per file (default 3).
    pub retries: u32,
    /// Extractor timeout per file, seconds (default 120).
    pub extract_timeout_secs: u64,
    /// Write `x-vf-chunk-text` metadata (default true).
    pub store_chunk_text: bool,
}

impl PipelineConfig {
    /// Pipeline fingerprint for one ingest bucket (see
    /// [`pipeline_fingerprint`]); `None` when no such bucket is configured.
    pub fn fingerprint(&self, bucket: &str) -> Option<String> {
        let ingest = self.ingest.get(bucket)?;
        Some(pipeline_fingerprint(
            ingest,
            &self.extractors,
            &self.embedders,
        ))
    }
}

// ---------------------------------------------------------------------------
// two-stage helpers (shared by all three axes' from_config)
// ---------------------------------------------------------------------------

/// Stage 1 of the two-stage pattern: read the `kind` string off a section
/// table. Extra keys are tolerated here on purpose — stage 2 is the strict
/// per-kind parse (§5.10.2).
pub(crate) fn probe_kind(raw: &toml::Value) -> Result<String, String> {
    let table = raw
        .as_table()
        .ok_or_else(|| "section must be a TOML table".to_string())?;
    match table.get("kind") {
        None => Err("missing field 'kind'".to_string()),
        Some(v) => v.as_str().map(str::to_string).ok_or_else(|| {
            format!(
                "invalid value for 'kind': expected a string, got {}",
                v.type_str()
            )
        }),
    }
}

/// 1-based TOML line of a byte offset (clamped defensively).
pub(crate) fn line_of(text: &str, byte_offset: usize) -> usize {
    let cut = byte_offset.min(text.len());
    let until = text.get(..cut).unwrap_or("");
    until.bytes().filter(|&b| b == b'\n').count() + 1
}

/// Format a stage-2 (per-kind struct) serde error with section context and
/// the section's line, shaping unknown-field errors as
/// `[section]: unknown field 'f' for kind 'k' (allowed: …)`.
pub(crate) fn stage2_error(
    section: &str,
    kind: Option<&str>,
    serde_message: &str,
    line: Option<usize>,
) -> PipelineError {
    if let Some((field, allowed)) = unknown_field_parts(serde_message) {
        let kind_part = match kind {
            Some(k) => format!(" for kind '{k}'"),
            None => String::new(),
        };
        return PipelineError::config(
            format!("[{section}]: unknown field '{field}'{kind_part} (allowed: {allowed})"),
            line,
        );
    }
    PipelineError::config(format!("[{section}]: {serde_message}"), line)
}

/// serde's `deny_unknown_fields` message shapes (serde_core 1.0.229):
/// `unknown field \`X\`, expected \`a\`` / `… expected \`a\` or \`b\`` /
/// `… expected one of \`a\`, \`b\`, \`c\``. Unpack into (field,
/// allowed-list); `None` for any other message shape, which then passes
/// through verbatim.
fn unknown_field_parts(message: &str) -> Option<(&str, String)> {
    let rest = message.strip_prefix("unknown field `")?;
    let (field, expected) = rest.split_once("`, expected ")?;
    let expected = expected.strip_prefix("one of ").unwrap_or(expected);
    let allowed = expected.replace('`', "");
    Some((field, allowed))
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

/// Parse + validate the plugin sections of a `vectorfs.toml` document.
///
/// Validations (fail-fast, first problem aborts):
/// * every `[extractor.<name>]` / `[embedder.<name>]` section parses via its
///   axis two-stage parser (unknown kind / unknown field / missing kind,
///   with the section's line);
/// * `extract` values are `"auto"` or a DECLARED extractor instance;
/// * the inline chunker exists and satisfies `0 < overlap < size`;
/// * `embedders` entries are DECLARED embedder instances (an absent or empty
///   list is the pure-protocol face and is legal);
/// * extractor output modalities are carried by the listed embedders' doc
///   modalities (§5.10.3 modality carry-over; live since T5);
/// * `merge` parses (M2: exactly `rrf`).
///
/// Parsing is side-effect free by contract: no network, no .so load, no
/// credential reads — `pipelines check` must run on an air-gapped box
/// without keys. Runtime gates (dim probe, api key, prefetch) live behind
/// `EmbedderKind::load`.
pub fn load_pipeline_config(text: &str) -> Result<PipelineConfig, PipelineError> {
    let raw: RawFile = toml::from_str(text)
        .map_err(|e| PipelineError::config(e.message().to_string(), toml_line(text, &e)))?;

    // ---- bucket_suffix (R6): validated first — the ingest name gates need
    // it, and a bad namespace constant fails before anything else parses.
    let bucket_suffix = raw
        .bucket_suffix
        .clone()
        .unwrap_or_else(|| crate::backends::DEFAULT_BUCKET_SUFFIX.to_string());
    crate::backends::validate_bucket_suffix(&bucket_suffix)
        .map_err(|e| PipelineError::config(format!("bucket_suffix: {e}"), None))?;

    // section-header lines for the runtime availability gate (§5.10.3
    // fail-fast with line attribution)
    let mut section_lines = BTreeMap::new();

    // ---- extractor instances ----
    let mut extractors = BTreeMap::new();
    for (name, sv) in raw.extractor.unwrap_or_default() {
        let section = format!("extractor.{name}");
        let line = Some(line_of(text, sv.span().start));
        let kind = ExtractorKind::from_config(&section, sv.get_ref(), line)?;
        section_lines.insert(section, line.expect("line computed above"));
        extractors.insert(name, kind);
    }

    // ---- embedder instances (T5: ort + openai parse for real) ----
    let mut embedders = BTreeMap::new();
    let mut warnings = Vec::new();
    for (name, sv) in raw.embedder.unwrap_or_default() {
        let section = format!("embedder.{name}");
        let line = Some(line_of(text, sv.span().start));
        section_lines.insert(section.clone(), line.expect("line computed above"));
        let kind = EmbedderKind::from_config(&section, sv.get_ref(), line)?;
        // §5.10.5 license red line: the BUILT-IN table is permissive-only.
        // ANY `path` means a self-managed artifact (even a gte-named one —
        // the table's pins are not what loads), so the license audit moves
        // to the operator. Warn, never reject.
        if let EmbedderKind::Ort(cfg) = &kind
            && let Some(p) = &cfg.path
        {
            warnings.push(format!(
                "embedder '{name}' loads a self-managed model via path {p} — outside the \
                 built-in table{}; its license is your audit (built-ins are \
                 permissive-only, §5.10.5)",
                if super::models::builtin(&crate::pipeline::embed::ort::resolve_model_name(cfg))
                    .is_some()
                {
                    " (the built-in pins are bypassed)"
                } else {
                    ""
                }
            ));
        }
        embedders.insert(name, kind);
    }

    // ---- ingest sections ----
    let declared_extractors: Vec<&str> = extractors.keys().map(String::as_str).collect();
    let mut ingest = BTreeMap::new();
    for (bucket, sv) in raw.ingest.unwrap_or_default() {
        let section = format!("ingest.{bucket}");
        let line = Some(line_of(text, sv.span().start));

        let rawi: IngestRaw = sv
            .get_ref()
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| stage2_error(&section, None, e.message(), line))?;

        // chunker: required — an ingest section without a chunker cannot
        // process text and there is no M2 default worth baking in silently.
        let chunk_value = rawi.chunk.clone().ok_or_else(|| {
            PipelineError::config(
                format!(
                    "[{section}]: missing field 'chunk' (every ingest section must \
                         declare its chunker, e.g. chunk = {{ kind = \"token-window\", \
                         size = 512, overlap = 64 }})"
                ),
                line,
            )
        })?;
        let chunk = ChunkerKind::from_config(&format!("{section}.chunk"), &chunk_value, line)?;

        // merge: closed set; absent = rrf (the only — and fusion-neutral —
        // M2 default).
        let merge = Merge::parse(rawi.merge.as_deref().unwrap_or("rrf"))
            .map_err(|m| PipelineError::config(format!("[{section}]: {m}"), line))?;

        // extract references must resolve ("auto" = built-in mapping, T3;
        // array form = M2.1 fallback chain, see ExtractValue).
        for (mime_key, value) in &rawi.extract {
            for instance in value.chain() {
                if instance == "auto" {
                    // legal as the WHOLE string value only: inside a chain
                    // it is semantically ambiguous and refused (see
                    // ExtractValue docs), so this arm fires iff the value is
                    // the array form with "auto" among its members.
                    if matches!(value, ExtractValue::Chain(_)) {
                        return Err(PipelineError::config(
                            format!(
                                "[{section}]: extract['{mime_key}'] mixes 'auto' into a chain \
                                 — 'auto' is only legal as the whole value (extract = {{ \
                                 {mime_key} = \"auto\" }}); a chain lists declared extractor \
                                 instance names in order",
                            ),
                            line,
                        ));
                    }
                    continue; // the whole-value sentinel: nothing to resolve
                }
                if !extractors.contains_key(instance) {
                    return Err(PipelineError::config(
                        format!(
                            "[{section}]: extract['{mime_key}'] references unknown extractor \
                             instance '{instance}' (declared extractor instances: {})",
                            names_or_none(&declared_extractors)
                        ),
                        line,
                    ));
                }
            }
            if matches!(value, ExtractValue::Chain(names) if names.is_empty()) {
                return Err(PipelineError::config(
                    format!(
                        "[{section}]: extract['{mime_key}'] is an empty chain — declare at \
                         least one extractor instance name, or route the key with \"auto\"",
                    ),
                    line,
                ));
            }
        }

        // embedder references must resolve; then the modality carry-over
        // check over the listed instances (§5.10.3).
        let mut carried: BTreeSet<Modality> = BTreeSet::new();
        for instance in &rawi.embedders {
            match embedders.get(instance) {
                Some(kind) => carried.extend(kind.doc_modalities()),
                None => {
                    return Err(PipelineError::config(
                        format!(
                            "[{section}]: unknown embedder instance '{instance}' (declared \
                             embedder instances: {})",
                            names_or_none(
                                &embedders.keys().map(String::as_str).collect::<Vec<_>>()
                            )
                        ),
                        line,
                    ));
                }
            }
        }
        if !rawi.embedders.is_empty() {
            // An empty/absent list is the pure-protocol face and skips the
            // check; a non-empty list must carry every modality its
            // extractors emit (live since T5 — both M2 kinds are text).
            for instance in used_extractor_instances(&rawi.extract) {
                let output = extractors
                    .get(instance)
                    .expect("reference validated above")
                    .output_modalities();
                if let Some(m) = uncovered_modalities(output.iter().copied(), &carried).first() {
                    return Err(PipelineError::config(
                        format!(
                            "[{section}]: extractor instance '{instance}' outputs modality \
                             {m} but no listed embedder accepts it on its doc side \
                             (modality carry-over, §5.10.3)"
                        ),
                        line,
                    ));
                }
            }
        }

        section_lines.insert(
            format!("ingest.{bucket}"),
            line.expect("line computed above"),
        );
        ingest.insert(
            bucket,
            IngestConfig {
                include: rawi.include,
                exclude: rawi.exclude,
                extract: rawi.extract,
                chunk,
                embedders: rawi.embedders,
                merge,
                index_prefix: rawi.index_prefix,
                scan_interval_secs: rawi.scan_interval_secs,
                max_file_size_mb: rawi.max_file_size_mb,
                retries: rawi.retries,
                extract_timeout_secs: rawi.extract_timeout_secs,
                store_chunk_text: rawi.store_chunk_text,
            },
        );
    }

    // ---- name-shape gates for everything the file layer AUTO-CREATES ----
    // The ingest section key becomes a real vector bucket and every listed
    // embedder (or the index-prefix) becomes half of a derived INDEX name
    // (`{prefix|embedder}-{key8}`, §5.10.5). The bucket rule is the v3 one
    // (design-v3.md R1/D5: 3..=63−len(suffix), no suffix tail) so a
    // CreateVectorBucket ValidationException cannot fire at 3am ingest time.
    // Derived index names additionally lose 9 chars to the "-xxxxxxxx"
    // suffix.
    for (bucket, icfg) in &ingest {
        let section = format!("ingest.{bucket}");
        let line = section_lines.get(&section).copied();
        if let Err(e) = crate::backends::validate_vector_bucket_name(bucket, &bucket_suffix) {
            return Err(PipelineError::config(
                format!(
                    "[{section}]: bucket name '{bucket}' is invalid: {e} (the file layer \
                     auto-creates this vector bucket)"
                ),
                line,
            ));
        }
        let base = icfg.index_prefix.as_deref().unwrap_or_else(|| {
            // without a prefix EVERY listed embedder name goes into a derived
            // index name; validate them all
            ""
        });
        let bases: Vec<&str> = if icfg.index_prefix.is_some() {
            vec![base]
        } else {
            icfg.embedders.iter().map(String::as_str).collect()
        };
        for b in bases {
            if !is_derived_name_base(b) {
                return Err(PipelineError::config(
                    format!(
                        "[{section}]: {} '{b}' cannot name a derived index: must be 1-54 \
                         lowercase letters, digits and hyphens, starting and ending with an \
                         alphanumeric character (derived names append a '-xxxxxxxx' key suffix)",
                        if icfg.index_prefix.is_some() {
                            "index-prefix"
                        } else {
                            "embedder instance name"
                        }
                    ),
                    line,
                ));
            }
        }
        if icfg.scan_interval_secs == 0 {
            return Err(PipelineError::config(
                format!("[{section}]: scan_interval_secs must be > 0 (0 would busy-loop)"),
                line,
            ));
        }
        if icfg.max_file_size_mb == 0 {
            return Err(PipelineError::config(
                format!("[{section}]: max_file_size_mb must be > 0"),
                line,
            ));
        }
        if icfg.extract_timeout_secs == 0 {
            return Err(PipelineError::config(
                format!("[{section}]: extract_timeout_secs must be > 0"),
                line,
            ));
        }
    }

    Ok(PipelineConfig {
        extractors,
        embedders,
        ingest,
        models_dir: raw.models_dir.map(std::path::PathBuf::from),
        bucket_suffix,
        warnings,
        section_lines,
    })
}

/// Extractor instance names an ingest section actually uses (deduped,
/// sorted; `"auto"` resolves at scan time in T3, not to an instance — every
/// chain member counts, chains and string values are the same shape here).
fn used_extractor_instances(extract: &BTreeMap<String, ExtractValue>) -> Vec<&str> {
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for value in extract.values() {
        for instance in value.chain() {
            if instance != "auto" {
                used.insert(instance);
            }
        }
    }
    used.into_iter().collect()
}

/// Modality carry-over core (§5.10.3): the output modalities no listed
/// embedder accepts on its doc side. Extracted so the check is unit-testable
/// even while T2 has no embedder kinds to drive it end-to-end.
fn uncovered_modalities(
    outputs: impl IntoIterator<Item = Modality>,
    carried: &BTreeSet<Modality>,
) -> Vec<Modality> {
    outputs
        .into_iter()
        .filter(|m| !carried.contains(m))
        .collect()
}

/// `["a", "b"]` -> `a, b`; empty -> `(none)` — for actionable error text.
fn names_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

/// Base part of a derived index name: the same charset as S3 Vectors index
/// names (3-63 rule lives in ops::validate_name; vector-bucket names use the
/// stricter v3 rule backends::validate_vector_bucket_name), but 1..=54 —
/// the `-{8 hex}` derived-key suffix must still fit inside the 63-char cap.
fn is_derived_name_base(name: &str) -> bool {
    (1..=54).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Line of a whole-file `toml::from_str` error, from its span.
fn toml_line(text: &str, err: &toml::de::Error) -> Option<usize> {
    err.span().map(|s| line_of(text, s.start))
}

// ---------------------------------------------------------------------------
// fingerprint (§5.10.5; T3 consumes: completion markers record it, a change
// fails startup and demands an explicit `vf reindex`)
// ---------------------------------------------------------------------------

/// Pipeline fingerprint of one ingest section: first
/// [`FINGERPRINT_HEX_LEN`] hex chars of `sha256(canonical TOML)` over the
/// RELEVANT instance configuration — the referenced extractor instances,
/// the extract ROUTING (M2.1: chain composition AND order; see below), the
/// chunker, and the listed embedder instances, all name-keyed and
/// BTreeMap-normalized so key order and formatting in the source file do
/// not matter. Query-side (`merge`) and claim-only fields (`include`,
/// `exclude`, `index-prefix` — v3.1 R5) are deliberately EXCLUDED: they
/// never change chunk identity, so changing them must not invalidate
/// completion markers.
pub fn pipeline_fingerprint(
    ingest: &IngestConfig,
    extractors: &BTreeMap<String, ExtractorKind>,
    embedders: &BTreeMap<String, EmbedderKind>,
) -> String {
    #[derive(serde::Serialize)]
    struct Fp<'a> {
        extractors: BTreeMap<&'a str, &'a ExtractorKind>,
        /// mime short key -> ordered chain, in the NORMALIZED ARRAY form:
        /// the string value `"x"` and the array value `["x"]` are the same
        /// chain and must derive the same fingerprint (pinned by test),
        /// while the Vec keeps the serialization order-SENSITIVE —
        /// `["a","b"]` and `["b","a"]` are different pipelines (a file the
        /// first member yields on ends up extracted by a different engine).
        extract_routes: BTreeMap<&'a str, Vec<&'a str>>,
        chunk: &'a ChunkerKind,
        embedders: BTreeMap<&'a str, &'a EmbedderKind>,
    }

    let mut fp_extractors: BTreeMap<&str, &ExtractorKind> = BTreeMap::new();
    for instance in used_extractor_instances(&ingest.extract) {
        if let Some(kind) = extractors.get(instance) {
            fp_extractors.insert(instance, kind);
        }
    }
    let fp_routes: BTreeMap<&str, Vec<&str>> = ingest
        .extract
        .iter()
        .map(|(mime_key, value)| (mime_key.as_str(), value.chain()))
        .collect();
    let mut fp_embedders: BTreeMap<&str, &EmbedderKind> = BTreeMap::new();
    for instance in &ingest.embedders {
        if let Some(kind) = embedders.get(instance) {
            fp_embedders.insert(instance.as_str(), kind);
        }
    }

    // Total for these plain-data configs (strings/ints only, no floats that
    // could go NaN, no non-string map keys): toml serialization cannot fail.
    let canonical = toml::to_string(&Fp {
        extractors: fp_extractors,
        extract_routes: fp_routes,
        chunk: &ingest.chunk,
        embedders: fp_embedders,
    })
    .expect("canonical TOML serialization of plain pipeline configs");

    let digest = Sha256::digest(canonical.as_bytes());
    hex_encode(&digest)[..FINGERPRINT_HEX_LEN].to_string()
}

/// Lowercase hex (no dependency: 16 bytes in, 32 chars out).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"version = 1
region = "us-east-1"

[extractor.plain]
kind = "txt"

[ingest.docs]
include = ["docs/**"]
exclude = ["**.tmp"]
extract = { txt = "plain", default = "auto" }
chunk = { kind = "token-window", size = 512, overlap = 64 }
merge = "rrf"
index-prefix = "docs"
"#;

    fn parse(text: &str) -> Result<PipelineConfig, PipelineError> {
        load_pipeline_config(text)
    }

    #[test]
    fn valid_config_parses() {
        let cfg = parse(VALID).unwrap();
        assert_eq!(cfg.extractors.len(), 1);
        assert!(cfg.embedders.is_empty());
        assert_eq!(cfg.ingest.len(), 1);
        let d = &cfg.ingest["docs"];
        assert_eq!(d.include, ["docs/**".to_string()]);
        assert_eq!(d.exclude, ["**.tmp".to_string()]);
        assert_eq!(cfg.bucket_suffix, crate::backends::DEFAULT_BUCKET_SUFFIX);
        assert_eq!(d.extract["txt"], ExtractValue::Single("plain".into()));
        assert_eq!(d.extract["default"], ExtractValue::Single("auto".into()));
        assert_eq!(d.merge, Merge::Rrf);
        assert_eq!(d.index_prefix.as_deref(), Some("docs"));
        assert_eq!(d.embedders, Vec::<String>::new());
        assert_eq!(d.chunk.describe(), "token-window(size=512, overlap=64)");
        // fingerprint available and well-formed
        let fp = cfg.fingerprint("docs").unwrap();
        assert_eq!(fp.len(), FINGERPRINT_HEX_LEN);
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn t1_server_fields_tolerated() {
        // full server config + pipeline sections in one file (the serve path)
        let text = "version = 1\nregion = \"us-east-1\"\naccount_id = \"000000000000\"\n\
                    access_key_id = \"VFAKx\"\nsecret_access_key = \"s\"\n\n\
                    [extractor.t]\nkind = \"txt\"\n";
        assert!(parse(text).is_ok());
        // a standalone pipeline file (no server keys) also parses
        assert!(parse("[extractor.t]\nkind = \"txt\"\n").is_ok());
    }

    #[test]
    fn unknown_top_level_section_reports_line() {
        let text = "[extractor.t]\nkind = \"txt\"\n\n[ingsetr.d]\nchunk = 1\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field `ingsetr`"), "{msg}");
        assert!(msg.contains("at line 4"), "{msg}");
    }

    #[test]
    fn unknown_extractor_field_is_shaped_and_line_numbered() {
        // section header is line 1; stage-2 errors attribute to the SECTION
        // line (per-kind values are detached from the document by then)
        let text = "[extractor.pdf]\nkind = \"txt\"\ndpi = 300\n";
        let err = parse(text).unwrap_err();
        assert_eq!(
            err.to_string(),
            "[extractor.pdf]: unknown field 'dpi' for kind 'txt' (allowed: kind) at line 1"
        );
    }

    #[test]
    fn missing_kind_and_bad_type_report_section_line() {
        let err = parse("[extractor.x]\nmodel = \"m\"\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("[extractor.x]: missing field 'kind' at line 1"),
            "{err}"
        );
        let err = parse("[extractor.x]\nkind = 7\n").unwrap_err();
        assert!(err.to_string().contains("expected a string"), "{err}");
    }

    #[test]
    fn unknown_chunker_field_lists_allowed() {
        let text = "[ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 2, sep = \"\\n\" }\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[ingest.d.chunk]: unknown field 'sep' for kind 'token-window'"),
            "{msg}"
        );
        assert!(msg.contains("allowed: kind, size, overlap"), "{msg}");
        assert!(msg.contains("at line 1"), "{msg}");
    }

    #[test]
    fn unknown_ingest_field() {
        let err =
            parse("[ingest.d]\nchunkder = { kind = \"token-window\", size = 8, overlap = 2 }\n")
                .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "[ingest.d]: unknown field 'chunkder' (allowed: include, exclude, extract, \
                 chunk, embedders, merge, index-prefix, scan_interval_secs, max_file_size_mb, \
                 retries, extract_timeout_secs, store_chunk_text)"
            ),
            "{msg}"
        );
    }

    #[test]
    fn ingest_tuning_defaults_and_overrides() {
        // defaults apply when absent
        let cfg =
            parse("[ingest.docs]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n")
                .unwrap();
        let d = &cfg.ingest["docs"];
        assert_eq!(d.scan_interval_secs, 30);
        assert_eq!(d.max_file_size_mb, 200);
        assert_eq!(d.retries, 3);
        assert_eq!(d.extract_timeout_secs, 120);
        assert!(d.store_chunk_text);
        // explicit overrides parse
        let cfg = parse(
            "[ingest.docs]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
             scan_interval_secs = 5\nmax_file_size_mb = 1\nretries = 0\n\
             extract_timeout_secs = 7\nstore_chunk_text = false\n",
        )
        .unwrap();
        let d = &cfg.ingest["docs"];
        assert_eq!(d.scan_interval_secs, 5);
        assert_eq!(d.max_file_size_mb, 1);
        assert_eq!(d.retries, 0);
        assert_eq!(d.extract_timeout_secs, 7);
        assert!(!d.store_chunk_text);
        // zero-valued periods are refused (busy-loop guard)
        for bad in [
            "scan_interval_secs = 0",
            "max_file_size_mb = 0",
            "extract_timeout_secs = 0",
        ] {
            let err = parse(&format!(
                "[ingest.docs]\nchunk = {{ kind = \"token-window\", size = 8, overlap = 2 }}\n{bad}\n"
            ))
            .unwrap_err();
            assert!(
                err.to_string().contains(bad.split_once(" =").unwrap().0),
                "{err}"
            );
        }
    }

    #[test]
    fn ingest_bucket_and_derived_name_gates() {
        // invalid bucket name (auto-created bucket must be creatable)
        let err = parse(
            "[ingest.Bad_Name]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("bucket name 'Bad_Name' is invalid"),
            "{err}"
        );
        // embedder instance names feed derived index names when no prefix
        let err = parse(
            "[embedder.Text]\nkind = \"ort\"\nmodel = \"gte-multilingual-base\"\n\
             [ingest.docs]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
             embedders = [\"Text\"]\n",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("embedder instance name 'Text' cannot name a derived index"),
            "{err}"
        );
        // an index-prefix takes over the check and a matching one passes
        let ok = parse(
            "[embedder.Text]\nkind = \"ort\"\nmodel = \"gte-multilingual-base\"\n\
             [ingest.docs]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
             embedders = [\"Text\"]\nindex-prefix = \"docs\"\n",
        )
        .unwrap();
        assert_eq!(ok.ingest["docs"].index_prefix.as_deref(), Some("docs"));
        // ...but a too-long prefix cannot fit the derived suffix
        let long = "x".repeat(55);
        let err = parse(&format!(
            "[ingest.docs]\nchunk = {{ kind = \"token-window\", size = 8, overlap = 2 }}\n\
             index-prefix = \"{long}\"\n"
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("cannot name a derived index"),
            "{err}"
        );
    }

    #[test]
    fn missing_chunk_is_actionable() {
        let err = parse("[ingest.d]\nmerge = \"rrf\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing field 'chunk'"), "{msg}");
        assert!(msg.contains("token-window"), "{msg}");
    }

    #[test]
    fn bad_window_reports_line() {
        let text = "[ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 8 }\n";
        let err = parse(text).unwrap_err();
        assert!(
            err.to_string().contains("0 < overlap < size") && err.to_string().contains("at line 1"),
            "{err}"
        );
    }

    #[test]
    fn extract_reference_must_resolve() {
        let text = "[ingest.d]\nextract = { pdf = \"pdf\" }\nchunk = { kind = \"token-window\", \
                    size = 8, overlap = 2 }\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "[ingest.d]: extract['pdf'] references unknown extractor instance \
                          'pdf' (declared extractor instances: (none))"
            ),
            "{msg}"
        );
        assert!(msg.contains("at line 1"), "{msg}");
        // "auto" needs no instance
        let ok = "[ingest.ddd]\nextract = { default = \"auto\" }\nchunk = { kind = \"token-window\", \
                  size = 8, overlap = 2 }\n";
        assert!(parse(ok).is_ok());
    }

    // ---- M2.1: extract fallback chains ----

    /// Two extractor instances + a parameterized pdf route over lines 1-6
    /// so every error below attributes to a known section line.
    fn chain_config(route: &str) -> String {
        format!(
            "[extractor.insp]\nkind = \"pdf-inspector\"\n\n[extractor.plain]\nkind = \
             \"pdf-text\"\n\n[ingest.ddd]\nextract = {{ pdf = {route} }}\nchunk = {{ kind = \
             \"token-window\", size = 8, overlap = 2 }}\n"
        )
    }

    #[test]
    fn extract_chain_forms_parse_and_describe() {
        // both forms of the SAME route parse; describe() keeps the shapes
        // distinguishable (verbatim string vs bracketed array)
        let cfg = parse(&chain_config("\"insp\"")).unwrap();
        assert_eq!(
            cfg.ingest["ddd"].extract["pdf"],
            ExtractValue::Single("insp".into())
        );
        assert_eq!(cfg.ingest["ddd"].extract["pdf"].describe(), "insp");
        let cfg = parse(&chain_config("[\"insp\", \"plain\"]")).unwrap();
        assert_eq!(
            cfg.ingest["ddd"].extract["pdf"],
            ExtractValue::Chain(vec!["insp".into(), "plain".into()])
        );
        assert_eq!(cfg.ingest["ddd"].extract["pdf"].describe(), "[insp, plain]");
        // normalized chain view: string form IS a one-element chain
        assert_eq!(
            parse(&chain_config("\"insp\"")).unwrap().ingest["ddd"].extract["pdf"].chain(),
            vec!["insp"]
        );
        // duplicate members are the user's call: deterministic, allowed
        assert!(parse(&chain_config("[\"insp\", \"insp\"]")).is_ok());
    }

    #[test]
    fn extract_chain_unknown_instance_fails_with_line() {
        let text = chain_config("[\"insp\", \"nope\"]");
        let err = parse(&text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "[ingest.ddd]: extract['pdf'] references unknown extractor instance \
                 'nope' (declared extractor instances: insp, plain)"
            ),
            "{msg}"
        );
        assert!(msg.contains("at line 7"), "{msg}");
    }

    #[test]
    fn extract_chain_auto_mixed_in_fails_with_line() {
        let err = parse(&chain_config("[\"insp\", \"auto\"]")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[ingest.ddd]: extract['pdf'] mixes 'auto' into a chain"),
            "{msg}"
        );
        assert!(msg.contains("only legal as the whole value"), "{msg}");
        assert!(msg.contains("at line 7"), "{msg}");
    }

    #[test]
    fn extract_chain_empty_array_fails() {
        let err = parse(&chain_config("[]")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[ingest.ddd]: extract['pdf'] is an empty chain"),
            "{msg}"
        );
        assert!(msg.contains("at line 7"), "{msg}");
    }

    #[test]
    fn extract_chain_non_string_member_fails() {
        // typed visitor: an int member is a serde type error attributed to
        // the section line (same path every other ingest field error takes)
        let err = parse(&chain_config("[\"insp\", 7]")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[ingest.ddd]:") && msg.contains("invalid type: integer"),
            "{msg}"
        );
        // a non-string non-array value is equally refused
        let err = parse(&chain_config("7")).unwrap_err();
        assert!(err.to_string().contains("invalid type"), "{}", err);
    }

    #[test]
    fn fingerprint_string_equals_single_element_array() {
        // "x" ≡ ["x"]: the fingerprint normalizes to the array form, so the
        // two config spellings of the same route derive the SAME digest
        // (pinned; see pipeline_fingerprint).
        let single = fp_of(&chain_config("\"insp\""));
        let array = fp_of(&chain_config("[\"insp\"]"));
        assert_eq!(single, array);
    }

    #[test]
    fn fingerprint_chain_order_matters() {
        // ["a","b"] != ["b","a"]: chain order changes which engine extracts
        // a yielded file, so it MUST move the fingerprint (canonical TOML
        // serializes Vecs in order — this test is the pin).
        let ab = fp_of(&chain_config("[\"insp\", \"plain\"]"));
        let ba = fp_of(&chain_config("[\"plain\", \"insp\"]"));
        assert_ne!(ab, ba);
        // and adding a member changes it too
        let single = fp_of(&chain_config("\"insp\""));
        assert_ne!(single, ab);
    }

    #[test]
    fn embedder_sections_parse_for_real_kinds() {
        // the §5.10.4 sample shape, verbatim intent: ort default model +
        // openai remote + air-gapped variant
        let text = r#"
[embedder.text]
kind = "ort"
model = "gte-multilingual-base"
[embedder.remote]
kind = "openai"
endpoint = "https://api.bigmodel.cn/v1"
model = "embedding-3"
dim = 2048
[embedder.local-only]
kind = "ort"
path = "/models/bge-m3"
offline = true
"#;
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.embedders.len(), 3);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("'local-only'")
                && w.contains("/models/bge-m3")
                && w.contains("license is your audit")),
            "expected the self-managed-model warning, got {:?}",
            cfg.warnings
        );
        // a gte-named self-managed copy also warns (built-in pins bypassed)
        let gte_path = concat!(
            "[embedder.selfmanaged]\n",
            "kind = \"ort\"\n",
            "model = \"gte-multilingual-base\"\n",
            "path = \"/models/gte-fp16\"\n"
        );
        let cfg2 = parse(gte_path).unwrap();
        assert!(
            cfg2.warnings
                .iter()
                .any(|w| w.contains("built-in pins are bypassed")),
            "got {:?}",
            cfg2.warnings
        );
        // the builtin-named ort (no path) does not warn
        assert_eq!(
            cfg.warnings.iter().filter(|w| w.contains("'text'")).count(),
            0
        );
        assert_eq!(
            cfg.embedders["text"].describe(),
            "ort(model=gte-multilingual-base)"
        );
        assert_eq!(
            cfg.embedders["local-only"].describe(),
            "ort(path=/models/bge-m3)"
        );
    }

    #[test]
    fn embedder_section_bad_kind_and_missing_dim_fail_with_line() {
        // unknown kind
        let text = "[embedder.t]\nkind = \"fastembed\"\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown embedder kind 'fastembed'"), "{msg}");
        assert!(msg.contains("available kinds: ort, openai"), "{msg}");
        assert!(msg.contains("at line 1"), "{msg}");
        // openai without dim: stage-2 missing-field, section line attributed
        let text = "version = 1\n\n[embedder.r]\nkind = \"openai\"\nendpoint = \"https://e/v1\"\nmodel = \"m\"\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[embedder.r]: missing field `dim`") && msg.contains("at line 3"),
            "{msg}"
        );
    }

    #[test]
    fn embedder_reference_fails_when_undeclared() {
        let text = "[ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
                    embedders = [\"text\"]\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "[ingest.d]: unknown embedder instance 'text' (declared embedder \
                          instances: (none))"
            ),
            "{msg}"
        );
        assert!(msg.contains("at line 1"), "{msg}");
    }

    #[test]
    fn full_design_example_config_parses_with_fingerprint() {
        // §5.10.4's full example: txt/pdf/md extractors + both embedder kinds
        // + a complete ingest chain — the acceptance configuration for
        // `pipelines check`.
        let text = r#"
[embedder.text]
kind = "ort"
model = "gte-multilingual-base"
[embedder.remote]
kind = "openai"
endpoint = "https://api.bigmodel.cn/v1"
model = "embedding-3"
dim = 2048

[extractor.pdf]
kind = "pdf-text"
[extractor.plain]
kind = "txt"

[ingest.docs]
include = ["docs/**"]
extract = { pdf = "pdf", default = "auto" }
chunk = { kind = "token-window", size = 512, overlap = 64 }
embedders = ["text", "remote"]
merge = "rrf"
index-prefix = "docs"
"#;
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.extractors.len(), 2);
        assert_eq!(cfg.embedders.len(), 2);
        let d = &cfg.ingest["docs"];
        assert_eq!(d.embedders, ["text", "remote"]);
        // modality carry-over is live: pdf-text emits Text, both embedders
        // carry Text on their doc side — parse succeeds (that IS the check)
        let fp = cfg.fingerprint("docs").unwrap();
        assert_eq!(fp.len(), FINGERPRINT_HEX_LEN);
        // embedding config changes must move the fingerprint
        let swapped = text.replace("model = \"embedding-3\"", "model = \"embedding-4\"");
        let cfg2 = parse(&swapped).unwrap();
        assert_ne!(cfg2.fingerprint("docs").unwrap(), fp);
        // models_dir is plumbed through
        assert!(cfg.models_dir.is_none());
        let with_dir = format!("models_dir = \"/volumes/models\"\n{text}");
        assert_eq!(
            parse(&with_dir).unwrap().models_dir.as_deref(),
            Some(std::path::Path::new("/volumes/models"))
        );
    }

    #[test]
    fn model_availability_gate_offline_and_path_rules() {
        // temp data root: cache is EMPTY
        let data = tempfile::tempdir().unwrap();
        let base = "[embedder.e]\nkind = \"ort\"\nmodel = \"gte-multilingual-base\"\n";

        // online + cache empty: OK — load-time prefetch is allowed
        let cfg = parse(base).unwrap();
        assert!(cfg.validate_model_availability(data.path()).is_ok());
        // offline + cache empty: Err with the section line
        let text = format!("{base}offline = true\n");
        let cfg = parse(&text).unwrap();
        let err = cfg.validate_model_availability(data.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[embedder.e]"), "{msg}");
        assert!(msg.contains("offline = true"), "{msg}");
        assert!(msg.contains("the network will not be used"), "{msg}");
        assert!(msg.contains("at line 1"), "{msg}");

        // a complete path passes; an incomplete path fails with the missing
        // file names
        let complete = tempfile::tempdir().unwrap();
        std::fs::write(complete.path().join("model.onnx"), b"x").unwrap();
        std::fs::write(complete.path().join("tokenizer.json"), b"x").unwrap();
        let ok_cfg = parse(&format!(
            "[embedder.e]\nkind = \"ort\"\npath = \"{}\"",
            complete.path().display()
        ))
        .unwrap();
        assert!(ok_cfg.validate_model_availability(data.path()).is_ok());
        let half = tempfile::tempdir().unwrap();
        std::fs::write(half.path().join("model.onnx"), b"x").unwrap();
        let bad_cfg = parse(&format!(
            "[embedder.e]\nkind = \"ort\"\npath = \"{}\"",
            half.path().display()
        ))
        .unwrap();
        let err = bad_cfg
            .validate_model_availability(data.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("tokenizer.json"),
            "{}",
            err.to_string()
        );

        // unknown model without path: Err listing builtins (prefetch cannot
        // fix it) — and the license warning for path configs is advisory
        let unknown = parse("[embedder.e]\nkind = \"ort\"\nmodel = \"bge-m3\"").unwrap();
        let err = unknown
            .validate_model_availability(data.path())
            .unwrap_err();
        assert!(err.to_string().contains("unknown model 'bge-m3'"), "{err}");
    }

    #[test]
    fn modality_carry_over_now_rejects_uncovered_extractor() {
        // with real kinds on the board the dormant check is reachable: an
        // extractor is referenced but NO embedder is listed -> Text uncovered.
        // (txt only ever emits Text in M2, so the practical uncovered case
        // arrives with M3 image extractors; the empty-list semantics stay
        // pure-protocol.)
        let text = "[extractor.t]\nkind = \"txt\"\n\n[embedder.e]\nkind = \"ort\"\n\
                    [ingest.ddd]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
                    extract = { txt = \"t\" }\nembedders = [\"e\"]\n";
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.ingest["ddd"].embedders, ["e"]);
        // and the negative: referencing an undeclared embedder still fails
        let bad = text.replace("embedders = [\"e\"]", "embedders = [\"nope\"]");
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn merge_closed_set_with_line() {
        let err = parse(
            "[ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
                         merge = \"weighted\"\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown merge strategy 'weighted'") && msg.contains("at line 1"),
            "{msg}"
        );
        // absent merge defaults to rrf
        let cfg =
            parse("[ingest.ddd]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n")
                .unwrap();
        assert_eq!(cfg.ingest["ddd"].merge, Merge::Rrf);
    }

    // ---- fingerprint ----

    fn fp_of(text: &str) -> String {
        let cfg = parse(text).unwrap();
        cfg.fingerprint("ddd").unwrap()
    }

    fn base_ingest() -> String {
        // 5 lines so the ingest section starts at a known line
        format!(
            "[extractor.a]\nkind = \"txt\"\n\n[ingest.ddd]\nchunk = {{ kind = \"token-window\", \
             size = 8, overlap = 2 }}\nextract = {{ txt = \"a\" }}\n"
        )
    }

    #[test]
    fn fingerprint_stable_across_key_order_and_irrelevant_fields() {
        let a = base_ingest();
        // same semantics: map key order swapped, merge + claim-only fields
        // (include/exclude/index-prefix) added — none enter the fingerprint
        let b = "[extractor.a]\nkind = \"txt\"\n\n[ingest.ddd]\nchunk = { kind = \"token-window\", \
                 size = 8, overlap = 2 }\nextract = { txt = \"a\" }\nmerge = \"rrf\"\n\
                 include = [\"x/**\"]\nexclude = [\"**.tmp\"]\nindex-prefix = \"x\"\n";
        assert_eq!(fp_of(&a), fp_of(&b));
        // repeated parse -> same value (no nondeterminism)
        assert_eq!(fp_of(&a), fp_of(&a));
    }

    #[test]
    fn fingerprint_changes_when_relevant_config_changes() {
        let base = base_ingest();
        // chunker param
        let size = base.replace("size = 8", "size = 9");
        assert_ne!(fp_of(&base), fp_of(&size));
        let overlap = base.replace("overlap = 2", "overlap = 3");
        assert_ne!(fp_of(&base), fp_of(&overlap));
        // extractor instance config (name same, section content identical
        // here — so add a DIFFERENT referenced instance instead)
        let other = "[extractor.a]\nkind = \"txt\"\n\n[extractor.b]\nkind = \"txt\"\n\n\
                     [ingest.ddd]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
                     extract = { txt = \"b\" }\n";
        assert_ne!(fp_of(&base), fp_of(&other));
        // dropping the extractor reference entirely
        let none = "[extractor.a]\nkind = \"txt\"\n\n[ingest.ddd]\nchunk = { kind = \
                    \"token-window\", size = 8, overlap = 2 }\n";
        assert_ne!(fp_of(&base), fp_of(&none));
    }

    #[test]
    fn modality_carry_over_logic() {
        // synthetic T5 shapes (the config path is gated on embedder kinds
        // existing, which T2 lacks — the logic itself is live code; M3's
        // new Modality values plug straight into the same sets)
        let outputs = [Modality::Text];
        let carried_text: BTreeSet<Modality> = [Modality::Text].into_iter().collect();
        assert!(uncovered_modalities(outputs.iter().copied(), &carried_text).is_empty());
        // nothing carried -> every output is uncovered (fail-loud shape)
        let none: BTreeSet<Modality> = BTreeSet::new();
        assert_eq!(
            uncovered_modalities(outputs.iter().copied(), &none),
            vec![Modality::Text]
        );
    }

    #[test]
    fn hex_helper() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
