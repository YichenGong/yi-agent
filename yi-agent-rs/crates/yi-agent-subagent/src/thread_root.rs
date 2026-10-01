//! One conversation's own application root on a shared project daemon.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

use crate::AttachedRoot;
use crate::binding::{RuntimeBinding, RuntimeHandle};

/// The stable attach key for one conversation's root.
///
/// The project key cannot be used: two conversations in one directory must get
/// two roots, or they share one `MAX_DIRECT_CHILDREN` budget. The thread id is
/// stable across retries and across a daemon restart, so re-attaching the same
/// conversation adopts its own root instead of minting a new one.
pub fn application_root_key(thread_id: &str) -> String {
    format!("thread:{thread_id}")
}

/// One conversation's own root on a shared project runtime.
///
/// The `RuntimeBinding` is shared (one daemon per project, self-healing); this
/// type owns only the root triple, so two conversations in one directory have
/// two independent root tasks and two independent child budgets.
pub struct ThreadRoot {
    binding: Arc<RuntimeBinding>,
    thread_id: String,
    project_dir: PathBuf,
    root: Mutex<Option<AttachedRoot>>,
}

impl ThreadRoot {
    pub fn new(
        binding: Arc<RuntimeBinding>,
        thread_id: impl Into<String>,
        project_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            thread_id: thread_id.into(),
            project_dir,
            root: Mutex::new(None),
        })
    }

    fn lock_root(&self) -> std::sync::MutexGuard<'_, Option<AttachedRoot>> {
        self.root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attaches (or adopts) this conversation's root on the shared daemon.
    ///
    /// Idempotent: the stable key means a second call adopts the root the first
    /// one created, including after a daemon restart.
    pub fn attach(&self) -> Result<(), String> {
        if self.lock_root().is_some() {
            return Ok(());
        }
        let handle = self.binding.current_or_repair()?;
        let response = send_request(
            &handle.socket_path,
            IpcRequest::AttachApplicationRoot {
                idempotency_key: application_root_key(&self.thread_id),
                workspace: self.project_dir.clone(),
            },
        )
        .map_err(|error| error.to_string())?;
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            workspace,
        } = response
        else {
            return Err(format!("daemon rejected the conversation root: {response:?}"));
        };
        *self.lock_root() = Some(AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace,
        });
        Ok(())
    }

    /// The handle a delegation call should act as: the shared socket plus this
    /// conversation's own root triple.
    pub fn handle(&self) -> Result<RuntimeHandle, String> {
        self.attach()?;
        let shared = self.binding.current_or_repair()?;
        let root = self
            .lock_root()
            .clone()
            .ok_or_else(|| "the conversation root is not attached".to_string())?;
        Ok(RuntimeHandle {
            socket_path: shared.socket_path,
            workspace_root: shared.workspace_root,
            session_id: root.session_id,
            task_id: root.task_id,
            capability: root.capability,
        })
    }

    /// This conversation's root task id, once attached.
    pub fn root_task_id(&self) -> Option<String> {
        self.lock_root().as_ref().map(|root| root.task_id.clone())
    }

    /// Activates this conversation's root with its first objective. Idempotent
    /// at the daemon, so a re-run after a repair is safe.
    pub fn activate(&self, objective: &str) -> Result<(), String> {
        let handle = self.handle()?;
        match send_request(
            &handle.socket_path,
            IpcRequest::ActivateApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                objective: objective.to_owned(),
            },
        ) {
            Ok(IpcResponse::ApplicationRootActivated) => Ok(()),
            Ok(other) => Err(format!("daemon rejected the conversation activation: {other:?}")),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Detaches this conversation's root. Best effort: a failure is a trace
    /// line, never an error that masks why the conversation is winding down.
    pub fn detach(&self) {
        let handle = match self.handle() {
            Ok(handle) => handle,
            Err(_) => return,
        };
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
            },
        ) {
            tracing::warn!(%error, "could not detach the conversation root");
        }
        *self.lock_root() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_key_is_stable_and_distinct_per_thread() {
        assert_eq!(application_root_key("thread-a"), "thread:thread-a");
        assert_eq!(application_root_key("thread-b"), "thread:thread-b");
        assert_eq!(
            application_root_key("thread-a"),
            application_root_key("thread-a"),
            "the same conversation must key the same root on every attempt"
        );
        assert_ne!(
            application_root_key("thread-a"),
            application_root_key("thread-b"),
            "two conversations must not share one root"
        );
    }
}
