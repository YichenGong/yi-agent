//! Passing file descriptors over a Unix socket.
//!
//! The managed-process channel needs this because a child's three standard
//! descriptors are not data: a `/dev/null` path in a frame would be re-opened by
//! the guardian, and a pipe has no path at all. `SCM_RIGHTS` is the only way to
//! hand the *same* open file description across, which is what keeps stdout and
//! stderr as two distinct streams rather than one merged one.
//!
//! Kept here rather than behind a dependency for one reason: the crate is linked
//! by the desktop shell, so its dependency list is a promise. This is ~60 lines of
//! `libc` in one place.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// The largest number of descriptors one message may carry.
///
/// Three is what the protocol needs; the bound exists so a malformed message
/// cannot make us allocate an unbounded control buffer.
pub const MAX_FDS_PER_MESSAGE: usize = 8;

/// Sends `fds` as ancillary data attached to `bytes`.
///
/// The receiving end must call [`recv_with_fds`] for the same message; ancillary
/// data is consumed once, and reading the bytes with a plain `recv` would silently
/// discard the descriptors.
pub fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[OwnedFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS_PER_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("at most {MAX_FDS_PER_MESSAGE} descriptors may travel in one message"),
        ));
    }
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // Space for the cmsg header plus the descriptor array. `cmsghdr` is padded to
    // its own alignment, so the buffer is sized from the C macros rather than
    // guessed.
    let space = unsafe { libc::CMSG_SPACE((fds.len() * std::mem::size_of::<RawFd>()) as u32) };
    let mut control = vec![0u8; space as usize];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if fds.is_empty() {
        // No ancillary data at all: a zero-length cmsg would be malformed.
        return write_all_with(&mut msg, stream);
    }
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for a control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * std::mem::size_of::<RawFd>()) as u32) as _;
        let data = libc::CMSG_DATA(cmsg) as *mut RawFd;
        for (index, fd) in fds.iter().enumerate() {
            *data.add(index) = fd.as_raw_fd();
        }
    }
    write_all_with(&mut msg, stream)
}

fn write_all_with(msg: &mut libc::msghdr, stream: &UnixStream) -> io::Result<()> {
    // A short write would split the frame from its descriptors, so the whole
    // message must go at once. The control frames here are tiny; a send that
    // cannot take it all means the peer is gone.
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receives one message, returning its bytes and any attached descriptors.
///
/// The descriptors are wrapped in `OwnedFd`, so a caller that ignores them still
/// closes them instead of leaking the child's pipes.
pub fn recv_with_fds(stream: &UnixStream, buffer: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
        iov_len: buffer.len(),
    };
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS_PER_MESSAGE * std::mem::size_of::<RawFd>()) as u32) };
    let mut control = vec![0u8; space as usize];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                let header = libc::CMSG_LEN(0) as usize;
                let payload = (*cmsg).cmsg_len as usize;
                let count = payload.saturating_sub(header) / std::mem::size_of::<RawFd>();
                for index in 0..count.min(MAX_FDS_PER_MESSAGE) {
                    // `FromRawFd` takes ownership: from here the descriptor closes
                    // with the returned value even if the caller drops it unused.
                    fds.push(OwnedFd::from_raw_fd(*data.add(index)));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        // Silently proceeding would leak descriptors the kernel had to discard.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ancillary data was truncated; the descriptor set is incomplete",
        ));
    }
    Ok((received as usize, fds))
}
