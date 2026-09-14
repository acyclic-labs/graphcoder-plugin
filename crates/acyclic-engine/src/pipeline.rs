//! The capture pipeline: one thread owns the checkout, watcher, and index.
//!
//! Requests are processed FIFO. Per-tool-call snapshots use `checkpoint()`
//! (fast, no authority publish); `commit()` runs at coarse boundaries only.
//! The pipeline runs on its own thread with a current-thread tokio runtime so
//! the `&mut Checkout` discipline never meets Send bounds.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use acyclic_fs::model::VolumeLimits;
use acyclic_fs::{capture_baseline, capture_root_identity, capture_watch_batch, CaptureOptions};
use acyclic_fs::{
    CancellationToken, CheckoutCommitOutcome, GenerationId, MountPublication, NativeWatch,
    NativeWatchOptions, OperationId, WatchBatch, WatchChange, WorkCounters,
};
use tokio::sync::{mpsc, oneshot};

use std::sync::Arc;

use crate::config::Config;
use crate::diff::{self, FileChange};
use crate::exclude::Exclusions;
use crate::fork::{ForkSeed, PromoteOutcome, SessionResolveOutcome, SharedLocalCheckout};
use crate::index::{Attribution, CheckpointKind, CheckpointRow, Index};
use crate::rewind::{self, RestoreOutcome, RewindOutcome};
use crate::store::Store;
use crate::{EngineError, Result};

const MAXIMUM_CAPTURE_PATHS: u32 = 4_000_000;
const MAXIMUM_EXTENT_SPANS: u32 = 65_536;
const WATCH_QUEUE: u32 = 65_536;
const POLL_CHANGES: u32 = 16_384;

/// Pipeline state reported by `status`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
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

/// Snapshot of pipeline health.
#[derive(Clone, Debug)]
pub struct StatusReport {
    pub state: State,
    pub last_checkpoint: Option<i64>,
    pub unpublished: u64,
    pub checkpoints_since_commit: u32,
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
    /// Restores one path from `target` into `root` (a copy fork's directory).
    RestorePathInto {
        target: GenerationId,
        root: PathBuf,
        path: PathBuf,
        reply: oneshot::Sender<Result<RestoreOutcome>>,
    },
    /// Copy-mode fork: write `generation` out to `destination`.
    Materialize {
        generation: GenerationId,
        destination: PathBuf,
        reply: oneshot::Sender<Result<()>>,
    },
    Promote {
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: String,
        reply: oneshot::Sender<Result<PromoteOutcome>>,
    },
    ResolveSession {
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: String,
        reply: oneshot::Sender<Result<SessionResolveOutcome>>,
    },
    ApplySession {
        generation: GenerationId,
        base: GenerationId,
        label: String,
        reply: oneshot::Sender<Result<PromoteOutcome>>,
    },
    Status {
        reply: oneshot::Sender<StatusReport>,
    },
    SetShadowed {
        active: bool,
        reply: oneshot::Sender<Result<()>>,
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
        request!(
            self,
            RecordGeneration {
                generation: generation,
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

    /// Restores one path from `target` into `root` rather than the working tree.
    pub async fn restore_path_into(
        &self,
        target: GenerationId,
        root: PathBuf,
        path: PathBuf,
    ) -> Result<RestoreOutcome> {
        request!(
            self,
            RestorePathInto {
                target: target,
                root: root,
                path: path
            }
        )?
    }

    /// Copy-mode fork: materializes `generation` into `destination`.
    pub async fn materialize(&self, generation: GenerationId, destination: PathBuf) -> Result<()> {
        request!(
            self,
            Materialize {
                generation: generation,
                destination: destination
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

    /// Safe Mode's commit half of promote: commits the overlay (or reports
    /// a conflict) without touching the real tree, so the caller can show
    /// an approval-gated diff before deciding whether to `apply_session`.
    pub async fn resolve_session(
        &self,
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: String,
    ) -> Result<SessionResolveOutcome> {
        request!(
            self,
            ResolveSession {
                shared: shared,
                base: base,
                label: label
            }
        )?
    }

    /// Safe Mode's swap half of promote: lands an already-`resolve_session`d
    /// generation onto the real tree.
    pub async fn apply_session(
        &self,
        generation: GenerationId,
        base: GenerationId,
        label: String,
    ) -> Result<PromoteOutcome> {
        request!(
            self,
            ApplySession {
                generation: generation,
                base: base,
                label: label
            }
        )?
    }

    pub async fn status(&self) -> Result<StatusReport> {
        request!(self, Status {})
    }

    /// Safe Mode: while a session's fork is shadow-mounted over the real repo
    /// root, mainline capture must pause — the pipeline's watcher would
    /// otherwise observe the fork's content and the mount/unmount lifecycle
    /// (which can map to the volume root) instead of real-tree mutations.
    /// `resolve_session`/`apply_session` clear it and rebuild the watcher.
    pub async fn set_shadowed(&self, active: bool) -> Result<()> {
        request!(self, SetShadowed { active: active })?
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
/// repo directory (Safe Mode shadowing it, or a fork session tearing down),
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
    watch: NativeWatch,
    options: CaptureOptions,
    /// Paths that never enter a checkpoint (`exclude` in the config).
    exclusions: Exclusions,
    cancel: CancellationToken,
    state: State,
    last_generation: GenerationId,
    last_checkpoint_row: Option<i64>,
    checkpoints_since_commit: u32,
    last_activity: Instant,
    /// A Safe Mode session's fork is shadow-mounted over the repo root:
    /// mainline capture is suspended until `resolve_session`/`apply_session`.
    shadowed: bool,
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

    loop {
        // Wake for whichever idle check is due sooner: the coarse authority
        // commit, or the auto-checkpoint safety net (disabled: `commit_idle_ms`
        // alone paces the loop, same as before this existed).
        let idle_ms = if pipeline.config.auto_checkpoint_idle_ms == 0 {
            pipeline.config.commit_idle_ms
        } else {
            pipeline
                .config
                .commit_idle_ms
                .min(pipeline.config.auto_checkpoint_idle_ms)
        };
        let idle = Duration::from_millis(idle_ms);
        let request = match tokio::time::timeout(idle, receiver.recv()).await {
            Ok(Some(request)) => request,
            Ok(None) => break, // all handles dropped
            Err(_) => {
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

fn fail_request(request: Request, message: &str) {
    let error = || EngineError::Store(message.to_string());
    match request {
        Request::Checkpoint { reply, .. } => drop(reply.send(Err(error()))),
        Request::Commit { reply } => drop(reply.send(Err(error()))),
        Request::Rewind { reply, .. } => drop(reply.send(Err(error()))),
        Request::Diff { reply, .. } => drop(reply.send(Err(error()))),
        Request::Fork { reply } => drop(reply.send(Err(error()))),
        Request::Promote { reply, .. } => drop(reply.send(Err(error()))),
        Request::Materialize { reply, .. } => drop(reply.send(Err(error()))),
        Request::ScratchCheckout { reply, .. } => drop(reply.send(Err(error()))),
        Request::PublishHead { reply } => drop(reply.send(Err(error()))),
        Request::MergePlan { reply, .. } => drop(reply.send(Err(error()))),
        Request::BuildGeneration { reply, .. } => drop(reply.send(Err(error()))),
        Request::ApplyToOverlay { reply, .. } => drop(reply.send(Err(error()))),
        Request::ReadFiles { reply, .. } => drop(reply.send(Err(error()))),
        Request::RestorePathInto { reply, .. } => drop(reply.send(Err(error()))),
        Request::MaterializePaths { reply, .. } => drop(reply.send(Err(error()))),
        Request::RecordGeneration { reply, .. } => drop(reply.send(Err(error()))),
        Request::SnapshotOverlay { reply, .. } => drop(reply.send(Err(error()))),
        Request::ResolveSession { reply, .. } => drop(reply.send(Err(error()))),
        Request::ApplySession { reply, .. } => drop(reply.send(Err(error()))),
        Request::RestorePath { reply, .. } => drop(reply.send(Err(error()))),
        Request::TurnStarted { reply, .. } => drop(reply.send(Err(error()))),
        Request::SetShadowed { reply, .. } => drop(reply.send(Err(error()))),
        Request::Status { reply } => drop(reply.send(StatusReport {
            state: State::Baselining,
            last_checkpoint: None,
            unpublished: 0,
            checkpoints_since_commit: 0,
        })),
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

        let mut watch = NativeWatch::open(
            &repo_root,
            NativeWatchOptions {
                limits: VolumeLimits::default(),
                maximum_queued_changes: WATCH_QUEUE,
                recursive: true,
            },
        )
        .map_err(EngineError::fs("open watcher"))?;
        watch
            .begin_rescan()
            .map_err(EngineError::fs("begin rescan"))?;

        let options = CaptureOptions {
            source_root: repo_root.clone(),
            expected_root_identity: capture_root_identity(&repo_root)
                .map_err(EngineError::fs("root identity"))?,
            maximum_paths: MAXIMUM_CAPTURE_PATHS,
            maximum_extent_spans: MAXIMUM_EXTENT_SPANS,
        };

        let exclusions = Exclusions::parse(&config.exclude)?;
        let mut pipeline = Self {
            store,
            index,
            config,
            watch,
            options,
            exclusions,
            cancel,
            state: State::Baselining,
            last_generation: GenerationId::new(acyclic_fs::Digest::ZERO),
            last_checkpoint_row: None,
            checkpoints_since_commit: 0,
            last_activity: Instant::now(),
            shadowed: false,
        };
        pipeline.baseline(CheckpointKind::Baseline).await?;
        Ok(pipeline)
    }

    /// Full baseline: capture the whole tree, checkpoint, finish the watcher
    /// rescan, publish. Used at startup and after RescanRequired/rewind.
    async fn baseline(&mut self, kind: CheckpointKind) -> Result<()> {
        crate::trace!(
            "pipeline",
            "baseline kind={kind:?}: full-tree rescan starting"
        );
        let baseline_started = Instant::now();
        self.state = State::Baselining;
        // A baseline requires a clean checkout. Mid-session (watcher
        // invalidation, rewind) the overlay holds uncommitted captures:
        // publish them first. At startup this is a no-op.
        self.commit_engine().await?;
        capture_baseline(
            &mut self.store.checkout,
            &self.options,
            WorkCounters::UNBOUNDED,
            &self.cancel,
        )
        .await
        .map_err(EngineError::fs("capture baseline"))?;
        self.scrub_exclusions().await?;
        let generation = self.checkpoint_engine().await?;
        let row = self
            .index
            .record(generation, kind, &Attribution::default())?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);

        // Changes that raced the baseline arrive as the rescan-completion
        // batch; fold them in before declaring Ready.
        let batch = self
            .watch
            .finish_rescan()
            .map_err(EngineError::fs("finish rescan"))?;
        // A root hint here is already covered by the rescan that just ran.
        let (batch, root) = strip_root_hints(batch);
        if root != RootHint::None {
            crate::trace!(
                "pipeline",
                "rescan tail: root hint ({root:?}) dropped, covered by the rescan"
            );
        }
        if let WatchBatch::Changes { ref changes, .. } = batch {
            if !changes.is_empty() {
                capture_watch_batch(
                    &mut self.store.checkout,
                    batch,
                    &self.options,
                    WorkCounters::UNBOUNDED,
                    &self.cancel,
                )
                .await
                .map_err(EngineError::fs("capture rescan tail"))?;
                self.scrub_exclusions().await?;
            }
        }
        self.commit_engine().await?;
        self.state = State::Ready;
        crate::trace!(
            "pipeline",
            "baseline done in {:.1}ms; state Ready",
            crate::trace::ms(baseline_started)
        );
        Ok(())
    }

    /// Handles one request; returns true when the pipeline should exit.
    async fn handle(&mut self, request: Request) -> bool {
        self.last_activity = Instant::now();
        match request {
            Request::Checkpoint {
                kind,
                attribution,
                reply,
            } => {
                crate::trace!(
                    "pipeline",
                    "request Checkpoint kind={kind:?} session={:?} tool={:?} admitted (queue depth unknown to the hook: ack was sent on send)",
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
                label,
                reply,
            } => {
                let result = self
                    .index
                    .record(
                        generation,
                        CheckpointKind::Manual,
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
            Request::RestorePathInto {
                target,
                root,
                path,
                reply,
            } => {
                let result =
                    Box::pin(rewind::restore_path_into(&self.store, target, &root, &path)).await;
                let _ = reply.send(result);
                false
            }
            Request::Materialize {
                generation,
                destination,
                reply,
            } => {
                let _ = reply
                    .send(rewind::materialize_into(&self.store, generation, &destination).await);
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
            Request::ResolveSession {
                shared,
                base,
                label,
                reply,
            } => {
                let _ = reply.send(self.resolve_session(shared, base, &label).await);
                false
            }
            Request::ApplySession {
                generation,
                base,
                label,
                reply,
            } => {
                let _ = reply.send(self.apply_session(generation, base, &label).await);
                false
            }
            Request::Status { reply } => {
                let _ = reply.send(StatusReport {
                    state: self.state,
                    last_checkpoint: self.last_checkpoint_row,
                    unpublished: self.index.unpublished_count().unwrap_or(0),
                    checkpoints_since_commit: self.checkpoints_since_commit,
                });
                false
            }
            Request::SetShadowed { active, reply } => {
                self.shadowed = active;
                let _ = reply.send(Ok(()));
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
        if self.shadowed {
            // A Safe Mode session's fork is mounted over the repo root; the
            // real tree is frozen and the watcher sees only fork/mount noise.
            // Record a noop so the hook gets a clean reply, but never drain
            // the watcher or snapshot the mainline mid-session.
            crate::trace!(
                "pipeline",
                "checkpoint kind={:?}: shadowed -> noop row, watcher untouched",
                kind
            );
            let row = self
                .index
                .record(self.last_generation, CheckpointKind::Noop, attribution)?;
            self.last_checkpoint_row = Some(row);
            return Ok(CheckpointOutcome {
                row_id: row,
                generation: self.last_generation,
                kind: CheckpointKind::Noop,
            });
        }
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
        let changed = self.drain_watcher().await?;
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
        self.checkpoints_since_commit += 1;
        let mut commit_ms = 0.0;
        if self.checkpoints_since_commit >= self.config.commit_every {
            let commit_started = Instant::now();
            self.commit_engine().await?;
            commit_ms = crate::trace::ms(commit_started);
        }
        crate::trace!(
            "pipeline",
            "checkpoint row #{row} kind={:?}: drain {drain_ms:.1}ms ({}), {} {capture_ms:.1}ms, index {record_ms:.1}ms, publish {commit_ms:.1}ms, total {:.1}ms",
            kind,
            if changed { "changes captured" } else { "no changes" },
            if changed { "snapshot" } else { "noop" },
            crate::trace::ms(started)
        );
        Ok(CheckpointOutcome {
            row_id: row,
            generation,
            kind,
        })
    }

    /// Polls the watcher until it stays quiet for `quiesce_ms` (capped at
    /// `quiesce_cap_ms`), capturing every non-empty batch. Returns whether
    /// anything was captured.
    async fn drain_watcher(&mut self) -> Result<bool> {
        let quiesce = Duration::from_millis(self.config.quiesce_ms);
        let cap = Duration::from_millis(self.config.quiesce_cap_ms);
        let started = Instant::now();
        let mut last_change = Instant::now();
        let mut changed = false;
        let mut polls = 0u32;
        let mut batches = 0u32;
        let mut hints = 0usize;
        loop {
            polls += 1;
            let batch = self
                .watch
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
                    capture_watch_batch(
                        &mut self.store.checkout,
                        batch,
                        &self.options,
                        WorkCounters::UNBOUNDED,
                        &self.cancel,
                    )
                    .await
                    .map_err(EngineError::fs("capture watch batch"))?;
                    if scrub {
                        self.scrub_exclusions().await?;
                    }
                    changed = true;
                    last_change = Instant::now();
                }
                WatchBatch::Changes { .. } => {
                    if last_change.elapsed() >= quiesce || started.elapsed() >= cap {
                        crate::trace!(
                            "pipeline",
                            "drain: done after {:.1}ms, {polls} polls, {batches} batch(es), {hints} hint(s); stopped by {}",
                            crate::trace::ms(started),
                            if started.elapsed() >= cap { "cap" } else { "quiesce window" }
                        );
                        return Ok(changed);
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                WatchBatch::RescanRequired { .. } => {
                    // Watcher overflow or invalidation: rebuild from scratch
                    // with a FRESH watcher. Reusing the invalidated one is a
                    // trap — if the baseline fails after begin_rescan, the
                    // old watcher stays wedged in RescanInProgress forever.
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
    /// has been quiet for `auto_checkpoint_idle_ms`. Every hook-driven host
    /// already drains the watcher on its own pre/post-tool checkpoints, so
    /// `drain_watcher` here almost always finds nothing and this is a noop —
    /// cheap (bounded by `quiesce_ms`), and unlike `checkpoint()` it never
    /// records a row when nothing changed, so it can run on every idle tick
    /// without spamming the timeline.
    async fn auto_checkpoint(&mut self) {
        if self.config.auto_checkpoint_idle_ms == 0 || self.shadowed || self.state != State::Ready {
            return;
        }
        if self.last_activity.elapsed() < Duration::from_millis(self.config.auto_checkpoint_idle_ms)
        {
            return;
        }
        // Like `idle_commit`, failures here are advisory: the next tick or
        // the next real request retries, and nothing is waiting on a reply.
        if let Err(error) = self.try_auto_checkpoint().await {
            crate::trace!("pipeline", "auto-checkpoint failed: {error}");
        }
    }

    async fn try_auto_checkpoint(&mut self) -> Result<()> {
        if !self.drain_watcher().await? {
            return Ok(());
        }
        let generation = self.checkpoint_engine().await?;
        let row = self
            .index
            .record(generation, CheckpointKind::Auto, &Attribution::default())?;
        crate::trace!(
            "pipeline",
            "auto-checkpoint row #{row}: idle timer, watcher had pending changes"
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

        let outcome = rewind::execute(
            &self.store,
            target.generation,
            self.config.trash_ttl_days,
            &self.exclusions,
        )
        .await;

        // The swap replaced the repo directory's inode: the pinned root
        // identity and the watcher both point at the old tree. Rebuild both,
        // then re-baseline.
        self.reset_watch().await?;
        self.baseline(CheckpointKind::Recovered).await?;
        outcome
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
        if self.shadowed {
            return Err(EngineError::Restore(
                "a Safe Mode session is shadowing the repo root; resolve it first".into(),
            ));
        }
        if self.exclusions.covers_host(path) {
            return Err(EngineError::Restore(format!(
                "{} is excluded from snapshots (`exclude` in {}); no checkpoint holds it",
                path.display(),
                crate::product::repo_config_file()
            )));
        }
        if self.state != State::Ready {
            self.reset_watch().await?;
            self.baseline(CheckpointKind::Recovered).await?;
        }
        if self.drain_watcher().await? {
            let safety = self.checkpoint_engine().await?;
            let row = self.index.record(
                safety,
                CheckpointKind::Manual,
                &Attribution {
                    label: Some(format!(
                        "before restore {} from #{}",
                        path.display(),
                        target.id
                    )),
                    ..Attribution::default()
                },
            )?;
            self.last_generation = safety;
            self.last_checkpoint_row = Some(row);
        }

        let outcome = rewind::restore_path(&self.store, target.generation, path).await?;

        // Let the watcher report the restore, then record it.
        self.drain_watcher().await?;
        let generation = self.checkpoint_engine().await?;
        let row = self.index.record(
            generation,
            CheckpointKind::Manual,
            &Attribution {
                label: Some(format!("restore {} from #{}", path.display(), target.id)),
                ..Attribution::default()
            },
        )?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);
        self.checkpoints_since_commit += 1;
        Ok(outcome)
    }

    /// Mints one fork: publish the current state (the fork base), then cut a
    /// fresh writable Head checkout whose overlay the daemon will mount.
    /// Same first half as `promote`: fold in pending changes, publish, and
    /// hand back the head so the caller can decide between a plain land
    /// (head == fork base) and a replay (head moved).
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
        self.commit_engine().await?;
        Ok(head)
    }

    async fn scratch_checkout(&mut self, base: GenerationId) -> Result<Arc<SharedLocalCheckout>> {
        let checkout = self
            .store
            .volume
            .checkout(
                acyclic_fs::model::GenerationSelector::Exact(base),
                crate::store::writable_head(),
                WorkCounters::UNBOUNDED,
                &self.cancel,
            )
            .await
            .map_err(EngineError::fs("scratch checkout"))?
            .value;
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
        self.commit_engine().await?;

        let checkout = self
            .store
            .volume
            .checkout(
                acyclic_fs::model::GenerationSelector::Head,
                crate::store::writable_head(),
                WorkCounters::UNBOUNDED,
                &self.cancel,
            )
            .await
            .map_err(EngineError::fs("fork checkout"))?
            .value;
        let config = checkout.volume_config();
        let volume_id = checkout.volume_id();
        Ok(ForkSeed {
            // Native close/flush must never publish: sibling forks share one
            // volume head, so a seal on close makes the next fork's mutations
            // Stale. Promote/resolve are the only commits.
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
                return Ok(PromoteOutcome::Conflict {
                    message: format!(
                        "the working tree moved past the fork's base \
                         ({}); promote in v1 requires an unmoved mainline — \
                         rewind to the base or re-fork and re-apply",
                        crate::generation_hex(base)
                    ),
                });
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
            .volume
            .checkout(
                acyclic_fs::model::GenerationSelector::Head,
                crate::store::writable_head(),
                WorkCounters::UNBOUNDED,
                &self.cancel,
            )
            .await
            .map_err(EngineError::fs("refresh checkout"))?
            .value;
        let row = self.index.record(
            generation,
            CheckpointKind::Manual,
            &Attribution {
                label: Some(label.to_string()),
                ..Attribution::default()
            },
        )?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);

        let swap = rewind::execute(
            &self.store,
            generation,
            self.config.trash_ttl_days,
            &self.exclusions,
        )
        .await;
        self.reset_watch().await?;
        self.baseline(CheckpointKind::Recovered).await?;
        let swap = swap?;
        Ok(PromoteOutcome::Promoted {
            generation,
            old_tree: Some(swap.old_tree),
        })
    }

    /// The commit half of promote, split out for Safe Mode: publishes the
    /// mainline safety net and commits the fork's overlay, but never
    /// touches the real tree — the caller diffs `base` against the
    /// returned generation and decides whether to `apply_session` it.
    async fn resolve_session(
        &mut self,
        shared: Arc<SharedLocalCheckout>,
        base: GenerationId,
        label: &str,
    ) -> Result<SessionResolveOutcome> {
        // The server unmounts the shadow before calling us, so the pipeline
        // watcher's queue is full of mount-teardown hints — some of which map
        // to the volume root and would fail capture outright. Leave shadow
        // mode and swap in a fresh watcher on the now-real tree to DISCARD
        // that queue. The pipeline's own checkout never moved during the
        // session (mainline checkpoints no-op while shadowed), so it still
        // sits at `base`; we must not re-baseline/publish it here or the
        // mainline HEAD would advance past the fork and the overlay commit
        // below would spuriously conflict.
        if self.shadowed {
            self.shadowed = false;
            self.reset_watch().await?;
            let _ = self
                .watch
                .finish_rescan()
                .map_err(EngineError::fs("finish rescan"))?;
            self.state = State::Ready;
        } else {
            // No shadow (direct callers, e.g. tests): the watcher is trusted,
            // so fold any pending real-tree change into the safety net.
            self.drain_watcher().await?;
        }
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
        // The safety checkpoint stays UNPUBLISHED (checkpoint, not commit):
        // publishing it would move the authority head off `base`, and Safe
        // Mode must leave the head at base until `apply_session` so that
        // `session-discard` truly changes nothing and the next session forks
        // cleanly. Unpublished checkpoint generations are fully restorable.
        let _ = base; // conflict against a moved mainline is detected at apply

        // Snapshot the fork's overlay as an unpublished generation: diffable
        // and rewind-applyable by id, without advancing the head.
        let generation = {
            let guard = shared.lock().await;
            if !guard.has_pending_mutations() {
                return Ok(SessionResolveOutcome::NoChanges);
            }
            guard
                .checkpoint(WorkCounters::UNBOUNDED, &self.cancel)
                .await
                .map_err(EngineError::fs("session checkpoint"))?
                .value
        };
        Ok(SessionResolveOutcome::Resolved { generation })
    }

    /// The swap half of promote, split out for Safe Mode: lands an
    /// already-committed generation (from `resolve_session`) onto the real
    /// tree, exactly like `promote`'s own tail.
    async fn apply_session(
        &mut self,
        generation: GenerationId,
        base: GenerationId,
        label: &str,
    ) -> Result<PromoteOutcome> {
        self.state = State::Rewinding;
        self.store.checkout = self
            .store
            .volume
            .checkout(
                acyclic_fs::model::GenerationSelector::Head,
                crate::store::writable_head(),
                WorkCounters::UNBOUNDED,
                &self.cancel,
            )
            .await
            .map_err(EngineError::fs("refresh checkout"))?
            .value;
        // Same v1 stance as promote: if the mainline published anything since
        // the fork's base, the head has moved off `base` and applying would
        // clobber it. Detected here rather than at resolve, because resolve
        // deliberately leaves the head at base (unpublished overlay).
        if self.store.checkout.generation_id() != base {
            self.state = State::Ready;
            return Ok(PromoteOutcome::Conflict {
                message: format!(
                    "the working tree moved past the session's base ({}); \
                     Safe Mode in v1 requires an unmoved mainline — rewind to \
                     the base or start a new session",
                    crate::generation_hex(base)
                ),
            });
        }
        let row = self.index.record(
            generation,
            CheckpointKind::Manual,
            &Attribution {
                label: Some(label.to_string()),
                ..Attribution::default()
            },
        )?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);

        let swap = rewind::execute(
            &self.store,
            generation,
            self.config.trash_ttl_days,
            &self.exclusions,
        )
        .await;
        self.reset_watch().await?;
        self.baseline(CheckpointKind::Recovered).await?;
        let swap = swap?;
        Ok(PromoteOutcome::Promoted {
            generation,
            old_tree: Some(swap.old_tree),
        })
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
        self.watch = NativeWatch::open(
            &repo_root,
            NativeWatchOptions {
                limits: VolumeLimits::default(),
                maximum_queued_changes: WATCH_QUEUE,
                recursive: true,
            },
        )
        .map_err(EngineError::fs("reopen watcher"))?;
        self.watch
            .begin_rescan()
            .map_err(EngineError::fs("begin rescan"))?;
        Ok(())
    }
}

#[cfg(test)]
mod root_hint_tests {
    use super::*;
    use acyclic_fs::kernel::{LogicalName, NameEncoding, NamespacePath};
    use acyclic_fs::{WatchEpoch, WatchSequence};

    fn root() -> NamespacePath {
        NamespacePath::new(Vec::new(), VolumeLimits::default()).unwrap()
    }

    fn file(name: &str) -> NamespacePath {
        let limits = VolumeLimits::default();
        let name = LogicalName::new(
            NameEncoding::PosixBytes,
            name.as_bytes().to_vec(),
            limits.maximum_component_bytes,
        )
        .unwrap();
        NamespacePath::new(vec![name], limits).unwrap()
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
