//! TUI-facing subagent runtime attachment model.

#![allow(dead_code)]

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiRuntimeMode {
    Attached(AttachedRoot),
    Disabled { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RuntimeBootstrapState {
    #[default]
    Disconnected,
    Prompting,
    Attached(AttachedRoot),
    Disabled {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStartPrompt {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct RuntimeBootstrapModel {
    state: RuntimeBootstrapState,
}

impl RuntimeBootstrapModel {
    pub fn disconnected_prompt(&mut self) -> Option<RuntimeStartPrompt> {
        match self.state {
            RuntimeBootstrapState::Disconnected => {
                self.state = RuntimeBootstrapState::Prompting;
                Some(RuntimeStartPrompt {
                    title: "启动本地 Agent Runtime?".into(),
                    body: "启动后可以直接用自然语言创建和管理子 Agent。".into(),
                })
            }
            _ => None,
        }
    }

    pub fn apply_choice<F>(&mut self, choice: RuntimeStartupChoice, mut attach: F) -> TuiRuntimeMode
    where
        F: FnMut() -> Result<AttachedRoot, String>,
    {
        match choice {
            RuntimeStartupChoice::Start => match attach() {
                Ok(root) => {
                    self.state = RuntimeBootstrapState::Attached(root.clone());
                    TuiRuntimeMode::Attached(root)
                }
                Err(reason) => {
                    self.state = RuntimeBootstrapState::Disabled {
                        reason: reason.clone(),
                    };
                    TuiRuntimeMode::Disabled { reason }
                }
            },
            RuntimeStartupChoice::ContinueWithoutDelegation => {
                let reason = "用户选择不启动本地 Agent Runtime".to_string();
                self.state = RuntimeBootstrapState::Disabled {
                    reason: reason.clone(),
                };
                TuiRuntimeMode::Disabled { reason }
            }
        }
    }
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
    use std::sync::{Arc, Mutex};

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
    fn disconnected_state_only_prompts_before_user_confirms_runtime_start() {
        let launches = Arc::new(Mutex::new(0));
        let mut model = RuntimeBootstrapModel::default();

        let prompt = model
            .disconnected_prompt()
            .expect("disconnected state prompts for runtime startup");

        assert!(prompt.title.contains("Runtime"));
        assert_eq!(*launches.lock().unwrap(), 0);
        let launches_for_attach = Arc::clone(&launches);
        let mode = model.apply_choice(RuntimeStartupChoice::Start, || {
            *launches_for_attach.lock().unwrap() += 1;
            Ok(attached_root())
        });
        assert!(matches!(mode, TuiRuntimeMode::Attached(_)));
        assert_eq!(*launches.lock().unwrap(), 1);
    }

    #[test]
    fn user_can_continue_without_starting_runtime() {
        let launches = Arc::new(Mutex::new(0));
        let launches_for_attach = Arc::clone(&launches);
        let mut model = RuntimeBootstrapModel::default();
        let _ = model.disconnected_prompt();

        let mode = model.apply_choice(RuntimeStartupChoice::ContinueWithoutDelegation, || {
            *launches_for_attach.lock().unwrap() += 1;
            Ok(attached_root())
        });

        assert!(matches!(mode, TuiRuntimeMode::Disabled { .. }));
        assert_eq!(*launches.lock().unwrap(), 0);
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
