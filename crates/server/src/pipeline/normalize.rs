//! Normalize preprocessing stage (M2.1 step 3, generalized ruling).
//!
//! Every document — and every child document a container unpacks — runs
//! through ONE fixed, ORDERED table of built-in byte-level transforms
//! BEFORE extractor routing sees it. The ruling that shaped this module:
//! preprocessing is more than decompression — it is a STAGE, and container
//! expansion is merely one FORM a stage entry could take (a form this
//! milestone wires through the container ROLE instead, see
//! [`crate::pipeline::Extractor::unpack`]).
//!
//! # Mechanism vs kind — the placement criterion (recorded, deliberate)
//!
//! * **Mechanically always-correct ⇒ normalizer.** A transform that needs
//!   NO knowledge of the payload's FORMAT — only of its own envelope — and
//!   whose output is always what the user means (gzip magic `1f 8b` is a
//!   gzip stream; decoding it is not a judgement call) belongs in the
//!   fixed table: it applies to every file uniformly and its correctness
//!   never depends on which extractor will consume the result.
//! * **Needs format knowledge or a user choice ⇒ kind (policy).** Anything
//!   that must UNDERSTAND the format to do its job — epub's OPF spine
//!   ordering is format intelligence, eml header/body splitting is a
//!   structural parse — is a container KIND routed by mime through the
//!   user's config (M3: epub/docx/eml + zip). A normalizer that had to
//!   know "this is an epub, order its spine" would smuggle a strategy axis
//!   into what must stay mechanism.
//!
//! # Not a configuration axis (§5.10.8 discipline)
//!
//! The table is compiled-in and NOT configurable. A "configurable
//! preprocessor axis" was explicitly considered and rejected: no user
//! evidence exists for needing to reorder or disable a mechanical
//! transform, and v2.1 cut five strategy axes to four on exactly this
//! discipline. The `&dyn Normalizer` table below is a PRIVATE fixed list,
//! not a plugin seam — the closed-enum registration model
//! (§5.10.2) governs the strategy axes only. Revisit as an M4+ OPTION if
//! evidence arrives (the note lives here so the decision is findable).
//!
//! # Table semantics (the ruled execution model)
//!
//! * Each transform runs ONCE against the CURRENT state, in table order.
//! * [`Normalized::Same`] continues down the table with the state intact.
//! * [`Normalized::Transformed`] REPLACES the state (bytes + mime + name)
//!   and ALSO continues down the table — the point of a table, not a
//!   chain-of-one: a future charset recoding can sit AFTER gunzip and see
//!   decompressed bytes (the "gz wrapping re-encoded text" combination),
//!   with no fixpoint loop, no re-entry, one pass.
//! * Table exhausted ⇒ the state is TERMINAL ([`NormalizeOutcome`]).
//! * [`Normalized::Expanded`] is REFUSED loudly here: expansion is the
//!   container role's path (children re-enter routing with the full
//!   chain semantics), not a table transform — an expander found in this
//!   table is a wiring bug this module makes impossible to miss.
//! * **Err is HARD.** The chain's members yield on Err (someone else may
//!   catch it); the normalize stage has NO fallback — a corrupt gzip is a
//!   corrupt document, and the Err surfaces as the document's failure
//!   reason (child level: the child is skipped with the reason on the
//!   container's aggregate). This division of labor is the whole reason
//!   `NormalizeError` is an alias of [`PipelineError`] rather than a new
//!   type: same seam, different severity contract.
//!
//! # Fingerprint: zero impact
//!
//! normalize is mechanism, not configuration — it must not appear in the
//! pipeline fingerprint (the §5.10.5 stance that already keeps probe/route
//! out). Fixing a normalizer bug changes extracted content the way an
//! engine upgrade would; re-embedding stays the explicit operator action.
//!
//! # The M2 table: `[GzNormalizer]`
//!
//! One real transform ships today (gzip; flate2 was already in the tree,
//! zero new crates). Scan-time claiming cooperates with it: a `.gz` /
//! `.gzip` suffix is a compression WRAPPER, not a content type, so the
//! ingest scanner's extension guess sees through one wrapper layer
//! ([`unwrap_compression`]) — `notes.txt.gz` is claimed as the text/plain
//! source it decompresses into, under the user's existing txt route, with
//! zero configuration.

use std::io::Read;

use bytes::Bytes;

use super::ChildDoc;
use super::PipelineError;
use super::extract::{mime_for_short_key, short_key_for_extension};

/// What ONE normalizer did to the current state (the per-transform
/// vocabulary; the table's execution model lives on it — see module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum Normalized {
    /// This transform does not apply — pass the current state through
    /// UNTOUCHED and let the table continue.
    Same,
    /// In-place transform (decompression / recoding): REPLACES the state.
    /// `name` is `Some(new)` when the transform rewrote the artifact name
    /// (gunzip strips its suffix), `None` to keep the current name.
    Transformed {
        bytes: Bytes,
        mime: String,
        name: Option<String>,
    },
    /// Expansion into child documents. NOT a table transform in M2 —
    /// expansion is the container role's path ([`Extractor::unpack`],
    /// children re-enter routing); the variant completes the vocabulary so
    /// a future ruling can wire an expanding normalizer (eml-split shape,
    /// M4+) WITHOUT touching the enum, and the table refuses it loudly
    /// today (see [`normalize`]).
    Expanded { children: Vec<ChildDoc> },
}

/// Normalize-stage error = the pipeline seam error (see module docs: same
/// type, harder severity contract — no fallback member exists here).
pub type NormalizeError = PipelineError;

/// One built-in table entry. PRIVATE on purpose: this is not a plugin
/// axis (§5.10.8; module docs). Implementations are mechanically-always-
/// correct transforms only — the placement criterion above is the gate.
trait Normalizer {
    /// Short label for the worker's one-line hit log
    /// (`normalize: a.txt.gz -> gunzip (text/plain)`).
    fn label(&self) -> &'static str;
    /// Inspect the current state; `hint_name` is the display name (object
    /// key at document level, entry path at child level) — a HINT for name
    /// derives (suffix stripping), never a routing input.
    fn run(&self, bytes: &[u8], mime: &str, hint_name: &str) -> Result<Normalized, NormalizeError>;
}

/// The fixed, ordered, compiled-in table (M2: gunzip). Order is mechanism:
/// entries see the state left by their predecessors (module docs). Under
/// cfg(test) a record-only second entry sits AFTER the real transform —
/// see [`RecordingNormalizer`] below (same production entry first, so the
/// table-order proof runs over the real thing).
#[cfg(not(test))]
const TABLE: &[&dyn Normalizer] = &[&GzNormalizer];

/// The TERMINAL state after the whole table has run (module docs: each
/// transform once, in order, `Transformed` continues, table end = terminal).
///
/// `applied` carries the labels of the transforms that FIRED, in table
/// order — the worker turns exactly that into its one stderr line; empty
/// means pure passthrough (nothing to log, the common leaf path stays
/// silent).
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizeOutcome {
    pub bytes: Bytes,
    pub mime: String,
    pub name: String,
    pub applied: Vec<&'static str>,
}

/// Strip ONE trailing compression wrapper suffix (`.gz` / `.gzip`) from a
/// display name, keeping any directory part; a name with no wrapper (or a
/// bare `.gz`) is returned unchanged. Shared by the scan-time extension
/// guess (a compression wrapper is not a content type — `x.txt.gz` IS a
/// text source, compressed) and by [`GzNormalizer`]'s name/mime derive, so
/// the two can never disagree about what the wrapper hides.
pub fn unwrap_compression(name: &str) -> &str {
    for suffix in [".gz", ".gzip"] {
        if let Some(stem) = name.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return stem;
        }
    }
    name
}

/// The single entry point: run the fixed table over one state. See
/// [`NormalizeOutcome`] and the module docs for the execution model;
/// `Err` is a hard, document-level failure reason (a corrupt input the
/// stage refuses to guess around).
pub fn normalize(bytes: Bytes, mime: &str, name: &str) -> Result<NormalizeOutcome, NormalizeError> {
    let mut cur_bytes = bytes;
    let mut cur_mime = mime.to_string();
    let mut cur_name = name.to_string();
    let mut applied: Vec<&'static str> = Vec::new();
    for entry in TABLE {
        match entry.run(&cur_bytes, &cur_mime, &cur_name)? {
            Normalized::Same => {}
            Normalized::Transformed { bytes, mime, name } => {
                applied.push(entry.label());
                cur_bytes = bytes;
                cur_mime = mime;
                if let Some(n) = name {
                    cur_name = n;
                }
            }
            Normalized::Expanded { .. } => {
                // unreachable with the M2 table — the loud wiring check the
                // enum variant exists to enforce (module docs): expansion
                // goes through the container role, never this table.
                return Err(PipelineError::extract(format!(
                    "normalize table entry '{}' returned an EXPANSION — expansion is the \
                     container role's path (Extractor::unpack; children re-enter routing), \
                     never a table transform: this is a mechanism wiring bug",
                    entry.label()
                )));
            }
        }
    }
    Ok(NormalizeOutcome {
        bytes: cur_bytes,
        mime: cur_mime,
        name: cur_name,
        applied,
    })
}

// ---------------------------------------------------------------------------
// the one real M2 transform: gzip
// ---------------------------------------------------------------------------

/// OOM structural backstop on decompression: the decode stops past this
/// many OUTPUT bytes and the transform Errs. This is NOT the policy cap —
/// the policy cap is `max_file_size_mb`, which the ingest worker enforces
/// on the POST-normalize bytes (document: park; child: skip). The ceiling
/// exists only so a hostile archive cannot make the process allocate
/// unbounded memory BEFORE that gate runs: 4 GiB sits above every sane
/// configured cap while bounding the allocation a bomb can demand in one
/// shot. Mechanism constant, deliberately not config (§5.10.8's spirit: a
/// guardrail is not policy).
const GUNZIP_OUTPUT_CEILING: u64 = 4 * 1024 * 1024 * 1024;

/// gzip magic (`1f 8b`, RFC 1952 §2.3.1) — the ONLY applicability test, by
/// design: a gz payload can arrive under any incoming mime (the scan-time
/// guess already sees through the wrapper; a container child carries
/// whatever mime its kind declared), so the transform trusts the bytes,
/// not the label.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Decompress a gzip stream IN PLACE. Mechanical by the module criterion:
/// the magic identifies the envelope, decoding it is always what the user
/// means, and nothing about the PAYLOAD's format is consulted. Corrupt
/// input (bad deflate, CRC mismatch, truncation) ⇒ `Err` naming the hint —
/// hard failure, no fallback (module docs). The OUTPUT mime is inferred
/// from the hint name with the wrapper stripped (`x.txt.gz` ⇒ text/plain);
/// a name that still says nothing (`x.gz`, `entry-01`) ⇒
/// `application/octet-stream`, and the ordinary routing semantics take
/// over downstream (no chain carries it ⇒ the child is skipped with a
/// reason; a document-level route that refuses it fails loudly).
struct GzNormalizer;

impl Normalizer for GzNormalizer {
    fn label(&self) -> &'static str {
        "gunzip"
    }

    fn run(
        &self,
        bytes: &[u8],
        _mime: &str,
        hint_name: &str,
    ) -> Result<Normalized, NormalizeError> {
        if !bytes.starts_with(&GZIP_MAGIC) {
            return Ok(Normalized::Same);
        }
        // MultiGzDecoder: concatenated members decode like `gzip -d` does
        // (gzip(1) emits them past 4 GiB); CRC32 + ISIZE verified per member
        // by flate2 — corruption and truncation arrive as io::Error.
        let mut out = Vec::new();
        // take(ceiling + 1): read at most one byte PAST the ceiling — enough
        // to distinguish "exactly at" from "over" without unbounded growth.
        let mut decoder = flate2::read::MultiGzDecoder::new(bytes).take(GUNZIP_OUTPUT_CEILING + 1);
        decoder
            .read_to_end(&mut out)
            .map_err(|e| PipelineError::extract(format!("gunzip of '{hint_name}': {e}")))?;
        if out.len() as u64 > GUNZIP_OUTPUT_CEILING {
            return Err(PipelineError::extract(format!(
                "gunzip of '{hint_name}': output exceeds the {} GiB decompression backstop — \
                 if this is a legitimate file it exceeds any sane max_file_size_mb; refusing to \
                 materialize it",
                GUNZIP_OUTPUT_CEILING / (1024 * 1024 * 1024)
            )));
        }
        // name/mime derive: ONE wrapper off the hint, then the ordinary
        // extension -> short key -> mime table (the same single source the
        // scanner routes by).
        let stem = unwrap_compression(hint_name);
        let name = (stem != hint_name).then(|| stem.to_string());
        let mime = infer_mime(stem);
        Ok(Normalized::Transformed {
            bytes: Bytes::from(out),
            mime,
            name,
        })
    }
}

/// Extension (of a wrapper-stripped name) -> canonical mime, through the
/// one mime/short-key table; unknown or absent extension ⇒
/// `application/octet-stream` (the honest "cannot tell" — downstream
/// routing treats it exactly like any off-table mime).
fn infer_mime(stripped_name: &str) -> String {
    let file = stripped_name.rsplit('/').next().unwrap_or(stripped_name);
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    ext.as_deref()
        .and_then(short_key_for_extension)
        .and_then(mime_for_short_key)
        .unwrap_or("application/octet-stream")
        .to_string()
}

// ---------------------------------------------------------------------------
// cfg(test): the table-order proof machinery
// ---------------------------------------------------------------------------

/// Record-only second table entry (cfg(test)): logs the state it was
/// handed and passes it through. Its entire purpose is to PROVE the table
/// semantics — after a `Transformed`, execution CONTINUES with the NEW
/// state (the recorder sees decompressed bytes / derived mime / stripped
/// name), and a `Same` does not stop the table either.
#[cfg(test)]
struct RecordingNormalizer;

/// Armed-recording shared state (cfg(test)): tests that need the trace
/// arm the flag FIRST, so the recorder stays a no-op under the parallel
/// unit tests that only exercise the real transforms.
#[cfg(test)]
static RECORD: (std::sync::atomic::AtomicBool, std::sync::Mutex<Vec<String>>) = (
    std::sync::atomic::AtomicBool::new(false),
    std::sync::Mutex::new(Vec::new()),
);

#[cfg(test)]
impl Normalizer for RecordingNormalizer {
    fn label(&self) -> &'static str {
        "recorder"
    }

    fn run(&self, bytes: &[u8], mime: &str, hint_name: &str) -> Result<Normalized, NormalizeError> {
        use std::sync::atomic::Ordering::Relaxed;
        if RECORD.0.load(Relaxed) {
            RECORD
                .1
                .lock()
                .unwrap()
                .push(format!("recorder/{mime}/{} bytes/{hint_name}", bytes.len()));
        }
        Ok(Normalized::Same)
    }
}

/// The TABLE the test build runs: gunzip then recorder — the recorder sits
/// AFTER the real transform so the table-order proof is over the REAL
/// production entry, not a mock of it.
#[cfg(test)]
const TABLE: &[&dyn Normalizer] = &[&GzNormalizer, &RecordingNormalizer];

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::extract::MIME_TO_SHORT_KEY;
    use super::*;

    /// Real gzip bytes (the same library the transform decodes with — the
    /// tests assert DECODE semantics, not encoder quirks).
    fn gz(payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn gz_transforms_in_place_with_inferred_mime_and_stripped_name() {
        // x.txt.gz -> decompressed bytes, text/plain, x.txt, one applied label
        let out = normalize(
            Bytes::from(gz(b"hello normalize")),
            "text/plain",
            "x.txt.gz",
        )
        .unwrap();
        assert_eq!(out.bytes, Bytes::from_static(b"hello normalize"));
        assert_eq!(out.mime, "text/plain");
        assert_eq!(out.name, "x.txt");
        assert_eq!(out.applied, vec!["gunzip"]);
        // the wrapper table covers every content family the mime table knows
        for (name, mime) in [
            ("doc.pdf.gz", "application/pdf"),
            ("notes.md.gzip", "text/markdown"),
            ("d/e/readme.txt.gz", "text/plain"),
        ] {
            let out = normalize(Bytes::from(gz(b"payload")), "application/gzip", name).unwrap();
            assert_eq!(out.mime, mime, "{name}");
            assert_eq!(
                out.name,
                name.trim_end_matches(".gz").trim_end_matches(".gzip"),
                "{name}"
            );
        }
        // magic wins over the label: a gz stream under a name with NO wrapper
        // still decompresses (the incoming mime guess may have missed it)
        let out = normalize(
            Bytes::from(gz(b"body")),
            "application/octet-stream",
            "chapter1",
        )
        .unwrap();
        assert_eq!(out.bytes, Bytes::from_static(b"body"));
        assert_eq!(out.mime, "application/octet-stream", "nothing inferable");
        assert_eq!(out.name, "chapter1", "no wrapper to strip");
    }

    #[test]
    fn non_gz_is_same_and_the_entry_returns_the_terminal_state() {
        // plain bytes: NOTHING applies — the terminal state is the input
        // state verbatim (bytes by reference, mime/name unchanged), and the
        // applied trace is empty (the worker logs nothing)
        let src = Bytes::from_static(b"just text");
        let out = normalize(src.clone(), "text/plain", "docs/a.txt").unwrap();
        assert_eq!(out.bytes, src);
        assert_eq!(out.mime, "text/plain");
        assert_eq!(out.name, "docs/a.txt");
        assert!(out.applied.is_empty());
        // a .gz NAME over plain bytes (misnamed file) is NOT gz: passthrough,
        // keeping the scan-time seen-through mime — the extractor reads it
        // as the plain text it is instead of failing on a wrapper that
        // is not there
        let out = normalize(
            Bytes::from_static(b"plain inside"),
            "text/plain",
            "a.txt.gz",
        )
        .unwrap();
        assert_eq!(out.bytes, Bytes::from_static(b"plain inside"));
        assert_eq!(out.mime, "text/plain");
        assert!(out.applied.is_empty());
    }

    #[test]
    fn corrupt_gz_errs_with_the_hint_name() {
        // truncated: header + partial deflate (no CRC/ISIZE trailer at all)
        let full = gz(b"some payload that will be truncated");
        let truncated = &full[..full.len() - 12];
        for (what, bad) in [
            ("truncated", truncated.to_vec()),
            // CRC flip: valid stream, wrong checksum in the trailer
            ("crc-mismatch", {
                let mut v = gz(b"checksummed payload");
                let last = v.len() - 1;
                v[last] ^= 0xff;
                v
            }),
            // garbage after the magic
            ("garbage", {
                let mut v = vec![0x1f, 0x8b, 0x08, 0x00];
                v.extend_from_slice(b"not a deflate stream at all, just noise");
                v
            }),
        ] {
            let err =
                normalize(Bytes::from(bad), "application/gzip", "docs/bad.txt.gz").unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("bad.txt.gz"), "{what}: {msg}");
            assert!(msg.contains("gunzip"), "{what}: {msg}");
        }
    }

    /// THE table-order proof: with the recorder armed behind the real
    /// GzNormalizer, (1) a `Transformed` CONTINUES down the table carrying
    /// the NEW state — the recorder sees the DECOMPRESSED length, the
    /// DERIVED mime and the STRIPPED name; (2) a `Same` does not stop the
    /// table either — the recorder still runs, over the untouched state.
    #[test]
    fn transformed_state_feeds_the_next_table_entry_and_same_continues_too() {
        use std::sync::atomic::Ordering::Relaxed;
        /// Arm the recorder, run `run`, hand back its result PLUS the
        /// captured trace; disarm and clear on the way out (even on panic —
        /// a leaked ARMED flag would let parallel tests pollute each
        /// other's records).
        fn with_recording<R>(run: impl FnOnce() -> R) -> (R, Vec<String>) {
            struct Disarm;
            impl Drop for Disarm {
                fn drop(&mut self) {
                    RECORD.0.store(false, Relaxed);
                    RECORD.1.lock().unwrap().clear();
                }
            }
            RECORD.1.lock().unwrap().clear();
            RECORD.0.store(true, Relaxed);
            let _disarm = Disarm;
            let r = run();
            // snapshot in its OWN statement after `run`: the guard must be
            // RELEASED around any user code, because `run` calls normalize()
            // and the recorder locks this same mutex (a temporary inside a
            // call expression would be held for the whole call — a
            // self-deadlock this comment exists to prevent coming back).
            let snap = RECORD.1.lock().unwrap().clone();
            (r, snap)
        }

        let payload = b"decompressed body, longer than the wrapper could ever be";
        let compressed = gz(payload);
        // the arm flag is process-global, so a PARALLEL unit test calling
        // normalize() inside this armed window records too — filter to
        // THIS test's unique names instead of asserting the whole list
        // (robust under the default multi-threaded harness).
        let (out, trace) = with_recording(|| {
            normalize(
                Bytes::from(compressed.clone()),
                "text/plain",
                "ordered.txt.gz",
            )
            .unwrap()
        });
        let mine: Vec<String> = trace
            .iter()
            .filter(|t| t.ends_with("ordered.txt"))
            .cloned()
            .collect();
        assert_eq!(out.bytes, Bytes::copy_from_slice(payload));
        assert_eq!(
            mine,
            vec![format!(
                "recorder/text/plain/{} bytes/ordered.txt",
                payload.len()
            )],
            "the recorder ran AFTER gunzip and saw the TRANSFORMED state"
        );

        let (_, trace) = with_recording(|| {
            normalize(Bytes::from_static(b"plain"), "text/markdown", "seq.md").unwrap()
        });
        let mine: Vec<String> = trace
            .iter()
            .filter(|t| t.ends_with("seq.md"))
            .cloned()
            .collect();
        assert_eq!(
            mine,
            vec!["recorder/text/markdown/5 bytes/seq.md".to_string()],
            "Same does not stop the table — every entry runs once"
        );
    }

    #[test]
    fn unwrap_compression_strips_exactly_one_wrapper() {
        assert_eq!(unwrap_compression("x.txt.gz"), "x.txt");
        assert_eq!(unwrap_compression("x.txt.gzip"), "x.txt");
        assert_eq!(unwrap_compression("d/e/x.md.gz"), "d/e/x.md");
        // exactly one: a double wrapper keeps the inner one for the NEXT
        // state's derives (one transform, one layer — no fixpoint loop)
        assert_eq!(unwrap_compression("x.txt.gz.gz"), "x.txt.gz");
        // no wrapper / bare wrapper / empty: unchanged
        assert_eq!(unwrap_compression("x.txt"), "x.txt");
        assert_eq!(unwrap_compression(".gz"), ".gz");
        assert_eq!(unwrap_compression(""), "");
    }

    /// The mime inference is TABLE-driven (single source with the
    /// scanner's routing), not a second map that can drift: everything the
    /// mime table knows infers; everything else is honestly octet-stream.
    /// Since M3-T4 a family may carry SEVERAL mimes and extensions (img:
    /// jpeg+png over jpg/jpeg/png) — the walk goes family by family through
    /// a REPRESENTATIVE extension, asserting the inferred mime is that
    /// family's canonical row (the first), exactly what
    /// `mime_for_short_key` answers for a short-key-only consumer.
    #[test]
    fn infer_mime_follows_the_short_key_table() {
        let mut seen: Vec<&str> = Vec::new();
        for (_mime, key) in MIME_TO_SHORT_KEY {
            if seen.contains(key) {
                continue; // one walk per family (the img family's second row)
            }
            seen.push(key);
            let Some((ext, _)) = super::super::extract::EXTENSION_ALIASES
                .iter()
                .find(|(_, k)| *k == *key)
            else {
                panic!("family {key} has no extension alias");
            };
            let name = format!("dir/file.{ext}");
            let canonical = super::super::extract::mime_for_short_key(key).unwrap();
            assert_eq!(infer_mime(&name), canonical, "{name}");
        }
        // the img family's aliases all infer the SAME canonical mime
        for ext in ["jpg", "jpeg", "png"] {
            assert_eq!(
                infer_mime(&format!("dir/file.{ext}")),
                "image/jpeg",
                "family canonical is the FIRST row"
            );
        }
        assert_eq!(infer_mime("x.gz"), "application/octet-stream");
        assert_eq!(infer_mime("no-extension"), "application/octet-stream");
        assert_eq!(infer_mime("x.unknownext"), "application/octet-stream");
    }
}
