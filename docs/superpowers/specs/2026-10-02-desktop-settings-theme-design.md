# 桌面端设置界面与主题（通用 Tab）设计

日期：2026-10-02
状态：待实现
模块：[desktop（Tauri GUI）](../../project-management/desktop.md)

## 问题

桌面端没有任何「设置」入口：唯一的设置类 UI 是中列看板面板顶部的
Superpowers 看板开关（`desktop/src/components/SuperpowersKanbanSettings.tsx`），
它既不可发现、也不可扩展。用户想要一个真正的设置界面。

同时桌面端是**深色单主题**：`index.css` 只有 `@import "tailwindcss"` 与
typography 插件，没有任何 dark 模式配置；配色由 187 处硬编码的 `neutral-*`
工具类散落在组件里（`ThreadSidebar.tsx` 43 处、`SubagentTrace.tsx` 28 处、
`ApprovalDialog.tsx` 18 处…）。因此「支持白天/黑夜两种模式」不是加一个开关，
而是要先建一层与主题无关的语义色 token。

用户还要求这两种模式**可以用自然语言对话控制**，例如「帮我切成亮色」。

## 目标

- `ThreadSidebar` 左下角新增设置按钮，打开一个设置界面。
- 设置界面含左侧垂直 Tab 栏；本次只实现「通用」Tab，Tab 结构按可扩展设计。
- 「通用」Tab 支持深色 / 浅色两种主题，点击即时生效并持久化。
- 主题支持两种控制路径，且共用同一份状态：
  1. 设置界面里的手动切换；
  2. 对话自然语言，由 agent 调用工具完成。
- 首屏不闪烁：启动即渲染正确主题。

## 非目标

- 不搬入其它设置项（Superpowers 看板开关留在原地，不属于本次范围）。
- 不做「跟随系统」第三态（只做显式深色 / 浅色两态；留作后续）。
- 不实现 web 的 16 个环境变量配置（那是后续 Tab，见「后续」）。
- 不动 TUI（`yi-agent-rs/` 的 TUI 侧），不改 core 的主题无关逻辑。
- 不把状态色（`amber` / `red` / `blue` / `emerald`）token 化——它们在两套主题下
  都可读，保留字面量。

## 方案

### 1. 入口与设置界面

**入口**：`ThreadSidebar` 当前结构是「顶部 New thread 按钮 → 中间可滚动列表 →
右侧拖拽分隔条」（`desktop/src/components/ThreadSidebar.tsx:364` 起的 `return`），
没有 footer。在滚动区之后、分隔条之前新增一条 footer，左对齐一个齿轮按钮
（`aria-label="设置"`），点击回调 `onOpenSettings` 由 `App.tsx` 注入。

**形态**：设置用**覆盖层模态**，不用侧栏内嵌（侧栏宽度仅 200–480px，
`sidebarWidth.ts` 的夹取范围，放不下带 Tab 栏的设置页）。

新组件 `desktop/src/components/SettingsDialog.tsx`：

- 全屏半透明遮罩 + 居中面板；面板内左侧垂直 Tab 栏、右侧内容区。
- Tab 栏本次只有一项「通用」；结构上是一个 `[tabId → {label, render}]` 表，
  加 Tab 只加表项。
- 退出三条路径：Esc、点遮罩、关闭按钮。与既有 `ApprovalDialog.tsx` 的
  模态写法（遮罩 + Esc）保持一致。

**「通用」Tab 内容**：主题分段控件（深色 / 浅色），即时生效；下方一行说明，
提示「也可以直接对话让 Yi-Agent 切换主题」。

**状态归属**：模态开关是纯 UI 状态，放在 `App.tsx`（`settingsOpen`），
不持久化——每次启动都从关闭态开始。

### 2. 主题 token 层

现有配色全部是字面色阶，主题切换必须先把它们变成语义 token。

`desktop/src/index.css`：

- 定义语义 CSS 变量，深色一套（= 现状 `neutral-*` 的等价色）、浅色一套：

  | token | 用途 | 深色（现状） |
  |---|---|---|
  | `--surface` | 应用根背景 | `neutral-950` |
  | `--panel` | 侧栏 / 状态栏 / 标题栏 | `neutral-900` |
  | `--raised` | 卡片 / 菜单 / 次级按钮 | `neutral-800` |
  | `--line` | 常规边框 | `neutral-800` |
  | `--line-strong` | 菜单 / 下拉边框 | `neutral-700` |
  | `--fg` | 主文字 | `neutral-100` |
  | `--fg-muted` | 次级文字 | `neutral-400` |
  | `--fg-subtle` | 弱化文字 | `neutral-500` |
  | `--fg-faint` | 最弱文字 / 时间戳 | `neutral-600` |

- 用 `@theme inline` 把变量映射成 Tailwind 颜色工具类，组件改用
  `bg-surface` / `bg-panel` / `text-fg` / `text-fg-muted` / `border-line` 等。
- 切主题 = 在 `<html>` 上设 `data-theme="dark" | "light"`，**CSS 变量整体换值，
  组件零改动**。`:root` 默认深色，`[data-theme="light"]` 覆盖为浅色一套。
- 半透明 hover（如 `hover:bg-neutral-800/50`）需保留 alpha，token 用
  `color-mix` 或直接给带透明度的浅/深值。

替换范围：全部含 `neutral-*` 的组件（`grep -rl neutral- desktop/src --include=*.tsx`
共 20 个文件，其中 2 个是测试，即 18 个源文件）。

**顺带修一处既有缺陷**：`bg-neutral-925` 不是 Tailwind 默认色阶（默认只到
`950`），仓库也未定义 `--color-*`，所以该 class **不产生任何背景色**——两个用了
它的面板（`SuperpowersKanbanCollapsedStrip.tsx:18`、`SubagentRail.tsx:41`）
当前实际是透明背景，直接透出根容器的 `neutral-950`。token 化时把这两处映射到
`--raised`（或专为面板侧条设一个 token），顺带把"没有背景"变成"有背景"。

### 3. 自然语言控制链路

agent 跑在 sidecar 里，拿不到前端句柄；而主题是前端的（React/Tailwind），
app-server 也不能自己往 webview 上刷颜色。因此唯一诚实的链路是：

```
用户说「切成亮色」→ agent 调 set_theme 工具
  → 工具写 preferences.json 并广播 → app-server 推 ui/settings/updated
  → 桌面端 App.tsx 收到通知 → 设 data-theme
```

- **工具**：app-server 在装配 thread 时，往该 thread 的 tool registry 注册
  `set_theme` 工具；注册点沿用现有的「app-server 注册委派工具」方式
  （`server.rs` 的 `attach_delegation` 路径），**不改** core / yi-agent-tools。
  工具参数：`{ theme: "dark" | "light" }`。
- **通知**：仿 `process/updated` 的 watcher 范式——已有一个源（`ProcessManager`
  的 broadcast）在变就由 watcher task 推通知（`server.rs:426` `watch_processes`，
  `protocol.rs` 的 `Notification::ProcessUpdated`）。主题照抄：一个主题 store +
  广播 → 推 `ui/settings/updated { theme }`。
- **前端**：`App.tsx` 的 `onNotification`（现有分支 `agent/children/updated`、
  `agent/trace/event`，`App.tsx:442` 起）新增一个分支。

### 4. 持久化与协议

**持久化**：写到 `<app-server workdir>/.yi-agent/preferences.json` 的 `theme` 键。
该文件已被多方共用，**必须读-改-写并保留无关键**——`runtime_prefs.rs` 的注释明确
写了「该文件与看板插件共享（`superpowers_kanban`）」，并提供了原子落盘
（临时文件 + rename）与保留无关键的实现（`runtime_prefs.rs:68` 起）。因此加
`theme` 键是既有模式，不会踩掉 `subagent_runtime` / `superpowers_kanban`。
缺文件 / 损坏 / 未知值一律回退 `dark`（现状即深色），与 `runtime_prefs` 的
「损坏不阻断启动」策略一致。

**RPC（新增）**：

| 方法 | 参数 | 返回 | 用途 |
|---|---|---|---|
| `ui/settings/read` | `{}` | `{ theme }` | 首屏与打开设置时读当前主题 |
| `ui/settings/write` | `{ theme }` | `{}` | 手动切换时写并广播 |

**通知（新增）**：`ui/settings/updated { theme }`。

手动切换与对话切换写的是同一份数据、推的是同一种通知，两条路径不会打架。

### 5. 首屏防闪烁

`App` 挂载后才拿到主题会先闪一下深色，因此：

- 主题变化时同步写一份 `localStorage` 缓存（`app.theme`）。
- `desktop/index.html` 的 `<head>` 内联一小段脚本，在首帧前读 localStorage
  并设 `data-theme`；脚本极短、无依赖、失败时静默（保持默认深色）。
- 这与现有先例一致：侧栏宽度已用 localStorage 持久化（`sidebarWidth.ts`）。

契约：**`preferences.json` 是权威，localStorage 只是首屏缓存**。`ui/settings/read`
返回后以服务端值为准并回写缓存（处理「另一个入口改了主题」的情况）。

### 6. 浅色下的代码高亮

`MarkdownText.tsx` 用 rehype-highlight 做代码高亮，当前按深色配色选的主题。
浅色下代码块会看不清，需为浅色单独配一套 highlight 样式，并按 `data-theme`
切换（例如两套 CSS 变量或在 `[data-theme="light"]` 下覆写 `.hljs-*`）。
这块算在本次工作量内。

## 验收与测试

**前端单测**（沿用 vitest）：

- 主题 token：默认渲染 `data-theme="dark"`；调用切换后变 `light`，容器类名生效。
- `SettingsDialog`：齿轮打开、Esc / 点遮罩 / 关闭按钮均可关闭；「通用」Tab 渲染
  主题控件；切换会调用 `ui/settings/write`。
- `ThreadSidebar`：左下角设置按钮存在且触发 `onOpenSettings`。
- 通知：收到 `ui/settings/updated` 后 `data-theme` 跟随变化。
- 防闪烁脚本：给定 localStorage 值时，`<head>` 脚本把 `data-theme` 设为该值。

**Rust 单测**：

- `set_theme` 工具写对 `theme` 键，且**保留** `subagent_runtime` /
  `superpowers_kanban`（照 `runtime_prefs.rs` 的 `saving_the_runtime_preference_preserves_unrelated_keys` 写法）。
- `ui/settings/read` 在缺文件 / 损坏 / 未知值时回退 `dark`。
- 工具被调用后，app-server 推出 `ui/settings/updated`。

**命令**：

```
cd desktop && npx vitest run && npx tsc --noEmit && npm run build
cd yi-agent-rs && cargo test -p yi-agent-app-server
```

## 后续（不在本次范围）

- 「通用」以外对齐 web 的 Tab（Model Provider / Agent / Tools），复用
  `yi-agent-web` 的 `config_meta` 与 `.env` 读写；届时需要 app-server 增加配置
  写 RPC，并处理「sidecar 只在启动时读 `.env`、写后需重启」、「桌面端 local 与
  global `.env` 都指向 `$HOME/.yi-agent/.env`」两个已知问题。
- 「跟随系统」主题第三态。
- 看板开关迁移进设置界面。

## 证据索引

- 无 footer / 无设置入口：`desktop/src/components/ThreadSidebar.tsx:364`
- 硬编码配色：`grep -rl neutral- desktop/src --include=*.tsx`（19 文件）
- 无 dark 模式配置：`desktop/src/index.css`（仅 `@import` + typography）
- 共享 preferences 文件与原子写：`yi-agent-rs/crates/yi-agent/src/tui/runtime_prefs.rs:27,68`
- 通知 watcher 范式：`yi-agent-app-server/src/server.rs:426`、`protocol.rs` `Notification::ProcessUpdated`
- 通知回调分支：`desktop/src/App.tsx:442`
- 首屏 localStorage 先例：`desktop/src/lib/sidebarWidth.ts`
- 模态写法先例：`desktop/src/components/ApprovalDialog.tsx`
