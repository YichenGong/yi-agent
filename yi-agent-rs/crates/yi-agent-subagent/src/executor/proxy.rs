//! The daemon's view of one client's executor.
//!
//! Three properties this file exists to protect:
//!
//! 1. **A cancel must not wait behind a reader.** The reader thread blocks in
//!    `read_frame` for as long as the worker is quiet. If it held the same lock the
//!    writer needs, every Cancel would queue behind that block -- the daemon would
//!    only be able to stop a worker once the worker had something to say, which is
//!    exactly backwards. Read and write therefore use separate halves of the
//!    socket and separate locks.
//! 2. **A gate must not starve a cancel.** Gate acquires are answered by the
//!    reader thread against the daemon's real gate; they never enter the request
//!    path, so a full gate cannot delay a Cancel or a Stop.
//! 3. **The daemon owns the permit.** The executor holds a *proxy* permit. If the
//!    executor dies, the daemon's real permit is released by dropping it -- the
//!    executor can neither leak a permit nor mint one.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_core::agent::{ProviderTurnGate, ProviderTurnLease};

use super::protocol::{
    DaemonFrame, ExecutorEvent, ExecutorFrame, ExecutorRequest, GateMessage, WorkerStartWire,
    read_frame, write_frame,
};

/// Budget for one request/response exchange on the executor channel.
///
/// Deliberately short: this channel carries control, and a slow exchange means the
/// executor is wedged, which the owner-host monitor must see rather than wait out.
pub const EXECUTOR_REQUEST_BUDGET: Duration = Duration::from_secs(2);

/// One connected executor.
///
/// The two locks are separate on purpose; see the module comment. `writer` is held
/// only for the duration of one frame, never across a read.
pub struct ExecutorProxy {
    writer: Mutex<UnixStream>,
    /// The read half, under its own lock.
    ///
    /// Separate from `writer` because `read_frame` blocks for as long as the
    /// executor is quiet. Sharing one lock would mean a Cancel could not be
    /// written until the worker said something -- precisely the deadlock that
    /// makes a hung worker unstoppable.
    reader: Mutex<BufReader<UnixStream>>,
    /// Real permits this daemon currently holds, by proxy id.
    ///
    /// Dropping the map entry releases the real gate permit, which is what makes
    /// an executor crash free its global slot instead of wedging admission.
    permits: Mutex<HashMap<String, Box<dyn ProviderTurnLease>>>,
    /// Starts already sent, keyed by `(command_id, generation, owner, task)`.
    sent_starts: Mutex<HashMap<String, ()>>,
}

impl ExecutorProxy {
    /// Wraps an inherited descriptor.
    ///
    /// The descriptor is private by construction: the daemon received it from the
    /// spawn it performed. Nothing here connects to a public path.
    pub fn from_fd(control: OwnedFd) -> io::Result<Self> {
        use std::os::fd::IntoRawFd;
        let stream = unsafe { UnixStream::from_raw_fd(control.into_raw_fd()) };
        let writer = stream.try_clone()?;
        writer.set_write_timeout(Some(EXECUTOR_REQUEST_BUDGET))?;
        // No read timeout on the shared descriptor: the reader must be able to
        // wait indefinitely for a quiet worker. Shutdown, not a timeout, ends it.
        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(BufReader::new(stream)),
            permits: Mutex::new(HashMap::new()),
            sent_starts: Mutex::new(HashMap::new()),
        })
    }

    /// Sends one request.
    pub fn send(&self, request: &ExecutorRequest) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(
            &mut *writer,
            &DaemonFrame::Request(Box::new(request.clone())),
        )
    }

    /// Sends a `Start`, refusing to send the same identity twice.
    ///
    /// A replay would start the same task attempt a second time. The executor also
    /// remembers applied command ids, but the daemon must not rely on the other
    /// side's memory: a fresh executor has none.
    pub fn send_start(
        &self,
        command_id: &str,
        generation: &str,
        owner: &str,
        payload: WorkerStartWire,
    ) -> io::Result<()> {
        payload.ensure_frameable()?;
        let key = format!("{command_id}/{generation}/{owner}/{}", payload.task_id);
        {
            let mut sent = self
                .sent_starts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sent.contains_key(&key) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "this Start was already sent; a reconnect must not replay it",
                ));
            }
            sent.insert(key, ());
        }
        self.send(&ExecutorRequest::Start {
            command_id: command_id.to_string(),
            generation: generation.to_string(),
            owner: owner.to_string(),
            payload: Box::new(payload),
        })
    }

    /// Reads one frame from the executor.
    ///
    /// Blocks while the worker is quiet, holding only the read lock. A concurrent
    /// [`Self::send`] takes the write lock and proceeds, which is what lets a
    /// cancel overtake a blocked read.
    pub fn receive(&self) -> io::Result<ExecutorFrame> {
        let mut reader = self
            .reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        read_frame(&mut *reader)
    }

    /// Answers one gate request using the daemon's real gate.
    ///
    /// The permit stays here. `permit_id` is only a handle the executor may quote
    /// back; it authorises nothing by itself.
    pub fn answer_gate(&self, request: GateMessage, gate: &dyn ProviderTurnGate) -> io::Result<()> {
        self.answer_gate_for(request, gate, &|| true)
    }

    /// Answers a gate request, dropping a grant the attempt can no longer use.
    ///
    /// `attempt_live` is asked *after* the permit is acquired, because that is the
    /// only moment the answer matters: an acquire may queue behind a full gate for
    /// minutes, and by then its attempt is often stopped. Holding the permit in
    /// that case would shrink the machine-wide limit for as long as the stale
    /// attempt's proxy lives. So the permit is dropped at once -- releasing the
    /// real slot -- and the caller is told `Denied` rather than left waiting for a
    /// grant that will never come.
    pub fn answer_gate_for(
        &self,
        request: GateMessage,
        gate: &dyn ProviderTurnGate,
        attempt_live: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        match request {
            GateMessage::AcquireTurn {
                request_id,
                task,
                attempt,
                ..
            } => {
                let permit = match futures::executor::block_on(gate.acquire()) {
                    Ok(permit) => permit,
                    // Explicit refusal: an executor waiting for silence cannot tell
                    // a full gate from a dead daemon.
                    Err(reason) => {
                        return self.send_gate(GateMessage::Denied { request_id, reason });
                    }
                };
                if !attempt_live() {
                    // Dropping the permit here is the release.
                    drop(permit);
                    return self.send_gate(GateMessage::Denied {
                        request_id,
                        reason: format!(
                            "attempt {task}/{attempt} ended before its turn was granted"
                        ),
                    });
                }
                let permit_id = format!("{request_id}-permit");
                self.permits
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(permit_id.clone(), permit);
                self.send_gate(GateMessage::TurnGranted {
                    request_id,
                    permit_id,
                })
            }
            GateMessage::ReleaseTurn { permit_id } => {
                // Dropping the boxed permit *is* the release.
                self.permits
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&permit_id);
                Ok(())
            }
            // The daemon never receives a grant it granted.
            GateMessage::TurnGranted { .. } | GateMessage::Denied { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the daemon does not accept gate grants",
            )),
        }
    }

    /// Sends a gate message as the executor would.
    ///
    /// Only for tests that drive the executor side of the sub-protocol directly;
    /// production code on this side only ever answers gate messages.
    pub fn send_gate_for_test(&self, message: GateMessage) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *writer, &ExecutorFrame::Gate(message))
    }

    fn send_gate(&self, message: GateMessage) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *writer, &DaemonFrame::Gate(message))
    }

    /// How many real permits this daemon is still holding for this executor.
    pub fn held_permits(&self) -> usize {
        self.permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Releases every real permit, for a dead or retiring executor.
    pub fn release_all_permits(&self) {
        self.permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Shuts the channel down so a blocked reader wakes up.
    ///
    /// A reader parked in `read_frame` cannot be cancelled from outside; shutting
    /// the socket down is what turns "the executor is gone" into an `Ok(0)` for it.
    /// Used when a client is reclaimed, so the reader thread exits promptly instead
    /// of lingering until the socket is finally dropped.
    pub fn shutdown(&self) {
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writer.shutdown(std::net::Shutdown::Both);
    }

    /// The raw descriptor, for a caller that must poll two channels at once.
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        let reader = self
            .reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reader.get_ref().as_raw_fd()
    }
}

impl Drop for ExecutorProxy {
    fn drop(&mut self) {
        // A dropped channel means the executor is gone as far as this daemon is
        // concerned; its slots must not stay occupied.
        self.release_all_permits();
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// An event the reader thread must act on, beyond merely forwarding it.
#[derive(Clone, Debug, PartialEq)]
pub enum FrameAction {
    /// Forward to the worker handle.
    Event(ExecutorEvent),
    /// Answer with the daemon's real gate.
    Gate(GateMessage),
}

/// Classifies one executor frame.
pub fn classify(frame: ExecutorFrame) -> FrameAction {
    match frame {
        ExecutorFrame::Event(event) => FrameAction::Event(*event),
        ExecutorFrame::Gate(gate) => FrameAction::Gate(gate),
    }
}

/// Flushes a writer, used by tests that need the bytes on the wire immediately.
pub fn flush(writer: &mut UnixStream) -> io::Result<()> {
    writer.flush()
}

/// A real permit held on behalf of a test.
pub type HeldPermit = Arc<dyn ProviderTurnLease>;
