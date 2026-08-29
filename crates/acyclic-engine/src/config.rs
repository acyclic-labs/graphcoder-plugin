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
        }
    }
}

impl Config {
    /// Loads the repo's checked-in `.acyclic/config.toml` over machine
    /// defaults from `~/.config/acyclic/config.toml`. Missing files are fine.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let mut config = Config::default();
        if let Some(home) = std::env::var_os("HOME") {
            let machine = Path::new(&home).join(".config/acyclic/config.toml");
            config = Self::merge_file(config, &machine)?;
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
