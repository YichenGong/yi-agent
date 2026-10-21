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
- [x] 发布构建环境门禁（原条目「release workflow 环境门禁」的根因描述已更正）— tag 触发的 Release 从 v0.1.4 起每次在 `Build & package all platforms` 1~3 秒失败，**真根因是 PATH 顺序让 Homebrew 的 Rust 抢占 rustup 的 Rust**（`/opt/homebrew/bin/cargo` → Cellar/rust/1.98.1，其 sysroot 不含 rustup 安装的跨平台 std，交叉编译报 `E0463: can't find crate for core`；runner 实测同一命令 0.468 秒失败，把 `$HOME/.cargo/bin` 提到最前则 1m05s 成功），**加上 launchd 的 `maxfiles=256` 导致并行链接 Linux 目标时 `ProcessFdQuotaExceeded`**（`ulimit -n 65536` 后 1m06s 成功）。此前登记的「家目录写死」不成立（runner 的 `HOME=/Users/agenticlabs` 可写）；`738d3eb0` 那次改动的实际价值是路径改由 `$HOME` 推导与补 `rustup update stable`，但没修掉失败。现修复：新增 `yi-agent-rs/scripts/setup-build-env.sh`（重排 PATH 让 rustup 优先、抬高 fd 上限、校验三平台 std 真的在 sysroot 里 + cargo/rustc 版本一致 + zig/zigbuild 可用，任一不满足即非 0 退出并给出可操作提示；判据基于 sysroot 而非 shim，因为直接软链到 toolchain 的布局同样合法），`justfile` 的 `build-target` 与 `release.yml` 的构建 step 均先 source 它，`GITHUB_PATH` 改为「先 brew、后 cargo」以保证最终顺序正确；`test_setup_build_env.sh`（10 例）与扩充后的 `test_release_workflow_env.sh`（14 例，新增写入顺序、必须 source 环境脚本、必须抬高 fd、必须拒绝 Homebrew 抢占、调用 `just` 的 step 必须有 `working-directory: yi-agent-rs`）分别并入 `ci` 的 `build-env-check`、`release-env-check`。判据：`cd yi-agent-rs && just build-env-check`（10 passed）、`just release-env-check`（14 passed）；回退 PATH 重排即 3 条红、回退 workflow 顺序即 2 条红；真机（刻意用坏 PATH）三平台 `just build-target` 全部成功（1m11s / 57.4s / 57.2s，38.8MB ELF + 42.4/44.9MB Mach-O）
- [x] npm 版本同步门禁 — `yi-agent-rs/scripts/sync-npm-versions.sh` + `scripts/test_sync_npm_versions.sh`；`npm-pack` 调用它同步主包、子包与 `optionalDependencies`，并自检 `platforms/*` 与依赖表逐一对应；`just npm-version-check` 已并入 `ci`。判据：`cd yi-agent-rs && just npm-version-check`（22 passed）
- [x] npm 包结构 — `yi-agent-rs/yi-agent-cli/` 主包 + 平台子包
- [x] Homebrew tap 自动更新脚本 — 发布脚本存在
- [x] GitLab CI 配置 — `.gitlab-ci.yml` 存在
- [x] GitHub Actions CI 配置 — `.github/workflows/ci.yml` PR 触发
- [x] GitHub Actions Release 配置 — `.github/workflows/release.yml` tag 触发
- [x] Mac mini runner 配置 — self-hosted runner 接入
- [x] 首次端到端验证 — v0.1.0 + v0.1.1 release 流水线全流程通过
- [x] GitLab 内网发布 macOS 桌面 App（arm64 dmg）+ 独立 CLI — `release:macos-dmg` job（`.gitlab-ci.yml`，tag 触发）+ `desktop/scripts/release-dmg.sh`（版本门禁 / 注入 App 版本 / 构建 dmg 与 CLI / 上传 Generic Package / 建 Release）+ `desktop/packaging/install.sh`（清 quarantine）；产物不经 GitHub，下载入口为 GitLab Deploy → Releases；验证：推 `v0.1.6` tag 后 job 全绿（pipeline 947）、Release 页 `yi-agent_0.1.6_aarch64.dmg` 与 `yi-agent-aarch64-apple-darwin.tar.gz` 均可下载且 sha256 匹配、dmg 内 App 的 `CFBundleShortVersionString` 与 CLI 均为 0.1.6；安装器侧的 CLI 软链与升级流程见 `docs/project-management/desktop.md` — [设计](../superpowers/specs/2026-10-09-gitlab-macos-release-design.md)
- [x] 面向用户的模型配置说明纠正（Release 说明 / 安装器 / README 中英）— 原文案教用户 `mkdir -p ~/.yi-agent && echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env`，并声称「App 内没有设置入口」。两处都不成立：模型配置的唯一真相源是 `~/.yi-agent/models.json`，`.env` 里的模型键会被 `yi-agent-runtime/src/config.rs:243` 的 `prune_dotenv_model_keys` 主动剔除；且迁移要求 `.env` 同时含 `YI_AGENT_MODEL`，只给 key 会以 `MissingModel` 静默放弃（`migrate.rs`）。实测照做后 `yi-agent run` 报「尚未配置模型」并 exit 1，`.env` 原样保留、`models.json` 未生成。设置入口早在 2026-10-08/09 就存在：`desktop/src/components/SettingsModelsTab.tsx`（设置 → 模型）与 `desktop/src/components/OnboardingWizard.tsx`（首启引导）。修复：`desktop/scripts/release-dmg.sh:170`（改为首启向导 + 设置 → 模型）、`:184`（共用 `models.json`）、`desktop/packaging/install.sh:257`（同上）、`README.md`、`README.en.md` 的桌面安装段与「快速开始」（改为 `yi-agent` 首启引导或三项齐全的整包参数），并把「换模型（整包覆盖）」一行补全 `YI_AGENT_MODEL`/`--model`。防复发：新增 `yi-agent-rs/scripts/check-model-setup-docs.sh`（扫 README×2、release-dmg.sh、install.sh 四个面向用户的文件，禁 `MODEL_API_KEY=sk-ant` 写进 `.env`、禁「没有设置入口」、禁「共用 .env」；非 git 仓库 exit 2 拒绝假绿；`docs/superpowers/**` 与 `docs/project-management/**` 是历史记录、允许引用旧文案）与自测 `yi-agent-rs/scripts/test_model_setup_docs.sh`（7 例），并入 `just ci` 的 `model-setup-docs-check`。验证：`bash yi-agent-rs/scripts/test_model_setup_docs.sh`（7 passed）、`bash yi-agent-rs/scripts/check-model-setup-docs.sh`（4 targets ok）、`bash desktop/scripts/test_install_sh.sh`（44 passed）、`bash -n` 两个脚本；回退验证：拿改前的原文件跑门禁，精确报出 5 处 `.env` 教学 + 2 处「没有设置入口」+ 1 处「共用 .env」

- [x] 发布 tag 位置门禁 — `yi-agent-rs/scripts/check-tag-on-main.sh` 断言 tag 指向的提交是 `main` 的祖先（`git merge-base --is-ancestor <tag^{}> main`），`release.yml`（`Verify the tag is on main`，在 `Verify tag matches Cargo.toml version` 之前）与 `.gitlab-ci.yml` 的 `release:macos-dmg` 都在动编译器之前调用它。动机：`release.yml` 只由 tag 触发且只打包不跑测试，一个打在未合并功能分支上的 tag 会被**直接发布**，线上版本从此脱离主干。判据：`cd yi-agent-rs && just tag-gate-check`（7 passed；`test_check_tag_on_main.sh` 覆盖 tag 在 main / 在未合并分支 / 在已合并分支 / tag 不存在 / base 分支不存在 / 参数个数错 / annotated tag）
- [x] 分支模型收敛为单一 `main` — 移除 `release/**` 的 CI 触发（`.github/workflows/ci.yml` 回到 `branches: [main]`），发版改为 `main` 上的一次动作（`just ci` → bump 版本 → 推 → 打 annotated tag → 推 tag）；仅「给旧版本发补丁」才从 tag 切短命 `fix/X.Y.Z`。依据：实测 `release/0.1.8` 上只有一个提交（bump + 两处 clippy），没有一件事必须靠常设分支完成。详见设计 D1 的修订段
- [x] npm 桌面 App 构建 step 的两处接线错误 — v0.1.9（run 39631821559）在新增的 `Build macOS desktop app for npm` 以 **0.03 秒**退 1，构建/打包/版本校验等前序 step 全绿。根因两条：(1) 该 step 与紧随的 `Verify the desktop app is in the npm package` 都漏了 `working-directory: yi-agent-rs`——justfile 只在 `yi-agent-rs/` 下，仓库根没有，`just npm-pack-desktop` 因此以 "no justfile found" 立即失败（同文件其余调用 `just` 的 step 都带该字段，属遗漏而非设计）；(2) 后者的版本门禁写 `../../Cargo.toml`，但脚本已 `cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64`，两级只到 `yi-agent-cli/`（无 Cargo.toml），须三级 `../../../Cargo.toml`，否则版本不一致时门禁静默失效。真机单独跑 `bash desktop/scripts/package-npm-app.sh` 完全成功（产 47M `yi-agent.app`），证明失败纯属 workflow 接线而非脚本或环境问题。防复发：`test_release_workflow_env.sh` 增至 14 例，新增「调用 `just` 的 step 必须有 `working-directory: yi-agent-rs`」与「子包校验必须用 `../../../Cargo.toml`」两条静态断言。判据：`cd yi-agent-rs && just release-env-check`（14 passed）；回退 workflow 到修复前即精确报出这 2 条红，其余 12 条仍绿
- [x] macOS 桌面 App 随 npm 分发 — `desktop/scripts/package-npm-app.sh` 用 Tauri 产出 `.app`、`codesign --force --deep --sign -` 修复 Tauri 的坏签名（`codesign --verify` 原本报 "code has no resources but signature indicates they must be present"）、再放进 `yi-agent-cli/platforms/darwin-arm64/`；该子包 `postinstall` 执行 `install-app.js`，仅在 `npm_config_global === "true"` 且 darwin/arm64 时复制到 `/Applications` 并按 `CFBundleShortVersionString` 比对版本，任何异常均退出 0 不拖垮 CLI 安装。判据：`cd yi-agent-rs/yi-agent-cli/platforms/darwin-arm64 && node --test test-install-app.js`（11 passed）；`just npm-pack-desktop` 产出 `yi-agent.app` 且 `codesign --verify --deep --strict` 通过；真机 `npm i -g` 后 `/Applications/yi-agent.app` 存在且版本与 `Cargo.toml` 一致 — [设计](../superpowers/specs/2026-10-21-npm-desktop-app-design.md)
- [x] 发布版本门禁（release vs dev）— `yi-agent-rs/crates/yi-agent/build.rs` 用 `git describe --tags --exact-match` 判定构建类型：命中 `v<Cargo 版本>` 为 `release`（裸版本号），有 git 无 tag 为 `<版本>-dev+<sha>[-dirty]`，无 git 为 `<版本>+nogit`；`--version` 因此能自证身份。流水线侧 `.github/workflows/release.yml` 改为 `fetch-depth: 0` 并新增 “Verify the binary reports the release version” step，`.gitlab-ci.yml` 的 `release:macos-dmg` 设 `GIT_DEPTH: "0"`。判据：`bash yi-agent-rs/scripts/test_release_version_gate.sh`（6 passed）；本地 `cargo build -p yi-agent && ./target/debug/yi-agent --version` 得 `0.1.7-dev+<sha>`，`YI_AGENT_BUILD_KIND=release cargo build -p yi-agent` 得 `0.1.7`
- [x] 真实 LLM 测试基础设施 — justfile `test-real-llm` / `test-real-e2e` recipes + `#[ignore]` gate — [设计](../plans/2026-07-26-real-llm-testing-design.md)
- [-] 覆盖率统计（codecov.io）— YAGNI，暂不做
- [-] crates.io 发布 — YAGNI，暂不做
