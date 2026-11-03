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

#[cfg(test)]
mod tests {
    use super::*;

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
