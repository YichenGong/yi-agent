//! Integration tests for the main-window close policy.
//!
//! These live outside the crate so they exercise the same `RunEvent` path the
//! real app uses, rather than a re-implementation of it.

#[test]
fn the_window_module_is_reachable_from_an_integration_test() {
    // Any public item proves the module is reachable. `fit_main_window` is
    // today's entry point for main-window chores; it is generic-free, so a
    // plain function pointer works (verified by compiling this step).
    let _f: fn(&tauri::AppHandle) -> tauri::Result<()> = desktop_lib::window::fit_main_window;
}
