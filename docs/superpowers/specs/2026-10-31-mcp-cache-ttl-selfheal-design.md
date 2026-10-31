# 设计：MCP schema 缓存按 TTL 自愈（远端工具改名后不再永久失效）

日期：2026-10-31
状态：待评审
模块：`yi-agent-mcp`
关联：`docs/project-management/yi-agent-mcp.md`、`docs/bug-list.md`、
`2026-10-13-mcp-http-transport-design.md`、`2026-10-25-mcp-reload-tool-design.md`

## 1. 背景与问题

### 1.1 现象（本机实测，非推测）

`.yi-agent/mcp.json` 里配置的 `horizon-websearch` 工具调不通：

```
mcp__horizon-websearch__SearchPro
→ -32603: upstream MCP server 'horizon-websearch' failed to call tool (after reconnect)
```

对同一端点 curl 直连，协议层完全正常：

| 请求 | 结果 |
|---|---|
| `POST initialize` | 200，`serverInfo.name = horizon-websearch` v3.1.2 |
| `POST tools/list` | 200，返回 `eo-browser-render__render_page`、`tencent-websearch__SearchPro` |
| `POST tools/call` `tencent-websearch__SearchPro` | 200，返回真实搜索结果 |
| `POST tools/call` `SearchPro`（缓存里的旧名） | 200，JSON-RPC `-32603` |

本地缓存与远端现状：

```
.yi-agent/mcp-cache.json:  horizon-websearch -> ["SearchPro"]
远端 tools/list 实际值:     ["eo-browser-render__render_page", "tencent-websearch__SearchPro"]
```

网关（AISIX）把上游 server 名拼进了工具名。上游一改名，缓存里的旧名就一直被沿用。

### 1.2 根因

`cache::McpCache::get_for` 只用 `ServerConfig` 的 SHA-256 指纹判断命中：

```rust
pub fn get_for(&self, name: &str, cfg: &ServerConfig) -> Option<&[CachedTool]> {
    let entry = self.servers.get(name)?;
    (entry.fingerprint == fingerprint(cfg)).then_some(entry.tools.as_slice())
}
```

指纹覆盖传输相关字段（kind/command/args/env/url/headers），**不含远端工具清单**。
因此「用户配置没变、远端清单变了」这个状态在缓存里不可见，用户只能手动删
`.yi-agent/mcp-cache.json` 才能恢复。

`cache.rs` 的文档已经预告了这个边界，把它当作已知限制：

> 失效只依据配置指纹,不感知远端工具列表的变化……需要强制刷新时,删除 `.yi-agent/mcp-cache.json` 即可。

实际使用中这个「手动删文件」不是可接受的逃生口：故障表现是一个工具调用失败，
报错文案（`upstream ... failed to call tool`）不指向缓存，用户无从得知要删什么。

### 1.3 为什么不能用「调用失败」触发重探

`call_tool` 里 `-32603` 映射为 `ServiceError::McpError`，但当前代码把它归进了
`CallOutcome::Transport`（`manager.rs` 的 `call_once`），于是触发了一次无意义的重连
再重试一次，最终把错误透传给模型。

即便修正这个分类，这条路仍然不可行——实测证伪了「工具不存在会回标准错误码」这个
假设：

| 请求 | 实际返回 |
|---|---|
| `tools/call` `SearchPro`（旧名） | `-32603 upstream MCP server 'x' failed to call tool` |
| `tools/call` `definitely_no_such_tool_xyz`（根本不存在） | **同样是 `-32603`**，一字不差 |
| `tools/call` `tencent-websearch__SearchPro`（正确名） | 200，正常结果 |

网关把所有上游失败都折成同一个 `-32603` 泛化文案，**不区分**「工具不存在」与
「上游临时故障」。所以调用失败无法作为「工具被改名」的信号：把它当信号会把限流、
超时、上游 5xx 一起误判成改名，并引发重探风暴；不把它当信号则只能靠时间自愈。
本设计选择后者。

### 1.4 探测成本实测

```
initialize:   0.99s
tools/list:   2.07s / 2.09s / 3.16s      (HTTP 端点，三次连续测量)
```

`register_mcp_tools` 在启动期是**阻塞**调用（`lib.rs` 的 `run_blocking`）。缓存命中
时成本为 0；若改成启动即校验，冷启动会稳定多 2~3s，惩罚最敏感的路径。

## 2. 目标与非目标

### 目标

- 远端工具清单变化后，缓存**自动**恢复，无需用户删文件。
- 不拖慢启动路径。
- 改动收敛在缓存判据一处，复用既有的 reload + 工具表重建链路。

### 非目标

- 不做漂移告警、不做缓存格式版本号治理（方案 C 的内容）。
- 不新增 `mcp_reload refresh` 动作或设置页「重新探测」按钮（方案 B 的内容）。
- 不修正 `-32603` 被误分类为传输错误的问题（见 §8，独立缺陷）。
- 不解决 stdio server 的工具清单变化实时感知。

## 3. 设计决策

### 决策 1：按时间 TTL 校验，而非启动时校验

| 选项 | 启动影响 | 自愈时机 | 结论 |
|---|---|---|---|
| 启动时校验 | 每次冷启动 +2~3s | 启动即正确 | 否决：惩罚启动路径 |
| 启动后后台校验 | 无 | 下一轮消息 | 否决：需新后台任务 + 重建触发，超出范围 |
| **每轮按 TTL 校验** | 无 | TTL 到期后的那一次对话开头 +2~3s | **采用** |

理由：两个入口（`yi-agent/src/main.rs:2489`、`yi-agent-app-server/src/server.rs:5491`）
在每轮开跑前都已调用 `config_changed_on_disk` → `reload` → `refresh_registry` →
重建 agent。扩展这个判据即可复用整条链路，无需新机制。

注意这个触发条件的代价：`needs_reload` 为真时，`reload` 会对**所有** server 逐个
判断新鲜度。未过期的仍然零成本复用，只有过期的那个付 2~3s——所以"一天一次"的开销
不会随 server 数量线性放大。

### 决策 2：TTL 记录在缓存文件里，而非进程内存

判据取值 **24 小时**，时间戳存进缓存文件：

- 桌面端 sidecar 常驻，进程内标记在「一直不重启」时等于没修——而这正是本 bug 的现场形态。
- 旧缓存文件缺该字段时 `#[serde(default)]` 读成 `None`，视为「从未校验」，下一轮重探一次。
  这就是存量用户的自动迁移路径，不需要写迁移代码。

24h 的依据：搜索类 MCP 的工具改名不是分钟级事件；一天一次的 2~3s 可接受，且能
挡住「同一分钟内反复重探」。

### 决策 3：过期要能覆盖内存快照的复用，不能只改缓存判据

`reload` 当前有一段「配置没变就直接复用内存快照」的短路：

```rust
let reusable = previous.filter(|e| e.config == *sc);
match reusable {
    Some(e) => (e.tools.clone(), false, None),   // 完全不看缓存
    ...
}
```

它的原意是「配置没变就不 spawn 子进程」（stdio 首次 `npx` 拉包最坏 55s）。但它
**先于**缓存判据执行，所以只改 `cache.rs` 的判据对本 bug 无效：进程内第二次及以后
的 `reload` 会命中这条短路，永远走不到 TTL 检查，也就永远拿不到新工具名。

因此过期判定必须**同时**作用在这条短路上：

```rust
let reusable = previous.filter(|e| e.config == *sc);
match reusable {
    // 新增「且缓存未过期」：否则 24h 后仍会复用旧快照
    Some(e) if cache.has_fresh(name, sc, now, TTL) => (e.tools.clone(), false, None),
    Some(e) => /* 快照在但已过期：走探测，失败则回落到 e.tools */,
    None => ...
}
```

这是本设计最容易漏的一处，也是「只改 cache.rs」这种最小改动必然失败的原因。

## 4. 详细设计

### 4.1 缓存结构（`crates/yi-agent-mcp/src/cache.rs`）

`CachedServer` 增加一个时间戳字段：

```rust
struct CachedServer {
    fingerprint: String,
    /// 上次成功探测远端工具清单的时间（Unix 秒）。旧缓存缺该字段 ⇒ `None`。
    #[serde(default)]
    probed_at: Option<i64>,
    tools: Vec<CachedTool>,
}
```

**判定必须用明确的枚举，不能用 `Option`。** 这是本设计最容易写错的一处：

| 情况 | 含义 | 期望行为 |
|---|---|---|
| 指纹不匹配 | 用户改了配置 | 重探 |
| 指纹匹配、已过期、**从未探测** | 新装的 server 或存量旧缓存 | 重探 |
| 指纹匹配、已过期、**上次探测失败** | 端点临时不可用 | 重探 |
| 指纹匹配、未过期 | 命中 | 直接用，不重探 |

若用 `Option<&[CachedTool]>`（`Some` = 命中），「过期且探测失败」就没有对应分支：
若回 `Some(旧工具)`，失败条目的工具名会继续生效、且永远不再重探（过期状态被固化成
命中）；若回 `None`，则与「从未探测」不可区分，探测失败会顺带清空已有工具清单。

```rust
/// 一次缓存查找的结果。两个变体足够：「旧值是否保留」是 reload 的策略，
/// 不属于查找语义，故不塞进枚举。
pub enum CacheLookup<'a> {
    /// 指纹匹配且未过期：直接用。
    Fresh(&'a [CachedTool]),
    /// 指纹不匹配、从未探测，或已过期：需要重新探测。
    Stale,
}
```

本 bug 的现场（指纹匹配、已过期、上次调用失败）落在 `Stale`，与「需要重探」一致；
至于「过期后探测失败、旧清单是否暂时保留」，由 `reload` 决定：**保留旧清单**并在
`ServerOutcome::error` 里报出失败，避免一次网络抖动把工具一次性清空。这比让
`CacheLookup` 再带一个 `Option` 载荷更简单，且把策略留在 `reload` 一处。

新增方法：

```rust
impl McpCache {
    /// 指纹匹配且 `probed_at` 在 TTL 内才返回 `Fresh`；其余一律 `Stale`。
    pub fn lookup<'a>(&'a self, name: &str, cfg: &ServerConfig, now: i64, ttl: Duration)
        -> CacheLookup<'a>;

    /// 记录一次成功探测，写入当前时间。
    pub fn put_probed(&mut self, name: &str, cfg: &ServerConfig, tools: Vec<CachedTool>, now: i64);

    /// TTL 内是否已有某 server 的新鲜条目（供 `needs_reload` 判定）。
    pub fn has_fresh(&self, name: &str, cfg: &ServerConfig, now: i64, ttl: Duration) -> bool;
}
```

`put` 保留原语义（供既有测试与既有调用点使用），内部转调 `put_probed` 并打上 `now`。

**`get_for` 不能直接删掉，它在设置页有一条语义不同的读路径。** 决策改为：

- 重命名 `get_for` → `cached_tools_ignoring_freshness`（`lib.rs` 的 `cached_tools_for`
  与 `reload` 的失败回退都用它），名字里写明「不看新鲜度」，杜绝被误当成命中判据。
- 注册/探测路径（`load_and_probe`、`reload`）只用 `lookup` / `has_fresh`。

原因是设置页 `mcp_rpc::list` 用它回答的是**另一个问题**：「这块缓存是否仍然对应
当前配置」。`None` ⇒ 面板显示 `tools: null`（「尚未探测」）。若改成 TTL 判据，一份
25 小时前的缓存会让面板显示「尚未探测」，而 agent 手上明明还有那批工具在跑——
面板与真实状态分叉，且每次开着设置页超过 24h 都会看到工具列表凭空消失。

面板展示的是「磁盘上有什么」，不是「运行时是否新鲜」，所以它继续忽略新鲜度；
`stale_cache_fingerprint_reports_unprobed_instead_of_stale_tools` 这条既有测试也
因此保持不变（它钉的是指纹不匹配，与 TTL 无关）。

**时钟注入**：`now` 通过参数传入而非在函数内取 `SystemTime::now()`，让 TTL 判定可被
单测直接驱动，不必 sleep。

### 4.2 判据接入（`crates/yi-agent-mcp/src/manager.rs`）

`config_changed_on_disk` 改名为语义更准确的 `needs_reload`（该名字现有 12 处引用，
分布在 `manager.rs`、`yi-agent/src/main.rs`、`yi-agent-app-server/src/server.rs`；
两个调用点均为 `if ... { reload() ... }` 形态），并增加「缓存过期」这个触发条件。

**TTL 判定只有一处实现**：`needs_reload` 不自己算 TTL，而是调用与 `reload` 相同的
`cache.has_fresh(...)`；`reload` 的每个 server 分支也用同一个 `has_fresh` 决定是否
复用快照。两处共用同一个函数，避免「判据说要 reload、reload 却认为都能复用」的
不一致（那会让 `needs_reload` 每轮返回真、`reload` 每轮什么都不做，形成空转）。

`reload` 的三分支（`load_and_probe` 同构）：

| `lookup` 结果 | 行为 |
|---|---|
| `Fresh(tools)` | 沿用，`probed = false` |
| `Stale` 且 `probe_tools` 成功 | `put_probed` 打新时间戳，`probed = true` |
| `Stale` 且 `probe_tools` 失败 | **保留旧清单**（若缓存里有），`error = 原因`，**不**更新时间戳 |

「失败时不更新时间戳」是刻意选择：若失败也刷新时间戳，端点挂一天就会把「清单不可用」
状态固化 24h；不刷新则下一轮继续尝试，失败成本由 `CONNECT_TIMEOUT`（30s）与
`PROBE_TIMEOUT`（20s）界定，且只在真的过期之后才发生。

「失败时保留旧清单」也是刻意选择：一次网络抖动不应让模型瞬间失去一批工具。代价是
旧名仍可能被调用并继续报错——但那比工具凭空消失更容易理解，且错误已带可操作提示
（§4.4）。

需要区分「缓存里有旧条目」与「完全没缓存」：前者失败回退到旧清单，后者失败只能给
空清单。实现上 `lookup` 的 `Stale` 变体不携带载荷，因此 `reload` 额外调用
`cached_tools_ignoring_freshness(name, cfg)` 取旧值——它只比指纹、不看时间，与
设置页共用同一函数（见 §4.1）。

### 4.3 数据流

```
每轮开始
  └─ needs_reload(workdir, now)?
       ├─ 否 → 继续
       └─ 是 → reload()
                └─ 每个 server:
                     ├─ 内存快照可复用(配置一致) 且 缓存未过期 → 沿用,probed=false
                     ├─ lookup()==Stale → probe_tools()
                     │    ├─ 成功 → put_probed()+时间戳,probed=true
                     │    └─ 失败 → 回退旧清单(若有),error=原因,时间戳不变
                     └─ refresh_registry() → 重建 agent,新工具名进入工具表
```

### 4.3.1 恢复耗时上限是 24h + 一次探测，且失败会立即重试

TTL 是「距离上次成功探测」计的，而探测失败不刷新时间戳：

```
T0       成功探测，工具名 X 正确
T0+1min  上游把 X 改名成 Y
T0+1min → T0+24h   旧名调用一直失败（缓存未过期，不会重探）
T0+24h   下一轮开头 needs_reload 为真 → 探测 → 拿到 Y，自愈
```

如果 T0+24h 那次探测恰好碰上上游抖动/限流，探测失败 → 时间戳不变 → **下一轮立刻
再试**（判据是「已过期」，不是「距上次尝试超过 N」）。所以失败不会把恢复再推迟一个
TTL，这是刻意的：换来的是端点持续不可用期间，每轮对话开头都要付一次探测超时代价。

需要明确这是一个**权衡**而不是缺陷：

| 选择 | 端点长期挂掉时的行为 |
|---|---|
| 失败不刷新时间戳（本设计） | 每轮都试，每轮 +2~3s；端点一恢复立刻自愈 |
| 失败也刷新时间戳 | 每 24h 只试一次，代价小；但端点恢复后最长还要等 24h |

本设计选前者，因为「每轮 +2~3s」只在缓存已过期**且**端点不可用时发生，是局部而非
全局降级；而「恢复后还等 24h」是用户可感知的功能性延迟。

自愈的用户可见效果：TTL 到期后的下一次对话开头，工具名更新为远端现值。在那之前，
旧名调用仍会失败——这是「不做人工入口」这个范围选择必然接受的延迟。

### 4.4 报错文案

缓存过期导致的探测失败沿用现有 `ServerOutcome::error` 透传，不新增文案。
但模型侧看到的 `-32603` 失败应带上一句可操作提示，避免用户像本次一样无从下手：

```
MCP call failed: ... (after reconnect).
The remote tool list may have changed. Delete <workdir>/.yi-agent/mcp-cache.json
and restart yi-agent; the list is re-probed on startup.
```

**文案必须写「重启」而不是「下一轮」，因为删缓存对常驻进程无效。** 缓存是在
`load_and_probe` / `reload` 时读进内存快照的，删掉磁盘文件不会让运行中的进程
重新探测——它手上那批 `McpTool` 已经是旧名。只写「删掉并重试」会让用户照做后
仍然失败，比不给提示更糟。

只在 `CallOutcome::Transport` 的最终失败分支追加，不改 JSON-RPC 错误分类（那是 §8 的独立缺陷）。

## 5. 错误处理

| 情况 | 行为 |
|---|---|
| 缓存文件损坏/缺失 | 既有行为：空缓存 → 全部重探 |
| `probed_at` 缺失（存量缓存） | 视为过期，重探一次并写入时间戳 |
| `probed_at` 在未来（时钟回拨） | 视为过期，重探并覆盖 |
| 探测超时/失败 | 保留过期状态，`error` 进 `ServerOutcome`，下一轮再试 |
| 系统时钟不可用 | `now` 取不到时退化为 `0`，等价于「总是过期」；不 panic |
| 只读文件系统导致 `save` 失败 | 既有行为：warn，不影响本次内存快照 |

## 6. 测试策略

全部为 `#[ignore]` 之外的常规单测，不需要网络（复用 `TEST_CONNECTOR` seam 与预置缓存）。

| 测试 | 断言 |
|---|---|
| `an_entry_older_than_the_ttl_is_not_fresh` | `probed_at = now - 25h` → `lookup` 返回 `Stale` |
| `an_entry_within_the_ttl_is_fresh` | `probed_at = now - 23h` → `lookup` 返回 `Fresh` |
| `a_legacy_entry_without_probed_at_is_stale` | 手写无 `probed_at` 的 JSON → `Stale` |
| `a_future_probed_at_is_treated_as_stale` | `probed_at = now + 1h` → `Stale` |
| `a_failed_probe_keeps_the_entry_stale` | 探测失败后 `probed_at` 不变，下一轮仍会重探 |
| `a_failed_probe_falls_back_to_the_old_tool_list` | 过期 + 探测失败 → 旧工具仍在、`error` 非空 |
| `a_failed_probe_with_no_cache_yields_no_tools` | 无缓存 + 探测失败 → 工具为空、`error` 非空 |
| `needs_reload_is_true_when_a_cache_entry_expired` | 配置未变 + 缓存过期 → `true` |
| `needs_reload_is_false_when_the_cache_is_fresh` | 配置未变 + 缓存新鲜 → `false`（防回归：不能每轮都重探） |
| `reload_refreshes_a_renamed_tool_after_the_ttl` | 预置旧名缓存 + 过期 → reload 后工具名变为新名 |

再加一条专钉 §3 决策 3 那个命门的测试（它覆盖的是「跳过缓存判据」这条路径，
只用缓存单测测不出来）：

| 测试 | 断言 |
|---|---|
| `reload_reprobes_when_the_in_memory_snapshot_has_expired` | 同一进程内先 `load_and_probe` 建立快照，把缓存时间戳改到 25h 前，再 `reload` → 必须重新探测且工具名更新（删掉 `Some(e) if cache.has_fresh(...)` 那个 guard 即变红） |

`reload_refreshes_a_renamed_tool_after_the_ttl` 与上一条都是本 bug 的直接回归测试。
变异验证对象：把 `needs_reload` 里的 TTL 判据删掉 → 第一条变红；把 `reusable` 分支
的 `has_fresh` guard 删掉 → 上表这条变红。两条守护的是不同路径，缺一不可。

同时修正真实端点冒烟测试（`tests/http_transport.rs`）——它已在 `a89b23f6` 中改为按
`__SearchPro` 后缀匹配，本轮只需确认它仍绿。

## 7. 验证判据

```bash
cd yi-agent-rs
cargo test -p yi-agent-mcp              # 期望：全绿，新增 10 例
cargo test -p yi-agent-mcp --test http_transport -- --ignored   # 真实端点
cargo test -p yi-agent-app-server --lib # 下游未受影响（1068+）
cargo test -p yi-agent-runtime --lib
cargo fmt --all -- --check
```

变异验证（逐条实测，回退即红）：删掉 TTL 判据 → `needs_reload_is_true_when_a_cache_entry_expired`
与 `reload_refreshes_a_renamed_tool_after_the_ttl` 变红；把 `probed_at` 的
`#[serde(default)]` 去掉 → `a_legacy_entry_without_probed_at_is_stale` 变红。

真机验收：删掉 `.yi-agent/mcp-cache.json` 之外的路径——把一个 server 的 `probed_at`
手工改到 25 小时前，启动会话并发一条消息，观察日志出现一次探测、工具名更新为远端现值。

## 8. 范围外的已知缺陷（记录，不在本 spec 处理）

1. **`-32603`（JSON-RPC 错误）被归入 `CallOutcome::Transport`**，导致无意义的重连
   重试一次，并在文案里显示 `(after reconnect)`。正确做法是区分「协议返回错误」
   与「传输失败」。独立修复。
2. **没有人工强制刷新入口**。用户遇到本类问题时最快的自救仍是删缓存文件，
   本设计只是让 24h 后自动恢复。若后续摩擦明显，加 `mcp_reload refresh`（方案 B）。
3. **stdio server 的清单变化**同样受 24h TTL 约束，但 stdio server 通常是本机安装，
   无「上游改名」场景，优先级低。
