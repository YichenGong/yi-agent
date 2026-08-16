# 自动化构建与测试流水线设计

## 元信息

- **日期**: 2026-07-19
- **状态**: 已确认
- **主仓库**: GitLab（代码主仓库）
- **GitHub 角色**: 镜像 + Release 触发点
- **运行环境**: Mac mini self-hosted runner（双 agent 共存，无安全隔离）

## 决策汇总

| 项 | 决策 |
|---|---|
| 范围 | 本地 + CI + Release 完整流水线 |
| 代码主仓库 | GitLab |
| GitHub 角色 | 镜像 + Release 触发点 |
| 仓库镜像 | GitLab 自动 push mirror 到 GitHub |
| 发布渠道 | GitHub Releases + Homebrew（独立 tap 仓库）+ npm（预编译二进制） |
| 目标平台 | macOS x64 / macOS ARM64 / Linux x64 |
| 触发策略 | push/PR 跑 CI，tag `v*` 触发 release |
| Rust 工具链 | stable 最新 |
| 覆盖率 | 不跑 |
| 安全审计 | 不跑 |
| 本地任务运行器 | just |
| Homebrew tap | 独立仓库 `homebrew-yi-agent` |
| 版本号 | 手动打 tag |
| 本地命令与 CI 命令 | 共享 justfile，CI 调用 just |
| CI runner | Mac mini self-hosted（GitLab + GitHub Actions 双 agent） |
| PR CI | 云端 runner（不进 Mac mini） |
| Release CI | Mac mini self-hosted runner |
| 安全隔离 | 不隔离 |
| npm 分发策略 | 主包 + 平台子包（optionalDependencies） |

## 整体架构

```
                    ┌──────────────────────┐
                    │  GitLab 主仓库        │
                    │  (代码 + CI 配置)     │
                    └──────────┬───────────┘
                               │ push mirror
                               ▼
                    ┌──────────────────────┐
                    │  GitHub 镜像仓库      │
                    │  (tag 触发 release)  │
                    └──────────┬───────────┘
                               │
        ┌──────────────────────┼──────────────────────┐
        │                      │                      │
        ▼                      ▼                      ▼
┌──────────────┐      ┌──────────────┐       ┌──────────────┐
│ GitLab CI    │      │ GitHub       │       │ justfile     │
│ (push: Mac   │      │ Actions      │       │ (唯一真源)   │
│  mini;       │      │ (tag →       │       │ build/test/  │
│  PR: 云端)   │      │  Mac mini)   │       │ lint/package │
└──────────────┘      └──────────────┘       └──────────────┘
                               │
                               │ tag v*.*.*
                               ▼
                    ┌──────────────────────┐
                    │  Release 流水线        │
                    │  (在 Mac mini 上跑)    │
                    │                       │
                    │  1. cargo-zigbuild   │
                    │     交叉编译三平台     │
                    │  2. 打 tarball+SHA256 │
                    │  3. 发布 GH Release   │
                    │  4. 更新 homebrew tap │
                    │  5. 发布到 npm        │
                    └──────────────────────┘
```

**关键设计原则：**

1. **justfile 是唯一真源** - 所有构建/测试/打包逻辑都放在 justfile，CI 只负责"什么时候跑、runner 是什么、产物上传到哪"
2. **双 CI 共用同一 justfile** - GitLab CI 和 GitHub Actions 都调用同一份 justfile，逻辑零重复
3. **GitLab 主、GitHub 镜像** - 日常开发在 GitLab，GitHub 自动同步，tag 推送后两边都触发 CI，但 release 只在 GitHub Actions 触发
4. **Mac mini 双 agent 共存** - GitLab Runner 和 GitHub Actions Runner 同时注册，共享 cargo registry 缓存
5. **三渠道发布顺序固定** - GitHub Release 先出（提供二进制下载 URL）→ Homebrew tap 引用 GitHub Release URL → npm 包内嵌二进制

## justfile 任务设计

**设计目标：** 所有构建/测试/打包逻辑集中在一个 justfile，本地和 CI 都通过 `just <task>` 调用。

**任务分层：**
- 底层原子任务（`build-target`、`package-target`）- 便于调试和复用
- 组合任务（`build-all-targets`、`release`）- CI 入口

**关键技术选型：**
- **cargo-zigbuild** 用于 Linux 交叉编译 - 用 zig 作为 linker，不需要 Docker，Mac mini 原生跑
- **版本号从 Cargo.toml 读** - `workspace.package.version` 是版本号唯一来源
- **产物目录 `dist/`** - 所有 release 产物统一存放

**完整 justfile（位于 `yi-agent-rs/`）：**

```just
# justfile (位于 yi-agent-rs/)

# 默认列出所有任务
default:
    @just --list

# === 日常开发 ===

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-features --workspace

build:
    cargo build --workspace

build-release:
    cargo build --workspace --release

# === CI 入口 ===

ci: fmt-check lint test build
    @echo "CI passed"

# === Release ===

# 三平台目标列表
targets := "x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu"

# 版本号（从 Cargo.toml workspace 读）
version := `grep '^version' Cargo.toml | head -1 | awk -F'"' '{print $$2}'`

# npm 包目录
npm-dir := "yi-agent-cli"

# 平台子包目录名列表
platform-dirs := "darwin-x64 darwin-arm64 linux-x64"

# 交叉编译单个目标
build-target target:
    @if echo "{{target}}" | grep -q "linux"; then \
        cargo zigbuild --release --target {{target}}; \
    else \
        cargo build --release --target {{target}}; \
    fi

# 交叉编译三平台
build-all-targets:
    @for target in {{targets}}; do \
        echo "Building $$target..."; \
        just build-target $$target; \
    done

# 打包单个目标为 tarball + SHA256
package-target target:
    @mkdir -p dist
    @tar -czf dist/yi-agent-{{target}}.tar.gz \
        -C target/{{target}}/release yi-agent
    @shasum -a 256 dist/yi-agent-{{target}}.tar.gz \
        | awk '{print $$1}' > dist/yi-agent-{{target}}.tar.gz.sha256

# 打包三平台
package-all: build-all-targets
    @for target in {{targets}}; do \
        just package-target $$target; \
    done

# 组装 npm 包（填充子包二进制 + 同步版本号）
npm-pack: package-all
    @for dir in {{platform-dirs}}; do \
        target=$(case $$dir in \
            darwin-x64) echo "x86_64-apple-darwin";; \
            darwin-arm64) echo "aarch64-apple-darwin";; \
            linux-x64) echo "x86_64-unknown-linux-gnu";; \
        esac); \
        mkdir -p {{npm-dir}}/platforms/$$dir/binaries; \
        cp target/$$target/release/yi-agent \
            {{npm-dir}}/platforms/$$dir/binaries/yi-agent; \
    done
    @# 同步版本号
    @sed -i.bak 's/"version": ".*"/"version": "{{version}}"/' \
        {{npm-dir}}/package.json && rm {{npm-dir}}/package.json.bak
    @for dir in {{platform-dirs}}; do \
        sed -i.bak 's/"version": ".*"/"version": "{{version}}"/' \
            {{npm-dir}}/platforms/$$dir/package.json \
            && rm {{npm-dir}}/platforms/$$dir/package.json.bak; \
    done

# 发布到 npm（CI 调用，子包先发）
npm-publish: npm-pack
    @for dir in {{platform-dirs}}; do \
        echo "Publishing $$dir..."; \
        cd {{npm-dir}}/platforms/$$dir && npm publish --access public && cd -; \
    done
    cd {{npm-dir}} && npm publish --access public

# Release 流水线入口（CI 调用）
release: clean package-all npm-pack
    @echo "Release artifacts ready in dist/"

clean:
    cargo clean
    rm -rf dist
```

## CI 工作流配置

**设计目标：** GitLab CI 管日常 push（Mac mini），GitHub Actions 管 PR（云端）和 release（Mac mini）。两边都调用同一 justfile，零逻辑重复。

### GitLab CI (`.gitlab-ci.yml`)

```yaml
stages:
  - check

variables:
  CARGO_TERM_COLOR: "always"

# 缓存 cargo registry 和 target 目录
cache:
  key: ${CI_COMMIT_REF_SLUG}
  paths:
    - yi-agent-rs/target/
    - ~/.cargo/registry/
    - ~/.cargo/git/

# push 到分支时跑（Mac mini self-hosted）
ci-push:
  stage: check
  tags:
    - mac-mini
  rules:
    - if: $CI_PIPELINE_SOURCE == "push"
  script:
    - cd yi-agent-rs
    - rustup default stable
    - rustup update stable
    - cargo install just --locked --quiet || true
    - just ci

# MR（Merge Request）走云端 runner
ci-mr:
  stage: check
  rules:
    - if: $CI_PIPELINE_SOURCE == "merge_request_event"
  image: rust:latest
  script:
    - cd yi-agent-rs
    - cargo install just --locked --quiet
    - just ci
```

### GitHub Actions CI (`.github/workflows/ci.yml`)

PR 触发，跑在云端 ubuntu runner：

```yaml
name: CI

on:
  pull_request:
    branches: [main]

jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          workspaces: yi-agent-rs

      - name: Install just
        uses: extractions/setup-just@v2

      - name: Run CI
        working-directory: yi-agent-rs
        run: just ci
```

### GitHub Actions Release (`.github/workflows/release.yml`)

Tag 触发，跑在 Mac mini self-hosted runner：

```yaml
name: Release

on:
  push:
    tags:
      - "v*.*.*"

jobs:
  release:
    runs-on: self-hosted  # Mac mini
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-apple-darwin,aarch64-apple-darwin,x86_64-unknown-linux-gnu

      - name: Install cargo-zigbuild
        run: cargo install cargo-zigbuild --locked

      - name: Install zig
        run: brew install zig

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          workspaces: yi-agent-rs

      - name: Install just
        uses: extractions/setup-just@v2

      - name: Build & package all platforms
        working-directory: yi-agent-rs
        run: just package-all

      - name: Upload artifacts to GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          files: |
            yi-agent-rs/dist/*.tar.gz
            yi-agent-rs/dist/*.sha256
          generate_release_notes: true

      - name: Setup Node.js
        uses: actions/setup-node@v4
        with:
          node-version: "20"
          registry-url: "https://registry.npmjs.org"

      - name: Publish to npm
        working-directory: yi-agent-rs
        run: just npm-publish
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}

      - name: Update Homebrew tap
        env:
          TAP_REPO_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}
        run: |
          VERSION=$(grep '^version' yi-agent-rs/Cargo.toml | head -1 | awk -F'"' '{print $2}')
          ./.github/scripts/update-homebrew-tap.sh "${{ github.ref_name }}" "$VERSION"
```

**关键设计点：**

1. **CI 任务三处复用 justfile** - GitLab push、GitLab MR、GitHub PR、GitHub Release 都调用同一个 `just ci` 或 `just package-all`
2. **runner 选择策略** - PR 走云端 runner（安全，不进 Mac mini），push 和 release 走 Mac mini self-hosted
3. **release 步骤顺序** - build → upload GH Release → npm publish → update Homebrew tap。GitHub Release 必须先出，因为 Homebrew 公式要引用 Release 资产 URL
4. **cargo-zigbuild + zig** - Linux 交叉编译通过 zig 作为 linker，不需要 Docker
5. **`generate_release_notes: true`** - GitHub 自动从上次 tag 到本次 tag 之间的 commit 生成 release notes

## Homebrew Tap 自动化

**设计目标：** tag 触发 release 后，自动更新独立的 `homebrew-yi-agent` 仓库里的 Homebrew 公式，用户用 `brew tap gongyichen/yi-agent && brew install yi-agent` 即可安装。

### Homebrew tap 仓库结构

独立仓库 `github.com/gongyichen/homebrew-yi-agent`：

```
homebrew-yi-agent/
└── Formula/
    └── yi-agent.rb    ← Homebrew 公式（唯一文件）
```

### 公式模板（`Formula/yi-agent.rb`）

Homebrew 用 `on_macos`/`on_intel`/`on_arm`/`on_linux` 条件块自动选择平台，用户无需手动指定：

```ruby
class YiAgent < Formula
  desc "A coding agent CLI"
  homepage "https://github.com/gongyichen/yi-agent"
  version "0.1.0"

  on_macos do
    on_intel do
      url "https://github.com/gongyichen/yi-agent/releases/download/v0.1.0/yi-agent-x86_64-apple-darwin.tar.gz"
      sha256 "INTEL_MAC_SHA256"
    end
    on_arm do
      url "https://github.com/gongyichen/yi-agent/releases/download/v0.1.0/yi-agent-aarch64-apple-darwin.tar.gz"
      sha256 "ARM_MAC_SHA256"
    end
  end

  on_linux do
    url "https://github.com/gongyichen/yi-agent/releases/download/v0.1.0/yi-agent-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "LINUX_SHA256"
  end

  def install
    bin.install "yi-agent"
  end

  test do
    assert_match "yi-agent", shell_output("#{bin}/yi-agent --version")
  end
end
```

### 自动化更新脚本（`.github/scripts/update-homebrew-tap.sh`）

在 release 流水线里执行，用 GitHub Contents API 更新 tap 仓库的公式文件：

```bash
#!/usr/bin/env bash
set -euo pipefail

VERSION="${1#v}"  # 去掉 v 前缀，v0.1.0 → 0.1.0
TAG="$1"
TAP_REPO="gongyichen/homebrew-yi-agent"
REPO="gongyichen/yi-agent"

# 从刚上传的 GitHub Release 下载 SHA256 文件
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

# 读取各平台 SHA256
INTEL_MAC_SHA=$(curl -sL "$BASE_URL/yi-agent-x86_64-apple-darwin.tar.gz.sha256")
ARM_MAC_SHA=$(curl -sL "$BASE_URL/yi-agent-aarch64-apple-darwin.tar.gz.sha256")
LINUX_SHA=$(curl -sL "$BASE_URL/yi-agent-x86_64-unknown-linux-gnu.tar.gz.sha256")

# 生成新的公式文件
cat > /tmp/yi-agent.rb <<EOF
class YiAgent < Formula
  desc "A coding agent CLI"
  homepage "https://github.com/$REPO"
  version "$VERSION"

  on_macos do
    on_intel do
      url "$BASE_URL/yi-agent-x86_64-apple-darwin.tar.gz"
      sha256 "$INTEL_MAC_SHA"
    end
    on_arm do
      url "$BASE_URL/yi-agent-aarch64-apple-darwin.tar.gz"
      sha256 "$ARM_MAC_SHA"
    end
  end

  on_linux do
    url "$BASE_URL/yi-agent-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "$LINUX_SHA"
  end

  def install
    bin.install "yi-agent"
  end

  test do
    assert_match "yi-agent", shell_output("#{bin}/yi-agent --version")
  end
end
EOF

# 用 GitHub Contents API PUT 更新文件
CONTENT=$(base64 < /tmp/yi-agent.rb | tr -d '\n')

# 获取当前文件 SHA（更新需要）
FILE_SHA=$(curl -sL \
  -H "Authorization: token $TAP_REPO_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$TAP_REPO/contents/Formula/yi-agent.rb" \
  | grep '"sha"' | head -1 | awk -F'"' '{print $4}')

# PUT 更新文件
curl -sL \
  -X PUT \
  -H "Authorization: token $TAP_REPO_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/$TAP_REPO/contents/Formula/yi-agent.rb" \
  -d "{
    \"message\": \"Update yi-agent to $VERSION\",
    \"content\": \"$CONTENT\",
    \"sha\": \"$FILE_SHA\"
  }"

echo "Homebrew tap updated to $VERSION"
```

**用户安装命令：**

```bash
brew tap gongyichen/yi-agent
brew install yi-agent
```

**前提：** `yi-agent` 二进制要支持 `--version` flag（实现时记得加，Homebrew test 块会调用）。

## Mac mini Runner 配置

**设计目标：** Mac mini 同时作为 GitLab Runner 和 GitHub Actions Runner，共享 cargo registry 缓存，无需安全隔离。

### Runner 架构

```
Mac mini (macOS)
├── GitLab Runner agent       ← 通过 brew services 后台运行
│   └── 注册 tag: mac-mini
│       └── 执行 .gitlab-ci.yml 里 tag: mac-mini 的 job
│
├── GitHub Actions Runner     ← 通过 launchd 后台运行
│   └── 注册为 self-hosted runner
│       └── 执行 .github/workflows/release.yml 里 runs-on: self-hosted 的 job
│
├── 共享缓存目录
│   ├── ~/.cargo/registry/   ← 两边 CI 共用（只读并发安全）
│   └── CARGO_TARGET_DIR     ← 两边 CI 独立（避免并发写冲突）
│
└── 工具链（全局共享）
    ├── Rust (rustup)
    ├── just
    ├── cargo-zigbuild + zig
    └── Node.js (为 npm publish)
```

### 一次性配置步骤

**1. 基础工具链：**

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source ~/.cargo/env

# just
cargo install just --locked

# cargo-zigbuild（交叉编译 Linux）
cargo install cargo-zigbuild --locked

# zig（cargo-zigbuild 依赖）
brew install zig

# Node.js（为 npm publish）
brew install node@20
```

**2. GitLab Runner 注册：**

```bash
# 安装 GitLab Runner
brew install gitlab-runner

# 注册（在 gitlab.com 对应仓库 → Settings → CI/CD → Runners → New project runner）
gitlab-runner register
#   URL: https://gitlab.com/  （或对应的 GitLab 实例 URL）
#   Token: 从 GitLab UI 复制
#   Executor: shell
#   Tags: mac-mini

# 启动为后台服务
brew services start gitlab-runner
```

**3. GitHub Actions Runner 注册：**

```bash
# 在 GitHub 仓库 → Settings → Actions → Runners → New self-hosted runner
# 按官方指引下载 runner agent 到 ~/actions-runner/

cd ~/actions-runner
./config.sh \
  --url https://github.com/gongyichen/yi-agent \
  --token <TOKEN>

# 安装为 launchd 服务（开机自启）
./svc.sh install
./svc.sh start
```

### 并发写问题处理

如果 GitLab CI 和 GitHub Actions 同时跑 job，两边都往 `target/` 目录写编译产物会冲突。解决方案：**使用独立 target 目录**。

```bash
# GitLab CI 里
export CARGO_TARGET_DIR="$HOME/cargo-target/gitlab"

# GitHub Actions 里
export CARGO_TARGET_DIR="$HOME/cargo-target/github"
```

- cargo registry（`~/.cargo/registry/`）共享 - 只读并发安全
- target 目录隔离 - 避免并发写冲突
- 代价：缓存不共享，磁盘占用翻倍。但 Rust 编译缓存本来就不值得跨 CI 系统共享（编译器版本、feature flag 差异会让缓存失效）

### Mac mini 配置要点

| 项 | 配置 |
|---|---|
| GitLab Runner | shell executor, tag: `mac-mini`, 并发 1 |
| GitHub Actions Runner | self-hosted, 并发 1 |
| Rust toolchain | stable（通过 rustup，CI 里 `rustup update` 保持最新） |
| just | 全局安装 |
| cargo-zigbuild + zig | 全局安装（为 Linux 交叉编译） |
| Node.js | 为 `npm publish` |
| cargo registry | 共享 `~/.cargo/registry/`（只读并发安全） |
| target 目录 | 隔离，GitLab/GitHub 各自独立（避免并发写） |

## npm 包结构（主包 + 平台子包）

**设计目标：** 主包体积小，用户只下载自己平台的二进制。esbuild、swc 等项目的标准做法。

### 目录结构

```
yi-agent-rs/
├── justfile
├── crates/...
├── yi-agent-cli/                 ← npm 包根（主包）
│   ├── .gitignore                ← 忽略 binaries/
│   ├── package.json              ← 主包，声明 optionalDependencies
│   ├── entry.js                  ← 平台选择入口，动态 require 子包
│   └── platforms/                ← 各平台子包（二进制不入版本控制）
│       ├── darwin-x64/
│       │   ├── package.json
│       │   └── binaries/
│       │       └── yi-agent      ← 构建时填充，gitignored
│       ├── darwin-arm64/
│       │   ├── package.json
│       │   └── binaries/
│       │       └── yi-agent
│       └── linux-x64/
│           ├── package.json
│           └── binaries/
│               └── yi-agent
└── ...
```

### 主包 `yi-agent-cli/package.json`

```json
{
  "name": "@gongyichen/yi-agent",
  "version": "0.1.0",
  "description": "A coding agent CLI",
  "license": "MIT",
  "repository": {
    "type": "git",
    "url": "https://github.com/gongyichen/yi-agent"
  },
  "bin": {
    "yi-agent": "entry.js"
  },
  "optionalDependencies": {
    "@gongyichen/yi-agent-darwin-x64": "0.1.0",
    "@gongyichen/yi-agent-darwin-arm64": "0.1.0",
    "@gongyichen/yi-agent-linux-x64": "0.1.0"
  },
  "files": ["entry.js"]
}
```

### 平台子包 `yi-agent-cli/platforms/darwin-x64/package.json`

```json
{
  "name": "@gongyichen/yi-agent-darwin-x64",
  "version": "0.1.0",
  "description": "yi-agent binary for darwin-x64",
  "license": "MIT",
  "os": ["darwin"],
  "cpu": ["x64"],
  "files": ["binaries/"]
}
```

（`darwin-arm64` 和 `linux-x64` 子包类似，只改 `name`、`os`、`cpu`）

### `yi-agent-cli/entry.js`

```javascript
#!/usr/bin/env node
const { existsSync } = require('fs');
const { join, dirname } = require('path');

const platform = process.platform;
const arch = process.arch;

const platformMap = {
  'darwin-x64': '@gongyichen/yi-agent-darwin-x64',
  'darwin-arm64': '@gongyichen/yi-agent-darwin-arm64',
  'linux-x64': '@gongyichen/yi-agent-linux-x64',
};

const key = `${platform}-${arch}`;
const pkgName = platformMap[key];

if (!pkgName) {
  console.error(`Unsupported platform: ${key}`);
  process.exit(1);
}

let binPath;
try {
  const pkgJsonPath = require.resolve(`${pkgName}/package.json`);
  binPath = join(dirname(pkgJsonPath), 'binaries', 'yi-agent');
} catch (e) {
  console.error(`Platform package ${pkgName} not installed.`);
  process.exit(1);
}

if (!existsSync(binPath)) {
  console.error(`Binary not found at ${binPath}`);
  process.exit(1);
}

const { spawn } = require('child_process');
const child = spawn(binPath, process.argv.slice(2), { stdio: 'inherit' });
child.on('close', (code) => process.exit(code ?? 1));
```

### 关键设计点

1. **发布顺序固定** - 子包先发（因为主包的 `optionalDependencies` 引用它们），justfile 的 `npm-publish` 任务内置这个顺序
2. **版本号单一真源** - `Cargo.toml` 的 `workspace.package.version` 是版本号唯一来源，`npm-pack` 时自动同步到主包和所有子包的 `package.json`
3. **os/cpu 字段让 npm 自动跳过** - 子包声明 `"os": ["darwin"]` 后，Linux 用户 `npm install` 时 npm 会自动跳过这个子包
4. **需要 npm scope** - 用 `@gongyichen` 作为 scope（在 npmjs.com 免费创建 organization 即可获得）
5. **二进制目录 gitignored** - `yi-agent-cli/platforms/*/binaries/` 不入版本控制，由 justfile 在构建时填充

## 完整文件清单

```
yi-agent/                                    ← 仓库根（GitLab 主仓库）
├── .gitignore                               ← 修改：追加 npm 二进制目录
├── .gitlab-ci.yml                           ← 新增：GitLab CI 配置
├── .github/
│   ├── workflows/
│   │   ├── ci.yml                           ← 新增：PR 触发的 CI
│   │   └── release.yml                      ← 新增：tag 触发的 release
│   └── scripts/
│       └── update-homebrew-tap.sh           ← 新增：Homebrew 自动更新脚本
├── yi-agent-rs/
│   ├── justfile                             ← 新增：核心任务定义
│   ├── rust-toolchain.toml                  ← 新增：pin stable
│   ├── crates/...                           ← 不动
│   └── yi-agent-cli/                        ← 新增：npm 包子项目
│       ├── .gitignore                       ← 新增
│       ├── package.json                     ← 新增：主包元数据
│       ├── entry.js                         ← 新增：平台选择入口
│       └── platforms/                       ← 新增：平台子包
│           ├── darwin-x64/
│           │   └── package.json
│           ├── darwin-arm64/
│           │   └── package.json
│           └── linux-x64/
│               └── package.json
└── docs/plans/
    └── 2026-07-18-yi-agent-core-design.md  ← 不动
```

### 补充文件内容

**`yi-agent-rs/rust-toolchain.toml`：**

```toml
[toolchain]
channel = "stable"
```

**`yi-agent-rs/yi-agent-cli/.gitignore`：**

```
binaries/
platforms/*/binaries/
node_modules/
```

**根目录 `.gitignore` 追加：**

```
yi-agent-rs/yi-agent-cli/platforms/*/binaries/
```

## 落地步骤

1. **新增基础文件**（本地）
   - 在 `yi-agent-rs/` 创建 `justfile`、`rust-toolchain.toml`
   - 在 `yi-agent-rs/yi-agent-cli/` 创建 `package.json`、`entry.js`、`.gitignore`
   - 在 `yi-agent-rs/yi-agent-cli/platforms/{darwin-x64,darwin-arm64,linux-x64}/` 创建各子包 `package.json`
   - 更新根 `.gitignore`

2. **新增 CI 配置**
   - 在仓库根创建 `.gitlab-ci.yml`
   - 在 `.github/workflows/` 创建 `ci.yml` 和 `release.yml`
   - 在 `.github/scripts/` 创建 `update-homebrew-tap.sh`

3. **创建 Homebrew tap 仓库**（GitHub 上手动操作）
   - 新建 `gongyichen/homebrew-yi-agent` 仓库
   - 在 `Formula/` 目录创建初始 `yi-agent.rb`（可以用占位 SHA256，首次 release 会自动更新）

4. **Mac mini 配置**（在 Mac mini 上操作）
   - 安装工具链：Rust、just、cargo-zigbuild、zig、Node.js
   - 注册 GitLab Runner（tag: `mac-mini`，executor: shell）
   - 注册 GitHub Actions Runner（self-hosted）

5. **配置 Secrets**
   - GitHub 仓库 Secrets：`NPM_TOKEN`、`HOMEBREW_TAP_TOKEN`
   - GitLab 仓库无额外 secret（本地 runner 不需要 token）

6. **配置 GitLab push mirror 到 GitHub**
   - GitLab 仓库 → Settings → Repository → Mirroring repositories
   - 添加 GitHub 仓库 URL，方向：Push
   - 使用 GitHub PAT 认证

7. **首次验证**
   - push 到任意分支，观察 GitLab CI 是否在 Mac mini 上跑通
   - 开一个 MR，观察 PR CI 是否在云端跑通
   - 打 tag `v0.1.0`，观察 release 流水线是否在 Mac mini 上跑通
   - 验证三个渠道（GitHub Release、npm、Homebrew tap）是否都更新

## 后续可选改进（YAGNI，先不做）

- crates.io 发布（如果用户想 `cargo install yi-agent`）
- 覆盖率统计（codecov.io）
- cargo-deny 安全审计
- release-plz 自动版本号推断
- ARM Linux 支持（aarch64-unknown-linux-gnu）
- Windows 支持（x86_64-pc-windows-msvc）
