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

`yi-agent-rs/crates/yi-agent-app-server/src/server/runtime_binding.rs:144`
的 `production_factory` 每构建一个会话就调一次 `bootstrap_agent` →
`build_provider`，而 `OpenaiProvider::new` / `AnthropicProvider::new` 每次
都调 `reqwest::Client::builder()...build()`，得到一个**独立的连接池**。

由此产生两个后果：

1. **每个新会话的首条消息重新付一次握手**（约 1s）。同会话内后续轮次可复用
   连接，所以问题只在「新会话」上显现。
2. `reqwest` 默认 `pool_idle_timeout = 90s`。空闲超过 90 秒后续轮次同样要
   重新握手。

### 附带缺陷：通道缓存的作用域与注释不符

`yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:84` 的字段注释写着
「网关能力判定缓存(`base_url` → 实际通道),**进程内共享**」，但
`client.rs:151` 实际是每个 provider 各自
`Arc::new(Mutex::new(HashMap::new()))`——注释描述的作用域与实现不符。

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

    if let Some(client) = lookup(&key) {
        return Ok(client);
    }

    // 构建放在锁外：`build()` 会初始化 TLS 后端，不该让其它线程排队等它。
    // `build()` 是同步的，此处没有 await 点，故不存在「持 std 锁跨 await」
    // 的问题——真正的风险是并发重复构建，由下面的二次检查处理。
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .build()?;

    // 二次检查（双检锁）：并发首次调用时可能都构建出了 client。若先到者已经
    // 写入，必须**丢弃自己刚构建的那个**并返回先到者——否则两个调用各自持有
    // 独立连接池，永久失去复用，也永远无法满足「只建立一条连接」这个核心
    // 不变量。丢弃一个 `Client` 只释放一个空池，代价远小于失去复用。
    let mut guard = clients().lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.entry(key).or_insert(client);
    Ok(entry.clone())
}

fn lookup(key: &ClientKey) -> Option<reqwest::Client> {
    clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}
```

要点说明：

- **`Client` 的 clone 是廉价的**：已核实 `reqwest` 0.12.28 的
  `#[derive(Clone)] pub struct Client { inner: Arc<ClientRef> }`，连接池在
  `ClientRef.hyper` 内，clone 共享同一份池。返回 owned 值而不是引用，避免在所有
  调用点引入生命周期。
- **双检锁是为了保住不变量，不是为了省一次构建**：单层「锁内查 + 锁外构建 +
  锁内插入」在并发首次调用时会互相覆盖，后写入者替换先写入者，两个调用方各自
  拿到不同池。`entry().or_insert()` 保证任何时刻同一个键只对应一个池。
- **锁用 `unwrap_or_else(|e| e.into_inner())`**：与本仓既有惯例一致（见
  `yi-agent-core/src/permission.rs:154`、`yi-agent-skills/src/service.rs:119`）。
  缓存表被 poison 不应让整个进程失去 HTTP 能力。
- **不加任何 `pool_*` 参数**：保持 `reqwest` 默认（`pool_idle_timeout = 90s`）。
  90s 空闲回收是有保护作用的——网关与 NAT 常在 60–120s 掐掉空闲 TCP，池里留着
  已被对端关闭的连接，复用时先撞 broken pipe 再重连，反而更慢。共享之后这条连接
  在正常对话中持续被使用，根本不会空闲到 90s。

  已核实 `reqwest` 0.12.28 的默认值还包括 `tcp_keepalive = 15s` 与
  `pool_max_idle_per_host = usize::MAX`（`async_impl/client.rs:298-303`）。
  15s 的 TCP keepalive 探测能在连接真被中间设备掐断时尽早发现，这进一步说明
  无需手动调参。

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
`trim_end_matches('/')`），因为它是缓存键的一部分。现有代码已经是这个顺序
（`openai/client.rs:120`、`anthropic/client.rs:74`），不需要调整。

**归一化只在 provider 构造处做一次，`shared_client` 不做。**
`https://x/v1` 与 `https://x/v1/` 因为已被 `trim_end_matches('/')` 处理，到达
`shared_client` 时必然相同，自然命中同一个键。不在 `shared_client` 里再 trim 一次
的理由：两处归一化必须永远保持一致，而它们一旦分叉就会产生「看起来同键、实际
两份额外池」的静默问题。单一职责点更安全。测试 3 用契约测试锁住这个决定。

### 3. `ChannelCache` 提升到进程级

`openai/client.rs:84` 的字段注释宣称「进程内共享」，而实现是每 provider 一份。
本次让实现追上这条注释。

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

**编译期连带影响（实现时必须处理，否则 `just ci` 的 clippy `-D warnings` 会红）**：
改动后 `client.rs:3` 的 `HashMap` 不再被使用（它原本只服务于
`channel_cache` 的构造），成为未使用导入；`Arc` 仍被 `effort_reject_cache`
使用，不受影响。导入需收紧为 `use std::collections::HashSet;`。
`OpenaiProvider` 的 `Debug` 实现是手写的且未列出 `channel_cache` 字段
（`client.rs:90-99`），因此字段换类型不会影响它。

**作用域风险与处理**：进程级表意味着「base_url 不支持 `/responses`」这个判定
跨会话共享。这正是想要的——同一 gateway 的能力不会因为换了会话而变化。但测试
里多个 wiremock server 各有独立 `base_url`，天然隔离；同一测试内复用一个
`base_url` 且期望不同通道时需要注意，见「测试」一节。

`effort_reject_cache` **保持每 provider 一份**，本次不动。理由要写清楚，否则
它看起来像是漏改：它的键是 `(base_url, model, channel)`，语义是「这个模型在这个
通道上拒绝过 effort」。跨 provider 共享它是**可以**省一次失败重投的（新会话不该
重试一个已知被拒的档位），但那属于「effort 能力判定」的独立议题——本次只解决
连接握手与通道探测，不把两件事绑在一起，免得将来调 effort 语义时被连接池的
改动边界拖住。已在「不改动的部分」与后续议题中记录。

### 4. 不改动的部分

- `probe.rs:71`（`probe_effort`）与 `probe.rs:154`（`detect_openai_channel`）：
  探测是「跑一次就扔」的短命对象，共享池收益
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
- **并发首次调用**：双检锁保证同一个键只留下一个池。并发时可能都构建一次，但
  `entry().or_insert()` 让先写入者胜出，后到者丢弃自己的 `Client` 并返回先到者，
  因此**所有调用方最终共享同一个池**。丢弃一个空池的代价是释放一次内存分配，
  远小于失去连接复用。

## 测试

`reqwest::Client` 的内部连接池无法从外部直接比较（没有 `PtrEq` 之类的出口），
所以「是否复用」必须靠**可观测的连接数**来断言，而不是比较对象相等。

为此新增一个测试辅助：`tests/common/conn_counter.rs`，在后台跑一个极简 HTTP/1.1
keep-alive 服务，**记录 `accept()` 被调用了几次**。

选择裸 TCP 而不是 wiremock 的理由：wiremock 不暴露连接计数（已核实 0.6.5 没有
相关 API），而「只建立了一条连接」正是本次要验证的核心事实，必须能直接观测。

**实现用 `tokio::net::TcpListener`，不用 `std::net::TcpListener` + 线程**：
本仓 `integration.rs:638` 与 `:706` 已有同样的先例（tokio TCP + 手写 HTTP 响应），
配套的 `drain_request`（`integration.rs:596`）负责读完请求头与 body。
tokio 版本便于在 `#[tokio::test]` 内运行，不必引入线程与跨线程计数。
`tokio` 的 `net`、`io-util` feature 需补进 dev-dependencies
（现有只有 `macros` / `rt-multi-thread`）。

**必须复用 `drain_request` 的做法**：`integration.rs:592-595` 的注释记录了一个
真实踩坑——若在关闭 socket 时仍有未读的入站数据，内核会发 RST，**截断已写出的
响应体**，`reqwest` 会报解码错误而所有事件其实都已送达。计数服务若不读请求体
（provider 每次 POST 都带 JSON body），同样会遇到这个 RST 截断，表现为难以
归因的 `StreamError`。所以每个请求都必须「读头 + 按 `content-length` 读完 body」
再回响应。

响应格式必须**严格合法**，否则客户端不会复用连接：
`HTTP/1.1 200 OK` + `content-length: N` + `\[empty line\]` + 恰好 N 字节 body。
**不用 chunked**：裸 socket 手写 chunked 极易出错，而 `Content-Length` 已足以让
一条 HTTP/1.1 连接保持可复用。响应体按 SSE 构造（`content-type: text/event-stream`，
body 为 `data: {...}\n\ndata: [DONE]\n\n`），因为 `Provider::call()` 会
`accumulate_stream`，要求流中出现 `finish_reason` 或 `[DONE]` 才正常结束
（见 `openai/stream.rs:145`）；body 不合法会让测试以 `StreamError` 失败，
掩盖真正要验证的连接复用。

服务端需按顺序处理同一连接上的**多个**请求（keep-alive 语义），并把每个请求的
`路径 → 次数` 记进共享计数表。

### 单元测试（`yi-agent-llm` 内）

1. **同键复用连接（核心不变量）**：对同一 `(base_url, timeout)` 调两次
   `shared_client`，两个 client 各发一个请求，断言服务端 `accept()` 计数 == 1。
   无法直接断言「两个 `Client` 指向同一池」——`reqwest::Client` 没有暴露内部的
   `Arc<ClientRef>`，确实只能靠连接层的行为断言。这是整次改动的根本目的，
   必须是可执行的验证。
2. **不同 timeout 不复用**：`shared_client(url, 15s)` 与 `shared_client(url, 300s)`
   各发一个请求，断言 `accept()` 计数 == 2（两个键 → 两个池 → 两条连接）。
3. **归一化由调用方负责，`shared_client` 不做归一化**：这是一条**契约测试**，
   不是行为测试。断言 `shared_client("http://h/v1", t)` 与
   `shared_client("http://h/v1/", t)` 得到的是**两个不同键**（因此两条连接）。
   这样写的原因：归一化发生在 provider 的 `new()` 里（`trim_end_matches('/')`），
   到不了 `shared_client`。若反过来让 `shared_client` 也 trim，就会有两处归一化
   逻辑必须永远保持一致——把职责留在唯一的入口（provider 构造）更好。
   本测试的作用是**锁住这个设计决定**：将来有人想在 `shared_client` 里"顺手"
   加归一化，这条测试会失败并提示先统一职责。
4. **不同 `base_url` 互不干扰**：两个计数服务，各自的 client 请求只打到自己的
   服务（各自的 `accept()` 计数各自 +1，不串）。

### 集成测试（跨 provider）

5. **两个 provider 共用同一连接池（真正的验收点）**：用同一个 `base_url` 与同一
   个 `timeout` 构造 `OpenaiProvider` A（`api_key = "k1"`）与 B（`api_key = "k2"`），
   各发一个请求到计数服务，断言 `accept()` 计数 == 1。
   这条同时证明了「`api_key` 不在缓存键里是安全的」——不同密钥的请求确实走了
   同一条连接。
6. **`ChannelCache` 跨 provider 共享**：用同一 `base_url` 构造两个 `api_style = Auto`
   的 provider，第一个触发通道探测，第二个发请求时**不再打 `/responses`**。
   断言计数服务上 `/responses` 的路径计数 == 1，`/chat/completions` == 2。

   探测能被识别为「路由缺失」的前提是 404 **且 body 为空**
   （`channel.rs:20` 的 `looks_like_missing_route`）；若 404 带 JSON body，
   会被判成真实错误而不是回退信号。计数服务对 `/responses` 必须返回
   `404` + 空 body + `content-length: 0`。

### 回归

现有 `yi-agent-llm` 的 wiremock 测试必须全部保持通过。进程级缓存引入两个新的
测试间耦合，都要显式处理：

**（a）端口复用会让测试假通过。** wiremock 的 `MockServer::start()` 拿的是 OS
随机端口；进程退出后该端口可能被下一次测试重新分配。若某次测试的
`(base_url, timeout)` 与之前某次完全相同（host 都是 `127.0.0.1`、端口偶然撞上、
timeout 都是 300s），共享 client 会命中一条已经死掉的连接池，测试就会以难解释的
方式失败，或更糟——假通过。

缓解不依赖端口随机性，而是让每个用例的 `base_url` 在缓存键上天然唯一：在
`base_url` 的**路径**里追加一个进程内唯一的测试段，例如 `{server.uri()}/v1/t{n}`，
wiremock 的 matcher 用 `path_regex("^/v1/t\\d+/")` 而不是精确路径。缓存键不同，
路由仍然匹配。

不用查询串（`?t=n`）的原因：`base_url` 会被直接拼成 `format!("{base}{path}")`，
带查询串时拼出的是 `...?t=1/chat/completions`，路径会解析错乱。

这一规则只在测试里使用，生产代码不引入任何测试专用分支。

**（b）`ChannelCache` 跨用例串味。** 它就是一张进程级 `HashMap<String, Channel>`，
测试并行执行时同一 `base_url` 的判定会互相覆盖。上面「`base_url` 天然唯一」的
规则同时解决这个问题。

**已核实的现状**（决定了改动量）：`provider_contract.rs` 显式传
`api_style: Some("chat")`，绕过了 `Auto` 探测；`integration.rs` 的两处用
`timeout: 400ms`，与其它用例的 `5s` / `30s` 天然分键。因此这些现有测试**不需要**
改，只有新增测试要遵守上面的唯一 `base_url` 规则。

**SSE 格式差异**：辅助计数服务要分别满足两个 provider 的终止条件——
OpenAI chat 认 `data: [DONE]`（`openai/stream.rs:145`）或
`finish_reason`；Anthropic 认 `event: message_stop`
（`anthropic/stream.rs:265`）。辅助服务按请求路径
（`/chat/completions`、`/v1/messages`、`/responses`）返回对应格式，
否则测试会以 `StreamError` 失败而不是验证连接复用。

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
