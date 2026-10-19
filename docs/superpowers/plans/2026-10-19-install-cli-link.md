# macOS 安装器：CLI 入口、原地升级与 daemon 协调 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让只装 App 的用户能直接用内置 CLI，并把升级做成「一条命令、自动退出 GUI、清理旧 daemon、失败不丢旧版」。

**Architecture:** 把 `desktop/packaging/install.sh` 从一段顺序脚本重写为「可 source 的纯函数库 + `main` 编排」，所有外部副作用（`ps`/`lsof`/`launchctl`/路径）经环境变量注入，使 `desktop/scripts/test_install_sh.sh` 能在临时目录里驱动每个函数而完全不碰 `/Applications` 和真实 launchd。

**Tech Stack:** Bash 3.2（macOS 自带）、`plutil`、`hdiutil`、`ditto`、`xattr`、`launchctl`、`lsof`、`osascript`。

## Global Constraints

- 目标平台仅 macOS arm64；不使用 Bash 4 特性（`declare -A`、`mapfile`、`${var,,}`）。
- 安装位置固定 `/Applications/yi-agent.app`，不降级到 `~/Applications`。
- CLI 入口只建软链，不拷贝：`ln -sf <app>/Contents/MacOS/yi-agent <bin>/yi-agent`。
- 源目录优先 `/usr/local/bin`，其次 `$HOME/.local/bin`，都不可写则跳过建链但安装仍成功。
- 绝不写入 `~/.zshrc`、`~/.zprofile` 或任何 shell 配置。
- 绝不按 LaunchAgent 的 label 名判断，只按 plist 内容判断。
- daemon 的终止判据必须同时满足：argv 为 `daemon serve` **且** 真实可执行文件（`lsof` 的 `txt` 行）属于本次安装的 App。
- 所有用户可见文本用中文；错误写 stderr。
- 仓库根：本 worktree。所有命令在 worktree 根执行。

---

### Task 1: 把 install.sh 重构为可 source 的函数库

**Files:**
- Modify: `desktop/packaging/install.sh`（全量重写，53 行 → 约 90 行）
- Test: `desktop/scripts/test_install_sh.sh`（新建）

**Interfaces:**
- Consumes: 无
- Produces:
  - `app_path()` → 打印 App 路径；读 `YI_AGENT_APP_PATH`，默认 `/Applications/yi-agent.app`
  - `bin_candidates()` → 每行一个候选 bin 目录
  - `main "$@"` → 编排入口
  - 文件末尾 `if [ "${BASH_SOURCE[0]}" = "$0" ]; then set -euo pipefail; main "$@"; fi`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/scripts/test_install_sh.sh`：

```bash
#!/usr/bin/env bash
# install.sh 的单元测试。不碰 /Applications，不调真实 launchctl。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="$HERE/../packaging/install.sh"
PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf 'ok   - %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL - %s\n' "$1" >&2; }

assert_eq() { # assert_eq <name> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else fail "$1 (expected [$2], got [$3])"; fi
}

# shellcheck source=../packaging/install.sh
. "$INSTALLER"

assert_eq "app_path honors override" \
  "/tmp/fake/yi-agent.app" \
  "$(YI_AGENT_APP_PATH=/tmp/fake/yi-agent.app app_path)"

assert_eq "app_path defaults to /Applications" \
  "/Applications/yi-agent.app" \
  "$(app_path)"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`app_path: command not found`。

- [ ] **Step 3: 重写 install.sh 的骨架**

把 `desktop/packaging/install.sh` 全量替换为：

```bash
#!/usr/bin/env bash
#
# yi-agent 桌面 App 安装器（macOS, arm64）。
# 未签名产物：挂载 dmg → 原子替换 /Applications → 清 quarantine →
# 建 CLI 软链 → 清理旧 runtime daemon。
#
# 本文件既可执行，也可被测试 source：source 时只定义函数，不产生副作用。

APP_NAME="yi-agent.app"

app_path() {
  printf '%s\n' "${YI_AGENT_APP_PATH:-/Applications/$APP_NAME}"
}

bin_candidates() {
  printf '%s\n' "/usr/local/bin"
  printf '%s\n' "$HOME/.local/bin"
}

main() {
  set -euo pipefail

  local dmg="${1:-}"
  if [ -z "$dmg" ]; then
    dmg="$(ls -t "$HOME/Downloads/"yi-agent_*_aarch64.dmg 2>/dev/null | head -1 || true)"
  fi
  if [ -z "$dmg" ] || [ ! -f "$dmg" ]; then
    echo "用法: bash install.sh /path/to/yi-agent_x.y.z_aarch64.dmg" >&2
    echo "（未传参时会在 ~/Downloads 找 yi-agent_*_aarch64.dmg）" >&2
    return 1
  fi

  echo "当前安装器版本：$(basename "$dmg")"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  main "$@"
fi
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `2 passed, 0 failed`，退出码 0。

- [ ] **Step 5: 确认脚本仍可直接执行**

Run: `bash desktop/packaging/install.sh 2>&1 | head -2`
Expected: 输出「用法: bash install.sh ...」，退出码 1。

- [ ] **Step 6: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "refactor(installer): make install.sh sourceable for testing"
```

---

### Task 2: 版本读取与展示

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `app_path()`
- Produces:
  - `app_version <app-path>` → 打印 `CFBundleShortVersionString`，失败打印空串
  - `cli_version <app-path>` → 执行 `<app>/Contents/MacOS/yi-agent --version`，打印结果，失败打印空串
  - `version_banner <old> <new>` → 打印「首次安装」或「升级」提示

- [ ] **Step 1: 写失败的测试**

在 `desktop/scripts/test_install_sh.sh` 的 `printf '\n%d passed'` 之前插入：

```bash
TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

mk_app() { # mk_app <dir> <version> [cli-exit-code]
  local dir="$1" version="$2" code="${3:-0}"
  mkdir -p "$dir/Contents/MacOS"
  cat > "$dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>$version</string>
</dict></plist>
PLIST
  cat > "$dir/Contents/MacOS/yi-agent" <<CLI
#!/bin/sh
echo "yi-agent $version"
exit $code
CLI
  chmod +x "$dir/Contents/MacOS/yi-agent"
}

APP="$TMPROOT/Fake.app"
mk_app "$APP" "1.2.3"

assert_eq "app_version reads plist" "1.2.3" "$(app_version "$APP")"
assert_eq "cli_version runs binary" "yi-agent 1.2.3" "$(cli_version "$APP")"
assert_eq "app_version missing plist is empty" "" \
  "$(app_version "$TMPROOT/NoSuch.app" 2>/dev/null)"

assert_eq "version_banner first install" \
  "已安装 yi-agent 1.2.3" \
  "$(version_banner "" "1.2.3")"
assert_eq "version_banner upgrade" \
  "检测到已安装版本 1.2.2 → 升级到 1.2.3" \
  "$(version_banner "1.2.2" "1.2.3")"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`app_version: command not found`。

- [ ] **Step 3: 实现三个函数**

在 `bin_candidates()` 之后插入：

```bash
# 读 App 的 CFBundleShortVersionString；失败时打印空串（调用方据此判断）。
app_version() {
  local plist="$1/Contents/Info.plist"
  [ -f "$plist" ] || return 0
  plutil -extract CFBundleShortVersionString raw -o - "$plist" 2>/dev/null || true
}

# 跑内置 CLI 的 --version；失败时打印空串。
cli_version() {
  local cli="$1/Contents/MacOS/yi-agent"
  [ -x "$cli" ] || return 0
  "$cli" --version 2>/dev/null | head -1 || true
}

version_banner() {
  local old="$1" new="$2"
  if [ -z "$old" ]; then
    printf '已安装 yi-agent %s\n' "$new"
  else
    printf '检测到已安装版本 %s → 升级到 %s\n' "$old" "$new"
  fi
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `7 passed, 0 failed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): read and display app versions"
```

---

### Task 3: CLI 软链：选目录、建链、校验冲突

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `app_path()`
- Produces:
  - `pick_bin_dir` → 打印第一个可写候选目录；都不可写时返回 1
  - `link_cli <bin-dir>` → 建立软链并返回 0；目标 CLI 不存在时返回 1
  - `link_status <bin-dir>` → 打印 `ok` / `shadowed:<path>` / `missing`

- [ ] **Step 1: 写失败的测试**

插入到测试脚本的汇总行之前：

```bash
# --- pick_bin_dir ---
BIN1="$TMPROOT/bin-a"; BIN2="$TMPROOT/bin-b"
mkdir -p "$BIN1" "$BIN2"
assert_eq "pick_bin_dir prefers first writable" \
  "$BIN1" \
  "$(YI_AGENT_BIN_DIRS=$BIN1:$BIN2 pick_bin_dir)"

NOPERM="$TMPROOT/bin-noperm"
mkdir -p "$NOPERM"; chmod 500 "$NOPERM"
assert_eq "pick_bin_dir skips unwritable" \
  "$BIN2" \
  "$(YI_AGENT_BIN_DIRS=$NOPERM:$BIN2 pick_bin_dir)"

# 注意：这条断言必须在恢复权限之前跑，否则目录又可写了。
assert_eq "pick_bin_dir fails when none writable" \
  "rc=1" \
  "$(YI_AGENT_BIN_DIRS=$NOPERM pick_bin_dir >/dev/null 2>&1; echo "rc=$?")"
chmod 700 "$NOPERM"

# 覆盖为空串表示「一个候选都没有」，不回落默认目录。
if [ "$(id -u)" -ne 0 ]; then
  assert_eq "pick_bin_dir fails on empty list" \
    "rc=1" "$(YI_AGENT_BIN_DIRS="" pick_bin_dir >/dev/null 2>&1; echo "rc=$?")"
fi

# --- link_cli ---
LINKDIR="$TMPROOT/linkbin"; mkdir -p "$LINKDIR"
link_cli "$APP" "$LINKDIR"
assert_eq "link_cli creates symlink to app cli" \
  "$APP/Contents/MacOS/yi-agent" \
  "$(readlink "$LINKDIR/yi-agent")"
# command -v 走的是当前 PATH，必须把测试目录放进去才能命中。
assert_eq "link_status reports ok" \
  "ok" "$(PATH="$LINKDIR:$PATH" link_status "$APP" "$LINKDIR")"

assert_eq "link_cli fails without cli" \
  "rc=1" "$(link_cli "$TMPROOT/NoSuch.app" "$LINKDIR" >/dev/null 2>&1; echo "rc=$?")"

# 模拟 PATH 里更靠前的同名程序
SHADOW="$TMPROOT/shadow"; mkdir -p "$SHADOW"
printf '#!/bin/sh\nexit 0\n' > "$SHADOW/yi-agent"; chmod +x "$SHADOW/yi-agent"
assert_eq "link_status detects shadowing" \
  "shadowed:$SHADOW/yi-agent" \
  "$(PATH="$SHADOW:$LINKDIR:$PATH" link_status "$APP" "$LINKDIR")"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`pick_bin_dir: command not found`。

- [ ] **Step 3: 实现三个函数**

替换 `bin_candidates()`，并新增两个函数：

```bash
# 候选目录：测试可经 YI_AGENT_BIN_DIRS（冒号分隔）覆盖。
# 用 ${VAR+set} 判断「是否设置」而非「是否非空」，这样置空即表示无候选。
bin_candidates() {
  if [ -n "${YI_AGENT_BIN_DIRS+set}" ]; then
    case "$YI_AGENT_BIN_DIRS" in
      "") return 0 ;;
    esac
    printf '%s\n' "$YI_AGENT_BIN_DIRS" | tr ':' '\n'
    return 0
  fi
  printf '%s\n' "/usr/local/bin"
  printf '%s\n' "$HOME/.local/bin"
}

# 第一个可写候选目录。都没有则返回 1（安装继续，只跳过建链）。
pick_bin_dir() {
  local dir
  while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    if [ -w "$dir" ]; then
      printf '%s\n' "$dir"
      return 0
    fi
    if [ ! -d "$dir" ] && mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; then
      printf '%s\n' "$dir"
      return 0
    fi
  done <<EOF
$(bin_candidates)
EOF
  return 1
}

# 建软链。目标 CLI 不存在时不建，返回 1。
link_cli() {
  local app="$1" bin_dir="$2"
  local cli="$app/Contents/MacOS/yi-agent"
  if [ ! -x "$cli" ]; then
    echo "ERROR: 未找到内置 CLI：$cli" >&2
    return 1
  fi
  ln -sfn "$cli" "$bin_dir/yi-agent"
}

# 判断 PATH 上实际命中的是不是我们这条软链。
link_status() {
  local app="$1" bin_dir="$2"
  local want="$bin_dir/yi-agent"
  if [ ! -L "$want" ]; then
    printf 'missing\n'
    return 0
  fi
  local hit
  hit="$(command -v yi-agent 2>/dev/null || true)"
  if [ -z "$hit" ]; then
    printf 'missing\n'
  elif [ "$hit" = "$want" ]; then
    printf 'ok\n'
  else
    printf 'shadowed:%s\n' "$hit"
  fi
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `15 passed, 0 failed`（非 root 时）。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): link bundled CLI into PATH"
```

---

### Task 4: 原子替换 App

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `app_path()`
- Produces:
  - `replace_app <src-app> <dest-app>` → 原子替换；失败时保留原目录并返回 1
  - `copy_runner <src> <dest>` → 可经 `YI_AGENT_COPY_CMD` 覆盖，默认 `ditto`

- [ ] **Step 1: 写失败的测试**

插入到汇总行之前：

```bash
# --- replace_app ---
SRC="$TMPROOT/Src.app"; mk_app "$SRC" "2.0.0"
DST="$TMPROOT/Dst.app";  mk_app "$DST" "1.0.0"

replace_app "$SRC" "$DST"
assert_eq "replace_app installs new version" "2.0.0" "$(app_version "$DST")"
assert_eq "replace_app leaves no staging dir" "absent" \
  "$([ -e "$DST.new" ] && echo present || echo absent)"
assert_eq "replace_app leaves no backup dir" "absent" \
  "$([ -e "$DST.old" ] && echo present || echo absent)"

# 模拟拷贝失败：必须保留旧版本
FAILSRC="$TMPROOT/Fail.app"; mk_app "$FAILSRC" "3.0.0"
assert_eq "replace_app reports failure" \
  "rc=1" "$(YI_AGENT_COPY_CMD=false replace_app "$FAILSRC" "$DST" >/dev/null 2>&1; echo "rc=$?")"
assert_eq "replace_app keeps old version on failure" "2.0.0" "$(app_version "$DST")"
assert_eq "replace_app cleans staging on failure" "absent" \
  "$([ -e "$DST.new" ] && echo present || echo absent)"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`replace_app: command not found`。

- [ ] **Step 3: 实现**

插入到 `link_status()` 之后：

```bash
# 注入的拷贝命令可能带参数，故用 eval。
copy_runner() {
  local src="$1" dest="$2"
  if [ -n "${YI_AGENT_COPY_CMD:-}" ]; then
    eval "$YI_AGENT_COPY_CMD \"$src\" \"$dest\""
  else
    ditto "$src" "$dest"
  fi
}

# 原子替换：先拷到 <dest>.new，成功后才替换。任何一步失败都保留旧目录。
replace_app() {
  local src="$1" dest="$2"
  local stage="$dest.new"
  local backup="$dest.old"

  rm -rf "$stage" "$backup"
  if ! copy_runner "$src" "$stage"; then
    echo "ERROR: 拷贝到暂存目录失败，已保留原版本" >&2
    rm -rf "$stage"
    return 1
  fi
  if [ -e "$dest" ]; then
    mv "$dest" "$backup" || { rm -rf "$stage"; return 1; }
  fi
  if ! mv "$stage" "$dest"; then
    echo "ERROR: 替换失败，正在恢复原版本" >&2
    [ -e "$backup" ] && mv "$backup" "$dest"
    rm -rf "$stage"
    return 1
  fi
  rm -rf "$backup"
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `21 passed, 0 failed`（非 root 时）。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): replace the app bundle atomically"
```

---

### Task 5: 检测并退出运行中的 GUI

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `app_path()`
- Produces:
  - `ps_lines` → 每行 `pid command`；可用 `YI_AGENT_PS_CMD` 注入
  - `gui_pids <app>` → 每行一个 pid
  - `quit_gui <pid> <app>` → 尝试 osascript 后 kill；成功返回 0
  - `osascript_cmd` → 可用 `YI_AGENT_OSASCRIPT_CMD` 注入

- [ ] **Step 1: 写失败的测试**

```bash
# --- gui_pids ---
APPDIR="/tmp/FakeHome/Applications/yi-agent.app"
PS_FIXTURE="$TMPROOT/ps.txt"
cat > "$PS_FIXTURE" <<EOF
  111 $APPDIR/Contents/MacOS/desktop
  222 $APPDIR/Contents/MacOS/yi-agent app-server --listen stdio://
  333 /Users/dev/proj/.worktrees/x/target/debug/yi-agent daemon serve
  444 /Users/dev/proj/.worktrees/x/target/debug/yi-agent resident watch
EOF
assert_eq "gui_pids finds only the GUI" "111" \
  "$(YI_AGENT_PS_CMD="cat $PS_FIXTURE" gui_pids "/tmp/FakeHome/Applications/yi-agent.app" | tr '\n' ' ' | sed 's/ $//')"

# --- quit_gui：osascript 成功时不应再 kill ---
# 注入的 kill 命令写成文件，便于断言「有没有被调用」。
KILLLOG="$TMPROOT/killelog.txt"
assert_eq "quit_gui succeeds via osascript" "rc=0" \
  "$(YI_AGENT_OSASCRIPT_CMD=true \
      YI_AGENT_KILL_CMD="echo \$1 >> $KILLLOG" \
      YI_AGENT_PID_ALIVE_CMD=false \
      quit_gui 111 "/tmp/FakeHome/Applications/yi-agent.app" >/dev/null 2>&1; echo "rc=$?")"
assert_eq "quit_gui does not kill when osascript worked" "absent" \
  "$([ -e "$KILLLOG" ] && echo present || echo absent)"

# osascript 失败且进程一直存活时，必须走到 SIGTERM
assert_eq "quit_gui falls back to kill" "rc=1" \
  "$(YI_AGENT_OSASCRIPT_CMD=false \
      YI_AGENT_KILL_CMD="echo \$1 >> $KILLLOG" \
      YI_AGENT_PID_ALIVE_CMD=true \
      YI_AGENT_SLEEP_CMD=true \
      quit_gui 222 "/tmp/FakeHome/Applications/yi-agent.app" >/dev/null 2>&1; echo "rc=$?")"
assert_eq "quit_gui killed the stubborn pid" "222" "$(cat "$KILLLOG" 2>/dev/null)"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`gui_pids: command not found`。

- [ ] **Step 3: 实现**

```bash
ps_lines() {
  if [ -n "${YI_AGENT_PS_CMD:-}" ]; then
    eval "$YI_AGENT_PS_CMD"
  else
    ps -Aw -o pid=,command=
  fi
}

# 匹配 Contents/MacOS/desktop（GUI 可执行文件名由 Cargo 包名决定）。
# 不用 pgrep、不用 comm：两者在本机均不可靠，见 spec「关键事实」。
gui_pids() {
  local app="$1"
  ps_lines | awk -v want="$app/Contents/MacOS/desktop" '{
    pid = $1
    sub(/^[ \t]+/, "", $0)
    cmd = $0
    sub(/^[0-9]+[ \t]+/, "", cmd)
    if (cmd == want || index(cmd, want " ") == 1) print pid
  }'
}

# 下面三个包装器都支持「注入带参数的命令」，故用 eval。
# 生产路径下它们分别等价于 `osascript -e ...`、`kill <pid>`、`lsof -p <pid>`。
osascript_cmd() {
  printf '%s\n' "${YI_AGENT_OSASCRIPT_CMD:-}"
}

sleep_cmd() {
  printf '%s\n' "${YI_AGENT_SLEEP_CMD:-}"
}

kill_pid() {
  local pid="$1"
  if [ -n "${YI_AGENT_KILL_CMD:-}" ]; then
    eval "$YI_AGENT_KILL_CMD"
  else
    kill "$pid" 2>/dev/null || true
  fi
}

# 等待 <seconds>；测试可置空以跳过真实 sleep。
wait_seconds() {
  local seconds="$1"
  if [ -n "$(sleep_cmd)" ]; then
    eval "$(sleep_cmd)"
  else
    sleep "$seconds"
  fi
}

pid_alive() {
  local pid="$1"
  if [ -n "${YI_AGENT_PID_ALIVE_CMD:-}" ]; then
    eval "$YI_AGENT_PID_ALIVE_CMD"
  else
    kill -0 "$pid" 2>/dev/null
  fi
}

# 先请求 GUI 自己退出，等 5 秒；仍在则 SIGTERM，再等 2 秒。
quit_gui() {
  local pid="$1"
  if [ -n "$(osascript_cmd)" ]; then
    eval "$(osascript_cmd)" || true
  else
    osascript -e 'tell application "yi-agent" to quit' >/dev/null 2>&1 || true
  fi

  local i
  for i in $(seq 1 25); do
    pid_alive "$pid" || return 0
    wait_seconds 0.2
  done

  kill_pid "$pid"
  for i in $(seq 1 10); do
    pid_alive "$pid" || return 0
    wait_seconds 0.2
  done
  echo "ERROR: GUI 进程 $pid 未能退出" >&2
  return 1
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `26 passed, 0 failed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): detect and quit the running GUI"
```

---

### Task 6: 识别属于本次安装的 runtime daemon

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `ps_lines()`, `app_path()`
- Produces:
  - `real_exe <pid>` → 经 `lsof` 打印真实可执行文件路径；不可得时打印空串
  - `daemon_pids <app>` → 每行一个属于该 App 的 `daemon serve` pid
  - `lsof_for_pid <pid>` → 可用 `YI_AGENT_LSOF_CMD` 注入
  - `is_argv_daemon_serve <command-line>` → 0/1

- [ ] **Step 1: 写失败的测试**

```bash
# --- is_argv_daemon_serve ---
assert_eq "daemon serve matches" "yes" \
  "$(is_argv_daemon_serve "/x/yi-agent daemon serve" && echo yes || echo no)"
assert_eq "TUI does not match" "no" \
  "$(is_argv_daemon_serve "/x/yi-agent" && echo yes || echo no)"
assert_eq "app-server does not match" "no" \
  "$(is_argv_daemon_serve "/x/yi-agent app-server --listen stdio://" && echo yes || echo no)"
assert_eq "boards watch does not match" "no" \
  "$(is_argv_daemon_serve "/x/yi-agent boards watch" && echo yes || echo no)"

# --- daemon_pids ---
APPDIR2="/Applications/yi-agent.app"
PS2="$TMPROOT/ps2.txt"
cat > "$PS2" <<EOF
  501 $APPDIR2/Contents/MacOS/yi-agent daemon serve
  502 $APPDIR2/Contents/MacOS/yi-agent app-server --listen stdio://
  503 /Users/dev/proj/.worktrees/x/target/debug/yi-agent daemon serve
  504 $APPDIR2/Contents/MacOS/yi-agent
EOF
LSOF2="$TMPROOT/lsof2.txt"
cat > "$LSOF2" <<'EOF'
COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
yi-agent 501 u txt REG 1,15 42 100 /Applications/yi-agent.app/Contents/MacOS/yi-agent
yi-agent 503 u txt REG 1,15 42 101 /Users/dev/proj/.worktrees/x/target/debug/yi-agent
EOF
assert_eq "daemon_pids keeps only app-owned daemon" "501" \
  "$(YI_AGENT_PS_CMD="cat $PS2" \
     YI_AGENT_LSOF_CMD="cat $LSOF2" \
     daemon_pids "$APPDIR2" | tr '\n' ' ' | sed 's/ $//')"

# 软链启动：comm 是软链路径，但 lsof 给出真实 inode 路径
LSOF3="$TMPROOT/lsof3.txt"
cat > "$LSOF3" <<'EOF'
COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
yi-agent 601 u txt REG 1,15 42 100 /Applications/yi-agent.app/Contents/MacOS/yi-agent
EOF
PS3="$TMPROOT/ps3.txt"
cat > "$PS3" <<'EOF'
  601 /usr/local/bin/yi-agent daemon serve
EOF
assert_eq "daemon_pids follows symlinked exe" "601" \
  "$(YI_AGENT_PS_CMD="cat $PS3" \
     YI_AGENT_LSOF_CMD="cat $LSOF3" \
     daemon_pids "$APPDIR2" | tr '\n' ' ' | sed 's/ $//')"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`is_argv_daemon_serve: command not found`。

- [ ] **Step 3: 实现**

```bash
lsof_for_pid() {
  local pid="$1"
  if [ -n "${YI_AGENT_LSOF_CMD:-}" ]; then
    eval "$YI_AGENT_LSOF_CMD" | awk -v p="$pid" '$2 == p'
  else
    lsof -p "$pid" 2>/dev/null
  fi
}

# argv 里第二个词必须是 daemon、第三个必须是 serve。
# 用 shell 分词即可：命令行由内核给出，不含需要引号解析的语法。
is_argv_daemon_serve() {
  # shellcheck disable=SC2086
  set -- $1
  [ $# -ge 3 ] || return 1
  # $1 是可执行文件
  shift
  [ "$1" = "daemon" ] && [ "$2" = "serve" ]
}

# 真实可执行文件（inode 路径）。软链启动时 comm 只显示软链路径，
# 故必须走 lsof 的 txt 行。
real_exe() {
  local pid="$1"
  lsof_for_pid "$pid" |
    awk '$4 == "txt" && $NF ~ /\/yi-agent$/ {print $NF; exit}'
}

# 属于 <app> 的 runtime daemon。
# 两个条件同时满足才返回：argv 是 daemon serve；真实 exe 是该 App 的内置 CLI。
daemon_pids() {
  local app="$1"
  local want="$app/Contents/MacOS/yi-agent"
  local pid cmd
  ps_lines | while read -r pid cmd; do
    [ -n "$pid" ] || continue
    is_argv_daemon_serve "$cmd" || continue
    local exe
    exe="$(real_exe "$pid")"
    [ "$exe" = "$want" ] || continue
    printf '%s\n' "$pid"
  done
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `32 passed, 0 failed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): identify daemons owned by this install"
```

---

### Task 7: LaunchAgent 协调与死配置清理

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `app_path()`
- Produces:
  - `plist_label <plist>` / `plist_exe <plist>`
  - `is_loaded <label>` → 0/1
  - `watchman_plists` → 每行一个 Label 以 `ai.yi-agent.` 开头的 plist
  - `restart_own_watchman <app>` → 停、重写、仅在原本已加载时重新加载
  - `dead_plists <app>` → 三重失效的 plist
  - `launchctl_run <args...>` → 可用 `YI_AGENT_LAUNCHCTL_CMD` 注入

- [ ] **Step 1: 写失败的测试**

```bash
# --- plist 解析与加载状态 ---
LA="$TMPROOT/LaunchAgents"; mkdir -p "$LA"
mk_plist() { # mk_plist <file> <label> <exe>
  cat > "$1" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key><string>$2</string>
<key>ProgramArguments</key><array>
<string>$3</string><string>boards</string><string>watch</string>
</array>
</dict></plist>
PLIST
}
P_OWN="$LA/ai.yi-agent.board-watchman.plist"
mk_plist "$P_OWN" "ai.yi-agent.board-watchman" "/Applications/yi-agent.app/Contents/MacOS/yi-agent"

assert_eq "plist_label" "ai.yi-agent.board-watchman" "$(plist_label "$P_OWN")"
assert_eq "plist_exe" "/Applications/yi-agent.app/Contents/MacOS/yi-agent" "$(plist_exe "$P_OWN")"

assert_eq "is_loaded true when launchctl exits 0" "yes" \
  "$(YI_AGENT_LAUNCHCTL_CMD=true is_loaded x && echo yes || echo no)"
assert_eq "is_loaded false when launchctl exits non-zero" "no" \
  "$(YI_AGENT_LAUNCHCTL_CMD="false" is_loaded x && echo yes || echo no)"

assert_eq "watchman_plists lists only yi-agent labels" \
  "ai.yi-agent.board-watchman" \
  "$(YI_AGENT_LAUNCH_AGENTS_DIR="$LA" watchman_plists | while read -r p; do plist_label "$p"; done | tr '\n' ' ' | sed 's/ $//')"

# --- dead_plists：三重失效才删 ---
mk_plist "$LA/ai.yi-agent.gone.plist" "ai.yi-agent.gone" "/nowhere/yi-agent"
mk_plist "$LA/ai.yi-agent.dev.plist"  "ai.yi-agent.dev"  "$TMPROOT/devbin"
mkdir -p "$TMPROOT/devbin"; printf '#!/bin/sh\n' > "$TMPROOT/devbin/yi-agent"; chmod +x "$TMPROOT/devbin/yi-agent"

assert_eq "dead_plists removes only fully-dead entries" \
  "ai.yi-agent.gone" \
  "$(YI_AGENT_LAUNCH_AGENTS_DIR="$LA" YI_AGENT_LAUNCHCTL_CMD=false \
     dead_plists "/Applications/yi-agent.app" | while read -r p; do plist_label "$p"; done | tr '\n' ' ' | sed 's/ $//')"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`plist_label: command not found`。

- [ ] **Step 3: 实现**

```bash
# 注入的命令可能带参数，故用 eval；生产路径等价于 `launchctl <args...>`。
launchctl_run() {
  if [ -n "${YI_AGENT_LAUNCHCTL_CMD:-}" ]; then
    eval "$YI_AGENT_LAUNCHCTL_CMD \"$*\""
  else
    launchctl "$@"
  fi
}

launch_agents_dir() {
  printf '%s\n' "${YI_AGENT_LAUNCH_AGENTS_DIR:-$HOME/Library/LaunchAgents}"
}

plist_label() {
  plutil -extract Label raw -o - "$1" 2>/dev/null || true
}

plist_exe() {
  plutil -extract ProgramArguments.0 raw -o - "$1" 2>/dev/null || true
}

# launchctl print 成功（exit 0）表示该服务已加载；实测未加载为 113。
is_loaded() {
  local label="$1"
  launchctl_run print "gui/$(id -u)/$label" >/dev/null 2>&1
}

watchman_plists() {
  local dir plist label
  dir="$(launch_agents_dir)"
  [ -d "$dir" ] || return 0
  for plist in "$dir"/*.plist; do
    [ -f "$plist" ] || continue
    label="$(plist_label "$plist")"
    case "$label" in
      ai.yi-agent.*) printf '%s\n' "$plist" ;;
    esac
  done
}

# 只处理「指向本 App 且原本已加载」的那一份。未加载的不替你开服务。
restart_own_watchman() {
  local app="$1"
  local want="$app/Contents/MacOS/yi-agent"
  local plist label loaded
  while IFS= read -r plist; do
    [ "$(plist_exe "$plist")" = "$want" ] || continue
    label="$(plist_label "$plist")"
    loaded=no
    is_loaded "$label" && loaded=yes
    if [ "$loaded" = no ]; then
      echo "提示：$label 未加载，本次不改动。" >&2
      continue
    fi
    launchctl_run bootout "gui/$(id -u)/$label" >/dev/null 2>&1 || \
      echo "WARN: bootout $label 失败，继续" >&2
    # plist 内容不变（exe 路径固定），重写只是为了确保文件是最新格式。
    if launchctl_run bootstrap "gui/$(id -u)" "$plist" >/dev/null 2>&1; then
      echo "已重启常驻值守：$label"
    else
      echo "WARN: 重新加载 $label 失败；该服务当前未运行" >&2
    fi
  done <<EOF
$(watchman_plists)
EOF
}

# 三重失效才删：未加载 + 不指向本 App + 指向的可执行文件不存在。
dead_plists() {
  local app="$1"
  local want="$app/Contents/MacOS/yi-agent"
  local plist label exe
  while IFS= read -r plist; do
    label="$(plist_label "$plist")"
    is_loaded "$label" && continue
    exe="$(plist_exe "$plist")"
    [ "$exe" = "$want" ] && continue
    [ -n "$exe" ] && [ -x "$exe" ] && continue
    printf '%s\n' "$plist"
  done <<EOF
$(watchman_plists)
EOF
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `38 passed, 0 failed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): coordinate the launchd watchman"
```

---

### Task 8: 交互确认与 daemon 终止

**Files:**
- Modify: `desktop/packaging/install.sh`
- Test: `desktop/scripts/test_install_sh.sh`

**Interfaces:**
- Consumes: `daemon_pids()`
- Produces:
  - `confirm_stop <count> <answer>` → 纯函数；`count=0` 或答案非 `n` 时返回 0
  - `stop_daemons <app>` → 逐个 SIGTERM，返回存活数量
  - `ask_answer` → 可经 `YI_AGENT_ANSWER` 注入

- [ ] **Step 1: 写失败的测试**

```bash
# --- confirm_stop（纯函数，便于测） ---
assert_eq "confirm_stop true when nothing to stop" "yes" \
  "$(confirm_stop 0 "" && echo yes || echo no)"
assert_eq "confirm_stop true on default yes" "yes" \
  "$(confirm_stop 2 "y" && echo yes || echo no)"
assert_eq "confirm_stop true on empty answer" "yes" \
  "$(confirm_stop 2 "" && echo yes || echo no)"
assert_eq "confirm_stop false on n" "no" \
  "$(confirm_stop 2 "n" && echo yes || echo no)"
assert_eq "confirm_stop false on N" "no" \
  "$(confirm_stop 2 "N" && echo yes || echo no)"

# --- stop_daemons ---
STOPPS="$TMPROOT/ps-stop.txt"
cat > "$STOPPS" <<EOF
  701 $APPDIR2/Contents/MacOS/yi-agent daemon serve
  702 /Users/dev/x/target/debug/yi-agent daemon serve
EOF
STOPLSOF="$TMPROOT/lsof-stop.txt"
cat > "$STOPLSOF" <<'EOF'
yi-agent 701 u txt REG 1,15 42 100 /Applications/yi-agent.app/Contents/MacOS/yi-agent
yi-agent 702 u txt REG 1,15 42 101 /Users/dev/x/target/debug/yi-agent
EOF
STOPLOG="$TMPROOT/stop.log"
YI_AGENT_PS_CMD="cat $STOPPS" \
YI_AGENT_LSOF_CMD="cat $STOPLSOF" \
YI_AGENT_KILL_CMD="echo \$1 >> $STOPLOG" \
YI_AGENT_PID_ALIVE_CMD=false \
stop_daemons "$APPDIR2" >/dev/null 2>&1
assert_eq "stop_daemons stops only the app daemon" "701" "$(cat "$STOPLOG" 2>/dev/null)"
```

- [ ] **Step 2: 运行测试确认失败**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: FAIL，`confirm_stop: command not found`。

- [ ] **Step 3: 实现**

```bash
ask_answer() {
  if [ -n "${YI_AGENT_ANSWER:-}" ]; then
    printf '%s\n' "$YI_AGENT_ANSWER"
    return 0
  fi
  local answer
  printf '继续？[Y/n] ' >&2
  read -r answer || answer=""
  printf '%s\n' "$answer"
}

confirm_stop() {
  local count="$1" answer="$2"
  [ "$count" -eq 0 ] && return 0
  # 显式拒绝优先于「非交互默认继续」，否则 `YI_AGENT_ANSWER=n`
  # 或手工输入 n 会被静默忽略。
  case "$answer" in
    n|N|no|NO) return 1 ;;
  esac
  # 非交互环境不阻塞等待，默认继续。
  if [ -z "${YI_AGENT_NONINTERACTIVE:-}" ] && [ ! -t 0 ] && [ -z "${YI_AGENT_ANSWER:-}" ]; then
    return 0
  fi
  return 0
}

# SIGTERM 每个属于本 App 的 daemon；返回仍未退出的数量。
stop_daemons() {
  local app="$1"
  local pid remaining=0
  for pid in $(daemon_pids "$app"); do
    kill_pid "$pid"
  done
  for pid in $(daemon_pids "$app"); do
    local i
    for i in $(seq 1 25); do
      pid_alive "$pid" || break
      sleep 0.2
    done
    pid_alive "$pid" && remaining=$((remaining + 1))
  done
  printf '%s\n' "$remaining"
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `44 passed, 0 failed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/packaging/install.sh desktop/scripts/test_install_sh.sh
git commit -m "feat(installer): ask before interrupting running daemons"
```

---

### Task 9: 串联 main 并做本机端到端验证

**Files:**
- Modify: `desktop/packaging/install.sh:main()`

**Interfaces:**
- Consumes: 前八个任务的全部函数
- Produces: 可执行的完整安装流程

- [ ] **Step 1: 重写 main**

```bash
main() {
  set -euo pipefail

  local dest; dest="$(app_path)"
  local dmg="${1:-}"
  if [ -z "$dmg" ]; then
    dmg="$(ls -t "$HOME/Downloads/"yi-agent_*_aarch64.dmg 2>/dev/null | head -1 || true)"
  fi
  if [ -z "$dmg" ] || [ ! -f "$dmg" ]; then
    echo "用法: bash install.sh /path/to/yi-agent_x.y.z_aarch64.dmg" >&2
    echo "（未传参时会在 ~/Downloads 找 yi-agent_*_aarch64.dmg）" >&2
    return 1
  fi

  # 1. 位置校验：不降级，避免 App 路径变成不确定事实。
  local app_parent; app_parent="$(dirname "$dest")"
  if [ ! -w "$app_parent" ]; then
    echo "ERROR: 无法写入 $app_parent（当前用户无权限）。" >&2
    echo "请用管理员权限重跑：" >&2
    echo "    sudo bash install.sh $dmg" >&2
    echo "不要改安装目录：LaunchAgent、命令行入口与升级流程都绑定 $dest。" >&2
    return 1
  fi

  local old_version; old_version="$(app_version "$dest")"

  # 2. 挂载并确定源目录。
  local mount; mount="$(mktemp -d /tmp/yi-agent-dmg.XXXXXX)"
  local stage_cleanup=1
  cleanup() {
    hdiutil detach "$mount" >/dev/null 2>&1 || true
    rm -rf "$mount"
    [ "$stage_cleanup" = 1 ] && rm -rf "$dest.new" "$dest.old"
  }
  trap cleanup EXIT
  echo "挂载 $dmg ..."
  hdiutil attach "$dmg" -mountpoint "$mount" -nobrowse -quiet
  if [ ! -d "$mount/$APP_NAME" ]; then
    echo "ERROR: dmg 内未找到 $APP_NAME" >&2
    return 1
  fi
  local new_version; new_version="$(app_version "$mount/$APP_NAME")"
  version_banner "$old_version" "$new_version"

  # 3. 先停值守，再用 SIGTERM 停旧 daemon（顺序不可交换）。
  restart_own_watchman "$dest" || true

  local pids_count; pids_count="$(daemon_pids "$dest" | wc -l | tr -d ' ')"
  if [ "$pids_count" -gt 0 ]; then
    local answer; answer="$(ask_answer)"
    if ! confirm_stop "$pids_count" "$answer"; then
      echo "已取消，未做任何改动。" >&2
      return 1
    fi
    stop_daemons "$dest" >/dev/null
  fi

  # 4. 退出 GUI（其 sidecar 靠 stdin EOF 自行退出）。
  local gui; gui="$(gui_pids "$dest" | head -1)"
  if [ -n "$gui" ]; then
    echo "正在退出 yi-agent（pid $gui）..."
    if ! quit_gui "$gui" "$dest"; then
      echo "ERROR: 无法退出 App，已保留现有版本。请手动退出后重跑。" >&2
      return 1
    fi
  fi

  # 5. 原子替换 + 清 quarantine。
  echo "安装到 $dest ..."
  replace_app "$mount/$APP_NAME" "$dest"
  xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true
  stage_cleanup=0

  # 6. CLI 入口。
  local bin_dir=""
  if bin_dir="$(pick_bin_dir)"; then
    if link_cli "$dest" "$bin_dir"; then
      echo "命令行入口：$bin_dir/yi-agent -> $dest/Contents/MacOS/yi-agent"
      local status; status="$(link_status "$dest" "$bin_dir")"
      case "$status" in
        ok) ;;
        missing)
          echo "提示：$bin_dir 不在当前 PATH。加到 shell 配置：" >&2
          echo "    export PATH=\"$bin_dir:\$PATH\"" >&2
          ;;
        shadowed:*)
          local hit="${status#shadowed:}"
          echo "WARN: 当前 'yi-agent' 命中的是 $hit，不是刚安装的这条。" >&2
          echo "      PATH 中更靠前的目录有同名程序（常见于 brew / npm 全局安装）。" >&2
          ;;
      esac
    fi
  else
    echo "WARN: 没有可写的 PATH 目录，跳过命令行入口。" >&2
    echo "      手动执行：sudo ln -sf \"$dest/Contents/MacOS/yi-agent\" /usr/local/bin/yi-agent" >&2
  fi

  # 7. 清理死配置。
  local dead
  while IFS= read -r dead; do
    [ -n "$dead" ] || continue
    rm -f "$dead" && echo "已清理失效的 LaunchAgent：$dead"
  done <<EOF
$(dead_plists "$dest")
EOF

  # 8. 校验。
  local got; got="$(cli_version "$dest")"
  if [ -z "$got" ]; then
    echo "ERROR: 安装后内置 CLI 无法执行" >&2
    return 1
  fi
  echo "校验通过：$(basename "$dest") 的 CLI 输出 $got"

  cat <<'EOF'

配置：App 与 TUI 共用 ~/.yi-agent/.env，只需写一次。

  mkdir -p ~/.yi-agent
  echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env

GUI：从「启动台 / 应用程序」打开 yi-agent。
TUI：cd 到项目目录后直接运行 yi-agent。

若首次打开提示「无法验证开发者」：
  System Settings → Privacy & Security → 点 "Open Anyway"。
EOF
}
```

- [ ] **Step 2: 语法检查**

Run: `bash -n desktop/packaging/install.sh`
Expected: 无输出，退出码 0。

- [ ] **Step 3: 跑单测**

Run: `bash desktop/scripts/test_install_sh.sh`
Expected: `44 passed, 0 failed`。

- [ ] **Step 4: shellcheck（若可用）**

Run: `command -v shellcheck >/dev/null && shellcheck -S warning desktop/packaging/install.sh desktop/scripts/test_install_sh.sh || echo "shellcheck 未安装，跳过"`

Expected: 无 error 级问题。

- [ ] **Step 5: 本机端到端（用真实 dmg，人工确认）**

```bash
# 记录现状
ls -l /usr/local/bin/yi-agent
/Applications/yi-agent.app/Contents/MacOS/yi-agent --version
ps -Aw -o pid=,command= | grep -F '/Applications/yi-agent.app/Contents/MacOS/'

# 用已有的 0.1.6 dmg 重装一次（等价于同版本升级）
bash desktop/packaging/install.sh desktop/src-tauri/target/release/bundle/dmg/yi-agent_0.1.6_aarch64.dmg
```

预期：
- GUI 若在运行会被自动退出
- 结束后 `/usr/local/bin/yi-agent` 指向 App 内 CLI
- `yi-agent --version` 输出 `yi-agent 0.1.6`
- `~/.yi-agent/.env` 未被修改（`ls -l` 的 mtime 不变）

- [ ] **Step 6: 提交**

```bash
git add desktop/packaging/install.sh
git commit -m "feat(installer): orchestrate upgrade with gui and daemon coordination"
```

---

### Task 10: Release 文案与项目文档同步

**Files:**
- Modify: `desktop/scripts/release-dmg.sh:163-190`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/ci-cd.md:38`

**Interfaces:**
- Consumes: 无
- Produces: 用户可见说明与文档条目

- [ ] **Step 1: 替换 Release 描述**

把 `desktop/scripts/release-dmg.sh` 中 `DESCRIPTION="$(cat <<EOF` 到对应 `EOF` 的「命令行版 yi-agent（可选）」整段替换为：

```bash
DESCRIPTION="$(cat <<EOF
## macOS 桌面 App（Apple Silicon）

下载 \`$ART\` 后：

1. \`bash install.sh ./$ART\`（或双击 dmg 把 yi-agent.app 拖进「应用程序」后执行
   \`xattr -dr com.apple.quarantine /Applications/yi-agent.app\`）
2. **配置模型 key（必需，App 内没有设置入口）**：
   \`mkdir -p ~/.yi-agent && echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env\`
3. 从「应用程序」打开 yi-agent。

未签名产物；首次打开如被拦，到 System Settings → Privacy & Security 点 "Open Anyway"。

## 终端版（TUI）

App 内已内置同一份 CLI，安装脚本会自动建好入口，**不需要另下产物**：

\`\`\`
/usr/local/bin/yi-agent -> /Applications/yi-agent.app/Contents/MacOS/yi-agent
\`\`\`

装完后 \`cd\` 到项目目录直接运行 \`yi-agent\`。配置与 App 共用
\`~/.yi-agent/.env\`，只写一次。

若安装器提示 \`/usr/local/bin\` 不可写，它会退到 \`~/.local/bin\` 并打印需要添加的
PATH 行；不会修改你的 shell 配置。

## 升级

下载新 dmg 后重跑同一条命令：

\`\`\`bash
bash install.sh ./yi-agent_<新版本>_aarch64.dmg
\`\`\`

安装器会：自动退出正在运行的 App → 停止旧版 runtime daemon（若正在跑子 agent
任务会先询问）→ 原子替换 App → 重建命令行入口。配置与历史都会保留。

- 安装位置固定为 \`/Applications\`；非管理员账户请用 \`sudo bash install.sh ...\`
- 不支持降级：数据库 schema 只升不降，回退旧版本可能读到不兼容数据
- 安装器会处理 \`ai.yi-agent.*\` 的 LaunchAgent，但只动「指向本 App 且原本已加载」
  的那份；指向开发构建的会被跳过并告警

校验（可选）：\`shasum -a 256 $ART\` 的输出应与 \`$ART.sha256\` 的内容一致。
EOF
)"
```

- [ ] **Step 2: 更新 desktop 模块文档**

在 `docs/project-management/desktop.md` 的 Features 列表末尾追加：

```markdown
- [x] 安装器建 CLI 软链 + 原地升级 + daemon 协调 — `desktop/packaging/install.sh`（`pick_bin_dir` / `link_cli` / `replace_app` / `gui_pids` / `daemon_pids` / `restart_own_watchman` / `dead_plists` / `confirm_stop`）；Release 文案见 `desktop/scripts/release-dmg.sh:163`；验证：`bash desktop/scripts/test_install_sh.sh` 全绿（44 passed），且本机执行 `bash desktop/packaging/install.sh <dmg>` 后 `yi-agent --version` 与 dmg 版本一致 — [设计](../superpowers/specs/2026-10-19-install-cli-link-design.md)
```

- [ ] **Step 3: 更新 CI/CD 模块文档**

把 `docs/project-management/ci-cd.md:38` 那条的判据结尾追加：

```markdown
；安装器侧的 CLI 软链与升级流程见 `docs/project-management/desktop.md`
```

- [ ] **Step 4: 校验脚本语法**

Run: `bash -n desktop/scripts/release-dmg.sh`
Expected: 无输出，退出码 0。

- [ ] **Step 5: 校验 dry-run 描述渲染**

Run: `DRY_RUN=1 CI_COMMIT_TAG=v0.1.6 CI_API_V4_URL=https://gitlab.example/api/v4 CI_PROJECT_ID=1 CI_JOB_TOKEN=x bash desktop/scripts/release-dmg.sh 2>&1 | grep -A4 '终端版' | head -8`
Expected: 输出包含「App 内已内置同一份 CLI」与 `/usr/local/bin/yi-agent`。

- [ ] **Step 6: 提交**

```bash
git add desktop/scripts/release-dmg.sh docs/project-management/desktop.md docs/project-management/ci-cd.md
git commit -m "docs(release): explain bundled CLI and the upgrade flow"
```

---

## 自检

**Spec 覆盖**

| Spec 章节 | 任务 |
| --- | --- |
| 1 CLI 入口 | Task 3 |
| 2 不降级安装位置 | Task 9 Step 1（位置校验） |
| 3 原子替换 | Task 4 |
| 4 自动退出 GUI | Task 5 |
| 5 旧 daemon 清理 | Task 6 + Task 8 |
| 6 交互确认 | Task 8 |
| 7 LaunchAgent 协调 | Task 7 |
| 8 残留 plist 清理 | Task 7 |
| 9 版本展示 | Task 2 + Task 9 |
| 10 可测试性 | Task 1（source 化）+ 各任务测试 |
| 11 Release 文案 | Task 10 |

**已知偏差**

- Spec 第 5 节写「不是本次安装刚启动的进程（避免自杀）」：本实现中安装器自身从不启动
  daemon，故没有自杀风险；`daemon_pids` 只从 `ps` 读取既有进程，无需额外判据。
- Spec 第 6 节要求「存在未完成任务时询问」：实现简化为「存在任何将终止的 daemon 时
  询问」。判断任务是否未完成需要读 runtime 数据库（`runtime.sqlite`），安装器不应
  依赖 schema，故取更保守的「只要有 daemon 就问」。