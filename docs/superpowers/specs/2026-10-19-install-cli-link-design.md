# macOS 桌面 App 安装器：内置 CLI 入口与原地升级设计

## 状态

设计已由用户确认（2026-10-19），等待 spec review。

## 背景

`desktop/packaging/install.sh` 目前只做三件事：挂载 dmg、把 `yi-agent.app`
拷进 `/Applications`、递归清 quarantine。dmg 内除 GUI 外还带一份完整的 CLI
（`Contents/MacOS/yi-agent`），它是由 `desktop/src-tauri/tauri.conf.json` 的
`externalBin` 打进 bundle 的 sidecar（`desktop/src-tauri/src/bridge.rs:323`）。
这份 CLI 与 GUI 同版本、同进程镜像，TUI 就是它。

问题是：用户在 Finder 里双击 dmg 或跑完 `install.sh` 之后，**没有任何入口
能用到这份 CLI**。Release 说明只写了一行「桌面 App 已内置同一份 CLI，装了
App 就无需再装这个」，但没说它在哪、怎么调用。于是「只装了 App 的用户如何
使用 TUI」在分发层是无解的，用户只能去另下一份独立 CLI tarball，从而引入
两份二进制版本漂移的风险。

同时，`install.sh` 目前不支持「覆盖升级」的语义：

- `rm -rf "$DEST"` 先于 `ditto`（`install.sh:34-35`），拷到一半失败会留下
  一个残缺的 App，CLI 与 GUI 一起不可用；
- App 正在运行时替换 `.app`，用户会处于「磁盘上是新版、进程还是旧版」的
  状态，且没有任何提示；
- 没有任何版本提示，用户无法确认升级是否生效。

## 目标

1. 装完 App 后，同一份内置 CLI 在终端里可直接以 `yi-agent` 调用，**不需要
   用户另下产物、不需要手写 shell 配置**。
2. 升级时用户仍然只需要跑一条 `bash install.sh <dmg>`；脚本自动退出运行中
   的 GUI、原子替换 App、重建 CLI 入口，并让用户看得见版本变化。
3. 升级过程中的任何失败都不会导致「旧版已删、新版没装好」。
4. 安装器不修改用户的 shell 配置文件（`.zshrc` / `.zprofile`）。

## 非目标

- **不**修「跨版本 runtime daemon 无法回收」的缺陷。旧 daemon 的协议版本
  不匹配（`yi-agent-store/src/ipc.rs:35` 的 `PROTOCOL_VERSION`，服务端在
  `ipc.rs:1629-1634` 回 `UnsupportedProtocol`），而客户端把该响应归为
  `Unknown` 后放弃（`yi-agent-subagent/src/attach.rs:345-356`、
  `attach.rs:407-411`），因此新版 TUI 的子 agent 派发可能不可用。这需要独立
  的 spec 与测试，本次只在 Release 文案里给出兜底动作。
- **不**做 Homebrew Cask 渠道（dmg 目前托管在 GitLab，不在 GitHub Releases）。
- **不**引入 Tauri updater（当前无 `updater` 插件与签名配置）。
- **不**在 App 内新增「安装命令行工具」按钮。软链由安装器完成；UI 入口留作
  后续增强。
- **不**支持 Intel Mac（dmg 只有 arm64）。

## 设计

### 1. CLI 入口的位置选择

macOS 的登录 shell 由 `/usr/libexec/path_helper` 读取 `/etc/paths` 与
`/etc/paths.d/*` 生成 PATH，`/usr/local/bin` 在其中，因此在
`/usr/local/bin` 建软链**等价于「已经配好 PATH」**，对 Intel 与 Apple
Silicon 都成立，也不依赖用户的 shell 类型。

按顺序选择第一个可写目录：

1. `/usr/local/bin`（首选）
2. `$HOME/.local/bin`（仅当 `/usr/local/bin` 不可写；该目录默认不在 PATH，
   安装器必须显式提示需要 `export PATH="$HOME/.local/bin:$PATH"`）
3. 都没有可写目录：跳过建链，打印手动 `sudo ln -sf ...` 的命令，安装整体
   仍算成功（CLI 不可用不应让 GUI 装不上）

建链一律用 `ln -sf` 指向 `/Applications/yi-agent.app/Contents/MacOS/yi-agent`，
**不拷贝**。软链指向固定路径，App 升级后自动指向新版本，避免版本漂移。

### 2. 建链后的校验与冲突提示

建链后读取 `command -v yi-agent`：

- 命中目标链接：成功。
- 命中别的可执行文件（本机就是这种情况：`/opt/homebrew/bin/yi-agent` 排在
  `/usr/local/bin` 之前）或未命中：打印警告，说明当前实际生效的是哪个路径、
  建议如何取舍（卸载 brew/npm 版本，或改用 `$HOME/.local/bin`）。安装器**不**
  替用户卸载或修改别人的安装。
- 若落在 `$HOME/.local/bin` 且该目录不在当前 PATH：打印需要添加的
  `export` 行，但不写入任何 rc 文件。

### 3. 升级语义：原子替换

替换 App 的顺序改为「先拷到暂存位置，成功后移入」：

```
STAGE=/Applications/.yi-agent.app.new
ditto "$MOUNT/yi-agent.app" "$STAGE"      # 失败 → 旧版本原样保留
[ -d "$DEST" ] && mv "$DEST" "$DEST.old"
mv "$STAGE" "$DEST"
rm -rf "$DEST.old"
```

任一步失败都不会丢失旧版本。清理动作放在 trap 里，异常退出也移除暂存目录。

### 4. 升级时自动退出运行中的 GUI

检测用完整路径匹配，**不用** `pgrep -x desktop`（实测进程的 `comm` 是
`/Applications/yi-agent.app/Contents/MacOS/desktop`，短名为空）：

```
ps -Ao pid=,comm= | grep -F "$DEST/Contents/MacOS/" 
```

匹配 `Contents/MacOS/` 这一层可同时覆盖 GUI（`desktop`）与其 sidecar
（`yi-agent`），且不会误伤开发构建（`.worktrees/**/target/debug/yi-agent`）。

退出流程：

1. `osascript -e 'tell application "yi-agent" to quit'`（用 bundle 名
   `yi-agent`，来自 `CFBundleName`；不是 `desktop`）
2. 轮询 pid 消失，最多 5 秒（每 200ms 一次）
3. 仍在则 `kill <pid>`，再等 2 秒；仍失败则**中止升级**并保留旧版本，
   提示用户手动退出后重跑

sidecar 是 GUI 的子进程（实测 `ppid` 指向 GUI），GUI 退出后随之结束，
不单独处理。

### 5. 升级时不动 TUI 进程

终端里正在跑的旧版 TUI 保持运行：macOS 的 unlink 语义让旧进程继续用已替换
的 inode 跑完当前回合，下次启动自然是新版。安装器只统计并提示数量，不中断
用户的进行中回合。

### 6. 版本展示

安装前读取旧版本，安装后读取新版本（`/usr/libexec/PlistBuddy -c 'Print
:CFBundleShortVersionString'`），分别打印：

- 首次安装：`已安装 yi-agent 0.1.7`
- 升级：`检测到已安装版本 0.1.6 → 升级到 0.1.7`

并对内置 CLI 执行 `--version`，确认软链实际可执行。

### 7. 可测试性

`install.sh` 需要能被单测驱动，因此：

- 把逻辑拆成纯函数（`pick_bin_dir`、`detect_running_pids`、
  `remove_quarantine`、`verify_cli`），主流程放在 `main`；
- 目录通过环境变量覆盖（`YI_AGENT_APP_PATH`、`YI_AGENT_BIN_DIR`），默认值
  仍是 `/Applications/yi-agent.app` 与自动探测结果；
- 末尾以 `if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi` 收口，
  使测试可以 `source` 后只测函数，不触碰 `/Applications`；
- 新增 `desktop/scripts/test_install_sh.sh`，在 `mktemp -d` 里构造假的 App
  目录与假的 CLI 脚本，覆盖：目录选择优先级、冲突检测、版本解析、原子替换
  （模拟 `ditto` 失败时旧目录仍在）、`rm -rf` 不越过预期路径。

测试不依赖真实 dmg，也不写 `/Applications`。

### 8. Release 文案

`desktop/scripts/release-dmg.sh` 的 `DESCRIPTION` 中，把「装了 App 就无需再装
这个」替换为可执行说明：

- TUI 入口：`/usr/local/bin/yi-agent -> /Applications/yi-agent.app/Contents/MacOS/yi-agent`
- 升级：重跑 `bash install.sh <新 dmg>`，脚本会自动退出 GUI 并保留配置
- 配置共享：`~/.yi-agent/.env` 两边共用，无需配两遍
- 已知问题与兜底：`subagent delegation unavailable` 时，到对应项目目录执行
  `pkill -f "yi-agent daemon serve"`（旧 daemon 不认新版 `Stop`）
- 不支持降级（schema 只升不降，`repository.rs:3829`）

## 边界与错误处理

| 情况 | 行为 |
| --- | --- |
| dmg 内无 `yi-agent.app` | 报错退出，不动已安装版本 |
| `/Applications/yi-agent.app` 存在但只读/被锁 | 尝试退出 GUI 后重试一次；仍失败则报错退出 |
| GUI 拒绝退出 | 先 `kill`，仍失败则中止并提示手动退出 |
| `/usr/local/bin` 不可写 | 回退 `$HOME/.local/bin`；仍不可写则跳过并给出手动命令 |
| 目标位置是断链 | `ln -sf` 直接覆盖 |
| 目标位置是别的可执行文件 | 覆盖为软链（这是本次安装的目标），并在结束时提示 PATH 里可能有更靠前的同名程序 |
| `xattr` 失败 | 只警告，不中断（与现状一致） |

## 验收判据

1. `bash desktop/scripts/test_install_sh.sh` 全绿。
2. 在干净机器（或删掉 `/Applications/yi-agent.app` 后）执行
   `bash install.sh <dmg>`，随后新开终端 `yi-agent --version` 输出与 dmg
   版本一致。
3. 在 App 运行中执行升级安装：App 被自动退出、替换完成、重新打开后版本为
   新版本，且 `~/.yi-agent/.env` 未变化。
4. 安装前后 `/usr/local/bin/yi-agent` 始终是指向
   `/Applications/yi-agent.app/Contents/MacOS/yi-agent` 的软链。
5. 仓库中不存在安装器对 `~/.zshrc` / `~/.zprofile` 的任何写入。