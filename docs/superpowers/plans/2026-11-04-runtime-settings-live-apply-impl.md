# Runtime Settings Live Apply Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 设置页保存运行参数后，同实例全部适用会话在下一个对应安全执行点采用，不重启、不打断或重放已开始的工作。

**Architecture:** runtime 提供按稳定设置路径共享的 LiveSettings，统一严格读取、原子提交、来源解析及可取消观察器。core 仅定义窄的模型配置/额度接口，在真正操作边界读取；各宿主、worker、恢复/重建路径持有同一动态绑定，协议通知只负责加速与失效提示。默认沙箱仅改变新会话默认值，历史权限以固化基线和当前显式安全限制求交。

**Tech Stack:** Rust 1.89+ / Tokio / serde_json / sha2 / libc flock；现有 Unix IPC、mock provider / wiremock / tempfile；React / TypeScript / Vitest / Testing Library。

## Global Constraints

- 唯一需求依据：[运行参数即时生效 spec](../specs/2026-11-04-runtime-settings-live-apply-design.md)，日期 2026-11-04，代码核查基线 `20a0a26c`。本计划是待执行文档，不表示代码或验收已完成。
- **保存后立即发布，所有消费者在下一个对应的安全执行点使用最新参数；不取消、不重跑已经开始的操作。** 等待首次准入不是已开始；同语义请求的传输重试保持原快照。
- 显式 override > 真实启动环境变量 > 当前 settings > 默认值；只保留真实存在的来源，不把初次合并结果或父会话普通默认值固化为 override。
- `startup.sandbox` 的策略是 `next_session`；其他 15 个字段为 `next_safe_point`。旧冷历史缺可信沙箱基线时按只读、不可提权恢复并告警；现有热任务不因保存降权/中断。
- 不改变 provider/凭据/模型选择、cwd/关联根、会话 ID、历史、取消句柄、父子关系、已消耗轮次/预算、旁问 lifecycle generation。正常配置更新不重建整个 Agent 或 provider。
- 同路径共享一个进程内配置源；配置路径装配时固化为绝对路径，不随 cwd/env 改变。不同实例绝对隔离。不增加全局配置 daemon、全体 ACK 注册表或另一份持久化设置。
- 完整 settings 文件上限 **1 MiB**；读取上限再探测一个字节；受管写锁单调时钟等待最多 **5 秒**；主动观察间隔至多 **1 秒**。本地提交直接唤醒，不等一整观察周期。
- 读取 refresh gate 覆盖重新 open→有界读→校验→发布缓存；不跨长操作持锁。写锁顺序：实例写锁→refresh gate。rename 是唯一提交点，权限和必要文件同步在它之前完成，提交后错误不能伪装成零提交。
- revision 是规范字段的内容标识，可 A→B→A，不可排序。不输出提示词、API key 或配置原文到新增错误/日志/通知；旧协议无 `live_apply_version: 1` 不承诺热更新。
- 优先复用现有依赖，不新增运行时依赖，不提高 `rust-version = "1.89"`；若实现不得不改依赖，必须另核真实 MSRV 并同步声明/门禁，不能悄悄变更本约束。
- 所有变更在隔离 worktree/非 main 分支完成。当前设计文档在 `.worktrees/runtime-settings-live-spec`、`docs/runtime-settings-live-spec`；开始实施前先确认这些未提交 spec/plan 的归属，不覆盖/删除。新实施会话可用已合入的文档创建 `feat/runtime-settings-live-apply` worktree；不能从 main 直接改代码。
- 每个任务先 RED 后 GREEN，测试不能只断言配置对象；每次提交前执行 `cd yi-agent-rs && cargo fmt --all && just ci`。下文 targeted 命令是迭代命令，不能替代提交前完整门禁；合并结果还需再跑一次。进度文档同提交维护，未完成整项仍用 `[ ]`，禁止 `[~]`。

---

## 0. 执行清单与任务依赖

- [ ] 确认 worktree/分支、文档基线和已有未提交文件；记录 `git status --short`、`git rev-parse HEAD`、可用磁盘。
- [ ] 使用 `test-driven-development` skill 实施每个任务；使用 `verification-before-completion` 验证每次成功声明。
- [ ] 顺序完成 Task 1–14，每项交付包含自己的测试闭环、文档记录和审阅。
- [ ] 按 §15 覆盖表验收，完成 spec/plan 自检、代码审阅、全量门禁和真实多宿主人工检查。
- [ ] 使用 `finishing-a-development-branch` 按项目规则合并和清理，不擅自发布/安装独立 app。

依赖图：`1 → 2 → 3` 是存储/来源基础；`4 → 5` 是 core 模型操作边界；`3 + 5 → 6 → 7` 完成提示词/各宿主；`3 + 4 + 6 → 8` 完成实际准入；`7 → 9` 完成历史基线；`2 + 3 → 10` 完成协议；`7 + 8 + 10 → 11` 完成主动观察；`10 → 12` 完成 UI；`7 + 8 + 11 → 13` 完成独立 executor/能力兼容；`1–13 → 14` 完成跨进程及反接线验收。

按 Task 编号串行执行最稳妥。Task 4 可以与 1–3 独立设计，但不要在共享工作区并发改同一文件。该功能是一个跨层一致性契约，不拆成相互脱节、各自宣称“全会话已支持”的子项目。

### 通用命令与提交规则

下面所有 Cargo 命令在实施 worktree 的 `yi-agent-rs/` 执行，npm 命令在 `desktop/` 执行。工具不在 PATH 时：

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
```

每项末尾的提交动作执行：

```bash
# 在实施 worktree 根；先确认暂存范围只包含本任务和配套进度文档。
(cd yi-agent-rs && cargo fmt --all && just ci) && git diff --check && git diff --stat
# 检查输出后，git add 本任务 Files 中的文件及实际更新的项目管理文档，再提交。
```

不使用 `git add .` 混入其他会话文件。每项的 commit message 在该任务末尾给出。文档只记录真实执行过的命令/结果；中间阶段不要将 runtime 的统一需求改成 `[x]`。大规模重编前先 `df -h .`，不在仍需编译时反复清 target，不并发跑多个全仓 CI。

## 文件与职责地图

下表的“新建”均为本计划规定的文件，不是现有 API。其余路径已按基线查验；现有大文件只做局部接线，不借机做全面重构。

| 文件/范围 | 职责 | Task |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`；新建同目录 `settings/{document,store}.rs` | FIELDS/兼容 facade；严格文档校验；锁/临时文件/原子提交 | 1–2 |
| 新建 `yi-agent-rs/crates/yi-agent-runtime/src/live_settings.rs`、`live_settings/{sources,observer,prompt,binding}.rs` | 共享快照/LKG/健康；固定来源；观察器；角色提示词；每会话绑定 | 3、6、11 |
| `yi-agent-rs/crates/yi-agent-runtime/src/{lib,config,bootstrap,migrate}.rs` | 导出、初始与动态同源解析、装配、迁移条件写 | 1–3、6–7 |
| 新建 `yi-agent-rs/crates/yi-agent-core/src/live_settings.rs`；`src/{lib,agent,compact}.rs` | 窄接口、Agent setter、THINK/压缩安全点 | 4–5 |
| 新建 `yi-agent-rs/crates/yi-agent-core/tests/{live_agent_settings,live_compact_settings}.rs`；`tests/common/live_settings.rs` | 真实 Agent 循环、准入屏障、重试/压缩请求录制测试 | 4–5 |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/{supervisor,worker}.rs`；`tests/{subagent_supervisor,subagent_scheduler}.rs` | 实际直接子准入、worker factory 提供动态额度 | 8 |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`；新建 `tests/live_admission.rs` | 实际 ResourceCoordinator 更新、排队唤醒、恢复准入 | 8、11 |
| `yi-agent-rs/crates/yi-agent-subagent/src/{lib,attach}.rs`、`executor/{factory,serve,protocol}.rs` | 现存 worker/缓存、executor 本地绑定和能力（不新增launcher） | 7、8、13 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/{runtime_binding.rs,driver/turn_driver.rs,transport/mod.rs}`；`server.rs` | 工厂/重建绑定、主动观察、通知广播 | 7、10–11 |
| `yi-agent-rs/crates/yi-agent-app-server/src/{thread_store,settings_rpc}.rs`；新建 `session_sandbox.rs`；`rpc/thread/lifecycle/{mod,resume,delete}.rs`、`server/turn/start.rs` | 基线持久化及恢复授权；settings 协议 | 9–10 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/{mod,assembly,thread_lifecycle,side}.rs`；新建 `runtime_settings.rs` | 实际 RPC/多会话/恢复/重建测试 | 7、9–10、14 |
| `yi-agent-rs/crates/yi-agent-runtime/src/side_conversation/{source,runner}.rs`；`tests.rs` | 动态绑定与 lifecycle generation 分离 | 7 |
| `yi-agent-rs/crates/yi-agent/src/{main,managed_runtime}.rs`、`tui/{title,slash}.rs`、`tui/notify/` | headless/TUI/内部 executor、通知/标题/诊断 | 7、11、13 |
| `yi-agent-rs/crates/yi-agent-web/src/api.rs` | 保留 web API 形状、接统一写入与健康读 | 2、10 |
| `yi-agent-rs/crates/yi-agent-store/src/ipc/{protocol,daemon}.rs` | 既有 daemon Status 能力扩展，旧值缺失不误判 | 13 |
| `desktop/src/lib/runtimeSettings.ts`；新建 `runtimeSettingsDrafts.ts`、`runtimeSettingsBus.ts` 及同名测试 | 精确协议解析；分字段编辑状态；按 RPC 身份路由失效提示 | 12 |
| `desktop/src/{App.tsx,hooks/useNotificationRouter.ts,components/SettingsDialog.tsx,components/SettingsRuntimeTab.tsx}` 及现有测试 | 设置页订阅/重连/文案/草稿并发 | 12 |
| 新建 `yi-agent-rs/crates/yi-agent-runtime/tests/live_settings_process.rs`、`yi-agent-rs/crates/yi-agent/tests/runtime_settings_live_e2e.rs` | 真双进程受管写入与无真实 LLM 的端到端 | 14 |
| `docs/project-management/{yi-agent-runtime,README}.md` 及被修改模块的已有模块文档 | 统一计数、任务证据和交叉引用 | 每项 |

## 接口契约总览

为避免跨任务臆造名称，以下是计划新增的精确接口。字段实现可为 private，但签名/序列化形状不能在下游偷偷分叉；需要调整时同步修订所有调用任务与测试。

### runtime：文档、快照和提交

```rust
// settings/document.rs
pub struct ValidatedSettingsDocument {
    pub raw: serde_json::Value, // 规范化已知旧键，保留未知键
    pub settings: crate::settings::Settings,
    pub revision: String,
}
pub fn validate_document(raw: serde_json::Value)
    -> Result<ValidatedSettingsDocument, crate::settings::SettingsError>;

// settings/store.rs
pub enum PatchMode { ReplaceProvided, FillMissing }
pub struct CommitReceipt {
    pub revision: String,
    pub changed_keys: Vec<String>,
}
pub fn commit_patch(path: &std::path::Path, patch: &serde_json::Value, mode: PatchMode)
    -> Result<CommitReceipt, crate::settings::SettingsError>;

// live_settings.rs；序列化 enum 采用 snake_case。
pub enum SnapshotOrigin { Current, LastKnownGood, StartupFallback }
pub enum HealthState { Healthy, Degraded }
pub struct SettingsHealth {
    pub state: HealthState,
    pub code: Option<String>,
    pub invalid_keys: Vec<String>,
}
pub struct SettingsSnapshot {
    pub settings: crate::settings::Settings,
    pub revision: String,
    pub origin: SnapshotOrigin,
    pub health: SettingsHealth,
}
pub struct LiveSettings { /* 唯一缓存、refresh gate、非阻塞 watch 唤醒 */ }
impl LiveSettings {
    pub fn shared(path: std::path::PathBuf) -> std::sync::Arc<Self>;
    pub fn refresh(&self) -> std::sync::Arc<SettingsSnapshot>;
    pub fn commit(&self, patch: &serde_json::Value, mode: PatchMode)
        -> Result<CommitReceipt, crate::settings::SettingsError>;
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64>;
}
```

这里注释是职责声明，不是要求照搬成空实现。`watch<u64>` 只是本进程失效序号，绝不暴露为持久化 revision，也不用于跨进程排序。`shared()` 固化路径，以 `Weak` 注册避免遗留实例；`refresh()` 对运行期 I/O/校验错误返回带降级状态的有效快照，不把它们传播成全局任务取消。

### runtime：固定来源和每会话适配器

```rust
// live_settings/sources.rs
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource { Override, Environment, Settings, Default }
#[derive(Clone)]
pub struct FrozenRuntimeValue {
    pub value: serde_json::Value,
    pub source: ValueSource,
}
#[derive(Clone, Default)]
pub struct FrozenRuntimeSources {
    values: std::collections::BTreeMap<String, FrozenRuntimeValue>,
}
pub struct EffectiveRuntimeSettings {
    pub values: std::collections::BTreeMap<String, serde_json::Value>,
    pub sources: std::collections::BTreeMap<String, ValueSource>,
}
impl FrozenRuntimeSources {
    pub fn capture(overrides: &crate::config::ConfigOverrides) -> anyhow::Result<Self>;
    pub fn validate(&self) -> anyhow::Result<()>;
    pub fn resolve(&self, settings: &crate::settings::Settings) -> EffectiveRuntimeSettings;
}

// live_settings/binding.rs
#[derive(Clone)]
pub struct RuntimeSettingsBinding {
    pub live: std::sync::Arc<crate::live_settings::LiveSettings>,
    pub sources: std::sync::Arc<FrozenRuntimeSources>,
}
pub struct AgentSettingsContext {
    pub model_context_length: u32,
    pub prompt: std::sync::Arc<crate::live_settings::prompt::LivePromptComposer>,
}
impl RuntimeSettingsBinding {
    pub fn agent_source(&self, context: AgentSettingsContext)
        -> std::sync::Arc<dyn yi_agent_core::live_settings::AgentSettingsSource>;
    pub fn admission_source(&self)
        -> std::sync::Arc<dyn yi_agent_core::live_settings::AdmissionLimitsSource>;
}
```

`FrozenRuntimeSources::default()` 是空覆盖表，供显式无环境测试/宿主使用；生产每个进程只capture一次，后续装配克隆binding。两个Frozen类型不提供serde接口，不通过JSON/argv/公开RPC传来源；手写Debug隐藏所有值。validate只接受适用的override/environment来源、已知键及既有来源解析结果，拒绝settings/default伪固定来源和模型/凭据键。不要把settings写入范围校验强加给真实CLI/env：旧来源能解析的零值/空值、非法数字忽略回落等语义按原config.rs保留，compact用户预算为零等原有报错仍报错；只有文件候选严格按FIELDS校验。capture在返回前validate；resolve只接收内部已验证构造的实例。

`values` 是 FIELDS 标量表：数字为 JSON number，布尔选项仍是字符串，system_prompt 未设置为 null；不是任意模型配置。`capture` 在初始装配且在设置合并前调用一次，生产装配把 binding 独立携带而非追加必填字段到所有 `RuntimeConfig` 测试字面量。初始 `RuntimeConfig::load_inner` 与 `resolve` 抽取同一套纯解析规则，不能各抄一套优先级。

### core：窄动态接口

```rust
// core/src/live_settings.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveAgentSnapshot {
    pub revision: String,
    pub max_turns: Option<u32>,
    pub compact_threshold: Option<u32>,
    pub compact_user_budget_tokens: usize,
    pub compact_tool_budget_tokens: usize,
    pub system_prompt: Option<String>,
}
#[derive(Clone, Debug, thiserror::Error)]
#[error("runtime settings refresh failed: {code}")]
pub struct LiveSettingsError { pub code: &'static str }
pub trait AgentSettingsSource: Send + Sync {
    fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionLimits { pub direct_children: u16, pub resident_subagents: u16 }
pub trait AdmissionLimitsSource: Send + Sync {
    fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError>;
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64>;
}
// 在 Agent impl 增加；未注入 source 的 Agent 保留静态配置行为。
pub fn set_live_settings(&mut self, source: std::sync::Arc<dyn AgentSettingsSource>);
pub fn live_settings_source(&self) -> Option<std::sync::Arc<dyn AgentSettingsSource>>;
// 在 AgentWorkerFactory trait 增加有默认值的方法，旧测试工厂无需逐个修改。
fn live_admission_limits(&self) -> Option<std::sync::Arc<dyn AdmissionLimitsSource>> { None }
```

`LiveSettingsError` 只用于派生失败/内部无法形成快照等本次操作错误，不携带提示词；文件坏掉由 runtime 的 LKG/health 消化。动态接口不暴露 provider 或 session，不能重建 provider。静态 Agent 通过把当前 AgentConfig 转成无动态 source 的操作快照复用同一循环分支，避免两份算法。

## Task 1: 建立严格文档校验和全字段采用策略

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs`、`yi-agent-rs/crates/yi-agent-web/src/api.rs`（仅适配错误枚举，保持可编译）
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/settings/document.rs`
- Test: 新文件内 `#[cfg(test)] mod tests`；保留 `settings.rs` 现有回归。

**Interfaces:** Consumes 现有 `Settings` / `FIELDS` / `SettingsError`；Produces `ValidatedSettingsDocument` / `validate_document`、`ApplyPolicy`、`apply_policy(key: &str) -> Option<ApplyPolicy>`。ApplyPolicy 的序列化仅有 `next_safe_point` / `next_session`。

- [ ] **Step 1: 写失败测试。** 使用真实 JSON，测试规范新键优先、非法已知字段整份拒绝、未知字段保留、同逻辑内容 revision 稳定。测试代码：

```rust
#[test]
fn live_document_rejects_bad_known_field_instead_of_defaulting() {
    let error = validate_document(serde_json::json!({
        "agent": {"max_turns": "not-a-number"},
        "unknown": {"future": true}
    })).err().expect("known invalid value must be rejected");
    assert!(error.to_string().contains("agent.max_turns"));
    assert!(!error.to_string().contains("not-a-number"));
}
#[test]
fn every_runtime_field_has_one_apply_policy() {
    assert_eq!(crate::settings::FIELDS.len(), 16);
    for field in crate::settings::FIELDS {
        let policy = apply_policy(field.key).expect(field.key);
        assert_eq!(policy == ApplyPolicy::NextSession, field.key == "startup.sandbox");
    }
}
```

同文件再添加 `revision_repeats_for_a_b_a` 和 `new_power_key_wins_and_unknown_keys_survive`：A=轮次200，B=201；验证 A→B→A 的 revision；两个防休眠键冲突时取 power，规范 raw 无旧 agent 键、未知键仍在。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-runtime --lib live_document`、`cargo test -p yi-agent-runtime --lib every_runtime_field_has_one_apply_policy`。预期缺接口编译失败；接入骨架后非法字段断言失败，不能把拼写/导入错误当成最终 RED 证据。
- [ ] **Step 3: 最小实现。** 从现有字段 validator 抽共享的纯值校验，遍历 FIELDS 在原始 JSON 上判断“未设置/合法/非法”；已知 group 非对象直接拒绝。数字字符串沿既有写入解析规则处理，不先宽容反序列化；合法数字字符串必须先转成规范number再构造Settings，不能只在revision表里规范化而让typed Settings丢值。对规范已知字段值/未设置状态构造排序 JSON，以现有 `sha2::Sha256` 算 revision；不把未知键、env 或完整 prompt 文本放入标识字符串。

```rust
let logical: std::collections::BTreeMap<_, _> = crate::settings::FIELDS
    .iter()
    .map(|field| (field.key, canonical_value(&normalized, field)))
    .collect();
let bytes = serde_json::to_vec(&logical)
    .map_err(|_| SettingsError::Corrupt("canonical serialization failed".into()))?;
let revision = format!("{:x}", sha2::Sha256::digest(bytes));
```

本步骤私有 `canonical_value(raw: &Value, field: &FieldSpec) -> Value` 只在完整字段校验成功后调用；未设置返回 null。为 serde_json 的错误显式映射 SettingsError，不靠 `?` 泄漏原始字段值；digest代码引入 `use sha2::Digest`。保留现有 SettingsError 三个变体，并新增 `InvalidDocument { keys: Vec<String> }`、`LockTimeout`、`TooLarge`，都不携带字段值。同步适配 settings_rpc/web 对该枚举的全部穷尽match，先返回受控错误code以保证Task1本身可构建，不拖到Task10才修编译；Task10再补齐精确健康/采用协议。migration现有 `other` 分支保留存储失败语义。

- [ ] **Step 4: GREEN。** Run `cargo test -p yi-agent-runtime --lib settings`，确认测试实际运行非零条，原 `.env` 解析及普通宽容启动读取测试不被误删。
- [ ] **Step 5: 文档/门禁/提交。** 更新 runtime 统一需求下的校验接口与测试命令证据，保持 `[ ]`；按通用提交规则，message `feat: add strict runtime settings document policies`。

## Task 2: 统一受管写入、条件迁移和提交语义

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/settings/store.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/{settings,migrate}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs`（切换统一入口并隔离阻塞等待，不改变现有响应形状）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/ui.rs`（受管写入改在spawn_blocking中执行）
- Test: `store.rs` 的 tests、`migrate.rs` 现有 tests。

**Interfaces:** Consumes Task 1 validator；Produces `PatchMode` / `CommitReceipt` / `commit_patch`。旧 `settings::write` / `write_to` 签名继续返回 `Result<(), SettingsError>`，内部调用 commit_patch 并丢弃 receipt；所有入口都纳入相同锁。Task 3 会加入提交事件桥接，不能再出现第二个原子写实现。

- [ ] **Step 1: 写失败测试。** 用临时目录而非改真实 HOME：

```rust
#[test]
fn clearing_new_power_key_cannot_resurrect_legacy_alias() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":"false"},"power":{"prevent_idle_sleep":"true"},"future":7}"#).unwrap();
    commit_patch(&path, &serde_json::json!({"power.prevent_idle_sleep": null}), PatchMode::ReplaceProvided).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(raw["power"].get("prevent_idle_sleep").is_none());
    assert!(raw["agent"].get("prevent_idle_sleep").is_none());
    assert_eq!(raw["future"], 7);
}
#[test]
fn fill_missing_rechecks_latest_file_under_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let migration_patch = serde_json::json!({"agent.max_turns": 50});
    commit_patch(&path, &serde_json::json!({"agent.max_turns": 77}), PatchMode::ReplaceProvided).unwrap();
    let receipt = commit_patch(&path, &migration_patch, PatchMode::FillMissing).unwrap();
    assert!(receipt.changed_keys.is_empty());
    assert_eq!(crate::settings::load_from(&path).agent.max_turns, Some(77));
}
```

额外按命名测试 `first_save_creates_private_parent_and_lock`、`candidate_over_limit_is_not_committed`、`repair_patch_must_repair_all_invalid_known_fields`、`writer_mutex_and_flock_share_one_deadline`（先消耗进程内等待，再竞争flock，不能各拿完整5秒）。在 store 内增加仅测试可用注入枚举 `CommitFault::{BeforeWrite, BeforePermissions, BeforeSync, BeforeRename, AfterRename}`，测试前四者原字节不变、最后一个不能伪装成未提交。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-runtime --lib clearing_new_power_key` 和 `cargo test -p yi-agent-runtime --lib fill_missing_rechecks`。先保留旧 write 路径跑到旧键复活/覆盖用户值断言失败，再切新入口。
- [ ] **Step 3: 最小实现。** 新父目录0700，已有目录不chmod；锁文件0600、固定inode。先取同路径进程内writer锁，再取 `libc::flock(fd, LOCK_EX | LOCK_NB)`；两次等待共用一个 `Instant` 的5秒deadline，进程内用try_lock/可唤醒等待而非无期限Mutex::lock，EWOULDBLOCK/EAGAIN重试、EINTR继续，其余errno返回受控I/O类别。RAII释放，不unlink锁。持锁后读取旧文件同样只读1MiB+1；仅要求顶层/已知group可用于patch的原始JSON，不在应用patch前严格拒绝坏字段，否则修复patch永远无法生效。缺文件用空对象；坏JSON/不可读/超限拒绝覆盖。

同步接口等待不得直接占用async reactor：本任务就将RPC/web写入口包在spawn_blocking中（Task10再换精确响应DTO），不能让Task2–9的可运行版本先引入5秒阻塞。

```rust
// 已持实例写锁，old 为最新原文；有效空值口径与 FIELDS 一致。
for (key, value) in patch_object {
    if matches!(mode, PatchMode::FillMissing) && canonical_key_is_present(&old, key) {
        continue;
    }
    apply_known_patch(&mut candidate, key, value)?;
}
let candidate = validate_document(candidate)?;
write_prepared_temp_then_rename(path, &candidate.raw)?;
```

私有 `canonical_key_is_present(raw: &Value, key: &str) -> bool` 判断规范字段及有效旧别名是否存在；存在非法值也返回 true，不允许迁移覆盖。`apply_known_patch(raw: &mut Value, key: &str, value: &Value) -> Result<(), SettingsError>` 删除空串/null以及已知别名，保留未知键。`write_prepared_temp_then_rename(path: &Path, raw: &Value) -> Result<(), SettingsError>` 有界序列化，唯一临时文件排他创建，0600/flush/sync 在 rename 前，前置故障仅删本次临时文件。

迁移把锁外筛选改为候选收集，逐个合法导入值以 FillMissing 提交，保留非法单值跳过和先 settings 后 `.env.migrated` 的顺序；存储失败禁止改名。锁外快照只可优化，不得决定最终是否覆盖。

- [ ] **Step 4: GREEN。** Run `cargo test -p yi-agent-runtime --lib settings`、`cargo test -p yi-agent-runtime --lib migrate`、`cargo test -p yi-agent-web`。并发真进程锁验证放 Task 14，但本任务先用两个独立 fd 测试锁超时/释放，不用 sleep 安排竞态。
- [ ] **Step 5: 文档/门禁/提交。** runtime/web 模块记录统一入口与回归，message `fix: serialize runtime settings commits and migrations`。

## Task 3: 建立 LiveSettings、健康状态和固定来源解析

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings.rs`
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings/sources.rs`
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings/binding.rs`（本任务只定义binding数据，Task6再实现agent/admission适配）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/{lib,config,settings}.rs`、`settings/store.rs`
- Test: live_settings.rs 与 sources.rs 内 tests。

**Interfaces:** Produces 总览中的 `LiveSettings` / `SettingsSnapshot` / `FrozenRuntimeSources` / `EffectiveRuntimeSettings` 及只有live/sources两字段的 RuntimeSettingsBinding。`LiveSettings::shared(path)` 使用稳定绝对路径和 Weak 注册表，测试可指定不同临时路径；初始缺文件是健康默认，坏文件是明确 fallback。新增 `RuntimeConfig::load_lenient_with_binding(overrides: &ConfigOverrides, settings_path: &Path) -> anyhow::Result<(RuntimeConfig, RuntimeSettingsBinding)>`，先capture真实来源，再refresh有界settings快照，再执行既有模型/其他装配。保留load_lenient原签名，经同一有界装配内部入口取config，不允许它先走无界旧load_from再补binding。

- [ ] **Step 1: 写失败测试。**

```rust
#[test]
fn live_refresh_retains_last_good_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let live = LiveSettings::shared(path.clone());
    live.commit(&serde_json::json!({"agent.max_turns": 17}), PatchMode::ReplaceProvided).unwrap();
    let good = live.refresh();
    std::fs::write(&path, b"{bad-json").unwrap();
    let failed = live.refresh();
    assert_eq!(failed.revision, good.revision);
    assert!(matches!(failed.origin, SnapshotOrigin::LastKnownGood));
    assert!(matches!(failed.health.state, HealthState::Degraded));
    std::fs::remove_file(&path).unwrap();
    let recovered = live.refresh();
    assert!(matches!(recovered.origin, SnapshotOrigin::Current));
    assert!(matches!(recovered.health.state, HealthState::Healthy));
    assert_eq!(recovered.settings.agent.max_turns, None);
}
```

再增加 `same_path_shares_source_and_other_path_isolated`（Arc::ptr_eq）、`old_open_cannot_overwrite_new_cache`、`same_bytes_keep_degraded_health_until_repaired`、`startup_oversize_never_calls_unbounded_loader`。旧open测试分两种时间线：
1. 同进程读者在gate内open后暂停，启动写线程并确认它正在等gate；先释放读屏障，再等待写入成功，最后发起新读取。不能一边暂停持gate的读者、一边同步等同进程commit完成，否则夹具本身死锁。
2. Task14用另一OS进程提交（无本进程gate），旧读暂停时允许新文件已提交；再启动本进程第二读者并确认其等gate，释放旧读后第二读者必须拿到新版本，缓存及失败LKG不倒退。用有deadline的屏障/通道，不靠sleep。
来源测试在sources子模块直接构造固定来源表，不使用并行set_var；以当前 `MAX_TURNS_DEFAULT` 常量作为default预期，逐级撤掉override/env/settings，不能把200硬编码成第二份默认。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-runtime --lib live_refresh`、`cargo test -p yi-agent-runtime --lib old_open_cannot`。用可控读取 hook 证明旧缓存发布缺门控时断言失败，不把空测试/只测 mutex 当证据。
- [ ] **Step 3: 最小实现。** 在 refresh gate 内重新 open，用 `Read::take((1024*1024+1) as u64)` 读取；严格候选成功才替换 LKG。失败复用最后成功 settings/revision，健康变更照样产生失效序号；启动 fallback 仅对已在上限内读取的 bytes 使用宽容规则，不重新无界 load_from。相同 bytes 缓存必须缓存校验/健康结果而非强制健康。

```rust
// 一次短刷新事务；派生提示词不在这个锁里。
let mut state = self.refresh_gate.lock().unwrap_or_else(|p| p.into_inner());
let input = read_bounded_input(&self.path);
let snapshot = state.accept_or_degrade(input);
let changed = state.changed_since_last_publish(&snapshot);
if changed { self.invalidate_nonblocking(); }
snapshot
```

本步骤私有读取层保留有界原文，而不是只返回严格验证结果：

```rust
enum BoundedSettingsInput { Missing, Bytes(Vec<u8>) }
fn read_bounded_input(path: &Path) -> Result<BoundedSettingsInput, SettingsError>;
fn accept_or_degrade(&mut self, input: Result<BoundedSettingsInput, SettingsError>)
    -> Arc<SettingsSnapshot>;
```

`accept_or_degrade` 在拥有bytes期间解析/严格校验；首次严格失败仍可用同一份已读bytes进行宽容启动归一化，读取错误/超限没有bytes则只回退默认。不能用 `Result<ValidatedSettingsDocument, SettingsError>` 作为唯一输入，否则非法字段时已经丢失启动回退所需原文。提取无日志的宽容纯函数，复用原解析语义但不调用会输出字段值的旧 `Settings::normalized` 日志分支。运行期已有成功快照后整份候选失败只走LKG。

`changed_since_last_publish(&mut self, next: &SettingsSnapshot) -> bool` 比较revision、origin与完整health；首次启动回退不是成功LKG，重复坏bytes仍保持startup_fallback，首次合法读取才建立LKG。commit 在实例写锁内进入同一个 gate；把 Task 2 的 store 分成已持锁内部提交与公共 facade，避免 commit 递归调用自己或重复取锁。旧 write_to 必须经注册表唤醒已存在 source，不能只有 LiveSettings::commit 能通知。所有入口对同一绝对路径使用同一Weak注册及gate，store内部不得持gate再请求实例写锁。

sources 仅从 ConfigOverrides/真实 env 抽适用键，复用 config.rs 现有每字段解析；模型相关键永不进入表。以同一解析器生成初始 RuntimeConfig 的运行部分；保留模型/权限装配原机制。

- [ ] **Step 4: GREEN。** Run `cargo test -p yi-agent-runtime --lib live_settings`、`cargo test -p yi-agent-runtime --lib config`。所有 16 个字段来源和删键回默认都要表驱动验证；同时证明不读/改 API key 和模型选择。
- [ ] **Step 5: 文档/门禁/提交。** message `feat: add shared live runtime settings snapshots`。

## Task 4: 给 core 增加窄接口并接入正常 THINK 安全点

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/live_settings.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/{lib,agent}.rs`
- Create: `yi-agent-rs/crates/yi-agent-core/tests/common/live_settings.rs`
- Create: `yi-agent-rs/crates/yi-agent-core/tests/live_agent_settings.rs`

**Interfaces:** Consumes 不依赖 runtime 的接口总览；Produces `Agent::set_live_settings`、run 内 source 的 Arc clone、正常 THINK 的完整操作快照。AgentConfig 保留现有字段，避免破坏静态测试/调用方。

- [ ] **Step 1: 写可复用录制 provider 与失败测试。** common 文件定义以下完整最小夹具；不同集成测试通过 `#[path = "common/live_settings.rs"] mod support;` 引用，不新增生产测试后门。

```rust
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use futures::StreamExt;
use yi_agent_core::{Provider, ProviderError, ProviderEvent, ProviderRequest};
pub struct RecordingProvider {
    pub requests: Arc<Mutex<Vec<ProviderRequest>>>,
    pub scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
}
#[async_trait]
impl Provider for RecordingProvider {
    async fn call_stream(&self, req: ProviderRequest)
        -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let events = self.scripts.lock().unwrap().pop_front().expect("script exhausted");
        Ok(futures::stream::iter(events).boxed())
    }
}
pub struct MutableSettings(pub Mutex<yi_agent_core::live_settings::LiveAgentSnapshot>);
impl yi_agent_core::live_settings::AgentSettingsSource for MutableSettings {
    fn snapshot(&self) -> Result<yi_agent_core::live_settings::LiveAgentSnapshot,
        yi_agent_core::live_settings::LiveSettingsError> {
        Ok(self.0.lock().unwrap().clone())
    }
}
```

在测试中构造两条 EndTurn script、静态 AgentConfig.prompt=`old`，动态 snapshot.prompt=`first`；run 后 drain stream；修改 MutableSettings 为 `second`，再次 run/drain；断言 provider.requests 的两个 system 分别 first/second，session_handle Arc 不变。用真实请求字段 `system` 断言，不检查外层 AgentConfig。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-core --test live_agent_settings`。先添加空 setter 使编译通过，确认请求仍 old 的行为断言失败，再实现接线。
- [ ] **Step 3: 最小实现。** Agent 增加 `Option<Arc<dyn AgentSettingsSource>>`；setter 仅设绑定，不更换 session/tools。run 将 source clone 到 spawned loop。每次首次 provider gate 成功后取 snapshot，再构造 req；重试循环放在同一语义快照内部。静态 Agent 的操作快照从本次 config 构建。错误转换成现有可恢复 AgentError 分支，确保 lease drop、历史配对不破坏。

```rust
// think_phase 的真实 ProviderRequest 构造移到首次 gate 成功之后；
// merge_static_and_live 是本任务新增的纯函数。
let operation = live_source.as_ref().map(|source| source.snapshot()).transpose();
let effective = match operation {
    Ok(value) => merge_static_and_live(config, value.as_ref()),
    Err(error) => {
        let _ = tx.send(AgentEvent::Error(AgentError::LiveSettings(error.code))).await;
        return ThinkOutcome::Stopped; // 当前作用域lease自动释放。
    }
};
let req = ProviderRequest {
    model: effective.model.clone(),
    system: effective.system_prompt.clone(),
    messages: if effective.preserve_thinking { messages.clone() }
        else { crate::thinking::strip_thinking_except_last_assistant(messages) },
    tools: tools.schemas(),
    params: effective.gen_params.clone(),
};
```

在现有AgentError新增受控 `LiveSettings(&'static str)` 分支，Display不得拼接设置值。保留现有think_phase的取消/gate错误选择；把首个lease带入重试循环的第一个attempt，之后重试重新acquire但复用req，避免首次准入两次获取。只新增私有 `merge_static_and_live(config: &AgentConfig, live: Option<&LiveAgentSnapshot>) -> AgentConfig`，仅覆盖五类适用字段，不改模型/gen_params/重试策略。core 不导入 runtime/文件路径。

- [ ] **Step 4: GREEN 和在途请求。** common 增加 `GateBarrier` 实现现有 ProviderTurnGate.acquire，内含 entered/release 两个 Semaphore，获得 lease 后 drop 可计数。安排 run→等待 entered→修改 source→release→断言首次 req 为新值。用 ToolUseStart/Delta/End + EndTurn 两段 script，在第一个工具内屏障暂停、修改 source 后释放；下一 THINK 新值，tool call/result id 一一配对，工具只调用一次。

Run `cargo test -p yi-agent-core --test live_agent_settings` 和 `cargo test -p yi-agent-core --test agent_tool_stream`。

- [ ] **Step 5: 文档/门禁/提交。** core/runtime 模块交叉引用统一需求，message `feat: refresh live settings at provider operation boundaries`。

## Task 5: 轮次、自动/手动压缩和同操作重试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/{agent,compact,live_settings,lib}.rs`（扩展trait并导出新wrapper）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/turn/command.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/tests/{live_agent_settings.rs,common/live_settings.rs}`
- Create: `yi-agent-rs/crates/yi-agent-core/tests/live_compact_settings.rs`

**Interfaces:** Consumes Task 4 source 与录制夹具；Produces 单一 `used: u32` 计数口径、带同一 LiveAgentSnapshot 的压缩操作。公开 compact_session 现有静态调用保持兼容；为外部手动入口新增如下窄接口，只应用压缩字段、不渲染正常提示词、不检查旧回合used，且使用独立lease：

```rust
// core/src/live_settings.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveCompactSnapshot {
    pub revision: String,
    pub compact_threshold: Option<u32>,
    pub user_budget_tokens: usize,
    pub tool_budget_tokens: usize,
}
// 在 AgentSettingsSource 增加此方法；无单独实现时从snapshot映射，runtime在Task6覆写以免目录派生。
fn compact_snapshot(&self) -> Result<LiveCompactSnapshot, LiveSettingsError> {
    let value = self.snapshot()?;
    Ok(LiveCompactSnapshot {
        revision: value.revision,
        compact_threshold: value.compact_threshold,
        user_budget_tokens: value.compact_user_budget_tokens,
        tool_budget_tokens: value.compact_tool_budget_tokens,
    })
}
// core/src/compact.rs
pub async fn compact_session_with_live_settings(
    provider: &std::sync::Arc<dyn crate::Provider>,
    config: &crate::AgentConfig,
    session: &crate::Session,
    source: Option<&dyn crate::live_settings::AgentSettingsSource>,
    gate: Option<&dyn crate::ProviderTurnGate>,
) -> Result<Option<crate::Session>, crate::AgentError>;
```

先等待gate、再取compact_snapshot并覆盖本次预算，调用既有摘要协议，取消由现有宿主control保持；错误释放lease。这里沿用真实compact_session的AgentError返回类型：来源失败映射Task4的AgentError::LiveSettings，准入失败映射既有AgentError::ProviderTurnAdmission，provider失败保留原分支；不要误改仅用于压缩历史规划的CompactError枚举。主循环自动路径内部复用同一预算映射但自行重查触发/used，不双重取gate。app-server `server/turn/command.rs` 和 CLI `main.rs` 的 DriverControl::Compact 改为此新入口，不能只修改 core 自动路径。Task7会给这些宿主注入source，在Task5已有静态None回归必须保持。

- [ ] **Step 1: 写失败测试。** 给 RecordingProvider 支持每次调用前/返回流前屏障，以及一段可返回 ProviderError 的脚本；脚本类型扩为 `VecDeque<Result<Vec<ProviderEvent>, ProviderError>>` 并更新 Task 4 夹具初始化，不引入第二个 provider。构造 max=1 的 ToolUse 第一次请求：工具完成、第二次 THINK 不发；保存 source.max_turns=3 后在仍活跃的工具屏障内释放，允许后续请求。对反向降额使用 `used + 1 > latest_limit`，已结束后增额不自动 run。

```rust
// 在同一用户回合的工具屏障内变更，而非调用第二次 run 代替。
source.0.lock().unwrap().max_turns = Some(1);
release_tool.add_permits(1);
while let Some(event) = stream.next().await {
    if let AgentEvent::Done { reason } = event {
        assert!(matches!(reason, DoneReason::MaxTurns));
    }
}
assert_eq!(requests.lock().unwrap().len(), 1);
```

实现时匹配真实 `AgentEvent::Done { reason }`（当前仅有reason字段）；若实施期间结构新增字段则再用 `..`。压缩测试设 Session.last_input_tokens 越过阈值，摘要 script 之后正常 THINK script；断言摘要 system=None，预算取同快照，正常 THINK 再读新的 system。单独覆盖手动压缩不继承旧回合已终止 used。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-core --test live_agent_settings`、`cargo test -p yi-agent-core --test live_compact_settings`。旧行为应在长回合降额/等待压缩采用新预算/准入后新值处失败。
- [ ] **Step 3: 最小实现。** 将 max 检查移到无必要自动压缩前；used 只在正常 THINK 真正开始时增一次，采用 u64 中间数或 checked_add 比较以免溢出。等待压缩/首次模型准入结束后重新刷新并比较同一下一轮，不重复加计数。

```rust
let next = u64::from(used) + 1;
if snapshot.max_turns.is_some_and(|limit| next > u64::from(limit)) {
    return finish_max_turns_without_new_provider_call();
}
```

`finish_max_turns_without_new_provider_call` 表示复用现有 DoneReason::MaxTurns 结束分支，不另设新任务重启行为。自动摘要等待后同时重查 ratio/预算/轮次，若不再需压缩直接释放 lease；已开始摘要按旧预算完成。压缩专用摘要 system=None；正常 prompt 不注入摘要。首次请求构造后的新长等待结束必须重构尚未发请求；已有同操作网络/429/5xx重试不换快照、不重计轮，保留原有“哪些错误可重试”分类。

- [ ] **Step 4: GREEN。** Run 两个 live 集成测试和 `cargo test -p yi-agent-core --lib compact`、`cargo test -p yi-agent-core --lib retry`。给 retry 用虚拟时间，不真 sleep：重试退避中修改 source，重试 req 与第一次逐字节相同；下一语义操作才更新。测试取消/MaxTurns/派生失败时 lease 归还，缺此断言不算完成。
- [ ] **Step 5: 文档/门禁/提交。** message `fix: apply live limits and compaction without replaying work`。

## Task 6: 统一动态提示词、技能预算和每会话 adapter

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings/prompt.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings/binding.rs`（Task3已建）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/{live_settings,bootstrap,config}.rs`
- Test: 新模块 tests、bootstrap.rs 现有提示词测试。

**Interfaces:** Consumes Tasks 3–5；Produces 总览 RuntimeSettingsBinding/AgentSettingsContext，以及：

```rust
pub enum PromptRole { Normal, Side, Naked }
pub enum InitialCatalogPolicy {
    Bounded,
    IncludeAll { budget: usize, source: ValueSource },
}
pub struct LivePromptComposer { /* 稳定项目/日期上下文、SkillsService、缓存 */ }
impl LivePromptComposer {
    pub fn new(cfg: &crate::config::RuntimeConfig, role: PromptRole,
        skills: Option<std::sync::Arc<yi_agent_skills::SkillsService>>,
        initial_policy: InitialCatalogPolicy)
        -> Result<Self, yi_agent_core::live_settings::LiveSettingsError>;
    pub fn render(&self, user: Option<&str>, budget: usize, source: ValueSource)
        -> Result<Option<String>, yi_agent_core::live_settings::LiveSettingsError>;
}
```

构造composer复用现有resolve_system_prompt的稳定段和SkillsService，不再触发install_system_skills。IncludeAll必须在用户作出原交互选择时记录当时的有效budget+source，不能只传bool再在首次render时绑定：用户选择后、首次请求前可能已保存新预算。改变数值或来源后永久失效为Bounded，本会话后来改回旧数值也不自动恢复。Side/Naked不继承Normal的IncludeAll选择。adapter解析同一settings快照并按真实模型上下文形成core snapshot；覆写Task5 compact_snapshot，只解析压缩参数、不调用render/目录扫描。

正常snapshot若有目录等待，先保留其输入指纹（用户段/有效预算及来源/策略），完成派生后重新refresh；指纹变了就丢弃未发出的派生结果、按新值重组，返回时的max/压缩参数也取最终同一快照。未变可复用目录片段。不能在async等待或可能长阻塞的目录刷新之前取一次快照后直接发请求；重试原请求路径则不再次调用source。使用可控目录屏障覆盖这一点。

- [ ] **Step 1: 写失败测试。** 在 prompt.rs 子模块中直接构造 composer 和临时 SkillsService；沿用 bootstrap 测试现有 catalog 夹具写一个 skill。验证默认→用户→AGENTS→日期→目录→旁问策略的相对 index；清空用户段只移除该段；Naked 返回 None。按 budget/source 序列 `(8192, Default) → 同预算/同来源+无关字段 → (8192, Settings) → (4096, Settings)` 验证 IncludeAll 保留/失效且不再弹交互。

```rust
let first = composer.render(Some("USER-A"), 8192, ValueSource::Settings).unwrap().unwrap();
let second = composer.render(Some("USER-B"), 8192, ValueSource::Settings).unwrap().unwrap();
assert!(second.contains("USER-B"));
assert!(!second.contains("USER-A"));
assert_eq!(second.matches("<available_skills>").count(), first.matches("<available_skills>").count());
```

目录标记以当前 SkillsService 的实际格式为准读取测试夹具；若其格式不是该标记，使用真实完整目录片段 `render_catalog` 作一次匹配，不能因标记不符使两边都 0 而假绿。

额外两个必须创建的屏障用例：`include_all_binds_at_selection_not_first_request` 在选择时记录8192/Default，首次请求前保存8192/Settings，断言已经失效；随后改回Default也不恢复。`catalog_wait_rechecks_unstarted_request` 让测试目录扫描停在屏障，保存新用户段/预算/轮次，再释放扫描；断言首次实际provider请求及轮次检查均取最终一致快照，而不是只检查composer内部字符串。屏障仅测试注入，不改变同步source trait或增加生产RPC。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-runtime --lib live_settings::prompt`，旧 SkillsCatalogHandle 固定 base/budget 应无法满足 USER-B/新预算断言。
- [ ] **Step 3: 最小实现。** SkillsCatalogHandle改持同一composer/binding，current_system_prompt与core adapter进入同一render，不再存第二份旧base。缓存按用户段、预算/来源/IncludeAll策略、目录版本、角色和稳定上下文区分；读取快照后释放gate再扫描/渲染目录，最后按上文重新确认快照，不能让这条旧的一次读取描述覆盖最终重查要求。现有正常目录扫描故障的已定义非致命行为保持一致；新派生内部错误返回受控LiveSettingsError，不偷偷用前一用户段。

```rust
let threshold = (u64::from(context.model_context_length) * u64::from(ratio) / 100)
    .min(u64::from(u32::MAX)) as u32;
```

上式沿用整数舍入，`model_context_length` 未声明时复用 config 的 200_000 fallback；ratio/默认常量仅来自同一 resolver。adapter 不把 RuntimeConfig 初次 max_turns 视作固定来源。编写 `same_snapshot_uses_session_model_context`，两个 context=100000/200000、ratio=50 得50000/100000。

- [ ] **Step 4: GREEN。** Run `cargo test -p yi-agent-runtime --lib bootstrap`、`cargo test -p yi-agent-runtime --lib live_settings`；验证同内容 prompt 字节稳定，已有 SkillsService 句柄复用，没有工具/MCP 重装计数增加。
- [ ] **Step 5: 文档/门禁/提交。** message `feat: compose prompts from live settings and stable context`。

## Task 7: 全宿主、现存 worker、旁问和重建路径接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`、`side_conversation/{source,runner}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/{lib,attach}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/{runtime_binding.rs,driver/turn_driver.rs,turn/command.rs}`、`server.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/{main,managed_runtime}.rs`
- Test: app-server `server/tests/{assembly,side}.rs`；runtime `side_conversation/tests.rs`；subagent lib.rs 现有 tests。

**Interfaces:** Consumes RuntimeSettingsBinding；Produces 新 `bootstrap_agent_with_binding(cfg: &RuntimeConfig, mode: PermissionMode, binding: RuntimeSettingsBinding) -> Result<AgentBootstrap>`，旧bootstrap_agent facade保留静态测试兼容；AgentBootstrap/BuiltAgent保存binding及composer的Arc，所有Agent::new重建显式重新set_live_settings。生产worker工厂入口新增明确签名：

```rust
// yi-agent-subagent/src/attach.rs
pub fn worker_factory_with_binding(
    cfg: &RuntimeConfig,
    runtime_socket: std::path::PathBuf,
    models_path: &std::path::Path,
    binding: Option<yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
) -> anyhow::Result<std::sync::Arc<dyn yi_agent_core::subagent::worker::AgentWorkerFactory>>;
```

**实现记录（兼容决策）**：binding 取 `Option` 而不是上面的非可选形态。理由是该函数是所有 attach 路径的唯一出口，而历史静态调用方（测试、TUI、无设置来源的宿主）必须继续得到 `None` 的逐字不变行为；可选参数把它们与热更新宿主统一在一个入口，不必复制两份装配逻辑。`Some` 与 `None` 的行为差由 `the_attach_factory_keeps_the_live_settings_binding` 钉住（`None` 不装来源、`Some` 装上同一份共享实例）。

现有attach_project_runtime及其with_environment内部装配共同接收调用宿主已有binding（可新增同名 `_with_binding` facade，原静态facade只用于旧测试），不要在每次attach/每模型刷新时重新capture。`main::run_internal_daemon` / `managed_runtime::run_managed_daemon` 也把入口binding传给此工厂；executor入口由Task13单独处理。

- [x] **Step 1: 写失败测试。** 在 assembly.rs 复用已有 mock工厂，增加 `rebuilt_agent_keeps_live_settings_binding`：建立会话→保存A→执行→经现有 MCP/模式/模型合法重建之一→保存B→再执行；录制请求依次A/B，session/provider身份只在原机制要求时变化。subagent 用当前 WorkerRefresh 夹具保持模型 EffectiveKey 不变，运行一段工具循环，在途保存后下一 THINK 新值；证明不是只更新未来 worker。

旁问测试在 runtime 的既有 runner 夹具上增加：

```rust
// 对同一已创建旁问，提交全局普通配置，不调用 SourceUpdate::commit。
let before_generation = source.generation;
live.commit(&serde_json::json!({"agent.max_turns": 9}), PatchMode::ReplaceProvided).unwrap();
assert_eq!(source.generation, before_generation);
// 继续运行当前 runner；断言真正请求/剩余额度，且父 session 消息数未变化。
```

generation 的访问在现有测试模块进行，不新增用户可写 lifecycle API。旁问正常参数更新取最新 binding；固定 ManagedRunLimits 及已消耗统计不变。

- [x] **Step 2: RED。** Run `cargo test -p yi-agent-app-server --lib rebuilt_agent_keeps_live_settings_binding`、`cargo test -p yi-agent-runtime --lib side_conversation`、`cargo test -p yi-agent-subagent --lib live_settings`。
- [x] **Step 3: 最小接线。** 进程入口在 cwd切换前解析稳定settings路径并调用Task3的load_lenient_with_binding；不要先load_config再capture来源。factory 捕获 binding，不捕获固定运行值。每会话装配用已解析模型上下文创建 adapter；非运行字段仍来自现有 thread_cfg。沿 `Agent::new` 搜索本 crate 的重建点逐个保留 binding、session、权限/checker、gate、process_manager。外层旧 set_system_prompt 改走 Task6 的统一 composer或删除重复覆盖。

```rust
let mut agent = Agent::new(provider, tools, agent_config);
agent.set_live_settings(binding.agent_source(AgentSettingsContext {
    model_context_length: effective_context_length,
    prompt: composer.clone(),
}));
```

为旁问 EffectiveSideSource 增加可克隆 binding/role context，但设置变化不调用 begin_update、不改 lifecycle revision/generation；model/权限的原有生命周期变化仍保留原行为。Naked 仅挂无提示词 adapter，不注册工具。手动压缩入口从 `agent.live_settings_source()` 克隆绑定并传给Task5新wrapper；Task5同步提供只读 `Agent::provider_turn_gate(&self) -> Option<Arc<dyn ProviderTurnGate>>`，必须把原Agent实际gate一起传入，不能用None绕过已有准入。app-server command函数增加source/gate入参（若已有agent所有权则直接取getter），CLI DriverControl::Compact同样传入；测试结束旧用户回合后保存新预算、阻塞摘要准入再保存另一预算，释放后必须在真实摘要请求/保留历史中观察最终新值。独立executor的本地来源绑定由Task13完成，本任务不宣称它已覆盖。

- [x] **Step 4: GREEN。** Run 上述 targeted命令及 `cargo test -p yi-agent-app-server --lib assembly`、`cargo test -p yi-agent-subagent --lib`、`cargo test -p yi-agent --bin yi-agent`。检查 CLI/headless/TUI三入口的真实构造路径，而非只测 bootstrap。
- [x] **Step 5: 文档/门禁/提交。** runtime/app-server/subagent/CLI/旁问模块加交叉引用，message `feat: retain live settings across hosts and agent rebuilds`。

## Task 8: 接入真实 supervisor 和 resident 准入

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/live_settings.rs`、`subagent/{supervisor,worker}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/{lib,attach}.rs`、`executor/factory.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings/binding.rs`
- Test: core `tests/{subagent_supervisor,subagent_scheduler}.rs`；Create store `tests/live_admission.rs`。

**Interfaces:** Produces core AdmissionLimitsSource/AdmissionLimits、AgentWorkerFactory.live_admission_limits的真实消费者（trait签名由Task4先建立，binding适配由Task6实现，本任务不重复定义）；AgentSupervisor 新增 `set_max_direct_children(&mut self, limit: usize)`；store RuntimeCoordinator 新增 `pub async fn refresh_live_admission(&self) -> Result<(), RuntimeCoordinatorError>` 及 `live_admission_receiver(&self) -> Option<watch::Receiver<u64>>`。source挂在factory，store不依赖runtime。supervisor按session分组且使用异步锁；主动刷新先克隆所需session/handle列表并释放映射锁，再逐个await取得supervisor并更新，不跨await持std MutexGuard，也不同时锁多个supervisor。现存start_worker等async准入路径在原保护区内同步读取短source，避免两次取得同一supervisor锁。测试调用refresh时使用.await，daemon同步循环用已有runtime.block_on驱动。

- [x] **Step 1: 写失败测试。** 在现有 store 测试工厂包装动态 source，容量初始2、已有2 worker、降到1；断言现存未取消、第三个排队。升到3并调用真实刷新调度入口，断言原排队 worker被启动。直接子上限以父 supervisor 的真实 nonterminal孩子计数测降/升，并发 spawn 以屏障同时提交，不能超发。

```rust
// 测试 source 的最小实现，watch只是唤醒。
struct LimitsSource {
    value: std::sync::Mutex<AdmissionLimits>,
    changed: tokio::sync::watch::Sender<u64>,
}
impl AdmissionLimitsSource for LimitsSource {
    fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError> {
        Ok(*self.value.lock().unwrap())
    }
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> { self.changed.subscribe() }
}
```

复用 store 现有 runtime 测试的 workspace/task/StopWitness 工厂，不新增替代调度器。对恢复/重新接纳也用同一 source。

- [x] **Step 2: RED。** Run `cargo test -p yi-agent-store --test live_admission`；`cargo test -p yi-agent-core --test subagent_supervisor`。旧“构造时 with_max_direct_children / with_resident_capacity”不刷新时必须失败。
- [x] **Step 3: 最小实现。** 真正创建/接纳前在现有父/准入受保护区域中 snapshot：更新 supervisor 限额并检查父状态/计数，resident在 actual resource lock内按最新source set_capacity 后 reserve/grant。禁止只改 factory返回常数；根不占resident额度，已有占用不减计数，降额采用saturating可用数。错误不得先造worker后再失败。

源码在实际grant处已持supervisor再取resource_coordinator锁；新路径沿用该方向，不得在refresh中反向持resource等待supervisor。配置source只读短refresh gate，不能回调调度器或在其锁内发送阻塞通知。`refresh_live_admission`按相同锁顺序更新，或分别更新并在真正grant点重新snapshot，不依赖一次预读永久生效。新增受控 `RuntimeCoordinatorError::LiveSettings { code: &'static str }`，适配所有穷尽匹配；派生失败阻止本次新准入而不取消现存worker，文件故障仍由LKG承接。

`refresh_live_admission` 读取一次limits，更新实际 coordinator并唤醒现有调度循环；待授予任务在grant点再读取，保存期间已经预留的lease视为已开始，不撤销。watch驱动由Task11安装，Task8直接调用刷新入口证明实际队列可重试。保持现有fairness/deadline/cursor持久化与取消释放口径。

- [x] **Step 4: GREEN。** Run `cargo test -p yi-agent-store --test live_admission`、`cargo test -p yi-agent-core --test subagent_scheduler`、`cargo test -p yi-agent-core --test subagent_supervisor`、`cargo test -p yi-agent-store --lib runtime`；同时测试 ProcessWorkerFactory 使用 daemon本地source，并不要求控制通道把trait对象序列化。
- [x] **Step 5: 文档/门禁/提交。** store/core/subagent模块记录真实准入证据，message `feat: apply live subagent limits at actual admission`。

## Task 9: 固化默认沙箱基线和安全历史恢复

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/session_sandbox.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{lib,thread_store,server}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread/lifecycle/{mod,resume,delete}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/{runtime_binding.rs,turn/start.rs}`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs`
- Test: session_sandbox.rs、thread_store.rs、`server/tests/thread_lifecycle.rs`。

**Interfaces:** ThreadMeta 的字段缺失与显式null必须区分；增加：

```rust
#[serde(default, deserialize_with = "deserialize_present_baseline",
        skip_serializing_if = "Option::is_none")]
pub sandbox_baseline: Option<SandboxBaseline>;

fn deserialize_present_baseline<'de, D>(de: D) -> Result<Option<SandboxBaseline>, D::Error>
where D: serde::Deserializer<'de> {
    // 缺字段由default产生None；字段存在则必须是合法对象，null同样报错。
    SandboxBaseline::deserialize(de).map(Some)
}
```

序列化固定 `{mode, promotable}`。SandboxBaseline 定义在 app-server session_sandbox，mode只允许spec三值；反序列化复用受控constructor归一read-only/false，不能直接derive后绕过该不变量。工具控制器增加只读 `baseline(&self) -> (SandboxMode, bool)`，不增加无授权 setter。新函数 `resolve_resume_baseline(saved: Option<SandboxBaseline>, trusted_live: Option<SandboxBaseline>, explicit_host: Option<SandboxBaseline>) -> ResolvedSandboxBaseline`；优先可信热控制器基线，冷恢复取saved，两者都没有才保守回退，最后与显式宿主限制求交。告警信息另携带受控来源类别（CLI/Environment/Parent），不携带设置值。

ThreadStore 增加 `load_for_activation(&self, id: &str) -> io::Result<Option<LoadedThread>>` 与 `read_meta_for_activation(&self, id: &str) -> io::Result<Option<ThreadMeta>>`：激活读中，整个meta不存在可按旧历史兼容；文件存在但I/O、JSON或基线形状错误一律Err。与既有load共享读取实现、对本次读取的一份meta作决定，不能先严格预读再调用会重新读取/吞错的load。列举/搜索的既有宽容API可以保留，但resume/fork/重建不得通过它们把非法元数据变成缺失。

- [x] **Step 1: 写失败测试。** session_sandbox.rs 内表驱动：saved workspace/promotable + explicit read-only→read-only/false；saved read-only + explicit full→仍read-only/false；无saved/无trusted→只读回退告警。ThreadMeta旧json缺字段可读；对null、非法mode、错误类型、坏JSON和存在但不可读meta，通过真实load_for_activation/resume/fork入口断言拒绝激活，不只单测serde。合法缺字段冷历史可保守激活，可信热controller优先且落盘失败仍保有内存基线。新增字段后适配所有ThreadMeta构造字面量（含其他测试模块），不能等Task14才补编译。

```rust
#[test]
fn wider_host_does_not_expand_saved_read_only() {
    let saved = SandboxBaseline::new("read-only", false).unwrap();
    let host = SandboxBaseline::new("danger-full-access", false).unwrap();
    let result = resolve_resume_baseline(Some(saved.clone()), None, Some(host));
    assert_eq!(result.baseline, saved);
}
```

定义 `SandboxBaseline::new(mode: &str, promotable: bool) -> Result<Self, String>` 为受控解析接口，错误仅模式类别不回显输入；read-only自动归一false。ResolvedSandboxBaseline 的 `baseline` 为公开内部字段，`warning: Option<SandboxBaselineWarning>`；warning枚举 `LegacyReadOnlyFallback` / `HostRestrictionApplied`。

- [x] **Step 2: RED。** Run `cargo test -p yi-agent-app-server --lib session_sandbox`、`cargo test -p yi-agent-app-server --lib sandbox_baseline`。新增RPC测试保存startup.sandbox后resume旧session不能套新默认，先观察旧代码变宽的失败。
- [x] **Step 3: 最小实现。** 新建会话用Task3当前默认+真实来源装配，激活工具/委派之前持久化实际base/promotable；不得取controller.effective的瞬时Yolo值。恢复/fork/合法重建把基线通过现有逐会话工厂上下文传入，不把某个会话写进共享cfg；临时上下文用RAII在正常/错误/panic展开时恢复，避免TLS泄漏。

老热会话从可信controller读取并保存，失败仅告警、继续保有内存基线；冷旧历史无证据时只读不可提权、告警再固化。对当前显式CLI/env硬限制求更严格模式、可提权取AND；根规则复用原授权，不合并陌生根。元数据非法拒绝激活；基线持久化新会话失败不启动工具/委派。不要在设置保存时批量恢复会话/扫描历史。

- [x] **Step 4: GREEN。** Run `cargo test -p yi-agent-tools --lib sandbox`、`cargo test -p yi-agent-app-server --lib sandbox_baseline`、`cargo test -p yi-agent-app-server --lib thread_lifecycle`。额外故障注入：新基线写失败无激活，热基线写失败不取消现存运行；normal/yolo双向切换遵循固化promotable。
- [x] **Step 5: 文档/门禁/提交。** permission/app-server模块交叉引用明确兼容例外，message `fix: preserve session sandbox baselines across live defaults`。

## Task 10: 精确 settings 协议、错误分类和诊断

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{settings_rpc,server}.rs`、`rpc/ui.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/runtime_settings.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/mod.rs`

**Interfaces:** `settings_rpc::read_live_value(binding: &RuntimeSettingsBinding) -> Value`、`write_live_value(binding: &RuntimeSettingsBinding, patch: &Value) -> Result<Value, SettingsError>`；RPC生产路径调用新接口，旧path函数可保留测试facade但不得成为漏通知写入口。JSON形状完全按spec§8.1；settings/changed `{revision,changed_keys}`。host值只表示宿主默认装配上下文。

- [ ] **Step 1: 写失败测试。** settings_rpc.rs新增：

```rust
let view = read_live_value(&binding);
assert_eq!(view["live_apply_version"], 1);
assert_eq!(view["snapshot_origin"], "current");
assert_eq!(view["health"]["state"], "healthy");
let saved = write_live_value(&binding, &serde_json::json!({"agent.max_turns": 41})).unwrap();
assert_eq!(saved["changed_keys"], serde_json::json!(["agent.max_turns"]));
assert_eq!(saved["apply_policy"]["agent.max_turns"], "next_safe_point");
assert_eq!(saved["ok"], true);
```

binding在测试用Task3 shared临时路径+空FrozenRuntimeSources构造。再测试 host override 遮蔽、default/null保留、startup策略next_session、degraded/LKG读取、错误不含USER-SECRET。真实RPC测试验证非法patch整批零通知、提交后广播失败不把响应变成未保存。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-app-server --lib settings_rpc` 和 `cargo test -p yi-agent-app-server --lib runtime_settings`。旧响应缺capability/策略处失败。
- [ ] **Step 3: 最小实现。** 输入patch不变，扩展read/write输出按spec逐键构造；apply_policy/overridden_keys只覆盖changed_keys。errors扩展锁超时/超限、多键非法，保留旧invalid_setting/settings_corrupt稳定分类；无值错误message。RPC/web异步处理用 `tokio::task::spawn_blocking` 调用同步commit，传owned binding/patch，不在reactor里等5秒锁。响应receipt在提交前就可计算好，rename后缓存/广播失败不能触发错误回滚。

```rust
let receipt = binding.live.commit(patch, PatchMode::ReplaceProvided)?;
let policies: std::collections::BTreeMap<_, _> = receipt.changed_keys.iter()
    .map(|key| (key.clone(), apply_policy(key).expect("validated known key")))
    .collect();
```

通知只入hub队列；空changed不发逻辑事件、health变化可同revision空keys失效。跨进程观察通知在Task11。config/read与/config只替换本spec字段当前host值/来源，其他字段沿原脱敏逻辑；不以此修改任何会话权限。web GET保留旧groups/vars形状，可增health/capability不删旧字段，PUT保留旧成功形状并可加元数据。

- [ ] **Step 4: GREEN。** Run上述app-server两组测试、`cargo test -p yi-agent-web`、`cargo test -p yi-agent --bin yi-agent`；以旧请求fixture和新响应fixture验证向后兼容。若新增导出DTO涉及bindings，执行仓库现有绑定生成/检查入口，不手改生成TS。
- [ ] **Step 5: 文档/门禁/提交。** message `feat: report live settings policies and health over rpc`。

## Task 11: 主动观察器、电源、TUI标题/通知和排队唤醒

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings/observer.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/live_settings.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`、`ipc/daemon.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/{lib,attach}.rs`（持有daemon工厂观察lease）
- Modify: `yi-agent-rs/crates/yi-agent/src/{main,managed_runtime}.rs`、`tui/title.rs` 和 `tui/notify/mod.rs`
- Test: observer.rs、app-server `server/tests/runtime_settings.rs`、store `tests/live_admission.rs`、title.rs/notify现有tests。

**Interfaces:** `LiveSettings::start_observing(self: &Arc<Self>, stop: CancellationToken) -> SettingsObservationLease`；lease Drop取消本订阅，最后订阅退出时结束唯一观察线程。`SettingsObservationLease`提供 `pub async fn shutdown(self)` 与 `pub fn shutdown_blocking(self)`，用于异步/同步宿主分别等待退出；单个订阅取消不影响仍活跃的订阅。

生产observer必须有独立受管的执行驱动：start_observing是同步可调用的，每路径首个订阅启动一个命名线程，在自己的Tokio runtime中运行 `async fn observer_loop(live: Arc<LiveSettings>, stop: CancellationToken)`；后续订阅复用，最后退出cancel+join，不依赖调用线程存在持续运行的Tokio executor。原因是daemon当前是std线程+间歇runtime.block_on，直接tokio::spawn不能保证空闲时被驱动。线程只读配置并发送watch失效，不直接执行consumer或等待其ACK。观察运行状态用独立订阅计数/取消句柄，不用Arc强引用数量充当订阅数；最后一个lease必须能cancel线程，即使线程持有LiveSettings的Arc也不能形成永不退出的环。普通Drop只取消并将join交给有界生命周期的回收责任，不在async reactor上同步等待线程；显式shutdown通过spawn_blocking join，shutdown_blocking仅用于同步退出路径，二者不得在观察线程自身join。最后租约退出后重新订阅须串行完成旧线程退出与新线程启动，不能短暂出现双observer。

本地commit直接发布watch，observer周期无需订阅自身watch，防止反馈循环。thread spawn失败以安全诊断标出观察降级，不阻止按需安全点读取，也不谎称主动刷新正常；不得借用“文件已健康”把观察故障误清掉。线程启动状态/诊断与文件快照health分开管理，不新增未在spec约定的文件health code；显式shutdown完成后测试确认无残留线程。

- [ ] **Step 1: 写失败测试。** observer_loop提取为内部async函数，单元测试在paused Tokio runtime直接spawn它，等首次扫描屏障完成后外部原子替换文件，advance999ms不刷新、再advance1ms刷新。不能用生产独立线程的真实时钟冒充被本测试advance控制。真实线程启动/共享/取消/join另用带deadline通道测试，不靠固定sleep。

```rust
#[tokio::test(start_paused = true)]
async fn local_commit_wakes_without_waiting_for_poll_tick() {
    let dir = tempfile::tempdir().unwrap();
    let live = LiveSettings::shared(dir.path().join("settings.json"));
    let stop = tokio_util::sync::CancellationToken::new();
    let observer = tokio::spawn(observer_loop(live.clone(), stop.clone()));
    let mut changes = live.subscribe();
    live.commit(&serde_json::json!({"tui.terminal_title":"false"}), PatchMode::ReplaceProvided).unwrap();
    changes.changed().await.unwrap();
    assert_eq!(live.refresh().settings.tui.terminal_title.as_deref(), Some("false"));
    stop.cancel();
    observer.await.unwrap();
}
```

上述用例仅证明commit唤醒不等周期，不单独证明observer有效；必须另有外部替换/删除/坏文件恢复的周期测试，以及两个lease中取消一个仍继续、最后一个退出线程停止、无运行中Tokio的同步宿主也可观察的生产装配测试。

TUI用已有writer/通知协议夹具：标题曾输出才清、tmux/nonTTY/writefailed不可绕过；去抖候选未发时关闭，之后开启不补发。电源用当前PowerGuard/clock测试夹具，三个参数同快照、资格消失起点不被保存重置、不推进10秒ticker也更新实际guard。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-runtime --lib live_settings::observer`、`cargo test -p yi-agent --bin yi-agent terminal_title`、`cargo test -p yi-agent-store --test live_admission`，确认观察/无新消息扩容唤醒缺失时失败。
- [ ] **Step 3: 最小实现。** 每路径一个周期，正常至多1秒；只刷新短事务，小文件阻塞I/O必要时spawn_blocking，取消不谎称可杀掉OS阻塞调用。consumer逻辑在收到失效后读取最新值，不重放每个中间revision。app-server既消费本地变化也读其他进程变化，广播settings/changed并重算PowerGuard；保留原定时busy/供电/网络探测，配置更新额外唤醒而非删除原探测。

observer由真正持有宿主生命周期的装配层启动：app-server在server transport持lease；TUI在main事件循环持lease；daemon的 `attach::worker_factory_with_binding` / managed_runtime装配持有共享observer lease，使用带binding的工厂持有它直到daemon退出。store层只拿窄source的watch，不依赖runtime。

daemon当前 `ipc/daemon.rs` 是std线程+10ms `daemon_accept_wait`，不是async select循环：在每轮reconcile之前非阻塞检查 `live_admission_receiver.has_changed()`，`borrow_and_update`消费序号，再 `runtime.block_on(coordinator.refresh_live_admission())` 并走既有reconcile/队列扫描；receiver关闭时移除它，不能一直就绪忙循环。本地commit已直接唤醒watch，这里最多等现有短poll，不等1秒观察周期/新用户消息；跨进程observer发布后同样处理。不为唤醒伪造客户端请求或remote touch，也不把整个daemon重写成async服务。TUI事件循环接收标题失效；通知在真正发出前read一次effective三个值，关闭时清候选；/config输出实际限制原因。

- [ ] **Step 4: GREEN。** Run observer/notify/title/power相关targeted测试和 `cargo test -p yi-agent-app-server --lib runtime_settings`。退出join观察任务、drop lease不遗留任务；故障恢复通知不得因revision相同被误去重。去重键为最近已观察的版本/健康组合，不是“历史上见过此hash”。
- [ ] **Step 5: 文档/门禁/提交。** power/TUI/store模块交叉引用，message `feat: observe live settings for power tui and queued work`。

## Task 12: 前端精确解析、草稿合并和可靠失效刷新

**Files:**
- Modify: `desktop/src/lib/runtimeSettings.ts`、`runtimeSettings.test.ts`
- Create: `desktop/src/lib/runtimeSettingsDrafts.ts`、`runtimeSettingsDrafts.test.ts`
- Create: `desktop/src/lib/runtimeSettingsBus.ts`、`runtimeSettingsBus.test.ts`
- Modify: `desktop/src/components/{SettingsRuntimeTab,SettingsDialog}.tsx` 及其 `.test.tsx`
- Modify: `desktop/src/{App.tsx,hooks/useNotificationRouter.ts}` 及通知router现有测试。

**Interfaces:** 原RuntimeField/View保留旧字段，新能力缺失为unsupported；新增精确类型对应spec，不把未知数字能力当1。新增 `parseRuntimeSettingsWrite(value: unknown): RuntimeSettingsWriteResult`。bus按RpcClient身份隔离，仅传“失效”，不缓存设置；App把稳定subscribe函数经SettingsDialog传给SettingsRuntimeTab，组件不直接获取全局client。重连也使当前client订阅失效。

```typescript
export type RuntimeSettingsSubscribe = (listener: () => void) => () => void;
export interface RuntimeSettingsWriteResult {
  ok: boolean;
  live_apply_version?: 1;
  revision?: string;
  changed_keys: string[];
  apply_policy: Record<string, "next_safe_point" | "next_session">;
  overridden_keys: Array<{ key: string; source: "override" | "environment" }>;
}
// runtimeSettingsDrafts.ts；edit generation只在本字段用户编辑时增加。
export interface RuntimeDraftState {
  baseline: Record<string, string>;
  drafts: Record<string, string>;
  edits: Record<string, number>;
  conflicts: string[];
}
export function mergeRuntimeRead(state: RuntimeDraftState, fields: RuntimeField[]): RuntimeDraftState;
export function editRuntimeDraft(state: RuntimeDraftState, key: string, value: string): RuntimeDraftState;
export function acknowledgeRuntimeSave(state: RuntimeDraftState,
  submitted: Record<string, string | null>, editsAtSubmit: Record<string, number>): RuntimeDraftState;
```

- [ ] **Step 1: 写失败测试。** 纯reducer测试远端更新只替换未改字段、同键dirty保留并标conflict、保存中edit generation增加不被ack清理；read解析验证origin/health/source/strategy和旧响应不支持。组件保留现有field/callWith夹具，增加两个deferred Promise交错：

```typescript
it("keeps edits made while save is pending", async () => {
  let finishSave!: (value: unknown) => void;
  const pending = new Promise<unknown>((resolve) => { finishSave = resolve; });
  const call = vi.fn(async (method: string) => method === "settings/read"
    ? { fields: [field({ value: 200 })], path: "/x/settings.json" }
    : pending);
  render(<SettingsRuntimeTab call={call} />);
  const input = await screen.findByLabelText("最大轮次");
  fireEvent.change(input, { target: { value: "201" } });
  fireEvent.click(screen.getByRole("button", { name: /保存/ }));
  fireEvent.change(input, { target: { value: "202" } });
  finishSave({ ok: true });
  await waitFor(() => expect((screen.getByLabelText("最大轮次") as HTMLInputElement).value).toBe("202"));
});
```

如果现有UI在saving期间禁用编辑，需按spec允许保留新编辑的契约测试程序性/其它草稿入口，最终组件不能依赖“禁用一切输入”来逃避reducer正确性。再测迟到read不覆盖新read、read失败不被“已保存”抹掉、手机隐藏TUI组不变。

- [ ] **Step 2: RED。** Run `npm test -- src/lib/runtimeSettings.test.ts src/lib/runtimeSettingsDrafts.test.ts src/components/SettingsRuntimeTab.test.tsx`，旧load中的setDrafts(draftsOf)应导致新编辑被抹的断言失败。
- [ ] **Step 3: 最小实现。** 用分字段baseline/draft/edit generation；save捕获submitted和editsAtSubmit，仅清掉generation未变的已提交键，save期间到来的通知延后一次read。save发起时先递增读取epoch，使保存前已经发出的read也失效；保存结束发一次权威read。call/client变化另递增连接generation并重绑订阅；异步read/save只有连接generation与请求序号都匹配才可合并，旧主机的响应不能更新新主机草稿或清除其dirty。对新client重建baseline/drafts状态，不把A的未保存修改移植到B；若产品需保留，必须按client分别存而不是共用一份。

remote冲突以该字段新值是否真的偏离旧baseline判断：远端未变的dirty字段不标冲突，远端已等于draft时可确认一致；其余同字段真实变更保留草稿并提示。ack仅处理本批提交键和对应edit generation，不能清除保存期间新增编辑。load分配本地request序号，丢弃旧序号迟到响应；不排序revision，A→B→A照样刷新。卸载取消订阅并失效epoch；重连/开页重read。补三个deferred测试：保存前read最后返回、切client后旧save/read返回、dirty但远端baseline未变。下列片段只展示单次read排序，实际组件必须同时检查连接generation和save失效epoch：

```typescript
const requestId = ++latestRead.current;
const view = parseRuntimeSettings(await call("settings/read", {}));
if (requestId !== latestRead.current) return;
setDraftState((state) => mergeRuntimeRead(state, view.fields));
```

bus接口 `onRuntimeSettingsInvalidated(client: RpcClient, listener: () => void): () => void`、`notifyRuntimeSettingsInvalidated(client: RpcClient): void`，内部WeakMap；router须在会话过滤前识别全局settings通知但按client身份路由，不能让远端项目A刷新另一台主机B设置页。App接缝跟当前runtimeSettingsCall使用同一clientRef，连接变化重绑订阅；不拿配置revision代替连接身份。

文案逐字采用spec§8.3，删除“启动时生效”的startup组误导；对旧服务器只显示“已保存”。显示host_source和health、同批沙箱例外，不宣称全体ACK；不要显示完整拼装system prompt。所有new parser对非法元数据安全降级，不能把字段丢掉导致用户草稿消失。

- [ ] **Step 4: GREEN。** Run targeted tests、`npm test`、`npm run build`。测试与现有Vitest保持兼容，不使用未安装的jest-dom matcher。需要安装依赖时在worktree `npm ci`，不要借用main的node_modules。
- [ ] **Step 5: 文档/门禁/提交。** desktop模块交叉引用，前端全测试+构建也作为本项提交前条件；message `feat: refresh runtime settings without losing user drafts`。

## Task 13: 独立 executor 本地绑定和旧 daemon 能力诊断

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/{main,managed_runtime}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/executor/{factory,serve}.rs`、`attach.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/tests/{executor_bridge,executor_gate}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc/{protocol,daemon}.rs`
- Test: CLI managed_runtime现有单元测试；app-server `server/tests/runtime_settings.rs`；Task14 CLI E2E。

**Interfaces:** Status响应新增可选 `#[serde(default, skip_serializing_if = "Option::is_none")] live_apply_version: Option<u8>`；旧Status请求不加字段，兼容deny_unknown_fields旧请求解析。无字段/非1均诊断旧能力，不自动重启。executor调用新增 `managed_runtime::run_executor(owner: String, config_path: Option<PathBuf>, invocation: RuntimeConfig, binding: RuntimeSettingsBinding) -> anyhow::Result<()>`；worker factory复用Task7带binding的装配入口。

```rust
// managed_runtime.rs：只沿用现有control FD，不增加第二个启动描述符。
pub(crate) fn take_executor_control() -> anyhow::Result<std::os::fd::OwnedFd>;
pub(crate) fn run_executor_with_control(
    control: std::os::fd::OwnedFd,
    owner: String,
    config_path: Option<std::path::PathBuf>,
    invocation: yi_agent_runtime::config::RuntimeConfig,
    binding: yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding,
) -> anyhow::Result<()>;
```

生产run_internal_executor调用take_executor_control一次，再装配配置，最终直接调用run_executor_with_control；外层run_executor facade只供仍需自行接管control的内部兼容调用，不得再次接管已转移FD。take验证/移除原授权环境变量，检查既有FD规则并以OwnedFd保证错误路径关闭；测试拒绝无授权/坏FD，不能绕过原内部入口授权。

**基线事实与范围：** `run_internal_executor` / `serve_executor` 是现有私有入口；`ProcessWorkerFactory` 的 connector 在本基线只有集成测试实现，普通生产daemon仍由 `attach::worker_factory` 建本地工厂。不存在可直接修改的生产executor launcher。本任务不把该脚手架扩成新的进程部署系统，不新建启动FD、敏感来源JSON、公开控制消息或第二份settings副本。以后若启用生产launcher，应另审其固定实例环境/生命周期契约；不得在本功能验收里伪称已经自动部署了独立executor。

- [ ] **Step 1: 写失败测试。** 扩展executor_bridge现有socketpair夹具：serve端使用Task7真实带binding的本地工厂和mock provider，工厂模型缓存不变时保存新设置，已有worker下一THINK采用新值。gate夹具确认透传等待结束后才取新快照；桥接测试不冒充真实OS进程。Task14再通过CLI二进制和继承的原有control FD启动真实internal executor，以临时YI_AGENT_HOME、项目cwd和模型清单隔离，证明入口也接入同一路径。

测试给daemon与executor分别注入不同的真实固定来源，验证各自遵循自己的override/env，文件默认值会刷新；不能强迫另一个进程继承父会话普通默认值或抹掉它自己的显式来源。只在同进程构造/重建时克隆已有binding；绝不把合并后的RuntimeConfig反推为固定覆盖表。

旧daemon fixture返回没有capability的Status；确认attach成功时仍可用旧功能，同时记录升级警告，保存只承诺支持者；不能把“Status正常”误判成热更新支持。新Status往返序列化包含Some(1)，旧响应读成None。

- [ ] **Step 2: RED。** Run `cargo test -p yi-agent-subagent --test executor_bridge`、`cargo test -p yi-agent-subagent --test executor_gate`、`cargo test -p yi-agent --bin yi-agent managed_runtime`、`cargo test -p yi-agent-store --lib ipc`。旧executor固定启动快照和旧能力误判必须出现行为失败。
- [ ] **Step 3: 最小实现。** `run_internal_executor` 在加载任何配置/创建工具前先取得并验证原有control descriptor的所有权；固定本进程稳定settings路径、capture本进程真实来源一次，调用Task3 load_lenient_with_binding后把config/binding一起交给run_executor。把现有inherited_descriptor拆成受控取用入口，避免run_executor重复接管同一FD；可由私有 `run_executor_with_control(control: OwnedFd, owner: String, config_path: Option<PathBuf>, invocation: RuntimeConfig, binding: RuntimeSettingsBinding) -> anyhow::Result<()>` 承接，外层run_executor保留取control的兼容facade，生产入口只取一次。

executor工厂在本地决定provider/凭据、显式权限及配置源。Start只携带现有任务协议，不允许它换settings路径、固定来源或凭据。Task7的带binding factory facade用于该本地装配；ProcessWorkerFactory的daemon本地额度source仍走Task8，不依赖远端worker推额度。保留StopWitness、cancel、gate mirror、frame上限和ready barrier。旧daemon诊断利用现有Status握手，不扫描全机进程，不注册全体ACK；宿主能力不代表未知外部CLI能力。

- [ ] **Step 4: GREEN。** Run上述targeted测试和 `cargo test -p yi-agent-subagent --lib`；Status枚举新增字段的所有构造/精确匹配同步适配。独立OS进程证据必须来自Task14的真实internal入口；未运行旧二进制只称旧响应fixture验证，不声称实测旧进程。
- [ ] **Step 5: 文档/门禁/提交。** runtime/subagent/CLI模块记录部署前提与脚手架边界，message `feat: bind executor settings locally and report live capability`。

## Task 14: 双进程、全消费覆盖、反接线与最终验收

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/tests/live_settings_process.rs`
- Create: `yi-agent-rs/crates/yi-agent/tests/runtime_settings_live_e2e.rs`
- Create: `yi-agent-rs/scripts/runtime-settings-consumers.json`、`yi-agent-rs/scripts/check-live-settings-coverage.py`（仅标准库，执行真实消费测试）
- Modify: app-server `server/tests/runtime_settings.rs`、core/runtime/subagent现有live测试
- Modify: `docs/project-management/{yi-agent-runtime,README}.md` 及已改模块交叉引用
- Create: `docs/research/2026-11-04-runtime-settings-live-apply-validation.md`（执行时如实记录实际日期/命令/结果，文件名固定便于引用）。

**Interfaces:** Consumes前述全部真实入口；无新生产API。子进程fixture使用同一test executable + `current_exe` 运行专用exact测试名，通过stdin/stdout行握手而非sleep；进程管道只传临时path/非秘密patch。

- [ ] **Step 1: 写失败的真实双进程测试。** 在runtime集成测试定义 `#[test] fn settings_writer_child()`：仅当 `YI_TEST_SETTINGS_CHILD_PATH`存在才先输出并flush专用前缀 `YI_SETTINGS_CHILD READY`，再等待stdin一行patch、调用真实commit_patch、输出并flush `YI_SETTINGS_CHILD COMMITTED`；普通cargo test无env时立即return。父测试Command.current_exe加 `--exact settings_writer_child --nocapture`，env临时path，stdin/out piped，逐行读取识别专用前缀，不把test harness标准输出当协议帧；用有deadline的通道/线程读，不无界阻塞。

```rust
let exe = std::env::current_exe().unwrap();
let mut child = std::process::Command::new(exe)
    .args(["--exact", "settings_writer_child", "--nocapture"])
    .env("YI_TEST_SETTINGS_CHILD_PATH", &path)
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn().unwrap();
// 等子进程 READY；写patch并等COMMITTED；本进程不收RPC通知。
// 调用live.refresh，断言新值；finally kill/wait只处理本测试child。
```

为该测试在同文件定义RAII ChildGuard保证失败时kill/wait，不杀其它cargo进程。两个子进程都READY后同时发不同键patch，结果必须都保留；同键明确控制先后COMMITTED验证最后提交胜出。另用持锁子进程验证5秒超时分类、进程退出后内核释放、锁inode未被删除。真实超时测试可用内部注入短lock deadline减少测试耗时，但生产5秒常量和可控时钟边界要另测。

- [ ] **Step 2: RED。** 暂时关闭一个实际消费接线或回退旧副本，运行对应测试必须红：正常factory、run循环、resident准入各至少一处。删除RPC通知后安全点新鲜度仍绿；主动消费者的观察器可以保留，不能删掉所有唤醒再要求标题“立即”更新。回滚临时变异后确认git diff不残留反接线代码。
- [ ] **Step 3: 完成真正端到端场景。** 用现有wiremock/provider脚本，不用真实API：两个项目、一个长工具回合、一个空闲会话、一个现存子代理、一个旁问；经真实settings/write保存，验证后续实际请求/配对/额度，非选中会话不漏。E2E mock端点绑定随机localhost端口，YI_AGENT_HOME临时独立，不能继承正式key或修改正式settings。手机入口按相同配对/control作用域RPC测试，不能用前端仅改state代替宿主采用。
- [ ] **Step 4: GREEN、覆盖表与全回归。** Run：

```bash
cargo test -p yi-agent-runtime --test live_settings_process
cargo test -p yi-agent-core --test live_agent_settings
cargo test -p yi-agent-core --test live_compact_settings
cargo test -p yi-agent-store --test live_admission
cargo test -p yi-agent-app-server --lib runtime_settings
cargo test -p yi-agent --test runtime_settings_live_e2e
cargo test -p yi-agent-subagent --lib
```

再在desktop执行 `npm test && npm run build`；回Rust执行 `cargo fmt --all && just ci`（唯一完整门禁入口，实际依赖以justfile为准，不把它误写成固定16项）。如果OOM/锁等待，先按项目诊断，不能删锁、关测试或用跳过门禁换绿。

- [ ] **Step 5: 人工与文档收口。** 记录两个项目/长回合/空闲/现存子代理、桌面与手机保存、缺基线历史告警、独立YI_AGENT_HOME隔离；若设备不可用明确写“未执行”，此时不得将统一需求标完成。全部满足后runtime统一项改 `[x]`、索引27/29改28/29（按当时真实计数重算，不硬改覆盖其他并发成果），各模块交叉引用不重复计数。提交message `test: verify runtime settings live apply across processes`。
- [ ] **Step 6: 审阅、合并后再验。** 请求code review并解决spec/实现两类问题；在全绿基础上按项目worktree流程 `git merge --no-ff <implementation-branch>`，在合并结果再次 `cargo fmt --all && just ci` 和desktop相关检查；通过后移除worktree/分支，确认无编译进程再按dry-run清理本轮target。未合并或门禁未通过，报告真实状态，不声称已交付。

## 15. Spec 验收追踪表

测试名是在上述任务中要求创建/扩展的用例，不能只在表里出现。执行者每完成任务将实际测试路径/名字和运行结果写入验证记录；此表不是已通过清单。

| Spec | 实施任务 | 可验证消费/失败点 |
|---|---|---|
| T01 | 7、14 | app-server双已有会话/新建会话真实provider请求，未选中会话同样更新 |
| T02 | 4、7、14 | 在途tool屏障更新后下一THINK新prompt、工具只一次且配对 |
| T03 | 5 | used+1边界、增降额、max=1、MaxTokens、结束不复活 |
| T04 | 5、6 | 各自模型context、摘要system=None、预算同快照 |
| T05 | 6、7 | 新用户段/清空、目录刷新、IncludeAll来源失效、无重装 |
| T06 | 8、11 | 真实supervisor和resident协调器、无新消息扩容唤醒、不超发 |
| T07 | 7、13 | 现存worker模型缓存不变、现存独立executor后续请求采用 |
| T08 | 3、14 | 双进程无RPC通知、同mtime/size新内容、实例隔离 |
| T09 | 2、14 | 多写入方、迁移锁内重查、实际RPC/web入口 |
| T10 | 2、14 | 首次目录、锁/临时文件/权限/sync/rename提交前故障 |
| T11 | 3、10、11 | LKG、startup_fallback、故障恢复/缺文件、health可见 |
| T12 | 3、6、10 | 全16字段默认/真实来源、非运行字段不变 |
| T13 | 11 | 真通知发送边界、标题观察、终端限制、退出取消 |
| T14 | 11、14 | 实际PowerGuard不等10秒tick、三字段同快照 |
| T15 | 9 | 新建固化、冷热恢复/fork/重建、显式宿主更严格限制、失败不扩权 |
| T16 | 10、12、13、14 | 手机Control保存、重连read、旧能力不作新保证 |
| T17 | 5、6、7 | 旁问独立历史/硬预算/generation与naked约束 |
| T18 | 12 | draft reducer/迟到read/save期间编辑/健康与例外文案 |
| T19 | 1、14 | FIELDS策略+来源+真实消费映射缺项必失败 |
| T20 | 14 | 去通知仍新鲜、恢复静态/取消真实准入则红 |
| T21 | 3、14 | 慢旧open屏障、缓存不倒退、失败回退不倒退 |
| T22 | 4、5 | 首次gate/摘要等待后重读、降额释放lease、同操作重试快照固定 |
| T23 | 7、9 | MCP/模式/模型合法重建后绑定与角色仍在 |
| T24 | 2、3、10、14 | rename后发布/通知失败不伪装未提交、重读确认 |
| T25 | 1、2 | 旧键优先级、清空不复活、未知键保留、逻辑版本去重 |
| T26 | 1、2、3 | 有界读写、候选修复、错误/日志无值 |
| T27 | 11 | 可控1秒周期、本地立即唤醒、事件丢失兜底/恢复/退出 |
| T28 | 3、10、12 | 精确新旧协议、A→B→A不排序、同revision健康失效 |

### 全字段实际消费清单

Task14增加表驱动测试遍历FIELDS，并维护以下Consumer集合；新增字段没有映射就失败，策略表有字段但没有实际测试也失败。

具体执行闭环：创建scripts/runtime-settings-consumers.json，每项包含key、package、target_kind（lib/bin/test）、target、完整test_name，测试名以实施后真实输出为准。runtime的live_settings_process中增加 `consumer_manifest_covers_all_fields`，用include_str!/serde_json读取该清单，校验键集合精确等于FIELDS、无重复键、每项至少一个消费场景；一个场景覆盖多字段时各字段分别显式映射。check-live-settings-coverage.py先运行该校验，再按package/target分组用cargo test的 `-- --list` 确认精确名称存在，随后逐项 `--exact` 执行并要求实际运行非零、非ignored、全通过；任何空匹配/跳过/缺字段均exit非零。不能只搜索源码里有测试函数或只验证本文表格。Task14的GREEN命令额外执行 `python3 scripts/check-live-settings-coverage.py` 并保存输出；CLI target_kind使用bin而非不存在的lib。该清单不是另一个运行期配置源。

| 字段 | 实际消费测试位置 |
|---|---|
| agent.max_turns | core `tests/live_agent_settings.rs` |
| agent.max_direct_children | core `tests/subagent_supervisor.rs`、store `tests/live_admission.rs` |
| agent.max_resident_subagents | store `tests/live_admission.rs` |
| agent.system_prompt | core `tests/live_agent_settings.rs`、runtime live_settings/prompt tests |
| agent.skills_catalog_budget | runtime live_settings/prompt tests + app-server runtime_settings loop测试 |
| compact.ratio | core `tests/live_compact_settings.rs` + runtime context推导测试 |
| compact.user_budget_tokens | core `tests/live_compact_settings.rs` |
| compact.tool_budget_tokens | core `tests/live_compact_settings.rs` |
| power.prevent_idle_sleep | app-server runtime_settings/PowerGuard测试 |
| power.idle_grace_minutes | app-server runtime_settings/PowerGuard测试 |
| power.remote_touch_minutes | app-server runtime_settings/PowerGuard测试 |
| tui.notifications | CLI notify实际发送测试 |
| tui.notification_method | CLI notify实际发送测试 |
| tui.notification_condition | CLI notify实际发送测试 |
| tui.terminal_title | CLI title实际writer测试 |
| startup.sandbox | app-server session_sandbox/thread_lifecycle测试 |

## 16. 计划自检与交接边界

- [ ] 实施前再核spec：§1/§5→Task4–7；§3/§7→Task7–9/13；§6/§9→Task1–3/8/11；§8→Task10/12/13；§10全部T01–T28→§15；§11完成判据→Task14。
- [ ] 检查所有新增文件已在任务中创建、所有后续接口能追溯到总览或前置任务；改名时一并更新总览、测试命令与下游调用。
- [ ] 不将本计划中的代码片段误当已应用补丁；测试fixture属于测试，不能把屏障/故障注入暴露成生产RPC。
- [ ] 文档阶段不运行真实LLM、不改正式配置、不打包或重启app；实施阶段同样先用mock/临时实例。
- [ ] 若要把spec/plan单独提交或合并，仍执行项目要求的门禁，不能因为“只是文档”谎称已全绿；当前写计划不自动提交/合并。

计划允许按任务审阅/暂停，但只有Task14全部证据具备才可宣称“运行参数全会话即时生效”完成。
