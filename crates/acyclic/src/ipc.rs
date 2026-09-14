//! Daemon transport: one request → one response, newline-delimited JSON.
//!
//! Unix uses a domain socket under the runtime directory. Windows uses a
//! named pipe derived from the same path, because `AF_UNIX` is not available
//! to `std` or to tokio there. Both carry the identical wire protocol; only
//! the endpoint naming, the "already running" check, and teardown differ.
//!
//! Callers name the endpoint with the socket path from
//! [`acyclic_engine::store::Paths::socket`] on every platform. On Windows
//! that path is never created on disk — it only supplies the stable,
//! per-store name the pipe is built from.

use std::io;
use std::path::Path;

/// How the endpoint should be described in traces and errors. On Unix this
/// is the socket path; on Windows, the pipe the path maps to.
pub fn endpoint_display(socket: &Path) -> String {
    #[cfg(unix)]
    {
        socket.display().to_string()
    }
    #[cfg(windows)]
    {
        pipe_name(socket)
    }
}

/// Removes a stale endpoint left by a dead daemon. A named pipe disappears
/// with the process that owned it, so this is Unix-only work.
pub fn cleanup(socket: &Path) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket);
    }
    #[cfg(windows)]
    {
        let _ = socket;
    }
}

/// `\\.\pipe\<product>-<store key>`. The store key is the socket file name,
/// which is already unique per store; any character a pipe name forbids is
/// replaced so the mapping stays total.
#[cfg(windows)]
fn pipe_name(socket: &Path) -> String {
    let key = socket.file_name().map_or_else(
        || "default".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let key: String = key
        .chars()
        .map(|c| {
            if c == '\\' || c == '/' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!(r"\\.\pipe\{}-{key}", acyclic_engine::product::NAME)
}

// ---------------------------------------------------------------- client

/// Blocking client end of the transport.
pub struct ClientStream {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    inner: std::fs::File,
}

impl ClientStream {
    /// Connects to a daemon that is already serving. A missing endpoint is an
    /// ordinary error: the caller decides whether to spawn one.
    pub fn connect(socket: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: std::os::unix::net::UnixStream::connect(socket)?,
            })
        }
        #[cfg(windows)]
        {
            // A named pipe is opened like a file. Every server instance being
            // momentarily busy is normal under concurrent hooks, so a short
            // bounded retry stands in for WaitNamedPipe.
            const BUSY: i32 = 231; // ERROR_PIPE_BUSY
            let name = pipe_name(socket);
            let mut last = None;
            for _ in 0..20 {
                match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&name)
                {
                    Ok(file) => return Ok(Self { inner: file }),
                    Err(error) => {
                        if error.raw_os_error() != Some(BUSY) {
                            return Err(error);
                        }
                        last = Some(error);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
            Err(last.unwrap_or_else(|| io::Error::other("pipe busy")))
        }
    }

    /// Bounds a single read. Windows named pipes opened as files carry no
    /// per-handle timeout, so this is a no-op there — see the deadline note
    /// in `docs/windows-verification.md`.
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.set_read_timeout(timeout)
        }
        #[cfg(windows)]
        {
            let _ = timeout;
            Ok(())
        }
    }

    /// Bounds a single write. No-op on Windows, as for reads.
    pub fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.set_write_timeout(timeout)
        }
        #[cfg(windows)]
        {
            let _ = timeout;
            Ok(())
        }
    }
}

impl io::Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------- server

/// The connected server end handed to one client session.
#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// Accepts client connections. Binding fails when a daemon is already serving
/// this store, which is how a second daemon refuses itself on both platforms:
/// Unix by `bind` after the stale-socket removal, Windows by
/// `first_pipe_instance`.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    name: String,
    #[cfg(windows)]
    idle: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl Listener {
    pub fn bind(socket: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: tokio::net::UnixListener::bind(socket)?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let name = pipe_name(socket);
            let idle = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)?;
            Ok(Self { name, idle })
        }
    }

    /// Waits for one client. The Windows form keeps exactly one unconnected
    /// instance alive at all times: the idle instance is handed out on
    /// connect and immediately replaced, so there is never a window in which
    /// a client finds no instance to open.
    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            Ok(stream)
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            self.idle.connect().await?;
            let next = ServerOptions::new().create(&self.name)?;
            Ok(std::mem::replace(&mut self.idle, next))
        }
    }
}
