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
# 收窄到 app,dmg。同时把 tag 版本注入 tauri.conf.json 与桌面 crate 的
# Cargo.toml——否则 App 的 CFBundleShortVersionString 会一直停在写死的
# 0.1.0（产物文件名被本脚本重命名过，看不出这个不一致）。
# 仅改工作区文件，不提交。
log "收窄 bundle.targets 为 app,dmg，并注入版本 $VERSION"
CONF="$HERE/src-tauri/tauri.conf.json"
jq --arg v "$VERSION" '.bundle.targets = ["app","dmg"] | .version = $v' "$CONF" > "$CONF.tmp"
mv "$CONF.tmp" "$CONF"
sed -i.bak -E "s/^version = \".*\"/version = \"$VERSION\"/" "$HERE/src-tauri/Cargo.toml"
rm -f "$HERE/src-tauri/Cargo.toml.bak"

log "npm ci"
(cd "$HERE" && npm ci)

log "构建 sidecar（release 模式的 yi-agent CLI）"
(cd "$HERE" && npm run sidecar:release)

log "tauri build"
(cd "$HERE" && npm run tauri build)

# --- 4. 归档与校验和 -----------------------------------------------------
log "归档 dmg 与独立 CLI 二进制"
mkdir -p "$ROOT/dist"

# 4a. 桌面 App 的 dmg。版本已注入 tauri 配置，故 tauri 产出的文件名恰好
#     等于 ART；按精确文件名取，避免持久 runner 的 build 目录里残留的旧
#     版本 dmg 被误发（旧的 `ls | head` 会按字典序选中它）。
ART="yi-agent_${VERSION}_aarch64.dmg"
export ART
# tauri 的产物目录跟着 CARGO_TARGET_DIR 走。GitLab 侧目前没设它，所以
# 默认值还能用；但一旦有人为复用缓存加上（GitHub 侧就是这么干的），
# 写死 src-tauri/target 会变成「构建全绿、发版找不到 dmg」。统一走
# cargo metadata 解析，两个 runner 都成立。
DMG_TARGET_DIR="$(cargo metadata --manifest-path "$HERE/src-tauri/Cargo.toml" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$DMG_TARGET_DIR" ] || DMG_TARGET_DIR="$HERE/src-tauri/target"
DMG_SRC="$DMG_TARGET_DIR/release/bundle/dmg/$ART"
if [ ! -f "$DMG_SRC" ]; then
  echo "ERROR: 未找到 tauri 产出的 ${ART}（目录内容如下）" >&2
  ls -l "$DMG_TARGET_DIR/release/bundle/dmg/" >&2 || true
  exit 1
fi
cp "$DMG_SRC" "$ROOT/dist/$ART"
( cd "$ROOT/dist" && shasum -a 256 "$ART" | awk '{print $1}' > "$ART.sha256" )
log "产物: dist/$ART"
ls -lh "$ROOT/dist/$ART" "$ROOT/dist/$ART.sha256"

# 4b. 独立 CLI（arm64）。直接复用第 3 步 sidecar 已编译好的 release 二进制，
#     避免再跑一次等价的交叉编译。命名对齐 justfile 的 package-target。
HOST_TRIPLE="$(rustc -vV 2>/dev/null | awk '/^host:/ {print $2}' || true)"
[ -n "$HOST_TRIPLE" ] || HOST_TRIPLE="aarch64-apple-darwin"
CLI_ART="yi-agent-${HOST_TRIPLE}.tar.gz"
export CLI_ART
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/yi-agent-rs/target}"
CLI_BIN="$TARGET_DIR/release/yi-agent"
if [ ! -x "$CLI_BIN" ]; then
  echo "ERROR: 未找到 sidecar CLI 二进制（${CLI_BIN}）" >&2
  exit 1
fi
tar -czf "$ROOT/dist/$CLI_ART" -C "$(dirname "$CLI_BIN")" yi-agent
( cd "$ROOT/dist" && shasum -a 256 "$CLI_ART" | awk '{print $1}' > "$CLI_ART.sha256" )
log "产物: dist/$CLI_ART"
ls -lh "$ROOT/dist/$CLI_ART" "$ROOT/dist/$CLI_ART.sha256"

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

log "上传桌面 App 与 CLI 到 Generic Package Registry"
if [ "${DRY_RUN:-0}" = "1" ]; then
  for f in "dist/$ART" "dist/$ART.sha256" "dist/$CLI_ART" "dist/$CLI_ART.sha256" "$INSTALLER"; do
    echo "[dry-run] curl --header \"$AUTH_KIND: ***\" --upload-file $f $PKG_BASE/$(basename "$f")"
  done
else
  ( cd "$ROOT" && upload "dist/$ART" "$ART" )
  ( cd "$ROOT" && upload "dist/$ART.sha256" "$ART.sha256" )
  ( cd "$ROOT" && upload "dist/$CLI_ART" "$CLI_ART" )
  ( cd "$ROOT" && upload "dist/$CLI_ART.sha256" "$CLI_ART.sha256" )
  upload "$INSTALLER" "install.sh"
fi

log "创建/更新 Release $CI_COMMIT_TAG"
DESCRIPTION="$(cat <<EOF
## macOS 桌面 App（Apple Silicon）

下载 \`$ART\` 后：

1. \`bash install.sh ./$ART\`（或双击 dmg 把 yi-agent.app 拖进「应用程序」后执行
   \`xattr -dr com.apple.quarantine /Applications/yi-agent.app\`）
2. **配置模型（必需）**：首次打开 yi-agent 会弹引导向导，填「API 密钥」与
   「模型标识」后点测试连接，通过即写入 \`~/.yi-agent/models.json\`。
   也可以之后在 App 的「设置 → 模型」里增删改。
3. 从「应用程序」打开 yi-agent。

未签名产物；首次打开如被拦，到 System Settings → Privacy & Security 点 "Open Anyway"。

## 终端版（TUI）

App 内已内置同一份 CLI，安装脚本会自动建好入口，**不需要另下产物**：

\`\`\`
/usr/local/bin/yi-agent -> /Applications/yi-agent.app/Contents/MacOS/yi-agent
\`\`\`

装完后 \`cd\` 到项目目录直接运行 \`yi-agent\`。App 与 TUI 共用同一份模型清单
\`~/.yi-agent/models.json\`，配一次即可；首启也会给出同样的终端问答引导。

若安装器提示 \`/usr/local/bin\` 不可写，它会退到 \`~/.local/bin\` 并打印需要添加的
PATH 行；不会修改你的 shell 配置。

## 升级

下载新 dmg 后重跑同一条命令：

\`\`\`bash
bash install.sh ./yi-agent_<新版本>_aarch64.dmg
\`\`\`

安装器会：自动退出正在运行的 App → 停止旧版 runtime daemon（若正在跑子 agent
任务会先询问）→ 原子替换 App → 重建命令行入口。配置与历史都会保留。

- 安装位置固定为 \`/Applications\`；非管理员账户请用 \`sudo bash install.sh ...\`
- 不支持降级：数据库 schema 只升不降，回退旧版本可能读到不兼容数据
- 安装器会处理 \`ai.yi-agent.*\` 的 LaunchAgent，但只动「指向本 App 且原本已加载」
  的那份；指向开发构建的会被跳过并告警

校验（可选）：\`shasum -a 256 $ART\` 的输出应与 \`$ART.sha256\` 的内容一致。
EOF
)"

PAYLOAD="$(jq -n \
  --arg tag "$CI_COMMIT_TAG" \
  --arg name "yi-agent $VERSION (macOS arm64)" \
  --arg desc "$DESCRIPTION" \
  --arg dmg_url "$PKG_BASE/$ART" \
  --arg dmg_name "$ART" \
  --arg sha_url "$PKG_BASE/$ART.sha256" \
  --arg cli_url "$PKG_BASE/$CLI_ART" \
  --arg cli_name "$CLI_ART" \
  --arg cli_sha_url "$PKG_BASE/$CLI_ART.sha256" \
  --arg ins_url "$PKG_BASE/install.sh" \
  '{
     tag_name: $tag,
     name: $name,
     description: $desc,
     assets: {
       links: [
         {name: $dmg_name, url: $dmg_url,   filepath: ("/" + $dmg_name), link_type: "package"},
         {name: ($dmg_name + ".sha256"), url: $sha_url, filepath: ("/" + $dmg_name + ".sha256"), link_type: "package"},
         {name: $cli_name, url: $cli_url,   filepath: ("/" + $cli_name), link_type: "package"},
         {name: ($cli_name + ".sha256"), url: $cli_sha_url, filepath: ("/" + $cli_name + ".sha256"), link_type: "package"},
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
