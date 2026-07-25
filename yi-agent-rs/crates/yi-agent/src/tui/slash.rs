//! Slash command definitions and popup state for the TUI.

/// A slash command the user can invoke from the TUI input.
///
/// Adds metadata (description, arg requirements) needed for the popup UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Quit,
    Clear,
    Model,
    Cost,
    Compact,
    Config,
    Help,
}

impl SlashCommand {
    /// The serialized command name (without the leading `/`).
    pub fn name(&self) -> &'static str {
        match self {
            SlashCommand::Quit => "quit",
            SlashCommand::Clear => "clear",
            SlashCommand::Model => "model",
            SlashCommand::Cost => "cost",
            SlashCommand::Compact => "compact",
            SlashCommand::Config => "config",
            SlashCommand::Help => "help",
        }
    }

    /// Short Chinese description shown in the popup.
    pub fn description(&self) -> &'static str {
        match self {
            SlashCommand::Quit => "退出程序",
            SlashCommand::Clear => "清空对话上下文",
            SlashCommand::Model => "切换模型 (需要参数)",
            SlashCommand::Cost => "显示 token 使用量",
            SlashCommand::Compact => "压缩对话历史",
            SlashCommand::Config => "显示当前配置",
            SlashCommand::Help => "显示帮助信息",
        }
    }

    /// Whether the command requires an argument (e.g. `/model gpt-4`).
    #[allow(dead_code)]
    pub fn needs_arg(&self) -> bool {
        matches!(self, SlashCommand::Model)
    }

    /// All available commands, in popup display order.
    pub fn all() -> &'static [SlashCommand] {
        &[
            SlashCommand::Quit,
            SlashCommand::Clear,
            SlashCommand::Model,
            SlashCommand::Cost,
            SlashCommand::Compact,
            SlashCommand::Config,
            SlashCommand::Help,
        ]
    }

    /// Look up a command by its name (without leading `/`).
    pub fn from_name(name: &str) -> Option<SlashCommand> {
        Self::all().iter().copied().find(|cmd| cmd.name() == name)
    }
}

/// State for the slash command popup shown above the input area.
pub struct CommandPopup {
    filtered: Vec<SlashCommand>,
    selected: usize,
    last_filter: String,
}

impl CommandPopup {
    /// Create a new popup with all commands visible.
    pub fn new() -> Self {
        Self {
            filtered: SlashCommand::all().to_vec(),
            selected: 0,
            last_filter: String::new(),
        }
    }

    /// Filter the command list by a prefix string (the text after `/`).
    /// Resets the selection to the first item only if the filter changed.
    pub fn filter(&mut self, text: &str) {
        let text = text.trim();
        if text == self.last_filter {
            return; // No change, preserve selection
        }
        self.last_filter = text.to_string();
        if text.is_empty() {
            self.filtered = SlashCommand::all().to_vec();
        } else {
            self.filtered = SlashCommand::all()
                .iter()
                .copied()
                .filter(|cmd| cmd.name().starts_with(text))
                .collect();
        }
        self.selected = 0;
    }

    /// The filtered list of commands currently visible in the popup.
    pub fn filtered(&self) -> &[SlashCommand] {
        &self.filtered
    }

    /// Move the selection up by one, wrapping to the bottom.
    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.filtered.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    /// Move the selection down by one, wrapping to the top.
    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
    }

    /// The currently selected command, if any.
    pub fn selected(&self) -> Option<SlashCommand> {
        self.filtered.get(self.selected).copied()
    }

    /// The index of the currently selected item.
    pub fn selected_index(&self) -> usize {
        self.selected
    }
}

impl Default for CommandPopup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_empty_shows_all() {
        let popup = CommandPopup::new();
        assert_eq!(popup.filtered().len(), SlashCommand::all().len());
    }

    #[test]
    fn filter_prefix_matches() {
        let mut popup = CommandPopup::new();
        popup.filter("cl");
        let names: Vec<&str> = popup.filtered().iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["clear"]);
    }

    #[test]
    fn filter_no_match() {
        let mut popup = CommandPopup::new();
        popup.filter("xyz");
        assert!(popup.filtered().is_empty());
        assert!(popup.selected().is_none());
    }

    #[test]
    fn filter_resets_selection() {
        let mut popup = CommandPopup::new();
        popup.move_down();
        popup.move_down();
        assert!(popup.selected > 0);
        popup.filter("c");
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn move_down_wraps() {
        let mut popup = CommandPopup::new();
        let len = popup.filtered().len();
        for _ in 0..len {
            popup.move_down();
        }
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn move_up_wraps() {
        let mut popup = CommandPopup::new();
        let len = popup.filtered().len();
        popup.move_up();
        assert_eq!(popup.selected, len - 1);
    }

    #[test]
    fn move_up_down_on_empty_filtered() {
        let mut popup = CommandPopup::new();
        popup.filter("xyz");
        popup.move_up();
        popup.move_down();
        assert!(popup.selected().is_none());
    }

    #[test]
    fn selected_returns_correct_command() {
        let mut popup = CommandPopup::new();
        popup.move_down();
        assert_eq!(popup.selected(), Some(SlashCommand::Clear));
        popup.move_down();
        assert_eq!(popup.selected(), Some(SlashCommand::Model));
    }

    #[test]
    fn from_name_finds_command() {
        assert_eq!(SlashCommand::from_name("quit"), Some(SlashCommand::Quit));
        assert_eq!(SlashCommand::from_name("clear"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::from_name("xyz"), None);
    }

    #[test]
    fn needs_arg_only_for_model() {
        for cmd in SlashCommand::all() {
            assert_eq!(cmd.needs_arg(), *cmd == SlashCommand::Model);
        }
    }

    #[test]
    fn all_commands_have_unique_names() {
        let names: Vec<&str> = SlashCommand::all().iter().map(|c| c.name()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate command names");
    }
}
