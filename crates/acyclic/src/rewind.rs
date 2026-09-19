//! Full-tree rewind: materialize the target generation into a sibling temp
//! directory, atomically exchange it with the working tree, keep the old tree
//! beside the repository, and journal every phase so kill -9 leaves the repo fully-old or
//! fully-new — never mixed.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use acyclic_fs::{
    durable_rename, exchange_native_entries, materialize_checkout, materialize_checkout_host_path,
    publish_native_exchange, MaterializeError, MaterializeOptions, RenameMode,
};
use acyclic_fs::{CancellationToken, GenerationId, WorkCounters};
use serde::{Deserialize, Serialize};

use crate::exclude::Exclusions;
use crate::store::{LocalCheckout, Store};
use crate::{EngineError, Result};

const MAXIMUM_DIRECTORY_ENTRIES: u32 = 1_024;
const MAXIMUM_EXTENT_SPANS: u32 = 65_536;
const TRANSFER_BYTES: u64 = 8 * 1024 * 1024;
static PARK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What a completed rewind reports back.
#[derive(Clone, Debug)]
pub struct RewindOutcome {
    pub restored: GenerationId,
    /// Where the replaced tree was retained beside the repository.
    pub old_tree: PathBuf,
    /// User-facing caveat: open editors keep inodes from the old tree.
    pub warning: &'static str,
}

pub(crate) struct PreparedRewind<'a> {
    store: &'a Store,
    target: GenerationId,
    tmp: PathBuf,
    parent: PathBuf,
    name: String,
    journal_path: PathBuf,
    carried: Vec<PathBuf>,
    trash_ttl_days: u32,
}

/// What a single-path restore did to the working tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    let mut checkout = store.checkout_exact(target).await?;
    materialize_path_into_checkout(&mut checkout, &root, relative, PathReplace::Atomic).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathReplace {
    Atomic,
    LiveMount,
}

pub(crate) async fn materialize_path_into_checkout(
    checkout: &mut LocalCheckout,
    root: &Path,
    relative: &Path,
    replace: PathReplace,
) -> Result<RestoreOutcome> {
    validate_relative(relative)?;
    let normalized = normalized_relative(relative);
    let destination = root.join(relative);
    ensure_real_parents(root, relative)?;
    let parent = destination
        .parent()
        .ok_or_else(|| EngineError::Restore("path has no parent".into()))?;
    let cancel = CancellationToken::new();
    // The SDK requires an empty root and recreates the selected path below
    // it. Keep ordinary restore staging outside the watched repository;
    // live mounts must stage on the mount to permit the final rename.
    let stage_parent = match replace {
        PathReplace::Atomic => root
            .parent()
            .ok_or_else(|| EngineError::Restore("repository root has no parent".into()))?,
        PathReplace::LiveMount => parent,
    };
    let stage_root = create_restore_stage(stage_parent)?;
    let staged = stage_root.join(&normalized);
    let materialized = materialize_checkout_host_path(
        checkout,
        &normalized,
        &MaterializeOptions {
            destination: stage_root.clone(),
            maximum_directory_entries: MAXIMUM_DIRECTORY_ENTRIES,
            maximum_extent_spans: MAXIMUM_EXTENT_SPANS,
            transfer_bytes: TRANSFER_BYTES,
        },
        WorkCounters::UNBOUNDED,
        &cancel,
    )
    .await;
    if matches!(
        materialized.as_ref().map_err(|failure| &failure.error),
        Err(MaterializeError::MissingPath)
    ) {
        std::fs::remove_dir_all(&stage_root)?;
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match replace {
                PathReplace::Atomic => Err(EngineError::Restore(format!(
                    "{} does not exist at that checkpoint or in the tree",
                    relative.display()
                ))),
                PathReplace::LiveMount => Ok(RestoreOutcome {
                    path: relative.to_path_buf(),
                    action: RestoreAction::Removed,
                }),
            },
            Err(error) => Err(error.into()),
        };
    }
    if let Err(error) = materialized {
        let _ = std::fs::remove_dir_all(&stage_root);
        return Err(EngineError::Restore(format!("materialize path: {error}")));
    }
    ensure_real_parents(root, relative)?;

    // A live mount must observe ordinary remove/rename operations through
    // its driver. A user restore exchanges an existing node atomically.
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => match replace {
            PathReplace::Atomic => {
                // An exchange may have published the new node before a
                // durability error. Keep both staged and scratch trees.
                exchange_native_entries(&destination, &staged)
                    .map_err(|error| EngineError::Restore(format!("exchange path: {error}")))?;
            }
            PathReplace::LiveMount => replace_live_mount(&staged, &destination, parent)?,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let renamed = match replace {
                PathReplace::Atomic => durable_rename(&staged, &destination, RenameMode::NoReplace),
                PathReplace::LiveMount => std::fs::rename(&staged, &destination),
            };
            if let Err(error) = renamed {
                return Err(error.into());
            }
        }
        Err(error) => {
            return Err(error.into());
        }
    }
    std::fs::remove_dir_all(&stage_root)?;
    #[cfg(unix)]
    if replace == PathReplace::Atomic {
        sync_parent(&stage_root)?;
    }
    Ok(RestoreOutcome {
        path: relative.to_path_buf(),
        action: RestoreAction::Restored,
    })
}

fn replace_live_mount(staged: &Path, destination: &Path, parent: &Path) -> Result<()> {
    let backup_root = create_restore_stage(parent)?;
    let backup = backup_root.join("old");
    if let Err(error) = std::fs::rename(destination, &backup) {
        let _ = std::fs::remove_dir(&backup_root);
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(staged, destination) {
        if let Err(rollback) = std::fs::rename(&backup, destination) {
            return Err(EngineError::Restore(format!(
                "materialize rename failed: {error}; old node remains at {} after rollback failed: {rollback}",
                backup.display()
            )));
        }
        let _ = std::fs::remove_dir(&backup_root);
        return Err(error.into());
    }
    remove_any(&backup)?;
    std::fs::remove_dir(&backup_root)?;
    Ok(())
}

fn normalized_relative(relative: &Path) -> PathBuf {
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

fn create_restore_stage(parent: &Path) -> Result<PathBuf> {
    for _ in 0..16 {
        let sequence = PARK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stage = parent.join(format!(
            ".{}-restore-{}-{sequence}",
            crate::product::NAME,
            std::process::id()
        ));
        match std::fs::create_dir(&stage) {
            Ok(()) => return Ok(stage),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(EngineError::Restore(
        "could not allocate a unique restore stage".into(),
    ))
}

pub(crate) fn validate_relative(relative: &Path) -> Result<Vec<Vec<u8>>> {
    let config = crate::store::volume_config();
    let namespace = acyclic_fs::host_path_to_namespace(relative, config.profile, config.limits)
        .map_err(|_| {
            EngineError::Restore(format!(
                "{}: path must be relative to the repo root and stay inside it",
                relative.display()
            ))
        })?;
    Ok(namespace
        .components()
        .iter()
        .map(|name| name.as_bytes().to_vec())
        .collect())
}

/// Create missing ancestors one component at a time, refusing symlinks and
/// Windows reparse points before a path restore touches its destination.
fn ensure_real_parents(root: &Path, relative: &Path) -> Result<()> {
    fn check_real_directory(path: &Path) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
        };
        #[cfg(not(windows))]
        let reparse = false;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse {
            return Err(EngineError::Restore(format!(
                "restore parent {} is not a real directory",
                path.display()
            )));
        }
        Ok(())
    }

    check_real_directory(root)?;
    let mut cursor = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            cursor.push(name);
            match std::fs::symlink_metadata(&cursor) {
                Ok(_) => check_real_directory(&cursor)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&cursor)?;
                    check_real_directory(&cursor)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(crate) fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

/// Returns every listed path present in `from` to `into` during legacy
/// journal recovery.
/// A conflict or I/O failure keeps the journal and both trees for retry.
fn move_back(from: &Path, into: &Path, relative: &[PathBuf]) -> Result<()> {
    for path in relative {
        let source = from.join(path);
        let destination = into.join(path);
        if !path_exists(&source)? {
            continue;
        }
        if path_exists(&destination)? {
            return Err(EngineError::Restore(format!(
                "rewind recovery found both copies of carried path {}",
                path.display()
            )));
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&source, &destination)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    /// Materializing into tmp; repo untouched. Recovery: delete tmp.
    Materializing,
    /// The prepared tree is complete and the SDK head is being restored.
    RestoringHead,
    /// Excluded paths moving from repo into tmp. Recovery: move back any
    /// that already moved, then delete tmp.
    Carrying,
    /// Root exchange in flight. Recovery makes the repo whole and parks every
    /// displaced tree; Windows may also have an intermediate scratch tree.
    Swapping,
}

/// Result of reconciling an interrupted root replacement.
#[derive(Debug)]
pub struct RecoveredSwap {
    /// The target was already published when a Windows parking rename failed.
    pub published: bool,
    /// The displaced tree retained beside the repository.
    pub old_tree: Option<PathBuf>,
    /// Generation named by the durable journal.
    pub target: GenerationId,
}

pub async fn recover_workspace(store: &mut Store, recovered: RecoveredSwap) -> Result<()> {
    store.recover_workspace_head(&recovered).await?;
    if recovered.published {
        return Ok(());
    }
    let text = match std::fs::read_to_string(store.paths.rewind_journal()) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let journal: Journal = serde_json::from_str(&text)
        .map_err(|error| EngineError::Restore(format!("rewind journal: {error}")))?;
    if journal.phase != Phase::RestoringHead {
        return Ok(());
    }
    let locator = publish_locator(&journal.repo_root, &store.paths.rewind_journal())?;
    let exchange = publish_native_exchange(
        &store.paths.rewind_journal(),
        &journal.repo_root,
        &journal.tmp,
        journal.carried,
    )
    .map_err(|error| EngineError::Restore(format!("recover publish tree: {error}")))?;
    if !exchange.published {
        return Err(EngineError::Restore(
            "rewind recovery did not publish prepared tree".into(),
        ));
    }
    if let Some(displaced) = exchange.displaced {
        let parent = journal
            .repo_root
            .parent()
            .ok_or_else(|| EngineError::Restore("rewind repo root has no parent".into()))?;
        let name = journal
            .repo_root
            .file_name()
            .ok_or_else(|| EngineError::Restore("rewind repo root has no name".into()))?
            .to_string_lossy();
        let _ = park_replaced_tree(&displaced, parent, &name)?;
    }
    let _ = std::fs::remove_file(locator);
    Ok(())
}

/// Stored beside the repository so startup can find the journal even while
/// the Windows exchange has temporarily removed the repository name. The
/// store location may come from a config file inside that missing tree.
#[derive(Serialize, Deserialize)]
struct RecoveryLocator {
    repo_root: PathBuf,
    journal: PathBuf,
}

fn locator_path(repo_root: &Path) -> Result<PathBuf> {
    let parent = repo_root
        .parent()
        .ok_or_else(|| EngineError::Restore("rewind repo root has no parent".into()))?;
    let name = repo_root
        .file_name()
        .ok_or_else(|| EngineError::Restore("rewind repo root has no name".into()))?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.{}-rewind.json", crate::product::NAME)))
}

/// Recovers before loading the repository's config or canonicalizing its root.
/// Both operations can fail after the first Windows rename has removed it.
pub fn recover_before_repo_open(repo_root: &Path) -> Result<PathBuf> {
    let canonical = match repo_root.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = repo_root
                .parent()
                .ok_or_else(|| EngineError::Restore("rewind repo root has no parent".into()))?;
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            parent.canonicalize()?.join(
                repo_root
                    .file_name()
                    .ok_or_else(|| EngineError::Restore("rewind repo root has no name".into()))?,
            )
        }
        Err(error) => return Err(error.into()),
    };
    #[cfg(windows)]
    let canonical = {
        let mut canonical = canonical;
        if let (Some(parent), Some(name)) = (canonical.parent(), canonical.file_name()) {
            let name = name.to_string_lossy();
            let suffix = format!(".{}-swap", crate::product::NAME);
            if let Some(original) = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(&suffix))
            {
                let candidate = parent.join(original);
                if locator_path(&candidate)?.exists() {
                    // Windows holds the current directory open. Leave the old
                    // tree before recovery renames it back to the repository.
                    std::env::set_current_dir(parent)?;
                    canonical = candidate;
                }
            }
        }
        canonical
    };
    let locator_path = locator_path(&canonical)?;
    let text = match std::fs::read_to_string(&locator_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(canonical),
        Err(error) => return Err(error.into()),
    };
    let locator: RecoveryLocator = serde_json::from_str(&text)
        .map_err(|error| EngineError::Restore(format!("rewind locator: {error}")))?;
    if locator.repo_root != canonical {
        return Err(EngineError::Restore("rewind locator repo mismatch".into()));
    }
    recover(&locator.journal)?;
    std::fs::remove_file(locator_path)?;
    Ok(canonical)
}

/// Executes a rewind against the store's repo. Called from the pipeline with
/// captures paused; the caller re-baselines afterwards. Excluded paths are
/// carried from the live tree into the restored one: no checkpoint holds
/// them, so the working copy is the only copy.
pub(crate) async fn prepare<'a>(
    store: &'a Store,
    target: GenerationId,
    trash_ttl_days: u32,
    exclusions: &Exclusions,
) -> Result<PreparedRewind<'a>> {
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

    // A failed previous exchange may have left a complete tree in scratch.
    // Resolve its journal before reusing either the temporary name or journal.
    recover(&journal_path)?;

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

    let carried: Vec<PathBuf> = exclusions
        .host_paths()
        .into_iter()
        .filter(|relative| std::fs::symlink_metadata(repo.join(relative)).is_ok())
        .collect();
    Ok(PreparedRewind {
        store,
        target,
        tmp,
        parent: parent.to_path_buf(),
        name,
        journal_path,
        carried,
        trash_ttl_days,
    })
}

impl PreparedRewind<'_> {
    pub(crate) fn mark_restoring_head(&self) -> Result<()> {
        write_journal(
            &self.journal_path,
            &Journal {
                target_generation: hex::encode(self.target.digest().as_bytes()),
                repo_root: self.store.repo_root.clone(),
                tmp: self.tmp.clone(),
                phase: Phase::RestoringHead,
                carried: self.carried.clone(),
            },
        )
    }

    pub(crate) fn publish(self) -> Result<RewindOutcome> {
        let repo = &self.store.repo_root;
        // The staging-only legacy phase has served its purpose. From this point
        // the SDK owns the durable carry/exchange journal at the same path.
        std::fs::remove_file(&self.journal_path)?;
        let locator = publish_locator(repo, &self.journal_path)?;
        let exchange = publish_native_exchange(&self.journal_path, repo, &self.tmp, self.carried)
            .map_err(|error| EngineError::Restore(format!("publish tree: {error}")))?;
        if !exchange.published {
            return Err(EngineError::Restore(
                "native exchange recovered without publishing the target tree".into(),
            ));
        }

        let displaced = exchange.displaced.unwrap_or(self.tmp);
        let old_tree = park_replaced_tree(&displaced, &self.parent, &self.name)?;
        let _ = std::fs::remove_file(locator);
        prune_sibling_trash(repo, self.trash_ttl_days);

        Ok(RewindOutcome {
            restored: self.target,
            old_tree,
            warning: "reload your editor: open files still point at the replaced tree",
        })
    }
}

/// Moves the replaced tree out of the way and returns where it landed.
///
/// The destination is a sibling so the rename stays on the repository volume.
fn park_replaced_tree(tmp: &Path, parent: &Path, name: &str) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let unique = format!(
        "{stamp}-{}-{}",
        std::process::id(),
        PARK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let sibling = parent.join(format!(".{name}.{}-trash-{unique}", crate::product::NAME));
    durable_rename(tmp, &sibling, RenameMode::NoReplace).map_err(|error| {
        EngineError::Restore(format!(
            "rewind: park the replaced tree at {}: {error}",
            sibling.display()
        ))
    })?;
    Ok(sibling)
}

/// Startup crash recovery. Reads the journal (if any) and finishes or unwinds
/// the interrupted rewind so the repo is whole before the pipeline baselines.
pub fn recover(journal_path: &Path) -> Result<Option<RecoveredSwap>> {
    let text = match std::fs::read_to_string(journal_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: Journal = serde_json::from_str(&text)
        .map_err(|error| EngineError::Restore(format!("rewind journal: {error}")))?;
    let target = decode_generation(&journal.target_generation)?;
    #[cfg(windows)]
    let mut published = false;
    #[cfg(not(windows))]
    let published = false;
    let mut old_tree = None;
    match journal.phase {
        Phase::Materializing => {
            // Repo untouched; the partial tmp tree is garbage.
            let _ = std::fs::remove_dir_all(&journal.tmp);
        }
        Phase::RestoringHead => {
            return Ok(Some(RecoveredSwap {
                published,
                old_tree,
                target,
            }))
        }
        Phase::Carrying => {
            // Some excluded paths may already sit in tmp: bring them home,
            // then drop the unused new tree.
            move_back(&journal.tmp, &journal.repo_root, &journal.carried)?;
            let _ = std::fs::remove_dir_all(&journal.tmp);
        }
        Phase::Swapping => {
            #[cfg(windows)]
            let scratch = swap_scratch(&journal.repo_root);
            #[cfg(windows)]
            let scratch_present = scratch
                .as_deref()
                .map(path_exists)
                .transpose()?
                .unwrap_or(false);
            let repo_present = path_exists(&journal.repo_root)?;
            let tmp_present = path_exists(&journal.tmp)?;
            #[cfg(windows)]
            {
                published = repo_present && scratch_present && !tmp_present;
            }
            #[cfg(windows)]
            if scratch_present && tmp_present && repo_present {
                return Err(EngineError::Restore(
                    "rewind recovery found three live trees; refusing to discard any".into(),
                ));
            }
            #[cfg(windows)]
            if !repo_present && scratch_present {
                // Rename #1 landed but the publication may not have. Restore
                // the old tree, including excluded paths carried into tmp.
                let old = scratch
                    .as_deref()
                    .ok_or_else(|| EngineError::Restore("rewind scratch path is missing".into()))?;
                move_back(&journal.tmp, old, &journal.carried)?;
                durable_rename(old, &journal.repo_root, RenameMode::NoReplace)?;
            } else if !repo_present && tmp_present {
                // A journal from an older implementation can have no scratch.
                // Make the repo whole before attempting store startup.
                durable_rename(&journal.tmp, &journal.repo_root, RenameMode::NoReplace)?;
            } else {
                // If the exchange never started, carried paths are still in
                // tmp and must return to the live tree. After a completed
                // exchange they are already in the live tree.
                move_back(&journal.tmp, &journal.repo_root, &journal.carried)?;
            }
            #[cfg(not(windows))]
            if !repo_present && tmp_present {
                durable_rename(&journal.tmp, &journal.repo_root, RenameMode::NoReplace)?;
            } else {
                move_back(&journal.tmp, &journal.repo_root, &journal.carried)?;
            }
            if !path_exists(&journal.repo_root)? {
                return Err(EngineError::Restore(
                    "rewind recovery could not find a complete repository tree".into(),
                ));
            }
            let parent = journal
                .repo_root
                .parent()
                .ok_or_else(|| EngineError::Restore("rewind repo root has no parent".into()))?;
            let name = journal
                .repo_root
                .file_name()
                .ok_or_else(|| EngineError::Restore("rewind repo root has no name".into()))?
                .to_string_lossy();
            if path_exists(&journal.tmp)? {
                old_tree = Some(park_replaced_tree(&journal.tmp, parent, &name)?);
            }
            #[cfg(windows)]
            if let Some(scratch) = scratch {
                if path_exists(&scratch)? {
                    old_tree = Some(park_replaced_tree(&scratch, parent, &name)?);
                }
            }
        }
    }
    std::fs::remove_file(journal_path)?;
    #[cfg(unix)]
    sync_parent(journal_path)?;
    Ok(Some(RecoveredSwap {
        published,
        old_tree,
        target,
    }))
}

fn decode_generation(encoded: &str) -> Result<GenerationId> {
    let bytes = hex::decode(encoded)
        .map_err(|error| EngineError::Restore(format!("rewind generation: {error}")))?;
    let digest: [u8; 32] = bytes
        .try_into()
        .map_err(|_| EngineError::Restore("rewind generation must be 32 bytes".into()))?;
    Ok(GenerationId::new(acyclic_fs::Digest::from_bytes(digest)))
}

fn path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
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
        durable_rename(&tmp, path, RenameMode::Replace)
    };
    write().map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::Restore(format!("rewind: journal {}: {error}", path.display()))
    })?;
    Ok(())
}

fn write_locator(path: &Path, locator: &RecoveryLocator) -> Result<()> {
    use std::io::Write;
    let text = serde_json::to_vec(locator)
        .map_err(|error| EngineError::Restore(format!("encode rewind locator: {error}")))?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&text)?;
    file.sync_all()?;
    drop(file);
    durable_rename(&tmp, path, RenameMode::Replace)?;
    Ok(())
}

fn publish_locator(repo: &Path, journal: &Path) -> Result<PathBuf> {
    let path = locator_path(repo)?;
    write_locator(
        &path,
        &RecoveryLocator {
            repo_root: repo.to_path_buf(),
            journal: journal.to_path_buf(),
        },
    )?;
    Ok(path)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| EngineError::Restore("rewind path has no parent".into()))?,
    )?
    .sync_all()?;
    Ok(())
}

fn prune_sibling_trash(repo: &Path, ttl_days: u32) {
    let Some(parent) = repo.parent() else {
        return;
    };
    let Some(name) = repo.file_name() else {
        return;
    };
    let prefix = format!(
        ".{}.{}-trash-",
        name.to_string_lossy(),
        crate::product::NAME
    );
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let ttl = std::time::Duration::from_secs(u64::from(ttl_days) * 24 * 3600);
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parking_replaced_trees_never_reuses_a_name() -> Result<()> {
        let work = tempfile::tempdir()?;
        let mut parked = Vec::new();
        for index in 0..3 {
            let tmp = work.path().join(format!("tmp-{index}"));
            std::fs::create_dir(&tmp)?;
            std::fs::write(tmp.join("old.txt"), index.to_string())?;
            let destination = park_replaced_tree(&tmp, work.path(), "repo")?;
            assert_eq!(
                std::fs::read_to_string(destination.join("old.txt"))?,
                index.to_string()
            );
            parked.push(destination);
        }
        assert!(parked[0] != parked[1] && parked[1] != parked[2] && parked[0] != parked[2]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn path_restore_rejects_symlinked_parent() -> Result<()> {
        let work = tempfile::tempdir()?;
        let root = work.path().join("repo");
        let outside = work.path().join("outside");
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&outside)?;
        std::os::unix::fs::symlink(&outside, root.join("dir"))?;
        assert!(ensure_real_parents(&root, Path::new("dir/file")).is_err());
        assert!(!outside.join("file").exists());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn path_restore_rejects_reparse_parent_when_symlinks_are_available() -> Result<()> {
        let work = tempfile::tempdir()?;
        let root = work.path().join("repo");
        let outside = work.path().join("outside");
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&outside)?;
        if let Err(error) = std::os::windows::fs::symlink_dir(&outside, root.join("dir")) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return Ok(());
            }
            return Err(error.into());
        }
        assert!(ensure_real_parents(&root, Path::new("dir/file")).is_err());
        assert!(!outside.join("file").exists());
        Ok(())
    }

    fn recover(journal_path: &Path) -> Result<Option<RecoveredSwap>> {
        super::recover(journal_path)
    }

    fn parked_tree_contains(repo: &Path, file: &str, expected: &[u8]) -> bool {
        repo.parent()
            .and_then(|parent| std::fs::read_dir(parent).ok())
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".acyclic-trash-")
            })
            .any(|entry| std::fs::read(entry.path().join(file)).ok().as_deref() == Some(expected))
    }

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

        let recovered = recover(&journal_path)
            .expect("recover")
            .expect("recovered journal");
        assert!(!recovered.published);
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
    fn recover_keeps_both_trees_and_journal_on_carried_path_conflict() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir(&repo).expect("repo");
        std::fs::create_dir(&tmp).expect("tmp");
        std::fs::write(repo.join(".env"), b"repo copy").expect("repo data");
        std::fs::write(tmp.join(".env"), b"staged copy").expect("staged data");
        let journal_path = work.path().join("journal.json");
        let mut entry = journal(&repo, &tmp, Phase::Carrying);
        entry.carried = vec![PathBuf::from(".env")];
        write(&journal_path, &entry);

        recover(&journal_path).expect_err("conflicting copies require inspection");
        assert_eq!(
            std::fs::read(repo.join(".env")).expect("repo"),
            b"repo copy"
        );
        assert_eq!(
            std::fs::read(tmp.join(".env")).expect("staged"),
            b"staged copy"
        );
        assert!(journal_path.exists());
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
        assert!(parked_tree_contains(&repo, "file.txt", b"old"));
    }

    #[test]
    fn journals_written_before_exclusions_still_decode() {
        let text =
            r#"{"target_generation":"00","repo_root":"/r","tmp":"/t","phase":"Materializing"}"#;
        let journal: Journal = serde_json::from_str(text).expect("decode");
        assert!(journal.carried.is_empty());
    }

    /// A crash after Windows rename #1 must roll back to the old tree and
    /// retain the staged new tree in trash for recovery or inspection.
    #[cfg(windows)]
    #[test]
    fn startup_recovers_before_the_repository_path_exists() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work
            .path()
            .canonicalize()
            .expect("canonical parent")
            .join("repo");
        let scratch = swap_scratch(&repo).expect("scratch path");
        let staged = work.path().join("staged");
        std::fs::create_dir(&scratch).expect("old tree");
        std::fs::write(scratch.join("old.txt"), b"original").expect("old file");
        std::fs::create_dir(&staged).expect("new tree");
        std::fs::write(staged.join("new.txt"), b"replacement").expect("new file");
        let store = work.path().join("custom-store");
        std::fs::create_dir(&store).expect("custom store");
        let journal_path = store.join("rewind-journal.json");
        write(&journal_path, &journal(&repo, &staged, Phase::Swapping));
        let locator = locator_path(&repo).expect("locator path");
        write_locator(
            &locator,
            &RecoveryLocator {
                repo_root: repo.clone(),
                journal: journal_path.clone(),
            },
        )
        .expect("locator");

        recover_before_repo_open(&repo).expect("startup recovery");
        assert_eq!(
            std::fs::read(repo.join("old.txt")).expect("old tree"),
            b"original"
        );
        assert!(!journal_path.exists());
        assert!(!locator.exists());
    }

    #[cfg(windows)]
    #[test]
    fn recover_preserves_both_trees_after_first_windows_rename() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        std::fs::write(tmp.join("file.txt"), b"new tree").expect("seed new");
        std::fs::write(tmp.join(".env"), b"carried live data").expect("carry");
        // Died between rename one and rename two: repo vacated, old tree parked.
        let scratch = swap_scratch(&repo).expect("scratch path");
        std::fs::create_dir(&scratch).expect("scratch");
        std::fs::write(scratch.join("file.txt"), b"old tree").expect("seed old");
        let journal_path = work.path().join("journal.json");
        let mut entry = journal(&repo, &tmp, Phase::Swapping);
        entry.carried = vec![PathBuf::from(".env")];
        write(&journal_path, &entry);

        recover(&journal_path).expect("recover");

        assert_eq!(
            std::fs::read(repo.join("file.txt")).expect("repo whole"),
            b"old tree"
        );
        assert!(!scratch.exists(), "scratch must not outlive recovery");
        assert!(!tmp.exists());
        assert!(!journal_path.exists());
        assert_eq!(
            std::fs::read(repo.join(".env")).expect("carried data survived"),
            b"carried live data"
        );
        assert!(parked_tree_contains(&repo, "file.txt", b"new tree"));
    }
}
