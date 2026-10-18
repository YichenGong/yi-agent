# Superpowers Kanban 插件

完整安装步骤（含每步的预期输出与卸载）见 [INSTALL.md](INSTALL.md)。

## 安装
1. 放入 `superpowers-kanban` 可执行文件（`cargo build -p superpowers-kanban-runner --release`，
   产物名为 `superpowers-kanban`）。
2. 放入 `superpowers-kanban` skill 目录（安装到 `~/.yi-agent/skills/superpowers-kanban/`）。
3. 把 `supervisors/superpowers-kanban.json` 复制到
   `<项目>/.yi-agent/supervisors/superpowers-kanban.json`，并按需改 `command` 为绝对路径。
4. 在 `<项目>/.yi-agent/preferences.json` 写入 `{"superpowers_kanban": true}`
   （或在 TUI/桌面里打开开关）。

## 运行前提
daemon 常驻（`yi-agent daemon start`）。daemon 会按清单与开关拉起 `superpowers-kanban run`
并守护它。

## 卸载
删除 `supervisors/superpowers-kanban.json`（daemon 随即停掉该进程），再删除本目录。
正在 daemon 中运行的会话会照常跑完，不影响主程序。

## 配置
`superpowers-kanban.toml` 放在 `<项目>/.yi-agent/superpowers-kanban/superpowers-kanban.toml`
（示例见本目录）。旧名 `kanban.toml` 仍会被读取（迁移期兼容），但**只读不改**。

并发与时间设置也可在桌面「设置 → 插件 → superpowers-kanban」里改：默认并发上限、
时段窗口（星期/起止/上限）、推进间隔秒数。保存即写入同一个
`superpowers-kanban.toml`；推进间隔下一个 tick 生效，无需重启插件。

## 加入看板
三种入口，写的是同一个投递目录：

- 命令行：`superpowers-kanban add <spec> <plan>`（可选 `--state-dir <dir>`）
- 命令行：`superpowers-kanban add-prequeued <title>`（需求已定、spec/plan 还没写时先记一张）
- 命令行：`superpowers-kanban add-merge <source> [--base <ref>]`（可选 `--state-dir <dir>`）
- TUI：`/superpowers-kanban add <spec> <plan>` / `/superpowers-kanban add-prequeued <title>`
- 桌面端：看板面板的入队动作（含最左 `PRE-QUEUED` 列的标题输入）

`add` 会先校验两份文件存在且互不相同，失败立刻以非零退出码报错。
投递写进 `<项目>/.yi-agent/superpowers-kanban/inbox/`，插件每 tick 消费并入队。

`add-merge` 投递一张合并卡：把 `source` 分支合进 `base`（缺省取
`origin/HEAD` 指向的默认分支）。它会先确认 `source` 分支真实存在，否则立刻报错
而不投递。

### 预排队（pre-queued）

需求已决定要做、但 spec/plan 还没写时，先记一张预排队卡持续追踪：

```
superpowers-kanban add-prequeued "需求标题"
```

它出现在看板最左的 `PRE-QUEUED` 列（桌面端侧栏摘要记为「N 预排队」），不占并发槽、
不会被起会话。设计写完、两份文件落盘后提升为正式排队卡：

```
superpowers-kanban promote <card-id> <spec> <plan> [--title <新标题>]
```

卡片的 id 在创建时由标题派生并定死（非 ASCII 标题回退为 `prequeued-<时间戳>`），
提升**不重算 id**、会重排到队尾，因此从预排队到 done 始终是同一张卡。`promote` 需在
插件下一次 tick 消费掉预排队卡之后再执行（此时卡已在 `board.json` 上）；同一 tick 内
紧跟着提升会报 `unknown card`。

```
superpowers-kanban add-merge kanban/2026-10-03-a-feature
superpowers-kanban add-merge kanban/2026-10-03-a-feature --base develop
```

## 命令行
`superpowers-kanban <run|add|add-prequeued|promote|add-merge|done|archive|purge|list|on|off|workdir>`——`run` 是 daemon 守护的推进循环，
其余是一次性查询/写入。`workdir` 报告状态目录对应的项目根。
`run` 每 tick 除消费看板投递外，还会认领一张待合并卡并执行合并（本项目同一时刻只有一张）。

`done <card-id>` 把一张 `awaiting_merge` 的卡结算为 `done`（人工合并后的登记通路，会核对
推导出的 `kanban/<slug>` 分支是否已并入默认分支并如实打印 `verified` / `branch-missing` /
`not-merged`，但不据此阻断）；`archive <card-id>|--all-terminal` 归档（隐藏但保留）；
`purge <card-id>|--all-archived` 真删（**仅限已归档**）。`list` 默认不显示归档卡，`list --all` 显示。

进入真终止态（`done`/`failed`/`cancelled`）的卡超过宽限期（`archive_grace_hours`，默认 24h，
`0` = 立即）会自动归档；`awaiting_merge`/`needs_you` 永不自动归档。

`board.json` 是看板的**唯一真相源**：daemon 与一次性 CLI 都在同一把 `board.lock`
（flock）内「读-改-写」，因此 daemon 在跑时 CLI 的 `done`/`archive`/`purge` 不会被
覆盖；daemon 缺席时同一条路径照常工作。`run` 拿不到 `plugin.lock` 时按
`instance_wait_secs`（默认 10 秒）有界等待后退出，不再无限空转。启动时还会清掉
本插件确定路径上连不上的死 socket。

## 开关
`superpowers-kanban on|off` 写**项目层**（`<项目>/.yi-agent/preferences.json`）。
全局层在 `~/.yi-agent/preferences.json`，需自行设置。

`off` 只**停止推进**（已在跑的会话不会被取消）。进程本身继续运行，因为它的查询
通道是宿主与桌面端读取、改写开关的唯一入口——停掉它，`off` 就无法再被翻回来。
清单里的 `stop_when_disabled: false` 正是在向监督器声明这一点。

## 后台值守（开机自启，仅 macOS）

插件进程（`superpowers-kanban run`）由项目 daemon 守护，但 **daemon 本身没人守护**：
桌面 app 关着时机器重启或 `yi-agent boards watch` 崩溃，看板就停了。为此**宿主**
（桌面 app / app-server）会装一个 macOS LaunchAgent 兜底：

- **装的是什么**：`~/Library/LaunchAgents/ai.yi-agent.board-watchman.plist`，
  `RunAtLoad` + `KeepAlive`，跑的是宿主子命令 `yi-agent boards watch`（只读通用常驻
  登记，不认识看板）。日志：`~/.yi-agent/logs/board-watchman.log`。
- **何时装**：`board/create`（在桌面/TUI 里创建看板）时，若宿主级开关
  `board_watchman_enabled` 为真（**默认开**）就装；可执行文件换位置（升级）会重装。
  best-effort——装不上不影响看板创建。
- **怎么开关**：桌面「看板设置」里的「后台值守（开机自启）」开关，即宿主级偏好
  `board_watchman_enabled`（写在 `<工作目录>/.yi-agent/preferences.json`）。关掉即卸载
  （`launchctl bootout` + 删 plist）。它与上面的插件级 `superpowers_kanban` 开关是
  **两件事**：前者管「机器重启后看板还活着吗」，后者管「要不要推进队列」。
- **不想让它碰 launchd**：以 `YI_AGENT_DISABLE_WATCHMAN=1` 启动宿主，装/卸一律退化成
  no-op。

这是**宿主**能力，不是插件二进制的能力——`superpowers-kanban` 没有值守子命令，
删掉本插件也不会自动删这个 LaunchAgent。要删就关掉那个开关，或
`launchctl bootout gui/$(id -u)/ai.yi-agent.board-watchman` 后删 plist。

## 迁移说明（旧名 → 新名）
旧布局仍被**读取**，不会被修改或删除：

| 旧 | 新 |
| --- | --- |
| 开关键 `superpowers_board` | `superpowers_kanban` |
| 状态目录 `.yi-agent/board/` | `.yi-agent/superpowers-kanban/` |
| 日历 `kanban.toml` | `superpowers-kanban.toml` |

确认新位置已就绪后，可自行删除旧文件。
