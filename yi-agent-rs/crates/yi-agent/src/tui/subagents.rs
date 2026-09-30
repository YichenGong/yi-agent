//! TUI-facing subagent runtime attachment model.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use yi_agent_core::ToolRegistry;
use yi_agent_core::subagent::worker::WorkerWorkspace;

use crate::subagent_runtime::register_application_subagent_tools;
#[cfg(test)]
use crate::tui::slash::SlashCommand;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStartupChoice {
    Start,
    ContinueWithoutDelegation,
}

/// What the TUI should do about the local subagent runtime on launch.
///
/// Chosen by `main.rs` from the persisted preference (§`runtime_prefs`), so the
/// UI layer never reads the preference file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeStartupIntent {
    /// Show the yes/no dialog and wait for the user.
    Prompt,
    /// Do not start; show one separator line explaining why.
    DisabledNotice { reason: String },
    /// Start without asking; `main.rs` pre-seeds `RuntimeStartupChoice::Start`.
    AutoStart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedRoot {
    pub session_id: String,
    pub task_id: String,
    pub capability: String,
    pub workspace: WorkerWorkspace,
}

static CURRENT_ATTACHED_ROOT: OnceLock<Mutex<Option<AttachedRoot>>> = OnceLock::new();

pub fn set_current_attached_root(root: AttachedRoot) {
    *CURRENT_ATTACHED_ROOT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("current attached root mutex poisoned") = Some(root);
}

pub fn current_attached_root() -> Option<AttachedRoot> {
    CURRENT_ATTACHED_ROOT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("current attached root mutex poisoned")
        .clone()
}

pub fn register_attached_root_tools(
    registry: &mut ToolRegistry,
    runtime_socket: PathBuf,
    root: &AttachedRoot,
) {
    register_application_subagent_tools(
        registry,
        runtime_socket,
        root.session_id.clone(),
        root.task_id.clone(),
        root.capability.clone(),
    );
}

#[cfg(test)]
mod tests {
    use yi_agent_core::subagent::task::WorkspaceLeaseId;

    use super::*;

    fn worker_workspace() -> WorkerWorkspace {
        WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: "/tmp/repo".into(),
            path: "/tmp/repo/.worktrees/root".into(),
            branch: "feat/root".into(),
            parent_branch: "main".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        }
    }

    fn attached_root() -> AttachedRoot {
        AttachedRoot {
            session_id: "session-1".into(),
            task_id: "task-1".into(),
            capability: "capability-1".into(),
            workspace: worker_workspace(),
        }
    }

    #[test]
    fn attached_tui_root_exposes_subagent_tools_without_a_delegate_command() {
        let mut registry = ToolRegistry::new();
        register_attached_root_tools(&mut registry, "/tmp/runtime.sock".into(), &attached_root());
        let names = registry
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();

        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"send_message".to_string()));
        assert!(names.contains(&"wait_agent".to_string()));
        assert!(
            !SlashCommand::all()
                .iter()
                .any(|command| command.name() == "delegate")
        );
    }
}
