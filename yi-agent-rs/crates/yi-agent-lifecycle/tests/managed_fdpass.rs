//! Real descriptor passing over a real `socketpair`.
//!
//! Nothing here is mocked: the point of the primitive is that the kernel carries
//! the same open file description across, so a test with a fake would prove only
//! that the fake agreed with itself.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use yi_agent_lifecycle::managed::fdpass::{MAX_FDS_PER_MESSAGE, recv_with_fds, send_with_fds};
use yi_agent_lifecycle::types::ManagedSpawnReceipt;

/// Waits up to five seconds for `fd` to become readable.
///
/// `File` has no read timeout, and a descriptor-discipline mistake in this test
/// must fail rather than hang the suite: an unbounded `read_to_end` on a pipe
/// whose writer is still open never returns.
fn wait_readable(fd: &impl AsRawFd) {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, 5_000) };
    assert!(
        ready > 0,
        "descriptor never became readable within five seconds; a write end is still open"
    );
}

fn pair() -> (UnixStream, UnixStream) {
    UnixStream::pair().expect("socketpair")
}

/// A pipe whose two ends are separately addressable, so a test can prove the
/// write end went to the child and the read end stayed with the parent.
fn pipe_pair() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    let mut ends = [0 as std::os::fd::RawFd; 2];
    let rc = unsafe { libc::pipe(ends.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
    unsafe {
        use std::os::fd::FromRawFd;
        (
            std::os::fd::OwnedFd::from_raw_fd(ends[0]),
            std::os::fd::OwnedFd::from_raw_fd(ends[1]),
        )
    }
}

#[test]
fn three_standard_descriptors_arrive_as_three_distinguishable_streams() {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    fn file_of(fd: OwnedFd) -> std::fs::File {
        unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) }
    }

    let (sender, receiver) = pair();
    let (stdin_read, stdin_write) = pipe_pair();
    let (stdout_read, stdout_write) = pipe_pair();
    let (stderr_read, stderr_write) = pipe_pair();

    // Hand over copies, keeping our own ends so the assertions can read them.
    send_with_fds(
        &sender,
        b"spawn",
        &[
            stdin_read.try_clone().expect("clone stdin"),
            stdout_write.try_clone().expect("clone stdout"),
            stderr_write.try_clone().expect("clone stderr"),
        ],
    )
    .expect("send three descriptors");

    let mut buffer = [0u8; 64];
    let (bytes, fds) = recv_with_fds(&receiver, &mut buffer).expect("receive");
    assert_eq!(&buffer[..bytes], b"spawn");
    assert_eq!(fds.len(), 3, "exactly three descriptors must travel");

    let mut transferred = fds.into_iter();
    let child_stdin = file_of(transferred.next().expect("stdin"));
    let mut child_stdout = file_of(transferred.next().expect("stdout"));
    let mut child_stderr = file_of(transferred.next().expect("stderr"));

    // One distinct marker per stream. A merged stdout/stderr would have to deliver
    // both markers on one read end, which the assertions below would catch.
    child_stdout.write_all(b"out-only").expect("write stdout");
    child_stderr.write_all(b"err-only").expect("write stderr");
    // Both the transferred copy *and* the parent's original must be closed before
    // a read can see EOF. Closing only one is the mistake that hangs this test.
    drop(child_stdout);
    drop(child_stderr);
    drop(stdout_write);
    drop(stderr_write);

    let out = {
        let mut file = file_of(stdout_read);
        wait_readable(&file);
        let mut out = Vec::new();
        file.read_to_end(&mut out).expect("read stdout");
        out
    };
    let err = {
        let mut file = file_of(stderr_read);
        wait_readable(&file);
        let mut err = Vec::new();
        file.read_to_end(&mut err).expect("read stderr");
        err
    };
    assert_eq!(out, b"out-only", "stdout must stay on its own stream");
    assert_eq!(err, b"err-only", "stderr must not merge into stdout");

    // The transferred stdin read end is a working read side: write through the
    // parent's write end and read it back through the transferred descriptor.
    {
        use std::io::Write as _;
        let mut write = file_of(stdin_write);
        write.write_all(b"to-child").expect("write stdin");
    }
    let mut seen = String::new();
    let mut read = child_stdin;
    wait_readable(&read);
    read.read_to_string(&mut seen).expect("read stdin");
    assert_eq!(seen, "to-child");
}

#[test]
fn more_descriptors_than_the_protocol_allows_are_refused_before_sending() {
    let (sender, _receiver) = pair();
    let mut fds = Vec::new();
    for _ in 0..=MAX_FDS_PER_MESSAGE {
        fds.push(pipe_pair().0);
    }
    let error = send_with_fds(&sender, b"x", &fds).expect_err("over-long descriptor set");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error.to_string().contains("at most"),
        "the refusal must name a bound: {error}"
    );
}

#[test]
fn a_spawn_receipt_round_trips_through_the_wire_codec() {
    let receipt = ManagedSpawnReceipt {
        process_id: "proc-7".to_string(),
        pid: 4242,
        pgid: 4242,
    };
    let frame = yi_agent_lifecycle::codec::encode(&receipt).expect("encode");
    let decoded: ManagedSpawnReceipt = yi_agent_lifecycle::codec::decode(&frame).expect("decode");
    assert_eq!(decoded, receipt);
}

#[test]
fn a_silent_peer_times_out_instead_of_wedging_the_serve_loop() {
    let (_sender, receiver) = pair();
    receiver
        .set_read_timeout(Some(Duration::from_millis(80)))
        .expect("set timeout");
    // Nothing is sent. A guardian serving several channels must get control back
    // rather than block forever on one quiet peer holding the lock.
    let mut buffer = [0u8; 8];
    let error = recv_with_fds(&receiver, &mut buffer)
        .map(|_| ())
        .expect_err("an idle peer must not satisfy a receive");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
}
