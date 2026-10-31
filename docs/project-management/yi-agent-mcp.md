# yi-agent-mcp

## 模块说明

yi-agent 的 Model Context Protocol（MCP）客户端 crate。目标是连接 MCP server、发现远端工具，并通过 `yi-agent-core::Tool` 将其暴露给 agent。

## 范围边界

**做什么：**
- MCP server 连接与配置
- 远端工具发现、调用和结果映射
- 多 server 生命周期管理

**不做什么：**
- 不实现 MCP server
- 不在本 crate 实现内置文件或 Shell 工具

## Features

- [x] MCP client 与远端 Tool 适配
  - 配置解析：`yi-agent-rs/crates/yi-agent-mcp/src/config.rs`（`.yi-agent/mcp.json`，兼容 Claude Desktop schema）
  - 工具命名：`yi-agent-rs/crates/yi-agent-mcp/src/naming.rs`（`mcp__{server}__{tool}`，字符清洗 + 64 字节截断）
  - schema 缓存：`yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`（`.yi-agent/mcp-cache.json`，按 `ServerConfig` SHA-256 指纹失效）
  - 懒连接 / 调用 / 重连 / 关闭：`yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`
  - Tool 适配与安全注解映射：`yi-agent-rs/crates/yi-agent-mcp/src/tool.rs`
  - 注册入口：`yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` 的 `register_mcp_tools`
  - 运行时开关：`/mcp` slash 命令（`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`）+ agent 热刷新（`ControlCommand::McpRefresh`，`yi-agent-rs/crates/yi-agent/src/main.rs`）
  - 验证：`cargo test -p yi-agent-mcp`（含 duplex 进程内协议往返测试 `manager::tests::probe_then_call_over_duplex`）
- [x] Streamable HTTP transport（仅 POST + JSON）
  - 配置：`ServerConfig` 增加 `type`（`"stdio"` 默认 / `"http"`）、`url`、`headers`；`transport()` 集中校验
  - 展开：http header 值支持 `${ENV_VAR}`，未定义变量丢弃该 header 并 warn；缓存指纹以未展开原文参与
  - 接线：`manager.rs` 的 `connect()` 按 `Transport` 分派 `TokioChildProcess` / `StreamableHttpClientTransport`
  - 指纹：`cache.rs` 缓存指纹扩展为覆盖 `kind`/`url`/`headers`（排除 `enabled`）
  - 验证：`cargo test -p yi-agent-mcp --test http_transport`（wiremock 往返）+ `-- --ignored`（真实 horizon 端点）
- [x] 设置页只读接缝
  - 公开视图：`yi-agent-rs/crates/yi-agent-mcp/src/lib.rs:29` 的 `McpCacheEntry` 与 `:40` 的 `cached_tools_for`（只回名字/描述/read_only，不带 `input_schema`；指纹不匹配回 `None`）
  - RPC：`mcp/list`（`yi-agent-rs/crates/yi-agent-app-server/src/mcp_rpc.rs`），`Observe` 档，读 `.yi-agent/mcp.json` + `.yi-agent/mcp-cache.json`；面板见 [desktop](./desktop.md)
  - 验证：`cargo test -p yi-agent-app-server --lib mcp_rpc`、`cargo test -p yi-agent-app-server mcp_list_is_available_at_observe_scope`
- [x] 会话内自助配置（`mcp_reload`）
  - 工具：`yi-agent-rs/crates/yi-agent-mcp/src/reload_tool.rs`，四个 action（`add`/`enable`/`disable`/`remove`），结构化参数，元数据 `requires_confirmation: true`
  - 纯逻辑：`yi-agent-rs/crates/yi-agent-mcp/src/reload.rs`（名字规则、`${VAR}` 必须已定义、header 名校验、add 重名需 `overwrite`；失败不改配置）
  - 热重载：`yi-agent-rs/crates/yi-agent-mcp/src/manager.rs` 的 `McpManager::reload`（只探测新增/指纹变化的 server；单个失败原文进 `ServerOutcome::error`，不吞）、`needs_reload`（每轮变更检测：一次读盘同时拿到指纹与配置，指纹变了或有「已过期且不在退避窗口」的启用 server 就重载）
  - 原子写盘：`McpConfig::save`（临时文件 + rename，见 `config.rs`）
  - 孤儿清理：`refresh_registry` 按 `last_registered` drain，删除 server 后其工具不再残留；`enabled_tools` 同时记账（启动期注册走的是它，漏记会让「启动后停用」的工具永久留在 registry）
  - 开关优先级：`reload` 里 server 的 `enabled` **以磁盘为准**，仅当磁盘值未变时沿用内存里的运行时开关（TUI 的 `/mcp disable` 只动内存）；`mcp_reload` 的 enable/disable 会同步内存开关
  - 注册：`register_mcp_reload_tool` **无条件注册**（无 `mcp.json` 时也有入口），接在 `yi-agent-runtime/src/bootstrap.rs` 与 `yi-agent/src/main.rs` 两处
  - 权限：`mcp_reload` 始终 `NeedConfirm`，**不受 yolo 豁免**（`yi-agent-rs/crates/yi-agent-core/src/permission.rs`）
  - 生效时机：下一轮生效（不改 `ToolRegistry` 为 `RwLock`）；返回文案固定写「新工具从下一条消息起可用」
  - 验证：`cargo test -p yi-agent-mcp`（当前 121 个 lib 测试，含 `reload_does_not_reprobe_an_unchanged_server`、`reload_reprobes_a_changed_server_and_reports_the_error`、`reload_removes_a_deleted_server`、`reload_honors_a_server_disabled_on_disk`、`reload_keeps_an_in_memory_switch_when_disk_did_not_change`、`disabling_a_server_after_startup_removes_its_tools`、`enable_after_a_runtime_only_disable_takes_effect`、`config_changed_on_disk_*`、`reload_tool::tests::*`）
- [x] 工具清单按 24h TTL 自动重探，失败按 5 分钟退避
  - 判定：`yi-agent-rs/crates/yi-agent-mcp/src/cache.rs` 的 `lookup`（四步顺序：指纹不匹配→`Stale`、`Fresh`、`BackedOff`、`Stale`；**指纹判定必须先于退避**，否则改 `mcp.json` 要等 5 分钟）
  - 常量：`TTL_SECS = 24h`、`RETRY_AFTER_SECS = 5min`（同文件）
  - 时间戳：`CachedServer` 的 `probed_at`（成功探测）与 `failed_at`（失败），均 `#[serde(default)]` 兼容旧缓存
  - 单次读盘：`manager.rs` 的 `fingerprint_and_config` 一次读盘同时给出指纹与配置；`needs_reload(workdir, now)` 据此避免读两遍
  - 分派：`manager.rs` 的 `reload(workdir, now)` 逐变体匹配——`Fresh` 用**磁盘清单**（跨进程同步）、`BackedOff` 用**内存快照**（刚失败，磁盘可能缺失）、`Stale` 探测且失败时回落到内存快照并写 `failed_at`
  - 停用短路：`load_and_probe` 与 `reload` 都不探测停用 server / 总开关关闭的情况（`probe_allowed = cfg.enabled && sc.enabled`）
  - 首次迁移：`backfill_probed_at` 把无 `probed_at` 的存量条目填成 `now`，避免升级首轮集体重探（10 个 server 串行 30~40s）
  - 手动 reload 说明：`ReloadReport::skipped_backoff_names()` + `reload_tool.rs` 渲染「跳过探测（退避窗口内），沿用 N 个工具」；TUI label 列出被跳过的 server
  - 原子写：`McpCache::save` 改为同目录 `tmp + rename`，失败清临时文件
  - 验证：`cargo test -p yi-agent-mcp --lib`（121 个，全绿）；关键回归测试 `reload_refreshes_a_renamed_tool_after_the_ttl`（本 bug）、`a_permanently_dead_server_is_probed_once_per_backoff_window`（10 轮只探 1~2 次）
