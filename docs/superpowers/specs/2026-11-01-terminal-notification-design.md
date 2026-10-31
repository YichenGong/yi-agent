# 终端通知（BEL / OSC 9）设计

日期：2026-11-01
状态：设计已确认，待实现

## 1. 问题

TUI 里跑一个长回合时，用户会切到别的窗口。回合结束时没有任何信号，只能不断切回来确认。当 agent 卡在权限确认弹窗等待时，这个成本更高——它真的停在那里等，而用户不知道。

终端本身提供了三种标准的通知机制，实现成本极低：

- **BEL**（`0x07`）：最古老，几乎全部终端支持。
- **OSC 9**（`ESC ] 9 ; <text> ST`）：iTerm2 起源（2010 年前），Ghostty、kitty、WezTerm、Warp 跟进。
- **OSC 777**（`ESC ] 777 ; notify ; <title> ; <body> ST`）：urxvt 扩展，WezTerm 支持。

当前 yi-agent 的 CLI 与 TUI **完全没有这部分实现**。全仓核查结果：

| 搜索项 | 结果 |
|---|---|
| `0x07` / `\x07` / `\u{7}` 在 `tui/theme.rs` 之外 | 0 处 |
| 写出 OSC 的位置 | 仅 `tui/theme.rs:322`，查询终端背景色 `\x1b]11;?\x1b\\` |
| CLI 参数中的通知开关 | 无（`config.rs:15-92`、`config.rs:96-205`） |

`tui/theme.rs:377` 确实出现 `0x07`，但那是**读**方向：解析 OSC 11 回复时把 BEL 当终止符，与发送通知无关。

## 2. 参照实现

本设计以本地 Codex 源码（`OpenSource/codex`，HEAD `67f0994b53`）为基准，逐项对齐其架构与行为。相对它共有六处差异：**三处是缺陷修正**（Windows 焦点、正文消毒、zellij 透传），**三处是范围减法**（不做 OSC 777、不做按类型白名单、不做热更新）。另有若干处是**主动加严**：markdown 剥离（Codex 无）、截断按字素（Codex 已按字素，此项一致）。汇总见 §8。

Codex 的实现分布：

| 文件 | 职责 |
|---|---|
| `config/src/types.rs:720-835` | `Notifications` / `NotificationMethod` / `NotificationCondition` / `TuiNotificationSettings` |
| `tui/src/notifications/mod.rs` | 后端枚举、`auto` 选择、`supports_osc9` 白名单 |
| `tui/src/notifications/bel.rs` | `PostNotification` → `\x07` |
| `tui/src/notifications/osc9.rs` | OSC 9，含 tmux DCS 包裹 |
| `tui/src/tui.rs:1056-1080` | `Tui::notify`：焦点条件 + 失败自禁 |
| `tui/src/tui.rs:123-128` | `should_emit_notification` |
| `tui/src/tui.rs:268-270` | `EnableFocusChange` / `DisableFocusChange` |
| `tui/src/tui/event_stream.rs:306-313` | `FocusGained/Lost` → `AtomicBool` |
| `tui/src/chatwidget/notifications.rs` | `Notification` 枚举、优先级、去抖、文案 |
| `tui/src/app.rs:1184` | `maybe_post_pending_notification`（绘制间隙 flush） |

## 3. 范围

**做：**

- 全局 `settings.json` 新增 `tui` 组，三个键：`notifications`、`notification_method`、`notification_condition`。
- 两个后端：`bel`、`osc9`。`auto` 按终端识别选择。
- 两个触发面：回合正常完成（`Done { reason: EndTurn }`）、权限请求（`AgentEvent::PermissionRequest`）。
- 焦点条件：`unfocused`（默认）/ `always`。
- 正文摘要与消毒。
- `/config` 只读输出中显示三项配置及解析后的实际后端。

**不做：**

- **OSC 777**。唯一优势是能带标题，但支持者只有 urxvt 与 WezTerm，而 WezTerm 同样支持 OSC 9。为一个几乎不存在的终端组合增加一个后端、一组分号转义规则和一批测试，属于 YAGNI。将来若要做，只作为显式 `method` 值，**不进入 `auto` 的自动选择**（否则 `auto` 需要回答一个永远没有正确答案的问题）。
- **按类型白名单**。Codex 的 `Notifications` 是 `bool | Vec<String>` 的 untagged enum，我们本次只有两类事件，白名单不产生价值。只实现 `bool`。将来加数组形态是向后兼容的扩展。
- **失败 / 中断通知**。`DoneReason` 有三个变体（`yi-agent-core/src/agent.rs:436-440`）：`EndTurn`、`MaxTurns`、`Interrupted { reason }`。只有 `EndTurn` 发通知；`MaxTurns`（撞上轮次上限，不是正常结束）与 `Interrupted`（含用户 `Ctrl+C`）都不发。`AgentEvent::Error` 与 `AgentEvent::Cancelled` 同样不发。失败时 TUI 已有醒目的红色错误行；`Ctrl+C` 说明用户人就在键盘前。这两种情况的通知价值远低于「你切走了但结果出来了」，且一旦默认开启会在 CI 与无人值守场景刷屏。
- **配置热更新**。Codex 有 `set_notification_settings` 供设置面板调用，但我们的 TUI 没有设置面板（`/config` 只渲染只读快照，`tui/slash.rs::TuiConfigSnapshot::render`），没有触发源。配置改动**下次启动生效**。
- **headless（`yi-agent run`）通知**。`run --json` 的 stdout 是逐行 JSONL，是端到端测试的契约（见 `docs/project-management/yi-agent-run.md`），注入控制字符会直接破坏它。headless 完全不动。

## 4. 配置模型

落点 `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`，文件是 `<instance_home>/settings.json`（`YI_AGENT_HOME` 感知，见 `yi-agent-store/src/paths.rs:20`）。

```json
{
  "tui": {
    "notifications": true,
    "notification_method": "auto",
    "notification_condition": "unfocused"
  }
}
```

三个字段都是 `Option<T>`，`None` = 用默认值（与 `Settings` 现有约定一致，`settings.rs:70-82`）：恢复默认等于删键，而不是把当前默认值写回去。

| 键 | 类型 | 默认 | 取值 |
|---|---|---|---|
| `tui.notifications` | `Option<bool>` | `true` | — |
| `tui.notification_method` | `Option<String>` | `"auto"` | `auto` / `osc9` / `bel` |
| `tui.notification_condition` | `Option<String>` | `"unfocused"` | `unfocused` / `always` |

`notification_method` 与 `notification_condition` 存字符串而非枚举，沿用 `startup.sandbox` 的理由（`settings.rs:112-118`）：非法值要能被加载侧识别并 warn 回退到默认，而不是让整份 JSON 反序列化失败，连带丢掉用户的其它字段。

三个字段同时登记进 `settings.rs` 的 `FIELDS`（`settings.rs:146` 起）。该表驱动 web 与桌面端设置页，登记即获得 UI，无需另写表单。

## 5. 架构

新增模块 `yi-agent-rs/crates/yi-agent/src/tui/notify/`：

```
notify/
├── mod.rs        后端枚举、auto 选择、焦点条件、失败自禁
├── bel.rs        BEL 后端
├── osc9.rs       OSC 9 后端（含 tmux / zellij 透传）
└── text.rs       正文摘要与消毒（纯函数）
```

`text.rs` 不碰 IO，可独立单测；`bel.rs` / `osc9.rs` 只负责字节编码；`mod.rs` 持有策略。这样「协议怎么编码」与「什么时候发」解耦，后者能在 `tui/app.rs` 的假事件源里测。

### 5.1 协议编码

用 crossterm 的 `Command` trait 实现 `write_ansi`，与 Codex 同法。好处是测试可以直接断言字节，不需要真实终端。

| 后端 | 非多路复用器 | tmux / zellij |
|---|---|---|
| BEL | `\x07` | `\x07`（不包裹） |
| OSC 9 | `\x1b]9;{msg}\x07` | `\x1bPtmux;\x1b\x1b]9;{msg}\x07\x1b\\` |

tmux 包裹时消息内的 `ESC` 必须翻倍为 `ESC ESC`（DCS 透传要求），否则外层终端会把内层 ESC 当透传结束符。

BEL 不做包裹（与 Codex 一致）：BEL 是普通控制字符，tmux 会原样透传。

zellij 复用 tmux 的包裹形式（zellij 明确兼容 `ESC Ptmux;` 透传），判据是环境变量 `ZELLIJ` 存在。**这是相对 Codex 的第 3 处修正**——Codex 只认 `Multiplexer::Tmux`，在 zellij 里 OSC 9 会静默丢失。

### 5.2 auto 选择

严格对齐 Codex `notifications/mod.rs:53-62` 的**五家白名单**：

```rust
matches!(name,
    Ghostty | Iterm2 | Kitty | WarpTerminal | WezTerm)
```

不扩表。这张表是「实测能弹出系统通知」的集合，不是「协议支持」的集合；扩表必须先实测，本次没有实测预算。显式 `method = osc9` 可让任何终端走 OSC 9。

与 `supports_osc9`（`notifications/mod.rs:53-62`）逐字对齐：恰好这五家，其余一律 BEL。Codex 的测试 `supports_osc9_for_unsupported_terminals`（`notifications/mod.rs:117-131`）把 `Alacritty`、`Dumb`、`GnomeTerminal`、`Konsole`、`Vte`、`VsCode`、`WindowsTerminal`、`Unknown` 全部钉在 false 上。

#### 5.2.1 终端识别判据

**必须可注入环境。** Codex 的识别函数签名是 `fn detect_terminal_identity_from_env(env: &dyn Environment)`（`lib.rs:289`），`Environment` 是一个只含 `var` / `var_non_empty` / `has` / `has_non_empty` 的私有 trait（`lib.rs:214`），真实进程环境经 `ProcessEnvironment` 传入，测试传入一个 `HashMap` 即可。

我们照这个形状做，不直接读 `std::env`：

```rust
#[cfg_attr(test, derive(Default))]
pub struct TerminalEnv(BTreeMap<String, String>);
impl TerminalEnv { pub fn from_process() -> Self { /* std::env::vars */ } }
```

理由很实际：用 `std::env::set_var` 写测试会污染其它并行测试（而且 `set_var` 在 Rust 2024 已是 `unsafe`）。注入式环境让「五位白名单 + 五种兜底 + 两个多路复用器」全部可以在无终端环境下断言。

Codex 把识别做成独立的 `terminal-detection` crate（`codex-rs/terminal-detection/src/lib.rs`）。我们**不移植整个 crate**，只实现本功能需要的五种识别，顺序照抄 `detect_terminal_identity_from_env`（`lib.rs:289-337`）：

| 顺序 | 判据 | 结论 |
|---|---|---|
| 1 | `TERM_PROGRAM` 非空且不是 `tmux` | 归一化后匹配：`iterm`/`iterm2`/`itermapp` → iTerm2；`ghostty` → Ghostty；`kitty` → Kitty；`warp`/`warpterminal` → Warp；`wezterm` → WezTerm；其余 → Unknown |
| 2 | `GHOSTTY_RESOURCES_DIR` 存在 | Ghostty |
| 3 | `WEZTERM_VERSION` 存在 | WezTerm |
| 4 | `ITERM_SESSION_ID` / `ITERM_PROFILE` / `ITERM_PROFILE_NAME` 存在 | iTerm2 |
| 5 | `KITTY_WINDOW_ID` 存在，或 `TERM` 含 `kitty` | Kitty |
| 6 | `ALACRITTY_SOCKET` 存在，或 `TERM == "alacritty"` | Alacritty（→ BEL） |
| 7 | `WT_SESSION` 存在 | Windows Terminal（→ BEL） |
| — | 都不匹配 | Unknown → BEL |

第 6、7 条不产生 OSC 9（两家都不在白名单），写出来是为了让「Unknown 回退 BEL」这条路径走得准：Alacritty 与 Windows Terminal 的 `TERM` 常用 `xterm-256color`，若不先识别成具体终端，就只是碰巧落到 BEL——将来若把某家加进白名单，缺这两条会静默错判。`VTE_VERSION` / `KONSOLE_VERSION` / `GNOME_TERMINAL_SCREEN` 同样只影响识别精度、不影响后端选择，本次**不实现**（见 §3 的 YAGNI 判断）。

归一化规则照 `lib.rs:412-418`：`trim` 后剔除空格、`-`、`_`、`.`，转小写。因此 `WarpTerminal` 与 `warp` 等价。Codex 还把 `appleterminal`/`rex`/`vscode`/`konsole`/`gnometerminal`/`vte`/`dumb`/`windowsterminal` 归一化到各自的 `TerminalName`（`lib.rs:419-434`）——这些都不在白名单，我们统一归为「非白名单」即可，不必逐一区分。

**注意**：`TERM=xterm-kitty` 不是主判据，`KITTY_WINDOW_ID` 优先；`TERM` 匹配只是兜底。早期版本的 Codex 曾用 `TERM=xterm-ghostty`（`lib.rs:142`），现在 `GHOSTTY_RESOURCES_DIR` 更可靠。

多路复用器单独判定，与终端名正交（`detect_multiplexer`，`lib.rs:368`）：

| 判据 | 结论 |
|---|---|
| `TMUX` 或 `TMUX_PANE` 非空 | tmux |
| `ZELLIJ` / `ZELLIJ_SESSION_NAME` / `ZELLIJ_VERSION` 任一非空 | zellij（第 3 处修正） |

三者都判「非空」而非「存在」：`TMUX=` 空串在部分 shell 里是残留，按存在判会误判成 tmux 环境（`lib.rs:368-385` 用的正是 `has_non_empty`）。

`TERM_PROGRAM=tmux` 时第 1 条识别被跳过（`is_tmux_term_program`），否则会把 tmux 本身当成终端名——此时真实终端信息要靠 `TERM` 兜底（第 5 条仍可能命中 kitty）。

## 6. 触发与投递

三层，对齐 Codex。

### 6.1 事件 → 候选通知

在 `tui/app.rs` 事件循环中从 `AgentEvent` 映射：

| AgentEvent | 类型 | 优先级 | 文案 |
|---|---|---|---|
| `Done { reason: EndTurn }` | `TurnComplete` | 0 | 正文摘要（空白折叠后截 200 字素，见 §7.1） |
| `PermissionRequest { tool_name, tool_input, .. }` | `ApprovalRequested` | 1 | `等待批准: <permission_summary 结果>`（截 200 字素，见下） |
| `Done { reason: MaxTurns }` / `Done { reason: Interrupted { .. } }` / `Error` / `Cancelled` | — | — | 不发 |

`PermissionRequest` 的文案**复用既有 `permission_summary`**（`tui/history.rs:119-141`），不另写摘要逻辑。

该函数已经处理好了三类工具的差异，且有三条既有测试钉住行为（`history.rs:3951`、`:3980`、`:4007`）：

| 工具 | 摘要形态 |
|---|---|
| `bash` / `process_start` | 直接取 `command`，丢掉 `expected_timeout_sec` 等 JSON 噪音 |
| `write` / `edit` | `path: <路径>, content: N bytes`，**绝不内联正文块** |
| 其它 | `input.to_string()` |

复用的理由不是省代码，而是**一致性**：同一个权限请求在转录里和在通知里必须说同一件事。若通知另写一套，`write` 场景就会把 500 字节的正文塞进通知（`history.rs:3980` 的测试正是防这个）。

通知文案 = `format!("等待批准: {}", permission_summary(tool_name, tool_input))`，再经 §7 的 `summarize`（会剥掉 `path:` 这类前缀中的标记并截断）与 `sanitize`。截断取 200 字素，与其他通知统一（Codex 对命令用 30，见 `chatwidget/notifications.rs:43-47`；我们的 `permission_summary` 对 `write`/`edit` 已压成「路径 + 字节数」，30 字素会连路径都截断，故放宽到 200）。

需要把 `permission_summary` 从 `tui/history.rs` 的私有函数提升为 `pub(crate)`，或移到 `tui/cell.rs` 旁的公共位置——实现时择一，以不破坏 `history.rs` 现有测试为准。

### 6.1.1 回合完成的正文来源

`AgentEvent::Done { reason }` **不携带回复正文**（`yi-agent-core/src/agent.rs:343-345`），`history.push_event` 收到它只压一条 `Separator`（`tui/history.rs:1101-1104`）。正文在流式阶段已逐段写进最后一条 `HistoryCell::AssistantMessage { markdown }`。

因此摘要来源是**转录的最后一条 assistant cell**，取值时机必须在处理 `Done` **之前**（`push_event` 会把 `Separator` 压到末尾，之后 `cells.last()` 就不再是 assistant）。

实现位置在 `tui/app.rs` 的事件循环里，`history.push_event(event, ...)` 之前（现为 `app.rs:568`）：

```rust
if let AgentEvent::Done { reason: DoneReason::EndTurn } = &event {
    pending_notification = coalesce(
        pending_notification,
        PendingNotification::TurnComplete(history.last_assistant_text()),
    );
}
```

需要给 `HistoryState` 新增 `last_assistant_text(&self) -> Option<&str>`：从尾部反向找第一个 `HistoryCell::AssistantMessage { markdown }`（`tui/cell.rs:18`），返回 `markdown` 原文。注意 `Reasoning { text }` 是思维链、显示专用，**不可**当作回复正文取用。

三个必须处理的边界：

- **该回合没有任何 assistant 文本**（例如只调了工具就结束）。返回 `None`，通知文案退回固定句 `yi-agent 回合完成`，不发空通知。
- **流式中途被压缩**（`AutoCompacting`）。压缩会改写转录，摘要取到的是压缩后的内容——这是可接受的，因为用户看到的也是压缩后的版本。
- **只取本轮**。不跨回合拼接：`last_assistant_text` 只返回最后一条，不做多段合并。

### 6.2 去抖

`pending_notification: Option<PendingNotification>`。新候选优先级 ≤ 已有则丢弃，> 则替换（对齐 Codex `chatwidget/notifications.rs:6-16`）。同一帧内多个 `PermissionRequest` 只留第一个。

### 6.3 绘制间隙 flush

候选**不立即写字节**。每次事件循环**绘制完成后**统一 flush 一次（对应 Codex `app.rs:1184` 的 `maybe_post_pending_notification`）。

这条是硬约束：通知字节若插在一帧 ANSI 序列中间，会把该帧切开，造成渲染残影。落到绘制之后，保证字节永远位于两帧之间。

### 6.4 焦点条件

- 启动时 `execute!(stdout, EnableFocusChange)`，退出时 `DisableFocusChange`。
- `Event::FocusGained` / `FocusLost` 维护 `Arc<AtomicBool>`，初值 `true`。
- `unfocused` = `!focused`；`always` = 恒真。

**第 1 处修正（Windows）**：Codex 在 Windows 上执行 `DisableFocusChange`（`tui.rs:269`），而初始 `focused = true`，导致 `unfocused` 条件恒不成立——Windows 用户等于没这个功能。我们的做法是：

- Windows 上**同样尝试** `EnableFocusChange`。
- 若启用失败（老 conhost 不支持），把 `unfocused` 按 `always` 处理，并在首次降级时打一条 `tracing::info`。

即 Windows 上宁可多响，不静默。功能存在的意义就是提醒；静默等于装了个假开关。

### 6.5 失败自禁

`notify()` 返回 `Err` 时置 `notification_backend = None` 并 `tracing::warn`，本会话不再尝试（对齐 Codex `tui.rs:1067-1078`）。写 stdout 失败通常意味着终端已不可用，继续每回合尝试只会刷日志。

`notify()` 入口先判 `std::io::stdout().is_terminal()`，非 TTY 直接返回成功且不发。TUI 天然是 TTY，这条防的是 TUI 被套进管道的异常场景。

### 6.6 可观测性

`/config`（`tui/slash.rs` 的 `TuiConfigSnapshot::render`，`slash.rs:299-305`）的只读输出追加一行：

```
  terminal notify: on（method=auto → osc9, condition=unfocused）
```

三个信息必须同时出现：开关键、**解析后的实际后端**、条件。解析结果是 Codex 没有的——只显示 `auto` 时，用户排查「为什么我的 kitty 不弹」没有任何线索。

Windows 上若 `EnableFocusChange` 失败降级（§6.4），该行追加标注：

```
  terminal notify: on（method=auto → bel, condition=unfocused→always 降级：本终端不支持焦点事件）
```

降级必须可见。用户在配置里写的是 `unfocused`、实际行为是 `always`，若不显示，这个差异永远无法自查。

`TuiConfigSnapshot` 新增五个字段：`notifications: bool`、`method: String`（配置值）、`effective_backend: String`（解析结果）、`condition: String`（生效条件）、`focus_degraded: bool`。由 `run_tui` 组装处填充；`app.rs:220` 的 `snapshot_for_tests()` 需同步补齐，否则测试构造点会编译失败——这正好保证新字段不会漏填。

## 7. 正文摘要与消毒

`notify/text.rs`，纯函数。

### 7.1 summarize

顺序固定，前四步与 Codex **不同**（Codex 没有 markdown 剥离，只做空白折叠），后两步对齐 `chatwidget/notifications.rs:101-114` 与 `text_formatting.rs:70-92`：

1. 丢弃代码围栏块（``` 之间的内容整体跳过）
2. 去掉行首 markdown 标记：`#`、`>`、`-`、`*`、`1.`
3. 去掉行内标记：`**`、`*`、`` ` ``、`[text](url)` → `text`
4. **空白折叠为单空格**（含换行——不是「只取首行」）
5. 按**字素簇**截 **200**（不是字节、不是 `char`——中文与 emoji 不会被切半）
6. 截断时保留总量：留前 **197** 个再补 `...`，使可见字素总数恰为 200
7. 空结果 → 回退固定文案 `yi-agent 回合完成`

**第 4 步与 Codex 一致的原因**：Codex 用 `response.split_whitespace()` 折叠（`notifications.rs:101-113`），换行因此变成空格。若只取首行，多行回复会丢掉用户最想看的结论段——模型常以「以上/结论」收尾。折叠保留全部语义，代价是通知里只有一行，这本来就是系统通知的形态。

**第 5、6 步的数字来自 Codex**：`AGENT_NOTIFICATION_PREVIEW_GRAPHEMES = 200`（`chatwidget/notifications.rs:134`），`truncate_text` 在 `max_graphemes >= 3` 时截到 `max_graphemes - 3` 再补 `...`（`text_formatting.rs:70-92`）。注意是 ASCII 三个点，不是 `…` 单字符。

**字素簇的实现**：`unicode-segmentation` 已在 `Cargo.lock`（v1.13.3，经 `unicode-truncate` 传递引入），但**不在** `yi-agent` 的直接依赖里。本项目现有截断（`tui/bash_popup.rs:296`、`tui/cell.rs:525`、`tui/wrap.rs:94`）全部用 `chars()`，按 `char` 而非字素。

本模块**新增直接依赖** `unicode-segmentation = "1"`（workspace 已有该 crate 的锁定版本，不引入新的下载）。理由：`chars().take(200)` 会把 `👨‍👩‍👧` 这类 ZWJ 序列切断，产生半个 emoji 显示为乱码方块——通知里出现方块比少显示几个字更糟。依赖面很小，且该 crate 已在构建图内。

### 7.2 sanitize

剥掉全部 C0（`U+0000`–`U+001F`）、`U+007F`（DEL）、C1（`U+0080`–`U+009F`）。

**第 2 处修正**：必须在**两种后端的所有路径**上执行，包括非 tmux 的 OSC 9。Codex 只在 tmux 分支里翻倍 ESC（`notifications/osc9.rs:48-50`），非 tmux 路径直接 `write!(f, "\x1b]9;{}\x07", self.message)`（`notifications/osc9.rs:52`）。而消息来自模型输出与命令文本，正文里一个 `\x1b` 就能截断序列、注入终端指令。

顺序是 `summarize` → `sanitize`，不可颠倒。先截断再消毒，避免截断点落在被剥掉的字符上导致长度抖动。

## 8. 与 Codex 的六处差异

| # | 差异 | 理由 |
|---|---|---|
| 1 | Windows 上尝试 `EnableFocusChange`，失败则 `unfocused` 退化为 `always` | Codex 在 Windows 上 `focused` 恒真，`unfocused` 永不成立，功能实际失效 |
| 2 | OSC 9 非 tmux 路径也执行控制字符消毒 | Codex 该路径直接拼接消息，消息含模型输出与命令文本，是真实注入面 |
| 3 | zellij 复用 tmux 的 DCS 包裹 | Codex 只认 tmux，zellij 里 OSC 9 静默丢失 |
| 4 | 不做 OSC 777 | 目标终端只有 urxvt 与 WezTerm 支持，而 WezTerm 也支持 OSC 9；收益仅剩 urxvt 一家 |
| 5 | 不做按类型白名单（`Notifications` 只到 `bool`） | Codex 有 `Notifications::Custom(Vec<String>)` 按 `type_name()` 过滤（`chatwidget/notifications.rs:94-100`）；我们只有两类事件，白名单不产生价值，将来加数组形态向后兼容 |
| 6 | 不做配置热更新 | 我们没有设置面板，`/config` 是只读快照，没有触发源 |

其余（配置键名与取值、两张协议字节、五家白名单、优先级与去抖、绘制间隙 flush、失败自禁）与 Codex 一致。

## 9. 测试

全部确定性，不依赖真实终端，不要求人工验证。

### 9.1 单元测试

`notify/text.rs`：

- 输入含 `\x1b]0;pwned\x07`、`\n`、`\t`、C1 区字符，`summarize` → `sanitize` 后输出不含任何 `< 0x20` 或 `0x7f`–`0x9f` 字节
- 代码围栏跳过、行首标记剥离、空白折叠
- 空输入 → 固定文案
- 200 字素边界：中文与 emoji 不被切半；截断后可见字素恰为 200（含 `...`）

`notify/osc9.rs`（用 `Command::write_ansi` 断言字节，与 Codex 同法）：

- 普通：`\x1b]9;hello\x07`
- tmux：`\x1bPtmux;\x1b\x1b]9;hello\x07\x1b\\`
- 消息含 `\x1b` 时输出不得出现裸 ESC（Codex 无此用例）

`notify/mod.rs` + 终端识别（注入 `TerminalEnv`，不读写真实环境）：

- `auto` 对五家白名单逐个选 OSC 9
- `auto` 对 Apple Terminal / Alacritty / VS Code / Windows Terminal / Unknown 选 BEL
- 显式 `osc9` / `bel` 覆盖自动判定
- `ZELLIJ` 存在时走 tmux 包裹
- `TERM_PROGRAM=WarpTerminal` 与 `warp` 归一化后等价（`lib.rs:415` 的规则）
- `TERM_PROGRAM=tmux` 时第 1 条识别被跳过，改由 `KITTY_WINDOW_ID` / `TERM` 兜底
- `GHOSTTY_RESOURCES_DIR` / `WEZTERM_VERSION` / `ITERM_SESSION_ID` / `KITTY_WINDOW_ID` 四条兜底各一例（`TERM_PROGRAM` 缺席时命中）

`tui/history.rs`：

- `last_assistant_text` 只返回最后一条 assistant cell
- 末条是 `Separator` 时仍能反向找到之前的 assistant cell（`push_event` 会压分隔线）
- 空转录返回 `None`
- 末条是 `Reasoning` 时不返回思维链

### 9.2 集成测试（`tui/app.rs` 测试模块）

沿用现有假 `EventSource` 模式（`app.rs:4999` 有先例）：

- `Done{EndTurn}` 后 `pending_notification` 被置位（摘要取自 `last_assistant_text`），下一次绘制后清空
- 该回合无 assistant 文本时，摘要退回固定句 `yi-agent 回合完成`
- `PermissionRequest` 优先级压过同帧的 `TurnComplete`
- `Cancelled` / `Error` / `Done{MaxTurns}` / `Done{Interrupted}` 不置位
- `notifications=false` 时任何事件都不置位
- `focused=true` + `unfocused` 不发；`FocusLost` 后发
- 非 TTY 时 `notify()` 短路

### 9.3 回归护栏

- `yi-agent run` 的 headless 路径零改动，`run --json` 的 stdout 逐字节不变，由现有 `drain_stream_*` 测试保证。
- `settings.rs` 的 `FIELDS` 表新增三个键后，需通过既有两条表结构断言：`field_keys_are_unique`（`settings.rs:771`）与 `every_select_field_has_options_and_every_other_field_has_none`（`settings.rs:781`）——`notification_method` / `notification_condition` 是 `Select`，必须带 `options`。

### 9.4 验收命令

```
cargo test -p yi-agent --bin yi-agent notify
cargo test -p yi-agent --bin yi-agent tui::app::tests::notification
cargo test -p yi-agent-runtime --lib settings
```

## 10. 风险

| 风险 | 缓解 |
|---|---|
| 默认开启后 CI / 无人值守场景刷屏 | 仅 TUI 实现，`run` 路径零改动；`notify()` 判 TTY；`unfocused` 默认已过滤前台噪音 |
| 终端收到不认识的 OSC 9 后打印乱码 | `auto` 白名单只含实测支持的终端；其余走 BEL（被不认识时通常静默） |
| 正文注入终端指令 | `sanitize` 剥离全部 C0/C1/DEL，两种后端所有路径统一执行 |
| 通知与绘制争用 stdout | 只在绘制完成后 flush |
| Windows 焦点事件不可用 | 退化为 `always`，宁可多响（§6.4） |
