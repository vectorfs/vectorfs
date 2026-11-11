//! `md` extractor kind: CommonMark to plain text via `pulldown-cmark`.
//!
//! Policy: walk the parser's event stream and keep exactly the TEXT-bearing
//! events — [`Event::Text`] (paragraph/heading/list/code-block content; code
//! blocks arrive as Text events) and [`Event::Code`] (inline code, without
//! the backticks) — while structural constructs are expressed as NEWLINES so
//! the output stays readable and deterministic (§5.10.2: md is a TEXT
//! extractor; structure preservation beyond line breaks is the M3 markdown
//! chunker's job, not the extractor's).
//!
//! Deliberate skips (each keeps output honest rather than decorative):
//! * `Html`/`InlineHtml`: raw markup is presentation, not text content;
//! * `FootnoteReference`: a marker, its definition (if any) still extracts;
//! * emphasis/strong/link/image wrappers: their inner Text events flow.
//!
//! `TaskListMarker` is kept as `[x] ` / `[ ] ` — task state is real content a
//! plain-text reader loses otherwise.

use pulldown_cmark::{Event, Parser, TagEnd};

use super::super::gate;
use super::{ContentClass, Extractor, Fit, Modality, Part, PipelineError};

/// Runtime instance (configless — the kind has no knobs).
#[derive(Debug, Clone, Copy, Default)]
pub struct MdExtractor;

/// Per-kind config struct (`deny_unknown_fields`; the typed `kind` tag is
/// the allowed-list source and part of the fingerprint).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MdConfig {
    pub kind: MdTag,
}

/// Literal tag: only deserializes from `"md"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MdTag {
    #[serde(rename = "md")]
    Md,
}

// ---------------------------------------------------------------------------
// affinity (M2.1 step 2): the kind's class-level routing declaration
// ---------------------------------------------------------------------------

/// Class-level affinity (see [`super::super::Extractor::affinity`]): a
/// markdown SOURCE is text-native by construction — decoding IS the
/// classification, no probe exists for the md family, so no other class can
/// ever reach it. The Refuses arms exist for completeness of the closed
/// vocabulary (a hypothetical cross-family chain route): the router would
/// skip this member rather than feed it bytes it cannot honestly serve.
pub fn affinity(class: super::ContentClass) -> super::Fit {
    match class {
        ContentClass::TextNative => Fit::Preferred,
        _ => Fit::Refuses,
    }
}

/// One-line reason the router quotes when this kind Refuses a class (only
/// called on the Refuses arm of [`affinity`]).
pub fn refusal_note(_class: super::ContentClass) -> String {
    "a text-family kind: only text-native content is its input".to_string()
}

/// Block-level closers that terminate the current output LINE. Everything
/// else (inline emphasis/links) lets its inner text flow unseparated.
fn is_block_end(end: &TagEnd) -> bool {
    matches!(
        end,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::MetadataBlock(_)
    )
}

/// Append one block-separating newline, never two in a row (a `List` of
/// `Item`s would otherwise stack `Item`-end and `List`-end newlines).
fn push_break(out: &mut String) {
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Markdown source -> plain text (the shared body of [`MdExtractor::extract`]
/// and the tests; deterministic by construction: same events in, same string
/// out — pulldown-cmark is a pure pull parser).
pub fn markdown_to_text(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for event in Parser::new(md) {
        match event {
            Event::Text(t) => out.push_str(&t),
            Event::Code(c) => out.push_str(&c),
            Event::SoftBreak | Event::HardBreak | Event::Rule => push_break(&mut out),
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::End(end) if is_block_end(&end) => push_break(&mut out),
            // Html/InlineHtml/FootnoteReference and inline wrappers: no text
            // contribution (see module docs).
            _ => {}
        }
    }
    out.trim().to_string()
}

impl Extractor for MdExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        // the shared input gates (messages byte-identical to the hand-rolled
        // copies this replaced — see pipeline/gate.rs)
        gate::refuse_empty_input("md", bytes)?;
        gate::refuse_foreign_mime("md", mime, gate::Accept::Exact("text/markdown"))?;
        let md = std::str::from_utf8(bytes).map_err(|e| {
            PipelineError::extract(format!(
                "input is not valid UTF-8 (md kind is strict, no lossy decode): {e}"
            ))
        })?;
        let text = markdown_to_text(md);
        if text.is_empty() {
            return Err(PipelineError::extract(
                "markdown source carries no text content (only markup/images/rules) — \
                 refusing to index an empty document",
            ));
        }
        Ok(vec![Part {
            modality: Modality::Text,
            text,
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
            real_lines: false,
            furniture_removed: 0,
            section: None,
            sections: Vec::new(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_becomes_newlines_deterministically() {
        let md = "# Title\n\nFirst paragraph.\n\n- one\n- two\n\n```rust\nlet x = 1;\n```\n";
        let text = markdown_to_text(md);
        assert_eq!(text, "Title\nFirst paragraph.\none\ntwo\nlet x = 1;");
        // headings, inline code, links keep their inner text
        let md = "## H2 with `code` and [a link](https://x.example)\n\npara";
        assert_eq!(
            markdown_to_text(md),
            "H2 with code and a link\npara",
            "inline markup must not leak into the text"
        );
        // exact same input -> exact same output (determinism, trivially)
        assert_eq!(markdown_to_text(md), markdown_to_text(md));
    }

    #[test]
    fn cjk_and_task_list() {
        let md = "# 汉字标题\n\n段落一。\n\n- [x] 完成\n- [ ] 未完成\n";
        assert_eq!(
            markdown_to_text(md),
            "汉字标题\n段落一。\n[x] 完成\n[ ] 未完成"
        );
    }

    #[test]
    fn html_and_rules_contribute_no_text() {
        // BLOCK html is an Html event (skipped); text INSIDE inline html
        // tags is still a Text event in CommonMark and correctly flows.
        let md = "<div>ignored block html</div>\n\nreal text\n\n---\n\n<span>inline</span> tail";
        assert_eq!(markdown_to_text(md), "real text\ninline tail");
        // a document with ONLY markup has no text content -> extract Errs
        let err = MdExtractor
            .extract(b"<img src=\"x.png\" />\n\n---\n", "text/markdown")
            .unwrap_err();
        assert!(err.to_string().contains("no text content"), "{err}");
    }

    #[test]
    fn extract_gates_and_part_shape() {
        let parts = MdExtractor
            .extract("# h\n\nbody 中文".as_bytes(), "text/markdown")
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].modality, Modality::Text);
        assert_eq!(parts[0].text, "h\nbody 中文");
        // empty mime = unknown, accepted
        assert!(MdExtractor.extract(b"# h", "").is_ok());
        // empty input
        let err = MdExtractor.extract(b"", "text/markdown").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        // wrong mime
        let err = MdExtractor.extract(b"# h", "application/pdf").unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot handle mime 'application/pdf'"),
            "{err}"
        );
        // non-UTF-8
        let err = MdExtractor
            .extract(&[0xff, 0xfe], "text/markdown")
            .unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn affinity_is_text_native_only() {
        assert_eq!(affinity(ContentClass::TextNative), Fit::Preferred);
        for class in [
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(affinity(class), Fit::Refuses, "{class}");
        }
        assert!(!refusal_note(ContentClass::ScanDominant).is_empty());
    }

    #[test]
    fn config_roundtrip() {
        let cfg: MdConfig = toml::from_str("kind = \"md\"").unwrap();
        assert_eq!(cfg.kind, MdTag::Md);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"md\"");
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch)
        assert!(toml::from_str::<MdConfig>("kind = \"txt\"").is_err());
        // unknown field rejected
        assert!(toml::from_str::<MdConfig>("kind = \"md\"\nflavor = \"gfm\"").is_err());
    }
}
