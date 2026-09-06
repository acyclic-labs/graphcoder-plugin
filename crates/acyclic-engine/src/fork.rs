//! Fork engine primitives (Launch 3).
//!
//! A fork is a writable overlay mount of a Head checkout: reads hydrate
//! lazily from the store (O(1) creation, node_modules included), writes
//! accumulate in that checkout's private overlay — invisible to the real
//! tree and to every other fork. The pipeline mints fork checkouts (it owns
//! the volume); the daemon owns the mount sessions (they must live in the
//! long-lived process).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use acyclic_fs::model::VolumeConfig;
use acyclic_fs::SharedCheckout;
use acyclic_fs::{GenerationId, LocalAuthorityBackend, LocalObjectBackend, VolumeId};

/// The mount-safe checkout wrapper for the local backend.
pub type SharedLocalCheckout = SharedCheckout<LocalAuthorityBackend, LocalObjectBackend>;

/// What the pipeline hands the daemon for one new fork.
pub struct ForkSeed {
    pub shared: Arc<SharedLocalCheckout>,
    pub config: VolumeConfig,
    pub volume_id: VolumeId,
    /// Published head the fork was cut from; promote conflicts are judged
    /// against movement past this point.
    pub base: GenerationId,
}

/// Result of a promote request.
#[derive(Clone, Debug)]
pub enum PromoteOutcome {
    Promoted {
        generation: GenerationId,
        /// None when the fork had no writes (nothing to land).
        old_tree: Option<PathBuf>,
    },
    /// The mainline moved past the fork's base — v1 surfaces the conflict
    /// legibly instead of merging.
    Conflict { message: String },
}

/// Result of the commit half of a Safe Mode session resolve (see
/// [`crate::pipeline::PipelineHandle::resolve_session`]) — the swap itself
/// is deferred to a separate `apply_session` call so the caller can show an
/// approval-gated diff in between.
#[derive(Clone, Debug)]
pub enum SessionResolveOutcome {
    /// The overlay committed cleanly; `generation` is ready for
    /// `apply_session`, or can simply be left unswapped (Safe Mode reject).
    Resolved { generation: GenerationId },
    /// Nothing was written; there is nothing to diff or apply.
    NoChanges,
    /// The mainline moved past the fork's base — same v1 stance as promote.
    Conflict { message: String },
}

/// Where a repo's fork workspaces live: a sibling of the repo, outside the
/// working tree so capture never sees them.
pub fn forks_root(repo_root: &Path) -> Option<PathBuf> {
    let parent = repo_root.parent()?;
    let name = repo_root.file_name()?.to_string_lossy();
    Some(parent.join(format!(".{name}.forks")))
}

/// The single mountpoint projecting every fork as a routed subdirectory.
pub fn forks_mount_root(repo_root: &Path) -> Option<PathBuf> {
    Some(forks_root(repo_root)?.join("mnt"))
}

/// Best-effort cleanup of fork dirs left by a dead daemon: unmount anything
/// still attached, then remove the directories. Mount sessions do not
/// survive the daemon in v1.
pub fn sweep_stale_forks(repo_root: &Path) {
    let Some(root) = forks_root(repo_root) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    // FUSE-T's go-nfsv4 helpers outlive a killed daemon and wedge the
    // vendor's tiny shared NFS port pool for every future mount on the
    // host — reap any helper serving one of OUR workspaces first.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(format!("go-nfsv4.*{}", root.display()))
            .status();
    }
    for entry in entries.flatten() {
        let path = entry.path();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("umount")
            .arg("-f")
            .arg(&path)
            .status();
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("fusermount")
                .arg("-u")
                .arg(&path)
                .status();
        }
        let _ = std::fs::remove_dir_all(&path);
    }
    let _ = std::fs::remove_dir(&root);
}

/// Best-effort cleanup of a Safe Mode shadow mount a dead daemon left
/// directly on `repo_root` itself. Unlike [`sweep_stale_forks`], the
/// directory is never removed here -- it IS the real repo -- only
/// force-unmounted so its real content reappears. A no-op if nothing is
/// mounted there.
pub fn sweep_stale_dry_session(repo_root: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("umount")
            .arg("-f")
            .arg(repo_root)
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("fusermount")
            .arg("-u")
            .arg(repo_root)
            .status();
    }
}

/// Reaps a Safe Mode shadow left by a *crashed* daemon before the caller
/// touches `repo`. A dead daemon's shadow is a loopback-NFS/FUSE mountpoint
/// whose server is gone, so every `stat`/`open` under it (config load, path
/// canonicalization) blocks indefinitely — the CLI would hang before it
/// could even spawn a fresh daemon to clean up.
///
/// Distinguishing a dead shadow from a live session (whose shadow is fine)
/// without a store/config lookup — which would itself stat `repo` — is done
/// by probing: a live server answers a `stat` immediately, while a dead one
/// either blocks or fails (the NFS layer surfaces `ETIMEDOUT`/`ENOTCONN`).
/// So this force-unmounts whenever a bounded probe does not cleanly succeed;
/// a healthy repo always stats OK and is never disturbed, and a `umount` of a
/// path that is not actually a mount is a harmless no-op.
pub fn reap_dead_shadow(repo: &Path) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::time::Duration;
        // Resolve to an absolute mountpoint via the (always-live) parent, so
        // the probe/unmount target is stable without stat-ing `repo` itself.
        let target = match (repo.parent(), repo.file_name()) {
            (Some(parent), Some(name)) => match parent.canonicalize() {
                Ok(parent) => parent.join(name),
                Err(_) => repo.to_path_buf(),
            },
            _ => repo.to_path_buf(),
        };
        let probe = target.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // Detached: if the stat is truly wedged it never returns, but the
        // force-unmount below releases it and this short-lived CLI exits.
        std::thread::spawn(move || {
            let _ = tx.send(std::fs::metadata(&probe).is_ok());
        });
        let healthy = matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(true));
        if !healthy {
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("umount")
                .arg("-f")
                .arg(&target)
                .status();
            #[cfg(target_os = "linux")]
            let _ = std::process::Command::new("fusermount")
                .arg("-u")
                .arg(&target)
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forks_root_is_a_hidden_sibling() {
        let root = forks_root(Path::new("/work/my-repo")).expect("root");
        assert_eq!(root, Path::new("/work/.my-repo.forks"));
    }

    #[test]
    fn sweep_removes_stale_directories() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        let root = forks_root(&repo).expect("root");
        std::fs::create_dir_all(root.join("dead-fork")).expect("stale");
        std::fs::write(root.join("dead-fork/leftover"), b"x").expect("file");

        sweep_stale_forks(&repo);
        assert!(!root.exists());
    }
}
