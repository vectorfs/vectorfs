//! Shared CLI output contract (design.md §5.8, "agent 一等契约"), pinned for
//! the M1 T6 commands (`vf status` / `vf doctor` / `vf mcp`):
//!
//! - **stdout = results** — a human-readable report; `--json` emits the same
//!   information as a structured object carrying `schema_version`.
//! - **stderr = progress** ("vf: reading catalog…"), never results.
//! - **exit codes are grep-compatible**: 0 = healthy/normal, non-zero =
//!   abnormal, and every abnormal exit prints a structured
//!   `{code, message, hint}` triple (human line on stderr without `--json`,
//!   a JSON object on stderr with it). `hint` is always an executable action.
//! - Deterministic output: key-ordered, no timestamps beyond what the data
//!   itself carries, no interleaving.

/// Bump when the `--json` shape of the CLI surface changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// A structured abnormal-exit triple (`{code, message, hint}`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Failure {
    /// Short stable machine code, e.g. `NO_DATA_ROOT`, `BACKEND_UNREACHABLE`.
    pub code: &'static str,
    /// What went wrong (one line).
    pub message: String,
    /// Executable next action for an operator or agent.
    pub hint: String,
}

impl Failure {
    pub fn new(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
            hint: hint.into(),
        }
    }

    /// Emit on stderr in the contract form and exit non-zero.
    ///
    /// `json` selects the structured form (both are one-line greppable);
    /// `exit_code` follows the existing `vf` conventions: 2 = bad
    /// invocation/config, 3 = data root held by another writer, 1 = anything
    /// else the command was asked to inspect.
    pub fn fail(self, json: bool, exit_code: i32) -> ! {
        if json {
            eprintln!(
                "{}",
                serde_json::to_string(&self).expect("Failure serializes")
            );
        } else {
            eprintln!("vf: error [{}]: {}", self.code, self.message);
            eprintln!("vf: hint: {}", self.hint);
        }
        std::process::exit(exit_code);
    }
}

/// `1234567` -> `"1.2 MiB"` (binary prefixes, one decimal below 100).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Package version + commit for `vf --version` and /healthz. The commit is
/// baked in at compile time via `VF_GIT_HASH` (the SIMPLE option over a
/// build.rs: zero build-script machinery, CI exports the hash from its
/// checkout — `VF_GIT_HASH=$(git rev-parse --short HEAD) cargo build`).
/// Release builds without the variable show `commit unknown` instead of
/// failing; nothing at runtime depends on the hash.
pub fn version_string() -> String {
    match option_env!("VF_GIT_HASH") {
        Some(commit) => format!("{} (commit {commit})", env!("CARGO_PKG_VERSION")),
        None => format!("{} (commit unknown)", env!("CARGO_PKG_VERSION")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_shapes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_bytes(5u64 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn version_string_shape() {
        // compile-time env not set here: must still be well-formed
        let v = version_string();
        assert!(v.starts_with("0.1.0"), "{v}");
        assert!(v.contains("commit"), "{v}");
    }
}
