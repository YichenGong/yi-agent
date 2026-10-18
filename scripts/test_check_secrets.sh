#!/usr/bin/env bash
# 自测 scripts/check-secrets.sh：确认它在「有真实凭据」时变红、
# 在「只有示例值」或「值在未跟踪文件里」时保持绿。
# 不用 `set -e`：本脚本要故意触发被测脚本的「非零退出」，那些非零是
# 期望结果，不能被 -e 当成致命错误。
set -uo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/check-secrets.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable"; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
failn=0
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "ok   - $1"; pass=$((pass+1));
  else echo "FAIL - $1 (expected exit $2, got $3)"; failn=$((failn+1)); fi
}

# --- case 1: 真实 session 在跟踪文件里 → 必须 exit 1
r1="$work/leaked"; mkdir -p "$r1/docs"
git -C "$r1" init -q
printf 'see wss://relay.yi-agent.top/connect?session=fe42addbfed257dcd396e7fcc45e2725\n' > "$r1/docs/a.md"
git -C "$r1" add docs/a.md
( cd "$r1" && bash "$SCRIPT" >/dev/null 2>&1 ); check "real session in tracked file fails" 1 $?

# --- case 2: 只有示例值 → 必须 exit 0
r2="$work/clean"; mkdir -p "$r2/docs"
git -C "$r2" init -q
printf 'see wss://relay.example.com/connect?session=<session_id>\n' > "$r2/docs/a.md"
printf 'lsof -nP -p "$(pgrep -f app-server)" | grep 203.0.113.10\n' >> "$r2/docs/a.md"
git -C "$r2" add docs/a.md
( cd "$r2" && bash "$SCRIPT" >/dev/null 2>&1 ); check "example values pass" 0 $?

# --- case 3: 真实值但文件未被跟踪 → 必须 exit 0（不误伤 .relay-local.md 范式）
r3="$work/untracked"; mkdir -p "$r3/deploy"
git -C "$r3" init -q
printf 'session = fe42addbfed257dcd396e7fcc45e2725\nip = 101.47.76.100\n' > "$r3/deploy/local.md"
printf 'x\n' > "$r3/keep.md"; git -C "$r3" add keep.md
( cd "$r3" && bash "$SCRIPT" >/dev/null 2>&1 ); check "untracked file is ignored" 0 $?

# --- case 4: 真实域名在跟踪文件里 → 必须 exit 1
r4="$work/host"; mkdir -p "$r4/docs"
git -C "$r4" init -q
printf 'connect to wss://relay.yi-agent.top/ws\n' > "$r4/docs/a.md"
git -C "$r4" add docs/a.md
( cd "$r4" && bash "$SCRIPT" >/dev/null 2>&1 ); check "real hostname in tracked file fails" 1 $?

# --- case 5: 从子目录调用仍须扫到仓库根。
#     泄露值刻意放在仓库根、调用点在其子目录 `src/`：若门禁以 cwd 为扫描界，
#     它会只看到 src/ 而漏掉根上的泄露（假绿）。`just` recipe 的 cwd 是
#     `yi-agent-rs/` 且要扫根目录 docs/，正是同一形态。
r5="$work/subdir"; mkdir -p "$r5/src"
git -C "$r5" init -q
printf 'session = fe42addbfed257dcd396e7fcc45e2725\n' > "$r5/leak.md"
printf 'nothing to see here\n' > "$r5/src/clean.md"
git -C "$r5" add leak.md src/clean.md
( cd "$r5/src" && bash "$SCRIPT" >/dev/null 2>&1 ); check "invoked from subdir still scans repo root" 1 $?

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"