//! TUI-facing subagent runtime attachment model.

use std::sync::{Mutex, OnceLock};

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

pub use yi_agent_subagent::{AttachedRoot, register_attached_root_tools};

/// The Chinese line shown when the runtime the user asked for (`y`, or a
/// remembered `always`) could not be started or activated.
///
/// Bring-up is best-effort: a runtime that cannot start degrades the session to
/// no-delegation rather than aborting it, so the notice must keep the session
/// usable *and* tell the user how to get delegation back. The raw cause is
/// logged (see `main.rs`) instead of rendered: an
/// `Error { code: InvalidState, message: ... }` dump buries the remedy, and
/// every failure kind clears the same way -- a fresh process.
pub const RUNTIME_RESTART_NOTICE: &str = "子 Agent runtime 未能启用；重启 yi-agent 后即可用";

pub fn runtime_restart_notice() -> crate::tui::cell::HistoryCell {
    crate::tui::cell::HistoryCell::Separator {
        label: Some(RUNTIME_RESTART_NOTICE.to_string()),
    }
}

/// The transcript line reporting that the runtime's startup sweep reclaimed
/// orphaned tasks.
///
/// This is information, not a failure, so it belongs in the transcript rather
/// than in `RUNTIME_RESTART_NOTICE`'s alert popup: delegation still works, the
/// children that died with their root were the ones that could never finish.
/// The count is kept because it is the only way to tell "your session lost
/// work" from "the sweep tidied up someone else's leftovers".
///
/// The daemon used to announce this itself with an `eprintln!`. That write
/// landed in the middle of a live TUI frame -- the daemon shares the process,
/// and the terminal, with the TUI -- and smeared the input box's styling, which
/// is exactly why the notice now travels as an event and is drawn here.
pub fn orphaned_reclaim_notice(count: usize) -> crate::tui::cell::HistoryCell {
    crate::tui::cell::HistoryCell::Separator {
        label: Some(format!(
            "runtime 已回收 {count} 个孤儿任务（所属 root 已退出）"
        )),
    }
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

#[cfg(test)]
mod tests {
    use yi_agent_core::ToolRegistry;
    use yi_agent_core::subagent::task::WorkspaceLeaseId;
    use yi_agent_core::subagent::worker::WorkerWorkspace;

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
    fn tui_registers_delegation_tools_with_a_controller() {
        use yi_agent_core::autonomy::YoloSwitch;
        use yi_agent_tools::{SandboxController, SandboxMode};

        // The controller the TUI passes is derived from the launch-time yolo
        // switch; asserting its effective mode proves the wiring is live, and
        // the `register_attached_root_tools` call below must compile with it.
        let controller =
            SandboxController::new(YoloSwitch::new(true), SandboxMode::WorkspaceWrite, true);
        assert_eq!(controller.effective(), SandboxMode::DangerFullAccess);

        let mut registry = ToolRegistry::new();
        register_attached_root_tools(
            &mut registry,
            "/tmp/runtime.sock".into(),
            &attached_root(),
            controller,
        );
        assert!(registry.names().contains(&"spawn_agent".to_string()));
    }

    #[test]
    fn attached_tui_root_exposes_subagent_tools_without_a_delegate_command() {
        let mut registry = ToolRegistry::new();
        register_attached_root_tools(
            &mut registry,
            "/tmp/runtime.sock".into(),
            &attached_root(),
            yi_agent_tools::SandboxController::new(
                yi_agent_core::autonomy::YoloSwitch::new(false),
                yi_agent_tools::SandboxMode::WorkspaceWrite,
                false,
            ),
        );
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

#[cfg(test)]
mod restart_notice_tests {
    use super::*;

    /// The restart notice is the whole user-facing story after a failed `y`, so
    /// it must name the remedy rather than the fault.
    #[test]
    fn the_notice_tells_the_user_to_restart() {
        let label = label_of(runtime_restart_notice());
        assert!(label.contains("重启"), "no restart guidance: {label}");
        assert!(
            label.contains("子 Agent"),
            "does not say what is off: {label}"
        );
    }

    /// What the user actually saw when the runtime failed to start after `y`:
    /// one English line (`Error: provider turn admission failed: subagent
    /// delegation unavailable: ...`) carrying an `IpcError` or
    /// `Error { code: InvalidState, message: ... }` dump. That reads as an
    /// internal fault and offers no next step, so the notice must not carry any
    /// of it -- the raw cause belongs in the trace, which `main.rs` writes.
    #[test]
    fn the_notice_never_renders_a_raw_diagnostic() {
        let label = label_of(runtime_restart_notice());
        for leak in ["Error", "error", "{", "code:", "unavailable", "admission"] {
            assert!(
                !label.contains(leak),
                "leaked {leak:?} to the user: {label}"
            );
        }
    }

    /// The reclaim line must name the count and never read as a failure: the
    /// session is fully usable, only the dead children were tidied up.
    #[test]
    fn the_reclaim_notice_states_the_count_without_sounding_like_a_failure() {
        let label = label_of(orphaned_reclaim_notice(3));
        assert!(label.contains('3'), "the count must be visible: {label}");
        for leak in ["Error", "error", "{", "code:", "unavailable", "failed"] {
            assert!(
                !label.contains(leak),
                "leaked {leak:?} into an informational notice: {label}"
            );
        }
    }

    fn label_of(notice: crate::tui::cell::HistoryCell) -> String {
        match notice {
            crate::tui::cell::HistoryCell::Separator { label: Some(label) } => label,
            other => panic!("expected a labelled separator, got {other:?}"),
        }
    }
}
