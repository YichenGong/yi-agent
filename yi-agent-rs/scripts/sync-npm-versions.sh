#!/usr/bin/env bash
# 把 npm 发布版本同步到主包、平台子包，以及主包的 optionalDependencies。
#
# 为什么需要它：`just npm-pack` 过去只 sed `"version"` 字段。主包的
# optionalDependencies 一直写着 0.1.0，于是 `npm i @yi-agent/yi-agent@0.1.3`
# 装到的是 0.1.0 的平台子包（实测量 430832 字节、`--version` 输出
# `yi-agent agent CLI (placeholder)`），而真正的二进制躺在 0.1.3 子包里。
#
# 用法：sync-npm-versions.sh <version> [package-dir]
#   <version>     不带 v 的语义化版本，如 0.1.7
#   [package-dir] 主包目录，默认是脚本上一级的 yi-agent-cli
#
# 退出码：0 = 已同步；1 = 包结构自检失败（依赖表与平台目录脱节）；2 = 用法错误。
# 自检失败必须在发布前拦住：依赖表与平台子包脱节正是本次事故的形态。
set -euo pipefail

usage() {
  echo "usage: $(basename "$0") <version> [package-dir]" >&2
}

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  usage
  exit 2
fi

version="$1"
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  echo "ERROR: invalid version: $version" >&2
  usage
  exit 2
fi

script_dir="$(cd "$(dirname "$0")" && pwd)"
pkg_dir="${2:-$script_dir/../yi-agent-cli}"
if [ ! -d "$pkg_dir" ]; then
  echo "ERROR: package directory not found: $pkg_dir" >&2
  exit 1
fi
pkg_dir="$(cd "$pkg_dir" && pwd)"

main_pkg="$pkg_dir/package.json"
if [ ! -f "$main_pkg" ]; then
  echo "ERROR: main package.json not found: $main_pkg" >&2
  exit 1
fi

# 平台目录名（darwin-arm64 等）与 npm 包名后缀同构，直接由目录推导，
# 避免再维护一份手写清单而与 platforms/ 漂移。
platform_dirs=()
for dir in "$pkg_dir"/platforms/*/; do
  [ -d "$dir" ] || continue
  platform_dirs+=("$(basename "$dir")")
done
if [ "${#platform_dirs[@]}" -eq 0 ]; then
  echo "ERROR: no platform packages under $pkg_dir/platforms" >&2
  exit 1
fi

# 主包的 optionalDependencies 必须与 platforms/ 目录逐一对应：
# 少了 → 装到该平台时拉不到二进制；多了 → 依赖一个不存在的子包。
for dir in "${platform_dirs[@]}"; do
  name="@yi-agent/yi-agent-$dir"
  if ! grep -qF -- "\"$name\"" "$main_pkg"; then
    echo "ERROR: optionalDependencies is missing $name (platform dir platforms/$dir exists)" >&2
    exit 1
  fi
done
for name in $(grep -oE '"@yi-agent/yi-agent-[^"]+"' "$main_pkg" | tr -d '"'); do
  suffix="${name#@yi-agent/yi-agent-}"
  found=0
  for dir in "${platform_dirs[@]}"; do
    if [ "$suffix" = "$dir" ]; then found=1; break; fi
  done
  if [ "$found" -eq 0 ]; then
    echo "ERROR: optionalDependencies lists $name but platforms/$suffix does not exist" >&2
    exit 1
  fi
done

# 只替换版本号字段：name / os / cpu / bin / files 这些分发契约一个字都不动。
set_version() { # set_version <file>
  local file="$1" tmp
  tmp="$(mktemp)"
  sed -E 's/^([[:space:]]*"version"[[:space:]]*:[[:space:]]*)"[^"]*"/\1"'"$version"'"/' "$file" > "$tmp"
  cat "$tmp" > "$file"
  rm -f "$tmp"
}

set_version "$main_pkg"
for dir in "${platform_dirs[@]}"; do
  set_version "$pkg_dir/platforms/$dir/package.json"
done

# optionalDependencies 里的值跟着版本走，否则整次同步就白做了。
tmp="$(mktemp)"
sed -E 's/("@yi-agent\/yi-agent-[a-z0-9-]+"[[:space:]]*:[[:space:]]*)"[^"]*"/\1"'"$version"'"/' \
  "$main_pkg" > "$tmp"
cat "$tmp" > "$main_pkg"
rm -f "$tmp"

echo "Synced npm packages in $pkg_dir to $version"
