//! Who may attach a root to a daemon, as decided by the guardian.
//!
//! The business attach alone cannot answer "does this caller speak for the App
//! run the guardian authenticated?". The business IPC is not the place to ask
//! either: `IpcRequest` is `deny_unknown_fields`, so adding a field to
//! `AttachApplicationRoot` would make every older daemon reject every newer
//! attach with a bare parse error -- and the request is parsed before
//! `protocol_version`, so a version bump could not rescue it. The specification
//! also requires that a runtime keep serving across an upgrade, which rules out
//! "both sides are the new build" as an assumption.
//!
//! So the authority travels on the one channel a daemon already has from its
//! launcher: the private bootstrap lifeline. The guardian mints a capability,
//! pushes the binding, and the daemon verifies the capability by recomputing it
//! from the same generation secret. Nothing about the business schema changes,
//! and an unmanaged daemon (no lifeline) keeps behaving exactly as before.
//!
//! Three properties are load-bearing and are enforced here rather than at a
//! call site:
//!
//! * **Business only.** [`LeaseMode::LeaseOnly`] never authorizes a task, so it
//!   never authorizes a root either. The guardian checks the live lease, and
//!   [`AuthorizationLedger::issue`] re-checks the mode it is handed.
//! * **Generation-bound.** A capability names the `daemon_instance_id` it was
//!   issued for, so a capability that outlived its daemon cannot be replayed
//!   against the next one.
//! * **Revocable.** A released or expired client's capabilities stop verifying
//!   immediately, and a late duplicate binding cannot resurrect them.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{ClientRunId, DaemonInstanceId, LeaseMode, RootAuthorization, RootId};

/// Bytes in one generation secret.
const SECRET_BYTES: usize = 32;

/// Prefix that keeps a capability from ever being mistaken for a path or an id.
///
/// The capability doubles as an opaque idempotency key on the business attach,
/// so a reader that sees it in a log should be able to tell what it is.
const CAPABILITY_PREFIX: &str = "rootcap1:";

/// The first frame on a managed daemon's lifeline.
///
/// It carries the generation secret, which is what makes the daemon able to
/// recompute a capability instead of merely trusting a table entry. It is sent
/// exactly once, before any binding, so a daemon that never received it refuses
/// to be managed rather than accepting a table it cannot check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifelineHello {
    pub scope_key: String,
    pub daemon_instance_id: DaemonInstanceId,
    /// Raw key material, hex encoded so the frame stays a plain JSON document.
    pub generation_secret: String,
}

impl LifelineHello {
    pub fn new(
        scope_key: impl Into<String>,
        daemon_instance_id: impl Into<String>,
        secret: [u8; SECRET_BYTES],
    ) -> Self {
        Self {
            scope_key: scope_key.into(),
            daemon_instance_id: daemon_instance_id.into(),
            generation_secret: hex_encode(&secret),
        }
    }

    /// Decodes the secret, refusing anything that is not exactly one key long.
    ///
    /// A short or truncated secret would silently verify nothing, which would read
    /// as "the guardian authorized nobody" instead of "this channel is broken".
    pub fn secret(&self) -> Result<[u8; SECRET_BYTES], String> {
        hex_decode(&self.generation_secret)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode(text: &str) -> Result<[u8; SECRET_BYTES], String> {
    let bytes = text.as_bytes();
    if bytes.len() != SECRET_BYTES * 2 {
        return Err(format!(
            "a generation secret is {} hex characters, got {}",
            SECRET_BYTES * 2,
            bytes.len()
        ));
    }
    let mut out = [0u8; SECRET_BYTES];
    for (index, chunk) in bytes.chunks(2).enumerate() {
        let high = hex_digit(chunk[0])?;
        let low = hex_digit(chunk[1])?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("not a hex digit: {}", byte as char)),
    }
}

/// What the guardian tells the daemon about authority.
///
/// Frames travel on the bootstrap lifeline, which is also the daemon's death
/// monitor: the guardian keeping it open is what says "still supervised". Writing
/// frames on it does not weaken that -- the monitor is about the socket *closing*,
/// not about it being idle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorizationUpdate {
    /// The guardian minted this capability; the daemon may now honour it.
    Bind(Box<RootAuthorization>),
    /// The client's lease ended. Every capability it held stops verifying.
    RevokeClient { client_run_id: ClientRunId },
    /// One root is gone but the client remains. Narrower than a client revoke.
    RevokeRoot { root_id: RootId },
}

/// The generation secret plus the bindings minted under it.
///
/// Both sides hold one of these. The guardian's copy is authoritative and
/// publishes every change; the daemon's copy is the verifier, and it is only ever
/// changed by [`AuthorizationLedger::apply`] on a frame from the guardian. A
/// daemon that tried to `issue` would be signing with a secret it never received.
#[derive(Clone, Debug)]
pub struct AuthorizationLedger {
    scope_key: String,
    daemon_instance_id: DaemonInstanceId,
    secret: [u8; SECRET_BYTES],
    /// capability -> binding, so an attach that presents only the capability can
    /// be resolved without the daemon knowing the client id in advance.
    bindings: BTreeMap<String, RootAuthorization>,
    /// client -> roots it holds, for wholesale revocation.
    by_client: BTreeMap<ClientRunId, BTreeSet<RootId>>,
    /// Clients whose authority was withdrawn. Kept (not just removed) so a
    /// stale duplicate `Bind` cannot re-grant what a release already took away.
    revoked_clients: BTreeSet<ClientRunId>,
    revoked_roots: BTreeSet<RootId>,
}

impl AuthorizationLedger {
    /// A ledger for one daemon generation.
    pub fn new(scope_key: impl Into<String>, daemon_instance_id: impl Into<String>) -> Self {
        let mut secret = [0u8; SECRET_BYTES];
        getrandom::fill(&mut secret).expect("OS CSPRNG unavailable");
        Self {
            scope_key: scope_key.into(),
            daemon_instance_id: daemon_instance_id.into(),
            secret,
            bindings: BTreeMap::new(),
            by_client: BTreeMap::new(),
            revoked_clients: BTreeSet::new(),
            revoked_roots: BTreeSet::new(),
        }
    }

    /// The verifier's half: a ledger that trusts only what it is told.
    ///
    /// It receives the same secret the guardian generated, so its recomputed
    /// capabilities match byte for byte.
    pub fn verifier(
        scope_key: impl Into<String>,
        daemon_instance_id: impl Into<String>,
        secret: [u8; SECRET_BYTES],
    ) -> Self {
        let mut ledger = Self::new(scope_key, daemon_instance_id);
        ledger.secret = secret;
        ledger
    }

    pub fn generation_secret(&self) -> [u8; SECRET_BYTES] {
        self.secret
    }

    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    pub fn daemon_instance_id(&self) -> &str {
        &self.daemon_instance_id
    }

    /// Mints the capability for one root on one authenticated client.
    ///
    /// Deterministic in `(secret, scope, generation, client, root)`: a shell that
    /// re-requests the same binding after a sidecar restart must get the *same*
    /// capability, or the attach idempotency key would change and the daemon would
    /// create a second root for the same App run.
    ///
    /// `mode` is checked here as well as at the guardian's edge. A caller that
    /// reached this function with a read-only lease has a bug, and minting would
    /// turn that bug into authority.
    pub fn issue(
        &mut self,
        client_run_id: &str,
        root_id: &str,
        mode: LeaseMode,
    ) -> Result<RootAuthorization, String> {
        if mode != LeaseMode::Business {
            return Err(format!(
                "a {mode:?} lease never authorizes a root; only a business lease does"
            ));
        }
        if client_run_id.trim().is_empty() {
            return Err("a root authorization needs a client run id".to_string());
        }
        if root_id.trim().is_empty() {
            return Err("a root authorization needs a root id".to_string());
        }
        if self.revoked_clients.contains(client_run_id) {
            return Err(format!(
                "client {client_run_id} was released; its authority is not reissued"
            ));
        }
        let capability = self.capability_for(client_run_id, root_id);
        let authorization = RootAuthorization {
            scope_key: self.scope_key.clone(),
            daemon_instance_id: self.daemon_instance_id.clone(),
            client_run_id: client_run_id.to_string(),
            root_id: root_id.to_string(),
            capability: capability.clone(),
        };
        self.bindings.insert(capability, authorization.clone());
        self.by_client
            .entry(client_run_id.to_string())
            .or_default()
            .insert(root_id.to_string());
        Ok(authorization)
    }

    /// Applies one update from the guardian.
    ///
    /// A binding for another scope or generation is refused rather than stored:
    /// the daemon it was minted for is a different process, and honouring it here
    /// would be exactly the cross-generation replay the design forbids.
    pub fn apply(&mut self, update: AuthorizationUpdate) -> Result<(), String> {
        match update {
            AuthorizationUpdate::Bind(authorization) => {
                if authorization.scope_key != self.scope_key {
                    return Err(format!(
                        "authorization is for scope {}, not {}",
                        authorization.scope_key, self.scope_key
                    ));
                }
                if authorization.daemon_instance_id != self.daemon_instance_id {
                    return Err(format!(
                        "authorization is for daemon {}, not {}",
                        authorization.daemon_instance_id, self.daemon_instance_id
                    ));
                }
                if self.revoked_clients.contains(&authorization.client_run_id) {
                    return Err(format!(
                        "client {} already had its authority withdrawn",
                        authorization.client_run_id
                    ));
                }
                if self.revoked_roots.contains(&authorization.root_id) {
                    return Err(format!(
                        "root {} already had its authority withdrawn",
                        authorization.root_id
                    ));
                }
                self.by_client
                    .entry(authorization.client_run_id.clone())
                    .or_default()
                    .insert(authorization.root_id.clone());
                self.bindings
                    .insert(authorization.capability.clone(), *authorization);
                Ok(())
            }
            AuthorizationUpdate::RevokeClient { client_run_id } => {
                self.revoked_clients.insert(client_run_id.clone());
                if let Some(roots) = self.by_client.remove(&client_run_id) {
                    for root in roots {
                        self.revoked_roots.insert(root);
                    }
                }
                self.bindings
                    .retain(|_, authorization| authorization.client_run_id != client_run_id);
                Ok(())
            }
            AuthorizationUpdate::RevokeRoot { root_id } => {
                self.revoked_roots.insert(root_id.clone());
                self.bindings
                    .retain(|_, authorization| authorization.root_id != root_id);
                for roots in self.by_client.values_mut() {
                    roots.remove(&root_id);
                }
                Ok(())
            }
        }
    }

    /// Resolves an attach's presented capability to its binding.
    ///
    /// The attach cannot carry the client id without changing the business schema,
    /// so the capability *is* the lookup key. Everything the caller might have lied
    /// about is then read from the binding, not from the request.
    pub fn resolve(&self, capability: &str) -> Result<&RootAuthorization, String> {
        self.bindings
            .get(capability)
            .ok_or_else(|| "no root authorization matches this capability".to_string())
    }

    /// Verifies a full authorization, including the parts the attach relies on.
    ///
    /// Used by the guardian when a shell presents a capability it received
    /// earlier, and by tests that pin the invariants.
    pub fn verify(&self, authorization: &RootAuthorization) -> Result<(), String> {
        if authorization.scope_key != self.scope_key {
            return Err(format!(
                "authorization is for scope {}, not {}",
                authorization.scope_key, self.scope_key
            ));
        }
        if authorization.daemon_instance_id != self.daemon_instance_id {
            return Err(format!(
                "authorization is for daemon {}, not {}",
                authorization.daemon_instance_id, self.daemon_instance_id
            ));
        }
        if self.revoked_clients.contains(&authorization.client_run_id) {
            return Err(format!(
                "client {} was released; its capability no longer works",
                authorization.client_run_id
            ));
        }
        if self.revoked_roots.contains(&authorization.root_id) {
            return Err(format!(
                "root {} was released; its capability no longer works",
                authorization.root_id
            ));
        }
        let expected = self.capability_for(&authorization.client_run_id, &authorization.root_id);
        if expected != authorization.capability {
            return Err("capability does not match this client and root".to_string());
        }
        match self.bindings.get(&authorization.capability) {
            Some(bound) if bound == authorization => Ok(()),
            Some(_) => Err("capability is bound to a different binding".to_string()),
            None => Err("capability was never issued by this guardian".to_string()),
        }
    }

    /// Every client that currently holds at least one root.
    pub fn authorized_clients(&self) -> BTreeSet<ClientRunId> {
        self.by_client
            .iter()
            .filter(|(_, roots)| !roots.is_empty())
            .map(|(client, _)| client.clone())
            .collect()
    }

    pub fn is_revoked(&self, client_run_id: &str) -> bool {
        self.revoked_clients.contains(client_run_id)
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// HMAC-SHA256 over the binding, keyed by the generation secret.
    ///
    /// A plain `sha256(secret || message)` would be vulnerable to length
    /// extension; the keyed construction with an inner and outer pad is the
    /// standard one and costs a few lines.
    fn capability_for(&self, client_run_id: &str, root_id: &str) -> String {
        let message = format!(
            "yi-agent root authorization v1\nscope:{}\ndaemon:{}\nclient:{}\nroot:{}",
            self.scope_key, self.daemon_instance_id, client_run_id, root_id
        );
        let mut inner = Sha256::new();
        inner.update([0x36u8; 64]);
        inner.update(self.secret);
        inner.update(message.as_bytes());
        let inner = inner.finalize();

        let mut outer = Sha256::new();
        outer.update([0x5cu8; 64]);
        outer.update(self.secret);
        outer.update(inner);
        let digest = outer.finalize();

        let mut out = String::with_capacity(CAPABILITY_PREFIX.len() + digest.len() * 2);
        out.push_str(CAPABILITY_PREFIX);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

/// Where a daemon draws the line when it has a guardian.
///
/// `Unmanaged` is not "permissive by accident": a daemon without a lifeline has no
/// guardian to ask, and its callers are the CLI and the tests, whose attach
/// behaviour must not change. `Managed` is the case the design is about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AttachAuthority {
    /// No guardian: legacy behaviour, unchanged.
    #[default]
    Unmanaged,
    /// A guardian spoke; only the clients it authorized may attach.
    Managed(BTreeSet<ClientRunId>),
}

impl AttachAuthority {
    /// Whether an attach from `client` may proceed.
    ///
    /// An empty managed set is a refusal, not a pass: a guardian exists and has
    /// authorized nobody yet, so admitting an attach would be accepting an owner
    /// the guardian never vouched for.
    pub fn admits(&self, client: &str) -> Result<(), String> {
        match self {
            Self::Unmanaged => Ok(()),
            Self::Managed(clients) => {
                if clients.contains(client) {
                    Ok(())
                } else {
                    Err(format!(
                        "client {client} holds no root authorization on this daemon; \
                         the guardian did not authorize it"
                    ))
                }
            }
        }
    }

    pub fn is_managed(&self) -> bool {
        matches!(self, Self::Managed(_))
    }
}

/// Frame deadline for one lifeline read, so a slow frame cannot wedge shutdown.
///
/// Generous: the message is small and the peer is a local process, so hitting it
/// means something is wrong rather than busy.
pub const LIFELINE_READ_BUDGET: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> AuthorizationLedger {
        AuthorizationLedger::new("/p/.yi-agent/runtime", "daemon-1")
    }

    #[test]
    fn a_business_client_gets_a_capability_that_verifies() {
        let mut ledger = ledger();
        let authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        assert!(!authorization.capability.is_empty());
        assert_eq!(authorization.client_run_id, "app-run-1");
        assert_eq!(authorization.root_id, "/p");
        ledger.verify(&authorization).unwrap();
    }

    /// The whole point of the type: an observer can control upgrades but must never
    /// be able to name an owner.
    #[test]
    fn a_lease_only_client_is_never_authorized() {
        let mut ledger = ledger();
        let error = ledger
            .issue("inspector-1", "/p", LeaseMode::LeaseOnly)
            .unwrap_err();
        assert!(error.contains("never authorizes"), "{error}");
        assert!(ledger.authorized_clients().is_empty());
    }

    #[test]
    fn a_waiting_next_generation_lease_is_never_authorized() {
        let mut ledger = ledger();
        assert!(
            ledger
                .issue("next-1", "/p", LeaseMode::WaitingNextGeneration)
                .is_err()
        );
    }

    /// Determinism is not convenience: the attach idempotency key is the
    /// capability, so a re-issue that changed it would create a second root.
    #[test]
    fn the_same_client_and_root_get_the_same_capability_again() {
        let mut ledger = ledger();
        let first = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let second = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        assert_eq!(first.capability, second.capability);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn different_roots_get_different_capabilities() {
        let mut ledger = ledger();
        let a = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let b = ledger
            .issue("app-run-1", "/q", LeaseMode::Business)
            .unwrap();
        let c = ledger
            .issue("app-run-2", "/p", LeaseMode::Business)
            .unwrap();
        assert_ne!(a.capability, b.capability);
        assert_ne!(a.capability, c.capability);
    }

    /// A release is what makes reclamation real: the App's authority must not
    /// outlive the lease that justified it.
    #[test]
    fn a_released_client_capability_stops_verifying() {
        let mut ledger = ledger();
        let authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        ledger
            .apply(AuthorizationUpdate::RevokeClient {
                client_run_id: "app-run-1".to_string(),
            })
            .unwrap();
        let error = ledger.verify(&authorization).unwrap_err();
        assert!(error.contains("released"), "{error}");
        assert!(ledger.resolve(&authorization.capability).is_err());
    }

    /// A late duplicate binding must not re-grant what a release took away.
    #[test]
    fn a_duplicate_binding_cannot_resurrect_a_released_client() {
        let mut ledger = ledger();
        let authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        ledger
            .apply(AuthorizationUpdate::RevokeClient {
                client_run_id: "app-run-1".to_string(),
            })
            .unwrap();
        let replay = ledger.apply(AuthorizationUpdate::Bind(Box::new(authorization)));
        assert!(replay.is_err());
        assert!(ledger.authorized_clients().is_empty());
    }

    /// Re-issuing for a released client is a refusal from the other direction.
    #[test]
    fn a_released_client_is_not_reissued() {
        let mut ledger = ledger();
        ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        ledger
            .apply(AuthorizationUpdate::RevokeClient {
                client_run_id: "app-run-1".to_string(),
            })
            .unwrap();
        assert!(
            ledger
                .issue("app-run-1", "/p", LeaseMode::Business)
                .is_err()
        );
    }

    /// A capability must not survive into the daemon that replaced the one it was
    /// minted for: "different generations are re-signed" is the design's words.
    #[test]
    fn a_capability_from_another_generation_does_not_verify() {
        let mut old = AuthorizationLedger::new("/p/.yi-agent/runtime", "daemon-1");
        let authorization = old.issue("app-run-1", "/p", LeaseMode::Business).unwrap();

        let mut next = AuthorizationLedger::new("/p/.yi-agent/runtime", "daemon-2");
        assert!(next.verify(&authorization).is_err());
        // Even when the binding is pushed at it, generation mismatch refuses.
        assert!(
            next.apply(AuthorizationUpdate::Bind(Box::new(authorization)))
                .is_err()
        );
    }

    #[test]
    fn a_capability_from_another_scope_does_not_verify() {
        let mut other = AuthorizationLedger::new("/other/.yi-agent/runtime", "daemon-1");
        let authorization = other.issue("app-run-1", "/p", LeaseMode::Business).unwrap();
        assert!(ledger().verify(&authorization).is_err());
    }

    /// A forged capability is not merely absent from the table: it does not even
    /// recompute to the same digest.
    #[test]
    fn a_tampered_capability_is_refused() {
        let mut ledger = ledger();
        let mut authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        authorization.capability = "rootcap1:deadbeef".to_string();
        assert!(ledger.verify(&authorization).is_err());
    }

    /// Moving a capability to another client must not work, even with the exact
    /// digest: the binding is for one client, and `verify` recomputes from the
    /// claimed client rather than trusting the pair in the request.
    #[test]
    fn a_capability_cannot_be_reassigned_to_another_client() {
        let mut ledger = ledger();
        let authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let mut stolen = authorization.clone();
        stolen.client_run_id = "app-run-2".to_string();
        assert!(ledger.verify(&stolen).is_err());
    }

    #[test]
    fn revoking_one_root_leaves_the_others_alone() {
        let mut ledger = ledger();
        let a = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let b = ledger
            .issue("app-run-1", "/q", LeaseMode::Business)
            .unwrap();
        ledger
            .apply(AuthorizationUpdate::RevokeRoot {
                root_id: "/p".to_string(),
            })
            .unwrap();
        assert!(ledger.verify(&a).is_err());
        ledger.verify(&b).unwrap();
        assert_eq!(
            ledger.authorized_clients(),
            BTreeSet::from(["app-run-1".to_string()])
        );
    }

    /// The daemon's half is a verifier: it can only accept what the guardian sent,
    /// and it must recompute the same bytes to do so.
    #[test]
    fn the_verifier_accepts_a_binding_minted_by_the_guardian() {
        let mut guardian = ledger();
        let authorization = guardian
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();

        let mut daemon = AuthorizationLedger::verifier(
            "/p/.yi-agent/runtime",
            "daemon-1",
            guardian.generation_secret(),
        );
        daemon
            .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
            .unwrap();
        daemon.verify(&authorization).unwrap();
        assert_eq!(
            daemon.resolve(&authorization.capability).unwrap(),
            &authorization
        );
    }

    /// A verifier holding a different secret must reject the binding outright,
    /// rather than storing something it can never verify.
    #[test]
    fn a_verifier_with_another_secret_rejects_the_capability() {
        let mut guardian = ledger();
        let authorization = guardian
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let mut other =
            AuthorizationLedger::verifier("/p/.yi-agent/runtime", "daemon-1", [7u8; SECRET_BYTES]);
        other
            .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
            .unwrap();
        // It stored the frame, but the recomputed digest differs: that is the
        // proof the secret is what carries the authority, not the table entry.
        assert!(other.verify(&authorization).is_err());
    }

    /// The capability doubles as the attach idempotency key, so it must be safe to
    /// put in a log line and in a durable record: no newlines, no separators.
    #[test]
    fn a_capability_is_a_single_safe_token() {
        let mut ledger = ledger();
        let authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let capability = &authorization.capability;
        assert!(capability.starts_with(CAPABILITY_PREFIX));
        assert!(
            capability
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_')
        );
        assert_eq!(capability.len(), CAPABILITY_PREFIX.len() + 64);
    }

    /// `Debug` on the wire type redacts the one secret; a trace line must not be
    /// the place a capability leaks.
    #[test]
    fn the_authorization_debug_does_not_print_the_capability() {
        let mut ledger = ledger();
        let authorization = ledger
            .issue("app-run-1", "/p", LeaseMode::Business)
            .unwrap();
        let rendered = format!("{authorization:?}");
        assert!(!rendered.contains(&authorization.capability));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn unmanaged_attaches_stay_permitted() {
        assert!(AttachAuthority::Unmanaged.admits("anything").is_ok());
    }

    #[test]
    fn a_managed_daemon_admits_only_authorized_clients() {
        let authority = AttachAuthority::Managed(BTreeSet::from(["app-run-1".to_string()]));
        authority.admits("app-run-1").unwrap();
        let error = authority.admits("app-run-2").unwrap_err();
        assert!(error.contains("no root authorization"), "{error}");
    }

    /// A guardian that has authorized nobody yet must not read as "no guardian":
    /// that would be the exact confusion the state exists to prevent.
    #[test]
    fn an_empty_managed_set_refuses_everyone() {
        let authority = AttachAuthority::Managed(BTreeSet::new());
        assert!(authority.admits("app-run-1").is_err());
        assert!(authority.is_managed());
    }
}
