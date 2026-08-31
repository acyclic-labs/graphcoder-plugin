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
use acyclic_fs::model::VolumeLimits;
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
struct RecordSummary {
    kind: FileKind,
    /// Full payload for content comparison (inline bytes included).
    /// `None` for directories: their payload changes with any descendant.
    payload: Option<acyclic_fs::kernel::FilePayload>,
    metadata: ObjectId,
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
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

/// Walks every directory record in a generation into path → record summary.
/// Directories themselves are included (metadata-only changes are visible).
async fn walk(checkout: &mut LocalCheckout) -> Result<BTreeMap<PathBuf, RecordSummary>> {
    let cancel = CancellationToken::new();
    let limits = VolumeLimits::default();
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

#[cfg(unix)]
fn logical_to_os(name: &acyclic_fs::kernel::LogicalName) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(name.as_bytes().to_vec())
}

#[cfg(not(unix))]
fn logical_to_os(name: &acyclic_fs::kernel::LogicalName) -> std::ffi::OsString {
    String::from_utf8_lossy(name.as_bytes()).into_owned().into()
}
