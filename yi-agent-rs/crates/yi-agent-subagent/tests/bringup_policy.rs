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

/// A managed start that cannot succeed must fail fast, not burn the whole budget.
///
/// The sidecar calls this from an async request path, synchronously. If a missing
/// or unusable guardian took the full 30 s allowanace, every thread start on a
/// machine without a managed runtime would stall for half a minute before falling
/// back -- a launch that looks hung. The budget is a ceiling for a wedged path, not
/// an expected duration.
#[test]
fn foreground_root_lifecycle_an_impossible_managed_start_fails_fast() {
    let directory = tempfile::tempdir().expect("tempdir");
    let cfg = RuntimeConfig::load_lenient(&yi_agent_runtime::config::ConfigOverrides {
        workdir: Some(directory.path().to_path_buf()),
        ..Default::default()
    })
    .expect("a lenient load must not need a model catalogue");
    let expected = yi_agent_lifecycle::types::ArtifactIdentity {
        build_id: "fails-fast-probe".into(),
        display_version: "0.0.0".into(),
        // Nothing is installed here, so the guardian cannot be launched.
        executable: directory.path().join("no-such-binary"),
        sha256: String::new(),
        schema_min: 0,
        schema_max: 17,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: Vec::new(),
    };
    let started = std::time::Instant::now();
    let outcome = ensure_owned_runtime_with_policy(
        &cfg,
        directory.path().join("runtime"),
        &directory.path().join("models.json"),
        directory.path(),
        BringUpPolicy::Managed,
        Some(&expected),
    );
    let elapsed = started.elapsed();
    assert!(
        outcome.is_err(),
        "a managed start with no installed guardian must not claim success"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "an impossible managed start took {elapsed:?}; the caller blocks an async \
         request path and would look hung"
    );
}

/// The two policy decisions stay separable.
///
/// `default_for_build` answers "what does an unconfigured build do" and must keep
/// answering `Embedded`, so a manually started `yi-agent daemon` owns its own
/// runtime. `for_managed_frontend` answers a different question -- "what does a
/// frontend that registered a lease do" -- and must answer `Managed`, because a
/// runtime it started itself has no guardian to authorize a root against, so its
/// foreground turns would fall back to the legacy watchdog and a long conversation
/// would be reported as a stalled background attempt.
///
/// Kept as two assertions rather than one collapsed constant: if these ever become
/// the same value, a manual daemon silently becomes guardian-owned.
#[test]
fn foreground_root_lifecycle_the_two_policy_decisions_stay_separable() {
    assert_eq!(
        yi_agent_subagent::attach::BringUpPolicy::for_managed_frontend(),
        yi_agent_subagent::attach::BringUpPolicy::Managed,
        "a frontend holding a lease must ask the guardian to own its runtime"
    );
    assert_eq!(
        yi_agent_subagent::attach::BringUpPolicy::default_for_build(),
        yi_agent_subagent::attach::BringUpPolicy::Embedded,
        "a manual `yi-agent daemon` must keep owning its own runtime"
    );
}

/// An explicit selector overrides in both directions, and an unknown value does not.
///
/// The environment variable is the only way an operator can pin behaviour back, so
/// `embedded` must still be reachable even once the managed path is the supported
/// one. Tested through `from_value` rather than `from_env` so the mapping can be
/// checked without mutating a process-global every other test also reads.
#[test]
fn foreground_root_lifecycle_an_explicit_selector_wins_both_ways() {
    use yi_agent_subagent::attach::BringUpPolicy;

    assert_eq!(
        BringUpPolicy::from_value(Some("managed")),
        BringUpPolicy::Managed
    );
    assert_eq!(
        BringUpPolicy::from_value(Some("embedded")),
        BringUpPolicy::Embedded
    );
    assert_eq!(
        BringUpPolicy::from_value(Some("nonsense")),
        BringUpPolicy::default_for_build(),
        "an unrecognised selector must not silently opt a build into managed"
    );
    assert_eq!(
        BringUpPolicy::from_value(None),
        BringUpPolicy::default_for_build()
    );
}

/// A frontend that declared itself gets the managed policy with **no** environment
/// variable, and the build default stays embedded beside it.
///
/// These two facts are the change, and they must hold together: the earlier attempt
/// applied the managed policy to anyone holding a context, which swept a manual
/// `daemon` into a guardian-owned runtime and broke the refusal the managed path is
/// supposed to make. An explicit declaration keeps them separable.
///
/// Asserted as a pure mapping because `install_managed_context` is one-shot per
/// process and the other tests in this binary must run without it.
#[test]
fn a_declared_frontend_selects_managed_while_the_build_default_stays_embedded() {
    assert_eq!(
        BringUpPolicy::for_managed_frontend(),
        BringUpPolicy::Managed,
        "a declared frontend must ask the guardian to own its runtime"
    );
    assert_eq!(
        BringUpPolicy::default_for_build(),
        BringUpPolicy::Embedded,
        "an unconfigured build must not be handed a guardian-owned runtime"
    );
    // The resolver's precedence, stated as the function that carries it: an explicit
    // selector wins in both directions, and only the declaration is consulted
    // otherwise.
    assert_eq!(
        BringUpPolicy::from_value(Some("managed")),
        BringUpPolicy::Managed
    );
    assert_eq!(
        BringUpPolicy::from_value(Some("embedded")),
        BringUpPolicy::Embedded
    );
    assert_eq!(BringUpPolicy::from_value(None), BringUpPolicy::Embedded);
}
