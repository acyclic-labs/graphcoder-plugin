//! `acyclic mcp` — an MCP stdio server for hosts with no lifecycle-hook API
//! (Claude Desktop). Exposes the same verbs already surfaced to every other
//! host via `AGENTS_MD_BLOCK`/`SELF_ROLLBACK_SKILL` (see `install.rs`) as
//! MCP tools, translating each call directly into the `acyclic-proto::Op`
//! the daemon already understands. See `install.rs`'s `ClaudeDesktop`
//! adapter for how this gets registered with the host.
//!
//! Unlike the hook path, there is no lifecycle event to piggyback on: every
//! checkpoint here happens because the model decided to call `checkpoint`,
//! steered by each tool's description text below.

use std::path::PathBuf;

use acyclic_engine::product::{self, NAME};
use acyclic_proto as proto;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{ErrorCode, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::client::{Client, ConnectError, Spawn};

pub async fn run(repo: PathBuf) -> Result<(), String> {
    let server = McpServer { repo };
    let service = server.serve(stdio()).await.map_err(|error| error.to_string())?;
    service.waiting().await.map_err(|error| error.to_string())?;
    Ok(())
}

fn internal_error(message: String) -> McpError {
    McpError::new(ErrorCode::INTERNAL_ERROR, message, None)
}

fn call(client: &mut Client, op: proto::Op) -> Result<proto::Reply, McpError> {
    client.call(op).map_err(internal_error)
}

#[derive(Clone)]
struct McpServer {
    repo: PathBuf,
}

impl McpServer {
    /// Connect to the repo's daemon, spawning it if it isn't running yet —
    /// the same "nothing running" case `acyclic init`/any interactive
    /// command already handles, not the hook path's `Spawn::Never`.
    fn connect(&self) -> Result<Client, McpError> {
        let paths = crate::store_paths(&self.repo).map_err(internal_error)?;
        let log = paths.root.join("daemon.log");
        Client::connect(&paths.socket(), &self.repo, &log, Spawn::Allowed).map_err(|error| match error {
            ConnectError::NoDaemon => internal_error("daemon not running and could not be started".into()),
            ConnectError::Other(message) => internal_error(message),
        })
    }
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
struct RestoreParams {
    /// Checkpoint id to restore from.
    checkpoint: i64,
    /// Repo-relative paths to bring back; everything else is left untouched.
    paths: Vec<String>,
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Snapshot the working tree now (untracked and gitignored files included). Call this before a risky change so a bad attempt can be rewound instead of hand-reverted."
    )]
    async fn checkpoint(
        &self,
        Parameters(params): Parameters<CheckpointParams>,
    ) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let reply = call(
            &mut client,
            proto::Op::Checkpoint {
                kind: "manual".into(),
                session_id: None,
                tool_call_id: None,
                tool_name: None,
                label: params.message,
                wait: true,
                durable: false,
            },
        )?;
        let proto::Reply::Checkpoint(info) = reply else {
            return Err(internal_error("unexpected reply".into()));
        };
        Ok(format!("checkpoint #{} ({})", info.row_id, info.generation))
    }

    #[tool(
        description = "List recent checkpoints, newest first. Each has an id you pass to rewind, diff, or restore."
    )]
    async fn timeline(
        &self,
        Parameters(params): Parameters<TimelineParams>,
    ) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let reply = call(
            &mut client,
            proto::Op::Timeline {
                session_id: None,
                turn: None,
                limit: params.limit.unwrap_or(50),
            },
        )?;
        let proto::Reply::Timeline(entries) = reply else {
            return Err(internal_error("unexpected reply".into()));
        };
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
            let turn = entry.turn.map(|turn| format!(" t{turn}")).unwrap_or_default();
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
    }

    #[tool(description = "Which prompt/turn caused which checkpoints, across sessions.")]
    async fn turns(&self) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let reply = call(&mut client, proto::Op::Turns { session_id: None })?;
        let proto::Reply::Turns(turns) = reply else {
            return Err(internal_error("unexpected reply".into()));
        };
        if turns.is_empty() {
            return Ok("no turns recorded yet".into());
        }
        let mut lines = Vec::with_capacity(turns.len());
        for turn in turns {
            let range = match (turn.first_checkpoint, turn.last_checkpoint) {
                (Some(first), Some(last)) if first != last => format!("#{first}..#{last}"),
                (Some(first), _) => format!("#{first}"),
                _ => "no checkpoints".to_string(),
            };
            lines.push(format!(
                "t{} {} {} {}",
                turn.turn,
                crate::age(turn.started_at),
                range,
                turn.prompt
            ));
        }
        Ok(lines.join("\n"))
    }

    #[tool(
        description = "Restore the working tree exactly to an earlier checkpoint (including untracked and gitignored files). Call `timeline` first and confirm the target checkpoint with the user before calling this — a safety checkpoint of the current state is taken automatically first."
    )]
    async fn rewind(&self, Parameters(params): Parameters<RewindParams>) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let reply = call(
            &mut client,
            proto::Op::Rewind {
                target: proto::RewindTarget::Checkpoint(params.checkpoint),
                path: None,
            },
        )?;
        let proto::Reply::Rewind(info) = reply else {
            return Err(internal_error("unexpected reply".into()));
        };
        Ok(format!(
            "restored checkpoint #{}\nold tree kept at {}\nnote: {}",
            info.restored_checkpoint, info.old_tree, info.warning
        ))
    }

    #[tool(
        description = "Show everything that changed between two checkpoints (defaults: session/baseline start to latest) — the blast radius of a session."
    )]
    async fn diff(&self, Parameters(params): Parameters<DiffParams>) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let reply = call(
            &mut client,
            proto::Op::Diff {
                before: params.before,
                after: params.after,
                before_hex: None,
                after_hex: None,
            },
        )?;
        let proto::Reply::Diff(entries) = reply else {
            return Err(internal_error("unexpected reply".into()));
        };
        if entries.is_empty() {
            return Ok("no changes".into());
        }
        let mut lines = Vec::with_capacity(entries.len());
        for entry in entries {
            let ignored = if entry.ignored { " (gitignored)" } else { "" };
            lines.push(format!("{} {}{}", entry.change, entry.path, ignored));
        }
        Ok(lines.join("\n"))
    }

    #[tool(
        description = "Bring back one or more files from an earlier checkpoint, leaving the rest of the tree untouched. Each restore is itself recorded as a checkpoint."
    )]
    async fn restore(&self, Parameters(params): Parameters<RestoreParams>) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let mut lines = Vec::with_capacity(params.paths.len());
        for path in params.paths {
            let reply = call(
                &mut client,
                proto::Op::Rewind {
                    target: proto::RewindTarget::Checkpoint(params.checkpoint),
                    path: Some(path),
                },
            )?;
            let proto::Reply::Restore(info) = reply else {
                return Err(internal_error("unexpected reply".into()));
            };
            match info.action.as_str() {
                "removed" => lines.push(format!("{}: absent at #{}, removed", info.path, info.checkpoint)),
                _ => lines.push(format!("{}: restored from #{}", info.path, info.checkpoint)),
            }
        }
        Ok(lines.join("\n"))
    }

    #[tool(
        description = "Call this once at the start of a conversation grounded in this repo: summarizes where the previous session left off and any abandoned branches."
    )]
    async fn brief(&self) -> Result<String, McpError> {
        let mut client = self.connect()?;
        let reply = call(&mut client, proto::Op::Brief { current: None })?;
        let proto::Reply::Brief(info) = reply else {
            return Err(internal_error("unexpected reply".into()));
        };
        Ok(crate::brief::render(&info))
    }
}

const MCP_INSTRUCTIONS: &str = "\
This repo uses {{name}}: the working tree (untracked + gitignored files \
included) is snapshotted on request — nothing here fires automatically the \
way it does in a hooked host, so call `checkpoint` yourself before a risky \
change. Call `brief` once at the start of a conversation grounded in this \
repo. After a failed attempt, call `rewind` instead of hand-reverting — but \
call `timeline` first and confirm the target checkpoint with the user, since \
there is no interactive confirmation prompt here. Before finishing, call \
`diff` and review the blast radius.";

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(product::render(MCP_INSTRUCTIONS))
    }
}
