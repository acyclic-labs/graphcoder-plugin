//! The capture pipeline: one thread owns the checkout, watcher, and index.
//!
//! Requests are processed FIFO. Per-tool-call snapshots use `checkpoint()`
//! (fast, no authority publish); `commit()` runs at coarse boundaries only.
//! The pipeline runs on its own thread with a current-thread tokio runtime so
//! the `&mut Checkout` discipline never meets Send bounds.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use acyclic_fs::{
    CancellationToken, CheckoutCommitOutcome, GenerationId, NativeWatch, NativeWatchOptions,
    OperationId, WatchBatch, WorkCounters,
};
use acyclic_fs::model::VolumeLimits;
use acyclic_fs_mount::{capture_baseline, capture_root_identity, capture_watch_batch, CaptureOptions};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::diff::{self, FileChange};
use crate::index::{Attribution, CheckpointKind, CheckpointRow, Index};
use crate::rewind::{self, RewindOutcome};
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
    Diff {
        before: GenerationId,
        after: GenerationId,
        reply: oneshot::Sender<Result<Vec<FileChange>>>,
    },
    Status {
        reply: oneshot::Sender<StatusReport>,
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
        request!(self, Checkpoint { kind: kind, attribution: attribution })?
    }

    pub async fn commit(&self) -> Result<()> {
        request!(self, Commit {})?
    }

    pub async fn rewind(&self, target: CheckpointRow) -> Result<RewindOutcome> {
        request!(self, Rewind { target: target })?
    }

    pub async fn diff(
        &self,
        before: GenerationId,
        after: GenerationId,
    ) -> Result<Vec<FileChange>> {
        request!(self, Diff { before: before, after: after })?
    }

    pub async fn status(&self) -> Result<StatusReport> {
        request!(self, Status {})
    }

    pub async fn session_started(&self, session_id: String, host: String) -> Result<()> {
        request!(self, SessionStarted { session_id: session_id, host: host })?
    }

    pub async fn session_ended(&self, session_id: String) -> Result<()> {
        request!(self, SessionEnded { session_id: session_id })?
    }

    pub async fn shutdown(&self) -> Result<()> {
        request!(self, Shutdown {})?
    }
}

/// Spawns the pipeline thread. The returned join handle resolves when the
/// pipeline has shut down (after a `shutdown` request or channel closure).
pub fn spawn(
    store: Store,
    index: Index,
    config: Config,
) -> (PipelineHandle, std::thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(1024);
    let thread = std::thread::Builder::new()
        .name("acyclic-pipeline".into())
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
    cancel: CancellationToken,
    state: State,
    last_generation: GenerationId,
    last_checkpoint_row: Option<i64>,
    checkpoints_since_commit: u32,
    last_activity: Instant,
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
        let idle = Duration::from_millis(pipeline.config.commit_idle_ms);
        let request = match tokio::time::timeout(idle, receiver.recv()).await {
            Ok(Some(request)) => request,
            Ok(None) => break, // all handles dropped
            Err(_) => {
                pipeline.idle_commit().await;
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
        watch.begin_rescan().map_err(EngineError::fs("begin rescan"))?;

        let options = CaptureOptions {
            source_root: repo_root.clone(),
            expected_root_identity: capture_root_identity(&repo_root)
                .map_err(EngineError::fs("root identity"))?,
            maximum_paths: MAXIMUM_CAPTURE_PATHS,
            maximum_extent_spans: MAXIMUM_EXTENT_SPANS,
        };

        let mut pipeline = Self {
            store,
            index,
            config,
            watch,
            options,
            cancel,
            state: State::Baselining,
            last_generation: GenerationId::new(acyclic_fs::Digest::ZERO),
            last_checkpoint_row: None,
            checkpoints_since_commit: 0,
            last_activity: Instant::now(),
        };
        pipeline.baseline(CheckpointKind::Baseline).await?;
        Ok(pipeline)
    }

    /// Full baseline: capture the whole tree, checkpoint, finish the watcher
    /// rescan, publish. Used at startup and after RescanRequired/rewind.
    async fn baseline(&mut self, kind: CheckpointKind) -> Result<()> {
        self.state = State::Baselining;
        capture_baseline(
            &mut self.store.checkout,
            &self.options,
            WorkCounters::UNBOUNDED,
            &self.cancel,
        )
        .await
        .map_err(EngineError::fs("capture baseline"))?;
        let generation = self.checkpoint_engine().await?;
        let row = self.index.record(generation, kind, &Attribution::default())?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);

        // Changes that raced the baseline arrive as the rescan-completion
        // batch; fold them in before declaring Ready.
        let batch = self
            .watch
            .finish_rescan()
            .map_err(EngineError::fs("finish rescan"))?;
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
            }
        }
        self.commit_engine().await?;
        self.state = State::Ready;
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
            Request::Diff {
                before,
                after,
                reply,
            } => {
                let _ = reply.send(diff::diff(&self.store, before, after).await);
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
        if self.state != State::Ready {
            return Err(EngineError::Capture(format!(
                "pipeline not ready ({:?})",
                self.state
            )));
        }
        let changed = self.drain_watcher().await?;
        let (generation, kind) = if changed {
            (self.checkpoint_engine().await?, kind)
        } else {
            (self.last_generation, CheckpointKind::Noop)
        };
        let row = self.index.record(generation, kind, attribution)?;
        self.last_generation = generation;
        self.last_checkpoint_row = Some(row);
        self.checkpoints_since_commit += 1;
        if self.checkpoints_since_commit >= self.config.commit_every {
            self.commit_engine().await?;
        }
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
        loop {
            let batch = self
                .watch
                .poll(POLL_CHANGES, WorkCounters::UNBOUNDED, &self.cancel)
                .map_err(EngineError::fs("watch poll"))?
                .value;
            match batch {
                WatchBatch::Changes { ref changes, .. } if !changes.is_empty() => {
                    capture_watch_batch(
                        &mut self.store.checkout,
                        batch,
                        &self.options,
                        WorkCounters::UNBOUNDED,
                        &self.cancel,
                    )
                    .await
                    .map_err(EngineError::fs("capture watch batch"))?;
                    changed = true;
                    last_change = Instant::now();
                }
                WatchBatch::Changes { .. } => {
                    if last_change.elapsed() >= quiesce || started.elapsed() >= cap {
                        return Ok(changed);
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                WatchBatch::RescanRequired { .. } => {
                    // Watcher overflow or invalidation: rebuild from scratch.
                    self.watch
                        .begin_rescan()
                        .map_err(EngineError::fs("begin rescan"))?;
                    self.baseline(CheckpointKind::Recovered).await?;
                    return Ok(true);
                }
            }
        }
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
            Err(failure) => {
                let text = format!("{failure:?}");
                if text.contains("NoPendingMutations") {
                    // Nothing new to publish means every recorded row's
                    // generation is already covered by the last publish —
                    // noop and quiet pre_rewind rows included.
                    if let Some(row) = self.last_checkpoint_row {
                        self.index.mark_published(row)?;
                    }
                    self.checkpoints_since_commit = 0;
                    return Ok(());
                }
                Err(EngineError::Fs(format!("commit: {text}")))
            }
        }
    }

    async fn idle_commit(&mut self) {
        if self.state == State::Ready
            && self.checkpoints_since_commit > 0
            && self.last_activity.elapsed()
                >= Duration::from_millis(self.config.commit_idle_ms)
        {
            let _ = self.commit_engine().await;
        }
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
                ..Attribution::default()
            },
        )?;
        self.last_generation = safety;
        self.last_checkpoint_row = Some(safety_row);
        self.commit_engine().await?;

        let outcome = rewind::execute(&self.store, target.generation, self.config.trash_ttl_days)
            .await;

        // The swap replaced the repo directory's inode: the pinned root
        // identity and the watcher both point at the old tree. Rebuild both,
        // then re-baseline.
        self.reset_watch().await?;
        self.baseline(CheckpointKind::Recovered).await?;
        outcome
    }

    /// Reopens the watcher and recomputes the capture root identity — needed
    /// whenever the repo directory inode may have changed (after a rewind).
    async fn reset_watch(&mut self) -> Result<()> {
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
