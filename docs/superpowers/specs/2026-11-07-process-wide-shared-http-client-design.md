# 进程级共享 HTTP Client 设计

日期：2026-11-07
状态：待实现
分支：`feat/shared-http-client`

## 问题

每个新会话的第一条消息都要多等约 1 秒。根因不是我们代码里的等待，而是
HTTP 连接握手成本被重复支付。

### 实测证据

本机到 `https://llmapi.horizon.auto` 的冷连接，10 次采样：

| 阶段 | 中位数 | 范围 |
|---|---|---|
| TCP 握手 | 0.26s | 0.25–1.15s |
| TLS 握手（自 TCP 完成后计） | 0.82s | 0.51–5.23s |
| TTFB（含上面两项） | 2.09s | 0.76–6.76s |

同一连接上连发两次请求（`curl --next`）：

```
req1 tcp=0.329s tls=0.905s ttfb=2.280s
req2 tcp=0.000s tls=0.000s ttfb=0.249s
```

握手一旦省掉，延迟立刻从 2.28s 降到 0.25s。TCP + TLS ≈ 1.08s，正是用户
感知到的「1 秒」。代码路径上不存在引入这段延迟的 `sleep`：`agent.rs` 里
唯一的定时等待是重试退避（`think_stall_backoff_base = 2s`）与 idle 超时
（60s），都不在正常路径上。

### 结构性原因

`yi-agent-rs/crates/yi-agent-app-server/src/server/runtime_binding.rs:126`
的 `production_factory` 每构建一个会话就调一次 `bootstrap_agent` →
`build_provider`，而 `OpenaiProvider::new` / `AnthropicProvider::new` 每次
都调 `reqwest::Client::builder()...build()`，得到一个**独立的连接池**。

由此产生两个后果：

1. **每个新会话的首条消息重新付一次握手**（约 1s）。同会话内后续轮次可复用
   连接，所以问题只在「新会话」上显现。
2. `reqwest` 默认 `pool_idle_timeout = 90s`。空闲超过 90 秒后续轮次同样要
   重新握手。

### 附带缺陷：通道缓存的作用域与注释不符

`yi-agent-rs/crates/yi-agent-llm/src/openai/channel.rs:16` 的类型别名上写着
「结果按 base_url 缓存（进程内共享）」，但
`openai/client.rs:151` 实际是每个 provider 各自
`Arc::new(Mutex::new(HashMap::new()))`。

后果：`api_style` 为 `null` 的条目（例如 `HORIZON-OG-Luna`）在**每个新会话**
都要先打一次 `/responses` 拿到 404 才退回 chat，多付一个完整 RTT。实测那次
探测请求的 TTFB 为 3.56s。日志里也印证了这个模式：`session-20261107-120120`
与 `session-20261107-115808` 各有两条 `response received ... 404 Not Found`，
紧跟着才是 `200 OK`。

## 目标

让所有 provider 请求共用进程内的一份 HTTP 连接池，使新会话的首条消息不再付
一次冷握手；同时把通道判定缓存提升到与之一致的进程级作用域。

**非目标**：不优化 gateway 自身的 TTFB（trace 显示同会话连续请求仍需
2–3 秒，那是服务端处理时间，改连接池救不了）。

## 设计

### 1. 新增模块 `yi-agent-llm/src/http.rs`

一个进程级缓存，键为 `(base_url, timeout)`，值为 `reqwest::Client`。

```rust
//! 进程级共享的 HTTP 连接池。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// `api_key` 刻意不在键里：密钥是每个请求的 header，与连接复用无关。
/// `timeout` 在键里：它被烤进 client，而探测（15s）与正式对话（300s）必须
/// 各用各的，绝不能让 15s 的探测缩短正式对话的读超时。
type ClientKey = (String, Duration);

static CLIENTS: OnceLock<Mutex<HashMap<ClientKey, reqwest::Client>>> = OnceLock::new();

fn clients() -> &'static Mutex<HashMap<ClientKey, reqwest::Client>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（必要时构建）一个共享 client。
///
/// 构建失败**不写入缓存**：真实原因几乎只有 TLS provider 配置这类进程级问题，
/// 下次调用重试构建无害；把失败固化成永久哨兵会让一次瞬时问题变成必须重启
/// 才能恢复的故障。
pub fn shared_client(
    base_url: &str,
    timeout: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    let key = (base_url.to_string(), timeout);
    if let Some(client) = clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return Ok(client.clone());
    }

    // 构建在锁外进行：`Client::builder().build()` 会初始化 TLS 后端，不该
    // 让其它线程在这个锁上排队。
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .build()?;

    clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, client.clone());
    Ok(client)
}
```

要点说明：

- **`Client` 的 clone 是廉价的**：`reqwest::Client` 内部是 `Arc`，clone 共享同一个
  连接池。返回 owned 值而不是引用，避免在所有调用点引入生命周期。
- **锁用 `unwrap_or_else(|e| e.into_inner())`**：与本仓既有惯例一致（见
  `yi-agent-core/src/permission.rs:154`、`yi-agent-skills/src/service.rs:119`）。
  缓存表被 poison 不应让整个进程失去 HTTP 能力。
- **构建在锁外**：`build()` 可能触发 TLS 后端初始化，持锁构建会让并发调用串行。
  代价是极端并发下可能重复构建一次并覆盖写入——无害（两个 client 都可用），
  且比串行化所有会话启动便宜。
- **不加任何 `pool_*` 参数**：保持 `reqwest` 默认（`pool_idle_timeout = 90s`）。
  90s 空闲回收是有保护作用的——网关与 NAT 常在 60–120s 掐掉空闲 TCP，池里留着
  已被对端关闭的连接，复用时先撞 broken pipe 再重连，反而更慢。共享之后这条连接
  在正常对话中持续被使用，根本不会空闲到 90s。

### 2. 两个 provider 改为取共享 client

`openai/client.rs` 与 `anthropic/client.rs` 中原本的：

```rust
let client = reqwest::Client::builder()
    .connect_timeout(timeout)
    .read_timeout(timeout)
    .build()
    .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;
```

改为：

```rust
let client = crate::http::shared_client(&base_url, timeout)
    .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;
```

错误类型与文案保持逐字一致，调用方行为不变。

注意 `base_url` 必须在构造 client **之前**完成解析（`unwrap_or_else` 默认值 +
`trim_end_matches('/')`），因为它是缓存键的一部分。现有代码已经是这个顺序，
不需要调整。

**键用归一化后的 `base_url`**：`https://x/v1` 与 `https://x/v1/` 归一化后是同一个
键，避免尾斜杠造成两份额外的池。

### 3. `ChannelCache` 提升到进程级

`openai/channel.rs` 的注释与实现已经不一致，本次让实现追上注释。

把 `ChannelCache` 从「每个 provider 一个新表」改为进程级单表：

```rust
pub type ChannelCache = &'static Mutex<HashMap<String, Channel>>;

static CHANNELS: OnceLock<Mutex<HashMap<String, Channel>>> = OnceLock::new();

pub fn channel_cache() -> ChannelCache {
    CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}
```

`OpenaiProvider` 中 `channel_cache: ChannelCache` 字段的类型随之变化，
构造处由 `Arc::new(Mutex::new(HashMap::new()))` 改为 `channel_cache()`。
`resolve_channel` / `cache_channel` 的调用形态（`.lock()`）不需要改。

**作用域风险与处理**：进程级表意味着「base_url 不支持 `/responses`」这个判定
跨会话共享。这正是想要的——同一 gateway 的能力不会因为换了会话而变化。但测试
里多个 wiremock server 各有独立 `base_url`，天然隔离；同一测试内复用一个
`base_url` 且期望不同通道时需要注意，见「测试」一节。

`effort_reject_cache` **保持每 provider 一份**：它的键里含 `model`，且语义是
「这个模型在这个通道上拒绝过 effort」，与通道探测是两件事，本次不动。

### 4. 不改动的部分

- `probe.rs:70` 与 `probe.rs:154`：探测是「跑一次就扔」的短命对象，共享池收益
  接近零；而且探测用的是整轮 cap 的 timeout，与正式对话语义不同。留待单独议题。
- `yi-agent-tools/src/web/fetch.rs`、`bocha.rs`：抓网页/搜索，与 LLM 无关。
- `bootstrap.rs`、`onboarding.rs` 等调用方：`build_provider` 的签名与语义不变，
  它们不需要任何改动。

## 数据流

```
每次新建会话
  bootstrap_agent → build_provider → OpenaiProvider::new(base_url, timeout)
      → http::shared_client(base_url, timeout)
          ├─ 缓存命中 → clone 一份 Arc（共享同一连接池，零握手）
          └─ 缓存未命中 → 构建 → 插入 → 返回
  → 首个请求直接复用池中已建立的连接
```

首次调用（进程内第一个会话）仍要付一次握手，这是无法避免的；从第二个会话起
全部命中缓存。

## 错误处理

- **构建 client 失败**：不写入缓存，原样返回 `reqwest::Error`，由各 provider 映射成
  `ProviderError::Network(...)`，文案与今天一致。
- **缓存表被 poison**：用 `into_inner()` 继续使用，不 panic、不降级为无缓存。
- **并发首次调用**：可能在锁外各自构建一次，后写入者覆盖先写入者。两个 client 都
  完全可用，且只在进程启动的最初几毫秒可能发生；不引入双检锁的复杂度。

## 测试

`reqwest::Client` 的内部连接池无法从外部直接比较（没有 `PtrEq` 之类的出口），
所以「是否复用」必须靠**可观测的连接数**来断言，而不是比较对象相等。

为此新增一个测试辅助：`tests/common/conn_counter.rs`，用 `std::net::TcpListener`
在后台线程跑一个极简 HTTP/1.1 keep-alive 服务，**记录 `accept()` 被调用了几次**。

选择裸 `TcpListener` 而不是 wiremock 的理由：wiremock 不暴露连接计数，而
「只建立了一条连接」正是本次要验证的核心事实，必须能直接观测。裸 TCP 也避免了
引入新依赖——用 `std` 的线程与监听套接字即可，不需要给 `tokio` 加 `net` feature。

该辅助服务对每个请求返回固定的 `Content-Length` 与 `Connection: keep-alive`，
使客户端确实可以复用连接；同时累计路径计数，供 `ChannelCache` 测试使用。

### 单元测试（`yi-agent-llm` 内）

1. **同键复用连接（核心不变量）**：对同一 `(base_url, timeout)` 调两次
   `shared_client`，各发一个请求，断言服务端 `accept()` 计数 == 1。
   这是整次改动的根本目的，必须可执行验证。
2. **不同 timeout 不复用**：`shared_client(url, 15s)` 与 `shared_client(url, 300s)`
   各发一个请求，断言 `accept()` 计数 == 2（两个键 → 两个池 → 两条连接）。
3. **`base_url` 归一化**：以 `https://host/v1` 与 `https://host/v1/` 调
   `shared_client`（传入前已 `trim_end_matches('/')`），断言命中同一个键——
   即两条请求只产生 1 次 `accept()`。
4. **不同 `base_url` 互不干扰**：两个计数服务，各自的 client 请求只打到自己的
   服务（各自的 `accept()` 计数各自 +1，不串）。

### 集成测试（跨 provider）

5. **两个 provider 共用同一连接池（真正的验收点）**：用同一个 `base_url` 与同一
   个 `timeout` 构造 `OpenaiProvider` A（`api_key = "k1"`）与 B（`api_key = "k2"`），
   各发一个请求到计数服务，断言 `accept()` 计数 == 1。
   这条同时证明了「`api_key` 不在缓存键里是安全的」——不同密钥的请求确实走了
   同一条连接。
6. **`ChannelCache` 跨 provider 共享**：用同一 `base_url` 构造两个 `api_style = Auto`
   的 provider，第一个触发通道探测（`/responses` 返回 404），第二个发请求时
   **不再打 `/responses`**。断言计数服务上 `/responses` 的路径计数 == 1，
   `/chat/completions` == 2。

### 回归

- 现有 `yi-agent-llm` 的 wiremock 测试全部保持通过。特别注意
  `openai_integration.rs` 里依赖通道探测的用例：`ChannelCache` 变成进程级后，
- **端口复用会让测试假通过**（进程级缓存引入的新隐患，必须显式防御）：
  wiremock 的 `MockServer::start()` 拿的是 OS 随机端口，进程退出后该端口可能被
  下一次测试重新分配。若某次测试的 `(base_url, timeout)` 与之前某次**完全相同**
  （host 都是 `127.0.0.1`，端口偶然撞上，timeout 都是 300s），共享 client 会命中
  一条已经死掉的连接池，测试就会以难解释的方式失败或（更糟）假通过。

  **缓解**：不依赖端口随机性，而是让**每个测试用例的 `base_url` 在缓存键上天然
  唯一**。做法是在 `base_url` 的**路径**里追加一个进程内唯一的测试段，例如
  `{server.uri()}/v1/t{n}`，并让 wiremock 的 matcher 用 `path_regex("^/v1/t\d+/")`
  而不是精确路径。这样缓存键不同（不会撞到别的用例的池），路由也仍然匹配。

  不用查询串（`?t=n`）的原因：`base_url` 会被直接拼成
  `format!("{base}{path}")`，带查询串时拼出的是 `...?t=1/chat/completions`，
  路径解析错乱。查询串会污染拼接，不能用来做隔离。

  这条规则只在测试里使用，生产代码不引入任何测试专用分支。

  另外，`ChannelCache` 是多线程共享的 `HashMap`，测试并行执行时同一 `base_url`
  的判定会互相覆盖。上面那条「`base_url` 天然唯一」的规则同时解决了这个问题。

## 实现顺序（TDD）

严格按测试先行，避免「先改代码再补测试」让测试沦为对实现的描述：

1. **先写 `tests/common/conn_counter.rs`** 与第 1 条测试（同键复用 → `accept() == 1`）。
2. **在未改动实现时运行，确认它失败**（此时每个 provider 各建 client，
   必然出现 2 次 `accept()`）。失败是这一步的验收标准。
3. 实现 `http.rs` 并接入两个 provider，让第 1 条通过。
4. 依次补第 2–4 条（timeout 分键、归一化、多 base_url）与第 5–6 条
   （跨 provider 共用池、`ChannelCache` 跨 provider 共享），每条都先写后验。
5. 最后跑 `just test-crate yi-agent-llm` 与 `just ci`。

## 验收判据

1. `just test-crate yi-agent-llm` 全绿。
2. 新增的第 1 条测试在**未改实现时失败**、改后通过——顺序已在上节固定，
   失败输出需留档以便复核。
3. 真实会话验证：连续开两个新会话，第二个会话的首条消息 `sending request` →
   `response received` 延迟相比改动前下降，且 trace 中不出现第二次 TLS 握手
   （可用 `ss`/`lsof` 或 gateway 侧连接数观察）。
4. `just ci` 全绿。

## 风险

- **连接池被单一慢请求占满**：`pool_max_idle_per_host` 默认无上限，共享后同一
  host 的并发请求共用连接上限（reqwest 默认无限流）。理论上多个会话同时打同一
  gateway 会建多条连接，这正是期望行为，不构成风险。
- **进程级状态让测试相互影响**：已在上文「回归」一节给出处理方式。
- **跨 provider 复用连接带来的信任边界**：不同 `api_key` 的请求走同一条 TCP 连接，
  但每个请求都独立携带自己的 header（HTTP/1.1 串行复用、HTTP/2 多路复用皆如此），
  密钥不会因此泄漏。

  已核实：`yi-agent-llm` 的 `Cargo.toml` 关掉了 reqwest 的 default-features，
  显式只启用 `json` / `stream` / `rustls-tls`，`cargo tree -p yi-agent-llm -e features`
  确认 **`http2` 未启用**，因此本 crate 的 provider 走 HTTP/1.1 keep-alive 复用。
  整个 workspace 一起构建时 `h2` 会因其它 crate 的 feature 合并而链接进来，
  但 provider 自身的 client 行为不变。这也意味着下文测试辅助服务用 HTTP/1.1
  裸 `TcpListener` 是匹配的——不会因为客户端想协商 HTTP/2 而让连接计数失真。
