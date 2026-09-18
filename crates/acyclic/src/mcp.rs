//! `acyclic mcp` — an MCP stdio server for hosts that speak MCP: the ones
//! with no lifecycle-hook API at all (Claude Desktop, VS Code) and the ones
//! that support it alongside hooks (Cursor). Exposes the same verbs already
//! surfaced to every other host via `AGENTS_MD_BLOCK`/`SELF_ROLLBACK_SKILL`
//! (see `install.rs`) as MCP tools, translating each call directly into the
//! `proto::Op` the daemon already understands. The MCP adapters in
//! `install.rs` register it with each host; `tests/acceptance/mcp-e2e.sh`
//! drives it end-to-end.
//!
//! Unlike the hook path, there is no per-tool-call event to piggyback on: a
//! checkpoint here happens because the model called `checkpoint` (steered
//! by each tool's description text below), or because the daemon's idle
//! timer noticed quiet after changes (`auto_checkpoint_idle_ms`).
//!
//! Every daemon round trip is synchronous socket I/O (and the first one may
//! spawn the daemon and wait for its baseline), so each tool runs its work
//! on tokio's blocking pool rather than the async worker that serves the
//! transport.

use std::path::{Path, PathBuf};

use crate::proto;
use acyclic::product::{self, NAME};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{ErrorCode, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::client::{Client, ConnectError, Spawn};

/// Serves stdio until the host closes stdin. Owns its runtime: the CLI is
/// otherwise synchronous, and this is the one command that lives as long
/// as the host process.
pub fn run(repo: PathBuf) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let service = McpServer { repo }
            .serve(stdio())
            .await
            .map_err(|error| error.to_string())?;
        service.waiting().await.map_err(|error| error.to_string())?;
        Ok::<(), String>(())
    })
}

/// The repo an MCP host meant, when it passed no `--repo`: the nearest
/// ancestor of `start` (itself included) that has a store, else `start`.
/// Checked-in configs cannot carry a path (they are shared across clones)
/// and hosts do not agree on the server's working directory — cursor-agent
/// starts it in the shell's cwd, which may be a subdirectory — so the
/// server finds the root the way git finds `.git`.
pub fn find_repo_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|dir| crate::store_paths(dir).is_ok_and(|paths| paths.meta().exists()))
        .map_or_else(|| start.to_path_buf(), Path::to_path_buf)
}

fn internal_error(message: String) -> McpError {
    McpError::new(ErrorCode::INTERNAL_ERROR, message, None)
}

/// Connect to the repo's daemon, spawning it if it isn't running yet — the
/// same "nothing running" case `acyclic init`/any interactive command
/// already handles, not the hook path's `Spawn::Never`.
fn connect(repo: &Path) -> Result<Client, McpError> {
    let paths = crate::store_paths(repo).map_err(internal_error)?;
    let log = paths.root.join("daemon.log");
    Client::connect(&paths.socket(), repo, &log, Spawn::Allowed).map_err(|error| match error {
        ConnectError::NoDaemon => {
            internal_error("daemon not running and could not be started".into())
        }
        ConnectError::Starting => internal_error("daemon is still starting".into()),
        ConnectError::Other(message) => internal_error(message),
    })
}

fn call(client: &mut Client, op: proto::Op) -> Result<proto::Reply, McpError> {
    client.call(op).map_err(internal_error)
}

/// Runs one tool's daemon conversation on the blocking pool: connect, then
/// whatever `work` does with the client. Keeps the socket I/O (and a
/// possible daemon spawn + baseline wait) off the async worker.
async fn with_daemon<T, F>(repo: PathBuf, work: F) -> Result<T, McpError>
where
    T: Send + 'static,
    F: FnOnce(&mut Client) -> Result<T, McpError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut client = connect(&repo)?;
        work(&mut client)
    })
    .await
    .map_err(|error| internal_error(format!("tool task failed: {error}")))?
}

/// One path of a `restore`: what happened, and the checkpoint the daemon
/// recorded afterwards (the same thing the CLI prints), so the caller can
/// refer to the post-restore state without another `timeline` call.
fn restore_one(client: &mut Client, checkpoint: i64, path: String) -> Result<String, McpError> {
    let info = client.restore(checkpoint, path).map_err(internal_error)?;
    let what = match info.action {
        proto::RestoreAction::Removed => format!("absent at #{}, removed", info.checkpoint),
        _ => format!("restored from #{}", info.checkpoint),
    };
    let recorded = info
        .recorded_checkpoint
        .map_or(String::new(), |row| format!(" (recorded as #{row})"));
    Ok(format!("{}: {what}{recorded}", info.path))
}

/// `turns` shows this many of the newest turns.
const TURNS_SHOWN: usize = 50;

#[derive(Clone)]
struct McpServer {
    repo: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CheckpointParams {
    /// Short label for this checkpoint, shown in `timeline`.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TimelineParams {
    /// Maximum number of checkpoints to return (default 50).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RewindParams {
    /// Checkpoint id from `timeline`. Call `timeline` first and confirm the
    /// target with the user before calling this — it replaces the whole
    /// working tree, though a safety checkpoint of the current state is
    /// taken automatically first.
    checkpoint: i64,
    /// Must be true, and only after the user has agreed to rewind to this
    /// checkpoint. The CLI asks interactively (`--yes` skips it); this is the
    /// MCP equivalent, so a host that auto-approves tool calls still cannot
    /// rewind without the model stating that consent explicitly.
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffParams {
    /// Earlier checkpoint id. Defaults to the session/baseline start.
    #[serde(default)]
    before: Option<i64>,
    /// Later checkpoint id. Defaults to the latest checkpoint.
    #[serde(default)]
    after: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SummaryParams {
    /// Session to summarise a turn of. Defaults to the most recent one.
    #[serde(default)]
    session_id: Option<String>,
    /// Turn number. Defaults to the last turn that finished.
    #[serde(default)]
    turn: Option<i64>,
    /// Milliseconds to wait if a summary is being produced right now. Up to
    /// a few seconds is reasonable here; zero never waits.
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreParams {
    /// Checkpoint id to restore from.
    checkpoint: i64,
    /// Repo-relative paths to bring back, at least one; everything else is
    /// left untouched.
    paths: Vec<String>,
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Snapshot the working tree now (untracked and gitignored files included). \
                       Call this before a risky change so a bad attempt can be rewound instead \
                       of hand-reverted.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn checkpoint(
        &self,
        Parameters(params): Parameters<CheckpointParams>,
    ) -> Result<String, McpError> {
        with_daemon(self.repo.clone(), move |client| {
            let info = client
                .manual_checkpoint(params.message)
                .map_err(internal_error)?;
            Ok(format!("checkpoint #{} ({})", info.row_id, info.generation))
        })
        .await
    }

    #[tool(
        description = "List recent checkpoints, newest first. Each has an id you pass to rewind, diff, or restore.",
        annotations(read_only_hint = true)
    )]
    async fn timeline(
        &self,
        Parameters(params): Parameters<TimelineParams>,
    ) -> Result<String, McpError> {
        with_daemon(self.repo.clone(), move |client| {
            let entries = client
                .timeline(None, None, params.limit.unwrap_or(50))
                .map_err(internal_error)?;
            if entries.is_empty() {
                return Ok("no checkpoints yet".into());
            }
            let mut lines = Vec::with_capacity(entries.len());
            for entry in entries {
                let label = entry
                    .label
                    .or(entry.tool_name)
                    .or(entry.error.map(|error| format!("error: {error}")))
                    .unwrap_or_default();
                let turn = entry
                    .turn
                    .map(|turn| format!(" t{turn}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "#{} {} {}{} {}",
                    entry.id,
                    crate::age(entry.created_at),
                    entry.kind,
                    turn,
                    label
                ));
            }
            Ok(lines.join("\n"))
        })
        .await
    }

    #[tool(
        description = "Which prompt/turn caused which checkpoints, across sessions.",
        annotations(read_only_hint = true)
    )]
    async fn turns(&self) -> Result<String, McpError> {
        with_daemon(self.repo.clone(), |client| {
            let reply = call(client, proto::Op::Turns { session_id: None })?;
            let proto::Reply::Turns(turns) = reply else {
                return Err(internal_error("unexpected reply".into()));
            };
            if turns.is_empty() {
                return Ok("no turns recorded yet".into());
            }
            let mut lines = Vec::with_capacity(turns.len());
            // Newest TURNS_SHOWN turns (the daemon lists oldest first),
            // prompts clipped: a long history of raw prompts would crowd the
            // host's context. The session prefix is what tells `t1` of one
            // session from another's.
            for turn in turns.into_iter().rev().take(TURNS_SHOWN) {
                let range = match (turn.first_checkpoint, turn.last_checkpoint) {
                    (Some(first), Some(last)) if first != last => format!("#{first}..#{last}"),
                    (Some(first), _) => format!("#{first}"),
                    _ => "no checkpoints".to_owned(),
                };
                lines.push(format!(
                    "{} t{} {} {} {}",
                    crate::short_session(&turn.session_id),
                    turn.turn,
                    crate::age(turn.started_at),
                    range,
                    crate::brief::quote(&turn.prompt, 72)
                ));
            }
            Ok(lines.join("\n"))
        })
        .await
    }

    #[tool(
        description = "Restore the working tree exactly to an earlier checkpoint (including \
                       untracked and gitignored files). Call `timeline` first, confirm the \
                       target checkpoint with the user, then call this with confirm=true — \
                       without it the call is refused. A safety checkpoint of the current \
                       state is taken automatically first.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn rewind(
        &self,
        Parameters(params): Parameters<RewindParams>,
    ) -> Result<String, McpError> {
        if !params.confirm {
            return Err(McpError::invalid_params(
                "rewind replaces the whole working tree: confirm the target with the user, \
                 then call again with confirm=true",
                None,
            ));
        }
        with_daemon(self.repo.clone(), move |client| {
            let info = client
                .rewind(proto::RewindTarget::Checkpoint(params.checkpoint))
                .map_err(internal_error)?;
            Ok(format!(
                "restored checkpoint #{}\nold tree kept at {}\nnote: {}",
                info.restored_checkpoint, info.old_tree, info.warning
            ))
        })
        .await
    }

    #[tool(
        description = "Show everything that changed between two checkpoints (defaults: \
                       session/baseline start to latest) — the blast radius of a session.",
        annotations(read_only_hint = true)
    )]
    async fn diff(&self, Parameters(params): Parameters<DiffParams>) -> Result<String, McpError> {
        with_daemon(self.repo.clone(), move |client| {
            let entries = client
                .diff(params.before, params.after, None, None)
                .map_err(internal_error)?;
            if entries.is_empty() {
                return Ok("no changes".into());
            }
            let mut lines = Vec::with_capacity(entries.len());
            for entry in entries {
                let ignored = if entry.ignored { " (gitignored)" } else { "" };
                lines.push(format!("{} {}{}", entry.change, entry.path, ignored));
            }
            Ok(lines.join("\n"))
        })
        .await
    }

    #[tool(
        description = "Bring back one or more files from an earlier checkpoint, leaving the rest \
                       of the tree untouched. Paths are restored one at a time and each is \
                       recorded as its own checkpoint, so if one fails the earlier ones stay \
                       restored; the error lists which were done and which were not attempted.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn restore(
        &self,
        Parameters(params): Parameters<RestoreParams>,
    ) -> Result<String, McpError> {
        if params.paths.is_empty() {
            return Err(McpError::invalid_params(
                "paths must name at least one file",
                None,
            ));
        }
        with_daemon(self.repo.clone(), move |client| {
            let mut done = Vec::with_capacity(params.paths.len());
            let mut paths = params.paths.into_iter();
            for path in paths.by_ref() {
                match restore_one(client, params.checkpoint, path.clone()) {
                    Ok(line) => done.push(line),
                    Err(error) => {
                        let remaining: Vec<String> = paths.collect();
                        return Err(internal_error(format!(
                            "{path}: {}\nalready restored: {}\nnot attempted: {}",
                            error.message,
                            if done.is_empty() {
                                "none".to_owned()
                            } else {
                                done.join("; ")
                            },
                            if remaining.is_empty() {
                                "none".to_owned()
                            } else {
                                remaining.join(", ")
                            },
                        )));
                    }
                }
            }
            Ok(done.join("\n"))
        })
        .await
    }

    #[tool(
        description = "Call this once at the start of a conversation grounded in this repo: \
                       summarizes where the previous session left off and any abandoned branches.",
        annotations(read_only_hint = true)
    )]
    async fn brief(&self) -> Result<String, McpError> {
        with_daemon(self.repo.clone(), |client| {
            let reply = call(client, proto::Op::Brief { current: None })?;
            let proto::Reply::Brief(info) = reply else {
                return Err(internal_error("unexpected reply".into()));
            };
            Ok(crate::brief::render(&info))
        })
        .await
    }

    #[tool(
        description = "What one conversation turn changed, in prose. Usually instant: the \
                       daemon writes these at turn boundaries, before anything asks. Returns \
                       'no summary' rather than producing one on demand — summaries cost the \
                       developer money, so asking is not permission to spend. The reply says \
                       where the words came from; treat them as a generated summary, not as a \
                       record of what happened.",
        annotations(read_only_hint = true)
    )]
    async fn summary(
        &self,
        Parameters(params): Parameters<SummaryParams>,
    ) -> Result<String, McpError> {
        with_daemon(self.repo.clone(), move |client| {
            let reply = call(
                client,
                proto::Op::Summary {
                    session_id: params.session_id,
                    turn: params.turn,
                    wait_ms: params.wait_ms.unwrap_or(0),
                },
            )?;
            let proto::Reply::Summary(info) = reply else {
                return Err(internal_error("unexpected reply".into()));
            };
            let prompt = crate::brief::quote(&info.prompt, 100);
            Ok(match info.text {
                Some(text) => format!(
                    "turn {} ({prompt}) — {} summary:\n{text}",
                    info.turn, info.source
                ),
                None => format!(
                    "turn {} ({prompt}): no summary ({})",
                    info.turn, info.source
                ),
            })
        })
        .await
    }
}

const MCP_INSTRUCTIONS: &str = "\
This repo uses {{name}}: the working tree (untracked + gitignored files \
included) is snapshotted into checkpoints. No hook fires per tool call here \
the way it does in a hooked host — the daemon checkpoints on its own once \
edits go quiet, but call `checkpoint` yourself before a risky change so the \
boundary is exact. Call `brief` once at the start of a conversation grounded \
in this repo. After a failed attempt, call `rewind` instead of hand-reverting \
— but call `timeline` first and confirm the target checkpoint with the user, \
since there is no interactive confirmation prompt here. Before finishing, \
call `diff` and review the blast radius.";

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(product::render(MCP_INSTRUCTIONS))
    }
}
