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
    /// Auto-checkpoint after this much idle time (ms), if the watcher has
    /// pending changes. The safety net for hosts with no lifecycle-hook API
    /// (e.g. Claude Desktop over MCP): a checkpoint no host asked for, taken
    /// once things go quiet, so `acyclic mcp`'s tools aren't the only path
    /// to a checkpoint. Zero disables it (hook-driven hosts don't need it).
    pub auto_checkpoint_idle_ms: u64,
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
    /// Snapshot exclusions: repo-relative paths (a file, or a directory and
    /// everything under it) that never enter a checkpoint. For secrets and
    /// bulky generated state the store must not shadow. A full rewind
    /// carries the live copies over untouched. See `crate::exclude`.
    pub exclude: Vec<String>,
    /// Parameters the fork-decomposition skill reads via `acyclic policy`.
    pub decompose: Decompose,
    /// Content-merge knobs (`[merge]`).
    pub merge: Merge,
}

/// `[merge]` table: limits for content-level merges at promote time.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Merge {
    /// Largest file the three-way merge will read; bigger ones refuse.
    pub max_file_bytes: u64,
}

impl Default for Merge {
    fn default() -> Self {
        Self {
            max_file_bytes: crate::merge::DEFAULT_MAX_FILE_BYTES,
        }
    }
}

impl Merge {
    pub fn limits(&self) -> crate::merge::MergeLimits {
        crate::merge::MergeLimits {
            max_file_bytes: self.max_file_bytes,
        }
    }
}

/// Knobs for the `acyclic-fork-decompose` skill. The skill text is the
/// same everywhere; a team tunes these in `.acyclic/config.toml` under
/// `[decompose]`, and `/fork` arguments override them per invocation.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Decompose {
    /// Forks per race (2..=4 is the useful range; the engine caps at 16).
    pub fan_out: u32,
    /// Promote-then-refork rounds the root may run before checking in
    /// with the user.
    pub max_depth: u32,
    /// Total forks one task may create across all rounds.
    pub max_forks: u32,
    /// A fork may win only if its tests pass.
    pub require_tests: bool,
    /// The command every subagent runs inside its fork before reporting.
    /// None: the skill asks the subagent to infer it from the repo.
    pub test_command: Option<String>,
    /// How to break a tie between passing forks: "smallest-diff" |
    /// "first-passing" | "ask-user".
    pub tie_break: String,
}

impl Default for Decompose {
    fn default() -> Self {
        Self {
            fan_out: 3,
            max_depth: 2,
            max_forks: 8,
            require_tests: true,
            test_command: None,
            tie_break: "smallest-diff".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            quiesce_ms: 50,
            quiesce_cap_ms: 500,
            commit_every: 25,
            commit_idle_ms: 60_000,
            auto_checkpoint_idle_ms: 5_000,
            trash_ttl_days: 7,
            store_dir: None,
            dry_run: false,
            guarded_paths: Vec::new(),
            exclude: Vec::new(),
            decompose: Decompose::default(),
            merge: Merge::default(),
        }
    }
}

impl Config {
    /// Loads the repo's checked-in `.acyclic/config.toml` over machine
    /// defaults from `~/.config/acyclic/config.toml`. Missing files are fine.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let machine = std::env::var_os("HOME")
            .map(|home| Path::new(&home).join(format!(".config/{}/config.toml", crate::product::NAME)));
        Self::load_layered(machine.as_deref(), repo_root)
    }

    /// The layering itself, with an explicit machine-config path so tests
    /// (and future hosts) control every input.
    pub fn load_layered(machine: Option<&Path>, repo_root: &Path) -> Result<Self> {
        let mut config = Config::default();
        if let Some(machine) = machine {
            config = Self::merge_file(config, machine)?;
        }
        Self::merge_file(config, &repo_root.join(crate::product::repo_config_file()))
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
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
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
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "dry_run = true\nguarded_paths = [\".env\", \"migrations/\"]\n",
        )
        .expect("write");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert!(config.dry_run);
        assert_eq!(config.guarded_paths, vec![".env", "migrations/"]);
    }

    #[test]
    fn exclude_list_parses_from_repo_config() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "exclude = [\".env\", \"secrets/\"]\n",
        )
        .expect("write");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert_eq!(config.exclude, vec![".env", "secrets/"]);
        assert!(Config::default().exclude.is_empty());
    }

    #[test]
    fn decompose_table_overrides_defaults_and_keeps_the_rest() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "[decompose]\nfan_out = 2\ntest_command = \"cargo test\"\n",
        )
        .expect("write");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert_eq!(config.decompose.fan_out, 2);
        assert_eq!(config.decompose.test_command.as_deref(), Some("cargo test"));
        assert_eq!(config.decompose.max_depth, 2);
        assert_eq!(config.decompose.tie_break, "smallest-diff");
    }

    #[test]
    fn merge_table_overrides_the_size_cap() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "[merge]\nmax_file_bytes = 1024\n",
        )
        .expect("write");
        let config = Config::load_layered(None, repo.path()).expect("load");
        assert_eq!(config.merge.max_file_bytes, 1024);
        assert_eq!(config.merge.limits().max_file_bytes, 1024);
        assert_eq!(Config::default().merge.max_file_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn unknown_keys_are_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "quiesce_millis = 10\n",
        )
        .expect("write");
        assert!(Config::load_layered(None, repo.path()).is_err());
    }
}
