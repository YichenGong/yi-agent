//! Per-conversation root authority on a managed runtime.
//!
//! Two things live here, and they are deliberately separate from the attach path:
//!
//! * [`RootAuthorityStore`] -- what the shell has told this sidecar it may attach,
//!   keyed by the exact five-part [`RootAuthorityKey`]. It is per session, never a
//!   process-global path map: two App generations, or two projects, must not be able
//!   to overwrite each other's answers.
//! * [`ManagedRootAuthority`] -- the trait a conversation uses to *ask* for one more
//!   root (the shell brokers it to the guardian). The app-server implements it by
//!   forwarding over the private management channel; the CLI implements it with its
//!   own `FrontendLease`. Neither invents authority locally.
//!
//! Why the store is keyed rather than path-indexed: a root id is a *daemon root
//! identity*, not a directory. Two conversations in one project have different root
//! ids, and a lookup by working directory would hand the second conversation the
//! first one's capability -- which is exactly the "one project root authorizes every
//! session" mistake the managed path must not repeat. There is consequently no
//! `get(path)`: a caller that only knows a directory has nothing to attach with.

use std::collections::BTreeMap;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::foreground::{RootBindIntent, SecretCapability};
use yi_agent_lifecycle::production::{
    RootAuthorityEntry, RootAuthorityKey, RootAuthoritySnapshot, RootIntentRequest,
};

/// Brokers one root intent to whoever actually holds the lease.
///
/// `Err` is a *typed refusal*, not "no authority exists": the caller must be able to
/// tell "the shell has no lease for this scope" from "you may not start work yet",
/// because only the first is a deployment fact and only the second is retryable.
pub trait ManagedRootAuthority: Send + Sync {
    /// Asks for authority to bind exactly one conversation root.
    fn authorize(
        &self,
        intent: &RootIntentRequest,
        deadline: Deadline,
    ) -> Result<RootAuthorityEntry, String>;
}

/// The authorizations the shell installed for this session.
///
/// Revisions are **per scope**, so updating project A can never invalidate what
/// project B was told. An empty `entries` list is a real answer: it revokes every
/// authorization for that scope (the user switched a build, so the old capabilities
/// name a generation that is gone).
#[derive(Debug, Default)]
pub struct RootAuthorityStore {
    /// The epoch fixed by the authenticated bootstrap.
    ///
    /// Never adopted from a snapshot. If it were, a compromised or stale sender
    /// could move the session to an epoch of its choosing and then install whatever
    /// it liked under it; the whole point of an epoch is that it is decided once, by
    /// the handshake that was authenticated.
    epoch: Option<String>,
    scopes: BTreeMap<String, ScopeAuthority>,
}

#[derive(Debug)]
struct ScopeAuthority {
    revision: u64,
    /// The generation every entry in this snapshot belongs to.
    ///
    /// Recorded once per scope rather than per entry because a snapshot describes
    /// one runtime: mixing generations in one snapshot would mean the shell was
    /// describing two daemons at once, and a capability could then be matched
    /// against a generation that is not the one serving the project.
    generation: String,
    /// Keyed by the full five-part key, so a lookup cannot ignore the client, the
    /// generation or the conversation.
    entries: BTreeMap<RootAuthorityKey, SecretCapability>,
}

impl RootAuthorityStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fixes the epoch from the authenticated bootstrap.
    ///
    /// Second call with a different value is refused rather than silently replacing
    /// it: two bootstraps on one session means one of them is not this session's
    /// handshake, and guessing which would defeat the purpose.
    pub fn bind_epoch(&mut self, epoch: impl Into<String>) -> Result<(), String> {
        let epoch = epoch.into();
        if epoch.trim().is_empty() {
            return Err("the bootstrap epoch is empty".to_string());
        }
        match &self.epoch {
            Some(existing) if existing == &epoch => Ok(()),
            Some(existing) => Err(format!(
                "this session was bootstrapped under epoch {existing}, not {epoch}"
            )),
            None => {
                self.epoch = Some(epoch);
                Ok(())
            }
        }
    }

    /// The epoch this session is bound to, once bootstrapped.
    pub fn epoch(&self) -> Option<&str> {
        self.epoch.as_deref()
    }

    /// Installs one scope's snapshot, atomically.
    ///
    /// Refused, leaving the previous snapshot intact in every case:
    ///
    /// * the snapshot's epoch is not this session's (a stale or foreign sender);
    /// * its revision is not newer than what this scope already has (a replayed or
    ///   out-of-order snapshot must not resurrect a capability that was revoked);
    /// * any entry names another scope or another client. A partially-matching
    ///   snapshot is refused wholesale rather than filtered: dropping the bad entry
    ///   would leave the session believing it installed what the shell sent.
    pub fn replace(&mut self, snapshot: RootAuthoritySnapshot) -> Result<(), String> {
        let Some(epoch) = self.epoch.as_deref() else {
            return Err("no bootstrap epoch is fixed, so no snapshot can be trusted".to_string());
        };
        if snapshot.sidecar_epoch != epoch {
            return Err(format!(
                "the snapshot is for sidecar epoch {}, not {epoch}",
                snapshot.sidecar_epoch
            ));
        }
        if snapshot.scope_key.trim().is_empty() {
            return Err("the snapshot names no scope".to_string());
        }

        if let Some(existing) = self.scopes.get(&snapshot.scope_key) {
            if snapshot.revision <= existing.revision {
                return Err(format!(
                    "scope {} already has revision {}, which is not older than {}",
                    snapshot.scope_key, existing.revision, snapshot.revision
                ));
            }
        }

        // The snapshot's own generation comes from its entries. An empty snapshot
        // keeps the previous generation's name so that a later lookup for it cannot
        // accidentally match a capability installed for a newer daemon.
        let previous_generation = self
            .scopes
            .get(&snapshot.scope_key)
            .map(|scope| scope.generation.clone());
        let mut generation: Option<String> = None;
        let mut entries = BTreeMap::new();
        for entry in snapshot.entries {
            if entry.key.scope_key != snapshot.scope_key {
                return Err(format!(
                    "an entry for scope {} was installed under {}",
                    entry.key.scope_key, snapshot.scope_key
                ));
            }
            if entry.key.client_run_id.trim().is_empty() {
                return Err("an entry names no client".to_string());
            }
            if entry.key.thread_key.trim().is_empty() {
                return Err("an entry names no conversation".to_string());
            }
            if entry.key.generation.trim().is_empty() {
                return Err("an entry names no runtime generation".to_string());
            }
            if entry.key.root_id.trim().is_empty() {
                return Err("an entry names no root".to_string());
            }
            match &generation {
                Some(existing) if existing != &entry.key.generation => {
                    return Err(format!(
                        "one snapshot named two runtime generations ({existing} and {})",
                        entry.key.generation
                    ));
                }
                Some(_) => {}
                None => generation = Some(entry.key.generation.clone()),
            }
            if entry.capability.expose().trim().is_empty() {
                return Err("an entry carries no capability".to_string());
            }
            entries.insert(entry.key, entry.capability);
        }

        // Only now is the previous snapshot replaced. Everything above is a check, so
        // a refusal leaves the session exactly as it was.
        let generation = generation.or(previous_generation).unwrap_or_default();
        self.scopes.insert(
            snapshot.scope_key,
            ScopeAuthority {
                revision: snapshot.revision,
                generation,
                entries,
            },
        );
        Ok(())
    }

    /// Installs one authorization the shell brokered for this session.
    ///
    /// A snapshot is what the shell *pushes*; this is what one conversation receives
    /// back from an authorization round trip, so it has to be accepted incrementally
    /// and atomically. The two rules that make that safe:
    ///
    /// * the entry's scope and client must be this session's -- an entry for another
    ///   project or another App run is refused rather than filed under a scope it does
    ///   not belong to;
    /// * a newer generation replaces the scope's entries instead of joining them. A
    ///   capability minted for a replaced daemon names a root that no longer exists,
    ///   and keeping it would let a later attach present it and fail at the daemon
    ///   instead of at the lookup.
    ///
    /// The revision moves forward by one per install, so an out-of-order push cannot
    /// be mistaken for newer state.
    pub fn install_entry(&mut self, entry: RootAuthorityEntry) -> Result<(), String> {
        if self.epoch.is_none() {
            return Err("no bootstrap epoch is fixed, so no authority can be trusted".to_string());
        }
        let scope_key = entry.key.scope_key.clone();
        if scope_key.trim().is_empty() {
            return Err("an authority entry names no scope".to_string());
        }
        if entry.key.generation.trim().is_empty() {
            return Err("an authority entry names no runtime generation".to_string());
        }
        if entry.key.client_run_id.trim().is_empty() {
            return Err("an authority entry names no client".to_string());
        }
        if entry.key.thread_key.trim().is_empty() {
            return Err("an authority entry names no conversation".to_string());
        }
        if entry.key.root_id.trim().is_empty() {
            return Err("an authority entry names no root".to_string());
        }
        match self.scopes.get_mut(&scope_key) {
            Some(scope) if scope.generation == entry.key.generation => {
                scope.revision = scope.revision.saturating_add(1);
                scope.entries.insert(entry.key, entry.capability);
            }
            Some(previous) => {
                // A new generation: everything the old one authorized is gone.
                let revision = previous.revision.saturating_add(1);
                let mut entries = BTreeMap::new();
                let generation = entry.key.generation.clone();
                entries.insert(entry.key, entry.capability);
                self.scopes.insert(
                    scope_key,
                    ScopeAuthority {
                        revision,
                        generation,
                        entries,
                    },
                );
            }
            None => {
                let mut entries = BTreeMap::new();
                let generation = entry.key.generation.clone();
                entries.insert(entry.key, entry.capability);
                self.scopes.insert(
                    scope_key,
                    ScopeAuthority {
                        revision: 1,
                        generation,
                        entries,
                    },
                );
            }
        }
        Ok(())
    }

    /// The capability for exactly this key, if the shell installed one.
    ///
    /// Exact match on all five parts. In particular the generation must be the
    /// *current* one: a capability minted for a replaced daemon names a binding that
    /// no longer exists, and presenting it would be an unattributed attach.
    pub fn get(&self, key: &RootAuthorityKey) -> Option<&SecretCapability> {
        self.scopes.get(&key.scope_key)?.entries.get(key)
    }

    /// The capability for one conversation, looked up without knowing its root id.
    ///
    /// The caller (a conversation) knows its scope, the client it belongs to and its
    /// own stable thread key. It does *not* know the daemon's root id: that is minted
    /// by the guardian, which is the only party that may decide it. Matching on the
    /// three known parts plus the scope's current generation is what lets the
    /// conversation find its own entry without being able to name another's.
    ///
    /// At most one entry can match: a conversation has one root per generation, and
    /// the store is keyed by the full tuple.
    pub fn capability_for(
        &self,
        scope_key: &str,
        client_run_id: &str,
        thread_key: &str,
    ) -> Option<&SecretCapability> {
        let scope = self.scopes.get(scope_key)?;
        if scope.generation.is_empty() {
            return None;
        }
        scope
            .entries
            .iter()
            .find(|(key, _)| {
                key.client_run_id == client_run_id
                    && key.thread_key == thread_key
                    && key.generation == scope.generation
            })
            .map(|(_, capability)| capability)
    }

    /// The entry for one conversation, root id included.
    ///
    /// [`Self::capability_for`] answers "may I attach?"; this answers "attach as
    /// *which* root?", which is what lets the caller check the daemon's actual root id
    /// against the one the shell authorized. The root id stays inside the trusted path:
    /// a caller that only knows its project directory cannot name it.
    pub fn entry_for(
        &self,
        scope_key: &str,
        client_run_id: &str,
        thread_key: &str,
    ) -> Option<RootAuthorityEntry> {
        let scope = self.scopes.get(scope_key)?;
        if scope.generation.is_empty() {
            return None;
        }
        scope
            .entries
            .iter()
            .find(|(key, _)| {
                key.client_run_id == client_run_id
                    && key.thread_key == thread_key
                    && key.generation == scope.generation
            })
            .map(|(key, capability)| RootAuthorityEntry {
                key: key.clone(),
                capability: capability.clone(),
            })
    }

    /// The generation this scope's installed snapshot names.
    pub fn generation(&self, scope_key: &str) -> Option<&str> {
        self.scopes
            .get(scope_key)
            .map(|scope| scope.generation.as_str())
            .filter(|generation| !generation.is_empty())
    }

    /// Drops one conversation's capability, leaving the rest of the scope alone.
    ///
    /// Used when a root's own readiness is invalidated: the other conversations in
    /// the same project are unaffected, and revoking the whole scope would stop work
    /// that never misbehaved.
    pub fn revoke(&mut self, key: &RootAuthorityKey) -> Option<SecretCapability> {
        self.scopes.get_mut(&key.scope_key)?.entries.remove(key)
    }

    /// Every key currently installed for `scope_key`, for diagnostics and tests.
    pub fn keys(&self, scope_key: &str) -> Vec<RootAuthorityKey> {
        self.scopes
            .get(scope_key)
            .map(|scope| scope.entries.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The revision this scope is at, if any.
    pub fn revision(&self, scope_key: &str) -> Option<u64> {
        self.scopes.get(scope_key).map(|scope| scope.revision)
    }
}

/// Why a managed conversation could not obtain authority for its root.
///
/// Typed rather than a string because the managed path must never degrade to legacy
/// semantics: a caller that received a plain `String` would have to guess whether the
/// conversation should keep running without activity reporting. Each variant names a
/// different deployment fact, and only `BrokerRefused` is worth retrying as-is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedAdmissionError {
    /// The session has no authenticated bootstrap epoch yet, so nothing the shell
    /// sent can be trusted and nothing may be installed.
    NoBootstrapEpoch,
    /// Nothing on this side can reach the party that holds the lease.
    NoBroker,
    /// The broker answered, and its answer was no.
    BrokerRefused(String),
    /// The capability is for a different project.
    WrongScope { wanted: String, got: String },
    /// The capability is for a different client (another App run).
    WrongClient { wanted: String, got: String },
    /// The capability is for a different conversation.
    WrongThread { wanted: String, got: String },
    /// The daemon minted a root other than the one that was authorized.
    ActualRootMismatch { authorized: String, actual: String },
    /// The store refused to install what came back.
    InstallRefused(String),
}

impl std::fmt::Display for ManagedAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoBootstrapEpoch => write!(
                f,
                "managed admission requires an authenticated bootstrap epoch, and this \
                 session has none"
            ),
            Self::NoBroker => write!(
                f,
                "managed admission requires a lease holder to broker authority, and this \
                 session has none"
            ),
            Self::BrokerRefused(reason) => write!(f, "root authority was refused: {reason}"),
            Self::WrongScope { wanted, got } => {
                write!(f, "root authority is for scope {got}, not {wanted}")
            }
            Self::WrongClient { wanted, got } => {
                write!(f, "root authority is for client {got}, not {wanted}")
            }
            Self::WrongThread { wanted, got } => {
                write!(f, "root authority is for conversation {got}, not {wanted}")
            }
            Self::ActualRootMismatch { authorized, actual } => write!(
                f,
                "the authorized root is {authorized} but the daemon attached {actual}"
            ),
            Self::InstallRefused(reason) => {
                write!(f, "the authority store refused the entry: {reason}")
            }
        }
    }
}

impl std::error::Error for ManagedAdmissionError {}

/// One conversation's view of the authority the shell granted this session.
///
/// The scope and client are fixed when the context is built: they come from the
/// authenticated handshake, not from the request, so a conversation cannot name a
/// different project or a different App run to reach someone else's capability.
pub struct ManagedContext {
    store: std::sync::Arc<std::sync::Mutex<RootAuthorityStore>>,
    scope_key: String,
    client_run_id: String,
    broker: Option<std::sync::Arc<dyn ManagedRootAuthority>>,
}

impl std::fmt::Debug for ManagedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedContext")
            .field("scope_key", &self.scope_key)
            .field("client_run_id", &self.client_run_id)
            .field("has_broker", &self.broker.is_some())
            .finish()
    }
}

impl ManagedContext {
    pub fn new(
        store: std::sync::Arc<std::sync::Mutex<RootAuthorityStore>>,
        scope_key: impl Into<String>,
        client_run_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            scope_key: scope_key.into(),
            client_run_id: client_run_id.into(),
            broker: None,
        }
    }

    /// Attaches the party that can ask for authority (the shell, over the private
    /// management channel).
    pub fn with_broker(mut self, broker: std::sync::Arc<dyn ManagedRootAuthority>) -> Self {
        self.broker = Some(broker);
        self
    }

    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    pub fn client_run_id(&self) -> &str {
        &self.client_run_id
    }

    /// The authority for this conversation's root, asking for one if it is absent.
    ///
    /// The lookup comes first: a re-attach after a lost answer must reuse the authority
    /// the shell already brokered rather than asking for a second root. Only when the
    /// store has nothing for this conversation does the broker run, and what it returns
    /// is validated here before it is installed -- a broker that answered for another
    /// scope, client or conversation has answered for someone else.
    pub fn resolve(
        &self,
        thread_key: &str,
        intent: &RootBindIntent,
    ) -> Result<RootAuthorityEntry, ManagedAdmissionError> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if store.epoch().is_none() {
            return Err(ManagedAdmissionError::NoBootstrapEpoch);
        }
        if let Some(entry) = store.entry_for(&self.scope_key, &self.client_run_id, thread_key) {
            return Ok(entry);
        }
        let Some(broker) = self.broker.as_ref() else {
            return Err(ManagedAdmissionError::NoBroker);
        };
        let epoch = store.epoch().expect("checked above").to_string();
        let request = RootIntentRequest {
            sidecar_epoch: epoch,
            scope_key: self.scope_key.clone(),
            client_run_id: self.client_run_id.clone(),
            intent: intent.clone(),
        };
        // Five seconds: the shell has already prepared the runtime, so this is one
        // guardian round trip, not a bring-up.
        let deadline = Deadline::after(std::time::Duration::from_secs(5));
        let entry = broker
            .authorize(&request, deadline)
            .map_err(ManagedAdmissionError::BrokerRefused)?;
        if entry.key.scope_key != self.scope_key {
            return Err(ManagedAdmissionError::WrongScope {
                wanted: self.scope_key.clone(),
                got: entry.key.scope_key,
            });
        }
        if entry.key.client_run_id != self.client_run_id {
            return Err(ManagedAdmissionError::WrongClient {
                wanted: self.client_run_id.clone(),
                got: entry.key.client_run_id,
            });
        }
        if entry.key.thread_key != thread_key {
            return Err(ManagedAdmissionError::WrongThread {
                wanted: thread_key.to_string(),
                got: entry.key.thread_key,
            });
        }
        store
            .install_entry(entry.clone())
            .map_err(ManagedAdmissionError::InstallRefused)?;
        Ok(entry)
    }

    /// Forgets one conversation's authority by naming the conversation.
    ///
    /// The key is looked up rather than rebuilt by the caller: a key carries five
    /// fields, and a caller that filled in the ones it happened to know (say, only the
    /// root id) would match nothing and revoke nothing -- silently, because a
    /// revocation of an entry that is not there is a no-op.
    pub fn revoke_thread(&self, thread_key: &str) {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(entry) = store.entry_for(&self.scope_key, &self.client_run_id, thread_key) else {
            return;
        };
        store.revoke(&entry.key);
    }

    /// Forgets one conversation's authority, leaving its siblings alone.
    ///
    /// Called when the daemon's actual root id disagrees with the authorization, or
    /// when a root's readiness is invalidated: the entry names something that does not
    /// exist, and keeping it would make every later attach fail the same way.
    pub fn revoke(&self, key: &RootAuthorityKey) {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        store.revoke(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "epoch-1";
    const SCOPE_A: &str = "/projects/a/.yi-agent/runtime";
    const SCOPE_B: &str = "/projects/b/.yi-agent/runtime";
    const CLIENT: &str = "app-run-1";

    fn key(scope: &str, thread: &str, root: &str) -> RootAuthorityKey {
        RootAuthorityKey {
            scope_key: scope.to_string(),
            generation: "gen-1".to_string(),
            client_run_id: CLIENT.to_string(),
            thread_key: thread.to_string(),
            root_id: root.to_string(),
        }
    }

    fn entry(scope: &str, thread: &str, root: &str, secret: &str) -> RootAuthorityEntry {
        RootAuthorityEntry {
            key: key(scope, thread, root),
            capability: SecretCapability::new(secret),
        }
    }

    fn snapshot(
        scope: &str,
        revision: u64,
        entries: Vec<RootAuthorityEntry>,
    ) -> RootAuthoritySnapshot {
        RootAuthoritySnapshot {
            sidecar_epoch: EPOCH.to_string(),
            scope_key: scope.to_string(),
            revision,
            entries,
        }
    }

    fn bound() -> RootAuthorityStore {
        let mut store = RootAuthorityStore::new();
        store.bind_epoch(EPOCH).unwrap();
        store
    }

    /// Two conversations in one project get two capabilities, and each is only
    /// reachable by its own key.
    #[test]
    fn two_roots_in_one_project_stay_distinct() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![
                    entry(SCOPE_A, "thread:a", "root-a", "cap-a"),
                    entry(SCOPE_A, "thread:b", "root-b", "cap-b"),
                ],
            ))
            .unwrap();

        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:a", "root-a"))
                .map(|c| c.expose()),
            Some("cap-a")
        );
        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:b", "root-b"))
                .map(|c| c.expose()),
            Some("cap-b")
        );
        // The wrong conversation in the right project is a different key, so it must
        // not resolve. Otherwise thread b could attach a's root.
        assert!(store.get(&key(SCOPE_A, "thread:b", "root-a")).is_none());
        assert!(store.get(&key(SCOPE_A, "thread:a", "root-b")).is_none());
    }

    /// A key built from the project path instead of the daemon's root id resolves
    /// nothing: root ids are identities, not directories.
    #[test]
    fn a_project_path_is_not_a_root_id() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap();

        let by_directory = RootAuthorityKey {
            scope_key: SCOPE_A.to_string(),
            generation: "gen-1".to_string(),
            client_run_id: CLIENT.to_string(),
            thread_key: "thread:a".to_string(),
            root_id: "/projects/a".to_string(),
        };
        assert!(store.get(&by_directory).is_none());
        assert!(store.get(&key(SCOPE_A, "thread:a", "root-a")).is_some());
    }

    /// Updating one scope must not disturb another: the App may have two projects
    /// open, and a switch in one is not news about the other.
    #[test]
    fn a_scope_update_preserves_the_other_scope() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a1")],
            ))
            .unwrap();
        store
            .replace(snapshot(
                SCOPE_B,
                1,
                vec![entry(SCOPE_B, "thread:c", "root-c", "cap-c")],
            ))
            .unwrap();

        store
            .replace(snapshot(
                SCOPE_A,
                2,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a2")],
            ))
            .unwrap();

        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:a", "root-a"))
                .map(|c| c.expose()),
            Some("cap-a2")
        );
        assert_eq!(
            store
                .get(&key(SCOPE_B, "thread:c", "root-c"))
                .map(|c| c.expose()),
            Some("cap-c"),
            "scope B must survive an update of scope A"
        );
    }

    /// Revisions are monotonic per scope, and a stale one is refused *without*
    /// disturbing the installed snapshot.
    #[test]
    fn a_stale_revision_is_refused_and_changes_nothing() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                2,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-v2")],
            ))
            .unwrap();

        let error = store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-v1")],
            ))
            .unwrap_err();
        assert!(error.contains("revision"), "{error}");
        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:a", "root-a"))
                .map(|c| c.expose()),
            Some("cap-v2"),
            "a refused snapshot must not have been applied"
        );
    }

    /// The same revision twice is also not "newer": a replay must not re-install a
    /// capability that a later empty snapshot revoked.
    #[test]
    fn an_empty_snapshot_revokes_the_scope() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap();
        store.replace(snapshot(SCOPE_A, 2, Vec::new())).unwrap();

        assert!(store.get(&key(SCOPE_A, "thread:a", "root-a")).is_none());
        assert_eq!(store.revision(SCOPE_A), Some(2));
    }

    /// A snapshot from another epoch is refused even with a higher revision: the
    /// epoch is fixed by the authenticated bootstrap, so "newer" cannot buy trust.
    #[test]
    fn a_higher_revision_from_another_epoch_is_refused() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap();

        let mut foreign = snapshot(
            SCOPE_A,
            99,
            vec![entry(SCOPE_A, "thread:a", "root-a", "cap-evil")],
        );
        foreign.sidecar_epoch = "epoch-2".to_string();
        let error = store.replace(foreign).unwrap_err();
        assert!(error.contains("epoch"), "{error}");
        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:a", "root-a"))
                .map(|c| c.expose()),
            Some("cap-a")
        );
    }

    /// An empty snapshot from another epoch is refused too, so a stale sender cannot
    /// silently revoke a live session's authority.
    #[test]
    fn an_empty_stale_snapshot_cannot_revoke() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap();

        let mut stale = snapshot(SCOPE_A, 2, Vec::new());
        stale.sidecar_epoch = "epoch-0".to_string();
        assert!(store.replace(stale).is_err());
        assert!(
            store.get(&key(SCOPE_A, "thread:a", "root-a")).is_some(),
            "a refused revocation must leave the capability in place"
        );
    }

    /// A snapshot installed before the bootstrap has nothing to check the epoch
    /// against, so it is refused outright.
    #[test]
    fn no_snapshot_is_accepted_before_the_epoch_is_fixed() {
        let mut store = RootAuthorityStore::new();
        let error = store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap_err();
        assert!(error.contains("bootstrap epoch"), "{error}");
    }

    /// The epoch is decided once. A second, different epoch is a second handshake
    /// claiming this session, which must not be believed.
    #[test]
    fn the_epoch_cannot_be_replaced() {
        let mut store = bound();
        store.bind_epoch(EPOCH).unwrap();
        assert!(store.bind_epoch("epoch-2").is_err());
        assert_eq!(store.epoch(), Some(EPOCH));
    }

    /// An entry for another scope inside a snapshot is refused wholesale, and the
    /// previous snapshot survives.
    #[test]
    fn an_entry_for_another_scope_refuses_the_whole_snapshot() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap();

        let error = store
            .replace(snapshot(
                SCOPE_A,
                2,
                vec![
                    entry(SCOPE_A, "thread:a", "root-a", "cap-new"),
                    entry(SCOPE_B, "thread:c", "root-c", "cap-foreign"),
                ],
            ))
            .unwrap_err();
        assert!(error.contains("scope"), "{error}");
        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:a", "root-a"))
                .map(|c| c.expose()),
            Some("cap-a"),
            "a half-applied snapshot would be worse than none"
        );
    }

    /// A replacement daemon retires the old generation's capability: the new
    /// snapshot names the new generation and nothing from the old one survives.
    ///
    /// There is deliberately no separate "revoke old generations" operation. A
    /// snapshot describes exactly one runtime, so installing the new one already
    /// removes the old; a second mechanism could only disagree with the first.
    #[test]
    fn a_generation_change_retires_the_old_capability() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-gen1")],
            ))
            .unwrap();

        let gen2 = RootAuthorityEntry {
            key: RootAuthorityKey {
                scope_key: SCOPE_A.to_string(),
                generation: "gen-2".to_string(),
                client_run_id: CLIENT.to_string(),
                thread_key: "thread:a".to_string(),
                root_id: "root-a2".to_string(),
            },
            capability: SecretCapability::new("cap-gen2"),
        };
        store
            .replace(snapshot(SCOPE_A, 2, vec![gen2.clone()]))
            .unwrap();

        assert!(
            store.get(&key(SCOPE_A, "thread:a", "root-a")).is_none(),
            "a capability minted for the replaced daemon must not resolve"
        );
        assert_eq!(store.get(&gen2.key).map(|c| c.expose()), Some("cap-gen2"));
        assert_eq!(store.generation(SCOPE_A), Some("gen-2"));
        assert_eq!(
            store
                .capability_for(SCOPE_A, CLIENT, "thread:a")
                .map(|c| c.expose()),
            Some("cap-gen2"),
            "the conversation's next attach uses the new generation"
        );
    }

    /// Revoking one conversation leaves its sibling in the same project alone.
    #[test]
    fn revoking_one_conversation_keeps_its_sibling() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![
                    entry(SCOPE_A, "thread:a", "root-a", "cap-a"),
                    entry(SCOPE_A, "thread:b", "root-b", "cap-b"),
                ],
            ))
            .unwrap();

        assert!(store.revoke(&key(SCOPE_A, "thread:a", "root-a")).is_some());
        assert!(store.get(&key(SCOPE_A, "thread:a", "root-a")).is_none());
        assert!(
            store.get(&key(SCOPE_A, "thread:b", "root-b")).is_some(),
            "one conversation's repair must not stop another"
        );
    }

    /// A conversation finds its own capability without knowing the root id, and
    /// cannot find another conversation's.
    #[test]
    fn a_conversation_finds_only_its_own_capability() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![
                    entry(SCOPE_A, "thread:a", "root-a", "cap-a"),
                    entry(SCOPE_A, "thread:b", "root-b", "cap-b"),
                ],
            ))
            .unwrap();

        assert_eq!(
            store
                .capability_for(SCOPE_A, CLIENT, "thread:a")
                .map(|c| c.expose()),
            Some("cap-a")
        );
        assert_eq!(
            store
                .capability_for(SCOPE_A, CLIENT, "thread:b")
                .map(|c| c.expose()),
            Some("cap-b")
        );
        // A different client in the same project is a different conversation's
        // authority, and must not be reachable by thread key alone.
        assert!(
            store
                .capability_for(SCOPE_A, "other-client", "thread:a")
                .is_none()
        );
        // Another project never resolves through this one.
        assert!(store.capability_for(SCOPE_B, CLIENT, "thread:a").is_none());
        assert_eq!(store.generation(SCOPE_A), Some("gen-1"));
    }

    /// A snapshot that mixes generations is refused: it would describe two daemons
    /// at once, and a capability could then be matched against the wrong one.
    #[test]
    fn one_snapshot_cannot_name_two_generations() {
        let mut store = bound();
        let error = store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![
                    entry(SCOPE_A, "thread:a", "root-a", "cap-a"),
                    RootAuthorityEntry {
                        key: RootAuthorityKey {
                            scope_key: SCOPE_A.to_string(),
                            generation: "gen-2".to_string(),
                            client_run_id: CLIENT.to_string(),
                            thread_key: "thread:b".to_string(),
                            root_id: "root-b".to_string(),
                        },
                        capability: SecretCapability::new("cap-b"),
                    },
                ],
            ))
            .unwrap_err();
        assert!(error.contains("generations"), "{error}");
        assert!(store.generation(SCOPE_A).is_none());
    }

    /// An incremental install joins its scope without disturbing the sibling scope,
    /// and a conversation that only knows its own key finds exactly its own entry.
    #[test]
    fn an_incremental_install_joins_the_scope_it_belongs_to() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-a")],
            ))
            .unwrap();
        store
            .replace(snapshot(
                SCOPE_B,
                1,
                vec![entry(SCOPE_B, "thread:b", "root-b", "cap-b")],
            ))
            .unwrap();

        store
            .install_entry(entry(SCOPE_A, "thread:c", "root-c", "cap-c"))
            .unwrap();

        assert_eq!(
            store
                .capability_for(SCOPE_A, CLIENT, "thread:c")
                .map(|c| c.expose()),
            Some("cap-c")
        );
        assert_eq!(
            store
                .capability_for(SCOPE_A, CLIENT, "thread:a")
                .map(|c| c.expose()),
            Some("cap-a"),
            "an install must not drop the scope it joined"
        );
        assert_eq!(
            store
                .capability_for(SCOPE_B, CLIENT, "thread:b")
                .map(|c| c.expose()),
            Some("cap-b"),
            "another project's authority is untouched"
        );
        assert_eq!(store.revision(SCOPE_A), Some(2));
    }

    /// A new generation's entry replaces the old generation wholesale: a capability
    /// for a replaced daemon must not survive beside the new one.
    #[test]
    fn an_incremental_install_of_a_new_generation_replaces_the_old_one() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                4,
                vec![entry(SCOPE_A, "thread:a", "root-a", "cap-gen1")],
            ))
            .unwrap();

        let mut newer = entry(SCOPE_A, "thread:a", "root-a2", "cap-gen2");
        newer.key.generation = "gen-2".to_string();
        store.install_entry(newer.clone()).unwrap();

        assert!(store.get(&key(SCOPE_A, "thread:a", "root-a")).is_none());
        assert_eq!(store.get(&newer.key).map(|c| c.expose()), Some("cap-gen2"));
        assert_eq!(store.generation(SCOPE_A), Some("gen-2"));
        assert_eq!(store.revision(SCOPE_A), Some(5));
    }

    /// Nothing is installable before the authenticated bootstrap fixed an epoch.
    #[test]
    fn an_incremental_install_needs_a_fixed_epoch() {
        let mut store = RootAuthorityStore::new();
        let error = store
            .install_entry(entry(SCOPE_A, "thread:a", "root-a", "cap-a"))
            .unwrap_err();
        assert!(error.contains("epoch"), "{error}");
        assert!(store.keys(SCOPE_A).is_empty());
    }

    /// A partial entry is refused rather than filed under a key with a hole in it.
    #[test]
    fn an_incremental_install_refuses_a_partial_entry() {
        let mut store = bound();
        for breakage in 0..4 {
            let mut broken = entry(SCOPE_A, "thread:a", "root-a", "cap-a");
            match breakage {
                0 => broken.key.scope_key.clear(),
                1 => broken.key.generation.clear(),
                2 => broken.key.client_run_id.clear(),
                _ => broken.key.thread_key.clear(),
            }
            assert!(
                store.install_entry(broken).is_err(),
                "an entry with a missing field must be refused"
            );
        }
        assert!(store.keys(SCOPE_A).is_empty());
    }

    /// A context refuses to ask for anything before the epoch is fixed, and it does
    /// not fall back to "no authority needed".
    #[test]
    fn a_context_without_an_epoch_refuses_to_resolve() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(RootAuthorityStore::new()));
        let context = ManagedContext::new(store, SCOPE_A, CLIENT);
        let error = context
            .resolve("thread:a", &intent("/tmp/project"))
            .unwrap_err();
        assert_eq!(error, ManagedAdmissionError::NoBootstrapEpoch);
    }

    /// With no broker and nothing installed, the answer is a typed refusal -- never a
    /// silent "run it unmanaged".
    #[test]
    fn a_context_without_a_broker_refuses_rather_than_degrading() {
        let mut store = RootAuthorityStore::new();
        store.bind_epoch(EPOCH).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let context = ManagedContext::new(store, SCOPE_A, CLIENT);
        let error = context
            .resolve("thread:a", &intent("/tmp/project"))
            .unwrap_err();
        assert_eq!(error, ManagedAdmissionError::NoBroker);
    }

    /// A broker that answered for another conversation is refused, and its answer is
    /// not installed: a capability from another conversation must never become ours.
    #[test]
    fn a_broker_answering_for_another_thread_is_refused() {
        let mut store = RootAuthorityStore::new();
        store.bind_epoch(EPOCH).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let broker = std::sync::Arc::new(FixedBroker {
            answer: entry(SCOPE_A, "thread:other", "root-a", "cap-a"),
        });
        let context =
            ManagedContext::new(std::sync::Arc::clone(&store), SCOPE_A, CLIENT).with_broker(broker);
        let error = context
            .resolve("thread:a", &intent("/tmp/project"))
            .unwrap_err();
        assert_eq!(
            error,
            ManagedAdmissionError::WrongThread {
                wanted: "thread:a".to_string(),
                got: "thread:other".to_string(),
            }
        );
        assert!(
            store.lock().unwrap().keys(SCOPE_A).is_empty(),
            "a refused answer must not be installed"
        );
    }

    /// A broker that answered for another project's scope is refused the same way.
    #[test]
    fn a_broker_answering_for_another_scope_is_refused() {
        let mut store = RootAuthorityStore::new();
        store.bind_epoch(EPOCH).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let broker = std::sync::Arc::new(FixedBroker {
            answer: entry(SCOPE_B, "thread:a", "root-a", "cap-a"),
        });
        let context =
            ManagedContext::new(std::sync::Arc::clone(&store), SCOPE_A, CLIENT).with_broker(broker);
        let error = context
            .resolve("thread:a", &intent("/tmp/project"))
            .unwrap_err();
        assert!(
            matches!(error, ManagedAdmissionError::WrongScope { .. }),
            "{error:?}"
        );
    }

    /// A broker that answered for another App run is refused: two runs' authority
    /// must never mix.
    #[test]
    fn a_broker_answering_for_another_client_is_refused() {
        let mut store = RootAuthorityStore::new();
        store.bind_epoch(EPOCH).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let mut answer = entry(SCOPE_A, "thread:a", "root-a", "cap-a");
        answer.key.client_run_id = "app-run-2".to_string();
        let context = ManagedContext::new(std::sync::Arc::clone(&store), SCOPE_A, CLIENT)
            .with_broker(std::sync::Arc::new(FixedBroker { answer }));
        let error = context
            .resolve("thread:a", &intent("/tmp/project"))
            .unwrap_err();
        assert!(
            matches!(error, ManagedAdmissionError::WrongClient { .. }),
            "{error:?}"
        );
    }

    /// The first answer is installed, and a second resolve reuses it instead of
    /// asking again: a lost attach answer must not mint a second root.
    #[test]
    fn a_second_resolve_reuses_the_installed_authority() {
        let mut store = RootAuthorityStore::new();
        store.bind_epoch(EPOCH).unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(store));
        let counting = std::sync::Arc::new(CountingBroker::new(entry(
            SCOPE_A, "thread:a", "root-a", "cap-a",
        )));
        let context = ManagedContext::new(std::sync::Arc::clone(&store), SCOPE_A, CLIENT)
            .with_broker(counting.clone());

        let first = context
            .resolve("thread:a", &intent("/tmp/project"))
            .expect("the first answer is usable");
        let second = context
            .resolve("thread:a", &intent("/tmp/project"))
            .expect("the second answer must still be usable");
        assert_eq!(first, second);
        assert_eq!(counting.calls(), 1, "the second resolve must not re-ask");
    }

    /// Revoking one conversation leaves its sibling authorized.
    #[test]
    fn revoking_one_conversation_leaves_the_other() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![
                    entry(SCOPE_A, "thread:a", "root-a", "cap-a"),
                    entry(SCOPE_A, "thread:b", "root-b", "cap-b"),
                ],
            ))
            .unwrap();
        store.revoke(&key(SCOPE_A, "thread:a", "root-a"));
        assert!(store.get(&key(SCOPE_A, "thread:a", "root-a")).is_none());
        assert_eq!(
            store
                .get(&key(SCOPE_A, "thread:b", "root-b"))
                .map(|c| c.expose()),
            Some("cap-b")
        );
    }

    fn intent(workspace: &str) -> RootBindIntent {
        RootBindIntent {
            operation_id: "managed-root:thread:a".to_string(),
            thread_key: "thread:a".to_string(),
            workspace: std::path::PathBuf::from(workspace),
            extra_roots: Vec::new(),
            previous_root_capability: None,
        }
    }

    struct FixedBroker {
        answer: RootAuthorityEntry,
    }

    impl ManagedRootAuthority for FixedBroker {
        fn authorize(
            &self,
            _intent: &RootIntentRequest,
            _deadline: Deadline,
        ) -> Result<RootAuthorityEntry, String> {
            Ok(self.answer.clone())
        }
    }

    struct CountingBroker {
        answer: RootAuthorityEntry,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingBroker {
        fn new(answer: RootAuthorityEntry) -> Self {
            Self {
                answer,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl ManagedRootAuthority for CountingBroker {
        fn authorize(
            &self,
            _intent: &RootIntentRequest,
            _deadline: Deadline,
        ) -> Result<RootAuthorityEntry, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.answer.clone())
        }
    }

    /// The store never prints a capability.
    #[test]
    fn the_store_does_not_print_capabilities() {
        let mut store = bound();
        store
            .replace(snapshot(
                SCOPE_A,
                1,
                vec![entry(SCOPE_A, "thread:a", "root-a", "super-secret")],
            ))
            .unwrap();
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}
