//! Full-tree rewind: materialize the target generation into a sibling temp
//! directory, atomically exchange it with the working tree, keep the old tree
//! beside the repository, and journal every phase so kill -9 leaves the repo fully-old or
//! fully-new — never mixed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use acyclic_fs::{
    durable_rename, prepare_native_exchange_with_recovery, publish_native_exchange,
    recover_native_exchange, HostPathReplacement, HostPathRestore, IdempotencyKey,
    MaterializeOptions, NativeExchangeJournal, RenameMode,
};
use acyclic_fs::{CancellationToken, GenerationId, WorkCounters};
use serde::{Deserialize, Serialize};

use crate::exclude::Exclusions;
use crate::store::Store;
use crate::{EngineError, Result};

static PARK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn publication_key(generation: GenerationId) -> Result<IdempotencyKey> {
    let bytes = generation.digest().as_bytes()[..16]
        .try_into()
        .map_err(|_| EngineError::Store("invalid generation digest".into()))?;
    Ok(IdempotencyKey::from_bytes(bytes))
}

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
    staging_journal_path: PathBuf,
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
    let generation = store.generation(target).await?;
    let cancel = CancellationToken::new();
    let restored = generation
        .restore_host_path(
            relative,
            HostPathReplacement::Atomic,
            &MaterializeOptions::native(&root),
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(|error| EngineError::Restore(error.to_string()))?;
    Ok(RestoreOutcome {
        path: relative.to_path_buf(),
        action: match restored.value {
            HostPathRestore::Restored => RestoreAction::Restored,
            HostPathRestore::Removed => RestoreAction::Removed,
        },
    })
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
struct LegacyJournal {
    pub target_generation: String,
    pub repo_root: PathBuf,
    pub tmp: PathBuf,
    pub phase: LegacyPhase,
    /// Excluded paths (repo-relative) moved from the live tree into `tmp`
    /// before the swap. Absent in journals written before exclusions.
    #[serde(default)]
    pub carried: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct StagingMarker {
    temporary: PathBuf,
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
enum LegacyPhase {
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
    let journal: LegacyJournal = serde_json::from_str(&text)
        .map_err(|error| EngineError::Restore(format!("rewind journal: {error}")))?;
    if journal.phase != LegacyPhase::RestoringHead {
        return Ok(());
    }
    let locator = publish_locator(&journal.repo_root, &store.paths.rewind_journal())?;
    let exchange = publish_native_exchange(
        &store.paths.rewind_journal(),
        &journal.repo_root,
        &journal.tmp,
        publication_key(recovered.target)?,
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
    let staging_journal_path = journal_path.with_extension("staging.json");

    // A failed previous exchange may have left a complete tree in scratch.
    // Resolve its journal before reusing either the temporary name or journal.
    recover(&journal_path)?;
    let _ = std::fs::remove_file(&staging_journal_path);

    // 1. Materialize the target into an empty sibling directory. A tmp left
    // by an earlier attempt that failed before the swap is stale by
    // construction -- the name carries this daemon's pid, and a rewind that
    // got as far as the swap removes it -- so clear it rather than refusing
    // every later rewind with "already exists".
    let _ = remove_any(&tmp);
    std::fs::create_dir(&tmp).map_err(|error| {
        EngineError::Restore(format!("rewind: stage {}: {error}", tmp.display()))
    })?;
    write_staging_marker(
        &staging_journal_path,
        &StagingMarker {
            temporary: tmp.clone(),
        },
    )?;
    let generation = store.generation(target).await?;
    let cancel = CancellationToken::new();
    generation
        .materialize(
            &MaterializeOptions::native(&tmp),
            WorkCounters::UNBOUNDED,
            &cancel,
        )
        .await
        .map_err(|error| {
            let _ = std::fs::remove_dir_all(&tmp);
            let _ = std::fs::remove_file(&staging_journal_path);
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
        staging_journal_path,
        carried,
        trash_ttl_days,
    })
}

impl PreparedRewind<'_> {
    pub(crate) fn mark_restoring_head(&self) -> Result<()> {
        std::fs::remove_file(&self.staging_journal_path)?;
        prepare_native_exchange_with_recovery(
            &self.journal_path,
            &self.store.repo_root,
            &self.tmp,
            publication_key(self.target)?,
            self.carried.clone(),
            self.target.digest().as_bytes().to_vec(),
        )
        .map_err(|error| EngineError::Restore(format!("prepare tree publication: {error}")))
    }

    pub(crate) fn publish(self) -> Result<RewindOutcome> {
        let repo = &self.store.repo_root;
        let locator = publish_locator(repo, &self.journal_path)?;
        let exchange = publish_native_exchange(
            &self.journal_path,
            repo,
            &self.tmp,
            publication_key(self.target)?,
            self.carried,
        )
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
    recover_staging(&journal_path.with_extension("staging.json"))?;
    let text = match std::fs::read_to_string(journal_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if let Ok(journal) = serde_json::from_str::<NativeExchangeJournal>(&text) {
        let target = decode_generation(&hex::encode(&journal.recovery))?;
        let outcome = recover_native_exchange(journal_path)
            .map_err(|error| EngineError::Restore(format!("recover native exchange: {error}")))?;
        return Ok(Some(RecoveredSwap {
            published: outcome.published,
            old_tree: outcome.displaced,
            target,
        }));
    }
    recover_legacy(journal_path, &text)
}

fn recover_staging(path: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let marker: StagingMarker = serde_json::from_str(&text)
        .map_err(|error| EngineError::Restore(format!("rewind staging journal: {error}")))?;
    remove_any(&marker.temporary)?;
    std::fs::remove_file(path)?;
    Ok(())
}

fn recover_legacy(journal_path: &Path, text: &str) -> Result<Option<RecoveredSwap>> {
    let journal: LegacyJournal = serde_json::from_str(text)
        .map_err(|error| EngineError::Restore(format!("rewind journal: {error}")))?;
    let target = decode_generation(&journal.target_generation)?;
    #[cfg(windows)]
    let mut published = false;
    #[cfg(not(windows))]
    let published = false;
    let mut old_tree = None;
    match journal.phase {
        LegacyPhase::Materializing => {
            // Repo untouched; the partial tmp tree is garbage.
            let _ = std::fs::remove_dir_all(&journal.tmp);
        }
        LegacyPhase::RestoringHead => {
            return Ok(Some(RecoveredSwap {
                published,
                old_tree,
                target,
            }))
        }
        LegacyPhase::Carrying => {
            // Some excluded paths may already sit in tmp: bring them home,
            // then drop the unused new tree.
            move_back(&journal.tmp, &journal.repo_root, &journal.carried)?;
            let _ = std::fs::remove_dir_all(&journal.tmp);
        }
        LegacyPhase::Swapping => {
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

fn write_staging_marker(path: &Path, marker: &StagingMarker) -> Result<()> {
    let text = serde_json::to_string(marker)
        .map_err(|error| EngineError::Restore(format!("encode staging marker: {error}")))?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    use std::io::Write;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    durable_rename(&tmp, path, RenameMode::Replace)?;
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

    fn journal(repo: &Path, tmp: &Path, phase: LegacyPhase) -> LegacyJournal {
        LegacyJournal {
            target_generation: "00".repeat(32),
            repo_root: repo.to_path_buf(),
            tmp: tmp.to_path_buf(),
            phase,
            carried: Vec::new(),
        }
    }

    fn write(path: &Path, value: &LegacyJournal) {
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
        write(&journal_path, &journal(&repo, &tmp, LegacyPhase::Swapping));

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
        write(
            &journal_path,
            &journal(&repo, &tmp, LegacyPhase::Materializing),
        );

        recover(&journal_path).expect("recover");
        assert_eq!(std::fs::read(repo.join("keep.txt")).expect("read"), b"live");
        assert!(!tmp.exists());
        assert!(!journal_path.exists());
    }

    #[test]
    fn recover_discards_separate_partial_staging() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        std::fs::write(repo.join("keep.txt"), b"live").expect("seed");
        let tmp = work.path().join("repo.tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        std::fs::write(tmp.join("partial.txt"), b"half").expect("seed");
        let journal_path = work.path().join("journal.json");
        let staging_path = journal_path.with_extension("staging.json");
        write_staging_marker(
            &staging_path,
            &StagingMarker {
                temporary: tmp.clone(),
            },
        )
        .expect("write staging marker");

        recover(&journal_path).expect("recover");
        assert_eq!(std::fs::read(repo.join("keep.txt")).expect("read"), b"live");
        assert!(!tmp.exists());
        assert!(!staging_path.exists());
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
        let mut entry = journal(&repo, &tmp, LegacyPhase::Carrying);
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
        let mut entry = journal(&repo, &tmp, LegacyPhase::Swapping);
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
        let mut entry = journal(&repo, &tmp, LegacyPhase::Carrying);
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
        let mut entry = journal(&repo, &tmp, LegacyPhase::Swapping);
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
        let journal: LegacyJournal = serde_json::from_str(text).expect("decode");
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
        write(
            &journal_path,
            &journal(&repo, &staged, LegacyPhase::Swapping),
        );
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
        let mut entry = journal(&repo, &tmp, LegacyPhase::Swapping);
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
