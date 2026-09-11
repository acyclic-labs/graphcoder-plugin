//! `acyclic` — checkpoints, rewind, and blast-radius diff for agent sessions.

mod brief;
mod client;
mod hook;
mod install;
mod server;

use std::path::{Path, PathBuf};

use acyclic_proto as proto;
use clap::{Parser, Subcommand};

use client::{Client, ConnectError, Spawn};

/// Exit codes: 0 ok · 1 failure · 2 daemon-unavailable no-op (hook path).
const EXIT_NO_DAEMON: i32 = 2;

#[derive(Parser)]
#[command(name = "acyclic", version, about)]
struct Cli {
    /// Repo root (defaults to the current directory).
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    /// Hook mode: never spawn a daemon; exit 2 quietly if one isn't running.
    #[arg(long, global = true, env = "ACYCLIC_HOOK")]
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
        /// Checkpoint kind recorded in the timeline.
        #[arg(long, default_value = "manual")]
        kind: String,
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
    /// Previous-session summary (what the SessionStart hook hands the agent).
    Brief {
        /// The session asking, excluded from the summary.
        #[arg(long)]
        current: Option<String>,
        /// Emit JSON instead of the agent-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Restore one or more paths from a checkpoint, leaving the rest of the
    /// tree untouched.
    Restore {
        /// Checkpoint id from `acyclic timeline`.
        checkpoint: i64,
        /// Paths relative to the repo root.
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Restore the tree to a checkpoint (untracked files included).
    Rewind {
        /// Checkpoint id from `acyclic timeline`.
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
        /// A checkpoint row id from `acyclic timeline`, or a generation hex
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
    /// Land a fork's changes in the real working tree.
    Promote { id: String },
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
        /// pre-tool | post-tool | user-prompt | session-start | session-end
        event: String,
    },
    /// Wire a host's adapter into the current repo.
    Install {
        /// claude-code | agents-md
        host: String,
    },
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
    /// Safe Mode: commit a session's shadow fork and show what it would
    /// change, without touching the real tree yet.
    #[command(hide = true)]
    SessionResolve { session_id: String },
    /// Safe Mode: apply a `session-resolve`d session's changes to the real
    /// tree.
    #[command(hide = true)]
    SessionApply { session_id: String },
    /// Safe Mode: discard a `session-resolve`d session without applying it.
    #[command(hide = true)]
    SessionDiscard { session_id: String },
    /// Internal: the per-repo daemon process.
    #[command(name = "__daemon", hide = true)]
    Daemon { repo_root: PathBuf },
}

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
    let repo_arg = cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    // Before touching the repo path (canonicalize, config load, socket): a
    // crashed Safe Mode daemon can leave a dead shadow mount over the repo
    // root that wedges every stat under it. Force-unmount it first (a no-op
    // unless a bounded probe shows the mount is genuinely wedged), so the
    // real tree is back before we read anything.
    acyclic_engine::fork::reap_dead_shadow(&repo_arg);
    let repo = repo_arg.canonicalize().unwrap_or_else(|error| {
        eprintln!("acyclic: bad repo path: {error}");
        std::process::exit(1);
    });
    if cli.repo.is_none() && stranded_in_trash(&repo) {
        // A rewind or promote swaps the repo directory's inode; a shell that
        // was inside it now resolves its cwd to the replaced tree in trash.
        // Every verb would otherwise target a store that does not exist.
        eprintln!(
            "acyclic: your shell is inside a tree that a rewind replaced ({});\n  re-enter the repo first:  cd \"$PWD\"",
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
                .map(|store| store.join("meta.json").is_file())
                .unwrap_or(false);
        }
        name.starts_with('.') && name.contains(".acyclic-trash-")
    })
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

        let sibling = work.path().join(".demo.acyclic-trash-1789144724/src");
        std::fs::create_dir_all(&sibling).expect("sibling");
        assert!(stranded_in_trash(&sibling));

        // A directory merely named trash, with no store above it, is fine.
        let plain = work.path().join("project/trash/notes");
        std::fs::create_dir_all(&plain).expect("plain");
        assert!(!stranded_in_trash(&plain));
        assert!(!stranded_in_trash(&work.path().join("demo")));
    }
}

fn run(cli: Cli, repo: &Path) -> i32 {
    match cli.command {
        Command::Daemon { repo_root } => match server::run(&repo_root) {
            Ok(()) => 0,
            Err(message) => {
                eprintln!("acyclic daemon: {message}");
                1
            }
        },
        Command::Init => init(repo),
        Command::Policy => policy(repo),
        Command::Hook { event } => hook::run(repo, &event),
        Command::Install { host } => match install::run(repo, &host) {
            Ok(()) => {
                print_mount_capability();
                0
            }
            Err(message) => {
                eprintln!("acyclic install: {message}");
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
                    eprintln!("acyclic: daemon not running; checkpoint skipped");
                    return EXIT_NO_DAEMON;
                }
                Err(ConnectError::Other(message)) => {
                    eprintln!("acyclic: {message}");
                    return 1;
                }
            };
            match execute(&mut client, command) {
                Ok(()) => 0,
                Err(message) => {
                    eprintln!("acyclic: {message}");
                    1
                }
            }
        }
    }
}

fn store_paths(repo: &Path) -> Result<acyclic_engine::store::StorePaths, String> {
    let config = acyclic_engine::config::Config::load(repo).map_err(|error| error.to_string())?;
    let stores_root = config.store_dir.as_ref().map(PathBuf::from);
    acyclic_engine::store::StorePaths::for_repo(repo, stores_root.as_deref())
        .map_err(|error| error.to_string())
}

fn connect(repo: &Path, spawn: Spawn) -> Result<Client, ConnectError> {
    let paths = store_paths(repo).map_err(ConnectError::Other)?;
    let log = paths.root.join("daemon.log");
    Client::connect(&paths.socket(), repo, &log, spawn)
}

/// One line on what forks and Safe Mode can do here, plus setup steps when
/// the host lacks a mount provider. Shown by `init` and `install`.
fn print_mount_capability() {
    let capability = acyclic_engine::fork::mount_capability();
    if capability.available {
        println!(
            "mounts:        {} (forks and Safe Mode available)",
            capability.provider
        );
    } else {
        println!(
            "mounts:        unavailable ({})",
            capability.reason.as_deref().unwrap_or("unknown reason")
        );
        println!("{}", acyclic_engine::fork::mount_setup_hint());
    }
}

/// `acyclic policy`: the effective `[decompose]` parameters as `key = value`
/// lines, so the skill reads one command instead of parsing TOML.
fn policy(repo: &Path) -> i32 {
    match acyclic_engine::config::Config::load(repo) {
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
            println!("# set these under [decompose] in .acyclic/config.toml");
            0
        }
        Err(error) => {
            eprintln!("acyclic policy: {error}");
            1
        }
    }
}

fn init(repo: &Path) -> i32 {
    let result = (|| -> Result<(), String> {
        let config =
            acyclic_engine::config::Config::load(repo).map_err(|error| error.to_string())?;
        let stores_root = config.store_dir.as_ref().map(PathBuf::from);
        let paths = acyclic_engine::store::StorePaths::for_repo(repo, stores_root.as_deref())
            .map_err(|error| error.to_string())?;
        if !paths.meta().exists() {
            let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
            runtime
                .block_on(acyclic_engine::store::Store::init(repo, paths.clone()))
                .map_err(|error| error.to_string())?;
            println!("store created at {}", paths.root.display());
        } else {
            println!("store already exists at {}", paths.root.display());
        }
        // Spawning the daemon builds (or refreshes) the baseline.
        let mut client = connect(repo, Spawn::Allowed).map_err(|error| match error {
            ConnectError::NoDaemon => "daemon failed to start".to_string(),
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
            eprintln!("acyclic init: {message}");
            1
        }
    }
}

fn execute(client: &mut Client, command: Command) -> Result<(), String> {
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
            let kind = match kind.as_str() {
                "pre" | "post" | "manual" => kind,
                other => return Err(format!("unknown kind {other:?}")),
            };
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
            let reply = client.call(proto::Op::Timeline {
                session_id: session,
                turn,
                limit,
            })?;
            let proto::Reply::Timeline(entries) = reply else {
                return Err("unexpected reply".into());
            };
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
                    _ => "no checkpoints".to_string(),
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
                    session
                        .ended_at
                        .map(|ended| format!("ended {}", age(ended)))
                        .unwrap_or_else(|| "(no end)".into()),
                    session.turns,
                    session.checkpoints,
                    session
                        .end_checkpoint
                        .map(|id| format!("#{id}"))
                        .unwrap_or_else(|| "-".into()),
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
        Command::Restore { checkpoint, paths } => {
            for path in paths {
                let reply = client.call(proto::Op::Rewind {
                    target: proto::RewindTarget::Checkpoint(checkpoint),
                    path: Some(path),
                })?;
                let proto::Reply::Restore(info) = reply else {
                    return Err("unexpected reply".into());
                };
                match info.action.as_str() {
                    "removed" => println!("{}: absent at #{}, removed", info.path, info.checkpoint),
                    _ => println!("{}: restored from #{}", info.path, info.checkpoint),
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
            let reply = client.call(proto::Op::Rewind { target, path: None })?;
            let proto::Reply::Rewind(info) = reply else {
                return Err("unexpected reply".into());
            };
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
            let (before, after, before_hex, after_hex) = match turn {
                Some(turn) => {
                    let (before, after) = turn_range(client, session, turn)?;
                    (before, after, None, None)
                }
                None => {
                    let (before, before_hex) = checkpoint_ref(before.as_deref())?;
                    let (after, after_hex) = checkpoint_ref(after.as_deref())?;
                    (before, after, before_hex, after_hex)
                }
            };
            let reply = client.call(proto::Op::Diff {
                before,
                after,
                before_hex,
                after_hex,
            })?;
            let proto::Reply::Diff(entries) = reply else {
                return Err("unexpected reply".into());
            };
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
                "{} fork(s) ready — work in them freely; `acyclic promote <id>` keeps a winner",
                entries.len()
            );
            if entries.iter().any(|entry| entry.mode == "copy") {
                println!(
                    "note: no mount provider on this host, so these are full copies \
                     (`acyclic status` explains; promote works the same)"
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
                    &entry.base[..12]
                );
            }
            Ok(())
        }
        Command::ForkDrop { id } => {
            client.call(proto::Op::ForkDrop { id })?;
            println!("fork dropped; its changes evaporated");
            Ok(())
        }
        Command::Promote { id } => {
            let reply = client.call(proto::Op::Promote { id: id.clone() })?;
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
                    "Resolve the markers in {} and run `acyclic promote {id}` again (the fork now sits on {})",
                    info.fork_path.as_deref().unwrap_or("the fork"),
                    &info.generation[..12]
                ));
                if !kept_note.is_empty() {
                    report.push('\n');
                    report.push_str(kept_note.trim_end());
                }
                return Err(report.into());
            }
            print!("{kept_note}");
            match (info.old_tree, info.replayed_paths, info.merged_files) {
                (Some(old_tree), _, _) => {
                    println!("promoted: working tree now at {}", &info.generation[..12]);
                    println!("old tree kept at {old_tree}");
                    println!("note: {}", info.warning);
                }
                (None, paths, merged) if merged > 0 => {
                    println!(
                        "promoted by merge: {merged} file(s) merged, {paths} path(s) written in place, tree now at {}",
                        &info.generation[..12]
                    );
                    println!("note: {}", info.warning);
                }
                (None, paths, _) if paths > 0 && info.mainline_moved => {
                    println!(
                        "promoted by replay: {paths} path(s) written in place, tree now at {}",
                        &info.generation[..12]
                    );
                    println!("note: {}", info.warning);
                }
                (None, paths, _) if paths > 0 => {
                    println!(
                        "promoted: {paths} path(s) written in place, tree now at {}",
                        &info.generation[..12]
                    );
                }
                (None, _, _) => println!("fork had no changes; nothing to land"),
            }
            Ok(())
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
                    .map(|id| format!("#{id}"))
                    .unwrap_or_else(|| "none".into())
            );
            println!("unpublished:   {}", info.unpublished);
            println!("store size:    {}", human_bytes(info.store_bytes));
            if info.mount_available {
                println!(
                    "mounts:        {} (forks mount, Safe Mode on)",
                    info.mount_provider
                );
            } else {
                println!(
                    "mounts:        unavailable ({}) — forks copy, Safe Mode off",
                    info.mount_reason.as_deref().unwrap_or("unknown reason")
                );
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
            println!("daemon stopping");
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
        Command::SessionResolve { session_id } => {
            let reply = client.call(proto::Op::SessionResolve { session_id })?;
            let proto::Reply::SessionPending(info) = reply else {
                return Err("unexpected reply".into());
            };
            if info.diff.is_empty() {
                println!("session {}: no changes", info.session_id);
                return Ok(());
            }
            for entry in &info.diff {
                let tag = match entry.change.as_str() {
                    "added" => "A",
                    "removed" => "D",
                    "modified" => "M",
                    _ => "m",
                };
                println!("{tag} {}", entry.path);
            }
            println!(
                "{} paths changed; run `acyclic session-apply {}` to land them or \
                 `acyclic session-discard {}` to throw them away",
                info.diff.len(),
                info.session_id,
                info.session_id
            );
            Ok(())
        }
        Command::SessionApply { session_id } => {
            let reply = client.call(proto::Op::SessionApply { session_id })?;
            let proto::Reply::Promote(info) = reply else {
                return Err("unexpected reply".into());
            };
            match info.old_tree {
                Some(old_tree) => {
                    println!("applied: working tree now at {}", &info.generation[..12]);
                    println!("old tree kept at {old_tree}");
                    println!("note: {}", info.warning);
                }
                None => println!("session had no changes; nothing to land"),
            }
            Ok(())
        }
        Command::SessionDiscard { session_id } => {
            client.call(proto::Op::SessionDiscard { session_id })?;
            Ok(())
        }
        Command::Init
        | Command::Policy
        | Command::Daemon { .. }
        | Command::Hook { .. }
        | Command::Install { .. } => {
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
        let tag = match entry.change.as_str() {
            "added" => "A",
            "removed" => "D",
            "modified" => "M",
            _ => "m",
        };
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
        return Ok((None, Some(arg.to_string())));
    }
    Err(format!(
        "{arg:?} is neither a checkpoint row id (see `acyclic timeline`) nor a generation hex prefix of at least 6 digits"
    ))
}

fn turn_range(
    client: &mut Client,
    session: Option<String>,
    turn: i64,
) -> Result<(Option<i64>, Option<i64>), String> {
    let session = match session {
        Some(session) => session,
        None => {
            let proto::Reply::Sessions(sessions) = client.call(proto::Op::Sessions { limit: 1 })?
            else {
                return Err("unexpected reply".into());
            };
            sessions
                .into_iter()
                .next()
                .map(|session| session.session_id)
                .ok_or("no sessions recorded")?
        }
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

fn short_session(session_id: &str) -> String {
    let mut short: String = session_id.chars().take(8).collect();
    if session_id.chars().count() > 8 {
        short.push('…');
    }
    short
}

fn age(created_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let delta = (now - created_at).max(0);
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

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
