//! The `on_exit` policy boundary, exercised through the public API.

use yi_agent_tools::{ManagedProcessPolicy, validate_on_exit};

#[test]
fn managed_keep_is_rejected_without_silent_conversion() {
    let policy = ManagedProcessPolicy::Frontend {
        client_run_id: "app".into(),
    };
    assert!(validate_on_exit(&policy, "keep").is_err());
    assert!(validate_on_exit(&policy, "kill").is_ok());
    assert!(validate_on_exit(&ManagedProcessPolicy::Unmanaged, "keep").is_ok());
}

#[test]
fn a_managed_refusal_stops_the_process_from_being_started_at_all() {
    // The point of checking at the boundary: nothing has been spawned yet, so a
    // refusal cannot leave a process behind.
    let policy = ManagedProcessPolicy::Frontend {
        client_run_id: "tui".into(),
    };
    let refused = validate_on_exit(&policy, "keep").unwrap_err();
    assert!(refused.contains("tui"), "{refused}");
}
