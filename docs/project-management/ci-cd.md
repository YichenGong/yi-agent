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
- [x] 发布构建环境门禁（原条目「release workflow 环境门禁」的根因描述已更正）— tag 触发的 Release 从 v0.1.4 起每次在 `Build & package all platforms` 1~3 秒失败，**真根因是 PATH 顺序让 Homebrew 的 Rust 抢占 rustup 的 Rust**（`/opt/homebrew/bin/cargo` → Cellar/rust/1.98.1，其 sysroot 不含 rustup 安装的跨平台 std，交叉编译报 `E0463: can't find crate for core`；runner 实测同一命令 0.468 秒失败，把 `$HOME/.cargo/bin` 提到最前则 1m05s 成功），**加上 launchd 的 `maxfiles=256` 导致并行链接 Linux 目标时 `ProcessFdQuotaExceeded`**（`ulimit -n 65536` 后 1m06s 成功）。此前登记的「家目录写死」不成立（runner 的 `HOME=/Users/agenticlabs` 可写）；`738d3eb0` 那次改动的实际价值是路径改由 `$HOME` 推导与补 `rustup update stable`，但没修掉失败。现修复：新增 `yi-agent-rs/scripts/setup-build-env.sh`（重排 PATH 让 rustup 优先、抬高 fd 上限、校验三平台 std 真的在 sysroot 里 + cargo/rustc 版本一致 + zig/zigbuild 可用，任一不满足即非 0 退出并给出可操作提示；判据基于 sysroot 而非 shim，因为直接软链到 toolchain 的布局同样合法），`justfile` 的 `build-target` 与 `release.yml` 的构建 step 均先 source 它，`GITHUB_PATH` 改为「先 brew、后 cargo」以保证最终顺序正确；`test_setup_build_env.sh`（10 例）与扩充后的 `test_release_workflow_env.sh`（12 例，新增写入顺序、必须 source 环境脚本、必须抬高 fd、必须拒绝 Homebrew 抢占）分别并入 `ci` 的 `build-env-check`、`release-env-check`。判据：`cd yi-agent-rs && just build-env-check`（10 passed）、`just release-env-check`（12 passed）；回退 PATH 重排即 3 条红、回退 workflow 顺序即 2 条红；真机（刻意用坏 PATH）三平台 `just build-target` 全部成功（1m11s / 57.4s / 57.2s，38.8MB ELF + 42.4/44.9MB Mach-O）
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
