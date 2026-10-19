#!/usr/bin/env bash
#
# yi-agent 桌面 App 安装器（macOS, arm64）。
# 未签名产物：挂载 dmg → 原子替换 /Applications → 清 quarantine →
# 建 CLI 软链 → 清理旧 runtime daemon。
#
# 本文件既可执行，也可被测试 source：source 时只定义函数，不产生副作用。

APP_NAME="yi-agent.app"

app_path() {
  printf '%s\n' "${YI_AGENT_APP_PATH:-/Applications/$APP_NAME}"
}

# 候选目录：测试可经 YI_AGENT_BIN_DIRS（冒号分隔）覆盖。
# 用 ${VAR+set} 判断「是否设置」而非「是否非空」，这样置空即表示无候选。
bin_candidates() {
  if [ -n "${YI_AGENT_BIN_DIRS+set}" ]; then
    case "$YI_AGENT_BIN_DIRS" in
      "") return 0 ;;
    esac
    printf '%s\n' "$YI_AGENT_BIN_DIRS" | tr ':' '\n'
    return 0
  fi
  printf '%s\n' "/usr/local/bin"
  printf '%s\n' "$HOME/.local/bin"
}

# 第一个可写候选目录。都没有则返回 1（安装继续，只跳过建链）。
pick_bin_dir() {
  local dir
  while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    if [ -w "$dir" ]; then
      printf '%s\n' "$dir"
      return 0
    fi
    if [ ! -d "$dir" ] && mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; then
      printf '%s\n' "$dir"
      return 0
    fi
  done <<EOF
$(bin_candidates)
EOF
  return 1
}

# 建软链。目标 CLI 不存在时不建，返回 1。
link_cli() {
  local app="$1" bin_dir="$2"
  local cli="$app/Contents/MacOS/yi-agent"
  if [ ! -x "$cli" ]; then
    echo "ERROR: 未找到内置 CLI：$cli" >&2
    return 1
  fi
  ln -sfn "$cli" "$bin_dir/yi-agent"
}

# 判断 PATH 上实际命中的是不是我们这条软链。
link_status() {
  local app="$1" bin_dir="$2"
  local want="$bin_dir/yi-agent"
  if [ ! -L "$want" ]; then
    printf 'missing\n'
    return 0
  fi
  local hit
  hit="$(command -v yi-agent 2>/dev/null || true)"
  if [ -z "$hit" ]; then
    printf 'missing\n'
  elif [ "$hit" = "$want" ]; then
    printf 'ok\n'
  else
    printf 'shadowed:%s\n' "$hit"
  fi
}

# 注入的拷贝命令可能带参数，故用 eval。
copy_runner() {
  local src="$1" dest="$2"
  if [ -n "${YI_AGENT_COPY_CMD:-}" ]; then
    eval "$YI_AGENT_COPY_CMD \"$src\" \"$dest\""
  else
    ditto "$src" "$dest"
  fi
}

# 原子替换：先拷到 <dest>.new，成功后才替换。任何一步失败都保留旧目录。
replace_app() {
  local src="$1" dest="$2"
  local stage="$dest.new"
  local backup="$dest.old"

  rm -rf "$stage" "$backup"
  if ! copy_runner "$src" "$stage"; then
    echo "ERROR: 拷贝到暂存目录失败，已保留原版本" >&2
    rm -rf "$stage"
    return 1
  fi
  if [ -e "$dest" ]; then
    mv "$dest" "$backup" || { rm -rf "$stage"; return 1; }
  fi
  if ! mv "$stage" "$dest"; then
    echo "ERROR: 替换失败，正在恢复原版本" >&2
    [ -e "$backup" ] && mv "$backup" "$dest"
    rm -rf "$stage"
    return 1
  fi
  rm -rf "$backup"
}

# 读 App 的 CFBundleShortVersionString；失败时打印空串（调用方据此判断）。
app_version() {
  local plist="$1/Contents/Info.plist"
  [ -f "$plist" ] || return 0
  plutil -extract CFBundleShortVersionString raw -o - "$plist" 2>/dev/null || true
}

# 跑内置 CLI 的 --version；失败时打印空串。
cli_version() {
  local cli="$1/Contents/MacOS/yi-agent"
  [ -x "$cli" ] || return 0
  "$cli" --version 2>/dev/null | head -1 || true
}

version_banner() {
  local old="$1" new="$2"
  if [ -z "$old" ]; then
    printf '已安装 yi-agent %s\n' "$new"
  else
    printf '检测到已安装版本 %s → 升级到 %s\n' "$old" "$new"
  fi
}

main() {
  set -euo pipefail

  local dmg="${1:-}"
  if [ -z "$dmg" ]; then
    dmg="$(ls -t "$HOME/Downloads/"yi-agent_*_aarch64.dmg 2>/dev/null | head -1 || true)"
  fi
  if [ -z "$dmg" ] || [ ! -f "$dmg" ]; then
    echo "用法: bash install.sh /path/to/yi-agent_x.y.z_aarch64.dmg" >&2
    echo "（未传参时会在 ~/Downloads 找 yi-agent_*_aarch64.dmg）" >&2
    return 1
  fi

  echo "当前安装器版本：$(basename "$dmg")"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  main "$@"
fi
