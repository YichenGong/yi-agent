#!/usr/bin/env bash
# check-plugin-release.sh 的行为测试。
#
# 每个用例造一个临时 git 仓库，不碰真实分支与 tag。四个门禁各自要有
# 一个"该拒绝就拒绝"的用例——只测通过路径的测试证明不了任何规则存在。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/check-plugin-release.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 造一个最小仓库：main 上一个提交，一个插件目录，主程序 workspace 版本 0.1.13。
new_repo() {
  local dir="$1" plugin_version="$2" ui_version="$3"
  mkdir -p "$dir/plugins/demo/ui" "$dir/yi-agent-rs"
  cat > "$dir/plugins/demo/Cargo.toml" <<EOF
[workspace]
members = ["crates/demo"]

[workspace.package]
version = "$plugin_version"
edition = "2024"
EOF
  cat > "$dir/plugins/demo/ui/package.json" <<EOF
{
  "name": "demo-ui",
  "version": "$ui_version"
}
EOF
  cat > "$dir/yi-agent-rs/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/main"]

[workspace.package]
version = "0.1.13"
edition = "2024"
EOF
  git -C "$dir" init -q -b main
  git -C "$dir" -c user.email=t@example.com -c user.name=t add -A
  git -C "$dir" -c user.email=t@example.com -c user.name=t commit -q -m init
}

run() { "$SCRIPT" "$@" >/dev/null 2>&1; }

# ---------- 1. 合法插件 tag → 通过 ----------
r="$TMP/ok"; new_repo "$r" "1.2.0" "1.2.0"
git -C "$r" tag plugin-demo-v1.2.0
run plugin-demo-v1.2.0 "$r" || fail "a well-formed plugin tag on main must pass"
pass "a well-formed plugin tag on main passes"

# 打出来的行要能被 CI 拿去比对主程序版本是否未变。
out="$("$SCRIPT" plugin-demo-v1.2.0 "$r")"
case "$out" in
  *"plugin=demo version=1.2.0 main-version=0.1.13"*) ;;
  *) fail "expected a parseable summary line, got: $out" ;;
esac
pass "the summary line reports plugin, version and main version"

# ---------- 2. 冒用主程序 tag 形状 → 拒绝 ----------
r="$TMP/main-tag"; new_repo "$r" "1.2.0" "1.2.0"
git -C "$r" tag v0.2.0
if run v0.2.0 "$r"; then
  fail "a main-app style tag must be rejected by the plugin gate"
fi
pass "a main-app style tag is rejected"

# ---------- 3. tag 指向未合并分支 → 拒绝 ----------
r="$TMP/unmerged"; new_repo "$r" "1.2.0" "1.2.0"
git -C "$r" checkout -q -b feat/x
git -C "$r" -c user.email=t@example.com -c user.name=t commit -q --allow-empty -m feature
git -C "$r" tag plugin-demo-v1.2.0
git -C "$r" checkout -q main
if run plugin-demo-v1.2.0 "$r"; then
  fail "a tag on an unmerged branch must be rejected"
fi
pass "a tag on an unmerged branch is rejected"

# ---------- 4. 版本与 Cargo.toml 不符 → 拒绝 ----------
r="$TMP/bad-cargo"; new_repo "$r" "1.2.0" "1.2.0"
git -C "$r" tag plugin-demo-v9.9.9
if run plugin-demo-v9.9.9 "$r"; then
  fail "a tag that disagrees with Cargo.toml must be rejected"
fi
pass "a tag that disagrees with Cargo.toml is rejected"

# ---------- 4b. 只有 Cargo 版本不符（UI 缺失）→ 拒绝 ----------
# 与用例 4 分开：这里删掉 UI package.json，让唯一可能拦住它的就是 Cargo 检查。
r="$TMP/bad-cargo-only"; new_repo "$r" "1.2.0" "1.2.0"
rm -rf "$r/plugins/demo/ui"
git -C "$r" -c user.email=t@example.com -c user.name=t add -A
git -C "$r" -c user.email=t@example.com -c user.name=t commit -q -m drop-ui
git -C "$r" tag plugin-demo-v9.9.9
if run plugin-demo-v9.9.9 "$r"; then
  fail "a tag that disagrees with Cargo.toml must be rejected even without a UI manifest"
fi
pass "a tag that disagrees with Cargo.toml is rejected without a UI manifest"

# ---------- 5. 版本与 UI package.json 不符 → 拒绝 ----------
r="$TMP/bad-ui"; new_repo "$r" "1.2.0" "1.3.0"
git -C "$r" tag plugin-demo-v1.2.0
if run plugin-demo-v1.2.0 "$r"; then
  fail "a tag that disagrees with the UI package.json must be rejected"
fi
pass "a tag that disagrees with the UI package.json is rejected"

# ---------- 6. tag 名字里的插件不存在 → 拒绝 ----------
r="$TMP/no-plugin"; new_repo "$r" "1.2.0" "1.2.0"
git -C "$r" tag plugin-nosuch-v1.2.0
if run plugin-nosuch-v1.2.0 "$r"; then
  fail "a tag naming an absent plugin must be rejected"
fi
pass "a tag naming an absent plugin is rejected"

# ---------- 7. 畸形版本 → 拒绝 ----------
r="$TMP/bad-semver"; new_repo "$r" "1.2.0" "1.2.0"
git -C "$r" tag plugin-demo-v1.2
if run plugin-demo-v1.2 "$r"; then
  fail "a non-semver plugin version must be rejected"
fi
pass "a non-semver plugin version is rejected"

# ---------- 8. 插件名含连字符也要能解析 ----------
r="$TMP/hyphen"; new_repo "$r" "1.2.0" "1.2.0"
mv "$r/plugins/demo" "$r/plugins/superpowers-kanban"
git -C "$r" -c user.email=t@example.com -c user.name=t add -A
git -C "$r" -c user.email=t@example.com -c user.name=t commit -q -m rename
git -C "$r" tag plugin-superpowers-kanban-v1.2.0
run plugin-superpowers-kanban-v1.2.0 "$r" \
  || fail "a hyphenated plugin name must parse"
pass "a hyphenated plugin name parses"

# ---------- 9. 主程序版本缺失 → 拒绝 ----------
r="$TMP/no-main"; new_repo "$r" "1.2.0" "1.2.0"
rm "$r/yi-agent-rs/Cargo.toml"
git -C "$r" -c user.email=t@example.com -c user.name=t add -A
git -C "$r" -c user.email=t@example.com -c user.name=t commit -q -m drop-main
git -C "$r" tag plugin-demo-v1.2.0
if run plugin-demo-v1.2.0 "$r"; then
  fail "a missing main manifest must be rejected"
fi
pass "a missing main manifest is rejected"

printf '%s\n' "all check-plugin-release cases passed"
