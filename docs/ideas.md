# 想法暂存

这里放"想做但暂不开发"的点子。只记想法与已知线索，不是承诺，也不是路线图
（已排期的能力登记在 `docs/project-management/`）。

---

## 1. super many subagents parallel run

大量 subagent 并行跑。

## 2. macOS 桌面的交互方案迁移到 Web

**状态：暂不开发（2026-10-17 记录）。需求留档，后续可能有用。**

一句话：把现在 Tauri 桌面 App 的交互界面搬到浏览器里。

### 落点待定（真正开工前要先回答）

- **A. 当远端控制台** —— 浏览器连自己 Mac 上正在跑的会话，形态对齐现有 iOS 端（推荐）
- **B. 部署成公网服务给多人用** —— 要先定认证与多租户边界，工作量上一个量级
- **C. 用浏览器替代本地 App** —— 本机 GUI 跑在浏览器里，仍只服务本机（需本机起
  `yi-agent app-server --listen ws://127.0.0.1:PORT`，不是"迁移"而是换安装方式）

### 已确认的可行性线索（2026-10-17 调研）

架构本来就是分层的，两件最容易卡住的事（前端宿主、传输）已完成大半：

- agent 逻辑一行都不在前端；Tauri 后端只做三件事（sidecar 生命周期、转发 stdio 帧、
  帧分类）—— `desktop/src-tauri/src/bridge.rs`
- app-server 传输无关，`serve()` 对 `AsyncRead`/`AsyncWrite` 泛型；协议是 JSON-RPC 2.0 /
  JSONL，`thread` / `turn` / `item` 三原语
- 前端只依赖线协议（`desktop/src/lib/protocol.ts` 是 types-only），不引用 Rust 类型
- **纯浏览器 Transport 已存在**：`desktop/src/wsTransport.ts`（注释明确"不假设 Tauri"）、
  `desktop/src/transportFactory.ts`（有持久化远端配置走 ws，否则走 tauri）
- **平台分支已广泛生效**：`desktop/src/lib/platform.ts` 的 `isRemoteClient()` 在
  `App.tsx` 有 20+ 处（文件对话框 no-op、外链走 `window.open`、本地缓存 gate 在它上面）；
  手机布局 `desktop/src/lib/useIsMobile.ts` 已存在
- **首次唤醒流程已存在**：`desktop/src/components/PairingScreen.tsx`——浏览器与 WKWebView
  是同类环境（都无法设握手头，token 走 `?token=`）
- 后端已就绪：`--listen ws://`、`Broadcaster` 多客户端扇出、投递分层、自建 WSS 中继
  （`yi-agent-rs/crates/yi-agent-relay/`）、配对与设备撤销
- 见 `docs/relay-deploy.md`（头部状态：配对端到端已打通、真中继下端到端跑通，
  未实跑的只有物理 iPhone 那一腿）

### 真正的新增工作（估计规模，均不大）

1. **静态托管（10 分钟级）** —— `npm run build` 产物可直接静态托管，但
   `desktop/dist/index.html` 引用的是绝对路径 `/assets/...`；挂子路径需在
   `vite.config.ts` 加 `base: "./"`，或托管在域名根
2. **web 没有 sidecar，默认 fallback 会踩空（半天级）** —— `transportFactory` 在无远端
   配置时退回 `tauriTransport()`，浏览器上没有 Tauri IPC，不配对即死路；解法已有先例：
   `App.tsx` 用 `needsPairing(isIos())` 门禁主界面，web 上放宽为"非桌面环境 + 未配对"
3. **文档附件缺上传通道（1-2 天级，唯一真缺的功能）** —— 浏览器没有文件系统路径，而
   文档走 `{type:"attachment", path}`（服务端按路径读）；图片已有
   `image/upload/{begin,chunk,commit,abort}` 分片上传（iOS 在用，`image_upload.rs`），
   **文档需要仿照加一条上传 RPC**。选目录（cwd）同理，但可先用"最近目录"下拉绕开
4. **安全模型变了（需先拍板，不是技术难点）** —— 桌面是 `local`/`Admin`（可
   `pair/create`、`device/revoke`），远程是 `Control`（admin RPC 被拒）；中继按 session
   配对转发、**不鉴权**（`session_id` 本身就是凭据）。托管位置决定了要不要认证/TLS/scope

### 开工时的第一步

选 A 的话，直接出 spec（复用 iOS 客户端路径 + 补文档上传通道）然后进 writing-plans；
选 B 的话，先 brainstorm 认证 / 多租户 / 沙箱边界。