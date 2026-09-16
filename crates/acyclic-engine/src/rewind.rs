//! Full-tree rewind: materialize the target generation into a sibling temp
//! directory, atomically exchange it with the working tree, keep the old tree
//! in trash, and journal every phase so kill -9 leaves the repo fully-old or
//! fully-new — never mixed.

use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use acyclic_fs::kernel::MetadataField;
use acyclic_fs::kernel::{FileKind, FilePayload, LogicalName, NamespacePath};
use acyclic_fs::{materialize_checkout, ByteRange, MaterializeOptions};
use acyclic_fs::{CancellationToken, GenerationId, WorkCounters};
use serde::{Deserialize, Serialize};

use crate::exclude::Exclusions;
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
    let root = store.repo_root.clone();
    restore_path_into(store, target, &root, relative).await
}

/// [`restore_path`] against an arbitrary root directory instead of the
/// working tree: a copy-mode fork's directory during a rebase.
pub async fn restore_path_into(
    store: &Store,
    target: GenerationId,
    root: &Path,
    relative: &Path,
) -> Result<RestoreOutcome> {
    let components = validate_relative(relative)?;
    let destination = root.join(relative);
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(EngineError::Restore(format!(
                    "{} does not exist at that checkpoint or in the tree",
                    relative.display()
                )))
            }
            Err(error) => Err(error.into()),
        };
    };

    // Stage next to the destination so the final rename never crosses a
    // filesystem; the parent must exist (it did at the checkpoint, but the
    // tree may have lost it since).
    std::fs::create_dir_all(parent)?;
    let staged = parent.join(format!(
        ".{name}.{}-restore-{}",
        crate::product::NAME,
        std::process::id()
    ));
    let _ = remove_any(&staged);
    let written = write_node(
        &mut checkout,
        &namespace,
        record.kind,
        &record.payload,
        &staged,
        limits,
        &cancel,
    )
    .await;
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
pub(crate) fn write_node<'a>(
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
                    _ => {
                        return Err(EngineError::Restore(
                            "regular file with foreign payload".into(),
                        ))
                    }
                };
                let mut file = std::fs::File::create(host)?;
                let chunk = TRANSFER_BYTES.min(limits.maximum_read_bytes.max(1));
                let mut offset = 0;
                while offset < length {
                    let take = chunk.min(length - offset);
                    let read = checkout
                        .read_file_range(
                            namespace,
                            ByteRange {
                                offset,
                                length: take,
                            },
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
                    write_node(
                        checkout,
                        &child,
                        kind,
                        &payload,
                        &child_host,
                        limits,
                        cancel,
                    )
                    .await?;
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

pub(crate) fn validate_relative(relative: &Path) -> Result<Vec<Vec<u8>>> {
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
        return Err(EngineError::Restore(format!(
            "restoring the whole tree is `{} rewind`, not a path restore",
            crate::product::NAME
        )));
    }
    Ok(components)
}

pub(crate) fn namespace_path(
    components: &[Vec<u8>],
    limits: acyclic_fs::model::VolumeLimits,
) -> Result<NamespacePath> {
    let names = components
        .iter()
        .map(|bytes| {
            LogicalName::new(
                crate::names::encoding(),
                bytes.clone(),
                limits.maximum_component_bytes,
            )
            .map_err(|error| EngineError::Restore(format!("bad path component: {error:?}")))
        })
        .collect::<Result<Vec<_>>>()?;
    NamespacePath::new(names, limits)
        .map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))
}

pub(crate) fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn os_to_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    crate::names::os_to_bytes(name)
}

fn logical_to_os(name: &LogicalName) -> std::ffi::OsString {
    crate::names::bytes_to_os(name.as_bytes())
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
    /// Excluded paths (repo-relative) moved from the live tree into `tmp`
    /// before the swap. Absent in journals written before exclusions.
    #[serde(default)]
    pub carried: Vec<PathBuf>,
}

/// Moves each `relative` path from `from` to `into`, replacing whatever the
/// materialized tree had there (the live copy wins). Stops at the first
/// failure; the caller unwinds with [`move_back`].
fn carry(from: &Path, into: &Path, relative: &[PathBuf]) -> Result<()> {
    for path in relative {
        let source = from.join(path);
        let destination = into.join(path);
        if std::fs::symlink_metadata(&source).is_err() {
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = remove_any(&destination);
        std::fs::rename(&source, &destination).map_err(|error| {
            EngineError::Restore(format!("carry excluded path {}: {error}", path.display()))
        })?;
    }
    Ok(())
}

/// Best-effort inverse of [`carry`]: every listed path present in `from`
/// and absent in `into` goes back. Used by crash recovery, where the only
/// wrong answer is losing a live copy.
fn move_back(from: &Path, into: &Path, relative: &[PathBuf]) {
    for path in relative {
        let source = from.join(path);
        let destination = into.join(path);
        if std::fs::symlink_metadata(&source).is_err()
            || std::fs::symlink_metadata(&destination).is_ok()
        {
            continue;
        }
        if let Some(parent) = destination.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::rename(&source, &destination);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    /// Materializing into tmp; repo untouched. Recovery: delete tmp.
    Materializing,
    /// Excluded paths moving from repo into tmp. Recovery: move back any
    /// that already moved, then delete tmp.
    Carrying,
    /// Atomic exchange in flight (or two-step: repo moved aside to tmp2).
    /// Recovery: if repo missing, move tmp into place; else delete tmp.
    Swapping,
}

/// Materializes `generation` into `destination`, which must not exist yet.
/// Used for copy-mode forks; a rewind does the same into its temp sibling.
pub async fn materialize_into(
    store: &Store,
    generation: GenerationId,
    destination: &Path,
) -> Result<()> {
    std::fs::create_dir(destination)?;
    let mut checkout = store.checkout_exact(generation).await?;
    let cancel = CancellationToken::new();
    materialize_checkout(
        &mut checkout,
        &MaterializeOptions {
            destination: destination.to_path_buf(),
            maximum_directory_entries: MAXIMUM_DIRECTORY_ENTRIES,
            maximum_extent_spans: MAXIMUM_EXTENT_SPANS,
            transfer_bytes: TRANSFER_BYTES,
        },
        WorkCounters::UNBOUNDED,
        &cancel,
    )
    .await
    .map_err(|error| {
        let _ = std::fs::remove_dir_all(destination);
        EngineError::Restore(format!("materialize: {error:?}"))
    })?;
    Ok(())
}

/// Executes a rewind against the store's repo. Called from the pipeline with
/// captures paused; the caller re-baselines afterwards. Excluded paths are
/// carried from the live tree into the restored one: no checkpoint holds
/// them, so the working copy is the only copy.
pub async fn execute(
    store: &Store,
    target: GenerationId,
    trash_ttl_days: u32,
    exclusions: &Exclusions,
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
    let tmp = parent.join(format!(".{name}.{}-tmp-{nonce}", crate::product::NAME));
    let journal_path = store.paths.rewind_journal();

    // 1. Materialize the target into an empty sibling directory. A tmp left
    // by an earlier attempt that failed before the swap is stale by
    // construction -- the name carries this daemon's pid, and a rewind that
    // got as far as the swap removes it -- so clear it rather than refusing
    // every later rewind with "already exists".
    let _ = remove_any(&tmp);
    std::fs::create_dir(&tmp).map_err(|error| {
        EngineError::Restore(format!("rewind: stage {}: {error}", tmp.display()))
    })?;
    write_journal(
        &journal_path,
        &Journal {
            target_generation: hex::encode(target.digest().as_bytes()),
            repo_root: repo.clone(),
            tmp: tmp.clone(),
            phase: Phase::Materializing,
            carried: Vec::new(),
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

    // 2. Carry the live excluded paths into the new tree. Journaled first,
    // so a crash mid-carry can move them back.
    let carried: Vec<PathBuf> = exclusions
        .host_paths()
        .into_iter()
        .filter(|relative| std::fs::symlink_metadata(repo.join(relative)).is_ok())
        .collect();
    if !carried.is_empty() {
        write_journal(
            &journal_path,
            &Journal {
                target_generation: hex::encode(target.digest().as_bytes()),
                repo_root: repo.clone(),
                tmp: tmp.clone(),
                phase: Phase::Carrying,
                carried: carried.clone(),
            },
        )?;
        if let Err(error) = carry(repo, &tmp, &carried) {
            // Undo what moved, then abandon the rewind with the repo whole.
            move_back(&tmp, repo, &carried);
            let _ = std::fs::remove_dir_all(&tmp);
            let _ = std::fs::remove_file(&journal_path);
            return Err(error);
        }
    }

    // 3. Atomic exchange: repo <-> tmp. After this the old tree is at `tmp`.
    write_journal(
        &journal_path,
        &Journal {
            target_generation: hex::encode(target.digest().as_bytes()),
            repo_root: repo.clone(),
            tmp: tmp.clone(),
            phase: Phase::Swapping,
            carried,
        },
    )?;
    atomic_exchange(repo, &tmp)?;

    // 4. Old tree to trash (best effort: EXDEV falls back to a sibling path).
    let trash_root = store.paths.trash();
    let old_tree = park_replaced_tree(&tmp, &trash_root, parent, &name)?;
    std::fs::remove_file(&journal_path)
        .map_err(|error| EngineError::Restore(format!("rewind: clear journal: {error}")))?;
    prune_trash(&trash_root, trash_ttl_days);

    Ok(RewindOutcome {
        restored: target,
        old_tree,
        warning: "reload your editor: open files still point at the replaced tree",
    })
}

/// Moves the replaced tree out of the way and returns where it landed.
///
/// The trash lives in the store, which can be on another volume than the
/// repo; a cross-device rename fails rather than copying, so a sibling of
/// the repo is the fallback. Either way the tree is off the repo path.
fn park_replaced_tree(tmp: &Path, trash_root: &Path, parent: &Path, name: &str) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let trashed = trash_root.join(format!("{name}-{stamp}"));
    if std::fs::rename(tmp, &trashed).is_ok() {
        return Ok(trashed);
    }
    let sibling = parent.join(format!(".{name}.{}-trash-{stamp}", crate::product::NAME));
    std::fs::rename(tmp, &sibling).map_err(|error| {
        EngineError::Restore(format!(
            "rewind: park the replaced tree at {}: {error}",
            sibling.display()
        ))
    })?;
    Ok(sibling)
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
        Phase::Carrying => {
            // Some excluded paths may already sit in tmp: bring them home,
            // then drop the unused new tree.
            move_back(&journal.tmp, &journal.repo_root, &journal.carried);
            let _ = std::fs::remove_dir_all(&journal.tmp);
        }
        Phase::Swapping => {
            // Windows swaps through a scratch name (see `atomic_exchange`);
            // whichever step it died on, the scratch holds a tree that one
            // of the branches below is about to supersede.
            #[cfg(windows)]
            let scratch = swap_scratch(&journal.repo_root);
            if !journal.repo_root.exists() && journal.tmp.exists() {
                // The swap died with the repo path vacated and the new tree
                // still parked at tmp: finish the move. The carried paths are
                // inside tmp and come along.
                std::fs::rename(&journal.tmp, &journal.repo_root)?;
            } else {
                // The repo path is whole, so it names exactly one tree and
                // tmp holds the other: the old tree (swap done — keep it out
                // of the way) or the unused new tree (swap never happened).
                // Carried paths live in whichever tree is new: if that is
                // still tmp, they must come back before tmp goes.
                move_back(&journal.tmp, &journal.repo_root, &journal.carried);
                let _ = std::fs::remove_dir_all(&journal.tmp);
            }
            // Whatever the scratch still holds has now been superseded by the
            // branch above, exactly as `tmp` is discarded there.
            #[cfg(windows)]
            if let Some(scratch) = scratch {
                let _ = remove_any(&scratch);
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
    // Durability before visibility: the journal only helps if it is on the
    // platter before the phase it describes begins.
    //
    // One writable handle carries all of it. `sync_all` is a `FlushFileBuffers`
    // on Windows, which needs write access -- flushing a handle from
    // `File::open` fails there with "access is denied" -- and the handle has
    // to be closed before the rename, because Windows will not rename a file
    // anyone still holds open.
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    };
    write().map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::Restore(format!("rewind: journal {}: {error}", path.display()))
    })?;
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
        if modified.elapsed().is_ok_and(|age| age > ttl) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The scratch name [`atomic_exchange`] swaps `a` through on Windows.
///
/// Derived from `a` rather than randomised so that [`recover`] can name it
/// without the journal having carried it, and so a crashed swap leaves at
/// most one predictable directory behind instead of one per attempt.
#[cfg(windows)]
pub(crate) fn swap_scratch(a: &Path) -> Option<PathBuf> {
    let parent = a.parent()?;
    let name = a.file_name()?.to_string_lossy().into_owned();
    Some(parent.join(format!(".{name}.{}-swap", crate::product::NAME)))
}

/// Exchanges two directories on the same filesystem.
///
/// **Not atomic on Windows.** There is no `RENAME_EXCHANGE` equivalent: NTFS
/// cannot swap two names in one operation, so this is three renames through
/// a scratch name in `a`'s directory. Each rename is atomic; the sequence is
/// not, and a crash can be observed between any two of them.
///
/// What still holds is the property rewind actually needs — the repo is
/// never a mixture of the two trees. Every intermediate state has the repo
/// path either absent or naming exactly one whole tree, and [`recover`]
/// resolves each of them from the journal: the `Swapping` arm finishes the
/// move when the repo path is missing, and clears the scratch either way.
/// A crash during a journal-less [`restore_path`] leaves the same scratch
/// behind, which the next swap of that path removes before it starts.
#[cfg(windows)]
fn atomic_exchange(a: &Path, b: &Path) -> Result<()> {
    let scratch =
        swap_scratch(a).ok_or_else(|| EngineError::Restore("swap path has no parent".into()))?;
    // A scratch left by an interrupted swap is stale by construction: the
    // recovery below never keeps it, so anything still here predates us.
    let _ = remove_any(&scratch);

    std::fs::rename(a, &scratch).map_err(|error| {
        EngineError::Restore(format!("swap: move aside {}: {error}", a.display()))
    })?;
    if let Err(error) = std::fs::rename(b, a) {
        // Nothing has been published yet; put `a` back and fail clean.
        let _ = std::fs::rename(&scratch, a);
        return Err(EngineError::Restore(format!(
            "swap: move {} into place: {error}",
            b.display()
        )));
    }
    std::fs::rename(&scratch, b).map_err(|error| {
        // `a` already holds the new tree, so the exchange has effectively
        // happened; only the old tree's parking spot is wrong. Leave the
        // scratch for recovery rather than unwinding a published swap.
        EngineError::Restore(format!("swap: park the replaced tree: {error}"))
    })
}

/// Atomically exchanges two directories on the same filesystem.
#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "renamex_np over two live NUL-terminated paths; RENAME_SWAP is atomic on APFS"
)]
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
#[allow(
    unsafe_code,
    reason = "renameat2 over two live NUL-terminated paths; RENAME_EXCHANGE is atomic where supported"
)]
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
            carried: Vec::new(),
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

    #[test]
    fn recover_mid_carry_returns_excluded_paths_to_the_repo() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir_all(repo.join("secrets")).expect("repo");
        std::fs::create_dir_all(tmp.join("secrets")).expect("tmp");
        // .env already moved into tmp; secrets/key.pem had not moved yet.
        std::fs::write(tmp.join(".env"), b"LIVE").expect("moved");
        std::fs::write(repo.join("secrets/key.pem"), b"KEY").expect("unmoved");
        let journal_path = work.path().join("journal.json");
        let mut entry = journal(&repo, &tmp, Phase::Carrying);
        entry.carried = vec![PathBuf::from(".env"), PathBuf::from("secrets/key.pem")];
        write(&journal_path, &entry);

        recover(&journal_path).expect("recover");
        assert_eq!(std::fs::read(repo.join(".env")).expect("back"), b"LIVE");
        assert_eq!(
            std::fs::read(repo.join("secrets/key.pem")).expect("kept"),
            b"KEY"
        );
        assert!(!tmp.exists(), "unused new tree removed");
        assert!(!journal_path.exists());
    }

    #[test]
    fn recover_before_the_swap_keeps_carried_paths_out_of_the_discarded_tree() {
        // Swapping phase, but the exchange never ran: repo is the old tree
        // (minus the carried paths), tmp is the new tree holding them.
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::write(repo.join("file.txt"), b"old").expect("old");
        std::fs::write(tmp.join("file.txt"), b"new").expect("new");
        std::fs::write(tmp.join(".env"), b"LIVE").expect("carried");
        let journal_path = work.path().join("journal.json");
        let mut entry = journal(&repo, &tmp, Phase::Swapping);
        entry.carried = vec![PathBuf::from(".env")];
        write(&journal_path, &entry);

        recover(&journal_path).expect("recover");
        assert_eq!(std::fs::read(repo.join("file.txt")).expect("repo"), b"old");
        assert_eq!(
            std::fs::read(repo.join(".env")).expect("carried back"),
            b"LIVE"
        );
        assert!(!tmp.exists());
    }

    #[test]
    fn recover_after_the_swap_leaves_carried_paths_in_the_new_tree() {
        // Exchange completed: repo is the new tree with the carried paths,
        // tmp is the old tree without them. Nothing moves; tmp goes.
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::write(repo.join("file.txt"), b"new").expect("new");
        std::fs::write(repo.join(".env"), b"LIVE").expect("carried");
        std::fs::write(tmp.join("file.txt"), b"old").expect("old");
        let journal_path = work.path().join("journal.json");
        let mut entry = journal(&repo, &tmp, Phase::Swapping);
        entry.carried = vec![PathBuf::from(".env")];
        write(&journal_path, &entry);

        recover(&journal_path).expect("recover");
        assert_eq!(std::fs::read(repo.join("file.txt")).expect("repo"), b"new");
        assert_eq!(
            std::fs::read(repo.join(".env")).expect("still here"),
            b"LIVE"
        );
        assert!(!tmp.exists());
    }

    #[test]
    fn journals_written_before_exclusions_still_decode() {
        let text =
            r#"{"target_generation":"00","repo_root":"/r","tmp":"/t","phase":"Materializing"}"#;
        let journal: Journal = serde_json::from_str(text).expect("decode");
        assert!(journal.carried.is_empty());
    }

    /// The contract every platform's exchange owes the caller, asserted
    /// against whichever implementation this host compiled: after it, each
    /// path names the other's tree. Windows reaches that through three
    /// renames rather than one syscall, so it is the arm most worth pinning.
    #[test]
    fn exchange_swaps_two_directories() {
        let work = tempfile::tempdir().expect("tempdir");
        let left = work.path().join("left");
        let right = work.path().join("right");
        std::fs::create_dir(&left).expect("left");
        std::fs::create_dir(&right).expect("right");
        std::fs::write(left.join("who.txt"), b"left").expect("seed left");
        std::fs::write(right.join("who.txt"), b"right").expect("seed right");

        atomic_exchange(&left, &right).expect("exchange");

        assert_eq!(std::fs::read(left.join("who.txt")).expect("left"), b"right");
        assert_eq!(
            std::fs::read(right.join("who.txt")).expect("right"),
            b"left"
        );
    }

    /// A failed exchange must leave the tree it was given untouched rather
    /// than half-moved. On Windows this exercises the unwind between the
    /// first and second rename, which is the window where the repo path is
    /// vacated and nothing has replaced it yet.
    #[test]
    fn a_failed_exchange_leaves_the_live_tree_whole() {
        let work = tempfile::tempdir().expect("tempdir");
        let live = work.path().join("live");
        std::fs::create_dir(&live).expect("live");
        std::fs::write(live.join("keep.txt"), b"precious").expect("seed");
        let missing = work.path().join("never-materialized");

        atomic_exchange(&live, &missing).expect_err("exchange must fail");

        assert!(live.is_dir(), "the live tree must still be a directory");
        assert_eq!(
            std::fs::read(live.join("keep.txt")).expect("content survives"),
            b"precious"
        );
        #[cfg(windows)]
        assert!(
            !swap_scratch(&live).expect("scratch path").exists(),
            "a failed exchange must not leave its scratch behind"
        );
    }

    /// Windows swaps through a scratch directory, so a crash can leave the
    /// repo path vacated with the new tree still at `tmp` and the old tree
    /// parked in the scratch. Recovery has to finish the move *and* clear the
    /// scratch, or the next rewind inherits a stale tree beside the repo.
    #[cfg(windows)]
    #[test]
    fn recover_clears_the_scratch_a_windows_swap_left() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        std::fs::write(tmp.join("file.txt"), b"new tree").expect("seed new");
        // Died between rename one and rename two: repo vacated, old tree parked.
        let scratch = swap_scratch(&repo).expect("scratch path");
        std::fs::create_dir(&scratch).expect("scratch");
        std::fs::write(scratch.join("file.txt"), b"old tree").expect("seed old");
        let journal_path = work.path().join("journal.json");
        write(&journal_path, &journal(&repo, &tmp, Phase::Swapping));

        recover(&journal_path).expect("recover");

        assert_eq!(
            std::fs::read(repo.join("file.txt")).expect("repo whole"),
            b"new tree"
        );
        assert!(!scratch.exists(), "scratch must not outlive recovery");
        assert!(!tmp.exists());
        assert!(!journal_path.exists());
    }

    /// The other Windows crash point: rename two landed, so the repo already
    /// holds the new tree and only the scratch is left to clear.
    #[cfg(windows)]
    #[test]
    fn recover_clears_the_scratch_after_the_swap_landed() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        std::fs::write(repo.join("file.txt"), b"new tree").expect("seed new");
        let tmp = work.path().join("repo.tmp");
        let scratch = swap_scratch(&repo).expect("scratch path");
        std::fs::create_dir(&scratch).expect("scratch");
        std::fs::write(scratch.join("file.txt"), b"old tree").expect("seed old");
        let journal_path = work.path().join("journal.json");
        write(&journal_path, &journal(&repo, &tmp, Phase::Swapping));

        recover(&journal_path).expect("recover");

        assert_eq!(
            std::fs::read(repo.join("file.txt")).expect("repo whole"),
            b"new tree"
        );
        assert!(!scratch.exists(), "scratch must not outlive recovery");
        assert!(!journal_path.exists());
    }
}
