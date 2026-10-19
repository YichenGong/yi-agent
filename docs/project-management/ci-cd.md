# ci-cd

## 模块说明

yi-agent 的自动化构建、测试、发布流水线。基于 justfile 作为单一真源，GitLab CI 管日常 push（Mac mini self-hosted runner），GitHub Actions 管 PR（云端 runner）和 release（Mac mini self-hosted runner）。发布到 GitHub Releases + Homebrew tap + npm（主包 + 平台子包）三渠道。

## 范围边界

**做什么：**
- 本地构建/测试/打包任务（justfile）
- CI 流水线配置（GitLab CI + GitHub Actions）
- 三平台交叉编译（macOS x64/ARM64、Linux x64）
- 三渠道发布（GitHub Release、Homebrew、npm）
- Mac mini runner 配置
- 真实 LLM 测试基础设施（`#[ignore]` gate，CI 不跑）
- GitLab Release 发布 macOS 桌面 App（arm64，内网，不签名）

**不做什么：**
- 不做覆盖率统计（codecov.io）— YAGNI
- 不做 crates.io 发布 — YAGNI
- 不做 ARM Linux 支持 — YAGNI
- 不做 Windows 支持 — YAGNI
- 不做安全审计（cargo-deny）— YAGNI

## Features

- [x] CI/CD 设计文档 — [设计](../plans/2026-07-19-ci-cd-design.md)
- [x] CI/CD 实现计划 — [实现](../plans/2026-07-19-ci-cd-impl.md)
- [x] justfile 与 rust-toolchain.toml — `yi-agent-rs/justfile` + `rust-toolchain.toml` 存在；其中 `bindings-check`（ts-rs 生成物漂移门禁）已并入 `ci: fmt-check lint test build bindings-check`，故 GitHub PR / GitLab push / GitLab MR 三条路径都跑该门禁
- [x] npm 版本同步门禁 — `yi-agent-rs/scripts/sync-npm-versions.sh` + `scripts/test_sync_npm_versions.sh`；`npm-pack` 调用它同步主包、子包与 `optionalDependencies`，并自检 `platforms/*` 与依赖表逐一对应；`just npm-version-check` 已并入 `ci`。判据：`cd yi-agent-rs && just npm-version-check`（22 passed）
- [x] npm 包结构 — `yi-agent-rs/yi-agent-cli/` 主包 + 平台子包
- [x] Homebrew tap 自动更新脚本 — 发布脚本存在
- [x] GitLab CI 配置 — `.gitlab-ci.yml` 存在
- [x] GitHub Actions CI 配置 — `.github/workflows/ci.yml` PR 触发
- [x] GitHub Actions Release 配置 — `.github/workflows/release.yml` tag 触发
- [x] Mac mini runner 配置 — self-hosted runner 接入
- [x] 首次端到端验证 — v0.1.0 + v0.1.1 release 流水线全流程通过
- [x] GitLab 内网发布 macOS 桌面 App（arm64 dmg）+ 独立 CLI — `release:macos-dmg` job（`.gitlab-ci.yml`，tag 触发）+ `desktop/scripts/release-dmg.sh`（版本门禁 / 注入 App 版本 / 构建 dmg 与 CLI / 上传 Generic Package / 建 Release）+ `desktop/packaging/install.sh`（清 quarantine）；产物不经 GitHub，下载入口为 GitLab Deploy → Releases；验证：推 `v0.1.6` tag 后 job 全绿（pipeline 947）、Release 页 `yi-agent_0.1.6_aarch64.dmg` 与 `yi-agent-aarch64-apple-darwin.tar.gz` 均可下载且 sha256 匹配、dmg 内 App 的 `CFBundleShortVersionString` 与 CLI 均为 0.1.6；安装器侧的 CLI 软链与升级流程见 `docs/project-management/desktop.md` — [设计](../superpowers/specs/2026-10-09-gitlab-macos-release-design.md)
- [x] 真实 LLM 测试基础设施 — justfile `test-real-llm` / `test-real-e2e` recipes + `#[ignore]` gate — [设计](../plans/2026-07-26-real-llm-testing-design.md)
- [-] 覆盖率统计（codecov.io）— YAGNI，暂不做
- [-] crates.io 发布 — YAGNI，暂不做
