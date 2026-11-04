//! `txt` extractor kind: UTF-8 decode to one text [`Part`].
//!
//! The M2 contract decisions (asserted by [`super::super::contract`]):
//! * **empty input -> Err**, not an empty Vec: an empty Source is a
//!   user-visible anomaly (truncated upload, `touch`ed placeholder); a
//!   uniform Err keeps every kind's empty-file behavior identical and lets
//!   the ingest worker mark FAILED with a reason instead of silently
//!   indexing nothing.
//! * **non-UTF-8 -> Err**: txt means UTF-8 text; guessing an encoding or
//!   lossy-decoding would corrupt embeddings silently.
//! * **mime gate**: accepts `text/*` and an EMPTY mime (unknown — the
//!   Source metadata may not carry one; T3's sniffing fills it in later);
//!   anything else is a routing bug and must fail loudly, not guess. This
//!   is deliberately a FAMILY gate, broader than txt's own row in
//!   [`super::MIME_TO_SHORT_KEY`]: text subtypes without a dedicated kind
//!   (`text/csv`, …) fall through to txt at routing time; the canonical
//!   mime↔short-key mapping that ingest's `auto` resolves through lives in
//!   that one table.

use super::{Extractor, Modality, Part, PipelineError};

/// Runtime instance (configless — the kind has no knobs).
#[derive(Debug, Clone, Copy, Default)]
pub struct TxtExtractor;

/// Per-kind config struct: the typed `kind` tag doubles as the allowed-field
/// list in unknown-key errors and as the fingerprint payload.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxtConfig {
    pub kind: TxtTag,
}

/// Literal tag: only deserializes from `"txt"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TxtTag {
    #[serde(rename = "txt")]
    Txt,
}

impl Extractor for TxtExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        if bytes.is_empty() {
            return Err(PipelineError::extract(
                "empty input (0 bytes): the txt kind refuses empty files — the ingest \
                 worker marks the source FAILED instead of silently indexing nothing",
            ));
        }
        if !mime.is_empty() && !mime.starts_with("text/") {
            return Err(PipelineError::extract(format!(
                "txt kind cannot handle mime '{mime}' (accepts text/* or empty/unknown); \
                 this is an extract-routing bug, not a bad file"
            )));
        }
        let text = std::str::from_utf8(bytes).map_err(|e| {
            PipelineError::extract(format!(
                "input is not valid UTF-8 (txt kind is strict, no lossy decode): {e}"
            ))
        })?;
        Ok(vec![Part {
            modality: Modality::Text,
            text: text.to_owned(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_single_text_part() {
        let parts = TxtExtractor
            .extract("héllo 世界".as_bytes(), "text/plain")
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].modality, Modality::Text);
        assert_eq!(parts[0].text, "héllo 世界");
        // empty mime = unknown, accepted
        assert!(TxtExtractor.extract(b"x", "").is_ok());
        // any text/* subtype accepted
        assert!(
            TxtExtractor
                .extract(b"x", "text/markdown; charset=utf-8")
                .is_ok()
        );
    }

    #[test]
    fn non_utf8_is_error() {
        let err = TxtExtractor
            .extract(&[0xff, 0xfe, 0x00], "text/plain")
            .unwrap_err();
        assert!(matches!(err, PipelineError::Extract(_)));
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn empty_input_is_error() {
        let err = TxtExtractor.extract(b"", "text/plain").unwrap_err();
        assert!(matches!(err, PipelineError::Extract(_)));
        assert!(err.to_string().contains("empty input"), "{err}");
    }

    #[test]
    fn wrong_mime_is_error() {
        let err = TxtExtractor
            .extract(b"%PDF-1.7", "application/pdf")
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot handle mime 'application/pdf'"),
            "{err}"
        );
    }

    #[test]
    fn config_roundtrip() {
        let cfg: TxtConfig = toml::from_str("kind = \"txt\"").unwrap();
        assert_eq!(cfg.kind, TxtTag::Txt);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"txt\"");
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch)
        assert!(toml::from_str::<TxtConfig>("kind = \"md\"").is_err());
    }
}
