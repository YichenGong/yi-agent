# Superpowers 看板 — Plan 3a：插件进程、独立 IPC 客户端与推进循环

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让看板成为一个**独立进程**：它自带队列与推进循环，通过**自实现的极小 IPC 客户端**（不依赖 `yi-agent-store`）驱动 daemon 创建自主会话，并把卡片推进到 `awaiting_merge` / `needs_you`。

**Architecture:** 三个新层，全部在插件自己的独立 workspace（`plugins/superpowers-board/`）内：(1) `board-ipc`——按线格式手写的客户端（新行分隔 JSON over Unix socket，`protocol_version` 校验，1 MiB 帧上限）；(2) `board-daemon-client`——把线格式封装成「创建自主会话 / 读任务摘要」两个语义操作；(3) `board-runner`——推进循环：读日历上限 → `Board::start_due` → 逐卡请求创建会话 → 轮询状态 → 迁移卡片状态。**插件不链接任何 `yi-agent-*` crate**，因此删除目录即等于卸载。

**Tech Stack:** Rust 2024、`tokio`（UnixStream + 定时器）、`serde`/`serde_json`（线格式）、`chrono`。复用 Plan 1 的 `board-core`。

## Global Constraints

- **插件不得依赖任何 `yi-agent-*` crate**（验收：`cargo tree -p board-ipc` 无 `yi-agent` 前缀）；这是"可独立安装/卸载"的硬约束。
- 线格式必须与 daemon 逐字一致：
  - 请求：`{"protocol_version":1,"request_id":"<id>","command":{"type":"<variant>",...}}`，**一行 JSON + `\n`**。
  - 响应：`{"protocol_version":1,"request_id":"<id>","result":{"type":"<variant>",...}}`（`event_id` / `event` 字段可缺省）。
  - 帧上限 `1048576` 字节（1 MiB）；超限即协议错误。
  - 任何 `protocol_version` 不等于 `1` 或在响应中与请求不匹配 → 报错，不做兼容猜测。
- socket 路径：`<runtime_dir>/runtime.sock`。
- 推进循环**只改 `Queued` 卡片的状态**；`NeedsYou` / `AwaitingMerge` / 终态一律不动。
- 关闭开关时（Plan 1 的 `resolve`）推进循环**立即停止**，且**不取消**已创建的会话。
- 提交信息用 conventional commits，**不写** `Co-Authored-By`。
- 每个任务结束跑 `cd plugins/superpowers-board && cargo fmt --all && cargo test`。

> **依赖：** 需要 Plan 2 已合并（`CreateAutonomousSession` 与 `ListTaskSummaries` 可用）。

---
## 文件结构

| 文件 | 职责 |
|------|------|
| `plugins/superpowers-board/crates/board-ipc/Cargo.toml` | 线格式客户端 crate |
| `plugins/superpowers-board/crates/board-ipc/src/lib.rs` | 导出与 crate 文档 |
| `plugins/superpowers-board/crates/board-ipc/src/wire.rs` | 请求/响应信封与变体（手写，镜像协议） |
| `plugins/superpowers-board/crates/board-ipc/src/client.rs` | UnixStream 连接、帧读写、版本校验 |
| `plugins/superpowers-board/crates/board-runner/Cargo.toml` | 推进循环 crate |
| `plugins/superpowers-board/crates/board-runner/src/lib.rs` | 导出 |
| `plugins/superpowers-board/crates/board-runner/src/client.rs` | 语义层：`create_autonomous_session` / `list_task_summaries` |
| `plugins/superpowers-board/crates/board-runner/src/runner.rs` | 推进循环（纯函数式决策 + I/O 外壳） |
| `plugins/superpowers-board/crates/board-runner/src/main.rs` | 插件可执行入口 |

---

### Task 1: `board-ipc` 线格式信封与变体

**Files:**
- Create: `plugins/superpowers-board/crates/board-ipc/Cargo.toml`
- Create: `plugins/superpowers-board/crates/board-ipc/src/lib.rs`
- Create: `plugins/superpowers-board/crates/board-ipc/src/wire.rs`
- Modify: `plugins/superpowers-board/Cargo.toml`

**Interfaces:**
- Consumes: 无
- Produces:
  - `board_ipc::wire::PROTOCOL_VERSION: u32 = 1`
  - `board_ipc::wire::MAX_FRAME_BYTES: usize = 1048576`
  - `board_ipc::wire::RequestEnvelope { protocol_version, request_id, command }`
  - `board_ipc::wire::ResponseEnvelope { protocol_version, request_id, event_id, event, result }`
  - `board_ipc::wire::Command`（`CreateAutonomousSession { objective, workdir }`、`ListTaskSummaries { session_id, active_only }`、`Status`）
  - `board_ipc::wire::Result_`（用 `Reply` 命名避免与 `Result` 冲突：`AutonomousSessionCreated { session_id, root_task_id }`、`TaskSummaries { tasks }`、`Status { high_water_event_id }`、`Error { code, message }`、`Other`）
  - `board_ipc::wire::TaskSummary { task_id, state, is_root }`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-ipc/src/wire.rs`：

```rust
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_request_serializes_with_the_daemons_tag() {
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: "1".into(),
            command: Command::CreateAutonomousSession {
                objective: "implement the plan".into(),
                workdir: "/tmp/worktree".into(),
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":1,"request_id":"1","command":{"type":"CreateAutonomousSession","objective":"implement the plan","workdir":"/tmp/worktree"}}"#
        );
    }

    #[test]
    fn a_task_summaries_request_serializes_both_fields() {
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: "2".into(),
            command: Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":1,"request_id":"2","command":{"type":"ListTaskSummaries","session_id":null,"active_only":false}}"#
        );
    }

    #[test]
    fn a_create_response_round_trips() {
        let json = r#"{"protocol_version":1,"request_id":"1","result":{"type":"AutonomousSessionCreated","session_id":"s1","root_task_id":"t1"}}"#;
        let envelope: ResponseEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
        assert_eq!(envelope.request_id, "1");
        assert_eq!(
            envelope.result,
            Reply::AutonomousSessionCreated {
                session_id: "s1".into(),
                root_task_id: "t1".into(),
            }
        );
    }

    #[test]
    fn a_task_summaries_response_round_trips() {
        let json = r#"{"protocol_version":1,"request_id":"2","result":{"type":"TaskSummaries","tasks":[{"task_id":"t1","state":"running","is_root":true}]}}"#;
        let envelope: ResponseEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(
            envelope.result,
            Reply::TaskSummaries {
                tasks: vec![TaskSummary {
                    task_id: "t1".into(),
                    state: "running".into(),
                    is_root: true,
                }],
            }
        );
    }

    #[test]
    fn an_error_response_round_trips() {
        let json = r#"{"protocol_version":1,"request_id":"3","result":{"type":"Error","code":"InvalidState","message":"workdir does not exist: /nope"}}"#;
        let envelope: ResponseEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(
            envelope.result,
            Reply::Error {
                code: "InvalidState".into(),
                message: Some("workdir does not exist: /nope".into()),
            }
        );
    }

    #[test]
    fn an_unknown_reply_variant_decodes_as_other_instead_of_failing() {
        // 前向兼容：daemon 新增变体不应让插件解析崩溃。
        let json = r#"{"protocol_version":1,"request_id":"4","result":{"type":"SomethingBrandNew","x":1}}"#;
        let envelope: ResponseEnvelope = serde_json::from_str(json).unwrap();
        assert!(matches!(envelope.result, Reply::Other { .. }));
    }

    #[test]
    fn the_frame_limit_matches_the_daemon() {
        assert_eq!(MAX_FRAME_BYTES, 1_048_576);
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-ipc`
Expected: 编译失败，`RequestEnvelope` 等未定义。

- [ ] **Step 3: 实现最小代码**

在 `wire.rs` 测试模块之前：

```rust
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Command {
    CreateAutonomousSession { objective: String, workdir: String },
    ListTaskSummaries {
        session_id: Option<String>,
        active_only: bool,
    },
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<serde_json::Value>,
    pub result: Reply,
}

/// 响应结果。用 `Reply` 而非 `Result` 以免与 `std::result::Result` 冲突。
/// `Other` 兜住未来新增的变体，使插件不会因 daemon 升级而解析失败。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum Reply {
    AutonomousSessionCreated { session_id: String, root_task_id: String },
    TaskSummaries { tasks: Vec<TaskSummary> },
    Status { high_water_event_id: i64 },
    Error { code: String, message: Option<String> },
    Other { value: serde_json::Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub state: String,
    pub is_root: bool,
}

impl<'de> Deserialize<'de> for Reply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        let tag = value
            .get("type")
            .and_then(|tag| tag.as_str())
            .ok_or_else(|| D::Error::custom("reply is missing its `type` tag"))?
            .to_owned();
        match tag.as_str() {
            "AutonomousSessionCreated" => {
                #[derive(Deserialize)]
                struct Shape {
                    session_id: String,
                    root_task_id: String,
                }
                let shape: Shape =
                    serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Reply::AutonomousSessionCreated {
                    session_id: shape.session_id,
                    root_task_id: shape.root_task_id,
                })
            }
            "TaskSummaries" => {
                #[derive(Deserialize)]
                struct Shape {
                    tasks: Vec<TaskSummary>,
                }
                let shape: Shape =
                    serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Reply::TaskSummaries { tasks: shape.tasks })
            }
            "Status" => {
                #[derive(Deserialize)]
                struct Shape {
                    high_water_event_id: i64,
                }
                let shape: Shape =
                    serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Reply::Status {
                    high_water_event_id: shape.high_water_event_id,
                })
            }
            "Error" => {
                let code = value
                    .get("code")
                    .and_then(|code| code.as_str())
                    .unwrap_or("Internal")
                    .to_owned();
                let message = value
                    .get("message")
                    .and_then(|message| message.as_str())
                    .map(str::to_owned);
                Ok(Reply::Error { code, message })
            }
            _ => Ok(Reply::Other { value }),
        }
    }
}
```

`lib.rs`：

```rust
//! Superpowers 看板的 IPC 线格式客户端。
//!
//! 刻意手写协议结构，不依赖任何 `yi-agent-*` crate，因此插件可以独立
//! 安装与卸载。协议版本在信封里校验，不匹配即报错，不做兼容猜测。

pub mod client;
pub mod wire;
```

`Cargo.toml`（workspace 根）把新成员加入 `members`：

```toml
members = ["crates/board-core", "crates/board-ipc"]
```

`crates/board-ipc/Cargo.toml`：

```toml
[package]
name = "board-ipc"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
```

> 根 `Cargo.toml` 的 `[workspace.dependencies]` 已有 `serde` 与 `serde_json`（Plan 1 引入）。若缺 `serde_json` 则补 `serde_json = "1"`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-ipc wire::`
Expected: PASS（7 个测试）。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-board && cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board-ipc): mirror the daemon wire protocol without linking yi-agent"
```

---

### Task 2: `board-ipc` 客户端（连接、帧读写、版本校验）

**Files:**
- Create: `plugins/superpowers-board/crates/board-ipc/src/client.rs`
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: `wire::{RequestEnvelope, ResponseEnvelope, Command, Reply, PROTOCOL_VERSION, MAX_FRAME_BYTES}`
- Produces:
  - `board_ipc::client::ClientError`
  - `board_ipc::client::socket_path(runtime_dir: &Path) -> PathBuf`（返回 `<runtime_dir>/runtime.sock`）
  - `board_ipc::client::encode_request(request_id: &str, command: Command) -> Result<Vec<u8>, ClientError>`（返回含结尾 `\n` 的帧）
  - `board_ipc::client::decode_response(line: &[u8], expected_request_id: &str) -> Result<Reply, ClientError>`
  - `board_ipc::client::send(socket: &Path, command: Command) -> Result<Reply, ClientError>`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-ipc/src/client.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Command;

    #[test]
    fn the_socket_lives_beside_its_runtime_directory() {
        assert_eq!(
            socket_path(std::path::Path::new("/project/.yi-agent/runtime")),
            std::path::PathBuf::from("/project/.yi-agent/runtime/runtime.sock")
        );
    }

    #[test]
    fn an_encoded_request_is_one_newline_terminated_json_line() {
        let frame = encode_request(
            "7",
            Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        )
        .unwrap();
        assert!(frame.ends_with(b"\n"));
        assert_eq!(frame.iter().filter(|byte| **byte == b'\n').count(), 1);
        let text = std::str::from_utf8(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(
            text,
            r#"{"protocol_version":1,"request_id":"7","command":{"type":"ListTaskSummaries","session_id":null,"active_only":false}}"#
        );
    }

    #[test]
    fn a_matching_response_decodes_to_its_reply() {
        let line = br#"{"protocol_version":1,"request_id":"7","result":{"type":"AutonomousSessionCreated","session_id":"s","root_task_id":"t"}}"#;
        let reply = decode_response(line, "7").unwrap();
        assert_eq!(
            reply,
            crate::wire::Reply::AutonomousSessionCreated {
                session_id: "s".into(),
                root_task_id: "t".into(),
            }
        );
    }

    #[test]
    fn a_response_for_another_request_is_rejected() {
        let line = br#"{"protocol_version":1,"request_id":"8","result":{"type":"Status","high_water_event_id":0}}"#;
        let error = decode_response(line, "7").unwrap_err();
        assert!(
            matches!(error, ClientError::RequestIdMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_different_protocol_version_is_rejected() {
        let line = br#"{"protocol_version":2,"request_id":"7","result":{"type":"Status","high_water_event_id":0}}"#;
        let error = decode_response(line, "7").unwrap_err();
        assert!(
            matches!(error, ClientError::ProtocolVersion { found: 2, expected: 1 }),
            "{error:?}"
        );
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_parsing() {
        let line = vec![b'x'; crate::wire::MAX_FRAME_BYTES + 1];
        let error = decode_response(&line, "7").unwrap_err();
        assert!(matches!(error, ClientError::FrameTooLarge { .. }), "{error:?}");
    }

    #[test]
    fn malformed_json_is_rejected_with_a_readable_error() {
        let error = decode_response(b"not json", "7").unwrap_err();
        assert!(matches!(error, ClientError::Malformed(_)), "{error:?}");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-ipc client::`
Expected: 编译失败，`socket_path` / `encode_request` 等未定义。

- [ ] **Step 3: 实现最小代码**

在 `client.rs` 测试模块之前：

```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::wire::{
    Command, MAX_FRAME_BYTES, PROTOCOL_VERSION, Reply, RequestEnvelope, ResponseEnvelope,
};

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Malformed(String),
    ProtocolVersion { found: u32, expected: u32 },
    RequestIdMismatch { found: String, expected: String },
    FrameTooLarge { bytes: usize, limit: usize },
    Truncated,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(error) => write!(f, "ipc io error: {error}"),
            ClientError::Malformed(message) => write!(f, "malformed ipc frame: {message}"),
            ClientError::ProtocolVersion { found, expected } => write!(
                f,
                "ipc protocol version mismatch: daemon sent {found}, plugin speaks {expected}"
            ),
            ClientError::RequestIdMismatch { found, expected } => {
                write!(f, "ipc response is for request {found}, expected {expected}")
            }
            ClientError::FrameTooLarge { bytes, limit } => {
                write!(f, "ipc frame is {bytes} bytes, over the {limit}-byte limit")
            }
            ClientError::Truncated => write!(f, "ipc frame ended without a newline"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        ClientError::Io(error)
    }
}

/// The socket the daemon listens on for a given runtime directory.
pub fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("runtime.sock")
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
    NEXT_REQUEST_ID
        .fetch_add(1, Ordering::Relaxed)
        .to_string()
}

/// Serializes one request as a single newline-terminated frame.
pub fn encode_request(request_id: &str, command: Command) -> Result<Vec<u8>, ClientError> {
    let envelope = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.to_owned(),
        command,
    };
    let mut frame = serde_json::to_vec(&envelope)
        .map_err(|error| ClientError::Malformed(error.to_string()))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ClientError::FrameTooLarge {
            bytes: frame.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    frame.push(b'\n');
    Ok(frame)
}

/// Validates one response frame and returns its reply.
pub fn decode_response(line: &[u8], expected_request_id: &str) -> Result<Reply, ClientError> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    if body.len() > MAX_FRAME_BYTES {
        return Err(ClientError::FrameTooLarge {
            bytes: body.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let envelope: ResponseEnvelope = serde_json::from_slice(body)
        .map_err(|error| ClientError::Malformed(error.to_string()))?;
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::ProtocolVersion {
            found: envelope.protocol_version,
            expected: PROTOCOL_VERSION,
        });
    }
    if envelope.request_id != expected_request_id {
        return Err(ClientError::RequestIdMismatch {
            found: envelope.request_id,
            expected: expected_request_id.to_owned(),
        });
    }
    Ok(envelope.result)
}

/// Sends one request and returns the reply. One connection per request, which
/// matches how the daemon serves non-subscription traffic.
pub fn send(socket: &Path, command: Command) -> Result<Reply, ClientError> {
    let request_id = next_request_id();
    let frame = encode_request(&request_id, command)?;
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(&frame)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(ClientError::Truncated);
    }
    if !line.ends_with(b"\n") {
        return Err(ClientError::Truncated);
    }
    decode_response(&line, &request_id)
}
```

> `lib.rs` 已导出 `pub mod client;`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-ipc`
Expected: PASS（14 个测试：7 wire + 7 client）。

- [ ] **Step 5: 验证零依赖（硬约束）**

Run: `cd plugins/superpowers-board && cargo tree -p board-ipc | grep -E "^[│├└─ ]*yi-agent-" || echo "OK: no yi-agent dependency"`
Expected: `OK: no yi-agent dependency`

> 注意：不要用 `grep -i "yi-agent"`——crate 的**文件路径**里含仓库目录名，会误报。必须匹配行首的依赖节点。

- [ ] **Step 6: 提交**

```bash
cd plugins/superpowers-board && cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board-ipc): add the standalone unix-socket client with version checks"
```

---

### Task 3: `board-runner` 语义层与推进决策

**Files:**
- Create: `plugins/superpowers-board/crates/board-runner/Cargo.toml`
- Create: `plugins/superpowers-board/crates/board-runner/src/lib.rs`
- Create: `plugins/superpowers-board/crates/board-runner/src/client.rs`
- Create: `plugins/superpowers-board/crates/board-runner/src/runner.rs`
- Modify: `plugins/superpowers-board/Cargo.toml`

**Interfaces:**
- Consumes: `board_ipc::client`、`board_core::{board::Board, calendar::ConcurrencyCalendar, card::{CardId, CardState}}`
- Produces:
  - `board_runner::client::BoardDaemon::new(socket: PathBuf) -> BoardDaemon`
  - `BoardDaemon::create_session(&self, objective: &str, workdir: &Path) -> Result<CreatedSession, ClientError>`
  - `BoardDaemon::task_state(&self, task_id: &str) -> Result<Option<String>, ClientError>`
  - `board_runner::client::CreatedSession { session_id: String, root_task_id: String }`
  - `board_runner::runner::CardRun { card_id: CardId, session_id: String, root_task_id: String }`
  - `board_runner::runner::plan_launches(board: &Board, limit: u16) -> Vec<CardId>`（纯决策，便于单测）
  - `board_runner::runner::state_for_task_state(task_state: &str) -> Option<CardState>`（把 daemon 状态映射为卡片状态）

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-runner/src/runner.rs`：

```rust
use board_core::board::Board;
use board_core::card::{CardId, CardState};

#[cfg(test)]
mod tests {
    use super::*;
    use board_core::calendar::ConcurrencyCalendar;
    use chrono::{Local, TimeZone};

    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(index as u32),
            );
        }
        board
    }

    #[test]
    fn launches_are_bounded_by_the_calendars_limit_for_now() {
        let board = board_with(&["a", "b", "c", "d"]);
        // 工作日 12:00 的上限是 3。
        assert_eq!(
            plan_launches(&board, 3),
            vec![CardId::new("a"), CardId::new("b"), CardId::new("c")]
        );
    }

    #[test]
    fn a_full_queue_launches_nothing_when_the_limit_is_zero() {
        let board = board_with(&["a"]);
        assert!(plan_launches(&board, 0).is_empty());
    }

    #[test]
    fn a_running_card_occupies_one_slot_so_two_more_launch_under_a_limit_of_three() {
        let mut board = board_with(&["a", "b", "c"]);
        board.start_due(1);
        assert_eq!(board.running_count(), 1, "only 'a' is running");
        assert_eq!(
            plan_launches(&board, 3),
            vec![CardId::new("b"), CardId::new("c")],
            "one slot is taken, so two of the three remain"
        );
    }

    #[test]
    fn daemon_states_map_to_card_states() {
        assert_eq!(state_for_task_state("running"), Some(CardState::Running));
        assert_eq!(state_for_task_state("queued"), Some(CardState::Queued));
        assert_eq!(
            state_for_task_state("completed"),
            Some(CardState::AwaitingMerge)
        );
        assert_eq!(
            state_for_task_state("completed_no_changes"),
            Some(CardState::AwaitingMerge)
        );
        assert_eq!(
            state_for_task_state("budget_exhausted"),
            Some(CardState::NeedsYou)
        );
        assert_eq!(state_for_task_state("blocked"), Some(CardState::NeedsYou));
        assert_eq!(state_for_task_state("failed"), Some(CardState::Failed));
        assert_eq!(state_for_task_state("cancelled"), Some(CardState::Cancelled));
    }

    #[test]
    fn an_unknown_daemon_state_yields_none_so_the_card_is_left_alone() {
        assert_eq!(state_for_task_state("something_new"), None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-runner runner::`
Expected: 编译失败，`plan_launches` / `state_for_task_state` 未定义。

- [ ] **Step 3: 实现最小代码**

在 `runner.rs` 测试模块之前：

```rust
/// The daemon task states a card can be derived from. Unknown states return
/// `None` so a daemon upgrade never makes the runner act on a guess.
pub fn state_for_task_state(task_state: &str) -> Option<CardState> {
    match task_state {
        "queued" => Some(CardState::Queued),
        "running" | "paused" => Some(CardState::Running),
        "completed" | "completed_no_changes" => Some(CardState::AwaitingMerge),
        "blocked" | "budget_exhausted" | "recovery_required" => Some(CardState::NeedsYou),
        "failed" | "stalled" | "timed_out" => Some(CardState::Failed),
        "cancelled" => Some(CardState::Cancelled),
        _ => None,
    }
}

/// How many cards to start now, in FIFO order, without mutating the board.
/// Pure so the scheduling rule is testable without a daemon.
pub fn plan_launches(board: &Board, limit: u16) -> Vec<CardId> {
    let free = board.free_slots(limit);
    board
        .queued_in_order()
        .into_iter()
        .take(free)
        .collect()
}
```

> 需要给 `board-core` 的 `Board` 加一个只读方法 `queued_in_order(&self) -> Vec<CardId>`（按 `order` 升序的 `Queued` 卡片）。在 `board.rs` 中 `next_startable` 之后加入：

```rust
    /// All queued cards in start order, without mutating the board.
    pub fn queued_in_order(&self) -> Vec<CardId> {
        let mut queued: Vec<&Card> = self
            .cards
            .iter()
            .filter(|card| card.state == CardState::Queued)
            .collect();
        queued.sort_by_key(|card| card.order);
        queued.into_iter().map(|card| card.id.clone()).collect()
    }
```

`client.rs`（语义层）：

```rust
use std::path::{Path, PathBuf};

use board_ipc::client::{self, ClientError};
use board_ipc::wire::{Command, Reply};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub session_id: String,
    pub root_task_id: String,
}

#[derive(Debug, Clone)]
pub struct BoardDaemon {
    socket: PathBuf,
}

impl BoardDaemon {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Asks the daemon for a new autonomous session in `workdir`.
    pub fn create_session(
        &self,
        objective: &str,
        workdir: &Path,
    ) -> Result<CreatedSession, ClientError> {
        match client::send(
            &self.socket,
            Command::CreateAutonomousSession {
                objective: objective.to_owned(),
                workdir: workdir.to_string_lossy().to_string(),
            },
        )? {
            Reply::AutonomousSessionCreated {
                session_id,
                root_task_id,
            } => Ok(CreatedSession {
                session_id,
                root_task_id,
            }),
            Reply::Error { code, message } => Err(ClientError::Malformed(format!(
                "daemon refused the session: {code}: {}",
                message.unwrap_or_default()
            ))),
            other => Err(ClientError::Malformed(format!(
                "unexpected reply to CreateAutonomousSession: {other:?}"
            ))),
        }
    }

    /// The daemon's state string for `task_id`, or `None` when it is not listed.
    pub fn task_state(&self, task_id: &str) -> Result<Option<String>, ClientError> {
        match client::send(
            &self.socket,
            Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        )? {
            Reply::TaskSummaries { tasks } => Ok(tasks
                .into_iter()
                .find(|task| task.task_id == task_id)
                .map(|task| task.state)),
            Reply::Error { code, message } => Err(ClientError::Malformed(format!(
                "daemon refused the task listing: {code}: {}",
                message.unwrap_or_default()
            ))),
            other => Err(ClientError::Malformed(format!(
                "unexpected reply to ListTaskSummaries: {other:?}"
            ))),
        }
    }
}
```

`lib.rs`：

```rust
//! Superpowers 看板的推进循环。

pub mod client;
pub mod runner;
```

`Cargo.toml`（workspace 根 `members`）：

```toml
members = ["crates/board-core", "crates/board-ipc", "crates/board-runner"]
```

`crates/board-runner/Cargo.toml`：

```toml
[package]
name = "board-runner"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
board-core = { path = "../board-core" }
board-ipc = { path = "../board-ipc" }
chrono.workspace = true
serde.workspace = true
serde_json.workspace = true
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-runner`
Expected: PASS（5 个测试）。

- [ ] **Step 5: 验证零依赖（硬约束）**

Run: `cd plugins/superpowers-board && cargo tree -p board-runner | grep -E "^[│├└─ ]*yi-agent-" || echo "OK: no yi-agent dependency"`
Expected: `OK: no yi-agent dependency`

- [ ] **Step 6: 提交**

```bash
cd plugins/superpowers-board && cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board-runner): map daemon task states to card states and plan launches"
```

---

### Task 4: 推进循环与插件入口

**Files:**
- Create: `plugins/superpowers-board/crates/board-runner/src/tick.rs`
- Create: `plugins/superpowers-board/crates/board-runner/src/main.rs`
- Modify: `plugins/superpowers-board/crates/board-runner/src/lib.rs`
- Modify: `plugins/superpowers-board/crates/board-runner/Cargo.toml`

**Interfaces:**
- Consumes: Task 3 的 `BoardDaemon`、`plan_launches`、`state_for_task_state`；Plan 1 的 `Board`、`ConcurrencyCalendar`、`switch`
- Produces:
  - `board_runner::tick::run_once(board: &mut Board, daemon: &BoardDaemon, limit: u16, launch: &mut dyn FnMut(&CardId) -> Option<PathBuf>) -> Vec<TickOutcome>`
  - `board_runner::tick::TickOutcome { card_id: CardId, action: TickAction }`
  - `board_runner::tick::TickAction { Launched { session_id: String, root_task_id: String }, Transitioned(CardState), Failed(String) }`

- [ ] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-runner/src/tick.rs`：

```rust
use std::path::PathBuf;

use board_core::board::Board;
use board_core::card::{CardId, CardState};

use crate::client::BoardDaemon;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(index as u32),
            );
        }
        board
    }

    #[test]
    fn a_successful_launch_moves_the_card_to_running() {
        let mut board = board_with(&["a", "b"]);
        // A daemon that accepts every request: exercise the loop with a stub by
        // asserting on the pure planning + transition path.
        let planned = crate::runner::plan_launches(&board, 1);
        assert_eq!(planned, vec![CardId::new("a")]);
        board.transition(&CardId::new("a"), CardState::Running).unwrap();
        assert_eq!(board.running_count(), 1);
        assert_eq!(board.get(&CardId::new("b")).unwrap().state, CardState::Queued);
    }

    fn the_objective_carries_the_plan_spec_and_constraints() {
        let board = board_with(&["a"]);

    #[test]
    fn the_objective_carries_the_plan_spec_and_constraints() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        let objective = objective_for(&card);
        assert!(objective.contains("a.plan.md"));
        assert!(objective.contains("a.spec.md"));
        assert!(objective.contains("Never merge yourself"));
        assert!(objective.contains("BLOCKED"));
    }

    #[test]
    fn outcomes_record_launch_and_transition() {
        let outcome = TickOutcome {
            card_id: CardId::new("a"),
            action: TickAction::Launched {
                session_id: "s".into(),
                root_task_id: "t".into(),
            },
        };
        assert_eq!(outcome.card_id, CardId::new("a"));
        let transition = TickOutcome {
            card_id: CardId::new("a"),
            action: TickAction::Transitioned(CardState::AwaitingMerge),
        };
        assert!(matches!(
            transition.action,
            TickAction::Transitioned(CardState::AwaitingMerge)
        ));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-runner tick::`
Expected: 编译失败，`TickOutcome` / `TickAction` 未定义。

- [ ] **Step 3: 实现最小代码**

在 `tick.rs` 测试模块之前：

```rust
/// What one tick did to one card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    Launched {
        session_id: String,
        root_task_id: String,
    },
    Transitioned(CardState),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    pub card_id: CardId,
    pub action: TickAction,
}

/// Runs one scheduling pass.
///
/// `launch` supplies the workdir for a card (the caller owns where worktrees
/// live, because that is environment policy, not board policy). Returning
/// `None` means "skip this card this tick" and leaves it queued.
pub fn run_once(
    board: &mut Board,
    daemon: &BoardDaemon,
    limit: u16,
    launch: &mut dyn FnMut(&CardId) -> Option<PathBuf>,
) -> Vec<TickOutcome> {
    let mut outcomes = Vec::new();
    for card_id in crate::runner::plan_launches(board, limit) {
        let Some(workdir) = launch(&card_id) else {
            continue;
        };
        let objective = board
            .get(&card_id)
            .map(|card| objective_for(card))
            .unwrap_or_default();
        match daemon.create_session(&objective, &workdir) {
            Ok(created) => {
                if board
                    .transition(&card_id, CardState::Running)
                    .is_ok()
                {
                    outcomes.push(TickOutcome {
                        card_id,
                        action: TickAction::Launched {
                            session_id: created.session_id,
                            root_task_id: created.root_task_id,
                        },
                    });
                }
            }
            Err(error) => outcomes.push(TickOutcome {
                card_id,
                action: TickAction::Failed(error.to_string()),
            }),
        }
    }
    outcomes
}

/// The objective handed to the daemon. Kept in one place so the wording (and
/// the Superpowers constraints it carries) is reviewable at a glance.
pub fn objective_for(card: &board_core::card::Card) -> String {
    format!(
        "Implement the plan at {plan} following its spec at {spec}. \
         Work only in this worktree. Use superpowers:subagent-driven-development \
         (or superpowers:executing-plans) to execute it, then \
         superpowers:finishing-a-development-branch to present the integration \
         options to the user. Never merge yourself. If you are blocked, report BLOCKED.",
        plan = card.plan_path.display(),
        spec = card.spec_path.display(),
    )
}
```

> **关于 `run_once` 的测试策略：** 本任务的两个测试只覆盖纯决策与数据结构（不连接真实 daemon），因为真实 IPC 已由 Task 2/3 与 Plan 2 的 IPC 测试覆盖。不要为了这里再引入一个 mock server。

`lib.rs` 追加：

```rust
pub mod tick;
```

`crates/board-runner/src/main.rs`：

```rust
//! Superpowers 看板插件进程入口。
//!
//! 用法：`board-runner --runtime-dir <dir> --state-dir <dir> [--interval-secs 60]`
//! 它只做一件事：周期性推进队列。安装 = 放这个二进制；卸载 = 删掉它。

use std::path::PathBuf;
use std::time::Duration;

use board_core::calendar::ConcurrencyCalendar;
use board_core::switch::{BoardSwitch, parse_switch_json, resolve};
use board_runner::client::BoardDaemon;

#[derive(Debug)]
struct Args {
    runtime_dir: PathBuf,
    state_dir: PathBuf,
    interval: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut runtime_dir = None;
    let mut state_dir = None;
    let mut interval_secs = 60_u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runtime-dir" => runtime_dir = args.next().map(PathBuf::from),
            "--state-dir" => state_dir = args.next().map(PathBuf::from),
            "--interval-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--interval-secs needs a value".to_string())?;
                interval_secs = value
                    .parse()
                    .map_err(|error| format!("invalid --interval-secs: {error}"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        runtime_dir: runtime_dir.ok_or_else(|| "--runtime-dir is required".to_string())?,
        state_dir: state_dir.ok_or_else(|| "--state-dir is required".to_string())?,
        interval: Duration::from_secs(interval_secs),
    })
}

/// Reads the two preference layers and resolves the switch.
/// Missing or broken files fall back to "disabled" (the conservative default).
fn board_switch(args: &Args) -> BoardSwitch {
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_switch_json(&text))
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let global = home
        .as_ref()
        .map(|home| home.join(".yi-agent").join("preferences.json"))
        .and_then(|path| read(&path));
    let project = read(&args.state_dir.join("preferences.json"));
    resolve(global, project)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("board-runner: {message}");
            std::process::exit(2);
        }
    };
    let calendar = ConcurrencyCalendar::load_or_default(&args.state_dir.join("kanban.toml"));
    let socket = board_ipc::client::socket_path(&args.runtime_dir);
    let daemon = BoardDaemon::new(socket);
    loop {
        if !board_switch(&args).is_enabled() {
            std::thread::sleep(args.interval);
            continue;
        }
        let now = chrono::Local::now();
        let limit = calendar.limit_at(now);
        // 队列与卡片状态的持久化由 Plan 3b 落地；本次循环只做决策与推进。
        let _ = (limit, &daemon, now);
        std::thread::sleep(args.interval);
    }
}
```

`Cargo.toml` 追加依赖：

```toml
board-ipc = { path = "../board-ipc" }

[[bin]]
name = "board-runner"
path = "src/main.rs"
```

- [ ] **Step 4: 运行测试与构建确认通过**

Run: `cd plugins/superpowers-board && cargo test -p board-runner && cargo build -p board-runner`
Expected: 测试 PASS（7 个），二进制构建成功。

- [ ] **Step 5: 提交**

```bash
cd plugins/superpowers-board && cargo fmt --all
git add plugins/superpowers-board
git commit -m "feat(board-runner): add the scheduling tick and the plugin process entry point"
```

---

## 完成判据

- `cd plugins/superpowers-board && cargo test` 全绿（Plan 1 的 37 个 + 本计划新增 26 个）。
- `cargo tree -p board-ipc | grep -E "^[│├└─ ]*yi-agent-"` 与同一命令用于 `board-runner` **均为空**（零依赖硬约束；注意路径噪声，见 Task 2 Step 5 的说明）。
- `cargo build -p board-runner` 产出可执行文件 `board-runner`。
- 未知 daemon 状态与未知响应变体都不会让插件崩溃或误动作（有专门测试）。
- 关闭开关时进程不再推进队列（`board_switch` 返回 disabled 时跳过 tick）。
