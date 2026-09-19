//! Store lifecycle: where engine state lives and how the volume opens.
//!
//! Everything lives OUTSIDE the working tree (capture snapshots the whole
//! tree and fail-closes on sockets). The repo carries only `.acyclic/config.toml`.

use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::{fs::OpenOptions, io::Write};

use acyclic_fs::model::{
    AccessMode, CheckoutMode, ConsistencyMode, GenerationSelector, Lifecycle, MutationMode,
    VolumeConfig,
};
use acyclic_fs::{
    CancellationToken, Checkout, LocalAuthorityBackend, LocalFs, LocalObjectBackend,
    LocalObjectsDurability, LocalOptions, LocalStreamDurability, VolumeId, WorkCounters,
    WorkspaceRestore,
};
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result};

/// Concrete checkout type for the local backend.
pub type LocalCheckout = Checkout<LocalAuthorityBackend, LocalObjectBackend>;
/// Concrete workspace type for the local backend.
pub type LocalWorkspace = acyclic_fs::Workspace<LocalAuthorityBackend, LocalObjectBackend>;
/// Concrete immutable generation type for the local backend.
pub type LocalGeneration = acyclic_fs::Generation<LocalAuthorityBackend, LocalObjectBackend>;

/// Batch limits sized for large monorepos: the fs defaults (2,048) reject any
/// baseline capture beyond ~2k paths. Immutable per volume — size generously.
const MUTATIONS_PER_BATCH: u32 = 4_194_304;

/// Filesystem layout of one repo's store.
#[derive(Clone, Debug)]
pub struct StorePaths {
    /// Store root: `<stores>/<repo-path-hash>/`.
    pub root: PathBuf,
}

impl StorePaths {
    /// Resolves the store root for a repo. `stores_root` override comes from
    /// config; the default is `~/.local/share/acyclic/stores`.
    pub fn for_repo(repo_root: &Path, stores_root: Option<&Path>) -> Result<Self> {
        let canonical = repo_root
            .canonicalize()
            .map_err(|error| EngineError::Store(format!("canonicalize repo root: {error}")))?;
        let base = if let Some(path) = stores_root {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                canonical.join(path)
            }
        } else {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| EngineError::Store("HOME is not set".into()))?;
            Path::new(&home).join(format!(".local/share/{}/stores", crate::product::NAME))
        };
        let base = canonicalize_planned(&base)?;
        let digest = blake3::hash(canonical.as_os_str().as_encoded_bytes());
        let hex = digest.to_hex();
        let short = hex.get(..16).unwrap_or(&hex);
        let paths = Self {
            root: base.join(short),
        };
        paths.ensure_outside_repo(&canonical)?;
        Ok(paths)
    }

    fn ensure_outside_repo(&self, repo_root: &Path) -> Result<()> {
        let repo = repo_root.canonicalize()?;
        for path in [
            self.root.clone(),
            self.object_store(),
            self.index_db(),
            self.spec_db(),
            self.spec_runs(),
            self.pidfile(),
            self.rewind_journal(),
            self.trash(),
            self.meta(),
            #[cfg(target_os = "windows")]
            self.continuity(),
            self.root.join("daemon.log"),
        ] {
            let resolved = canonicalize_planned(&path)?;
            if resolved.starts_with(&repo) {
                return Err(EngineError::Store(format!(
                    "store must be outside the repository: {}",
                    resolved.display()
                )));
            }
        }
        Ok(())
    }

    pub fn object_store(&self) -> PathBuf {
        self.root.join("store")
    }
    pub fn index_db(&self) -> PathBuf {
        self.root.join("index.db")
    }
    /// The daemon socket lives in a short per-user runtime directory, NOT in
    /// the store: `sun_path` is capped (~104 bytes on macOS) and store roots
    /// can be arbitrarily deep.
    pub fn socket(&self) -> PathBuf {
        let store_key = self.root.file_name().map_or_else(
            || "default".into(),
            |name| name.to_string_lossy().into_owned(),
        );
        runtime_dir().join(format!("{store_key}.sock"))
    }
    pub fn pidfile(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }
    pub fn rewind_journal(&self) -> PathBuf {
        self.root.join("rewind-journal.json")
    }
    pub fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }
    pub fn meta(&self) -> PathBuf {
        self.root.join("meta.json")
    }
    #[cfg(target_os = "windows")]
    pub fn continuity(&self) -> PathBuf {
        self.root.join("continuity.bin")
    }
    /// Speculation cache. Deliberately NOT a table in `index_db`: the
    /// pipeline thread owns that connection and writes to it synchronously,
    /// so a second writer contending for `SQLite`'s write lock would block the
    /// thread every hook call waits on. See `crate::spec`.
    pub fn spec_db(&self) -> PathBuf {
        self.root.join("spec.db")
    }
    /// Scratch and pid files for in-flight speculative child processes.
    pub fn spec_runs(&self) -> PathBuf {
        self.root.join("spec")
    }
}

fn canonicalize_planned(path: &Path) -> std::io::Result<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        match cursor.canonicalize() {
            Ok(mut resolved) => {
                for name in missing.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if cursor
                    .symlink_metadata()
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(std::io::Error::other("dangling store path symlink"));
                }
                let name = cursor.file_name().ok_or(error)?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| std::io::Error::other("store path has no existing ancestor"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Persisted store identity. The `VolumeId` MUST survive restarts:
/// generations only resolve on their own volume.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreMeta {
    pub schema: u32,
    pub repo_root: PathBuf,
    pub volume_id: VolumeId,
}

/// An opened store: the fs engine, its volume, and a writable Head checkout.
pub struct Store {
    pub fs: LocalFs,
    pub workspace: LocalWorkspace,
    pub checkout: LocalCheckout,
    pub volume_id: VolumeId,
    pub paths: StorePaths,
    pub repo_root: PathBuf,
}

/// The volume every store opens with.
///
/// The profile is per-platform and decides how names are encoded on the way
/// in and out — see [`crate::names`], which every caller that mints a name
/// must go through. It is fixed for the life of a volume: a store created
/// under one profile cannot be reopened under another, so this must never
/// become a runtime choice.
pub(crate) fn volume_config() -> VolumeConfig {
    let mut config = VolumeConfig {
        profile: crate::names::profile(),
        ..VolumeConfig::portable(Lifecycle::Durable)
    };
    config.limits.maximum_mutations_per_batch = MUTATIONS_PER_BATCH;
    config.limits.maximum_paths_per_batch = MUTATIONS_PER_BATCH;
    config.limits.maximum_component_bytes = crate::names::maximum_component_bytes();
    config
}

pub(crate) fn writable_head() -> CheckoutMode {
    CheckoutMode {
        access: AccessMode::ReadWrite,
        consistency: ConsistencyMode::TrackingSafe,
        mutations: MutationMode::PrivateOverlay,
    }
}

/// Read-only pinned mode for historical generations.
pub fn read_only() -> CheckoutMode {
    CheckoutMode {
        access: AccessMode::ReadOnly,
        consistency: ConsistencyMode::Pinned,
        mutations: MutationMode::None,
    }
}

/// Exact local durability supported by this platform.
pub fn local_options(root: impl Into<PathBuf>) -> LocalOptions {
    let mut options = LocalOptions::new(root);
    #[cfg(target_vendor = "apple")]
    {
        // Apple exposes an ordered barrier that does not drain the device cache.
        options.stream.durability = LocalStreamDurability::Barrier;
        options.objects.durability = LocalObjectsDurability::Barrier;
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        options.stream.durability = LocalStreamDurability::FullFlush;
        options.objects.durability = LocalObjectsDurability::FullFlush;
    }
    options
}

impl Store {
    /// Reconciles an SDK head after crash recovery completed a host tree swap.
    pub async fn recover_workspace_head(
        &mut self,
        recovered: &crate::rewind::RecoveredSwap,
    ) -> Result<()> {
        if !recovered.published {
            return Ok(());
        }
        let current = self
            .workspace
            .head()
            .await
            .map_err(EngineError::fs("workspace head"))?;
        if current.id() != recovered.target {
            let target = self
                .workspace
                .generation(recovered.target)
                .await
                .map_err(EngineError::fs("recover rewind generation"))?;
            let key = acyclic_fs::IdempotencyKey::from_bytes(
                recovered.target.digest().as_bytes()[..16]
                    .try_into()
                    .map_err(|_| EngineError::Store("invalid generation digest".into()))?,
            );
            match self
                .workspace
                .restore_generation(&target, current.id(), key)
                .await
                .map_err(EngineError::fs("recover workspace rewind"))?
            {
                WorkspaceRestore::Restored(_)
                | WorkspaceRestore::AlreadyRestored(_)
                | WorkspaceRestore::Current(_) => {}
                WorkspaceRestore::Stale(_)
                | WorkspaceRestore::Fenced
                | WorkspaceRestore::IdempotencyConflict => {
                    return Err(EngineError::Store(
                        "workspace head changed during rewind recovery".into(),
                    ));
                }
            }
        }
        self.checkout = self
            .workspace
            .checkout(GenerationSelector::Head, writable_head())
            .await
            .map_err(EngineError::fs("refresh recovered workspace"))?;
        Ok(())
    }

    /// Creates the store for a repo: directories, volume, meta record.
    /// Fails if the store already exists.
    pub async fn init(repo_root: &Path, paths: StorePaths) -> Result<Self> {
        paths.ensure_outside_repo(repo_root)?;
        if paths.meta().exists() {
            return Err(EngineError::Store(format!(
                "store already initialized at {}",
                paths.root.display()
            )));
        }
        std::fs::create_dir_all(&paths.root)?;
        paths.ensure_outside_repo(repo_root)?;
        std::fs::create_dir_all(paths.object_store())?;
        std::fs::create_dir_all(paths.trash())?;
        paths.ensure_outside_repo(repo_root)?;

        let cancel = CancellationToken::new();
        let fs = LocalFs::local(local_options(paths.object_store()))
            .await
            .map_err(EngineError::fs("open object store"))?;
        let volume_id = VolumeId::new();
        let volume = fs
            .create_volume_with_id(volume_id, volume_config(), WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(EngineError::fs("create volume"))?
            .value;
        let workspace = fs
            .adopt_volume_workspace("main", volume)
            .map_err(EngineError::fs("adopt workspace"))?;
        let checkout = workspace
            .checkout(GenerationSelector::Head, writable_head())
            .await
            .map_err(EngineError::fs("checkout head"))?;

        let repo_root = repo_root.canonicalize()?;
        let meta = StoreMeta {
            schema: 1,
            repo_root: repo_root.clone(),
            volume_id,
        };
        atomic_write_json(&paths.meta(), &meta)?;
        Ok(Self {
            fs,
            workspace,
            checkout,
            volume_id,
            paths,
            repo_root,
        })
    }

    /// Opens an existing store recorded in `meta.json`.
    pub async fn open(repo_root: &Path, paths: StorePaths) -> Result<Self> {
        paths.ensure_outside_repo(repo_root)?;
        let text = std::fs::read_to_string(paths.meta()).map_err(|error| {
            EngineError::Store(format!(
                "no store at {} ({error}); run init first",
                paths.root.display()
            ))
        })?;
        let meta: StoreMeta = serde_json::from_str(&text)
            .map_err(|error| EngineError::Store(format!("meta.json: {error}")))?;
        if meta.schema != 1 {
            return Err(EngineError::Store(format!(
                "unsupported store schema {}",
                meta.schema
            )));
        }
        let expected_repo = repo_root.canonicalize()?;
        if meta.repo_root.canonicalize()? != expected_repo {
            return Err(EngineError::Store(
                "store belongs to a different repository".into(),
            ));
        }
        paths.ensure_outside_repo(&expected_repo)?;

        let cancel = CancellationToken::new();
        let phase = std::time::Instant::now();
        let fs = LocalFs::local(local_options(paths.object_store()))
            .await
            .map_err(EngineError::fs("open object store"))?;
        let objects_ms = crate::trace::ms(phase);
        let phase = std::time::Instant::now();
        let volume = fs
            .open_volume(meta.volume_id, WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(EngineError::fs("open volume"))?
            .value;
        let volume_ms = crate::trace::ms(phase);
        let phase = std::time::Instant::now();
        let workspace = fs
            .adopt_volume_workspace("main", volume)
            .map_err(EngineError::fs("adopt workspace"))?;
        let checkout = workspace
            .checkout(GenerationSelector::Head, writable_head())
            .await
            .map_err(EngineError::fs("checkout head"))?;
        crate::trace!(
            "store",
            "open: object store {objects_ms:.1}ms, volume {volume_ms:.1}ms, head checkout {:.1}ms",
            crate::trace::ms(phase)
        );
        Ok(Self {
            fs,
            workspace,
            checkout,
            volume_id: meta.volume_id,
            paths,
            repo_root: meta.repo_root,
        })
    }

    /// Opens a read-only checkout of one historical generation.
    pub async fn checkout_exact(
        &self,
        generation: acyclic_fs::GenerationId,
    ) -> Result<LocalCheckout> {
        self.workspace
            .checkout(GenerationSelector::Exact(generation), read_only())
            .await
            .map_err(EngineError::fs("checkout exact"))
    }

    /// Opens one authenticated immutable generation handle.
    pub async fn generation(
        &self,
        generation: acyclic_fs::GenerationId,
    ) -> Result<LocalGeneration> {
        self.workspace
            .generation(generation)
            .await
            .map_err(EngineError::fs("open exact generation"))
    }
}

/// Short per-user directory for daemon sockets. Created 0700 on first use.
///
/// Deliberately NOT `std::env::temp_dir()`: that honors `TMPDIR`, which can
/// be arbitrarily deep, and `sun_path` is capped (~104 bytes on macOS). The
/// path must be short and identical across every process of this user.
#[allow(unsafe_code, reason = "getuid() has no preconditions and cannot fail")]
pub fn runtime_dir() -> PathBuf {
    #[cfg(unix)]
    let dir = {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/{}-{uid}", crate::product::NAME))
    };
    #[cfg(not(unix))]
    let dir = std::env::temp_dir().join(crate::product::NAME);
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| EngineError::Store(format!("encode {}: {error}", path.display())))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn durable_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("bin.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = tmp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both paths are terminated UTF-16 buffers live for this call.
    #[allow(unsafe_code)]
    let replaced = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_then_open_preserves_volume_identity() {
        let repo = tempfile::tempdir().expect("repo dir");
        let stores = tempfile::tempdir().expect("stores dir");
        std::fs::write(repo.path().join("file.txt"), b"hi").expect("seed file");
        let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("resolve paths");

        let created = Store::init(repo.path(), paths.clone()).await.expect("init");
        let created_id = created.volume_id;
        drop(created);

        let reopened = Store::open(repo.path(), paths).await.expect("open");
        assert_eq!(reopened.volume_id, created_id);
        assert_eq!(
            reopened.repo_root,
            repo.path().canonicalize().expect("canonical repo")
        );
    }

    #[tokio::test]
    async fn double_init_is_refused() {
        let repo = tempfile::tempdir().expect("repo dir");
        let stores = tempfile::tempdir().expect("stores dir");
        let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("resolve paths");
        Store::init(repo.path(), paths.clone()).await.expect("init");
        assert!(Store::init(repo.path(), paths).await.is_err());
    }

    #[tokio::test]
    async fn store_inside_repo_is_rejected_on_init_and_open() {
        let repo = tempfile::tempdir().expect("repo dir");
        let paths = StorePaths {
            root: repo.path().join(".stores").join("fixture"),
        };
        assert!(StorePaths::for_repo(repo.path(), Some(&repo.path().join(".stores"))).is_err());
        assert!(matches!(
            Store::init(repo.path(), paths.clone()).await,
            Err(EngineError::Store(message)) if message.contains("outside the repository")
        ));
        assert!(!paths.root.exists());
        assert!(!paths.meta().exists());

        std::fs::create_dir_all(&paths.root).expect("create invalid root");
        atomic_write_json(
            &paths.meta(),
            &StoreMeta {
                schema: 1,
                repo_root: repo.path().canonicalize().expect("canonical repo"),
                volume_id: VolumeId::new(),
            },
        )
        .expect("seed meta");
        assert!(matches!(
            Store::open(repo.path(), paths).await,
            Err(EngineError::Store(message)) if message.contains("outside the repository")
        ));
    }

    #[test]
    fn relative_store_dir_resolves_from_repo_before_daemon_changes_cwd() {
        let repo = tempfile::tempdir().expect("repo dir");
        let paths = StorePaths::for_repo(repo.path(), Some(Path::new("../stores")))
            .expect("resolve relative store");
        assert!(paths.root.is_absolute());
        assert!(paths.root.starts_with(
            repo.path()
                .canonicalize()
                .expect("canonical repo")
                .parent()
                .expect("repo parent")
                .join("stores")
        ));
    }

    #[tokio::test]
    async fn store_open_rejects_a_different_requested_repository() {
        let repo = tempfile::tempdir().expect("repo dir");
        let other = tempfile::tempdir().expect("other repo dir");
        let stores = tempfile::tempdir().expect("stores dir");
        let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("resolve paths");
        drop(Store::init(repo.path(), paths.clone()).await.expect("init"));
        assert!(matches!(
            Store::open(other.path(), paths).await,
            Err(EngineError::Store(message)) if message.contains("different repository")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn store_child_symlink_into_repo_is_rejected() {
        let repo = tempfile::tempdir().expect("repo dir");
        let stores = tempfile::tempdir().expect("stores dir");
        let target = repo.path().join("misplaced-store");
        std::fs::create_dir(&target).expect("target dir");
        let paths = StorePaths::for_repo(repo.path(), Some(stores.path())).expect("resolve paths");
        std::fs::create_dir_all(&paths.root).expect("store root");
        std::os::unix::fs::symlink(&target, paths.object_store()).expect("store link");
        assert!(StorePaths::for_repo(repo.path(), Some(stores.path())).is_err());
        assert!(matches!(
            Store::init(repo.path(), paths).await,
            Err(EngineError::Store(message)) if message.contains("outside the repository")
        ));
        assert_eq!(
            std::fs::read_dir(target).expect("target entries").count(),
            0
        );
    }
}
