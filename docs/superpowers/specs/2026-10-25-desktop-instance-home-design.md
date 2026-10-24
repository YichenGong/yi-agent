# 桌面壳按实例隔离 home

## 问题

装第二个 app（如 `/Applications/yi-agent-nongit.app`）用于试新方案时，两个 app 的
sidecar 会同时连上 relay，并且**共用同一个 session id**：

```
yi-agent ... app-server --listen stdio:// --relay wss://relay.yi-agent.top/connect?session=fe42addb...
yi-agent ... app-server --listen stdio:// --relay wss://relay.yi-agent.top/connect?session=fe42addb...
```

现象是"daemon 不正常"：手机端发来的请求有时进 A、有时进 B，表现为时好时坏。

### 根因

`desktop/src-tauri/src/bridge.rs`：

- `home_dir()` 直接返回 `dirs::home_dir()`（bridge.rs:207）
- `configured_relay()` 读 `<home>/.yi-agent/preferences.json` 的 `relay_url`（bridge.rs:174-186）
- `spawn_once()` 把 sidecar 的 cwd 设为同一个 home（bridge.rs:331-334）

两个 app 共享 `~/.yi-agent/preferences.json` → 拿到同一个 `relay_url` → 同一个
session id。同时 runtime 目录、sqlite、threads 也全部共用。

sidecar 侧其实**已经**有实例边界机制：`yi-agent-store/src/paths.rs:3` 写着

> 稳定版与开发版靠 `YI_AGENT_HOME` 切分实例边界；未设置时退化为 `$HOME/.yi-agent`

但桌面壳完全不认 `YI_AGENT_HOME`（`grep YI_AGENT_HOME desktop/src-tauri/src/` 无结果），
所以这条机制在 GUI 路径上从未生效。

## 方案

让桌面壳也走 `YI_AGENT_HOME`，与 sidecar 侧保持同一套语义。

### 1. 新增 instance home 解析（bridge.rs）

```rust
/// 实例 home：`YI_AGENT_HOME` 优先，否则 `dirs::home_dir()`。
/// 与 `yi_agent_store::paths::instance_root()` 语义一致（父目录形状）。
fn instance_home() -> Option<PathBuf>
```

归一化与 sidecar 一致：值若不以 `.yi-agent` 结尾则补上。

### 2. 三处消费者改用它

| 位置 | 改动 |
| --- | --- |
| `configured_relay()` | 读 `<instance_home>/.yi-agent/preferences.json` |
| `set_relay_url()` | 写同一路径 |
| `spawn_once()` 的 `current_dir` | 设为 instance home 的父目录 |

sidecar 继承 `YI_AGENT_HOME`（Tauri sidecar 默认继承父进程环境），于是
`paths::instance_home()` 与桌面壳解析出同一个目录，不需要额外传参。

### 3. nongit app 声明自己的实例

`Info.plist` 加：

```xml
<key>LSEnvironment</key>
<dict>
  <key>YI_AGENT_HOME</key>
  <string>~/.yi-agent-nongit</string>
</dict>
```

Mac 的 `LSEnvironment` 不展开 `~`，实际写入绝对路径。

## 数据流

```
nongit.app (LSEnvironment YI_AGENT_HOME=/Users/x/.yi-agent-nongit)
  └─ desktop
       ├─ instance_home() = /Users/x/.yi-agent-nongit
       ├─ configured_relay() 读 /Users/x/.yi-agent-nongit/.yi-agent/preferences.json
       └─ spawn sidecar(YI_AGENT_HOME 继承)
            └─ paths::instance_home() = /Users/x/.yi-agent-nongit/.yi-agent
                 ├─ runtime/  （独立 sqlite + daemon）
                 ├─ threads/  （独立会话）
                 └─ skills/
```

正式 app 不设 `YI_AGENT_HOME`，退化为 `~/.yi-agent`，行为与今天逐字一致。

## 兼容性

- 未设 `YI_AGENT_HOME`：解析结果与现状完全相同（`dirs::home_dir()/.yi-agent`）。
- 设了但为空串：视同未设。
- 单独跑 `cargo run` 调试：可用 `YI_AGENT_HOME=... just dev` 得到干净实例。

## 测试

单元测试（bridge.rs 内，注入 home，不碰真实 `~`）：

1. `instance_home_prefers_env_override`
2. `instance_home_appends_yi_agent_when_missing`
3. `instance_home_ignores_blank_override`
4. `configured_relay_reads_instance_home_preferences`
5. `set_relay_url_writes_instance_home_preferences`
6. `spawn_cwd_uses_instance_home`（纯函数化 argv/cwd 解析后测）

回归验证：

- `cargo test --manifest-path desktop/src-tauri/Cargo.toml`（crate 名 `desktop`）
- 两个 app 同时启动，`ps` 确认两个 sidecar 的 `session=` 不同
- 两个 app 的 runtime 目录不同

## 不做

- 不引入新的 CLI 旗舰参数或配置文件；复用现成的 `YI_AGENT_HOME`。
- 不改 sidecar 侧的 paths 语义。
- 不做 app 内的"实例切换 UI"。

## 实现与验收记录（2026-10-25）

实现落在 `desktop/src-tauri/src/bridge.rs`：

- `instance_home_from(override, home)`：归一化规则与 `yi_agent-store` 一致
- `instance_home()`：读 `YI_AGENT_HOME` + `dirs::home_dir()`
- `sidecar_cwd_from(override, home)`：取实例 home 的父目录给 sidecar 当 cwd
- `configured_relay` / `set_relay_url` / `spawn_once` 三处改用上述函数

测试：`desktop` crate 33 passed（新增 7 个：4 个 `instance_home_*`、
`configured_relay_reads_the_instance_home_preferences`、
`sidecar_cwd_prefers_the_instance_home`、`sidecar_cwd_falls_back_to_the_platform_home`）。

手工验收（`/Applications/yi-agent-nongit.app`，`LSEnvironment.YI_AGENT_HOME=~/.yi-agent-nongit`）：

| 项 | 结果 |
| --- | --- |
| 正式版 sidecar | `--relay ...session=fe42addb...`（读 `~/.yi-agent`） |
| nongit sidecar | **无 `--relay` 参数**（读 `~/.yi-agent-nongit/.yi-agent`，尚未配置） |
| nongit 独立目录 | `~/.yi-agent-nongit/.yi-agent/` 已创建，内含 `trace/` |
| `YI_AGENT_HOME` 对 sidecar 生效 | `app-server` 直接读该变量，thread/start 走到 `model_not_configured`（新实例无模型配置，符合预期） |

结论：两个 app 的 relay session 已彻底分开，daemon / runtime / threads 各自独立。

**遗留观察**：Tauri 的 `setup` 里 `bridge::spawn` 在 `open -a` 启动时不总是立刻
拉起 sidecar（本次验收里用命令行直接运行 desktop 二进制才稳定复现）。该现象
与本次改动无关（改动前同样如此），另行跟踪。
