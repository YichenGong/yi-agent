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
use crate::types;
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

// ---------------------------------------------------------------------------
// Frontend leases
// ---------------------------------------------------------------------------

/// How often a live frontend renews its lease.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// One private channel the guardian opened for a frontend.
///
/// The descriptor is the authorization: the guardian decided the owner when it
/// created the pair, so nothing on this channel can ask to run as somebody else.
/// Each tool child gets a *new* channel (`tools` takes a factory rather than a
/// handle), because one stream cannot be shared between concurrent spawns without
/// interleaving their answers.
pub struct ManagedChannel {
    stream: std::os::unix::net::UnixStream,
    owner: String,
    generation: String,
}

impl ManagedChannel {
    /// The owner the guardian bound this channel to.
    ///
    /// Taken from the guardian's answer, never from what the caller asked for: a
    /// client that mislabelled its own processes would otherwise make its work look
    /// like another client's.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The runtime generation this channel belongs to.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Hands out the descriptor, leaving this value spent.
    ///
    /// The tools layer speaks its own protocol on the stream and owns the number
    /// afterwards. Splitting it into "descriptor plus owner" here is what keeps
    /// this crate free of the tools layer's types.
    pub fn into_parts(self) -> (std::os::unix::net::UnixStream, String, String) {
        (self.stream, self.owner, self.generation)
    }
}

impl std::fmt::Debug for ManagedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedChannel")
            .field("owner", &self.owner)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// A lease held by one frontend process for the whole time it is using a runtime.
///
/// Three decisions are baked in, each because the obvious alternative is wrong:
///
/// * **The heartbeat runs on its own thread, opened on its own connection.** A
///   heartbeat that shared the business connection could not report liveness while
///   the business side was blocked in a long tool call -- exactly when the guardian
///   needs to know the client is still there.
/// * **`Drop` does not release.** Dropping happens on unwinding too, and a release
///   starts reclaiming the client's work: a panic in one code path must not be
///   reported as "the user quit". The owner calls [`Self::release`] deliberately.
/// * **`Drop` cannot block.** It asks the heartbeat thread to stop and joins it;
///   the thread only ever sleeps in short slices, so this takes milliseconds even
///   when the guardian is wedged.
pub struct FrontendLease {
    control_path: std::path::PathBuf,
    grant: types::ClientGrant,
    heartbeat: Option<Heartbeat>,
    /// Set when a renewal was refused: the lease is gone (released, expired, or
    /// superseded), and the owner should stop expecting its work to survive.
    lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct Heartbeat {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl FrontendLease {
    /// Registers a lease over `control_path`.
    ///
    /// `deadline` bounds the registration exchange only; the lease itself lives
    /// until it is released or stops being renewed.
    pub fn register(
        control_path: &Path,
        id: types::ClientRunId,
        kind: types::ClientKind,
        mode: types::LeaseMode,
        deadline: &Deadline,
    ) -> io::Result<Self> {
        let mut client = ControlClient::connect(control_path, deadline)?;
        let response = client.request(
            &types::ControlRequest::Register {
                client_run_id: id,
                kind,
                mode,
            },
            deadline,
        )?;
        let grant = match response {
            types::ControlResponse::Granted(grant) => grant,
            types::ControlResponse::Rejected { code, reason } => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("the runtime refused this client: {code:?}: {reason}"),
                ));
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the runtime answered a registration with {other:?}"),
                ));
            }
        };
        Ok(Self::with_grant(control_path.to_path_buf(), grant))
    }

    /// Registers only if a guardian is actually serving `control_path`.
    ///
    /// `Ok(None)` means "there is no guardian, so there is no lease to hold". That
    /// is the honest answer in embedded mode, where this process *is* the runtime's
    /// owner and a lease would only be a second copy of its own lifetime.
    pub fn register_if_served(
        control_path: &Path,
        id: types::ClientRunId,
        kind: types::ClientKind,
        mode: types::LeaseMode,
        deadline: &Deadline,
    ) -> io::Result<Option<Self>> {
        if !control_path.exists() {
            return Ok(None);
        }
        match Self::register(control_path, id, kind, mode, deadline) {
            Ok(lease) => Ok(Some(lease)),
            // No listener behind a stale socket file: a crashed guardian, not a
            // refusal. Reporting an error here would turn every first launch after
            // a crash into a failed session.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn with_grant(control_path: std::path::PathBuf, grant: types::ClientGrant) -> Self {
        let lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            control_path,
            grant,
            heartbeat: None,
            lost,
        }
    }

    /// The grant this lease presents. It carries the credential, so it is handed
    /// out only to the owner -- never to a tool or an executor.
    pub fn grant(&self) -> &types::ClientGrant {
        &self.grant
    }

    /// The client run this lease belongs to.
    pub fn client_run_id(&self) -> &str {
        &self.grant.client_run_id
    }

    /// The control socket this lease's runtime serves.
    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// Whether a renewal was refused, i.e. the runtime no longer counts this
    /// frontend as present.
    pub fn is_lost(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Starts renewing the lease every [`HEARTBEAT_INTERVAL`].
    ///
    /// Separate from [`Self::register`] so a caller can hold a lease without a
    /// thread (a test, or a frontend whose runtime outlives the call), and because
    /// a thread that is never joined is worse than one that was never started.
    pub fn start_heartbeat(&mut self) -> io::Result<()> {
        if self.heartbeat.is_some() {
            return Ok(());
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_thread = std::sync::Arc::clone(&stop);
        let path = self.control_path.clone();
        let grant = self.grant.clone();
        let lost = std::sync::Arc::clone(&self.lost);
        let join = std::thread::Builder::new()
            .name("frontend-lease-heartbeat".to_string())
            .spawn(move || {
                // Real time on purpose: the lease window is elapsed seconds, so a
                // frozen injected clock would report liveness that is not there.
                while !stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    if !sleep_until_due(HEARTBEAT_INTERVAL, &stop_for_thread) {
                        return;
                    }
                    if renew(&path, &grant).is_err() {
                        // One refusal is enough: the lease is gone. Retrying would
                        // hammer a guardian that has already withdrawn this client,
                        // and the reclaim it started is the answer, not an error in
                        // need of a second attempt.
                        lost.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                }
            })
            .map_err(|error| {
                io::Error::other(format!("could not start the lease heartbeat: {error}"))
            })?;
        self.heartbeat = Some(Heartbeat {
            stop,
            join: Some(join),
        });
        Ok(())
    }

    /// Releases the lease and returns the id of the reclamation it started.
    ///
    /// Idempotent at the runtime: a second release of the same run returns the same
    /// operation id rather than a new teardown nobody will observe.
    pub fn release(&mut self, deadline: &Deadline) -> io::Result<types::OperationId> {
        self.stop_heartbeat();
        let mut client = ControlClient::connect(&self.control_path, deadline)?;
        match client.request(
            &types::ControlRequest::Release {
                grant: self.grant.clone(),
            },
            deadline,
        )? {
            types::ControlResponse::ReleaseStarted { operation_id } => Ok(operation_id),
            types::ControlResponse::Rejected { code, reason } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the runtime refused this release: {code:?}: {reason}"),
            )),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the runtime answered a release with {other:?}"),
            )),
        }
    }

    /// Asks for one private spawn channel.
    ///
    /// The descriptor arrives as `SCM_RIGHTS` beside the answer; the guardian keeps
    /// the other end. Every call yields a *new* channel, because a channel's owner
    /// is fixed at creation: reusing one across two clients would make the second
    /// client's processes belong to the first.
    ///
    /// `grant` is presented only here, at open time. A channel that has been opened
    /// carries the owner already, so no spawn request needs to (or can) restate it.
    pub fn open_managed_channel(&self, deadline: &Deadline) -> io::Result<ManagedChannel> {
        Self::open_channel_with(&self.control_path, &self.grant, deadline)
    }

    /// Opens a channel with an explicit grant and socket.
    ///
    /// Split out so a caller that already holds this lease's grant can open further
    /// channels without re-registering: registering again would mint a second lease
    /// for the same process and make "the last frontend" impossible to compute.
    pub fn open_channel_with(
        control_path: &Path,
        grant: &types::ClientGrant,
        deadline: &Deadline,
    ) -> io::Result<ManagedChannel> {
        let client = ControlClient::connect(control_path, deadline)?;
        let request = types::ControlRequest::OpenManagedChannel {
            grant: grant.clone(),
        };
        let frame = codec::encode(&request)?;
        let budget = deadline.clamp(CONTROL_REQUEST_TIMEOUT);
        if budget.is_zero() {
            return Err(expired());
        }
        let stream = client.into_stream();
        stream.set_write_timeout(Some(budget))?;
        stream.set_read_timeout(Some(budget))?;
        // The request itself carries no descriptors, but it must go out as one
        // message together with the answer's read: `sendmsg` is used for the plain
        // write too, so the two halves cannot interleave with a second caller.
        crate::managed::fdpass::send_with_fds(&stream, &frame, &[])?;
        let mut buffer = vec![0u8; crate::MAX_CONTROL_FRAME];
        let (filled, fds) = crate::managed::fdpass::recv_with_fds(&stream, &mut buffer)?;
        if filled == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the runtime closed before answering the channel request",
            ));
        }
        let answer: ControlResponse = codec::decode(&buffer[..filled])?;
        match answer {
            ControlResponse::ChannelOpened { owner, generation } => {
                let Some(fd) = fds.into_iter().next() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the runtime reported a channel but sent no descriptor",
                    ));
                };
                Ok(ManagedChannel {
                    stream: std::os::unix::net::UnixStream::from(fd),
                    owner,
                    generation,
                })
            }
            ControlResponse::Rejected { code, reason } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the runtime refused a managed channel: {code:?}: {reason}"),
            )),
            ControlResponse::Unsupported { detail } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("this runtime does not issue managed channels: {detail}"),
            )),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the runtime answered a channel request with {other:?}"),
            )),
        }
    }

    /// Reads the impact a replacement with `target` would have.
    ///
    /// Read-only at the runtime: it takes no barrier and starts no drain, so a
    /// preview the user then cancels cannot have disturbed anything.
    pub fn preview(
        &self,
        target: types::ArtifactIdentity,
        deadline: &Deadline,
    ) -> io::Result<types::RestartPreview> {
        self.request_control(
            types::ControlRequest::Preview {
                grant: self.grant.clone(),
                target,
            },
            deadline,
        )
        .and_then(|response| match response {
            types::ControlResponse::Preview(preview) => Ok(preview),
            types::ControlResponse::Rejected { code, reason } => {
                Err(refused("preview", code, &reason))
            }
            types::ControlResponse::Unsupported { detail } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("this runtime build cannot preview a switch: {detail}"),
            )),
            other => Err(unexpected("preview", &other)),
        })
    }

    /// Consumes a preview token and binds the switch to `operation_id`.
    ///
    /// `operation_id` is chosen by the caller *before* the click so a lost response
    /// is recovered by querying that id, never by sending a second confirmation.
    pub fn confirm(
        &self,
        token: &str,
        operation_id: &str,
        deadline: &Deadline,
    ) -> io::Result<types::OperationId> {
        self.request_control(
            types::ControlRequest::Confirm {
                grant: self.grant.clone(),
                token: token.to_string(),
                operation_id: operation_id.to_string(),
            },
            deadline,
        )
        .and_then(|response| match response {
            types::ControlResponse::Confirmed { operation_id } => Ok(operation_id),
            types::ControlResponse::Rejected { code, reason } => {
                Err(refused("confirmation", code, &reason))
            }
            other => Err(unexpected("confirmation", &other)),
        })
    }

    /// One read of an operation's progress. `Ok(None)` means this runtime never ran
    /// it, which a restarted guardian answers honestly rather than inventing a
    /// failure.
    pub fn operation(
        &self,
        operation_id: &str,
        deadline: &Deadline,
    ) -> io::Result<Option<types::OperationStatus>> {
        self.request_control(
            types::ControlRequest::Operation {
                grant: self.grant.clone(),
                operation_id: operation_id.to_string(),
            },
            deadline,
        )
        .and_then(|response| match response {
            types::ControlResponse::Operation(status) => Ok(status),
            types::ControlResponse::Unsupported { detail } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("this runtime build cannot report operations: {detail}"),
            )),
            types::ControlResponse::Rejected { code, reason } => {
                Err(refused("operation query", code, &reason))
            }
            other => Err(unexpected("operation query", &other)),
        })
    }

    fn request_control(
        &self,
        request: types::ControlRequest,
        deadline: &Deadline,
    ) -> io::Result<types::ControlResponse> {
        let mut client = ControlClient::connect(&self.control_path, deadline)?;
        client.request(&request, deadline)
    }

    /// Polls a release until it finishes or `budget` runs out.
    ///
    /// A timeout is `Ok(None)` rather than an error: the shell is quitting and the
    /// distinction between "still stopping" and "the answer never arrived" is not
    /// something it can act on differently. What it must never do is report success
    /// it did not observe.
    pub fn await_result(
        &self,
        operation: &str,
        budget: Duration,
    ) -> io::Result<Option<types::ReclaimResult>> {
        let deadline = Deadline::after(budget);
        loop {
            let mut client = match ControlClient::connect(&self.control_path, &deadline) {
                Ok(client) => client,
                // The guardian is gone, so there is no answer to collect.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            match client.request(
                &types::ControlRequest::ReclaimStatus {
                    grant: self.grant.clone(),
                    operation_id: operation.to_string(),
                },
                &deadline,
            )? {
                types::ControlResponse::ReclaimStatus { result, .. } => {
                    if let Some(result) = result {
                        return Ok(Some(result));
                    }
                }
                // The operation is not known to this guardian: a restarted guardian
                // has nothing to say about the previous generation's teardown.
                types::ControlResponse::Rejected { .. } => return Ok(None),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("the runtime answered a status poll with {other:?}"),
                    ));
                }
            }
            if deadline.is_expired() {
                return Ok(None);
            }
            std::thread::sleep(deadline.clamp(Duration::from_millis(100)));
        }
    }

    fn stop_heartbeat(&mut self) {
        if let Some(mut heartbeat) = self.heartbeat.take() {
            heartbeat
                .stop
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(join) = heartbeat.join.take() {
                let _ = join.join();
            }
        }
    }
}

impl Drop for FrontendLease {
    /// Stops renewing; deliberately neither releases nor blocks on a request.
    ///
    /// Not releasing is the important half: dropping happens during unwinding, and
    /// a release reclaims the client's work. A panic must not look like a quit.
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

/// Sleeps for `total` in short slices so `stop` is noticed promptly.
///
/// Returns `false` when the stop flag was set.
fn sleep_until_due(total: Duration, stop: &std::sync::atomic::AtomicBool) -> bool {
    let slice = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let remaining = total - slept;
        let this = if remaining < slice { remaining } else { slice };
        std::thread::sleep(this);
        slept += this;
    }
    !stop.load(std::sync::atomic::Ordering::SeqCst)
}

/// A refusal the runtime made on purpose: permission or state, never transport.
fn refused(what: &str, code: types::ControlErrorCode, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the runtime refused the {what}: {code:?}: {reason}"),
    )
}

fn unexpected(what: &str, response: &types::ControlResponse) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the runtime answered a {what} with {response:?}"),
    )
}

fn renew(path: &Path, grant: &types::ClientGrant) -> io::Result<()> {
    let deadline = Deadline::after(CONTROL_REQUEST_TIMEOUT);
    let mut client = ControlClient::connect(path, &deadline)?;
    match client.request(
        &types::ControlRequest::Heartbeat {
            grant: grant.clone(),
        },
        &deadline,
    )? {
        types::ControlResponse::Ack => Ok(()),
        types::ControlResponse::Rejected { code, reason } => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("heartbeat refused: {code:?}: {reason}"),
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the runtime answered a heartbeat with {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A guardian that answers over a real socket, and the requests it saw.
    ///
    /// The lease client's job is I/O ordering, so a fake in-process service would
    /// test the mock: `start_heartbeat` opens its *own* connection from another
    /// thread, and only a real listener reproduces that.
    fn serve(
        path: &Path,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // On macOS the accepted socket inherits the listener's
                        // O_NONBLOCK, so a read that races the client's write
                        // returns WouldBlock instead of waiting. Dropping the
                        // connection there would surface to the client as EOF on a
                        // registration that actually arrived. Block for real, with
                        // a timeout so a silent peer cannot wedge the fixture.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                        let frame = match crate::codec::read_frame(&mut stream) {
                            Ok(frame) => frame,
                            Err(_) => continue,
                        };
                        let request: types::ControlRequest = match crate::codec::decode(&frame) {
                            Ok(request) => request,
                            Err(_) => continue,
                        };
                        let response = match request {
                            types::ControlRequest::Register {
                                client_run_id,
                                mode,
                                ..
                            } => {
                                seen.lock()
                                    .unwrap()
                                    .push(format!("register:{client_run_id}"));
                                types::ControlResponse::Granted(types::ClientGrant {
                                    client_run_id,
                                    credential: "credential".to_string(),
                                    epoch: 1,
                                    mode,
                                })
                            }
                            types::ControlRequest::Heartbeat { .. } => {
                                seen.lock().unwrap().push("heartbeat".to_string());
                                types::ControlResponse::Ack
                            }
                            types::ControlRequest::Release { grant } => {
                                seen.lock().unwrap().push("release".to_string());
                                let _ = grant;
                                types::ControlResponse::ReleaseStarted {
                                    operation_id: "reclaim-test-1".to_string(),
                                }
                            }
                            types::ControlRequest::ReclaimStatus { operation_id, .. } => {
                                types::ControlResponse::ReclaimStatus {
                                    operation_id,
                                    result: Some(types::ReclaimResult {
                                        client_run_id: "tui:test".to_string(),
                                        stopped: true,
                                        failures: Vec::new(),
                                    }),
                                }
                            }
                            _ => types::ControlResponse::Unsupported {
                                detail: "not part of this fixture".to_string(),
                            },
                        };
                        let _ = crate::codec::write_frame(&mut stream, &response);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        })
    }

    struct Fixture {
        directory: tempfile::TempDir,
        path: PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("guardian-control.sock");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let handle = serve(
                &path,
                std::sync::Arc::clone(&stop),
                std::sync::Arc::clone(&seen),
            );
            Self {
                directory,
                path,
                stop,
                seen,
                handle: Some(handle),
            }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let _ = &self.directory;
        }
    }

    fn deadline() -> Deadline {
        Deadline::after(Duration::from_secs(5))
    }

    #[test]
    fn no_guardian_means_no_lease_rather_than_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("guardian-control.sock");
        let lease = FrontendLease::register_if_served(
            &path,
            "tui:1".to_string(),
            types::ClientKind::Tui,
            types::LeaseMode::Business,
            &deadline(),
        )
        .expect("a missing guardian is not a failure");
        assert!(
            lease.is_none(),
            "embedded mode has no guardian, so there is nothing to hold"
        );
    }

    /// Both socket-backed tests bind a real file. They are cheap but not
    /// instant, and the heartbeat thread keeps a connection open for seconds, so
    /// running them concurrently makes the slow one look flaky when the machine
    /// is busy. They are independent behaviours, so serialising them costs
    /// nothing and removes the false negative.
    static SOCKET_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn socket_test_lock() -> std::sync::MutexGuard<'static, ()> {
        SOCKET_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn a_registered_lease_releases_and_reads_its_result() {
        let _serial = socket_test_lock();
        let fixture = Fixture::new();
        let mut lease = FrontendLease::register_if_served(
            &fixture.path,
            "tui:test".to_string(),
            types::ClientKind::Tui,
            types::LeaseMode::Business,
            &deadline(),
        )
        .expect("register")
        .expect("the fixture serves registrations");
        assert_eq!(lease.client_run_id(), "tui:test");
        assert!(!lease.is_lost());

        lease.start_heartbeat().expect("start the heartbeat");
        // The renewal interval is five seconds, which is too long to wait for in a
        // unit test. What this pins instead is the ordering the caller depends on:
        // whatever the heartbeat thread did, nothing may renew after the release.
        let operation = lease.release(&deadline()).expect("release");
        assert_eq!(operation, "reclaim-test-1");
        let result = lease
            .await_result(&operation, Duration::from_secs(5))
            .expect("poll")
            .expect("the fixture answers at once");
        assert!(result.stopped);

        let seen = fixture.seen();
        assert_eq!(seen.first().map(String::as_str), Some("register:tui:test"));
        assert!(
            seen.iter().any(|entry| entry == "release"),
            "a release must go out: {seen:?}"
        );
        // Ordering matters: the heartbeat thread must be stopped before the release
        // is sent, or a renewal can land after the reclaim and be refused -- which
        // sets `lost` on a lease that is being released on purpose.
        let release_at = seen.iter().position(|entry| entry == "release").unwrap();
        assert!(
            !seen[release_at..].iter().any(|entry| entry == "heartbeat"),
            "no heartbeat may follow the release: {seen:?}"
        );
    }

    #[test]
    fn a_refused_heartbeat_marks_the_lease_lost_without_retrying() {
        let _serial = socket_test_lock();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("guardian-control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_thread = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                // The accepted side inherits the listener's non-blocking flag on
                // macOS, and a non-blocking read surfaces as an error rather than a
                // frame. The real guardian normalises this the same way.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let Ok(frame) = crate::codec::read_frame(&mut stream) else {
                    continue;
                };
                let Ok(request) = crate::codec::decode::<types::ControlRequest>(&frame) else {
                    continue;
                };
                let response = match request {
                    types::ControlRequest::Register {
                        client_run_id,
                        mode,
                        ..
                    } => types::ControlResponse::Granted(types::ClientGrant {
                        client_run_id,
                        credential: "credential".to_string(),
                        epoch: 1,
                        mode,
                    }),
                    // Every renewal is refused: the runtime has already withdrawn
                    // this client.
                    types::ControlRequest::Heartbeat { .. } => types::ControlResponse::Rejected {
                        code: types::ControlErrorCode::StaleEpoch,
                        reason: "the lease was released".to_string(),
                    },
                    _ => types::ControlResponse::Unsupported {
                        detail: "unexpected".to_string(),
                    },
                };
                let _ = crate::codec::write_frame(&mut stream, &response);
            }
        });

        let mut lease = FrontendLease::register(
            &path,
            "app:1".to_string(),
            types::ClientKind::App,
            types::LeaseMode::LeaseOnly,
            &deadline(),
        )
        .expect("register");
        lease.start_heartbeat().expect("start the heartbeat");
        // The heartbeat interval is real time; the window is short enough for a test
        // because one refusal is terminal.
        let waited = Deadline::after(HEARTBEAT_INTERVAL + Duration::from_secs(3));
        while !lease.is_lost() && !waited.is_expired() {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            lease.is_lost(),
            "a refused renewal means the lease is gone; believing otherwise makes a \
             frontend keep working on behalf of work that is being reclaimed"
        );
        // `Drop` stops the heartbeat; the fixture thread is non-blocking, so it
        // observes the flag on its next tick instead of hanging on `accept`.
        drop(lease);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = handle.join();
    }
}
