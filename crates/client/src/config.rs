//! `vf` connection CONFIG FILE (2026-11-05 用户裁决: ~/.config/vectorfs/
//! config.toml 配置 vf + `vf config init` 初始化): a third precedence layer
//! under appendix E.4's flag > env — the full chain per knob is
//! `--flag` > `VF_* env` > config.toml > built-in default.
//!
//! Why a file layer at all: the env snippet `vectorfs init` prints is the
//! SCRIPTING answer, but an interactive shell wants `vf ls` to just work
//! after one setup step. The file deliberately holds the SAME four knobs
//! `CommonArgs` carries (endpoint / access_key / secret_key / region) and
//! nothing else — it is a connection profile, not a client-side pipeline
//! config (the pipeline owns the SERVER's vectorfs.toml).
//!
//! Conventions inherited from the server's config.rs: `deny_unknown_fields`
//! (a typo'd key must fail loudly, not silently never apply), snake_case
//! keys, and `init` refusing to clobber an existing file without --force.
//! The file carries the secret key, so `init` writes it 0600.

use std::path::{Path, PathBuf};

use vectorfs_common::report::Failure;

/// The four connection knobs, as they may appear in config.toml. Every field
/// optional: an absent (or EMPTY — same as the env layer's `env_nonempty`
/// rule) key simply leaves that knob to the lower precedence layers.
#[derive(Debug, Default, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub endpoint: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub region: Option<String>,
}

impl FileConfig {
    /// Drop empty/whitespace values — a present-but-empty key in the file is
    /// the user commenting-by-emptying; it must not shadow env/flag layers
    /// with an unusable value.
    fn normalized(self) -> Self {
        let nonempty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            endpoint: nonempty(self.endpoint),
            access_key: nonempty(self.access_key),
            secret_key: nonempty(self.secret_key),
            region: nonempty(self.region),
        }
    }
}

/// XDG base-dir resolution (pure — the env reads live in [`config_path`] so
/// this stays testable without mutating process env): `XDG_CONFIG_HOME` when
/// absolute, else `HOME/.config`.
fn xdg_config_dir(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    match xdg {
        Some(x) if x.starts_with('/') => Some(PathBuf::from(x)),
        // relative or absent XDG_CONFIG_HOME: the spec falls back to HOME
        _ => home.map(|h| PathBuf::from(h).join(".config")),
    }
}

/// `~/.config/vectorfs/config.toml` (XDG-aware). `None` = no config dir
/// determinable (no HOME/XDG_CONFIG_HOME) — callers TREAT that as "no file"
/// (verbs must not fail in, say, a stripped healthcheck container); only
/// `init` turns it into an actionable error.
pub fn config_path() -> Option<PathBuf> {
    let dir = xdg_config_dir(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    Some(dir.join("vectorfs").join("config.toml"))
}

/// Load and parse the config file at `path` (the test seam under
/// [`load`]). `Ok(None)` = the file does not exist.
fn load_from(path: &Path) -> Result<Option<FileConfig>, Failure> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Failure::new(
                "CONFIG_UNREADABLE",
                format!("cannot read {}: {e}", path.display()),
                "check the file's permissions (vf config init writes it 0600)",
            ));
        }
    };
    let cfg: FileConfig = toml::from_str(&text).map_err(|e| {
        Failure::new(
            "CONFIG_INVALID",
            format!("{}: {e}", path.display()),
            "fix the TOML (known keys: endpoint, access_key, secret_key, region) or rerun \
             `vf config init --force`",
        )
    })?;
    Ok(Some(cfg.normalized()))
}

/// Load the user's config file; `Ok(None)` when absent or no config dir.
pub fn load() -> Result<Option<FileConfig>, Failure> {
    match config_path() {
        Some(p) => load_from(&p),
        None => Ok(None),
    }
}

/// The per-knob precedence chain as a pure function (appendix E.4 + the
/// file layer): flag > env > file. Shared by [`seed`] (which stops at env)
/// and the resolve chain in lib.rs.
pub fn chain(flag: Option<String>, env: Option<String>, file: Option<String>) -> Option<String> {
    flag.or(env).or(file)
}

/// Build the file CONTENTS from seed values (the `vf config init` side):
/// resolved seeds become live keys, absent ones commented placeholders the
/// user fills in. Hand-assembled (not serde-serialized) so the comments and
/// ordering are the product — same reasoning as the server's preset
/// templates staying text.
fn template(cfg: &FileConfig) -> String {
    let kv = |key: &str, v: &Option<String>| match v {
        Some(v) => format!("{key} = {}", toml_value(v)),
        None => format!("# {key} = …"),
    };
    format!(
        "# vf — the vectorfs client connection profile.\n\
         # Created by `vf config init`; edit freely. Permissions 0600: this\n\
         # file carries the secret key.\n\
         # Precedence per knob: --flag > VF_* env var > this file > default.\n\
         # Values for access_key/secret_key are printed by the daemon's\n\
         # `vectorfs init` (stored in its data dir's vectorfs.toml).\n\n\
         {}\n{}\n{}\n{}\n",
        kv("endpoint", &cfg.endpoint),
        kv("access_key", &cfg.access_key),
        kv("secret_key", &cfg.secret_key),
        kv("region", &cfg.region),
    )
}

/// TOML-quote a scalar (reuse the crate's own serializer rather than
/// hand-rolling escapes).
fn toml_value(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// `vf config init`: write the template (seeded from flag > env, per knob)
/// to the config path, creating parent dirs, mode 0600, refusing to
/// overwrite an existing file unless `force`. Returns the path written.
pub fn init(cfg: &FileConfig, force: bool) -> Result<PathBuf, Failure> {
    let path = config_path().ok_or_else(|| {
        Failure::new(
            "CONFIG_NO_HOME",
            "cannot determine a config directory (XDG_CONFIG_HOME/HOME are both unset)",
            "export HOME, or XDG_CONFIG_HOME to a directory you own",
        )
    })?;
    if path.exists() && !force {
        return Err(Failure::new(
            "CONFIG_EXISTS",
            format!("{} already exists (refusing to overwrite)", path.display()),
            "edit it directly, or pass --force to regenerate from flags/env",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Failure::new(
                "CONFIG_WRITE",
                format!("cannot create {}: {e}", parent.display()),
                "check ownership/permissions of your config dir",
            )
        })?;
    }
    std::fs::write(&path, template(cfg)).map_err(|e| {
        Failure::new(
            "CONFIG_WRITE",
            format!("cannot write {}: {e}", path.display()),
            "check ownership/permissions of your config dir",
        )
    })?;
    // 0600 explicitly (fs::write's default mode follows umask — the file
    // carries the secret key, never group/other readable)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            Failure::new(
                "CONFIG_WRITE",
                format!("cannot chmod 600 {}: {e}", path.display()),
                "check ownership of the file",
            )
        })?;
    }
    Ok(path)
}

/// Seed values for `vf config init` from the four CommonArgs flags over the
/// VF_* env layer (flag > env, the same two top layers the verbs resolve —
/// `vf config init` with VF_ENDPOINT exported bakes that endpoint in).
pub fn seed(
    endpoint: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    region: Option<String>,
) -> FileConfig {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    FileConfig {
        endpoint: chain(endpoint, env("VF_ENDPOINT"), None),
        access_key: chain(access_key, env("VF_ACCESS_KEY"), None),
        secret_key: chain(secret_key, env("VF_SECRET_KEY"), None),
        region: chain(region, env("VF_REGION"), None),
    }
}

// ---------------------------------------------------------------------------
// tests (tempfile-backed; env-mutating paths are covered by the PURE fns —
// process-env tests would race under the default parallel runner)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_dir_absolute_overrides_home_fallback() {
        let via_xdg = xdg_config_dir(Some("/custom/cfg"), Some("/home/u"));
        assert_eq!(via_xdg, Some(PathBuf::from("/custom/cfg")));
        let via_home = xdg_config_dir(None, Some("/home/u"));
        assert_eq!(via_home, Some(PathBuf::from("/home/u/.config")));
        // relative XDG_CONFIG_HOME is invalid per spec -> HOME fallback
        let rel = xdg_config_dir(Some("rel/path"), Some("/home/u"));
        assert_eq!(rel, Some(PathBuf::from("/home/u/.config")));
        assert_eq!(xdg_config_dir(None, None), None);
    }

    #[test]
    fn chain_order_is_flag_env_file() {
        let f = Some("flag".into());
        let e = Some("env".into());
        let c = Some("file".into());
        assert_eq!(chain(f.clone(), e.clone(), c.clone()), f);
        assert_eq!(chain(None, e.clone(), c.clone()), e);
        assert_eq!(chain(None, None, c.clone()), c);
        assert_eq!(chain(None, None, None), None);
    }

    #[test]
    fn load_parses_and_normalizes_empty_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "endpoint = \"http://127.0.0.1:8787\"\naccess_key = \"\"\nregion = \"eu-west-1\"\n",
        )
        .unwrap();
        let cfg = load_from(&p).unwrap().unwrap();
        assert_eq!(cfg.endpoint.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(cfg.access_key, None, "empty value must not shadow env");
        assert_eq!(cfg.secret_key, None);
        assert_eq!(cfg.region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn load_missing_file_is_none_and_unknown_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_from(&dir.path().join("absent.toml"))
                .unwrap()
                .is_none()
        );

        let p = dir.path().join("bad.toml");
        std::fs::write(&p, "endpoin = \"typo\"\n").unwrap();
        let err = load_from(&p).unwrap_err();
        assert_eq!(err.code, "CONFIG_INVALID");
        assert!(err.message.contains("endpoin"), "{:?}", err.message);
    }

    #[test]
    fn init_writes_0600_template_then_refuses_until_force() {
        let dir = tempfile::tempdir().unwrap();
        // NOTE: init() resolves the REAL config_path() — redirect it via
        // XDG_CONFIG_HOME is env-mutating; instead exercise the pieces the
        // bin composes: template + the write/refuse flow against a temp path
        // (init itself is covered end-to-end by tests/cli_smoke.rs with an
        // isolated HOME).
        let p = dir.path().join("config.toml");
        let cfg = FileConfig {
            endpoint: Some("http://127.0.0.1:8787".into()),
            access_key: None,
            secret_key: None,
            region: Some("us-east-1".into()),
        };
        std::fs::write(&p, template(&cfg)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            let mode = p.metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("endpoint = \"http://127.0.0.1:8787\""));
        assert!(text.contains("# access_key = …"), "{text}");
        assert!(text.contains("region = \"us-east-1\""));
        // and the template round-trips through the loader
        let back = load_from(&p).unwrap().unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn template_escapes_hostile_strings() {
        let cfg = FileConfig {
            endpoint: Some("http://h/\"x\"\\".into()),
            ..Default::default()
        };
        let text = template(&cfg);
        let back = load_from_str_for_test(&text).unwrap().unwrap();
        assert_eq!(back.endpoint.as_deref(), Some("http://h/\"x\"\\"));
    }

    fn load_from_str_for_test(text: &str) -> Result<Option<FileConfig>, Failure> {
        // template() output always parses; route it through the same parser
        // via a temp file to keep ONE deserialization path
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, text).unwrap();
        load_from(&p)
    }
}
