//! The guardian's side of a private managed-process channel.
//!
//! One channel belongs to exactly one owner. The owner is decided when the
//! channel is created -- the guardian forks the peer and knows which frontend it
//! forked for -- and is *never* read from the request. That asymmetry is the whole
//! security property: a tool process cannot ask to spawn as somebody else, cannot
//! register itself, and cannot renew anyone's lease. It can only ask the guardian
//! to start a group *for the owner this connection was issued to*.
//!
//! The channel is `SCM_RIGHTS`-only for stdio: three descriptors arrive with the
//! spawn frame and are installed as fd 0/1/2. That is what keeps stdout and stderr
//! two streams and lets a caller feed stdin from a pipe.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use yi_agent_lifecycle::MAX_CONTROL_FRAME;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::managed::fdpass;
use yi_agent_lifecycle::types::{
    ManagedSpawnReceipt, ManagedSpawnRequest, ManagedSpawnResponse, RootId,
};

use super::guardian::{ManagedCommand, ProcessOwner, ProcessRegistry, TaskExecutionKey};

/// Who a channel may act for. Fixed at creation, not taken from requests.
#[derive(Clone, Debug)]
pub struct ChannelGrant {
    pub owner: ProcessOwner,
    pub root: Option<RootId>,
    pub execution: Option<TaskExecutionKey>,
}

impl ChannelGrant {
    pub fn new(owner: ProcessOwner) -> Self {
        Self {
            owner,
            root: None,
            execution: None,
        }
    }

    pub fn with_root(mut self, root: Option<RootId>) -> Self {
        self.root = root;
        self
    }

    pub fn with_execution(mut self, execution: Option<TaskExecutionKey>) -> Self {
        self.execution = execution;
        self
    }
}

/// Serves one channel until its peer disconnects.
///
/// Requests are answered in order and one at a time: these are short control
/// exchanges, and a second request arriving while the first still waits would need
/// per-request correlation the protocol does not carry. Sequential handling is
/// also what keeps a `Wait` from blocking a `Terminate` that should have cancelled
/// it -- the caller writes them on separate channels when it needs that.
pub fn serve_channel(
    registry: &mut ProcessRegistry,
    grant: &ChannelGrant,
    mut stream: UnixStream,
    budget: Duration,
) -> io::Result<()> {
    stream.set_read_timeout(Some(budget))?;
    stream.set_write_timeout(Some(budget))?;
    loop {
        let frame = match read_frame_with_fds(&stream) {
            Ok(Some((frame, fds))) => (frame, fds),
            // Clean EOF: the owner went away. Its groups stay registered; the
            // guardian's owner reclamation is what stops them, not this loop.
            Ok(None) => return Ok(()),
            Err(error) => return Err(error),
        };
        let (frame, fds) = frame;
        let request: ManagedSpawnRequest = match yi_agent_lifecycle::codec::decode(&frame) {
            Ok(request) => request,
            Err(_) => {
                // A malformed request is not a reason to take the guardian down;
                // the peer may be a stale build. Close the channel.
                return Ok(());
            }
        };
        let response = match request {
            ManagedSpawnRequest::Spawn(spec) => {
                let stdio = match take_stdio(fds) {
                    Ok(stdio) => stdio,
                    Err(reason) => {
                        write_response(&mut stream, ManagedSpawnResponse::Failed { reason })?;
                        continue;
                    }
                };
                let command = ManagedCommand {
                    executable: spec.executable,
                    args: spec.args,
                    env: spec.env,
                    cwd: spec.cwd,
                    // From the connection, never from the request.
                    owner: grant.owner.clone(),
                    root: grant.root.clone(),
                    execution: grant.execution.clone(),
                    // A tool spawn is authorised by the channel that carried it, not
                    // by an inherited bootstrap: giving a tool process one would let
                    // it claim to be a managed role itself. The bridge is refused for
                    // the same reason -- installing authority is the guardian's job,
                    // and a tool that could send install frames would be minting it.
                    bootstrap: None,
                    bridge: None,
                };
                let deadline = Deadline::after(budget);
                match registry.spawn_with_stdio(command, Some(stdio), deadline) {
                    Ok(process_id) => {
                        let record = registry
                            .records()
                            .iter()
                            .find(|record| record.process_id == process_id)
                            .expect("spawn just recorded this process");
                        ManagedSpawnResponse::Spawned(ManagedSpawnReceipt {
                            process_id,
                            pid: record.pid as u32,
                            pgid: record.pgid,
                        })
                    }
                    Err(error) => ManagedSpawnResponse::Failed {
                        reason: error.to_string(),
                    },
                }
            }
            ManagedSpawnRequest::Wait { process_id } => {
                if let Some(reason) = control_refusal(registry, grant, &process_id) {
                    ManagedSpawnResponse::Failed { reason }
                } else {
                    match registry.wait_exit(&process_id, Deadline::after(budget)) {
                        Ok(exit) => ManagedSpawnResponse::Exit(exit),
                        Err(error) => ManagedSpawnResponse::Failed {
                            reason: error.to_string(),
                        },
                    }
                }
            }
            ManagedSpawnRequest::TryWait { process_id } => {
                if let Some(reason) = control_refusal(registry, grant, &process_id) {
                    ManagedSpawnResponse::Failed { reason }
                } else {
                    match registry.try_exit(&process_id) {
                        Ok(Some(exit)) => ManagedSpawnResponse::Exit(exit),
                        Ok(None) => ManagedSpawnResponse::Running,
                        Err(error) => ManagedSpawnResponse::Failed {
                            reason: error.to_string(),
                        },
                    }
                }
            }
            ManagedSpawnRequest::Terminate { process_id } => {
                if let Some(reason) = control_refusal(registry, grant, &process_id) {
                    ManagedSpawnResponse::Failed { reason }
                } else {
                    let deadline = Deadline::after(budget);
                    // `stop` is private; the owner-scoped termination reuses the
                    // same verification and only touches this grant's owner.
                    match registry.terminate_granted(&grant.owner, &process_id, deadline) {
                        Ok(exit) => ManagedSpawnResponse::Exit(exit),
                        Err(error) => ManagedSpawnResponse::Failed {
                            reason: error.to_string(),
                        },
                    }
                }
            }
        };
        write_response(&mut stream, response)?;
    }
}

/// Refuses to act on a `process_id` this grant does not own.
///
/// Without this a channel issued to one frontend could wait on, or terminate, a
/// group started for another -- which would make the per-owner boundary
/// decorative.
fn control_refusal(
    registry: &ProcessRegistry,
    grant: &ChannelGrant,
    process_id: &str,
) -> Option<String> {
    let record = registry
        .records()
        .iter()
        .find(|record| record.process_id == process_id)?;
    if record.owner == grant.owner {
        None
    } else {
        Some(format!(
            "process {process_id} does not belong to this channel's owner"
        ))
    }
}

/// Splits three descriptors out of an ancillary set, insisting on exactly three.
///
/// Fewer would leave a child with an inherited descriptor for its stdio (a
/// leak into whatever the guardian had open there); more means the peer sent
/// something this protocol never defined.
fn take_stdio(mut fds: Vec<OwnedFd>) -> Result<[OwnedFd; 3], String> {
    if fds.len() != 3 {
        return Err(format!(
            "a spawn must carry exactly three stdio descriptors, got {}",
            fds.len()
        ));
    }
    let stderr = fds.pop().expect("three descriptors");
    let stdout = fds.pop().expect("three descriptors");
    let stdin = fds.pop().expect("three descriptors");
    Ok([stdin, stdout, stderr])
}

fn write_response(stream: &mut UnixStream, response: ManagedSpawnResponse) -> io::Result<()> {
    yi_agent_lifecycle::codec::write_frame(stream, &response)
}

/// Reads one frame, keeping the descriptors that arrived with it.
///
/// `codec::read_frame` would do two separate reads and drop the ancillary data, so
/// this duplicates its shape: one `recvmsg` for the header (which is where
/// `SCM_RIGHTS` rides), then a plain read for whatever body bytes are still
/// outstanding.
fn read_frame_with_fds(stream: &UnixStream) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut buffer = vec![0u8; MAX_CONTROL_FRAME];
    let (mut filled, fds) = match fdpass::recv_with_fds(stream, &mut buffer) {
        Ok((0, _)) => return Ok(None),
        Ok(received) => received,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };
    if filled < 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "managed frame header is truncated",
        ));
    }
    let body_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if body_len + 4 > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed frame exceeds MAX_CONTROL_FRAME",
        ));
    }
    let total = body_len + 4;
    while filled < total {
        use std::io::Read;
        let rest = &mut buffer[filled..total];
        // `Read` is implemented for `&UnixStream`, so the cursor is the reference.
        let mut handle = stream;
        let read = handle.read(rest)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "managed frame body is shorter than its header",
            ));
        }
        filled += read;
    }
    buffer.truncate(total);
    Ok(Some((buffer, fds)))
}
