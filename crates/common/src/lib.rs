//! `vectorfs-common` — the shared wire core of the three-crate split
//! (design.md §0 2026-11-05 ruling: server `vectorfs-server` may be large,
//! client `vectorfs-client` must stay small, and the two share exactly this).
//!
//! What lives here is decided by ONE rule: a module belongs in common iff
//! BOTH the server binary and the client binary need it. That is precisely
//! three things today:
//!
//! * [`sigv4`] — the signer (client) and the verifier (server gate) over ONE
//!   canonicalization, so the pair stays byte-exact by construction. Splitting
//!   or copying it is banned: the pre-split design comment ("the same
//!   canonicalization the gate verifies against, so the pair stays
//!   byte-exact by construction") is a load-bearing property, not a nicety.
//! * [`http`] — the proxy-aware TLS HTTP leaf: server-side embedder egress
//!   (openai kind, model prefetch) and the client's transport to the daemon.
//! * [`report`] — the CLI report vocabulary (`Failure` / `SCHEMA_VERSION` /
//!   `version_string`): both binaries answer with the same structured
//!   `{code, message, hint}` shape and the same baked version.
//!
//! Nothing engine-shaped (lance/arrow/ort/pdfium/axum) may appear here — the
//! client's small-binary guarantee is this crate's dependency list.
//!
//! Publishing: crate names are crates.io-ready (`vectorfs-common` /
//! `vectorfs-server` / `vectorfs-client`), but the actual `cargo publish` is
//! a separate, explicitly adjudicated step — NOT implied by this split.

pub mod http;
pub mod report;
pub mod sigv4;
