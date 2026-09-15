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
    /// Auto-checkpoint once the watcher has been quiet this long (ms) with
    /// changes no checkpoint has recorded. The safety net for hosts with no
    /// lifecycle-hook API (Claude Desktop over MCP, a plain AGENTS.md
    /// agent): a checkpoint no host asked for, so `acyclic mcp`'s tools are
    /// not the only path to one. On by default for every daemon because the
    /// daemon cannot know which host is driving it; on a hook-driven host
    /// the hooks drain the watcher first, so the tick finds nothing and
    /// records nothing. Zero disables it.
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
        let machine = std::env::var_os("HOME").map(|home| {
            Path::new(&home).join(format!(".config/{}/config.toml", crate::product::NAME))
        });
        Self::load_layered(machine.as_deref(), repo_root)
    }

    /// The layering itself, with an explicit machine-config path so tests
    /// (and future hosts) control every input.
    ///
    /// Merging is per key, not per file: the repo layer overrides only the
    /// keys it names, and a key set only in the machine layer survives. The
    /// obvious shape — deserialize each file into a whole `Config` — cannot
    /// do that, because `#[serde(default)]` makes "absent" and "set to the
    /// default" indistinguishable once parsed, so the later file would
    /// silently reset every key it omits.
    pub fn load_layered(machine: Option<&Path>, repo_root: &Path) -> Result<Self> {
        let mut merged = toml::value::Table::new();
        if let Some(machine) = machine {
            Self::merge_file(&mut merged, machine)?;
        }
        Self::merge_file(
            &mut merged,
            &repo_root.join(crate::product::repo_config_file()),
        )?;
        toml::Value::Table(merged)
            .try_into()
            .map_err(|error| EngineError::Config(error.to_string()))
    }

    /// Overlays one file's keys onto `merged`. A missing file is fine.
    fn merge_file(merged: &mut toml::value::Table, path: &Path) -> Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let named = |error: &dyn std::fmt::Display| {
            EngineError::Config(format!("{}: {error}", path.display()))
        };
        let table: toml::value::Table = toml::from_str(&text).map_err(|error| named(&error))?;
        // Validate this layer on its own so an unknown key or a bad type is
        // reported against the file that holds it. The merged value is
        // deserialized again by the caller; this pass only names the file.
        let _: Self = table
            .clone()
            .try_into()
            .map_err(|error: toml::de::Error| named(&error))?;
        overlay(merged, table);
        Ok(())
    }
}

/// Deep-merges `overlay` onto `base`: tables recurse, everything else
/// (scalars, arrays) replaces. Replacing arrays is what a reader expects of
/// `exclude` or `guarded_paths` — a repo list overrides the machine list
/// rather than appending to it.
fn overlay(base: &mut toml::value::Table, overlay_table: toml::value::Table) {
    for (key, value) in overlay_table {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                overlay(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
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

    /// The layering the doc comment and README promise: a key set only in
    /// the machine config survives a repo config that does not mention it.
    /// Before the per-key merge, parsing the repo file discarded the whole
    /// machine layer.
    #[test]
    fn machine_and_repo_layers_merge_per_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let machine = home.path().join("config.toml");
        std::fs::write(
            &machine,
            "quiesce_ms = 10\ncommit_every = 5\nstore_dir = \"/tmp/machine-stores\"\n\
             [decompose]\nfan_out = 4\nmax_depth = 7\n",
        )
        .expect("write");
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "commit_every = 9\ntrash_ttl_days = 3\n[decompose]\nfan_out = 2\n",
        )
        .expect("write");

        let config = Config::load_layered(Some(&machine), repo.path()).expect("load");
        // Machine-only keys survive.
        assert_eq!(config.quiesce_ms, 10);
        assert_eq!(config.store_dir.as_deref(), Some("/tmp/machine-stores"));
        // The repo layer wins where both name a key.
        assert_eq!(config.commit_every, 9);
        // Repo-only keys apply.
        assert_eq!(config.trash_ttl_days, 3);
        // Sub-tables merge per key too, rather than replacing wholesale.
        assert_eq!(config.decompose.fan_out, 2);
        assert_eq!(config.decompose.max_depth, 7);
        assert_eq!(
            config.decompose.max_forks,
            Config::default().decompose.max_forks
        );
        // Keys named by neither file keep their defaults.
        assert_eq!(config.quiesce_cap_ms, Config::default().quiesce_cap_ms);
    }

    /// An unknown key is still fatal, and the error names the file holding
    /// it rather than the merged result.
    #[test]
    fn a_bad_machine_layer_names_the_machine_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let machine = home.path().join("config.toml");
        std::fs::write(&machine, "quiesce_millis = 10\n").expect("write");
        let error =
            Config::load_layered(Some(&machine), repo.path()).expect_err("unknown key must fail");
        assert!(
            error.to_string().contains("config.toml"),
            "error should name the file: {error}"
        );
    }

    /// A list in the repo layer replaces the machine list rather than
    /// appending to it.
    #[test]
    fn repo_lists_replace_machine_lists() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let machine = home.path().join("config.toml");
        std::fs::write(&machine, "exclude = [\"machine/\"]\n").expect("write");
        std::fs::create_dir(repo.path().join(crate::product::repo_config_dir())).expect("dir");
        std::fs::write(
            repo.path().join(crate::product::repo_config_file()),
            "exclude = [\"repo/\"]\n",
        )
        .expect("write");
        let config = Config::load_layered(Some(&machine), repo.path()).expect("load");
        assert_eq!(config.exclude, vec!["repo/"]);
    }
}
