//! The daemon's private readiness proof.
//!
//! A listening business socket says only "something bound this path". It cannot say
//! *which* process bound it, whether that process is the one the guardian just
//! started, or whether it is an old daemon that happened to be there first. Trusting
//! it is how a guardian reports ready while serving somebody else's process -- and
//! the user sees a runtime that accepts work and then loses it.
//!
//! So readiness travels on the one channel that is private to this launch: the
//! lifeline pair the guardian created before the fork. The daemon writes one
//! [`DaemonReady`] frame over it after it has actually taken its lock and bound, and
//! the guardian commits its own state only after the frame matches what it expected.
//!
//! Bounded in both directions. A reader that trusted `read_exact` would be bounded
//! only per syscall, and a peer trickling one byte under the timeout could hold the
//! guardian forever; every read and write here asks for the *remaining* budget and
//! the whole frame has a hard ceiling.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{BuildId, DaemonInstanceId};

/// The largest legal Ready frame, 4-byte length header included.
///
/// Its own ceiling rather than the shared control frame's: this frame carries six
/// short strings, and a bound derived from another protocol's limit would silently
/// change whenever that one did.
pub const MAX_READY_FRAME_BYTES: usize = 16 * 1024;

const LENGTH_HEADER_BYTES: usize = 4;

/// What a daemon proves about itself, over the channel only its launcher holds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonReady {
    /// The per-launch secret. This is what makes an old daemon unable to satisfy a
    /// new guardian: it never saw this nonce, so it cannot produce this frame.
    pub startup_nonce: String,
    pub scope_key: String,
    pub daemon_instance_id: DaemonInstanceId,
    pub build_id: BuildId,
    pub business_protocol: u32,
}

fn invalid(kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message.to_string())
}

/// Whether the descriptor is currently blocking.
///
/// Read from the descriptor itself rather than assumed: a caller may have made it
/// non-blocking for its own reasons, and a handshake that silently turned it back
/// into a blocking one would change somebody else's behaviour.
fn is_blocking(stream: &UnixStream) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & libc::O_NONBLOCK == 0)
}

fn remaining_millis(deadline: &Deadline) -> io::Result<libc::c_int> {
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(invalid(
            io::ErrorKind::TimedOut,
            "the daemon readiness handshake exceeded its budget",
        ));
    }
    Ok(remaining.as_millis().min(i32::MAX as u128) as libc::c_int)
}

fn poll_for(stream: &UnixStream, events: libc::c_short, deadline: &Deadline) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let millis = remaining_millis(deadline)?;
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
    if ready > 0 {
        return Ok(());
    }
    if ready == 0 {
        return Err(invalid(
            io::ErrorKind::TimedOut,
            "the daemon readiness handshake exceeded its budget",
        ));
    }
    Err(io::Error::last_os_error())
}

/// Reads exactly one Ready frame, or fails inside the budget.
pub fn read_ready(stream: &mut UnixStream, deadline: &Deadline) -> io::Result<DaemonReady> {
    read_ready_frame(stream, deadline, MAX_READY_FRAME_BYTES)
}

fn read_ready_frame(
    stream: &mut UnixStream,
    deadline: &Deadline,
    ceiling: usize,
) -> io::Result<DaemonReady> {
    // Both are read *before* anything switches the socket to non-blocking, because
    // `read_timeout` itself is rejected with `EINVAL` on a non-blocking socket.
    // Both are read *before* anything switches the socket to non-blocking, because
    // `read_timeout` itself is rejected with `EINVAL` on a non-blocking socket.
    let previous_timeout = stream.read_timeout()?;
    let was_nonblocking = !is_blocking(stream)?;
    let result = read_ready_inner(stream, deadline, ceiling);
    restore_read_mode(stream, was_nonblocking, previous_timeout);
    result
}

/// Puts the descriptor's read mode back without letting the restore hide the result.
///
/// Attempted on both paths because this descriptor is shared with the rest of the
/// lifeline's traffic and a leftover timeout would fire mid-frame later. It must not
/// *replace* the outcome, though: macOS answers `EINVAL` to `set_read_timeout` once
/// the peer has closed, so a failed read would surface as "Invalid argument" and the
/// real reason -- a closed lifeline -- would be lost.
fn restore_read_mode(
    stream: &UnixStream,
    was_nonblocking: bool,
    previous_timeout: Option<Duration>,
) {
    let _ = stream.set_nonblocking(was_nonblocking);
    if let Err(error) = stream.set_read_timeout(previous_timeout) {
        tracing::debug!(
            error = %error,
            "could not restore the lifeline read timeout after the readiness handshake"
        );
    }
}

fn read_ready_inner(
    stream: &mut UnixStream,
    deadline: &Deadline,
    ceiling: usize,
) -> io::Result<DaemonReady> {
    stream.set_nonblocking(true)?;
    let mut header = [0u8; LENGTH_HEADER_BYTES];
    read_exact_until(stream, &mut header, deadline)?;
    let body_len = u32::from_be_bytes(header) as usize;
    // Both ends checked before any allocation: a peer that declares 0 or 4 GiB is
    // rejected rather than believed.
    if body_len == 0 || body_len + LENGTH_HEADER_BYTES > ceiling {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "the daemon readiness frame is empty or larger than its ceiling",
        ));
    }
    let mut frame = Vec::with_capacity(LENGTH_HEADER_BYTES + body_len);
    frame.extend_from_slice(&header);
    let mut body = vec![0u8; body_len];
    read_exact_until(stream, &mut body, deadline)?;
    frame.extend_from_slice(&body);
    yi_agent_lifecycle::codec::decode(&frame)
}

fn read_exact_until(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: &Deadline,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(invalid(
                    io::ErrorKind::UnexpectedEof,
                    "the daemon closed the lifeline before it proved readiness",
                ));
            }
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                poll_for(stream, libc::POLLIN, deadline)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Writes the Ready frame, bounded by the same deadline.
pub fn write_ready(
    stream: &mut UnixStream,
    ready: &DaemonReady,
    deadline: &Deadline,
) -> io::Result<()> {
    let frame = yi_agent_lifecycle::codec::encode(ready)?;
    if frame.len() > MAX_READY_FRAME_BYTES {
        return Err(invalid(
            io::ErrorKind::InvalidInput,
            "the daemon readiness frame exceeds its ceiling",
        ));
    }
    let previous_timeout = stream.write_timeout()?;
    let was_nonblocking = !is_blocking(stream)?;
    let result = write_all_until(stream, &frame, deadline);
    // Same rule as the reader: restore, but never let the restore replace the result.
    let _ = stream.set_nonblocking(was_nonblocking);
    if let Err(error) = stream.set_write_timeout(previous_timeout) {
        tracing::debug!(
            error = %error,
            "could not restore the lifeline write timeout after the readiness handshake"
        );
    }
    result
}

fn write_all_until(stream: &mut UnixStream, bytes: &[u8], deadline: &Deadline) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(invalid(
                    io::ErrorKind::WriteZero,
                    "the daemon lifeline closed while readiness was being written",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                poll_for(stream, libc::POLLOUT, deadline)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Checks a received Ready against the launch the guardian actually performed.
///
/// Every field is checked separately so the failure names which agreement broke.
/// A guardian that accepted a frame with one wrong field would be trusting a claim
/// it did not verify.
pub fn validate_ready(actual: &DaemonReady, expected: &DaemonReady) -> io::Result<()> {
    if actual.startup_nonce != expected.startup_nonce {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "the daemon proved a different startup nonce than this launch issued",
        ));
    }
    if actual.scope_key != expected.scope_key {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "the daemon proved a different scope than this launch authorized",
        ));
    }
    if actual.daemon_instance_id != expected.daemon_instance_id {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "the daemon proved a different instance than this launch started",
        ));
    }
    if actual.build_id != expected.build_id {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "the daemon proved a different build than this launch pinned",
        ));
    }
    if actual.business_protocol != expected.business_protocol {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "the daemon proved a different business protocol than this launch expected",
        ));
    }
    Ok(())
}

/// Bounds for callers that want the same budget as the handshake.
pub fn handshake_budget() -> Duration {
    Duration::from_secs(5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> DaemonReady {
        DaemonReady {
            startup_nonce: "0123456789abcdef".repeat(4),
            scope_key: "/scope".to_string(),
            daemon_instance_id: "daemon-1".to_string(),
            build_id: "build-a".to_string(),
            business_protocol: 7,
        }
    }

    #[test]
    fn daemon_ready_accepts_a_matching_frame() {
        assert!(validate_ready(&ready(), &ready()).is_ok());
    }

    #[test]
    fn daemon_ready_rejects_mismatched_identity_and_nonce() {
        let expected = ready();
        for mutate in [
            (|value: &mut DaemonReady| value.startup_nonce.push('0')) as fn(&mut DaemonReady),
            |value: &mut DaemonReady| value.scope_key = "/other".to_string(),
            |value: &mut DaemonReady| value.daemon_instance_id = "daemon-2".to_string(),
            |value: &mut DaemonReady| value.build_id = "build-b".to_string(),
            |value: &mut DaemonReady| value.business_protocol = 8,
        ] {
            let mut wrong = expected.clone();
            mutate(&mut wrong);
            let error = validate_ready(&wrong, &expected).expect_err("must be refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn daemon_ready_partial_frame_has_a_real_deadline() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let deadline = Deadline::after(Duration::from_millis(200));
        let worker = std::thread::spawn(move || {
            // Declare a body, then send only part of it and keep the socket open.
            writer.write_all(&[0, 0, 0, 200]).unwrap();
            writer.write_all(b"{\"daemon_instance_id\":").unwrap();
            writer.flush().unwrap();
            std::thread::sleep(Duration::from_secs(3));
        });
        let started = std::time::Instant::now();
        let error = read_ready(&mut reader, &deadline).expect_err("a partial frame is not ready");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the read must return on its budget, took {:?}",
            started.elapsed()
        );
        drop(worker);
    }

    #[test]
    fn daemon_ready_uses_versioned_control_envelope() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let expected = ready();
        let worker = std::thread::spawn(move || {
            write_ready(
                &mut writer,
                &expected,
                &Deadline::after(Duration::from_secs(2)),
            )
            .unwrap();
        });
        let actual = read_ready(&mut reader, &Deadline::after(Duration::from_secs(2))).unwrap();
        assert_eq!(actual, ready());
        worker.join().unwrap();
    }

    #[test]
    fn daemon_ready_rejects_empty_and_oversized_frames() {
        for header in [[0u8, 0, 0, 0], [0xff, 0xff, 0xff, 0xff]] {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(&header).unwrap();
            let error = read_ready(&mut reader, &Deadline::after(Duration::from_millis(500)))
                .expect_err("an impossible length must be refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn daemon_ready_rejects_a_plain_frame_without_a_version() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let body = serde_json::to_vec(&ready()).unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        writer.write_all(&frame).unwrap();
        let error = read_ready(&mut reader, &Deadline::after(Duration::from_millis(500)))
            .expect_err("a raw payload has no control_version");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn daemon_ready_reports_closed_lifeline() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        drop(writer);
        let error = read_ready(&mut reader, &Deadline::after(Duration::from_millis(500)))
            .expect_err("a closed lifeline proves nothing");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
