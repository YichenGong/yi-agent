#!/usr/bin/env bash
# check-release-version.sh 的行为测试。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$HERE/check-release-version.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

# 1. 一致 → 通过
"$GATE" 0.1.7 0.1.7 >/dev/null || fail "an exact match must pass"
pass "an exact match passes"

# 2. dev 构建 → 拒绝
if "$GATE" 0.1.7 0.1.7-dev+e7aae577 >/dev/null 2>&1; then
  fail "a dev build must be rejected"
fi
pass "a dev build is rejected"

# 3. 无 git 构建 → 拒绝
if "$GATE" 0.1.7 0.1.7+nogit >/dev/null 2>&1; then
  fail "a nogit build must be rejected"
fi
pass "a nogit build is rejected"

# 4. 版本不一致 → 拒绝
if "$GATE" 0.1.7 0.1.6 >/dev/null 2>&1; then
  fail "a version mismatch must be rejected"
fi
pass "a version mismatch is rejected"

# 5. 空值 → 拒绝
if "$GATE" 0.1.7 "" >/dev/null 2>&1; then
  fail "an empty reported version must be rejected"
fi
pass "an empty reported version is rejected"

# 6. 参数个数不对 → 用法错误
if "$GATE" 0.1.7 >/dev/null 2>&1; then
  fail "wrong arity must fail"
fi
pass "wrong arity fails"

echo "all release gate checks passed"
