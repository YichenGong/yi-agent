# desktop（Tauri GUI）

## 模块说明

`desktop/` 是 yi-agent 的原生桌面应用，基于 **Tauri 2.x**（原生 OS webview）+
**React 19 + TypeScript + Tailwind CSS v4 + Vite** 前端。Rust 后端是一个薄桥接层，
**不含任何 agent 逻辑**：它只负责管理 `yi-agent app-server` sidecar 子进程的生命周期，
并在其 stdio 与前端之间转发 **JSON-RPC 2.0**（JSONL 分帧）帧。前端只依赖线协议
（`desktop/src/lib/protocol.ts`），从不依赖 Rust 类型。

`desktop/` **不是** `yi-agent-rs/` cargo workspace 的成员，独立构建。Rust workspace
不受本模块影响：`cargo test -p yi-agent-app-server`（69 个测试）与
`cargo test -p yi-agent-runtime`（全绿）仍全绿。

## 范围边界

**做什么：**
- 原生窗口（Tauri 2 webview，无浏览器 chrome）
- sidecar 生命周期管理 + stdio JSON-RPC 帧桥接
- 前端 JSON-RPC 客户端（递增 id 关联、乱序响应缓冲、发送失败清理）
- 会话状态机（item upsert、agent 文本 delta 累积、turn 生命周期、token 用量）
- 聊天 UI（消息列表、自动滚动、错误横幅、输入框、工具调用卡片、状态栏）
- 权限审批弹窗（Allow once / Always allow tool / Always allow prefix / Deny）

**不做什么（延后）：**
- 不做会话持久化 / 历史侧栏
- 不做 markdown 富渲染 / 代码高亮
- 不做文件树 / diff 视图
- 不做多标签页
- 不做 MCP / 子 agent 任务树
- 不做 Linux 打包
- 不做会话 item 上限 / 裁剪（长会话内存无界增长，baseline 接受）

## Features

- [x] Tauri 2 + React 19 + TS + Tailwind v4 + Vite 骨架 — `desktop/package.json:17`（`react ^19.1.0`）/ `desktop/package.json:28`（`tailwindcss ^4.3.3`）/ `desktop/package.json:30`（`vite ^8.0.16`）/ `desktop/vite.config.ts:8`（`defineConfig`）/ `desktop/src/main.tsx:6`（`createRoot`）/ `desktop/src/index.css:1`（`@import "tailwindcss";`）；验证 `cd desktop && npm run build`
- [x] sidecar 打包脚本（构建 `yi-agent` CLI 并复制为带 target-triple 后缀的 externalBin）— `desktop/scripts/build-sidecar.sh:12`（release 构建）/ `desktop/scripts/build-sidecar.sh:15`（debug 构建）/ `desktop/scripts/build-sidecar.sh:20`（`cp` 到 `src-tauri/binaries/yi-agent-$TRIPLE`）；npm scripts `sidecar` / `sidecar:release` — `desktop/package.json:11` / `desktop/package.json:12`；`bundle.externalBin` — `desktop/src-tauri/tauri.conf.json:27`
- [x] Rust 桥接：帧分类 + Tauri 命令 + 事件转发 + sidecar 生命周期 — `desktop/src-tauri/src/bridge.rs:20`（`classify`，按 `method`/`id` 有无分四类）/ `desktop/src-tauri/src/bridge.rs:56`（`rpc` 命令）/ `desktop/src-tauri/src/bridge.rs:70`（`rpc_respond` 命令）/ `desktop/src-tauri/src/bridge.rs:82`（`spawn` 拉起 sidecar）/ `desktop/src-tauri/src/bridge.rs:118`（`forward_stdout` 按帧类型发 `app-server://request` / `app-server://message` 事件）；装配 — `desktop/src-tauri/src/lib.rs:4`（`run`）/ `desktop/src-tauri/src/lib.rs:8`（`invoke_handler`）；验证 `cd desktop/src-tauri && cargo test`
- [x] 前端线协议类型（types only，无运行时逻辑）— `desktop/src/lib/protocol.ts:26`（`Item` 联合）/ `desktop/src/lib/protocol.ts:41`（`Notification` 联合）/ `desktop/src/lib/protocol.ts:72`（`Decision` 联合）/ `desktop/src/lib/protocol.ts:59`（`ApprovalRequest`）
- [x] JSON-RPC 客户端（递增 id 关联、乱序响应缓冲、发送失败清理）— `desktop/src/lib/rpc.ts:52`（`request`：`nextId++` + 先查 `buffered` + 注册 `pending`）/ `desktop/src/lib/rpc.ts:81`（`onMessage` 分发响应 / 通知）/ `desktop/src/lib/rpc.ts:32`（`buffered` map）/ `desktop/src/lib/rpc.ts:69`（send 失败删 `pending` 防泄漏）
- [x] 会话状态机（item upsert、agent 文本 delta 累积、turn 生命周期、token 用量）— `desktop/src/lib/session.ts:25`（`apply` 按 `notification.method` 折叠状态）/ `desktop/src/lib/session.ts:35`（`item/delta` 分支：就地累积 agent 文本）/ `desktop/src/lib/session.ts:57`（`thread/tokenUsage/updated` → `usage`）
- [x] Tauri transport（`invoke` / `listen` 适配 `Transport` 接口，监听 `app-server://message|request|status`）— `desktop/src/tauriTransport.ts:31`（`tauriTransport` 工厂）/ `desktop/src/tauriTransport.ts:34`（`invoke("rpc", …)`）/ `desktop/src/tauriTransport.ts:37`（`invoke("rpc_respond", …)`）/ `desktop/src/tauriTransport.ts:39` / `desktop/src/tauriTransport.ts:40` / `desktop/src/tauriTransport.ts:41`（三个 `listen` 订阅，含 `disposed` 防早退订阅泄漏 — `desktop/src/tauriTransport.ts:12`）
- [x] 聊天 UI（消息列表 + 自动滚动 + 错误横幅、输入框 Send/Stop + Enter/Shift+Enter、工具调用卡片、状态栏）— `desktop/src/components/ChatView.tsx:16`（自动滚动 `useEffect`）/ `desktop/src/components/ChatView.tsx:56`（错误横幅）/ `desktop/src/components/ChatView.tsx:43`（工具卡片分派）；`desktop/src/components/MessageInput.tsx:34`（Enter 发送 / Shift+Enter 换行）/ `desktop/src/components/MessageInput.tsx:55`（`Stop`/`Send` 标签）/ `desktop/src/components/MessageInput.tsx:47`（按钮点击按 `turnActive` 分流发送或中断）；`desktop/src/components/ToolCallCard.tsx:12`（可折叠卡片）/ `desktop/src/components/ToolCallCard.tsx:6`（`statusStyles` 三态配色）；`desktop/src/components/StatusBar.tsx:1`（状态栏）/ `desktop/src/components/StatusBar.tsx:22`（token 用量显示）；`turnActive` 时 Enter 触发中断而非发送
- [x] 权限审批弹窗（Allow once / Always allow tool / Always allow prefix / Deny；Esc=Deny；`onDecide` 只触发一次；`inert` 隔离背景）— `desktop/src/components/ApprovalDialog.tsx:24`（`submittedRef` 守卫，同 tick 双决策只放行一次）/ `desktop/src/components/ApprovalDialog.tsx:38`（Esc → `deny`）/ `desktop/src/components/ApprovalDialog.tsx:87`（Deny）/ `desktop/src/components/ApprovalDialog.tsx:95`（Allow once）/ `desktop/src/components/ApprovalDialog.tsx:103`（Always allow tool）/ `desktop/src/components/ApprovalDialog.tsx:113`（Always allow prefix，仅 `prefix_suggestion !== null` 时渲染）；背景隔离 `inert` — `desktop/src/App.tsx:88`
- [x] macOS 打包产出可启动的 `.app` / `.dmg` 并内嵌 sidecar — 判据：`cd desktop && npm run sidecar:release && npm run tauri build` 产出 `desktop/src-tauri/target/release/bundle/macos/yi-agent.app`（内含 `Contents/MacOS/desktop` 启动器 + `Contents/MacOS/yi-agent` sidecar）与 `desktop/src-tauri/target/release/bundle/dmg/yi-agent_0.1.0_aarch64.dmg`；`open yi-agent.app` 后 `ps` 可见 `desktop` 与子进程 `yi-agent app-server --listen stdio://`
- [ ] 手动端到端冒烟（原生窗口 / 流式文本 / 工具卡片 / 审批弹窗放行与拒绝 / Stop 中断 / 杀 sidecar 不崩）— 判据：`cd desktop && npm run sidecar && npm run tauri dev` 后人工逐项确认（设计文档 §12 成功判据，`docs/superpowers/plans/2026-09-26-desktop-gui-design.md:354`）；自动化可覆盖部分已验：应用可启动、sidecar 子进程被拉起

**验证命令：** `cd desktop && npx vitest run`（17 个前端单测，`desktop/src/lib/rpc.test.ts` 7 个 + `desktop/src/lib/session.test.ts` 10 个）+ `cd desktop && npm run build` + `cd desktop/src-tauri && cargo test` + `cd desktop && npm run sidecar:release && npm run tauri build`（产出 `.app` / `.dmg`）
