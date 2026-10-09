# GitLab 内网发布 macOS 桌面 App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 打 `v*.*.*` tag 时，在 GitLab 内网流水线里自动构建 macOS（arm64）桌面 App 的 `.dmg` 并发布为 GitLab Release 资产，供内网同事一键下载。

**Architecture:** GitLab CI 新增一个 `release` stage（tag 触发、`mac-mini` self-hosted runner 上跑），job 只做一件事——调用 `desktop/scripts/release-dmg.sh`。脚本负责：环境自愈 → 版本门禁 → 构建 dmg（Tauri + sidecar）→ 上传到 Generic Package Registry（不过期）→ 调 Release API 建 Release 并把 dmg 挂成带 `filepath` 的直链。不使用 `release:` 关键字（自建实例的 shell executor 不预装 release-cli）。

**Tech Stack:** GitLab CI（shell executor）、Tauri 2.x CLI、npm 20+、`curl` + `jq`、GitLab Generic Package Registry + Releases API。

## Global Constraints

- **仅 arm64**（Apple Silicon）。不构建 `x86_64-apple-darwin`，不支持 Intel Mac。
- **不签名、不公证**。不引入 Apple Developer 账号、不调用 `codesign`/`notarytool`。未签名产物靠随附 `install.sh` 清 `com.apple.quarantine` 解决。
- **版本唯一真源 = `yi-agent-rs/Cargo.toml` 的 workspace `version`**（当前 `0.1.3`）。tag 必须等于 `v<该版本>`，否则 job fail。`desktop/package.json`、`desktop/src-tauri/tauri.conf.json`、`desktop/src-tauri/Cargo.toml` 里的 `0.1.0` **不参与**，也不要求同步。
- **发布产物只在 Generic Package Registry**，绝不依赖 job artifact（artifact 有 `expire_in`）。
- **建 Release 用 REST API，不用 `release:` 关键字**。
- 提交规范：conventional commits（`ci:`、`docs:`、`feat:`），首行 ≤72 字符，**不写 `Co-Authored-By`**。
- 工作流：改动在 worktree 分支 `ci/gitlab-macos-release`（`.worktrees/gitlab-macos-release`）上进行，**禁止在 `main` 直接提交**。完成后 `git merge --no-ff` 回 `main`。
- 本仓库无 Rust 代码改动，故无需 `cargo fmt --all`。
- 文档同步：完成时更新 `docs/project-management/ci-cd.md` 的 Features（带可验证判据），并同步 `README.md` 模块索引计数（若计数变化）。

---

## 文件结构

| 文件 | 责任 | 动作 |
|---|---|---|
| `desktop/scripts/release-dmg.sh` | 发布编排：env 自愈、版本门禁、构建、打包、上传、建 Release | 新建 |
| `desktop/packaging/install.sh` | 用户侧安装器：挂载 dmg、拷进 `/Applications`、清 quarantine | 新建 |
| `.gitlab-ci.yml` | 新增 `release` stage + `release:macos-dmg` job | 修改 |
| `README.md` | 新增「内网从 GitLab 下载 Mac 版」小节 | 修改 |
| `docs/project-management/ci-cd.md` | Features 增加一条可验证条目 | 修改 |

`release-dmg.sh` 通过 `DRY_RUN=1` 分离"可本地验证"与"仅 CI 可验证"的部分：构建段本地能跑通，发布段在 dry-run 下只打印 curl 命令与 Release payload。

---

### Task 1: 脚本骨架 —— 环境自愈与版本门禁

**Files:**
- Create: `desktop/scripts/release-dmg.sh`
- Test: 以 shell 命令直接调用该脚本（无单测框架，本仓库 CI/shell 脚本约定如此）

**Interfaces:**
- Consumes: 无
- Produces:
  - 可执行脚本 `desktop/scripts/release-dmg.sh`
  - 环境变量：读 `CI_COMMIT_TAG`（或首个位置参数 `$1`）、`VERSION`（导出给后续步骤）、`DRY_RUN`（可选）
  - 失败即 `exit 1`，成功走到构建段

- [ ] **Step 1: 写脚本骨架，包含 PATH/工具自愈与版本门禁**

创建 `desktop/scripts/release-dmg.sh`：

```bash
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
```

- [ ] **Step 2: 让它可执行，并跑失败用例（tag 与 Cargo.toml 不匹配）**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/gitlab-macos-release
chmod +x desktop/scripts/release-dmg.sh
CI_COMMIT_TAG=v9.9.9 bash desktop/scripts/release-dmg.sh; echo "exit=$?"
```
Expected: 打印 `ERROR: tag (v9.9.9) 与 yi-agent-rs/Cargo.toml 版本 (v0.1.3) 不一致`，`exit=1`。

- [ ] **Step 3: 跑成功用例（匹配当前 Cargo.toml 版本）**

Run:
```bash
CI_COMMIT_TAG=v0.1.3 bash desktop/scripts/release-dmg.sh; echo "exit=$?"
```
Expected: 打印 `版本校验通过: 0.1.3 (arm64)` 与 `环境与版本检查完成`，`exit=0`。

> 注：本机 PATH 缺 node 时会在 Step 1 的工具检查处失败——那是**预期**的报错路径。若本机想跑通，先 `export PATH="/opt/homebrew/bin:$PATH"`。

- [ ] **Step 4: 提交**

```bash
git add desktop/scripts/release-dmg.sh
git commit -m "ci(desktop): add release script skeleton with env and version gate"
```

---

### Task 2: 构建与打包 dmg

**Files:**
- Modify: `desktop/scripts/release-dmg.sh`（在 Task 1 的骨架后追加构建段）

**Interfaces:**
- Consumes: Task 1 的 `$VERSION`、`$ROOT`、`$HERE`
- Produces:
  - `$ROOT/dist/yi-agent_${VERSION}_aarch64.dmg`
  - `$ROOT/dist/yi-agent_${VERSION}_aarch64.dmg.sha256`
  - 导出 `ART`（dmg 文件名，供 Task 3 复用）

- [ ] **Step 1: 追加构建段**

在 `release-dmg.sh` 末尾（`log "环境与版本检查完成"` 之后）追加：

```bash
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
```

- [ ] **Step 2: 本地端到端跑通构建（耗时数分钟，预期超时给足）**

Run（本机 arm64，已装 node/rust）：
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/gitlab-macos-release
rm -rf dist
CI_COMMIT_TAG=v0.1.3 bash desktop/scripts/release-dmg.sh; echo "exit=$?"
```
Expected: `exit=0`；随后 `ls -lh dist/` 能看到
`yi-agent_0.1.3_aarch64.dmg`（数十 MB）与 `yi-agent_0.1.3_aarch64.dmg.sha256`。

> 这是全流程最慢的一步（CLI release 构建 + Tauri 打包，可能 5-15 分钟），bash 调用时给 `expected_timeout_sec` ≥ 1200。

- [ ] **Step 3: 断言产物与校验和正确**

Run:
```bash
ls dist/
test -s dist/yi-agent_0.1.3_aarch64.dmg && echo "dmg ok"
test -s dist/yi-agent_0.1.3_aarch64.dmg.sha256 && echo "sha ok"
cd dist && shasum -a 256 -c yi-agent_0.1.3_aarch64.dmg.sha256 2>/dev/null \
  || (echo "(sha 文件是裸哈希，用下面方式核对)"; \
      [ "$(cat yi-agent_0.1.3_aarch64.dmg.sha256)" = "$(shasum -a 256 yi-agent_0.1.3_aarch64.dmg | awk '{print $1}')" ] && echo "sha matches")
```
Expected: 打印 `dmg ok` 与 `sha ok`，且哈希一致。

- [ ] **Step 4: 确认 bundle 目标改动没被误提交**

Run:
```bash
git status --porcelain desktop/src-tauri/tauri.conf.json
grep -n 'targets' desktop/src-tauri/tauri.conf.json
```
Expected: 该文件显示为已修改（` M`）——**这是预期的**，不要 `git add` 它。`targets` 已被脚本改成 `["app","dmg"]`。执行结束时用 `git checkout -- desktop/src-tauri/tauri.conf.json` 还原。

- [ ] **Step 5: 还原配置并提交脚本**

```bash
git checkout -- desktop/src-tauri/tauri.conf.json
echo "dist/" >> .gitignore   # 只加一次；若已存在则跳过
git add desktop/scripts/release-dmg.sh .gitignore
git commit -m "ci(desktop): build and archive the arm64 dmg in the release script"
```

> `dist/` 是构建产物，必须 gitignore（当前 `.gitignore` 未忽略 `dist/`，只有 `.worktrees/` 与 `target`）。用 `grep -qx 'dist/' .gitignore || echo 'dist/' >> .gitignore` 保证幂等。

---

### Task 3: 上传 Package 与创建 Release

**Files:**
- Modify: `desktop/scripts/release-dmg.sh`（追加发布段）
- Create: `desktop/packaging/install.sh`

**Interfaces:**
- Consumes: Task 2 的 `$ART`、`$ROOT/dist/`
- Produces: GitLab Release `$CI_COMMIT_TAG`，含 dmg 直链与 install.sh 附件
- 依赖 CI 变量：`CI_API_V4_URL`、`CI_PROJECT_ID`、`CI_JOB_TOKEN`（GitLab 预置）；可选 `RELEASE_TOKEN`（兜底）

- [ ] **Step 1: 写用户侧安装脚本 `desktop/packaging/install.sh`**

```bash
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
```

- [ ] **Step 2: 让安装脚本可执行**

```bash
chmod +x desktop/packaging/install.sh
bash -n desktop/packaging/install.sh && echo "syntax ok"
```
Expected: `syntax ok`（`bash -n` 只做语法检查，不会真的挂载 dmg）。

- [ ] **Step 3: 追加发布段到 `release-dmg.sh`**

```bash
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
```

- [ ] **Step 4: 用 dry-run 验证发布段（无需真 CI）**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/gitlab-macos-release
CI_API_V4_URL="https://gitlab.example.com/api/v4" \
CI_PROJECT_ID="123" \
CI_JOB_TOKEN="fake-token" \
CI_COMMIT_TAG="v0.1.3" \
CI_PROJECT_URL="https://gitlab.example.com/agenticlabs/yi-agent" \
DRY_RUN=1 \
bash desktop/scripts/release-dmg.sh 2>&1 | tail -40; echo "exit=${PIPESTATUS[0]}"
```
Expected: `exit=0`；输出包含三条 `[dry-run] curl ... --upload-file dist/yi-agent_0.1.3_aarch64.dmg ...`，以及 pretty-print 的 release payload（`tag_name`、`assets.links` 三条）。

- [ ] **Step 5: 断言 payload 结构正确**

Run:
```bash
CI_API_V4_URL=x CI_PROJECT_ID=1 CI_JOB_TOKEN=t CI_COMMIT_TAG=v0.1.3 \
CI_PROJECT_URL=x DRY_RUN=1 bash desktop/scripts/release-dmg.sh 2>&1 \
  | sed -n '/^{/,$p' | jq -e '.assets.links | length == 3 and .[0].filepath == "/yi-agent_0.1.3_aarch64.dmg"' && echo "payload ok"
```
Expected: `payload ok`。

- [ ] **Step 6: 提交**

```bash
git add desktop/scripts/release-dmg.sh desktop/packaging/install.sh
git commit -m "ci(desktop): publish dmg to the package registry and open a release"
```

---

### Task 4: 接入 GitLab CI

**Files:**
- Modify: `.gitlab-ci.yml`

**Interfaces:**
- Consumes: `desktop/scripts/release-dmg.sh`
- Produces: tag 触发的 `release:macos-dmg` job

- [ ] **Step 1: 新增 stage 与 job**

把 `.gitlab-ci.yml` 顶部的

```yaml
stages:
  - check
```

改为

```yaml
stages:
  - check
  - release
```

并在文件末尾追加：

```yaml
# tag 推送时在 Mac mini 上构建并发布 macOS(arm64) 桌面 App 的 dmg。
# 逻辑全部在 desktop/scripts/release-dmg.sh 里，job 保持薄。
release:macos-dmg:
  stage: release
  tags:
    - mac-mini
  rules:
    - if: $CI_COMMIT_TAG
  # 同一 tag 不并发发布（重跑安全，脚本内已做建/改 Release 的分支）
  resource_group: release-macos-dmg
  interruptible: false
  script:
    - bash desktop/scripts/release-dmg.sh
```

- [ ] **Step 2: 本地校验 YAML 可解析**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/gitlab-macos-release
ruby -ryaml -e 'y=YAML.load_file(".gitlab-ci.yml"); p y["stages"]; p y["release:macos-dmg"]["tags"]' \
  || python3 -c 'import yaml,sys; y=yaml.safe_load(open(".gitlab-ci.yml")); print(y["stages"]); print(y["release:macos-dmg"]["tags"])'
```
Expected: `["check", "release"]` 与 `["mac-mini"]`。

- [ ] **Step 3: 校验 job 的触发条件与既有 job 不冲突**

Run:
```bash
grep -n "CI_COMMIT_TAG\|CI_PIPELINE_SOURCE\|rules:" .gitlab-ci.yml
```
Expected: `ci-push` 仍是 `$CI_PIPELINE_SOURCE == "push"`，`ci-mr` 仍是 `merge_request_event`，新 job 是 `$CI_COMMIT_TAG`——tag 推送时二者规则不同，不会互相误触。

- [ ] **Step 4: 提交**

```bash
git add .gitlab-ci.yml
git commit -m "ci(gitlab): publish the macos dmg on tag pushes"
```

---

### Task 5: 文档与模块登记

**Files:**
- Modify: `README.md`（「安装 → 直接下载」附近）
- Modify: `docs/project-management/ci-cd.md`（Features）

**Interfaces:**
- Consumes: Task 1–4 的产出
- Produces: 用户可见的下载说明；项目进度登记

- [ ] **Step 1: README 增加内网下载小节**

在 `README.md` 的「### 直接下载」小节之后插入：

```markdown
### macOS 桌面 App（内网）

内网用户到 GitLab 的 **Deploy → Releases** 下载 `yi-agent_<版本>_aarch64.dmg`
（Apple Silicon），然后：

```bash
# 1. 安装（自动清 quarantine；未做签名/公证）
bash install.sh ./yi-agent_<版本>_aarch64.dmg

# 2. 配置模型 key —— App 内没有设置入口，必须手写文件
mkdir -p ~/.yi-agent
echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env
```

从「应用程序」打开 `yi-agent`。首次打开若提示「无法验证开发者」，到
System Settings → Privacy & Security 点 **Open Anyway**。仅 arm64（不支持 Intel Mac）。
```

- [ ] **Step 2: ci-cd.md 登记 feature（带可验证判据）**

在 `docs/project-management/ci-cd.md` 的 Features 列表中，把

```markdown
- [x] 首次端到端验证 — v0.1.0 + v0.1.1 release 流水线全流程通过
```

之后追加一行：

```markdown
- [x] GitLab 内网发布 macOS 桌面 App（arm64 dmg）— `release:macos-dmg` job（`.gitlab-ci.yml`，tag 触发）+ `desktop/scripts/release-dmg.sh`（版本门禁 / 构建 / 上传 Generic Package / 建 Release）+ `desktop/packaging/install.sh`（清 quarantine）；产物不经 GitHub，下载入口为 GitLab Deploy → Releases；验证：推 `v0.1.4` tag 后 job 全绿、Release 页 dmg 可下载并能在 arm64 Mac 上安装启动 — [设计](../../superpowers/specs/2026-10-09-gitlab-macos-release-design.md)
```

同时在「范围边界 → 做什么」补一条：

```markdown
- GitLab Release 发布 macOS 桌面 App（arm64，内网，不签名）
```

- [ ] **Step 3: 校验文档链接与格式**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/gitlab-macos-release
test -f docs/superpowers/specs/2026-10-09-gitlab-macos-release-design.md && echo "spec link ok"
grep -n "GitLab 内网发布 macOS" docs/project-management/ci-cd.md
grep -n "macOS 桌面 App（内网）" README.md
```
Expected: 三行匹配都命中。

- [ ] **Step 4: 提交**

```bash
git add README.md docs/project-management/ci-cd.md
git commit -m "docs: document the internal GitLab download path for the mac app"
```

---

## 上线验证（全部 Task 完成后，由用户执行）

- [ ] **V1. 同步版本号**：编辑 `yi-agent-rs/Cargo.toml` 的 workspace `version` 到目标版本（如 `0.1.4`），提交并合回 `main`。
- [ ] **V2. 打 tag**：`git tag v0.1.4 && git push origin v0.1.4`。
- [ ] **V3. 看流水线**：GitLab → CI/CD → Pipelines，确认 `release:macos-dmg` job 全绿。若在建 Release 处 401/403，按下方"兜底"处理。
- [ ] **V4. 验收产物**：Deploy → Releases → `v0.1.4`，确认 dmg 可下载、`install.sh` 可下载。
- [ ] **V5. 干净机安装**：在**另一台 arm64 Mac**（最好无 Rust/node）上下载并执行 `bash install.sh ./yi-agent_0.1.4_aarch64.dmg`，配好 `~/.yi-agent/.env`，打开 App，`ps aux | grep 'yi-agent app-server'` 能看到 sidecar。

**兜底（若 JOB-TOKEN 无权建 Release）**：在 GitLab 项目 → Settings → CI/CD → Variables 新建 **masked** 变量 `RELEASE_TOKEN`，值为一个有 `api` scope 的 Project Access Token（Maintainer 建）。脚本会自动切到 `PRIVATE-TOKEN` 路径，无需改代码。同时把此约束补进 `docs/project-management/ci-cd.md`。

---

## Self-Review 记录

- **Spec 覆盖**：§3 决策 1→Task 4；决策 2/3→Task 3 Step 3；决策 4→Task 3 Step 3（API，非 `release:`）；决策 5→Task 3 Step 3（`filepath`）；决策 6→Task 2/4（无 x86_64）；决策 7→Task 3 Step 1（install.sh 清 quarantine）；决策 8→Task 1 Step 1（版本门禁）；决策 9→Task 2 Step 1（jq 收窄 targets）；决策 10→Task 2 Step 1（重命名）。§4.1→Task 4；§4.2→Task 1-3；§4.3→Task 5；§4.4→Task 1/2/3 的 `exit 1` 与 Release 建/改分支。§6→Task 5。
- **占位符扫描**：无 TBD/TODO；所有代码步骤含完整可粘贴代码。
- **类型/命名一致性**：`VERSION`、`ART`、`PKG_BASE`、`AUTH_KIND`/`AUTH_VALUE`、`RELEASE_API` 在 Task 2/3 间一致；`install.sh` 路径 `desktop/packaging/install.sh` 全篇一致。
- **已知未决项**：`CI_JOB_TOKEN` 能否建 Release 依赖实例版本，已用 `RELEASE_TOKEN` 兜底并在上线验证中给出处置路径（非阻塞）。
