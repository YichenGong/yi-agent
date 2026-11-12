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

gh_note() { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::notice title=npm-pack-desktop::$*" || true; }
gh_err()  { [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error title=npm-pack-desktop::$*" || true; }

gh_note "stage: npm ci"
log "npm ci"
(cd "$HERE" && npm ci) || { gh_err "npm ci failed"; exit 1; }

gh_note "stage: build sidecar"
log "构建 sidecar（release 模式 CLI）"
(cd "$HERE" && npm run sidecar:release) || { gh_err "sidecar:release failed"; exit 1; }

gh_note "stage: tauri build"
log "tauri build（只出 .app）"
# 完整输出落盘，失败时把尾部转成 annotation。原因：这个仓库是私有的，CI
# 日志要 admin 权限才能读（匿名 API 对 logs 与 job logs 都回 403），唯一能
# 匿名读到的是 check-run annotations。而 `tauri build` 的失败原因藏在几十行
# 输出里（cargo 报错、build script panic、bundle 失败），只回一句
# "tauri build failed" 等于什么都没说。
TAURI_LOG="$(mktemp -t yi-agent-tauri-build)"
if ! (cd "$HERE" && npm run tauri build -- --bundles app) 2>&1 | tee "$TAURI_LOG"; then
  # 也把尾部原样打到 stdout，便于本地复现时阅读。
  echo "---- tauri build: last 20 lines ----"
  tail -20 "$TAURI_LOG"
  echo "-------------------------------------"
  # annotation 单条有长度上限，压缩连续空白并截断，保留最能定位的尾部。
  detail="$(tail -20 "$TAURI_LOG" | tr -s '[:space:]' ' ' | cut -c1-1200)"
  gh_err "tauri build failed: $detail"
  rm -f "$TAURI_LOG"
  exit 1
fi
rm -f "$TAURI_LOG"

# target 目录不能假定是 src-tauri/target：CI 把 CARGO_TARGET_DIR 指到
# $HOME/cargo-target/github，tauri 的产物会跟着走。cargo metadata 是唯一
# 真源（同时认 CARGO_TARGET_DIR 与 .cargo/config 的 build.target-dir），
# 写死路径会让「编译成功但找不到 .app」这种最难查的失败复活。
TAURI_TARGET_DIR="$(cargo metadata --manifest-path "$HERE/src-tauri/Cargo.toml" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TAURI_TARGET_DIR" ] || TAURI_TARGET_DIR="$HERE/src-tauri/target"
APP_SRC="$TAURI_TARGET_DIR/release/bundle/macos/yi-agent.app"
if [ ! -d "$APP_SRC" ]; then
  echo "::error title=npm-pack-desktop::no .app at $APP_SRC" || true
  echo "ERROR: 未找到 $APP_SRC" >&2
  ls -l "$TAURI_TARGET_DIR/release/bundle/macos/" >&2 || true
  exit 1
fi

# --- 4. 签名与运行时清单 -------------------------------------------------
# 顺序固定，由 finalize-runtime-bundle.sh 保证：先签 sidecar（字节定稿）→
# 写 runtime-sidecar.json（含最终 digest）→ 签外层 app → verify 并复核
# digest。**不能再 --deep**：它会把已签的 sidecar 重签一遍，使刚写下的
# digest 与实际文件不符，而清单正是桌面壳用来证明「要跑的 daemon 就是
# 我发的那个」的依据。
gh_note "stage: finalize bundle"
log "finalize 运行时清单并重签"
bash "$HERE/scripts/finalize-runtime-bundle.sh" "$APP_SRC" || { gh_err "finalize-runtime-bundle failed"; exit 1; }

gh_note "stage: verify signature"
log "校验签名"
if ! codesign --verify --strict "$APP_SRC"; then
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
