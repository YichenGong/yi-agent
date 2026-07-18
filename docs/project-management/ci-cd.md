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

**不做什么：**
- 不做覆盖率统计（codecov.io）— YAGNI
- 不做 crates.io 发布 — YAGNI
- 不做 ARM Linux 支持 — YAGNI
- 不做 Windows 支持 — YAGNI
- 不做安全审计（cargo-deny）— YAGNI

## Features

- [x] CI/CD 设计文档 — [设计](../plans/2026-07-19-ci-cd-design.md)
- [x] CI/CD 实现计划 — [实现](../plans/2026-07-19-ci-cd-impl.md)
- [ ] justfile 与 rust-toolchain.toml
- [ ] npm 包结构（主包 + 平台子包）
- [ ] Homebrew tap 自动更新脚本
- [ ] GitLab CI 配置
- [ ] GitHub Actions CI 配置（PR 触发）
- [ ] GitHub Actions Release 配置（tag 触发）
- [ ] Mac mini runner 配置
- [ ] 首次端到端验证
- [-] 覆盖率统计（codecov.io）— YAGNI，暂不做
- [-] crates.io 发布 — YAGNI，暂不做
