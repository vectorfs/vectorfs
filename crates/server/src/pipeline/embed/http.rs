//! Thin re-export shell: the shared TLS HTTP leaf lives in `vectorfs-common`
//! since the three-crate split (the `vf` client and this crate's openai kind
//! / model prefetch ride the SAME leaf — one proxy implementation, one TLS
//! provider decision). This file re-exports it at the historical path so
//! `pipeline::embed::http` consumers (openai.rs, models.rs, ingest) keep
//! compiling unchanged; new code may import `vectorfs_common::http` directly.
pub use vectorfs_common::http::*;
