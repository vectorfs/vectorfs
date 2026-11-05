//! Shared input gates every extractor kind opens with (plugin-axis hardening,
//! 2026-11-05): the uniform empty-file and wrong-mime refusals.
//!
//! Before this module each kind hand-rolled the same two checks — four
//! copies of one template is drift surface (a fifth kind paraphrasing the
//! message would silently fork the contract). The helpers below reproduce
//! the historical messages BYTE-FOR-BYTE (tests pin them; `statusReason`
//! strings already persisted in markers must keep reading the same), so
//! this is a consolidation, not a behavior change.
//!
//! Contract these gates encode (extractor contract, `contract.rs`):
//! * **Empty file ⇒ Err** — uniform empty-source handling; "silently index
//!   nothing" is the failure mode it exists to prevent.
//! * **Wrong mime ⇒ Err, never a guess** — but an EMPTY mime is TOLERATED:
//!   mime-unspecified means "the bytes are the truth", which child documents
//!   (ChildDoc mime always set) and table-routed documents never hit today;
//!   the tolerance is defensive and uniform across kinds.

use super::PipelineError;

/// How a kind's mime gate accepts NON-empty mimes. The accepted-set
/// description rides into the error message (`accepts text/* or
/// empty/unknown`), so it is part of the pinned message surface.
pub enum Accept {
    /// Exactly one canonical mime (pdf, md).
    Exact(&'static str),
    /// A mime-type family by TYPE prefix (txt: `text/` — every text/*
    /// subtype). The constructor takes the PREFIX WITHOUT the `*`; the
    /// description appends it.
    Family(&'static str),
}

impl Accept {
    fn accepts(&self, mime: &str) -> bool {
        match self {
            Accept::Exact(m) => mime == *m,
            Accept::Family(p) => mime.starts_with(p),
        }
    }

    /// The `accepts …` phrase the wrong-mime error quotes.
    fn describes(&self) -> String {
        match self {
            Accept::Exact(m) => (*m).to_string(),
            Accept::Family(p) => format!("{p}*"),
        }
    }
}

/// The empty-file refusal: `kind` is the registry label ("txt", "pdf-text").
/// Byte-identical to the per-kind copies it replaced.
pub fn refuse_empty_input(kind: &str, bytes: &[u8]) -> Result<(), PipelineError> {
    if bytes.is_empty() {
        return Err(PipelineError::extract(format!(
            "empty input (0 bytes): the {kind} kind refuses empty files — the ingest \
                 worker marks the source FAILED instead of silently indexing nothing"
        )));
    }
    Ok(())
}

/// The wrong-mime refusal. An EMPTY mime passes (tolerated by contract —
/// see the module doc); a non-empty mime the `accepts` set rejects is a
/// routing bug, never a bad file, and the message says so.
pub fn refuse_foreign_mime(kind: &str, mime: &str, accepts: Accept) -> Result<(), PipelineError> {
    if !mime.is_empty() && !accepts.accepts(mime) {
        return Err(PipelineError::extract(format!(
            "{kind} kind cannot handle mime '{mime}' (accepts {} or empty/unknown); \
                 this is an extract-routing bug, not a bad file",
            accepts.describes()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_message_is_the_pinned_template() {
        let err = refuse_empty_input("txt", b"").unwrap_err().to_string();
        // byte-stable vs the pre-hardening per-kind copies (the persisted
        // statusReason surface must keep reading the same)
        assert_eq!(
            err,
            "extract failed: empty input (0 bytes): the txt kind refuses empty files — the \
             ingest worker marks the source FAILED instead of silently indexing nothing"
        );
    }

    #[test]
    fn foreign_mime_message_is_the_pinned_template() {
        let err = refuse_foreign_mime("pdf-text", "text/plain", Accept::Exact("application/pdf"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "pdf-text kind cannot handle mime 'text/plain' (accepts application/pdf or \
                 empty/unknown); this is an extract-routing bug, not a bad file"
            ),
            "{err}"
        );
    }

    #[test]
    fn empty_mime_is_tolerated_both_shapes() {
        // exact + family shapes both pass an unspecified mime
        assert!(refuse_foreign_mime("md", "", Accept::Exact("text/markdown")).is_ok());
        assert!(refuse_foreign_mime("txt", "", Accept::Family("text/*")).is_ok());
        // and accept their own mimes, of course
        assert!(refuse_foreign_mime("md", "text/markdown", Accept::Exact("text/markdown")).is_ok());
        assert!(refuse_foreign_mime("txt", "text/plain", Accept::Family("text/")).is_ok());
        // the family prefix is the TYPE prefix: every subtype passes
        assert!(refuse_foreign_mime("txt", "text/csv", Accept::Family("text/")).is_ok());
        assert!(refuse_foreign_mime("txt", "application/pdf", Accept::Family("text/")).is_err());
    }
}
