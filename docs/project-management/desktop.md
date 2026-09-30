# desktop（Tauri GUI）

## 模块说明

`desktop/` 是 yi-agent 的原生桌面应用，基于 **Tauri 2.x**（原生 OS webview）+
**React 19 + TypeScript + Tailwind CSS v4 + Vite** 前端。Rust 后端是一个薄桥接层，
**不含任何 agent 逻辑**：它只负责管理 `yi-agent app-server` sidecar 子进程的生命周期，
并在其 stdio 与前端之间转发 **JSON-RPC 2.0**（JSONL 分帧）帧。前端只依赖线协议
（`desktop/src/lib/protocol.ts`），从不依赖 Rust 类型。

`desktop/` **不是** `yi-agent-rs/` cargo workspace 的成员，独立构建。Rust workspace
不受本模块影响：`cargo test -p yi-agent-app-server`（148 个测试）与
`cargo test -p yi-agent-runtime`（全绿）仍全绿。

## 范围边界

**做什么：**
- 原生窗口（Tauri 2 webview，无浏览器 chrome）
- sidecar 生命周期管理 + stdio JSON-RPC 帧桥接
- 前端 JSON-RPC 客户端（递增 id 关联、乱序响应缓冲、发送失败清理）
- 会话状态机（item upsert、agent 文本 delta 累积、turn 生命周期、token 用量）
- 聊天 UI（消息列表、自动滚动、错误横幅、输入框、工具调用卡片、状态栏）
- 权限审批弹窗（Allow once / Always allow tool / Always allow prefix / Deny）
- 历史侧栏（列表 / 恢复继续对话 / 双击内联重命名 / 删除 / New thread）
- agentMessage markdown 富渲染 + 代码高亮（react-markdown + remark-gfm + rehype-highlight）
- 用量/成本面板（StatusBar 可展开：input / output / cache read / cache write + 估算成本）

**不做什么（非目标）：**
- 会话 item 上限 / 裁剪（长会话内存无界增长，baseline 接受）
- markdown 中的 LaTeX / 数学公式渲染

**后续路线：** 计划中但尚未交付的能力（reasoning 展示、P1 剩余、P2/P3 路线图、
打包交付缺口）逐一登记在下方 Features 的 `[ ]` 条目；路线图出处为设计文档 §11
（`docs/superpowers/plans/2026-09-26-desktop-gui-design.md:344`）。

## Features

- [x] Tauri 2 + React 19 + TS + Tailwind v4 + Vite 骨架 — `desktop/package.json:19`（`react ^19.1.0`）/ `desktop/package.json:36`（`tailwindcss ^4.3.3`）/ `desktop/package.json:38`（`vite ^8.0.16`）/ `desktop/vite.config.ts:8`（`defineConfig`）/ `desktop/src/main.tsx:6`（`createRoot`）/ `desktop/src/index.css:1`（`@import "tailwindcss";`）；验证 `cd desktop && npm run build`
- [x] sidecar 打包脚本（构建 `yi-agent` CLI 并复制为带 target-triple 后缀的 externalBin）— `desktop/scripts/build-sidecar.sh:12`（release 构建）/ `desktop/scripts/build-sidecar.sh:15`（debug 构建）/ `desktop/scripts/build-sidecar.sh:20`（`cp` 到 `src-tauri/binaries/yi-agent-$TRIPLE`）；npm scripts `sidecar` / `sidecar:release` — `desktop/package.json:11` / `desktop/package.json:12`；`bundle.externalBin` — `desktop/src-tauri/tauri.conf.json:27`
- [x] Rust 桥接：帧分类 + Tauri 命令 + 事件转发 + sidecar 生命周期 — `desktop/src-tauri/src/bridge.rs:20`（`classify`，按 `method`/`id` 有无分四类）/ `desktop/src-tauri/src/bridge.rs:56`（`rpc` 命令）/ `desktop/src-tauri/src/bridge.rs:70`（`rpc_respond` 命令）/ `desktop/src-tauri/src/bridge.rs:82`（`spawn` 拉起 sidecar）/ `desktop/src-tauri/src/bridge.rs:118`（`forward_stdout` 按帧类型发 `app-server://request` / `app-server://message` 事件）；装配 — `desktop/src-tauri/src/lib.rs:4`（`run`）/ `desktop/src-tauri/src/lib.rs:8`（`invoke_handler`）；验证 `cd desktop/src-tauri && cargo test`
- [x] 前端线协议类型（types only，无运行时逻辑）— `desktop/src/lib/protocol.ts:26`（`Item` 联合）/ `desktop/src/lib/protocol.ts:41`（`Notification` 联合）/ `desktop/src/lib/protocol.ts:72`（`Decision` 联合）/ `desktop/src/lib/protocol.ts:59`（`ApprovalRequest`）
- [x] JSON-RPC 客户端（递增 id 关联、乱序响应缓冲、发送失败清理）— `desktop/src/lib/rpc.ts:52`（`request`：`nextId++` + 先查 `buffered` + 注册 `pending`）/ `desktop/src/lib/rpc.ts:81`（`onMessage` 分发响应 / 通知）/ `desktop/src/lib/rpc.ts:32`（`buffered` map）/ `desktop/src/lib/rpc.ts:69`（send 失败删 `pending` 防泄漏）
- [x] 会话状态机（item upsert、agent 文本 delta 累积、turn 生命周期、token 用量）— `desktop/src/lib/session.ts:41`（`apply` 按 `notification.method` 折叠状态）/ `desktop/src/lib/session.ts:51`（`item/delta` 分支：就地累积 agent 文本）/ `desktop/src/lib/session.ts:73`（`thread/tokenUsage/updated` → `usage`）
- [x] Tauri transport（`invoke` / `listen` 适配 `Transport` 接口，监听 `app-server://message|request|status`）— `desktop/src/tauriTransport.ts:31`（`tauriTransport` 工厂）/ `desktop/src/tauriTransport.ts:34`（`invoke("rpc", …)`）/ `desktop/src/tauriTransport.ts:37`（`invoke("rpc_respond", …)`）/ `desktop/src/tauriTransport.ts:39` / `desktop/src/tauriTransport.ts:40` / `desktop/src/tauriTransport.ts:41`（三个 `listen` 订阅，含 `disposed` 防早退订阅泄漏 — `desktop/src/tauriTransport.ts:12`）
- [x] 聊天 UI（消息列表 + 自动滚动 + 错误横幅、输入框 Send/Stop + Enter/Shift+Enter、工具调用卡片、状态栏）— `desktop/src/components/ChatView.tsx:25`（自动滚动 `useEffect`）/ `desktop/src/components/ChatView.tsx:70`（错误横幅）/ `desktop/src/components/ChatView.tsx:48`（工具卡片分派）；`desktop/src/components/MessageInput.tsx:34`（Enter 发送 / Shift+Enter 换行）/ `desktop/src/components/MessageInput.tsx:55`（`Stop`/`Send` 标签）/ `desktop/src/components/MessageInput.tsx:47`（按钮点击按 `turnActive` 分流发送或中断）；`desktop/src/components/ToolCallCard.tsx:12`（可折叠卡片）/ `desktop/src/components/ToolCallCard.tsx:6`（`statusStyles` 三态配色）；`desktop/src/components/StatusBar.tsx:1`（状态栏）/ `desktop/src/components/StatusBar.tsx:28`（用量按钮，点击展开 UsagePanel）；`turnActive` 时 Enter 触发中断而非发送
- [x] 权限审批弹窗（Allow once / Always allow tool / Always allow prefix / Deny；Esc=Deny；`onDecide` 只触发一次；遮罩只罩对话列、侧栏保持可点）— `desktop/src/components/ApprovalDialog.tsx:24`（`submittedRef` 守卫，同 tick 双决策只放行一次）/ `desktop/src/components/ApprovalDialog.tsx:38`（Esc → `deny`）/ `desktop/src/components/ApprovalDialog.tsx:89`（Deny）/ `desktop/src/components/ApprovalDialog.tsx:97`（Allow once）/ `desktop/src/components/ApprovalDialog.tsx:105`（Always allow tool）/ `desktop/src/components/ApprovalDialog.tsx:116`（Always allow prefix，仅 `prefix_suggestion !== null` 时渲染）；遮罩 `absolute inset-0` 定位到对话列（不再全局 `fixed`），列容器 `relative` — `desktop/src/components/ApprovalDialog.tsx:50` / `desktop/src/App.tsx:368`
- [x] macOS 打包产出可启动的 `.app` / `.dmg` 并内嵌 sidecar — 判据：`cd desktop && npm run sidecar:release && npm run tauri build` 产出 `desktop/src-tauri/target/release/bundle/macos/yi-agent.app`（内含 `Contents/MacOS/desktop` 启动器 + `Contents/MacOS/yi-agent` sidecar）与 `desktop/src-tauri/target/release/bundle/dmg/yi-agent_0.1.0_aarch64.dmg`；`open yi-agent.app` 后 `ps` 可见 `desktop` 与子进程 `yi-agent app-server --listen stdio://`
- [x] 会话持久化 + 历史侧栏（列表 / 恢复并继续对话 / 双击内联重命名 / 删除 / New thread / 当前高亮）— `desktop/src/lib/session.ts:30`（`reset`）/ `desktop/src/components/ThreadSidebar.tsx:18` / `desktop/src/App.tsx:60`（`refreshThreads`）/ `desktop/src/App.tsx:86`（`resumeThread` 前置同步 `reset` 防回放竞态）
- [x] agentMessage markdown 富渲染 + 代码高亮 + 用量/成本面板 — `desktop/src/components/MarkdownText.tsx:39`（memo 按 text 记忆 + remark-gfm + rehype-highlight + 外链走 opener）/ `desktop/src/components/ChatView.tsx:42`（接线）/ `desktop/src/lib/pricing.ts:20`（`PRICES` 定价表）/ `desktop/src/lib/pricing.ts:35`（`priceFor`）/ `desktop/src/lib/pricing.ts:46`（`estimateCost`）/ `desktop/src/components/UsagePanel.tsx:5` / `desktop/src/components/StatusBar.tsx:28`（点击展开）
- [x] 工作目录选择（原生文件夹选择器 + 最近目录下拉 + 侧栏按目录分组可折叠）— `desktop/src/App.tsx:118`（`newThread(cwd?)`）/ `desktop/src/App.tsx:60`（`refreshThreads` 走 `thread/listAll`）/ `desktop/src/App.tsx:75`（`refreshWorkspaces` 走 `workspace/list`）/ `desktop/src/App.tsx:147`（`pickDirectory` 走 `@tauri-apps/plugin-dialog`）/ `desktop/src/App.tsx:165`（`onBrowse`）/ `desktop/src/App.tsx:153`（`addWorkspace`）/ `desktop/src/App.tsx:191`（`removeWorkspace`）；侧栏分组 + 折叠 + 组头右键/键盘菜单 — `desktop/src/components/ThreadSidebar.tsx:18`；协议类型 `desktop/src/lib/protocol.ts:119` / `desktop/src/lib/protocol.ts:125`；纯函数 `desktop/src/lib/workspaceGroups.ts:3` / `desktop/src/lib/workspaceGroups.ts:7`；请求参数 `desktop/src/lib/threadStart.ts:2`；dialog 权限 `desktop/src-tauri/capabilities/default.json`（`dialog:allow-open`）；验证 `cd desktop && npx tsc --noEmit && npm test`
- [x] 活跃轮次下的追加输入（`turn/interject`）— `desktop/src/App.tsx::send` 按 thread 的服务端权威状态（`threadStore` 的 `status`）选方法：`running` 时走 `turn/interject`，否则仍走 `turn/start`；`thread/status/updated` 落在 `turn/start` 与点击之间导致方法选错时，拒绝会把乐观气泡回滚（与既有 `-32012` 回滚同一路径）；`user_interjection` item 在 `desktop/src/lib/session.ts::apply` 归一为用户气泡（同一 item 的 `started`/`completed` 由共享的按 id 合并逻辑收敛为一条），`turn/interjectionsReturned` 记入 `Session.returnedInterjections`，服务端保证它排在 `turn/completed` 之前；协议类型见 `desktop/src/lib/protocol.ts`；验证：`cd desktop && npm test`（`src/lib/session.test.ts` 的 interjection 用例 + `src/App.test.tsx` 的 `turn/interject` 用例）— [设计](../superpowers/specs/2026-09-30-mid-turn-user-interjection-design.md)
- [x] 输入框权限模式 chip（按线程 Normal / YOLO，启用 YOLO 前确认，模式未知时禁用）— `desktop/src/components/ModeChip.tsx:16`（灰 Normal / 红 YOLO；`role="menu"` 下拉 + 启用 YOLO 弹 `role="dialog"` 确认框 `:159`，含「跳过审批 / 沙箱放开完全访问 / 黑名单仍拒」）；接入 `desktop/src/components/MessageInput.tsx`（`mode` / `onModeChange`，`disabled={mode === null}`）；模式按线程存储于 `ThreadView.mode`（`desktop/src/lib/threadStore.ts:14`，`null` = 尚未解析；`seed()` 不写、仅 resume/start/显式切换时回读写入），切换 thread 时 chip 自动反映各自模式（warm 切换无需重新 resume）— `desktop/src/App.tsx:120` / `desktop/src/App.tsx:150` / `desktop/src/App.tsx:257`；`desktop/src/App.tsx:35`（`modeForThread` 从 `thread/listAll` 回读，线程缺失→unknown，绝不回退 `normal`）/ `desktop/src/App.tsx:392`（`mode={current?.mode ?? null}`）/ `desktop/src/App.tsx:251`（`setThreadMode` RPC 成功才更新本地状态）；协议类型 `desktop/src/lib/protocol.ts:110`（`permission_mode?`）+ helper `desktop/src/lib/threadPermissionMode.ts:2`；验证：`cd desktop && npx vitest run src/components/ModeChip.test.tsx src/components/MessageInput.test.tsx src/App.test.tsx`
- [x] 可拖拽侧边栏宽度（拖动右侧分隔条调宽、夹到 [200, 480]、`localStorage` 持久化、重启恢复、漏 mouseup/窗口失焦不残留监听）— `desktop/src/lib/sidebarWidth.ts:11`（`clampSidebarWidth`）/ `desktop/src/lib/sidebarWidth.ts:17`（`loadSidebarWidth`）/ `desktop/src/lib/sidebarWidth.ts:29`（`saveSidebarWidth`）/ `desktop/src/components/ThreadSidebar.tsx:57`（`onHandleDown`）/ `desktop/src/components/ThreadSidebar.tsx:200`（`style={{ width }}`）/ `desktop/src/components/ThreadSidebar.tsx:354`（`role="separator"` 拖拽手柄）；显示名统一为 `Yi-Agent` — `desktop/index.html:7` / `desktop/src-tauri/tauri.conf.json:15`；验证 `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
- [x] 每 thread 状态徽标 + 未读注意力点 + 逐 thread 审批横幅 — `desktop/src/lib/threadStore.ts:6`（`ThreadView` = session / status / unread / approval / info / mode；按 `thread_id` 路由通知，离开 `awaiting_approval` 清审批 `desktop/src/lib/threadStore.ts:88`，非当前 thread 的 `turn/completed` 置 unread `desktop/src/lib/threadStore.ts:100`）/ `desktop/src/components/ApprovalBanner.tsx:10`（顶部可关闭横幅 + 逐 thread「Jump · <tool>」按钮）；判据：`desktop/src/components/ThreadSidebar.tsx:180` 渲染 `aria-label="Running"`、`desktop/src/components/ThreadSidebar.tsx:187` 渲染 `aria-label="Awaiting approval"`、`desktop/src/components/ThreadSidebar.tsx:206` 渲染 `aria-label="Unread"`；验证 `cd desktop && npx vitest run src/lib/threadStore.test.ts src/components/ApprovalBanner.test.tsx src/components/ThreadSidebar.test.tsx`

**打包交付缺口：**

- [ ] macOS 代码签名 + 公证（notarization）— 当前 `.app` / `.dmg` 未签名（`desktop/src-tauri/tauri.conf.json` 无 `bundle.macOS.signingIdentity`），他人机器双击会被 Gatekeeper 拦截；判据：`codesign --verify --deep --strict yi-agent.app` 与 `spctl -a -vv yi-agent.app` 通过，且 `xcrun notarytool submit` 返回 `Accepted`
- [ ] Universal（arm64 + x86_64）打包 — 当前仅产出 `aarch64-apple-darwin` 单架构；判据：`lipo -info yi-agent.app/Contents/MacOS/yi-agent` 同时列出 `arm64` 与 `x86_64`（或分别为两架构产出 bundle）
- [ ] 手动端到端冒烟（原生窗口 / 流式文本 / 工具卡片 / 审批弹窗放行与拒绝 / Stop 中断 / 杀 sidecar 不崩）— 判据：`cd desktop && npm run sidecar && npm run tauri dev` 后人工逐项确认（设计文档 §12 成功判据，`docs/superpowers/plans/2026-09-26-desktop-gui-design.md:354`）；自动化可覆盖部分已验：应用可启动、sidecar 子进程被拉起

**P1 剩余：**

- [ ] reasoning / thinking 展示 — core `ProviderEvent`（`yi-agent-rs/crates/yi-agent-core/src/provider.rs:43`）与 `AgentEvent`（`yi-agent-rs/crates/yi-agent-core/src/agent.rs:198`）当前无 thinking 变体，provider 未解析 thinking block；需 core + provider + protocol + translate + 前端逐层加支持；判据：发一条触发 extended thinking 的 prompt，GUI 显示 reasoning 内容

**P2 路线图：**

- [ ] 文件树 — 判据：侧栏展示当前 workdir 文件树
- [ ] 启动时用 `cfg.workdir` 播种「最近目录」索引（解决升级安装旧对话不显示）— 判据：`$HOME/.yi-agent/threads/` 已有对话但索引为空时，启动后 `thread/listAll` 仍把 `$HOME` 分组列出
- [ ] diff 视图 — 需先在协议层新增 `turn/diff/updated` 通知（当前不存在，`grep -rn "turn/diff" yi-agent-rs/crates/yi-agent-app-server/src/` 无结果）；判据：agent 改文件后 GUI 逐 turn 显示 diff
- [ ] compact 手动控制 — 判据：GUI 提供手动触发上下文压缩的入口
- [ ] 图片附件输入 — 判据：输入框可附加图片并随 prompt 发送
- [x] 多 thread 标签页 — 可同时运行多个 thread 并在 Sidebar 间自由切换（切换已打开的 warm thread 只换视图、不再 `thread/resume`，`desktop/src/App.tsx:103` `selectThread` + `warm` 集合 `desktop/src/App.tsx:50`；全局 `busy` 门禁已移除，`turnActive` 期间仍可切换/删除）；判据：起两个 turn 后两 thread 状态徽标同时为 running。

**P3 路线图：**

- [ ] MCP 集成 — 后端 `yi-agent-mcp` 已完成（配置/懒连接/调用/`/mcp` 开关，见 [yi-agent-mcp](./yi-agent-mcp.md)）；判据：GUI 可查看 / 启停 MCP server
- [ ] 子 agent 任务树可视化 — 复用现有 daemon IPC（见 [subagent-runtime](./subagent-runtime.md)）；判据：GUI 展示子 agent 任务树
- [ ] Unix socket / websocket 传输 — 当前仅 stdio（其它一律拒绝，`yi-agent-rs/crates/yi-agent/src/main.rs:74`）；判据：`--listen` 支持 socket/ws 且 GUI 可连
- [ ] 多窗口共享 daemon — 判据：多个 GUI 窗口连同一 daemon
- [ ] Linux 打包验证 — 判据：产出并启动 Linux 包

**已知限制：** 「最近目录」索引只由用户在 UI 里打开/添加的目录产生。升级安装（`$HOME/.yi-agent/threads/` 已有历史对话）时索引为空，`thread/listAll` 只遍历索引，故侧栏初始可能看不到旧的 `$HOME` 对话；需在 UI 里打开 `$HOME`（或对应目录）才会重新出现。`thread/start` 缺省 cwd 仍回退 `cfg.workdir`（`$HOME`），未种子化索引；修复（启动时用 `cfg.workdir` 播种索引）见 P2 `[ ]` 条目。

**验证命令：** `cd desktop && npx vitest run`（125 个前端单测：`desktop/src/lib/rpc.test.ts` 7 + `desktop/src/lib/session.test.ts` 15 + `desktop/src/lib/pricing.test.ts` 7 + `desktop/src/lib/protocol.test.ts` 2 + `desktop/src/lib/workspaceGroups.test.ts` 4 + `desktop/src/lib/threadStart.test.ts` 2 + `desktop/src/lib/threadPermissionMode.test.ts` 2 + `desktop/src/lib/sidebarWidth.test.ts` 9 + `desktop/src/lib/threadStore.test.ts` 8 + `desktop/src/components/MarkdownText.test.tsx` 5 + `desktop/src/components/ThreadSidebar.test.tsx` 27 + `desktop/src/components/MessageInput.test.tsx` 5 + `desktop/src/components/ApprovalBanner.test.tsx` 4 + `desktop/src/components/ModeChip.test.tsx` 17 + `desktop/src/App.test.tsx` 11）+ `cd desktop && npx tsc --noEmit` + `cd desktop && npm run build` + `cd desktop/src-tauri && cargo test` + `cd desktop && npm run sidecar:release && npm run tauri build`（产出 `.app` / `.dmg`）
