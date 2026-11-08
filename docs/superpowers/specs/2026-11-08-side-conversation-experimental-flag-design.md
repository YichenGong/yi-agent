# `/side` 纳入「实验功能」开关设计

**日期**：2026-11-08
**状态**：已批准（设计三问均已由用户拍板）
**范围**：把旁问（`/side`、`/btw`、`/back`、`/side-close`）从常开能力改为
`yi-agent-flags` 注册表里的一项实验开关，关掉时三层同时消失。

## 1. 结论摘要

新增 flag `side_conversation`，默认关、dev-only、到期 2027-06-30。判定链完全
复用 `yi-agent-flags` 的既有取值链（默认 → 文件 → 环境变量 → **构建类型硬闸**），
因此 release 构建里 `/side` 根本不存在，与 `remote_ssh` 同构。关掉时：

1. TUI 侧 `/side`、`/btw`、`/back`、`/side-close` 明确报「本版本未启用旁问」，
   不当作未知命令、也不把命令文本发给模型；
2. app-server 侧 `side/*` 六个方法一律 `-32601`，`initialize` 的能力位报 `false`；
3. 桌面 / 手机据能力位自动隐藏入口，且前端在本地槽位拨关时立刻清掉旁支视图；
4. 已打开的旁支被服务端**立即关闭**（不是等它跑完），关掉再打开仍可用。

## 2. 为什么是 flag，而不是独立配置

「实验功能」页（`SettingsFlagsTab`）不是独立配置项，它只做一件事：渲染
`yi-agent-flags` 的 `REGISTRY` 并调 `flags/list` / `flags/set`。因此在注册表登记
一项，设置页自动多出一行，前端零改动。用户已确认接受随之而来的代价：该硬闸
让 release 里 `/side` 完全不存在。这是刻意的——实验功能不该漏给稳定版用户。

## 3. 三层闸门

### 3.1 注册表与判定模块

`yi-agent-rs/crates/yi-agent-flags/src/registry.rs` 增加：

```rust
pub const SIDE_CONVERSATION_FLAG: &str = "side_conversation";

FlagDef {
    name: SIDE_CONVERSATION_FLAG,
    summary: "旁问：在主会话之外开一条只读临时分支（/side、/btw）",
    owner: "yichen",
    expires_on: "2027-06-30",
    default: false,
}
```

名字与能力位 `side_conversation_v1` 同源同义（去掉版本后缀），因为两者描述的
是同一个能力的两个视角：一个是机器级开关，一个是单连接的能力声明。

新增 `yi-agent-app-server/src/side_gate.rs`，结构逐条对齐 `lead_gate.rs`：

```rust
pub const SIDE_CONVERSATION_FLAG: &str = "side_conversation";
pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool;
pub fn from_process_environment() -> bool;
pub fn is_enabled(flag_value: Option<bool>) -> bool; // None 失败关闭
```

`side_gate` 与 `lead_gate` 一样有 `the_flag_name_matches_the_registry` 测试把两个
字符串钉死——抄错名字就是静默失效，且没有任何其它测试会失败。

### 3.2 网关对象：为什么不是又一个 `bool`

`lead_agent_enabled` 是装配时算一次的 `bool`，因为组长能力不需要运行中改变。
旁问不同：用户会**在运行中**在设置页拨开关，而需求明确要求那一刻就生效
（立即关闭在跑的旁支）。所以闸门必须**每请求重读**，不能是启动快照。

但不能直接调 `yi_agent_flags::is_enabled`：它读进程级 `YI_AGENT_HOME` 与
进程级 `build_kind()`，而测试二进制的 `build_kind` 恒为 `release`，直接调会让
**现有全部 side 测试集体变红**——这正是 `lead_gate` 注释里写明的坑。

因此引入可注入的**判定对象**（不是闭包，便于测试直接构造）：

```rust
pub(crate) type SideGate = Arc<dyn Fn() -> bool + Send + Sync>;
```

- 生产：`Arc::new(crate::side_gate::from_process_environment)`
- 测试：`Arc::new(|| true)` / `Arc::new(|| false)`，或带可变开关的测试替身

放进 `RuntimeAttachments` 与 `AppState`，与 `lead_agent_enabled` 并列。

> 选型说明：可以用 `Arc<AtomicBool>` + 每次 `flags/set` 时改写，但这要求宿主
> 记住「谁改了文件」，而用户在 CLI 里 `yi-agent flags set` 时宿主看不到。闭包
> 形态让判定始终回到文件，与 `remote_ssh` 的每请求读一次完全一致。

> **分层约束**：`side_gate` 住在 `yi-agent-app-server`，而 TUI 在 `yi-agent`
> crate——后者不依赖前者，也不该依赖（会把整个 app-server 链进 TUI 二进制）。
> 所以 TUI 侧直接用 `yi_agent_flags::is_enabled(SIDE_CONVERSATION_FLAG)`，常量
> 取自 `yi_agent_flags::registry::SIDE_CONVERSATION_FLAG`。两边共用同一个名字
> 常量与同一条取值链，不共用代码，这正是 `yi-agent-flags` 作为「唯一判定点」
> 的既有定位。

### 3.3 app-server

**`initialize`**：`capabilities.side_conversation_v1` 由常量改为该连接的判定值。

```rust
"capabilities": {
    "side_conversation_v1": side_gate(),
}
```

`protocol::SIDE_CONVERSATION_V1` 这个 `bool` 常量**保留**，但语义收窄为「本协议
版本支持旁支协议」，不再是下发的值。`server/tests/side.rs` 里
`side_initialize_advertises_capability_and_a_host_id` 现按常量断言，改为断言
「与注入的 gate 值一致」——这是本功能唯一必须改动的既有测试，且改动是把
断言换成语义更强的形式（同时覆盖 true 与 false 两条）。

注意 `sideHostInstanceId` **照旧下发**：宿主身份与能力是两件事，前端用前者区分
「重连」与「宿主换代」。只翻能力位而不给 id，客户端会丢掉全部本地基线。

**`side/*` 六个方法**：在 `rpc/side.rs` 的 `owns()` 之后、参数解码之前判定。
关着时回 `RpcError::method_not_found(method)`（`-32601`），与 `remote_ssh` 关掉时
`remote/relayTargets` 的做法同构。理由：让客户端看到的是「这个方法不存在」，
而不是「权限不够」——后者会让前端渲染一个必然失败的面板。

**判定必须在 scope 检查之前**：功能不存在时，权限档不构成回答。

**`flags/set` 后的收尾**：`rpc/flags.rs` 在 `flags/set` 返回成功后，若
`name == "side_conversation"` 且新值为 `false`，用 `tokio::spawn` 发起一次
`state.side_registry.close_all()`。

- 放在**请求路径之外**：`close_all` 要等 runner 收敛，内联 `await` 会堵住主循环，
  与 `side/open` 的纪律相同（`side_creating_open_does_not_block_the_loop`）。
- 放在**宿主**而不是前端：用户在 TUI 或 CLI 拨开关时前端根本不存在，只有宿主
  能保证「开关关了，旁支真停了」。

### 3.4 `close_all` 与 `shutdown` 的区别（关键）

现状只有 `SideRegistry::shutdown()`，它把 `closed = true`（`AtomicBool`），而
`open()` 第一行就是 `if self.closed.load(...) { return Err(...) }`。**复用它做
开关关闭会让「关掉再打开」永久失效**，因为该标志位没有复位路径——它本来就是
进程退出的终态。

新增并列方法：

```rust
/// 关闭所有旁支，但不把 registry 置为终态。
///
/// 与 `shutdown()` 的唯一区别是**不碰 `closed`**：开关是可逆的，
/// 关掉之后用户还能再打开。
pub async fn close_all(&self) { ... }
```

实现取自 `shutdown()` 的主体（逐个 `close` → 撤销 replay token → 清空
`sides` / `parents`），但：

- **不置 `closed`**；
- 每条单独的关闭用 `close(side_id, None)` 的语义要小心：`expected` 是 CAS 值，
  传 `None` 只匹配「当前无活跃轮」。有在途轮的旁支会得到 `CloseConflict`。
  所以 `close_all` 走 `runner.close()` 直接关闭（与 `shutdown()` 一致），
  再逐个 `release_side_snapshots`、发布 `side/closed`，最后清空两张表。

**必须发布 `side/closed`**：它是终态，客户端靠它丢弃该 generation 的本地内容。
不发布会让前端停在一条永远不会更新的视图上。

### 3.5 TUI

`main.rs` 的旁支装配点（`side_driver::spawn_side_driver` 处）：

- flag 关 → 直接 `SideRuntime::disabled()`，**不建** `SideHostResources`、不 spawn
  driver。这是已有的失败降级路径（`side_resources` 的 `Err` 分支已经这么做），
  新代码只是把「关」也接到同一条路径上。
- 好处是不必再造一套「已禁用」状态：`side_capable` 已经是全 TUI 的唯一判据，
  所有入口（`execute_side_command_in_main`、`intercept_side_command`、
  `submit_side_draft`）都已按它分支。

**用户可见语义**：保留既有的「明确告知」路径，而不是伪装成未知命令。

现有代码在 `side_capable == false` 时已经会打印
`本次会话未启用旁支（/side 不可用）`。这符合本仓既有约定
（`hidden_commands_report_a_reason_not_unknown`：已知但未接线的命令要报明原因，
不能报「未知命令」——那会把「本版本没有」误报成「你打错了」）。

因此需要改的是**帮助与补全**，让 `/side` 不再出现在本版本不可用的目录里：

- `help_text(None)` 的「旁问」段：flag 关时不输出整段（否则用户看到一个永远
  报错的命令）；
- `help_text(Some("side" | "btw" | "back" | "side-close"))`：仍能查到，但说明里
  追加一行「本版本未启用旁问」。

`classify_conversation_input` **不改**：它已经是纯函数且不知道能力位，改动它会
破坏 §4.1 的既有分类契约。判定留在调用方（`side_capable`），这是仓库现有的分层。

> 撤回上一轮口头设计中的一个错误：我曾说「关掉时 `/side` 不识别、回落成普通
> 文本发给模型」。这与既有约定冲突，且会让用户的话被模型当成任务执行。正确
> 语义是「明确拒绝 + 说明原因」。

### 3.6 桌面 / 手机

前端本来就按能力位决定入口显隐（`parseSideHandshake` → `supported: false`），
**不需要新增任何判断**。唯一要加的收尾在 `App.tsx` 的 `flagsCall`：

`flags/set` 成功后它已经会重读 `flags/list` 并把权威值回灌（现用于 `lead_agent`）。
新增：若 `side_conversation` 变 `false`，对**本地槽位**的 binding 直接
`dropHost`，把旁支视图一起清掉。

- 只处理本地槽位：远端项目的 side flag 由那台机器的 `initialize` 决定，本机拨
  开关不该影响远端（沿用 spec 2026-11-03 §5.2 的口径）。
- 不清会怎样：宿主已经把旁支都关了，界面却还挂着一个点不动、永远
  `side_not_found` 的视图。这正是 `remote_ssh` 关掉时 `removeRemotes()` 要解决
  的同一类问题。

## 4. 数据流

```
设置页拨开关
   └─ flags/set (Control)
        ├─ 写 flags.json
        ├─ 回包
        ├─ 后台 spawn close_all()  ──▶ 逐个关闭 runner + 发布 side/closed
        └─ 前端重读 flags/list ──▶ 本地 binding dropHost（视图消失）

下一次 side/* 或 initialize
   └─ side_gate() 读 flags.json ──▶ false ──▶ -32601 / 能力位 false
```

## 5. 错误处理与边界

| 场景 | 行为 |
|---|---|
| 开关关着时收到 `side/*` | `-32601 method not found`（先于 scope 判定） |
| 开关关着时收到 `initialize` | `side_conversation_v1: false`，`sideHostInstanceId` 照旧 |
| `close_all` 期间有请求进来 | 该请求读到的已是 false，直接 `-32601`，不会看到半关闭状态 |
| `close_all` 某条关闭失败 | 记录日志，继续关其余；registry 仍可用 |
| `flags.json` 损坏 | 走既有取值链：当作未设置 → 默认关 |
| 测试二进制（build_kind 恒 release） | 全部走注入的 `SideGate`，不读全局 |
| CLI 拨开关（TUI 正在跑） | 该 TUI 进程**下次启动**生效；不承诺热重载 |

**关于 TUI 不热重载**：TUI 的旁支 driver 在启动时装配，运行中销毁它需要拆通道、
丢弃在途轮次、重建视图状态。那不是本功能的范围，且没有真实收益——用户拨的是
一个 dev-only 实验开关。spec 明确写死：TUI 下次启动生效。app-server 必须热生效，
因为桌面端就是从它读能力位。

## 6. 测试与验收判据

**注册表**
- `cargo test -p yi-agent-flags`：新 flag 已登记、`default == false`、未过期、
  与 `side_gate::SIDE_CONVERSATION_FLAG` 同名字（`find(...).is_some()`）

**闸门**
- `side_gate`：注入 `Some(true)` / `Some(false)` / `None` 分别得
  true / false / **false**（失败关闭）
- `resolve_from_store` 用 `build_kind = "release"` 时恒 false，即使文件里写着
  `true`（硬闸的鉴别测试）

**app-server**
- flag 关：`initialize` 能力位 false 且 `sideHostInstanceId` 非空（两件事独立）
- flag 关：六个 `side/*` 全回 `-32601`（逐条断言，不抽样）
- flag 开：既有 side 测试**不改**即全绿（证明没有回归）
- `close_all` 后能再次 `open` 成功；`shutdown()` 后不能（两条测试并列，
  后者是既有语义的保护，防止有人图省事把两者合并）

**TUI**
- flag 关：`help_text(None)` 不含 `/side` / `/btw` / `/back` / `/side-close`
- flag 关：`help_text(Some("side"))` 说明含「本版本未启用」
- flag 开：既有帮助测试全绿

**测试装配的既有缺口（必须一并修）**

`crates/yi-agent-app-server/src/server/tests/mod.rs` 的 `Harness` 现在没有任何
注入 side 闸门的地方（`lead_agent_enabled` 有，side 没有）。新增闸门后：

- `RuntimeAttachments` 加 `side_gate` 字段 → 所有构造点必须补值，包括
  `server/tests/thread_index.rs`、`server/tests/transport.rs`、`server/tests/mod.rs`
  的两处、`ws.rs`。漏一处就是编译错误（好事，不会静默）。
- `Harness` 的默认值必须是 `Arc::new(|| true)`：现有 side 测试全部假定能力可用，
  默认 `false` 会让它们集体变红。需要覆盖关闭路径的测试显式传 `|| false`。
- 新增 `Harness::with_side_flag(bool)` 一类的构造器，与 `with_lead_flag` 对称。
  理由同 `harness` 里既有注释：走 `flags.json` 要求 `build_kind == "dev"` 且共享
  进程级 `YI_AGENT_HOME`，并行测试必然互相污染。

**桌面**
- `npx vitest run`：新增「本地槽位关掉 `side_conversation` 后 binding 被丢弃、
  旁支视图消失、且不影响远端槽位」

**端到端**
- `just ci` 全绿；`cargo fmt --all`

## 7. 明确不做

- 不做 TUI 热重载。
- 不做按用户/租户分群。
- 不把 `session_search` 之类的常开功能再包回开关（注册表里已有守护测试）。
- 不改 `classify_conversation_input` 的纯函数契约。
- 不给 release 留逃生口（用户已明确选择 dev-only 硬闸）。

## 8. 落地清单

| 文件 | 改动 |
|---|---|
| `crates/yi-agent-flags/src/registry.rs` | 新增 flag + 测试 |
| `crates/yi-agent-app-server/src/side_gate.rs` | 新建 |
| `crates/yi-agent-app-server/src/lib.rs` | 导出模块 |
| `crates/yi-agent-app-server/src/rpc/side.rs` | 闸门前置判定 |
| `crates/yi-agent-app-server/src/rpc/flags.rs` | `flags/set` 后 spawn `close_all` |
| `crates/yi-agent-app-server/src/server/transport/mod.rs` | `initialize` 能力位；透传 `side_gate` |
| `crates/yi-agent-app-server/src/server.rs` | `RuntimeAttachments` 加字段 |
| `crates/yi-agent-app-server/src/server/transport/scoped.rs`、`ws.rs` | 生产构造点填 `from_process_environment` |
| `crates/yi-agent-app-server/src/server/tests/mod.rs`、`tests/side.rs` | Harness 默认开 + 新增可关构造器；能力位断言改为跟注入值对齐 |
| `crates/yi-agent-app-server/src/side_conversation/registry.rs` | 新增 `close_all` |
| `crates/yi-agent/src/main.rs` | flag 关走 `SideRuntime::disabled()` |
| `crates/yi-agent/src/tui/slash.rs` | 帮助目录/说明按 flag 条件化 |
| `desktop/src/App.tsx` | `flagsCall` 收尾清本地 binding |
| `docs/project-management/*.md` | 五个模块文件同步 |
