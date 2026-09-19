//! Fork engine primitives (Launch 3).
//!
//! A fork is a writable overlay mount of a Head checkout: reads hydrate
//! lazily from the store (O(1) creation, `node_modules` included), writes
//! accumulate in that checkout's private overlay — invisible to the real
//! tree and to every other fork. The pipeline mints fork checkouts (it owns
//! the volume); the daemon owns the mount sessions (they must live in the
//! long-lived process).
//!
//! When the host has no mount provider (no usable `/dev/fuse` on Linux, or
//! loopback NFS blocked on macOS) a fork degrades to a *copy*: the base
//! generation is materialized into a real directory, and at promote time
//! that directory is captured back into the fork's overlay so the same
//! commit, conflict check, and swap run unchanged. The promise a fork makes
//! — a writable tree that never touches the real one until promoted — holds
//! either way; only the O(1) creation cost is lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use acyclic_fs::model::VolumeConfig;
use acyclic_fs::SharedCheckout;
use acyclic_fs::{
    capture_baseline, capture_root_identity, probe_native_mount, CancellationToken, CaptureOptions,
    GenerationId, LocalAuthorityBackend, LocalObjectBackend, NativeMountKind, VolumeId,
    WorkCounters,
};

use crate::{EngineError, Result};

/// How a fork is realized on this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkMode {
    /// Routed native mount: O(1) creation, lazy hydration.
    Mount,
    /// Materialized directory: full copy up front, captured back at promote.
    Copy,
}

impl ForkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ForkMode::Mount => "mount",
            ForkMode::Copy => "copy",
        }
    }
}

/// What the host can do for fork mounts, probed once at daemon start and
/// reported by `status`/`init`.
#[derive(Clone, Debug)]
pub struct MountCapability {
    /// Human name of the provider this build would use ("fuse", "nfs loopback").
    pub provider: &'static str,
    pub available: bool,
    /// Why not, when unavailable.
    pub reason: Option<String>,
}

impl MountCapability {
    pub fn fork_mode(&self) -> ForkMode {
        if self.available {
            ForkMode::Mount
        } else {
            ForkMode::Copy
        }
    }
}

/// Live probe of the native mount provider.
pub fn mount_capability() -> MountCapability {
    let probe = probe_native_mount();
    let provider = match probe.kind {
        Some(NativeMountKind::LinuxFuse) => "fuse",
        Some(NativeMountKind::MacOsNfs) => "nfs loopback",
        Some(NativeMountKind::WindowsProjFs) => "projfs",
        None => "none",
    };
    MountCapability {
        provider,
        available: probe.available,
        reason: probe.unavailable_reason,
    }
}

/// Platform-specific instructions for making mounts available. Printed by
/// `init` and `install` when the probe fails, so a user learns on day one
/// rather than the day they first try a fork.
pub fn mount_setup_hint() -> &'static str {
    if cfg!(target_os = "linux") {
        concat!(
            "forks will use full copies until /dev/fuse is usable. To enable mounts:\n",
            "  sudo modprobe fuse                          # load the kernel module\n",
            "  sudo usermod -aG fuse \"$USER\"               # if /dev/fuse is group-restricted; log in again\n",
            "  docker run --device /dev/fuse --cap-add SYS_ADMIN ...   # inside a container",
        )
    } else if cfg!(target_os = "macos") {
        concat!(
            "forks will use full copies: the built-in NFS mount tools (/sbin/mount_nfs, /sbin/umount)\n",
            "are missing or blocked by policy. No extra software is needed on macOS;\n",
            "ask your administrator to allow loopback NFS mounts.",
        )
    } else if cfg!(windows) {
        concat!(
            "forks will use full copies until the optional Windows Projected File System\n",
            "feature is enabled and available to this process. Enable Client-ProjFS in\n",
            "Windows Features to use accelerated forks. ProjFS safely rejects cross-root\n",
            "directory moves that it cannot capture atomically.",
        )
    } else {
        "native mounts are not supported on this platform; forks use full copies."
    }
}

/// Root for copy-mode fork directories (a sibling of the mount root).
pub fn forks_copy_root(repo_root: &Path) -> Option<PathBuf> {
    Some(forks_root(repo_root)?.join("copy"))
}

/// Captures the current content of `root` into a fork's overlay, so that
/// promote sees it exactly as it would see writes through a mount.
/// The overlay must be pristine (a fresh fork seed): capture is a full-tree
/// baseline against the checkout, so only paths that actually differ from
/// the base become pending mutations.
pub async fn capture_copy(shared: &SharedLocalCheckout, root: &Path) -> Result<()> {
    let options = CaptureOptions {
        source_root: root.to_path_buf(),
        expected_root_identity: capture_root_identity(root)
            .map_err(EngineError::fs("copy root identity"))?,
        maximum_paths: 4_000_000,
        maximum_extent_spans: 65_536,
    };
    let cancel = CancellationToken::new();
    let mut guard = shared.lock().await;
    capture_baseline(&mut guard, &options, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(EngineError::fs("capture copy fork"))?;
    Ok(())
}

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

/// Recovers a repository still covered by a shadow mount from a legacy
/// `dry_run` daemon. New versions no longer create these mounts, but upgrade
/// must remain able to expose the real repository before opening it.
pub fn reap_legacy_shadow(repo: &Path) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::time::Duration;
        let target = match (repo.parent(), repo.file_name()) {
            (Some(parent), Some(name)) => parent
                .canonicalize()
                .map_or_else(|_| repo.to_path_buf(), |parent| parent.join(name)),
            _ => repo.to_path_buf(),
        };
        let probe = target.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || drop(sender.send(std::fs::metadata(probe).is_ok())));
        if !matches!(receiver.recv_timeout(Duration::from_secs(5)), Ok(true)) {
            #[cfg(target_os = "macos")]
            drop(
                std::process::Command::new("umount")
                    .arg("-f")
                    .arg(target)
                    .status(),
            );
            #[cfg(target_os = "linux")]
            drop(
                std::process::Command::new("fusermount")
                    .arg("-u")
                    .arg(target)
                    .status(),
            );
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = repo;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_names_a_provider_and_picks_a_mode() {
        let capability = mount_capability();
        assert_ne!(capability.provider, "");
        assert_eq!(capability.available, capability.reason.is_none());
        assert_eq!(
            capability.fork_mode(),
            if capability.available {
                ForkMode::Mount
            } else {
                ForkMode::Copy
            }
        );
        assert!(!mount_setup_hint().is_empty());
    }

    #[test]
    fn copy_root_sits_beside_mount_root() {
        let repo = Path::new("/work/my-repo");
        assert_eq!(
            forks_copy_root(repo).expect("copy"),
            Path::new("/work/.my-repo.forks/copy")
        );
        assert_eq!(
            forks_mount_root(repo).expect("mnt"),
            Path::new("/work/.my-repo.forks/mnt")
        );
    }

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
