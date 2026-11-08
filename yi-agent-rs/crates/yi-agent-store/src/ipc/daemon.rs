use super::*;
use crate::repository::RepositoryError;

pub(crate) const CONFIRMATION_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) struct PendingConfirmation {
    task_id: String,
    recursive: bool,
    scope: CancelScope,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CancelScope {
    task_ids: Vec<String>,
    active_leases: Vec<IpcCancelLease>,
    unmerged_deliveries: Vec<IpcCancelDelivery>,
}

#[derive(Default)]
pub(crate) struct ConfirmationStore {
    pending: Mutex<HashMap<String, PendingConfirmation>>,
}

impl ConfirmationStore {
    fn issue(&self, task_id: String, recursive: bool, scope: CancelScope) -> String {
        let token = Uuid::new_v4().to_string();
        self.pending
            .lock()
            .expect("confirmation store mutex poisoned")
            .insert(
                token.clone(),
                PendingConfirmation {
                    task_id,
                    recursive,
                    scope,
                    expires_at: Instant::now() + CONFIRMATION_TTL,
                },
            );
        token
    }

    fn consume(&self, token: &str, task_id: &str, recursive: bool, scope: &CancelScope) -> bool {
        let Some(pending) = self
            .pending
            .lock()
            .expect("confirmation store mutex poisoned")
            .remove(token)
        else {
            return false;
        };
        pending.expires_at > Instant::now()
            && pending.task_id == task_id
            && pending.recursive == recursive
            && pending.scope == *scope
    }
}

/// Serves the guardian's private authority bridge until it reaches EOF.
///
/// Runs on its own thread rather than inside the accept loop, because the two have
/// opposite cadences: the accept loop must answer clients promptly, while a bridge
/// frame can sit waiting for a guardian that is busy with another client's lease. One
/// loop for both would let a slow bridge stall every business request.
///
/// A frame the daemon does not answer (a `Challenge`, which only the daemon sends)
/// yields nothing to write, and that is not an error. A write failure ends the loop:
/// the peer is gone, and the bridge's EOF path is what closes admission.
fn serve_authority_bridge(
    stream: std::os::unix::net::UnixStream,
    coordinator: std::sync::Arc<crate::runtime::RuntimeCoordinator>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::io::Write;
    let mut stream = stream;
    if let Err(error) = stream.set_read_timeout(Some(std::time::Duration::from_millis(200))) {
        tracing::warn!(error = %error, "authority bridge could not be made pollable");
        return;
    }
    coordinator.bridge_reconnected();
    let mut reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!(error = %error, "authority bridge could not be duplicated");
            coordinator.bridge_closed();
            return;
        }
    };
    loop {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let raw = match yi_agent_lifecycle::codec::read_frame(&mut reader) {
            Ok(raw) => raw,
            // A bounded read that produced nothing is the normal idle state; the bridge
            // is a request/response channel that is silent most of the time.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            // EOF: the guardian is gone. Admission closes and stays closed, but no
            // tombstone is written -- a crashed guardian is not a decision to revoke.
            Err(_) => break,
        };
        let frame: yi_agent_lifecycle::foreground::BridgeFrame =
            match yi_agent_lifecycle::codec::decode(&raw) {
                Ok(frame) => frame,
                Err(error) => {
                    // One malformed frame is that frame's problem, not the runtime's: the
                    // guardian may still be healthy and simply speaking a newer protocol.
                    tracing::warn!(error = %error, "authority bridge received an undecodable frame");
                    continue;
                }
            };
        let Some(answer) = coordinator.serve_bridge_frame(frame) else {
            continue;
        };
        if let Err(error) = yi_agent_lifecycle::codec::write_frame(&mut stream, &answer) {
            tracing::warn!(error = %error, "authority bridge answer could not be written");
            break;
        }
        let _ = stream.flush();
    }
    coordinator.bridge_closed();
}

/// A manually owned, current-user-only local daemon socket.
pub struct Daemon {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    coordinator: Arc<RuntimeCoordinator>,
    listener: Option<JoinHandle<()>>,
    /// The guardian authority bridge thread, when this daemon has one.
    ///
    /// Joined on teardown so a bridge answer cannot be written to a descriptor whose
    /// socket has already been unlinked. `None` means "no guardian", which is the
    /// embedded case and not a degraded managed one.
    bridge_thread: Option<JoinHandle<()>>,
    /// Orphaned tasks the startup sweep reclaimed, reported to the caller
    /// instead of printed. Whoever owns the terminal must decide how this
    /// reaches the user: a bare write corrupts a live TUI frame, because the
    /// embedded daemon shares the process (and therefore the terminal) with the
    /// front end that started it.
    reclaimed_orphans: usize,
    /// Held for the daemon's whole lifetime. Dropping it releases the kernel
    /// lock, so a crashed daemon can never leave a lock behind.
    _instance_lock: InstanceLock,
}

impl Daemon {
    pub fn start(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
    ) -> Result<Self, IpcError> {
        Self::start_with_factory(
            runtime_dir,
            database_path,
            Arc::new(UnavailableWorkerFactory),
        )
    }

    /// Starts the daemon with application-owned worker construction. The
    /// default `start` remains useful for inspection-only clients, but a
    /// runnable daemon supplies its provider/tool factory here.
    pub fn start_with_factory(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, IpcError> {
        Self::start_with_factory_and_authority(
            runtime_dir,
            database_path,
            factory,
            crate::foreground::ForegroundActivation::Disabled,
            None,
            None,
        )
    }

    /// Starts the daemon every shipping host uses: container writes must name an
    /// open turn.
    ///
    /// This is a separate constructor rather than a flipped default because the
    /// two behaviours answer different questions. A test that drives the IPC of a
    /// *provider* runtime -- budgets, delivery, restart hydration -- is not
    /// exercising the fenced contract and would only be rewritten to satisfy a
    /// gate it never claimed. A host that drives a conversation does claim it, so
    /// each of them calls this explicitly and a new host cannot inherit the old
    /// answer by accident.
    pub fn start_with_factory_enforcing_session_turns(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, IpcError> {
        let daemon = Self::start_with_factory(runtime_dir, database_path, factory)?;
        daemon
            .coordinator
            .enforce_session_turns(std::sync::atomic::Ordering::Release);
        Ok(daemon)
    }

    /// The same, with the guardian's authority already installed.
    pub fn start_with_factory_authority_enforcing_session_turns(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
        activation: crate::foreground::ForegroundActivation,
        managed: Option<yi_agent_lifecycle::authorization::AuthorizationLedger>,
        bridge: Option<std::os::fd::OwnedFd>,
    ) -> Result<Self, IpcError> {
        let daemon = Self::start_with_factory_and_authority(
            runtime_dir,
            database_path,
            factory,
            activation,
            managed,
            bridge,
        )?;
        daemon
            .coordinator
            .enforce_session_turns(std::sync::atomic::Ordering::Release);
        Ok(daemon)
    }

    /// Starts a daemon that refuses container writes without an open turn.
    ///
    /// Tests only: production uses [`Self::start_with_factory_enforcing_session_turns`].
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn start_with_factory_enforcing_session_turns_for_test(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, IpcError> {
        let daemon = Self::start_with_factory(runtime_dir, database_path, factory)?;
        daemon.coordinator.enforce_session_turns_for_test();
        Ok(daemon)
    }

    /// The real startup sequence, with the foreground switch made explicit.
    ///
    /// `Disabled` reproduces the historical order exactly; `Enabled` additionally
    /// reconciles the persisted foreground facts *after* the generic sweeps and
    /// *before* the coordinator hydrates, which is the only window where no work is
    /// admitted yet and no in-memory authority exists to disagree with the rows.
    pub fn start_with_factory_and_activation(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
        activation: crate::foreground::ForegroundActivation,
    ) -> Result<Self, IpcError> {
        Self::start_with_factory_and_authority(
            runtime_dir,
            database_path,
            factory,
            activation,
            None,
            None,
        )
    }

    /// Starts the daemon with the guardian's authority already installed.
    ///
    /// Installed *before* the listener binds on purpose: an attach that raced the
    /// handshake would be judged by the unmanaged default and admitted with no
    /// authorization at all. `None` keeps the old behaviour exactly.
    ///
    /// `activation` and `managed` are independent: the first decides whether this
    /// daemon reconciles persisted foreground facts at startup, the second is the
    /// verifier half of the guardian's capability ledger. A managed daemon needs
    /// both; an embedded one passes `Disabled` and `None`.
    pub fn start_with_factory_and_authority(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
        activation: crate::foreground::ForegroundActivation,
        managed: Option<yi_agent_lifecycle::authorization::AuthorizationLedger>,
        // The daemon's end of the guardian's *foreground* authority bridge. A
        // separate descriptor from the lifeline that carried `managed`: the lifeline
        // is the daemon's death monitor and capability feed, while this channel
        // carries the challenge/answer exchange that installs a root. Sharing one
        // descriptor would make a half-written foreground frame look like a dropped
        // lifeline, i.e. a live daemon reported dead.
        bridge: Option<std::os::fd::OwnedFd>,
    ) -> Result<Self, IpcError> {
        raise_file_descriptor_limit();
        let runtime_dir = runtime_dir.as_ref();
        fs::create_dir_all(runtime_dir)?;
        fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700))?;

        let socket_path = socket_path_for(runtime_dir)?;
        let lock_path = runtime_dir.join("runtime.lock");
        let instance_lock = InstanceLock::acquire(runtime_dir, &lock_path)?;
        // Before any recovery, orphan reclamation or `RuntimeCoordinator::open`,
        // which all write. A database written by a newer build must be refused
        // read-only up front: `RuntimeRepository::open` would otherwise migrate
        // (no-op) and start rewriting rows under a schema this build does not
        // understand.
        yi_agent_store_schema_preflight(database_path.as_ref())?;
        // Holding the exclusive lock proves no live daemon serves this directory,
        // so any socket node still on disk was left by a killed process. `bind`
        // fails with `EADDRINUSE` on an existing path, so the stale node must go
        // before we bind our own listener.
        remove_if_exists(&socket_path)?;
        let mut repository = RuntimeRepository::open(database_path.as_ref())?;
        if activation.is_enabled() {
            // Before the generic sweeps, because those are exactly the paths that would
            // otherwise park a managed container or promote a half-prepared root. Both
            // operations are idempotent and neither needs an authorization: they only
            // ever *close* admission or cancel a reservation nothing can resume.
            let cancelled_pending = repository.cancel_pending_foreground_roots()?;
            if !cancelled_pending.is_empty() {
                eprintln!(
                    "yi-agent: cancelled {} abandoned foreground root reservation(s)",
                    cancelled_pending.len()
                );
            }
            // A new generation holds no authorization, so every persisted root starts
            // closed. A `managed` root keeps its identity here and nothing more.
            repository.close_foreground_admission()?;
            // A `ready` operation's confirmation token lived in the dead process; the
            // freeze survives, the token does not, so the host must prepare again.
            let expired = repository.expire_uncommitted_recovery_operations()?;
            if expired > 0 {
                eprintln!(
                    "yi-agent: {expired} foreground recovery operation(s) need a fresh preview"
                );
            }
        }
        repository.recover_inflight_tasks_with_activation(activation)?;
        // The sweep above parks interrupted work in `recovery_required`, which
        // is a terminal state, so anything that cannot actually resume would sit
        // there forever and pin its session's supervisor. Reclaim those, plus
        // tasks whose owning root has detached, before the coordinator hydrates.
        let reclaimed_orphans = repository.reclaim_orphaned_tasks(DEFAULT_ORPHAN_GRACE_SECS)?;
        // Leases held by tasks that already settled are unreleasable garbage:
        // nothing schedules a settled task again, and `worktree:` / `workspace:`
        // rows are `exclusive`, so each one pins its workspace's admission slot
        // for good. Boot is the only safe moment to sweep them — no work is
        // admitted yet, so every such row is provably stale rather than in use.
        let released_settled_leases = repository.release_leases_of_settled_tasks()?;
        if released_settled_leases > 0 {
            eprintln!(
                "yi-agent: released {released_settled_leases} active lease(s) held by settled tasks"
            );
        }
        drop(repository);
        // A managed daemon takes its generation from the guardian's ledger: every
        // authority frame names that value, so a daemon that made up its own would
        // refuse the first install as stale and never serve a managed root.
        let generation = managed
            .as_ref()
            .map(|ledger| ledger.daemon_instance_id().to_string());
        let coordinator = Arc::new(RuntimeCoordinator::open_with_activation_and_generation(
            database_path.as_ref(),
            factory,
            activation,
            generation,
        )?);
        if let Some(ledger) = managed {
            coordinator
                .adopt_managed_authority(ledger)
                .map_err(|error| {
                    IpcError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                })?;
        }
        // A fresh daemon starts with no plugin routes; the supervisor republishes
        // them as soon as it reconciles. Only this project's routes are dropped:
        // another project's daemon in the same process keeps serving.
        clear_plugin_sockets(runtime_dir);
        // The socket now lives under the instance home, in a directory nothing else
        // creates; without it the bind fails with ENOENT, a worse failure than the
        // over-long path this layout replaced because ENOENT does not name its cause.
        //
        // Created here rather than beside `socket_path_for` so that every earlier
        // refusal (lock, schema preflight, repository open) leaves no directory
        // behind: those returns are common in tests and on a mismatched build.
        if let Some(socket_dir) = socket_path.parent() {
            fs::create_dir_all(socket_dir)?;
            fs::set_permissions(socket_dir, fs::Permissions::from_mode(0o700))?;
        }
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                if let Some(socket_dir) = socket_path.parent() {
                    let _ = fs::remove_dir(socket_dir);
                }
                return Err(error.into());
            }
        };
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        // Record the socket node's identity so the exit path can tell our node
        // apart from a replacement bound at the same path after this directory
        // was rebuilt.
        let socket_identity = {
            use std::os::unix::fs::MetadataExt;
            fs::symlink_metadata(&socket_path)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .ok()
        };
        // Stays non-blocking: `accept` runs only after the poll reports
        // readiness, but the peer can disconnect in between, and that must
        // surface as `WouldBlock` rather than blocking the loop.
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        // The bridge thread starts before the socket answers clients. A client that
        // could reach the daemon but not a bounded authority channel would be told
        // "managed" by a runtime that cannot install anything yet.
        let bridge_thread = bridge.map(|fd| {
            use std::os::fd::{FromRawFd, IntoRawFd};
            // SAFETY: the descriptor came from the guardian's inherited handle, which
            // proved it open; this is the only conversion and it takes ownership.
            let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd.into_raw_fd()) };
            let coordinator = Arc::clone(&coordinator);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve_authority_bridge(stream, coordinator, stop))
        });
        let thread_stop = Arc::clone(&stop);
        let daemon_coordinator = Arc::clone(&coordinator);
        let confirmations = Arc::new(ConfirmationStore::default());
        let listener_confirmations = Arc::clone(&confirmations);
        let client_handlers = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
        let listener_handlers = Arc::clone(&client_handlers);
        let database_path = database_path.as_ref().to_path_buf();
        let request_context = Arc::new(RequestContext::new(runtime_dir, &database_path)?);
        let watched_runtime_dir = runtime_dir.to_path_buf();
        let cleanup_socket = socket_path.clone();
        let cleanup_lock = lock_path.clone();
        let cleanup_socket_identity = socket_identity;
        let listener = thread::spawn(move || {
            // Worker facts arrive independently of client traffic. Reconcile
            // them here so a parent blocked in wait_agent is always woken.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon reconciliation runtime must initialize");
            let mut last_schedule_minute = None;
            let mut last_runtime_dir_check: Option<Instant> = None;
            // Live admission wake-up. This daemon is a std thread driving a
            // current-thread runtime in short bursts, not an async service, so
            // the change is picked up by a non-blocking check at the top of each
            // pass rather than by an async `select!`. The accept poll is 10ms, so
            // "as soon as a save lands" costs at most that much -- not the 1s
            // observer period and not a new user message.
            //
            // The very first tick must not count as a change: subscribing yields
            // an already-changed value, and reacting to it would refresh once for
            // nothing at startup.
            let mut live_admission_wakeup = coordinator.live_admission_wakeup();
            if let Some(receiver) = live_admission_wakeup.as_mut() {
                receiver.borrow_and_update();
            }
            while !thread_stop.load(Ordering::Acquire) {
                // Retry the queue the moment capacity can have changed. Reading
                // the new limits and re-running admission are two steps on
                // purpose: the limit that admitted a task is the one that was in
                // force at that grant, not a value sampled before it.
                if let Some(receiver) = live_admission_wakeup.as_mut()
                    && receiver.has_changed().unwrap_or(false)
                {
                    receiver.borrow_and_update();
                    if let Err(error) = runtime.block_on(coordinator.refresh_live_admission()) {
                        tracing::error!(
                            error = %error,
                            "daemon could not apply the changed admission limits"
                        );
                    } else {
                        let admitted = runtime.block_on(coordinator.wake_queued_residents());
                        if admitted > 0 {
                            tracing::info!(
                                admitted,
                                "admitted queued resident children after a capacity change"
                            );
                        }
                    }
                }
                // The directory this daemon serves can be removed underneath it
                // -- `git worktree remove --force` on the project does exactly
                // that, and the socket survives in $TMPDIR when the runtime path
                // is too long for `sun_path`. Every later request then fails to
                // open a database that no longer exists, while the socket stays
                // alive, so nothing upstream recognises the daemon as dead and it
                // retries forever. A missing runtime directory is terminal: say
                // why, then stop so the socket dies and the normal repair path
                // can take over.
                if last_runtime_dir_check
                    .map(|last| last.elapsed() >= DAEMON_RUNTIME_DIR_CHECK)
                    .unwrap_or(true)
                {
                    last_runtime_dir_check = Some(Instant::now());
                    if !watched_runtime_dir.exists() {
                        tracing::warn!(
                            runtime_dir = %watched_runtime_dir.display(),
                            "runtime directory disappeared; stopping this daemon so its socket can be replaced"
                        );
                        thread_stop.store(true, Ordering::Release);
                        break;
                    }
                }
                // Worker facts arrive independently of client traffic. A failure
                // here used to vanish, so a wedged repository looked like a
                // healthy daemon: keep listening, but say what broke.
                if let Err(error) = runtime.block_on(coordinator.reconcile_worker_events()) {
                    tracing::error!(
                        error = %error,
                        "daemon worker reconciliation failed; retrying next tick"
                    );
                }
                let now = Local::now();
                let minute = now
                    .with_second(0)
                    .and_then(|value| value.with_nanosecond(0));
                if minute != last_schedule_minute {
                    Daemon::run_minute_maintenance(&runtime, &coordinator, now);
                    last_schedule_minute = minute;
                }
                // Block until a connection arrives or the wait times out. This
                // keeps `thread_stop` on a fixed cadence without adding the
                // wait's whole duration to every request, which is what the old
                // non-blocking-plus-sleep shape did.
                let ready = match daemon_accept_wait(&listener, DAEMON_ACCEPT_POLL) {
                    Ok(ready) => ready,
                    Err(error) => {
                        tracing::error!(error = %error, "daemon accept poll failed; retrying");
                        continue;
                    }
                };
                if !ready {
                    continue;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let database_path = database_path.clone();
                        let request_context = Arc::clone(&request_context);
                        let stop = Arc::clone(&thread_stop);
                        let coordinator = Arc::clone(&coordinator);
                        let confirmations = Arc::clone(&listener_confirmations);
                        let handler = thread::spawn(move || {
                            let _ = handle_client(
                                stream,
                                &database_path,
                                &request_context,
                                &stop,
                                &coordinator,
                                &confirmations,
                            );
                        });
                        let mut handlers = listener_handlers
                            .lock()
                            .expect("client handler list mutex poisoned");
                        // Reap finished handlers before recording this one. A
                        // finished thread's stack stays resident for as long as
                        // its `JoinHandle` is alive, and the app-server polls
                        // over fresh short-lived connections; keeping every
                        // handle forever therefore leaked one stack per
                        // connection for the daemon's whole lifetime (hundreds
                        // of megabytes over hours, with only a handful of live
                        // threads). Dropping a finished handle releases its
                        // stack; live handles stay so shutdown can still join.
                        handlers.retain(|handle| !handle.is_finished());
                        handlers.push(handler);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
            let handlers = std::mem::take(
                &mut *listener_handlers
                    .lock()
                    .expect("client handler list mutex poisoned"),
            );
            for handler in handlers {
                let _ = handler.join();
            }
            remove_socket_if_same_inode(&cleanup_socket, cleanup_socket_identity);
            // Only removes the directory when it is empty. The guardian shares
            // `sockets/<hash>/`, so on a normal teardown its control socket is still
            // present and this fails with ENOTEMPTY -- deliberately ignored.
            if let Some(socket_dir) = cleanup_socket.parent() {
                let _ = fs::remove_dir(socket_dir);
            }
            let _ = remove_if_exists(&cleanup_lock);
        });
        Ok(Self {
            socket_path,
            stop,
            coordinator: daemon_coordinator,
            listener: Some(listener),
            bridge_thread,
            reclaimed_orphans,
            _instance_lock: instance_lock,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The coordinator this daemon serves.
    ///
    /// Test hook: the foreground lifecycle tests drive a real daemon over its socket
    /// and then assert on coordinator state (which children exist, which are
    /// attributable to an owner). Reading it from a projection instead would let the
    /// two disagree, which is the failure class this work is about.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn coordinator_for_test(&self) -> &Arc<RuntimeCoordinator> {
        &self.coordinator
    }

    /// 每分钟的周期维护。抽成方法是为了让测试能驱动**线程每分会调用的同一条
    /// 路径**，而不是等墙上时钟走到下一个整分（否则测试只能绕过驱动路径
    /// 直调各方法，那正好测不到「接线」本身）。
    fn run_minute_maintenance(
        runtime: &tokio::runtime::Runtime,
        coordinator: &RuntimeCoordinator,
        now: DateTime<Local>,
    ) {
        let _ = coordinator.evaluate_schedules(now);
        // 看门狗决定任务生死（终止越限 attempt 并释放其租约），所以它的失败
        // 不能像 prune 那样静默：一个失败的 tick 意味着越限的任务又活了一分钟。
        // `evaluate_watchdogs` 是 async（要拿 supervisor 的 async 锁），用
        // current-thread runtime 的 block_on 驱动——与 `reconcile_worker_events`
        // 在 accept 循环里的调用方式一致。
        match runtime.block_on(coordinator.evaluate_watchdogs(Utc::now())) {
            Ok(terminated) if terminated > 0 => {
                tracing::info!(terminated, "watchdog terminated over-budget attempts")
            }
            Ok(_) => {}
            Err(error) => tracing::error!(
                error = %error,
                "watchdog evaluation failed; over-budget attempts survive until the next tick"
            ),
        }
        // Trace retention is cleanup, not work: a failure here must never wedge
        // the accept loop, but a wedged database must be visible rather than silent.
        match coordinator.prune_terminal_traces(Utc::now()) {
            Ok(pruned) => tracing::debug!(pruned, "pruned terminal task traces"),
            Err(error) => tracing::warn!(
                error = %error,
                "terminal trace pruning failed; retrying next minute"
            ),
        }
        // Abandoned fork uploads are the other per-minute leak: a client that
        // dies mid-upload leaves state a later client may never claim.
        let expired_uploads = coordinator.prune_expired_fork_uploads(Instant::now());
        if expired_uploads > 0 {
            tracing::debug!(expired_uploads, "pruned expired fork uploads");
        }
    }

    /// How many orphaned tasks this daemon's startup sweep reclaimed.
    ///
    /// Only a daemon whose caller owns the terminal should print this. The TUI
    /// starts the daemon on its own thread while it paints an alternate screen,
    /// so writing here would land in the middle of a frame and smudge the
    /// layout; it surfaces the count as a transcript notice instead. The CLI
    /// (`daemon serve`) and headless runs have a terminal of their own and keep
    /// printing the historical one-liner.
    pub fn reclaimed_orphans(&self) -> usize {
        self.reclaimed_orphans
    }

    /// The coordinator this daemon serves, for a managed launcher that has to
    /// install the guardian's authority before the socket is reachable.
    pub fn coordinator(&self) -> &Arc<RuntimeCoordinator> {
        &self.coordinator
    }

    /// Stop this manually started daemon and release its local runtime files.
    pub fn stop(&mut self) -> Result<(), IpcError> {
        prepare_coordinator_for_stop(&self.coordinator)?;
        self.stop_listener()
    }

    fn stop_listener(&mut self) -> Result<(), IpcError> {
        self.stop.store(true, Ordering::Release);
        // Wake the accept loop so shutdown does not wait out the poll interval:
        // the connection makes the listener readable, so `poll` returns at once.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(listener) = self.listener.take() {
            listener.join().map_err(|_| IpcError::ListenerPanicked)?;
        }
        // After the listener, not before: a client mid-request may still need an
        // install to land, and tearing the bridge down first would turn that request
        // into a timeout it does not deserve.
        if let Some(bridge) = self.bridge_thread.take() {
            let _ = bridge.join();
        }
        Ok(())
    }

    /// Block the owning daemon process until a local or IPC stop completes.
    pub fn wait(mut self) -> Result<(), IpcError> {
        if let Some(listener) = self.listener.take() {
            listener.join().map_err(|_| IpcError::ListenerPanicked)?;
        }
        Ok(())
    }
}

/// Resolves the Unix-domain socket for a runtime directory.
///
/// Raises the process soft file-descriptor limit toward the hard limit.
///
/// A daemon holds one descriptor per live client, worker, and database handle,
/// and an embedding test process holds many daemons at once. The default soft
/// limit (256 on macOS) is easily exhausted, at which point `accept` fails with
/// `EMFILE` and the listener thread exits, dropping every connection. Raising
/// the soft limit toward the hard limit lets the daemon keep serving.
pub(crate) fn raise_file_descriptor_limit() {
    static RAISED: std::sync::Once = std::sync::Once::new();
    RAISED.call_once(|| {
        // Enough headroom for a busy daemon without claiming the whole hard
        // limit on a machine that allows an enormous one.
        const TARGET: libc::rlim_t = 4096;
        // SAFETY: `getrlimit`/`setrlimit` receive a valid pointer to a
        // zero-initialized `rlimit`. Only the soft limit is raised, and never
        // above the hard limit the kernel enforces.
        unsafe {
            let mut limit = std::mem::zeroed::<libc::rlimit>();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return;
            }
            let target = if limit.rlim_max == libc::RLIM_INFINITY {
                TARGET
            } else {
                limit.rlim_max.min(TARGET)
            };
            if limit.rlim_cur >= target {
                return;
            }
            limit.rlim_cur = target;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    });
}

/// The exclusive instance lock for one runtime directory.
///
/// Backed by an advisory `flock` on `runtime.lock` rather than a PID file, so
/// ownership is decided by the kernel and released automatically when the
/// holding process exits — including on a crash. A PID file cannot do this: the
/// OS recycles PIDs, so a lock left by a dead daemon starts looking owned by
/// whatever unrelated process inherits that number, and `kill -0` then reports
/// it as permanently live.
///
/// The lock file itself is left in place. Removing it would race a concurrent
/// acquirer that already opened the same inode: the newcomer would hold a lock
/// on an unlinked file while a third process created a fresh one, and two
/// daemons would both believe they own the directory.
pub(crate) struct InstanceLock {
    /// Dropping this file closes its descriptor, and the kernel releases the
    /// `flock` with it. That is the whole unlock protocol: no `Drop` impl is
    /// needed, and a process that dies without unwinding still releases the lock.
    _file: std::fs::File,
}

impl InstanceLock {
    /// Takes the lock, or reports that another daemon already holds it.
    fn acquire(runtime_dir: &Path, lock_path: &Path) -> Result<Self, IpcError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            return Ok(Self { _file: file });
        }
        let error = std::io::Error::last_os_error();
        // `EWOULDBLOCK` is how a held advisory lock reports contention; anything
        // else is a real failure (a bad descriptor, an unsupported filesystem)
        // and must not be misreported as "another daemon is running".
        if matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            return Err(IpcError::AlreadyRunning {
                path: runtime_dir.to_path_buf(),
            });
        }
        Err(IpcError::Io(error))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Destructors can run while Tokio is driving an async test or caller.
        // Starting a second current-thread runtime here panics, so only an
        // explicit `stop()` performs the cooperative checkpoint protocol.
        let _ = self.stop_listener();
    }
}

/// Refuse a database this build cannot safely write, before touching it.
///
/// Logically `schema_compat::require_compatible`, lifted into `IpcError` so the
/// daemon's existing error surface reports it. Kept as its own function so the
/// ordering above is visible at the call site.
fn yi_agent_store_schema_preflight(database_path: &Path) -> Result<(), IpcError> {
    crate::schema_compat::require_compatible(
        database_path,
        crate::schema_compat::SchemaRange::supported(),
    )
    .map_err(|error| IpcError::SchemaIncompatible(error.to_string()))
}

pub(crate) fn remove_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes a socket node only if it is still the one we bound.
///
/// The daemon's exit path unlinks its socket by path. Between a runtime
/// directory being removed and rebuilt -- which is exactly what a kanban retry
/// does -- a replacement daemon can bind the same path first, and the outgoing
/// daemon would then unlink *its* socket, stranding the replacement the same way
/// this whole design exists to prevent. Comparing `(dev, ino)` deletes only the
/// node this daemon bound.
pub(crate) fn remove_socket_if_same_inode(path: &Path, expected: Option<(u64, u64)>) {
    use std::os::unix::fs::MetadataExt;
    // No recorded identity means we cannot tell our node from someone else's, so
    // leave the path alone rather than risk unlinking a live daemon's socket.
    let Some(expected) = expected else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if (metadata.dev(), metadata.ino()) == expected {
        let _ = remove_if_exists(path);
    }
}

/// Wait until the listener is readable (a connection is pending) or the timeout
/// elapses; the granularity matches [`DAEMON_ACCEPT_POLL`].
///
/// The plugin side does the same: `poll(2)` replaces "non-blocking plus sleep",
/// so a connection returns immediately rather than adding a blind 0-10ms wait to
/// every request, while `thread_stop` still gets checked on a fixed cadence.
pub(crate) fn daemon_accept_wait(
    listener: &UnixListener,
    timeout: Duration,
) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: one initialized `pollfd`, so `nfds = 1` matches it; `poll` only
    // reads `fd`/`events` and writes `revents`. There are no other pointer
    // arguments.
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    match ready {
        // Timed out: no connection arrived.
        0 => Ok(false),
        // Readable, or hung up: `accept` reports the specific error if so.
        count if count > 0 => Ok(true),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// accept wait granularity: it only decides how often `thread_stop` is checked,
/// it is no longer a per-request latency floor.
pub(crate) const DAEMON_ACCEPT_POLL: Duration = Duration::from_millis(10);

/// How often the accept loop asks whether the runtime directory it serves still
/// exists. A missing runtime directory is terminal and unrecoverable -- this
/// daemon's database lives there -- so the check is deliberately coarse: the
/// 10ms accept poll would stat a directory every tick for nothing.
pub(crate) const DAEMON_RUNTIME_DIR_CHECK: Duration = Duration::from_secs(1);

/// The `protocol_version` a frame declares, read without parsing the command.
///
/// `None` means the frame is not even shaped like an envelope; the ordinary parse
/// path then reports it as a validation failure with a request id where possible.
fn envelope_version(frame: &[u8]) -> Option<u32> {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()?
        .get("protocol_version")?
        .as_u64()
        .map(|version| version as u32)
}

/// Whether a frame's command is one that must carry a turn fence.
///
/// Matched on the raw JSON rather than on `IpcRequest` on purpose: the whole
/// point is to answer an old envelope whose command would otherwise parse into a
/// perfectly ordinary unfenced write.
fn is_fenced_write(frame: &[u8]) -> bool {
    const FENCED: [&str; 10] = [
        "SpawnApplicationChild",
        "SpawnChild",
        "BeginForkUpload",
        "SendApplicationMessage",
        "CancelChild",
        "PrepareSessionDelegation",
        "BeginSessionTurn",
        "EndSessionTurn",
        "OpenSessionHostControl",
        "AttachSessionContainer",
    ];
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| {
            value
                .get("command")?
                .get("type")?
                .as_str()
                .map(str::to_owned)
        })
        .is_some_and(|kind| FENCED.contains(&kind.as_str()))
}

pub(crate) fn handle_client(
    mut stream: UnixStream,
    database_path: &Path,
    context: &RequestContext,
    stop: &Arc<AtomicBool>,
    coordinator: &Arc<RuntimeCoordinator>,
    confirmations: &Arc<ConfirmationStore>,
) -> Result<(), IpcError> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    // The read timeout guards against a half-open peer and stays tight. The
    // write timeout must span a full 1 MiB response to a slow reader, so it
    // tracks COMMAND_WRITE_DEADLINE rather than the read budget.
    stream.set_write_timeout(Some(COMMAND_WRITE_DEADLINE))?;
    let frame = match read_incoming_frame(&mut BufReader::new(stream.try_clone()?))? {
        Some(IncomingFrame::TooLarge(prefix)) => {
            return write_response_frame(
                &mut stream,
                &request_id_from_prefix(&prefix),
                None,
                &IpcResponse::Error {
                    code: IpcErrorCode::Validation,
                    message: None,
                },
            );
        }
        None => return Ok(()),
        Some(IncomingFrame::Complete(frame)) => frame,
    };
    let request_id = request_id_from_frame(&frame);
    // The version is decided from the raw frame, before the command is parsed.
    // `session_turn` is additive and optional, so an old envelope still decodes;
    // deciding afterwards would let a v2 write look like a legitimate unfenced
    // one. Reading the version first makes "upgrade the client" a different
    // answer from "fix the request" no matter what the command contains.
    if let Some(version) = envelope_version(&frame)
        && version != PROTOCOL_VERSION
    {
        {
            let response = if is_fenced_write(&frame) {
                session_rejection(RuntimeCoordinatorError::session_delegation(
                    yi_agent_lifecycle::session_delegation::SessionFailureCode::ProtocolMismatch,
                    format!(
                        "protocol version {version} cannot issue a fenced session write; \
                         this daemon speaks {PROTOCOL_VERSION}"
                    ),
                ))
            } else {
                IpcResponse::UnsupportedProtocol {
                    supported_min: PROTOCOL_VERSION,
                    supported_max: PROTOCOL_VERSION,
                }
            };
            return write_response_frame(&mut stream, &request_id, None, &response);
        }
    }
    let response = match serde_json::from_slice::<RequestEnvelope>(&frame) {
        Ok(envelope) => match envelope.command {
            IpcRequest::Stop => {
                prepare_coordinator_for_stop(coordinator)?;
                stop.store(true, Ordering::Release);
                IpcResponse::Stopping
            }
            IpcRequest::SubscribeTrace {
                task_ids,
                after_id,
                kinds,
            } => {
                let requested = task_ids
                    .iter()
                    .map(|task_id| parse_id::<TaskId>(task_id))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| {
                        IpcError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "invalid task id in trace subscription",
                        ))
                    })
                    .unwrap_or_default();
                if requested.is_empty() {
                    return write_response_frame(
                        &mut stream,
                        &envelope.request_id,
                        None,
                        &IpcResponse::Error {
                            code: IpcErrorCode::Validation,
                            message: Some(
                                "a trace subscription requires at least one valid task id".into(),
                            ),
                        },
                    );
                }
                let subscription = stream_trace_subscription(
                    &mut stream,
                    database_path,
                    Arc::clone(stop),
                    Arc::clone(coordinator),
                    requested,
                    after_id.unwrap_or(0),
                    kinds,
                    &envelope.request_id,
                );
                return respond_to_subscription_result(
                    &mut stream,
                    &envelope.request_id,
                    subscription,
                );
            }
            IpcRequest::OpenSessionHostControl { binding } => {
                // A control stream, not a request: it owns this connection for as
                // long as the host keeps it open, so it is dispatched before the
                // one-shot reply path below and answered in place.
                return stream_session_host_control(&mut stream, coordinator, binding, &request_id);
            }
            IpcRequest::SubscribeEvents {
                after_event_id,
                filters,
            } => {
                let subscription = stream_subscription(
                    &mut stream,
                    database_path,
                    Arc::clone(stop),
                    Arc::clone(coordinator),
                    after_event_id,
                    filters,
                    &envelope.request_id,
                );
                return respond_to_subscription_result(
                    &mut stream,
                    &envelope.request_id,
                    subscription,
                );
            }
            IpcRequest::PreviewCancel { task_id, recursive } => {
                match preview_cancel(database_path, confirmations, task_id, recursive) {
                    Ok(response) => response,
                    Err(error) => {
                        log_request_failure(&envelope.request_id, &error);
                        error_response(&error)
                    }
                }
            }
            IpcRequest::ConfirmCancel {
                task_id,
                recursive,
                confirmation_token,
            } => match confirm_cancel(
                database_path,
                coordinator,
                confirmations,
                task_id,
                recursive,
                confirmation_token,
            ) {
                Ok(response) => response,
                Err(error) => {
                    log_request_failure(&envelope.request_id, &error);
                    error_response(&error)
                }
            },
            request => match respond(context, coordinator, request) {
                Ok(response) => response,
                Err(error) => {
                    log_request_failure(&envelope.request_id, &error);
                    error_response(&error)
                }
            },
        },
        Err(_) => IpcResponse::Error {
            code: IpcErrorCode::Validation,
            message: None,
        },
    };
    write_response_frame(&mut stream, &request_id, None, &response)
}

/// Serves the host control stream until the host closes it.
///
/// One frame at a time, each answered with the same `request_id` so a host can
/// correlate replies with what it sent. A refusal is answered, never silently
/// dropped: a host that cannot register an execution has to learn that before it
/// starts doing work, not by a later write failing for an unexplained reason.
pub(crate) fn stream_session_host_control(
    stream: &mut UnixStream,
    coordinator: &Arc<RuntimeCoordinator>,
    binding: yi_agent_lifecycle::session_delegation::SessionBinding,
    request_id: &str,
) -> Result<(), IpcError> {
    if let Err(error) = coordinator.open_host_control(&binding) {
        let error = IpcError::from(error);
        log_request_failure(request_id, &error);
        write_response_frame(stream, request_id, None, &error_response(&error))?;
        return Ok(());
    }
    write_response_frame(
        stream,
        request_id,
        None,
        &IpcResponse::SessionHostControlReady,
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    // The accepted socket inherits `O_NONBLOCK` from the listener, so a plain
    // read reports `WouldBlock` whenever no frame is queued yet. A control stream
    // is idle most of its life by design, so the wait is explicit rather than
    // "treat it as a disconnect": the latter would kill every control stream on
    // its first quiet moment.
    reader
        .get_ref()
        .set_nonblocking(false)
        .map_err(IpcError::Io)?;
    loop {
        let frame = match read_limited_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            // A host that goes away is the normal end of this stream and says
            // nothing about the container: its durable `starting` rows are what
            // keep the next driver honest.
            Ok(None) | Err(_) => return Ok(()),
        };
        let Ok(frame) = serde_json::from_slice::<SessionHostControlFrame>(&frame) else {
            write_response_frame(
                stream,
                request_id,
                None,
                &IpcResponse::Error {
                    code: IpcErrorCode::Validation,
                    message: None,
                },
            )?;
            continue;
        };
        let response = match frame.request {
            SessionHostControlRequest::Register { turn_id } => {
                match coordinator.register_host_control_execution(&binding, &turn_id) {
                    Ok(execution) => IpcResponse::SessionHostExecutionRegistered { execution },
                    Err(RuntimeCoordinatorError::SessionDelegation { code, reason }) => {
                        IpcResponse::SessionDelegationRejected {
                            failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(
                                code, reason,
                            ),
                        }
                    }
                    Err(error) => {
                        let error = IpcError::from(error);
                        log_request_failure(request_id, &error);
                        error_response(&error)
                    }
                }
            }
            SessionHostControlRequest::Stopped { evidence } => {
                match coordinator.record_host_control_stop(&binding, &evidence) {
                    Ok(()) => IpcResponse::SessionHostStopRecorded,
                    Err(RuntimeCoordinatorError::SessionDelegation { code, reason }) => {
                        IpcResponse::SessionDelegationRejected {
                            failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(
                                code, reason,
                            ),
                        }
                    }
                    Err(error) => {
                        let error = IpcError::from(error);
                        log_request_failure(request_id, &error);
                        error_response(&error)
                    }
                }
            }
        };
        write_response_frame(stream, request_id, None, &response)?;
    }
}

pub(crate) fn respond_to_subscription_result(
    stream: &mut UnixStream,
    request_id: &str,
    result: Result<(), SubscriptionFailure>,
) -> Result<(), IpcError> {
    match result {
        Ok(()) | Err(SubscriptionFailure::AfterInitialFrame) => Ok(()),
        Err(SubscriptionFailure::BeforeInitialFrame) => write_response_frame(
            stream,
            request_id,
            None,
            &IpcResponse::Error {
                code: IpcErrorCode::Internal,
                message: None,
            },
        ),
    }
}

pub(crate) fn preview_cancel(
    database_path: &Path,
    confirmations: &ConfirmationStore,
    task_id: String,
    recursive: bool,
) -> Result<IpcResponse, IpcError> {
    let repository = RuntimeRepository::open(database_path)?;
    let task = parse_id::<TaskId>(&task_id)?;
    let scope = cancellation_scope(&repository, &task, recursive)?;
    let confirmation_token = confirmations.issue(task_id, recursive, scope.clone());
    Ok(IpcResponse::CancelPreview {
        confirmation_token,
        worktree_leases: scope
            .active_leases
            .iter()
            .filter(|lease| lease.resource_key.starts_with("worktree:"))
            .map(|lease| lease.resource_key.clone())
            .collect(),
        task_ids: scope.task_ids,
        active_leases: scope.active_leases,
        unmerged_deliveries: scope.unmerged_deliveries,
        expires_in_secs: CONFIRMATION_TTL.as_secs(),
    })
}

pub(crate) fn cancellation_scope(
    repository: &RuntimeRepository,
    task: &TaskId,
    recursive: bool,
) -> Result<CancelScope, IpcError> {
    let scope = repository.cancel_scope(task, recursive)?;
    Ok(CancelScope {
        task_ids: scope
            .task_ids
            .into_iter()
            .map(|task| task.to_string())
            .collect(),
        active_leases: scope
            .active_leases
            .into_iter()
            .map(|lease| IpcCancelLease {
                lease_id: lease.lease_id,
                task_id: lease.task_id.to_string(),
                resource_key: lease.resource_key,
            })
            .collect(),
        unmerged_deliveries: scope
            .unmerged_deliveries
            .into_iter()
            .map(|delivery| IpcCancelDelivery {
                delivery_id: delivery.delivery_id,
                task_id: delivery.task_id.to_string(),
                payload_json: delivery.payload_json,
            })
            .collect(),
    })
}

pub(crate) fn confirm_cancel(
    database_path: &Path,
    coordinator: &RuntimeCoordinator,
    confirmations: &ConfirmationStore,
    task_id: String,
    recursive: bool,
    confirmation_token: String,
) -> Result<IpcResponse, IpcError> {
    let repository = RuntimeRepository::open(database_path)?;
    let task = parse_id::<TaskId>(&task_id)?;
    let detail = repository.task_detail(&task)?;
    let scope = cancellation_scope(&repository, &task, recursive)?;
    if !confirmations.consume(&confirmation_token, &task_id, recursive, &scope) {
        return Ok(IpcResponse::Error {
            code: IpcErrorCode::ConfirmationRequired,
            message: None,
        });
    }
    let session = parse_id::<RootSessionId>(&detail.session_id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(coordinator.cancel_task(&session, &task, recursive))?;
    Ok(IpcResponse::TaskCancelled)
}

pub(crate) fn prepare_coordinator_for_stop(
    coordinator: &RuntimeCoordinator,
) -> Result<(), IpcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(IpcError::Io)?;
    runtime.block_on(async {
        coordinator
            .graceful_stop(RuntimeStopOptions::default())
            .await
    })?;
    Ok(())
}

pub(crate) fn write_response_frame(
    stream: &mut UnixStream,
    request_id: &str,
    event_id: Option<i64>,
    response: &IpcResponse,
) -> Result<(), IpcError> {
    write_envelope_frame(
        stream,
        &response_envelope(request_id, event_id, response.clone()),
    )
}

/// Writes a whole frame, tolerating partial writes and backpressure.
///
/// The accepted connection inherits `O_NONBLOCK` from the non-blocking
/// listener, so a single `write` consumes only what fits in the socket send
/// buffer (8192 bytes on macOS) and then reports `WouldBlock`. `write_all`
/// treats that as fatal and abandons the response mid-frame; this loops
/// instead, sleeping briefly until the peer drains the buffer or the deadline
/// expires.
pub(crate) fn write_frame_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), IpcError> {
    let mut written = 0usize;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write response frame",
                )));
            }
            Ok(step) => written += step,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(IpcError::FrameWriteTimeout {
                        written,
                        total: bytes.len(),
                    });
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(IpcError::Io(error)),
        }
    }
    Ok(())
}

pub(crate) fn write_envelope_frame(
    stream: &mut UnixStream,
    envelope: &ResponseEnvelope,
) -> Result<(), IpcError> {
    let frame = match encode_envelope(envelope) {
        Ok(frame) => frame,
        // Resynchronization is meaningful only for event subscriptions. A
        // regular command reply must retain its typed error semantics.
        Err(IpcError::FrameTooLarge) => encode_envelope(&response_envelope(
            &envelope.request_id,
            None,
            IpcResponse::Error {
                code: IpcErrorCode::Internal,
                message: None,
            },
        ))?,
        Err(error) => return Err(error),
    };
    let deadline = Instant::now() + COMMAND_WRITE_DEADLINE;
    write_frame_until(stream, &frame, deadline)?;
    write_frame_until(stream, b"\n", deadline)?;
    stream.flush()?;
    Ok(())
}

/// 是否该在本次请求里跑一轮 worker 对账。
///
/// daemon 的 accept 循环已经每轮跑 [`RuntimeCoordinator::reconcile_worker_events`]，
/// `respond` 里那次是重复的。窗口只用来合并「几乎同时到达」的请求，保证请求仍能
/// 观察到已对账状态，而不是把对账从请求路径上彻底删掉。
pub(crate) fn should_reconcile(last: Option<Instant>, now: Instant, window: Duration) -> bool {
    match last {
        Some(previous) => now.duration_since(previous) >= window,
        None => true,
    }
}

/// 每个 daemon 一份的请求执行上下文：复用一个 tokio runtime 与一条 sqlite 连接。
///
/// `respond` 原先**每请求**都新建这两样（`Runtime::new` + `RuntimeRepository::open`
/// 里的两条 PRAGMA 与 `migrate` 的 schema 解析），单次 `Status` 因此要 ~45ms。
/// `rusqlite::Connection` 是 `!Sync`，故用 `Mutex` 串行持有——daemon 的请求路径
/// 本就串行，不引入新的并发度。
pub(crate) struct RequestContext {
    runtime: tokio::runtime::Runtime,
    repository: std::sync::Mutex<RuntimeRepository>,
    reconciled_at: std::sync::Mutex<Option<Instant>>,
    reconcile_window: Duration,
    /// 本 daemon 的项目身份：它的 runtime 目录。插件路由按它分区，因为同一个
    /// 进程可能替多个项目各起一个 daemon（桌面 app-server 就是如此）。
    scope: PathBuf,
}

/// 同一窗口内的并发请求合并成一次对账。
pub(crate) const RECONCILE_WINDOW: Duration = Duration::from_millis(5);

impl RequestContext {
    fn new(scope: &Path, database_path: &Path) -> Result<Self, IpcError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let repository = RuntimeRepository::open(database_path).map_err(IpcError::from)?;
        Ok(Self {
            runtime,
            repository: std::sync::Mutex::new(repository),
            scope: scope.to_path_buf(),
            reconciled_at: std::sync::Mutex::new(None),
            reconcile_window: RECONCILE_WINDOW,
        })
    }

    /// 借出那条长生命周期连接。调用方在闭包内串行使用它。
    ///
    /// `respond` 只有那四个真正读写 sqlite 的 arm 走这里，借还都落在 arm 内，
    /// 绝不跨 `block_on`（尤其是能永久挂住的 `WaitAgent`）持有——连接的互斥因此
    /// 不会把一个请求的等待时间转嫁到另一个请求上。
    fn with_repository<R>(
        &self,
        f: impl FnOnce(&mut RuntimeRepository) -> Result<R, IpcError>,
    ) -> Result<R, IpcError> {
        let mut repository = self
            .repository
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut repository)
    }

    /// 需要时跑一轮对账；窗口内重复调用直接返回。
    fn reconcile_if_stale(&self, coordinator: &Arc<RuntimeCoordinator>) -> Result<(), IpcError> {
        let now = Instant::now();
        let last = *self
            .reconciled_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !should_reconcile(last, now, self.reconcile_window) {
            return Ok(());
        }
        self.runtime
            .block_on(coordinator.reconcile_worker_events())?;
        *self
            .reconciled_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        Ok(())
    }
}

/// 维护屏障查询。
///
/// # 三条判定，顺序不能换
///
/// 1. **协议兼容**先判：对端太旧时它根本不会理解"closed"这个回答，而它
///    自己的启动路径也不会来问。此时唯一安全的答复是明确拒绝，并告诉用户
///    该升级哪一侧。把它读成"没有维护"会让旧 daemon 在迁移期间照常拉起
///    后端——那正是这条协议要防的事。
/// 2. 插件 id 必须可解析。解析失败不是"没有这个插件"，而是"我看不懂这条
///    请求"，同样不能当成放行。
/// 3. 最后才读磁盘。**每次都读**：内存里的结论在通知丢失、或事务由别的
///    进程推进时，会与事实相反。
fn plugin_maintenance(plugin: String, protocol: Option<u32>) -> Result<IpcResponse, IpcError> {
    use crate::ipc::managed::{Compatibility, MaintenanceBarrier, MaintenanceState};

    // 直接回 `IpcResponse::Error` 而不是 `Err(IpcError::...)`：`error_response`
    // 刻意把 Io 类错误的 message 抹成 `None`，而这里必须把"升级哪一侧"
    // 送到用户眼前——只说"validation failed"等于什么都没说。这与
    // `CancelTask` 的处理方式一致：可预期的对端错误带上消息直接回。
    let compatibility = Compatibility::check(protocol);
    if !compatibility.is_supported() {
        return Ok(IpcResponse::Error {
            code: IpcErrorCode::Validation,
            message: Some(format!(
                "maintenance protocol {} is not supported by this daemon: {}",
                protocol.unwrap_or(0),
                compatibility.advice()
            )),
        });
    }

    let plugin = match crate::ipc::managed::parse_plugin(&plugin) {
        Ok(plugin) => plugin,
        Err(error) => {
            return Ok(IpcResponse::Error {
                code: IpcErrorCode::Validation,
                message: Some(error.to_string()),
            });
        }
    };

    // 实例根可能不存在（测试、或宿主还没初始化）。没有实例根就**没有**
    // 任何事务日志，因此结论是 `open`——但这是"确实没有记录"，
    // 不是"读失败"，两者在 `MaintenanceState` 里必须分开。
    let Some(home) = crate::paths::instance_home() else {
        return Ok(IpcResponse::PluginMaintenanceStatus {
            state: MaintenanceState::Open.as_str().to_string(),
            transaction_id: None,
            phase: None,
            detail: Some("no instance home is configured; no transaction can be recorded".into()),
        });
    };

    let status = MaintenanceBarrier::at(home).state(&plugin);
    Ok(IpcResponse::PluginMaintenanceStatus {
        state: status.state.as_str().to_string(),
        transaction_id: status.transaction_id,
        phase: status.phase,
        detail: status.detail,
    })
}

/// A session-delegation refusal, as the typed response the client branches on.
///
/// Without this, a refusal raised inside a coordinator call would be flattened by
/// `error_response` into a bare `Conflict` frame, and the model would learn only
/// "something was wrong" instead of "this turn is stale, open a new one".
pub(crate) fn session_rejection(error: RuntimeCoordinatorError) -> IpcResponse {
    match error {
        RuntimeCoordinatorError::SessionDelegation { code, reason } => {
            IpcResponse::SessionDelegationRejected {
                failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(code, reason),
            }
        }
        other => error_response(&IpcError::from(other)),
    }
}

pub(crate) fn respond(
    context: &RequestContext,
    coordinator: &Arc<RuntimeCoordinator>,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    context.reconcile_if_stale(coordinator)?;
    // 每条连接一个 handler 线程，所以请求**是**并发的：`respond` 里既有秒级
    // 的协调器调用，也有能永久挂住的 `WaitAgent`（`timeout_ms: None` 时
    // `wait_for_children_authorized` 永不返回）。连接因此必须**按需**借、绝不可
    // 跨这些调用持有：否则一个仍在等待的 `WaitAgent` 会把每个 `Status` 轮询都
    // 挡在锁后——恰恰是这次改动要修掉的那种「`pwd && ls` 都慢」。真正用到连接
    // 的只有下面四个 arm，它们都只做一次本地 sqlite 读写，不阻塞。
    match request {
        IpcRequest::Status => Ok(IpcResponse::Status {
            high_water_event_id: context.with_repository(|repository| {
                repository.latest_event_id().map_err(IpcError::from)
            })?,
            // This daemon refreshes its live-admission source and its workers'
            // settings at each operation boundary (see `runtime.rs`), so it can
            // honour "a save reaches in-flight work". Advertising it is what lets
            // a client tell this apart from an older daemon that cannot.
            live_apply_version: Some(crate::ipc::LIVE_APPLY_VERSION),
        }),
        IpcRequest::Stop => Ok(IpcResponse::Stopping),
        IpcRequest::CreateSession => {
            let session_id = coordinator.create_session()?;
            let root_task_id = coordinator.root_task_id(&session_id)?;
            Ok(IpcResponse::SessionCreated {
                session_id: session_id.to_string(),
                root_task_id: root_task_id.to_string(),
            })
        }
        IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace,
            extra_roots,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let attached = runtime.block_on(coordinator.attach_application_root(
                &idempotency_key,
                &workspace,
                &extra_roots,
            ))?;
            Ok(IpcResponse::ApplicationRootAttached {
                session_id: attached.session_id.to_string(),
                root_task_id: attached.root_task_id.to_string(),
                message_capability: attached.message_capability,
                workspace: attached.workspace,
            })
        }
        IpcRequest::ActivateApplicationRoot {
            session_id,
            root_task_id,
            capability,
            objective,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let root_task_id = parse_id::<TaskId>(&root_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.activate_application_root(
                &session_id,
                &root_task_id,
                &capability,
                objective,
            ))?;
            Ok(IpcResponse::ApplicationRootActivated)
        }
        IpcRequest::DetachApplicationRoot {
            session_id,
            root_task_id,
            capability,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let root_task_id = parse_id::<TaskId>(&root_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.detach_application_root(
                &session_id,
                &root_task_id,
                &capability,
            ))?;
            // Reclaim BEFORE answering, on this handler thread.
            //
            // Running it on a detached thread looks harmless but is unbounded
            Ok(IpcResponse::ApplicationRootDetached)
        }
        IpcRequest::AttachSessionContainer {
            idempotency_key,
            workspace,
            extra_roots,
            operation_id,
            creation_resume_digest,
            identity,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let root = match runtime.block_on(coordinator.attach_session_container(
                &idempotency_key,
                &workspace,
                &extra_roots,
                &operation_id,
                creation_resume_digest,
                identity,
            )) {
                Ok(root) => root,
                Err(RuntimeCoordinatorError::SessionDelegation { code, reason }) => {
                    return Ok(IpcResponse::SessionDelegationRejected {
                        failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(
                            code, reason,
                        ),
                    });
                }
                Err(error) => return Err(error.into()),
            };
            Ok(IpcResponse::SessionContainerAttached {
                root: root.reference(),
                workspace: root.workspace,
            })
        }
        IpcRequest::PrepareSessionDelegation { request } => {
            match coordinator.prepare_session(request) {
                Ok(binding) => Ok(IpcResponse::SessionPrepared { binding }),
                Err(RuntimeCoordinatorError::SessionDelegation { code, reason }) => {
                    Ok(IpcResponse::SessionDelegationRejected {
                        failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(
                            code, reason,
                        ),
                    })
                }
                Err(error) => Err(error.into()),
            }
        }
        IpcRequest::BeginSessionTurn { binding, turn_id } => {
            match coordinator.begin_session_turn(&binding, &turn_id) {
                Ok(turn) => Ok(IpcResponse::SessionTurnOpened { turn }),
                Err(RuntimeCoordinatorError::SessionDelegation { code, reason }) => {
                    Ok(IpcResponse::SessionDelegationRejected {
                        failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(
                            code, reason,
                        ),
                    })
                }
                Err(error) => Err(error.into()),
            }
        }
        IpcRequest::EndSessionTurn { turn } => match coordinator.end_session_turn(&turn) {
            Ok(()) => Ok(IpcResponse::SessionTurnClosed),
            Err(RuntimeCoordinatorError::SessionDelegation { code, reason }) => {
                Ok(IpcResponse::SessionDelegationRejected {
                    failure: yi_agent_lifecycle::session_delegation::SessionFailure::new(
                        code, reason,
                    ),
                })
            }
            Err(error) => Err(error.into()),
        },
        IpcRequest::BindApplicationRootDriver {
            authorization,
            operation_id,
        } => {
            match coordinator.bind_foreground_driver(&authorization, &operation_id) {
                Ok(binding) => Ok(IpcResponse::ForegroundDriverBound { binding }),
                // A foreground refusal is a policy answer with a code the client
                // dispatches on, so it must not be flattened into `invalid_state`.
                Err(RuntimeCoordinatorError::Repository(RepositoryError::Foreground(error))) => Ok(
                    IpcResponse::ForegroundFailure(crate::ipc::foreground_failure(&error)),
                ),
                Err(error) => Err(error.into()),
            }
        }
        IpcRequest::SetApplicationRootActivity {
            driver_capability,
            update,
        } => match coordinator.set_foreground_activity(&driver_capability, &update) {
            Ok(snapshot) => Ok(IpcResponse::ForegroundActivity { snapshot }),
            Err(RuntimeCoordinatorError::Repository(RepositoryError::Foreground(error))) => Ok(
                IpcResponse::ForegroundFailure(crate::ipc::foreground_failure(&error)),
            ),
            Err(error) => Err(error.into()),
        },
        IpcRequest::GetApplicationRootActivity {
            authorization,
            root,
        } => match coordinator.get_foreground_activity(&authorization, &root) {
            Ok(Some(snapshot)) => Ok(IpcResponse::ForegroundActivity { snapshot }),
            Ok(None) => Ok(IpcResponse::ForegroundActivityAbsent),
            Err(RuntimeCoordinatorError::Repository(RepositoryError::Foreground(error))) => Ok(
                IpcResponse::ForegroundFailure(crate::ipc::foreground_failure(&error)),
            ),
            Err(error) => Err(error.into()),
        },
        IpcRequest::PrepareApplicationRootRecovery {
            authorization,
            request,
        } => match coordinator.prepare_foreground_recovery(&authorization, &request) {
            Ok(status) => Ok(IpcResponse::ForegroundRecovery { status }),
            Err(RuntimeCoordinatorError::Repository(RepositoryError::Foreground(error))) => Ok(
                IpcResponse::ForegroundFailure(crate::ipc::foreground_failure(&error)),
            ),
            Err(error) => Err(error.into()),
        },
        IpcRequest::RecoverApplicationRoot {
            authorization,
            request,
            confirmation_token,
        } => {
            match coordinator.recover_foreground_root(&authorization, &request, &confirmation_token)
            {
                Ok(result) => Ok(IpcResponse::ForegroundRecovery {
                    status: yi_agent_lifecycle::foreground::RecoveryOperationStatus::Committed(
                        result,
                    ),
                }),
                Err(RuntimeCoordinatorError::Repository(RepositoryError::Foreground(error))) => Ok(
                    IpcResponse::ForegroundFailure(crate::ipc::foreground_failure(&error)),
                ),
                Err(error) => Err(error.into()),
            }
        }
        IpcRequest::GetApplicationRootRecovery {
            authorization,
            root,
            operation_id,
        } => match coordinator.foreground_recovery_status(&authorization, &root, &operation_id) {
            Ok(Some(status)) => Ok(IpcResponse::ForegroundRecovery { status }),
            Ok(None) => Ok(IpcResponse::ForegroundRecoveryAbsent),
            Err(RuntimeCoordinatorError::Repository(RepositoryError::Foreground(error))) => Ok(
                IpcResponse::ForegroundFailure(crate::ipc::foreground_failure(&error)),
            ),
            Err(error) => Err(error.into()),
        },
        IpcRequest::CreateSchedule { cron, objective } => {
            let definition = ScheduleDefinition::new(cron, objective).map_err(|error| {
                IpcError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
            })?;
            let next_run_at = definition.next_run_after(Local::now()).map_err(|error| {
                IpcError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
            })?;
            let schedule = context.with_repository(|repository| {
                repository
                    .create_schedule(&definition, next_run_at)
                    .map_err(IpcError::from)
            })?;
            Ok(IpcResponse::ScheduleCreated {
                schedule_id: schedule.id,
            })
        }
        IpcRequest::ListSchedules => Ok(IpcResponse::Schedules {
            schedules: context
                .with_repository(|repository| repository.schedules().map_err(IpcError::from))?
                .into_iter()
                .map(|schedule| IpcSchedule {
                    schedule_id: schedule.id,
                    cron: schedule.definition.cron,
                    objective: schedule.definition.objective,
                    next_run_at: schedule.next_run_at.to_rfc3339(),
                })
                .collect(),
        }),
        // Handled by `handle_client` before this one-shot dispatcher: the control
        // stream owns its connection, so reaching here means it was not opened.
        IpcRequest::OpenSessionHostControl { .. } => Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a host control stream cannot be opened on the one-shot request path",
        ))),
        // Forwarded verbatim: the daemon never inspects `method` or `params`,
        // and never invents a socket path - only a supervised plugin's declared
        // socket is reachable, so this cannot serve as a local socket proxy.
        IpcRequest::PluginQuery {
            plugin,
            method,
            params,
        } => forward_plugin_query(&context.scope, &plugin, &method, params),
        IpcRequest::PluginMaintenance { plugin, protocol } => plugin_maintenance(plugin, protocol),
        IpcRequest::DeleteSchedule { schedule_id } => {
            if !context.with_repository(|repository| {
                repository
                    .delete_schedule(&schedule_id)
                    .map_err(IpcError::from)
            })? {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "schedule does not exist",
                )));
            }
            Ok(IpcResponse::ScheduleDeleted)
        }
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id,
            objective,
            session_turn,
            mode,
            model,
            workdir,
            sandbox,
            fork_token,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            if let Err(error) =
                coordinator.admit_session_write(&parent_task_id, session_turn.as_ref())
            {
                return Ok(session_rejection(error));
            }
            let workspace_mode = parse_workspace_mode(mode)?;
            let inherited_sandbox = parse_inherited_sandbox(sandbox)?;
            let workdir = workdir.map(std::path::PathBuf::from);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let task_id = runtime.block_on(coordinator.spawn_child_and_admit(
                &session_id,
                &parent_task_id,
                objective,
                workspace_mode,
                model,
                workdir,
                // Legacy session spawns carry no conversation; a descendant
                // still inherits its parent's marker inside the coordinator.
                None,
                inherited_sandbox,
                fork_token,
            ))?;
            Ok(IpcResponse::TaskSpawned {
                task_id: task_id.to_string(),
            })
        }
        IpcRequest::SpawnApplicationChild {
            session_id,
            parent_task_id,
            capability,
            objective,
            mode,
            model,
            workdir,
            thread_id,
            sandbox,
            fork_token,
            session_turn,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            if let Err(error) =
                coordinator.admit_session_write(&parent_task_id, session_turn.as_ref())
            {
                return Ok(session_rejection(error));
            }
            let workspace_mode = parse_workspace_mode(mode)?;
            let inherited_sandbox = parse_inherited_sandbox(sandbox)?;
            let workdir = workdir.map(std::path::PathBuf::from);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let task_id = match runtime.block_on(coordinator.spawn_application_child(
                &session_id,
                &parent_task_id,
                &capability,
                objective,
                workspace_mode,
                model,
                workdir,
                thread_id,
                inherited_sandbox,
                fork_token,
                session_turn.as_ref(),
            )) {
                Ok(task_id) => task_id,
                Err(error) => return Ok(session_rejection(error)),
            };
            Ok(IpcResponse::TaskSpawned {
                task_id: task_id.to_string(),
            })
        }
        IpcRequest::BeginForkUpload {
            session_id,
            caller_task_id,
            capability,
            total_bytes,
            session_turn,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            if let Err(error) =
                coordinator.admit_session_write(&caller_task_id, session_turn.as_ref())
            {
                return Ok(session_rejection(error));
            }
            // Authorization reaches the supervisor (root capability, or the
            // caller's own worker capability when a subagent forks itself), so
            // it must run on a runtime the same way the spawn arms do.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let fork_token = runtime.block_on(coordinator.begin_fork_upload(
                &session_id,
                &caller_task_id,
                &capability,
                total_bytes,
            ))?;
            Ok(IpcResponse::ForkUploadStarted { fork_token })
        }
        IpcRequest::AppendForkChunk {
            fork_token,
            seq,
            data,
        } => {
            let received = coordinator.append_fork_chunk(&fork_token, seq, &data)?;
            Ok(IpcResponse::ForkChunkAccepted { received })
        }
        IpcRequest::AbortForkUpload { fork_token } => {
            coordinator.abort_fork_upload(&fork_token)?;
            Ok(IpcResponse::ForkUploadAborted)
        }
        IpcRequest::StartWorker {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.start_worker(&session_id, &task_id))?;
            Ok(IpcResponse::TaskStarted)
        }
        IpcRequest::CreateAutonomousSession { objective, workdir } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let created = runtime.block_on(
                coordinator.create_autonomous_session(objective, std::path::PathBuf::from(workdir)),
            )?;
            Ok(IpcResponse::AutonomousSessionCreated {
                session_id: created.session_id.to_string(),
                root_task_id: created.root_task_id.to_string(),
            })
        }
        IpcRequest::CancelThreadTasks {
            session_id,
            thread_id,
            force,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            // One handler, one decision: the count and the cancellation are taken
            // back to back, so a child cannot slip in between "may I?" and the
            // cancel that follows a `force`.
            let active = coordinator.active_thread_children(&thread_id)?;
            if !force && !active.is_empty() {
                return Ok(IpcResponse::ThreadHasActiveChildren {
                    count: active.len(),
                });
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let cancelled = runtime
                .block_on(coordinator.cancel_thread_tasks(&session_id, &thread_id))?
                .into_iter()
                .map(|task| task.to_string())
                .collect();
            Ok(IpcResponse::ThreadTasksCancelled {
                task_ids: cancelled,
            })
        }
        IpcRequest::CancelTask { .. } => Ok(IpcResponse::Error {
            code: IpcErrorCode::ConfirmationRequired,
            message: None,
        }),
        IpcRequest::PreviewCancel { .. } | IpcRequest::ConfirmCancel { .. } => {
            Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "confirmation requests require daemon state",
            )))
        }
        IpcRequest::RetryTask {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.retry_task(&session_id, &task_id))?;
            Ok(IpcResponse::TaskRetried)
        }
        IpcRequest::PauseTask {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.pause_task(&session_id, &task_id))?;
            Ok(IpcResponse::TaskPaused)
        }
        IpcRequest::ResumeTask {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.resume_task(&session_id, &task_id))?;
            Ok(IpcResponse::TaskResumed)
        }
        IpcRequest::ResolvePermission {
            request_id,
            decision,
        } => {
            let request_id = parse_id::<PermissionRequestId>(&request_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.resolve_permission(&request_id, decision.into()))?;
            Ok(IpcResponse::PermissionResolved)
        }
        IpcRequest::SendMessage {
            session_id,
            sender_task_id,
            worker_capability,
            recipient_task_id,
            message,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let sender_task_id = parse_id::<TaskId>(&sender_task_id)?;
            let recipient_task_id = parse_id::<TaskId>(&recipient_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.send_message(
                &session_id,
                &sender_task_id,
                &worker_capability,
                recipient_task_id,
                message,
            ))?;
            Ok(IpcResponse::MessageQueued)
        }
        IpcRequest::SendApplicationMessage {
            session_id,
            sender_task_id,
            capability,
            recipient_task_id,
            message,
            session_turn,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let sender_task_id = parse_id::<TaskId>(&sender_task_id)?;
            if let Err(error) =
                coordinator.admit_session_write(&sender_task_id, session_turn.as_ref())
            {
                return Ok(session_rejection(error));
            }
            let recipient_task_id = parse_id::<TaskId>(&recipient_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            if let Err(error) = runtime.block_on(coordinator.send_application_message(
                &session_id,
                &sender_task_id,
                &capability,
                recipient_task_id,
                message,
                session_turn.as_ref(),
            )) {
                return Ok(session_rejection(error));
            }
            Ok(IpcResponse::MessageQueued)
        }
        IpcRequest::SendUserMessage { task_id, message } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.send_user_override(&task_id, message))?;
            Ok(IpcResponse::MessageQueued)
        }
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id,
            capability,
            mode,
            timeout_ms,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            let mode = match mode.as_str() {
                "one" | "any" => yi_agent_core::subagent::supervisor::WaitMode::Any,
                "all" => yi_agent_core::subagent::supervisor::WaitMode::All,
                _ => {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "mode must be one, any, or all",
                    )));
                }
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let outcome = if let Some(timeout_ms) = timeout_ms {
                runtime.block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(timeout_ms),
                        coordinator.wait_for_children_authorized(
                            &session_id,
                            &caller_task_id,
                            &capability,
                            mode,
                        ),
                    )
                    .await
                })
            } else {
                Ok(runtime.block_on(coordinator.wait_for_children_authorized(
                    &session_id,
                    &caller_task_id,
                    &capability,
                    mode,
                )))
            };
            let (status, children, reports) = match outcome {
                Err(_) => {
                    // Same runtime as the wait itself: the snapshot needs the
                    // supervisor guard, and the wait may have just released it.
                    let (children, reports) = runtime.block_on(
                        coordinator.child_completion_snapshot(&session_id, &caller_task_id),
                    )?;
                    (
                        "timeout".into(),
                        children
                            .into_iter()
                            .map(|child| child.to_string())
                            .collect(),
                        reports
                            .into_iter()
                            .map(|report| IpcCompletedChildReport {
                                task_id: report.task_id.to_string(),
                                state: report.state,
                                report: report.report,
                                delivery: report.delivery,
                            })
                            .collect(),
                    )
                }
                Ok(outcome) => match outcome? {
                    yi_agent_core::subagent::supervisor::WaitOutcome::NeedsAttention {
                        reports,
                    } => (
                        "needs_attention".into(),
                        Vec::new(),
                        reports
                            .into_iter()
                            .map(|report| IpcCompletedChildReport {
                                task_id: report.task_id.to_string(),
                                state: report.state,
                                report: report.report,
                                delivery: report.delivery,
                            })
                            .collect(),
                    ),
                    yi_agent_core::subagent::supervisor::WaitOutcome::Completed {
                        children,
                        reports,
                    } => (
                        "completed".into(),
                        children
                            .into_iter()
                            .map(|child| child.to_string())
                            .collect(),
                        reports
                            .into_iter()
                            .map(|report| IpcCompletedChildReport {
                                task_id: report.task_id.to_string(),
                                state: report.state,
                                report: report.report,
                                delivery: report.delivery,
                            })
                            .collect(),
                    ),
                },
            };
            Ok(IpcResponse::WaitCompleted {
                status,
                children,
                reports,
            })
        }
        IpcRequest::InspectTask { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let detail = context.with_repository(|repository| {
                repository.task_detail(&task_id).map_err(IpcError::from)
            })?;
            Ok(IpcResponse::TaskDetail(IpcTaskDetail {
                task_id: detail.task_id,
                session_id: detail.session_id,
                parent_task_id: detail.parent_task_id,
                depth: detail.depth,
                state: detail.state,
                delivery_json: detail.delivery_json,
                terminal_json: detail.terminal_json,
            }))
        }
        IpcRequest::InspectChild {
            session_id,
            caller_task_id,
            capability,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let detail = runtime.block_on(coordinator.inspect_child_authorized(
                &session_id,
                &caller_task_id,
                &capability,
                &task_id,
            ))?;
            Ok(IpcResponse::TaskDetail(IpcTaskDetail {
                task_id: detail.task_id,
                session_id: detail.session_id,
                parent_task_id: detail.parent_task_id,
                depth: detail.depth,
                state: detail.state,
                delivery_json: detail.delivery_json,
                terminal_json: detail.terminal_json,
            }))
        }
        IpcRequest::CancelChild {
            session_id,
            caller_task_id,
            capability,
            task_id,
            recursive,
            session_turn,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            if let Err(error) =
                coordinator.admit_session_write(&caller_task_id, session_turn.as_ref())
            {
                return Ok(session_rejection(error));
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.cancel_child_authorized(
                &session_id,
                &caller_task_id,
                &capability,
                &task_id,
                recursive,
            ))?;
            Ok(IpcResponse::TaskCancelled)
        }
        IpcRequest::ListTaskSummaries {
            session_id,
            active_only,
        } => {
            let session_id = session_id
                .as_deref()
                .map(parse_id::<RootSessionId>)
                .transpose()?;
            let tasks = context
                .with_repository(|repository| {
                    repository
                        .task_summaries(session_id.as_ref(), active_only)
                        .map_err(IpcError::from)
                })?
                .into_iter()
                .map(|task| IpcTaskSummary {
                    task_id: task.task_id,
                    state: task.state,
                    is_root: task.is_root,
                    parent_task_id: task.parent_task_id,
                    thread_id: task.thread_id,
                    usage: task.usage,
                    role: task.role,
                    container_activity: task.container_activity,
                    historical_execution_state: task.historical_execution_state,
                })
                .collect();
            Ok(IpcResponse::TaskSummaries { tasks })
        }
        IpcRequest::ReadTaskEvents {
            task_id,
            after_event_id,
        } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let events = context
                .with_repository(|repository| {
                    repository
                        .event_records_for_task_after(&task_id, after_event_id.unwrap_or(0))
                        .map_err(IpcError::from)
                })?
                .into_iter()
                .map(ipc_event)
                .collect();
            Ok(IpcResponse::TaskEvents { events })
        }
        IpcRequest::ReadTaskMailbox { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let messages = context
                .with_repository(|repository| {
                    repository
                        .mailbox_messages_for_task(&task_id)
                        .map_err(IpcError::from)
                })?
                .into_iter()
                .map(|message| IpcMailboxMessage {
                    message_id: message.message_id,
                    recipient_task_id: message.recipient_task_id.to_string(),
                    sender_task_id: message.sender_task_id.map(|task_id| task_id.to_string()),
                    kind: message.kind,
                    priority: message.priority,
                    payload_json: message.payload_json,
                    delivered_at: message.delivered_at,
                    created_at: message.created_at,
                })
                .collect();
            Ok(IpcResponse::TaskMailbox { messages })
        }
        IpcRequest::ReadTaskDiff { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let detail = context.with_repository(|repository| {
                repository.task_detail(&task_id).map_err(IpcError::from)
            })?;
            let diff = coordinator.delivery_diff(&task_id).ok().flatten();
            Ok(IpcResponse::TaskDiff {
                task_id: detail.task_id,
                delivery_json: detail.delivery_json,
                diff,
            })
        }
        IpcRequest::ReadTaskTrace { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let rows: Vec<IpcTraceRow> = context
                .with_repository(|repository| {
                    repository.trace_after(&task_id, 0).map_err(IpcError::from)
                })?
                .into_iter()
                .map(ipc_trace_row)
                .collect();
            // The cursor is the last row actually returned, so it can never
            // claim a row the caller did not receive.
            let high_water_id = rows.last().map(|row| row.event_id).unwrap_or(0);
            Ok(IpcResponse::TaskTrace {
                task_id: task_id.to_string(),
                high_water_id,
                rows,
            })
        }
        IpcRequest::SubscribeEvents {
            after_event_id,
            filters,
        } => {
            let snapshot = context.with_repository(|repository| {
                repository
                    .subscription_snapshot(after_event_id)
                    .map_err(IpcError::from)
            })?;
            let tasks = snapshot
                .tasks
                .into_iter()
                .map(|task| IpcTask {
                    task_id: task.task_id,
                    state: task.state,
                })
                .collect();
            let events = snapshot
                .events
                .into_iter()
                .map(ipc_event)
                .filter(|event| filters.matches(event))
                .collect();
            Ok(IpcResponse::Subscription(SubscriptionSnapshot {
                high_water_event_id: snapshot.high_water_event_id,
                tasks,
                events,
            }))
        }
        // Trace subscriptions stream their rows on the connection itself; this
        // request/response path cannot represent a stream, so it is answered
        // like any other unserviceable command rather than half-served.
        IpcRequest::SubscribeTrace { .. } => Ok(IpcResponse::Error {
            code: IpcErrorCode::Validation,
            message: Some("trace subscriptions are served on their own connection".into()),
        }),
    }
}

pub(crate) struct UnavailableWorkerFactory;

impl AgentWorkerFactory for UnavailableWorkerFactory {
    fn is_available(&self) -> bool {
        false
    }

    fn start(
        &self,
        _request: WorkerStart,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async {
            Err(WorkerError::Startup(
                "no application worker factory registered".into(),
            ))
        })
    }
}

#[cfg(test)]
mod error_logging_tests {
    use super::*;

    /// The daemon used to answer every failed request with a bare `internal`
    /// and log nothing, so a wedged runtime -- one whose per-request repository
    /// connection fails before it can read anything -- left no trace at all.
    /// The field symptom was a client that could only ever print
    /// `daemon rejected the runtime attachment: internal`, with nothing to
    /// diagnose from.
    #[test]
    fn a_request_failure_is_logged_with_its_cause() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(std::sync::Arc::clone(&captured));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let error = IpcError::Repository(crate::repository::RepositoryError::TaskNotFound {
            task: "task-9f2c".into(),
        });
        log_request_failure("client-7", &error);

        let logged = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("client-7"),
            "the log must identify the request: {logged}"
        );
        assert!(
            logged.contains("task-9f2c"),
            "the log must carry the real cause, not just `internal`: {logged}"
        );
    }

    #[derive(Clone)]
    struct CapturedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod request_context_tests {
    use super::*;

    #[test]
    fn should_reconcile_only_outside_the_window() {
        let window = Duration::from_millis(5);
        let base = Instant::now();
        // 从未对账过：必须跑。
        assert!(should_reconcile(None, base, window));
        // 刚跑过：窗口内跳过。
        assert!(!should_reconcile(
            Some(base),
            base + Duration::from_millis(1),
            window
        ));
        // 窗口已过：再跑一次。
        assert!(should_reconcile(
            Some(base),
            base + Duration::from_millis(6),
            window
        ));
    }

    #[test]
    fn a_request_context_reuses_one_repository() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("runtime.sqlite");
        let context = RequestContext::new(dir.path(), &database_path).expect("context builds");
        // 同一个连接可重复使用；两次读取给出同一个 schema 版本。
        let first = context
            .with_repository(|repository| repository.schema_version().map_err(IpcError::from))
            .expect("first read");
        let second = context
            .with_repository(|repository| repository.schema_version().map_err(IpcError::from))
            .expect("second read");
        assert_eq!(first, second);
    }

    /// 回归测试：不用仓库的 arm 不得被用仓库的 arm 挡住。
    ///
    /// 每条连接一个 handler 线程，请求是并发的。曾经的写法在 `match` 之前就
    /// `context.repository.lock()` 并跨整个 `match` 持有；那样一个仍在等待的
    /// `WaitAgent`（`timeout_ms: None` 时永不返回）会把每个 `Status` 轮询都挡在
    /// 锁后——正是这次要修掉的「`pwd && ls` 都慢」。这里用一个在 sqlite 读期间
    /// 故意睡 300ms 的持有者模拟慢 arm：`Stop` 根本不碰仓库，正确的按 arm 借还
    /// 下它会立刻返回；跨 arm 持锁的写法则会让它排队等满那 300ms。
    #[test]
    fn an_arm_that_does_not_use_the_repository_is_not_blocked_by_one_that_does() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("runtime.sqlite");
        let factory: Arc<dyn AgentWorkerFactory> = Arc::new(UnavailableWorkerFactory);
        let coordinator =
            Arc::new(RuntimeCoordinator::open(&database_path, factory).expect("coordinator opens"));
        let context =
            Arc::new(RequestContext::new(dir.path(), &database_path).expect("context builds"));

        let slow_context = Arc::clone(&context);
        let slow = thread::spawn(move || {
            slow_context
                .with_repository(|repository| {
                    thread::sleep(Duration::from_millis(300));
                    repository.latest_event_id().map_err(IpcError::from)
                })
                .expect("slow read");
        });
        // 与该持有者争用同一条连接。
        thread::sleep(Duration::from_millis(50));

        // `Stop` 不碰仓库：它不该被那个持有者挡住。
        let started = Instant::now();
        let response = respond(&context, &coordinator, IpcRequest::Stop).expect("stop answers");
        let elapsed = started.elapsed();
        slow.join().unwrap();

        assert!(matches!(response, IpcResponse::Stopping));
        assert!(
            elapsed < Duration::from_millis(150),
            "an arm that never touches the repository must not wait behind one that does; waited {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod watchdog_tick_tests {
    //! spec §3.3 的「驱动路径」回归：从 daemon 线程每分调用的那条路径证明
    //! 越限 attempt 真的会被终止。
    //!
    //! 缺陷形状是「看门狗从不运行」：`evaluate_watchdogs` 判定逻辑本身没问题，
    //! 只是没有任何生产调用者，于是越限的任务永远活着。直调
    //! `evaluate_watchdogs` 的测试发现不了这件事——把接线拆掉它照样绿。所以这
    //! 里驱动的是 `Daemon::run_minute_maintenance`（Task 3 落地的生产调用者）。
    //!
    //! 终止断言要求任务所在会话在 daemon 进程内已 hydrate：`record_watchdog_terminal`
    //! 通过 `supervisor(session)` 取 supervisor，未 hydrate 的会话拿不到，Task 1
    //! 让 `evaluate_watchdogs` 恰恰隔离了这个错误（warn 后继续）。所以「终止」
    //! 用 hydrated 会话验，「隔离」用 hydrated + unhydrated 并置验。

    use super::*;
    use crate::repository::{PersistedEvent, RuntimeEvent};
    use crate::schedule::{WatchdogLimits, WatchdogUsage};
    use rusqlite::Connection;

    /// idle 超过 1 秒即判 `Stalled`。`evaluate_watchdogs` 内部用真实
    /// `Utc::now()`，所以只要 `last_meaningful_at` 足够久远就一定越限。
    fn over_budget_limits() -> WatchdogLimits {
        WatchdogLimits {
            max_turns: None,
            max_wall_time_secs: None,
            max_idle_time_secs: Some(1),
            ..WatchdogLimits::default()
        }
    }

    fn ancient_start() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-09T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn active_attempt_id(database: &Path, task: &TaskId) -> yi_agent_core::AttemptId {
        Connection::open(database)
            .unwrap()
            .query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                [task.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap()
    }

    fn recorded_watchdog_terminal(events: &[PersistedEvent], task: &TaskId) -> bool {
        events.iter().any(|event| {
            event.task_id == *task
                && matches!(
                    event.event,
                    RuntimeEvent::TaskStalled
                        | RuntimeEvent::TaskTimedOut
                        | RuntimeEvent::TaskBudgetExhausted
                )
        })
    }

    /// 与 daemon 线程完全相同的调用形状：current-thread runtime + block_on。
    fn drive_one_minute_maintenance(daemon: &Daemon, now: DateTime<Local>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("minute maintenance runtime must initialize");
        Daemon::run_minute_maintenance(&runtime, &daemon.coordinator, now);
    }

    fn start_daemon(database: &Path) -> Daemon {
        let runtime_dir = database
            .parent()
            .expect("database lives in a directory")
            .join("runtime");
        Daemon::start_with_factory(runtime_dir, database, Arc::new(UnavailableWorkerFactory))
            .expect("daemon starts")
    }

    /// 越限 attempt 会被一次分钟维护 tick 终止，并释放它的租约。
    ///
    /// 回归缺陷：看门狗没有生产调用者。把 `run_minute_maintenance` 里的
    /// `evaluate_watchdogs` 调用去掉，本测试失败（状态停在 `queued`）。
    #[test]
    fn a_minute_maintenance_tick_terminates_an_over_budget_attempt() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon = start_daemon(&database);

        // hydrate：create_session 把 supervisor 放进进程内的 map，只有这类会话
        // 的 attempt 才可能被判定终止。
        let session = daemon.coordinator.create_session().unwrap();
        let root = daemon.coordinator.root_task_id(&session).unwrap();
        let attempt = active_attempt_id(&database, &root);
        let started = ancient_start();
        {
            let mut repository = RuntimeRepository::open(&database).unwrap();
            repository
                .save_attempt_watchdog_snapshot(
                    &root,
                    &attempt,
                    &over_budget_limits(),
                    &WatchdogUsage::default(),
                    None,
                    started,
                    None,
                )
                .unwrap();
            // 一条真实活跃租约，让「释放租约」这条断言真的被走到。
            repository
                .record_recovery_context(
                    &root,
                    &attempt,
                    "workspace:watchdog",
                    "worktree:watchdog",
                    r#"{"git_head":"test"}"#,
                    r#"{"state":"available"}"#,
                )
                .unwrap();
        }
        assert!(
            RuntimeRepository::open(&database)
                .unwrap()
                .has_active_lease_prefix(&root, "")
                .unwrap(),
            "precondition: the attempt holds an active lease"
        );

        drive_one_minute_maintenance(
            &daemon,
            (started + chrono::Duration::seconds(10)).with_timezone(&Local),
        );

        let repository = RuntimeRepository::open(&database).unwrap();
        let state = repository.task_state(&root).unwrap();
        assert!(
            matches!(state.as_str(), "stalled" | "timed_out" | "budget_exhausted"),
            "over-budget attempt must be terminated by the tick, got {state}"
        );
        let events = repository.event_records_after(0).unwrap();
        assert!(
            recorded_watchdog_terminal(&events, &root),
            "a watchdog terminal event must be recorded"
        );
        assert!(
            !repository.has_active_lease_prefix(&root, "").unwrap(),
            "the attempt's leases must be released"
        );
    }

    /// 一个未 hydrate 的会话不得阻断整轮扫描。
    ///
    /// 这是 Task 1 契约的驱动路径形式：tick 跑完（`Ok`）、未 hydrate 的那条被
    /// **跳过而非判定**（状态不变、无终态事件），同时 hydrate 的兄弟仍被终止。
    ///
    /// 覆盖的是**坏记录先被扫描到**这一顺序（见下方「先写坏记录」的说明），
    /// 也正因为如此它才能区分「逐条隔离」是否还在。顺序无关的保证由 Task 1 的
    /// 直接测试 `an_unhydrated_session_does_not_fail_the_whole_watchdog_sweep`
    /// 负责（那里不依赖扫描顺序）。
    #[test]
    fn one_unhydrated_session_does_not_stop_the_tick_from_terminating_another() {
        let directory = tempfile::TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon = start_daemon(&database);

        let started = ancient_start();

        // 未 hydrate 的兄弟：手工造一条 queued 越限快照，其会话不在 map 里。
        //
        // 必须**先于** `create_session()` 写入：`create_session` 会在
        // `insert_attempt` 里替它的根 attempt 插一行 `attempt_watchdogs`，而
        // `active_attempt_watchdogs()` 没有 `ORDER BY`，扫描顺序即 rowid 顺序。
        // 先写坏记录，才能保证这一轮扫描**先撞上**那条未 hydrate 的记录——只有
        // 这个顺序才真正区分「逐条隔离」是否还在：隔离被移除时，扫描在第 1 条
        // （坏记录）就中止，后面已 hydrate 的兄弟永远等不到判定，本测试即失败。
        let unhydrated_session = RootSessionId::new();
        let unhydrated_task = TaskId::new();
        let unhydrated_attempt = yi_agent_core::AttemptId::new();
        {
            let mut repository = RuntimeRepository::open(&database).unwrap();
            repository
                .create_task_with_attempt(
                    &unhydrated_task,
                    &unhydrated_session,
                    &unhydrated_attempt,
                    1,
                    "queued",
                )
                .unwrap();
            repository
                .save_attempt_watchdog_snapshot(
                    &unhydrated_task,
                    &unhydrated_attempt,
                    &over_budget_limits(),
                    &WatchdogUsage::default(),
                    None,
                    started,
                    None,
                )
                .unwrap();
        }

        // 再 hydrate 一个会话，并给它写一条同样越限的快照。
        let session = daemon.coordinator.create_session().unwrap();
        let hydrated_root = daemon.coordinator.root_task_id(&session).unwrap();
        let hydrated_attempt = active_attempt_id(&database, &hydrated_root);
        {
            let mut repository = RuntimeRepository::open(&database).unwrap();
            repository
                .save_attempt_watchdog_snapshot(
                    &hydrated_root,
                    &hydrated_attempt,
                    &over_budget_limits(),
                    &WatchdogUsage::default(),
                    None,
                    started,
                    None,
                )
                .unwrap();
        }

        // 前提：两条越限快照都真的进了本轮扫描——否则「未 hydrate 被跳过」这条
        // 断言就是空的（它可能压根没被看过）。
        assert_eq!(
            RuntimeRepository::open(&database)
                .unwrap()
                .active_attempt_watchdogs()
                .unwrap()
                .len(),
            2,
            "precondition: both over-budget attempts must be in the sweep"
        );

        drive_one_minute_maintenance(
            &daemon,
            (started + chrono::Duration::seconds(10)).with_timezone(&Local),
        );

        let repository = RuntimeRepository::open(&database).unwrap();
        let hydrated_state = repository.task_state(&hydrated_root).unwrap();
        assert!(
            matches!(
                hydrated_state.as_str(),
                "stalled" | "timed_out" | "budget_exhausted"
            ),
            "the hydrated sibling must still be terminated: the sweep has to survive the \
             unhydrated entry, got {hydrated_state}"
        );
        let events = repository.event_records_after(0).unwrap();
        assert!(
            recorded_watchdog_terminal(&events, &hydrated_root),
            "the hydrated sibling must record a watchdog terminal event"
        );
        assert_eq!(
            repository.task_state(&unhydrated_task).unwrap(),
            "queued",
            "an unhydrated session's attempt must be skipped, not judged"
        );
        assert!(
            !recorded_watchdog_terminal(&events, &unhydrated_task),
            "no watchdog terminal event may be recorded for an unhydrated session"
        );
    }
}

#[cfg(test)]
mod schema_preflight_tests {
    use super::*;

    /// A future-schema database must be refused by `Daemon::start` *before*
    /// recovery, orphan reclamation or coordinator hydration write to it.
    ///
    /// This is the ordering that matters: `RuntimeRepository::open` happily opens
    /// a newer database (its `migrate` gate now refuses, but only after the WAL
    /// pragma), and `recover_inflight_tasks` would rewrite rows under a schema this
    /// build does not understand. Asserting on the error alone would not prove
    /// ordering, so the test also proves the file is byte-identical afterwards.
    #[test]
    fn a_future_schema_database_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().join("runtime");
        let database = dir.path().join("runtime.sqlite");

        // A database this build can create, then pushed one version past the top.
        {
            let repository = RuntimeRepository::open(&database).unwrap();
            drop(repository);
        }
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .execute(
                    "INSERT INTO schema_migrations (version) VALUES (?1)",
                    [crate::repository::LATEST_SCHEMA_VERSION + 1],
                )
                .unwrap();
        }
        let before = std::fs::read(&database).unwrap();

        let error = match Daemon::start(&runtime_dir, &database) {
            Ok(_) => panic!("a newer schema must not start a daemon"),
            Err(error) => error,
        };
        assert!(
            matches!(error, IpcError::SchemaIncompatible(_)),
            "{error:?}"
        );
        // The category is what a client sees: `InvalidState` reads as "upgrade or
        // stop the other build", while `Internal` would send someone hunting for a
        // crashed daemon that is running fine.
        assert_eq!(
            super::error::ipc_error_code(&error),
            IpcErrorCode::InvalidState,
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(&database).unwrap(),
            before,
            "the daemon wrote to a database it cannot understand"
        );
        // The pre-flight also runs before the socket is bound, so a refused start
        // leaves nothing for the next client to mistake for a live daemon.
        assert!(
            !socket_path_for(&runtime_dir).unwrap().exists(),
            "a refused start must not leave a socket behind"
        );
    }
}
