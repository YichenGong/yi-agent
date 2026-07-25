# TUI 对话历史展示改进设计

日期: 2026-07-25

## 背景

当前 yi-agent 的 TUI 使用 reedline + InlineRenderer 架构,对话历史以线性文本流方式打印到 prompt 上方,无法回滚查看,缺少结构化布局和视觉层次。参考 codex 的 ratatui 全屏 TUI 实现,改进对话历史展示。

## 决策摘要

| 决策点 | 选择 |
|---|---|
| 架构 | 迁移到 ratatui 全屏布局 |
| 历史区交互 | 可滚动,Shift+↑/↓ 选中 cell,Ctrl+U/D 半屏滚动 |
| 输入区实现 | 自实现行编辑器(放弃 reedline) |
| 历史数据模型 | 结构化 cell 列表 |
| 视觉风格 | 贴近 codex(`> ` / `" ` 前缀,圆点状态图标,`───` 分隔线) |
| Markdown 渲染 | 完整渲染(pulldown-cmark + 可选 syntect 高亮) |
| 折叠交互 | 工具调用/结果默认折叠,Ctrl+O 展开 |

## 架构

### 整体布局

```
┌─────────────────────────────┐
│  历史区 (可滚动)              │  ← ratatui 管理,flex:1
│  > 用户消息                   │     结构化 cell 列表
│  " 助手消息(markdown 渲染)    │     光标可上下移动选中 cell
│  ● 工具调用(折叠)             │     Ctrl+O 展开/折叠选中 cell
│  ─── 分隔线 ───              │
│                             │
├─────────────────────────────┤
│  > 输入区(自实现行编辑)       │  ← 固定 1 行,灰色背景
└─────────────────────────────┘
```

### 核心模块

- `tui/history.rs` — 历史区 widget,管理 `Vec<HistoryCell>` 和滚动状态
- `tui/input.rs` — 自实现输入行(基本编辑 + 历史上下翻)
- `tui/cell.rs` — `HistoryCell` trait + 各类 cell(用户消息/助手消息/工具调用/工具结果/分隔线)
- `tui/markdown.rs` — markdown 渲染(`pulldown-cmark` 解析 + 自定义 ratatui Line 生成)
- `tui/app.rs` — 主事件循环,组装布局,处理按键

### 依赖新增

- `ratatui` — TUI 框架
- `crossterm` — 已有
- `pulldown-cmark` — markdown 解析
- `syntect` — 代码高亮(可选 feature,默认关闭)

## HistoryCell 类型与折叠

### Cell 类型

```rust
enum HistoryCell {
    UserMessage { text: String },
    AssistantMessage { markdown: String, rendered_lines: Vec<Line<'static>> },
    ToolCall { name: String, input: Value, state: CallState, expanded: bool },
    ToolResult { call_id: String, result: ContentBlock, is_error: bool, expanded: bool },
    Separator { label: Option<String> },  // ── Worked for 2m ──
}

enum CallState { Running, Success, Failed }
```

### 折叠规则

- `UserMessage` / `AssistantMessage` / `Separator` — 不折叠,始终展开
- `ToolCall` / `ToolResult` — 默认折叠,只显示摘要行:
  - 折叠时:`● tool_name(args_summary)` (绿色=成功 / 红色=失败 / 黄色动画=运行中)
  - 展开时:摘要行 + 下方缩进显示完整 input JSON 和 result 文本,用 `└` 树形连接符
- `Ctrl+O` 切换当前选中(光标所在)cell 的折叠状态

### 选中与滚动

- 历史区有一个"选中行"光标(暗色高亮整行),默认在底部(最新内容)
- `Shift+↑/↓` 移动选中光标
- 选中光标移出可视区域时自动滚动
- 普通滚动用 `Ctrl+U`/`Ctrl+D`(半屏)或 `Shift+PageUp`/`Shift+PageDown`

### 渲染逻辑

- 每个 cell 实现 `fn lines(&self, width: u16) -> Vec<Line>`
- 折叠的 cell 只返回摘要行(1-2 行)
- 展开的 cell 返回摘要 + 完整内容
- resize 时所有 cell 重新计算折行(因为 width 变了)

## 输入区与事件循环

### 自实现输入行

```rust
struct InputLine {
    buffer: String,       // 当前输入文本
    cursor: usize,        // 字节位置(注意 UTF-8 边界)
    history: Vec<String>, // 历史输入
    history_idx: Option<usize>, // None=当前行, Some(i)=浏览历史
}
```

### 支持的按键

- 基本编辑:字符输入、`Backspace`、`Delete`、`Ctrl+A`(行首)、`Ctrl+E`(行尾)
- 光标移动:`←`、`→`、`Ctrl+B`、`Ctrl+F`
- 历史:`↑`/`↓` 浏览历史输入(类似 shell)
- 提交:`Enter` 发送
- 中断:`Esc` 或 `Ctrl+C` 中断当前 agent 执行
- 粘贴:`Ctrl+V` 或终端 bracketed paste

### 输入区渲染

- 固定 1 行高度(多行输入先不做)
- 灰色背景(color 240),`> ` 前缀
- 光标位置用反色或竖线显示
- 整行填充灰色(`Clear(ClearType::UntilNewLine)`)

### 主事件循环

```rust
loop {
    terminal.draw(|f| {
        // 上半部分:历史区 (flex: 1)
        // 下半部分:输入区 (固定 1 行)
        // 中间 1 行空白间隔
    })?;

    if event_poll? {
        match event {
            Key(Ctrl+O) => toggle_selected_cell_fold(),
            Key(Shift+↑/↓) => move_selection(),
            Key(Ctrl+U/D) => scroll_half_page(),
            Key(Enter) if input_not_empty => submit_input(),
            Key(Esc) => interrupt_agent(),
            _ => input.handle_key(event),
        }
    }

    if let Some(agent_event) = agent_rx.try_recv() {
        history.push_cell(agent_event);
    }
}
```

### Agent 事件接入

- 现有 `AgentEvent` 流通过 channel 传给 TUI 主循环
- `AgentEvent::AssistantText` 流式追加到当前 `AssistantMessage` cell(而非每个 chunk 一个 cell)
- `AgentEvent::ToolCall` / `ToolResult` 创建对应 cell
- `Done::EndTurn` 后插入 `Separator` cell

## Markdown 渲染与代码高亮

### 渲染管线

```rust
fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>>
```

1. **解析:** `pulldown-cmark` 解析 markdown 为 AST events
2. **遍历:** 自定义 `LineBuilder` 遍历 events,生成 `ratatui::text::Line`
3. **折行:** 每个 `Line` 超过 `width` 时按词折行(保留缩进)

### 样式映射(贴近 codex)

| Markdown 元素 | 样式 |
|---|---|
| `# H1` | bold + underlined |
| `## H2` | bold |
| `### H3` | bold + italic |
| `*emphasis*` | italic |
| `**strong**` | bold |
| `` `inline code` `` | cyan |
| `[link](url)` | cyan + underlined |
| `> blockquote` | green |
| `- list item` | 普通文本 + `•` marker(暗色) |
| `1. ordered` | 普通文本 + 数字 marker(light blue) |
| `~~strike~~` | crossed_out |

### 代码块处理

- 代码块不折行(保留原始格式),宽度溢出时截断
- 代码块上下加暗色 `─────` 分隔线,或用背景色区分
- `syntect` 高亮做成可选 feature,默认关闭;关闭时代码块用纯色(cyan 或默认)
- `syntect` 高亮失败时回退到单一色

### 性能考量

- `AssistantMessage` cell 存 `markdown: String`(原始)和 `rendered_lines: Vec<Line>`(缓存)
- 首次渲染时生成 `rendered_lines`,resize 时重新生成
- 流式追加时:每个 chunk 拼到 `markdown` 末尾,重新渲染最后一行(而非全部)

## 视觉风格(贴近 codex)

| 元素 | 前缀 | 样式 | 背景 |
|---|---|---|---|
| 用户消息 | `> ` bold dim | 普通文本,特定元素 cyan | 极淡(12% 白 / 4% 黑) |
| 助手消息 | `" ` dim | 完整 markdown 渲染 | 无 |
| 工具调用(运行中) | `●` 黄色动画 | "Calling" + 工具名 | 无 |
| 工具调用(成功) | `●` green bold | "Called" + 工具名 | 无 |
| 工具调用(失败) | `●` red bold | "Called" + 工具名 | 无 |
| 工具结果 | `└` dim | 结果文本 dim | 无 |
| 轮次分隔线 | — | 全宽 `─` dim,可带 `─ Worked for 2m ─` | 无 |

## 迁移策略

渐进式,保留 InlineRenderer 作后备:

```
阶段 1: 新增 tui/ 模块,不影响现有代码
  ├─ tui/cell.rs        — HistoryCell 类型
  ├─ tui/history.rs     — 历史区 widget
  ├─ tui/input.rs       — 自实现输入行
  ├─ tui/markdown.rs    — markdown 渲染
  └─ tui/app.rs         — 主事件循环

阶段 2: 接入 AgentEvent,新 TUI 可独立运行
  └─ 新增 --tui ratatui 启动选项,默认仍用 InlineRenderer

阶段 3: 验证稳定后,切换默认 TUI,移除 InlineRenderer
```

理由:
- 现有 InlineRenderer 能用,避免一次性大重构的风险
- 新旧 TUI 共享同一个 `AgentEvent` 流,切换成本低
- 可以并行对比两种渲染效果

## 测试策略

| 层次 | 方法 |
|---|---|
| `HistoryCell` | 单元测试:各 cell 类型的 `lines(width)` 输出正确(折叠/展开两种状态) |
| `InputLine` | 单元测试:光标移动、编辑操作、历史浏览、UTF-8 边界 |
| Markdown 渲染 | 单元测试:各 markdown 元素生成正确的 `Line`/`Span` 样式 |
| 折行 | 单元测试:长行在指定 width 下正确折行,保留缩进 |
| 集成 | `ratatui::backend::TestBackend` 渲染整个画面,assert buffer 内容 |
| 手动 | 真实终端跑一遍:流式输出、工具调用折叠、Ctrl+O、滚动、resize |

## 风险点

1. **reedline 移除后失去的功能:** kill-ring、yank、历史搜索 — YAGNI,先不做,用户反馈再加
2. **流式渲染性能:** 高频 chunk 更新可能卡顿 — 批处理(16ms 节流)
3. **resize 处理:** 所有 cell 重新折行可能慢 — 只重新渲染可见区域
4. **syntect 体积大:** 先做成可选 feature,默认关闭,代码块用纯色
