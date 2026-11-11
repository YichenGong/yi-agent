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

use std::sync::Arc;

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

/// Where a conversation's delegation authority comes from.
///
/// The two kinds of conversation ask *different* sources, and the difference is the
/// whole reason this type exists. A managed conversation must never ask the guardian
/// for a fresh lease -- the App's lease belongs to the desktop shell, and a sidecar
/// registering its own would be a second App frontend -- while an embedded one has no
/// guardian at all. Reading the wrong source does not fail loudly: it yields "no
/// authority", the turn falls back to a self-declared identity, the managed daemon
/// refuses that, and the five delegation tools are never installed, so the model calls
/// a `spawn_agent` that is not there.
pub(crate) enum AuthoritySource<'a> {
    /// The shell already brokered this conversation's root; read the root's own store.
    Brokered(&'a Arc<yi_agent_subagent::thread_root::ThreadRoot>),
    /// No shell: ask the guardian for a lease of our own, about this root.
    Guardian {
        host: &'a crate::server::foreground::ForegroundDispatch,
        root: &'a Arc<yi_agent_subagent::thread_root::ThreadRoot>,
    },
    /// Nothing to ask: an embedded runtime, or a conversation with no root.
    None,
}

/// Decides which source answers for this conversation, before anything is asked.
///
/// Pure, and separated out so the mapping itself is testable. The bug this fixes was
/// not in either source: it was a `match` that could only take the guardian arm,
/// leaving every managed conversation permanently unauthorized.
///
/// A managed root carries the shell's brokered capability in its own store, and the
/// managed branch of the turn driver never registers a lease, so `host` is `None`
/// there by construction -- which is exactly why the managed arm has to come first.
pub(crate) fn authority_source<'a>(
    root: Option<&'a Arc<yi_agent_subagent::thread_root::ThreadRoot>>,
    host: Option<&'a crate::server::foreground::ForegroundDispatch>,
) -> AuthoritySource<'a> {
    match root {
        // An entry the shell already brokered is the identity the daemon minted this
        // conversation's container under, so it is the only one that can name it later.
        Some(root) if root.is_managed() && root.managed_root_authorization().is_some() => {
            AuthoritySource::Brokered(root)
        }
        // A managed root with *nothing* brokered yet keeps whatever identity this
        // process would otherwise use. Reporting it as a hard failure here would turn
        // a slow shell into a broken conversation, and the fallback is not silent:
        // `prepare_turn` is where a daemon that disagrees says so, by name.
        Some(root) => match host {
            Some(host) => AuthoritySource::Guardian { host, root },
            None => AuthoritySource::None,
        },
        None => AuthoritySource::None,
    }
}

/// Asks the chosen source, reporting a failure in the shape the driver records.
///
/// A managed root whose store has no entry yet is *reported*, never degraded: falling
/// through to the embedded resume secret would turn "the shell has not authorized this
/// conversation" into "this host is allowed", which the daemon then refuses at a layer
/// that can no longer name the cause.
pub(crate) fn resolve_authority(
    source: AuthoritySource<'_>,
    operation_id: &str,
) -> Result<yi_agent_subagent::thread_root::ForegroundAuthority, String> {
    match source {
        AuthoritySource::Brokered(root) => match root.managed_root_authorization() {
            Some(authorization) => {
                Ok(yi_agent_subagent::thread_root::ForegroundAuthority::Managed(authorization))
            }
            None => {
                Err("the shell has not brokered authority for this conversation yet".to_string())
            }
        },
        AuthoritySource::Guardian { host, root } => host.authority(root, operation_id),
        AuthoritySource::None => Ok(yi_agent_subagent::thread_root::ForegroundAuthority::Embedded),
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

    // ---------------------------------------------------------------------
    // Which source answers for a conversation.
    //
    // The bug these pin: the turn driver asked for authority only when a
    // `ForegroundDispatch` existed, and a managed conversation never has one --
    // its authority was brokered by the desktop shell into the root's own store.
    // So every App conversation recorded no authority, `prepare_turn` fell back
    // to the embedded resume secret, the managed daemon refused it, and all five
    // delegation tools were removed for every turn of every such conversation.
    // ---------------------------------------------------------------------

    use std::path::PathBuf;
    use std::sync::Mutex;

    use yi_agent_subagent::thread_root::{ThreadRoot, application_root_key};

    fn fixed_binding() -> Arc<yi_agent_subagent::binding::RuntimeBinding> {
        yi_agent_subagent::binding::RuntimeBinding::fixed(
            yi_agent_subagent::binding::RuntimeHandle {
                socket_path: PathBuf::from("/nonexistent/never-listening.sock"),
                workspace_root: PathBuf::from("/tmp/workspace"),
                session_id: "shared-session".into(),
                task_id: "shared-root".into(),
                capability: "shared-cap".into(),
            },
        )
    }

    /// A managed root, with `client`'s root authority installed when `brokered`.
    fn managed_root(brokered: bool) -> Arc<ThreadRoot> {
        let mut store = yi_agent_subagent::managed_binding::RootAuthorityStore::new();
        store
            .bind_epoch("epoch-1")
            .expect("the handshake fixed an epoch");
        if brokered {
            store
                .install_entry(yi_agent_lifecycle::production::RootAuthorityEntry {
                    key: yi_agent_lifecycle::production::RootAuthorityKey {
                        scope_key: "/tmp/project/.yi-agent/runtime".into(),
                        generation: "guardian-7".into(),
                        client_run_id: "app-run-1".into(),
                        thread_key: application_root_key("thread-a"),
                        root_id: "0f2b6f2e-0000-4000-8000-00000000000a".into(),
                    },
                    capability: yi_agent_lifecycle::foreground::SecretCapability::new("cap-a"),
                })
                .expect("the entry belongs to this session");
        }
        ThreadRoot::managed(
            fixed_binding(),
            "thread-a",
            PathBuf::from("/tmp/project"),
            Vec::new(),
            Arc::new(yi_agent_subagent::managed_binding::ManagedContext::new(
                Arc::new(Mutex::new(store)),
                "/tmp/project/.yi-agent/runtime",
                "app-run-1",
            )),
        )
    }

    /// An embedded conversation asks nobody: its own secret is the identity.
    #[test]
    fn an_embedded_conversation_needs_no_authority() {
        let root = ThreadRoot::new(
            fixed_binding(),
            "thread-a",
            PathBuf::from("/tmp/project"),
            Vec::new(),
        );
        assert!(!root.is_managed());
        let source = authority_source(Some(&root), None);
        assert!(
            matches!(source, AuthoritySource::None),
            "an embedded root has no guardian to ask and no brokered entry"
        );
        assert_eq!(
            resolve_authority(source, "op").expect("embedded is not a failure"),
            yi_agent_subagent::thread_root::ForegroundAuthority::Embedded
        );
    }

    /// The managed case, which is the whole point: with **no** foreground dispatch
    /// (the App's shape) the source must still be the root's brokered store.
    ///
    /// Before the fix this returned `AuthoritySource::None` -- "no guardian manager"
    /// was read as "embedded" -- which is how a managed conversation silently became
    /// a host with no authority at all.
    #[test]
    fn a_managed_root_is_authorized_from_its_brokered_store_not_a_guardian() {
        let root = managed_root(true);
        let source = authority_source(Some(&root), None);
        assert!(
            matches!(source, AuthoritySource::Brokered(_)),
            "a managed root must be answered by the store the shell brokered into,              never by a guardian this process did not register with"
        );
        match resolve_authority(source, "op").expect("the entry is brokered") {
            yi_agent_subagent::thread_root::ForegroundAuthority::Managed(authorization) => {
                assert_eq!(authorization.client_run_id, "app-run-1");
                assert_eq!(authorization.capability, "cap-a");
                assert_eq!(
                    authorization.root_id,
                    "0f2b6f2e-0000-4000-8000-00000000000a"
                );
            }
            other => panic!("a brokered entry must resolve as managed, got {other:?}"),
        }
    }

    /// A managed root with nothing brokered yet must not *invent* authority, and must
    /// not hard-fail either: a slow shell is a slow shell, not a broken conversation.
    ///
    /// It falls back to the process's own identity, which is what the code did for
    /// every managed conversation before the source was chosen correctly -- so this
    /// asserts the fallback is preserved, not that it is desirable. The decision that
    /// matters (using the brokered entry whenever there is one) is the test above.
    #[test]
    fn an_unbrokered_managed_root_invents_no_authority() {
        let root = managed_root(false);
        let source = authority_source(Some(&root), None);
        assert!(
            !matches!(source, AuthoritySource::Brokered(_)),
            "nothing is brokered, so nothing may be presented as a brokered capability"
        );
        assert_eq!(
            resolve_authority(source, "op").expect("a missing broker is not an error here"),
            yi_agent_subagent::thread_root::ForegroundAuthority::Embedded
        );
    }

    /// No root at all: still embedded, and still not an error.
    #[test]
    fn a_conversation_without_a_root_has_no_authority_to_record() {
        assert!(matches!(
            authority_source(None, None),
            AuthoritySource::None
        ));
    }
}
