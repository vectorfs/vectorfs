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
use super::{Embedder, Merge, Modality, PipelineError};

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
    // M2 T2 plugin sections: instance name -> spanned section table.
    extractor: Option<BTreeMap<String, toml::Spanned<toml::Value>>>,
    embedder: Option<BTreeMap<String, toml::Spanned<toml::Value>>>,
    ingest: Option<BTreeMap<String, toml::Spanned<toml::Value>>>,
}

/// Per-bucket raw ingest shape (`deny_unknown_fields`; `chunk` stays a raw
/// value because it is parsed by the chunker axis's own two-stage parser).
#[derive(serde::Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct IngestRaw {
    scope: Option<String>,
    #[serde(default)]
    extract: BTreeMap<String, String>,
    chunk: Option<toml::Value>,
    #[serde(default)]
    embedders: Vec<String>,
    merge: Option<String>,
    /// TOML key is `index-prefix` (§5.10.4 spells it kebab-case).
    #[serde(rename = "index-prefix")]
    index_prefix: Option<String>,
}

/// Validated pipeline configuration (the parse product consumed by T3's
/// ingest worker; T2's consumers are `pipelines check` and the serve gate).
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// `[extractor.<name>]` instances, parsed.
    pub extractors: BTreeMap<String, ExtractorKind>,
    /// `[embedder.<name>]` instances, parsed (EMPTY in T2 — no kinds yet).
    pub embedders: BTreeMap<String, EmbedderKind>,
    /// `[ingest.<bucket>]` sections, validated.
    pub ingest: BTreeMap<String, IngestConfig>,
}

/// One `[ingest.<bucket>]` section (design.md §5.10.4).
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Ingest scope prefix/glob inside the instance root; `None` = whole
    /// root (`.vectorfs/` is always excluded — mechanism, not config).
    pub scope: Option<String>,
    /// mime short key (plus the reserved `default` key) -> extractor
    /// instance name, or the sentinel `"auto"` (built-in mime->kind map).
    pub extract: BTreeMap<String, String>,
    /// Inline chunker (two-stage kind parse).
    pub chunk: ChunkerKind,
    /// Embedder instance names (fan-out per §5.10.1 routing rules).
    pub embedders: Vec<String>,
    /// Query-side merge strategy (defaults to `rrf`).
    pub merge: Merge,
    /// Derived index name prefix (optional).
    pub index_prefix: Option<String>,
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
/// * `embedders` entries are DECLARED embedder instances — in T2 no embedder
///   kinds exist, so any non-empty list fails pointing at M2-T5 (an absent
///   or empty list is the pure-protocol face and is legal);
/// * extractor output modalities are carried by the listed embedders' doc
///   modalities (§5.10.3 modality carry-over; dormant until T5 registers
///   kinds, but wired and unit-tested);
/// * `merge` parses (M2: exactly `rrf`).
pub fn load_pipeline_config(text: &str) -> Result<PipelineConfig, PipelineError> {
    let raw: RawFile = toml::from_str(text)
        .map_err(|e| PipelineError::config(e.message().to_string(), toml_line(text, &e)))?;

    // ---- extractor instances ----
    let mut extractors = BTreeMap::new();
    for (name, sv) in raw.extractor.unwrap_or_default() {
        let section = format!("extractor.{name}");
        let line = Some(line_of(text, sv.span().start));
        let kind = ExtractorKind::from_config(&section, sv.get_ref(), line)?;
        extractors.insert(name, kind);
    }

    // ---- embedder instances (T2: declaration always fails — no kinds) ----
    let mut embedders = BTreeMap::new();
    for (name, sv) in raw.embedder.unwrap_or_default() {
        let section = format!("embedder.{name}");
        let line = Some(line_of(text, sv.span().start));
        let kind = EmbedderKind::from_config(&section, sv.get_ref(), line)?;
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

        // extract references must resolve ("auto" = built-in mapping, T3).
        for (mime_key, instance) in &rawi.extract {
            if instance != "auto" && !extractors.contains_key(instance) {
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
                             embedder instances: {}; no embedder kinds are registered yet — \
                             ort/openai land in M2-T5, so any list fails until then)",
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
            // extractors emit (unreachable in T2 — no kinds — but wired and
            // unit-tested for T5).
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

        ingest.insert(
            bucket,
            IngestConfig {
                scope: rawi.scope,
                extract: rawi.extract,
                chunk,
                embedders: rawi.embedders,
                merge,
                index_prefix: rawi.index_prefix,
            },
        );
    }

    Ok(PipelineConfig {
        extractors,
        embedders,
        ingest,
    })
}

/// Extractor instance names an ingest section actually uses (deduped,
/// sorted; `"auto"` resolves at scan time in T3, not to an instance).
fn used_extractor_instances(extract: &BTreeMap<String, String>) -> Vec<&str> {
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for instance in extract.values() {
        if instance != "auto" {
            used.insert(instance.as_str());
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
/// the chunker, and the listed embedder instances, all name-keyed and
/// BTreeMap-normalized so key order and formatting in the source file do
/// not matter. Query-side (`merge`) and routing-only fields (`scope`,
/// `index-prefix`) are deliberately EXCLUDED: they never change chunk
/// identity, so changing them must not invalidate completion markers.
pub fn pipeline_fingerprint(
    ingest: &IngestConfig,
    extractors: &BTreeMap<String, ExtractorKind>,
    embedders: &BTreeMap<String, EmbedderKind>,
) -> String {
    #[derive(serde::Serialize)]
    struct Fp<'a> {
        extractors: BTreeMap<&'a str, &'a ExtractorKind>,
        chunk: &'a ChunkerKind,
        embedders: BTreeMap<&'a str, &'a EmbedderKind>,
    }

    let mut fp_extractors: BTreeMap<&str, &ExtractorKind> = BTreeMap::new();
    for instance in used_extractor_instances(&ingest.extract) {
        if let Some(kind) = extractors.get(instance) {
            fp_extractors.insert(instance, kind);
        }
    }
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
scope = "docs/**"
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
        assert_eq!(d.scope.as_deref(), Some("docs/**"));
        assert_eq!(d.extract["txt"], "plain");
        assert_eq!(d.extract["default"], "auto");
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
                "[ingest.d]: unknown field 'chunkder' (allowed: scope, extract, chunk, \
                 embedders, merge, index-prefix)"
            ),
            "{msg}"
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
        let ok = "[ingest.d]\nextract = { default = \"auto\" }\nchunk = { kind = \"token-window\", \
                  size = 8, overlap = 2 }\n";
        assert!(parse(ok).is_ok());
    }

    #[test]
    fn embedder_section_fails_with_t5_note_and_line() {
        let text = "[embedder.text]\nkind = \"ort\"\nmodel = \"gte-multilingual-base\"\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[embedder.text]: unknown embedder kind 'ort'"),
            "{msg}"
        );
        assert!(msg.contains("M2-T5"), "{msg}");
        assert!(msg.contains("at line 1"), "{msg}");
    }

    #[test]
    fn embedder_reference_fails_with_t5_note_and_line() {
        let text = "[ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
                    embedders = [\"text\"]\n";
        let err = parse(text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "[ingest.d]: unknown embedder instance 'text' (declared embedder \
                          instances: (none)"
            ),
            "{msg}"
        );
        assert!(msg.contains("M2-T5"), "{msg}");
        assert!(msg.contains("at line 1"), "{msg}");
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
        let cfg = parse("[ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n")
            .unwrap();
        assert_eq!(cfg.ingest["d"].merge, Merge::Rrf);
    }

    // ---- fingerprint ----

    fn fp_of(text: &str) -> String {
        let cfg = parse(text).unwrap();
        cfg.fingerprint("d").unwrap()
    }

    fn base_ingest() -> String {
        // 5 lines so the ingest section starts at a known line
        format!(
            "[extractor.a]\nkind = \"txt\"\n\n[ingest.d]\nchunk = {{ kind = \"token-window\", \
             size = 8, overlap = 2 }}\nextract = {{ txt = \"a\" }}\n"
        )
    }

    #[test]
    fn fingerprint_stable_across_key_order_and_irrelevant_fields() {
        let a = base_ingest();
        // same semantics: map key order swapped, merge/scope/index-prefix added
        let b = "[extractor.a]\nkind = \"txt\"\n\n[ingest.d]\nchunk = { kind = \"token-window\", \
                 size = 8, overlap = 2 }\nextract = { txt = \"a\" }\nmerge = \"rrf\"\n\
                 scope = \"x/**\"\nindex-prefix = \"x\"\n";
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
                     [ingest.d]\nchunk = { kind = \"token-window\", size = 8, overlap = 2 }\n\
                     extract = { txt = \"b\" }\n";
        assert_ne!(fp_of(&base), fp_of(&other));
        // dropping the extractor reference entirely
        let none = "[extractor.a]\nkind = \"txt\"\n\n[ingest.d]\nchunk = { kind = \
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
