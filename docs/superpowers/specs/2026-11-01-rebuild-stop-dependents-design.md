# rebuild.sh 重建前停掉所有依附进程

> 目标：`scripts/rebuild.sh` 在编译/替换产物之前，先把所有正在使用旧二进制的
> 进程停干净，使「重建 → 重开 App」必然落到新版本。本文只改一个本地脚本，
> 不改任何 Rust/前端源码。
>
> 日期：2026-11-01
> 相关：`scripts/rebuild.sh`（本机开发重编脚本，`scripts/` 整个目录在
> `.git/info/exclude:15` 里，不进版本控制）、
> `yi-agent-rs/crates/yi-agent-supervisors/src/watchman.rs`（launchd 值守
> 的安装/卸载）、
> `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs:393-409`
> （daemon 退出时 `stop_all` 回收托管子进程）

## 1. 问题

### 1.1 现象

重建并重启 App 后，看板可能连到**旧版本**的 `yi-agent`。

### 1.2 根因

`build_desktop()` 在替换 `/Applications/yi-agent.app` 之前，检测到 App 在运行
就**跳过替换**（`scripts/rebuild.sh:144-151`）：

```sh
if app_is_running; then
  warn "App 正在运行，跳过替换 ${APP_DEST}。"
  warn "请手动退出 App 后重跑: ./scripts/rebuild.sh desktop"
else
  rm -rf "$APP_DEST"
  cp -R "$app_src" "$APP_DEST"
fi
```

于是 `/Applications/yi-agent.app` 仍然是旧 bundle。重开 App 后，它拉起的
sidecar / `daemon serve` / 被监督的看板全部来自旧 bundle，而
`/opt/homebrew/bin/superpowers-kanban` 即便已被 `build_plugin()` 换成新二进制，
也仍由**旧 daemon** 按旧清单托管。

换言之：**产物换了，进程没换；或者进程在跑，产物根本没换。** 两条路径都
让「重建」这个动作对运行中的系统无效。

### 1.3 实测的进程拓扑（2026-11-01，本机）

```
69419  /Applications/yi-agent.app/Contents/MacOS/desktop
69432    └─ .../MacOS/yi-agent app-server --listen stdio:// --relay wss://…
36833  /Applications/yi-agent.app/Contents/MacOS/yi-agent daemon serve   (ppid=1)
39679  /Applications/yi-agent.app/Contents/MacOS/yi-agent resident watch (ppid=1，launchd 托管)
67466    └─ superpowers-kanban run --runtime-dir …/.yi-agent/runtime …   (ppid=36833)
```

四个关键事实，每一个都影响实现：

1. **`daemon serve` 已脱离会话**（`daemon.rs` 的 `spawn_detached` 调 `setsid`，
   注释见该文件 1-8 行）。杀 desktop 不会带走它，必须单独处理。
2. **`resident watch` 由 launchd 以 `KeepAlive=true` 托管**
   （`~/Library/LaunchAgents/ai.yi-agent.resident-daemon-watchman.plist`）。直接
   `kill` 会在 `ThrottleInterval`（10s）后复活；并且它每 30s 通过
   `watch::once` → `daemon::ensure_running` 把 `daemon serve` 重新拉起
   （`yi-agent-rs/crates/yi-agent-supervisors/src/watch.rs:1-30`）。
3. **`superpowers-kanban` 是 `daemon serve` 的子进程**，由
   `Supervisor::stop_all` 在 daemon 正常退出时回收
   （`supervisor.rs:393-409`）。所以杀掉 daemon 本身就能带走它——但前提是
   daemon 走的是优雅退出路径。
4. **macOS 的 `pgrep -f` 不匹配完整 argv**（脚本 `:139-143` 已记录此坑）。
   任何基于 `pgrep -f yi-agent` 的匹配都不可靠。

## 2. 设计

### 2.1 停止顺序

顺序是设计的一部分，换序就会杀不干净：

```
① launchctl bootout gui/$UID/ai.yi-agent.resident-daemon-watchman   （最先）
② SIGTERM 全部命中进程
③ 轮询等待，最多 5s
④ SIGKILL 仍存活的
⑤ 再轮询确认归零，打印「实际停掉了哪些 pid」
```

**为什么 ① 必须最先**：`KeepAlive=true` 意味着只要先把 `resident watch` 杀掉，
launchd 会在 10 秒内拉起新的一个，而新实例的第一个动作就是
`ensure_running`，把刚杀掉的 `daemon serve` 复活。先 `bootout` 才能让「值守」
在整个重建窗口内不存在。

**为什么 ② 用 SIGTERM 而不是直接 SIGKILL**：daemon 收到 `SIGTERM` 后走正常
退出路径，`Supervisor::stop_all` 会先给 `superpowers-kanban` 写一条
`stopped … (supervisor shutting down)` 日志再回收（`supervisor.rs:393-399`）。
直接 SIGKILL 会让看板的子进程失去回收者，可能留下孤儿。SIGKILL 保留为超时兜底。

### 2.2 launchd 条目：只 bootout，保留 plist

`watchman.rs:150-167` 的 `uninstall` 会同时 `bootout` **并删除 plist**。本设计
**不调用它**，只做 `launchctl bootout`：

- plist 留在 `~/Library/LaunchAgents/`，下次登录时 launchd 自动重新加载，
  值守自己回来，用户不需要任何操作。
- 若连 plist 一起删（等于卸载），之后必须去 App 设置里把「后台值守」开关
  关掉再打开才能装回来——多一步人工操作，且用户很容易忘。
- 代价：从重建到下次登录之间，值守是停的。这段时间手动打开 App，daemon 与
  看板仍会被正常拉起（`app-server` 有 `ensure_running` 路径），只是没有登录
  自启与崩溃自拉。这个代价被显式接受。

标签取自 `watchman.rs:9` 的 `LABEL`。旧标签
`ai.yi-agent.board-watchman`（`watchman.rs:22`）也一并 `bootout`：老机器上它
可能还加载着，而它指向的子命令已不存在（`watchman.rs:13-22` 的注释）。删除
它属于 `watchman::install` 的职责，本脚本只 `bootout`、不删文件。

### 2.3 匹配规则

用 `ps -axo pid=,ppid=,command=` 取完整命令行，逐行判定命中**任一条**：

```
argv[0] 的 basename ∈ { yi-agent, yi-agent-dev, superpowers-kanban }
或 command 含 "/Applications/yi-agent.app/Contents/"
```

第二条覆盖 bundle 内的 Helper 进程（例如 `desktop` 的 basename 不叫
`yi-agent`，但路径在 bundle 内）。

**为什么不用 `pgrep -f`**：见 §1.3 事实 4。也不做
`command` 里包含 `yi-agent` 的宽匹配——那会误杀 `vi yi-agent/docs/x.md`、
`grep yi-agent`、`tail -f …/yi-agent.log` 这类无关进程。

**已知不覆盖**：`bash -c "... yi-agent ..."` 这类 shell 包装。本机当前实测
不存在这类常驻进程（§1.3），不为其增加复杂度。

### 2.4 祖先进程保护（关键）

从 App 里发起的重建（本会话正是如此：agent 进程 69432 就跑在
`app-server` 里）如果无条件 SIGKILL，会把**发起脚本的祖先**一起杀掉，脚本
自己在 `rm -rf` 与 `cp` 之间被打断，留下一个残缺的 `/Applications/yi-agent.app`。

实现：脚本先把自己的祖先链算出来（`$$` → 逐级 `ps -o ppid=` → 到 1），
命中目标时**跳过并告警**：

```
WARN: pid 69432 (app-server) 是本次重建的祖先进程，跳过。
      它仍持有旧二进制，需在 App 内重开会话才会换新。
```

其余目标照常停。这样从 App 对话里发起重建不会自毁，代价只是那一个进程留旧，
且告警明确指出。

**但 `desktop` / `all` 必须例外地提前失败。** 这两个子命令最后要替换
`/Applications/yi-agent.app`，而祖先正占用着它——无论先编多久，最后都装不进去。
所以 `stop_dependents` 在检测到「有祖先进程命中」且 `cmd` 是 `desktop`/`all` 时
直接 `die`，并给出「请到终端里运行」的完整命令。`cli` / `plugin` 不受影响：
它们不碰 bundle，祖先进程留旧不影响本次产物落地。

### 2.5 调用点：新增 `stop` 子命令，四个构建子命令都先停

新增 `./scripts/rebuild.sh stop`：只停进程、不编译。用途有两个——用户随时手动
清理；实现完成后可以直接用它做验收，不必等一次完整编译。

`all | cli | desktop | plugin` 四个子命令都在 `preflight()` 之后、任何构建之前
调用同一个 `stop_dependents()`。理由：`plugin` 单独重建也要让
`/opt/homebrew/bin/superpowers-kanban` 的新二进制生效，而旧 daemon 正托管着
该路径的旧进程。

`build_desktop()` 里原本「App 在运行就跳过替换」的分支改为真正的替换——此时
已经没人占用 bundle。保留一次 `app_is_running` 兜底检查：若仍有进程存活（例如
§2.4 跳过的祖先），则**不** `rm -rf`，而是 `die` 并说明原因。宁可失败也不要
产出残缺 bundle。

### 2.6 明确不做

- 不删 plist、不调 `watchman::uninstall`、不碰 `~/.local/bin` / `~/.cargo/bin`
- 不删 `.yi-agent/runtime/runtime.lock`（有活 daemon 时删它会破坏独占语义，
  见 `daemon.rs:29-35` 关于 runtime 目录的约定）
- 不负责恢复：不 `open -a`、不 `launchctl bootstrap`。脚本末尾打印下一步指引
- 不改任何 Rust / 前端源码；不改 `justfile`
- `scripts/` 在 `.git/info/exclude` 内，所以脚本改动本身不进 git。本文档
  （`docs/`）照常提交

## 3. 错误处理

| 情况 | 行为 |
| --- | --- |
| 没有命中任何进程 | 打印「没有需要停止的进程」，退出码 0（幂等） |
| `launchctl bootout` 失败（未加载） | 不计为错误，打印一行说明后继续 |
| SIGTERM 后 5s 仍有存活 | SIGKILL，再等 2s；仍存活则 `die`，不继续构建 |
| 目标 pid 是祖先进程 | 跳过 + 告警，其余照常 |
| 祖先进程命中且子命令是 `desktop`/`all` | `die` 并提示到终端里重跑（bundle 无法替换） |
| `ps` 解析不到某行 | 跳过该行，不中断 |

## 4. 验收

1. `./scripts/rebuild.sh stop`
   → 打印 `bootout` 结果 + 停掉的 pid 清单（含 `desktop` / `app-server` /
     `daemon serve` / `resident watch` / `superpowers-kanban`，祖先除外）
2. `ps -axo command | grep -E 'yi-agent|superpowers-kanban' | grep -v grep`
   → 只剩本 shell 链与 §2.4 跳过的祖先
3. `launchctl list | grep -E 'yi-agent|kanban'` → 无输出
4. 再次 `./scripts/rebuild.sh stop` → 「没有需要停止的进程」，退出码 0（幂等）
5. `./scripts/rebuild.sh desktop` 全流程走通，`/Applications/yi-agent.app`
   被真正替换（用 `stat -f '%Sm'` 比对时间戳）
6. 重新构造 `resident watch` 场景（或等待下一次登录）后确认值守能自行恢复，
   验证 §2.2「保留 plist 即可自动回来」成立

## 5. 影响面

- 只影响本机开发脚本，不进入发布产物，不影响 CI，不影响任何用户
- 唯一的行为变化：重建会主动结束正在运行的 App 与会话。发起重建等于放弃
  当前正在进行的会话——这正是用户明确接受的取舍（「我不会需要保护 app 的时候
  运行这个更新脚本」）

## 6. 实现与验证记录（2026-11-01）

实现落在 `scripts/rebuild.sh`：新增 `WATCHMAN_LABEL` /
`LEGACY_WATCHMAN_LABEL`、`is_target_command`、`target_pids`、
`ancestor_pids`、`wait_gone`、`stop_dependents`，新增 `stop` 子命令，
四个构建子命令在 `preflight` 之后调用 `stop_dependents`。

`scripts/` 整个目录在 `.git/info/exclude`，故脚本改动本身不入库（与该文件头部
声明一致）；本文档照常入库。

### 验证

**匹配器单测（18/18）**：命中 `desktop`、`daemon serve`、`resident watch`、
`app-server`、`superpowers-kanban run`、`yi-agent-dev`、`~/.cargo/bin/yi-agent`、
终端 `yi-agent tui`；不命中 `vi yi-agent/docs/foo.md`、`grep -r yi-agent .`、
`tail -f …/resident-daemon-watchman.log`、`sh -c echo yi-agent`、
`vim …/superpowers-kanban/SKILL.md`、`cargo build --manifest-path …/yi-agent-rs/…`、
`mdfind superpowers-kanban`、`ls /Applications/yi-agent.app`、
`open -a /Applications/yi-agent.app`、`/sbin/launchd`。

**`stop_dependents` 流程测试（16/16）**：把 `ps` / `launchctl` / `kill` /
`sleep` / `date` 换成假实现（不碰真实进程），覆盖——

- 四个目标全部停掉（`daemon serve` / `resident watch` /
  `superpowers-kanban` / 终端 `yi-agent`）
- 祖先进程（`desktop` / `app-server`）被跳过且有告警
- 无关进程（`vim`）不被误杀
- 先 `bootout` 值守，且旧标签 `ai.yi-agent.board-watchman` 一并清掉
- 幂等：无目标时打印「没有需要停止的进程」并退出 0
- `desktop` 从 App 内发起时提前失败（不白等编译），原因明确
- 超时未退出时走 SIGKILL 兜底
- 确实杀不掉时中止（退出码非 0），不会进入替换 `/Applications` 的分支

**真实进程表上的只读演练**：对当时真实运行的 5 个进程执行
`target_pids "$(ancestor_pids)"`，结果为「停 3 个（`daemon serve`、
`resident watch`、`superpowers-kanban`）、跳过 2 个祖先（`desktop`、
`app-server`）」，与预期一致。

**未执行**：真实调用 `./scripts/rebuild.sh stop` 会杀掉承载当次会话的
`app-server`，使验证报告无法交付，故留在用户实际重建时执行。用
`./scripts/rebuild.sh stop` 即可复现 §4 的验收 1-4。
