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

## 3. Superpowers 看板卡片依赖关系与 DAG 可视化

**状态：暂不开发（2026-10-18 记录）。需求留档，开工前先回答下面的"待决策"。**

一句话：让看板卡片能声明"卡 B 依赖卡 A"，**A 合并完成之前 B 不许开始开发**；
并在桌面端把卡片之间的依赖画成有向无环图（DAG）。

### 为什么现在提

现在的调度只看队列顺序（`order`），不看卡片之间的真实先后关系。两件真实存在的事
说明这个缺口不是假想的：

- plan 正文里已经在**手写**"前置"约定：`docs/superpowers/plans/2026-10-17-plugin-lifecycle-decoupling.md:12`
  写着「**前置：** G1（`2026-10-17-session-driver-decoupling.md`）**先落地**——G1 已删
  `card_scheduler` 与 `board_query`，本 plan 再删 `yi-agent-boards` 与 `board/*` RPC，
  避免两处并行改同一批文件」。这条依赖目前**只靠人记得住**，看板一无所知。
- 若两张这样的卡同时在队里，调度器会把它们（在槽位允许时）一起启动，两处并行改同一批文件，
  正是上面那句注释想避免的事故。

### 已确认的现状线索（2026-10-18 摸代码）

- **卡片模型里没有依赖字段**：`Card` 有 `kind`（implementation/merge）、`order`、
  `origin_card`（仅自动派生合并卡时指回实现卡），但没有"我依赖谁" ——
  `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs:113-155`。
- **调度器只看 `order`，不看依赖**：`Board::next_startable` / `claim_next_launch` 只筛
  `Queued && Implementation`，取 `order` 最小者 ——
  `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/board.rs:186-221`。
  调用点：`.../superpowers-kanban-runner/src/service.rs:172`（会话通路）、`:409`（合并通路）。
- **状态机已经区分"开发"与"合并"**：`CardState` 有 `Running`（占会话槽）与
  `Merging`（不占会话槽），终态含 `Done` —— `card.rs:32-47`、`card.rs:49-111`。
  "A 合并完成才放 B" 的判据可以落在 A 的 `Done` 上，语义已具备。
- **前端是四列看板、没有图概念**：`boardColumns` 把状态映射到
  queued/doing/needDecision/done 四列（`desktop/src/lib/superpowersKanbanBoard.ts:22-42`），
  渲染在 `desktop/src/components/SuperpowersKanbanBoard.tsx` / `SuperpowersKanbanColumn.tsx`，
  卡片形状见 `desktop/src/lib/superpowersKanbanState.ts`（`BoardCard`）。
- **`board.json` 是控制面与插件之间的事实契约（文件即契约）** ——
  `board.rs:28-36` 的注释；新增依赖边要经这里落盘、再被前端读到。

### 待决策（真正开工前要逐条回答，brainstorm 的落点）

1. **依赖从哪里来？**
   - A. 显式 CLI 参数：`add <spec> <plan> --depends-on <card-id>`（可多次），依赖成为
     卡片一等字段。意图明确、可校验无环，但要求入队时手填。
   - B. 从 plan 正文解析「前置：…」段落自动抽边。零额外输入，但解析散文不可靠，
     漏掉依赖 = 后卡在前卡没合并时开跑（正是要防的事故）。
   - C. A + B 组合：显式参数为主，正文解析出的边作为建议打印出来让人确认/否决。
2. **依赖的判据是"A 卡片 done"还是"A 已合并进 main"？** 前者简单（读 A 的状态即可），
   后者才是需求原话（"前一个要合并完了"）。注意卡片 `Done` 与"真的合并进主线"在现有
   语义下并不等价（合并卡与实现卡是两类卡片，`card.rs:22-30`）。
3. **环怎么办？** 入队时拒绝（保持 DAG）还是允许并检测？失败/取消的上游如何处理
   下游（永久阻塞 / 自动取消 / 提醒人）？
4. **可视化形态？** 复用四列之上叠加依赖线、还是另开一个"图"标签页画 DAG
   （布局算法、缩放/平移、悬停高亮上下游）？注意现有看板是纯四列 + 卡片，没有 SVG/图库。
5. **与既有并发控制的关系？** 依赖阻塞的卡是否要显式占用一个"被阻塞"状态（新增状态
   会影响 `CardState::can_transition_to` 的迁移表和一堆测试），还是仅靠调度器过滤
   （不新增状态，最小改动）？

### 开工时的第一步

先 brainstorm 定 Q1/Q2（依赖来源与判据是地基），再出 spec → writing-plans。
最小可用切片建议：显式 `--depends-on` + 调度器过滤 + `board.json` 落边，先不做可视化。
