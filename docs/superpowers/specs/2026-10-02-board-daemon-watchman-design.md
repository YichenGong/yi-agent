# 看板 daemon 的持续值守（watchman）— Design

状态：待实现（本 spec 已通过人类评审）。
上游决策：本文回答 spec §7 已知缺口 2「daemon 崩溃无人重启」。

## 1. 问题

看板插件由 `daemon serve` 进程监督重启（`yi-agent-supervisors`），但**没有任何东西监督
daemon 本身**。于是：

1. `daemon serve` 一崩（半夜崩溃、被 OOM 杀掉、误杀），该项目的 daemon 不再回来，
   插件的监督者也没了。重启桌面 app 也没用——`board/list` 只读登记表，从不拉起 daemon。
2. 崩溃时插件会变成**孤儿进程**继续空转（插件无 daemon 存活探测，监督器也未给插件
   `setsid`，单个 pid 被 `kill -9` 不会连带杀子进程）。
3. 当新 daemon 起来，它会再拉一个插件 —— 于是**两个插件同时跑同一块看板，同一张卡被
   重复启动**。这对「限流下单一队列推进」的目的是致命的。

目标：让排队中的看板任务能在无人盯着的情况下**持续自动推进**，包括 app 关闭期间与
系统重启之后；且任何时刻同一项目**只有一个插件在推进队列**。

## 2. 设计原则

- **watchman 只关心 daemon，不关心看板。** 它读一个**通用**的「常驻 daemon 登记」，
  不知道「看板」是什么。看板是外部插件，它只是把自己的需要写进那份通用登记。
- **一份保证逻辑，两个宿主。** `ensure_daemons(projects)`（探测 `is_running`，不在就
  `launch_if_absent`）同时喂给常驻 watchman 与 app 内循环。
- **唯一事实来源是登记，不是进程树。** 谁该有 daemon 由登记声明；daemon 是否有由 socket
  探测决定（`is_running`）。
- **幂等、无协调信号。** 两个守护者重复探测无害；daemon 自身的独占 flock 保证不会双起。

## 3. 架构与数据流

```
看板 create/remove ──> 通用常驻登记 ~/.yi-agent/resident-daemons.json
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
  watchman（launchd 托管，常驻）      app-server 内循环（app 打开时）
     每 ~30s ensure_daemons              每 ~30s ensure_daemons
              └────────────> 各项目 daemon（独占 flock）─> 插件监督循环
                                     └─> 插件（单实例锁 + daemon 失联即退）
```

- **A（持久）**：launchd 的 `KeepAlive` 托管 watchman；watchman 保证各项目 daemon 存活。
- **B（会话期）**：app-server 跑同一 `ensure_daemons`。
- 两条腿都幂等；daemon 启动拿独占锁（`InstanceLock`），第二个以 `AlreadyRunning` 干净
  退出，**不会双起 daemon**。

## 4. 组件

### 4.1 通用常驻登记（宿主，非看板专属）

- 新文件：`~/.yi-agent/resident-daemons.json`
- 形状：
  ```json
  { "projects": [ { "project": "/abs/path", "required_by": ["superpowers-kanban"] } ] }
  ```
- API 落在 **`yi-agent-store::resident`**（宿主的通用存储层，非看板 crate），以保持登记本身
  与看板解耦；写它的是 `yi-agent-boards`（看板生命周期），读它的是 watchman 与 app-server：
  - `require(global, project, requester)`——合并登记（幂等，追加 requester）。
  - `release(global, project, requester)`——摘除 requester；`required_by` 空则删该项目项。
  - `list(global) -> Vec<PathBuf>`——watchman 与 app 循环的输入。
  - 损坏文件按「拒绝覆盖 + 当作空」处理，与 `boards.json` 的既有约定一致（不静默丢数据）。
- **不含任何 board 语义**：字段是 `project` + `required_by`，将来别的常驻插件可复用。

### 4.2 watchman：`yi-agent boards watch`

- 宿主侧子命令。循环：`list()` → 逐个 `ensure_daemons`（`is_running` 否则
  `launch_if_absent`）→ sleep ~30s。
- 不读 `boards.json`，不 spawn 看板专属逻辑；daemon 起来后插件由 daemon 自己监督（现成）。
- 非 macOS / 无 launchd 时，可当作普通脱离进程运行（与 daemon 同样的 `setsid` 手法）。

### 4.3 `ensure_daemons` 胶水

- 位置：`yi-agent-boards`（已有 `launch_if_absent` 与 `is_running`）。
- `pub fn ensure_daemons(projects: &[PathBuf])`：对每个项目 `launch_if_absent`（内部先
  `is_running` 再 spawn），失败只记日志不中断其余项目。
- **B 与 A 共用它**，不重复实现。

### 4.4 LaunchAgent 安装 / 卸载 / 开关（I2）

- 首次**创建看板**时自动安装 LaunchAgent（`~/Library/LaunchAgents/`），
  `RunAtLoad` + `KeepAlive`，`ProgramArguments = [<yi-agent abs path>, "boards", "watch"]`。
- 安装后**界面上明确告知**：「看板将在后台持续运行，已设置开机自启」。
- 设置界面给一个开关「后台值守 / 开机自启」：
  - 关 → `launchctl bootout` + 删 plist。
  - 开 → 重装 plist + `bootstrap`。
- 该开关是**宿主级**设置（`~/.yi-agent/preferences.json`，键 `board_watchman_enabled`，
  默认 `true`）；app-server 在首次安装时写入 `true`。
- **失败不静默、不打扰**：plist 写入或 `launchctl` 加载失败时**不**弹阻塞式错误，但也不
  假装成功——记一条警告日志，并在设置项旁显示内联提示（「后台值守未启用」）。
- **最后一块看板移除** → 自动卸载（并 `release` 登记）。
- plist 写死 `yi-agent` 绝对路径；若检测到路径与当前可执行文件不符，重装（安装路径稳定，
  可接受）。

### 4.5 app 侧自愈（B）

- `app-server` 加轻量周期任务（~30s）：`resident::list()` → `ensure_daemons`。
- 与 watchman 重叠无害（幂等）。app 关掉后由 watchman 接管。
- TUI 可选同做（非本轮必需，记为可选）。

### 4.6 插件侧：单实例锁 + daemon 失联即退（防双跑、防空转）

- **单实例锁**：插件启动即对 `<state_dir>/plugin.lock` 取 `flock(LOCK_EX|LOCK_NB)`；拿不到
  则说明已有插件在跑，本进程等待（轮询）或退出——保证同一时刻**只有一个插件推进队列**。
  复用与本仓库一致的 flock 约定。
- **daemon 失联即退**：推进循环每轮（或在独立线程按 ~5s）探一次 daemon `Status`；连续失败
  到一个阈值（例如 3 次）即 `exit(0)`，不再空转。
- 时序：daemon 崩 → 孤儿插件数秒内自退并释放锁 → 新 daemon 拉新插件 → 新插件立刻拿锁
  开工。窗口极小，且**绝不两个插件同时推进**。
- 备选（更复杂，本轮不做）：kqueue 监听 parent 退出即时感知；新插件阻塞等锁而非退出。

### 4.7 重启后的卡片对账（复用 Task 8 对账）

- 任务状态持久化（`tasks` 表），新 daemon 会 rehydrate。`reconcile_running` 据
  `task_state` 把「跑完的」迁 `AwaitingMerge`、「需人处理」（`recovery_required` 等）迁
  `NeedsYou`——**都会释放全局名额**，队列继续。
- 这是既有机制的复用，不新增对账逻辑；只补测试覆盖「daemon 重启后停在 Running 的卡能被
  对账移走并释放名额」。

## 5. 验收

1. **持久守护（A）**：杀掉某项目 daemon，~30s 内被自动拉回并应答 `Status`；插件的排队
   任务继续推进（实测：临时 HOME + 真 daemon/插件）。
2. **会话期自愈（B）**：app-server 运行时杀掉 daemon，~30s 内被拉回。
3. **不双起**：两个守护者同时跑、或两次 ensure，任何时候每项目只有一个 daemon
   （`AlreadyRunning` 干净退出）。
4. **不双跑插件**：daemon 崩后孤儿插件在有界时间内退出；新 daemon 的新插件拿锁成功；
   断言期间同一项目只有一个插件在推进队列（无重复启动同一张卡）。
5. **登记耦合**：看板 create 写通用登记（`required_by` 含 `superpowers-kanban`）、remove
   摘除；watchman 只读通用登记、不读 `boards.json`。
6. **开关**：关「后台值守」→ LaunchAgent 卸载、plist 删除；开 → 重装；行为与界面告知一致。
7. **重启恢复**：杀死带运行中卡片的 daemon 后重启，卡片被对账移出 `Running`、名额释放、
   队列继续。
8. **回归**：宿主、插件、桌面全绿。

## 6. 非目标（本轮）

- 用户可见的「开机自启项」完整管理 UI（本轮只给一个开关 + 一次安装告知）。
- Linux/Windows 的等价自启机制（先做 macOS launchd；watchman 逻辑本身跨平台）。
- TUI 侧的 B（会话期自愈）非必需，记为可选。
- 「一个总管进程统管所有项目」的其它形态。

## 7. 风险

| 风险 | 处置 |
|---|---|
| 孤儿插件与新插件双跑推进同一队列 | 插件单实例锁 + daemon 失联即退（4.6）；集成测试覆盖 |
| 两个守护者同时 ensure 抢拉 daemon | daemon 独占 flock，第二个 AlreadyRunning 干净退出；测试覆盖 |
| LaunchAgent 写死的 yi-agent 路径失效 | 检测路径不符则重装；安装路径稳定 |
| 通用登记损坏 | 拒绝覆盖 + 当作空，与 `boards.json` 既有约定一致，不静默丢数据 |
| 阈值/间隔不当导致误退或响应慢 | 间隔 ~5s、阈值 3 次；可调常量集中一处 |
