//! The private runtime roles and the descriptor that authorizes them.
//!
//! A managed guardian and a managed daemon are started *by* something that already
//! owns the lifecycle decision. They must therefore refuse to run when started by
//! hand: a daemon that accepted its scope from a public command line could be
//! pointed at another project's database, and a guardian that accepted "serve this
//! scope" from any local process would let anyone own this user's process groups.
//!
//! The proof is an inherited descriptor. Not a token, not a file: a descriptor can
//! only come from a parent that had it, so "my parent started me as a managed
//! role" is unfalsifiable by the child itself, which is exactly the property we
//! want. The number is passed in the environment and removed immediately, so a
//! process this one starts does not inherit a way to claim the same role.

use std::io;
use std::os::fd::OwnedFd;

/// Environment variable naming the inherited bootstrap descriptor.
pub const BOOTSTRAP_FD_ENV: &str = "YI_AGENT_BOOTSTRAP_FD";

/// The scope key a managed role must serve, passed alongside the descriptor.
pub const SCOPE_KEY_ENV: &str = "YI_AGENT_SCOPE_KEY";

/// Which private role is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedRole {
    Guardian,
    Daemon,
}

impl ManagedRole {
    /// The command a guardian uses to start its daemon, and the one a user would
    /// type by mistake.
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::Guardian => "guardian",
            Self::Daemon => "daemon",
        }
    }
}

/// Takes the bootstrap descriptor for `role`, or explains why it is absent.
///
/// On success the variable is removed from the environment: a child must not
/// inherit an authorization it was not given, and this is the only place that
/// knows the variable exists.
pub fn inherited_bootstrap(role: ManagedRole) -> io::Result<OwnedFd> {
    let raw = std::env::var(BOOTSTRAP_FD_ENV).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{BOOTSTRAP_FD_ENV} is not set; the managed {} role is started by the \
                 runtime, never by hand",
                role.subcommand()
            ),
        )
    })?;
    // SAFETY: this runs at the top of a fresh process, before any thread exists, so
    // no reader can race the removal.
    unsafe { std::env::remove_var(BOOTSTRAP_FD_ENV) };
    let raw: i32 = raw
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad bootstrap descriptor"))?;
    if raw <= 2 {
        // 0/1/2 are a terminal or a log. A daemon whose bootstrap channel is stdout
        // would read its own log as protocol.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the bootstrap descriptor must not be a standard stream",
        ));
    }
    // The number can name a descriptor that is not open -- a launcher that died
    // before the `exec`, or a stale variable. Taking ownership of a closed number
    // is not merely wrong, it aborts the process with an I/O-safety violation, so
    // the check has to happen before `from_raw_fd`.
    // SAFETY: `fcntl(F_GETFD)` only reads flags; it neither closes nor opens.
    if unsafe { libc::fcntl(raw, libc::F_GETFD) } == -1 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{BOOTSTRAP_FD_ENV} names descriptor {raw}, which is not open; the \
                 managed {} role must be started by a live launcher",
                role.subcommand()
            ),
        ));
    }
    // SAFETY: the descriptor is open and was dup'd into this child by the launcher,
    // which declared it in the environment; taking ownership here is what closes it
    // on exit.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

use std::os::fd::FromRawFd;

/// The scope key a managed role must serve.
pub fn scope_key() -> io::Result<String> {
    std::env::var(SCOPE_KEY_ENV).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{SCOPE_KEY_ENV} is not set; a managed role is told which runtime it \
                 serves by the process that started it"
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables are process-global, so every assertion about their
    /// absence has to run in a child process. A mutex would serialise the writers,
    /// not the readers.
    fn in_child(test: &str, env: &[(&str, &str)], remove: &[&str]) -> String {
        let executable = std::env::current_exe().unwrap();
        let mut command = std::process::Command::new(executable);
        command
            .arg("--exact")
            .arg(test)
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env_remove(BOOTSTRAP_FD_ENV)
            .env_remove(SCOPE_KEY_ENV);
        for key in remove {
            command.env_remove(key);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            text.contains("PROBE"),
            "the child never reported: stdout={text} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        text
    }

    #[test]
    fn bootstrap_probe() {
        if std::env::var_os("YI_AGENT_BOOTSTRAP_PROBE").is_none() {
            return;
        }
        match inherited_bootstrap(ManagedRole::Daemon) {
            Ok(_) => println!("PROBE ok"),
            Err(error) => println!("PROBE kind={:?} message={error}", error.kind()),
        }
    }

    fn probe(env: &[(&str, &str)]) -> String {
        in_child(
            "lifecycle::roles::tests::bootstrap_probe",
            &[("YI_AGENT_BOOTSTRAP_PROBE", "1")]
                .into_iter()
                .chain(env.iter().copied())
                .collect::<Vec<_>>(),
            &[],
        )
    }

    /// A managed role started by hand must not run.
    #[test]
    fn a_role_without_a_bootstrap_descriptor_refuses_to_run() {
        let text = probe(&[]);
        assert!(text.contains("kind=NotFound"), "{text}");
        assert!(text.contains("never by hand"), "{text}");
    }

    /// A standard stream is not an authorization channel.
    #[test]
    fn a_standard_stream_is_not_accepted_as_a_bootstrap_descriptor() {
        let text = probe(&[(BOOTSTRAP_FD_ENV, "1")]);
        assert!(text.contains("kind=InvalidInput"), "{text}");
        assert!(text.contains("standard stream"), "{text}");
    }

    #[test]
    fn a_nonsense_descriptor_is_rejected() {
        let text = probe(&[(BOOTSTRAP_FD_ENV, "not a number")]);
        assert!(text.contains("kind=InvalidInput"), "{text}");
    }

    #[test]
    fn scope_key_probe() {
        if std::env::var_os("YI_AGENT_SCOPE_PROBE").is_none() {
            return;
        }
        match scope_key() {
            Ok(key) => println!("PROBE key={key}"),
            Err(error) => println!("PROBE kind={:?}", error.kind()),
        }
    }

    /// A managed role is told which runtime it serves; guessing one would let a
    /// hand-started process adopt any runtime it can name.
    #[test]
    fn a_role_without_a_scope_key_refuses_to_run() {
        let text = in_child(
            "lifecycle::roles::tests::scope_key_probe",
            &[("YI_AGENT_SCOPE_PROBE", "1")],
            &[SCOPE_KEY_ENV],
        );
        assert!(text.contains("kind=NotFound"), "{text}");
    }
}
