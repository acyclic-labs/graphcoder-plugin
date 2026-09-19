//! `acyclic` — checkpoints, rewind, and blast-radius diff for agent sessions.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )
)]

mod brief;
mod client;
mod hook;
mod install;
mod ipc;
mod mcp;
mod proto;
mod server;
mod spec_runner;
mod speculate;

use std::path::{Path, PathBuf};

use acyclic::product::{self, NAME};
use acyclic::short_hex;
use clap::{Parser, Subcommand};

use client::{Client, ConnectError, Spawn};

/// Exit codes: 0 ok · 1 failure · 2 daemon-unavailable no-op (hook path).
const EXIT_NO_DAEMON: i32 = 2;

#[derive(Parser)]
#[command(name = product::NAME, bin_name = product::NAME, version, about)]
struct Cli {
    /// Repo root (defaults to the current directory).
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    /// Hook mode: never spawn a daemon; exit 2 quietly if one isn't running.
    #[arg(long, global = true, env = product::HOOK_ENV)]
    hook: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize the store for this repo and start checkpointing.
    Init,
    /// Snapshot now (hooks do this automatically).
    Checkpoint {
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Return as soon as the checkpoint is queued instead of waiting for
        /// it to land. Only safe when nothing edits the tree right after:
        /// a queued snapshot can include edits made before it runs.
        #[arg(long, conflicts_with = "durable")]
        no_wait: bool,
        /// Accepted for compatibility: waiting is now the default.
        #[arg(long, hide = true)]
        wait: bool,
        /// Also publish to the durable authority (coarse boundary).
        #[arg(long)]
        durable: bool,
        /// Checkpoint kind recorded in the timeline: pre | post | manual.
        #[arg(long, default_value = "manual")]
        kind: proto::CheckpointRequestKind,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        tool_call_id: Option<String>,
        #[arg(long)]
        tool_name: Option<String>,
    },
    /// List checkpoints, newest first, with their conversation turn.
    Timeline {
        #[arg(long)]
        session: Option<String>,
        /// Only this conversation turn of --session.
        #[arg(long)]
        turn: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Conversation turns: which prompt caused which checkpoints.
    Turns {
        #[arg(long)]
        session: Option<String>,
    },
    /// One checkpoint resolved to its session, turn, and prompt.
    Show { checkpoint: i64 },
    /// Host sessions, newest first.
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Previous-session summary (what the `SessionStart` hook hands the agent).
    Brief {
        /// The session asking, excluded from the summary.
        #[arg(long)]
        current: Option<String>,
        /// Emit JSON instead of the agent-readable text.
        #[arg(long)]
        json: bool,
    },
    /// What one conversation turn did, in prose, if speculation produced a
    /// summary for it at the turn boundary.
    Summary {
        #[arg(long)]
        session: Option<String>,
        /// Defaults to the last turn that finished.
        #[arg(long)]
        turn: Option<i64>,
        /// Milliseconds to wait for a summary still being produced. The
        /// default never waits.
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Restore one or more paths from a checkpoint, leaving the rest of the
    /// tree untouched.
    Restore {
        /// Checkpoint id from `{NAME} timeline`.
        checkpoint: i64,
        /// Paths relative to the repo root.
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Restore the tree to a checkpoint (untracked files included).
    Rewind {
        /// Checkpoint id from `{NAME} timeline`.
        target: Option<i64>,
        /// Rewind to the most recent checkpoint.
        #[arg(long)]
        last: bool,
        /// Rewind to the first checkpoint of a session.
        #[arg(long)]
        session_start: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Blast radius: what changed between two checkpoints, or in one turn.
    Diff {
        /// A checkpoint row id from `{NAME} timeline`, or a generation hex
        /// prefix as printed by `promote`/`status`.
        before: Option<String>,
        after: Option<String>,
        /// What one conversation turn changed (latest session unless --session).
        #[arg(long, conflicts_with_all = ["before", "after"])]
        turn: Option<i64>,
        #[arg(long, requires = "turn")]
        session: Option<String>,
        /// One line per file (the default output already is).
        #[arg(long)]
        stat: bool,
    },
    /// Fork the working tree N ways (writable overlay mounts; O(1), no copy).
    Fork {
        #[arg(short = 'n', long, default_value_t = 1)]
        count: u32,
    },
    /// List live forks.
    Forks,
    /// Discard a fork (its changes evaporate).
    ForkDrop { id: String },
    /// Land one or more forks' changes in the real working tree, in the
    /// order given. Each fork is reported on its own line; a conflict in one
    /// does not stop the rest.
    Promote {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
    /// Blast radius of a live fork against its base, without landing it.
    ForkDiff { id: String },
    /// Print the fork-decomposition parameters ([decompose] in
    /// .acyclic/config.toml over machine defaults) for the skill to read.
    Policy,
    /// Daemon and store health.
    Status,
    /// Publish pending checkpoints to the durable authority now.
    Commit,
    /// Stop this repo's daemon.
    Stop,
    /// Host-hook entrypoint: reads the hook's JSON payload from stdin and
    /// performs the matching engine action. Always exits 0 (never blocks the
    /// agent); a missing daemon is a silent no-op.
    Hook {
        /// One of pre-tool, post-tool, user-prompt, session-start,
        /// session-end. Anything else is ignored (exit 0), never a usage
        /// error: a hook must not block the agent.
        event: String,
    },
    /// Wire a host's adapter into the current repo.
    Install {
        #[arg(value_enum)]
        host: install::Host,
    },
    /// MCP stdio server: exposes checkpoint/timeline/rewind/diff/restore/
    /// turns/brief as tools for hosts that speak MCP (Claude Desktop, VS
    /// Code, Cursor). Runs until stdin closes.
    Mcp,
    /// Record a host session starting (hook use).
    #[command(hide = true)]
    SessionStart {
        session_id: String,
        #[arg(long, default_value = "")]
        host: String,
    },
    /// Record a host session ending (hook use).
    #[command(hide = true)]
    SessionEnd { session_id: String },
    /// Internal: the per-repo daemon process.
    #[command(name = "__daemon", hide = true)]
    Daemon { repo_root: PathBuf },
}

#[allow(
    unsafe_code,
    reason = "restores SIGPIPE's default disposition; no handler runs, nothing is aliased"
)]
fn main() {
    let cli = Cli::parse();
    // CLI verbs behave like Unix tools when a pager/grep closes the pipe
    // early: die silently on SIGPIPE instead of panicking on stdout errors.
    // The DAEMON must keep Rust's ignore-SIGPIPE default — mount teardown
    // can write to closed descriptors, and default disposition would kill
    // the whole daemon silently.
    #[cfg(unix)]
    if !matches!(cli.command, Command::Daemon { .. }) {
        // SAFETY: SIG_DFL restores default disposition; no handler runs.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    if matches!(cli.command, Command::Daemon { .. }) {
        std::process::exit(run(cli, Path::new(".")));
    }
    let repo_arg = cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    acyclic::fork::reap_legacy_shadow(&repo_arg);
    let repo_arg = acyclic::rewind::recover_before_repo_open(&repo_arg).unwrap_or_else(|error| {
        eprintln!("{}: rewind recovery: {error}", product::NAME);
        std::process::exit(1);
    });
    let repo = repo_arg.canonicalize().unwrap_or_else(|error| {
        eprintln!("{}: bad repo path: {error}", product::NAME);
        std::process::exit(1);
    });
    if cli.repo.is_none() && stranded_in_trash(&repo) {
        // A rewind or promote swaps the repo directory's inode; a shell that
        // was inside it now resolves its cwd to the replaced tree in trash.
        // Every verb would otherwise target a store that does not exist.
        eprintln!(
            "{}: your shell is inside a tree that a rewind replaced ({});\n  re-enter the repo first:  cd \"$PWD\"",
            product::NAME,
            repo.display()
        );
        std::process::exit(1);
    }
    let code = run(cli, &repo);
    std::process::exit(code);
}

/// True when `repo` (canonical) lies inside a store's trash (an ancestor
/// named `trash` whose parent is a store root, marked by `meta.json`), or
/// inside a rewind's sibling fallback trash directory (`.<name>.acyclic-trash-*`).
fn stranded_in_trash(repo: &Path) -> bool {
    repo.ancestors().any(|ancestor| {
        let Some(name) = ancestor.file_name().map(|name| name.to_string_lossy()) else {
            return false;
        };
        if name == "trash" {
            return ancestor
                .parent()
                .is_some_and(|store| store.join("meta.json").is_file());
        }
        name.starts_with('.') && name.contains(&format!(".{}-trash-", product::NAME))
    })
}

fn run(cli: Cli, repo: &Path) -> i32 {
    match cli.command {
        Command::Daemon { repo_root } => match server::run(&repo_root) {
            Ok(()) => 0,
            Err(message) => {
                eprintln!("{} daemon: {message}", product::NAME);
                1
            }
        },
        Command::Init => init(repo),
        Command::Policy => policy(repo),
        Command::Hook { event } => hook::run(repo, &event),
        Command::Mcp => match mcp::run(if cli.repo.is_none() {
            mcp::find_repo_root(repo)
        } else {
            repo.to_path_buf()
        }) {
            Ok(()) => 0,
            Err(message) => {
                eprintln!("{} mcp: {message}", product::NAME);
                1
            }
        },
        Command::Install { host } => match install::run(repo, host) {
            Ok(()) => {
                print_mount_capability();
                0
            }
            Err(message) => {
                eprintln!("{} install: {message}", product::NAME);
                1
            }
        },
        command => {
            let spawn = if cli.hook {
                Spawn::Never
            } else {
                Spawn::Allowed
            };
            let mut client = match connect(repo, spawn) {
                Ok(client) => client,
                Err(ConnectError::NoDaemon) => {
                    eprintln!("{}: daemon not running; checkpoint skipped", product::NAME);
                    return EXIT_NO_DAEMON;
                }
                Err(ConnectError::Starting) => {
                    eprintln!(
                        "{}: daemon still starting; checkpoint skipped",
                        product::NAME
                    );
                    return EXIT_NO_DAEMON;
                }
                Err(ConnectError::Other(message)) => {
                    eprintln!("{}: {message}", product::NAME);
                    return 1;
                }
            };
            match execute(&mut client, command, repo) {
                Ok(()) => 0,
                Err(message) => {
                    eprintln!("{}: {message}", product::NAME);
                    1
                }
            }
        }
    }
}

/// Steps the client out of the working tree, ahead of a whole-tree swap.
///
/// A process's current directory is an open handle to that directory on
/// Windows, so a client standing in the repo blocks the very rename it just
/// asked the daemon to perform -- the rewind comes back as a sharing
/// violation, having changed nothing. Everything after this point is a
/// daemon round-trip and some printing; no relative path is resolved again.
///
/// Unix renames a directory out from under a process's cwd without
/// complaint, so there is nothing to step out of there.
#[cfg(windows)]
fn step_aside() {
    let _ = std::env::set_current_dir(std::env::temp_dir());
}

#[cfg(not(windows))]
fn step_aside() {}

fn store_paths(repo: &Path) -> Result<acyclic::store::StorePaths, String> {
    let config = acyclic::config::Config::load(repo).map_err(|error| error.to_string())?;
    let stores_root = config.store_dir.as_ref().map(PathBuf::from);
    acyclic::store::StorePaths::for_repo(repo, stores_root.as_deref())
        .map_err(|error| error.to_string())
}

fn connect(repo: &Path, spawn: Spawn) -> Result<Client, ConnectError> {
    let paths = store_paths(repo).map_err(ConnectError::Other)?;
    let log = paths.root.join("daemon.log");
    Client::connect(&paths.socket(), repo, &log, spawn)
}

/// One line on native fork support, plus setup steps when the host lacks a
/// mount provider. Shown by `init` and `install`.
fn print_mount_capability() {
    let capability = acyclic::fork::mount_capability();
    if capability.available {
        println!("mounts:        {} (forks mount)", capability.provider);
    } else {
        println!(
            "mounts:        unavailable ({})",
            capability.reason.as_deref().unwrap_or("unknown reason")
        );
        println!("{}", acyclic::fork::mount_setup_hint());
    }
}

/// `{NAME} policy`: the effective `[decompose]` parameters as `key = value`
/// lines, so the skill reads one command instead of parsing TOML.
/// The speculation rollup under `status`.
///
/// Two lines, and both earn their place: the first says whether this daemon
/// can spend money, the second says whether speculating is working. A claim
/// rate near zero, or a median lead near zero, means the triggers are firing
/// too late to be worth anything — which is the point of measuring it.
/// Lands one fork and returns the text to print, or the conflict/refusal
/// report as the error.
fn promote_one(client: &mut Client, id: &str) -> Result<String, String> {
    let reply = client.call(proto::Op::Promote { id: id.to_owned() })?;
    let proto::Reply::Promote(info) = reply else {
        return Err("unexpected reply".into());
    };
    let kept_note = if info.kept_mainline.is_empty() {
        String::new()
    } else {
        format!(
            "kept the mainline's copy of {} gitignored path(s) both sides changed: {}\n",
            info.kept_mainline.len(),
            info.kept_mainline.join(", ")
        )
    };
    if !info.conflicts.is_empty() {
        let mut report = format!(
            "promote fork {id}: {} file(s) conflict; markers written into the fork, nothing landed\n",
            info.conflicts.len()
        );
        for conflict in &info.conflicts {
            report.push_str(&format!("  {}: {}\n", conflict.path, conflict.detail));
        }
        report.push_str(&format!(
            "Resolve the markers in {} and run `{NAME} promote {id}` again (the fork now sits on {})",
            info.fork_path.as_deref().unwrap_or("the fork"),
            short_hex(&info.generation)
        ));
        if !kept_note.is_empty() {
            report.push('\n');
            report.push_str(kept_note.trim_end());
        }
        return Err(report);
    }
    let mut out = kept_note;
    match (info.old_tree, info.replayed_paths, info.merged_files) {
        (Some(old_tree), _, _) => {
            out.push_str(&format!(
                "promoted: working tree now at {}\nold tree kept at {old_tree}\nnote: {}\n",
                short_hex(&info.generation),
                info.warning
            ));
        }
        (None, paths, merged) if merged > 0 => {
            out.push_str(&format!(
                "promoted by merge: {merged} file(s) merged, {paths} path(s) written in place, \
                 tree now at {}\nnote: {}\n",
                short_hex(&info.generation),
                info.warning
            ));
        }
        (None, paths, _) if paths > 0 && info.mainline_moved => {
            out.push_str(&format!(
                "promoted by replay: {paths} path(s) written in place, tree now at {}\nnote: {}\n",
                short_hex(&info.generation),
                info.warning
            ));
        }
        (None, paths, _) if paths > 0 => {
            out.push_str(&format!(
                "promoted: {paths} path(s) written in place, tree now at {}\n",
                short_hex(&info.generation)
            ));
        }
        (None, _, _) => out.push_str("fork had no changes; nothing to land\n"),
    }
    Ok(out)
}

fn print_speculation(spec: &proto::SpecStatus) {
    let mode = if spec.spends_tokens {
        format!("precompute + model runs ({})", spec.command)
    } else {
        "precompute only, no model runs".to_owned()
    };
    println!("speculation:   on — {mode}");
    let attempts = spec.claimed.saturating_add(spec.missed);
    let claimed = match spec.claimed.saturating_mul(100).checked_div(attempts) {
        Some(percent) => format!("{} of {attempts} claimed ({percent}%)", spec.claimed),
        None => "nothing asked yet".to_owned(),
    };
    // Integer maths rather than a float: a lead is milliseconds, and casting
    // i64 to f64 to print one decimal place is a lossy cast for nothing.
    let lead = match spec.median_lead_ms {
        Some(lead_ms) => format!(
            " · median lead {}.{}s",
            lead_ms / 1_000,
            (lead_ms % 1_000) / 100
        ),
        None => String::new(),
    };
    let timeouts = if spec.timeouts > 0 {
        format!(" · {} timeout(s)", spec.timeouts)
    } else {
        String::new()
    };
    println!(
        "               24h: {} run(s) · {claimed}{lead} · {} out{timeouts}",
        spec.runs,
        human_bytes(spec.bytes_out)
    );
}

fn policy(repo: &Path) -> i32 {
    match acyclic::config::Config::load(repo) {
        Ok(config) => {
            let d = config.decompose;
            println!("fan_out = {}", d.fan_out);
            println!("max_depth = {}", d.max_depth);
            println!("max_forks = {}", d.max_forks);
            println!("require_tests = {}", d.require_tests);
            match d.test_command {
                Some(command) => println!("test_command = {command:?}"),
                None => println!("test_command = (infer from the repo)"),
            }
            println!("tie_break = {:?}", d.tie_break);
            println!(
                "# set these under [decompose] in {}",
                product::repo_config_file()
            );
            0
        }
        Err(error) => {
            eprintln!("{} policy: {error}", product::NAME);
            1
        }
    }
}

fn init(repo: &Path) -> i32 {
    let result = (|| -> Result<(), String> {
        let config = acyclic::config::Config::load(repo).map_err(|error| error.to_string())?;
        let stores_root = config.store_dir.as_ref().map(PathBuf::from);
        let paths = acyclic::store::StorePaths::for_repo(repo, stores_root.as_deref())
            .map_err(|error| error.to_string())?;
        if paths.meta().exists() {
            println!("store already exists at {}", paths.root.display());
        } else {
            let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
            runtime
                .block_on(acyclic::store::Store::init(repo, paths.clone()))
                .map_err(|error| error.to_string())?;
            println!("store created at {}", paths.root.display());
        }
        // Spawning the daemon builds (or refreshes) the baseline.
        let mut client = connect(repo, Spawn::Allowed).map_err(|error| match error {
            ConnectError::NoDaemon => "daemon failed to start".to_owned(),
            ConnectError::Starting => "daemon is still starting".to_owned(),
            ConnectError::Other(message) => message,
        })?;
        client.call(proto::Op::Ping)?;
        println!("daemon ready — checkpointing is on");
        print_mount_capability();
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{} init: {message}", product::NAME);
            1
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per Command; each arm is a single call plus its printing"
)]
fn execute(client: &mut Client, command: Command, repo: &Path) -> Result<(), String> {
    match command {
        Command::Checkpoint {
            message,
            no_wait,
            wait: _,
            durable,
            kind,
            session_id,
            tool_call_id,
            tool_name,
        } => {
            let reply = client.call(proto::Op::Checkpoint {
                kind,
                session_id,
                tool_call_id,
                tool_name,
                label: message,
                // Waiting is the default so `checkpoint` followed by an edit
                // snapshots the pre-edit tree. A durable checkpoint is a
                // publish barrier and always waits.
                wait: !no_wait || durable,
                durable,
            })?;
            match reply {
                proto::Reply::Enqueued => println!("checkpoint queued"),
                proto::Reply::Checkpoint(info) => {
                    println!("checkpoint #{} ({})", info.row_id, info.kind);
                }
                other => return Err(format!("unexpected reply {other:?}")),
            }
            Ok(())
        }
        Command::Timeline {
            session,
            turn,
            limit,
        } => {
            let entries = client.timeline(session, turn, limit)?;
            if entries.is_empty() {
                println!("no checkpoints yet");
                return Ok(());
            }
            for entry in entries {
                let label = entry
                    .label
                    .or(entry.tool_name)
                    .or(entry.error.map(|error| format!("error: {error}")))
                    .unwrap_or_default();
                let turn = entry
                    .turn
                    .map(|turn| format!("t{turn}"))
                    .unwrap_or_default();
                println!(
                    "#{:<5} {:<10} {:<9} {:<4} {}{}",
                    entry.id,
                    age(entry.created_at),
                    entry.kind,
                    turn,
                    label,
                    if entry.published {
                        ""
                    } else {
                        "  (unpublished)"
                    },
                );
            }
            Ok(())
        }
        Command::Turns { session } => {
            let reply = client.call(proto::Op::Turns {
                session_id: session,
            })?;
            let proto::Reply::Turns(turns) = reply else {
                return Err("unexpected reply".into());
            };
            if turns.is_empty() {
                println!("no turns recorded (the user-prompt hook records them)");
                return Ok(());
            }
            for turn in turns {
                let range = match (turn.first_checkpoint, turn.last_checkpoint) {
                    (Some(first), Some(last)) if first != last => format!("#{first}..#{last}"),
                    (Some(first), _) => format!("#{first}"),
                    _ => "no checkpoints".to_owned(),
                };
                println!(
                    "{}  t{:<3} {:<10} {:<14} {}",
                    short_session(&turn.session_id),
                    turn.turn,
                    age(turn.started_at),
                    range,
                    brief::quote(&turn.prompt, 72),
                );
            }
            Ok(())
        }
        Command::Show { checkpoint } => {
            let reply = client.call(proto::Op::Inspect { checkpoint })?;
            let proto::Reply::Inspect(info) = reply else {
                return Err("unexpected reply".into());
            };
            println!(
                "checkpoint:  #{}  ({}{})",
                info.id,
                info.kind,
                if info.published { "" } else { ", unpublished" }
            );
            println!("generation:  {}", info.generation);
            println!("created:     {}", age(info.created_at));
            match &info.session_id {
                Some(session) => println!(
                    "session:     {}{}",
                    session,
                    info.host
                        .as_ref()
                        .map(|host| format!("  ({host})"))
                        .unwrap_or_default()
                ),
                None => println!("session:     none (outside any host session)"),
            }
            match info.turn {
                Some(turn) => println!("turn:        {turn}"),
                None => println!("turn:        none"),
            }
            if let Some(prompt) = &info.prompt {
                println!("prompt:      {}", brief::quote(prompt, 200));
            }
            if let Some(tool) = &info.tool_name {
                println!(
                    "tool:        {tool}{}",
                    info.tool_call_id
                        .as_ref()
                        .map(|id| format!("  ({id})"))
                        .unwrap_or_default()
                );
            }
            if let Some(label) = &info.label {
                println!("label:       {label}");
            }
            if let Some(target) = info.rewind_target {
                println!("rewind to:   #{target}");
            }
            if let Some(error) = &info.error {
                println!("error:       {error}");
            }
            Ok(())
        }
        Command::Sessions { limit } => {
            let reply = client.call(proto::Op::Sessions { limit })?;
            let proto::Reply::Sessions(sessions) = reply else {
                return Err("unexpected reply".into());
            };
            if sessions.is_empty() {
                println!("no sessions recorded");
                return Ok(());
            }
            for session in sessions {
                println!(
                    "{}  {:<12} started {:<10} {:<12} {:>3} turns  {:>4} checkpoints  ends at {}",
                    short_session(&session.session_id),
                    session.host.unwrap_or_default(),
                    age(session.started_at),
                    session.ended_at.map_or_else(
                        || "(no end)".into(),
                        |ended| format!("ended {}", age(ended))
                    ),
                    session.turns,
                    session.checkpoints,
                    session
                        .end_checkpoint
                        .map_or_else(|| "-".into(), |id| format!("#{id}")),
                );
            }
            Ok(())
        }
        Command::Brief { current, json } => {
            let reply = client.call(proto::Op::Brief { current })?;
            let proto::Reply::Brief(info) = reply else {
                return Err("unexpected reply".into());
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&info).map_err(|error| error.to_string())?
                );
            } else {
                print!("{}", brief::render(&info));
            }
            Ok(())
        }
        Command::Summary {
            session,
            turn,
            wait_ms,
            json,
        } => {
            let reply = client.call(proto::Op::Summary {
                session_id: session,
                turn,
                wait_ms,
            })?;
            let proto::Reply::Summary(info) = reply else {
                return Err("unexpected reply".into());
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&info).map_err(|error| error.to_string())?
                );
            } else {
                println!("turn {} of {}", info.turn, short_session(&info.session_id));
                println!("  prompt: {}", brief::quote(&info.prompt, 100));
                match info.text {
                    // Says where the words came from: this is generated
                    // prose, not a record of what happened.
                    Some(text) => println!("  {} summary: {text}", info.source),
                    None => println!("  no summary ({})", info.source),
                }
            }
            Ok(())
        }
        Command::Restore { checkpoint, paths } => {
            for path in paths {
                let info = client.restore(checkpoint, path)?;
                match info.action {
                    proto::RestoreAction::Removed => {
                        println!("{}: absent at #{}, removed", info.path, info.checkpoint);
                    }
                    proto::RestoreAction::Restored => {
                        println!("{}: restored from #{}", info.path, info.checkpoint);
                    }
                }
                if let Some(recorded) = info.recorded_checkpoint {
                    println!("  recorded as checkpoint #{recorded}");
                }
            }
            Ok(())
        }
        Command::Rewind {
            target,
            last,
            session_start,
            yes,
        } => {
            let target = match (target, last, session_start) {
                (Some(id), false, None) => proto::RewindTarget::Checkpoint(id),
                (None, true, None) => proto::RewindTarget::Last,
                (None, false, Some(session)) => proto::RewindTarget::SessionStart(session),
                _ => return Err("pass exactly one of <id>, --last, --session-start".into()),
            };
            if !yes {
                eprint!("rewind will replace the working tree (a safety checkpoint is taken first). Continue? [y/N] ");
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|error| error.to_string())?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    println!("aborted");
                    return Ok(());
                }
            }
            step_aside();
            let info = client.rewind(target)?;
            println!("restored checkpoint #{}", info.restored_checkpoint);
            println!("old tree kept at {}", info.old_tree);
            println!("note: {}", info.warning);
            Ok(())
        }
        Command::Diff {
            before,
            after,
            turn,
            session,
            ..
        } => {
            let (before, after, before_hex, after_hex) = if let Some(turn) = turn {
                let (before, after) = turn_range(client, session, turn)?;
                (before, after, None, None)
            } else {
                let (before, before_hex) = checkpoint_ref(before.as_deref())?;
                let (after, after_hex) = checkpoint_ref(after.as_deref())?;
                (before, after, before_hex, after_hex)
            };
            let entries = client.diff(before, after, before_hex, after_hex)?;
            print_diff(&entries);
            Ok(())
        }
        Command::Fork { count } => {
            let reply = client.call(proto::Op::Fork {
                count,
                session_id: None,
            })?;
            let proto::Reply::Forks(entries) = reply else {
                return Err("unexpected reply".into());
            };
            for entry in &entries {
                println!("fork {}  ({})  {}", entry.id, entry.mode, entry.path);
            }
            println!(
                "{} fork(s) ready — work in them freely; `{NAME} promote <id>` keeps a winner",
                entries.len()
            );
            if entries.iter().any(|entry| entry.mode == "copy") {
                println!(
                    "note: no mount provider on this host, so these are full copies \
                     (`{NAME} status` explains; promote works the same)"
                );
            }
            Ok(())
        }
        Command::Forks => {
            let reply = client.call(proto::Op::ForkList)?;
            let proto::Reply::Forks(entries) = reply else {
                return Err("unexpected reply".into());
            };
            if entries.is_empty() {
                println!("no live forks (forks do not survive daemon restarts)");
                return Ok(());
            }
            for entry in entries {
                let conflict = if entry.conflict_paths.is_empty() {
                    String::new()
                } else {
                    format!(
                        "  conflict: {} path(s), resolve then promote",
                        entry.conflict_paths.len()
                    )
                };
                println!(
                    "{}  {}  {}  {}  base {}{conflict}",
                    entry.id,
                    entry.mode,
                    age(entry.created_at),
                    entry.path,
                    short_hex(&entry.base)
                );
            }
            Ok(())
        }
        Command::ForkDrop { id } => {
            client.call(proto::Op::ForkDrop { id })?;
            println!("fork dropped; its changes evaporated");
            Ok(())
        }
        Command::Promote { ids } => {
            step_aside();
            let total = ids.len();
            let several = total > 1;
            let mut failures = Vec::new();
            for id in ids {
                match promote_one(client, &id) {
                    Ok(report) => {
                        if several {
                            print!("{id} -> ");
                        }
                        print!("{report}");
                    }
                    Err(report) => {
                        if several {
                            eprintln!("{id} -> {report}");
                        } else {
                            return Err(report);
                        }
                        failures.push(id);
                    }
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{} of {total} fork(s) did not land: {}",
                    failures.len(),
                    failures.join(", ")
                ))
            }
        }
        Command::ForkDiff { id } => {
            let reply = client.call(proto::Op::ForkDiff { id })?;
            let proto::Reply::Diff(entries) = reply else {
                return Err("unexpected reply".into());
            };
            print_diff(&entries);
            Ok(())
        }
        Command::Status => {
            let reply = client.call(proto::Op::Status)?;
            let proto::Reply::Status(info) = reply else {
                return Err("unexpected reply".into());
            };
            println!("repo:          {}", info.repo_root);
            println!("state:         {}", info.state);
            println!(
                "last checkpoint: {}",
                info.last_checkpoint
                    .map_or_else(|| "none".into(), |id| format!("#{id}"))
            );
            println!("unpublished:   {}", info.unpublished);
            println!("store size:    {}", human_bytes(info.store_bytes));
            if info.mount_available {
                println!("mounts:        {} (forks mount)", info.mount_provider);
            } else {
                println!(
                    "mounts:        unavailable ({}) — forks copy",
                    info.mount_reason.as_deref().unwrap_or("unknown reason")
                );
            }
            // Printed only when the watcher has lost its epoch at least
            // once: a clean daemon prints exactly what it printed before
            // this line existed.
            if let Some(watcher) = info.watcher.filter(|w| w.invalidations > 0) {
                println!(
                    "watcher:       {} invalidation(s), {} full rescan(s) costing {:.1}s total, \
                     last {:.0}ms{}",
                    watcher.invalidations,
                    watcher.recovery_rescans,
                    watcher.recovery_ms_total / 1000.0,
                    watcher.last_recovery_ms,
                    watcher
                        .last_reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" ({reason})"))
                );
            }
            // Printed only when speculation is configured: the default
            // output has to stay exactly what it was.
            if let Some(spec) = info.speculate {
                print_speculation(&spec);
            }
            Ok(())
        }
        Command::Commit => {
            client.call(proto::Op::Commit)?;
            println!("published");
            Ok(())
        }
        Command::Stop => {
            client.call(proto::Op::Stop)?;
            let pidfile = store_paths(repo)?.pidfile();
            let started = std::time::Instant::now();
            while pidfile.exists() {
                if started.elapsed() >= std::time::Duration::from_secs(30) {
                    return Err("daemon did not finish stopping within 30 seconds".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("daemon stopped");
            Ok(())
        }
        Command::SessionStart { session_id, host } => {
            client.call(proto::Op::SessionStart { session_id, host })?;
            Ok(())
        }
        Command::SessionEnd { session_id } => {
            client.call(proto::Op::SessionEnd { session_id })?;
            Ok(())
        }
        Command::Init
        | Command::Policy
        | Command::Daemon { .. }
        | Command::Hook { .. }
        | Command::Install { .. }
        | Command::Mcp => {
            unreachable!("handled in run()")
        }
    }
}

/// Resolves `--turn N` to (base, last) checkpoint ids: the latest real
/// checkpoint before the turn's first one, and the turn's last one.
/// Prints one diff listing: a tag per path, `(gitignored)` on paths the
/// repo ignores, and a count that separates real blast radius from noise.
fn print_diff(entries: &[proto::DiffEntry]) {
    if entries.is_empty() {
        println!("no changes");
        return;
    }
    let mut ignored = 0usize;
    for entry in entries {
        let tag = entry.change.tag();
        if entry.ignored {
            ignored += 1;
            println!("{tag} {}  (gitignored)", entry.path);
        } else {
            println!("{tag} {}", entry.path);
        }
    }
    if ignored > 0 {
        println!("{} paths changed ({ignored} gitignored)", entries.len());
    } else {
        println!("{} paths changed", entries.len());
    }
}

/// A checkpoint argument: a row id, or a generation hex prefix (at least
/// six hex digits, as `promote` and `status` print).
fn checkpoint_ref(arg: Option<&str>) -> Result<(Option<i64>, Option<String>), String> {
    let Some(arg) = arg else {
        return Ok((None, None));
    };
    let arg = arg.trim().trim_start_matches('#');
    if let Ok(id) = arg.parse::<i64>() {
        return Ok((Some(id), None));
    }
    if arg.len() >= 6 && arg.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok((None, Some(arg.to_owned())));
    }
    Err(format!(
        "{arg:?} is neither a checkpoint row id (see `{NAME} timeline`) \
         nor a generation hex prefix of at least 6 digits"
    ))
}

fn turn_range(
    client: &mut Client,
    session: Option<String>,
    turn: i64,
) -> Result<(Option<i64>, Option<i64>), String> {
    let session = if let Some(session) = session {
        session
    } else {
        let proto::Reply::Sessions(sessions) = client.call(proto::Op::Sessions { limit: 1 })?
        else {
            return Err("unexpected reply".into());
        };
        sessions
            .into_iter()
            .next()
            .map(|session| session.session_id)
            .ok_or("no sessions recorded")?
    };
    let proto::Reply::Turns(turns) = client.call(proto::Op::Turns {
        session_id: Some(session.clone()),
    })?
    else {
        return Err("unexpected reply".into());
    };
    let entry = turns
        .into_iter()
        .find(|entry| entry.turn == turn)
        .ok_or(format!("session {session} has no turn {turn}"))?;
    let Some(last) = entry.last_checkpoint else {
        return Err(format!("turn {turn} produced no checkpoints"));
    };
    Ok((entry.base_checkpoint, Some(last)))
}

pub(crate) fn short_session(session_id: &str) -> String {
    let mut short: String = session_id.chars().take(8).collect();
    if session_id.chars().count() > 8 {
        short.push('…');
    }
    short
}

fn age(created_at: i64) -> String {
    let delta = (acyclic::unix_now() - created_at).max(0);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "display only: one decimal of a size, not arithmetic"
)]
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in UNITS.into_iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

#[cfg(test)]
mod stranded_tests {
    use super::stranded_in_trash;

    #[test]
    fn store_trash_and_sibling_trash_are_detected() {
        let work = tempfile::tempdir().expect("tempdir");
        let store = work.path().join("stores/944d51144ca00be5");
        let trashed = store.join("trash/demo-1789144724/src");
        std::fs::create_dir_all(&trashed).expect("trash tree");
        std::fs::write(store.join("meta.json"), b"{}").expect("meta");
        assert!(stranded_in_trash(&trashed));
        assert!(stranded_in_trash(trashed.parent().unwrap()));

        let sibling = work.path().join(format!(
            ".demo.{}-trash-1789144724/src",
            super::product::NAME
        ));
        std::fs::create_dir_all(&sibling).expect("sibling");
        assert!(stranded_in_trash(&sibling));

        // A directory merely named trash, with no store above it, is fine.
        let plain = work.path().join("project/trash/notes");
        std::fs::create_dir_all(&plain).expect("plain");
        assert!(!stranded_in_trash(&plain));
        assert!(!stranded_in_trash(&work.path().join("demo")));
    }
}
