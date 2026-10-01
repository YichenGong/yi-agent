//! Slash command definitions and popup state for the TUI.

use crate::control_commands::{CommandSpec, ControlCommand};
use crate::tui::runtime_prefs::RuntimePreference;

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
    Agents,
    Agent,
    Events,
    Diff,
    Mailbox,
    Message,
    Pause,
    Resume,
    Cancel,
    Retry,
    Priority,
    Approve,
    Deny,
    Review,
    Accept,
    Rework,
    Reject,
    Budget,
    Daemon,
    Mcp,
    Runtime,
    Kanban,
}

impl SlashCommand {
    fn control_spec(self) -> Option<CommandSpec> {
        let command = match self {
            Self::Agents => ControlCommand::Agents,
            Self::Agent => ControlCommand::Agent,
            Self::Events => ControlCommand::Events,
            Self::Diff => ControlCommand::Diff,
            Self::Mailbox => ControlCommand::Mailbox,
            Self::Message => ControlCommand::Message,
            Self::Pause => ControlCommand::Pause,
            Self::Resume => ControlCommand::Resume,
            Self::Cancel => ControlCommand::Cancel,
            Self::Retry => ControlCommand::Retry,
            Self::Priority => ControlCommand::Priority,
            Self::Approve => ControlCommand::Approve,
            Self::Deny => ControlCommand::Deny,
            Self::Review => ControlCommand::Review,
            Self::Accept => ControlCommand::Accept,
            Self::Rework => ControlCommand::Rework,
            Self::Reject => ControlCommand::Reject,
            Self::Budget => ControlCommand::Budget,
            Self::Daemon => ControlCommand::Daemon,
            Self::Mcp => ControlCommand::Mcp,
            Self::Help => ControlCommand::Help,
            Self::Quit
            | Self::Clear
            | Self::Model
            | Self::Cost
            | Self::Compact
            | Self::Config
            | Self::Runtime
            | Self::Kanban => {
                return None;
            }
        };
        Some(command.spec())
    }

    /// The serialized command name (without the leading `/`).
    pub fn name(&self) -> &'static str {
        if let Some(spec) = self.control_spec() {
            return spec.slash_name;
        }
        match self {
            SlashCommand::Quit => "quit",
            SlashCommand::Clear => "clear",
            SlashCommand::Model => "model",
            SlashCommand::Cost => "cost",
            SlashCommand::Compact => "compact",
            SlashCommand::Config => "config",
            SlashCommand::Help => "help",
            SlashCommand::Agents => "agents",
            SlashCommand::Agent => "agent",
            SlashCommand::Events => "events",
            SlashCommand::Diff => "diff",
            SlashCommand::Mailbox => "mailbox",
            SlashCommand::Message => "message",
            SlashCommand::Pause => "pause",
            SlashCommand::Resume => "resume",
            SlashCommand::Cancel => "cancel",
            SlashCommand::Retry => "retry",
            SlashCommand::Priority => "priority",
            SlashCommand::Approve => "approve",
            SlashCommand::Deny => "deny",
            SlashCommand::Review => "review",
            SlashCommand::Accept => "accept",
            SlashCommand::Rework => "rework",
            SlashCommand::Reject => "reject",
            SlashCommand::Budget => "budget",
            SlashCommand::Daemon => "daemon",
            SlashCommand::Mcp => "mcp",
            SlashCommand::Runtime => "runtime",
            SlashCommand::Kanban => "superpowers-kanban",
        }
    }

    /// Short Chinese description shown in the popup.
    pub fn description(&self) -> &'static str {
        if let Some(spec) = self.control_spec() {
            return spec.description;
        }
        match self {
            SlashCommand::Quit => "退出程序",
            SlashCommand::Clear => "清空对话上下文",
            SlashCommand::Model => "切换模型 (需要参数)",
            SlashCommand::Cost => "显示 token 使用量",
            SlashCommand::Compact => "压缩对话历史",
            SlashCommand::Config => "显示当前配置",
            SlashCommand::Help => "显示帮助信息",
            SlashCommand::Agents => "显示当前 session 的 agent 任务树",
            SlashCommand::Agent => "显示单个 agent 详情 (需要任务 ID)",
            SlashCommand::Events => "查看任务事件 (需要任务 ID)",
            SlashCommand::Diff => "查看任务 diff (需要任务 ID)",
            SlashCommand::Mailbox => "查看任务 mailbox (需要任务 ID)",
            SlashCommand::Message => "向相邻 agent 发送消息",
            SlashCommand::Pause => "暂停任务",
            SlashCommand::Resume => "恢复任务",
            SlashCommand::Cancel => "取消任务",
            SlashCommand::Retry => "创建新的重试 attempt",
            SlashCommand::Priority => "调整任务优先级",
            SlashCommand::Approve => "批准权限请求",
            SlashCommand::Deny => "拒绝权限请求",
            SlashCommand::Review => "查看 delivery 审查",
            SlashCommand::Accept => "接受并集成子任务 delivery",
            SlashCommand::Rework => "请求子任务返工",
            SlashCommand::Reject => "拒绝子任务 delivery",
            SlashCommand::Budget => "查看或收窄任务预算",
            SlashCommand::Daemon => "管理本地 runtime daemon",
            SlashCommand::Mcp => "管理 MCP server 开关",
            SlashCommand::Runtime => "查看或设置子 Agent runtime 偏好",
            SlashCommand::Kanban => "Superpowers 看板：查看状态与开关",
        }
    }

    /// Required positional arguments, rendered consistently in help and completion.
    pub fn argument_usage(&self) -> Option<&'static str> {
        if let Some(spec) = self.control_spec() {
            return (!spec.usage.is_empty()).then_some(spec.usage);
        }
        match self {
            SlashCommand::Agents => Some("[--all|--active]"),
            SlashCommand::Model => Some("<model-name>"),
            SlashCommand::Agent => Some("<task-id>"),
            SlashCommand::Events | SlashCommand::Diff | SlashCommand::Mailbox => Some("<task-id>"),
            SlashCommand::Message => Some("<task-id> <text>"),
            SlashCommand::Cancel => Some("<task-id> [--recursive] [--confirm <token>]"),
            SlashCommand::Pause | SlashCommand::Resume | SlashCommand::Retry => Some("<task-id>"),
            SlashCommand::Priority => Some("<task-id> <level>"),
            SlashCommand::Runtime => Some("[ask|always|never]"),
            SlashCommand::Kanban => Some("[on|off|add <spec> <plan>]"),
            SlashCommand::Approve => Some("<request-id> [once|task]"),
            SlashCommand::Deny => Some("<request-id>"),
            SlashCommand::Review => Some("<task-id>"),
            SlashCommand::Accept => Some("<task-id> [--confirm <token>]"),
            SlashCommand::Rework => Some("<task-id> <feedback> [--confirm <token>]"),
            SlashCommand::Reject => Some("<task-id> <reason> [--confirm <token>]"),
            _ => None,
        }
    }

    /// Whether the command requires an argument.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn needs_arg(&self) -> bool {
        self.argument_usage().is_some()
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
            SlashCommand::Agents,
            SlashCommand::Agent,
            SlashCommand::Events,
            SlashCommand::Diff,
            SlashCommand::Mailbox,
            SlashCommand::Message,
            SlashCommand::Pause,
            SlashCommand::Resume,
            SlashCommand::Cancel,
            SlashCommand::Retry,
            SlashCommand::Priority,
            SlashCommand::Approve,
            SlashCommand::Deny,
            SlashCommand::Review,
            SlashCommand::Accept,
            SlashCommand::Rework,
            SlashCommand::Reject,
            SlashCommand::Budget,
            SlashCommand::Daemon,
            SlashCommand::Mcp,
            SlashCommand::Runtime,
            SlashCommand::Kanban,
        ]
    }

    /// Look up a command by its name (without leading `/`).
    pub fn from_name(name: &str) -> Option<SlashCommand> {
        if name == "?" {
            return Some(Self::Help);
        }
        // `/kanban` 是过渡别名：老名字仍可解析，但规范名是 `/superpowers-kanban`。
        if name == "kanban" {
            return Some(Self::Kanban);
        }
        Self::all().iter().copied().find(|cmd| cmd.name() == name)
    }
}

/// Render the same command catalog used by slash completion and dispatch.
pub fn help_text(target: Option<&str>) -> String {
    match target
        .map(|name| name.trim_start_matches('/'))
        .filter(|name| !name.is_empty())
    {
        Some(name) => match SlashCommand::from_name(name) {
            Some(command) => format!(
                "/{}{}\n{}",
                command.name(),
                command
                    .argument_usage()
                    .map(|usage| format!(" {usage}"))
                    .unwrap_or_default(),
                command.description()
            ),
            None => format!("未知命令: /{name}\n使用 /help 查看可用命令。"),
        },
        None => {
            let mut text = String::from("可用命令:\n");
            for command in SlashCommand::all() {
                let usage = command
                    .argument_usage()
                    .map(|usage| format!(" {usage}"))
                    .unwrap_or_default();
                text.push_str(&format!(
                    "  /{}{} {}\n",
                    command.name(),
                    usage,
                    command.description()
                ));
            }
            text
        }
    }
}

/// A parsed `/runtime` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAction {
    /// Show the current preference and its source.
    Status,
    /// Persist a new preference.
    Set(RuntimePreference),
}

/// Parse `/runtime` arguments. Returns a user-facing usage error on bad input.
pub fn parse_runtime_args(args: &str) -> Result<RuntimeAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(RuntimeAction::Status),
        ["ask"] => Ok(RuntimeAction::Set(RuntimePreference::Ask)),
        ["always"] => Ok(RuntimeAction::Set(RuntimePreference::Always)),
        ["never"] => Ok(RuntimeAction::Set(RuntimePreference::Never)),
        _ => Err("用法: /runtime [ask|always|never]".into()),
    }
}

/// A parsed `/mcp` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAction {
    /// List configured servers and their effective on/off state.
    Status,
    /// Toggle the session-wide master switch.
    Master(bool),
    /// Toggle one server.
    Server { name: String, on: bool },
}

/// Parse `/mcp` arguments. Returns a user-facing usage error on bad input.
pub fn parse_mcp_args(args: &str) -> Result<McpAction, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(McpAction::Status),
        ["on"] => Ok(McpAction::Master(true)),
        ["off"] => Ok(McpAction::Master(false)),
        ["enable", name] => Ok(McpAction::Server {
            name: (*name).into(),
            on: true,
        }),
        ["disable", name] => Ok(McpAction::Server {
            name: (*name).into(),
            on: false,
        }),
        _ => Err("用法: /mcp [on|off|enable <server>|disable <server>|status]".into()),
    }
}

/// Render `/mcp status` output. Pure so it is unit-testable without a manager.
pub fn render_mcp_status(master: bool, servers: &[(String, bool)]) -> String {
    if servers.is_empty() {
        return "未配置 MCP server".to_string();
    }
    let mut out = format!("MCP master: {}\n", if master { "on" } else { "off" });
    for (name, on) in servers {
        out.push_str(&format!("  {name}: {}\n", if *on { "on" } else { "off" }));
    }
    out
}

/// Maximum popup height in rows, including borders. Shared by the layout (which
/// reserves the rows) and the widget (which fills them) so they cannot drift.
const POPUP_MAX_ROWS: usize = 10;
/// Rows `compute_layout` is expected to keep outside the popup on a comfortable
/// terminal: history minimum (3), status bar (1), blank gap (1), input (1).
/// Queued messages and a wrapping input can claim more, so this is a floor for
/// the height estimate, not a guarantee. A squeezed layout hands the popup a
/// smaller area, which is why the draw path re-clamps rather than trusting it.
const MIN_NON_POPUP_ROWS: usize = 6;

/// State for the slash command popup shown above the input area.
pub struct CommandPopup {
    filtered: Vec<SlashCommand>,
    selected: usize,
    /// Index of the first command drawn in the visible window. The filtered
    /// list can be taller than the popup, so this is what keeps the highlighted
    /// row on screen.
    offset: usize,
    last_filter: String,
}

impl CommandPopup {
    /// Create a new popup with all commands visible.
    pub fn new() -> Self {
        Self {
            filtered: SlashCommand::all().to_vec(),
            selected: 0,
            offset: 0,
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
        self.offset = 0;
    }

    /// The filtered list of commands. Production draws through [`Self::visible`];
    /// this stays for assertions about the underlying list.
    #[cfg(test)]
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
        self.scroll_to_selection();
    }

    /// Move the selection down by one, wrapping to the top.
    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
        self.scroll_to_selection();
    }

    /// The currently selected command, if any.
    pub fn selected(&self) -> Option<SlashCommand> {
        self.filtered.get(self.selected).copied()
    }

    /// The index of the currently selected item.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Index of the first command actually drawn, clamped so the highlight sits
    /// inside a window of `height` rows even when the layout squeezes the popup
    /// shorter than [`Self::height`] predicted. [`Self::window_start`] reports
    /// this value, so the widget's highlight index and the drawn rows stay in
    /// the same coordinate space.
    pub fn window_start(&self, height: usize) -> usize {
        let window = self.effective_window(height);
        self.clamped_start(window)
    }

    /// The commands to draw, at most `height` of them: the effective window,
    /// which always contains the selection (unless the list is empty).
    pub fn visible(&self, height: usize) -> &[SlashCommand] {
        let start = self.clamped_start(self.effective_window(height));
        let end = (start + height).min(self.filtered.len());
        &self.filtered[start..end]
    }

    /// Rows the popup reserves for commands while handling keys -- `height` for
    /// a comfortable terminal. The draw path sizes from the live popup area and
    /// re-clamps, so a squeezed layout can never hide the selection either.
    fn window_height(&self) -> usize {
        POPUP_MAX_ROWS.saturating_sub(2).max(1)
    }

    /// Fold the selection into a `height`-row window without narrowing below
    /// what the cursor needs, so the highlight is always drawn.
    fn effective_window(&self, height: usize) -> usize {
        height.max(1).min(self.filtered.len().max(1))
    }

    /// Window start that keeps `selected` visible within `window` rows, and
    /// stays at 0 when the list is short enough to fit.
    fn clamped_start(&self, window: usize) -> usize {
        let start = if self.filtered.len() <= window {
            0
        } else if self.selected < self.offset {
            self.selected
        } else if self.selected >= self.offset + window {
            self.selected + 1 - window
        } else {
            self.offset
        };
        start.min(self.filtered.len().saturating_sub(window))
    }

    /// Slide the window so `selected` is inside it, touching only what is
    /// needed: at most one row of scrolling per move keeps the list steady
    /// rather than jumping.
    fn scroll_to_selection(&mut self) {
        let height = self.window_height();
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
    }

    /// Rows the popup occupies (borders included) for a terminal `area_height`
    /// tall: tall enough for every command, but capped so a long catalog can
    /// never crowd out history, status, or the input. An empty filter keeps the
    /// minimal border-only box rather than reserving the full cap.
    pub fn height(&self, area_height: u16) -> usize {
        let available = (area_height as usize).saturating_sub(MIN_NON_POPUP_ROWS);
        (self.filtered.len() + 2)
            .min(POPUP_MAX_ROWS)
            .min(available)
            .max(1)
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
    fn visible_window_contains_the_selection_after_scrolling_past_the_cap() {
        let mut popup = CommandPopup::new();
        let total = popup.filtered().len();
        let window = POPUP_MAX_ROWS - 2;
        assert!(total > window, "the catalog must be long enough to scroll");
        for _ in 0..total {
            popup.move_down();
            let drawn = popup.visible(window);
            assert!(
                drawn.contains(&popup.selected().unwrap()),
                "selection escaped the visible window at index {}",
                popup.selected_index()
            );
        }
    }

    /// A squeezed layout hands the popup fewer rows than [`CommandPopup::height`]
    /// predicted. The drawn window must still contain the highlight.
    #[test]
    fn drawn_window_clamps_the_selection_when_the_popup_is_squeezed() {
        let mut popup = CommandPopup::new();
        let squeezed = 6;
        for _ in 0..12 {
            popup.move_down();
        }
        assert!(popup.selected_index() >= squeezed);
        assert!(
            popup.visible(squeezed).contains(&popup.selected().unwrap()),
            "highlight escaped the squeezed window"
        );
        assert_eq!(
            popup.window_start(squeezed) + popup.visible(squeezed).len(),
            popup.selected_index() + 1,
            "the highlight must be the last drawn row"
        );
    }

    #[test]
    fn scrolling_past_the_cap_advances_the_window() {
        let mut popup = CommandPopup::new();
        let window = POPUP_MAX_ROWS - 2;
        // The window holds indices `0..window`; the selection stays inside it
        // until it steps one past the last visible row.
        for _ in 0..window - 1 {
            popup.move_down();
        }
        assert_eq!(
            popup.window_start(window),
            0,
            "window must not move until it fills"
        );
        popup.move_down();
        assert_eq!(
            popup.window_start(window),
            1,
            "window follows the selection down"
        );
    }

    #[test]
    fn move_up_wraps_and_keeps_the_last_command_visible() {
        let mut popup = CommandPopup::new();
        let window = POPUP_MAX_ROWS - 2;
        popup.move_up();
        assert_eq!(popup.selected_index(), popup.filtered().len() - 1);
        assert!(popup.visible(window).contains(&popup.selected().unwrap()));
    }

    #[test]
    fn filtering_resets_the_window_to_the_top() {
        let mut popup = CommandPopup::new();
        let window = POPUP_MAX_ROWS - 2;
        for _ in 0..12 {
            popup.move_down();
        }
        assert!(popup.window_start(window) > 0);
        popup.filter("c");
        assert_eq!(popup.window_start(window), 0);
    }

    #[test]
    fn height_lists_every_command_until_the_cap() {
        let popup = CommandPopup::new();
        assert_eq!(
            popup.height(24),
            10,
            "the full catalog is capped at 10 rows"
        );
        assert_eq!(popup.height(50), 10);
    }

    #[test]
    fn height_reserves_rows_for_history_on_a_short_terminal() {
        let popup = CommandPopup::new();
        // 8 rows - 6 reserved = 2 available, even though the catalog is longer.
        assert_eq!(popup.height(8), 2);
        assert_eq!(popup.height(0), 1);
    }

    #[test]
    fn height_shrinks_to_fit_a_filtered_list() {
        let mut popup = CommandPopup::new();
        popup.filter("cl");
        assert_eq!(popup.filtered().len(), 1);
        assert_eq!(popup.height(24), 3, "one command plus borders");
    }

    #[test]
    fn height_stays_minimal_when_nothing_matches() {
        let mut popup = CommandPopup::new();
        popup.filter("zzz");
        assert_eq!(popup.filtered().len(), 0);
        assert_eq!(popup.height(24), 2, "borders only: no blank rows budget");
    }

    #[test]
    fn visible_returns_the_whole_list_when_it_fits() {
        let mut popup = CommandPopup::new();
        popup.filter("r");
        let matched = popup.filtered().len();
        assert!(matched > 0 && matched < 8, "pick a filter that fits");
        assert_eq!(popup.visible(8).len(), matched);
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
    fn help_alias_and_contextual_help_use_the_command_catalog() {
        assert_eq!(SlashCommand::from_name("?"), Some(SlashCommand::Help));
        assert!(help_text(Some("agents")).contains("/agents"));
        assert!(help_text(Some("missing")).contains("未知命令"));
    }

    #[test]
    fn task_controls_publish_usage_in_help_and_metadata() {
        for command in [
            SlashCommand::Pause,
            SlashCommand::Resume,
            SlashCommand::Retry,
        ] {
            assert_eq!(command.argument_usage(), Some("<task-id>"));
            assert!(command.needs_arg());
            assert!(
                help_text(Some(command.name())).contains(&format!("/{} <task-id>", command.name()))
            );
        }
    }

    #[test]
    fn executable_commands_publish_their_argument_signatures() {
        for (command, usage) in [
            (SlashCommand::Agent, "<task-id>"),
            (SlashCommand::Message, "<task-id> <text>"),
            (SlashCommand::Cancel, "<task-id> [--recursive]"),
            (SlashCommand::Model, "<model-name>"),
        ] {
            assert_eq!(command.argument_usage(), Some(usage));
            assert!(command.needs_arg());
            assert!(
                help_text(Some(command.name())).contains(&format!("/{} {usage}", command.name()))
            );
        }
    }

    #[test]
    fn needs_arg_includes_commands_with_usage_metadata() {
        for cmd in SlashCommand::all() {
            assert_eq!(cmd.needs_arg(), cmd.argument_usage().is_some());
        }
    }

    #[test]
    fn all_commands_have_unique_names() {
        let names: Vec<&str> = SlashCommand::all().iter().map(|c| c.name()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate command names");
    }

    #[test]
    fn mcp_command_is_registered_with_usage() {
        assert_eq!(SlashCommand::from_name("mcp"), Some(SlashCommand::Mcp));
        assert_eq!(
            SlashCommand::Mcp.argument_usage(),
            Some("[on|off|enable <server>|disable <server>|status]")
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Mcp));
    }

    #[test]
    fn runtime_command_is_registered_with_usage() {
        assert_eq!(
            SlashCommand::from_name("runtime"),
            Some(SlashCommand::Runtime)
        );
        assert_eq!(
            SlashCommand::Runtime.argument_usage(),
            Some("[ask|always|never]")
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Runtime));
    }

    #[test]
    fn parse_runtime_args_accepts_status_and_every_state() {
        assert_eq!(parse_runtime_args(""), Ok(RuntimeAction::Status));
        assert_eq!(parse_runtime_args("status"), Ok(RuntimeAction::Status));
        assert_eq!(
            parse_runtime_args("ask"),
            Ok(RuntimeAction::Set(RuntimePreference::Ask))
        );
        assert_eq!(
            parse_runtime_args("always"),
            Ok(RuntimeAction::Set(RuntimePreference::Always))
        );
        assert_eq!(
            parse_runtime_args("never"),
            Ok(RuntimeAction::Set(RuntimePreference::Never))
        );
    }

    #[test]
    fn parse_runtime_args_rejects_unknown_input() {
        assert_eq!(
            parse_runtime_args("sometimes"),
            Err("用法: /runtime [ask|always|never]".into())
        );
        assert_eq!(
            parse_runtime_args("always never"),
            Err("用法: /runtime [ask|always|never]".into())
        );
    }

    #[test]
    fn parse_mcp_args_empty_and_status_mean_status() {
        assert_eq!(parse_mcp_args(""), Ok(McpAction::Status));
        assert_eq!(parse_mcp_args("status"), Ok(McpAction::Status));
    }

    #[test]
    fn parse_mcp_args_on_and_off_toggle_master() {
        assert_eq!(parse_mcp_args("on"), Ok(McpAction::Master(true)));
        assert_eq!(parse_mcp_args("off"), Ok(McpAction::Master(false)));
    }

    #[test]
    fn parse_mcp_args_enable_disable_toggle_one_server() {
        assert_eq!(
            parse_mcp_args("enable fs"),
            Ok(McpAction::Server {
                name: "fs".into(),
                on: true
            })
        );
        assert_eq!(
            parse_mcp_args("disable fs"),
            Ok(McpAction::Server {
                name: "fs".into(),
                on: false
            })
        );
    }

    #[test]
    fn parse_mcp_args_rejects_bad_input_with_usage_message() {
        assert_eq!(
            parse_mcp_args("bogus"),
            Err("用法: /mcp [on|off|enable <server>|disable <server>|status]".into())
        );
        assert_eq!(
            parse_mcp_args("enable"),
            Err("用法: /mcp [on|off|enable <server>|disable <server>|status]".into())
        );
    }

    #[test]
    fn render_mcp_status_without_servers_says_unconfigured() {
        assert_eq!(render_mcp_status(true, &[]), "未配置 MCP server");
    }

    #[test]
    fn render_mcp_status_lists_master_and_each_server() {
        assert_eq!(
            render_mcp_status(true, &[("fs".into(), true), ("gh".into(), false)]),
            "MCP master: on\n  fs: on\n  gh: off\n"
        );
    }

    #[test]
    fn the_legacy_kanban_name_still_resolves_to_the_command() {
        assert_eq!(SlashCommand::from_name("kanban"), Some(SlashCommand::Kanban));
        assert_eq!(
            SlashCommand::from_name("superpowers-kanban"),
            Some(SlashCommand::Kanban)
        );
    }

    #[test]
    fn kanban_is_a_known_slash_command_with_a_description() {
        let command = SlashCommand::Kanban;
        assert_eq!(command.name(), "superpowers-kanban");
        assert!(
            command.description().contains("Superpowers 看板"),
            "{}",
            command.description()
        );
        assert!(SlashCommand::all().contains(&SlashCommand::Kanban));
    }
}
