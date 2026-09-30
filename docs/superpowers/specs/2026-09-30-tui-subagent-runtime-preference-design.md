# TUI 子 Agent Runtime 启动偏好（记住选择 + /runtime 可逆）设计

**目标：** 消除"每次打开 `yi-agent` 都弹『启动本地 Agent Runtime?』"的重复打扰，同时
保住 spec 要求的"启动本地 daemon 必须是用户显式同意的动作"，并补上当前缺失的**逆转**
路径（按 `n` 之后本会话与后续会话都再也无法启用子 Agent）。

**状态：** 已设计，待实现。

**相关文档：** `docs/superpowers/specs/2026-08-10-subagent-tui-mvp-design.md`（确定
"显式同意"约束的原始 spec）。

---

## 1. 问题

### 1.1 现象

每次启动 `yi-agent`（TUI）都会无条件弹出确认框：

```
┌ 启动本地 Agent Runtime? ─────────────────────────┐
│ 启动后可以直接用自然语言创建和管理子 Agent。按 y 启动，按 n 跳过。 │
│                                                 │
│ [y] 启动并启用子 Agent   [n/Esc] 暂不启用         │
└─────────────────────────────────────────────────┘
```

### 1.2 根因（已核实）

1. 提示是无条件传入的：`crates/yi-agent/src/main.rs:1600-1603` 直接构造
   `Some(RuntimeStartPrompt { .. })` 交给 `run_tui`，与 runtime 是否已在运行、用户既往
   选择均无关。
2. 没有持久化：`RuntimeBootstrapState`（`crates/yi-agent/src/tui/subagents.rs:53-60`）
   起点固定 `Disconnected`，不存在"已确认/记住选择"的字段；`RuntimeConfig`
   （`yi-agent-runtime/src/config.rs`）也没有委派开关。
3. 状态机未被生产代码使用：`RuntimeBootstrapModel` / `TuiRuntimeMode` 仅出现在
   `tui/subagents.rs` 自身与其单测中（`grep` 零生产引用），所以"只在 disconnected 时
   提示"这层逻辑在 TUI 主路径上等于没有。
4. 代价不低：按 `y` 会走 `main.rs:1406` → `attach_tui_runtime`
   （`main.rs:819`），启动内嵌 daemon、创建 runtime 目录/DB/socket/lock、attach
   application root；首次输入还会 `activate_tui_runtime_root`。

### 1.3 为什么不能简单删掉提示

`2026-08-10-subagent-tui-mvp-design.md`:

> 如果本地 runtime 不可用，TUI 显示明确的启动确认。用户确认后，无需离开 TUI 即可启动
> runtime；用户拒绝后，当前对话仍可作为不带委派能力的普通单 Agent 对话使用。

> 在 TUI 内启动 runtime 始终是用户明确确认的动作。**不能仅因为 Agent 尝试委派，就静默
> 启动 daemon。**

因此本设计**保留显式同意**，只把"每次都要重新同意"降级为"同意一次，记住，且可随时改回"。

### 1.4 当前缺失的逆转路径

- `runtime_choice` 通道一次性（`main.rs:1286`），消费后不再产生。
- slash 命令表（`tui/slash.rs`）中没有 runtime / 子 Agent 开关（已 grep 确认）。
- `RuntimeBootstrapState` 无 `Disabled -> Prompting` 回退边。

结论：按 `n`（或 `Esc`）之后，**本会话与后续会话都没有任何办法启用子 Agent**。

---

## 2. 范围与决策（已与用户确认）

| 决策 | 结论 |
| --- | --- |
| 偏好存放位置 | 项目级 `<workdir>/.yi-agent/preferences.json`（非全局） |
| 三态 | `ask`（默认，弹窗）/ `always`（直接启动）/ `never`（跳过） |
| `y` | 本次启动 + 落盘 `always` |
| `n` | 本次不启动 + 落盘 `never` |
| `Esc` | 本次不启动 + **不落盘**（保持原偏好不变） |
| `/runtime` 能力 | 最小版：读写偏好文件，**重启后生效**；不做会话内热 attach |
| 偏好文件写入方 | TUI 按键处理处直接写（`run_loop` 已持有 `workdir`） |
| driver / daemon | **不改**：`RuntimeStartupChoice` 仍保持两态 |
| CLI / env 覆盖 | **不做**（YAGNI，见 §2.1） |

### 2.1 非目标（YAGNI）

- 不做会话内热切换（`/runtime always` 后需重启才 attach；`never` 后不做热 detach）。
- 不改 daemon、IPC 协议、`AttachedRoot` 生命周期。
- 不改 headless `run --subagents` 语义。
- 不新增 `ControlCommand` 变体（避免自动多出一个 CLI 子命令，见 §5.3）。
- `/runtime` 不加 `status` 之外的子动作（无 `reset` 等）。
- 不为该偏好新增 CLI flag 或环境变量；临时覆盖留待真实需求出现再加。

---

## 3. 设计

### 3.1 偏好文件

路径：`<workdir>/.yi-agent/preferences.json`（复用 `.gitignore:30` 已忽略的
`.yi-agent/` 根，不会弄脏 checkout）。

```json
{ "subagent_runtime": "ask" }
```

- 用**容器对象**而非裸字符串，便于后续加其它偏好而不破坏兼容。
- 序列化不复用 `McpConfig` 的 `json5` 依赖：`serde`（derive）与 `serde_json` 已是
  `yi-agent` 的常规依赖（`crates/yi-agent/Cargo.toml:27-28`），直接可用。
- 三个合法值：`ask` / `always` / `never`。
- **缺失 → `ask`**（保持现状语义，老用户行为不变）。
- **损坏 / 非法值 / 未知值 → `ask` + `tracing::warn!` 一次**，绝不因偏好文件问题阻断启动。

解析/写入层：新增 `crates/yi-agent/src/tui/runtime_prefs.rs`（单职责、可独立单测），并在
`tui/mod.rs` 注册 `pub mod runtime_prefs;`：

```rust
pub enum RuntimePreference { Ask, Always, Never }   // Default = Ask
pub fn preferences_path(workdir: &Path) -> PathBuf;      // workdir/.yi-agent/preferences.json
pub fn load(workdir: &Path) -> RuntimePreference;        // 缺失/损坏 -> Ask（warn）
pub fn save(workdir: &Path, pref: RuntimePreference) -> io::Result<()>;
```

`save` 必须**原子写**：`create_dir_all` → 写 `preferences.json.tmp` → `fs::rename`。
沿用仓库既有先例 `yi-agent-core/src/permission.rs:288-297`（`permissions.toml`）：

```rust
// 原子写入:先写临时文件,再 rename(同一文件系统内 rename 是原子的)
```

### 3.2 生效偏好的解析

- 唯一来源是偏好文件；缺失或非法一律 `ask`（§3.1）。
- `<workdir>` 取 `config.workdir`，与 runtime 目录共用同一解析基准（`main.rs:558`
  `runtime_directory_for`），保证"读偏好"与"起 runtime"看的是同一个项目。
- 不提供 CLI / env 覆盖（见 §2.1）。若要临时改偏好，用 `/runtime`（§3.6）。

### 3.3 启动意图（TUI 侧）

把 `run_tui` / `run_loop` 的 `runtime_start_prompt: Option<RuntimeStartPrompt>` 参数改为
`Option<RuntimeStartupIntent>`——**保留 `Option`**，`None` 仍表示该入口不涉及 runtime
交互（`run_tui_with_backend` 正是如此传参，`app.rs:191`），从而无需引入第四个"无"变体。
"弹窗 / 提示 / 静默"由这一处决定（`tui/subagents.rs`）：

```rust
pub enum RuntimeStartupIntent {
    Prompt(RuntimeStartPrompt),       // ask
    DisabledNotice { reason: String },// never
    AutoStart,                        // always
}
```

`run_loop` 据此分支：

| intent | 渲染 | 传给 driver |
| --- | --- | --- |
| `Prompt(p)` | 弹窗 + 按键处理（写偏好） | 等待用户 `y`/`n`/`Esc` |
| `DisabledNotice{reason}` | 启动时 push 一条 `HistoryCell::Separator` | `runtime_choice_tx = None` |
| `AutoStart` | 不渲染 | main.rs 预置 `Start`（见 §3.4） |

### 3.4 driver 零改动地实现 `always`

`always` 不需要新代码：main.rs 在 `run_tui_agent` 里**预先向通道投入 `Start`**，
driver 首次 `select!` 就会消费到（`biased;` 顺序中 `runtime_choice_rx` 优先于
`input_rx`，`main.rs:1329-1332`）：

```rust
if pref == RuntimePreference::Always {
    let _ = runtime_choice_tx.clone().try_send(RuntimeStartupChoice::Start);
}
```

于是 `always` 复用现有 `main.rs:1405` 分支的 attach + 工具表重建 +
`register_attached_root_tools` 全流程，`runtime_start_prompt: None` 不弹窗。

**`always` 启动失败**（dirty checkout、socket 超长等）：沿用既有降级路径
——`TuiRuntimeSession::Unavailable` / attach 被拒 → `AgentEvent::Error` 显示一行错误，
**不再弹窗二次确认**（用户既已选 always，除非其本人改回，否则不应被打断）。

### 3.5 弹窗文案与布局

内容行（替换 `app.rs:430` 那一行）：

```
启动后可以直接用自然语言创建和管理子 Agent。
此选择会被记住，可用 /runtime 修改。

[y] 启动并记住
[n] 跳过并记住    [Esc] 本次跳过（不记住）
```

要点：

- 三个键各自对应互不重叠的语义（本次行为 + 是否落盘），`Esc` 不再与 `n` 视觉合并。
- 现状 `box_h = 6` 是**固定高度**，且 ratatui 对超宽 `Line` 只从右侧静默裁掉（见
  `2026-09-27-tui-line-clipping-design.md`）。文案由 3 行增至 5 行，故 `box_h` 提升为
  `10`。
- **必须显式 `.wrap(ratatui::widgets::Wrap { trim: true })`**：ratatui 0.29 的
  `Paragraph` 文档明确写 "not wrapped"（`ratatui-0.29.0/src/widgets/paragraph.rs:29`），
  不加这只会在窄终端静默裁掉右半行。仓库现有渲染路径从不需要折行，**没有 `.wrap()`
  先例可抄**。
- 键位提示行按显示宽度计算为 58 列（CJK 计 2 列），**超过 `box_w - 2 = 56` 的内宽**，
  必定折行。因此把它拆成两行源文本（按键与说明分离），使折行发生在语义边界而非字符
  中间。`runtime_prompt_lines()` 的注释需写明"每行显示宽度 ≤ 56"这一不变式。
- 渲染测试取 40 与 80 两个宽度，断言各选项尾部文案（`启动并记住`/`跳过并记住`/
  `本次跳过`/`不记住`）均可见——既覆盖右侧裁剪，也覆盖折行。

### 3.6 `/runtime` 命令（最小版）

新增 `SlashCommand::Runtime`：

- `name()`：`"runtime"`
- `description()`：`"查看或设置子 Agent runtime 偏好"`
- `argument_usage()`：`Some("[ask|always|never]")`（与 `/mcp`、`/model` 同风格）
- 加入 `SlashCommand::all()`（自动进入 `/help` 与补全）

执行语义（对齐 `/mcp [on|off|...|status]` 的写法，`slash.rs:262-278`）：

| 输入 | 行为 |
| --- | --- |
| `/runtime` 或 `/runtime status` | 显示当前值与来源，如 `子 Agent runtime 偏好: always（来源: .yi-agent/preferences.json）; 重启后生效` |
| `/runtime ask\|always\|never` | 写偏好文件（原子写），回显 `已设为 never（重启后生效）` |
| 其它 | 回显用法错误 `用法: /runtime [ask|always|never]` |

输出统一用 `HistoryCell::Separator`（`/mcp`、`/review` 已有先例，见
`app.rs:1706-1713`），**不新增 `AgentEvent` 变体**（`AgentEvent` 无中性提示变体，新增会
牵动 driver 与全部消费方）。

解析器 `parse_runtime_args(args: &str) -> Result<RuntimePreference, String>` 放在
`slash.rs`，与 `parse_mcp_args` 并列，便于单测。

**`Esc` / `n` 之后的会话内行为不变**：`/runtime always` 只落盘，需重启才 attach。
`/runtime`（无参）始终显示真值，因此用户能确认设置已生效。

### 3.7 `never` 的可见性

`never` 时不弹窗、不启动，但**必须留一行提示**，否则用户会问"为什么没有
`spawn_agent` 工具"（这正是 `runtime_unavailable_reason` 当初存在的理由，`main.rs:789`）：

```
已禁用子 Agent 委派（.yi-agent/preferences.json: never）；用 /runtime 开启
```

在 `run_loop` 首次渲染时 push 一条 `HistoryCell::Separator` 即可（纯 TUI 本地，driver 无感）。

**语言一致性：** TUI 上面向用户的行一律用中文——既有先例见 `/mcp` 的
`MCP server '{name}' 已开启` 与 `/clear` 的 `对话已清空`（`app.rs:1699-1704`、
`app.rs:1568-1572`）。英文只用于 `runtime_unavailable_reason`（`main.rs:789`）这类**内部
诊断**字符串（它们经 `tracing` 或错误行输出）。§3.6 的 `/runtime` 回显同理用中文。

---

## 4. 涉及文件

| 文件 | 改动 |
| --- | --- |
| `crates/yi-agent/src/tui/runtime_prefs.rs` | **新增**：`RuntimePreference` + `load`/`save`（原子写）+ 单测 |
| `crates/yi-agent/src/tui/mod.rs` | 注册 `pub mod runtime_prefs;` |
| `crates/yi-agent/src/tui/subagents.rs` | 新增 `RuntimeStartupIntent`；**删除**零引用的死代码 `RuntimeBootstrapModel` / `RuntimeBootstrapState` / `TuiRuntimeMode`（见 §5.5） |
| `crates/yi-agent/src/main.rs` | 读偏好决定 intent；`always` 预置 `Start`；构造 `RuntimeStartupIntent` 传入 `run_tui` |
| `crates/yi-agent/src/tui/app.rs` | `run_tui` / `run_loop` 参数改为 `Option<RuntimeStartupIntent>`（含 `run_tui_with_backend` 的 `None` 传参，`app.rs:191`）；弹窗文案与 `box_h`；按键写偏好；`/runtime` 执行分支 |
| `crates/yi-agent/src/tui/slash.rs` | `SlashCommand::Runtime` + `parse_runtime_args` |
| `docs/bug-list.md` / `docs/project-management/subagent-runtime.md` | 实现后补条目（含验证命令） |

---

## 5. 关键约束（实现时必须遵守）

### 5.1 测试不得污染 `/tmp`

现有两条测试以 `std::env::temp_dir()` 作为 `run_loop` 的 `workdir` 实参
（`app.rs:4485`、`app.rs:4536`）。一旦 `y`/`n` 落盘，就会写入 `/tmp/.yi-agent/`（跨用例
共享、污染 HOME 之外的全局状态）。**必须改为 `tempfile::TempDir`**，并断言文件内容。

### 5.2 落盘失败不得阻断会话

`save` 失败时：push 一条 `HistoryCell::Separator` 警告，但**仍然**把选择发往 driver
（本次行为照常执行）。偏好是便利设施，不能成为启动路径的单点故障。

### 5.3 不要走 `ControlCommand`

`ControlCommand`（`control_commands.rs:7-29`）与 clap 的 `Command` 子命令是**镜像**的
（`main.rs:60-64` 逐个 `control_*`），且
`control_command_catalog_covers_the_documented_cli_and_slash_actions`
（`control_commands_tests.rs`）要求每个 slash 名都有对应 CLI 能力。偏好设置是纯 TUI 本地
动作，加入 `ControlCommand` 会凭空多出一个 CLI 子命令与一条需维护的目录测试，故
`/runtime` **只**落在 `SlashCommand` 与 `execute_slash_command`。

`execute_slash_command` 是**穷尽 match、无 catch-all**（已核实），新增变体会被编译器强制
处理——不要为它加 `_ =>` 兜底。

### 5.4 `always` 仍属"显式同意"

弹窗原文即 `[y] 启动并启用子 Agent`，用户按 `y` 就是显式同意；记住它只是免除重复同意，
不是"静默启动"。`always` **仅**由用户按键或 `/runtime` 显式设置产生（无 CLI/env 捷径，
§2.1），Agent 的任何委派尝试都不会触发它。

### 5.5 删除死代码（已核实为零引用）

`crates/yi-agent/src/tui/subagents.rs` 顶部有 `#![allow(dead_code)]`，掩盖了三项从未在
生产路径使用的类型。外部引用计数（`grep -rn` 排除 `subagents.rs` 自身）：

| 类型 | 外部引用 |
| --- | --- |
| `TuiRuntimeMode` | 0 |
| `RuntimeBootstrapModel` | 0 |
| `RuntimeBootstrapState` | 0 |
| （对照）`RuntimeStartupChoice` | 10 |
| （对照）`AttachedRoot` | 11 |
| （对照）`register_attached_root_tools` | 2 |

因此：**删除** `TuiRuntimeMode`、`RuntimeBootstrapModel`、`RuntimeBootstrapState` 及其
两个专属单测（`disconnected_state_only_prompts_before_user_confirms_runtime_start`、
`user_can_continue_without_starting_runtime`）。本设计中 `runtime_prefs` 与
`RuntimeStartupIntent` 取代了它们的职责。

保留 `RuntimeStartupChoice`（两态不变，§3.4）、`AttachedRoot`、
`register_attached_root_tools`、`set_current_attached_root`、`current_attached_root`
——这些仍在生产路径使用。

删除后应能移除 `subagents.rs` 顶部的 `#![allow(dead_code)]`；若编译器仍报未使用项，
逐个核对后一并清理，**不得**用新增 `allow` 掩盖。

---

## 6. 验证

单测（新增）：

- `runtime_prefs`：文件缺失 → `Ask`；`save`→`load` 往返；非法/损坏 JSON → `Ask` 且不
  panic；`save` 产生目录、原子写无残留 `.tmp`。
- `app.rs`：`y` 写入 `always`；`n` 写入 `never`；`Esc` **不写**（原有文件不变 / 仍不存在）；
  `never` 启动推送禁用提示分隔行；40 列下弹窗提示行完整可见。
- `slash.rs`：`/runtime` 注册、`from_name("runtime")`、名称唯一、`parse_runtime_args` 的
  合法/非法输入。
- `main.rs`：文件值 `ask`/`always`/`never` 分别映射到 `Prompt`/`AutoStart`/`DisabledNotice`；
  `AutoStart` 时通道已预置 `Start`；`DisabledNotice` 时不预置。

回归（必须全绿）：

```
cargo test -p yi-agent --bin yi-agent
```

重点回归对象：

```
cargo test -p yi-agent --bin yi-agent -- runtime_start_prompt --nocapture
cargo test -p yi-agent --bin yi-agent -- slash
cargo test -p yi-agent --bin yi-agent -- control_command
```

手工确认：

1. 删除 `.yi-agent/preferences.json` → 启动弹窗（现状不变）。
2. 按 `y` → 本次启动子 Agent，文件写入 `always`；重启不再弹窗且已 attach。
3. 按 `n` → 本次无委派 + 一行禁用提示，文件写入 `never`；重启不弹窗、无委派、有提示。
4. 按 `Esc` → 本次无委派，**文件不存在或内容不变**；重启仍弹窗。
5. `/runtime` → 显示当前值与来源；`/runtime always` → 提示重启后生效；重启后生效。
6. 手工把文件改成 `{"subagent_runtime":"bogus"}` → 启动不失败，按 `ask` 处理并 warn。
