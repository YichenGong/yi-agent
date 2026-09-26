# 权限确认弹窗命令完整可见设计

**目标：** 让 TUI 权限确认弹窗完整、准确地展示待确认的工具调用内容，使
用户能据此做出「允许/拒绝」的判断；同时保证命令文本在渲染过程中**不被
改写**。

**状态：** 设计已确认，待转实现计划。

---

## 1. 问题

用户在确认 bash 命令是否允许时，弹窗里只能看到命令的**前半截**，看不到
完整命令，因此无法正确判断内容。

复现（80 列终端，bash 权限请求）：

```
? Permission needed: bash
  bash: {"command":"cd /Users/someone/projects/personalProjects/yi-agent && carg
  [1] Allow once
  [2] Always allow tool
  [3] Always allow prefix: cd
  [4] Deny
  Enter = Allow once
```

命令在 `&& carg` 处被切断，其后的
`o test -p yi-agent-core --lib permission","expected_timeout_sec":120}` 用户
完全看不到。

实测丢失量（`render_permission_request` 输出行宽 vs 80 列终端）：

| 工具 | display 字符数 | 渲染后最大行宽 | 右侧丢失列数 |
|---|---|---|---|
| bash | 147 | 149 | 69 |
| write | 1322 | 1324 | 1244 |
| edit | 460 | 462 | 382 |
| process_start | 143 | 145 | 65 |

丢失是**永久性**的：不是换行、也不可滚动，右侧被直接裁掉。

## 2. 根因（已核实）

1. **渲染忽略宽度**：`yi-agent-rs/crates/yi-agent/src/tui/cell.rs:211-217`
   `render_permission_request` 的 `width` 参数被命名为 `_width`，函数体从不
   使用它。
2. **内容塞进单个 Line**：`cell.rs:235`
   `Line::from(format!("  {display}")).style(dim_style)` —— 整条内容是一个
   `Line`。
3. **display 是原始 JSON**：`tui/history.rs:402`
   `let display = format!("{}: {}", tool_name, tool_input);`
4. **每个 Line 只占 1 行高度**：`tui/history.rs:519-535` 把每个 `Line` 渲染
   进 `height: 1` 的 `Rect`（`history.rs:532`）。
5. **ratatui 对超宽 Line 是截断而非换行**：
   `ratatui-0.29.0/src/text/line.rs:731-732` 注释明确
   "As the right side is truncated by the area width, only truncate the
   left"，`skip_width` 只用于左侧偏移，右侧被 area 宽度裁掉。

同仓库已有正确范例，说明这是遗漏而非设计选择：

- `cell.rs:313 wrap_with_prefix`（用户消息换行）
- `bash_popup.rs:229 wrap_text`（bash 详情换行），由 commit
  `4d154a2 fix(tui): wrap bash detail content` 专门修复

权限弹窗这条路径漏了同样的处理。

## 3. 现状关键事实（实现约束）

以下事实均已在代码中核实，决定设计的形状：

- **cell 不持有 `tool_input`**：`HistoryCell::PermissionRequest`
  （`cell.rs:34-42`）只有预格式化好的 `display: String`。因此「按工具精简」
  必须在**事件到达时**做（`history.rs:395-411`），cell 保持为纯渲染器，不
  感知 JSON 结构。
- **唯一构造点**：`history.rs:403` 是唯一构造该 cell 的地方；其余引用为
  读取（`history.rs:280`、`418`）与测试（`history.rs:1044`、`1068`）。
- **没有测试断言 `display` 内容**：已 grep 确认，故字段重命名/格式调整风险低。
- **权限待确认时按键被吞**：`tui/app.rs:841-863` 对除
  `1/2/3/4/Enter/Ctrl+Q/Esc` 外的所有按键返回 `KeyOutcome::None`。因此
  `Up/Down/PageUp/PageDown` **到不了** history 滚动逻辑。若 `[e]` 展开出的
  内容高于视口，顶部将无法到达。
- **纯 `e` 键未被占用**：`grep "KeyCode::Char('e')"` 无匹配，`e` 可用。
- **history 区高度充足**：实测 `compute_layout`（`app.rs:2122-2151`）：

  | 终端高度 | history 区高度 |
  |---|---|
  | 24 | 21 |
  | 30 | 27 |
  | 40 | 37 |

  24 行终端下 history 有 21 行，折叠态弹窗（实测 11 行，黑名单场景 12 行）
  可完整容纳。
- **`wrap_with_prefix` 会改写命令（安全缺陷）**：`cell.rs:324` 用
  `split_whitespace()` 并按单个空格重组，导致连续空白被折叠。实测：

  ```
  original : python3 -c "print('a    b')" && git commit -m "fix:  spacing   matters"
  rendered : > python3 -c "print('a b')" && git commit -m "fix: spacing matters"
  ```

  对用户消息（回显输入）尚可接受，但对**权限闸门**不可接受：用户批准的
  文本与实际执行的命令不一致。heredoc、引号内多空格、对齐 SQL、
  `-m "fix:  two  spaces"` 都会被错误呈现，且会破坏
  `[3] Always allow prefix` 的语义（用户看到的 prefix 与真实命令不匹配）。
- **`bash_popup.rs:229 wrap_text` 是按字符宽度切分**，保留原始字符，是正确
  的模型。

## 4. 设计

### 4.1 数据（`tui/history.rs`）

在事件到达处（`history.rs:395-411`）由 `tool_input` 计算两个字符串：

- `summary`：折叠态显示，按工具精简
- `full`：展开态显示，`tool_name` + `serde_json::to_string_pretty(tool_input)`

`summary` 生成规则：

| 工具 | summary | 理由 |
|---|---|---|
| `bash`, `process_start` | 仅 `command` | 判断依据就是命令本身；剔除 `timeout`/`env`/`expected_timeout_sec` 噪音 |
| `write`, `edit` | `path` + 各字段字节数（如 `content: 1322 bytes`） | 判断依据是「写哪个文件」；内容块不是安全信号，且体积巨大 |
| 其他 | `tool_input.to_string()` | 安全兜底 |

字段调整：`display: String` 重命名为 `summary: String`（语义更准确），新增
`full: String` 与 `expanded: bool`。已确认无测试断言 `display` 内容。

### 4.2 共享换行器（新增 `tui/wrap.rs`）

抽出按显示宽度、**保留原始字符**的换行函数：

```rust
pub fn wrap_by_display_width(
    text: &str,
    width: usize,
    first_prefix: &str,
    cont_prefix: &str,
) -> Vec<String>
```

- 按 `\n` 切分，保留显式换行与空行
- 每个物理行按 `UnicodeWidthChar` 逐字符切分，**不改写任何字符**（空格、
  tab 原样保留）
- 前缀计入宽度
- 返回纯字符串，样式由调用方施加

`bash_popup::wrap_text` 改为委托给它（`map(Line::raw)`），消除重复实现；
现有测试 `detail_wraps_long_command_stdout_stderr_and_cjk` 作为等价性回归。

**不使用 `wrap_with_prefix`** 渲染权限内容（原因见 3 节安全缺陷）。用户消息
保持现状，不在本次范围内。

### 4.3 渲染（`tui/cell.rs`）

`render_permission_request` 启用 `width` 参数，输出顺序固定为：

```
? Permission needed: {tool_name}
{body，按 width 换行}
[!] Blacklisted command            （仅黑名单时）
[1] Allow once
[2] Always allow tool
[3] Always allow prefix: {prefix}  （有 prefix 时；prefix 行同样换行）
[4] Deny
Enter = {Allow once | Deny}
```

- `body`：折叠态用 `summary`，展开态用 `full`
- 折叠态最多 `MAX_COLLAPSED_LINES = 4` 行，超出则截断并追加
  `  … (+M 行，按 [e] 展开)`；展开态不截断，追加 `  [e] 收起`
- 未被截断且未展开时不显示 `[e]` 提示
- **header、菜单、hint 永远在 body 之后渲染**，因此不可能被 body 顶出屏幕
- `resolved` 分支（`cell.rs:223-228`）同样改用换行器，不再单行截断

折叠态行数上界（实测各项计数）：1 header + ≤4 body + 1 截断提示 +
4 menu + 1 hint = 11 行；黑名单场景多 1 行 `[!]` 警告 = 12 行。两者都小于
24 行终端下的 history 区高度（21 行），菜单可完整显示。

### 4.4 按键（`tui/app.rs`）

在权限待确认分支（`app.rs:841-863`）内：

- `e`：切换当前待确认 cell 的 `expanded`（新增
  `HistoryState::toggle_pending_permission_expanded() -> bool`；因
  `pending_permission_info` 返回借用，需独立方法避免借用冲突）
- `Up` / `Down` / `PageUp` / `PageDown`：**放行**给 history 滚动逻辑（非破坏性
  操作，安全）。修复 3 节所述「展开后顶部不可达」问题
- 决策键 `1/2/3/4/Enter`、退出键 `Ctrl+Q`/`Esc` 行为不变

## 5. 测试

先写失败测试再实现。关键用例：

**`cell.rs`**

1. `permission_body_never_exceeds_width` —— 四种工具在 width 40 与 80 下所有
   渲染行宽 ≤ width（直接覆盖本 bug）
2. `permission_wrap_preserves_exact_command_whitespace` —— 命令含多空格/引号
   时，渲染结果包含原始命令的**精确子串**（安全回归）
3. `permission_collapsed_body_is_bounded` —— 长 body 折叠后行数 ≤ 上界
4. `permission_collapsed_shows_expand_hint_when_truncated`
5. `permission_expanded_recovers_command_tail` —— 展开后能找到命令尾部
6. `permission_menu_rendered_after_body` —— 4 个选项均在，且位于 body 之后
7. `permission_cjk_body_wraps_at_display_width`
8. `permission_resolved_line_wraps`

**`history.rs`**

9. `permission_summary_for_bash_drops_json_noise` —— 不含
   `expected_timeout_sec`
10. `permission_summary_for_write_hides_content_blob` —— 含 path 与字节数，
    不含内容正文
11. `toggle_pending_permission_expanded_toggles_only_pending`

**`app.rs`**

12. `permission_key_e_toggles_expanded`
13. `scroll_keys_work_while_permission_pending` —— 待确认时 `Up` 改变
    `scroll_offset`

**`bash_popup.rs`**

14. 现有 `detail_wraps_long_command_stdout_stderr_and_cjk` 保持通过（换行器
    抽取后的等价性回归）

## 6. 范围

**改动文件：** `tui/cell.rs`、`tui/history.rs`、`tui/app.rs`、新增
`tui/wrap.rs`、`tui/bash_popup.rs`（委托），以及
`docs/project-management/permission.md`、`docs/project-management/yi-agent-tui.md`
与 `README.md` 计数同步。

**不做：**

- 不改 `yi-agent-core` 权限判定逻辑，不改决策协议
  （`PermissionRequest`/`PermissionResolved` 事件不变）
- 不改用户消息换行（`wrap_with_prefix` 保留现状）
- 不为 write/edit 做专门的内容视图；展开态统一显示 pretty JSON。若日后需要
  更可读的 write/edit 视图，单独迭代
- 不改 `resolved` 之后的持久化格式
