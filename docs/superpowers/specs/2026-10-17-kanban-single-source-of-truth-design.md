# 看板真相源收敛与实例让位 Design

日期：2026-10-17
状态：设计已定稿，待用户 review 后进入 writing-plans。

## 1. 背景与问题

看板插件（`plugins/superpowers-kanban`）的三个 CLI 写入动词 `done` / `archive` / `purge`
在当前架构下**不可靠**：写下 `board.json` 后会被常驻 daemon 静默覆盖。

实测（2026-10-17）：把某张已归档卡的 `archived` 由 `true` 改为 `false`（等价于一次
CLI 写入），运行中的 daemon 在下一个 tick 内把它改回 `true`：

```
t=+15s: archived=False
t=+30s: archived=False
t=+45s: archived=True    ← 被 daemon 覆盖
```

根因是**两个真相源**：

1. **daemon 内存快照**。`BoardService::new`（`service.rs:111`）在**启动时**把
   `board.json` 载入 `Inner.board` 一次，此后运行期**没有任何 reload**（全插件 grep
   无 `reload`/`mtime`/`modified`）。
2. **磁盘文件**。CLI 的 `done`/`archive`/`purge`（`main.rs:528/572/602`）与
   `merge-request`/`merge-finish`（经 `dispatch::dispatch` 的进程内 `service_for`）
   都是"load → 改 → save"的一次性进程写法。

而 daemon 每 tick **无条件**用内存快照全量覆盖磁盘：`run_daemon` 循环每 tick 调
`consume_inbox`（`service.rs:382`），其结尾无条件 `self.save(&inner)`
（`service.rs:385`），而 `save`（`service.rs:137`）序列化整个 `Inner.board` 覆盖写。
于是外部写入必被覆盖。

影响面比三个动词更广：**凡是 CLI 写盘的通路都会被覆盖**。`plugin.lock` 单实例锁也
帮不上——它只被 `run_daemon` 取，一次性子命令不取，因此没有"daemon 在跑就别写盘"的
保护。socket（`dispatch.rs` 的方法表）也**没有** done/archive/purge，CLI 无处可绕。

### 1.1 第二个问题：拿不到锁的实例无限空转

`run_daemon` 取不到 `plugin.lock` 就 `loop { sleep(500ms) }` **无限等待**
（`main.rs:768-777`），既不推进也不退出（只有当它探不到宿主 daemon 时才自杀）。

监督器（`yi-agent-supervisors`）的 `ensure_running` **只跟踪自己 spawn 的 child**，
spawn 前**不探测**是否已有实例在应答；`restart_backoff_max_ms` 定义了却从未使用
（只读固定 `restart_backoff_ms`）。于是：健康孤儿持锁 → 监督器不认它 → 每次对账都
spawn 一个新实例 → 新实例拿不到锁 → 空转。

实测（2026-10-17）：同项目上有两个 daemon（`9542` 持锁、`3137` 空转）。杀掉空转的
那个后，`boards watch` 在 **3 秒内**又拉起一个新实例（`43799`），照样拿不到锁继续
空转。这是一个结构性循环，手工杀治标不了。

### 1.2 第三个问题：陈旧 socket 文件无人清理

项目路径过长时，查询 socket 按规则回退到 `$TMPDIR/plugin-<hash>.sock`
（`ipc/src/socket.rs` 的 `plugin_socket_for`）。而 `serve_with` 只在**自己 bind 的
那一刻**清理目标路径（`ipc/src/server.rs`）。

当前实例 bind 在回退落点上，于是清单声明的直连路径
`.yi-agent/superpowers-kanban/superpowers-kanban.sock`（mtime 2026-10-08）与
`$TMPDIR` 里另外 4 个（2026-10-02~03）死文件永远不会被清。排查时极易拨错地址
（本次排障就拨错过：拨直连路径得到 `ECONNREFUSED`，而真实实例在回退路径上应答）。

## 2. 目标与非目标

**目标**

- **真相源唯一**：`board.json` 是看板的唯一真相；所有读-改-写在同一把文件锁内完成；
  没有任何进程跨 tick 缓存看板。
- **CLI 与 daemon 同一条代码路径**：CLI 写完后 daemon 下个 tick 读到新数据；daemon
  在跑时 CLI 也不会被覆盖；daemon 缺席时 CLI 照常工作。
- **实例让位有界**：拿不到 `plugin.lock` 的实例在有界时间内退出；监督器不再为已在
  应答的实例重复 spawn；反复失败时按指数退避重启。
- **陈旧 socket 可清理**：由本插件派生的确定路径上的死 socket 会被清掉，同时绝不
  误删活实例的 socket。

**非目标**

- 不引入"持锁者卡死时主动接管"（那是另一个 live-ness 议题，见 §5）。
- 不改 socket 协议，不改 `board.*` 方法名（宿主 `card_scheduler` 契约不动）。
- 不改 lease 机制：内存 lease（flock fd）**本就该是进程态**，留在 `Inner`。
- 不动宿主（runtime）socket 的同类死文件（`yi-agent-store` 的职责，另一 crate）。
- 不扫描/不清理 `$TMPDIR` 里其它 `plugin-*.sock`（无法证明归属）。

## 3. 术语与既有事实

- **真相源**：看板数据的权威副本。本设计规定为 `board.json`。
- **进程态**：只能活在某个进程生命周期内的状态。槽位租约（flock fd）即此类。
- `plugin.lock`（`single_instance.rs`）：`flock` 独占锁，语义是"谁是推进者"，
  只有 `run` 取。
- `board.lock`（本设计新增）：`flock` 独占锁，语义是"谁在读改写 board.json"，
  任何写者都取。
- `save_board`（`persist.rs:33`）：原子写（同目录临时文件 + `rename`）。
- `load_board`（`persist.rs:25`）：文件缺失或损坏一律回退为空队列，绝不 panic。

## 4. 设计

### 4.1 core 与 runner：单一写锁 + 事务助手

新增 `<state_dir>/board.lock`，与 `plugin.lock` 同为内核 `flock`（进程退出含
`SIGKILL` 自动释放；不在 drop 时 unlink，理由同 `single_instance.rs` 的注释：删除
文件会让另一个进程在旧 inode 上持锁、在新 inode 上再取到锁）。

新增事务助手，作为**唯一**的数据访问入口：

```rust
/// 锁内 load → 改 → save。任何对 board.json 的读-改-写都必须经此。
///
/// 成功后落盘；闭包返回 Err 时不落盘（失败不留半写状态）。
/// `f` 拿到的 `Board` 在锁保护下，锁内的读-改-写相对其它进程是原子的。
pub fn with_board<T>(
    state_dir: &Path,
    f: impl FnOnce(&mut Board) -> Result<T, String>,
) -> Result<T, String>;
```

`BoardService` 去掉缓存：`Inner` 删除 `board: Board` 字段，只保留
`leases: HashMap<CardId, Lease>` 与 `notified_missing_branch: HashSet<CardId>`。
每个动作改为在需要时经 `with_board` 读-改-写：

| 动作 | 现状 | 改为 |
|---|---|---|
| `consume_inbox` | 改内存 + 无条件 `save` | `with_board` 内消费 inbox |
| `next_launch` | 改内存 + `save` | `with_board` 内认领（lease 仍在锁内操作） |
| `mark_running` / `mark_terminal` / `release` | 改内存 + `save` | `with_board` |
| `merge_request` / `merge_finish` | 改内存 + `save` | `with_board` |
| `merge_next` | 改内存 + `save` | `with_board` |
| `archive_due` | 改内存 + 条件 `save` | `with_board` |
| `list` | 读内存 | 锁内 `load_board` 一次（只读） |

CLI 的 `done` / `archive` / `purge`（以及 `merge-request` / `merge-finish`）改用
同一个 `with_board`：CLI 与 daemon 因此共享同一条代码路径。

**关于 `Inner` 的两把锁**：`BoardService` 现有的 `Mutex<Inner>` 继续保护进程态
（lease map、提示记忆）；`board.lock` 保护磁盘数据。二者获取顺序固定为
"先 `Mutex<Inner>`，后 `board.lock`"（或干脆不在持 `Mutex` 时取 `board.lock`：
lease 的 `acquire` 与 board 的读-改-写拆成两步，先取 board 数据、再定租约），
避免锁序反转——仓库已有过 "resident lease 释放与 resource coordinator 锁序反转"
导致的死锁记录（见 `docs/bug-list.md`）。

### 4.2 runner：有界让位

`run_daemon` 的取锁循环（`main.rs:768-777`）由无限等待改为**有界等待**：

- 默认上限 10s（可经日历配置覆盖，见 §4.4）。
- 期间每 500ms 重试 `acquire`。
- 超时即打印一行明确原因（"another instance holds the lock at …; exiting"）并
  以非零码退出。

于是"拿不到锁的实例"最多存活 10s，不再无限空转。

### 4.3 监督器：幂等 spawn + 指数退避

- `Supervisor::ensure_running`（`supervisor.rs:154`）在 spawn 前先探测该清单声明的
  `query_socket`：有实例应答 → 跳过 spawn（幂等，与宿主
  `yi-agent-boards::lifecycle::launch_if_absent` 的 `is_running` 早退同款）。
- 启用 `restart_backoff_max_ms`：反复失败时退避 `restart_backoff_ms` → ×2 → … →
  `restart_backoff_max_ms` 封顶（当前只读固定初值、上限字段闲置）。

三条合起来的稳态：健康实例应答 → 不 spawn（无等待者）；探测窗口内的短暂竞争 →
最多一个有界等待者，10s 内退出；真崩溃 → 退避重启，不密集刷。

### 4.4 配置项

新增项目级日历配置 `instance_wait_secs`（默认 10），落在既有的
`superpowers-kanban.toml`（`ConcurrencyCalendar`）。语义：`run` 取不到 `plugin.lock`
时的有界等待上限。`0` 表示不等待、立即让位。

### 4.5 陈旧 socket 清理

daemon 启动时（**bind 之前**）做一次清扫，规则：

1. 算出两个候选落点：
   - 直连路径：`<state_dir>/superpowers-kanban.sock`
   - 回退路径：`plugin_socket_for(直连路径)` 的返回（`$TMPDIR/plugin-<hash>.sock`）
2. 对每个**已存在**的候选：尝试连接。
   - 连得上 → 有活实例，**绝不删**。
   - 连不上（`ECONNREFUSED` 等）→ 死文件，`remove_file`。
3. 只碰这两个由本插件派生的确定路径，不扫 `$TMPDIR` 其它内容。

**为什么用连接探测而不是 mtime**：只有"连不上"才是"绑定进程已不在"的可靠证据
（Unix socket 的语义）。mtime 会被无关操作扰动，存在性更不可信。

**并发安全**：清理与 bind 都在同一进程内串行（`plugin.lock` 保证同一时刻只有一个
推进者），且"连得上就不删"这条规则保证即便有活实例也不会被误删。

## 5. 边界与取舍

| 场景 | 行为 |
|---|---|
| daemon 在跑，CLI 执行 `done` | `with_board` 锁内改盘；daemon 下个 tick 读到新数据 |
| daemon 缺席，CLI 执行 `done` | 同一条路径，照常工作 |
| CLI 与 daemon 同时写 | `board.lock` 串行化，后到者读到先到者的结果 |
| 拿不到锁的实例 | 有界等待（默认 10s）后退出，不再无限空转 |
| 监督器对账时已有健康实例 | 探测 `query_socket` 有应答 → 不 spawn |
| 反复启动失败 | 指数退避至 `restart_backoff_max_ms` 封顶 |
| 直连 socket 路径上有死文件 | 启动时连接探测→连不上→清掉 |
| 死文件路径上有活实例 | 连得上 → 保留，绝不误删 |
| 持锁者卡死（不健康） | **本设计不处理**；稳态最多约 10s/退避间隔的空转进程，有界不增长 |
| 宿主 runtime socket 的死文件 | **不处理**（另一 crate 的职责） |

## 6. 验收

1. **覆盖消除**：以 `with_board` 写入后，跑一个 tick，值不被回退（实测口径：写后
   跨 ≥2 个 tick 复读一致）。
2. **CLI 在 daemon 在跑时可用**：daemon 运行中执行 `done` 再 `list`，状态为 `done`。
3. **CLI 在 daemon 缺席时可用**：停掉 daemon 后 `done` 仍生效。
4. **并发不丢更新**：两个写者并发执行各自的 `with_board`，两者结果都在最终文件里。
5. **有界让位**：预置一个持锁进程，新实例在 ≤ `instance_wait_secs` + 余量内退出，
   且打印明确原因。
6. **幂等 spawn**：已有实例在 `query_socket` 上应答时，监督器不再 spawn 第二个。
7. **指数退避**：连续失败时重启间隔递增，至 `restart_backoff_max_ms` 封顶。
8. **stale socket 清理**：预置一个连不上的死 socket 文件，daemon 启动后被清掉；
   预置一个活 socket，保持不动。
9. **零回归**：`plugins/superpowers-kanban` 全量测试通过；宿主
   `card_scheduler` 契约（`board.*` 方法名与返回形状）不变。

**验证命令：**

```
cd plugins/superpowers-kanban && cargo test --offline
cd yi-agent-rs && cargo test --offline -p yi-agent-supervisors -p yi-agent-boards
```

## 7. 涉及文件

- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/persist.rs`
  （新增 `board.lock` 取锁与 `with_board` 助手）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
  （`Inner` 去 `board` 字段；各动作改经 `with_board`；`list` 改为只读载入）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
  （`run_daemon` 有界等待；`command_done`/`command_archive`/`command_purge`/
  `command_merge_*` 改用 `with_board`；启动时 stale socket 清扫）
- `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
  （`board.*` 各分支经 `with_board`，方法名不变）
- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`
  （新增 `instance_wait_secs`，默认 10）
- `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`
  （spawn 前探测 `query_socket`；指数退避启用 `restart_backoff_max_ms`）
- `plugins/superpowers-kanban/README.md`（登记真相源与让位行为）

## 8. 风险

| 风险 | 处置 |
|---|---|
| 每 tick 读一次 board.json 的开销 | 文件约 15KB，每 tick 一次，相对既有 git 调用可忽略 |
| `board.lock` 与 `Mutex<Inner>` 锁序反转 | 固定锁序（先 `Mutex` 后 `board.lock`），或二者不嵌套持有；补锁序测试 |
| 有界让位 + 监督器补位 → 变相重启风暴 | 退避封顶 + 探测幂等，两者同时落地（验收 6/7） |
| 清理误删活 socket | 只清"连不上"的确定路径；连得上绝不删（验收 8） |
| 去掉内存缓存后 `next_launch` 的租约原子性 | lease 的操作与 board 的读-改-写分两步，先定租约再落 board；补并发测试（验收 4） |
| 老 `board.json` 兼容 | 不改数据格式，仅改访问方式，无损 |

**遗留（另立条目）**：持锁者卡死时无接管机制；宿主 runtime socket 的陈旧文件清理。
