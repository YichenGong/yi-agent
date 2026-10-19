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

bin_candidates() {
  printf '%s\n' "/usr/local/bin"
  printf '%s\n' "$HOME/.local/bin"
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
