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
            // 读路径走 `_for_read`：新目录优先，回退旧目录（迁移期兼容）。
            let state_dir = yi_agent_board_ui::inbox::board_state_dir_for_read(workdir);
            let cards = yi_agent_board_ui::state::load_cards(&state_dir);
            let view = yi_agent_board_ui::view::BoardView {
                switch_on: resolved.value.is_enabled(),
                switch_source: source,
                cards,
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
        _ if argument.starts_with("add ") || argument == "add" => {
            let mut parts = argument.split_whitespace();
            let _verb = parts.next();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(spec), Some(plan), None) => {
                    let state_dir = yi_agent_board_ui::inbox::board_state_dir(workdir);
                    let id = card_id_for(spec, plan);
                    match yi_agent_board_ui::inbox::deliver_card(&state_dir, &id, spec, plan) {
                        Ok(()) => KanbanOutcome {
                            lines: vec![format!("Superpowers 看板: delivered {id} to inbox")],
                            toggled_to: None,
                        },
                        Err(error) => KanbanOutcome {
                            lines: vec![format!("Superpowers 看板: could not deliver: {error}")],
                            toggled_to: None,
                        },
                    }
                }
                _ => KanbanOutcome {
                    lines: vec!["usage: /kanban add <spec> <plan>".to_string()],
                    toggled_to: None,
                },
            }
        }
        _ => KanbanOutcome {
            lines: vec!["usage: /kanban [on|off|run|add <spec> <plan>]".to_string()],
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
    fn the_board_shows_cards_read_from_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        // 开板才能看到卡片（关闭时只渲染「已禁用」）。
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent/preferences.json"),
            r#"{"superpowers_board":true}"#,
        )
        .unwrap();
        let state_dir = yi_agent_board_ui::inbox::board_state_dir(dir.path());
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            yi_agent_board_ui::state::board_state_path(&state_dir),
            r#"{"cards":[{"id":"card-1","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"running","enqueued_at":"2026-10-01T09:00:00+08:00","order":0}],"next_order":1}"#,
        )
        .unwrap();

        let outcome = handle_kanban(dir.path(), "");
        assert!(
            outcome.lines.iter().any(|line| line.contains("card-1")),
            "the live card should be rendered, got {:?}",
            outcome.lines
        );
        assert!(
            outcome.lines.iter().any(|line| line.contains("running")),
            "the card state should be visible, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn add_delivers_a_card_to_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "add a.spec.md a.plan.md");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("inbox") || outcome.lines[0].contains("投递"),
            "expected a delivery acknowledgement, got {:?}",
            outcome.lines
        );
        let state_dir = yi_agent_board_ui::inbox::board_state_dir(dir.path());
        let entries: Vec<_> = std::fs::read_dir(yi_agent_board_ui::inbox::inbox_dir(&state_dir))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1, "exactly one delivery file is written");
    }

    #[test]
    fn add_with_missing_paths_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "add only-one-arg");
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

    #[test]
    fn saving_the_runtime_preference_does_not_drop_the_board_switch() {
        let dir = tempfile::tempdir().unwrap();
        handle_kanban(dir.path(), "on");
        crate::tui::runtime_prefs::save(
            dir.path(),
            crate::tui::runtime_prefs::RuntimePreference::Never,
        )
        .unwrap();
        assert_eq!(
            yi_agent_board_ui::switch::read_layer(&yi_agent_board_ui::switch::project_path(
                dir.path()
            )),
            Some(yi_agent_board_ui::switch::BoardSwitch::Enabled),
            "a runtime-pref save must not clobber the board switch in preferences.json"
        );
    }
}

/// 由一对路径派生卡片 id：取两文件名主干，非字母数字折叠为 `-`。
fn card_id_for(spec: &str, plan: &str) -> String {
    let stem = |path: &str| {
        std::path::Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let slug = |text: &str| {
        let mut out = String::new();
        let mut last_dash = false;
        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        out.trim_matches('-').to_string()
    };
    let id = format!("{}-{}", slug(&stem(spec)), slug(&stem(plan)));
    if id == "-" || id.is_empty() {
        "card".to_string()
    } else {
        id
    }
}
