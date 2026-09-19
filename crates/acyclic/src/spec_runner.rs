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
//! The child is spawned into a group of its own — a process group on Unix,
//! a job object on Windows — so a timeout kills the whole tree: an agent CLI
//! is typically a runtime that spawns its own children, and killing only the
//! direct child would leak them. See [`RunGroup`].

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
    let group = RunGroup::prepare(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return RunOutcome::Failed(format!("spawn {program}: {error}")),
    };
    group.adopt(&child);
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
            group.kill(pid, spec.kill_grace).await;
            RunOutcome::Timeout
        }
        _ = cancel => {
            group.kill(pid, spec.kill_grace).await;
            RunOutcome::Cancelled
        }
    };
    space.clean();
    outcome
}

/// Holds a run's whole process tree, so a timeout can reach all of it.
///
/// A model CLI is usually a runtime with children of its own, so killing
/// only the child we spawned would leave them running — and billing. Both
/// platforms have an answer; they are not the same answer.
///
/// - **Unix**: the child calls `setsid` between fork and exec, leading a
///   process group of its own, and the timeout signals the group.
/// - **Windows**: the child is assigned to a job object. There is no
///   `pre_exec` equivalent, so the assignment happens just after spawn —
///   a descendant started in that window escapes the job. It is narrow and
///   it is the price of not hand-rolling `CreateProcess`.
struct RunGroup {
    /// The job the run's processes belong to, absent if it could not be
    /// created. `None` degrades to killing the direct child, which is what
    /// this platform did before.
    #[cfg(windows)]
    job: Option<OwnedJob>,
}

#[cfg(unix)]
impl RunGroup {
    /// Puts the child in a process group of its own.
    #[allow(
        unsafe_code,
        reason = "setsid() between fork and exec is async-signal-safe and is the \
                  only way to make a timeout reach the child's own children"
    )]
    fn prepare(command: &mut Command) -> Self {
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        Self {}
    }

    /// Nothing to do: `setsid` already ran, in the child.
    fn adopt(&self, _child: &tokio::process::Child) {}

    /// Signals the whole group, then makes sure.
    #[allow(
        unsafe_code,
        reason = "killpg on a group this process created and still owns; no \
                  handler runs and nothing is aliased"
    )]
    async fn kill(&self, pgid: i32, grace: Duration) {
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        tokio::time::sleep(grace).await;
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
impl RunGroup {
    /// Creates the job the child will be assigned to, before the spawn so
    /// the window between the two is as short as it can be.
    ///
    /// `KILL_ON_JOB_CLOSE` is what makes the tree die with the daemon: this
    /// handle is the only one, so a force-killed daemon takes every
    /// speculative descendant with it. That is why `sweep_stale_runs` needs
    /// no Windows arm.
    #[allow(
        unsafe_code,
        reason = "creates an unnamed job object and sets one documented limit on it; \
                  the handle is owned by OwnedJob and closed exactly once"
    )]
    fn prepare(_command: &mut Command) -> Self {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // SAFETY: an unnamed job with default security; returns null on
        // failure, which is the only thing done with the result.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Self { job: None };
        }
        let job = OwnedJob(handle as isize);

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0);
        // SAFETY: `limits` is a live, fully initialised value of exactly the
        // type the information class names, and its length is passed as
        // written. A failure here costs the kill-on-close limit, not the
        // job, so it is not treated as fatal.
        unsafe {
            SetInformationJobObject(
                job.handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size,
            );
        }
        Self { job: Some(job) }
    }

    /// Assigns the spawned child, and with it every process it goes on to
    /// start, to the job.
    #[allow(
        unsafe_code,
        reason = "assigns a child this process just spawned, and whose handle it still \
                  owns, to a job it created"
    )]
    fn adopt(&self, child: &tokio::process::Child) {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let (Some(job), Some(process)) = (self.job.as_ref(), child.raw_handle()) else {
            return;
        };
        // SAFETY: both handles are live and owned here — the job by
        // `OwnedJob`, the process by `child`, which outlives this call.
        unsafe {
            AssignProcessToJobObject(job.handle(), process.cast());
        }
    }

    /// Ends the tree. The grace period has no counterpart here: Windows
    /// offers no graceful signal to a job, so this is the `SIGKILL` half
    /// without the `SIGTERM` half.
    #[allow(
        unsafe_code,
        reason = "terminates a job this process created and still owns"
    )]
    async fn kill(&self, _pgid: i32, _grace: Duration) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        let Some(job) = self.job.as_ref() else { return };
        // SAFETY: the handle is live for as long as `self` is.
        unsafe {
            TerminateJobObject(job.handle(), 1);
        }
    }
}

/// Neither `setsid` nor a job object: the direct child is killed on drop and
/// anything it started outlives it.
#[cfg(not(any(unix, windows)))]
impl RunGroup {
    fn prepare(_command: &mut Command) -> Self {
        Self {}
    }

    fn adopt(&self, _child: &tokio::process::Child) {}

    async fn kill(&self, _pgid: i32, _grace: Duration) {}
}

/// Owns a job handle and closes it once.
///
/// Held as an `isize` rather than a `HANDLE`, because a raw pointer would
/// make [`RunGroup`] `!Send` and the run future is spawned onto the runtime.
#[cfg(windows)]
struct OwnedJob(isize);

#[cfg(windows)]
impl OwnedJob {
    fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0 as windows_sys::Win32::Foundation::HANDLE
    }
}

#[cfg(windows)]
impl Drop for OwnedJob {
    #[allow(
        unsafe_code,
        reason = "closes the handle CreateJobObjectW returned, exactly once"
    )]
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateJobObjectW` in `prepare`, is
        // not closed anywhere else, and this runs once. Closing the last
        // handle is also what enforces KILL_ON_JOB_CLOSE, reaping anything
        // the run left behind.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle());
        }
    }
}

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

/// Windows needs no sweep: each run's job object carries
/// `KILL_ON_JOB_CLOSE` and the daemon holds its only handle, so a daemon
/// that dies — however it dies — takes the run's whole tree with it.
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
    ///
    /// POSIX-only as written: it needs `pgrep` and a shell that backgrounds
    /// with `&`. The Windows half of the same property is below.
    #[cfg(unix)]
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

    /// The Windows half, asserted at the mechanism rather than by hunting
    /// processes: there is no `pgrep`, and job membership is exactly the
    /// thing that can be queried directly. A process in a job has its own
    /// descendants in that job — that is what a job object is — so
    /// membership plus a working `TerminateJobObject` is the property.
    ///
    /// What this cannot see is the window between `CreateProcess` returning
    /// and the assignment, which is recorded in `docs/windows-verification.md`.
    #[cfg(windows)]
    #[test]
    #[allow(
        unsafe_code,
        reason = "IsProcessInJob only reads membership of two handles this test owns"
    )]
    fn the_child_joins_the_job_and_the_job_kill_ends_it() {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut command = Command::new("cmd");
            command
                .args(["/c", "ping", "-n", "60", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true);
            let group = RunGroup::prepare(&mut command);
            let mut child = command.spawn().expect("spawn");
            group.adopt(&child);

            let process = child.raw_handle().expect("child is live");
            let job = group.job.as_ref().expect("job created").handle();
            let mut inside = 0;
            // SAFETY: both handles are live and owned here — the process by
            // `child`, the job by `group` — and the call only reads.
            let queried = unsafe { IsProcessInJob(process.cast(), job, &raw mut inside) };
            assert!(queried != 0, "IsProcessInJob failed");
            assert!(inside != 0, "the child never joined the job");

            group.kill(0, Duration::from_millis(0)).await;
            let status = child.wait().await.expect("wait");
            assert!(!status.success(), "a terminated child cannot exit cleanly");
        });
    }

    #[test]
    fn sweeping_an_absent_directory_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        sweep_stale_runs(&dir.path().join("nothing-here"));
    }
}
