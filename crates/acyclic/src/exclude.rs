//! Snapshot exclusions: paths that never enter a checkpoint.
//!
//! The store deliberately captures what git ignores, so a declared secret
//! path (`.env`, `secrets/`) would otherwise live in history for longer than
//! it lives in the working tree. `exclude` in `.acyclic/config.toml` keeps
//! such paths out, enforced in three places:
//!
//! 1. Watcher hints at or under an excluded prefix are dropped before the
//!    capture runs, so the ordinary per-tool-call path never reads them.
//! 2. After any capture that may have re-walked an excluded path (the full
//!    baseline, or a hint on one of its ancestors), the path is scrubbed from
//!    the checkout before the generation is checkpointed.
//! 3. A full rewind carries the live excluded paths into the restored tree:
//!    no checkpoint holds them, so the working copy is the only copy.
//!
//! Exclusion is not purge. A generation captured before a path was excluded
//! still holds it, and at the pinned sdk revision the fs has no way to
//! release a retained generation, so nothing can be physically removed from
//! history. That gap is documented in docs/design/implementation-rewind.md.

use std::path::{Path, PathBuf};

use acyclic_fs::kernel::NamespacePath;
use acyclic_fs::CapturePolicy;

use crate::{EngineError, Result};

/// Parsed `exclude` rules: repo-relative path prefixes. A rule matches the
/// path itself and everything under it; `secrets` and `secrets/` are the
/// same rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exclusions {
    prefixes: Vec<NamespacePath>,
}

impl Exclusions {
    /// Parses the config list. Rules must be relative and stay inside the
    /// repo; an empty rule or one naming the repo root is refused, since
    /// excluding everything is the same as not running the engine.
    pub fn parse(patterns: &[String]) -> Result<Self> {
        let config = crate::store::volume_config();
        let mut prefixes = Vec::new();
        for pattern in patterns {
            let trimmed = pattern.trim().trim_end_matches('/');
            let prefix = acyclic_fs::host_path_to_namespace(
                Path::new(trimmed),
                config.profile,
                config.limits,
            )
            .map_err(|_| {
                EngineError::Config(format!(
                    "exclude rule {pattern:?} must be a non-empty relative path inside the repo"
                ))
            })?;
            prefixes.push(prefix);
        }
        prefixes.sort();
        prefixes.dedup();
        let mut canonical = Vec::<NamespacePath>::new();
        for prefix in prefixes {
            if canonical
                .last()
                .is_none_or(|ancestor| !prefix.is_within(ancestor))
            {
                canonical.push(prefix);
            }
        }
        Ok(Self {
            prefixes: canonical,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Every rule as a repo-relative host path.
    pub fn host_paths(&self) -> Vec<PathBuf> {
        self.prefixes
            .iter()
            .filter_map(|prefix| acyclic_fs::namespace_to_host_path(prefix).ok())
            .collect()
    }

    /// True when `relative` is an excluded path or lies under one.
    pub fn covers_host(&self, relative: &Path) -> bool {
        let config = crate::store::volume_config();
        acyclic_fs::host_path_to_namespace(relative, config.profile, config.limits)
            .is_ok_and(|path| self.covers(&path))
    }

    /// True when `path` is an excluded path or lies under one.
    pub fn covers(&self, path: &NamespacePath) -> bool {
        let candidate = self
            .prefixes
            .partition_point(|prefix| prefix <= path)
            .checked_sub(1)
            .and_then(|index| self.prefixes.get(index));
        candidate.is_some_and(|prefix| path.is_within(prefix))
    }

    /// Canonical SDK capture policy shared by baseline, watcher, and direct
    /// subtree reconciliation.
    pub fn capture_policy(&self) -> Result<CapturePolicy> {
        CapturePolicy::excluding(self.prefixes.clone())
            .map_err(|error| EngineError::Fs(format!("capture policy: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(list: &[&str]) -> Exclusions {
        Exclusions::parse(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parse")
    }

    #[test]
    fn rules_normalize_and_reject_escapes() {
        let parsed = rules(&[".env", "secrets/", "./build/out/"]);
        assert!(parsed.covers_host(Path::new(".env")));
        assert!(parsed.covers_host(Path::new("secrets/key.pem")));
        assert!(parsed.covers_host(Path::new("build/out")));
        assert!(!parsed.covers_host(Path::new("build")));
        assert!(!parsed.covers_host(Path::new(".env.example")));
        assert!(Exclusions::parse(&["../x".into()]).is_err());
        assert!(Exclusions::parse(&["/etc".into()]).is_err());
        assert!(Exclusions::parse(&["".into()]).is_err());
        assert!(Exclusions::parse(&["./".into()]).is_err());
        assert_eq!(
            parsed.host_paths(),
            vec![
                PathBuf::from(".env"),
                PathBuf::from("build/out"),
                PathBuf::from("secrets")
            ]
        );
        assert_eq!(
            rules(&["secrets/deep", "secrets", "secrets/deeper"]).host_paths(),
            vec![PathBuf::from("secrets")]
        );
    }
}
