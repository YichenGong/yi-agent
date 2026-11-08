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

/// Takes the inherited control descriptor, closing it on any later error.
///
/// Separate from [`run_executor_with_control`] so the production path can prove
/// it refused an unauthorised start *before* reading configuration, creating a
/// runtime socket directory, or building tools.
pub fn take_executor_control() -> anyhow::Result<OwnedFd> {
    Ok(inherited_descriptor()?)
}

/// Runs one executor on an already-taken control descriptor.
///
/// Taking an `OwnedFd` by value is the ordering guarantee, not a style choice:
/// there is no way to reach this function without having authorised first, so a
/// later refactor cannot accidentally load configuration (and thus this
/// machine's credentials) before the refusal. The descriptor also closes on any
/// error path below, which an `i32` would not.
///
/// The control channel carries work, never authority: the provider, the model
/// and the credentials are decided here, from this machine's files. `binding`
/// is this process's own settings source -- whatever the daemon that spawned us
/// resolved is *not* inherited, because a shared settings path does not imply
/// shared overrides or environment.
pub fn run_executor_with_control(
    control: OwnedFd,
    owner: String,
    config_path: Option<PathBuf>,
    invocation: yi_agent_runtime::config::RuntimeConfig,
    binding: Option<yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
) -> anyhow::Result<()> {
    let models_path = config_path.unwrap_or_else(yi_agent_runtime::models::models_path);
    let runtime_socket = yi_agent_subagent::attach::project_runtime_directory(&invocation.workdir);
    // Hold the observation lease for as long as this executor runs: without it a
    // settings save would only be seen when a worker happens to read on demand.
    let _settings_lease = binding
        .as_ref()
        .map(|binding| binding.live.start_observing());
    // `worker_factory_with_binding` consumes its argument, so hand it a clone and
    // keep ours alive for the lease above.
    let factory = yi_agent_subagent::attach::worker_factory_with_binding(
        &invocation,
        runtime_socket,
        &models_path,
        binding,
    )?;
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

/// Serves one runtime's business daemon under its guardian.
///
/// This is the only daemon start that carries the exit guarantees. It is
/// deliberately not reachable through the public `daemon serve`: those callers are
/// unmanaged by contract, and the manual `daemon start/serve` semantics must not
/// change.
pub fn run_managed_daemon(
    invocation: yi_agent_runtime::config::RuntimeConfig,
    live_settings: Option<yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
) -> anyhow::Result<()> {
    use yi_agent_supervisors::lifecycle::roles::{ManagedRole, inherited_bootstrap};
    let _bootstrap = inherited_bootstrap(ManagedRole::Daemon)?;
    // The instance's configuration is inherited from the process that started us,
    // not re-derived from the project cwd: a shared daemon must keep reading the
    // same `YI_AGENT_HOME` and model catalogue its launcher used. Only the workdir
    // (the *root* being served) is taken from the invocation.
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&invocation.workdir);
    let database = runtime_dir.join("runtime.sqlite");
    // The daemon resolves its own source (the launcher passes one in) so every
    // worker it starts follows a settings save at its next safe point. `None`
    // keeps the historical static factory for callers that have no binding.
    // The lease is taken *before* the binding is handed to the factory, from a
    // clone: `worker_factory_with_binding` consumes its argument.
    let settings_lease = live_settings
        .as_ref()
        .map(|binding| binding.live.start_observing());
    let factory = yi_agent_subagent::attach::worker_factory_with_binding(
        &invocation,
        runtime_dir.clone(),
        &yi_agent_runtime::models::models_path(),
        live_settings,
    )?;
    let daemon = yi_agent_store::ipc::Daemon::start_with_factory(&runtime_dir, &database, factory)
        .map_err(|error| anyhow::anyhow!("could not start the managed daemon: {error}"))?;
    // The business daemon supervises its own plugin children -- that is not handed
    // to the guardian, which knows process groups but not plugin manifests. The
    // second argument is the *runtime directory*: the route table is partitioned by
    // it, and passing the socket path would publish every route into a partition
    // nobody reads.
    let _supervisor = yi_agent_runtime::supervise::serve(&invocation.workdir, &runtime_dir);
    let result = daemon
        .wait()
        .map_err(|error| anyhow::anyhow!("the managed daemon stopped: {error}"));
    // 同步退出路径，没有 reactor：阻塞等待观察线程结束，而不是留下一个
    // 正在退出的进程里的孤儿线程。
    if let Some(lease) = settings_lease {
        lease.shutdown_blocking();
    }
    result
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
