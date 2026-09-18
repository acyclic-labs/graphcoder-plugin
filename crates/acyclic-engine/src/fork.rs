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

/// What the host can do for fork and Safe Mode mounts, probed once at
/// daemon start and reported by `status`/`init`.
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
///
/// Windows is held to copy forks whatever the probe says. `ProjFS` mounts
/// and projects correctly there — a fork's tree appears and reads back fine —
/// but writes into the projection stop at the `ProjFS` local cache and never
/// reach the overlay checkout, so `fork-diff` reports no changes and
/// `promote` lands nothing. Silently discarding a fork's work is far worse
/// than copying it, and copy forks are verified on Windows: write, diff and
/// promote all behave.
///
/// Revisit when the sdk's `ProjFS` provider carries writes back. The probe
/// still runs so `status` can name the provider it found.
pub fn mount_capability() -> MountCapability {
    let probe = probe_native_mount();
    let provider = match probe.kind {
        Some(NativeMountKind::LinuxFuse) => "fuse",
        Some(NativeMountKind::MacOsNfs) => "nfs loopback",
        Some(NativeMountKind::WindowsProjFs) => "projfs",
        None => "none",
    };
    #[cfg(windows)]
    {
        let _ = probe.available;
        MountCapability {
            provider,
            available: false,
            reason: Some("ProjFS does not carry fork writes back to the store".to_owned()),
        }
    }
    #[cfg(not(windows))]
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
            "  docker run --device /dev/fuse --cap-add SYS_ADMIN ...   # inside a container\n",
            "Safe Mode (dry_run) needs mounts and refuses to start without them.",
        )
    } else if cfg!(target_os = "macos") {
        concat!(
            "forks will use full copies: the built-in NFS mount tools (/sbin/mount_nfs, /sbin/umount)\n",
            "are missing or blocked by policy. No extra software is needed on macOS;\n",
            "ask your administrator to allow loopback NFS mounts.\n",
            "Safe Mode (dry_run) needs mounts and refuses to start without them.",
        )
    } else if cfg!(windows) {
        concat!(
            "forks use full copies on Windows; `promote` captures their changes.\n",
            "Safe Mode (dry_run) is unavailable. Nothing to install.",
        )
    } else {
        "native mounts are not supported on this platform; forks use full copies \
         and Safe Mode (dry_run) is unavailable."
    }
}

/// Root for copy-mode fork directories (a sibling of the mount root).
pub fn forks_copy_root(repo_root: &Path) -> Option<PathBuf> {
    Some(forks_root(repo_root)?.join("copy"))
}

/// Captures the current content of `root` into a fork's overlay, so that
/// promote/resolve see it exactly as they would see writes through a mount.
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
    // A ProjFS virtualization root stops with the process that owned it, so
    // a dead daemon leaves nothing mounted over the repo to reap.
    #[cfg(windows)]
    {
        let _ = repo_root;
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
    // A ProjFS shadow dies with the daemon that projected it, so there is no
    // wedged mountpoint to probe for and nothing to force-unmount.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = repo;
    }
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
