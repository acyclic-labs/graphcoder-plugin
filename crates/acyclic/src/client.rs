//! Synchronous client: connect to the daemon socket, one request → response.
//! Interactive verbs may spawn a dead daemon; hook-invoked calls never do.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

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
    pub fn connect(socket: &Path, repo_root: &Path, spawn: Spawn) -> Result<Self, ConnectError> {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Self::from_stream(stream);
        }
        match spawn {
            Spawn::Never => Err(ConnectError::NoDaemon),
            Spawn::Allowed => {
                spawn_daemon(repo_root)?;
                wait_for_socket(socket)
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

    pub fn call(&mut self, op: proto::Op) -> Result<proto::Reply, String> {
        let id = self.next_id;
        self.next_id += 1;
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
            proto::Payload::Ok(reply) => Ok(reply),
            proto::Payload::Err { message } => Err(message),
        }
    }
}

fn spawn_daemon(repo_root: &Path) -> Result<(), ConnectError> {
    let exe = std::env::current_exe().map_err(|error| ConnectError::Other(error.to_string()))?;
    std::process::Command::new(exe)
        .arg("__daemon")
        .arg(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| ConnectError::Other(format!("spawn daemon: {error}")))
}

/// Waits for the daemon socket; the first baseline of a big repo can take a
/// while, so this is generous and prints progress.
fn wait_for_socket(socket: &Path) -> Result<Client, ConnectError> {
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
        if started.elapsed() > deadline {
            return Err(ConnectError::Other(
                "daemon did not become ready (see store logs)".into(),
            ));
        }
        if started.elapsed() > Duration::from_secs(2) && !reported {
            eprintln!("acyclic: daemon starting (building the first snapshot of the tree)...");
            reported = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
