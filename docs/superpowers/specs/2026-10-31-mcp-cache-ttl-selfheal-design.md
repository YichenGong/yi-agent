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

`cache::McpCache::get_for`（本设计后更名为 `cached_tools_ignoring_freshness`，见 §4.1）
只用 `ServerConfig` 的 SHA-256 指纹判断命中：

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
- 复用既有的 reload + 工具表重建链路，不新增后台任务或定时器。

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
    // `Fresh` 与 `BackedOff` 都是「本轮沿用旧快照」，只有 `Stale` 才往下探
    Some(e) if cache.lookup(name, sc, now, TTL_SECS, RETRY_AFTER_SECS).is_settled()
        => (e.tools.clone(), false, None),
    Some(e) => /* 快照在但已 Stale：走探测，失败则回落到 e.tools */,
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
    /// 上次探测**失败**的时间（Unix 秒）。用于失败退避。
    #[serde(default)]
    failed_at: Option<i64>,
    tools: Vec<CachedTool>,
}
```

`failed_at` 不是可选项，它是防性能回归的关键，见 §4.3.1。

**判定必须用明确的枚举，不能用 `Option`。** 这是本设计最容易写错的一处：

| 情况 | 含义 | 期望行为 |
|---|---|---|
| 指纹不匹配 | 用户改了配置 | 重探（不受退避约束） |
| 指纹匹配、未过期 | 命中 | 直接用，不重探 |
| 指纹匹配、已过期、从未失败过 | 首次过期 | 重探 |
| 指纹匹配、已过期、失败在退避窗口内 | 刚失败过 | **不探**，沿用旧清单 |
| 指纹匹配、已过期、失败已超退避窗口 | 可以再试了 | 重探 |

最后两行就是 `BackedOff` 与 `Stale` 的分界，也是不能用 `Option` 的原因：`Option`
只能表达「有」或「没有」，而这里需要三种处置（用、等、探）。若硬塞进 `Option`，
「过期且刚失败」只能二选一——回 `Some` 会让它永远不再重探（过期状态被固化成命中），
回 `None` 会与「从未探测」混同并清空已有清单，且必然破坏退避。

```rust
/// 一次缓存查找的结果。「旧值是否保留」是 reload 的策略，不属于查找语义，
/// 故不塞进枚举。
pub enum CacheLookup<'a> {
    /// 指纹匹配且未过期：直接用。
    Fresh(&'a [CachedTool]),
    /// 已过期但仍在失败退避窗口内：本轮不探测。
    BackedOff,
    /// 指纹不匹配、从未探测，或已过期且允许重试：需要重新探测。
    Stale,
}

impl CacheLookup<'_> {
    /// 「本轮无事可做」：`Fresh` 与 `BackedOff` 都为真。
    /// `reload` 与 `needs_reload` 都用它，才不会有第二处枚举匹配漂移。
    pub fn is_settled(&self) -> bool {
        !matches!(self, CacheLookup::Stale)
    }
}
```

`BackedOff` 与 `Stale` 必须分开，理由见 §4.3.1：合并成一个变体的话，`reload` 无法
区分「该探」与「该等」，只能二选一——探则每轮 55s，等则永远不恢复。

本 bug 的现场是「指纹匹配、已过期」，走 `Stale` 分支重探一次即可自愈——注意
**调用失败不会被记成探测失败**：`call_tool` 的失败不写 `failed_at`，只有 `probe_tools`
失败才写。否则一次工具调用报错就会把重探推迟 5 分钟，让自愈变得不可预测。

至于「过期后探测失败、旧清单是否暂时保留」，由 `reload` 决定：**保留旧清单**并在
`ServerOutcome::error` 里报出失败，避免一次网络抖动把工具一次性清空。这比让
`CacheLookup` 再带一个 `Option` 载荷更简单，且把策略留在 `reload` 一处。

新增方法：

```rust
impl McpCache {
    /// `Fresh` 条件：指纹匹配且 `probed_at` 在 TTL 内。
    /// `BackedOff` 条件：不新鲜，但 `failed_at` 在退避窗口内。
    /// 其余（指纹不匹配 / 从未探测 / 过期且可重试）⇒ `Stale`。
    pub fn lookup<'a>(
        &'a self,
        name: &str,
        cfg: &ServerConfig,
        now: i64,          // Unix 秒
        ttl: i64,          // 秒，TTL_SECS
        retry_after: i64,  // 秒，RETRY_AFTER_SECS
    ) -> CacheLookup<'a>;

    /// 记录一次成功探测（工具清单 + 时间戳）。
    pub fn put(&mut self, name: &str, cfg: &ServerConfig, tools: Vec<CachedTool>, now: i64);

    /// 记录一次**失败**的探测尝试，只更新时间戳，不动工具清单。
    pub fn note_failure(&mut self, name: &str, cfg: &ServerConfig, now: i64);

}
```

`lookup` 内部只用两个**模块级私有**函数，各管一个时钟、互相不写对方对象
（不挂在 `McpCache` 上，也不对外暴露，避免出现第二份 TTL 判据）：

```rust
/// 工具清单是否还新鲜（决定旧工具名能否继续用）。
/// 时间戳在未来（时钟回拨）或缺失 ⇒ 不新鲜。
fn is_fresh(probed_at: Option<i64>, now: i64, ttl: i64) -> bool {
    probed_at.is_some_and(|t| t <= now && now - t < ttl)
}

/// 现在是否处于失败退避窗口内（决定要不要发起探测）。
/// 时间戳在未来 ⇒ **不**退避（宁可多试一次，也不要因为时钟跳变把退避拉长到无限）。
fn is_backed_off(failed_at: Option<i64>, now: i64, retry_after: i64) -> bool {
    failed_at.is_some_and(|t| t <= now && now - t < retry_after)
}
```

时间一律用 **Unix 秒（`i64`）**，不用 `Duration`：缓存文件里存的就是秒，比较时若混用
`Duration` 就得到处 `as_secs()`，反而多一处可错的转换。常量：

```rust
const TTL_SECS: i64 = 24 * 3600;
const RETRY_AFTER_SECS: i64 = 5 * 60;
```

两个函数都用 `t <= now` 显式挡掉负 delta。少了这一条，`now - t < ttl` 在 `now < t`
时为真——时钟回拨或 `failed_at` 比 `now` 新时，条目会被判成「永远新鲜」，或「永远
退避」，两种都是永久失效。

`put` **必须跟着改签名**为 `put(&mut self, name, cfg, tools, now)`：它现在的签名里
没有时间参数，若不改就只能内部调 `SystemTime::now()`，这与下面「时钟注入」的原则
直接矛盾（测试无法把 `probed_at` 摆到任意位置）。既有调用点共 17 处，逐处补 `now`：

| 文件 | 处数 | 备注 |
|---|---|---|
| `cache.rs` | 5 | 全部在 `#[cfg(test)]` 内 |
| `manager.rs` | 6 | 生产 2（`load_and_probe`、`reload`）、测试 4 |
| `lib.rs` | 2 | 测试 |
| `reload_tool.rs` | 2 | 测试 |
| `mcp_rpc.rs` | 2 | 测试 |

生产代码只有 `manager.rs` 那 2 处，其余 15 处是测试——签名一改会连锁报错，属于
预期内的机械改动，不必为兼容旧签名留重载。

保留 `put` 这个名字而不用 `put_probed`：两者语义相同（记录一次成功的清单），多一个
名字只会让人以为行为有差别。

**`get_for` 不能直接删掉，它在设置页有一条语义不同的读路径。** 决策改为：

- 重命名 `get_for` → `cached_tools_ignoring_freshness`（`lib.rs` 的 `cached_tools_for`
  与 `reload` 的失败回退都用它），名字里写明「不看新鲜度」，杜绝被误当成命中判据。
- 注册/探测路径（`load_and_probe`、`reload`）只用 `lookup`（其判定细节不外露）。

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

`config_changed_on_disk` 改名为语义更准确的 `needs_reload`，并增加「缓存过期」这个
触发条件。原方法体里的指纹比较抽成私有 `fingerprint_changed`（其余不变）。

`config_changed_on_disk` 当前共 12 处出现：定义 1 处、`manager.rs` 内测试 9 处、
两处调用点各 1 处（`yi-agent/src/main.rs:2489`、`yi-agent-app-server/src/server.rs:5491`，
均为 `if ... { reload() ... }` 形态）。改签名只动这 12 处，测试里补一个 `now` 实参。

**判定只有一处实现，且入口必须是 `lookup`。** `needs_reload` 的触发条件是
「**存在某个 server 的 `lookup` 返回 `Stale`**」，`reload` 的每个 server 分支用同一个
`lookup` 决定走哪条路。`is_fresh` / `is_backed_off` 降为 `lookup` 的私有辅助函数，
不再对外暴露——对外只留一个判断入口，才不会有第二份 TTL 逻辑漂移。

这里不能退化成「`needs_reload` 判 `!is_fresh`」。第二轮加退避后，这两个判据在退避
窗口内必然分叉：`!is_fresh` 为真（条目确实过期），但 `reload` 因 `BackedOff` 什么都不
做。结果是 `needs_reload` 每轮返回真 → 每轮 `reload` → 每轮 `McpCache::load` + `save`
（磁盘 I/O）→ 每轮 `refresh_registry` → 每轮 clone 整个工具表并重建 `Agent`。端点坏掉
期间每轮白做一遍，正是这段本该防住的空转。用 `lookup == Stale` 作触发条件就没有这个
缺口：`Fresh` 和 `BackedOff` 都表示「本轮无事可做」。

```rust
pub fn needs_reload(&self, workdir: &Path, now: i64) -> bool {
    // 1. 配置指纹变了 ⇒ 一定重载（既有逻辑，先于缓存判断）
    if self.fingerprint_changed(workdir) { return true; }

    // 2. 配置没变，但某个 server 的缓存条目已过期且允许重试 ⇒ 也要重载
    let Ok(Some(cfg)) = McpConfig::load(workdir) else { return false };
    let cache = McpCache::load(workdir);
    cfg.mcp_servers.iter().any(|(name, sc)| {
        matches!(cache.lookup(name, sc, now, TTL_SECS, RETRY_AFTER_SECS), CacheLookup::Stale)
    })
}
```

两条注意：

- **配置读不出来时返回 `false`**：`McpConfig::load` 失败（文件损坏）时既有路径会让
  `reload` 报错并展示，不该在这里短路成「配置变了」——指纹逻辑已经覆盖了「文件被删 /
  被删空」这些情况，重复判断只会掩盖真正的配置错误。
- `needs_reload` 每轮多一次 `mcp-cache.json` 读取（小文件，与既有的 `mcp.json` 读取
  同级）。`reload` 进来后会再 load 一次，两次之间别人可能刚好写了新时间戳——这只会让
  本轮少探一次（下次再来）或多探一次（本来就允许），不会产出错误的工具清单。为省这
  一次读取而把签名改成「传一份快照」，反而会让两个入口各自持有一份可能更旧的状态。

`reload` 按 `lookup` 结果分四条路径（`load_and_probe` 用同样的 `lookup` 三态，仅
`Fresh` 的处置不同，见本节末尾）：

| `lookup` 结果 | 行为 |
|---|---|
| `Fresh(tools)` | 沿用，`probed = false` |
| `Stale` 且 `probe_tools` 成功 | `put` 写新清单与时间戳，`probed = true` |
| `BackedOff` | 不探测，沿用旧清单，`probed = false` |
| `Stale` 且 `probe_tools` 失败 | **保留旧清单**（若缓存里有），`error = 原因`，`note_failure` 写 `failed_at` |

「失败不改 `probed_at`」是刻意选择：若成功时间戳被失败刷新，端点挂一天就会把
「清单已过期」状态固化成「新鲜」，`is_fresh` 会误判为命中，此后 24h 内再也不会重探。

「失败要写 `failed_at`」是另一半：它只影响**要不要再试**，不影响**清单是否新鲜**。
两个时钟职责分离，才不会重现 §4.3.1 描述的每轮 55s 回归。

「失败时保留旧清单」也是刻意选择：一次网络抖动不应让模型瞬间失去一批工具。代价是
旧名仍可能被调用并继续报错——但那比工具凭空消失更容易理解，且错误已带可操作提示
（§4.4）。

`load_and_probe`（启动路径）与 `reload` 用同一套 `lookup` 三态判断，**唯一的区别在
`Fresh` 的处置**：启动时内存里还没有客户端，即使工具清单是新鲜的也必须建连（只跳过
`tools/list`）；而 `reload` 在 `Fresh` 时连快照一起复用，不建连。`BackedOff` 与
`Stale` 在两条路径上行为一致（`BackedOff` 用旧清单、`Stale` 探测）。

这个差别在当前代码里就已存在（启动路径 `get_for` 命中后仍会 new client），本设计
不改变它，只是把判据换成 `lookup`。

`load_and_probe` 现在用 `cache_dirty` 标记「需要写盘」。必须让 `note_failure` 也置位，
否则启动时探测失败写下的 `failed_at` 不落盘，进程重启后立刻重试——正是 §4.3.1 说
「落盘才不放大代价」要防的情形。

需要区分「缓存里有旧条目」与「完全没缓存」：前者失败回退到旧清单，后者失败只能给
空清单。实现上 `lookup` 的 `Stale` 变体不携带载荷，因此 `reload` 额外调用
`cached_tools_ignoring_freshness(name, cfg)` 取旧值——它只比指纹、不看时间，与
设置页共用同一函数（见 §4.1）。

### 4.3 数据流

```
每轮开始
  └─ needs_reload(workdir, now)?   // 指纹变了 或 任一 server lookup()==Stale
       ├─ 否（未变 / 全 Fresh / 全 BackedOff）→ 继续本轮
       └─ 是 → reload()
                ├─ 对每个 server（顺序执行，见下）:
                │    ├─ lookup()==Fresh      → 沿用旧快照,probed=false
                │    ├─ lookup()==BackedOff  → 沿用旧清单,不探测
                │    └─ lookup()==Stale      → probe_tools()
                │         ├─ 成功 → put(tools, now)  → probed=true
                │         └─ 失败 → 回退 cached_tools_ignoring_freshness(若有)
                │                   note_failure(now) → error=原因
                └─ 循环结束后（只做一次）:
                     ├─ cache.save(workdir)（有改动才写）
                     └─ refresh_registry() → 重建 agent,新工具名进入工具表
```

注意 `refresh_registry` 与重建 agent 在**循环之外**、每轮最多一次；把它画进 per-server
循环会让人以为每个 server 都重建一遍 agent。

### 4.3.1 失败必须退避，否则每轮都付 55 秒

这是本设计唯一可能造成**严重性能回归**的地方，初稿写错过。

如果「探测失败不刷新任何时间戳」，那么一个**长期不可用**的 server（离线、端点下线、
凭据过期）会让 `needs_reload` 每一轮都为真，`reload` 每一轮都去探它。单次探测的成本是：

```
CONNECT_TIMEOUT 30s + PROBE_TIMEOUT 20s + REAP_TIMEOUT 5s = 55s     （每轮，每个坏 server）
```

当前 main 上不会有这个问题：`config_changed_on_disk` 只比指纹，坏 server 不触发
reload。所以这是**新增**的回归，不是既有缺陷。方向必须反过来。

引入第二个时间戳，两个时钟各管一件事：

| 字段 | 含义 | 谁写 |
|---|---|---|
| `probed_at` | 上次**成功**探测 | 探测成功时 |
| `failed_at` | 上次**失败**尝试 | 探测失败时 |

判据拆成两条，互不干扰（实现见 §4.1 的 `is_fresh` / `is_backed_off`，此处不重复定义）：

| 时钟 | 周期 | 回答的问题 |
|---|---|---|
| `fresh` | 24h（`TTL_SECS`） | 旧工具名还能不能继续用 |
| `backed_off` | 5min（`RETRY_AFTER_SECS`） | 现在允不允许再试一次 |

`Stale`（= `!fresh && !backed_off`）才会真正触发探测；否则沿用旧清单、不探测。于是：

| 场景 | 行为 |
|---|---|
| 缓存新鲜 | 零成本复用 |
| 缓存过期、从未失败过 | 探测一次（+2~3s） |
| 缓存过期、上次失败在 5 分钟内 | 跳过，本轮零成本；旧清单继续可用 |
| 缓存过期、上次失败超 5 分钟 | 再试一次 |
| 完全无缓存（新装）、探测失败 | 记 `failed_at`，之后 5 分钟内不再试，工具为空 |

`RETRY_AFTER_SECS = 5min` 的依据：自愈延迟从 24h 变成「24h + 最多 5 分钟」，用户不可感知；
而最坏情况开销从「每轮 55s」降到「每 5 分钟 55s」。两者相差约 3 个数量级。

为什么失败也要写盘（`note_failure`）：`failed_at` 若不落盘，进程重启后又会立刻重试一次
——重启频繁时同样会放大代价。落盘后重启不会重置退避。

### 4.3.2 恢复耗时

```
T0       成功探测，工具名 X 正确
T0+1min  上游把 X 改名成 Y
T0+1min → T0+24h   旧名调用失败（缓存未过期，不重探）
T0+24h   下一轮开头 needs_reload 为真 → 探测 → 拿到 Y，自愈
```

若 T0+24h 那次探测碰上上游抖动，`failed_at` 记录之，5 分钟后重试；端点恢复后最多
再等 5 分钟即自愈。所以恢复耗时的上限是 **TTL + `RETRY_AFTER_SECS`**，而不是无限重试。

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
| `failed_at` 在未来（时钟回拨） | 视为**不**在退避窗口内，允许重试一次并覆盖 |
| 探测超时/失败 | 保留旧清单，写 `failed_at`，`error` 进 `ServerOutcome`，退避 5 分钟后重试 |
| 端点长期不可用 | 每 `RETRY_AFTER_SECS` 付一次 55s，而不是每轮 |
| 系统时钟不可用（`now` 取不到） | `now` 退化为 `0`，此时 `t <= now` 对任何真实时间戳都不成立 ⇒ `is_fresh` 与 `is_backed_off` 同为 `false` ⇒ 所有条目 `Stale`。为避免「每轮付 55s」，`needs_reload` 在取不到时钟时只做指纹判断、跳过 TTL 触发；不 panic |
| 只读文件系统导致 `save` 失败 | 既有行为：warn，不影响本次内存快照 |

## 6. 测试策略

全部为 `#[ignore]` 之外的常规单测，不需要网络（复用 `TEST_CONNECTOR` seam 与预置缓存）。

| 测试 | 断言 |
|---|---|
| `an_entry_older_than_the_ttl_is_not_fresh` | `probed_at = now - 25h` → `lookup` 返回 `Stale` |
| `an_entry_within_the_ttl_is_fresh` | `probed_at = now - 23h` → `lookup` 返回 `Fresh` |
| `a_legacy_entry_without_probed_at_is_stale` | 手写无 `probed_at` 的 JSON → `Stale` |
| `a_future_probed_at_is_treated_as_stale` | `probed_at = now + 1h` → `Stale` |
| `a_failed_probe_does_not_make_the_entry_fresh` | 探测失败后 `probed_at` 不变（不能假装新鲜） |
| `retry_is_skipped_within_the_backoff_window` | `failed_at = now - 1min` → 不探测、旧清单照旧 |
| `retry_happens_after_the_backoff_window` | `failed_at = now - 6min` → 再试一次 |
| `a_failure_persists_the_backoff_across_restarts` | 失败后重新 `McpCache::load`，`failed_at` 仍在 |
| `a_permanently_dead_server_is_probed_once_per_backoff_window` | 连续 10 轮 `needs_reload`，探测次数为 1~2，不是 10 |
| `a_failed_probe_falls_back_to_the_old_tool_list` | 过期 + 探测失败 → 旧工具仍在、`error` 非空 |
| `a_failed_probe_with_no_cache_yields_no_tools` | 无缓存 + 探测失败 → 工具为空、`error` 非空 |
| `needs_reload_is_true_when_a_cache_entry_expired` | 配置未变 + 缓存过期 → `true` |
| `needs_reload_is_false_when_the_cache_is_fresh` | 配置未变 + 缓存新鲜 → `false`（防回归：不能每轮都重探） |
| `needs_reload_is_false_while_a_stale_entry_is_backed_off` | 缓存过期 + `failed_at = now - 1min` → `needs_reload` 为 `false`（防每轮空转 reload + 重建 agent） |
| `reload_refreshes_a_renamed_tool_after_the_ttl` | 预置旧名缓存 + 过期 → reload 后工具名变为新名 |

再加一条专钉 §3 决策 3 那个命门的测试（它覆盖的是「跳过缓存判据」这条路径，
只用缓存单测测不出来）：

| 测试 | 断言 |
|---|---|
| `reload_reprobes_when_the_in_memory_snapshot_has_expired` | 同一进程内先 `load_and_probe` 建立快照，把缓存时间戳改到 25h 前，再 `reload` → 必须重新探测且工具名更新（删掉 `Some(e) if lookup(...).is_settled()` 那个 guard 即变红） |

`reload_refreshes_a_renamed_tool_after_the_ttl` 与上一条都是本 bug 的直接回归测试。
变异验证对象：把 `needs_reload` 里的 TTL 判据删掉 → 第一条变红；把 `reusable` 分支
的 `is_settled()` guard 删掉 → 上表这条变红。两条守护的是不同路径，缺一不可。

同时修正真实端点冒烟测试（`tests/http_transport.rs`）——它已在 `a89b23f6` 中改为按
`__SearchPro` 后缀匹配，本轮只需确认它仍绿。

## 7. 验证判据

```bash
cd yi-agent-rs
cargo test -p yi-agent-mcp              # 期望：全绿，新增 16 例
cargo test -p yi-agent-mcp --test http_transport -- --ignored   # 真实端点
cargo test -p yi-agent-app-server --lib # 下游未受影响（1068+）
cargo test -p yi-agent-runtime --lib
cargo fmt --all -- --check
```

变异验证（逐条实测，回退即红）：

- 删掉 TTL 判据 → `needs_reload_is_true_when_a_cache_entry_expired`、
  `reload_refreshes_a_renamed_tool_after_the_ttl` 变红。
- 去掉 `probed_at` 的 `#[serde(default)]` → `a_legacy_entry_without_probed_at_is_stale` 变红。
- 把 `needs_reload` 的触发条件从 `lookup()==Stale` 退回 `!is_fresh` →
  `needs_reload_is_false_while_a_stale_entry_is_backed_off` 变红。
- 把 `is_backed_off` 改成恒 `false` → `a_permanently_dead_server_is_probed_once_per_backoff_window`、
  `needs_reload_is_false_while_a_stale_entry_is_backed_off` 变红
  （这条专门守护性能回归，是 §4.3.1 的直接防线）。

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
