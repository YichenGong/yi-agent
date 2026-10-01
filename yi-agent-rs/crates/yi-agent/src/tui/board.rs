use std::path::Path;

use yi_agent_board_ui::switch::{self, BoardSwitch};

/// What `/kanban` produced: lines to show, and whether it changed the switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanOutcome {
    pub lines: Vec<String>,
    /// `Some(true)` / `Some(false)` when this invocation wrote the project layer.
    pub toggled_to: Option<bool>,
}

/// Handles `/kanban [on|off|run]`.
///
/// The board is *disabled by default*, so an accidental invocation cannot start
/// a queue. `run` is intentionally refused while disabled: the switch is the
/// user's explicit consent to let the board act.
pub fn handle_kanban(workdir: &Path, args: &str) -> KanbanOutcome {
    let argument = args.trim();
    let project = switch::read_layer(&switch::project_path(workdir));
    let global = switch::global_path().and_then(|path| switch::read_layer(&path));
    let resolved = switch::resolve(global, project);
    let source = match resolved.source {
        switch::SwitchSource::Project => "project",
        switch::SwitchSource::Global => "global",
        switch::SwitchSource::Default => "default",
    };

    match argument {
        "" => {
            let view = yi_agent_board_ui::view::BoardView {
                switch_on: resolved.value.is_enabled(),
                switch_source: source,
                // The card list is supplied by the plugin process (Plan 3a);
                // this first version renders the switch state and the empty
                // state without inventing rows.
                cards: Vec::new(),
            };
            let mut lines = vec![view.header()];
            lines.extend(view.render_lines());
            KanbanOutcome {
                lines,
                toggled_to: None,
            }
        }
        "on" | "off" => {
            let value = if argument == "on" {
                BoardSwitch::Enabled
            } else {
                BoardSwitch::Disabled
            };
            let path = switch::project_path(workdir);
            match switch::write_layer(&path, value) {
                Ok(()) => KanbanOutcome {
                    lines: vec![format!(
                        "Superpowers 看板 {} (project: {})",
                        if value.is_enabled() {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        path.display()
                    )],
                    toggled_to: Some(value.is_enabled()),
                },
                Err(error) => KanbanOutcome {
                    lines: vec![format!("could not write {}: {error}", path.display())],
                    toggled_to: None,
                },
            }
        }
        "run" => {
            if resolved.value.is_enabled() {
                KanbanOutcome {
                    lines: vec![
                        "Superpowers 看板 is enabled; the plugin process advances the queue."
                            .to_string(),
                    ],
                    toggled_to: None,
                }
            } else {
                KanbanOutcome {
                    lines: vec![format!(
                        "Superpowers 看板 is disabled (source: {source}). \
                         Enable it with /kanban on, or set \"superpowers_board\": true."
                    )],
                    toggled_to: None,
                }
            }
        }
        _ => KanbanOutcome {
            lines: vec!["usage: /kanban [on|off|run]".to_string()],
            toggled_to: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_arguments_it_shows_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "");
        assert_eq!(outcome.toggled_to, None);
        assert!(!outcome.lines.is_empty());
        assert!(
            outcome.lines[0].contains("Superpowers 看板"),
            "{:?}",
            outcome.lines
        );
    }

    #[test]
    fn on_enables_the_project_layer() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "on");
        assert_eq!(outcome.toggled_to, Some(true));
        assert_eq!(
            yi_agent_board_ui::switch::read_layer(&yi_agent_board_ui::switch::project_path(
                dir.path()
            )),
            Some(yi_agent_board_ui::switch::BoardSwitch::Enabled)
        );
    }

    #[test]
    fn off_disables_the_project_layer() {
        let dir = tempfile::tempdir().unwrap();
        handle_kanban(dir.path(), "on");
        let outcome = handle_kanban(dir.path(), "off");
        assert_eq!(outcome.toggled_to, Some(false));
        assert_eq!(
            yi_agent_board_ui::switch::read_layer(&yi_agent_board_ui::switch::project_path(
                dir.path()
            )),
            Some(yi_agent_board_ui::switch::BoardSwitch::Disabled)
        );
    }

    #[test]
    fn an_unknown_argument_is_reported_instead_of_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "sideways");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("usage"),
            "expected a usage line, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn a_disabled_board_refuses_to_run_and_says_where_the_switch_is() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "run");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("disabled"),
            "expected a disabled notice, got {:?}",
            outcome.lines
        );
    }
}
