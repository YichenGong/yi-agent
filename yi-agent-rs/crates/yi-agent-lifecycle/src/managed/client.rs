//! The caller's side of a private managed-process channel.
//!
//! One synchronous request per call, deliberately. These channels are per-owner
//! and short-lived; a caller that could pipeline requests would need a way to
//! match a late answer to the attempt that asked, and that is how a stale answer
//! ends up applied to new work.
//!
//! The channel is created from an *inherited* descriptor. Nothing here connects to
//! a path: a socket path would let any process in the same session ask the guardian
//! to spawn something.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::deadline::Deadline;
use crate::managed::fdpass;

use crate::types::{ManagedSpawnReceipt, ManagedSpawnRequest, ManagedSpawnResponse};

/// Pipes for a child's standard descriptors, in the order stdin, stdout, stderr.
///
/// Separate pipes, never one merged pair: stderr must stay distinguishable from
/// stdout, or a tool's error output becomes indistinguishable from its data.
pub struct StdioFds {
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
}

impl StdioFds {
    fn as_array(&self) -> [&OwnedFd; 3] {
        [&self.stdin, &self.stdout, &self.stderr]
    }
}

/// A connected private managed-process channel.
pub struct ManagedSpawnClient {
    stream: UnixStream,
}

impl ManagedSpawnClient {
    /// Hands back the raw descriptor, so a caller can keep speaking the protocol
    /// on the same connection with a different layer.
    pub fn into_raw(self) -> std::os::fd::RawFd {
        use std::os::fd::IntoRawFd;
        self.stream.into_raw_fd()
    }

    /// Wraps an inherited descriptor.
    pub fn connect(control: OwnedFd) -> io::Result<Self> {
        // `into_raw_fd` prevents the `OwnedFd` from closing the number on the way
        // in; the stream now owns it.
        let raw = {
            use std::os::fd::IntoRawFd;
            control.into_raw_fd()
        };
        let stream = unsafe { UnixStream::from_raw_fd(raw) };
        Ok(Self { stream })
    }

    /// Starts one process group, handing the guardian its three stdio pipes.
    ///
    /// The receipt is not a claim that the process exited: it only says the group
    /// exists and is registered, which is the precondition for ever stopping it.
    pub fn spawn(
        &mut self,
        spec: ManagedSpawnSpecArg,
        stdio: StdioFds,
    ) -> io::Result<ManagedSpawnReceipt> {
        let request = ManagedSpawnRequest::Spawn(spec);
        // `encode` already produces a complete length-prefixed frame. It must go
        // out as one message together with the descriptors, so the guardian can
        // pair them; sending the header and body separately would leave the
        // `SCM_RIGHTS` attached to whichever half went last.
        let frame = crate::codec::encode(&request)?;
        let fds: Vec<&OwnedFd> = stdio.as_array().to_vec();
        let owned: Vec<OwnedFd> = fds
            .into_iter()
            .map(|fd| {
                // A temporary duplicate: the caller keeps its own descriptors, and
                // the guardian receives copies rather than ownership of the
                // caller's originals.
                let raw = unsafe { libc::dup(fd.as_raw_fd()) };
                if raw < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
                }
            })
            .collect::<io::Result<Vec<_>>>()?;
        fdpass::send_with_fds(&self.stream, &frame, &owned)?;
        match self.read_answer()? {
            ManagedSpawnResponse::Spawned(receipt) => Ok(receipt),
            ManagedSpawnResponse::Failed { reason } => Err(io::Error::other(reason)),
            other => Err(io::Error::other(format!(
                "expected a spawn receipt, got {other:?}"
            ))),
        }
    }

    /// Waits for the group to end, up to the deadline.
    pub fn wait(&mut self, process_id: &str, deadline: &Deadline) -> io::Result<ManagedExitArg> {
        let response = self.request(
            ManagedSpawnRequest::Wait {
                process_id: process_id.to_string(),
            },
            deadline,
        )?;
        expect_exit(response)
    }

    /// Asks whether the group ended, without waiting.
    pub fn try_wait(&mut self, process_id: &str) -> io::Result<Option<ManagedExitArg>> {
        match self.request(
            ManagedSpawnRequest::TryWait {
                process_id: process_id.to_string(),
            },
            &Deadline::after(Duration::from_secs(2)),
        )? {
            ManagedSpawnResponse::Running => Ok(None),
            response => Ok(Some(expect_exit(response)?)),
        }
    }

    /// Asks the guardian to stop the group and waits for the confirmation.
    pub fn terminate(
        &mut self,
        process_id: &str,
        deadline: &Deadline,
    ) -> io::Result<ManagedExitArg> {
        let response = self.request(
            ManagedSpawnRequest::Terminate {
                process_id: process_id.to_string(),
            },
            deadline,
        )?;
        expect_exit(response)
    }

    fn request(
        &mut self,
        request: ManagedSpawnRequest,
        deadline: &Deadline,
    ) -> io::Result<ManagedSpawnResponse> {
        let budget = deadline.clamp(Duration::from_secs(30));
        if budget.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "managed-process budget already exhausted",
            ));
        }
        self.stream.set_read_timeout(Some(budget))?;
        self.stream.set_write_timeout(Some(budget))?;
        // Plain write: these requests carry no descriptors.
        crate::codec::write_frame(&mut self.stream, &request)?;
        self.read_answer()
    }

    fn read_answer(&mut self) -> io::Result<ManagedSpawnResponse> {
        let frame = crate::codec::read_frame(&mut self.stream)?;
        crate::codec::decode(&frame)
    }
}

fn expect_exit(response: ManagedSpawnResponse) -> io::Result<ManagedExitArg> {
    match response {
        ManagedSpawnResponse::Exit(exit) => Ok(exit),
        ManagedSpawnResponse::Failed { reason } => Err(io::Error::other(reason)),
        other => Err(io::Error::other(format!(
            "expected an exit answer, got {other:?}"
        ))),
    }
}

/// Local alias so the call sites read as "a spec", not "the wire type".
pub type ManagedSpawnSpecArg = crate::types::ManagedSpawnSpec;
/// Local alias for the exit description.
pub type ManagedExitArg = crate::types::ManagedExit;
