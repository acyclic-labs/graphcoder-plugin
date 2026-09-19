//! Fork engine primitives (Launch 3).
//!
//! A fork is a writable overlay mount of a Head checkout: reads hydrate
//! lazily from the store (O(1) creation, `node_modules` included), writes
//! accumulate in that checkout's private overlay — invisible to the real
//! tree and to every other fork. The pipeline mints fork checkouts (it owns
//! the volume); the daemon owns the mount sessions (they must live in the
//! long-lived process).
//!
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acyclic_fs::model::VolumeConfig;
use acyclic_fs::SharedCheckout;
use acyclic_fs::{
    probe_native_mount, GenerationId, LocalAuthorityBackend, LocalObjectBackend, NativeMountKind,
    VolumeId,
};

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
            "forks require usable /dev/fuse. To enable mounts:\n",
            "  sudo modprobe fuse                          # load the kernel module\n",
            "  sudo usermod -aG fuse \"$USER\"               # if /dev/fuse is group-restricted; log in again\n",
            "  docker run --device /dev/fuse --cap-add SYS_ADMIN ...   # inside a container",
        )
    } else if cfg!(target_os = "macos") {
        concat!(
            "forks require the built-in NFS mount tools (/sbin/mount_nfs, /sbin/umount), which\n",
            "are missing or blocked by policy. No extra software is needed on macOS;\n",
            "ask your administrator to allow loopback NFS mounts.",
        )
    } else if cfg!(windows) {
        concat!(
            "forks require the optional Windows Projected File System feature. Enable\n",
            "Client-ProjFS in Windows Features. ProjFS safely rejects cross-root\n",
            "directory moves that it cannot capture atomically.",
        )
    } else {
        "native mounts are required for forks and are not supported on this platform."
    }
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

/// Removes the single routed mount and workspace directory a dead daemon
/// left behind. An absent root is the only successful no-op.
pub fn sweep_stale_forks(repo_root: &Path) -> Result<(), String> {
    let Some(root) = forks_root(repo_root) else {
        return Err("repo root has no parent for fork workspaces".to_owned());
    };
    if !root.try_exists().map_err(|error| error.to_string())? {
        return Ok(());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let mount = root.join("mnt");
    // FUSE-T's go-nfsv4 helpers outlive a killed daemon and wedge the
    // vendor's tiny shared NFS port pool for every future mount on the
    // host — reap any helper serving one of OUR workspaces first.
    #[cfg(target_os = "macos")]
    if mount.try_exists().map_err(|error| error.to_string())? {
        let status = std::process::Command::new("pkill")
            .arg("-f")
            .arg(format!("go-nfsv4.*{}", mount.display()))
            .status()
            .map_err(|error| format!("stop stale NFS helper: {error}"))?;
        if !status.success() && status.code() != Some(1) {
            return Err(format!("stop stale NFS helper: {status}"));
        }
        let status = std::process::Command::new("umount")
            .arg("-f")
            .arg(&mount)
            .status()
            .map_err(|error| format!("unmount stale fork workspace: {error}"))?;
        if !status.success() {
            return Err(format!("unmount stale fork workspace: {status}"));
        }
    }
    #[cfg(target_os = "linux")]
    if mount.try_exists().map_err(|error| error.to_string())? {
        let status = std::process::Command::new("fusermount")
            .arg("-u")
            .arg(&mount)
            .status()
            .map_err(|error| format!("unmount stale fork workspace: {error}"))?;
        if !status.success() {
            return Err(format!("unmount stale fork workspace: {status}"));
        }
    }
    std::fs::remove_dir_all(&root)
        .map_err(|error| format!("remove stale fork workspace {}: {error}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_names_a_provider() {
        let capability = mount_capability();
        assert_ne!(capability.provider, "");
        assert_eq!(capability.available, capability.reason.is_none());
        assert!(!mount_setup_hint().is_empty());
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

        sweep_stale_forks(&repo).expect("sweep");
        assert!(!root.exists());
    }

    #[test]
    fn sweeping_an_absent_root_is_a_no_op() {
        let work = tempfile::tempdir().expect("tempdir");
        let repo = work.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        sweep_stale_forks(&repo).expect("absent root");
    }
}
