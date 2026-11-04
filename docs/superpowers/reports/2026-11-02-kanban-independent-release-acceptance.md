# 独立发布验收报告（A01–A20）

- 日期：2026-11-05
- 执行器：`scripts/plugins/e2e.mjs --suite all --output .artifacts/plugins/acceptance.json`
- 校验器：`node scripts/plugins/e2e-report.mjs .artifacts/plugins/acceptance.json`
- 被测构建：`probe`（本地构建，非官方签名包）
- 结论：**14 / 20 通过，6 条 unsupported，0 条 fail。验收未完成，不构成发布依据。**

## 为什么这份报告不是「全绿」

本报告由执行器真跑产出：每条 `pass` 都对应一条命令真的执行过并成功。下面 6 条
不是失败，而是**这台机器此刻无法证明**它们，原因各不相同且都写在上表：

| 用例 | 缺什么 | 谁能补齐 |
|---|---|---|
| A03 | 官方签名包（`requires: official`） | 发布授权 + 生产签名密钥 |
| A07 / A08 | 已构建的 `.app`（`desktop/src-tauri/target/release/bundle/macos/yi-agent.app`） | 本地构建即可，见下 |
| A16 / A17 | 非 loopback 的真实远端主机 | 一台可访问的外部机器 |
| A20 | 官方签名/公证包 | 发布授权 + 生产签名密钥 |

把「没有远端机器」写成 `skipped` 会让人以为无关紧要；写成 `pass` 则是造假。
所以执行器只有三种判词：`pass` / `fail` / `unsupported`，且 `unsupported`
必须带理由。

## 逐条结果

| 用例 | 判词 | 证据（命令或缺失原因） |
|---|---|---|
| A01 | pass | `python3 scripts/plugins/check-host-boundary.py` |
| A02 | pass | `cargo nextest run -p yi-agent-supervisors -E 'test(install::tests)'` |
| A03 | unsupported | `missing capability: official` |
| A04 | pass | `cargo nextest run -p yi-agent-supervisors -E 'test(managed_launch)'` |
| A05 | pass | `cargo nextest run -p yi-agent-plugins -E 'test(admission)'` |
| A06 | pass | `cargo nextest run -p yi-agent-skills --test managed_source` |
| A07 | unsupported | `missing prerequisite: desktop/src-tauri/target/release/bundle/macos/yi-agent.app` |
| A08 | unsupported | `missing prerequisite: desktop/src-tauri/target/release/bundle/macos/yi-agent.app` |
| A09 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A10 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A11 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A12 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A13 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A14 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A15 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A16 | unsupported | `no remote host was provided; a loopback peer cannot prove cross-machine behaviour` |
| A17 | unsupported | `no remote host was provided; a loopback peer cannot prove cross-machine behaviour` |
| A18 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A19 | pass | `node ../scripts/plugins/fault-matrix.mjs` |
| A20 | unsupported | `no official signed release candidate was authorised; a local build cannot prove the download path` |

## 如何把 6 条 unsupported 变成可判定

1. **A07/A08**：在 worktree 里构建 bundle 后重跑即可——
   `cd desktop && npm ci && npm run build && npx tauri build --bundles app --features plugin-probe`
   （A08 的故障注入用例需要同一个 probe bundle）。
2. **A16/A17**：需要先实现 `scripts/plugins/e2e-remote.mjs`，再给一台真实远端
   主机，重跑 `node scripts/plugins/e2e.mjs --suite A16,A17 --remote-host <host>`。
   当前这两条把该脚本列为前置需求，所以**即使声明了远端主机**它们仍记
   `unsupported: missing prerequisite: scripts/plugins/e2e-remote.mjs`——缺的是
   执行器，不是机器。这样写是为了不让一个不存在的脚本冒充「跑过但失败」。
3. **A03/A20**：需要获授权的签名 release candidate，本地无法伪造。

## 已知缺口（本报告暴露出来的，未在本轮修复）

- `scripts/plugins/e2e-remote.mjs` 尚不存在。`acceptance-commands.json` 把
  A16/A17 绑到它上面，但没有远端主机时用例根本不会执行，所以这个缺口不会
  变成「假通过」，只会变成 `unsupported`。写它需要一台真实远端机器才能验证，
  无机器时写出来的只能是没跑过的代码。
- A03 依赖 `YI_AGENT_ACCEPTANCE_APP` 指向一个已构建的 `.app`，并需要
  `yi-agent-rs/target/debug/{yi-agent,plugin-pack}` 两个二进制先构建好。

## 复算方法

```
node scripts/plugins/e2e.mjs --suite all --output .artifacts/plugins/acceptance.json
node scripts/plugins/e2e-report.mjs .artifacts/plugins/acceptance.json
```

第一条写报告，第二条判它能不能算全绿。当前第二条会以非零退出并列出 6 条
非通过项——这是设计要的结果，不是缺陷。
