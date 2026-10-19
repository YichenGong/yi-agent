# macOS 桌面 App 安装器：CLI 入口、原地升级与 daemon 协调设计

## 状态

设计已由用户确认（2026-10-19），已通过代码级审查并修正 5 处不成立假设。

## 背景

`desktop/packaging/install.sh` 目前只做三件事：挂载 dmg、把 `yi-agent.app`
拷进 `/Applications`、递归清 quarantine。dmg 内除 GUI 外还带一份完整的 CLI
（`Contents/MacOS/yi-agent`），由 `desktop/src-tauri/tauri.conf.json` 的
`externalBin` 打进 bundle（`desktop/src-tauri/src/bridge.rs:323`）。这份 CLI
与 GUI 同版本，TUI 就是它。

现状有三个缺口：

1. 用户装完 App 后没有任何入口能用到这份 CLI。Release 说明只写了一行
   「桌面 App 已内置同一份 CLI」，没说它在哪、怎么调用。用户只能另下独立
   tarball，从而引入两份二进制的版本漂移。
2. `rm -rf "$DEST"` 先于 `ditto`（`install.sh:34-35`）。拷贝中途失败会留下
   残缺 App，GUI 与 CLI 一起不可用。
3. 升级后旧 runtime daemon 不认新版本：协议版本不匹配时服务端回
   `UnsupportedProtocol`（`yi-agent-store/src/ipc.rs:1628-1634`），客户端把
   该响应归为 `Unknown` 后放弃（`yi-agent-subagent/src/attach.rs:345-356`、
   `407-411`）。旧 daemon 连 `Stop` 都不认，因为版本检查在命令分发之前。
   结果是升级后子 agent 派发可能永久不可用，重启 TUI 也无效。

## 目标

1. 装完 App 后，内置 CLI 可在终端直接以 `yi-agent` 调用，不另下产物、不改
   用户 shell 配置。
2. 升级只需重跑 `bash install.sh <dmg>`：自动退出运行的 GUI、原子替换 App、
   重建 CLI 入口、清理旧 daemon、并让用户看得见版本变化。
3. 任何一步失败都不会留下「旧版已删、新版没装好」的状态。
4. 不误伤开发环境（worktree 里的 `target/debug/yi-agent`）与用户正在使用的
   TUI 会话。

## 非目标

- 不修 `UnsupportedProtocol` 被归为 `Unknown` 的客户端分类逻辑。本次用
  「安装时清理旧 daemon」绕过它；分类逻辑本身需要独立 spec 与测试。
- 不做 Homebrew Cask（dmg 托管在 GitLab，不在 GitHub Releases）。
- 不引入 Tauri updater（当前无 updater 插件与签名配置）。
- 不在 App 内新增「安装命令行工具」按钮。
- 不支持 Intel Mac（dmg 只有 arm64）。
- **不支持装到 `/Applications` 以外**。理由见「不降级安装位置」。

## 关键事实（实测，均在本机验证）

这些是设计的前提，后续修改前必须重新验证：

| 事实 | 证据 |
| --- | --- |
| `/usr/local/bin` 在默认 PATH 中，无需改 shell 配置 | `env -i /bin/zsh -c 'echo $PATH'` 含 `/usr/local/bin`；`/usr/libexec/path_helper -s` 同 |
| GUI 进程名是 `desktop`，但 `pgrep -x desktop` 返回空 | `ps -o comm= -p <gui>` 返回 `/Applications/yi-agent.app/Contents/MacOS/desktop` |
| `pgrep -f` 在本机找不到 App 的 sidecar | `pgrep -f 'yi-agent app-server'` 为空，而 `ps` 能列出 |
| `comm` 在多列组合下被截断 | `ps -Aw -o pid=,comm=,command=` 的 comm 列被截成 `/tmp/ya-probe-li` |
| 通过软链启动的进程，`comm` 是软链路径 | `ln -sf /bin/sleep /tmp/x && /tmp/x 30` → `comm=/tmp/x` |
| `lsof` 能给出真实 inode 路径 | `lsof -p <pid>` 的 `txt` 行是 `/Applications/.../yi-agent` |
| sidecar 在 GUI 退出后靠 stdin EOF 自行退出 | `tauri-plugin-shell` spawn 未设 `kill_on_drop`；`app-server/src/server.rs:1616`、`2123-2128` 明确 stdio EOF ⇒ 优雅退出 |
| `/Applications` 属 `root:admin drwxrwxr-x` | 非 admin 账户不可写 |
| `ai.yi-agent.board-watchman` 是当前正式 label | `yi-agent-boards/src/watchman.rs:9`；`resident-daemon-watchman` 来自未合并分支 |

## 设计

### 1. CLI 入口

按顺序选择第一个可写目录：

1. `/usr/local/bin`（首选，天然在 PATH）
2. `$HOME/.local/bin`（仅当 `/usr/local/bin` 不可写；该目录默认不在 PATH，
   必须显式提示需要 `export PATH="$HOME/.local/bin:$PATH"`）
3. 都不可写：跳过建链，打印手动 `sudo ln -sf ...`，安装整体仍成功

一律 `ln -sf /Applications/yi-agent.app/Contents/MacOS/yi-agent <dir>/yi-agent`，
不拷贝。软链指向固定路径，升级后自动指向新版本。

建链后读取 `command -v yi-agent`：

- 命中目标链接：成功
- 命中别的可执行文件（本机为 `/opt/homebrew/bin/yi-agent`，排在
  `/usr/local/bin` 之前）或未命中：打印警告，说明实际生效的路径，建议
  卸载同名版本或改用 `$HOME/.local/bin`。安装器不替用户卸载任何东西。
- 落在 `$HOME/.local/bin` 且不在 PATH：打印需要添加的 `export` 行，
  **不写入任何 rc 文件**

### 2. 不降级安装位置

`/Applications` 不可写时（非 admin 账户），**报错退出**并提示
`sudo bash install.sh <dmg>`，不降级到 `~/Applications`。

理由：App 位置被四处代码当作全局唯一事实依赖，分裂会导致

- `watchman.rs:64` 的 `is_installed_for` 用 plist 文本包含当前 exe 路径来判断，
  位置变化会让它认为未安装并反复重装；而 `bootstrap` 在已加载时返回
  「already bootstrapped」被当作成功（`watchman.rs:104-110`），结果是 plist
  改了但 launchd 仍指向旧路径，用户看不到任何错误
- 软链目标失效，`command -v yi-agent` 落到别处的旧版本
- `scripts/rebuild.sh:37` 的 `APP_DEST` 仍指向 `/Applications`
- 升级时的进程检测基于 `$DEST`，两处合法值会导致漏杀

### 3. 原子替换

```
STAGE=/Applications/.yi-agent.app.new
rm -rf "$STAGE"
ditto "$MOUNT/yi-agent.app" "$STAGE"        # 失败 → 旧版本原样保留
[ -d "$DEST" ] && mv "$DEST" "$DEST.old"
mv "$STAGE" "$DEST"
rm -rf "$DEST.old"
```

清理动作放 trap，异常退出也移除暂存目录与 `.old`。

### 4. 升级时自动退出 GUI

检测（**不用 `pgrep`，不用 `comm`**）：

```bash
ps -Aw -o pid=,command= |
  awk -v d="$DEST/Contents/MacOS/" 'index($0, d) {print $1}'
```

取到 pid 后按 argv 分类：

- `argv[1]` 为 `desktop`（或可执行文件是 `$DEST/Contents/MacOS/desktop`）→ GUI
- 其余同前缀进程 → 逐个处理

退出流程：

1. `osascript -e 'tell application "yi-agent" to quit'`（用 bundle 名
   `yi-agent`，来自 `CFBundleName`；`desktop` 只是可执行文件名）
2. 轮询 pid 消失，最多 5 秒（每 200ms 一次）
3. 仍在则 `kill <pid>`，再等 2 秒
4. 仍失败则**中止升级**，保留旧版本，提示手动退出后重跑

sidecar（`app-server --listen stdio://`）不需要单独处理：GUI 退出后其 stdin
pipe 关闭，`serve()` 在 EOF 处返回并优雅退出（`server.rs:1616`）。这一点依赖
sidecar 由 GUI 以管道方式持有，不得改成 detached 启动。

### 5. 旧 runtime daemon 的清理

这是 `UnsupportedProtocol` 缺陷的用户侧绕过。

**判据（三个条件同时满足才杀）**：

1. `ps -Aw -o pid=,command=` 中 argv 精确为 `daemon serve`
   —— TUI 无子命令、sidecar 是 `app-server`，两者都不会命中
2. 该进程的真实可执行文件属于本次安装的 App：
   `lsof -p <pid>` 的 `txt` 行路径 == `$DEST/Contents/MacOS/yi-agent`，
   或其软链 `/usr/local/bin/yi-agent` 解析后是同一 inode
   —— 用 `lsof` 而非 `comm`，因为软链启动时 `comm` 显示软链路径
3. 不是本次安装刚启动的进程（避免自杀）

不满足条件的进程一律不动，包括 worktree 的
`.../target/debug/yi-agent daemon serve`（本机同时有多个）。

**顺序必须是：先停值守 → 再停 daemon → 最后替换 App。**
否则 `KeepAlive=true` 会在 10 秒内把 daemon 拉回来（`watchman.rs:plist_contents`
的 `ThrottleInterval`），kill 变成无效操作；且替换窗口内 launchd 会连续
失败重试、污染日志。

**停止方式**：`kill`（SIGTERM）。`daemon serve` 主循环走 `daemon.wait()`
（`main.rs:722`），没有 SIGTERM 处理器，进程直接终止。代价是正在执行的
subagent 任务被中断；好在新 daemon 启动时会 `recover_inflight_tasks()`
（`ipc.rs:857`）把 `running/waiting_*` 收进 `recovery_required`，并
`reclaim_orphaned_tasks()`（`ipc.rs:862`），且「Reaping is deliberately
gentle: the partial report is preserved」（`repository.rs:2236-2238`），
已完成的部分不会丢。

### 6. 交互确认

检测到将被终止的 daemon 且其中存在未完成任务时，提示：

```
检测到 N 个正在运行的 yi-agent runtime daemon（属于已安装的 App）。
安装新版本需要重启它们；正在执行的子 agent 任务会被中断，
已完成的部分会保留。
继续？[Y/n]
```

- 用户输入 `n` → 退出安装，不修改任何文件
- 无 TTY（`[ ! -t 0 ]`）或 `YI_AGENT_NONINTERACTIVE=1` → 默认继续
- 没有待终止的 daemon → 不询问

### 7. LaunchAgent（常驻值守）协调

**不按 label 名判断**。label 正在变更（`ai.yi-agent.board-watchman` 是当前
正式名，未合并分支将改为 `ai.yi-agent.resident-daemon-watchman`），写死名字
会留下迁移债。改为遍历 `~/Library/LaunchAgents/*.plist`：

```
对每个 Label 以 "ai.yi-agent." 开头的 plist：
  exe = plutil -extract ProgramArguments.0 raw -o - <plist>
  若 exe == $DEST/Contents/MacOS/yi-agent：
      支持状态 = 已加载（launchctl print gui/$UID/<label> 退出码 0）
      若已加载 → bootout → 重写 plist（内容不变）→ bootstrap
      若未加载 → 不动，提示「值守偏好为开但未加载，下次从 App 创建看板时会自动安装」
  否则（指向别处，如 worktree debug 构建）：
      不动，警告「检测到指向开发构建的常驻值守，未修改」
```

`launchctl print` 的退出码判据已实测：未加载 113，已加载 0。

**不主动 bootstrap 未加载的服务**：加载等于开启一个后台自启服务，这是用户
决策。而且两个 label 并存时盲目加载会让两个值守抢同一项目的
`runtime.lock`（`ipc.rs:1200` 的 flock），其中一个持续报 `AlreadyRunning`。

### 8. 残留 plist 的清理

只删同时满足三条的：

1. Label 以 `ai.yi-agent.` 开头
2. `launchctl print gui/$UID/<label>` 失败（未加载）
3. `ProgramArguments[0]` 既不是当前 App 内 CLI，也不指向任何存在的可执行文件

三条全中才是真正的死配置。只要还指向一个存在的二进制（例如 debug 构建），
就不删——那可能是用户正在用的开发环境。

### 9. 版本展示

安装前读旧版本，安装后读新版本（`plutil -extract CFBundleShortVersionString
raw -o - $DEST/Contents/Info.plist`）：

- 首次安装：`已安装 yi-agent 0.1.7`
- 升级：`检测到已安装版本 0.1.6 → 升级到 0.1.7`

最后对内置 CLI 执行 `--version`，确认软链实际可执行。

### 10. 可测试性

`install.sh` 需能被单测驱动：

- 逻辑拆成纯函数：`pick_bin_dir`、`detect_gui_pids`、`daemon_pids_for`、
  `real_exe_of`、`watchman_plists`、`dead_plists`、`verify_cli`
- 路径经环境变量覆盖：`YI_AGENT_APP_PATH`、`YI_AGENT_BIN_DIR`、
  `YI_AGENT_LAUNCH_AGENTS_DIR`、`YI_AGENT_PS_CMD`、`YI_AGENT_LSOF_CMD`、
  `YI_AGENT_LAUNCHCTL_CMD`，默认值保持生产行为
- 末尾 `if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi` 收口

新增 `desktop/scripts/test_install_sh.sh`，在 `mktemp -d` 里构造假 App 与假
CLI，覆盖：

1. 目录选择优先级（`/usr/local/bin` 可写 / 不可写 / 都不可写）
2. `command -v` 命中他人时的告警
3. 版本解析（首次 / 升级 / plist 缺键）
4. 原子替换：模拟 `ditto` 失败时旧目录仍在
5. `daemon_pids_for`：`daemon serve` 命中、TUI 不命中、`app-server` 不命中、
   软链 exe 命中、worktree debug 不命中
6. `dead_plists`：三重失效才删
7. 非 TTY 下不阻塞

测试不依赖真实 dmg，不写 `/Applications`，不调真实 `launchctl`。

### 11. Release 文案

`desktop/scripts/release-dmg.sh` 的 `DESCRIPTION` 替换现有一句
「装了 App 就无需再装这个」为：

- TUI 入口：`/usr/local/bin/yi-agent -> /Applications/yi-agent.app/Contents/MacOS/yi-agent`
- 升级：重跑 `bash install.sh <新 dmg>`；会自动退出 GUI、清理旧 daemon、保留配置
- 配置共享：`~/.yi-agent/.env` 两边共用，无需配两遍
- 安装位置固定为 `/Applications`，非管理员需 `sudo`
- 不支持降级（schema 只升不降，`repository.rs:3829`）

## 边界与错误处理

| 情况 | 行为 |
| --- | --- |
| dmg 内无 `yi-agent.app` | 报错退出，不动已安装版本 |
| `/Applications` 不可写 | 报错，提示 `sudo`，不降级 |
| GUI 拒绝退出且 `kill` 无效 | 中止，保留旧版本，提示手动退出 |
| `ditto` 到暂存失败 | 中止，旧版本原样保留 |
| `/usr/local/bin` 不可写 | 回退 `$HOME/.local/bin`；都不可写则跳过并给手动命令 |
| 目标是断链 | `ln -sf` 覆盖 |
| 目标的 PATH 优先级被他人占先 | 告警，不自动卸载 |
| `xattr` 失败 | 只警告，不中断 |
| LaunchAgent 指向开发构建 | 只告警 |
| `launchctl bootout` 失败 | 警告后继续；重写 plist，不 bootstrap |
| 用户回答 `n` | 退出，零改动 |

## 验收判据

1. `bash desktop/scripts/test_install_sh.sh` 全绿。
2. 删掉 `/Applications/yi-agent.app` 后执行 `bash install.sh <dmg>`，
   新开终端 `yi-agent --version` 与 dmg 版本一致。
3. App 运行中执行升级：GUI 被自动退出、替换完成、重新打开为新版本、
   `~/.yi-agent/.env` 未变化。
4. App 运行中执行升级且存在 `daemon serve`：安装后该进程不再存在；
   同时 worktree 的 debug daemon 仍存活。
5. 安装前后 `/usr/local/bin/yi-agent` 始终指向
   `/Applications/yi-agent.app/Contents/MacOS/yi-agent`。
6. 仓库中不存在安装器对 `~/.zshrc` / `~/.zprofile` 的写入。
7. 非 admin 账户执行时给出 `sudo` 提示且不产生 `~/Applications/yi-agent.app`。