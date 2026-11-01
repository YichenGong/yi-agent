# 休眠配置进设置界面 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在「运行参数」页新增「电源与休眠」分组，把保活的两个时长参数（条件消失后的宽限窗口、远端交互有效窗口）开放给用户配置，同时把总开关从 `agent.prevent_idle_sleep` 迁移到 `power.prevent_idle_sleep`。

**Architecture:** 自下而上四层：(1) `yi-agent-power::policy` 的 `Policy` 从「读全局 `GRACE` 常量」改成「持有两个 `Duration` 字段」，`Supervisor` 转发 `set_windows`；(2) `yi-agent-runtime::settings` 新增 `PowerSettings` 结构与两个 `FieldType::Number` 字段，`load_from` 在 `serde_json::Value` 阶段搬旧键（反序列化之后旧键已被 serde 静默丢弃，无处可搬）；(3) `yi-agent-app-server` 主循环每拍 `load_from` 一次，把两个窗口经 `Arc<AtomicU64>` 写给保活线程，`remote_touch_busy` 加第三参数；(4) 前端三张分组表加一行、`VALUE_LABELS` 加一条。旧键迁移只搬内存、不落盘。

**Tech Stack:** Rust 2024、`serde` / `serde_json`、`tokio`（主循环 tick）、`std::sync::atomic::{AtomicBool, AtomicU64}`、React + TypeScript + Vitest（前端）。

**设计文档:** `docs/superpowers/specs/2026-11-01-power-settings-ui-design.md`（806 行；所有 `file:line` 引用以其为准）

## Global Constraints

以下约束对**每一个** Task 都成立，不再逐条重复：

- **测试名必须带指定前缀。** `cargo test -p <crate> <filter>` 是**测试名子串匹配**，且 cargo 对 0 匹配的过滤词**照样 exit 0、不报错**。所以一个被遗漏的测试在 CI 里完全静默——比测试失败更危险。本 plan 里每条新测试的名字都在 Task 中逐字给出，**不要改写**。
- **提交前必须跑 `cd yi-agent-rs && cargo fmt --all && just ci`。** 这是项目唯一 CI 入口（GitHub PR 与 GitLab MR 都只跑它），涵盖 12 道门禁：`fmt-check`、`lint`（`clippy --all-targets`，含测试代码）、`test`、`build`、`bindings-check`、`secret-check` 等。少跑一次就可能让 `main` 变红。
- **Commit message 用 conventional commits**，首行 ≤72 字符。**不要**写 `Co-Authored-By` 行。
- **禁止在 `main` 上直接提交。** 所有改动在本 worktree 的分支 `docs/power-settings-ui-design` 上进行。
- **`set_windows` / `set_enabled` / `set_activity` 只许 `store`，不许阻塞。** 它们由主循环在每一拍调用，一旦阻塞就卡住所有 RPC。
- **键名必须恰好是 `power.<field>`（两层）。** `settings.rs` 的 `apply_op` 用 `key.split_once('.')` 拆 `group` / `field`，多一层点会让写入路径与读取路径不一致。
- **旧键字符串 `"agent.prevent_idle_sleep"` 只允许出现在两个地方：** `migrate_legacy_keys` 函数体内（作为**字面量**，不得引用 `PREVENT_IDLE_SLEEP_KEY` 常量），以及测试的文件内容里。全仓其它地方不许出现。自查：`grep -rn 'agent\.prevent_idle_sleep' crates/`（预期只剩这两类）。
- **`Policy` 的 `Default` 必须手写。** 派生 `Default` 会把两个新 `Duration` 字段置为 **0 秒**，`Policy::default()` 就变成「立刻放弃保活」且**不报错**。必须从 `#[derive(...)]` 里删掉 `Default` 并手写 `impl Default` 转调 `new()`。
- **`normalized()` 里的 `clamp!` 是逐字段列举的**（`settings.rs:473-490` 共 7 处，只覆盖 `Number` 字段）。新增两个数值字段**必须自己加两行 `clamp!`**，漏了不报错、越界值会一路进策略层。

## 文件结构

| 文件 | 职责 | 本 plan 的动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-power/src/policy.rs` | 纯策略状态机，零 I/O | 参数化两个窗口；手写 `Default`；`Idle` 时清 `last_ok`；+6 测试 |
| `yi-agent-rs/crates/yi-agent-power/src/supervisor.rs` | 10s tick 串联 clock/probe/assertion | 加 `set_windows` 转发；+1 测试 |
| `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs` | 设置的读写、校验、归一化、字段元数据 | 新增 `PowerSettings`；`power_windows_minutes`；常量；`clamp!` ×2；倒挂回落；迁移函数；`FIELDS` 三项；删旧字段；`write_to` 跨字段校验；改 6 条既有测试；+9+3 测试 |
| `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` | 运行时常量 + 默认值漂移守卫 | 默认值测试 `cases` 加两行 |
| `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs` | RPC 读设置 | `current_value` 加三分支、改一分支 |
| `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs` | busy 三路并集 | `remote_touch_busy` 加第三参数；4 处测试补参；+1 测试 |
| `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs` | 保活线程（唯一持有 IOKit 断言） | 两个 `AtomicU64`；`spawn` 签名；`set_windows`；`slack_for`；2 处测试补参 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 主循环装配 | `spawn` 调用点 + `power_ticker` 分支接线 |
| `yi-agent-rs/crates/yi-agent-web/src/api.rs` | web 端配置只读接口 | `field_value` 加三分支、改一分支（**无测试守卫**） |
| `desktop/src/components/SettingsRuntimeTab.tsx` | 设置页渲染 | 三张表加 `power`；`VALUE_LABELS` 加一条 |
| `desktop/src/components/SettingsRuntimeTab.test.tsx` | 前端测试 | 改 `GROUP_ORDER` 断言；+1 `labelFor` 测试 |
| `docs/project-management/*.md` | 项目进度 | 更新对应模块与 README 计数（见 Task 11） |

**依赖顺序：** Task 1（policy）→ Task 2（supervisor）→ Task 3~5（settings）→ Task 6（两个 RPC 读点）→ Task 7（power_busy）→ Task 8（power_guard）→ Task 9（server 接线）→ Task 10（前端）→ Task 11（文档与收尾）。Task 3 是 Task 4/5 的前置；Task 8 依赖 Task 3 的 `power_windows_minutes`；Task 9 依赖 Task 7/8。

---

### Task 1: `Policy` 参数化两个窗口

把 `policy.rs` 里写死的 `GRACE` / `WATCHDOG_SLACK` 改成结构体字段，同时修掉一个既有缺陷：`decide` 返回 `Idle` 时不清 `last_ok`，会让「调长窗口」把已释放的 hold 复活。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/policy.rs:4-5`（常量）、`:40-49`（结构体与 `new`）、`:62-93`（`decide`）、`:96-...`（测试模块）
- Test: `yi-agent-rs/crates/yi-agent-power/src/policy.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: 无（本 crate 零依赖，`Snapshot` / `Decision` / `Sample` 均为本文件既有类型）
- Produces:
  - `pub const DEFAULT_GRACE: Duration = Duration::from_secs(15 * 60);`
  - `pub const DEFAULT_WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);`
  - `pub const GRACE: Duration = DEFAULT_GRACE;`（别名，让既有 16 条测试零改动）
  - `pub const WATCHDOG_SLACK: Duration = DEFAULT_WATCHDOG_SLACK;`（同上）
  - `Policy::new() -> Self`（语义不变：默认窗口）
  - `Policy::with_windows(grace: Duration, watchdog_slack: Duration) -> Self`
  - `Policy::set_windows(&mut self, grace: Duration, watchdog_slack: Duration)`

- [ ] **Step 1: 写 6 条失败测试**

在 `policy.rs` 的 `#[cfg(test)] mod tests` 内、**先加一个夹具**，再追加测试。`tests` 模块已有 `at` / `frozen_mono` / `ac_online` 三个夹具，但 **`ac_online` 不能用来测释放**：

```rust
    fn ac_online(busy: bool) -> Snapshot {
        Snapshot { on_ac: true, online: true, busy }
    }
```

`keeps_awake = online && (on_ac || busy)`，`ac_online` 的 `on_ac` 恒为 `true`，于是**无论 `busy` 是什么都为真**、每拍都刷新 `last_ok`、永远不会释放。用 `ac_online(false)` 写「条件消失」的断言会得到恒 `Hold`，测试永远绿——假通过。

要测释放必须换成「电池 + 有网」：此时 `keeps_awake = busy`，`busy=false` 才真的代表「活干完了」。所以**新增这个夹具**：

```rust
    /// 电池 + 有网：`keeps_awake == busy`。测「条件消失」只能用这个——
    /// `ac_online` 的 `on_ac` 恒为真，`busy` 参不参与判定，永远 Hold。
    fn battery(busy: bool) -> Snapshot {
        Snapshot {
            on_ac: false,
            online: true,
            busy,
        }
    }
```

然后追加 7 条测试：

```rust
    // ---- 窗口参数化（本设计新增） ----

    #[test]
    fn a_custom_grace_releases_at_its_own_boundary() {
        // 正常时钟：mono 与 wall 同步前进，看门狗分支不参与。
        let mut p = Policy::with_windows(Duration::from_secs(300), Duration::from_secs(300));
        // 首拍必须有活，`last_ok` 才会被写入——`battery(false)` 在这一拍是 Idle。
        assert_eq!(p.decide(&battery(true), at(0)), Decision::Hold);
        // 条件消失后走到第 299 秒仍在宽限内。
        assert_eq!(p.decide(&battery(false), at(299)), Decision::Hold);
        // 第 300 秒到期。
        assert_eq!(p.decide(&battery(false), at(300)), Decision::Idle);
    }

    /// 看门狗只在「单调钟冻结」的路径上生效，因此**必须**用 `frozen_mono()`。
    /// 用 `at()` 时 `mono_grew` 恒为真，看门狗分支根本不执行，测试会假通过。
    ///
    /// 断言点是宽限 60 分钟 + slack 20 分钟 = 80 分钟（4800 秒）。
    /// 不要写成「60 分钟就释放」——那是错的。
    #[test]
    fn the_watchdog_scales_with_the_grace_window() {
        let mut p = Policy::with_windows(Duration::from_secs(3600), Duration::from_secs(1200));
        // mono 冻结在 0，墙钟照走。首拍建立 hold_since = (mono 0, wall 0)。
        assert_eq!(p.decide(&battery(true), frozen_mono(0, 0)), Decision::Hold);
        // 墙钟到 4800 仍未严格超过 grace + slack。
        assert_eq!(
            p.decide(&battery(false), frozen_mono(0, 4800)),
            Decision::Hold
        );
        // 4801 严格大于 4800，看门狗强制释放。
        assert_eq!(
            p.decide(&battery(false), frozen_mono(0, 4801)),
            Decision::Idle
        );
    }

    #[test]
    fn shrinking_the_grace_releases_an_in_flight_hold_immediately() {
        let mut p = Policy::with_windows(Duration::from_secs(3600), Duration::from_secs(1200));
        assert_eq!(p.decide(&battery(true), at(0)), Decision::Hold);
        // 空跑 20 分钟：条件已消失，但仍在 3600 秒宽限内。
        assert_eq!(p.decide(&battery(false), at(1200)), Decision::Hold);
        p.set_windows(Duration::from_secs(300), Duration::from_secs(300));
        // 1201 - 0 = 1201 >= 300，下一拍立即释放（窗口不缓存）。
        assert_eq!(p.decide(&battery(false), at(1201)), Decision::Idle);
    }

    /// 对照断言：条件仍在时缩窄窗口**不得**释放。
    /// 防止有人为了「让缩窄立刻生效」而在忙时错误释放。
    #[test]
    fn shrinking_the_grace_keeps_holding_while_work_is_running() {
        let mut p = Policy::with_windows(Duration::from_secs(3600), Duration::from_secs(1200));
        assert_eq!(p.decide(&battery(true), at(0)), Decision::Hold);
        assert_eq!(p.decide(&battery(true), at(1200)), Decision::Hold);
        p.set_windows(Duration::from_secs(300), Duration::from_secs(300));
        // 每拍刷新 last_ok，与窗口大小无关。
        assert_eq!(p.decide(&battery(true), at(1201)), Decision::Hold);
    }

    #[test]
    fn the_grace_window_accepts_its_documented_limits() {
        // 下限 1 分钟。
        let mut low = Policy::with_windows(Duration::from_secs(60), Duration::from_secs(300));
        assert_eq!(low.decide(&battery(true), at(0)), Decision::Hold);
        assert_eq!(low.decide(&battery(false), at(59)), Decision::Hold);
        assert_eq!(low.decide(&battery(false), at(60)), Decision::Idle);

        // 上限 120 分钟。
        let mut high = Policy::with_windows(Duration::from_secs(7200), Duration::from_secs(2400));
        assert_eq!(high.decide(&battery(true), at(0)), Decision::Hold);
        assert_eq!(high.decide(&battery(false), at(7199)), Decision::Hold);
        assert_eq!(high.decide(&battery(false), at(7200)), Decision::Idle);
    }

    /// 给「手写 `Default` 后忘了转调 `new()`」兜底：
    /// 若 `Default` 把宽限置 0，`battery(true)` 在 `at(0)` 同拍就会 idle
    /// （`0 - 0 < 0` 为假），这条测试立刻红。
    #[test]
    fn default_windows_match_the_historical_constants() {
        assert_eq!(GRACE, DEFAULT_GRACE, "the GRACE alias must not drift");
        assert_eq!(
            WATCHDOG_SLACK, DEFAULT_WATCHDOG_SLACK,
            "the WATCHDOG_SLACK alias must not drift"
        );
        assert_eq!(DEFAULT_GRACE, Duration::from_secs(15 * 60));
        assert_eq!(DEFAULT_WATCHDOG_SLACK, Duration::from_secs(5 * 60));

        let mut p = Policy::default();
        assert_eq!(p.decide(&battery(true), at(0)), Decision::Hold);
        // 第 899 秒仍在 15 分钟宽限内。
        assert_eq!(p.decide(&battery(false), at(899)), Decision::Hold);
        // 第 900 秒到期。
        assert_eq!(p.decide(&battery(false), at(900)), Decision::Idle);
    }

    /// 「调长窗口」不得让一个**已经释放**的 hold 复活。
    /// 这条钉住 `Idle` 分支里的 `self.last_ok = None;`。
    #[test]
    fn lengthening_the_window_does_not_revive_a_released_hold() {
        let mut p = Policy::with_windows(Duration::from_secs(300), Duration::from_secs(300));
        assert_eq!(p.decide(&battery(true), at(0)), Decision::Hold);
        // 400 >= 300，已释放。
        assert_eq!(p.decide(&battery(false), at(400)), Decision::Idle);
        p.set_windows(Duration::from_secs(3600), Duration::from_secs(1200));
        // 401 - 0 < 3600 本可再次成立，但 last_ok 已在 Idle 时清空，故仍是 Idle。
        assert_eq!(p.decide(&battery(false), at(401)), Decision::Idle);
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-power --lib policy
```

预期：编译失败，报 `no function or associated item named 'with_windows'` / `'set_windows'` / `cannot find value 'DEFAULT_GRACE' in this scope` / `no variant or associated item named 'default'`（`Policy::default()` 若 `Default` 派生仍在则能编译，但 `at(899)` 那条会跑出 `Idle` 而失败）。

- [ ] **Step 3: 改常量与结构体**

把 `policy.rs:4-5` 的两行：

```rust
pub const GRACE: Duration = Duration::from_secs(15 * 60);
pub const WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);
```

替换为：

```rust
/// 默认宽限：条件消失后再扛 15 分钟。
pub const DEFAULT_GRACE: Duration = Duration::from_secs(15 * 60);
/// 默认看门狗余量：见 `Policy::decide` 的看门狗分支。
pub const DEFAULT_WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);

/// 历史别名，保留是为了让既有的 `GRACE + WATCHDOG_SLACK` 计算（测试内两处）
/// 与语义引用不必改动。值必须与 `DEFAULT_*` 完全一致，有测试钉住。
pub const GRACE: Duration = DEFAULT_GRACE;
pub const WATCHDOG_SLACK: Duration = DEFAULT_WATCHDOG_SLACK;
```

把 `policy.rs:40-49`：

```rust
#[derive(Debug, Default)]
pub struct Policy {
    last_ok: Option<Duration>,
    hold_since: Option<Sample>,
    disabled: bool,
}

impl Policy {
    pub fn new() -> Self {
        Self::default()
    }
```

替换为：

```rust
#[derive(Debug)]
pub struct Policy {
    /// 条件消失后还能扛多久。由调用方按设置下发。
    grace: Duration,
    /// 看门狗的额外余量。跟随 `grace` 缩放，推导在 `power_guard::slack_for`。
    watchdog_slack: Duration,
    last_ok: Option<Duration>,
    hold_since: Option<Sample>,
    disabled: bool,
}

impl Default for Policy {
    /// **必须手写。** 派生 `Default` 会把两个 `Duration` 字段置 0，
    /// 于是 `Policy::default()` 变成「立刻放弃保活」——而且不报错。
    fn default() -> Self {
        Self::new()
    }
}

impl Policy {
    /// 默认窗口。既有测试全部走这条路，源码零改动。
    pub fn new() -> Self {
        Self::with_windows(DEFAULT_GRACE, DEFAULT_WATCHDOG_SLACK)
    }

    pub fn with_windows(grace: Duration, watchdog_slack: Duration) -> Self {
        Self {
            grace,
            watchdog_slack,
            last_ok: None,
            hold_since: None,
            disabled: false,
        }
    }

    /// 运行时换窗口。**下一拍 `decide` 就按新值算**，不缓存、
    /// 不做「值变了才生效」的判断——`decide` 每拍现读字段。
    pub fn set_windows(&mut self, grace: Duration, watchdog_slack: Duration) {
        self.grace = grace;
        self.watchdog_slack = watchdog_slack;
    }
```

- [ ] **Step 4: 改 `decide` 的两处常量引用与 `Idle` 分支**

`decide` 内改写两处判定，并把尾部 `match` 的 `Idle` 分支补上清 `last_ok`：

```rust
    pub fn decide(&mut self, snapshot: &Snapshot, now: Sample) -> Decision {
        if self.disabled {
            return Decision::Idle;
        }

        // 看门狗：扛了多久由**墙钟**度量（单调钟冻结时它照样前进），
        // 「有没有前进」由**单调钟**判定。
        if let Some(since) = self.hold_since {
            let held_for = now.wall.saturating_sub(since.wall);
            let mono_grew = now.mono > since.mono;
            if !mono_grew && held_for > self.grace + self.watchdog_slack {
                self.last_ok = None;
                self.hold_since = None;
                return Decision::Idle;
            }
        }

        if snapshot.keeps_awake() {
            self.last_ok = Some(now.mono);
        }

        let hold = self
            .last_ok
            .is_some_and(|last| now.mono.saturating_sub(last) < self.grace);
        let decision = if hold { Decision::Hold } else { Decision::Idle };

        match (decision, self.hold_since) {
            (Decision::Hold, None) => self.hold_since = Some(now),
            // `last_ok` 也要清：否则「已释放 → 用户调长窗口」会让 hold 复活。
            // 重新有活时 `keeps_awake` 那行会立刻重新写入，故清空不影响正常路径。
            (Decision::Idle, _) => {
                self.hold_since = None;
                self.last_ok = None;
            }
            (Decision::Hold, Some(_)) => {}
        }
        decision
    }
```

**不要**顺手改看门狗分支里的 `self.last_ok = None;`——那是刻意的（时钟故障必须彻底重置）。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-power --lib policy
```

预期：`22 passed`（既有 16 + 新增 6；本 Task 实际写了 7 条，其中 `shrinking_the_grace_keeps_holding_while_work_is_running` 是 spec §7.1 第 3 条要求的对照断言，故总数 23 —— 以实际输出为准，**0 failed** 即可）。

同时确认既有 16 条**一条都没被跳过**：

```bash
cargo test -p yi-agent-power --lib policy 2>&1 | grep -E "^test result|running"
```

预期 `running 23 tests` 且 `0 failed`。若 `running` 是 16 或 17，说明新测试名写错或没被 `mod tests` 收进来。

- [ ] **Step 6: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/policy.rs
git commit -m "feat(power): 保活策略的宽限与看门狗余量参数化

Policy 从「读全局 GRACE 常量」改为持有两个 Duration 字段，新增
with_windows / set_windows。decide 每拍现读字段，故窗口缩短在下一拍
即生效，不需要额外代码。

Default 改为手写并转调 new()：派生 Default 会把两个 Duration 置 0，
让 Policy::default() 变成「立刻放弃保活」且不报错。

同时修掉一处既有缺陷：返回 Idle 时原先只清 hold_since、不清 last_ok，
于是「5 分钟下已释放 → 用户把宽限改成 60 分钟 → 立刻重新 Hold」。
用户刚放过一次睡眠，配置变更不该追溯性地重新占住它。清空后对既有
测试无影响——重新有活时 keeps_awake 那行会立刻写回 last_ok。"
```

---

### Task 2: `Supervisor::set_windows` 转发

`Supervisor` 是主循环与 `Policy` 之间的唯一通道；线程每拍需要下发窗口。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/supervisor.rs:17-35`（`new` 之后加方法）、`:...`（测试模块）
- Test: `yi-agent-rs/crates/yi-agent-power/src/supervisor.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `Policy::set_windows(grace, watchdog_slack)`
- Produces: `Supervisor::set_windows(&mut self, grace: Duration, watchdog_slack: Duration)`

- [ ] **Step 1: 写失败测试**

在 `supervisor.rs` 的 `#[cfg(test)] mod tests` 内、最后一个测试之后追加：

```rust
    /// 下发更宽的窗口后，原本该释放的时刻不再释放——证明
    /// `set_windows` 真的转到了 `Policy` 上，而不是被吞掉。
    #[test]
    fn set_windows_reaches_the_policy() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), false, true, Arc::clone(&assertion));
        // 电池 + 有活 → 持有。
        sup.tick(true, true);
        assert!(sup.is_holding());

        // 活干完，推进到默认 15 分钟宽限的边界之后。
        clock.advance(Duration::from_secs(901));
        sup.tick(false, true);
        assert!(
            !sup.is_holding(),
            "default 15-minute grace must release at 901s"
        );

        // 换 60 分钟窗口，重新有活，再空跑 20 分钟：仍应持有。
        sup.set_windows(Duration::from_secs(3600), Duration::from_secs(1200));
        clock.advance(Duration::from_secs(10));
        sup.tick(true, true);
        assert!(sup.is_holding());
        clock.advance(Duration::from_secs(1200));
        sup.tick(false, true);
        assert!(
            sup.is_holding(),
            "with a 60-minute grace, 20 minutes of idle must still hold"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-power --lib supervisor
```

预期：编译失败，`no method named 'set_windows' found for struct 'Supervisor'`。

- [ ] **Step 3: 实现转发**

在 `supervisor.rs` 的 `impl Supervisor` 内、`pub fn new(...)` 之后（`pub fn tick` 之前）插入：

```rust
    /// 运行时换窗口。由 `PowerGuard` 的线程每拍下发，值与设置保持一致。
    ///
    /// 只改 `Policy` 的字段，不触发任何 I/O；断言是否释放仍由下一拍的
    /// `tick` 决定——所以「缩短宽限立即生效」不需要在这里补逻辑。
    pub fn set_windows(&mut self, grace: Duration, watchdog_slack: Duration) {
        self.policy.set_windows(grace, watchdog_slack);
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-power
```

预期：全 crate 通过（`policy` 23 + `supervisor` 既有 8 + 新增 1 + 其余模块），`0 failed`。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/supervisor.rs
git commit -m "feat(power): Supervisor 转发 set_windows 到 Policy

保活线程需要每拍把设置里的两个窗口下发给策略层。只改字段、不触发
I/O，是否释放仍由下一拍的 tick 决定。"
```

---

### Task 3: `settings.rs` 的 `PowerSettings`、常量与生效窗口函数

先把数据形状立起来，让后续 Task 有地方挂字段。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs:79-84`（`Settings`）、`:96-103`（删 `AgentSettings.prevent_idle_sleep`）、`:174`（常量值）、`:447-551`（`normalized`）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub struct PowerSettings { pub prevent_idle_sleep: Option<String>, pub idle_grace_minutes: Option<u64>, pub remote_touch_minutes: Option<u64> }`
  - `Settings.power: PowerSettings`
  - `pub const POWER_IDLE_GRACE_MINUTES_DEFAULT: u64 = 15;`
  - `pub const POWER_REMOTE_TOUCH_MINUTES_DEFAULT: u64 = 5;`
  - `pub fn power_windows_minutes(settings: &Settings) -> (u64, u64)` — 返回 `(grace, remote)`，**只兜 `None`、不兜越界**
  - `pub const PREVENT_IDLE_SLEEP_KEY: &str = "power.prevent_idle_sleep";`

- [ ] **Step 1: 写 5 条失败测试**

在 `settings.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
    #[test]
    fn power_windows_default_to_fifteen_and_five() {
        let (grace, remote) = power_windows_minutes(&Settings::default());
        assert_eq!(grace, POWER_IDLE_GRACE_MINUTES_DEFAULT);
        assert_eq!(remote, POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
        assert_eq!((grace, remote), (15, 5));
    }

    /// 两个值各自合法但倒挂（宽限 5、远端 30）→ 两个都回默认。
    ///
    /// 只清远端会留下「宽限 5 单独生效」，用户看到的仍是自己设的 5 分钟，
    /// 而文件里那对矛盾值会一直在，每次启动 warn 一次。两个一起回默认，
    /// 行为单调且可解释。
    #[test]
    fn power_windows_inverted_pair_falls_back_to_both_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}"#,
        )
        .unwrap();
        let (grace, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!(grace, POWER_IDLE_GRACE_MINUTES_DEFAULT);
        assert_eq!(remote, POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    }

    /// 相等是合法的：远端窗口在宽限到期的同一刻失效，语义自洽。
    #[test]
    fn power_windows_allow_equal_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"power":{"idle_grace_minutes":10,"remote_touch_minutes":10}}"#,
        )
        .unwrap();
        let (grace, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!((grace, remote), (10, 10));
    }

    #[test]
    fn power_windows_read_through_from_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"power":{"idle_grace_minutes":45,"remote_touch_minutes":20}}"#,
        )
        .unwrap();
        let (grace, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!((grace, remote), (45, 20));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib power_windows
```

预期：编译失败，`cannot find function 'power_windows_minutes'` / `no field 'power' on type 'Settings'` / `cannot find value 'POWER_IDLE_GRACE_MINUTES_DEFAULT'`。

- [ ] **Step 3: 加常量、`PowerSettings` 与 `Settings.power`**

在 `settings.rs` 的 `PREVENT_IDLE_SLEEP_KEY` **之前**加常量与结构体：

```rust
/// 「条件消失后再保活多久」的默认值，单位分钟。
///
/// 与字段语义同单位（分钟），秒数换算只在取用窗口时做一次。
/// 单位真实值 15 分钟 = `yi_agent_power::policy::DEFAULT_GRACE`，
/// 但本 crate 不依赖 `yi-agent-power`（二者互不依赖），故此处独立定义，
/// 由 `power_windows_minutes` 的测试与 `Policy` 的测试各自钉住。
pub const POWER_IDLE_GRACE_MINUTES_DEFAULT: u64 = 15;

/// 「远端交互有效窗口」的默认值，单位分钟。对应 300 秒。
pub const POWER_REMOTE_TOUCH_MINUTES_DEFAULT: u64 = 5;

/// 电源与休眠设置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerSettings {
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// 存字符串而不是 `Option<bool>`：`FIELDS` 的 `Select` 只接受 JSON
    /// 字符串（`validate_value`），用 `Option<bool>` 会让设置页写进
    /// `"true"` 后反序列化读不回来。与 `tui.notifications` 完全同构。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub prevent_idle_sleep: Option<String>,
    /// 条件消失后再保活多久（分钟）。范围 1–120。
    #[serde(default, deserialize_with = "de_optional_u64")]
    pub idle_grace_minutes: Option<u64>,
    /// 最近一次远端交互在多长时间内仍算「有活」（分钟）。范围 1–60。
    #[serde(default, deserialize_with = "de_optional_u64")]
    pub remote_touch_minutes: Option<u64>,
}
```

在 `struct Settings` 内加字段（放在 `tui` 之后，与 `FIELDS` 的组顺序一致）：

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub agent: AgentSettings,
    pub compact: CompactSettings,
    pub startup: StartupSettings,
    pub tui: TuiSettings,
    pub power: PowerSettings,
}
```

在 `struct AgentSettings` 内**删除** `prevent_idle_sleep` 字段及其上方三行文档注释：

```rust
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// 存字符串而不是 `Option<bool>`：`FIELDS` 的 `Text`/`Select` 只接受 JSON
    /// 字符串（`validate_value`），用 `Option<bool>` 会让设置页写进 `"true"`
    /// 后反序列化读不回来。与 `tui.notifications` 完全同构。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub prevent_idle_sleep: Option<String>,
```

- [ ] **Step 4: 改常量值、读取函数与 `power_windows_minutes`**

把 `PREVENT_IDLE_SLEEP_KEY` 的值改成新键（变量名不变，4 处引用自动跟随）：

```rust
/// 开关的点路径键。多处引用同一常量，避免字符串抄错。
///
/// **它从此只表示新键。** 迁移函数里的旧键必须写成字面量
/// `"agent.prevent_idle_sleep"`，不能引用本常量——否则将来再改常量，
/// 迁移就会跟着漂移、静默失效。
pub const PREVENT_IDLE_SLEEP_KEY: &str = "power.prevent_idle_sleep";
```

把 `prevent_idle_sleep_enabled` 里读的字段换到 `power`：

```rust
pub fn prevent_idle_sleep_enabled(settings: &Settings) -> bool {
    match settings.power.prevent_idle_sleep.as_deref() {
        Some(value) => !value.eq_ignore_ascii_case("false"),
        None => true,
    }
}
```

在同文件、`prevent_idle_sleep_enabled` 之后加 `power_windows_minutes`：

```rust
/// 两个窗口的实际生效值（分钟），返回 `(grace, remote)`。
///
/// **只兜 `None`，不兜越界**——越界值由 `normalized()` 的 `clamp!` 先行清空，
/// 调用方必须先取得经过 `normalized()` 的 `Settings`。`load_from` 的返回值
/// 已经过；`write_to` 里要自己先调 `.normalized()`。
///
/// 不做 slack 推导，也不做单位换算：slack 是策略层概念，放在
/// `power_guard::slack_for`；分钟到秒的换算由调用点乘 60。
pub fn power_windows_minutes(settings: &Settings) -> (u64, u64) {
    let grace = settings
        .power
        .idle_grace_minutes
        .unwrap_or(POWER_IDLE_GRACE_MINUTES_DEFAULT);
    let remote = settings
        .power
        .remote_touch_minutes
        .unwrap_or(POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    (grace, remote)
}
```

- [ ] **Step 5: 在 `normalized()` 里加两行 `clamp!` 与倒挂回落**

在 `normalized()` 内既有 7 处 `clamp!` 之后（`clamp!("compact.tool_budget_tokens", ...)` 那块的收尾之后）追加：

```rust
        clamp!("power.idle_grace_minutes", self.power.idle_grace_minutes);
        clamp!("power.remote_touch_minutes", self.power.remote_touch_minutes);
```

**这两行此刻还不生效，是正常的。** `clamp!` 内部调 `within(key, value)`，而 `within` 靠 `find_spec(key)` 取 `min`/`max`；`FIELDS` 里还没有 `power.*` 两个条目（Task 5 Step 3 才加），`find_spec` 返回 `None` 时 `within` 直接返回 `true`，于是越界值不会被清掉。

这不是缺陷，只是**依赖顺序**：`clamp!` 的行现在写上，问责令测试（`power_windows_out_of_range_fall_back_to_defaults`）放在 Task 5 的 Step 6，那里 `FIELDS` 已补齐、测试才有意义。**不要**为了让 Task 3 的测试变绿而提前在 `FIELDS` 里加字段——`FIELDS` 一改，`settings_rpc` 的读回守卫会立刻变红，而那属于 Task 6。

再在 `normalized()` 内、把原先校验 `self.agent.prevent_idle_sleep` 的那段改为 `self.power`，warn 文案同步改键名：

```rust
        if let Some(enabled) = self.power.prevent_idle_sleep.as_deref() {
            if !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false") {
                tracing::warn!(
                    value = enabled,
                    "settings.json has an unknown power.prevent_idle_sleep; falling back to the default"
                );
                self.power.prevent_idle_sleep = None;
            }
        }
```

最后在 `normalized()` 的**末尾**（尾表达式 `self` 之前）加倒挂回落：

```rust
        // 跨字段：远端窗口长于宽限会让「宽限」在电池场景下失去意义——
        // 用户以为「没活 15 分钟就放睡」，实际因为远端窗口 30 分钟一直在扛。
        // `clamp!` 管不到这个：两个值各自都在范围内，倒挂仍然成立。
        if let (Some(remote), Some(grace)) =
            (self.power.remote_touch_minutes, self.power.idle_grace_minutes)
        {
            if remote > grace {
                tracing::warn!(
                    remote,
                    grace,
                    "power.remote_touch_minutes exceeds power.idle_grace_minutes; falling back to the defaults"
                );
                self.power.remote_touch_minutes = None;
                self.power.idle_grace_minutes = None;
            }
        }
```

**必须两个都清空**，不是只清远端（理由见 spec §5.3 末段）。

- [ ] **Step 6: 修 `FIELDS` 里那一项的 group 与 description**

`FIELDS` 里 `PREVENT_IDLE_SLEEP_KEY` 那项的 `group` 改成 `"power"`，`description` 去掉写死的「15 分钟」（15 已可变），`label` / `default_display` / `options` 不动：

```rust
    FieldSpec {
        key: PREVENT_IDLE_SLEEP_KEY,
        group: "power",
        label: "干活期间防止休眠",
        field_type: FieldType::Select,
        default_display: "true",
        description: "有网且（插电 或 有活在跑）时阻止系统空闲休眠；条件消失后按「保活窗口」再等一段时间",
        options: &["true", "false"],
        min: None,
        max: None,
    },
```

- [ ] **Step 7: 改 4 条既有测试的字段路径**

`settings.rs` 里 4 条直连结构体的测试要把 `settings.agent.prevent_idle_sleep` 换成 `settings.power.prevent_idle_sleep`：

- `prevent_idle_sleep_honours_false`
- `prevent_idle_sleep_is_case_insensitive`
- `an_unknown_prevent_idle_sleep_falls_back_to_enabled`（断言侧）
- `an_empty_prevent_idle_sleep_is_unset`（断言侧）

**输入文件的键名保留旧的 `agent.`**（那是迁移的输入端，改成 `power.` 就测不到迁移了）——这两条的完整改法在 Task 5。本 Step 只做「能编译」的机械替换：

```rust
    #[test]
    fn prevent_idle_sleep_honours_false() {
        let mut settings = Settings::default();
        settings.power.prevent_idle_sleep = Some("false".to_string());
        assert!(!prevent_idle_sleep_enabled(&settings));
    }

    #[test]
    fn prevent_idle_sleep_is_case_insensitive() {
        for raw in ["false", "FALSE", "False"] {
            let mut settings = Settings::default();
            settings.power.prevent_idle_sleep = Some(raw.to_string());
            assert!(!prevent_idle_sleep_enabled(&settings), "{raw} must disable");
        }
    }
```

另外两条（`an_unknown_prevent_idle_sleep_falls_back_to_enabled`、`an_empty_prevent_idle_sleep_is_unset`）要连同断言侧一起改。**这两条在本 Task 结束时就会绿，但那不代表迁移做对了**：此时 `AgentSettings` 已删掉该字段、`Settings` 又没有 `deny_unknown_fields`，serde 会静默忽略文件里的 `agent.prevent_idle_sleep`，于是 `power` 侧自然是 `None`、断言照样成立。它们真正验证迁移是在 Task 4/5（那时输入仍是旧键名，但旧值必须被搬过来）。

所以本 Step 只做「编译通过 + 语义更新」，不要误以为「这两条绿了 = 迁移好了」。

```rust
    /// 未知值在 `normalized` 里被清成 `None`，于是读取端按默认（开）处理。
    /// 文件用的是**旧键名**——那正是迁移的输入端。
    #[test]
    fn an_unknown_prevent_idle_sleep_falls_back_to_enabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":"yes"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep, None);
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// 空串等于未设置。
    #[test]
    fn an_empty_prevent_idle_sleep_is_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":""}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep, None);
    }
```

- [ ] **Step 8: 修 `default_display` 测试的 `cases`**

`settings.rs` 的 `default_display_matches_the_runtime_constants` 与 `config.rs` 的 `settings_metadata_defaults_track_these_constants` 都是**逐项列举**的 `cases` 数组——新字段不加进 `cases` 就不会被检查。本 Task 先加两条数值字段（`power.prevent_idle_sleep` 的 `default_display` 是 `"true"`，与 `options[0]` 一致，不加进 `cases`；若想一并钉住可加，见下）。

在两个测试的 `use crate::config::{...}` / `cases` 里各加两行。`settings.rs` 侧：

```rust
        use crate::config::{
            COMPACT_RATIO_DEFAULT, COMPACT_TOOL_BUDGET_DEFAULT, COMPACT_USER_BUDGET_DEFAULT,
            DIRECT_CHILDREN_DEFAULT, MAX_TURNS_DEFAULT, RESIDENT_SUBAGENTS_DEFAULT,
            SKILLS_CATALOG_BUDGET_DEFAULT,
        };
        use crate::settings::{
            POWER_IDLE_GRACE_MINUTES_DEFAULT, POWER_REMOTE_TOUCH_MINUTES_DEFAULT,
        };
        let cases: &[(&str, String)] = &[
            ("agent.max_turns", MAX_TURNS_DEFAULT.to_string()),
            (
                "agent.max_direct_children",
                DIRECT_CHILDREN_DEFAULT.to_string(),
            ),
            (
                "agent.max_resident_subagents",
                RESIDENT_SUBAGENTS_DEFAULT.to_string(),
            ),
            (
                "agent.skills_catalog_budget",
                SKILLS_CATALOG_BUDGET_DEFAULT.to_string(),
            ),
            ("compact.ratio", COMPACT_RATIO_DEFAULT.to_string()),
            (
                "compact.user_budget_tokens",
                COMPACT_USER_BUDGET_DEFAULT.to_string(),
            ),
            (
                "compact.tool_budget_tokens",
                COMPACT_TOOL_BUDGET_DEFAULT.to_string(),
            ),
            (
                "power.idle_grace_minutes",
                POWER_IDLE_GRACE_MINUTES_DEFAULT.to_string(),
            ),
            (
                "power.remote_touch_minutes",
                POWER_REMOTE_TOUCH_MINUTES_DEFAULT.to_string(),
            ),
        ];
```

`config.rs` 侧同样在 `cases` 末尾加那两项，并在 `use crate::settings::find_spec;` 之后补：

```rust
        use crate::settings::{
            POWER_IDLE_GRACE_MINUTES_DEFAULT, POWER_REMOTE_TOUCH_MINUTES_DEFAULT,
        };
```

- [ ] **Step 9: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib power_windows
cargo test -p yi-agent-runtime --lib default_display
cargo test -p yi-agent-runtime --lib settings_metadata_defaults
```

预期：`power_windows` 前缀 5 条全过；`default_display` 1 条过；`settings_metadata_defaults` 1 条过。`0 failed`。

- [ ] **Step 10: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(settings): 新增 power 分组的数据形状与生效窗口函数

PowerSettings 持有开关与两个时长（分钟）。power_windows_minutes 只兜
None、不兜越界，越界由 normalized 的 clamp! 先行清空——所以本 Task
同时把那两行 clamp! 加上，否则越界值会一路进策略层。

跨字段倒挂（远端窗口 > 宽限）在 normalized 里两个一起回默认：只清
远端会留下「宽限单独生效」，而那对矛盾值会一直留在文件里，每次启动
warn 一次。

开关常量 PREVENT_IDLE_SLEEP_KEY 的值改为 power.prevent_idle_sleep，
四处引用自动跟随。"
```

---

### Task 4: `migrate_legacy_keys` —— 在原始 JSON 上搬键

迁移必须做在 `serde_json::Value` 阶段。反序列化之后旧键已被 serde **静默丢弃**（`Settings` 上没有 `deny_unknown_fields`），无处可搬。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs:404-434`（`load_from`）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: Task 3 的 `PowerSettings`
- Produces: `fn migrate_legacy_keys(value: &mut serde_json::Value)`（模块内私有）

- [ ] **Step 1: 写 3 条纯 JSON 单测**

迁移函数纯操作 JSON、零 I/O，直接单测比绕 `load_from` 造临时文件清晰得多。在 `mod tests` 内追加：

```rust
    #[test]
    fn migrate_moves_the_legacy_key_into_power() {
        let mut value = serde_json::json!({"agent":{"prevent_idle_sleep":"false"}});
        migrate_legacy_keys(&mut value);
        assert_eq!(value["power"]["prevent_idle_sleep"], "false");
        // 旧键必须被删掉，否则幂等性靠的是「新键已存在」这个间接条件。
        assert!(
            value["agent"].get("prevent_idle_sleep").is_none(),
            "the legacy key must be removed, got {value}"
        );
        // agent 变空壳则整组删掉，不留 {"agent":{}} 噪音。
        assert!(
            value.get("agent").is_none(),
            "an emptied agent group must be pruned, got {value}"
        );
    }

    #[test]
    fn migrate_keeps_an_explicit_power_key() {
        let mut value = serde_json::json!({
            "agent": {"prevent_idle_sleep": "false"},
            "power": {"prevent_idle_sleep": "true"}
        });
        migrate_legacy_keys(&mut value);
        // 新键优先：不能被旧的 false 覆盖。
        assert_eq!(value["power"]["prevent_idle_sleep"], "true");
    }

    #[test]
    fn migrate_twice_is_a_no_op() {
        let mut value = serde_json::json!({"agent":{"prevent_idle_sleep":"false"}});
        migrate_legacy_keys(&mut value);
        let once = value.clone();
        migrate_legacy_keys(&mut value);
        assert_eq!(value, once, "migration must be idempotent");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib migrate_
```

预期：编译失败，`cannot find function 'migrate_legacy_keys' in this scope`。

- [ ] **Step 3: 实现迁移函数**

在 `settings.rs` 里 `load_from` **之前**加：

```rust
/// 把旧键 `agent.prevent_idle_sleep` 搬到 `power.prevent_idle_sleep`。
///
/// **必须做在 `serde_json::Value` 上，不能等反序列化之后。**
/// `Settings` 上没有 `deny_unknown_fields`，`AgentSettings` 里删掉该字段后，
/// serde 会静默忽略文件里的旧键——反序列化之后旧值**已经不存在**，无处可搬。
///
/// 三条约束：
/// 1. 只在新键缺失时迁移，否则用户显式设的 `power` 值每次启动都被旧值覆盖；
/// 2. 不落盘（只在内存里搬），避免擅自改用户文件；
/// 3. 幂等——旧键被删掉，同一份 `Value` 上再跑是空操作。
///
/// 旧键字符串是**字面量**，不得引用 `PREVENT_IDLE_SLEEP_KEY`：那个常量
/// 已经指向新键，将来再改值会让迁移跟着漂移、静默失效。
fn migrate_legacy_keys(value: &mut serde_json::Value) {
    const LEGACY_GROUP: &str = "agent";
    const LEGACY_FIELD: &str = "prevent_idle_sleep";
    const NEW_GROUP: &str = "power";

    let existing = value
        .get(LEGACY_GROUP)
        .and_then(|group| group.get(LEGACY_FIELD))
        .cloned();
    let Some(legacy) = existing else {
        return;
    };

    // 新键已存在就不动：用户显式设的值优先。
    let already_set = value
        .get(NEW_GROUP)
        .and_then(|group| group.get(LEGACY_FIELD))
        .is_some();
    if already_set {
        return;
    }

    let Some(object) = value.as_object_mut() else {
        return;
    };
    object
        .entry(NEW_GROUP.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("just inserted an object")
        .insert(LEGACY_FIELD.to_string(), legacy);

    // 删旧键；agent 变空壳则整组删掉，与 `write_to` 的空组清理同口径。
    let agent_now_empty = match object
        .get_mut(LEGACY_GROUP)
        .and_then(|group| group.as_object_mut())
    {
        Some(group) => {
            group.remove(LEGACY_FIELD);
            group.is_empty()
        }
        None => false,
    };
    if agent_now_empty {
        object.remove(LEGACY_GROUP);
    }
}
```

- [ ] **Step 4: 在 `load_from` 里接上迁移**

`load_from` 的 `value` 要改成 `mut`（迁移就地修改它），并在 `if !value.is_object()` 之后、`from_value` 之前插入调用。改后形状：

```rust
    let mut value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "invalid settings.json; using defaults");
            return Settings::default();
        }
    };
    if !value.is_object() {
        tracing::warn!(path = %path.display(), "settings.json is not a JSON object; using defaults");
        return Settings::default();
    }

    // 旧键在反序列化之后就不存在了，必须在这里搬。
    migrate_legacy_keys(&mut value);

    let settings = match serde_json::from_value::<Settings>(value) {
        Ok(settings) => settings,
        Err(error) => {
            // 到这一步只剩结构性的坏（比如 `"agent": 5`）。字段级的类型不符
            // 已被 `de_optional_*` 吸收成 `None`，不会走到这里。
            tracing::warn!(error = %error, path = %path.display(), "invalid settings.json; using defaults");
            return Settings::default();
        }
    };
    settings.normalized()
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib migrate
```

预期：本 Task 3 条 + Task 5 的 `migrate_` 前缀测试若已写则一并跑；此时至少 3 passed，`0 failed`。

再跑一遍 Task 3 里那两条原本会失败的测试：

```bash
cargo test -p yi-agent-runtime --lib prevent_idle_sleep
```

预期：6 条全过（迁移接上后，用旧键名造的文件能被正确搬过来）。

- [ ] **Step 6: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs
git commit -m "feat(settings): 在原始 JSON 上把旧开关键迁到 power 组

迁移必须做在 serde_json::Value 阶段：AgentSettings 删掉该字段后，
Settings 没有 deny_unknown_fields，serde 会静默忽略文件里的旧键，
反序列化之后就无处可搬了。

只在新键缺失时迁移（用户显式设的值优先）；不落盘（不擅自改用户
文件）；幂等（旧键被删掉）。旧键字符串写死在函数里做成字面量，
不引用 PREVENT_IDLE_SLEEP_KEY——那个常量已指向新键，引用它会让迁移
跟着常量漂移、静默失效。"
```

---

### Task 5: `FIELDS` 补两个数值字段、`write_to` 跨字段校验、迁移的端到端测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs:245-255`（`FIELDS`）、`:583-660`（`write_to`）、`:640-644`（空组清理列表）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: Task 3 的 `PowerSettings` / `power_windows_minutes`；Task 4 的 `migrate_legacy_keys`
- Produces: `FIELDS` 新增两项（`power.idle_grace_minutes`、`power.remote_touch_minutes`）；`write_to` 在倒挂时返回 `SettingsError::InvalidSetting { key: "power.remote_touch_minutes", .. }`

- [ ] **Step 1: 写 6 条端到端测试**

```rust
    fn write_json(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("settings.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// 造真实旧格式文件，走完整的「读 → 解析 → 迁移 → 反序列化 → normalized」。
    #[test]
    fn migrate_the_old_agent_key_into_the_power_group() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep.as_deref(), Some("false"));
        assert!(!prevent_idle_sleep_enabled(&settings));
    }

    #[test]
    fn migrate_twice_on_disk_is_stable() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        let first = load_from(&path);
        let second = load_from(&path);
        assert_eq!(first, second);
        assert_eq!(first.power.prevent_idle_sleep.as_deref(), Some("false"));
    }

    /// 两个键都在且冲突时，`power` 的值生效。
    #[test]
    fn migrate_keeps_an_explicit_power_key_on_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"agent":{"prevent_idle_sleep":"false"},"power":{"prevent_idle_sleep":"true"}}"#,
        );
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep.as_deref(), Some("true"));
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// 写入被拒时文件必须**逐字节不变**。
    /// 不断言 mtime：同一秒内两次写入的 mtime 可能一样，APFS 的粗粒度
    /// 时间戳会让这条断言假通过。字节比对才是「零写入」的真正判据。
    #[test]
    fn power_windows_reject_a_remote_window_longer_than_the_grace() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":5}}"#,
        );
        let before = std::fs::read(&path).unwrap();

        // 只 patch 宽限（5 → 60 会合法；这里相反：把宽限压到 2，远端 5 就倒挂了）。
        let patch = serde_json::json!({"power.idle_grace_minutes": 2});
        let error = write_to(&path, &patch).unwrap_err();
        match error {
            SettingsError::InvalidSetting { key, reason } => {
                assert_eq!(key, "power.remote_touch_minutes");
                assert!(
                    reason.contains("must not exceed"),
                    "reason should explain the cross-field rule, got {reason}"
                );
            }
            other => panic!("expected InvalidSetting, got {other:?}"),
        }

        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, before, "a rejected write must not touch the file");
    }

    /// 旧键不会因为「保存了别的字段」而消失——这是预期行为，不是 bug。
    /// `write_to` 只对 patch 里出现的键调 `apply_op`，其余原样保留。
    #[test]
    fn migrate_legacy_key_stays_on_disk_after_other_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        write_to(&path, &serde_json::json!({"power.idle_grace_minutes": 30})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("agent"),
            "the legacy key must survive an unrelated write, got {text}"
        );
        assert!(text.contains("prevent_idle_sleep"), "got {text}");
        // 而新值确实写进去了。
        assert!(text.contains("idle_grace_minutes"), "got {text}");
    }

    /// 迁移结果可重放：把迁好的 Settings 整份写回，再读一次必须相等。
    /// 若迁移只在内存搬、而值本身落不进新键，第二次读会退回默认（开）。
    #[test]
    fn migrate_value_survives_a_fresh_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        let first = load_from(&path);
        assert_eq!(first.power.prevent_idle_sleep.as_deref(), Some("false"));

        std::fs::write(&path, serde_json::to_string(&first).unwrap()).unwrap();
        let second = load_from(&path);
        assert_eq!(first, second, "a migrated value must survive a rewrite");
        assert_eq!(second.power.prevent_idle_sleep.as_deref(), Some("false"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib migrate
cargo test -p yi-agent-runtime --lib power_windows
```

预期：`power_windows_reject_a_remote_window_longer_than_the_grace` 失败——宽限 2、远端 5 的倒挂没有被拒绝，`write_to` 返回 `Ok(())`，`.unwrap_err()` panic。其余 5 条应已通过（Task 4 的迁移已就位）。

- [ ] **Step 3: 在 `FIELDS` 里加两个数值字段**

紧接 `power.prevent_idle_sleep` 那一项之后插入两项：

```rust
    FieldSpec {
        key: "power.idle_grace_minutes",
        group: "power",
        label: "条件消失后再保活多久（分钟）",
        field_type: FieldType::Number,
        default_display: "15",
        description: "有网且（插电 或 有活在跑）的条件消失后，继续阻止休眠这么久；范围 1–120",
        options: &[],
        min: Some(1),
        max: Some(120),
    },
    FieldSpec {
        key: "power.remote_touch_minutes",
        group: "power",
        label: "远端交互有效窗口（分钟）",
        field_type: FieldType::Number,
        default_display: "5",
        description: "手机等远端客户端最近一次请求在这么久之内，仍算作有活在跑；不得大于上面的保活窗口。范围 1–60",
        options: &[],
        min: Some(1),
        max: Some(60),
    },
```

- [ ] **Step 4: 把 `power` 加进 `write_to` 的空组清理列表**

```rust
    // 删掉只剩空壳的分组对象，避免留下 {"agent":{}} 这种噪音。
    for group in ["agent", "compact", "startup", "tui", "power"] {
```

- [ ] **Step 5: 在 `write_to` 里加跨字段校验**

`write_to` 的形状是「解析 patch → 逐项 `validate_value` → 读原文件成 object → 逐个 `apply_op` → 删空壳组 → 序列化写回」。跨字段检查必须插在 `apply_op` **之后**、空组清理**之前**（那一刻还没写盘，天然零写入）。

**先看清楚为什么不能在这里调 `normalized()`。** `normalized()` 自带倒挂回落（Step 5 之前的 Task 3 已经加了）：倒挂时它把两个值都清成 `None`。于是「先归一化、再比较」会得到 `power_windows_minutes` 的默认值 `(15, 5)`，`5 > 15` 恒假——**任何倒挂写入都不会被拒绝**，这段校验等于没写。实测确认过这条死逻辑。

正确做法：对合并后的两个字段各自做**单字段钳制**，不做倒挂回落。

在 `for (spec, value) in ops { apply_op(&mut object, spec.key, value); }` 之后插入：

```rust
    // 跨字段：远端窗口不得长于宽限窗口。`validate_value` 是逐字段无状态的，
    // 做不了这个判断，只能在这里做。
    //
    // 必须基于**合并后的完整对象**：用户可能只 patch 一个键（例如只把宽限改小），
    // 此时远端窗口的生效值来自文件里的旧值。
    //
    // 但**不能**在这里调 `normalized()`：它自带的倒挂回落会把倒挂值清成 None，
    // 于是下面的比较永远为假、任何倒挂都不会被拒绝。改为对两个字段各自做
    // 单字段钳制（`within` 按 FieldSpec.min/max 判界），越界即当默认。
    let merged_value = serde_json::Value::Object(object.clone());
    let merged = match serde_json::from_value::<Settings>(merged_value) {
        Ok(settings) => settings,
        Err(error) => {
            // `write_to` 只校验过「顶层是对象」，内层可能是结构性的坏
            // （比如 `"agent": 5`）。这与「文件存在却读不出来 → 拒绝覆盖」
            // 的守卫同向：宁可不写，也不把坏文件当默认值覆盖掉。
            return Err(SettingsError::Corrupt(format!(
                "the merged settings would not deserialize: {error}"
            )));
        }
    };

    // 只钳制、不做倒挂回落——否则本次要检查的倒挂会先被清掉。
    // 不复用 `power_windows_minutes`：它要一个完整的 `&Settings`，
    // 而这里手上只有两个 Option。直接算等价。
    let grace_minutes = merged
        .power
        .idle_grace_minutes
        .filter(|&value| within("power.idle_grace_minutes", value))
        .unwrap_or(POWER_IDLE_GRACE_MINUTES_DEFAULT);
    let remote_minutes = merged
        .power
        .remote_touch_minutes
        .filter(|&value| within("power.remote_touch_minutes", value))
        .unwrap_or(POWER_REMOTE_TOUCH_MINUTES_DEFAULT);

    // 只在本次 patch 触碰了这两个键时才拒绝。否则文件里既有的倒挂会让用户
    // 连改 `agent.max_turns` 都失败——那是把人锁在文件外面。文件里的既有
    // 倒挂由加载侧的 `normalized()` 静默回落并 warn，不影响写入。
    let touches_power_windows = patch.contains_key("power.idle_grace_minutes")
        || patch.contains_key("power.remote_touch_minutes");
    if touches_power_windows && remote_minutes > grace_minutes {
        return Err(SettingsError::InvalidSetting {
            key: "power.remote_touch_minutes".to_string(),
            reason: format!(
                "must not exceed power.idle_grace_minutes \
                 (the remote window {remote_minutes} is longer than the grace \
                 {grace_minutes}, which would make the grace ineffective on battery)"
            ),
        });
    }
```

三个细节必须对上：

- `patch` 在这里已被 `let patch = patch.as_object()...` 转成 `&serde_json::Map`，所以用 `patch.contains_key(..)` 而不是 `patch.get(..)`。
- `within` 是 `settings.rs` 既有的私有函数（`fn within(key: &str, value: u64) -> bool`），`find_spec` 未命中即返回 `true`；两个键都在 `FIELDS` 里，所以它按 `min`/`max` 判界。
- 这里**不建临时 `Settings`**，直接用 `unwrap_or(POWER_*_MINUTES_DEFAULT)` 回落到默认——与 `power_windows_minutes` 的语义等价，但不必为了调它构造一个只有 `power` 字段有值的结构体。

- [ ] **Step 6: 补 3 条测试：倒挂守卫的两侧 + `clamp!` 的问责令**

`touches_power_windows` 守卫是本节唯一容易漏的分支（两侧各一条），加上 Task 3 写好的 `clamp!` 现在才真正生效、需要一条问责令：

```rust
    /// `clamp!` 是逐字段列举的，新增字段漏了它不会报错、越界值会一路进
    /// 策略层。这条测试是那两个 `clamp!` 的唯一问责令。
    ///
    /// 它必须放在本 Task 而不是 Task 3：`within` 靠 `find_spec` 取 min/max，
    /// `FIELDS` 里没有 `power.*` 时 `within` 返回 true、`clamp!` 清不掉任何值。
    #[test]
    fn power_windows_out_of_range_fall_back_to_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        // 下限：0 被清成 None。
        std::fs::write(&path, r#"{"power":{"idle_grace_minutes":0}}"#).unwrap();
        let (grace, _) = power_windows_minutes(&load_from(&path));
        assert_eq!(
            grace, POWER_IDLE_GRACE_MINUTES_DEFAULT,
            "0 must be clamped away, not passed through"
        );

        // 上限：121 > 120 被清成 None。
        std::fs::write(&path, r#"{"power":{"idle_grace_minutes":121}}"#).unwrap();
        let (grace, _) = power_windows_minutes(&load_from(&path));
        assert_eq!(grace, POWER_IDLE_GRACE_MINUTES_DEFAULT);

        // 远端窗口上限 60。
        std::fs::write(&path, r#"{"power":{"remote_touch_minutes":61}}"#).unwrap();
        let (_, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!(remote, POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    }

    /// 文件里已有一对倒挂的手改值（宽限 5、远端 30）时，改**无关**字段
    /// 仍须成功。否则用户手改坏了一对值之后就再也存不进任何设置——
    /// 那是把人锁在文件外面。倒挂本身由加载侧 `normalized()` 回落。
    #[test]
    fn an_unrelated_write_succeeds_despite_an_inverted_pair_on_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}"#,
        );
        write_to(&path, &serde_json::json!({"agent.max_turns": 123})).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("123"), "the unrelated write must land, got {text}");
    }

    /// 但触碰这对窗口就被拒——哪怕只是其中一个键。
    #[test]
    fn touching_an_inverted_pair_is_still_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}"#,
        );
        let error = write_to(
            &path,
            &serde_json::json!({"power.idle_grace_minutes": 2}),
        )
        .unwrap_err();
        assert!(
            matches!(error, SettingsError::InvalidSetting { .. }),
            "touching the inverted pair must be rejected, got {error:?}"
        );
    }
```

- [ ] **Step 7: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib migrate
cargo test -p yi-agent-runtime --lib power_windows
cargo test -p yi-agent-runtime --lib prevent_idle_sleep
```

预期：三组全过（`migrate` 前缀 8 条、`power_windows` 前缀 6 条、`prevent_idle_sleep` 6 条），`0 failed`。

- [ ] **Step 8: 跑整个 runtime crate 的测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent-runtime
```

预期：全过。这一步会暴露「加字段导致既有测试夹具不完整」之类的问题。`0 failed`。

- [ ] **Step 9: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs
git commit -m "feat(settings): power 组的两个时长字段与跨字段校验

FIELDS 新增 idle_grace_minutes（1-120）与 remote_touch_minutes
（1-60），并在 write_to 的 apply_op 之后、写盘之前加跨字段检查：
远端窗口不得长于宽限窗口。

检查基于合并后的完整对象（用户可能只 patch 一个键，另一个的生效值
来自文件旧值）。这里**不能**先跑 normalized：它自带的倒挂回落会把
倒挂值清成 None，使比较永远为假、任何倒挂都不会被拒绝。改为对两个
字段各自做单字段钳制（within），越界即当默认。

只在本次 patch 触碰这两个键时才拒绝，否则文件里既有的倒挂会让用户
连给无关字段都存不进去。from_value 失败转 Corrupt 而不是 unwrap。

同时把 power 加进 write_to 的空组清理列表，否则会留下 {"power":{}}。"
```

---

### Task 6: 两个读设置的地方补 `power` 分支

`settings_rpc::current_value` 有守卫测试；`yi-agent-web::api::field_value` **没有任何测试**，漏改会静默返回 `null`。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs:46-67`（`current_value`）
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs:79-100`（`field_value`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs:119`（既有守卫，不改代码）

**Interfaces:**
- Consumes: Task 3 的 `PowerSettings` 字段
- Produces: 无（纯读路径）

- [ ] **Step 1: 先跑既有守卫，确认它此刻是红的**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib every_field_in_fields_can_be_read_back
```

预期：**FAILED**。因为 Task 5 已经往 `FIELDS` 加了两个新键，而 `current_value` 还没有对应分支，`_ => Value::Null` 兜底会让它们返回 null。报错文本形如：

```
power.idle_grace_minutes is in FIELDS but current_value returned null for it
```

这就是「先红后绿」的红。若它是绿的，说明 `FIELDS` 那两个字段没加上——回 Task 5 检查。

- [ ] **Step 2: 改 `settings_rpc::current_value`**

把 `PREVENT_IDLE_SLEEP_KEY` 那个分支改成读 `power`，并在其后加两个分支：

```rust
        settings::PREVENT_IDLE_SLEEP_KEY => {
            option_to_json(current.power.prevent_idle_sleep.as_deref())
        }
        "power.idle_grace_minutes" => option_to_json(current.power.idle_grace_minutes),
        "power.remote_touch_minutes" => option_to_json(current.power.remote_touch_minutes),
```

- [ ] **Step 3: 跑守卫确认变绿**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib every_field_in_fields_can_be_read_back
```

预期：1 passed。

- [ ] **Step 4: 改 `yi-agent-web::api::field_value`**

```rust
        settings::PREVENT_IDLE_SLEEP_KEY => {
            opt(current.power.prevent_idle_sleep.clone().map(Value::from))
        }
        "power.idle_grace_minutes" => opt(current.power.idle_grace_minutes.map(Value::from)),
        "power.remote_touch_minutes" => opt(current.power.remote_touch_minutes.map(Value::from)),
```

- [ ] **Step 5: 用 arm 计数自查两边是否一致**

**不要**用 `grep -c '=> opt('` / `grep -c '=> option_to_json('`：三个分支（`agent.max_resident_subagents`、`PREVENT_IDLE_SLEEP_KEY`、`tui.notification_condition`）的 `=> {` 与函数调用分处两行，单行 grep 会漏掉它们，实测只有 10 / 11。

```bash
cd yi-agent-rs
_ARMS='^[[:space:]]+("[a-z]+\.[a-z_]+"|settings::PREVENT_IDLE_SLEEP_KEY) =>'
grep -cE "$_ARMS" crates/yi-agent-web/src/api.rs
grep -cE "$_ARMS" crates/yi-agent-app-server/src/settings_rpc.rs
awk '/pub static FIELDS/,/^];/' crates/yi-agent-runtime/src/settings.rs | grep -c 'key:'
```

预期三个数字**都是 15**（改动前 13，本设计加两个数值字段后 15）。`FIELDS` 那一行是分母——若它不是 15，先回 Task 5 确认字段真加上了。

- [ ] **Step 6: 跑测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib settings_rpc
cargo test -p yi-agent-web
cargo build -p yi-agent-web
```

预期：`settings_rpc` 全过；`yi-agent-web` 的测试（若有）全过；`build` 通过。**注意 `yi-agent-web` 里没有测试**，所以 `cargo test -p yi-agent-web` 可能显示 0 tests——这不是「验过了」，web 侧的保证来自 Step 5 的计数。

- [ ] **Step 7: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs yi-agent-rs/crates/yi-agent-web/src/api.rs
git commit -m "feat(settings): 两个读设置的 match 补 power 分支

settings_rpc 有 every_field_in_fields_can_be_read_back 守卫，漏分支会红。
yi-agent-web 的 field_value 没有任何测试，兜底分支会让漏改静默返回
null——接口照样 200、页面照着渲染，只是字段永远显示「未设置」。故
这一处靠 arm 计数自查：三个数字必须都等于 FIELDS.len()（当前 15）。"
```

---

### Task 7: `remote_touch_busy` 的窗口参数化

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs:77-79`（函数）、`:388-430`（4 处测试调用点）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: 无
- Produces: `pub(crate) fn remote_touch_busy(last_remote_touch: Option<Stamp>, clock: &dyn Clock, window: Duration) -> bool`

- [ ] **Step 1: 写失败测试**

在 `power_busy.rs` 的 `mod tests` 内追加：

```rust
    /// 窗口真的被参数化了，而不是还在读 `REMOTE_TOUCH_WINDOW`。
    #[test]
    fn a_custom_remote_window_expires_at_its_own_boundary() {
        let stamp = Stamp::from_duration(Duration::ZERO);
        let window = Duration::from_secs(120);
        // 第 119 秒仍在窗口内。
        assert!(remote_touch_busy(
            Some(stamp),
            &FixedClock(Duration::from_secs(119)),
            window
        ));
        // 第 120 秒边界到期（判定取 `<`）。
        assert!(!remote_touch_busy(
            Some(stamp),
            &FixedClock(Duration::from_secs(120)),
            window
        ));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib power_busy
```

预期：编译失败，`this function takes 2 arguments but 3 arguments were supplied`（4 处既有调用点 + 新增这条都是 3 个参数）。

- [ ] **Step 3: 加第三参数**

```rust
/// (c) 远端近期交互。
///
/// 时间戳必须走 `clock` 的睡眠感知时钟：睡 8 小时后 `since()` 就是 8 小时，
/// 远超窗口，`remote_touch` 自动失效。**不能**用 `Instant::now()`——那只前进
/// 几分钟，会让一条早已失效的交互记录继续算作「刚刚有过交互」，使保活窗口
/// 形同虚设。
///
/// `window` 由设置下发（`power.remote_touch_minutes` × 60）；默认值见
/// `REMOTE_TOUCH_WINDOW`，供测试与装配回退引用。
pub(crate) fn remote_touch_busy(
    last_remote_touch: Option<Stamp>,
    clock: &dyn Clock,
    window: Duration,
) -> bool {
    last_remote_touch.is_some_and(|stamp| clock.since(stamp) < window)
}
```

- [ ] **Step 4: 给 4 处既有调用点补参**

前三处传 `REMOTE_TOUCH_WINDOW` 以保持原语义：

```rust
    #[test]
    fn remote_touch_expires_at_the_window_boundary() {
        let stamp = Stamp::from_duration(Duration::ZERO);
        // 边界取 `<`：恰好 5 分钟已过期。
        assert!(remote_touch_busy(
            Some(stamp),
            &FixedClock(Duration::from_secs(299)),
            REMOTE_TOUCH_WINDOW
        ));
        assert!(!remote_touch_busy(
            Some(stamp),
            &FixedClock(REMOTE_TOUCH_WINDOW),
            REMOTE_TOUCH_WINDOW
        ));
    }

    #[test]
    fn no_remote_touch_ever_is_not_busy() {
        assert!(!remote_touch_busy(
            None,
            &FixedClock(Duration::ZERO),
            REMOTE_TOUCH_WINDOW
        ));
    }

    #[test]
    fn a_local_stdio_client_does_not_keep_the_machine_awake() {
        let never_touched: Option<Stamp> = None;
        for secs in [0u64, 60, 3600, 8 * 3600] {
            assert!(
                !remote_touch_busy(
                    never_touched,
                    &FixedClock(Duration::from_secs(secs)),
                    REMOTE_TOUCH_WINDOW
                ),
                "a local-only session must never look like remote activity (t={secs}s)"
            );
        }
    }
```

**注意** `remote_touch_expires_at_the_window_boundary` 里 `FixedClock(REMOTE_TOUCH_WINDOW)` 那处是**时钟读数**（模拟「已过去 5 分钟」），不是窗口参数——两处都要传，但语义不同。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib power_busy
```

预期：全过（原 8 条 + 新增 1 条 = 9 条，以实际输出为准），`0 failed`。

- [ ] **Step 6: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs
git commit -m "feat(power): 远端交互窗口参数化

remote_touch_busy 加第三参数 window，由设置下发。REMOTE_TOUCH_WINDOW
保留为默认值，供测试与装配回退引用。"
```

---

### Task 8: `PowerGuard` 传两个窗口、推导 slack

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs:17-35`（结构体）、`:36-80`（`spawn`）、`:83-85`（`set_enabled` 之后）、`:125-170`（2 处测试调用点）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs`（同文件 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: Task 2 的 `Supervisor::set_windows(grace, watchdog_slack)`；`yi_agent_power::policy::{DEFAULT_GRACE, DEFAULT_WATCHDOG_SLACK}`
- Produces:
  - `PowerGuard::spawn(enabled: bool, grace_secs: u64, remote_secs: u64) -> Self`
  - `PowerGuard::set_windows(&self, grace_secs: u64, remote_secs: u64)`
  - `fn slack_for(grace: Duration) -> Duration`（模块内私有）

- [ ] **Step 1: 写 `slack_for` 的测试**

`slack_for` 是纯算术，单测成本极低。在 `power_guard.rs` 的 `mod tests` 内追加：

```rust
    /// `watchdog_slack = max(5min, grace/3)`。
    /// 下限 5 分钟的理由：宽限设成 1 分钟时，20 秒的 slack 会让看门狗
    /// 过于敏感——一次短暂的采样抖动就可能被判成时钟故障。
    #[test]
    fn slack_scales_with_the_grace_window() {
        let cases = [
            (1u64, 5u64),    // 1 分钟 → 下限
            (5, 5),          // 5 分钟 → 下限
            (15, 5),         // 15 分钟 → 下限（与历史行为一致）
            (60, 20),        // 60/3 = 20
            (120, 40),       // 120/3 = 40
        ];
        for (grace_min, want_slack_min) in cases {
            assert_eq!(
                slack_for(Duration::from_secs(grace_min * 60)),
                Duration::from_secs(want_slack_min * 60),
                "grace {grace_min}min should give slack {want_slack_min}min"
            );
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib power_guard
```

预期：编译失败，`cannot find function 'slack_for' in this scope`。

- [ ] **Step 3: 加 `slack_for` 与两个原子**

在 `power_guard.rs` 的 `SUPERVISOR_TICK` 之后加：

```rust
/// 看门狗余量跟随宽限缩放：`max(5min, grace/3)`。
///
/// 为什么需要缩放（已核实 `policy.rs` 的看门狗分支）：看门狗用**墙钟**
/// 度量「扛了多久」、用**单调钟**判定「有没有前进」。在「信任期且 mono 恰好
/// 冻结」这条路径上，用 `wall > grace + slack` 近似 `mono > grace`。若用户把
/// 宽限调到 120 分钟而 slack 固定 5 分钟，mono 冻结过一次就会在 125 分钟强制
/// 释放——那比用户设定还早。这条差异只在时钟异常路径体现，但正因为异常，
/// 用户不可能自己排查，所以参数关系必须在设计上自洽。
///
/// 放在这里而不是 `Policy` 内部：`Policy` 保持「给我两个窗口值」的纯函数
/// 语义，推导规则单点定义、有独立测试。
fn slack_for(grace: Duration) -> Duration {
    (grace / 3).max(yi_agent_power::policy::DEFAULT_WATCHDOG_SLACK)
}
```

在 `PowerGuard` 结构体里加两个字段：

```rust
pub(crate) struct PowerGuard {
    /// 主循环每拍写入：本进程侧是否有活（本地会话 ∪ 远端近期交互）。
    activity: Arc<AtomicBool>,
    /// 主循环每拍写入：设置页开关的当前值。
    enabled: Arc<AtomicBool>,
    /// 主循环每拍写入：条件消失后的宽限，单位秒。
    grace_secs: Arc<AtomicU64>,
    /// 主循环每拍写入：远端交互有效窗口，单位秒。
    remote_secs: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
```

顶部 import 改成：

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
```

- [ ] **Step 4: 改 `spawn` 签名与线程体**

```rust
impl PowerGuard {
    /// 起线程。三个初值都是装配时读到的——避免线程第一拍先误持、再释放。
    pub(crate) fn spawn(enabled: bool, grace_secs: u64, remote_secs: u64) -> Self {
        let activity = Arc::new(AtomicBool::new(false));
        let enabled_flag = Arc::new(AtomicBool::new(enabled));
        let grace_flag = Arc::new(AtomicU64::new(grace_secs));
        let remote_flag = Arc::new(AtomicU64::new(remote_secs));
        let stop = Arc::new(AtomicBool::new(false));
        let activity_in = Arc::clone(&activity);
        let enabled_in = Arc::clone(&enabled_flag);
        let grace_in = Arc::clone(&grace_flag);
        let stop_in = Arc::clone(&stop);

        let handle = std::thread::Builder::new()
            .name("yi-agent-power".to_string())
            .spawn(move || {
                // 时钟在线程里建：`SystemClock` 自己持有「单调钟是否可信」的状态，
                // 每拍读一次就是它的全部用途，跨线程共享没有意义。
                let clock = Arc::new(SystemClock::new()) as Arc<dyn Clock>;
                let mut supervisor = Supervisor::new(
                    clock,
                    Box::new(SystemPowerProbe::new()),
                    Box::new(SystemNetProbe::new()),
                    platform_assertion(ASSERTION_LABEL),
                    enabled_in.load(Ordering::Relaxed),
                );
                while !stop_in.load(Ordering::Relaxed) {
                    let busy = activity_in.load(Ordering::Relaxed);
                    let enabled = enabled_in.load(Ordering::Relaxed);
                    // 每拍重算并下发：成本是两次 store，远低于一次时钟采样。
                    // 不做「值变了才下发」的优化——那要多保存一份可能失同步的状态。
                    let grace = Duration::from_secs(grace_in.load(Ordering::Relaxed));
                    supervisor.set_windows(grace, slack_for(grace));
                    supervisor.tick(busy, enabled);
                    sleep_until_stop(&stop_in);
                }
                // 显式 drop：确保断言在 join 返回前释放，不依赖栈清理时机。
                drop(supervisor);
            })
            .expect("spawn the power guard thread");

        Self {
            activity,
            enabled: enabled_flag,
            grace_secs: grace_flag,
            remote_secs: remote_flag,
            stop,
            handle: Some(handle),
        }
    }
```

- [ ] **Step 5: 加 `set_windows`**

在 `set_enabled` 之后：

```rust
    /// 主循环每拍调用：把设置里的两个窗口写进来。只 `store`，不阻塞。
    ///
    /// 与 `set_enabled` 同形状：主循环绝不在这里做 I/O，窗口的读取与
    /// 换算都在调用方那一拍完成。
    pub(crate) fn set_windows(&self, grace_secs: u64, remote_secs: u64) {
        self.grace_secs.store(grace_secs, Ordering::Relaxed);
        // 远端窗口由主循环自己消费（传给 `remote_touch_busy`），线程不需要它；
        // 但存下来让「装配值与运行值来自同一处」这件事可被测试断言。
        self.remote_secs.store(remote_secs, Ordering::Relaxed);
    }
```

**注意**：`remote_secs` 在 `PowerGuard` 内部没有被线程读取。它存在是为了两个目的：(1) `set_windows` 的签名对称；(2) `Drop` 之后可被测试验证「主循环确实下发过」。若 `clippy` 报 `field is never read`，在该字段上加 `#[allow(dead_code)]` 并注明「由主循环持有，用于测试断言下发路径」。

- [ ] **Step 6: 给 2 处测试调用点补参**

```rust
        let guard = PowerGuard::spawn(true, 15 * 60, 5 * 60);
```

```rust
        let guard = PowerGuard::spawn(false, 15 * 60, 5 * 60);
```

- [ ] **Step 7: 跑测试确认通过**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib power_guard
```

预期：`slack_scales_with_the_grace_window` 通过；`dropping_the_guard_does_not_wait_a_whole_tick` 通过；`#[ignore]` 那条被跳过。`0 failed`。

- [ ] **Step 8: 跑 clippy 确认没留下 dead_code 告警**

```bash
cd yi-agent-rs
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings
```

预期：0 告警。若报 `remote_secs` 未读，按 Step 5 的说明加 `#[allow(dead_code)]`。

- [ ] **Step 9: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs
git commit -m "feat(power): 保活线程接收两个窗口并推导看门狗余量

新增两个 AtomicU64 存秒数，spawn 增加两个初值（避免线程第一拍用错
窗口），set_windows 与 set_enabled 同形状——只 store、不阻塞。

slack_for 实现 max(5min, grace/3)：看门狗用墙钟度量「扛了多久」、
用单调钟判定「有没有前进」，若 slack 固定 5 分钟而用户把宽限调到
120 分钟，mono 冻结时会在 125 分钟强制释放，早于用户设定。"
```

---

### Task 9: `server.rs` 装配与 `power_ticker` 接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3268-3275`（装配）、`:3868-3890`（`power_ticker` 分支）
- Test: 无独立测试（由 Task 6~8 的分项测试与 `just ci` 覆盖）

**Interfaces:**
- Consumes: Task 3 的 `power_windows_minutes`、Task 7 的 `remote_touch_busy(_, _, window)`、Task 8 的 `PowerGuard::spawn(enabled, grace_secs, remote_secs)` / `set_windows`
- Produces: 无（装配层）

- [ ] **Step 1: 改装配点**

`server.rs:3268-3275` 现在是：

```rust
    let initial_enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(
        &yi_agent_runtime::settings::load_from(&settings_path),
    );
    let power_guard = crate::power_guard::PowerGuard::spawn(initial_enabled);
```

改成（**只读一次盘**，三个初值来自同一份 `Settings`）：

```rust
    let initial_settings = yi_agent_runtime::settings::load_from(&settings_path);
    let initial_enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(&initial_settings);
    let (initial_grace_min, initial_remote_min) =
        yi_agent_runtime::settings::power_windows_minutes(&initial_settings);
    let power_guard = crate::power_guard::PowerGuard::spawn(
        initial_enabled,
        initial_grace_min * 60,
        initial_remote_min * 60,
    );
```

- [ ] **Step 2: 改 `power_ticker` 分支**

`server.rs:3868-3890` 里的分支改成：

```rust
            _ = power_ticker.tick() => {
                // 开关与两个窗口每拍现读磁盘，所以设置页保存后下一拍就生效，
                // 不必重启进程。`load_from` 是「读一次文件 + 逐字段校验」，
                // 10s 一次开销可忽略。
                //
                // **同一拍内只读一次盘**：`settings` 这一个变量同时喂给开关、
                // 两个窗口与远端判定，四处不会读到不同版本。
                let settings = yi_agent_runtime::settings::load_from(&settings_path);
                let enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(&settings);
                let (grace_min, remote_min) =
                    yi_agent_runtime::settings::power_windows_minutes(&settings);
                let (grace_secs, remote_secs) = (grace_min * 60, remote_min * 60);

                // 本进程侧：本地会话 ∪ 远端近期交互。两项都是纯内存判断，
                // 放主循环里是安全的（不碰 I/O，也不起子进程）。
                let local = crate::power_busy::local_busy(&threads);
                let remote = crate::power_busy::remote_touch_busy(
                    last_remote_touch,
                    &power_clock,
                    Duration::from_secs(remote_secs),
                );
                // `latest()` 永远立刻返回（上一拍的结果），绝不等待。
                let daemon = busy_sampler.latest();

                // 只写原子。探测与 IOKit 都在供电线程里，这里绝不阻塞。
                power_guard.set_activity(local || remote || daemon);
                power_guard.set_enabled(enabled);
                power_guard.set_windows(grace_secs, remote_secs);
            }
```

- [ ] **Step 3: 确认 `Duration` 已在作用域内**

```bash
cd yi-agent-rs
grep -n "^use std::time" crates/yi-agent-app-server/src/server.rs
```

若没有 `Duration`，在既有 `use std::time::...` 行里补上（`power_ticker` 的 `POWER_TICK` 已经是 `Duration`，大概率已导入）。

- [ ] **Step 4: 编译与测试**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server
cargo test -p yi-agent-app-server --lib power
```

预期：build 通过；`power` 前缀的测试（`power_busy`、`power_guard`）全过。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(power): 主循环把设置里的两个窗口下发给保活线程

装配点与 power_ticker 分支各读一次盘。同一拍内 settings 只读一次，
一个变量同时喂给开关、两个窗口与远端判定，四处不会读到不同版本。
只写原子，绝不在主循环里做探测或 IOKit。"
```

---

### Task 10: 前端「电源与休眠」分组

**Files:**
- Modify: `desktop/src/components/SettingsRuntimeTab.tsx:10-42`（三张表 + `VALUE_LABELS`）
- Test: `desktop/src/components/SettingsRuntimeTab.test.tsx:184-205`

**Interfaces:**
- Consumes: 后端 `FIELDS` 的 `group: "power"`（Task 5）
- Produces: 无（纯渲染）

- [ ] **Step 1: 改测试**

`SettingsRuntimeTab.test.tsx:185-193` 现在是：

```ts
  it("names every backend group in Chinese", () => {
    // 与 yi-agent-runtime/src/settings.rs 的 FIELDS 分组一一对应。漏一个就会在页面上
    // 露出英文组名（tui 曾经就是这样）。
    expect(GROUP_ORDER).toEqual(["agent", "compact", "tui", "startup"]);
    for (const group of GROUP_ORDER) {
      expect(GROUP_LABELS[group]).toBeTruthy();
      expect(GROUP_HINTS[group]).toBeTruthy();
    }
  });
```

改成：

```ts
  it("names every backend group in Chinese", () => {
    // 与 yi-agent-runtime/src/settings.rs 的 FIELDS 分组一一对应。漏一个就会在页面上
    // 露出英文组名（tui 曾经就是这样）。
    //
    // 不写精确 toEqual：这张表会随功能增长，每加一组就要改一次期望数组，
    // 而本测试真正要守的是「没有组漏出英文名」。下面两条循环断言已覆盖它，
    // toContain 再钉住顺序表里确实登记了新组。
    for (const group of GROUP_ORDER) {
      expect(GROUP_LABELS[group]).toBeTruthy();
      expect(GROUP_HINTS[group]).toBeTruthy();
    }
    expect(GROUP_ORDER).toContain("power");
  });
```

并在 `translates enum values but keeps unknown ones verbatim` 之后追加一条：

```ts
  it("translates the power switch like the other on/off selects", () => {
    // 漏加映射不会报错，只会让下拉框显示英文 true/false，与相邻的
    // 「终端通知」组风格不一致。
    expect(labelFor("power.prevent_idle_sleep", "true")).toBe("开");
    expect(labelFor("power.prevent_idle_sleep", "false")).toBe("关");
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop
npx vitest run src/components/SettingsRuntimeTab.test.tsx
```

预期：`names every backend group in Chinese` 的 `expect(GROUP_ORDER).toContain("power")` 失败；新增那条 `translates the power switch` 失败（返回 `"true"` 而非 `"开"`）。

- [ ] **Step 3: 改三张表与 `VALUE_LABELS`**

```ts
export const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "上下文压缩",
  startup: "启动",
  tui: "终端通知",
  power: "电源与休眠",
};

/** 每组一句话说明在管什么。后端新增分组时查不到就不渲染，不造空话。 */
export const GROUP_HINTS: Record<string, string> = {
  agent: "单次对话的执行上限与提示词覆盖",
  compact: "上下文接近上限时的自动压缩策略",
  tui: "命令行界面在回合结束时的提示方式",
  startup: "启动时生效的默认档位",
  power: "有网且（插电 或 有活在跑）时阻止系统空闲休眠",
};

/** 按「影响面从大到小」排：agent 是主循环，compact 是它的兜底，tui 是另一个前端。 */
export const GROUP_ORDER = ["agent", "compact", "tui", "startup", "power"];
```

理由是 §0.3：前四组影响单次对话，`power` 影响整机、改动频率最低，放末尾。

`VALUE_LABELS` 里加一条（与 `tui.notifications` 同形态）：

```ts
const VALUE_LABELS: Record<string, Record<string, string>> = {
  "startup.sandbox": {
    "read-only": "只读",
    "workspace-write": "工作区可写",
    "danger-full-access": "完全访问",
  },
  "tui.notifications": { true: "开", false: "关" },
  "power.prevent_idle_sleep": { true: "开", false: "关" },
  "tui.notification_method": { auto: "自动", osc9: "OSC 9（带正文）", bel: "响铃" },
  "tui.notification_condition": { unfocused: "仅窗口失焦时", always: "总是" },
};
```

**`labelFor` 不用改**（`SettingsRuntimeTab.tsx:68-71` 是纯查表，表里有就生效）。两个数值字段也**不要**指望 `labelFor` 加单位——`Number` 走的是 `<input type="number">`，单位已写在 `label` 里。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop
npx vitest run src/components/SettingsRuntimeTab.test.tsx
npx tsc --noEmit
```

预期：测试全过；类型检查无错误。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SettingsRuntimeTab.tsx desktop/src/components/SettingsRuntimeTab.test.tsx
git commit -m "feat(desktop): 设置页新增「电源与休眠」分组

三张表各加一行、GROUP_ORDER 末尾追加 power、VALUE_LABELS 补开关的
中英文映射（不补会让下拉框显示英文 true/false，与相邻的「终端通知」
组风格不一致）。

GROUP_ORDER 那条精确 toEqual 改成覆盖断言：该表会随功能增长，而这条
测试真正要守的是「没有组漏出英文名」，循环断言已覆盖，toContain 钉住
新组已登记。"
```

---

### Task 11: 更新项目进度文档并跑完整 CI

项目规范要求每组需求完成后同步更新 `docs/project-management/`，且在同一个 commit 内提交。

**Files:**
- Modify: `docs/project-management/desktop.md`（若有设置页相关条目）
- Modify: `docs/project-management/README.md`（模块索引表的计数）
- 视情况: `docs/project-management/` 下与 power / settings 对应的模块文件

**Interfaces:**
- Consumes: Task 1~10 的全部产出
- Produces: 无

- [ ] **Step 1: 找到该记在哪**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-power-settings-ui
ls docs/project-management/
grep -rn "休眠\|prevent_idle_sleep\|保活" docs/project-management/ | head -20
```

预期：能看到既有的「干活期间防止空闲休眠」条目（在某个模块文件里）。记在**该条目所在模块**，新增一条「休眠参数进设置界面」，或在该条目下补充子项——按文件既有粒度决定。

- [ ] **Step 2: 新增条目**

格式必须是三态之一（`[x]` 已完成 / `[ ]` 未完成），**禁用 `[~]`**，并带可验证判据（代码位置或可执行命令）。按实际落点写，例如：

```markdown
- [x] 休眠参数进设置界面：`power` 分组开放「条件消失后再保活多久」「远端交互有效窗口」两项，开关迁到 `power.prevent_idle_sleep`
  - 字段定义：`yi-agent-rs/crates/yi-agent-runtime/src/settings.rs` 的 `FIELDS`（`power.idle_grace_minutes` / `power.remote_touch_minutes`）
  - 迁移：同文件 `migrate_legacy_keys`；判据 `cargo test -p yi-agent-runtime migrate`
  - 跨字段拒绝：同文件 `write_to`；判据 `cargo test -p yi-agent-runtime power_windows`
  - 策略参数化：`yi-agent-rs/crates/yi-agent-power/src/policy.rs`；判据 `cargo test -p yi-agent-power policy`
  - 前端分组：`desktop/src/components/SettingsRuntimeTab.tsx`；判据 `npx vitest run src/components/SettingsRuntimeTab.test.tsx`
```

- [ ] **Step 3: 更新 README 计数**

```bash
grep -n "desktop" docs/project-management/README.md | head -10
```

把对应模块的「完成 / 总计」计数加 1（例如 `106 / 117` → `107 / 118`）。总数与完成数**都要加**，因为这是新增条目而不是勾掉既有条目。

- [ ] **Step 4: 跑完整 CI**

```bash
cd yi-agent-rs
cargo fmt --all
just ci
```

预期：12 道门禁全绿。这是项目规定的唯一 CI 入口。若有 red，逐个修——**不要**跳过或注释掉失败的检查。

- [ ] **Step 5: 跑前端验证**

```bash
cd ../desktop
npx vitest run src/components/SettingsRuntimeTab.test.tsx
npx tsc --noEmit
```

预期：全过。

- [ ] **Step 6: 跑 arm 计数自查**

```bash
cd ../yi-agent-rs
_ARMS='^[[:space:]]+("[a-z]+\.[a-z_]+"|settings::PREVENT_IDLE_SLEEP_KEY) =>'
echo -n "web arms: "   && grep -cE "$_ARMS" crates/yi-agent-web/src/api.rs
echo -n "rpc arms: "   && grep -cE "$_ARMS" crates/yi-agent-app-server/src/settings_rpc.rs
echo -n "FIELDS:   "   && awk '/pub static FIELDS/,/^];/' crates/yi-agent-runtime/src/settings.rs | grep -c 'key:'
```

预期：三个数字都是 15。

- [ ] **Step 7: 确认旧键字面量只出现在允许的两处**

```bash
cd yi-agent-rs
grep -rn 'agent\.prevent_idle_sleep' crates/
```

预期：只剩 `migrate_legacy_keys` 函数体内（注释 + 常量 `LEGACY_*` 的拼接，注意这里的 `LEGACY_GROUP` / `LEGACY_FIELD` 是分开的常量，所以**可能 grep 不到**——若有 `#[cfg(test)]` 里用旧键名造文件的字符串常量，也会出现在这里），以及测试的文件内容里。若出现生产代码的其他位置，说明漏改。

- [ ] **Step 8: 提交**

```bash
git add docs/project-management/
git commit -m "docs(project-management): 记录休眠参数进设置界面

按规范同步模块条目与 README 计数，每条带可验证判据（测试命令）。
just ci 12 道门禁全绿。"
```

---

## 自检记录（写入本文档时的核对结果）

### 多轮自检中发现并修掉的 6 个问题

这份 plan 不是一次写成的。下面是逐轮核对中实际查出的问题，全部已修正。实现时若觉得某处「看起来奇怪」，先看这张表——它记录了为什么那样写。

| # | 问题 | 严重度 | 怎么发现的 | 修法 |
|---|---|---|---|---|
| 1 | Task 1 的测试全用 `ac_online(false)` 表示「条件消失」，但 `ac_online` 的 `on_ac` 恒为 `true`、`keeps_awake` 恒真，永远不释放 | **照做会假通过** | 用 Python 复刻 `keeps_awake` 推演 | 新增 `battery(busy)` 夹具（`on_ac=false, online=true`），测释放只用它；并在文档里写明为什么不能用 `ac_online` |
| 2 | 首拍写成 `battery(false)` 会直接 Idle（`last_ok` 尚未写入），测试从第一条断言就错 | **照做会失败** | 逐条跑推演脚本 | 全部 7 条测试的首拍改 `battery(true)`；`default_windows` 那条的注释也改成「`busy` 在 `at(0)` 同拍」 |
| 3 | Task 5 的跨字段校验写成「先 `normalized()` 再比较」，而 `normalized()` 自带倒挂回落会把值清成 `None` → 比较恒假 → **任何倒挂都不被拒绝** | **功能完全失效** | 模拟 `normalized()` 的倒挂分支 | 改为对两个字段各自做单字段钳制（`within`），不调 `normalized()`；同步改了 spec §5.2 |
| 4 | 若每次都检查文件里既有的倒挂值，用户手改坏了之后**连改无关字段都会失败** | **把人锁在文件外** | 顺着 #3 继续推演 | 加 `touches_power_windows` 守卫，只在本次 patch 触碰那两个键时才拒绝；补 2 条测试钉住两侧 |
| 5 | Task 3 的 `clamp!` 测试依赖 `FIELDS` 里的 `min`/`max`，但那两个字段 Task 5 才加；`within` 对未知键返回 `true` → 测试必失败 | **照做会失败** | 查 `within` → `find_spec` 的返回值 | 把 `power_windows_out_of_range_fall_back_to_defaults` 移到 Task 5 Step 6，并在 Task 3 Step 5 注明「这两行此刻不生效是正常的」 |
| 6 | spec §7.4 的自查命令 `grep -c '=> opt('` 数出 10，而 `FIELDS` 是 13 —— 三个跨行分支匹配不到 | **自查会误报** | 实跑该命令 | 改成数 arm 模式行（`^[[:space:]]+("[a-z]+\.[a-z_]+"|settings::PREVENT_IDLE_SLEEP_KEY) =>`），实测 13/13/13；spec 与本 plan 同步 |

### 已用真实编译验证的代码

不是「看起来对」，是真的跑过：

| 验证对象 | 方式 | 结果 |
|---|---|---|
| Task 1 的 `Policy` 全量代码（常量、手写 `Default`、`with_windows`/`set_windows`、`decide` 的 `Idle` 清空）+ 7 条测试断言 | 抽成独立 crate，`cargo run` 真实编译执行 | 22 条断言全绿，含「`grace=0` 时首拍即 Idle」这条反证（证明 `Default` 置零会被抓到） |
| Task 5 Step 5 的跨字段校验代码（`within` 钳制 + `power_windows_minutes` + `patch.contains_key`） | 独立 crate 编译 + 9 个场景推演 | 编译通过；9 个场景（倒挂拒绝／相等放行／越界回落／无关写入放行／触碰拒绝）全部符合预期 |
| Task 8 的 `slack_for` 推导表 | Python 复刻 `(grace/3).max(300s)` | 1→5、5→5、15→5、60→20、120→40 分钟，与 spec §4.2 的表逐行一致 |
| Task 6 的 arm 计数自查命令 | 在当前代码上实跑 | 13 / 13 / 13，与 `FIELDS.len()` 相等 |
| Task 2 的 supervisor 测试时序 | Python 复刻 | 901 秒释放、换 60 分钟窗口后空跑 20 分钟仍持有 |


**Spec 覆盖检查：**

| Spec 章节 | 落在哪个 Task |
|---|---|
| §0 / §0.1~0.4 前端现状与三张表 | Task 10 |
| §3.1 新分组 `power` 的三个字段 | Task 3（形状）、Task 5（`FIELDS`）、Task 6（读回）、Task 10（前端） |
| §3.1 硬编码列表（空组白名单 / `Settings` 字段 / 两个 match / 前端三表） | Task 3 Step 3、Task 5 Step 4、Task 6、Task 10 |
| §3.2 键名必须两层 | Global Constraints |
| §3.3 用分钟 | Task 3 Step 3、Task 5 Step 3 |
| §3.4 类型选择 / 常量 / `default_display` 两条测试 / description 改文案 | Task 3 Step 3+8、Task 5 Step 3 |
| §4.1 `Policy` 参数化 + 手写 `Default` + `GRACE` 别名 | Task 1 |
| §4.2 `watchdog_slack = max(5min, grace/3)` 与 `slack_for` | Task 8 Step 1+3 |
| §4.3 不开放 `SUPERVISOR_TICK` / `CONNECT_TIMEOUT` | 无（明确不做） |
| §4.4 `Supervisor` / `PowerGuard` 数据通路 + `spawn` 3 个调用点 | Task 2、Task 8、Task 9 |
| §4.4.1 `power_windows_minutes` 签名与语义 | Task 3 Step 4 |
| §4.5 `REMOTE_TOUCH_WINDOW` 参数化 | Task 7 |
| §5.1 单字段校验（`min`/`max`） | Task 5 Step 3 |
| §5.2 跨字段拒绝 + 合并对象 + `normalized` + `Corrupt` | Task 5 Step 5 |
| §5.3 降级（`clamp!` ×2 + 倒挂两个都清） | Task 3 Step 5 |
| §5.4 生效时机 | Task 9 Step 2（每拍现读） |
| §5.5 缩短立即生效 + 调长不复活 | Task 1（3 条测试 + 手写 `Default`）、Task 2 |
| §6.1 迁移在 `Value` 上 + 三条约束 + 独立函数 | Task 4 |
| §6.2 不落盘 + 旧键长期残留 | Task 4 Step 3、Task 5 Step 1（`migrate_legacy_key_stays_on_disk_after_other_writes`） |
| §6.3 旧键不进 `FIELDS` + 6 条既有测试 + 常量只表示新键 | Task 3 Step 3+7、Task 5 Step 1 |
| §7.1 6 条 policy 测试 | Task 1 Step 1（写 7 条，含 spec 要求的对照断言） |
| §7.2 `power_busy` 4 处补参 + 1 条新测试 | Task 7 |
| §7.3 9 条 settings 测试 + 3 条纯 JSON 单测 | Task 3 Step 1（5 条）、Task 4 Step 1（3 条）、Task 5 Step 1（6 条） |
| §7.4 两个 match + arm 计数自查 | Task 6 |
| §7.5 前端三表 + `VALUE_LABELS` + `labelFor` 测试 | Task 10 |
| §8.1 / §8.1.1 自动化与 `just ci` | Task 11 Step 4/5/6 |
| §8.2 真机验收 | 不在 plan 内（需人工），见下方「交付后人工验收」 |
| §9 风险 | 各 Task 的 commit message 与 Global Constraints |

**占位符扫描：** 无 `TBD` / `TODO` / `FIXME` / `implement later` / `similar to Task N` / `add appropriate error handling`。每个 code step 都给了可直接粘贴的代码块。

**类型一致性核对：**

| 名字 | 定义处 | 使用处 | 类型 |
|---|---|---|---|
| `Policy::with_windows` | Task 1 Step 3 | Task 1 Step 1 | `(Duration, Duration) -> Self` |
| `Policy::set_windows` | Task 1 Step 3 | Task 1 Step 1、Task 2 Step 3 | `(&mut self, Duration, Duration)` |
| `Supervisor::set_windows` | Task 2 Step 3 | Task 8 Step 4 | `(&mut self, Duration, Duration)` |
| `power_windows_minutes` | Task 3 Step 4 | Task 3 Step 1、Task 5 Step 5、Task 9 Step 1+2 | `(&Settings) -> (u64, u64)`，顺序 `(grace, remote)` |
| `POWER_IDLE_GRACE_MINUTES_DEFAULT` | Task 3 Step 3 | Task 3 Step 1+8、Task 5 Step 3（`default_display: "15"` 字面量） | `u64 = 15` |
| `POWER_REMOTE_TOUCH_MINUTES_DEFAULT` | Task 3 Step 3 | Task 3 Step 1+8、Task 5 Step 3（`default_display: "5"`） | `u64 = 5` |
| `migrate_legacy_keys` | Task 4 Step 3 | Task 4 Step 1+4 | `(&mut serde_json::Value)` |
| `remote_touch_busy` | Task 7 Step 3 | Task 7 Step 1+4、Task 9 Step 2 | `(Option<Stamp>, &dyn Clock, Duration) -> bool` |
| `PowerGuard::spawn` | Task 8 Step 4 | Task 8 Step 6、Task 9 Step 1 | `(bool, u64, u64) -> Self` |
| `PowerGuard::set_windows` | Task 8 Step 5 | Task 9 Step 2 | `(&self, u64, u64)` |
| `slack_for` | Task 8 Step 3 | Task 8 Step 1+4 | `(Duration) -> Duration` |
| `PowerSettings` 三字段 | Task 3 Step 3 | Task 3 Step 4+5、Task 5、Task 6、Task 9 | `Option<String>` / `Option<u64>` / `Option<u64>` |

**参数顺序一致性**：`power_windows_minutes` 返回 `(grace, remote)`；`set_windows(grace_secs, remote_secs)`；`PowerGuard::spawn(enabled, grace_secs, remote_secs)`；`slack_for(grace)`。四处顺序统一，无错位。

**测试名前缀核对**（供 `cargo test <filter>` 命中）：

| 过滤词 | 命中条数 | 前缀是否正确 |
|---|---|---|
| `cargo test -p yi-agent-power --lib policy` | Task 1 新增 7 + 既有 16 = 23 | 全在 `policy` 模块内 |
| `cargo test -p yi-agent-power --lib supervisor` | Task 2 新增 1 + 既有 8 = 9 | 全在 `supervisor` 模块内 |
| `cargo test -p yi-agent-runtime --lib power_windows` | Task 3 的 4 + Task 5 的 2 = 6 | 全部 `power_windows_` 前缀 |
| `cargo test -p yi-agent-runtime --lib migrate` | Task 4 的 3 + Task 5 的 5 = 8 | 全部 `migrate_` 前缀 |
| `cargo test -p yi-agent-app-server --lib power_busy` | Task 7 新增 1 + 既有 3（本 Task 改到的那 3 条）+ 其余既有 | 全在 `power_busy` 模块内 |
| `cargo test -p yi-agent-app-server --lib power_guard` | Task 8 新增 1 + 既有 2 | 全在 `power_guard` 模块内 |

**逐条数清（供实现时对照）**：

- **Task 3 的 `power_windows_` 前缀 4 条**：`power_windows_default_to_fifteen_and_five`、`power_windows_inverted_pair_falls_back_to_both_defaults`、`power_windows_allow_equal_values`、`power_windows_read_through_from_a_file`。（`power_windows_out_of_range_fall_back_to_defaults` 已移到 Task 5，见 Task 3 Step 5 的说明。）
- **Task 5 的 `power_windows_` 前缀 2 条**：`power_windows_out_of_range_fall_back_to_defaults`、`power_windows_reject_a_remote_window_longer_than_the_grace`。
- **Task 4 的 `migrate_` 前缀 3 条**：`migrate_moves_the_legacy_key_into_power`、`migrate_keeps_an_explicit_power_key`、`migrate_twice_is_a_no_op`。
- **Task 5 的 `migrate_` 前缀 5 条**：`migrate_the_old_agent_key_into_the_power_group`、`migrate_twice_on_disk_is_stable`、`migrate_keeps_an_explicit_power_key_on_disk`、`migrate_legacy_key_stays_on_disk_after_other_writes`、`migrate_value_survives_a_fresh_write`。（我这里逐项列出了 5 个名字。）
- **Task 5 另有 2 条不以两个前缀开头的测试**：`an_unrelated_write_succeeds_despite_an_inverted_pair_on_disk`、`touching_an_inverted_pair_is_still_rejected`。它们靠 `cargo test -p yi-agent-runtime --lib unrelated_write` 与 `--lib touching_an_inverted` 单独跑，或在 `cargo test -p yi-agent-runtime` 全量跑时覆盖。**不要**以为 `power_windows` 与 `migrate` 两个过滤词能覆盖它们。

## 交付后人工验收（不在 plan 的 task 内）

spec §8.2 的四条真机项需要人工操作，实现者完成后应逐条走一遍：

1. **生效时延**：设置页把宽限从 15 改成 1 并保存 → `pmset -g assertions` 在 20 秒内反映新窗口语义。
2. **立即释放**：断言持有期间把宽限从 15 改到 1，条件平复后 1 分钟内断言消失。
3. **迁移不丢设置**：手工把 `settings.json` 写成旧格式（`agent.prevent_idle_sleep: "false"`），启动后设置页「干活期间防止休眠」显示「关」。
4. **跨字段拒绝**：远端窗口设 30、宽限设 5 → 保存被拒并在界面看到原因。
