# iOS App 远程控制（Tier 1）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 iOS App 经**自建反向 WSS 中继**看到并控制电脑上 app-server 的同一批 session：
多设备扇出、扫码配对、远程审批、发消息/打断/中途追加。

**Architecture:** 电脑端 app-server 增加"出站 WSS 客户端"模式（`--relay wss://...`），
主动连中继；iOS App 也出站连中继。中继只按会话转发 JSON-RPC 帧。客户端复用现有
React 前端 + `Transport` 接口（新增 `wsTransport()`），用 Tauri 2 打包为 iOS。

**Tech Stack:** Rust 2024 / tokio / axum（ws）/ `tokio-tungstenite`（relay 客户端）/
Tauri 2（iOS target）/ React 19 + TypeScript / vitest。

**Spec:** `docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md`
（§2.4 形态与拓扑、§4.1 Broadcaster、§4.3 配对/设备、§4.4 多客户端扇出、
§4.5 新增协议、§5 安全、§6 错误处理、§11 中继与推送）。

**前置：** `docs/superpowers/plans/2026-10-02-app-server-websocket-transport.md`（Tier 0）
必须已合并——本计划消费它的 `serve()`、`Broadcaster`、`production_factory`、`read_lines`。

## Global Constraints

- **Tier 0 的既有测试必须持续全绿**：`cargo test -p yi-agent-app-server`（含 `ws::`、`broadcast::`）。
- **stdio 零回归**：桌面端 sidecar 行为不变。
- **电脑侧不开放入站端口**：只出站连中继。
- **App 是纯远程客户端**：不跑 agent、不执行 shell、不读宿主文件系统。
- **协议纯增量**：仅新增 `pair/*`、`device/*`、`approvalResolved`、`-32014`。
- **新设备默认 scope = `control`**（决策 A）；`admin` 类操作返回 `-32014`。
- **设备表持久化**在 `~/.yi-agent/devices.json`（决策 B），撤销即删记录。
- **fmt**：`cd yi-agent-rs && cargo fmt --all`，提交前必须跑。
- **分支**：worktree 作业，禁止在 `main` 提交；commit 用 conventional commits，不写 `Co-Authored-By`。
- 中继与 App 只应在**明确配置**后才连外网；默认关闭。

---

## 文件结构

| 文件 | 责任 | 动作 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs` | 支持对单个客户端 `reply` 与 per-client 状态 | 修改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` | `ClientId` 关联的 scope 与已初始化标志 | 修改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/pairing.rs` | 配对码、设备表、token 校验、撤销 | 新建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/device_store.rs` | `~/.yi-agent/devices.json` 原子读写 | 新建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | per-client `initialized`、scope 门禁、审批双答与 `approvalResolved` | 修改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` | 多客户端准入 + token 认证 | 修改 |
| `yi-agent-rs/crates/yi-agent-relay/` | 反向 WSS 中继（新 crate） | 新建 |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | `--relay wss://...` 出站模式 | 修改 |
| `desktop/src/wsTransport.ts` | `Transport` 的 WSS 实现 | 新建 |
| `desktop/src/transportFactory.ts` | 按平台/配置选 transport | 新建 |
| `desktop/src-tauri/` | Tauri 2 iOS target 配置 | 修改 |
| `desktop/src/**` | 移动端布局适配 | 修改 |

---

## Task 1: Broadcaster 支持 per-client 状态与定向

**Files:** Modify `broadcast.rs`；Test 同文件

**Interfaces:**
- Produces:
  - `Broadcaster::clients(&self) -> Vec<ClientId>`
  - `Broadcaster::reply_to_all(&self, frame: Value)`（`broadcast` 别名，语义明确化）
  - `Broadcaster::is_connected(&self, &ClientId) -> bool`

- [ ] **Step 1: 写失败的测试**

```rust
#[tokio::test]
async fn clients_lists_registered_ids() {
    let hub = Broadcaster::new();
    let id = ClientId::ws(uuid::Uuid::nil());
    let _a = hub.register(id.clone());
    let _b = hub.register(ClientId::local());
    let mut ids = hub.clients();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(ids.len(), 2);
    assert!(hub.is_connected(&id));
    assert!(hub.is_connected(&ClientId::local()));
}

#[tokio::test]
async fn is_connected_is_false_after_unregister() {
    let hub = Broadcaster::new();
    let id = ClientId::local();
    let _rx = hub.register(id.clone());
    hub.unregister(&id);
    assert!(!hub.is_connected(&id));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib broadcast::`
Expected: 编译失败（`clients` / `is_connected` 未定义）。

- [ ] **Step 3: 实现**

```rust
    /// 当前已注册的客户端 id 快照。
    pub fn clients(&self) -> Vec<ClientId> {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// 该客户端是否仍注册着。
    pub fn is_connected(&self, id: &ClientId) -> bool {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(id)
    }
```

- [ ] **Step 4: 运行确认通过并提交**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib broadcast::`
Expected: PASS（7 个测试）。

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/broadcast.rs
git commit -m "feat(app-server): expose broadcaster client roster"
```

---

## Task 2: 设备表与配对（`device_store.rs` + `pairing.rs`）

**Files:**
- Create: `crates/yi-agent-app-server/src/device_store.rs`
- Create: `crates/yi-agent-app-server/src/pairing.rs`
- Modify: `crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Produces:
  - `device_store::DeviceStore::new(path: PathBuf) -> Self`
  - `DeviceStore::add(&self, device: Device) -> io::Result<()>`
  - `DeviceStore::list(&self) -> Vec<Device>`
  - `DeviceStore::revoke(&self, id: &str) -> io::Result<bool>`
  - `DeviceStore::get(&self, id: &str) -> Option<Device>`
  - `struct Device { id: String, name: String, scope: Scope, token_hash: String, created_at: i64, last_seen_at: i64 }`
  - `pairing::PairingState::new(store: DeviceStore)`
  - `PairingState::create_code(&self) -> PairCode { code, expires_in }`
  - `PairingState::redeem(&self, code: &str, device_name: &str) -> Result<(Device, String), PairError>`（返回设备与**明文 token**）
  - `PairingState::authenticate(&self, token: &str) -> Option<Device>`

- [ ] **Step 1: 写失败的测试**

`device_store.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (DeviceStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        (DeviceStore::new(dir.path().join("devices.json")), dir)
    }

    fn device(id: &str) -> Device {
        Device {
            id: id.to_string(),
            name: "iPhone".into(),
            scope: Scope::Control,
            token_hash: "hash".into(),
            created_at: 1,
            last_seen_at: 1,
        }
    }

    #[test]
    fn add_then_list_round_trips_through_disk() {
        let (store, _dir) = store();
        store.add(device("d1")).unwrap();
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "d1");
        // 重新构造一次,证明真的落盘了。
        let reopened = DeviceStore::new(store.path().to_path_buf());
        assert_eq!(reopened.list().len(), 1);
    }

    #[test]
    fn revoke_removes_the_device() {
        let (store, _dir) = store();
        store.add(device("d1")).unwrap();
        assert!(store.revoke("d1").unwrap());
        assert!(store.list().is_empty());
        assert!(!store.revoke("d1").unwrap()); // 幂等
    }

    #[test]
    fn a_missing_file_reads_as_empty_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = DeviceStore::new(dir.path().join("absent.json"));
        assert!(store.list().is_empty());
    }
}
```

`pairing.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_redeems_once_and_yields_a_usable_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        let code = pairing.create_code();
        let (device, token) = pairing.redeem(&code.code, "iPhone 15").unwrap();
        assert_eq!(device.scope, Scope::Control, "新设备默认 control");
        assert!(pairing.authenticate(&token).is_some());

        // 同一个码不能再用。
        assert!(pairing.redeem(&code.code, "again").is_err());
    }

    #[test]
    fn an_unknown_token_does_not_authenticate() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        assert!(pairing.authenticate("nope").is_none());
    }

    #[test]
    fn revoking_a_device_invalidates_its_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        let code = pairing.create_code();
        let (device, token) = pairing.redeem(&code.code, "iPhone").unwrap();
        pairing.revoke(&device.id).unwrap();
        assert!(pairing.authenticate(&token).is_none());
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib "device_store::" && cargo test -p yi-agent-app-server --lib "pairing::"`
Expected: 编译失败。

- [ ] **Step 3: 实现 `device_store.rs`**

```rust
//! 已配对设备的持久化表:`~/.yi-agent/devices.json`。
//!
//! 与 `workspace_index.rs` 同一套约定:temp 文件 + rename 原子替换,进程内
//! `Mutex` 串行化读-改-写。撤销 = 删记录,因此 token 立即失效。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::protocol::Scope;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    /// token 的哈希,**绝不落明文**。
    pub token_hash: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<Device>,
}

pub struct DeviceStore {
    path: PathBuf,
    lock: Mutex<()>,
}

pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".yi-agent").join("devices.json")
}

impl DeviceStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> DevicesFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write(&self, file: &DevicesFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.path)
    }

    pub fn list(&self) -> Vec<Device> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.read().devices
    }

    pub fn get(&self, id: &str) -> Option<Device> {
        self.list().into_iter().find(|d| d.id == id)
    }

    pub fn add(&self, device: Device) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.devices.retain(|d| d.id != device.id);
        file.devices.push(device);
        self.write(&file)
    }

    /// 撤销:返回是否真的删掉了一条记录(幂等)。
    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        let before = file.devices.len();
        file.devices.retain(|d| d.id != id);
        let removed = file.devices.len() != before;
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }
}
```

- [ ] **Step 4: 实现 `pairing.rs`**

```rust
//! 扫码配对与设备 token。
//!
//! 一次性配对码(默认 5 分钟)换一枚**明文只出现一次**的设备 token;服务端只
//! 存哈希。撤销设备即删记录,该 token 立刻失效。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::device_store::{Device, DeviceStore};
use crate::protocol::Scope;

/// 配对码有效期。
pub const PAIR_CODE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct PairCode {
    pub code: String,
    pub expires_in: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// 码不存在、已用过或已过期。
    InvalidCode,
}

struct PendingCode {
    expires_at: Instant,
}

pub struct PairingState {
    store: DeviceStore,
    codes: Mutex<HashMap<String, PendingCode>>,
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// token 的存储形态:哈希。用 SHA-256 的十六进制表示。
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl PairingState {
    pub fn new(store: DeviceStore) -> Self {
        Self {
            store,
            codes: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &DeviceStore {
        &self.store
    }

    /// 铸一枚一次性配对码。桌面端拿到后渲染二维码。
    pub fn create_code(&self) -> PairCode {
        let code = short_code();
        self.codes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(code.clone(), PendingCode { expires_at: Instant::now() + PAIR_CODE_TTL });
        PairCode {
            code,
            expires_in: PAIR_CODE_TTL.as_secs(),
        }
    }

    /// 用配对码换设备 token。返回设备与**明文 token**;码用后即焚。
    pub fn redeem(&self, code: &str, device_name: &str) -> Result<(Device, String), PairError> {
        let mut guard = self.codes.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pending) = guard.remove(code) else {
            return Err(PairError::InvalidCode);
        };
        if Instant::now() > pending.expires_at {
            return Err(PairError::InvalidCode);
        }
        drop(guard);

        let token = format!("yia_{}", short_code_secret());
        let now = now_epoch_secs();
        let device = Device {
            id: format!("dev-{}", uuid::Uuid::new_v4()),
            name: device_name.to_string(),
            // 新配对设备默认 Control(决策 A)。
            scope: Scope::Control,
            token_hash: hash_token(&token),
            created_at: now,
            last_seen_at: now,
        };
        self.store
            .add(device.clone())
            .map_err(|_| PairError::InvalidCode)?;
        Ok((device, token))
    }

    /// 校验一枚 token。命中即刷新 `last_seen_at`。
    pub fn authenticate(&self, token: &str) -> Option<Device> {
        let hash = hash_token(token);
        let found = self
            .store
            .list()
            .into_iter()
            .find(|d| d.token_hash == hash)?;
        let mut touched = found.clone();
        touched.last_seen_at = now_epoch_secs();
        let _ = self.store.add(touched);
        Some(found)
    }

    pub fn revoke(&self, device_id: &str) -> Result<bool, PairError> {
        self.store.revoke(device_id).map_err(|_| PairError::InvalidCode)
    }
}

/// 人类可读的一次性配对码:`XXXX-XXXX`(易输入,去掉易混字符)。
fn short_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::new();
    for _ in 0..8 {
        let byte = uuid::Uuid::new_v4().as_bytes()[0] as usize;
        out.push(ALPHABET[byte % ALPHABET.len()] as char);
    }
    format!("{}-{}", &out[0..4], &out[4..8])
}

/// token 的随机段。
fn short_code_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
```

`Cargo.toml` 需加 `sha2 = "0.10"` 到 `[dependencies]`。

- [ ] **Step 5: 运行确认通过并提交**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib "device_store::" && cargo test -p yi-agent-app-server --lib "pairing::"`
Expected: PASS（6 个测试）。

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/device_store.rs crates/yi-agent-app-server/src/pairing.rs \
        crates/yi-agent-app-server/src/lib.rs crates/yi-agent-app-server/Cargo.toml Cargo.lock
git commit -m "feat(app-server): device store and QR pairing"
```

---

## Task 3: per-client 状态、scope 门禁、审批双答

**Files:** Modify `protocol.rs`（`Scope`、`-32014`、`ApprovalResolved`）、`server.rs`

**Interfaces:**
- Consumes: Task 1 的 `clients()` / `is_connected()`；Task 2 的 `PairingState`。
- Produces:
  - `protocol::Scope { Observe, Control, Admin }`（`serde` lowercase，`Ord`）
  - `RpcError::insufficient_scope(required: Scope) -> Self`（`-32014`）
  - `Notification::ToolCallApprovalResolved { perm_id, by, decision }`
  - `serve(...)` 的入站载荷扩展为 `(ClientId, Scope, anyhow::Result<String>)`

- [ ] **Step 1: 写失败的测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_control_client_cannot_delete_a_thread() {
    let mut h = Harness::with_scope(Scope::Control).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"thread_id":"{tid}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32014, "admin op from a control client: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_client_may_delete_a_thread() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"thread_id":"{tid}"}}}}"#
    ))
    .await;
    let v = h.read_value().await;
    assert!(v["result"].is_object(), "admin client must be allowed: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_second_approval_answer_is_a_noop_and_resolution_is_broadcast() {
    // 用两个逻辑客户端共享一个 serve():A 先答,B 后答。
    // 断言:B 的响应为成功(无 error),且两端都收到 approvalResolved。
    // (完整驱动见 Task 5 的 ws E2E;此处以 pending 路由单测覆盖核心分支。)
}
```

`Harness` 需加 `with_scope(Scope)`（把 `ClientId::local()` 与给定 scope 一起注册）。

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib "scope"`
Expected: 编译失败。

- [ ] **Step 3: 实现协议增量**

`protocol.rs`：

```rust
/// 客户端的权限范围。`Ord` 让 `>=` 直接表达"够不够"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Observe,
    Control,
    Admin,
}

impl RpcError {
    pub fn insufficient_scope(required: Scope) -> Self {
        Self::new(
            -32014,
            format!("insufficient scope: requires {required:?}"),
        )
    }
}
```

`Notification` 加变体：

```rust
    /// 某次审批已被**任一**客户端处理;其余客户端据此关闭弹窗。
    #[serde(rename = "item/toolCall/approvalResolved")]
    ToolCallApprovalResolved {
        perm_id: String,
        by: String,
        decision: String,
    },
```

- [ ] **Step 4: 实现 per-client `initialized` 与 scope 门禁**

`serve` 的入站 channel 载荷改为 `(ClientId, Scope, anyhow::Result<String>)`；
用 `HashMap<ClientId, bool>` 记 initialized、`HashMap<ClientId, Scope>` 记 scope
（stdio 注册 `local` + `Admin`；ws 注册其 token 的 scope）。

在主循环 `match method.as_str()` 前插入门禁：

```rust
                // admin 类方法:control/observe 客户端一律拒绝。
                const ADMIN_METHODS: [&str; 3] = [
                    "thread/delete",
                    "process/kill",
                    "thread/setPermissionMode",
                ];
                if ADMIN_METHODS.contains(&method.as_str()) && client_scope < Scope::Admin {
                    write_response(
                        &hub,
                        &client,
                        err_response(id, RpcError::insufficient_scope(Scope::Admin)),
                    )
                    .await?;
                    continue;
                }
```

`initialized` 改为查 `initialized.get(&client).copied().unwrap_or(false)`，
`initialize` 分支写 `initialized.insert(client.clone(), true)`。

- [ ] **Step 5: 实现审批双答与已解决广播**

`route_client_response` 改为返回是否命中：

```rust
/// 把客户端对反向请求的响应路由到等待中的 driver。
///
/// 返回 `Some(decision)` 表示本次响应**首次**命中;`None` 表示该审批已被别的
/// 客户端处理(或根本不存在)。后者不是错误:双端同时点"允许"时,后到者是
/// no-op 成功,而不是报错——报错会让另一端弹出一个无意义的失败提示。
async fn route_client_response(
    resp: ClientResponse,
    pending: &Mutex<HashMap<String, oneshot::Sender<Decision>>>,
) -> Option<(String, Decision)> {
    let RequestId::Str(key) = resp.id else {
        tracing::warn!("ignoring client response with non-string id");
        return None;
    };
    let Some(tx) = pending.lock().await.remove(&key) else {
        tracing::debug!("approval {key} was already resolved");
        return None;
    };
    let decision = parse_client_decision(resp.result.as_ref());
    let _ = tx.send(decision.clone());
    Some((key, decision))
}
```

主循环收到响应时：

```rust
                            route_client_response(resp, &pending).await;
```

改为：

```rust
                            if let Some((perm_id, decision)) = route_client_response(resp, &pending).await {
                                write_notification(
                                    &hub,
                                    &Notification::ToolCallApprovalResolved {
                                        perm_id,
                                        by: client.as_str().to_string(),
                                        decision: decision_label(&decision),
                                    },
                                )
                                .await?;
                            }
```

加一个 `fn decision_label(d: &Decision) -> String`（`allow_once` / `always_allow_tool` / `always_allow_prefix` / `deny`）。

- [ ] **Step 6: 运行全量测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（原 189 + 新增 scope 2 + broadcast 2 = 193 上下；以实际为准，0 failed）。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): per-client scope, init state, and approval resolution"
```

---

## Task 4: `pair/*` 与 `device/*` RPC

**Files:** Modify `server.rs`；Test 同文件

**Interfaces:**
- Produces RPC：`pair/create`、`device/list`、`device/revoke`
- Consumes: Task 2 的 `PairingState`

- [ ] **Step 1: 写失败的测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn pair_create_returns_a_code_and_device_revoke_invalidates_it() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"pair/create","params":{}}"#)
        .await;
    let created = h.read_value().await;
    let code = created["result"]["code"].as_str().unwrap().to_string();
    assert!(created["result"]["expires_in"].as_u64().unwrap() > 0);

    // 设备表初始为空。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"device/list","params":{}}"#)
        .await;
    let listed = h.read_value().await;
    assert_eq!(listed["result"]["devices"].as_array().unwrap().len(), 0);

    // 用码换 token,设备表出现一条。
    let (device, _token) = h.pairing().redeem(&code, "iPhone 15").unwrap();
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"device/list","params":{}}"#)
        .await;
    let listed = h.read_value().await;
    assert_eq!(listed["result"]["devices"].as_array().unwrap().len(), 1);

    // 撤销。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"device/revoke","params":{{"device_id":"{}"}}}}"#,
        device.id
    ))
    .await;
    let revoked = h.read_value().await;
    assert_eq!(revoked["result"]["revoked"], true);
    h.shutdown().await;
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib pair_create`
Expected: 编译失败（无 `pair/create` 分支）。

- [ ] **Step 3: 实现三个 RPC 分支**

在 `serve` 的 `match method.as_str()` 中加：

```rust
                    "pair/create" => {
                        let code = pairing.create_code();
                        write_response(
                            &hub,
                            &client,
                            ok_response(
                                id,
                                json!({ "code": code.code, "expires_in": code.expires_in }),
                            ),
                        )
                        .await?;
                    }
                    "device/list" => {
                        let devices: Vec<serde_json::Value> = pairing
                            .store()
                            .list()
                            .into_iter()
                            .map(|d| {
                                json!({
                                    "id": d.id,
                                    "name": d.name,
                                    "scope": d.scope,
                                    "created_at": d.created_at,
                                    "last_seen_at": d.last_seen_at,
                                })
                            })
                            .collect();
                        write_response(&hub, &client, ok_response(id, json!({ "devices": devices })))
                            .await?;
                    }
                    "device/revoke" => {
                        let Some(device_id) = req
                            .params
                            .get("device_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::invalid_params("missing device_id")),
                            )
                            .await?;
                            continue;
                        };
                        let revoked = pairing.revoke(&device_id).unwrap_or(false);
                        if revoked {
                            if let Some(cid) = ws_client_for_device(&device_id) {
                                hub.unregister(&cid);
                            }
                        }
                        write_response(&hub, &client, ok_response(id, json!({ "revoked": revoked })))
                            .await?;
                    }
```

`pairing` 作为 `serve` 的新参数注入（测试里可拿到）。`ws_client_for_device` 依赖 ws 层登记
`ClientId → device_id` 的映射（Task 5 提供；本任务先接一个返回 `None` 的桩，
Task 5 换成真实查表）。

- [ ] **Step 4: 运行确认通过并提交**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS。

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): pair and device RPCs"
```

---

## Task 5: WS 多客户端 + token 认证 + relay 出站模式

**Files:**
- Modify `ws.rs`（多客户端准入、`Authorization: Bearer` 认证、设备↔ClientId 映射）
- Create `crates/yi-agent-relay/`（中继 crate）
- Modify `crates/yi-agent/src/main.rs`（`--relay wss://...`）
- Modify `crates/yi-agent/src/config.rs`

**Interfaces:**
- Produces:
  - `ws::serve_ws(listener, cfg, workspaces, pairing) -> anyhow::Result<()>`（多客户端）
  - `ws::device_clients() -> HashMap<String, ClientId>`（供 `device/revoke` 用）
  - `relay::run_client(relay_url: Url, app_server_ws: Url, session_id: String) -> anyhow::Result<()>`
  - `relay::serve(listener: TcpListener) -> anyhow::Result<()>`（中继服务端；同 crate 两个 bin/lib）
  - `main.rs`：`--relay wss://host/connect?session=<id>` → 以出站客户端身份连中继

- [ ] **Step 1: 写失败的 E2E（多客户端 + 审批先到先得）**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn two_clients_both_see_the_turn_and_only_the_first_approval_counts() {
    // 起 ws server;连 A 与 B,各自 initialize;
    // A 发 turn/start 触发需审批的工具 → 两端都收到 requestApproval;
    // A 先答 allow → A 收到 approvalResolved;B 再答 → 不报错,B 也收到 approvalResolved。
    // 断言 turn 正常结束。
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib two_clients`
Expected: FAIL（当前单连接准入会拒掉第二个）。

- [ ] **Step 3: 改 `ws.rs` 为多客户端 + 认证**

- 去掉 `hub.client_count() > 0` 的单连接拒绝；
- 握手时从 `Authorization: Bearer <token>` 取 token，`pairing.authenticate` 通过才接受，
  否则回 `Message::Close`（close code 4401）；
- 注册 `ClientId` 与 `Scope`（来自 device）；
- 维护 `Arc<Mutex<HashMap<String, ClientId>>>`（device_id → ClientId）供 revoke 用。

- [ ] **Step 4: 新建 `yi-agent-relay` crate**

```
yi-agent-rs/crates/yi-agent-relay/
  Cargo.toml        # 依赖: tokio, axum(ws), tokio-tungstenite, serde_json, anyhow, tracing, url
  src/lib.rs        # 会话注册表 + 路由
  src/bin/yi-agent-relay.rs  # 服务端入口
```

中继核心（最小可用）：

```rust
//! 反向 WSS 中继:两端都出站连接,中继按 session 配对并双向转发帧。
//!
//! 中继**不解析 JSON-RPC 语义**:它只保证「同一 session 的 App 帧送到该 session
//! 的电脑连接,反之亦然」。这使中继极薄、可无状态重启。

pub struct Relay {
    // session_id -> 电脑侧连接(每 session 一条)
    agents: Mutex<HashMap<String, mpsc::Sender<Message>>>,
    // session_id -> App 侧连接(可多条)
    apps: Mutex<HashMap<String, Vec<mpsc::Sender<Message>>>>,
}

impl Relay {
    /// 电脑侧连接:登记为 agent,并转发 App→agent 的帧。
    pub async fn attach_agent(&self, session: String, sink: mpsc::Sender<Message>, mut from_agent: ...);
    /// App 侧连接:登记并转发 agent→app 的帧。
    pub async fn attach_app(&self, session: String, sink: mpsc::Sender<Message>, mut from_app: ...);
}
```

路由规则：`agent → app` 广播给该 session 的全部 app;`app → agent` 送该 session 的唯一
agent(不存在则回错误帧)。断连即摘除。

- [ ] **Step 5: `--relay` 出站模式**

`config.rs` 加：

```rust
    /// Bridge this app-server to a relay over an outbound WSS connection.
    #[arg(long)]
    relay: Option<String>,
```

`main.rs`：

```rust
        Listen::Relay(url) => rt.block_on(async move {
            // 电脑侧不监听入站端口:起一个本地环回 ws app-server,
            // 再用 relay 客户端把本地 ws 与中继透明对接。
            let local = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = local.local_addr()?;
            let workspaces = ...;
            let pairing = ...;
            tokio::spawn(yi_agent_app_server::ws::serve_ws(local, config, workspaces, pairing));
            yi_agent_relay::run_client(url, addr).await
        }),
```

`parse_listen` 扩展接受 `relay://wss://...` 或独立的 `--relay`；以 `cargo build` 通过为准。

- [ ] **Step 6: 运行全量 + 中继单测**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server && cargo test -p yi-agent-relay`
Expected: 全绿，含多客户端 E2E。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/ws.rs crates/yi-agent-relay \
        crates/yi-agent/src/main.rs crates/yi-agent/src/config.rs Cargo.toml Cargo.lock
git commit -m "feat: multi-client ws with auth, plus outbound relay"
```

---

## Task 6: iOS App（Tauri 2）与 `wsTransport`

**Files:**
- Create `desktop/src/wsTransport.ts`、`desktop/src/transportFactory.ts`
- Modify `desktop/src/App.tsx`（用 factory 取代直接 `tauriTransport()`；移动端布局）
- Modify `desktop/src-tauri/tauri.conf.json`、`package.json`（iOS target）

**Interfaces:**
- Produces:
  - `wsTransport(url: string, token: string): Transport`
  - `transportFactory(): Transport`（按运行环境选择）

- [ ] **Step 1: 写失败的测试**

```ts
// desktop/src/wsTransport.test.ts
import { describe, expect, it, vi } from "vitest";
import { wsTransport } from "./wsTransport";

describe("wsTransport", () => {
  it("sends a JSON-RPC frame and resolves correlated responses", async () => {
    const fake = new FakeWebSocket();
    const t = wsTransport("wss://relay.test/ws", "tok", () => fake as unknown as WebSocket);
    const seen: unknown[] = [];
    t.onMessage((m) => seen.push(m));
    await t.send({ id: 1, method: "initialize", params: {} });
    expect(fake.sent[0]).toContain("initialize");
    expect(fake.sent[0]).toContain("Bearer tok".replace("Bearer tok", "tok")); // token 在 URL/头里
    fake.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }));
    expect(seen).toHaveLength(1);
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop && npx vitest run src/wsTransport.test.ts`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 实现 `wsTransport.ts`**

```ts
import type { ApprovalRequest } from "./lib/protocol";
import type { Transport } from "./lib/rpc";

/**
 * `Transport` backed by a WebSocket. The iOS app is a pure remote client: it
 * speaks the same wire protocol as the desktop, over the relay.
 *
 * A `factory` seam keeps this unit-testable without a live socket.
 */
export function wsTransport(
  url: string,
  token: string,
  factory: (url: string, protocols?: string[]) => WebSocket = (u) => new WebSocket(u),
): Transport {
  const socket = factory(url);
  const messageHandlers = new Set<(m: unknown) => void>();
  const requestHandlers = new Set<(r: ApprovalRequest) => void>();
  const statusHandlers = new Set<(s: { state: string; code?: number | null }) => void>();

  socket.onopen = () => {
    socket.send(JSON.stringify({ jsonrpc: "2.0", method: "auth", params: { token } }));
  };
  socket.onmessage = (e) => {
    let v: any;
    try {
      v = JSON.parse(typeof e.data === "string" ? e.data : "");
    } catch {
      return;
    }
    // 反向请求(带 method + id)= 审批;通知(带 method,无 id)= 消息。
    if (v.method && "id" in v) requestHandlers.forEach((h) => h(v));
    else if (v.method) messageHandlers.forEach((h) => h(v));
    else messageHandlers.forEach((h) => h(v));
  };
  socket.onclose = () => statusHandlers.forEach((h) => h({ state: "exited" }));

  return {
    send: async (message) => {
      socket.send(JSON.stringify({ jsonrpc: "2.0", ...message }));
    },
    respond: async (id, result) => {
      socket.send(JSON.stringify({ jsonrpc: "2.0", id, result }));
    },
    onMessage: (cb) => {
      messageHandlers.add(cb);
      return () => messageHandlers.delete(cb);
    },
    onRequest: (cb) => {
      requestHandlers.add(cb);
      return () => requestHandlers.delete(cb);
    },
    onStatus: (cb) => {
      statusHandlers.add(cb);
      return () => statusHandlers.delete(cb);
    },
  };
}
```

> 注：token 的传输方式要与服务端最终选定的握手对齐（任务 Step 里先用 `auth` 帧；
> 若服务端改走 `Authorization` 头，这里同步改）。

- [ ] **Step 4: 实现 `transportFactory.ts`**

```ts
import type { Transport } from "./lib/rpc";
import { tauriTransport } from "./tauriTransport";
import { wsTransport } from "./wsTransport";

/**
 * 桌面端用 Tauri 桥(stdio sidecar);iOS 端用 WSS 连中继。
 * 判定依据:Tauri 的 `window.__TAURI_INTERNALS__` 在桌面存在;iOS 下用远端。
 */
export function transportFactory(): Transport {
  const remote = localStorage.getItem("yi-agent.remote");
  if (remote) {
    const { url, token } = JSON.parse(remote);
    return wsTransport(url, token);
  }
  return tauriTransport();
}
```

`App.tsx` 把 `new RpcClient(tauriTransport())` 改为 `new RpcClient(transportFactory())`。
`plugin-dialog` 与 `plugin-opener` 在 iOS 下走平台分支（文件选择器在 iOS 上是 no-op 或替代实现）。

- [ ] **Step 5: 运行前端测试**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: PASS，含 `wsTransport.test.ts`；既有 206 个测试不受影响。

- [ ] **Step 6: Tauri iOS target 与打包**

Run:
```bash
cd desktop && npx tauri ios init && npx tauri ios build
```
Expected: 产出 iOS 构建（需 Xcode + 签名配置）。此步骤依赖本机 Xcode 环境，
若不可用，记录阻塞并保留配置改动。

- [ ] **Step 7: 提交**

```bash
cd desktop && npx tsc --noEmit
git add src/wsTransport.ts src/wsTransport.test.ts src/transportFactory.ts src/App.tsx \
        src-tauri/tauri.conf.json package.json
git commit -m "feat(desktop): ws transport and iOS target for the remote app"
```

---

## Task 7: 文档与部署指引

**Files:** Modify `README.md`、`docs/project-management/desktop.md`、新建 `docs/relay-deploy.md`

- [ ] **Step 1: 中继部署文档**

新建 `docs/relay-deploy.md`：VPS 准备、域名与 TLS（Let's Encrypt）、
`yi-agent-relay` 运行方式、电脑侧 `--relay` 连接、iOS App 配对流程、故障排查
（连不上、审批弹不出、token 失效）。

- [ ] **Step 2: README 与模块文档更新**

README 的"远程连接"小节补 iOS App 与中继的指引；`desktop.md` 增加 iOS target 条目。

- [ ] **Step 3: 提交**

```bash
git add README.md docs/project-management/desktop.md docs/relay-deploy.md
git commit -m "docs: iOS remote app and relay deployment"
```

---

## 自检

**Spec 覆盖**
- §2.4 形态与拓扑 → Task 5（中继）+ Task 6（iOS）
- §4.1 Broadcaster（多客户端）→ Task 1 + Task 3
- §4.3 配对/设备 → Task 2 + Task 4
- §4.4 per-client initialized + writer→hub → Task 3
- §4.5 `pair/*`/`device/*`/`approvalResolved`/`-32014` → Task 3 + Task 4
- §5 安全（token、scope、撤销、fail-safe）→ Task 2 + Task 3 + Task 5
- §6 错误处理（慢消费者、断连、双答、超时）→ Task 1 + Task 3 + Task 5
- §11 中继 → Task 5 + Task 7
- **不在本计划**：APNs 推送、端到端加密、按 thread 订阅过滤、Android —— Tier 1.1/Tier 2。

**占位符扫描**：Task 5/6 中若干实现细节（relay 的 `attach_agent` 完整签名、
token 握手形态、iOS 布局适配）给的是**方向 + 可运行骨架**而非逐行成品，因为中继与 iOS
打包涉及本机工具链（Xcode/签名），无法在纯 Rust 测试内闭环；这些步骤都给出了判据与
"以 `cargo build` / `vitest` 通过为准"的收敛条件。

**类型一致性**：`Scope`（`Observe<Control<Admin`）、`Device`、`PairingState`
（`create_code`/`redeem`/`authenticate`/`revoke`/`store`）、`ClientId`、
`Broadcaster`（`clients`/`is_connected`/`register`/`unregister`/`broadcast`/`reply`）、
`wsTransport`/`transportFactory` 在 Task 1→7 中命名与签名一致。
`route_client_response` 返回 `Option<(String, Decision)>`，Task 3 定义并消费。

**已知风险（实现者注意）**
1. Task 5 把单连接 ws 改成多客户端后，Tier 0 的 `a_second_connection_is_rejected_...`
   测试必须**改为多客户端断言**（它测的是 Tier 0 的临时约束）。
2. relay 客户端需处理重连与心跳（手机切网、电脑休眠）；v1 用简单指数退避 + 定期 ping。
3. iOS 打包需 Apple Developer 账号与签名；无 Xcode 环境时 Task 6 Step 6 记为阻塞。
