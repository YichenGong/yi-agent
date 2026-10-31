//! Integration tests for the main-window close policy.
//!
//! These live outside the crate so they exercise the same `RunEvent` path the
//! real app uses, rather than a re-implementation of it.

use std::sync::{Arc, Mutex};

use tauri::{Manager, RunEvent};

/// What the event loop observed while the test ran.
#[derive(Debug, Default)]
struct Seen {
    close_requests: u32,
    exit_requests: u32,
    /// `Some` once we looked: was the main window still registered?
    alive_after_close: Option<bool>,
}

fn run_close_cycle() -> Seen {
    let app = tauri::test::mock_app();
    let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();

    // Shared, not captured by value: a plain `let mut` would be moved into the
    // closure and the outer read would see the initial value forever.
    let seen = Arc::new(Mutex::new(Seen::default()));
    let observer = Arc::clone(&seen);
    let mut close_sent = false;

    app.run(move |app, event| {
        // The production policy under test.
        desktop_lib::window::handle_run_event(app, &event);

        match event {
            RunEvent::WindowEvent {
                event: tauri::WindowEvent::CloseRequested { .. },
                ..
            } => observer.lock().unwrap().close_requests += 1,

            // First pass: ask the window to close, as the red button does.
            RunEvent::MainEventsCleared if !close_sent => {
                close_sent = true;
                let _ = window.close();
            }

            // Later passes: the close has been handled. Look, then stop the
            // mock loop by destroying the window (its only exit path).
            RunEvent::MainEventsCleared => {
                let seen = observer.lock().unwrap();
                if seen.alive_after_close.is_none() {
                    drop(seen);
                    let alive = app.get_webview_window("main").is_some();
                    observer.lock().unwrap().alive_after_close = Some(alive);
                    let _ = window.destroy();
                }
            }

            RunEvent::ExitRequested { .. } => observer.lock().unwrap().exit_requests += 1,
            _ => {}
        }
    });

    let seen = seen.lock().unwrap();
    Seen {
        close_requests: seen.close_requests,
        exit_requests: seen.exit_requests,
        alive_after_close: seen.alive_after_close,
    }
}

#[test]
fn closing_the_window_keeps_it_registered_instead_of_quitting() {
    let seen = run_close_cycle();

    assert_eq!(
        seen.close_requests, 1,
        "the red button must arrive as one CloseRequested"
    );
    assert_eq!(
        seen.alive_after_close,
        Some(true),
        "prevent_close must leave the window registered; Some(false) means it was destroyed"
    );
}
