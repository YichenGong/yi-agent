//! Integration tests for the quit barrier, driven through the real `RunEvent`
//! path rather than a re-implementation of the policy.
//!
//! These live outside the crate for the same reason `close_hides_window.rs`
//! does: `handle_run_event` takes a `RunEvent`, and only a running app delivers
//! real ones.
//!
//! `MockRuntime::request_exit` is `unimplemented!()`, so no test here drives the
//! real `app.exit(0)`: a loop that called it would panic before asserting
//! anything. What is testable without it is asserted instead -- the barrier's
//! phase machine, the run-event handler's dispatch, and the drain itself running
//! against a real `AppHandle` -- and the full loop is covered by the paused-clock
//! unit tests in `runtime_lifecycle`.

#![cfg(desktop)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use desktop_lib::runtime_lifecycle::{
    self, ControlBackend, DesktopRuntimeManager, ExitDecision, QuitBarrier, QuitState,
};
use tauri::{Manager, RunEvent};
use yi_agent_lifecycle::types::{LeaseMode, ReclaimResult, RuntimeScope};

/// A control channel that records every release and reports a stop at once.
///
/// Deliberately does not model latency: the point of these tests is the event
/// loop's behaviour (hold once, drain, allow). Timing is covered by the
/// paused-clock unit tests, which can drive a clock a real loop cannot.
struct RecordingControl {
    releases: Arc<AtomicUsize>,
}

impl ControlBackend for RecordingControl {
    fn acquire(
        &self,
        _client_run_id: &str,
        _scope: &RuntimeScope,
        _mode: LeaseMode,
        _budget: Duration,
    ) -> std::io::Result<bool> {
        Ok(true)
    }

    fn release(
        &self,
        client_run_id: &str,
        _scope: &RuntimeScope,
        _budget: Duration,
    ) -> std::io::Result<ReclaimResult> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(ReclaimResult {
            client_run_id: client_run_id.to_string(),
            stopped: true,
            failures: Vec::new(),
        })
    }
}

fn project_scope(root: &Path, name: &str) -> RuntimeScope {
    RuntimeScope {
        key: format!("{}/{name}", root.display()),
        runtime_dir: root.join(name).join(".yi-agent/runtime"),
        project_key: root.join(name).display().to_string(),
        instance_home: root.join("home"),
    }
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("yi-agent-quit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn the_barrier_only_holds_the_first_request() {
    let barrier = QuitBarrier::new();
    assert_eq!(barrier.decide(), ExitDecision::BeginDrain);
    // Re-entry while draining must not start a second drain.
    assert_eq!(barrier.decide(), ExitDecision::KeepHolding);
    // After the drain releases, the framework's own exit request passes.
    barrier.release();
    assert_eq!(barrier.decide(), ExitDecision::Allow);
}

#[test]
fn a_non_exit_event_never_starts_a_drain() {
    // Hiding the window is a `WindowEvent`, not an exit: it must not release
    // anything, or the user's open work would be reclaimed by a window hide.
    let app = tauri::test::mock_app();
    let releases = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(RecordingControl {
        releases: Arc::clone(&releases),
    });
    let manager = Arc::new(DesktopRuntimeManager::with_backend("app-run", control));
    app.manage(QuitState::new(manager));

    let handle = app.handle().clone();
    runtime_lifecycle::handle_run_event(&handle, &RunEvent::MainEventsCleared);
    runtime_lifecycle::handle_run_event(&handle, &RunEvent::Ready);
    runtime_lifecycle::handle_run_event(&handle, &RunEvent::Resumed);

    assert_eq!(
        releases.load(Ordering::SeqCst),
        0,
        "a non-exit event must not release any project"
    );
}

/// The drain itself, exercised without the mock runtime's exit limitation.
///
/// This is the integration-level counterpart of the paused-clock unit tests: a
/// real `AppHandle` provides the state, and `shutdown` must still release every
/// registered project.
#[test]
fn the_drain_releases_every_project_through_a_real_app_handle() {
    let app = tauri::test::mock_app();
    let releases = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(RecordingControl {
        releases: Arc::clone(&releases),
    });
    let manager = Arc::new(DesktopRuntimeManager::with_backend("app-run", control));
    let root = temp_root("drain");

    tauri::async_runtime::block_on(async {
        for index in 0..3 {
            manager
                .acquire_project(
                    project_scope(&root, &format!("p{index}")),
                    LeaseMode::Business,
                )
                .await
                .unwrap();
        }
        let results = manager
            .shutdown(yi_agent_lifecycle::deadline::Deadline::after(
                Duration::from_secs(5),
            ))
            .await;
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|result| result.stopped));
    });
    app.manage(QuitState::new(manager));

    assert_eq!(releases.load(Ordering::SeqCst), 3);
    let _ = std::fs::remove_dir_all(&root);
}
