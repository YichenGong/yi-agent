# 桌面与手机共用同一 app-server 会话（stdio + relay 合一）

**状态：** 设计（待评审）
**日期：** 2026-10-03
**相关：** `docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md`（父 spec）、`docs/relay-deploy.md`

## 1. 背景与问题

父 spec 的目标是「手机看到电脑上的**同一份** session 并互动」。目前 [[Tier 1]] 已落地：
ws 传输、Broadcaster 多客户端扇出、配对与设备表、反向 WSS 中继、iOS 构建与扫码。
但**「同一份 session」这一条尚未真正打通**。

### 1.1 现状（已核实）

`app-server` 同一时刻只跑**一种**传输，两条入口各自建一个 `serve()` 主循环：

| 入口 | 代码路径 | 主循环 | 谁在连 |
| --- | --- | --- | --- |
| `--listen stdio://`（默认） | `run_app_server` → `server::run` → `serve_stdio`/`serve_scoped` | 自己的 `serve()` | 桌面 GUI（stdout/stdin） |
| `--relay <url>`（`relay://`） | `run_app_server` → `run_relay_mode` → `serve_ws`（回环） | **另一个** `serve()` | 手机（经中继） |

桌面 GUI 的侧车是 `app-server --listen stdio://`（`desktop/src-tauri/src/bridge.rs:spawn_once`），
**不带 `--relay`**；而 `run_relay_mode` 起的是一个**独立进程级 `serve()`**。

后果：

1. **不是同一个 session**：桌面 GUI 与手机各连一个 `serve()`，各自持有
   `threads: HashMap<ThreadSession>`、`runtimes`、`thread_roots`、`pending` 等在**内存**里
   的会话状态。二者只在 thread_store 落盘这一层间接一致，**实时视图与推送互不相通**。
2. 手机看不到桌面正在跑的 turn 的**实时事件**；桌面也看不到手机发起的 turn。
3. 需要手工另起一个带 `--relay` 的 app-server 才能让手机有事可看（运维负担）。

### 1.2 关键发现：核心 `serve()` 本就是多客户端的

`serve_scoped`（stdio）与 `serve_ws_inner`（ws）**都调用同一个** `server::serve`：

- `serve_scoped`：`hub.register_reliable(local)` + `read_lines` + `pump_stdout` →
  `serve(inbound_rx, hub, … client_scopes={local→Admin}, client_initialized)`。
- `serve_ws_inner`：自建 `hub` + `inbound_tx/rx` + `client_scopes`/`client_initialized`/
  `device_clients`，ws 路由把每条连接登记为 `ws-<uuid>`(Control) → **同样**调 `serve(...)`。

即 `serve()` 已经支持多 `ClientId`、按 `ClientId` 路由/扇出、按 scope 门禁。
**缺的不是多客户端能力，而是「让 stdio 与 ws 共用同一个 `serve()` 循环与同一套 hub/表」。**

## 2. 目标与非目标

### 2.1 目标（v1）

- `app-server --listen stdio:// --relay <url>`：**一个进程、一个 `serve()` 循环**，同时服务
  - stdio 客户端（桌面 GUI，`ClientId="local"`，Admin），与
  - 经中继接入的手机客户端（`ws-<uuid>`，Control）。
- 二者共享同一 `hub`：任一侧的 turn 事件、审批请求、`thread/status/updated` **扇出到双方**。
- 桌面在配置了中继地址时**自动**以该模式启动侧车（无需手工起第二个进程）。
- **stdio 零回归**：不带 `--relay` 时，stdio 路径与今日**逐字节一致**。

### 2.2 非目标（YAGNI）

- 端到端加密、多用户/多租户、把桌面 GUI 改成 ws 客户端。
- 让 sidecar 在桌面 GUI 退出后继续存活（手机<->桌面同生共死，符合"控制你自己的电脑"）。
- APNs 推送（父 spec 已有章节，另议）。

## 3. 架构

### 3.1 核心思路：把 ws 前端「接」到既有的 `serve()` 上

把 `serve_ws_inner` 拆成两半：

1. **`attach_ws_frontend(...)`（新）**：只负责「监听 + 路由 + 每连接的握手/鉴权/升级 + 把该连接的
   帧送入**给定的** inbound channel、把出站登记进**给定的** hub、把 scope/initialized 写进
   **给定的**表」。**它不调用 `serve()`。**
2. 两个调用方各自决定 hub/表/channel 从哪来：
   - 纯 ws 模式（`serve_ws`）：自建 hub/channel/表 → `attach_ws_frontend` → `serve()`。
   - **合并模式（新）**：自建 hub/channel/表 → 挂 stdio 前端(`local`) + `attach_ws_frontend`
     (回环) + 起 relay 客户端 → **一个** `serve()`。

```
桌面 GUI ──stdio──┐
                  │        ┌───────────────────────────────┐
手机 ──wss──▶ 中继 ──wss──▶│ yi_agent_relay::run_client     │
                  │        └──────────────┬────────────────┘
                  │                       │ ws (仅回环 127.0.0.1:0)
                  │        ┌──────────────▼────────────────┐
                  └───────▶│ attach_ws_frontend (回环)      │
                           └──────────────┬────────────────┘
        stdio read_lines(local) ──────────┤ 同一个 inbound channel
                                          ▼
                            ┌──────────────────────────┐
                            │  server::serve()  ← 唯一循环 │
                            │  hub / scopes / initialized │
                            │  threads / runtimes / ...   │
                            └──────────────────────────┘
```

### 3.2 不变量

1. **stdio 零回归**：`--relay` 缺省时，`serve_stdio` 的注册方式（`register_reliable`）、
   退出语义（EOF 后 drain `pump_stdout`）、错误语义（写失败即失败）**不变**。
2. **单一事实来源**：合并后只有一个 `serve()`，`threads` 等内存会话状态只有一份。
3. **scope 不变**：stdio=`Admin`，手机=`Control`；既有门禁测试全绿。
4. **生命周期**：stdio EOF（桌面 GUI 退出）⇒ 整个合并进程优雅退出，手机断连。这是刻意的：
   桌面关了，手机没东西可控。
5. **回环不暴露**：本地 ws 仍绑 `127.0.0.1:0`；中继客户端仍以 `seed_local_device` 铸的
   token 认证；不新增入站端口。

## 4. 接口

### 4.1 `ws.rs`

```rust
/// 只挂 ws 前端：监听 + 路由 + 每连接握手/鉴权/升级，把帧送进**给定**的
/// `inbound_tx`，把出站登记进**给定**的 `hub`，scope/initialized/devices 用**给定**的表。
/// **不**调用 `serve()`——调用方负责用同一个 hub/channel 驱动唯一的 `serve()`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn attach_ws_frontend(
    listener: tokio::net::TcpListener,
    hub: Arc<Broadcaster>,
    inbound_tx: tokio::sync::mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    client_scopes: ClientScopes,
    client_initialized: ClientInitialized,
    device_clients: WsDeviceRegistry,
    pairing: Arc<PairingState>,
) -> tokio::task::JoinHandle<anyhow::Result<()>>;
```

`serve_ws_inner` 改为：建 hub/channel/表 → `attach_ws_frontend(...)` → `serve(...)`（行为不变）。

### 4.2 新入口：`server::run_stdio_with_relay`

在 `server.rs` 增加一个合并入口（与 `serve_stdio` 同构，额外挂 ws 前端与 relay 客户端）：

```rust
/// 合并模式：stdio(local/Admin) + 回环 ws(手机/Control) 共享**一个** `serve()`。
pub(crate) async fn serve_stdio_with_relay<R, W, F>(
    reader: R, writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    attachments: RuntimeAttachments,
    build_agent: F,
    relay_url: String,
) -> anyhow::Result<()>
where /* 同 serve_stdio */;
```

体内：建 `hub` + `inbound_tx/rx` + `client_scopes{local→Admin}` + `client_initialized` +
`device_clients`；`register_reliable(local)`；起 `pump_stdout` + `read_lines(local)`；
建回环 `TcpListener` 并 `attach_ws_frontend`；`seed_local_device("relay-bridge")` 铸 token；
起 `yi_agent_relay::run_client(relay_url, loopback_ws, session, token)`；调用唯一的 `serve()`。
`stdio EOF` ⇒ `serve()` 返回 ⇒ 摘除 `local`、`await pump`、abort ws 前端与 relay 任务。

### 4.3 CLI（`yi-agent/src/main.rs`、`config.rs`）

`--relay` 与 `--listen stdio://` **可组合**：

- `--relay <url>` 且 `--listen` 为 `stdio://`（含缺省）⇒ 合并模式 `serve_stdio_with_relay`。
- `--relay <url>` 且未显式给 `--listen`：语义仍为**合并**（桌面即默认 stdio）——这是个**行为变更**，
  见 §6。
- `--listen relay://…`（旧写法，无 stdio）⇒ 保持 `run_relay_mode`（纯中继，无 GUI）。

### 4.4 桌面（`desktop/src-tauri/src/bridge.rs`）

`spawn_once` 在**配置了中继地址**时追加 `--relay <url>`。中继地址来源：桌面设置
（复用 `settings_store` / 远端 Tab 的既有配置），缺省不带（= 今日行为）。

## 5. 测试策略

- **单测（Rust）**
  1. `attach_ws_frontend` 拆分后，既有 ws 套件**全绿**（行为不变门禁）。
  2. 合并模式：一个 stdio 客户端 + 一个经回环 ws 的客户端，`thread/listAll` 看到**同一**
     thread 集合；stdio `thread/start` 后，ws 侧收到 `thread/started`/status 通知（扇出证明）。
  3. 合并模式：手机(Control) 调 Admin-only RPC 仍被拒（scope 门禁不变）。
  4. stdio EOF ⇒ `serve()` 退出、`pump_stdout` drain、ws/relay 任务终止（生命周期）。
  5. 回归：`--listen stdio://`（无 relay）与今日逐字节一致（既有 182+ 测试）。
- **集成（端到端）**：起 `app-server --listen stdio:// --relay ws://127.0.0.1:<port>/connect?session=x`
  + 一个真中继；stdio 侧 `thread/start`，ws 侧（带配对 token）收到同一 thread 的实时通知。

## 6. 行为变更与兼容

- `app-server --relay <url>`（不给 `--listen`）从「纯中继」变为「stdio+中继」。**兼容风险**：
  现有 `relay-deploy.md` 文档与脚本以 `--relay` 起「无 GUI 的纯中继」。处理：
  - 纯中继改用 `--listen relay://<url>`（旧写法保留且行为不变）。
  - `--relay` 的语义变更写进 CHANGELOG 与 `relay-deploy.md`。
- 若不愿变更 `--relay` 语义，替代方案：新增 `--with-relay <url>`（与 `--relay` 并存）。
  **本 spec 采用前者**（`--relay` 可组合），理由：桌面侧车写一行参数即可，语义更直觉。

## 7. 分阶段

1. 拆 `attach_ws_frontend`（零行为变更）+ 回归门禁。
2. 加 `serve_stdio_with_relay` + 单测（扇出 / scope / 生命周期）。
3. CLI 组合 + `run_app_server` 分派。
4. 桌面 `bridge.rs` 按配置追加 `--relay`。
5. 端到端（真中继）验证 + 文档更新。

## 8. 开放问题

- 中继地址在桌面侧的**存储与配置入口**（设置页字段？配对时下发？）。
- 手机与桌面**同时**发起 turn 的冲突语义（父 spec 已定「单一事实来源」，但 UI 提示另议）。
- 断线重连时 ws 客户端是否需要重放未读（可与父 spec 的订阅窗口合并讨论）。
