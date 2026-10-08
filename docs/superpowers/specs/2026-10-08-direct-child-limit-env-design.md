# 直接子任务上限接入 env 与清理 TOML 死层 Design

日期：2026-10-08
状态：设计已确认（用户逐项拍板），待用户 review 本文件后进入 writing-plans。
范围：`yi-agent-runtime/src/config.rs`、`yi-agent-core/src/subagent/{worker,supervisor}.rs`、`yi-agent-subagent/src/{lib,attach}.rs`、`yi-agent-store/src/{schedule,runtime,ipc}.rs`、`yi-agent-store/Cargo.toml` 及其测试、`README.md`、`.env.example`、`docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md`。

## 1. 背景与问题

用户遇到 `error: daemon rejected spawn request: invalid_state: an agent may have at most four direct children`。追查后有两个独立问题：

1. **"four" 不可配。** 直接子任务上限是编译期常量 `MAX_DIRECT_CHILDREN: usize = 4`（`yi-agent-core/src/subagent/supervisor.rs:29`），唯一消费点是 `supervisor.rs:1269`。全仓库没有任何 env / CLI / 配置文件读取它。
2. **TOML 侧存在一个"看起来能用、实际没接线"的同义旋钮。** `yi-agent-store/src/schedule.rs` 的 `RuntimePolicyLayer` / `EffectiveRuntimePolicy` 收窄子系统由 commit `209c0be8`（2026-08-09）引入，定位是"先解析骨架"。其中 `max_direct_children_per_agent`（`schedule.rs:278` / `:317`）语义与 `MAX_DIRECT_CHILDREN` 完全重复，但**生产代码零引用**。

### 1.1 已核实的"死"范围

对 `schedule.rs` 逐个符号做全仓引用统计（排除 `src/schedule.rs` 自身与 `tests/scheduler.rs`）：

| 符号 | 生产引用 | 判定 |
|---|---|---|
| `RuntimePolicyLayer` | 0 | 死 |
| `RuntimeLimitsLayer` / `ResourceLimitsLayer` / `AttemptLimitsLayer` / `ScheduleDefaultsLayer` | 0 | 死 |
| `EffectiveRuntimePolicy` | 0 | 死 |
| `RuntimePolicyLayer::from_toml` / `effective_with` / `effective_schedule_with` / `default_schedule_policy_with` | 0 | 死 |
| 自由函数 `narrow` | 0 | 死 |
| `RuntimePolicy::narrowed_by` | 0 | 死 |
| `SchedulePolicy` / `RuntimePolicy` | **在用**（经字段访问，非按类型名）：`repository.rs:690` 持久化 `definition.policy`，`:703-704` 读 `policy.runtime.max_turns`/`max_wall_time_secs` | 保留 |
| `MissedRunPolicy` | 4（`runtime.rs:1479` 等） | 保留 |
| `WatchdogLimits` / `WatchdogUsage` / `WatchdogObservation` / `WatchdogOutcome` / `evaluate_watchdog` | 在用（`repository.rs`、`runtime.rs:2352`） | 保留 |
| `evaluate_retry` / `RetryFailure` / `RetryDecision` | 在用（`yi-agent-subagent/src/lib.rs:891`） | 保留 |

关键：`SchedulePolicy` / `RuntimePolicy` **不是死的**——`repository.rs:690` 持久化 `definition.policy`，`repository.rs:703-704` 从 `policy.runtime.max_turns` / `max_wall_time_secs` 派生 attempt 的 watchdog 限额。它们必须保留，且保留 serde 形状以避免持久化迁移。

### 1.2 这套 TOML 层的具体危害（删除理由）

- **零接线两个月**：`from_toml` 的调用者只有测试，没有 TOML 文件发现 / 加载 / 消费入口。
- **默认值互相矛盾**：`max_resident_subagents` 在收窄解析器里是 16（`schedule.rs:339`）、在 `SchedulePolicy::default()` 里是 4（`:585`）、真正生效的 factory/env 默认是 64（`worker.rs:657` / `config.rs:14`）。分层配置的全部意义是防止默认值失真，这套分层反而制造了失真。
- **两套并行配置系统**：活的 env/CLI 扁平标量 vs 死的 TOML 多层收窄，在**同一批旋钮**上重叠。
- **层次错位**：策略权威放在 `yi-agent-store`，执行点在 `yi-agent-core`（core 不能依赖 store），导致执行点只能自写常量 `4`。

## 2. 目标与非目标

**目标**

1. 直接子任务上限可由 env 定义：`YI_AGENT_MAX_DIRECT_CHILDREN`，默认 4，完全镜像 `YI_AGENT_MAX_RESIDENT_SUBAGENTS` 的既有链路。
2. 删除 TOML 侧未接线的收窄解析层及其同义旋钮，消除"同一概念两个默认值"的误导。
3. 错误文案携带真实上限，不再写死 "four"。
4. 只删"未被消费的代码"，不触碰任何被持久化或在执行路径上被读取的策略形状。

**非目标**

- 不做 TOML 文件发现 / 加载（不把死层"接线"，而是删除）。
- **不参数化 `max_depth`**：它是 `TaskDepth` 三态类型机（`task.rs:66-80`，`Root/Child/Leaf`），是类型级约束，不是数值上限。
- 不加 CLI flag `--max-direct-children`（`max_resident_subagents` 也只有 env，保持一致；需要时可后加）。
- 不动 `allow_coding` / `max_host_build_jobs` / `max_llm_requests_per_provider_key` 等其余未接线旋钮（超出本次范围）。
- 不改 `SchedulePolicy::default()` 的其余值语义。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 配置通道 | 只走 env：`YI_AGENT_MAX_DIRECT_CHILDREN`（默认 4），镜像 `YI_AGENT_MAX_RESIDENT_SUBAGENTS` |
| D2 | TOML 死层处置 | 整体删除 `RuntimePolicyLayer` / `EffectiveRuntimePolicy` / `narrow` / `narrowed_by` 及其测试 |
| D3 | 持久化形状 | 保留 `SchedulePolicy` / `RuntimePolicy`（含 serde）与 `SchedulePolicy::default()`，不做持久化迁移 |
| D4 | 默认值来源 | `MAX_DIRECT_CHILDREN`（core 常量）＝ `DIRECT_CHILDREN_DEFAULT`（runtime 常量）＝ 4，两处各自具名 |
| D5 | 传递路径 | `RuntimeConfig` → `AgentWorkerFactory::max_direct_children()` → `RuntimeCoordinator` → `AgentSupervisor` |
| D6 | 错误文案 | `DirectChildLimitReached { limit }` 携带真实上限，Display 动态打印 |
| D7 | `toml` 依赖 | 删除 `yi-agent-store` 的 `toml = "0.8"`（仅 `from_toml` 使用） |

## 4. 清理：删除 TOML 收窄解析层

### 4.1 从 `yi-agent-store/src/schedule.rs` 删除

- `RuntimePolicyLayer`（`:261-271`）
- `RuntimeLimitsLayer`（`:273-281`）、`ResourceLimitsLayer`（`:283-289`）、`AttemptLimitsLayer`（`:291-299`）、`ScheduleDefaultsLayer`（`:301-310`）
- `EffectiveRuntimePolicy`（`:312-330`）
- `impl RuntimePolicyLayer` 整块（`:332-519`，含 `from_toml` / `effective_with` / `effective_schedule_with` / `default_schedule_policy_with`）
- 自由函数 `narrow`（`:521-523`）
- `RuntimePolicy::narrowed_by`（`:534-548`）

### 4.2 `yi-agent-store/Cargo.toml`

删除 `toml = "0.8"`（`:23`）。已确认全仓库仅 `from_toml` 使用 `toml::`。

### 4.3 保留（附注释）

`SchedulePolicy` / `RuntimePolicy` 形状不变（serde 内嵌进 `definition.policy`，`repository.rs:690` 持久化）。给 `RuntimePolicy::max_resident_subagents` 加注释说明：它是 **scheduled root 的保守默认（=4）**，与 daemon 容量 `YI_AGENT_MAX_RESIDENT_SUBAGENTS`（默认 64）语义不同、互不影响——因为生产路径目前不读该字段，但它是被持久化的形状，删字段会带来旧 `schedule_policy` JSON 反序列化风险。

### 4.4 测试调整（`yi-agent-store/tests/scheduler.rs`，16 → 7）

删除 9 个纯收窄层测试：
`toml_policy_layers_can_only_narrow_the_user_ceiling`、`effective_policy_narrows_resource_and_retry_limits`、`coordination_reserve_is_clamped_to_the_effective_llm_total`、`effective_policy_only_narrows_numeric_limits_and_capabilities`、`explicit_schedule_selection_is_only_narrowed_by_project_and_global_limits`、`schedule_defaults_seed_a_conservative_policy_without_an_explicit_selection`、`schedule_policy_is_clamped_to_effective_global_runtime_limits`、`explicit_user_schedule_selection_can_exceed_user_schedule_defaults`、`schedule_selection_intersects_global_project_capabilities`。

保留 7 个（均覆盖活代码）：`schedule_definition_requires_exactly_five_cron_fields`、`schedule_definition_embeds_conservative_defaults`、`schedule_occurrence_claim_is_durable_and_idempotent`、`retry_policy_only_retries_explicit_transient_failures_with_bounded_backoff`、`watchdog_uses_persisted_progress_and_resource_queue_time`、`watchdog_classifies_turn_and_token_budgets_without_waiting_for_wall_clock`、`scheduled_policy_defaults_to_read_only_background_without_overlap_or_catch_up`。

同时删除 `tests/scheduler.rs:6` 里已失效的 import（`RuntimePolicyLayer`、`RuntimePolicy` 视保留项而定）。

## 5. env 接通（与 resident 完全同构）

| # | 文件 | 改动 |
|---|---|---|
| 1 | `yi-agent-runtime/src/config.rs` | 加 `pub const DIRECT_CHILDREN_DEFAULT: u16 = 4;`；`RuntimeConfig` 加 `pub max_direct_children: u16`；`load()` 在 `:235` 旁读 `YI_AGENT_MAX_DIRECT_CHILDREN`（解析失败/未设回退默认）；`redacted_view()`（`:359` 旁）加字段 |
| 2 | `yi-agent-core/src/subagent/worker.rs` | `AgentWorkerFactory` 加 `fn max_direct_children(&self) -> usize { crate::subagent::supervisor::MAX_DIRECT_CHILDREN }`（默认值注释说明它镜像 runtime 常量，core 不依赖 runtime） |
| 3 | `yi-agent-core/src/subagent/supervisor.rs` | `AgentSupervisor` 加字段 `max_direct_children: usize`；`new_with_objective` 默认置 `MAX_DIRECT_CHILDREN`；加 `pub fn with_max_direct_children(mut self, n: usize) -> Self`；`:1269` 判据改用 `self.max_direct_children`；`MAX_DIRECT_CHILDREN = 4` 保留为默认值 |
| 4 | `yi-agent-subagent/src/lib.rs` | `DaemonAgentWorkerFactory` 加字段、`with_max_direct_children` builder（镜像 `:131` `with_max_resident_subagents`）、trait 实现（镜像 `:471`） |
| 5 | `yi-agent-subagent/src/attach.rs` | `build_worker_factory`（`:104`）链上 `.with_max_direct_children(effective.max_direct_children)`。**这是必须项**：`2026-10-01-per-session-subagent-root` 计划 `:1251` 记录过同类事故——resident 版曾在 factory 漏转发，导致 env 静默失效 |
| 6 | `yi-agent-store/src/runtime.rs` | `RuntimeCoordinator`（`:194`）加 `max_direct_children: usize`；`open()`（`:750` 的 `Ok(Self { .. })`）从 `factory.max_direct_children()` 取；两处非测试 supervisor 构造点注入 `with_max_direct_children`：`create_session_with_objective_and_mode`（`:811`）、scheduled root（`:1503`） |

字段新增会波及全部 `RuntimeConfig` 字面量构造点（已清点）：`config.rs:332`（load）、`config.rs:377`（`sample_config`）、`models.rs:370`、`app-server/src/server.rs:8030` / `:8824` 附近、`subagent/tests/attach_delegation.rs:30`、`subagent/src/attach.rs:419` / `:533`。

## 6. 错误文案动态化

- `SpawnError::DirectChildLimitReached`（`supervisor.rs:43`）改为 `DirectChildLimitReached { limit: usize }`（枚举仍 `Copy`）。
- Display 改 `#[error("an agent may have at most {limit} direct children")]`。
- `supervisor.rs:1270` 返回 `SpawnError::DirectChildLimitReached { limit: self.max_direct_children }`。
- `yi-agent-store/src/ipc.rs:2913` 的 `ipc_error_message` 匹配改为带字段 `format!("an agent may have at most {limit} direct children")`，**消除现在重复硬编码的字符串**。

文案由 "four" 变为数字（默认 4 时输出 `an agent may have at most 4 direct children`），需同步更新断言：
`yi-agent-core/tests/subagent_supervisor.rs:278`、`yi-agent-store/tests/runtime_ipc.rs:1687`、`yi-agent-subagent/src/lib.rs:2482` 与 `:2487`。

## 7. 新增测试

1. **core**：`with_max_direct_children(2)` 时第 3 个直接子任务被拒，且错误携带 `limit == 2`（钉死"配置真的影响判据"）。
2. **runtime**：`YI_AGENT_MAX_DIRECT_CHILDREN=8` 时 `RuntimeConfig::load` 得到 8；未设时为 `DIRECT_CHILDREN_DEFAULT`（镜像 `max_resident_subagents_defaults_to_sixty_four`）。
3. **subagent**：配置值经 `build_worker_factory` 到达 factory（镜像 `the_configured_resident_capacity_reaches_the_worker_factory`）——这条专门防"漏转发"回归。
4. **store/ipc**：`DirectChildLimitReached { limit }` 经 IPC 序列化后 message 含真实数字。

## 8. 文档

- `README.md` 常用配置表加一行：`YI_AGENT_MAX_DIRECT_CHILDREN`（默认 4），与 `YI_AGENT_MAX_RESIDENT_SUBAGENTS` 并列说明二者语义差异（每会话直接子任务 vs 跨会话常驻总量）。
- `.env.example` 在 `YI_AGENT_MAX_RESIDENT_SUBAGENTS` 旁加一行。
- `docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md`：把"Configuration Schema And Effective Policy"小节（`:55-95`）里那段 TOML schema 与 `min(user, project, ...)` 描述**改写**为一句现状说明——"这些旋钮当前以 env 提供（`YI_AGENT_MAX_RESIDENT_SUBAGENTS` / `YI_AGENT_MAX_DIRECT_CHILDREN`）；多层 TOML 收窄（`RuntimePolicyLayer`/`EffectiveRuntimePolicy`）从未接线，已于 2026-10-08 移除"。选择改写而非删节，是因为该 spec 的其余部分（准入、watchdog、fair queue）仍是有效设计记录。

## 9. 验证

```bash
cd yi-agent-rs
cargo test -p yi-agent-core -p yi-agent-runtime -p yi-agent-subagent -p yi-agent-store -p yi-agent-app-server
cargo clippy --workspace --all-targets -- -D warnings
```

注意：本机存在既存的 socket 路径过长类失败，需与基线数量比对（改动前后一致即为通过）。

## 10. 风险

1. **漏转发导致 env 静默失效**（历史事故）。缓解：§5 第 5 步与 §7 第 3 条用例专门钉死 factory 转发。
2. **删层误伤持久化形状**。缓解：§4.3 保留 `SchedulePolicy`/`RuntimePolicy` 与 serde；`repository.rs` 相关测试（`runtime_coordinator.rs` 的 schedule 用例）作为回归网。
3. **文案变更破坏既有断言**。缓解：§6 列全了 3 个文件 4 处断言，一并更新。
