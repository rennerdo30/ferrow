//! Nonblocking connect, including stale-socket probes. A full backlog is a
//! refusal, never evidence that an existing socket is stale.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

pub(super) fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    super::remaining(deadline)?;
    // SAFETY: sockaddr_un is a C POD; zero fills padding and pathname terminator.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = *byte as libc::c_char;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: socket has no pointer arguments; ownership is taken immediately.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a newly allocated descriptor, owned exactly once.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: live descriptor, no pointer arguments. Set flags before connect.
    if unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(owned);
    stream.set_nonblocking(true)?;
    super::remaining(deadline)?;
    // SAFETY: initialized sockaddr with its exact pathname length; stream is live.
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&raw const address).cast(),
            length as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            // Linux AF_UNIX EAGAIN (full backlog) has no pending connection.
            // Return promptly and retain the existing socket during recovery.
            return Err(error);
        }
        finish_pending(&stream, deadline, super::platform::wait_io)?;
    }
    super::remaining(deadline)?;
    Ok(stream)
}

fn finish_pending(
    stream: &UnixStream,
    deadline: Instant,
    mut wait: impl FnMut(&UnixStream, i16, Instant) -> io::Result<()>,
) -> io::Result<()> {
    loop {
        super::remaining(deadline)?;
        match wait(stream, libc::POLLOUT, deadline) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        }
        return match stream.take_error()? {
            Some(error) => Err(error),
            None => Ok(()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_connect_uses_one_deadline_and_drops_owned_descriptors() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        let mut calls = 0;
        let error = finish_pending(&stream, deadline, |_, events, actual| {
            assert_eq!(events, libc::POLLOUT);
            assert_eq!(actual, deadline);
            calls += 1;
            Err(io::Error::from(if calls == 1 {
                io::ErrorKind::Interrupted
            } else {
                io::ErrorKind::TimedOut
            }))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls, 2);
        drop(stream);
        let mut byte = [0];
        assert_eq!(std::io::Read::read(&mut &peer, &mut byte).unwrap(), 0);
    }

    #[test]
    fn expired_connect_does_not_enter_the_os_connect() {
        let error = connect(Path::new("/nonexistent/ferrow.sock"), Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
