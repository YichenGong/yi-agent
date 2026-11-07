#!/usr/bin/env bash
# test-tiers.sh 的自测：判据必须只按「路径前缀」分类。
#
# 为什么值得单独测：这个分类决定了 test-affected 跑哪些 crate。分类写宽了
# 会把 desktop/ 当成 Rust crate 去跑（报错或空跑）；写窄了会静默少测——
# 后者是本设计要消除的失败模式，不会报错，只能靠这里钉住。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/test-tiers.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable" >&2; exit 1; }

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }
n=0

# 固定成员列表，避免测试依赖真实 workspace 内容。
export WORKSPACE_CRATES="yi-agent-core yi-agent-flags yi-agent-web"

map() { printf '%s\n' "$@" | "$SCRIPT" map; }

out="$(map 'yi-agent-rs/crates/yi-agent-core/src/a.rs')"
grep -qx 'MAPPED: yi-agent-core' <<<"$out" || { printf '%s\n' "$out" >&2; fail "core src not mapped"; }
n=$((n + 1)); pass "maps crates/<name>/src to <name>"

out="$(map 'yi-agent-rs/crates/yi-agent-flags/tests/b.rs')"
grep -qx 'MAPPED: yi-agent-flags' <<<"$out" || fail "tests/ path not mapped"
n=$((n + 1)); pass "maps crates/<name>/tests to <name>"

out="$(map 'yi-agent-rs/crates/yi-agent-web/Cargo.toml')"
grep -qx 'MAPPED: yi-agent-web' <<<"$out" || fail "manifest path not mapped"
n=$((n + 1)); pass "maps crates/<name>/Cargo.toml to <name>"

# 去重 + 保持出现顺序
out="$(map 'yi-agent-rs/crates/yi-agent-core/src/a.rs' 'yi-agent-rs/crates/yi-agent-core/src/b.rs' 'yi-agent-rs/crates/yi-agent-flags/src/c.rs')"
grep -qx 'MAPPED: yi-agent-core yi-agent-flags' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "did not dedupe/order"; }
n=$((n + 1)); pass "dedupes while preserving first-seen order"

out="$(map 'docs/x.md' 'README.md' 'AGENTS.md' 'CLAUDE.md')"
grep -qx 'DOCS: docs/x.md README.md AGENTS.md CLAUDE.md' <<<"$out" || fail "docs not classified"
grep -qx 'MAPPED: ' <<<"$out" || fail "docs wrongly mapped to a crate"
n=$((n + 1)); pass "classifies docs as DOCS, not as crates"

out="$(map 'desktop/src/app.ts' 'plugins/foo.json' 'integrations/x.rs')"
grep -qx 'UNMAPPED: desktop/src/app.ts plugins/foo.json integrations/x.rs' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "non-crate paths not classified as unmapped"; }
n=$((n + 1)); pass "classifies non-crate paths as UNMAPPED"

# 关键反例：crates/ 下但不是宵 workspace 成员的目录（已删除的 crate）
out="$(map 'yi-agent-rs/crates/yi-agent-gone/src/a.rs')"
grep -qx 'UNMAPPED: yi-agent-rs/crates/yi-agent-gone/src/a.rs' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "deleted crate should be UNMAPPED, not MAPPED"; }
n=$((n + 1)); pass "treats crates/<removed>/ as UNMAPPED, not MAPPED"

# yi-agent-rs/crates 之外的 yi-agent-rs 文件（justfile 等）也是 UNMAPPED
out="$(map 'yi-agent-rs/justfile')"
grep -qx 'UNMAPPED: yi-agent-rs/justfile' <<<"$out" || fail "justfile should be UNMAPPED"
n=$((n + 1)); pass "treats yi-agent-rs/justfile as UNMAPPED"

# 关键反例：读不到成员列表时必须失败退出，不能把全部路径判成 UNMAPPED 后返回 0。
# 这是本设计最危险的静默失败：cargo 不可用 → 一个 crate 都匹配不上 → 少测所有东西。
unset WORKSPACE_CRATES
err="$(env PATH=/usr/bin:/bin "$SCRIPT" map <<<'yi-agent-rs/crates/yi-agent-core/src/a.rs' 2>&1 >/dev/null)"
rc=$?
[ "$rc" -ne 0 ] || { fail "map must fail (not exit 0) when workspace members cannot be read"; }
grep -q 'cargo' <<<"$err" || { printf '%s\n' "$err" >&2; fail "error must mention cargo"; }
n=$((n + 1)); pass "fails loudly when workspace members cannot be read"

# 同上，但 cargo 存在而 metadata 失败（模拟损坏的 manifest）
fakebin="$(mktemp -d)"
trap 'rm -rf "$fakebin"' EXIT
cat >"$fakebin/cargo" <<'SH'
#!/bin/sh
echo "error: failed to read manifest" >&2
exit 101
SH
chmod +x "$fakebin/cargo"
err="$(env PATH="$fakebin:/usr/bin:/bin" "$SCRIPT" map <<<'yi-agent-rs/crates/yi-agent-core/src/a.rs' 2>&1 >/dev/null)"
rc=$?
[ "$rc" -ne 0 ] || { fail "map must fail when cargo metadata fails"; }
grep -q 'cargo metadata failed' <<<"$err" || { printf '%s\n' "$err" >&2; fail "error must mention cargo metadata failure"; }
n=$((n + 1)); pass "fails loudly when cargo metadata fails"

printf '\n%d passed\n' "$n"
