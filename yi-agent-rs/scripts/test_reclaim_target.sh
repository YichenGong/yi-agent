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

out="$(bash "$SCRIPT" --target "$tmp" --min-age 0 2>/dev/null)" || fail "dry-run exited nonzero"
grep -q "孤儿 .o         : 5 个文件 / 2 个编译单元" <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "dry-run did not report 5 orphan files in 2 units"; }
n=$((n + 1)); pass "dry-run identifies 5 orphan files across 2 units"

# 预演不得删任何东西。
[ "$(find "$deps" -name '*.rcgu.o' | wc -l | tr -d ' ')" = "8" ] || fail "dry-run deleted files"
n=$((n + 1)); pass "dry-run leaves all 8 .o in place"

bash "$SCRIPT" --apply --target "$tmp" --min-age 0 >/dev/null || fail "--apply exited nonzero"

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
bash "$SCRIPT" --apply --target "$tmp" --min-age 0 >/dev/null || fail "second --apply exited nonzero"
n=$((n + 1)); pass "idempotent on an already-clean target"

# ---- D1: incremental 单元内的旧会话 ----
inc="$tmp/debug/incremental"
mkdir -p "$inc/alpha-1111/s-old" "$inc/alpha-1111/s-new" "$inc/beta-2222/s-only"
printf 'x%.0s' {1..3000} >"$inc/alpha-1111/s-old/obj.o"
printf 'x%.0s' {1..1000} >"$inc/alpha-1111/s-new/obj.o"
printf 'x%.0s' {1..500}  >"$inc/beta-2222/s-only/obj.o"
# 会话目录里还有 .lock，删目录时要一并带走且不报错
printf 'lock' >"$inc/alpha-1111/s-old/s-old.lock"
# 人为让 s-new 比 s-old 新（判据用 mtime）
touch -t 202601010000 "$inc/alpha-1111/s-old"
touch -t 202601020000 "$inc/alpha-1111/s-new"

out="$(bash "$SCRIPT" --target "$tmp" --min-age 0 2>/dev/null)" || fail "dry-run exited nonzero"
grep -q "incremental 旧会话" <<<"$out" || { printf '%s\n' "$out" >&2; fail "dry-run does not report incremental"; }

bash "$SCRIPT" --apply --target "$tmp" --min-age 0 >/dev/null || fail "--apply exited nonzero"

[ -d "$inc/alpha-1111/s-old" ] && fail "kept the stale session dir"
n=$((n + 1)); pass "D1 removes the stale session inside a unit"
[ -f "$inc/alpha-1111/s-new/obj.o" ] || fail "removed the newest session"
n=$((n + 1)); pass "D1 keeps the newest session in a unit"
[ -f "$inc/beta-2222/s-only/obj.o" ] || fail "removed a single-session unit"
n=$((n + 1)); pass "D1 leaves single-session units alone"

# --skip-incremental 时完全不碰
mkdir -p "$inc/alpha-1111/s-old2"; printf 'y' >"$inc/alpha-1111/s-old2/obj.o"
touch -t 202512010000 "$inc/alpha-1111/s-old2"
bash "$SCRIPT" --apply --target "$tmp" --skip-incremental --min-age 0 >/dev/null || fail "--skip-incremental exited nonzero"
[ -d "$inc/alpha-1111/s-old2" ] || fail "--skip-incremental still deleted"
n=$((n + 1)); pass "D1 honours --skip-incremental"

# ---- D2: N_OSO 引用集 ----
# 用独立的 target 根目录，避免和前面 D1 的 fixture 互相干扰。
d2="$tmp/t2/debug/deps"
mkdir -p "$d2"
# 活跃单元 foo：有 rlib，3 个 mid；只有 mid0 被二进制引用
for i in 0 1 2; do printf 'x%.0s' {1..1000} >"$d2/foo-aaaaaaaaaaaaaaaa.mid$i.0.rcgu.o"; done
printf 'r%.0s' {1..100} >"$d2/libfoo-aaaaaaaaaaaaaaaa.rlib"
# 孤儿单元 bar：无 rlib，应被整组删掉
printf 'y%.0s' {1..2000} >"$d2/bar-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"

# 一个可执行文件（名字不带 '.'，cargo 产物即如此）
: >"$d2/foo_bin-1234"; chmod +x "$d2/foo_bin-1234"

# 假 nm：只报告引用了 foo 的 mid0
stub="$tmp/stubbin"; mkdir -p "$stub"
cat >"$stub/nm" <<NMEOF
#!/bin/sh
echo "0000000100000000-0000000100000010 OSO $d2/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o"
NMEOF
chmod +x "$stub/nm"

out="$(PATH="$stub:$PATH" bash "$SCRIPT" --target "$tmp/t2" --min-age 0 2>/dev/null)" || fail "dry-run exited nonzero"
grep -q "未被引用" <<<"$out" || { printf '%s\n' "$out" >&2; fail "dry-run does not report unreferenced .o"; }
n=$((n + 1)); pass "D2 reports unreferenced .o"

# 预演不删
[ "$(find "$d2" -name '*.rcgu.o' | wc -l | tr -d ' ')" = "4" ] || fail "dry-run deleted .o"
n=$((n + 1)); pass "D2 dry-run deletes nothing"

PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp/t2" --min-age 0 >/dev/null || fail "D2 --apply failed"
[ -f "$d2/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] || fail "deleted the OSO-referenced .o"
n=$((n + 1)); pass "D2 keeps OSO-referenced .o"
[ -f "$d2/foo-aaaaaaaaaaaaaaaa.mid1.0.rcgu.o" ] && fail "kept an unreferenced .o in a live unit"
n=$((n + 1)); pass "D2 removes unreferenced .o in a live unit"
[ -f "$d2/bar-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] && fail "kept an orphan-key .o"
n=$((n + 1)); pass "D2 removes orphan-key .o"
[ -f "$d2/libfoo-aaaaaaaaaaaaaaaa.rlib" ] || fail "deleted an rlib"
n=$((n + 1)); pass "D2 leaves rlib alone"

# 幂等
PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp/t2" --min-age 0 >/dev/null || fail "2nd apply failed"
n=$((n + 1)); pass "D2 idempotent"

# 没有任何可执行文件时必须退化为只清孤儿 key，不误删活跃单元
d3="$tmp/t3/debug/deps"; mkdir -p "$d3"
printf 'a' >"$d3/solo-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"
printf 'b' >"$d3/libsolo-bbbbbbbbbbbbbbbb.rlib"
printf 'c' >"$d3/orphan-dddddddddddddddd.mid0.0.rcgu.o"
bash "$SCRIPT" --apply --target "$tmp/t3" --min-age 0 >/dev/null || fail "no-binary case failed"
[ -f "$d3/solo-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] || fail "no-binary fallback deleted a live unit"
n=$((n + 1)); pass "D2 falls back to orphan-key-only when no binaries exist"
[ -f "$d3/orphan-dddddddddddddddd.mid0.0.rcgu.o" ] && fail "fallback did not clean orphan key"
n=$((n + 1)); pass "D2 fallback still cleans orphan keys"

# --skip-deps：完全不碰 deps
d4="$tmp/t4/debug/deps"; mkdir -p "$d4"
printf 'c' >"$d4/orphan-eeeeeeeeeeeeeeee.mid0.0.rcgu.o"
bash "$SCRIPT" --apply --target "$tmp/t4" --skip-deps --min-age 0 >/dev/null || fail "--skip-deps failed"
[ -f "$d4/orphan-eeeeeeeeeeeeeeee.mid0.0.rcgu.o" ] || fail "--skip-deps still cleaned deps"
n=$((n + 1)); pass "D2 honours --skip-deps"

# ---- 引用集零交集：必须整体放弃引用判据（这是 47 GB 误删的唯一防线）----
# 真实场景：split-debuginfo 从 unpacked 切到 packed 后，存量的 unpacked .o
# 与新一代二进制的 N_OSO 名字格式完全不同，交集为 0。此时若照用引用判据，
# 会把 100% 的 .o 判成"未被引用"（实测 268,571/268,571）。
t8="$tmp/t8/debug/deps"; mkdir -p "$t8"
printf 'r' >"$t8/livc-1111111111111111.rlib"
printf '%.0sA' {1..100} >"$t8/livc-1111111111111111.mid0.0.rcgu.o"
: >"$t8/livc_bin-8888"; chmod +x "$t8/livc_bin-8888"
# 桩 nm：返回一个完全不存在于磁盘的名字 —— 精确模拟跨代际零交集
mkdir -p "$tmp/nmstub-zero"
cat >"$tmp/nmstub-zero/nm" <<'NM'
#!/bin/sh
echo "0000-1111 OSO /nowhere/ghost-9999999999999999.mid9.9.rcgu.o"
NM
chmod +x "$tmp/nmstub-zero/nm"
PATH="$tmp/nmstub-zero:$PATH" bash "$SCRIPT" --apply --target "$tmp/t8" --min-age 0 >/dev/null \
  || fail "zero-overlap case failed"
[ -f "$t8/livc-1111111111111111.mid0.0.rcgu.o" ] \
  || fail "zero-overlap deleted a live .o (this is the 47GB failure mode)"
n=$((n + 1)); pass "D2 abandons N_OSO when reference set does not intersect disk"
outz="$(PATH="$tmp/nmstub-zero:$PATH" bash "$SCRIPT" --target "$tmp/t8" --min-age 0 2>/dev/null)" \
  || fail "zero-overlap dry-run failed"
grep -qiE "跳过|skip|0 命中|no overlap|无交集" <<<"$outz" \
  || { printf '%s\n' "$outz" >&2; fail "zero-overlap not explained in output"; }
n=$((n + 1)); pass "D2 explains why N_OSO was skipped"

# 硬链接：文件照删（另半边可能是 incremental 会话），但 nlink>1 时不计入释放量
d5="$tmp/t5/debug/deps"; mkdir -p "$d5"
# 必须有一个**真实存在且被引用**的文件，否则 referenced ∩ objs == 0，
# use_ref 会是 False，活跃 key 的 .o 一个都不会被删，nlink 逻辑测不到。
printf 'k' >"$d5/keep-eeeeeeeeeeeeeeee.mid0.0.rcgu.o"
printf 'r' >"$d5/libkeep-eeeeeeeeeeeeeeee.rlib"
# 同一 key 的两个名字共享 inode —— 必须落在「活跃单元 + 未被引用」这一分支
printf 'z%.0s' {1..4000} >"$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o"
printf 'r' >"$d5/liblnk-ffffffffffffffff.rlib"
ln "$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o" "$d5/lnk-ffffffffffffffff.mid0.1.rcgu.o"
ln "$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o" "$tmp/hardlinked_elsewhere"
: >"$d5/lnk_bin-9999"; chmod +x "$d5/lnk_bin-9999"
mkdir -p "$tmp/nmstub-lnk"
cat >"$tmp/nmstub-lnk/nm" <<NMEOF
#!/bin/sh
echo "0000-1111 OSO $d5/keep-eeeeeeeeeeeeeeee.mid0.0.rcgu.o"
NMEOF
chmod +x "$tmp/nmstub-lnk/nm"

out="$(PATH="$tmp/nmstub-lnk:$PATH" bash "$SCRIPT" --target "$tmp/t5" --min-age 0 2>/dev/null)" || fail "hardlink dry-run failed"
grep -q "未被引用的 .o   : 2 个文件" <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "hardlinked .o pair not reported as 2 unreferenced files"; }
n=$((n + 1)); pass "D2 reports both hardlinked names as unreferenced"

outa="$(PATH="$tmp/nmstub-lnk:$PATH" bash "$SCRIPT" --apply --target "$tmp/t5" --min-age 0 2>/dev/null)" || fail "hardlink apply failed"
[ -e "$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o" ] && fail "hardlinked .o was not deleted"
[ -e "$d5/lnk-ffffffffffffffff.mid0.1.rcgu.o" ] && fail "second hardlinked name was not deleted"
[ -f "$tmp/hardlinked_elsewhere" ] || fail "hardlink outside target was removed"
n=$((n + 1)); pass "D2 deletes hardlinked .o but spares its external link"
# nlink>1 的删除不释放空间，所以「回收」必须是 0 —— 这是计数规则的直接断言
grep -q "D2 已删除       : 2 个，回收 0.0 B" <<<"$outa" \
  || { printf '%s\n' "$outa" >&2; fail "nlink>1 deletions were counted as freed bytes"; }
n=$((n + 1)); pass "D2 does not count hardlinked deletions as freed bytes"
[ -f "$d5/keep-eeeeeeeeeeeeeeee.mid0.0.rcgu.o" ] || fail "deleted the referenced .o"
n=$((n + 1)); pass "D2 keeps the referenced .o alongside hardlinks"

# ---- D3: --prune-units=N ----
inc="$tmp/debug/incremental"
mkdir -p "$inc/gamma-1111/s-a" "$inc/gamma-2222/s-b" "$inc/gamma-3333/s-c"
printf 'x' >"$inc/gamma-1111/s-a/o"; printf 'y' >"$inc/gamma-2222/s-b/o"; printf 'z' >"$inc/gamma-3333/s-c/o"
touch -t 202601010000 "$inc/gamma-1111"; touch -t 202601020000 "$inc/gamma-2222"; touch -t 202601030000 "$inc/gamma-3333"

# 默认不执行：三个都还在
bash "$SCRIPT" --apply --target "$tmp" --min-age 0 >/dev/null 2>&1 || fail "default run failed"
[ -d "$inc/gamma-1111" ] || fail "pruned units without --prune-units"
n=$((n + 1)); pass "D3 is off by default"

bash "$SCRIPT" --apply --target "$tmp" --prune-units 1 --min-age 0 >/dev/null 2>&1 || fail "--prune-units 1 failed"
[ -d "$inc/gamma-3333" ] || fail "pruned the newest unit"
n=$((n + 1)); pass "D3 keeps the newest N units"
[ -d "$inc/gamma-1111" ] && fail "kept an old unit when N=1"
n=$((n + 1)); pass "D3 prunes units beyond N"

# 参数校验：非法 N 必须报错，不能当成 0 静默跳过
bash "$SCRIPT" --prune-units abc >/dev/null 2>&1 && fail "accepted a non-numeric --prune-units"
n=$((n + 1)); pass "D3 rejects non-numeric --prune-units"

# ---- 并发护栏 ----
m="$tmp/t6/debug/deps"; mkdir -p "$m"
printf 'x%.0s' {1..500} >"$m/fresh-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o"
printf 'y%.0s' {1..500} >"$m/old-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"
# old 设成 2 小时前，fresh 保持 mtime = 现在
touch -t 202601010000 "$m/old-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"

bash "$SCRIPT" --apply --target "$tmp/t6" --min-age 600 >/dev/null 2>&1 || fail "--min-age failed"
[ -f "$m/fresh-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] || fail "--min-age did not protect a fresh file"
n=$((n + 1)); pass "--min-age protects files newer than the threshold"
[ -f "$m/old-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] && fail "--min-age skipped an old file too"
n=$((n + 1)); pass "--min-age still deletes files older than the threshold"

# --min-age 0 解除保护
bash "$SCRIPT" --apply --target "$tmp/t6" --min-age 0 >/dev/null 2>&1 || fail "--min-age 0 failed"
[ -f "$m/fresh-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] && fail "--min-age 0 still protected"
n=$((n + 1)); pass "--min-age 0 disables the protection"

# 检测到构建锁被持有时默认拒绝
l="$tmp/t7/debug"; mkdir -p "$l/deps"
printf 'x' >"$l/deps/orphan-cccccccccccccccc.mid0.0.rcgu.o"
touch -t 202601010000 "$l/deps/orphan-cccccccccccccccc.mid0.0.rcgu.o"
: >"$l/.cargo-build-lock"
# 用假 lsof 模拟「锁被持有」
stub2="$tmp/stub2"; mkdir -p "$stub2"
printf '#!/bin/sh\necho "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME"\necho "cargo 123 user 3w REG 1,2 0 1 $1"\n' >"$stub2/lsof"
chmod +x "$stub2/lsof"
PATH="$stub2:$PATH" bash "$SCRIPT" --apply --target "$tmp/t7" --min-age 0 >/dev/null 2>&1 \
  && fail "did not refuse while the build lock was held"
n=$((n + 1)); pass "refuses to run while a cargo build lock is held"
PATH="$stub2:$PATH" bash "$SCRIPT" --apply --target "$tmp/t7" --min-age 0 --force >/dev/null 2>&1 \
  || fail "--force did not override the lock check"
n=$((n + 1)); pass "--force overrides the lock check"

# --dry-run 不受锁检测影响（只读，没必要拦）
PATH="$stub2:$PATH" bash "$SCRIPT" --target "$tmp/t7" --min-age 0 >/dev/null 2>&1 \
  || fail "dry-run was refused by the lock check"
n=$((n + 1)); pass "dry-run ignores the lock check"

# ---- D4: 陈旧二进制 + dSYM ----
# 分组键必须是 (crate, 目标类型)。只用 crate 前缀会把 bin 和 test-bin 当成
# 同一组 —— 实测真实仓库 plugin_pack 的两个哈希 mtime 只差 0.338s，但它们
# 分别是 bin-plugin-pack 和 test-bin-plugin-pack，两个都必须保留。
t9="$tmp/t9"
d9="$t9/debug/deps"; mkdir -p "$d9"
fp="$t9/debug/.fingerprint"

# 同一 crate、同一目标(bin) 的两个 hash：旧的删、新的留
mkdir -p "$fp/demo-aaaaaaaaaaaaaaaa" "$fp/demo-bbbbbbbbbbbbbbbb"
: >"$fp/demo-aaaaaaaaaaaaaaaa/bin-demo.json"
: >"$fp/demo-bbbbbbbbbbbbbbbb/bin-demo.json"
printf 'old' >"$d9/demo-aaaaaaaaaaaaaaaa"; chmod +x "$d9/demo-aaaaaaaaaaaaaaaa"
printf 'new' >"$d9/demo-bbbbbbbbbbbbbbbb"; chmod +x "$d9/demo-bbbbbbbbbbbbbbbb"
mkdir -p "$d9/demo-aaaaaaaaaaaaaaaa.dSYM/Contents/Resources/DWARF"
printf 'dsym-old' >"$d9/demo-aaaaaaaaaaaaaaaa.dSYM/Contents/Resources/DWARF/demo"
# 同名 .d / .rlib 也算「该目标的产物」，但只有二进制和 dSYM 在回收范围
printf 'dep' >"$d9/demo-aaaaaaaaaaaaaaaa.d"
touch -t 202601010000 "$d9/demo-aaaaaaaaaaaaaaaa" "$d9/demo-aaaaaaaaaaaaaaaa.dSYM"
touch -t 202601020000 "$d9/demo-bbbbbbbbbbbbbbbb"

# 同一 crate、不同目标(bin vs test-bin)：mtime 相同，两个都必须留
mkdir -p "$fp/multi-cccccccccccccccc" "$fp/multi-dddddddddddddddd"
: >"$fp/multi-cccccccccccccccc/bin-multi.json"
: >"$fp/multi-dddddddddddddddd/test-bin-multi.json"
printf 'a' >"$d9/multi-cccccccccccccccc"; chmod +x "$d9/multi-cccccccccccccccc"
printf 'b' >"$d9/multi-dddddddddddddddd"; chmod +x "$d9/multi-dddddddddddddddd"
touch -t 202601030000 "$d9/multi-cccccccccccccccc" "$d9/multi-dddddddddddddddd"

out="$(bash "$SCRIPT" --target "$t9" --min-age 0 2>/dev/null)" || fail "D4 dry-run failed"
grep -q "陈旧二进制" <<<"$out" || { printf '%s\n' "$out" >&2; fail "dry-run does not report stale binaries"; }
n=$((n + 1)); pass "D4 reports stale binaries in dry-run"
[ -e "$d9/demo-aaaaaaaaaaaaaaaa" ] || fail "D4 dry-run deleted a binary"
n=$((n + 1)); pass "D4 dry-run deletes nothing"

bash "$SCRIPT" --apply --target "$t9" --min-age 0 >/dev/null 2>&1 || fail "D4 apply failed"
[ -e "$d9/demo-aaaaaaaaaaaaaaaa" ] && fail "D4 kept a stale binary"
n=$((n + 1)); pass "D4 deletes a stale binary (same crate + same target)"
[ -e "$d9/demo-aaaaaaaaaaaaaaaa.dSYM" ] && fail "D4 kept the stale dSYM"
n=$((n + 1)); pass "D4 deletes the stale binary's dSYM"
[ -x "$d9/demo-bbbbbbbbbbbbbbbb" ] || fail "D4 deleted the newest binary"
n=$((n + 1)); pass "D4 keeps the newest binary"
[ -x "$d9/multi-cccccccccccccccc" ] && [ -x "$d9/multi-dddddddddddddddd" ] \
  || fail "D4 confused two different targets of the same crate"
n=$((n + 1)); pass "D4 keeps bin and test-bin of the same crate separately"
# 无 fingerprint 的二进制一律保留（判不了就不动）
printf 'x' >"$d9/nofp-eeeeeeeeeeeeeeee"; chmod +x "$d9/nofp-eeeeeeeeeeeeeeee"
bash "$SCRIPT" --apply --target "$t9" --min-age 0 >/dev/null 2>&1 || fail "D4 second apply failed"
[ -x "$d9/nofp-eeeeeeeeeeeeeeee" ] || fail "D4 deleted a binary without fingerprint evidence"
n=$((n + 1)); pass "D4 keeps binaries that have no fingerprint entry"

# 回归：fingerprint 目录用连字符、deps 二进制用下划线，哈希相同。
# 实测真实仓库就是这种形式（.fingerprint/yi-agent-d89cb... vs deps/yi_agent-d89cb...）。
# 若索引键拼上包名，这类二进制全部匹配不上 → D4 静默失效、一个都删不掉。
t10="$tmp/t10"; d10="$t10/debug/deps"; mkdir -p "$d10"
f10="$t10/debug/.fingerprint"
h_old=1111111111111111; h_new=2222222222222222
mkdir -p "$f10/my-crate-$h_old" "$f10/my-crate-$h_new"
: >"$f10/my-crate-$h_old/bin-my-crate.json"
: >"$f10/my-crate-$h_new/bin-my-crate.json"
printf 'old' >"$d10/my_crate-$h_old"; chmod +x "$d10/my_crate-$h_old"
printf 'new' >"$d10/my_crate-$h_new"; chmod +x "$d10/my_crate-$h_new"
touch -t 202601010000 "$d10/my_crate-$h_old"
touch -t 202601020000 "$d10/my_crate-$h_new"
bash "$SCRIPT" --apply --target "$t10" --min-age 0 >/dev/null 2>&1 || fail "hyphen/underscore case failed"
[ -e "$d10/my_crate-$h_old" ] && fail "D4 failed to match hyphenated fingerprint to underscore binary"
n=$((n + 1)); pass "D4 matches fingerprint hashes across hyphen/underscore name forms"
[ -x "$d10/my_crate-$h_new" ] || fail "D4 deleted the newest binary in hyphen/underscore case"
n=$((n + 1)); pass "D4 keeps the newest binary in hyphen/underscore case"

# 参数校验。
bash "$SCRIPT" --bogus >/dev/null 2>&1 && fail "accepted an unknown option"
n=$((n + 1)); pass "rejects unknown options"

printf '\n%d passed\n' "$n"
