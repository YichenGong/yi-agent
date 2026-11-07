# 进程级共享 HTTP Client 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让所有 LLM provider 请求共用一个进程级 HTTP 连接池，使新会话的首条消息不再重复支付 TCP+TLS 握手（实测约 1.08s）。

**Architecture:** 在 `yi-agent-llm` 新增 `src/http.rs`，用 `OnceLock<Mutex<HashMap<(String, Duration), reqwest::Client>>>` 缓存 client，键为归一化后的 `base_url` 与 `timeout`。`OpenaiProvider::new` 与 `AnthropicProvider::new` 改为从该缓存取 client。同时把 `OpenaiProvider` 的 `ChannelCache` 从每实例一份提升为进程级单表，让 `/responses` 通道探测只发生一次。

**Tech Stack:** Rust 2021、`reqwest` 0.12.28（已锁定）、`std::sync::OnceLock`、`tokio` 测试、`wiremock` 0.6（dev）。

## Global Constraints

- 工作目录：`.worktrees/feat-shared-http-client/yi-agent-rs`（所有命令在此下执行）。
- `timeout` 必须在缓存键里。探测用 15s、正式对话用 300s，绝不能让两者互相污染。
- `api_key` 必须在缓存键**之外**。它是每个请求的 header，与连接复用无关。
- 保留 `reqwest` 默认连接池参数，不添加任何 `pool_*` 调用（默认 `pool_idle_timeout = 90s`、`tcp_keepalive = 15s`、`pool_max_idle_per_host = usize::MAX`）。
- 错误文案必须逐字保持 `failed to build HTTP client: {e}`，映射为 `ProviderError::Network(...)`。
- `base_url` 归一化只在 provider 的 `new()` 里做（已有的 `trim_end_matches('/')`），`shared_client` 不做。
- **范围边界**：本计划只改 `openai/client.rs` 与 `anthropic/client.rs` 两处建
  client 的代码。以下三处**故意不动**，不要顺手改：
  - `crates/yi-agent-llm/src/probe.rs:70`、`:154`：探测是一次性动作，生命周期
    短，纳入共享池反而会把探测的超时语义混进对话池。
  - `crates/yi-agent-tools/src/web/{bocha,fetch}.rs`：属于 web 工具，与 LLM
    provider 的连接池无关，且 `yi-agent-tools` 不依赖 `yi-agent-llm`。
- `cargo fmt --all` 与 `just ci` 必须通过；clippy 以 `-D warnings` 运行。
- 提交信息用 conventional commits，不写 `Co-Authored-By`。

---

### Task 1: 共享 client 缓存模块（含核心复用测试）

**Files:**
- Create: `crates/yi-agent-llm/src/http.rs`
- Modify: `crates/yi-agent-llm/src/lib.rs:6-9`（加 `pub mod http;`）
- Create: `crates/yi-agent-llm/tests/common/mod.rs`
- Create: `crates/yi-agent-llm/tests/shared_client.rs`
- Modify: `crates/yi-agent-llm/Cargo.toml:31-33`（dev-deps 补 `tokio` 的 `net`、`io-util`）

**Interfaces:**
- Produces:
  - `pub fn shared_client(base_url: &str, timeout: std::time::Duration) -> Result<reqwest::Client, reqwest::Error>`
  - `tests/common/mod.rs` 中的 `pub struct ConnCounter`（含 `pub fn uri(&self) -> String`、`pub fn accepts(&self) -> usize`、`pub fn path_hits(&self, path: &str) -> usize`、`pub async fn start() -> Self`）

- [ ] **Step 1: 补 dev-dependencies 的 tokio feature**

编辑 `crates/yi-agent-llm/Cargo.toml`，把 `[dev-dependencies]` 改为：

```toml
[dev-dependencies]
wiremock = "0.6"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net", "io-util"] }
```

- [ ] **Step 2: 先写计数服务辅助（测试基础设施）**

创建 `crates/yi-agent-llm/tests/common/mod.rs`。要点：

- 裸 TCP + 手写 HTTP/1.1，因为 `reqwest::Client` 不暴露连接池，只能靠服务端
  `accept()` 次数断言「是否复用」；wiremock 0.6.5 没有连接计数 API。
- `uri()` 带一个进程内唯一的路径段（`/t0`、`/t1`…）。共享 client 按
  `(base_url, timeout)` 缓存，`bind(0)` 拿到的端口在实例销毁后可能被后来的测试
  复用；端口撞上且 timeout 相同时会命中上一个测试的连接池。唯一路径段让缓存键
  天然不同，而路由匹配前会先剥掉该前缀，所以测试里仍按裸路径声明路由。
- 请求体必须读掉：socket 残留未读入站数据就关闭会触发 RST，截断已写出的响应
  （`integration.rs:592` 记录过这个坑）。

```rust
//! 记录 TCP 连接数与路径命中数的极简 HTTP/1.1 keep-alive 服务。
//!
//! `reqwest::Client` 不暴露内部连接池，无法比较两个 client 是否同源，所以
//! 「是否复用连接」只能靠服务端的 `accept()` 次数来断言。

// 共享测试辅助：每个测试 crate 只用得到其中一部分，
// 未用到的方法不该让 `clippy -D warnings` 失败。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 每个请求返回什么。`Empty` 用于只验证连接复用的测试（不消费流）；
/// `Sse` 用于必须走 `Provider::call_stream` 的测试。
#[derive(Clone)]
pub enum Body {
    Empty,
    Sse(String),
}

#[derive(Clone)]
struct State {
    accepts: usize,
    path_hits: HashMap<String, usize>,
    /// 命中该路径时返回的响应，未配置的路径回 404 空 body
    /// （正是 `looks_like_missing_route` 认的「路由不存在」）。
    routes: HashMap<String, (u16, Body)>,
}

/// 让每个实例的 `uri()` 带一个进程内唯一的路径段（`/t0`、`/t1`…）。
///
/// 共享 client 的缓存键是 `(base_url, timeout)`，而 `TcpListener::bind(0)` 拿到的
/// 端口在实例销毁后可能被后来的测试复用。端口撞上、timeout 又相同时，就会命中
/// 上一个测试留下的连接池。把唯一段放进**路径**里让缓存键天然不同；不用查询串，
/// 因为 `base_url` 会被直接拼成 `format!("{base}{path}")`。
static NEXT_TAG: AtomicUsize = AtomicUsize::new(0);

pub struct ConnCounter {
    uri: String,
    prefix: String,
    state: Arc<Mutex<State>>,
}

impl ConnCounter {
    /// 起服务。`routes` 的键是完整路径（如 `"/chat/completions"`）。
    pub async fn start(routes: Vec<(&str, u16, Body)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("/t{tag}");

        let state = Arc::new(Mutex::new(State {
            accepts: 0,
            path_hits: HashMap::new(),
            routes: routes
                .into_iter()
                .map(|(p, status, body)| (p.to_string(), (status, body)))
                .collect(),
        }));

        let accept_state = Arc::clone(&state);
        let match_prefix = prefix.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                accept_state.lock().unwrap().accepts += 1;

                let conn_state = Arc::clone(&accept_state);
                let match_prefix = match_prefix.clone();
                tokio::spawn(async move {
                    // 同一连接上按 keep-alive 语义连续处理多个请求。
                    // `carry` 保存上一轮已多读到的字节（下一个请求的开头），
                    // 否则 keep-alive 的第二个请求会被丢弃，表现为客户端超时。
                    let mut carry: Vec<u8> = Vec::new();
                    loop {
                        let Some(head) = read_request(&mut socket, &mut carry).await else {
                            return;
                        };

                        // 请求体必须读掉：socket 若残留未读入站数据就关闭，内核发 RST，
                        // 会把已写出的响应截断（integration.rs:592 记录过这个坑）。
                        // 去掉唯一前缀再匹配，测试里仍按裸路径（`/v1/chat/completions`）
                        // 声明路由，不必关心前缀。
                        let path = head
                            .path
                            .strip_prefix(match_prefix.as_str())
                            .unwrap_or(&head.path)
                            .to_string();
                        let (status, resp_body) = {
                            let mut guard = conn_state.lock().unwrap();
                            *guard.path_hits.entry(path.clone()).or_insert(0) += 1;
                            match guard.routes.get(&path).cloned() {
                                Some(route) => route,
                                // 未配置 = 路由不存在：404 + 空 body。
                                None => (404, Body::Empty),
                            }
                        };

                        let payload = match resp_body {
                            Body::Empty => Vec::new(),
                            Body::Sse(text) => text.into_bytes(),
                        };

                        let reason = match status {
                            200 => "OK",
                            404 => "Not Found",
                            500 => "Internal Server Error",
                            _ => "Unknown",
                        };
                        let head_text = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             content-length: {}\r\n\
                             content-type: text/event-stream\r\n\
                             connection: keep-alive\r\n\r\n",
                            payload.len()
                        );
                        if socket.write_all(head_text.as_bytes()).await.is_err() {
                            return;
                        }
                        if socket.write_all(&payload).await.is_err() {
                            return;
                        }
                        if socket.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        Self {
            uri: format!("http://{addr}{prefix}"),
            prefix,
            state,
        }
    }

    pub fn uri(&self) -> String {
        self.uri.clone()
    }

    pub fn accepts(&self) -> usize {
        self.state.lock().unwrap().accepts
    }

    pub fn path_hits(&self, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .path_hits
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

struct RequestHead {
    path: String,
}

/// 读一个完整请求（头 + 按 `content-length` 的 body），返回它的路径。
///
/// `carry` 跨请求保留已多读的字节：`read` 可能一次读到两个请求的边界，
/// 丢掉它们会让 keep-alive 的第二个请求凭空消失。
async fn read_request(
    socket: &mut tokio::net::TcpStream,
    carry: &mut Vec<u8>,
) -> Option<RequestHead> {
    let mut buf = std::mem::take(carry);
    let mut chunk = [0u8; 1024];

    loop {
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            continue;
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next()?;
        let mut parts = request_line.split_whitespace();
        let _method = parts.next()?;
        let path = parts.next()?.to_string();

        let body_len = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .next()
            .unwrap_or(0);

        let body_start = head_end + 4;
        // 补齐本请求剩余的 body 字节（可能已全部读到，也可能还没读全）。
        while buf.len() < body_start + body_len {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }

        // 把超出本请求的字节留给下一轮（keep-alive 的下一个请求）。
        *carry = buf.split_off(body_start + body_len);
        return Some(RequestHead { path });
    }
}
```

- [ ] **Step 3: 先写第 1 条测试（核心不变量）**

创建 `crates/yi-agent-llm/tests/shared_client.rs`：

```rust
mod common;

use std::time::Duration;

use common::{Body, ConnCounter};

/// 核心不变量：同一个 (base_url, timeout) 必须共用一条连接。
#[tokio::test]
async fn same_key_reuses_one_connection() {
    let server = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;
    let url = server.uri();

    let a = yi_agent_llm::http::shared_client(&url, Duration::from_secs(5)).unwrap();
    let b = yi_agent_llm::http::shared_client(&url, Duration::from_secs(5)).unwrap();

    for client in [a, b] {
        let _ = client
            .post(format!("{url}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(
        server.accepts(),
        1,
        "two clients with the same key must share one TCP connection"
    );
}
```

- [ ] **Step 4: 运行测试，确认它失败**

Run: `cargo test -p yi-agent-llm --test shared_client`
Expected: 编译失败，报 `could not find 'http' in 'yi_agent_llm'`（模块尚不存在）。这是本步骤的验收标准。

- [ ] **Step 5: 实现 `http.rs`**

创建 `crates/yi-agent-llm/src/http.rs`：

```rust
//! 进程级共享的 HTTP 连接池。
//!
//! 每个 provider 都各自 `reqwest::Client::builder().build()` 会得到一个独立连接池，
//! 于是**每个新会话的首条消息都要重付一次 TCP+TLS 握手**（本机实测 TCP 0.26s +
//! TLS 0.82s）。这里把 client 按 `(base_url, timeout)` 缓存到进程级，让所有会话
//! 复用同一份连接。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// `api_key` 刻意不在键里：密钥是每个请求的 header，与连接复用无关。
/// `timeout` 在键里：它被烤进 client，且探测（15s）与正式对话（300s）必须各用
/// 各的，绝不能让 15s 的探测缩短正式对话的读超时。
type ClientKey = (String, Duration);

static CLIENTS: OnceLock<Mutex<HashMap<ClientKey, reqwest::Client>>> = OnceLock::new();

fn clients() -> &'static Mutex<HashMap<ClientKey, reqwest::Client>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lookup(key: &ClientKey) -> Option<reqwest::Client> {
    clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

/// 取（必要时构建）一个共享 client。
///
/// 构建失败**不写入缓存**：真实原因几乎只有 TLS provider 配置这类进程级问题，
/// 下次调用重试构建无害；把失败固化成永久哨兵会让一次瞬时问题变成必须重启才能
/// 恢复的故障。
pub fn shared_client(
    base_url: &str,
    timeout: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    let key = (base_url.to_string(), timeout);

    if let Some(client) = lookup(&key) {
        return Ok(client);
    }

    // 构建放在锁外：`build()` 会初始化 TLS 后端，不该让其它线程排队等它。
    // `build()` 是同步的，此处没有 await 点，不存在「持 std 锁跨 await」的问题。
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .build()?;

    // 二次检查（双检锁）：并发首次调用可能都构建出了 client。若先到者已写入，
    // 必须丢弃自己刚构建的那个并返回先到者——否则两个调用方各自持有独立连接池，
    // 永久失去复用，也永远无法满足「只建立一条连接」这个核心不变量。
    let mut guard = clients().lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.entry(key).or_insert(client);
    Ok(entry.clone())
}

#[cfg(test)]
mod tests {
    use super::shared_client;
    use std::time::Duration;

    #[test]
    fn building_a_client_does_not_panic_for_a_normal_url() {
        // 只验证构建路径本身可用；连接复用由集成测试断言。
        assert!(shared_client("http://127.0.0.1:1", Duration::from_secs(1)).is_ok());
    }
}
```

- [ ] **Step 6: 在 `lib.rs` 声明模块**

编辑 `crates/yi-agent-llm/src/lib.rs`，在 `pub mod probe;` 之后加一行：

```rust
pub mod http;
pub mod probe;
```

- [ ] **Step 7: 运行测试，确认通过**

Run: `cargo test -p yi-agent-llm --test shared_client`
Expected: PASS（1 passed）

- [ ] **Step 8: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat-shared-http-client/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-llm/src/http.rs crates/yi-agent-llm/src/lib.rs crates/yi-agent-llm/Cargo.toml crates/yi-agent-llm/tests/common/mod.rs crates/yi-agent-llm/tests/shared_client.rs
git commit -m "feat(llm): share one HTTP connection pool across providers

Every provider built its own reqwest::Client, so every new session
re-paid a full TCP+TLS handshake (measured 0.26s + 0.82s locally).
Cache clients per (base_url, timeout) at process scope.

api_key stays out of the key: it is a per-request header and does not
affect connection reuse."
```

---

### Task 2: 缓存键语义（timeout 分键、不做归一化、多 base_url 隔离）

**Files:**
- Modify: `crates/yi-agent-llm/tests/shared_client.rs`

**Interfaces:**
- Consumes: `shared_client` from Task 1; `ConnCounter` from Task 1.

- [ ] **Step 1: 写三个测试**

在 `crates/yi-agent-llm/tests/shared_client.rs` 末尾追加：

```rust
/// 不同 timeout 必须分池：探索/探测用短线，正式对话用长线，不能互相污染。
#[tokio::test]
async fn different_timeouts_use_separate_pools() {
    let server = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;
    let url = server.uri();

    let short = yi_agent_llm::http::shared_client(&url, Duration::from_secs(15)).unwrap();
    let long = yi_agent_llm::http::shared_client(&url, Duration::from_secs(300)).unwrap();

    for client in [short, long] {
        let _ = client
            .post(format!("{url}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(
        server.accepts(),
        2,
        "different timeouts must not share a pool"
    );
}

/// 契约测试：归一化由 provider 的 `new()` 负责，`shared_client` 不做。
/// 这条锁住设计决定——将来有人在 shared_client 里"顺手"加 trim 会失败。
#[tokio::test]
async fn shared_client_does_not_normalise_trailing_slash() {
    let server = ConnCounter::start(vec![("/v1/chat/completions", 200, Body::Empty)]).await;
    let bare = format!("{}/v1", server.uri());
    let slashed = format!("{}/v1/", server.uri());

    let a = yi_agent_llm::http::shared_client(&bare, Duration::from_secs(5)).unwrap();
    let b = yi_agent_llm::http::shared_client(&slashed, Duration::from_secs(5)).unwrap();

    // 两个键不同 → 两个池 → 两条连接。provider 侧会先 trim 再传，所以生产路径上
    // 不会出现这种差；这里只锁住 shared_client 的职责边界。
    for client in [a, b] {
        let _ = client
            .post(format!("{bare}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await;
    }

    assert_eq!(
        server.accepts(),
        2,
        "shared_client must not normalise; that is the provider's job"
    );
}

/// 不同 base_url 必须落在不同的池，互不干扰。
#[tokio::test]
async fn different_base_urls_are_isolated() {
    let one = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;
    let two = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;

    for server in [&one, &two] {
        let url = server.uri();
        let client = yi_agent_llm::http::shared_client(&url, Duration::from_secs(5)).unwrap();
        let _ = client
            .post(format!("{url}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(one.accepts(), 1);
    assert_eq!(two.accepts(), 1);
    assert_eq!(one.path_hits("/chat/completions"), 1);
    assert_eq!(two.path_hits("/chat/completions"), 1);
}
```

- [ ] **Step 2: 运行测试**

Run: `cargo test -p yi-agent-llm --test shared_client`
Expected: 4 passed（第 1 条 + 新加的 3 条）。实现已正确，无需改 `http.rs`。

若 `different_timeouts_use_separate_pools` 失败且 `accepts()` 报 1，说明缓存键漏了
`timeout`——检查 `ClientKey` 是否确实是 `(String, Duration)`。

- [ ] **Step 3: 提交**

```bash
cargo fmt --all
git add crates/yi-agent-llm/tests/shared_client.rs
git commit -m "test(llm): lock down shared client key semantics

Different timeouts must not share a pool, different base_urls stay
isolated, and shared_client deliberately does not normalise trailing
slashes (that belongs to the provider constructor)."
```

---

### Task 3: 两个 provider 接入共享 client

**Files:**
- Modify: `crates/yi-agent-llm/src/openai/client.rs:136-142`
- Modify: `crates/yi-agent-llm/src/anthropic/client.rs:91-97`

**Interfaces:**
- Consumes: `crate::http::shared_client` from Task 1.
- Produces: `OpenaiProvider` / `AnthropicProvider` 行为不变，但内部 client 来自共享池。

- [ ] **Step 1: 先写跨 provider 复用测试（会失败）**

在 `crates/yi-agent-llm/tests/shared_client.rs` 追加：

```rust
use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

/// 真正的验收点：两个 provider（不同 api_key）共用同一条 TCP 连接。
#[tokio::test]
async fn two_providers_share_one_connection() {
    let server = ConnCounter::start(vec![("/v1/chat/completions", 200, Body::Empty)]).await;
    let base = format!("{}/v1", server.uri());

    for key in ["key-one", "key-two"] {
        let provider = OpenaiProvider::new(OpenaiProviderOpts {
            base_url: Some(base.clone()),
            api_key: Some(key.to_string()),
            timeout: Some(Duration::from_secs(5)),
            api_style: Some("chat".into()),
            ..Default::default()
        })
        .expect("provider construction");

        // 只发请求不消费流：body 是否合法 SSE 与连接复用无关。
        let _ = provider
            .client()
            .post(format!("{base}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(
        server.accepts(),
        1,
        "two providers with the same base_url and timeout must share one connection"
    );
}
```

- [ ] **Step 2: 运行测试，确认它失败**

Run: `cargo test -p yi-agent-llm --test shared_client two_providers_share_one_connection`
Expected: 编译失败，报 `no method named 'client' found for struct 'OpenaiProvider'`。
这是预期的失败——下面两步才会提供该出口并接入共享池。

- [ ] **Step 3: 给 `OpenaiProvider` 加只读 client 出口**

编辑 `crates/yi-agent-llm/src/openai/client.rs`，在 `impl OpenaiProvider` 内
（`new` 之后、`resolve_channel` 之前）插入：

```rust
    /// 内部 client 的只读访问，供测试断言连接复用。
    ///
    /// 生产代码一律走 `call_stream`，不直接用它——暴露引用而不是 clone，
    /// 避免调用方误以为拿到的是独立池。
    #[doc(hidden)]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
```

- [ ] **Step 4: 运行测试，确认失败原因变成连接数不符**

Run: `cargo test -p yi-agent-llm --test shared_client two_providers_share_one_connection`
Expected: FAIL，`assertion failed: 1 == 2`（两个 provider 各建了一个 client，服务端看到 2 次 accept）。

- [ ] **Step 5: 接入共享池（openai）**

编辑 `crates/yi-agent-llm/src/openai/client.rs`，把：

```rust
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;
```

替换为：

```rust
        // 归一化已在上面完成（`trim_end_matches('/')`），这里直接用。
        // 共享池让新会话不再重付 TCP+TLS 握手。
        let client = crate::http::shared_client(&base_url, timeout)
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;
```

- [ ] **Step 6: 接入共享池（anthropic）**

编辑 `crates/yi-agent-llm/src/anthropic/client.rs`，把：

```rust
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;
```

替换为：

```rust
        // 同 openai：归一化已完成，共享池避免新会话重付握手。
        let client = crate::http::shared_client(&base_url, timeout)
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;
```

- [ ] **Step 7: 运行测试，确认通过**

Run: `cargo test -p yi-agent-llm --test shared_client`
Expected: 5 passed

- [ ] **Step 8: 跑整个 crate 的测试，确认没有回归**

Run: `cargo test -p yi-agent-llm`
Expected: 全部通过。若 `integration.rs` 的两条 400ms 超时测试失败，说明缓存键漏了
`timeout`（它们与 5s/30s 的用例必须分池）。

- [ ] **Step 9: 提交**

```bash
cargo fmt --all
git add crates/yi-agent-llm/src/openai/client.rs crates/yi-agent-llm/src/anthropic/client.rs crates/yi-agent-llm/tests/shared_client.rs
git commit -m "feat(llm): take provider HTTP clients from the shared pool

Both providers now resolve their client through http::shared_client,
so a new session reuses the connection already established by an
earlier one instead of paying a fresh TCP+TLS handshake."
```

---

### Task 4: `ChannelCache` 提升到进程级

**Files:**
- Modify: `crates/yi-agent-llm/src/openai/channel.rs:16`
- Modify: `crates/yi-agent-llm/src/openai/client.rs:1-5`（导入）、`:85`（字段）、`:151`（构造）
- Modify: `crates/yi-agent-llm/tests/shared_client.rs`

**Interfaces:**
- Produces: `pub type ChannelCache = &'static Mutex<HashMap<String, Channel>>;`
  `pub fn channel_cache() -> ChannelCache`

- [ ] **Step 1: 先写跨 provider 共享通道判定的测试**

在 `crates/yi-agent-llm/tests/shared_client.rs` 追加：

```rust
/// 通道判定必须跨 provider 共享：同一 gateway 的能力不会因为换了会话而变。
/// 否则每个新会话都要先吃一次 `/responses` 404 才回退 chat，多付一个 RTT。
#[tokio::test]
async fn channel_detection_is_shared_across_providers() {
    let chat_sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let server = ConnCounter::start(vec![
        // `/responses` 故意不配置 → 服务端回 404 空 body，正是
        // `looks_like_missing_route` 认的「路由不存在」。
        ("/v1/chat/completions", 200, Body::Sse(chat_sse.to_string())),
    ])
    .await;
    let base = format!("{}/v1", server.uri());

    use futures::StreamExt;
    use yi_agent_core::{GenParams, Message, Provider, ProviderRequest};

    let request = || ProviderRequest {
        model: "test-model".into(),
        system: None,
        messages: vec![Message::user("ping")],
        tools: Vec::new(),
        params: GenParams {
            max_tokens: Some(8),
            ..Default::default()
        },
    };

    for _ in 0..2 {
        let provider = OpenaiProvider::new(OpenaiProviderOpts {
            base_url: Some(base.clone()),
            api_key: Some("k".into()),
            timeout: Some(Duration::from_secs(5)),
            api_style: Some("auto".into()),
            ..Default::default()
        })
        .expect("provider construction");

        let mut stream = provider.call_stream(request()).await.expect("stream ok");
        while stream.next().await.is_some() {}
    }

    assert_eq!(
        server.path_hits("/v1/responses"),
        1,
        "the /responses probe must run once per process, not once per provider"
    );
    assert_eq!(server.path_hits("/v1/chat/completions"), 2);
    assert_eq!(
        server.accepts(),
        1,
        "same base_url and timeout must still share one connection"
    );
}
```

`futures` 已是 `[dependencies]` 下的依赖，测试可直接用。

- [ ] **Step 2: 运行测试，确认它失败**

Run: `cargo test -p yi-agent-llm --test shared_client channel_detection_is_shared_across_providers`
Expected: FAIL，`path_hits("/v1/responses")` 为 2（每个 provider 各探测一次）。

- [ ] **Step 3: 把 `ChannelCache` 改成进程级**

编辑 `crates/yi-agent-llm/src/openai/channel.rs`，把：

```rust
pub type ChannelCache = Arc<Mutex<HashMap<String, Channel>>>;
```

替换为：

```rust
/// 网关能力判定的进程级缓存（`base_url` → 实际通道）。
///
/// 用 `&'static` 而不是 `Arc`：它是进程内唯一的一份，共享引用即可，
/// 不需要在每处 clone 一个 `Arc` 来表达"同一张表"。
pub type ChannelCache = &'static Mutex<HashMap<String, Channel>>;

static CHANNELS: OnceLock<Mutex<HashMap<String, Channel>>> = OnceLock::new();

/// 取进程级的通道判定表。
pub fn channel_cache() -> ChannelCache {
    CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}
```

同时把文件顶部的 `use std::sync::{Arc, Mutex};` 改为：

```rust
use std::sync::{Mutex, OnceLock};
```

- [ ] **Step 4: 更新 `client.rs` 的导入与构造**

`crates/yi-agent-llm/src/openai/client.rs` 共两处（实测通过的最小改动）：

1. 第 3 行 `use std::collections::{HashMap, HashSet};` →
   ```rust
   use std::collections::HashSet;
   ```
   （`HashMap` 此后不再被本文件使用；留着会触发 clippy 的 unused import。）

   注意第 4 行的 `use std::sync::{Arc, Mutex};` **必须保留**：
   `effort_reject_cache: Arc<Mutex<HashSet<String>>>` 仍在用它。

2. 第 150 行构造：
   ```rust
   channel_cache: Arc::new(Mutex::new(HashMap::new())),
   ```
   改为内联全路径（不必再加 import，也避免字段名与函数名同名带来的可读性问题）：
   ```rust
   channel_cache: crate::openai::channel::channel_cache(),
   ```

   第 15 行的 `use crate::openai::channel::{Channel, ChannelCache, looks_like_missing_route};`
   **不需要改**。

- [ ] **Step 5: 编译并运行测试**

Run: `cargo test -p yi-agent-llm --test shared_client`
Expected: 6 passed

若报 `cannot find type 'Arc'`，说明 `client.rs` 里 `Arc` 仍被
`effort_reject_cache` 使用而导入被误删——`effort_reject_cache:
Arc<Mutex<HashSet<String>>>` 需要 `Arc` 与 `Mutex`，第 4 行的
`use std::sync::{Arc, Mutex};` 必须保留。

- [ ] **Step 6: 跑整个 crate 并检查 clippy**

```bash
cargo test -p yi-agent-llm
cargo clippy -p yi-agent-llm --all-targets -- -D warnings
```
Expected: 测试全通过；clippy 无告警。

- [ ] **Step 7: 提交**

```bash
cargo fmt --all
git add crates/yi-agent-llm/src/openai/channel.rs crates/yi-agent-llm/src/openai/client.rs crates/yi-agent-llm/tests/shared_client.rs
git commit -m "feat(llm): make the channel cache process-wide

The field already claimed to be process-shared while every provider
built its own table, so each new session re-probed /responses and ate
a wasted 404 round trip before falling back to chat."
```

---

### Task 5: 全量验证与收尾

**Files:**
- 无新增；只跑验证与记录。

- [ ] **Step 1: 跑受影响测试**

Run: `cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat-shared-http-client/yi-agent-rs && just test-crate yi-agent-llm`
Expected: 全绿。

- [ ] **Step 2: 检查有没有其它 crate 依赖被改动的行为**

Run: `just test-affected`
Expected: 全绿。若 `yi-agent-runtime` 或 `yi-agent-app-server` 报错，检查它们是否
在测试里构造了预期「每次 new 都独立 client」的行为（应当没有）。

- [ ] **Step 3: 跑完整 CI**

Run: `just ci`
Expected: 全绿（16 道门禁）。重点看 `fmt-check`、`lint`、`msrv-check`。

- [ ] **Step 4: 更新项目进度文档**

若 `docs/project-management/` 下有与 LLM/性能相关的条目，加一条已完成项并附
判据（`crates/yi-agent-llm/src/http.rs:N` 与 `tests/shared_client.rs`）。
纯新增模块且无既有条目时，跳过本步并在提交信息里说明。

- [ ] **Step 5: 提交收尾**

```bash
git add -A
git commit -m "docs: record shared HTTP client verification"
```

---

## 验证清单（合并前自查）

- [ ] `cargo test -p yi-agent-llm` 全绿
- [ ] `cargo clippy -p yi-agent-llm --all-targets -- -D warnings` 干净
- [ ] `just ci` 全绿
- [ ] `git log` 上 5 个提交，每个都有独立可验证的交付物
- [ ] `crates/yi-agent-llm/src/http.rs` 里没有 `pool_*` 调用
- [ ] `ClientKey` 是 `(String, Duration)`，不含 `api_key`
- [ ] 错误文案仍为 `failed to build HTTP client: {e}`
