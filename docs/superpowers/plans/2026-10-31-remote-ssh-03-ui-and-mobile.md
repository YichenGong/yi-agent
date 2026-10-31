# 远程 SSH 执行 · 计划 3：路径语义、设置页与手机多连接

**Goal:** 让远端项目在 UI 上真正可用：远端模式下不传本机 `cwd`（避免 `canonicalize` 失败）、设置页能增删远端项目并显示连接进度与断线原因、手机经中继拿到项目清单并为每个项目各建一条连接。

**Architecture:** 路径语义走 spec §4.4 的"方案 B"——远端 `cwd` 完全由 ssh 会话的工作目录决定，前端在远端模式下**根本不传** `cwd` 字段。手机清单由 `remote/relayTargets` 提供（app-server 唯一新增方法，只读），其 `available` 字段读桥接层写的 `remote-status.json` 旁路文件。

**Tech Stack:** Rust（Tauri 命令、app-server RPC handler）、TypeScript（React、vitest）、`yi-agent-app-server` 的 `rpc/` 域模块模式。

**依据 spec:** `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md` §4.3、§4.4、§4.6（障碍一~六、relayTargets、手机多连接）、§5.1、§5.2、§5.4、§8.9。

**前置依赖:** 计划 1（多槽位桥接层）必须已完成。

## Global Constraints

- **远端模式下前端不得传 `cwd`**（spec §4.4）。本机路径送到远端 `canonicalize` 必然失败。
- **`remote/*` 命令在 flag 关闭时必须返回"命令不存在"语义的类型化错误**，与 `flags_rpc.rs:36` 对 stable 的处理同构。
- **前端永远不从 sidecar 的 `flags/list` 读 `remote_ssh`**（spec §5.2）：那是远端的 flag。
- **`remote/relayTargets` 的 `Observe` scope 不得返回 `session`**（spec §4.6、§8.9）。
- **关 flag 时必须杀掉全部远端 slot**，并等进程真正退出后才返回（spec §5.1 第 3 条）。
- **`remote-status.json` 未知取值按 `disconnected` 处理**（fail-closed）。
- **手机端 `RemoteConfig` 扩展必须向后兼容**：旧的单个 `{url, token}` 仍能读（spec §4.6 第 12 步）。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `desktop/src-tauri/src/bridge.rs` | 远端 slot 的退避上限 60s、5 次后停止；写 `remote-status.json` | 改 |
| `desktop/src-tauri/src/remote/status.rs` | `remote-status.json` 读写（与 `config.rs` 同款写盘纪律） | 建 |
| `desktop/src-tauri/src/remote/rpc.rs` | `remote/*` 的 Tauri 命令集合（含 flag 闸门） | 建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/remote.rs` | `remote/relayTargets` handler | 建 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` | 注册 `remote` 域 | 改 |
| `desktop/src/RemoteSettings.tsx` | 「远程」设置页 | 建 |
| `desktop/src/App.tsx` | 远端模式下 `thread/start` 不传 `cwd` | 改 |
| `desktop/src/lib/remoteConfig.ts` | `RemoteConfig` 扩成列表，向后兼容 | 改 |
| `desktop/src/transportFactory.ts` | 支持按项目取 transport | 改 |

---

### Task 1: 远端 slot 的重试策略（60s 上限、5 次后停）

**Files:**
- Modify: `desktop/src-tauri/src/bridge.rs:96-99`（`next_restart_delay`）
- Test: `bridge.rs` 的 `mod tests`

**Interfaces:**
- Consumes: 计划 1 的 `ProfileKey`
- Produces:
  - `fn next_restart_delay_for(slot: &ProfileKey, consecutive: u32) -> Duration`
  - `const REMOTE_MAX_RETRIES: u32 = 5`

**关键（spec §4.3）：** 本地保持 200ms→5s；远端 200ms→60s，且连续 5 次后停止。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn local_backoff_keeps_its_five_second_cap() {
        use std::time::Duration;
        assert_eq!(
            next_restart_delay_for(&ProfileKey::Local, 10),
            Duration::from_millis(5_000),
            "local behavior must not change"
        );
    }

    #[test]
    fn remote_backoff_climbs_to_sixty_seconds() {
        use std::time::Duration;
        let key = ProfileKey::Remote("p".into());
        assert_eq!(next_restart_delay_for(&key, 0), Duration::from_millis(200));
        assert_eq!(next_restart_delay_for(&key, 9), Duration::from_secs(60));
    }

    #[test]
    fn remote_gives_up_after_five_consecutive_failures() {
        let key = ProfileKey::Remote("p".into());
        assert!(should_retry(&key, 4), "the 5th attempt still happens");
        assert!(!should_retry(&key, 5), "after 5 failures stop hammering the server");
    }

    #[test]
    fn local_never_gives_up() {
        assert!(should_retry(&ProfileKey::Local, 1_000));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib bridge::tests::remote_backoff
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
/// 远端连续失败这么多次后停止自动重试，转「断线」让人手动重连。
///
/// 理由（spec §4.3）：远端失败常见原因是网络断/服务器关机/VPN 掉线，
/// 会持续数分钟到数小时。无限重试等于每 5s 发起一次 ssh，既刷日志也给
/// 服务器制造无意义的认证请求。
const REMOTE_MAX_RETRIES: u32 = 5;

fn next_restart_delay_for(slot: &ProfileKey, consecutive_restarts: u32) -> Duration {
    const BASE_MS: u64 = 200;
    let cap_ms: u64 = match slot {
        // 本地是同机进程，崩溃多半瞬时；保持原样。
        ProfileKey::Local => 5_000,
        // 远端涉及网络，退回慢一些，减少无意义认证。
        ProfileKey::Remote(_) => 60_000,
    };
    let shift = consecutive_restarts.min(16);
    let ms = (BASE_MS << shift).min(cap_ms);
    Duration::from_millis(ms)
}

fn should_retry(slot: &ProfileKey, consecutive_failures: u32) -> bool {
    match slot {
        ProfileKey::Local => true,
        ProfileKey::Remote(_) => consecutive_failures < REMOTE_MAX_RETRIES,
    }
}
```

监督循环里把 `next_restart_delay(consecutive_restarts)` 换成 `next_restart_delay_for(&key, consecutive_restarts)`，并在重试前判断 `should_retry`；不重试时 emit 一条 `{state:"disconnected", reason:"gave up after 5 attempts"}` 到该 slot 的 status 事件名然后退出循环。

保留旧的 `next_restart_delay` 供既有测试用（或把既有测试改成 `for(&Local, n)`，二选一，但**不要留下两个语义不同的同名函数**）。

- [ ] **Step 4: 跑测试**

```bash
cd desktop/src-tauri && cargo test --lib bridge::tests 2>&1 | tail -5
```

Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/bridge.rs
git commit -m "feat(remote): back off further and give up after 5 failures"
```

---

### Task 2: `remote-status.json` 旁路文件

**Files:**
- Create: `desktop/src-tauri/src/remote/status.rs`
- Modify: `desktop/src-tauri/src/bridge.rs`（slot 状态变化时写）
- Modify: `desktop/src-tauri/src/remote/mod.rs`
- Test: `status.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: `ProfileKey`
- Produces:
  - `pub fn write_status(path: &Path, profile_id: &str, status: SlotStatus) -> io::Result<()>`
  - `pub fn read_status(path: &Path) -> HashMap<String, SlotStatus>`
  - `pub enum SlotStatus { Connected, Connecting, Disconnected }`，`impl` `as_str` / `from_str`（未知 → `Disconnected`）

**为什么需要（spec §4.6）：** 本机 app-server 完全不知道远端连没连——那是 Tauri 层的 `HashMap`。但 `remote/relayTargets` 要在 app-server 里返回 `available`，所以桥接层得把状态写到 app-server 能读的地方。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_a_single_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        write_status(&path, "proj-foo", SlotStatus::Connected).unwrap();
        let map = read_status(&path);
        assert_eq!(map.get("proj-foo"), Some(&SlotStatus::Connected));
    }

    #[test]
    fn writing_one_profile_preserves_the_others() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        write_status(&path, "a", SlotStatus::Connected).unwrap();
        write_status(&path, "b", SlotStatus::Connecting).unwrap();
        let map = read_status(&path);
        assert_eq!(map.get("a"), Some(&SlotStatus::Connected));
        assert_eq!(map.get("b"), Some(&SlotStatus::Connecting));
    }

    #[test]
    fn unknown_values_fail_closed_to_disconnected() {
        // 宁可让手机多试一次，也不要报一个假的"可用"。
        assert_eq!(SlotStatus::from_str("weird"), SlotStatus::Disconnected);
        assert_eq!(SlotStatus::from_str(""), SlotStatus::Disconnected);
    }

    #[test]
    fn broken_file_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(read_status(&path).is_empty());
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_status(&dir.path().join("nope.json")).is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::status
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! `remote-status.json`：桥接层写、app-server 读的运行时状态旁路。
//!
//! **它与 `remote.json` 刻意分开**（spec §4.9）：一个是用户配置（需严格校验、
//! 原子性重要），一个是易失状态（高频更新、可丢）。合并会让状态的高频写
//! 污染配置的原子性。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Connected,
    Connecting,
    Disconnected,
}

impl SlotStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotStatus::Connected => "connected",
            SlotStatus::Connecting => "connecting",
            SlotStatus::Disconnected => "disconnected",
        }
    }

    /// 未知值**必须**落到 `Disconnected`：fail-closed。
    pub fn from_str(raw: &str) -> Self {
        match raw {
            "connected" => SlotStatus::Connected,
            "connecting" => SlotStatus::Connecting,
            _ => SlotStatus::Disconnected,
        }
    }
}

fn read_all(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("statuses").and_then(|s| s.as_object()).cloned())
        .unwrap_or_default()
}

pub fn read_status(path: &Path) -> HashMap<String, SlotStatus> {
    read_all(path)
        .into_iter()
        .map(|(k, v)| {
            let status = v.as_str().map(SlotStatus::from_str).unwrap_or(SlotStatus::Disconnected);
            (k, status)
        })
        .collect()
}

/// 读-改-写。写失败**只记日志**：这是可丢的运行时状态，不该让连接流程失败。
pub fn write_status(path: &Path, profile_id: &str, status: SlotStatus) -> io::Result<()> {
    let mut all = read_all(path);
    all.insert(profile_id.to_string(), serde_json::json!(status.as_str()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&serde_json::json!({ "statuses": all }))
        .map_err(io::Error::other)?;
    let tmp: PathBuf = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}
```

- [ ] **Step 4: 在 bridge 的状态变化点调用**

`spawn_remote_once` 成功后写 `Connected`（或先写 `Connecting` 再在首帧后写 `Connected`）；`clear_child` 与"放弃重试"分支写 `Disconnected`。

```rust
fn status_path() -> PathBuf {
    instance_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("remote-status.json")
}
```

- [ ] **Step 5: 跑测试**

```bash
cd desktop/src-tauri && cargo test --lib remote::status
```

Expected: 5 passed。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/status.rs desktop/src-tauri/src/remote/mod.rs desktop/src-tauri/src/bridge.rs
git commit -m "feat(remote): publish slot status for app-server to read"
```

---

### Task 3: `remote/*` 的 Tauri 命令与 flag 闸门

**Files:**
- Create: `desktop/src-tauri/src/remote/rpc.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`、`desktop/src-tauri/src/lib.rs`
- Test: `rpc.rs` 内嵌 `mod tests`（测纯逻辑：闸门判定、参数校验）

**Interfaces:**
- Consumes: `RemoteConfig`（计划 1 Task 4）、`remote_argv`（计划 1 Task 3）、`is_enabled`（flag）
- Produces（均为 `#[tauri::command]`）：
  - `remote_list(app) -> Result<Value, String>` → `{enabled: bool, profiles: [...]}`
  - `remote_upsert(app, profile: Value) -> Result<(), String>`
  - `remote_remove(app, profile_id: String) -> Result<(), String>`
  - `remote_enabled() -> bool`
  - `remote_connect` / `remote_disconnect` / `remote_status`（计划 1 Task 7 已有，这里补闸门与校验统一）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_command_is_absent_when_the_flag_is_off() {
        // 纯函数版本，注入 flag 值，避免测试依赖真实 flags.json。
        assert!(!gate(true, "release"));
        assert!(!gate(false, "dev"));
        assert!(gate(true, "dev"));
    }

    #[test]
    fn a_nogit_build_is_treated_like_release() {
        // spec §5.3：build_kind != "dev" 一律关。
        assert!(!gate(true, "nogit"));
    }

    #[test]
    fn upsert_rejects_a_bad_profile_id() {
        let err = validate_profile_payload(&serde_json::json!({
            "id": "Bad_ID", "label": "x", "host": "h",
            "local_path": "/a", "remote_path": "/b",
            "instance_root": "/c/.yi-agent", "binary_version": "1",
            "remote_arch": "x86_64-unknown-linux-gnu"
        }))
        .unwrap_err();
        assert!(err.contains("Bad_ID"), "{err}");
    }

    #[test]
    fn upsert_rejects_an_unsupported_arch() {
        let err = validate_profile_payload(&serde_json::json!({
            "id": "ok", "label": "x", "host": "h",
            "local_path": "/a", "remote_path": "/b",
            "instance_root": "/c/.yi-agent", "binary_version": "1",
            "remote_arch": "aarch64-unknown-linux-gnu"
        }))
        .unwrap_err();
        assert!(err.contains("aarch64-unknown-linux-gnu"), "{err}");
    }
}
```

- [ ] **Step 2: 实现**

```rust
//! `remote/*` 的 Tauri 命令。
//!
//! **为什么这些不放 app-server**（spec §3.2）：app-server 是被 ssh 运送的那一端，
//! 把"怎么连远端"放进去等于让远端进程决定怎么连自己。而且开关关闭时远端可能
//! 在跑旧二进制，根本不认识这些方法。

use serde_json::Value;

/// 闸门：flag 开着**且**构建是 dev。
///
/// 抽成纯函数是为了可测——真实调用走 `yi_agent_flags::is_enabled("remote_ssh")`。
fn gate(flag_on: bool, build_kind: &str) -> bool {
    flag_on && build_kind == "dev"
}

fn gate_now() -> bool {
    gate(
        yi_agent_flags::is_enabled("remote_ssh"),
        yi_agent_flags::build_kind(),
    )
}

const SUPPORTED_ARCHES: [&str; 3] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
];

/// 校验前端传来的 profile。先做**所有**检查再返回第一个错误——
/// 逐个报错会让设置页反复来回。
fn validate_profile_payload(raw: &Value) -> Result<crate::remote::profile::RemoteProfile, String> {
    use crate::remote::profile::{validate_profile_id, RemoteProfile};
    let get = |k: &str| raw.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();

    let id = get("id");
    validate_profile_id(&id).map_err(|e| e.to_string())?;

    let arch = get("remote_arch");
    if !SUPPORTED_ARCHES.contains(&arch.as_str()) {
        return Err(format!(
            "remote_arch `{arch}` has no prebuilt yi-agent binary; \
             supported: {}",
            SUPPORTED_ARCHES.join(", ")
        ));
    }

    let local_path = get("local_path");
    let p = std::path::Path::new(&local_path);
    if !p.is_absolute() {
        return Err(format!("local_path `{local_path}` must be absolute"));
    }

    Ok(RemoteProfile {
        id,
        label: get("label"),
        host: get("host"),
        local_path: p.to_path_buf(),
        remote_path: get("remote_path"),
        instance_root: get("instance_root"),
        binary_version: get("binary_version"),
        remote_arch: arch,
    })
}
```

命令本体：`remote_list` 与 `remote_enabled` 在闸门关时返回 `{enabled:false, profiles:[]}`（而不是报错——UI 需要知道"功能不可用"才能不渲染入口）；`remote_upsert` / `remote_remove` / `remote_connect` 在闸门关时返回 `Err`。

- [ ] **Step 3: 跑测试**

```bash
cd desktop/src-tauri && cargo test --lib remote::rpc
```

Expected: 4 passed。

- [ ] **Step 4: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/rpc.rs desktop/src-tauri/src/remote/mod.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(remote): expose remote commands behind the dev-only gate"
```

---

### Task 4: `remote/relayTargets`（app-server 唯一新增方法）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/remote.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`
- Test: `rpc/remote.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: `remote.json` 与 `remote-status.json`（只读）
- Produces: `remote/relayTargets` → `{targets: [{profileId, label, session, available}]}`

**关键约束：**
- **`Observe` 不得拿到 `session`**（spec §4.6、§8.9）——它是凭据
- flag 关闭时返回 `-32601`（方法不存在）
- 只读，不加锁

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn targets_json(scope: Scope) -> serde_json::Value {
        build_targets(
            &json!({
                "version": 1,
                "profiles": [{
                    "id": "proj-foo", "label": "foo",
                    "host": "h", "local_path": "/a", "remote_path": "/b",
                    "instance_root": "/c/.yi-agent", "binary_version": "1",
                    "remote_arch": "x86_64-unknown-linux-gnu"
                }]
            }),
            &json!({"statuses": {"proj-foo": "connected"}}),
            "wss://relay/?session=base",
            scope,
        )
    }

    #[test]
    fn control_sees_the_derived_session() {
        let v = targets_json(Scope::Control);
        let t = &v["targets"][0];
        assert_eq!(t["profileId"], "proj-foo");
        assert_eq!(t["available"], true);
        assert!(t["session"].as_str().unwrap().contains("proj-foo"));
    }

    #[test]
    fn observe_never_sees_the_session() {
        // session 就是凭据，只读设备拿到它等于拿到"动"的能力。
        let v = targets_json(Scope::Observe);
        let t = &v["targets"][0];
        assert!(t["session"].is_null(), "Observe must not receive session: {t}");
        // 但 label 与 available 仍然给，UI 才能显示"可见但只读"。
        assert_eq!(t["label"], "foo");
        assert_eq!(t["available"], true);
    }

    #[test]
    fn a_disconnected_project_reports_unavailable() {
        let v = build_targets(
            &json!({"version":1,"profiles":[{"id":"p","label":"p","host":"h","local_path":"/a","remote_path":"/b","instance_root":"/c/.yi-agent","binary_version":"1","remote_arch":"x86_64-unknown-linux-gnu"}]}),
            &json!({"statuses": {}}),
            "wss://r/",
            Scope::Control,
        );
        assert_eq!(v["targets"][0]["available"], false);
    }

    #[test]
    fn an_empty_config_yields_an_empty_list() {
        let v = build_targets(&json!({"version":1,"profiles":[]}), &json!({}), "wss://r/", Scope::Control);
        assert_eq!(v["targets"].as_array().unwrap().len(), 0);
    }
}
```

- [ ] **Step 2: 实现**

```rust
//! `remote/relayTargets`：手机拿"本项目清单 + 每项目的 relay session"。
//!
//! **这是唯一放进 app-server 的 `remote/*` 方法**（spec §4.6）：手机经中继访问
//! 本机 app-server，链路上没有 Tauri 桥接层，只能由 app-server 提供。
//! 它**只读**，返回几个字符串，不碰连接逻辑。

use serde_json::{json, Value};

use crate::protocol::{RpcError, Scope};

/// 从两份 JSON 组装清单。纯函数，可穷举测试。
pub fn build_targets(
    config: &Value,
    status: &Value,
    relay_base: &str,
    scope: Scope,
) -> Value {
    let statuses = status.get("statuses").and_then(|v| v.as_object());
    let targets: Vec<Value> = config
        .get("profiles")
        .and_then(|v| v.as_array())
        .map(|profiles| {
            profiles
                .iter()
                .filter_map(|p| {
                    let id = p.get("id")?.as_str()?;
                    let label = p.get("label").and_then(|v| v.as_str()).unwrap_or(id);
                    let available = statuses
                        .and_then(|s| s.get(id))
                        .and_then(|v| v.as_str())
                        .map(|s| s == "connected")
                        .unwrap_or(false);

                    // Observe 只拿到"看得见"，拿不到凭据。
                    let session = if scope >= Scope::Control {
                        json!(crate::remote_session::derive(relay_base, id))
                    } else {
                        Value::Null
                    };
                    Some(json!({
                        "profileId": id,
                        "label": label,
                        "session": session,
                        "available": available,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "targets": targets })
}
```

`Scope` 需要 `PartialOrd`（spec §4.6 说 `Observe < Control < Admin`，`protocol.rs:18-22` 已定义；若未派生 `Ord`，先补 `#[derive(PartialOrd, Ord)]` 并加一条测试钉住顺序）。

`derive` 的实现在 app-server 侧要独立于 desktop（app-server 不依赖 desktop crate）：把计划 1 里 `remote/profile.rs` 的 `derive_relay` 逻辑复制成 `remote_session.rs` 的纯函数版本，**并在两处测试里用同一组输入断言相同输出**（这是唯一的重复，因为两个 crate 不能互相依赖；在代码注释里写明"必须与 desktop 侧保持同构"）。

- [ ] **Step 3: 注册到 `rpc/mod.rs`**

加 `pub(crate) mod remote;`，并在 `remote::handle` 里按 `rpc/flags.rs` 的模式处理：

```rust
"remote/relayTargets" => {
    // flag 关闭时方法不存在（与 flags_rpc.rs:36 对 stable 的处理同构）。
    if !yi_agent_flags::is_enabled("remote_ssh") {
        return Err(RpcError::method_not_found(req.method.as_str()));
    }
    // ...
}
```

- [ ] **Step 4: 跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib rpc::remote
```

Expected: 4 passed（+ Scope 顺序测试）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/remote.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs yi-agent-rs/crates/yi-agent-app-server/src/remote_session.rs yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs
git commit -m "feat(app-server): add read-only remote/relayTargets"
```

---

### Task 5: 远端模式下不传 `cwd`

**Files:**
- Modify: `desktop/src/App.tsx:956`（`threadStartParams`）
- Modify: `desktop/src/App.tsx:1692`（`thread/lead`）、`ThreadSidebar.tsx:205`、`threadStore.ts:25`
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: 活动槽位（计划 1 Task 8 的 `activeSlotRef`）
- Produces: `threadStartParams(cwd, slot)` 在远端槽位时**省略** `cwd` 键。

**关键（spec §4.4）：** 本机路径送到远端 `canonicalize` 必然失败，用户看到"新建会话失败"且看不出原因。

- [ ] **Step 1: 写失败测试**

```tsx
it("omits cwd for a remote slot so the remote does not canonicalize a local path", () => {
  const params = threadStartParams("/Users/y/proj/foo", "proj-foo");
  expect("cwd" in params).toBe(false);
});

it("still sends cwd for the local slot", () => {
  const params = threadStartParams("/Users/y/proj/foo", "local");
  expect(params.cwd).toBe("/Users/y/proj/foo");
});
```

- [ ] **Step 2: 实现**

```tsx
export function threadStartParams<T extends { cwd?: string }>(
  cwd: string,
  slot: Slot,
  base: T,
): T & { cwd?: string } {
  // 远端模式下**必须**省略 cwd：cwd 会在运行 app-server 的那台机器上被
  // canonicalize，本机路径在远端必然不存在（spec §4.4）。
  // 省略后 resolve_thread_cwd 走 `None => cfg.workdir`，用 ssh 会话的 cwd。
  if (slot !== LOCAL_SLOT) return { ...base };
  return { ...base, cwd };
}
```

- [ ] **Step 3: 处理其余三个入口**

- `thread/lead`（`:1692`）：远端模式下不传 `cwd`（lead 在远端的语义 spec §7 明确**不做**，所以远端模式下直接不提供 lead 入口）
- `onRemoveWorkspace`（`ThreadSidebar.tsx:205`）：远端项目的"移除"走 `remote_remove`，不调本地 workdir 索引
- `threadStore.ts:25` 的 `info.cwd`：直接展示 `thread/start` 返回的远端路径，不回写本机索引

- [ ] **Step 4: 跑测试**

```bash
cd desktop && npx vitest run src/App.test.tsx && npx tsc --noEmit
```

Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx desktop/src/lib/threadStore.ts desktop/src/components/ThreadSidebar.tsx
git commit -m "feat(remote): omit cwd for remote slots"
```

---

### Task 6: 「远程」设置页

**Files:**
- Create: `desktop/src/RemoteSettings.tsx`
- Test: `desktop/src/RemoteSettings.test.tsx`

**Interfaces:**
- Consumes: `remote_list` / `remote_upsert` / `remote_remove` / `remote_connect` / `remote_disconnect`（Task 3）
- Produces: 一个受 `remote_enabled()` 控制的设置页组件。

- [ ] **Step 1: 写失败测试**

```tsx
it("renders nothing when the gate is off", async () => {
  render(<RemoteSettings invoke={async (cmd) => (cmd === "remote_enabled" ? false : null)} />);
  await waitFor(() => expect(screen.queryByText("远程项目")).toBeNull());
});

it("shows the disconnect reason and a manual reconnect button", async () => {
  const invoke = vi.fn(async (cmd: string) => {
    if (cmd === "remote_enabled") return true;
    if (cmd === "remote_list") return {
      enabled: true,
      profiles: [{ id: "proj-foo", label: "foo", status: "disconnected",
                   lastError: "gave up after 5 attempts" }],
    };
    return null;
  });
  render(<RemoteSettings invoke={invoke} />);
  expect(await screen.findByText(/gave up after 5 attempts/)).toBeTruthy();
  expect(screen.getByRole("button", { name: /重连/ })).toBeTruthy();
});
```

- [ ] **Step 2: 实现**

要点（spec §4.3、§5.1）：

- 开关关闭时**整个入口不渲染**（`remote_enabled` 返回 false 就直接 `return null`）
- 每个 profile 显示：label、host、`local_path` → `remote_path`、连接状态、最近一次错误
- 断线时显示**明确原因**与「重连」按钮（不自动无限重试，spec §4.3 第 2 条）
- 安装进度：`Connecting` 状态下列出当前阶段（探测架构 / 下载 / 上传 / 解压 / 启动）
- 新增/编辑表单只暴露必要字段，`instance_root` **不由用户输入**（由 App 派生，spec §4.9）

- [ ] **Step 3: 跑测试**

```bash
cd desktop && npx vitest run src/RemoteSettings.test.tsx
```

Expected: 全绿。

- [ ] **Step 4: Commit**

```bash
git add desktop/src/RemoteSettings.tsx desktop/src/RemoteSettings.test.tsx
git commit -m "feat(remote): add the remote projects settings page"
```

---

### Task 7: `RemoteConfig` 扩展为列表（向后兼容）

**Files:**
- Modify: `desktop/src/lib/remoteConfig.ts`
- Modify: `desktop/src/transportFactory.ts`
- Test: `desktop/src/transportFactory.test.ts`

**Interfaces:**
- Consumes: 现有的 `RemoteConfig {url, token}`
- Produces:
  - `interface RemoteProjectBinding { profileId: string; session: string }`
  - `interface RemoteConfigList { url: string; token: string; projects: RemoteProjectBinding[] }`
  - `parseRemoteConfig` 对旧的 `{url, token}` 返回 `projects: []`（**不是 null**）

**关键（spec §4.6 第 12 步）：** 桌面路径不能被破坏。

- [ ] **Step 1: 写失败测试**

```ts
it("reads a legacy single-binding config as an empty project list", () => {
  const cfg = parseRemoteConfig(JSON.stringify({ url: "wss://r/", token: "t" }));
  expect(cfg).toEqual({ url: "wss://r/", token: "t", projects: [] });
});

it("reads a new multi-project config", () => {
  const cfg = parseRemoteConfig(
    JSON.stringify({ url: "wss://r/", token: "t",
                     projects: [{ profileId: "proj-foo", session: "s1" }] }),
  );
  expect(cfg?.projects).toHaveLength(1);
});

it("drops malformed project entries instead of failing the whole config", () => {
  const cfg = parseRemoteConfig(
    JSON.stringify({ url: "wss://r/", token: "t",
                     projects: [{ profileId: "ok", session: "s" }, { nope: 1 }] }),
  );
  expect(cfg?.projects).toEqual([{ profileId: "ok", session: "s" }]);
});

it("the desktop still gets a tauri transport when nothing is stored", () => {
  const t = transportFactory({ storage: { getItem: () => null, setItem: () => {}, removeItem: () => {} } });
  // 桌面分支的关键行为：不是 ws transport。
  expect(t).toBeDefined();
});
```

- [ ] **Step 2: 实现**

`parseRemoteConfig` 里：`projects` 字段缺失或不是数组 → `[]`；数组里每项必须是 `{profileId: string, session: string}` 且都非空，否则丢弃该项（**不丢整个配置**）。

`transportFactory` 增加一个可选的重载：`transportFactory({ slot })` 时，若 `slot !== LOCAL_SLOT` 且有对应 binding，返回 `wsTransport(relayUrlFor(binding), token)`；否则回落到原逻辑。

- [ ] **Step 3: 跑测试**

```bash
cd desktop && npx vitest run src/transportFactory.test.ts
```

Expected: 全绿，**包括所有既有用例**（不破坏桌面分支）。

- [ ] **Step 4: Commit**

```bash
git add desktop/src/lib/remoteConfig.ts desktop/src/transportFactory.ts desktop/src/transportFactory.test.ts
git commit -m "feat(remote): let the phone hold one binding per project"
```

---

### Task 8: 手机端为每个项目各建一条连接

**Files:**
- Modify: `desktop/src/App.tsx`（远端客户端的 client 建立逻辑）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 7 的列表配置、`remote/relayTargets`
- Produces: 手机启动时按清单并发建 N 条 `RpcClient`。

- [ ] **Step 1: 实现**

流程：

1. 先用主 binding 连上本机（现有流程不变）
2. 调 `remote/relayTargets` 拿 `[{profileId, label, session, available}]`
3. 对每个 `available && session !== null` 的项，用 `wsTransport(relayUrl, token)` 建一条连接，槽位名用 `profileId`
4. `session` 为 null（Observe scope）的项目**只显示不可连接**，不建 client
5. 某个项目连不上时，只标记该项目不可用，**不影响其它项目**

- [ ] **Step 2: 验证桌面路径未被破坏**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```

Expected: 全绿。特别确认 `tauriTransport()` 分支的既有测试仍过。

- [ ] **Step 3: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(remote): connect one client per remote project on mobile"
```

---

## 自检（Self-Review）

**1. Spec 覆盖**

| spec 要求 | 任务 |
|---|---|
| §4.3 远端重试 60s 上限 + 5 次停止 + 手动重连 | Task 1、Task 6 |
| §4.4 远端不传 cwd（四个入口） | Task 5 |
| §4.6 relayTargets + Observe 不给 session | Task 4 |
| §4.6 available 来自状态旁路 | Task 2 |
| §5.1 关 flag 杀全部 slot + relayTargets 空 | Task 3（闸门）、计划 1 Task 9 |
| §5.2 flag 在本机判定 | Task 3（`gate`） |
| §4.6 第 12 步 RemoteConfig 扩列表 + 桌面不受影响 | Task 7、Task 8 |

**2. 占位符扫描**：无 TBD。Task 6 / Task 8 的实现步骤以要点形式给出（它们主要是 UI 组装与连接编排），但每条要点都指定了具体行为和断言，不是"适当处理"。

**3. 类型一致性**：
- `SlotStatus` 三值在 Task 2 定义、`build_targets` 的 `available` 判定（`== "connected"`）一致
- `gate(flag_on, build_kind)` 在 Task 3 定义与测试一致
- `RemoteConfig.projects` 在 Task 7 定义、Task 8 消费一致
- `derive_relay`（desktop）与 `remote_session::derive`（app-server）是**刻意的两处实现**，已在 Task 4 注释里写明必须同构，并各配测试

**已知取舍**：Task 4 的 `remote_session::derive` 与计划 1 的 `derive_relay` 重复。两个 crate 不能互相依赖（app-server 不应依赖 desktop），所以这是必要的重复；防漂移靠"两处用同一组输入、断言相同输出"的测试对。
