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
