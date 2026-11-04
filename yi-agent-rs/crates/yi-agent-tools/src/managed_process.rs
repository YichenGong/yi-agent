//! Who owns a spawned process, and which exit policies that ownership allows.
//!
//! The rule the managed runtime makes is narrow and absolute: a process the
//! product started on a frontend's behalf must not outlive that frontend by
//! accident. `on_exit: keep` is exactly that accident -- it asks for a background
//! process to survive the agent, which is what makes "quitting the app stops the
//! app's work" untrue.
//!
//! So the rejection happens at the tool boundary, on the value the caller supplied,
//! and it is an error rather than a silent rewrite: silently turning `keep` into
//! `kill` would leave the caller believing its process survives when it does not,
//! which is the same broken promise in the other direction.

use std::fmt;

use yi_agent_lifecycle::managed::client::ManagedSpawnClient;
use yi_agent_lifecycle::managed::client::StdioFds;
use yi_agent_lifecycle::types::{ManagedSpawnRequest, ManagedSpawnResponse, ManagedSpawnSpec};

/// Whether a spawned process belongs to a frontend, or to a runtime with no
/// lifecycle guarantee to keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedProcessPolicy {
    /// A manual daemon or a test harness: there is no owner to reclaim for, and no
    /// promise that quitting a frontend stops anything.
    Unmanaged,
    /// A managed runtime, where the named client owns everything spawned for it.
    Frontend { client_run_id: String },
}

impl ManagedProcessPolicy {
    /// The owning client, when there is one.
    pub fn client_run_id(&self) -> Option<&str> {
        match self {
            Self::Frontend { client_run_id } => Some(client_run_id),
            Self::Unmanaged => None,
        }
    }

    /// Whether a process started here must be registered with the guardian.
    ///
    /// Registration is what makes a hard stop possible later, so an unmanaged
    /// runtime -- which has no guardian to register with -- skips it.
    pub fn requires_registration(&self) -> bool {
        self.client_run_id().is_some()
    }
}

impl fmt::Display for ManagedProcessPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmanaged => f.write_str("unmanaged"),
            Self::Frontend { client_run_id } => write!(f, "frontend {client_run_id}"),
        }
    }
}

/// The exit policies a caller may ask for, checked against its ownership.
///
/// `keep` is accepted only where nothing was promised: an unmanaged runtime. In a
/// managed runtime it fails, and the message says so plainly instead of hinting at
/// a conversion that did not happen.
pub fn validate_on_exit(policy: &ManagedProcessPolicy, on_exit: &str) -> Result<(), String> {
    let known = matches!(on_exit, "kill" | "keep");
    if !known {
        return Err(format!(
            "unknown on_exit policy {on_exit:?}; expected \"kill\" or \"keep\""
        ));
    }
    if on_exit == "keep" && policy.requires_registration() {
        return Err(format!(
            "on_exit=keep is not available for {policy}: a managed process must stop with \
             the work that started it. Use on_exit=kill, or run it outside the managed runtime."
        ));
    }
    Ok(())
}

/// The `on_exit` values a tool schema may offer.
///
/// Exposed so a schema and the check above cannot drift apart: the schema offers
/// both values because the same tool also serves unmanaged runtimes, and the
/// managed refusal happens at call time.
pub const ON_EXIT_VALUES: [&str; 2] = ["kill", "keep"];

#[cfg(test)]
mod tests {
    use super::*;

    fn frontend() -> ManagedProcessPolicy {
        ManagedProcessPolicy::Frontend {
            client_run_id: "app".into(),
        }
    }

    #[test]
    fn managed_keep_is_rejected_without_silent_conversion() {
        let policy = ManagedProcessPolicy::Frontend {
            client_run_id: "app".into(),
        };
        assert!(validate_on_exit(&policy, "keep").is_err());
        assert!(validate_on_exit(&policy, "kill").is_ok());
        assert!(validate_on_exit(&ManagedProcessPolicy::Unmanaged, "keep").is_ok());
    }

    #[test]
    fn the_refusal_names_the_policy_and_the_reason() {
        let error = validate_on_exit(&frontend(), "keep").unwrap_err();
        // Both halves matter: which client, and why. A bare "invalid" would leave
        // the caller guessing whether it is a typo or a policy.
        assert!(error.contains("app"), "{error}");
        assert!(error.contains("managed"), "{error}");
    }

    #[test]
    fn an_unknown_policy_is_an_error_rather_than_a_default() {
        // Defaulting an unrecognised value to `kill` would silently accept a value
        // the caller did not choose.
        assert!(validate_on_exit(&ManagedProcessPolicy::Unmanaged, "KEEP").is_err());
        assert!(validate_on_exit(&ManagedProcessPolicy::Unmanaged, "").is_err());
    }

    #[test]
    fn only_a_frontend_requires_registration() {
        assert!(frontend().requires_registration());
        assert!(!ManagedProcessPolicy::Unmanaged.requires_registration());
        assert_eq!(frontend().client_run_id(), Some("app"));
        assert_eq!(ManagedProcessPolicy::Unmanaged.client_run_id(), None);
    }

    #[test]
    fn every_schema_value_is_accepted_somewhere() {
        // The schema offers these; a value offered by the schema and rejected for
        // every caller would be a trap.
        for value in ON_EXIT_VALUES {
            assert!(
                validate_on_exit(&ManagedProcessPolicy::Unmanaged, value).is_ok(),
                "{value} is in the schema but unmanaged runtimes may not use it"
            );
        }
    }
}

/// A child the managed runtime started through the guardian.
///
/// The shape deliberately mirrors `tokio::process::Child` at the points the tools
/// use: three pipes, an exit status, and a group-level kill. It is *not* that type,
/// because the process is not a child of this process -- it belongs to the
/// guardian, which is what lets it be reclaimed even when this process is dead.
///
/// Holding one of these is holding an `process_id`, not authority: every
/// operation goes back to the guardian, which checks the owner of the channel it
/// arrives on.
pub struct ManagedChild {
    /// Pipes the caller reads and writes.
    ///
    /// `tokio::fs::File`, not `ChildStdout`: these descriptors belong to pipes we
    /// opened around a child that is not ours, so there is no `Child` to take them
    /// from. `File` still implements `AsyncRead`/`AsyncWrite`, which is all a
    /// reader task or an rmcp transport needs.
    pub stdin: Option<tokio::fs::File>,
    pub stdout: Option<tokio::fs::File>,
    pub stderr: Option<tokio::fs::File>,
    /// The guardian-facing half, shareable with a watcher task.
    handle: std::sync::Arc<ManagedHandle>,
}

/// Everything about a managed child except its pipes.
///
/// Separated so an exit watcher can hold it while the caller keeps the pipes: a
/// shell needs one task that waits for the exit while its own select loop keeps
/// draining output, and both must speak to the same single guardian connection.
#[derive(Debug)]
struct ManagedHandle {
    process_id: String,
    /// Requests to the per-child worker that owns the guardian connection.
    ///
    /// One owning worker rather than a shared socket: the guardian serves one
    /// connection sequentially, so two callers sharing a socket would interleave
    /// a request and a response. And because the queue is separate from the
    /// socket, a caller whose `wait` is dropped mid-flight leaves the protocol in
    /// sync -- the worker still finishes writing and reading that one exchange.
    commands: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ChildCommand>>>,
    /// Set once an exit has been observed, so a repeated `wait` answers from the
    /// observed value instead of asking a guardian that may have moved on.
    cached: std::sync::Mutex<Option<ManagedExit>>,
}

/// One exchange with the guardian, carried out entirely on the worker thread.
enum ChildCommand {
    Ask {
        request: ManagedSpawnRequest,
        timeout: std::time::Duration,
        reply: tokio::sync::oneshot::Sender<std::io::Result<ManagedSpawnResponse>>,
    },
}

/// How a managed child ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManagedExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Whether the whole group was observed empty, which is the only fact that
    /// makes "this work is finished" true.
    pub group_stopped: bool,
}

impl ManagedChild {
    /// Wraps the pipes and channel a spawn left behind.
    pub fn new(
        stdin: Option<tokio::fs::File>,
        stdout: Option<tokio::fs::File>,
        stderr: Option<tokio::fs::File>,
        process_id: String,
        channel: std::os::unix::net::UnixStream,
    ) -> Self {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        // A dedicated thread, not the blocking pool: the socket is blocking, the
        // connection lives as long as the child, and a pool thread parked here for
        // minutes would starve everything else the pool is asked to do.
        std::thread::Builder::new()
            .name(format!("managed-{process_id}"))
            .spawn(move || serve_child(channel, receiver))
            .ok();
        Self {
            stdin,
            stdout,
            stderr,
            handle: std::sync::Arc::new(ManagedHandle {
                process_id,
                commands: std::sync::Mutex::new(Some(commands)),
                cached: std::sync::Mutex::new(None),
            }),
        }
    }

    /// The guardian's identifier for this group.
    pub fn process_id(&self) -> &str {
        &self.handle.process_id
    }

    /// A handle the exit watcher can own for as long as the caller needs it.
    pub fn watcher(&self) -> ManagedWatcher {
        ManagedWatcher {
            handle: std::sync::Arc::clone(&self.handle),
        }
    }

    /// Waits for the group to end, bounded by `timeout`.
    pub async fn wait(&self, timeout: std::time::Duration) -> std::io::Result<ManagedExit> {
        self.handle.wait(timeout).await
    }

    /// Asks without waiting. `None` means the group still has live members.
    pub async fn try_wait(&self) -> std::io::Result<Option<ManagedExit>> {
        self.handle.try_wait().await
    }

    /// Stops the whole group and reports how it ended.
    pub async fn terminate_group(
        &self,
        timeout: std::time::Duration,
    ) -> std::io::Result<ManagedExit> {
        self.handle.terminate_group(timeout).await
    }

    /// Gives up the channel without stopping the group.
    pub fn retain_group(&self) {
        self.handle.retain_group();
    }
}

/// The guardian-facing half of a managed child, ownable by a watcher task.
///
/// Same authority as `ManagedChild` -- it is the same connection -- so every rule
/// that applies there applies here: the guardian checks the owner of the channel,
/// and the handle cannot renew or reassign it.
#[derive(Debug)]
pub struct ManagedWatcher {
    handle: std::sync::Arc<ManagedHandle>,
}

impl ManagedWatcher {
    pub fn process_id(&self) -> &str {
        &self.handle.process_id
    }

    /// Waits for the group to end. `None` from the guardian's point of view means
    /// the group is still running, so this never returns early for a live group.
    pub async fn wait(&self, timeout: std::time::Duration) -> std::io::Result<ManagedExit> {
        self.handle.wait(timeout).await
    }

    pub async fn try_wait(&self) -> std::io::Result<Option<ManagedExit>> {
        self.handle.try_wait().await
    }

    pub async fn terminate_group(
        &self,
        timeout: std::time::Duration,
    ) -> std::io::Result<ManagedExit> {
        self.handle.terminate_group(timeout).await
    }
}

impl ManagedHandle {
    /// Waits for the group to end, bounded by `timeout`.
    ///
    /// The guardian performs the wait, so this returns even if the leader has
    /// already been reaped by an earlier `try_wait`.
    async fn wait(&self, timeout: std::time::Duration) -> std::io::Result<ManagedExit> {
        if let Some(exit) = self.observe_cached() {
            return Ok(exit);
        }
        let response = self
            .request(
                ManagedSpawnRequest::Wait {
                    process_id: self.process_id.clone(),
                    budget_secs: timeout.as_secs(),
                },
                timeout,
            )
            .await?;
        let exit = expect_exit(response)?;
        self.remember(exit);
        Ok(exit)
    }

    /// Asks without waiting. `None` means the group still has live members.
    async fn try_wait(&self) -> std::io::Result<Option<ManagedExit>> {
        if let Some(exit) = self.observe_cached() {
            return Ok(Some(exit));
        }
        let response = self
            .request(
                ManagedSpawnRequest::TryWait {
                    process_id: self.process_id.clone(),
                },
                std::time::Duration::from_secs(5),
            )
            .await?;
        match response {
            ManagedSpawnResponse::Running => Ok(None),
            response => {
                let exit = expect_exit(response)?;
                self.remember(exit);
                Ok(Some(exit))
            }
        }
    }

    /// Stops the whole group and reports how it ended.
    ///
    /// Whole group, not `kill(child)` on the leader: a shell that forked a
    /// background job leaves it in this group, and a leader-only kill would report
    /// success while the work continued.
    async fn terminate_group(&self, timeout: std::time::Duration) -> std::io::Result<ManagedExit> {
        let response = self
            .request(
                ManagedSpawnRequest::Terminate {
                    process_id: self.process_id.clone(),
                },
                timeout,
            )
            .await?;
        let exit = expect_exit(response)?;
        self.remember(exit);
        Ok(exit)
    }

    async fn request(
        &self,
        request: ManagedSpawnRequest,
        timeout: std::time::Duration,
    ) -> std::io::Result<ManagedSpawnResponse> {
        // Cloned out inside its own scope so the guard is released before the
        // await below; a `MutexGuard` cannot cross an await point.
        let commands = {
            let guard = self
                .commands
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.as_ref().cloned()
        };
        let commands = commands.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("managed process {} has no channel left", self.process_id),
            )
        })?;
        let (reply, response) = tokio::sync::oneshot::channel();
        commands
            .send(ChildCommand::Ask {
                request,
                timeout,
                reply,
            })
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    format!("managed process {} worker is gone", self.process_id),
                )
            })?;
        response.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!(
                    "managed process {} worker dropped the reply",
                    self.process_id
                ),
            )
        })?
    }
}

/// Owns the guardian connection for one child and answers one request at a time.
fn serve_child(
    stream: std::os::unix::net::UnixStream,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<ChildCommand>,
) {
    while let Some(ChildCommand::Ask {
        request,
        timeout,
        reply,
    }) = commands.blocking_recv()
    {
        // A dropped reply means the caller gave up (a cancelled wait). The
        // exchange still completes, which is what keeps the next request aligned
        // with its own response.
        let _ = reply.send(exchange(&stream, &request, timeout));
    }
}

/// Writes one frame and reads exactly one frame back.
fn exchange(
    stream: &std::os::unix::net::UnixStream,
    request: &ManagedSpawnRequest,
    timeout: std::time::Duration,
) -> std::io::Result<ManagedSpawnResponse> {
    use std::io::Write;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let body = yi_agent_lifecycle::codec::encode(request)?;
    (&*stream).write_all(&body)?;
    let frame = yi_agent_lifecycle::codec::read_frame(&mut &*stream)?;
    yi_agent_lifecycle::codec::decode(&frame)
}

impl ManagedHandle {
    /// Records an exit that proved the whole group empty.
    fn remember(&self, exit: ManagedExit) {
        if exit.group_stopped {
            let mut cached = self
                .cached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *cached = Some(exit);
        }
    }

    fn observe_cached(&self) -> Option<ManagedExit> {
        *self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Gives up the channel without stopping the group.
    ///
    /// The normal end of a shell command: the leader exited and the caller wants a
    /// `nohup ... &` background job to keep running, exactly as a locally spawned
    /// shell would. Closing the connection does not unregister the group, so the
    /// guardian still reclaims it when the owning frontend goes away.
    pub fn retain_group(&self) {
        let mut commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        commands.take();
    }
}

impl Drop for ManagedHandle {
    fn drop(&mut self) {
        // Best effort only: the guardian's owner reclamation is what actually
        // guarantees the group stops, and pretending otherwise here would make
        // `drop` look synchronous when it cannot be. Cuing a terminate and then
        // dropping the queue lets the worker finish it and exit.
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(commands) = commands {
            let (reply, _response) = tokio::sync::oneshot::channel();
            let _ = commands.send(ChildCommand::Ask {
                request: ManagedSpawnRequest::Terminate {
                    process_id: self.process_id.clone(),
                },
                timeout: std::time::Duration::from_secs(5),
                reply,
            });
        }
    }
}

fn expect_exit(response: ManagedSpawnResponse) -> std::io::Result<ManagedExit> {
    match response {
        ManagedSpawnResponse::Exit(exit) => Ok(ManagedExit {
            code: exit.code,
            signal: exit.signal,
            group_stopped: exit.group_stopped,
        }),
        ManagedSpawnResponse::Failed { reason } => Err(std::io::Error::other(reason)),
        other => Err(std::io::Error::other(format!(
            "expected an exit answer, got {other:?}"
        ))),
    }
}

/// Turns a program name into an absolute path the guardian can exec.
///
/// A path that already contains a separator is resolved against `cwd` (the
/// sandbox's choice of directory) rather than the guardian's. A bare name goes
/// through `PATH`, because that is what `Command::new("sh")` would have done.
pub fn resolve_program(program: &str) -> std::io::Result<std::path::PathBuf> {
    let path = std::path::Path::new(program);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if program.contains('/') {
        return Ok(std::env::current_dir()?.join(path));
    }
    let search = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("program {program:?} was not found on PATH"),
    ))
}

/// Opens a fresh private channel to the guardian.
///
/// A factory rather than a socket path: the guardian does not listen on a path a
/// tool could connect to by itself. Every channel is issued to the process that
/// started this one, so the owner is fixed by inheritance rather than declared in
/// a request.
pub type ChannelFactory =
    std::sync::Arc<dyn Fn() -> std::io::Result<std::os::unix::net::UnixStream> + Send + Sync>;

/// Starts managed children on a frontend's behalf.
///
/// Held by `ToolsContext`, so every spawning tool goes through the same owner. The
/// context keeps `None` for an unmanaged runtime, where tools keep their original
/// direct-spawn behaviour.
#[derive(Clone)]
pub struct ManagedSpawner {
    factory: ChannelFactory,
}

impl std::fmt::Debug for ManagedSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSpawner").finish_non_exhaustive()
    }
}

impl ManagedSpawner {
    pub fn new(factory: ChannelFactory) -> Self {
        Self { factory }
    }

    /// Spawns `command` under this channel's owner, with three fresh pipes.
    ///
    /// The child's ends go to the guardian; the parent keeps the other ends as
    /// async handles, so callers stream output exactly as they would from a local
    /// child.
    pub async fn spawn(
        &self,
        executable: &std::path::Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
    ) -> std::io::Result<ManagedChild> {
        let spec = ManagedSpawnSpec {
            executable: executable.to_path_buf(),
            args: args.to_vec(),
            env: env.to_vec(),
            cwd: cwd.to_path_buf(),
        };
        let stream = (self.factory)()?;

        // Three pipes, one per stream. `pipe` rather than a single socketpair: the
        // child must see three distinct streams, and a merged pair would put
        // stderr on stdout.
        let (stdin_read, stdin_write) = pipe()?;
        let (stdout_read, stdout_write) = pipe()?;
        let (stderr_read, stderr_write) = pipe()?;
        // Parent-side ends, captured before the child-side ends move into the
        // blocking spawn task.
        use std::os::fd::{FromRawFd, IntoRawFd};
        // A `tokio::fs::File` is the async handle for an arbitrary descriptor. It
        // registers for readiness on the runtime, so a reader task sees the same
        // chunked output a `ChildStdout` would give it.
        let stdin_handle = Some(unsafe {
            tokio::fs::File::from_std(std::fs::File::from_raw_fd(stdin_write.into_raw_fd()))
        });
        let stdout_handle = Some(unsafe {
            tokio::fs::File::from_std(std::fs::File::from_raw_fd(stdout_read.into_raw_fd()))
        });
        let stderr_handle = Some(unsafe {
            tokio::fs::File::from_std(std::fs::File::from_raw_fd(stderr_read.into_raw_fd()))
        });

        // The blocking client lives on its own task: the guardian performs the
        // spawn synchronously, and blocking the async runtime on that would stall
        // every other tool for the duration.
        let (client, receipt) = tokio::task::spawn_blocking(move || {
            use std::os::fd::IntoRawFd;
            let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(stream.into_raw_fd()) };
            let mut client = ManagedSpawnClient::connect(owned)?;
            let receipt = client.spawn(
                spec,
                StdioFds {
                    stdin: stdin_read,
                    stdout: stdout_write,
                    stderr: stderr_write,
                },
            )?;
            Ok::<_, std::io::Error>((client, receipt))
        })
        .await
        .map_err(|error| std::io::Error::other(format!("join: {error}")))??;

        Ok(ManagedChild::new(
            stdin_handle,
            stdout_handle,
            stderr_handle,
            receipt.process_id,
            stream_from_client(client),
        ))
    }
}

/// Recovers the underlying stream so later `Wait`/`Terminate` requests reuse the
/// same channel instead of needing a new connection (there is no path to connect
/// to: the guardian only hands channels to processes it started).
fn stream_from_client(client: ManagedSpawnClient) -> std::os::unix::net::UnixStream {
    use std::os::fd::FromRawFd;
    let raw = client.into_raw();
    unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw) }
}

fn pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    let mut ends = [0 as std::os::fd::RawFd; 2];
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd;
    Ok(unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(ends[0]),
            std::os::fd::OwnedFd::from_raw_fd(ends[1]),
        )
    })
}

/// Turns any pipe into the boxed async reader the stream pumps consume.
///
/// A local child and a guardian-owned child produce different concrete pipe types;
/// the reader task only needs `AsyncRead`, so the box erases the difference instead
/// of duplicating the pump.
pub(crate) fn into_reader<R>(reader: R) -> Box<dyn tokio::io::AsyncRead + Send + Unpin>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    Box::new(reader)
}
