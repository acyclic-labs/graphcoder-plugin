//! Layered configuration: machine defaults ← checked-in repo config.

use serde::Deserialize;
use std::path::Path;

use crate::{EngineError, Result};

/// Effective engine configuration. Every field has a safe default; zero
/// config is a supported state.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Watcher quiet window before a capture runs (ms).
    pub quiesce_ms: u64,
    /// Hard cap on the quiet-window wait (ms).
    pub quiesce_cap_ms: u64,
    /// Authority commit after this many checkpoints.
    pub commit_every: u32,
    /// Authority commit after this much idle time (ms).
    pub commit_idle_ms: u64,
    /// Days a rewound-away tree is kept in the store's trash.
    pub trash_ttl_days: u32,
    /// Override for the store directory (defaults to the per-machine root).
    pub store_dir: Option<String>,
    /// Safe Mode: root every session in a fork by default, gated on an
    /// approved diff before anything reaches the real tree.
    pub dry_run: bool,
    /// Safe Mode: path prefixes (relative to the repo root) no fork or
    /// scratch tree may write to, enforced at the native mount layer.
    pub guarded_paths: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            quiesce_ms: 50,
            quiesce_cap_ms: 500,
            commit_every: 25,
            commit_idle_ms: 60_000,
            trash_ttl_days: 7,
            store_dir: None,
            dry_run: false,
            guarded_paths: Vec::new(),
        }
    }
}

impl Config {
    /// Loads the repo's checked-in `.acyclic/config.toml` over machine
    /// defaults from `~/.config/acyclic/config.toml`. Missing files are fine.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let machine = std::env::var_os("HOME")
            .map(|home| Path::new(&home).join(".config/acyclic/config.toml"));
        Self::load_layered(machine.as_deref(), repo_root)
    }

    /// The layering itself, with an explicit machine-config path so tests
    /// (and future hosts) control every input.
    pub fn load_layered(machine: Option<&Path>, repo_root: &Path) -> Result<Self> {
        let mut config = Config::default();
        if let Some(machine) = machine {
            config = Self::merge_file(config, machine)?;
        }
        Self::merge_file(config, &repo_root.join(".acyclic/config.toml"))
    }

    fn merge_file(base: Config, path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| EngineError::Config(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(base),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_yield_defaults() {
        let repo = tempfile::tempdir().expect("tempdir");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn repo_config_overrides_defaults() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(".acyclic")).expect("dir");
        std::fs::write(
            repo.path().join(".acyclic/config.toml"),
            "quiesce_ms = 10\ncommit_every = 5\n",
        )
        .expect("write");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert_eq!(config.quiesce_ms, 10);
        assert_eq!(config.commit_every, 5);
        // Unspecified keys keep their defaults.
        assert_eq!(config.trash_ttl_days, Config::default().trash_ttl_days);
    }

    #[test]
    fn safe_mode_fields_parse_from_repo_config() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(".acyclic")).expect("dir");
        std::fs::write(
            repo.path().join(".acyclic/config.toml"),
            "dry_run = true\nguarded_paths = [\".env\", \"migrations/\"]\n",
        )
        .expect("write");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert!(config.dry_run);
        assert_eq!(config.guarded_paths, vec![".env", "migrations/"]);
    }

    #[test]
    fn unknown_keys_are_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(".acyclic")).expect("dir");
        std::fs::write(
            repo.path().join(".acyclic/config.toml"),
            "quiesce_millis = 10\n",
        )
        .expect("write");
        assert!(Config::load_layered(None, repo.path()).is_err());
    }
}
