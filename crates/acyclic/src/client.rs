//! Synchronous client: connect to the daemon socket, one request → response.
//! Interactive verbs may spawn a dead daemon; hook-invoked calls never do.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::ipc::ClientStream;
use acyclic_engine::product::NAME;
use acyclic_proto as proto;

#[derive(Clone, Copy)]
pub enum Spawn {
    /// Interactive: start the daemon if it isn't running (waits for baseline).
    Allowed,
    /// Start the daemon if it isn't running, but wait at most this long for
    /// it to answer. Past that, [`ConnectError::Starting`]: the daemon keeps
    /// coming up in the background. The session-start hook uses this so a
    /// cold daemon never delays the agent's first turn.
    AllowedFor(Duration),
    /// Hook path: never spawn; a missing daemon is a warning-and-exit-2.
    Never,
}

pub struct Client {
    stream: BufReader<ClientStream>,
    next_id: u64,
}

pub enum ConnectError {
    /// No daemon and spawning was not allowed.
    NoDaemon,
    /// A daemon was spawned and is still starting; the bounded wait ran out.
    Starting,
    Other(String),
}

impl Client {
    pub fn connect(
        socket: &Path,
        repo_root: &Path,
        log_path: &Path,
        spawn: Spawn,
    ) -> Result<Self, ConnectError> {
        let started = std::time::Instant::now();
        if let Ok(stream) = ClientStream::connect(socket) {
            acyclic_engine::trace!(
                "client",
                "connected to running daemon at {} in {:.1}ms",
                crate::ipc::endpoint_display(socket),
                acyclic_engine::trace::ms(started)
            );
            return Self::from_stream(stream);
        }
        match spawn {
            Spawn::Never => {
                acyclic_engine::trace!(
                    "client",
                    "no daemon at {} and spawning is not allowed here",
                    crate::ipc::endpoint_display(socket)
                );
                Err(ConnectError::NoDaemon)
            }
            Spawn::Allowed | Spawn::AllowedFor(_) => {
                acyclic_engine::trace!(
                    "client",
                    "no daemon at {}: spawning one",
                    crate::ipc::endpoint_display(socket)
                );
                let child = spawn_daemon(repo_root, log_path)?;
                let bound = match spawn {
                    Spawn::AllowedFor(bound) => Some(bound),
                    _ => None,
                };
                let client = wait_for_socket(socket, child, log_path, bound);
                acyclic_engine::trace!(
                    "client",
                    "daemon spawn + socket wait took {:.1}ms",
                    acyclic_engine::trace::ms(started)
                );
                client
            }
        }
    }

    fn from_stream(stream: ClientStream) -> Result<Self, ConnectError> {
        stream
            .set_read_timeout(None)
            .map_err(|error| ConnectError::Other(error.to_string()))?;
        Ok(Self {
            stream: BufReader::new(stream),
            next_id: 1,
        })
    }

    /// Bounds how long a single call may wait for its reply. Used by the
    /// pre-tool hook: an exact boundary is worth milliseconds, not seconds.
    pub fn set_deadline(&mut self, deadline: std::time::Duration) {
        let _ = self.stream.get_ref().set_read_timeout(Some(deadline));
        let _ = self.stream.get_ref().set_write_timeout(Some(deadline));
    }

    pub fn call(&mut self, op: proto::Op) -> Result<proto::Reply, String> {
        let id = self.next_id;
        self.next_id += 1;
        let name = format!("{op:?}");
        let name = name.split([' ', '{', '(']).next().unwrap_or("?").to_owned();
        let started = std::time::Instant::now();
        acyclic_engine::trace!("client", "call #{id} {name}");
        let result = self.call_inner(id, op);
        match &result {
            Ok(reply) => {
                let reply_name = format!("{reply:?}");
                let reply_name = reply_name.split([' ', '{', '(']).next().unwrap_or("?");
                acyclic_engine::trace!(
                    "client",
                    "call #{id} {name} -> {reply_name} in {:.1}ms",
                    acyclic_engine::trace::ms(started)
                );
            }
            Err(message) => acyclic_engine::trace!(
                "client",
                "call #{id} {name} -> error in {:.1}ms: {}",
                acyclic_engine::trace::ms(started),
                message.lines().next().unwrap_or("")
            ),
        }
        result
    }

    fn call_inner(&mut self, id: u64, op: proto::Op) -> Result<proto::Reply, String> {
        let request = proto::Request {
            v: proto::PROTOCOL_VERSION,
            id,
            op,
        };
        let mut line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        line.push(b'\n');
        self.stream
            .get_mut()
            .write_all(&line)
            .map_err(|error| format!("send: {error}"))?;
        let mut response_line = String::new();
        self.stream
            .read_line(&mut response_line)
            .map_err(|error| format!("receive: {error}"))?;
        // A daemon that exits mid-answer closes the socket, so the read
        // succeeds with nothing. Left to serde that surfaced as
        // "decode: EOF while parsing a value at line 1 column 0", which reads
        // like corruption rather than what it is: the daemon stopped. Anyone
        // running `stop` and then any other verb hit it.
        parse_response(&response_line)
    }
}

/// One response line to a reply. Split out from the socket so the
/// shutdown case can be tested without a daemon.
fn parse_response(line: &str) -> Result<proto::Reply, String> {
    if line.trim().is_empty() {
        return Err("daemon stopped while answering; nothing was recorded".to_owned());
    }
    let response: proto::Response =
        serde_json::from_str(line).map_err(|error| format!("decode: {error}"))?;
    match response.payload {
        proto::Payload::Ok(reply) => Ok(*reply),
        proto::Payload::Err { message } => Err(message),
    }
}

fn spawn_daemon(repo_root: &Path, log_path: &Path) -> Result<std::process::Child, ConnectError> {
    let exe = std::env::current_exe().map_err(|error| ConnectError::Other(error.to_string()))?;
    let log = std::fs::File::create(log_path)
        .map_err(|error| ConnectError::Other(format!("daemon log: {error}")))?;
    let _detached = detach_stdio();
    let mut command = std::process::Command::new(exe);
    command
        .arg("__daemon")
        .arg(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // The daemon must not stand in the repo it manages. A process's working
    // directory is an open handle to that directory on Windows, and a rewind
    // renames the repo root — with the daemon sitting there (it inherits our
    // cwd, and we are usually inside the repo), every rewind and promote
    // fails with a sharing violation. The daemon takes the repo as an
    // absolute argument and never resolves a relative path, so the store
    // directory — which no swap touches — is a safe place to stand.
    //
    // Windows-only to keep this branch's invariant that Unix behaviour is
    // unchanged. Unix would arguably benefit too (the daemon's cwd follows
    // the old tree into the trash after a rewind), but that is a separate
    // change with its own acceptance run.
    #[cfg(windows)]
    if let Some(store_root) = log_path.parent() {
        command.current_dir(store_root);
    }
    command
        .spawn()
        .map_err(|error| ConnectError::Other(format!("spawn daemon: {error}")))
}

/// Keeps the daemon from inheriting *this* process's stdio.
///
/// Redirecting the child's three standard handles is not enough on Windows.
/// `CreateProcess` is called with `bInheritHandles = TRUE`, which duplicates
/// every inheritable handle in the parent — our own stdout among them — into
/// a daemon that then outlives us. A caller reading our output through a
/// pipe (which is what a host hook does) never sees EOF, because the daemon
/// is still holding the write end: `acyclic init | cat` hangs forever while
/// `acyclic init > file` returns at once.
///
/// Clearing the inherit flag for the duration of the spawn is the fix. The
/// child gets the explicit `Stdio` handles set above and nothing else; the
/// guard restores our flags so later spawns and our own output are unharmed.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "reads and clears HANDLE_FLAG_INHERIT on this process's own live standard handles"
)]
fn detach_stdio() -> StdioInheritance {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};

    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    let mut restore = Vec::new();
    for handle in handles {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE.cast() {
            continue;
        }
        let mut flags = 0_u32;
        // SAFETY: `handle` is a live standard handle owned by this process
        // and checked non-null above; both calls only read or write its
        // inherit flag and borrow nothing past the call.
        let inheritable = unsafe {
            windows_sys::Win32::Foundation::GetHandleInformation(handle.cast(), &raw mut flags) != 0
                && flags & HANDLE_FLAG_INHERIT != 0
        };
        if !inheritable {
            continue;
        }
        // SAFETY: as above; clears exactly the inherit bit.
        let cleared = unsafe {
            windows_sys::Win32::Foundation::SetHandleInformation(
                handle.cast(),
                HANDLE_FLAG_INHERIT,
                0,
            ) != 0
        };
        if cleared {
            restore.push(handle);
        }
    }
    StdioInheritance { restore }
}

/// Guard held across the spawn, restoring the inherit flags [`detach_stdio`]
/// cleared. Carries nothing on Unix, where nothing was cleared; it exists
/// there so the call site reads the same on both platforms.
pub(crate) struct StdioInheritance {
    #[cfg(windows)]
    restore: Vec<std::os::windows::io::RawHandle>,
}

#[cfg(windows)]
impl Drop for StdioInheritance {
    #[allow(
        unsafe_code,
        reason = "restores HANDLE_FLAG_INHERIT on the same handles detach_stdio cleared it on"
    )]
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
        for handle in self.restore.drain(..) {
            // SAFETY: each handle was live and inheritable a moment ago in
            // `detach_stdio`; this puts back exactly the bit it cleared.
            unsafe {
                windows_sys::Win32::Foundation::SetHandleInformation(
                    handle.cast(),
                    HANDLE_FLAG_INHERIT,
                    HANDLE_FLAG_INHERIT,
                );
            }
        }
    }
}

/// Unix inherits only what `Command` is told to pass, so there is nothing to
/// detach and nothing to restore.
#[cfg(not(windows))]
const fn detach_stdio() -> StdioInheritance {
    StdioInheritance {}
}

/// Waits for the daemon socket. The first baseline of a big repo can take a
/// while, so success is patient — but a daemon that exits without binding
/// fails fast with its log.
fn wait_for_socket(
    socket: &Path,
    mut child: std::process::Child,
    log_path: &Path,
    bound: Option<Duration>,
) -> Result<Client, ConnectError> {
    let started = Instant::now();
    let deadline = bound.unwrap_or(Duration::from_secs(30 * 60));
    let mut reported = false;
    loop {
        if let Ok(stream) = ClientStream::connect(socket)
            && let Ok(mut client) = Client::from_stream(stream)
        {
            // The daemon binds its socket before it opens the store, so
            // a connect can succeed while the ping waits on the store
            // open; bound the ping too so a caller with a bound never
            // sits on it.
            client.set_deadline(bound.unwrap_or(Duration::from_secs(60)));
            if client.call(proto::Op::Ping).is_ok() {
                client.set_deadline(Duration::from_secs(24 * 60 * 60));
                return Ok(client);
            }
        }
        if bound.is_some_and(|bound| started.elapsed() > bound) {
            acyclic_engine::trace!(
                "client",
                "daemon still starting after {:.1}ms; not waiting",
                acyclic_engine::trace::ms(started)
            );
            return Err(ConnectError::Starting);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            let tail: String = log.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
            return Err(ConnectError::Other(format!(
                "daemon exited ({status}) before serving: {tail}"
            )));
        }
        if started.elapsed() > deadline {
            return Err(ConnectError::Other(format!(
                "daemon did not become ready (log: {})",
                log_path.display()
            )));
        }
        if bound.is_none() && started.elapsed() > Duration::from_secs(2) && !reported {
            eprintln!("{NAME}: daemon starting (building the first snapshot of the tree)...");
            reported = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_socket_says_the_daemon_stopped() {
        // The regression: a daemon that exits mid-answer closes the socket, the
        // read succeeds with nothing, and serde called that
        // "decode: EOF while parsing a value at line 1 column 0" — which reads
        // as corruption. Anyone running `stop` then any other verb saw it.
        for line in ["", "\n", "   \n"] {
            let error = parse_response(line).expect_err("empty must be an error");
            assert!(
                error.contains("daemon stopped"),
                "unhelpful message for {line:?}: {error}"
            );
            assert!(!error.contains("decode"), "leaked serde wording: {error}");
        }
    }

    #[test]
    fn malformed_json_still_reports_a_decode_error() {
        // Genuine corruption must stay distinguishable from a clean shutdown.
        let error = parse_response("{not json").expect_err("must be an error");
        assert!(error.starts_with("decode:"), "{error}");
    }

    #[test]
    fn an_error_payload_surfaces_its_own_message() {
        // Built from the protocol types and serialized, rather than a
        // hand-written literal: the payload is flattened and renamed, so a
        // literal here would test my guess at the wire format instead of the
        // format. The first attempt did exactly that and failed.
        let line = serde_json::to_string(&proto::Response {
            id: 1,
            payload: proto::Payload::Err {
                message: "no such checkpoint".to_owned(),
            },
        })
        .expect("serialize");
        let error = parse_response(&line).expect_err("must be an error");
        assert_eq!(error, "no such checkpoint");
    }

    #[test]
    fn an_ok_payload_round_trips() {
        let line = serde_json::to_string(&proto::Response {
            id: 1,
            payload: proto::Payload::Ok(Box::new(proto::Reply::Pong)),
        })
        .expect("serialize");
        assert!(matches!(
            parse_response(&line).expect("ok payload"),
            proto::Reply::Pong
        ));
    }
}
