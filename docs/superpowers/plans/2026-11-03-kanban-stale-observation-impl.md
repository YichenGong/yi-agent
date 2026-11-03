# 看板会话观测误报修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让普通恢复的热会话在下一次成功 driver tick 被看板正确观测，且未加载/未知不再被描述为历史消失。

**Architecture:** 宿主从完整 ThreadSession 热表采样，新增可选 loaded_threads 请求字段。插件在独立小模块中严格解析、按相关执行 ID 归一化，将旁路原子保存成功作为回报消费与调度的前置条件；现有 lease/gate 使用该有效事实。桌面与 CLI 仅展示加载观测，不推断任务完成。

**Tech Stack:** Rust、Tokio、serde/serde_json、chrono、既有 Unix IPC/flock；TypeScript、React、Vitest。不新增依赖、不升级 MSRV、不新增公开 RPC 或卡片状态。

## Global Constraints

- 唯一需求源：[spec](../specs/2026-11-03-kanban-stale-observation-design.md)，含 K1–K17；调查与设计基线 `a44fbe5a`。本计划未执行，不是修复报告。
- 本轮只复核和写计划。实施必须另行获得授权，在新的 `fix/kanban-stale-observation` worktree 执行，不在 main 修改代码。
- 观测不修改 card state/thread_id/origin_thread_id/workdir/detail/terminal_at；既有匹配回报和授权派发的合法写卡保持原规则。
- 不自动恢复、不发新模型 turn 来清角标、不读历史判断 alive、不猜 opaque driver_ref 的插件归属、不把 origin_thread_id 当执行绑定。
- loaded_threads 缺字段为旧宿主；显式 null/非数组/缺字段/空或纯空白 ID/错误类型/重复 ID 为错误。不 trim ID，允许未知附加键，不推断插件归属。
- idle = ThreadStatus::Idle 且 active_turn_id=None；AwaitingApproval 与 Running 不算 idle。sessions.running 口径和全机额度不改。
- 帧限制保持 1,048,576 字节；不得截断/省略完整快照或添加分页协议。
- 顺序：解析/相关状态读取 → 归一化 → 保存旁路 → tick 序号 → 消费结果 → gate 结算 → lease 补账 → 调度。保存/前置读取失败必须不消费回报、不释放资源、不派发。
- pending/accepted/uncertain merge attempt 保护同一卡片单份已有 lease；不改 gate 的 accepted、绑定、后续 alive+idle 等释放条件。
- 旧 alive/idle 默认值保持调度兼容，但没有显式 alive 不能伪造加载证据或最后已加载时间；旧宿主遗漏 ID 的旧配额局限不在本次被保证修复。
- 旧 runner + 新 UI 可修文案；旧 UI 硬编码不能靠 runner 修复。宿主、runner、UI 均升级后才验收完整功能。
- 新测试名前缀统一 `kanban_observation_`；宿主生产逻辑不得因此新增看板专属分支。默认 mock，不用 --ignored，不调用真实 LLM。
- 每个交付提交执行主 workspace `cargo fmt --all && just ci`，插件 workspace 另跑格式与 runner 测试；仅提交本任务文件及对应进度。合并后再执行同门禁。
- 安装版本可能已与调查基线不同；实施前定点检查下表路径及相关函数，不顺手重构已拆分的大模块。

## 文件地图与固定接口

下文所有路径和命令默认从实施 worktree 根执行；不使用省略路径作为命令。

| 文件 | 责任 |
|---|---|
| `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs` | 宿主 LoadedThreadFacts DTO、热快照纯函数与 driver 请求编码 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/driver/tick.rs` | launcher 前采样并传递完整热快照 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/session_driver_tests.rs` | 真 tick/真实插件串联；只扩展测试 fixture 的可见性 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/thread_lifecycle.rs` | 普通 thread/resume + 真 serve tick 恢复回归 |
| 新 `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/driver_observation.rs` | 请求解析、相关 ID 归一化纯逻辑，不做磁盘或调度 |
| `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/observation.rs` | 可兼容旁路记录与每 ID 展示降级，共用投影 |
| `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` | 注册模块、解析接线、匹配 pending/保存顺序、lease 安全 |
| `plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json` | 双端契约样例，保留旧请求样例 |
| `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs` | 离线 CLI 复用观测投影，不重复解释 alive |
| `desktop/src/lib/superpowersKanbanState.ts`、同目录 `.test.ts` | list 新字段归一化、兼容旧 stale |
| `desktop/src/components/SuperpowersKanbanCard.tsx`、同目录 `.test.tsx` | 加载角标、截至时间、原会话链接 |
| 新 `desktop/src/lib/kanbanObservation.ts`、同目录 `.test.ts` | 观测提示和包含日期的时间格式纯函数 |
| `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`、`INSTALL.md` | 使用与混装边界 |
| `docs/project-management/superpowers-kanban.md`、`docs/project-management/README.md`、`docs/bug-list.md` | 每个交付记录实际证据，最终通过才勾完成 |

固定接口（任务间逐字一致）：

```rust
// 宿主：session_driver.rs。与插件类型分离，不添加生产依赖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedThreadFacts { pub thread_id: String, pub idle: bool }
pub(crate) fn loaded_threads_snapshot(
    threads: &std::collections::HashMap<String, crate::session::ThreadSession>,
) -> Vec<LoadedThreadFacts>;
pub(crate) fn loaded_threads_payload(threads: &[LoadedThreadFacts]) -> serde_json::Value;
// 宿主 driver_step 保留现有参数，最后追加：loaded_threads: &[LoadedThreadFacts]。

// 插件：driver_observation.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedThreadFacts { pub thread_id: String, pub idle: bool }
pub struct ObservationInput {
    pub legacy: Vec<crate::service::ThreadFacts>,
    pub reported_alive: std::collections::BTreeMap<String, bool>,
    pub loaded: Option<Vec<LoadedThreadFacts>>,
}
pub struct NormalizedObservation {
    pub facts: Vec<crate::service::ThreadFacts>,
    pub file: crate::observation::ObservationFile,
}
pub fn parse_observation_input(params: &serde_json::Value) -> Result<ObservationInput, String>;
pub fn normalize(
    input: &ObservationInput,
    relevant: &std::collections::BTreeSet<String>,
    previous: &crate::observation::ObservationFile,
    now: chrono::DateTime<chrono::Local>,
) -> NormalizedObservation;

// observation.rs：仅展示投影，不用于推断跨进程停止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionObservation { Loaded, NotLoaded, Unknown }
impl SessionObservation { pub fn as_str(self) -> &'static str; }
impl Observation {
    pub fn presentation(&self) -> SessionObservation;
    pub fn presentation_at(&self) -> Option<chrono::DateTime<chrono::Local>>;
}
pub fn for_card(card: &superpowers_kanban_core::card::Card,
                file: &ObservationFile) -> SessionObservation;

// service.rs：保持旧调用面，旧 driver_step 包装并委托新入口。
pub fn driver_step_with_loaded(&self, input: &ObservationInput,
    results: &[DriverResult], sessions_running: Option<u32>,
    capabilities: &DriverCapabilities) -> Result<Option<DriverAction>, String>;
// 私有 related_thread_ids(results) -> Result<BTreeSet<String>, String>；
// write_observations 不再 log-and-continue，由新入口显式处理保存错误。
```

旧 `BoardService::driver_step(threads, results, sessions_running, capabilities)` 不再保留另一套流程：它构造 legacy 输入（显式 typed ThreadFacts 的 alive 是已提供证据），loaded=None 后直接委托新入口。dispatch 从原始 JSON 解析 reported_alive，才能保留“缺 alive”与“显式 true”的区别。

## 执行与测试约定

```sh
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/sbin:$PATH"
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib kanban_observation_ -- --list
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib kanban_observation_ -- --nocapture
```

过滤测试必须先 --list 确认目标名字存在且非零，不能以“0 tests, exit 0”验收。flock 复用遵循已有有界等待 fixture，禁止删锁解排队；不并发启动多个针对同 target 的编译。

每 Task 最后独立等待以下门禁，不把 CI 当作一个 2 分钟编辑动作：

```sh
(cd yi-agent-rs && cargo fmt --all && just ci) &&
(cd plugins/superpowers-kanban && cargo fmt --all -- --check && cargo test -p superpowers-kanban-runner) &&
git diff --check
```

主 CI 不代替插件独立 workspace 测试。Task 8 才做真机打包验收；前面只用隔离 fixture，不安装到生产 App，不重启生产看板。新文件先 git add 后才能被只扫描 tracked 文件的 secret-check 检查；门禁失败不得 commit。

---

### Task 1: 通用快照契约与插件严格解析

**Files:** Create `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/driver_observation.rs`；Modify `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lib.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json`；Test 新模块的 tests。

**Interfaces:** Produces 插件 LoadedThreadFacts、ObservationInput、parse_observation_input；normalize 留给 Task 2，不建立空实现。Consumes 既有 service::ThreadFacts。

- [ ] Step 1：写解析测试，覆盖缺新字段保留旧语义与空完整数组的区别。实际代表测试：

```rust
#[test]
fn kanban_observation_empty_complete_is_not_legacy() {
    let legacy = parse_observation_input(&serde_json::json!({"threads": []})).unwrap();
    let complete = parse_observation_input(
        &serde_json::json!({"threads": [], "loaded_threads": []})
    ).unwrap();
    assert!(legacy.loaded.is_none());
    assert_eq!(complete.loaded, Some(Vec::new()));
}
#[test]
fn kanban_observation_invalid_complete_is_rejected() {
    for value in [
        serde_json::json!(null), serde_json::json!({}),
        serde_json::json!([{"thread_id":"t"}]),
        serde_json::json!([{"thread_id":" ","idle":false}]),
        serde_json::json!([{"thread_id":"t","idle":"yes"}]),
        serde_json::json!([{"thread_id":"t","idle":false},{"thread_id":"t","idle":true}]),
    ] {
        assert!(parse_observation_input(
            &serde_json::json!({"threads":[],"loaded_threads":value})
        ).is_err());
    }
}
#[test]
fn kanban_observation_default_alive_is_not_reported_alive() {
    let input = parse_observation_input(
        &serde_json::json!({"threads":[{"thread_id":"t","idle":false}]})
    ).unwrap();
    assert!(input.legacy[0].alive);
    assert!(!input.reported_alive.contains_key("t"));
}
```

- [ ] Step 2：运行 `cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-runner --lib kanban_observation_ -- --nocapture`，先记录新 API 编译红；实现后删除一次“duplicate ID 拒绝分支”重新跑重复项测试，必须行为红，再恢复。编译红不替代产品行为红。
- [ ] Step 3：把 dispatch 当前 threads 解析移入 parse_observation_input，保持 legacy 缺 idle=false、缺 alive=true。reported_alive 只收 as_bool 的显式项。loaded 存在时逐项验证 nonblank 字符串 ID、boolean idle，用 BTreeSet 拒绝重复，ID 原样保存。错误带 `driver/next loaded_threads[index]` 和字段原因；不输出历史/凭据。

```rust
let loaded = match params.get("loaded_threads") {
    None => None,
    Some(value) => {
        let array = value.as_array().ok_or_else(||
            "driver/next loaded_threads must be an array".to_string())?;
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::with_capacity(array.len());
        for (index, item) in array.iter().enumerate() {
            let id = item.get("thread_id").and_then(serde_json::Value::as_str)
                .filter(|id| !id.trim().is_empty()).ok_or_else(||
                    format!("driver/next loaded_threads[{index}] needs a nonblank thread_id"))?;
            let idle = item.get("idle").and_then(serde_json::Value::as_bool)
                .ok_or_else(|| format!("driver/next loaded_threads[{index}] needs boolean idle"))?;
            if !seen.insert(id.to_owned()) {
                return Err(format!("driver/next loaded_threads[{index}] duplicates thread_id"));
            }
            out.push(LoadedThreadFacts { thread_id: id.to_owned(), idle });
        }
        Some(out)
    }
};
```

- [ ] Step 4：契约的 request 增加 loaded_threads 样例；新增 `legacy_request` 样例保留原 request 但删新字段。双端已有测试读取 request 时不得删 action/results 断言。此任务不接 dispatch，所以不会改变运行行为。上述测试与既有 driver-protocol include 测试通过；记录 K9 的类型/空白/重复覆盖。
- [ ] Step 5：更新模块未完成条目，注明解析交付而非功能已修；执行统一门禁并提交 `feat(kanban): define complete loaded-thread snapshot contract`。

### Task 2: 可兼容旁路与纯归一化

**Files:** Modify `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/observation.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/driver_observation.rs`；Test 两模块 tests。

**Interfaces:** Produces SessionObservation、Observation::presentation/presentation_at、for_card、normalize 和 NormalizedObservation。Observation 保留 alive 与 last_seen_alive_at，增加可选 `session_observation: Option<String>`、`observed_at: Option<String>`。

- [ ] Step 1：先写旧 false 不构成新死亡证明的测试，以及新字段坏类型不清空其他 ID 的测试：

```rust
#[test]
fn kanban_observation_legacy_false_is_unknown() {
    let obs: Observation = serde_json::from_value(serde_json::json!({
        "alive":false,"last_seen_alive_at":"2026-11-03T08:35:13+08:00"
    })).unwrap();
    assert_eq!(obs.presentation(), SessionObservation::Unknown);
    assert!(obs.presentation_at().is_none());
}
#[test]
fn kanban_observation_bad_new_fields_preserve_other_ids() {
    let file: ObservationFile = serde_json::from_value(serde_json::json!({"threads":{
        "bad":{"alive":false,"session_observation":42,"observed_at":{}},
        "good":{"alive":true,"session_observation":"loaded", "observed_at":"2026-11-03T10:00:00+08:00"}
    }})).unwrap();
    assert_eq!(file.threads["bad"].presentation(), SessionObservation::Unknown);
    assert_eq!(file.threads["good"].presentation(), SessionObservation::Loaded);
    assert!(file.threads["good"].alive);
}
```

- [ ] Step 2：运行上述 crate filtered tests，确认新接口红。现有 Observation struct literals 定点搜索 `git grep -n 'Observation {' -- plugins/superpowers-kanban yi-agent-rs`，逐处显式补新字段，不 blanket 删除旧单测。
- [ ] Step 3：新增字段使用容错字符串反序列化，避免未知枚举/坏类型使整个文件 load→default。旧字段的既有读取规则不改。

```rust
fn optional_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.as_str().map(str::to_owned))
}
// 两个新字段都用 #[serde(default, deserialize_with = "optional_string")]。
// presentation_at: DateTime::parse_from_rfc3339(s).ok().map(|at| at.with_timezone(&Local))。
// presentation: 有合法 observed_at 且字符串为 loaded/not_loaded 才返回对应态，否则 Unknown。
```

- [ ] Step 4：实现 normalize：相关集合是唯一持久化范围，BTree 排序使测试稳定。
  - loaded=Some：为每个相关 ID 生成 facts；命中=true/实际 idle，未命中=false/false；observed_at=now，presentation 对应 loaded/not_loaded；仅 true 刷 last_seen_alive_at。
  - loaded=None：facts 保留相关 legacy 项的 alive/idle；只 reported_alive 内的 boolean 更新展示与最后 true 时间。缺 alive 展示 unknown/observed_at=None，调度默认 true 保留。
  - legacy 完全遗漏的相关 ID：有旧记录时保留最后 true 时间、按旧 write_observations 规则 alive=false 但展示 unknown；此前无记录则不插入 false，保留现有 running/terminal 的缺记录租约默认规则。for_card 对缺记录返回 Unknown。
  - 无关旧记录 prune。起始 previous 是只读输入，不就地污染。
  - ObservationFile::observe 的 typed bool 调用是显式证据，可生成新字段；normalize 的缺字段分支不能借该函数 observe(true) 编造时间。stale 改为 running + nonblank 执行 ID + presentation=NotLoaded，CLI/list 复用同函数。
- [ ] Step 5：写完整优先级测试：

```rust
#[test]
fn kanban_observation_complete_overrides_stale_subset() {
    let input = parse_observation_input(&serde_json::json!({
        "threads":[{"thread_id":"t","alive":false,"idle":false}],
        "loaded_threads":[{"thread_id":"t","idle":true},{"thread_id":"other","idle":false}]
    })).unwrap();
    let relevant = std::collections::BTreeSet::from(["t".to_string()]);
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-03T10:00:00+08:00")
        .unwrap().with_timezone(&chrono::Local);
    let normalized = normalize(&input, &relevant, &ObservationFile::default(), now);
    assert_eq!(normalized.facts.len(),1);
    assert!(normalized.facts[0].alive && normalized.facts[0].idle);
    assert_eq!(normalized.file.threads["t"].presentation(), SessionObservation::Loaded);
    assert!(!normalized.file.threads.contains_key("other"));
}
```

补测 K6/K12/K15/K16：两次 idle true 只刷新“观测”时间；缺 alive 不刷新历史 true 时间；invalid RFC3339/未知态只该 ID unknown；complete=[] 与 legacy 缺项不同；来源和孤儿 ID 未纳入相关集合不保存。通过 filtered tests 与 runner 全测，记录变异（把 complete 优先级移到 legacy 后，K3 红）。
- [ ] Step 6：更新对应未完成进度，统一门禁提交 `fix(kanban): distinguish loaded observations from legacy defaults`。

### Task 3: service 前置保存、回报身份及租约接线

**Files:** Modify `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`；Test 两文件 tests。

**Interfaces:** Consumes Task 1–2 的 ObservationInput、normalize、ObservationFile。Produces driver_step_with_loaded 和旧 driver_step 委托层、list 的 session_observation/observed_at 投影；related_thread_ids 私有 helper。不更改结果 schema、merge attempt phase 或 can_release_merge 判据。

- [ ] Step 1：先用现有 dispatch tests 的隔离 fixture，写真实 list 优先级用例：

```rust
#[test]
fn kanban_observation_dispatch_finds_a_loaded_bound_thread() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_with_worktree(dir.path());
    let service = service_with_one_card(&project);
    let state = state_dir(&project);
    crate::persist::with_board(&state, |board| {
        board.transition(&superpowers_kanban_core::card::CardId::new("card-1"),
            superpowers_kanban_core::card::CardState::Running)
            .map_err(|error| error.to_string())
    }).unwrap();
    service.mark_running("card-1", "thread-1").unwrap();
    let reply = dispatch_with_service(&service,&state,None,"driver/next",&serde_json::json!({
        "threads":[],"loaded_threads":[{"thread_id":"thread-1","idle":true}],
        "results":[],"sessions":{"running":0}
    })).unwrap();
    assert_eq!(reply["action"],serde_json::Value::Null);
    let card = &service.list()["cards"][0];
    assert_eq!(card["session_observation"],"loaded");
    assert_eq!(card["stale"],false);
    assert_eq!(card["thread_id"],"thread-1");
}
```

- [ ] Step 2：跑 filtered tests 确认接口未接线时红。随后在 service tests 用既有 service_with_card/project_with_worktree 造实际 pending Launch：第一拍获得 action_id，第二拍回成功 result 和 loaded ID。断言绑定落盘前已观察该 ID，结果重复回报不再消费；陌生 action_id/ResumeForMerge 错 ID 不写进旁路。
- [ ] Step 3：related_thread_ids 按既有锁顺序读数据，短暂 Inner 锁只复制 pending 标识后释放；不要持 board.lock 等 Inner。严格 read_board/merge_attempt；不存在 board=首次空，坏读=Err。收集 board 全体（含 archived）执行 ID、有效 attempt thread、pending resume ledger thread、匹配成功 Launch/LaunchForMerge result ID；ResumeForMerge 成功 ID 必须等于 ledger 预期 ID。空 ID 不纳入。来源讨论、purge 孤儿、陌生 result 排除。
- [ ] Step 4：新入口先保留既有 leases_dir 前置检查，再 normalize+save，成功后执行既有主体。此 Task 同时接 list 新字段，使本任务的真实 dispatch→list 测试可独立通过；Task 5 只扩充展示/CLI 契约。关键代码顺序：

```rust
let relevant = self.related_thread_ids(results)?;
let previous = crate::observation::load(&self.state_dir);
let normalized = crate::driver_observation::normalize(
    input, &relevant, &previous, chrono::Local::now());
crate::observation::save(&self.state_dir, &normalized.file)
    .map_err(|error| format!("could not save session observations: {error}"))?;
// 这里之后才 increment driver_tick_seq、consume_results。
// release_gate_when_the_merge_is_over(&normalized.facts, tick_seq)。
// 既有 gate 完成结算后 adopt_running_leases(limit)，不以 budget>0 为前提。
// promote_merge_queue 使用 normalized.facts；next_launch 从已保存旁路读取 lease 事实。
```

dispatch 在任何 service 调用前完成 input、results、capabilities 的解析。缺完整数组时仍有新展示来源标记，不能包装为全显式 ThreadFacts 丢掉 missing-alive 信息。
list 每卡字段在此任务接线：

```rust
"session_observation": card.thread_id.as_deref()
    .filter(|id| !id.trim().is_empty())
    .map(|_| crate::observation::for_card(card, &observations).as_str()),
"observed_at": card.thread_id.as_deref()
    .and_then(|id| observations.threads.get(id))
    .filter(|obs| obs.presentation() != crate::observation::SessionObservation::Unknown)
    .and_then(crate::observation::Observation::presentation_at)
    .map(|at| at.to_rfc3339()),
```

- [ ] Step 5：reconcile_leases_locked 加有效 merge attempt 卡集合保护，pending/accepted/uncertain 未清理时 retain 原 lease；新完整 not_loaded 不提前放。已有同卡 lease 不再 acquire，缺原 lease 的恢复按现有补领机制处理。相关 journal 坏读不启动新动作；本次不改 release_gate_when_the_merge_is_over 的释放条件。budget=0 也补账，确保重启后的 loaded 热卡单份 lease 被复用/补领。
- [ ] Step 6：故障测试不靠 chmod（root/ACL 环境不稳定），用目录占据 `observations.json.tmp` 让保存明确失败；先读旧 observations 字节并记 pending 数/tick_seq/lease 数/holder，再调用带匹配 result 的新入口。断言 Err、全部未变化且无新 claim；删测试目录后重试成功一次。分别用坏 board JSON、坏 attempt JSON 做前置拒绝。新事实缺合并 ID 的 pending/accepted/uncertain case 都断言 lease 保留、gate 不放；已 accepted 当拍 idle 不放、后一拍合规 idle 才按旧规则释放。
- [ ] Step 7：runner 全测通过。原测试里“空快照必 stale”的期望有意改为 unknown，但保留 card state 不变断言；旧 terminal 缺记录不占槽/新 loaded terminal 仍按热口径占槽回归不得删除。记录 K9/K11/K14/K16。
- [ ] Step 8：模块进度仍未完成（宿主/UI 未交付），统一门禁提交 `fix(kanban): commit observations before consuming driver results`。

### Task 4: 宿主真实热快照生产与接线

**Files:** Modify `yi-agent-rs/crates/yi-agent-app-server/src/session_driver.rs`、`yi-agent-rs/crates/yi-agent-app-server/src/server/driver/tick.rs`；Test `session_driver.rs`、`server/session_driver_tests.rs`。

**Interfaces:** Produces 宿主 LoadedThreadFacts、loaded_threads_snapshot、loaded_threads_payload；driver_step 最后参数追加 &[LoadedThreadFacts]。Consumes Task 1 的共读契约，生产宿主不依赖 runner 类型。

- [ ] Step 1：先在真 Host/FakeDaemon fixture 写：用 Launch 生成热 thread，等完成并成功回报，然后 `drivers.clear()`；下一次 tick 捕获 params.loaded_threads 包含原 ID，不要求 DriverState 重新伪造 Launch。补 idle/busy/AwaitingApproval 三态、热表移除后完整空数组。
- [ ] Step 2：跑 `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib kanban_observation_ -- --nocapture`，旧生产者没有字段时行为红。不要只测试人工 Vec 编码。
- [ ] Step 3：在 session_driver.rs 实现唯一热快照函数，排序 ID；不读磁盘，不读 plugin metadata：

```rust
pub(crate) fn loaded_threads_snapshot(
    threads: &std::collections::HashMap<String, crate::session::ThreadSession>,
) -> Vec<LoadedThreadFacts> {
    let mut out: Vec<_> = threads.iter().map(|(id, session)| {
        let status = *session.status.lock().unwrap_or_else(|p| p.into_inner());
        LoadedThreadFacts {
            thread_id: id.clone(),
            idle: matches!(status, crate::protocol::ThreadStatus::Idle)
                && session.active_turn_id.is_none(),
        }
    }).collect();
    out.sort_by(|a,b| a.thread_id.cmp(&b.thread_id));
    out
}
pub(crate) fn loaded_threads_payload(threads: &[LoadedThreadFacts]) -> serde_json::Value {
    serde_json::Value::Array(threads.iter().map(|fact| serde_json::json!({
        "thread_id":fact.thread_id,"idle":fact.idle
    })).collect())
}
```

- [ ] Step 4：driver_tick 在每个插件 launcher 构造之前采 loaded_threads；legacy idle 快照由该 owned Vec 导出，使同拍相同 ID 使用同一 idle 读数，sessions.running 保持现有独立 conservative 统计。把 loaded_threads 传入 driver_step 的最后参数，编码请求新增一行：

```rust
"loaded_threads": loaded_threads_payload(loaded_threads),
```

采样是同一主循环处理阶段，不承诺 status mutex 对多个会话具有原子全局时刻。不要移到 launcher 之后借用 &mut threads；不要把热 Vec 塞进 DriverState 记账或 results。
- [ ] Step 5：`git grep -n 'driver_step(' -- yi-agent-rs/crates/yi-agent-app-server/src` 找齐调用；生产调用传真实 snapshot，旧 unit fake 明确传 `&[]` 或命名 LoadedThreadFacts fixtures，不能为了编译把生产传空数组。共读 request test 验证新增字段，legacy_request test 固定缺字段兼容；已有 query 失败保留 results 测试继续通过。
- [ ] Step 6：K17 使用已有 IPC client frame limit 用例增加 loaded_threads 大数组请求（构造最终 envelope 超过 1,048,576 字节），必须传输错误而不是服务端收 truncated 数组；宿主 failed query test 验 results 未清/launcher 未调用。无需造上万个 ThreadSession 或提高限额。
- [ ] Step 7：运行新 tests、`cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib session_driver`，及 IPC crate 全测。记录 K1/K3/K4/K5/K17；统一门禁提交 `fix(app-server): report the complete loaded-thread snapshot`。

### Task 5: list、CLI 与随包说明使用统一观测投影

**Files:** Modify `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/observation.rs`、`plugins/superpowers-kanban/INSTALL.md`、`plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`；Test service/list 与 main/command_list tests。

**Interfaces:** Consumes Task 2 for_card/presentation_at；Consumes Task 3 已接线的 list `session_observation`（无执行 ID 为 null）、`observed_at`（unknown 为 null）；Produces CLI 展示及 list/CLI 一致性测试，旧 stale/last_alive_at 保留。

- [ ] Step 1：在 observation tests 用已定义 at(day,hour)、Board fixture 测三态投影；main 的既有 command_list fixture 中写旧 false 和新 not_loaded 分别验 `[observation unknown]`、`[stale]` 的语义；service list 与 CLI 不重复判断 alive。

```rust
#[test]
fn kanban_observation_cli_legacy_does_not_claim_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let mut board = superpowers_kanban_core::board::Board::new();
    let now = chrono::Local::now();
    let id = superpowers_kanban_core::card::CardId::new("c");
    board.enqueue(id.clone(),"a.spec.md".into(),"a.plan.md".into(),now);
    board.transition(&id,superpowers_kanban_core::card::CardState::Running).unwrap();
    board.set_thread_id(&id,"t".into()).unwrap();
    superpowers_kanban_runner::persist::save_board(&dir.path().join("board.json"),&board).unwrap();
    std::fs::write(dir.path().join("observations.json"),
        r#"{"threads":{"t":{"alive":false}}}"#).unwrap();
    let text = command_list(dir.path(),false).unwrap();
    assert!(text.contains("[observation unknown]"));
    assert!(!text.contains("会话已不在"));
}
```

- [ ] Step 2：运行 `cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-runner kanban_observation_ -- --nocapture`，确认旧 CLI 的 legacy stale 输出与新 unknown 期望冲突。
- [ ] Step 3：list 和 CLI 都通过 for_card 派生态，non-running 不显示加载异常；不抹 last_alive_at，unknown 时间为 null。CLI not_loaded 格式 `[stale] 当前宿主未加载（截至 <RFC3339>；历史可能仍存在）`；unknown 格式 `[observation unknown] 当前宿主未观测到（历史可能仍存在）`；loaded 不加角标。保持 [archived]/[merging]/顶层 merge attempt 行，不改 action_id 提示。
- [ ] Step 4：skill 与 INSTALL 修旧死亡/最后活动表述，新增升级矩阵：新 UI+旧 runner 仅文案修正，新 runner+旧宿主不保证热发现，旧 UI 保留旧硬编码而非声称已修，三者升级才完整验收；旧 daemon inode 重启、新 skill 生效时间沿用既有指南。不要自动执行 on/off/state/restart。
- [ ] Step 5：runner 全测与 `the_skill` 契约测试通过；K6/K8/K10/K12 的 CLI 路径和 null/invalid metadata 保持不崩溃；统一门禁提交 `fix(kanban): describe stale cards as host loading observations`。

### Task 6: 桌面归一化、诚实角标与日期

**Files:** Create `desktop/src/lib/kanbanObservation.ts`、`desktop/src/lib/kanbanObservation.test.ts`；Modify `desktop/src/lib/superpowersKanbanState.ts`、`desktop/src/lib/superpowersKanbanState.test.ts`、`desktop/src/components/SuperpowersKanbanCard.tsx`、`desktop/src/components/SuperpowersKanbanCard.test.tsx`。

**Interfaces:** BoardCard 增加 `sessionObservation?: "loaded" | "not_loaded" | "unknown"`、`observedAt?: string`；缺字段仍为 undefined，不预先归一化 unknown 而失去旧 stale 来源。新 helper：

```typescript
export function observationDate(value: string | undefined): string | null;
export function observationNotice(card: BoardCard): {
  label: string; title: string; tone: "warning" | "neutral";
} | null;
```

observedAt 原样 optionalString；session_observation 缺键保留 undefined，有键但 null/坏类型/未知值归一化为 unknown，不能丢失新格式来源并退回旧 stale。helper 使用严格有效时间决定是否能称已观测。`observationDate` 必须包含 year/month/day/hour/minute 与明确时区说明或 offset，固定 `zh-CN`，不依赖纯 HH:mm。

- [ ] Step 1：先写组件旧 stale 文案回归和已 loaded 覆盖旧 stale 的测试：

```tsx
it("kanban_observation_legacy_warning_does_not_claim_deletion", () => {
  render(<SuperpowersKanbanCard card={card({
    state:"running",threadId:"t",stale:true,
    lastAliveAt:"2026-11-02T08:35:13+08:00"
  })}/>);
  const badge = screen.getByLabelText("会话未观测到");
  expect(badge.getAttribute("title")).toContain("历史可能仍存在");
  expect(screen.queryByText(/最后活动/)).toBeNull();
  expect(screen.getByText(/最后观测到已加载/)).toBeTruthy();
});
it("kanban_observation_loaded_wins_over_legacy_stale", () => {
  render(<SuperpowersKanbanCard card={card({
    state:"running",threadId:"t",stale:true,sessionObservation:"loaded",
    observedAt:"2026-11-03T10:00:00+08:00"
  })}/>);
  expect(screen.queryByLabelText("会话未加载")).toBeNull();
  expect(screen.queryByLabelText("会话未观测到")).toBeNull();
});
```

- [ ] Step 2：跑 `cd desktop && npx vitest run src/components/SuperpowersKanbanCard.test.tsx`，先看到旧 label/旧时间文案行为红，再补类型和 helper。
- [ ] Step 3：observationNotice 顺序固定：非 running 或无 nonblank 执行 ID→null；合法新态且有效 observedAt→loaded null/not_loaded warning；有新字段但坏时间/unknown态→unknown neutral；缺新字段且旧 stale=true→legacy warning label“会话未观测到”；缺新字段且旧 stale=false/缺省→neutral unknown（有执行绑定才显示）。不能以 originThreadId 生成异常。
  - not_loaded title 为“截至 <包含日期的时间>，当前宿主未加载该会话；会话历史可能仍存在。卡片仍为 running，需检查实际进度。”
  - unknown title 为“会话观测未知；历史可能仍存在”，无伪截至。
  - legacy title 为“当前宿主未观测到该会话；会话历史可能仍存在”。
  - 最后 true 时间仅有有效 lastAliveAt 时显示“最后观测到已加载 <日期时间>”，不是最后活动；观察警告/未知时显示即可，不为 loaded 增加噪声行。
- [ ] Step 4：组件只 render helper 返回值，不重复用 card.stale 覆盖新态。点击原执行链接仍只调用 onOpenThread(originalId)；保持原合并角标和 shrink/truncate classes。不新增状态迁移按钮、自动 resume effect 或消息发送。
- [ ] Step 5：补 helper 和归一化测试：三态、旧 raw list、未知 enum、null、坏日期、跨日、缺执行 ID、仅 origin ID、非 running、conflicting stale。日期断言验证包含年份/月日而不是依赖测试机器的本地 HH:mm；时间 formatter 使用显式 UTC 时区并在字符串末尾带 UTC，避免 jsdom locale 环境不稳定。接受 RFC3339 的 Z 或 ±HH:mm，校验月日真实存在、时分秒与 offset 范围后才 Date.parse；日期正则匹配不能替代闰年/月末检查，2026-02-30 必须返回 null。原会话点击测试断言 ID 不变。
- [ ] Step 6：运行 `cd desktop && npx vitest run src/lib/kanbanObservation.test.ts src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx && npx tsc --noEmit`；删 helper loaded 优先分支时冲突测试应红，恢复后全绿。K8/K10/K12 覆盖归一化到组件的真实载荷。更新模块为前后端已接线但仍待真机/系统回归；统一门禁提交 `fix(desktop): show precise kanban loading observations`。

### Task 7: 真恢复、空 driver 记账及资源回归串联

**Files:** Test `yi-agent-rs/crates/yi-agent-app-server/src/server/session_driver_tests.rs`、`yi-agent-rs/crates/yi-agent-app-server/src/server/tests/thread_lifecycle.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`；允许为 fixture 复用修改测试可见性，不改普通 resume 生产路径。

**Interfaces:** Consumes Tasks 1–6。已有 fixture 是 Host::tick、KanbanProject、FakeDaemon::bind_at、install_driver_plugin 和 server::tests 的 Harness/rpc/initialize。把 `FakeDaemon`、`bind_at`、`calls_to`、`install_driver_plugin` 最小扩为 `pub(super)`，供 sibling tests 引用；不把它们移到生产模块。Harness 自有 `resident_dir: TempDir`，不用改 HOME。

- [ ] Step 1：在 existing kanban_original_session_hot_round_trip 的相同建仓/daemon/Host 隔离 setup 后增加独立用例；首两拍启动并回报，随后执行下列实际断言段：

```rust
let before = project.card(&service);
let tid = before["thread_id"].as_str().unwrap().to_string();
drivers.clear();
host.tick(resident_dir.path(), &mut drivers, &build_test_agent).await;
let after = project.card(&service);
assert_eq!(after["thread_id"],tid);
assert_eq!(after["state"],before["state"]);
assert_eq!(after["workdir"],before["workdir"]);
assert_eq!(after["detail"],before["detail"]);
assert_eq!(after["session_observation"],"loaded");
assert_eq!(after["stale"],false);
assert_eq!(host.threads.len(),1);
```

完整 setup 使用同文件 KanbanProject::new/enqueue/card、BoardService::new(Some(temp_home))、FakeDaemon::bind_at 转发真实 dispatch、block_delegation_for、Host::new、install_driver_plugin。给用例命名 `kanban_observation_empty_driver_state_keeps_loaded_card`。不把 fake daemon 改为自行返回 loaded 结果。
- [ ] Step 2：普通 RPC 用例 `kanban_observation_plain_rpc_resume_is_discovered` 放到 thread_lifecycle.rs。用临时项目、本地 ThreadStore::create 与合法 `.jsonl`（可以合法空日志）、thread_id=`thread-observation-cold`；meta 构造沿用该文件既有 lifecycle tests 的 ThreadMeta 字段，不丢 ID/cwd/driver_project/ref。board 人工预置 running+同 ID+workdir（fixture 数据，不是生产卡）；service 注入独立 temp home；FakeDaemon 转发真实 dispatch。
  - Harness::with_config(cfg.workdir=temp_project, build_test_agent, PERMISSION_TIMEOUT) 后，在其 resident_dir 下安装 driver plugin；initialize。
  - 首个有界 tick 使 list=not_loaded，证明启动时热表确实空。
  - RPC 走真实普通入口而不是 resume_append：

```rust
let resumed = rpc(&mut h,2,"thread/resume",serde_json::json!({
    "threadId":"thread-observation-cold"
})).await;
assert!(resumed.get("error").is_none(),"{resumed}");
let deadline = tokio::time::Instant::now() + EVENT_WAIT;
loop {
    let listed = service.list();
    if listed["cards"][0]["session_observation"] == "loaded" {
        assert_eq!(listed["cards"][0]["thread_id"],"thread-observation-cold");
        assert_eq!(listed["cards"][0]["state"],"running");
        break;
    }
    assert!(tokio::time::Instant::now()<deadline,"resume was not observed: {listed}");
    tokio::time::sleep(Duration::from_millis(20)).await;
}
h.shutdown().await;
```

  捕获 FakeDaemon 原请求用于断言 loaded_threads 真由 serve 发出、results 没有伪造 Launch；用计数 mock Provider 证明恢复及 tick 没增加模型调用，用现有计数 provider fixture 的同构最小 struct（只计一次真正 complete 调用）而非检查字幕猜没有 turn。schema/meta-only 的严格合并恢复规则不适用于普通 RPC，这里实际创建合法日志避免分歧。
- [ ] Step 3：热移除 ID 但保留历史的 Host 用例，list 变 not_loaded，而历史字节/卡绑定不变；多项目两个 service 同宿主 snapshot、自身绑定/无关 ID 去重；归档热卡纳入租约，purge 无 attempt 引用时旁路清理；有 attempt 引用时不得清理。测 budget=0 的重启补账以及同卡重复 loaded tick 只一份 lease。
- [ ] Step 4：系统 red-green：分别临时删除宿主 loaded_threads 字段编码、把 plugin normalize 改为忽略 complete，运行 K1/K2/K3 对应 tests，必须出现预期 not_loaded/unknown 与 loaded 不等的行为红；恢复两处并重跑全绿。只改测试 worktree 产品变异，禁止提交变异，禁止覆盖用户并发修改。
- [ ] Step 5：运行 app-server filtered tests、session_driver 回归、runner 全测、desktop Task 6 测试；记录名字/数量/退出码。避免 cargo --workspace 全量，统一 CI 自有 nextest 配置除外。更新 K1–K17 映射中实际通过项；真机未验收前 bug 仍 [ ]。
- [ ] Step 6：统一门禁并提交 `test(kanban): cover plain resume and forgotten driver state`。

### Task 8: 文档结算、混装与隔离真机验收

**Files:** Modify `docs/project-management/superpowers-kanban.md`、`docs/project-management/README.md`、`docs/bug-list.md`、本 spec/plan 的实际验收记录；如 fixture 增加新测试，在 Task 7 所列测试文件提交，不修改生产状态文件。

**Interfaces:** Consumes 所有交付；Produces K1–K17 证据表、独立实例验收记录。只有相关 tests 与真机全部通过才把 bug/feature 勾 [x] 并将索引 19/21 更新为实际计数（本特性完成后预计 20/21；实施时按实际条目重算）。

- [ ] Step 1：更新文档前核对 pending/results 故障测试记录、模块实际 feature 计数与实现位置。保存一张证据表：case、测试函数/命令、退出码、对应 commit、真机是否通过；未跑不得写通过。
- [ ] Step 2：执行完整 mock 门禁并记录日志：

```sh
(cd yi-agent-rs && cargo fmt --all && just ci) &&
(cd plugins/superpowers-kanban && cargo fmt --all -- --check && cargo test -p superpowers-kanban-runner && cargo test -p superpowers-kanban-ipc) &&
(cd desktop && npx tsc --noEmit && npx vitest run) &&
git diff --check
```

运行全量桌面 tests，含看板原会话链接和合并角标；CI 若缺 ignored real keys 不去调用。检查 rust-version/lock 无变动，新增 DTO 不需要改 protocol.rs 或 ts-rs generated；不得借本任务引入无关 bindings 漂移。
- [ ] Step 3：混装 fixture 测试矩阵：新 runner 接 legacy_request、旧 list 载荷喂新 UI、含新字段请求喂未接 loaded 的旧判定替身、缺新字段 typed driver_step wrapper。验证不崩溃/不丢 result，明确旧 UI 文案不在保证中，不能为“混装全绿”删除限制造假。
- [ ] Step 4：按项目 CLAUDE.md 的独立 App 打包流程：新 bundle identifier、绝对 YI_AGENT_HOME、worktree sidecar 资源、先 npm build 再 tauri bundle、sidecar→desktop→app 三层签名。用独立插件二进制/skill 与独立项目状态目录，必须核对进程参数没有生产 relay/session 串用；不直接覆盖 /Applications/yi-agent.app 或 /opt/homebrew/bin/superpowers-kanban。
- [ ] Step 5：隔离实例人工验收：合法卡与历史准备后重启宿主，观察原卡未加载语义；手动打开原 thread，下一成功 tick 不再 warning；没有额外 turn/新 ID；热表移除但保留历史后只提示未加载。再用旧载荷确认新 UI 只说未观测到。记录完整日期标签、原会话按钮、当前宿主范围；过程不修改生产卡、不为清角标写 done。无真机可用时状态保持未完成并列出缺失验收，不宣称完成。
- [ ] Step 6：证据齐全后更新 docs 和计数，执行统一门禁，提交 `docs(kanban): record loading-observation repair verification`。只在获授权后使用 finishing-a-development-branch 流程整合；若是看板卡执行，本会话仅 merge-request 等 daemon 持名额，不绕过 merge.lock 自己 merge。
- [ ] Step 7：合并结果再 `just ci`、runner/IPC tests、桌面 tests。确认无需增量开发后按项目要求先 dry-run/看占用/确认无编译进程，再清自己的 target 和 worktree，不删除别人的开发树。

## Spec 覆盖与复核

| Spec | 任务 |
|---|---|
| §1–4 范围、观测不推断状态、宿主通用边界 | Global Constraints、Tasks 3/4/7 |
| §5 完整/旧请求、严格解析、帧限额 | Tasks 1/4/8；K9/K17 |
| §6 相关 ID、来源、归一化、保存、租约 | Tasks 2/3/7；K3/K7/K11/K14/K15/K16 |
| §7 list/CLI/UI/skill、日期、原 ID 链接 | Tasks 5/6/8；K8/K10/K12 |
| §8 查询与保存失败、旧数据、无自动恢复 | Tasks 2/3/4/7；K6/K9/K10/K14 |
| K1 | Tasks 4/7：空 DriverState 真 tick |
| K2 | Task 7：普通 RPC 实际恢复 + serve tick |
| K3 | Tasks 2/3/4：complete 覆盖陈旧子集 |
| K4 | Tasks 4/7：idle/busy/AwaitingApproval |
| K5 | Tasks 4/7：移除热表保留历史 |
| K6 | Tasks 2/5/7：旧 false / complete=[] / unknown |
| K7 | Tasks 2/3/7：跨项目无关 ID |
| K8 | Tasks 1/5/6/8：混装承诺与实际边界 |
| K9 | Tasks 1/3：非法请求不消费 |
| K10 | Tasks 2/4/5/6：坏新字段、查询与日期 |
| K11 | Tasks 3/7：一份 lease、budget=0、attempt gate |
| K12 | Tasks 2/5/6：轮询不是活动、跨日标签 |
| K13 | Tasks 2/4/7：分别删除生产/消费的行为变异 |
| K14 | Task 3：真实 I/O 前置失败与重试 |
| K15 | Tasks 1/2/6：missing alive 与 per-ID 降级 |
| K16 | Tasks 3/7：matched result、origin、归档/purge |
| K17 | Tasks 4/8：frame 错误不截断 |

计划自检：没有空实现/稍后决定的接口；Task 2 实现 normalize 后 Task 3 接线并产生 list 新字段，Task 5 不重复延后该依赖；Task 4 宿主与插件 LoadedThreadFacts 各自定义，不新增生产耦合；Task 5/6 共用同一加载语义，Task 7 明确不是 resume_append 替代普通 RPC；Task 8 真机未通过不勾完成。实施时每 Task 实际文件和 tests 仍需现场确认，不能照计划宣称已运行。

## 执行交接

本轮 spec 已完成多轮修订，plan 已写，所有任务仍未执行。可以另行选择 subagent-driven-development（逐任务两阶段审查）或 executing-plans（分批执行与检查点）。未获实施授权前不开始代码、不提交为已修复、不把本计划自动加入看板。
