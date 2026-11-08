//! 终端标签页标题（OSC 2）。
//!
//! 与 `notify/` 平级而非其子模块：通知是离散事件（攒候选、去抖、丢弃
//! 无所谓），标题是持续状态（有当前值、需 diff、退出必须还原）。两者没有
//! 共享状态，只复用 `sanitize` 与 `is_multiplexed` 两个纯工具。

use std::path::Path;

/// 标题的显示列宽上限。
///
/// 终端标签栏普遍 15-25 列，24 是「尽量多显示又不挤掉两侧标签」的折中；
/// 再长终端自己也会截断，而终端的截断不受我们控制（可能切在 CJK 中间）。
pub const MAX_COLUMNS: usize = 24;

/// 取不到项目名时的回退值。
pub const FALLBACK_PROJECT: &str = "yi-agent";

/// 运行态前缀。`●` 是 U+25CF，实测 `UnicodeWidthChar::width` 返回 `Some(1)`，
/// 加一个空格共 2 列。
const RUNNING_PREFIX: &str = "● ";

/// 从工作目录取项目名：最后一级目录名。
///
/// 判据与 `app-server/src/lead.rs` 的 `lead_title_for_cwd` 一致
/// （`file_name() → to_str() → 非空` 三步），但产物不同：那边拼
/// 「`{name} 组长`」、回退「组长」，这里不带后缀、回退 [`FALLBACK_PROJECT`]。
pub fn project_label(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(FALLBACK_PROJECT)
        .to_string()
}

/// 组装标题：运行态前缀 + 项目名，经消毒后按显示列宽截断到 [`MAX_COLUMNS`]。
///
/// 消毒在截断之前：项目目录名可以含 ESC，一个裸 ESC 就能截断 OSC 序列并
/// 注入终端指令。这是安全边界，不是格式美化。
pub fn compose(project: &str, running: bool) -> String {
    let raw = if running {
        format!("{RUNNING_PREFIX}{project}")
    } else {
        project.to_string()
    };
    let cleaned = crate::tui::notify::text::sanitize(&raw);
    crate::tui::wrap::truncate_to_width(&cleaned, MAX_COLUMNS)
}

/// 写 OSC 2 设置终端标题。
///
/// 自己实现 `Command` 而不用 crossterm 自带的 `terminal::SetTitle`：本仓已
/// 确立「自定义 `Command` + 可断言 `write_ansi`」的范式（`notify/osc9.rs`、
/// `notify/bel.rs`），且单测需要逐字节断言——依赖 crossterm 的类型做不到。
///
/// 终止符用 BEL 而非 ST：BEL 是 OSC 的传统终止符，兼容面最广；ST 更规范但
/// 少部分老终端不认。
pub struct SetTitle(pub String);

impl crossterm::Command for SetTitle {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        write!(f, "\u{1b}]2;{}\u{7}", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "tried to execute SetTitle using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

use std::io::{self, stdout};

/// 跳过原因。`None` = 正常可用。
///
/// 三种原因共用一个字段：`sync` / `clear` 只需一次 `is_some()` 判断，不必
/// 分别检查「配置关」「多路复用器」「写失败」三个来源，也就不存在两处状态
/// 不一致的可能。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledReason {
    /// 配置关闭（`tui.terminal_title = "false"`）。
    Off,
    /// tmux / zellij 内：不写任何字节（不做 DCS 透传）。
    Multiplexer,
    /// 写过一次失败，本会话自禁。
    WriteFailed,
}

/// `/config` 的展示值。
///
/// 四个变体里 `WriteFailed` 是运行期状态，而 `/config` 渲染的是启动时的
/// 快照，所以界面上只会看到前三者。保留第四态是为了将来接热更新时不必改
/// 渲染层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleStatus {
    On,
    Off,
    Multiplexer,
    WriteFailed,
}

impl DisabledReason {
    fn to_status(self) -> TitleStatus {
        match self {
            Self::Off => TitleStatus::Off,
            Self::Multiplexer => TitleStatus::Multiplexer,
            Self::WriteFailed => TitleStatus::WriteFailed,
        }
    }
}

impl std::fmt::Display for TitleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Multiplexer => "disabled (multiplexer)",
            Self::WriteFailed => "disabled (write failed)",
        })
    }
}

/// 往哪里写标题。
///
/// 抽成 trait 的唯一理由是「写入次数」必须可观测：`cargo test` 的 stdout
/// 不是 tty，`StdoutSink` 会短路，于是「逐帧重写导致闪烁」这个最需要钉住的
/// 属性在集成测试里完全看不见。生产实现只有 [`StdoutSink`]，每次会话几十次
/// 调用，动态分发的开销可忽略。
pub trait TitleSink: Send {
    fn write_title(&mut self, text: &str) -> io::Result<()>;
}

/// 写 stdout 的生产实现。
///
/// stdout 非 tty 时返回 `Ok(())` 而不写、也**不**置自禁：那不是「终端不支持
/// OSC 2」，只是「这次没有终端」。会话中途被重定向同样走这条路径。
#[derive(Debug, Default, Clone)]
pub struct StdoutSink;

impl TitleSink for StdoutSink {
    fn write_title(&mut self, text: &str) -> io::Result<()> {
        use std::io::IsTerminal;
        if !stdout().is_terminal() {
            return Ok(());
        }
        crossterm::execute!(stdout(), SetTitle(text.to_string()))
    }
}

/// 测试用：把写入记进外部持有的容器，调用方在 writer 被移动后仍可读。
///
/// `Box<dyn TitleSink>` 交出去就拿不回来了，所以记录必须共享：调用方先
/// `clone()` 一份探针，再把原 sink 装箱喂进去。
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct RecordingSink {
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    fail_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
impl RecordingSink {
    /// 至今收到的所有写入值，按调用顺序。
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// 让下一次写入返回 `Err`（只生效一次）。
    pub fn fail_next(&self) {
        self.fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl TitleSink for RecordingSink {
    fn write_title(&mut self, text: &str) -> io::Result<()> {
        self.calls.lock().unwrap().push(text.to_string());
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(io::Error::other("injected failure"));
        }
        Ok(())
    }
}

/// 上一次的写入状态，用于避免每帧重写。
#[derive(Debug, Default)]
pub struct TitleState {
    /// 上一次**成功**写出的值。
    written: Option<String>,
    /// 上一次观察到的运行态。`None` = 还没观察过。
    observed_running: Option<bool>,
}

/// 标题的写入策略。
///
/// 三个跳过原因拆成三个字段而不是一个 `Option<DisabledReason>`，因为它们的
/// **可变性不同**：`enabled` 来自设置文件，一次保存就可能改；`multiplexed`
/// 是本进程的环境事实，只有重启才会变；`write_failed` 是终端已经拒绝过
/// OSC 2 之后的粘性状态。合成一个字段时，「先关再开」会把粘性失败写回
/// `None`，于是热更新等于凭空重试一次终端明确不接受的写入。
pub struct TitleWriter {
    /// 用户开关（`tui.terminal_title`）。热更新只改这一项。
    enabled: bool,
    /// tmux / zellij 内不可写。环境事实，设置改不动。
    multiplexed: bool,
    /// 写过一次失败，本会话自禁。粘性，设置改不动。
    write_failed: bool,
    sink: Box<dyn TitleSink>,
}

impl std::fmt::Debug for TitleWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TitleWriter")
            .field("enabled", &self.enabled)
            .field("multiplexed", &self.multiplexed)
            .field("write_failed", &self.write_failed)
            .finish_non_exhaustive()
    }
}

impl TitleWriter {
    /// 生产构造：写 stdout。
    pub fn new(enabled: bool, multiplexed: bool) -> Self {
        Self::with_sink(enabled, multiplexed, Box::new(StdoutSink))
    }

    /// 可注入 sink 的构造（测试用）。
    ///
    /// 判定顺序固定为 `enabled` → `multiplexed`：配置关且身处 tmux 时报告
    /// `Off`——用户明确关掉的东西，没必要再报告环境限制。
    pub fn with_sink(enabled: bool, multiplexed: bool, sink: Box<dyn TitleSink>) -> Self {
        Self {
            enabled,
            multiplexed,
            write_failed: false,
            sink,
        }
    }

    /// 当前跳过原因。顺序固定为「用户开关 → 写失败 → 多路复用器」。
    ///
    /// `WriteFailed` 排在 `Multiplexer` 之前是因为它只会出现在曾真正尝试
    /// 写入的 writer 上（多路复用器分支从不写），顺序对生产无影响，但让
    /// 「用户关掉」始终压过两种自禁——用户明确关掉的东西，没必要再报告
    /// 环境限制或历史故障。
    fn disabled_reason(&self) -> Option<DisabledReason> {
        if !self.enabled {
            Some(DisabledReason::Off)
        } else if self.write_failed {
            Some(DisabledReason::WriteFailed)
        } else if self.multiplexed {
            Some(DisabledReason::Multiplexer)
        } else {
            None
        }
    }

    /// Re-evaluates the config switch without touching the sink.
    ///
    /// Settings are live: a save can turn the title on or off while the session
    /// runs. Two states stay sticky on purpose:
    ///
    /// * `Multiplexer` — an environment fact that no setting can change.
    /// * `WriteFailed` — the terminal already refused OSC 2 once. Re-enabling
    ///   after a save would retry a write that the terminal has already
    ///   demonstrated it does not accept, and the retry would print garbage into
    ///   the frame. It is the one skip reason a setting must not override.
    ///
    /// Turning the switch off always wins, even over those two: nothing is
    /// written, so there is nothing to protect.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// 当前状态，供 `/config` 渲染。
    pub fn status(&self) -> TitleStatus {
        self.disabled_reason()
            .map_or(TitleStatus::On, DisabledReason::to_status)
    }

    /// 按运行态同步标题。值没变就不写。
    ///
    /// 判定顺序（每一步都有理由）：
    /// 1. 已失效 → 直接返回，且**不**更新 `observed_running`：自禁是终态，
    ///    状态应冻结在「失效那一刻」，而不是看起来还在跟进。
    /// 2. 运行态未变 → 不重组字符串、不碰 stdout：挡的是「每帧调用」。
    /// 3. 目标值与自己写过的相同 → 跳过：挡的是「运行态翻转但截断后目标串
    ///    恰好一样」。
    /// 4. 写。失败则自禁并吞掉错误——调用方不该因标题失败而中断。
    pub fn sync(&mut self, state: &mut TitleState, project: &str, running: bool) -> io::Result<()> {
        if self.disabled_reason().is_some() {
            return Ok(());
        }
        if state.observed_running == Some(running) {
            return Ok(());
        }
        state.observed_running = Some(running);

        let target = compose(project, running);
        if state.written.as_deref() == Some(target.as_str()) {
            return Ok(());
        }

        match self.sink.write_title(&target) {
            Ok(()) => {
                state.written = Some(target);
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "terminal title write failed; disabling for this session"
                );
                self.write_failed = true;
                Ok(())
            }
        }
    }

    /// 退出还原：写空串。
    ///
    /// 返回 `()` 而非 `Result` 是刻意的——签名层面杜绝「有人把它的错误并进
    /// `run_tui` 的 `cleanup_error`」。那会让上百种不支持 OSC 2 的终端在每次
    /// 正常退出时都返回非零，被 shell 的 `set -e`、CI 脚本当成崩溃。
    ///
    /// 判据是「我们真的写过东西」，**不是**禁用原因。两者在静态装配下等价
    /// （关闭/tmux 时从未写过），但运行期设置把开关关掉之后 reason 变成
    /// `Off` 而终端里正摆着我们写过的标题——那一刻恰恰必须清掉。用 reason
    /// 当判据会让这条路径永远短路，屏幕上留着旧项目名直到退出。
    ///
    /// 反过来，从没写过时（开机就关、tmux、写失败）`written` 是 `None`，
    /// 于是不会写空串去覆盖用户自己的标题。
    pub fn clear(&mut self, state: &mut TitleState) {
        if state.written.is_none() {
            return;
        }
        if state.written.as_deref() == Some("") {
            return;
        }
        if self.sink.write_title("").is_ok() {
            state.written = Some(String::new());
        }
    }
}

/// 把 writer、state 与项目名绑在一起，让 `run_loop` 只多一个参数。
///
/// `run_loop` 已经有 16 个参数（靠 `#[allow(clippy::too_many_arguments)]`
/// 压着），不该为标题再涨两个。这个聚合器是纯转发，不含新逻辑。
#[derive(Debug)]
pub struct TitleHandle {
    writer: TitleWriter,
    state: TitleState,
    project: String,
}

impl TitleHandle {
    /// 生产构造：从工作目录取项目名。
    pub fn new(enabled: bool, multiplexed: bool, workdir: &Path) -> Self {
        Self {
            writer: TitleWriter::new(enabled, multiplexed),
            state: TitleState::default(),
            project: project_label(workdir),
        }
    }

    /// 可注入 sink 的构造（测试用）。
    #[cfg(test)]
    pub fn with_sink(
        enabled: bool,
        multiplexed: bool,
        workdir: &Path,
        sink: Box<dyn TitleSink>,
    ) -> Self {
        Self {
            writer: TitleWriter::with_sink(enabled, multiplexed, sink),
            state: TitleState::default(),
            project: project_label(workdir),
        }
    }

    /// 永不写入。给那些只关心渲染、不关心标题的既有测试用。
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            writer: TitleWriter::with_sink(false, false, Box::new(StdoutSink)),
            state: TitleState::default(),
            project: FALLBACK_PROJECT.to_string(),
        }
    }

    /// 当前状态。生产路径由 `main.rs` 在构造快照时直接问 `TitleWriter`，
    /// 这里只为测试保留。
    #[cfg(test)]
    pub fn status(&self) -> TitleStatus {
        self.writer.status()
    }

    /// Applies a live `tui.terminal_title` value.
    ///
    /// After a save disables the title we must also *clear* what we wrote: a
    /// stale project name would otherwise sit in the terminal for the rest of
    /// the session, and the user asked for it to stop appearing. Clearing only
    /// happens if we actually wrote something (`TitleState::written`), so a
    /// title that was never shown is never overwritten either.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.writer.set_enabled(enabled);
        if !enabled {
            self.clear();
        }
        // 重新打开时必须让下一帧真正写一次：`sync` 以「运行态未变」为
        // 第一道闸，而关闭期间运行态很可能没变，否则标题会一直空着。
        // 置 `None` 只让下一帧重算一次，不改变任何判定语义。
        self.state.observed_running = None;
    }

    /// 同步标题。错误在 writer 内部已吞掉，这里也不返回。
    pub fn sync(&mut self, running: bool) {
        let _ = self.writer.sync(&mut self.state, &self.project, running);
    }

    /// 退出还原。
    pub fn clear(&mut self) {
        self.writer.clear(&mut self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn compose_idle_is_just_the_project() {
        assert_eq!(compose("myproject", false), "myproject");
    }

    #[test]
    fn compose_running_adds_dot_prefix() {
        assert_eq!(compose("myproject", true), "● myproject");
    }

    #[test]
    fn compose_truncates_to_24_columns() {
        let long = "a".repeat(40);
        let out = compose(&long, false);
        let width = UnicodeWidthStr::width(out.as_str());
        assert!(width <= MAX_COLUMNS, "{out:?} is {width} cols");
        assert!(out.ends_with('…'), "should end with an ellipsis: {out:?}");
    }

    #[test]
    fn compose_measures_cjk_by_display_width() {
        // 24 个汉字 = 48 列。按 `chars().count()` 截断会得出「24 个字符」
        // 但实际 48 列，标签栏错位。
        let long = "中".repeat(24);
        let out = compose(&long, false);
        let width = UnicodeWidthStr::width(out.as_str());
        assert!(width <= MAX_COLUMNS, "{out:?} is {width} cols");
    }

    #[test]
    fn compose_counts_the_dot_prefix_against_the_budget() {
        // 前缀占 2 列，所以运行态下留给项目名的预算少 2 列。用『留下的 a
        // 的个数』断言，而不是断言两边总宽度相等——后者恒真，测不出东西。
        let long = "a".repeat(40);
        let idle = compose(&long, false);
        let running = compose(&long, true);
        assert!(running.starts_with("● "), "{running:?}");
        let idle_a = idle.matches('a').count();
        let running_a = running.matches('a').count();
        assert_eq!(
            idle_a,
            MAX_COLUMNS - 1,
            "idle should keep {} a's plus the ellipsis: {idle:?}",
            MAX_COLUMNS - 1
        );
        assert_eq!(
            running_a,
            MAX_COLUMNS - 1 - 2,
            "the 2-column prefix must cost exactly 2 a's: {running:?}"
        );
    }

    #[test]
    fn compose_strips_control_characters_from_project() {
        // 项目目录名可以含控制字符。判定用断言而非肉眼：把一个真实会被
        // 终端解释的 OSC 2 序列塞进项目名。
        let out = compose("evil\u{1b}]2;pwned\u{7}", false);
        assert_eq!(out.matches('\u{1b}').count(), 0, "ESC: {out:?}");
        assert_eq!(out.matches('\u{7}').count(), 0, "BEL: {out:?}");
    }

    #[test]
    fn compose_never_exceeds_budget_for_any_shape() {
        let twenty_three = "a".repeat(23);
        let twenty_four = "a".repeat(24);
        let twenty_five = "a".repeat(25);
        let cjk_twelve = "中".repeat(12);
        let cjk_thirteen = "中".repeat(13);
        let mixed = "a中".repeat(20);
        let shapes = [
            "",
            "a",
            twenty_three.as_str(),
            twenty_four.as_str(),
            twenty_five.as_str(),
            cjk_twelve.as_str(),
            cjk_thirteen.as_str(),
            mixed.as_str(),
        ];
        for shape in shapes {
            for running in [false, true] {
                let out = compose(shape, running);
                let width = UnicodeWidthStr::width(out.as_str());
                assert!(
                    width <= MAX_COLUMNS,
                    "shape {shape:?} running={running} -> {out:?} at {width} cols"
                );
            }
        }
    }

    #[test]
    fn project_label_uses_last_path_segment() {
        assert_eq!(project_label(Path::new("/work/my-app")), "my-app");
    }

    #[test]
    fn project_label_falls_back_for_root() {
        assert_eq!(project_label(Path::new("/")), FALLBACK_PROJECT);
    }

    #[test]
    fn project_label_falls_back_for_empty_name() {
        assert_eq!(project_label(Path::new("")), FALLBACK_PROJECT);
    }
    #[test]
    fn set_title_writes_exactly_one_osc2() {
        use crossterm::Command;
        let mut out = String::new();
        SetTitle("myproject".to_string())
            .write_ansi(&mut out)
            .unwrap();
        assert_eq!(out, "\u{1b}]2;myproject\u{7}");
        assert_eq!(out.as_bytes(), b"\x1b]2;myproject\x07");
    }

    #[test]
    fn set_title_with_empty_string_still_terminates() {
        use crossterm::Command;
        let mut out = String::new();
        SetTitle(String::new()).write_ansi(&mut out).unwrap();
        assert_eq!(out, "\u{1b}]2;\u{7}");
    }

    #[test]
    fn set_title_does_not_emit_st() {
        use crossterm::Command;
        let mut out = String::new();
        SetTitle("x".to_string()).write_ansi(&mut out).unwrap();
        assert!(
            !out.contains("\u{1b}\\"),
            "BEL and ST must not both appear: {out:?}"
        );
    }
    /// 构造一个能观测写入的 writer，并留下探针。
    fn writer_with_probe(
        enabled: bool,
        multiplexed: bool,
    ) -> (TitleWriter, RecordingSink, TitleState) {
        let sink = RecordingSink::default();
        let probe = sink.clone();
        let writer = TitleWriter::with_sink(enabled, multiplexed, Box::new(sink));
        (writer, probe, TitleState::default())
    }

    #[test]
    fn disabled_writer_writes_nothing() {
        let (mut writer, probe, mut state) = writer_with_probe(false, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
        assert_eq!(writer.status(), TitleStatus::Off);
    }

    #[test]
    fn multiplexed_writer_writes_nothing() {
        let (mut writer, probe, mut state) = writer_with_probe(true, true);
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
        assert_eq!(writer.status(), TitleStatus::Multiplexer);
    }

    #[test]
    fn off_wins_over_multiplexer() {
        assert_eq!(TitleWriter::new(false, true).status(), TitleStatus::Off);
    }

    #[test]
    fn status_maps_each_disabled_reason() {
        assert_eq!(TitleWriter::new(true, false).status(), TitleStatus::On);
        assert_eq!(TitleWriter::new(false, false).status(), TitleStatus::Off);
        assert_eq!(
            TitleWriter::new(true, true).status(),
            TitleStatus::Multiplexer
        );
        assert_eq!(TitleStatus::On.to_string(), "on");
        assert_eq!(TitleStatus::Off.to_string(), "off");
        assert_eq!(
            TitleStatus::Multiplexer.to_string(),
            "disabled (multiplexer)"
        );
        assert_eq!(
            TitleStatus::WriteFailed.to_string(),
            "disabled (write failed)"
        );
    }

    #[test]
    fn sync_is_a_no_op_when_running_is_unchanged() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        for _ in 0..5 {
            writer.sync(&mut state, "p", true).unwrap();
        }
        assert_eq!(probe.calls(), vec!["● p".to_string()]);
    }

    #[test]
    fn sync_rewrites_when_running_flips() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(probe.calls(), vec!["● p".to_string(), "p".to_string()]);
    }

    #[test]
    fn sync_rewrites_when_project_changes() {
        // 运行态未变（true → true），但项目名变了：第 2 步会挡住这次 sync。
        // 现实中项目名在会话内不变，所以这里直接把 `observed_running` 置回
        // `None` 模拟「下一次观察」，验证第 3 步的 diff 用的是最新目标串。
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(probe.calls().len(), 1, "unchanged running must not rewrite");

        state.observed_running = None;
        writer.sync(&mut state, "q", true).unwrap();
        assert_eq!(probe.calls(), vec!["● p".to_string(), "● q".to_string()]);
    }

    #[test]
    fn sync_skips_when_target_equals_written() {
        // 运行态翻转，但截断后的目标串与上次写过的完全相同 → 不写。
        let long = "a".repeat(40);
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        let already = compose(&long, true);
        state.written = Some(already);
        state.observed_running = Some(false);
        writer.sync(&mut state, &long, true).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
    }

    #[test]
    fn clear_empties_the_title() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.clear(&mut state);
        assert_eq!(probe.calls(), vec!["● p".to_string(), String::new()]);
        assert_eq!(state.written, Some(String::new()));
    }

    #[test]
    fn clear_writes_at_most_one_blank_after_a_title_was_written() {
        // 判据是 `written`，所以「从没写过」不发字节（见下一条），而「写过」
        // 只清一次：连续两次 clear 不该再发一条空串。
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", false).unwrap();
        writer.clear(&mut state);
        writer.clear(&mut state);
        assert_eq!(probe.calls(), vec!["p".to_string(), String::new()]);
    }

    #[test]
    fn clear_is_a_no_op_when_never_wrote() {
        // 配置关闭时从没写过，退出也不该写空串去覆盖用户的标题。
        let (mut writer, probe, mut state) = writer_with_probe(false, false);
        writer.clear(&mut state);
        assert_eq!(probe.calls(), Vec::<String>::new());
    }

    #[test]
    fn a_failed_write_disables_the_writer() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(writer.status(), TitleStatus::WriteFailed);
        assert_eq!(probe.calls().len(), 1);

        // 自禁后不再碰 sink。
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(probe.calls().len(), 1);
    }

    #[test]
    fn a_disabled_writer_stays_quiet_and_ok() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.sync(&mut state, "p", true).unwrap();
        assert!(
            writer.sync(&mut state, "p", false).is_ok(),
            "a disabled writer must keep returning Ok"
        );
        assert_eq!(writer.status(), TitleStatus::WriteFailed);
    }

    #[test]
    fn clear_swallows_errors_without_disabling() {
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.clear(&mut state); // 返回 ()，不 panic
        assert_eq!(
            writer.status(),
            TitleStatus::On,
            "clear must not self-disable"
        );
    }

    #[test]
    fn title_handle_exposes_project_and_status() {
        let handle = TitleHandle::new(false, false, Path::new("/work/my-app"));
        assert_eq!(handle.status(), TitleStatus::Off);
    }

    #[test]
    fn disabled_handle_never_writes() {
        let mut handle = TitleHandle::disabled();
        handle.sync(true);
        handle.sync(false);
        handle.clear();
        assert_eq!(handle.status(), TitleStatus::Off);
    }

    #[test]
    fn title_handle_writes_through_its_sink() {
        let sink = RecordingSink::default();
        let probe = sink.clone();
        let mut handle =
            TitleHandle::with_sink(true, false, Path::new("/work/my-app"), Box::new(sink));
        handle.sync(true);
        handle.sync(true);
        handle.sync(false);
        assert_eq!(
            probe.calls(),
            vec!["● my-app".to_string(), "my-app".to_string()]
        );
        handle.clear();
        assert_eq!(probe.calls().last().map(String::as_str), Some(""));
    }

    // ---- live re-evaluation of `tui.terminal_title` -----------------------

    #[test]
    fn turning_the_title_off_clears_what_was_written() {
        // 关掉之后终端里不该还留着项目名：用户要求的正是「不再出现」。
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.sync(&mut state, "p", true).unwrap();
        writer.set_enabled(false);
        writer.clear(&mut state);
        assert_eq!(
            probe.calls(),
            vec!["● p".to_string(), String::new()],
            "disabling must blank the already-written title"
        );
    }

    #[test]
    fn turning_the_title_off_before_any_write_does_not_touch_the_terminal() {
        // 从没写过就没有可清的东西：此时写空串会覆盖用户自己的标题。
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        writer.set_enabled(false);
        writer.clear(&mut state);
        assert_eq!(probe.calls(), Vec::<String>::new());
    }

    #[test]
    fn re_enabling_does_not_wait_for_the_running_flag_to_flip() {
        // 关闭期间运行态往往没变；`sync` 的第一道闸是「运行态未变」，
        // 不重置它就永远不会重写，标题会一直空着。
        let sink = RecordingSink::default();
        let probe = sink.clone();
        let mut handle =
            TitleHandle::with_sink(true, false, Path::new("/work/app"), Box::new(sink));
        handle.sync(false);
        handle.set_enabled(false);
        handle.set_enabled(true);
        handle.sync(false);
        assert_eq!(
            probe.calls(),
            vec!["app".to_string(), String::new(), "app".to_string()]
        );
    }

    #[test]
    fn a_write_failure_survives_turning_the_title_back_on() {
        // 终端已经拒绝过 OSC 2。设置保存不该让程序再赌一次：那不是配置问题，
        // 是这台终端不支持的证据。
        let (mut writer, probe, mut state) = writer_with_probe(true, false);
        probe.fail_next();
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(writer.status(), TitleStatus::WriteFailed);

        writer.set_enabled(false);
        writer.set_enabled(true);
        writer.sync(&mut state, "p", false).unwrap();
        assert_eq!(
            probe.calls().len(),
            1,
            "a failed write must not be retried after a save: {:?}",
            probe.calls()
        );
        assert_eq!(writer.status(), TitleStatus::WriteFailed);
    }

    #[test]
    fn the_multiplexer_guard_survives_turning_the_title_back_on() {
        let (mut writer, probe, mut state) = writer_with_probe(true, true);
        writer.set_enabled(false);
        writer.set_enabled(true);
        writer.sync(&mut state, "p", true).unwrap();
        assert_eq!(probe.calls(), Vec::<String>::new());
        assert_eq!(writer.status(), TitleStatus::Multiplexer);
    }

    #[test]
    fn the_user_switch_outranks_both_sticky_reasons() {
        // 用户关掉了：报 off，而不是拿环境/历史故障当解释。
        let failed = {
            let (mut writer, probe, mut state) = writer_with_probe(true, false);
            probe.fail_next();
            writer.sync(&mut state, "p", true).unwrap();
            writer
        };
        let mut failed = failed;
        failed.set_enabled(false);
        assert_eq!(failed.status(), TitleStatus::Off);

        let mut multiplexed = TitleWriter::new(true, true);
        multiplexed.set_enabled(false);
        assert_eq!(multiplexed.status(), TitleStatus::Off);
    }

    #[test]
    fn clearing_on_disable_is_idempotent() {
        // 一帧内同一设置变化可能被处理两次（本地提交 + 外部观察）。
        let sink = RecordingSink::default();
        let probe = sink.clone();
        let mut handle =
            TitleHandle::with_sink(true, false, Path::new("/work/app"), Box::new(sink));
        handle.sync(false);
        handle.set_enabled(false);
        handle.set_enabled(false);
        assert_eq!(
            probe.calls(),
            vec!["app".to_string(), String::new()],
            "the second disable must not write another blank"
        );
    }

    /// `cargo test` 的 stdout 不是 TTY，这条恰好覆盖 `StdoutSink` 的第一道
    /// 短路：必须返回 `Ok` 而不是报错。
    #[test]
    fn stdout_sink_is_a_no_op_when_not_a_tty() {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            eprintln!("skip: stdout is a tty");
            return;
        }
        let mut sink = StdoutSink;
        assert!(sink.write_title("hello").is_ok());
    }
}
