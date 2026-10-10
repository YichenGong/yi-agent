#!/usr/bin/env bash
#
# 构建并发布 macOS(arm64) 桌面 App 的 .dmg 到 GitLab Release。
#
# 用法:
#   CI_COMMIT_TAG=v0.1.4 bash desktop/scripts/release-dmg.sh
#   DRY_RUN=1 CI_COMMIT_TAG=v0.1.4 bash desktop/scripts/release-dmg.sh   # 只打印发布命令
#
# 设计: 见 docs/superpowers/specs/2026-10-09-gitlab-macos-release-design.md
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # desktop/
ROOT="$(cd "$HERE/.." && pwd)"                            # repo root

log() { printf '\n=== %s ===\n' "$*"; }

# --- 1. 环境自愈 ---------------------------------------------------------
# runner 是非交互 shell，PATH 缺 homebrew/cargo。对齐 .gitlab-ci.yml 与
# .github/workflows/release.yml 的既有做法。
export PATH="/opt/homebrew/bin:$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

for tool in node npm jq shasum; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERROR: '$tool' not found on PATH ($PATH)" >&2
    echo "       runner 上缺 node 时请执行: brew install node jq" >&2
    exit 1
  fi
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: 'cargo' not found; runner 上需装 rustup（见 .gitlab-ci.yml 的自愈逻辑）" >&2
  exit 1
fi

# --- 2. 版本门禁 ---------------------------------------------------------
TAG="${CI_COMMIT_TAG:-${1:-}}"
if [ -z "$TAG" ]; then
  echo "ERROR: 需要 tag：设置 CI_COMMIT_TAG 或传首个参数（如 v0.1.4）" >&2
  exit 1
fi
if ! [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "ERROR: tag 必须形如 v0.1.4，实际: $TAG" >&2
  exit 1
fi
VERSION="${TAG#v}"
export VERSION

CARGO_VERSION="$(grep '^version' "$ROOT/yi-agent-rs/Cargo.toml" | head -1 | awk -F'"' '{print $2}')"
if [ "$VERSION" != "$CARGO_VERSION" ]; then
  echo "ERROR: tag ($TAG) 与 yi-agent-rs/Cargo.toml 版本 (v$CARGO_VERSION) 不一致" >&2
  echo "       请先同步 Cargo.toml 再打 tag。" >&2
  exit 1
fi
log "版本校验通过: $VERSION (arm64)"

log "环境与版本检查完成"
