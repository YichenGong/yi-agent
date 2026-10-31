# 终端通知（BEL / OSC 9）设计

日期：2026-11-01
状态：设计已确认，待实现

## 1. 问题

TUI 里跑一个长回合时，用户会切到别的窗口。回合结束时没有任何信号，只能不断切回来确认。当 agent 卡在权限确认弹窗等待时，这个成本更高——它真的停在那里等，而用户不知道。

终端本身提供了三条件内通知机制，实现成本极低：

- **BEL**（`0x07`）：最古老，几乎全部终端支持。
- **OSC 9**（`ESC ] 9 ; <text> ST`）：iTerm2 起源（2070 年前），Ghostty、kitty、WezTerm、Warp 跟进。
- **OSC 777**（`ESC ] 777 ; notify ; <title> ; <body> ST`）：urxvt 扩展，WezTerm 支持。

当前 yi-agent 的 CLI 与 TUI **完全没有这部分实现**。全仓核查结果：

| 搜索项 | 结果 |
|---|---|
| `0x07` / `\x07` / `\u{7}` 在 `tui/theme.rs` 之外 | 0 处 |
| 写出 OSC 的位置 | 仅 `tui/theme.rs:322`，查询终端背景色 `\x1b]11;?\x1b\\` |
| CLI 参数中的通知开关 | 无（`config.rs:15-92`、`config.rs:96-205`） |

`tui/theme.rs:377` 确实出现 `0x07`，但那是**读**方向：解析 OSC 11 回复时把 BEL 当终止符，与发送通知无关。

## 2. 参照实现

本设计以本地 Codex 源码（`OpenSource/codex`，HEAD `67f0994b53`）为基准，逐项对齐其架构与行为，并修正其中三处已知缺陷。

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
| `tui/src/event_stream.rs:306-313` | `FocusGained/Lost` → `AtomicBool` |
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
- **失败 / 中断通知**。`Done { reason: MaxTurns }`、`AgentEvent::Error`、`AgentEvent::Cancelled` 一律不发。失败时 TUI 已有醒目的红色错误行；`Ctrl+C` 说明用户人就在键盘前。这两种情况的通知价值远低于「你切走了但结果出来了」，且一旦默认开启会在 CI 与无人值守场景刷屏。
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

严格对齐 Codex `notifications/mod.rs:57-66` 的五家白名单：

| 终端 | 判据 | 结果 |
|---|---|---|
| Ghostty | `TERM_PROGRAM=ghostty` | OSC 9 |
| iTerm2 | `TERM_PROGRAM=iTerm.app` | OSC 9 |
| kitty | `TERM=xterm-kitty` | OSC 9 |
| Warp | `TERM_PROGRAM=WarpTerminal` | OSC 9 |
| WezTerm | `TERM_PROGRAM=WezTerm` | OSC 9 |
| 其余（Apple Terminal、Alacritty、GNOME、Konsole、VS Code、Windows Terminal、Unknown） | — | BEL |

**不扩表。** 这张表是「实测能弹出系统通知」的集合，不是「协议支持」的集合。扩表必须先实测，本次没有实测预算。显式指定 `method = osc9` 可以让任何终端走 OSC 9。

## 6. 触发与投递

三层，对齐 Codex。

### 6.1 事件 → 候选通知

在 `tui/app.rs` 事件循环中从 `AgentEvent` 映射：

| AgentEvent | 类型 | 优先级 | 文案 |
|---|---|---|---|
| `Done { reason: EndTurn }` | `TurnComplete` | 0 | 正文首行摘要（截 60 字素） |
| `PermissionRequest { tool_name, tool_input, .. }` | `ApprovalRequested` | 1 | `等待批准: <命令或工具名，截 30 字素>` |
| `Done { reason: MaxTurns }` / `Error` / `Cancelled` | — | — | 不发 |

`PermissionRequest` 的文案取值：按顺序取第一个非空值——`tool_input.command`（bash 类工具）→ `tool_name` → 固定文案 `等待批准`。截 30 字素，同样经过 §7 的 `summarize` + `sanitize`。

### 6.2 去抖

`pending_notification: Option<PendingNotification>`。新候选优先级 ≤ 已有则丢弃，> 则替换（对齐 Codex `notifications.rs:11-16`）。同一帧内多个 `PermissionRequest` 只留第一个。

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

`/config` 的只读快照追加三行：开关状态、`method` 配置值、**解析后的实际后端**。

最后一项是 Codex 没有的：只显示 `auto` 时，用户排查「为什么我的 kitty 不弹」没有任何线索。

## 7. 正文摘要与消毒

`notify/text.rs`，纯函数。

### 7.1 summarize

顺序固定：

1. 丢弃代码围栏块（``` 之间的内容整体跳过）
2. 取首个非空行
3. 去掉行首 markdown 标记：`#`、`>`、`-`、`*`、`1.`
4. 去掉行内标记：`**`、`*`、`` ` ``、`[text](url)` → `text`
5. 空白折叠为单空格
6. 按**字素**截 60（不是字节、不是 char——中文与 emoji 不会被切半）
7. 空结果 → 回退固定文案 `yi-agent 回合完成`

### 7.2 sanitize

剥掉全部 C0（`U+0000`–`U+001F`）、`U+007F`（DEL）、C1（`U+0080`–`U+009F`）。

**第 2 处修正**：必须在**两种后端的所有路径**上执行，包括非 tmux 的 OSC 9。Codex 只在 tmux 分支里翻倍 ESC（`osc9.rs:44-46`），非 tmux 路径直接 `format!("\x1b]9;{}\x07", message)`（`osc9.rs:41`）。而消息来自模型输出与命令文本，正文里一个 `\x1b` 就能截断序列、注入终端指令。

顺序是 `summarize` → `sanitize`，不可颠倒。先截断再消毒，避免截断点落在被剥掉的字符上导致长度抖动。

## 8. 与 Codex 的四处差异

| # | 差异 | 理由 |
|---|---|---|
| 1 | Windows 上尝试 `EnableFocusChange`，失败则 `unfocused` 退化为 `always` | Codex 在 Windows 上 `focused` 恒真，`unfocused` 永不成立，功能实际失效 |
| 2 | OSC 9 非 tmux 路径也执行控制字符消毒 | Codex 该路径直接拼接消息，含是真实注入面 |
| 3 | zellij 复用 tmux 的 DCS 包裹 | Codex 只认 tmux，zellij 里 OSC 9 静默丢失 |
| 4 | 不做 OSC 777；不做按类型白名单；不做热更新 | 分别见 §3 的 YAGNI 判断 |

其余（配置键名与取值、两张协议字节、五家白名单、优先级与去抖、绘制间隙 flush、失败自禁）与 Codex 一致。

## 9. 测试

全部确定性，不依赖真实终端，不要求人工验证。

### 9.1 单元测试

`notify/text.rs`：

- 输入含 `\x1b]0;pwned\x07`、`\n`、`\t`、C1 区字符，`summarize` → `sanitize` 后输出不含任何 `< 0x20` 或 `0x7f`–`0x9f` 字节
- 代码围栏跳过、行首标记剥离、空白折叠
- 空输入 → 固定文案
- 60 字素边界：中文与 emoji 不被切半

`notify/osc9.rs`（用 `Command::write_ansi` 断言字节，与 Codex 同法）：

- 普通：`\x1b]9;hello\x07`
- tmux：`\x1bPtmux;\x1b\x1b]9;hello\x07\x1b\\`
- 消息含 `\x1b` 时输出不得出现裸 ESC（Codex 无此用例）

`notify/mod.rs`：

- `auto` 对五家白名单逐个选 OSC 9
- `auto` 对 Apple Terminal / Alacritty / VS Code / Windows Terminal / Unknown 选 BEL
- 显式 `osc9` / `bel` 覆盖自动判定
- `ZELLIJ` 存在时走 tmux 包裹

### 9.2 集成测试（`tui/app.rs` 测试模块）

沿用现有假 `EventSource` 模式（`app.rs:4999` 有先例）：

- `Done{EndTurn}` 后 `pending_notification` 被置位，下一次绘制后清空
- `PermissionRequest` 优先级压过同帧的 `TurnComplete`
- `Cancelled` / `Error` / `Done{MaxTurns}` 不置位
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
