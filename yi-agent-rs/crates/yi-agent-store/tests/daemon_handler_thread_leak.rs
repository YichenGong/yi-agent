//! Regression test: the daemon's accept loop must not pin the stack of every
//! finished connection-handler thread.
//!
//! Each accepted connection used to get its own `thread::spawn`, whose
//! `JoinHandle` was pushed into a `Vec` and never reaped until shutdown. A
//! finished thread's stack mapping stays resident for as long as its
//! `JoinHandle` is alive (dropping the handle releases the stack). The
//! app-server polls the daemon over *fresh* short-lived connections, so every
//! poll leaked one stack for the daemon's whole lifetime -- ~96 KiB/s, i.e.
//! hundreds of megabytes over hours. The field symptom was a `daemon serve`
//! process sitting at 1.6 GB with only a handful of live threads.
//!
//! The leak is invisible to thread counts (the handler threads exit) and only
//! the process footprint grows, so the only honest observable is the process's
//! own memory. This test lives in its own integration binary so it runs as an
//! isolated process: no other test shares (and perturbs) its memory.

use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};

/// This process's resident size in bytes, via `task_info(MACH_TASK_BASIC_INFO)`.
///
/// Retained, finished thread stacks stay *resident* (they were written to
/// before the thread exited), so `resident_size` is exactly the counter that
/// exposes the leak.
#[allow(deprecated)] // `mach_task_self` is the only stable way to name this task.
fn resident_bytes() -> u64 {
    // SAFETY: `info` is a zeroed `MACH_TASK_BASIC_INFO` buffer, and `count`
    // states its length in `natural_t` words, which is what the kernel
    // expects for this flavor.
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
            / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        let result = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        assert_eq!(
            result,
            libc::KERN_SUCCESS,
            "task_info(MACH_TASK_BASIC_INFO) failed with {result}"
        );
        info.resident_size as u64
    }
}

/// One poll, exactly as the app-server does it: a fresh connection per request.
fn status_request(socket: &Path) {
    let response = send_request(socket, IpcRequest::Status).expect("status request must succeed");
    assert!(
        matches!(response, IpcResponse::Status { .. }),
        "expected a status response, got {response:?}"
    );
}

#[test]
fn many_short_lived_connections_do_not_leak_a_stack_each() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    // Drive enough connections that the leak (one ~16 KiB stack each) dwarfs any
    // incidental allocation the requests themselves make.
    let connections = 1_500usize;
    let stack_bytes = 16 * 1024u64;

    // Warm up first so one-time daemon setup (repository page cache, runtime
    // buffers) is not mistaken for growth.
    for _ in 0..100 {
        status_request(daemon.socket_path());
    }
    std::thread::sleep(Duration::from_millis(200));
    let before = resident_bytes();

    for _ in 0..connections {
        status_request(daemon.socket_path());
    }
    // Give finished threads a moment to actually terminate before we measure.
    std::thread::sleep(Duration::from_millis(500));
    let after = resident_bytes();

    let growth = after.saturating_sub(before);
    // Half of what a full leak would cost: comfortably above the noise of a
    // healthy daemon and far below one stack per connection.
    let leak_floor = (connections as u64) * stack_bytes / 2;
    assert!(
        growth < leak_floor,
        "resident memory grew by {growth} bytes across {connections} connections \
         (allowed < {leak_floor}); finished handler-thread stacks are being pinned, \
         which is the thread-per-connection stack leak"
    );
}
