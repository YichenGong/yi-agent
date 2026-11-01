# 终端标题上报（OSC 2）设计

日期：2026-11-01
状态：设计已确认，待实现

## 1. 问题

同时开多个终端标签页跑不同项目时，标签名全部是默认的（`zsh`、`tmux`、或终端自带的目录名）。用户唯一能区分它们的信息——「这是哪个项目」——恰恰不在标签栏上，只能逐个点开确认。

第二个诉求是状态：跑一个长回合后切到别的窗口，回头看标签栏时无法判断这轮是还在跑还是已经结束。终端通知（`docs/superpowers/specs/2026-11-01-terminal-notification-design.md`）解决的是「回合完成时提醒你」这个**离散事件**，标题解决的是「随时扫一眼就知道状态」这个**持续可见**问题。两者互补，不重叠。

终端为此提供了标准机制：**OSC 2**（`ESC ] 2 ; <title> BEL`，或 ST 终止的 `ESC ] 2 ; <title> ESC \`）。几乎所有现代终端都支持，且是「设置标签页/窗口标题」的通用约定。

## 2. 现状核查

| 搜索项 | 结果 |
|---|---|
| `SetTitle` / `]0;` / `]1;` / `]2;` 在 `yi-agent-rs/crates` 与 `desktop/src` | **0 处真实实现**。唯一命中是 `tui/notify/osc9.rs:113` 与 `tui/notify/text.rs:121,202` 的**测试夹具**——它们把 `\u{1b}]0;pwned` 当恶意输入喂给 `sanitize`，断言控制字符被剥掉 |
| `terminal_title` / `term_title` / `osc_2` 标识符 | 0 处 |
| TUI 终端初始化 | `tui/app.rs:134-140` 只有 `enable_raw_mode` / `EnterAlternateScreen` / `EnableBracketedPaste`；清理段 `:197-210` 只还原这几项（含 `:202` 的焦点事件） |
| 桌面端窗口标题 | `desktop/src-tauri/tauri.conf.json:15` 静态 `"Yi-Agent"`；`desktop/src/` 无 `document.title` 或 `setTitle()` 调用 |

结论：**当前完全不支持**。这是「能力具备、实现缺失」，不是「做不到」。

**已具备的基础设施**（本设计可直接复用）：

| 能力 | 位置 | 复用方式 |
|---|---|---|
| 自定义 `crossterm::Command` 写 OSC 序列的范式 | `tui/notify/osc9.rs:22-44` | `SetTitle` 照抄结构，`write_ansi` 可断言字节 |
| 控制字符消毒 | `tui/notify/text.rs:15-22` `sanitize` | 项目目录名可含 `ESC`，必须先剥 |
| 多路复用器检测 | `tui/notify/mod.rs:153-161` `is_multiplexed` | 判 tmux / zellij，是则整体跳过 |
| 按显示列宽截断 | `tui/wrap.rs:94-111` `truncate_to_width` | 已有 CJK 宽度测试，直接复用 |
| 绘制间隙写字节 | `tui/app.rs:871-876`（`notifications.take()` 处） | 标题更新并列在同一个窗口 |
| 写失败即自禁 | `tui/notify/mod.rs:236-240` | 同样的失败语义 |

## 3. 范围

**做：**

- `settings.json` 的 `tui` 组新增一个键：`terminal_title`，取值 `"true"` / `"false"` 字符串（与同组三个通知键同构），默认 `"true"`。
- 进入 TUI 后设置标签标题，退出时还原为空串。
- 标题内容：`● ` 前缀（运行中）+ 项目名，按显示列宽截断到 24 列。
- 项目名取 `cwd.file_name()`，取不到时回退 `yi-agent`（判据细节见 §4）。
- `/config` 显示标题状态：`on` / `off` / `disabled (multiplexer)` 三种（`disabled (write failed)` 属运行期状态，`/config` 是启动快照，见 §7.2）。
- tmux / zellij 内整体跳过（不写任何字节）。

**不做：**

- **动画 spinner**。标签栏是低频更新 UI，部分终端每次 SET 都重绘标签栏，100ms 一次的写入会闪烁；tmux 内每条还要包一次 DCS 透传，代价更高。静态圆点已经回答了「跑完了没」这个唯一问题。Codex 与 Claude Code 都不在标题里做动画，正是因为这类投诉最多。
- **tmux / zellij DCS 透传**。与 `osc9` 的处理**故意不同**：OSC 9 是离散事件，漏一次无影响；标题是**持续性状态覆盖**，在 tmux 里等于持续踩用户的 `status-left` / `window-name` 自定义配置。更硬的理由是 tmux 的 `set-titles` 默认 **off**——透传过去无人接收，白写字节。故直接跳过，见 §6.3。
- **`yi-agent` 前缀**。终端标签宽度普遍 15-25 列，`yi-agent · ` 一进来就吃掉 11 列，项目名稍长即被截成 `yi-agent · mypro…`，最关键的信息先死。用户开标签页时已知在跑什么程序，前缀零信息量。
- **模型名进标题**。`claude-sonnet-4-5-20250929` 这类串会把项目名挤没；模型在状态栏（`tui/statusbar.rs:202` `render_statusbar`）已可见，不必重复。
- **标题模板自定义**（`title_template: "{project} - {model}"`）。模板引擎要处理转义、宽度截断、非法占位符回退，而真实需求只是「哪个项目、跑完了没」。真需要时按具体格式再加。
- **per-project 覆盖**。`settings.json` 是全局的，加局部覆盖要引入配置优先级链，不值。
- **配置热更新**。与通知功能一致：TUI 无设置面板，`/config` 是只读快照，无触发源。改动下次启动生效。
- **headless（`yi-agent run`）**。该路径不跑 TUI；且 `run --json` 的 stdout 是 JSONL 契约，注入控制字符会直接破坏它。完全不动。
- **桌面 app**。OSC 2 对非终端程序无意义；Tauri 窗口标题是独立链路（`window.setTitle`），当前静态 `"Yi-Agent"`，不在本次范围。

## 4. 行为契约

```
空闲：  myproject
运行中：● myproject
```

- 项目名 = `cwd.file_name()`，**取法**与 `app-server/src/lead.rs:136` 的 `lead_title_for_cwd` 一致（同样的 `file_name() → to_str() → 非空` 三步）；但**产物不同**：那边拼「`{name} 组长`」、回退「组长」，这里不带后缀、回退 `yi-agent`。取不到（`/`、空、非 UTF-8）时回退 `yi-agent`。
- 整条标题（含 `● ` 两列）按**显示列宽**截断到 **24 列**。超出部分以 `…` 收尾。
- 圆点为静态，整个会话只有「有/无」两态。
- 退出时写空串，把标签名还给用户终端。

**为什么 24 列**：终端标签宽度普遍 15-25 列，24 是「在不挤掉两侧标签的前提下尽量多显示」的折中；再长终端自己也会截断，而终端的截断不受我们控制（可能切在 CJK 中间）。

**为什么按显示列宽而非字节/字符**：中文路径一字占两列，按 `chars().count()` 截断会得出「24 个字符」但实际 48 列，标签栏错位。`tui/wrap.rs:94` 的 `truncate_to_width` 已用 `UnicodeWidthChar`，且有 `truncate_to_width_keeps_cjk_within_budget`（`wrap.rs:197`）测试。

**关于 `●` 的宽度（已实测，不是推测）**：`●` 是 U+25CF，属 Unicode 的 ambiguous 宽度字符——理论上 `cjk` feature 会把它算作 2 列，但**实测结果是不受影响**：本仓 `unicode-width` 开启 `cjk` feature（`yi-agent-rs/Cargo.toml:61`），用同一版本 + 同一 feature 建临时 crate 实测，`UnicodeWidthChar::width('●')` 返回 `Some(1)`，`UnicodeWidthStr::width("● ")` 为 2、`"● myproject"` 为 11。

结论：前缀确实占 2 列，但这是**实测事实**而非文档推断。为防将来升级 `unicode-width` 或改字符导致结论失效，**§8.1 的 `compose_never_exceeds_budget_for_any_shape` 用 `UnicodeWidthStr::width` 断言上限**，不硬编码列数——变了测试立刻红。

## 5. 模块结构

新建 `yi-agent-rs/crates/yi-agent/src/tui/title.rs`，**与 `notify/` 平级**，不进 `notify/`：

> 通知是**离散事件**（攒候选、去抖、按优先级择一、丢弃无所谓），标题是**持续状态**（有当前值、需要 diff、退出必须还原）。两者生命周期与失败语义不同，混在一个模块里会让 `NotificationState` 的「至多一条候选」语义被标题的「状态机」污染。

并在 `tui/mod.rs`（现有 21 个 `pub mod`，按字母序排列）加 `pub mod title;`——字母序位置在 `pub mod theme;`（`tui/mod.rs:21`）与 `pub mod trace;`（`:22`）之间。

```text
title.rs
├── pub const MAX_COLUMNS: usize = 24;
├── pub const FALLBACK_PROJECT: &str = "yi-agent";
├── pub fn project_label(cwd: &Path) -> String
│     取 file_name；非法/空/非 UTF-8 回退 FALLBACK_PROJECT
├── pub fn compose(project: &str, running: bool) -> String
│     running 预置 "● " → sanitize → truncate_to_width(MAX_COLUMNS)；纯函数
├── struct SetTitle(String)
│     impl crossterm::Command；write_ansi 写 "\u{1b}]2;{title}\u{7}"
├── pub trait TitleSink: Send
│     fn write_title(&mut self, text: &str) -> io::Result<()>
│     ── 生产实现 StdoutSink：先判 is_terminal，再 execute!(SetTitle)
│     ── 测试实现 RecordingSink：只记录不写，用来数写入次数
├── pub struct TitleState {
│       written: Option<String>,     // 上一次成功写出的值，用于 diff
│       observed_running: Option<bool>,  // 上一次观察到的运行态，避免每帧重组
│   }
└── pub struct TitleWriter {
        disabled_reason: Option<DisabledReason>,   // 三态跳过的唯一来源
        sink: Box<dyn TitleSink>,
    }
    ├── pub fn new(enabled: bool, multiplexed: bool) -> Self            // 生产：StdoutSink
    ├── pub fn with_sink(enabled: bool, multiplexed: bool, sink: Box<dyn TitleSink>) -> Self
    ├── pub fn status(&self) -> TitleStatus                            // 供 /config 渲染
    ├── pub fn sync(&mut self, state: &mut TitleState, project: &str, running: bool) -> io::Result<()>
    │     运行态未变则直接返回；变了才 compose 再写
    └── pub fn clear(&mut self, state: &mut TitleState)                 // 退出还原，吞掉错误
```

两个枚举把 `/config` 的四种文案从散落的布尔判断收成一处，避免渲染层自己拼字符串：

```rust
/// 跳过原因。`None` = 正常可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledReason {
    Off,            // 配置关
    Multiplexer,    // tmux / zellij
    WriteFailed,    // 写过一次失败，本会话自禁
}

/// `/config` 的四态展示值。`Display` 输出小写串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleStatus { On, Off, Multiplexer, WriteFailed }

impl DisabledReason {
    fn to_status(self) -> TitleStatus { /* 一一对应 */ }
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
```

`DisabledReason` 必须 `PartialEq`，因为 §6.3 的判定与 §8.3 的测试都用 `== Some(Off)` 这种比较；`TitleStatus` 的 `Display` 是本仓既有做法（`TuiConfigSnapshot::render` 直接 `{}` 插值，见 `tui/slash.rs:312-336`），把文案收在这里而不是散在渲染处。

`new(enabled, multiplexed)` 的两个参数**不是互斥的**：配置关且身处 tmux 时，`enabled=false` 优先——用户明确关掉的东西，没必要再报告环境限制。判定顺序在 `new` 里固定为 `enabled` → `multiplexed`，且**两者都写进 `disabled_reason`**（而非另设布尔字段）：

```rust
pub fn new(enabled: bool, multiplexed: bool) -> Self {
    Self::with_sink(enabled, multiplexed, Box::new(StdoutSink::default()))
}

pub fn with_sink(enabled: bool, multiplexed: bool, sink: Box<dyn TitleSink>) -> Self {
    let disabled_reason = if !enabled {
        Some(DisabledReason::Off)
    } else if multiplexed {
        Some(DisabledReason::Multiplexer)
    } else {
        None
    };
    Self { disabled_reason, sink }
}
```

把三种跳过原因统一收进一个 `Option<DisabledReason>`，`sync` 就只需一次 `is_some()` 判断，不必分别检查 `enabled` / `multiplexed` / `WriteFailed` 三个来源。`status()` 也只是 `disabled_reason` 的一个映射，不存在两处可能不一致的状态。

**为什么 `TitleSink` 必须是 trait 而不是直接写 stdout**：`cargo test` 的 stdout 不是 tty，`StdoutSink` 会短路，于是「写入了几次」在集成测试里**完全不可观测**——而这正是本设计最需要钉住的属性（diff 失效会导致逐帧闪烁）。把「写哪」抽成 trait 后，集成测试用 `RecordingSink` 计数，生产路径仍是 `StdoutSink`。若不做这层抽象，§8.4 的两条核心测试根本写不出来。

**`RecordingSink` 的取回方式**：`Box<dyn TitleSink>` 一旦交给 `run_loop` 就拿不回来了，所以记录必须放在**外部持有**的共享容器里，sink 只写引用：

```rust
/// 测试用：把写入记录进外部持有的 Vec，调用方在 `run_loop` 结束后仍可读。
#[derive(Clone, Default)]
pub struct RecordingSink {
    calls: Arc<Mutex<Vec<String>>>,
    fail_next: Arc<AtomicBool>,   // 注入一次失败，用于测自禁
}

impl RecordingSink {
    pub fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
}
```

测试侧：`let sink = RecordingSink::default(); let probe = sink.clone();` 把 `sink` 装箱喂进 `run_loop`，跑完后用 `probe.calls()` 断言。`Arc<Mutex<..>>` 这一层也让 §8.4 的集成测试与 §8.3 的单测复用同一份 `RecordingSink` 实现，不必写两套假对象。

`TitleSink: Send` 的约束来自 `run_loop` 在多线程测试里使用；去掉 `Send` 会让集成测试无法把 sink 跨线程共享。

`SetTitle` **自己实现** `Command`，不直接用 crossterm 自带的 `terminal::SetTitle`（`crossterm-0.28.1/src/terminal.rs:386` 确实存在）。理由：本仓已确立「自定义 `Command` + 可断言 `write_ansi`」的范式（`osc9.rs:28`、`bel.rs:14`），且测试需要逐字节断言；直接依赖 crossterm 的类型会让字节无法在单测里固定。

`Command` trait 在 Windows 上还要求 `execute_winapi`（`command.rs:26-27`）与 `is_ansi_code_supported`（`:33-35`），两个 `#[cfg(windows)]` 方法照 `osc9.rs:32-42` 的写法：

```rust
#[cfg(windows)]
fn execute_winapi(&self) -> io::Result<()> {
    Err(io::Error::other("tried to execute SetTitle using WinAPI; use ANSI instead"))
}

#[cfg(windows)]
fn is_ansi_code_supported(&self) -> bool { true }
```

用 `Error::other` 而非 `Error::new(ErrorKind::Other, ..)`：前者是 `rust-version = 1.85`（`Cargo.toml:24`）已稳定的写法，仓内已有先例（`yi-agent-store/src/resident.rs:38`）；后者会触发 `clippy::io_other_error`，而 `just lint`（`justfile:19`）是 `-- -D warnings`，警告即失败。

**终字节选择**：用 BEL（`\u{7}`）而非 ST（`\u{1b}\\`）终止，与 `osc9.rs` 一致。BEL 是 OSC 的传统终止符，兼容面最广；ST 更规范但少部分老终端不认。

**Windows 行为**：legacy conhost 不支持 OSC 2，`execute!` 会走 `execute_winapi` 返回 `Err`，于是首次写入即自禁。这与其他平台的降级路径一致，无需额外分支。注意 `/config` 是**启动快照**，看不到运行期置位的 `WriteFailed`（§7.2）；该状态只出现在 `tracing` 日志里（`terminal title write failed; disabling for this session`），排查时看日志而非 `/config`。

## 6. 触发、投递与失败

### 6.1 写时机

三个触发点，全部走 `TitleWriter::sync` / `clear`，靠 `TitleState` 做 diff——**值没变就不写**。

| 时机 | 位置 | 写入值 |
|---|---|---|
| 启动 | `tui/app.rs:159`（`Terminal::new` 之后、`run_loop` 之前） | `compose(project, false)` |
| 每帧（运行态翻转时） | `tui/app.rs:871` 的 flush 窗口 | `compose(project, is_running)` |
| 退出清理 | `tui/app.rs:208` 之后、`:214` 之前（见 §6.5） | `""` |

**为什么每帧检查却几乎不写**：`run_loop` 每帧已经在读 `is_running`（`app.rs:2208`、`:2219` 的取消键判定就是这么做的，用 `Ordering::SeqCst`），这里只是**多读一次**同一个原子量，成本可忽略。真正写字节由 `TitleState` 的 diff 拦住：连续多帧同一运行态只有第一帧会写。

一次完整会话的写入次数：**启动 1 次 + 每回合 2 次（进入运行、退出运行）+ 退出 1 次**。跑 10 个回合就是 22 次，全部落在状态翻转点上，不会出现「每帧重写」的闪烁。

**为什么走 `app.rs:871`**：那里已在 `terminal.draw()` 之后、两帧之间，是全仓唯一「保证不会把 OSC 序列插进一帧 ANSI 中间」的位置（`:870` 的注释原文：「通知只在两帧之间发：插在帧中间会把 ANSI 序列切断，留下渲染残影」）。`notifications.take()`（`:871`）与标题同步并列即可，不改变既有结构。

**为什么启动要等 `Terminal::new` 之后**：`run_tui` 的注释（`app.rs:135-137`）已经确立「探测必须在 alternate screen 之前，任何杂散输出要落进正常滚动区」的规矩。标题不属于「探测输出」，它是**要保留的效果**，放在 alternate screen 之后更安全——万一写入出错，错误不会污染启动画面。


### 6.2 运行态判定：直接读 `is_running`

`is_running` 是 `Arc<AtomicBool>`（`main.rs:2284`），由 driver 任务在 `main.rs:2924`（`agent.run` 之前）置 `true`、`main.rs:3049`（回合收尾之后）置 `false`。`run_loop` 已经持有它（`app.rs:390`），用于 `Esc` / `Ctrl+C` 的取消判定。

**关键事实**：`store(false)` 位于 `match agent.run(...)` 的**之后**（`main.rs:3023` 的 `Err(e)` 分支与 `Ok(stream)` 分支在此汇合），因此它覆盖**全部**收尾路径：

| 路径 | 是否走 `:3049` |
|---|---|
| 正常结束 `EndTurn` | 是 |
| 撞轮次上限 `MaxTurns` | 是 |
| 用户中断 `Interrupted` / 取消 | 是 |
| `agent.run` 返回 `Err` | 是（`Err(e)` 分支只发 `AgentEvent::Error`，不提前 return） |

所以**不需要枚举 `AgentEvent`**，也不需要像 `NotificationState` 那样做事件收敛。每帧调用一次 `sync`，它自己比对：

```rust
// 在 app.rs:871 的 flush 窗口内
let running = is_running.load(std::sync::atomic::Ordering::SeqCst);  // 与 app.rs:2208 一致
let _ = title_writer.sync(&mut title_state, &project_label, running);
```

`sync` 的判定顺序固定如下，每一步都有理由：

```rust
fn sync(&mut self, state: &mut TitleState, project: &str, running: bool) -> io::Result<()> {
    // 1. 已失效：直接返回，且**不**更新 observed_running
    //    保持「自禁后状态静止」，便于 /config 与调试判断真实最后状态。
    if self.disabled_reason.is_some() { return Ok(()); }
    // 2. 运行态没变：不重组字符串、不碰 stdout
    if state.observed_running == Some(running) { return Ok(()); }
    state.observed_running = Some(running);
    // 3. 目标值与自己写过的相同（例如项目名未变时的重复 sync）：跳过
    let target = compose(project, running);
    if state.written.as_deref() == Some(target.as_str()) { return Ok(()); }
    // 4. 写。失败则自禁，错误只在内部记一次
    match self.sink.write_title(&target) {
        Ok(()) => { state.written = Some(target); Ok(()) }
        Err(error) => {
            tracing::warn!(error = %error, "terminal title write failed; disabling for this session");
            self.disabled_reason = Some(DisabledReason::WriteFailed);
            Ok(())
        }
    }
}
```

第 1 步放在最前面是有意的：自禁是**终态**，此后 `observed_running` 冻结，`/config` 与日志反映的是「失效那一刻的真实状态」，而不是一个继续空转的假象。反过来（先更新 `observed_running` 再判 disabled）会让状态看起来还在跟进，掩盖已失效的事实。

第 2 与第 3 步都必要且不重复：第 2 步挡的是「每帧调用」，第 3 步挡的是「运行态翻转但目标串恰好相同」——比如项目名被截断成一样的前缀时，翻转后目标仍是同一串。

这比事件路径更稳：`AgentEvent` 出现过多次「某条路径不发 Done」的问题（`main.rs:3026-3028` 的注释就记着「取消路径压根不发 Done，只挂 Done 会让每次 Ctrl+C 的那一轮静默丢失」），而复用驱动任务已经统一收敛好的那个布尔量，天然继承同一个修复。

延迟上界是一帧（`events.poll(33ms)`，`app.rs:886`），人眼不可感知。

**关键决策：取消与出错也回空闲。** 通知功能（spec `2026-11-01-terminal-notification-design.md` §3）刻意**不**为失败/中断发通知，因为那是离散提醒、会刷屏。但标题的语义不同——它回答的是「**现在**还在跑吗」。一个被 `Ctrl+C` 中断的回合如果标题仍挂着 `●`，标签栏会一直谎报，比不显示更糟。

本方案让这条自动成立：`store(false)` 覆盖全部路径，标题自然回空闲。**这是选择读原子量而非枚举事件的第二个理由**——如果用 `AgentEvent` 枚举，就得逐个列出 `Done{EndTurn}`、`Done{MaxTurns}`、`Done{Interrupted}`、`Cancelled`、`Error` 五个分支，漏一个就是「标签栏永久挂着圆点」的 bug。

### 6.3 跳过条件

`sync` 在以下任一成立时**直接返回 `Ok(())` 且不写一个字节**：

| 条件 | 判定 | `/config` 显示 |
|---|---|---|
| 配置关闭 | `disabled_reason == Some(Off)` | `title: off` |
| tmux / zellij 内 | `disabled_reason == Some(Multiplexer)` | `title: disabled (multiplexer)` |
| 已自禁（写失败过一次） | `disabled_reason == Some(WriteFailed)` | 运行期状态，`/config` 快照看不到，只进日志（§7.2） |
| stdout 非 tty | `std::io::stdout().is_terminal()`（与 `notify/mod.rs:228` 同判据） | — |

**`Off` 与 `Multiplexer` 在构造期（`new`）就定了**，会话内不变——多路复用器判定只做一次，与 `Osc9Backend::new` 的 `dcs_passthrough` 同做法（`osc9.rs:46-55`）。**`WriteFailed` 是运行期产生的**，由 §6.4 的失败路径置位，二者共用一个 `Option<DisabledReason>` 字段，`sync`/`clear` 只需一次 `is_some()` 判断。

**stdout 是否为 tty 不能进 `disabled_reason`**：它属于 `StdoutSink` 的内部决策，不是 writer 的状态。测试与管道环境里 stdout 非 tty，但会话中途被重定向同样应停止写入，故放在 `StdoutSink::write_title` 里每次判、返回 `Ok(())` 而不置位。抽到 sink 内还有一个好处：`RecordingSink` 完全不受 tty 影响，测试可稳定计数。

### 6.4 失败自禁

写失败时的处理与 `Notifier`（`notify/mod.rs:236-240`）逐条对齐：

- `tracing::warn!` 记一次，`disabled_reason = Some(WriteFailed)`。
- 之后所有 `sync` 立即返回 `Ok(())`——终端已不可用，每回合重试只会刷日志。
- **不向调用方传播错误**：`sync` 的 `io::Result` 只在调用点被 `let _ =` 吞掉，`run_loop` 不因标题失败而中断。

`clear`（退出还原）**同样吞掉错误且不置位自禁**：此时进程正在退出，`disabled_reason` 已无人读取；置位只会让 `/config` 的最终快照出现一条无意义的记录。

`clear` 的第一行为同样是 `if self.disabled_reason.is_some() { return; }`——配置关闭或身处 tmux 时，我们从没写过标题，也就**不该**在退出时写一个空串去覆盖。这是「不碰用户终端」原则的延伸：没写过的东西，退出时也不动。

### 6.5 退出还原绝不能影响退出码

现有清理段（`app.rs:197-210`）是「每步都试、只记第一个错误」，而这个错误会**变成 `run_tui` 的返回值**：

```rust
match (result, cleanup_error) {
    (Err(error), _) => Err(error),
    (Ok(_), Some(error)) => Err(error),      // ← 清理失败 ⇒ 退出码非零
    (Ok(_), None) => Ok(()),
}
```

标题还原**必须**走独立路径——`clear` 返回 `()`、自身吞错，因此无法被放进 `cleanup_error`（签名层面杜绝）。**插入点固定在 `app.rs:210` 之后、`:214` 之前**：

```rust
    if let Err(error) = execute!(terminal.backend_mut(), DisableBracketedPaste) {   // :199
        cleanup_error.get_or_insert(error);
    }
    if let Err(error) = execute!(terminal.backend_mut(), DisableFocusChange) {      // :202
        cleanup_error.get_or_insert(error);
    }
    if let Err(error) = disable_raw_mode() {                                        // :205
        cleanup_error.get_or_insert(error);
    }
    if let Err(error) = execute!(terminal.backend_mut(), LeaveAlternateScreen) {    // :208
        cleanup_error.get_or_insert(error);
    }

    // 标题还原：独立路径，失败只记日志、不影响退出码。
    title_writer.clear(&mut title_state);                                           // ← 新增

    if let Ok(dropped) = result {                                                   // :214
        report_dropped_pending(&mut io::stderr(), dropped);
    }

    match (result, cleanup_error) {                                                 // :218
```

**为什么必须在 `LeaveAlternateScreen`（`:208`）之后**：alternate screen 被关闭时，多数终端会重置标题或让写入落到滚动区；先写空串等于白写。放在 `:208` 之后、`:214` 之前，是唯一同时满足「alternate screen 已退出」与「stdout 尚未被 dropped」的窗口。

**失败后果的量级**：若把 `clear` 的错误并进 `cleanup_error`，在上百种不支持 OSC 2 的终端组合上，每次正常退出都会返回非零，`yi-agent` 会被 shell 的 `set -e`、CI 脚本、以及用户自己的判断当成崩溃。

这是本设计里**最容易写错的一处**，§8 有专门的回归测试。

## 7. 配置与可观测性

### 7.1 配置键

```json
{
  "tui": {
    "terminal_title": "true"
  }
}
```

- 类型 `Option<String>`，与同组三个通知键**完全同构**（`yi-agent-runtime/src/settings.rs:131-138`）。`None` = 用默认值（删键恢复默认）。
- **必须是字符串 `"true"` / `"false"`，不能是 JSON 布尔 `true`**：`FIELDS` 的 `Select` 与 `Text` 都只接受字符串（`settings.rs:126-130` 的注释原文：用 `Option<bool>` 会导致设置页写进 `"true"` 后反序列化读不回来）。写成裸布尔会让桌面设置页写进去的值读不回来。
- 默认 `"true"`。理由：这正是功能价值所在；退出会还原，无持久副作用。
- 需要 `FieldSpec` 登记（照 `settings.rs:370-380` 的 `tui.notifications` 同型）：`key: "tui.terminal_title"`、`group: "tui"`、`label: "终端标题"`、`field_type: FieldType::Select`、`default_display: "true"`、`description: "在终端标签页标题里显示项目名与运行状态；空表示用默认（开）"`、`options: &["true", "false"]`、`min: None`、`max: None`（`FieldSpec` 的 **9 个字段**都要显式写出、无 `..Default`：`key` / `group` / `label` / `field_type` / `default_display` / `description` / `options` / `min` / `max`，见 `settings.rs:170-184`）。桌面端与 web 设置界面直接渲染这张表（`settings.rs:168-169` 注释原文：「web 与桌面端都渲染这张表，故描述与默认值文案只在这里写一次」）。
- 非法值（如 `"maybe"`）照 `settings.rs:637-645` 的 `tui.notifications` 布尔白名单校验写：`matches!(v.to_ascii_lowercase().as_str(), "true" | "false")` 不匹配时 `tracing::warn!` + 置 `None` 回退默认，不 fatal。（`:646-668` 的两个枚举白名单是同一模式的另一例。）
- 解析进 `RuntimeConfig`（`yi-agent-runtime/src/config.rs:124-129` 的字段区末尾加 `pub terminal_title: bool`）与 `RuntimeConfig::default()`（`config.rs:529-533` 的初始化区末尾加 `terminal_title: true`）。

解析语句照 `config.rs:419` 的一行写法（那里注释已解释「存储侧是字符串，这里解析成 bool；空串与非法值已在 `normalized` 里清成 None，故只需处理默认」）：

```rust
let terminal_title = !matches!(settings.tui.terminal_title.as_deref(), Some("false"));
```

**不能写成 `== Some("true")`**：那样 `None`（键缺失，即最常见情况）会得到 `false`，把默认值 `true` 变成 `false`。`!matches!(.., Some("false"))` 让 `None` 与 `Some("true")` 同时为真，正好表达「默认开」。

**桌面设置页要补一行中文标签**：`desktop/src/components/SettingsRuntimeTab.tsx:45-48` 的 `VALUE_LABELS` 是 `FIELDS` 之外的**第二处硬编码表**（`FIELDS` 只给控件类型与选项，标签是前端另存的）。不加的话，新字段的下拉会显示原始值 `true` / `false` 而不是「开 / 关」：

```ts
"tui.terminal_title": { true: "开", false: "关" },
```

（`SettingsRuntimeTab.test.tsx:201` 有 `labelFor(key, value)` 式的断言范式可照抄，建议补一条 `expect(labelFor("tui.terminal_title", "false")).toBe("关")`。）

**分组表不用动**：`SettingsRuntimeTab.tsx:118-124` 按 `group` 字段分组，`tui` 组已在 `GROUP_ORDER` / `GROUP_LABELS` / `GROUP_HINTS` 里登记过，新键的 `group: "tui"` 会自动落进「终端」分组。这是本功能唯一需要改前端的地方；其余全部在 Rust 侧。

**`RuntimeConfig` 加字段的波及面（必须提前知道）**：该结构体在所有构造点都是**穷举字面量、无 `..Default::default()` 兜底**，加一个字段会让下列 9 个文件同时编译失败，必须逐个补齐：

| 文件 | 构造点 |
|---|---|
| `yi-agent-runtime/src/config.rs` | `:431`（生产解析）、`:506`（`sample_config`） |
| `yi-agent-runtime/src/models.rs` | `:755`（测试辅助）、`:1507`（`cfg()`） |
| `yi-agent-app-server/src/test_support.rs` | `:86`（`test_config`） |
| `yi-agent-app-server/src/rpc_tests.rs` | `:1959`（`default_config`）、`:14239`（`pin_test_config`） |
| `yi-agent-app-server/src/onboarding_rpc.rs` | `:119`（`fallback`） |
| `yi-agent-app-server/src/model_rpc.rs` | `:504`（`fallback_cfg`） |
| `yi-agent-subagent/src/attach.rs` | `:473`、`:657`、`:707` |
| `yi-agent-subagent/src/lib.rs` | `:2805` |
| `yi-agent-subagent/tests/attach_delegation.rs` | `:30` |

（核验方式：`grep -rn 'RuntimeConfig *{' --include=*.rs yi-agent-rs/crates`，再剔除 `pub struct` / `impl` / 函数签名 `-> RuntimeConfig {` 三类命中。）这是编译期即可发现的错误，不是隐藏风险；列出它是为了让 plan 的任务拆分把这 9 个文件算进去，而不是实现到一半才发现跨 crate 连锁。

### 7.2 `/config` 显示

在现有 `terminal notify:` 行（`tui/slash.rs:325-335`，`render` 末尾）之后加一行。`TitleStatus` 有四个变体，但 `/config` 只会出现前三种：

```
  terminal title: on                       # 正常
  terminal title: off                      # 配置关闭
  terminal title: disabled (multiplexer)   # tmux / zellij
```

第四种 `disabled (write failed)` 是**运行期**状态（§6.4），而 `/config` 渲染的是启动时构造的 `TuiConfigSnapshot`（`main.rs:2340`），故不会出现。`Display` 仍实现四个变体，将来若接热更新不必改渲染层。

`TuiConfigSnapshot`（`tui/slash.rs:298-306` 的字段区）加一个 `title_status: String` 字段，在 `main.rs:2340-2380` 构造快照处填值。

**`TuiConfigSnapshot` 共 3 个构造点，加字段后都要补**：

| 构造点 | 性质 |
|---|---|
| `main.rs:2341` | 生产路径（唯一的真实构造） |
| `app.rs:350`（`snapshot_for_tests`） | 测试共享工厂 |
| `slash.rs:1228`（`config_snapshot_renders_key_fields_without_secrets`） | 单测内联字面量 |

**必须同步改 `app.rs:349` 的 `snapshot_for_tests()`**：该函数是所有 TUI 测试共享的快照工厂（`app.rs:345-347` 的注释原文：「Preferred single definition: changing the struct's fields only needs one test-side update」），在 `app.rs` 内被 **89 处**调用，分布在 **80 个 `#[test]` 函数**里。给它补一行 `title_status: "on".into(),` 即可全部通过；**若漏掉，80 个测试函数同时编译失败**（整个 `app.rs` 测试二进制都编不出来）——这是本设计里改动波及面最大的一处，也是最容易被忽略的一处。

（核验：`grep -rn 'snapshot_for_tests()' yi-agent-rs/crates/yi-agent/src --include=*.rs | wc -l` 得 89；按 `#[test]` 归属去重得 80 个测试函数。）

**取值方式**：与同处 `effective_backend` 的写法完全同构（`main.rs:2366-2374`）——即**重新构造一个 writer 只为了问它的状态**：

```rust
title_status: crate::tui::title::TitleWriter::new(
    config.terminal_title,
    crate::tui::notify::is_multiplexed(&crate::tui::notify::TerminalEnv::from_process()),
)
.status()
.to_string(),
```

代价是多建一个 `TitleWriter`（只含一个 `Option` 与一个 `Box`），与既有 `effective_backend` 的做法一致，不引入新机制。注意**不能**读运行中那个 writer 的状态：它在 `run_loop` 内部、`/config` 渲染在外层。

**`WriteFailed` 不会出现在 `/config` 里**：`/config` 是启动时的快照（`TuiConfigSnapshot` 在 `main.rs:2340` 构造一次），运行期自禁后快照不会刷新。这是本仓既有的「`/config` 是只读启动快照」约定（§3「不做：配置热更新」），文案仍保留 `WriteFailed` 分支以便将来接热更新时不必改渲染逻辑。

**为什么必须显示**：tmux 内静默跳过是个**不可见的降级**。不显示的话，tmux 用户会以为功能坏了却查不到原因——这正是 `docs/bug-list.md:92` 记过的教训：supervisor 的 stderr 被 `.stderr(Stdio::null())` 丢进 `/dev/null`，「系统在报告、报告被丢弃」——用户看到的是「功能静默失效、查不到原因」。与通知功能显示 `effective_backend`（`slash.rs:329-333`）同理。

## 8. 测试

### 8.1 纯函数（`title.rs` 内）

| 测试 | 断言 |
|---|---|
| `compose_idle_is_just_the_project` | `compose("myproject", false) == "myproject"` |
| `compose_running_adds_dot_prefix` | `compose("myproject", true) == "● myproject"` |
| `compose_truncates_to_24_columns` | 40 字符 ASCII 名截断后 `UnicodeWidthStr::width <= 24`，且以 `…` 收尾 |
| `compose_measures_cjk_by_display_width` | 24 个汉字 = 48 列，截断后 `<= 24` 列 |
| `compose_counts_the_dot_prefix_against_the_budget` | 运行态下可用宽度少 2 列：同一长名在 `running=true` 时正文比 `false` 时短 2 列 |
| `compose_strips_control_characters_from_project` | 项目名含 `\u{1b}]2;pwned\u{7}` 时输出里 `\u{1b}` 计数为 0，判定用断言而非肉眼 |
| `compose_never_exceeds_budget_for_any_shape` | 参数化：空串、1 列、23/24/25 列、全 CJK、混合——**无条件**断言 `width <= 24` |
| `project_label_uses_last_path_segment` | `/work/my-app` → `my-app` |
| `project_label_falls_back_for_root` | `/` → `yi-agent` |
| `project_label_falls_back_for_empty_name` | `Path::new("")` → `yi-agent` |

`compose_never_exceeds_budget_for_any_shape` 是本节最重要的一条：`truncate_to_width` 的 `budget = width - 1` 逻辑在 `width = 0` 时会 `saturating_sub` 到 0、随后仍 `push('…')`，得到 1 列——`MAX_COLUMNS` 是常量 24 不会触发，但把上限做成参数化测试能挡住未来有人把它改成配置项时引入的越界。

### 8.2 编码（逐字节）

| 测试 | 断言 |
|---|---|
| `set_title_writes_exactly_one_osc2` | `write_ansi` 输出 `== "\u{1b}]2;myproject\u{7}"` |
| `set_title_with_empty_string_still_terminates` | 输出 `== "\u{1b}]2;\u{7}"` |
| `set_title_does_not_emit_st` | 输出里不含 `\u{1b}\\`（BEL 与 ST 只应有其一） |

照 `bel.rs:55-65` 的 `the_command_writes_exactly_one_bel` 范式：断言 `Command` 真正写出的字节，而不只是常量。只测常量的话，把 `write_ansi` 改成空实现仍然全绿，用户却什么都看不到。

### 8.3 策略（`TitleWriter`，用 `RecordingSink`）

| 测试 | 断言 |
|---|---|
| `disabled_writer_writes_nothing` | `enabled=false` 时 `sync` 不产生任何 sink 调用 |
| `multiplexed_writer_writes_nothing` | 同上，且 `status()` 为 `Multiplexer` |
| `sync_is_a_no_op_when_running_is_unchanged` | 连续 5 次 `sync(running=true)`，sink 只被调用 1 次 |
| `sync_rewrites_when_running_flips` | `true` → sink 收到 `"● p"`；`false` → 收到 `"p"`，共 2 次 |
| `sync_rewrites_when_project_changes` | 同运行态、不同 `project` 参数 → 会重写（`written` 与目标值不同即写） |
| `clear_empties_the_title` | `clear` 后 sink 收到 `""`，且 `state.written == Some("")` |
| `clear_is_a_no_op_when_already_empty` | 连续两次 `clear`，sink 只被调用 1 次 |
| `clear_is_a_no_op_when_never_wrote` | `new(enabled=false, ...)` 后 `clear` 不调用 sink——没写过的终端不该被退出时覆盖 |
| `sync_skips_when_target_equals_written` | 运行态翻转但 `compose` 结果相同（超长名截断后前缀一致）时，sink 不再被调用 |
| `a_failed_write_disables_the_writer` | `RecordingSink` 注入 `Err`，首次 `sync` 后 `status()` 为 `WriteFailed`，后续 `sync` 不再调用 sink |
| `a_disabled_writer_stays_quiet_and_ok` | 自禁后 `sync` 仍返回 `Ok` |
| `clear_swallows_errors_without_disabling` | 正常 writer（`status()==On`）下 `clear` 遇 `Err`：不 panic、`status()` 仍是 `On`（`clear` 不置自禁） |
| `status_maps_each_disabled_reason` | `Off` → `off`、`Multiplexer` → `disabled (multiplexer)`、`WriteFailed` → `disabled (write failed)`、`None` → `on` |
| `off_wins_over_multiplexer` | `new(enabled=false, multiplexed=true).status()` 是 `Off`，不是 `Multiplexer` |

`non_tty_writes_nothing` 移到这里**不成立**了：它属于 `StdoutSink` 的行为，不是 `TitleWriter` 的。改为在 `StdoutSink` 自身的单测里覆盖（照 `notify/mod.rs:397-424` 的写法：非 tty 时返回 `Ok` 而不写，含「真终端则跳过」而不是给假结论）。

### 8.4 集成（`tui/app.rs` 测试模块）

复用既有 `ResizableTestBackend`（`app.rs:5302` 已有先例）跑真实 `run_loop`。为让写入可观测，`run_tui` 的测试变体需接受一个注入了 `RecordingSink` 的 `TitleWriter`——**这也是 §5 引入 `TitleSink` 的原因**。

| 测试 | 断言 |
|---|---|
| `title_follows_the_running_flag` | 置 `true` 跑一帧 → sink 收到 `● p`；置 `false` 再跑一帧 → 收到 `p` |
| `title_is_written_once_while_running_stays_true` | 连续 60 帧 `true`，sink 只收到 1 次 |
| `cancelled_turn_returns_title_to_idle` | 从 `true` 置 `false`（等价于 `main.rs:3049` 的收尾）→ 收到 `p`。**这是与通知功能相反的关键行为**：`Done{Interrupted}` 不发通知，但必须还原标题 |
| `error_path_returns_title_to_idle` | 同上；另在 spec 层记录：`main.rs:3023-3025` 的 `Err(e)` 分支只发 `AgentEvent::Error` 后落回，`store(false)`（`:3049`）在其后无条件执行 |

第二条是本次简化的核心保障：如果 diff 失效，60 帧会写 60 次，标签栏在每个终端上都会明显闪烁。

**关于 `run_loop` 的可见性**：它当前是私有 `fn`（`app.rs:543-544`，已带 `#[allow(clippy::too_many_arguments)]`），已有 **16 个参数**（`:545-562`：`terminal` / `agent_rx` / `history` / `input` / `channels` / `is_running` / `events` / `model` / `runtime_intent` / `runtime_choice_tx` / `process_manager` / `workdir` / `mcp` / `config` / `catalog` / `theme`；其中 `workdir` 与 `config: &TuiConfigSnapshot` 正是 §8.4 方案 2 就地取值的依据），测试模块与它同文件，可直接调用。注入 `TitleWriter` 需要给 `run_loop` 多加一个参数，仓库内共 **15 处**调用点：生产 1 处（`app.rs:178`，`run_tui` 内）、`#[cfg(test)]` 辅助函数 2 处（`:402`、`:486`，被各测试共享）、`mod tests` 内（起点 `:4018`）12 处。

两种落地方式，倾向后者（不扩散到既有测试）：

1. 逐个改 15 处调用点传 `&mut TitleWriter`——函数已是 16 个参数（靠 `#[allow(clippy::too_many_arguments)]` 压着），加到 17 个只是把 allow 撑得更宽；且 15 处调用里大多数只关心渲染、不关心标题，改它们纯属噪声。
2. **在 `run_loop` 内部构造**：`run_loop` 已经有 `theme`、`workdir`、`config` 等入参，标题所需的 `project_label(&workdir)` 与 `config.terminal_title` 都能就地取得。只把 `&mut TitleWriter` 这一项做成可注入，靠 `#[cfg(test)]` 的包装函数（`run_loop_with_writer`）暴露给测试，生产路径的 `run_loop` 保持原签名不变。

spec 只要求「写入次数在集成层可断言」，具体用哪种留给 plan 阶段定；但**必须**保证生产路径签名不因测试需求而膨胀。

### 8.5 回归护栏（最重要的一条）

| 测试 | 断言 |
|---|---|
| `title_restore_failure_does_not_change_exit_code` | 注入一个必然失败的 `clear`，断言 `run_tui` 仍返回 `Ok`。变异验证：把 `title_writer.clear(&mut title_state);` 改成 `cleanup_error.get_or_insert(...)`，此测试必须变红 |
| `clear_happens_after_leave_alternate_screen` | 用一个记录调用顺序的 sink，断言 `clear` 发生在 `LeaveAlternateScreen`（`app.rs:208`）之后；变异验证：把 `clear` 挪到 `:199`（清理段开头）之前，此测试必须变红 |

第二条防的是一个具体错法：若在 `LeaveAlternateScreen` 之前写 `""`，该序列会落进备用屏幕，退出后既没还原标题也没报错——用户终端被永久改成空标题。

### 8.6 验收命令

```bash
# 1. 纯函数与策略单测（tui/title.rs 内的 mod tests）
cargo test -p yi-agent --bin yi-agent tui::title

# 2. 集成测试（app.rs 的 mod tests：写入次数、退出顺序）
cargo test -p yi-agent --bin yi-agent title_

# 3. 配置键的解析与回退
cargo test -p yi-agent-runtime terminal_title

# 4. 桌面设置页标签（改了 SettingsRuntimeTab.tsx 才有意义）
cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx

# 5. 合并前唯一入口：fmt-check / lint --all-targets / test / build /
#    bindings-check / secret-check 等全部门禁
just ci
```

**关于 `--bin yi-agent`**：`yi-agent` crate 只有 `[[bin]]` 一个 target（`crates/yi-agent/Cargo.toml:11-13`，`path = "src/main.rs"`），**没有 `lib.rs` 也没有 `[lib]` 段**，`tui/` 挂在 `src/main.rs:13` 的 `mod tui;` 下。因此 `-p yi-agent` 本身已足够，`--bin` 只是显式化、防止将来新增 lib target 时静默漏跑。核实方式：`ls crates/yi-agent/src/lib.rs` 应报不存在，`grep -n '^\[lib\]' crates/yi-agent/Cargo.toml` 应无输出。

**第 3 条的测试名照通知键的既有四条写**（`yi-agent-runtime` 内，`settings.rs:1539-1580` 与 `config.rs:753-800` 是同构模板）：`invalid_terminal_title_flag_is_dropped`、`valid_terminal_title_values_survive`、`tui_terminal_title_reaches_runtime_config`、`tui_terminal_title_defaults_apply_when_absent`。第四个尤其重要——它钉住「键缺失时 `terminal_title == true`」，正是 §7.1 强调的 `!matches!(.., Some("false"))` 那行。

**不要只跑前四条就合并**：`just ci` 的 `lint --all-targets` 会检查测试代码，`bindings-check` 会检查序列化契约，两者都不在上面的命令里。

## 9. 风险与取舍

| 风险 | 应对 |
|---|---|
| 用户的终端/多路复用器不认 OSC 2 | 失败即静默自禁，不报错、不改退出码；`/config` 可查状态 |
| 标题还原失败导致退出码非零 | §6.5 独立路径 + §8.5 两条专门测试 |
| 项目目录名含控制字符造成终端注入 | `notify::text::sanitize` 在 `compose` 内先跑；§8.1 的 `compose_strips_control_characters_from_project` 用断言（非肉眼）钉住 |
| 每帧写标题造成闪烁 | §6.1 的 diff：值不变不写；实际写入只落在状态翻转点（每回合 2 次） |
| tmux 用户被覆盖自定义标题 | §6.3 直接跳过，不写字节 |
| 24 列在实际终端里仍偏长 | 常量集中一处，后续按反馈调整；不影响结构 |
| 与通知功能的行为不一致引起困惑 | §6.2 明确记录「通知不为失败发、标题为失败还原」的相反取舍及理由 |
| 新增 `TitleSink` trait 增加一层间接 | 换来的收益是「写入次数可断言」——否则 §8.4 两条核心测试写不出来（`cargo test` 的 stdout 非 tty，`StdoutSink` 会短路）。生产实现只有 `StdoutSink` 一个，trait 无动态分发的性能顾虑（每会话几十次调用） |
| 忘记在 `tui/mod.rs` 登记 `pub mod title;` | 直接表现为编译错误（`crate::tui::title` 不存在），不是静默失败；§5 已写明位置 |
| `just ci` 的 `lint --all-targets` 会检查测试代码 | 测试辅助函数（`RecordingSink`、注入变体）同样受 clippy 约束；§8.4 的 `#[cfg(test)]` 包装须保持 lint 干净 |
| 未来把 24 列改成配置项 | `truncate_to_width` 在 `width` 极小时会产出 1 列（`saturating_sub` 后仍 `push('…')`）；`MAX_COLUMNS` 现为常量不受影响，若开放为配置必须补下界校验 |
| `unicode-width` 升级改变 `●` 宽度 | §8.1 的 `compose_never_exceeds_budget_for_any_shape` 用 `UnicodeWidthStr::width` 断言上限，不硬编码列数 |

## 10. 与既有通知设计的关系

| 维度 | 通知（OSC 9 / BEL） | 标题（OSC 2） |
|---|---|---|
| 性质 | 离散事件提醒 | 持续状态覆盖 |
| 触发 | 回合完成、权限请求 | 启动、运行态翻转、退出 |
| 频率 | 每回合至多 1 次 | 每回合 2 次（进入/退出运行），有 diff |
| tmux | DCS 透传（`osc9.rs:13-15`） | **跳过不写** |
| 失败/中断 | **不发**（避免刷屏） | **回空闲**（避免谎报） |
| 失败处理 | 自禁 | 自禁 |
| 模块 | `tui/notify/` | `tui/title.rs` |

两处「故意相反」的取舍都是**语义驱动**而非风格差异：通知是「值得打断你吗」，标题是「现在是什么状态」。混为一谈会同时做错两件事。
