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
    CancellationToken, Checkout, LocalAuthorityBackend, LocalFs, LocalObjectBackend, LocalOptions,
    VolumeId, WorkCounters,
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
        let base = match stores_root {
            Some(path) => path.to_path_buf(),
            None => {
                let home = std::env::var_os("HOME")
                    .ok_or_else(|| EngineError::Store("HOME is not set".into()))?;
                Path::new(&home).join(".local/share/acyclic/stores")
            }
        };
        let canonical = repo_root
            .canonicalize()
            .map_err(|error| EngineError::Store(format!("canonicalize repo root: {error}")))?;
        let digest = blake3::hash(canonical.as_os_str().as_encoded_bytes());
        let short = &digest.to_hex()[..16];
        Ok(Self {
            root: base.join(short.to_string()),
        })
    }

    pub fn object_store(&self) -> PathBuf {
        self.root.join("store")
    }
    pub fn index_db(&self) -> PathBuf {
        self.root.join("index.db")
    }
    pub fn socket(&self) -> PathBuf {
        self.root.join("daemon.sock")
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

fn writable_head() -> CheckoutMode {
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
        let fs = LocalFs::local(LocalOptions::new(paths.object_store()))
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
        let fs = LocalFs::local(LocalOptions::new(paths.object_store()))
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
        let paths =
            StorePaths::for_repo(repo.path(), Some(stores.path())).expect("resolve paths");

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
        let paths =
            StorePaths::for_repo(repo.path(), Some(stores.path())).expect("resolve paths");
        Store::init(repo.path(), paths.clone()).await.expect("init");
        assert!(Store::init(repo.path(), paths).await.is_err());
    }
}
