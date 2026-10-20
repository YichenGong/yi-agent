#!/usr/bin/env bash
# 把 Tauri 产出的 .app 修复签名后放进 npm 的 darwin-arm64 子包。
#
# 与 release-dmg.sh 的分工：那个脚本产出 dmg 发到 GitLab（内网分发），
# 这个脚本产出 .app 塞进 npm 包（公网分发）。两者共用同一套版本注入
# 与 Tauri 构建，但产物形式不同——npm 场景下 .app 目录可直接复制，
# 不需要 dmg 那层挂载。
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"          # desktop/
ROOT="$(cd "$HERE/.." && pwd)"                    # repo root
NPM_PKG="$ROOT/yi-agent-rs/yi-agent-cli/platforms/darwin-arm64"

log() { printf '>>> %s\n' "$*"; }

# --- 1. 环境 -------------------------------------------------------------
for tool in node npm jq codesign; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERROR: '$tool' not found on PATH ($PATH)" >&2
    exit 1
  fi
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "ERROR: 桌面 App 只能在 macOS 上构建（当前 $(uname -s)）" >&2
  exit 1
fi
if [ "$(uname -m)" != "arm64" ]; then
  echo "ERROR: 只产出 arm64 .app，当前机器是 $(uname -m)" >&2
  exit 1
fi

# --- 2. 版本 -------------------------------------------------------------
VERSION="$(grep '^version' "$ROOT/yi-agent-rs/Cargo.toml" | head -1 | awk -F'"' '{print $2}')"
if [ -z "$VERSION" ]; then
  echo "ERROR: 读不到 yi-agent-rs/Cargo.toml 的 version" >&2
  exit 1
fi

if [ -n "${CI_COMMIT_TAG:-}" ]; then
  if [ "v$VERSION" != "$CI_COMMIT_TAG" ]; then
    echo "ERROR: tag ($CI_COMMIT_TAG) 与 Cargo.toml ($VERSION) 不一致" >&2
    exit 1
  fi
  log "版本校验通过: $VERSION"
fi

# --- 3. 构建 -------------------------------------------------------------
# 版本必须注入 tauri.conf.json，否则 App 的 CFBundleShortVersionString
# 会一直停在写死的值，postinstall 的版本比对就永远判定「需要替换」。
CONF="$HERE/src-tauri/tauri.conf.json"
log "注入版本 $VERSION 并收窄 targets 为 app"
jq --arg v "$VERSION" '.bundle.targets = ["app"] | .version = $v' "$CONF" > "$CONF.tmp"
mv "$CONF.tmp" "$CONF"
# 只在工作区改，不提交；CI 里用完即弃。
trap 'git -C "$ROOT" checkout -- "$CONF" 2>/dev/null || true' EXIT

log "npm ci"
(cd "$HERE" && npm ci)

log "构建 sidecar（release 模式 CLI）"
(cd "$HERE" && npm run sidecar:release)

log "tauri build（只出 .app）"
(cd "$HERE" && npm run tauri build -- --bundles app)

APP_SRC="$HERE/src-tauri/target/release/bundle/macos/yi-agent.app"
if [ ! -d "$APP_SRC" ]; then
  echo "ERROR: 未找到 $APP_SRC" >&2
  ls -l "$HERE/src-tauri/target/release/bundle/macos/" >&2 || true
  exit 1
fi

# --- 4. 修签名 -----------------------------------------------------------
# Tauri 产出的签名是坏的：codesign --verify 报
# "code has no resources but signature indicates they must be present"。
# ad-hoc 重签可修复（实测 verify 通过、启动正常）。
log "ad-hoc 重签"
codesign --force --deep --sign - "$APP_SRC"

log "校验签名"
if ! codesign --verify --deep --strict "$APP_SRC"; then
  echo "ERROR: 重签后签名仍无效，拒绝打包坏产物" >&2
  exit 1
fi

EXPECTED="$VERSION"
ACTUAL="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
  "$APP_SRC/Contents/Info.plist" 2>/dev/null || true)"
if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "ERROR: App 版本 $ACTUAL 与预期 $EXPECTED 不一致" >&2
  exit 1
fi

# --- 5. 就位 -------------------------------------------------------------
log "复制到 npm 子包"
rm -rf "$NPM_PKG/yi-agent.app"
mkdir -p "$NPM_PKG"
cp -R "$APP_SRC" "$NPM_PKG/yi-agent.app"

du -sh "$NPM_PKG/yi-agent.app"
log "完成: $NPM_PKG/yi-agent.app ($VERSION)"
