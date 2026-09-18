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
    #[cfg(windows)]
    read_timeout: std::cell::Cell<Option<std::time::Duration>>,
    #[cfg(windows)]
    write_timeout: std::cell::Cell<Option<std::time::Duration>>,
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
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
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
                    .custom_flags(FILE_FLAG_OVERLAPPED)
                    .open(&name)
                {
                    Ok(file) => {
                        return Ok(Self {
                            inner: file,
                            read_timeout: std::cell::Cell::new(None),
                            write_timeout: std::cell::Cell::new(None),
                        });
                    }
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

    /// Bounds a single read, including Windows overlapped named-pipe reads.
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.set_read_timeout(timeout)
        }
        #[cfg(windows)]
        {
            self.read_timeout.set(timeout);
            Ok(())
        }
    }

    /// Bounds a single write, including Windows overlapped named-pipe writes.
    pub fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.set_write_timeout(timeout)
        }
        #[cfg(windows)]
        {
            self.write_timeout.set(timeout);
            Ok(())
        }
    }
}

impl io::Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            self.inner.read(buf)
        }
        #[cfg(windows)]
        {
            pipe_io(
                &self.inner,
                buf.as_mut_ptr(),
                buf.len(),
                self.read_timeout.get(),
                true,
            )
        }
    }
}

impl io::Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            self.inner.write(buf)
        }
        #[cfg(windows)]
        {
            pipe_io(
                &self.inner,
                buf.as_ptr().cast_mut(),
                buf.len(),
                self.write_timeout.get(),
                false,
            )
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.flush()
        }
        #[cfg(windows)]
        {
            Ok(())
        }
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "owns the event and waits for completion or cancellation before releasing the caller's buffer and OVERLAPPED"
)]
fn pipe_io(
    file: &std::fs::File,
    buffer: *mut u8,
    len: usize,
    timeout: Option<std::time::Duration>,
    read: bool,
) -> io::Result<usize> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE};
    use windows_sys::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };

    if len == 0 {
        return Ok(0);
    }
    let handle = file.as_raw_handle().cast();
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    struct Event(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Event {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    let event = Event(event);
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.0;
    let count = u32::try_from(len).unwrap_or(u32::MAX);
    let submitted = if read {
        unsafe {
            ReadFile(
                handle,
                buffer,
                count,
                std::ptr::null_mut(),
                &raw mut overlapped,
            )
        }
    } else {
        unsafe {
            WriteFile(
                handle,
                buffer,
                count,
                std::ptr::null_mut(),
                &raw mut overlapped,
            )
        }
    };
    if submitted == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING.cast_signed()) {
            return Err(error);
        }
    }
    let milliseconds = timeout.map_or(INFINITE, |duration| {
        u32::try_from(duration.as_millis().max(1)).unwrap_or(INFINITE - 1)
    });
    let mut transferred = 0;
    let completed = unsafe {
        GetOverlappedResultEx(
            handle,
            &raw mut overlapped,
            &raw mut transferred,
            milliseconds,
            0,
        )
    };
    if completed != 0 {
        return Ok(transferred as usize);
    }
    let error = io::Error::last_os_error();
    // A failed wait may still leave the operation pending. Keep the caller's
    // buffer, OVERLAPPED, and event alive until cancellation has completed.
    unsafe {
        CancelIoEx(handle, &raw mut overlapped);
        GetOverlappedResult(handle, &raw mut overlapped, &raw mut transferred, 1);
    }
    if error.raw_os_error() == Some(WAIT_TIMEOUT.cast_signed()) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "named-pipe operation timed out",
        ));
    }
    Err(error)
}

#[cfg(all(test, windows))]
mod deadline_tests {
    use super::*;
    use std::io::Read as _;
    use std::time::{Duration, Instant};

    #[test]
    fn named_pipe_read_deadline_is_enforced() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let socket = std::path::PathBuf::from(format!("deadline-{}-{nonce}", std::process::id()));
        let name = pipe_name(&socket);
        let (ready, connected) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let pipe = create_pipe_instance(&name, true).expect("pipe");
                ready.send(()).expect("ready");
                pipe.connect().await.expect("connect");
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        });
        connected.recv().expect("server ready");
        let mut client = ClientStream::connect(&socket).expect("client connect");
        client
            .set_read_timeout(Some(Duration::from_millis(30)))
            .expect("deadline");
        let started = Instant::now();
        let error = client
            .read(&mut [0_u8; 1])
            .expect_err("read should time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(180));
        server.join().expect("server exit");
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
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

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
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
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
