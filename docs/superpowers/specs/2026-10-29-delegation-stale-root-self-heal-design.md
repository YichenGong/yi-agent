# 委派工具陈旧 root 自愈 设计

**日期**: 2026-10-29
**状态**: 待实现
**相关代码**: `yi-agent-subagent`（`binding.rs`、`thread_root.rs`、`lib.rs`、`attach.rs`）

## 1. 背景

### 1.1 现象

桌面端 / TUI 的委派（`spawn_agent` / `send_message` / `wait_agent` /
`inspect_agent` / `cancel_agent`）存在两个彼此独立的失效形态，用户看到的是同一句话：
**「这个项目的委派坏了，重启才行」**。

### 1.2 失效形态 A：daemon 死掉，工具不修

`yi-agent-subagent` 的模块文档、`binding.rs` 的注释、以及
`yi-agent-app-server/src/server.rs:1259` 的注释都声称"委派工具持
`Arc<RuntimeBinding>`，每次调用解析当届 socket，连接失效时 repair 并重试一次"。
**这句话对工具路径不成立。**

实测（2026-10-29，隔离 TempDir，daemon 被 `Stop` 后退出，binding 未被告知）：

```
TOOL RESULT: error: daemon is unavailable: No such file or directory (os error 2)
GENERATION AFTER: 0 (before 0)          # 没有发生 repair
PROBE AFTER: Dead
BINDING.SEND HEALED: true               # 同一个 binding 走 send 就能修
GENERATION END: 1
```

根因是三处叠加：

1. **工具没走自愈通道。** 七个 `call()` 实现全部是
   `self.root.handle()` + 裸 `send_request`：
   `lib.rs:1464`（`DaemonApplicationSendMessageTool`）、
   `lib.rs:1524`（`DaemonSendMessageTool`）、
   `lib.rs:2120`（`DaemonApplicationSpawnAgentTool`）、
   `lib.rs:2186`（`DaemonInspectAgentTool`）、
   `lib.rs:2265`（`DaemonCancelAgentTool`）、
   `lib.rs:2316`（`DaemonWaitAgentTool`）、
   `lib.rs:2383`（`DaemonSpawnAgentTool`）。
   唯一带 repair 重试的 `ThreadRoot::send_as_root`（`thread_root.rs:217`）
   **只有 `activate` 在用**（`thread_root.rs:235`）。

2. **`handle()` 内部的重试是空转。** `ThreadRoot::handle()`（`thread_root.rs:176`）
   先 `attach()` 再 `current_with_generation()`；`attach()` 在缓存有效时直接
   `return Ok(())`（`thread_root.rs:128`）。所以"daemon 刚死、缓存仍有效"这个正是
   需要修复的时刻，`attach()` 恰恰短路。

3. **`current_or_repair` 是死代码，且即使被调用也不会修。**
   `RuntimeBinding::current()`（`binding.rs:125`）签名返回 `Result` 但永远返回
   `Ok`，于是 `current_or_repair`（`binding.rs:262`）的 `Err(_) => self.repair()`
   分支不可达。全仓库除 `binding.rs` 自身注释外**没有任何调用点**。
   commit `1e709e9b` 的 message 已指出这一点，但只把 `attach` / `activate`
   换了通道，七个工具没跟着换。

### 1.3 失效形态 B：daemon 被替换，root 永久失效

`ThreadRoot` 只在**本进程** `repair()` 推进 generation 时才丢弃缓存 root
（`thread_root.rs:53` 的 `cached_root_is_live`）。当 daemon 是被**别的持有者**换掉的
（另一个 yi-agent 实例、app-server 回收空闲项目后重建、TUI 退出后 app-server 接管），
socket 始终活着，于是：

- 不会出现 liveness 错误 → `binding.send` 的 repair 分支永不触发；
- generation 不动 → `cached_root_is_live` 恒真 → 缓存 root 一直被发出。

实测（本进程 attach → `Stop` → 由"同伴"重新 attach 出新 root）：

```
ORIGINAL session=01289b97-…  task=1c7537f0-…
FRESH    session=368bf9e1-…  task=7d8acd79-…
STALE send_message   -> Ok(Error { code: NotFound, message: None })
STALE spawn_agent    -> Ok(Error { code: NotFound, message: None })
STALE wait_agent     -> Ok(Error { code: NotFound, message: None })
STALE detach         -> Ok(Error { code: NotFound, message: None })
STALE begin_fork     -> Ok(ForkUploadStarted { … })     # 注意：这条没被拦
```

四个委派路径稳定返回 `IpcErrorCode::NotFound`，且**不是**传输层错误，
所以 `is_liveness_error != true`，任何既有自愈都不会介入。
父 agent 拿到的是一句裸 `daemon rejected message: not_found`，
之后每次调用都重复失败，直到重启进程。

**注意 `begin_fork` 未被拦**：`authorize_child_access`（`runtime.rs:2804`）先试
application root 授权失败后回退 `supervisor(session)?`，而新 daemon 确实有这个
session 的 supervisor（`AttachApplicationRoot` 新建了它），于是放行、并分配了一个
**永远不会有 spawn 消费的 fork token**。这是同一根因的另一个表现：
**授权判据是"这个 session 存在"，而不是"这个 root 属于我"。**

## 2. 目标与非目标

### 2.1 目标

1. **所有委派工具在运行时失效时自愈一次。** 覆盖两类信号：
   传输层（`is_liveness_error`）与身份层（`NotFound` / `AuthorityDenied`）。
2. **陈旧 root 可被识别并重挂。** 重挂后重试一次，用户看到的是成功，
   而不是"这个项目坏了"。
3. **不引入写操作二次副作用。** 重试只在"可以证明请求没有被 daemon 接受"时发生。
4. **修好或删除死代码。** `current_or_repair` 不得保留"看起来在自愈、实际不会"的假象。
5. `detach` 不再触发 repair（当前它会重建 daemon，见 §4.4）。

### 2.2 非目标（YAGNI）

- **不改 wire 契约。** 不加 `IpcRequest` / `IpcResponse` / `IpcErrorCode` 变体，
  不动 `PROTOCOL_VERSION`（当前 2）。`NotFound` / `AuthorityDenied` 已经存在且语义准确。
- **不改 daemon 侧授权逻辑。** §1.3 末尾那个 `begin_fork` 放行是 daemon 的判据问题，
  属于独立缺陷，本次只在 spec 里记录（见 §7），不在本设计内修。
- **不碰 `wait_agent` 的 120 秒阻塞。** `TUI_WAIT_AGENT_TIMEOUT_MS = 120_000`
  （`lib.rs:46`）在 tokio 运行时上同步阻塞，是独立问题（见 §7）。
- **不做 daemon 侧主动心跳/代际协商。** 不值得为这一个信号加协议工作量。
- **不重放非幂等写操作。** 见 §3.2。

## 3. 方案

### 3.1 核心：在 `ThreadRoot` 收口一条"自愈通道"

给 `ThreadRoot` 增加一个唯一入口，替换七个工具里的
`handle()` + 裸 `send_request`：

```rust
/// 以本会话 root 的身份发送一条请求；运行时失效时自愈一次并按 `policy`
/// 决定是否重发。
///
/// 与 `handle()` 的区别：`handle()` 只解析句柄，失效由调用方各自处理（实际
/// 结果是没人处理）。这里是唯一入口，把"句柄解析 + 失效判定 + repair +
/// 重挂 + 单次重试"封在一起，七个委派工具不再各写一遍。
fn send_as_root<F>(&self, policy: RetryPolicy, build: F) -> Result<IpcResponse, String>

/// 一条请求能否在身份层失败后被重发。
pub(crate) enum RetryPolicy { Idempotent, SingleShot }
```

`send_as_root` 已存在（`thread_root.rs:217`），本次把它提升为 `pub(crate)`
（`RetryPolicy` 与提示文案 `stale_root_hint()` 同为模块级 `pub(crate)`），并把
失败分类补全。

**关键实现约束：传输层分支必须先 `repair()` 再 `handle()`。** `handle()` →
`attach()` 在 `live_cached_root(generation).is_some()` 时直接短路返回 `Ok(())`
（`thread_root.rs:128`）。daemon 刚死、缓存仍属于当届 generation 时，缓存
"看起来"有效，`attach()` 短路，重发会再次打到死 socket——**这正是形态 A 要被修掉的
行为**。只有先 `repair()`（它推进 generation，使缓存失效），随后的 `handle()`
才会真正重挂。

### 3.2 失败三分类与重试许可

| 类别 | 判据 | 动作 | 允许重试 |
|---|---|---|---|
| 传输层 | `is_liveness_error(&IpcError)`（`binding.rs:340`） | `binding.repair()` → 重挂 root | 是（现状已有） |
| 身份层 | `Ok(IpcResponse::Error { code: NotFound \| AuthorityDenied, .. })` | 丢弃缓存 root → `binding.repair()`（`probe_runtime` 判 Dead/Wedged 才换 daemon，Healthy 只重挂）→ 重试 | **仅限幂等请求** |
| 业务层 | 其余一切 `Error`（`Validation`、`RateLimited`、`InvalidState`、带 `coding_requires_git:` 前缀的拒绝等） | 原样返回 | 否 |

**为什么身份层要区分幂等性。** 身份层重试意味着"再发一次同样的请求"。若第一次
其实已被 daemon 接受、只是应答丢失，重放就会产生两个子任务或两条消息。因此：

- **可安全重试**（请求本身幂等，或第一次必然未被接受）：
  `WaitAgent`、`InspectAgent`、`CancelAgent`、`ActivateApplicationRoot`、
  `AttachApplicationRoot`、`DetachApplicationRoot`。前三个是只读/幂等目标操作，
  后三个在 daemon 侧按 `(session, root, capability)` 或 `idempotency_key` 幂等
  （`attach_application_root` 用 `application_root_key` 稳定键，见
  `thread_root.rs:17`）。
- **不重试**：`SendApplicationMessage`、`SendMessage`、`SpawnApplicationChild`、
  `BeginForkUpload` / `AppendForkChunk`。这三类会改变 daemon 状态，重放有真实副作用。

对不重试的路径，本次的收益是**把错误说清楚**：捕获身份层错误后丢弃缓存 root，
返回一句可操作的说明（"该会话的 root 已被替换，已重新挂载；请重试本次调用"），
下一次调用就正常。这比现状（永久失败、无提示）是实质性改善，且零副作用风险。

### 3.3 重挂上限：每次调用 1 次，不跨调用记忆

- 每次工具调用最多触发 **1 次** repair + 重挂 + 重试（最坏 2 次请求）。
- **不**在 binding 上记忆"已经重挂过"，因为用户可能刚合并、刚恢复会话、
  或另一个进程刚退出——下一次调用应当重新获得自愈机会。
- 防死循环由**结构**保证，不靠 generation 断言：`send_as_root` 的重发点是单条的、
  不在循环里，也不递归调用自己；第二次仍失败时直接返回，不会第三次。
  （不用 generation 断言的理由：repair 在 socket 健康时不推进 generation，
  "generation 未变"因此既可能意味着"无须重挂"、也可能意味着"重挂已发生但没换
  daemon"，用它做判据会误伤第二种情况。）

### 3.4 修掉 `current_or_repair`

两条路，取前者：

- 让 `current()` 真的能失败（例如 managed binding 在 runtime 为 `None` 时返回 `Err`），
  从而 `current_or_repair` 可达；**或**
- 删除 `current_or_repair`，并把 `binding.rs:66`、`:458` 里提到它的注释改掉。

选**删除**：`current()` 返回 `Result` 的签名保留（fixed binding 也永远成功，
语义是"句柄恒存在"），但把它标注为"只解析、不自愈"。

自愈的**唯一**出口是 `ThreadRoot::send_as_root`；`RuntimeBinding::send_with_generation`
继续只处理传输层（它的既有职责，`activate` / `attach` 走它）。身份层判定放在
`ThreadRoot` 而非 binding，是因为"这个应答说明**我的 root** 过期了"是会话侧知识：
binding 是项目共享的，它不知道哪个 root 属于哪次调用。这样不会留下第二条
"看起来能修"的入口。

### 3.5 `detach` 与 repair 脱钩

`ThreadRoot::detach()`（`thread_root.rs:251`）当前第一步是 `self.handle()?`，
而 `handle()` → `attach()` → `binding.send` 会 repair。实测后果：

```
PROBE3 DAEMON REVIVED BY DETACH: true
PROBE3 GENERATION: 2
```

即"删除一个会话"会冷启动一个 daemon，并且幂等 attach 会把本该 detach 的 root
在新 daemon 上**重新创建**，然后才发 detach。这直接抵消 `evict_idle_runtimes`
的回收效果（`docs/bug-list.md` 第 15 条修的正是"监督线程只增不减"）。

修法：`detach()` 只用**当届句柄**，不 attach、不 repair：

```rust
pub fn detach(&self) {
    // 只对"当前认为存在的" root 发 detach。缓存已失效或 runtime 已退休时，
    // daemon 侧要么不存在这个 root，要么已经整体消失——两种情况都没有东西
    // 可 detach，而重建一个 daemon 只为删掉刚建的 root 是纯浪费。
    let Some(root) = self.live_cached_root(self.binding.generation()) else {
        return;
    };
    // ... send_request 直接用 binding.current() 的 socket，失败只 warn
}
```

`binding.detach()`（`binding.rs:319`）已经是这个形状（用 `current()`、失败只
warn），保持不变。

## 4. 具体改动清单

| # | 位置 | 改动 |
|---|---|---|
| 1 | `binding.rs:340` 附近 | 新增 `pub fn is_stale_root_response(response: &IpcResponse) -> bool`，判 `Error { code: NotFound \| AuthorityDenied }` |
| 2 | `thread_root.rs:217` | `send_as_root` 提升 `pub(crate)`；补身份层判定；参数加 `RetryPolicy`（`Idempotent` / `SingleShot`） |
| 3 | `thread_root.rs:251` | `detach()` 改为不 attach、不 repair（§3.5） |
| 4 | `binding.rs:262` | 删除 `current_or_repair`；同步改 `binding.rs:66`、`:458` 的注释 |
| 5 | `lib.rs:1464/1524/2120/2186/2265/2316/2383` | 七个工具改用 `root.send_as_root(...)`，按 §3.2 表声明重试许可 |
| 6 | `thread_root.rs` 模块级 | 新增 `stale_root_hint()`（"已重挂、未重发、请再试一次"）与 `RetryPolicy`；`spawn_outcome`（`lib.rs:1950`）识别该提示并原样透出，不包 `daemon is unavailable` |
| 7 | `docs/project-management/yi-agent-subagent.md` | 修正"每次调用经 `current_or_repair`"这句失真描述 |

工具改造后 `handle()` 仍保留（`spawn_agent` 需要先拿 socket 做 fork 上传），
但所有会碰 wire 的调用都经 `send_as_root`。

`spawn_outcome` 的第一个参数从 `Result<IpcResponse, IpcError>` 改为
`Result<IpcResponse, String>`（`send_as_root` 的错误类型），两个调用点
（`lib.rs:2144`、`:2404`）同步改。

## 5. 内存/binding 类型不变性

- `fixed` binding（TUI、daemon 自己的 worker）行为不变：无 repair plan，
  `send_as_root` 遇到身份层错误时 `repair()` 返回 `Err("this runtime binding
  cannot be repaired")`，此时**不得**把内部错误抛给模型，而是用原始业务错误返回
  （`Ok(response)`）。传输层同理：repair 失败时返回**原始传输错误**，
  不要拼接内部措辞。
- `managed` binding 沿用 `repair()` 的既有单飞（`flight` mutex）与 generation 推进。

## 6. 测试策略

三个失败场景必须转成**可复现的正式测试**（不是临时探针）。断言只钉**由修复
独占决定**的事实，不钉会被无关校验规则带偏的细节（这是第一版计划的硬伤）：

1. `a_tool_repairs_a_daemon_that_died` —— 形态 A。`Stop` 后调用真实注册的
   `send_message` 工具。断言：文本**不含** `daemon is unavailable`、`generation`
   推进、`probe_runtime == Healthy`、之后用该 root 发只读请求得到 `TaskSummaries`。
   **不**断言工具调用成功：`send_message` 是 `SingleShot`，第一次请求因 liveness
   作废，重发那一次的 recipient 是随机 UUID，业务结论必然是拒绝。
2. `a_replaced_daemon_invalidates_the_cached_root` —— 形态 B。`Stop` → 同伴
   `attach_project_runtime` 起新 daemon → 调用 `inspect_agent`（幂等路径）。
   断言：调用后 `root_task_id()` 与调用前**不同**，且新 root 可用。
   **不**断言 `!text.contains("not_found")`：随机 UUID 即使重挂成功也会返回
   `not_found`（该 task 确实不存在），与"陈旧 root"的错误码无法区分；想用真实
   child 造判据也不成立（陈旧 root 造不出 child，同伴造的 child 归同伴管）。
   另注意**不可**用调用前的 `root.attach()` 刷新缓存——那会让形态 B 的前提消失。
3. `a_non_idempotent_call_reports_the_stale_root_instead_of_replaying` ——
   **必须用形态 B**：形态 A 下请求未送达、重发是必要的，重放与否在 daemon 侧
   不留差异，测不出来。在形态 B 下第一次请求已送达并被旧身份拒绝，若重放，
   新 runtime 会真的建出子任务。断言：`is_error`、文本含 `Retry this call once`、
   调用后该 root 名下子任务数为 **0**（用 `ListTaskSummaries` 按
   `parent_task_id` 计数；**不用** `WaitAgent`——它只反映 supervisor 里活着的
   子任务，terminated 的不在其中，数出来恒为 0，会让断言变成空断言）。
4. `detaching_a_retired_root_does_not_start_a_daemon` —— `retire()` 后
   `root.detach()`，断言 `probe_runtime != Healthy`、`generation` 不因 detach 推进。

反向验证（每条都必须做）：把对应修复逐条回退，确认该测试变红。

既有基线：`cargo test -p yi-agent-subagent`（lib 84 + e2e 9），
`cargo test -p yi-agent-app-server --lib`。新增用例只增不改既有断言。

## 7. 记录但不修的相邻缺陷

1. **`begin_fork` 对陈旧 root 放行**（§1.3）。`authorize_child_access`
   （`runtime.rs:2804`）在 application root 授权失败后回退 supervisor 判定，
   而替换后的 daemon 有新 session 的 supervisor，于是分配一个永远不被消费的
   fork token（存储侧无 TTL 淘汰）。修法是让 attach 的 root 与 session 绑定身份，
   而不是"session 存在即通过"。独立设计。
2. **`wait_agent` 120 秒阻塞 tokio executor**（`lib.rs:46`、`lib.rs:2322`）。
   `Tool::call` 是 async fn，内部全同步 `send_request`，`agent.rs:1569` 直接
   `tokio::select!` await，没有 `spawn_blocking`。独立设计。
3. **linked worktree 的 `git-common-dir` 被整个交给 shell 沙箱可写**
   （`lib.rs:1280`）。实测 linked worktree 的 `--git-common-dir` 指向主仓库
   `.git`，等于向 coding 子 agent 开放主仓库 objects/refs。
4. **wedged 退休后立即重启的竞态**（`attach.rs:450` + `ipc.rs` 的
   `LOCK_NB`）。`retire_if_wedged` 返回 true 后立刻 `start_with_factory`，
   旧 daemon 未释放 flock 时得到 `AlreadyRunning`，随后 attach 到僵死 daemon。

## 8. 验收判据

- `cargo test -p yi-agent-subagent`（lib + e2e，含 4 个新用例）全绿。
- `cargo test -p yi-agent-app-server --lib a_dead_runtime_is_repaired` 仍绿。
- 四个新用例逐条回退验证（§6）。
- `cargo fmt --all` 通过。
- `docs/project-management/yi-agent-subagent.md` 与 `docs/bug-list.md` 同步更新。
