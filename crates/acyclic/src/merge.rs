//! Content-level merge (Merge v2, see docs/design/implementation-merge.md).
//!
//! Three generations take part: the fork base **B**, the mainline head
//! **H** ("theirs"), and the fork snapshot **F** ("ours"; the fork is
//! `ours` because it merges *into* the mainline, graphcoder's convention).
//! [`plan`] walks the union of changed paths, applies the entry-level
//! decision table, runs the SDK text driver on regular text files both sides
//! changed, and returns everything the daemon needs to either land the
//! merge or rebase the fork with conflict markers. Nothing here writes to
//! the working tree.
//!
//! The SDK owns marker and newline semantics so every consumer sees the same result.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use acyclic_fs::kernel::{FileKind, FileMetadata, FileRecord, MetadataField, NamespacePath};
pub use acyclic_fs::text_merge::{
    conflict_hunks, has_conflict_markers, ByteConflictKind as ConflictKind,
};
use acyclic_fs::text_merge::{
    merge_bytes, ByteMerge as ContentMerge, ByteMergeError, ByteMergeLimits,
};
use acyclic_fs::{ByteRange, CancellationToken, GenerationId, WorkCounters};
use bytes::Bytes;

use crate::diff;
use crate::rewind::{namespace_path, validate_relative};
use crate::store::{LocalCheckout, Store};
use crate::{EngineError, Result};

/// Default `[merge] max_file_bytes`.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const TRANSFER_BYTES: u64 = 8 * 1024 * 1024;
const PAGE_ENTRIES: u32 = 1_024;
// ---------------------------------------------------------------------------
// Per-path decision table
// ---------------------------------------------------------------------------

/// Knobs for the text gate.
#[derive(Clone, Copy, Debug)]
pub struct MergeLimits {
    pub max_file_bytes: u64,
}

impl Default for MergeLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// Which side a decision favors. The fork is `Ours`, the mainline `Theirs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Ours,
    Theirs,
}

/// Why a path cannot be merged at all. Refusals leave everything untouched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reason {
    /// File kinds differ across the sides (file vs symlink vs directory),
    /// or a symlink was retargeted on both sides.
    KindChange,
    /// Not UTF-8 text, or a NUL byte in the first 8 KiB.
    Binary,
    /// Larger than `[merge] max_file_bytes`.
    TooLarge,
    /// One side deleted a directory the other side changed inside.
    Ancestry { inner: Vec<PathBuf> },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::KindChange => write!(f, "kind changed on both sides"),
            Reason::Binary => write!(f, "binary"),
            Reason::TooLarge => write!(f, "too large"),
            Reason::Ancestry { inner } => {
                let shown: Vec<String> = inner
                    .iter()
                    .take(4)
                    .map(|p| p.display().to_string())
                    .collect();
                let more = inner.len().saturating_sub(shown.len());
                write!(
                    f,
                    "deleted on one side, changed inside on the other ({}{})",
                    shown.join(", "),
                    if more > 0 {
                        format!(" +{more} more")
                    } else {
                        String::new()
                    }
                )
            }
        }
    }
}

/// Merges one regular file both sides changed. `None` on a side means that
/// side deleted the file; `base` is `None` when both sides added it.
pub fn merge_file(
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
    ours_name: &str,
    theirs_name: &str,
    limits: &MergeLimits,
) -> std::result::Result<ContentMerge, Reason> {
    merge_bytes(
        base,
        ours,
        theirs,
        ours_name,
        theirs_name,
        ByteMergeLimits {
            max_bytes: limits.max_file_bytes,
        },
    )
    .map_err(|error| match error {
        ByteMergeError::Binary => Reason::Binary,
        ByteMergeError::TooLarge => Reason::TooLarge,
    })
}

// ---------------------------------------------------------------------------
// Planning over three generations
// ---------------------------------------------------------------------------

/// A file whose content the merge produced (clean).
#[derive(Clone, Debug)]
pub struct MergedFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub mode: Option<u32>,
}

/// A file whose merge conflicted; `bytes` carries the markers.
#[derive(Clone, Debug)]
pub struct ConflictedFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub hunks: u32,
    pub kind: ConflictKind,
    pub mode: Option<u32>,
}

impl ConflictedFile {
    /// One-line human description used in refusal reports.
    pub fn describe(&self) -> String {
        match self.kind {
            ConflictKind::Hunks => format!("{} conflicting hunk(s)", self.hunks),
            ConflictKind::TheirsDeleted => "mainline deleted, fork modified".into(),
            ConflictKind::OursDeleted => "fork deleted, mainline modified".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Refusal {
    pub path: PathBuf,
    pub reason: Reason,
}

/// Everything a promote onto a moved mainline needs, computed before any
/// write: subtree roots to take from each side, merged files, conflicted
/// files, and refusals. Roots never nest and never contain a merged path.
#[derive(Clone, Debug, Default)]
pub struct MergePlan {
    /// Subtree roots only the fork changed: replay from F.
    pub take_ours: Vec<PathBuf>,
    /// Subtree roots only the mainline changed: the fork lacks them (a
    /// rebase writes them into the fork).
    pub take_theirs: Vec<PathBuf>,
    pub merged: Vec<MergedFile>,
    pub conflicted: Vec<ConflictedFile>,
    pub refusals: Vec<Refusal>,
}

impl MergePlan {
    /// Drops refusals and conflicts on `ignored` paths (build artifacts,
    /// caches, secrets: things `.gitignore` says are not merge payload) so
    /// the mainline simply keeps its own copy. Returns the paths dropped.
    pub fn keep_mainline_for(&mut self, ignored: &[PathBuf]) -> Vec<PathBuf> {
        let mut kept = Vec::new();
        self.refusals.retain(|refusal| {
            let drop = ignored.contains(&refusal.path);
            if drop {
                kept.push(refusal.path.clone());
            }
            !drop
        });
        self.conflicted.retain(|file| {
            let drop = ignored.contains(&file.path);
            if drop {
                kept.push(file.path.clone());
            }
            !drop
        });
        kept.sort();
        kept
    }

    /// True when the mainline would be byte-identical after landing.
    pub fn lands_nothing(&self) -> bool {
        self.take_ours.is_empty() && self.merged.is_empty() && self.conflicted.is_empty()
    }
    /// Paths a landing writes onto the mainline, from M.
    pub fn landing_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.take_ours.clone();
        paths.extend(self.merged.iter().map(|file| file.path.clone()));
        paths.sort();
        paths
    }
}

/// Does `set` hold `path` or anything beneath it?
fn subtree_changed(set: &BTreeSet<PathBuf>, path: &Path) -> bool {
    set.range(path.to_path_buf()..)
        .next()
        .is_some_and(|next| next.starts_with(path))
}

fn descendants<'a>(
    set: &'a BTreeSet<PathBuf>,
    path: &'a Path,
) -> impl Iterator<Item = &'a PathBuf> + 'a {
    let owned = path.to_path_buf();
    let root = owned.clone();
    set.range(owned..)
        .take_while(move |next| next.starts_with(&root))
        .filter(move |next| next.as_path() != path)
}

async fn changed_paths(
    before: &crate::store::LocalGeneration,
    after: &crate::store::LocalGeneration,
) -> Result<BTreeSet<PathBuf>> {
    let changes = before
        .diff_to(after, u32::MAX)
        .await
        .map_err(EngineError::fs("diff merge generations"))?
        .changed_paths(u32::MAX)
        .await
        .map_err(EngineError::fs("resolve merge paths"))?;
    changes
        .into_iter()
        .filter(|change| match (change.before, change.after) {
            (Some(before), Some(after)) => {
                before.kind != after.kind
                    || (before.kind != FileKind::Directory && before.payload != after.payload)
            }
            _ => true,
        })
        .map(|change| {
            acyclic_fs::namespace_to_host_path(&change.path)
                .map_err(EngineError::fs("resolve merge host path"))
        })
        .filter(|path| match path {
            Ok(path) => !diff::is_git_internal(path),
            Err(_) => true,
        })
        .collect()
}

async fn records_at(
    generation: &crate::store::LocalGeneration,
    paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, FileRecord>> {
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let namespaces = paths
        .iter()
        .map(|path| namespace_of(path))
        .collect::<Result<Vec<_>>>()?;
    let records = generation
        .lookup_paths(&namespaces)
        .await
        .map_err(EngineError::fs("lookup merge paths"))?;
    Ok(paths
        .iter()
        .cloned()
        .zip(records)
        .filter_map(|(path, record)| record.map(|record| (path, record)))
        .collect())
}

struct MergeInputs {
    base: BTreeMap<PathBuf, FileRecord>,
    theirs: BTreeMap<PathBuf, FileRecord>,
    ours: BTreeMap<PathBuf, FileRecord>,
    ours_changed: BTreeSet<PathBuf>,
    theirs_changed: BTreeSet<PathBuf>,
}

async fn merge_inputs(
    store: &Store,
    base: GenerationId,
    theirs: GenerationId,
    ours: GenerationId,
) -> Result<MergeInputs> {
    let base_generation = store.generation(base).await?;
    let theirs_generation = store.generation(theirs).await?;
    let ours_generation = store.generation(ours).await?;
    let ours_changed = changed_paths(&base_generation, &ours_generation).await?;
    let theirs_changed = changed_paths(&base_generation, &theirs_generation).await?;
    let paths: Vec<_> = ours_changed.union(&theirs_changed).cloned().collect();
    Ok(MergeInputs {
        base: records_at(&base_generation, &paths).await?,
        theirs: records_at(&theirs_generation, &paths).await?,
        ours: records_at(&ours_generation, &paths).await?,
        ours_changed,
        theirs_changed,
    })
}

/// Computes the merge of fork `ours` onto mainline `theirs` from `base`.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per (base, ours, theirs) kind combination; the table reads best whole"
)]
pub async fn plan(
    store: &Store,
    base: GenerationId,
    theirs: GenerationId,
    ours: GenerationId,
    ours_name: &str,
    limits: &MergeLimits,
) -> Result<MergePlan> {
    let inputs = merge_inputs(store, base, theirs, ours).await?;
    let MergeInputs {
        base: base_map,
        theirs: theirs_map,
        ours: ours_map,
        ours_changed,
        theirs_changed,
    } = inputs;

    let mut base_checkout = store.checkout_exact(base).await?;
    let mut theirs_checkout = store.checkout_exact(theirs).await?;
    let mut ours_checkout = store.checkout_exact(ours).await?;

    let mut plan = MergePlan::default();
    let mut decided_root: Option<PathBuf> = None;
    let union: BTreeSet<&PathBuf> = ours_changed.iter().chain(theirs_changed.iter()).collect();
    for path in union {
        if decided_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
        {
            continue;
        }
        let ours_touched = subtree_changed(&ours_changed, path);
        let theirs_touched = subtree_changed(&theirs_changed, path);
        if !ours_touched {
            plan.take_theirs.push(path.clone());
            decided_root = Some(path.clone());
            continue;
        }
        if !theirs_touched {
            plan.take_ours.push(path.clone());
            decided_root = Some(path.clone());
            continue;
        }
        let b = base_map.get(path);
        let h = theirs_map.get(path);
        let f = ours_map.get(path);
        let kind = |record: Option<&FileRecord>| record.map(|record| record.kind);
        match (kind(b), kind(f), kind(h)) {
            // Nothing to decide here: both deleted it (or both changed it
            // inside a now-absent dir), or independent edits below one
            // directory resolve by descendants.
            (_, None, None)
            | (
                None | Some(FileKind::Directory),
                Some(FileKind::Directory),
                Some(FileKind::Directory),
            ) => {}
            // Regular files on both sides.
            (_, Some(FileKind::Regular), Some(FileKind::Regular)) => {
                if f.map(|record| record.payload) == h.map(|record| record.payload) {
                    continue;
                }
                let base_bytes = match kind(b) {
                    Some(FileKind::Regular) => Some(read_regular(&mut base_checkout, path).await?),
                    None => None,
                    Some(_) => {
                        plan.refusals.push(Refusal {
                            path: path.clone(),
                            reason: Reason::KindChange,
                        });
                        continue;
                    }
                };
                let ours_bytes = read_regular(&mut ours_checkout, path).await?;
                let theirs_bytes = read_regular(&mut theirs_checkout, path).await?;
                let mode = merged_mode(
                    read_mode(&mut base_checkout, path).await?,
                    read_mode(&mut ours_checkout, path).await?,
                    read_mode(&mut theirs_checkout, path).await?,
                );
                push_content(
                    &mut plan,
                    path,
                    mode,
                    merge_file(
                        base_bytes.as_deref(),
                        Some(&ours_bytes),
                        Some(&theirs_bytes),
                        ours_name,
                        "mainline",
                        limits,
                    ),
                );
            }
            // Modify/delete in either direction.
            (Some(FileKind::Regular), Some(FileKind::Regular), None)
            | (Some(FileKind::Regular), None, Some(FileKind::Regular)) => {
                let base_bytes = read_regular(&mut base_checkout, path).await?;
                let (ours_bytes, theirs_bytes, mode) = if f.is_some() {
                    (
                        Some(read_regular(&mut ours_checkout, path).await?),
                        None,
                        read_mode(&mut ours_checkout, path).await?,
                    )
                } else {
                    (
                        None,
                        Some(read_regular(&mut theirs_checkout, path).await?),
                        read_mode(&mut theirs_checkout, path).await?,
                    )
                };
                push_content(
                    &mut plan,
                    path,
                    mode,
                    merge_file(
                        Some(&base_bytes),
                        ours_bytes.as_deref(),
                        theirs_bytes.as_deref(),
                        ours_name,
                        "mainline",
                        limits,
                    ),
                );
            }
            // One side deleted a directory the other changed inside.
            (Some(FileKind::Directory), None, Some(FileKind::Directory)) => {
                let inner: Vec<PathBuf> = descendants(&theirs_changed, path).cloned().collect();
                plan.refusals.push(Refusal {
                    path: path.clone(),
                    reason: Reason::Ancestry { inner },
                });
                decided_root = Some(path.clone());
            }
            (Some(FileKind::Directory), Some(FileKind::Directory), None) => {
                let inner: Vec<PathBuf> = descendants(&ours_changed, path).cloned().collect();
                plan.refusals.push(Refusal {
                    path: path.clone(),
                    reason: Reason::Ancestry { inner },
                });
                decided_root = Some(path.clone());
            }
            // Symlinks retargeted identically are fine.
            (_, Some(FileKind::SymbolicLink), Some(FileKind::SymbolicLink))
                if f.map(|record| record.payload) == h.map(|record| record.payload) => {}
            // Everything else is a kind clash: a file on one side and a
            // directory or symlink on the other, symlinks retargeted
            // differently, or a file that became a directory on both sides.
            _ => {
                plan.refusals.push(Refusal {
                    path: path.clone(),
                    reason: Reason::KindChange,
                });
                decided_root = Some(path.clone());
            }
        }
    }
    Ok(plan)
}

fn push_content(
    plan: &mut MergePlan,
    path: &Path,
    mode: Option<u32>,
    outcome: std::result::Result<ContentMerge, Reason>,
) {
    match outcome {
        Ok(ContentMerge::Merged(bytes)) => plan.merged.push(MergedFile {
            path: path.to_path_buf(),
            bytes,
            mode,
        }),
        Ok(ContentMerge::Conflicted { bytes, hunks, kind }) => {
            plan.conflicted.push(ConflictedFile {
                path: path.to_path_buf(),
                bytes,
                hunks,
                kind,
                mode,
            });
        }
        Err(reason) => plan.refusals.push(Refusal {
            path: path.to_path_buf(),
            reason,
        }),
    }
}

/// The side that changed the mode wins; the fork when both did.
fn merged_mode(base: Option<u32>, ours: Option<u32>, theirs: Option<u32>) -> Option<u32> {
    if ours != base {
        ours
    } else if theirs != base {
        theirs
    } else {
        ours
    }
}

pub(crate) fn namespace_of(path: &Path) -> Result<NamespacePath> {
    let components = validate_relative(path)?;
    namespace_path(&components, acyclic_fs::model::VolumeLimits::default())
}

/// Whole content of a regular file at `path` in `checkout`.
pub(crate) async fn read_regular(checkout: &mut LocalCheckout, path: &Path) -> Result<Vec<u8>> {
    let cancel = CancellationToken::new();
    let namespace = namespace_of(path)?;
    let lookup = checkout
        .lookup_no_follow(&namespace, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(EngineError::fs("lookup"))?
        .value;
    let record = lookup
        .record
        .ok_or_else(|| EngineError::Fs(format!("{}: absent", path.display())))?;
    let length = match record.payload {
        acyclic_fs::kernel::FilePayload::InlineRegular(inline) => inline.as_bytes().len() as u64,
        acyclic_fs::kernel::FilePayload::Regular { logical_bytes, .. } => logical_bytes,
        _ => {
            return Err(EngineError::Fs(format!(
                "{}: not a regular file",
                path.display()
            )))
        }
    };
    let limits = checkout.volume_config().limits;
    let chunk = TRANSFER_BYTES.min(limits.maximum_read_bytes.max(1));
    let mut out = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    let mut offset = 0;
    while offset < length {
        let take = chunk.min(length - offset);
        let read = checkout
            .read_file_range(
                &namespace,
                ByteRange {
                    offset,
                    length: take,
                },
                WorkCounters::UNBOUNDED,
                &cancel,
            )
            .await
            .map_err(EngineError::fs("read file range"))?
            .value;
        out.extend_from_slice(&read.bytes);
        offset += take;
    }
    Ok(out)
}

/// Content of `path` in `generation` if it is a regular file there.
pub async fn read_file(
    store: &Store,
    generation: GenerationId,
    path: &Path,
) -> Result<Option<Vec<u8>>> {
    let mut checkout = store.checkout_exact(generation).await?;
    let cancel = CancellationToken::new();
    let namespace = namespace_of(path)?;
    let lookup = checkout
        .lookup_no_follow(&namespace, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(EngineError::fs("lookup"))?
        .value;
    match lookup.record {
        Some(record) if record.kind == FileKind::Regular => {
            Ok(Some(read_regular(&mut checkout, path).await?))
        }
        _ => Ok(None),
    }
}

async fn read_mode(checkout: &mut LocalCheckout, path: &Path) -> Result<Option<u32>> {
    let cancel = CancellationToken::new();
    let namespace = namespace_of(path)?;
    let lookup = checkout
        .lookup_no_follow(&namespace, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(EngineError::fs("lookup"))?
        .value;
    if lookup.record.is_none() {
        return Ok(None);
    }
    let metadata = checkout
        .read_metadata(&namespace, WorkCounters::UNBOUNDED, &cancel)
        .await
        .map_err(EngineError::fs("read metadata"))?
        .value;
    Ok(match metadata.posix_mode {
        MetadataField::Value(mode) => Some(mode & 0o7777),
        MetadataField::Unavailable => None,
    })
}

// ---------------------------------------------------------------------------
// Writing entries into a checkout (M, R, and fork rebases)
// ---------------------------------------------------------------------------

/// One path to write into a checkout.
#[derive(Clone, Debug)]
pub enum Entry {
    /// A regular file with this exact content.
    Regular { bytes: Vec<u8>, mode: Option<u32> },
    /// Whatever `generation` holds at the path (a subtree, or absence).
    FromGeneration { generation: GenerationId },
}

/// Applies `entries` to `dst` in order. Each path is replaced wholesale.
pub async fn apply_entries(
    store: &Store,
    dst: &mut LocalCheckout,
    entries: &[(PathBuf, Entry)],
) -> Result<()> {
    let cancel = CancellationToken::new();
    for (path, entry) in entries {
        let namespace = namespace_of(path)?;
        // Boxed per entry: keeps the facade's large futures off the caller's
        // stack frame.
        Box::pin(apply_entry(store, dst, &namespace, entry, &cancel)).await?;
    }
    Ok(())
}

async fn apply_entry(
    store: &Store,
    dst: &mut LocalCheckout,
    namespace: &NamespacePath,
    entry: &Entry,
    cancel: &CancellationToken,
) -> Result<()> {
    {
        {
            match entry {
                Entry::Regular { bytes, mode } => {
                    remove_subtree(dst, namespace, cancel).await?;
                    ensure_parents(dst, namespace, cancel).await?;
                    dst.create_file(
                        namespace.clone(),
                        Bytes::copy_from_slice(bytes),
                        WorkCounters::UNBOUNDED,
                        cancel,
                    )
                    .await
                    .map_err(EngineError::fs("create file"))?;
                    if let Some(mode) = mode {
                        let metadata = FileMetadata {
                            posix_mode: MetadataField::Value(*mode),
                            ..FileMetadata::default()
                        };
                        dst.set_metadata(
                            namespace.clone(),
                            metadata,
                            WorkCounters::UNBOUNDED,
                            cancel,
                        )
                        .await
                        .map_err(EngineError::fs("set metadata"))?;
                    }
                }
                Entry::FromGeneration { generation } => {
                    let mut src = store.checkout_exact(*generation).await?;
                    remove_subtree(dst, namespace, cancel).await?;
                    ensure_parents(dst, namespace, cancel).await?;
                    copy_node(&mut src, dst, namespace, cancel).await?;
                }
            }
        }
    }
    Ok(())
}

/// Creates missing ancestor directories of `namespace` in `dst`.
async fn ensure_parents(
    dst: &mut LocalCheckout,
    namespace: &NamespacePath,
    cancel: &CancellationToken,
) -> Result<()> {
    let limits = dst.volume_config().limits;
    let components = namespace.components();
    for depth in 1..components.len() {
        let parent = NamespacePath::new(components.iter().take(depth).cloned().collect(), limits)
            .map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))?;
        let lookup = dst
            .lookup_no_follow(&parent, WorkCounters::UNBOUNDED, cancel)
            .await
            .map_err(EngineError::fs("lookup"))?
            .value;
        if lookup.record.is_none() {
            dst.create_directory(parent, WorkCounters::UNBOUNDED, cancel)
                .await
                .map_err(EngineError::fs("create directory"))?;
        }
    }
    Ok(())
}

fn child_path(
    parent: &NamespacePath,
    name: &acyclic_fs::kernel::LogicalName,
    limits: acyclic_fs::model::VolumeLimits,
) -> Result<NamespacePath> {
    let mut components = parent.components().to_vec();
    components.push(name.clone());
    NamespacePath::new(components, limits)
        .map_err(|error| EngineError::Fs(format!("namespace path: {error:?}")))
}

async fn list_children(
    checkout: &mut LocalCheckout,
    namespace: &NamespacePath,
    cancel: &CancellationToken,
) -> Result<Vec<acyclic_fs::kernel::LogicalName>> {
    let mut names = Vec::new();
    let mut after = None;
    loop {
        let page = checkout
            .list_directory_records(
                namespace,
                after.as_ref(),
                PAGE_ENTRIES,
                WorkCounters::UNBOUNDED,
                cancel,
            )
            .await
            .map_err(EngineError::fs("list directory"))?
            .value;
        for entry in &page.entries {
            names.push(entry.name.clone());
        }
        match page.entries.last() {
            Some(last) if page.has_more => after = Some(last.name.clone()),
            _ => break,
        }
    }
    Ok(names)
}

/// Removes `namespace` from `dst`, recursively for directories; absent is
/// fine. Iterative (post-order over an explicit stack): the fs facade's
/// futures are large, and nesting them on the pipeline thread's stack
/// overflows it within a few levels.
async fn remove_subtree(
    dst: &mut LocalCheckout,
    namespace: &NamespacePath,
    cancel: &CancellationToken,
) -> Result<()> {
    let limits = dst.volume_config().limits;
    // (path, children_expanded)
    let mut stack: Vec<(NamespacePath, bool)> = vec![(namespace.clone(), false)];
    while let Some((path, expanded)) = stack.pop() {
        if expanded {
            dst.remove(path, None, WorkCounters::UNBOUNDED, cancel)
                .await
                .map_err(EngineError::fs("remove"))?;
            continue;
        }
        let lookup = dst
            .lookup_no_follow(&path, WorkCounters::UNBOUNDED, cancel)
            .await
            .map_err(EngineError::fs("lookup"))?
            .value;
        let Some(record) = lookup.record else {
            continue;
        };
        if record.kind == FileKind::Directory {
            let children = list_children(dst, &path, cancel).await?;
            stack.push((path.clone(), true));
            for name in children {
                stack.push((child_path(&path, &name, limits)?, false));
            }
        } else {
            stack.push((path, true));
        }
    }
    Ok(())
}

/// Copies the node at `namespace` (recursively) from `src` into `dst`,
/// where the path must be absent. Absent in `src` copies nothing.
/// Iterative pre-order for the same stack reason as [`remove_subtree`].
async fn copy_node(
    src: &mut LocalCheckout,
    dst: &mut LocalCheckout,
    namespace: &NamespacePath,
    cancel: &CancellationToken,
) -> Result<()> {
    let limits = src.volume_config().limits;
    let mut queue: Vec<NamespacePath> = vec![namespace.clone()];
    while let Some(path) = queue.pop() {
        let lookup = src
            .lookup_no_follow(&path, WorkCounters::UNBOUNDED, cancel)
            .await
            .map_err(EngineError::fs("lookup"))?
            .value;
        let Some(record) = lookup.record else {
            continue;
        };
        let metadata = src
            .read_metadata(&path, WorkCounters::UNBOUNDED, cancel)
            .await
            .map_err(EngineError::fs("read metadata"))?
            .value;
        match record.kind {
            FileKind::Regular => {
                let bytes = read_regular_ns(src, &path, &record.payload, cancel).await?;
                dst.create_file(
                    path.clone(),
                    Bytes::from(bytes),
                    WorkCounters::UNBOUNDED,
                    cancel,
                )
                .await
                .map_err(EngineError::fs("create file"))?;
            }
            FileKind::SymbolicLink => {
                let target = src
                    .read_symbolic_link(&path, WorkCounters::UNBOUNDED, cancel)
                    .await
                    .map_err(EngineError::fs("read symlink"))?
                    .value;
                dst.create_symbolic_link(
                    path.clone(),
                    Bytes::copy_from_slice(&target),
                    WorkCounters::UNBOUNDED,
                    cancel,
                )
                .await
                .map_err(EngineError::fs("create symlink"))?;
            }
            FileKind::Directory => {
                dst.create_directory(path.clone(), WorkCounters::UNBOUNDED, cancel)
                    .await
                    .map_err(EngineError::fs("create directory"))?;
                for name in list_children(src, &path, cancel).await? {
                    queue.push(child_path(&path, &name, limits)?);
                }
            }
            other => {
                return Err(EngineError::Fs(format!(
                    "cannot copy a {other:?} node (only files, symlinks, and directories)"
                )))
            }
        }
        if let MetadataField::Value(mode) = metadata.posix_mode {
            let set = FileMetadata {
                posix_mode: MetadataField::Value(mode),
                ..FileMetadata::default()
            };
            dst.set_metadata(path, set, WorkCounters::UNBOUNDED, cancel)
                .await
                .map_err(EngineError::fs("set metadata"))?;
        }
    }
    Ok(())
}

async fn read_regular_ns(
    checkout: &mut LocalCheckout,
    namespace: &NamespacePath,
    payload: &acyclic_fs::kernel::FilePayload,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let length = match payload {
        acyclic_fs::kernel::FilePayload::InlineRegular(inline) => inline.as_bytes().len() as u64,
        acyclic_fs::kernel::FilePayload::Regular { logical_bytes, .. } => *logical_bytes,
        _ => return Err(EngineError::Fs("regular file with foreign payload".into())),
    };
    let limits = checkout.volume_config().limits;
    let chunk = TRANSFER_BYTES.min(limits.maximum_read_bytes.max(1));
    let mut out = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    let mut offset = 0;
    while offset < length {
        let take = chunk.min(length - offset);
        let read = checkout
            .read_file_range(
                namespace,
                ByteRange {
                    offset,
                    length: take,
                },
                WorkCounters::UNBOUNDED,
                cancel,
            )
            .await
            .map_err(EngineError::fs("read file range"))?
            .value;
        out.extend_from_slice(&read.bytes);
        offset += take;
    }
    Ok(out)
}

/// Which of `paths` the repo's `.gitignore` rules ignore, per
/// `git check-ignore --no-index`. Empty when git is absent, the root is not
/// a repository, or nothing matches: ignore rules are advisory here, never
/// a reason to fail a promote.
pub fn ignored_paths(repo_root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if paths.is_empty() {
        return Vec::new();
    }
    let mut input = Vec::new();
    for path in paths {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            input.extend_from_slice(path.as_os_str().as_bytes());
        }
        #[cfg(not(unix))]
        input.extend_from_slice(path.to_string_lossy().as_bytes());
        input.push(0);
    }
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&input);
    }
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    // Exit 1 means "none ignored"; 128 means not a repo. Either way the
    // stdout list is authoritative for what it does contain.
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                PathBuf::from(std::ffi::OsStr::from_bytes(chunk))
            }
            #[cfg(not(unix))]
            PathBuf::from(String::from_utf8_lossy(chunk).into_owned())
        })
        .filter(|path| paths.contains(path))
        .collect()
}

/// Writes `paths` from `generation` into `dir` with plain filesystem
/// operations (no staging or atomic swap): a path absent in the generation
/// is removed, a present one replaced wholesale. For a fork workspace that
/// is a live mount this is the only coherent way to change it: the mount
/// driver and the kernel see writes they performed themselves, whereas a
/// write through the checkout behind the mount's back is invisible to
/// their name caches (and the FUSE transport cannot be told to drop them).
pub async fn materialize_paths(
    store: &Store,
    generation: GenerationId,
    dir: &Path,
    paths: &[PathBuf],
) -> Result<()> {
    let mut checkout = store.checkout_exact(generation).await?;
    for path in paths {
        crate::rewind::materialize_path_into_checkout(
            &mut checkout,
            dir,
            path,
            crate::rewind::PathReplace::LiveMount,
        )
        .await?;
    }
    Ok(())
}

/// Collapses a sorted path list to its subtree roots.
pub fn subtree_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = paths.to_vec();
    sorted.sort();
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in sorted {
        if !roots.iter().any(|root| path.starts_with(root)) {
            roots.push(path);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: MergeLimits = MergeLimits {
        max_file_bytes: DEFAULT_MAX_FILE_BYTES,
    };

    // --- graphcoder's own merge3 fixtures --------------------------------

    #[test]
    fn clean_merge_no_overlap() {
        let base = "line 1\nline 2\nline 3\n";
        let ours = "OURS line 1\nline 2\nline 3\n";
        let theirs = "line 1\nline 2\nTHEIRS line 3\n";
        let result = acyclic_fs::text_merge::merge_text(base, ours, theirs, "child", "parent");
        assert!(result.clean);
        assert_eq!(result.content, "OURS line 1\nline 2\nTHEIRS line 3\n");
    }

    #[test]
    fn conflict_same_line() {
        let base = "line 1\nline 2\nline 3\n";
        let ours = "OURS line 1\nline 2\nline 3\n";
        let theirs = "THEIRS line 1\nline 2\nline 3\n";
        let result =
            acyclic_fs::text_merge::merge_text(base, ours, theirs, "child-agent", "parent-agent");
        assert!(!result.clean);
        assert!(result.content.contains("<<<<<<< child-agent\n"));
        assert!(result.content.contains("||||||| original\n"));
        assert!(result.content.contains(">>>>>>> parent-agent\n"));
        assert_eq!(conflict_hunks(&result.content), 1);
    }

    #[test]
    fn identical_changes_no_conflict() {
        let base = "line 1\nline 2\n";
        let ours = "SAME line 1\nline 2\n";
        let theirs = "SAME line 1\nline 2\n";
        let result = acyclic_fs::text_merge::merge_text(base, ours, theirs, "child", "parent");
        assert!(result.clean);
        assert_eq!(result.content, "SAME line 1\nline 2\n");
    }

    #[test]
    fn conflict_markers_have_proper_newlines() {
        let result =
            acyclic_fs::text_merge::merge_text("content", "ours", "theirs", "child", "parent");
        assert!(!result.clean);
        assert!(result.content.contains("\n|||||||"));
        assert!(result.content.contains("\n=======\n"));
        assert!(result.content.contains("\n>>>>>>> parent"));
    }

    #[test]
    fn empty_base_ours_added_theirs_empty() {
        let result = acyclic_fs::text_merge::merge_text("", "added", "", "child", "parent");
        assert!(result.clean);
        assert_eq!(result.content, "added");
    }

    #[test]
    fn empty_base_both_add_different() {
        let result =
            acyclic_fs::text_merge::merge_text("", "ours\n", "theirs\n", "child", "parent");
        assert!(!result.clean);
        assert!(result.content.contains("<<<<<<< child"));
        assert!(result.content.contains(">>>>>>> parent"));
    }

    #[test]
    fn fork_appends_without_trailing_newline_and_head_edits_top() {
        let base = "one\ntwo\nthree\n";
        let ours = "one\ntwo\nthree\nfour";
        let theirs = "ONE\ntwo\nthree\n";
        let result = acyclic_fs::text_merge::merge_text(base, ours, theirs, "fork", "mainline");
        assert!(result.clean);
        assert_eq!(result.content, "ONE\ntwo\nthree\nfour");
    }

    #[test]
    fn crlf_is_preserved_through_a_clean_merge() {
        let base = "a\r\nb\r\nc\r\n";
        let ours = "A\r\nb\r\nc\r\n";
        let theirs = "a\r\nb\r\nC\r\n";
        let result = acyclic_fs::text_merge::merge_text(base, ours, theirs, "fork", "mainline");
        assert!(result.clean);
        assert_eq!(result.content, "A\r\nb\r\nC\r\n");
    }

    #[test]
    fn adjacent_but_non_overlapping_hunks_merge() {
        let base = "1\n2\n3\n4\n";
        let ours = "1x\n2\n3\n4\n";
        let theirs = "1\n2y\n3\n4\n";
        let result = acyclic_fs::text_merge::merge_text(base, ours, theirs, "fork", "mainline");
        // Adjacent edits are a conflict for diff3 (git behaves the same);
        // whichever way diffy decides, the result must be consistent with clean.
        if result.clean {
            assert_eq!(result.content, "1x\n2y\n3\n4\n");
        } else {
            assert!(has_conflict_markers(&result.content));
        }
    }

    #[test]
    fn fork_deletes_a_block_the_head_edited_conflicts() {
        let base = "a\nb\nc\nd\n";
        let ours = "a\nd\n";
        let theirs = "a\nB\nc\nd\n";
        let result = acyclic_fs::text_merge::merge_text(base, ours, theirs, "fork", "mainline");
        assert!(!result.clean);
        assert_eq!(conflict_hunks(&result.content), 1);
    }

    // --- marker detection (markers.ts) -------------------------------------

    #[test]
    fn marker_detection_matches_markers_ts() {
        assert!(has_conflict_markers(
            "<<<<<<< fork a\nx\n=======\ny\n>>>>>>> mainline\n"
        ));
        assert!(has_conflict_markers(
            "<<<<<<< fork a (deleted)\n||||||| original\nb\n=======\ny\n\
             >>>>>>> mainline (modified)\n"
        ));
        assert!(has_conflict_markers(
            "pre\n<<<<<<< x\r\na\r\n=======\r\nb\r\n>>>>>>> y\r\n"
        ));
        // No opening marker at a line start.
        assert!(!has_conflict_markers(
            "text <<<<<<< not a marker\n=======\n"
        ));
        // Opening marker but no separator line.
        assert!(!has_conflict_markers("<<<<<<< x\nalone\n"));
        // Separator alone.
        assert!(!has_conflict_markers("=======\n"));
        // Resolved content.
        assert!(!has_conflict_markers("clean\nfile\n"));
    }

    // --- merge_file: decision table --------------------------------------

    #[test]
    fn merge_file_clean_and_conflict() {
        let merged = merge_file(
            Some(b"1\n2\n3\n"),
            Some(b"1x\n2\n3\n"),
            Some(b"1\n2\n3y\n"),
            "fork a",
            "mainline",
            &LIMITS,
        )
        .expect("gate");
        assert_eq!(merged, ContentMerge::Merged(b"1x\n2\n3y\n".to_vec()));
        let conflicted = merge_file(
            Some(b"1\n"),
            Some(b"a\n"),
            Some(b"b\n"),
            "fork a",
            "mainline",
            &LIMITS,
        )
        .expect("gate");
        let ContentMerge::Conflicted { bytes, hunks, kind } = conflicted else {
            panic!("expected conflict")
        };
        assert_eq!(hunks, 1);
        assert_eq!(kind, ConflictKind::Hunks);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "<<<<<<< fork a\na\n||||||| original\n1\n=======\nb\n>>>>>>> mainline\n"
        );
    }

    #[test]
    fn identical_changes_take_either_without_diffing() {
        let merged = merge_file(
            Some(b"1\n"),
            Some(b"same\n"),
            Some(b"same\n"),
            "f",
            "m",
            &LIMITS,
        )
        .unwrap();
        assert_eq!(merged, ContentMerge::Merged(b"same\n".to_vec()));
    }

    #[test]
    fn modify_delete_is_a_labelled_conflict() {
        let ours_modified = merge_file(
            Some(b"base\n"),
            Some(b"ours\n"),
            None,
            "fork a",
            "mainline",
            &LIMITS,
        )
        .unwrap();
        let ContentMerge::Conflicted { bytes, hunks, kind } = ours_modified else {
            panic!("expected conflict")
        };
        assert_eq!((hunks, kind), (1, ConflictKind::TheirsDeleted));
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "<<<<<<< fork a (modified)\nours\n||||||| original\nbase\n=======\n>>>>>>> mainline (deleted)\n"
        );
        let ours_deleted = merge_file(
            Some(b"base"),
            None,
            Some(b"theirs"),
            "fork a",
            "mainline",
            &LIMITS,
        )
        .unwrap();
        let ContentMerge::Conflicted { bytes, kind, .. } = ours_deleted else {
            panic!("expected conflict")
        };
        assert_eq!(kind, ConflictKind::OursDeleted);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with(
            "<<<<<<< fork a (deleted)\n||||||| original\nbase\n=======\ntheirs\n\
             >>>>>>> mainline (modified)\n"
        ));
        assert!(has_conflict_markers(&text));
    }

    #[test]
    fn add_add_same_takes_and_add_add_different_conflicts() {
        let same = merge_file(None, Some(b"x\n"), Some(b"x\n"), "f", "m", &LIMITS).unwrap();
        assert_eq!(same, ContentMerge::Merged(b"x\n".to_vec()));
        let differ = merge_file(None, Some(b"x\n"), Some(b"y\n"), "f", "m", &LIMITS).unwrap();
        assert!(matches!(differ, ContentMerge::Conflicted { .. }));
    }

    #[test]
    fn text_gate_refuses_binary_and_size() {
        assert_eq!(
            merge_file(Some(b"a"), Some(b"a\0b"), Some(b"c"), "f", "m", &LIMITS).unwrap_err(),
            Reason::Binary
        );
        assert_eq!(
            merge_file(Some(b"a"), Some(b"\xff\xfe"), Some(b"c"), "f", "m", &LIMITS).unwrap_err(),
            Reason::Binary
        );
        let small = MergeLimits { max_file_bytes: 3 };
        assert_eq!(
            merge_file(Some(b"a"), Some(b"abcd"), Some(b"c"), "f", "m", &small).unwrap_err(),
            Reason::TooLarge
        );
        // The gate applies to the base too.
        assert_eq!(
            merge_file(Some(b"\0"), Some(b"a"), Some(b"b"), "f", "m", &LIMITS).unwrap_err(),
            Reason::Binary
        );
    }

    #[test]
    fn mode_comes_from_the_side_that_changed_it() {
        assert_eq!(
            merged_mode(Some(0o644), Some(0o644), Some(0o755)),
            Some(0o755)
        );
        assert_eq!(
            merged_mode(Some(0o644), Some(0o755), Some(0o644)),
            Some(0o755)
        );
        assert_eq!(
            merged_mode(Some(0o644), Some(0o700), Some(0o755)),
            Some(0o700)
        );
        assert_eq!(
            merged_mode(Some(0o644), Some(0o644), Some(0o644)),
            Some(0o644)
        );
    }

    #[test]
    fn keep_mainline_for_drops_only_ignored_entries() {
        let mut plan = MergePlan::default();
        plan.refusals.push(Refusal {
            path: PathBuf::from("a.pyc"),
            reason: Reason::Binary,
        });
        plan.refusals.push(Refusal {
            path: PathBuf::from("b.bin"),
            reason: Reason::Binary,
        });
        plan.conflicted.push(ConflictedFile {
            path: PathBuf::from(".env"),
            bytes: Vec::new(),
            hunks: 1,
            kind: ConflictKind::Hunks,
            mode: None,
        });
        plan.conflicted.push(ConflictedFile {
            path: PathBuf::from("src/x.rs"),
            bytes: Vec::new(),
            hunks: 1,
            kind: ConflictKind::Hunks,
            mode: None,
        });
        let kept = plan.keep_mainline_for(&[PathBuf::from(".env"), PathBuf::from("a.pyc")]);
        assert_eq!(kept, vec![PathBuf::from(".env"), PathBuf::from("a.pyc")]);
        assert_eq!(plan.refusals.len(), 1);
        assert_eq!(plan.refusals[0].path, PathBuf::from("b.bin"));
        assert_eq!(plan.conflicted.len(), 1);
        assert_eq!(plan.conflicted[0].path, PathBuf::from("src/x.rs"));
    }

    #[test]
    fn ignored_paths_follow_the_repos_gitignore() {
        let repo = tempfile::tempdir().expect("tempdir");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status();
        if !init.is_ok_and(|s| s.success()) {
            eprintln!("git unavailable; skipping");
            return;
        }
        std::fs::write(
            repo.path().join(".gitignore"),
            "__pycache__/\n*.log\n.env\n",
        )
        .unwrap();
        let asked = vec![
            PathBuf::from("src/__pycache__/m.cpython-313.pyc"),
            PathBuf::from("src/m.py"),
            PathBuf::from("run.log"),
            PathBuf::from(".env"),
        ];
        let mut ignored = ignored_paths(repo.path(), &asked);
        ignored.sort();
        assert_eq!(
            ignored,
            vec![
                PathBuf::from(".env"),
                PathBuf::from("run.log"),
                PathBuf::from("src/__pycache__/m.cpython-313.pyc"),
            ]
        );
        // Not a repo: nothing is ignored, nothing fails.
        let plain = tempfile::tempdir().expect("tempdir");
        assert!(ignored_paths(plain.path(), &asked).is_empty());
        assert!(ignored_paths(repo.path(), &[]).is_empty());
    }

    #[test]
    fn subtree_helpers() {
        let set: BTreeSet<PathBuf> = ["a/b/c", "a/bc", "d"].iter().map(PathBuf::from).collect();
        assert!(subtree_changed(&set, Path::new("a/b")));
        assert!(subtree_changed(&set, Path::new("a")));
        assert!(!subtree_changed(&set, Path::new("a/c")));
        assert!(!subtree_changed(&set, Path::new("b")));
        let inner: Vec<&PathBuf> = descendants(&set, Path::new("a")).collect();
        assert_eq!(inner, vec![&PathBuf::from("a/b/c"), &PathBuf::from("a/bc")]);
        assert_eq!(
            subtree_roots(&[
                PathBuf::from("x/y/z"),
                PathBuf::from("x/y"),
                PathBuf::from("w")
            ]),
            vec![PathBuf::from("w"), PathBuf::from("x/y")]
        );
    }
}
