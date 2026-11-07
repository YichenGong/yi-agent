#!/usr/bin/env bash
# reclaim-target.sh 的自测：删除判据必须只命中「孤儿」编译单元。
#
# 为什么值得单独测：这个脚本的动作是 `rm`。判据写宽了会删掉**活跃**编译单元的
# `.o`（下次重编从零开始，代价是分钟级）；写窄了等于没清理。两种错误都不会报错，
# 只能靠这里钉住。
#
# 用假文件驱动，不编译任何东西：判据只依赖文件名形状与是否存在同名 rlib。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/reclaim-target.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }
n=0

deps="$tmp/debug/deps"
mkdir -p "$deps"

# 活跃单元 foo：有 rlib，3 个 .o 必须保留。
for i in 0 1 2; do printf 'x%.0s' {1..1000} >"$deps/foo-aaaaaaaaaaaaaaaa.$i.rcgu.o"; done
printf 'r%.0s' {1..100} >"$deps/libfoo-aaaaaaaaaaaaaaaa.rlib"

# 孤儿单元 bar：无 rlib，4 个 .o 必须删。
for i in 0 1 2 3; do printf 'y%.0s' {1..2000} >"$deps/bar-bbbbbbbbbbbbbbbb.$i.rcgu.o"; done

# 孤儿单元 baz：单个 .o，必须删。
printf 'z%.0s' {1..500} >"$deps/baz-cccccccccccccccc.0.rcgu.o"

# 非 .o 文件永不触碰（即使对应的 rlib 不存在）。
printf 'd' >"$deps/bar-bbbbbbbbbbbbbbbb.0.d"
printf 'other' >"$deps/unrelated.txt"

out="$(bash "$SCRIPT" --target "$tmp")" || fail "dry-run exited nonzero"
grep -q "孤儿 .o         : 5 个文件 / 2 个编译单元" <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "dry-run did not report 5 orphan files in 2 units"; }
n=$((n + 1)); pass "dry-run identifies 5 orphan files across 2 units"

# 预演不得删任何东西。
[ "$(find "$deps" -name '*.rcgu.o' | wc -l | tr -d ' ')" = "8" ] || fail "dry-run deleted files"
n=$((n + 1)); pass "dry-run leaves all 8 .o in place"

bash "$SCRIPT" --apply --target "$tmp" >/dev/null || fail "--apply exited nonzero"

for i in 0 1 2; do
  [ -f "$deps/foo-aaaaaaaaaaaaaaaa.$i.rcgu.o" ] || fail "removed live unit .o ($i)"
done
n=$((n + 1)); pass "live unit (has rlib) preserved"

[ -f "$deps/libfoo-aaaaaaaaaaaaaaaa.rlib" ] || fail "removed rlib"
n=$((n + 1)); pass "rlib untouched"

leftover="$(find "$deps" -name 'bar-*.rcgu.o' -o -name 'baz-*.rcgu.o' | wc -l | tr -d ' ')"
[ "$leftover" = "0" ] || fail "$leftover orphan .o survived"
n=$((n + 1)); pass "orphan units removed"

[ -f "$deps/bar-bbbbbbbbbbbbbbbb.0.d" ] || fail "removed a .d file"
[ -f "$deps/unrelated.txt" ] || fail "removed an unrelated file"
n=$((n + 1)); pass "non-.o files untouched"

# 幂等：再跑一次不应报错，也不应再删东西。
bash "$SCRIPT" --apply --target "$tmp" >/dev/null || fail "second --apply exited nonzero"
n=$((n + 1)); pass "idempotent on an already-clean target"

# 参数校验。
bash "$SCRIPT" --bogus >/dev/null 2>&1 && fail "accepted an unknown option"
n=$((n + 1)); pass "rejects unknown options"

printf '\n%d passed\n' "$n"
