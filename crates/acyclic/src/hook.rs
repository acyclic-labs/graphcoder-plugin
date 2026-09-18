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
    /// `PreToolUse`: what the tool is about to touch. Edit/Write/MultiEdit
    /// name a path here; Bash does not, and a shell command can touch
    /// anything — which is why a missing path records a wildcard.
    #[serde(default)]
    tool_input: Option<ToolInput>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ToolInput {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    path: Option<String>,
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

pub fn run(repo: &Path, event: &str) -> i32 {
    let Some(event) = HookEvent::parse(event) else {
        acyclic_engine::trace!("hook", "unknown event {event:?}: ignored");
        return 0;
    };
    // Reading stdin can't hang the agent: hosts close it after writing.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let mut payload = parse_payload(&raw);
    let host = std::env::var("ACYCLIC_HOST").unwrap_or_else(|_| "claude-code".into());

    // Before the connect, deliberately. A lease says what the agent is about
    // to touch, and that is worth recording whether or not a daemon is up —
    // the connect returns early when there is none, so recording afterwards
    // would silently stop working exactly when checkpointing is off.
    if event == HookEvent::PreTool {
        let path = payload
            .tool_input
            .as_ref()
            .and_then(|i| i.file_path.as_deref().or(i.path.as_deref()));
        record_lease(repo, payload.tool_name.as_deref(), path);
    }

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

/// Record what the agent is *about* to touch, for anything scheduling work
/// alongside it.
///
/// Two decisions here, both forced by measurement.
///
/// **It rides on the pre-tool hook** rather than being a hook of its own. A
/// separate hook process measured ~10ms at best against this binary's own
/// ~10ms, so a second hook roughly doubles what every Edit, Write and Bash
/// pays — to record a path this process is already holding. Here the marginal
/// cost is one append.
///
/// **It writes into the STORE, not the repo.** The obvious placement,
/// `<repo>/.speculation/leases`, took the pre-tool hook from 65ms to 120ms with
/// a daemon running: a write inside the tree wakes the watcher, and this very
/// hook then waits for the resulting checkpoint. The lease write became work
/// the lease writer waited on. It would also have shown up in every blast
/// radius as a changed path. Outside the tree, neither happens.
///
/// Every failure is swallowed. A hook may not break a tool call, and a missing
/// lease only means a speculator schedules more conservatively.
fn record_lease(repo: &Path, tool: Option<&str>, path: Option<&str>) {
    use std::io::Write;

    // An off switch, because this sits on the agent's critical path. Anything
    // that runs on every Edit, Write and Bash should be disableable without a
    // rebuild.
    if std::env::var_os("ACYCLIC_NO_LEASES").is_some() {
        return;
    }
    let Ok(paths) = crate::store_paths(repo) else {
        return;
    };
    // The store root exists after `init`, but a lease is worth recording from
    // the very first tool call, which can precede it.
    if std::fs::create_dir_all(&paths.root).is_err() {
        return;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Repo-relative, and no tab: the file is tab separated and a reader must
    // never mis-split a line.
    let path = path
        .map(|p| {
            p.trim_start_matches(&*repo.to_string_lossy())
                .trim_start_matches('/')
        })
        .filter(|p| !p.is_empty() && !p.contains('\t'))
        .unwrap_or("*");
    let tool = tool.filter(|t| !t.contains('\t')).unwrap_or("?");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.root.join("leases"))
    {
        let _ = writeln!(f, "{at}\t{tool}\t{path}");
    }
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

    /// A scratch repo whose store lives beside it, so `record_lease` writes
    /// somewhere real and nothing touches the developer's own stores. The
    /// store root comes from the repo's own config, so that is where the
    /// redirect goes — there is no env override, deliberately.
    fn scratch() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".acyclic")).expect("repo");
        let stores = dir.path().join("stores");
        std::fs::create_dir_all(&stores).expect("stores");
        std::fs::write(
            repo.join(".acyclic/config.toml"),
            format!("store_dir = {:?}\n", stores.to_string_lossy()),
        )
        .expect("config");
        // The store root is created lazily by `init`; the lease writer must
        // work before that, which is what create_dir_all in it is for.
        (dir, repo)
    }

    fn leases_of(repo: &Path) -> String {
        let paths = crate::store_paths(repo).expect("store paths");
        std::fs::read_to_string(paths.root.join("leases")).unwrap_or_default()
    }

    #[test]
    fn a_path_is_recorded_repo_relative() {
        let (_dir, repo) = scratch();
        let absolute = repo.join("src/report.py");
        record_lease(&repo, Some("Edit"), Some(&absolute.to_string_lossy()));
        record_lease(&repo, Some("Write"), Some("src/money.py"));
        let text = leases_of(&repo);
        // Absolute and relative inputs both land relative: a reader compares
        // these against paths from `diff`, which are repo-relative.
        assert!(
            text.contains("\tEdit\tsrc/report.py\n"),
            "absolute path not made relative: {text}"
        );
        assert!(text.contains("\tWrite\tsrc/money.py\n"), "{text}");
    }

    #[test]
    fn a_tool_with_no_path_records_a_wildcard() {
        let (_dir, repo) = scratch();
        // Bash names no file and can touch anything, so a reader must block
        // rather than guess.
        record_lease(&repo, Some("Bash"), None);
        assert!(
            leases_of(&repo).contains("\tBash\t*\n"),
            "{}",
            leases_of(&repo)
        );
    }

    #[test]
    fn a_tab_in_either_field_is_refused() {
        let (_dir, repo) = scratch();
        // The file is tab separated. A tab smuggled in through a filename
        // would make a reader mis-split the line and treat junk as a path.
        record_lease(&repo, Some("Ed\tit"), Some("src/a\tb.py"));
        let text = leases_of(&repo);
        assert!(text.contains("\t?\t*\n"), "tabs not neutralised: {text}");
        assert_eq!(text.lines().count(), 1, "one line per call: {text}");
    }

    #[test]
    fn the_kill_switch_writes_nothing() {
        let (_dir, repo) = scratch();
        std::env::set_var("ACYCLIC_NO_LEASES", "1");
        record_lease(&repo, Some("Edit"), Some("src/report.py"));
        std::env::remove_var("ACYCLIC_NO_LEASES");
        assert!(
            leases_of(&repo).is_empty(),
            "the off switch must be an off switch"
        );
    }

    #[test]
    fn leases_never_land_inside_the_repo() {
        let (_dir, repo) = scratch();
        record_lease(&repo, Some("Edit"), Some("src/report.py"));
        // The regression this guards: writing into the tree woke the watcher,
        // and the pre-tool hook then waited for the checkpoint its own write
        // caused — 65ms to 120ms. It would also have shown up in every blast
        // radius as a changed path.
        assert!(
            !repo.join(".speculation").exists(),
            "a lease inside the repo is captured by the watcher and inflates \
             every diff"
        );
    }

    #[test]
    fn appending_keeps_earlier_lines() {
        let (_dir, repo) = scratch();
        for path in ["a.py", "b.py", "c.py"] {
            record_lease(&repo, Some("Edit"), Some(path));
        }
        // A reader takes the live window by timestamp, so history must not be
        // truncated by a later write.
        assert_eq!(leases_of(&repo).lines().count(), 3, "{}", leases_of(&repo));
    }
}
