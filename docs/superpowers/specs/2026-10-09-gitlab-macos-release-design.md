# GitLab 内网发布 macOS 桌面 App Design

日期：2026-10-09
状态：设计已确认（用户拍板：只 arm64、先不签名、版本号以 `yi-agent-rs/Cargo.toml` 为准），待用户 review 本文件后进入 writing-plans。
范围：`.gitlab-ci.yml`、新增 `desktop/scripts/release-dmg.sh`、`README.md`（下载小节）、`docs/project-management/ci-cd.md`（模块登记）。

## 1. 背景与问题

内网同事希望通过 GitLab 下载并使用 macOS 桌面 App。现状：

- **GitLab CI 只跑 check。** `.gitlab-ci.yml` 只有一个 stage `check`，两个 job（`ci-push` 跑 Mac mini self-hosted runner、`ci-mr` 跑云端 runner），都只执行 `just ci`。**GitLab 侧没有任何打包/发布能力。**
- **打包发布全在 GitHub，且不含桌面 App。** `.github/workflows/release.yml` 在 tag 推送时跑 `just package-all`，产出的是 **CLI** 的 `.tar.gz`，发到 GitHub Releases + npm + Homebrew。桌面 App（`.dmg`）**不在任何自动流水线里**。
- **桌面 App 目前只有手工产物。** 本地执行 `cd desktop && npm run sidecar:release && npm run tauri build` 得到 `desktop/src-tauri/target/release/bundle/dmg/yi-agent_0.1.0_aarch64.dmg`，靠"把 `.app` 拷进 `/Applications`"人工分发。

GitLab 部署在内网（`ssh://git@10.79.10.70:1022`，Web 在 `http://10.79.10.70/`），目标用户是内网同事，无外网可达诉求。

### 1.1 已核实的事实

| 事实 | 证据 |
|---|---|
| GitLab 已有 Mac mini self-hosted runner（tag `mac-mini`、shell executor） | `.gitlab-ci.yml:20-23`（`tags: mac-mini`）；该 job 已 `source ~/.cargo/env`、自愈安装 rustup/just |
| runner 上 Node/npm **未确认存在**，需 job 内自愈 | GitHub release 用 `actions/setup-node@v4` 单独装 node（`.github/workflows/release.yml:87-89`），说明 node 不在默认 PATH |
| GitHub release 打包的 mac 目标是 **arm64** | `just package-all` → `build-all-targets` 含 `aarch64-apple-darwin`（`.github/scripts/update-homebrew-tap.sh:36`） |
| runner `PATH` 模式 | `.github/workflows/release.yml:21`（`/opt/homebrew/bin:…:$HOME/.local/bin:$HOME/.cargo/bin:…`），GitLab shell job 可沿用同一思路 |
| `bundle/`、`*.dmg` **未被 gitignore** | `.gitignore` 只有 `/target/`、`**/target/`；`desktop/.gitignore` 只忽略 `src-tauri/target`、`src-tauri/binaries` |
| `glab` / `release-cli` **未安装** | `command -v glab release-cli` 无输出 |
| Tauri bundle 目标为 `"all"` | `desktop/src-tauri/tauri.conf.json:26` |
| 版本号分散且不同步 | `yi-agent-rs/Cargo.toml:21` = `0.1.3`（workspace）；`desktop/package.json:4` = `0.1.0`；`desktop/src-tauri/tauri.conf.json:4` = `0.1.0`；`desktop/src-tauri/Cargo.toml:3` = `0.1.0` |
| 配置只从 `.env` 读，key 必需 | `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:100-220`：local `.yi-agent/.env` → global `~/.yi-agent/.env`；无 key 报 `API key required: set MODEL_API_KEY` |
| desktop 无 API key 设置界面 | `desktop/src-tauri/src/bridge.rs` 只处理 relay，key 只能由 `~/.yi-agent/.env` 提供 |

## 2. 目标与非目标

**目标**

1. 打 `v*.*.*` tag 时，GitLab 流水线在 mac-mini runner 上自动构建 macOS（arm64）`.dmg` 并发布为 GitLab Release 资产。
2. 内网同事经 **Deploy → Releases** 一键下载 `.dmg`，无需登录外网、无需任何 token。
3. 未签名产物可安装：随 Release 附安装脚本，清掉 `com.apple.quarantine`。
4. 版本号以 `yi-agent-rs/Cargo.toml` 为唯一真源，tag 与之一致才允许发布。
5. 发布说明写清"装完还要配 `~/.yi-agent/.env` 的模型 key"，避免同事装完打不开用。

**非目标**

- **不做签名与公证**（Apple Developer $99/年 + notarytool）。用户拍板先不签名。
- **不支持 Intel mac**（x86_64-apple-darwin）。用户拍板只 arm64。
- 不做 GitLab Pages 下载页（YAGNI；Release 页够用）。
- 不改动 GitHub release 流水线（那条继续管 CLI/npm/Homebrew）。
- 不做版本号自动同步（把 `desktop/*` 三处对齐 Cargo.toml）。本次只在 job 里**校验** tag 与 Cargo.toml 一致，不做写入。
- 不做 Linux/Windows 桌面产物（YAGNI）。

## 3. 设计决策汇总

| # | 决策 | 取值 | 理由 |
|---|---|---|---|
| 1 | 触发方式 | `rules: if: $CI_COMMIT_TAG` + `tags: [mac-mini]` | 与 GitHub release 的 `v*.*.*` tag 语义对齐；Tauri 必须在本平台原生构建 |
| 2 | 产物存放 | **Generic Package Registry**，非 job artifact | Package 文件不过期；job artifact 受 `expire_in`（默认 30 天）限制，且需改实例设置才能永久 |
| 3 | 上传/下载鉴权 | `JOB-TOKEN: $CI_JOB_TOKEN` | 零额外 token/密码，无需新增 CI 变量 |
| 4 | 建 Release 的方式 | 直接 `curl` 调 Release API，**不用 `release:` 关键字** | `release:` 依赖 release-cli，自建 GitLab 不预装（GitLab.com docker runner 才自带）；`glab`/`release-cli` 本机也未装。直接调 API 零依赖、对实例版本不敏感 |
| 5 | 资产直链 | Release `assets.links` 带 `filepath` | 用户点下载时文件名是 `yi-agent_<版本>_aarch64.dmg`，而非 URL 尾段 |
| 6 | 架构 | 仅 arm64 | 用户拍板；与 GitHub release 的 `aarch64-apple-darwin` 一致 |
| 7 | 签名 | 不签名 + 附 `install.sh` 清 quarantine | 用户拍板；内网分发，避免 Apple 账号成本 |
| 8 | 版本真源 | `yi-agent-rs/Cargo.toml` 的 workspace `version` | 与 GitHub release 的校验逻辑一致（`release.yml` 的 "Verify tag matches Cargo.toml version"）；桌面端三处 version 不参与 |
| 9 | bundle 目标 | job 内把 `targets` 改为 `app,dmg` | 现为 `"all"`，会连带产出 updater tar 等，且大版本下 `all` 可能引入非 mac 目标导致失败 |
| 10 | dmg 文件名 | 统一重命名为 `yi-agent_<版本>_aarch64.dmg` | Tauri 默认用 `package.json` 的 `0.1.0`，与 tag 版本不符 |

## 4. 设计细节

### 4.1 流水线结构

`.gitlab-ci.yml` 新增 `release` stage（置于 `check` 之后），单个 job：

```yaml
stages:
  - check
  - release

release:macos-dmg:
  stage: release
  tags:
    - mac-mini
  rules:
    - if: $CI_COMMIT_TAG
  script:
    - bash desktop/scripts/release-dmg.sh
```

job 保持"薄"：全部逻辑在 `desktop/scripts/release-dmg.sh` 里，便于在 runner 上单独复跑、也便于本地调试。

### 4.2 `desktop/scripts/release-dmg.sh` 步骤

脚本按顺序执行，任一步失败即退出（`set -euo pipefail`）：

1. **环境自愈**：`source "$HOME/.cargo/env"`；`command -v node` 缺失则报错并提示（或尝试通过 brew 安装），`command -v rustup`/`just` 的处理沿用 `.gitlab-ci.yml:36-44` 的既有写法。
2. **版本校验**：从 `$CI_COMMIT_TAG` 或参数取 tag；比对 `yi-agent-rs/Cargo.toml` 的 `^version`；不一致直接 `exit 1`。算出 `VERSION`。
3. **前端依赖**：`cd desktop && npm ci`。
4. **sidecar**：`npm run sidecar:release`（把 release 构建的 `yi-agent` 复制成 `src-tauri/binaries/yi-agent-aarch64-apple-darwin`，机制见 `desktop/scripts/build-sidecar.sh:8-21`）。
5. **改 bundle 目标**：用 `jq` 把 `tauri.conf.json` 的 `bundle.targets` 从 `"all"` 改写为 `["app","dmg"]`（仅工作区临时改，不提交）。
6. **构建**：`npm run tauri build`。
7. **重命名**：把 `bundle/dmg/*.dmg` 复制为 `yi-agent_<VERSION>_aarch64.dmg`；同时产出 `.sha256`。
8. **上传 Package**：
   ```bash
   curl --fail --header "JOB-TOKEN: $CI_JOB_TOKEN" \
     --upload-file yi-agent_<VERSION>_aarch64.dmg \
     "$CI_API_V4_URL/projects/$CI_PROJECT_ID/packages/generic/yi-agent-macos/$VERSION/yi-agent_<VERSION>_aarch64.dmg"
   ```
   同一 URL 亦可 `.sha256` 上传。
9. **建 Release**：`curl -X POST "$CI_API_V4_URL/projects/$CI_PROJECT_ID/releases"`，body 含 `tag_name`、`name`、`description`（写明安装步骤 + `~/.yi-agent/.env` 配置要求 + quarantine 旁路），以及 `assets.links`：
   - `name: yi-agent_<VERSION>_aarch64.dmg`，`url: <package URL>`，`filepath: /yi-agent_<VERSION>_aarch64.dmg`，`link_type: package`
   - 安装脚本同理挂为 `/install.sh`
10. **install.sh**：随 Release 上传，内容为挂载 dmg、`ditto` 拷贝到 `/Applications`、`xattr -dr com.apple.quarantine`。

### 4.3 用户侧流程（写进 Release 说明与 README）

```
1. Deploy → Releases → 下载 yi-agent_<版本>_aarch64.dmg
2. 双击前先清隔离(或直接跑 install.sh)：
     xattr -dr com.apple.quarantine /Applications/yi-agent.app
3. 配模型 key（App 没有设置界面，必须手写文件）：
     mkdir -p ~/.yi-agent && echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env
4. 从 /Applications 打开 yi-agent
```

### 4.4 错误处理

| 情况 | 行为 |
|---|---|
| tag 与 `Cargo.toml` 不一致 | job 在步骤 2 fail，不产生任何 Release |
| runner 缺 node | 步骤 1 明确报错（提示 `brew install node`），不静默继续 |
| Package 上传失败 | `curl --fail` 退出，Release 不建（避免挂空链） |
| Release 已存在（重跑） | 先 `GET` 检查，存在则 `PUT` 更新 description/assets，避免 409 |

## 5. 测试与验证

本设计的验证以"真实跑通一次"为准，不引入单测（CI 配置与 shell 脚本，单测收益低）：

1. **本地干跑脚本**：在开发机上以 `CI_COMMIT_TAG=$HOME/.yi-agent/... ` 模拟环境跑 `release-dmg.sh` 的前半段（构建 + 重命名），确认 `tauri build` 产出与重命名正确。上传/建 Release 段用 dry-run 变量跳过。
2. **真流水线**：推一个 `v0.1.4` tag（版本号需先同步 Cargo.toml），观察 job 全绿。
3. **产物验收**：在 GitLab Deploy → Releases 页确认 dmg 可下载；下载到另一台 arm64 Mac，按 §4.3 步骤安装，`open` 后 `ps` 可见 sidecar `yi-agent app-server` 子进程。
4. **卸载/覆盖**：再发一个 tag，确认同 URL 更新、旧版本 Release 不被破坏。

> 注：验证步骤 3 需要一台**非开发机的 arm64 Mac**（最好是无 Rust/node 的干净机器），才能真正证明"别人都能装"。若不可得，退而求其次在开发机上把 `.app` 拷到 `/Applications` 并清 quarantine 后启动。

## 6. 影响面与周边更新

| 文件 | 变更 |
|---|---|
| `.gitlab-ci.yml` | 新增 `release` stage + `release:macos-dmg` job |
| `desktop/scripts/release-dmg.sh` | 新增（约 80-120 行） |
| `README.md` | 「直接下载」旁新增「内网从 GitLab 下载 Mac 版」小节 |
| `docs/project-management/ci-cd.md` | Features 增加一条：GitLab Release 发布 macOS dmg，附可验证判据 |

## 7. 开放风险

1. **runner 是否已装 node**：未知。job 首个 step 会自愈，若装不上需人工在 mini 上 `brew install node`。这是上线前唯一需实地确认的环境项。
2. **GitLab 版本与 Generic Package Registry**：Generic packages 自 GitLab 13.5 起可用，内网实例几乎必然满足。若包注册表被管理员关闭，回退方案是 job artifact（牺牲"不过期"）。
3. **Package Registry 存储配额**：dmg 约几十 MB，多次发布累积。内网自建通常无配额限制，但应定期清理旧版本（可在设计外后续加清理 job）。
4. **未签名体验**：即便清了 quarantine，App 可能仍弹"无法验证开发者"（取决于 Gatekeeper 设置）。内网可让用户在 System Settings → Privacy & Security 点 "Open Anyway"，或统一关闭 Gatekeeper（不推荐）。此项需在 Release 说明里写全。
