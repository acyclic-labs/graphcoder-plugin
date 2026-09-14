//! Synchronous client: connect to the daemon socket, one request → response.
//! Interactive verbs may spawn a dead daemon; hook-invoked calls never do.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use acyclic_engine::product::NAME;
use acyclic_proto as proto;

pub enum Spawn {
    /// Interactive: start the daemon if it isn't running (waits for baseline).
    Allowed,
    /// Hook path: never spawn; a missing daemon is a warning-and-exit-2.
    Never,
}

pub struct Client {
    stream: BufReader<UnixStream>,
    next_id: u64,
}

pub enum ConnectError {
    /// No daemon and spawning was not allowed.
    NoDaemon,
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
        if let Ok(stream) = UnixStream::connect(socket) {
            acyclic_engine::trace!(
                "client",
                "connected to running daemon at {} in {:.1}ms",
                socket.display(),
                acyclic_engine::trace::ms(started)
            );
            return Self::from_stream(stream);
        }
        match spawn {
            Spawn::Never => {
                acyclic_engine::trace!(
                    "client",
                    "no daemon at {} and spawning is not allowed here",
                    socket.display()
                );
                Err(ConnectError::NoDaemon)
            }
            Spawn::Allowed => {
                acyclic_engine::trace!("client", "no daemon at {}: spawning one", socket.display());
                let child = spawn_daemon(repo_root, log_path)?;
                let client = wait_for_socket(socket, child, log_path);
                acyclic_engine::trace!(
                    "client",
                    "daemon spawn + socket wait took {:.1}ms",
                    acyclic_engine::trace::ms(started)
                );
                client
            }
        }
    }

    fn from_stream(stream: UnixStream) -> Result<Self, ConnectError> {
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
        let name = name
            .split([' ', '{', '('])
            .next()
            .unwrap_or("?")
            .to_string();
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
        let response: proto::Response =
            serde_json::from_str(&response_line).map_err(|error| format!("decode: {error}"))?;
        match response.payload {
            proto::Payload::Ok(reply) => Ok(*reply),
            proto::Payload::Err { message } => Err(message),
        }
    }
}

fn spawn_daemon(repo_root: &Path, log_path: &Path) -> Result<std::process::Child, ConnectError> {
    let exe = std::env::current_exe().map_err(|error| ConnectError::Other(error.to_string()))?;
    let log = std::fs::File::create(log_path)
        .map_err(|error| ConnectError::Other(format!("daemon log: {error}")))?;
    std::process::Command::new(exe)
        .arg("__daemon")
        .arg(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|error| ConnectError::Other(format!("spawn daemon: {error}")))
}

/// Waits for the daemon socket. The first baseline of a big repo can take a
/// while, so success is patient — but a daemon that exits without binding
/// fails fast with its log.
fn wait_for_socket(
    socket: &Path,
    mut child: std::process::Child,
    log_path: &Path,
) -> Result<Client, ConnectError> {
    let started = Instant::now();
    let deadline = Duration::from_secs(30 * 60);
    let mut reported = false;
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            if let Ok(mut client) = Client::from_stream(stream) {
                if client.call(proto::Op::Ping).is_ok() {
                    return Ok(client);
                }
            }
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
        if started.elapsed() > Duration::from_secs(2) && !reported {
            eprintln!("{NAME}: daemon starting (building the first snapshot of the tree)...");
            reported = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
