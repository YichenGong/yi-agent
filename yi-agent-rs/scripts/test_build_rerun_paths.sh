#!/usr/bin/env bash
# build.rs 的 rerun 路径必须真实存在。
#
# 为什么：cargo 把「不存在的 rerun-if-changed 路径」当作永远变化，于是 build
# script 每次都重跑——版本号看起来是对的，但这是巧合而不是设计。一旦路径哪天
# 真的存在了（或 cargo 行为收紧），打了 tag 却不重建，`--version` 就会继续报
# `-dev`，而发布门禁会在打包后才发现。
#
# 做法：跑一次 build，然后读 cargo 记下的 fingerprint，逐条确认路径存在。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS_DIR="$(cd "$HERE/.." && pwd)"
cd "$RS_DIR"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || fail "cargo not found"

cargo build -p yi-agent >/dev/null 2>&1 || fail "cargo build -p yi-agent failed"

FP_ROOT="${CARGO_TARGET_DIR:-$RS_DIR/target}/debug/.fingerprint"
[ -d "$FP_ROOT" ] || fail "no fingerprint directory at $FP_ROOT"

# 找出 yi-agent 的 build-script 运行记录。
record="$(ls -1t "$FP_ROOT"/yi-agent-*/run-build-script-build-script-build.json 2>/dev/null | head -1)"
[ -n "$record" ] || fail "no run-build-script json for yi-agent (did the build script run?)"

# 那个 json 所在目录就是 cargo 解析相对路径的基准。
base="$(dirname "$record")"

paths="$(python3 - "$record" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
for entry in data.get("local", []):
    changed = entry.get("RerunIfChanged")
    if changed:
        for p in changed.get("paths", []):
            print(p)
PY
)"

[ -n "$paths" ] || fail "build.rs declares no rerun-if-changed paths"

count=0
while IFS= read -r rel; do
    [ -n "$rel" ] || continue
    resolved="$(cd "$base" && python3 -c 'import os,sys; print(os.path.normpath(sys.argv[1]))' "$rel")"
    if [ ! -e "$resolved" ]; then
        printf '  missing: %s -> %s\n' "$rel" "$resolved" >&2
        fail "build.rs listens to a path that does not exist: $rel"
    fi
    count=$((count + 1))
done <<< "$paths"

pass "all $count rerun-if-changed path(s) exist"

# tag 更新必须能触发重建：refs/tags 目录本身要在监听列表里。
if ! grep -q 'refs/tags' "$record"; then
    fail "build.rs does not watch .git/refs/tags, so creating a tag will not rebuild the binary"
fi
pass "build.rs watches refs/tags, so tagging triggers a rebuild"

echo "all build-rerun checks passed"
