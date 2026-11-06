//! `zip` CONTAINER kind (M3-T7): a generic zip archive `unpack`s into one
//! [`ChildDoc`] per file entry, with each child's mime inferred from its
//! entry-name extension through the ONE mime/short-key table — the second
//! production container kind (after epub), and deliberately the DUMBEST
//! honest shape a container can have.
//!
//! # The kind's format intelligence = the extension→mime inference
//!
//! A generic zip has no manifest, no reading order, no notion of which
//! entries are "the document" — every judgment an epub's OPF provides is
//! absent. The ONE piece of format intelligence here is routing-shaped:
//! an entry's NAME extension decides which mime (hence which user chain)
//! the child re-enters, through the same single table the scanner routes
//! by ([`short_key_for_extension`] → [`mime_for_short_key`]). That is the
//! §0 division of labor: the kind supplies the format fact (the name),
//! the mechanism supplies the routing, the user's config supplies the
//! opinion.
//!
//! # Unknown extensions: application/octet-stream, NOT a kind-side skip
//!
//! An entry whose extension is off the table becomes a child with
//! `application/octet-stream` — the same honest "cannot tell" the gzip
//! normalizer's `infer_mime` answers for a wrapper-stripped name that
//! says nothing. The deliberate alternative (the kind skipping unknown
//! entries with its own reason) was considered and rejected on mechanism
//! semantics: the "skipped but siblings continue" vocabulary lives in the
//! INGEST WORKER (`run_member`), where a child with no claiming chain is
//! already skipped with a reason on the container's aggregate — a kind
//! preempting that decision would duplicate mechanism policy and, worse,
//! take the choice away from the operator: `extract = { default = … }`
//! CAN deliberately claim unknown children. The kind expands; routing
//! belongs to the user.
//!
//! Directory entries (trailing-`/` names) are skipped — they are the
//! archive's namespace, not content.
//!
//! # Nested archives
//!
//! A `.zip` entry inside a zip becomes an `application/zip` child, which
//! re-enters routing and lands on this kind again — the mechanism's
//! MAX_CONTAINER_DEPTH=2 bounds the recursion (two layers allowed, the
//! third refused outright; proven for the mechanism in M2.1 and for real
//! zips in the ingest e2e).
//!
//! # Guards (the docx discipline, same constants)
//!
//! * [`super::CONTAINER_MAX_ENTRIES`] checked before ANY entry is read
//!   (the entry-count bomb form); the worker's MAX_CHILDREN=256 guards
//!   the produced-children side — for THIS kind entries ≈ children, so
//!   both trip at the same number.
//! * [`super::ENTRY_DECOMPRESSION_CEILING`] = 256 MiB per entry, enforced
//!   while READING (zip headers lie; the cap bounds what a bomb can make
//!   this process materialize before the worker's max_file_size_mb gate
//!   sees the child). A corrupt/encrypted entry fails the whole unpack
//!   loudly — a partially-expanded archive must not index as if whole.
//!
//! # Affinity: deliberately undeclared
//!
//! The ContentClass vocabulary is PDF-shaped (probe covers the pdf family
//! only) — a zip source probes as Unknown and the router never consults
//! affinity for it (the image-kind precedent, M3-T4).
//!
//! # Role separation
//!
//! unpack is the kind's ONE product path: [`Extractor::extract`] refuses
//! loudly (the TestContainer-established container semantics). An archive
//! that unpacks to ZERO file entries returns `Ok(Some(vec![]))` and the
//! worker's "none of its 0 children produced extractable content" Err is
//! the loud verdict — an empty archive is not a source worth indexing.
//!
//! # Contract behavior
//!
//! Empty input → Err; wrong mime → Err (exact gate on `application/zip`);
//! empty mime tolerated; truncated zip → Err; output is
//! `Ok(Some(children))` — never `Ok(None)`.

use super::super::gate;
use super::{ChildDoc, Extractor, PipelineError};

/// The canonical zip mime (what the short-key table maps `zip` to).
pub(crate) const ZIP_MIME: &str = "application/zip";

/// Runtime instance (configless — expansion is the whole kind; which
/// chains consume the entries is the USER's config, the container-role
/// property).
#[derive(Debug, Clone, Copy, Default)]
pub struct ZipExtractor;

impl Extractor for ZipExtractor {
    fn extract(&self, _bytes: &[u8], _mime: &str) -> Result<Vec<super::Part>, PipelineError> {
        // role separation made loud (module docs): the worker re-routes the
        // children; extract() here is a routing bug and must fail visibly
        // instead of guessing
        Err(PipelineError::extract(
            "zip is a container kind: it unpacks (every file entry, mime by extension), never \
             extracts — the worker re-routes its children through the user's configured \
             chains; extract() here is a routing bug",
        ))
    }

    fn unpack(&self, bytes: &[u8], mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("zip", bytes)?;
        gate::refuse_foreign_mime("zip", mime, gate::Accept::Exact(ZIP_MIME))?;
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
            PipelineError::extract(format!(
                "zip: cannot open as a zip archive (corrupt upload, truncated transfer, or not \
                 a .zip file): {e}"
            ))
        })?;
        if zip.len() > super::CONTAINER_MAX_ENTRIES {
            return Err(PipelineError::extract(format!(
                "zip: the archive lists {} entries (max {}) — refused before reading any entry; \
                 if the archive is legitimate, split it",
                zip.len(),
                super::CONTAINER_MAX_ENTRIES
            )));
        }
        let mut children = Vec::with_capacity(usize::min(zip.len(), super::CONTAINER_MAX_ENTRIES));
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| PipelineError::extract(format!("zip: opening entry {i}: {e}")))?;
            if entry.is_dir() {
                continue; // namespace, not content
            }
            // name_raw over name(): the lossy view cannot invent a useful
            // extension for non-UTF-8 names, and the honest answer below is
            // octet-stream anyway
            let name = String::from_utf8_lossy(entry.name_raw()).into_owned();
            let bytes = super::read_entry_capped(
                &mut entry,
                super::ENTRY_DECOMPRESSION_CEILING,
                "zip",
                &name,
            )?;
            children.push(ChildDoc::new(
                name.clone(),
                super::infer_mime_from_name(&name),
                bytes,
            ));
        }
        Ok(Some(children))
    }
}

// ---------------------------------------------------------------------------
// config (no knobs — the tag is the whole section)
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): the typed `kind` tag
/// doubles as the allowed-field list in unknown-key errors and as the
/// fingerprint payload (canonical form `kind = "zip"`, knob-free).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZipConfig {
    pub kind: ZipTag,
}

/// Literal tag: only deserializes from `"zip"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ZipTag {
    #[serde(rename = "zip")]
    Zip,
}

// ---------------------------------------------------------------------------
// tests — in-test zip builders (the docx discipline: no committed binary
// blobs in unit tests; the archive is built with the same zip crate the
// kind reads).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A zip of `(name, bytes)` entries, deflate, in the given order.
    pub(crate) fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, bytes) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn unpack(bytes: &[u8]) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        ZipExtractor.unpack(bytes, ZIP_MIME)
    }

    #[test]
    fn every_file_entry_becomes_a_child_with_the_table_mime() {
        let zip = build_zip(&[
            ("notes/a.txt", b"plain body".as_slice()),
            ("readme.md", b"# heading".as_slice()),
            ("page.html", b"<p>hi</p>".as_slice()),
            ("doc.docx", b"pk-bytes".as_slice()),
            ("img/pic.png", b"\x89PNG".as_slice()),
        ]);
        let children = unpack(&zip).unwrap().expect("container verdict");
        assert_eq!(
            children
                .iter()
                .map(|c| (c.name.as_str(), c.mime.as_str()))
                .collect::<Vec<_>>(),
            [
                ("notes/a.txt", "text/plain"),
                ("readme.md", "text/markdown"),
                ("page.html", "text/html"),
                (
                    "doc.docx",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                ),
                ("img/pic.png", "image/jpeg"),
            ],
            "entry mime = extension through the ONE table (the img family's canonical row)"
        );
        assert_eq!(children[0].bytes, bytes::Bytes::from_static(b"plain body"));
        // entry order is preserved (the kind adds no ordering opinion —
        // unlike epub there IS no reading order to recover)
        // (implicitly asserted by the tuple order above)
        // determinism
        assert_eq!(unpack(&zip).unwrap(), Some(children));
    }

    #[test]
    fn unknown_extensions_and_directories() {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = Default::default();
        // a directory entry: skipped entirely
        zip.add_directory("some/dir/", opts).unwrap();
        zip.start_file("mystery.xyz", opts).unwrap();
        use std::io::Write;
        zip.write_all(b"unknown content").unwrap();
        zip.start_file("no-extension", opts).unwrap();
        zip.write_all(b"also unknown").unwrap();
        // a nested archive entry: application/zip child (re-enters routing;
        // the depth gate bounds recursion)
        let inner = build_zip(&[("a.txt", b"inner text".as_slice())]);
        zip.start_file("bundle.zip", opts).unwrap();
        zip.write_all(&inner).unwrap();
        let zip = zip.finish().unwrap().into_inner();

        let children = unpack(&zip).unwrap().expect("container verdict");
        assert_eq!(
            children
                .iter()
                .map(|c| (c.name.as_str(), c.mime.as_str()))
                .collect::<Vec<_>>(),
            [
                ("mystery.xyz", "application/octet-stream"),
                ("no-extension", "application/octet-stream"),
                ("bundle.zip", "application/zip"),
            ],
            "unknown ⇒ honest octet-stream (routing decides downstream); dirs skipped"
        );
    }

    #[test]
    fn corrupt_not_a_zip_and_truncated_are_loud_errors() {
        let err = unpack(b"plainly not a zip").unwrap_err();
        assert!(
            err.to_string().contains("cannot open as a zip archive"),
            "{err}"
        );
        // truncated real zip: byte-half cuts the central directory
        let zip = build_zip(&[("a.txt", b"content here long enough to truncate".as_slice())]);
        let err = unpack(&zip[..zip.len() / 2]).unwrap_err();
        assert!(
            err.to_string().contains("cannot open as a zip archive"),
            "truncated zip framing must be detected: {err}"
        );
        // the uniform gates
        let err = unpack(b"").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        let zip = build_zip(&[("a.txt", b"x".as_slice())]);
        let err = ZipExtractor
            .unpack(&zip, "application/epub+zip")
            .unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        assert!(unpack(&zip).is_ok(), "canonical mime passes");
        assert!(
            ZipExtractor.unpack(&zip, "").is_ok(),
            "empty mime tolerated"
        );
    }

    #[test]
    fn entry_count_bomb_refused_before_any_entry_read() {
        let entries: Vec<(String, &[u8])> = (0..=super::super::CONTAINER_MAX_ENTRIES)
            .map(|i| (format!("junk/{i}"), b"x" as &[u8]))
            .collect();
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        let bomb = build_zip(&refs);
        let err = unpack(&bomb).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("max {}", super::super::CONTAINER_MAX_ENTRIES)),
            "{msg}"
        );
    }

    #[test]
    fn extract_refuses_loudly_never_guesses() {
        let err = ZipExtractor.extract(b"whatever", ZIP_MIME).unwrap_err();
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
            assert_eq!(Extractor::affinity(&ZipExtractor, class), Fit::Capable);
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: ZipConfig = toml::from_str("kind = \"zip\"").unwrap();
        assert_eq!(cfg.kind, ZipTag::Zip);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"zip\"");
        assert!(toml::from_str::<ZipConfig>("kind = \"epub\"").is_err());
        assert!(toml::from_str::<ZipConfig>("kind = \"zip\"\ndpi = 300").is_err());
    }
}
