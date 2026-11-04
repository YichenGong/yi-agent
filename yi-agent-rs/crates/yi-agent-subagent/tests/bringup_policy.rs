//! Which bring-up path a build uses, and which one it must not yet use.
//!
//! The default is the load-bearing assertion here. Shipping the managed path as the
//! default before it has been validated end to end would make every user the test of
//! it, and the failure mode is not a crash -- it is a runtime that starts under one
//! configuration and is reused under another.

use std::path::Path;
use std::time::Duration;

use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::attach::{BringUpPolicy, ensure_owned_runtime_with_policy};

#[test]
fn the_default_path_is_still_the_embedded_one() {
    assert_eq!(
        BringUpPolicy::default_for_build(),
        BringUpPolicy::Embedded,
        "the managed path becomes the default only after real-process validation"
    );
}

#[test]
fn the_environment_can_select_the_managed_path_explicitly() {
    // `from_env` is the only selector; asserting the value here keeps the mapping
    // honest without mutating a process-global variable that other tests read.
    assert_eq!(
        BringUpPolicy::from_value(Some("managed")),
        BringUpPolicy::Managed
    );
}

#[test]
fn an_unknown_or_absent_selector_falls_back_to_the_build_default() {
    assert_eq!(BringUpPolicy::from_value(None), BringUpPolicy::Embedded);
    assert_eq!(
        BringUpPolicy::from_value(Some("embedded")),
        BringUpPolicy::Embedded
    );
    assert_eq!(
        BringUpPolicy::from_value(Some("")),
        BringUpPolicy::Embedded,
        "an empty variable is not a selection"
    );
    assert_eq!(
        BringUpPolicy::from_value(Some("MANAGED")),
        BringUpPolicy::Embedded,
        "the selector is exact: a near miss must not silently change the path"
    );
}

/// A managed bring-up with no expectation must refuse instead of guessing.
///
/// The refusal is the substantive part: a managed runtime is classified against the
/// artifact the caller shipped with, and `Unknown` is deliberately not permission to
/// run work. If this returned `Ok`, the caller would silently get either a wrongly
/// accepted runtime or a mismatched one.
#[test]
fn a_managed_bring_up_without_an_expected_artifact_is_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let cfg = RuntimeConfig::load_lenient(&yi_agent_runtime::config::ConfigOverrides {
        workdir: Some(directory.path().to_path_buf()),
        ..Default::default()
    })
    .expect("a lenient load must not need a model catalogue");
    let outcome = ensure_owned_runtime_with_policy(
        &cfg,
        directory.path().join("runtime"),
        &directory.path().join("models.json"),
        directory.path(),
        BringUpPolicy::Managed,
        None,
    );
    let failure = match outcome {
        Ok(_) => panic!("a managed start must not guess the artifact it expects"),
        Err(failure) => failure,
    };
    assert_eq!(failure.stage, "runtime bring-up");
    assert!(
        failure.cause.contains("artifact identity"),
        "the refusal must name what is missing, got: {}",
        failure.cause
    );
    assert!(
        !failure.retryable,
        "retrying without an expectation cannot help, so this is not transient"
    );
    assert!(
        !directory.path().join("runtime").exists(),
        "a refused managed start must not create a runtime directory"
    );
}

/// The managed budget has to be long enough for the work it covers.
#[test]
fn the_managed_budget_covers_a_cold_guardian_and_daemon_start() {
    // The guardian alone may spend 20 s starting its daemon before reporting
    // failure, so a smaller end-to-end budget would time out a path that was about
    // to succeed -- which reads as "the runtime is broken" rather than "we stopped
    // waiting".
    assert!(
        yi_agent_subagent::attach::MANAGED_START_BUDGET >= Duration::from_secs(25),
        "the managed budget must exceed the guardian's own start allowance"
    );
    let _ = Path::new("/");
}

/// The sidecar the desktop launches must be a *managed* frontend.
///
/// This is the whole reason the foreground contract exists, and it is worth
/// stating as its own assertion: a direct `app-server` sidecar runs embedded, has
/// no guardian, and therefore can never be authorized -- so every turn would fall
/// back to the legacy watchdog and a long-lived conversation would still be
/// settled as a stalled background attempt.
///
/// The value is read from the same place production reads it, so flipping the
/// default without wiring the caller shows up here.
#[test]
fn foreground_root_lifecycle_the_desktop_sidecar_is_a_managed_frontend() {
    assert_eq!(
        yi_agent_subagent::attach::BringUpPolicy::for_managed_frontend(),
        BringUpPolicy::Managed,
        "a frontend that holds a lease must ask the guardian, not start its own daemon"
    );
}
