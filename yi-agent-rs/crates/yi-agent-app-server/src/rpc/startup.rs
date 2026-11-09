//! The legacy plugin-startup unit: ensure the daemon, wait for its query
//! channel, install the resident watchman -- **under one instance gate**.
//!
//! Why one unit and not three tasks: each step is a way for a migration to be
//! overtaken by the very self-healing it is shutting down. Between `ensure` and the
//! readiness wait, a migration can stop the daemon that was just started and this
//! code will then wait ten seconds for a process that no longer exists; after the
//! migration writes its freeze marker, the watchman install would put the
//! self-healing back. Holding the instance gate for the whole unit is what makes
//! those interleavings impossible, and it is also what makes the drain honest: a
//! migration waits for this unit to finish instead of racing it.
//!
//! It runs on a blocking worker rather than in an async task because every step
//! blocks on a child process or a socket poll, and because the gate must be owned by
//! the thing actually doing the work: if the RPC caller drops the future, the worker
//! still finishes and only then releases the gate.

use std::path::Path;
#[cfg(test)]
use std::sync::Arc;

use yi_agent_supervisors::lifecycle::migration_operation::RetirementGate;

use crate::server::{EnsureDaemon, WatchmanInstall};

/// One budget for the whole unit.
///
/// Long enough for a cold daemon plus a plugin that needs a moment to bind its
/// socket, short enough that a wedged plugin cannot hold the instance gate -- and so
/// every migration waiting behind it -- for minutes.
const PLUGIN_STARTUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// What the unit managed to do.
pub(crate) struct StartupOutcome {
    /// `running`, `retrying`, or `awaiting-demand` -- the same vocabulary the RPC
    /// response uses, kept here so the mapping lives in one place.
    pub(crate) startup: &'static str,
    /// Whether the plugin can be queried right now.
    pub(crate) queryable: bool,
    /// A human-readable note for the log, when something worth reporting happened.
    pub(crate) warning: Option<String>,
}

/// Run the three steps under the instance gate, on a shared budget.
///
/// Returns `awaiting-demand` without doing anything when the instance is retired or
/// frozen: a migration owns the instance then, and every step here is exactly what
/// it is shutting down.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_plugin(
    daemon_project: &Path,
    ready_project: &Path,
    manifest: &yi_agent_supervisors::manifest::SupervisorManifest,
    workdir: &Path,
    home: &Path,
    instance_home: &Path,
    ensure: &EnsureDaemon,
    install: &WatchmanInstall,
) -> StartupOutcome {
    let budget = yi_agent_lifecycle::deadline::Deadline::after(PLUGIN_STARTUP_BUDGET);
    let gate = match RetirementGate::acquire(instance_home, budget.clone()) {
        Ok(gate) => gate,
        Err(_) => {
            // A migration holds the gate, so it is either stopping daemons now or
            // about to. Starting one here is the resurrection the gate prevents.
            return StartupOutcome {
                startup: "awaiting-demand",
                queryable: false,
                warning: Some(
                    "a migration holds the instance gate; not starting the plugin now".to_string(),
                ),
            };
        }
    };
    // Re-read the markers *under* the gate: they may have been written between the
    // caller's own check and this acquisition.
    if !yi_agent_supervisors::watch::may_start_unattended(instance_home) {
        drop(gate);
        return StartupOutcome {
            startup: "awaiting-demand",
            queryable: false,
            warning: Some(
                "the instance stopped allowing unattended starts; the plugin waits for demand"
                    .to_string(),
            ),
        };
    }
    let outcome = start_under_gate(
        daemon_project,
        ready_project,
        manifest,
        workdir,
        home,
        &budget,
        ensure,
        install,
    );
    // Explicit rather than incidental: the gate must be gone before this returns, so
    // a caller that immediately starts another installation is not blocked by us.
    drop(gate);
    outcome
}

/// The steps themselves, already under the gate.
#[allow(clippy::too_many_arguments)]
fn start_under_gate(
    daemon_project: &Path,
    ready_project: &Path,
    manifest: &yi_agent_supervisors::manifest::SupervisorManifest,
    workdir: &Path,
    home: &Path,
    budget: &yi_agent_lifecycle::deadline::Deadline,
    ensure: &EnsureDaemon,
    install: &WatchmanInstall,
) -> StartupOutcome {
    let mut warning = None;

    // Step 1: the daemon. Best-effort by contract -- the manifest is already on disk
    // and the resident watch will retry -- but "best-effort" is about the *result*,
    // not about skipping the budget check.
    let mut startup = if budget.is_expired() {
        warning = Some("the startup budget ran out before the daemon could start".to_string());
        "retrying"
    } else {
        match ensure(daemon_project) {
            Ok(true) => "running",
            Ok(false) => {
                warning = Some(
                    "the daemon did not become ready in time; the resident watch will retry"
                        .to_string(),
                );
                "retrying"
            }
            Err(error) => {
                warning = Some(format!(
                    "could not start the daemon: {error}; the resident watch will retry"
                ));
                "retrying"
            }
        }
    };

    // Step 2: the query channel. Only meaningful when a daemon is running; asking
    // for a channel whose daemon we never started just burns the budget.
    // The wait keeps its own short cap, clamped by what is left: it is a *poll for a
    // socket*, not a place to spend the whole unit budget. Clamping rather than
    // using the budget directly also keeps the old behaviour where a plugin that
    // never appears is reported in seconds instead of at the budget's edge.
    let queryable = if startup == "running" && !budget.is_expired() {
        // The wait keeps its own short cap, clamped by what is left of the unit
        // budget: it polls for a socket, it is not a place to spend the whole budget.
        // Clamping also keeps the old behaviour where a plugin that never appears is
        // reported in seconds rather than at the budget's edge.
        let wait = yi_agent_lifecycle::deadline::Deadline::after(
            budget.clamp(crate::server::PLUGIN_READY_TIMEOUT),
        );
        crate::server::wait_for_plugin_query_socket_until(ready_project, manifest, &wait)
    } else {
        false
    };
    if startup == "running" && !queryable {
        warning = Some(
            "the plugin's query channel was not ready in time; the resident watch will retry"
                .to_string(),
        );
        startup = "retrying";
    }

    // Step 3: the resident watchman. Still under the same gate, so it cannot appear
    // *after* a migration froze the instance: the watchman is the self-healing this
    // migration exists to remove.
    if !budget.is_expired()
        && let Err(error) =
            crate::server::ensure_watchman_installed_until(workdir, home, install, budget)
    {
        warning = Some(format!("could not install the resident watchman: {error}"));
    }

    StartupOutcome {
        startup,
        queryable,
        warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_manifest() -> yi_agent_supervisors::manifest::SupervisorManifest {
        yi_agent_supervisors::manifest::SupervisorManifest {
            name: "demo".into(),
            command: "demo".into(),
            args: Vec::new(),
            switch_key: "demo.enabled".into(),
            restart_backoff_ms: 100,
            restart_backoff_max_ms: 1000,
            stop_when_disabled: true,
            query_socket: None,
            session_driver: false,
        }
    }

    /// A budget already spent must not run any step: the caller gave up, and the gate
    /// should not be held for work nobody is waiting for.
    #[test]
    fn an_expired_budget_runs_no_step() {
        let directory = tempfile::tempdir().unwrap();
        let ensure: EnsureDaemon = Arc::new(|_: &Path| -> Result<bool, String> {
            panic!("an expired budget must not start a daemon")
        });
        let install: WatchmanInstall = Arc::new(|_: &Path, _: &Path| -> Result<(), String> {
            panic!("an expired budget must not install the watchman")
        });
        let manifest = demo_manifest();
        let budget = yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::ZERO);

        let outcome = start_under_gate(
            directory.path(),
            directory.path(),
            &manifest,
            directory.path(),
            directory.path(),
            &budget,
            &ensure,
            &install,
        );

        assert_eq!(outcome.startup, "retrying");
        assert!(!outcome.queryable, "no daemon ran, so nothing is queryable");
    }

    /// The budget is one budget, shared by every step: once it is spent, the later
    /// steps must not run. Otherwise "I stopped after the deadline" would only be
    /// true of the step that noticed.
    #[test]
    fn expired_startup_budget_prevents_later_side_effects() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let directory = tempfile::tempdir().unwrap();
        let installs = Arc::new(AtomicUsize::new(0));
        let installs_for_closure = Arc::clone(&installs);
        // A daemon that consumes the whole budget before answering.
        let ensure: EnsureDaemon = Arc::new(|_: &Path| -> Result<bool, String> {
            std::thread::sleep(std::time::Duration::from_millis(80));
            Ok(true)
        });
        // The recorded closure is the *outer* budget gate's only observable: if the
        // unit calls it at all after the budget is gone, this counter moves.
        let install: WatchmanInstall = Arc::new(move |_: &Path, _: &Path| -> Result<(), String> {
            installs_for_closure.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let manifest = demo_manifest();
        // A budget that the ensure step itself exhausts.
        let budget =
            yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_millis(50));

        let outcome = start_under_gate(
            directory.path(),
            directory.path(),
            &manifest,
            directory.path(),
            directory.path(),
            &budget,
            &ensure,
            &install,
        );

        assert!(
            !outcome.queryable,
            "a spent budget must not be spent further on a query wait"
        );
        // Even though the settings switch defaults to on, the *outer* gate must not
        // reach the install at all: the only thing that could have stopped it here is
        // this unit's own budget check.
        //
        // `ensure_watchman_installed_until` also checks the budget internally, which
        // is deliberate defence in depth -- but it means the counter alone cannot tell
        // the two gates apart. What distinguishes them is whether the call happened,
        // which the warning records.
        assert!(
            outcome
                .warning
                .as_deref()
                .is_some_and(|warning| !warning.contains("watchman")),
            "the watchman step must be skipped outright, not attempted and refused: {:?}",
            outcome.warning
        );
    }

    /// The gate belongs to the worker, not to the caller: dropping the async task
    /// must not release it while the worker is still running.
    ///
    /// Proven with real threads and the real gate, because the property is about
    /// ownership of a file lock -- a mock gate would prove nothing about it.
    #[test]
    fn timed_out_caller_does_not_release_worker_gate() {
        use yi_agent_supervisors::lifecycle::migration_operation::RetirementGate;

        let directory = tempfile::tempdir().unwrap();
        let instance_home = directory.path().to_path_buf();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let worker_home = instance_home.clone();
        let worker = std::thread::spawn(move || {
            let budget =
                yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(10));
            let gate = RetirementGate::acquire(&worker_home, budget.clone())
                .expect("the worker takes the gate first");
            entered_tx.send(()).expect("announce the acquisition");
            // Stand in for the blocking steps; the caller has already given up.
            let _ = release_rx.recv();
            drop(gate);
        });

        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the worker must reach its barrier");
        // The caller timed out long ago; the worker still owns the gate.
        let contender = RetirementGate::acquire(
            &instance_home,
            yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_millis(120)),
        );
        assert!(
            contender.is_err(),
            "a timed-out caller must not release the worker's gate"
        );
        release_tx.send(()).expect("let the worker finish");
        worker.join().expect("the worker must not panic");
        assert!(
            RetirementGate::acquire(
                &instance_home,
                yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(2))
            )
            .is_ok(),
            "once the worker ends, the gate is free"
        );
    }

    /// A frozen instance must not be handed a daemon: that is the resurrection the
    /// freeze exists to stop.
    #[test]
    fn a_frozen_instance_starts_nothing() {
        let directory = tempfile::tempdir().unwrap();
        yi_agent_supervisors::lifecycle::retirement::freeze(directory.path(), "a migration")
            .unwrap();
        let ensure: EnsureDaemon = Arc::new(|_: &Path| -> Result<bool, String> {
            panic!("a frozen instance must not start a daemon")
        });
        let install: WatchmanInstall = Arc::new(|_: &Path, _: &Path| -> Result<(), String> {
            panic!("a frozen instance must not install the watchman")
        });
        let manifest = demo_manifest();

        let outcome = start_plugin(
            directory.path(),
            directory.path(),
            &manifest,
            directory.path(),
            directory.path(),
            directory.path(),
            &ensure,
            &install,
        );

        assert_eq!(outcome.startup, "awaiting-demand");
        assert!(!outcome.queryable);
    }
}
