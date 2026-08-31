//! Full-tree rewind: materialize the target generation into a sibling temp
//! directory, atomically exchange it with the working tree, keep the old tree
//! in trash, and journal every phase so kill -9 leaves the repo fully-old or
//! fully-new — never mixed.

use std::path::{Path, PathBuf};

use acyclic_fs::{CancellationToken, GenerationId, WorkCounters};
use acyclic_fs_mount::{materialize_checkout, MaterializeOptions};
use serde::{Deserialize, Serialize};

use crate::store::Store;
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
    let result = unsafe {
        libc::renamex_np(a_c.as_ptr(), b_c.as_ptr(), libc::RENAME_SWAP)
    };
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
