//! Running a model as a child process, on behalf of nobody.
//!
//! This is the one place the daemon spends money, and the one place it sends
//! repo content anywhere. Both facts shape the code:
//!
//! - **It is off unless a developer turned it on**, in a config file of
//!   their own, with a command they named. There is no default command and
//!   no default model; the daemon has no opinion about which model you use
//!   or who you buy it from.
//! - **The child gets no path into the repository.** Its working directory
//!   is an empty scratch dir and its entire input arrives on stdin, built
//!   from a store-computed diff that already honours `exclude`. A
//!   summarizer does not need a tree to walk, and handing an agent CLI the
//!   real working tree would invite it to write files — which would race the
//!   watcher and manufacture checkpoints nobody asked for. That is the worst
//!   failure mode available to a checkpointing product.
//!
//! The child is spawned into its own process group so a timeout kills the
//! whole tree: an agent CLI is typically a runtime that spawns its own
//! children, and killing only the direct child would leak them.

use std::path::{Path, PathBuf};
use std::time::Duration;

// Only the Unix sweep names the product; on Windows there is no sweep.
#[cfg(unix)]
use acyclic_engine::product::NAME;
use acyclic_engine::spec::RunOutcome;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// One run to perform.
pub struct RunSpec {
    /// Argv, from config. Never a shell string, and never carrying the
    /// prompt: argv is visible in `ps` and length-limited.
    pub command: Vec<String>,
    pub stdin: String,
    pub timeout: Duration,
    pub kill_grace: Duration,
    pub max_output_bytes: u64,
}

/// A run's scratch directory and pid file, both under the store.
///
/// The pid file is a file rather than a row because reaping has to happen
/// before the store and index are open — exactly like the fork sweep, which
/// runs first thing in `server::run` for the same reason.
pub struct RunSpace {
    root: PathBuf,
    pid_file: PathBuf,
    scratch: PathBuf,
}

impl RunSpace {
    /// Creates the scratch dir for one run. `spec_runs` is the store's
    /// speculation directory.
    pub fn create(spec_runs: &Path, run_id: i64) -> std::io::Result<Self> {
        let scratch = spec_runs.join(format!("run-{run_id}"));
        std::fs::create_dir_all(&scratch)?;
        std::fs::create_dir_all(spec_runs.join("pids"))?;
        Ok(Self {
            root: spec_runs.to_path_buf(),
            pid_file: spec_runs.join("pids").join(run_id.to_string()),
            scratch,
        })
    }

    fn record(&self, pgid: i32, argv0: &str) {
        let line = format!("{pgid} {} {argv0}\n", acyclic_engine::unix_now());
        let _ = std::fs::write(&self.pid_file, line);
    }

    fn clean(&self) {
        let _ = std::fs::remove_file(&self.pid_file);
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

impl Drop for RunSpace {
    fn drop(&mut self) {
        self.clean();
        // Tidy the container when this was the last run; failure means it
        // is not empty, which is fine.
        let _ = std::fs::remove_dir(self.root.join("pids"));
        let _ = std::fs::remove_dir(&self.root);
    }
}

/// Runs the configured command, enforcing the timeout, the output cap and
/// cancellation. Never returns an error: every failure is an outcome the
/// cache records, because a speculation that went wrong is not the caller's
/// problem.
pub async fn run(
    spec: RunSpec,
    space: &RunSpace,
    cancel: tokio::sync::oneshot::Receiver<()>,
) -> RunOutcome {
    let Some((program, args)) = spec.command.split_first() else {
        return RunOutcome::Failed("no command configured".to_owned());
    };
    let argv0 = Path::new(program).file_name().map_or_else(
        || program.clone(),
        |name| name.to_string_lossy().into_owned(),
    );

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&space.scratch)
        .env("ACYCLIC_SPEC", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    own_process_group(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return RunOutcome::Failed(format!("spawn {program}: {error}")),
    };
    // The child leads its own group, so its pid is the group id.
    let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) else {
        return RunOutcome::Failed("child exited before it could be tracked".to_owned());
    };
    space.record(pid, &argv0);

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(spec.stdin.as_bytes()).await;
        // Closing stdin is what tells the model the prompt is complete.
        drop(stdin);
    }

    let outcome = tokio::select! {
        finished = child.wait_with_output() => match finished {
            Ok(output) if output.status.success() => {
                let body = String::from_utf8_lossy(&output.stdout).into_owned();
                if body.len() as u64 > spec.max_output_bytes {
                    RunOutcome::Overflow
                } else {
                    RunOutcome::Ready { body }
                }
            }
            Ok(output) => RunOutcome::Failed(format!(
                "exit {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next_back()
                    .unwrap_or("no stderr")
            )),
            Err(error) => RunOutcome::Failed(error.to_string()),
        },
        _ = tokio::time::sleep(spec.timeout) => {
            kill_group(pid, spec.kill_grace).await;
            RunOutcome::Timeout
        }
        _ = cancel => {
            kill_group(pid, spec.kill_grace).await;
            RunOutcome::Cancelled
        }
    };
    space.clean();
    outcome
}

/// Signals the whole group, then makes sure. A model CLI is usually a
/// runtime with children of its own, so signalling only the child we spawned
/// would leave them running — and billing.
#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "killpg on a group this process created and still owns; no \
              handler runs and nothing is aliased"
)]
async fn kill_group(pgid: i32, grace: Duration) {
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    tokio::time::sleep(grace).await;
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
async fn kill_group(_pgid: i32, _grace: Duration) {}

/// Puts the child in a process group of its own.
#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "setsid() between fork and exec is async-signal-safe and is the \
              only way to make a timeout reach the child's own children"
)]
fn own_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn own_process_group(_command: &mut Command) {}

/// Kills anything a dead daemon left running, before the store opens.
///
/// Matched on the recorded command name as well as the pid, so a reused pid
/// belonging to something else is never signalled — the check costs one
/// `ps` and removes the whole class of mistake.
#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "kill(pid, 0) only tests for the process's existence; the \
              killpg that follows is guarded by a command-name match"
)]
pub fn sweep_stale_runs(spec_runs: &Path) {
    let pids = spec_runs.join("pids");
    let Ok(entries) = std::fs::read_dir(&pids) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let _ = std::fs::remove_file(entry.path());
        let mut fields = text.split_whitespace();
        let Some(pgid) = fields.next().and_then(|raw| raw.parse::<i32>().ok()) else {
            continue;
        };
        let Some(argv0) = fields.nth(1) else { continue };
        let alive = unsafe { libc::kill(pgid, 0) } == 0;
        if !alive || !command_name_matches(pgid, argv0) {
            continue;
        }
        eprintln!("{NAME} daemon: killing speculative run {pgid} ({argv0}) from a dead daemon");
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    // Scratch dirs from runs that never finished.
    if let Ok(entries) = std::fs::read_dir(spec_runs) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("run-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(&pids);
    let _ = std::fs::remove_dir(spec_runs);
}

#[cfg(not(unix))]
pub fn sweep_stale_runs(_spec_runs: &Path) {}

/// Whether the live process really is the run we recorded, rather than
/// whatever inherited its pid.
#[cfg(unix)]
fn command_name_matches(pid: i32, expected: &str) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let live = String::from_utf8_lossy(&output.stdout);
    let live = live.trim();
    Path::new(live)
        .file_name()
        .is_some_and(|name| name.to_string_lossy() == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &[&str], stdin: &str) -> RunSpec {
        RunSpec {
            command: command.iter().map(|part| (*part).to_owned()).collect(),
            stdin: stdin.to_owned(),
            timeout: Duration::from_secs(5),
            kill_grace: Duration::from_millis(50),
            max_output_bytes: 1024,
        }
    }

    fn run_blocking(spec: RunSpec, dir: &Path) -> RunOutcome {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let space = RunSpace::create(dir, 1).expect("space");
        let (_cancel, receiver) = tokio::sync::oneshot::channel();
        runtime.block_on(run(spec, &space, receiver))
    }

    #[test]
    fn stdin_reaches_the_child_and_stdout_comes_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_blocking(spec(&["cat"], "the diff"), dir.path());
        match outcome {
            RunOutcome::Ready { body } => assert_eq!(body, "the diff"),
            other => panic!("expected a body, got {other:?}"),
        }
    }

    #[test]
    fn output_over_the_cap_is_refused_rather_than_stored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut spec = spec(&["cat"], &"x".repeat(2048));
        spec.max_output_bytes = 16;
        assert!(matches!(
            run_blocking(spec, dir.path()),
            RunOutcome::Overflow
        ));
    }

    #[test]
    fn a_failing_command_reports_its_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_blocking(
            spec(&["sh", "-c", "echo went wrong >&2; exit 3"], ""),
            dir.path(),
        );
        match outcome {
            RunOutcome::Failed(message) => assert!(
                message.contains("went wrong"),
                "stderr should reach the record: {message}"
            ),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_command_is_an_outcome_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            run_blocking(spec(&["definitely-not-a-real-binary"], ""), dir.path()),
            RunOutcome::Failed(_)
        ));
        assert!(matches!(
            run_blocking(spec(&[], ""), dir.path()),
            RunOutcome::Failed(_)
        ));
    }

    #[test]
    fn a_slow_command_times_out_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut spec = spec(&["sleep", "30"], "");
        spec.timeout = Duration::from_millis(150);
        assert!(matches!(
            run_blocking(spec, dir.path()),
            RunOutcome::Timeout
        ));
        // The scratch dir goes with the run.
        assert!(!dir.path().join("run-1").exists());
    }

    /// The child must not be able to see, let alone write, the repository.
    #[test]
    fn the_child_runs_in_an_empty_scratch_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_blocking(spec(&["sh", "-c", "ls -A | wc -l"], ""), dir.path());
        match outcome {
            RunOutcome::Ready { body } => assert_eq!(body.trim(), "0"),
            other => panic!("expected a body, got {other:?}"),
        }
    }

    /// The property the timeout is actually for: an agent CLI is a runtime
    /// that spawns children, and killing only the process we spawned would
    /// leave those running — and billing.
    #[test]
    fn a_timeout_kills_the_children_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut spec = spec(&["sh", "-c", "sleep 91 & sleep 91"], "");
        spec.timeout = Duration::from_millis(200);
        assert!(matches!(
            run_blocking(spec, dir.path()),
            RunOutcome::Timeout
        ));
        std::thread::sleep(Duration::from_millis(300));
        let output = std::process::Command::new("pgrep")
            .args(["-f", "sleep 91"])
            .output()
            .expect("pgrep");
        assert!(
            output.stdout.is_empty(),
            "children survived: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn sweeping_an_absent_directory_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        sweep_stale_runs(&dir.path().join("nothing-here"));
    }
}
