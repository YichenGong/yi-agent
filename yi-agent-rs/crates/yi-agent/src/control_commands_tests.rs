use super::control_commands::ControlCommand;
use super::tui::slash::SlashCommand;

#[test]
fn control_command_catalog_covers_the_documented_cli_and_slash_actions() {
    let names = ControlCommand::all()
        .iter()
        .map(|command| command.slash_name())
        .collect::<Vec<_>>();
    for required in [
        "agents", "agent", "events", "diff", "mailbox", "message", "pause", "resume", "cancel",
        "retry", "priority", "approve", "deny", "review", "accept", "rework", "reject", "budget",
        "daemon", "help",
    ] {
        assert!(names.contains(&required), "missing /{required}");
    }
    assert_eq!(
        names.len(),
        names.iter().collect::<std::collections::HashSet<_>>().len()
    );
}

#[test]
fn slash_completion_uses_every_control_command_name_from_the_catalog() {
    for command in ControlCommand::all() {
        assert!(
            SlashCommand::from_name(command.slash_name()).is_some(),
            "missing catalog command /{} from Slash completion",
            command.slash_name()
        );
    }
}
