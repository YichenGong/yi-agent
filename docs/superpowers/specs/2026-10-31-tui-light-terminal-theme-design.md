# TUI 浅色终端配色适配 设计

**日期**: 2026-10-31
**状态**: 待实现
**相关代码**: `yi-agent-rs/crates/yi-agent/src/tui/`（`app.rs`、`cell.rs`、`markdown.rs`、`statusbar.rs`、`bash_popup.rs`、`process_popup.rs`、`trace.rs`、`queued.rs`）

## 1. 背景

### 1.1 现状：TUI 没有配色层

`yi-agent` 的 TUI 是 ratatui 全屏界面。它的所有颜色都是**散落在 7 个文件里的字面量**，从来没有问过终端背景是什么颜色：

- `yi-agent-rs/crates/yi-agent/Cargo.toml` 只有 `crossterm 0.28` + `ratatui 0.29`，**没有** `termbg` / `ansi_colours` 之类的背景探测依赖（`Cargo.lock` 亦无）。
- 全仓库 grep `OSC 11`、`SetBackgroundColor`、`SetForegroundColor`、`NO_COLOR`、`COLORFGBG`、`TERM_PROGRAM` —— **零命中**。
- `config.rs`、`tui/runtime_prefs.rs` 里没有终端配色字段；没有任何 `/theme` 命令。

所以答案是明确的：**TUI 没有做浅色适配**。

### 1.2 但表现不是「全屏黑底」，而是「大量可读 + 少数失效」

关键事实：**历史区、queued 区、状态栏、弹窗都没有设置背景色**（`history.rs` / `cell.rs` / `queued.rs` 里 `bg(` 零命中）。未写样式的单元格走 ratatui 的 `Color::Reset`，不下发 SGR 48，于是**透出终端自身的背景**。

结果是在浅色终端里，正文是「终端默认前景（通常近黑）压终端默认背景（白）」—— 能读。真正失效的只是少数几处把深色语境写死的值。

### 1.3 量化：哪些元素在浅底上失效

用 WCAG 相对亮度公式计算对比度（正文门槛 4.5:1，大字号 3:1）。ANSI 色按标准 xterm 256 调色板取值。

| 元素 | 代码位置 | 浅底 | 深底 | 判定 |
|---|---|---|---|---|
| 输入条背景 `Indexed(240)` = `#585858` | `tui/app.rs:3518,3521` | 7.11 | 2.34 | 浅色终端里唯一一块**深灰斑** |
| 输入条上的默认字色（近黑） | 同上 | **2.95** | — | 深字压深灰条，低于 AA |
| `fg(DarkGray)` 共 24 处 | `cell.rs:223,233,313`、`statusbar.rs:237,268`、`trace.rs:366,396,400,483,492,562,570,575,587,592`、`bash_popup.rs:124,186,212,215,221,224,232`、`process_popup.rs:124,243,250,253,260,265` | 7.11 | **2.34** | 浅底反而更清楚 |
| `fg(Gray)` 共 2 处 | `statusbar.rs:248,257`（prefill/decode 计数） | **1.90** | 8.78 | 浅底**几乎看不见** |
| 光标 `fg(Black)+bg(White)` | `tui/app.rs:3594` | **1.00** | 21.0 | 浅底光标**完全消失** |
| 状态栏呼吸点 `Rgb(v,v,v)`，v∈[100,255] | `statusbar.rs:117-122`（`spinner_color`），用于 `statusbar.rs:203` | v=255 时 **1.00**，v=100 时 5.92 | 反向 | 脉冲最亮段在浅底不可见 |
| `fg(Cyan)` 共 7 处 | `markdown.rs:457,466,477,529`、`cell.rs:368`、`trace.rs:556,558` | 2.93 | 5.69 | 浅底偏淡 |
| `fg(Yellow)` 共 4 处 | `markdown.rs:740`、`cell.rs:366`、`trace.rs:545`、`app.rs:3516` | 2.27 | 7.35 | 浅底偏淡 |
| 选中行 `bg(Blue)+fg(White)` | `app.rs:3497`、`bash_popup.rs:127`、`process_popup.rs:144` | 可读 | 可读 | 双向可用，仅风格不统一 |

两个值得注意的观察：

- **`DarkGray` 在深底只有 2.34:1，在浅底才是 7.11:1。** 这说明这些灰阶并非照着任何一种背景选的，是随手写的 —— 两种背景都不完全对。所以本设计不宣称「深色是基线、只需补浅色」，而是**两侧都按对比度重新定值**。
- 弹窗用 `Clear`（`app.rs:627,1004,1031,1042,1055,1084,1119,1127,1133,1161`）。`Clear` 只清成**终端默认背景**，弹窗是一层「透明浮层」而非卡片，在浅色下会和正文糊在一起。

### 1.4 与桌面端的对照

`docs/bug-list.md:27` 修过桌面端同类问题（字面色阶不随主题反色），但那套修复只落在 `desktop/`，`desktop/src/lib/themeTokens.test.ts` 的静态门禁也只扫 TSX。**TUI 这一侧没有对应的语义 token 体系，也没有门禁。**

## 2. 目标

**给 TUI 引入一层主题抽象，让同一份代码在浅色和深色终端上都可读，且默认行为不回归。**

具体：

1. 新增 `TuiTheme`，把现有字面量映射到语义槽位。
2. 两种主题：`Dark`（等价于今天的观感）与 `Light`（浅底可用）。
3. 启动时探测一次终端背景，失败回退 `Dark`；`YI_AGENT_THEME=light|dark` 可强制覆盖。
4. 修掉 §1.3 表里所有低于 4.5:1 的项（光标、`Gray`、`spinner` 高亮段、输入条背景）。
5. 加静态门禁，防止新代码再写裸 `Color::Gray` / `Color::DarkGray` / `Color::Indexed(`。

非目标：

- 不做 syntax highlighting（仍是既有 `[ ]` 项）。
- 不做运行中 `/theme` 热切换（首版只认启动配置；见 §5）。
- 不改桌面端配色（已有一套独立 token 体系）。
- 不改 `NO_COLOR` 下的无色彩行为 —— 目前**本来就没有**该支持，列为 §6 的独立问题。
- 不重排任何布局，不改任何文案。

## 3. 设计

### 3.1 语义槽位

`TuiTheme` 只暴露语义槽位，不暴露「第几号 ANSI 色」。槽位取自现有代码里实际出现的用途：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiThemeKind { Dark, Light }

#[derive(Debug, Clone, Copy)]
pub struct TuiTheme {
    pub kind: TuiThemeKind,
    /// 正文前景。用于必须显式指定前景的场合（如输入条上的文字）。
    pub fg: Color,
    /// 次要信息：工具结果摘要、trace 步骤、弹窗空态占位。
    pub fg_muted: Color,
    /// 最弱信息：状态栏模型名、折叠提示、`已略去 N 条`。
    pub fg_faint: Color,
    /// 强调色：行内代码、公式、slash 菜单、`发消息>`。
    pub accent: Color,
    /// 进行中 / 提示。
    pub warn: Color,
    /// 失败 / 拒绝 / 取消。
    pub danger: Color,
    /// 成功 / 完成。
    pub success: Color,
    /// 抬升表面的背景（输入条、后续可能的卡片）。
    pub bg_raised: Color,
    /// 选中行前景 / 背景。
    pub sel_fg: Color,
    pub sel_bg: Color,
    /// 光标。用 REVERSED 时这两项不参与渲染，仅为需要实心块的场合保留。
    pub cursor_fg: Color,
    pub cursor_bg: Color,
}
```

### 3.2 取值（含对比度实测）

深色一侧以 `#1e1e1e` 为假想底色、浅色一侧以 `#ffffff` 为假想底色。所有值都按 WCAG 相对亮度公式验算过：

```rust
impl TuiTheme {
    pub const DARK: TuiTheme = TuiTheme {
        kind: TuiThemeKind::Dark,
        fg:         Color::Rgb(0xe4, 0xe4, 0xe4), // 13.11:1
        fg_muted:   Color::Rgb(0xb2, 0xb2, 0xb2), //  7.86:1
        fg_faint:   Color::Rgb(0x8c, 0x8c, 0x8c), //  4.96:1
        accent:     Color::Rgb(0x5f, 0xd7, 0xd7), //  9.68:1
        warn:       Color::Rgb(0xe3, 0xc5, 0x6b), //  9.90:1
        danger:     Color::Rgb(0xff, 0x7b, 0x72), //  6.61:1
        success:    Color::Rgb(0x7e, 0xe7, 0x87), // 10.85:1
        bg_raised:  Color::Rgb(0x2a, 0x2a, 0x2a), // 与 #1e1e1e 亮度差 1.16
        sel_fg:     Color::Rgb(0xff, 0xff, 0xff), // 5.06:1 on sel_bg
        sel_bg:     Color::Rgb(0x2f, 0x6f, 0xbf),
        cursor_fg:  Color::Rgb(0x1e, 0x1e, 0x1e),
        cursor_bg:  Color::Rgb(0xe4, 0xe4, 0xe4),
    };

    pub const LIGHT: TuiTheme = TuiTheme {
        kind: TuiThemeKind::Light,
        fg:         Color::Rgb(0x1f, 0x1f, 0x1f), // 16.48:1
        fg_muted:   Color::Rgb(0x4a, 0x4a, 0x4a), //  8.86:1
        fg_faint:   Color::Rgb(0x6b, 0x6b, 0x6b), //  5.33:1
        accent:     Color::Rgb(0x0a, 0x6b, 0x6b), //  6.31:1
        warn:       Color::Rgb(0x7a, 0x5c, 0x00), //  6.25:1
        danger:     Color::Rgb(0xa0, 0x18, 0x18), //  7.96:1
        success:    Color::Rgb(0x14, 0x69, 0x1f), //  6.84:1
        bg_raised:  Color::Rgb(0xed, 0xed, 0xed), // 与 #ffffff 亮度差 1.17
        sel_fg:     Color::Rgb(0xff, 0xff, 0xff), // 6.44:1 on sel_bg
        sel_bg:     Color::Rgb(0x1f, 0x5f, 0xa8),
        cursor_fg:  Color::Rgb(0xff, 0xff, 0xff),
        cursor_bg:  Color::Rgb(0x1f, 0x1f, 0x1f),
    };
}
```

**为什么用真彩 `Rgb` 而不是 ANSI 灰阶：** ANSI 16 色的实际像素由终端主题决定，同一个 `DarkGray`（SGR 90）在 iTerm 和 Terminal.app 下完全不同 —— 这正是当前问题的来源之一。固定 `Rgb` 让观感可预测。代价是 256 色终端会降级，见 §5。

**浅色侧不用「反色」：** 直接把深色值取反会得到刺眼的纯白/纯黑，且对比度失控。上表是重新挑的值，每个都过了 4.5:1。

### 3.3 探测：OSC 11 + 环境变量覆盖

优先级从高到低：

1. **`YI_AGENT_THEME` 环境变量**：`light` / `dark`（大小写不敏感，容忍首尾空白）。非法值记一条 `tracing::warn!` 后继续下一级。
2. **OSC 11 查询**：向终端写 `ESC ] 11 ; ? ESC \`（`\x1b]11;?\x1b\\`），读回复 `ESC ] 11 ; rgb:RRRR/GGGG/BBBB ESC \`（也接受 `rgba:` 与 `\x07` 作为终止符）。取 RGB 的**相对亮度**，`> 0.5` 判为浅色。
3. **DPI 兜底**：探测失败（无 TTY、非交互、超时、回复无法解析）→ `Dark`。

探测必须**不阻塞启动**、**不污染输出**：

- 全程 `enable_raw_mode()` 之后、进入备用屏幕**之前**完成（`app.rs:132` 与 `:133` 之间），此时屏幕还没被接管。
- 用 `poll(timeout)` 轮询而不是阻塞读；**上限 200ms**。超时就放弃并回退 `Dark`。
- 读到的任何非 OSC 11 回复字节必须**丢弃且不回显**（raw mode 下不会回显，但要显式丢弃，避免影响后续输入）。
- 探测结束后必须确保终端已无残留输入。

**为什么不用 `termbg` crate：** 它可以省事，但要引入新依赖并连带一套 `crossterm` 版本约束。仓库已有 `libc`（`yi-agent-rs/crates/yi-agent/Cargo.toml:59`）且只用 crossterm，自己写约 60 行、可完全掌控超时与失败路径。取舍见 §5。

### 3.4 渲染层的接入方式（关键取舍）

现有渲染函数签名如下，**非测试调用点很少**（实测）：

| 函数 | 文件 | 非测试调用点 |
|---|---|---|
| `HistoryCell::lines(&self, width)` | `cell.rs:79` | 4 |
| `render_markdown(src, width)` | `markdown.rs:17` | 2 |
| `render_queued_preview(items, width)` | `queued.rs:137` | 1 |
| `render_subagent_list(...)` | `trace.rs:370` | 1 |
| `render_subagent_detail(...)` | `trace.rs:436` | 1 |
| `process_detail_lines(...)` | `process_popup.rs:202` | 3 |
| `detail_lines(task, width)` | `bash_popup.rs:178` | 6 |
| `render_statusbar(...)` | `statusbar.rs` | 0（只经 `app.rs` 内一层） |

而 `render_markdown` 的 68 次出现里有 65 次在 `markdown.rs` 自己的测试中。

**方案：保留每个公开函数的现有签名，在其内部转发到带 theme 的变体。**

```rust
// markdown.rs
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    render_markdown_with(src, width, TuiTheme::DARK)
}
pub fn render_markdown_with(src: &str, width: u16, theme: TuiTheme) -> Vec<Line<'static>> { ... }
```

理由：

- **零回归风险**：所有既有测试断言（如 `markdown.rs:1336` 断言 `fg == Some(Color::Cyan)`）继续通过，不必一次改上百个断言。
- **不制造误伤**：`run_tui_with_backend_and_events` 有 31 处调用，不动它的签名。
- **渐进**：每个文件可独立迁移，任务边界清晰（见 plan）。

代价是「默认深色」这个隐含约定留在旧入口上。用一条静态门禁兜住：旧入口只允许被测试或 `_with` 变体调用（见 §4.3）。

真正需要传 theme 的路径是**生产渲染路径**：`run_loop` 从 `run_tui` 收一个 `TuiTheme`，经 `HistoryView` / `cell::lines_with` / `render_statusbar_with` / 三个弹窗一路传下去。`HistoryView` 需要新增一个 `theme` 字段，并在渲染缓存 key 里带上 theme（见 §3.6）。

### 3.5 逐项修复

| 现状 | 改为 | 效果 |
|---|---|---|
| `app.rs:3518,3521` `bg(Indexed(240))` | `bg(theme.bg_raised)` | 浅底浅灰条、深底深灰条 |
| `app.rs:3594` `fg(Black)+bg(White)` | `Style::new().add_modifier(Modifier::REVERSED)` | **两种背景都成立**，这是根因级修复 |
| `statusbar.rs:248,257` `fg(Gray)` | `fg(theme.fg_muted)` | 浅底从 1.90 → 8.86 |
| `statusbar.rs:237,268` `fg(DarkGray)` | `fg(theme.fg_faint)` | 两侧都 ≥4.96 |
| `statusbar.rs:117-122` `Rgb(v,v,v)` | 按亮度插值 `theme.fg_faint ↔ theme.fg` | 浅底脉冲在深灰↔黑之间摆，不再消失 |
| 24 处 `fg(DarkGray)` | `fg(theme.fg_muted)` 或 `fg(theme.fg_faint)`（按语义分） | 语义化 |
| `fg(Cyan)` | `fg(theme.accent)` | 浅底 6.31 |
| `fg(Yellow)` / `fg(Red)` / `fg(Green)` | `theme.warn` / `theme.danger` / `theme.success` | 两侧达标 |
| `bg(Blue)+fg(White)` | `bg(theme.sel_bg)+fg(theme.sel_fg)` | 统一，两侧 5.06/6.44 |
| 弹窗 `Clear` | `Clear` + `bg(theme.bg_raised)` | 卡片有边界，浅色下不再糊 |

`spinner` 的插值公式改为在两个**主题色**之间线性插值（RGB 各通道），保留现有 `0..360` 相位与 `sin` 波形，只是端点换成主题色。这样 `statusbar.rs:203` 的调用点不变。

### 3.6 缓存正确性

`HistoryState` 有一份按宽度缓存的渲染行（`tui/history.rs` 的 `HistoryCache`，见 `2026-09-30-tui-history-render-cache-design.md`）。**缓存 key 必须包含 theme**，否则主题变化会命中陈旧行。

首版主题在进程生命周期内不变（启动时定一次），所以理论上不会触发。但为了不给后续加 `/theme` 热切换留陷阱，缓存 key 里显式带上 `theme.kind`，并加一条测试断言（`history.rs`）：换 theme 后缓存必须失效。

### 3.7 不污染 TUI 的约束

探测期间**绝不能**往 stdout/stderr 写任何东西。`docs/bug-list.md:101` 记录过一次教训：库往全局 stderr 裸写一行，落进 TUI 正在绘制的帧中间，把输入框背景涂花。

因此：

- 探测内部的所有失败路径只 `tracing::warn!`，不 `eprintln!`。
- 探测在进入备用屏幕前完成，即便将来出现意外输出也在正常滚动区内，不会污染帧。
- 加一条测试：探测失败（模拟 200ms 超时）后 `stdout` 缓冲区为空。

## 4. 测试

### 4.1 主题取值

```rust
#[test] fn both_themes_meet_contrast_floor()      // 每个前景槽位对其假定底色 >= 4.5
#[test] fn raised_surface_is_distinguishable()    // bg_raised 与假定底色亮度差 >= 1.15
#[test] fn selection_text_is_readable()           // sel_fg on sel_bg >= 4.5，两种主题
```

`both_themes_meet_contrast_floor` 用仓库内自实现的 `relative_luminance`（约 12 行，无新依赖），而不是引入 `palette` 之类。

### 4.2 探测

```rust
#[test] fn env_var_overrides_probe()              // YI_AGENT_THEME=light → Light，即便 OSC 回复是深色
#[test] fn invalid_env_var_falls_through()        // YI_AGENT_THEME=purple → 继续走探测
#[test] fn osc11_reply_parsed_as_light()          // "\x1b]11;rgb:ffff/ffff/ffff\x1b\\" → Light
#[test] fn osc11_reply_parsed_as_dark()           // "\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\" → Dark
#[test] fn osc11_bel_terminator_is_accepted()     // 用 \x07 结尾同样解析
#[test] fn probe_timeout_falls_back_to_dark()     // 无回复 → Dark，且不 panic
#[test] fn probe_failure_writes_nothing()         // 失败路径 stdout 为空
#[test] fn unparseable_reply_falls_back_to_dark()
```

探测逻辑与 IO 解耦：`detect_theme(reader: &mut impl BufRead, env: Option<&str>) -> TuiThemeKind` 便于单测，真实 IO 走一层薄包装。

### 4.3 渲染

```rust
#[test] fn input_stripe_uses_theme_raised()       // 浅色下输入条 bg == LIGHT.bg_raised
#[test] fn cursor_uses_reversed_modifier()        // 光标格带 REVERSED，不再硬编码 White
#[test] fn statusbar_gray_is_readable_on_light()  // 浅色主题下 prefill/decode 段 fg == LIGHT.fg_muted
#[test] fn spinner_pulses_between_theme_colors()  // 相位 0..360 两端的 fg 分别落在 LIGHT.fg_faint 与 LIGHT.fg 之间
#[test] fn light_theme_render_snapshot()          // 一整帧快照，TestBackend，断言无 cell 使用裸 ANSI 灰阶
```

`light_theme_render_snapshot` 是主验收：用 `TestBackend` 渲染一帧含用户消息、markdown、工具调用、状态栏、输入条的完整界面，遍历 buffer，断言**没有任何 cell** 的 `fg`/`bg` 落在「会导致低对比」的集合里（即 `Color::Gray`、`Color::DarkGray`、`Color::Indexed(240)`、`Color::White` 作为输入条背景）。

现有 3 个光标测试（`app.rs:7582` `cursor_shown_with_reverse_video`、`:7640` `cursor_visible_on_empty_buffer`、`:7690` `cursor_in_middle_of_text`）断言 `cell.bg == Color::White`。改用 `REVERSED` 后必须同步改写为断言 `modifier.contains(Modifier::REVERSED)` —— **这是行为等价的断言迁移，不是放宽测试**。

### 4.4 静态门禁

新增 `tui/theme_guard.rs`（`#[cfg(test)]` 内）或独立测试文件 `crates/yi-agent/tests/tui_theme_guard.rs`：

扫描 `crates/yi-agent/src/tui/` 下所有 `.rs`，**排除** `theme.rs` 自身与 `#[cfg(test)]` 之后的内容，出现下列模式即失败：

- `Color::Gray`（裸 `Color::Gray`；`DarkGray` 单独一条）
- `Color::DarkGray`
- `Color::Indexed(`

例外白名单（文件:行 或 明确的注释标记 `// theme-guard: allow`）—— 首版应为空。

对应防复发说明：桌面端同类门禁是 `desktop/src/lib/themeTokens.test.ts`，本设计是它在 TUI 侧的对等物。

## 5. 取舍与不做的部分

**真彩 `Rgb` vs ANSI 色。** 选 `Rgb`，因为 ANSI 色的实际观感由终端主题决定，这正是浅色下失效的根源。代价：256 色终端上 `Rgb` 会被终端自行降级到最接近的调色板项，退化后未必精确，但不会比今天更糟（今天用的就是 ANSI 灰阶）。

**自己写探测 vs 引 `termbg`。** 自己写约 60 行，完全掌控 200ms 超时与「失败绝不输出」这条硬约束；引依赖要连带版本约束，且 `termbg` 的某些路径会写终端。选自己写。

**保留旧签名 vs 全量改签名。** 选保留。`render_markdown` 有 68 处出现（65 处是测试），全量改会淹没真正的逻辑变更。代价是多一对函数，用门禁约束旧入口只被测试和 `_with` 调用。

**不做 `/theme` 热切换。** 首版只认启动配置。热切换需要 `HistoryState` 缓存全量失效 + 重绘，收益低（用户换终端主题的频率远低于换应用主题）。若将来要做，§3.6 已把缓存 key 铺好。

**不做 `NO_COLOR`。** 见 §6。

**不改 `bg(Blue)` 之外的 16 色语义。** 例如 `Line::styled("stdout:", fg(DarkGray))` 这种「标签」色，统一归到 `fg_muted`。

**深色观感会有细微变化。** `Dark` 主题值与原字面量不完全相同（例如 `DarkGray` `#585858` → `fg_muted` `#b2b2b2`，因为原值在深底只有 2.34:1 本就偏暗）。这是**修正**而非回归，但要在 plan 的验收里明确记录「深色下次要文字会比今天更亮」。

## 6. 观测到但不在本设计范围内的问题

1. **没有 `NO_COLOR` / `CLICOLOR=0` 支持**（全仓库零命中）。对色盲、纯日志重定向场景是缺陷，但与本设计正交。
2. **`docs/bug-list.md:57`**「命令行需要输入密码的话，TUI 会出现显示故障」—— 与终端背景无关，是另一个未解问题；本设计的探测逻辑（raw mode 期间读字节）**不得**加剧它，实施时要回归这条。
3. **弹窗 `Clear` 后未铺底** 在深色下也是「纯终端背景」，视觉上是透明的。本设计顺带给了 `bg_raised`，但真正做成「卡片 + 阴影」需要另立设计。
4. **`markdown.rs:740` 的 `Yellow`** 用于什么语义需实施时确认（当前看是某个列表/标记），归类前必须读代码确认，不要凭色名硬套 `warn`。

## 7. 验收

- `cargo test -p yi-agent --bin yi-agent tui::theme` 全绿（§4.1 + §4.2）
- `cargo test -p yi-agent --bin yi-agent tui::` 全绿（含既有渲染测试，无回归）
- `cargo test -p yi-agent --bin yi-agent tui_theme_guard` 全绿（§4.4），且**故意插入一处 `Color::Gray` 能变红**
- `cargo fmt --all` 干净；`cargo clippy -p yi-agent --all-targets` 无新增 warning
- 手工验收：在浅色 Terminal.app 里启动 TUI，确认（a）输入条是浅灰而非深灰斑；（b）光标可见；（c）状态栏 prefill/decode 数字可读；（d）运行中呼吸点脉冲全程可见
- 手工验收：在深色终端里启动，确认无「比今天更糟」的区域（允许更亮的次要文字，见 §5）
- `YI_AGENT_THEME=light` 在深色终端下强制浅色主题生效（反向亦然）
