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
- 不让 headless（`yi-agent run`）在进程内自愈：它没有每轮检查，且启动路径刻意不校验
  TTL（§3 决策 1、§4.2）。

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

**这个机制在 headless（`yi-agent run`）下不存在**：`run_headless` 没有每轮检查，只在
启动时装配一次。因此 headless 进程内不会自愈——启动路径刻意不校验 TTL（见 §4.2），
否则就等于把否决掉的「启动时校验」加回去，而 headless 是唯一只有启动路径的场景。

注意这个触发条件的代价：`needs_reload` 为真时，`reload` 会对**所有** server 逐个
判断新鲜度。未过期的仍然零成本复用，只有过期的那个付 2~3s——所以"一天一次"的开销
不会随 server 数量线性放大。

### 决策 2：TTL 记录在缓存文件里，而非进程内存

判据取值 **24 小时**，时间戳存进缓存文件：

- 桌面端 sidecar 常驻，进程内标记在「一直不重启」时等于没修——而这正是本 bug 的现场形态。
- 旧缓存文件缺该字段时 `#[serde(default)]` 读成 `None`。**但迁移时不能让它触发
  「所有存量条目在同一轮集体重探」**——那是用户可感知的数分钟卡顿，处理方式见
  §4.3.1.1.1（迁移时填成「现在」，而非留 `None`）。

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

**若采纳 §4.3.1.1(b) 的错峰抖动，抖动必须发生在调用方、不能藏进 `put`。**
`put` 是唯一写入 `probed_at` 的地方，测试要靠它把时间戳精确摆到 `now - 25h` 这类
位置；若 `put` 内部悄悄加抖动，`a_legacy_entry_without_probed_at_is_stale` 这类断言
就会变得依赖运气。正确做法是 `reload` / `load_and_probe` 在调用 `put` 前算好
`put_at = now ± jitter(name)` 再传进去——时钟注入的意义正是「时间由调用方决定」。

**`get_for` 不能直接删掉，它在设置页有一条语义不同的读路径。** 决策改为：

- 重命名 `get_for` → `cached_tools_ignoring_freshness`（`lib.rs` 的 `cached_tools_for`、
  `load_and_probe` 的启动判据、`reload` 的失败回退都用它），名字里写明「不看新鲜度」，
  杜绝被误当成每轮的自愈判据。
- `reload`（每轮 / 手动）只用 `lookup`（其判定细节不外露）。
- `load_and_probe`（启动）**继续用** `cached_tools_ignoring_freshness`，不看 TTL
  ——决策 1 要求不惩罚启动路径，理由见 §4.2。

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

    // 2. 配置没变，但某个 server 的缓存条目已过期且允许重试 ⇒ 也要重载。
    //    复用第 1 步已解析出的配置，不重复读盘（见下）。
    let Some((_, cfg)) = self.fingerprint_and_config(workdir) else { return false };
    let cache = McpCache::load(workdir);
    cfg.mcp_servers.iter().any(|(name, sc)| {
        matches!(cache.lookup(name, sc, now, TTL_SECS, RETRY_AFTER_SECS), CacheLookup::Stale)
    })
}
```

**实现要点：把现有的 `config_fingerprint` 拆成「读一次盘、同时拿到指纹和配置」**
（`fingerprint_and_config`），`needs_reload` 内部只读一次。既有的
`config_fingerprint` 每轮已经 `read_to_string` + `serde_json::from_str` 一次，
若按最直观的写法再调一次 `McpConfig::load`，每轮就要多读多解析一遍 `mcp.json`。
把解析结果传下去，比多读一次省事得多——这是纯重构，行为不变。

两条注意：

- **配置读不出来时返回 `false`**：`McpConfig::load` 失败（文件损坏）时既有路径会让
  `reload` 报错并展示，不该在这里短路成「配置变了」——指纹逻辑已经覆盖了「文件被删 /
  被删空」这些情况，重复判断只会掩盖真正的配置错误。
- `needs_reload` 每轮多一次 `mcp-cache.json` 读取（小文件，本机实测 2 KB，与既有的
  `mcp.json` 读取同级；`mcp.json` 的读盘一次都不增加，见上）。`reload` 进来后会再
  load 一次，两次之间别人可能刚好写了新时间戳——这只会让本轮少探一次（下次再来）或
  多探一次（本来就允许），不会产出错误的工具清单。为省这一次读取而把签名改成「传一份
  快照」，反而会让两个入口各自持有一份可能更旧的状态。

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

**`load_and_probe` 保持原有语义：只看指纹，不看 TTL。**

这是决策 1 的直接结论，也是初稿写错的地方。决策 1 明确否决了「启动时校验」，理由是
冷启动会稳定多 2~3s。但若让 `load_and_probe` 也走 `lookup`，过期条目在**启动时**就会
被探测——等于把被否决的选项改了名字加回来。

而且这不仅是理论问题：`yi-agent run`（headless）**没有每轮检查**（`run_headless`
里没有 `config_changed_on_disk` 调用，只在启动时 `register_mcp_tools` 一次）。对
headless 而言，「启动时校验」是唯一会发生的校验，也就是它必然付出这笔开销：一次
`yi-agent run` 本来只跑一个 prompt，却要先卡 2~3s（最坏 55s）去探测工具清单。

所以分工是：

| 路径 | 判据 | 理由 |
|---|---|---|
| `load_and_probe`（启动） | `cached_tools_ignoring_freshness`（只比指纹） | 决策 1：不惩罚启动；headless 只有这一条路径 |
| `reload`（每轮 / 手动） | `lookup`（指纹 + TTL + 退避） | 自愈发生在这里 |

`load_and_probe` 仍需写 `probed_at`（探测成功时）与 `failed_at`（探测失败时），否则
TTL 计时与退避在「只启动、不 reload」的 headless 场景下永远不会开始记。

**headless 因此不自愈**：一次 `yi-agent run` 的进程中缓存若已过期，它仍会用旧工具名
跑完这次 prompt，直到某次 TUI / app-server 会话触发 `reload` 把清单刷新。这是可接受
的——headless 是短命进程，缓存过期只影响少量调用，而代价（每次启动 +2~3s）会落在
所有调用上。这一点必须写进文档，避免后续被当成缺陷重新「修复」。

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
而最坏情况开销从「每轮 55s」降到「每 5 分钟 55s」。

### 4.3.1.1 三个尚未被覆盖的放大路径

上面那句「每 5 分钟一次」只在「单个坏 server、每轮间隔大于 0」时成立。以下三种情形会
放大它，实现时必须处理，否则这个防回归措施本身会变成新的回归。

**（a）`reload` 不检查 `enabled`，停用的坏 server 照样占满探测预算。**

`reload` 的循环体（`manager.rs:226`）只按配置指纹和缓存判据决定是否探测，
**完全不看 `sc.enabled`**——`enabled` 只影响工具是否注册进 registry（`refresh_registry`
里过滤）。现状下这没造成问题，因为停用的 server 一旦进过缓存就不再重探。

本设计改变了这一点：停用的坏 server 也会因为 24h 过期而被重新探测。用户特意停用一个
端点，24h 后却仍要为它每 5 分钟付一次 55s。修法是在 `lookup` 之前加一条：`sc.enabled ==
false` 且缓存里有条目时，直接沿用条目、不探测（工具反正不注册）。这既省掉无谓探测，
也符合「停用就是别碰它」的直觉。

**（b）探测是串行的，代价是 `N × 55s` 而不是 55s。**

`probe_tools` 在 `for` 循环里逐个 `await`（`manager.rs:133` 的文档注释自己写着
`N * (CONNECT_TIMEOUT + PROBE_TIMEOUT + REAP_TIMEOUT)`）。本设计让所有过期条目在
**同一轮**里集体触发探测：一个装了 5 个 HTTP server、且它们同时到期的会话，那一轮开头
会卡 `5 × 55s ≈ 4.6 分钟`。这是**用户可感知的卡顿**，不是理论上的开销。

缓解方式（按性价比排序，实现时至少取第一条）：
1. 把过期时刻**错开**：`put` 写入 `probed_at` 时加一个基于 server 名的确定性抖动
   （0~2 小时），避免所有 server 在同一轮集体到期。
2. 限制每轮最多探测 `MAX_PROBES_PER_RELOAD = 1` 个 server，其余顺延到后续轮次。
   自愈延迟从「TTL」变成「TTL + N 轮」，仍远小于「永不恢复」，而单轮卡顿有上界。

**（c）47 小时的往返不可避免，但 55s 的上界需要重新核对。**

`now - t < ttl` 无法表达方向：一份 47 小时前写入、此后未再探测的缓存，与一份 23 小时前
的缓存，在 `now - t` 上都可能落到「刚好差一点」。用户开机（间隔 47h）→ 探测成功 →
关机 3h → 开机，此时距 `probed_at` 已 50h，会**立刻再探一次**。这是正确行为，不是缺陷，
但意味着「一天一次」实际上是「每次跨越 24h 边界都探一次」。

同时 `55s` 是**上界而非典型值**：实测连接被拒 2ms、DNS 失败 21ms。只有「连接握手挂住
直到超时」才接近 55s。所以（b）的串行放大在常见故障下（端点直接拒绝连接）代价很小，
但在「防火墙黑洞 / 上游不响应」时才是真问题——而后者恰恰是最常见的 MCP 故障形态。

为什么失败也要写盘（`note_failure`）：`failed_at` 若不落盘，进程重启后又会立刻重试一次
——重启频繁时同样会放大代价。落盘后重启不会重置退避。

### 4.3.1.1.1 首次迁移是最坏情况，必须单独处理

§4.3.1.1(b) 的串行放大在**升级后的第一次**最严重，因为存量缓存的条目**全都缺
`probed_at`**（本机实测：`{"servers":{"horizon-websearch":{"fingerprint":...,"tools":[...]}}}`
——确实没有该字段），`#[serde(default)]` 读成 `None` ⇒ 全部 `Stale` ⇒ 在**同一轮**里
集体探测。

| server 数 | 单轮卡顿上界（串行 × 55s） |
|---|---|
| 1 | 55s |
| 3 | 165s |
| 5 | 275s |
| 10 | 550s（9 分钟） |

10 个 MCP server 不是极端配置。用户升级后第一次发消息，界面卡 9 分钟且没有任何进度
反馈，会被判定为「升级把软件搞坏了」。这是本设计**引入**的最严重的一次性退化。

必须处理，且不能只依赖 §4.3.1.1(b) 的「每轮最多探 1 个」——那会让首次迁移后要连续
`N` 轮才全部探完，期间每轮卡 55s，用户同样难受。推荐组合：

1. **迁移时把 `probed_at` 填成「现在」而不是 `None`。** 存量条目本来就有工具清单且
   一直在用，把它们一律视为「刚探测过」不会让行为变差；真正的自愈仍会在 24h 后发生。
   这一步把「升级即触发全量探测」变成「升级 24h 后才逐个探测」，配合错峰抖动即可
   完全消失。
   - 代价：一次升级后要等满 24h 才享受到自愈。但用户此前已被这个 bug 困了很久
     （spec 的立身之本就是「永久失效」），多等一天可接受；且真急着要，删缓存重启
     即可（§4.4 文案）。
2. 若坚持「存量条目视为过期」，则必须配合错峰抖动 + 每轮限额，并且**在启动时就把
   `probed_at` 错开写入**（而不是在探测时），否则仍然是同一轮集体到期。

方案 1 与 §3 决策 2 里「缺字段时 `#[serde(default)]` 读成 `None`，视为从未校验，
下一轮重探一次」直接冲突。**以本节为准**：那段描述的是「单个新装 server」的正常路径，
不适用于「所有存量条目同时缺字段」的批量迁移。实现时按方案 1 走，并补一条测试
`a_legacy_entry_without_probed_at_is_migrated_to_now`（区别于
`a_legacy_entry_without_probed_at_is_stale`：前者钉迁移后的行为，后者钉 `lookup` 对
`None` 的原始判定）。

### 4.3.1.2 手动 reload 的用户预期被退避挡住

`reload` 有三个调用方：两个每轮自动检查，外加**手动的 `/mcp reload`（TUI）与
`mcp_reload` 工具（`reload_tool.rs:105`）**。三者共用同一个 `reload()`，因此也会共用
同一个退避窗口。

这会产生一个别扭的用户体验：用户刚看到工具报错，敲 `/mcp reload` 想强制刷新，得到的是
「没有探测」——因为 2 分钟前自动检查刚失败过。他无法区分「真的没问题」与「被退避挡住
了」。

处理方式是在**报告里如实说明**，而不是给手动路径开后门：

- `ReloadReport` 增一个 `skipped_backoff: Vec<String>`（或复用 `ServerOutcome` 加一个
  `skipped: bool` 字段），记录「本轮因退避而没有探测」的 server。
- 手动入口（`/mcp reload`、`mcp_reload` 工具）在输出里明确写出
  「`X` 上次探测失败于 …，跳过重试，最早 … 后再试」。
- 不建议为手动调用传 `force: true` 绕过退避：§1 已经论证「重探风暴」是真实风险
  （网关不区分工具不存在与上游故障），让一次手动点击就能触发全量重探，等于把这个
  风险重新引入。用户若确实要立即重试，删缓存文件并重启仍是最直接的逃生口（§4.4 的
  文案已经这么写）。

这样做的代价是手动入口的行为需要新增一个参数或字段，比纯改缓存判据多一处改动；
但它把「静默什么都不做」变成「明确告诉你为什么没做」，是必要的一步。

### 4.3.2 恢复耗时

```
T0       成功探测，工具名 X 正确
T0+1min  上游把 X 改名成 Y
T0+1min → T0+24h   旧名调用失败（缓存未过期，不重探）
T0+24h   下一轮开头 needs_reload 为真 → 探测 → 拿到 Y，自愈
```

若 T0+24h 那次探测碰上上游抖动，`failed_at` 记录之，5 分钟后重试；端点恢复后最多
再等 5 分钟即自愈。所以恢复耗时的上限是 **TTL + `RETRY_AFTER_SECS`**，而不是无限重试。

若采纳 §4.3.1.1(b) 的「每轮最多探测 1 个」限额，且同一轮有 `N` 个 server 排队，则
恢复耗时上界变为 **TTL + N 轮 + `RETRY_AFTER_SECS`**。`N` 是同时过期的 server 数，
通常等于配置里的 server 总数（首次迁移时所有条目都没有 `probed_at`，会同时过期）。

### 4.3.3 一个会误导排查的窗口：`Fresh` 但 `headers` 轮换

指纹刻意排除了 `headers` 里 `${ENV_VAR}` 的展开值（`cache.rs` 注释写明「轮换 token
不失效缓存」）。这意味着 token 轮换后缓存仍判 `Fresh`，而实际调用会因鉴权失败而报错。
TTL 到期后探测才会发现。

这是**既有行为**，本设计不改变它，但要注意它会让用户误判：报错看起来像「工具改名」
（因为 §4.4 的文案会提示「远端工具清单可能已变化」），实际原因是凭据过期。§4.4 的
文案因此不能断言原因，只能写「可能」。当前写法（`may have changed`）已经足够谨慎，
实现时不要把它改成断言式的 `has changed`。

### 4.3.4 缓存写入不再是「可选的副作用」

这是本设计带来的一个容易被忽略的语义变化。

今天 `cache::save` 只在一个 server **首次探测成功**时被调用（`reload` 里
`cache_dirty` / `load_and_probe` 里同样的标记），失败路径从不写盘。本设计让它成为
**每轮自动检查的产物**：`note_failure` 必须落盘，否则退避在重启后失效（§4.3.1）。

问题在于 `cache::save` **不是原子写**：

```rust
let text = serde_json::to_string_pretty(self)?;
std::fs::write(&path, text)?;          // 截断后直接写，没有 tmp + rename
```

对比 `McpConfig::save`（`config.rs:191`）——它老老实实用了 `tmp + rename`。缓存文件按
`workdir` 定位（`cache.rs:43`），而同一个项目被 TUI、desktop sidecar、app-server
同时打开是完全正常的用法。两个进程都在 `reload` 里写同一份 `mcp-cache.json` 时，
`std::fs::write` 的截断 + 写入不是原子的，可能被读到半截 JSON。

现在这没有后果：`McpCache::load` 解析失败就 `unwrap_or_default()` 静默返回空缓存，
下一次探测重新填上。但本设计让退避依赖这份文件——**文件被写坏 ⇒ 退避信息丢失 ⇒
坏 server 立刻重试**。虽然只是一次额外探测，但这恰好是要防的性能回归路径。

修法很小：把 `cache::save` 改成与 `McpConfig::save` 一致的 `tmp + rename`（顺带
`remove_file` 清理失败残留）。这不是本 spec 的核心改动，但**不做的话 `failed_at` 的
可靠性没有保证**，所以列为实现必做项。

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
| `probed_at` 缺失（存量缓存，单个新装 server） | 视为过期，重探一次并写入时间戳 |
| `probed_at` 缺失（**批量迁移**，全部条目都缺） | 迁移时填成「现在」，不集体重探（§4.3.1.1.1） |
| `probed_at` 在未来（时钟回拨） | 视为过期，重探并覆盖 |
| `failed_at` 在未来（时钟回拨） | 视为**不**在退避窗口内，允许重试一次并覆盖 |
| 探测超时/失败 | 保留旧清单，写 `failed_at`，`error` 进 `ServerOutcome`，退避 5 分钟后重试 |
| 端点长期不可用 | 每 `RETRY_AFTER_SECS` 付一次 55s（上界；连接被拒时实测 2ms），而不是每轮 |
| 端点长期不可用**且已停用** | 不探测（`enabled=false` 时直接沿用缓存条目，见 §4.3.1.1(a)） |
| 多个 server 同时过期 | 串行探测，单轮最坏 `N × 55s`；用错峰或每轮限额压住（§4.3.1.1(b)） |
| 系统时钟不可用（`now` 取不到） | `now` 退化为 `0`，此时 `t <= now` 对任何真实时间戳都不成立 ⇒ `is_fresh` 与 `is_backed_off` 同为 `false` ⇒ 所有条目 `Stale`。为避免「每轮付 55s」，`needs_reload` 在取不到时钟时只做指纹判断、跳过 TTL 触发；不 panic |
| 只读文件系统导致 `save` 失败 | 既有行为：warn，不影响本次内存快照；但退避信息会丢，表现为「每次重启重试一次」——可接受，不要为此 panic |
| `cache.save` 与另一进程并发写 | 改为 `tmp + rename` 后无损坏（§4.3.3）；改动前可能产生半截 JSON，`load` 静默退回空缓存 |

## 6. 测试策略

全部为 `#[ignore]` 之外的常规单测，不需要网络（复用 `TEST_CONNECTOR` seam 与预置缓存）。

| 测试 | 断言 |
|---|---|
| `an_entry_older_than_the_ttl_is_not_fresh` | `probed_at = now - 25h` → `lookup` 返回 `Stale` |
| `an_entry_within_the_ttl_is_fresh` | `probed_at = now - 23h` → `lookup` 返回 `Fresh` |
| `a_legacy_entry_without_probed_at_is_stale` | 手写无 `probed_at` 的 JSON → `lookup` 返回 `Stale` |
| `a_legacy_entry_without_probed_at_is_migrated_to_now` | 载入存量缓存后 `probed_at` 被填成 `now`，`lookup` 返回 `Fresh`（钉住 §4.3.1.1.1 的迁移策略） |
| `migration_of_many_servers_probes_none_of_them` | 10 条存量条目迁移后连续 `needs_reload` 为 `false`（防「升级即卡 9 分钟」） |
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
| `load_and_probe_does_not_probe_an_expired_entry` | 预置 25h 前的缓存 → 启动路径不探测、沿用旧清单（钉住决策 1：启动不校验 TTL） |
| `load_and_probe_records_probed_at_on_success` | 冷缓存探测成功后 `probed_at` 被写入（否则 TTL 永不开始计时） |
| `reload_reprobes_when_the_in_memory_snapshot_has_expired` | 同一进程内先 `load_and_probe` 建立快照，把缓存时间戳改到 25h 前，再 `reload` → 必须重新探测且工具名更新（删掉 `Some(e) if lookup(...).is_settled()` 那个 guard 即变红） |

`reload_refreshes_a_renamed_tool_after_the_ttl` 与上一条都是本 bug 的直接回归测试。
变异验证对象：把 `needs_reload` 里的 TTL 判据删掉 → 第一条变红；把 `reusable` 分支
的 `is_settled()` guard 删掉 → 上表这条变红。两条守护的是不同路径，缺一不可。

同时修正真实端点冒烟测试（`tests/http_transport.rs`）——它已在 `a89b23f6` 中改为按
`__SearchPro` 后缀匹配，本轮只需确认它仍绿。

## 6.5 风险清单（按严重程度排序）

本设计不是「改一个判据」那么小。以下是全部已知风险，**实现必须逐条对照**，任何一条
未处理都会让这次修复引入比原 bug 更糟的问题（原 bug 是「永久失效，但零额外开销」）。

### 高：会让用户可感知卡顿

| # | 风险 | 触发条件 | 处置 |
|---|---|---|---|
| R1 | 存量缓存全部缺 `probed_at`，升级后**同一轮**集体探测 | 任何从旧版本升级的用户 | 迁移时把 `probed_at` 填成「现在」（§4.3.1.1.1） |
| R2 | 串行探测，单轮最坏 `N × 55s` | N 个 server 同时过期（常见：升级、长期关机后） | 错峰抖动 + 每轮限额（§4.3.1.1(b)） |
| R3 | 停用的坏 server 仍被 TTL 唤醒 | 用户停用了不可用端点 | `enabled=false` 时跳过探测（§4.3.1.1(a)） |
| R4 | 端点黑洞时每 5 分钟一次 55s | 防火墙丢包 / 上游不响应 | 退避本身（§4.3.1）；连接被拒场景实测 2ms，不构成问题 |

### 中：会让人误判或行为不符预期

| # | 风险 | 触发条件 | 处置 |
|---|---|---|---|
| R5 | 手动 `/mcp reload` 被退避挡住，静默无事发生 | 用户看到报错后想强制刷新 | 报告里明确写出「因退避跳过」（§4.3.1.2） |
| R6 | headless 不自愈 | `yi-agent run` 单次执行 | 接受，写进非目标；启动路径不校验 TTL（§4.2） |
| R7 | token 轮换后缓存仍 `Fresh`，报错像「工具改名」 | `${ENV_VAR}` 凭据过期 | 既有行为；文案保持 `may have changed` 不断言（§4.3.4） |
| R8 | 24h 边界导致的「立刻再探」 | 跨 24h 的关机/长间隔 | 正确行为，不算缺陷（§4.3.1.1(c)） |

### 低：实现细节，但会让退避不可靠

| # | 风险 | 触发条件 | 处置 |
|---|---|---|---|
| R9 | `cache::save` 非原子，并发写产生半截 JSON | 同项目多进程（TUI + sidecar） | 改 `tmp + rename`（§4.3.3） |
| R10 | `note_failure` 不置 `cache_dirty`，退避不落盘 | 只读盘不写盘的路径 | 与 `put` 同样置位（§4.2） |
| R11 | 抖动塞进 `put` 会让测试不可控 | 采纳 R2 的错峰 | 抖动在调用方计算（§4.1） |
| R12 | `needs_reload` 重复读解析 `mcp.json` | 每轮 | 复用已解析的配置（§4.2） |

### 为什么这份清单比设计本身更重要

原 bug 的性质是「工具永远用旧名，但没有任何额外开销」。上面任何一条风险失控，都会把
它换成「工具偶尔能自愈，但每轮/每次升级要卡几十秒到几分钟」——用户对前者的容忍度
远高于后者（前者像是「远端改了名」，后者像是「软件坏了」）。所以实现阶段的验收标准
不是「bug 修好了」，而是「bug 修好了，且 §7 的变异验证里关于性能的两条都是绿的」。

## 7. 验证判据

```bash
cd yi-agent-rs
cargo test -p yi-agent-mcp              # 期望：全绿，新增 20 例
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

真机验收（两条都要做）：

1. **自愈**：把一个 server 的 `probed_at` 手工改到 25 小时前，启动会话并发一条消息，
   观察日志出现**一次**探测、工具名更新为远端现值。
2. **不卡顿**：准备 3 个 server 的缓存，把它们的 `probed_at` 同时改到 25 小时前，再发
   一条消息，**计时**这一轮的耗时。若超过 10s，说明 R2 未处理（应看到错峰或限额生效）。
3. **升级路径**：用一份不含 `probed_at` 的旧缓存启动，确认**没有任何探测**发生
   （R1 已处理），且 `mcp-cache.json` 里被补上了 `probed_at`。

## 8. 范围外的已知缺陷（记录，不在本 spec 处理）

1. **`-32603`（JSON-RPC 错误）被归入 `CallOutcome::Transport`**，导致无意义的重连
   重试一次，并在文案里显示 `(after reconnect)`。正确做法是区分「协议返回错误」
   与「传输失败」。独立修复。
2. **没有人工强制刷新入口**。用户遇到本类问题时最快的自救仍是删缓存文件，
   本设计只是让 24h 后自动恢复。若后续摩擦明显，加 `mcp_reload refresh`（方案 B）。
3. **stdio server 的清单变化**同样受 24h TTL 约束，但 stdio server 通常是本机安装，
   无「上游改名」场景，优先级低。
