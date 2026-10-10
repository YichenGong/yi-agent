#!/usr/bin/env bash
#
# yi-agent 桌面 App 安装器（macOS, arm64）。
# 未签名产物：挂载 dmg → 拷进 /Applications → 清 quarantine。
set -euo pipefail

APP_NAME="yi-agent.app"
DEST="/Applications/$APP_NAME"

MOUNT_POINT="$(mktemp -d /tmp/yi-agent-dmg.XXXXXX)"
DMG="${1:-}"

if [ -z "$DMG" ]; then
  DMG="$(ls -t "$HOME/Downloads/"yi-agent_*_aarch64.dmg 2>/dev/null | head -1 || true)"
fi
if [ -z "$DMG" ] || [ ! -f "$DMG" ]; then
  echo "用法: bash install.sh /path/to/yi-agent_x.y.z_aarch64.dmg" >&2
  echo "（未传参时会在 ~/Downloads 找 yi-agent_*_aarch64.dmg）" >&2
  exit 1
fi

cleanup() { hdiutil detach "$MOUNT_POINT" >/dev/null 2>&1 || true; rm -rf "$MOUNT_POINT"; }
trap cleanup EXIT

echo "挂载 $DMG ..."
hdiutil attach "$DMG" -mountpoint "$MOUNT_POINT" -nobrowse -quiet

if [ ! -d "$MOUNT_POINT/$APP_NAME" ]; then
  echo "ERROR: dmg 内未找到 $APP_NAME" >&2
  exit 1
fi

echo "安装到 $DEST ..."
rm -rf "$DEST"
ditto "$MOUNT_POINT/$APP_NAME" "$DEST"

echo "清除 quarantine 属性（未签名产物必需）..."
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

cat <<'EOF'

安装完成。还需要一步才能用：

  App 没有模型 key 设置界面，必须手写配置文件：

    mkdir -p ~/.yi-agent
    echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env

  然后从「启动台 / 应用程序」打开 yi-agent。

若首次打开仍提示「无法验证开发者」：
  System Settings → Privacy & Security → 点 "Open Anyway"。
EOF
