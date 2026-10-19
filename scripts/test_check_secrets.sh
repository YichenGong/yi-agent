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

# --- case 6: 不在 git 仓库里 → 必须 exit 2（扫不动就要红，而不是假绿）
#     这里刻意不做 `git init`：macOS 上 `cd ""` 返回 0，修复前脚本会一路走到
#     `secret-check passed` 并 exit 0，即「零文件扫描」被当成「没有凭据」。
r6="$work/notarepo"; mkdir -p "$r6"
( cd "$r6" && bash "$SCRIPT" >/dev/null 2>&1 ); check "not a git repository fails (cannot scan)" 2 $?

# --- case 7: 空仓库（unborn HEAD，无提交、索引为空）→ 必须 exit 2
#     实测：此处 `git grep` 对"无匹配"返回 rc=1，并非 rc=128，所以「零文件被
#     扫描」不会被 rc 检查抓到，靠的是「一个可扫文件都没有」这道守卫。
r7="$work/nocommits"; mkdir -p "$r7/docs"
git -C "$r7" init -q
printf 'session = fe42addbfed257dcd396e7fcc45e2725\n' > "$r7/docs/a.md"
( cd "$r7" && bash "$SCRIPT" >/dev/null 2>&1 ); check "repo with nothing tracked fails (cannot verify)" 2 $?

# Helper for one-file tracked repositories used by the structural rules below.
fixture() { # fixture <name> <file-content>
  local name="$1" content="$2" repo
  repo="$work/$name"
  mkdir -p "$repo"
  git -C "$repo" init -q
  printf '%s\n' "$content" > "$repo/fixture.txt"
  git -C "$repo" add fixture.txt
}

# --- case 8: arbitrary 32-hex session structure → exit 1
fixture session-structure 'relay session=0123456789abcdef0123456789abcdef'
( cd "$work/session-structure" && bash "$SCRIPT" >/dev/null 2>&1 ); check "32-hex session structure fails" 1 $?

# --- case 9: session placeholder → exit 0
fixture session-placeholder 'relay session=<session_id>'
( cd "$work/session-placeholder" && bash "$SCRIPT" >/dev/null 2>&1 ); check "session placeholder passes" 0 $?

# --- case 10: relay.example.com → exit 0
fixture example-host 'connect to relay.example.com'
( cd "$work/example-host" && bash "$SCRIPT" >/dev/null 2>&1 ); check "relay.example.com passes" 0 $?

# --- case 10b: URL percent-encoded relay.example.com → exit 0
fixture encoded-example-host 'relay=wss%3A%2F%2Frelay.example.com%2Fws%3Fsession%3Dabc'
( cd "$work/encoded-example-host" && bash "$SCRIPT" >/dev/null 2>&1 ); check "encoded relay.example.com passes" 0 $?

# --- case 11: other relay.* host → exit 1
fixture other-relay-host 'connect to relay.other.example'
( cd "$work/other-relay-host" && bash "$SCRIPT" >/dev/null 2>&1 ); check "other relay hostname fails" 1 $?

# --- case 12: relay.example.com.evil is not the example hostname → exit 1
fixture lookalike-host 'connect to relay.example.com.evil'
( cd "$work/lookalike-host" && bash "$SCRIPT" >/dev/null 2>&1 ); check "relay.example.com lookalike fails" 1 $?

# --- case 13: ssh options before root@public IPv4 → exit 1
fixture ssh-public-ip 'ssh -n -o BatchMode=yes root@198.51.101.7'
( cd "$work/ssh-public-ip" && bash "$SCRIPT" >/dev/null 2>&1 ); check "ssh options with public IP fail" 1 $?

# --- case 14: ssh RFC 5737 address → exit 0
fixture ssh-doc-ip 'ssh -n -o BatchMode=yes root@198.51.100.7'
( cd "$work/ssh-doc-ip" && bash "$SCRIPT" >/dev/null 2>&1 ); check "ssh RFC 5737 address passes" 0 $?

# --- case 15: lsof/grep public IPv4 context → exit 1
fixture lsof-public-ip 'lsof -nP -iTCP | grep 198.51.101.8'
( cd "$work/lsof-public-ip" && bash "$SCRIPT" >/dev/null 2>&1 ); check "lsof grep public IP fails" 1 $?

# --- case 16: lsof/grep RFC 5737 address → exit 0
fixture lsof-doc-ip 'lsof -nP -iTCP | grep 198.51.100.8'
( cd "$work/lsof-doc-ip" && bash "$SCRIPT" >/dev/null 2>&1 ); check "lsof grep RFC 5737 address passes" 0 $?

# --- case 17: same diagnostic line has RFC example and public IP → still exit 1
fixture mixed-ip-line 'ssh root@203.0.113.7; ssh root@198.51.101.9'
( cd "$work/mixed-ip-line" && bash "$SCRIPT" >/dev/null 2>&1 ); check "public IP is not hidden by RFC address on same line" 1 $?

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"
