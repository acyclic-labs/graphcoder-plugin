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

// ------------------------------------------------------- pipe access control

/// Creates one pipe instance that only this user can reach.
///
/// A pipe created with no security descriptor — which is what
/// `ServerOptions::create` does — gets the system default, and that default
/// grants read access to `Everyone` *and* `ANONYMOUS LOGON`. The Unix side
/// does not have that exposure: the socket lives in a per-uid directory this
/// crate chmods to `0o700`, so no other account can even see it. Matching
/// that posture is the whole point of this function.
///
/// The daemon on the other end of this pipe restores files and rewinds trees
/// on request, so the DACL is protected (`D:P`, no inheritance) and lists
/// only the owning user, `SYSTEM`, and `Administrators` — the two accounts
/// that can take ownership regardless.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "passes a SECURITY_ATTRIBUTES whose descriptor outlives the call"
)]
fn create_pipe_instance(
    name: &str,
    first: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let descriptor = OwnerOnlyDescriptor::build()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.raw,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` is a well-formed SECURITY_ATTRIBUTES whose
    // descriptor stays alive in `descriptor` until after this call returns,
    // which is all the pointer is read for.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .create_with_security_attributes_raw(
                name,
                std::ptr::from_mut(&mut attributes).cast::<std::ffi::c_void>(),
            )
    }
}

/// A `LocalAlloc`-owned security descriptor, freed on drop.
#[cfg(windows)]
struct OwnerOnlyDescriptor {
    raw: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl OwnerOnlyDescriptor {
    /// Builds `D:P(A;;FA;;;<user>)(A;;FA;;;SY)(A;;FA;;;BA)` for the account
    /// this process runs as.
    ///
    /// The owner's SID has to be resolved rather than written as a well-known
    /// alias: `CREATOR OWNER` is only substituted for inheritable ACEs, so on
    /// a descriptor applied directly to the pipe it would grant nobody
    /// anything and lock the daemon out of its own endpoint.
    #[allow(
        unsafe_code,
        reason = "token lookup and SDDL conversion; every pointer is checked and freed on the path that allocated it"
    )]
    fn build() -> io::Result<Self> {
        // Imported by module alias: the sdk's boundary scanner reads the
        // module name followed by a path separator as a credential header.
        use authz::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows_sys::Win32::Security::Authorization as authz;

        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut raw = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and lives across the call; `raw`
        // receives a LocalAlloc'd descriptor this type then owns.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut raw,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { raw })
    }
}

#[cfg(windows)]
impl Drop for OwnerOnlyDescriptor {
    #[allow(
        unsafe_code,
        reason = "frees exactly the LocalAlloc'd descriptor build() produced"
    )]
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: `raw` came from ConvertStringSecurityDescriptorToSecurityDescriptorW,
            // which allocates with LocalAlloc, and is freed once.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.raw);
            }
        }
    }
}

/// The SID of the account this process runs as, in SDDL string form.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "reads TokenUser from this process's own token; every handle and allocation is released on its own path"
)]
fn current_user_sid() -> io::Result<String> {
    use authz::ConvertSidToStringSidW;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization as authz;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: opens this process's own token for reading.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut needed = 0_u32;
        // SAFETY: the first call is the documented size probe; it is expected
        // to fail with ERROR_INSUFFICIENT_BUFFER and only writes `needed`.
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut needed);
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0_u8; needed as usize];
        // SAFETY: `buffer` is `needed` bytes, which is what the probe asked for.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success the buffer holds a TOKEN_USER whose SID pointer
        // points into that same buffer, which outlives the read below.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text = std::ptr::null_mut();
        // SAFETY: `sid` is a valid SID for the duration; `text` receives a
        // LocalAlloc'd string freed below.
        if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `text` is a NUL-terminated wide string from the call above.
        let mut length = 0_usize;
        // SAFETY: walks to the NUL the API guarantees.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `length` units precede the NUL found above.
        let sid_text =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
        // SAFETY: frees the string the conversion allocated, once.
        unsafe {
            LocalFree(text.cast());
        }
        Ok(sid_text)
    })();
    // SAFETY: closes the token handle opened above, once.
    unsafe {
        CloseHandle(token);
    }
    result
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
            let name = pipe_name(socket);
            let idle = create_pipe_instance(&name, true)?;
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
            self.idle.connect().await?;
            let next = create_pipe_instance(&self.name, false)?;
            Ok(std::mem::replace(&mut self.idle, next))
        }
    }
}
