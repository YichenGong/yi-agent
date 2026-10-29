# 单一配置读法 + 不可失败的启动路径 设计

日期：2026-10-29
状态：已实现
相关 bug：[bug-list](../../bug-list.md) 首条

## 问题

命令行启动 `yi-agent` 直接打印：

```
─ Error: provider turn admission failed: API key required: set MODEL_API_KEY or use --api-key
```

一个回合都还没开始。机器上 `~/.yi-agent/models.json` 有三个完好条目（默认
`HORIZON-DeepSeek`），所以这不是「没配 key」。

## 根因

两个缺陷叠加。

### 缺陷 1：同一份配置有两种读法

| 路径 | 加载方式 | 结果 |
|---|---|---|
| `run_agent`（TUI 自己的回合） | `load_lenient` + `resolve_effective` | 读到清单里的 key，正常 |
| `build_daemon_worker_factory`（启动期拉起子 Agent runtime） | `config::load`（严格） | 读不到 key，`bail!` |

严格加载器只认「CLI 覆盖 + 真实进程环境变量」。模型键在
`migrate_model_keys` 之后已从 `.env` 注释成 `# MIGRATED: MODEL_API_KEY=...`，
改由 `models.json` 接管，因此进程环境里没有 key。

两条路径对「这台机器配好了没有」给出相反结论。这是**机制性**问题：只要存在两个
入口，任何新增的启动期调用点都可能选错那个。

### 缺陷 2：启动失败伪装成回合失败

```rust
build_daemon_worker_factory(cli, socket_path.clone())?,   // 错误逃出 attach_tui_runtime
```

落进 driver 的 `Err` 臂，被包成 `AgentError::ProviderTurnAdmission`，其 Display
是 `provider turn admission failed: {0}`。

触发条件：项目里 `subagent_runtime: "always"`，启动即自动 attach，无需用户按 `y`。

## 决定

### 1. 删除严格加载器

`RuntimeConfig::load` 与 `config::load` 一并移除，`load_inner` 不再接受
`allow_missing_key`，缺 key 一律返回空串。

**不变量**：「能否发起一次对话」只由 `models::is_usable(&cfg)` 判定，且必须在
`resolve_effective` 之后。加载器只负责把能读到的值读全。

代价：失去「启动期强制要求 key」这个语义。核对过全仓库，只有 `control_schedule`
一处生产调用者，而它的正确语义是「要发请求」，不是「要启动」——需要显式检查，不
该由加载器兜底。

### 2. 启动路径不可失败

`attach_tui_runtime` 签名：

```rust
// 前
fn attach_tui_runtime(...) -> Result<Option<TuiRuntimeSession>>
// 后
fn attach_tui_runtime(...) -> TuiRuntimeSession
```

原签名两个失败分支都是死的：5 处 return 全为 `Ok(Some(..))`，`Ok(None)` 从未
产生。唯一能把错误送进 `Err` 的是 `socket_path_for(..)?`，现在就地转成
`TuiRuntimeSession::Unavailable`——它本来就是「这次没有委派」，不是「机器坏了」。

调用方的 `Err` 臂随之删除：**启动失败没有「回合失败」这个通道可走，因为签名里
不存在该通道。**

`attach_headless_runtime`（`run --subagents`）刻意保留 `Result`：显式要求委派的
一次性命令，失败就该让命令失败。

### 3. 提示携带版本号

`runtime_restart_notice_text()` 在重启提示后追加 `（当前版本 X）`。
`render_runtime_notice_popup` 宽度按文本长度自适应（原固定 58 会裁掉版本号）。

动机：本次现场是「二进制比修复早 6 天」。旧提示与新提示一字不差，用户无法从界面
判断该重启还是该升级，只能手动 `--version`。

刻意离线：不做联网更新检查。联网检测发现得比这更晚（版本号在出错当场就有），
且引入启动期网络依赖、离线兜底与隐私问题。真要主动提醒，应放在 npm
`postinstall` 或 brew `caveats`，不是启动路径。

## 验证

| 命令 | 结果 |
|---|---|
| `cargo test -p yi-agent-runtime --lib` | 187 |
| `cargo test -p yi-agent --bin yi-agent` | 661 |
| `cargo test -p yi-agent-core --lib` | 302 |
| `cargo test -p yi-agent-app-server --lib` | 1054 |
| `cargo fmt --all -- --check` | 通过 |

变异验证（确认测试有鉴别力）：

| 变异 | 结果 |
|---|---|
| 签名改回 `Result<Option<..>>` | 6 处编译错误 |
| `load(cli)` 调回来 | `E0425` 编译失败 |
| 版本号从提示里去掉 | `the_notice_names_the_running_version` 红 |

真机实测（隔离 HOME + pty，`subagent_runtime: always`、只有 `models.json`）：

- 修复前构建：复现该错误行
- 修复后构建：无 `API key required` / `admission failed`
- `schedule add`：修复前报 `API key required`，修复后正常解析出 `0 9 * * *`

## 新增回归测试

- `attach_tui_runtime_is_infallible` —— 断言签名不含 `Result` / `Option`
- `only_the_lenient_loader_exists` —— 断言源码里不再出现 `config::load(&cli)` 形态
- `the_notice_names_the_running_version` —— 断言提示含当前版本
