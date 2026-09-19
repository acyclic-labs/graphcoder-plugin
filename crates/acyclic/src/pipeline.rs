//! The capture pipeline: one thread owns the checkout, watcher, and index.
//!
//! Requests are processed FIFO. Per-tool-call snapshots use `checkpoint()`
//! (fast, no authority publish); `commit()` runs at coarse boundaries only.
//! The pipeline runs on its own thread with a current-thread tokio runtime so
//! the `&mut Checkout` discipline never meets Send bounds.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use acyclic_fs::model::VolumeLimits;
use acyclic_fs::{
    capture_baseline, capture_root_identity, capture_subtree, capture_watch_batch, CaptureOptions,
};
use acyclic_fs::{
    CancellationToken, CheckoutCommitOutcome, GenerationId, MountPublication, NativeWatch,
    NativeWatchOptions, OperationId, WatchBatch, WatchChange, WatchEpoch, WatchSequence,
    WorkCounters, WorkspaceRestore,
};
use tokio::sync::{mpsc, oneshot};

use std::sync::Arc;

use crate::config::Config;
use crate::diff::{self, FileChange};
use crate::exclude::Exclusions;
use crate::fork::{ForkSeed, PromoteOutcome, SharedLocalCheckout};
use crate::index::{Attribution, CheckpointKind, CheckpointRow, Index};
use crate::rewind::{self, RestoreOutcome, RewindOutcome};
use crate::store::Store;
use crate::{EngineError, Result};

const MAXIMUM_CAPTURE_PATHS: u32 = 4_000_000;
const MAXIMUM_EXTENT_SPANS: u32 = 65_536;
const WATCH_QUEUE: u32 = 65_536;
const POLL_CHANGES: u32 = 16_384;

#[cfg(target_os = "windows")]
const CONTINUITY_MAGIC: &[u8; 8] = b"ACCONT\0\x01";
#[cfg(target_os = "windows")]
const CONTINUITY_BYTES: usize = 88;

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct ContinuityRecord {
    generation: GenerationId,
    row: i64,
    usn: acyclic_fs::WindowsUsnCheckpoint,
}

#[cfg(target_os = "windows")]
impl ContinuityRecord {
    fn encode(self) -> [u8; CONTINUITY_BYTES] {
        let mut bytes = [0_u8; CONTINUITY_BYTES];
        bytes[..8].copy_from_slice(CONTINUITY_MAGIC);
        bytes[8..40].copy_from_slice(self.generation.digest().as_bytes());
        bytes[40..48].copy_from_slice(&self.row.to_le_bytes());
        bytes[48..].copy_from_slice(&self.usn.to_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != CONTINUITY_BYTES || !bytes.starts_with(CONTINUITY_MAGIC) {
            return None;
        }
        let mut generation = [0_u8; 32];
        generation.copy_from_slice(bytes.get(8..40)?);
        let mut row = [0_u8; 8];
        row.copy_from_slice(bytes.get(40..48)?);
        let row = i64::from_le_bytes(row);
        if row <= 0 {
            return None;
        }
        Some(Self {
            generation: GenerationId::new(acyclic_fs::Digest::from_bytes(generation)),
            row,
            usn: acyclic_fs::WindowsUsnCheckpoint::from_bytes(bytes.get(48..)?).ok()?,
        })
    }
}

fn fork_moved(base: GenerationId) -> PromoteOutcome {
    PromoteOutcome::Conflict {
        message: format!(
            "the working tree moved past the fork's base ({}); promote in v1 requires an unmoved mainline — rewind to the base or re-fork and re-apply",
            crate::generation_hex(base)
        ),
    }
}

/// Pipeline state reported by `status`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// Metadata and watcher are open; no repository descendants were scanned.
    NeedsBaseline,
    Baselining,
    Ready,
    Rewinding,
}

/// Result of a checkpoint request.
#[derive(Clone, Debug)]
pub struct CheckpointOutcome {
    pub row_id: i64,
    pub generation: GenerationId,
    pub kind: CheckpointKind,
}

/// What one batched restore landed, and the row that records it.
#[derive(Clone, Debug)]
pub struct RestoredPaths {
    pub outcomes: Vec<RestoreOutcome>,
    pub generation: GenerationId,
    pub row: i64,
}

/// Snapshot of pipeline health.
#[derive(Clone, Debug)]
pub struct StatusReport {
    pub state: State,
    pub last_checkpoint: Option<i64>,
    pub unpublished: u64,
    pub checkpoints_since_commit: u32,
    pub watcher: WatcherHealth,
}

/// How often the native watcher has lost its epoch since the daemon
/// started, and what each recovery cost. An invalidation forces a full-tree
/// recovery baseline, so this is the number to read when checkpoints or
/// promotes are slow.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatcherHealth {
    /// Times the watcher reported that its hints could no longer be trusted.
    pub invalidations: u32,
    /// The reason the last invalidation gave.
    pub last_reason: Option<String>,
    /// Full-tree recovery baselines run because of an invalidation.
    pub recovery_rescans: u32,
    /// Wall time spent in those recovery baselines, in milliseconds.
    pub recovery_ms_total: f64,
    /// Wall time of the most recent recovery baseline, in milliseconds.
    pub last_recovery_ms: f64,
}

/// A path paired with its regular-file contents, or `None` when the path is
/// absent or not a regular file.
type FileContents = Vec<(PathBuf, Option<Vec<u8>>)>;

enum Request {
    Checkpoint {
        kind: CheckpointKind,
        attribution: Attribution,
        reply: oneshot::Sender<Result<CheckpointOutcome>>,
    },
    Commit {
        reply: oneshot::Sender<Result<()>>,
    },
    Rewind {
        target: CheckpointRow,
        reply: oneshot::Sender<Result<RewindOutcome>>,
    },
    RestorePath {
        target: CheckpointRow,
        path: PathBuf,
        reply: oneshot::Sender<Result<RestoreOutcome>>,
    },
    RestorePaths {
        target: CheckpointRow,
        paths: Vec<PathBuf>,
        safety: bool,
        label: Option<String>,
        reply: oneshot::Sender<Result<RestoredPaths>>,
    },
    TurnStarted {
        session_id: String,
        prompt: String,
        reply: oneshot::Sender<Result<i64>>,
    },
    Diff {
        before: GenerationId,
        after: GenerationId,
        reply: oneshot::Sender<Result<Vec<FileChange>>>,
    },
    Fork {
        reply: oneshot::Sender<Result<ForkSeed>>,
    },
    /// A private writable overlay pinned at `base`, with no index record
    /// and no publish: the scratch space `fork-diff` captures a fork's
    /// current tree into.
    ScratchCheckout {
        base: GenerationId,
        reply: oneshot::Sender<Result<Arc<SharedLocalCheckout>>>,
    },
    /// Folds pending real-tree changes in, publishes, and returns the
    /// published head: the reference point a promote is judged against.
    PublishHead {
        reply: oneshot::Sender<Result<GenerationId>>,
    },
    /// Records an existing (e.g. snapshot) generation as a checkpoint row so
    /// it can be a `restore_path` target.
    RecordGeneration {
        generation: GenerationId,
        kind: CheckpointKind,
        label: String,
        reply: oneshot::Sender<Result<CheckpointRow>>,
    },
    /// Snapshots an overlay's pending mutations as an unpublished
    /// generation (diffable by id; the head does not move).
    SnapshotOverlay {
        shared: Arc<SharedLocalCheckout>,
        reply: oneshot::Sender<Result<GenerationId>>,
    },
    /// Merge v2: the three-way plan of fork `ours` onto mainline `theirs`.
    MergePlan {
        base: GenerationId,
        theirs: GenerationId,
        ours: GenerationId,
        ours_name: String,
        reply: oneshot::Sender<Result<crate::merge::MergePlan>>,
    },
    /// Merge v2: a new unpublished generation equal to `from` with
    /// `entries` written over it (the merge generation M, the rebase R).
    BuildGeneration {
        from: GenerationId,
        entries: Vec<(PathBuf, crate::merge::Entry)>,
        reply: oneshot::Sender<Result<GenerationId>>,
    },
    /// Merge v2: writes `entries` into a live fork overlay (a rebase of a
    /// mounted fork).
    ApplyToOverlay {
        shared: Arc<SharedLocalCheckout>,
        entries: Vec<(PathBuf, crate::merge::Entry)>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Regular-file contents at `paths` in `generation` (None when absent
    /// or not a regular file): the conflict-marker scan before a re-promote.
    ReadFiles {
        generation: GenerationId,
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<FileContents>>,
    },
    /// Plain writes of `paths` from `generation` into `root` (a mounted
    /// fork's directory, written through the mount so its caches stay
    /// coherent).
    MaterializePaths {
        generation: GenerationId,
        root: PathBuf,
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<()>>,
    },
    Promote {
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: String,
        reply: oneshot::Sender<Result<PromoteOutcome>>,
    },
    Status {
        reply: oneshot::Sender<Result<StatusReport>>,
    },
    SessionStarted {
        session_id: String,
        host: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SessionEnded {
        session_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

impl Request {
    const fn requires_ready(&self) -> bool {
        !matches!(
            self,
            Self::Status { .. }
                | Self::TurnStarted { .. }
                | Self::RecordGeneration { .. }
                | Self::Shutdown { .. }
        )
    }
}

/// Cloneable handle used by the daemon to talk to the pipeline.
#[derive(Clone)]
pub struct PipelineHandle {
    sender: mpsc::Sender<Request>,
}

macro_rules! request {
    ($self:ident, $variant:ident { $($field:ident : $value:expr),* $(,)? }) => {{
        let (reply, receiver) = oneshot::channel();
        $self
            .sender
            .send(Request::$variant { $($field: $value,)* reply })
            .await
            .map_err(|_| EngineError::Store("pipeline is gone".into()))?;
        receiver
            .await
            .map_err(|_| EngineError::Store("pipeline dropped the request".into()))
    }};
}

impl PipelineHandle {
    pub async fn checkpoint(
        &self,
        kind: CheckpointKind,
        attribution: Attribution,
    ) -> Result<CheckpointOutcome> {
        request!(
            self,
            Checkpoint {
                kind: kind,
                attribution: attribution
            }
        )?
    }

    /// Enqueues a checkpoint and returns as soon as the pipeline has
    /// admitted it (FIFO), without waiting for the capture. Admission before
    /// acknowledgement is the guarantee the hook path relies on: a stop that
    /// arrives after the ack is queued behind the capture, never before it.
    pub async fn checkpoint_enqueued(
        &self,
        kind: CheckpointKind,
        attribution: Attribution,
    ) -> Result<()> {
        let (reply, receiver) = oneshot::channel();
        drop(receiver); // outcome is recorded in the index, not awaited
        self.sender
            .send(Request::Checkpoint {
                kind,
                attribution,
                reply,
            })
            .await
            .map_err(|_| EngineError::Store("pipeline is gone".into()))
    }

    pub async fn commit(&self) -> Result<()> {
        request!(self, Commit {})?
    }

    pub async fn rewind(&self, target: CheckpointRow) -> Result<RewindOutcome> {
        request!(self, Rewind { target: target })?
    }

    /// Restores one path from `target` without touching the rest of the tree.
    pub async fn restore_path(
        &self,
        target: CheckpointRow,
        path: PathBuf,
    ) -> Result<RestoreOutcome> {
        request!(
            self,
            RestorePath {
                target: target,
                path: path
            }
        )?
    }

    /// Restores several paths from `target` as ONE timeline event: one
    /// safety row if the tree had pending changes, every path written, one
    /// watcher drain, one recorded row. Promote lands a fork this way; the
    /// per-path form costs a drain and a row per path.
    ///
    /// `safety` asks for a safety row first when the tree holds changes the
    /// timeline has not seen; a caller that captured the tree moments ago
    /// (promote, right after publishing the head) passes `false` and skips
    /// that drain. `label` names the recorded row; `None` gets
    /// `restore <what>`.
    pub async fn restore_paths(
        &self,
        target: CheckpointRow,
        paths: Vec<PathBuf>,
        safety: bool,
        label: Option<String>,
    ) -> Result<RestoredPaths> {
        request!(
            self,
            RestorePaths {
                target: target,
                paths: paths,
                safety: safety,
                label: label
            }
        )?
    }

    /// Records a conversation turn; returns its 1-based number. Later
    /// checkpoints in the session inherit it.
    pub async fn turn_started(&self, session_id: String, prompt: String) -> Result<i64> {
        request!(
            self,
            TurnStarted {
                session_id: session_id,
                prompt: prompt
            }
        )?
    }

    pub async fn diff(&self, before: GenerationId, after: GenerationId) -> Result<Vec<FileChange>> {
        request!(
            self,
            Diff {
                before: before,
                after: after
            }
        )?
    }

    /// A diff that yields rather than queues.
    ///
    /// `Ok(None)` means the request channel was already loaded, so this
    /// speculation is abandoned instead of landing behind a backlog. That is
    /// the whole discipline speculation runs under: it may use idle capacity
    /// and must never compete for busy capacity, because the queue it would
    /// join is the one the hook path waits in. Deliberately not a new
    /// `Request` variant — it reuses `Diff`, so the pipeline gains no state
    /// and `fail_request` needs no new arm.
    pub async fn diff_speculative(
        &self,
        before: GenerationId,
        after: GenerationId,
    ) -> Result<Option<Vec<FileChange>>> {
        let (reply, receiver) = oneshot::channel();
        match self.sender.try_send(Request::Diff {
            before,
            after,
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Ok(None),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(EngineError::Store("pipeline is gone".into()))
            }
        }
        let changes = receiver
            .await
            .map_err(|_| EngineError::Store("pipeline dropped the request".into()))??;
        Ok(Some(changes))
    }

    pub async fn fork(&self) -> Result<ForkSeed> {
        request!(self, Fork {})?
    }

    /// Publishes the current real tree and returns the head generation.
    pub async fn publish_head(&self) -> Result<GenerationId> {
        request!(self, PublishHead {})?
    }

    /// Records `generation` as a manual checkpoint row (a `restore_path` target).
    pub async fn record_generation(
        &self,
        generation: GenerationId,
        label: String,
    ) -> Result<CheckpointRow> {
        self.record_generation_as(generation, CheckpointKind::Manual, label)
            .await
    }

    /// [`Self::record_generation`] with an explicit row kind. Records a
    /// generation the caller already holds: no drain, no capture, so it is
    /// the way to mark a "before" point when the tree was captured moments
    /// ago and nothing has been written since.
    pub async fn record_generation_as(
        &self,
        generation: GenerationId,
        kind: CheckpointKind,
        label: String,
    ) -> Result<CheckpointRow> {
        request!(
            self,
            RecordGeneration {
                generation: generation,
                kind: kind,
                label: label
            }
        )?
    }

    /// Private writable overlay pinned at `base`; nothing recorded or published.
    pub async fn scratch_checkout(&self, base: GenerationId) -> Result<Arc<SharedLocalCheckout>> {
        request!(self, ScratchCheckout { base: base })?
    }

    /// Unpublished generation holding `shared`'s pending mutations.
    pub async fn snapshot_overlay(&self, shared: Arc<SharedLocalCheckout>) -> Result<GenerationId> {
        request!(self, SnapshotOverlay { shared: shared })?
    }

    /// Merge v2: plans fork `ours` onto mainline `theirs` from `base`.
    pub async fn merge_plan(
        &self,
        base: GenerationId,
        theirs: GenerationId,
        ours: GenerationId,
        ours_name: String,
    ) -> Result<crate::merge::MergePlan> {
        request!(
            self,
            MergePlan {
                base: base,
                theirs: theirs,
                ours: ours,
                ours_name: ours_name
            }
        )?
    }

    /// Merge v2: unpublished generation = `from` + `entries`.
    pub async fn build_generation(
        &self,
        from: GenerationId,
        entries: Vec<(PathBuf, crate::merge::Entry)>,
    ) -> Result<GenerationId> {
        request!(
            self,
            BuildGeneration {
                from: from,
                entries: entries
            }
        )?
    }

    /// Merge v2: writes `entries` into a fork's overlay.
    pub async fn apply_to_overlay(
        &self,
        shared: Arc<SharedLocalCheckout>,
        entries: Vec<(PathBuf, crate::merge::Entry)>,
    ) -> Result<()> {
        request!(
            self,
            ApplyToOverlay {
                shared: shared,
                entries: entries
            }
        )?
    }

    /// Regular-file contents at `paths` in `generation`.
    pub async fn read_files(
        &self,
        generation: GenerationId,
        paths: Vec<PathBuf>,
    ) -> Result<FileContents> {
        request!(
            self,
            ReadFiles {
                generation: generation,
                paths: paths
            }
        )?
    }

    /// Plain writes of `paths` from `generation` into `root`.
    pub async fn materialize_paths(
        &self,
        generation: GenerationId,
        root: PathBuf,
        paths: Vec<PathBuf>,
    ) -> Result<()> {
        request!(
            self,
            MaterializePaths {
                generation: generation,
                root: root,
                paths: paths
            }
        )?
    }

    pub async fn promote(
        &self,
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: String,
    ) -> Result<PromoteOutcome> {
        request!(
            self,
            Promote {
                shared: shared,
                base: base,
                label: label
            }
        )?
    }

    pub async fn status(&self) -> Result<StatusReport> {
        request!(self, Status {})?
    }

    pub async fn session_started(&self, session_id: String, host: String) -> Result<()> {
        request!(
            self,
            SessionStarted {
                session_id: session_id,
                host: host
            }
        )?
    }

    pub async fn session_ended(&self, session_id: String) -> Result<()> {
        request!(
            self,
            SessionEnded {
                session_id: session_id
            }
        )?
    }

    pub async fn shutdown(&self) -> Result<()> {
        request!(self, Shutdown {})?
    }
}

/// Spawns the pipeline thread. The returned join handle resolves when the
/// pipeline has shut down (after a `shutdown` request or channel closure).
/// What a watch batch said about the volume root itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootHint {
    None,
    /// Only the root's metadata: nothing to capture (the engine never
    /// tracks root metadata), safe to drop.
    MetadataOnly,
    /// The root appeared, vanished, or was renamed: what a mount or unmount
    /// over the repo directory looks like. The hints that follow may be
    /// about a different tree; only a fresh watcher and a rescan are safe.
    Structural,
}

/// Removes hints that target the volume root from a batch. The engine
/// refuses any mutation of the root ("mutation cannot target the volume
/// root"), and such hints only ever come from mount lifecycle events on the
/// repo directory (for example, a fork session tearing down),
/// never from the user's edits.
fn strip_root_hints(batch: WatchBatch) -> (WatchBatch, RootHint) {
    let WatchBatch::Changes {
        epoch,
        first_sequence,
        next_sequence,
        changes,
    } = batch
    else {
        return (batch, RootHint::None);
    };
    let is_root = |path: &acyclic_fs::kernel::NamespacePath| path.components().is_empty();
    let mut hint = RootHint::None;
    let changes = changes
        .into_iter()
        .filter(|change| {
            let root_hint = match change {
                WatchChange::MetadataChanged(path) if is_root(path) => RootHint::MetadataOnly,
                WatchChange::Created(path)
                | WatchChange::Modified(path)
                | WatchChange::Removed(path)
                    if is_root(path) =>
                {
                    RootHint::Structural
                }
                WatchChange::Renamed { from, to } if is_root(from) || is_root(to) => {
                    RootHint::Structural
                }
                _ => RootHint::None,
            };
            if root_hint == RootHint::None {
                return true;
            }
            if root_hint == RootHint::Structural || hint == RootHint::None {
                hint = root_hint;
            }
            false
        })
        .collect();
    (
        WatchBatch::Changes {
            epoch,
            first_sequence,
            next_sequence,
            changes,
        },
        hint,
    )
}

/// Starts the pipeline on its own thread and returns the handle to it.
///
/// # Panics
///
/// If the OS refuses to create the thread or tokio its runtime: nothing
/// else in the daemon can run without the pipeline, so this is fatal.
#[allow(
    clippy::expect_used,
    reason = "documented above: a daemon without its pipeline thread cannot serve anything"
)]
pub fn spawn(
    store: Store,
    index: Index,
    config: Config,
) -> (PipelineHandle, std::thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(1024);
    let thread = std::thread::Builder::new()
        .name(format!("{}-pipeline", crate::product::NAME))
        // The fs facade's futures are large and a few of them nest per
        // request (a subtree copy, a restore); the 2 MiB default is tight
        // in debug builds. Virtual reservation only: untouched pages cost
        // nothing.
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("pipeline runtime");
            runtime.block_on(run(store, index, config, receiver));
        })
        .expect("spawn pipeline thread");
    (PipelineHandle { sender }, thread)
}

struct Pipeline {
    store: Store,
    index: Index,
    config: Config,
    watch: Option<NativeWatch>,
    options: CaptureOptions,
    /// Paths that never enter a checkpoint (`exclude` in the config).
    exclusions: Exclusions,
    cancel: CancellationToken,
    state: State,
    last_generation: GenerationId,
    last_checkpoint_row: Option<i64>,
    checkpoints_since_commit: u32,
    last_activity: Instant,
    /// Watcher invalidations and recovery rescans since start; see
    /// [`WatcherHealth`].
    watcher_health: WatcherHealth,
    /// Watcher changes an idle tick drained into the checkout that no
    /// checkpoint has recorded yet: the generation that was current when
    /// they were drained, and when the last of them arrived. Cleared once
    /// any checkpoint (auto or requested) moves `last_generation` past it.
    auto_pending: Option<AutoPending>,
}

#[derive(Clone, Copy)]
struct AutoPending {
    since_generation: GenerationId,
    last_change: Instant,
}

async fn run(store: Store, index: Index, config: Config, mut receiver: mpsc::Receiver<Request>) {
    let mut pipeline = match Pipeline::start(store, index, config).await {
        Ok(pipeline) => pipeline,
        Err(error) => {
            // Baseline failed: drain requests with the startup error.
            let message = format!("pipeline failed to start: {error}");
            while let Some(request) = receiver.recv().await {
                fail_request(request, &message);
            }
            return;
        }
    };

    // Wake for whichever idle check is due sooner: the coarse authority
    // commit, or the auto-checkpoint safety net (disabled: `commit_idle_ms`
    // alone paces the loop, same as before this existed).
    let tick_ms = if pipeline.config.auto_checkpoint_idle_ms == 0 {
        pipeline.config.commit_idle_ms
    } else {
        pipeline
            .config
            .commit_idle_ms
            .min(pipeline.config.auto_checkpoint_idle_ms)
    };
    let tick = Duration::from_millis(tick_ms);
    // A fixed deadline, not a timeout restarted per request: a host that
    // polls `status` every few seconds would otherwise keep pushing the
    // idle checks out and pending watcher changes would never be
    // checkpointed on their own.
    let mut next_tick = tokio::time::Instant::now() + tick;
    loop {
        let request = match tokio::time::timeout_at(next_tick, receiver.recv()).await {
            Ok(Some(request)) => request,
            Ok(None) => break, // all handles dropped
            Err(_) => {
                next_tick = tokio::time::Instant::now() + tick;
                pipeline.idle_commit().await;
                pipeline.auto_checkpoint().await;
                continue;
            }
        };
        if pipeline.handle(request).await {
            break;
        }
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "the arms look identical but each `reply` is a differently typed sender"
)]
fn fail_request(request: Request, error: impl std::fmt::Display) {
    let message = error.to_string();
    let error = || EngineError::Store(message.clone());
    match request {
        Request::Checkpoint { reply, .. } => drop(reply.send(Err(error()))),
        Request::Commit { reply } => drop(reply.send(Err(error()))),
        Request::Rewind { reply, .. } => drop(reply.send(Err(error()))),
        Request::Diff { reply, .. } => drop(reply.send(Err(error()))),
        Request::Fork { reply } => drop(reply.send(Err(error()))),
        Request::Promote { reply, .. } => drop(reply.send(Err(error()))),
        Request::ScratchCheckout { reply, .. } => drop(reply.send(Err(error()))),
        Request::PublishHead { reply } => drop(reply.send(Err(error()))),
        Request::MergePlan { reply, .. } => drop(reply.send(Err(error()))),
        Request::BuildGeneration { reply, .. } => drop(reply.send(Err(error()))),
        Request::ApplyToOverlay { reply, .. } => drop(reply.send(Err(error()))),
        Request::ReadFiles { reply, .. } => drop(reply.send(Err(error()))),
        Request::MaterializePaths { reply, .. } => drop(reply.send(Err(error()))),
        Request::RecordGeneration { reply, .. } => drop(reply.send(Err(error()))),
        Request::SnapshotOverlay { reply, .. } => drop(reply.send(Err(error()))),
        Request::RestorePath { reply, .. } => drop(reply.send(Err(error()))),
        Request::RestorePaths { reply, .. } => drop(reply.send(Err(error()))),
        Request::TurnStarted { reply, .. } => drop(reply.send(Err(error()))),
        Request::Status { reply } => drop(reply.send(Err(error()))),
        Request::SessionStarted { reply, .. } | Request::SessionEnded { reply, .. } => {
            drop(reply.send(Err(error())));
        }
        Request::Shutdown { reply } => drop(reply.send(Ok(()))),
    }
}

impl Pipeline {
    async fn start(store: Store, index: Index, config: Config) -> Result<Self> {
        let cancel = CancellationToken::new();
        let repo_root: PathBuf = store.repo_root.clone();

        let options = CaptureOptions {
            source_root: repo_root,
            // First filesystem demand replaces this before any capture. Keeping
            // startup free of native root access lets metadata-only consumers
            // run even when the repository is temporarily unavailable.
            expected_root_identity: acyclic_fs::NativeRootIdentity::from_bytes([0; 16]),
            maximum_paths: MAXIMUM_CAPTURE_PATHS,
            maximum_extent_spans: MAXIMUM_EXTENT_SPANS,
        };

        let exclusions = Exclusions::parse(&config.exclude)?;
        let pipeline = Self {
            store,
            index,
            config,
            watch: None,
            options,
            exclusions,
            cancel,
            state: State::NeedsBaseline,
            last_generation: GenerationId::new(acyclic_fs::Digest::ZERO),
            last_checkpoint_row: None,
            checkpoints_since_commit: 0,
            last_activity: Instant::now(),
            watcher_health: WatcherHealth::default(),
            auto_pending: None,
        };
        Ok(pipeline)
    }

    #[cfg(target_os = "windows")]
    fn admit_continuity(store: &Store, index: &Index, watch: &mut NativeWatch) -> Result<bool> {
        let bytes = match std::fs::read(store.paths.continuity()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Ok(false),
        };
        let Some(record) = ContinuityRecord::decode(&bytes) else {
            return Ok(false);
        };
        let Some(row) = index.latest()? else {
            return Ok(false);
        };
        if !row.published
            || row.id != record.row
            || row.generation != record.generation
            || store.checkout.generation_id() != record.generation
        {
            return Ok(false);
        }
        Ok(matches!(
            watch
                .accept_windows_usn_baseline(record.usn)
                .map_err(EngineError::fs("admit Windows continuity"))?,
            Ok(acyclic_fs::WindowsUsnContinuity::Unchanged)
        ))
    }

    async fn ensure_ready(&mut self) -> Result<()> {
        if self.state == State::Ready {
            return Ok(());
        }
        if self.watch.is_none() {
            let repo_root = self.store.repo_root.clone();
            self.options.expected_root_identity =
                capture_root_identity(&repo_root).map_err(EngineError::fs("root identity"))?;
            let mut watch = NativeWatch::open(
                &repo_root,
                NativeWatchOptions {
                    limits: VolumeLimits::default(),
                    maximum_queued_changes: WATCH_QUEUE,
                    recursive: true,
                },
            )
            .map_err(EngineError::fs("open watcher"))?;
            #[cfg(target_os = "windows")]
            if Self::admit_continuity(&self.store, &self.index, &mut watch)? {
                let latest = self.index.latest()?.ok_or_else(|| {
                    EngineError::Store("continuity admitted without an index row".into())
                })?;
                self.last_generation = latest.generation;
                self.last_checkpoint_row = Some(latest.id);
                self.watch = Some(watch);
                self.state = State::Ready;
                return Ok(());
            }
            watch
                .begin_rescan()
                .map_err(EngineError::fs("begin rescan"))?;
            self.watch = Some(watch);
        }
        self.baseline(CheckpointKind::Baseline).await
    }

    /// Full baseline: capture the whole tree, checkpoint, finish the watcher
    /// rescan, publish. Used at startup and after RescanRequired/rewind.
    #[allow(
        clippy::too_many_lines,
        reason = "rescan retries share one publication boundary"
    )]
    async fn baseline(&mut self, kind: CheckpointKind) -> Result<()> {
        crate::trace!(
            "pipeline",
            "baseline kind={kind:?}: full-tree rescan starting"
        );
        let baseline_started = Instant::now();
        self.state = State::Baselining;
        // A newly created hard link can race the rescan tail. Restart with a
        // fresh watcher so the next full capture sees every alias together.
        for attempt in 0..3 {
            #[cfg(target_os = "windows")]
            let continuity = acyclic_fs::capture_windows_usn_checkpoint(&self.store.repo_root).ok();
            // A baseline requires a clean checkout. Mid-session (watcher
            // invalidation, rewind) the overlay holds uncommitted captures:
            // publish them first. At startup this is a no-op.
            let phase = Instant::now();
            self.commit_engine().await?;
            let precommit_ms = crate::trace::ms(phase);
            let phase = Instant::now();
            capture_baseline(
                &mut self.store.checkout,
                &self.options,
                WorkCounters::UNBOUNDED,
                &self.cancel,
            )
            .await
            .map_err(EngineError::fs("capture baseline"))?;
            let capture_ms = crate::trace::ms(phase);
            let phase = Instant::now();
            self.scrub_exclusions().await?;
            let scrub_ms = crate::trace::ms(phase);
            // Changes that raced the baseline arrive as the rescan-completion
            // batch; fold them in before declaring Ready.
            let batch = self
                .watch
                .as_mut()
                .ok_or_else(|| EngineError::Store("watcher is inactive".into()))?
                .finish_rescan()
                .map_err(EngineError::fs("finish rescan"))?;
            // A root hint here is already covered by the rescan that just ran.
            let (batch, root) = strip_root_hints(batch);
            if matches!(batch, WatchBatch::RescanRequired { .. }) {
                self.watcher_health.invalidations += 1;
                self.watcher_health.last_reason = Some("rescan tail invalidated".into());
                self.reset_watch().await?;
                crate::trace!(
                    "pipeline",
                    "baseline rescan tail invalidated; retry {}",
                    attempt + 1
                );
                continue;
            }
            if root != RootHint::None {
                crate::trace!(
                    "pipeline",
                    "rescan tail: root hint ({root:?}) dropped, covered by the rescan"
                );
            }
            if let WatchBatch::Changes { ref changes, .. } = batch {
                if !changes.is_empty() {
                    let captured = capture_watch_batch(
                        &mut self.store.checkout,
                        batch,
                        &self.options,
                        WorkCounters::UNBOUNDED,
                        &self.cancel,
                    )
                    .await;
                    if let Err(failure) = captured {
                        if matches!(
                            failure.error,
                            acyclic_fs::CaptureError::RescanRequired { .. }
                        ) {
                            self.watcher_health.invalidations += 1;
                            self.watcher_health.last_reason = Some(failure.error.to_string());
                            self.reset_watch().await?;
                            crate::trace!(
                                "pipeline",
                                "baseline rescan tail needs full capture; retry {}",
                                attempt + 1
                            );
                            continue;
                        }
                        return Err(EngineError::fs("capture rescan tail")(failure));
                    }
                    self.scrub_exclusions().await?;
                }
            }
            let phase = Instant::now();
            let generation = self.checkpoint_engine().await?;
            let snapshot_ms = crate::trace::ms(phase);
            let row = self
                .index
                .record(generation, kind, &Attribution::default())?;
            self.last_generation = generation;
            self.last_checkpoint_row = Some(row);
            let phase = Instant::now();
            self.commit_engine().await?;
            #[cfg(target_os = "windows")]
            if let Some(usn) = continuity {
                crate::store::durable_replace(
                    &self.store.paths.continuity(),
                    &ContinuityRecord {
                        generation,
                        row,
                        usn,
                    }
                    .encode(),
                )?;
            }
            let postcommit_ms = crate::trace::ms(phase);
            self.state = State::Ready;
            if kind == CheckpointKind::Recovered {
                let ms = crate::trace::ms(baseline_started);
                self.watcher_health.recovery_rescans += 1;
                self.watcher_health.recovery_ms_total += ms;
                self.watcher_health.last_recovery_ms = ms;
            }
            crate::trace!(
                "pipeline",
                "baseline phases: pre-commit {precommit_ms:.1}ms, full capture {capture_ms:.1}ms, \
             scrub {scrub_ms:.1}ms, snapshot {snapshot_ms:.1}ms, post-commit {postcommit_ms:.1}ms"
            );
            crate::trace!(
                "pipeline",
                "baseline done in {:.1}ms; state Ready",
                crate::trace::ms(baseline_started)
            );
            return Ok(());
        }
        Err(EngineError::Store(
            "baseline rescan repeatedly invalidated".into(),
        ))
    }

    /// Handles one request; returns true when the pipeline should exit.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per Request variant; the dispatch table reads best whole"
    )]
    async fn handle(&mut self, request: Request) -> bool {
        self.last_activity = Instant::now();
        if request.requires_ready() {
            if let Err(error) = self.ensure_ready().await {
                fail_request(request, error);
                return false;
            }
        }
        match request {
            Request::Checkpoint {
                kind,
                attribution,
                reply,
            } => {
                crate::trace!(
                    "pipeline",
                    "request Checkpoint kind={kind:?} session={:?} tool={:?} admitted \
                     (queue depth unknown to the hook: ack was sent on send)",
                    attribution.session_id,
                    attribution.tool_name
                );
                let result = self.checkpoint(kind, &attribution).await;
                if let Err(error) = &result {
                    // Record the failure but keep the pipeline alive.
                    let _ = self.index.record_failure(
                        self.last_generation,
                        &error.to_string(),
                        &attribution,
                    );
                }
                let _ = reply.send(result);
                false
            }
            Request::Commit { reply } => {
                let _ = reply.send(self.commit_engine().await);
                false
            }
            Request::Rewind { target, reply } => {
                let _ = reply.send(self.rewind(target).await);
                false
            }
            Request::RestorePath {
                target,
                path,
                reply,
            } => {
                let _ = reply.send(self.restore_path(target, &path).await);
                false
            }
            Request::RestorePaths {
                target,
                paths,
                safety,
                label,
                reply,
            } => {
                let _ = reply.send(self.restore_paths(target, &paths, safety, label).await);
                false
            }
            Request::TurnStarted {
                session_id,
                prompt,
                reply,
            } => {
                let _ = reply.send(self.index.turn_started(&session_id, &prompt));
                false
            }
            Request::Diff {
                before,
                after,
                reply,
            } => {
                let _ = reply.send(diff::diff(&self.store, before, after).await);
                false
            }
            Request::Fork { reply } => {
                let _ = reply.send(self.fork().await);
                false
            }
            Request::ScratchCheckout { base, reply } => {
                let _ = reply.send(self.scratch_checkout(base).await);
                false
            }
            Request::PublishHead { reply } => {
                let _ = reply.send(self.publish_head().await);
                false
            }
            Request::RecordGeneration {
                generation,
                kind,
                label,
                reply,
            } => {
                let result = self
                    .index
                    .record(
                        generation,
                        kind,
                        &Attribution {
                            label: Some(label),
                            ..Attribution::default()
                        },
                    )
                    .and_then(|id| self.index.by_id(id))
                    .and_then(|row| {
                        row.ok_or_else(|| EngineError::Store("recorded row vanished".into()))
                    });
                let _ = reply.send(result);
                false
            }
            Request::SnapshotOverlay { shared, reply } => {
                let result = async {
                    let guard = shared.lock().await;
                    Ok(guard
                        .checkpoint(WorkCounters::UNBOUNDED, &self.cancel)
                        .await
                        .map_err(EngineError::fs("overlay snapshot"))?
                        .value)
                }
                .await;
                let _ = reply.send(result);
                false
            }
            Request::MergePlan {
                base,
                theirs,
                ours,
                ours_name,
                reply,
            } => {
                // Boxed: the fs facade's futures are large, and inlining
                // them into the pipeline's main state machine overflows the
                // thread stack.
                let limits = self.config.merge.limits();
                let result = Box::pin(crate::merge::plan(
                    &self.store,
                    base,
                    theirs,
                    ours,
                    &ours_name,
                    &limits,
                ))
                .await;
                let _ = reply.send(result);
                false
            }
            Request::BuildGeneration {
                from,
                entries,
                reply,
            } => {
                let result = Box::pin(self.build_generation(from, entries)).await;
                let _ = reply.send(result);
                false
            }
            Request::ApplyToOverlay {
                shared,
                entries,
                reply,
            } => {
                let result = Box::pin(async {
                    let mut guard = shared.lock().await;
                    crate::merge::apply_entries(&self.store, &mut guard, &entries).await
                })
                .await;
                let _ = reply.send(result);
                false
            }
            Request::ReadFiles {
                generation,
                paths,
                reply,
            } => {
                let result = Box::pin(async {
                    let mut out = Vec::with_capacity(paths.len());
                    for path in paths {
                        let bytes = crate::merge::read_file(&self.store, generation, &path).await?;
                        out.push((path, bytes));
                    }
                    Ok(out)
                })
                .await;
                let _ = reply.send(result);
                false
            }
            Request::MaterializePaths {
                generation,
                root,
                paths,
                reply,
            } => {
                let result = Box::pin(crate::merge::materialize_paths(
                    &self.store,
                    generation,
                    &root,
                    &paths,
                ))
                .await;
                let _ = reply.send(result);
                false
            }
            Request::Promote {
                shared,
                base,
                label,
                reply,
            } => {
                let _ = reply.send(self.promote(shared, base, &label).await);
                false
            }
            Request::Status { reply } => {
                let _ = reply.send(Ok(StatusReport {
                    state: self.state,
                    last_checkpoint: self.last_checkpoint_row,
                    unpublished: self.index.unpublished_count().unwrap_or(0),
                    checkpoints_since_commit: self.checkpoints_since_commit,
                    watcher: self.watcher_health.clone(),
                }));
                false
            }
            Request::SessionStarted {
                session_id,
                host,
                reply,
            } => {
                let result = async {
                    self.index.session_started(&session_id, &host)?;
                    // Session start is a coarse boundary.
                    self.commit_engine().await
                }
                .await;
                let _ = reply.send(result);
                false
            }
            Request::SessionEnded { session_id, reply } => {
                let result = async {
                    self.index.session_ended(&session_id)?;
                    self.commit_engine().await
                }
                .await;
                let _ = reply.send(result);
                false
            }
            Request::Shutdown { reply } => {
                let _ = reply.send(self.commit_engine().await);
                true
            }
        }
    }

    /// One checkpoint: quiesce the watcher, capture pending change batches,
    /// snapshot with `checkpoint()`, and index the result.
    async fn checkpoint(
        &mut self,
        kind: CheckpointKind,
        attribution: &Attribution,
    ) -> Result<CheckpointOutcome> {
        let started = Instant::now();
        if self.state != State::Ready {
            // A failed recovery leaves state at Baselining; a request is the
            // natural moment to retry rather than staying down forever.
            crate::trace!(
                "pipeline",
                "checkpoint kind={:?}: state {:?} -> reset watch + recovery baseline first",
                kind,
                self.state
            );
            self.reset_watch().await?;
            self.baseline(CheckpointKind::Recovered).await?;
        }
        let drain_started = Instant::now();
        // Changes an idle tick already drained into the checkout but has
        // not recorded yet count as pending too: without this the row
        // would be a noop at the previous generation and lose them.
        let pending = self.has_pending_drained_changes();
        let changed = self.drain_watcher().await? || pending;
        let drain_ms = crate::trace::ms(drain_started);
        let capture_started = Instant::now();
        let (generation, kind) = if changed {
            (self.checkpoint_engine().await?, kind)
        } else {
            (self.last_generation, CheckpointKind::Noop)
        };
        let capture_ms = crate::trace::ms(capture_started);
        let record_started = Instant::now();
        let row = self.index.record(generation, kind, attribution)?;
        let record_ms = crate::trace::ms(record_started);
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);
        if changed {
            // Only now: a failure above leaves the marker so a later tick
            // or request still snapshots what was drained.
            self.auto_pending = None;
        }
        self.checkpoints_since_commit += 1;
        let mut commit_ms = 0.0;
        if self.checkpoints_since_commit >= self.config.commit_every {
            let commit_started = Instant::now();
            self.commit_engine().await?;
            commit_ms = crate::trace::ms(commit_started);
        }
        crate::trace!(
            "pipeline",
            "checkpoint row #{row} kind={:?}: drain {drain_ms:.1}ms ({}), {} {capture_ms:.1}ms, \
             index {record_ms:.1}ms, publish {commit_ms:.1}ms, total {:.1}ms",
            kind,
            if changed {
                "changes captured"
            } else {
                "no changes"
            },
            if changed { "snapshot" } else { "noop" },
            crate::trace::ms(started)
        );
        Ok(CheckpointOutcome {
            row_id: row,
            generation,
            kind,
        })
    }

    /// Captures every available watcher batch, then waits for `quiesce_ms`
    /// of quiet after the first change (capped at `quiesce_cap_ms`). An empty
    /// first poll returns immediately. Returns whether anything was captured.
    #[allow(
        clippy::too_many_lines,
        reason = "watch invalidation and capture share one poll loop"
    )]
    async fn drain_watcher(&mut self) -> Result<bool> {
        let quiesce = Duration::from_millis(self.config.quiesce_ms);
        let cap = Duration::from_millis(self.config.quiesce_cap_ms);
        let started = Instant::now();
        let mut last_change = None;
        let mut changed = false;
        let mut polls = 0u32;
        let mut batches = 0u32;
        let mut hints = 0usize;
        loop {
            polls += 1;
            let batch = self
                .watch
                .as_mut()
                .ok_or_else(|| EngineError::Store("watcher is inactive".into()))?
                .poll(POLL_CHANGES, WorkCounters::UNBOUNDED, &self.cancel)
                .map_err(EngineError::fs("watch poll"))?
                .value;
            let (batch, root) = strip_root_hints(batch);
            if root != RootHint::None {
                crate::trace!(
                    "pipeline",
                    "drain: watcher hinted at the volume root ({root:?}); dropped"
                );
            }
            let (batch, scrub) = self.exclusions.filter_batch(batch);
            if root == RootHint::Structural {
                // The repo directory itself changed identity (a mount came
                // or went): re-baseline on a fresh watcher rather than apply
                // hints that may describe another tree.
                crate::trace!(
                    "pipeline",
                    "drain: structural root hint -> fresh watcher + recovery baseline"
                );
                self.reset_watch().await?;
                self.baseline(CheckpointKind::Recovered).await?;
                return Ok(true);
            }
            match batch {
                WatchBatch::Changes { ref changes, .. } if !changes.is_empty() => {
                    batches += 1;
                    hints += changes.len();
                    let captured = capture_watch_batch(
                        &mut self.store.checkout,
                        batch,
                        &self.options,
                        WorkCounters::UNBOUNDED,
                        &self.cancel,
                    )
                    .await;
                    if let Err(failure) = captured {
                        if matches!(
                            failure.error,
                            acyclic_fs::CaptureError::RescanRequired { .. }
                        ) {
                            self.watcher_health.invalidations += 1;
                            self.watcher_health.last_reason = Some(failure.error.to_string());
                            self.reset_watch().await?;
                            self.baseline(CheckpointKind::Recovered).await?;
                            return Ok(true);
                        }
                        return Err(EngineError::fs("capture watch batch")(failure));
                    }
                    if scrub {
                        self.scrub_exclusions().await?;
                    }
                    changed = true;
                    last_change = Some(Instant::now());
                }
                WatchBatch::Changes { .. } => {
                    if last_change.is_none()
                        || last_change.is_some_and(|last| last.elapsed() >= quiesce)
                        || started.elapsed() >= cap
                    {
                        crate::trace!(
                            "pipeline",
                            "drain: done after {:.1}ms, {polls} polls, {batches} batch(es), \
                             {hints} hint(s); stopped by {}",
                            crate::trace::ms(started),
                            if last_change.is_none() {
                                "empty poll"
                            } else if started.elapsed() >= cap {
                                "cap"
                            } else {
                                "quiesce window"
                            }
                        );
                        return Ok(changed);
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                WatchBatch::RescanRequired { epoch, reason } => {
                    // Watcher overflow or invalidation: rebuild from scratch
                    // with a FRESH watcher. Reusing the invalidated one is a
                    // trap — if the baseline fails after begin_rescan, the
                    // old watcher stays wedged in RescanInProgress forever.
                    crate::trace!(
                        "pipeline",
                        "drain: watcher INVALIDATED after {:.1}ms ({polls} polls, {batches} \
                         batch(es), {hints} hint(s) captured first): epoch {epoch:?}, reason: \
                         {reason}; fresh watcher + full recovery baseline",
                        crate::trace::ms(started)
                    );
                    self.watcher_health.invalidations += 1;
                    self.watcher_health.last_reason = Some(reason.to_string());
                    self.reset_watch().await?;
                    self.baseline(CheckpointKind::Recovered).await?;
                    return Ok(true);
                }
            }
        }
    }

    /// Drops excluded paths a capture may have pulled into the checkout,
    /// before the generation they would otherwise land in is checkpointed.
    async fn scrub_exclusions(&mut self) -> Result<()> {
        let removed = self.exclusions.scrub(&mut self.store.checkout).await?;
        if removed > 0 {
            crate::trace!(
                "pipeline",
                "exclusions: scrubbed {removed} excluded path(s) from the checkout"
            );
        }
        Ok(())
    }

    /// `checkpoint()`: snapshot without authority publish. The fast path.
    async fn checkpoint_engine(&mut self) -> Result<GenerationId> {
        Ok(self
            .store
            .checkout
            .checkpoint(WorkCounters::UNBOUNDED, &self.cancel)
            .await
            .map_err(EngineError::fs("checkpoint"))?
            .value)
    }

    /// `commit()`: authority publish (O(tree) closure proof). Coarse
    /// boundaries only. "Nothing pending" is success.
    async fn commit_engine(&mut self) -> Result<()> {
        let outcome = self
            .store
            .checkout
            .commit(OperationId::new(), WorkCounters::UNBOUNDED, &self.cancel)
            .await;
        match outcome {
            Ok(receipt) => match receipt.value {
                CheckoutCommitOutcome::Committed { .. }
                | CheckoutCommitOutcome::AlreadyCommitted { .. } => {
                    if let Some(row) = self.last_checkpoint_row {
                        self.index.mark_published(row)?;
                    }
                    self.checkpoints_since_commit = 0;
                    Ok(())
                }
                other => Err(EngineError::Fs(format!(
                    "unexpected commit outcome (single-writer invariant broken): {other:?}"
                ))),
            },
            Err(failure) if matches!(failure.error, acyclic_fs::FsError::NoPendingMutations) => {
                // Nothing new to publish means every recorded row's
                // generation is already covered by the last publish — noop
                // and quiet pre_rewind rows included.
                if let Some(row) = self.last_checkpoint_row {
                    self.index.mark_published(row)?;
                }
                self.checkpoints_since_commit = 0;
                Ok(())
            }
            Err(failure) => Err(EngineError::Fs(format!("commit: {failure:?}"))),
        }
    }

    async fn idle_commit(&mut self) {
        if self.state == State::Ready
            && self.checkpoints_since_commit > 0
            && self.last_activity.elapsed() >= Duration::from_millis(self.config.commit_idle_ms)
        {
            let _ = self.commit_engine().await;
        }
    }

    /// The safety net for hosts with no lifecycle-hook API (Claude Desktop
    /// over MCP): a checkpoint no request asked for, taken once the watcher
    /// has been quiet for `auto_checkpoint_idle_ms`. Runs on every idle
    /// tick: drains whatever the watcher has (an empty watcher returns after
    /// one poll; hook-driven hosts usually drained on their own pre/post-tool
    /// checkpoints), then records a row only once a full tick has passed with
    /// no further changes and nothing else has checkpointed them in the
    /// meantime. Never records a row when nothing changed, so it cannot spam
    /// the timeline.
    async fn auto_checkpoint(&mut self) {
        if self.config.auto_checkpoint_idle_ms == 0 {
            return;
        }
        // An idle daemon must remain O(1) in repository size. The first
        // consumer operation that needs authenticated contents performs the
        // baseline; a timer alone is not such an operation.
        if self.state == State::NeedsBaseline {
            return;
        }
        if self.state != State::Ready {
            return;
        }
        // Like `idle_commit`, failures here are advisory: the pending state
        // survives them, so the next tick retries, and nothing is waiting on
        // a reply.
        // The marker is keyed on the generation current *after* the drain:
        // a drain that re-baselined (structural root hint, `Recovered` row)
        // moved it, and edits the rescan captured after that row still need
        // a checkpoint. `try_auto_checkpoint` records nothing when the tree
        // hashes the same as the last row, so a recovery that captured
        // nothing new costs no spurious `Auto` row either.
        let mark_pending = |pipeline: &mut Self| {
            pipeline.auto_pending = Some(AutoPending {
                since_generation: pipeline.last_generation,
                last_change: Instant::now(),
            });
        };
        match self.drain_watcher().await {
            Ok(true) => mark_pending(self),
            Ok(false) => {}
            Err(error) => {
                // A batch may have been captured before the failure; keep
                // (or start) the marker so the next tick snapshots it
                // rather than trusting an empty watcher.
                crate::trace!("pipeline", "auto-checkpoint drain failed: {error}");
                if !self.has_pending_drained_changes() {
                    mark_pending(self);
                }
                return;
            }
        }
        if !self.has_pending_drained_changes() {
            return;
        }
        let Some(pending) = self.auto_pending else {
            return;
        };
        if pending.last_change.elapsed()
            < Duration::from_millis(self.config.auto_checkpoint_idle_ms)
        {
            return;
        }
        match self.try_auto_checkpoint().await {
            Ok(()) => self.auto_pending = None,
            Err(error) => crate::trace!("pipeline", "auto-checkpoint failed: {error}"),
        }
    }

    /// Whether an idle tick drained changes that no checkpoint has recorded
    /// since. A marker from before the last recorded generation is stale
    /// (a requested checkpoint, rewind, fork or promote snapshotted the
    /// checkout in between) and is dropped here.
    fn has_pending_drained_changes(&mut self) -> bool {
        match self.auto_pending {
            Some(pending) if pending.since_generation == self.last_generation => true,
            Some(_) => {
                self.auto_pending = None;
                false
            }
            None => false,
        }
    }

    async fn try_auto_checkpoint(&mut self) -> Result<()> {
        let generation = self.checkpoint_engine().await?;
        if generation == self.last_generation {
            // Whatever was drained left the tree identical to the last
            // recorded row (a recovery baseline, or a write that restored
            // the previous bytes): nothing to record.
            crate::trace!(
                "pipeline",
                "auto-checkpoint: tree unchanged since last row, no row"
            );
            return Ok(());
        }
        let row = self
            .index
            .record(generation, CheckpointKind::Auto, &Attribution::default())?;
        crate::trace!(
            "pipeline",
            "auto-checkpoint row #{row}: idle timer, watcher changes went quiet"
        );
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);
        self.checkpoints_since_commit += 1;
        if self.checkpoints_since_commit >= self.config.commit_every {
            self.commit_engine().await?;
        }
        Ok(())
    }

    async fn rewind(&mut self, target: CheckpointRow) -> Result<RewindOutcome> {
        self.state = State::Rewinding;
        // Safety net first: the pre-rewind state must itself be a checkpoint.
        self.drain_watcher().await?;
        let safety = self.checkpoint_engine().await?;
        let safety_row = self.index.record(
            safety,
            CheckpointKind::PreRewind,
            &Attribution {
                label: Some(format!("before rewind to #{}", target.id)),
                rewind_target: Some(target.id),
                ..Attribution::default()
            },
        )?;
        self.last_generation = safety;
        self.last_checkpoint_row = Some(safety_row);
        self.commit_engine().await?;

        self.swap_root_and_rebaseline(target.generation).await
    }

    /// Single-path restore, bracketed by checkpoints: a `manual` safety row
    /// before if the tree has uncaptured edits (so the pre-restore content
    /// is one `acyclic restore` away) and a `manual` row after, so the
    /// timeline shows what the restore changed. Not `pre_rewind`: a restore
    /// abandons nothing, so it must not read as a branch in the brief.
    /// No swap of the repo root, so the watcher stays valid and the
    /// post-restore capture is an ordinary incremental one.
    async fn restore_path(
        &mut self,
        target: CheckpointRow,
        path: &std::path::Path,
    ) -> Result<RestoreOutcome> {
        let mut restored = self
            .restore_paths(target, &[path.to_path_buf()], true, None)
            .await?;
        restored
            .outcomes
            .pop()
            .ok_or_else(|| EngineError::Restore("restore produced no outcome".into()))
    }

    /// Captures `paths` without waiting for the watcher: the caller wrote
    /// them and knows it. The native echo of those writes, 100ms+ later,
    /// is harmless: a modified hint on a path whose content already matches
    /// captures nothing, and a staged sibling's create+rename resolves to an
    /// absent path.
    async fn capture_paths_directly(&mut self, paths: &[PathBuf]) -> Result<()> {
        // One hint per path, plus one per distinct parent directory: the
        // rename into place changed the parent's metadata, and the native
        // watcher would have said so. Without it the parent's mtime lands
        // in a later, spurious row.
        let mut hints = Vec::with_capacity(paths.len() * 2);
        let mut parents = std::collections::BTreeSet::new();
        for path in paths {
            hints.push(WatchChange::Modified(crate::merge::namespace_of(path)?));
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                parents.insert(parent.to_path_buf());
            }
        }
        for parent in parents {
            hints.push(WatchChange::MetadataChanged(crate::merge::namespace_of(
                &parent,
            )?));
        }
        let captured = capture_watch_batch(
            &mut self.store.checkout,
            WatchBatch::Changes {
                epoch: WatchEpoch::from_u64(0),
                first_sequence: WatchSequence::from_u64(0),
                next_sequence: WatchSequence::from_u64(0),
                changes: hints,
            },
            &self.options,
            WorkCounters::UNBOUNDED,
            &self.cancel,
        )
        .await;
        if let Err(failure) = captured {
            if matches!(
                failure.error,
                acyclic_fs::CaptureError::RescanRequired { .. }
            ) {
                self.watcher_health.invalidations += 1;
                self.watcher_health.last_reason = Some(failure.error.to_string());
                self.reset_watch().await?;
                self.baseline(CheckpointKind::Recovered).await?;
                return Ok(());
            }
            return Err(EngineError::fs("capture restored paths")(failure));
        }
        self.scrub_exclusions().await
    }

    /// Reconciles exact restored roots immediately, including all descendants
    /// of a directory that was replaced or removed. The SDK unions the host
    /// and checkout subtrees, so stale descendants are removed without waiting
    /// for the native watcher to enumerate the write.
    async fn capture_restored_subtrees(&mut self, paths: &[PathBuf]) -> Result<()> {
        for path in paths {
            let namespace = crate::merge::namespace_of(path)?;
            capture_subtree(
                &mut self.store.checkout,
                namespace,
                &self.options,
                WorkCounters::UNBOUNDED,
                &self.cancel,
            )
            .await
            .map_err(EngineError::fs("capture restored subtree"))?;
        }
        self.scrub_exclusions().await
    }

    /// Restores `paths` from `target` as one event: at most one safety row
    /// before, one drain and one row after, however many paths land.
    async fn restore_paths(
        &mut self,
        target: CheckpointRow,
        paths: &[PathBuf],
        safety: bool,
        label: Option<String>,
    ) -> Result<RestoredPaths> {
        let paths = crate::merge::subtree_roots(paths);
        for path in &paths {
            if self.exclusions.covers_host(path) {
                return Err(EngineError::Restore(format!(
                    "{} is excluded from snapshots (`exclude` in {}); no checkpoint holds it",
                    path.display(),
                    crate::product::repo_config_file()
                )));
            }
        }
        if self.state != State::Ready {
            self.reset_watch().await?;
            self.baseline(CheckpointKind::Recovered).await?;
        }
        let what = match paths.as_slice() {
            [path] => format!("{} from #{}", path.display(), target.id),
            _ => format!("{} path(s) from #{}", paths.len(), target.id),
        };
        let started = Instant::now();
        let mut safety_row = None;
        if safety {
            let pending = self.has_pending_drained_changes();
            if self.drain_watcher().await? || pending {
                let generation = self.checkpoint_engine().await?;
                let row = self.index.record(
                    generation,
                    CheckpointKind::Manual,
                    &Attribution {
                        label: Some(format!("before restore {what}")),
                        ..Attribution::default()
                    },
                )?;
                self.last_generation = generation;
                self.last_checkpoint_row = Some(row);
                self.auto_pending = None;
                safety_row = Some(row);
            }
        }
        let pre_ms = crate::trace::ms(started);

        let write_started = Instant::now();
        let mut outcomes = Vec::with_capacity(paths.len());
        for path in &paths {
            outcomes.push(rewind::restore_path(&self.store, target.generation, path).await?);
        }
        let write_ms = crate::trace::ms(write_started);

        // Capture the written paths directly: the pipeline knows exactly
        // what it wrote, and the native echo of those writes arrives 100ms+
        // later. The echo is harmless when it comes: a modified hint on a
        // path whose content already matches captures nothing, and the
        // staged sibling's create+rename resolves to an absent path.
        // Leaf batches avoid directory traversal. Directory and absent roots
        // use the SDK subtree reconciler, which unions live and stored
        // descendants and therefore captures replacements and removals exactly.
        let post_started = Instant::now();
        let all_leaves = paths.iter().all(|path| {
            std::fs::symlink_metadata(self.store.repo_root.join(path))
                .is_ok_and(|metadata| metadata.is_file() || metadata.is_symlink())
        });
        let capture_mode = if all_leaves {
            self.capture_paths_directly(&paths).await?;
            "direct capture"
        } else {
            self.capture_restored_subtrees(&paths).await?;
            "direct subtree capture"
        };
        let post_ms = crate::trace::ms(post_started);
        let capture_started = Instant::now();
        let generation = self.checkpoint_engine().await?;
        let row = self.index.record(
            generation,
            CheckpointKind::Manual,
            &Attribution {
                label: Some(label.unwrap_or_else(|| format!("restore {what}"))),
                ..Attribution::default()
            },
        )?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);
        self.checkpoints_since_commit += 1;
        crate::trace!(
            "pipeline",
            "restore {what} -> row #{row}: pre-drain {pre_ms:.1}ms ({}), write {} path(s) \
             {write_ms:.1}ms, {capture_mode} {post_ms:.1}ms, snapshot+index {:.1}ms, total {:.1}ms",
            match safety_row {
                Some(row) => format!("safety row #{row}"),
                None if safety => "no safety row".to_owned(),
                None => "safety skipped by caller".to_owned(),
            },
            outcomes.len(),
            crate::trace::ms(capture_started),
            crate::trace::ms(started)
        );
        Ok(RestoredPaths {
            outcomes,
            generation,
            row,
        })
    }

    /// Captures the current tree as the head a promote is judged against
    /// (recorded as `promote base`), and hands it back so the caller can
    /// decide between a plain land (head == fork base) and a replay (head
    /// moved). It does not publish: authority publish is O(tree) and runs
    /// on the idle timer.
    async fn publish_head(&mut self) -> Result<GenerationId> {
        if self.state != State::Ready {
            return Err(EngineError::Capture(format!(
                "pipeline not ready ({:?})",
                self.state
            )));
        }
        self.drain_watcher().await?;
        let head = self.checkpoint_engine().await?;
        let row = self.index.record(
            head,
            CheckpointKind::Manual,
            &Attribution {
                label: Some("promote base".into()),
                ..Attribution::default()
            },
        )?;
        self.last_generation = head;
        self.last_checkpoint_row = Some(row);
        self.checkpoints_since_commit += 1;
        Ok(head)
    }

    async fn scratch_checkout(&mut self, base: GenerationId) -> Result<Arc<SharedLocalCheckout>> {
        let checkout = self
            .store
            .workspace
            .checkout(
                acyclic_fs::model::GenerationSelector::Exact(base),
                crate::store::writable_head(),
            )
            .await
            .map_err(EngineError::fs("scratch checkout"))?;
        Ok(Arc::new(SharedLocalCheckout::with_publication(
            checkout,
            MountPublication::Manual,
        )))
    }

    /// Merge v2: a scratch overlay at `from`, `entries` written over it,
    /// checkpointed as an unpublished generation. The head never moves.
    async fn build_generation(
        &mut self,
        from: GenerationId,
        entries: Vec<(PathBuf, crate::merge::Entry)>,
    ) -> Result<GenerationId> {
        let scratch = self.scratch_checkout(from).await?;
        let mut guard = scratch.lock().await;
        crate::merge::apply_entries(&self.store, &mut guard, &entries).await?;
        if !guard.has_pending_mutations() {
            return Ok(from);
        }
        Ok(guard
            .checkpoint(WorkCounters::UNBOUNDED, &self.cancel)
            .await
            .map_err(EngineError::fs("build generation"))?
            .value)
    }

    async fn fork(&mut self) -> Result<ForkSeed> {
        if self.state != State::Ready {
            return Err(EngineError::Capture(format!(
                "pipeline not ready ({:?})",
                self.state
            )));
        }
        self.drain_watcher().await?;
        let base = self.checkpoint_engine().await?;
        let row = self.index.record(
            base,
            CheckpointKind::Manual,
            &Attribution {
                label: Some("fork base".into()),
                ..Attribution::default()
            },
        )?;
        self.last_generation = base;
        self.last_checkpoint_row = Some(row);
        self.checkpoints_since_commit += 1;

        // Cut the fork at the exact base generation, published or not: an
        // authority publish is O(tree) and belongs on the idle timer, not in
        // front of a fork.
        let checkout = self
            .store
            .workspace
            .checkout(
                acyclic_fs::model::GenerationSelector::Exact(base),
                crate::store::writable_head(),
            )
            .await
            .map_err(EngineError::fs("fork checkout"))?;
        let config = checkout.volume_config();
        let volume_id = checkout.volume_id();
        Ok(ForkSeed {
            // Native close/flush must never publish: sibling forks share one
            // volume head, so a seal on close makes the next fork's mutations
            // stale. Promote is the only commit.
            shared: Arc::new(SharedLocalCheckout::with_publication(
                checkout,
                MountPublication::Manual,
            )),
            config,
            volume_id,
            base,
        })
    }

    /// Promotes a fork: publish the fork's overlay (legible conflict if the
    /// mainline moved past its base), then land the winning generation in
    /// the real tree via the rewind swap.
    async fn promote(
        &mut self,
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: &str,
    ) -> Result<PromoteOutcome> {
        // Safety net + publish the mainline. If anything real changed since
        // the fork base, the head moves and the fork's commit conflicts.
        self.drain_watcher().await?;
        let safety = self.checkpoint_engine().await?;
        let safety_row = self.index.record(
            safety,
            CheckpointKind::PreRewind,
            &Attribution {
                label: Some(format!("before {label}")),
                ..Attribution::default()
            },
        )?;
        self.last_generation = safety;
        self.last_checkpoint_row = Some(safety_row);
        self.commit_engine().await?;

        // The fork may have been cut from an unpublished checkpoint. Publishing
        // that same generation advances the authority sequence, so its checkout
        // must rebase before committing even though the mainline did not move.
        if self.store.checkout.generation_id() != base {
            return Ok(fork_moved(base));
        }

        let outcome = {
            let mut guard = shared.lock().await;
            if !guard.has_pending_mutations() {
                // Nothing was written in the fork: the tree already equals
                // the base — nothing to land.
                return Ok(PromoteOutcome::Promoted {
                    generation: base,
                    old_tree: None,
                });
            }
            let rebased = guard
                .rebase_head(0, WorkCounters::UNBOUNDED, &self.cancel)
                .await
                .map_err(EngineError::fs("fork rebase"))?
                .value;
            if matches!(
                rebased,
                acyclic_fs::kernel::RebaseDecision::Conflicted { .. }
            ) {
                return Ok(fork_moved(base));
            }
            guard
                .commit(OperationId::new(), WorkCounters::UNBOUNDED, &self.cancel)
                .await
                .map_err(EngineError::fs("fork commit"))?
                .value
        };
        let generation = match outcome {
            CheckoutCommitOutcome::Committed { generation_id, .. }
            | CheckoutCommitOutcome::AlreadyCommitted { generation_id, .. } => generation_id,
            CheckoutCommitOutcome::Conflict { .. } | CheckoutCommitOutcome::Fenced { .. } => {
                return Ok(fork_moved(base));
            }
            other => {
                return Err(EngineError::Fs(format!(
                    "unexpected fork commit outcome: {other:?}"
                )))
            }
        };

        // The head advanced under our checkout: replace it, then land the
        // winner with the same journaled swap a rewind uses.
        self.state = State::Rewinding;
        self.store.checkout = self
            .store
            .workspace
            .checkout(
                acyclic_fs::model::GenerationSelector::Head,
                crate::store::writable_head(),
            )
            .await
            .map_err(EngineError::fs("refresh checkout"))?;
        let row = self.index.record(
            generation,
            CheckpointKind::Manual,
            &Attribution {
                label: Some(label.to_owned()),
                ..Attribution::default()
            },
        )?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);

        let swap = self.swap_root_and_rebaseline(generation).await?;
        Ok(PromoteOutcome::Promoted {
            generation,
            old_tree: Some(swap.old_tree),
        })
    }

    /// Stops the old event source before the intentional root exchange. An
    /// old callback must never publish a hint into the new watcher's epoch.
    async fn swap_root_and_rebaseline(
        &mut self,
        generation: GenerationId,
    ) -> Result<RewindOutcome> {
        drop(self.watch.take());
        let prepared = rewind::prepare(
            &self.store,
            generation,
            self.config.trash_ttl_days,
            &self.exclusions,
        )
        .await?;
        let current = self
            .store
            .workspace
            .head()
            .await
            .map_err(EngineError::fs("workspace head before rewind"))?;
        let target = self
            .store
            .workspace
            .generation(generation)
            .await
            .map_err(EngineError::fs("rewind generation"))?;
        prepared.mark_restoring_head()?;
        let key_bytes: [u8; 16] = generation.digest().as_bytes()[..16]
            .try_into()
            .map_err(|_| EngineError::Store("invalid generation digest".into()))?;
        match self
            .store
            .workspace
            .restore_generation(
                &target,
                current.id(),
                acyclic_fs::IdempotencyKey::from_bytes(key_bytes),
            )
            .await
            .map_err(EngineError::fs("restore workspace generation"))?
        {
            WorkspaceRestore::Restored(_)
            | WorkspaceRestore::AlreadyRestored(_)
            | WorkspaceRestore::Current(_) => {}
            WorkspaceRestore::Stale(_)
            | WorkspaceRestore::Fenced
            | WorkspaceRestore::IdempotencyConflict => {
                return Err(EngineError::Store(
                    "workspace head changed during rewind".into(),
                ));
            }
        }
        let outcome = prepared.publish();
        self.store.checkout = self
            .store
            .workspace
            .checkout(
                acyclic_fs::model::GenerationSelector::Head,
                crate::store::writable_head(),
            )
            .await
            .map_err(EngineError::fs("refresh rewind checkout"))?;
        self.reset_watch().await?;
        self.baseline(CheckpointKind::Recovered).await?;
        outcome
    }

    /// Reopens the watcher and recomputes the capture root identity — needed
    /// whenever the repo directory inode may have changed (after a rewind).
    async fn reset_watch(&mut self) -> Result<()> {
        crate::trace!(
            "pipeline",
            "reset_watch: reopening the native watcher (old queue discarded)"
        );
        let repo_root = self.store.repo_root.clone();
        self.options.expected_root_identity =
            capture_root_identity(&repo_root).map_err(EngineError::fs("root identity"))?;
        self.watch = Some(
            NativeWatch::open(
                &repo_root,
                NativeWatchOptions {
                    limits: VolumeLimits::default(),
                    maximum_queued_changes: WATCH_QUEUE,
                    recursive: true,
                },
            )
            .map_err(EngineError::fs("reopen watcher"))?,
        );
        self.watch
            .as_mut()
            .ok_or_else(|| EngineError::Store("watcher is inactive".into()))?
            .begin_rescan()
            .map_err(EngineError::fs("begin rescan"))?;
        Ok(())
    }
}

#[cfg(test)]
mod root_hint_tests {
    use super::*;
    use acyclic_fs::kernel::NamespacePath;
    use acyclic_fs::{WatchEpoch, WatchSequence};
    use std::path::Path;

    fn root() -> NamespacePath {
        NamespacePath::new(Vec::new(), VolumeLimits::default()).unwrap()
    }

    fn file(name: &str) -> NamespacePath {
        let limits = VolumeLimits::default();
        acyclic_fs::host_path_to_namespace(Path::new(name), crate::store::host_profile(), limits)
            .unwrap()
    }

    fn batch(changes: Vec<WatchChange>) -> WatchBatch {
        WatchBatch::Changes {
            epoch: WatchEpoch::from_u64(1),
            first_sequence: WatchSequence::from_u64(1),
            next_sequence: WatchSequence::from_u64(2),
            changes,
        }
    }

    fn changes_of(batch: &WatchBatch) -> Vec<WatchChange> {
        match batch {
            WatchBatch::Changes { changes, .. } => changes.clone(),
            WatchBatch::RescanRequired { .. } => panic!("not a change batch"),
        }
    }

    #[test]
    fn ordinary_hints_pass_through_untouched() {
        let (out, hint) = strip_root_hints(batch(vec![WatchChange::Modified(file("a.txt"))]));
        assert_eq!(hint, RootHint::None);
        assert_eq!(changes_of(&out), vec![WatchChange::Modified(file("a.txt"))]);
    }

    #[test]
    fn root_metadata_hint_is_dropped_quietly() {
        let (out, hint) = strip_root_hints(batch(vec![
            WatchChange::MetadataChanged(root()),
            WatchChange::Created(file("b.txt")),
        ]));
        assert_eq!(hint, RootHint::MetadataOnly);
        assert_eq!(changes_of(&out), vec![WatchChange::Created(file("b.txt"))]);
    }

    #[test]
    fn structural_root_hints_are_reported_and_dropped() {
        for change in [
            WatchChange::Removed(root()),
            WatchChange::Created(root()),
            WatchChange::Modified(root()),
            WatchChange::Renamed {
                from: root(),
                to: file("x"),
            },
            WatchChange::Renamed {
                from: file("x"),
                to: root(),
            },
        ] {
            let (out, hint) = strip_root_hints(batch(vec![
                WatchChange::MetadataChanged(root()),
                change,
                WatchChange::Modified(file("c.txt")),
            ]));
            assert_eq!(hint, RootHint::Structural);
            assert_eq!(changes_of(&out), vec![WatchChange::Modified(file("c.txt"))]);
        }
    }

    #[test]
    fn rescan_required_is_left_alone() {
        let input = WatchBatch::RescanRequired {
            epoch: WatchEpoch::from_u64(1),
            reason: acyclic_fs::WatchInvalidationReason::InitialSnapshotRequired,
        };
        let (out, hint) = strip_root_hints(input);
        assert_eq!(hint, RootHint::None);
        assert!(matches!(out, WatchBatch::RescanRequired { .. }));
    }
}

#[cfg(all(test, target_os = "windows"))]
mod continuity_tests {
    use super::*;

    fn record() -> ContinuityRecord {
        let mut bytes = [0_u8; 40];
        bytes[..8].copy_from_slice(b"ACYUSN\0\x01");
        bytes[8..24].copy_from_slice(&[7; 16]);
        bytes[24..32].copy_from_slice(&19_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&23_u64.to_le_bytes());
        let usn = acyclic_fs::WindowsUsnCheckpoint::from_bytes(&bytes).expect("canonical fixture");
        ContinuityRecord {
            generation: GenerationId::new(acyclic_fs::Digest::from_bytes([3; 32])),
            row: 7,
            usn,
        }
    }

    #[test]
    fn continuity_record_is_canonical_and_rejects_torn_state() {
        let record = record();
        let bytes = record.encode();
        let decoded = ContinuityRecord::decode(&bytes).expect("canonical record");
        assert_eq!(decoded.generation, record.generation);
        assert_eq!(decoded.row, record.row);
        assert_eq!(decoded.usn, record.usn);
        assert!(ContinuityRecord::decode(&bytes[..CONTINUITY_BYTES - 1]).is_none());
        let mut wrong_version = bytes;
        wrong_version[0] ^= 0xff;
        assert!(ContinuityRecord::decode(&wrong_version).is_none());
        let mut invalid_row = bytes;
        invalid_row[40..48].copy_from_slice(&0_i64.to_le_bytes());
        assert!(ContinuityRecord::decode(&invalid_row).is_none());
    }

    #[test]
    fn durable_replacement_keeps_only_the_complete_new_record() {
        let directory = tempfile::tempdir().expect("store");
        let path = directory.path().join("continuity.bin");
        let first = record().encode();
        crate::store::durable_replace(&path, &first).expect("first replace");
        let mut second = record();
        second.row += 1;
        crate::store::durable_replace(&path, &second.encode()).expect("second replace");
        let stored = std::fs::read(path).expect("read marker");
        assert_eq!(
            ContinuityRecord::decode(&stored).expect("complete").row,
            second.row
        );
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;

    #[test]
    fn metadata_requests_do_not_force_a_repository_scan() {
        let (status_reply, _) = oneshot::channel();
        assert!(!Request::Status {
            reply: status_reply
        }
        .requires_ready());

        let (turn_reply, _) = oneshot::channel();
        assert!(!Request::TurnStarted {
            session_id: "session".to_owned(),
            prompt: "prompt".to_owned(),
            reply: turn_reply,
        }
        .requires_ready());

        let (checkpoint_reply, _) = oneshot::channel();
        assert!(Request::Checkpoint {
            kind: CheckpointKind::Manual,
            attribution: Attribution::default(),
            reply: checkpoint_reply,
        }
        .requires_ready());
    }
}
