#!/usr/bin/env bash
# 自测 rebuild.sh 的安装段：dev/stable 两个 profile 分别把二进制装到哪、全局
# 软链指到哪、以及拒绝覆盖真实文件与缺失父目录时必须失败。
#
# 只 source 脚本、喂一个假二进制，不编译 Rust，也不碰真实的
# /opt/homebrew/bin（三个落点全由 REBUILD_* 环境变量指向 mktemp 目录）。
#
# 不用 `set -e`：下面多处要故意触发被测函数的非零退出。
#
#   bash scripts/test_rebuild_global_link.sh
#
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/rebuild.sh"
[ -f "$SCRIPT" ] || { echo "FAIL: $SCRIPT not found"; exit 1; }

pass=0
failn=0
ok()  { echo "ok   - $1"; pass=$((pass+1)); }
bad() { echo "FAIL - $1"; failn=$((failn+1)); }
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected exit $2, got $3)"; fi
}

root="$(mktemp -d "${HERE}/.selftest.XXXXXX")" || { echo "FAIL: mktemp"; exit 1; }
trap 'rm -rf "$root"' EXIT

mkfake() { # mkfake <path> <marker>
  mkdir -p "$(dirname "$1")"
  printf '#!/bin/sh\necho %s\n' "$2" > "$1"
  chmod 0755 "$1"
}

# 切到一套独立的落点并立即重新加载常量。必须加载：LOCAL_BIN_DIR 等是从
# 环境变量算出来的脚本变量，只改 env 不重载会让后续用例继续用旧路径。
# 用法：use_case <case-dir> <profile>
use_case() {
  CASE="$1"
  # 生产里 /opt/homebrew/bin 一定存在；测试自建一个充当它。父目录缺失的
  # 分支由用例 5 单独验。
  mkdir -p "$CASE/global"
  export REBUILD_LOCAL_BIN_DIR="$CASE/local"
  export REBUILD_CARGO_BIN_DIR="$CASE/cargo"
  export REBUILD_GLOBAL_BIN="$CASE/global/yi-agent"
  load_script "$2"
}

# rebuild.sh 顶部有 `set -euo pipefail`，source 后会把 -e 带进本测试进程，
# 于是第一个「预期失败」的用例会导致测试静默退出。source 完立刻恢复。
load_script() { # load_script <profile>
  PROFILE="$1"
  # shellcheck source=/dev/null
  . "$SCRIPT"
  PROFILE="$1"
  set +e +u +o pipefail
}

link_target() { readlink "$GLOBAL_BIN" 2>/dev/null || true; }

# rebuild.sh 的 die 是 `exit 1`：在同一进程里直接调用会把本测试一起退掉。
# 每次都放进子 shell，退出码即被测函数的退出码，文件系统改动照样可见。
run_install() { ( install_cli_bin "$1" ) >/dev/null 2>&1; }

# --- 用例 1：dev 只写 yi-agent-dev，全局软链指过去 -----------
use_case "$root/dev" dev
src="$root/dev/src-yi-agent"; mkfake "$src" "fake-dev-binary"
run_install "$src"
check "dev: install exits 0" 0 $?

if [ -x "$LOCAL_BIN_DIR/yi-agent-dev" ]; then ok "dev: $LOCAL_BIN_DIR/yi-agent-dev written and executable"
else bad "dev: $LOCAL_BIN_DIR/yi-agent-dev missing"; fi
if [ -L "$GLOBAL_BIN" ]; then ok "dev: global path is a symlink"; else bad "dev: global path is not a symlink"; fi
if [ "$(link_target)" = "$LOCAL_BIN_DIR/yi-agent-dev" ]; then ok "dev: global symlink targets yi-agent-dev"
else bad "dev: global symlink targets '$(link_target)'"; fi

out="$("$GLOBAL_BIN" 2>&1)"
if [ "$out" = "fake-dev-binary" ]; then ok "dev: running the global path reaches the new binary"
else bad "dev: global path runs '$out'"; fi
if [ -e "$CARGO_BIN_DIR/yi-agent" ]; then bad "dev: stable binary must not be written"
else ok "dev: stable binary untouched (cargo bin absent)"; fi

# --- 用例 2：全局路径是真实文件时必须失败且不覆盖 ------------
use_case "$root/dev-real" dev
src="$root/dev-real/src-yi-agent"; mkfake "$src" "fake-dev-binary"
mkdir -p "$(dirname "$GLOBAL_BIN")"
printf 'real-homebrew-binary\n' > "$GLOBAL_BIN"
run_install "$src"
check "dev: refuses to overwrite a real file (nonzero exit)" 1 $?
if [ "$(cat "$GLOBAL_BIN" 2>/dev/null)" = "real-homebrew-binary" ] && [ ! -L "$GLOBAL_BIN" ]; then
  ok "dev: real file left byte-identical"
else bad "dev: real file was altered or replaced"; fi
if [ -x "$LOCAL_BIN_DIR/yi-agent-dev" ]; then ok "dev: dev binary still installed before refusing"
else bad "dev: dev binary not installed"; fi

# --- 用例 3：stable 写 cargo bin，全局软链指过去，dev 二进制不动 ---------
use_case "$root/stable" stable
src="$root/stable/src-yi-agent"; mkfake "$src" "fake-stable-binary"
devfile="$LOCAL_BIN_DIR/yi-agent-dev"; mkfake "$devfile" "pre-existing-dev"
run_install "$src"
check "stable: install exits 0" 0 $?

if [ -x "$CARGO_BIN_DIR/yi-agent" ]; then ok "stable: $CARGO_BIN_DIR/yi-agent written"
else bad "stable: $CARGO_BIN_DIR/yi-agent missing"; fi
if [ "$(link_target)" = "$CARGO_BIN_DIR/yi-agent" ]; then ok "stable: global symlink targets cargo bin"
else bad "stable: global symlink targets '$(link_target)'"; fi
if [ "$("$devfile" 2>/dev/null)" = "pre-existing-dev" ]; then ok "stable: existing dev binary left alone"
else bad "stable: dev binary was overwritten"; fi

# --- 用例 4：幂等 + 回切（dev → dev → stable） ---------------
use_case "$root/switch" dev
src="$root/switch/src-yi-agent"; mkfake "$src" "fake-binary"
run_install "$src"
run_install "$src"
check "idempotent: second dev run exits 0" 0 $?
if [ "$(link_target)" = "$LOCAL_BIN_DIR/yi-agent-dev" ]; then ok "idempotent: still one symlink to yi-agent-dev"
else bad "idempotent: symlink drifted to '$(link_target)'"; fi
# 软链不能自指：跑起来得能拿到假二进制的输出。
out="$("$GLOBAL_BIN" 2>&1)"
if [ "$out" = "fake-binary" ]; then ok "idempotent: symlink resolves to an executable file"
else bad "idempotent: symlink does not execute ('$out')"; fi
if [ -e "$(link_target)" ]; then ok "idempotent: symlink resolves to an existing file"
else bad "idempotent: symlink is dangling"; fi

PROFILE=stable
. "$SCRIPT"
PROFILE=stable
run_install "$src"
check "switch back: stable run exits 0" 0 $?
if [ "$(link_target)" = "$CARGO_BIN_DIR/yi-agent" ]; then ok "switch back: global symlink back on cargo bin"
else bad "switch back: global symlink targets '$(link_target)'"; fi

# --- 用例 5：父目录不存在 → 跳过软链但不中断，且不凭空创建 ----------
# 这台机器上没有 Homebrew 时不该让整条 `all` 构建因为一个软链而失败，
# 但也不能代建 /opt/homebrew/bin。
use_case "$root/absent" dev
GLOBAL_BIN="$root/absent/not-created/yi-agent"
src="$root/absent/src-yi-agent"; mkfake "$src" "fake-binary"
run_install "$src"
check "missing global dir: still exits 0 (skip, do not fail the build)" 0 $?
if [ ! -e "$root/absent/not-created" ]; then ok "missing global dir: parent not created"
else bad "missing global dir: parent was created"; fi
if [ -x "$LOCAL_BIN_DIR/yi-agent-dev" ]; then ok "missing global dir: dev binary still installed"
else bad "missing global dir: dev binary missing"; fi

# --- 用例 6：父目录存在但不可写 → 失败（路径真实存在，静默跳过会误导） ---
use_case "$root/ro" dev
GLOBAL_BIN="$root/ro/locked/yi-agent"
mkdir -p "$root/ro/locked"
chmod 0555 "$root/ro/locked"
src="$root/ro/src-yi-agent"; mkfake "$src" "fake-binary"
run_install "$src"
check "read-only global dir: fails (nonzero exit)" 1 $?
if [ ! -e "$GLOBAL_BIN" ]; then ok "read-only global dir: no symlink created"
else bad "read-only global dir: symlink was created"; fi
chmod 0755 "$root/ro/locked"

# --- 用例 7：--help 不吞代码（usage 截注释靠「遇非注释即停」） ----------
if bash "$SCRIPT" --help 2>/dev/null | grep -q '^set -euo pipefail'; then
  bad "--help leaks script body"
else ok "--help stops at the first non-comment line"; fi

# --- 用例 8：僵尸进程必须算作「已退出」 ---------------------------
#
# 回归缺陷（2026-11-09 实测）：guardian 被杀后停在 `<defunct>`（父进程
# app-server 活着却不 wait），`kill -0` 对僵尸仍返回成功，于是 `wait_gone`
# 永不到期、`stop_dependents` 报「仍有进程停不下来」并 die——而进程其实早就
# 死了。`is_alive` 必须按 `ps -o stat=Z` 判否。
sh_pid=""
# 后台起一个长命进程，取它真实的 pid（不走管道：管道会让 `head` 退出后
# `sleep` 收到 SIGPIPE 立刻死掉，pid 立刻变僵尸，用例就失去了意义）。
sleep 30 &
sh_pid=$!
if [ -n "$sh_pid" ]; then
  # 先确认它此刻是活进程（非 Z），否则本用例没有意义。
  if is_alive "$sh_pid"; then ok "is_alive: a running process counts as alive"
  else bad "is_alive: a running process was reported dead (pid $sh_pid, stat=$(ps -o stat= -p "$sh_pid" 2>/dev/null | xargs))"; fi
  kill -KILL "$sh_pid" 2>/dev/null || true
  wait "$sh_pid" 2>/dev/null || true
else
  bad "zombie fixture: could not spawn a probe process"
fi

# 造一个真僵尸：子进程 kill 掉但不 wait，父进程继续活着。
zombie_parent="$(sh -c '
  sh -c "sleep 30" &
  child=$!
  kill -KILL "$child" 2>/dev/null
  # 不 wait：子进程停在 Z，父进程挂着保持它不被回收。
  sleep 5
' >/dev/null 2>&1 &
echo $!)"
sleep 0.5
# 从父进程的僵尸子进程里挑一个。
zombie_pid="$(ps -axo pid=,ppid=,stat= 2>/dev/null | awk -v p="$zombie_parent" '$2==p && $3 ~ /^Z/ {print $1}' | head -1)"
if [ -n "$zombie_pid" ]; then
  if is_alive "$zombie_pid"; then
    bad "is_alive: a zombie (Z) was reported alive (pid $zombie_pid)"
  else
    ok "is_alive: a zombie (Z) counts as gone"
  fi
  # kill -0 对僵尸成功，这条断言把「为什么不能用 kill -0」钉住。
  if kill -0 "$zombie_pid" 2>/dev/null; then
    ok "zombie fixture: kill -0 still succeeds (the reason is_alive exists)"
  else
    ok "zombie fixture: kill -0 already fails on this platform"
  fi
  if wait_gone 1 "$zombie_pid"; then
    ok "wait_gone: returns 0 for an already-dead zombie"
  else
    bad "wait_gone: timed out on a zombie (pid $zombie_pid)"
  fi
  kill -KILL "$zombie_parent" 2>/dev/null || true
else
  bad "zombie fixture: could not produce a zombie child"
  kill -KILL "$zombie_parent" 2>/dev/null || true
fi

# --- 用例 9：guardian.pid 必须先于 daemon 被停 -------------------
#
# guardian 活着就会重启它托管的 daemon，所以先杀 daemon 等于白杀
# （`lifecycle/guardian.rs` 的 `GUARDIAN_PID_FILE` 注释记的就是这个坑）。
# 这里只验解析与「只认 internal-runtime guardian」两道判据，不真杀进程。
use_case "$root/guardian" dev
ROOT="$root/guardian/repo"
mkdir -p "$ROOT/.yi-agent/runtime"
gp="$root/guardian/guardian-pid.json"
printf '{"pid": 99999, "start_micros": 1794240682569446}\n' > "$ROOT/.yi-agent/runtime/guardian.pid"
if [ "$(guardian_pid_records)" = "99999" ]; then ok "guardian.pid: pid is read back"
else bad "guardian.pid: read '$(guardian_pid_records)'"; fi

# 一个不存在的 pid 必须被 is_alive 判否，绝不能凭记录就发信号。
if is_alive 99999; then bad "guardian.pid: a dead recorded pid was reported alive"
else ok "guardian.pid: a dead recorded pid is not signalled"; fi

# 缺文件时安静返回空，不报错（首次构建没有该文件）。
rm -f "$ROOT/.yi-agent/runtime/guardian.pid"
if [ -z "$(guardian_pid_records)" ]; then ok "guardian.pid: absent file yields nothing"
else bad "guardian.pid: absent file yielded '$(guardian_pid_records)'"; fi

# 坏 JSON 同样安静跳过。
printf 'not json\n' > "$ROOT/.yi-agent/runtime/guardian.pid"
if [ -z "$(guardian_pid_records)" ]; then ok "guardian.pid: malformed file is skipped"
else bad "guardian.pid: malformed file yielded '$(guardian_pid_records)'"; fi

# 只认 internal-runtime guardian：拿一个普通进程冒充记录里的 pid，必须不杀。
sleep 30 &
impostor=$!
printf '{"pid": %s, "start_micros": 1}\n' "$impostor" > "$ROOT/.yi-agent/runtime/guardian.pid"
( stop_recorded_guardians ) >/dev/null 2>&1
if is_alive "$impostor"; then ok "guardian.pid: a pid reused by another program is not killed"
else bad "guardian.pid: killed an unrelated process that reused the pid"; fi
kill -KILL "$impostor" 2>/dev/null || true
wait "$impostor" 2>/dev/null || true

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"
