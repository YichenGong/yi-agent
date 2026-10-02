# 手机 App 远程查看与控制电脑 session 调研

> 调研目标：新开发一个手机 App，能实时看到电脑上的 session，并对电脑上的环境做互动与管理；
> 电脑端与手机端看到的是**同一个** session；且要考虑手机在外网、电脑在内网（无公网 IP）时的打通方式。
> 日期：2026-10-02

## 目录

1. [结论摘要（TL;DR）](#1-结论摘要tldr)
2. [现状盘点：yi-agent 已经具备的地基](#2-现状盘点yi-agent-已经具备的地基)
3. [同类产品怎么做（Claude Code / Codex / OpenClaw）](#3-同类产品怎么做)
4. [目标能力分解](#4-目标能力分解)
5. [五个关键架构决策](#5-五个关键架构决策)
6. [推荐方案](#6-推荐方案)
7. [分阶段实施路线图](#7-分阶段实施路线图)
8. [风险与缓解](#8-风险与缓解)
9. [待决策问题](#9-待决策问题)
10. [参考资料](#10-参考资料)

---

## 1. 结论摘要（TL;DR）

这件事**不需要重写任何 agent 逻辑**。yi-agent 的 `app-server` 已经把「桌面 GUI 的后端」
设计成 codex 风格的 app-server（`thread` / `turn` / `item` 三原语 + JSON-RPC 2.0 + 服务端反向请求），
手机 App 本质上是**第三个前端**，和桌面端平级。

三条核心结论：

1. **手机 App = 现有 app-server 的第二个客户端**，复用同一套线协议与 thread 持久化。
   真正的服务端改造只有一处：把「单客户端 writer + 单审批登记表」改成**多客户端扇出（fan-out）**
   并定义审批的归属规则（见 §5.3）。这在一台电脑上同时开着桌面端和手机时是必须的。

2. **内网打通用「纯出站 + 中继」**，不要指望端口映射。三款同类产品（Claude Code Remote Control、
   Codex 手机端）清一色采用：本地只发出站连接，厂商/自建中继路由消息，扫码或多设备登录配对。
   对个人开发者，**最小成本是 VPN overlay（Tailscale）+ 反向 WebSocket 直连**；**最省事是自建/复用 FRP
   或 Cloudflare Tunnel**；**与项目已有 IM 集成设想最契合的是「飞书机器人当 transport」**（见 §5.2、§6）。

3. **安全边界要早定**：手机看到和终端里"完全一样"意味着远端可批准危险命令。必须叠加
   「短期作用域凭证 + 设备配对 + 审批归属 + 可选只读模式」，不能把 app-server 裸暴露到公网。

一个务实的推荐路径：**先做 Tier 1（PWA/移动 Web + VPN overlay 直连，纯出站反向连接）**，
零应用商店成本即可验证体验；体验跑通后再决定是否投入原生 App（iOS/Android + 推送）。

---

## 2. 现状盘点：yi-agent 已经具备的地基

### 2.1 app-server 就是为"多前端"设计的

`yi-agent-app-server` 是一个长驻进程，通过 **stdio 上的 JSON-RPC 2.0（JSONL 分帧）** 与前端通信，
明确采用 codex 风格架构，把 `AgentEvent` 翻译成稳定的线协议，避免前端耦合内部类型
（`docs/project-management/yi-agent-app-server.md`）。

对话模型与 Codex app-server 三个原语一一对应：

- **Item**：`userMessage` / `agentMessage` / `toolCall` / `user_interjection`，有 `started` / `delta` / `completed` 生命周期
- **Turn**：一次 agent 工作单元（`turn/start` / `turn/interrupt` / `turn/interject` / `turn/completed`）
- **Thread**：持久会话容器（`thread/start` / `thread/resume` / `thread/list` / `thread/listAll` / `thread/delete` / `thread/rename` / `thread/clear` / `thread/compact`）

服务端 → 客户端通知：`thread/started`、`turn/started`、`thread/status/updated`、`item/*`、`turn/completed`、
`thread/tokenUsage/updated`、`error`；服务端 → 客户端**反向请求**：`item/toolCall/requestApproval`（权限审批）。

已有能力（手机端可直接复用，无需新协议）：

| 能力 | RPC | 手机端用途 |
| --- | --- | --- |
| 列所有目录下的会话 | `thread/listAll` | 首页 session 列表 |
| 会话实时状态 | `thread/status/updated`（`idle`/`running`/`awaiting_approval`）+ `thread/listAll` 每条带 `status` | 列表徽标、锁屏通知 |
| 恢复并继续对话 | `thread/resume`（回放历史 + 恢复上下文） | 打开会话看到完整历史 |
| 发消息 / 打断 / 中途追加 | `turn/start` / `turn/interrupt` / `turn/interject` | 聊天 + 停止 + 边跑边补指令 |
| 审批 | 反向请求 `item/toolCall/requestApproval` + 客户端响应 `Decision` | **手机远程批准危险命令（核心价值）** |
| 管理 | `thread/rename`/`delete`/`clear`/`compact`、`thread/setPermissionMode` | 会话管理、切 YOLO |
| 托管进程 | `process/list`/`read`/`kill` + `process/updated` | 看后台任务、杀进程 |
| 子 agent 观察与干预 | `agent/children/list`、`agent/trace/read`/`watch`、`agent/message`、`agent/cancel/preview`+`cancel` | 子 agent 进展、发消息、两步取消 |
| 工作目录 | `workspace/list`/`add`/`remove` | 切项目 |

**关键含义：手机端要的"看 session / 互动 / 管理"，协议层几乎全部已经存在。** 缺口是传输与多客户端，不是功能。

### 2.2 传输层已对 channel 泛型，但只支持 stdio

`server::run<R, W>(reader, writer, cfg)` 对 `AsyncRead` / `AsyncWrite` 泛型
（`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:765`），而 CLI 入口把
`tokio::io::stdin()/stdout()` 传进去（`yi-agent-rs/crates/yi-agent/src/main.rs`，`run_app_server`）。
`ensure_stdio_listen` 目前**只接受 `stdio://`**，其它一律拒绝。

也就是说：**换传输是接线工作，不是重构**——问题在于 app-server 需要一个"双向流"，
而 WebSocket / 手机长连接天然满足。项目 P3 路线图里已经登记了
「Unix socket / websocket 传输」（`docs/project-management/desktop.md`）。

### 2.3 但当前服务端假设"恰好一个客户端"

这是本次改造真正的硬骨头：

- `writer` 是**单个** `Arc<MessageWriter<W>>`（`server.rs:862`），所有通知往这一条流写。
- 权限 `pending` 是**单个** `HashMap<String, oneshot::Sender<Decision>>`（`server.rs:869`）。

当桌面端和手机端同时在线时：
- 通知需要**广播给所有已连接客户端**（否则手机看不到桌面正在跑的会话）；
- 审批响应需要**路由到最早等待的那个决定**（谁先点谁生效，另一个客户端要收到"已被处理"）；
- 反向请求（审批弹窗）**广播给谁**？本方案建议广播，任一客户端可应答，先到先得（见 §5.3）。

### 2.4 桌面端是"薄桥接"，可原样照抄给手机不算方案

`desktop/src-tauri/src/bridge.rs` 的职责只有三件：管理 sidecar 生命周期、转发 stdio 帧、
把帧分类成 `Notification` / `ReverseRequest` / `Response` / `Garbage`。它**不含 agent 逻辑**。
手机端如果走"隧道到电脑的 stdio"，会立刻遇到 stdio 无法被多个远端共享的问题——
所以不能停在"把 stdio 透出去"，必须给 app-server 加一个真正的网络传输（见 §5.1）。

### 2.5 thread 持久化天然是"跨设备一致"的

每个 thread 落 `<该 thread 的 cwd>/.yi-agent/threads/<thread_id>.jsonl`（只追加）+
`<thread_id>.meta.json`（可变），跨目录清单在 `~/.yi-agent/workspaces.json`
（`docs/project-management/yi-agent-app-server.md`；`thread_store.rs`、`workspace_index.rs`）。
**"手机上看到的就是电脑上的完整对话"在数据层已经成立**——`thread/resume` 就是回放这份日志。
不需要为手机单独做会话存储或同步。

---

## 3. 同类产品怎么做

三家主流产品给出的答案高度一致：**本地执行 + 中继 + 配对 + 手机只做控制台**。

### 3.1 Claude Code Remote Control（2026-02 上线）

- **定位**：同步层 / 中继网关，**不是**把工作迁到云端。session 始终跑在本地，远端只是"窗口"。
- **网络**：本地进程**只发出站 HTTPS**，从不开任何入站端口；经 Anthropic API 路由消息；
  多条**短期、单用途、独立过期**的凭证；全程 TLS。
- **配对**：终端输入 `/remote-control`（或 `/rc` 带历史）→ 显示 URL / 二维码 → 手机扫码。
- **稳定性**：断网/休眠/切 Wi-Fi 后自动重连，断网约 10 分钟才超时。
- **一致性**：本地文件系统、MCP server、skills 全部保留；每个实例同一时刻**只允许一个远程会话**。

### 3.2 OpenAI Codex 手机端（2026-05 上线，ChatGPT App 内）

- **定位**：多 agent 工作的**移动控制台/指挥中心**，不是手机写代码。
- **网络**：**secure relay（安全中继）**，不把桌面应用暴露到公网；配对是桌面端 App 出示二维码、
   ChatGPT 手机 App 扫码。
- **能力**：加载环境实时状态（threads / approvals / plugins / 项目上下文）、看输出、批命令、
  切模型、起新任务；截图、终端输出、diff、测试结果、审批实时推送到手机。
- **细节**：深度适配 **iOS Live Activities / 推送**，锁屏追踪长任务；文件、凭证、权限全留在电脑上，
  **手机不能直接操作本地文件系统**（这是设计上的安全边界）。

### 3.3 OpenClaw（开源，IM 网关流派）

- **定位**：自托管 agent 网关，把 WhatsApp / Telegram / 飞书 / 企微 / Slack 等 IM 接到 agent 上。
- **网络**：IM 的 webhook 需要**公网可达 URL**（或长轮询/中继）。
- **与 yi-agent 的交集**：和 `docs/core-feature.md` 里第 3 条「IM 集成（飞书、微信）」是同一思路——
  **用 IM 当 transport 和 UI，省掉开发手机 App**。飞书卡片能审批、能看进度、能发消息。

### 3.4 三家对比

| 维度 | Claude Code RC | Codex 手机端 | OpenClaw（IM） |
| --- | --- | --- | --- |
| 代码/执行位置 | 本地 | 本地/云端 devbox | 本地（自托管） |
| 打通方式 | 出站 HTTPS + 厂商中继 | secure relay | IM webhook（需公网 URL） |
| 配对 | 终端二维码 | 桌面 App 二维码 | IM 账号 / 群聊 |
| 手机端形态 | 官方 App + Web | ChatGPT App | 任何 IM 客户端 |
| 推送 | 有 | 有（Live Activities） | IM 原生推送 |
| 手机能否改文件 | 不能（只做控制台） | 不能 | 取决于工具权限 |
| 是否依赖厂商云 | 是 | 是 | 否（自托管） |

**对 yi-agent 的启示**：走"控制台"定位，别在手机上复刻桌面全功能；打通优先"出站 + 中继"；
配对用二维码；推送是体验分水岭（无论走原生 App 还是 IM）。

---

## 4. 目标能力分解

把用户诉求拆成可验证的能力项：

| 编号 | 能力 | 依赖现有 RPC | 新增工作 |
| --- | --- | --- | --- |
| C1 | 手机看到电脑上所有 session 列表 + 实时状态 | `thread/listAll`、`thread/status/updated` | 无（协议已有） |
| C2 | 打开某 session 看到完整对话历史 | `thread/resume` | 无 |
| C3 | 实时跟流式输出（文字/工具卡片） | `item/*` delta | 无 |
| C4 | 发消息 / 打断 / 中途追加 | `turn/start`、`turn/interrupt`、`turn/interject` | 无 |
| C5 | **远程审批危险命令** | 反向请求 + `Decision` | 多客户端审批路由 |
| C6 | 会话管理（改名/删/清空/压缩/切 YOLO） | `thread/*`、`thread/setPermissionMode` | 无 |
| C7 | 看/杀托管进程 | `process/*` | 无 |
| C8 | 看子 agent 进展并干预 | `agent/*` | 无 |
| C9 | 外网可达（电脑在内网） | —— | **传输 + 中继** |
| C10 | 推送通知（等你审批 / 任务完成） | —— | **推送服务** |
| C11 | 手机与电脑**同屏一致**（同一 session 双端可见可操作） | —— | **多客户端扇出** |

C9 / C10 / C11 是新工作，且 C11 是"理论上手机和电脑 session 一样"这句话的技术落点。

---

## 5. 五个关键架构决策

### 5.1 D1：手机 App 形态怎么选？

| 方案 | 优点 | 缺点 | 适用 |
| --- | --- | --- | --- |
| **A. PWA / 移动 Web**（复用 desktop 前端 + 浏览器） | 零商店成本、迭代最快、iOS/Android 通吃、可加主屏图标 | 无原生推送（iOS PWA 推送受限）、后台易被冻结 | **Tier 1 首选，先验证体验** |
| **B. 原生 App（React Native / Tauri Mobile / Flutter）** | 原生推送、后台保活、Live Activity、体验最好 | 开发与上架成本高、双端维护 | Tier 2，体验跑通后投入 |
| **C. IM 机器人（飞书 / 微信 / Telegram）** | **零 App 开发**、复用 IM 推送与账号体系、和 core-feature 第 3 条一致 | UI 表达力受限于 IM 卡片、交互不如聊天界面流畅 | 与 A 并行，做"轻量通知 + 审批" |

**建议**：A + C 先行。PWA 负责"看 + 聊 + 管"，IM 机器人负责"推送 + 一键审批"。
两者共用同一份线协议与后端。原生 App 留作 Tier 2。

> 参考依据：Codex / Claude 都选择了"手机端 = 控制台"而非全功能，说明先做控制台是行业默认路数。

### 5.2 D2：外网如何打通？

前提：电脑在内网、无公网 IP、手机在 4G/5G 或别的 Wi-Fi。可选：

| 方案 | 原理 | 成本 | 优点 | 缺点 | 推荐度 |
| --- | --- | --- | --- | --- | --- |
| **Tailscale / ZeroTier（overlay VPN）** | WireGuard 组虚拟局域网，P2P 优先、打洞失败走 DERP 中继 | 免费（个人） | 无需公网 IP、无需开端口、全平台、端到端加密 | 手机需装 App/VPN profile；中继在境外时延迟高 | **高（首选，最省心）** |
| **FRP（自建 VPS）** | 云服务器做反向代理，本地 frpc 主动出站 | 需一台公网 VPS | 可控、稳定、可暴露任意端口、社区成熟（~11w star） | 需自备 VPS 与运维；流量走服务器带宽 | **高（有 VPS 时）** |
| **Cloudflare Tunnel** | cloudflared 出站到 CF 边缘 | 免费，需域名托管到 CF | 可视化配置、无需公网 IP、可用 443 | 国内网络不佳；**私有服务必须叠加 Cloudflare Access** | 中 |
| **ngrok / 类似托管隧道** | 托管隧道服务 | 免费档有限 | 极省事 | 免费档限速/限时、依赖第三方 | 中 |
| **自建反向 WebSocket 中继** | 手机与电脑都连到你的 VPS 上的一条 WS，服务端配对转发 | 需 VPS + 少量开发 | **最贴合 yi-agent**：把 JSON-RPC 帧原样在 WS 上跑，和 app-server 的多客户端改造天然统一；可做端到端加密 | 需写中继服务 | **高（与 §5.4 配套）** |
| **WebRTC DataChannel** | P2P 数据通道，需信令 + TURN | 中 | 延迟低、P2P | 需信令服务、TURN、复杂度高 | 低（过度设计） |
| **MQTT（公共 broker）** | 发布订阅 | 免费 | 极简 | 语义不匹配 RPC/流式、需自建鉴权 | 低 |
| **IM 当 transport** | 飞书机器人 webhook/长连接 | 免费 | 免 App、自带推送 | 需公网回调或长连接、表达力受限 | 中（配合 D1-C） |

**建议**：
- **自用/个人最优先**：Tailscale。装完即用，无公网 IP 也能点对点，是"电脑在内网"最直接的答案。
- **想完全自主可控 + 有 VPS**：自建反向 WebSocket 中继（与后端多客户端改造同构）。
- **不想装客户端**：FRP 或 Cloudflare Tunnel（注意给私有服务加认证）。
- **与 IM 集成愿景合并**：飞书机器人做轻量通道，但重会话体验仍走 WS。

> 关键原则（三款产品一致）：**本地只发出站连接，绝不开放入站端口**。这既是安全，也是"免防火墙配置"的前提。

### 5.3 D3：服务端要不要支持多客户端？（关键）

用户明确说"理论上手机上和电脑上的 session 都是一样的"。这要求 app-server **同时服务多个客户端**：

- **当前**：单 `Arc<MessageWriter<W>>` + 单 `pending` 审批表 → 第二个客户端连上会抢走同一份流，
  桌面端和手机端互相看不见对方的通知。
- **目标**：一个**广播中心**（hub），所有客户端订阅；通知扇出到全部；RPC 请求带客户端身份。

需要定义的行为：

1. **通知扇出**：`item/delta`、`turn/completed`、`thread/status/updated` 等广播给所有订阅了该 thread 的客户端。
2. **审批归属**：反向请求 `item/toolCall/requestApproval` 广播给所有客户端；**任一客户端先应答即生效**，
   其余客户端收到"已被处理"（避免两边各答一次）。也可加"仅当前活跃客户端可答"的策略开关。
3. **请求来源**：`turn/start` 由谁发的要记录，`turn/interrupt` 允许多端；冲突时以服务端状态为准
   （桌面端已有 `-32012`/`-32013` 方法自愈逻辑可复用，见 `desktop/src/App.tsx` 发送自愈）。
4. **订阅粒度**：客户端可订阅"全部 thread"或"仅当前打开的 thread"，降低移动网络流量。

> 落地位置：把 `server.rs` 里的 `Arc<MessageWriter<W>>` 抽象成一个 `Broadcaster` trait；
> stdio 传输实现成"单订阅者 broadcaster"（行为与今天一致，桌面端零回归），
> WS 传输实现成"多订阅者 broadcaster"。这一步是**向后兼容的增量改造**。

### 5.4 D4：配对与认证怎么做？

学 Codex/Claude：**扫码配对 + 短期作用域凭证**。

- 电脑端生成一次性配对码 / 二维码（含中继地址 + 配对 token）；
- 手机扫码 → 换取**短期 access token**（+ refresh token），token 绑定设备；
- token 有明确 scope（只读 / 可审批 / 可写）与过期时间；
- 设备可在电脑端"已连接设备"列表里撤销；
- 传输强制 TLS；自建中继可再做端到端加密（中继只转发密文）。

不要的做法：把 api key / 无认证的端口直接暴露公网；长期不过期的 token；手机端能直接读全盘文件。

### 5.5 D5：安全边界定到什么程度？

"手机上看到的和终端里一样"是双刃剑。建议默认**收紧**，可显式放宽：

- **默认只读**：手机默认只能看 + 审批，发消息/改文件需二次确认或显式开关；
- **审批 fail-safe**：超时/无响应按 `Deny`（服务端已有此语义，`PERMISSION_TIMEOUT` + Deny）；
- **YOLO 模式要有红字确认**（桌面端 `ModeChip` 已有确认框，手机端照做）；
- **危险操作审计**：手机端触发的 `turn/start`、审批决定、`process/kill` 记 trace；
- **沙箱不变**：沿用 `--sandbox read-only/workspace-write`，手机不能突破电脑端配置的沙箱。

---

## 6. 推荐方案

### 6.1 架构总览

```
┌──────────────┐        ┌──────────────┐
│  手机 (PWA)   │        │  IM 机器人    │  ← 推送 + 一键审批
│  聊天/列表/审批 │        │ (飞书/Telegram)│
└──────┬───────┘        └──────┬───────┘
       │ WSS (JSON-RPC over WS) │
       │                        │
   ┌───▼────────────────────────▼───┐
   │   中继 / 打通层（三选一）          │
   │  A. Tailscale overlay（直连）     │
   │  B. 自建反向 WS 中继（VPS）        │
   │  C. FRP / CF Tunnel              │
   └───────────────┬─────────────────┘
                   │ 出站连接（本地不开入站端口）
   ┌───────────────▼─────────────────┐
   │  电脑：yi-agent app-server       │
   │  + Broadcaster（多客户端扇出）     │
   │  + 新传输：--listen ws://...      │
   │  （agent / thread / 权限逻辑不变）  │
   └─────────────────────────────────┘
```

### 6.2 分阶段策略

- **Tier 0（验证，1 周内）**：给 app-server 加 `--listen ws://127.0.0.1:PORT`，
  只做**单客户端**验证（浏览器能连、能看到 thread 列表和流式输出）。
  此阶段不动多客户端逻辑，用最小改动证明"换传输可行"。
- **Tier 1（可用）**：Broadcaster 多客户端扇出 + 移动 Web UI + 配对/凭证 + Tailscale/中继打通。
  达到 C1–C9、C11。**这一步交付即可日常使用。**
- **Tier 2（好用）**：推送（原生 App 或 IM 机器人）、语音输入、diff/截图回传、Live Activity 式锁屏进度。
- **Tier 3（团队/多设备）**：多用户、共享会话、审计与 RBAC（对齐 OpenClaw 2.0 的多人协作方向）。

---

## 7. 分阶段实施路线图

### Tier 0：WebSocket 传输（单客户端）

- [ ] `ensure_stdio_listen` 扩展接受 `ws://host:port`（或在 app-server 内新增 `--listen` 分支）
- [ ] 用现成异步 WS 库把每条 WS 消息当一行 JSON 喂给 `MessageReader`。workspace 已声明 `axum = "0.8"`（`yi-agent-rs/Cargo.toml:51`，目前仅 `yi-agent-web` 使用），启用其 `ws` feature 即可用 `axum::extract::ws`；或引入 `tokio-tungstenite`——**注意 app-server 目前并不依赖 axum**，需要新增依赖
- [ ] 复用 `server::run`，`reader`/`writer` 换成 WS 的 sink/stream 适配器
- [ ] 验证：浏览器 `WebSocket` 连上后 `initialize` → `thread/listAll` → `thread/resume` → `turn/start` 全通
- **判据**：不改任何 agent/thread 逻辑，浏览器端能跑通一次完整对话与一次审批

### Tier 1：多客户端 + 移动端 + 打通

- [ ] 抽象 `Broadcaster`：stdio 实现单订阅（零回归），WS 实现多订阅扇出
- [ ] 审批广播 + 先到先得 + "已被处理"回执
- [ ] 订阅粒度（全部 / 当前 thread）
- [ ] 移动 Web UI（复用 `desktop/src/lib/protocol.ts` 类型与状态机，抽成共享包）
- [ ] 配对流程：电脑端出二维码 → 手机换短期 token；凭证 scope 与过期；已连接设备列表 + 撤销
- [ ] 打通层接入：Tailscale 直连（文档化指引）或自建 WS 中继
- **判据**：桌面端与手机端同时连同一 app-server，两端看到同一 session 的实时流，手机能审批、能发消息、能打断

### Tier 2：推送与体验

- [ ] 推送通道（任选）：IM 机器人 / iOS APNs + Android FCM / Web Push
- [ ] `awaiting_approval`、`turn/completed` 触发推送与锁屏进度
- [ ] 手机端只读模式默认、写操作二次确认
- [ ] 可选：语音输入、图片附件（协议已预留 `--naked` 之外的扩展空间）
- **判据**：手机锁屏能收到"等你审批"通知并一键批准

### Tier 3：多用户与协作（远期）

- [ ] 多用户 / 共享会话 / 会话归属（creator-owner-participant）
- [ ] 审计日志、RBAC、工具级命令权限
- [ ] 团队中继服务

---

## 8. 风险与缓解

| 风险 | 说明 | 缓解 |
| --- | --- | --- |
| **多客户端改造回归桌面端** | 今天的行为是单客户端，改成扇出可能引入通知重复/丢失 | Broadcaster 的 stdio 实现保持单订阅语义；桌面端测试全绿作为回归门禁 |
| **审批双答** | 两端同时点"允许/拒绝" | 先到先得 + 服务端幂等 + 另一端点后收"已被处理"回执 |
| **移动网络不可靠** | 切网/弱网导致流断裂 | 断线重连 + `thread/resume` 增量回放（协议已支持）；事件带 id 便于对账 |
| **安全暴露** | 中继/token 泄露 → 远端可批准危险命令 | 短期内作用域凭证 + 设备撤销 + 默认只读 + 审批 fail-safe(Deny) + TLS（自建中继可端到端加密） |
| **中继在境外延迟高** | Tailscale DERP / CF 国内体验 | 自建 VPS 中继（FRP / WS）作为国内低延迟选项 |
| **PWA 后台被冻结** | iOS 上后台无法长连 | 关键事件走推送而非长连保活；前台才维持 WS |
| **协议演进** | 手机与电脑版本不一致 | 协议已有 `PROTOCOL_VERSION` 与版本不符拒绝的先例（daemon IPC），app-server 线协议也应加版本协商 |

---

## 9. 待决策问题

1. **形态优先级**：先做 PWA，还是直接投原生 App？（决定 Tier 1 的工作量与是否依赖应用商店）
2. **打通选型**：个人自用走 Tailscale，还是自建 WS 中继（更可控、但要写/运维中继）？
3. **是否复用 IM 机器人**：与 `docs/core-feature.md` 第 3 条合并，用飞书承担推送 + 轻审批？
4. **多客户端审批策略**：广播先到先得，还是"仅当前活跃/主客户端可答"？
5. **手机端默认权限**：默认只读（更安全）还是与桌面等权（更顺手）？
6. **是否值得单独立项**：本调研建议 Tier 0/1 可并入 app-server 路线图（它已登记"websocket 传输"），
   Tier 2/3 再作为独立产品投入。

---

## 10. 参考资料

### 代码与文档（本仓库）

- `docs/project-management/yi-agent-app-server.md` — app-server 全量能力与线协议
- `docs/project-management/desktop.md` — 桌面端薄桥接与 P3 路线图（含 websocket 传输）
- `desktop/src-tauri/src/bridge.rs` — 帧分类 / 转发实现（手机端可参考）
- `desktop/src/lib/protocol.ts` — 线协议类型（可抽成手机端共享包）
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:765` — `run<R, W>` 传输泛型
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:862/869` — 单 writer / 单审批表（本次改造点）
- `yi-agent-rs/crates/yi-agent/src/main.rs` — `run_app_server` / `ensure_stdio_listen`（传输入口）
- `docs/core-feature.md` — 第 3 条「IM 集成（飞书、微信）」

### 同类产品

- Claude Code Remote Control：本地只发出站 HTTPS + 厂商中继 + 扫码配对 + 短期作用域凭证 + 自动重连
  （官方文档 code.claude.com/docs/en/remote-control）
- OpenAI Codex 手机端：secure relay（不暴露公网）+ 桌面二维码配对 + 全量实时状态 + Live Activities 推送；
  文件/凭证/权限留在电脑（OpenAI "Work with Codex from anywhere"，2026-05）
- OpenClaw：自托管 agent 网关 + 多 IM 渠道（WhatsApp/Telegram/飞书/企微/Slack），2.0 引入共享云会话与多人协作
- Codex app-server 架构：Item / Turn / Thread 三原语 + JSON-RPC over stdio + 服务端反向请求
  （OpenAI 工程师 Celia Chen 系列文章）

### 打通方案

- Tailscale / ZeroTier：WireGuard overlay，P2P 优先、打洞失败走中继，个人免费，全平台
- FRP：自建 VPS 反向代理，开源（~11w star），适合有公网 VPS 且要完全可控
- Cloudflare Tunnel：出站隧道，免费但需域名；私有服务须叠加 Cloudflare Access
- 关键共识：**本地只发出站连接，不开放入站端口**
