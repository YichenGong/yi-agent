# MCP 客户端设计与实现方案

- 日期：2026-09-28
- 状态：设计定稿，待实现
- 模块：`yi-agent-mcp`
- 关联：`docs/project-management/yi-agent-mcp.md`、`docs/bug-list.md`

## 1. 背景与现状

当前 `crates/yi-agent-mcp/src/lib.rs` 只有 5 行文档注释，无任何实现。
`yi-agent-core/src/tool.rs:78` 的 `ToolSource::Mcp { server_name }` 枚举变体已存在
并有单测覆盖，但没有任何生产代码构造它。`yi-agent` 二进制 crate 声明了
`yi-agent-mcp` 依赖但从未使用。模块状态文档记为 `0 / 1`。

因此这是一个**绿地实现任务**：地基（`Tool` trait、`ToolRegistry`、`ToolSource::Mcp`
变体、crate 占位、构建接线）已就绪，缺的是全部逻辑。

模块边界（沿用既有声明）：`yi-agent-mcp` 只做 **MCP 客户端**——连接外部 MCP
server、发现远端工具、适配进 `yi-agent-core::Tool`。**不做** MCP server，**不碰**
内建文件/shell 工具。

参考实现能力（`rmcp` 3.5.0 官方 Rust SDK）：

```rust
use rmcp::{ServiceExt, transport::{TokioChildProcess, ConfigureCommandExt}};
let transport = TokioChildProcess::new(Command::new("npx").configure(|c| {
    c.arg("-y").arg("@modelcontextprotocol/server-everything");
}))?;
let client = ().serve(transport).await?;          // 长连接 Peer
let tools = client.list_all_tools().await?;        // Vec<ToolInfo>（含 schema）
client.call_tool(CallToolRequestParams::new("add")).await?;
```

## 2. 已确认的设计决策

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | Transport 范围 | **首期仅 stdio**，架构预留 transport 抽象以便后续加 SSE / streamable HTTP |
| 2 | 协议实现 | **官方 `rmcp` SDK**（`client` + `transport-child-process` feature） |
| 3 | 配置格式 | **`.yi-agent/mcp.json`**，Claude Desktop 兼容的 `{"mcpServers": {...}}` schema |
| 4 | 工具命名 | **`mcp__{server}__{tool}`**（与 Claude Code 一致，天然防冲突） |
| 5 | 加载策略 | **懒加载**：schema 走缓存，长连接在首次调用时才建立 |
| 6 | 冷启动 | 无缓存时**首次自动短连一次**建缓存，之后走缓存、不连接 |
| 7 | 安全元数据 | 读 MCP annotations：`readOnlyHint=true` → `read_only`，其余（含无标注）默认 `requires_confirmation: true` |
| 8 | 开关粒度 | **每个 server 独立**，非全局 MCP 总开关 |
| 9 | 开关语义 | **全局 = 文件默认**（`mcp.json`，跨会话）；**会话内 = 运行时临时**（slash command，不写回文件） |
| 10 | 功能范围 | 仅 tools（不做 resources / prompts / sampling） |

## 3. 配置

### 3.1 `.yi-agent/mcp.json`

```json
{
  "enabled": true,
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allow"],
      "enabled": true
    },
    "git": {
      "command": "uvx",
      "args": ["mcp-server-git", "--repository", "."],
      "enabled": false
    }
  }
}
```

- 顶层 `enabled`：全局 master 开关（默认 `true`）
- 每个 server 的 `enabled`：该 server 的默认开关（默认 `true`）
- `command` / `args` / `env`：子进程启动参数（`env` 可选）

### 3.2 schema 缓存 `.yi-agent/mcp-cache.json`

```json
{
  "servers": {
    "filesystem": {
      "config_hash": "<sha256 of command+args+env>",
      "tools": [
        {
          "name": "read_file",
          "description": "...",
          "input_schema": { "type": "object", "properties": {} },
          "read_only": true
        }
      ]
    }
  }
}
```

- 启动时按 `config_hash` 判定缓存是否有效；command/args/env 变了则视为失效、重新探测
- 缓存失效或缺失时短连一次 `tools/list` 重建并写回

## 4. 两层开关语义

- **全局（持久）**：`mcp.json` 的顶层 `enabled` 与每 server `enabled`，决定启动默认，跨会话保留，手改文件即可
- **会话内（临时）**：TUI slash command 运行时切换，只影响当前会话，**不写回文件**；重启回到全局默认

**有效状态** = `global_master && global_server_default`，会话内的 slash command 覆盖当前会话的有效状态。

关闭某 server 时：注销其工具（LLM 不可见）+ 断开连接；开启时：注册工具 + 按需懒连接。

### 4.1 Slash command 语法

| 命令 | 语义 |
|------|------|
| `/mcp` 或 `/mcp status` | 列出所有 server 及各自 effective on/off |
| `/mcp on` / `/mcp off` | 会话内切换全局 master |
| `/mcp enable <server>` / `/mcp disable <server>` | 会话内切换单个 server |
| `/mcp enable all` / `/mcp disable all` | 会话内批量 |

## 5. 架构

```
yi-agent-mcp crate
  ├── McpConfig          读 .yi-agent/mcp.json（Claude Desktop 兼容 schema）
  ├── McpCache           读/写 .yi-agent/mcp-cache.json（config_hash 失效判定）
  ├── McpManager         Arc 共享，注册进 ToolRegistry
  │   ├── master_enabled: AtomicBool            会话内 master 状态
  │   ├── servers: HashMap<String, ServerEntry>
  │   └── call_tool(server, remote_name, args)  懒连接 + 调用
  ├── McpTool            实现 yi-agent-core::Tool 的适配器（每远端工具一个）
  └── register_mcp_tools(&mut ToolRegistry, workdir)
```

### 5.1 类型

```rust
pub struct McpManager {
    servers: HashMap<String, ServerEntry>,
    master_enabled: AtomicBool,               // 会话内，init = 文件顶层 enabled
    cache_path: PathBuf,
}

struct ServerEntry {
    config: ServerConfig,                     // command / args / env
    default_enabled: bool,                     // 来自 mcp.json
    enabled: AtomicBool,                       // 会话内，init = default_enabled
    tools: Vec<ToolInfo>,                      // schema（缓存或探测得来）
    client: tokio::sync::Mutex<Option<Peer>>,  // 懒连接的长连接，None = 未连
}

pub struct McpTool {
    server: String,
    remote_name: String,                       // MCP 原始工具名（回调用）
    qualified: String,                         // mcp__{server}__{remote_name}
    description: String,
    input_schema: Value,
    read_only: bool,                           // annotations.readOnlyHint
    manager: Arc<McpManager>,
}
```

### 5.2 `impl Tool for McpTool`

- `name()` → `qualified`
- `schema()` → 缓存的 `input_schema`
- `description()` → 缓存的 description
- `metadata()` → `ToolSource::Mcp { server_name }`；`read_only = annotations.readOnlyHint`；`requires_confirmation = !read_only`
- `call(args)` → `manager.call_tool(server, remote_name, args)`

### 5.3 `McpManager::call_tool` 数据流

1. 检查 effective 状态，关闭则返回 `ToolResult::error("MCP disabled")`
2. `ensure_connected(server)`：若 `client.is_none()`，`TokioChildProcess::new(cmd)`
   → `().serve(transport)` → 存 `Peer`（懒连接点）
3. `peer.call_tool(CallToolRequestParams { name: remote_name, arguments })`，带超时
4. 结果映射到 `ToolResult`：
   - MCP `Text` → `ContentBlock::Text`
   - MCP `Image` → `ContentBlock::Image { source: Base64 { media_type, data } }`
   - MCP `Resource` / embedded → 序列化为文本
   - `isError` → `ToolResult.is_error`

### 5.4 工具名映射

远端工具名可能含非法字符或超 64 字节（Anthropic 工具名限制
`^[a-zA-Z0-9_-]{1,64}$`）。`qualified` 需 sanitize，而 `remote_name` 保留原名，
供 `call_tool` 回传。

## 6. 生命周期与注册点

### 6.1 注册点（两处，都要接）

| 路径 | 位置 | 场景 |
|------|------|------|
| shared bootstrap | `yi-agent-runtime/src/bootstrap.rs:162` `build_tool_setup_with_switch` | headless / app-server |
| TUI | `yi-agent/src/main.rs:1003` `run_agent` | 交互式 TUI |

两处均调用 `yi_agent_mcp::register_mcp_tools(&mut registry, workdir)`。

因为 `ToolRegistry` 是 `Arc<dyn Tool>` 的浅克隆（`tool.rs:127`），父 registry 里的
MCP 工具会被子 agent 自动继承（`subagent_runtime.rs:92` clone 后仅重注册 builtin）。

### 6.2 启动流程（懒加载）

1. 读 `.yi-agent/mcp.json` → 不存在或全局关则直接返回
2. 对每个 server：按 `config_hash` 查缓存 → 命中则用缓存 tools 注册；
   未命中则短连一次 `tools/list` → 写缓存 → 注册
3. 只注册 effective 状态为 on 的 server 的工具
4. **不建立长连接**——长连接在首次 `call` 时才由 `ensure_connected` 建立

### 6.3 运行时开关变更

- 新增 `ControlCommand::McpSetMaster(bool)` 与 `ControlCommand::McpSetServer { server, enabled }`
- driver 收到后改 `McpManager` 的会话状态 → 对 `current_tools` 增/删对应 `McpTool`
- 需给 `ToolRegistry` 增加 `remove(&str) -> Option<Arc<dyn Tool>>`（当前仅 `register`）

### 6.4 Teardown

- `McpManager` 实现 `Drop` → drop `Peer` → rmcp 清理子进程
- headless 路径 `main.rs:1289` 用 `std::process::exit`（**不走析构**），
  需在 exit 前显式 `manager.shutdown()`
- 参照 `Daemon`（`ipc.rs:912`）的显式 `Drop` + `stop()` 模式——这是项目里唯一
  正确清理长生命周期子资源的样板（`ProcessManager` 恰恰缺这个）

### 6.5 错误处理

- server spawn / initialize 失败 → `eprintln` warning + 跳过该 server，不阻断 agent
- 缓存读写失败 → warning + 退化为探测
- 工具调用连接断开 → 尝试重连一次，仍失败返回 `ToolResult::error`
- 调用加超时 → 超时返回 `ToolResult::error`

## 7. 测试

### 7.1 单元测试

- `mcp.json` 解析（缺字段、默认值、非法 JSON）
- 工具名 sanitize / `qualified` ↔ `remote_name` 映射
- annotations → `ToolMetadata`（`readOnlyHint` 有无、`requires_confirmation` 取反）
- 缓存 `config_hash` 失效率（command/args/env 变化）
- 两层开关 effective 状态计算

### 7.2 集成测试

- 用 `rmcp` 的 `server` feature（dev-dependency）写 in-process 假 MCP server，
  spawn 成子进程，跑通 `initialize` / `tools/list` / `tools/call`
- 断言懒加载：首次 `call` 前无子进程
- 断言 toggle：`disable` 后 `registry.schemas()` 不含该 server 的工具
- 断言结果映射：Text / Image / isError

CI 只跑 mock / 本地假 server，不依赖外部网络或真实 MCP server。

## 8. 实现步骤概览

1. `yi-agent-mcp` 加 `rmcp` 依赖（workspace + crate）
2. `McpConfig` + `McpCache` + 解析/失效逻辑（含单测）
3. `McpManager` + `McpTool` + `Tool` 适配（含单测）
4. `register_mcp_tools` 入口 + 两种注册点接线
5. `ToolRegistry::remove`
6. `ControlCommand` / `SlashCommand` 新增 + driver 处理
7. `Drop` / `shutdown` teardown
8. 集成测试（假 MCP server）
9. 同步更新 `docs/project-management/yi-agent-mcp.md`（0/1 → 完成）与 `README.md` 计数

## 9. 非目标（YAGNI）

- 不做 HTTP/SSE transport（首期），仅预留抽象
- 不做 MCP server 端
- 不做 resources / prompts / sampling / elicitation
- 不做 server 进程的健康检查 / 自动重启（失败即报错，重连一次封顶）
- 不做 GUI 集成（desktop 路线图的 P3 另议）

## 10. 实现偏差（as-built notes）

以下与上文设计不同，以实现为准：

- **§4.1 slash 语法**：`/mcp enable all` / `/mcp disable all` 未实现。master 批量
  已由 `/mcp on` / `/mcp off` 覆盖，故不再单列；单 server 用
  `/mcp enable <server>` / `/mcp disable <server>`。见
  `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` 的 `parse_mcp_args`。
- **§6.3 运行时开关**：未新增 `McpSetMaster(bool)` / `McpSetServer { server, enabled }`
  两个 `ControlCommand`。TUI 直接持有共享 `Arc<McpManager>`，就地改原子开关并发一个
  **无字段** 的 `ControlCommand::McpRefresh`，driver 收到后对 `current_tools` 做
  `refresh_registry` 并重建 agent（保持 `ControlCommand: Copy`）。
- **§6.4 Teardown**：`McpManager` **没有** `impl Drop`；改用显式
  `shutdown().await`，对每个连接 `Arc::try_unwrap` + 有界 `cancel()`（`REAP_TIMEOUT`），
  失败时回落到 `RunningService` 的 drop 路径。headless/TUI 两条路径都在 runtime
  存活时调用 `shutdown()`。
- **§7.2 集成测试**：用 `tokio::io::duplex` 进程内往返（`#[cfg(test)]` 连接器 seam），
  不 spawn 子进程；测试为 `manager::tests::probe_then_call_over_duplex`。

