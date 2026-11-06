//! `eml` CONTAINER kind (M3-T7): an RFC 5322 message (`message/rfc822`)
//! `unpack`s its MIME structure into [`ChildDoc`]s — text bodies by their
//! part mime, attachments by their declared Content-Type — the third
//! production container kind.
//!
//! # The format intelligence = the MIME-multipart walk
//!
//! §0's criterion (「格式智力/用户可选→kind」) is as unambiguous here as
//! it is for epub: which parts exist, which is the body, which is an
//! attachment, and what charset each part's bytes carry are all facts
//! ENCODED IN THE FORMAT'S HEADERS — a judgement call about structure no
//! byte-level normalizer could make mechanically. mailparse (pure Rust,
//! RFC 5322/2045-2047) does the parse; the kind supplies the policy
//! below; children re-enter USER routing (the container role — a
//! self-digesting eml kind would break the same configuration-coherence
//! property the epub kind exists to preserve).
//!
//! # What is produced
//!
//! Walking the part tree depth-first, in wire order:
//!
//! * **`multipart/*` containers** are recursed into (the classic
//!   `multipart/alternative` carrying the same body as text/plain AND
//!   text/html — BOTH become children, and the user's per-mime chains
//!   express their own opinion about each; indexing both is the honest
//!   expansion, choosing one would be a hidden policy).
//! * **Body text parts** — `text/plain` or `text/html` WITHOUT an
//!   attachment disposition/filename — become children carrying the
//!   part's OWN mime, bytes CTE- and charset-decoded to UTF-8 (the part's
//!   `charset=` param is format metadata; `get_body()` honors it — the
//!   reason charset handling belongs HERE and not in the normalizer,
//!   which has no headers to read).
//! * **Attachments** (Content-Disposition `attachment`, or any part with
//!   a filename) become children with the declared Content-Type mime;
//!   `text/*` attachments are charset-decoded like body parts, binary
//!   ones passed through CTE-decoded and untouched. A declaration of the
//!   generic `application/octet-stream` is CROSS-CHECKED against the
//!   filename extension through the one mime table (the zip kind's
//!   inference — `notes.txt` arriving as octet-stream claims text/plain),
//!   the optional refinement the kind can make without guessing: the
//!   name is format metadata, exactly like the header.
//! * **`message/rfc822` parts** (an attached or embedded email) become
//!   ONE child with mime `message/rfc822` and CTE-decoded bytes — it
//!   re-enters routing, lands on the user's eml chain, and the MECHANISM
//!   depth gate (MAX_CONTAINER_DEPTH=2) bounds the recursion; an eml
//!   inside an eml indexes, an eml inside that inside that refuses
//!   loudly.
//! * **Everything else** — inline images in a `multipart/related`, audio,
//!   … — is NOT produced: rendering resources, not independently
//!   routable documents (the epub non-spine-resources ruling's analog).
//!
//! Part NAMES: an attachment's filename when present (provenance/logs —
//! never routing input), else `part-{n}` in walk order.
//!
//! # Guards
//!
//! The document's own bytes are capped upstream by `max_file_size_mb`;
//! parts are subsets of the parsed message (mailparse is in-memory,
//! zero-copy over the input), so the per-child size and count gates the
//! WORKER already owns are the whole story here. A malformed message
//! fails loudly (mailparse errs on broken framing); an unpack that finds
//! no body and no attachment yields zero children and the worker's
//! "none of its 0 children" Err is the loud verdict.
//!
//! # Affinity: deliberately undeclared
//!
//! The ContentClass vocabulary is PDF-shaped (probe covers the pdf family
//! only) — an eml source probes as Unknown and the router never consults
//! affinity for it (the image-kind precedent, M3-T4).
//!
//! # Role separation
//!
//! unpack is the kind's ONE product path: [`Extractor::extract`] refuses
//! loudly (the TestContainer-established container semantics).
//!
//! # Contract behavior
//!
//! Empty input → Err; wrong mime → Err (exact gate on `message/rfc822`);
//! empty mime tolerated; unparseable message → Err; output is
//! `Ok(Some(children))` — never `Ok(None)`.

use super::super::gate;
use super::{ChildDoc, Extractor, PipelineError};
use mailparse::{DispositionType, ParsedMail};

/// The canonical eml mime (what the short-key table maps `eml` to).
pub(crate) const EML_MIME: &str = "message/rfc822";

/// Runtime instance (configless — which chains consume the parts is the
/// USER's config, the container-role property).
#[derive(Debug, Clone, Copy, Default)]
pub struct EmlExtractor;

impl Extractor for EmlExtractor {
    fn extract(&self, _bytes: &[u8], _mime: &str) -> Result<Vec<super::Part>, PipelineError> {
        // role separation made loud (module docs): the worker re-routes the
        // children; extract() here is a routing bug and must fail visibly
        // instead of guessing
        Err(PipelineError::extract(
            "eml is a container kind: it unpacks (MIME parts by their part mime), never extracts \
             — the worker re-routes its children through the user's configured chains; \
             extract() here is a routing bug",
        ))
    }

    fn unpack(&self, bytes: &[u8], mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("eml", bytes)?;
        gate::refuse_foreign_mime("eml", mime, gate::Accept::Exact(EML_MIME))?;
        let mail = mailparse::parse_mail(bytes).map_err(|e| {
            PipelineError::extract(format!(
                "eml: the message does not parse as RFC 5322/MIME (corrupt upload or truncated \
                 transfer): {e}"
            ))
        })?;
        let mut children = Vec::new();
        walk_parts(&mail, &mut children, &mut 0)?;
        Ok(Some(children))
    }
}

/// Depth-first walk of the MIME part tree (module docs: the policy).
fn walk_parts(
    part: &ParsedMail<'_>,
    out: &mut Vec<ChildDoc>,
    counter: &mut usize,
) -> Result<(), PipelineError> {
    let mimetype = part.ctype.mimetype.to_ascii_lowercase();
    if mimetype.starts_with("multipart/") {
        for sub in &part.subparts {
            walk_parts(sub, out, counter)?;
        }
        return Ok(());
    }
    // an embedded/attached email: ONE message/rfc822 child, re-routed (the
    // mechanism depth gate bounds the recursion — module docs)
    if mimetype == "message/rfc822" {
        let name = part_name(part, counter);
        let bytes = part.get_body_raw().map_err(|e| body_err(&name, e))?;
        out.push(ChildDoc::new(name, EML_MIME, bytes));
        return Ok(());
    }
    let disposition = part.get_content_disposition();
    let filename = disposition.params.get("filename").cloned();
    let is_attachment =
        matches!(disposition.disposition, DispositionType::Attachment) || filename.is_some();
    if is_attachment {
        let name = filename.unwrap_or_else(|| part_name(part, counter));
        // text/* attachments decode like body parts (charset metadata
        // honored); everything else rides through CTE-decoded and raw
        let bytes = if mimetype.starts_with("text/") {
            part.get_body()
                .map_err(|e| body_err(&name, e))?
                .into_bytes()
        } else {
            part.get_body_raw().map_err(|e| body_err(&name, e))?
        };
        // the octet-stream cross-check (module docs): the FILENAME is
        // format metadata exactly like the header — `notes.txt` arriving
        // declared as the generic octet-stream claims text/plain
        let mime = if mimetype == "application/octet-stream" {
            super::infer_mime_from_name(&name)
        } else {
            mimetype
        };
        out.push(ChildDoc::new(name, mime, bytes));
        return Ok(());
    }
    // body text parts (module docs): the part's OWN mime, charset-decoded
    if mimetype == "text/plain" || mimetype == "text/html" {
        let name = part_name(part, counter);
        let body = part.get_body().map_err(|e| body_err(&name, e))?;
        out.push(ChildDoc::new(name, mimetype, body));
    }
    // everything else: rendering resources — deliberately not produced
    Ok(())
}

/// The part's display name: an attachment's filename (the caller supplies
/// it), else a stable `part-{n}` in walk order. Never a routing input.
fn part_name(part: &ParsedMail<'_>, counter: &mut usize) -> String {
    // non-attachment parts rarely carry names; when one does (a `name=`
    // param), it is still the honest display name
    if let Some(name) = part.ctype.params.get("name") {
        return name.clone();
    }
    *counter += 1;
    format!("part-{}", *counter - 1)
}

fn body_err(name: &str, e: mailparse::MailParseError) -> PipelineError {
    PipelineError::extract(format!(
        "eml: decoding the body of part '{name}' failed (corrupt transfer encoding): {e}"
    ))
}

// ---------------------------------------------------------------------------
// config (no knobs — the tag is the whole section)
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): the typed `kind` tag
/// doubles as the allowed-field list in unknown-key errors and as the
/// fingerprint payload (canonical form `kind = "eml"`, knob-free).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmlConfig {
    pub kind: EmlTag,
}

/// Literal tag: only deserializes from `"eml"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EmlTag {
    #[serde(rename = "eml")]
    Eml,
}

// ---------------------------------------------------------------------------
// tests — in-test message builders (plain strings: the format is text; the
// committed e2e fixture under tests/fixtures/containers/ is a real .eml
// written once, read by the worker-level tests).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn unpack(bytes: &[u8]) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        EmlExtractor.unpack(bytes, EML_MIME)
    }

    /// (name, mime, bytes) triples of the unpack — the common assertion
    /// surface.
    fn shapes(bytes: &[u8]) -> Vec<(String, String, Vec<u8>)> {
        unpack(bytes)
            .unwrap()
            .expect("container verdict")
            .into_iter()
            .map(|c| (c.name, c.mime, c.bytes.to_vec()))
            .collect()
    }

    /// base64 of a UTF-8 string (raw bytes in, so CJK survives verbatim).
    fn b64(s: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    /// THE shape test: multipart/alternative (text/plain + text/html) plus
    /// a base64 txt attachment — two body children by their part mimes,
    /// one attachment child, CJK and transfer decoding intact.
    #[test]
    fn multipart_alternative_and_base64_attachment() {
        let eml = format!(
            "From: qa@example.org\r\n\
             To: dev@example.org\r\n\
             Subject: =?utf-8?B?5rWL6K+V?=\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"OUTER\"\r\n\
             \r\n\
             --OUTER\r\n\
             Content-Type: multipart/alternative; boundary=\"INNER\"\r\n\
             \r\n\
             --INNER\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             纯文本正文：会议改到周四。\r\n\
             --INNER\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <html><head><title>忽略标题</title></head>\
             <body><h1>通知</h1><p>HTML 正文：会议改到周四。</p></body></html>\r\n\
             --INNER--\r\n\
             --OUTER\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: base64\r\n\
             Content-Disposition: attachment; filename=\"notes.txt\"\r\n\
             \r\n\
             {}\r\n\
             --OUTER--\r\n",
            b64("附件内容：请带笔记本电脑。")
        );
        let children = shapes(eml.as_bytes());
        assert_eq!(
            children
                .iter()
                .map(|(n, m, _)| (n.as_str(), m.as_str()))
                .collect::<Vec<_>>(),
            [
                ("part-0", "text/plain"),
                ("part-1", "text/html"),
                ("notes.txt", "text/plain"),
            ],
            "bodies by part mime in wire order, attachment by filename"
        );
        assert_eq!(
            children[2].2,
            "附件内容：请带笔记本电脑。".as_bytes(),
            "base64 CTE decoded"
        );
        assert!(
            String::from_utf8_lossy(&children[0].2).contains("纯文本正文"),
            "CJK body intact: {:?}",
            children[0].2
        );
        assert!(
            String::from_utf8_lossy(&children[1].2).contains("<h1>通知</h1>"),
            "html body passed through verbatim (the html CHAIN digests it)"
        );
        // determinism
        assert_eq!(shapes(eml.as_bytes()), children);
    }

    #[test]
    fn gbk_body_part_decodes_to_utf8_via_the_part_charset() {
        // GBK bytes for 附录：第一章讲义。 — the part's charset= param is
        // the metadata that makes this mechanical (why this lives in the
        // KIND, not the byte-level normalizer)
        let gbk_bytes: &[u8] = &[
            0xb8, 0xbd, 0xc2, 0xbc, 0xa3, 0xba, 0xb5, 0xda, 0xd2, 0xbb, 0xd5, 0xc2, 0xbd, 0xb2,
            0xd2, 0xe5, 0xa1, 0xa3,
        ];
        let eml = format!(
            "From: a@b.c\r\nSubject: gbk body\r\nContent-Type: text/plain; charset=gbk\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}",
            {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(gbk_bytes)
            }
        );
        let children = shapes(eml.as_bytes());
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].1, "text/plain");
        assert_eq!(
            String::from_utf8_lossy(&children[0].2),
            "附录：第一章讲义。",
            "part charset honored (mailparse -> encoding_rs)"
        );
    }

    #[test]
    fn octet_stream_attachment_claims_its_extension() {
        let eml = "From: a@b.c\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"X\"\r\n\r\n\
             --X\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nbody\r\n\
             --X\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"data.bin\"\r\n\r\nZZ\r\n\
             --X\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"readme.md\"\r\n\r\nZZ\r\n\
             --X--\r\n";
        let children = shapes(eml.as_bytes());
        assert_eq!(
            children
                .iter()
                .map(|(_, m, _)| m.as_str())
                .collect::<Vec<_>>(),
            ["text/plain", "application/octet-stream", "text/markdown"],
            "octet-stream + table extension claims the table mime; a truly \
             unknown extension stays honest octet-stream"
        );
    }

    #[test]
    fn embedded_message_becomes_one_rfc822_child_and_inline_resources_skip() {
        let inner = "From: x@y.z\r\nSubject: inner\r\nContent-Type: text/plain; charset=utf-8\r\n\
             \r\ninner body";
        let eml = format!(
            "From: a@b.c\r\nSubject: outer\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"X\"\r\n\r\n\
             --X\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nouter body\r\n\
             --X\r\nContent-Type: message/rfc822\r\n\r\n{inner}\r\n\
             --X\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\n\r\nPNGSTUB-BYTES\r\n\
             --X--\r\n"
        );
        let children = shapes(eml.as_bytes());
        assert_eq!(
            children
                .iter()
                .map(|(n, m, _)| (n.as_str(), m.as_str()))
                .collect::<Vec<_>>(),
            [("part-0", "text/plain"), ("part-1", "message/rfc822")],
            "the embedded email is ONE re-routable child; the inline image is a resource, not a \
             document"
        );
        assert_eq!(children[1].2, inner.as_bytes());
    }

    #[test]
    fn plain_single_body_message_is_one_text_child() {
        let children = shapes(b"From: a@b.c\r\nSubject: hi\r\n\r\njust a body");
        assert_eq!(
            children
                .iter()
                .map(|(_, m, _)| m.as_str())
                .collect::<Vec<_>>(),
            ["text/plain"]
        );
        assert_eq!(children[0].2, b"just a body");
    }

    #[test]
    fn gates_empty_wrong_mime_and_garbage() {
        let err = unpack(b"").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        let err = EmlExtractor
            .unpack(b"From: a@b.c\r\n\r\nx", "application/zip")
            .unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        assert!(
            unpack(b"From: a@b.c\r\n\r\nx").is_ok(),
            "canonical mime passes"
        );
        assert!(
            EmlExtractor.unpack(b"From: a@b.c\r\n\r\nx", "").is_ok(),
            "empty mime tolerated"
        );
    }

    #[test]
    fn extract_refuses_loudly_never_guesses() {
        let err = EmlExtractor.extract(b"whatever", EML_MIME).unwrap_err();
        assert!(
            err.to_string().contains("container kind: it unpacks"),
            "{err}"
        );
    }

    #[test]
    fn affinity_is_the_undeclared_default() {
        use crate::pipeline::{ContentClass, Fit};
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Extractor::affinity(&EmlExtractor, class), Fit::Capable);
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: EmlConfig = toml::from_str("kind = \"eml\"").unwrap();
        assert_eq!(cfg.kind, EmlTag::Eml);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"eml\"");
        assert!(toml::from_str::<EmlConfig>("kind = \"zip\"").is_err());
        assert!(toml::from_str::<EmlConfig>("kind = \"eml\"\ndpi = 300").is_err());
    }
}
