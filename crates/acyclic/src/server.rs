//! The per-repo daemon: owns the pipeline, serves the unix socket.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acyclic_engine::config::Config;
use acyclic_engine::fork::{
    self, ForkMode, MountCapability, PromoteOutcome, SessionResolveOutcome, SharedLocalCheckout,
};
use acyclic_engine::guard::GuardedMountFilesystem;
use acyclic_engine::index::{Attribution, CheckpointKind, CheckpointRow, Index};
use acyclic_engine::merge::{self, Entry};
use acyclic_engine::pipeline::{self, PipelineHandle};
use acyclic_engine::product::NAME;
use acyclic_engine::store::{Store, StorePaths};
use acyclic_engine::{rewind, EngineError};
use acyclic_fs::model::VolumeConfig;
use acyclic_fs::{
    mount_native, mount_native_over_existing, CheckoutMountSource, MountFilesystem,
    NativeMountRequest, NativeMountSession, RoutedMountSource,
};
use acyclic_proto as proto;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
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
    base: acyclic_engine::GenerationId,
    entry: proto::ForkEntry,
    /// Copy-mode forks only: the materialized directory the user works in.
    /// Captured back into `shared` at promote, removed at drop/promote.
    copy_dir: Option<PathBuf>,
    /// Set by a conflicting promote until the markers are gone.
    conflict: Option<OpenConflict>,
}

/// A conflict a promote wrote into a fork workspace (graphcoder's
/// `FS_CONFLICT_OPENED` payload: base, ours, theirs, paths).
#[derive(Clone, Debug)]
struct OpenConflict {
    paths: Vec<PathBuf>,
}

/// One Safe Mode session: its fork and the shadow mount that projects it
/// directly at the real repo root for the session's duration. Only one can
/// be active at a time -- shadowing is a whole-path substitution, so two
/// sessions can't both shadow the same repo root concurrently.
struct DrySession {
    fork_id: String,
    session_id: String,
    shared: Arc<SharedLocalCheckout>,
    base: acyclic_engine::GenerationId,
    mount: NativeMountSession,
}

/// A `SessionResolve`d session awaiting `SessionApply`/`SessionDiscard`. Its
/// overlay is already committed to the store under `generation`; nothing
/// has touched the real tree yet.
struct PendingSession {
    generation: acyclic_engine::GenerationId,
    base: acyclic_engine::GenerationId,
    label: String,
}

/// The one native session projecting every fork through the router.
/// Mounted lazily on the first fork, unmounted when the last route goes.
struct ForkMount {
    router: Arc<RoutedMountSource>,
    session: Option<NativeMountSession>,
}

pub fn run(repo_root: &Path) -> Result<(), String> {
    // FIRST, before anything reads through `repo_root`: a Safe Mode shadow
    // mount from a crashed daemon leaves the repo root a dead NFS mountpoint
    // that wedges every stat/open under it (Config::load, canonicalize, ...).
    // The force-unmount acts on the mountpoint path itself without touching
    // the dead server, so the real tree reappears before we read the config.
    fork::sweep_stale_dry_session(repo_root);

    let config = Config::load(repo_root).map_err(|error| error.to_string())?;
    let stores_root = config.store_dir.as_ref().map(PathBuf::from);
    let paths = StorePaths::for_repo(repo_root, stores_root.as_deref())
        .map_err(|error| error.to_string())?;

    // Finish or unwind any rewind that a crash interrupted BEFORE the store
    // opens and the pipeline baselines; sweep fork dirs a dead daemon left
    // mounted (fork sessions do not survive the daemon).
    rewind::recover(&paths.rewind_journal()).map_err(|error| error.to_string())?;
    fork::sweep_stale_forks(repo_root);

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let store = runtime
        .block_on(Store::open(paths.clone()))
        .map_err(|error| error.to_string())?;
    let repo_root = store.repo_root.clone();
    let index = Index::open(&paths.index_db()).map_err(|error| error.to_string())?;
    let (handle, pipeline_thread) = pipeline::spawn(store, index, config.clone());

    // Socket + pidfile. A stale socket from a dead daemon is removed; a live
    // one refuses the second daemon via bind failure after removal race.
    let _ = std::fs::remove_file(paths.socket());
    let listener = runtime
        .block_on(async { UnixListener::bind(paths.socket()) })
        .map_err(|error| format!("bind {}: {error}", paths.socket().display()))?;
    std::fs::write(paths.pidfile(), std::process::id().to_string())
        .map_err(|error| error.to_string())?;

    let shutdown = Arc::new(Notify::new());
    let mut mounts = fork::mount_capability();
    // Test hook: exercise the copy-fork paths on a host that has mounts.
    if std::env::var_os("ACYCLIC_FORCE_COPY_FORKS").is_some() {
        mounts.available = false;
        mounts.reason = Some("ACYCLIC_FORCE_COPY_FORKS is set".into());
    }
    if !mounts.available {
        eprintln!(
            "{NAME} daemon: mounts unavailable ({}): forks fall back to copies, Safe Mode is off",
            mounts.reason.as_deref().unwrap_or("unknown reason")
        );
    }
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
        dry_session: Arc::new(Mutex::new(None)),
        pending: Arc::new(Mutex::new(HashMap::new())),
    };

    runtime.block_on(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let server = server.clone();
                    tokio::spawn(async move { server.serve(stream).await });
                }
                _ = shutdown.notified() => break,
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        let _ = handle.shutdown().await;
    });

    let _ = pipeline_thread.join();
    let _ = std::fs::remove_file(paths.socket());
    let _ = std::fs::remove_file(paths.pidfile());
    Ok(())
}

#[derive(Clone)]
struct Server {
    /// Probed once at start: decides fork mode and gates Safe Mode.
    mounts: MountCapability,
    handle: PipelineHandle,
    index_db: PathBuf,
    store_root: PathBuf,
    repo_root: PathBuf,
    config: Config,
    shutdown: Arc<Notify>,
    forks: Arc<Mutex<HashMap<String, ForkState>>>,
    fork_mount: Arc<Mutex<ForkMount>>,
    /// The one active Safe Mode session shadow-mounted at `repo_root`, if any.
    dry_session: Arc<Mutex<Option<DrySession>>>,
    /// Sessions that resolved (committed) but haven't been applied/discarded.
    pending: Arc<Mutex<HashMap<String, PendingSession>>>,
}

impl Server {
    async fn serve(&self, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
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
        acyclic_engine::trace!("daemon", "op {name} received");
        let payload = match self.dispatch_inner(op).await {
            Ok(reply) => {
                acyclic_engine::trace!(
                    "daemon",
                    "op {name} -> {} in {:.1}ms",
                    reply_name(&reply),
                    acyclic_engine::trace::ms(started)
                );
                proto::Payload::Ok(Box::new(reply))
            }
            Err(message) => {
                acyclic_engine::trace!(
                    "daemon",
                    "op {name} -> error in {:.1}ms: {}",
                    acyclic_engine::trace::ms(started),
                    message.lines().next().unwrap_or("")
                );
                err(message)
            }
        };
        payload
    }

    async fn dispatch_inner(&self, op: proto::Op) -> Result<proto::Reply, String> {
        match op {
            proto::Op::Ping => Ok(proto::Reply::Pong),
            proto::Op::Status => {
                let status = self.handle.status().await.map_err(stringify)?;
                Ok(proto::Reply::Status(proto::StatusInfo {
                    state: format!("{:?}", status.state).to_lowercase(),
                    last_checkpoint: status.last_checkpoint,
                    unpublished: status.unpublished,
                    store_bytes: directory_bytes(&self.store_root.join("store")),
                    repo_root: self.repo_root.display().to_string(),
                    mount_provider: self.mounts.provider.to_string(),
                    mount_available: self.mounts.available,
                    mount_reason: self.mounts.reason.clone(),
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
                let kind = parse_kind(&kind)?;
                let attribution = Attribution {
                    session_id,
                    tool_call_id,
                    tool_name,
                    label,
                    turn: None,
                    rewind_target: None,
                };
                if wait {
                    acyclic_engine::trace!(
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
                        kind: outcome.kind.as_str().to_string(),
                    }))
                } else {
                    // Enqueue-ack: the hook path. Admission into the FIFO
                    // happens BEFORE the ack, so a stop arriving after the
                    // ack queues behind the capture instead of dropping it.
                    // Failures land in the index as `failed`.
                    acyclic_engine::trace!(
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
                    kind: row.kind.as_str().to_string(),
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
            proto::Op::Brief { current } => {
                let brief = self.brief(current.as_deref()).await?;
                Ok(proto::Reply::Brief(brief))
            }
            proto::Op::Rewind {
                target,
                path: Some(path),
            } => {
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
                    action: match outcome.action {
                        rewind::RestoreAction::Restored => "restored",
                        rewind::RestoreAction::Removed => "removed",
                    }
                    .to_string(),
                    recorded_checkpoint,
                }))
            }
            proto::Op::Rewind { target, path: None } => {
                let row = self.resolve_target(target)?;
                let row_id = row.id;
                let outcome = self.handle.rewind(row).await.map_err(stringify)?;
                Ok(proto::Reply::Rewind(proto::RewindInfo {
                    restored_checkpoint: row_id,
                    old_tree: outcome.old_tree.display().to_string(),
                    warning: outcome.warning.to_string(),
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
                self.handle
                    .session_started(session_id.clone(), host)
                    .await
                    .map_err(stringify)?;
                if self.config.dry_run {
                    self.session_fork(session_id).await?;
                }
                Ok(proto::Reply::Unit)
            }
            proto::Op::SessionEnd { session_id } => {
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
                    let fork = self.forks.lock().await.remove(&id);
                    let copy_dir = fork.and_then(|fork| fork.copy_dir);
                    if let Err(error) = self.discard_fork_workspace(&id, copy_dir.as_deref()).await
                    {
                        eprintln!("{NAME} daemon: drop scratch fork {id}: {error}");
                    }
                }
                Ok(proto::Reply::Unit)
            }
            proto::Op::Commit => {
                self.handle.commit().await.map_err(stringify)?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::Stop => {
                // Unmount an active Safe Mode shadow first: it sits directly
                // on the real repo root, so this must never be left mounted
                // once the daemon that owns it is gone.
                if let Some(mut session) = self.dry_session.lock().await.take() {
                    let _ = tokio::task::block_in_place(|| session.mount.stop());
                }
                self.pending.lock().await.clear();
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
                if self.mounts.fork_mode() == ForkMode::Copy {
                    return self.fork_copies(count, session_id).await;
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
                        mode: ForkMode::Mount.as_str().to_string(),
                        base: acyclic_engine::generation_hex(seed.base),
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
                            copy_dir: None,
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
                let fork = forks.remove(&id).ok_or(format!("no fork {id}"))?;
                drop(forks);
                self.discard_fork_workspace(&id, fork.copy_dir.as_deref())
                    .await?;
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
                        fork.entry.base = acyclic_engine::generation_hex(theirs);
                        fork.entry.conflict_paths =
                            files.iter().map(|file| file.path.clone()).collect();
                        fork.entry.conflict = Some(proto::ConflictInfo {
                            base: acyclic_engine::generation_hex(conflict_base),
                            ours: acyclic_engine::generation_hex(ours),
                            theirs: acyclic_engine::generation_hex(theirs),
                        });
                        let fork_path = fork.entry.path.clone();
                        self.keep_fork(id, fork).await?;
                        return Ok(proto::Reply::Promote(proto::PromoteInfo {
                            generation: acyclic_engine::generation_hex(theirs),
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
                // Landed: the fork is consumed. Drop its route (mount fork)
                // or its directory (copy fork).
                if let Err(error) = self
                    .discard_fork_workspace(&id, fork.copy_dir.as_deref())
                    .await
                {
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
                        generation: acyclic_engine::generation_hex(generation),
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
                            generation: acyclic_engine::generation_hex(generation),
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
                let (base, shared, copy_dir) = {
                    let forks = self.forks.lock().await;
                    let fork = forks.get(&id).ok_or(format!("no fork {id}"))?;
                    (fork.base, Arc::clone(&fork.shared), fork.copy_dir.clone())
                };
                // Mounted fork: its writes already sit in its own overlay, so
                // snapshot that. Copy fork: read the directory into a
                // scratch overlay pinned at the base (native capture cannot
                // read through the mount itself: NFS lacks the extent ioctl).
                // Either way the fork stays promotable afterwards.
                let overlay = match copy_dir {
                    None => shared,
                    Some(dir) => {
                        let scratch = self
                            .handle
                            .scratch_checkout(base)
                            .await
                            .map_err(stringify)?;
                        fork::capture_copy(&scratch, &dir)
                            .await
                            .map_err(stringify)?;
                        scratch
                    }
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
                // Timestamps differ on a copy fork by construction; only
                // content is a blast radius.
                Ok(proto::Reply::Diff(
                    self.annotate_ignored(
                        changes
                            .into_iter()
                            .filter(|change| {
                                change.change != acyclic_engine::diff::ChangeKind::MetadataOnly
                            })
                            .map(diff_entry)
                            .collect(),
                    ),
                ))
            }
            proto::Op::SessionFork { session_id } => {
                self.session_fork(session_id).await?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::SessionResolve { session_id } => {
                let mut slot = self.dry_session.lock().await;
                let session = slot
                    .take()
                    .filter(|session| session.session_id == session_id)
                    .ok_or_else(|| format!("no active Safe Mode session {session_id}"))?;
                drop(slot);
                // Unmount first: the real tree must reappear before we ask
                // the engine to touch it, and no new writes can race the
                // commit below.
                let DrySession {
                    fork_id,
                    session_id,
                    shared,
                    base,
                    mut mount,
                } = session;
                tokio::task::block_in_place(|| mount.stop())
                    .map_err(|error| format!("unmount: {error:?}"))?;
                let label = format!("safe mode session {fork_id}");
                let outcome = self
                    .handle
                    .resolve_session(Arc::clone(&shared), base, label.clone())
                    .await
                    .map_err(stringify)?;
                match outcome {
                    SessionResolveOutcome::NoChanges => {
                        Ok(proto::Reply::SessionPending(proto::SessionPendingInfo {
                            session_id,
                            diff: Vec::new(),
                        }))
                    }
                    SessionResolveOutcome::Resolved { generation } => {
                        let changes = self
                            .handle
                            .diff(base, generation)
                            .await
                            .map_err(stringify)?;
                        self.pending.lock().await.insert(
                            session_id.clone(),
                            PendingSession {
                                generation,
                                base,
                                label,
                            },
                        );
                        Ok(proto::Reply::SessionPending(proto::SessionPendingInfo {
                            session_id,
                            diff: changes.into_iter().map(diff_entry).collect(),
                        }))
                    }
                    SessionResolveOutcome::Conflict { message } => Err(message),
                }
            }
            proto::Op::SessionApply { session_id } => {
                let mut pending = self.pending.lock().await;
                let session = pending
                    .remove(&session_id)
                    .ok_or_else(|| format!("no resolved Safe Mode session {session_id}"))?;
                drop(pending);
                let outcome = self
                    .handle
                    .apply_session(session.generation, session.base, session.label)
                    .await
                    .map_err(stringify)?;
                match outcome {
                    PromoteOutcome::Promoted {
                        generation,
                        old_tree,
                    } => Ok(proto::Reply::Promote(proto::PromoteInfo {
                        generation: acyclic_engine::generation_hex(generation),
                        old_tree: old_tree.map(|path| path.display().to_string()),
                        warning: "reload your editor: open files still point at the replaced tree"
                            .into(),
                        replayed_paths: 0,
                        merged_files: 0,
                        conflicts: Vec::new(),
                        fork_path: None,
                        kept_mainline: Vec::new(),
                        mainline_moved: false,
                    })),
                    PromoteOutcome::Conflict { message } => Err(message),
                }
            }
            proto::Op::SessionDiscard { session_id } => {
                self.pending.lock().await.remove(&session_id);
                Ok(proto::Reply::Unit)
            }
        }
    }

    /// Forks one checkout and shadow-mounts it directly at `repo_root` for
    /// `session_id`'s duration (Safe Mode's session redirection). Only one
    /// Safe Mode session can be active per repo at a time.
    async fn session_fork(&self, session_id: String) -> Result<(), String> {
        if !self.mounts.available {
            return Err(format!(
                "Safe Mode needs a mount provider and this host has none ({}).\n{}",
                self.mounts.reason.as_deref().unwrap_or("unknown reason"),
                fork::mount_setup_hint()
            ));
        }
        if self.dry_session.lock().await.is_some() {
            return Err("a Safe Mode session is already active for this repo".to_string());
        }
        let seed = self.handle.fork().await.map_err(stringify)?;
        let shared = Arc::clone(&seed.shared);
        let config = seed.config;
        let guarded_paths = self.config.guarded_paths.clone();
        let volume_id = seed.volume_id;
        let destination = self.repo_root.clone();
        let mount = tokio::task::block_in_place(move || {
            let source = CheckoutMountSource::new(shared, config)
                .map_err(|error| format!("mount source: {error:?}"))?;
            let source: Arc<dyn MountFilesystem> =
                if GuardedMountFilesystem::is_active(&guarded_paths) {
                    Arc::new(GuardedMountFilesystem::new(
                        Arc::new(source),
                        &guarded_paths,
                    ))
                } else {
                    Arc::new(source)
                };
            mount_native_over_existing(
                NativeMountRequest {
                    mount_id: acyclic_engine::MountId::new(),
                    volume_id,
                    destination,
                    writable: true,
                },
                source,
            )
            .map_err(|error| format!("shadow mount: {error:?}"))
        })?;
        *self.dry_session.lock().await = Some(DrySession {
            fork_id: short_id(),
            session_id,
            shared: seed.shared,
            base: seed.base,
            mount,
        });
        // The fork now shadows the real repo root: suspend mainline capture
        // until resolve/apply, or the pipeline watcher captures the shadow's
        // content and the mount lifecycle instead of real-tree mutations.
        self.handle.set_shadowed(true).await.map_err(stringify)?;
        Ok(())
    }

    /// Copy-mode forks: materialize the base generation into a real
    /// directory per fork. Same ids, lifecycle, and promote semantics as
    /// mounted forks; creation is O(tree) instead of O(1).
    async fn fork_copies(
        &self,
        count: u32,
        session_id: Option<String>,
    ) -> Result<proto::Reply, String> {
        let root = fork::forks_copy_root(&self.repo_root)
            .ok_or("repo root has no parent for fork workspaces")?;
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let mut created = Vec::new();
        for _ in 0..count {
            let seed = self.handle.fork().await.map_err(stringify)?;
            let id = short_id();
            let dir = root.join(&id);
            self.handle
                .materialize(seed.base, dir.clone())
                .await
                .map_err(stringify)?;
            let entry = proto::ForkEntry {
                id: id.clone(),
                path: dir.display().to_string(),
                mode: ForkMode::Copy.as_str().to_string(),
                base: acyclic_engine::generation_hex(seed.base),
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
                    copy_dir: Some(dir),
                    conflict: None,
                },
            );
            created.push(entry);
        }
        Ok(proto::Reply::Forks(created))
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
        // Get at the fork's overlay. A mounted fork keeps serving while we
        // work: its snapshot is taken under the checkout lock, and the
        // route is detached only once the fork has landed. (Detaching
        // first and re-attaching on a conflict left the kernel's negative
        // name cache hiding the fork on Linux FUSE, which cannot
        // invalidate a route name.)
        let overlay = match fork.copy_dir.as_deref() {
            Some(dir) => {
                let scratch = self
                    .handle
                    .scratch_checkout(fork.base)
                    .await
                    .map_err(stringify)?;
                fork::capture_copy(&scratch, dir).await.map_err(stringify)?;
                scratch
            }
            None => Arc::clone(&fork.shared),
        };
        // A rebased fork must have resolved its markers before it can land.
        if let Some(conflict) = fork.conflict.as_ref() {
            self.refuse_unresolved_markers(&overlay, conflict).await?;
        }
        let head = self.handle.publish_head().await.map_err(stringify)?;
        acyclic_engine::trace!(
            "daemon",
            "promote {id}: {} fork, mainline {} since the fork's base",
            if fork.copy_dir.is_some() {
                "copy"
            } else {
                "mount"
            },
            if head == fork.base {
                "UNMOVED (plain in-place write)"
            } else {
                "MOVED (three-way merge)"
            }
        );
        let landed = self
            .merge_onto_head(
                id,
                overlay,
                fork.base,
                head,
                fork.copy_dir.as_deref(),
                label,
            )
            .await;
        acyclic_engine::trace!(
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
                Ok(Landed::Nothing { .. }) => "NOTHING to land".to_string(),
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
            .add_route(id.to_string().into_bytes(), source)
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
                        mount_id: acyclic_engine::MountId::new(),
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
                    tokio::task::block_in_place(|| mount.router.remove_route(id.as_bytes()));
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
    async fn merge_onto_head(
        &self,
        id: &str,
        overlay: Arc<SharedLocalCheckout>,
        base: acyclic_engine::GenerationId,
        head: acyclic_engine::GenerationId,
        copy_dir: Option<&Path>,
        label: &str,
    ) -> Result<Landed, String> {
        let moved = head != base;
        if !overlay.lock().await.has_pending_mutations() {
            return Ok(Landed::Nothing {
                generation: head,
                kept: Vec::new(),
            });
        }
        let snapshot = self
            .handle
            .snapshot_overlay(Arc::clone(&overlay))
            .await
            .map_err(stringify)?;
        let mut plan = self
            .handle
            .merge_plan(base, head, snapshot, format!("fork {id}"))
            .await
            .map_err(stringify)?;
        // Gitignored paths (bytecode caches, build output, .env) are not
        // merge payload: a fork's copy never blocks a promote, the
        // mainline keeps its own. Cheap: only the contested paths are asked.
        let contested: Vec<PathBuf> = plan
            .refusals
            .iter()
            .map(|refusal| refusal.path.clone())
            .chain(plan.conflicted.iter().map(|file| file.path.clone()))
            .collect();
        let ignored =
            tokio::task::block_in_place(|| merge::ignored_paths(&self.repo_root, &contested));
        let kept: Vec<String> = plan
            .keep_mainline_for(&ignored)
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        acyclic_engine::trace!(
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
                acyclic_engine::generation_hex(base),
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

        // M = H + (fork-only subtrees from F) + (content-merged files).
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
        let merged = self
            .handle
            .build_generation(head, entries)
            .await
            .map_err(stringify)?;

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
                        &acyclic_engine::generation_hex(head)[..12],
                        plan.conflicted.len()
                    ),
                )
                .await
                .map_err(stringify)?;
            self.rebase_fork(id, snapshot, rebased, copy_dir).await?;
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

        let target = self
            .handle
            .record_generation(
                merged,
                if moved {
                    format!(
                        "fork {id} merge ({} merged, {} replayed)",
                        plan.merged.len(),
                        plan.take_ours.len()
                    )
                } else {
                    format!("fork {id} snapshot ({} paths)", plan.take_ours.len())
                },
            )
            .await
            .map_err(stringify)?;
        self.handle
            .checkpoint(
                CheckpointKind::PreRewind,
                Attribution {
                    label: Some(if moved {
                        format!("before {label} (merge)")
                    } else {
                        format!("before {label}")
                    }),
                    ..Attribution::default()
                },
            )
            .await
            .map_err(stringify)?;
        let mut written = 0u32;
        for root in plan.landing_paths() {
            self.handle
                .restore_path(target.clone(), root.clone())
                .await
                .map_err(|error| {
                    format!(
                        "merge stopped at {} after {written} path(s): {error}. The tree is partially \
                         merged; `{NAME} rewind <id>` of the `before promote ... (merge)` row in \
                         `{NAME} timeline` returns to the pre-merge tree",
                        root.display()
                    )
                })?;
            written += 1;
        }
        let landed_label = if moved {
            format!(
                "{label} (merged {} file(s), replayed {written} path(s) onto moved mainline)",
                plan.merged.len()
            )
        } else {
            format!("{label} ({written} path(s) written in place)")
        };
        let landed = self
            .handle
            .checkpoint(
                CheckpointKind::Manual,
                Attribution {
                    label: Some(landed_label.clone()),
                    ..Attribution::default()
                },
            )
            .await
            .map_err(stringify)?;
        // Each restore_path already captured its write, so the landing
        // checkpoint above usually finds nothing new and comes back as a
        // noop row. Promote must be a real rewind target (spec I6): record
        // the landed generation as a manual row under the same label.
        if landed.kind == CheckpointKind::Noop {
            self.handle
                .record_generation(landed.generation, landed_label)
                .await
                .map_err(stringify)?;
        }
        self.handle.commit().await.map_err(stringify)?;
        Ok(Landed::Replayed {
            generation: landed.generation,
            paths: written,
            merged: plan.merged.len() as u32,
            kept,
            moved,
        })
    }

    /// Makes the fork workspace equal to `rebased`: writes R − F into the
    /// fork's directory, through the mount for a mounted fork. The real
    /// tree is never touched.
    async fn rebase_fork(
        &self,
        id: &str,
        snapshot: acyclic_engine::GenerationId,
        rebased: acyclic_engine::GenerationId,
        copy_dir: Option<&Path>,
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
        match copy_dir {
            Some(dir) => {
                for root in &roots {
                    self.handle
                        .restore_path_into(rebased, dir.to_path_buf(), root.clone())
                        .await
                        .map_err(stringify)?;
                }
            }
            None => {
                // A mounted fork: write THROUGH the mount, never behind it.
                // The driver and the kernel keep name and attribute caches
                // that only their own operations update; a write via the
                // checkout leaves a file the fork had deleted invisible for
                // good, and the FUSE transport has no invalidation at all.
                let dir = fork::forks_mount_root(&self.repo_root)
                    .ok_or("repo root has no parent for fork workspaces")?
                    .join(id);
                self.handle
                    .materialize_paths(rebased, dir, roots)
                    .await
                    .map_err(stringify)?;
            }
        }
        Ok(())
    }

    /// Drops whatever backs a fork: its route for mounted forks, its
    /// directory for copy forks.
    async fn discard_fork_workspace(
        &self,
        id: &str,
        copy_dir: Option<&Path>,
    ) -> Result<(), String> {
        match copy_dir {
            Some(dir) => {
                std::fs::remove_dir_all(dir)
                    .map_err(|error| format!("remove fork copy: {error}"))?;
                Self::remove_if_empty(dir.parent());
                Ok(())
            }
            None => self.detach_route(id).await,
        }
    }

    fn remove_if_empty(dir: Option<&Path>) {
        if let Some(dir) = dir {
            let _ = std::fs::remove_dir(dir);
            let _ = dir.parent().map(std::fs::remove_dir);
        }
    }

    /// Removes one fork's route; the session unmounts (and the mount root
    /// disappears) when the last route goes, freeing the FUSE-T pool slot.
    async fn detach_route(&self, id: &str) -> Result<(), String> {
        let mut mount = self.fork_mount.lock().await;
        // Dropping a route drops its CheckoutMountSource, which owns a tokio
        // runtime — runtimes must never be dropped on an async worker.
        tokio::task::block_in_place(|| mount.router.remove_route(id.as_bytes()));
        // The kernel may hold a positive entry cache for the removed name
        // (FSKit caches until told otherwise): invalidate it eagerly.
        if let Some(session) = mount.session.as_ref() {
            if let Err(error) = tokio::task::block_in_place(|| session.invalidate(id.as_bytes())) {
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
        let row = row.ok_or_else(|| "no matching checkpoint".to_string())?;
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
        base.ok_or_else(|| "no checkpoints yet".to_string())
    }

    /// Builds the previous-session brief: where the last session (other
    /// than `current`) ended, what it changed, and every branch it abandoned
    /// by rewinding. Diffs are computed against the store, so counts are
    /// exact rather than remembered.
    async fn brief(&self, current: Option<&str>) -> Result<proto::BriefInfo, String> {
        let index = self.open_index()?;
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
                    self.handle
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

        let mut abandoned = Vec::new();
        if let (Some(start), Some(last)) = (&start, &last_any) {
            for rewind in index
                .rewinds_between(start.id, last.id)
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
                let files = match target_row {
                    Some(target_row) if target_row.generation != last.generation => {
                        content_changes(
                            self.handle
                                .diff(target_row.generation, last.generation)
                                .await
                                .map_err(stringify)?,
                        )
                        .len() as u64
                    }
                    _ => 0,
                };
                let turn = last.turn.or(first.turn);
                let prompt = match (last.session_id.as_deref(), turn) {
                    (Some(session), Some(turn)) => index
                        .turn(session, turn)
                        .map_err(stringify)?
                        .map(|turn| turn.prompt),
                    _ => None,
                };
                abandoned.push(proto::BriefAbandoned {
                    from_checkpoint: first.id,
                    to_checkpoint: last.id,
                    rewound_to: target,
                    turn,
                    prompt,
                    checkpoints: branch.len() as i64,
                    files_changed: files,
                });
            }
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
                self.handle
                    .diff(end.generation, latest.generation)
                    .await
                    .map_err(stringify)?,
            )
            .len()
                as u64,
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
                abandoned,
            }),
            drift_files,
        })
    }
}

/// Content changes only. A rewind or restore rewrites mtimes on every path
/// it materializes, so metadata-only rows are noise for "what changed".
fn content_changes(
    changes: Vec<acyclic_engine::diff::FileChange>,
) -> Vec<acyclic_engine::diff::FileChange> {
    changes
        .into_iter()
        .filter(|change| change.change != acyclic_engine::diff::ChangeKind::MetadataOnly)
        .collect()
}

fn err(message: String) -> proto::Payload {
    proto::Payload::Err { message }
}

fn short_id() -> String {
    // UUIDv7 leads with timestamp bits (identical across nearby calls);
    // the tail is the random section.
    acyclic_engine::MountId::new().into_bytes()[10..]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// How a fork ended up in the real tree.
enum Landed {
    /// Path-by-path write onto the current head; never a directory swap.
    /// `merged` of the paths were produced by a three-way content merge;
    /// `moved` says whether the mainline had moved past the fork's base.
    Replayed {
        generation: acyclic_engine::GenerationId,
        paths: u32,
        merged: u32,
        kept: Vec<String>,
        moved: bool,
    },
    /// No content changes to land.
    Nothing {
        generation: acyclic_engine::GenerationId,
        kept: Vec<String>,
    },
    /// Nothing landed: the fork was rebased onto `theirs` and `files`
    /// carry conflict markers in the fork workspace.
    Conflicted {
        theirs: acyclic_engine::GenerationId,
        ours: acyclic_engine::GenerationId,
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
        .to_string()
}

fn reply_name(reply: &proto::Reply) -> String {
    let debug = format!("{reply:?}");
    debug
        .split([' ', '{', '('])
        .next()
        .unwrap_or("?")
        .to_string()
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

fn stringify(error: EngineError) -> String {
    error.to_string()
}

fn parse_kind(kind: &str) -> Result<CheckpointKind, String> {
    Ok(match kind {
        "pre" => CheckpointKind::Pre,
        "post" => CheckpointKind::Post,
        "manual" => CheckpointKind::Manual,
        other => return Err(format!("unknown checkpoint kind {other:?}")),
    })
}

fn diff_entry(change: acyclic_engine::diff::FileChange) -> proto::DiffEntry {
    proto::DiffEntry {
        path: change.path.display().to_string(),
        change: match change.change {
            acyclic_engine::diff::ChangeKind::Added => "added",
            acyclic_engine::diff::ChangeKind::Removed => "removed",
            acyclic_engine::diff::ChangeKind::Modified => "modified",
            acyclic_engine::diff::ChangeKind::MetadataOnly => "metadata",
        }
        .to_string(),
        file_kind: format!("{:?}", change.file_kind).to_lowercase(),
        ignored: false,
    }
}

fn timeline_entry(row: CheckpointRow) -> proto::TimelineEntry {
    proto::TimelineEntry {
        id: row.id,
        created_at: row.created_at,
        kind: row.kind.as_str().to_string(),
        published: row.published,
        session_id: row.session_id,
        tool_name: row.tool_name,
        label: row.label,
        error: row.error,
        turn: row.turn,
    }
}

fn hex_generation(generation: acyclic_engine::GenerationId) -> String {
    acyclic_engine::generation_hex(generation)
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
