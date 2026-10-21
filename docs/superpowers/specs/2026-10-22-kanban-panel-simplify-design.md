# 看板面板瘦身：移除两个冗余控件，把「移除看板」做成真卸载

日期：2026-10-22
状态：已实现

## 1. 要解决的问题

看板页顶栏现在堆了三个控件：项目开关、后台值守开关、「加入看板」按钮。用户实测
后提了三条意见，其中两条成立：

1. 面板里的「Superpowers 看板」开关多余。创建看板时它必然已被打开，再问一次没有
   信息量。
2. 「后台值守」开关多余，应当默认开启且不打扰。它本来缺省就是开，而且已经不是
   看板专属。
3. 「加入看板」按钮多余——不成立。右键菜单只有创建/移除，它是唯一的手工入队入口。

另有**一个真实缺陷**必须先修：右键「移除看板」并不真正卸载。它只做两件事——撤销
常驻登记、删状态目录。它不停该项目的 daemon，不删 supervisor 清单，也不清项目
开关。而插件进程仍在跑，`consume_inbox` 每拍经 `with_board` 走
`create_dir_all`，会把刚删掉的状态目录重建出来。

用真实二进制做过隔离实验：删掉 `--state-dir` 后，下一拍目录里重新出现了
`board.json` 与 `board.lock`。（实验局限：真实 `run` 依赖上游 daemon，隔离环境
里它几秒后自行退出，故只证明了写入路径会重建目录，不证明线上时序。）

因此顺序是：**先让「移除」真的能停下看板**，再把「停下」的唯一入口从开关上摘掉。

## 2. 决策

| 控件 | 处置 |
|---|---|
| 面板「Superpowers 看板」开关 | 降级为一行只读状态；写入口移除 |
| 「后台值守」开关 | 从看板面板删除，迁到设置页「通用」Tab |
| 「加入看板」按钮 | 保留功能，收进顶栏「＋」弹出菜单 |
| 右键「移除看板」 | 升级为完整卸载 |

### 2.1 为什么只读状态要留下

`(project)` / `(global)` 那个来源标记是用户判断「这个项目为什么不动」的唯一线索。
开关的写入口可以没有，状态不能没有。

### 2.2 为什么值守挪去「通用」

`plugin/install` 成功后会 best-effort 调 `ensure_watchman_installed`，只要开关为开
且 plist 未指向当前 exe 就装一次。值缺省为 `true`（坏文件、缺键同样回退 `true`）。
它装的是通用 `ai.yi-agent.resident-daemon-watchman`，跑 `yi-agent resident watch`，
服务所有登记在册的项目——与当前选中的看板无关，所以放在看板面板是错位。

### 2.3 「移除看板」的新语义

一次完整卸载，四步，幂等：

1. 若该项目 daemon 在应答，经插件查询 `lifecycle/remove`：撤销常驻登记 + 插件删
   自己的状态目录。daemon 不在时**静默跳过**——那不是「插件拒绝配合」。
2. 宿主再删一次状态目录（`<项目>/.yi-agent/<清单名>`）：插件可能刚又写了一拍。
3. 从 `<项目>/.yi-agent/preferences.json` 删掉清单声明的 `switch_key`（只删这
   一个键）；删日志。
4. 最后删 `<项目>/.yi-agent/supervisors/<plugin>.json`。

### 2.4 为什么不停 daemon

最初的设计想在第 2 步 `daemon::stop`，实测后放弃了，因为它是**项目级**的：同一个
项目里可能跑着用户并未要求我碰的会话，为一个插件的卸载把它停掉属于误伤。

不需要它也能达到目的。`Supervisor::reconcile` 每 500ms 扫一遍清单目录，清单被删除
的进程走 `stop(&name, "manifest removed")`——**只停这一个子进程**。而常驻登记已被
第 1 步撤销，值守不会再把它拉起来。实测证据：`supervisor.rs:200` 的 `stale` 分支。

第 4 步（删清单）因此必须放在最后：它才是「让监督器停掉它」的那一下，前面任何
一步失败都该留一条「至少还能再装一次」的路。

## 3. 接口

### 3.1 新 RPC：`plugin/uninstall`

宿主侧通用接口，不认识任何插件名：

```
plugin/uninstall { project, plugin }
→ { "uninstalled": true, "warnings": [string] }
```

- 与 `plugin/install` 严格对偶：安装做「写清单 + 开开关」，卸载做「删清单 +
  删开关 + 删日志 + 删状态目录」。
- 不返回 `stopped`：进程退出是 supervisor 下一拍的事，这里不谎称「已停」。
  `uninstalled` 的含义是「文件层已摘干净，监督器不会再拉起它」。
- 开关名**只从已安装的清单里读**，不接受调用方传：传一个不存在的名字会删掉别人
  的键。
- 好处：`plugin/uninstall` 是通用能力，以后任何常驻插件都能用，不需要看板专属
  RPC（这与 `plugin/install` 当初的通用化方向一致）。

### 3.2 落地位置

- `yi-agent-supervisors/src/install.rs`：`remove_manifest`、`disable_switch`、
  `uninstall_manifest_for`（与 `install_manifest_for` / `enable_switch` 对称）。
- `yi-agent-app-server/src/rpc/plugin.rs`：`plugin/uninstall` 分支。
- `desktop/src/lib/superpowersKanbanBoards.ts`：`removeBoard` 先卸载、再用
  `lifecycle/remove` 幂等兜底（兜底失败被显式忽略——卸载成功即移除成功）。

### 3.3 桌面

`removeBoard` 改为：先 `plugin/uninstall`，再 `lifecycle/remove` 兜底（幂等，
覆盖步骤 1 因 socket 不通而跳过的情形）。

## 4. 验收

- `cargo test -p yi-agent-app-server --lib plugin_uninstall`：卸载后清单不存在、
  `switch_key` 从偏好里消失且其它键保留、状态目录与日志不存在、重复调用与
  「从未装过」都成功、含 `../` 的名字被拒。
- `cargo test -p yi-agent-supervisors --lib`：`install`/`uninstall` 对称性
  （`uninstalling_clears_the_switch_the_log_and_the_manifest` 等）。
- `cd desktop && npx vitest run src/App.test.tsx`：看板页不再有开关复选框与值守
  复选框；「加入看板」在「＋」菜单内可点；移除看板先发 `plugin/uninstall`。
- `cd desktop && npx vitest run`：全套 1567 passed。
- 手工：右键移除后 `pgrep -f "superpowers-kanban run"` 在约 500ms（一个
  reconcile 周期）内不再有该项目的进程，状态目录不会被重建。

## 5. 结果

| 项 | 结果 |
|---|---|
| `yi-agent-supervisors --lib` | 64 passed |
| `yi-agent-app-server --lib` | 982 passed |
| `desktop` `tsc --noEmit` | 无输出（干净） |
| `desktop` `vitest run` | 122 files / 1567 tests passed |
