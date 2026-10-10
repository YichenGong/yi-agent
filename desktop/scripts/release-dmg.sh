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

# --- 3. 构建 -------------------------------------------------------------
# Tauri 的 bundle.targets 默认是 "all"，会连带产出 updater tar 等非必要物；
# 收窄到 app,dmg。仅改工作区文件，不提交。
log "收窄 bundle.targets 为 app,dmg"
CONF="$HERE/src-tauri/tauri.conf.json"
jq '.bundle.targets = ["app","dmg"]' "$CONF" > "$CONF.tmp"
mv "$CONF.tmp" "$CONF"

log "npm ci"
(cd "$HERE" && npm ci)

log "构建 sidecar（release 模式的 yi-agent CLI）"
(cd "$HERE" && npm run sidecar:release)

log "tauri build"
(cd "$HERE" && npm run tauri build)

# --- 4. 归档与校验和 -----------------------------------------------------
log "归档 dmg"
mkdir -p "$ROOT/dist"
DMG_SRC="$(ls "$HERE/src-tauri/target/release/bundle/dmg/"*.dmg | head -1)"
if [ -z "$DMG_SRC" ]; then
  echo "ERROR: 未找到 tauri 产出的 .dmg（$HERE/src-tauri/target/release/bundle/dmg/）" >&2
  exit 1
fi
ART="yi-agent_${VERSION}_aarch64.dmg"
export ART
cp "$DMG_SRC" "$ROOT/dist/$ART"
( cd "$ROOT/dist" && shasum -a 256 "$ART" | awk '{print $1}' > "$ART.sha256" )
log "产物: dist/$ART"
ls -lh "$ROOT/dist/$ART" "$ROOT/dist/$ART.sha256"

# --- 5. 发布 -------------------------------------------------------------
: "${CI_API_V4_URL:?缺少 CI_API_V4_URL（只能在 GitLab CI 里跑发布段）}"
: "${CI_PROJECT_ID:?缺少 CI_PROJECT_ID}"
: "${CI_COMMIT_TAG:?缺少 CI_COMMIT_TAG}"

PKG_NAME="yi-agent-macos"
PKG_BASE="$CI_API_V4_URL/projects/$CI_PROJECT_ID/packages/generic/$PKG_NAME/$VERSION"
INSTALLER="$HERE/packaging/install.sh"

# 鉴权：优先 JOB-TOKEN；若实例不允许 job token 建 Release，用 RELEASE_TOKEN 兜底。
# 参考 spec §3 决策 3/4。
AUTH_KIND="JOB-TOKEN"
AUTH_VALUE="${CI_JOB_TOKEN:-}"
if [ -n "${RELEASE_TOKEN:-}" ]; then
  AUTH_KIND="PRIVATE-TOKEN"
  AUTH_VALUE="$RELEASE_TOKEN"
fi
if [ -z "$AUTH_VALUE" ]; then
  echo "ERROR: 没有可用凭据（CI_JOB_TOKEN 与 RELEASE_TOKEN 均为空）" >&2
  exit 1
fi

upload() {  # upload <local-file> <remote-filename>
  curl --fail --silent --show-error \
    --header "$AUTH_KIND: $AUTH_VALUE" \
    --upload-file "$1" "$PKG_BASE/$2"
}

log "上传 dmg 与校验和到 Generic Package Registry"
if [ "${DRY_RUN:-0}" = "1" ]; then
  echo "[dry-run] curl --header \"$AUTH_KIND: ***\" --upload-file dist/$ART $PKG_BASE/$ART"
  echo "[dry-run] curl --header \"$AUTH_KIND: ***\" --upload-file dist/$ART.sha256 $PKG_BASE/$ART.sha256"
else
  ( cd "$ROOT" && upload "dist/$ART" "$ART" )
  ( cd "$ROOT" && upload "dist/$ART.sha256" "$ART.sha256" )
  upload "$INSTALLER" "install.sh"
fi

log "创建/更新 Release $CI_COMMIT_TAG"
DESCRIPTION="$(cat <<EOF
## macOS 桌面 App（Apple Silicon）

下载 \`$ART\` 后：

1. \`bash install.sh ./$ART\`（或双击 dmg 把 yi-agent.app 拖进「应用程序」后执行
   \`xattr -dr com.apple.quarantine /Applications/yi-agent.app\`）
2. **配置模型 key（必需，App 内没有设置入口）**：
   \`mkdir -p ~/.yi-agent && echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env\`
3. 从「应用程序」打开 yi-agent。

未签名产物；首次打开如被拦，到 System Settings → Privacy & Security 点 "Open Anyway"。
EOF
)"

PAYLOAD="$(jq -n \
  --arg tag "$CI_COMMIT_TAG" \
  --arg name "yi-agent $VERSION (macOS arm64)" \
  --arg desc "$DESCRIPTION" \
  --arg dmg_url "$PKG_BASE/$ART" \
  --arg dmg_name "$ART" \
  --arg sha_url "$PKG_BASE/$ART.sha256" \
  --arg ins_url "$PKG_BASE/install.sh" \
  '{
     tag_name: $tag,
     name: $name,
     description: $desc,
     assets: {
       links: [
         {name: $dmg_name, url: $dmg_url,   filepath: ("/" + $dmg_name), link_type: "package"},
         {name: ($dmg_name + ".sha256"), url: $sha_url, filepath: ("/" + $dmg_name + ".sha256"), link_type: "package"},
         {name: "install.sh", url: $ins_url, filepath: "/install.sh", link_type: "package"}
       ]
     }
   }')"

RELEASE_API="$CI_API_V4_URL/projects/$CI_PROJECT_ID/releases"

if [ "${DRY_RUN:-0}" = "1" ]; then
  echo "[dry-run] release payload:"
  echo "$PAYLOAD" | jq .
else
  STATUS="$(curl --silent --output /dev/null --write-out '%{http_code}' \
    --header "$AUTH_KIND: $AUTH_VALUE" "$RELEASE_API/$CI_COMMIT_TAG")"
  if [ "$STATUS" = "200" ]; then
    echo "Release 已存在，更新中 (PUT)"
    curl --fail --silent --show-error --request PUT \
      --header "$AUTH_KIND: $AUTH_VALUE" \
      --header "Content-Type: application/json" \
      --data "$PAYLOAD" "$RELEASE_API/$CI_COMMIT_TAG" >/dev/null
  else
    echo "创建 Release (POST)"
    curl --fail --silent --show-error --request POST \
      --header "$AUTH_KIND: $AUTH_VALUE" \
      --header "Content-Type: application/json" \
      --data "$PAYLOAD" "$RELEASE_API" >/dev/null
  fi
fi

log "完成: $CI_PROJECT_URL/-/releases/$CI_COMMIT_TAG"
