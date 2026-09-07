//! Full-tree rewind: materialize the target generation into a sibling temp
//! directory, atomically exchange it with the working tree, keep the old tree
//! in trash, and journal every phase so kill -9 leaves the repo fully-old or
//! fully-new — never mixed.

use std::path::{Component, Path, PathBuf};

use acyclic_fs::kernel::{FileKind, FilePayload, LogicalName, MetadataField, NameEncoding, NamespacePath};
use acyclic_fs::{materialize_checkout, ByteRange, MaterializeOptions};
use acyclic_fs::{CancellationToken, GenerationId, WorkCounters};
use serde::{Deserialize, Serialize};

use crate::store::{LocalCheckout, Store};
use crate::{EngineError, Result};

const MAXIMUM_DIRECTORY_ENTRIES: u32 = 1_024;
const MAXIMUM_EXTENT_SPANS: u32 = 65_536;
const TRANSFER_BYTES: u64 = 8 * 1024 * 1024;

/// What a completed rewind reports back.
#[derive(Clone, Debug)]
pub struct RewindOutcome {
    pub restored: GenerationId,
    /// Where the replaced tree went (trash, TTL-pruned).
    pub old_tree: PathBuf,
    /// User-facing caveat: open editors keep inodes from the old tree.
    pub warning: &'static str,
}

/// What a single-path restore did to the working tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreAction {
    /// The path now matches the checkpoint's content.
    Restored,
    /// The path was absent at the checkpoint and has been removed.
    Removed,
}

/// Result of [`restore_path`].
#[derive(Clone, Debug)]
pub struct RestoreOutcome {
    pub path: PathBuf,
    pub action: RestoreAction,
}

/// Restores ONE path (file, symlink, or directory subtree) from `target`
/// into the working tree, leaving every other path untouched. The content
/// is staged in a hidden sibling and swapped in atomically (directory
/// subtrees use the same exchange as a full rewind), so a crash leaves the
/// path either old or new. Called from the pipeline, which brackets it with
/// checkpoints so the timeline records the restore.
pub async fn restore_path(
    store: &Store,
    target: GenerationId,
    relative: &Path,
) -> Result<RestoreOutcome> {
    let components = validate_relative(relative)?;
    let destination = store.repo_root.join(relative);
    let parent = destination
        .parent()
        .ok_or_else(|| EngineError::Restore("path has no parent".into()))?;
    let name = destination
        .file_name()
        .ok_or_else(|| EngineError::Restore("path has no name".into()))?
        .to_string_lossy()
        .into_owned();

    let mut checkout = store.checkout_exact(target).await?;
    let limits = checkout.volume_config().limits;
    let cancel = CancellationToken::new();
    let namespace = namespace_path(&components, limits)?;
    let lookup = checkout
        .lookup_no_follow(&namespace, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(EngineError::fs("lookup"))?
        .value;

    let Some(record) = lookup.record else {
        // Faithful restore of an absent path: remove it if it exists now.
        return match std::fs::symlink_metadata(&destination) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    std::fs::remove_dir_all(&destination)?;
                } else {
                    std::fs::remove_file(&destination)?;
                }
                Ok(RestoreOutcome {
                    path: relative.to_path_buf(),
                    action: RestoreAction::Removed,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(EngineError::Restore(
                format!("{} does not exist at that checkpoint or in the tree", relative.display()),
            )),
            Err(error) => Err(error.into()),
        };
    };

    // Stage next to the destination so the final rename never crosses a
    // filesystem; the parent must exist (it did at the checkpoint, but the
    // tree may have lost it since).
    std::fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".{name}.acyclic-restore-{}", std::process::id()));
    let _ = remove_any(&staged);
    let written = write_node(&mut checkout, &namespace, record.kind, &record.payload, &staged, limits, &cancel).await;
    if let Err(error) = written {
        let _ = remove_any(&staged);
        return Err(error);
    }

    // Swap in. An existing destination is exchanged atomically and the old
    // node discarded; an absent one is a plain rename.
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => {
            if let Err(error) = atomic_exchange(&destination, &staged) {
                let _ = remove_any(&staged);
                return Err(error);
            }
            let _ = remove_any(&staged);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) = std::fs::rename(&staged, &destination) {
                let _ = remove_any(&staged);
                return Err(error.into());
            }
        }
        Err(error) => {
            let _ = remove_any(&staged);
            return Err(error.into());
        }
    }
    Ok(RestoreOutcome {
        path: relative.to_path_buf(),
        action: RestoreAction::Restored,
    })
}

/// Writes one checkpoint node (recursively for directories) to a fresh host
/// path. Modes are applied; mtimes are not (same contract as a full rewind).
fn write_node<'a>(
    checkout: &'a mut LocalCheckout,
    namespace: &'a NamespacePath,
    kind: FileKind,
    payload: &'a FilePayload,
    host: &'a Path,
    limits: acyclic_fs::model::VolumeLimits,
    cancel: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        match kind {
            FileKind::Regular => {
                let length = match payload {
                    FilePayload::InlineRegular(inline) => inline.as_bytes().len() as u64,
                    FilePayload::Regular { logical_bytes, .. } => *logical_bytes,
                    _ => return Err(EngineError::Restore("regular file with foreign payload".into())),
                };
                let mut file = std::fs::File::create(host)?;
                let chunk = TRANSFER_BYTES.min(limits.maximum_read_bytes.max(1));
                let mut offset = 0;
                while offset < length {
                    let take = chunk.min(length - offset);
                    let read = checkout
                        .read_file_range(
                            namespace,
                            ByteRange { offset, length: take },
                            WorkCounters::UNBOUNDED,
                            cancel,
                        )
                        .await
                        .map_err(EngineError::fs("read file range"))?
                        .value;
                    use std::io::Write;
                    file.write_all(&read.bytes)?;
                    offset += take;
                }
                file.sync_all()?;
                drop(file);
                apply_mode(checkout, namespace, host, cancel).await
            }
            FileKind::SymbolicLink => {
                let target = checkout
                    .read_symbolic_link(namespace, WorkCounters::UNBOUNDED, cancel)
                    .await
                    .map_err(EngineError::fs("read symlink"))?
                    .value;
                create_symlink(&target, host)
            }
            FileKind::Directory => {
                std::fs::create_dir(host)?;
                let mut entries = Vec::new();
                let mut after = None;
                loop {
                    let page = checkout
                        .list_directory_records(
                            namespace,
                            after.as_ref(),
                            MAXIMUM_DIRECTORY_ENTRIES,
                            WorkCounters::UNBOUNDED,
                            cancel,
                        )
                        .await
                        .map_err(EngineError::fs("list directory"))?
                        .value;
                    for entry in &page.entries {
                        entries.push((entry.name.clone(), entry.record.kind, entry.record.payload));
                    }
                    match page.entries.last() {
                        Some(last) if page.has_more => after = Some(last.name.clone()),
                        _ => break,
                    }
                }
                for (name, kind, payload) in entries {
                    let mut components = namespace.components().to_vec();
                    components.push(name.clone());
                    let child = NamespacePath::new(components, limits)
                        .map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))?;
                    let child_host = host.join(logical_to_os(&name));
                    write_node(checkout, &child, kind, &payload, &child_host, limits, cancel).await?;
                }
                apply_mode(checkout, namespace, host, cancel).await
            }
            other => Err(EngineError::Restore(format!(
                "cannot restore a {other:?} node (only files, symlinks, and directories)"
            ))),
        }
    })
}

async fn apply_mode(
    checkout: &mut LocalCheckout,
    namespace: &NamespacePath,
    host: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let metadata = checkout
        .read_metadata(namespace, WorkCounters::UNBOUNDED, cancel)
        .await
        .map_err(EngineError::fs("read metadata"))?
        .value;
    #[cfg(unix)]
    if let MetadataField::Value(mode) = metadata.posix_mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(host, std::fs::Permissions::from_mode(mode & 0o7777))?;
    }
    #[cfg(not(unix))]
    let _ = (metadata, host);
    Ok(())
}

fn validate_relative(relative: &Path) -> Result<Vec<Vec<u8>>> {
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => components.push(os_to_bytes(name)),
            Component::CurDir => {}
            _ => {
                return Err(EngineError::Restore(format!(
                    "{}: path must be relative to the repo root and stay inside it",
                    relative.display()
                )))
            }
        }
    }
    if components.is_empty() {
        return Err(EngineError::Restore(
            "restoring the whole tree is `acyclic rewind`, not a path restore".into(),
        ));
    }
    Ok(components)
}

fn namespace_path(
    components: &[Vec<u8>],
    limits: acyclic_fs::model::VolumeLimits,
) -> Result<NamespacePath> {
    let names = components
        .iter()
        .map(|bytes| {
            LogicalName::new(NameEncoding::PosixBytes, bytes.clone(), limits.maximum_component_bytes)
                .map_err(|error| EngineError::Restore(format!("bad path component: {error:?}")))
        })
        .collect::<Result<Vec<_>>>()?;
    NamespacePath::new(names, limits).map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn os_to_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_to_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn logical_to_os(name: &LogicalName) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(name.as_bytes().to_vec())
}

#[cfg(not(unix))]
fn logical_to_os(name: &LogicalName) -> std::ffi::OsString {
    String::from_utf8_lossy(name.as_bytes()).into_owned().into()
}

#[cfg(unix)]
fn create_symlink(target: &[u8], host: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), host)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &[u8], _host: &Path) -> Result<()> {
    Err(EngineError::Restore("symlink restore is unix-only".into()))
}

/// Crash-recovery journal. Present on disk only while a swap is in flight.
#[derive(Debug, Serialize, Deserialize)]
pub struct Journal {
    pub target_generation: String,
    pub repo_root: PathBuf,
    pub tmp: PathBuf,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    /// Materializing into tmp; repo untouched. Recovery: delete tmp.
    Materializing,
    /// Atomic exchange in flight (or two-step: repo moved aside to tmp2).
    /// Recovery: if repo missing, move tmp into place; else delete tmp.
    Swapping,
}

/// Executes a rewind against the store's repo. Called from the pipeline with
/// captures paused; the caller re-baselines afterwards.
pub async fn execute(
    store: &Store,
    target: GenerationId,
    trash_ttl_days: u32,
) -> Result<RewindOutcome> {
    let repo = &store.repo_root;
    let parent = repo
        .parent()
        .ok_or_else(|| EngineError::Restore("repo root has no parent".into()))?;
    let name = repo
        .file_name()
        .ok_or_else(|| EngineError::Restore("repo root has no name".into()))?
        .to_string_lossy()
        .into_owned();
    let nonce = std::process::id();
    let tmp = parent.join(format!(".{name}.acyclic-tmp-{nonce}"));
    let journal_path = store.paths.rewind_journal();

    // 1. Materialize the target into an empty sibling directory.
    std::fs::create_dir(&tmp)?;
    write_journal(
        &journal_path,
        &Journal {
            target_generation: hex::encode(target.digest().as_bytes()),
            repo_root: repo.clone(),
            tmp: tmp.clone(),
            phase: Phase::Materializing,
        },
    )?;
    let mut checkout = store.checkout_exact(target).await?;
    let cancel = CancellationToken::new();
    materialize_checkout(
        &mut checkout,
        &MaterializeOptions {
            destination: tmp.clone(),
            maximum_directory_entries: MAXIMUM_DIRECTORY_ENTRIES,
            maximum_extent_spans: MAXIMUM_EXTENT_SPANS,
            transfer_bytes: TRANSFER_BYTES,
        },
        WorkCounters::UNBOUNDED,
        &cancel,
    )
    .await
    .map_err(|error| {
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&journal_path);
        EngineError::Restore(format!("materialize: {error:?}"))
    })?;

    // 2. Atomic exchange: repo <-> tmp. After this the old tree is at `tmp`.
    write_journal(
        &journal_path,
        &Journal {
            target_generation: hex::encode(target.digest().as_bytes()),
            repo_root: repo.clone(),
            tmp: tmp.clone(),
            phase: Phase::Swapping,
        },
    )?;
    atomic_exchange(repo, &tmp)?;

    // 3. Old tree to trash (best effort: EXDEV falls back to a sibling path).
    let trash_root = store.paths.trash();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let trashed = trash_root.join(format!("{name}-{stamp}"));
    let old_tree = match std::fs::rename(&tmp, &trashed) {
        Ok(()) => trashed,
        Err(_) => {
            let sibling = parent.join(format!(".{name}.acyclic-trash-{stamp}"));
            std::fs::rename(&tmp, &sibling)?;
            sibling
        }
    };
    std::fs::remove_file(&journal_path)?;
    prune_trash(&trash_root, trash_ttl_days);

    Ok(RewindOutcome {
        restored: target,
        old_tree,
        warning: "reload your editor: open files still point at the replaced tree",
    })
}

/// Startup crash recovery. Reads the journal (if any) and finishes or unwinds
/// the interrupted rewind so the repo is whole before the pipeline baselines.
pub fn recover(journal_path: &Path) -> Result<Option<Journal>> {
    let text = match std::fs::read_to_string(journal_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: Journal = serde_json::from_str(&text)
        .map_err(|error| EngineError::Restore(format!("rewind journal: {error}")))?;
    match journal.phase {
        Phase::Materializing => {
            // Repo untouched; the partial tmp tree is garbage.
            let _ = std::fs::remove_dir_all(&journal.tmp);
        }
        Phase::Swapping => {
            if !journal.repo_root.exists() && journal.tmp.exists() {
                // Two-step fallback died between renames: finish it.
                std::fs::rename(&journal.tmp, &journal.repo_root)?;
            } else {
                // Exchange is atomic: repo is whole; tmp holds either the old
                // tree (swap done — keep it out of the way) or the unused new
                // tree (swap never happened). Either way it is not the repo.
                let _ = std::fs::remove_dir_all(&journal.tmp);
            }
        }
    }
    std::fs::remove_file(journal_path)?;
    Ok(Some(journal))
}

fn write_journal(path: &Path, journal: &Journal) -> Result<()> {
    let text = serde_json::to_string(journal)
        .map_err(|error| EngineError::Restore(format!("encode journal: {error}")))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    let file = std::fs::File::open(&tmp)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn prune_trash(trash_root: &Path, ttl_days: u32) {
    let Ok(entries) = std::fs::read_dir(trash_root) else {
        return;
    };
    let ttl = std::time::Duration::from_secs(u64::from(ttl_days) * 24 * 3600);
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().map(|age| age > ttl).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Atomically exchanges two directories on the same filesystem.
#[cfg(target_os = "macos")]
fn atomic_exchange(a: &Path, b: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a_c = std::ffi::CString::new(a.as_os_str().as_bytes())
        .map_err(|_| EngineError::Restore("path contains NUL".into()))?;
    let b_c = std::ffi::CString::new(b.as_os_str().as_bytes())
        .map_err(|_| EngineError::Restore("path contains NUL".into()))?;
    // SAFETY: both are live NUL-terminated paths; RENAME_SWAP exchanges them
    // atomically on APFS.
    let result = unsafe { libc::renamex_np(a_c.as_ptr(), b_c.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(EngineError::Restore(format!(
            "renamex_np: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(target_os = "linux")]
fn atomic_exchange(a: &Path, b: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a_c = std::ffi::CString::new(a.as_os_str().as_bytes())
        .map_err(|_| EngineError::Restore("path contains NUL".into()))?;
    let b_c = std::ffi::CString::new(b.as_os_str().as_bytes())
        .map_err(|_| EngineError::Restore("path contains NUL".into()))?;
    // SAFETY: both are live NUL-terminated paths; RENAME_EXCHANGE swaps them
    // atomically on filesystems that support it.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a_c.as_ptr(),
            libc::AT_FDCWD,
            b_c.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(EngineError::Restore(format!(
            "renameat2: {}",
            std::io::Error::last_os_error()
        )))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn journal(repo: &Path, tmp: &Path, phase: Phase) -> Journal {
        Journal {
            target_generation: "00".repeat(32),
            repo_root: repo.to_path_buf(),
            tmp: tmp.to_path_buf(),
            phase,
        }
    }

    fn write(path: &Path, value: &Journal) {
        std::fs::write(path, serde_json::to_string(value).expect("encode")).expect("write");
    }

    #[test]
    fn recover_finishes_interrupted_two_step_swap() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        std::fs::write(tmp.join("file.txt"), b"restored").expect("seed");
        let journal_path = work.path().join("journal.json");
        write(&journal_path, &journal(&repo, &tmp, Phase::Swapping));

        recover(&journal_path).expect("recover");
        assert_eq!(
            std::fs::read(repo.join("file.txt")).expect("read"),
            b"restored"
        );
        assert!(!tmp.exists());
        assert!(!journal_path.exists());
    }

    #[test]
    fn recover_discards_partial_materialization() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        std::fs::write(repo.join("keep.txt"), b"live").expect("seed");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        std::fs::write(tmp.join("partial.txt"), b"half").expect("seed");
        let journal_path = work.path().join("journal.json");
        write(&journal_path, &journal(&repo, &tmp, Phase::Materializing));

        recover(&journal_path).expect("recover");
        assert_eq!(std::fs::read(repo.join("keep.txt")).expect("read"), b"live");
        assert!(!tmp.exists());
        assert!(!journal_path.exists());
    }

    #[test]
    fn recover_with_no_journal_is_a_noop() {
        let work = tempfile::tempdir().expect("tempdir");
        assert!(recover(&work.path().join("missing.json"))
            .expect("recover")
            .is_none());
    }
}
