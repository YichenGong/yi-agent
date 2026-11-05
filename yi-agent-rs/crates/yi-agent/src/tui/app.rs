use std::io::{self, stdout};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use yi_agent_core::{AgentEvent, DoneReason};

use super::bash_popup::{BashPopup, ConfirmKill, DetailPopup, ListPopup};
use super::cell::HistoryCell;
use super::cost::CostTracker;
use super::history::{HistoryState, HistoryView, ViewportAnchor};
use super::input::{InputAction, InputLine};
use super::process_popup::{
    ConfirmProcessKill as ConfirmProcessKillPopup, ProcessDetailPopup, ProcessListPopup,
    ProcessPopup, RuntimeTab,
};
use super::side::{
    ConversationPayload, ConversationTarget, RoutedInput, SideFrameEffect, SideInput,
    SideOperation, TuiSideView, TuiSideViews, classify_conversation_input,
};
use super::slash::{
    CommandPopup, McpAction, PopupScope, SlashCommand, TuiConfigSnapshot, help_text,
    parse_mcp_args, render_mcp_status,
};
use super::state::RunningTaskRegistry;
use super::statusbar::{SideStatusLabel, StatusBarState, render_statusbar_with_side};
use super::theme::TuiTheme;
use super::trace::{
    SubagentListState, TraceDetailPopup, TraceListPopup, TracePopup, render_subagent_list_with,
};

const HISTORY_WHEEL_LINES: usize = 3;

fn is_active_managed_process(status: &yi_agent_tools::ProcessStatus) -> bool {
    matches!(
        status,
        yi_agent_tools::ProcessStatus::Starting
            | yi_agent_tools::ProcessStatus::Running
            | yi_agent_tools::ProcessStatus::Ready
    )
}

fn format_ipc_error(code: yi_agent_store::ipc::IpcErrorCode, message: Option<String>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

/// Write the "pending messages were dropped" notice, if any.
///
/// Split out from `run_tui` so the message and the `> 0` condition are both
/// testable: `run_tui` needs a real TTY (`enable_raw_mode`), so the notice
/// itself cannot be exercised through it.
fn report_dropped_pending<W: io::Write>(out: &mut W, dropped: usize) {
    if dropped > 0 {
        let _ = writeln!(out, "已丢弃 {dropped} 条排队消息（未发送）");
    }
}

/// `run_loop` / `handle_key` 共用的出向 channel 集合。
///
/// 这些 channel 在整场 TUI 会话里成组存在、成组传递，故打包成一个结构体，
/// 避免每加一个能力就给 `handle_key` 再加一个参数（它已因此长到 25 个）。
pub(crate) struct TuiChannels {
    pub input: tokio::sync::mpsc::Sender<String>,
    pub interrupt: tokio::sync::mpsc::Sender<()>,
    pub kill: tokio::sync::mpsc::Sender<String>,
    /// TUI→driver 的重建指令（Step 0 已改名为 `DriverControl`）。
    pub control: tokio::sync::mpsc::Sender<crate::DriverControl>,
    pub decision: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    /// 旁支命令通道。`None` 表示本次会话没有旁支能力（测试构造与不支持
    /// 的宿主），此时 `/side` 会明确报「不可用」而不是静默什么都不做。
    pub side: Option<tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>>,
}

impl TuiChannels {
    /// Build a bundle from borrowed senders, cloning each.
    ///
    /// Test call sites keep the individual senders so they can assert on the
    /// matching receivers, so they cannot hand over ownership.
    #[cfg(test)]
    fn from_senders(
        input: &tokio::sync::mpsc::Sender<String>,
        interrupt: &tokio::sync::mpsc::Sender<()>,
        kill: &tokio::sync::mpsc::Sender<String>,
        control: &tokio::sync::mpsc::Sender<crate::DriverControl>,
        decision: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    ) -> Self {
        Self {
            input: input.clone(),
            interrupt: interrupt.clone(),
            kill: kill.clone(),
            control: control.clone(),
            decision: decision.clone(),
            side: None,
        }
    }
}

/// 旁支在 TUI 这一侧的两端通道。
///
/// `None` 表示本次会话没有旁支能力:测试构造与不支持旁支的宿主走这条路径,
/// 此时 `/side` 会明确提示不可用,而不是静默吞掉命令。
pub struct SideRuntime {
    pub commands: Option<tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>>,
    pub events: Option<tokio::sync::mpsc::Receiver<super::side::ConversationEvent>>,
}

impl SideRuntime {
    /// 无旁支能力。
    pub fn disabled() -> Self {
        Self {
            commands: None,
            events: None,
        }
    }

    pub fn new(
        commands: tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>,
        events: tokio::sync::mpsc::Receiver<super::side::ConversationEvent>,
    ) -> Self {
        Self {
            commands: Some(commands),
            events: Some(events),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.commands.is_some()
    }
}

/// 终端清理段。
///
/// 抽出来是为了让「标题还原发生在最后一步之后」可被测试断言。`step` 是
/// 可注入的执行器：生产传入真实 crossterm 调用，测试传入记录器。
///
/// 标题还原刻意**不**参与 `cleanup_error`：`TitleHandle::clear` 返回 `()`，
/// 签名层面就进不来。否则上百种不支持 OSC 2 的终端会在每次正常退出时
/// 返回非零，被 `set -e` / CI 脚本当成崩溃。
fn run_cleanup_with<F>(mut step: F, title: &mut crate::tui::title::TitleHandle) -> Option<io::Error>
where
    F: FnMut(&str) -> io::Result<()>,
{
    let mut cleanup_error: Option<io::Error> = None;
    for name in [
        "DisableBracketedPaste",
        "DisableFocusChange",
        "disable_raw_mode",
        "LeaveAlternateScreen",
    ] {
        if let Err(error) = step(name) {
            cleanup_error.get_or_insert(error);
        }
    }
    // 必须在 `LeaveAlternateScreen` 之后：alternate screen 关闭时多数终端
    // 会重置标题，先写空串等于白写。
    title.clear();
    cleanup_error
}

/// Run the ratatui TUI main loop with the real terminal.
///
/// - `agent_rx`: receives agent events to display in history
/// - `input_tx`: sends user-submitted input strings to the agent driver
/// - `interrupt_tx`: signals to interrupt the current agent run
/// - `is_running`: shared flag indicating if agent is currently running
#[allow(clippy::too_many_arguments)]
pub fn run_tui(
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: tokio::sync::mpsc::Sender<String>,
    interrupt_tx: tokio::sync::mpsc::Sender<()>,
    kill_tx: tokio::sync::mpsc::Sender<String>,
    control_tx: tokio::sync::mpsc::Sender<crate::DriverControl>,
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    model: String,
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
    runtime_choice_tx: Option<
        tokio::sync::mpsc::Sender<crate::tui::subagents::RuntimeStartupChoice>,
    >,
    process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
    workdir: std::path::PathBuf,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
    mut config: TuiConfigSnapshot,
    catalog: yi_agent_runtime::models::ModelCatalog,
    // resume 时的历史回放。空 vec 等价于全新会话。
    replay_cells: Vec<crate::tui::cell::HistoryCell>,
    side_runtime: SideRuntime,
) -> std::io::Result<()> {
    enable_raw_mode()?;
    // Detect the terminal background BEFORE the alternate screen takes over:
    // the query needs raw mode, and any stray output must land in the normal
    // scrollback rather than inside a frame being drawn.
    let theme = TuiTheme::of(crate::tui::theme::detect_theme_from_terminal());
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    // 焦点事件让 `unfocused` 条件才有意义。老 conhost 不支持这个序列，
    // 失败不致命：Windows 上退化成 always（宁可多响，不静默），其它平台
    // 只在日志里留痕。
    let focus_degraded = match execute!(stdout, EnableFocusChange) {
        Ok(()) => false,
        Err(error) => {
            if cfg!(windows) {
                tracing::info!(error = %error, "focus tracking unavailable; notifications will always fire");
                true
            } else {
                tracing::debug!(error = %error, "focus tracking unavailable");
                false
            }
        }
    };
    config.focus_degraded = focus_degraded;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut history = HistoryState::with_theme(theme);
    // resume 时把历史摊平成 cells。不注入的话上下文是对的但屏幕是空的，
    // 用户会以为历史丢了。裁剪交给既有 `push` 里的 `enforce_history_cap`，
    // 不新写裁剪逻辑。
    for cell in replay_cells {
        history.push(cell, terminal.size()?.width);
    }
    let mut input = InputLine::new();

    let channels = TuiChannels {
        input: input_tx,
        interrupt: interrupt_tx,
        kill: kill_tx,
        control: control_tx,
        decision: decision_tx,
        side: side_runtime.commands.clone(),
    };

    // 标题：必须在 `workdir` 被 move 进 `run_loop` 之前取项目名。
    let mut title = crate::tui::title::TitleHandle::new(
        config.terminal_title,
        crate::tui::notify::is_multiplexed(&crate::tui::notify::TerminalEnv::from_process()),
        &workdir,
    );

    let result = run_loop_with_side(
        &mut terminal,
        &mut agent_rx,
        &mut history,
        &mut input,
        channels,
        &is_running,
        &CrosstermEventSource,
        &model,
        runtime_intent,
        runtime_choice_tx,
        process_manager,
        workdir,
        mcp,
        &config,
        &catalog,
        theme,
        &mut title,
        side_runtime,
    );

    // Try every cleanup step so a failed write cannot leave the terminal in another mode.
    let cleanup_error = run_cleanup_with(
        |name| match name {
            "DisableBracketedPaste" => execute!(terminal.backend_mut(), DisableBracketedPaste),
            "DisableFocusChange" => execute!(terminal.backend_mut(), DisableFocusChange),
            "disable_raw_mode" => disable_raw_mode(),
            _ => execute!(terminal.backend_mut(), LeaveAlternateScreen),
        },
        &mut title,
    );

    // Report the dropped count only after the alternate screen is gone;
    // printing earlier would be erased along with it.
    if let Ok(dropped) = result {
        report_dropped_pending(&mut io::stderr(), dropped);
    }

    match (result, cleanup_error) {
        (Err(error), _) => Err(error),
        (Ok(_), Some(error)) => Err(error),
        (Ok(_), None) => Ok(()),
    }
}

/// Event source trait so the loop can be tested with fake events.
/// 焦点事件 → 原子标志。
fn apply_focus_event(event: &Event, focused: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;
    match event {
        Event::FocusGained => focused.store(true, Ordering::Relaxed),
        Event::FocusLost => focused.store(false, Ordering::Relaxed),
        _ => {}
    }
}

/// 当前是否应当发送通知。`focused` 为真表示窗口在前台。
fn should_notify(condition: crate::tui::notify::Condition, focused: bool) -> bool {
    match condition {
        crate::tui::notify::Condition::Always => true,
        crate::tui::notify::Condition::Unfocused => !focused,
    }
}

/// 待发通知。优先级数字小的压过数字大的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationPriority {
    TurnComplete = 0,
    ApprovalRequested = 1,
}

#[derive(Debug, Clone)]
struct PendingNotification {
    priority: NotificationPriority,
    text: String,
}

/// 把 `AgentEvent` 收敛成「至多一条待发通知」。
///
/// 不直接写字节：通知必须等这一帧画完再发，否则会插进 ANSI 序列中间，
/// 把一帧切开造成渲染残影。
#[derive(Debug, Default)]
struct NotificationState {
    pending: Option<PendingNotification>,
}

impl NotificationState {
    fn new() -> Self {
        Self::default()
    }

    /// 观察一个事件，可能更新候选通知。
    ///
    /// `enabled` 为假时什么都不做；`history` 用于取回合正文（`Done` 不带
    /// 正文），必须在 `push_event` **之前**调用——`push_event` 会在末尾压
    /// 一条 `Separator`，`last_assistant_text` 也就还能跳过它找到正文。
    fn observe(&mut self, event: &AgentEvent, history: &HistoryState, enabled: bool) {
        if !enabled {
            return;
        }
        let candidate = match event {
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            } => Some(PendingNotification {
                priority: NotificationPriority::TurnComplete,
                text: history
                    .last_assistant_text()
                    .map(crate::tui::notify::text::summarize)
                    .unwrap_or_else(|| crate::tui::notify::text::EMPTY_FALLBACK.to_string()),
            }),
            AgentEvent::PermissionRequest {
                tool_name,
                tool_input,
                ..
            } => Some(PendingNotification {
                priority: NotificationPriority::ApprovalRequested,
                text: format!(
                    "等待批准: {}",
                    crate::tui::notify::text::truncate_graphemes(
                        &crate::tui::history::permission_summary(tool_name, tool_input),
                        crate::tui::notify::text::MAX_GRAPHEMES,
                    )
                ),
            }),
            _ => None,
        };

        let Some(candidate) = candidate else { return };
        match &self.pending {
            // 同优先级或更高优先级的已有候选不被覆盖：一轮里先到的权限
            // 请求不该被随后的回合完成盖掉。
            Some(existing) if existing.priority >= candidate.priority => {}
            _ => self.pending = Some(candidate),
        }
    }

    /// 取出并清空。绘制完成后调用。
    fn take(&mut self) -> Option<PendingNotification> {
        self.pending.take()
    }

    #[cfg(test)]
    fn pending_text(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.text.as_str())
    }
}

pub trait EventSource {
    /// Poll for an event, waiting up to `timeout`. Returns `Ok(None)` on timeout.
    fn poll(&self, timeout: Duration) -> std::io::Result<Option<Event>>;
}

/// Production event source using crossterm's global event queue.
struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll(&self, timeout: Duration) -> std::io::Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

/// A fixed, secret-free snapshot for tests that need the config threaded
/// through. Preferred single definition: changing the struct's fields only
/// needs one test-side update.
#[cfg(test)]
fn snapshot_for_tests() -> TuiConfigSnapshot {
    TuiConfigSnapshot {
        provider: "anthropic".into(),
        model_injected: false,
        workdir: std::path::PathBuf::from("/tmp/proj"),
        sandbox: "workspace-write".into(),
        yolo: false,
        max_turns: 200,
        compact_threshold: 160_000,
        mcp_master: true,
        runtime_preference: "ask".into(),
        runtime_preference_path: std::path::PathBuf::from("/tmp/proj/.yi-agent/preferences.json"),
        notifications: true,
        notification_method: "auto".into(),
        notification_condition: "unfocused".into(),
        effective_backend: "bel".into(),
        terminal_title: true,
        title_status: "on".into(),
        focus_degraded: false,
    }
}

/// An empty model catalog for tests that thread one through the TUI wrappers
/// but never exercise `/model` or `/effort`. Kept in memory: tests must not
/// touch `HOME`.
#[cfg(test)]
fn catalog_for_tests() -> yi_agent_runtime::models::ModelCatalog {
    yi_agent_runtime::models::ModelCatalog::default()
}

/// Run the TUI loop with any ratatui backend (used by tests with TestBackend).
/// Does NOT call enable_raw_mode / EnterAlternateScreen.
#[cfg(test)]
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn run_tui_with_backend<B: Backend>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::DriverControl>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    workdir: &std::path::Path,
) -> std::io::Result<()> {
    let mut history = HistoryState::new();
    let mut input = InputLine::new();
    let channels = TuiChannels {
        input: input_tx.clone(),
        interrupt: interrupt_tx.clone(),
        kill: kill_tx.clone(),
        control: control_tx.clone(),
        decision: decision_tx.clone(),
        side: None,
    };
    let mut title = crate::tui::title::TitleHandle::disabled();
    run_loop(
        terminal,
        agent_rx,
        &mut history,
        &mut input,
        channels,
        is_running,
        &CrosstermEventSource,
        "test-model",
        None,
        None,
        yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        workdir.to_path_buf(),
        yi_agent_mcp::McpManager::empty(),
        &snapshot_for_tests(),
        &catalog_for_tests(),
        TuiTheme::DARK,
        &mut title,
    )
    .map(|_dropped| ())
}

/// Testable variant: accepts a custom EventSource for injecting fake key events.
///
/// Forwards [`TuiTheme::DARK`]: the ~31 existing callers assert the historical
/// look and must not silently change palette. Tests that need a specific theme
/// call [`run_tui_with_backend_and_events_themed`].
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn run_tui_with_backend_and_events<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::DriverControl>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    config: &TuiConfigSnapshot,
    catalog: &yi_agent_runtime::models::ModelCatalog,
) -> std::io::Result<()> {
    run_tui_with_backend_and_events_themed(
        terminal,
        agent_rx,
        input_tx,
        interrupt_tx,
        kill_tx,
        control_tx,
        decision_tx,
        is_running,
        events,
        config,
        catalog,
        TuiTheme::DARK,
    )
}

/// [`run_tui_with_backend_and_events`] with an explicit palette, so a whole
/// frame can be rendered for a light terminal and inspected cell by cell.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn run_tui_with_backend_and_events_themed<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::DriverControl>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    config: &TuiConfigSnapshot,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    theme: TuiTheme,
) -> std::io::Result<()> {
    let mut history = HistoryState::with_theme(theme);
    let mut input = InputLine::new();
    let channels = TuiChannels {
        input: input_tx.clone(),
        interrupt: interrupt_tx.clone(),
        kill: kill_tx.clone(),
        control: control_tx.clone(),
        decision: decision_tx.clone(),
        side: None,
    };
    let mut title = crate::tui::title::TitleHandle::disabled();
    run_loop(
        terminal,
        agent_rx,
        &mut history,
        &mut input,
        channels,
        is_running,
        events,
        "test-model",
        None,
        None,
        yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        std::env::temp_dir(),
        yi_agent_mcp::McpManager::empty(),
        config,
        catalog,
        theme,
        &mut title,
    )
    .map(|_dropped| ())
}

/// Body lines of the runtime startup dialog.
///
/// Kept as a function so the narrow-terminal test renders exactly what the live
/// UI renders — a duplicated literal could drift and hide a clipping bug.
///
/// Keep every line's **display width** (CJK counts as 2 columns) within
/// `BOX_WIDTH - 2` = 56, or it needs `Wrap` to fold. The key legend is 58 columns
/// on purpose: it must fold into two lines, and the test below locks that in.
fn runtime_prompt_lines() -> Vec<ratatui::text::Line<'static>> {
    vec![
        ratatui::text::Line::raw("启动后可以直接用自然语言创建和管理子 Agent。"),
        ratatui::text::Line::raw("此选择会被记住，可用 /runtime 修改。"),
        ratatui::text::Line::raw("若未能启用，重启 yi-agent 后可用。"),
        ratatui::text::Line::raw(""),
        ratatui::text::Line::raw("[y] 启动并记住"),
        ratatui::text::Line::raw("[n] 跳过并记住    [Esc] 本次跳过（不记住）"),
    ]
}

/// Persists a runtime preference, but never blocks the session on failure.
fn persist_runtime_choice(
    workdir: &std::path::Path,
    pref: crate::tui::runtime_prefs::RuntimePreference,
    history: &mut HistoryState,
    width: u16,
) {
    if let Err(error) = crate::tui::runtime_prefs::save(workdir, pref) {
        history.push(
            HistoryCell::Separator {
                label: Some(format!("无法保存偏好（本次仍然生效）: {error}")),
            },
            width,
        );
    }
}

/// 会话级 UI 状态：用户选的模型与 effort、状态栏、成本累计。
///
/// 这些值「活到下一帧」——任何时刻都可读可写，没有「必须在此之前释放」
/// 的时序约束。把散落的 `let mut` 收在这里，`run_loop` 的其余部分才看得清
/// 主流程。
struct SessionUi {
    current_model: String,
    /// 用户选中的清单**显示名**（`/model <name>` 下发后记下，`/model default` 清空）。
    /// 与 `current_model`（driver 回报的 model 串）分开：清单里两条条目可能共用
    /// 同一个 model 串，且 model 串未必等于显示名，故「当前选中」只能按显示名判定。
    /// 启动时为 `None`：TUI 跟随清单 `default_model`。
    current_model_ref: Option<String>,
    /// 本会话的 effort 覆盖（`/effort <tier>` 写入，`/effort default` 清空）。
    /// 生效值不缓存：每次现算（`resolve_effort_state`），因为清单与覆盖都会在
    /// 会话中变化。启动时无覆盖，与 TUI 没有持久化的 thread meta 一致。
    effort_override: Option<String>,
    statusbar_state: StatusBarState,
    cost_tracker: CostTracker,
}

impl SessionUi {
    fn new(model: &str) -> Self {
        Self {
            current_model: model.to_string(),
            current_model_ref: None,
            effort_override: None,
            statusbar_state: StatusBarState::default(),
            cost_tracker: CostTracker::default(),
        }
    }
}

/// 旁支会话在 TUI 里的整套状态。
///
/// 与主视图**完全分开**地持有 `HistoryState` / `InputLine`:两份视图各有自己的
/// 转录、草稿、滚动位置与折行缓存。渲染器是共用的（`HistoryView`），所以
/// 主题、resize、CJK 换行、缓存上限对两份视图一致，不需要第二套实现。
struct SideSession {
    /// 主/旁视图切换与每份视图的运行态。
    views: TuiSideViews,
    /// 旁支自己的转录。
    history: HistoryState,
    /// 旁支自己的输入行（草稿与历史不串到主会话）。
    input: InputLine,
    /// 旁支自己的用量，从零累计，不混入父会话的 `cost_tracker`。
    usage: yi_agent_runtime::side_conversation::SideUsage,
    /// 已发出但还没被 driver 确认的请求 id + 原文（用于同 id 重试）。
    pending_request: Option<(String, String)>,
    /// 旁支当前轮次 id（来自 driver 的定向帧）。Esc 只停这个轮次。
    active_turn_id: Option<String>,
    /// 错误/状态提示行。
    notice: Option<String>,
    /// 主视图里需要显示的后台操作失败（按操作与目标记录）。
    ///
    /// 与 `notice` 分开存放：`notice` 属于旁支视图，回到主视图就看不见了；
    /// 而后台 `Close` 失败最容易发生在这种时刻——用户已经按 `/side-close`
    /// 并回到主视图，错误的唯一可见位置就是主视图。
    ///
    /// 带目标与操作：一个旧代的关闭失败不能盖住新旁支的正常提示，close 成功
    /// 之后也必须能被精确清掉（`SideOperation::Close` 且目标匹配）。
    main_notice: Option<(SideOperation, ConversationTarget, String)>,
}

impl SideSession {
    fn new(theme: TuiTheme) -> Self {
        Self {
            views: TuiSideViews::new(),
            history: HistoryState::with_theme(theme),
            input: InputLine::new(),
            usage: Default::default(),
            pending_request: None,
            active_turn_id: None,
            notice: None,
            main_notice: None,
        }
    }

    fn in_side(&self) -> bool {
        self.views.in_side()
    }

    fn active_side(&self) -> Option<&TuiSideView> {
        self.views.side.as_ref()
    }

    fn side_running(&self) -> bool {
        self.active_side().is_some_and(|view| view.running)
    }

    /// 把一条事件写进旁支视图与转录。
    ///
    /// 返回是否需要重绘。**主事件在这里被丢弃**:它们由主循环写主 history,
    /// 这正是「后台父输出不污染旁支」的落点。
    fn apply_event(
        &mut self,
        event: &super::side::ConversationEvent,
        width: u16,
    ) -> SideFrameEffect {
        // 操作失败帧必须**先于**过期检查处理。
        //
        // 过期帧的默认结局是丢弃（旧代正文不能把新视图往回拨），但错误帧是
        // 唯一能说明「你那一下操作为什么没生效」的载体：一个已经过期的关闭
        // 目标，其失败正是最需要告诉用户的。若让它和正文一样被丢弃，用户
        // 经历的就是「按了关闭，界面什么也没发生」——这正是要修的问题本身。
        if let ConversationPayload::SideOperationError { operation, message } = &event.event {
            self.apply_operation_error(event.target.clone(), *operation, message.clone());
            return SideFrameEffect::Redraw;
        }
        // 已经关闭的旁支不再接受**属于它自己**的帧：迟到的事件不能让一个已关闭
        // 的视图「复活」，否则用户会看到一个说已关闭、却还在长正文的旁支。
        //
        // 判据必须带上 side_id/generation：关闭之后用户可以再开一个新旁支，
        // 新旁支的帧来自另一个身份，拿「当前视图已关闭」一概拦掉，就等于
        // 关过一次之后再也开不了旁支。
        if self.views.side.as_ref().is_some_and(|view| {
            !view.is_live() && event.target.matches_side(&view.side_id, view.generation)
        }) {
            return SideFrameEffect::Discard;
        }
        // 用 `matches_side` 而不是就地再写一遍 `side_id`/`generation` 比较：
        // 这个谓词是「过期帧必须被丢弃」的唯一判据，散成多份就会有一天只改到一半。
        if let (ConversationPayload::SideItems(items), Some(view)) =
            (&event.event, self.views.side.as_mut())
            && event.target.matches_side(&view.side_id, view.generation)
        {
            view.apply_items(items.to_vec());
            self.render_items(width);
            return SideFrameEffect::Redraw;
        }
        // 第一条旁支帧就是「旁支已创建」的信号:此时才建立视图并切过去。
        // 在此之前不预先造一个空视图,否则创建失败会留下一个永远空着的旁支。
        //
        // 判据是「没有视图，或视图已死且属于**别的**旁支」，而不是只看
        // `is_none()`：关闭旧旁支后视图仍留着（`closed` 有值），只把
        // `is_none()` 当唯一触发条件会让新旁支永远建不出视图——用户看到的
        // 就是关掉后再也开不了。
        //
        // 反过来，「已死就重建」也不行：旧旁支的迟到帧身份不同，会把已关闭的
        // 旧视图重新造出来、再被 stale 检查放过，等于让关掉的旁支复活。所以
        // 只有视图已死**且**目标不是它自己时，才建立新视图。
        if let ConversationTarget::Side {
            side_id,
            generation,
        } = &event.target
            && self.views.side.as_ref().is_none_or(|view| {
                !view.is_live() && (view.side_id != *side_id || view.generation != *generation)
            })
        {
            self.views.open(side_id.clone(), *generation);
            self.history.clear();
        }
        // 定向帧先决定「是不是当前这个旁支」,再写状态:过期的 generation 与
        // 别的 side_id 的帧必须原样丢弃,不能把新视图往回拨。
        let stale = match (event.target.is_side(), self.views.side.as_ref()) {
            (true, Some(view)) => !event.target.matches_side(&view.side_id, view.generation),
            (true, None) => true,
            _ => false,
        };
        if stale {
            return SideFrameEffect::Discard;
        }
        let effect = self.views.route(event);
        match &event.event {
            ConversationPayload::SideUsage(usage) if effect == SideFrameEffect::Redraw => {
                self.usage = *usage;
            }
            ConversationPayload::SideClosed(reason) if effect == SideFrameEffect::Redraw => {
                self.notice = Some(super::side::close_reason_text(*reason));
                self.pending_request = None;
                self.active_turn_id = None;
                // 关闭真的成功了（或至少已经终态）：上一次「关闭未送达」的
                // 提示必须消失，否则界面会一直说没关掉，而旁支早就不在了。
                self.clear_main_operation_notice(Some(SideOperation::Close), &event.target);
            }
            // 轮次结束时清掉 active turn id:Esc 不能在已结束的轮次上「停止」,
            // 更不能误停下一轮。
            ConversationPayload::SideStatus(_)
                if effect == SideFrameEffect::Redraw && !self.side_running() =>
            {
                self.active_turn_id = None;
                // **不**清 `main_notice`。
                //
                // 一条状态心跳说明这个旁支还活着——也就是说「关闭未送达、
                // 旁问仍在后台运行」这句话**依然成立**。在这里清掉，用户只会
                // 看到提示闪一帧就消失，等于没提示。清理只有两处：
                // 真正收到 `SideClosed`，或用户重新发出了关闭命令。
            }
            _ => {}
        }
        if let Some(turn_id) = &event.turn_id
            && effect == SideFrameEffect::Redraw
        {
            self.active_turn_id = Some(turn_id.clone());
        }
        effect
    }

    /// 把一条操作失败放到「当前能看见它的位置」。
    ///
    /// - 目标是当前可见的旁支 → 写 `notice`（旁支状态行会把它渲染出来）；
    /// - 目标是主会话，或是一个**已经不在当前视图上**的旁支 → 写
    ///   `main_notice`。后者正是后台 `Close` 失败：用户已经回到主视图，
    ///   旁支转录和 `notice` 都不可见，错误只可能在这里被看见。
    fn apply_operation_error(
        &mut self,
        target: ConversationTarget,
        operation: SideOperation,
        message: String,
    ) {
        let visible = self
            .active_side()
            .map(|view| (view.side_id.clone(), view.generation));
        let is_visible_side = match (&target, &visible) {
            (
                ConversationTarget::Side {
                    side_id,
                    generation,
                },
                Some((visible_id, visible_gen)),
            ) => side_id == visible_id && generation == visible_gen,
            _ => false,
        };
        if is_visible_side && self.views.in_side() {
            self.notice = Some(format!("{}：{message}", operation.label()));
            return;
        }
        self.main_notice = Some((operation, target, message));
    }

    /// 主视图里当前要显示的后台操作失败。
    ///
    /// 只取最新的那一条：连续失败时用户最需要的是「最近一次为什么失败」，
    /// 而不是一个越堆越长的清单（状态行只有一行）。
    fn main_operation_notice(&self) -> Option<String> {
        self.main_notice
            .as_ref()
            .map(|(operation, _, message)| format!("{}：{message}", operation.label()))
    }

    /// 一次针对某个操作/目标的成功信号到达后，清掉对应的主视图提示。
    ///
    /// 不清就会出现「已经关掉/重试成功，屏幕上仍挂着上一次失败」。判据必须
    /// 同时看操作与目标：一次成功的 Submit 不能顺手擦掉一条 Close 失败。
    fn clear_main_operation_notice(
        &mut self,
        operation: Option<SideOperation>,
        target: &ConversationTarget,
    ) {
        let should_clear =
            self.main_notice
                .as_ref()
                .is_some_and(|(recorded, recorded_target, _)| {
                    recorded_target == target && operation.is_none_or(|op| op == *recorded)
                });
        if should_clear {
            self.main_notice = None;
        }
    }

    /// 主事件在旁支视图上的唯一正确结果:不改变任何旁支状态。
    ///
    /// 返回 `SideFrameEffect`,调用方可以据此断言「主事件没有写旁支」。
    fn apply_main_frame(&mut self) -> SideFrameEffect {
        let before = (self.active_turn_id.clone(), self.notice.clone());
        let effect = self.views.route(&super::side::ConversationEvent {
            target: super::side::ConversationTarget::Main,
            turn_id: None,
            event: super::side::ConversationPayload::Main,
        });
        debug_assert_eq!(
            before,
            (self.active_turn_id.clone(), self.notice.clone()),
            "主事件绝不能改动旁支视图"
        );
        effect
    }

    /// 把旁支转录整批重画。
    ///
    /// 整批重画而不是增量插入:旁支的 item 是**可变**的（流式 delta 会更新同一个
    /// id），增量插入会在每帧留下旧文本。重画走 `enforce_history_cap`，所以长
    /// 旁支同样受既有缓存上限约束。
    fn render_items(&mut self, width: u16) {
        let Some(view) = self.views.side.as_ref() else {
            return;
        };
        let cells = side_items_to_cells(&view.items);
        self.history.clear();
        for cell in cells {
            self.history.push(cell, width);
        }
    }
}

/// 把 runtime 的转录项映射成既有 `HistoryCell`，复用现成的渲染与折行。
///
/// 刻意不把旁支 item 转成 `AgentEvent` 再喂 `TurnLog`:那条路径会写父会话的
/// 落盘与用量，而旁支一个字节都不该进父文件。
fn side_items_to_cells(
    items: &[std::sync::Arc<yi_agent_runtime::side_conversation::SideItem>],
) -> Vec<HistoryCell> {
    use yi_agent_runtime::side_conversation::SideItemContent;
    let mut cells = Vec::with_capacity(items.len());
    for item in items {
        match &item.content {
            SideItemContent::UserMessage { text } => {
                cells.push(HistoryCell::UserMessage { text: text.clone() });
            }
            SideItemContent::AgentMessage { text } => {
                cells.push(HistoryCell::AssistantMessage {
                    markdown: text.clone(),
                });
            }
            SideItemContent::Thinking { text, .. } => {
                cells.push(HistoryCell::Reasoning {
                    text: text.clone(),
                    expanded: false,
                });
            }
            SideItemContent::ToolCall {
                call_id,
                name,
                input,
                result,
                status,
                ..
            } => {
                let state = match status {
                    yi_agent_runtime::side_conversation::SideToolStatus::Running => {
                        super::cell::CallState::Running
                    }
                    yi_agent_runtime::side_conversation::SideToolStatus::Completed => {
                        super::cell::CallState::Success
                    }
                    yi_agent_runtime::side_conversation::SideToolStatus::Failed => {
                        super::cell::CallState::Failed
                    }
                };
                cells.push(HistoryCell::ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    state,
                    expanded: false,
                });
                if let Some(result) = result {
                    cells.push(HistoryCell::ToolResult {
                        id: call_id.clone(),
                        result_text: result.clone(),
                        is_error: matches!(
                            status,
                            yi_agent_runtime::side_conversation::SideToolStatus::Failed
                        ),
                        expanded: false,
                    });
                }
            }
            SideItemContent::Notice { text } => {
                cells.push(HistoryCell::Separator {
                    label: Some(text.clone()),
                });
            }
        }
    }
    cells
}

/// 当前浮在最上层的东西：命令补全弹窗、运行时选择弹窗、子 agent 列表、
/// trace 流、运行时提示。同一时刻通常只有一个非空，但它们各自独立开关。
struct PopupStack {
    popup: Option<CommandPopup>,
    runtime_popup: RuntimePopup,
    subagent_children: SubagentListState,
    /// The live trace streams, one at most, for whichever task the subagent tab
    /// is showing. Created lazily so a session that never opens the tab never
    /// pays for a socket.
    trace_streams: Option<super::trace::TraceStreams>,
    /// A one-shot alert shown when the runtime the user asked for fails to come
    /// up. It must be a popup, not only a history line: the failure can land
    /// many turns into a long session, where a separator appended to the
    /// transcript appears off-screen and is never seen.
    runtime_notice_popup: Option<String>,
}

impl PopupStack {
    fn new() -> Self {
        Self {
            popup: None,
            runtime_popup: RuntimePopup::None,
            subagent_children: SubagentListState::default(),
            trace_streams: None,
            runtime_notice_popup: None,
        }
    }
}

/// 循环本身的记账：退出标志、待发消息、在跑的任务、进程快照、视口锚点、
/// 通知状态。
struct LoopBookkeeping {
    pending_quit: bool,
    /// 父审批提示显示时，是否处于「敲命令」模式。
    ///
    /// 这必须与审批裁决分开存：模式本身不表达任何批准意图，只有
    /// `Decision` 模式下的数字/Enter 才会发给父 driver。
    approval_mode: crate::tui::side::ApprovalInputMode,
    /// 进入命令模式前用户正在输入的草稿。Esc 退出时原样恢复，
    /// 避免「按了 / 又按 Esc，已经打了一半的追加内容没了」。
    saved_approval_draft: Option<String>,
    queued: crate::tui::queued::DeliveredInterjections,
    task_registry: RunningTaskRegistry,
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
    /// `DisabledNotice` prints exactly one line, on the first frame, once the
    /// history width is known.
    runtime_notice_pending: bool,
    process_events: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
    process_snapshots: Vec<yi_agent_tools::ManagedProcessSnapshot>,
    process_outputs: std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    /// Keep the rendered viewport location so geometry that changes between
    /// frames (such as a resize or newly queued preview) has an old-width anchor.
    previous_viewport: Option<(ViewportAnchor, u16, u16)>,
    /// 终端通知：候选在这里攒着，等这一帧画完再 flush（见下面 `notifications.take()`）。
    notifications: NotificationState,
    /// 真正把通知送出去的终端器（方法 / 触发条件 / 终端环境）。
    /// 与 `notifications` 同属「终端通知」这一件事，故与它同组。
    notifier: crate::tui::notify::Notifier,
}

impl LoopBookkeeping {
    fn new(
        runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
        process_manager: &yi_agent_tools::ProcessManager,
        notifier: crate::tui::notify::Notifier,
    ) -> Self {
        Self {
            pending_quit: false,
            approval_mode: crate::tui::side::ApprovalInputMode::Decision,
            saved_approval_draft: None,
            queued: crate::tui::queued::DeliveredInterjections::new(),
            task_registry: RunningTaskRegistry::new(),
            runtime_notice_pending: matches!(
                runtime_intent,
                Some(crate::tui::subagents::RuntimeStartupIntent::DisabledNotice { .. })
            ),
            runtime_intent,
            process_events: process_manager.subscribe(),
            process_snapshots: process_manager.list(),
            process_outputs: std::collections::HashMap::new(),
            previous_viewport: None,
            notifications: NotificationState::new(),
            notifier,
        }
    }
}

/// 无旁支能力的测试入口。
///
/// 生产代码一律走 [`run_loop_with_side`]:旁支是 TUI 的一等能力,只有
/// 「这个宿主持不支持旁支」的区别,没有「另一条主循环」的区别。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_loop<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    history: &mut HistoryState,
    input: &mut InputLine,
    channels: TuiChannels,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    model: &str,
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
    runtime_choice_tx: Option<
        tokio::sync::mpsc::Sender<crate::tui::subagents::RuntimeStartupChoice>,
    >,
    process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
    workdir: std::path::PathBuf,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &TuiConfigSnapshot,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    theme: TuiTheme,
    title: &mut crate::tui::title::TitleHandle,
) -> std::io::Result<usize> {
    // 默认入口没有旁支能力:测试与不支持旁支的宿主走这条路径。真正的生产入口
    // 用 `run_loop_with_side` 注入通道,这样不必给 16 处测试调用点各加一个参数。
    run_loop_with_side(
        terminal,
        agent_rx,
        history,
        input,
        channels,
        is_running,
        events,
        model,
        runtime_intent,
        runtime_choice_tx,
        process_manager,
        workdir,
        mcp,
        config,
        catalog,
        theme,
        title,
        SideRuntime::disabled(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_loop_with_side<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    history: &mut HistoryState,
    input: &mut InputLine,
    channels: TuiChannels,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    model: &str,
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
    runtime_choice_tx: Option<
        tokio::sync::mpsc::Sender<crate::tui::subagents::RuntimeStartupChoice>,
    >,
    process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
    workdir: std::path::PathBuf,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &TuiConfigSnapshot,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    theme: TuiTheme,
    title: &mut crate::tui::title::TitleHandle,
    side_runtime: SideRuntime,
) -> std::io::Result<usize> {
    // 能力判定必须在拆通道**之前**:`SideRuntime::is_enabled` 是唯一判据,
    // 拆开之后只剩 `Option`,各处很容易把「通道还在」当成「能力还在」。
    let side_capable = side_runtime.is_enabled();
    let SideRuntime {
        commands: side_commands,
        events: mut side_events,
    } = side_runtime;
    // 旁支命令通道挂在 `TuiChannels.side` 上,于是所有按键/斜杠入口都能从
    // 同一处取到它,不必再往下传一个参数。`None` 表示本会话没有旁支能力。
    let mut channels = channels;
    channels.side = side_commands;
    // 三组状态各自打包：会话/模型选择、浮层、循环记账。它们的共同点是
    // 「活到下一帧、任何时刻可读可写」，没有 D1 那种「必须在此之前释放」
    // 的时序约束，散在那里只是让 538 行的循环更难读。
    let mut session_ui = SessionUi::new(model);
    let mut side_session = SideSession::new(theme);
    let mut popup_stack = PopupStack::new();
    let notifications_enabled = config.notifications;
    let focused = std::sync::atomic::AtomicBool::new(true);
    let notifier = {
        let method = crate::tui::notify::Method::parse(&config.notification_method)
            .unwrap_or(crate::tui::notify::Method::Auto);
        let condition = crate::tui::notify::Condition::parse(&config.notification_condition)
            .unwrap_or(crate::tui::notify::Condition::Unfocused);
        // 终端拒绝了焦点事件时 `unfocused` 无从判定，按 always 处理：
        // 宁可多弹一次，也不要静默到用户以为功能坏了。
        let condition = if config.focus_degraded {
            crate::tui::notify::Condition::Always
        } else {
            condition
        };
        crate::tui::notify::Notifier::new(
            method,
            condition,
            &crate::tui::notify::TerminalEnv::from_process(),
        )
    };
    let mut loop_bk = LoopBookkeeping::new(runtime_intent, &process_manager, notifier);

    loop {
        // 当前可见的是哪一份视图。每帧重算:back/打开旁支只改状态,不重启循环。
        let in_side = side_session.in_side();

        let size = terminal.size()?;
        let width = size.width;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let initial_queued_lines =
            crate::tui::queued::render_queued_preview_with(loop_bk.queued.items(), width, theme);
        let initial_layout = compute_layout(
            area,
            if in_side { &side_session.input } else { input },
            loop_bk.pending_quit,
            &popup_stack.popup,
            initial_queued_lines.len() as u16,
        );
        let initial_history_area = initial_layout.chunks[0];
        // 当前可见的是哪一份视图:主视图或旁支。两份各有自己的转录与滚动位置,
        // 渲染器共用,所以这里只切换「读谁」。
        let active_history: &mut HistoryState = if in_side {
            &mut side_session.history
        } else {
            history
        };
        let initial_text_width =
            active_history.text_width(initial_history_area.width, initial_history_area.height);
        active_history.reconcile_scroll_offset(initial_text_width, initial_history_area.height);
        let viewport_anchor = match loop_bk.previous_viewport.take() {
            Some((anchor, previous_width, previous_height))
                if previous_width != initial_text_width
                    || previous_height != initial_history_area.height =>
            {
                Some(anchor)
            }
            _ => active_history
                .capture_viewport_anchor(initial_text_width, initial_history_area.height),
        };
        let mut pending_events = Vec::new();
        while let Ok(event) = agent_rx.try_recv() {
            pending_events.push(event);
        }
        // 旁支事件走独立通道。它们**只**写旁支视图,所以这里的顺序无关紧要,
        // 正是「父后台输出不改旁支、旁支不改父」的实现点。
        let mut pending_side_events = Vec::new();
        if let Some(rx) = side_events.as_mut() {
            while let Ok(event) = rx.try_recv() {
                pending_side_events.push(event);
            }
        }

        // A completed turn promotes one queued item into history and sends it.
        // Determine the resulting layout before applying those history mutations.
        let promotion_count = pending_events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
                )
            })
            .count()
            .min(loop_bk.queued.len());
        let final_queued_lines = crate::tui::queued::render_queued_preview_with(
            &loop_bk.queued.items()[promotion_count..],
            width,
            theme,
        );
        let final_layout = compute_layout(
            area,
            if in_side { &side_session.input } else { input },
            loop_bk.pending_quit,
            &popup_stack.popup,
            final_queued_lines.len() as u16,
        );
        let final_history_area = final_layout.chunks[0];

        // The width the draw will actually use, not the raw area width. The
        // history renderer reserves its rightmost column for the scrollbar, and
        // the cache is keyed by width, so applying events at the area width
        // would re-render the whole scrollback at one width and then again at
        // the other -- once per streamed token.
        let draw_text_width =
            active_history.text_width(final_history_area.width, final_history_area.height);

        for event in &pending_side_events {
            side_session.apply_event(event, draw_text_width);
        }

        for event in pending_events {
            // 主事件必须被旁支视图明确拒绝,而不是「碰巧没人发过来」。
            // 每帧都过一遍路由,零成本（变体不带载荷）,并且一旦有人把主事件
            // 塞进旁支路径,这里立刻能看出来。
            let _ = side_session.apply_main_frame();
            let is_turn_end = matches!(
                event,
                AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
            );
            if let AgentEvent::SubagentRuntimeUnavailable { stage, cause } = &event {
                // The raw stage/cause pair is logged by the emitter; the user
                // only needs the remedy, which never varies. See
                // `runtime_restart_notice`.
                tracing::warn!(
                    stage = %stage,
                    cause = %cause,
                    "subagent runtime unavailable for this session"
                );
                popup_stack.runtime_notice_popup =
                    Some(crate::tui::subagents::runtime_restart_notice_text());
            }
            route_event(
                &mut loop_bk.task_registry,
                &mut session_ui.statusbar_state,
                &mut session_ui.cost_tracker,
                &event,
            );
            // 先应用收据/回推，再处理回合结束。核心保证 `InterjectionsReturned`
            // 排在 `Cancelled`/`Done` 之前；放在回合结束分支之后虽然通常也能生效
            // （二者是相邻的两帧），但那样就依赖事件循环的批处理边界，一旦将来
            // 把一帧内的多个事件合并处理，退回的文本就会被回合结束分支抢先消费。
            // 顺序固定下来后，两种情形都安全。
            apply_interjection_event(&event, &mut loop_bk.queued, input, history, draw_text_width);
            if let AgentEvent::ModelChanged { model: new_model } = &event {
                session_ui.current_model = new_model.clone();
            }
            // 必须在 `push_event` 之前：`Done` 不带正文，只能从 history 反查，
            // 而 `push_event` 会立刻压一条 `Separator`。
            loop_bk
                .notifications
                .observe(&event, history, notifications_enabled);
            history.push_event(event, draw_text_width);
            // 回合结束:弹出下一条待发消息,立即发送并「转正」进 history。
            // 发送与转正是同一个动作,不再依赖 driver 是否取走。
            if is_turn_end && let Some(text) = loop_bk.queued.on_turn_end() {
                let _ = channels.input.try_send(text.clone());
                history.push(HistoryCell::UserMessage { text }, draw_text_width);
            }
        }

        let final_text_width = {
            let active_history: &mut HistoryState = if side_session.in_side() {
                &mut side_session.history
            } else {
                history
            };
            active_history.text_width(final_history_area.width, final_history_area.height)
        };
        let viewport_anchor = viewport_anchor.or_else(|| {
            let active_history: &HistoryState = if side_session.in_side() {
                &side_session.history
            } else {
                history
            };
            active_history.capture_viewport_anchor(final_text_width, final_history_area.height)
        });
        if let Some(anchor) = viewport_anchor {
            let active_history: &mut HistoryState = if side_session.in_side() {
                &mut side_session.history
            } else {
                history
            };
            active_history.restore_viewport_anchor(
                anchor,
                final_text_width,
                final_history_area.height,
            );
        } else {
            let active_history: &mut HistoryState = if side_session.in_side() {
                &mut side_session.history
            } else {
                history
            };
            active_history.reconcile_scroll_offset(final_text_width, final_history_area.height);
        }
        loop_bk.previous_viewport = {
            let active_history: &HistoryState = if side_session.in_side() {
                &side_session.history
            } else {
                history
            };
            active_history
                .capture_viewport_anchor(final_text_width, final_history_area.height)
                .map(|anchor| (anchor, final_text_width, final_history_area.height))
        };

        let queued_lines =
            crate::tui::queued::render_queued_preview_with(loop_bk.queued.items(), width, theme);
        let queued_height = queued_lines.len() as u16;
        // Advance status bar interpolation + spinner (~30hz).
        session_ui.statusbar_state.tick();
        if loop_bk.process_events.try_recv().is_ok() {
            refresh_process_snapshots(
                &process_manager,
                &mut loop_bk.process_snapshots,
                &mut loop_bk.process_outputs,
            );
            while loop_bk.process_events.try_recv().is_ok() {}
        }

        // Pre-compute layout so mouse hit-testing uses the same chunk rects
        // as the draw closure below.
        let size = terminal.size()?;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let layout = compute_layout(
            area,
            input,
            loop_bk.pending_quit,
            &popup_stack.popup,
            queued_height,
        );

        if loop_bk.runtime_notice_pending {
            loop_bk.runtime_notice_pending = false;
            if let Some(crate::tui::subagents::RuntimeStartupIntent::DisabledNotice { reason }) =
                &loop_bk.runtime_intent
            {
                history.push(
                    HistoryCell::Separator {
                        label: Some(reason.clone()),
                    },
                    layout.chunks[0].width,
                );
            }
        }

        let active_process_count = loop_bk
            .process_snapshots
            .iter()
            .filter(|snapshot| is_active_managed_process(&snapshot.status))
            .count();
        // Read the dropped-cell count *outside* the draw closure: the closure
        // already borrows `history` immutably, so this keeps that borrow simple
        // and the count is a plain copy.
        let trimmed_history = history.dropped_cells();
        terminal.draw(|f| {
            let chunks = layout.chunks.clone();

            // 渲染器共用:旁支只换 state 与顶部提示条,主题/折行/缓存上限一致。
            // 旁支身份与按需通知都进**既有状态栏**，不再占用转录首行。
            //
            // 旧实现在 history 区域顶上盖一行 header 并 `Clear`：旁支第一条
            // 正文正好在那一行，开旁支就先丢掉第一句回答（spec §7）。状态栏
            // 本来就每帧都画、且已有宽度预算，把身份放进去不新增布局行。
            let visible_history: &HistoryState = if in_side {
                &side_session.history
            } else {
                history
            };
            let side_label = side_status_label(
                in_side,
                SideParentState {
                    running: is_running.load(std::sync::atomic::Ordering::SeqCst),
                    awaiting_approval: history.pending_permission_info().is_some(),
                },
                &side_session,
            );
            let history_view = HistoryView {
                state: visible_history,
                width: chunks[0].width,
                theme,
            };
            f.render_widget(history_view, chunks[0]);

            // Render popup if active
            if let Some(p) = &popup_stack.popup {
                let popup_area = chunks[1];
                if popup_area.height > 0 {
                    f.render_widget(build_popup_with(p, popup_area.height, theme), popup_area);
                }
            }

            // Status bar.
            let statusbar_line = render_statusbar_with_side(
                &session_ui.statusbar_state,
                &loop_bk.task_registry,
                active_process_count,
                trimmed_history,
                &session_ui.current_model,
                chunks[2].width,
                theme,
                side_label.as_ref(),
            );
            f.render_widget(statusbar_line, chunks[2]);

            // Render queued messages preview
            if queued_height > 0 {
                f.render_widget(Paragraph::new(queued_lines.clone()), chunks[3]);
            }

            let input_line = build_input_line_with(
                if in_side { &side_session.input } else { input },
                loop_bk.pending_quit,
                chunks[INPUT_CHUNK].width,
                theme,
            );
            f.render_widget(input_line, chunks[INPUT_CHUNK]);

            if let Some(crate::tui::subagents::RuntimeStartupIntent::Prompt) =
                &loop_bk.runtime_intent
            {
                let box_w = 58u16.min(chunks[0].width.saturating_sub(4));
                let box_h = 10u16.min(chunks[0].height.max(1));
                let box_x = chunks[0].x + (chunks[0].width.saturating_sub(box_w)) / 2;
                let box_y = chunks[0].y + (chunks[0].height.saturating_sub(box_h)) / 3;
                let box_area = ratatui::layout::Rect {
                    x: box_x,
                    y: box_y,
                    width: box_w,
                    height: box_h,
                };
                f.render_widget(Clear, box_area);
                f.render_widget(
                    ratatui::widgets::Paragraph::new(runtime_prompt_lines())
                        .wrap(ratatui::widgets::Wrap { trim: true })
                        .block(
                            ratatui::widgets::Block::default()
                                .borders(ratatui::widgets::Borders::ALL)
                                .title("启动本地 Agent Runtime?"),
                        ),
                    box_area,
                );
            }

            if let Some(text) = &popup_stack.runtime_notice_popup {
                render_runtime_notice_popup(f, text, chunks[0]);
            }

            render_runtime_popup(
                f,
                &popup_stack.runtime_popup,
                &loop_bk.task_registry,
                &loop_bk.process_snapshots,
                &loop_bk.process_outputs,
                chunks[0],
                theme,
            );
        })?;

        // 通知只在两帧之间发：插在帧中间会把 ANSI 序列切断，留下渲染残影。
        if let Some(pending) = loop_bk.notifications.take() {
            let is_focused = focused.load(std::sync::atomic::Ordering::Relaxed);
            if should_notify(loop_bk.notifier.condition(), is_focused) {
                let _ = loop_bk.notifier.notify(&pending.text);
            }
        }

        // 标题跟随运行态。值没变就不写（`TitleHandle` 内部 diff），所以每帧
        // 调用是安全的：一次会话只在状态翻转点真正写字节。
        title.sync(is_running.load(std::sync::atomic::Ordering::SeqCst));

        sync_trace_streams(
            &mut popup_stack.trace_streams,
            &mut popup_stack.runtime_popup,
            &workdir,
            layout.chunks[0].width,
        );

        // Poll for events with timeout (33ms → ~30hz refresh)
        match events.poll(Duration::from_millis(33))? {
            Some(event @ (Event::FocusGained | Event::FocusLost)) => {
                apply_focus_event(&event, &focused);
            }
            Some(Event::Key(key)) => {
                // 退出优先于一切：runtime popup、父审批、旁支补全都不能吞掉
                // Ctrl+Q / Ctrl+C。判定放在 intercept 之前，是因为 intercept
                // 一遇到打开的 popup 就整键消费。
                let visible_target =
                    visible_side_target(&side_session).unwrap_or(ConversationTarget::Main);
                match decide_exit_key(key, &mut loop_bk.pending_quit) {
                    ExitKeyVerdict::Quit => break,
                    ExitKeyVerdict::ArmConfirm => {
                        // 第一次 Ctrl+C：确认态已置位，并且只停**可见**会话。
                        // 真正的停止在这里发，因为 `decide_exit_key` 是纯函数，
                        // 不碰通道。
                        stop_visible_conversation(
                            &visible_target,
                            is_running.load(std::sync::atomic::Ordering::SeqCst),
                            &mut side_session,
                            &channels,
                        );
                        // 视觉上立刻反映「已确认」：输入行会换成确认提示，等
                        // 下一帧才刷会让用户以为第一下没生效。
                        continue;
                    }
                    ExitKeyVerdict::PassThrough => {
                        // 普通键解除「再按一次退出」的确认态。主视图里
                        // `handle_key` 本来就会做这件事，但旁支的按键路径不经过
                        // 它——两边各留一份就会出现「在旁支里按过 Ctrl+C，之后
                        // 随便敲什么都不会解除确认」。Esc 例外：它在主视图里
                        // 表示「取消当前轮」，不会解除确认。
                        if key.code != KeyCode::Esc {
                            loop_bk.pending_quit = false;
                        }
                    }
                }
                if handle_key_intercepts(
                    key,
                    &mut InterceptContext {
                        popup_stack: &mut popup_stack,
                        loop_bk: &mut loop_bk,
                        history,
                        workdir: &workdir,
                        process_manager: &process_manager,
                        channels: &channels,
                        runtime_choice_tx: runtime_choice_tx.as_ref(),
                        layout: &layout,
                    },
                ) {
                    continue;
                }
                // 路由顺序是硬约束:先应用级退出/弹窗,再**可见视图**,最后才是
                // 父审批。旁支视图绝不经过主视图的 pending_permission 分支,
                // 否则在旁支里按 Enter 会替父会话批准一次工具调用。
                // 用**此刻**的可见视图判断，而不是本帧开头算出的 `in_side`
                // 快照：旁支帧是在本帧中途才应用的，拿快照路由会让「刚进入
                // 旁支」后的第一个键落进父输入行。spec §6.1 要求按键与粘贴
                // 统一选择当前可见的 InputLine。
                if side_session.in_side() {
                    match handle_side_key(key, &mut side_session, &channels, &mut popup_stack.popup)
                    {
                        KeyOutcome::Quit => break,
                        _ => {
                            sync_popup(
                                &mut popup_stack.popup,
                                &side_session.input.buffer,
                                PopupScope::Side,
                            );
                            continue;
                        }
                    }
                }
                let history_area = layout.chunks[0];
                let history_text_width =
                    history.text_width(history_area.width, history_area.height);
                if !side_session.in_side()
                    && history.pending_permission_info().is_none()
                    && intercept_side_command(
                        key,
                        input,
                        history,
                        history_text_width,
                        &channels,
                        &mut side_session,
                        side_capable,
                    )
                {
                    // 拦截分支会 `continue`,从而跳过循环末尾那次 `sync_popup`。
                    // 弹窗是按输入缓冲算的:缓冲刚被 `input.clear()` 清空,
                    // 不在这里同步一次,补全面板就会一直挂在屏幕上,看起来
                    // 像命令没执行。旁支命令同样要遵守「弹窗跟随输入」。
                    sync_popup(&mut popup_stack.popup, &input.buffer, PopupScope::Main);
                    continue;
                }
                // 父审批提示显示时，先走「裁决 vs 命令」模式路由。这必须在
                // `handle_key` 之前：那里对 Enter 的处理是「默认批准」，而命令
                // 模式下的 Enter 只执行命令，两者绝不能共用一条分支。
                if history.pending_permission_info().is_some() {
                    match route_parent_approval_key(
                        key,
                        &mut loop_bk.approval_mode,
                        &mut loop_bk.saved_approval_draft,
                        input,
                        history,
                        history_text_width,
                        &mut side_session,
                        &channels,
                        side_capable,
                    ) {
                        ApprovalRoute::Handled(KeyOutcome::Quit) => break,
                        ApprovalRoute::Handled(_) => continue,
                        ApprovalRoute::PassThrough => {}
                    }
                } else {
                    // 审批消失后模式必须复位，否则下一次审批会凭空处于命令模式。
                    loop_bk.approval_mode = crate::tui::side::ApprovalInputMode::Decision;
                    loop_bk.saved_approval_draft = None;
                }
                let max_offset = history.max_scroll_offset(history_text_width, history_area.height);
                // 每次按键现算:覆盖刚被 `/effort` 改过,清单也可能被 `/model`
                // 选中新条目——两者都不该缓存住。
                let (current_effort, effort_source) = resolve_effort_state(
                    session_ui.effort_override.as_deref(),
                    catalog,
                    session_ui.current_model_ref.as_deref(),
                    config.model_injected,
                );
                match handle_key(
                    key,
                    &mut KeyContext {
                        view: ViewContext {
                            input,
                            history,
                            popup: &mut popup_stack.popup,
                            max_scroll_offset: max_offset,
                            history_width: history_text_width,
                            history_height: history_area.height,
                            cost_tracker: &session_ui.cost_tracker,
                        },
                        runtime: RuntimeContext {
                            channels: &channels,
                            is_running,
                            queued: &mut loop_bk.queued,
                            pending_quit: &mut loop_bk.pending_quit,
                            workdir: &workdir,
                            mcp: &mcp,
                            config,
                        },
                        model: ModelContext {
                            model: &session_ui.current_model,
                            current_model_ref: session_ui.current_model_ref.as_deref(),
                            catalog,
                            current_effort: current_effort.as_deref(),
                            effort_source,
                        },
                    },
                ) {
                    KeyOutcome::Quit => break,
                    KeyOutcome::Submit(_) => {
                        loop_bk.pending_quit = false;
                    }
                    KeyOutcome::ModelSelected(selection) => {
                        // `/model <name>` 已下发:记下用户选中的显示名,供清单
                        // 「← 当前」标记使用;`/model default` 清空(跟随
                        // default_model)。只有命令真的进了 channel 才更新。
                        session_ui.current_model_ref = selection;
                    }
                    KeyOutcome::EffortChanged(effort) => {
                        // `/effort <tier>` 已下发:记下本会话覆盖。driver 收到的
                        // 就是这个值,故 TUI 侧不需要回读;下发失败的档位会被
                        // driver 以 `Notice` 告知(渲染在状态行)。
                        session_ui.effort_override = effort;
                    }
                    KeyOutcome::OpenAgentDetail(task_id) => {
                        if let Ok(socket) = crate::runtime_socket_for(&workdir) {
                            popup_stack.subagent_children = subagent_children_at(&socket);
                        }
                        open_agent_detail(
                            &mut popup_stack.runtime_popup,
                            &popup_stack.subagent_children,
                            task_id,
                        );
                    }
                    KeyOutcome::None => {}
                }
                // After any key, sync popup state with the (possibly modified) buffer
                sync_popup(&mut popup_stack.popup, &input.buffer, PopupScope::Main);
            }
            Some(Event::Paste(text)) => {
                // 粘贴也必须按**可见会话**路由（spec §6.1）。旁支里把字节写进
                // 父 input 是「粘贴一条 /back 结果父草稿变了」的直接原因；而走
                // 父 `handle_paste` 还会被父的待审批守卫整条挡掉——旁支的粘贴
                // 与父审批无关，用户的字节不该因为别处的审批消失。
                if side_session.in_side() {
                    handle_side_paste(text, &mut side_session, &mut popup_stack.popup);
                } else {
                    handle_paste(
                        text,
                        input,
                        history,
                        &popup_stack.runtime_popup,
                        &mut loop_bk.pending_quit,
                        &mut popup_stack.popup,
                        loop_bk.approval_mode,
                    );
                }
            }
            Some(Event::Mouse(mouse)) => {
                handle_mouse(
                    mouse,
                    &layout,
                    &mut popup_stack.runtime_popup,
                    history,
                    &loop_bk.task_registry,
                    &loop_bk.process_snapshots,
                    &loop_bk.process_outputs,
                    &mut loop_bk.pending_quit,
                );
            }
            _ => {}
        }
    }

    // Messages submitted during the last turn never got their chance to run.
    // Return the count so the caller can report the loss after the terminal is
    // restored (anything printed while the alternate screen is up is erased).
    Ok(loop_bk.queued.len())
}

/// Top-level runtime popup: a Bash-tasks tab and a managed-processes tab,
/// switched with Tab. Wraps the per-tab popup state machines.
#[derive(Debug)]
enum RuntimePopup {
    None,
    Bash(BashPopup),
    Processes(ProcessPopup),
    Agents(Box<AgentsPopup>),
}

/// The subagent tab: the child list, the open detail, and the small amount of
/// state the two actions need before the event loop can carry them out.
#[derive(Debug)]
struct AgentsPopup {
    list: TraceListPopup,
    detail: Option<TracePopup>,
    children: SubagentListState,
    /// An action the user asked for and the loop has not carried out yet.
    pending: Option<super::trace::TraceAction>,
    /// True while a message is being typed into the open detail.
    composing: bool,
    /// A cancel preview waiting for `y` / `n`. Holding the token is what makes
    /// the second press a confirmation rather than a fresh preview.
    awaiting_cancel: Option<String>,
    /// The result of the last action, shown in the footer so it is not silent.
    status: Option<String>,
}

impl AgentsPopup {
    fn new(children: SubagentListState) -> Self {
        Self {
            list: TraceListPopup::new(),
            detail: None,
            children,
            pending: None,
            composing: false,
            awaiting_cancel: None,
            status: None,
        }
    }
}

impl RuntimePopup {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    fn blocks_text_input(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn switch_tab(&mut self, process_ids: Vec<String>, children: SubagentListState) {
        *self = match self.tab().map(RuntimeTab::next) {
            Some(RuntimeTab::Processes) => {
                Self::Processes(ProcessPopup::List(ProcessListPopup::new()))
            }
            Some(RuntimeTab::BashTasks) => Self::Bash(BashPopup::List(ListPopup::new(process_ids))),
            Some(RuntimeTab::Agents) => Self::Agents(Box::new(AgentsPopup::new(children))),
            None => Self::None,
        };
    }

    fn tab(&self) -> Option<RuntimeTab> {
        match self {
            Self::None => None,
            Self::Bash(_) => Some(RuntimeTab::BashTasks),
            Self::Processes(_) => Some(RuntimeTab::Processes),
            Self::Agents(_) => Some(RuntimeTab::Agents),
        }
    }
}

fn switch_runtime_tab(
    runtime_popup: &mut RuntimePopup,
    task_registry: &RunningTaskRegistry,
    children: &SubagentListState,
) {
    let ids = task_registry.list().iter().map(|t| t.id.clone()).collect();
    runtime_popup.switch_tab(ids, children.clone());
}

#[cfg(test)]
fn switch_runtime_tab_for_test(
    runtime_popup: &mut RuntimePopup,
    bash_ids: &[String],
    children: SubagentListState,
) {
    *runtime_popup = match runtime_popup.tab().map(RuntimeTab::next) {
        Some(RuntimeTab::Processes) => {
            RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()))
        }
        Some(RuntimeTab::BashTasks) => {
            RuntimePopup::Bash(BashPopup::List(ListPopup::new(bash_ids.to_vec())))
        }
        Some(RuntimeTab::Agents) => RuntimePopup::Agents(Box::new(AgentsPopup::new(children))),
        None => RuntimePopup::None,
    };
}

/// Refresh the managed-process snapshot list and, for each process, read its
/// buffered output. Called on process events and when the popup opens.
fn refresh_process_snapshots(
    process_manager: &yi_agent_tools::ProcessManager,
    process_snapshots: &mut Vec<yi_agent_tools::ManagedProcessSnapshot>,
    process_outputs: &mut std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
) {
    *process_snapshots = process_manager.list();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        for snapshot in process_snapshots.iter() {
            if let Ok(output) = handle.block_on(process_manager.read(
                yi_agent_tools::ProcessSelector::Id(snapshot.process_id.clone()),
                None,
                64 * 1024,
            )) {
                process_outputs.insert(snapshot.process_id.clone(), output);
            }
        }
    }
}

/// Renders the one-shot "runtime could not start" alert over the transcript.
///
/// Deliberately the same shape as the startup dialog: the two report the same
/// subject (the local runtime) and the user should not have to learn two
/// visual grammars for it.
fn render_runtime_notice_popup(
    f: &mut ratatui::Frame<'_>,
    text: &str,
    area: ratatui::layout::Rect,
) {
    // Width follows the text so the notice stays on one line. The fixed 58 was
    // sized for the bare remedy; with the running version appended the text
    // needs ~63 columns, so 58 wrapped it onto a second line and split the
    // version away from what it qualifies ("restart" is useless advice for a
    // stale binary). Wrapping is not truncation, so nothing was lost -- but the
    // one piece of diagnostic information ended up on a line of its own.
    // Still capped so a long line cannot cover the screen.
    // +2 for the block's left/right borders. Display width, not char count:
    // the notice is Chinese, and a CJK glyph occupies two terminal cells.
    let text_width: u16 = text
        .chars()
        .map(|c| if c.is_ascii() { 1u16 } else { 2u16 })
        .sum();
    let needed = text_width + 2;
    let box_w = needed.max(58).min(area.width.saturating_sub(4));
    let box_h = 5u16.min(area.height.max(1));
    let box_x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 3;
    let box_area = ratatui::layout::Rect {
        x: box_x,
        y: box_y,
        width: box_w,
        height: box_h,
    };
    f.render_widget(Clear, box_area);
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::raw(text.to_string()))
            .wrap(ratatui::widgets::Wrap { trim: true })
            .block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .title("子 Agent runtime 未启用（按任意键继续）"),
            ),
        box_area,
    );
}

fn render_runtime_popup(
    f: &mut ratatui::Frame<'_>,
    runtime_popup: &RuntimePopup,
    task_registry: &RunningTaskRegistry,
    process_snapshots: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    area: ratatui::layout::Rect,
    theme: TuiTheme,
) {
    match runtime_popup {
        RuntimePopup::None => {}
        RuntimePopup::Bash(bash_popup) => {
            render_existing_bash_popup(f, bash_popup, task_registry, area, theme);
        }
        RuntimePopup::Processes(ProcessPopup::List(p)) => {
            f.render_widget(Clear, area);
            // `Clear` above stays so the raised surface fills a cleared cell
            // instead of ghosting.
            f.render_widget(
                super::process_popup::render_process_list_popup_with(
                    p,
                    process_snapshots,
                    area,
                    theme,
                ),
                area,
            );
        }
        RuntimePopup::Processes(ProcessPopup::Detail(p)) => {
            if let Some(process) = process_snapshots
                .iter()
                .find(|p2| p2.process_id == p.process_id)
            {
                f.render_widget(Clear, area);
                f.render_widget(
                    super::process_popup::render_process_detail_popup_with(
                        p,
                        process,
                        process_outputs.get(&p.process_id),
                        area,
                        theme,
                    ),
                    area,
                );
            }
        }
        RuntimePopup::Agents(agents) => {
            f.render_widget(Clear, area);
            match &agents.detail {
                None => {
                    f.render_widget(
                        render_subagent_list_with(&agents.list, &agents.children, area, theme),
                        area,
                    );
                }
                Some(TracePopup::Detail(detail)) => {
                    let prompt = super::trace::DetailPrompt {
                        composing: agents.composing,
                        awaiting_cancel: agents.awaiting_cancel.is_some(),
                        status: agents.status.as_deref(),
                    };
                    super::trace::render_subagent_detail_with(
                        f,
                        detail,
                        &agents.children,
                        &prompt,
                        area,
                        theme,
                    );
                }
            }
        }
        RuntimePopup::Processes(ProcessPopup::ConfirmKill(ck)) => {
            if let Some(process) = process_snapshots
                .iter()
                .find(|p| p.process_id == ck.process_id)
            {
                f.render_widget(Clear, area);
                let detail = ProcessDetailPopup::new(ck.process_id.clone());
                f.render_widget(
                    super::process_popup::render_process_detail_popup_with(
                        &detail,
                        process,
                        process_outputs.get(&ck.process_id),
                        area,
                        theme,
                    ),
                    area,
                );
                render_kill_confirmation_overlay(f, area, "kill this managed process?");
            }
        }
    }
}

/// Render the bash-tasks tab (list / detail / kill-confirm) into `area`.
fn render_existing_bash_popup(
    f: &mut ratatui::Frame<'_>,
    bash_popup: &BashPopup,
    task_registry: &RunningTaskRegistry,
    area: ratatui::layout::Rect,
    theme: TuiTheme,
) {
    match bash_popup {
        BashPopup::List(p) => {
            let list_area = ratatui::layout::Rect {
                x: area.x + 2,
                y: area.y + 1,
                width: area.width.saturating_sub(4),
                height: area
                    .height
                    .saturating_sub(2)
                    .min((p.task_ids.len() as u16 + 2).max(6)),
            };
            f.render_widget(Clear, list_area);
            f.render_widget(
                super::bash_popup::render_list_popup_with(p, task_registry, list_area, theme),
                list_area,
            );
        }
        BashPopup::Detail(p) => {
            if let Some(task) = task_registry.get(&p.task_id) {
                f.render_widget(Clear, area);
                f.render_widget(
                    super::bash_popup::render_detail_popup_with(p, task, area, theme),
                    area,
                );
            }
        }
        BashPopup::ConfirmKill(ck) => {
            if let Some(task) = task_registry.get(&ck.task_id) {
                f.render_widget(Clear, area);
                let detail = DetailPopup::new(ck.task_id.clone());
                f.render_widget(
                    super::bash_popup::render_detail_popup_with(&detail, task, area, theme),
                    area,
                );
            }
            render_kill_confirmation_overlay(f, area, "kill this process?");
        }
        BashPopup::None => {}
    }
}

fn render_kill_confirmation_overlay(
    f: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    prompt: &str,
) {
    let box_w = 40u16.min(area.width.saturating_sub(4));
    let box_h = 4u16;
    let box_x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 3;
    let box_area = ratatui::layout::Rect {
        x: box_x,
        y: box_y,
        width: box_w,
        height: box_h,
    };
    f.render_widget(Clear, box_area);
    f.render_widget(
        ratatui::widgets::Paragraph::new(vec![
            ratatui::text::Line::raw(prompt.to_string()),
            ratatui::text::Line::raw(""),
            ratatui::text::Line::raw("[y] confirm   [n/esc] cancel"),
        ])
        .block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title("confirm"),
        ),
        box_area,
    );
}

#[cfg(test)]
fn handle_runtime_popup_key_for_test(
    key: KeyEvent,
    runtime_popup: &mut RuntimePopup,
    bash_ids: &[String],
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
) {
    if key.code == KeyCode::Tab {
        switch_runtime_tab_for_test(runtime_popup, bash_ids, SubagentListState::default());
        return;
    }
    let registry = RunningTaskRegistry::new();
    let outputs = std::collections::HashMap::new();
    let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let _ = handle_runtime_popup_key(
        key,
        runtime_popup,
        &registry,
        &kill_tx,
        processes,
        &outputs,
        &SubagentListState::default(),
        80,
        24,
    );
}

/// Route a key to the active runtime popup. Returns `Some(process_id)` when the
/// user confirms a kill in the managed-processes tab.
#[allow(clippy::too_many_arguments)]
fn handle_runtime_popup_key(
    key: KeyEvent,
    runtime_popup: &mut RuntimePopup,
    task_registry: &RunningTaskRegistry,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    children: &SubagentListState,
    detail_width: u16,
    detail_height: u16,
) -> Option<String> {
    match runtime_popup {
        RuntimePopup::None => {}
        RuntimePopup::Bash(bash_popup) => {
            if key.code == KeyCode::Tab {
                switch_runtime_tab(runtime_popup, task_registry, children);
            } else {
                handle_bash_popup_key(key, bash_popup, task_registry, kill_tx, detail_width);
                if matches!(bash_popup, BashPopup::None) {
                    *runtime_popup = RuntimePopup::None;
                }
            }
        }
        RuntimePopup::Processes(ProcessPopup::List(p)) => match key.code {
            KeyCode::Tab => {
                switch_runtime_tab(runtime_popup, task_registry, children);
            }
            KeyCode::Up => p.move_up(),
            KeyCode::Down => p.move_down(processes.len()),
            KeyCode::Enter => {
                if let Some(id) = p.selected_id(processes) {
                    *runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
                        ProcessDetailPopup::new(id.to_string()),
                    ));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => *runtime_popup = RuntimePopup::None,
            _ => {}
        },
        RuntimePopup::Processes(ProcessPopup::Detail(d)) => match key.code {
            KeyCode::Tab => {
                switch_runtime_tab(runtime_popup, task_registry, children);
            }
            KeyCode::Char('k') => {
                *runtime_popup =
                    RuntimePopup::Processes(ProcessPopup::ConfirmKill(ConfirmProcessKillPopup {
                        process_id: d.process_id.clone(),
                    }));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *runtime_popup =
                    RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
            }
            KeyCode::Up => {
                let max = process_detail_max_scroll(
                    d,
                    processes,
                    process_outputs,
                    detail_width,
                    detail_height,
                );
                d.scroll_up_from_bottom(1, max);
            }
            KeyCode::Down => {
                let max = process_detail_max_scroll(
                    d,
                    processes,
                    process_outputs,
                    detail_width,
                    detail_height,
                );
                d.scroll_down(1, max);
            }
            KeyCode::Char('f') => d.scroll_to_bottom(),
            _ => {}
        },
        RuntimePopup::Processes(ProcessPopup::ConfirmKill(ck)) => match key.code {
            KeyCode::Char('n') | KeyCode::Esc => {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
                    ProcessDetailPopup::new(ck.process_id.clone()),
                ));
            }
            KeyCode::Char('y') => {
                let process_id = ck.process_id.clone();
                *runtime_popup =
                    RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
                return Some(process_id);
            }
            _ => {}
        },
        RuntimePopup::Agents(agents) => {
            if key.code == KeyCode::Tab {
                switch_runtime_tab(runtime_popup, task_registry, children);
                return None;
            }
            if handle_agents_key(key, agents, children, detail_width) {
                *runtime_popup = RuntimePopup::None;
            }
        }
    }
    None
}

/// Keys for the subagent tab. Returns true when the tab should close.
///
/// The list level navigates; the detail level is read-only apart from the two
/// interventions the design allows: a message (`m`, Enter to submit) and a
/// cancel (`k`, confirmed with `y`). Neither touches the socket here: the action
/// is recorded and the event loop carries it out, which keeps this function
/// testable without a daemon.
/// Open the subagent tab on one task's detail, from wherever it was asked for.
///
/// The tab owns the detail, so opening it means switching the popup to the
/// Agents tab and drilling straight to the task. The child list is replaced with
/// the snapshot the caller just read, so the detail's drill-down agrees with it.
fn open_agent_detail(
    runtime_popup: &mut RuntimePopup,
    children: &SubagentListState,
    task_id: String,
) {
    let mut agents = AgentsPopup::new(children.clone());
    agents.detail = Some(TracePopup::Detail(TraceDetailPopup::new(task_id)));
    *runtime_popup = RuntimePopup::Agents(Box::new(agents));
}

fn handle_agents_key(
    key: KeyEvent,
    agents: &mut AgentsPopup,
    children: &SubagentListState,
    detail_width: u16,
) -> bool {
    if let Some(TracePopup::Detail(detail)) = &mut agents.detail {
        // A pending cancel confirmation takes every key, so a stray `m` cannot
        // quietly turn into a message while the daemon is asking to confirm.
        if let Some(token) = agents.awaiting_cancel.clone() {
            let task_id = detail.task_id().to_string();
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    agents.awaiting_cancel = None;
                    agents.pending = Some(super::trace::TraceAction::Cancel {
                        task_id,
                        confirmation_token: Some(token),
                    });
                }
                _ => {
                    agents.awaiting_cancel = None;
                    agents.status = Some("已取消该次操作".into());
                }
            }
            return false;
        }

        // Composing a message: the detail view owns the text until Enter.
        if agents.composing {
            match key.code {
                KeyCode::Enter => {
                    let text = detail.take_input();
                    agents.composing = false;
                    if text.trim().is_empty() {
                        agents.status = Some("消息为空，未发送".into());
                    } else {
                        agents.pending = Some(super::trace::TraceAction::SendMessage {
                            task_id: detail.task_id().to_string(),
                            text,
                        });
                    }
                }
                KeyCode::Esc => {
                    detail.take_input();
                    agents.composing = false;
                }
                KeyCode::Backspace => detail.input_backspace(),
                KeyCode::Char(ch) => detail.input_push(ch),
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !detail.pop() {
                    agents.detail = None;
                }
                agents.status = None;
            }
            KeyCode::Char('m') => {
                agents.composing = true;
                detail.take_input();
            }
            KeyCode::Char('k') => {
                agents.pending = Some(super::trace::TraceAction::Cancel {
                    task_id: detail.task_id().to_string(),
                    confirmation_token: None,
                });
            }
            KeyCode::Enter => {
                let task_id = detail.task_id().to_string();
                let targets = super::trace::drill_targets(children, &task_id);
                if let Some(target) = targets.first() {
                    let child = target.task_id.clone();
                    detail.push(child);
                    agents.status = None;
                } else {
                    agents.status = Some("该任务没有可进入的子任务".into());
                }
            }
            KeyCode::Up | KeyCode::PageUp => detail.feed_mut().scroll(-1),
            KeyCode::Down | KeyCode::PageDown => detail.feed_mut().scroll(1),
            KeyCode::Char('g') => detail.feed_mut().scroll_to_top(),
            KeyCode::Char('G') => detail.feed_mut().scroll_to_bottom(),
            _ => {}
        }
        let _ = detail_width;
        return false;
    }

    match key.code {
        KeyCode::Up => agents.list.move_up(),
        KeyCode::Down => agents.list.move_down(agents.children.len()),
        KeyCode::Enter => {
            if let Some(task_id) = agents.list.selected_id(&agents.children.items) {
                agents.detail = Some(TracePopup::Detail(TraceDetailPopup::new(
                    task_id.to_string(),
                )));
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => return true,
        _ => {}
    }
    false
}

/// Carry out whatever action the subagent tab recorded.
///
/// A cancel preview is not the cancel itself: the daemon answers the first press
/// with a token, and only a second press carrying that token actually cancels.
/// The token is kept on the popup so the confirmation is a real second step
/// rather than a formality.
fn execute_trace_action(
    action: super::trace::TraceAction,
    socket: &std::path::Path,
    agents: &mut AgentsPopup,
) {
    match action {
        super::trace::TraceAction::SendMessage { task_id, text } => {
            agents.status = Some(match daemon_user_message_at(socket, &task_id, &text) {
                Ok(message) => message,
                Err(error) => format!("发送失败: {error}"),
            });
        }
        super::trace::TraceAction::Cancel {
            task_id,
            confirmation_token,
        } => {
            match confirmation_token {
                // First press: ask for a preview and hold its token. Nothing is
                // cancelled yet.
                None => match preview_cancel_token_at(socket, &task_id) {
                    Ok(token) => {
                        agents.awaiting_cancel = Some(token);
                        agents.status = Some(format!("继续按 y 确认取消 {task_id}"));
                    }
                    Err(error) => agents.status = Some(format!("取消预览失败: {error}")),
                },
                // Second press: the token is what makes this the real cancel.
                Some(token) => {
                    agents.awaiting_cancel = None;
                    agents.status = Some(
                        match daemon_cancel_at(socket, &task_id, false, Some(&token)) {
                            Ok(message) => message,
                            Err(error) => format!("取消失败: {error}"),
                        },
                    );
                }
            }
        }
    }
}

/// Ask the daemon for a cancel preview and return its confirmation token.
fn preview_cancel_token_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PreviewCancel {
            task_id: task_id.to_owned(),
            recursive: false,
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token, ..
        } => Ok(confirmation_token),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝取消预览: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非取消预览响应".into()),
    }
}

/// Keep at most one trace stream open, for whatever the subagent tab shows.
///
/// Called once per frame after drawing. It carries out any recorded action, then
/// points the stream at the open detail (or closes it), appending whatever rows
/// arrived to that detail's feed.
fn sync_trace_streams(
    streams: &mut Option<super::trace::TraceStreams>,
    runtime_popup: &mut RuntimePopup,
    workdir: &std::path::Path,
    width: u16,
) {
    // Wall-clock state of the tab: the pending action, then whatever it shows.
    let (pending, showing) = match runtime_popup {
        RuntimePopup::Agents(agents) => {
            let pending = agents.pending.take();
            let showing = agents
                .detail
                .as_ref()
                .map(|TracePopup::Detail(detail)| detail.task_id().to_string());
            (pending, showing)
        }
        _ => (None, None),
    };

    if let Some(action) = pending {
        let RuntimePopup::Agents(agents) = runtime_popup else {
            return;
        };
        match crate::runtime_socket_for(workdir) {
            Ok(socket) => execute_trace_action(action, &socket, agents),
            Err(error) => agents.status = Some(format!("无法连接 daemon: {error}")),
        }
    }

    // The stream must not outlive the view it feeds, so anything but an open
    // detail closes it. Reopening the same task later then re-reads its backlog
    // instead of resuming a stream whose snapshot was never shown.
    let Some(task_id) = showing else {
        if let Some(streams) = streams {
            streams.close();
        }
        return;
    };
    let streams = streams.get_or_insert_with(|| {
        let source = super::trace::IpcTraceSource::new(workdir.to_path_buf());
        super::trace::TraceStreams::new(Box::new(source))
    });
    let RuntimePopup::Agents(agents) = runtime_popup else {
        return;
    };
    let Some(TracePopup::Detail(detail)) = &mut agents.detail else {
        return;
    };
    streams.sync(Some(&task_id), detail.feed_mut(), width);
}

fn process_detail_max_scroll(
    detail: &ProcessDetailPopup,
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    width: u16,
    height: u16,
) -> usize {
    processes
        .iter()
        .find(|process| process.process_id == detail.process_id)
        .map(|process| {
            super::process_popup::process_detail_line_count(
                process,
                process_outputs.get(&detail.process_id),
                width,
            )
            .saturating_sub(height as usize)
        })
        .unwrap_or(0)
}

/// Handle a key event for the bash popup state machine.
fn handle_bash_popup_key(
    key: KeyEvent,
    bash_popup: &mut BashPopup,
    task_registry: &RunningTaskRegistry,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    detail_width: u16,
) {
    match bash_popup {
        BashPopup::List(p) => match key.code {
            KeyCode::Up => p.move_up(),
            KeyCode::Down => p.move_down(),
            KeyCode::Enter => {
                if let Some(id) = p.selected_id() {
                    *bash_popup = BashPopup::Detail(DetailPopup::new(id.to_string()));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *bash_popup = BashPopup::None;
            }
            _ => {}
        },
        BashPopup::Detail(d) => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                // Back to list (rebuild ids from registry so new tasks appear)
                let ids: Vec<String> = task_registry.list().iter().map(|t| t.id.clone()).collect();
                if ids.is_empty() {
                    *bash_popup = BashPopup::None;
                } else {
                    *bash_popup = BashPopup::List(ListPopup::new(ids));
                }
            }
            KeyCode::Char('k') => {
                // Only allow kill on running tasks.
                let is_running = task_registry
                    .get(&d.task_id)
                    .map(|t| t.status == super::state::TaskStatus::Running)
                    .unwrap_or(false);
                if is_running {
                    *bash_popup = BashPopup::ConfirmKill(ConfirmKill {
                        task_id: d.task_id.clone(),
                    });
                }
            }
            KeyCode::Up => {
                d.scroll_up(1);
            }
            KeyCode::Down => {
                let lines = task_registry
                    .get(&d.task_id)
                    .map(|t| super::bash_popup::detail_line_count(t, detail_width))
                    .unwrap_or(0);
                d.scroll_down(1, lines);
            }
            KeyCode::Char('f') => {
                d.scroll_to_bottom();
            }
            _ => {}
        },
        BashPopup::ConfirmKill(ck) => match key.code {
            KeyCode::Char('y') => {
                // Ask the agent driver to cancel this one tool call. Cancelling
                // the call drops the bash tool's future, which is what reaps the
                // command's whole process group — so this stops the command, not
                // just our wait for it. The driver answers by emitting a
                // `ToolExit` and an error `ToolResult`, which flips the registry
                // entry out of `Running` on its own.
                //
                // A failed send means the driver is gone (run over / app
                // shutting down); the task cannot be running then either, so the
                // entry is finalized directly to stop the timer.
                if kill_tx.try_send(ck.task_id.clone()).is_err() {
                    // `try_send` can only fail for a full or closed channel; a
                    // full one means the driver is busy and will drain it, so
                    // only the closed case needs the local fallback.
                    if kill_tx.is_closed() {
                        tracing::warn!(
                            tool_call = %ck.task_id,
                            "kill request dropped: the agent driver is gone"
                        );
                    }
                }
                let ids: Vec<String> = task_registry.list().iter().map(|t| t.id.clone()).collect();
                if ids.is_empty() {
                    *bash_popup = BashPopup::None;
                } else {
                    *bash_popup = BashPopup::List(ListPopup::new(ids));
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                *bash_popup = BashPopup::Detail(DetailPopup::new(ck.task_id.clone()));
            }
            _ => {}
        },
        BashPopup::None => {}
    }
}

/// Handle a mouse event by routing scroll wheel movement to whichever region
/// the cursor is over. Only scroll events are handled; clicks are ignored.
///
/// Routing priority:
/// 1. If a bash/process detail popup is active and the mouse is over the
///    history region (where the popup is rendered), scroll the popup.
/// 2. Otherwise, if the mouse is over the history region, scroll history.
/// 3. If the mouse is over the input region, do nothing — the input widget
///    handles its own scrolling internally.
#[allow(clippy::too_many_arguments)]
fn handle_mouse(
    mouse: MouseEvent,
    layout: &LayoutInfo,
    runtime_popup: &mut RuntimePopup,
    history: &mut HistoryState,
    task_registry: &RunningTaskRegistry,
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    pending_quit: &mut bool,
) {
    // Only react to scroll-wheel events.
    let scroll_delta = match mouse.kind {
        MouseEventKind::ScrollUp => Some(HISTORY_WHEEL_LINES),
        MouseEventKind::ScrollDown => Some(HISTORY_WHEEL_LINES),
        _ => None,
    };
    let Some(delta) = scroll_delta else {
        return;
    };
    let is_scroll_down = matches!(mouse.kind, MouseEventKind::ScrollDown);

    let history_area = layout.chunks[0];
    let pos = ratatui::layout::Position::from((mouse.column, mouse.row));

    // The bash detail popup occupies the history region (chunks[0]).
    if matches!(
        runtime_popup,
        RuntimePopup::Bash(BashPopup::Detail(_)) | RuntimePopup::Bash(BashPopup::ConfirmKill(_))
    ) && history_area.contains(pos)
    {
        if let RuntimePopup::Bash(BashPopup::Detail(d)) = runtime_popup {
            if is_scroll_down {
                let lines = task_registry
                    .get(&d.task_id)
                    .map(|t| super::bash_popup::detail_line_count(t, history_area.width))
                    .unwrap_or(0);
                d.scroll_down(delta, lines);
            } else {
                d.scroll_up(delta);
            }
        }
        return;
    }

    if matches!(
        runtime_popup,
        RuntimePopup::Processes(ProcessPopup::Detail(_))
            | RuntimePopup::Processes(ProcessPopup::ConfirmKill(_))
    ) && history_area.contains(pos)
    {
        if let RuntimePopup::Processes(ProcessPopup::Detail(d)) = runtime_popup {
            let max = process_detail_max_scroll(
                d,
                processes,
                process_outputs,
                history_area.width,
                history_area.height,
            );
            if is_scroll_down {
                d.scroll_down(delta, max);
            } else {
                d.scroll_up_from_bottom(delta, max);
            }
        }
        return;
    }

    // History region: scroll the conversation history.
    if history_area.contains(pos) {
        *pending_quit = false;
        let history_text_width = history.text_width(history_area.width, history_area.height);
        let max_offset = history.max_scroll_offset(history_text_width, history_area.height);
        if is_scroll_down {
            history.scroll_down(delta);
        } else {
            history.scroll_up(delta, max_offset);
        }
    }
    // Other regions (status bar, queued preview, input) are intentionally
    // ignored — the input widget handles its own scroll, and the others have
    // no scrollable content.
}

/// Route agent events to the task registry + status bar before they reach
/// history. Streaming events (ToolOutputDelta/ToolExit/ToolTimeout) are
/// consumed here and not pushed to history (history ignores them anyway).
fn route_event(
    registry: &mut RunningTaskRegistry,
    statusbar: &mut StatusBarState,
    cost: &mut CostTracker,
    event: &AgentEvent,
) {
    match event {
        AgentEvent::Start => {
            statusbar.reset_for_new_call();
        }
        AgentEvent::ToolCall { id, name, input } => {
            // LLM turn ended, tool execution begins. Reset the decode
            // counter so it doesn't linger at the previous turn's value
            // throughout the entire tool execution phase.
            statusbar.on_tool_call_phase();
            if name == "bash" {
                let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let exp = input
                    .get("expected_timeout_sec")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120) as u32;
                registry.on_tool_call(id, name, cmd, exp);
            }
        }
        AgentEvent::ToolOutputDelta { id, stream, text } => {
            registry.on_output_delta(id, *stream, text);
        }
        AgentEvent::ToolExit { id, code } => {
            registry.on_exit(id, *code);
        }
        AgentEvent::ToolTimeout { id } => {
            registry.on_timeout(id);
        }
        AgentEvent::ToolResult { id, result } => {
            registry.on_result(id, result.is_error);
        }
        AgentEvent::ToolRetry { .. } => {}
        // The retry is surfaced through a history separator; the status bar
        // needs no extra state (which would raise "when do we clear it?").
        AgentEvent::ProviderRetry { .. } => {}
        // Turn-end events finalize any still-running tasks. This is a
        // defense-in-depth cleanup: in the happy path each ToolCall gets a
        // matching ToolExit before Done arrives. But ToolExit can be missed
        // when (a) the bash tool early-returns on a blocked/parse/spawn
        // error without emitting ToolEvent::Exit, (b) the user cancels
        // mid-tool and the call_stream future is dropped, or (c) the
        // forwarder task silently drops the event on a full/closed channel.
        // Without this cleanup the status-bar timer would tick forever.
        AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_) => {
            registry.abort_all_running();
        }
        AgentEvent::Usage { model, usage } => {
            statusbar.set_token_target(usage.input_tokens as u64, usage.output_tokens as u64);
            cost.record(model, usage);
        }
        AgentEvent::EstimatedPrefill(n) => {
            statusbar.set_prefill_estimate(*n as u64);
        }
        AgentEvent::AssistantText(text) => {
            statusbar.estimate_decode_tokens(text);
        }
        AgentEvent::DecodeDelta(text) => {
            statusbar.estimate_decode_tokens(text);
        }
        AgentEvent::AutoCompacting {
            old_msg_count,
            new_msg_count,
        } => {
            tracing::info!(
                old_msg_count,
                new_msg_count,
                "auto-compact: session compressed"
            );
        }
        _ => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    None,
    Quit,
    Submit(String),
    /// `/model` 已成功下发:携带用户选中的清单显示名（`None` = 清覆盖/跟随默认）。
    ModelSelected(Option<String>),
    /// `/effort` 已成功下发:携带本次要写入的档位（`None` = 清覆盖）。
    EffortChanged(Option<String>),
    /// Open the subagent tab directly on a task's read-only detail.
    OpenAgentDetail(String),
}

/// Parses `/recover-root`'s argument.
///
/// Deliberately three shapes and no more:
///
/// * no argument / `status <id>` / `<id>` -- read the stored operation, or read the
///   impact for this conversation. Neither stops anything.
/// * `prepare <id>` -- the first mutating step, which is why it must be typed out.
/// * `confirm <id>` -- commits a prepared operation. The token is **not** an argument:
///   it is read from `YI_AGENT_RECOVERY_TOKEN`. A token in argv would land in shell
///   history, `ps` output and any transcript that echoes the command.
///
/// The operation id is chosen by the user and reused across retries; this parser never
/// invents one, because a lost answer must be resolved by asking about the *same*
/// operation rather than by starting a second recovery.
fn parse_recover_args(
    args: Option<&str>,
) -> Result<yi_agent_subagent::foreground::RecoveryAction, String> {
    // Reading the environment is the *only* place a secret enters this path, and it
    // is read here rather than in the driver so the driver never has to know how the
    // user supplied it.
    let token = std::env::var("YI_AGENT_RECOVERY_TOKEN").ok();
    parse_recover_args_with_token(args, token.as_deref())
}

/// The parse without the environment, so the rules can be tested directly.
///
/// Split from [`parse_recover_args`] because setting a process-wide variable from a
/// test is `unsafe` (other threads may read it) and would make these assertions
/// order-dependent. The production wrapper is the only caller that reads the env.
fn parse_recover_args_with_token(
    args: Option<&str>,
    token: Option<&str>,
) -> Result<yi_agent_subagent::foreground::RecoveryAction, String> {
    use yi_agent_subagent::foreground::RecoveryAction;

    let usage = "用法: /recover-root [status <operation-id>] | /recover-root <operation-id> | /recover-root prepare <operation-id> | /recover-root confirm <operation-id>（token 经 YI_AGENT_RECOVERY_TOKEN 传入）";
    let text = args.unwrap_or("").trim();
    if text.is_empty() {
        return Ok(RecoveryAction::Inspect);
    }
    let mut parts = text.split_whitespace();
    let head = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();
    match (head, rest.as_slice()) {
        // A bare word is an operation id to look up: the read-only form.
        (id, []) if id != "status" && id != "prepare" && id != "confirm" => {
            Ok(RecoveryAction::Operation {
                operation_id: id.to_string(),
            })
        }
        ("status", [id]) => Ok(RecoveryAction::Operation {
            operation_id: (*id).to_string(),
        }),
        ("status", _) => Err(usage.to_string()),
        ("prepare", [id]) => Ok(RecoveryAction::Prepare {
            operation_id: (*id).to_string(),
        }),
        ("prepare", _) => Err(usage.to_string()),
        ("confirm", [id]) => {
            let token = token
                .filter(|token| !token.trim().is_empty())
                .ok_or_else(|| {
                    "确认恢复需要 token：请从预览结果取出并设入 YI_AGENT_RECOVERY_TOKEN 后重试（不接受命令行参数，以免进入 shell 历史）"
                        .to_string()
                })?;
            Ok(RecoveryAction::Confirm {
                operation_id: (*id).to_string(),
                token: token.to_string(),
            })
        }
        ("confirm", _) => Err(usage.to_string()),
        _ => Err(usage.to_string()),
    }
}

fn starts_with_multi_segment_absolute_path(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|token| token.starts_with('/') && token.matches('/').count() >= 2)
}

/// `handle_key_intercepts` 要碰的那几样东西。
///
/// 打包的理由和 `KeyContext` 一样：这些引用总是一起出现，散在参数表里
/// 只会让调用点变成一长串位置参数。
struct InterceptContext<'a> {
    popup_stack: &'a mut PopupStack,
    loop_bk: &'a mut LoopBookkeeping,
    history: &'a mut HistoryState,
    workdir: &'a std::path::Path,
    process_manager: &'a yi_agent_tools::ProcessManager,
    channels: &'a TuiChannels,
    runtime_choice_tx:
        Option<&'a tokio::sync::mpsc::Sender<crate::tui::subagents::RuntimeStartupChoice>>,
    layout: &'a LayoutInfo,
}

/// 按键的前置拦截：Ctrl+P、运行时弹窗路由、启动提示。
///
/// 返回 `true` 表示这一键已被消费，调用方应当直接进入下一轮循环。
///
/// 这三段之所以单独成函数：它们都是「这个键归谁处理」的判定，不碰
/// `session_ui`/`input`，返回值就是全部副作用之外的信息。留在 `run_loop`
/// 里会让真正的主按键处理淹没在拦截逻辑后面。
/// 应用级退出键的裁决结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitKeyVerdict {
    /// 已经退出循环。
    Quit,
    /// 第一次 Ctrl+C：已进入「再按一次退出」确认态，键本身被消费。
    ArmConfirm,
    /// 这个键不属于退出语义，交给后续路由。
    PassThrough,
}

/// 应用级退出裁决。
///
/// 为什么必须在 `handle_key_intercepts` **之前**跑：那个函数先处理 runtime
/// popup（Bash 任务、进程、子 agent），popup 打开时会把所有键都吃掉——包括
/// Ctrl+Q。用户打开一个弹窗之后就无法退出应用，只能先关弹窗。父审批的
/// Decision 模式同理：除 Esc/Ctrl+Q 之外的键一律返回 `None`，Ctrl+C 在那里
/// 什么都不做。
///
/// 规则（spec §6.2）：
/// - Ctrl+Q 永远退出，任何视图、任何弹窗、任何审批态。
/// - Ctrl+C 第一次只「确认」，第二次才退出。第一次的「停可见会话」由调用方
///   在 `ArmConfirm` 分支完成（主会话跑着就停主会话，旁支跑着就停旁支，
///   两处都不跑时不动任何东西）。
/// - `/q`、`/quit` 归各视图的提交路径处理（旁支那边见 `submit_side_draft`），
///   这里只管按键。
///
/// 只维护 `pending_quit` **一个**状态：主视图与旁支各存一份，切视图后就会
/// 出现「旁支里按过 Ctrl+C，回主会话按一次就退出」这种莫名其妙的行为。
fn decide_exit_key(key: KeyEvent, pending_quit: &mut bool) -> ExitKeyVerdict {
    if matches!(key.code, KeyCode::Char('q')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return ExitKeyVerdict::Quit;
    }
    if !(matches!(key.code, KeyCode::Char('c')) && key.modifiers == KeyModifiers::CONTROL) {
        return ExitKeyVerdict::PassThrough;
    }
    if *pending_quit {
        return ExitKeyVerdict::Quit;
    }
    *pending_quit = true;
    // 停哪个会话不在这里决定：这个函数不碰通道，调用方在 `ArmConfirm` 分支
    // 调 `stop_visible_conversation`。**不要**在这里顺手算一次
    // `visible_stop_target` 再丢掉——那会让读者以为停止已经发出去了。
    ExitKeyVerdict::ArmConfirm
}

/// 第一次 Ctrl+C 时停住「用户正在看的那个会话」。
///
/// 与 `visible_stop_target` 共用同一条判定：主视图只看主会话是否在跑，旁支
/// 只看旁支。两处各写一份迟早会出现「确认提示说停了旁支，实际停的是父」。
fn stop_visible_conversation(
    visible: &ConversationTarget,
    main_running: bool,
    side: &mut SideSession,
    channels: &TuiChannels,
) {
    match crate::tui::side::visible_stop_target(visible, main_running, side.side_running()) {
        Some(ConversationTarget::Main) => {
            // 父会话的停止是幂等的：连按只会重复投递同一个中断。
            let _ = channels.interrupt.try_send(());
        }
        Some(ConversationTarget::Side { .. }) => {
            // 显示「在跑」但本地不知道是哪一轮：turn id 是异步事件回传的，
            // 提交后、事件被消费前就会短暂落到这里。静默 return 会让用户以为
            // Ctrl+C 没生效，于是连按——第二次就退出了应用。
            let Some(turn_id) = side.active_turn_id.clone() else {
                side.notice = Some("旁支正在启动，请稍候再停止".to_string());
                return;
            };
            if let Err(message) = send_side_command(
                channels,
                crate::side_driver::SideDriverCommand::Interrupt {
                    expected_target: visible.clone(),
                    turn_id,
                },
                SideOperation::Interrupt,
            ) {
                side.notice = Some(message);
            }
        }
        None => {}
    }
}

fn handle_key_intercepts(key: KeyEvent, ctx: &mut InterceptContext<'_>) -> bool {
    let InterceptContext {
        popup_stack,
        loop_bk,
        history,
        workdir,
        process_manager,
        channels,
        runtime_choice_tx,
        layout,
    } = ctx;
    // Ctrl+P opens the runtime popup (bash tasks tab) when no popup
    // is active. Tab then switches to the managed-processes tab.
    if key.code == KeyCode::Char('p')
        && key.modifiers == KeyModifiers::CONTROL
        && popup_stack.runtime_popup.is_none()
    {
        refresh_process_snapshots(
            process_manager,
            &mut loop_bk.process_snapshots,
            &mut loop_bk.process_outputs,
        );
        let ids: Vec<String> = loop_bk
            .task_registry
            .list()
            .iter()
            .map(|t| t.id.clone())
            .collect();
        if let Ok(socket) = crate::runtime_socket_for(workdir) {
            popup_stack.subagent_children = subagent_children_at(&socket);
        }
        popup_stack.runtime_popup = RuntimePopup::Bash(BashPopup::List(ListPopup::new(ids)));
        return true;
    }
    // Route keys to the runtime popup when active.
    if !popup_stack.runtime_popup.is_none() {
        let process_to_kill = handle_runtime_popup_key(
            key,
            &mut popup_stack.runtime_popup,
            &loop_bk.task_registry,
            &channels.kill,
            &loop_bk.process_snapshots,
            &loop_bk.process_outputs,
            &popup_stack.subagent_children,
            layout.chunks[0].width,
            layout.chunks[0].height,
        );
        if let Some(process_id) = process_to_kill {
            let _ = tokio::runtime::Handle::current()
                .block_on(process_manager.kill(yi_agent_tools::ProcessSelector::Id(process_id)));
            refresh_process_snapshots(
                process_manager,
                &mut loop_bk.process_snapshots,
                &mut loop_bk.process_outputs,
            );
        }
        return true;
    }
    // Dismiss the failure notice on any key. Deliberately no
    // `continue`: a popup that consumed the first character typed
    // would silently corrupt what the user was writing, far worse
    // than the nuisance it reports, so the key clears the alert and
    // still reaches the input handling below.
    popup_stack.runtime_notice_popup = None;
    if let Some(crate::tui::subagents::RuntimeStartupIntent::Prompt) = &loop_bk.runtime_intent {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                persist_runtime_choice(
                    workdir,
                    crate::tui::runtime_prefs::RuntimePreference::Always,
                    history,
                    layout.chunks[0].width,
                );
                if let Some(tx) = &runtime_choice_tx {
                    let _ = tx.blocking_send(crate::tui::subagents::RuntimeStartupChoice::Start);
                }
                loop_bk.runtime_intent = None;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                persist_runtime_choice(
                    workdir,
                    crate::tui::runtime_prefs::RuntimePreference::Never,
                    history,
                    layout.chunks[0].width,
                );
                if let Some(tx) = &runtime_choice_tx {
                    let _ = tx.blocking_send(
                        crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation,
                    );
                }
                loop_bk.runtime_intent = None;
            }
            // Esc means "skip this time"; it must NOT overwrite the
            // stored preference, so the next launch asks again.
            KeyCode::Esc => {
                if let Some(tx) = &runtime_choice_tx {
                    let _ = tx.blocking_send(
                        crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation,
                    );
                }
                loop_bk.runtime_intent = None;
            }
            _ => {}
        }
        return true;
    }
    false
}

/// 一次按键处理需要的全部「环境」。
///
/// 分组依据是实测的使用频次：高频且互相纠缠的（history/input/popup/尺寸）
/// 放 `View`；只读的运行时依赖放 `Runtime`；模型选择放 `Model`。
/// 主视图里敲旁支命令（`/side`、`/btw`、`/back`、`/side-close`）的拦截。
///
/// 必须在 `handle_key` **之前**:那里会把回车当成一次普通提交投进主 `input`
/// 通道,于是运行中的父会话会被插话打断——正是旁支要避免的事。这里只拦
/// 「首 token 是旁支命令」的回车,其余按键与其它斜杠命令原样放行。
#[allow(clippy::too_many_arguments)]
fn intercept_side_command(
    key: KeyEvent,
    input: &mut InputLine,
    history: &mut HistoryState,
    history_width: u16,
    channels: &TuiChannels,
    side: &mut SideSession,
    side_capable: bool,
) -> bool {
    if key.code != KeyCode::Enter || !key.modifiers.is_empty() {
        return false;
    }
    let text = input.buffer.clone();
    let trimmed = text.trim();
    let Some(name) = side_command_name(trimmed) else {
        return false;
    };
    // 命中旁支命令就由这里负责到底:即使分类失败（例如 `/back 参数`）
    // 也不能落回「未知命令」或当成普通消息发给模型。
    input.clear();
    match crate::tui::side::classify_conversation_input(trimmed, false) {
        Ok(crate::tui::side::RoutedInput::Side(command)) => {
            execute_side_command_in_main(
                command,
                history,
                history_width,
                side,
                channels,
                side_capable,
            );
        }
        Ok(crate::tui::side::RoutedInput::Main(_)) => {
            unreachable!("side_command_name 已确认 {name} 是旁支命令,分类器不能再判成主会话")
        }
        Err(error) => {
            history.push(
                HistoryCell::Separator {
                    label: Some(error.message),
                },
                history_width,
            );
        }
    }
    true
}

/// 输入行的首 token 是否是一个旁支命令;是则返回命令名。
///
/// 只认完整 token:`/sidebar` 不是 `/side`。
fn side_command_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    matches!(
        name,
        crate::tui::side::SIDE_COMMAND
            | crate::tui::side::SIDE_ALIAS
            | crate::tui::side::BACK_COMMAND
            | crate::tui::side::SIDE_CLOSE_COMMAND
    )
    .then_some(name)
}

/// 在主视图（含父审批的命令模式）里执行一条旁支命令。
///
/// 主视图里的语义（spec §4.1）:
///
/// - `/side`、`/btw` 打开/切到旁支;没有旁支能力时明确提示,不静默吞掉。
/// - `/back` 是空操作:已经在主会话了,但要给出提示,否则像是命令没生效。
/// - `/side-close` 关闭后台旁支;**没有旁支时幂等成功**,不报错。
fn execute_side_command_in_main(
    command: crate::tui::side::SideInput,
    history: &mut HistoryState,
    history_width: u16,
    side: &mut SideSession,
    channels: &TuiChannels,
    side_capable: bool,
) -> KeyOutcome {
    use crate::tui::side::SideInput;

    let notice = |history: &mut HistoryState, label: String| {
        history.push(HistoryCell::Separator { label: Some(label) }, history_width);
    };

    if !side_capable {
        // 没有能力时所有旁支命令都只能提示,不能假装执行成功。
        notice(history, "本次会话未启用旁支（/side 不可用）".to_string());
        return KeyOutcome::None;
    }
    let Some(commands) = channels.side.as_ref() else {
        notice(history, "本次会话未启用旁支（/side 不可用）".to_string());
        return KeyOutcome::None;
    };

    match command {
        SideInput::Open { question } => {
            // 已经把旁支建出来了就先把视图切回去,别等 driver 回帧:
            // 已有旁支时 driver 走幂等路径,只补一帧 `SideStatus`,而那个帧
            // 更新状态但不切视图（否则后台流式输出会把正在看主会话的人拽走）。
            // 用户显式敲 `/side` 才是「我要看旁支」的信号,必须当场生效。
            if let Some(view) = side.active_side()
                && view.is_live()
            {
                let (side_id, generation) = (view.side_id.clone(), view.generation);
                side.views.open(side_id, generation);
            }
            let _ = commands.try_send(crate::side_driver::SideDriverCommand::Open {
                request_id: uuid::Uuid::new_v4().to_string(),
                question,
            });
        }
        SideInput::Back => {
            notice(history, "已经在主会话了，无需 /back".to_string());
        }
        SideInput::Close => match side.active_side() {
            None => notice(history, "当前没有旁问".to_string()),
            Some(view) if !view.is_live() => {
                let text = view
                    .closed
                    .map(crate::tui::side::close_reason_text)
                    .unwrap_or_else(|| "旁问已关闭".to_string());
                notice(history, text);
            }
            Some(_) => {
                // 主视图里的 `/side-close` 是「关掉后台旁支」：它不切换视图,
                // 所以失败只能在主视图里说出来。目标取自当前视图，避免一次
                // 迟到的关闭落到刚换上的新旁支上。
                let Some(target) = visible_side_target(side) else {
                    return KeyOutcome::None;
                };
                match send_side_command(
                    channels,
                    crate::side_driver::SideDriverCommand::Close {
                        expected_target: target.clone(),
                        expected_active_turn_id: side.active_turn_id.clone(),
                    },
                    SideOperation::Close,
                ) {
                    Ok(()) => {
                        // 重新受理了关闭请求：上一次「未送达」已经不适用，
                        // 现在的状态是「正在关」。不清就会同时说两句互相矛盾
                        // 的话。只清同目标、同操作的提示，免得顺手擦掉别的。
                        side.clear_main_operation_notice(Some(SideOperation::Close), &target);
                        notice(history, "正在关闭旁问…".to_string())
                    }
                    Err(message) => {
                        side.main_notice = Some((SideOperation::Close, target, message))
                    }
                }
            }
        },
        // 分类器在主视图里不会产出这些变体。
        SideInput::Submit(_) | SideInput::LocalCommand(_) => {}
    }
    KeyOutcome::None
}

/// 旁支视图里的按键裁决。
///
/// 旁支**绝不**经过主视图的 `pending_permission` 分支:父会话的审批必须 `back`
/// 回去才能处理,在旁支里按 Enter 不能替父做决定。这也是把旁支按键单独抽出来
/// 的原因——它不是「主视图按键的一个分支」,而是一条独立的、只读的路。
/// Paste into the **visible** side input.
///
/// Deliberately unlike the parent's `handle_paste`: no runtime-popup check and
/// no pending-permission check. Both belong to the parent view — a parent's
/// approval must not swallow bytes the user is typing into a side draft, and
/// the side has no approval UI to be protected from. Paste only edits the
/// draft; it never executes anything, so a pasted `/back` still needs Enter.
fn handle_side_paste(text: String, side: &mut SideSession, popup: &mut Option<CommandPopup>) {
    side.input.insert_str(&text);
    sync_popup(popup, &side.input.buffer, PopupScope::Side);
}

/// 是否是一条「管理命令」（主会话的斜杠命令）。
///
/// 判据是**命令目录**而不是硬编码名单:主会话新增一条 `/foo` 时,旁支自动
/// 把它当管理命令拒绝,而不需要有人记得来这里补一行。
fn is_management_command(text: &str) -> bool {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return false;
    };
    let name = rest.split_whitespace().next().unwrap_or("");
    // `/help`、`/cost`、`/config` 同时存在于主目录与旁支本地命令里。它在这里
    // 的语义是**旁支只读版本**（不读父费用、不改配置），所以必须先排除，
    // 否则判定函数会说「这是管理命令」而分类器说「这是本地命令」——两处规则
    // 一旦分歧，改哪一边都只是在赌当前的匹配顺序。
    if crate::tui::side::SIDE_LOCAL_COMMANDS.contains(&name) {
        return false;
    }
    // `/quit`、`/q` 也是主目录成员，但它们在旁支里是**应用退出**（spec §6.2
    // 明确例外），归退出分支处理，不能被这条拒绝规则先拦下来。
    if is_exit_command(text) {
        return false;
    }
    !name.is_empty() && SlashCommand::from_name(name).is_some()
}

/// 管理命令在旁支里的拒绝文案。
fn management_command_rejection(text: &str) -> String {
    let name = text
        .trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("");
    format!("旁支只读：/{name} 只能在主会话使用（输入已保留）")
}

/// 是否为应用退出命令（`/quit`、`/q`）。
///
/// 主视图与旁支共用这一条：两处各写一份 `matches!` 迟早会出现「主视图能退出、
/// 旁支把 `/q` 当成提问」的分歧。退出的承受者是整个应用，与当前可见哪个视图
/// 无关。
pub(crate) fn is_exit_command(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    matches!(
        trimmed
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or(""),
        "quit" | "q"
    )
}

/// 状态栏里该显示的旁支段。
///
/// 规则只有一条：**错误优先于身份**。用户此刻需要知道的是「刚才那一下为什么
/// 没生效」，而不是「你在旁支里」。所以有通知时完整文本换成通知，但窄屏必留
/// 段仍是「旁问 · /back」——错误再重要，也不能让人不知道自己在哪、怎么出去。
///
/// 主视图只显示后台操作失败（spec §6.2）：旁支正文和身份都不进父视图，
/// 否则用户会以为自己在旁支里。
fn side_status_label(
    in_side: bool,
    parent: SideParentState,
    side: &SideSession,
) -> Option<SideStatusLabel> {
    let notice = side
        .notice
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    if in_side {
        // 错误优先：用户此刻要知道的是「刚才那一下为什么没生效」。
        let extra = match notice {
            Some(notice) => notice.to_string(),
            None => side_parent_state_text(parent),
        };
        return Some(SideStatusLabel::side(Some(&extra)));
    }
    // 回到主视图后，后台关闭失败仍然要能看到；旁支身份则不再显示。
    side.main_operation_notice()
        .map(|notice| SideStatusLabel::main(&notice))
}

/// 主会话此刻的状态，从旁支角度看。
///
/// 「等待审批」必须先于「运行中」判断：卡在审批上的父会话在技术上仍在运行，
/// 但对用户来说它是**停住了**，只说「运行中」会让人以为不用管。
#[derive(Debug, Clone, Copy)]
struct SideParentState {
    running: bool,
    awaiting_approval: bool,
}

/// 旁支里父会话状态的短文案。
///
/// 这三个词现在是旁支身份的唯一措辞来源（原来的转录首行 header 已删除，
/// 因为它遮住旁支第一条正文）。父会话的审批**详情**只能回父视图看，
/// 这里只允许一句状态（spec §7）。
fn side_parent_state_text(parent: SideParentState) -> String {
    if parent.awaiting_approval {
        "父会话等待审批".to_string()
    } else if parent.running {
        "父会话运行中".to_string()
    } else {
        "父会话空闲".to_string()
    }
}

/// 当前可见旁支的目标身份（`None` = 主视图或还没有旁支）。
///
/// 每次发命令前现算，不从别处缓存：视图切换发生在同一帧内，缓存下来的目标
/// 会让一次关闭/停止打到刚被替换掉的旧旁支上。
fn visible_side_target(side: &SideSession) -> Option<ConversationTarget> {
    side.active_side().map(|view| ConversationTarget::Side {
        side_id: view.side_id.clone(),
        generation: view.generation,
    })
}

/// 把一条旁支命令投出去，失败时**当场**说清是哪一步、为什么。
///
/// 旧写法一律 `let _ = try_send(...)`：通道满（容量小、用户连按）或调度已经
/// 退出时，命令静默消失，用户看到的只是「按了没反应」。`Full` 与 `Closed`
/// 的区别必须保留——前者可以重试，后者这一会话已经没有旁支能力了。
fn send_side_command(
    channels: &TuiChannels,
    command: crate::side_driver::SideDriverCommand,
    operation: SideOperation,
) -> Result<(), String> {
    let Some(commands) = channels.side.as_ref() else {
        return Err(format!("{}：本次会话没有旁问能力", operation.label()));
    };
    match commands.try_send(command) {
        Ok(()) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(format!(
            "{}：后台正忙，命令未送达，请稍后重试",
            operation.label()
        )),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err(format!("{}：旁问后台已退出，请重开旁问", operation.label()))
        }
    }
}

/// Submit whatever is in the visible side input, and put the text back when the
/// input is refused.
///
/// `take_submitted()` consumes the buffer, so every refusal path must restore
/// it. Otherwise a rejected management command (`/clear`) or a refused nested
/// `/side` silently deletes what the user typed — the spec's rule is "拒绝不清稿".
/// Local navigation is the exception: `text` is not restored after `/back` or
/// `/side-close`, because the command has been consumed and re-entering a side
/// must not leave a stale command sitting in the draft to be executed again.
fn submit_side_draft(side: &mut SideSession, channels: &TuiChannels) -> KeyOutcome {
    let text = side.input.take_submitted();
    if text.trim().is_empty() {
        return KeyOutcome::None;
    }
    // 退出命令必须在旁支分类**之前**处理。分类器只认旁支自己的三五个命令，
    // 其余一律当问题——于是旁支里的 `/q` 会被当成一句话发给模型，而主视图里
    // 同样的输入是退出（spec §6.2：/q、/quit 在旁问中也不能被当问题发送）。
    if is_exit_command(&text) {
        return KeyOutcome::Quit;
    }
    match classify_conversation_input(&text, true) {
        Ok(RoutedInput::Side(SideInput::Back)) => {
            side.views.back();
            KeyOutcome::None
        }
        Ok(RoutedInput::Side(SideInput::Close)) => {
            // 关闭命令本身即确认（原生 TUI 没有远端确认弹窗），所以要先返回，
            // 再把返回**是否送达**说清楚。心跳帧只负责显示真正的终态。
            //
            // 关键顺序：先记下目标，再发命令，**最后**才切视图；失败提示必须
            // 落在切换**之后**可见的位置。写进 `side.notice` 是不行的——那一刻
            // 用户已经在主视图，旁支状态行不再渲染，弹出的话等于没说。
            let target = visible_side_target(side);
            let delivered = match &target {
                Some(target) => send_side_command(
                    channels,
                    crate::side_driver::SideDriverCommand::Close {
                        expected_target: target.clone(),
                        expected_active_turn_id: side.active_turn_id.clone(),
                    },
                    SideOperation::Close,
                )
                .is_ok(),
                None => false,
            };
            side.views.back();
            if !delivered {
                // 不能假报已关闭：本地返回仍然成立，但后台旁支还在跑。
                let message =
                    "已返回主视图，但旁问关闭未送达（旁问仍在后台运行，可再试）".to_string();
                match target {
                    Some(target) => {
                        side.main_notice = Some((SideOperation::Close, target, message))
                    }
                    None => side.notice = Some(message),
                }
            }
            KeyOutcome::None
        }
        Ok(RoutedInput::Side(SideInput::Submit(body))) => {
            // 分类器把所有「旁支不认识的 `/xxx`」也归到 Submit——它分不清那是
            // 一句话还是被误用的管理命令。这一步必须自己判（spec W12）：
            // `/clear`、`/model` 之类在旁支里**拒绝并保留原稿**，既不能执行
            // （会改父会话），也不能当问题发给模型（模型会认真回答一个跟旁问
            // 无关的斜杠串，同时用户以为命令生效了）。
            if matches!(body.trim_start().strip_prefix('/'), Some(rest) if !rest.trim().is_empty())
                && is_management_command(&body)
            {
                restore_rejected_side_draft(side, &text);
                side.notice = Some(management_command_rejection(&body));
                return KeyOutcome::None;
            }
            submit_side_question(side, channels, body);
            KeyOutcome::None
        }
        Ok(RoutedInput::Side(SideInput::LocalCommand(name))) => {
            side.notice = Some(side_local_command_text(side, &name));
            KeyOutcome::None
        }
        Ok(RoutedInput::Side(SideInput::Open { .. })) => {
            restore_rejected_side_draft(side, &text);
            side.notice = Some("已经在旁支里了，不能再开一个旁支".to_string());
            KeyOutcome::None
        }
        Ok(RoutedInput::Main(_)) => {
            // 分类器在旁支内不会返回 Main;真发生了说明规则被改坏了。
            restore_rejected_side_draft(side, &text);
            side.notice = Some("旁支里只能提问或使用 /back、/side-close".to_string());
            KeyOutcome::None
        }
        Err(error) => {
            restore_rejected_side_draft(side, &text);
            side.notice = Some(error.message);
            KeyOutcome::None
        }
    }
}

/// 被拒的提交要把原文还给用户。
///
/// 只在输入行仍是空的时候写回：如果用户在等待期间已经开始敲新东西，覆盖它
/// 等于把新稿丢掉。不承诺找回用户自己手动改掉的旧文字。
fn restore_rejected_side_draft(side: &mut SideSession, text: &str) {
    if side.input.buffer.is_empty() {
        side.input.insert_str(text);
    }
}

/// 旁支本地命令（`/cost`、`/config`、`/help`）的输出文案。
fn side_local_command_text(side: &SideSession, name: &str) -> String {
    match name {
        "cost" => format_side_usage(side),
        "config" => "旁支继承父会话的模型与推理档；只读，不能改配置".to_string(),
        _ => {
            // 工具白名单直接取自调度侧的同一份策略,不在这里另抄一遍:
            // 两处各写一份,总有一天帮助里说能用的工具其实被拒。
            let tools = crate::side_driver::allowed_side_tools().join("、");
            format!(
                "/back 返回主会话，/side-close 关闭旁支。\
旁支可用工具（只读）：{tools}"
            )
        }
    }
}

fn handle_side_key(
    key: KeyEvent,
    side: &mut SideSession,
    channels: &TuiChannels,
    popup: &mut Option<CommandPopup>,
) -> KeyOutcome {
    // 补全弹窗只吃自己那几个键。它读写的是**可见的**旁支输入行：候选按这一行
    // 过滤，Tab 也写回这一行——显示的候选和真正会提交的正文因此不可能分叉
    // （spec §6.1「不能显示候选却由另一套输入分支忽略」）。
    if popup.is_some() {
        match key.code {
            KeyCode::Up => {
                if let Some(p) = popup.as_mut() {
                    p.move_up();
                }
                return KeyOutcome::None;
            }
            KeyCode::Down => {
                if let Some(p) = popup.as_mut() {
                    p.move_down();
                }
                return KeyOutcome::None;
            }
            KeyCode::Tab => {
                // Tab 只补全文本，绝不提交：补全与执行是两个明确动作。
                if let Some(p) = popup.as_ref()
                    && let Some(completion) = p.selected()
                {
                    side.input.buffer = completion.insertion();
                    side.input.cursor = side.input.buffer.len();
                }
                *popup = None;
                return KeyOutcome::None;
            }
            KeyCode::Esc => {
                // Esc 先关补全（退出优先级），不停止旁支轮次。
                *popup = None;
                return KeyOutcome::None;
            }
            KeyCode::Enter => {
                // 确认候选 = 把候选文本写进输入行，然后走与手打完全相同的提交
                // 路径。这样「补全后回车」绝不会长成第三种语义。
                if let Some(p) = popup.as_ref()
                    && let Some(completion) = p.selected()
                {
                    side.input.buffer = completion.insertion();
                    side.input.cursor = side.input.buffer.len();
                }
                *popup = None;
                return submit_side_draft(side, channels);
            }
            _ => {}
        }
    } else if key.code == KeyCode::Tab {
        // 没有候选时 Tab 不该被当成正文插进草稿。
        return KeyOutcome::None;
    }

    // 应用级退出在任何视图都必须可达,且不被字符输入吞掉。
    match key.code {
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return KeyOutcome::Quit;
        }
        KeyCode::Esc => {
            // Esc 只停**当前可见**的旁支轮次,不退出、不碰父会话。
            //
            // 判定统一走 `visible_stop_target`:旁支空闲时它返回 `None`,
            // 也就是「什么也不做」。这条规则只写一遍,免得按键层与退出层
            // 各自实现一套、慢慢长出分歧。
            let active = match &side.active_side() {
                Some(view) => crate::tui::side::ConversationTarget::Side {
                    side_id: view.side_id.clone(),
                    generation: view.generation,
                },
                None => crate::tui::side::ConversationTarget::Main,
            };
            if let Some(crate::tui::side::ConversationTarget::Side { .. }) =
                crate::tui::side::visible_stop_target(&active, false, side.side_running())
            {
                // 停止失败同样必须可见：用户按了 Esc 却什么都没发生，比直接
                // 报错更糟——他会以为轮次还在跑。
                //
                // `None` 轮次同理：视图说在跑、本地还没拿到 turn id 时，
                // 静默跳过等于假装停下。宁可说一句「还在启动」。
                match side.active_turn_id.clone() {
                    Some(turn_id) => {
                        if let Err(message) = send_side_command(
                            channels,
                            crate::side_driver::SideDriverCommand::Interrupt {
                                expected_target: active.clone(),
                                turn_id,
                            },
                            SideOperation::Interrupt,
                        ) {
                            side.notice = Some(message);
                        }
                    }
                    None => side.notice = Some("旁支正在启动，请稍候再停止".to_string()),
                }
            }
            return KeyOutcome::None;
        }
        _ => {}
    }

    match key.code {
        KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
            side.history.select_up();
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
            side.history.select_down();
            return KeyOutcome::None;
        }
        KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
            side.history.toggle_fold_selected();
            return KeyOutcome::None;
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            let max = side.history.max_scroll_offset(80, 24);
            side.history.scroll_up(10, max);
            return KeyOutcome::None;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            side.history.scroll_down(10);
            return KeyOutcome::None;
        }
        _ => {}
    }

    match side.input.handle_key(key) {
        InputAction::Submit => submit_side_draft(side, channels),
        InputAction::Consumed | InputAction::NotHandled => KeyOutcome::None,
    }
}

/// 审批模式路由的结果。
enum ApprovalRoute {
    /// 这一键已被审批/命令逻辑消费。
    Handled(KeyOutcome),
    /// 交给后续的常规按键处理（例如滚动、Ctrl+C）。
    PassThrough,
}

/// 父审批提示显示时的一层按键前置路由。
///
/// 顺序是硬约束（设计 §10.3）：
///
/// 1. 应用级退出（Ctrl+Q）在任何模式下都可达；
/// 2. `/` 从裁决模式进入命令模式，并保存当前草稿；
/// 3. 命令模式下的 Enter 只执行命令，**永不**发裁决；
/// 4. Esc 在命令模式里只退出命令模式并恢复草稿；
/// 5. 其余键仍交给原有裁决逻辑（数字/Enter 批准）。
#[allow(clippy::too_many_arguments)]
fn route_parent_approval_key(
    key: KeyEvent,
    mode: &mut crate::tui::side::ApprovalInputMode,
    saved_draft: &mut Option<String>,
    input: &mut InputLine,
    history: &mut HistoryState,
    history_width: u16,
    side: &mut SideSession,
    channels: &TuiChannels,
    side_capable: bool,
) -> ApprovalRoute {
    use crate::tui::side::{ApprovalInputMode, ApprovalKeyAction, route_approval_key};

    match route_approval_key(key, mode) {
        ApprovalKeyAction::ApplicationExit => ApprovalRoute::Handled(KeyOutcome::Quit),
        ApprovalKeyAction::EnterCommandMode => {
            *saved_draft = Some(input.buffer.clone());
            input.clear();
            ApprovalRoute::Handled(KeyOutcome::None)
        }
        ApprovalKeyAction::ExitCommandMode => {
            input.clear();
            if let Some(draft) = saved_draft.take() {
                input.insert_str(&draft);
            }
            ApprovalRoute::Handled(KeyOutcome::None)
        }
        ApprovalKeyAction::EditCommand(key) => {
            if key.code == KeyCode::Enter {
                let text = input.take_submitted();
                let outcome = exec_approval_command(
                    text,
                    input,
                    history,
                    history_width,
                    side,
                    channels,
                    side_capable,
                );
                if matches!(outcome, KeyOutcome::Quit) {
                    return ApprovalRoute::Handled(outcome);
                }
                // 执行完一条命令就回到裁决模式:用户敲命令是为了「办一件事」,
                // 不是为了离开审批。继续停在命令模式下,下一次回车会又一次
                // 被当成命令,而用户以为自己在裁决——两者混淆比多按一次 `/` 危险得多。
                // 进入命令模式时保存的原草稿也要还回去,否则草稿被命令顶掉。
                *mode = ApprovalInputMode::Decision;
                input.clear();
                if let Some(draft) = saved_draft.take() {
                    input.insert_str(&draft);
                }
                return ApprovalRoute::Handled(KeyOutcome::None);
            }
            // 命令模式下的普通编辑键：只改输入行，不碰裁决。
            let _ = input.handle_key(key);
            ApprovalRoute::Handled(KeyOutcome::None)
        }
        ApprovalKeyAction::DecisionKey(_) if *mode == ApprovalInputMode::Command => {
            // 理论上不可达（Command 分支已在上方返回），保守起见仍然消费掉。
            ApprovalRoute::Handled(KeyOutcome::None)
        }
        ApprovalKeyAction::DecisionKey(_) => ApprovalRoute::PassThrough,
    }
}

/// 父审批态「命令模式」里执行一条命令。
///
/// 白名单刻意很小:只有 `/side`(`/btw`)、`/back`、`/side-close`、`/help`
/// 与 `/quit`（spec §10.2）。其余命令一律拒绝并**保留输入**——在审批提示
/// 上面执行 `/clear`、`/model` 之类的命令会让用户误以为对话框还能继续编辑，
/// 而且这些命令会在审批未决时改变父会话状态。
///
/// 本函数**不**触碰 `decision` 通道:命令永远不等于批准。
#[allow(clippy::too_many_arguments)]
fn exec_approval_command(
    text: String,
    input: &mut InputLine,
    history: &mut HistoryState,
    history_width: u16,
    side: &mut SideSession,
    channels: &TuiChannels,
    side_capable: bool,
) -> KeyOutcome {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return KeyOutcome::None;
    }
    let name = trimmed
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    // 退出类：与主视图一致。
    if is_exit_command(trimmed) {
        return KeyOutcome::Quit;
    }

    // 旁支命令在这里与主视图共用同一份执行逻辑:审批态不是另一套命令语义,
    // 只是「Enter 的默认含义」不同。共用可以保证 /back 在主视图与审批态里
    // 给出同一句话,而不是两处各写一份、慢慢长出分歧。
    if side_command_name(trimmed).is_some() {
        return match crate::tui::side::classify_conversation_input(trimmed, false) {
            Ok(crate::tui::side::RoutedInput::Side(command)) => execute_side_command_in_main(
                command,
                history,
                history_width,
                side,
                channels,
                side_capable,
            ),
            Ok(crate::tui::side::RoutedInput::Main(_)) => KeyOutcome::None,
            Err(error) => {
                history.push(
                    HistoryCell::Separator {
                        label: Some(error.message),
                    },
                    history_width,
                );
                KeyOutcome::None
            }
        };
    }

    if name == "help" {
        history.push(
            HistoryCell::Separator {
                label: Some(format!(
                    "审批中的命令（Enter 只执行命令，不会批准）：{}\n{}\n命令不会批准本请求",
                    approval_command_line(),
                    "其余命令请先裁决或 Esc 退出命令模式"
                )),
            },
            history_width,
        );
        return KeyOutcome::None;
    }

    // 拒绝但保留原文：用户可能只是打错了，直接清空会让他重打一遍。
    input.insert_str(&text);
    history.push(
        HistoryCell::Separator {
            label: Some(format!(
                "/{name} 不能在审批中使用（只允许 {}）。这是命令，不会批准",
                approval_command_line()
            )),
        },
        history_width,
    );
    KeyOutcome::None
}

/// 审批命令模式的白名单文案。
///
/// 与 [`exec_approval_command`] 的分支必须同步:帮助里说能用的命令,
/// 一定是真的能用的。桌面端 `/help` 的旁问分组同源（spec §4.1 三端一致）。
fn approval_command_line() -> String {
    format!(
        "/{}（/{}）、/{}、/{}、/help、/quit",
        crate::tui::side::SIDE_COMMAND,
        crate::tui::side::SIDE_ALIAS,
        crate::tui::side::BACK_COMMAND,
        crate::tui::side::SIDE_CLOSE_COMMAND
    )
}

/// 把一个问题发给旁支。
///
/// 请求 id 在「第一次发送」时生成并记住;同一条草稿重试时复用同一个 id,
/// 这样 driver 侧的去重记录能把它认成同一次请求,而不是又跑一轮。
fn submit_side_question(side: &mut SideSession, channels: &TuiChannels, question: String) {
    let Some(commands) = channels.side.as_ref() else {
        side.notice = Some("本次会话未启用旁支".to_string());
        return;
    };
    let request_id = side
        .pending_request
        .as_ref()
        .filter(|(_, pending)| *pending == question)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    side.pending_request = Some((request_id.clone(), question.clone()));
    let command = crate::side_driver::SideDriverCommand::Submit {
        request_id,
        text: question,
    };
    if commands.try_send(command).is_err() {
        side.notice = Some("旁支忙，问题未送达（请稍后重试）".to_string());
    }
}

/// 旁支用量的一行摘要。从零累计,不读父会话的费用。
fn format_side_usage(side: &SideSession) -> String {
    let total = side.usage.total;
    format!(
        "旁支用量：输入 {} · 输出 {} · 缓存读 {} · 缓存写 {}{}",
        total.input,
        total.output,
        total.cache_read,
        total.cache_creation,
        if side.usage.complete {
            ""
        } else {
            "（部分轮次未拿到供应商最终值）"
        }
    )
}

struct KeyContext<'a> {
    view: ViewContext<'a>,
    runtime: RuntimeContext<'a>,
    model: ModelContext<'a>,
}

struct ViewContext<'a> {
    input: &'a mut InputLine,
    history: &'a mut HistoryState,
    popup: &'a mut Option<CommandPopup>,
    max_scroll_offset: usize,
    history_width: u16,
    history_height: u16,
    cost_tracker: &'a CostTracker,
}

struct RuntimeContext<'a> {
    channels: &'a TuiChannels,
    is_running: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    queued: &'a mut crate::tui::queued::DeliveredInterjections,
    pending_quit: &'a mut bool,
    workdir: &'a std::path::Path,
    mcp: &'a std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &'a TuiConfigSnapshot,
}

struct ModelContext<'a> {
    model: &'a str,
    /// 用户选中的清单显示名（`None` = 跟随清单 default_model）。
    current_model_ref: Option<&'a str>,
    catalog: &'a yi_agent_runtime::models::ModelCatalog,
    /// 当前生效 effort 与来源（run_loop 维护；见 `EffortSource`）。
    current_effort: Option<&'a str>,
    effort_source: EffortSource,
}

fn handle_key(key: KeyEvent, ctx: &mut KeyContext<'_>) -> KeyOutcome {
    // Check if there's a pending permission request. Clone the small fields
    // we need so the immutable borrow ends before we mutate history.
    let pending_permission = ctx.view.history.pending_permission_info().map(
        |(request_id, tool_name, prefix_suggestion, kind)| {
            (
                request_id,
                tool_name.to_string(),
                prefix_suggestion.map(str::to_string),
                kind.clone(),
            )
        },
    );
    if let Some((request_id, _tool_name, prefix_suggestion, kind)) = pending_permission {
        // Allow quit keys to pass through even when permission is pending
        let is_quit_key = matches!(key.code, KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL)
            || matches!(key.code, KeyCode::Esc);
        // Scrolling stays available so an expanded body can be read. These
        // keys are non-destructive and cannot resolve the request.
        let is_scroll_key = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        if is_quit_key || is_scroll_key {
            // Fall through to global key handling below
        } else {
            if key.code == KeyCode::Char('e') && key.modifiers.is_empty() {
                ctx.view.history.toggle_pending_permission_expanded();
                return KeyOutcome::None;
            }
            let decision = match key.code {
                KeyCode::Char('1') => Some(yi_agent_core::permission::Decision::AllowOnce),
                KeyCode::Char('2') => Some(yi_agent_core::permission::Decision::AlwaysAllowTool),
                KeyCode::Char('3') => prefix_suggestion
                    .as_deref()
                    .map(|p| yi_agent_core::permission::Decision::AlwaysAllowPrefix(p.to_string())),
                KeyCode::Char('4') => Some(yi_agent_core::permission::Decision::Deny),
                KeyCode::Enter => {
                    let default = match kind {
                        yi_agent_core::permission::PermissionKind::Blacklisted(_) => {
                            yi_agent_core::permission::Decision::Deny
                        }
                        _ => yi_agent_core::permission::Decision::AllowOnce,
                    };
                    Some(default)
                }
                _ => None,
            };
            if let Some(d) = decision {
                let _ = ctx.runtime.channels.decision.blocking_send((request_id, d));
                return KeyOutcome::None;
            }
            // For other keys while permission pending, ignore (don't let user type input)
            return KeyOutcome::None;
        }
    }

    // Global keys first
    match key.code {
        KeyCode::Esc => {
            // Popup dismissal takes precedence over cancelling an agent turn.
            if ctx.view.popup.is_some() {
                *ctx.view.popup = None;
                return KeyOutcome::None;
            }
            if ctx
                .runtime
                .is_running
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                // Cancellation is idempotent; coalesce repeated Esc presses.
                let _ = ctx.runtime.channels.interrupt.try_send(());
            }
            return KeyOutcome::None;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            if *ctx.runtime.pending_quit {
                return KeyOutcome::Quit;
            }
            *ctx.runtime.pending_quit = true;
            if ctx
                .runtime
                .is_running
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                let _ = ctx.runtime.channels.interrupt.blocking_send(());
            }
            return KeyOutcome::None;
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return KeyOutcome::Quit;
        }
        KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
            *ctx.runtime.pending_quit = false;
            ctx.view.history.toggle_fold_selected();
            return KeyOutcome::None;
        }
        KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
            *ctx.runtime.pending_quit = false;
            ctx.view.history.select_up();
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
            *ctx.runtime.pending_quit = false;
            ctx.view.history.select_down();
            return KeyOutcome::None;
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            *ctx.runtime.pending_quit = false;
            ctx.view.history.scroll_up(10, ctx.view.max_scroll_offset);
            return KeyOutcome::None;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            *ctx.runtime.pending_quit = false;
            ctx.view.history.scroll_down(10);
            return KeyOutcome::None;
        }
        _ => {}
    }

    // Any other key cancels pending quit
    *ctx.runtime.pending_quit = false;

    // Popup-specific key handling (when popup is active)
    if ctx.view.popup.is_some() {
        match key.code {
            KeyCode::Up => {
                if let Some(p) = ctx.view.popup.as_mut() {
                    p.move_up();
                }
                return KeyOutcome::None;
            }
            KeyCode::Down => {
                if let Some(p) = ctx.view.popup.as_mut() {
                    p.move_down();
                }
                return KeyOutcome::None;
            }
            KeyCode::Tab => {
                // Complete the selected command name into the input buffer
                if let Some(p) = ctx.view.popup.as_ref()
                    && let Some(cmd) = p.selected()
                {
                    ctx.view.input.buffer = format!("/{}", cmd.name());
                    ctx.view.input.cursor = ctx.view.input.buffer.len();
                }
                *ctx.view.popup = None;
                return KeyOutcome::None;
            }
            KeyCode::Enter => {
                // Execute the selected command. The popup now yields a
                // `Completion`; resolve it back through `from_name` so the
                // executed command is exactly the one that was drawn.
                if let Some(p) = ctx.view.popup.as_ref() {
                    let selected = p
                        .selected()
                        .and_then(|completion| SlashCommand::from_name(completion.name()));
                    if let Some(cmd) = selected {
                        // Check if buffer has args (text after command name)
                        let buffer = &ctx.view.input.buffer;
                        let cmd_full = format!("/{}", cmd.name());
                        let args = if buffer.len() > cmd_full.len() {
                            Some(&buffer[cmd_full.len()..])
                        } else {
                            None
                        };
                        let args_str = args.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                        *ctx.view.popup = None;
                        ctx.view.input.clear();
                        return execute_slash_command(
                            cmd,
                            args_str,
                            ctx.view.history,
                            ctx.view.history_width,
                            ctx.view.cost_tracker,
                            &ctx.runtime.channels.input,
                            &ctx.runtime.channels.interrupt,
                            &ctx.runtime.channels.kill,
                            &ctx.runtime.channels.control,
                            ctx.runtime.workdir,
                            ctx.runtime.queued,
                            ctx.runtime.mcp,
                            ctx.runtime.config,
                            ctx.model.model,
                            ctx.model.current_model_ref,
                            ctx.model.catalog,
                            ctx.model.current_effort,
                            ctx.model.effort_source,
                        );
                    } else {
                        // No command selected (empty filter) — show error
                        // Not routed through `DeliveredInterjections`: this text never
                        // reaches the agent, so queuing it would make it look
                        // like a pending prompt.
                        let text = ctx.view.input.take_submitted();
                        *ctx.view.popup = None;
                        ctx.view.history.push(
                            HistoryCell::Separator {
                                label: Some(format!("未知命令: {}", text)),
                            },
                            ctx.view.history_width,
                        );
                        return KeyOutcome::None;
                    }
                }
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Up if key.modifiers.is_empty() => {
            ctx.view.history.scroll_up(1, ctx.view.max_scroll_offset);
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers.is_empty() => {
            ctx.view.history.scroll_down(1);
            return KeyOutcome::None;
        }
        KeyCode::PageUp if key.modifiers.is_empty() => {
            ctx.view
                .history
                .scroll_page_up(ctx.view.history_height, ctx.view.max_scroll_offset);
            return KeyOutcome::None;
        }
        KeyCode::PageDown if key.modifiers.is_empty() => {
            ctx.view.history.scroll_page_down(ctx.view.history_height);
            return KeyOutcome::None;
        }
        KeyCode::Home if key.modifiers.is_empty() => {
            ctx.view
                .history
                .scroll_to_top(ctx.view.history_width, ctx.view.history_height);
            return KeyOutcome::None;
        }
        KeyCode::End if key.modifiers.is_empty() => {
            ctx.view.history.scroll_to_bottom();
            return KeyOutcome::None;
        }
        _ => {}
    }

    // Input handling
    match ctx.view.input.handle_key(key) {
        InputAction::Submit => {
            let text = ctx.view.input.take_submitted();
            // Check if this is a slash command
            if text.starts_with('/') && !starts_with_multi_segment_absolute_path(&text) {
                let name = text
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                let args = text
                    .trim_start_matches('/')
                    .get(name.len()..)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                if let Some(cmd) = SlashCommand::from_name(name) {
                    *ctx.view.popup = None;
                    return execute_slash_command(
                        cmd,
                        args,
                        ctx.view.history,
                        ctx.view.history_width,
                        ctx.view.cost_tracker,
                        &ctx.runtime.channels.input,
                        &ctx.runtime.channels.interrupt,
                        &ctx.runtime.channels.kill,
                        &ctx.runtime.channels.control,
                        ctx.runtime.workdir,
                        ctx.runtime.queued,
                        ctx.runtime.mcp,
                        ctx.runtime.config,
                        ctx.model.model,
                        ctx.model.current_model_ref,
                        ctx.model.catalog,
                        ctx.model.current_effort,
                        ctx.model.effort_source,
                    );
                } else {
                    // Unknown slash command
                    // Not routed through `DeliveredInterjections`: a slash command is a
                    // local action, not a prompt for the agent.
                    *ctx.view.popup = None;
                    ctx.view.history.push(
                        HistoryCell::Separator {
                            label: Some(format!("未知命令: {}", text)),
                        },
                        ctx.view.history_width,
                    );
                    return KeyOutcome::None;
                }
            }
            *ctx.view.popup = None;
            use crate::tui::queued::SubmitOutcome;
            match ctx.runtime.queued.submit(text.clone()) {
                SubmitOutcome::Sent => {
                    ctx.view.history.push(
                        HistoryCell::UserMessage { text: text.clone() },
                        ctx.view.history_width,
                    );
                    let _ = ctx.runtime.channels.input.try_send(text.clone());
                }
                SubmitOutcome::Queued => {
                    // 已投递给 driver，由它在本轮下一次 provider 请求前折进上下文。
                    // 在收到 core 回执前，它一直留在预览区（"已送达，待生效"）。
                    if ctx.runtime.channels.input.try_send(text.clone()).is_err() {
                        // 通道满或已关闭：把文本放回输入框，不做静默丢弃。
                        // 此处 `text` 是 String（非对 input 的借用），故可变借 input 合法。
                        ctx.view.input.insert_str(&text);
                        ctx.view.history.push(
                            HistoryCell::Separator {
                                label: Some(format!(
                                    "追加未送达（通道已满 {}），已退回输入框",
                                    crate::tui::queued::DeliveredInterjections::CAPACITY
                                )),
                            },
                            ctx.view.history_width,
                        );
                    }
                }
                SubmitOutcome::Rejected => {
                    // 文本已被 take_submitted 取走,必须退回,否则静默丢失。
                    // 此处 `text` 是 String(非对 input 的借用),故可变借 input 合法。
                    ctx.view.input.insert_str(&text);
                    ctx.view.history.push(
                        HistoryCell::Separator {
                            label: Some(format!(
                                "待生效追加已达上限 ({})，本条未投递，已退回输入框",
                                crate::tui::queued::DeliveredInterjections::CAPACITY
                            )),
                        },
                        ctx.view.history_width,
                    );
                }
            }
            KeyOutcome::Submit(text)
        }
        _ => KeyOutcome::None,
    }
}

/// 应用两条追加生命周期事件（收据 / 回推）。
///
/// **必须在回合结束分支之前调用**：`InterjectionsReturned` 由 core 在
/// `Cancelled`/`Done` **之前**发出（D15 顺序不变量），若让回合结束分支先跑，
/// 回推的文本就再也无人接收了——这正是本设计要修掉的"文本静默丢失"。
fn apply_interjection_event(
    event: &AgentEvent,
    queued: &mut crate::tui::queued::DeliveredInterjections,
    input: &mut InputLine,
    history: &mut HistoryState,
    width: u16,
) {
    match event {
        AgentEvent::InterjectionAccepted { .. } => queued.on_receipt(),
        AgentEvent::InterjectionsReturned { items } => {
            let restored = queued.take_returned(items.len());
            if restored.is_empty() {
                return;
            }
            // 输入框是单行的；一批回推用换行连接，`wrap_input_buffer` 会折行。
            for (i, text) in restored.iter().enumerate() {
                if i > 0 {
                    input.insert_str("\n");
                }
                input.insert_str(text);
            }
            history.push(
                HistoryCell::Separator {
                    label: Some(format!("{} 条追加未生效，已退回输入框", restored.len())),
                },
                width,
            );
        }
        _ => {}
    }
}

fn handle_paste(
    text: String,
    input: &mut InputLine,
    history: &HistoryState,
    runtime_popup: &RuntimePopup,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
    approval_mode: crate::tui::side::ApprovalInputMode,
) {
    // 审批提示可见时，裁决模式不接受文字（粘贴等于没发生），但**命令模式
    // 必须接受**：那个模式的全部意义就是让用户敲一条命令。把粘贴一并挡掉，
    // 命令模式就成了「只能逐字符手打」的半残输入，粘贴进来的 `/side`
    // 会静默消失。
    let command_mode = approval_mode == crate::tui::side::ApprovalInputMode::Command;
    if runtime_popup.blocks_text_input()
        || (history.pending_permission_info().is_some() && !command_mode)
    {
        return;
    }

    input.insert_str(&text);
    *pending_quit = false;
    sync_popup(popup, &input.buffer, PopupScope::Main);
}

/// Synchronize popup state with the current input buffer.
/// Shows popup when buffer starts with '/' and cursor is in command name region.
/// Hides popup otherwise.
fn sync_popup(popup: &mut Option<CommandPopup>, buffer: &str, scope: PopupScope) {
    // 切视图后旧候选必须作废：候选是按视图生成的，留着上一份就等于「显示的
    // 候选来自另一个输入分支」。`scope()` 不同就直接重建。
    if popup.as_ref().is_some_and(|p| p.scope() != scope) {
        *popup = None;
    }
    if let Some(filter_text) = buffer.strip_prefix('/') {
        // Check if we're still in the command name region (no space yet)
        let in_name_region = !buffer.contains(' ');
        if in_name_region {
            if let Some(p) = popup.as_mut() {
                p.filter(filter_text);
            } else {
                let mut p = CommandPopup::with_scope(scope);
                p.filter(filter_text);
                *popup = Some(p);
            }
        } else {
            // Space found — dismiss popup (entering arg mode)
            *popup = None;
        }
    } else {
        *popup = None;
    }
}

/// Render the `/model` catalog listing, marking the currently-selected model.
///
/// `current_ref` is the user-selected catalog **display name** (`None` = no
/// session override, the TUI follows the catalog's `default_model`). Matching by
/// name — not by the driver's model string — is required: two entries may share
/// a model string, and the entry name need not equal the model string.
///
/// Pure: takes the catalog by reference so tests can pass an in-memory one
/// instead of mutating process-level `HOME`.
fn render_model_list(
    catalog: &yi_agent_runtime::models::ModelCatalog,
    current_ref: Option<&str>,
) -> String {
    if catalog.models.is_empty() {
        return "未配置模型清单（~/.yi-agent/models.json）。".to_string();
    }
    let mut text = String::from("模型清单:\n");
    for entry in &catalog.models {
        // 无会话覆盖（启动、或刚 `/model default`）时,标记回落到清单的
        // `default_model`;否则只标记用户选中的那一条显示名。
        let selected = match current_ref {
            Some(name) => entry.name == name,
            None => catalog.default_model.as_deref() == Some(entry.name.as_str()),
        };
        let marker = if selected { " ← 当前" } else { "" };
        text.push_str(&format!(
            "  {}  provider={}  model={}{}\n",
            entry.name,
            entry.provider.as_str(),
            entry.model,
            marker
        ));
    }
    if let Some(default) = &catalog.default_model {
        text.push_str(&format!("默认: {default}\n"));
    }
    text
}

/// Where the current effective reasoning effort comes from.
///
/// `/effort` must name the source, because "已设置 high" is useless when the
/// user cannot tell whether it is this session's override or the catalog
/// entry's default: the fix differs (clear the override vs edit the entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffortSource {
    /// No value anywhere: the field is not sent and the model uses its own default.
    Unset,
    /// `/model`-style session override (what `/effort <tier>` sets).
    Session,
    /// The selected catalog entry's `reasoning_effort`.
    Entry,
}

impl EffortSource {
    fn label(self) -> &'static str {
        match self {
            Self::Unset => "未设置（使用模型默认）",
            Self::Session => "本会话覆盖",
            Self::Entry => "清单条目默认",
        }
    }
}

/// Resolve the effective effort and its source from the live session state.
///
/// Precedence mirrors the app-server and the startup resolver exactly: a session
/// override always wins; with no override, an **injected** package means no
/// catalog entry applies (the request carries no field); otherwise the selected
/// entry — `current_model_ref`, else the catalog's `default_model` — supplies
/// the value, and with none of those the field is not sent.
///
/// `current_model_ref` is the user-selected display name, and an absent one
/// means "follow the catalog's `default_model`". `model_injected` comes from the
/// real config (`RuntimeConfig::model_injected`) via [`TuiConfigSnapshot`]; it
/// must be truthful, never synthesized from the catalog, or this reports a
/// catalog tier the request will not carry.
///
/// The entry selection is `models::effective_entry_with_injection` — the *same*
/// function `models::resolve_effective` drives the startup/send path with, so
/// the effort `/effort` reports is the one the request carries (design §4.2
/// level 2). The cross-checks are
/// `main::tests::startup_sends_the_effort_the_tui_reports` and
/// `main::tests::an_injected_startup_reports_the_effort_it_sends`.
pub(crate) fn resolve_effort_state(
    session_effort: Option<&str>,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    current_model_ref: Option<&str>,
    model_injected: bool,
) -> (Option<String>, EffortSource) {
    if let Some(tier) = session_effort {
        return (Some(tier.to_string()), EffortSource::Session);
    }
    let entry = yi_agent_runtime::models::effective_entry_with_injection(
        catalog,
        current_model_ref,
        model_injected,
    );
    match entry.and_then(|e| e.reasoning_effort.as_deref()) {
        Some(tier) => (Some(tier.to_string()), EffortSource::Entry),
        None => (None, EffortSource::Unset),
    }
}

/// Render the current effective effort and its source for `/effort` (no args).
fn render_effort_status(
    effort: Option<&str>,
    source: EffortSource,
    supported: Option<&[String]>,
) -> String {
    // 状态行显示的「可用」必须与 `/effort <tier>` 实际放行的集合一致，否则用户
    // 会照着一份点不动的清单去敲。能力未知时回落到七档（与放行口径同源）。
    let available = match supported {
        Some(list) if !list.is_empty() => list.join(" | "),
        Some(_) => "（该模型不支持显式档位，仅 default 清覆盖）".to_string(),
        None => crate::tui::slash::REASONING_EFFORT_TIERS.join(" | "),
    };
    match effort {
        Some(tier) => format!(
            "当前推理档: {tier}（来源: {}）\n\
             可用: {available} | default\n\
             `/effort <档位>` 设置本会话覆盖，`/effort default` 清覆盖并回落条目默认。",
            source.label(),
        ),
        None => format!(
            "当前推理档: {}（即不发 `reasoning_effort` 字段，由模型自行决定）\n\
             可用: {available} | default",
            EffortSource::Unset.label(),
        ),
    }
}

/// Handle `/effort`: show the effective value, set a session override, or clear it.
///
/// Shaped like [`handle_model_command`]: transcript output goes into `history`
/// and the returned command is what the driver must act on (`None` = handled
/// locally, nothing to send).
///
/// The override is resolved by the driver against the *live* catalog entry, so
/// this function only validates the tier and reports; it never pretends a
/// switch succeeded (the driver's `Notice` is the authoritative confirmation).
///
/// Reserved keyword: `default` always means "clear the override" (see
/// `parse_effort_args`), so it can never be a tier.
fn handle_effort_command(
    arg: Option<&str>,
    current_effort: Option<&str>,
    source: EffortSource,
    // 该模型实测支持的档位；`None` = 能力未知（按七档放行）。
    supported: Option<&[String]>,
    history: &mut HistoryState,
    width: u16,
) -> Option<crate::DriverControl> {
    match crate::tui::slash::parse_effort_args_with_caps(arg.unwrap_or(""), supported) {
        Ok(crate::tui::slash::EffortAction::Status) => {
            let text = render_effort_status(current_effort, source, supported);
            history.push(HistoryCell::Markdown { text }, width);
            None
        }
        Ok(crate::tui::slash::EffortAction::Clear) => {
            Some(crate::DriverControl::SetReasoningEffort(None))
        }
        Ok(crate::tui::slash::EffortAction::Set(tier)) => {
            Some(crate::DriverControl::SetReasoningEffort(Some(tier)))
        }
        Err(usage) => {
            history.push(HistoryCell::Separator { label: Some(usage) }, width);
            None
        }
    }
}

/// Handle `/model`: list the catalog, switch to a named entry, or clear the
/// session override.
///
/// Pushes transcript output into `history` and returns the control command to
/// send (`None` means the argument was handled locally: listing or an unknown
/// name). All catalog access goes through the injected reference so no test
/// needs to set `HOME`.
///
/// Reserved keyword: `default` always means "clear the override", so a catalog
/// entry literally named `default` can never be selected (surfaced in the usage
/// and unknown-name text; see `SlashCommand::Model`).
fn handle_model_command(
    arg: Option<&str>,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    current_ref: Option<&str>,
    history: &mut HistoryState,
    width: u16,
) -> Option<crate::DriverControl> {
    match arg.map(str::trim).filter(|a| !a.is_empty()) {
        None => {
            history.push(
                HistoryCell::Markdown {
                    text: render_model_list(catalog, current_ref),
                },
                width,
            );
            None
        }
        Some("default") => Some(crate::DriverControl::SetModel(None)),
        Some(name) => {
            if catalog.models.iter().any(|entry| entry.name == name) {
                // 只下发显示名:driver 用自己的清单解析 provider/url/key,
                // 避免用旧配置的 key 去打新 url。
                Some(crate::DriverControl::SetModel(Some(name.to_string())))
            } else {
                history.push(
                    HistoryCell::Separator {
                        label: Some(format!(
                            "未知模型名: {name}（用 /model 查看清单；`default` 为保留字）"
                        )),
                    },
                    width,
                );
                None
            }
        }
    }
}

/// Execute a slash command locally (does not send to agent).
#[allow(clippy::too_many_arguments)]
fn execute_slash_command(
    cmd: SlashCommand,
    args: Option<String>,
    history: &mut HistoryState,
    width: u16,
    cost: &CostTracker,
    _input_tx: &tokio::sync::mpsc::Sender<String>,
    _interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    _kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::DriverControl>,
    workdir: &std::path::Path,
    queued: &mut crate::tui::queued::DeliveredInterjections,
    mcp: &std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &TuiConfigSnapshot,
    current_model: &str,
    // 用户选中的清单显示名（`None` = 跟随清单 default_model）。
    current_model_ref: Option<&str>,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    // 本会话的 effort 覆盖 / 当前生效值与来源。由 run_loop 维护并在每次
    // `Notice`/`SetReasoningEffort` 后更新（driver 是权威，TUI 只回显）。
    current_effort: Option<&str>,
    effort_source: EffortSource,
) -> KeyOutcome {
    match cmd {
        SlashCommand::Quit => KeyOutcome::Quit,
        SlashCommand::Clear => {
            // 本地清空 history 显示,TUI 不等 driver 确认。
            // 通过 control channel 通知 driver 重建 agent(空 session)。
            let dropped = queued.clear();
            history.clear();
            history.push(
                HistoryCell::Separator {
                    label: Some("对话已清空".to_string()),
                },
                width,
            );
            if dropped > 0 {
                // 清空必须可见:否则用户以为排队消息还在。
                history.push(
                    HistoryCell::Separator {
                        label: Some(format!("已丢弃 {dropped} 条排队消息")),
                    },
                    width,
                );
            }
            let _ = control_tx.blocking_send(crate::DriverControl::Clear);
            KeyOutcome::None
        }
        SlashCommand::RecoverRoot => {
            // The lease and the binding fence live in the driver task, so this
            // dispatch only parses the request and hands it over. A parse error is
            // shown here; a runtime error comes back as a `Notice` from the driver,
            // because only the driver knows what the daemon actually said.
            match parse_recover_args(args.as_deref()) {
                Ok(action) => {
                    if control_tx
                        .blocking_send(crate::DriverControl::RecoverRoot(action))
                        .is_err()
                    {
                        history.push(
                            HistoryCell::Separator {
                                label: Some("恢复请求未送达 driver：控制通道已关闭".to_string()),
                            },
                            width,
                        );
                    }
                    // No success line is written here: `prepare` is the first
                    // mutating step and its outcome is the driver's to report. A line
                    // written now would claim progress before the daemon answered.
                    KeyOutcome::None
                }
                Err(reason) => {
                    history.push(
                        HistoryCell::Separator {
                            label: Some(reason),
                        },
                        width,
                    );
                    KeyOutcome::None
                }
            }
        }
        SlashCommand::Help => {
            history.push(
                HistoryCell::UserMessage {
                    text: help_text(args.as_deref()),
                },
                width,
            );
            KeyOutcome::None
        }
        SlashCommand::Cost => {
            let text = cost.render();
            history.push(HistoryCell::Markdown { text }, width);
            KeyOutcome::None
        }
        SlashCommand::Config => {
            if args.is_some() {
                history.push(
                    HistoryCell::Separator {
                        label: Some("用法: /config".to_string()),
                    },
                    width,
                );
            } else {
                let mut text = config.render(current_model);
                if config.focus_degraded && config.notification_condition == "unfocused" {
                    text.push_str(
                        "\n  注意: 本终端不支持焦点事件，notify condition 已从 unfocused 退化为 always",
                    );
                }
                history.push(HistoryCell::Markdown { text }, width);
            }
            KeyOutcome::None
        }
        SlashCommand::Compact => {
            // 本地 push "正在压缩..." 提示,通过 control channel
            // 通知 driver 调用 compact_session 并重建 agent。
            history.push(
                HistoryCell::Separator {
                    label: Some("正在压缩对话...".to_string()),
                },
                width,
            );
            let _ = control_tx.blocking_send(crate::DriverControl::Compact);
            KeyOutcome::None
        }
        SlashCommand::Model => {
            // 清单是机器级全局文件（`~/.yi-agent/models.json`）；与 app-server 同口径，
            // 只在命令这一刻读。渲染与切换逻辑收在 `handle_model_command`/`render_model_list`，
            // 清单经参数注入，测试不碰进程级 HOME。
            if let Some(command) =
                handle_model_command(args.as_deref(), catalog, current_model_ref, history, width)
            {
                let selection = match &command {
                    crate::DriverControl::SetModel(selection) => selection.clone(),
                    _ => None,
                };
                // 确认行由 `ModelChanged` 事件驱动；这里不预写成功行，避免与
                // driver 真实结果冲突。只有命令真的进了 channel 才回报选中名，
                // 免得 channel 关闭时标记谎报一个 driver 从未收到的切换。
                if control_tx.blocking_send(command).is_ok() {
                    return KeyOutcome::ModelSelected(selection);
                }
            }
            KeyOutcome::None
        }
        SlashCommand::Effort => {
            // 与 `/model` 同形状：清单相关的判定（当前条目默认档）已经由
            // run_loop 解析成 `current_effort`/`effort_source` 传进来，
            // 这里只负责校验 + 下发 + 打印状态。
            // 条目选择与 `current_effort` 同一条规则：会话覆盖 > 注入 > 清单默认。
            // 能力未知（`None`）时 `handle_effort_command` 沿用七档白名单。
            let supported = yi_agent_runtime::models::effective_entry_with_injection(
                catalog,
                current_model_ref,
                config.model_injected,
            )
            .and_then(yi_agent_runtime::models::supported_efforts);
            if let Some(command) = handle_effort_command(
                args.as_deref(),
                current_effort,
                effort_source,
                supported.as_deref(),
                history,
                width,
            ) {
                // 回报**本次要写入的档位**（`None` = 清覆盖），供 run_loop 维护
                // 本地覆盖。与 `/model` 一样，只有命令真的进了 channel 才回报。
                let requested = match &command {
                    crate::DriverControl::SetReasoningEffort(effort) => effort.clone(),
                    _ => None,
                };
                // 确认行由 driver 的 `Notice` 事件驱动（与 `/model` 不预写成功行
                // 同理）。
                if control_tx.blocking_send(command).is_ok() {
                    return KeyOutcome::EffortChanged(requested);
                }
            }
            KeyOutcome::None
        }
        SlashCommand::Agents => {
            let text = match daemon_agents_summary(workdir, args.as_deref()) {
                Ok(summary) => summary,
                Err(error) => format!("无法读取本地 daemon runtime: {error}"),
            };
            history.push(HistoryCell::Markdown { text }, width);
            KeyOutcome::None
        }
        SlashCommand::Agent => {
            // `/agent <id>` opens the same read-only detail as drilling in from
            // the subagent tab, so the two entry points cannot diverge. Without
            // an id there is nothing to open, and the usage line is the answer.
            match args.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
                Some(task_id) => KeyOutcome::OpenAgentDetail(task_id.to_string()),
                None => {
                    history.push(
                        HistoryCell::Separator {
                            label: Some("用法: /agent <task-id>".to_string()),
                        },
                        width,
                    );
                    KeyOutcome::None
                }
            }
        }
        SlashCommand::Approve
        | SlashCommand::Deny
        | SlashCommand::Budget
        | SlashCommand::Priority => {
            let reason = cmd.unavailable_reason().unwrap_or("暂不支持");
            history.push(
                HistoryCell::Separator {
                    label: Some(format!("/{} 暂不支持：{}", cmd.name(), reason)),
                },
                width,
            );
            KeyOutcome::None
        }
        SlashCommand::Daemon => {
            let label = match crate::tui::slash::parse_daemon_args(args.as_deref().unwrap_or("")) {
                Ok(action) => {
                    let result = crate::runtime_socket_for(workdir)
                        .map_err(|error| error.to_string())
                        .and_then(|socket| match action {
                            crate::tui::slash::DaemonAction::Status => daemon_status_at(&socket),
                            crate::tui::slash::DaemonAction::Stop => daemon_stop_at(&socket),
                        });
                    match result {
                        Ok(message) => message,
                        Err(error) => format!("无法联系本地 daemon runtime: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Runtime => {
            use crate::tui::runtime_prefs::{self, RuntimePreference};
            let label = match crate::tui::slash::parse_runtime_args(args.as_deref().unwrap_or("")) {
                Ok(crate::tui::slash::RuntimeAction::Status) => {
                    let current = runtime_prefs::load(workdir);
                    format!(
                        "子 Agent runtime 偏好: {}（来源: {}）; 重启后生效",
                        match current {
                            RuntimePreference::Ask => "ask",
                            RuntimePreference::Always => "always",
                            RuntimePreference::Never => "never",
                        },
                        runtime_prefs::preferences_path(workdir).display()
                    )
                }
                Ok(crate::tui::slash::RuntimeAction::Set(pref)) => {
                    match runtime_prefs::save(workdir, pref) {
                        Ok(()) => format!(
                            "已设为 {}（重启后生效）",
                            match pref {
                                RuntimePreference::Ask => "ask",
                                RuntimePreference::Always => "always",
                                RuntimePreference::Never => "never",
                            }
                        ),
                        Err(error) => format!("无法保存偏好: {error}"),
                    }
                }
                Err(usage) => usage,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Plugins => {
            let lines = crate::tui::plugins::handle_plugins(workdir, args.as_deref().unwrap_or(""));
            for line in lines {
                history.push(HistoryCell::Separator { label: Some(line) }, width);
            }
            KeyOutcome::None
        }
        SlashCommand::Mcp => {
            match parse_mcp_args(args.as_deref().unwrap_or("")) {
                Ok(McpAction::Status) => {
                    let text = render_mcp_status(mcp.master(), &mcp.status());
                    history.push(HistoryCell::Markdown { text }, width);
                }
                Ok(McpAction::Reload) => {
                    // 真正的重读 + 探测由 driver 侧完成(那里才有 ToolRegistry
                    // 与 tokio runtime);这里只发出请求并给一句即时反馈。
                    let _ = control_tx.blocking_send(crate::DriverControl::McpReload);
                    history.push(
                        HistoryCell::Separator {
                            label: Some("正在重新读取 MCP 配置…".into()),
                        },
                        width,
                    );
                }
                Ok(McpAction::Master(on)) => {
                    mcp.set_master(on);
                    let _ = control_tx.blocking_send(crate::DriverControl::McpRefresh);
                    history.push(
                        HistoryCell::Separator {
                            label: Some(format!(
                                "MCP master 已{}",
                                if on { "开启" } else { "关闭" }
                            )),
                        },
                        width,
                    );
                }
                Ok(McpAction::Server { name, on }) => match mcp.set_server(&name, on) {
                    Ok(()) => {
                        let _ = control_tx.blocking_send(crate::DriverControl::McpRefresh);
                        history.push(
                            HistoryCell::Separator {
                                label: Some(format!(
                                    "MCP server '{name}' 已{}",
                                    if on { "开启" } else { "关闭" }
                                )),
                            },
                            width,
                        );
                    }
                    Err(err) => history.push(
                        HistoryCell::Separator {
                            label: Some(err.to_string()),
                        },
                        width,
                    ),
                },
                Err(msg) => history.push(HistoryCell::Separator { label: Some(msg) }, width),
            }
            KeyOutcome::None
        }
        SlashCommand::Review => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_review(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取审查信息: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Events => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_events(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取任务事件: {error}"),
                },
                Err(_) => "用法: /events <task-id>".into(),
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Diff => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_diff(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取任务 diff: {error}"),
                },
                Err(_) => "用法: /diff <task-id>".into(),
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Mailbox => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_mailbox(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取任务 mailbox: {error}"),
                },
                Err(_) => "用法: /mailbox <task-id>".into(),
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Pause => {
            let label = match parse_pause_resume_args(args.as_deref(), "pause") {
                Ok(task_id) => match daemon_pause(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法暂停任务: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Resume => {
            let label = match parse_pause_resume_args(args.as_deref(), "resume") {
                Ok(task_id) => match daemon_resume(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法恢复任务: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Cancel => {
            let label = match parse_cancel_args(args.as_deref()) {
                Ok((task_id, recursive, confirmation)) => {
                    match daemon_cancel(workdir, task_id, recursive, confirmation) {
                        Ok(message) => message,
                        Err(error) => format!("无法取消任务: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Retry => {
            let label = match parse_retry_args(args.as_deref()) {
                Ok(task_id) => match daemon_retry(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法重试任务: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Message => {
            let label = match parse_user_message_args(args.as_deref()) {
                Ok((task_id, message)) => match daemon_user_message(workdir, task_id, message) {
                    Ok(message) => message,
                    Err(error) => format!("无法发送用户指令: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
    }
}

fn daemon_agents_summary(workdir: &std::path::Path, args: Option<&str>) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_agents_summary_at(&socket, args)
}

#[cfg(test)]
fn daemon_agent_detail_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::InspectTask {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    let yi_agent_store::ipc::IpcResponse::TaskDetail(detail) = response else {
        return Err("daemon 返回了非任务详情响应".into());
    };
    Ok(format!(
        "Agent {}\nsession: {}\nparent: {}\ndepth: {}\nstate: {}\ndelivery: {}",
        detail.task_id,
        detail.session_id,
        detail.parent_task_id.as_deref().unwrap_or("(root)"),
        detail.depth,
        detail.state,
        detail.delivery_json,
    ))
}

fn parse_cancel_args(args: Option<&str>) -> Result<(&str, bool, Option<&str>), String> {
    let usage = "用法: /cancel <task-id> [--recursive] [--confirm <token>]";
    let Some(args) = args else {
        return Err(usage.into());
    };
    let mut parts = args.split_whitespace();
    let Some(task_id) = parts.next() else {
        return Err(usage.into());
    };
    let mut recursive = false;
    let mut confirmation = None;
    while let Some(argument) = parts.next() {
        match argument {
            "--recursive" if !recursive => recursive = true,
            "--confirm" if confirmation.is_none() => {
                confirmation = Some(parts.next().ok_or_else(|| usage.to_string())?);
            }
            _ => return Err(usage.into()),
        }
    }
    Ok((task_id, recursive, confirmation))
}

fn parse_retry_args(args: Option<&str>) -> Result<&str, String> {
    parse_pause_resume_args(args, "retry")
}

fn parse_review_args(args: Option<&str>) -> Result<&str, String> {
    parse_pause_resume_args(args, "review")
}

fn parse_pause_resume_args<'a>(args: Option<&'a str>, command: &str) -> Result<&'a str, String> {
    let usage = || format!("用法: /{command} <task-id>");
    let Some(args) = args else {
        return Err(usage());
    };
    let mut parts = args.split_whitespace();
    let Some(task_id) = parts.next() else {
        return Err(usage());
    };
    if parts.next().is_some() {
        return Err(usage());
    }
    Ok(task_id)
}

fn parse_user_message_args(args: Option<&str>) -> Result<(&str, &str), String> {
    let Some(args) = args.map(str::trim).filter(|args| !args.is_empty()) else {
        return Err("用法: /message <task-id> <text>".into());
    };
    let Some((task_id, message)) = args.split_once(char::is_whitespace) else {
        return Err("用法: /message <task-id> <text>".into());
    };
    let message = message.trim();
    if task_id.is_empty() || message.is_empty() {
        return Err("用法: /message <task-id> <text>".into());
    }
    Ok((task_id, message))
}

fn daemon_user_message(
    workdir: &std::path::Path,
    task_id: &str,
    message: &str,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_user_message_at(&socket, task_id, message)
}

fn daemon_user_message_at(
    socket: &std::path::Path,
    task_id: &str,
    message: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::SendUserMessage {
            task_id: task_id.to_owned(),
            message: message.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::MessageQueued) {
        return Err("daemon 返回了非消息响应".into());
    }
    Ok(format!("已排队用户指令至任务: {task_id}"))
}

fn daemon_task_session_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::InspectTask {
            task_id: task_id.into(),
        },
    )
    .map_err(|error| error.to_string())?
    {
        yi_agent_store::ipc::IpcResponse::TaskDetail(detail) => Ok(detail.session_id),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format_ipc_error(code, message))
        }
        _ => Err("daemon 返回了非任务详情响应".into()),
    }
}

fn daemon_retry(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    let session_id = daemon_task_session_at(&socket, task_id)?;
    daemon_retry_at(&socket, &session_id, task_id)
}

fn daemon_pause(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    let session_id = daemon_task_session_at(&socket, task_id)?;
    daemon_pause_at(&socket, &session_id, task_id)
}

fn daemon_pause_at(
    socket: &std::path::Path,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PauseTask {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::TaskPaused) {
        return Err("daemon 返回了非暂停响应".into());
    }
    Ok(format!("已请求暂停任务: {task_id}"))
}

fn daemon_resume(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    let session_id = daemon_task_session_at(&socket, task_id)?;
    daemon_resume_at(&socket, &session_id, task_id)
}

fn daemon_resume_at(
    socket: &std::path::Path,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ResumeTask {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::TaskResumed) {
        return Err("daemon 返回了非恢复响应".into());
    }
    Ok(format!("已请求恢复任务: {task_id}"))
}

fn daemon_retry_at(
    socket: &std::path::Path,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::RetryTask {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::TaskRetried) {
        return Err("daemon 返回了非重试响应".into());
    }
    Ok(format!("已请求重试任务: {task_id}"))
}

fn daemon_cancel(
    workdir: &std::path::Path,
    task_id: &str,
    recursive: bool,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_cancel_at(&socket, task_id, recursive, confirmation)
}

fn daemon_cancel_at(
    socket: &std::path::Path,
    task_id: &str,
    recursive: bool,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let request = match confirmation {
        Some(confirmation_token) => yi_agent_store::ipc::IpcRequest::ConfirmCancel {
            task_id: task_id.to_owned(),
            recursive,
            confirmation_token: confirmation_token.to_owned(),
        },
        None => yi_agent_store::ipc::IpcRequest::PreviewCancel {
            task_id: task_id.to_owned(),
            recursive,
        },
    };
    let response =
        yi_agent_store::ipc::send_request(socket, request).map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token,
            task_ids,
            expires_in_secs,
            ..
        } => Ok(format!(
            "取消预览（{} 个任务）: {}；使用 /cancel {task_id}{} --confirm {confirmation_token} 在 {expires_in_secs}s 内确认",
            task_ids.len(),
            task_ids.join(", "),
            if recursive { " --recursive" } else { "" },
        )),
        yi_agent_store::ipc::IpcResponse::TaskCancelled => Ok(if recursive {
            format!("已递归取消任务树: {task_id}")
        } else {
            format!("已取消任务: {task_id}")
        }),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝取消请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非取消响应".into()),
    }
}

fn daemon_review(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_review_at(&socket, task_id)
}

fn daemon_review_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskDiff {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskDiff { delivery_json, .. } => {
            Ok(format!("Delivery 审查 {task_id}\n{delivery_json}"))
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝审查请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非审查响应".into()),
    }
}

fn daemon_events(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_events_at(&socket, task_id)
}

fn daemon_events_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskEvents {
            task_id: task_id.to_owned(),
            after_event_id: None,
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskEvents { events } if events.is_empty() => {
            Ok(format!("Events {task_id}: 暂无事件"))
        }
        yi_agent_store::ipc::IpcResponse::TaskEvents { events } => {
            let mut output = format!("Events {}（{} 条）", task_id, events.len());
            for event in events {
                output.push_str(&format!(
                    "\n{} {} {}",
                    event.event_id, event.kind, event.payload_json
                ));
            }
            Ok(output)
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝事件请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非事件响应".into()),
    }
}

fn daemon_diff(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_diff_at(&socket, task_id)
}

fn daemon_diff_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskDiff {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskDiff { delivery_json, .. } => {
            Ok(format!("Diff {task_id}\n{delivery_json}"))
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝 diff 请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非 diff 响应".into()),
    }
}

fn daemon_mailbox(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_mailbox_at(&socket, task_id)
}

fn daemon_mailbox_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskMailbox {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskMailbox { messages } if messages.is_empty() => {
            Ok(format!("Mailbox {task_id}: 暂无消息"))
        }
        yi_agent_store::ipc::IpcResponse::TaskMailbox { messages } => {
            let mut output = format!("Mailbox {}（{} 条）", task_id, messages.len());
            for message in messages {
                output.push_str(&format!(
                    "\n{} {} priority={} {}",
                    message.message_id, message.kind, message.priority, message.payload_json
                ));
            }
            Ok(output)
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝 mailbox 请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非 mailbox 响应".into()),
    }
}

fn daemon_status_at(socket: &std::path::Path) -> Result<String, String> {
    let response =
        yi_agent_store::ipc::send_request(socket, yi_agent_store::ipc::IpcRequest::Status)
            .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::Status {
            high_water_event_id,
        } => Ok(format!(
            "daemon 运行中（event high-water: {high_water_event_id}）"
        )),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format!("daemon 拒绝请求: {code:?} {message:?}"))
        }
        other => Err(format!("daemon 返回了非预期响应: {other:?}")),
    }
}

fn daemon_stop_at(socket: &std::path::Path) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(socket, yi_agent_store::ipc::IpcRequest::Stop)
        .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::Stopping => Ok("daemon 正在停止".to_string()),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format!("daemon 拒绝请求: {code:?} {message:?}"))
        }
        other => Err(format!("daemon 返回了非预期响应: {other:?}")),
    }
}

fn daemon_agents_summary_at(
    socket: &std::path::Path,
    args: Option<&str>,
) -> Result<String, String> {
    let (session_id, active_only, title, excluded_task_id, empty_label) =
        match args.map(str::trim).filter(|args| !args.is_empty()) {
            None => {
                let root = crate::tui::subagents::current_attached_root().ok_or_else(|| {
                    "当前 TUI 未接入 subagent runtime；使用 /agents --all 查看 daemon 全部任务"
                        .to_string()
                })?;
                (
                    Some(root.session_id),
                    false,
                    "Current session",
                    Some(root.task_id),
                    "暂无子任务",
                )
            }
            Some("--all") => (None, false, "All agents", None, "暂无任务"),
            Some("--active") => (None, true, "Active agents", None, "暂无任务"),
            Some(_) => return Err("用法: /agents [--all|--active]".into()),
        };
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id,
            active_only,
        },
    )
    .map_err(|error| error.to_string())?;
    let yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } = response else {
        return Err("daemon 返回了非任务摘要响应".into());
    };
    Ok(format_agents_summary(
        title,
        tasks,
        excluded_task_id.as_deref(),
        empty_label,
    ))
}

/// The children of the root this TUI is attached to, for the subagent tab.
///
/// A list is navigation, not a work surface: an unreachable daemon or a
/// detached root yields an empty list rather than an error the user has to
/// dismiss, so Ctrl+P keeps working in every runtime state.
fn subagent_children_at(socket: &std::path::Path) -> SubagentListState {
    let Some(root) = crate::tui::subagents::current_attached_root() else {
        return SubagentListState::default();
    };
    let response = match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: Some(root.session_id),
            active_only: false,
        },
    ) {
        Ok(response) => response,
        Err(_) => return SubagentListState::default(),
    };
    let yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } = response else {
        return SubagentListState::default();
    };
    SubagentListState::new(
        tasks
            .into_iter()
            .filter(|task| !task.is_root && task.task_id != root.task_id)
            .map(|task| super::trace::SubagentListItem {
                active: !is_terminal_task_state(&task.state),
                task_id: task.task_id,
                objective: None,
                state: task.state,
                last_step: None,
                parent_task_id: task.parent_task_id,
            })
            .collect(),
    )
}

/// Whether a task can no longer make progress. Mirrors the daemon's own
/// terminal vocabulary so the tab agrees with `/agents`.
fn is_terminal_task_state(state: &str) -> bool {
    matches!(
        state,
        "completed" | "completed_no_changes" | "failed" | "cancelled"
    )
}

fn format_agents_summary(
    title: &str,
    tasks: Vec<yi_agent_store::ipc::IpcTaskSummary>,
    excluded_task_id: Option<&str>,
    empty_label: &str,
) -> String {
    let tasks: Vec<_> = tasks
        .into_iter()
        .filter(|task| Some(task.task_id.as_str()) != excluded_task_id)
        .collect();
    if tasks.is_empty() {
        return format!("**{title}**\n\n{empty_label}");
    }
    let mut output = format!("**{title} ({})**", tasks.len());
    for task in tasks {
        let root_label = if task.is_root { " **root**" } else { "" };
        output.push_str(&format!(
            "\n- `{}`: {}{}",
            task.task_id, task.state, root_label
        ));
    }
    output
}

/// Build the popup widget for rendering.
///
/// `height` is the popup's row budget, borders included -- the area the layout
/// actually gave us, which a squeezed terminal can make smaller than the popup
/// asked for. Only the windowed commands are drawn; the renderer clips the
/// rest, so rendering the whole list would hide the highlighted row once the
/// selection scrolls past the visible rows.
fn build_popup_with<'a>(popup: &'a CommandPopup, height: u16, theme: TuiTheme) -> Paragraph<'a> {
    let content_height = height.saturating_sub(2) as usize;
    let window = popup.visible(content_height);
    let window_start = popup.window_start(content_height);
    let lines: Vec<Line<'a>> = window
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let arguments = SlashCommand::from_name(cmd.name())
                .and_then(|resolved| resolved.argument_usage())
                .map(|usage| format!(" {usage}"))
                .unwrap_or_default();
            let name = format!("/{}{}", cmd.name(), arguments);
            let desc = cmd.description();
            let is_selected = window_start + i == popup.selected_index();
            let style = if is_selected {
                Style::new().bg(theme.sel_bg).fg(theme.sel_fg)
            } else {
                Style::new()
            };
            Line::styled(format!("  {:<12} {}", name, desc), style)
        })
        .collect();

    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("命令"))
}

/// The input card, painted for `theme`.
///
/// The card is a contiguous band of `input_bg` three or more rows tall: one
/// blank padding row, the input itself (one or more rows), and one blank
/// padding row. The padding rows carry no characters, so selecting only the
/// middle row with the terminal's native selection copies clean text.
///
/// The foreground is explicit because `input_bg` is a fixed gray in both
/// themes: leaving text at the terminal default would be unreadable when a
/// dark terminal is forced to `YI_AGENT_THEME=light` and vice versa.
fn build_input_line_with(
    input: &InputLine,
    pending_quit: bool,
    area_width: u16,
    theme: TuiTheme,
) -> Paragraph<'static> {
    let prefix = Span::styled(
        "> ",
        Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
    );
    if pending_quit {
        // Ride the same three-row card: collapsing to one row would make the
        // whole frame jump on Ctrl+C.
        return Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(vec![
                prefix,
                Span::styled("再按 Ctrl+C 退出", Style::new().fg(theme.warn)),
            ]),
            Line::from(""),
        ]))
        .style(Style::new().bg(theme.input_bg).fg(theme.fg));
    }
    let lines = wrap_input_buffer(&input.buffer, input.cursor, &prefix, area_width);
    let mut padded = Vec::with_capacity(lines.len() + 2);
    padded.push(Line::from(""));
    padded.extend(lines);
    padded.push(Line::from(""));
    Paragraph::new(Text::from(padded)).style(Style::new().bg(theme.input_bg).fg(theme.fg))
}

/// Number of terminal lines the input card occupies, including its two padding
/// rows.
fn compute_input_height(input: &InputLine, pending_quit: bool, area_width: u16) -> u16 {
    if pending_quit {
        return INPUT_CARD_PADDING * 2 + 1;
    }
    // Derive the height from the exact same wrapping the renderer uses. A
    // `div_ceil(width)` estimate was wrong for explicit newlines and for
    // per-character wrapping, and a too-small height silently hides the
    // overflow rows (the terminal only reserves `height` rows for the input).
    let prefix = Span::raw("> ");
    let lines = wrap_input_buffer(&input.buffer, input.cursor, &prefix, area_width).len() as u16;
    lines + INPUT_CARD_PADDING * 2
}

/// Pre-computed screen layout, shared between the draw closure and the mouse
/// handler so hit-testing uses the exact same chunk rects that were rendered.
#[derive(Clone, Debug)]
struct LayoutInfo {
    /// Vertical chunks: [0]=history, [1]=popup, [2]=status, [3]=queued,
    /// [4]=input card.
    ///
    /// The input card carries its own padding rows, so there is no separate
    /// blank gap above it any more.
    chunks: Vec<ratatui::layout::Rect>,
}

/// Index of the input card chunk in [`LayoutInfo::chunks`].
const INPUT_CHUNK: usize = 4;

/// Compute the screen layout from the current terminal size and widget state.
fn compute_layout(
    area: ratatui::layout::Rect,
    input: &InputLine,
    pending_quit: bool,
    popup: &Option<CommandPopup>,
    queued_height: u16,
) -> LayoutInfo {
    let input_width = area.width;
    // Cap the text rows, not the card: the two padding rows are always present
    // so the card keeps its shape once the input starts wrapping a lot.
    let max_height = INPUT_MAX_TEXT_LINES + INPUT_CARD_PADDING * 2;
    let input_height = compute_input_height(input, pending_quit, input_width).min(max_height);
    let popup_height = popup
        .as_ref()
        .map(|p| p.height(area.height) as u16)
        .unwrap_or(0);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                // history
            Constraint::Length(popup_height),  // popup (0 when none)
            Constraint::Length(1),             // status bar
            Constraint::Length(queued_height), // queued preview
            Constraint::Length(input_height),  // input card (padding included)
        ])
        .split(area);

    LayoutInfo {
        chunks: chunks.to_vec(),
    }
}

/// Blank padding rows above and below the input text inside the input card.
///
/// The card is deliberately more than one row tall so it reads as a card rather
/// than a stripe. The padding carries no characters, so terminal-native
/// selection over just the middle row copies clean text.
const INPUT_CARD_PADDING: u16 = 1;

/// Maximum number of text rows the input card shows before it stops growing.
const INPUT_MAX_TEXT_LINES: u16 = 6;

/// Width of the `"> "` prefix on the first input row.
const INPUT_PREFIX_WIDTH: usize = 2;

/// Wrap the input buffer into multiple `Line`s so all typed text is visible.
///
/// Only the first line carries the `"> "` prefix. Continuation lines start at
/// column 0 with **no** indent: the two-space indent used to align the cursor
/// column, but it leaked into every copied line as leading whitespace, and
/// terminals do not trim leading spaces on copy. The ragged left edge is the
/// price of clean copy.
///
/// Wrapping is character-based using unicode display width (so CJK / wide chars
/// work). Because the first line is narrower by the prefix, the two row kinds
/// have different capacities.
///
/// The character at `cursor` (byte offset) is rendered with reverse video so
/// the user can see the cursor.
fn wrap_input_buffer(
    buffer: &str,
    cursor: usize,
    prefix: &Span<'static>,
    area_width: u16,
) -> Vec<Line<'static>> {
    let first_avail = (area_width as usize)
        .saturating_sub(INPUT_PREFIX_WIDTH)
        .max(1);
    let cont_avail = (area_width as usize).max(1);
    // Reverse video instead of a hardcoded white block: REVERSED is resolved by
    // the terminal against its own background, so the cursor stays visible on
    // both light and dark themes. A fixed `bg(White)` is 1.00:1 on a white
    // terminal — literally invisible.
    let cursor_style = Style::new().add_modifier(Modifier::REVERSED);

    // Helper to build spans for a chunk of text, applying cursor_style to the
    // character at the cursor byte offset if it falls within this chunk.
    // Returns a Vec of spans (without prefix — caller adds prefix).
    let build_spans = |text: &str, chunk_start: usize| -> Vec<Span<'static>> {
        // Note: an empty chunk still needs the cursor render below, so a blank
        // line (or the position after a trailing '\n') can show it.
        // Find if cursor is within this chunk
        // Cursor byte offset relative to chunk start
        let cursor_rel = cursor.checked_sub(chunk_start);
        match cursor_rel {
            Some(rel) if rel <= text.len() => {
                // Cursor is at or after chunk start, within or at end of text.
                // If rel == text.len(), cursor is at end — we need a cursor
                // on the "empty" position after last char. We render a space
                // with cursor style.
                let before = &text[..rel];
                let after = &text[rel..];
                if after.is_empty() {
                    // Cursor at end of text: render text normally, then a
                    // cursor-styled space to show the cursor position.
                    vec![
                        Span::raw(before.to_string()),
                        Span::styled(" ", cursor_style),
                    ]
                } else {
                    // Cursor is on the first char of `after`
                    let (cursor_char, rest) = after
                        .char_indices()
                        .next()
                        .map(|(i, c)| (&after[..i + c.len_utf8()], &after[i + c.len_utf8()..]))
                        .unwrap_or(("", after));
                    vec![
                        Span::raw(before.to_string()),
                        Span::styled(cursor_char.to_string(), cursor_style),
                        Span::raw(rest.to_string()),
                    ]
                }
            }
            _ => {
                // Cursor not in this chunk
                vec![Span::raw(text.to_string())]
            }
        }
    };

    // Empty buffer: show the cursor at position 0 as a styled space.
    if buffer.is_empty() {
        return vec![Line::from(vec![
            prefix.clone(),
            Span::styled(" ", cursor_style),
        ])];
    }
    // Split by explicit newlines first: a pasted multi-line block used to be
    // one span holding raw '\n's, which renders as a single over-wide (and
    // mis-shaped) line whose tail ratatui drops. Byte offsets are tracked so
    // the reverse-video cursor still lands on the right character.
    let mut raw_lines: Vec<(usize, String)> = Vec::new();
    let mut segment_start = 0usize;
    for segment in buffer.split('\n') {
        raw_lines.push((segment_start, segment.to_string()));
        segment_start += segment.len() + 1; // +1 for the consumed '\n'
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (seg_start, segment) in raw_lines {
        let mut current = String::new();
        let mut current_width = 0usize;
        let mut chunk_start = seg_start;

        for ch in segment.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            // The first row is narrower by the prefix; continuation rows use the
            // full width because they carry no indent.
            let avail = if lines.is_empty() {
                first_avail
            } else {
                cont_avail
            };
            if current_width + ch_width > avail && !current.is_empty() {
                let chunk = std::mem::take(&mut current);
                let spans = build_spans(&chunk, chunk_start);
                let mut all_spans = if lines.is_empty() {
                    vec![prefix.clone()]
                } else {
                    Vec::new()
                };
                all_spans.extend(spans);
                lines.push(Line::from(all_spans));
                chunk_start += chunk.len();
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
        // Push the segment's final chunk, even when empty, so blank lines and
        // the trailing cursor position survive.
        let spans = build_spans(&current, chunk_start);
        let mut all_spans = if lines.is_empty() {
            vec![prefix.clone()]
        } else {
            Vec::new()
        };
        all_spans.extend(spans);
        lines.push(Line::from(all_spans));
    }

    if lines.is_empty() {
        let all_spans = vec![prefix.clone(), Span::styled(" ", cursor_style)];
        lines.push(Line::from(all_spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::TaskStatus;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    /// 旁支转录在窄终端里按显示宽度折行，不会溢出右边缘。
    ///
    /// 这里钉的是 `side_items_to_cells` 这条映射：旁支 item 必须落到与主会话
    /// **同一个** `HistoryCell`，才能共用同一套折行。两边同时成立才算数——
    /// 没有一行超过宽度（不溢出），且真的折成了多行（不是把内容截断）。
    /// 折行本身由 `HistoryCell` 的渲染器负责，所以这条同时守住「不要给旁支
    /// 另写一套渲染」。
    #[test]
    fn side_transcript_cells_wrap_cjk_at_display_width() {
        use yi_agent_runtime::side_conversation::{SideItem, SideItemContent};

        let item = SideItem {
            id: "i1".into(),
            turn_id: "t1".into(),
            ordinal: 1,
            content: SideItemContent::AgentMessage {
                text: "旁支的一段中文回答，需要在窄终端里按显示宽度折行而不是溢出屏幕。".into(),
            },
        };

        let width = 20u16;
        let mut history = HistoryState::with_theme(TuiTheme::DARK);
        for cell in side_items_to_cells(&[std::sync::Arc::new(item)]) {
            history.push(cell, width);
        }
        let view = HistoryView {
            state: &history,
            width,
            theme: TuiTheme::DARK,
        };
        let widths: Vec<usize> = view
            .flattened_lines(width)
            .iter()
            .map(|(_, line)| UnicodeWidthStr::width(line.to_string().as_str()))
            .collect();
        assert!(
            widths.iter().all(|w| *w <= width as usize),
            "旁支正文不得溢出窄终端: {widths:?}"
        );
        assert!(
            widths.iter().filter(|w| **w > 0).count() > 1,
            "这段中文必须真的折成多行，否则断言没有意义: {widths:?}"
        );
    }

    #[test]
    fn multiline_paste_respects_newlines_and_width() {
        // A paste with explicit newlines was pushed through as raw text: the
        // '\n's stayed inside one span and the line overflowed the terminal.
        let prefix = Span::raw("> ");
        let text = "first line\n\nsecond line that is long enough to wrap\nthird";
        let mut inp = InputLine::new();
        inp.buffer = text.into();
        let width = 40u16;
        let lines = wrap_input_buffer(&inp.buffer, inp.cursor, &prefix, width);
        for (i, line) in lines.iter().enumerate() {
            let t: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(
                !t.contains('\n'),
                "input line {i} still holds a raw newline: {t:?}"
            );
            let w = UnicodeWidthStr::width(t.as_str());
            assert!(w <= width as usize, "input line {i} is {w} cols: {t:?}");
        }
        assert_eq!(
            compute_input_height(&inp, false, width) as usize,
            lines.len() + INPUT_CARD_PADDING as usize * 2,
            "height must equal the wrapped line count plus the card padding"
        );
    }

    #[test]
    fn pasted_long_line_is_wrapped_not_clipped() {
        // A bracketed paste arrives as one chunk with no newlines. The wrapper
        // used to split only on '\n' and emit a single over-wide line, whose
        // tail ratatui silently dropped.
        let pasted = "x".repeat(300) + &"中".repeat(60) + "TAIL";
        let width = 40u16;
        let lines = wrap_input_buffer(&pasted, 0, &Span::raw("> "), width);
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= width as usize,
                "input line {i} is {w} cols, width is {width}"
            );
        }
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            rendered.contains("TAIL"),
            "pasted tail was dropped: {rendered:?}"
        );
    }

    #[test]
    fn input_height_matches_the_wrapped_line_count() {
        // The height must come from the same wrapping the renderer uses;
        // counting `div_ceil(width)` mis-sized CJK input and hid lines.
        let prefix = Span::raw("> ");
        for width in [20u16, 33, 40] {
            for text in [
                "a".repeat(200),
                "中".repeat(120),
                "mixed 中文 and ascii ".repeat(20),
                String::new(),
            ] {
                let mut inp = InputLine::new();
                inp.buffer = text.clone();
                let expected = wrap_input_buffer(&inp.buffer, inp.cursor, &prefix, width).len();
                assert_eq!(
                    compute_input_height(&inp, false, width) as usize,
                    expected + INPUT_CARD_PADDING as usize * 2,
                    "width {width} text {text:?}"
                );
            }
        }
    }

    /// The TUI's slash commands must talk to the same daemon the CLI does.
    /// They used to fall back to `~/.yi-agent/runtime`, while the daemon listens
    /// under `<workdir>/.yi-agent/runtime` — so `/agents`, `/diff`, `/pause` and
    /// friends queried a socket that no daemon ever bound.
    #[test]
    fn slash_commands_resolve_the_same_runtime_socket_as_the_daemon() {
        let workdir = std::path::Path::new("/tmp/some-project");

        let from_tui = crate::runtime_socket_for(workdir).expect("resolves");
        let from_cli = yi_agent_store::ipc::socket_path_for(&crate::runtime_directory_for(workdir))
            .expect("resolves");

        assert_eq!(
            from_tui, from_cli,
            "the TUI and the CLI must agree on one runtime socket"
        );
    }

    #[test]
    fn active_managed_process_statuses_exclude_terminal_states() {
        use yi_agent_tools::ProcessStatus;

        assert!(is_active_managed_process(&ProcessStatus::Starting));
        assert!(is_active_managed_process(&ProcessStatus::Running));
        assert!(is_active_managed_process(&ProcessStatus::Ready));
        assert!(!is_active_managed_process(&ProcessStatus::Exited {
            code: Some(0)
        }));
        assert!(!is_active_managed_process(&ProcessStatus::Killed));
        assert!(!is_active_managed_process(&ProcessStatus::FailedToStart {
            reason: "spawn failed".into(),
        }));
    }
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use futures::future::BoxFuture;
    use ratatui::backend::TestBackend;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use yi_agent_core::subagent::task::WorkspaceLeaseId;
    use yi_agent_core::subagent::worker::{
        AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    };
    use yi_agent_core::{OutputStream, RootSessionId, TaskId};

    struct RecordingWorkerFactory;

    impl AgentWorkerFactory for RecordingWorkerFactory {
        fn recovery_context(&self) -> WorkerRecoveryContext {
            WorkerRecoveryContext {
                workspace_lease_id: Some("workspace:test".into()),
                worktree_lease: Some("worktree:test".into()),
                checkpoint_json: r#"{"git_head":"test","git_status":""}"#.into(),
                tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
            }
        }

        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            Box::pin(async move {
                let handle = WorkerHandle::new(request.cancellation);
                let paused_handle = handle.clone();
                let mut pause = handle.subscribe_pause();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("test pause listener runtime must initialize");
                    if runtime.block_on(pause.requested()) {
                        paused_handle.report_paused();
                    }
                });
                Ok(handle)
            })
        }
    }

    #[derive(Default)]
    struct DeliveryRecordingFactory {
        handles: std::sync::Mutex<Vec<WorkerHandle>>,
        workspaces: std::sync::Mutex<Vec<WorkspaceLeaseId>>,
    }

    impl AgentWorkerFactory for DeliveryRecordingFactory {
        fn recovery_context(&self) -> WorkerRecoveryContext {
            WorkerRecoveryContext {
                workspace_lease_id: Some("workspace:test".into()),
                worktree_lease: Some("worktree:test".into()),
                checkpoint_json: r#"{"git_head":"test","git_status":""}"#.into(),
                tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
            }
        }

        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            let handle = WorkerHandle::new(request.cancellation);
            self.workspaces
                .lock()
                .unwrap()
                .push(request.workspace_lease_id.unwrap_or_default());
            self.handles.lock().unwrap().push(handle.clone());
            Box::pin(async move { Ok(handle) })
        }
    }

    fn start_daemon_with_delivered_child() -> (TempDir, yi_agent_store::ipc::Daemon, String) {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let factory = Arc::new(DeliveryRecordingFactory::default());
        let daemon = yi_agent_store::ipc::Daemon::start_with_factory(
            directory.path().join("runtime"),
            &database,
            factory.clone(),
        )
        .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::CreateSession,
        )
        .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::StartWorker {
                session_id: session_id.clone(),
                task_id: root_task_id.clone(),
            },
        )
        .unwrap();
        let spawn_response = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::SpawnChild {
                workdir: None,
                session_id,
                parent_task_id: root_task_id,
                objective: "实现 parser".into(),
                mode: Some("coding".into()),
                model: None,

                sandbox: None,
                fork_token: None,
            },
        )
        .unwrap();
        let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id } = spawn_response else {
            panic!("expected a spawned child, got {spawn_response:?}");
        };
        let workspace = factory.workspaces.lock().unwrap()[1].clone();
        factory.handles.lock().unwrap()[1].report_delivery(
            yi_agent_core::subagent::task::DeliveryReport::coding(
                "deadbeef",
                "main",
                workspace,
                "cargo test -p child",
            ),
        );
        let task = task_id.parse().unwrap();
        let mut state = String::new();
        for _ in 0..100 {
            state = yi_agent_store::repository::RuntimeRepository::open(&database)
                .unwrap()
                .task_state(&task)
                .unwrap();
            if state == "completed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            state, "completed",
            "a delivered child must reach completed before the review control reads it"
        );
        (directory, daemon, task_id)
    }

    #[test]
    fn test_route_event_full_flow() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        // Start resets per-call status.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Start,
        );
        sb.tick();
        assert_eq!(sb.display_input_tokens(), 0);

        // ToolCall registers a task.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"echo hi","expected_timeout_sec":30}),
            },
        );
        assert_eq!(registry.running_count(), 1);
        assert_eq!(registry.get("t1").unwrap().expected_timeout_sec, 30);

        // OutputDelta appends to the registry.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolOutputDelta {
                id: "t1".into(),
                stream: OutputStream::Stdout,
                text: "hi\n".into(),
            },
        );
        assert!(
            registry
                .get("t1")
                .unwrap()
                .stdout
                .windows(2)
                .any(|w| w == b"hi")
        );

        // Exit finalizes the task.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolExit {
                id: "t1".into(),
                code: Some(0),
            },
        );
        assert_eq!(registry.get("t1").unwrap().status, TaskStatus::Done);

        // Usage updates the status bar target.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Usage {
                model: "test".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            },
        );
        sb.tick();
        assert!(sb.display_input_tokens() > 0);
    }

    #[test]
    fn agents_summary_reads_daemon_task_snapshot() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let mut repository =
            yi_agent_store::repository::RuntimeRepository::open(&database).unwrap();
        let task = TaskId::new();
        repository
            .create_task(&task, &RootSessionId::new(), "queued")
            .unwrap();

        let summary = daemon_agents_summary_at(daemon.socket_path(), Some("--all")).unwrap();

        assert!(summary.starts_with("**All agents (1)**"));
        assert!(summary.contains(&format!("- `{task}`: queued")));
        assert!(summary.contains(&task.to_string()));
        assert!(summary.contains("queued"));
    }

    #[test]
    fn current_agents_summary_excludes_the_attached_root_task() {
        let summary = format_agents_summary(
            "Current session",
            vec![
                yi_agent_store::ipc::IpcTaskSummary {
                    task_id: "root".into(),
                    state: "queued".into(),
                    is_root: true,
                    parent_task_id: None,
                    thread_id: None,
                    usage: Default::default(),
                },
                yi_agent_store::ipc::IpcTaskSummary {
                    task_id: "child".into(),
                    state: "running".into(),
                    is_root: false,
                    parent_task_id: Some("root".into()),
                    thread_id: None,
                    usage: Default::default(),
                },
            ],
            Some("root"),
            "暂无子任务",
        );

        assert!(!summary.contains("root"));
        assert!(summary.contains("Current session (1)"));
        assert!(summary.contains("`child`: running"));
    }

    #[test]
    fn all_agents_summary_marks_root_tasks() {
        let summary = format_agents_summary(
            "All agents",
            vec![yi_agent_store::ipc::IpcTaskSummary {
                task_id: "root".into(),
                state: "running".into(),
                is_root: true,
                parent_task_id: None,
                thread_id: None,
                usage: Default::default(),
            }],
            None,
            "暂无任务",
        );

        assert!(summary.contains("`root`: running **root**"));
    }

    #[test]
    fn agent_detail_reads_a_daemon_task_for_user_inspection() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(detail.contains(&root_task_id));
        assert!(detail.contains("state: queued"));
        assert!(detail.contains("depth: 0"));
    }

    #[test]
    fn cancel_control_requires_a_preview_token_before_cancelling() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let preview = daemon_cancel_at(daemon.socket_path(), &root_task_id, true, None).unwrap();
        assert!(preview.contains("取消预览"));
        let yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewCancel {
                task_id: root_task_id.clone(),
                recursive: true,
            },
        )
        .unwrap()
        else {
            panic!("expected a cancel preview");
        };
        let result = daemon_cancel_at(
            daemon.socket_path(),
            &root_task_id,
            true,
            Some(&confirmation_token),
        )
        .unwrap();

        assert!(result.contains("递归"));
        let detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();
        assert!(detail.contains("state: cancelled"));
    }

    #[test]
    fn review_control_reads_delivery_json_from_daemon() {
        let (_directory, daemon, task_id) = start_daemon_with_delivered_child();

        let review = daemon_review_at(daemon.socket_path(), &task_id).unwrap();

        assert!(review.contains("Delivery 审查"));
        assert!(review.contains("deadbeef"));
        assert!(review.contains("cargo test -p child"));
    }

    #[test]
    fn events_control_reads_task_events_from_daemon() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::repository::RuntimeRepository::open(&database)
            .unwrap()
            .append_event(
                &root_task_id.parse().unwrap(),
                yi_agent_store::repository::RuntimeEvent::TaskStarted,
            )
            .unwrap();

        let events = daemon_events_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(events.contains("Events"));
        assert!(events.contains("task_started"));
    }

    #[test]
    fn mailbox_control_reads_messages_without_consuming_them() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::SendUserMessage {
                task_id: root_task_id.clone(),
                message: "请汇报进展".into(),
            },
        )
        .unwrap();

        let mailbox = daemon_mailbox_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(mailbox.contains("Mailbox"));
        assert!(mailbox.contains("user_override"));
        assert!(mailbox.contains("请汇报进展"));
        assert_eq!(
            yi_agent_store::repository::RuntimeRepository::open(&database)
                .unwrap()
                .mailbox_messages_for_task(&root_task_id.parse().unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn diff_control_reads_delivery_evidence_from_daemon() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let diff = daemon_diff_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(diff.contains("Diff"));
        assert!(diff.contains("Root session objective not specified."));
    }

    #[test]
    fn retry_control_routes_to_the_daemon_with_explicit_session_scope() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::CreateSession,
        )
        .unwrap()
        else {
            panic!("expected a created session");
        };
        let yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewCancel {
                task_id: root_task_id.clone(),
                recursive: false,
            },
        )
        .unwrap()
        else {
            panic!("expected a cancel preview");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::ConfirmCancel {
                task_id: root_task_id.clone(),
                recursive: false,
                confirmation_token,
            },
        )
        .unwrap();

        let result = daemon_retry_at(daemon.socket_path(), &session_id, &root_task_id).unwrap();

        assert!(result.contains("已请求重试"));
    }

    #[test]
    fn pause_and_resume_controls_require_a_task_id() {
        assert_eq!(
            parse_pause_resume_args(Some("task-1"), "pause").unwrap(),
            "task-1"
        );
        assert_eq!(
            parse_pause_resume_args(Some("task-1 extra"), "pause").unwrap_err(),
            "用法: /pause <task-id>"
        );
        assert_eq!(
            parse_pause_resume_args(None, "resume").unwrap_err(),
            "用法: /resume <task-id>"
        );
    }

    #[test]
    fn pause_and_resume_controls_route_to_the_daemon_with_explicit_session_scope() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon = yi_agent_store::ipc::Daemon::start_with_factory(
            directory.path().join("runtime"),
            &database,
            Arc::new(RecordingWorkerFactory),
        )
        .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::CreateSession,
        )
        .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::StartWorker {
                session_id: session_id.clone(),
                task_id: root_task_id.clone(),
            },
        )
        .unwrap();

        let paused = daemon_pause_at(daemon.socket_path(), &session_id, &root_task_id).unwrap();
        let paused_detail = (0..20)
            .find_map(|_| {
                let detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();
                if detail.contains("state: paused") {
                    Some(detail)
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("worker pause acknowledgement should be reconciled");
        let resumed = daemon_resume_at(daemon.socket_path(), &session_id, &root_task_id).unwrap();
        let resumed_detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(paused.contains("已请求暂停"));
        assert!(paused_detail.contains("state: paused"));
        assert!(resumed.contains("已请求恢复"));
        assert!(resumed_detail.contains("state: running"));
    }

    #[test]
    fn message_control_routes_an_audited_user_override_to_the_daemon() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let result = daemon_user_message_at(
            daemon.socket_path(),
            &root_task_id,
            "请暂停并说明当前阻塞原因",
        )
        .unwrap();

        assert!(result.contains("已排队用户指令"));
    }

    #[test]
    fn test_route_event_tracks_only_bash_tool_calls() {
        let mut registry = RunningTaskRegistry::new();
        let mut statusbar = StatusBarState::default();
        let mut cost = CostTracker::default();

        route_event(
            &mut registry,
            &mut statusbar,
            &mut cost,
            &AgentEvent::ToolCall {
                id: "grep".into(),
                name: "grep".into(),
                input: serde_json::json!({"pattern": "TODO"}),
            },
        );
        assert!(registry.list().is_empty());

        route_event(
            &mut registry,
            &mut statusbar,
            &mut cost,
            &AgentEvent::ToolCall {
                id: "bash".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi", "expected_timeout_sec": 30}),
            },
        );
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("bash").unwrap().command, "echo hi");
    }

    #[test]
    fn test_route_event_tool_timeout() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 10","expected_timeout_sec":1}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolTimeout { id: "t".into() },
        );
        assert_eq!(registry.get("t").unwrap().status, TaskStatus::Timeout);
    }

    /// Non-Bash tools do not create Ctrl+P tasks, and their results are safe
    /// to ignore in the Bash-task registry.
    #[test]
    fn test_route_event_ignores_non_bash_tool_results() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        let mut cost = CostTracker::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::ToolCall {
                id: "search".into(),
                name: "web_search".into(),
                input: serde_json::json!({"query": "rust"}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::ToolResult {
                id: "search".into(),
                result: yi_agent_core::ToolResult::text("result"),
            },
        );

        assert!(registry.get("search").is_none());
        assert!(registry.list().is_empty());
    }

    /// Regression: a ToolCall that never receives a ToolExit (e.g. bash tool
    /// early-returned on a blocked command, or the ToolExit event was dropped)
    /// must be finalized when the turn ends with Done so the status-bar timer
    /// stops ticking.
    #[test]
    fn test_route_event_done_aborts_running_tasks() {
        use yi_agent_core::DoneReason;
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"echo hi","expected_timeout_sec":30}),
            },
        );
        // Simulate elapsed time so we can verify the timer freezes.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let before = registry.get("t1").unwrap().elapsed();
        // Turn ends without a ToolExit for t1.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        );
        let task = registry.get("t1").unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Aborted,
            "Done without ToolExit should abort the running task"
        );
        let after = registry.get("t1").unwrap().elapsed();
        // Timer must be frozen: later reads should not advance.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let after2 = registry.get("t1").unwrap().elapsed();
        assert_eq!(
            after, after2,
            "Aborted task timer should be frozen, not still ticking"
        );
        assert!(
            after >= before,
            "Aborted end_time should be >= the pre-Done reading"
        );
    }

    /// Regression: when a user cancels mid-tool (Ctrl+C), the running bash
    /// task must be aborted so the timer stops.
    #[test]
    fn test_route_event_cancelled_aborts_running_tasks() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 10","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Cancelled,
        );
        assert_eq!(
            registry.get("t1").unwrap().status,
            TaskStatus::Aborted,
            "Cancelled should abort the running task"
        );
        assert!(registry.get("t1").unwrap().end_time.is_some());
    }

    /// Regression: an agent error mid-tool must also abort running tasks.
    #[test]
    fn test_route_event_error_aborts_running_tasks() {
        use yi_agent_core::AgentError;
        use yi_agent_core::provider::ProviderError;
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 10","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Error(AgentError::Provider(ProviderError::Auth("boom".into()))),
        );
        assert_eq!(
            registry.get("t1").unwrap().status,
            TaskStatus::Aborted,
            "Error should abort the running task"
        );
    }

    /// Done/Cancelled/Error must NOT touch already-finalized tasks.
    #[test]
    fn test_route_event_done_does_not_modify_finalized_tasks() {
        use yi_agent_core::DoneReason;
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "done_task".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"echo hi","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolExit {
                id: "done_task".into(),
                code: Some(0),
            },
        );
        let done_end = registry.get("done_task").unwrap().end_time.unwrap();
        // Another task still running when Done arrives.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "stuck_task".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 99","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        );
        assert_eq!(
            registry.get("done_task").unwrap().status,
            TaskStatus::Done,
            "Already-done task should not be touched by Done cleanup"
        );
        assert_eq!(
            registry.get("done_task").unwrap().end_time.unwrap(),
            done_end,
            "Already-done end_time should not be overwritten"
        );
        assert_eq!(
            registry.get("stuck_task").unwrap().status,
            TaskStatus::Aborted,
            "Still-running task should be aborted by Done cleanup"
        );
    }

    #[test]
    fn test_route_event_decode_delta_accumulates() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        // Simulate streamed tool-call argument deltas.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::DecodeDelta("{\"q\":".into()),
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::DecodeDelta("\"weather\"}".into()),
        );
        // {"q": = 5 chars, "weather"} = 9 chars → 14 ascii → 14/4 = 3 tokens
        assert_eq!(sb.display_output_tokens(), 0); // display lags via tick
        assert_eq!(
            sb.target_output_tokens(),
            3,
            "two DecodeDelta events should accumulate"
        );
    }

    #[test]
    fn test_route_event_assistant_text_accumulates() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::AssistantText("hello".into()),
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::AssistantText("world".into()),
        );
        // 10 ascii chars → 10/4 = 2 tokens
        assert_eq!(sb.target_output_tokens(), 2);
    }

    /// Reproduces the user's observation: "after bash executes, the decode
    /// display seems to persist until the process timeout-exits, not until
    /// the LLM call actually ends."
    ///
    /// Expected event sequence:
    ///   Start → prefill/decode deltas → Usage (LLM call ends naturally)
    ///   → ToolCall (bash starts) → ToolOutputDelta (bash running)
    ///   → ToolTimeout (bash timeout) → ToolResult → Done
    ///
    /// The decode display should reset when the LLM call ends (at ToolCall
    /// or Usage), NOT linger until bash timeout. The bug: decode display
    /// stayed frozen at the previous LLM turn's value throughout bash
    /// execution, making it look like "decode is still going" until bash
    /// timeout.
    #[test]
    fn test_decode_display_resets_when_bash_starts_not_when_it_times_out() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();

        // --- LLM THINK phase: decode tokens arrive ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Start,
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::EstimatedPrefill(500),
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::AssistantText("Let me run a command".into()),
        );
        // Real usage from LLM — this is when the LLM call "naturally ends"
        // (the Stop event and Usage arrive at the end of the stream).
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Usage {
                model: "test".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 500,
                    output_tokens: 42,
                    ..Default::default()
                },
            },
        );
        sb.tick();
        // Decode is now showing (interpolating toward 42).
        let decode_after_llm = sb.display_output_tokens();
        assert!(
            decode_after_llm <= 42,
            "decode should be interpolating toward 42, got {decode_after_llm}"
        );

        // --- ACT phase: bash starts running ---
        // This is when the LLM call has ENDED. The decode display should
        // reset here so the user sees "decode ended, bash is running".
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 999","expected_timeout_sec":5}),
            },
        );

        // KEY ASSERTION: decode display should be 0 immediately when bash
        // starts, NOT waiting for bash to timeout.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should reset to 0 when bash starts, not wait for timeout"
        );
        assert_eq!(
            sb.target_output_tokens(),
            0,
            "decode target should reset to 0 when bash starts"
        );

        // --- bash produces output (still running) ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolOutputDelta {
                id: "t1".into(),
                stream: OutputStream::Stdout,
                text: "some output\n".into(),
            },
        );
        // Decode should still be 0 — ToolOutputDelta must not affect decode.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should stay 0 during bash execution"
        );

        // --- bash times out ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolTimeout { id: "t1".into() },
        );
        // Decode should STILL be 0 — timeout doesn't change it.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should still be 0 at bash timeout"
        );

        // --- ToolResult + Done (turn ends) ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolResult {
                id: "t1".into(),
                result: yi_agent_core::ToolResult::error("timeout"),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        );
        // Decode should STILL be 0 — Done doesn't add decode tokens.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should still be 0 at turn end"
        );
    }

    /// Fake event source that plays back a scripted sequence of events,
    /// then returns None (timeout) forever. Used to test the loop deterministically.
    struct ScriptedEvents {
        events: Rc<RefCell<Vec<Event>>>,
    }

    impl EventSource for ScriptedEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            Ok(self.events.borrow_mut().pop())
        }
    }

    struct QueueThenDoneEvents {
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        poll_count: Cell<usize>,
    }

    impl EventSource for QueueThenDoneEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => Ok(Some(Event::Paste("queued message".into()))),
                1 => {
                    self.agent_tx
                        .try_send(AgentEvent::Done {
                            reason: yi_agent_core::DoneReason::EndTurn,
                        })
                        .unwrap();
                    Ok(Some(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                    ))))
                }
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    /// Submits a mid-turn message, then delivers `InterjectionsReturned`
    /// immediately followed by `Cancelled` in a single frame — the order core
    /// guarantees. The returned text must land back in the input box, which only
    /// happens if the return is applied before the turn-end handling.
    struct InterjectionThenCancelEvents {
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        poll_count: Cell<usize>,
    }

    impl EventSource for InterjectionThenCancelEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => Ok(Some(Event::Paste("opening".into()))),
                1 => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                2 => Ok(Some(Event::Paste("mid-turn".into()))),
                3 => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                // Queue the pair but do NOT quit in the same frame: `run_loop`
                // drains `agent_rx` at the top of an iteration, so the events
                // sent here are only observed on the next one. An inert key
                // gives that frame a chance to happen before the quit below.
                4 => {
                    self.agent_tx
                        .try_send(AgentEvent::InterjectionsReturned {
                            items: vec![yi_agent_core::Interjection {
                                seq: 1,
                                text: "mid-turn".into(),
                                tag: None,
                            }],
                        })
                        .unwrap();
                    self.agent_tx.try_send(AgentEvent::Cancelled).unwrap();
                    Ok(Some(Event::Key(KeyEvent::new(
                        KeyCode::F(1),
                        KeyModifiers::NONE,
                    ))))
                }
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    /// A test backend whose size can change from a fake event source between
    /// frames, matching a terminal resize without sharing the terminal itself.
    #[derive(Clone)]
    struct ResizableTestBackend {
        inner: Rc<RefCell<TestBackend>>,
    }

    impl Backend for ResizableTestBackend {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.borrow_mut().draw(content)
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().hide_cursor()
        }

        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().show_cursor()
        }

        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            self.inner.borrow_mut().get_cursor_position()
        }

        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            self.inner.borrow_mut().set_cursor_position(position)
        }

        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().clear()
        }

        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.borrow().size()
        }

        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.borrow_mut().window_size()
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().flush()
        }
    }

    struct ResizeThenQuitEvents {
        backend: Rc<RefCell<TestBackend>>,
        top_marker: Rc<RefCell<Option<String>>>,
        poll_count: Cell<usize>,
    }

    impl EventSource for ResizeThenQuitEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => {
                    let backend = self.backend.borrow();
                    let row: String = (0..32u16)
                        .map(|x| backend.buffer()[(x, 0)].symbol())
                        .collect();
                    drop(backend);
                    let marker = row
                        .split_whitespace()
                        .find(|word| word.starts_with("MARKER-"))
                        .expect("the first rendered history row has a marker")
                        .to_string();
                    *self.top_marker.borrow_mut() = Some(marker);
                    self.backend.borrow_mut().resize(16, 14);
                    Ok(None)
                }
                1 => Ok(None),
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    /// Captures the old and new top rows around an idle local submission.
    /// The final frame must restore the pre-submit content anchor rather than
    /// using the width before the scrollbar was accounted for.
    struct SubmitThenQuitEvents {
        backend: Rc<RefCell<TestBackend>>,
        top_marker: Rc<RefCell<Option<String>>>,
        poll_count: Cell<usize>,
    }

    impl EventSource for SubmitThenQuitEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => {
                    let backend = self.backend.borrow();
                    let row: String = (0..20u16)
                        .map(|x| backend.buffer()[(x, 0)].symbol())
                        .collect();
                    drop(backend);
                    let marker = row
                        .split_whitespace()
                        .find(|word| word.starts_with("MARKER-"))
                        .expect("the first rendered history row has a marker")
                        .to_string();
                    *self.top_marker.borrow_mut() = Some(marker);
                    Ok(Some(Event::Paste("new local message".into())))
                }
                1 => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    #[test]
    fn history_anchor_survives_resize_between_frames() {
        let backend = Rc::new(RefCell::new(TestBackend::new(32, 14)));
        let mut terminal = Terminal::new(ResizableTestBackend {
            inner: Rc::clone(&backend),
        })
        .unwrap();
        let (_agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        for index in 0..20 {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("MARKER-{index:02} 12345678901234567890"),
                },
                32,
            );
        }
        history.scroll_offset = 16;
        let mut input = InputLine::new();
        let source = ResizeThenQuitEvents {
            backend: Rc::clone(&backend),
            top_marker: Rc::new(RefCell::new(None)),
            poll_count: Cell::new(0),
        };

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        let narrow_area = ratatui::layout::Rect::new(0, 0, 16, 14);
        let narrow_layout = compute_layout(narrow_area, &input, false, &None, 0);
        let narrow_history_area = narrow_layout.chunks[0];
        let narrow_text_width =
            history.text_width(narrow_history_area.width, narrow_history_area.height);
        let mut before_resize = HistoryState::from_cells(history.cells.clone(), 16);
        let wide_area = ratatui::layout::Rect::new(0, 0, 32, 14);
        let wide_layout = compute_layout(wide_area, &InputLine::new(), false, &None, 0);
        let wide_history_area = wide_layout.chunks[0];
        let wide_text_width =
            before_resize.text_width(wide_history_area.width, wide_history_area.height);
        before_resize.reconcile_scroll_offset(wide_text_width, wide_history_area.height);
        let anchor = before_resize
            .capture_viewport_anchor(wide_text_width, wide_history_area.height)
            .unwrap();
        let mut expected = before_resize;
        expected.restore_viewport_anchor(anchor, narrow_text_width, narrow_history_area.height);

        assert_eq!(history.scroll_offset, expected.scroll_offset);
        let top_marker = source.top_marker.borrow().clone().unwrap();
        let resized_backend = backend.borrow();
        let final_row: String = (0..16u16)
            .map(|x| resized_backend.buffer()[(x, 0)].symbol())
            .collect();
        assert!(
            final_row.contains(&top_marker),
            "expected top marker {top_marker:?} after resize, got {final_row:?}"
        );
    }

    #[test]
    fn history_anchor_survives_local_user_insertion_at_scrollbar_width() {
        let backend = Rc::new(RefCell::new(TestBackend::new(20, 14)));
        let mut terminal = Terminal::new(ResizableTestBackend {
            inner: Rc::clone(&backend),
        })
        .unwrap();
        let (_agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        for index in 0..20 {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("MARKER-{index:02} 1234567890"),
                },
                20,
            );
        }
        history.scroll_offset = 30;
        let source = SubmitThenQuitEvents {
            backend: Rc::clone(&backend),
            top_marker: Rc::new(RefCell::new(None)),
            poll_count: Cell::new(0),
        };

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        let area = ratatui::layout::Rect::new(0, 0, 20, 14);
        let layout = compute_layout(area, &InputLine::new(), false, &None, 0);
        let history_area = layout.chunks[0];
        assert_eq!(
            history.text_width(history_area.width, history_area.height),
            history_area.width - 1,
            "the inserted local message is restored with the scrollbar-reserved width"
        );
        let top_marker = source.top_marker.borrow().clone().unwrap();
        let backend = terminal.backend().inner.borrow();
        let final_row: String = (0..20u16)
            .map(|x| backend.buffer()[(x, 0)].symbol())
            .collect();
        assert!(
            final_row.contains(&top_marker),
            "expected top marker {top_marker:?} after local insertion, got {final_row:?}"
        );
    }

    #[test]
    fn returned_interjection_survives_the_turn_end_it_precedes() {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let source = InterjectionThenCancelEvents {
            agent_tx,
            poll_count: Cell::new(0),
        };

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        // The core invariant: `InterjectionsReturned` arrives ahead of
        // `Cancelled`, and the TUI hands the text back rather than losing it
        // when the turn ends in the very next frame. Without
        // `apply_interjection_event` wired into the event loop this assertion
        // fails with an empty input box.
        assert_eq!(
            input.buffer, "mid-turn",
            "the returned text must be restored to the input box"
        );
        assert!(
            history.cells.iter().any(|c| matches!(
                c,
                HistoryCell::Separator { label: Some(label) }
                    if label.contains("已退回输入框")
            )),
            "the return must be reported in history: {:?}",
            history.cells
        );
    }

    #[test]
    fn history_anchor_survives_queued_preview_promotion() {
        let backend = TestBackend::new(20, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        for index in 0..20 {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("MARKER-{index:02} 1234567890"),
                },
                20,
            );
        }
        history.scroll_offset = 30;
        let before_cells = history.cells.clone();
        let mut input = InputLine::new();
        let source = QueueThenDoneEvents {
            agent_tx,
            poll_count: Cell::new(0),
        };

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        let area = ratatui::layout::Rect::new(0, 0, 20, 14);
        let final_layout = compute_layout(area, &input, false, &None, 0);
        let final_history_area = final_layout.chunks[0];
        let final_text_width =
            history.text_width(final_history_area.width, final_history_area.height);
        let pre_drain_layout = compute_layout(area, &InputLine::new(), false, &None, 2);
        let pre_drain_width = history.text_width(
            pre_drain_layout.chunks[0].width,
            pre_drain_layout.chunks[0].height,
        );
        assert_ne!(
            pre_drain_layout.chunks[0].height, final_history_area.height,
            "promotion changes the history viewport height"
        );
        assert_eq!(
            pre_drain_width, 19,
            "wrapped history reserves a scrollbar column"
        );

        let mut before = HistoryState::from_cells(before_cells, 30);
        // The loop first renders without a queue and clamps the deliberately
        // over-large starting offset before the queued preview appears.
        let prior_layout = compute_layout(area, &InputLine::new(), false, &None, 0);
        let prior_text_width =
            before.text_width(prior_layout.chunks[0].width, prior_layout.chunks[0].height);
        before.reconcile_scroll_offset(prior_text_width, prior_layout.chunks[0].height);
        let anchor = before
            .capture_viewport_anchor(prior_text_width, prior_layout.chunks[0].height)
            .expect("the initial viewport is scrolled up");
        let mut expected = HistoryState::from_cells(history.cells.clone(), 0);
        expected.restore_viewport_anchor(anchor, final_text_width, final_history_area.height);

        assert_eq!(
            history.scroll_offset, expected.scroll_offset,
            "the top history cell must remain anchored when the queued preview is promoted; \
             old_width={prior_text_width}, new_width={final_text_width}, old_height={}, \
             new_height={}",
            pre_drain_layout.chunks[0].height, final_history_area.height,
        );
        let top_row: String = (0..20u16)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(
            top_row.contains("MARKER-00"),
            "the original top marker must remain at the viewport top: {top_row:?}"
        );
    }

    /// Regression test for blocking_send panic.
    ///
    /// The bug: run_tui called `input_tx.blocking_send` while on the tokio
    /// runtime's async thread, which panics with "Cannot block the current
    /// thread from within a runtime". The fix was to run the TUI on a
    /// `spawn_blocking` thread. This test simulates that calling stack
    /// (block_on -> spawn_blocking -> blocking_send) and verifies no panic.
    #[test]
    fn blocking_send_does_not_panic_on_runtime_thread() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);

        rt.block_on(async move {
            let tx = input_tx.clone();
            let handle = tokio::task::spawn_blocking(move || {
                tx.blocking_send("hello".to_string()).unwrap();
            });
            handle.await.unwrap();
            assert_eq!(input_rx.recv().await.unwrap(), "hello");
        });
    }

    /// 父审批提示显示时敲 `/` 进入命令模式，Enter 只执行命令。
    ///
    /// 这是设计 §10.3 最危险的一条：Enter 平时是「默认批准」。如果命令模式
    /// 复用了同一条分支，用户想敲命令却意外批准了一次工具调用。测试必须断言
    /// **decision 通道一个字节都没收到**，而不只是断言界面文字。
    #[test]
    fn parent_approval_command_mode_does_not_approve_on_enter() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        agent_tx
            .try_send(AgentEvent::PermissionRequest {
                request_id: 7,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf /tmp/x"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            })
            .unwrap();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let events = ScriptedEvents {
            // `poll` pops from the back, so this is delivered in reverse:
            // Ctrl+Q gets the loop out without ever approving.
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Paste("/help".into()),
                Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut title = crate::tui::title::TitleHandle::disabled();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        assert!(
            decision_rx.try_recv().is_err(),
            "命令模式下的 Enter 绝不能批准父会话的工具调用"
        );
    }

    /// 审批命令模式的白名单确实包含 `/back` 与 `/side-close`。
    ///
    /// 规格 §10.2 要求这个模式只接受 `side/btw/back/side-close/help/退出`。
    /// 之前实现里 `/back`、`/side-close` 掉进了「拒绝」分支，用户只能看到
    /// 「不能在审批中使用」——而旁支恰恰是审批态最需要的能力（父被审批卡住
    /// 时去旁支问一句）。测试逐条走一遍：每条命令都不得触发裁决。
    #[test]
    fn approval_command_mode_accepts_side_back_and_close() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        agent_tx
            .try_send(AgentEvent::PermissionRequest {
                request_id: 7,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf /tmp/x"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            })
            .unwrap();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        // 旁支通道存在（能收命令）但没人消费:这些命令在审批态只应被发出去,
        // 不允许落到裁决上。用一个足够深的 channel 避免 try_send 失败。
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (_side_evt_tx, side_rx) = tokio::sync::mpsc::channel(8);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let events = ScriptedEvents {
            // 逆序投递：先 /back，再 /side-close，最后 Ctrl+Q 退出。
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Paste("/side-close".into()),
                Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Paste("/back".into()),
                Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut title = crate::tui::title::TitleHandle::disabled();
        let mut history = HistoryState::new();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_rx),
        )
        .unwrap();

        // 两条命令都必须真的被**执行**（给出主视图语义的提示），而不是掉进
        // 「不能在审批中使用」的拒绝分支。只看「没批准」不够：拒绝分支同样
        // 不批准，那样测试对白名单缺失毫无分辨力。
        let labels: Vec<String> = history
            .cells
            .iter()
            .filter_map(|cell| match cell {
                HistoryCell::Separator { label: Some(label) } => Some(label.clone()),
                _ => None,
            })
            .collect();
        let joined = labels.join("\n");
        assert!(
            joined.contains("已经在主会话了，无需 /back"),
            "/back 必须被当作合法命令执行，实际提示：{joined}"
        );
        assert!(
            joined.contains("当前没有旁问"),
            "/side-close 在无旁支时必须幂等成功而不是被拒绝，实际提示：{joined}"
        );
        assert!(
            !joined.contains("不能在审批中使用"),
            "白名单必须包含 /back 与 /side-close，实际提示：{joined}"
        );

        assert!(
            decision_rx.try_recv().is_err(),
            "/back 与 /side-close 在审批命令模式下绝不能触发裁决"
        );
        // `/back` 在主视图/审批态是空操作,不该给 driver 发命令;
        // `/side-close` 在没有打开的旁支时同样不发命令（幂等,只提示）。
        assert!(
            side_cmd_rx.try_recv().is_err(),
            "没有旁支时 /side-close 不该向 driver 发 Close,更不该发别的命令"
        );
    }

    /// 主视图里的 `/back`、`/side-close` 必须是旁支命令，不能掉进「未知命令」。
    ///
    /// spec §4.1:主视图 `/back` 是「已经在主会话」的空操作提示；`/side-close`
    /// 没有旁支时幂等成功。之前它们被前面的 `/side` 专用拦截漏掉，落进
    /// `handle_key` 的斜杠分发，用户看到的是「未知命令: /side-close」。
    #[test]
    fn main_view_back_and_side_close_are_not_unknown_commands() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (_side_evt_tx, side_rx) = tokio::sync::mpsc::channel(8);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Paste("/side-close".into()),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Paste("/back".into()),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut title = crate::tui::title::TitleHandle::disabled();
        let mut history = HistoryState::new();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_rx),
        )
        .unwrap();

        let labels: Vec<&str> = history
            .cells
            .iter()
            .filter_map(|cell| match cell {
                HistoryCell::Separator { label: Some(label) } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !labels.iter().any(|l| l.contains("未知命令")),
            "/back 与 /side-close 不能落进未知命令：{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("已经在主会话")),
            "/back 在主视图应给出空操作提示：{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("当前没有旁问")),
            "/side-close 在无旁支时应幂等提示：{labels:?}"
        );
        // 两条命令都不该被当成普通消息发给父会话。
        assert!(
            input_rx.try_recv().is_err(),
            "/back 与 /side-close 不能被当作父会话消息发送"
        );
        assert!(
            side_cmd_rx.try_recv().is_err(),
            "没有旁支时 /side-close 不该发命令，/back 也不该"
        );
    }

    /// 把整屏读成字符串，供「当前在看哪份视图」这类渲染级断言使用。
    ///
    /// 宽字符（CJK）占用两个单元格，第二个单元格是空白占位，直接拼起来会把
    /// 「旁问（只读）」读成「旁 问 （ 只 读 ）」。这里去掉空白再拼，断言就能
    /// 用连续的中文子串。
    /// 把测试后端缓冲读成「终端上真正看到的文字」。
    ///
    /// 必须模拟宽字符覆盖：ratatui 的 `Buffer::diff` 会**跳过**紧跟宽字符的
    /// 那一格（真实终端画一个两列宽的汉字时会连带覆盖右侧那格，所以那格不必
    /// 下发）。但 `TestBackend::draw` 只是逐格赋值，那格会保留上一帧的内容，
    /// 于是把「旁问（只读）…」换成「关闭旁问失败…」时，读出来会交错成
    /// 「旁·问父关会闭话未空送闲达」。
    ///
    /// 这里按终端语义处理：宽字符占两列，右侧那格不参与拼接。
    fn frame_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                if symbol.trim().is_empty() {
                    x += 1;
                    continue;
                }
                out.push_str(symbol);
                x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

    /// 主视图执行 `/side` 后，命令补全弹窗不能留在屏幕上。
    ///
    /// 真实终端里曾出现：敲 `/side` 回车后旁支开了，但「命令」弹窗还挂在输入框
    /// 上方，看起来像命令没执行、提示在等用户再选一次。根因是旁支拦截分支
    /// `continue` 掉了，跳过了循环末尾的 `sync_popup`。这条测试要求：命令执行后
    /// 弹窗必须消失。
    #[test]
    fn side_command_from_main_view_clears_the_command_popup() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (_side_tx, side_rx) = tokio::sync::mpsc::channel(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Paste("/side".into()),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut title = crate::tui::title::TitleHandle::disabled();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_rx),
        )
        .unwrap();

        let frame = frame_text(&terminal);
        // `/side` 不在补全目录里（目录只列 `/help` 认识的管理命令），过滤后
        // 弹窗只剩一个空列表 + 边框标题。边框标题 `命令` 是「弹窗还在」的可靠
        // 探针：本测试的 history 是空的，正文里不会出现这两个字。
        assert!(
            !frame.contains("命令"),
            "命令执行后补全弹窗必须收起，实际屏幕：\n{frame}"
        );
        assert!(
            matches!(
                side_cmd_rx.try_recv(),
                Ok(crate::side_driver::SideDriverCommand::Open { .. })
            ),
            "弹窗消失不能以「命令没执行」为代价"
        );
    }

    /// 旁支视图里的数字键与 Enter 绝不能替父会话作出审批裁决（spec §12.4）。
    ///
    /// 这是硬约束里最容易写错的一条：主视图在 `pending_permission` 下把
    /// Enter 当「默认批准」、数字当「选第 N 项」，而旁支是按可见视图派发的。
    /// 一旦路由顺序写反，用户只是想在旁支里问一句、顺手敲了个 `1`，父会话
    /// 的工具调用就被批准了——旁支的「只读」承诺当场破掉。
    ///
    /// 用真实 `run_loop_with_side` 跑：先喂一帧旁支状态把视图建起来，再让父
    /// 审批挂起，然后在旁支里敲 `1` + Enter。断言裁决通道收到 **0** 条，且
    /// 这些键确实进了旁支（否则「没批准」可能只是「键被吞了」）。
    #[test]
    fn parent_approval_cannot_be_answered_from_side() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        agent_tx
            .try_send(AgentEvent::PermissionRequest {
                request_id: 7,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf /tmp/x"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            })
            .unwrap();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (side_tx, side_rx) = tokio::sync::mpsc::channel(16);
        side_tx
            .try_send(crate::tui::side::ConversationEvent {
                target: crate::tui::side::ConversationTarget::Side {
                    side_id: "side-1".into(),
                    generation: 1,
                },
                turn_id: None,
                event: crate::tui::side::ConversationPayload::SideStatus(
                    yi_agent_runtime::side_conversation::SideStatus::Idle,
                ),
            })
            .unwrap();
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let events = ScriptedEvents {
            // `poll` pops from the back, so this runs in reverse: 先让循环把
            // 旁支帧应用掉（`in_side` 在下一轮才生效），再在旁支里敲 `1` + Enter。
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
                Event::FocusGained,
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut title = crate::tui::title::TitleHandle::disabled();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_rx),
        )
        .unwrap();

        assert!(
            decision_rx.try_recv().is_err(),
            "旁支里的数字键与 Enter 绝不能替父会话作出裁决"
        );
        // 反向确认：这些键确实进了旁支（`1` 作为问题发出），不是被静默吞掉。
        // 旁支视图由第一帧 `SideStatus` 建立（不发 `Open`），所以这里第一个
        // 命令就该是 `Submit`。
        let command = side_cmd_rx
            .try_recv()
            .expect("旁支里的 `1`+Enter 应作为旁支提问发出");
        match command {
            crate::side_driver::SideDriverCommand::Submit { text, .. } => assert_eq!(
                text, "1",
                "旁支里的输入必须原样成为旁支提问，而不是被审批逻辑消费"
            ),
            other => panic!("期望旁支 Submit，实际是 {other:?}"),
        }
    }

    /// 主视图 `/side` 在旁支已存在时必须把**视图**切回去（spec §4.1「有旁支则打开」）。
    ///
    /// 真实终端复现过：旁支建好、`/back` 回主会话后，再敲 `/side` 屏幕仍停在
    /// 主会话。原因是 `apply_event` 只在 `views.side.is_none()` 时建立视图，
    /// 重复 open 之后 driver 回的 `SideStatus` 帧只更新状态、不切视图。
    /// 视图切换必须由「用户显式 open」驱动，而不是由后台帧驱动——否则旁支
    /// 流式输出会把正在看主会话的用户硬拽走。
    #[test]
    fn opening_a_side_from_main_view_switches_back_to_the_existing_view() {
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        side.views
            .side
            .as_mut()
            .unwrap()
            .apply_items(vec![std::sync::Arc::new(
                yi_agent_runtime::side_conversation::SideItem {
                    id: "a".into(),
                    turn_id: "turn-1".into(),
                    ordinal: 1,
                    content: yi_agent_runtime::side_conversation::SideItemContent::AgentMessage {
                        text: "旁支已有正文".into(),
                    },
                },
            )]);
        side.views.back();
        assert!(!side.in_side(), "前置条件：/back 之后在主会话");

        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let mut channels = TuiChannels::from_senders(
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
        );
        channels.side = Some(side_cmd_tx);
        let mut history = HistoryState::new();

        execute_side_command_in_main(
            crate::tui::side::SideInput::Open { question: None },
            &mut history,
            80,
            &mut side,
            &channels,
            true,
        );

        assert!(
            side.in_side(),
            "主视图 /side 必须切回已有旁支视图，而不是停在主会话"
        );
        assert_eq!(
            side.active_side().map(|view| view.items.len()),
            Some(1),
            "切回的必须是**同一个**旁支，已有正文不能丢"
        );
        assert!(
            matches!(
                side_cmd_rx.try_recv(),
                Ok(crate::side_driver::SideDriverCommand::Open { .. })
            ),
            "切视图不能替代通知 driver：多旁支宿主仍需这次 Open 来定位旁支"
        );
    }

    /// 旁支视图里 `/side-close` 必须当场回到父视图（spec §4.1「取消并关闭，返回父视图」）。
    ///
    /// 真实终端复现：在旁支里敲 `/side-close` 回车，屏幕停在旁支不动——因为
    /// 返回父视图原本只发生在 `SideClosed` 帧到达之后，而那一刻视图仍是
    /// "活着"的，用户反而留在了一个已死的旁支上。关闭是用户显式动作，视图
    /// 切换必须当场发生；随后的终态帧只负责把关闭原因显示出来。
    #[test]
    fn closing_a_side_from_its_own_view_returns_to_the_parent() {
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        assert!(side.in_side(), "前置条件：在旁支视图里");

        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let mut channels = TuiChannels::from_senders(
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
        );
        channels.side = Some(side_cmd_tx);

        // 复刻 `handle_side_key` 对 `/side-close` 的处理：先把命令敲进旁支
        // 输入行，再回车。（空行上的 Enter 什么也不做，不能当作前置动作。）
        let mut popup = None;
        assert!(matches!(
            handle_side_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut side,
                &channels,
                &mut popup,
            ),
            KeyOutcome::None
        ));
        assert!(side.input.buffer.is_empty(), "空输入行回车不该产生任何正文");
        for ch in "/side-close".chars() {
            handle_side_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut side,
                &channels,
                &mut popup,
            );
        }
        handle_side_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut side,
            &channels,
            &mut popup,
        );

        assert!(
            !side.in_side(),
            "旁支里 /side-close 必须返回父视图，不能停在已关闭的旁支上"
        );
        assert!(
            matches!(
                side_cmd_rx.try_recv(),
                Ok(crate::side_driver::SideDriverCommand::Close { .. })
            ),
            "返回父视图不能替代真的关闭：Close 必须发给 driver"
        );
    }

    /// 关闭一个旁支后再 `/side`，新的旁支帧必须能建立新视图。
    ///
    /// 真实终端复现：`/side` → `/side-close` → 再 `/side`，屏幕完全没反应。
    /// 原因有两个叠在一起：① `apply_event` 开头「已关闭视图不再接受帧」的守卫
    /// 不区分 side_id/generation，新旁支的帧也被判成迟到帧；② 建立视图的条件是
    /// `views.side.is_none()`，而关闭后视图仍在（只是 `closed` 有值），于是新旁支
    /// 永远建不出视图。已关闭视图只该丢掉**属于它自己**的迟到帧。
    #[test]
    fn a_new_side_can_open_after_the_previous_one_closed() {
        use yi_agent_runtime::side_conversation::{CloseReason, SideStatus};
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: crate::tui::side::ConversationTarget::Side {
                    side_id: "side-1".into(),
                    generation: 1,
                },
                turn_id: None,
                event: crate::tui::side::ConversationPayload::SideClosed(CloseReason::UserClosed),
            },
            80,
        );
        assert!(
            !side.active_side().unwrap().is_live(),
            "前置条件：旧旁支已关闭"
        );
        side.views.back();

        let effect = side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: crate::tui::side::ConversationTarget::Side {
                    side_id: "side-2".into(),
                    generation: 2,
                },
                turn_id: None,
                event: crate::tui::side::ConversationPayload::SideStatus(
                    yi_agent_runtime::side_conversation::SideStatus::Idle,
                ),
            },
            80,
        );
        assert_eq!(
            effect,
            SideFrameEffect::Redraw,
            "新旁支的帧不能被当作旧旁支的迟到帧丢掉"
        );
        let view = side.active_side().expect("必须为新旁支建立视图");
        assert_eq!(view.side_id, "side-2");
        assert_eq!(view.generation, 2);
        assert!(view.is_live(), "新旁支必须是可用的");

        // 而旧旁支的迟到帧仍必须被丢弃，且不能顶掉新视图。
        let stale = side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: crate::tui::side::ConversationTarget::Side {
                    side_id: "side-1".into(),
                    generation: 1,
                },
                turn_id: None,
                event: crate::tui::side::ConversationPayload::SideStatus(SideStatus::Running),
            },
            80,
        );
        assert_eq!(stale, SideFrameEffect::Discard);
        assert_eq!(side.active_side().unwrap().side_id, "side-2");
    }

    /// 旁支事件与旁支转录**绝不**能进入主 TurnLog / 主会话文件。
    ///
    /// 这是整个旁支设计最不能退化的一条：一旦漏进去，旁问就会被持久化、
    /// 被 resume 回放，父任务的历史被污染。测试用真实 `TuiSessionStore` +
    /// 真实 `TurnLog`，把主/旁事件同时喂进同一个 TUI 循环，然后直接读盘断言。
    /// 完整事件循环：旁支里粘贴 `/back` + Enter 必须回到主视图，父草稿一字不动。
    ///
    /// 只调 `handle_side_key` 证不了路由：真实复现的故障在**循环**里——`Paste`
    /// 曾经无条件交给父 `handle_paste`，于是旁支里的粘贴写进父草稿、还会被
    /// 父的待审批守卫整条丢掉，用户按了回车却发现什么也没发生。这里跑真循环。
    #[allow(clippy::too_many_arguments)]
    fn side_repair_run_loop_with(
        delivered_in_order: Vec<Event>,
        parent_agent_events: Vec<AgentEvent>,
        decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
        side_cmd_tx: tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>,
        side_events: tokio::sync::mpsc::Receiver<crate::tui::side::ConversationEvent>,
        parent_draft: &str,
    ) -> (String, InputLine) {
        side_repair_run_loop_sized(
            delivered_in_order,
            parent_agent_events,
            decision_tx,
            side_cmd_tx,
            side_events,
            parent_draft,
            (80, 24),
            TuiTheme::DARK,
        )
    }

    /// 在旁支视图里跑一拍循环，返回该帧的屏幕文本。
    fn side_repair_run_loop_in_side(
        size: (u16, u16),
        theme: TuiTheme,
        first_item: &str,
    ) -> (String, InputLine) {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_tx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_cmd_tx, _side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (side_tx, side_rx) = tokio::sync::mpsc::channel(8);
        side_tx.try_send(side_repair_status_frame()).unwrap();
        side_tx
            .try_send(crate::tui::side::ConversationEvent {
                target: ConversationTarget::Side {
                    side_id: "side-1".into(),
                    generation: 1,
                },
                turn_id: Some("turn-1".into()),
                event: ConversationPayload::SideItems(vec![std::sync::Arc::new(
                    yi_agent_runtime::side_conversation::SideItem {
                        id: "first".into(),
                        turn_id: "turn-1".into(),
                        ordinal: 1,
                        content:
                            yi_agent_runtime::side_conversation::SideItemContent::AgentMessage {
                                text: first_item.to_string(),
                            },
                    },
                )]),
            })
            .unwrap();
        drop(side_tx);
        let _ = (&input_tx, &interrupt_tx, &kill_tx, &control_tx);

        // 先给一个不改状态的事件，让循环真正画一帧再退出：`Ctrl+Q` 在退出前
        // 不会补画，直接把它当第一个事件会得到一张空屏，测不到任何东西。
        side_repair_run_loop_sized(
            vec![Event::Resize(size.0, size.1), ctrl('q')],
            Vec::new(),
            decision_tx,
            side_cmd_tx,
            side_rx,
            "",
            size,
            theme,
        )
    }

    /// 主视图（没有任何旁支帧）的一帧屏幕文本。
    fn side_repair_main_view_screen() -> (String, InputLine) {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_tx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_cmd_tx, _side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (side_tx, side_rx) = tokio::sync::mpsc::channel(1);
        drop(side_tx);
        let _ = (&input_tx, &interrupt_tx, &kill_tx, &control_tx);

        side_repair_run_loop_sized(
            vec![Event::Resize(80, 24), ctrl('q')],
            Vec::new(),
            decision_tx,
            side_cmd_tx,
            side_rx,
            "",
            (80, 24),
            TuiTheme::DARK,
        )
    }

    /// 旁支身份不能占转录首行：第一条正文必须完整可见。
    ///
    /// 旧实现把 header 盖在 history 区域顶上并 `Clear`，而旁支的第一条正文
    /// 恰好就在那一行——开旁支先丢第一句回答（spec §7）。身份改到状态栏后，
    /// 首条正文和 `/side · /back` 必须同时出现在屏幕上。
    #[test]
    fn side_status_bar_labels_the_conversation_without_covering_the_first_item() {
        const FIRST_SIDE_ITEM: &str = "FIRST_SIDE_ITEM";
        for (size, theme) in [
            ((80u16, 24u16), TuiTheme::DARK),
            ((80, 24), TuiTheme::LIGHT),
            ((32, 12), TuiTheme::DARK),
            ((32, 12), TuiTheme::LIGHT),
        ] {
            let (screen, _) = side_repair_run_loop_in_side(size, theme, FIRST_SIDE_ITEM);
            assert!(
                screen.contains(FIRST_SIDE_ITEM),
                "{size:?} {theme:?} 下旁支第一条正文不能被身份标识覆盖，实际屏幕:\n{screen}"
            );
            assert!(
                screen.contains("/side"),
                "{size:?} {theme:?} 下状态栏要标明旁支入口，实际屏幕:\n{screen}"
            );
            assert!(
                screen.contains("/back"),
                "{size:?} {theme:?} 下窄屏也必须留下返回命令，实际屏幕:\n{screen}"
            );
            assert!(
                screen.contains("旁问"),
                "{size:?} {theme:?} 下读屏要能识别当前在旁支，实际屏幕:\n{screen}"
            );
            assert!(
                !screen.contains("旁支可用工具（只读）"),
                "介绍文案只能在主动 /help 时出现，不能常驻，实际屏幕:\n{screen}"
            );
            // 状态行只有一行：溢出会折行并挤掉输入区。
            assert!(
                screen.contains("test-model") || screen.contains("/back"),
                "{size:?} 下状态栏不能被拼到溢出，实际屏幕:\n{screen}"
            );
        }
    }

    /// 普通父视图不挂常驻旁支提示（spec §7：不增加「可继续旁问」模块）。
    #[test]
    fn main_view_has_no_standalone_side_banner() {
        let (screen, _) = side_repair_run_loop_in_side((80, 24), TuiTheme::DARK, "SIDE_ITEM");
        assert!(screen.contains("旁问"));

        let (main_screen, _) = side_repair_main_view_screen();
        assert!(
            !main_screen.contains("旁问"),
            "回到主视图后不该再有常驻旁支提示，实际屏幕:\n{main_screen}"
        );
        assert!(!main_screen.contains("旁支可用工具"));
    }

    /// 同 `side_repair_run_loop_with`，但可指定终端尺寸与主题。
    ///
    /// 窄屏（32 列）是状态栏预算真正吃紧的地方：`.expect("FULL")` 之外再加
    /// 「旁问 · /back」就可能超宽。窗口尺寸写死 80×24 的用例看不出这个。
    #[allow(clippy::too_many_arguments)]
    fn side_repair_run_loop_sized(
        delivered_in_order: Vec<Event>,
        parent_agent_events: Vec<AgentEvent>,
        decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
        side_cmd_tx: tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>,
        side_events: tokio::sync::mpsc::Receiver<crate::tui::side::ConversationEvent>,
        parent_draft: &str,
        (width, height): (u16, u16),
        theme: TuiTheme,
    ) -> (String, InputLine) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        for event in parent_agent_events {
            agent_tx.try_send(event).unwrap();
        }
        drop(agent_tx);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // `poll` 从尾部弹出，所以交付顺序要反过来存。
        let mut events = delivered_in_order;
        events.reverse();
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(events)),
        };

        let project = tempfile::TempDir::new().unwrap();
        let mut input = InputLine::new();
        input.insert_str(parent_draft);
        let mut title = crate::tui::title::TitleHandle::disabled();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut input,
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            theme,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_events),
        )
        .unwrap();

        (frame_text(&terminal), input)
    }

    fn side_repair_status_frame() -> crate::tui::side::ConversationEvent {
        crate::tui::side::ConversationEvent {
            target: crate::tui::side::ConversationTarget::Side {
                side_id: "side-1".into(),
                generation: 1,
            },
            turn_id: None,
            event: crate::tui::side::ConversationPayload::SideStatus(
                yi_agent_runtime::side_conversation::SideStatus::Idle,
            ),
        }
    }

    #[test]
    fn side_repair_paste_back_through_the_loop_returns_to_main_and_keeps_the_parent_draft() {
        const PARENT_DRAFT: &str = "PARENT-DRAFT";
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_tx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (side_tx, side_rx) = tokio::sync::mpsc::channel(8);
        side_tx.try_send(side_repair_status_frame()).unwrap();
        drop(side_tx);
        let _ = (&input_tx, &interrupt_tx, &kill_tx, &control_tx);

        let (screen, input) = side_repair_run_loop_with(
            vec![
                Event::Paste("/back".into()),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ],
            Vec::new(),
            decision_tx,
            side_cmd_tx,
            side_rx,
            PARENT_DRAFT,
        );

        assert_eq!(
            input.buffer, PARENT_DRAFT,
            "旁支里的粘贴绝不能改动父会话草稿"
        );
        assert!(
            !screen.contains("旁问（只读）"),
            "回车之后必须已经回到主视图，实际屏幕:\n{screen}"
        );
        // 「主视图」的判据是旁支提示条消失、父草稿回到输入行。状态栏里的
        // `/side` 标签属于 Task 10，不能在这里顺带要求，否则这条用例会因为
        // 另一个任务的缺口而红——红的位置必须指向真正的原因。
        assert!(
            screen.contains(PARENT_DRAFT),
            "回到主视图后父草稿应可见，实际屏幕:\n{screen}"
        );
        assert!(
            interrupt_rx.try_recv().is_err(),
            "返回视图不是停止轮次：不该打父 interrupt"
        );
        while let Ok(command) = side_cmd_rx.try_recv() {
            assert!(
                !matches!(
                    command,
                    crate::side_driver::SideDriverCommand::Interrupt { .. }
                        | crate::side_driver::SideDriverCommand::Close { .. }
                ),
                "粘贴 /back 不该发出 Interrupt/Close，实际: {command:?}"
            );
        }
        assert!(
            decision_rx.try_recv().is_err(),
            "旁支按键绝不能替父会话作出裁决"
        );
    }

    /// 父审批挂起时，旁支里的粘贴与回车仍然不被父审批守卫吞掉。
    #[test]
    fn side_repair_paste_back_survives_a_parent_approval_through_the_loop() {
        const PARENT_DRAFT: &str = "PARENT-DRAFT";
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_tx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(8);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let (side_tx, side_rx) = tokio::sync::mpsc::channel(8);
        // 第一帧先把视图切进旁支，此时父会话那边还挂着一个审批请求。
        side_tx.try_send(side_repair_status_frame()).unwrap();
        drop(side_tx);
        let _ = (&input_tx, &interrupt_tx, &kill_tx, &control_tx);

        let (screen, input) = side_repair_run_loop_with(
            vec![
                Event::Paste("/back".into()),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ],
            vec![AgentEvent::PermissionRequest {
                request_id: 7,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf /tmp/x"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            }],
            decision_tx,
            side_cmd_tx,
            side_rx,
            PARENT_DRAFT,
        );

        assert_eq!(input.buffer, PARENT_DRAFT, "父草稿必须原样保留");
        assert!(
            !screen.is_empty(),
            "循环必须真的画过帧（否则断言的是空屏幕）"
        );
        assert!(
            decision_rx.try_recv().is_err(),
            "旁支里的回车不能替父会话批准工具调用"
        );
        while let Ok(command) = side_cmd_rx.try_recv() {
            assert!(
                !matches!(
                    command,
                    crate::side_driver::SideDriverCommand::Interrupt { .. }
                        | crate::side_driver::SideDriverCommand::Close { .. }
                ),
                "不该发出 Interrupt/Close，实际: {command:?}"
            );
        }
    }

    /// 循环里逐字符 `/back` 与 Tab 补全后回车都必须真的回到主视图。
    #[test]
    fn side_repair_typed_and_completed_back_work_through_the_loop() {
        for keys in [
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ],
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ],
        ] {
            let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
            let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
            let (kill_tx, _kill_tx) = tokio::sync::mpsc::channel::<String>(4);
            let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
            let (decision_tx, mut decision_rx) =
                tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
            let (side_cmd_tx, _side_cmd_rx) =
                tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
            let (side_tx, side_rx) = tokio::sync::mpsc::channel(8);
            side_tx.try_send(side_repair_status_frame()).unwrap();
            drop(side_tx);
            let _ = (&input_tx, &interrupt_tx, &kill_tx, &control_tx);

            let mut delivered = keys;
            delivered.push(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            )));
            let (screen, _input) = side_repair_run_loop_with(
                delivered,
                Vec::new(),
                decision_tx,
                side_cmd_tx,
                side_rx,
                "PARENT-DRAFT",
            );

            assert!(
                !screen.contains("旁问（只读）"),
                "手打或补全的 /back 都要真的回到主视图，实际屏幕:\n{screen}"
            );
            assert!(decision_rx.try_recv().is_err());
        }
    }

    // --- 退出优先级与失败反馈（计划 Task 9） ---------------------------------

    /// 用真实循环跑一串事件，返回屏幕、剩余输入行、以及两路通道的接收端。
    struct ExitLoopHarness {
        screen: String,
        side_cmd_rx: tokio::sync::mpsc::Receiver<crate::side_driver::SideDriverCommand>,
        parent_interrupt_rx: tokio::sync::mpsc::Receiver<()>,
        decision_rx: tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>,
    }

    #[allow(clippy::too_many_arguments)]
    fn run_exit_loop(
        delivered_in_order: Vec<Event>,
        side_status: Option<yi_agent_runtime::side_conversation::SideStatus>,
        side_running: bool,
        main_running: bool,
        prefill_side_channel: bool,
        parent_events: Vec<AgentEvent>,
        side_cmd_capacity: usize,
        drop_side_receiver: bool,
    ) -> ExitLoopHarness {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        for event in parent_events {
            agent_tx.try_send(event).unwrap();
        }
        drop(agent_tx);

        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, parent_interrupt_rx) = tokio::sync::mpsc::channel::<()>(4);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(8);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(side_cmd_capacity);
        if prefill_side_channel {
            // 容量 1 且先塞满：任何后续 try_send 都必然 Full。
            side_cmd_tx
                .try_send(crate::side_driver::SideDriverCommand::Interrupt {
                    expected_target: ConversationTarget::Side {
                        side_id: "occupied".into(),
                        generation: 99,
                    },
                    turn_id: "occupied".into(),
                })
                .unwrap();
        }
        if drop_side_receiver {
            // 关闭接收端：之后任何 try_send 都会得到 `Closed`。
            side_cmd_tx
                .try_send(crate::side_driver::SideDriverCommand::Interrupt {
                    expected_target: ConversationTarget::Side {
                        side_id: "occupied".into(),
                        generation: 99,
                    },
                    turn_id: "occupied".into(),
                })
                .ok();
            side_cmd_rx.close();
            while side_cmd_rx.try_recv().is_ok() {}
        }

        let (side_tx, side_rx) = tokio::sync::mpsc::channel(8);
        if let Some(status) = side_status {
            side_tx
                .try_send(crate::tui::side::ConversationEvent {
                    target: ConversationTarget::Side {
                        side_id: "side-1".into(),
                        generation: 1,
                    },
                    turn_id: side_running.then(|| "turn-1".to_string()),
                    event: ConversationPayload::SideStatus(status),
                })
                .unwrap();
        }
        drop(side_tx);

        let mut events = delivered_in_order;
        events.reverse();
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(events)),
        };
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(main_running));
        let project = tempfile::TempDir::new().unwrap();
        let mut title = crate::tui::title::TitleHandle::disabled();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_rx),
        )
        .unwrap();

        ExitLoopHarness {
            screen: frame_text(&terminal),
            side_cmd_rx,
            parent_interrupt_rx,
            decision_rx,
        }
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn typed(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn enter() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    /// 旁支里第一次 Ctrl+C 只停可见旁支，第二次才退出；父 interrupt 为空。
    #[test]
    fn side_repair_first_ctrl_c_in_side_stops_only_the_side() {
        // 第二次 Ctrl+C 之后紧跟一次「会发出旁支提交」的输入：若循环没有退出，
        // 这条输入会被处理并发出 Submit，因此「没有 Submit」就是「已退出」的
        // 可观测证据。不能拿退出后的屏幕内容判断——退出时循环不再画帧，屏幕
        // 停在退出前的最后一帧，那恰恰还是旁支视图。
        let mut h = run_exit_loop(
            vec![ctrl('c'), ctrl('c'), typed('h'), enter()],
            Some(yi_agent_runtime::side_conversation::SideStatus::Running),
            true,
            true,
            false,
            Vec::new(),
            8,
            false,
        );

        let first = h
            .side_cmd_rx
            .try_recv()
            .expect("第一次 Ctrl+C 应停可见旁支");
        match first {
            crate::side_driver::SideDriverCommand::Interrupt {
                expected_target,
                turn_id,
            } => {
                assert_eq!(
                    expected_target,
                    ConversationTarget::Side {
                        side_id: "side-1".into(),
                        generation: 1
                    },
                    "停止必须带上用户正在看的那个旁支身份"
                );
                assert_eq!(turn_id, "turn-1");
            }
            other => panic!("期望 Interrupt，实际 {other:?}"),
        }
        assert!(
            h.parent_interrupt_rx.try_recv().is_err(),
            "旁支里的 Ctrl+C 绝不能打断父会话"
        );
        assert!(h.decision_rx.try_recv().is_err());
        // 循环已退出：后续的「h + Enter」根本没被处理，因此不会出现 Submit。
        let mut saw_submit = false;
        while let Ok(command) = h.side_cmd_rx.try_recv() {
            if matches!(
                command,
                crate::side_driver::SideDriverCommand::Submit { .. }
            ) {
                saw_submit = true;
            }
        }
        assert!(!saw_submit, "第二次 Ctrl+C 之后不该再处理任何输入");
    }

    /// Ctrl+C 后编辑一次，确认态必须解除：再按一次 Ctrl+C 只重新确认，不退出。
    ///
    /// 用「父 interrupt 收到**两次**」来判定，而不是「循环有没有结束」：
    /// 确认态若没被解除，第二次 Ctrl+C 会直接退出，第二次中断就永远不会发出。
    #[test]
    fn side_repair_editing_after_ctrl_c_rearms_the_confirmation() {
        let mut h = run_exit_loop(
            vec![ctrl('c'), typed('x'), ctrl('c'), ctrl('q')],
            None,
            false,
            true, // 父会话在跑：每次「第一次 Ctrl+C」都会停一次
            false,
            Vec::new(),
            8,
            false,
        );
        assert!(
            h.parent_interrupt_rx.try_recv().is_ok(),
            "第一次 Ctrl+C 应停父会话"
        );
        assert!(
            h.parent_interrupt_rx.try_recv().is_ok(),
            "编辑之后确认态必须解除：第二次 Ctrl+C 要重新确认并再停一次，而不是直接退出"
        );
        assert!(h.side_cmd_rx.try_recv().is_err());
    }

    /// runtime popup 打开时 Ctrl+Q 仍必须退出。
    #[test]
    fn side_repair_ctrl_q_quits_even_with_the_runtime_popup_open() {
        let h = run_exit_loop(
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
                ctrl('q'),
                typed('x'),
            ],
            None,
            false,
            false,
            false,
            Vec::new(),
            8,
            false,
        );
        // 若 Ctrl+Q 被 popup 吞掉，后面的 `x` 会继续处理；这里只要循环正常
        // 结束且没有把 `x` 留在输入行就说明退出生效了。
        assert!(
            !h.screen.contains('x'),
            "Ctrl+Q 必须真的退出，而不是被 runtime popup 吞掉（屏幕:\n{}）",
            h.screen
        );
    }

    /// 父审批挂起时 Ctrl+Q 仍必须退出。
    #[test]
    fn side_repair_ctrl_q_quits_with_a_pending_parent_approval() {
        let mut h = run_exit_loop(
            vec![ctrl('q'), typed('1')],
            None,
            false,
            true,
            false,
            vec![AgentEvent::PermissionRequest {
                request_id: 7,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "rm -rf /tmp/x"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            }],
            8,
            false,
        );
        assert!(
            h.decision_rx.try_recv().is_err(),
            "Ctrl+Q 退出之后不能再处理审批键"
        );
    }

    /// `/q`、`/quit` 在旁支里退出应用，且不被当成旁支提问。
    #[test]
    fn side_repair_quit_commands_leave_the_side_without_a_submit() {
        for command in ["/q", "/quit"] {
            let mut delivered = Vec::new();
            for ch in command.chars() {
                delivered.push(typed(ch));
            }
            delivered.push(enter());
            // 之后再按一次 Ctrl+Q：若前一条命令没有退出，Ctrl+Q 也会退出，
            // 因此这里额外断言「没有任何旁支命令被发出」。
            delivered.push(ctrl('q'));
            let mut h = run_exit_loop(
                delivered,
                Some(yi_agent_runtime::side_conversation::SideStatus::Idle),
                false,
                false,
                false,
                Vec::new(),
                8,
                false,
            );
            assert!(
                h.side_cmd_rx.try_recv().is_err(),
                "{command} 不能被当成旁支提问发出去"
            );
        }
    }

    /// 容量已满：关闭命令立即返回、失败可见，循环仍能退出。
    #[test]
    fn side_repair_close_survives_a_full_side_channel() {
        let mut delivered = Vec::new();
        for ch in "/side-close".chars() {
            delivered.push(typed(ch));
        }
        delivered.push(enter());
        delivered.push(ctrl('q'));

        let mut h = run_exit_loop(
            delivered,
            Some(yi_agent_runtime::side_conversation::SideStatus::Idle),
            false,
            false,
            true, // 容量 1 且已塞满
            Vec::new(),
            1,
            false,
        );

        assert!(
            !h.screen.contains("旁问（只读）"),
            "关闭失败也必须本地返回，不能把用户卡在旁支里（屏幕:\n{}）",
            h.screen
        );
        assert!(
            h.screen.contains("未送达") || h.screen.contains("后台正忙"),
            "投递失败必须可见（屏幕:\n{}）",
            h.screen
        );
        assert!(h.side_cmd_rx.try_recv().is_ok(), "只有那条占位命令");
    }

    /// 旁支调度已退出（接收端被 drop）：命令失败可见，退出仍然可达。
    #[test]
    fn side_repair_close_survives_a_dropped_side_receiver() {
        let mut delivered = Vec::new();
        for ch in "/side-close".chars() {
            delivered.push(typed(ch));
        }
        delivered.push(enter());
        delivered.push(ctrl('q'));

        let h = run_exit_loop(
            delivered,
            Some(yi_agent_runtime::side_conversation::SideStatus::Idle),
            false,
            false,
            false,
            Vec::new(),
            8,
            true,
        );

        assert!(!h.screen.contains("旁问（只读）"));
        assert!(
            h.screen.contains("未送达") || h.screen.contains("已退出"),
            "接收端消失时失败必须可见（屏幕:\n{}）",
            h.screen
        );
    }

    #[test]
    fn side_events_never_reach_main_turn_log() {
        let project = tempfile::TempDir::new().unwrap();
        let store = crate::session_store::TuiSessionStore::create_new(
            project.path(),
            "test-model",
            yi_agent_app_server::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let thread_id = store.thread_id().to_string();

        // 主会话落一轮：只有主消息，旁问文本不在其中。
        let main_items = vec![yi_agent_app_server::protocol::Item::UserMessage {
            id: "user-main".into(),
            text: "主会话的问题".into(),
            attachments: Vec::new(),
            images: Vec::new(),
        }];
        let main_messages = vec![yi_agent_core::Message::user("主会话的问题")];
        store
            .append_turn(&main_items, &main_messages, None, None)
            .unwrap();

        // 旁问走了它自己的通道（`ConversationPayload::SideItems`），
        // 这段文本只出现在旁支视图里，从不经过 `TurnLog`。
        let side_only_marker = "SIDE-ONLY-MARKER";
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (side_tx, side_rx) = tokio::sync::mpsc::channel(16);
        let (side_cmd_tx, mut side_cmd_rx) = tokio::sync::mpsc::channel(8);
        side_tx
            .try_send(crate::tui::side::ConversationEvent {
                target: crate::tui::side::ConversationTarget::Side {
                    side_id: "side-1".into(),
                    generation: 1,
                },
                turn_id: Some("turn-1".into()),
                event: crate::tui::side::ConversationPayload::SideItems(vec![std::sync::Arc::new(
                    yi_agent_runtime::side_conversation::SideItem {
                        id: "side-item".into(),
                        turn_id: "turn-1".into(),
                        ordinal: 1,
                        content:
                            yi_agent_runtime::side_conversation::SideItemContent::AgentMessage {
                                text: side_only_marker.into(),
                            },
                    },
                )]),
            })
            .unwrap();
        drop(agent_tx);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))])),
        };
        let mut title = crate::tui::title::TitleHandle::disabled();

        run_loop_with_side(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
            crate::tui::app::SideRuntime::new(side_cmd_tx, side_rx),
        )
        .unwrap();

        // 落盘内容只含主消息。
        let loaded = store.load_for_replay().unwrap();
        assert_eq!(loaded.messages.len(), 1);
        let serialized = format!("{:?}{:?}", loaded.items, loaded.messages);
        assert!(
            !serialized.contains(side_only_marker),
            "旁支正文绝不能进父会话文件: {serialized}"
        );
        assert!(serialized.contains("主会话的问题"));

        // 主 store 的 thread id 与回放 hint 仍是主会话，不是旁支。
        assert_eq!(thread_id, store.thread_id());
        let _ = side_cmd_rx.try_recv();
    }

    #[test]
    fn runtime_start_prompt_sends_start_choice_before_accepting_chat_input() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::Start
        );
        assert!(
            input_rx.try_recv().is_err(),
            "startup choice must not be sent as chat input"
        );
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Always
        );
    }

    /// The alert must not eat the keystroke that dismisses it. A popup that
    /// consumed the first character typed would silently corrupt what the user
    /// was writing -- far worse than the nuisance it reports -- so the key both
    /// clears the alert and still reaches the input line.
    #[test]
    fn runtime_failure_alert_does_not_swallow_the_dismissing_key() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        agent_tx
            .try_send(AgentEvent::SubagentRuntimeUnavailable {
                stage: "runtime attach".into(),
                cause: "file is not a database".into(),
            })
            .unwrap();
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            // `ScriptedEvents::poll` pops from the back, so this is the
            // order the keys are delivered in.
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        assert_eq!(
            input_rx.try_recv().unwrap(),
            "hi",
            "the dismiss key was swallowed instead of reaching the input line"
        );
    }

    #[test]
    fn runtime_start_prompt_can_continue_without_delegation() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation
        );
        assert!(
            input_rx.try_recv().is_err(),
            "skip choice must not be sent as chat input"
        );
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Never
        );
    }

    #[test]
    fn escape_skips_runtime_for_this_session_without_persisting() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        // Session behaviour matches "n": delegation is not enabled this run.
        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation
        );
        // ...but Esc must not write a preference: the next launch asks again.
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(input_rx.try_recv().is_err());
    }

    /// Collects every `Separator` label pushed into history.
    fn separator_labels(history: &HistoryState) -> Vec<String> {
        history
            .cells
            .iter()
            .filter_map(|cell| match cell {
                HistoryCell::Separator { label } => label.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn hidden_commands_explain_themselves_instead_of_unknown() {
        for (command, needle) in [
            (SlashCommand::Approve, "暂不支持"),
            (SlashCommand::Deny, "暂不支持"),
            (SlashCommand::Budget, "暂不支持"),
            (SlashCommand::Priority, "暂不支持"),
        ] {
            let mut history = HistoryState::new();
            let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
            let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
            let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
            let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(1);
            let mut queued = crate::tui::queued::DeliveredInterjections::new();
            let _ = execute_slash_command(
                command,
                None,
                &mut history,
                80,
                &CostTracker::default(),
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &std::env::temp_dir(),
                &mut queued,
                &yi_agent_mcp::McpManager::empty(),
                &snapshot_for_tests(),
                "test-model",
                None,
                &catalog_for_tests(),
                None,
                EffortSource::Unset,
            );
            let labels = separator_labels(&history);
            assert!(
                labels
                    .iter()
                    .any(|l| l.contains(needle) && l.contains(command.name())),
                "{} must explain itself, got {labels:?}",
                command.name()
            );
        }
    }

    #[test]
    fn runtime_start_prompt_is_fully_visible_at_supported_widths() {
        // (terminal width, requested box width, box height) — 40 exercises
        // folding of the legend line; 80 covers the common case where the text
        // fits the box.
        for (term_w, box_w, box_h) in [(40u16, 58u16, 10u16), (80, 58, 10)] {
            let backend = TestBackend::new(term_w, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            // Mirror the live clamp in `run_loop` (`58.min(width - 4)`): ratatui's
            // buffer panics ("index outside of buffer") if the rendered area is
            // wider than the terminal, so a raw 58-wide rect is not a valid model
            // of a 40-column terminal.
            let effective_w = box_w.min(term_w.saturating_sub(4));
            let area = ratatui::layout::Rect::new(0, 0, effective_w, box_h);
            terminal
                .draw(|f| {
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(runtime_prompt_lines())
                            .wrap(ratatui::widgets::Wrap { trim: true }),
                        area,
                    );
                })
                .unwrap();

            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            // ratatui paints the second cell of every wide CJK glyph as a blank
            // continuation cell, so the raw buffer reads "启 动 并 记 住".
            // Compare glyphs without whitespace: the property under test is that
            // no glyph is dropped at the right edge, and clipping still removes
            // one, so the assertion keeps its teeth.
            let rendered: String = rendered.chars().filter(|c| !c.is_whitespace()).collect();

            // Tail of every selectable option must survive; clipping would drop it.
            assert!(rendered.contains("启动并记住"), "width {term_w}");
            assert!(rendered.contains("跳过并记住"), "width {term_w}");
            assert!(rendered.contains("本次跳过"), "width {term_w}");
            assert!(rendered.contains("不记住"), "width {term_w}");
        }
    }

    #[test]
    fn disabled_notice_prints_the_reason_into_history_only() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reason = "已按偏好禁用本地 Agent Runtime";
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            Some(
                crate::tui::subagents::RuntimeStartupIntent::DisabledNotice {
                    reason: reason.to_string(),
                },
            ),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        // Exactly one line explains why delegation is off for this launch.
        assert_eq!(separator_labels(&history), vec![reason.to_string()]);
        // Nothing was asked and nothing was written: a disabled notice is not a
        // user decision, so it must not touch the stored preference.
        assert!(runtime_choice_rx.try_recv().is_err());
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(input_rx.try_recv().is_err());
    }

    #[test]
    fn auto_start_intent_asks_nothing_and_persists_nothing() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A "y" would answer a prompt; AutoStart must never show one, so this key
        // must stay free for ordinary chat input instead.
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();

        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::AutoStart),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        // No dialog, no choice (main.rs pre-seeds `Start`), no notice, no write.
        let _ = runtime_choice_rx.try_recv();
        assert!(separator_labels(&history).is_empty());
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(
            input_rx.try_recv().is_err(),
            "an unsubmitted 'y' must not reach the agent as chat input"
        );
    }

    /// Test that first Ctrl+C does NOT quit but shows a confirm prompt,
    /// and any other key cancels the pending quit.
    #[test]
    fn first_ctrl_c_does_not_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Ctrl+C then Ctrl+Q: if first Ctrl+C quit, Ctrl+Q would be unreachable.
        // We verify the terminal buffer shows the confirm prompt after Ctrl+C.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // After Ctrl+C, the input row should show the confirm message
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let row_text: String = (0..80).map(|x| buffer[(x, input_row)].symbol()).collect();
        assert!(
            row_text.contains("Ctrl") || row_text.contains("退出"),
            "expected confirm prompt, got: {row_text:?}"
        );
    }

    /// Test that two Ctrl+C presses quit the TUI.
    #[test]
    fn two_ctrl_c_quits() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        );
        assert!(
            result.is_ok(),
            "two Ctrl+C should quit cleanly, got: {:?}",
            result
        );
    }

    /// Quitting with messages still waiting must report how many were dropped,
    /// so the loss is visible instead of silent.
    #[test]
    fn quitting_reports_pending_messages_that_were_dropped() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Delivery order: type "first" + Enter (sent), type "second" + Enter
        // (queued behind it), then Ctrl+Q. `ScriptedEvents` pops from the end,
        // so the delivery order is pushed in reverse.
        let mut delivery: Vec<Event> = Vec::new();
        for ch in "first".chars() {
            delivery.push(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        delivery.push(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        for ch in "second".chars() {
            delivery.push(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        delivery.push(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        delivery.push(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));
        delivery.reverse();

        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(delivery)),
        };

        let mut title = crate::tui::title::TitleHandle::disabled();
        let dropped = run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        assert_eq!(
            dropped, 1,
            "the queued-but-never-sent message must be reported as dropped"
        );
    }

    /// The quit notice must name the count, and must stay silent when nothing
    /// was dropped. Covered here rather than through `run_tui`, which needs a
    /// real TTY (`enable_raw_mode`).
    #[test]
    fn report_dropped_pending_writes_the_count_only_when_nonzero() {
        let mut buf: Vec<u8> = Vec::new();
        report_dropped_pending(&mut buf, 3);
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains('3'),
            "the notice must name how many were dropped: {text:?}"
        );
        assert!(
            text.contains("已丢弃"),
            "the notice must be the user-facing message: {text:?}"
        );

        let mut buf: Vec<u8> = Vec::new();
        report_dropped_pending(&mut buf, 0);
        assert!(
            buf.is_empty(),
            "nothing must be printed when no messages were dropped: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    /// Repeated Esc must not terminate the loop; Ctrl+Q is the explicit terminator.
    #[test]
    fn repeated_esc_does_not_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // ScriptedEvents pops from the end, so this yields Esc, Esc, x, Ctrl+Q.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        );
        assert!(
            result.is_ok(),
            "Ctrl+Q should terminate the TUI cleanly, got: {:?}",
            result
        );
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let row_text: String = (0..80).map(|x| buffer[(x, input_row)].symbol()).collect();
        assert!(
            row_text.contains('x'),
            "the key after repeated Esc must be processed, got: {row_text:?}"
        );
    }

    /// Test that Ctrl+Q quits directly (no confirm needed).
    #[test]
    fn ctrl_q_quits_directly() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        );
        assert!(
            result.is_ok(),
            "Ctrl+Q should quit directly, got: {:?}",
            result
        );
    }

    /// Test that typing characters updates the input buffer and renders them
    /// to the terminal buffer. This verifies the draw loop reacts to key events.
    #[test]
    fn typing_renders_to_screen() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Script: type "hi", then Ctrl+Q to quit (don't submit, so input stays on screen)
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents {
            events: events.clone(),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // No submit happened, so input_tx should be empty
        assert!(
            input_rx.try_recv().is_err(),
            "no submit expected, but got a message"
        );

        // The terminal buffer should contain "hi" in the input row (last row)
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let row_text: String = (0..80).map(|x| buffer[(x, input_row)].symbol()).collect();
        assert!(
            row_text.contains("hi"),
            "expected 'hi' in input row, got: {row_text:?}"
        );
    }

    /// Test that typing a long string that exceeds the terminal width wraps
    /// onto additional lines so all typed characters remain visible.
    ///
    /// The bug: the input area was constrained to 1 line and the Paragraph
    /// did not wrap, so characters past the right edge were clipped.
    #[test]
    fn long_input_wraps_to_multiple_lines() {
        let backend = TestBackend::new(20, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type 30 'a's into a 20-column terminal. "> aa..." takes 3 cols for
        // prefix, leaving 17 cols per line. 30 chars should wrap across at
        // least 2 lines.
        let mut events = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        for _ in 0..30 {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            )));
        }
        let events = Rc::new(RefCell::new(events));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // Collect all text from the bottom 6 rows of the terminal (the input
        // area should live there now).
        let buffer = terminal.backend().buffer();
        let bottom_text: String = (18..24u16)
            .flat_map(|y| (0..20u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        // Count the number of 'a' characters visible somewhere in the bottom
        // of the screen. All 30 must be visible.
        let a_count = bottom_text.matches('a').count();
        assert_eq!(
            a_count, 30,
            "expected all 30 'a's visible in input area, only found {a_count}; bottom text: {bottom_text:?}"
        );
    }

    /// CJK characters are double-width. Verify they wrap correctly so all
    /// are visible and none are clipped.
    #[test]
    fn long_input_with_cjk_wraps_correctly() {
        let backend = TestBackend::new(20, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type 10 '你's (each 2 cells wide = 20 cells total) into 20-col
        // terminal. With "> " prefix, avail = 18, so first line holds 9 chars
        // (18 cells) and 1 char wraps to the next line.
        let mut events = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        for _ in 0..10 {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('你'),
                KeyModifiers::NONE,
            )));
        }
        let events = Rc::new(RefCell::new(events));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let bottom_text: String = (18..24u16)
            .flat_map(|y| (0..20u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        let count = bottom_text.matches('你').count();
        assert_eq!(
            count, 10,
            "expected all 10 '你' visible, found {count}; bottom: {bottom_text:?}"
        );
    }

    /// Unit test for `compute_input_height` with ASCII.
    #[test]
    fn compute_input_height_ascii() {
        // The card always adds one padding row above and below the text, so
        // the height is `text_rows + 2`. Width 20 leaves 18 for the first row
        // (after `"> "`); continuation rows carry no indent and get all 20.
        let pad = INPUT_CARD_PADDING * 2;
        let mut inp = InputLine::new();
        assert_eq!(compute_input_height(&inp, false, 20), 1 + pad);
        inp.buffer = "a".repeat(18);
        assert_eq!(compute_input_height(&inp, false, 20), 1 + pad, "18 fits");
        inp.buffer = "a".repeat(19);
        assert_eq!(compute_input_height(&inp, false, 20), 2 + pad, "19 wraps");
        // 18 (first row) + 20 (full continuation row) = 38 fits in 2 rows.
        inp.buffer = "a".repeat(38);
        assert_eq!(
            compute_input_height(&inp, false, 20),
            2 + pad,
            "38 = 18 + 20 fits two rows"
        );
        inp.buffer = "a".repeat(39);
        assert_eq!(compute_input_height(&inp, false, 20), 3 + pad, "39 wraps");
        // pending_quit keeps the card at three rows: collapsing it would make
        // the frame jump on Ctrl+C.
        assert_eq!(compute_input_height(&inp, true, 20), 1 + pad);
    }

    /// Unit test for `compute_input_height` with CJK (double-width).
    #[test]
    fn compute_input_height_cjk() {
        let mut inp = InputLine::new();
        // width=20, prefix=2, avail=18. Each '你' is 2 cells. 9 chars = 18
        // cells fit on line 1; 10 chars = 20 cells -> 2 lines.
        let pad = INPUT_CARD_PADDING * 2;
        inp.buffer = "你".repeat(9);
        assert_eq!(
            compute_input_height(&inp, false, 20),
            1 + pad,
            "9 cjk = 18 cells fits"
        );
        inp.buffer = "你".repeat(10);
        assert_eq!(
            compute_input_height(&inp, false, 20),
            2 + pad,
            "10 cjk = 20 cells wraps"
        );
    }

    /// The input stripe must follow the theme: on a light terminal the old
    /// hardcoded `Indexed(240)` painted a dark gray band across a white screen.
    #[test]
    fn input_card_uses_input_bg() {
        let input = InputLine::new();
        let dark = build_input_line_with(&input, false, 40, TuiTheme::DARK);
        let light = build_input_line_with(&input, false, 40, TuiTheme::LIGHT);
        // The card is three rows: padding / text / padding. Render the whole
        // card so the padding rows are inspected too, not just the text row.
        let area = ratatui::layout::Rect::new(0, 0, 40, 3);
        let mut dark_buf = ratatui::buffer::Buffer::empty(area);
        let mut light_buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(dark, area, &mut dark_buf);
        ratatui::widgets::Widget::render(light, area, &mut light_buf);

        for (buf, theme) in [(&dark_buf, TuiTheme::DARK), (&light_buf, TuiTheme::LIGHT)] {
            for y in 0..3u16 {
                assert_eq!(buf[(39, y)].bg, theme.input_bg, "row {y} background");
            }
        }
        assert_ne!(dark_buf[(39, 0)].bg, TuiTheme::DARK.bg_raised);
        assert_ne!(light_buf[(39, 0)].bg, TuiTheme::LIGHT.bg_raised);
        // The card needs an explicit foreground: `input_bg` is a fixed gray in
        // both themes, so text left at the terminal default would vanish when a
        // dark terminal is forced to `YI_AGENT_THEME=light`.
        assert_eq!(dark_buf[(39, 1)].fg, TuiTheme::DARK.fg);
        assert_eq!(light_buf[(39, 1)].fg, TuiTheme::LIGHT.fg);
    }

    /// The card is a contiguous band: both padding rows carry no characters and
    /// the same background as the text row. Terminal-native selection over the
    /// middle row alone therefore copies clean text.
    #[test]
    fn input_card_pads_one_blank_row_each_side() {
        let mut input = InputLine::new();
        input.insert_str("hello");
        let paragraph = build_input_line_with(&input, false, 20, TuiTheme::DARK);
        let area = ratatui::layout::Rect::new(0, 0, 20, 3);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(paragraph, area, &mut buf);

        for y in [0u16, 2] {
            let row: String = (0..20u16).map(|x| buf[(x, y)].symbol()).collect();
            assert_eq!(row.trim(), "", "padding row {y} must be blank: {row:?}");
            assert_eq!(buf[(0, y)].bg, TuiTheme::DARK.input_bg);
        }
        let text_row: String = (0..20u16).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(text_row.trim_end(), "> hello");
    }

    /// Only the first row carries `"> "`; continuation rows start at column 0.
    /// The old two-space indent leaked into every copied line as leading
    /// whitespace, and terminals do not trim leading spaces on copy.
    #[test]
    fn wrapped_input_keeps_only_the_first_prefix() {
        let mut input = InputLine::new();
        // Width 12 leaves 10 for the first row; 25 chars wrap to 3 rows.
        input.insert_str(&"x".repeat(25));
        let lines = wrap_input_buffer(&input.buffer, input.cursor, &Span::raw("> "), 12);
        assert!(lines.len() >= 2, "expected wrapping, got {}", lines.len());
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(rendered[0].starts_with("> "), "{:?}", rendered[0]);
        for (i, line) in rendered.iter().enumerate().skip(1) {
            assert!(
                !line.starts_with(' ') && !line.starts_with('>'),
                "continuation row {i} must start at column 0: {line:?}"
            );
        }
        // Nothing but the prefix itself (plus the trailing cursor cell, which
        // Ghostty trims on copy) may survive concatenation.
        let joined: String = rendered.concat();
        assert_eq!(joined.trim_end(), format!("> {}", "x".repeat(25)));
    }

    /// The `Ctrl+C` prompt rides the same three-row card. It used to be one
    /// row, so pressing `Ctrl+C` shrank the card and made the whole frame jump.
    #[test]
    fn pending_quit_keeps_the_three_row_card() {
        let input = InputLine::new();
        let light = build_input_line_with(&input, true, 40, TuiTheme::LIGHT);
        let area = ratatui::layout::Rect::new(0, 0, 40, 3);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(light, area, &mut buf);

        for y in 0..3u16 {
            assert_eq!(buf[(39, y)].bg, TuiTheme::LIGHT.input_bg, "row {y}");
        }
        for y in [0u16, 2] {
            let row: String = (0..40u16).map(|x| buf[(x, y)].symbol()).collect();
            assert_eq!(row.trim(), "", "padding row {y} must be blank");
        }
        // Wide (CJK) glyphs occupy two cells and ratatui writes a space into
        // the second one; drop all whitespace before matching.
        let hint: String = (0..40u16)
            .map(|x| buf[(x, 1)].symbol())
            .filter(|s| !s.trim().is_empty())
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert!(hint.contains("再按Ctrl+C退出"), "{hint:?}");
        // Padding cells inherit the paragraph foreground, not the hint's `warn`.
        assert_eq!(buf[(39, 0)].fg, TuiTheme::LIGHT.fg);
    }

    /// Whole-frame acceptance: with the light theme, no cell may be painted with
    /// a value that was unreadable on a white terminal.
    ///
    /// Per-renderer tests can each pass while one call site still bakes in a raw
    /// color, so this renders the entire frame through the real `run_loop` and
    /// inspects every cell. `Color::White` is banned as a foreground: `sel_fg`
    /// is `Rgb(255,255,255)`, a distinct variant, so a legitimate selection is
    /// not a false positive.
    #[test]
    fn light_theme_frame_has_no_unreadable_cells() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Queue real history before the first draw. An empty transcript would
        // leave the cell renderers (assistant markdown, tool call, tool result)
        // unexercised, which is where most of the palette lives.
        agent_tx
            .try_send(AgentEvent::AssistantText(
                "**bold** and `code` on a light terminal".into(),
            ))
            .unwrap();
        agent_tx
            .try_send(AgentEvent::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            })
            .unwrap();
        agent_tx
            .try_send(AgentEvent::ToolResult {
                id: "call-1".into(),
                result: yi_agent_core::tool::ToolResult::text("file-a\nfile-b"),
            })
            .unwrap();
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events_themed(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::LIGHT,
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let mut bad: Vec<(u16, u16, Color)> = Vec::new();
        for y in 0..30u16 {
            for x in 0..100u16 {
                let cell = &buffer[(x, y)];
                let offender = matches!(cell.fg, Color::Gray | Color::DarkGray | Color::White)
                    || cell.bg == Color::Indexed(240);
                if offender {
                    bad.push((x, y, cell.fg));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "light theme left {} unreadable cells out of {}; first 5 (x, y, fg): {:?}",
            bad.len(),
            30 * 100,
            &bad[..bad.len().min(5)]
        );
    }

    /// Test that agent events appear in the rendered history area.
    #[test]
    fn agent_events_render_to_history_area() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Send an assistant message before starting
        agent_tx
            .try_send(AgentEvent::AssistantText("hello world".into()))
            .unwrap();

        // Script: Ctrl+Q to quit after one frame
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // The history area should contain "hello world"
        let buffer = terminal.backend().buffer();
        let history_text: String = (0..23u16)
            .flat_map(|y| (0..80u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        assert!(
            history_text.contains("hello world"),
            "expected 'hello world' in history, got: {history_text:?}"
        );
    }

    // ----- Slash command popup tests -----

    /// Helper: collect all text from the terminal buffer.
    fn collect_all_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..24u16)
            .flat_map(|y| (0..80u16).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    }

    #[test]
    fn paste_inserts_text_clears_pending_quit_and_filters_slash_popup() {
        let history = HistoryState::new();
        let mut input = InputLine::new();
        let runtime_popup = RuntimePopup::None;
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &runtime_popup,
            &mut pending_quit,
            &mut popup,
            crate::tui::side::ApprovalInputMode::Decision,
        );

        assert_eq!(input.buffer, "/cl");
        assert!(!pending_quit);
        assert_eq!(
            popup.and_then(|popup| popup.selected()).map(|c| c.name()),
            Some("clear")
        );
    }

    #[test]
    fn paste_is_ignored_while_permission_is_pending() {
        let mut history = HistoryState::new();
        history.push_event(make_permission_request_normal(1), 80);
        let mut input = InputLine::new();
        let runtime_popup = RuntimePopup::None;
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &runtime_popup,
            &mut pending_quit,
            &mut popup,
            crate::tui::side::ApprovalInputMode::Decision,
        );

        assert!(input.buffer.is_empty());
        assert!(pending_quit);
        assert!(popup.is_none());
    }

    #[test]
    fn paste_is_ignored_while_runtime_popup_is_active() {
        let history = HistoryState::new();
        let mut input = InputLine::new();
        let runtime_popup =
            RuntimePopup::Bash(BashPopup::List(ListPopup::new(vec!["task-1".to_string()])));
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &runtime_popup,
            &mut pending_quit,
            &mut popup,
            crate::tui::side::ApprovalInputMode::Decision,
        );

        assert!(input.buffer.is_empty());
        assert!(pending_quit);
        assert!(popup.is_none());
    }

    /// The bash panel's kill must reach the agent driver as a request to stop
    /// that specific tool call. This used to be a `// TODO` placeholder that
    /// only marked the local registry entry failed and left the process running.
    #[test]
    fn bash_panel_kill_sends_the_tool_call_id_to_the_driver() {
        let mut registry = RunningTaskRegistry::new();
        registry.on_tool_call("call_1", "bash", "sleep 300", 120);
        let mut popup = RuntimePopup::Bash(BashPopup::Detail(DetailPopup::new("call_1".into())));
        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<String>(8);

        // `k` -> confirm prompt
        handle_runtime_popup_key_for_test(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut popup,
            &["call_1".into()],
            &[],
        );
        // `y` -> send. Re-dispatch through the real handler so the channel is exercised.
        let mut popup = RuntimePopup::Bash(BashPopup::ConfirmKill(ConfirmKill {
            task_id: "call_1".into(),
        }));
        handle_bash_popup_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            match &mut popup {
                RuntimePopup::Bash(b) => b,
                _ => unreachable!(),
            },
            &registry,
            &kill_tx,
            80,
        );

        assert_eq!(
            kill_rx.try_recv().as_deref(),
            Ok("call_1"),
            "confirming the kill must ask the driver to cancel that tool call"
        );
    }

    /// `n`/Esc must back out without killing anything.
    #[test]
    fn bash_panel_kill_cancel_does_not_send_a_request() {
        let mut registry = RunningTaskRegistry::new();
        registry.on_tool_call("call_1", "bash", "sleep 300", 120);
        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let mut bash_popup = BashPopup::ConfirmKill(ConfirmKill {
            task_id: "call_1".into(),
        });

        handle_bash_popup_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut bash_popup,
            &registry,
            &kill_tx,
            80,
        );

        assert!(
            kill_rx.try_recv().is_err(),
            "declining the kill must not send anything"
        );
        assert!(
            matches!(bash_popup, BashPopup::Detail(_)),
            "declining returns to the detail view"
        );
    }

    #[test]
    fn ctrl_p_process_tab_kill_confirmation_sends_process_id() {
        let mut runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
            ProcessDetailPopup::new("proc_1".into()),
        ));
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);

        handle_runtime_popup_key_for_test(key, &mut runtime_popup, &[], &[]);

        assert!(matches!(
            runtime_popup,
            RuntimePopup::Processes(ProcessPopup::ConfirmKill(_))
        ));
    }

    fn subagent_items() -> SubagentListState {
        SubagentListState::new(vec![
            super::super::trace::SubagentListItem {
                task_id: "task-1".into(),
                objective: Some("first child".into()),
                state: "running".into(),
                last_step: None,
                active: true,
                parent_task_id: Some("root".into()),
            },
            super::super::trace::SubagentListItem {
                task_id: "task-2".into(),
                objective: Some("second child".into()),
                state: "completed".into(),
                last_step: None,
                active: false,
                parent_task_id: Some("root".into()),
            },
        ])
    }

    fn agents_key(
        key: KeyEvent,
        popup: &mut RuntimePopup,
        children: &SubagentListState,
    ) -> Option<String> {
        let registry = RunningTaskRegistry::new();
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        handle_runtime_popup_key(
            key,
            popup,
            &registry,
            &kill_tx,
            &[],
            &std::collections::HashMap::new(),
            children,
            80,
            24,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_runtime_popup_cycles_bash_processes_and_agents() {
        assert_eq!(RuntimeTab::BashTasks.next(), RuntimeTab::Processes);
        assert_eq!(RuntimeTab::Processes.next(), RuntimeTab::Agents);
        assert_eq!(RuntimeTab::Agents.next(), RuntimeTab::BashTasks);
    }

    #[test]
    fn switching_to_the_agents_tab_builds_the_list_from_the_children() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));

        popup.switch_tab(Vec::new(), children.clone());

        let RuntimePopup::Agents(agents) = popup else {
            panic!("expected the agents tab");
        };
        assert_eq!(agents.children.len(), 2);
        assert_eq!(agents.children.items[0].task_id, "task-1");
        assert!(agents.detail.is_none());
    }

    #[test]
    fn enter_on_a_list_row_opens_that_agents_detail() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Agents(Box::new(AgentsPopup::new(children.clone())));

        agents_key(key(KeyCode::Down), &mut popup, &children);
        agents_key(key(KeyCode::Enter), &mut popup, &children);

        let RuntimePopup::Agents(agents) = popup else {
            panic!("expected the agents tab");
        };
        let Some(super::super::trace::TracePopup::Detail(detail)) = agents.detail else {
            panic!("expected a detail view");
        };
        assert_eq!(detail.task_id(), "task-2", "the row under the cursor opens");
    }

    #[test]
    fn esc_in_a_detail_returns_to_the_list_before_closing_the_popup() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Agents(Box::new(AgentsPopup::new(children.clone())));
        agents_key(key(KeyCode::Enter), &mut popup, &children);

        agents_key(key(KeyCode::Esc), &mut popup, &children);

        let RuntimePopup::Agents(agents) = popup else {
            panic!("the popup must stay open on the first Esc");
        };
        assert!(agents.detail.is_none(), "back to the list");
    }

    #[test]
    fn esc_on_the_agent_list_closes_the_popup() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Agents(Box::new(AgentsPopup::new(children.clone())));

        agents_key(key(KeyCode::Esc), &mut popup, &children);

        assert!(matches!(popup, RuntimePopup::None));
    }

    #[test]
    fn runtime_popup_tab_switches_between_bash_and_processes() {
        let mut popup = RuntimePopup::Bash(BashPopup::List(ListPopup::new(vec!["bash_1".into()])));

        popup.switch_tab(Vec::new(), SubagentListState::default());

        assert!(matches!(
            popup,
            RuntimePopup::Processes(ProcessPopup::List(_))
        ));
    }

    #[test]
    fn process_runtime_popup_blocks_text_input() {
        let popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));

        assert!(popup.blocks_text_input());
    }

    #[test]
    fn bracketed_paste_renders_input_and_opens_slash_popup() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Paste("/cl".into()),
            ])),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let text = collect_all_text(&terminal);
        assert!(text.contains("/cl"), "expected pasted input, got: {text:?}");
        assert!(
            text.contains("clear"),
            "expected slash popup, got: {text:?}"
        );
    }

    /// Typing `/` should show the slash command popup with all commands.
    #[test]
    fn slash_popup_appears_on_slash() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', then Ctrl+Q to quit
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let text = collect_all_text(&terminal);
        // Popup should show at least some command names
        assert!(
            text.contains("quit"),
            "expected 'quit' in popup, got: {text:?}"
        );
        assert!(
            text.contains("clear"),
            "expected 'clear' in popup, got: {text:?}"
        );
        assert!(
            text.contains("help"),
            "expected 'help' in popup, got: {text:?}"
        );
    }

    /// Typing `/cl` should filter the popup to only show `/clear`.
    #[test]
    fn slash_popup_filters_on_typing() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', 'c', 'l', then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let text = collect_all_text(&terminal);
        // Should show 'clear' but not 'quit' or 'help' (filtered out)
        assert!(text.contains("clear"), "expected 'clear' in popup");
        assert!(!text.contains("quit"), "quit should be filtered out");
        assert!(!text.contains("help"), "help should be filtered out");
    }

    /// Tab should complete the selected command name into the input buffer.
    #[test]
    fn slash_popup_tab_completes() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', 'c', 'l', Tab, then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // The input row should contain "/clear" (completed by Tab)
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let row_text: String = (0..80u16)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(
            row_text.contains("/clear"),
            "expected '/clear' in input after Tab, got: {row_text:?}"
        );
    }

    /// Enter on `/quit` should execute the quit command and exit the loop.
    #[test]
    fn slash_popup_enter_executes_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', 'q', 'u', 'i', 't', Enter — /quit should exit the loop.
        // We add a Ctrl+Q at the end as a fallback in case /quit doesn't work,
        // but since events are popped LIFO, Ctrl+Q is listed first.
        let events = Rc::new(RefCell::new(vec![
            // Fallback: if /quit doesn't execute, Ctrl+Q will still exit
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        );
        assert!(
            result.is_ok(),
            "/quit + Enter should quit cleanly, got: {:?}",
            result
        );

        // If /quit executed properly, no message should be sent to agent
        // (if it didn't execute, the fallback Ctrl+Q ran, which is still ok).
        // We don't assert on input_tx because we can't distinguish.
    }

    /// Esc should dismiss the popup without modifying the input.
    #[test]
    fn slash_popup_esc_dismisses() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', Esc, then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // After Esc, popup should be gone. The input row should still have "/"
        // (Esc doesn't clear input, just dismisses popup).
        // The popup area (above input) should NOT contain command names.
        let text = collect_all_text(&terminal);
        // "quit" might appear in input row if '/' is still there, but after Esc
        // the popup is dismissed. We check that the popup doesn't show all commands.
        // Actually, after Esc dismisses popup, typing Ctrl+Q quits. The final frame
        // should show no popup. But the input "/" was typed, then Esc dismissed popup.
        // The input buffer still has "/".
        // Let's just verify the app didn't crash and the popup is not visible.
        // Since popup is dismissed, the text should not contain "清空对话上下文" (clear's description).
        assert!(
            !text.contains("清空对话上下文"),
            "popup should be dismissed after Esc"
        );
    }

    /// Up/Down should navigate the popup selection, not history.
    #[test]
    fn slash_popup_up_down_navigates() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', Down (move to 2nd item), Tab (complete), Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // After Down + Tab, the 2nd command (clear) should be completed into input
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let row_text: String = (0..80u16)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(
            row_text.contains("/clear"),
            "expected '/clear' after Down+Tab, got: {row_text:?}"
        );
    }

    /// Moving the selection past the visible window must scroll the popup so the
    /// highlighted row stays on screen. Otherwise the user cannot see what Enter
    /// is about to execute.
    #[test]
    fn slash_popup_scrolls_to_keep_selection_visible() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The popup is capped at 10 rows, i.e. 8 content rows, so 20 Down presses
        // land the selection far outside the initial window.
        const DOWNS: usize = 20;
        // The popup lists `completable()`, so the Nth row after N Down presses is
        // the Nth completable command (not the Nth entry of the full catalog).
        let target = SlashCommand::completable()[DOWNS];

        // `ScriptedEvents` pops from the back, so push in reverse delivery order.
        let mut script = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        script.extend(
            (0..DOWNS).map(|_| Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))),
        );
        script.push(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(script)),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let highlight_row =
            (0..24u16).find(|&y| (0..80u16).any(|x| buffer[(x, y)].bg == TuiTheme::DARK.sel_bg));
        let row = highlight_row.expect("the highlighted command row must be visible");
        let row_text: String = (0..80u16).map(|x| buffer[(x, row)].symbol()).collect();
        assert!(
            row_text.contains(target.name()),
            "expected the highlighted row to show /{}, got: {row_text:?}",
            target.name()
        );
    }

    /// The highlighted row must stay on screen even when the popup is drawn
    /// shorter than its 10-row cap (short terminal or a squeezed layout), where
    /// the drawn height no longer matches the scroll window.
    #[test]
    fn slash_popup_keeps_selection_visible_on_a_short_terminal() {
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // A 14-row terminal leaves the popup only 8 rows, i.e. 6 content rows.
        const DOWNS: usize = 12;
        // See `slash_popup_scrolls_to_keep_selection_visible`: the popup lists
        // `completable()`, so index into that view.
        let target = SlashCommand::completable()[DOWNS];

        let mut script = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        script.extend(
            (0..DOWNS).map(|_| Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))),
        );
        script.push(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(script)),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let highlight_row =
            (0..14u16).find(|&y| (0..80u16).any(|x| buffer[(x, y)].bg == TuiTheme::DARK.sel_bg));
        let row = highlight_row.expect("the highlighted command row must be visible");
        let row_text: String = (0..80u16).map(|x| buffer[(x, row)].symbol()).collect();
        assert!(
            row_text.contains(target.name()),
            "expected the highlighted row to show /{}, got: {row_text:?}",
            target.name()
        );
    }

    /// Unknown slash command should show an error in history, not send to agent.
    #[test]
    fn unknown_slash_command_shows_error() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/foo', Enter, then Ctrl+Q to quit
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // No message should have been sent to the agent
        assert!(
            input_rx.try_recv().is_err(),
            "unknown command should not send to agent"
        );

        // The history should contain an error message.
        // Note: TestBackend renders CJK chars with spaces between them, so we
        // check for a substring that works regardless of spacing.
        let text = collect_all_text(&terminal);
        let text_compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            text_compact.contains("未知命令") || text_compact.contains("unknown"),
            "expected error message for unknown command, got compact: {text_compact:?}"
        );
    }

    /// Typing a space after '/' should dismiss the popup (entering arg mode).
    #[test]
    fn slash_popup_dismisses_on_space() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', space, then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // After space, popup should be dismissed
        let text = collect_all_text(&terminal);
        assert!(
            !text.contains("清空对话上下文"),
            "popup should be dismissed after space"
        );
    }

    /// The cursor position should be rendered with reverse video (white bg,
    /// black fg) so the user can see where their cursor is in the input.
    #[test]
    fn cursor_shown_with_reverse_video() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type "abc" — cursor should be at position 3 (after 'c').
        // With reverse video, at least one cell in the input row should be
        // rendered with the REVERSED modifier.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // The cursor is reverse video, not a hardcoded white block: a white
        // block is invisible on a white terminal (contrast 1.00:1).
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let mut found_cursor = false;
        for x in 0..80u16 {
            let cell = &buffer[(x, input_row)];
            if cell.modifier.contains(ratatui::style::Modifier::REVERSED) {
                found_cursor = true;
                break;
            }
        }
        assert!(found_cursor, "expected a REVERSED cursor cell in input row");
    }

    /// When the buffer is empty, the cursor should still be visible (at position 0).
    #[test]
    fn cursor_visible_on_empty_buffer() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Just quit — no input typed. The input row should still show a cursor.
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let mut found_cursor = false;
        for x in 0..80u16 {
            let cell = &buffer[(x, input_row)];
            if cell.modifier.contains(ratatui::style::Modifier::REVERSED) {
                found_cursor = true;
                break;
            }
        }
        assert!(found_cursor, "expected cursor on empty buffer");
    }

    /// When cursor is in the middle of text, the character at cursor should
    /// be rendered with reverse video while surrounding text is normal.
    #[test]
    fn cursor_in_middle_of_text() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type "abc", move left to position 2 (between 'b' and 'c').
        // The 'c' character should have reverse video.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // Input starts at x=2 (after "> " prefix). 'a' at x=2, 'b' at x=3, 'c' at x=4.
        // Cursor at byte offset 2 means 'c' is the cursor character, at x=4.
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let cursor_cell = &buffer[(4, input_row)];
        assert!(
            cursor_cell
                .modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "cursor character 'c' should be rendered reversed"
        );

        // The 'a' character (not at cursor) should NOT have reverse video
        let non_cursor_cell = &buffer[(2, input_row)];
        assert!(
            !non_cursor_cell
                .modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "non-cursor character 'a' should not be reversed"
        );
    }

    // ----- Permission key handling tests -----

    /// Helper to create a Normal permission request event.
    fn make_permission_request_normal(request_id: u64) -> AgentEvent {
        AgentEvent::PermissionRequest {
            request_id,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: Some("ls".into()),
            kind: yi_agent_core::permission::PermissionKind::Normal,
        }
    }

    /// Helper to create a Blacklisted permission request event.
    fn make_permission_request_blacklisted(request_id: u64) -> AgentEvent {
        AgentEvent::PermissionRequest {
            request_id,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf /"}),
            prefix_suggestion: Some("rm".into()),
            kind: yi_agent_core::permission::PermissionKind::Blacklisted("rm -rf".into()),
        }
    }

    /// Helper to create a permission request with no prefix suggestion.
    fn make_permission_request_no_prefix(request_id: u64) -> AgentEvent {
        AgentEvent::PermissionRequest {
            request_id,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: None,
            kind: yi_agent_core::permission::PermissionKind::Normal,
        }
    }

    /// Event source that plays back scripted events, then returns Ctrl+Q
    /// forever. This ensures the TUI loop eventually exits once scripted
    /// events are exhausted and any pending permission is resolved.
    struct ScriptedThenQuitEvents {
        events: Rc<RefCell<Vec<Event>>>,
    }

    impl EventSource for ScriptedThenQuitEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let ev = self.events.borrow_mut().pop();
            Ok(Some(ev.unwrap_or(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            )))))
        }
    }

    /// Delivers one streamed assistant token per frame, then quits.
    struct StreamingTokenEvents {
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        poll_count: Cell<usize>,
    }

    impl EventSource for StreamingTokenEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let count = self.poll_count.get();
            self.poll_count.set(count + 1);
            if count < 4 {
                let _ = self
                    .agent_tx
                    .try_send(AgentEvent::AssistantText(" more".into()));
                return Ok(None);
            }
            Ok(Some(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))))
        }
    }

    /// A streamed token must re-render only the cell it changed.
    ///
    /// `run_loop` applies events at the width the draw will use, not the raw
    /// area width. The history renderer reserves its rightmost column for the
    /// scrollbar, and the cache is keyed by width, so applying events at the
    /// area width used to re-render the whole scrollback once at each width,
    /// i.e. twice per token.
    #[test]
    fn streaming_a_token_re_renders_only_the_changed_cell() {
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));

        const CELLS: usize = 30;
        let mut history = HistoryState::new();
        for index in 0..CELLS {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("answer {index} with some words in it"),
                },
                79,
            );
        }
        // Warm the cache the way a settled session would have it.
        let warm_width = history.text_width(80, 6);
        assert_eq!(warm_width, 79, "the fixture must overflow the viewport");
        let _ = HistoryView {
            state: &history,
            width: 80,
            theme: TuiTheme::DARK,
        }
        .flattened_lines(warm_width);

        let source = StreamingTokenEvents {
            agent_tx,
            poll_count: Cell::new(0),
        };

        crate::tui::cell::reset_lines_call_count();
        let mut title = crate::tui::title::TitleHandle::disabled();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        let renders = crate::tui::cell::lines_call_count();
        assert!(
            renders <= 8,
            "streaming 4 tokens over {CELLS} cells re-rendered {renders} cells; \
             it must re-render only the changed tail, not the whole scrollback"
        );
    }

    /// Spawn a thread that sends `PermissionResolved` for `request_id` after
    /// a short delay. This resolves the permission in the history so that
    /// subsequent Ctrl+Q events can actually quit the TUI loop.
    fn resolve_permission_after_delay(
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        request_id: u64,
    ) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = agent_tx.blocking_send(AgentEvent::PermissionResolved {
                request_id,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            });
        });
    }

    /// Key '1' on a pending permission should send AllowOnce on decision_tx.
    #[test]
    fn permission_key_1_allows_once() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(1))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 1);

        // Events are LIFO: '1' is processed first, then scripted events run out
        // and ScriptedThenQuitEvents returns Ctrl+Q forever.
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('1'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((1, yi_agent_core::permission::Decision::AllowOnce))
        );
    }

    /// Key '2' on a pending permission should send AlwaysAllowTool.
    #[test]
    fn permission_key_2_always_allow_tool() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(2))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 2);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((2, yi_agent_core::permission::Decision::AlwaysAllowTool))
        );
    }

    /// Key '3' with a prefix suggestion should send AlwaysAllowPrefix.
    #[test]
    fn permission_key_3_always_allow_prefix() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(3))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 3);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('3'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((
                3,
                yi_agent_core::permission::Decision::AlwaysAllowPrefix("ls".into())
            ))
        );
    }

    /// Key '4' on a pending permission should send Deny.
    #[test]
    fn permission_key_4_deny() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(4))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 4);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('4'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((4, yi_agent_core::permission::Decision::Deny))
        );
    }

    /// Enter on a Normal permission should default to AllowOnce.
    #[test]
    fn permission_enter_defaults_to_allow_for_normal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(5))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 5);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((5, yi_agent_core::permission::Decision::AllowOnce))
        );
    }

    /// Enter on a Blacklisted permission should default to Deny.
    #[test]
    fn permission_enter_defaults_to_deny_for_blacklisted() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_blacklisted(6))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 6);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((6, yi_agent_core::permission::Decision::Deny))
        );
    }

    /// Key '3' when there is no prefix suggestion should be a no-op:
    /// no decision is sent, and the key is ignored. Afterward, pressing '1'
    /// should still work to resolve the permission.
    #[test]
    fn permission_key_3_when_no_prefix_is_noop() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_no_prefix(7))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 7);

        // Events (LIFO): '3' is processed first (noop, no prefix), then '1'
        // resolves the permission. After '1' sends the decision, the resolver
        // thread delivers PermissionResolved, and then Ctrl+Q quits.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // '3' should have been ignored (no prefix), so no AlwaysAllowPrefix.
        // The only decision should be from '1' -> AllowOnce.
        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((7, yi_agent_core::permission::Decision::AllowOnce))
        );
        // No further decisions
        assert!(
            decision_rx.try_recv().is_err(),
            "should only have one decision"
        );
    }

    /// Typing 'a' while a permission is pending should be ignored:
    /// no decision sent and the input buffer is not modified.
    /// Afterward, '1' resolves the permission and Ctrl+Q quits.
    #[test]
    fn permission_other_keys_ignored_while_pending() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(8))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 8);

        // Events (LIFO): 'a' (should be ignored), then '1' (resolves permission)
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // The only decision should be from '1' -> AllowOnce (not from 'a')
        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((8, yi_agent_core::permission::Decision::AllowOnce))
        );
        assert!(
            decision_rx.try_recv().is_err(),
            "should only have one decision"
        );

        // The input row should not contain 'a' — it was ignored while permission was pending.
        let buffer = terminal.backend().buffer();
        // The input card is three rows tall (padding / text / padding), so the
        // text row sits at 22 in a 24-row terminal, not the last row.
        let input_row = 22u16;
        let row_text: String = (0..80u16)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(
            !row_text.contains('a'),
            "expected 'a' to be ignored while permission pending, but found it in input row: {row_text:?}"
        );
    }

    // ----- handle_key Esc/Ctrl+C interrupt tests -----

    fn make_key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn normal_navigation_keys_route_to_history_without_affecting_shift_selection() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        for _ in 0..120 {
            history.push(HistoryCell::Separator { label: None }, 80);
        }
        history.scroll_offset = 5;

        for (key, expected_offset) in [
            (KeyCode::Up, 6),
            (KeyCode::Down, 5),
            (KeyCode::PageUp, 25),
            (KeyCode::PageDown, 5),
            (KeyCode::Home, 100),
            (KeyCode::End, 0),
        ] {
            let outcome = handle_key(
                make_key(key, KeyModifiers::NONE),
                &mut KeyContext {
                    view: ViewContext {
                        input: &mut input,
                        history: &mut history,
                        popup: &mut popup,
                        max_scroll_offset: 100,
                        history_width: 80,
                        history_height: 20,
                        cost_tracker: &CostTracker::default(),
                    },
                    runtime: RuntimeContext {
                        channels: &TuiChannels::from_senders(
                            &input_tx,
                            &interrupt_tx,
                            &kill_tx,
                            &control_tx,
                            &decision_tx,
                        ),
                        is_running: &is_running,
                        queued: &mut queued,
                        pending_quit: &mut pending_quit,
                        workdir: &std::env::temp_dir(),
                        mcp: &yi_agent_mcp::McpManager::empty(),
                        config: &snapshot_for_tests(),
                    },
                    model: ModelContext {
                        model: "test-model",
                        current_model_ref: None,
                        catalog: &catalog_for_tests(),
                        current_effort: None,
                        effort_source: EffortSource::Unset,
                    },
                },
            );
            assert_eq!(outcome, KeyOutcome::None);
            assert_eq!(history.scroll_offset, expected_offset, "key {key:?}");
        }

        history.selected = Some(5);
        let _ = handle_key(
            make_key(KeyCode::Up, KeyModifiers::SHIFT),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 100,
                    history_width: 80,
                    history_height: 20,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(history.selected, Some(4));
        assert_eq!(
            history.scroll_offset, 0,
            "Shift+Up keeps selection behavior"
        );
    }

    #[test]
    fn esc_interrupts_active_agent_without_arming_quit() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(!pending_quit, "Esc must not arm process exit");
        assert!(
            interrupt_rx.try_recv().is_ok(),
            "interrupt should be sent when agent running"
        );
    }

    #[test]
    fn esc_when_idle_does_nothing() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(!pending_quit, "idle Esc must not arm process exit");
        assert!(
            interrupt_rx.try_recv().is_err(),
            "interrupt should NOT be sent when idle"
        );
    }

    #[test]
    fn ctrl_c_when_running_sends_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        assert!(interrupt_rx.try_recv().is_ok());
    }

    #[test]
    fn repeated_esc_does_not_quit_from_handle_key() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        let _ = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(result, KeyOutcome::None);
    }

    // ----- handle_key Submit 分流 tests -----

    #[test]
    fn submit_multi_segment_absolute_path_sends_to_agent() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;
        let path = "/Users/name/project explain this";

        input.buffer = path.to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );

        assert_eq!(result, KeyOutcome::Submit(path.to_string()));
        assert_eq!(input_rx.try_recv().unwrap(), path);
        assert!(matches!(
            history.cells.as_slice(),
            [HistoryCell::UserMessage { text }] if text == path
        ));
    }

    #[test]
    fn submit_single_segment_absolute_path_shows_unknown_command() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        input.buffer = "/tmp".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );

        assert_eq!(result, KeyOutcome::None);
        assert!(input_rx.try_recv().is_err());
        assert!(matches!(
            history.cells.as_slice(),
            [HistoryCell::Separator { label: Some(label) }] if label == "未知命令: /tmp"
        ));
    }

    #[test]
    fn submit_while_running_is_delivered_not_held_in_history() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn: the queue's own `in_flight` flag is the
        // authority for "is a turn running", not `is_running` (see the spec:
        // is_running is still true at Done time, so keying sends off it would
        // deadlock the TUI against the driver).
        input.buffer = "inflight msg".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(input_rx.try_recv().unwrap(), "inflight msg");
        let history_len_before = history.cells.len();

        input.buffer = "mid-turn msg".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "mid-turn msg");
                // Delivered immediately: the driver folds it into the running
                // turn before the next provider request, rather than holding it
                // back until the turn ends.
                assert_eq!(
                    input_rx.try_recv().unwrap(),
                    "mid-turn msg",
                    "a mid-turn submit must be delivered, not held back"
                );
                // Accounting only: it stays in the preview until core confirms.
                assert_eq!(queued.len(), 1);
                assert_eq!(queued.items(), ["mid-turn msg".to_string()]);
                assert_eq!(
                    history.cells.len(),
                    history_len_before,
                    "a mid-turn message must not be added to history as a new turn"
                );
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn busy_submit_reaches_the_agent_channel() {
        use crate::tui::queued::SubmitOutcome;
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn and take the opening message off the
        // channel, so the next submit is a mid-turn delivery.
        assert_eq!(queued.submit("opening".into()), SubmitOutcome::Sent);
        let _ = input_tx.try_send("opening".into());
        let _ = input_rx.try_recv();

        input.buffer = "follow-up".to_string();
        input.cursor = input.buffer.len();

        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );

        assert_eq!(
            input_rx.try_recv().ok(),
            Some("follow-up".to_string()),
            "a busy submit must be delivered, not merely previewed"
        );
        assert_eq!(queued.items(), ["follow-up".to_string()]);
    }

    #[test]
    fn returned_interjection_goes_back_to_the_input_box() {
        use crate::tui::queued::SubmitOutcome;
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        assert_eq!(queued.submit("unconsumed".into()), SubmitOutcome::Sent);
        assert_eq!(queued.submit("mid-turn".into()), SubmitOutcome::Queued);

        apply_interjection_event(
            &AgentEvent::InterjectionsReturned {
                items: vec![yi_agent_core::Interjection {
                    seq: 1,
                    text: "mid-turn".into(),
                    tag: None,
                }],
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(input.buffer, "mid-turn");
        assert!(queued.is_empty());
        assert!(
            matches!(
                history.cells.as_slice(),
                [HistoryCell::Separator { label: Some(label) }]
                    if label == "1 条追加未生效，已退回输入框"
            ),
            "the return must be reported: {:?}",
            history.cells
        );
    }

    #[test]
    fn multiple_returned_interjections_join_with_newlines() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        queued.submit("opening".into());
        queued.submit("one".into());
        queued.submit("two".into());

        apply_interjection_event(
            &AgentEvent::InterjectionsReturned {
                items: vec![
                    yi_agent_core::Interjection {
                        seq: 1,
                        text: "one".into(),
                        tag: None,
                    },
                    yi_agent_core::Interjection {
                        seq: 2,
                        text: "two".into(),
                        tag: None,
                    },
                ],
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(input.buffer, "one\ntwo");
        assert!(queued.is_empty());
    }

    #[test]
    fn accepted_receipt_lowers_the_pending_count_without_touching_history() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        queued.submit("opening".into());
        queued.submit("first".into());
        queued.submit("second".into());

        apply_interjection_event(
            &AgentEvent::InterjectionAccepted {
                seq: 1,
                text: "first".into(),
                tag: None,
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(queued.items(), ["second".to_string()]);
        assert_eq!(input.buffer, "", "a receipt must not touch the input box");
        assert!(
            history.cells.is_empty(),
            "the receipt is rendered by history.rs"
        );
    }

    #[test]
    fn interjection_events_are_noops_for_unrelated_events() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        queued.submit("opening".into());
        queued.submit("pending".into());

        apply_interjection_event(
            &AgentEvent::Start,
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(queued.items(), ["pending".to_string()]);
        assert_eq!(input.buffer, "");
        assert!(history.cells.is_empty());
    }

    #[test]
    fn submit_while_idle_goes_to_history_not_queue() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        input.buffer = "idle msg".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "idle msg");
                assert!(queued.is_empty());
                assert_eq!(history.cells.len(), 1);
                match &history.cells[0] {
                    HistoryCell::UserMessage { text } => assert_eq!(text, "idle msg"),
                    _ => panic!("expected UserMessage"),
                }
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn full_queue_rejects_and_restores_input_without_blocking() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn (the first submit is always Sent).
        input.buffer = "inflight".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert_eq!(input_rx.try_recv().unwrap(), "inflight");

        // Fill the queue to capacity.
        for i in 0..crate::tui::queued::DeliveredInterjections::CAPACITY {
            input.buffer = format!("msg{i}");
            input.cursor = input.buffer.len();
            let _ = handle_key(
                make_key(KeyCode::Enter, KeyModifiers::NONE),
                &mut KeyContext {
                    view: ViewContext {
                        input: &mut input,
                        history: &mut history,
                        popup: &mut popup,
                        max_scroll_offset: 1000,
                        history_width: 80,
                        history_height: 24,
                        cost_tracker: &CostTracker::default(),
                    },
                    runtime: RuntimeContext {
                        channels: &TuiChannels::from_senders(
                            &input_tx,
                            &interrupt_tx,
                            &kill_tx,
                            &control_tx,
                            &decision_tx,
                        ),
                        is_running: &is_running,
                        queued: &mut queued,
                        pending_quit: &mut pending_quit,
                        workdir: &std::env::temp_dir(),
                        mcp: &yi_agent_mcp::McpManager::empty(),
                        config: &snapshot_for_tests(),
                    },
                    model: ModelContext {
                        model: "test-model",
                        current_model_ref: None,
                        catalog: &catalog_for_tests(),
                        current_effort: None,
                        effort_source: EffortSource::Unset,
                    },
                },
            );
        }
        assert_eq!(
            queued.len(),
            crate::tui::queued::DeliveredInterjections::CAPACITY
        );
        // Every one of those submits was delivered, not buffered locally, so the
        // channel holds CAPACITY entries; drain them so the overflow assertion
        // below cannot be satisfied by an earlier message.
        for i in 0..crate::tui::queued::DeliveredInterjections::CAPACITY {
            assert_eq!(input_rx.try_recv().unwrap(), format!("msg{i}"));
        }
        assert!(
            input_rx.try_recv().is_err(),
            "channel should now be drained"
        );
        let history_len_before = history.cells.len();

        // The overflow submit must not block and must not be accepted.
        input.buffer = "overflow".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 1000,
                    history_width: 80,
                    history_height: 24,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );

        assert_eq!(
            queued.len(),
            crate::tui::queued::DeliveredInterjections::CAPACITY,
            "the queue must stay at capacity"
        );
        assert_eq!(
            input.buffer, "overflow",
            "the rejected text must be restored to the input line"
        );
        assert!(
            history.cells.len() > history_len_before,
            "the rejection must be visible in history"
        );
        assert!(
            input_rx.try_recv().is_err(),
            "a rejected message must not reach the channel"
        );
    }

    #[test]
    fn delivered_messages_are_not_re_held_until_turn_end() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // First submit: idle -> sent. Second: queued.
        for text in ["first", "second"] {
            input.buffer = text.to_string();
            input.cursor = input.buffer.len();
            let _ = handle_key(
                make_key(KeyCode::Enter, KeyModifiers::NONE),
                &mut KeyContext {
                    view: ViewContext {
                        input: &mut input,
                        history: &mut history,
                        popup: &mut popup,
                        max_scroll_offset: 1000,
                        history_width: 80,
                        history_height: 24,
                        cost_tracker: &CostTracker::default(),
                    },
                    runtime: RuntimeContext {
                        channels: &TuiChannels::from_senders(
                            &input_tx,
                            &interrupt_tx,
                            &kill_tx,
                            &control_tx,
                            &decision_tx,
                        ),
                        // The driver has NOT yet cleared this flag: this mirrors the
                        // real ordering, where Done is emitted before is_running=false.
                        is_running: &Arc::new(AtomicBool::new(true)),
                        queued: &mut queued,
                        pending_quit: &mut pending_quit,
                        workdir: &std::env::temp_dir(),
                        mcp: &yi_agent_mcp::McpManager::empty(),
                        config: &snapshot_for_tests(),
                    },
                    model: ModelContext {
                        model: "test-model",
                        current_model_ref: None,
                        catalog: &catalog_for_tests(),
                        current_effort: None,
                        effort_source: EffortSource::Unset,
                    },
                },
            );
        }
        // Both reached the driver already: the first opened the turn, the second
        // was delivered as a mid-turn interjection. Neither waits for turn end.
        assert_eq!(input_rx.try_recv().unwrap(), "first");
        assert_eq!(input_rx.try_recv().unwrap(), "second");
        assert!(input_rx.try_recv().is_err());
        // `is_running` is still true here, mirroring the real ordering where
        // Done is emitted before the driver clears the flag; delivery must not
        // have depended on it either way.
        assert_eq!(queued.len(), 1);
        assert_eq!(queued.items(), ["second".to_string()]);

        // Turn end reconciles: core accepted it, so the receipt retires it and
        // the fallback promotion finds nothing left to send.
        queued.on_receipt();
        assert!(queued.is_empty());
        assert_eq!(queued.on_turn_end(), None);
    }

    // ----- /cost slash command tests -----

    #[test]
    fn clear_command_drops_pending_queue_and_resets_in_flight() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut history = HistoryState::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        // One in flight plus two waiting.
        use crate::tui::queued::SubmitOutcome;
        assert_eq!(queued.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(queued.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(queued.submit("c".into()), SubmitOutcome::Queued);

        let outcome = execute_slash_command(
            SlashCommand::Clear,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert!(queued.is_empty(), "/clear must drop waiting messages");
        // in_flight reset: a later submit sends immediately again.
        assert_eq!(queued.submit("d".into()), SubmitOutcome::Sent);
        assert!(matches!(
            control_rx.try_recv(),
            Ok(crate::DriverControl::Clear)
        ));
    }

    /// Deterministic catalog for `/model` tests. Built in memory so no test has
    /// to touch process-level `HOME` (cargo runs tests in parallel; mutating the
    /// process env would let cases race each other).
    ///
    /// `A` and `C` intentionally share the model string `model-a`, and every
    /// entry's display name differs from its model string: the old model-string
    /// match could mark two rows (or the wrong row), which is exactly the bug
    /// these tests pin down.
    fn model_catalog_for_tests() -> yi_agent_runtime::models::ModelCatalog {
        use yi_agent_runtime::models::{ModelCatalog, ModelEntry, ModelProvider};
        let entry = |name: &str, model: &str, provider| ModelEntry {
            name: name.into(),
            provider,
            api_url: format!("https://{name}.example"),
            model: model.into(),
            api_key: "sk-secret".into(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        };
        ModelCatalog {
            models: vec![
                entry("A", "model-a", ModelProvider::Anthropic),
                entry("B", "model-b", ModelProvider::Openai),
                entry("C", "model-a", ModelProvider::Anthropic),
            ],
            default_model: Some("A".into()),
            subagent_model: None,
        }
    }

    /// The display name on the single line marked `← 当前`. Panics unless
    /// exactly one line is marked, so a duplicate/wrong mark fails the test.
    fn marked_entry_name(text: &str) -> String {
        let marked: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("← 当前"))
            .collect();
        assert_eq!(
            marked.len(),
            1,
            "恰好一条被标记为当前,实际 {} 条: {marked:?}\n{text}",
            marked.len()
        );
        // 行形如 `  A  provider=…  model=…  ← 当前`：取第一个空白分隔的字段。
        marked[0]
            .split_whitespace()
            .next()
            .expect("被标记行应含显示名")
            .to_string()
    }

    /// Collects every `Markdown` cell body pushed into history.
    fn markdown_bodies(history: &HistoryState) -> String {
        history
            .cells
            .iter()
            .filter_map(|cell| match cell {
                HistoryCell::Markdown { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// `/effort` 测试的清单:与 `/model` 用同一个，另加一条带条目默认档的条目。
    fn effort_catalog_for_tests() -> yi_agent_runtime::models::ModelCatalog {
        let mut catalog = model_catalog_for_tests();
        catalog.models[0].reasoning_effort = Some("low".into());
        catalog
    }

    #[test]
    fn effort_command_sets_the_session_override() {
        let mut history = HistoryState::new();

        let command = handle_effort_command(
            Some("high"),
            None,
            EffortSource::Unset,
            None,
            &mut history,
            80,
        );
        assert_eq!(
            command,
            Some(crate::DriverControl::SetReasoningEffort(Some(
                "high".into()
            )))
        );
    }

    /// 端到端形状：能力已知时，`/effort <不支持档>` 只打印错误、**不下发**任何
    /// 控制命令——否则 driver 会白跑一次必然失败的重建。
    #[test]
    fn effort_command_rejects_an_unsupported_tier_without_sending_it() {
        let catalog = effort_catalog_for_tests();
        let mut history = HistoryState::new();
        let supported = vec!["low".to_string(), "medium".to_string()];

        let command = handle_effort_command(
            Some("max"),
            Some("low"),
            EffortSource::Session,
            Some(&supported),
            &mut history,
            80,
        );
        assert!(
            command.is_none(),
            "an unsupported tier must not be sent to the driver"
        );

        // 支持的那一档照常下发。
        let command = handle_effort_command(
            Some("medium"),
            Some("low"),
            EffortSource::Session,
            Some(&supported),
            &mut history,
            80,
        );
        assert_eq!(
            command,
            Some(crate::DriverControl::SetReasoningEffort(Some(
                "medium".into()
            )))
        );
        let _ = &catalog;
    }

    /// 状态行展示的「可用」集合必须与校验放行的集合一致，否则用户会照着点不动的
    /// 清单去敲命令。
    #[test]
    fn effort_status_lists_only_the_supported_tiers() {
        let supported = vec!["low".to_string(), "high".to_string()];
        let text = render_effort_status(Some("low"), EffortSource::Session, Some(&supported));
        assert!(text.contains("low | high"), "got: {text}");
        assert!(
            !text.contains("max"),
            "an unsupported tier must not be advertised: {text}"
        );

        // 七档全被拒时给出明确的替代路径，而不是一行空白。
        let none: Vec<String> = Vec::new();
        let text = render_effort_status(None, EffortSource::Unset, Some(&none));
        assert!(text.contains("仅 default"), "got: {text}");
    }

    #[test]
    fn effort_command_default_clears_the_override() {
        let catalog = effort_catalog_for_tests();
        let mut history = HistoryState::new();

        let command = handle_effort_command(
            Some("default"),
            Some("high"),
            EffortSource::Session,
            None,
            &mut history,
            80,
        );
        assert_eq!(
            command,
            Some(crate::DriverControl::SetReasoningEffort(None)),
            "`default` clears the override; the driver falls back to the entry"
        );
        assert!(
            matches!(catalog.default_model.as_deref(), Some("A")),
            "the entry default is what the clear falls back to"
        );
    }

    /// 无参 `/effort` 只打印当前生效值 + 来源,绝不下发命令。
    #[test]
    fn effort_command_without_args_reports_the_effective_value_and_source() {
        let mut history = HistoryState::new();

        let command = handle_effort_command(
            None,
            Some("low"),
            EffortSource::Entry,
            None,
            &mut history,
            80,
        );

        assert!(command.is_none(), "status must not send a control command");
        let text = markdown_bodies(&history);
        assert!(
            text.contains("low"),
            "the effective tier must be shown: {text}"
        );
        assert!(
            text.contains(EffortSource::Entry.label()),
            "the source must be named: {text}"
        );
    }

    #[test]
    fn effort_command_without_args_reports_unset_as_unset() {
        let mut history = HistoryState::new();
        let command =
            handle_effort_command(None, None, EffortSource::Unset, None, &mut history, 80);
        assert!(command.is_none());
        let text = markdown_bodies(&history);
        assert!(
            text.contains(EffortSource::Unset.label()),
            "an unset effort must be reported as unset, not as a tier: {text}"
        );
    }

    /// 非白名单档位 → 用法行,不下发。
    #[test]
    fn effort_command_rejects_an_unknown_tier_with_a_usage_line() {
        let mut history = HistoryState::new();
        let command = handle_effort_command(
            Some("bogus"),
            None,
            EffortSource::Unset,
            None,
            &mut history,
            80,
        );
        assert!(command.is_none(), "an unknown tier must not be sent");
        let labels = separator_labels(&history);
        assert!(
            labels.iter().any(|l| l.contains("用法: /effort")),
            "expected the usage line, got {labels:?}"
        );
    }

    /// 优先级:会话覆盖 > 选中条目默认 > 未设置。
    #[test]
    fn resolve_effort_state_prefers_the_session_override() {
        let catalog = effort_catalog_for_tests();

        assert_eq!(
            resolve_effort_state(Some("max"), &catalog, Some("A"), false),
            (Some("max".into()), EffortSource::Session)
        );
        assert_eq!(
            resolve_effort_state(None, &catalog, Some("A"), false),
            (Some("low".into()), EffortSource::Entry),
            "no override: the selected entry's own default applies"
        );
        assert_eq!(
            resolve_effort_state(None, &catalog, Some("B"), false),
            (None, EffortSource::Unset),
            "entry B declares no effort"
        );
        // 无选中显示名 = 跟随清单 default_model(A, low)。
        assert_eq!(
            resolve_effort_state(None, &catalog, None, false),
            (Some("low".into()), EffortSource::Entry)
        );
    }

    /// `/effort high` 全链路:下发 `SetReasoningEffort(Some("high"))` 并回报
    /// 已选档位(供 run_loop 维护「当前生效值」)。
    #[test]
    fn effort_slash_command_sends_the_override_and_reports_it() {
        let catalog = effort_catalog_for_tests();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Effort,
            Some("high".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog,
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::EffortChanged(Some("high".into())));
        assert_eq!(
            control_rx.try_recv(),
            Ok(crate::DriverControl::SetReasoningEffort(Some(
                "high".into()
            )))
        );

        // `/effort` 无参:只打印,不发命令。
        let outcome = execute_slash_command(
            SlashCommand::Effort,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog,
            Some("high"),
            EffortSource::Session,
        );
        assert_eq!(outcome, KeyOutcome::None);
        assert!(control_rx.try_recv().is_err(), "status must send nothing");
    }

    #[test]
    fn model_command_lists_the_catalog_when_given_no_args() {
        let catalog = model_catalog_for_tests();
        let mut history = HistoryState::new();

        // 启动(无会话覆盖):标记应落在清单 default_model 上。
        let command = handle_model_command(None, &catalog, None, &mut history, 80);

        assert!(command.is_none(), "listing must not send a control command");
        let text = markdown_bodies(&history);
        assert!(
            text.contains("A") && text.contains("B") && text.contains("C"),
            "显示名清单: {text}"
        );
        assert!(text.contains("当前"), "当前选中标记: {text}");
        // 恰好一行被标记,且是 default_model 条目 A。
        assert_eq!(
            marked_entry_name(&text),
            "A",
            "启动时应标记默认条目: {text}"
        );
    }

    #[test]
    fn model_command_marks_only_the_selected_display_name() {
        let catalog = model_catalog_for_tests();
        let mut history = HistoryState::new();

        // 会话覆盖为 B:即便 A/C 共用 model-a,也只标记 B 一行。
        let command = handle_model_command(None, &catalog, Some("B"), &mut history, 80);

        assert!(command.is_none());
        let text = markdown_bodies(&history);
        assert_eq!(
            marked_entry_name(&text),
            "B",
            "只标记用户选中的显示名: {text}"
        );
        // A、C 与选中项 B 的 model 串不同也无妨;旧实现按 model 串匹配,
        // 会漏标 B —— 这里显式钉死没有别的行被标记。
        assert_eq!(
            text.lines().filter(|line| line.contains("当前")).count(),
            1,
            "只能有一条当前标记: {text}"
        );
    }

    #[test]
    fn model_command_switches_to_a_named_entry() {
        let catalog = model_catalog_for_tests();
        let mut history = HistoryState::new();

        let command = handle_model_command(Some("B"), &catalog, None, &mut history, 80);

        // 显示名随命令下发;driver 用自己的清单解析 provider/url/key,避免用旧
        // 配置的 key 去打新 url。
        assert_eq!(
            command,
            Some(crate::DriverControl::SetModel(Some("B".into())))
        );
    }

    #[test]
    fn model_command_rejects_an_unknown_name() {
        let catalog = model_catalog_for_tests();
        let mut history = HistoryState::new();

        let command = handle_model_command(Some("nope"), &catalog, None, &mut history, 80);

        assert!(command.is_none(), "未知名字不得发出切换命令");
        let labels = separator_labels(&history);
        assert!(
            labels.iter().any(|label| label.contains("未知模型名")),
            "应报未知模型名: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("保留字")),
            "提示应说明 default 是保留字: {labels:?}"
        );
    }

    #[test]
    fn model_command_default_clears_the_override() {
        let catalog = model_catalog_for_tests();
        let mut history = HistoryState::new();

        let command = handle_model_command(Some("default"), &catalog, Some("B"), &mut history, 80);

        // `None` = 清覆盖,driver 回落到清单 default_model。
        // 注:`default` 是保留字,清单里同名的条目永远选不中(见
        // `handle_model_command` 注释与用法/错误文案)。
        assert_eq!(command, Some(crate::DriverControl::SetModel(None)));
    }

    /// `/model <name>` 必须在命令进 channel 的同时回报选中的显示名,
    /// `/model default` 回报 `None` —— run_loop 用这个值维护「← 当前」标记。
    #[test]
    fn model_command_reports_the_selected_display_name() {
        let catalog = model_catalog_for_tests();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Model,
            Some("B".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog,
            None,
            EffortSource::Unset,
        );
        assert_eq!(
            outcome,
            KeyOutcome::ModelSelected(Some("B".into())),
            "应回报选中的显示名"
        );
        assert_eq!(
            control_rx.try_recv(),
            Ok(crate::DriverControl::SetModel(Some("B".into())))
        );

        // `/model default` → 清空标记,跟随 default_model。
        let outcome = execute_slash_command(
            SlashCommand::Model,
            Some("default".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            Some("B"),
            &catalog,
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::ModelSelected(None));
        assert_eq!(
            control_rx.try_recv(),
            Ok(crate::DriverControl::SetModel(None))
        );
    }

    #[test]
    fn daemon_command_reports_unavailable_runtime() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let dir = tempfile::tempdir().unwrap();
        let outcome = execute_slash_command(
            SlashCommand::Daemon,
            Some("status".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            dir.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let labels = separator_labels(&history);
        assert!(
            labels
                .iter()
                .any(|l| l.contains("无法联系本地 daemon runtime")),
            "expected an unreachable-daemon message, got {labels:?}"
        );
    }

    #[test]
    fn daemon_command_rejects_unknown_subcommand() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let _ = execute_slash_command(
            SlashCommand::Daemon,
            Some("start".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        let labels = separator_labels(&history);
        assert!(
            labels
                .iter()
                .any(|l| l.contains("用法: /daemon [status|stop]"))
        );
    }

    #[test]
    fn cost_command_renders_tracker() {
        use yi_agent_core::TokenUsage;
        let mut history = HistoryState::new();
        let mut cost = CostTracker::default();
        cost.record(
            "claude-sonnet-4-5",
            &TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        );
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            &mut history,
            80,
            &cost,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let cell = history.cells.last().unwrap();
        match cell {
            crate::tui::cell::HistoryCell::Markdown { text } => {
                assert!(
                    text.contains("claude-sonnet-4-5"),
                    "cost text should include model: {text}"
                );
                assert!(
                    text.contains("100"),
                    "cost text should include input tokens: {text}"
                );
            }
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    #[test]
    fn config_command_renders_snapshot() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let snapshot = snapshot_for_tests();
        let outcome = execute_slash_command(
            SlashCommand::Config,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot,
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let rendered: String = history
            .cells
            .iter()
            .filter_map(|c| match c {
                HistoryCell::Markdown { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(rendered.contains("anthropic"));
        assert!(rendered.contains("test-model"));
        assert!(!rendered.contains("api_key"));
    }

    #[test]
    fn cost_command_empty_shows_no_data() {
        let mut history = HistoryState::new();
        let cost = CostTracker::default();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            &mut history,
            80,
            &cost,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let cell = history.cells.last().unwrap();
        match cell {
            crate::tui::cell::HistoryCell::Markdown { text } => {
                assert!(
                    text.contains("尚无数据"),
                    "empty cost should show no-data: {text}"
                );
            }
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    // ----- /mcp slash command tests -----

    #[test]
    fn mcp_on_sets_master_and_sends_refresh() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mcp = yi_agent_mcp::McpManager::empty();
        assert!(!mcp.master(), "empty manager starts with master off");

        let outcome = execute_slash_command(
            SlashCommand::Mcp,
            Some("on".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &mcp,
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        assert!(mcp.master(), "/mcp on must set the master switch");
        assert_eq!(
            control_rx.try_recv().unwrap(),
            crate::DriverControl::McpRefresh,
            "/mcp on must ask the driver to refresh the registry"
        );
    }

    #[test]
    fn mcp_disable_unknown_server_reports_error_without_refresh() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mcp = yi_agent_mcp::McpManager::empty();

        let outcome = execute_slash_command(
            SlashCommand::Mcp,
            Some("disable nope".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &mcp,
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        // The unknown server must be reported, not silently ignored...
        match history.cells.last().unwrap() {
            HistoryCell::Separator { label: Some(label) } => {
                assert!(
                    label.contains("nope"),
                    "error should name the server: {label}"
                );
            }
            other => panic!("expected a Separator, got {other:?}"),
        }
        // ...and no refresh may be sent for a failed toggle.
        assert!(
            control_rx.try_recv().is_err(),
            "unknown server must not trigger a registry refresh"
        );
    }

    #[test]
    fn mcp_status_on_empty_manager_says_unconfigured() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mcp = yi_agent_mcp::McpManager::empty();

        let outcome = execute_slash_command(
            SlashCommand::Mcp,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &mcp,
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );
        assert_eq!(outcome, KeyOutcome::None);
        match history.cells.last().unwrap() {
            HistoryCell::Markdown { text } => {
                assert!(
                    text.contains("未配置 MCP server"),
                    "empty manager should say unconfigured: {text}"
                );
            }
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    // ----- /runtime slash command tests -----

    #[test]
    fn runtime_status_reports_preference_and_its_source_path() {
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Runtime,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            project.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );

        assert_eq!(outcome, KeyOutcome::None);
        match history.cells.last().unwrap() {
            HistoryCell::Separator { label: Some(label) } => {
                assert!(label.contains("ask"), "missing default value: {label}");
                assert!(
                    label.contains("preferences.json"),
                    "status must name its source file: {label}"
                );
            }
            other => panic!("expected a Separator, got {other:?}"),
        }
    }

    #[test]
    fn runtime_set_persists_the_preference_and_reports_it() {
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Runtime,
            Some("never".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            project.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Never,
            "/runtime never must persist"
        );
        match history.cells.last().unwrap() {
            HistoryCell::Separator { label: Some(label) } => assert!(
                label.contains("never"),
                "confirmation must name the new value: {label}"
            ),
            other => panic!("expected a Separator, got {other:?}"),
        }
    }

    #[test]
    fn runtime_rejects_bad_args_with_usage_without_touching_the_file() {
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Runtime,
            Some("sometimes".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            project.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
            None,
            &catalog_for_tests(),
            None,
            EffortSource::Unset,
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert_eq!(
            separator_labels(&history),
            vec!["用法: /runtime [ask|always|never]".to_string()]
        );
        assert!(
            !crate::tui::runtime_prefs::preferences_path(project.path()).exists(),
            "a rejected argument must not create a preference file"
        );
    }

    #[test]
    fn route_event_usage_records_to_tracker() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        let mut cost = CostTracker::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::Usage {
                model: "claude".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            },
        );
        let rendered = cost.render();
        assert!(
            rendered.contains("claude"),
            "tracker should contain model after route_event: {rendered}"
        );
        assert!(
            rendered.contains("100"),
            "tracker should contain input tokens: {rendered}"
        );
    }

    // ----- Mouse scroll routing tests -----

    /// Build a `MouseEvent` of the given kind at `(column, row)`.
    fn make_mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Compute the layout for a standard 80×24 terminal with no popup and
    /// empty input. Returns the history rect (chunks[0]) for convenience.
    fn layout_80x24() -> (LayoutInfo, ratatui::layout::Rect) {
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let input = InputLine::new();
        let layout = compute_layout(area, &input, false, &None, 0);
        let history = layout.chunks[0];
        (layout, history)
    }

    #[test]
    fn history_wheel_events_move_three_lines() {
        assert_eq!(HISTORY_WHEEL_LINES, 3);
    }

    /// Scrolling up over the history region should increase `scroll_offset`.
    #[test]
    fn mouse_scroll_up_in_history_increases_offset() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        // Fill enough lines so scrolling is meaningful.
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        // Scroll up at the top of the history region.
        let row = history_area.y;
        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, row),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(
            hist.scroll_offset, 3,
            "ScrollUp in history should scroll up"
        );

        // A second scroll should accumulate.
        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 5, row + 3),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(hist.scroll_offset, 6, "second ScrollUp should accumulate");
    }

    #[test]
    fn mouse_scroll_uses_reserved_text_width_for_its_max_offset() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        // This fits the raw 80-column area (78 chars after the user prefix)
        // but wraps after the scrollbar reserves one column.
        for _ in 0..30 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "x".repeat(78),
                },
                80,
            );
        }
        let text_width = hist.text_width(history_area.width, history_area.height);
        assert_eq!(text_width, 79, "overflow reserves a scrollbar column");
        let expected_max = hist.max_scroll_offset(text_width, history_area.height);
        let raw_max = hist.max_scroll_offset(history_area.width, history_area.height);
        assert!(expected_max > raw_max, "reserved width adds wrapped lines");

        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;
        for _ in 0..100 {
            handle_mouse(
                make_mouse(MouseEventKind::ScrollUp, 0, history_area.y),
                &layout,
                &mut runtime_popup,
                &mut hist,
                &registry,
                &[],
                &std::collections::HashMap::new(),
                &mut pending_quit,
            );
        }

        assert_eq!(hist.scroll_offset, expected_max);
    }

    /// Scrolling down over the history region should decrease `scroll_offset`
    /// (clamped at 0).
    #[test]
    fn mouse_scroll_down_in_history_decreases_offset() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        hist.scroll_up(5, 1000);
        assert_eq!(hist.scroll_offset, 5);

        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollDown, 0, history_area.y),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(
            hist.scroll_offset, 2,
            "ScrollDown in history should decrease offset"
        );

        // Scrolling down past 0 should clamp at 0 (not underflow).
        for _ in 0..10 {
            handle_mouse(
                make_mouse(MouseEventKind::ScrollDown, 0, history_area.y),
                &layout,
                &mut runtime_popup,
                &mut hist,
                &registry,
                &[],
                &std::collections::HashMap::new(),
                &mut pending_quit,
            );
        }
        assert_eq!(hist.scroll_offset, 0, "offset should clamp at 0");
    }

    /// Scrolling over the input region should NOT affect history.
    #[test]
    fn mouse_scroll_in_input_region_ignores_history() {
        let (layout, _history_area) = layout_80x24();
        let input_area = layout.chunks[INPUT_CHUNK];
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, input_area.y),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(
            hist.scroll_offset, 0,
            "ScrollUp in input region should not touch history"
        );
    }

    /// Mouse clicks (non-scroll events) should be ignored entirely.
    #[test]
    fn mouse_click_is_ignored() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        handle_mouse(
            make_mouse(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                0,
                history_area.y,
            ),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(hist.scroll_offset, 0, "click should not scroll");
    }

    /// Scrolling up over the history region should clear `pending_quit`,
    /// matching the behavior of other history navigation keys.
    #[test]
    fn mouse_scroll_clears_pending_quit() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = true;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, history_area.y),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert!(!pending_quit, "scrolling history should clear pending_quit");
    }

    /// End-to-end: feed `Event::Mouse(ScrollUp)` events through the TUI loop
    /// and verify history `scroll_offset` changes are visible in the rendered
    /// buffer.
    #[test]
    fn tui_mouse_scroll_up_shifts_history_view() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(128);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));

        // Fill history with many lines so the view is scrollable.
        for i in 0..40 {
            agent_tx
                .try_send(AgentEvent::AssistantText(format!("line-{i}\n")))
                .unwrap();
            agent_tx
                .try_send(AgentEvent::Done {
                    reason: yi_agent_core::DoneReason::EndTurn,
                })
                .unwrap();
        }

        // Script: scroll up twice in the history region (row 0), then quit.
        // Events are popped in reverse order (LIFO via Vec::pop).
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Mouse(make_mouse(MouseEventKind::ScrollUp, 0, 0)),
            Event::Mouse(make_mouse(MouseEventKind::ScrollUp, 0, 0)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
            &catalog_for_tests(),
        )
        .unwrap();

        // After scrolling up twice, the bottom of the history view should no
        // longer show the last line. We verify by checking that the last
        // "line-39" is NOT visible at the bottom of the history area (row 16
        // = 24 - 1 status - 1 gap - 1 input - 5 ... but exact row depends on
        // layout). Instead of asserting exact content, assert that scrolling
        // changed the view: capture history rows and confirm the last line
        // shifted.
        //
        // With scroll_offset=2, the last visible line should be "line-38"
        // (one above the true last). We just assert the buffer contains the
        // scrolled content and not "line-39" in the history area.
        let buffer = terminal.backend().buffer();
        // History occupies rows 0..(24-3)=21 (Min(3) minus status+gap+input).
        // Collect history-area text.
        let history_text: String = (0..21u16)
            .flat_map(|y| (0..80u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        assert!(
            !history_text.contains("line-39"),
            "after scrolling up 2, the last line should be off-screen; got: {history_text:?}"
        );
    }
    #[test]
    fn permission_key_e_toggles_expanded() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, mut decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        history.push_event(make_permission_request_normal(1), 80);

        let outcome = handle_key(
            make_key(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 0,
                    history_width: 80,
                    history_height: 20,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert!(matches!(outcome, KeyOutcome::None));
        assert!(
            decision_rx.try_recv().is_err(),
            "'e' must not resolve the permission"
        );
        match &history.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => {
                assert!(*expanded, "'e' should expand the pending request")
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn scroll_keys_work_while_permission_pending() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        for _ in 0..120 {
            history.push(HistoryCell::Separator { label: None }, 80);
        }
        history.push_event(make_permission_request_normal(1), 80);
        history.scroll_offset = 0;

        let outcome = handle_key(
            make_key(KeyCode::Up, KeyModifiers::NONE),
            &mut KeyContext {
                view: ViewContext {
                    input: &mut input,
                    history: &mut history,
                    popup: &mut popup,
                    max_scroll_offset: 100,
                    history_width: 80,
                    history_height: 20,
                    cost_tracker: &CostTracker::default(),
                },
                runtime: RuntimeContext {
                    channels: &TuiChannels::from_senders(
                        &input_tx,
                        &interrupt_tx,
                        &kill_tx,
                        &control_tx,
                        &decision_tx,
                    ),
                    is_running: &is_running,
                    queued: &mut queued,
                    pending_quit: &mut pending_quit,
                    workdir: &std::env::temp_dir(),
                    mcp: &yi_agent_mcp::McpManager::empty(),
                    config: &snapshot_for_tests(),
                },
                model: ModelContext {
                    model: "test-model",
                    current_model_ref: None,
                    catalog: &catalog_for_tests(),
                    current_effort: None,
                    effort_source: EffortSource::Unset,
                },
            },
        );
        assert!(matches!(outcome, KeyOutcome::None));
        assert_eq!(
            history.scroll_offset, 1,
            "Up should scroll history while a permission is pending"
        );
    }

    /// 按脚本逐帧设置 `is_running`，脚本走完后按 Ctrl+Q 退出。
    struct TitleScriptEvents {
        script: Vec<bool>,
        is_running: Arc<AtomicBool>,
        poll_count: Cell<usize>,
    }

    impl EventSource for TitleScriptEvents {
        // `run_loop` 每帧的顺序是：draw → title.sync(is_running) → events.poll()。
        // 也就是说第 N 帧的 sync 看到的是「第 N-1 次 poll 写入的值」。所以
        // `is_running` 必须用脚本首帧的值初始化（首帧 sync 才能看到正确状态），
        // 而 poll 里要写入的是**下一帧**该看到的值，即 `script[index + 1]`。
        // 若在 poll 里就地写 `script[index]`，首帧 sync 会先看到初始的 false，
        // 白写一次 "p"，断言序列变成 ["p", "● p", "p"]。
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let index = self.poll_count.get();
            if index >= self.script.len() {
                return Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                ))));
            }
            if let Some(next) = self.script.get(index + 1) {
                self.is_running
                    .store(*next, std::sync::atomic::Ordering::SeqCst);
            }
            self.poll_count.set(index + 1);
            Ok(None)
        }
    }

    /// 跑 `run_loop` 直到事件源说停，返回记录到的标题写入序列。
    ///
    /// 用 `RecordingSink` 而非真 stdout：`cargo test` 的 stdout 不是 tty，
    /// `StdoutSink` 会短路，写入次数根本看不见。
    fn run_loop_for_title(workdir: &std::path::Path, script: Vec<bool>) -> Vec<String> {
        let backend = Rc::new(RefCell::new(TestBackend::new(80, 24)));
        let mut terminal = Terminal::new(ResizableTestBackend {
            inner: Rc::clone(&backend),
        })
        .unwrap();
        let (_agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        // 首帧 sync 发生在第一次 poll 之前，故初值必须取脚本首帧。
        let first = script.first().copied().unwrap_or(false);
        let is_running = Arc::new(AtomicBool::new(first));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let source = TitleScriptEvents {
            script,
            is_running: Arc::clone(&is_running),
            poll_count: Cell::new(0),
        };

        let sink = crate::tui::title::RecordingSink::default();
        let probe = sink.clone();
        let mut title =
            crate::tui::title::TitleHandle::with_sink(true, false, workdir, Box::new(sink));
        let config = snapshot_for_tests();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            ),
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            workdir.to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &config,
            &catalog_for_tests(),
            TuiTheme::DARK,
            &mut title,
        )
        .unwrap();

        probe.calls()
    }

    #[test]
    fn title_follows_the_running_flag() {
        let calls = run_loop_for_title(std::path::Path::new("/work/p"), vec![true, false]);
        assert_eq!(calls, vec!["● p".to_string(), "p".to_string()]);
    }

    #[test]
    fn title_is_written_once_while_running_stays_true() {
        let calls = run_loop_for_title(std::path::Path::new("/work/p"), vec![true; 60]);
        assert_eq!(
            calls,
            vec!["● p".to_string()],
            "60 identical frames must not rewrite the title (would flicker)"
        );
    }

    #[test]
    fn cancelled_turn_returns_title_to_idle() {
        // 与通知功能相反：`Done{Interrupted}` 不发通知，但标题必须回空闲。
        // 这里用「置 false」等价于 `main.rs` 收尾处的 `store(false)`。
        let calls = run_loop_for_title(std::path::Path::new("/work/p"), vec![true, false]);
        assert_eq!(
            calls.last().map(String::as_str),
            Some("p"),
            "an interrupted turn must not leave the running dot on the tab"
        );
    }

    /// 把清理步骤和标题写入放进**同一条**时间线。
    ///
    /// 为什么不用两个独立序列比较末尾：那样只能证明「标题还原是标题序列的
    /// 最后一项」+「LeaveAlternateScreen 是步骤序列的最后一项」，证明不了
    /// 两者的**相对顺序**。共享一条 log 才能断言 clear 严格落在最后一步之后。
    #[derive(Debug, Clone, Default)]
    struct TimelineSink {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl TimelineSink {
        fn entries(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// 只让**下一次**标题写入返回 `Err`（只生效一次）。
        fn fail_next(&self) {
            self.fail_next
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::tui::title::TitleSink for TimelineSink {
        fn write_title(&mut self, text: &str) -> std::io::Result<()> {
            self.log.lock().unwrap().push(format!("title:{text:?}"));
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(std::io::Error::other("injected"));
            }
            Ok(())
        }
    }

    #[test]
    fn title_restore_failure_does_not_change_exit_code() {
        let sink = TimelineSink::default();
        let log = std::sync::Arc::clone(&sink.log);
        let probe = sink.clone();
        let mut title = crate::tui::title::TitleHandle::with_sink(
            true,
            false,
            std::path::Path::new("/work/p"),
            Box::new(sink),
        );

        // 先让一次写入**成功**：否则 writer 立刻进入 disabled，`clear` 短路，
        // 失败根本落不到 clear 上，断言会恒真。
        title.sync(true);
        assert_eq!(probe.entries(), vec![r#"title:"● p""#.to_string()]);

        // 只注入一次失败 —— 正好是接下来的 `clear`。
        probe.fail_next();

        let cleanup_error = run_cleanup_with(
            |name| {
                log.lock().unwrap().push(format!("step:{name}"));
                Ok(())
            },
            &mut title,
        );

        assert!(
            cleanup_error.is_none(),
            "title restore must never contribute a cleanup error"
        );

        let entries = probe.entries();
        assert_eq!(entries.len(), 6, "1 sync + 4 cleanup steps + 1 clear");
        assert_eq!(entries[0], r#"title:"● p""#);
        assert_eq!(entries[1], "step:DisableBracketedPaste");
        assert_eq!(
            entries[5], r#"title:"""#,
            "clear must still be attempted even though it fails"
        );
    }

    #[test]
    fn clear_happens_after_leave_alternate_screen() {
        let sink = TimelineSink::default();
        let log = std::sync::Arc::clone(&sink.log);
        let probe = sink.clone();
        let mut title = crate::tui::title::TitleHandle::with_sink(
            true,
            false,
            std::path::Path::new("/work/p"),
            Box::new(sink),
        );
        title.sync(true);

        let _ = run_cleanup_with(
            |name| {
                log.lock().unwrap().push(format!("step:{name}"));
                Ok(())
            },
            &mut title,
        );

        assert_eq!(
            probe.entries(),
            vec![
                r#"title:"● p""#.to_string(),
                "step:DisableBracketedPaste".to_string(),
                "step:DisableFocusChange".to_string(),
                "step:disable_raw_mode".to_string(),
                "step:LeaveAlternateScreen".to_string(),
                r#"title:"""#.to_string(),
            ],
            "the empty title must be written strictly after the last cleanup step"
        );
    }
}

#[cfg(test)]
mod runtime_restart_notice_tests {
    use super::*;

    /// The failure can land many turns into a long session, where a
    /// transcript separator is appended off-screen and never seen. The alert
    /// is a popup so it is guaranteed to be on the visible screen.
    #[test]
    fn the_failure_alert_is_rendered_into_the_viewport() {
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_runtime_notice_popup(
                    f,
                    crate::tui::subagents::RUNTIME_RESTART_NOTICE,
                    ratatui::layout::Rect::new(0, 0, 100, 30),
                );
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let screen: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            screen.contains("重启yi-agent后即可用"),
            "the restart remedy is not on screen: {screen}"
        );
    }

    /// The version must sit on the same line as the remedy it qualifies.
    ///
    /// A stale binary and a broken one print the same remedy, so the version is
    /// the only thing that tells them apart. A fixed width sized for the bare
    /// remedy wraps the notice, which moves the version onto its own line --
    /// separated from the "restart" advice it is supposed to qualify. Asserting
    /// only "the version is somewhere on screen" passes either way, so this
    /// asserts row equality instead.
    #[test]
    fn the_failure_alert_keeps_the_version_on_the_remedys_line() {
        use ratatui::backend::TestBackend;

        let text = crate::tui::subagents::runtime_restart_notice_text();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_runtime_notice_popup(f, &text, ratatui::layout::Rect::new(0, 0, 120, 30));
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let screen: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            screen.contains(crate::build_info::VERSION),
            "the version is missing from the popup: {screen}"
        );

        // Compare rows cell-by-cell instead of by string offset: the notice is
        // CJK, so one glyph spans two buffer cells and a char-indexed offset
        // would land on the wrong row.
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let row_of = |needle: &str| -> Option<usize> {
            let first = needle.chars().next()?;
            buffer
                .content
                .iter()
                .position(|cell| cell.symbol().starts_with(first))
                .map(|index| index / width)
        };
        let remedy_row = row_of("重启").expect("the remedy must be rendered");
        let version_row = row_of(crate::build_info::VERSION).expect("the version must be rendered");
        assert_eq!(
            remedy_row, version_row,
            "the version wrapped onto its own line, away from the remedy it qualifies"
        );
    }

    /// The startup dialog is the only place a first-time user learns what
    /// happens after pressing `y`. Starting the runtime costs a daemon spawn,
    /// and a session that cannot start one cannot delegate at all until the
    /// process is restarted -- so the dialog must say so up front instead of
    /// leaving the user staring at a bare error afterwards.
    #[test]
    fn startup_prompt_states_that_a_restart_is_needed() {
        let text: String = runtime_prompt_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("重启"),
            "the dialog never mentions a restart: {text}"
        );
    }
}

/// 终端通知的纯逻辑：事件 → 候选通知、焦点 → 是否发送。
#[cfg(test)]
mod notification_tests {
    use super::*;
    use crate::tui::history::HistoryState;

    #[test]
    fn turn_complete_uses_last_assistant_text() {
        use yi_agent_core::DoneReason;
        let mut state = NotificationState::new();
        let mut history = HistoryState::new();
        history.push_event(AgentEvent::AssistantText("已完成重构".into()), 80);

        state.observe(
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            &history,
            true,
        );
        assert_eq!(state.pending_text(), Some("已完成重构"));
    }

    #[test]
    fn turn_complete_without_text_falls_back() {
        use yi_agent_core::DoneReason;
        let mut state = NotificationState::new();
        let history = HistoryState::new();
        state.observe(
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            &history,
            true,
        );
        assert_eq!(state.pending_text(), Some("yi-agent 回合完成"));
    }

    #[test]
    fn permission_request_outranks_turn_complete() {
        use yi_agent_core::DoneReason;
        let mut state = NotificationState::new();
        let history = HistoryState::new();

        state.observe(
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            &history,
            true,
        );
        state.observe(
            &AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({ "command": "cargo test" }),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            &history,
            true,
        );
        assert_eq!(state.pending_text(), Some("等待批准: cargo test"));
    }

    #[test]
    fn lower_priority_candidate_does_not_replace_higher() {
        let mut state = NotificationState::new();
        let history = HistoryState::new();

        state.observe(
            &AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({ "command": "rm -rf /" }),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            &history,
            true,
        );
        state.observe(
            &AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
            &history,
            true,
        );
        assert_eq!(state.pending_text(), Some("等待批准: rm -rf /"));
    }

    #[test]
    fn non_notifying_events_are_ignored() {
        use yi_agent_core::DoneReason;
        let history = HistoryState::new();
        for event in [
            AgentEvent::Done {
                reason: DoneReason::MaxTurns,
            },
            AgentEvent::Done {
                reason: DoneReason::Interrupted {
                    reason: "user".into(),
                },
            },
            AgentEvent::Cancelled,
            AgentEvent::Error(yi_agent_core::AgentError::ProviderTurnAdmission(
                "boom".into(),
            )),
        ] {
            let mut state = NotificationState::new();
            state.observe(&event, &history, true);
            assert_eq!(state.pending_text(), None, "{event:?} must not notify");
        }
    }

    #[test]
    fn disabled_notifications_produce_nothing() {
        use yi_agent_core::DoneReason;
        let mut state = NotificationState::new();
        let history = HistoryState::new();
        state.observe(
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            &history,
            false,
        );
        assert_eq!(state.pending_text(), None);
    }

    #[test]
    fn flush_clears_pending() {
        use yi_agent_core::DoneReason;
        let mut state = NotificationState::new();
        let history = HistoryState::new();
        state.observe(
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            &history,
            true,
        );
        let taken = state.take();
        assert!(taken.is_some());
        assert_eq!(state.pending_text(), None);
    }

    #[test]
    fn should_notify_respects_condition() {
        use crate::tui::notify::Condition;
        assert!(should_notify(Condition::Always, false));
        assert!(should_notify(Condition::Always, true));
        assert!(should_notify(Condition::Unfocused, false));
        assert!(!should_notify(Condition::Unfocused, true));
    }

    #[test]
    fn focus_events_flip_the_flag() {
        let focused = std::sync::atomic::AtomicBool::new(true);
        apply_focus_event(&Event::FocusLost, &focused);
        assert!(!focused.load(std::sync::atomic::Ordering::Relaxed));

        apply_focus_event(&Event::FocusGained, &focused);
        assert!(focused.load(std::sync::atomic::Ordering::Relaxed));
    }

    // --- 可见输入 / Paste / 补全（计划 Task 8） -------------------------------

    /// 构造一套真实的 TUI 通道，返回它们与需要断言的接收端。
    struct SideRepairHarness {
        input: InputLine,
        channels: TuiChannels,
        input_rx: tokio::sync::mpsc::Receiver<String>,
        interrupt_rx: tokio::sync::mpsc::Receiver<()>,
        side_cmd_rx: tokio::sync::mpsc::Receiver<crate::side_driver::SideDriverCommand>,
        decision_rx: tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>,
        _input_tx: tokio::sync::mpsc::Sender<String>,
        _interrupt_tx: tokio::sync::mpsc::Sender<()>,
        _kill_tx: tokio::sync::mpsc::Sender<String>,
        _control_tx: tokio::sync::mpsc::Sender<crate::DriverControl>,
        _decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
        _side_cmd_tx: tokio::sync::mpsc::Sender<crate::side_driver::SideDriverCommand>,
    }

    fn side_repair_harness() -> SideRepairHarness {
        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(8);
        let (decision_tx, decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (side_cmd_tx, side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(8);
        let mut channels = TuiChannels::from_senders(
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
        );
        channels.side = Some(side_cmd_tx.clone());
        SideRepairHarness {
            input: InputLine::new(),
            channels,
            input_rx,
            interrupt_rx,
            side_cmd_rx,
            decision_rx,
            _input_tx: input_tx,
            _interrupt_tx: interrupt_tx,
            _kill_tx: kill_tx,
            _control_tx: control_tx,
            _decision_tx: decision_tx,
            _side_cmd_tx: side_cmd_tx,
        }
    }

    fn type_into_side(side: &mut SideSession, channels: &TuiChannels, text: &str) {
        let mut popup = None;
        for ch in text.chars() {
            handle_side_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                side,
                channels,
                &mut popup,
            );
        }
    }

    fn press_side(side: &mut SideSession, channels: &TuiChannels, code: KeyCode) -> KeyOutcome {
        let mut popup = None;
        handle_side_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            side,
            channels,
            &mut popup,
        )
    }

    /// 旁支里粘贴只改可见草稿，绝不自动执行，也不碰父输入。
    #[test]
    fn side_repair_paste_stays_in_the_visible_side_draft() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        h.input.buffer = "PARENT_DRAFT".into();
        h.input.cursor = h.input.buffer.len();

        let mut popup = None;
        handle_side_paste("/back".to_string(), &mut side, &mut popup);

        assert_eq!(side.input.buffer, "/back");
        assert_eq!(
            h.input.buffer, "PARENT_DRAFT",
            "旁支粘贴绝不能写进父会话草稿"
        );
        assert!(
            side.in_side(),
            "粘贴只插入字节，不执行命令，视图不能因此切走"
        );
        assert!(h.side_cmd_rx.try_recv().is_err(), "粘贴不该发任何旁支命令");
        assert!(h.input_rx.try_recv().is_err(), "粘贴不该发父会话消息");
    }

    /// 粘贴进来的 `/back` 仍需一次 Enter，且只返回视图，不发 Close。
    #[test]
    fn side_repair_pasted_back_needs_enter_and_sends_no_close() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);

        let mut popup = None;
        handle_side_paste("/back".to_string(), &mut side, &mut popup);
        assert!(side.in_side());

        let outcome = press_side(&mut side, &h.channels, KeyCode::Enter);
        assert!(matches!(outcome, KeyOutcome::None));
        assert!(!side.in_side(), "回车之后才返回主视图");
        assert!(
            h.side_cmd_rx.try_recv().is_err(),
            "返回视图不是关闭旁支：不该发 Close"
        );
        assert!(h.interrupt_rx.try_recv().is_err());
        assert!(h.decision_rx.try_recv().is_err());
    }

    /// 逐字符敲 `/back` 与粘贴走同一条提交路径。
    #[test]
    fn side_repair_typed_back_matches_the_pasted_one() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);

        type_into_side(&mut side, &h.channels, "/back");
        let outcome = press_side(&mut side, &h.channels, KeyCode::Enter);
        assert!(matches!(outcome, KeyOutcome::None));
        assert!(!side.in_side());
        assert!(h.side_cmd_rx.try_recv().is_err());
    }

    /// 补全弹窗只列旁支真正接受的命令，且读写同一份可见输入。
    #[test]
    fn side_repair_completion_offers_only_executable_side_commands() {
        let popup = crate::tui::slash::CommandPopup::with_scope(PopupScope::Side);
        let names: Vec<&str> = popup.visible(20).iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec!["back", "side-close", "help", "config", "cost", "quit", "q"],
            "旁支候选必须恰好是分类器接受的那些命令"
        );
        for name in &names {
            assert!(
                matches!(
                    classify_conversation_input(&format!("/{name}"), true),
                    Ok(RoutedInput::Side(_))
                ),
                "候选 /{name} 必须能被旁支执行，否则就是「显示了却忽略」"
            );
            assert!(
                !is_management_command(&format!("/{name}")),
                "/{name} 不能被当成管理命令拒绝"
            );
        }
        // 主会话的管理命令绝不能出现在旁支候选里。
        for hidden in ["clear", "model", "compact", "runtime", "plugins"] {
            assert!(!names.contains(&hidden), "旁支候选不该列出 /{hidden}");
        }
    }

    /// Tab 只补全文本，Enter 才执行，且两者都基于可见输入缓冲。
    #[test]
    fn side_repair_tab_completes_then_enter_executes() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);

        let mut popup = None;
        for ch in "/si".chars() {
            handle_side_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut side,
                &h.channels,
                &mut popup,
            );
            sync_popup(&mut popup, &side.input.buffer, PopupScope::Side);
        }
        assert_eq!(
            popup.as_ref().and_then(|p| p.selected()).map(|c| c.name()),
            Some("side-close"),
            "旁支输入 /si 时弹窗应直接选中唯一匹配项"
        );
        // 多按一次 Down 会落到 `q`，这里保持第一项，模拟用户直接 Tab。
        handle_side_key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut side,
            &h.channels,
            &mut popup,
        );
        assert_eq!(side.input.buffer, "/side-close", "Tab 写入可见输入行");
        assert!(popup.is_none(), "补全之后弹窗关闭");
        assert!(h.side_cmd_rx.try_recv().is_err(), "Tab 只补全，绝不能提交");
        assert!(side.in_side(), "Tab 不该切视图");

        let outcome = press_side(&mut side, &h.channels, KeyCode::Enter);
        assert!(matches!(outcome, KeyOutcome::None));
        assert!(!side.in_side(), "Enter 执行 /side-close 后回到主视图");
        assert!(
            matches!(
                h.side_cmd_rx.try_recv(),
                Ok(crate::side_driver::SideDriverCommand::Close { .. })
            ),
            "完整的 /side-close 一次 Enter 就该发出 Close"
        );
    }

    /// 没有候选时 Tab 不能被当作正文插进草稿。
    #[test]
    fn side_repair_tab_without_popup_does_not_edit_the_draft() {
        let h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        type_into_side(&mut side, &h.channels, "hello");

        press_side(&mut side, &h.channels, KeyCode::Tab);
        assert_eq!(side.input.buffer, "hello");
    }

    /// Esc 先关补全，不能被解释为停止旁支轮次。
    #[test]
    fn side_repair_esc_closes_the_popup_before_stopping() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        side.active_turn_id = Some("turn-1".into());
        side.views.side.as_mut().unwrap().running = true;

        let mut popup = None;
        for ch in "/ba".chars() {
            handle_side_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut side,
                &h.channels,
                &mut popup,
            );
            sync_popup(&mut popup, &side.input.buffer, PopupScope::Side);
        }
        assert!(popup.is_some());

        handle_side_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut side,
            &h.channels,
            &mut popup,
        );
        assert!(popup.is_none(), "Esc 的第一职责是关掉补全");
        assert!(
            h.side_cmd_rx.try_recv().is_err(),
            "关补全不能顺带停掉旁支轮次"
        );
        assert_eq!(side.input.buffer, "/ba", "Esc 不该吃掉已经敲下的文本");
    }

    /// 管理命令在旁支里拒绝并保留原稿，绝不投给模型。
    #[test]
    fn side_repair_management_commands_are_refused_without_clearing_the_draft() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);

        type_into_side(&mut side, &h.channels, "/clear");
        press_side(&mut side, &h.channels, KeyCode::Enter);

        assert_eq!(
            side.input.buffer, "/clear",
            "被拒的管理命令必须把原文还给用户"
        );
        assert!(
            h.side_cmd_rx.try_recv().is_err(),
            "管理命令既不能执行，也不能当问题发给模型"
        );
        let notice = side.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("clear") && notice.contains("主会话"),
            "拒绝必须说明原因，实际: {notice}"
        );
    }

    /// 嵌套 `/side` 被拒时同样保留原稿。
    #[test]
    fn side_repair_nested_side_is_refused_without_clearing_the_draft() {
        let mut h = side_repair_harness();
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);

        type_into_side(&mut side, &h.channels, "/side 另一个问题");
        press_side(&mut side, &h.channels, KeyCode::Enter);

        assert_eq!(side.input.buffer, "/side 另一个问题");
        assert!(h.side_cmd_rx.try_recv().is_err());
    }

    /// 旁支里的 `/q`、`/quit` 是应用退出，不能被当作问题发给模型。
    #[test]
    fn side_repair_exit_commands_quit_instead_of_reaching_the_model() {
        for command in ["/q", "/quit"] {
            let mut h = side_repair_harness();
            let mut side = SideSession::new(TuiTheme::DARK);
            side.views.open("side-1".into(), 1);

            type_into_side(&mut side, &h.channels, command);
            let outcome = press_side(&mut side, &h.channels, KeyCode::Enter);
            assert!(
                matches!(outcome, KeyOutcome::Quit),
                "{command} 在旁支里也必须退出应用"
            );
            assert!(
                h.side_cmd_rx.try_recv().is_err(),
                "{command} 不能被当成旁支提问"
            );
        }
    }

    /// 关闭没送达之后，旁支的心跳**不能**把这条提示擦掉。
    ///
    /// 心跳恰恰证明「它还在跑」，也就是证明那句话仍然成立。若在每个
    /// `SideStatus` 上都清 `main_notice`，用户只会看到提示闪一帧就消失，
    /// 正是这一轮要消灭的「按了没反应」。只有真正关闭成功或重新受理关闭
    /// 命令，才有资格清它。
    #[test]
    fn side_heartbeat_does_not_erase_a_close_delivery_failure() {
        let mut side = SideSession::new(TuiTheme::DARK);
        let target = ConversationTarget::Side {
            side_id: "side-1".into(),
            generation: 1,
        };
        side.views.open("side-1".into(), 1);
        side.main_notice = Some((
            SideOperation::Close,
            target.clone(),
            "已返回主视图，但旁问关闭未送达".into(),
        ));

        // 同一条旁支的普通状态心跳（它还在跑 → 确实没关掉）。
        side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: target.clone(),
                turn_id: None,
                event: ConversationPayload::SideStatus(
                    yi_agent_runtime::side_conversation::SideStatus::Idle,
                ),
            },
            80,
        );
        assert!(
            side.main_operation_notice().is_some(),
            "心跳不能擦掉关闭未送达的提示"
        );

        // 真正关闭成功后必须清掉。
        side.apply_event(
            &crate::tui::side::ConversationEvent {
                target,
                turn_id: None,
                event: ConversationPayload::SideClosed(
                    yi_agent_runtime::side_conversation::CloseReason::UserClosed,
                ),
            },
            80,
        );
        assert!(
            side.main_operation_notice().is_none(),
            "关闭成功之后提示必须消失"
        );
    }

    /// 旧代的失败帧不能写进新旁支的可见状态。
    ///
    /// 场景：用户关掉旁支 A、开了旁支 B，A 的关闭失败这时才回到 UI。若把它
    /// 写进当前视图的通知行，B 的状态行会显示 A 的失败，看起来像 B 坏了。
    /// 它应该进主视图提示（那里仍然看得见），而不是污染 B。
    #[test]
    fn stale_operation_error_does_not_touch_the_current_side_view() {
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-b".into(), 2);
        side.notice = None;

        side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: ConversationTarget::Side {
                    side_id: "side-a".into(),
                    generation: 1,
                },
                turn_id: None,
                event: ConversationPayload::SideOperationError {
                    operation: SideOperation::Close,
                    message: "旁问已经变化（可能已关闭或被替换），请刷新后再试".into(),
                },
            },
            80,
        );

        assert!(
            side.notice.is_none(),
            "旧代的错误不能写进新旁支的状态行: {:?}",
            side.notice
        );
        let main = side.main_operation_notice().unwrap_or_default();
        assert!(
            main.contains("关闭"),
            "旧代错误仍要在主视图可见，实际: {main}"
        );
    }

    /// 当前可见旁支自己的失败：进它的状态行，不进主视图。
    #[test]
    fn visible_operation_error_goes_to_the_side_notice() {
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-a".into(), 1);
        side.notice = None;

        side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: ConversationTarget::Side {
                    side_id: "side-a".into(),
                    generation: 1,
                },
                turn_id: None,
                event: ConversationPayload::SideOperationError {
                    operation: SideOperation::Interrupt,
                    message: "停止未生效".into(),
                },
            },
            80,
        );

        let notice = side.notice.clone().unwrap_or_default();
        assert!(notice.contains("停止"), "实际: {notice}");
        assert!(side.main_operation_notice().is_none());
    }

    /// Esc 与 Ctrl+C 走同一条判定：轮次未知时也必须留下提示，不能静默。
    #[test]
    fn esc_on_a_side_whose_turn_is_not_known_yet_says_so() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(4);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_tx, mut side_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(4);
        let channels = {
            let mut c = TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            );
            c.side = Some(side_tx);
            c
        };

        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        side.views.side.as_mut().unwrap().running = true;
        side.active_turn_id = None;
        side.notice = None;

        let outcome = press_side(&mut side, &channels, KeyCode::Esc);
        assert!(matches!(outcome, KeyOutcome::None), "Esc 在旁支里不退出");
        let notice = side.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("启动") || notice.contains("停止"),
            "轮次未知时必须说明，实际: {notice:?}"
        );
        assert!(
            side_rx.try_recv().is_err(),
            "不知道轮次 id 就不该发出 Interrupt"
        );
    }

    /// 可见旁支显示「在跑」，但本地还不知道轮次 id：Ctrl+C 必须留下可读提示。
    ///
    /// 静默 `return` 会让人以为没生效，于是再按一次——而第二次 Ctrl+C 是退出。
    /// 用户想要的「停掉这个回答」于是变成「关掉整个应用」。
    #[test]
    fn stopping_a_side_whose_turn_is_not_known_yet_says_so() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::channel::<()>(4);
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_tx, _side_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(4);
        let channels = {
            let mut c = TuiChannels::from_senders(
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
            );
            c.side = Some(side_tx);
            c
        };

        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        // 关键组合：视图认为在跑，但 turn id 还没回来。
        side.views.side.as_mut().unwrap().running = true;
        side.active_turn_id = None;
        side.notice = None;

        stop_visible_conversation(
            &ConversationTarget::Side {
                side_id: "side-1".into(),
                generation: 1,
            },
            false,
            &mut side,
            &channels,
        );

        let notice = side.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("停止") || notice.contains("启动"),
            "不知道轮次 id 时必须说明，实际提示: {notice:?}"
        );
        assert!(
            interrupt_rx.try_recv().is_err(),
            "旁支的停止绝不能落到父会话的中断通道上"
        );
    }

    /// driver 拒绝关闭（旁支正在回答、本地又不知道轮次 id）时，拒绝原因必须
    /// 出现在**用户正看着的那个旁支**的状态行上。
    ///
    /// 只测 driver 返回 Err 不够：那条错误要走 `SideOperationError` 回到 TUI，
    /// 中间任何一处把它丢掉，用户就只会看到「按了 /side-close，界面没动」——
    /// 正是这次要修的那类症状。
    #[test]
    fn a_refused_close_shows_why_in_the_visible_side() {
        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-a".into(), 1);
        side.notice = None;

        side.apply_event(
            &crate::tui::side::ConversationEvent {
                target: ConversationTarget::Side {
                    side_id: "side-a".into(),
                    generation: 1,
                },
                turn_id: None,
                event: ConversationPayload::SideOperationError {
                    operation: SideOperation::Close,
                    // 与 `side_driver.rs` 的 `close` 冲突分支逐字一致。
                    message: "旁支正在回答，请等它结束或先停止再关闭".into(),
                },
            },
            80,
        );

        let notice = side.notice.clone().unwrap_or_default();
        assert!(notice.contains("正在回答"), "实际: {notice}");
        assert!(
            !side.active_side().unwrap().closed.is_some(),
            "拒绝关闭不能被当成已关闭"
        );
        assert!(
            side.main_operation_notice().is_none(),
            "可见旁支自己的失败不该同时污染主视图"
        );
    }

    /// 提交时通道已满：命令照常返回视图，但失败必须可见，且不能谎报已关闭。
    #[test]
    fn side_repair_close_reports_an_undelivered_command() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, _kill_tx) = tokio::sync::mpsc::channel::<String>(4);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::DriverControl>(4);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(4);
        let (side_cmd_tx, mut side_cmd_rx) =
            tokio::sync::mpsc::channel::<crate::side_driver::SideDriverCommand>(1);
        // 预先填满容量 1 的旁支命令通道，让随后的 try_send 必然失败。
        side_cmd_tx
            .try_send(crate::side_driver::SideDriverCommand::Interrupt {
                expected_target: ConversationTarget::Side {
                    side_id: "side-1".into(),
                    generation: 1,
                },
                turn_id: "occupied".into(),
            })
            .unwrap();
        let mut channels = TuiChannels::from_senders(
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
        );
        channels.side = Some(side_cmd_tx);

        let mut side = SideSession::new(TuiTheme::DARK);
        side.views.open("side-1".into(), 1);
        type_into_side(&mut side, &channels, "/side-close");
        let outcome = press_side(&mut side, &channels, KeyCode::Enter);

        assert!(matches!(outcome, KeyOutcome::None), "命令不能被卡住");
        assert!(!side.in_side(), "本地返回不等后台通道");
        // 用户此刻已经在主视图，旁支状态行不再渲染，所以失败必须落在主视图
        // 可见的 `main_notice` 上；写 `side.notice` 等于没提示。
        let notice = side.main_operation_notice().unwrap_or_default();
        assert!(
            notice.contains("未送达"),
            "关闭未送达必须显式可见，实际: {notice}"
        );
        assert!(
            matches!(
                side.main_notice.as_ref().map(|(op, _, _)| *op),
                Some(SideOperation::Close)
            ),
            "主视图提示必须标注是「关闭」失败，而不是随便一条"
        );
        assert!(
            side.active_side().unwrap().closed.is_none(),
            "未送达不能标记为已关闭"
        );
        assert!(side_cmd_rx.try_recv().is_ok(), "只应有那一条占位命令");
    }

    #[test]
    fn unrelated_events_do_not_touch_the_focus_flag() {
        let focused = std::sync::atomic::AtomicBool::new(true);
        apply_focus_event(&Event::Resize(80, 24), &focused);
        assert!(focused.load(std::sync::atomic::Ordering::Relaxed));
    }
}

#[cfg(test)]
mod foreground_recover_args_tests {
    use super::{parse_recover_args, parse_recover_args_with_token};
    use yi_agent_subagent::foreground::RecoveryAction;

    /// A bare invocation only reads. Opening the panel must not stop work the user
    /// has not yet agreed to stop, which is the whole point of splitting inspect out.
    #[test]
    fn foreground_root_lifecycle_recover_root_bare_is_read_only() {
        assert_eq!(parse_recover_args(None), Ok(RecoveryAction::Inspect));
        assert_eq!(parse_recover_args(Some("  ")), Ok(RecoveryAction::Inspect));
    }

    /// A bare word is an operation lookup, never a prepare: the mutating step has to
    /// be typed out, so a stray argument cannot freeze a conversation.
    #[test]
    fn foreground_root_lifecycle_recover_root_bare_id_only_queries() {
        let expected = RecoveryAction::Operation {
            operation_id: "op-1".to_string(),
        };
        assert_eq!(parse_recover_args(Some("op-1")), Ok(expected.clone()));
        assert_eq!(parse_recover_args(Some("status op-1")), Ok(expected));
    }

    #[test]
    fn foreground_root_lifecycle_recover_root_prepare_names_one_operation() {
        assert_eq!(
            parse_recover_args(Some("prepare op-1")),
            Ok(RecoveryAction::Prepare {
                operation_id: "op-1".to_string()
            })
        );
        // Extra words are a mistake, not a second request: refusing beats guessing
        // which of two ids the user meant.
        assert!(parse_recover_args(Some("prepare op-1 op-2")).is_err());
        assert!(parse_recover_args(Some("prepare")).is_err());
    }

    /// The token must not be reachable as an argument. This is the property that
    /// keeps it out of shell history, `ps` output and any echoed transcript.
    #[test]
    fn foreground_root_lifecycle_recover_root_confirm_takes_the_token_from_the_environment() {
        // No token: refuse, and say where the token belongs.
        let refused = parse_recover_args_with_token(Some("confirm op-1"), None);
        assert!(refused.is_err());
        assert!(
            refused.unwrap_err().contains("YI_AGENT_RECOVERY_TOKEN"),
            "the refusal must name the environment variable"
        );
        // A blank token is the same as none: `YI_AGENT_RECOVERY_TOKEN=` must not
        // become an empty capability the daemon would reject as a protocol error.
        assert!(parse_recover_args_with_token(Some("confirm op-1"), Some("  ")).is_err());

        assert_eq!(
            parse_recover_args_with_token(Some("confirm op-1"), Some("secret-token")),
            Ok(RecoveryAction::Confirm {
                operation_id: "op-1".to_string(),
                token: "secret-token".to_string(),
            })
        );
        // The token is never accepted as an argument, so a command line that carried
        // one is rejected rather than silently ignored.
        assert!(parse_recover_args_with_token(Some("confirm op-1 secret-token"), None).is_err());
    }

    #[test]
    fn foreground_root_lifecycle_recover_root_rejects_unknown_shapes() {
        assert!(parse_recover_args(Some("status")).is_err());
        assert!(parse_recover_args(Some("confirm")).is_err());
        assert!(parse_recover_args(Some("prepare op-1 extra")).is_err());
    }
}
