#!/usr/bin/env bash
# 自测 scripts/test-magpie-agent.sh：把「检查自身会失败」的路径钉死，
# 尤其缺 Node 时必须 exit 2（基础设施故障）而不是假绿。
set -uo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/test-magpie-agent.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable"; exit 1; }

# 临时隔离目录不放在 $TMPDIR（macOS 沙箱对 /var/folders/... 会 EPERM）。
HERE="$(cd "$(dirname "$0")" && pwd)"

pass=0
failn=0
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "ok   - $1"; pass=$((pass+1));
  else echo "FAIL - $1 (expected exit $2, got $3)"; failn=$((failn+1)); fi
}

# 无 node 的 PATH：必须 exit 2。保留 /bin 与 /usr/bin（shebang 是
# `#!/usr/bin/env bash`，`env` 要靠 PATH 找到 bash，dirname/pwd 也在这里），
# 只把 node 所在的 `/opt/homebrew/bin` 拿掉——这才是「有 shell、没 node」的真实
# 最小环境，而不是空 PATH（空 PATH 会让 `env bash` 自己 127）。
{
  PATH="/bin:/usr/bin" bash "$SCRIPT"
} >/dev/null 2>&1
check "missing node is an infra failure (exit 2)" 2 $?

# 正常环境：必须 exit 0（真实跑语法 + 行为测试）。
{
  bash "$SCRIPT"
} >/dev/null 2>&1
check "a full run passes (exit 0)" 0 $?

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"
