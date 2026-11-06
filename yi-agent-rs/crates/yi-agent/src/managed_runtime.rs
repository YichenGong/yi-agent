//! The managed runtime's private entry points.
//!
//! Everything here is reached through an *inherited descriptor*, never through a
//! public socket, and every role refuses to run without one. That refusal is the
//! security boundary: an executor that accepted "run this task" from any local
//! process would be a remote code execution hole, and a daemon that accepted its
//! configuration from a public socket could be pointed at different credentials.
//!
//! This module deliberately does no discovery: no scanning for a runtime
//! directory, no reading another project's state, no PATH lookup. The descriptor
//! and the local configuration are the whole input.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;

use yi_agent_subagent::executor::serve::{EXECUTOR_FD_ENV, serve_executor};

/// Runs one client's executor until the daemon closes its control channel.
///
/// `owner` comes from the daemon's spawn command line; the descriptor comes from
/// the same spawn. Neither is derived from anything a process could talk its way
/// into: a wrong owner is refused by the executor itself against the descriptor's
/// identity, and a missing descriptor is a hard error here.
pub fn run_executor(
    owner: String,
    config_path: Option<PathBuf>,
    invocation: yi_agent_runtime::config::RuntimeConfig,
) -> anyhow::Result<()> {
    let control = inherited_descriptor()?;
    // The factory is rebuilt *locally* from this machine's configuration. The
    // control channel carries work, never authority to pick a provider, a model,
    // or a credential.
    let models_path = config_path.unwrap_or_else(yi_agent_runtime::models::models_path);
    let runtime_socket = yi_agent_subagent::attach::project_runtime_directory(&invocation.workdir);
    let factory =
        yi_agent_subagent::attach::worker_factory(&invocation, runtime_socket, &models_path)?;
    // A managed executor always offers the gate: the daemon above it is the only
    // place a machine-wide limit can be enforced.
    serve_executor(control, owner, factory, true)?;
    Ok(())
}

/// Takes the inherited control descriptor from the environment.
///
/// The descriptor number is passed explicitly so the launcher does not have to
/// assume one, and it is removed from the environment immediately: a child this
/// process starts must not inherit a way to talk to the daemon.
fn inherited_descriptor() -> io::Result<OwnedFd> {
    let raw = std::env::var(EXECUTOR_FD_ENV).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{EXECUTOR_FD_ENV} is not set; an executor must be started by the runtime, \
                 never by hand"
            ),
        )
    })?;
    // SAFETY: no other thread has been started yet -- this runs at the top of a
    // fresh process -- so mutating the environment here cannot race a reader.
    unsafe { std::env::remove_var(EXECUTOR_FD_ENV) };
    let raw: i32 = raw
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad executor descriptor"))?;
    if raw <= 2 {
        // Refusing 0/1/2 is not ceremony: an executor whose control channel is a
        // terminal would read keystrokes as protocol frames.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the executor descriptor must not be a standard stream",
        ));
    }
    // Safety: the launcher dup'd this descriptor into the child and declared it in
    // the environment; taking ownership here is what closes it on exit.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Takes the guardian's bootstrap descriptor, refusing before any side effect.
///
/// Split out so the refusal happens *before* the scope is resolved: a hand-started
/// guardian must not create a control socket, let alone take ownership of process
/// groups, and reporting "no scope key" first would tell a user to set one instead
/// of telling them this role is not theirs to run.
pub fn take_guardian_bootstrap() -> anyhow::Result<std::os::fd::OwnedFd> {
    Ok(yi_agent_supervisors::lifecycle::roles::inherited_bootstrap(
        yi_agent_supervisors::lifecycle::roles::ManagedRole::Guardian,
    )?)
}

/// Serves one runtime as its guardian.
///
/// The scope comes from the environment the launcher set, never from an argument:
/// a guardian that parsed a path from its command line could be pointed at another
/// project's process journal and would then adopt groups it does not own.
pub fn run_guardian(
    scope: yi_agent_lifecycle::types::RuntimeScope,
    launcher: yi_agent_supervisors::lifecycle::guardian::LauncherLifeline,
) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let artifact = crate::build_info::artifact_identity(executable.clone());
    // The guardian starts the daemon from *this* executable, which is what makes the
    // pinned selection meaningful: the bytes the guardian was authorised with are the
    // bytes the daemon runs, and a bundle replaced afterwards cannot change them.
    yi_agent_supervisors::lifecycle::guardian::serve_with_lifeline(
        scope,
        artifact,
        || false,
        std::sync::Arc::new(yi_agent_supervisors::lifecycle::preview::NoCoordinator),
        Some(&executable),
        Some(launcher),
    )
    .map_err(|error| anyhow::anyhow!("the managed guardian stopped: {error}"))
}

/// Refuses a hello that does not name the runtime this process was told to serve.
///
/// The comparison is what connects "a launcher authorized me" to "it authorized me
/// *for this runtime*". Without it a misconfigured launcher could leave this
/// process serving one project's database while the guardian believes it authorized
/// another -- and every later authority check would be made against the wrong scope.
///
/// An absent key is a refusal, not "unknown, carry on": a managed role is always
/// told which runtime it serves (see [`yi_agent_supervisors::lifecycle::roles`]),
/// so an empty observed scope means the frame is not a valid hello.
pub(crate) fn validate_managed_hello(expected: &str, observed: &str) -> io::Result<()> {
    if observed.is_empty() || expected != observed {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the guardian authorized a different runtime scope",
        ));
    }
    Ok(())
}

/// Reads the guardian's opening frame, if one arrives in time.
///
/// `Ok(None)` means "no greeting arrived". For the *managed* role that is a startup
/// refusal (see [`read_required_lifeline_hello`]): a launcher that assigned a
/// private role but never proved which runtime it was assigning proves nothing, and
/// treating silence as "unmanaged" would let a broken launch keep serving. The
/// public `daemon serve` keeps its own unmanaged semantics and never calls this.
///
/// The guardian writes this frame right after a successful spawn, before it waits
/// for the socket. That ordering is load-bearing: this daemon reads the greeting
/// before it binds, so a guardian that waited for `Status` first would deadlock
/// against the daemon it just started.
fn read_lifeline_hello(
    lifeline: &std::os::fd::OwnedFd,
) -> io::Result<Option<yi_agent_lifecycle::authorization::LifelineHello>> {
    use std::io::Read;
    let mut probe = [0u8; 1];
    // A one-byte peek with a bounded wait: the frame is small and the guardian is a
    // local process, so hitting the budget means nothing is coming.
    let mut poll = libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(lifeline),
        events: libc::POLLIN,
        revents: 0,
    };
    let budget = yi_agent_lifecycle::authorization::LIFELINE_READ_BUDGET
        .as_millis()
        .min(i32::MAX as u128) as i32;
    // SAFETY: `poll` only inspects the descriptor's readiness.
    let ready = unsafe { libc::poll(&mut poll, 1, budget) };
    if ready == 0 {
        return Ok(None);
    }
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is an open socket; `Read` on it does not change
    // ownership. `poll` said a byte is available, so this cannot block.
    let mut stream = unsafe {
        use std::os::fd::FromRawFd;
        // Borrow, do not take: the `OwnedFd` above stays the owner.
        std::mem::ManuallyDrop::new(std::os::unix::net::UnixStream::from_raw_fd(
            std::os::fd::AsRawFd::as_raw_fd(lifeline),
        ))
    };
    stream.set_read_timeout(Some(
        yi_agent_lifecycle::authorization::LIFELINE_READ_BUDGET,
    ))?;
    stream.read_exact(&mut probe)?;
    // The frame header is four bytes and the read above consumed one, so the frame
    // reader is given the header back by reading it directly and prepending.
    let mut header = [0u8; 3];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes([probe[0], header[0], header[1], header[2]]) as usize;
    if length == 0 || length + 4 > yi_agent_lifecycle::MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("lifeline frame length {length} is out of range"),
        ));
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    // `decode` takes a *whole* frame, header included. Handing it the body alone
    // made it read the first four JSON bytes as a length and refuse the frame as
    // oversized -- a bug the tests never saw only because the greeting was never
    // actually sent before the daemon started serving.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(length as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    let hello: yi_agent_lifecycle::authorization::LifelineHello =
        yi_agent_lifecycle::codec::decode(&frame)?;
    Ok(Some(hello))
}

/// Reads the greeting a *managed* role requires, refusing when none arrives.
///
/// Split from [`read_lifeline_hello`] on purpose: the reader is also used to answer
/// "is there a greeting at all", and only the managed entry points may turn
/// silence into a startup refusal.
fn read_required_lifeline_hello(
    lifeline: &std::os::fd::OwnedFd,
) -> io::Result<yi_agent_lifecycle::authorization::LifelineHello> {
    read_lifeline_hello(lifeline)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the launcher sent no guardian authorization hello; a managed role must \
             never start without one",
        )
    })
}

/// Applies every authority update the guardian sends, until the channel closes.
///
/// The loop ending *is* the daemon's notice that its guardian is gone; the caller
/// keeps it running for the daemon's whole life for that reason. A malformed or
/// refused frame is logged and skipped rather than fatal: one bad binding must not
/// take down a serving daemon, and the refusal is the guardian's bug to fix.
fn pump_lifeline_authority(
    lifeline: std::os::fd::OwnedFd,
    coordinator: std::sync::Arc<yi_agent_store::runtime::RuntimeCoordinator>,
    demand: std::sync::Arc<yi_agent_runtime::supervise::WantedPlugins>,
    generation: String,
) {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: this thread is the sole owner of `lifeline` now, and the stream takes
    // over exactly that descriptor.
    let mut stream = std::mem::ManuallyDrop::new(unsafe {
        std::os::unix::net::UnixStream::from_raw_fd(lifeline.as_raw_fd())
    });
    let mut sequence = 0u64;
    loop {
        match yi_agent_lifecycle::codec::read_frame(&mut &mut *stream) {
            Ok(frame) => {
                // Two frame types share this channel, and they must not be confused:
                // authority decides what a client may do, demand decides what the
                // machine runs. `AuthorizationUpdate` is externally tagged, so a
                // snapshot matches no variant and falls through to the second
                // attempt; the reverse fails on required fields. Neither direction
                // guesses at the other's meaning.
                if let Ok(snapshot) = yi_agent_lifecycle::codec::decode::<
                    yi_agent_lifecycle::authorization::DemandSnapshot,
                >(&frame)
                {
                    apply_demand_snapshot(&snapshot, &generation, &demand, &mut sequence);
                    continue;
                }
                match yi_agent_lifecycle::codec::decode::<
                    yi_agent_lifecycle::authorization::AuthorizationUpdate,
                >(&frame)
                {
                    Ok(update) => {
                        if let Err(error) = coordinator.apply_authority_update(update) {
                            tracing::warn!(error = %error, "refused an authority update");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "undecodable authority update");
                    }
                }
            }
            // EOF is the guardian dying. There is nothing to report to: the process
            // is on its own from here, and the reclaimer's own leases are what stop
            // any work it still owns.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(error) => {
                tracing::warn!(error = %error, "the guardian lifeline failed");
                return;
            }
        }
    }
}

/// Applies one demand snapshot, refusing the ones that describe another world.
///
/// Two checks, both load-bearing. A snapshot for a different generation was made by
/// a guardian that is not this daemon's, so its plugin list says nothing about this
/// process. A snapshot at or below the last applied sequence is older than what is
/// already in force; applying it would resurrect a demand a later snapshot
/// withdrew, which reads as "the plugin came back by itself".
fn apply_demand_snapshot(
    snapshot: &yi_agent_lifecycle::authorization::DemandSnapshot,
    generation: &str,
    demand: &yi_agent_runtime::supervise::WantedPlugins,
    last_sequence: &mut u64,
) {
    if snapshot.daemon_instance_id != generation {
        tracing::warn!(
            snapshot = %snapshot.daemon_instance_id,
            generation,
            "ignored a demand snapshot from another daemon generation"
        );
        return;
    }
    if snapshot.sequence <= *last_sequence {
        tracing::warn!(
            snapshot = snapshot.sequence,
            applied = *last_sequence,
            "ignored a demand snapshot that is not newer than the applied one"
        );
        return;
    }
    *last_sequence = snapshot.sequence;
    demand.replace(snapshot.plugins.iter().cloned());
}

/// Serves one runtime's business daemon under its guardian.
///
/// This is the only daemon start that carries the exit guarantees. It is
/// deliberately not reachable through the public `daemon serve`: those callers are
/// unmanaged by contract, and the manual `daemon start/serve` semantics must not
/// change.
pub fn run_managed_daemon(
    invocation: yi_agent_runtime::config::RuntimeConfig,
) -> anyhow::Result<()> {
    use yi_agent_supervisors::lifecycle::roles::{
        ManagedRole, inherited_bootstrap, inherited_bridge,
    };
    let bootstrap = inherited_bootstrap(ManagedRole::Daemon)?;
    // The guardian's first frame carries the generation secret. It is read before
    // the daemon serves because authority must be verifiable from the first attach:
    // a daemon that bound its socket first would spend a window admitting calls it
    // could not check.
    //
    // A greeting that never arrives is a *refusal*, not "run unmanaged". This
    // process was started as the managed daemon role by something that held the
    // inherited descriptor; only that launcher could have chosen the scope, so
    // silence means the launch is broken or unauthorized. Falling back to unmanaged
    // would hand the exit guarantees the role exists for to a process nobody can
    // account for.
    let hello = read_required_lifeline_hello(&bootstrap).map_err(|error| {
        anyhow::anyhow!("the guardian's authorization handshake failed: {error}")
    })?;
    let secret = hello.secret().map_err(|error| anyhow::anyhow!(error))?;
    // The expected scope is the daemon's own resolved runtime directory, which the
    // guardian named in this process's environment at launch and which the guardian
    // resolved the same way. Comparing against it (rather than against a workdir
    // guessed here) is what makes a symlinked or redirected project unable to
    // produce two scopes for one runtime.
    let expected_scope = yi_agent_subagent::attach::project_runtime_directory(&invocation.workdir)
        .to_string_lossy()
        .into_owned();
    // Two independent agreements are required. The launcher's declared scope key
    // must match this process's own resolution (otherwise the two sides canonicalised
    // different paths), and the authenticated hello must name that same scope. Checking
    // only the hello would let a launcher that declared one scope in the environment
    // and another in the frame go unnoticed.
    let declared_scope = yi_agent_supervisors::lifecycle::roles::scope_key()
        .map_err(|error| anyhow::anyhow!("the launcher named no scope: {error}"))?;
    validate_managed_hello(&declared_scope, &expected_scope).map_err(|error| {
        anyhow::anyhow!(
            "the launcher named scope {declared_scope}, but {} resolves to {expected_scope}: {error}",
            invocation.workdir.display()
        )
    })?;
    validate_managed_hello(&expected_scope, &hello.scope_key).map_err(|error| {
        anyhow::anyhow!("the guardian authorized a different runtime scope: {error}")
    })?;
    // The project is checked the same way as the scope: the guardian names it in the
    // environment, and this process's own working directory is what the guardian set.
    // A disagreement means the daemon would answer for a project the guardian is not
    // supervising -- the same failure one level down.
    let declared_project = yi_agent_supervisors::lifecycle::roles::project_key()
        .map_err(|error| anyhow::anyhow!("the guardian named no project: {error}"))?;
    let observed_project = invocation.workdir.to_string_lossy().into_owned();
    if !yi_agent_supervisors::lifecycle::bringup::scope_keys_agree(
        &declared_project,
        &observed_project,
    ) {
        anyhow::bail!(
            "the guardian named project {declared_project}, but this daemon stands in {observed_project}"
        );
    }
    let ledger = yi_agent_lifecycle::authorization::AuthorizationLedger::verifier(
        hello.scope_key.clone(),
        hello.daemon_instance_id.clone(),
        secret,
    );
    // The channel stays open for the daemon's whole life: it is both the death
    // monitor and the authority feed, and closing it early would tell the guardian
    // the daemon is gone.
    let managed = Some((ledger, bootstrap));
    // The guardian's half of the foreground authority bridge rides the *same*
    // lifeline the hello arrived on. It is taken here, before the daemon starts,
    // because a daemon that bound its socket first would spend a window accepting
    // foreground frames it could not answer.
    let bridge = match inherited_bridge() {
        Ok(bridge) => bridge,
        Err(error) => {
            return Err(anyhow::anyhow!(
                "could not take the foreground authority bridge: {error}"
            ));
        }
    };
    // The instance's configuration is inherited from the process that started us,
    // not re-derived from the project cwd: a shared daemon must keep reading the
    // same `YI_AGENT_HOME` and model catalogue its launcher used. Only the workdir
    // (the *root* being served) is taken from the invocation.
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&invocation.workdir);
    let database = runtime_dir.join("runtime.sqlite");
    let factory = yi_agent_subagent::attach::worker_factory(
        &invocation,
        runtime_dir.clone(),
        &yi_agent_runtime::models::models_path(),
    )?;
    let (ledger, lifeline) = match managed {
        Some((ledger, lifeline)) => (Some(ledger), Some(lifeline)),
        None => (None, None),
    };
    let daemon = yi_agent_store::ipc::Daemon::start_with_factory_authority_enforcing_session_turns(
        &runtime_dir,
        &database,
        factory,
        // Enabled unconditionally: this is the *managed* daemon, the only entry
        // whose guardian can vouch for a root. Not reachable from an environment
        // variable, because a switch a user can flip would let a runtime classify
        // containers without an authority channel to fence them.
        yi_agent_store::foreground::ForegroundActivation::Enabled,
        ledger,
        bridge,
    )
    .map_err(|error| anyhow::anyhow!("could not start the managed daemon: {error}"))?;
    // Only now does listening for further authority make sense: the coordinator
    // exists, so a binding frame has somewhere to land. Until the daemon stops, this
    // loop also *is* the death monitor -- it returns when the guardian closes the
    // channel, which is the signal that nothing supervises this process any more.
    // The business daemon supervises its own plugin children -- that is not handed
    // to the guardian, which knows process groups but not plugin manifests. The
    // second argument is the *runtime directory*: the route table is partitioned by
    // it, and passing the socket path would publish every route into a partition
    // nobody reads.
    //
    // This is the *managed* entry, so plugins run only while a client wants them.
    // A daemon that supervised from configuration alone would start every enabled
    // plugin at launch -- before the first client could possibly ask for one -- and
    // the runtime would still carry the background load it was introduced to remove.
    let demand = std::sync::Arc::new(yi_agent_runtime::supervise::WantedPlugins::new());
    let supervisor = yi_agent_runtime::supervise::serve_managed(
        &invocation.workdir,
        &runtime_dir,
        std::sync::Arc::clone(&demand),
    );
    if let Some(lifeline) = lifeline {
        let coordinator = std::sync::Arc::clone(daemon.coordinator());
        let generation = hello.daemon_instance_id.clone();
        std::thread::Builder::new()
            .name("lifeline-authority".to_string())
            .spawn(move || pump_lifeline_authority(lifeline, coordinator, demand, generation))
            .map_err(|error| anyhow::anyhow!("could not watch the guardian lifeline: {error}"))?;
    }
    let waited = daemon
        .wait()
        .map_err(|error| anyhow::anyhow!("the managed daemon stopped: {error}"));
    // Explicit, so the plugin children are stopped before this function reports the
    // daemon gone; `Drop` would do it too, but only after the error value was built.
    drop(supervisor);
    waited
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The managed daemon must serve plugins from the daemon's runtime directory.
    ///
    /// The plugin route table is partitioned by runtime directory, so a daemon that
    /// published into a different partition (the socket path, say) would look like
    /// "the plugin is running but nothing can find it". Pinned as a source-level
    /// check because the call is what carries the guarantee and there is no value to
    /// assert on: `serve` starts a thread.
    #[test]
    fn the_managed_daemon_supervises_the_runtime_directory_not_the_socket() {
        let source = include_str!("managed_runtime.rs");
        let body = source
            .split("pub fn run_managed_daemon")
            .nth(1)
            .expect("the managed daemon entry point must exist");
        // The *managed* entry point, not the plain one: a daemon that supervised from
        // configuration alone would start every enabled plugin at launch, before any
        // client could ask for one. Matching on `supervise::serve(` would have gone on
        // passing after the switch to managed mode, checking a call site that no
        // longer carries the behaviour.
        // The whole call, not one line: the arguments are broken across lines by the
        // formatter, and a single-line check would silently assert on a prefix.
        let start = body
            .find("supervise::serve_managed(")
            .expect("the managed daemon must supervise plugin children from live demand");
        let call = &body[start
            ..body[start..]
                .find(");")
                .map(|end| start + end)
                .expect("the supervise call must be terminated")];
        assert!(
            call.contains("&runtime_dir"),
            "plugin routes are partitioned by runtime directory: {call}"
        );
        assert!(
            !call.contains("socket"),
            "passing the socket path partitions routes where nothing reads them: {call}"
        );
    }

    /// A snapshot from another generation describes a world this daemon is not in.
    #[test]
    fn a_demand_snapshot_from_another_generation_is_ignored() {
        let demand = yi_agent_runtime::supervise::WantedPlugins::new();
        let mut sequence = 0;
        apply_demand_snapshot(
            &snapshot("daemon-other", 7, &["demo"]),
            "daemon-mine",
            &demand,
            &mut sequence,
        );
        assert!(
            demand.snapshot().is_empty(),
            "another generation's plugins must not start here"
        );
        assert_eq!(
            sequence, 0,
            "an ignored snapshot must not advance the sequence"
        );
    }

    /// An old snapshot must not resurrect demand a newer one already withdrew.
    #[test]
    fn a_demand_snapshot_that_is_not_newer_is_ignored() {
        let demand = yi_agent_runtime::supervise::WantedPlugins::new();
        let mut sequence = 0;
        apply_demand_snapshot(
            &snapshot("daemon-1", 5, &["demo"]),
            "daemon-1",
            &demand,
            &mut sequence,
        );
        assert_eq!(demand.snapshot(), vec!["demo".to_string()]);

        // Same sequence, different content: older than what is in force.
        apply_demand_snapshot(
            &snapshot("daemon-1", 5, &["other"]),
            "daemon-1",
            &demand,
            &mut sequence,
        );
        assert_eq!(
            demand.snapshot(),
            vec!["demo".to_string()],
            "a replay must not change the answer"
        );

        // A genuinely newer one replaces the whole set, including removals.
        apply_demand_snapshot(
            &snapshot("daemon-1", 6, &[]),
            "daemon-1",
            &demand,
            &mut sequence,
        );
        assert!(demand.snapshot().is_empty(), "newer wins, including empty");
        assert_eq!(sequence, 6);
    }

    fn snapshot(
        generation: &str,
        sequence: u64,
        plugins: &[&str],
    ) -> yi_agent_lifecycle::authorization::DemandSnapshot {
        yi_agent_lifecycle::authorization::DemandSnapshot {
            daemon_instance_id: generation.to_string(),
            sequence,
            plugins: plugins.iter().map(|plugin| plugin.to_string()).collect(),
        }
    }

    /// Runs this test binary again in a child, invoking `probe` as the only test.
    ///
    /// Environment variables are process-global, so a test that sets one cannot
    /// avoid racing every other test in the binary -- a mutex only serialises the
    /// mutators, not the readers. A child process gets its own environment, which
    /// is the only honest way to test "this variable is absent".
    fn in_child(test: &str, env: &[(&str, &str)]) -> std::process::Output {
        let executable = std::env::current_exe().unwrap();
        let mut command = std::process::Command::new(executable);
        command
            .arg("--exact")
            .arg(test)
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env_remove(EXECUTOR_FD_ENV);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// The probe the child runs: prints the refusal kind.
    #[test]
    fn descriptor_probe() {
        if std::env::var_os("YI_AGENT_DESCRIPTOR_PROBE").is_none() {
            return;
        }
        match inherited_descriptor() {
            Ok(_) => println!("PROBE ok"),
            Err(error) => println!("PROBE kind={:?} message={error}", error.kind()),
        }
    }

    fn probe(env: &[(&str, &str)]) -> String {
        let output = in_child(
            "managed_runtime::tests::descriptor_probe",
            &[("YI_AGENT_DESCRIPTOR_PROBE", "1")]
                .into_iter()
                .chain(env.iter().copied())
                .collect::<Vec<_>>(),
        );
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            text.contains("PROBE"),
            "the child never reported: stdout={text} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        text
    }

    /// Without the inherited descriptor the role refuses, and says why.
    ///
    /// This is the only thing standing between "a private spawn" and "any local
    /// process can ask this binary to execute an agent": the descriptor is what
    /// proves the daemon started us.
    #[test]
    fn an_executor_without_an_inherited_descriptor_refuses_to_run() {
        let reported = probe(&[]);
        assert!(
            reported.contains("NotFound") && reported.contains("must be started by the runtime"),
            "expected a self-explaining refusal, got {reported}"
        );
    }

    /// A standard stream is refused: a terminal would be read as protocol frames.
    #[test]
    fn a_standard_stream_is_not_accepted_as_the_control_channel() {
        let reported = probe(&[(EXECUTOR_FD_ENV, "1")]);
        assert!(
            reported.contains("InvalidInput") && reported.contains("standard stream"),
            "expected a standard-stream refusal, got {reported}"
        );
    }

    /// A non-numeric descriptor is refused rather than guessed.
    #[test]
    fn a_non_numeric_descriptor_is_refused() {
        let reported = probe(&[(EXECUTOR_FD_ENV, "not-a-number")]);
        assert!(
            reported.contains("InvalidInput"),
            "expected a bad-descriptor refusal, got {reported}"
        );
    }

    /// A managed daemon whose launcher authorized a different scope must refuse.
    ///
    /// The hello is the only thing that connects "the guardian started me" to
    /// "the guardian started me *for this runtime*". Without the comparison a
    /// misconfigured launcher would have the daemon serve one project's database
    /// while the guardian believes it authorized another.
    #[test]
    fn mismatching_nonempty_scope_is_refused() {
        assert!(validate_managed_hello("scope-a", "scope-b").is_err());
        assert!(validate_managed_hello("scope-a", "").is_err());
        assert!(validate_managed_hello("scope-a", "scope-a").is_ok());
    }
}
