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

use acyclic_fs::kernel::{FileKind, LogicalName, NamespacePath};
use acyclic_fs::model::VolumeLimits;
use acyclic_fs::{CancellationToken, WatchBatch, WatchChange, WorkCounters};

use crate::store::LocalCheckout;
use crate::{EngineError, Result};

const PAGE_ENTRIES: u32 = 1_024;

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
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
        Ok(Self { prefixes })
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
        self.prefixes.iter().any(|prefix| path.is_within(prefix))
    }

    /// True when `path` is a strict ancestor of an excluded path (the repo
    /// root included). A capture hinted at such a path may re-walk the
    /// excluded subtree, so the checkout needs a scrub afterwards.
    fn is_ancestor(&self, path: &NamespacePath) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| path.depth() < prefix.depth() && prefix.is_within(path))
    }

    /// Drops hints the capture must not read and rewrites renames that
    /// cross the exclusion boundary so the uncovered side is re-examined.
    /// Returns the batch and whether a scrub is needed after capturing it.
    pub fn filter_batch(&self, batch: WatchBatch) -> (WatchBatch, bool) {
        if self.is_empty() {
            return (batch, false);
        }
        let WatchBatch::Changes {
            epoch,
            first_sequence,
            next_sequence,
            changes,
        } = batch
        else {
            return (batch, false);
        };
        let mut scrub = false;
        let mut kept = Vec::with_capacity(changes.len());
        for change in changes {
            match change {
                WatchChange::Created(path)
                | WatchChange::Modified(path)
                | WatchChange::MetadataChanged(path)
                | WatchChange::Removed(path) => {
                    if self.covers(&path) {
                        continue;
                    }
                    scrub |= self.is_ancestor(&path);
                    kept.push(WatchChange::Modified(path));
                }
                WatchChange::Renamed { from, to } => match (self.covers(&from), self.covers(&to)) {
                    (true, true) => {}
                    (true, false) => {
                        scrub |= self.is_ancestor(&to);
                        kept.push(WatchChange::Modified(to));
                    }
                    (false, true) => {
                        scrub |= self.is_ancestor(&from);
                        kept.push(WatchChange::Modified(from));
                    }
                    (false, false) => {
                        scrub |= self.is_ancestor(&from) || self.is_ancestor(&to);
                        kept.push(WatchChange::Renamed { from, to });
                    }
                },
            }
        }
        (
            WatchBatch::Changes {
                epoch,
                first_sequence,
                next_sequence,
                changes: kept,
            },
            scrub,
        )
    }

    /// Removes every excluded path that is present in the checkout. Returns
    /// how many rules had something to remove.
    pub async fn scrub(&self, checkout: &mut LocalCheckout) -> Result<u32> {
        if self.is_empty() {
            return Ok(0);
        }
        let config = checkout.volume_config();
        let limits = config.limits;
        let cancel = CancellationToken::new();
        let mut removed = 0;
        for path in &self.prefixes {
            let names = path.components().to_vec();
            let lookup = checkout
                .lookup_no_follow(path, WorkCounters::UNBOUNDED, &cancel)
                .await
                .map_err(EngineError::fs("exclusion lookup"))?
                .value;
            let Some(record) = lookup.record else {
                continue;
            };
            if record.kind == FileKind::Directory {
                remove_subtree(checkout, names, limits, &cancel).await?;
            }
            checkout
                .remove(path.clone(), None, WorkCounters::UNBOUNDED, &cancel)
                .await
                .map_err(EngineError::fs("exclusion remove"))?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Post-order removal of a directory's contents (the fs refuses to unbind a
/// non-empty directory). The directory binding itself is left to the caller.
fn remove_subtree<'a>(
    checkout: &'a mut LocalCheckout,
    directory: Vec<LogicalName>,
    limits: VolumeLimits,
    cancel: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let path = namespace(directory.clone(), limits)?;
        let mut entries = Vec::new();
        let mut after = None;
        loop {
            let page = checkout
                .list_directory_records(
                    &path,
                    after.as_ref(),
                    PAGE_ENTRIES,
                    WorkCounters::UNBOUNDED,
                    cancel,
                )
                .await
                .map_err(EngineError::fs("exclusion list"))?
                .value;
            for entry in &page.entries {
                entries.push((entry.name.clone(), entry.record.kind));
            }
            match page.entries.last() {
                Some(last) if page.has_more => after = Some(last.name.clone()),
                _ => break,
            }
        }
        for (name, kind) in entries {
            let mut child = directory.clone();
            child.push(name);
            if kind == FileKind::Directory {
                remove_subtree(checkout, child.clone(), limits, cancel).await?;
            }
            let child_path = namespace(child, limits)?;
            checkout
                .remove(child_path, None, WorkCounters::UNBOUNDED, cancel)
                .await
                .map_err(EngineError::fs("exclusion remove"))?;
        }
        Ok(())
    })
}

fn namespace(names: Vec<LogicalName>, limits: VolumeLimits) -> Result<NamespacePath> {
    NamespacePath::new(names, limits)
        .map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use acyclic_fs::{WatchEpoch, WatchSequence};

    fn rules(list: &[&str]) -> Exclusions {
        Exclusions::parse(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parse")
    }

    fn ns(path: &str) -> NamespacePath {
        let limits = VolumeLimits::default();
        let names = path
            .split('/')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let namespace = acyclic_fs::host_path_to_namespace(
                    Path::new(part),
                    crate::store::host_profile(),
                    limits,
                )
                .expect("name");
                namespace.components()[0].clone()
            })
            .collect();
        NamespacePath::new(names, limits).expect("path")
    }

    fn batch(changes: Vec<WatchChange>) -> WatchBatch {
        WatchBatch::Changes {
            epoch: WatchEpoch::from_u64(1),
            first_sequence: WatchSequence::from_u64(1),
            next_sequence: WatchSequence::from_u64(2),
            changes,
        }
    }

    fn changes(batch: &WatchBatch) -> &[WatchChange] {
        match batch {
            WatchBatch::Changes { changes, .. } => changes,
            WatchBatch::RescanRequired { .. } => panic!("rescan"),
        }
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
                PathBuf::from("secrets"),
                PathBuf::from("build/out")
            ]
        );
    }

    #[test]
    fn covered_hints_are_dropped_and_ancestors_demand_a_scrub() {
        let parsed = rules(&["secrets"]);
        let (filtered, scrub) = parsed.filter_batch(batch(vec![
            WatchChange::Created(ns("secrets/key.pem")),
            WatchChange::Modified(ns("src/main.rs")),
        ]));
        assert_eq!(
            changes(&filtered),
            &[WatchChange::Modified(ns("src/main.rs"))]
        );
        assert!(!scrub);

        let (filtered, scrub) = parsed.filter_batch(batch(vec![WatchChange::Modified(ns(""))]));
        assert_eq!(changes(&filtered).len(), 1);
        assert!(scrub, "a root hint may re-walk the excluded subtree");
    }

    #[test]
    fn renames_across_the_boundary_reexamine_the_uncovered_side() {
        let parsed = rules(&["secrets"]);
        let (filtered, _) = parsed.filter_batch(batch(vec![WatchChange::Renamed {
            from: ns("staging"),
            to: ns("secrets"),
        }]));
        assert_eq!(changes(&filtered), &[WatchChange::Modified(ns("staging"))]);

        let (filtered, _) = parsed.filter_batch(batch(vec![WatchChange::Renamed {
            from: ns("secrets"),
            to: ns("public"),
        }]));
        assert_eq!(changes(&filtered), &[WatchChange::Modified(ns("public"))]);

        let (filtered, _) = parsed.filter_batch(batch(vec![WatchChange::Renamed {
            from: ns("secrets/a"),
            to: ns("secrets/b"),
        }]));
        assert!(changes(&filtered).is_empty());
    }

    #[test]
    fn empty_rules_pass_batches_through_untouched() {
        let parsed = rules(&[]);
        let original = batch(vec![WatchChange::Created(ns("anything"))]);
        let (filtered, scrub) = parsed.filter_batch(original.clone());
        assert_eq!(filtered, original);
        assert!(!scrub);
    }
}
