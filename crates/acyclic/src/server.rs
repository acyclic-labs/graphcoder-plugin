//! The per-repo daemon: owns the pipeline, serves the unix socket.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acyclic_engine::config::Config;
use acyclic_engine::fork::{self, PromoteOutcome, SharedLocalCheckout};
use acyclic_engine::index::{Attribution, CheckpointKind, CheckpointRow, Index};
use acyclic_engine::pipeline::{self, PipelineHandle};
use acyclic_engine::store::{Store, StorePaths};
use acyclic_engine::{rewind, EngineError};
use acyclic_fs_mount::{mount_native, CheckoutMountSource, NativeMountRequest, NativeMountSession};
use acyclic_proto as proto;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};

/// One live fork: its mount session (Drop unmounts), the shared checkout the
/// mount writes into, and its wire-visible facts.
struct ForkState {
    session: NativeMountSession,
    shared: Arc<SharedLocalCheckout>,
    base: acyclic_engine::GenerationId,
    entry: proto::ForkEntry,
}

pub fn run(repo_root: &Path) -> Result<(), String> {
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
    let (handle, pipeline_thread) = pipeline::spawn(store, index, config);

    // Socket + pidfile. A stale socket from a dead daemon is removed; a live
    // one refuses the second daemon via bind failure after removal race.
    let _ = std::fs::remove_file(paths.socket());
    let listener = runtime
        .block_on(async { UnixListener::bind(paths.socket()) })
        .map_err(|error| format!("bind {}: {error}", paths.socket().display()))?;
    std::fs::write(paths.pidfile(), std::process::id().to_string())
        .map_err(|error| error.to_string())?;

    let shutdown = Arc::new(Notify::new());
    let server = Server {
        handle: handle.clone(),
        index_db: paths.index_db(),
        store_root: paths.root.clone(),
        repo_root,
        shutdown: shutdown.clone(),
        forks: Arc::new(Mutex::new(HashMap::new())),
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
    handle: PipelineHandle,
    index_db: PathBuf,
    store_root: PathBuf,
    repo_root: PathBuf,
    shutdown: Arc<Notify>,
    forks: Arc<Mutex<HashMap<String, ForkState>>>,
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
        match self.dispatch_inner(op).await {
            Ok(reply) => proto::Payload::Ok(reply),
            Err(message) => err(message),
        }
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
                };
                if wait {
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
            proto::Op::Timeline { session_id, limit } => {
                let index = self.open_index()?;
                let rows = index
                    .list(session_id.as_deref(), limit)
                    .map_err(stringify)?;
                Ok(proto::Reply::Timeline(
                    rows.into_iter().map(timeline_entry).collect(),
                ))
            }
            proto::Op::Rewind { target, path } => {
                if path.is_some() {
                    return Err("single-path restore is not implemented yet".into());
                }
                let row = self.resolve_target(target)?;
                let row_id = row.id;
                let outcome = self.handle.rewind(row).await.map_err(stringify)?;
                Ok(proto::Reply::Rewind(proto::RewindInfo {
                    restored_checkpoint: row_id,
                    old_tree: outcome.old_tree.display().to_string(),
                    warning: outcome.warning.to_string(),
                }))
            }
            proto::Op::Diff { before, after } => {
                let index = self.open_index()?;
                let before_row = match before {
                    Some(id) => index
                        .by_id(id)
                        .map_err(stringify)?
                        .ok_or(format!("no checkpoint #{id}"))?,
                    None => self.default_diff_base(&index)?,
                };
                let after_row = match after {
                    Some(id) => index
                        .by_id(id)
                        .map_err(stringify)?
                        .ok_or(format!("no checkpoint #{id}"))?,
                    None => index
                        .latest()
                        .map_err(stringify)?
                        .ok_or("no checkpoints yet")?,
                };
                let changes = self
                    .handle
                    .diff(before_row.generation, after_row.generation)
                    .await
                    .map_err(stringify)?;
                Ok(proto::Reply::Diff(
                    changes
                        .into_iter()
                        .map(|change| proto::DiffEntry {
                            path: change.path.display().to_string(),
                            change: match change.change {
                                acyclic_engine::diff::ChangeKind::Added => "added",
                                acyclic_engine::diff::ChangeKind::Removed => "removed",
                                acyclic_engine::diff::ChangeKind::Modified => "modified",
                                acyclic_engine::diff::ChangeKind::MetadataOnly => "metadata",
                            }
                            .to_string(),
                            file_kind: format!("{:?}", change.file_kind).to_lowercase(),
                        })
                        .collect(),
                ))
            }
            proto::Op::SessionStart { session_id, host } => {
                self.handle
                    .session_started(session_id, host)
                    .await
                    .map_err(stringify)?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::SessionEnd { session_id } => {
                self.handle
                    .session_ended(session_id)
                    .await
                    .map_err(stringify)?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::Commit => {
                self.handle.commit().await.map_err(stringify)?;
                Ok(proto::Reply::Unit)
            }
            proto::Op::Stop => {
                // Drop fork mounts before the pipeline goes away: their
                // callback runtimes reach into the shared checkouts.
                let mut forks = self.forks.lock().await;
                for (_, mut fork) in forks.drain() {
                    let _ = tokio::task::block_in_place(|| fork.session.stop());
                    let _ = std::fs::remove_dir_all(entry_dir(&fork.entry));
                }
                self.shutdown.notify_one();
                Ok(proto::Reply::Unit)
            }
            proto::Op::Fork { count } => {
                if count == 0 || count > 16 {
                    return Err("fork count must be 1..=16".into());
                }
                let root = fork::forks_root(&self.repo_root)
                    .ok_or("repo root has no parent for fork workspaces")?;
                let mut created = Vec::new();
                for _ in 0..count {
                    let seed = self.handle.fork().await.map_err(stringify)?;
                    let id = short_id();
                    let directory = root.join(&id);
                    if directory.exists() {
                        return Err(format!("fork workspace collision at {id}; retry"));
                    }
                    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
                    // Mount drivers block on their own runtimes; never
                    // run them on this async worker directly.
                    let shared = Arc::clone(&seed.shared);
                    let config = seed.config;
                    let volume_id = seed.volume_id;
                    let dest = directory.clone();
                    let session = tokio::task::block_in_place(move || {
                        let source = Arc::new(
                            CheckoutMountSource::new(shared, config)
                                .map_err(|error| format!("mount source: {error:?}"))?,
                        );
                        mount_native(
                            NativeMountRequest {
                                mount_id: acyclic_engine::MountId::new(),
                                volume_id,
                                destination: dest,
                                writable: true,
                            },
                            source,
                        )
                        .map_err(|error| format!("mount: {error:?}"))
                    })?;
                    let entry = proto::ForkEntry {
                        id: id.clone(),
                        path: directory.display().to_string(),
                        base: acyclic_engine::generation_hex(seed.base),
                        created_at: unix_now(),
                    };
                    self.forks.lock().await.insert(
                        id,
                        ForkState {
                            session,
                            shared: seed.shared,
                            base: seed.base,
                            entry: clone_entry(&entry),
                        },
                    );
                    created.push(entry);
                }
                Ok(proto::Reply::Forks(created))
            }
            proto::Op::ForkList => {
                let forks = self.forks.lock().await;
                let mut entries: Vec<proto::ForkEntry> =
                    forks.values().map(|fork| clone_entry(&fork.entry)).collect();
                entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                Ok(proto::Reply::Forks(entries))
            }
            proto::Op::ForkDrop { id } => {
                let mut forks = self.forks.lock().await;
                let mut fork = forks.remove(&id).ok_or(format!("no fork {id}"))?;
                tokio::task::block_in_place(|| fork.session.stop())
                    .map_err(|error| format!("unmount: {error:?}"))?;
                let _ = std::fs::remove_dir_all(entry_dir(&fork.entry));
                Ok(proto::Reply::Unit)
            }
            proto::Op::Promote { id } => {
                let mut forks = self.forks.lock().await;
                let mut fork = forks.remove(&id).ok_or(format!("no fork {id}"))?;
                // Detach the mount first: no more writes can race the commit.
                tokio::task::block_in_place(|| fork.session.stop())
                    .map_err(|error| format!("unmount: {error:?}"))?;
                drop(forks);

                let outcome = self
                    .handle
                    .promote(
                        Arc::clone(&fork.shared),
                        fork.base,
                        format!("promote fork {id}"),
                    )
                    .await
                    .map_err(stringify)?;
                let _ = std::fs::remove_dir_all(entry_dir(&fork.entry));
                match outcome {
                    PromoteOutcome::Promoted { generation, old_tree } => {
                        Ok(proto::Reply::Promote(proto::PromoteInfo {
                            generation: acyclic_engine::generation_hex(generation),
                            old_tree: old_tree.map(|path| path.display().to_string()),
                            warning:
                                "reload your editor: open files still point at the replaced tree"
                                    .into(),
                        }))
                    }
                    PromoteOutcome::Conflict { message } => Err(message),
                }
            }
        }
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
                "checkpoint #{} records a failed capture, not a tree state; pick another from `acyclic timeline`",
                row.id
            ));
        }
        Ok(row)
    }

    /// Default diff base: the most recent session's first checkpoint, falling
    /// back to the oldest checkpoint on record.
    fn default_diff_base(&self, index: &Index) -> Result<CheckpointRow, String> {
        let rows = index.list(None, 10_000).map_err(stringify)?;
        let base = rows
            .iter()
            .rev()
            .find(|row| row.session_id.is_some())
            .and_then(|row| row.session_id.clone())
            .and_then(|session| index.session_start(&session).ok().flatten())
            .or_else(|| rows.last().cloned());
        base.ok_or_else(|| "no checkpoints yet".to_string())
    }
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

fn clone_entry(entry: &proto::ForkEntry) -> proto::ForkEntry {
    proto::ForkEntry {
        id: entry.id.clone(),
        path: entry.path.clone(),
        base: entry.base.clone(),
        created_at: entry.created_at,
    }
}

fn entry_dir(entry: &proto::ForkEntry) -> PathBuf {
    PathBuf::from(&entry.path)
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
