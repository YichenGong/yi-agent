#!/usr/bin/env bash
# 独立插件发布门禁：确认一个插件 tag 是**插件**发布，且身份自洽。
#
# 用法: check-plugin-release.sh <tag> [repo-dir]
#
# 为什么单列一个脚本：主程序 `release.yml` 由 `v*.*.*` 触发，跑在 self-hosted
# runner 上并持有签名密钥。插件必须走自己的 tag 前缀与自己的 workflow，
# 否则一次插件 UI 小改就会发布一个新版本的主程序；更糟的是，main 的版本号
# 会因插件更新而变化，用户升级主程序时拿到的其实是"看板改了个字"。
#
# 规则（design §13 / T26 Step 2）：
#   1. tag 必须匹配 `plugin-<name>-v<semver>`，不冒用主程序 `v<semver>`；
#   2. tag 指向的提交必须在 main 可达（未合并的分支不得发布）；
#   3. 版本号必须与插件 workspace 的 Cargo.toml、UI 的 package.json 一致；
#   4. 主程序的 workspace 版本**不得**因插件发布而变化（这里只检查"存在且可读"，
#      真正的"没变"由 CI 对比基线完成——脚本不该猜基线）。
#
# 退出码：0 通过，1 规则不满足，2 用法错误。
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  printf 'usage: %s <tag> [repo-dir]\n' "$0" >&2
  exit 2
fi

tag="$1"
repo="${2:-.}"

fail() { printf 'plugin release gate FAILED: %s\n' "$*" >&2; exit 1; }

[ -n "$tag" ] || fail "empty tag name"

# ---------- 1. tag 形状：插件 tag 不得冒用主程序 tag ----------
case "$tag" in
  plugin-*-v*.*.*) ;;
  v*.*.*)
    fail "tag '$tag' looks like a main-app release; plugins use 'plugin-<name>-v<semver>'"
    ;;
  *)
    fail "tag '$tag' is not a plugin release tag (expected 'plugin-<name>-v<semver>')"
    ;;
esac

# 解析出插件名与版本。用贪心前缀剥离：插件名本身可以含 `-`。
rest="${tag#plugin-}"
plugin="${rest%-v*}"
version="${rest##*-v}"
[ -n "$plugin" ] || fail "tag '$tag' has an empty plugin name"
case "$version" in
  *[!0-9.]*|.*|*.) fail "tag '$tag' has a malformed version '$version'" ;;
esac
case "$version" in
  *.*.*) ;;
  *) fail "tag '$tag' has a version '$version' that is not semver-shaped" ;;
esac

# ---------- 2. tag 必须落在 main 可达的提交上 ----------
if [ -d "$repo/.git" ] || git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
  git -C "$repo" rev-parse --verify --quiet "refs/tags/$tag^{commit}" >/dev/null \
    || fail "tag '$tag' does not exist in $repo"
  base="${PLUGIN_RELEASE_BASE:-main}"
  git -C "$repo" rev-parse --verify --quiet "refs/heads/$base" >/dev/null \
    || fail "base branch '$base' does not exist"
  git -C "$repo" merge-base --is-ancestor "refs/tags/$tag^{commit}" "refs/heads/$base" \
    || fail "tag '$tag' is not an ancestor of '$base'; publish only from merged commits"
else
  fail "$repo is not a git repository"
fi

# ---------- 3. 版本必须与插件 workspace / UI 一致 ----------
plugin_dir="$repo/plugins/$plugin"
[ -d "$plugin_dir" ] || fail "tag names plugin '$plugin' but $plugin_dir does not exist"

cargo_manifest="$plugin_dir/Cargo.toml"
[ -f "$cargo_manifest" ] || fail "$cargo_manifest is missing"

# 读 workspace 版本：优先 `[workspace.package] version`，其次根 `[package] version`。
read_cargo_version() {
  awk '
    /^\[/ { section = $0; next }
    section ~ /^\[(workspace\.package|package)\]$/ && $0 ~ /^version[[:space:]]*=/ {
      gsub(/.*=[[:space:]]*"/, ""); gsub(/".*/, ""); print; exit
    }
  ' "$1"
}

workspace_version="$(read_cargo_version "$cargo_manifest")"
[ -n "$workspace_version" ] \
  || fail "$cargo_manifest declares no [workspace.package]/[package] version"

ui_manifest="$plugin_dir/ui/package.json"
ui_version=""
if [ -f "$ui_manifest" ]; then
  # 不依赖 node/jq：发布机可能没有 Node，而 `just ci` 明确不要求 Node。
  ui_version="$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$ui_manifest" | head -n 1)"
fi

[ "$version" = "$workspace_version" ] \
  || fail "tag says $version but $cargo_manifest says $workspace_version"
if [ -n "$ui_version" ] && [ "$version" != "$ui_version" ]; then
  fail "tag says $version but $ui_manifest says $ui_version"
fi

# ---------- 4. 主程序版本必须存在且与插件版本解耦 ----------
main_manifest="$repo/yi-agent-rs/Cargo.toml"
[ -f "$main_manifest" ] || fail "$main_manifest is missing"
main_version="$(read_cargo_version "$main_manifest")"
[ -n "$main_version" ] || fail "$main_manifest declares no workspace version"

# 插件版本变化**不得**推动主程序版本。这里只能查到当前值，真正的"没变"
# 需要基线对比：CI 里对 `git show <base>:yi-agent-rs/Cargo.toml` 再跑一遍本脚本
# 的 extractor，两值相等才算通过。脚本把值打出来，供 CI 比较。
printf 'plugin=%s version=%s main-version=%s\n' "$plugin" "$version" "$main_version"
