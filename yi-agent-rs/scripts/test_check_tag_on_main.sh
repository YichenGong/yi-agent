#!/usr/bin/env bash
# check-tag-on-main.sh 的行为测试。
#
# 每个用例都在临时 git 仓库里跑，不碰真实仓库的分支与 tag。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/check-tag-on-main.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 造一个最小的 git 仓库：init + 一个提交。
new_repo() {
  local dir="$1"
  mkdir -p "$dir"
  git -C "$dir" init -q -b main
  git -C "$dir" -c user.email=t@example.com -c user.name=t commit -q --allow-empty -m init
}

# ---------- 1. tag 在 main 上 → 通过 ----------
r="$TMP/on-main"; new_repo "$r"
git -C "$r" tag v1.0.0
if ! "$SCRIPT" v1.0.0 main "$r" >/dev/null 2>&1; then
  fail "a tag on main must pass"
fi
pass "a tag on main passes"

# ---------- 2. tag 在功能分支上（main 不含它）→ 拒绝 ----------
r="$TMP/off-main"; new_repo "$r"
git -C "$r" checkout -q -b feat/x
git -C "$r" -c user.email=t@example.com -c user.name=t commit -q --allow-empty -m feature
git -C "$r" tag v1.0.1
git -C "$r" checkout -q main
if "$SCRIPT" v1.0.1 main "$r" >/dev/null 2>&1; then
  fail "a tag on an unmerged feature branch must be rejected"
fi
pass "a tag on an unmerged feature branch is rejected"

# ---------- 3. 功能分支已合入 main（tag 打在合并后的提交上）→ 通过 ----------
r="$TMP/merged"; new_repo "$r"
git -C "$r" checkout -q -b feat/y
git -C "$r" -c user.email=t@example.com -c user.name=t commit -q --allow-empty -m feature
git -C "$r" checkout -q main
git -C "$r" -c user.email=t@example.com -c user.name=t merge -q --no-ff feat/y -m "merge feat/y"
git -C "$r" tag v1.0.2
if ! "$SCRIPT" v1.0.2 main "$r" >/dev/null 2>&1; then
  fail "a tag on a merged commit must pass"
fi
pass "a tag on a merged commit passes"

# ---------- 4. tag 不存在 → 拒绝（而不是误判为通过） ----------
r="$TMP/missing"; new_repo "$r"
if "$SCRIPT" v9.9.9 main "$r" >/dev/null 2>&1; then
  fail "a nonexistent tag must be rejected"
fi
pass "a nonexistent tag is rejected"

# ---------- 5. 基线分支不存在 → 拒绝 ----------
r="$TMP/nobase"; new_repo "$r"
git -C "$r" tag v1.0.3
if "$SCRIPT" v1.0.3 no-such-branch "$r" >/dev/null 2>&1; then
  fail "an unknown base branch must be rejected"
fi
pass "an unknown base branch is rejected"

# ---------- 6. 参数个数不对 → 用法错误 ----------
if "$SCRIPT" v1.0.0 >/dev/null 2>&1; then
  fail "wrong arity must fail"
fi
pass "wrong arity fails"

# ---------- 7. annotated tag 也要能识别 ----------
r="$TMP/annotated"; new_repo "$r"
git -C "$r" -c user.email=t@example.com -c user.name=t tag -a v1.0.4 -m "release 1.0.4"
if ! "$SCRIPT" v1.0.4 main "$r" >/dev/null 2>&1; then
  fail "an annotated tag on main must pass"
fi
pass "an annotated tag on main passes"

echo "all tag-on-main checks passed"
