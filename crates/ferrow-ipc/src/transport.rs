//! Platform transports and OS-derived caller identity (M2a).
//!
//! - Windows: a named pipe (`\\.\pipe\ferrowd` for the service)
//!   created with an explicit security descriptor — LocalSystem
//!   and administrators full control, authenticated users
//!   connect/read/write — and `PIPE_REJECT_REMOTE_CLIENTS`, so
//!   only local callers can connect at all.
//! - Linux: a Unix domain socket at `/run/ferrow/ferrowd.sock`;
//!   macOS: `/var/run/ferrow/ferrowd.sock`. The socket file is
//!   mode 0666 so every local user may connect; *what a caller
//!   may see* is enforced per request from the OS identity
//!   (M2c), never from anything the client claims.
//!
//! Caller identity comes only from the OS: Windows
//! `GetNamedPipeClientProcessId` plus the client token's user SID,
//! Linux `SO_PEERCRED`, macOS `getpeereid`.

use std::io::{Read, Write};
#[cfg(unix)]
use std::path::PathBuf;

/// Where a service listens and clients connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    #[cfg(windows)]
    pipe_name: String,
    #[cfg(unix)]
    path: PathBuf,
}

impl Endpoint {
    /// The system service endpoint: `\\.\pipe\ferrowd` on Windows,
    /// `/run/ferrow/ferrowd.sock` on Linux,
    /// `/var/run/ferrow/ferrowd.sock` on macOS.
    pub fn default_service() -> Endpoint {
        #[cfg(windows)]
        {
            Endpoint {
                pipe_name: r"\\.\pipe\ferrowd".to_owned(),
            }
        }
        #[cfg(target_os = "linux")]
        {
            Endpoint {
                path: PathBuf::from("/run/ferrow/ferrowd.sock"),
            }
        }
        #[cfg(target_os = "macos")]
        {
            Endpoint {
                path: PathBuf::from("/var/run/ferrow/ferrowd.sock"),
            }
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
        {
            Endpoint {
                path: PathBuf::from("/tmp/ferrowd.sock"),
            }
        }
    }

    /// A per-user endpoint for unprivileged development runs
    /// (`ferrow service run`): pipe `ferrowd-user-<name>` on
    /// Windows, a socket of that name in the temp dir on Unix.
    pub fn user_service() -> Endpoint {
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".to_owned())
            .replace(
                |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "_",
            );
        Endpoint::named(&format!("ferrowd-user-{user}"))
    }

    /// An endpoint from a short name: `\\.\pipe\<name>` on
    /// Windows, `<temp dir>/<name>.sock` on Unix. Tests use a
    /// unique name per test so nothing privileged is touched.
    pub fn named(name: &str) -> Endpoint {
        #[cfg(windows)]
        {
            let pipe_name = if name.starts_with(r"\\") {
                name.to_owned()
            } else {
                format!(r"\\.\pipe\{name}")
            };
            Endpoint { pipe_name }
        }
        #[cfg(unix)]
        {
            Endpoint {
                path: std::env::temp_dir().join(format!("{name}.sock")),
            }
        }
    }

    /// An endpoint at an explicit Unix socket path.
    #[cfg(unix)]
    pub fn from_path(path: PathBuf) -> Endpoint {
        Endpoint { path }
    }

    /// Human-readable endpoint for logs and status output.
    pub fn display(&self) -> String {
        #[cfg(windows)]
        {
            self.pipe_name.clone()
        }
        #[cfg(unix)]
        {
            self.path.display().to_string()
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// Who is on the other end, as reported by the OS. Fields a
/// platform cannot provide stay `None`; `pid` is present on
/// Windows and Linux, `uid`/`gid` on Unix, `user_sid` (the token
/// user, e.g. `S-1-5-21-…`) on Windows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallerIdentity {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub user_sid: Option<String>,
}

#[cfg(unix)]
mod platform {
    use super::{CallerIdentity, Endpoint};
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    pub struct Listener {
        inner: UnixListener,
        path: std::path::PathBuf,
    }

    pub struct Stream {
        inner: UnixStream,
        caller: Option<CallerIdentity>,
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> std::io::Result<Listener> {
            use std::os::unix::fs::PermissionsExt;
            if let Some(parent) = endpoint.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // The endpoint is owned by this service instance: a
            // leftover socket file from a previous run is removed.
            match std::fs::remove_file(&endpoint.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let inner = UnixListener::bind(&endpoint.path)?;
            std::fs::set_permissions(&endpoint.path, std::fs::Permissions::from_mode(0o666))?;
            Ok(Listener {
                inner,
                path: endpoint.path.clone(),
            })
        }

        pub fn accept(&self) -> std::io::Result<(Stream, CallerIdentity)> {
            let (inner, _) = self.inner.accept()?;
            let caller = peer_identity(&inner)?;
            Ok((
                Stream {
                    inner,
                    caller: Some(caller.clone()),
                },
                caller,
            ))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Stream {
        pub fn connect(endpoint: &Endpoint) -> std::io::Result<Stream> {
            Ok(Stream {
                inner: UnixStream::connect(&endpoint.path)?,
                caller: None,
            })
        }

        pub fn caller(&self) -> Option<&CallerIdentity> {
            self.caller.as_ref()
        }

        pub fn shutdown(&self) -> std::io::Result<()> {
            self.inner.shutdown(std::net::Shutdown::Both)
        }
    }

    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    #[cfg(target_os = "linux")]
    fn peer_identity(stream: &UnixStream) -> std::io::Result<CallerIdentity> {
        use std::os::unix::io::AsRawFd;
        // SAFETY: zeroed ucred is a valid getsockopt output buffer;
        // the fd is the live socket owned by `stream`.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: buffer pointer and length describe `cred`
        // exactly and it outlives the call.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(CallerIdentity {
            pid: u32::try_from(cred.pid).ok(),
            uid: Some(cred.uid),
            gid: Some(cred.gid),
            user_sid: None,
        })
    }

    #[cfg(target_os = "macos")]
    fn peer_identity(stream: &UnixStream) -> std::io::Result<CallerIdentity> {
        use std::os::unix::io::AsRawFd;
        let (mut uid, mut gid) = (0 as libc::uid_t, 0 as libc::gid_t);
        // SAFETY: output pointers point at live uid/gid values;
        // the fd is the live socket owned by `stream`.
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(CallerIdentity {
            pid: None,
            uid: Some(uid),
            gid: Some(gid),
            user_sid: None,
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn peer_identity(_stream: &UnixStream) -> std::io::Result<CallerIdentity> {
        Ok(CallerIdentity::default())
    }
}

#[cfg(windows)]
mod platform {
    use super::{CallerIdentity, Endpoint};
    use std::io::{Read, Write};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
        INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_READ_DATA, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
    };

    /// SDDL_REVISION_1.
    const SDDL_REVISION_1: u32 = 1;
    /// Service pipe DACL: LocalSystem (SY) and built-in
    /// administrators (BA) full control; authenticated users (AU)
    /// generic read/write (connect). Nobody else gets access.
    // AU data/attribute/synchronization rights exclude FILE_CREATE_PIPE_INSTANCE.
    // Owner rights allow an unprivileged dev listener to create successors.
    const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12019b;;;AU)";
    const PIPE_INSTANCES: u32 = 33; // 32 connected streams plus one pending instance

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn last_error() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    /// An owned pipe handle; disconnected and closed on drop.
    struct PipeHandle(HANDLE);

    // SAFETY: this wrapper uniquely owns the kernel handle. Moving it transfers
    // ownership; it is never cloned or exposed for concurrent I/O. Windows pipe
    // handles are not thread-affine. This does not implement Sync.
    unsafe impl Send for PipeHandle {}

    impl Drop for PipeHandle {
        fn drop(&mut self) {
            // Closing releases ownership without waiting for peer reads.
            // Do not FlushFileBuffers or DisconnectNamedPipe here: the former
            // waits indefinitely, and the latter discards queued refusals.
            // SAFETY: the handle is owned here and closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Build SECURITY_ATTRIBUTES from the SDDL above. The returned
    /// descriptor must be freed with LocalFree after the pipe
    /// instance is created (the kernel copies it at creation).
    fn security_attributes() -> std::io::Result<(SECURITY_ATTRIBUTES, *mut core::ffi::c_void)> {
        let owner = token_user_sid(std::process::id()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cannot identify pipe owner",
            )
        })?;
        let sddl = wide(&format!("O:{owner}{PIPE_SDDL}"));
        let mut sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is a NUL-terminated wide string that
        // outlives the call; `sd` is a live output pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok((
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            },
            sd,
        ))
    }

    /// Best-effort user SID string from the client process token.
    /// The pid stays authoritative when this fails (for example
    /// for a protected process).
    fn token_user_sid(pid: u32) -> Option<String> {
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{TOKEN_QUERY, TOKEN_USER};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: returns a handle we own and close, or null.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `process` is live; `token` is a live output.
        let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) };
        // SAFETY: `process` is owned here and closed once.
        unsafe { CloseHandle(process) };
        if ok == 0 || token.is_null() {
            return None;
        }
        let sid = token_user(token).and_then(|buf| {
            let _ = &buf;
            // SAFETY: GetTokenInformation initialized at least TOKEN_USER bytes.
            // Vec<usize> provides TOKEN_USER's pointer alignment, and `buf`
            // keeps both the structure and its embedded SID alive below.
            let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
            let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
            // SAFETY: the SID pointer comes from the token buffer;
            // `text` is a live output freed with LocalFree.
            let ok = unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) };
            if ok == 0 || text.is_null() {
                return None;
            }
            // SAFETY: `text` is a NUL-terminated wide string
            // owned by us until LocalFree below.
            let mut len = 0;
            while unsafe { *text.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: the slice covers exactly the string above.
            let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
            // SAFETY: frees the string allocated by the convert call.
            unsafe { LocalFree(text.cast()) };
            Some(out)
        });
        // SAFETY: `token` is owned here and closed once.
        unsafe { CloseHandle(token) };
        sid
    }

    fn token_user(token: HANDLE) -> Option<Vec<usize>> {
        use windows_sys::Win32::Security::{GetTokenInformation, TokenUser};
        let mut len = 0u32;
        // SAFETY: sizing call; a null buffer with length 0 is the
        // documented way to learn the required buffer size.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut len) };
        if (len as usize) < size_of::<windows_sys::Win32::Security::TOKEN_USER>() {
            return None;
        }
        let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        // SAFETY: storage is pointer-aligned and holds at least len bytes.
        // The API writes only len bytes, including its embedded SID; all
        // allocated words are initialized and the buffer outlives parsing.
        let ok = unsafe {
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &raw mut len)
        };
        if ok == 0 { None } else { Some(buf) }
    }

    fn client_identity(handle: HANDLE) -> std::io::Result<CallerIdentity> {
        let mut pid = 0u32;
        // SAFETY: `handle` is a connected server pipe; `pid` is a
        // live output.
        let ok = unsafe { GetNamedPipeClientProcessId(handle, &raw mut pid) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(CallerIdentity {
            pid: Some(pid),
            uid: None,
            gid: None,
            user_sid: token_user_sid(pid),
        })
    }

    pub struct Listener {
        name: Vec<u16>,
        pending: std::sync::Mutex<PipeHandle>,
    }

    pub struct Stream {
        handle: Option<PipeHandle>,
        caller: Option<CallerIdentity>,
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> std::io::Result<Listener> {
            let name = wide(&endpoint.pipe_name);
            let pending = create_instance(&name, true)?;
            Ok(Listener {
                name,
                pending: std::sync::Mutex::new(pending),
            })
        }

        pub fn accept(&self) -> std::io::Result<(Stream, CallerIdentity)> {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| std::io::Error::other("listener poisoned"))?;
            let handle = pending.0;
            // SAFETY: the mutex exclusively owns this live pending instance.
            let ok = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            if ok == 0 && last_error().raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(last_error());
            }
            // Retain continuous name ownership before handing off this stream.
            let next = create_instance(&self.name, false)?;
            let owned = std::mem::replace(&mut *pending, next);
            let caller = client_identity(handle)?;
            Ok((
                Stream {
                    handle: Some(owned),
                    caller: Some(caller.clone()),
                },
                caller,
            ))
        }
    }

    fn create_instance(name: &[u16], first: bool) -> std::io::Result<PipeHandle> {
        let (mut attrs, sd) = security_attributes()?;
        // SAFETY: NUL-terminated name and descriptor live through creation;
        // successful handle ownership transfers to PipeHandle.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        0
                    },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_INSTANCES,
                super::MAX_PIPE_BUFFER as u32,
                super::MAX_PIPE_BUFFER as u32,
                0,
                &raw mut attrs,
            )
        };
        let error = last_error();
        // SAFETY: descriptor was allocated by ConvertStringSecurityDescriptor.
        unsafe { LocalFree(sd) };
        if handle == INVALID_HANDLE_VALUE {
            Err(error)
        } else {
            Ok(PipeHandle(handle))
        }
    }

    impl Stream {
        pub fn connect(endpoint: &Endpoint) -> std::io::Result<Stream> {
            let name = wide(&endpoint.pipe_name);
            for _attempt in 0..50 {
                // SAFETY: name outlives the call; the returned
                // handle is owned by PipeHandle on success.
                let handle = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        FILE_READ_DATA | FILE_WRITE_DATA | 0x120000,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if handle != INVALID_HANDLE_VALUE {
                    let owned = PipeHandle(handle);
                    authenticate_server(handle, endpoint)?;
                    return Ok(Stream {
                        handle: Some(owned),
                        caller: None,
                    });
                }
                let err = last_error();
                if err.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
                    return Err(err);
                }
                // SAFETY: name outlives the bounded wait.
                unsafe { WaitNamedPipeW(name.as_ptr(), 2_000) };
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "named pipe stays busy",
            ))
        }

        pub fn caller(&self) -> Option<&CallerIdentity> {
            self.caller.as_ref()
        }

        pub fn shutdown(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn raw(&self) -> HANDLE {
            self.handle
                .as_ref()
                .map(|h| h.0)
                .unwrap_or(std::ptr::null_mut())
        }
    }

    fn authenticate_server(handle: HANDLE, endpoint: &Endpoint) -> std::io::Result<()> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
        let mut owner = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        // SAFETY: live pipe handle with READ_CONTROL; all outputs live through call.
        let code = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut sd,
            )
        };
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code as i32));
        }
        let mut text = std::ptr::null_mut();
        // SAFETY: owner points into the returned descriptor, retained below.
        let ok = unsafe { ConvertSidToStringSidW(owner, &raw mut text) };
        let mut actual = String::new();
        if ok != 0 {
            let mut len = 0;
            // SAFETY: conversion returns a NUL-terminated allocated SID string.
            unsafe {
                while *text.add(len) != 0 {
                    len += 1;
                }
                actual = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                LocalFree(text.cast());
            }
        }
        // SAFETY: GetSecurityInfo allocated this descriptor; owner is no longer used.
        unsafe { LocalFree(sd) };
        let expected = if *endpoint == Endpoint::default_service() {
            "S-1-5-18".to_owned()
        } else {
            token_user_sid(std::process::id()).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cannot identify client",
                )
            })?
        };
        if ok == 0 || actual != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "untrusted pipe server owner",
            ));
        }
        Ok(())
    }

    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut read = 0u32;
            // SAFETY: buffer pointer/length describe `buf`; the
            // handle is live and owned by this stream.
            let ok = unsafe {
                ReadFile(
                    self.raw(),
                    buf.as_mut_ptr().cast(),
                    u32::try_from(buf.len()).unwrap_or(u32::MAX),
                    &raw mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = last_error();
                if err.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    return Ok(0);
                }
                return Err(err);
            }
            Ok(read as usize)
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut written = 0u32;
            // SAFETY: buffer pointer/length describe `buf`; the
            // handle is live and owned by this stream.
            let ok = unsafe {
                WriteFile(
                    self.raw(),
                    buf.as_ptr().cast(),
                    u32::try_from(buf.len()).unwrap_or(u32::MAX),
                    &raw mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(last_error());
            }
            Ok(written as usize)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            // Nothing is buffered on our side; WriteFile hands the
            // bytes to the pipe directly. (FlushFileBuffers on a
            // pipe would block until the peer reads, which a
            // streaming writer must not require.)
            Ok(())
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn token_buffer_has_token_user_allocation_alignment() {
            use windows_sys::Win32::Security::{TOKEN_QUERY, TOKEN_USER};
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
            let mut token = std::ptr::null_mut();
            // SAFETY: pseudo-process handle is valid; output is live, closed below.
            assert_ne!(
                unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) },
                0
            );
            let buffer = token_user(token).unwrap();
            // SAFETY: this test owns the token handle.
            unsafe { CloseHandle(token) };
            assert!(std::mem::align_of_val(&buffer[0]) >= align_of::<TOKEN_USER>());
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{CallerIdentity, Endpoint};
    use std::io::{Read, Write};

    fn unsupported() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no IPC transport on this platform",
        )
    }

    pub struct Listener;
    pub struct Stream;

    impl Listener {
        pub fn bind(_endpoint: &Endpoint) -> std::io::Result<Listener> {
            Err(unsupported())
        }
        pub fn accept(&self) -> std::io::Result<(Stream, CallerIdentity)> {
            Err(unsupported())
        }
    }

    impl Stream {
        pub fn connect(_endpoint: &Endpoint) -> std::io::Result<Stream> {
            Err(unsupported())
        }
        pub fn caller(&self) -> Option<&CallerIdentity> {
            None
        }
        pub fn shutdown(&self) -> std::io::Result<()> {
            Err(unsupported())
        }
    }

    impl Read for Stream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(unsupported())
        }
    }

    impl Write for Stream {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(unsupported())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(unsupported())
        }
    }
}

/// Pipe buffer size hint (one max frame) for the Windows server.
#[cfg(windows)]
const MAX_PIPE_BUFFER: usize = crate::protocol::MAX_FRAME_BYTES;

/// A bound server endpoint. Blocking; run the accept loop on its
/// own thread, one thread per accepted connection.
pub struct Listener(platform::Listener);

/// One connected stream (either side).
pub struct Stream(platform::Stream);

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> std::io::Result<Listener> {
        Ok(Listener(platform::Listener::bind(endpoint)?))
    }

    /// Accept one connection, with the caller's OS identity.
    pub fn accept(&self) -> std::io::Result<(Stream, CallerIdentity)> {
        let (stream, caller) = self.0.accept()?;
        Ok((Stream(stream), caller))
    }
}

impl Stream {
    pub fn connect(endpoint: &Endpoint) -> std::io::Result<Stream> {
        Ok(Stream(platform::Stream::connect(endpoint)?))
    }

    /// The caller's OS identity, on the accepted (server) side.
    pub fn caller(&self) -> Option<&CallerIdentity> {
        self.0.caller()
    }

    pub fn shutdown(&self) -> std::io::Result<()> {
        self.0.shutdown()
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
