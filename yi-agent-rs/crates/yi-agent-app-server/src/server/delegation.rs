//! The app-server's view of per-turn session delegation.
//!
//! The machinery lives in [`yi_agent_subagent::delegation`] because the TUI and
//! `run` need the same ordering and must not grow a second copy of it. What stays
//! here is only what is specific to this host: how a failure is shaped for the
//! app-server's own `thread/delegationStatus/updated` notification.
//!
//! One rule is easy to get wrong and is pinned by a test below: a guardian's
//! "no" is a decision about this host, not an absence of guardian, so it must
//! never be reported as (or replaced by) embedded permission.

pub(crate) use yi_agent_subagent::delegation::{DelegationFailure, PreparedTurn, ThreadDelegation};

/// The wire shape the client branches on: a stable code plus a human reason.
///
/// A free function rather than a method on `DelegationFailure`: that type lives in
/// `yi-agent-subagent`, and an inherent `impl` here would be an orphan.
pub(crate) fn payload(failure: &DelegationFailure) -> crate::protocol::DelegationFailurePayload {
    crate::protocol::DelegationFailurePayload {
        code: failure.code(),
        reason: failure.reason().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused turn is never reported as ready, and keeps the daemon's reason.
    #[test]
    fn a_failure_is_reported_rather_than_degraded_silently() {
        let refused = DelegationFailure::from_rejected(
            yi_agent_lifecycle::session_delegation::SessionFailure::new(
                yi_agent_lifecycle::session_delegation::SessionFailureCode::SessionClosed,
                "closed by the user",
            ),
        );
        let payload = super::payload(&refused);
        assert_eq!(payload.code, "session_closed");
        assert_eq!(payload.reason, "closed by the user");
    }

    /// A guardian's refusal is a decision, not a missing feature.
    ///
    /// The bug this pins: the authority probe used to answer `None` for both "no
    /// guardian" and "the guardian refused", so a refused host fell back to its
    /// embedded resume secret. A policy "no" then became permission.
    #[test]
    fn a_refused_authority_is_not_downgraded_to_embedded() {
        use yi_agent_subagent::thread_root::ForegroundAuthority;
        assert_eq!(
            ForegroundAuthority::Refused("policy said no".into()).into_authorization(),
            None,
            "a refusal must not yield an authorization"
        );
        assert_eq!(
            ForegroundAuthority::Embedded.into_authorization(),
            None,
            "embedded hosts authorize nothing at the guardian"
        );
        // Still distinguishable, which is the point: only the refusal has a
        // reason, and the driver reports it instead of trying the secret.
        assert_ne!(
            ForegroundAuthority::Embedded,
            ForegroundAuthority::Refused("policy said no".into())
        );
    }
}
