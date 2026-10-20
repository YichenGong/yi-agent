# macOS 桌面 App 随 npm 分发 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `npm i -g @yi-agent/yi-agent` 在 Apple Silicon Mac 上自动把桌面 App 装进 `/Applications`。

**Architecture:** 构建期用一个独立脚本把 Tauri 产出的 `.app` 修复签名后放进 `darwin-arm64` 子包；发布期靠 npm 的 `files` 白名单自然地把它打进 tarball；安装期在该子包的 `postinstall` 里判断「是否全局安装 + 是否 darwin/arm64」，满足才复制到 `/Applications`。整个安装期逻辑失败安全：任何异常都退出 0，绝不让 CLI 装不上。

**Tech Stack:** Bash（构建脚本）、Node.js CommonJS（postinstall，仓库已有 `entry.js` 用同样风格）、Tauri 2.x、npm lifecycle hooks、`node:test`（Node 20+ 内置，无需新依赖）

## Global Constraints

- 构建脚本遵循仓库既有风格：`#!/usr/bin/env bash` + `set -euo pipefail`，中文注释解释「为什么」。
- `postinstall` 必须**永不**以非 0 退出。桌面 App 装失败不能拖垮 CLI 安装。
- 只在 `process.env.npm_config_global === "true"` 且 `process.platform === "darwin"` 且 `process.arch === "arm64"` 时才动 `/Applications`。
- 签名用 ad-hoc：`codesign --force --deep --sign -`。不引入 Apple 证书。
- 不修改 GitLab 现有 dmg 流程（`desktop/scripts/release-dmg.sh`、`.gitlab-ci.yml` 的 `release:macos-dmg`）。
- 所有新脚本必须在 `just ci` 下通过（含 `shellcheck` 若有、`cargo fmt`、clippy）。
- 版本号唯一真源是 `yi-agent-rs/Cargo.toml` 的 workspace `version`。
- 提交信息用 conventional commits，**不加** `Co-Authored-By` 行。

---

### Task 1: postinstall 的纯判断逻辑

先把「该不该装」的判断抽成纯函数并用测试锁死，再接文件系统。这样安装期风险最高的部分有回归保护。

**Files:**
- Create: `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/install-app.js`
- Test: `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/test-install-app.js`

**Interfaces:**
- Produces:
  - `shouldInstall(env, platform, arch) -> { ok: boolean, reason: string }`
    - `env` 是 `process.env` 形状的对象；`platform` / `arch` 是 `process.platform` / `process.arch` 的值。
  - `needsReplace(installedVersion, bundledVersion) -> boolean`
    - 任一为 `null` / `undefined` / 空串视为「没装过」，返回 `true`。
  - `parseAppVersion(plistText) -> string | null`
  - `isWritable(dir) -> boolean`
- 后续 Task 2 依赖这三个函数的签名，不得改名。

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/test-install-app.js`：

```js
const test = require('node:test');
const assert = require('node:assert');
const { shouldInstall, needsReplace, parseAppVersion } = require('./install-app.js');

test('非全局安装时不安装', () => {
  const r = shouldInstall({}, 'darwin', 'arm64');
  assert.equal(r.ok, false);
  assert.match(r.reason, /not a global install/);
});

test('全局安装且 darwin/arm64 时安装', () => {
  const r = shouldInstall({ npm_config_global: 'true' }, 'darwin', 'arm64');
  assert.equal(r.ok, true);
});

test('全局安装但在 linux 上不安装', () => {
  const r = shouldInstall({ npm_config_global: 'true' }, 'linux', 'x64');
  assert.equal(r.ok, false);
  assert.match(r.reason, /linux/);
});

test('全局安装但 darwin/x64 不安装', () => {
  const r = shouldInstall({ npm_config_global: 'true' }, 'darwin', 'x64');
  assert.equal(r.ok, false);
  assert.match(r.reason, /x64/);
});

test('未装过时需要安装', () => {
  assert.equal(needsReplace(null, '0.1.8'), true);
  assert.equal(needsReplace('', '0.1.8'), true);
});

test('版本相同不替换', () => {
  assert.equal(needsReplace('0.1.8', '0.1.8'), false);
});

test('版本不同需要替换', () => {
  assert.equal(needsReplace('0.1.7', '0.1.8'), true);
});

test('能从 Info.plist 解析出版本号', () => {
  const plist = `<?xml version="1.0"?>
<plist version="1.0"><dict>
  <key>CFBundleShortVersionString</key>
  <string>1.2.3</string>
  <key>CFBundleVersion</key>
  <string>1.2.3</string>
</dict></plist>`;
  assert.equal(parseAppVersion(plist), '1.2.3');
});

test('Info.plist 缺版本号时返回 null', () => {
  assert.equal(parseAppVersion('<plist><dict></dict></plist>'), null);
});

test('isWritable 对不存在的目录返回 false', () => {
  assert.equal(isWritable('/nonexistent-dir-for-test-xyz'), false);
});

test('isWritable 对可写目录返回 true', () => {
  assert.equal(isWritable(require('os').tmpdir()), true);
});
```

（`isWritable` 需要和另外三个函数一起从模块导出，见 Step 3。）

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64
node --test test-install-app.js
```

预期：`Cannot find module './install-app.js'`。

- [ ] **Step 3: 写最小实现**

创建 `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/install-app.js`：

```js
#!/usr/bin/env node
// postinstall：把随包分发的 .app 复制到 /Applications。
//
// 硬性要求：任何情况下都退出 0。桌面 App 装不上不能拖垮 CLI 安装——
// 用户敲 `npm i -g` 的首要目的是拿到 yi-agent 命令。
//
// 只在 `npm i -g` 时动手，避免本包被当作依赖装进别人的 node_modules
// 时顺手污染 /Applications。
'use strict';

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const APP_NAME = 'yi-agent.app';
const TARGET_DIR = '/Applications';

/// 判断是否该在本机安装桌面 App。
/// 返回 { ok, reason }；ok 为 false 时 reason 说明原因（便于日志排查）。
function shouldInstall(env, platform, arch) {
  if (env.npm_config_global !== 'true') {
    return { ok: false, reason: 'not a global install' };
  }
  if (platform !== 'darwin') {
    return { ok: false, reason: `platform ${platform} is not darwin` };
  }
  if (arch !== 'arm64') {
    return { ok: false, reason: `arch ${arch} is not arm64` };
  }
  return { ok: true, reason: 'ok' };
}

/// 已有版本与待装版本不同才需要替换。没装过（任一侧为空）视为需要装。
function needsReplace(installedVersion, bundledVersion) {
  if (!installedVersion) return true;
  if (!bundledVersion) return true;
  return installedVersion !== bundledVersion;
}

/// 从 Info.plist 文本里取 CFBundleShortVersionString。
function parseAppVersion(plistText) {
  const m = plistText.match(
    /<key>CFBundleShortVersionString<\/key>\s*<string>([^<]+)<\/string>/
  );
  return m ? m[1].trim() : null;
}

function readAppVersion(appPath) {
  try {
    const plist = fs.readFileSync(
      path.join(appPath, 'Contents', 'Info.plist'),
      'utf8'
    );
    return parseAppVersion(plist);
  } catch (e) {
    return null;
  }
}

/// 复制 .app 并清 quarantine。先复制到临时目录再原子替换，
/// 避免中途失败留下半个 App（沿用 desktop/packaging/install.sh 的做法）。
function installApp(sourceApp, targetApp) {
  const staging = `${targetApp}.staging-${process.pid}`;
  fs.rmSync(staging, { recursive: true, force: true });
  fs.cpSync(sourceApp, staging, { recursive: true, dereference: true });

  // ad-hoc 重签。Tauri 产出的签名是坏的（codesign --verify 报
  // "code has no resources but signature indicates they must be present"），
  // 复制后重签可修复。失败不致命：签名坏了多半仍能启动。
  try {
    execFileSync('codesign', ['--force', '--deep', '--sign', '-', staging], {
      stdio: 'ignore',
    });
  } catch (e) {
    console.warn(`[yi-agent] codesign failed (non-fatal): ${e.message}`);
  }

  fs.rmSync(targetApp, { recursive: true, force: true });
  fs.renameSync(staging, targetApp);

  // 从 npm 装的包不带 quarantine，但用户若曾手工拷过同路径可能残留标记。
  try {
    execFileSync('xattr', ['-dr', 'com.apple.quarantine', targetApp], {
      stdio: 'ignore',
    });
  } catch (e) {
    // 没有该属性时 xattr 返回非 0，忽略。
  }
}

function main() {
  try {
    const verdict = shouldInstall(process.env, process.platform, process.arch);
    if (!verdict.ok) {
      // 静默退出：本地开发安装不该有任何输出噪音。
      return;
    }

    const sourceApp = path.join(__dirname, APP_NAME);
    if (!fs.existsSync(sourceApp)) {
      console.log('[yi-agent] desktop app not found in package, skipping');
      return;
    }

    const targetApp = path.join(TARGET_DIR, APP_NAME);
    const bundledVersion = readAppVersion(sourceApp);
    const installedVersion = readAppVersion(targetApp);

    if (!needsReplace(installedVersion, bundledVersion)) {
      console.log(
        `[yi-agent] desktop app ${bundledVersion} already in ${TARGET_DIR}`
      );
      return;
    }

    if (!fs.existsSync(TARGET_DIR) || !isWritable(TARGET_DIR)) {
      console.log(
        `[yi-agent] cannot write to ${TARGET_DIR} (needs admin). ` +
          `CLI is installed; to add the desktop app run:\n` +
          `  sudo cp -R "${sourceApp}" "${TARGET_DIR}/"`
      );
      return;
    }

    installApp(sourceApp, targetApp);
    console.log(`[yi-agent] desktop app ${bundledVersion} installed to ${targetApp}`);
  } catch (e) {
    // 兜底：绝不因装饰性功能让 install 失败。
    console.warn(`[yi-agent] desktop app install skipped: ${e.message}`);
  }
}

function isWritable(dir) {
  try {
    fs.accessSync(dir, fs.constants.W_OK);
    return true;
  } catch (e) {
    return false;
  }
}

module.exports = { shouldInstall, needsReplace, parseAppVersion, isWritable };

if (require.main === module) {
  main();
  process.exit(0);
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64
node --test test-install-app.js
```

预期：11 个测试全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/install-app.js \
        yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/test-install-app.js
git commit -m "feat(npm): add desktop app postinstall with pure decision logic"
```

---

### Task 2: 接上 postinstall 与 files 白名单

**Files:**
- Modify: `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/package.json`

**Interfaces:**
- Consumes: Task 1 的 `install-app.js`（作为 `postinstall` 的入口）。
- Produces: 该子包在 `npm pack` 时包含 `yi-agent.app/`，安装时触发 `postinstall`。

- [ ] **Step 1: 改 package.json**

把 `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/package.json` 改成：

```json
{
  "name": "@yi-agent/yi-agent-darwin-arm64",
  "version": "0.1.0",
  "description": "yi-agent binary for darwin-arm64",
  "license": "MIT",
  "os": ["darwin"],
  "cpu": ["arm64"],
  "scripts": {
    "postinstall": "node install-app.js",
    "test": "node --test test-install-app.js"
  },
  "files": ["binaries/", "yi-agent.app/", "install-app.js"]
}
```

注意：
- `files` 必须显式列出 `install-app.js`，否则 npm 默认虽然会带上它，但显式声明更稳（`files` 是一份白名单，未来新增文件不会被意外打包或意外遗漏）。
- `test-install-app.js` **不要**放进 `files`，测试不该进发布包。

- [ ] **Step 2: 验证 files 白名单真的生效（用假 .app 占位）**

```bash
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64
mkdir -p yi-agent.app/Contents/MacOS binaries
printf '#!/bin/sh\necho stub\n' > binaries/yi-agent && chmod +x binaries/yi-agent
mkdir -p yi-agent.app/Contents && cat > yi-agent.app/Contents/Info.plist <<'EOF'
<?xml version="1.0"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>0.0.1-probe</string>
</dict></plist>
EOF
npm pack --dry-run 2>&1 | grep -E "yi-agent.app|install-app.js|test-install-app.js|binaries/"
```

预期：输出里能看到 `yi-agent.app/`、`install-app.js`、`binaries/`，**看不到** `test-install-app.js`。

- [ ] **Step 3: 清理占位产物**

```bash
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64
rm -rf yi-agent.app binaries
git status --porcelain
```

预期：只剩 `package.json` 的修改（占位文件都在 `.gitignore` 覆盖范围内，不出现）。

- [ ] **Step 4: 提交**

```bash
git add yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/package.json
git commit -m "feat(npm): ship desktop app in darwin-arm64 package and run postinstall"
```

---

### Task 3: 构建脚本 package-npm-app.sh

把 Tauri 构建 → 修签名 → 就位这套动作封装成一个可独立运行的脚本，供 CI 和本地调用。

**Files:**
- Create: `desktop/scripts/package-npm-app.sh`

**Interfaces:**
- Consumes: `desktop/src-tauri/tauri.conf.json`、`desktop/package.json`、`yi-agent-rs/Cargo.toml`。
- Produces: `yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/yi-agent.app`（已签名）。
- 入参：无。版本从 `yi-agent-rs/Cargo.toml` 读；若设置了 `CI_COMMIT_TAG` 则校验 tag 与版本一致。

- [ ] **Step 1: 写脚本**

创建 `desktop/scripts/package-npm-app.sh`：

```bash
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
```

- [ ] **Step 2: 赋予可执行权限并做 shellcheck 静态检查**

```bash
chmod +x desktop/scripts/package-npm-app.sh
bash -n desktop/scripts/package-npm-app.sh && echo "syntax OK"
command -v shellcheck >/dev/null 2>&1 && shellcheck desktop/scripts/package-npm-app.sh || echo "shellcheck 未安装，跳过"
```

预期：`syntax OK`；若装了 shellcheck 则无 error 级问题。

- [ ] **Step 3: 验证前置检查会正确失败**

```bash
# 在非 mac 环境无法测，改为验证版本门禁：
CI_COMMIT_TAG=v99.99.99 bash desktop/scripts/package-npm-app.sh 2>&1 | head -3
```

预期：`ERROR: tag (v99.99.99) 与 Cargo.toml (0.1.8) 不一致`，且**未**开始构建。

- [ ] **Step 4: 提交**

```bash
git add desktop/scripts/package-npm-app.sh
git commit -m "feat(desktop): add script to package .app for npm"
```

---

### Task 4: 把构建接入 justfile

**Files:**
- Modify: `yi-agent-rs/justfile:209`（`npm-pack` 之前）

**Interfaces:**
- Consumes: Task 3 的 `desktop/scripts/package-npm-app.sh`。
- Produces: `just npm-pack-desktop` 供 CI 调用；`npm-pack` 不自动调用它（保持本地打包快速，见下方说明）。

- [ ] **Step 1: 修改 justfile**

在 `yi-agent-rs/justfile` 的 `# 组装 npm 包` 注释块之前插入：

```
# 构建 macOS 桌面 App 并放进 npm 的 darwin-arm64 子包。
# 单独成 task 而不是塞进 npm-pack：Tauri 构建要十分钟，本地只发
# CLI 时不该被迫等它；CI 在 publish 前显式调用。
npm-pack-desktop:
    #!/usr/bin/env bash
    set -e
    bash ../desktop/scripts/package-npm-app.sh
```

- [ ] **Step 2: 验证 task 被识别**

```bash
cd yi-agent-rs && just --list 2>&1 | grep -E "npm-pack-desktop|npm-pack$"
```

预期：两行都出现。

- [ ] **Step 3: 验证在非 macOS 上会明确失败（若当前在 mac 上则跳过）**

```bash
cd yi-agent-rs && uname -m
```

在 arm64 mac 上此步无需执行额外命令；若在 Linux/CI 需确认脚本第 1 段的 `uname` 检查给出可读错误。

- [ ] **Step 4: 提交**

```bash
git add yi-agent-rs/justfile
git commit -m "feat(just): add npm-pack-desktop task"
```

---

### Task 5: CI 接入

**Files:**
- Modify: `.github/workflows/release.yml`（`Build & package all platforms` 之后、`Publish to npm` 之前）

**Interfaces:**
- Consumes: Task 4 的 `just npm-pack-desktop`。
- Produces: 发布到 npm 的 tarball 内含已签名的 `yi-agent.app`。

- [ ] **Step 1: 插入新 step**

在 `.github/workflows/release.yml` 里定位 `- name: Upload artifacts to GitHub Release` 之前，插入：

```yaml
      - name: Build macOS desktop app for npm
        # 只有 arm64 的 .app 会进 darwin-arm64 子包。Linux/Intel 用户
        # 不会因此多下载——npm 按 os/cpu 只装匹配的子包。
        run: |
          source "$HOME/.cargo/env"
          just npm-pack-desktop

      - name: Verify the desktop app is in the npm package
        # 构建「成功」但 .app 没进包的情况最难查（用户装完发现没有 App）。
        # 这里在发布前直接验白名单产物。
        run: |
          cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64
          test -d yi-agent.app || { echo "ERROR: yi-agent.app missing"; exit 1; }
          codesign --verify --deep --strict yi-agent.app
          V=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
              yi-agent.app/Contents/Info.plist)
          echo "desktop app version: $V"
          grep -q "^version = \"$V\"" ../../Cargo.toml || {
            echo "ERROR: desktop app version $V != workspace version"; exit 1; }
```

- [ ] **Step 2: 本地验证 YAML 语法**

```bash
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('YAML OK')"
```

预期：`YAML OK`。

- [ ] **Step 3: 跑仓库已有的 workflow 结构性检查**

```bash
cd yi-agent-rs && just release-env-check
```

预期：12 passed（该检查锁住 PATH 顺序、fd 上限等既有约束，新增 step 不该破坏它们）。

- [ ] **Step 4: 提交**

```bash
git add .github/workflows/release.yml
git commit -m "ci(release): build macOS desktop app before npm publish"
```

---

### Task 6: 文档与项目进度

**Files:**
- Modify: `README.md`（桌面 App 小节）
- Modify: `docs/project-management/ci-cd.md`（模块登记）

**Interfaces:**
- Consumes: 前五个 task 的最终行为。
- Produces: 用户可查的安装说明；项目进度表可核对。

- [ ] **Step 1: 更新 README 的桌面 App 小节**

把 `README.md` 现有的「### macOS 桌面 App（内网）」小节整段替换为：

```markdown
### macOS 桌面 App（Apple Silicon）

已装 Node 的话，一条命令同时装好 CLI 和桌面 App：

```bash
npm install -g @yi-agent/yi-agent
```

`postinstall` 会把 `yi-agent.app` 复制到 `/Applications` 并清掉 quarantine。
只在 `npm i -g`（全局安装）时触发，且仅限 Apple Silicon。

若 `/Applications` 不可写（企业机器常见），CLI 仍会装好，并提示你手动执行
`sudo cp` 完成桌面 App 的安装。

内网用户仍可走 GitLab **Deploy → Releases** 下载 dmg（含 `install.sh`）。

首次启动前需要配置模型 key —— App 内没有设置入口，必须手写文件：

```bash
mkdir -p ~/.yi-agent
echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env
```
```

- [ ] **Step 2: 更新 ci-cd.md**

在 `docs/project-management/ci-cd.md` 的条目列表里追加一条（格式与既有条目一致，带可验证判据）：

```markdown
- [x] macOS 桌面 App 随 npm 分发 — `desktop/scripts/package-npm-app.sh` 用 Tauri 产出 `.app`、`codesign --force --deep --sign -` 修复 Tauri 的坏签名（`codesign --verify` 原本报 "code has no resources but signature indicates they must be present"）、再放进 `yi-agent-cli/platforms/darwin-arm64/`；该子包 `postinstall` 执行 `install-app.js`，仅在 `npm_config_global === "true"` 且 darwin/arm64 时复制到 `/Applications` 并按 `CFBundleShortVersionString` 比对版本，任何异常均退出 0 不拖垮 CLI 安装。判据：`cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64 && node --test test-install-app.js`（11 passed）；`just npm-pack-desktop` 产出 `yi-agent.app` 且 `codesign --verify --deep --strict` 通过；真机 `npm i -g` 后 `/Applications/yi-agent.app` 存在且版本与 `Cargo.toml` 一致。
```

- [ ] **Step 3: 核对 README 模块索引计数**

```bash
grep -nE "ci-cd" docs/project-management/README.md | head -3
```

若该表有「完成 / 总计」计数，按新增一条同步更新数字。

- [ ] **Step 4: 提交**

```bash
git add README.md docs/project-management/ci-cd.md docs/project-management/README.md
git commit -m "docs: document desktop app installation via npm"
```

---

### Task 7: 端到端验证

不写自动化用例（真实 `npm i -g` 会改 `/Applications`，不适合进 CI），改为一次性手工验收，并把结果记进 PR 描述。

**Files:** 无（仅验证）

**Interfaces:**
- Consumes: 全部前序 task。
- Produces: 可引用的验收证据。

- [ ] **Step 1: 完整跑一遍静态检查与单测**

```bash
ps aux | grep -v grep | grep -cE "cargo|rustc"   # 确认无残留编译进程
cd yi-agent-rs && just ci
```

预期：`CI passed`，exit 0。

- [ ] **Step 2: 构建真实 .app 并检查签名与版本**

```bash
cd yi-agent-rs && just npm-pack-desktop
A=yi-agent-cli/platforms/darwin-arm64/yi-agent.app
codesign --verify --deep --strict "$A" && echo "signature OK"
/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$A/Contents/Info.plist"
```

预期：`signature OK`，版本等于 `Cargo.toml` 的版本。

- [ ] **Step 3: 打包并检查 tarball 内含 .app**

```bash
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64 && npm pack --dry-run 2>&1 | tail -20
```

预期：`package size` 约 33 MB；文件列表含 `yi-agent.app/` 与 `install-app.js`，不含 `test-install-app.js`。

- [ ] **Step 4: 真机验证 postinstall**

```bash
# 先移走已有的，确保测的是新装
sudo rm -rf /Applications/yi-agent.app
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64 && npm pack
npm i -g ./yi-agent-cli-darwin-arm64-*.tgz
ls -d /Applications/yi-agent.app && \
  /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
  /Applications/yi-agent.app/Contents/Info.plist
```

预期：出现 `[yi-agent] desktop app X.Y.Z installed to /Applications/yi-agent.app`；目录存在且版本正确。

- [ ] **Step 5: 验证幂等与「非全局不触发」**

```bash
# 再装一次，应跳过而非重装
npm i -g ./yi-agent-rs/yi-agent-cli/*.tgz 2>&1 | grep -i "already in"
# 本地安装（非 -g）不应动 /Applications
cd /tmp && rm -rf probe && mkdir probe && cd probe && npm init -y >/dev/null && \
  npm i ./yi-agent-rs/yi-agent-cli/platforms/darwin-arm64/*.tgz 2>&1 | grep -c "installed to" || echo "0（正确：本地安装不触发）"
```

预期：第一条出现 `already in /Applications`；第二条输出 `0`。

- [ ] **Step 6: 清理测试残留**

```bash
sudo rm -f /tmp/probe/yi-agent-cli-platforms-darwin-arm64-*.tgz 2>/dev/null
rm -rf /tmp/probe
cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64 && rm -f *.tgz
git status --porcelain
```

预期：工作区干净（`yi-agent.app/` 已被 `.gitignore` 覆盖）。

---

## 附：风险与回退

- **体积**：`darwin-arm64` 子包从 15MB 涨到约 33MB，仅 Apple Silicon 用户承担。
- **签名**：ad-hoc 签名。npm 安装路径不带 quarantine，实测可启动；从浏览器下载同一 App 仍需 `install.sh` 清标记。这与现状一致。
- **回退**：删掉 `install-app.js` 与 `package.json` 的 `scripts.postinstall`、`files` 里的 `yi-agent.app/`，即可退回到「只有 CLI」。CI 里去掉两个 step 即可。GitLab dmg 流程全程未动，不受影响。
