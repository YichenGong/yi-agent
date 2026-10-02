# App-Server WebSocket 传输 实现计划（Tier 0）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `yi-agent app-server` 支持 `--listen ws://host:port`，使网络客户端（手机 PWA 的前置条件）能连上并跑通完整的 JSON-RPC 会话，同时 stdio 路径行为逐字节不变。

**Architecture:** 把 `server.rs` 中传输无关的主循环抽成 `serve(inbound, hub, ...)`；`Broadcaster`（扇出中心）接管所有出站帧。stdio 成为"只有一个 `local` 客户端"的传输实现，WS 成为第二个传输实现。Tier 0 的 WS 只接受**一个**连接，且**不含认证**，因此默认只绑定 `127.0.0.1`。多客户端扇出、配对/设备、scope、移动 UI 属于 Tier 1，另立计划。

**Tech Stack:** Rust 2024 / tokio（workspace 已 `features = ["full"]`）/ `axum 0.8`（需启用 `ws` feature）/ `serde_json` / 测试用 `tokio-tungstenite`（dev-dependency）。

**Spec:** `docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md`（§3 架构、§4.1 Broadcaster、§4.2 ws 传输、§4.4 server 改动、§6 错误处理、§8 兼容、§9 Tier 0）。

## Global Constraints

- **默认传输不变**：`--listen` 缺省仍是 `stdio://`；未显式传 `ws://` 时，桌面端 sidecar 行为与今天完全一致。
- **stdio 零回归**：现有 182 个 `cargo test -p yi-agent-app-server` 测试必须全绿，这是每个任务的门禁。
- **Tier 0 的 WS 无认证**：`ws://` 只绑定 `127.0.0.1`；绑定其它地址需显式 `--listen ws://0.0.0.0:PORT`，且 CLI help 必须写明"Tier 0 无认证，请勿暴露公网"。
- **协议纯增量**：Tier 0 不新增任何 RPC 方法；只换传输。
- **Tier 0 单客户端**：`ws://` 同时只接受一个连接；第二个连接被拒绝（close）。
- **fmt**：在 `yi-agent-rs/` 下 `cargo fmt --all`，提交前必须跑。
- **分支**：在 worktree 上作业，禁止在 `main` 提交；commit 用 conventional commits，不写 `Co-Authored-By`。
- 验证命令统一在 worktree 根目录下的 `yi-agent-rs/` 执行。

---

## 文件结构

| 文件 | 责任 | 动作 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs` | 扇出中心：ClientId、注册/注销、广播/定向回复 | 新建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 导出新模块 | 修改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 抽出 `serve()`、`Broadcaster` 接线、纯传输化出站 | 修改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` | WebSocket 传输（单连接） | 新建 |
| `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` | `axum` ws feature + `tokio-tungstenite` dev-dep | 修改 |
| `yi-agent-rs/crates/yi-agent/src/config.rs` | `--listen` 的 help 文案 | 修改 |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | `parse_listen` 取代 `ensure_stdio_listen`，分派 stdio/ws | 修改 |
| `README.md`、`docs/project-management/yi-agent-app-server.md`、`docs/project-management/desktop.md` | 文档：新传输、Tailscale 直连步骤、路线图状态 | 修改 |

---

## Task 1: Broadcaster（扇出中心）

独立、可单测，是 Tier 1 多客户端的地基。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（加 `pub mod broadcast;`）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `broadcast::ClientId`（`Clone + PartialEq + Eq + Hash`），构造子 `ClientId::local() -> Self`、`ClientId::ws(uuid::Uuid) -> Self`、`as_str(&self) -> &str`
  - `broadcast::Broadcaster::new() -> Self`
  - `Broadcaster::register(&self, ClientId) -> tokio::sync::mpsc::Receiver<serde_json::Value>`
  - `Broadcaster::unregister(&self, &ClientId)`
  - `Broadcaster::client_count(&self) -> usize`
  - `Broadcaster::broadcast(&self, serde_json::Value)`
  - `Broadcaster::reply(&self, &ClientId, serde_json::Value) -> Result<(), broadcast::Closed>`（`async`）

- [ ] **Step 1: 写失败的测试**

创建 `broadcast.rs`，先只放测试骨架与 `unimplemented!` 的类型占位不必要——直接写测试，编译器会因类型缺失而失败：

```rust
//! 服务端 → 客户端的扇出中心。
//!
//! 所有出站帧（通知、反向请求、响应）都经此转发。stdio 传输只注册一个
//! `local` 客户端，语义与改造前的单流写一致；WS 传输注册多个客户端并扇出。

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use serde_json::Value;
use tokio::sync::mpsc;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn broadcast_reaches_every_registered_client() {
        let hub = Broadcaster::new();
        let mut a = hub.register(ClientId::ws(uuid::Uuid::nil()));
        let mut b = hub.register(ClientId::local());
        hub.broadcast(serde_json::json!({"method": "ping"}));
        assert_eq!(a.recv().await.unwrap()["method"], "ping");
        assert_eq!(b.recv().await.unwrap()["method"], "ping");
    }

    #[tokio::test]
    async fn reply_targets_only_the_addressed_client() {
        let hub = Broadcaster::new();
        let id_a = ClientId::ws(uuid::Uuid::nil());
        let mut a = hub.register(id_a.clone());
        let mut b = hub.register(ClientId::local());
        hub.reply(&id_a, serde_json::json!({"id": 7})).await.unwrap();
        assert_eq!(a.recv().await.unwrap()["id"], 7);
        // b 不应收到任何东西。
        assert!(tokio::time::timeout(Duration::from_millis(50), b.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reply_to_an_unknown_client_is_closed() {
        let hub = Broadcaster::new();
        assert!(hub.reply(&ClientId::local(), serde_json::json!({})).await.is_err());
    }

    #[tokio::test]
    async fn unregister_stops_delivery() {
        let hub = Broadcaster::new();
        let id = ClientId::local();
        let mut rx = hub.register(id.clone());
        hub.unregister(&id);
        assert_eq!(hub.client_count(), 0);
        hub.broadcast(serde_json::json!({"method": "ping"}));
        assert!(tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_slow_consumer_is_dropped_without_blocking_others() {
        let hub = Broadcaster::new();
        let slow_id = ClientId::ws(uuid::Uuid::nil());
        let _slow = hub.register(slow_id.clone()); // 从不 recv
        let mut fast = hub.register(ClientId::local());
        // 灌满并超过慢消费者的队列容量。
        for i in 0..(CLIENT_QUEUE + 8) {
            hub.broadcast(serde_json::json!({ "n": i }));
        }
        // 慢消费者被摘除,快消费者仍能收到最后一帧。
        assert_eq!(hub.client_count(), 1);
        let last = fast.recv().await.unwrap();
        assert_eq!(last["n"], CLIENT_QUEUE + 7);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib broadcast::`
Expected: 编译失败（`Broadcaster`/`ClientId`/`CLIENT_QUEUE` 未定义）。

- [ ] **Step 3: 实现最小代码**

在 `broadcast.rs` 测试模块之前加入实现：

```rust
/// 每个客户端的出站队列容量。写满即视为慢消费者,`broadcast` 会摘除它
/// 而不是阻塞其它客户端。
pub const CLIENT_QUEUE: usize = 256;

/// 定向回复失败:客户端已注销或其出站队列已关闭。
#[derive(Debug, PartialEq, Eq)]
pub struct Closed;

/// 一个已连接客户端的身份。
///
/// stdio 传输固定为 `local`(全进程唯一);WS 传输每个连接一个 `ws-<uuid>`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(String);

impl ClientId {
    /// stdio 传输的唯一客户端。
    pub fn local() -> Self {
        Self("local".to_string())
    }

    /// 一个 WS 连接。
    pub fn ws(uuid: uuid::Uuid) -> Self {
        Self(format!("ws-{uuid}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 服务端 → 客户端的扇出中心。
///
/// 锁只在插入/摘除/取 sender 时短暂持有,绝不跨 `.await`,因此 `broadcast`
/// 与 `reply` 可以并发调用。
pub struct Broadcaster {
    clients: StdMutex<HashMap<ClientId, mpsc::Sender<Value>>>,
}

impl Default for Broadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl Broadcaster {
    pub fn new() -> Self {
        Self {
            clients: StdMutex::new(HashMap::new()),
        }
    }

    /// 注册一个客户端,返回它专属的出站接收端。
    pub fn register(&self, id: ClientId) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        rx
    }

    /// 摘除一个客户端(断连时)。
    pub fn unregister(&self, id: &ClientId) {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }

    /// 当前已注册客户端数(测试与单连接准入用)。
    pub fn client_count(&self) -> usize {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// 广播给所有客户端。
    ///
    /// 失效或队列已满(慢消费者)的订阅者会被立即摘除——这是**背压**而非无限
    /// 缓冲:一个连不上的手机不能让主循环卡住。
    pub fn broadcast(&self, frame: Value) {
        let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, tx| tx.try_send(frame.clone()).is_ok());
    }

    /// 只发给指定客户端。
    ///
    /// 与 `broadcast` 不同,这里 `await` 到入队成功为止:stdio 传输依赖它保留
    /// “写阻塞直到对端读”(改造前 `write_all(...).await` 的语义)。客户端已
    /// 注销或队列关闭时返回 `Err(Closed)`,调用方据此终止会话。
    pub async fn reply(&self, id: &ClientId, frame: Value) -> Result<(), Closed> {
        let tx = self
            .clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned();
        match tx {
            Some(tx) => tx.send(frame).await.map_err(|_| Closed),
            None => Err(Closed),
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib broadcast::`
Expected: PASS（5 个测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/broadcast.rs crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): add Broadcaster fan-out centre"
```

---

## Task 2: 抽出传输无关的 serve()，出站改走 Broadcaster

纯重构：行为不变，门禁是现有测试全绿。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 的 `Broadcaster`、`ClientId`、`Closed`。
- Produces（供 Task 3 复用）:
  - `async fn serve<F>(inbound: mpsc::Receiver<(ClientId, anyhow::Result<String>)>, hub: Arc<Broadcaster>, cfg: RuntimeConfig, permission_timeout: Duration, workspaces: Arc<WorkspaceIndex>, attachments: RuntimeAttachments, build_agent: F) -> anyhow::Result<()>`
  - `fn production_factory(cfg: RuntimeConfig) -> impl Fn(Option<yi_agent_core::Session>, &Path, crate::thread_store::ThreadMode) -> anyhow::Result<BuiltAgent> + Send + 'static`
  - `async fn read_lines<R: AsyncRead + Unpin>(reader: R, client: ClientId, tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>)`
  - `async fn pump_stdout<W: AsyncWrite + Unpin>(outbound: mpsc::Receiver<Value>, writer: W, tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>, client: ClientId)`

- [ ] **Step 1: 建立基线（现有套件必须已全绿）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（182 个测试，0 failed）。记录这个数字；重构后必须仍是 182 且全绿。

- [ ] **Step 2: 从 `run()` 抽出 `production_factory`**

`run<R, W>` 目前把 agent 工厂闭包内联在 `run_with(...)` 调用里（`server.rs:784-810` 的 `move |session, cwd, mode| { ... }`）。把该闭包整体搬到一个新函数，`run` 只负责建 hub/inbound/stdio 传输：

```rust
/// 生产环境的 agent 工厂:按 thread 的 cwd 覆盖 workdir 与 yolo 后引导一个 agent。
fn production_factory(
    cfg: RuntimeConfig,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ 'static {
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &thread_cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )?;
        let config = built.agent.config().clone();
        Ok(BuiltAgent {
            agent: apply_session(built.agent, session),
            provider: built.provider,
            config,
            decision_tx: built.decision_tx,
            decision_rx: built.decision_rx,
            catalog: built.catalog,
            yolo: built.yolo,
            process_manager: built.process_manager,
        })
    }
}
```

`run` 的新实现：

```rust
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let local = crate::broadcast::ClientId::local();
    let outbound = hub.register(local.clone());
    let (inbound_tx, inbound_rx) = mpsc::channel::<(crate::broadcast::ClientId, anyhow::Result<String>)>(64);
    tokio::spawn(pump_stdout(outbound, writer, inbound_tx.clone(), local.clone()));
    tokio::spawn(read_lines(reader, local, inbound_tx));
    serve(
        inbound_rx,
        hub,
        cfg.clone(),
        PERMISSION_TIMEOUT,
        workspaces,
        RuntimeAttachments { runtimes, thread_roots },
        production_factory(cfg),
    )
    .await
}
```

- [ ] **Step 3: 加入 `read_lines` 与 `pump_stdout`**

```rust
/// 从一条 `AsyncRead` 逐行读取并送入主循环。
///
/// `MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,因此由本任务
/// 独占 reader,主循环只 select cancel-safe 的 channel `recv`。
async fn read_lines<R>(
    reader: R,
    client: crate::broadcast::ClientId,
    tx: mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = MessageReader::new(reader);
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                if tx.send((client.clone(), Ok(line))).await.is_err() {
                    break;
                }
            }
            Ok(None) => break, // EOF → 丢弃 tx → 主循环优雅退出
            Err(e) => {
                tracing::error!("app-server read error: {e}");
                let _ = tx.send((client.clone(), Err(e))).await;
                break;
            }
        }
    }
}

/// 把该客户端的出站帧写到一条 `AsyncWrite`(stdio 场景即 stdout)。
///
/// 写失败时回送一个 `Err`,让主循环按**改造前**的语义退出——`write_response(..)?`
/// 曾把写失败直接变成会话错误,这条路径必须保留。
async fn pump_stdout<W>(
    mut outbound: mpsc::Receiver<serde_json::Value>,
    writer: W,
    tx: mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    client: crate::broadcast::ClientId,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut writer = MessageWriter::new(writer);
    while let Some(frame) = outbound.recv().await {
        if let Err(e) = writer.write_value(&frame).await {
            let _ = tx
                .send((client.clone(), Err(anyhow::anyhow!("stdout write failed: {e}"))))
                .await;
            break;
        }
    }
}
```

- [ ] **Step 4: 把 `run_with` 改名并改签名为 `serve`**

`run_with` 的头（`server.rs:821-846`）改为：

```rust
async fn serve<F>(
    mut inbound: mpsc::Receiver<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    hub: Arc<crate::broadcast::Broadcaster>,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    attachments: RuntimeAttachments,
    build_agent: F,
) -> anyhow::Result<()>
where
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + 'static,
{
```

删除原来在 `serve` 内部的 `req_tx/req_rx` 与 reader spawn（`server.rs:846-861`），因为读取已由传输层负责；同时删除 `let writer = Arc::new(MessageWriter::new(writer));`（`server.rs:862`），改用传入的 `hub`。

- [ ] **Step 5: 改主循环的入站解构**

`loop { tokio::select! { line = req_rx.recv() => { ... } } }` 的头部改为：

```rust
        let Some((client, item)) = inbound.recv().await else { break }; // EOF
```

注意：`select!` 现在只剩一个分支（`inbound`）时可直接用 `let ... else`；若 `select!` 还有其它分支（`turn_rx`）则保留 `select!`，只把分支改为：

```rust
            line = inbound.recv() => {
                let Some((client, item)) = line else { break };
                // ... 原有 Ok/Err/parse 分支,凡写响应处改用 write_response(&hub, &client, ..)
            }
```

- [ ] **Step 6: 出站调用点全量迁移（编译器驱动）**

改两个 helper 的签名，然后让编译器找出所有调用点：

```rust
async fn write_response(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(&resp)
        .map_err(|e| anyhow::anyhow!("failed to serialize response: {e}"))?;
    hub.reply(client, frame)
        .await
        .map_err(|_| anyhow::anyhow!("client {} disconnected", client.as_str()))
}

async fn write_notification(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(&NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    // 广播给所有客户端:fan-out 是 Tier 1 的能力,但 Tier 0 立刻走同一条路径,
    // 避免以后再改一次接线。单客户端下等价于写那一条流。
    hub.broadcast(frame);
    Ok(())
}
```

然后**编译并逐个修复**（`cargo build -p yi-agent-app-server 2>&1 | grep -E "^error"`）：
- `write_response(&writer, X)` → `write_response(&hub, &client, X)`
- `write_notification(&writer, &X)` → `write_notification(&hub, &X)`
- `require_thread_id(&writer, ...)` → `require_thread_id(&hub, &client, ...)`（同步改该 helper 签名，第一个参数换成 `&Broadcaster, &ClientId`）
- `resolve_thread_cwd(&req.params, &cfg, &writer, id)` → `resolve_thread_cwd(&req.params, &cfg, &hub, &client, id)`
- `update_status<W>(writer: &MessageWriter<W>, ...)` → `update_status(hub: &Broadcaster, ...)`；其内部 `write_notification` 调用同步改
- `run_thread_driver(..., driver_writer, ...)`：参数类型 `Arc<MessageWriter<W>>` → `Arc<Broadcaster>`；去掉该函数的 `<W>` 泛型参数
- `watch_processes(Arc::clone(&writer), ...)`：同上去泛型
- `Arc::clone(&writer)` 的所有点 → `Arc::clone(&hub)`

判据：`cargo build -p yi-agent-app-server` 无 error，且 `grep -n "MessageWriter" crates/yi-agent-app-server/src/server.rs` 只剩 import 与 `pump_stdout` 用法。

- [ ] **Step 7: 运行套件确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS，测试数仍为 182，0 failed。

若 `eof_exits_gracefully`、`oversized_frame_returns_err`、`agent_factory_failure_returns_internal_error` 失败，检查：`pump_stdout` 的写失败是否回送了 `Err`（Step 3）、`read_lines` 的 EOF 是否正确丢弃 sender。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/server.rs
git commit -m "refactor(app-server): route outbound frames through Broadcaster"
```

---

## Task 3: WebSocket 传输（单连接）+ `--listen ws://`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（`pub mod ws;`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（把 `production_factory` 设为 `pub(crate)`）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`parse_listen` + 分派）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` 内的 E2E

**Interfaces:**
- Consumes: Task 2 的 `serve`、`production_factory`、`RuntimeAttachments`、`PERMISSION_TIMEOUT`。
- Produces:
  - `ws::serve_ws(listener: tokio::net::TcpListener, cfg: RuntimeConfig, workspaces: Arc<WorkspaceIndex>) -> anyhow::Result<()>`
  - `yi_agent::config` 与 main 内的 `enum Listen { Stdio, Ws(std::net::SocketAddr) }`
  - `main.rs::parse_listen(&str) -> anyhow::Result<Listen>`

- [ ] **Step 1: 加依赖并对齐 tungstenite 版本**

`Cargo.toml`：

```toml
[dependencies]
# ...既有依赖...
axum = { workspace = true, features = ["ws"] }

[dev-dependencies]
async-trait = "0.1"
tempfile = "3"
tokio-tungstenite = "0.26"
```

Run: `cd yi-agent-rs && cargo tree -p axum | grep -i tungstenite`
Expected: 打印 axum 依赖的 tungstenite 版本。若与 `0.26` 主版本不一致，把 dev-dep 改成同一主版本，再 `cargo build -p yi-agent-app-server`。若 axum 的 ws feature 需要额外开启（例如 `tokio-tungstenite` 的 `connect`），按 `cargo` 报错补齐——以 `cargo build` 通过为准。

- [ ] **Step 2: 写失败的 E2E 测试**

创建 `ws.rs`，先只写测试：

```rust
//! WebSocket 传输(Tier 0,单连接)。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::broadcast::{Broadcaster, ClientId};
use crate::protocol::MAX_FRAME_BYTES;
use crate::server::{production_factory, serve, RuntimeAttachments, PERMISSION_TIMEOUT};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_runtime::config::RuntimeConfig;

#[cfg(test)]
mod tests {
    use super::*;

    /// 起一个只监听 127.0.0.1 的 ws server,返回它的地址与后台句柄。
    async fn spawn_ws(cfg: RuntimeConfig) -> (SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>) {
        // workdir 与 workspace 索引都落在同一个临时目录里,并刻意让它活到进程
        // 结束(`mem::forget`):索引必须隔离,否则会读写用户真实的
        // `~/.yi-agent/workspaces.json`。
        let dir = tempfile::TempDir::new().unwrap();
        let workdir = dir.path().to_path_buf();
        let index_path = dir.path().join("workspaces.json");
        std::mem::forget(dir);
        let mut cfg = cfg;
        cfg.workdir = workdir;
        let workspaces = Arc::new(WorkspaceIndex::new(index_path));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(serve_ws(listener, cfg, workspaces));
        (addr, handle)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ws_client_completes_a_jsonrpc_handshake() {
        let (addr, handle) = spawn_ws(crate::server::tests_support::test_config()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("ws connect");
        // initialize
        ws.send(Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
        ))
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        // config/read
        ws.send(Message::Text(
            r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#.into(),
        ))
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["id"], 2);
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_connection_is_rejected_while_the_first_is_open() {
        let (addr, handle) = spawn_ws(crate::server::tests_support::test_config()).await;
        let (mut first, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("first connect");
        first
            .send(Message::Text(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
            ))
            .await
            .unwrap();
        let _ = first.next().await; // 等它注册完成
        let second = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await;
        match second {
            Ok((mut s, _)) => {
                // 服务端应随即关闭这条连接。
                let closed = tokio::time::timeout(std::time::Duration::from_secs(2), s.next()).await;
                assert!(
                    matches!(closed, Ok(Some(Ok(Message::Close(_)))) | Ok(None)),
                    "second connection must be closed by the server"
                );
            }
            Err(_) => {} // 握手阶段被拒也可接受
        }
        handle.abort();
    }
}
```

`test_config()` 目前是 `server.rs` 测试模块里的私有函数。为复用，在 `server.rs` 的 `mod tests` **之外**加一个 `pub(crate) mod tests_support { pub(crate) fn test_config() -> RuntimeConfig { ... } }`，内容与现有 `test_config()` 相同（`server.rs:3936`），并让 `mod tests` 内的 `test_config()` 委托它。

- [ ] **Step 3: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib ws::`
Expected: 编译失败（`serve_ws` 未定义）。

- [ ] **Step 4: 实现 `serve_ws` 与 `handle_ws`**

在 `ws.rs` 测试模块之前加入：

```rust
/// 用一个已绑定的 listener 跑 ws 传输,直到进程退出。
///
/// 单个 ws server 共享**一套**主循环(thread 会话、审批表都在其中),因此所有
/// 连接都往同一个入站 channel 灌帧,并共享同一个 `Broadcaster`。
pub async fn serve_ws(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
) -> anyhow::Result<()> {
    let hub = Arc::new(Broadcaster::new());
    let (inbound_tx, inbound_rx) = mpsc::channel::<(ClientId, anyhow::Result<String>)>(64);
    let serve_hub = Arc::clone(&hub);
    // 先克隆出主循环要用的 config,再把 cfg 丢给下面的 router 闭包。
    let serve_cfg = cfg.clone();
    let serve_task = tokio::spawn(async move {
        use std::collections::HashMap;
        use std::sync::Mutex as StdMutex;
        serve(
            inbound_rx,
            serve_hub,
            serve_cfg.clone(),
            PERMISSION_TIMEOUT,
            workspaces,
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
            },
            production_factory(serve_cfg),
        )
        .await
    });

    let app = Router::new().route(
        "/ws",
        get(move |upgrade: WebSocketUpgrade| {
            let hub = Arc::clone(&hub);
            let tx = inbound_tx.clone();
            async move {
                // Tier 0:只接受一个客户端。
                if hub.client_count() > 0 {
                    return upgrade.on_upgrade(|mut socket: WebSocket| async move {
                        let _ = socket.send(Message::Close(None)).await;
                    });
                }
                let id = ClientId::ws(uuid::Uuid::new_v4());
                upgrade.on_upgrade(move |socket| handle_ws(socket, hub, tx, id))
            }
        }),
    );

    tracing::info!(addr = %listener.local_addr()?, "app-server ws listening");
    axum::serve(listener, app).await?;
    serve_task.abort();
    Ok(())
}

/// 一个 WS 连接的生命周期:注册客户端 → 出口泵转发帧 → 入站帧喂主循环。
async fn handle_ws(
    socket: WebSocket,
    hub: Arc<Broadcaster>,
    inbound_tx: mpsc::Sender<(ClientId, anyhow::Result<String>)>,
    id: ClientId,
) {
    let outbound = hub.register(id.clone());
    let (mut sink, mut stream) = socket.split();

    // 出口泵:把该客户端的出站帧写成 Text 帧。
    let pump = tokio::spawn(async move {
        let mut outbound = outbound;
        while let Some(frame) = outbound.recv().await {
            let text = frame.to_string();
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    // 入站:每个 Text/Binary 帧当作一行 JSONL。
    while let Some(Ok(msg)) = stream.next().await {
        let line = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => continue, // 非 UTF-8 帧丢弃,不打断连接
            },
            Message::Close(_) => break,
            _ => continue, // Ping/Pong 由 axum 处理
        };
        if line.len() > MAX_FRAME_BYTES {
            tracing::warn!("ws frame exceeds max size; closing");
            break;
        }
        if inbound_tx.send((id.clone(), Ok(line))).await.is_err() {
            break;
        }
    }

    hub.unregister(&id);
    pump.abort();
}
```

`lib.rs` 增加 `pub mod broadcast;`（Task 1 已加）与 `pub mod ws;`。
`server.rs`：把 `serve`、`production_factory`、`RuntimeAttachments`、`PERMISSION_TIMEOUT` 提升为 `pub(crate)`（`serve` 与 `production_factory` 至少 `pub(crate)`）。

- [ ] **Step 5: 运行 ws 测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib ws::`
Expected: PASS（2 个测试）。

- [ ] **Step 6: 接上 CLI `--listen`**

`config.rs` 的 `AppServer` 变体加 help 说明（保持 `listen: String` 不表）：

```rust
    /// Run the JSON-RPC app-server. Used by the desktop GUI sidecar and, over
    /// `ws://`, by network clients.
    AppServer {
        /// Listen transport: `stdio://` (default) or `ws://host:port`.
        /// The ws transport has NO authentication in this version and should be
        /// bound to loopback only.
        #[arg(long, default_value = "stdio://")]
        listen: String,
    },
```

`main.rs`：用 `parse_listen` 取代 `ensure_stdio_listen`：

```rust
/// 解析 `--listen`。只支持 stdio 与 ws 两种传输。
enum Listen {
    Stdio,
    Ws(std::net::SocketAddr),
}

fn parse_listen(listen: &str) -> Result<Listen> {
    if listen == "stdio://" {
        return Ok(Listen::Stdio);
    }
    if let Some(rest) = listen.strip_prefix("ws://") {
        let addr: std::net::SocketAddr = rest
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid ws address `{rest}`: {e}"))?;
        return Ok(Listen::Ws(addr));
    }
    anyhow::bail!(
        "unsupported app-server transport `{listen}`: expected `stdio://` or `ws://host:port`"
    )
}

fn run_app_server(cli: Cli, listen: &str) -> Result<()> {
    let listen = parse_listen(listen)?;
    let config = config::load(&cli)?;
    let rt = tokio::runtime::Runtime::new()?;
    match listen {
        Listen::Stdio => rt.block_on(yi_agent_app_server::run(
            tokio::io::stdin(),
            tokio::io::stdout(),
            config,
        )),
        Listen::Ws(addr) => rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let bound = listener.local_addr()?;
            if !bound.ip().is_loopback() {
                eprintln!(
                    "warning: ws app-server is bound to {bound} and has NO authentication; \
                     expose it only over a trusted tunnel"
                );
            }
            let workspaces = std::sync::Arc::new(yi_agent_app_server::workspace_index::WorkspaceIndex::new(
                yi_agent_app_server::workspace_index::default_path(),
            ));
            yi_agent_app_server::ws::serve_ws(listener, config, workspaces).await
        }),
    }
}
```

把 `main.rs` 里两个既有测试 `ensure_stdio_listen_accepts_stdio` / `ensure_stdio_listen_rejects_other_transports` 改写为 `parse_listen` 的三例：

```rust
    #[test]
    fn parse_listen_accepts_stdio() {
        assert!(matches!(parse_listen("stdio://").unwrap(), Listen::Stdio));
    }

    #[test]
    fn parse_listen_accepts_ws_with_an_address() {
        match parse_listen("ws://127.0.0.1:8790").unwrap() {
            Listen::Ws(addr) => assert_eq!(addr.port(), 8790),
            _ => panic!("ws:// must parse to a Ws listener"),
        }
    }

    #[test]
    fn parse_listen_rejects_other_transports() {
        assert!(parse_listen("tcp://127.0.0.1:9000").is_err());
        assert!(parse_listen("ws://not-an-address").is_err());
    }
```

- [ ] **Step 7: 运行全部相关测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server && cargo test -p yi-agent --bin yi-agent listen`
Expected: app-server 全绿（182 + 2 ws + 5 broadcast = 189）；`parse_listen` 三例 PASS。

- [ ] **Step 8: 手动冒烟（真实进程 + 真 ws）**

Run:
```bash
cd yi-agent-rs && cargo run -p yi-agent -- app-server --listen ws://127.0.0.1:8790
```
Expected: stderr 打印 `app-server ws listening addr=127.0.0.1:8790`（tracing 输出取决于 `YI_LOG`），进程保持运行。另开一个终端用任意 ws 客户端发送 `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}`，应收到 `serverInfo` 响应。`Ctrl+C` 结束。

- [ ] **Step 9: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/Cargo.toml crates/yi-agent-app-server/src/ws.rs \
        crates/yi-agent-app-server/src/lib.rs crates/yi-agent-app-server/src/server.rs \
        crates/yi-agent/src/config.rs crates/yi-agent/src/main.rs Cargo.lock
git commit -m "feat(app-server): serve JSON-RPC over WebSocket (single client)"
```

---

## Task 4: 文档、Tailscale 直连指引、路线图状态

**Files:**
- Modify: `README.md`（三种用法 → 增加"远程/手机"小节与 `ws://` 说明）
- Modify: `docs/project-management/yi-agent-app-server.md`（范围 + Features 增加 ws 传输条目）
- Modify: `docs/project-management/desktop.md`（P3 的"Unix socket / websocket 传输"标记为部分完成）

- [ ] **Step 1: README 增加远程访问小节**

在"三种用法"之后、`## 配置放在哪` 之前插入：

```markdown
## 从手机/其它设备远程连（实验性）

`app-server` 支持通过 WebSocket 提供服务，供网络客户端（Tier 0 尚未带认证）：

```bash
yi-agent app-server --listen ws://127.0.0.1:8790
```

**Tier 0 无认证，只应绑定回环地址。** 要在外网连回家里/公司的电脑，推荐先用
[Tailscale](https://tailscale.com/)（基于 WireGuard 的零配置组网，无需公网 IP）：

1. 电脑与手机安装 Tailscale 并登录同一账号；
2. 电脑上 `yi-agent app-server --listen ws://$(tailscale ip -4):8790`；
3. 手机浏览器打开 `http://<电脑的 tailscale IP>:8790/`（移动端 UI 见 Tier 1 计划）。

多设备同时连接、扫码配对与设备撤销属于 Tier 1，尚未实现。
```

- [ ] **Step 2: app-server 模块文档增加 ws 条目**

在 `docs/project-management/yi-agent-app-server.md` 的"范围边界/做什么"补一行，并在 Features 追加：

```markdown
- [x] WebSocket 传输（`--listen ws://host:port`，Tier 0 单连接、无认证、默认回环）— `src/ws.rs`（`serve_ws` / `handle_ws`）；出站经 `src/broadcast.rs` 的 `Broadcaster` 扇出（stdio 单客户端语义不变）；CLI 分派 `yi-agent-rs/crates/yi-agent/src/main.rs`（`parse_listen` / `run_app_server`）；验证 `cargo test -p yi-agent-app-server`（含 `ws::` 2 例 + `broadcast::` 5 例）与 `cargo test -p yi-agent --bin yi-agent listen` — [设计](../superpowers/specs/2026-10-02-mobile-remote-access-design.md)
```

同时更新文件末尾的"验证命令"测试计数。

- [ ] **Step 3: desktop 路线图标注**

把 `docs/project-management/desktop.md` 中 P3 的：

```markdown
- [ ] Unix socket / websocket 传输 — 当前仅 stdio（...）；判据：`--listen` 支持 socket/ws 且 GUI 可连
```

改为：

```markdown
- [ ] WebSocket 传输（app-server 侧已支持 `--listen ws://`，见 [yi-agent-app-server](./yi-agent-app-server.md)；桌面端 GUI 仍走 stdio，尚未接入 ws）
```

- [ ] **Step 4: 提交**

```bash
git add README.md docs/project-management/yi-agent-app-server.md docs/project-management/desktop.md
git commit -m "docs: record app-server WebSocket transport and Tailscale quickstart"
```

---

## 自检

**Spec 覆盖**
- §3 架构（传输抽象 + Broadcaster）→ Task 2
- §4.1 Broadcaster → Task 1
- §4.2 ws 传输 → Task 3
- §4.4 server 改动（writer→Broadcaster、write_response/notification 迁移）→ Task 2
- §6 错误处理（写失败终止 stdio 会话、单客户端准入、畸形帧只回该客户端）→ Task 2/3
- §8 迁移兼容（默认 stdio、协议纯增量）→ Task 2/3/4
- §9 Tier 0 → 本计划整体
- **不在本计划**：§4.3 配对/设备、§4.4 多客户端扇出与 per-client initialized、§4.5 `pair/*`/`device/*`/`approvalResolved`/`-32014`、§5 安全、§7 移动 UI —— 均属 **Tier 1**，另立计划。

**占位符扫描**：无 TBD/TODO；每个代码步骤都给了可编译的实体代码或明确的编译器驱动清单。

**类型一致性**：`ClientId`（`local`/`ws`/`as_str`）、`Broadcaster`（`register`/`unregister`/`client_count`/`broadcast`/`reply`）、`serve`/`production_factory`/`read_lines`/`pump_stdout`/`serve_ws`/`handle_ws`、`Listen`/`parse_listen` 在 Task 1→4 中命名与签名一致。`write_response` 新签名 `(&Broadcaster, &ClientId, ResponseEnvelope)`、`write_notification` 新签名 `(&Broadcaster, &Notification)` 在 Task 2 定义、Task 2 内消费。

**已知风险（实现者注意）**
1. `pump_stdout` 必须保留"写失败 → 主循环 Err"的语义，否则 `oversized_frame_returns_err` 会红。
2. `axum` 的 `ws` feature 与 `tokio-tungstenite` 版本需对齐（Task 3 Step 1）。
3. `Message::Text` 在 axum 0.8 使用 `Utf8Bytes`；如编译器要求 `.into()`，按报错加。
