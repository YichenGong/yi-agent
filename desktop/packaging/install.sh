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

# 下面几个包装器都支持「注入带参数的命令」，故用 eval。
# 生产路径下它们分别等价于 `osascript -e ...`、`kill <pid>`。
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

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  main "$@"
fi
