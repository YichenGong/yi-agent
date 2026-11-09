//! The ordered start of a *managed* CLI frontend.
//!
//! The TUI and `yi-agent run` both used to attach a root and *then* register their
//! lease. That is backwards on a guardian-supervised runtime: the guardian authorizes
//! roots for clients it knows are present, so an attach that happens before the lease
//! asks the daemon to mint a root nobody vouched for. It used to work because nothing
//! enforced the order; with per-root authorization the guardian refuses, and a client
//! that treated the refusal as "no guardian" would fall back to a direct spawn -- an
//! unowned process nothing can reclaim.
//!
//! So the order is fixed and enforced by [`yi_agent_subagent::managed_startup`]:
//!
//! ```text
//! prepare -> register_business -> heartbeat -> authorize_root ->
//! attach_root -> bind_driver -> open_managed_channel -> admit_turn
//! ```
//!
//! This module wires those stages to the real calls. It owns no policy of its own: a
//! refusal anywhere ends the start, and the caller gets the stages that completed so
//! the failure is explainable.
//!
//! The last three stages are separate methods rather than one function because they
//! happen at different moments: a TUI binds its driver and opens its spawn channel
//! while it is still starting, but admits its first turn only when the user types one.
//! Collapsing them would mean the TUI either admitted work it had not been asked for or
//! re-did the earlier stages per turn.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, ClientKind, RootAuthorization};
use yi_agent_subagent::managed_startup::{
    ManagedStart, ManagedStartObserver, ManagedStartRefusal, ManagedStartStage,
};

use crate::frontend_lease::FrontendLeaseHandle;

/// Everything the start needs before it touches the runtime.
pub struct ManagedStartRequest<'a> {
    pub cfg: &'a yi_agent_runtime::config::RuntimeConfig,
    pub runtime_dir: PathBuf,
    pub instance_home: PathBuf,
    pub expected: ArtifactIdentity,
    pub client_kind: ClientKind,
    pub client_run_id: String,
    /// The conversation this frontend is starting for: stable across turns so a
    /// retried authorization resolves to the same root instead of a second one.
    pub thread_key: String,
    /// Where the root runs. The daemon compares it with the guardian's reservation.
    pub workspace: PathBuf,
    pub extra_roots: Vec<PathBuf>,
    pub deadline: Deadline,
}

/// A managed frontend that has prepared a runtime, holds its lease, and attached its
/// root -- but has not yet been allowed to run a turn.
///
/// The type is the point: a caller cannot reach the conversation with a root alone.
/// Binding the driver and opening the spawn channel are explicit steps, and `admit`
/// is the last gate, so "we attached but never opened a channel" cannot silently look
/// like a working session.
pub struct PendingManagedStart {
    start: ManagedStart,
    lease: FrontendLeaseHandle,
    attached: yi_agent_subagent::attach::AttachedProjectRuntime,
    authorization: RootAuthorization,
    thread_key: String,
    scope_key: String,
    runtime_dir: PathBuf,
    workspace: PathBuf,
    extra_roots: Vec<PathBuf>,
    pub client_run_id: String,
}

/// What an admitted session carries into the conversation.
///
/// Deliberately without the spawn channel: `ManagedSpawner` is a factory, not a value
/// to copy, and one channel belongs to one owner for its lifetime. The call that
/// admits hands the channel out beside this struct, so there is no state in which a
/// session is admitted and has no channel -- or has two.
pub struct AdmittedManagedStart {
    pub attached: yi_agent_subagent::attach::AttachedProjectRuntime,
    pub lease: FrontendLeaseHandle,
}

impl PendingManagedStart {
    /// The conversation root, on a runtime that brokers authority.
    ///
    /// Built here rather than by each caller so the authority store, the scope key and
    /// the client run always agree with the authorization that was just obtained: three
    /// values assembled at three call sites would eventually disagree, and the
    /// disagreement would show up as a root the daemon refuses to bind.
    ///
    /// The epoch is the guardian's generation. There is no shell bootstrap on the CLI
    /// path -- that handshake belongs to the desktop -- so the generation named by the
    /// guardian's own authenticated answer is the only epoch this session can honestly
    /// claim, and it is already fixed for the life of the runtime.
    pub fn managed_root(
        &self,
        cfg: &yi_agent_runtime::config::RuntimeConfig,
        thread_id: impl Into<String>,
    ) -> Result<Arc<yi_agent_subagent::thread_root::ThreadRoot>, String> {
        use yi_agent_subagent::managed_binding::{LeaseRootAuthority, ManagedContext};
        let thread_id = thread_id.into();
        let store = authority_store_for(&self.authorization, &self.scope_key, &thread_id)?;
        let context = Arc::new(
            ManagedContext::new(
                Arc::new(std::sync::Mutex::new(store)),
                self.scope_key.clone(),
                self.client_run_id.clone(),
            )
            .with_broker(Arc::new(LeaseRootAuthority::new(self.lease.shared()))),
        );
        let binding = yi_agent_subagent::binding::RuntimeBinding::fixed_in(
            self.runtime_dir.clone(),
            yi_agent_subagent::binding::RuntimeHandle {
                socket_path: self.attached.socket_path.clone(),
                workspace_root: self.attached.workspace_root.clone(),
                session_id: self.attached.attached_root.session_id.clone(),
                task_id: self.attached.attached_root.task_id.clone(),
                capability: self.attached.attached_root.capability.clone(),
            },
        );
        let _ = cfg;
        Ok(
            yi_agent_subagent::thread_root::ThreadRoot::managed_attached(
                binding,
                thread_id,
                self.workspace.clone(),
                self.extra_roots.clone(),
                context,
                yi_agent_subagent::AttachedRoot {
                    session_id: self.attached.attached_root.session_id.clone(),
                    task_id: self.attached.attached_root.task_id.clone(),
                    capability: self.attached.attached_root.capability.clone(),
                    workspace: self.attached.attached_root.workspace.clone(),
                },
                // A fixed binding reports generation 0 forever, so the cached root stays
                // live for the session. It was minted against exactly this runtime.
                0,
            ),
        )
    }

    /// Binds the foreground driver for the conversation root.
    ///
    /// The turn itself is not opened here: which turn is running is the caller's
    /// business, and opening one before a prompt exists would report activity for work
    /// the user never asked for.
    pub fn bind_driver(
        &mut self,
        root: &Arc<yi_agent_subagent::thread_root::ThreadRoot>,
    ) -> Result<yi_agent_subagent::foreground::ForegroundTurn, ManagedStartRefusal> {
        self.start.step(ManagedStartStage::BindDriver, || {
            let turn = yi_agent_subagent::foreground::ForegroundTurn::bind(
                Arc::clone(root),
                self.authorization.clone(),
                yi_agent_subagent::thread_root::managed_bind_operation_id(&self.thread_key),
            )
            .map_err(|error| format!("the foreground driver could not bind: {error}"))?;
            Ok(turn)
        })
    }

    /// Opens the private spawn channel every tool child runs over.
    ///
    /// Taken during the start, not at the first tool call: a frontend that discovered
    /// only mid-turn that the guardian will not issue a channel would already have told
    /// the user work had begun. Returning `Err` here means the session must not run.
    pub fn open_managed_channel(
        &mut self,
    ) -> Result<
        (
            yi_agent_tools::ManagedProcessPolicy,
            yi_agent_tools::ManagedSpawner,
        ),
        ManagedStartRefusal,
    > {
        self.start.step(ManagedStartStage::OpenManagedChannel, || {
            self.lease.managed_owner_required()
        })
    }

    /// Reports a failure that happened between stages.
    ///
    /// Building the root from an authorization is not a stage of its own, but it can
    /// fail; the refusal names the stage it follows so the trace stays truthful about
    /// what did and did not happen.
    pub fn refuse_at(&mut self, stage: ManagedStartStage, cause: String) -> ManagedStartRefusal {
        self.start.refuse(stage, cause)
    }

    /// The final gate: from here on the conversation may run work.
    ///
    /// Used by the TUI, which opened its channel while starting and admits each turn
    /// later. A stage that already failed refuses here without running: `ManagedStart`
    /// keeps the first refusal, so a caller that ignored an `Err` above still cannot
    /// admit.
    pub fn admit(mut self) -> Result<AdmittedManagedStart, ManagedStartRefusal> {
        self.start.step(ManagedStartStage::AdmitTurn, || Ok(()))?;
        Ok(self.admitted())
    }

    fn admitted(self) -> AdmittedManagedStart {
        AdmittedManagedStart {
            attached: self.attached,
            lease: self.lease,
        }
    }
}

/// Resolves the scope key of a runtime directory exactly as the guardian does.
///
/// The guardian keys roots by `scope.key`; a CLI that invented its own key would build
/// an authority entry the store then refuses -- or worse, one that matches nothing at
/// attach time.
fn scope_key_for(workdir: &Path, runtime_dir: &Path) -> String {
    let instance_home = yi_agent_store::paths::instance_home()
        .unwrap_or_else(|| runtime_dir.parent().unwrap_or(runtime_dir).to_path_buf());
    yi_agent_lifecycle::scope::resolve(runtime_dir, workdir, &instance_home)
        .map(|scope| scope.key)
        .unwrap_or_else(|_| runtime_dir.to_string_lossy().into_owned())
}

/// The authority store a managed conversation's root is built from.
///
/// The authorization the start just obtained *is* this conversation's authority, so it
/// is installed here rather than left for the first attach to fetch. An empty store
/// makes `ManagedContext::resolve` miss and ask the broker a *second* time for a root
/// whose authority the daemon has already installed; the daemon answers that second
/// round trip with `StaleAttempt`, because its `Renew` arm refuses a binding epoch the
/// guardian has already moved past. The failure then surfaces on a *different*
/// frontend's start as `the daemon refused the install: StaleAttempt`, which points at
/// a client that did nothing wrong.
///
/// The entry's `thread_key` is the conversation key the attach will present
/// (`thread:<id>`), which is exactly what the intent named, so the lookup hits.
fn authority_store_for(
    authorization: &RootAuthorization,
    scope_key: &str,
    thread_id: &str,
) -> Result<yi_agent_subagent::managed_binding::RootAuthorityStore, String> {
    let mut store = yi_agent_subagent::managed_binding::RootAuthorityStore::new();
    store.bind_epoch(authorization.daemon_instance_id.clone())?;
    store.install_entry(yi_agent_lifecycle::production::RootAuthorityEntry {
        key: yi_agent_lifecycle::production::RootAuthorityKey {
            scope_key: scope_key.to_string(),
            generation: authorization.daemon_instance_id.clone(),
            client_run_id: authorization.client_run_id.clone(),
            thread_key: yi_agent_subagent::thread_root::application_root_key(thread_id),
            root_id: authorization.root_id.clone(),
        },
        capability: yi_agent_lifecycle::foreground::SecretCapability::new(
            authorization.capability.clone(),
        ),
    })?;
    Ok(store)
}

/// Runs the start up to the point where a turn could be admitted.
///
/// `observer` receives the trace; the CLI installs one that appends to
/// `YI_AGENT_STARTUP_TRACE` when that variable names a file, so an integration test can
/// assert the order a *real* binary followed.
pub fn begin_managed_frontend(
    request: ManagedStartRequest<'_>,
    observer: Arc<dyn ManagedStartObserver>,
) -> Result<PendingManagedStart, ManagedStartRefusal> {
    let mut start = ManagedStart::new(observer);

    start.step(ManagedStartStage::Prepare, || {
        yi_agent_subagent::attach::prepare_managed_runtime(
            request.cfg,
            &request.runtime_dir,
            &request.instance_home,
            &request.expected,
            yi_agent_supervisors::lifecycle::bringup::BringUpIntent::Business,
            request.deadline.clone(),
        )
        .map_err(|failure| format!("{}: {}", failure.stage, failure.cause))
    })?;

    // The lease is taken *before* anything is authorized. No "best effort" fallback:
    // `Ok(None)` here would mean the bring-up reported a runtime whose guardian is not
    // serving, and proceeding would attach a root nobody can reclaim.
    let lease = start.step(ManagedStartStage::RegisterBusiness, || {
        FrontendLeaseHandle::register_required(
            &request.runtime_dir,
            request.client_kind,
            request.client_run_id.clone(),
        )
        .map_err(|error| format!("the guardian did not register a frontend lease: {error}"))
    })?;

    start.step(ManagedStartStage::Heartbeat, || {
        lease
            .require_heartbeat()
            .map_err(|error| format!("the frontend lease cannot be renewed: {error}"))
    })?;

    let intent = yi_agent_subagent::thread_root::root_bind_intent(
        &request.thread_key,
        request.workspace.clone(),
        request.extra_roots.clone(),
    );
    let authorization = start.step(ManagedStartStage::AuthorizeRoot, || {
        lease
            .authorize_root(&intent, &request.deadline)
            .map_err(|error| format!("root authorization was refused: {error}"))
    })?;

    // The capability *is* the attach key on a managed daemon: it is what attributes the
    // root to this client. The daemon answers with the root id it actually attached, and
    // a different id means the capability resolved to something else -- refused here
    // rather than cached as if this conversation owned it.
    let attached = start.step(ManagedStartStage::AttachRoot, || {
        let attached = yi_agent_subagent::attach::attach_authorized_root(
            request.cfg,
            &request.runtime_dir,
            &authorization.capability,
        )
        .map_err(|failure| format!("{}: {}", failure.stage, failure.cause))?;
        if attached.attached_root.task_id != authorization.root_id {
            return Err(format!(
                "the authorized root is {} but the daemon attached {}",
                authorization.root_id, attached.attached_root.task_id
            ));
        }
        Ok(attached)
    })?;

    Ok(PendingManagedStart {
        start,
        lease,
        attached,
        authorization,
        thread_key: request.thread_key,
        scope_key: scope_key_for(&request.cfg.workdir, &request.runtime_dir),
        runtime_dir: request.runtime_dir,
        workspace: request.workspace,
        extra_roots: request.extra_roots,
        client_run_id: request.client_run_id,
    })
}

/// Appends the start's stages to a file when `YI_AGENT_STARTUP_TRACE` names one.
///
/// A file rather than stderr because the TUI owns the screen: a trace line printed there
/// would be drawn over the UI, and an integration test reading stderr would then race the
/// renderer. Absent or unusable, nothing is recorded and nothing fails -- a diagnostic
/// must never be the reason a launch stops.
pub struct FileTraceObserver {
    path: Option<PathBuf>,
}

impl FileTraceObserver {
    pub fn from_env() -> Self {
        let path = std::env::var_os("YI_AGENT_STARTUP_TRACE")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        Self { path }
    }

    fn write(&self, line: &str) {
        use std::io::Write as _;
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

impl ManagedStartObserver for FileTraceObserver {
    fn completed(&self, stage: ManagedStartStage) {
        self.write(stage.label());
    }

    fn refused(&self, stage: ManagedStartStage, cause: &str) {
        // One line, with the cause, because the *reason* a start stopped is the part a
        // reader cannot reconstruct from the completed stages alone. The cause never
        // carries a capability: every message on this path is built from an `io::Error`
        // or an `AttachFailure`, and `RootAuthorization`'s `Debug` redacts its secret.
        self.write(&format!(
            "refused:{}:{}",
            stage.label(),
            cause.replace('\n', " ")
        ));
    }
}

/// Everything an admitted one-shot run needs, produced in one ordered pass.
pub struct StartedManagedRun {
    pub admitted: AdmittedManagedStart,
    pub owner: (
        yi_agent_tools::ManagedProcessPolicy,
        yi_agent_tools::ManagedSpawner,
    ),
    /// The conversation root, attached under the authority the start obtained.
    pub root: Arc<yi_agent_subagent::thread_root::ThreadRoot>,
}

/// The `yi-agent run` path: every stage, in order, in one call.
///
/// `run` has no gap between starting and working -- it exists to answer one prompt --
/// so it walks the stages itself instead of handing back a pending value. That is also
/// why the driver is bound here: a one-shot run that skipped the bind would reach its
/// first turn with no driver, and the trace would show a start that never proved the
/// contract it claims to follow.
pub fn start_managed_run(
    request: ManagedStartRequest<'_>,
    observer: Arc<dyn ManagedStartObserver>,
    thread_id: &str,
) -> Result<StartedManagedRun, ManagedStartRefusal> {
    let cfg = request.cfg;
    let mut pending = begin_managed_frontend(request, observer)?;
    // The root is built from the authorization just obtained; a run that attached one
    // itself would be acting on a root no authority names.
    let root = match pending.managed_root(cfg, thread_id) {
        Ok(root) => root,
        Err(cause) => return Err(pending.refuse_at(ManagedStartStage::AttachRoot, cause)),
    };
    pending.bind_driver(&root)?;
    let owner = pending.open_managed_channel()?;
    let admitted = pending.admit()?;
    Ok(StartedManagedRun {
        admitted,
        owner,
        root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The observer writes what the gate reports and nothing else.
    #[test]
    fn the_file_observer_records_labels_and_refusals() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("trace.log");
        let observer = FileTraceObserver {
            path: Some(path.clone()),
        };
        observer.completed(ManagedStartStage::Prepare);
        observer.refused(ManagedStartStage::Heartbeat, "no renewals");
        let text = std::fs::read_to_string(&path).expect("trace");
        assert_eq!(text, "prepare\nrefused:heartbeat:no renewals\n");
    }

    /// A missing trace path is not a failure: the start must not depend on where the
    /// diagnostics go.
    #[test]
    fn no_trace_path_records_nothing_and_succeeds() {
        let observer = FileTraceObserver { path: None };
        observer.completed(ManagedStartStage::Prepare);
        observer.refused(ManagedStartStage::Prepare, "no");
    }

    /// The scope key is the guardian's, not a path spelling invented here.
    #[test]
    fn the_scope_key_is_the_resolved_runtime_directory() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let runtime_dir = dir.path().join("runtime");
        let key = scope_key_for(&dir.path().join("repository"), &runtime_dir);
        assert!(
            key.contains("runtime"),
            "the key must describe the runtime directory: {key}"
        );
    }

    /// The store a managed root is built from must already hold the authorization the
    /// start obtained, keyed by the conversation the attach will name.
    ///
    /// Without this the store is an empty cache: `ThreadRoot::attach_session_container`
    /// calls `ManagedContext::resolve`, misses, and asks the broker a *second* time for
    /// a root whose authority the daemon has already installed. The daemon answers that
    /// second round trip with `StaleAttempt` (its `Renew` arm refuses a binding epoch
    /// the guardian has already moved past), which then surfaces on a *different*
    /// frontend's start as `the daemon refused the install: StaleAttempt` -- so the
    /// symptom points at a client that did nothing wrong.
    #[test]
    fn the_managed_root_starts_from_the_authorization_its_start_obtained() {
        let authorization = yi_agent_lifecycle::types::RootAuthorization {
            scope_key: "/private/tmp/scope".into(),
            daemon_instance_id: "daemon-1-2".into(),
            client_run_id: "run:7:conv".into(),
            root_id: "root-7".into(),
            capability: "rootcap1:abc".into(),
        };
        let conversation = "run:7:conv";

        let store = authority_store_for(&authorization, &authorization.scope_key, conversation)
            .expect("the store is built from the start's own authorization");

        let found = store.entry_for(
            &authorization.scope_key,
            &authorization.client_run_id,
            &yi_agent_subagent::thread_root::application_root_key(conversation),
        );
        assert_eq!(
            found.map(|entry| entry.capability.expose().to_string()),
            Some(authorization.capability.clone()),
            "the attach key must resolve locally, without a second broker round trip"
        );
    }
}
