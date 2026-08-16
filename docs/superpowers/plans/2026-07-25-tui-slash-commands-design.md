# TUI Slash 命令功能设计

日期: 2026-07-25

## 背景与目标

当前 TUI 输入框把所有用户输入原样发给 agent driver,没有任何 slash 命令处理。
`crates/yi-agent/src/input.rs` 中的 `UserCommand` 枚举只用于 InlineRenderer 路径,TUI 路径未使用。

目标: 在 TUI 输入框中,当用户输入 `/` 时,自动弹出匹配的 slash 命令列表,
帮助用户发现和填写命令。参考 Codex 的 `CommandPopup` 实现。

## 命令集

复用现有 `UserCommand` 枚举定义的命令,不新增:

| 命令     | 描述                   | 需要参数 |
|----------|------------------------|----------|
| /quit    | 退出程序               | 否       |
| /clear   | 清空对话上下文         | 否       |
| /model   | 切换模型               | 是       |
| /cost    | 显示 token 使用量      | 否       |
| /compact | 压缩对话历史           | 否       |
| /config  | 显示当前配置           | 否       |
| /help    | 显示帮助信息           | 否       |

## 架构

### 新文件: `tui/slash.rs`

独立模块,包含 `SlashCommand` 枚举和 `CommandPopup` 状态结构。

```rust
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
    pub fn name(&self) -> &'static str { ... }       // "quit", "clear", ...
    pub fn description(&self) -> &'static str { ... } // 中文描述
    pub fn needs_arg(&self) -> bool { ... }           // 只有 Model 返回 true
    pub fn all() -> &'static [SlashCommand] { ... }   // 所有命令
}

pub struct CommandPopup {
    filtered: Vec<SlashCommand>,  // 过滤后的命令列表
    selected: usize,               // 当前选中索引
}

impl CommandPopup {
    pub fn new() -> Self { ... }
    pub fn filter(&mut self, text: &str) { ... }  // 根据输入过滤,重置 selected
    pub fn filtered(&self) -> &[SlashCommand] { ... }
    pub fn move_up(&mut self) { ... }
    pub fn move_down(&mut self) { ... }
    pub fn selected(&self) -> Option<&SlashCommand> { ... }
}
```

### 修改文件: `tui/app.rs`

在 `run_loop` 中新增 `popup: Option<CommandPopup>` 状态:
- `None` — 弹窗未激活
- `Some` — 弹窗激活

新增 `handle_slash_command` 函数处理命令执行。

### 修改文件: `tui/mod.rs`

添加 `pub mod slash;` 声明。

## 交互流程

### 弹窗激活条件

每次按键后检查:
- 缓冲区以 `/` 开头
- 光标在命令名范围内(第一个空格之前,或缓冲区末尾)

满足条件时,若弹窗不存在则创建;已存在则更新过滤文本。

### 弹窗失活条件

- 用户按 `Esc`(保留输入内容)
- 用户输入空格(进入参数输入模式)
- 用户选中命令并执行(Tab 补全或 Enter 执行)
- 缓冲区不再以 `/` 开头(退格删掉了 `/`)

### 按键处理优先级(弹窗激活时)

1. `Esc` → 关闭弹窗,保留输入内容
2. `Up` → 弹窗选中项上移(不触发历史导航)
3. `Down` → 弹窗选中项下移(不触发历史导航)
4. `Tab` → 补全选中命令名到输入框(如 `/cl` + Tab → `/clear `),光标移到末尾,弹窗关闭
5. `Enter` →
   - 如果有选中命令 → 执行该命令
   - 如果过滤后无匹配 → 显示"未知命令"错误
6. 其他字符 → 正常插入到缓冲区,然后重新过滤弹窗

### Tab 与 Enter 的区别

- **Tab**: 只补全命令名到输入框,不执行。适用于需要继续输入参数的命令(如 `/model gpt-4`)。补全后弹窗关闭,光标在命令名后加一个空格。
- **Enter**: 直接执行命令。对于无参数命令立即执行;对于需要参数的命令,如果没参数则提示错误。

## 命令执行

### 无参数命令(立即执行)

- `/quit` → 返回 `KeyOutcome::Quit`,主循环退出
- `/clear` → 清空 `history.cells`,清空 `history.scroll_offset`,显示一条 `Separator { label: Some("对话已清空") }`
- `/cost` → 在历史中追加 `Separator { label: Some("Token 用量: ...") }`(初版占位,实际数据从 agent 获取)
- `/compact` → 向 agent 发送压缩指令(初版占位)
- `/config` → 在历史中追加显示当前配置(初版占位)
- `/help` → 在历史中追加帮助文本,列出所有可用命令

### 需要参数命令

- `/model` → 如果没有参数,在历史中追加 `Separator { label: Some("用法: /model <model-name>") }`;如果有参数,切换模型(初版占位)

### 未知命令

用户输入 `/foo` 这类不存在的命令并按 Enter:
- 在历史中追加 `Separator { label: Some("未知命令: /foo") }`
- 不发送给 agent
- 清空输入框

## 渲染与布局

### 布局调整

当前布局(弹窗未激活时不变):
```
[history(Min 3), gap(1), input(Length 1-6)]
```

弹窗激活时:
```
[history(Min 3), popup(Max 8), gap(1), input(Length 1-6)]
```

- 弹窗高度根据过滤后的命令数量动态计算,最多 8 行
- 弹窗未激活时,popup 约束为 `Length(0)`,不影响现有布局

### 弹窗渲染

每行一个命令,格式:
```
  /quit    退出程序
  /clear   清空对话上下文
  /model   切换模型 (需要参数)
  /cost    显示 token 使用量
  /compact 压缩对话历史
  /config  显示当前配置
  /help    显示帮助信息
```

- 选中行用反色高亮(背景蓝色,前景白色)
- 命令名用 `Color::Cyan`
- 描述用默认色
- 弹窗使用 `Block::bordered()`,标题为 `"命令"`

## 与现有 `UserCommand` 的关系

TUI 的 `SlashCommand` 独立实现,不复用 `crate::input::UserCommand`:
1. TUI 需要中文描述字段,`UserCommand` 没有
2. TUI 需要知道哪些命令需要参数,`UserCommand` 没有这个信息
3. 避免修改 `input.rs` 影响 InlineRenderer 路径

## 测试策略

按 TDD 原则,先写测试再实现。

### `slash.rs` 单元测试

- `filter_empty_shows_all` — 空过滤显示所有命令
- `filter_prefix_matches` — `/cl` 过滤到 `/clear`
- `filter_no_match` — `/xyz` 无匹配
- `move_up_down_wraps` — 上下导航循环
- `selected_returns_correct_command` — 选中项正确

### `app.rs` 集成测试

- `slash_popup_appears_on_slash` — 输入 `/` 弹窗出现
- `slash_popup_filters_on_typing` — 输入 `/cl` 过滤到 `/clear`
- `slash_popup_tab_completes` — Tab 补全命令名
- `slash_popup_enter_executes_quit` — Enter 执行 `/quit` 退出
- `slash_popup_esc_dismisses` — Esc 关闭弹窗
- `slash_popup_up_down_navigates` — Up/Down 导航弹窗
- `unknown_slash_command_shows_error` — 未知命令提示错误
- `slash_popup_dismisses_on_space` — 输入空格关闭弹窗
