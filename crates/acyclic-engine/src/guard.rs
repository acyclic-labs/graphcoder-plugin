//! Enforces `.acyclic/config.toml`'s `guarded_paths` at the native mount
//! layer, independent of `RoutedMountSource`'s routing.
//!
//! `RoutedMountSource` (in `acyclic-fs`) is a pure dispatcher with no
//! interposition, and its routing is single-level and non-overlaying — it
//! cannot layer a read-only view over part of a writable fork. Guarding
//! therefore wraps the *inner* source (a fork's `CheckoutMountSource`)
//! directly, before it is ever registered as a route or shadow mount, so
//! guarded prefixes work at any depth.

use acyclic_fs::kernel::FileMetadata;
use acyclic_fs::native_mount::MountAttributeWriteMode;
use acyclic_fs::{
    FileId, MountAttributePage, MountDirectoryPage, MountFilesystem, MountLookup, MountOpenFile,
    MountPath, MountRangeAllocation, MountSeekTarget, MountSourceError,
};
use bytes::Bytes;
use std::path::Path;
use std::sync::Arc;

fn guarded_error() -> MountSourceError {
    MountSourceError::Unsupported(format!("path is guarded by {}", crate::product::repo_config_file()))
}

/// One guarded path prefix, pre-split into components for comparison
/// against a [`MountPath`] without re-splitting on every check.
struct GuardedPrefix {
    components: Vec<Vec<u8>>,
}

impl GuardedPrefix {
    fn matches(&self, components: &[Vec<u8>]) -> bool {
        components.len() >= self.components.len()
            && components
                .iter()
                .zip(self.components.iter())
                .all(|(component, prefix)| component == prefix)
    }
}

fn parse_guarded_path(path: &str) -> Vec<Vec<u8>> {
    path.trim()
        .trim_matches('/')
        .split('/')
        .filter(|component| !component.is_empty())
        .map(|component| component.as_bytes().to_vec())
        .collect()
}

/// Wraps one [`MountFilesystem`] and rejects mutating calls under any
/// configured guarded prefix.
pub struct GuardedMountFilesystem {
    inner: Arc<dyn MountFilesystem>,
    guarded: Vec<GuardedPrefix>,
}

impl GuardedMountFilesystem {
    /// Creates one guard from `.acyclic/config.toml`'s `guarded_paths`
    /// (POSIX-style paths relative to the repo root, e.g. `.env`,
    /// `migrations/`). An empty list guards nothing.
    #[must_use]
    pub fn new(inner: Arc<dyn MountFilesystem>, guarded_paths: &[String]) -> Self {
        let guarded = guarded_paths
            .iter()
            .map(|path| GuardedPrefix {
                components: parse_guarded_path(path),
            })
            .filter(|prefix| !prefix.components.is_empty())
            .collect();
        Self { inner, guarded }
    }

    /// Whether wrapping a projection in the guard adds anything. Always true:
    /// even with no configured prefixes the guard drops macOS AppleDouble
    /// sidecars, which every mount over the NFS transport would otherwise
    /// capture. Kept as a predicate so the wrap sites read intently and a
    /// future zero-cost fast path has a single place to live.
    #[must_use]
    pub fn is_active(_guarded_paths: &[String]) -> bool {
        true
    }

    fn is_guarded(&self, path: &MountPath) -> bool {
        let components = path.components();
        self.guarded.iter().any(|prefix| prefix.matches(components))
    }

    /// Whether a mutating call to `path` must be refused: either it falls
    /// under a configured guarded prefix, or its leaf is a macOS AppleDouble
    /// sidecar (`._X`). Sidecars are written by the macOS client over the
    /// mount to carry a file's xattrs / resource fork; in a Safe Mode or fork
    /// projection they are pure transport noise that would otherwise pollute
    /// the session diff and litter the real tree on apply, and a guarded
    /// file's metadata must not leak into one either. Dropping every sidecar
    /// covers both at once.
    fn blocked(&self, path: &MountPath) -> bool {
        self.is_guarded(path) || is_appledouble(path)
    }

    fn guard(&self, path: &MountPath) -> Result<(), MountSourceError> {
        if self.blocked(path) {
            Err(guarded_error())
        } else {
            Ok(())
        }
    }
}

/// A path whose final component is a macOS AppleDouble sidecar (`._name`).
fn is_appledouble(path: &MountPath) -> bool {
    path.components()
        .last()
        .is_some_and(|leaf| leaf.starts_with(b"._"))
}

/// Wraps one open file handle so writes through it are still checked, even
/// though [`MountOpenFile`]'s methods carry no path of their own.
struct GuardedOpenFile {
    inner: Arc<dyn MountOpenFile>,
    guarded: bool,
}

impl MountOpenFile for GuardedOpenFile {
    fn lookup(&self) -> Result<MountLookup, MountSourceError> {
        self.inner.lookup()
    }

    fn read_range(&self, offset: u64, length: u32) -> Result<Bytes, MountSourceError> {
        self.inner.read_range(offset, length)
    }

    fn seek(&self, offset: u64, target: MountSeekTarget) -> Result<Option<u64>, MountSourceError> {
        self.inner.seek(offset, target)
    }

    fn write_range(&self, offset: u64, bytes: Bytes) -> Result<(), MountSourceError> {
        if self.guarded {
            return Err(guarded_error());
        }
        self.inner.write_range(offset, bytes)
    }

    fn resize(&self, logical_bytes: u64) -> Result<(), MountSourceError> {
        if self.guarded {
            return Err(guarded_error());
        }
        self.inner.resize(logical_bytes)
    }

    fn allocate_range(
        &self,
        offset: u64,
        length: u64,
        operation: MountRangeAllocation,
    ) -> Result<(), MountSourceError> {
        if self.guarded {
            return Err(guarded_error());
        }
        self.inner.allocate_range(offset, length, operation)
    }

    fn set_attributes(
        &self,
        metadata: FileMetadata,
        logical_bytes: Option<u64>,
    ) -> Result<(), MountSourceError> {
        if self.guarded {
            return Err(guarded_error());
        }
        self.inner.set_attributes(metadata, logical_bytes)
    }

    fn read_attribute(&self, name: &[u8]) -> Result<Option<Bytes>, MountSourceError> {
        self.inner.read_attribute(name)
    }

    fn list_attributes(
        &self,
        cursor: Option<&[u8]>,
        maximum_entries: u32,
    ) -> Result<MountAttributePage, MountSourceError> {
        self.inner.list_attributes(cursor, maximum_entries)
    }

    fn write_attribute(
        &self,
        name: &[u8],
        value: Bytes,
        mode: MountAttributeWriteMode,
    ) -> Result<(), MountSourceError> {
        if self.guarded {
            return Err(guarded_error());
        }
        self.inner.write_attribute(name, value, mode)
    }

    fn remove_attribute(&self, name: &[u8]) -> Result<(), MountSourceError> {
        if self.guarded {
            return Err(guarded_error());
        }
        self.inner.remove_attribute(name)
    }
}

impl MountFilesystem for GuardedMountFilesystem {
    fn lookup(&self, path: &MountPath) -> Result<Option<MountLookup>, MountSourceError> {
        self.inner.lookup(path)
    }

    fn open_file(&self, path: &MountPath) -> Result<Arc<dyn MountOpenFile>, MountSourceError> {
        let inner = self.inner.open_file(path)?;
        Ok(Arc::new(GuardedOpenFile {
            inner,
            guarded: self.blocked(path),
        }))
    }

    fn detach_file(&self, path: &MountPath) -> Result<Arc<dyn MountOpenFile>, MountSourceError> {
        let guarded = self.blocked(path);
        if guarded {
            return Err(guarded_error());
        }
        let inner = self.inner.detach_file(path)?;
        Ok(Arc::new(GuardedOpenFile {
            inner,
            guarded: false,
        }))
    }

    fn read_link(&self, path: &MountPath) -> Result<Bytes, MountSourceError> {
        self.inner.read_link(path)
    }

    fn read_range(
        &self,
        path: &MountPath,
        offset: u64,
        length: u32,
    ) -> Result<Bytes, MountSourceError> {
        self.inner.read_range(path, offset, length)
    }

    fn seek(
        &self,
        path: &MountPath,
        offset: u64,
        target: MountSeekTarget,
    ) -> Result<Option<u64>, MountSourceError> {
        self.inner.seek(path, offset, target)
    }

    fn read_directory(
        &self,
        path: &MountPath,
        cursor: Option<&[u8]>,
        maximum_entries: u32,
    ) -> Result<MountDirectoryPage, MountSourceError> {
        self.inner.read_directory(path, cursor, maximum_entries)
    }

    fn create_file(
        &self,
        path: &MountPath,
        metadata: FileMetadata,
    ) -> Result<MountLookup, MountSourceError> {
        self.guard(path)?;
        self.inner.create_file(path, metadata)
    }

    fn create_directory(
        &self,
        path: &MountPath,
        metadata: FileMetadata,
    ) -> Result<MountLookup, MountSourceError> {
        self.guard(path)?;
        self.inner.create_directory(path, metadata)
    }

    fn create_symbolic_link(
        &self,
        path: &MountPath,
        target: Bytes,
        metadata: FileMetadata,
    ) -> Result<MountLookup, MountSourceError> {
        self.guard(path)?;
        self.inner.create_symbolic_link(path, target, metadata)
    }

    fn create_special(
        &self,
        path: &MountPath,
        kind: acyclic_fs::MountNodeKind,
        device: Option<(u32, u32)>,
        metadata: FileMetadata,
    ) -> Result<MountLookup, MountSourceError> {
        self.guard(path)?;
        self.inner.create_special(path, kind, device, metadata)
    }

    fn set_attributes(
        &self,
        path: &MountPath,
        metadata: FileMetadata,
        logical_bytes: Option<u64>,
    ) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.set_attributes(path, metadata, logical_bytes)
    }

    fn read_attribute(
        &self,
        path: &MountPath,
        name: &[u8],
    ) -> Result<Option<Bytes>, MountSourceError> {
        self.inner.read_attribute(path, name)
    }

    fn list_attributes(
        &self,
        path: &MountPath,
        cursor: Option<&[u8]>,
        maximum_entries: u32,
    ) -> Result<MountAttributePage, MountSourceError> {
        self.inner.list_attributes(path, cursor, maximum_entries)
    }

    fn write_attribute(
        &self,
        path: &MountPath,
        name: &[u8],
        value: Bytes,
        mode: MountAttributeWriteMode,
    ) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.write_attribute(path, name, value, mode)
    }

    fn remove_attribute(&self, path: &MountPath, name: &[u8]) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.remove_attribute(path, name)
    }

    fn write_range(
        &self,
        path: &MountPath,
        offset: u64,
        bytes: Bytes,
    ) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.write_range(path, offset, bytes)
    }

    fn resize(&self, path: &MountPath, logical_bytes: u64) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.resize(path, logical_bytes)
    }

    fn allocate_range(
        &self,
        path: &MountPath,
        offset: u64,
        length: u64,
        operation: MountRangeAllocation,
    ) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.allocate_range(path, offset, length, operation)
    }

    fn clone_range(
        &self,
        source: &MountPath,
        source_offset: u64,
        destination: &MountPath,
        destination_offset: u64,
        length: u64,
    ) -> Result<(), MountSourceError> {
        self.guard(destination)?;
        self.inner.clone_range(
            source,
            source_offset,
            destination,
            destination_offset,
            length,
        )
    }

    // `clone_range_by_id` carries no path, only stable `FileId`s already
    // resolved by a prior lookup/open -- this wrapper keeps no id->path
    // index, so it cannot check a destination id against guarded prefixes.
    // Fail closed whenever any guard is configured: the caller (a reflink
    // accelerator) falls back to a plain read+write copy through
    // `write_range`, which *is* guarded.
    #[allow(clippy::too_many_arguments)]
    fn clone_range_by_id(
        &self,
        source_file_id: FileId,
        source_offset: u64,
        destination_file_id: FileId,
        destination_offset: u64,
        length: u64,
    ) -> Result<(), MountSourceError> {
        if !self.guarded.is_empty() {
            return Err(guarded_error());
        }
        self.inner.clone_range_by_id(
            source_file_id,
            source_offset,
            destination_file_id,
            destination_offset,
            length,
        )
    }

    fn remove(&self, path: &MountPath, expected: Option<FileId>) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.remove(path, expected)
    }

    fn rename(
        &self,
        source: &MountPath,
        destination: &MountPath,
        replace: bool,
    ) -> Result<(), MountSourceError> {
        self.guard(source)?;
        self.guard(destination)?;
        self.inner.rename(source, destination, replace)
    }

    fn hard_link(
        &self,
        source: &MountPath,
        destination: &MountPath,
    ) -> Result<(), MountSourceError> {
        self.guard(source)?;
        self.guard(destination)?;
        self.inner.hard_link(source, destination)
    }

    fn flush(&self) -> Result<(), MountSourceError> {
        self.inner.flush()
    }

    fn capture_host_path(
        &self,
        source_root: &Path,
        path: &MountPath,
    ) -> Result<(), MountSourceError> {
        self.guard(path)?;
        self.inner.capture_host_path(source_root, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acyclic_fs::model::{
        AccessMode, CheckoutMode, ConsistencyMode, GenerationSelector, Lifecycle, MutationMode,
        VolumeConfig,
    };
    use acyclic_fs::{CancellationToken, CheckoutMountSource, Fs, SharedCheckout, WorkBudget};

    fn metadata() -> FileMetadata {
        FileMetadata::default()
    }

    fn test_path(components: &[&str]) -> MountPath {
        let mut path = MountPath::root();
        for component in components {
            path = path.child(component.as_bytes().to_vec());
        }
        path
    }

    fn guarded_source(
        guarded_paths: &[String],
    ) -> Result<GuardedMountFilesystem, Box<dyn std::error::Error>> {
        let config = VolumeConfig::portable(Lifecycle::Ephemeral);
        let fs = Fs::memory();
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        let checkout = runtime.block_on(async {
            let cancellation = CancellationToken::new();
            let volume = fs
                .create_volume(config, WorkBudget::UNBOUNDED, &cancellation)
                .await?
                .value;
            volume
                .checkout(
                    GenerationSelector::Head,
                    CheckoutMode {
                        access: AccessMode::ReadWrite,
                        consistency: ConsistencyMode::Pinned,
                        mutations: MutationMode::PrivateOverlay,
                    },
                    WorkBudget::UNBOUNDED,
                    &cancellation,
                )
                .await
                .map(|receipt| receipt.value)
        })?;
        let shared = Arc::new(SharedCheckout::new(checkout));
        let inner = Arc::new(CheckoutMountSource::new(shared, config)?);
        Ok(GuardedMountFilesystem::new(inner, guarded_paths))
    }

    #[test]
    fn write_to_guarded_file_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let guard = guarded_source(&[".env".to_owned()])?;
        let path = test_path(&[".env"]);
        assert!(matches!(
            guard.create_file(&path, metadata()),
            Err(MountSourceError::Unsupported(_))
        ));
        Ok(())
    }

    #[test]
    fn write_to_guarded_directory_is_rejected_at_any_depth(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let guard = guarded_source(&["migrations".to_owned()])?;
        let deep = test_path(&["migrations", "2024", "001_init.sql"]);
        assert!(matches!(
            guard.create_file(&deep, metadata()),
            Err(MountSourceError::Unsupported(_))
        ));
        Ok(())
    }

    #[test]
    fn write_outside_guarded_paths_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        let guard = guarded_source(&[".env".to_owned()])?;
        let path = test_path(&["README.md"]);
        assert!(guard.create_file(&path, metadata()).is_ok());
        Ok(())
    }

    #[test]
    fn open_file_write_through_handle_is_also_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let guard = guarded_source(&[".env".to_owned()])?;
        let path = test_path(&[".env"]);
        // Create the file through the guard's own inner checkout directly,
        // as if it predates the guard being configured.
        guard.inner.create_file(&path, metadata())?;
        // open_file itself is a read/open operation and succeeds, but the
        // returned handle must still refuse a write reached through it.
        let handle = guard.open_file(&path)?;
        assert!(matches!(
            handle.write_range(0, Bytes::from_static(b"x")),
            Err(MountSourceError::Unsupported(_))
        ));
        Ok(())
    }

    #[test]
    fn appledouble_sidecars_are_always_rejected() -> Result<(), Box<dyn std::error::Error>> {
        // Every `._X` sidecar is transport noise and must be refused —
        // whether or not its base is guarded, at the root or nested — while
        // the ordinary files beside them stay writable.
        let guard = guarded_source(&[".env".to_owned(), "migrations".to_owned()])?;
        for sidecar in [
            test_path(&["._.env"]),
            test_path(&["._migrations"]),
            test_path(&["._notes.txt"]),
            test_path(&["src", "._main.rs"]),
        ] {
            assert!(
                matches!(
                    guard.create_file(&sidecar, metadata()),
                    Err(MountSourceError::Unsupported(_))
                ),
                "sidecar {sidecar:?} must be refused"
            );
        }
        assert!(guard.create_file(&test_path(&["notes.txt"]), metadata()).is_ok());
        Ok(())
    }

    #[test]
    fn guard_wrapping_always_applies_to_drop_sidecars() {
        // Always active: even with no configured prefixes the guard is worth
        // wrapping because it drops AppleDouble sidecars.
        assert!(GuardedMountFilesystem::is_active(&[]));
        assert!(GuardedMountFilesystem::is_active(&[".env".to_owned()]));
    }
}
