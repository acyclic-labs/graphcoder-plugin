//! Blast-radius diff between two generations, keyed by path.
//!
//! The SDK's Merkle-aware change set resolves only changed identities to paths.

use std::path::PathBuf;

use acyclic_fs::kernel::FileKind;
use acyclic_fs::GenerationId;
use serde::{Deserialize, Serialize};

use crate::store::Store;
use crate::{EngineError, Result};

/// One changed path between two generations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub change: ChangeKind,
    pub file_kind: FileKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    #[serde(rename = "metadata")]
    MetadataOnly,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Modified => "modified",
            Self::MetadataOnly => "metadata",
        }
    }

    /// The one-letter marker used by the CLI's diff output.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Removed => "D",
            Self::Modified => "M",
            Self::MetadataOnly => "m",
        }
    }
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// Computes the path-keyed diff `before → after`.
pub async fn diff(
    store: &Store,
    before: GenerationId,
    after: GenerationId,
) -> Result<Vec<FileChange>> {
    if before == after {
        return Ok(Vec::new());
    }
    let before = store.generation(before).await?;
    let after = store.generation(after).await?;
    let set = before
        .diff_to(&after, u32::MAX)
        .await
        .map_err(EngineError::fs("diff generations"))?;
    let paths = set
        .changed_paths(u32::MAX)
        .await
        .map_err(EngineError::fs("resolve changed paths"))?;
    let mut changes = paths
        .into_iter()
        .filter_map(|path| {
            let (change, file_kind) = match (&path.before, &path.after) {
                (None, Some(after)) => (ChangeKind::Added, after.kind),
                (Some(before), None) => (ChangeKind::Removed, before.kind),
                (Some(before), Some(after)) => {
                    let change = if before.kind == after.kind && before.payload == after.payload {
                        ChangeKind::MetadataOnly
                    } else {
                        ChangeKind::Modified
                    };
                    (change, after.kind)
                }
                (None, None) => return None,
            };
            let path = path
                .path
                .components()
                .iter()
                .fold(PathBuf::new(), |mut path, component| {
                    path.push(crate::names::bytes_to_os(component.as_bytes()));
                    path
                });
            Some(FileChange {
                path,
                change,
                file_kind,
            })
        })
        .collect::<Vec<_>>();
    // Snapshots carry `.git` so rewind restores it, but a blast-radius
    // report is about the working tree: object and ref churn from ordinary
    // git commands would otherwise swamp the real changes.
    changes.retain(|change| !is_git_internal(&change.path));
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// `.git` itself or anything beneath it, at the repo root only.
pub(crate) fn is_git_internal(path: &std::path::Path) -> bool {
    path.components()
        .next()
        .is_some_and(|first| first.as_os_str() == ".git")
}

#[cfg(test)]
mod tests {
    use super::is_git_internal;
    use std::path::Path;

    #[test]
    fn only_root_git_dir_is_internal() {
        assert!(is_git_internal(Path::new(".git")));
        assert!(is_git_internal(Path::new(".git/HEAD")));
        assert!(is_git_internal(Path::new(".git/objects/ab/cd")));
        assert!(!is_git_internal(Path::new(".gitignore")));
        assert!(!is_git_internal(Path::new("src/.git/config")));
        assert!(!is_git_internal(Path::new("vendor/.gitkeep")));
        assert!(!is_git_internal(Path::new("a.txt")));
    }
}
