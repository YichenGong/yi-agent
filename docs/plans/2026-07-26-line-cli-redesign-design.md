# 行式 CLI 重构设计:抛弃全屏 TUI,改用终端原生 scrollback

日期:2026-07-26

## 背景与目标

当前 TUI 使用 ratatui 全屏模式(EnterAlternateScreen + EnableMouseCapture)。这导致:

1. 终端原生鼠标选择/复制被禁用 —— `EnableMouseCapture` 把鼠标事件交给应用,
   应用只处理滚轮(`app.rs:480` `_ => None`),点击拖拽被吞掉
2. 终端原生 scrollback 不可用 —— 备用屏幕没有 scrollback 历史
3. Cmd+C 在终端层无法复制(没有终端层选中)

用户需求:能像普通 CLI 工具(`git log`、`cat`)一样用鼠标选中 + Cmd+C 复制,
能用滚轮翻阅长历史。

目标:**抛弃全屏 TUI,改用终端原生 scrollback**,把 ratatui 渲染循环换成
"逐行流式打印 + 单行输入编辑"的行式 CLI 模型。

## 架构总览

### 删除的东西

- `EnterAlternateScreen` / `LeaveAlternateScreen`
- `EnableMouseCapture` / `DisableMouseCapture`
- ratatui `Terminal::draw()` 整个渲染循环
- `HistoryState` 的 `scroll_offset` / `selected` 字段(但 `cells` 概念转为
  "输出已 emit 的事件流")
- `bash_popup.rs` 覆盖式弹窗
- `history.rs` 的 `HistoryView` 渲染
- `handle_mouse` 函数(不再处理任何 `Event::Mouse`)

### 保留的东西

- `enable_raw_mode()` —— 仅在读取输入行时开启,打印输出时切回 cooked
- `yi-agent run` 子命令保持不变(已有 headless 模式)
- AgentEvent 流、Agent 驱动循环、ControlCommand 通道、permission 通道都不变
- `InputLine` 单行编辑(左右移动、Ctrl+A/E、历史上下翻)
- `CostTracker` 累计逻辑
- `RunningTaskRegistry` 追踪 bash 任务
- `slash.rs` 的 `CommandPopup` 状态机
- `cell.rs` / `markdown.rs` —— 转为"事件 → ANSI 终端文本"渲染器

### 新增的东西

- 行式 `run_loop`(替换 ratatui 渲染循环)
- ANSI 渲染器(替代 ratatui `Line`/`Span`/`Widget`)
- 内联 slash 菜单(ANSI 绘制,非覆盖层)
- `/bash` 命令(替代 Ctrl+P 弹窗)
- prompt 前缀状态行(替代固定底栏)

## 输入/输出交错与 raw mode 切换

### 核心时序(单次循环)

```
loop {
    1. disable_raw_mode()                 // 切回 cooked,输出可被终端原生选择/滚轮
    2. drain agent_rx → 渲染到 stdout     // 流式打印期间终端是普通模式
    3. flush stdout
    4. 重绘 prompt 区域(状态行 + 输入行)
    5. enable_raw_mode()                  // 进 raw 模式读单行输入
    6. poll(33ms) → 按键 / 超时           // 30hz tick 用于状态平滑动画
    7. 处理按键 → InputLine 编辑 / 提交
    8. 如有 LLM 事件到达:擦除 prompt → 打印 → 重绘
}
```

### cooked mode vs raw mode

- **cooked mode**(默认):终端自己做行编辑,Ctrl+C 发 SIGINT,输出到终端
  的内容按普通文本处理,终端不接管鼠标,滚轮走原生 scrollback,鼠标拖拽可以
  做选择。LLM 输出期间用 cooked mode,让用户能选中复制。
- **raw mode**:终端把每个按键即时发给应用,不做任何处理。用户输入编辑
  期间用 raw mode,让 InputLine 自己处理光标移动、历史上下翻。

### LLM 事件到达时的擦除顺序

```
1. ANSI 擦除 prompt 区域(上移 2 行 + 清行)
2. disable_raw_mode
3. drain + 打印 LLM 输出
4. enable_raw_mode
5. 重绘 prompt 区域(状态 + 输入)
```

raw mode 下 ANSI 转义(`\x1b[2A`、`\x1b[J`、`\x1b[7m`)完全工作。

### Ctrl+C 处理

- raw 模式下自己捕获 `KeyCode::Char('c') + CONTROL` → 发 interrupt
- cooked 模式下 Ctrl+C 由终端默认 ISIG 信号处理,会 SIGINT 进程
- 输出阶段极短,用户通常不会按;先不特殊处理 cooked 期间的 Ctrl+C

## AgentEvent → 终端文本渲染

不再走 ratatui `Line`/`Span`/`Widget`,直接用 ANSI 转义序列打印到 stdout。
每个 `AgentEvent` 映射到一段文本输出。

### 事件映射表

| AgentEvent | 输出行为 |
|------------|---------|
| `Start` | 不输出 |
| `AssistantText(text)` | `print!("{text}")`,流式逐 delta 打印(不带换行,LLM 自带 `\n` 保留) |
| `ToolCall { id, name, input }` | `\n\x1b[36m⚡ {name}\x1b[0m {input_one_line}\n`(青色工具名 + 单行 JSON) |
| `ToolResult { id, result_text, is_error }` | 错误用红色 `✗`,成功用绿色 `✓`,内容按行打印;长输出保留完整(终端 scrollback 接住) |
| `Done { EndTurn }` | `\n` 分隔空行 + 状态行(见"Cost/Token 显示") |
| `Done { MaxTurns }` | `\x1b[33m! Max turns\x1b[0m\n` |
| `Cancelled` | `\x1b[33m! Interrupted\x1b[0m\n` |
| `Error(err)` | `\x1b[31m! Error: {err}\x1b[0m\n` |
| `PermissionRequest { .. }` | 打印请求摘要 + `[y/n/a]` 提示,阻塞读一行(见"权限请求交互") |
| `PermissionResolved { decision }` | `\x1b[32m✓ allowed\x1b[0m` / `\x1b[31m✗ denied\x1b[0m` |
| `Usage` / `EstimatedPrefill` / `DecodeDelta` | 不直接打印,更新内部统计 |
| `ToolOutputDelta` | 实时直透打印(见"Bash 工具输出处理") |
| `ToolExit` | 打印 `[bash {id}] exit={code} elapsed={n}s` |
| `ToolTimeout` | 打印 `[bash {id}] timeout` |
| `AutoCompacting` | `\x1b[33m# compacting...\x1b[0m\n` |

### markdown 渲染

`markdown.rs` 当前把 markdown 转成 ratatui `Line`。改为转成 ANSI 字符串
(粗体 `\x1b[1m`、代码块缩进 + 灰色背景、标题加色等)。

**第一版简化**:只处理 `**bold**`、`` `code` ``、`# heading`、代码块缩进,
不做语法高亮。后续可用 `bat` 或 `syntect` 加语法高亮。

## Bash 工具输出处理

### 实时直透打印(4A)

- `ToolOutputDelta` 到达即 `print!` 到 stdout,前缀加淡灰 `[bash t1]` 标记
- `ToolExit` 打印 `[bash t1] exit={code} elapsed={n}s`
- `RunningTaskRegistry` 仍保留 64KB 缓存,用于 `/bash` 命令回看
- 用户能看到进度,像在终端里直接跑命令一样
- 长输出"挤占"对话上下文显示,但终端 scrollback 接住——往上翻就能看到
  之前的 LLM 回复

### `/bash` 命令(替代当前 Ctrl+P 弹窗)

- `/bash` —— 列出所有任务(一行一个,带状态符号)
- `/bash <id>` —— 打印该任务完整 stdout/stderr(从 64KB 缓存读)
- `/bash <id> kill` —— 发 kill 信号(替代当前 `k` 键)

### Ctrl+C 语义

- raw 期间(等输入)Ctrl+C → 中断 Agent
- bash 长任务跑期间,Ctrl+C 中断 Agent,Agent 被中断时 cascade kill bash
- 语义保持不变

## 权限请求交互

当 `AgentEvent::PermissionRequest` 到达时:

```
\x1b[33m⚡ bash: ls -la\x1b[0m
  allow? [y]es / [n]o / [a]lways (default: n): _
```

### 交互流程

1. 在 cooked mode 下打印请求摘要(黄色)
2. 打印提示行
3. 切 raw mode,用 InputLine 读一行(`y` / `n` / `a` / 回车默认 `n`)
4. 解析后通过 `decision_tx` 发送 `Decision::AllowOnce` / `DenyOnce` / `AllowAlways`
5. 打印确认 `\x1b[32m✓ allowed\x1b[0m` 或 `\x1b[31m✗ denied\x1b[0m`

### 关键约束

- 权限请求是**同步阻塞**的——Agent 在等 decision 才能继续
- 触发点从"渲染循环每 tick 检查"变成"收到 PermissionRequest 事件立刻交互"
- 先不做 Tab 补全,`prefix_suggestion` 在请求摘要里显示成灰色提示:
  `(prefix: "git")`,用户自己看着输入

## Slash 命令内联菜单

参考 Claude Code / Codex 的设计:输入 `/` 时在输入行上方显示过滤菜单,
上下键选择,Enter 执行,Esc 取消。菜单不是 ratatui 覆盖层,而是用 ANSI
转义打印的临时文本,选择后清除。

### 交互模式

```
> /cl        ← 用户输入到此处
  /clear     Clear conversation history
  /compact   Compact context
  /cost      Show token cost summary
❯ /config    Show current configuration    ← 高亮选中(反色)
```

### 实现要点

- 用户键入 `/` 开头的输入时,在输入行上方用 ANSI 打印过滤后的命令列表
- 每次按键后重新过滤 + 重绘菜单(先 ANSI 光标上移 + 清行,再打印新菜单)
- `↑`/`↓` 在 `slash.rs` 的 `CommandPopup` 里移动 `selected`
- `Enter`:执行选中的命令,清除菜单
- `Esc` 或 `Ctrl+C`:清除菜单,清空输入
- 继续输入非 `/` 字符则菜单消失,当普通 prompt 处理

### raw mode 兼容性

- raw mode 下 ANSI 转义(`\x1b[<n>A` 上移、`\x1b[2K` 清行、`\x1b[7m` 反色)完全工作
- `event::poll(timeout)` 用 33ms 超时实现 30hz tick
- 用户按键即时响应(poll 短超时 + 立即处理输入)
- LLM 事件到达时:先擦除菜单行 → disable_raw → 打印事件 → enable_raw → 重绘菜单

### 保留的 slash 命令

| 命令 | 行为 |
|------|------|
| `/quit` | 退出 |
| `/clear` | 发 `ControlCommand::Clear`,打印 `# cleared` |
| `/compact` | 发 `ControlCommand::Compact`,打印 `# compacting...` |
| `/model <name>` | 切模型;无参数时打印当前 model |
| `/cost` | 打印完整 cost 摘要 |
| `/config` | 打印当前配置 |
| `/help` | 打印可用命令列表 |
| `/bash` | 列出任务 |
| `/bash <id>` | 打印任务详情 |
| `/bash <id> kill` | kill 任务 |

### 移除的快捷键

- `Ctrl+P`(bash popup)→ `/bash` 命令
- `Shift+Up/Down`(选择 cell)→ 不需要,终端原生选择
- `Ctrl+O`(折叠 cell)→ 不需要,内容已全部打印
- `Ctrl+U/D`(翻 10 行)→ 不需要,终端原生滚轮
- `Ctrl+Q`(两次退出确认)→ Ctrl+C 直接退出

## Cost / Token 显示

### 常驻状态行(体验对齐全屏 TUI)

为了体验与全屏 TUI 一致,保留**常驻状态显示 + 平滑插值动画**,载体从
ratatui 底栏改为"prompt 前缀状态行 + 30hz 重绘当前行"。

```
[● bash 3.2s] prefill 1,234  decode 567  claude-opus-4
> hello world_
```

- 第一行是**状态行**,第二行是**输入行**,两行一起组成"prompt 区域"
- 30hz tick 时:擦除这两行(`\x1b[2A\x1b[J`)→ 重新打印最新状态 + 当前 InputLine
- LLM 输出期间:**暂停** tick(输出是主流,不抢屏);输出结束后 `Done` 事件
  触发一次状态更新
- bash 任务运行时:状态行前缀 `[● bash 3.2s]` 平滑刷新
- 平滑插值逻辑(`StatusBarState::tick`)保留,调用时机从"渲染循环"改为
  "等输入期间的 30hz poll 循环"

### 移除的东西

- `StatusBarState` 的 30hz `tick()` + 插值动画 → 改为每次 `Usage` 事件直接
  更新,回合结束时打印;**但保留插值,用 30hz poll tick 驱动**
- `spinner_phase` 旋转动画 → 保留,用 `[● tool Ns]` 文本指示
- `last_usage_time` + 1s snap 到 target 逻辑 → 保留

### 保留的东西

- `CostTracker` 累计逻辑不变
- `RunningTaskRegistry` 不变
- `set_token_target` / `set_prefill_estimate` / `estimate_decode_tokens` 保留

### `/cost` 命令输出

```
Total: 12,345 input / 3,456 output tokens
Estimated cost: $0.42
By tool:
  bash       8 calls / 1,200 output
  read       3 calls / 150 output
```

## 模块处置

### 重写

- `app.rs` —— `run_loop` 改为行式循环,`run_tui` 初始化去掉备用屏幕和鼠标捕获

### 删除或大幅瘦身

- `bash_popup.rs` —— 删除(改用 `/bash` 命令)
- `history.rs` —— 删除 `HistoryView` 渲染;`HistoryState` 只保留 `cells` 概念
  用于追踪(或完全删除,直接用 `Vec<AgentEvent>` 缓存)
- `statusbar.rs` —— 渲染改为 ANSI 字符串生成函数,保留 `StatusBarState`
- `queued.rs` —— 删除(行式 CLI 下排队输入直接打印到 stdout 等待)

### 保留

- `input.rs` —— InputLine 不变
- `slash.rs` —— `CommandPopup` 状态机保留,渲染改为 ANSI
- `cell.rs` / `markdown.rs` —— 转为 ANSI 渲染器
- `cost.rs` —— `CostTracker` 不变,渲染改为 ANSI
- `state.rs` —— `RunningTaskRegistry` 不变

## 迭代实施计划

分阶段实施,每阶段可独立验证后再进下一阶段。

### Phase 1:最小可运行行式 CLI(骨架)

- 重写 `run_tui` / `run_loop`:删除备用屏幕、鼠标捕获、ratatui draw 循环
- 实现 cooked/raw 切换时序
- AgentEvent → 纯文本 print(无色,无 markdown,无 bash 实时输出)
- 简单 `> ` prompt,InputLine 单行编辑
- Ctrl+C 中断,Enter 提交
- **验证**:能跑通一次对话,LLM 文本流式打印到 stdout,用户能选中复制

### Phase 2:ANSI 渲染 + 状态行

- AgentEvent → ANSI 彩色打印(ToolCall 青色、错误红色、Done 分隔)
- prompt 前缀状态行:`prefill N decode M model`
- 30hz tick 重绘状态行
- bash 任务指示器 `[● bash Ns]`
- **验证**:视觉接近全屏 TUI,状态平滑动画保留

### Phase 3:Bash 工具实时输出 + `/bash` 命令

- `ToolOutputDelta` 实时直透打印
- `/bash` / `/bash <id>` / `/bash <id> kill` 命令
- `RunningTaskRegistry` 64KB 缓存保留
- **验证**:bash 工具跑时能看到进度,`/bash` 能回看

### Phase 4:Slash 命令内联菜单

- ANSI 绘制 slash 菜单,上下键选择
- LLM 事件到达时擦除/重绘菜单
- 保留 `/quit` `/clear` `/compact` `/model` `/cost` `/config` `/help` `/bash`
- **验证**:输入 `/` 弹出菜单,上下键选择,Enter 执行

### Phase 5:权限请求交互

- `PermissionRequest` 事件触发阻塞交互
- cooked 打印请求 + raw 读 y/n/a
- `PermissionResolved` 打印确认
- **验证**:工具调用时弹出权限请求,y/n/a 能正确响应

### Phase 6:Markdown 渲染

- `markdown.rs` 转 ANSI 渲染器
- 简化版:`**bold**`、`` `code` ``、`# heading`、代码块缩进
- **验证**:LLM 回复有基本格式化显示

## 测试策略

- 每个 Phase 完成后,手动运行 `yi-agent` 验证对应功能
- 保留现有单元测试:`StatusBarState`、`CostTracker`、`RunningTaskRegistry`、
  `InputLine`、`CommandPopup` 的状态逻辑测试不变
- `app.rs` 的 `run_tui_with_backend` 测试需要重写:改用 fake stdin/stdout
  替代 ratatui `TestBackend`
- 端到端测试(`yi-agent run`)不受影响,因为走 headless 路径

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| cooked/raw 切换抖动 | print + flush 足够快,肉眼基本无感 |
| LLM 输出和输入交错时屏幕跳动 | 擦除→打印→重绘 顺序严格,ANSI 转义可靠 |
| 长时间 bash 输出淹没对话 | 终端 scrollback 接住,`/bash` 命令回看 |
| 权限请求阻塞 Agent | 保持现有 `decision_tx` 同步语义,只是触发点改变 |
| 终端兼容性(ANSI 转义) | 主流终端(iTerm2/kitty/wezterm/Terminal.app)都支持基本 ANSI |
