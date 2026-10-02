# 手机远程访问：同一 session 的跨设备查看与控制

**目标：** 新开发一个手机端界面，能实时看到电脑上的 session（与桌面端**同一份**），
并对电脑上的环境做互动与管理（发消息、打断、中途追加、远程审批、管理进程与会话）；
手机可能在外网、电脑在内网，需要打通。

**状态：** 已设计，待实现（v1 范围：Tier 0 + Tier 1）。

**相关文档：**
`docs/research/2026-10-02-mobile-remote-access.md`（调研：现状盘点、同类产品、打通选型）、
`docs/project-management/yi-agent-app-server.md`（线协议全量能力）、
`docs/project-management/desktop.md`（桌面端薄桥接与 P3 路线图，含 websocket 传输）、
`docs/superpowers/plans/2026-09-26-desktop-gui-design.md`（app-server 原始设计 §11 路线图）。

---

## 1. 背景与问题

### 1.1 现状（已核实）

`yi-agent-app-server` 是桌面 GUI 的后端长驻进程，采用 codex 风格 app-server 架构，
通过 **stdio 上的 JSON-RPC 2.0（JSONL 分帧）** 与前端通信。它已经把 `AgentEvent`
翻译成稳定的线协议，前端不耦合内部类型。三个对话原语与 Codex app-server 一一对应：
**Item**（`userMessage` / `agentMessage` / `toolCall` / `user_interjection`，
带 `started`/`delta`/`completed` 生命周期）、**Turn**（`turn/start`/`interrupt`/`interject`/`completed`）、
**Thread**（`thread/start`/`resume`/`list`/`listAll`/`rename`/`delete`/`clear`/`compact`）。

手机端要的"看 session / 互动 / 管理"，**协议层几乎全部已存在**：

| 能力 | RPC | 手机端用途 |
| --- | --- | --- |
| 列所有目录下的会话 | `thread/listAll` | 首页 session 列表 |
| 会话实时状态 | `thread/status/updated` + `thread/listAll` 带 `status` | 列表徽标 |
| 恢复并继续对话 | `thread/resume`（回放历史 + 恢复上下文） | 打开会话看到完整历史 |
| 发消息 / 打断 / 中途追加 | `turn/start` / `turn/interrupt` / `turn/interject` | 聊天 + 停止 + 边跑边补 |
| 审批 | 反向请求 `item/toolCall/requestApproval` + `ClientResponse` | **远程批准危险命令（核心价值）** |
| 管理 | `thread/rename`/`delete`/`clear`/`compact`、`thread/setPermissionMode` | 会话管理、切 YOLO |
| 托管进程 | `process/list`/`read`/`kill` + `process/updated` | 看后台任务、杀进程 |
| 子 agent | `agent/children/list`、`agent/trace/read`/`watch`、`agent/message`、`agent/cancel/preview`+`cancel` | 进展与干预 |
| 工作目录 | `workspace/list`/`add`/`remove` | 切项目 |

thread 持久化天然跨设备一致：每个 thread 落 `<thread 的 cwd>/.yi-agent/threads/<id>.jsonl`
（只追加）+ `<id>.meta.json`（可变），跨目录清单在 `~/.yi-agent/workspaces.json`。
**"手机上看到的就是电脑上的完整对话"在数据层已经成立**——`thread/resume` 就是回放这份日志。

### 1.2 真正的缺口（两个）

1. **传输**：只支持 stdio（`ensure_stdio_listen` 拒绝一切其它值，
   `yi-agent-rs/crates/yi-agent/src/main.rs:83`），手机无法通过网络连接。
   `server::run<R, W>(reader, writer, cfg)` 已对传输泛型
   （`yi-agent-app-server/src/server.rs:765`），因此换传输是接线，不是重构。
2. **多客户端**：服务端假设**恰好一个**客户端——`writer` 是单个
   `Arc<MessageWriter<W>>`（`server.rs:862`），权限 `pending` 是单个
   `HashMap<String, oneshot::Sender<Decision>>`（`server.rs:869`）。桌面端与手机同时在线时，
   通知无法到达两端，审批无法路由。**这是"手机与电脑 session 一样"这句话的技术落点。**

### 1.3 同类产品结论（调研 §3）

Claude Code Remote Control 与 OpenAI Codex 手机端**清一色**采用：
**本地只发出站连接 + 中继 + 扫码配对 + 短期作用域凭证**，且手机定位为"控制台"
（看、批、指挥），不做全功能、不能直接读文件系统。OpenClaw 用 IM 当 transport 与 UI。
本设计沿用这套路数。

---

## 2. 范围

### 2.1 做什么（v1）

- app-server 新增 **WebSocket 传输**（`--listen ws://host:port`；`stdio://` 仍是默认，行为不变）
- **多客户端扇出**（Broadcaster）：通知广播、响应定向、审批先到先得
- **配对与设备管理**：一次性配对码扫码 → 持久化设备表 → 短期设备 token → 撤销
- **iOS 原生 App**：用 **Tauri 2** 复用现有 React 前端（见 §2.4）；App 是**纯远程客户端**，
  不跑 agent、不执行 shell、不读本地文件
- **打通 v1**：**自建反向 WSS 中继**（见 §11）——手机与电脑都只发出站连接

### 2.2 不做什么（非目标，YAGNI）

- Android App（v2；Tauri 2 共享代码，届时成本低于 iOS）
- IM 机器人（独立项目，v2）
- 多用户 / 多租户协作：仍是"一台电脑一个人"，只是同一个人多设备
- 按 thread 订阅过滤（v1 广播全部通知）
- 中继端到端加密（v1.1；先把功能跑通）
- thread_store 多写者（持久化仍由 app-server 单点负责）
- 手机端直接读写宿主文件系统（**设计上的安全边界**，非缺陷）

### 2.4 形态与端到端拓扑（已决策）

**App 形态：Tauri 2 iOS 原生 App，复用现有 React 前端。** 依据：

- 前端的 Tauri 耦合极小——只有 `tauriTransport.ts`（44 行）、`App.tsx` 的 `plugin-dialog`
  动态导入、`MarkdownText.tsx` 的 `plugin-opener` 三处；`protocol.ts` / `rpc.ts` /
  `session.ts` / `threadStore.ts` 是纯 TS，可零改动复用。
- `rpc.ts` 已定义 `Transport` 接口，`tauriTransport()` 只是其一个实现；iOS 只需新增
  `wsTransport()`。
- 定位为**纯远程客户端**（不跑 agent、不执行 shell、不读本地文件），因此
  **不触碰 App Store 的"下载并执行代码"红线**，审核风险远低于"手机上跑 coding agent"。

**打通：自建反向 WSS 中继**（不用 Tailscale）。依据：

- iOS 同一时刻只允许一个 VPN，要求用户开 Tailscale 会与其代理/VPN 冲突；
- 国内 App Store 装不到 Tailscale（需外区 Apple ID），"为用你的 App 先装另一个 App 并注册账号"
  在产品上不可接受；
- 反向 WSS 中继让用户**只装一个 App、扫码即用**，与 Codex 的 secure relay 同构。

拓扑：

```
┌───────────────────────┐
│ iOS App (Tauri)        │
│  React UI + wsTransport│
└───────────┬───────────┘
            │ WSS 出站
┌───────────▼───────────┐
│ 自建反向 WSS 中继        │  只按会话路由/转发 JSON-RPC 帧,
│ (VPS + 域名 + TLS)      │  不解析、不落库明文;同时承担 APNs 推送
└───────────▲───────────┘
            │ WSS 出站
┌───────────┴───────────┐
│ 电脑 app-server         │  ← 不开放入站端口
└───────────────────────┘
```

**边界**：电脑侧不开放入站端口；App 只连中继、不直连电脑；中继在 v1 只做转发
（端到端加密列 v1.1）。

---

## 3. 架构

### 3.1 分层

```
┌──────────────┐   ┌──────────────┐
│ iOS App       │   │ 桌面端 (现有)  │
│ (Tauri 2)     │   │ Tauri          │
│ 聊天/列表/审批 │   │                │
└──────┬───────┘   └──────┬─────────┘
       │ WSS (经中继)       │ stdio (本机)
       │                   │
   ┌───▼───────────────────┼──────────┐
   │ 反向 WSS 中继 (VPS)     │          │
   └───┬───────────────────┘          │
       │ WSS 出站                       │
        ┌───────▼──────────────┐       │
        │ app-server            │◀──────┘
        │  ├ Transport (stdio)  │ ← 现有，行为不变
        │  ├ Transport (ws)     │ ← 新增
        │  └ Broadcaster        │ ← 新增：扇出/订阅
        │  ├ thread_store        │ ← 不动（单写者）
        │  ├ Translator          │ ← 不动
        │  └ Agent / runtime     │ ← 不动
        └───────────────────────┘
```

### 3.2 核心思路

app-server 从"单前端 sidecar"升级为"可被多个前端通过网络连接的长驻服务"，
**agent / thread / 权限逻辑零改动**。

1. **传输抽象**：新增一条 WS 路径，把"WS 每条消息"当作"JSONL 一行"喂进现有解析与分发。
   stdio 路径完全不动。
2. **客户端注册表**：app-server 维护 `ClientId → sender`。stdio 客户端注册为固定 id
   `"local"`；每个 WS 连接注册一个 `ws-<uuid>`。
3. **Broadcaster**：所有出站帧（通知 + 反向请求 + 响应）都经它。stdio 模式下只有 1 个订阅者，
   语义与今天**逐字节一致**；WS 模式扇出到全部。

### 3.3 不变量

1. **stdio 零回归**：stdio 下 Broadcaster 只有 `local`，`reply`/`broadcast` 退化为今天的单流写；
   现有 182 个 app-server 测试全绿是门禁。
2. **单一事实来源**：thread 状态、history 永远以 app-server 为准，手机端只是视图。
3. **广播幂等**：同一通知对所有客户端内容一致；`approvalResolved` 保证"已处理"可见。
4. **一个客户端故障不拖垮服务**：断连/慢消费者只影响该 `ClientId`。

---

## 4. 组件与接口

### 4.1 `broadcast.rs`（新增）— 扇出中心

```rust
/// 一个已连接客户端的身份。
pub struct ClientId(String);            // stdio = "local"，ws = "ws-<uuid>"

/// 权限范围：observe < control < admin。
pub enum Scope { Observe, Control, Admin }

/// 服务端 → 客户端的扇出中心。
pub struct Broadcaster {
    clients: StdMutex<HashMap<ClientId, mpsc::Sender<Value>>>,
}

impl Broadcaster {
    /// 注册一个客户端，返回它专属的出站接收端。
    pub fn register(&self, id: ClientId) -> mpsc::Receiver<Value>;
    /// 摘除一个客户端（断连时）。
    pub fn unregister(&self, id: &ClientId);
    /// 通知 / 反向请求：发给所有客户端；失效订阅自动剔除。
    pub fn broadcast(&self, frame: Value);
    /// 响应：只回请求发起者；Err 表示该客户端已断。
    pub fn reply(&self, id: &ClientId, frame: Value) -> Result<(), Closed>;
}
```

每个客户端有一个专属出站 channel + 一个写任务；`broadcast` 遍历所有 sender，
`try_send` 失败（队列满或已关闭）即摘除该客户端——**背压而非无限缓冲**。

### 4.2 `ws.rs`（新增）— WebSocket 传输

- 用 `axum` 0.8 的 `ws` feature（workspace 已声明 `axum = "0.8"` 于 `yi-agent-rs/Cargo.toml:51`，
  目前仅 `yi-agent-web` 使用；**需给 `yi-agent-app-server` 新增该依赖**）。
- 每个连接：认证 → 注册 `ClientId` + sink → 把该连接收到的每帧当作"JSONL 一行"喂入入站流。
- **协议解析、分发逻辑全部复用**；ws 层只做"帧 ↔ 行"转换与连接生命周期。
- 入站帧复用现有 `run_with` 的 reader→channel → 主循环模型；出站走 Broadcaster。

### 4.3 `pairing.rs` + 设备存储（新增）

```
pair/create   → { code, ws_url, expires_in }   // 桌面端调用，展示二维码
（手机带上 code 连 ws：/ws?pair=<code>）
→ 换取短期 session token（scope 绑定设备）
device/list   → 已连接设备（名称、scope、最后活跃）
device/revoke → 撤销某设备（立即断开其 ws 并使其 token 失效）
```

- 一次性配对码：5 分钟过期、用后即焚。
- 二维码携带**服务端公钥指纹**，手机首次连接核对，防中间人。
- 设备表持久化（**决策 B**）：落在 `~/.yi-agent/devices.json`，复用
  `workspace_index.rs` 的"temp 文件 + rename 原子替换 + 进程内 Mutex 串行化"模式。
  每设备一条记录（id、名称、scope、token 哈希、创建/最后活跃时间）。
  **撤销即删记录**，token 随之失效。

### 4.4 `server.rs` 的改动

| 位置 | 今天 | 改为 |
| --- | --- | --- |
| `writer`（`server.rs:862`） | 单个 `Arc<MessageWriter<W>>` | `Arc<Broadcaster>` |
| `initialized`（`server.rs`） | 全局 `bool` | **每客户端**（ws 连接各自握手） |
| `write_response(&writer, …)` | 直接写 | `hub.reply(origin_client, …)` |
| `write_notification(&writer, …)`（`:2586`） | 直接写 | `hub.broadcast(…)` |
| 反向请求 `requestApproval`（`:3017`） | 单流 | `hub.broadcast(…)`，**先到先得** |
| `route_client_response`（`:2665`） | 命中即路由 | 未命中（已被别端处理）**静默成功**；命中后广播 `item/toolCall/approvalResolved { perm_id, by, decision }` |

`pending` 表按 `perm_id` 全局唯一（现状已如此：id 取自进程级 `perm_seq` 计数器，
`server.rs:879`），因此**无需改 key**，只需加"双答"处理与已解决广播。

### 4.5 新增协议（最小集）

- 请求：`pair/create`、`device/list`、`device/revoke`
- 通知：`item/toolCall/approvalResolved { perm_id, by, decision }`
- 错误码：`-32014 insufficient_scope`（`admin` 类操作在 `control` scope 下触发）
- `initialize` 的 `capabilities` 暴露新增能力，不破坏旧客户端

`thread/*`、`turn/*`、`item/*`、`process/*`、`agent/*` 全部复用，**不新增方法**。

---

## 5. 配对与安全

### 5.1 流程 A：首次配对（扫码）

```
电脑端（桌面/local）              iOS App（外网）
   │ 1. RPC: pair/create             │
   │    → { code:"7F3K-9Q2M",         │
   │        ws_url:"wss://host/ws",   │
   │        expires_in:300 }          │
   │ 2. 渲染二维码（含 ws_url+code+公钥指纹）
   │                                  │
   │ 3. 手机扫码 → GET /ws?pair=<code> │
   │ 4. 服务端校验（一次性、未过期）      │
   │ 5. 通过 → 铸 device token(scope=control)
   │    code 立即作废 ───────────────▶ 手机持久化 token
```

### 5.2 流程 B：日常连接与重连

```
手机 → WSS（Authorization: Bearer <device token>）
  → 服务端验签 + 查设备表（未被撤销）
  → 分配 ClientId → 注册到 Broadcaster
  → initialize 握手（该连接独立）
  → thread/listAll 拉列表 → thread/resume 看历史 → 订阅增量
断线：重连 → 重新 initialize → thread/resume 增量回放
```

### 5.3 流程 C：远程审批（先到先得）

```
agent 触发危险命令
  → 反向请求 item/toolCall/requestApproval { perm_id, tool, input }
  → Broadcaster 扇出到 [桌面 local, 手机 A, 手机 B]
  → 手机 A 先点"允许" → ClientResponse{ id: perm_id, result: allow }
  → 命中 pending[perm_id] → 回传 Decision 给 agent
  → 广播 item/toolCall/approvalResolved { perm_id, by:"ws-<A>", decision }
  → 桌面端/手机 B 收到后关闭弹窗（"已由手机 A 处理"）
无人应答 → 超时（沿用 PERMISSION_TIMEOUT=300s）→ Deny
```

### 5.4 安全边界

| 边界 | 做法 |
| --- | --- |
| 传输加密 | WSS/TLS；自建中继场景可再叠端到端（中继只见密文，v1.1） |
| 认证 | 设备 token（短期 + 可续期），签名（HMAC/Ed25519），服务端存哈希 |
| 授权 | scope：`observe` < `control` < `admin`；**新配对设备默认 `control`**（决策 A） |
| 危险操作 | `admin` 类（`thread/delete`、`process/kill`、`thread/setPermissionMode`=yolo）在 `control` 下返回 `-32014`，引导去桌面端授权 |
| 撤销 | `device/revoke` 立即断开该设备 ws 并使其 token 失效 |
| 失败安全 | 审批超时/无效响应一律 `Deny`（沿用现状） |
| 审计 | 手机端触发的 `turn/start`、审批决定、`process/kill` 打 `tracing` 带 `client_id` |

---

## 6. 错误处理

原则：**一个客户端的故障绝不拖垮服务或其他客户端**。

| 场景 | 处理 |
| --- | --- |
| WS 传输错误 / 客户端断连 | 只摘除该 `ClientId`；`broadcast` 遇失效订阅静默剔除 |
| 畸形帧 | 沿用 `-32700`，**只回该客户端**，连接不断（与 stdio 一致） |
| 认证失败 / token 过期 | 关闭该连接（WS close code 4401），手机端提示重新配对 |
| token 被撤销（会话中） | 服务端主动关闭该设备连接 |
| 慢消费者（广播积压） | 队列超限即**断开该客户端**，不阻塞其他端 |
| 双端审批 | 后到者为 no-op 成功；两端都收到 `approvalResolved` |
| 审批超时 / 无效 | `Deny` |
| 服务重启 | 客户端自动重连 → 重新 `initialize` → `thread/resume` 增量回放 |

---

## 7. 测试策略

对齐项目既有分级。

**Tier 0（单元，mock）**
- `Broadcaster`：注册 / 扇出 / `reply` 定向 / 失效剔除 / 慢消费者断开
- `pairing`：码一次性、过期拒绝、scope 绑定
- 设备存储：落盘、撤销即失效
- 审批：双答只生效一次、`approvalResolved` 广播、超时 Deny

**Tier 0（集成，mock provider）**
- `run_with` 注入双逻辑客户端：断言通知扇出、响应只回发起者、scope 不足返回 `-32014`
- 现有 182 个 app-server 测试**全绿**（stdio 零回归门禁）

**Tier 1（WS E2E）**
- 起真实 ws listener + 测试客户端：`initialize` → `thread/listAll` → `thread/resume`
  → `turn/start` → 断言流式通知与审批闭环

**前端**
- 移动客户端状态机单测（配对、token 刷新、断线重连、`approvalResolved` 关弹窗）
- 桌面端 206 个前端测试**全绿**（共享 `protocol.ts` 抽包不得改变现有行为）

**验证命令**
```
cargo test -p yi-agent-app-server
cargo test -p yi-agent --bin yi-agent stdio
cd desktop && npx vitest run && npx tsc --noEmit
```

---

## 8. 迁移与兼容

- `--listen` **默认仍是 `stdio://`**，桌面端行为与今天完全一致；`ws://` 是显式 opt-in。
- 协议**纯增量**：复用全部既有方法，仅新增 `pair/*`、`device/*` 与 `approvalResolved`。
- `PROTOCOL_VERSION`（`protocol.rs:6`，当前 1）通过 `initialize` 的 capabilities 暴露新增能力。
- 中继接入：电脑端以"出站 WSS 客户端"模式连中继（`--relay wss://...`），
  不开放入站端口；详见 §11。

---

## 9. 分阶段交付

- **Tier 0（已计划）**：仅加 `--listen ws://`，**单客户端**跑通（浏览器能连、能看流式输出）。
  证明换传输可行，不动多客户端逻辑。计划见
  `docs/superpowers/plans/2026-10-02-app-server-websocket-transport.md`。
- **Tier 1（可用）**：Broadcaster 多客户端 + 配对/设备 + 反向 WSS 中继 + iOS App（Tauri 2）。
  达到"手机与电脑同屏一致、手机能审批/发消息/打断"。
- **Tier 1.1**：APNs 推送、中继端到端加密、按 thread 订阅过滤。
- **Tier 2+（另立项）**：Android App、IM 机器人、多用户协作。

---

## 10. 待决策 / 开放问题

1. iOS App 与桌面端共享前端代码的方式（Tauri 2 单仓 `desktop/` 加 iOS target，
   vs 抽出 `shared/` 前端包）——留实现计划定。
2. 设备 token 的签名算法选型（HMAC vs Ed25519）——留实现计划定。
3. 中继的部署形态（独立 crate `yi-agent-relay` vs 直接复用 `yi-agent-web` 扩展）——留实现计划定。

---

## 11. 反向 WSS 中继与推送（v1 新增章节）

### 11.1 中继职责

中继是一个**极薄的转发服务**，不做业务：

| 做 | 不做 |
| --- | --- |
| 按配对会话把 App 与电脑的帧互相转发 | 不解析 JSON-RPC 方法语义 |
| 维护"电脑在线"注册表（哪台电脑、哪个 relay session） | 不落库明文会话内容 |
| TLS 终止（wss） / 设备认证 / 速率限制 | 不执行任何 agent 逻辑 |
| 承担 APNs 推送（v1.1） | 不代理文件系统访问 |

方向：**两端都出站**。电脑侧跑一个 relay 客户端（把 app-server 的 ws 连接桥到中继），
App 侧直接连中继。中继只关心"会话 A 的消息转给会话 B 的对端"。

### 11.2 配对与凭证（跨中继）

沿用 §5 的配对流程，新增两段：

- `pair/create` 现在产出 `{ code, relay_url }`（不再是局域网 `ws_url`）；
- 中继侧也校验同一配对码：App 用 code 连中继时，中继向电脑侧要"这个 code 是否有效"，
  或由电脑侧在创建时把 code 的哈希注册到中继（两者取一，留实现计划定）。

### 11.3 推送（APNs）

- **必须由服务端签发**：笔记本上的 app-server 发不了 APNs，因此由**中继**承担。
- 触发点：`thread/status/updated → awaiting_approval`、`turn/completed`。
- 电脑侧把这些事件以最小 payload 送给中继（含 device push token 关联），中继走 APNs。
- **隐私边界**：payload 只含"哪个会话在等你"，**不含命令内容**；详情在 App 打开后经
  WSS 拉取。
- 需要：Apple Developer 账号（$99/年）、APNs 密钥（.p8）、App 端 `UNUserNotificationCenter` 注册。

### 11.4 中继相关风险

| 风险 | 缓解 |
| --- | --- |
| 中继成为单点故障 | 中继无状态（会话注册表可重建）；电脑侧断线重连 + App 侧 `thread/resume` 增量回放 |
| 中继运维成本 | v1 单实例起步；接口极薄，可水平扩展 |
| 明文经过中继 | v1.1 加端到端加密（中继只见密文）；v1 靠 TLS |
| 域名/证书 | `wss://` 需要有效证书，用 Let's Encrypt 自动签发 |
| 电脑侧要常驻 | 用户需保持 app-server（或 relay 客户端）运行；可选登录项自启 |

