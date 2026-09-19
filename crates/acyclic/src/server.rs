//! The per-repo daemon: owns the pipeline, serves the unix socket.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ipc;
use crate::proto;
use acyclic::config::Config;
use acyclic::fork::{self, MountCapability, SharedLocalCheckout};
use acyclic::guard::GuardedMountFilesystem;
use acyclic::index::{Attribution, CheckpointKind, CheckpointRow, Index};
use acyclic::merge::{self, Entry};
use acyclic::pipeline::{self, PipelineHandle};
use acyclic::product::NAME;
use acyclic::spec::SpeculateConfig;
use acyclic::store::{Store, StorePaths};
use acyclic::{rewind, EngineError};
use acyclic_fs::model::VolumeConfig;
use acyclic_fs::{
    mount_native, CheckoutMountSource, MountFilesystem, NativeMountRequest, NativeMountSession,
    RoutedMountSource,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify};

/// One live fork: the shared checkout its route serves plus wire facts.
/// Forks are routes inside ONE native mount session (see [`ForkMount`]):
/// FUSE-T serves a single session per burst reliably, and one session is
/// all the routed design ever needs.
struct ForkState {
    shared: Arc<SharedLocalCheckout>,
    /// The generation promote is judged against. Starts at the fork's cut
    /// point; a conflicting promote rebases the fork and moves it to the
    /// head it was rebased onto.
    base: acyclic::GenerationId,
    entry: proto::ForkEntry,
    /// Set by a conflicting promote until the markers are gone.
    conflict: Option<OpenConflict>,
}

/// A conflict a promote wrote into a fork workspace (graphcoder's
/// `FS_CONFLICT_OPENED` payload: base, ours, theirs, paths).
#[derive(Clone, Debug)]
struct OpenConflict {
    paths: Vec<PathBuf>,
}

/// The one native session projecting every fork through the router.
/// Mounted lazily on the first fork, unmounted when the last route goes.
struct ForkMount {
    router: Arc<RoutedMountSource>,
    session: Option<NativeMountSession>,
}

/// Accepts connections until a stop, a signal, or the idle-exit clock.
async fn serve_until_done(
    server: Server,
    mut listener: ipc::Listener,
    shutdown: Arc<Notify>,
    handle: PipelineHandle,
) {
    let idle_exit = Duration::from_millis(server.config.daemon_idle_exit_ms);
    let mut idle_tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok(stream) = accepted else { continue };
                let server = server.clone();
                tokio::spawn(async move { server.serve(stream).await });
            }
            _ = idle_tick.tick() => {
                if !idle_exit.is_zero() && server.idle_for(idle_exit).await {
                    eprintln!(
                        "{NAME} daemon: idle for {}s with no session or fork; exiting \
                         (the next session start brings it back)",
                        idle_exit.as_secs()
                    );
                    break;
                }
            }
            _ = shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = handle.shutdown().await;
}

/// Binds the daemon socket and writes the pidfile. A stale socket from a
/// dead daemon is removed; a live one refuses the second daemon via bind
/// failure after the removal race.
fn bind_socket(
    runtime: &tokio::runtime::Runtime,
    paths: &StorePaths,
) -> Result<ipc::Listener, String> {
    ipc::cleanup(&paths.socket());
    let listener = runtime
        .block_on(async { ipc::Listener::bind(&paths.socket()) })
        .map_err(|error| format!("bind {}: {error}", ipc::endpoint_display(&paths.socket())))?;
    std::fs::write(paths.pidfile(), std::process::id().to_string())
        .map_err(|error| error.to_string())?;
    Ok(listener)
}

/// Speculation is opt-in, per developer, from a file of its own; a
/// malformed one disables it and says so rather than failing the daemon.
fn spawn_speculation(
    paths: &StorePaths,
    handle: &pipeline::PipelineHandle,
) -> (
    Option<Arc<crate::speculate::SpecHandle>>,
    Option<std::thread::JoinHandle<()>>,
) {
    let (speculate_config, speculate_warning) = SpeculateConfig::load();
    if let Some(warning) = speculate_warning {
        eprintln!("{NAME} daemon: speculation config: {warning}");
    }
    let speculation = crate::speculate::spawn(
        speculate_config,
        crate::speculate::SpecDeps {
            index_db: paths.index_db(),
            spec_db: paths.spec_db(),
            spec_runs: paths.spec_runs(),
            handle: handle.clone(),
        },
    );
    let (spec, spec_thread) = match speculation {
        Some((handle, thread)) => (Some(Arc::new(handle)), Some(thread)),
        None => (None, None),
    };
    if let Some(spec) = spec.as_ref() {
        eprintln!(
            "{NAME} daemon: speculation on ({})",
            if spec.config().spends_tokens() {
                "precompute + model runs"
            } else {
                "precompute only, no model runs"
            }
        );
    }
    (spec, spec_thread)
}

pub fn run(repo_root: &Path) -> Result<(), String> {
    let startup = std::time::Instant::now();
    let mut phase = std::time::Instant::now();
    let mut lap = |name: &str| {
        acyclic::trace!(
            "daemon",
            "startup: {name} {:.1}ms (t+{:.1}ms)",
            acyclic::trace::ms(phase),
            acyclic::trace::ms(startup)
        );
        phase = std::time::Instant::now();
    };
    fork::reap_legacy_shadow(repo_root);
    let repo_root =
        rewind::recover_before_repo_open(repo_root).map_err(|error| error.to_string())?;
    lap("rewind recovery before repo open");
    let config = Config::load(&repo_root).map_err(|error| error.to_string())?;
    lap("config load");
    let stores_root = config.store_dir.as_ref().map(PathBuf::from);
    let paths = StorePaths::for_repo(&repo_root, stores_root.as_deref())
        .map_err(|error| error.to_string())?;

    // Finish or unwind any rewind that a crash interrupted BEFORE the store
    // opens and the pipeline baselines; sweep fork dirs a dead daemon left
    // mounted (fork sessions do not survive the daemon).
    rewind::recover(
        &paths.rewind_journal(),
        &paths.trash(),
        config.trash_ttl_days,
    )
    .map_err(|error| error.to_string())?;
    lap("rewind journal recovery");
    fork::sweep_stale_forks(&repo_root);
    lap("sweep stale forks");
    // Same reason as the fork sweep, and the same moment: a model run a
    // crashed daemon left behind is still running, and still billing.
    crate::spec_runner::sweep_stale_runs(&paths.spec_runs());
    lap("sweep stale spec runs");

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    // Socket + pidfile FIRST, before the store opens: a client can then
    // connect at once and decide how long to wait for readiness (the
    // session-start hook waits 300ms), instead of polling a missing socket
    // through an O(history) store open. A stale socket from a dead daemon
    // is removed; a live one refuses the second daemon via bind failure.
    let listener = bind_socket(&runtime, &paths)?;
    lap("socket bind + pidfile");
    let store = runtime
        .block_on(Store::open(&repo_root, paths.clone()))
        .map_err(|error| error.to_string())?;
    let repo_root = store.repo_root.clone();
    lap("store open");
    let index = Index::open(&paths.index_db()).map_err(|error| error.to_string())?;
    lap("index open");
    let (handle, pipeline_thread) = pipeline::spawn(store, index, config.clone());
    lap("pipeline thread spawn (baseline runs on it)");

    let shutdown = Arc::new(Notify::new());
    let mounts = fork::mount_capability();
    lap("native mount probe");
    if !mounts.available {
        eprintln!(
            "{NAME} daemon: mounts unavailable ({}): forks are disabled",
            mounts.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    // Speculation is opt-in, per developer, from a file of its own; a
    // malformed one disables it and says so rather than failing the daemon.
    let (spec, spec_thread) = spawn_speculation(&paths, &handle);
    lap("speculation spawn");

    let server = Server {
        mounts,
        handle: handle.clone(),
        index_db: paths.index_db(),
        store_root: paths.root.clone(),
        repo_root,
        config,
        shutdown: shutdown.clone(),
        forks: Arc::new(Mutex::new(HashMap::new())),
        fork_mount: Arc::new(Mutex::new(ForkMount {
            router: Arc::new(RoutedMountSource::new()),
            session: None,
        })),
        spec,
        live_sessions: Arc::new(Mutex::new(HashSet::new())),
        last_activity: Arc::new(Mutex::new(Instant::now())),
        store_bytes: Arc::new(Mutex::new(None)),
    };

    runtime.block_on(serve_until_done(server, listener, shutdown, handle));

    let _ = pipeline_thread.join();
    if let Some(thread) = spec_thread {
        let _ = thread.join();
    }
    ipc::cleanup(&paths.socket());
    let _ = std::fs::remove_file(paths.pidfile());
    Ok(())
}

#[derive(Clone)]
struct Server {
    /// Probed once at start to decide whether forks mount or materialize.
    mounts: MountCapability,
    handle: PipelineHandle,
    index_db: PathBuf,
    store_root: PathBuf,
    repo_root: PathBuf,
    config: Config,
    shutdown: Arc<Notify>,
    forks: Arc<Mutex<HashMap<String, ForkState>>>,
    fork_mount: Arc<Mutex<ForkMount>>,
    /// The speculation scheduler, when it is enabled. `None` makes every
    /// call site a no-op, so the default path costs nothing.
    spec: Option<Arc<crate::speculate::SpecHandle>>,
    /// Sessions that started and have not ended: the daemon does not idle
    /// out while one is open.
    live_sessions: Arc<Mutex<HashSet<String>>>,
    /// When the last request arrived; the idle-exit clock.
    last_activity: Arc<Mutex<Instant>>,
    /// `store size` for `status`, refreshed in the background: walking the
    /// object directory costs ~1s on an aged store.
    store_bytes: Arc<Mutex<Option<(Instant, u64)>>>,
}

impl Server {
    async fn serve(&self, stream: ipc::ServerStream) {
        let (read, mut write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let response = match serde_json::from_str::<proto::Request>(&line) {
                Ok(request) if request.v == proto::PROTOCOL_VERSION => proto::Response {
                    id: request.id,
                    payload: self.dispatch(request.op).await,
                },
                Ok(request) => proto::Response {
                    id: request.id,
                    payload: err(format!("unsupported protocol version {}", request.v)),
                },
                Err(error) => proto::Response {
                    id: 0,
                    payload: err(format!("bad request: {error}")),
                },
            };
            let Ok(mut encoded) = serde_json::to_vec(&response) else {
                break;
            };
            encoded.push(b'\n');
            if write.write_all(&encoded).await.is_err() {
                break;
            }
        }
    }

    async fn dispatch(&self, op: proto::Op) -> proto::Payload {
        let name = op_name(&op);
        let started = std::time::Instant::now();
        acyclic::trace!("daemon", "op {name} received");
        let payload = match self.dispatch_inner(op).await {
            Ok(reply) => {
                acyclic::trace!(
                    "daemon",
                    "op {name} -> {} in {:.1}ms",
                    reply_name(&reply),
                    acyclic::trace::ms(started)
                );
                proto::Payload::Ok(Box::new(reply))
            }
            Err(message) => {
                acyclic::trace!(
                    "daemon",
                    "op {name} -> error in {:.1}ms: {}",
                    acyclic::trace::ms(started),
                    message.lines().next().unwrap_or("")
                );
                err(message)
            }
        };
        payload
    }

    /// True when nothing has needed this daemon for `idle`: no request, no
    /// open session, and no live fork.
    async fn idle_for(&self, idle: Duration) -> bool {
        self.last_activity.lock().await.elapsed() >= idle
            && self.live_sessions.lock().await.is_empty()
            && self.forks.lock().await.is_empty()
    }

    /// The store's size on disk, from a cache that a background walk
    /// refreshes once it is a minute old. Only the very first call walks.
    async fn store_size(&self) -> u64 {
        const FRESH: Duration = Duration::from_secs(60);
        let root = self.store_root.join("store");
        let cached = *self.store_bytes.lock().await;
        match cached {
            Some((at, bytes)) if at.elapsed() < FRESH => bytes,
            Some((_, bytes)) => {
                let cache = Arc::clone(&self.store_bytes);
                tokio::task::spawn_blocking(move || {
                    let fresh = directory_bytes(&root);
                    if let Ok(mut slot) = cache.try_lock() {
                        *slot = Some((Instant::now(), fresh));
                    }
                });
                bytes
            }
            None => {
                let bytes = tokio::task::block_in_place(|| directory_bytes(&root));
                *self.store_bytes.lock().await = Some((Instant::now(), bytes));
                bytes
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "one arm per protocol Op; the wire-to-engine table reads best whole"
    )]
    async fn dispatch_inner(&self, op: proto::Op) -> Result<proto::Reply, String> {
        *self.last_activity.lock().await = Instant::now();
        match op {
            proto::Op::Ping => {
                self.handle.status().await.map_err(stringify)?;
                Ok(proto::Reply::Pong)
            }
            proto::Op::Status => {
                let status = self.handle.status().await.map_err(stringify)?;
                Ok(proto::Reply::Status(proto::StatusInfo {
                    state: format!("{:?}", status.state).to_lowercase(),
                    last_checkpoint: status.last_checkpoint,
                    unpublished: status.unpublished,
                    store_bytes: self.store_size().await,
                    repo_root: self.repo_root.display().to_string(),
                    mount_provider: self.mounts.provider.to_owned(),
                    mount_available: self.mounts.available,
                    mount_reason: self.mounts.reason.clone(),
                    speculate: self.speculation_status(),
                    watcher: Some(proto::WatcherStatus {
                        invalidations: status.watcher.invalidations,
                        last_reason: status.watcher.last_reason,
                        recovery_rescans: status.watcher.recovery_rescans,
                        recovery_ms_total: status.watcher.recovery_ms_total,
                        last_recovery_ms: status.watcher.last_recovery_ms,
                    }),
                }))
            }
            proto::Op::Checkpoint {
                kind,
                session_id,
                tool_call_id,
                tool_name,
                label,
                wait,
                durable,
            } => {
                let kind = engine_kind(kind);
                let attribution = Attribution {
                    session_id,
                    tool_call_id,
                    tool_name,
                    label,
                    turn: None,
                    rewind_target: None,
                };
                if wait {
                    acyclic::trace!(
                        "daemon",
                        "checkpoint: WAIT path (reply after the capture lands; durable={durable})"
                    );
                    let outcome = self
                        .handle
                        .checkpoint(kind, attribution)
                        .await
                        .map_err(stringify)?;
                    if durable {
                        self.handle.commit().await.map_err(stringify)?;
                    }
                    Ok(proto::Reply::Checkpoint(proto::CheckpointInfo {
                        row_id: outcome.row_id,
                        generation: hex_generation(outcome.generation),
                        kind: outcome.kind,
                    }))
                } else {
                    // Enqueue-ack: the hook path. Admission into the FIFO
                    // happens BEFORE the ack, so a stop arriving after the
                    // ack queues behind the capture instead of dropping it.
                    // Failures land in the index as `failed`.
                    acyclic::trace!(
                        "daemon",
                        "checkpoint: ENQUEUE path (ack on admission, capture runs behind)"
                    );
                    self.handle
                        .checkpoint_enqueued(kind, attribution)
                        .await
                        .map_err(stringify)?;
                    if durable {
                        let handle = self.handle.clone();
                        tokio::spawn(async move {
                            let _ = handle.commit().await;
                        });
                    }
                    Ok(proto::Reply::Enqueued)
                }
            }
            proto::Op::Timeline {
                session_id,
                turn,
                limit,
            } => {
                if turn.is_some() && session_id.is_none() {
                    return Err("--turn needs --session (turn numbers are per session)".into());
                }
                let index = self.open_index()?;
                let rows = index
                    .list(session_id.as_deref(), turn, limit)
                    .map_err(stringify)?;
                Ok(proto::Reply::Timeline(
                    rows.into_iter().map(timeline_entry).collect(),
                ))
            }
            proto::Op::Turns { session_id } => {
                let index = self.open_index()?;
                let turns = index.turns(session_id.as_deref()).map_err(stringify)?;
                let mut entries = Vec::with_capacity(turns.len());
                for turn in turns {
                    let base_checkpoint = match turn.first_checkpoint {
                        Some(first) => index
                            .latest_target_before(first)
                            .map_err(stringify)?
                            .map(|row| row.id),
                        None => None,
                    };
                    entries.push(proto::TurnEntry {
                        session_id: turn.session_id,
                        turn: turn.turn,
                        started_at: turn.started_at,
                        prompt: turn.prompt,
                        first_checkpoint: turn.first_checkpoint,
                        last_checkpoint: turn.last_checkpoint,
                        checkpoints: turn.checkpoints,
                        base_checkpoint,
                    });
                }
                Ok(proto::Reply::Turns(entries))
            }
            proto::Op::TurnStart { session_id, prompt } => {
                let turn = self
                    .handle
                    .turn_started(session_id.clone(), prompt)
                    .await
                    .map_err(stringify)?;
                if let Some(spec) = self.spec.as_ref() {
                    spec.notify(crate::speculate::SpecEvent::TurnStarted {
                        session_id: session_id.clone(),
                        turn,
                    });
                }
                Ok(proto::Reply::Turn(proto::TurnInfo { session_id, turn }))
            }
            proto::Op::Inspect { checkpoint } => {
                let index = self.open_index()?;
                let row = index
                    .by_id(checkpoint)
                    .map_err(stringify)?
                    .ok_or(format!("no checkpoint #{checkpoint}"))?;
                let (host, prompt) = match (&row.session_id, row.turn) {
                    (Some(session), turn) => {
                        let host = index
                            .session(session)
                            .map_err(stringify)?
                            .and_then(|session| session.host);
                        let prompt = match turn {
                            Some(turn) => index
                                .turn(session, turn)
                                .map_err(stringify)?
                                .map(|turn| turn.prompt),
                            None => None,
                        };
                        (host, prompt)
                    }
                    (None, _) => (None, None),
                };
                Ok(proto::Reply::Inspect(proto::InspectInfo {
                    id: row.id,
                    generation: hex_generation(row.generation),
                    created_at: row.created_at,
                    kind: row.kind,
                    published: row.published,
                    session_id: row.session_id,
                    host,
                    turn: row.turn,
                    prompt,
                    tool_name: row.tool_name,
                    tool_call_id: row.tool_call_id,
                    label: row.label,
                    error: row.error,
                    rewind_target: row.rewind_target,
                }))
            }
            proto::Op::Sessions { limit } => {
                let index = self.open_index()?;
                let sessions = index.sessions(limit).map_err(stringify)?;
                let mut entries = Vec::with_capacity(sessions.len());
                for session in sessions {
                    let end_checkpoint = index
                        .session_end(&session.session_id)
                        .map_err(stringify)?
                        .map(|row| row.id);
                    entries.push(proto::SessionEntry {
                        session_id: session.session_id,
                        host: session.host,
                        started_at: session.started_at,
                        ended_at: session.ended_at,
                        checkpoints: session.checkpoints,
                        turns: session.turns,
                        end_checkpoint,
                    });
                }
                Ok(proto::Reply::Sessions(entries))
            }
            proto::Op::Summary {
                session_id,
                turn,
                wait_ms,
            } => {
                let info = self.summary(session_id, turn, wait_ms).await?;
                Ok(proto::Reply::Summary(info))
            }
            proto::Op::Brief { current } => {
                let brief = self.brief(current.as_deref()).await?;
                Ok(proto::Reply::Brief(brief))
            }
            proto::Op::Rewind {
                target,
                path: Some(path),
            } => {
                self.invalidate(&crate::speculate::Cause::TreeMoved);
                let row = self.resolve_target(target)?;
                let row_id = row.id;
                let outcome = self
                    .handle
                    .restore_path(row, PathBuf::from(&path))
                    .await
                    .map_err(stringify)?;
                let recorded_checkpoint = self
                    .handle
                    .status()
                    .await
                    .map_err(stringify)?
                    .last_checkpoint;
                Ok(proto::Reply::Restore(proto::RestoreInfo {
                    checkpoint: row_id,
                    path: outcome.path.display().to_string(),
                    action: outcome.action,
                    recorded_checkpoint,
                }))
            }
            proto::Op::Rewind { target, path: None } => {
                self.invalidate(&crate::speculate::Cause::TreeMoved);
                let row = self.resolve_target(target)?;
                let row_id = row.id;
                let outcome = self.handle.rewind(row).await.map_err(stringify)?;
                Ok(proto::Reply::Rewind(proto::RewindInfo {
                    restored_checkpoint: row_id,
                    old_tree: outcome.old_tree.display().to_string(),
                    warning: outcome.warning.to_owned(),
                }))
            }
            proto::Op::Diff {
                before,
                after,
                before_hex,
                after_hex,
            } => {
                // Resolve both rows before the first await: the SQLite
                // handle is not Sync and must not live across it.
                let (before_row, after_row) = {
                    let index = self.open_index()?;
                    let resolve = |id: Option<i64>,
                                   hex: Option<String>|
                     -> Result<Option<CheckpointRow>, String> {
                        match (id, hex) {
                            (Some(id), _) => Ok(Some(
                                index
                                    .by_id(id)
                                    .map_err(stringify)?
                                    .ok_or(format!("no checkpoint #{id}"))?,
                            )),
                            (None, Some(hex)) => Ok(Some(
                                index
                                    .by_generation_prefix(&hex)
                                    .map_err(stringify)?
                                    .ok_or(format!(
                                        "no checkpoint has a generation starting {hex}; `{NAME} timeline` lists row ids"
                                    ))?,
                            )),
                            (None, None) => Ok(None),
                        }
                    };
                    let before_row = match resolve(before, before_hex)? {
                        Some(row) => row,
                        None => self.default_diff_base(&index)?,
                    };
                    let after_row = match resolve(after, after_hex)? {
                        Some(row) => row,
                        None => index
                            .latest()
                            .map_err(stringify)?
                            .ok_or("no checkpoints yet")?,
                    };
                    (before_row, after_row)
                };
                let changes = self
                    .handle
                    .diff(before_row.generation, after_row.generation)
                    .await
                    .map_err(stringify)?;
                Ok(proto::Reply::Diff(self.annotate_ignored(
                    changes.into_iter().map(diff_entry).collect(),
                )))
            }
            proto::Op::SessionStart { session_id, host } => {
                self.live_sessions.lock().await.insert(session_id.clone());
                self.handle
                    .session_started(session_id.clone(), host)
                    .await
                    .map_err(stringify)?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::SessionEnd { session_id } => {
                self.live_sessions.lock().await.remove(&session_id);
                self.handle
                    .session_ended(session_id.clone())
                    .await
                    .map_err(stringify)?;
                // Scratch trees are tagged with their owning session and
                // never meant to be promoted: best-effort drop, log rather
                // than fail session-end over a leaked mount.
                let scratch_ids: Vec<String> = self
                    .forks
                    .lock()
                    .await
                    .iter()
                    .filter(|(_, fork)| {
                        fork.entry.session_id.as_deref() == Some(session_id.as_str())
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in scratch_ids {
                    self.forks.lock().await.remove(&id);
                    if let Err(error) = self.detach_route(&id).await {
                        eprintln!("{NAME} daemon: drop scratch fork {id}: {error}");
                    }
                }
                // The session that just ended is the one the NEXT session's
                // brief will describe, and nothing is asking for it yet:
                // the ideal moment to compute it.
                if let Some(spec) = self.spec.as_ref() {
                    // Nothing left to summarise for a session that is over.
                    spec.notify(crate::speculate::SpecEvent::Invalidate(
                        crate::speculate::Cause::Session(session_id.clone()),
                    ));
                    spec.notify(crate::speculate::SpecEvent::SessionEnded);
                }
                Ok(proto::Reply::Unit)
            }
            proto::Op::Commit => {
                self.handle.commit().await.map_err(stringify)?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::Stop => {
                // Stop speculating before anything else is torn down: a
                // speculation in flight is holding the pipeline handle this
                // shutdown is about to invalidate.
                if let Some(spec) = self.spec.as_ref() {
                    spec.shutdown().await;
                }
                // Detach the fork session before the pipeline goes away: its
                // callback runtimes reach into the shared checkouts.
                self.forks.lock().await.clear();
                let mut mount = self.fork_mount.lock().await;
                if let Some(mut session) = mount.session.take() {
                    let _ = tokio::task::block_in_place(|| session.stop());
                }
                drop(mount);
                if let Some(root) = fork::forks_mount_root(&self.repo_root) {
                    let _ = std::fs::remove_dir_all(root);
                }
                self.shutdown.notify_one();
                Ok(proto::Reply::Unit)
            }
            proto::Op::Fork { count, session_id } => {
                if count == 0 || count > 16 {
                    return Err("fork count must be 1..=16".into());
                }
                if !self.mounts.available {
                    return Err(format!(
                        "native {} mounts are unavailable: {}\n{}",
                        self.mounts.provider,
                        self.mounts.reason.as_deref().unwrap_or("unknown reason"),
                        fork::mount_setup_hint()
                    ));
                }
                let root = fork::forks_mount_root(&self.repo_root)
                    .ok_or("repo root has no parent for fork workspaces")?;
                let mut created = Vec::new();
                for _ in 0..count {
                    let seed = self.handle.fork().await.map_err(stringify)?;
                    let id = short_id();
                    self.attach_route(&id, Arc::clone(&seed.shared), seed.config, seed.volume_id)
                        .await?;
                    let entry = proto::ForkEntry {
                        id: id.clone(),
                        path: root.join(&id).display().to_string(),
                        mode: "mount".to_owned(),
                        base: acyclic::generation_hex(seed.base),
                        created_at: unix_now(),
                        session_id: session_id.clone(),
                        conflict_paths: Vec::new(),
                        conflict: None,
                    };
                    self.forks.lock().await.insert(
                        id,
                        ForkState {
                            shared: seed.shared,
                            base: seed.base,
                            entry: clone_entry(&entry),
                            conflict: None,
                        },
                    );
                    created.push(entry);
                }
                Ok(proto::Reply::Forks(created))
            }
            proto::Op::ForkList => {
                let forks = self.forks.lock().await;
                let mut entries: Vec<proto::ForkEntry> = forks
                    .values()
                    .map(|fork| clone_entry(&fork.entry))
                    .collect();
                entries.sort_by_key(|entry| entry.created_at);
                Ok(proto::Reply::Forks(entries))
            }
            proto::Op::ForkDrop { id } => {
                let mut forks = self.forks.lock().await;
                forks.remove(&id).ok_or(format!("no fork {id}"))?;
                drop(forks);
                self.detach_route(&id).await?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::Promote { id } => {
                let mut forks = self.forks.lock().await;
                let mut fork = forks.remove(&id).ok_or(format!("no fork {id}"))?;
                drop(forks);
                let label = format!("promote fork {id}");
                let result = self.promote_fork(&id, &fork, &label).await;
                let landed = match result {
                    Ok(Landed::Conflicted {
                        theirs,
                        ours,
                        files,
                        kept,
                    }) => {
                        // Nothing landed: the fork was rebased onto `theirs`
                        // with markers written in. Keep it, judged against
                        // the head it now sits on.
                        let conflict_base = fork.base;
                        fork.conflict = Some(OpenConflict {
                            paths: files.iter().map(|file| PathBuf::from(&file.path)).collect(),
                        });
                        fork.base = theirs;
                        fork.entry.base = acyclic::generation_hex(theirs);
                        fork.entry.conflict_paths =
                            files.iter().map(|file| file.path.clone()).collect();
                        fork.entry.conflict = Some(proto::ConflictInfo {
                            base: acyclic::generation_hex(conflict_base),
                            ours: acyclic::generation_hex(ours),
                            theirs: acyclic::generation_hex(theirs),
                        });
                        let fork_path = fork.entry.path.clone();
                        self.keep_fork(id, fork).await?;
                        return Ok(proto::Reply::Promote(proto::PromoteInfo {
                            generation: acyclic::generation_hex(theirs),
                            old_tree: None,
                            warning: String::new(),
                            replayed_paths: 0,
                            merged_files: 0,
                            conflicts: files,
                            fork_path: Some(fork_path),
                            kept_mainline: kept,
                            mainline_moved: true,
                        }));
                    }
                    Err(message) => {
                        // A refusal (or a failure) leaves the fork exactly as
                        // it was, still promotable once the cause is fixed.
                        // A resolved conflict stays resolved.
                        if fork.conflict.is_some()
                            && !message.starts_with("unresolved conflict markers")
                        {
                            fork.conflict = None;
                            fork.entry.conflict_paths.clear();
                            fork.entry.conflict = None;
                        }
                        self.keep_fork(id, fork).await?;
                        return Err(message);
                    }
                    Ok(landed) => landed,
                };
                // Landed: the fork is consumed. Drop its mount route.
                if let Err(error) = self.detach_route(&id).await {
                    eprintln!("{NAME} daemon: discard fork {id} after promote: {error}");
                }
                match landed {
                    Landed::Replayed {
                        generation,
                        paths,
                        merged,
                        kept,
                        moved,
                    } => Ok(proto::Reply::Promote(proto::PromoteInfo {
                        generation: acyclic::generation_hex(generation),
                        old_tree: None,
                        warning: if moved {
                            "the mainline had moved; the fork's paths were merged onto it in place"
                                .into()
                        } else {
                            String::new()
                        },
                        replayed_paths: paths,
                        merged_files: merged,
                        conflicts: Vec::new(),
                        fork_path: None,
                        kept_mainline: kept,
                        mainline_moved: moved,
                    })),
                    Landed::Nothing { generation, kept } => {
                        Ok(proto::Reply::Promote(proto::PromoteInfo {
                            generation: acyclic::generation_hex(generation),
                            old_tree: None,
                            warning: String::new(),
                            replayed_paths: 0,
                            merged_files: 0,
                            conflicts: Vec::new(),
                            fork_path: None,
                            kept_mainline: kept,
                            mainline_moved: false,
                        }))
                    }
                    Landed::Conflicted { .. } => unreachable!("handled above"),
                }
            }
            proto::Op::ForkDiff { id } => {
                let (base, overlay) = {
                    let forks = self.forks.lock().await;
                    let fork = forks.get(&id).ok_or(format!("no fork {id}"))?;
                    (fork.base, Arc::clone(&fork.shared))
                };
                let changes = if overlay.lock().await.has_pending_mutations() {
                    let generation = self
                        .handle
                        .snapshot_overlay(overlay)
                        .await
                        .map_err(stringify)?;
                    self.handle
                        .diff(base, generation)
                        .await
                        .map_err(stringify)?
                } else {
                    Vec::new()
                };
                Ok(proto::Reply::Diff(
                    self.annotate_ignored(
                        changes
                            .into_iter()
                            .filter(|change| {
                                change.change != acyclic::diff::ChangeKind::MetadataOnly
                            })
                            .map(diff_entry)
                            .collect(),
                    ),
                ))
            }
        }
    }

    /// Lands a fork in place. The fork's paths are merged onto the current
    /// head (a three-way merge when the mainline moved; a plain write of
    /// the fork's paths when it did not) and written onto the real tree
    /// one path at a time. The repo directory is never replaced. A content
    /// conflict rebases the fork with markers and lands nothing; a refusal
    /// is an error naming the paths and leaves everything untouched.
    async fn promote_fork(
        &self,
        id: &str,
        fork: &ForkState,
        label: &str,
    ) -> Result<Landed, String> {
        let overlay = Arc::clone(&fork.shared);
        // A rebased fork must have resolved its markers before it can land.
        if let Some(conflict) = fork.conflict.as_ref() {
            self.refuse_unresolved_markers(&overlay, conflict).await?;
        }
        let publish_started = std::time::Instant::now();
        let head = self.handle.publish_head().await.map_err(stringify)?;
        acyclic::trace!(
            "daemon",
            "promote {id}: publish_head {:.1}ms; mainline {} since the fork's base",
            acyclic::trace::ms(publish_started),
            if head == fork.base {
                "UNMOVED (plain in-place write)"
            } else {
                "MOVED (three-way merge)"
            }
        );
        let landed = self
            .merge_onto_head(id, overlay, fork.base, head, label)
            .await;
        acyclic::trace!(
            "daemon",
            "promote {id}: outcome {}",
            match &landed {
                Ok(Landed::Replayed {
                    paths,
                    merged,
                    kept,
                    ..
                }) => format!(
                    "LANDED ({paths} path(s) written, {merged} merged by content, {} ignored kept)",
                    kept.len()
                ),
                Ok(Landed::Nothing { .. }) => "NOTHING to land".to_owned(),
                Ok(Landed::Conflicted { files, .. }) => format!(
                    "CONFLICT: {} file(s) rebased into the fork with markers",
                    files.len()
                ),
                Err(message) => format!("REFUSED: {}", message.lines().next().unwrap_or("")),
            }
        );
        landed
    }

    /// Marks the entries the repo's `.gitignore` covers. One git call.
    fn annotate_ignored(&self, mut entries: Vec<proto::DiffEntry>) -> Vec<proto::DiffEntry> {
        let paths: Vec<PathBuf> = entries
            .iter()
            .map(|entry| PathBuf::from(&entry.path))
            .collect();
        let ignored = merge::ignored_paths(&self.repo_root, &paths);
        for entry in &mut entries {
            entry.ignored = ignored.iter().any(|path| path == Path::new(&entry.path));
        }
        entries
    }

    /// Re-inserts a fork that did not land. Its route was never detached.
    async fn keep_fork(&self, id: String, fork: ForkState) -> Result<(), String> {
        self.forks.lock().await.insert(id, fork);
        Ok(())
    }

    /// Builds the mount source for a fork's shared checkout and routes it
    /// under the one native session (mounting it on the first route).
    async fn attach_route(
        &self,
        id: &str,
        shared: Arc<SharedLocalCheckout>,
        config: VolumeConfig,
        volume_id: acyclic_fs::VolumeId,
    ) -> Result<(), String> {
        let root = fork::forks_mount_root(&self.repo_root)
            .ok_or("repo root has no parent for fork workspaces")?;
        // Mount-source construction spins up a callback runtime;
        // keep it (and any mount syscall) off async workers.
        let guarded_paths = self.config.guarded_paths.clone();
        let source = tokio::task::block_in_place(move || {
            let source = CheckoutMountSource::new(shared, config)
                .map_err(|error| format!("mount source: {error:?}"))?;
            Ok::<Arc<dyn MountFilesystem>, String>(
                if GuardedMountFilesystem::is_active(&guarded_paths) {
                    Arc::new(GuardedMountFilesystem::new(
                        Arc::new(source),
                        &guarded_paths,
                    ))
                } else {
                    Arc::new(source)
                },
            )
        })?;
        let mut mount = self.fork_mount.lock().await;
        mount
            .router
            .add_route(route_name(id), source)
            .map_err(|error| format!("route: {error:?}"))?;
        // The ONE session, mounted lazily on the first fork. A route
        // insert is all later forks pay.
        if mount.session.is_none() {
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
            let router = Arc::clone(&mount.router) as Arc<dyn MountFilesystem>;
            let dest = root.clone();
            let session = tokio::task::block_in_place(move || {
                mount_native(
                    NativeMountRequest {
                        mount_id: acyclic::MountId::new(),
                        volume_id,
                        destination: dest,
                        writable: true,
                    },
                    router,
                )
                .map_err(|error| format!("mount: {error:?}"))
            });
            match session {
                Ok(session) => mount.session = Some(session),
                Err(error) => {
                    tokio::task::block_in_place(|| mount.router.remove_route(&route_name(id)));
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Refuses a re-promote while any path of the open conflict still holds
    /// markers in the fork. A deleted path counts as resolved.
    async fn refuse_unresolved_markers(
        &self,
        overlay: &Arc<SharedLocalCheckout>,
        conflict: &OpenConflict,
    ) -> Result<(), String> {
        if !overlay.lock().await.has_pending_mutations() {
            // Nothing written since the rebase: the markers are still there.
            return Err(unresolved_message(&conflict.paths));
        }
        let snapshot = self
            .handle
            .snapshot_overlay(Arc::clone(overlay))
            .await
            .map_err(stringify)?;
        let files = self
            .handle
            .read_files(snapshot, conflict.paths.clone())
            .await
            .map_err(stringify)?;
        let unresolved: Vec<PathBuf> = files
            .into_iter()
            .filter(|(_, bytes)| {
                bytes
                    .as_deref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .is_some_and(merge::has_conflict_markers)
            })
            .map(|(path, _)| path)
            .collect();
        if unresolved.is_empty() {
            Ok(())
        } else {
            Err(unresolved_message(&unresolved))
        }
    }

    /// The merge primitive (v2, content-level). The mainline moved past the
    /// fork's base. Plan the three-way merge of base/head/fork in full
    /// before writing anything. Any refusal: error naming the paths, fork
    /// and tree untouched. Any conflict: the fork is rebased onto the head
    /// (R = head + fork's paths + merged files + marker-bearing files) and
    /// nothing lands. Otherwise M = head + fork's paths + merged files is
    /// written onto the real tree path by path with the same atomic
    /// single-path restore a `restore` uses, then checkpointed and published.
    #[allow(
        clippy::too_many_lines,
        reason = "plan, conflict scan, rebase-or-land, and record are one transaction"
    )]
    async fn merge_onto_head(
        &self,
        id: &str,
        overlay: Arc<SharedLocalCheckout>,
        base: acyclic::GenerationId,
        head: acyclic::GenerationId,
        label: &str,
    ) -> Result<Landed, String> {
        let moved = head != base;
        if !overlay.lock().await.has_pending_mutations() {
            return Ok(Landed::Nothing {
                generation: head,
                kept: Vec::new(),
            });
        }
        let snapshot_started = std::time::Instant::now();
        let snapshot = self
            .handle
            .snapshot_overlay(Arc::clone(&overlay))
            .await
            .map_err(stringify)?;
        let snapshot_ms = acyclic::trace::ms(snapshot_started);
        let plan_started = std::time::Instant::now();
        let mut plan = self
            .handle
            .merge_plan(base, head, snapshot, format!("fork {id}"))
            .await
            .map_err(stringify)?;
        acyclic::trace!(
            "daemon",
            "promote {id}: snapshot_overlay {snapshot_ms:.1}ms, merge_plan {:.1}ms",
            acyclic::trace::ms(plan_started)
        );
        // Gitignored paths (bytecode caches, build output, .env) are not
        // merge payload: a fork's copy never blocks a promote, the
        // mainline keeps its own. Cheap: only the contested paths are asked.
        let contested: Vec<PathBuf> = plan
            .refusals
            .iter()
            .map(|refusal| refusal.path.clone())
            .chain(plan.conflicted.iter().map(|file| file.path.clone()))
            .collect();
        let ignore_started = std::time::Instant::now();
        let ignored =
            tokio::task::block_in_place(|| merge::ignored_paths(&self.repo_root, &contested));
        let ignore_ms = acyclic::trace::ms(ignore_started);
        let kept: Vec<String> = plan
            .keep_mainline_for(&ignored)
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        acyclic::trace!(
            "daemon",
            "merge plan: take_ours={} take_theirs={} merged={} conflicted={} refused={} kept_mainline={}",
            plan.take_ours.len(),
            plan.take_theirs.len(),
            plan.merged.len(),
            plan.conflicted.len(),
            plan.refusals.len(),
            kept.len()
        );
        if !plan.refusals.is_empty() {
            let mut lines: Vec<String> = plan
                .refusals
                .iter()
                .map(|refusal| format!("  {}: {}", refusal.path.display(), refusal.reason))
                .collect();
            lines.sort();
            return Err(format!(
                "the working tree moved past the fork's base ({}) and {} path(s) cannot be merged:\n{}\n\
                 One fork must own those paths: re-fork from the current tree and redo that part, \
                 or rewind to the base. The fork is untouched",
                acyclic::generation_hex(base),
                plan.refusals.len(),
                lines.join("\n")
            ));
        }
        if !moved && !plan.conflicted.is_empty() {
            // Cannot happen (nothing changed on the mainline side), but
            // never let a bug write markers anywhere.
            return Err("internal: conflicts against an unmoved mainline".into());
        }
        if plan.lands_nothing() {
            return Ok(Landed::Nothing {
                generation: head,
                kept,
            });
        }

        // The generation the landing paths are restored FROM.
        //
        // Nothing merged by content and no conflict: every landing path is a
        // fork-only subtree, and the fork's own snapshot F already holds each
        // of them exactly. Restoring them from F is the merge; building
        // M = H + F-subtrees would copy ten subtrees through the store, each
        // object behind a barrier fsync, to arrive at the same bytes.
        //
        // Content merges or conflicts: build M = H + (fork-only subtrees
        // from F) + (content-merged files). A conflict needs the real M,
        // because the rebased fork R = M + markers has to sit on H.
        let build_started = std::time::Instant::now();
        let merged = if plan.merged.is_empty() && plan.conflicted.is_empty() {
            snapshot
        } else {
            let mut entries: Vec<(PathBuf, Entry)> = plan
                .take_ours
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        Entry::FromGeneration {
                            generation: snapshot,
                        },
                    )
                })
                .collect();
            entries.extend(plan.merged.iter().map(|file| {
                (
                    file.path.clone(),
                    Entry::Regular {
                        bytes: file.bytes.clone(),
                        mode: file.mode,
                    },
                )
            }));
            self.handle
                .build_generation(head, entries)
                .await
                .map_err(stringify)?
        };
        acyclic::trace!(
            "daemon",
            "promote {id}: gitignore check {ignore_ms:.1}ms ({} contested), landing source {} in \
             {:.1}ms ({} replayed subtree(s), {} merged file(s))",
            contested.len(),
            if merged == snapshot {
                "= fork snapshot (no build)"
            } else {
                "= built merged generation"
            },
            acyclic::trace::ms(build_started),
            plan.take_ours.len(),
            plan.merged.len()
        );

        if !plan.conflicted.is_empty() {
            // R = M + marker-bearing files; the fork becomes R.
            let entries: Vec<(PathBuf, Entry)> = plan
                .conflicted
                .iter()
                .map(|file| {
                    (
                        file.path.clone(),
                        Entry::Regular {
                            bytes: file.bytes.clone(),
                            mode: file.mode,
                        },
                    )
                })
                .collect();
            let rebased = self
                .handle
                .build_generation(merged, entries)
                .await
                .map_err(stringify)?;
            self.handle
                .record_generation(
                    rebased,
                    format!(
                        "fork {id} rebased onto {} ({} conflict(s))",
                        acyclic::short_hex(&acyclic::generation_hex(head)),
                        plan.conflicted.len()
                    ),
                )
                .await
                .map_err(stringify)?;
            self.rebase_fork(id, snapshot, rebased).await?;
            let mut files: Vec<proto::ConflictEntry> = plan
                .conflicted
                .iter()
                .map(|file| proto::ConflictEntry {
                    path: file.path.display().to_string(),
                    detail: file.describe(),
                })
                .collect();
            files.sort_by(|left, right| left.path.cmp(&right.path));
            return Ok(Landed::Conflicted {
                theirs: head,
                ours: snapshot,
                files,
                kept,
            });
        }

        let land_started = std::time::Instant::now();
        let target = self
            .handle
            .record_generation(
                merged,
                if merged == snapshot {
                    format!("fork {id} snapshot ({} paths)", plan.take_ours.len())
                } else {
                    format!(
                        "fork {id} merge ({} merged, {} replayed)",
                        plan.merged.len(),
                        plan.take_ours.len()
                    )
                },
            )
            .await
            .map_err(stringify)?;
        // The "before" row: the head was captured and published moments
        // ago by publish_head, so record that generation rather than
        // draining the watcher again for a row that would come back noop.
        self.handle
            .record_generation_as(
                head,
                CheckpointKind::PreRewind,
                if moved {
                    format!("before {label} (merge)")
                } else {
                    format!("before {label}")
                },
            )
            .await
            .map_err(stringify)?;
        let pre_ms = acyclic::trace::ms(land_started);
        // One restore for every landing path: one drain, one timeline row
        // carrying the promote label, however many paths the fork touched.
        // publish_head captured the tree just now, so no safety row either.
        let landing = plan.landing_paths();
        let landed_label = if moved {
            format!(
                "{label} (merged {} file(s), replayed {} path(s) onto moved mainline)",
                plan.merged.len(),
                landing.len()
            )
        } else {
            format!("{label} ({} path(s) written in place)", landing.len())
        };
        let restore_started = std::time::Instant::now();
        let restored = self
            .handle
            .restore_paths(target.clone(), landing.clone(), false, Some(landed_label))
            .await
            .map_err(|error| {
                format!(
                    "merge stopped while landing {} path(s): {error}. The tree may be partially \
                     merged; `{NAME} rewind <id>` of the `before promote ... (merge)` row in \
                     `{NAME} timeline` returns to the pre-merge tree",
                    landing.len()
                )
            })?;
        let restore_ms = acyclic::trace::ms(restore_started);
        let written = u32::try_from(restored.outcomes.len()).unwrap_or(u32::MAX);
        let landed_generation = restored.generation;
        // No inline publish: the landed row is a checkpoint like any other
        // and the idle timer publishes it. Authority publish is O(tree).
        acyclic::trace!(
            "daemon",
            "promote {id}: landing {written} path(s): record rows {pre_ms:.1}ms, \
             restore {restore_ms:.1}ms, land total {:.1}ms",
            acyclic::trace::ms(land_started)
        );
        Ok(Landed::Replayed {
            generation: landed_generation,
            paths: written,
            merged: u32::try_from(plan.merged.len()).unwrap_or(u32::MAX),
            kept,
            moved,
        })
    }

    /// Makes the mounted fork equal to `rebased` by writing R − F through
    /// the mount. The real tree is never touched.
    async fn rebase_fork(
        &self,
        id: &str,
        snapshot: acyclic::GenerationId,
        rebased: acyclic::GenerationId,
    ) -> Result<(), String> {
        let changed: Vec<PathBuf> = content_changes(
            self.handle
                .diff(snapshot, rebased)
                .await
                .map_err(stringify)?,
        )
        .into_iter()
        .map(|change| change.path)
        .collect();
        let roots = merge::subtree_roots(&changed);
        // Write THROUGH the mount, never behind it. The driver and kernel
        // keep name and attribute caches that only their own operations
        // update.
        let dir = fork::forks_mount_root(&self.repo_root)
            .ok_or("repo root has no parent for fork workspaces")?
            .join(id);
        self.handle
            .materialize_paths(rebased, dir, roots)
            .await
            .map_err(stringify)
    }

    /// Removes one fork's route; the session unmounts (and the mount root
    /// disappears) when the last route goes, freeing the FUSE-T pool slot.
    async fn detach_route(&self, id: &str) -> Result<(), String> {
        let mut mount = self.fork_mount.lock().await;
        // Dropping a route drops its CheckoutMountSource, which owns a tokio
        // runtime — runtimes must never be dropped on an async worker.
        tokio::task::block_in_place(|| mount.router.remove_route(&route_name(id)));
        // The kernel may hold a positive entry cache for the removed name
        // (FSKit caches until told otherwise): invalidate it eagerly.
        if let Some(session) = mount.session.as_ref() {
            if let Err(error) = tokio::task::block_in_place(|| session.invalidate(&route_name(id)))
            {
                eprintln!("{NAME} daemon: invalidate {id}: {error:?}");
            }
        }
        if mount.router.is_empty() {
            if let Some(mut session) = mount.session.take() {
                tokio::task::block_in_place(|| session.stop())
                    .map_err(|error| format!("unmount: {error:?}"))?;
            }
            if let Some(root) = fork::forks_mount_root(&self.repo_root) {
                let _ = std::fs::remove_dir_all(root);
            }
        }
        Ok(())
    }

    fn open_index(&self) -> Result<Index, String> {
        Index::open(&self.index_db).map_err(stringify)
    }

    fn resolve_target(&self, target: proto::RewindTarget) -> Result<CheckpointRow, String> {
        let index = self.open_index()?;
        let row = match target {
            proto::RewindTarget::Checkpoint(id) => index.by_id(id).map_err(stringify)?,
            proto::RewindTarget::Last => index.latest_target().map_err(stringify)?,
            proto::RewindTarget::SessionStart(session) => {
                index.session_start(&session).map_err(stringify)?
            }
        };
        let row = row.ok_or_else(|| "no matching checkpoint".to_owned())?;
        if !row.is_restorable() {
            return Err(format!(
                "checkpoint #{} records a failed capture, not a tree state; pick another from `{NAME} timeline`",
                row.id
            ));
        }
        Ok(row)
    }

    /// Default diff base: the most recent session's first checkpoint, falling
    /// back to the oldest checkpoint on record.
    fn default_diff_base(&self, index: &Index) -> Result<CheckpointRow, String> {
        let from_session = match index.latest_session().map_err(stringify)? {
            Some(session) => index
                .session_start(&session.session_id)
                .map_err(stringify)?,
            None => None,
        };
        let base = match from_session {
            Some(row) => Some(row),
            None => index.oldest().map_err(stringify)?,
        };
        base.ok_or_else(|| "no checkpoints yet".to_owned())
    }

    /// Builds the previous-session brief: where the last session (other
    /// than `current`) ended, what it changed, and every branch it abandoned
    /// by rewinding. Diffs are computed against the store, so counts are
    /// exact rather than remembered.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass over the session's rows builds every section of the brief"
    )]
    /// One turn's summary, from the speculation cache.
    ///
    /// Never produces one on demand: a summary costs money, and a request
    /// arriving is not consent to spend. If the runner has not produced one,
    /// this says so and says why — the alternative, quietly billing whoever
    /// asked, is worse than an honest "not ready".
    async fn summary(
        &self,
        session_id: Option<String>,
        turn: Option<i64>,
        wait_ms: u64,
    ) -> Result<proto::SummaryInfo, String> {
        let index = self.open_index()?;
        // "Nothing to summarise yet" is an answer, not an error: this is an
        // informational read, and a host that surfaces an error for an empty
        // repo makes the tool look broken.
        let nothing = |reason: &str| proto::SummaryInfo {
            session_id: String::new(),
            turn: 0,
            prompt: String::new(),
            text: None,
            source: format!("unavailable: {reason}"),
            lead_ms: None,
        };
        let session_id = match session_id {
            Some(id) => id,
            None => match index.latest_session().map_err(stringify)? {
                Some(session) => session.session_id,
                None => return Ok(nothing("no sessions on record")),
            },
        };
        // The default is the last turn that has actually finished: the
        // current one is still being worked on and cannot be summarised.
        let turn = if let Some(turn) = turn {
            turn
        } else {
            let last = index
                .turns(Some(&session_id))
                .map_err(stringify)?
                .iter()
                .filter(|row| row.checkpoints > 0)
                .map(|row| row.turn)
                .next_back();
            let Some(last) = last else {
                return Ok(nothing("no completed turn in this session"));
            };
            last
        };
        let unavailable = |source: &str, prompt: String| proto::SummaryInfo {
            session_id: session_id.clone(),
            turn,
            prompt,
            text: None,
            source: source.to_owned(),
            lead_ms: None,
        };
        let Some(spec) = self.spec.as_ref() else {
            return Ok(unavailable(
                "unavailable: speculation is off",
                String::new(),
            ));
        };
        let Some((key, prompt)) = spec.summary_key(&index, &session_id, turn) else {
            return Ok(unavailable(
                "unavailable: this turn changed nothing",
                String::new(),
            ));
        };
        let (hit, source) = spec.claim_summary(&key, wait_ms).await;
        Ok(proto::SummaryInfo {
            session_id,
            turn,
            prompt,
            text: hit.as_ref().map(|hit| hit.body.trim().to_owned()),
            lead_ms: hit.as_ref().map(|hit| hit.lead_ms),
            source,
        })
    }

    /// Stops a model run that real work has just made pointless.
    ///
    /// Purely a spend control. A result computed against a tree that has
    /// moved is already unreachable through its key, so nothing here is
    /// protecting correctness — it is protecting the bill.
    fn invalidate(&self, cause: &crate::speculate::Cause) {
        if let Some(spec) = self.spec.as_ref() {
            spec.notify(crate::speculate::SpecEvent::Invalidate(cause.clone()));
        }
    }

    /// Speculation's rollup for `status`, or `None` when it is off — which
    /// keeps the default output byte-identical to before the feature.
    fn speculation_status(&self) -> Option<proto::SpecStatus> {
        let spec = self.spec.as_ref()?;
        let metrics = spec.metrics().unwrap_or_default();
        Some(proto::SpecStatus {
            spends_tokens: spec.config().spends_tokens(),
            command: spec.config().command.join(" "),
            runs: metrics.runs,
            claimed: metrics.claimed,
            missed: metrics.missed,
            timeouts: metrics.timeouts,
            bytes_out: metrics.bytes_out,
            median_lead_ms: metrics.median_lead_ms,
        })
    }

    /// The brief, from the speculation cache when one matches and from the
    /// pipeline otherwise.
    ///
    /// This is the request the agent actually waits on: the `SessionStart`
    /// hook prints the result into the model's context before the session
    /// does anything. Every speculative step here is allowed to fail — a
    /// miss just means doing the work now, exactly as before.
    async fn brief(&self, current: Option<&str>) -> Result<proto::BriefInfo, String> {
        let index = self.open_index()?;
        let Some(spec) = self.spec.as_ref() else {
            return compute_brief(index, &self.handle, current).await;
        };
        let key = crate::speculate::brief_key(&index, spec.config(), current).ok();
        if let Some(key) = key.as_ref().and_then(Option::as_ref) {
            if let Some(mut info) = spec.claim_brief(key) {
                self.attach_summary(&mut info).await;
                return Ok(info);
            }
        }
        drop(index);
        let index = self.open_index()?;
        let mut info = compute_brief(index, &self.handle, current).await?;
        // Memoize the miss: the next session start over this same tree is a
        // hit even though nothing scheduled it.
        if let Some(key) = key.and_then(|key| key) {
            spec.store_brief(&key, &info);
        }
        self.attach_summary(&mut info).await;
        Ok(info)
    }

    /// Adds the previous session's closing summary to a brief, when the
    /// runner produced one.
    ///
    /// Looked up after the brief rather than inside it, and never waited
    /// for: this runs on `SessionStart`, which the agent blocks on.
    async fn attach_summary(&self, info: &mut proto::BriefInfo) {
        let (Some(spec), Some(session)) = (self.spec.as_ref(), info.session.as_mut()) else {
            return;
        };
        let Some(turn) = session.end_turn else { return };
        let Ok(index) = self.open_index() else { return };
        let Some((key, _)) = spec.summary_key(&index, &session.session_id, turn) else {
            return;
        };
        let (hit, _) = spec.claim_summary(&key, 0).await;
        session.summary = hit.map(|hit| hit.body.trim().to_owned());
    }
}

/// The previous-session brief: where the last session ended, what it changed,
/// and which branches it abandoned.
///
/// A free function rather than a method because the speculation scheduler
/// computes exactly this, ahead of time, off the request path. It is the
/// most expensive thing the agent waits on — `SessionStart` prints it into
/// context on every session start, and it costs one pipeline diff per
/// abandoned branch plus two more.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the index must be OWNED across the awaits below: rusqlite's \
              Connection is Send but not Sync, so a &Index held across an \
              await would make this future non-Send and the per-connection \
              tokio::spawn would not compile"
)]
pub(crate) async fn compute_brief(
    index: Index,
    handle: &PipelineHandle,
    current: Option<&str>,
) -> Result<proto::BriefInfo, String> {
    let Some(session) = index
        .last_session_with_checkpoints(current)
        .map_err(stringify)?
    else {
        return Ok(proto::BriefInfo::default());
    };
    let id = session.session_id.clone();
    let start = index.session_start(&id).map_err(stringify)?;
    let end = index.session_end(&id).map_err(stringify)?;
    let last_any = index
        .list(Some(&id), None, 1)
        .map_err(stringify)?
        .into_iter()
        .next();

    let (files_changed, sample_paths) = match (&start, &end) {
        (Some(start), Some(end)) if start.generation != end.generation => {
            let changes = content_changes(
                handle
                    .diff(start.generation, end.generation)
                    .await
                    .map_err(stringify)?,
            );
            let sample = changes
                .iter()
                .take(5)
                .map(|change| change.path.display().to_string())
                .collect();
            (changes.len() as u64, sample)
        }
        _ => (0, Vec::new()),
    };

    // Gathered from the index first (no awaits), then diffed: holding a
    // `&Index` across an await would make this future non-Send.
    let pending = abandoned_branches(&index, start.as_ref(), last_any.as_ref())?;
    let mut abandoned = Vec::with_capacity(pending.len());
    for branch in pending {
        let files_changed = match branch.diff {
            Some((before, after)) => {
                content_changes(handle.diff(before, after).await.map_err(stringify)?).len() as u64
            }
            None => 0,
        };
        abandoned.push(proto::BriefAbandoned {
            from_checkpoint: branch.from_checkpoint,
            to_checkpoint: branch.to_checkpoint,
            rewound_to: branch.rewound_to,
            turn: branch.turn,
            prompt: branch.prompt,
            checkpoints: branch.checkpoints,
            files_changed,
        });
    }

    let (end_turn, end_prompt) = match end.as_ref().and_then(|row| row.turn) {
        Some(turn) => (
            Some(turn),
            index
                .turn(&id, turn)
                .map_err(stringify)?
                .map(|turn| turn.prompt),
        ),
        None => (None, None),
    };

    // Drift: the tree may have moved since the session ended (another
    // session that never ended cleanly, or edits with no session).
    let drift_files = match (&end, index.latest_target().map_err(stringify)?) {
        (Some(end), Some(latest)) if latest.generation != end.generation => content_changes(
            handle
                .diff(end.generation, latest.generation)
                .await
                .map_err(stringify)?,
        )
        .len() as u64,
        _ => 0,
    };

    Ok(proto::BriefInfo {
        session: Some(proto::BriefSession {
            session_id: id,
            host: session.host,
            started_at: session.started_at,
            ended_at: session.ended_at,
            turns: session.turns,
            checkpoints: session.checkpoints,
            end_checkpoint: end.as_ref().map(|row| row.id),
            end_turn,
            end_prompt,
            files_changed,
            sample_paths,
            // Filled in by the caller: whether a summary exists is
            // independent of everything else here, so baking it into the
            // cached body would mean a brief cached before the summary
            // landed could never pick it up.
            summary: None,
            abandoned,
        }),
        drift_files,
    })
}

/// One abandoned branch, read out of the index but not yet costed.
struct PendingBranch {
    from_checkpoint: i64,
    to_checkpoint: i64,
    rewound_to: i64,
    turn: Option<i64>,
    prompt: Option<String>,
    checkpoints: i64,
    /// Generations to diff for the branch's size, when the two differ.
    diff: Option<(acyclic::GenerationId, acyclic::GenerationId)>,
}

/// Every branch the session rewound away from, with the work it would take
/// to size each one. Deliberately synchronous: the caller does the awaiting,
/// so nothing holds a borrow of the index across one.
fn abandoned_branches(
    index: &Index,
    start: Option<&CheckpointRow>,
    last_any: Option<&CheckpointRow>,
) -> Result<Vec<PendingBranch>, String> {
    let (Some(start), Some(last_any)) = (start, last_any) else {
        return Ok(Vec::new());
    };
    let mut branches = Vec::new();
    for rewind in index
        .rewinds_between(start.id, last_any.id)
        .map_err(stringify)?
    {
        let Some(target) = rewind.rewind_target else {
            continue;
        };
        let branch = index.between(target, rewind.id).map_err(stringify)?;
        let (Some(first), Some(last)) = (branch.first(), branch.last()) else {
            continue;
        };
        let target_row = index.by_id(target).map_err(stringify)?;
        let diff = target_row
            .filter(|row| row.generation != last.generation)
            .map(|row| (row.generation, last.generation));
        let turn = last.turn.or(first.turn);
        let prompt = match (last.session_id.as_deref(), turn) {
            (Some(session), Some(turn)) => index
                .turn(session, turn)
                .map_err(stringify)?
                .map(|turn| turn.prompt),
            _ => None,
        };
        branches.push(PendingBranch {
            from_checkpoint: first.id,
            to_checkpoint: last.id,
            rewound_to: target,
            turn,
            prompt,
            checkpoints: i64::try_from(branch.len()).unwrap_or(i64::MAX),
            diff,
        });
    }
    Ok(branches)
}

/// Content changes only. A rewind or restore rewrites mtimes on every path
/// it materializes, so metadata-only rows are noise for "what changed".
fn content_changes(changes: Vec<acyclic::diff::FileChange>) -> Vec<acyclic::diff::FileChange> {
    changes
        .into_iter()
        .filter(|change| change.change != acyclic::diff::ChangeKind::MetadataOnly)
        .collect()
}

fn err(message: String) -> proto::Payload {
    proto::Payload::Err { message }
}

/// A fork id as the router's route name.
///
/// The router projects a route as a directory entry, so the name crosses the
/// same boundary as any captured name and has to carry the same encoding —
/// the `ProjFS` provider decodes every entry name it is handed as UTF-16LE,
/// and hands an id passed as raw ASCII back to the user as mojibake. Ids are
/// hex, so this is a widening on Windows and a copy everywhere else.
fn route_name(id: &str) -> Vec<u8> {
    acyclic::names::str_to_bytes(id)
}

fn short_id() -> String {
    // UUIDv7 leads with timestamp bits (identical across nearby calls);
    // the tail is the random section.
    acyclic::MountId::new().into_bytes()[10..]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unix_now() -> i64 {
    acyclic::unix_now()
}

/// How a fork ended up in the real tree.
enum Landed {
    /// Path-by-path write onto the current head; never a directory swap.
    /// `merged` of the paths were produced by a three-way content merge;
    /// `moved` says whether the mainline had moved past the fork's base.
    Replayed {
        generation: acyclic::GenerationId,
        paths: u32,
        merged: u32,
        kept: Vec<String>,
        moved: bool,
    },
    /// No content changes to land.
    Nothing {
        generation: acyclic::GenerationId,
        kept: Vec<String>,
    },
    /// Nothing landed: the fork was rebased onto `theirs` and `files`
    /// carry conflict markers in the fork workspace.
    Conflicted {
        theirs: acyclic::GenerationId,
        ours: acyclic::GenerationId,
        files: Vec<proto::ConflictEntry>,
        kept: Vec<String>,
    },
}

/// The variant name of an op, for trace lines.
fn op_name(op: &proto::Op) -> String {
    let debug = format!("{op:?}");
    debug
        .split([' ', '{', '('])
        .next()
        .unwrap_or("?")
        .to_owned()
}

fn reply_name(reply: &proto::Reply) -> String {
    let debug = format!("{reply:?}");
    debug
        .split([' ', '{', '('])
        .next()
        .unwrap_or("?")
        .to_owned()
}

fn unresolved_message(paths: &[PathBuf]) -> String {
    let shown: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    format!(
        "unresolved conflict markers in: {}. Resolve every <<<<<<< / ||||||| / ======= / >>>>>>> block \
         in the fork (or delete the file) and promote again",
        shown.join(", ")
    )
}

fn clone_entry(entry: &proto::ForkEntry) -> proto::ForkEntry {
    proto::ForkEntry {
        id: entry.id.clone(),
        path: entry.path.clone(),
        mode: entry.mode.clone(),
        base: entry.base.clone(),
        created_at: entry.created_at,
        session_id: entry.session_id.clone(),
        conflict_paths: entry.conflict_paths.clone(),
        conflict: entry.conflict.clone(),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as `.map_err(stringify)`, which hands over the error by value"
)]
fn stringify(error: EngineError) -> String {
    error.to_string()
}

/// A request kind is a strict subset of the engine's kinds: the wire type
/// can't name `baseline`, `noop`, or the other daemon-decided ones.
fn engine_kind(kind: proto::CheckpointRequestKind) -> CheckpointKind {
    match kind {
        proto::CheckpointRequestKind::Pre => CheckpointKind::Pre,
        proto::CheckpointRequestKind::Post => CheckpointKind::Post,
        proto::CheckpointRequestKind::Manual => CheckpointKind::Manual,
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as `.map(diff_entry)` over an owning iterator"
)]
fn diff_entry(change: acyclic::diff::FileChange) -> proto::DiffEntry {
    proto::DiffEntry {
        path: change.path.display().to_string(),
        change: change.change,
        file_kind: format!("{:?}", change.file_kind).to_lowercase(),
        ignored: false,
    }
}

fn timeline_entry(row: CheckpointRow) -> proto::TimelineEntry {
    proto::TimelineEntry {
        id: row.id,
        created_at: row.created_at,
        kind: row.kind,
        published: row.published,
        session_id: row.session_id,
        tool_name: row.tool_name,
        label: row.label,
        error: row.error,
        turn: row.turn,
    }
}

fn hex_generation(generation: acyclic::GenerationId) -> String {
    acyclic::generation_hex(generation)
}

fn directory_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    total
}
