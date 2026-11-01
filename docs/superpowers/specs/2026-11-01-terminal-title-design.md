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
| 自定义 `crossterm::Command` 写 OSC 序列的范式 | `tui/notify/osc9.rs:22-41` | `SetTitle` 照抄结构，`write_ansi` 可断言字节 |
| 控制字符消毒 | `tui/notify/text.rs:15-22` `sanitize` | 项目目录名可含 `ESC`，必须先剥 |
| 多路复用器检测 | `tui/notify/mod.rs:153-161` `is_multiplexed` | 判 tmux / zellij，是则整体跳过 |
| 按显示列宽截断 | `tui/wrap.rs:94-111` `truncate_to_width` | 已有 CJK 宽度测试，直接复用 |
| 绘制间隙写字节 | `tui/app.rs:871-876`（`notifications.take()` 处） | 标题更新并列在同一个窗口 |
| 写失败即自禁 | `tui/notify/mod.rs:236-240` | 同样的失败语义 |

## 3. 范围

**做：**

- `settings.json` 的 `tui` 组新增一个键：`terminal_title`（`bool`，默认 `true`）。
- 进入 TUI 后设置标签标题，退出时还原为空串。
- 标题内容：`● ` 前缀（运行中）+ 项目名，按显示列宽截断到 24 列。
- 项目名取 `cwd.file_name()`，回退 `yi-agent`。
- `/config` 显示标题状态三态。
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

- 项目名 = `cwd.file_name()`，与 `app-server/src/lead.rs:136` 的 `lead_title_for_cwd` **同一判据**；取不到（`/`、空、非 UTF-8）时回退 `yi-agent`。
- 整条标题（含 `● ` 两列）按**显示列宽**截断到 **24 列**。超出部分以 `…` 收尾。
- 圆点为静态，整个会话只有「有/无」两态。
- 退出时写空串，把标签名还给用户终端。

**为什么 24 列**：终端标签宽度普遍 15-25 列，24 是「在不挤掉两侧标签的前提下尽量多显示」的折中；再长终端自己也会截断，而终端的截断不受我们控制（可能切在 CJK 中间）。

**为什么按显示列宽而非字节/字符**：中文路径一字占两列，按 `chars().count()` 截断会得出「24 个字符」但实际 48 列，标签栏错位。`tui/wrap.rs:94` 的 `truncate_to_width` 已用 `UnicodeWidthChar`，且有 `truncate_to_width_keeps_cjk_within_budget`（`:197`）测试。

## 5. 模块结构

新建 `yi-agent-rs/crates/yi-agent/src/tui/title.rs`，**与 `notify/` 平级**，不进 `notify/`：

> 通知是**离散事件**（攒候选、去抖、按优先级择一、丢弃无所谓），标题是**持续状态**（有当前值、需要 diff、退出必须还原）。两者生命周期与失败语义不同，混在一个模块里会让 `NotificationState` 的「至多一条候选」语义被标题的「状态机」污染。

```
title.rs
├── pub fn project_label(cwd: &Path) -> String
│     取 file_name，非法/空回退 "yi-agent"
├── pub fn compose(project: &str, running: bool) -> String
│     running 加 "● " 前缀 → sanitize → truncate_to_width(24)；纯函数
├── struct SetTitle(String)
│     impl crossterm::Command；write_ansi 写 "\u{1b}]2;{title}\u{7}"
├── pub struct TitleState {
        current: Option<String>,        // 上一次真正写出去的值，用于 diff
        last_running: Option<bool>,     // 上一次观察到的运行态，用于避免每帧重组
    }
└── pub struct TitleWriter {
        enabled: bool,
        disabled_reason: Option<DisabledReason>,   // 自禁后置位
    }
    ├── pub fn new(enabled: bool, multiplexed: bool) -> Self
    ├── pub fn status(&self) -> TitleStatus                    // 供 /config 渲染
    ├── pub fn sync(&mut self, state: &mut TitleState, project: &str, running: bool) -> io::Result<()>
    │     运行态未变则直接返回；变了才 compose 再写
    └── pub fn clear(&mut self, state: &mut TitleState) -> io::Result<()>   // 退出还原
```

两个枚举把 `/config` 的四种文案从散落的布尔判断收成一处，避免渲染层自己拼字符串：

```rust
/// 跳过原因。`None` = 正常可用。
pub enum DisabledReason {
    Off,            // 配置关
    Multiplexer,    // tmux / zellij
    WriteFailed,    // 写过一次失败，本会话自禁
}

/// `/config` 的四态展示值。
pub enum TitleStatus { On, Off, Multiplexer, WriteFailed }

impl DisabledReason {
    fn to_status(self) -> TitleStatus { /* 一一对应 */ }
}
```

`new(enabled, multiplexed)` 的两个参数**不是互斥的**：配置关且身处 tmux 时，`enabled=false` 优先——用户明确关掉的东西，没必要再报告环境限制。判定顺序在 `new` 里固定为 `enabled` → `multiplexed`。

`SetTitle` **自己实现** `Command`，不直接用 crossterm 自带的 `terminal::SetTitle`（`crossterm-0.28.1/src/terminal.rs:386` 确实存在）。理由：本仓已确立「自定义 `Command` + 可断言 `write_ansi`」的范式（`osc9.rs:28`、`bel.rs:14`），且测试需要逐字节断言；直接依赖 crossterm 的类型会让字节无法在单测里固定。

**终字节选择**：用 BEL（`\u{7}`）而非 ST（`\u{1b}\\`）终止，与 `osc9.rs` 一致。BEL 是 OSC 的传统终止符，兼容面最广；ST 更规范但少部分老终端不认。

## 6. 触发、投递与失败

### 6.1 写时机

三个触发点，全部走 `TitleWriter::sync` / `clear`，靠 `TitleState.current` 做 diff——**值没变就不写**。

| 时机 | 位置 | 写入值 |
|---|---|---|
| 启动 | `tui/app.rs:159`（`Terminal::new` 之后、`run_loop` 之前） | `compose(project, false)` |
| 每帧（运行态翻转时） | `tui/app.rs:871` 的 flush 窗口 | `compose(project, is_running)` |
| 退出清理 | `tui/app.rs:197-210`，`LeaveAlternateScreen` **之后** | `""` |

**为什么每帧检查却只写 2 次**：`run_loop` 每帧已经读一次 `is_running`（`app.rs:2208`、`:2219` 的取消键判定就是这么做的），这里只是**多读一次**这个已经加载到缓存行的原子量，成本可忽略。而真正写字节由 `TitleState.current` 的 diff 拦住——整个会话里运行态只翻转两次，所以实际写入也只有两次。

**为什么走 `:871`**：那里已在 `terminal.draw()` 之后、两帧之间，是全仓唯一「保证不会把 OSC 序列插进一帧 ANSI 中间」的位置。`notifications.take()`（`:871`）与标题更新并列即可，不改变既有结构。

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
let running = is_running.load(Ordering::Relaxed);
let _ = title_writer.sync(&mut title_state, &project_label, running);
```

`sync` 内部先比 `state.last_running`，相同就直接返回——不重组字符串、不碰 stdout。只有翻转时才走 `compose` + 写字节。

这比事件路径更稳：`AgentEvent` 出现过多次「某条路径不发 Done」的问题（`main.rs:3026-3028` 的注释就记着「取消路径压根不发 Done，只挂 Done 会让每次 Ctrl+C 的那一轮静默丢失」），而复用驱动任务已经统一收敛好的那个布尔量，天然继承同一个修复。

延迟上界是一帧（`events.poll(33ms)`，`app.rs:886`），人眼不可感知。

**关键决策：取消与出错也回空闲。** 通知功能（spec `2026-11-01-terminal-notification-design.md` §3）刻意**不**为失败/中断发通知，因为那是离散提醒、会刷屏。但标题的语义不同——它回答的是「**现在**还在跑吗」。一个被 `Ctrl+C` 中断的回合如果标题仍挂着 `●`，标签栏会一直谎报，比不显示更糟。

本方案让这条自动成立：`store(false)` 覆盖全部路径，标题自然回空闲。**这是选择读原子量而非枚举事件的第二个理由**——如果用 `AgentEvent` 枚举，就得逐个列出 `Done{EndTurn}`、`Done{MaxTurns}`、`Done{Interrupted}`、`Cancelled`、`Error` 五个分支，漏一个就是「标签栏永久挂着圆点」的 bug。

### 6.3 跳过条件

`sync` 在以下任一成立时**直接返回 `Ok(())` 且不写一个字节**：

| 条件 | 判定 | `/config` 显示 |
|---|---|---|
| 配置关闭 | `enabled == false` | `title: off` |
| stdout 非 tty | `std::io::stdout().is_terminal()`（与 `notify/mod.rs:228` 同判据） | — |
| tmux / zellij 内 | `notify::is_multiplexed(&env)`（`mod.rs:153`） | `title: disabled (multiplexer)` |
| 已自禁（写失败过一次） | `disabled_reason.is_some()` | `title: disabled (write failed)` |

多路复用器判定在**构造时**做一次，会话内不变（与 `Osc9Backend::new` 的 `dcs_passthrough` 同做法，`osc9.rs:46-55`）。

### 6.4 失败自禁

写失败时的处理与 `Notifier`（`notify/mod.rs:236-240`）逐条对齐：

- `tracing::warn!` 记一次，`disabled_reason` 置位。
- 之后所有 `sync` 立即返回 `Ok(())`——终端已不可用，每回合重试只会刷日志。
- **不向调用方传播错误**。

### 6.5 退出还原绝不能影响退出码

现有清理段（`app.rs:197-210`）是「每步都试、只记第一个错误」，而这个错误会**变成 `run_tui` 的返回值**：

```rust
match (result, cleanup_error) {
    (Err(error), _) => Err(error),
    (Ok(_), Some(error)) => Err(error),      // ← 清理失败 ⇒ 退出码非零
    (Ok(_), None) => Ok(()),
}
```

标题还原**必须**走独立路径：`let _ = restore_title();`，不进 `cleanup_error` 的比较链。否则在上百种不支持 OSC 2 的终端组合上，每次正常退出都会返回非零，`yi-agent` 会被 shell 的 `set -e`、CI 脚本、以及用户自己的判断当成崩溃。

这是本设计里**最容易写错的一处**，§8 有专门的回归测试。

## 7. 配置与可观测性

### 7.1 配置键

```json
{
  "tui": {
    "terminal_title": true
  }
}
```

- 类型 `Option<String>`，与同组三个通知键完全同构（`yi-agent-runtime/src/settings.rs:132-138`）。`None` = 用默认值（删键恢复默认）。
- 默认 `true`。理由：这正是功能价值所在；退出会还原，无持久副作用。
- 需要 `FieldSpec` 登记（`settings.rs:371-398` 同型），`group: "tui"`、`label: "终端标题"`、`field_type: FieldType::Select`、`options: &["true", "false"]`、`default_display: "true"`——桌面端与 web 设置界面直接渲染这张表（`settings.rs:168-169` 注释）。
- 解析进 `RuntimeConfig`（`yi-agent-runtime/src/config.rs:126` 旁）与 `RuntimeConfig::default()`（`config.rs:531` 旁）。

### 7.2 `/config` 显示

在现有 `terminal notify:` 行（`tui/slash.rs:333-343`）之后加一行，四行道：

```
  terminal title: on
  terminal title: off
  terminal title: disabled (multiplexer)
  terminal title: disabled (write failed)
```

`TuiConfigSnapshot`（`tui/slash.rs:298-302` 旁）加一个 `title_status: String` 字段，在 `main.rs:2340-2380` 构造快照处填值。

**为什么必须显示**：tmux 内静默跳过是个**不可见的降级**。不显示的话，tmux 用户会以为功能坏了却查不到原因——这正是 `docs/bug-list.md:70` 记过的教训：「系统在报告、报告被丢弃」。与通知功能显示 `effective_backend`（`slash.rs:329-333`）同理。

## 8. 测试

### 8.1 纯函数（`title.rs` 内）

| 测试 | 断言 |
|---|---|
| `compose_idle_is_just_the_project` | `compose("myproject", false) == "myproject"` |
| `compose_running_adds_dot_prefix` | `compose("myproject", true) == "● myproject"` |
| `compose_truncates_to_24_columns` | 长名截断且总宽度 ≤ 24、以 `…` 收尾 |
| `compose_measures_cjk_by_display_width` | 24 个汉字 → 实际 48 列，必须截到 ≤ 24 列 |
| `compose_strips_control_characters_from_project` | 项目名含 `\u{1b}]2;pwned\u{7}` 时不产出裸 ESC |
| `project_label_uses_last_path_segment` | `/work/my-app` → `my-app` |
| `project_label_falls_back_for_root` | `/` → `yi-agent` |

### 8.2 编码（逐字节）

| 测试 | 断言 |
|---|---|
| `set_title_writes_exactly_one_osc2` | `write_ansi` 输出 `== "\u{1b}]2;myproject\u{7}"` |
| `set_title_with_empty_string_still_terminates` | 输出 `== "\u{1b}]2;\u{7}"` |

照 `bel.rs:52-65` 的 `the_command_writes_exactly_one_bel` 范式：断言 `Command` 真正写出的字节，而不只是常量。只测常量的话，把 `write_ansi` 改成空实现仍然全绿，用户却什么都看不到。

### 8.3 策略（`TitleWriter`）

| 测试 | 断言 |
|---|---|
| `disabled_writer_writes_nothing` | `enabled=false` 时 `sync` 返回 `Ok`，`state.current` 保持 `None` |
| `multiplexed_writer_writes_nothing` | 同上，`status()` 为 disabled |
| `non_tty_writes_nothing` | `cargo test` 的 stdout 非 tty，恰好覆盖该短路（照 `notify/mod.rs:397-424` 的 `notify_is_a_no_op_when_stdout_is_not_a_tty` 写法，含「真终端则跳过」而不是给假结论） |
| `sync_is_a_no_op_when_running_is_unchanged` | 连续两次 `sync(running=true)`，第二次不改变 `state.current` |
| `sync_rewrites_when_running_flips` | `true` → `sync` 写入 `● p`；`false` → 写入 `p` |
| `clear_empties_the_title` | `clear` 后 `state.current == Some("")` |
| `a_disabled_writer_stays_quiet_and_ok` | 强制置自禁后 `sync` 仍返回 `Ok` |
| `status_maps_each_disabled_reason` | `Off` → `off`、`Multiplexer` → `disabled (multiplexer)`、`WriteFailed` → `disabled (write failed)`、`None` → `on`；驱动 `/config` 三种文案全部覆盖 |
| `off_wins_over_multiplexer` | `new(enabled=false, multiplexed=true).status()` 是 `off`，不是 `disabled (multiplexer)` |

### 8.4 集成（`tui/app.rs` 测试模块）

复用既有 `ResizableTestBackend`（`app.rs:5302` 已有先例），跑真实 `run_loop`。测试把 `is_running` 这个 `Arc<AtomicBool>` 传进去，在事件轮次之间翻转它：

| 测试 | 断言 |
|---|---|
| `title_follows_the_running_flag` | 置 `true` 跑一帧 → 标题 `● p`；置 `false` 再跑一帧 → 标题 `p` |
| `title_updates_at_most_twice_per_turn` | 连续 60 帧 `true` 期间，实际写入次数为 1（diff 拦住重复写） |
| `cancelled_turn_returns_title_to_idle` | 置 `false`（等价于 `main.rs:3049` 的收尾）→ 标题回空闲。**这是与通知功能相反的关键行为**：`Done{Interrupted}` 不发通知，但必须还原标题 |
| `error_path_returns_title_to_idle` | 同上，验证 `main.rs:3023` 的 `Err(e)` 分支也会走到 `store(false)` |

第二条是本次简化的核心保障：如果 diff 失效，60 帧会写 60 次，标签栏在每个终端上都会明显闪烁。

### 8.5 回归护栏（最重要的一条）

| 测试 | 断言 |
|---|---|
| `title_restore_failure_does_not_change_exit_code` | 注入一个必然失败的标题还原，断言 `run_tui` 仍返回 `Ok`。变异验证：把 `let _ =` 改成进 `cleanup_error`，此测试必须变红 |

### 8.6 验收命令

```bash
cargo test -p yi-agent --bin yi-agent tui::title
cargo test -p yi-agent --bin yi-agent title
cargo test -p yi-agent-runtime settings
cargo test -p yi-agent-runtime tui_      # 通知组回归
just ci
```

## 9. 风险与取舍

| 风险 | 应对 |
|---|---|
| 用户的终端/多路复用器不认 OSC 2 | 失败即静默自禁，不报错、不改退出码；`/config` 可查状态 |
| 标题还原失败导致退出码非零 | §6.5 独立路径 + §8.5 专门回归测试 |
| 项目目录名含控制字符造成终端注入 | `notify::text::sanitize` 在 `compose` 内先跑；§8.1 有对应用例 |
| 每帧写标题造成闪烁 | §6.1 的 diff：值不变不写；整个会话最多写 2 次（开始/结束） |
| tmux 用户被覆盖自定义标题 | §6.3 直接跳过，不写字节 |
| 24 列在实际终端里仍偏长 | 常量集中一处，后续按反馈调整；不影响结构 |
| 与通知功能的行为不一致引起困惑 | §6.2 明确记录「通知不为失败发、标题为失败还原」的相反取舍及理由 |

## 10. 与既有通知设计的关系

| 维度 | 通知（OSC 9 / BEL） | 标题（OSC 2） |
|---|---|---|
| 性质 | 离散事件提醒 | 持续状态覆盖 |
| 触发 | 回合完成、权限请求 | 回合开始/结束、启动、退出 |
| 频率 | 每回合至多 1 次 | 每回合 2 次，有 diff |
| tmux | DCS 透传（`osc9.rs:17-22`） | **跳过不写** |
| 失败/中断 | **不发**（避免刷屏） | **回空闲**（避免谎报） |
| 失败处理 | 自禁 | 自禁 |
| 模块 | `tui/notify/` | `tui/title.rs` |

两处「故意相反」的取舍都是**语义驱动**而非风格差异：通知是「值得打断你吗」，标题是「现在是什么状态」。混为一谈会同时做错两件事。
