#!/usr/bin/env bash
# check-msrv.sh 的行为测试。
#
# 正向用例用真实工具链跑（本机必须已装 1.89.0，CI 的 msrv job 会装）；
# 负向用例都在临时目录里构造，不碰真实仓库。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/check-msrv.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok   - %s\n' "$*"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---------- 1. 没有 Cargo.toml → 退出 ----------
mkdir -p "$TMP/empty"
if "$SCRIPT" "$TMP/empty" >/dev/null 2>&1; then
  fail "missing Cargo.toml must fail"
fi
pass "missing Cargo.toml fails"

# ---------- 2. Cargo.toml 里没有 rust-version → 退出 ----------
mkdir -p "$TMP/nover"
printf '[workspace]\nmembers = []\n' > "$TMP/nover/Cargo.toml"
if "$SCRIPT" "$TMP/nover" >/dev/null 2>&1; then
  fail "missing rust-version must fail"
fi
pass "missing rust-version fails"

# ---------- 3. 声明的版本没装 → 退出，且提示如何安装 ----------
mkdir -p "$TMP/nodl"
printf '[workspace]\nmembers = []\n\n[workspace.package]\nrust-version = "1.0"\n' \
  > "$TMP/nodl/Cargo.toml"
if out="$("$SCRIPT" "$TMP/nodl" 2>&1)"; then
  fail "an uninstalled toolchain must fail"
fi
case "$out" in
  *"rustup toolchain install 1.0.0"*) ;;
  *) fail "missing-toolchain message must say how to install it; got: $out" ;;
esac
pass "uninstalled toolchain fails with install hint"

# ---------- 4. 真实仓库：声明的 MSRV 必须真的能编译 ----------
REPO="$(cd "$HERE/.." && pwd)"
declared="$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$REPO/Cargo.toml" | head -1)"
[ -n "$declared" ] || fail "real repo has no rust-version"
"$SCRIPT" "$REPO" > "$TMP/real.log" 2>&1 \
  || { tail -20 "$TMP/real.log" >&2; fail "declared MSRV $declared does not build"; }
grep -q "rustc ${declared}" "$TMP/real.log" \
  || fail "log should name the declared rustc version"
pass "real repo compiles on its declared MSRV ($declared)"

echo
echo "all msrv checks passed"
