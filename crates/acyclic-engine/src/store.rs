//! Store lifecycle: where engine state lives and how the volume opens.
//!
//! Everything lives OUTSIDE the working tree (capture snapshots the whole
//! tree and fail-closes on sockets). The repo carries only `.acyclic/config.toml`.

use std::path::{Path, PathBuf};

use acyclic_fs::model::{
    AccessMode, CheckoutMode, ConsistencyMode, FilesystemProfile, GenerationSelector, Lifecycle,
    MutationMode, VolumeConfig,
};
use acyclic_fs::{
    CancellationToken, Checkout, LocalAuthorityBackend, LocalFs, LocalObjectBackend,
    LocalObjectsDurability, LocalOptions, LocalStreamDurability, VolumeId, WorkCounters,
};
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result};

/// Concrete checkout type for the local backend.
pub type LocalCheckout = Checkout<LocalAuthorityBackend, LocalObjectBackend>;
/// Concrete volume type for the local backend.
pub type LocalVolume = acyclic_fs::LocalVolume;

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
        let base = if let Some(path) = stores_root {
            path.to_path_buf()
        } else {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| EngineError::Store("HOME is not set".into()))?;
            Path::new(&home).join(format!(".local/share/{}/stores", crate::product::NAME))
        };
        let canonical = repo_root
            .canonicalize()
            .map_err(|error| EngineError::Store(format!("canonicalize repo root: {error}")))?;
        let digest = blake3::hash(canonical.as_os_str().as_encoded_bytes());
        let short = &digest.to_hex()[..16];
        Ok(Self {
            root: base.join(short),
        })
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
    pub volume: LocalVolume,
    pub checkout: LocalCheckout,
    pub volume_id: VolumeId,
    pub paths: StorePaths,
    pub repo_root: PathBuf,
}

fn volume_config() -> VolumeConfig {
    let mut config = VolumeConfig {
        profile: FilesystemProfile::Posix,
        ..VolumeConfig::portable(Lifecycle::Durable)
    };
    config.limits.maximum_mutations_per_batch = MUTATIONS_PER_BATCH;
    config.limits.maximum_paths_per_batch = MUTATIONS_PER_BATCH;
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

/// Barrier durability for both providers: a full device flush per journal
/// frame costs ~5ms each on Apple SSDs and a small capture issues dozens,
/// while the store only needs to survive a daemon crash — a torn tail after
/// power loss just drops the newest checkpoint.
pub fn local_options(root: impl Into<PathBuf>) -> LocalOptions {
    let mut options = LocalOptions::new(root);
    options.stream.durability = LocalStreamDurability::Barrier;
    options.objects.durability = LocalObjectsDurability::Barrier;
    options
}

impl Store {
    /// Creates the store for a repo: directories, volume, meta record.
    /// Fails if the store already exists.
    pub async fn init(repo_root: &Path, paths: StorePaths) -> Result<Self> {
        if paths.meta().exists() {
            return Err(EngineError::Store(format!(
                "store already initialized at {}",
                paths.root.display()
            )));
        }
        std::fs::create_dir_all(paths.object_store())?;
        std::fs::create_dir_all(paths.trash())?;

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
        let checkout = volume
            .checkout(
                GenerationSelector::Head,
                writable_head(),
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(EngineError::fs("checkout head"))?
            .value;

        let repo_root = repo_root.canonicalize()?;
        let meta = StoreMeta {
            schema: 1,
            repo_root: repo_root.clone(),
            volume_id,
        };
        atomic_write_json(&paths.meta(), &meta)?;
        Ok(Self {
            fs,
            volume,
            checkout,
            volume_id,
            paths,
            repo_root,
        })
    }

    /// Opens an existing store recorded in `meta.json`.
    pub async fn open(paths: StorePaths) -> Result<Self> {
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

        let cancel = CancellationToken::new();
        let fs = LocalFs::local(local_options(paths.object_store()))
            .await
            .map_err(EngineError::fs("open object store"))?;
        let volume = fs
            .open_volume(meta.volume_id, WorkCounters::UNBOUNDED, &cancel)
            .await
            .map_err(EngineError::fs("open volume"))?
            .value;
        let checkout = volume
            .checkout(
                GenerationSelector::Head,
                writable_head(),
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(EngineError::fs("checkout head"))?
            .value;
        Ok(Self {
            fs,
            volume,
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
        let cancel = CancellationToken::new();
        Ok(self
            .volume
            .checkout(
                GenerationSelector::Exact(generation),
                read_only(),
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(EngineError::fs("checkout exact"))?
            .value)
    }
}

/// Short per-user directory for daemon sockets. Created 0700 on first use.
///
/// Deliberately NOT `std::env::temp_dir()`: that honors `TMPDIR`, which can
/// be arbitrarily deep, and `sun_path` is capped (~104 bytes on macOS). The
/// path must be short and identical across every process of this user.
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

        let reopened = Store::open(paths).await.expect("open");
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
}
