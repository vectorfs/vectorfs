//! Engine version observability (2026-11-05, user-adjudicated): WHICH
//! extraction/embedding engine versions produced the indexed data, vs which
//! this binary runs.
//!
//! Why this exists: an engine upgrade (pdfium binary, pdf-inspector crate,
//! onnxruntime) can change extraction/embedding output WITHOUT changing the
//! pipeline fingerprint — by longstanding ruling (design.md, the
//! normalize/mechanism precedent) the re-embed on engine change is the
//! OPERATOR's explicit call, never automatic. That ruling is sound (silent
//! re-embedding is worse), but it made the drift INVISIBLE: chunks from
//! engine N and engine N+1 could silently mix under one root. This module
//! is the visible half of that ruling:
//!
//! * [`engine_versions`] — what THIS binary runs, surfaced on /healthz,
//!   /health and `vf status`;
//! * the same map persisted on every completing [`crate::store::SourceMarker`]
//!   (`engines` field, additive schema) — so `vectorfs doctor` can count
//!   drifted sources and point at the remedy (`serve --reindex`).
//!
//! NOT in the pipeline fingerprint, deliberately: fingerprint drift blocks
//! startup; engine drift only WARNS. Re-embedding on an engine bump stays
//! the operator's call.
//!
//! Drift-proofing the constants themselves (the project's parity-test
//! idiom, zero new deps): the pins are asserted against
//! `scripts/runtime_lib_pins.sh` (the single source of truth for the .so
//! downloads) and the crate versions against the workspace `Cargo.lock` —
//! bump either without this module and CI goes red, not the runtime silent.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The pdfium BINARY pin this build expects (`PDFIUM_RELEASE` in
/// scripts/runtime_lib_pins.sh — the fetch script and the Dockerfile
/// install exactly this build; pdfium-render's `pdfium_7881` bindings gate
/// accepts at-or-below).
pub const PDFIUM_PIN: &str = "chromium/8066";

/// The ONNX Runtime BINARY pin (`ORT_RELEASE` — pairs with the `ort` crate
/// pin below; ort's dylib loader refuses mismatched minors, so the two
/// bump together or dlopen fails loudly).
pub const ONNXRUNTIME_PIN: &str = "1.31.0";

/// The `ort` crate version this build links (Cargo.lock).
pub const ORT_CRATE_VERSION: &str = "2.0.0-rc.15";

/// The `pdf-inspector` crate version this build links (Cargo.lock) — the
/// classify-then-extract PDF engine whose output shape its minors have
/// changed before (the golden corpus pins observed behavior).
pub const PDF_INSPECTOR_VERSION: &str = "1.25.2";

/// What THIS binary runs. Keys are stable identifiers (`marker.engines`
/// persists them); values are human-readable version strings.
pub fn engine_versions() -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    m.insert("pdfium", PDFIUM_PIN.to_string());
    m.insert(
        "onnxruntime",
        format!(
            "{ONNXRUNTIME_PIN} (ort crate {ORT_CRATE_VERSION}, api v{})",
            {
                // compile-time const from ort-sys (ORT_API_VERSION) — no dlopen
                ort::MINOR_VERSION
            }
        ),
    );
    m.insert("pdf-inspector", PDF_INSPECTOR_VERSION.to_string());
    m
}

/// Owned-key form for persistence (the marker's serde shape).
pub fn engine_versions_owned() -> BTreeMap<String, String> {
    engine_versions()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// The workspace root for the parity tests below (`crates/server/../..`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The .so pins must equal the fetch script's single source of truth —
    /// bumping `runtime_lib_pins.sh` without this module (or vice versa)
    /// fails HERE instead of surfacing as silent marker drift.
    #[test]
    fn pins_match_the_fetch_script() {
        let script = std::fs::read_to_string(workspace_root().join("scripts/runtime_lib_pins.sh"))
            .expect("runtime_lib_pins.sh readable from the test");
        let assignment = |var: &str| {
            script
                .lines()
                .find(|l| l.starts_with(var))
                .unwrap_or_else(|| panic!("{var} assignment not found"))
                .to_string()
        };
        assert!(
            assignment("PDFIUM_RELEASE").contains(&format!("\"{PDFIUM_PIN}\"")),
            "PDFIUM_PIN drifted from runtime_lib_pins.sh"
        );
        assert!(
            assignment("ORT_RELEASE").contains(&format!("\"v{ONNXRUNTIME_PIN}\"")),
            "ONNXRUNTIME_PIN drifted from runtime_lib_pins.sh"
        );
    }

    /// The engine CRATE versions must equal the workspace lockfile — a
    /// `cargo update` of pdf-inspector/ort without touching this module
    /// fails here (the marker would otherwise record a stale version).
    #[test]
    fn crate_versions_match_the_lockfile() {
        let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock"))
            .expect("workspace Cargo.lock readable from the test");
        let version_of = |name: &str| -> String {
            let needle = format!("name = \"{name}\"");
            let mut lines = lock.lines();
            while let Some(l) = lines.next() {
                if l.trim() == needle {
                    if let Some(v) = lines.next() {
                        if let Some(v) = v.trim().strip_prefix("version = ") {
                            return v.trim_matches('"').to_string();
                        }
                    }
                }
            }
            panic!("{name} not found in Cargo.lock");
        };
        assert_eq!(version_of("pdf-inspector"), PDF_INSPECTOR_VERSION);
        assert_eq!(version_of("ort"), ORT_CRATE_VERSION);
    }

    #[test]
    fn the_table_names_every_engine() {
        // the observability surface is closed: exactly the three engines
        // whose output shape matters (pdfium extraction, lopdf-classify
        // extraction, onnxruntime embedding), in BTreeMap key order
        let keys: Vec<&str> = engine_versions().keys().copied().collect();
        assert_eq!(keys, ["onnxruntime", "pdf-inspector", "pdfium"]);
        for (_k, v) in engine_versions() {
            assert!(!v.is_empty());
        }
    }
}
