# 看板并发配额改为「全机运行会话数」判定

日期：2026-10-22
状态：待实现

## 1. 问题

看板调度（`BoardService::driver_step`）判断「这一拍能不能起新会话」时，只数**看板自己**
的在跑卡片：

- `board.running_count()`（`superpowers-kanban-core/src/board.rs:224`）只统计 `state == Running`
  的卡片，而 `occupies_slot` 只认 `Running`（`card.rs:53`）。
- `free_slots(limit) = limit - running_count()`（`board.rs:232`）。
- `limit` 来自项目自己的 `superpowers-kanban.toml` → `limit_at(now)`（`service.rs:476`、
  `calendar.rs:115`）。

于是：峰值 4、看板内 1 张卡在跑、看板外还有 3 个会话在跑（桌面里手工开的），看板仍认为
自己还有 3 个空位，会再起 3 个会话——全机实际 7 个。而真正被限流的是**总流量**。

已有的全局 `flock` 租约池（`$HOME/.yi-agent/superpowers-kanban/leases`，`lease.rs:42`）
只解决「多个看板项目之间互不超发」：只有看板调的 `next_launch` 会去抢槽（`service.rs:747`），
手动会话不持租约、也不进任何计数。

### 目标

配额判定改为：**可用 = 全局峰值 P − 当前全机运行会话数**，其中 P 是机器级唯一值。
"运行"= 正在调用模型或正在执行进程；等待人决策（`AwaitingApproval`）不算。

### 非目标

- 不管 TUI / `yi-agent run` / 其它独立进程里的会话。范围明确限定为**桌面 app-server
  这一个宿主进程**管理的会话（用户已确认）。这属于有意的口径收窄，不是遗漏。
- 不改 flock 租约池的机制本身（它继续负责跨看板项目的原子扣减）。
- 不改合并轮（`Merging`）是否占会话槽的既有语义——它本来就不占（`card.rs:53`）。

## 2. 方案

### 2.1 宿主侧：`driver/next` 增加全机事实

`driver_tick`（`yi-agent-app-server/src/server.rs:5689` 一带）已经每拍为每个
(项目, 插件) 采集一次 idle 快照。同一处再算一个全机计数：

```rust
fn running_sessions(threads: &HashMap<String, ThreadSession>) -> u32
```

口径（与 `driver_tick` 现有的 idle 判据同源，`server.rs:5682`）：

- 计入：`active_turn_id.is_some()`，或 `status` 为 `Running`。
- 不计：`status == AwaitingApproval`（等人决策）、`Idle`（含已结束）。

这个数**不区分归属**——手动会话、别插件起的会话、看板会话都算。宿主不认识看板，
它只回答"本进程此刻有几个会话在跑"，这是通用事实。

`threads_payload`（`session_driver.rs:132`）的产物里加一个平级键：

```json
{
  "threads": [ { "thread_id": "...", "idle": true } ],
  "results": [ ... ],
  "sessions": { "running": 3 }
}
```

`contract/driver-protocol.json` 的 `request` 同步加 `sessions`，两侧测试（宿主
`the_driver_contract_request_uses_...`、插件同名测试）各自断言，改一侧不改另一侧就红。

旧宿主不带 `sessions` 时，插件必须退化为现状（见 2.4），不能因此整拍报错。
这与 `driver_project` / `driver_ref` 的既有前向兼容处理同款。

### 2.2 插件侧：判定口径换成「全局余量」

`driver_step` 里（`service.rs:470` 一带）：

```rust
let limit = global_limit();                       // 全局 P，见 2.3
let running = params.sessions.running;            // 宿主下发的全机运行数
let free = limit.saturating_sub(running);         // 全机还能起几个
```

关键点：`free` 一旦已经是"全机余量"，就**不能**再让 `board.free_slots(limit)` 减一次
看板内的 `running_count()`，否则同一批会话被扣两遍。所以：

- `next_launch(limit, now)` 的签名改为 `next_launch(budget, now)`，`budget` 是上面算出的
  全机余量；
- `Board::claim_next_launch` 不再调 `free_slots`，只判 `budget > 0` 与队首是否存在；
- `Board::free_slots` / `Board::running_count` / `Board::start_due` 从生产路径移除。
  `start_due` 只被测试使用，一并删掉或改用新口径，避免留下"另一套并发规则"。

`plan_launches`（`runner.rs:20`）同样换成按 `budget` 取队首 N 张。

flock 租约继续保留：它是**同机多进程**下唯一的原子扣减手段，且能覆盖"两个项目同一拍
各自看到 free=1"的竞争。`acquire_in(dir, budget)` 的 `limit` 参数含义从"峰值"变成
"本次可用额度"——注意槽位文件按 `0..limit` 编号复用（`lease.rs:60`），额度变小时会
复用低编号槽位，语义仍然成立。

### 2.3 全局配置：`~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml`

新增路径，与 `$HOME/.yi-agent/superpowers-kanban/`（状态 + `leases/`）区分：

- 前者是**插件自己的配置目录**（用户指定）；
- 后者是看板状态与租约池。

`layout.rs` 增加：

```rust
pub fn global_settings_path() -> Option<PathBuf>
// $HOME/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml
```

读取顺序：全局配置 → 缺失/损坏回退内置默认（`calendar.rs` 的
`load_preferring_new` 契约不变："绝不 panic，损坏回退默认"）。

**项目级不再覆盖并发字段**。具体做法：

| 字段 | 归属 | 说明 |
|---|---|---|
| `default_max_tasks` | 全局 | 全机峰值 P |
| `window`（时段窗口） | 全局 | 时段化峰值也是全机语义 |
| `interval_secs` | 项目级 | 推进节拍，与配额无关 |
| `archive_grace_hours` | 项目级 | 归档宽限，与配额无关 |
| `instance_wait_secs` | 项目级 | 抢 `plugin.lock` 的等待预算 |

即使项目级文件里还留着 `default_max_tasks` / `window`，也**忽略**（读路径容忍旧文件，
不是报错）。`settings.write` 只写全局文件；写路径继续拒绝越界（`from_settings_json` 不变）。

### 2.4 兼容与退化

宿主不发 `sessions` 时（旧宿主 + 新插件）：退化为现状
（`free = P - 看板内 running_count()`），并打一行 warn 说明配额只覆盖看板内会话。
绝不能因为缺字段就拒绝调度。

### 2.5 时序正确性

`driver_tick` 按 (项目, 插件) **串行**拉拍，且每拍开头重新采集快照
（`server.rs:5679`）。所以 A 项目这一拍刚起的会话，B 项目下一拍就能看见，不会同拍
互相超发。同一拍内只有一个动作（`driver_step` 的既有契约：至多一个 `Launch` 或
`Append`），也不存在拍内连起两个。

## 3. 改动清单

**宿主（`yi-agent-app-server`）**

- `server.rs`：新增 `running_sessions()`，在 `driver_tick` 里算好并传入 `driver_step`。
- `session_driver.rs`：`threads_payload` 或新增参数带上 `sessions.running`；
  `driver_step` 签名加一路输入。
- `contract/driver-protocol.json`：`request.sessions`。
- 测试：计数口径（Running 计、AwaitingApproval 不计、Idle 不计）；契约两侧一致。

**插件（`plugins/superpowers-kanban`）**

- `core/src/layout.rs`：`global_settings_path()`。
- `core/src/calendar.rs`：并发字段只从全局文件读；`save` 写全局路径。
- `core/src/board.rs`：移除 `free_slots` / `running_count` / `start_due` 的并发职责，
  `claim_next_launch(budget)`。
- `runner/src/service.rs`：`driver_step` 读 `sessions.running`，`next_launch(budget)`。
- `runner/src/dispatch.rs`：`driver/next` 解析 `sessions`；`settings.read|write` 走全局文件；
  `board.next_launch`（`:233`）同样改用全机余量，不能留一条绕过新口径的旧通路。
- `runner/src/runner.rs`：`plan_launches` 按 `budget`。
- `INSTALL.md`：登记新配置路径。

**测试（回归锁）**

1. 看板外 3 个会话在跑、P=4、看板内 0 个在跑 → 只起 1 个，第二张仍 `queued`。
2. 看板外 3 个在跑、P=4、看板内已有 1 个在跑 → 一个都不起（这是双重扣减的回归锁：
   若仍走 `free_slots`，会错误地再起 1 个）。
3. `AwaitingApproval` 的会话不计入 `running`。
4. 旧宿主不带 `sessions` → 退化为看板内口径，且照常调度。
5. 项目级配置写 `default_max_tasks = 10` 而全局写 4 → 生效值是 4。

## 4. 风险

- **口径是"本进程可见"**：桌面 app-server 之外的会话（TUI、`yi-agent run`）不计入。
  用户已确认接受。若日后要覆盖，得把租约扩到所有会话启动路径（另案）。
- **`sessions.running` 与插件侧对账回写存在一拍滞后**：会话刚结束的那一拍可能仍算在跑，
  最多让看板晚一拍起新会话。可接受（保守方向，不会超发）。
- **全局配置文件缺失**时用内置默认 3（`DEFAULT_MAX_TASKS`）。用户若期望别的峰值，需显式
  创建该文件；`settings.write` 会创建它。
