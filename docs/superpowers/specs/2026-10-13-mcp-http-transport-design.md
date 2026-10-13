# 设计：为 yi-agent-mcp 增加 Streamable HTTP transport

日期：2026-10-13
状态：待评审
模块：`yi-agent-mcp`
关联：`docs/project-management/yi-agent-mcp.md`、`docs/plans/2026-09-28-mcp-client-design.md`

## 1. 背景与问题

### 1.1 直接触发：要把 horizon-websearch 装进来

用户要求把远端 MCP server `horizon-websearch` 接入 yi-agent：

```
mcp add horizon-websearch --url https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp
```

### 1.2 现状（已核实，非推测）

对 `https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp` 实测：

| 探测 | 结果 |
|---|---|
| `POST` `initialize` | 200，`serverInfo.name = horizon-websearch` v3.1.2，`protocolVersion 2025-06-18` |
| `POST` `tools/list` | 200，暴露一个工具 `SearchPro`（联网搜索） |
| `POST` `tools/call` `SearchPro` | 200，无需鉴权即返回结果 |
| `GET` 同 URL | **405 Method Not Allowed**（`allow: POST`） |
| 响应头 | 无 `Mcp-Session-Id`（**stateless**） |

这是标准的 MCP **Streamable HTTP** transport（单端点、只收 POST、可回 JSON 或 SSE、
无 session）。

但 yi-agent 的 MCP 客户端**只支持 stdio**：

- `crates/yi-agent-mcp/src/config.rs` 的 `ServerConfig` 只有 `command`/`args`/`env`，
  且 `command` 是**必填**字段——`--url` 这种配置在 yi-agent 里无处可放。
- `crates/yi-agent-mcp/src/manager.rs` 的 `connect()` 硬编码 `TokioChildProcess`。
- 全仓库搜 `StreamableHttp` / `transport-sse` / `url` 无任何传输实现。
- 设计文档（`2026-09-28-mcp-client-design.md` 决策 1）明确：**首期仅 stdio**，
  架构预留 transport 抽象以便后续加 SSE / streamable HTTP。
- yi-agent CLI 也**没有** `mcp add` 子命令（`mcp add ... --url ...` 是 Claude Code
  的 CLI 语法）。

### 1.3 结论

"装这个 MCP" 在 yi-agent 里不是改一行配置，而是**给 `yi-agent-mcp` 补上
Streamable HTTP transport**——兑现 2026-09-28 设计文档预留的抽象。

好消息：依赖的 `rmcp 3.5.0` 自带 `transport-streamable-http-client-reqwest`
feature，且 HTTP 与 stdio 两条路径最终都产出同一个
`RunningService<RoleClient, ()>` 类型，因此改动可以收敛在 `connect()` 一处，
其余（懒连接、重连一次、超时、shutdown、结果映射）零改动复用。

## 2. 目标与非目标

### 目标

- `yi-agent-mcp` 支持标准 MCP Streamable HTTP transport（仅 POST + JSON）。
- 把 `horizon-websearch` 接入 `.yi-agent/mcp.json`，让其 `SearchPro` 工具对
  yi-agent agent 可用。
- 现有 stdio 配置**零破坏**（向后兼容）。

### 非目标（YAGNI）

- **不做**旧版 HTTP+SSE transport（`transport-sse`）——只做 Streamable HTTP。
- **不做** `mcp add` CLI 子命令——单独需求，交互细节值得单独 brainstorm。
- **不做** OAuth 动态鉴权流程——只支持静态 header。
- **不做**多 server 并发探测、健康检查等本设计未涉及的增强。

## 3. 配置 schema

### 3.1 形状：单结构体 + `type` 判别（向后兼容）

`ServerConfig` 保持**单一结构体**（不改成 serde tagged enum），新增可选字段：

```rust
pub struct ServerConfig {
    /// 传输类型：`"stdio"`（默认）或 `"http"`。
    #[serde(rename = "type", default)]
    pub kind: ServerKind,          // enum { Stdio, Http }，默认 Stdio
    // ---- stdio ----
    pub command: Option<String>,   // 原为必填，现改 Option（http 时不需要）
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    // ---- http ----
    pub url: Option<String>,       // http 必填
    #[serde(default)]
    pub headers: BTreeMap<String, String>,  // 值支持 ${ENV_VAR} 展开
    // ---- 通用 ----
    #[serde(default = "default_true")]
    pub enabled: bool,
}
```

为什么用单结构体而非 externally-tagged enum：Claude Desktop / Claude Code 的
MCP schema 本身就是"可选字段 + `type`"形态，单结构体对老配置（无 `type`、有
`command`）零破坏，且校验集中在 `transport()` 一处、报错信息可控。

### 3.2 配置示例

```jsonc
// stdio（原样，缺省 type 即 stdio）
{ "command": "npx", "args": ["-y", "pkg"] }

// http（新增）
{
  "type": "http",
  "url": "https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp",
  "headers": { "Authorization": "Bearer ${HORIZON_TOKEN}" }
}
```

### 3.3 校验与内部枚举

新增 `ServerConfig::transport() -> Result<Transport>`，集中校验并把配置降维成
两个明确的变体：

```rust
pub enum Transport {
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String> },
    Http  { url: String, headers: BTreeMap<String, String> },
}
```

- `kind = Stdio` 时 `command` 必填 → 缺失返回 `Err`（错误信息含 server 名与原因）。
- `kind = Http` 时 `url` 必填 → 缺失返回 `Err`。
- 校验失败**不 panic**：沿用现状——`load_and_probe` 对单个 server 的失败
  warn 并跳过（`Vec::new()` 工具集），不阻断其它 server 与 agent 启动。

注：`command` 由必填改为 `Option` 会让"缺 command 的老配置"从**解析期报错**
变成**校验期报错**。现有 `config.rs` 单测 `rejects_missing_command` 需相应更新为
断言 `transport()` 返回 `Err`。

### 3.4 `${ENV_VAR}` 展开

- 展开只作用于 **http 的 `headers` 值**（不作用于 url / command / args）。
- 语法：`${NAME}`；只替换**整段**匹配，未定义变量 → warn + **丢弃该 header**
  （不发送空值，避免下游 401 难排查）。
- 展开发生在构造 transport 之前；**指纹用未展开的原文**（见 4 节），避免轮换
  token 时无谓地让缓存失效。

### 3.5 `enabled` 语义不变

HTTP server 复用同一套 per-server 开关、缓存指纹、懒连接、重连语义，无特殊处理。

## 4. 缓存指纹

`cache.rs` 的 `fingerprint()` 目前只序列化 `command`/`args`/`env`。改为序列化
**整个 `ServerConfig`**（含 `kind`/`url`/`headers`），再取 SHA-256：

- 改 `url` 或 `headers` → 指纹变化 → 缓存失效、重新探测。
- `enabled` 仍排除（它是运行时开关，不影响 schema），沿用现状注释的意图。
- 仍是**摘要**落盘，header 里的 token 不会明文进 `.yi-agent/mcp-cache.json`。
- 用未展开的原始 header 值参与指纹。

## 5. 传输接线

`manager.rs` 的 `connect(cfg)` 改为按 `cfg.transport()?` 分派：

```rust
match cfg.transport()? {
    Transport::Stdio { command, args, env } => {
        // 现状：TokioChildProcess::new(cmd) → ().serve(transport)
    }
    Transport::Http { url, headers } => {
        let mut cfg = StreamableHttpClientTransportConfig::with_uri(url);
        if !headers.is_empty() {
            cfg = cfg.custom_headers(expand_and_parse(headers)?);
        }
        let transport = StreamableHttpClientTransport::from_config(cfg);
        // ().serve(transport)
    }
}
```

- 两条路径返回同一个 `type Client = RunningService<RoleClient, ()>`，因此
  `acquire_client` / `call_once` / `map_result` / 重连 / `shutdown` **全部复用**。
- `allow_stateless` 默认 `true`，正好适配本端点（无 session）。
- **注意 shutdown 语义差异**：stdio 的 `Drop` 靠杀子进程；HTTP 无子进程，
  `RunningService` drop 只是关连接。现有 `shutdown()` 的 `cancel()` + 超时逻辑
  对 HTTP 依然正确（收到 cancel 即可），无需分支。

### 5.1 依赖（`yi-agent-mcp/Cargo.toml`）

```toml
rmcp = { version = "3.5", default-features = false,
         features = ["client", "transport-child-process",
                     "transport-streamable-http-client-reqwest", "reqwest"] }
```

`reqwest` feature 启用 rustls，**HTTPS（本端点是 https）必需**。

### 5.2 测试 seam

`connect()` 现有 `#[cfg(test)] TestConnector` 在测试时短路，绕开真实 spawn。
保留：HTTP 的协议往返测试走 **wiremock**（见 7 节），不依赖此 seam。

## 6. 落地配置（手写）

`.yi-agent/mcp.json`（该目录已在 `.gitignore`，仅本地生效，不进版本控制）：

```json
{
  "enabled": true,
  "mcpServers": {
    "horizon-websearch": {
      "type": "http",
      "url": "https://llmapi.horizon.auto/mcp-servers/horizon-websearch/Mcp"
    }
  }
}
```

生效后 agent 应能看到工具 `mcp__horizon-websearch__SearchPro`。

## 7. 测试

### 7.1 单元测试（`config.rs` / `cache.rs`）

- stdio 老配置（无 `type`）仍解析成功，`transport()` → `Stdio`。
- http 配置解析 → `Http`，`headers` 保留。
- `type:"http"` 缺 `url` → `transport()` 返回 `Err`。
- `type:"stdio"`（或默认）缺 `command` → `transport()` 返回 `Err`
  （替换现有 `rejects_missing_command`）。
- `${ENV_VAR}` 展开：已定义 → 替换；未定义 → 该 header 被丢弃 + warn。
- 指纹：改 `url` 使 `get_for` 失效；改 `headers` 使失效；改 `enabled` **不**失效。

### 7.2 集成测试（wiremock，`yi-agent-mcp` 新增 dev-dep）

用 wiremock mock 一个 Streamable HTTP MCP server（对同一 mock URI 依次响应
`initialize` / `tools/list` / `tools/call` 的 JSON-RPC），端到端验证：
`McpManager` 连接 → 发现 `SearchPro` → 调用 → 正确地 `map_result`。

> wiremock 已在 `yi-agent-app-server` / `yi-agent-llm` / `yi-agent-runtime` /
> `yi-agent-tools` 使用，是项目既有 dev-dep，无新引入风险。

### 7.3 真实冒烟（`#[ignore]` gate）

一个打真实 horizon 端点的测试，验证 `SearchPro` 可调。默认跳过（CI 不跑），
手动 `--ignored` 运行。无网络/无外网时安全跳过。

## 8. 流程与交付

- 在 worktree `.worktrees/feat-mcp-http-transport`（分支 `feat/mcp-http-transport`）
  开发，**不在 `main` 上直接改**。
- 更新 `docs/project-management/yi-agent-mcp.md`（新增 HTTP transport 条目）。
- 跑 `cargo test -p yi-agent-mcp` + `cargo fmt --all` 后提交。
- 合并回 `main`（`git merge --no-ff`），删分支 + 移除 worktree。

## 9. 风险与权衡

| 风险 | 处置 |
|---|---|
| `reqwest` feature 拉入较重依赖树 | 只为 HTTPS 必需；接受。可后续评估换 unix-socket/其它后端。 |
| `command` 改 `Option` 削弱了 serde 层校验 | 校验移到 `transport()`，报错仍清晰（含 server 名）；补单测覆盖。 |
| Streamable HTTP 的 SSE 分支（server 回 SSE 而非 JSON） | rmcp 已内建处理；本端点实测回 JSON。集成测试可加一条 SSE 用例作为加固（可选）。 |
| 指纹改含 `headers`，历史上已存在的缓存 | 首次运行指纹必然不匹配 → 自动重新探测一次，无副作用。 |

