//! Blocking client for the control channel.
//!
//! Deliberately synchronous: the guardian serves it from its own thread, and the
//! desktop shell runs it on a dedicated I/O worker rather than on the Tauri main
//! loop or a Tokio runtime thread. A blocking socket here is what lets the shell
//! guarantee it never blocks its event loop on an unresponsive daemon.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::CONTROL_REQUEST_TIMEOUT;
use crate::codec;
use crate::deadline::Deadline;
use crate::types::{ControlRequest, ControlResponse};

/// A connected control channel.
#[derive(Debug)]
pub struct ControlClient {
    stream: UnixStream,
}

impl ControlClient {
    /// Connect within `deadline`. The connect itself is bounded too: a socket
    /// file that exists but whose listener is wedged must not hang a quit.
    pub fn connect(path: &Path, deadline: &Deadline) -> io::Result<Self> {
        let budget = deadline.clamp(CONTROL_REQUEST_TIMEOUT);
        if budget.is_zero() {
            return Err(expired());
        }
        // UnixStream has no connect timeout on stable Rust, so the connect is done
        // with the socket in non-blocking mode and a bounded poll.
        let stream = connect_with_timeout(path, budget)?;
        stream.set_write_timeout(Some(budget))?;
        stream.set_read_timeout(Some(budget))?;
        Ok(Self { stream })
    }

    /// Send one request and read exactly one response, bounded by
    /// `min(CONTROL_REQUEST_TIMEOUT, deadline.remaining())`.
    ///
    /// A control disconnection is reported as an error and never retried here: a
    /// dropped `Confirm` must not be silently re-sent, because re-sending a
    /// confirmation is how a user's single approval turns into two switches.
    pub fn request(
        &mut self,
        request: &ControlRequest,
        deadline: &Deadline,
    ) -> io::Result<ControlResponse> {
        let budget = deadline.clamp(CONTROL_REQUEST_TIMEOUT);
        if budget.is_zero() {
            return Err(expired());
        }
        self.stream.set_write_timeout(Some(budget))?;
        self.stream.set_read_timeout(Some(budget))?;
        codec::write_frame(&mut self.stream, request)?;
        let frame = codec::read_frame(&mut self.stream)?;
        codec::decode(&frame)
    }

    /// Like [`Self::request`] but also hands the caller the raw stream, for the
    /// executor and tool channels that need private file descriptors rather than
    /// control frames.
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

fn expired() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "control channel budget already exhausted",
    )
}

#[cfg(unix)]
fn connect_with_timeout(path: &Path, budget: Duration) -> io::Result<UnixStream> {
    use std::os::fd::AsRawFd;

    let stream = UnixStream::connect(path)?;
    stream.set_nonblocking(true)?;
    // A completed non-blocking connect either succeeded already or reports
    // EINPROGRESS; either way, confirm writability within the budget.
    let raw = stream.as_raw_fd();
    let millis = budget.as_millis().min(i32::MAX as u128) as i32;
    let mut pollfd = libc::pollfd {
        fd: raw,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `pollfd` points at one live entry and `1` is its length.
    let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if ready == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "control channel connect timed out",
        ));
    }
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    // Surface a connect error that poll reported as readiness.
    let mut errno: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the option buffer is `len` bytes and points at a live `c_int`.
    let rc = unsafe {
        libc::getsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut errno as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && errno != 0 {
        return Err(io::Error::from_raw_os_error(errno));
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
