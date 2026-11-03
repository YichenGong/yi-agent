//! Which bring-up path a build uses, and which one it must not yet use.
//!
//! The default is the load-bearing assertion here. Shipping the managed path as the
//! default before it has been validated end to end would make every user the test of
//! it, and the failure mode is not a crash -- it is a runtime that starts under one
//! configuration and is reused under another.

use yi_agent_subagent::attach::BringUpPolicy;

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
