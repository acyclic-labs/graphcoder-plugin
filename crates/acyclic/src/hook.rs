//! Host-hook entrypoint. Claude Code (and compatible hosts) invoke
//! `{NAME} hook <event>` with a JSON payload on stdin. The contract:
//! NEVER block or fail the agent — every path exits 0, a missing daemon is
//! a silent no-op, and pre-tool waits are bounded. The one exception is
//! `session-start`: it runs once per session, before any edit, and the host
//! waits for it anyway, so it starts the daemon if none is running. Without
//! that, a session begun after a reboot would never be checkpointed.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use acyclic_engine::product::NAME;
use acyclic_proto as proto;

use crate::client::{Client, ConnectError, Spawn};

/// Host hook payload (superset-tolerant: unknown fields ignored). Claude
/// Code and Codex both use `session_id`/`tool_name`/`tool_use_id`; Cursor
/// instead sends `conversation_id` and, for `beforeShellExecution`, a bare
/// `command` string with no tool name at all.
#[derive(Debug, serde::Deserialize, Default)]
struct Payload {
    #[serde(default)]
    session_id: Option<String>,
    /// Cursor's stand-in for `session_id` on most events.
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    /// `UserPromptSubmit` (Cursor: `beforeSubmitPrompt`): the prompt text.
    #[serde(default)]
    prompt: Option<String>,
    /// `SessionStart`: "startup" | "resume" | "clear" | "compact".
    #[serde(default)]
    source: Option<String>,
    /// Cursor's `beforeShellExecution`/`afterShellExecution`: the command
    /// run, standing in for `tool_name` when that field is absent.
    #[serde(default)]
    command: Option<String>,
}

impl Payload {
    fn session(&mut self) -> Option<String> {
        self.session_id
            .take()
            .or_else(|| self.conversation_id.take())
    }

    fn tool(&mut self) -> Option<String> {
        self.tool_name
            .take()
            .or_else(|| self.command.take().map(|_| "Bash".to_owned()))
    }
}

/// The bound on a pre-tool wait: an exact boundary is nice to have, but the
/// agent's latency matters more. Past this, the capture still lands (FIFO),
/// just labeled by enqueue order rather than a strict barrier.
const PRE_TOOL_WAIT: Duration = Duration::from_millis(2_000);

/// How long a session start waits for a daemon that is still coming up.
/// A warm daemon answers in a few milliseconds; a cold one is building its
/// first snapshot, which can take minutes on a large tree, and the agent's
/// first turn must not wait for that.
const SESSION_START_WAIT: Duration = Duration::from_millis(300);

fn print_starting_notice() {
    // Stdout lands in the agent's context, like the brief would.
    println!(
        "{NAME}: building the first snapshot of this tree in the background; checkpoints \
         start once `{NAME} status` says ready, and `{NAME} brief` has the previous session."
    );
}

fn is_timeout(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timed out") || lower.contains("timeout") || lower.contains("would block")
}

/// The lifecycle events a host adapter wires `acyclic hook <event>` to.
/// The CLI argument form (`pre-tool`, ...) is what the adapters write into
/// hook config, so it is derived here rather than spelled in `install.rs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookEvent {
    PreTool,
    PostTool,
    UserPrompt,
    SessionStart,
    SessionEnd,
}

impl HookEvent {
    pub const ALL: [Self; 5] = [
        Self::PreTool,
        Self::PostTool,
        Self::UserPrompt,
        Self::SessionStart,
        Self::SessionEnd,
    ];

    /// The argument as `acyclic hook` accepts it.
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::PreTool => "pre-tool",
            Self::PostTool => "post-tool",
            Self::UserPrompt => "user-prompt",
            Self::SessionStart => "session-start",
            Self::SessionEnd => "session-end",
        }
    }

    /// Parsed here, not by clap: a config written by a newer or older
    /// release may name an event this binary does not know, and clap's
    /// usage error exits 2 — which Claude Code reads as "block the tool".
    /// Unknown events must stay a silent exit 0 like every other hook path.
    pub fn parse(arg: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_arg() == arg)
    }
}

pub fn run(repo: &Path, event: &str, host: Option<String>) -> i32 {
    let Some(event) = HookEvent::parse(event) else {
        acyclic_engine::trace!("hook", "unknown event {event:?}: ignored");
        return 0;
    };
    // Reading stdin can't hang the agent: hosts close it after writing.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let mut payload = parse_payload(&raw);
    let host = host
        .or_else(|| std::env::var("ACYCLIC_HOST").ok())
        .unwrap_or_else(|| "claude-code".into());

    // A session start may spawn the daemon, but never waits for its first
    // snapshot: the agent's first turn is behind this hook.
    let spawn = if event == HookEvent::SessionStart {
        Spawn::AllowedFor(SESSION_START_WAIT)
    } else {
        Spawn::Never
    };
    acyclic_engine::trace!(
        "hook",
        "event {}: daemon spawn {}; pre-tool waits (bounded), post-tool enqueues (ack before capture)",
        event.as_arg(),
        if matches!(spawn, Spawn::Allowed) { "allowed" } else { "never" }
    );
    let mut client = match connect(repo, spawn) {
        Ok(client) => client,
        Err(ConnectError::Starting) => {
            print_starting_notice();
            return 0;
        }
        // No daemon (not initialized, or stopped): checkpointing is off.
        // Stay quiet — hooks fire on every tool call.
        Err(_) => return 0,
    };

    let op = match event {
        HookEvent::PreTool => proto::Op::Checkpoint {
            kind: proto::CheckpointRequestKind::Pre,
            session_id: payload.session(),
            tool_call_id: payload.tool_use_id.take(),
            tool_name: payload.tool(),
            label: None,
            wait: true,
            durable: false,
        },
        HookEvent::PostTool => proto::Op::Checkpoint {
            kind: proto::CheckpointRequestKind::Post,
            session_id: payload.session(),
            tool_call_id: payload.tool_use_id.take(),
            tool_name: payload.tool(),
            label: None,
            wait: false,
            durable: false,
        },
        HookEvent::UserPrompt => proto::Op::TurnStart {
            session_id: payload.session().unwrap_or_default(),
            prompt: payload.prompt.unwrap_or_default(),
        },
        HookEvent::SessionStart => {
            let session_id = payload.session().unwrap_or_default();
            // A daemon mid-baseline (first snapshot, or a recovery rescan)
            // answers when it is Ready; it still registers the session
            // then. Do not hold the agent's first turn for it.
            client.set_deadline(SESSION_START_WAIT);
            let registered = client.call(proto::Op::SessionStart {
                session_id: session_id.clone(),
                host,
            });
            if let Err(message) = registered {
                if is_timeout(&message) {
                    print_starting_notice();
                } else {
                    eprintln!("{NAME} hook (session-start): {message}");
                }
                return 0;
            }
            // Stdout of a SessionStart hook lands in the agent's context:
            // hand it the previous session's end state and abandoned
            // branches. A compaction restart already has that context.
            if payload.source.as_deref() != Some("compact") {
                match client.call(proto::Op::Brief {
                    current: Some(session_id),
                }) {
                    Ok(proto::Reply::Brief(info)) => print!("{}", crate::brief::render(&info)),
                    Ok(_) => {}
                    Err(message) => eprintln!("{NAME} hook (session-start brief): {message}"),
                }
            }
            return 0;
        }
        HookEvent::SessionEnd => proto::Op::SessionEnd {
            session_id: payload.session().unwrap_or_default(),
        },
    };

    if event == HookEvent::PreTool {
        client.set_deadline(PRE_TOOL_WAIT);
    }
    if let Err(message) = client.call(op) {
        // Deadline overruns and daemon hiccups are advisory only.
        eprintln!("{NAME} hook ({}): {message}", event.as_arg());
    }
    // Cursor's permission-controlled hooks (beforeShellExecution,
    // beforeSubmitPrompt) require a JSON response on stdout. Claude Code
    // injects UserPromptSubmit stdout into the conversation as context, so
    // this must stay Cursor-only rather than firing for every host.
    if host == "cursor" && matches!(event, HookEvent::PreTool | HookEvent::UserPrompt) {
        println!("{{\"permission\":\"allow\"}}");
    }
    0
}

/// Malformed or empty payloads degrade to attribution-less checkpoints —
/// a checkpoint with no session id beats a dropped one.
fn parse_payload(raw: &str) -> Payload {
    serde_json::from_str(raw).unwrap_or_default()
}

fn connect(repo: &Path, spawn: Spawn) -> Result<Client, ConnectError> {
    let paths = crate::store_paths(repo).map_err(ConnectError::Other)?;
    let log = paths.root.join("daemon.log");
    Client::connect(&paths.socket(), repo, &log, spawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_claude_code_payload_parses() {
        let payload = parse_payload(
            r#"{"session_id":"abc","transcript_path":"/x","cwd":"/y",
                "hook_event_name":"PostToolUse","tool_name":"Edit",
                "tool_use_id":"toolu_01","tool_input":{"file_path":"/z"},
                "tool_response":{"ok":true}}"#,
        );
        assert_eq!(payload.session_id.as_deref(), Some("abc"));
        assert_eq!(payload.tool_name.as_deref(), Some("Edit"));
        assert_eq!(payload.tool_use_id.as_deref(), Some("toolu_01"));
    }

    #[test]
    fn user_prompt_payload_carries_the_prompt() {
        let payload = parse_payload(
            r#"{"session_id":"abc","hook_event_name":"UserPromptSubmit",
                "prompt":"fix the JWT refactor"}"#,
        );
        assert_eq!(payload.prompt.as_deref(), Some("fix the JWT refactor"));
        let start = parse_payload(r#"{"session_id":"abc","source":"compact"}"#);
        assert_eq!(start.source.as_deref(), Some("compact"));
    }

    #[test]
    fn cursor_payload_falls_back_to_conversation_id_and_command() {
        let mut payload = parse_payload(
            r#"{"conversation_id":"c1","command":"ls -la","cwd":"/x",
                "hook_event_name":"beforeShellExecution"}"#,
        );
        assert_eq!(payload.session_id, None);
        assert_eq!(payload.session(), Some("c1".to_owned()));
        assert_eq!(payload.tool(), Some("Bash".to_owned()));
    }

    #[test]
    fn session_prefers_session_id_over_conversation_id() {
        let mut payload = parse_payload(r#"{"session_id":"s1","conversation_id":"c1"}"#);
        assert_eq!(payload.session(), Some("s1".to_owned()));
    }

    #[test]
    fn garbage_and_empty_payloads_degrade_to_default() {
        for raw in ["", "not json", "[1,2,3]", "{\"session_id\": 7}"] {
            let payload = parse_payload(raw);
            assert!(payload.session_id.is_none(), "raw: {raw}");
            assert!(payload.tool_name.is_none());
        }
    }
}
