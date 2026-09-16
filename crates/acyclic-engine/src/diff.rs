//! Blast-radius diff between two generations, keyed by path.
//!
//! `Volume::diff_generations` returns FileId-keyed changes with no path
//! strings, so v1 walks both generations' directory records and compares
//! records per path. Content addressing makes the comparison exact: equal
//! payload object ids mean equal content. (Merkle-guided walking that skips
//! identical subtrees needs an upstream cursor/path API — tracked.)

use std::collections::BTreeMap;
use std::path::PathBuf;

use acyclic_fs::kernel::{FileKind, NamespacePath};
use acyclic_fs::{CancellationToken, GenerationId, ObjectId, WorkCounters};

use crate::store::{LocalCheckout, Store};
use crate::{EngineError, Result};

const PAGE_ENTRIES: u32 = 1_024;

/// One changed path between two generations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub change: ChangeKind,
    pub file_kind: FileKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    MetadataOnly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordSummary {
    pub(crate) kind: FileKind,
    /// Full payload for content comparison (inline bytes included).
    /// `None` for directories: their payload changes with any descendant.
    pub(crate) payload: Option<acyclic_fs::kernel::FilePayload>,
    pub(crate) metadata: ObjectId,
}

impl RecordSummary {
    /// Same kind and same content; metadata (mode, times) is ignored.
    pub(crate) fn same_content(&self, other: &RecordSummary) -> bool {
        self.kind == other.kind && self.payload == other.payload
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
    let mut before_checkout = store.checkout_exact(before).await?;
    let mut after_checkout = store.checkout_exact(after).await?;
    let before_map = walk(&mut before_checkout).await?;
    let after_map = walk(&mut after_checkout).await?;

    let mut changes = Vec::new();
    for (path, summary) in &before_map {
        match after_map.get(path) {
            None => changes.push(FileChange {
                path: path.clone(),
                change: ChangeKind::Removed,
                file_kind: summary.kind,
            }),
            Some(other) if other == summary => {}
            Some(other) => {
                let change = if other.kind == summary.kind && other.payload == summary.payload {
                    ChangeKind::MetadataOnly
                } else {
                    ChangeKind::Modified
                };
                changes.push(FileChange {
                    path: path.clone(),
                    change,
                    file_kind: other.kind,
                });
            }
        }
    }
    for (path, summary) in &after_map {
        if !before_map.contains_key(path) {
            changes.push(FileChange {
                path: path.clone(),
                change: ChangeKind::Added,
                file_kind: summary.kind,
            });
        }
    }
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

/// Path → record summary of every entry in `generation` (directories included).
pub(crate) async fn summaries(
    store: &Store,
    generation: GenerationId,
) -> Result<BTreeMap<PathBuf, RecordSummary>> {
    let mut checkout = store.checkout_exact(generation).await?;
    walk(&mut checkout).await
}

/// Walks every directory record in a generation into path → record summary.
/// Directories themselves are included (metadata-only changes are visible).
async fn walk(checkout: &mut LocalCheckout) -> Result<BTreeMap<PathBuf, RecordSummary>> {
    let cancel = CancellationToken::new();
    // The volume's own limits, not the defaults: a store raises the
    // per-component byte budget on hosts whose names cost more than one
    // byte per character (see `names::maximum_component_bytes`), and a walk
    // built on the default would refuse paths the store happily holds.
    let limits = checkout.volume_config().limits;
    let mut result = BTreeMap::new();
    // (namespace components, os path) work queue, starting at the root.
    let mut queue: Vec<(Vec<acyclic_fs::kernel::LogicalName>, PathBuf)> =
        vec![(Vec::new(), PathBuf::new())];

    while let Some((components, os_path)) = queue.pop() {
        let directory = NamespacePath::new(components.clone(), limits)
            .map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))?;
        let mut after = None;
        loop {
            let page = checkout
                .list_directory_records(
                    &directory,
                    after.as_ref(),
                    PAGE_ENTRIES,
                    WorkCounters::UNBOUNDED,
                    &cancel,
                )
                .await
                .map_err(EngineError::fs("list directory"))?
                .value;
            for entry in &page.entries {
                let child_os = os_path.join(logical_to_os(&entry.name));
                let payload = if entry.record.kind == FileKind::Directory {
                    None
                } else {
                    Some(entry.record.payload)
                };
                result.insert(
                    child_os.clone(),
                    RecordSummary {
                        kind: entry.record.kind,
                        payload,
                        metadata: entry.record.metadata,
                    },
                );
                if entry.record.kind == FileKind::Directory {
                    let mut child_components = components.clone();
                    child_components.push(entry.name.clone());
                    queue.push((child_components, child_os));
                }
            }
            match page.entries.last() {
                Some(last) if page.has_more => after = Some(last.name.clone()),
                _ => break,
            }
        }
    }
    Ok(result)
}

fn logical_to_os(name: &acyclic_fs::kernel::LogicalName) -> std::ffi::OsString {
    crate::names::bytes_to_os(name.as_bytes())
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
