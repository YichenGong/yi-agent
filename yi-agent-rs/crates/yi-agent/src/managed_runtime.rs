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
pub fn run_guardian(scope: yi_agent_lifecycle::types::RuntimeScope) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let artifact = crate::build_info::artifact_identity(executable.clone());
    // The guardian starts the daemon from *this* executable, which is what makes the
    // pinned selection meaningful: the bytes the guardian was authorised with are the
    // bytes the daemon runs, and a bundle replaced afterwards cannot change them.
    yi_agent_supervisors::lifecycle::guardian::serve_with_daemon(
        scope,
        artifact,
        || false,
        std::sync::Arc::new(yi_agent_supervisors::lifecycle::preview::NoCoordinator),
        Some(&executable),
    )
    .map_err(|error| anyhow::anyhow!("the managed guardian stopped: {error}"))
}

/// Reads the guardian's opening frame, if one arrives in time.
///
/// `Ok(None)` means "no greeting, treat as unmanaged". Silence is deliberately not
/// a refusal: the only way to see none is a guardian from an older build, or one
/// that failed to write before we asked. Refusing there would turn a compatibility
/// case into a broken runtime, and the design's security claim is about *managed*
/// daemons.
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

/// Applies every authority update the guardian sends, until the channel closes.
///
/// The loop ending *is* the daemon's notice that its guardian is gone; the caller
/// keeps it running for the daemon's whole life for that reason. A malformed or
/// refused frame is logged and skipped rather than fatal: one bad binding must not
/// take down a serving daemon, and the refusal is the guardian's bug to fix.
fn pump_lifeline_authority(
    lifeline: std::os::fd::OwnedFd,
    coordinator: std::sync::Arc<yi_agent_store::runtime::RuntimeCoordinator>,
) {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: this thread is the sole owner of `lifeline` now, and the stream takes
    // over exactly that descriptor.
    let mut stream = std::mem::ManuallyDrop::new(unsafe {
        std::os::unix::net::UnixStream::from_raw_fd(lifeline.as_raw_fd())
    });
    loop {
        match yi_agent_lifecycle::codec::read_frame(&mut &mut *stream) {
            Ok(frame) => {
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

/// Serves one runtime's business daemon under its guardian.
///
/// This is the only daemon start that carries the exit guarantees. It is
/// deliberately not reachable through the public `daemon serve`: those callers are
/// unmanaged by contract, and the manual `daemon start/serve` semantics must not
/// change.
pub fn run_managed_daemon(
    invocation: yi_agent_runtime::config::RuntimeConfig,
) -> anyhow::Result<()> {
    use yi_agent_supervisors::lifecycle::roles::{ManagedRole, inherited_bootstrap};
    let bootstrap = inherited_bootstrap(ManagedRole::Daemon)?;
    // The guardian's first frame carries the generation secret. It is read before
    // the daemon serves because authority must be verifiable from the first attach:
    // a daemon that bound its socket first would spend a window admitting calls it
    // could not check. A guardian that says nothing leaves the daemon unmanaged
    // rather than broken -- the handshake is on a channel the guardian owns, and
    // treating silence as "refuse everything" would make a launch race look like a
    // permission bug.
    let managed = match read_lifeline_hello(&bootstrap) {
        Ok(Some(hello)) => {
            let secret = hello.secret().map_err(|error| anyhow::anyhow!(error))?;
            if hello.scope_key
                != yi_agent_subagent::attach::project_runtime_directory(&invocation.workdir)
                    .to_string_lossy()
                    .into_owned()
                && hello.scope_key.is_empty()
            {
                return Err(anyhow::anyhow!(
                    "the guardian authorized a different runtime scope"
                ));
            }
            let ledger = yi_agent_lifecycle::authorization::AuthorizationLedger::verifier(
                hello.scope_key.clone(),
                hello.daemon_instance_id.clone(),
                secret,
            );
            // The channel stays open for the daemon's whole life: it is both the
            // death monitor and the authority feed, and closing it early would tell
            // the guardian the daemon is gone.
            Some((ledger, bootstrap))
        }
        Ok(None) => None,
        Err(error) => {
            return Err(anyhow::anyhow!(
                "the guardian's authorization handshake failed: {error}"
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
    let daemon = yi_agent_store::ipc::Daemon::start_with_factory_and_authority(
        &runtime_dir,
        &database,
        factory,
        ledger,
    )
    .map_err(|error| anyhow::anyhow!("could not start the managed daemon: {error}"))?;
    // Only now does listening for further authority make sense: the coordinator
    // exists, so a binding frame has somewhere to land. Until the daemon stops, this
    // loop also *is* the death monitor -- it returns when the guardian closes the
    // channel, which is the signal that nothing supervises this process any more.
    if let Some(lifeline) = lifeline {
        let coordinator = std::sync::Arc::clone(daemon.coordinator());
        std::thread::Builder::new()
            .name("lifeline-authority".to_string())
            .spawn(move || pump_lifeline_authority(lifeline, coordinator))
            .map_err(|error| anyhow::anyhow!("could not watch the guardian lifeline: {error}"))?;
    }
    // The business daemon supervises its own plugin children -- that is not handed
    // to the guardian, which knows process groups but not plugin manifests. The
    // second argument is the *runtime directory*: the route table is partitioned by
    // it, and passing the socket path would publish every route into a partition
    // nobody reads.
    let _supervisor = yi_agent_runtime::supervise::serve(&invocation.workdir, &runtime_dir);
    daemon
        .wait()
        .map_err(|error| anyhow::anyhow!("the managed daemon stopped: {error}"))
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
        let call = body
            .lines()
            .find(|line| line.contains("supervise::serve("))
            .expect("the managed daemon must supervise plugin children");
        assert!(
            call.contains("&runtime_dir"),
            "plugin routes are partitioned by runtime directory: {call}"
        );
        assert!(
            !call.contains("socket"),
            "passing the socket path partitions routes where nothing reads them: {call}"
        );
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
}
