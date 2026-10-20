# yi-agent-flags

## 模块说明

功能开关（feature flags）的唯一判定点。让功能可以先合入 `main`、默认对用户不可见，
只有 `-dev` 构建能逐项打开。设计见
[2026-10-20-feature-flags-design.md](../superpowers/specs/2026-10-20-feature-flags-design.md)，
实现计划见 [2026-10-20-feature-flags-impl.md](../superpowers/plans/2026-10-20-feature-flags-impl.md)。

## 范围边界

**做什么：**
- `registry.rs`：全部 flag 的静态注册表（名字、说明、责任人、过期日、默认值）
- `store.rs`：`$YI_AGENT_HOME/flags.json` 的读写与取值链
- `error.rs`：未注册键、落盘失败

**不做什么：**
- 不做 CLI / RPC / UI 入口（分别在 `yi-agent`、`yi-agent-app-server`、`desktop`）
- 不做按用户/租户的开关分群，不做 A/B 实验度量

## Features

- [x] 注册表是唯一真源 — `src/registry.rs` 的 `REGISTRY`；未登记的键读时忽略、写时 `FlagError::UnknownFlag`。判据：`cargo test -p yi-agent-flags`（17 passed）含 `writing_an_unregistered_name_is_refused`、`unknown_keys_in_the_file_are_ignored`
- [x] 默认全关 — `registry.rs` 的 `every_flag_defaults_to_off` 钉住：`default: true` 的注册项会让测试失败（合并即改用户可见行为是不允许的）。判据：新增一个 `default: true` 的项后 `cargo test -p yi-agent-flags` 失败
- [x] 过期日强制 — 每项必填 `expires_on: "YYYY-MM-DD"`；`no_flag_is_past_its_expiry_date` 到期即失败，失败信息同时给出「删掉」与「续期」两条出路；`expiry_dates_are_iso_formatted` 钉住格式。判据：把任一 `expires_on` 改成过去日期后 `cargo test -p yi-agent-flags` 失败且信息含 flag 名
- [x] 取值链与来源可查 — `store.rs` 的 `status_at` 顺序为「构建类型硬闸 → 环境变量 `YI_AGENT_FLAG_<NAME>` → `flags.json` → 默认」，`Source::{BuildKind,Env,File,Default}` 说明值来自哪一层。判据：`cargo test -p yi-agent-flags` 的 `an_env_override_beats_the_file`、`a_dev_build_reads_the_file`、`the_build_kind_gate_wins_over_everything`
- [x] stable 硬闸无逃生口 — `status_at` 第一行 `if build_kind != "dev" { return (false, Source::BuildKind) }`，放在取值链内部而不是调用点（调用点分散一定会漏）。构建类型由 binary 启动时 `set_build_info()` 注入，未注入按 `release` 处理（fail-closed）。判据：`cargo test -p yi-agent-flags` 的 `an_uninjected_build_kind_is_fail_closed`、`a_nogit_build_is_treated_like_release`；端到端 `YI_AGENT_BUILD_KIND=release cargo build -p yi-agent && ./target/debug/yi-agent flags status x` 报 `source: build-kind`
- [x] 坏文件不阻断启动 — `flags.json` 读不出来或不是合法 JSON 时当作空表，下一次写入覆盖为合法结构。判据：`cargo test -p yi-agent-flags` 的 `a_corrupt_file_reads_as_empty_instead_of_failing_startup`
- [x] 写入是原子替换 — 先写 `flags.json.tmp` 再 `rename`，避免读者看到半截 JSON。判据：`store.rs` 的 `write_at`；`a_saved_value_round_trips` 覆盖读写往返
- [x] 首个真实 flag：`lead_agent` — `registry.rs` 的 `LEAD_AGENT_FLAG`，默认 `false`、`expires_on: "2027-01-31"`。判据：`cargo test -p yi-agent-flags` 的 `the_lead_agent_flag_is_registered`（18 passed）；端到端 `cargo test -p yi-agent-app-server --lib lead_tools`（3 passed）证明关掉时两类会话都没有组长工具
