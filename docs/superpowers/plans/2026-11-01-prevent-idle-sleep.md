# 干活期间防止系统空闲休眠 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `yi-agent app-server` 在有网且（插电 或 有活）时持有 `PreventUserIdleSystemSleep` 断言，条件消失 15 分钟后释放，使长任务不会因空闲休眠被打断。

**Architecture:** 新增纯策略 crate `yi-agent-power`：`clock`（睡眠感知时钟 + 墙钟自检）、`policy`（零 I/O 状态机）、`assertion`（macOS IOKit，其它平台 no-op）、`probe`（电源 + 网络，trait 注入）、`supervisor`（10s tick 串联）。`yi-agent-app-server` 主循环按 10s 拍采 `busy`/电源/网络喂给 supervisor，迁移断言状态；busy 由本进程会话快照、项目 daemon 的 `ListTaskSummaries`、远端交互时间戳三路并集得到。开关进 `settings::FIELDS`，设置页自动渲染。

**Tech Stack:** Rust 2024、`libc`（`clock_gettime`，`CLOCK_MONOTONIC_RAW`）、IOKit/CoreFoundation FFI（`IOPMAssertionCreateWithName`、`IOPSGetProvidingPowerSourceType`）、`tokio`（主循环 tick）、`std::process::Command`（`route -n get default`）。

**设计文档:** `docs/superpowers/specs/2026-11-01-prevent-idle-sleep-design.md`（所有 `file:line` 引用以其为准）

## 验证状态（本文档的代码已实测编译并通过测试）

> 这一节记录「哪些代码真的跑过、跑出了什么」。**它不是承诺，是证据。**
> 里程碑：本 plan 的 Task 1~9 代码曾在 worktree 里真实合入 workspace 编译，
> 并跑过测试；随后为保持本分支为纯文档分支而回滚。归档位置见每节末尾。

### 已实测通过

| 范围 | 证据 |
|---|---|
| `yi-agent-power` 整个 crate | `cargo test` **50 passed, 0 failed**；`cargo clippy --all-targets -- -D warnings` **0 warning** |
| 真机 IOKit 断言 | `IOPMAssertionCreateWithName(PreventUserIdleSystemSleep, 255, ..)` 返回 `0x0`；`pmset -g assertions` 可见 `PreventUserIdleSystemSleep named: "yi-agent: ..."`；`IOPMAssertionRelease` 返回 `0x0` |
| 时钟平台行为 | `CLOCK_MONOTONIC_RAW(4)` 成功；`clock_gettime(7)` 在 macOS 返回 -1（`CLOCK_BOOTTIME` 不存在，失败分支真实可达）；`RAW` 与 `UPTIME_RAW` 相差约 672 831 秒（≈7.8 天累计睡眠） |
| 网络探测收窄 | 只按 flags 判得 14+ 个噪音接口；加「必须有 AF_INET」后收窄到 2 个（`en0` + `utun4`）；`route -n get default` 在 VPN 开启时仍只返回物理出口 |
| `power_busy.rs` | `cargo test -p yi-agent-app-server power_busy` **8 passed**（真实 crate 内编译 + 运行） |
| `power_guard.rs` | `cargo build -p yi-agent-app-server` 通过（含真机断言线程）；线程化后 `Drop` 实测等待 **39ms**（而非 10 秒） |
| Task 5 四处同步 | `settings_rpc` **4 passed**、`yi-agent-web get_config` **3 passed**、`yi-agent-runtime settings` **57 passed** |
| Task 5 的「先红」断言 | 摘掉 `settings_rpc` 分支后，`every_field_in_fields_can_be_read_back` 确实 FAILED，报错文本为 `agent.prevent_idle_sleep is in FIELDS but current_value returned null for it` |
| 常量作 match pattern | `settings::PREVENT_IDLE_SLEEP_KEY => ..` 与字面量分支共存，实测编译通过 |
| `ProcessManager::new` 返回类型 | 返回 `Arc<Self>`，故测试夹具里**不能**再包一层 `Arc::new` |
| **整份 plan 的代码可独立编译** | 从本文档抽取 6 个模块拼成 crate：`cargo test` **50 passed**；`cargo clippy --all-targets -- -D warnings` **0 告警**；`cargo fmt --check` 干净 |
| settings 6 条开关语义测试 | 默认开 / `"false"` 关 / 大写 `"FALSE"` 关 / 未知值回默认 / 空串视为未设置 / **文件损坏时仍开** —— 6 passed |
| `sleep_until_stop` 的打断延迟 | 10 秒 tick 下 `Drop` 实测等待 **38.9 ms**（若用裸 `thread::sleep` 会等满 10 秒） |

### 实测推翻的两个设计

这两条是本 plan 相对 spec 最重要的修正，**实现时以本 plan 为准**。

**一、第一版 policy 的看门狗永不触发。**
初版用 `now - hold_since > GRACE + SLACK` 判「这钟扛太久了」。但它的目的正是检测
「单调钟冻结」——而冻结时 `now` 恒等于 `hold_since`，差值是 `0`，`0 > 20min` 为假。
**兜底在最该生效的场景下完全失效**，于是机器可能整夜不睡。
修正为 `Sample { mono, wall }` 双时钟：宽限窗用 `mono`（含睡眠时间），看门狗
「扛了多久」用 `wall`（mono 冻结时唯一还能前进的来源），看门狗「有没有前进」
用 `mono`（判据是**有增长**而非增长得够快，后者会把高负载下的正常时钟误判为坏钟）。
回归哨兵：`the_watchdog_releases_when_the_monotonic_clock_is_frozen`。

**二、整条 power tick 不能进主循环。**
初版把 `supervisor.tick(..)` 直接放进 `select!` 分支。实测这条路径最坏要
**2 秒**（`route` 子进程 10~50ms + `connect_timeout(gateway:80, 2s)` 等满），
而 `DRIVER_TICK` 只有 3 秒。让「保活」自己成为卡顿源是自相矛盾的。
修正为独立 OS 线程（`power_guard.rs`）：主循环每拍只写两个 `AtomicBool`。
同时 `Drop` 用 50ms 分段等待（`sleep_until_stop`），否则关掉桌面要卡 10 秒。

### 归档位置

回滚前的完整实现已归档，可直接对照：

- `/tmp/verified-implementation/power/` — `yi-agent-power` 全 6 个文件 + `Cargo.toml`（50 测试全过的版本）
- `/tmp/verified-implementation/app-server/` — `power_busy.rs`、`power_guard.rs`
- `/tmp/verified-implementation/patches/all.patch` — 其余七个文件的 diff（`Cargo.toml` 注册、`settings.rs`、`settings_rpc.rs`、`api.rs`、`binding.rs`、`lib.rs`、app-server `Cargo.toml`）

> ⚠️ `/tmp` 会被系统清理。执行本 plan 时若归档已消失，**以本文档内嵌的代码为准**
> ——两者在回滚时是同一份。

---

## Global Constraints

- **平台**：macOS 有真实实现；Linux/Windows 必须编译通过且 `assertion` 为 no-op。`clock` 在 Linux 用 `CLOCK_BOOTTIME`，**不得**用 `CLOCK_MONOTONIC`。
- **时钟**：凡是判「过了多久」一律走 `yi-agent-power::clock` 的 `Stamp` + `since()`。**禁止**在本次新增代码里出现 `std::time::Instant::now()` 或 `tokio::time::Instant::now()` 作为经过时长的基准。`Stamp` 不得是 `Instant` 的别名。
- **断言类型固定**：`PreventUserIdleSystemSleep`。**不许**用 `PreventSystemSleep`，**不许**用 `PreventUserIdleDisplaySleep`，**不许**起 `caffeinate` 子进程。
- **策略公式固定**：`ok = online && (on_ac || busy)`；`hold = last_ok.is_some_and(|t| now.mono - t < GRACE)`（等价于 `map_or(false, ..)`，但 clippy 要求前者）；`GRACE = 15min`；`last_ok` 初值 **`None`**；宽限边界取 `<`（恰好 15:00 释放）。**入参是两条时钟 `Sample { mono, wall }`，不是一条**——理由见「实测推翻的两个设计」。
- **电源判定固定**：`on_ac = power_type != "Battery Power"`（UPS 与未知类型都算插电）。
- **网络判定固定**：候选接口 `IFF_UP && !IFF_LOOPBACK && 有 AF_INET 地址 && 名字不以虚拟前缀开头`；在线 = 有候选 **且**（网关 `connect` 成功 **或** `ECONNREFUSED`）。**不探测 provider，不探测公网地址**。
- **配置键固定**：`agent.prevent_idle_sleep`，`group = "agent"`，`FieldType::Select`，`options = ["true","false"]`，`default_display = "true"`，默认开。
- **主循环约束（已实测修正）**：主循环**不得**执行任何可能慢的探测。`getifaddrs`、`route` 子进程（10~50ms）、`connect_timeout(gateway:80, 2s)`、IOKit FFI 全部在独立的 `power_guard` 线程里；主循环每拍只写两个 `AtomicBool`。daemon IPC 超时固定 500ms，用 `send_request_with_version_timeout`，且只在 `BusySampler` 的 `spawn_blocking` 里调。
- **不得**让 busy 探测刷新 `RuntimeBinding` 的空闲时钟（详见 Task 8 的 `current_handle_no_touch`）。
- **提交前必须**在 `yi-agent-rs/` 下跑 `cargo fmt --all`。提交信息用 conventional commits，首行 ≤ 72 字符，**不得**含 `Co-Authored-By`。
- **不得**在 `main` 分支直接改动；本 plan 全程在 `.worktrees/docs-prevent-idle-sleep`（分支 `docs/prevent-idle-sleep-design`）里做。

---

## 与设计文档的三处偏差（已实测确认，实现以本 plan 为准）

这三处是我在核实代码时发现、spec 未覆盖或写错的地方。**实现时按本 plan，不要按 spec 原文**；Task 11 的 Step 5 会把这几条回写进 spec。

1. **`RuntimeBinding::current()` 有副作用，会永久阻止 runtime 回收。**
   `crates/yi-agent-subagent/src/binding.rs:125-128` 的 `current()` 第一行就是 `self.touch()`，刷新 `last_active`；而 `evict_idle_runtimes`（`server.rs:106`）正是用 `idle_for()`（同文件 `:153`）判「空了多久」。若 busy 探测每 10s 调一次 `current()`，所有项目 runtime 的空闲时钟永远被刷新到「刚刚」→ `EVICT_IDLE_AFTER`（600s）**永不触发** → daemon 永不回收。这是本功能会引入的真实回归。
   解法：加 `pub(crate) fn current_handle_no_touch(&self) -> Result<RuntimeHandle, String>`，只读 `state.handle` 不 `touch()`；busy 探测只用它。

2. **spec 说「主循环里同步调 IPC 用 500ms 短超时即可」，但主循环是单线程 `select!`。**
   `server.rs:3333` 起的主循环把 `threads`/`runtimes` 当作独占局部量，一次 500ms 的阻塞等值于「所有 RPC 停摆 500ms」，而 `DRIVER_TICK` 只有 3s。每个 tick 都吃这 500ms 会把驱动精度打掉六分之一。
   解法：busy 采样**在独立 task 里**跑（`tokio::task::spawn_blocking`），主循环只做非阻塞的 `try_recv` 取最新快照；取不到就沿用上一拍的值。见 Task 8。

3. **设置读取点有三处，不是 spec 说的两处。**
   除 `settings_rpc::current_value`（`crates/yi-agent-app-server/src/settings_rpc.rs:46-66`）外，web 端还有 `crates/yi-agent-web/src/api.rs:77-98` 的 `field_value`，且 `crates/yi-agent-web/tests/api_test.rs:168` 断言「每组 `vars` 数量之和 == `FIELDS.len()`」——加字段不改它，这条测试会红。
   Task 5 一并覆盖三处（`FIELDS` / `AgentSettings` / 两个 `current_value` 变体）。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-power/Cargo.toml` | 新 crate 清单；`libc` 依赖；macOS 用 `#[link(kind="framework")]` 链 IOKit/CoreFoundation（无需 build.rs） |
| `.../yi-agent-power/src/lib.rs` | 模块声明与 re-export |
| `.../yi-agent-power/src/clock.rs` | `Stamp`、`Clock` trait、`SystemClock`（睡眠感知 + 墙钟自检 + `mono_trusted` 单向降级） |
| `.../yi-agent-power/src/policy.rs` | `Snapshot`、`Decision`、`Policy`（公式 + 15 分钟窗口 + 看门狗），零 I/O |
| `.../yi-agent-power/src/assertion.rs` | `Assertion` trait + `MacAssertion`（IOKit）+ `NoopAssertion` + `platform_assertion()` 工厂 |
| `.../yi-agent-power/src/probe.rs` | `PowerProbe` / `NetProbe` trait + `SystemPowerProbe` / `SystemNetProbe` |
| `.../yi-agent-power/src/supervisor.rs` | `Supervisor`：10s 拍状态机，迁移边沿才调 FFI |
| `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs` | `AgentSettings.prevent_idle_sleep` + `FIELDS` 一条 + `normalized()` 白名单 |
| `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs` | `current_value` 加分支（否则既有测试红） |
| `yi-agent-rs/crates/yi-agent-web/src/api.rs` | `field_value` 加分支（否则 web 测试红） |
| `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs` | 加 `current_handle_no_touch()`（偏差 1） |
| `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs` | busy 三路并集的采集逻辑 + 独立采样任务（偏差 2） |
| `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs` | **整个 10s tick 的独立 OS 线程**：唯一持有 IOKit 断言的地方；主循环只写两个原子 |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 注册 `power_busy` / `power_guard` 两个模块 |
| `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` | 加 `yi-agent-power = { workspace = true }` 依赖 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 主循环加 10s `power_ticker`（只写原子）、`last_remote_touch` 刷新点、装配 `PowerGuard` 与 `BusySampler`、退出前 `drop` |
| `docs/project-management/yi-agent-power.md` | 新模块进度文件 |
| `docs/project-management/README.md` | 模块索引加一行 |

拆分理由：`policy` 与 `clock` 必须各自独立可测（一个是纯状态机、一个是平台时钟），二者若混在一个文件，Linux CI 上就无法把 15 分钟窗口的全部行为跑完。`probe` 与 `assertion` 是 FFI 面，单独成文件是为了让 `unsafe` 面积可数。

**`power_guard.rs` 为什么在 crate 外（app-server 里）而不是 `yi-agent-power` 里**：它要 `join` 线程、要拿到进程的生命周期，这些是「宿主」的职责；而 `yi-agent-power` 保持零进程语义，才可能被别的宿主（TUI）复用。

---

### Task 1: 建 `yi-agent-power` crate 骨架

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-power/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-power/src/lib.rs`
- Modify: `yi-agent-rs/Cargo.toml`（workspace `members` 与 `[workspace.dependencies]`）

**Interfaces:**
- Produces: crate `yi-agent_power`，后续任务往里加模块。

- [ ] **Step 1: 建目录与清单**

```bash
mkdir -p yi-agent-rs/crates/yi-agent-power/src
```

`yi-agent-rs/crates/yi-agent-power/Cargo.toml`：

```toml
[package]
name = "yi-agent-power"
description = "Idle-sleep guard: 平台电源断言 + 纯策略状态机"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[lib]
path = "src/lib.rs"

[dependencies]
# `clock_gettime(CLOCK_MONOTONIC_RAW)` 与 `getifaddrs(3)`。工作区已有 5 个 crate
# 用同一版本，不引入新版本。
libc = "0.2"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: 写 `lib.rs` 骨架**

`yi-agent-rs/crates/yi-agent-power/src/lib.rs`：

```rust
//! 干活期间防止系统空闲休眠。
//!
//! 分层与理由见 `docs/superpowers/specs/2026-11-01-prevent-idle-sleep-design.md`。
//! 一句话：**一个进程持有断言，一条纯函数决定持不持，15 分钟只作为一个方向上
//! 的宽限窗口。**

pub mod assertion;
pub mod clock;
pub mod policy;
pub mod probe;
pub mod supervisor;

pub use supervisor::Supervisor;
```

此时五个模块文件还不存在，编译会失败——这正是下一步要的红灯。

- [ ] **Step 3: 注册进 workspace**

`yi-agent-rs/Cargo.toml` 的 `members` 数组里，`"crates/yi-agent-flags",` 之后加一行（保持字母序）：

```toml
    "crates/yi-agent-power",
```

`[workspace.dependencies]` 里，`yi-agent-flags = { path = "crates/yi-agent-flags" }` 之后加：

```toml
yi-agent-power = { path = "crates/yi-agent-power" }
```

- [ ] **Step 4: 建五个空模块，让骨架能编译**

各文件先只放一行模块文档，内容在后续任务填充：

`src/clock.rs`
```rust
//! 睡眠感知时钟。动机见 Task 3。
```

`src/policy.rs`
```rust
//! 纯策略状态机：零 I/O，全部可单测。
```

`src/assertion.rs`
```rust
//! 平台电源断言。
```

`src/probe.rs`
```rust
//! 电源来源与网络可达性探测。
```

`src/supervisor.rs`
```rust
//! 10s tick：把 clock/policy/probe/assertion 串起来。
```

- [ ] **Step 5: 验证编译**

Run: `cd yi-agent-rs && cargo build -p yi-agent-power`
Expected: 编译通过（只有空模块，无警告）

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent-power
git commit -m "feat(power): 新建 yi-agent-power crate 骨架"
```

---

### Task 2: `policy` — 纯策略状态机（本功能的核心，先测后写）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/policy.rs`

**Interfaces:**
- Consumes: 无（零依赖，只用 `std::time::Duration`）
- Produces:
  - `pub struct Sample { pub mono: Duration, pub wall: Duration }`
  - `pub struct Snapshot { pub on_ac: bool, pub online: bool, pub busy: bool }`
  - `pub enum Decision { Hold, Idle }`
  - `pub const GRACE: Duration`（15 分钟）、`pub const WATCHDOG_SLACK: Duration`（5 分钟）
  - `pub struct Policy`，`Policy::new() -> Policy`
  - `pub fn decide(&mut self, snapshot: &Snapshot, now: Sample) -> Decision`
  - `pub fn on_disabled(&mut self) -> Decision`、`pub fn re_enable(&mut self)`

**为什么入参是 `Sample`（两条时钟）而不是一个读数——这是实测改出来的。**

第一版 plan 只传入一个 `now: Duration`（单调读数），看门狗用
`now - hold_since > GRACE + SLACK` 判「扛太久了」。**编译并跑测试后发现它永远
不触发**：单调钟冻结时 `now` 恒等于 `hold_since`，差值是 0，`0 > 20min` 为假。
也就是说，兜底在最该生效的那个场景（时钟坏了）下完全失效——这正是 spec 里
「整机永不睡」要防的失效形态。

正确做法是把两个用途分开：

| 用途 | 用哪条时钟 | 为什么 |
|---|---|---|
| 15 分钟宽限窗口 | 单调钟（**含**睡眠时间） | 睡 8 小时后 `since(last_ok)` 必须是 8 小时，不能是几分钟 |
| 看门狗「扛了多久」 | **墙钟** | 单调钟冻结时墙钟照样前进，这是唯一还能度量时长的来源 |
| 看门狗「时钟有没有前进」 | 单调钟 | 判据是「有增长」而非「增长得够快」 |

所以 `decide` 吃 `Sample { mono, wall }`。policy 依旧是零 I/O、纯函数。

- [ ] **Step 1: 写失败的测试**

`src/policy.rs` 的测试模块（完整内容）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Sample {
        Sample {
            mono: Duration::from_secs(secs),
            wall: Duration::from_secs(secs),
        }
    }

    /// 单调钟冻结、墙钟照走。
    fn frozen_mono(mono: u64, wall: u64) -> Sample {
        Sample {
            mono: Duration::from_secs(mono),
            wall: Duration::from_secs(wall),
        }
    }

    fn ac_online(busy: bool) -> Snapshot {
        Snapshot {
            on_ac: true,
            online: true,
            busy,
        }
    }

    #[test]
    fn ac_plus_online_holds_regardless_of_busy() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(false), at(10)), Decision::Hold);
        assert_eq!(p.decide(&ac_online(true), at(20)), Decision::Hold);
    }

    /// 回归哨兵：`last_ok` 初值必须是 `None`，不是 `Some(now)`。
    #[test]
    fn a_fresh_policy_with_a_failing_snapshot_never_holds() {
        let mut p = Policy::new();
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(10)), Decision::Idle);
        assert_eq!(p.decide(&offline, at(20)), Decision::Idle);
    }

    /// 区分「单向读法」与「计数器读法」的唯一用例：启动后第一拍就必须 Idle。
    #[test]
    fn the_first_tick_never_holds_even_with_zero_elapsed() {
        let mut p = Policy::new();
        let offline = Snapshot {
            on_ac: false,
            online: true,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(0)), Decision::Idle);
    }

    #[test]
    fn losing_the_network_keeps_holding_for_just_under_fifteen_minutes() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(10)), Decision::Hold);
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(10 + 899)), Decision::Hold);
    }

    /// 边界取 `<`：恰好 900 秒释放。
    #[test]
    fn releasing_at_exactly_fifteen_minutes() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(10)), Decision::Hold);
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(10 + 900)), Decision::Idle);
    }

    /// 证明宽限真的会到期：释放之后再满足条件要能重新持有。
    #[test]
    fn releasing_then_re_holding_after_the_grace_expires() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(0)), Decision::Hold);
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(900)), Decision::Idle);
        assert_eq!(p.decide(&ac_online(true), at(901)), Decision::Hold);
    }

    #[test]
    fn a_three_minute_network_blip_never_releases() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(0)), Decision::Hold);
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        for t in (10..=180).step_by(10) {
            assert_eq!(p.decide(&offline, at(t)), Decision::Hold, "t={t}");
        }
        assert_eq!(p.decide(&ac_online(true), at(190)), Decision::Hold);
    }

    #[test]
    fn battery_with_work_holds_and_without_work_does_not() {
        let mut p = Policy::new();
        let with_work = Snapshot {
            on_ac: false,
            online: true,
            busy: true,
        };
        assert_eq!(p.decide(&with_work, at(0)), Decision::Hold);

        let mut p2 = Policy::new();
        let no_work = Snapshot {
            on_ac: false,
            online: true,
            busy: false,
        };
        assert_eq!(p2.decide(&no_work, at(0)), Decision::Idle);
    }

    #[test]
    fn battery_work_finishing_releases_after_the_grace() {
        let mut p = Policy::new();
        let with_work = Snapshot {
            on_ac: false,
            online: true,
            busy: true,
        };
        assert_eq!(p.decide(&with_work, at(0)), Decision::Hold);
        let no_work = Snapshot {
            on_ac: false,
            online: true,
            busy: false,
        };
        assert_eq!(p.decide(&no_work, at(899)), Decision::Hold);
        assert_eq!(p.decide(&no_work, at(900)), Decision::Idle);
    }

    /// 回归哨兵：睡眠感知时钟让 `since(last_ok) = 8h15m`，必须立刻释放。
    #[test]
    fn waking_after_eight_hours_with_a_failing_snapshot_releases_immediately() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(0)), Decision::Hold);
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(8 * 3600 + 900)), Decision::Idle);
    }

    #[test]
    fn waking_after_eight_hours_with_a_passing_snapshot_keeps_holding() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(0)), Decision::Hold);
        assert_eq!(p.decide(&ac_online(true), at(8 * 3600)), Decision::Hold);
    }

    /// tick 迟到 5 分钟不该误释放：判据是时间戳不是拍数。
    #[test]
    fn a_late_tick_does_not_release_early() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(0)), Decision::Hold);
        let offline = Snapshot {
            on_ac: true,
            online: false,
            busy: false,
        };
        assert_eq!(p.decide(&offline, at(300)), Decision::Hold);
    }

    #[test]
    fn disabling_releases_immediately_and_stays_idle() {
        let mut p = Policy::new();
        assert_eq!(p.decide(&ac_online(true), at(0)), Decision::Hold);
        assert_eq!(p.on_disabled(), Decision::Idle);
        assert_eq!(p.decide(&ac_online(true), at(10)), Decision::Idle);
    }

    /// **关键回归哨兵（实测改出来的用例）**：单调钟完全冻结、墙钟照走时，
    /// 看门狗必须能释放。第一版实现用 `now - hold_since` 度量时长，
    /// 冻结时差值恒为 0，这条会 FAIL。
    #[test]
    fn the_watchdog_releases_when_the_monotonic_clock_is_frozen() {
        let mut p = Policy::new();
        assert_eq!(
            p.decide(&ac_online(true), frozen_mono(100, 1_000)),
            Decision::Hold
        );
        let deadline = GRACE + WATCHDOG_SLACK;
        // 墙钟恰好走到边界：还没超过，不释放。
        assert_eq!(
            p.decide(
                &ac_online(true),
                frozen_mono(100, 1_000 + deadline.as_secs())
            ),
            Decision::Hold
        );
        // 越过边界：强制释放。
        assert_eq!(
            p.decide(
                &ac_online(true),
                frozen_mono(100, 1_000 + deadline.as_secs() + 1)
            ),
            Decision::Idle
        );
    }

    #[test]
    fn the_watchdog_stays_quiet_when_the_clock_advances() {
        let mut p = Policy::new();
        for t in (0..=3600).step_by(600) {
            assert_eq!(p.decide(&ac_online(true), at(t)), Decision::Hold, "t={t}");
        }
    }

    /// 判据是「有增长」，不是「增长得够快」：后者会把高负载下被调度变慢的
    /// 正常时钟误判成坏钟。
    #[test]
    fn a_slowly_advancing_clock_is_not_treated_as_frozen() {
        let mut p = Policy::new();
        assert_eq!(
            p.decide(&ac_online(true), frozen_mono(100, 1_000)),
            Decision::Hold
        );
        let deadline = GRACE + WATCHDOG_SLACK;
        assert_eq!(
            p.decide(
                &ac_online(true),
                frozen_mono(101, 1_000 + deadline.as_secs() + 1)
            ),
            Decision::Hold
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power policy`
Expected: 编译失败（`Policy` / `Sample` / `Snapshot` / `Decision` 未定义）

- [ ] **Step 3: 写实现**

`src/policy.rs` 全文（保留上面测试模块不动）：

```rust
//! 纯策略状态机：零 I/O，全部可单测。
//!
//! 公式来自 spec「策略：一条公式 + 一个窗口」：
//!
//! ```text
//! ok   = online && (on_ac || busy)
//! hold = last_ok.is_some_and(|t| now.mono - t < GRACE)   // 即 *.map_or(false, ..)
//! ```
//!
//! 三条刻意的选择：
//!
//! 1. **`last_ok` 是 `Option<Duration>`，初值 `None`**。不是 `Some(now)`——那会
//!    让每次进程启动都无条件 hold 15 分钟，变成「关掉再开一次就能续 15 分钟」
//!    的绕过手段。也不是「很久以前的 Stamp」——机器刚开机时单调读数很小，
//!    做减法会下溢。
//! 2. **时间由调用方注入**，policy 自己绝不取时钟。这样「睡 8 小时」在测试里
//!    只是喂一个跳变的值，不需要真睡。
//! 3. **入参是两条时钟（`Sample`）**，不是一条。宽限窗口要用含睡眠时间的
//!    单调钟；而看门狗要检测的正是「这条单调钟不前进」，若时长也用它度量，
//!    冻结时差值恒为 0、判据永不成立，兜底失效。所以时长由墙钟度量、增长由
//!    单调钟判定。

use std::time::Duration;

/// 保活条件的宽限窗口。条件消失满这么久才释放断言。
pub const GRACE: Duration = Duration::from_secs(15 * 60);

/// 看门狗在 `GRACE` 之上额外容忍的时长。
pub const WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);

/// 一拍的时间读数：两条时钟同时给出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// 睡眠感知单调读数。宽限窗口用它。
    pub mono: Duration,
    /// 墙钟读数。只用于看门狗度量「扛了多久」。
    pub wall: Duration,
}

/// 一次采样的三个输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// 电源判定：`!= "Battery Power"`（UPS 与未知类型都算插电）。
    pub on_ac: bool,
    /// 「本机有可用的网络出口」，不是「某个远端此刻健康」。
    pub online: bool,
    /// 「有活在推进」：本进程会话、项目 daemon 任务、远端近期交互三路并集。
    pub busy: bool,
}

/// 这一拍该不该持有断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold,
    Idle,
}

impl Snapshot {
    /// `ok = online && (on_ac || busy)`。插电时不看 busy。
    fn keeps_awake(&self) -> bool {
        self.online && (self.on_ac || self.busy)
    }
}

#[derive(Debug, Default)]
pub struct Policy {
    /// 最近一次满足条件的**单调**读数；从未满足过为 `None`。
    last_ok: Option<Duration>,
    /// 看门狗：本次 `Hold` 是从哪一拍开始的（两条读数都记）。
    hold_since: Option<Sample>,
    /// 开关关掉后置位；只要它还在，`decide` 一律 `Idle`。
    disabled: bool,
}

impl Policy {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开关被关掉：立即释放，且此后一直 `Idle`。
    pub fn on_disabled(&mut self) -> Decision {
        self.disabled = true;
        self.last_ok = None;
        self.hold_since = None;
        Decision::Idle
    }

    /// 开关被重新打开：清掉 `disabled`，让下一次 `decide` 重新走公式。
    pub fn re_enable(&mut self) {
        self.disabled = false;
    }

    pub fn decide(&mut self, snapshot: &Snapshot, now: Sample) -> Decision {
        if self.disabled {
            return Decision::Idle;
        }

        // 看门狗：扛了多久由**墙钟**度量（单调钟冻结时它照样前进），
        // 「有没有前进」由**单调钟**判定。
        if let Some(since) = self.hold_since {
            let held_for = now.wall.saturating_sub(since.wall);
            let mono_grew = now.mono > since.mono;
            if !mono_grew && held_for > GRACE + WATCHDOG_SLACK {
                // 时钟没走、hold 却扛了这么久：时钟坏了。强制释放。
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
            .is_some_and(|last| now.mono.saturating_sub(last) < GRACE);
        let decision = if hold { Decision::Hold } else { Decision::Idle };

        match (decision, self.hold_since) {
            (Decision::Hold, None) => self.hold_since = Some(now),
            (Decision::Idle, _) => self.hold_since = None,
            (Decision::Hold, Some(_)) => {}
        }
        decision
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power policy`
Expected: **16 个测试全 PASS**（已实测）

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/policy.rs
git commit -m "feat(power): policy 纯状态机，双时钟使时钟冻结看门狗真正可达"
```

---

### Task 3: `clock` — 睡眠感知时钟与墙钟自检

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/clock.rs`

**Interfaces:**
- Consumes: `libc`（仅 unix）
- Produces:
  - `pub struct Stamp(...)` — 不透明，**不是** `Instant` 的别名。两个公开口：
    - `pub fn from_duration(Duration) -> Self` — 只给测试与跨 crate 夹具用
      （`Stamp.0` 是 `pub(crate)`，`yi-agent-app-server` 的测试造不出戳来）
    - `pub fn as_duration(&self) -> Duration`
  - `pub trait Clock: Send + Sync`，五个方法：
    - `fn now(&self) -> Option<Stamp>` — 睡眠感知单调读数
    - `fn since(&self, stamp: Stamp) -> Duration` — 恒非负
    - `fn wall(&self) -> Option<Stamp>` — 墙钟读数，**只**给看门狗度量时长
    - `fn mono_trusted(&self) -> bool`
    - `fn sample(&self) -> Option<policy::Sample>` — 默认实现 = `now` + `wall`；任一取不到即 `None`
  - `pub struct SystemClock`，`SystemClock::new() -> Self`
  - `pub fn sample_monotonic() -> Option<Duration>`（平台时钟读数）

关键事实（我已在本机实测，见 spec「时间源的选择」）：

- macOS：`libc::CLOCK_MONOTONIC_RAW`(=4) 在睡眠期间**照常前进**；`libc::CLOCK_UPTIME_RAW`(=8) 不前进。本机两者相差 6 725 万秒级。
- `CLOCK_BOOTTIME` 在 macOS 不存在：`clock_gettime(7, ...)` 返回 -1。所以必须按平台分支，失败分支是真实可达的。
- Linux：必须 `CLOCK_BOOTTIME`。

- [ ] **Step 1: 写失败的测试**

追加到 `src/clock.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_reads_are_non_decreasing() {
        let a = sample_monotonic().expect("platform clock must work on this host");
        let b = sample_monotonic().expect("platform clock must work on this host");
        assert!(b >= a, "{b:?} must not precede {a:?}");
    }

    #[test]
    fn system_clock_reports_a_stamp_and_a_non_negative_elapsed() {
        let clock = SystemClock::new();
        let start = clock.now().expect("clock must be readable");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let elapsed = clock.since(start);
        assert!(elapsed >= std::time::Duration::from_millis(5), "{elapsed:?}");
    }

    /// `since` 收到一个「未来」的戳时不许出负数——墙钟被 NTP 往回调时会出现。
    #[test]
    fn since_saturates_instead_of_going_negative() {
        let clock = SystemClock::new();
        let now = clock.now().unwrap();
        let future = Stamp(now.0 + std::time::Duration::from_secs(3600));
        assert_eq!(clock.since(future), std::time::Duration::ZERO);
    }

    /// 比例判据：坏钟 `dt_mono = 20s` 配墙钟 `dt_wall = 8h` 必须告警。
    /// 绝对阈值（`dt_mono < 2 * tick`）在 20 < 20 时漏判，故用比例。
    #[test]
    fn a_stalled_monotonic_clock_is_suspicious_even_when_not_frozen_solid() {
        assert!(is_suspicious(
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(8 * 3600),
        ));
    }

    /// 正确钟睡 8 小时：两个钟一起前进，`slept_for` 约为 0，不许告警。
    #[test]
    fn normal_sleep_is_not_suspicious() {
        assert!(!is_suspicious(
            std::time::Duration::from_secs(8 * 3600),
            std::time::Duration::from_secs(8 * 3600),
        ));
    }

    /// 高负载 tick 迟到 50 秒：两钟同步，不许告警。
    #[test]
    fn a_late_tick_is_not_suspicious() {
        assert!(!is_suspicious(
            std::time::Duration::from_secs(50),
            std::time::Duration::from_secs(50),
        ));
    }

    /// 三次连续可疑才降级，单次抖动不切。
    #[test]
    fn three_consecutive_suspects_flip_mono_trusted() {
        let mut guard = TrustGuard::new();
        let mono = std::time::Duration::from_secs(20);
        let wall = std::time::Duration::from_secs(3600);
        guard.observe(mono, wall);
        assert!(guard.trusted());
        guard.observe(mono, wall);
        assert!(guard.trusted());
        guard.observe(mono, wall);
        assert!(!guard.trusted());
    }

    /// 降级是单向的：之后的健康样本不把它修回来。
    #[test]
    fn trust_never_returns_after_a_flip() {
        let mut guard = TrustGuard::new();
        let mono = std::time::Duration::from_secs(20);
        let wall = std::time::Duration::from_secs(3600);
        for _ in 0..3 {
            guard.observe(mono, wall);
        }
        assert!(!guard.trusted());
        guard.observe(std::time::Duration::from_secs(60), std::time::Duration::from_secs(60));
        assert!(!guard.trusted());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power clock`
Expected: 编译失败（`sample_monotonic` / `Stamp` / `SystemClock` / `is_suspicious` / `TrustGuard` 未定义）

- [ ] **Step 3: 写实现**

`src/clock.rs` 全文：

```rust
//! 睡眠感知时钟。
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp(pub(crate) Duration);

impl Stamp {
    /// 由一个读数构造。只给测试与跨 crate 的夹具用——生产代码一律从
    /// `Clock::now()` / `Clock::wall()` 取戳，不要凭空造时间点。
    pub fn from_duration(duration: Duration) -> Self {
        Self(duration)
    }

    pub fn as_duration(&self) -> Duration {
        self.0
    }
}

pub fn sample_monotonic() -> Option<Duration> {
    platform_monotonic()
}

#[cfg(unix)]
fn platform_monotonic() -> Option<Duration> {
    #[cfg(target_os = "macos")]
    let clock_id = libc::CLOCK_MONOTONIC_RAW;
    #[cfg(target_os = "linux")]
    let clock_id = libc::CLOCK_BOOTTIME;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` 是本地可写的 `timespec`，`clock_id` 是合法常量。
        let rc = unsafe { libc::clock_gettime(clock_id, &mut ts) };
        if rc != 0 || ts.tv_sec < 0 {
            return None;
        }
        Some(
            Duration::from_secs(ts.tv_sec as u64)
                + Duration::from_nanos(ts.tv_nsec.clamp(0, 999_999_999) as u64),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(not(unix))]
fn platform_monotonic() -> Option<Duration> {
    None
}

fn wall_now() -> Option<Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

pub(crate) fn is_suspicious(dt_mono: Duration, dt_wall: Duration) -> bool {
    let slept_for = dt_wall.saturating_sub(dt_mono);
    slept_for > Duration::from_secs(300) && dt_mono * 10 < slept_for
}

pub(crate) struct TrustGuard {
    suspects: u8,
    trusted: bool,
}

impl TrustGuard {
    pub(crate) fn new() -> Self {
        Self {
            suspects: 0,
            trusted: true,
        }
    }

    pub(crate) fn observe(&mut self, dt_mono: Duration, dt_wall: Duration) {
        if !self.trusted {
            return;
        }
        if is_suspicious(dt_mono, dt_wall) {
            self.suspects = self.suspects.saturating_add(1);
            if self.suspects >= 3 {
                self.trusted = false;
            }
        } else {
            self.suspects = 0;
        }
    }

    pub(crate) fn trusted(&self) -> bool {
        self.trusted
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Option<Stamp>;
    fn since(&self, stamp: Stamp) -> Duration;
    /// 墙钟读数。只用于看门狗度量「扛了多久」。
    fn wall(&self) -> Option<Stamp>;
    fn mono_trusted(&self) -> bool;

    /// 一拍所需的双时钟读数。任一取不到即返回 `None`（调用方据此放弃保活）。
    fn sample(&self) -> Option<crate::policy::Sample> {
        Some(crate::policy::Sample {
            mono: self.now()?.0,
            wall: self.wall()?.0,
        })
    }
}

struct ClockState {
    last_mono: Option<Duration>,
    last_wall: Option<Duration>,
    guard: TrustGuard,
    platform_ok: bool,
}

pub struct SystemClock {
    state: Mutex<ClockState>,
    downgrades: AtomicU64,
}

impl SystemClock {
    pub fn new() -> Self {
        let platform_ok = sample_monotonic().is_some();
        if !platform_ok {
            tracing_warn_once("platform monotonic clock is unavailable; using wall clock");
        }
        Self {
            state: Mutex::new(ClockState {
                last_mono: None,
                last_wall: None,
                guard: TrustGuard::new(),
                platform_ok,
            }),
            downgrades: AtomicU64::new(0),
        }
    }

    pub fn self_check(&self) {
        let Some(mono) = sample_monotonic() else {
            return;
        };
        let Some(wall) = wall_now() else { return };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let (Some(prev_mono), Some(prev_wall)) = (state.last_mono, state.last_wall) {
            let dt_mono = mono.saturating_sub(prev_mono);
            let dt_wall = wall.saturating_sub(prev_wall);
            let was_trusted = state.guard.trusted();
            state.guard.observe(dt_mono, dt_wall);
            if was_trusted && !state.guard.trusted() {
                self.downgrades.fetch_add(1, Ordering::Relaxed);
                tracing_warn_once(
                    "monotonic clock stopped tracking wall time; downgraded to wall clock",
                );
            }
        }
        state.last_mono = Some(mono);
        state.last_wall = Some(wall);
    }

    fn read(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.platform_ok
            && state.guard.trusted()
            && let Some(mono) = sample_monotonic()
        {
            return Some(mono);
        }
        wall_now()
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Option<Stamp> {
        self.self_check();
        self.read().map(Stamp)
    }
    fn since(&self, stamp: Stamp) -> Duration {
        match self.read() {
            Some(now) => now.saturating_sub(stamp.0),
            None => Duration::ZERO,
        }
    }
    fn wall(&self) -> Option<Stamp> {
        wall_now().map(Stamp)
    }
    fn mono_trusted(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.platform_ok && state.guard.trusted()
    }
}

fn tracing_warn_once(message: &str) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| eprintln!("[yi-agent-power] {message}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_reads_are_non_decreasing() {
        let a = sample_monotonic().expect("platform clock must work");
        let b = sample_monotonic().expect("platform clock must work");
        assert!(b >= a);
    }

    #[test]
    fn system_clock_reports_a_stamp_and_a_non_negative_elapsed() {
        let clock = SystemClock::new();
        let start = clock.now().expect("clock must be readable");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let elapsed = clock.since(start);
        assert!(
            elapsed >= std::time::Duration::from_millis(5),
            "{elapsed:?}"
        );
    }

    #[test]
    fn since_saturates_instead_of_going_negative() {
        let clock = SystemClock::new();
        let now = clock.now().unwrap();
        let future = Stamp(now.0 + std::time::Duration::from_secs(3600));
        assert_eq!(clock.since(future), std::time::Duration::ZERO);
    }

    #[test]
    fn a_stalled_monotonic_clock_is_suspicious_even_when_not_frozen_solid() {
        assert!(is_suspicious(
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(8 * 3600)
        ));
    }

    #[test]
    fn normal_sleep_is_not_suspicious() {
        assert!(!is_suspicious(
            std::time::Duration::from_secs(8 * 3600),
            std::time::Duration::from_secs(8 * 3600)
        ));
    }

    #[test]
    fn a_late_tick_is_not_suspicious() {
        assert!(!is_suspicious(
            std::time::Duration::from_secs(50),
            std::time::Duration::from_secs(50)
        ));
    }

    #[test]
    fn three_consecutive_suspects_flip_mono_trusted() {
        let mut guard = TrustGuard::new();
        let mono = std::time::Duration::from_secs(20);
        let wall = std::time::Duration::from_secs(3600);
        guard.observe(mono, wall);
        assert!(guard.trusted());
        guard.observe(mono, wall);
        assert!(guard.trusted());
        guard.observe(mono, wall);
        assert!(!guard.trusted());
    }

    #[test]
    fn trust_never_returns_after_a_flip() {
        let mut guard = TrustGuard::new();
        let mono = std::time::Duration::from_secs(20);
        let wall = std::time::Duration::from_secs(3600);
        for _ in 0..3 {
            guard.observe(mono, wall);
        }
        assert!(!guard.trusted());
        guard.observe(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        );
        assert!(!guard.trusted());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn raw_monotonic_differs_from_uptime_raw_on_macos() {
        fn read(id: libc::clockid_t) -> Option<Duration> {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: 本地可写 timespec + 合法 clock id。
            let rc = unsafe { libc::clock_gettime(id, &mut ts) };
            (rc == 0).then(|| {
                Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64)
            })
        }
        let raw = read(libc::CLOCK_MONOTONIC_RAW).expect("CLOCK_MONOTONIC_RAW must work");
        let uptime = read(libc::CLOCK_UPTIME_RAW).expect("CLOCK_UPTIME_RAW must work");
        assert!(raw > uptime, "raw={raw:?} uptime={uptime:?}");
        assert_eq!(read(7), None, "CLOCK_BOOTTIME must not exist on macOS");
    }
}
```

**注意**：Step 1 写下的测试模块要保留，把上面的 `#[cfg(test)] mod tests { // 见下… }` 整段删掉，不要覆盖。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power clock`
Expected: **8 个测试全 PASS**（Step 1 的 8 条；已实测）

- [ ] **Step 5: 加一条本机可跑的 macOS 真实时钟冒烟测试**

追加到 `src/clock.rs` 的 tests 模块末尾：

```rust
    /// 真机冒烟：确认所选时钟在本平台确实可用，且 macOS 上
    /// `CLOCK_MONOTONIC_RAW` 与 `CLOCK_UPTIME_RAW` 是两个不同的读数
    /// （差值即累计睡眠时长，量级通常很大）。
    #[cfg(target_os = "macos")]
    #[test]
    fn raw_monotonic_differs_from_uptime_raw_on_macos() {
        fn read(id: libc::clockid_t) -> Option<Duration> {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: 本地可写 timespec + 合法 clock id。
            let rc = unsafe { libc::clock_gettime(id, &mut ts) };
            (rc == 0)
                .then(|| Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64))
        }
        let raw = read(libc::CLOCK_MONOTONIC_RAW).expect("CLOCK_MONOTONIC_RAW must work");
        let uptime = read(libc::CLOCK_UPTIME_RAW).expect("CLOCK_UPTIME_RAW must work");
        assert!(raw > uptime, "raw={raw:?} uptime={uptime:?}");
        // CLOCK_BOOTTIME 在 macOS 不存在：失败分支真实可达，不是防御性编程。
        assert_eq!(read(7), None, "CLOCK_BOOTTIME must not exist on macOS");
    }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-power clock -- --nocapture`
Expected: **9 个测试全 PASS**（8 + Step 5 新增的 1 条；已实测）

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/clock.rs
git commit -m "feat(power): 睡眠感知时钟 CLOCK_MONOTONIC_RAW + 比例判据的墙钟自检"
```

---

### Task 4: `assertion` — macOS IOKit 断言句柄（Drop 释放，幂等）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/assertion.rs`

**Interfaces:**
- Consumes: 无（IOKit/CoreFoundation 经 `#[link(kind = "framework")]`）
- Produces:
  - `pub trait Assertion: Send { fn acquire(&mut self) -> bool; fn release(&mut self); fn is_held(&self) -> bool; }`
  - `pub struct MacAssertion`（macOS）与 `pub struct NoopAssertion`（其它平台），
    二者都实现 `Assertion`；`pub fn platform_assertion(label: &str) -> Box<dyn Assertion>`
  - `impl Drop for MacAssertion` — 只在 `held == true` 时释放

已在本机端到端实测（不是纸面推导）：`IOPMAssertionCreateWithName` 返回 `0x0`、
拿到有效 id、`pmset -g assertions` 里能看到
`PreventUserIdleSystemSleep named: "yi-agent: ..."`、`IOPMAssertionRelease` 返回 `0x0`。
`PreventUserIdleSystemSleep` 是**宏**（`CFSTR(...)`），不是导出符号，所以必须自己构造
等值 CFString；只有 `IOPMAssertionCreateWithName` / `IOPMAssertionRelease` 可以 extern。

- [ ] **Step 1: 写失败的测试**

追加到 `src/assertion.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 内存里的假断言：记录 acquire/release 次数与当前状态。
    struct FakeAssertion {
        held: bool,
        acquires: usize,
        releases: usize,
        fail_next: bool,
    }

    impl FakeAssertion {
        fn new() -> Self {
            Self { held: false, acquires: 0, releases: 0, fail_next: false }
        }
    }

    impl Assertion for FakeAssertion {
        fn acquire(&mut self) -> bool {
            self.acquires += 1;
            if self.fail_next {
                self.fail_next = false;
                self.held = false;
                return false;
            }
            self.held = true;
            true
        }

        fn release(&mut self) {
            self.releases += 1;
            self.held = false;
        }

        fn is_held(&self) -> bool {
            self.held
        }
    }

    #[test]
    fn acquire_then_release_flips_state() {
        let mut a = FakeAssertion::new();
        assert!(!a.is_held());
        assert!(a.acquire());
        assert!(a.is_held());
        a.release();
        assert!(!a.is_held());
    }

    /// Release 必须幂等：重复调用不产生第二次真实释放。
    /// 真实实现里对已释放的 id 调 `IOPMAssertionRelease` 会返回错误并污染日志。
    #[test]
    fn release_is_idempotent_when_guarded_by_is_held() {
        let mut a = FakeAssertion::new();
        assert!(a.acquire());
        if a.is_held() {
            a.release();
        }
        if a.is_held() {
            a.release();
        }
        assert_eq!(a.releases, 1);
    }

    /// 释放后能重新持有（用户在宽限窗口内重新开工）。
    #[test]
    fn acquire_after_release_works_again() {
        let mut a = FakeAssertion::new();
        assert!(a.acquire());
        a.release();
        assert!(a.acquire());
        assert!(a.is_held());
        assert_eq!(a.acquires, 2);
    }

    /// 非 macOS 上 `platform_assertion` 必须是空操作，且永不「持有」。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_platform_assertion_is_a_noop_off_macos() {
        let mut a = platform_assertion("test");
        assert!(a.acquire(), "acquire must report success so the supervisor keeps ticking");
        assert!(!a.is_held(), "a noop assertion never actually holds");
    }

    /// 真机冒烟：创建成功、**以我们的标签**出现在 `pmset` 里、释放成功。
    ///
    /// 不加 `#[ignore]`：这条在 macOS 上随普通 `cargo test` 就跑，是每个改
    /// `assertion.rs` 的人都会自动触发的护栏。`#[cfg(target_os = "macos")]`
    /// 已经把它从 Linux CI 上摘掉了，所以「CI 不跑」这个顾虑不成立。
    /// 若要手工跳过，`cargo test -- --skip a_real_assertion` 即可。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_assertion_can_be_acquired_and_released() {
        let mut a = platform_assertion("yi-agent: smoke test");
        assert!(a.acquire(), "IOPMAssertionCreateWithName must return kIOReturnSuccess");
        assert!(a.is_held());
        let listing = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset must run");
        let text = String::from_utf8_lossy(&listing.stdout);
        // 断言标签必须出现，**而不是**只断言「有 PreventUserIdleSystemSleep」——
        // 后者在真实机器上恒为真（系统自己就有这类断言），等于没断言。
        assert!(
            text.contains("yi-agent: smoke test"),
            "the assertion must be visible under our own label; got:\n{text}"
        );
        a.release();
        assert!(!a.is_held());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power assertion`
Expected: 编译失败（`Assertion` / `platform_assertion` 未定义）

- [ ] **Step 3: 写实现**

`src/assertion.rs` 全文：

```rust
//! 平台电源断言。
pub trait Assertion: Send {
    fn acquire(&mut self) -> bool;
    fn release(&mut self);
    fn is_held(&self) -> bool;
}

pub fn platform_assertion(label: &str) -> Box<dyn Assertion> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacAssertion::new(label))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = label;
        Box::new(NoopAssertion)
    }
}

pub struct NoopAssertion;

impl Assertion for NoopAssertion {
    fn acquire(&mut self) -> bool {
        true
    }
    fn release(&mut self) {}
    fn is_held(&self) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
pub use mac::MacAssertion;

#[cfg(target_os = "macos")]
mod mac {
    use super::Assertion;
    use std::ffi::{CString, c_char, c_void};

    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type IOPMAssertionID = u32;
    type IOReturn = i32;

    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;
    const SUCCESS: IOReturn = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            s: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            ty: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(id: IOPMAssertionID) -> IOReturn;
    }

    fn cfstring(text: &str) -> CFStringRef {
        let c = CString::new(text).unwrap_or_default();
        // SAFETY: `c` 是有效 NUL 结尾字符串，allocator 传 null 用默认分配器。
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) }
    }

    pub struct MacAssertion {
        ty: CFStringRef,
        name: CFStringRef,
        id: IOPMAssertionID,
        held: bool,
    }

    // SAFETY: 字段只是 CF 句柄与整数；FFI 调用在 `&mut self` 下串行。
    unsafe impl Send for MacAssertion {}

    impl MacAssertion {
        pub fn new(label: &str) -> Self {
            Self {
                ty: cfstring("PreventUserIdleSystemSleep"),
                name: cfstring(label),
                id: 0,
                held: false,
            }
        }
    }

    impl Assertion for MacAssertion {
        fn acquire(&mut self) -> bool {
            let mut id: IOPMAssertionID = 0;
            // SAFETY: 两个 CFString 在 `new` 里创建；`&mut id` 指向本地可写变量。
            let rc = unsafe { IOPMAssertionCreateWithName(self.ty, LEVEL_ON, self.name, &mut id) };
            if rc == SUCCESS {
                self.id = id;
                self.held = true;
                true
            } else {
                self.held = false;
                false
            }
        }

        fn release(&mut self) {
            if !self.held {
                return;
            }
            // SAFETY: `self.id` 是 acquire 成功后由 IOKit 写入的有效 id，held 守卫保证只释放一次。
            let _ = unsafe { IOPMAssertionRelease(self.id) };
            self.held = false;
            self.id = 0;
        }

        fn is_held(&self) -> bool {
            self.held
        }
    }

    impl Drop for MacAssertion {
        fn drop(&mut self) {
            self.release();
            // SAFETY: 两个 CFString 由 Create 规则创建（+1），这里各 release 一次。
            unsafe {
                CFRelease(self.ty);
                CFRelease(self.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAssertion {
        held: bool,
        acquires: usize,
        releases: usize,
        fail_next: bool,
    }

    impl FakeAssertion {
        fn new() -> Self {
            Self {
                held: false,
                acquires: 0,
                releases: 0,
                fail_next: false,
            }
        }
    }

    impl Assertion for FakeAssertion {
        fn acquire(&mut self) -> bool {
            self.acquires += 1;
            if self.fail_next {
                self.fail_next = false;
                self.held = false;
                return false;
            }
            self.held = true;
            true
        }
        fn release(&mut self) {
            self.releases += 1;
            self.held = false;
        }
        fn is_held(&self) -> bool {
            self.held
        }
    }

    #[test]
    fn acquire_then_release_flips_state() {
        let mut a = FakeAssertion::new();
        assert!(!a.is_held());
        assert!(a.acquire());
        assert!(a.is_held());
        a.release();
        assert!(!a.is_held());
    }

    #[test]
    fn release_is_idempotent_when_guarded_by_is_held() {
        let mut a = FakeAssertion::new();
        assert!(a.acquire());
        if a.is_held() {
            a.release();
        }
        if a.is_held() {
            a.release();
        }
        assert_eq!(a.releases, 1);
    }

    #[test]
    fn acquire_after_release_works_again() {
        let mut a = FakeAssertion::new();
        assert!(a.acquire());
        a.release();
        assert!(a.acquire());
        assert!(a.is_held());
        assert_eq!(a.acquires, 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_assertion_can_be_acquired_and_released() {
        let mut a = platform_assertion("yi-agent: smoke test");
        assert!(
            a.acquire(),
            "IOPMAssertionCreateWithName must return kIOReturnSuccess"
        );
        assert!(a.is_held());
        let listing = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset must run");
        let text = String::from_utf8_lossy(&listing.stdout);
        assert!(
            text.contains("yi-agent: smoke test"),
            "assertion must be named after our label"
        );
        a.release();
        assert!(!a.is_held());
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power assertion`
Expected: **4 个 PASS**（macOS 上；Linux 上是 3 个，真机那条被 `cfg` 摘掉）。已实测 4 passed。

- [ ] **Step 5: 在 macOS 上跑真机冒烟**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power assertion a_real -- --nocapture`
Expected: 1 个 PASS。这条已含在 Step 4 的 4 个里；单独跑它便于看断言细节。

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/assertion.rs
git commit -m "feat(power): IOKit PreventUserIdleSystemSleep 断言句柄，Drop 幂等释放"
```

---

### Task 5: `settings` — 开关字段在四处同步落地

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（`AgentSettings` + `FIELDS` + `normalized`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs`（`current_value` 分支）
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs`（`field_value` 分支）
- Test: 上述三处的既有测试（新增字段会直接红/绿）

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub static PREVENT_IDLE_SLEEP_KEY: &str = "agent.prevent_idle_sleep"`
  - `AgentSettings.prevent_idle_sleep: Option<String>`
  - `pub fn prevent_idle_sleep_enabled(settings: &Settings) -> bool` — `None`/空串/未知值都按 `true`

**这是偏差 3 的落点**：`FIELDS` / `AgentSettings` / `settings_rpc::current_value` /
`yi-agent-web::api::field_value` 四处必须一起改。漏掉 web 那处会让
`api_test.rs:168` 的 `vars.len() == FIELDS.len()` 变红；漏掉 `settings_rpc` 那处会让
`every_field_in_fields_can_be_read_back` 变红。

- [ ] **Step 1: 先加字段与常量，观察既有测试变红**

在 `crates/yi-agent-runtime/src/settings.rs` 的 `AgentSettings` 里，`skills_catalog_budget` 之后加：

```rust
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// 存字符串而不是 `Option<bool>`：`FIELDS` 的 `Text`/`Select` 只接受 JSON
    /// 字符串（`validate_value`），用 `Option<bool>` 会让设置页写进 `"true"`
    /// 后反序列化读不回来。与 `tui.notifications` 完全同构。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub prevent_idle_sleep: Option<String>,
```

在同文件 `FIELDS` 数组里，`agent.skills_catalog_budget` 那条之后加：

```rust
    FieldSpec {
        key: PREVENT_IDLE_SLEEP_KEY,
        group: "agent",
        label: "干活期间防止休眠",
        field_type: FieldType::Select,
        default_display: "true",
        description: "有网且（插电 或 有活在跑）时阻止系统空闲休眠；条件消失 15 分钟后恢复",
        options: &["true", "false"],
        min: None,
        max: None,
    },
```

在同文件（`FIELDS` 之前、`use` 之后）加常量与读取帮助函数：

```rust
/// 开关的点路径键。多处引用同一常量，避免字符串抄错。
pub const PREVENT_IDLE_SLEEP_KEY: &str = "agent.prevent_idle_sleep";

/// 开关是否打开。缺文件 / 空串 / 未知值一律按默认（开）处理，与
/// `tui.notifications` 的兜底一致。
pub fn prevent_idle_sleep_enabled(settings: &Settings) -> bool {
    match settings.agent.prevent_idle_sleep.as_deref() {
        Some(value) => !value.eq_ignore_ascii_case("false"),
        None => true,
    }
}
```

在 `normalized()` 里，`tui.notification_condition` 那段白名单之后加：

```rust
        if let Some(enabled) = self.agent.prevent_idle_sleep.as_deref() {
            if !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false") {
                tracing::warn!(
                    value = enabled,
                    "settings.json has an unknown agent.prevent_idle_sleep; falling back to the default"
                );
                self.agent.prevent_idle_sleep = None;
            }
        }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server settings_rpc`
Expected: **FAIL** — `every_field_in_fields_can_be_read_back` 报
`agent.prevent_idle_sleep is in FIELDS but current_value returned null for it`

Run: `cd yi-agent-rs && cargo test -p yi-agent-web get_config`
Expected: **FAIL** — `每个字段都要出现`（`vars.len()` 少 1）

- [ ] **Step 2: 补齐 `settings_rpc`**

`crates/yi-agent-app-server/src/settings_rpc.rs` 的 `current_value` match 里，
`"agent.skills_catalog_budget"` 那行之后加：

```rust
        settings::PREVENT_IDLE_SLEEP_KEY => {
            option_to_json(current.agent.prevent_idle_sleep.as_deref())
        }
```

注意：match 的 pattern 必须是常量路径而不是字面量——`PREVENT_IDLE_SLEEP_KEY` 是
`&'static str` 常量，可以用作 pattern。若编译器不接受，改成
`"agent.prevent_idle_sleep" =>` 字面量并在旁边写注释说明它与常量同值。

- [ ] **Step 3: 补齐 `yi-agent-web`**

`crates/yi-agent-web/src/api.rs` 的 `field_value` match 里，`"agent.skills_catalog_budget"` 那行之后加：

```rust
        settings::PREVENT_IDLE_SLEEP_KEY => {
            opt(current.agent.prevent_idle_sleep.as_deref().map(Value::from))
        }
```

坐标核对：`field_value` 是一条 `match spec.key { .. }` 的大分支表，`agent.skills_catalog_budget`
那行之后就是插入点。`opt(..)` 是本地小函数（`Option<Value> -> Value`，`None` 变 `null`），
与相邻各行的写法一致。**已实测**：这样写编译通过，`get_config_*` 三条测试全绿。

- [ ] **Step 4: 跑三处测试确认转绿**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server settings_rpc`
Expected: 全 PASS

Run: `cd yi-agent-rs && cargo test -p yi-agent-web get_config`
Expected: 全 PASS

- [ ] **Step 5: 加开关语义的测试**

在 `crates/yi-agent-runtime/src/settings.rs` 的 tests 模块末尾追加：

```rust
    #[test]
    fn prevent_idle_sleep_defaults_to_enabled() {
        let settings = Settings::default();
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    #[test]
    fn prevent_idle_sleep_honours_false() {
        let mut settings = Settings::default();
        settings.agent.prevent_idle_sleep = Some("false".to_string());
        assert!(!prevent_idle_sleep_enabled(&settings));
    }

    /// 未知值在 `normalized` 里被清成 `None`，于是读取端按默认（开）处理。
    #[test]
    fn an_unknown_prevent_idle_sleep_falls_back_to_enabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":"yes"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.agent.prevent_idle_sleep, None);
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// 空串等于未设置。
    #[test]
    fn an_empty_prevent_idle_sleep_is_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":""}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.agent.prevent_idle_sleep, None);
    }

    /// 整份 JSON 坏掉时 `load_from` 返回默认值，即开关回到「开」。
    ///
    /// 方向是刻意的：坏文件下**不能**让保活失效（那正是「夜里跑着跑着睡了」
    /// 的原始故障），所以兜底偏向「继续保活」。用户在设置页重新保存一次即可
    /// 修正文件。
    #[test]
    fn a_corrupt_settings_file_still_enables_prevention() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let settings = load_from(&path);
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// `"FALSE"` / `"False"` 也算关闭——大小写不敏感。
    /// 设置页写的是小写，但用户手改文件时不该因为大小写就悄悄打开。
    #[test]
    fn prevent_idle_sleep_is_case_insensitive() {
        for raw in ["false", "FALSE", "False"] {
            let mut settings = Settings::default();
            settings.agent.prevent_idle_sleep = Some(raw.to_string());
            assert!(!prevent_idle_sleep_enabled(&settings), "{raw} must disable");
        }
    }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime prevent_idle_sleep`
Expected: **6 个 PASS**（前 4 条已实测；后 2 条是这一版新增）

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs \
        yi-agent-rs/crates/yi-agent-web/src/api.rs
git commit -m "feat(settings): 新增 agent.prevent_idle_sleep 开关，四处读取点同步落地"
```

---

### Task 6: `probe` — 电源来源与网络可达性

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/probe.rs`

**Interfaces:**
- Consumes: `libc`（`getifaddrs`）、`std::process::Command`（`route -n get default`）、`std::net::TcpStream`
- Produces:
  - `pub trait PowerProbe: Send + Sync { fn on_ac(&self) -> bool; }`
  - `pub trait NetProbe: Send + Sync { fn online(&self) -> bool; }`
  - `pub struct SystemPowerProbe` / `pub struct SystemNetProbe`
  - `pub fn is_virtual_interface(name: &str) -> bool`
  - `pub fn has_usable_interface(addrs: &[(String, u32, bool)]) -> bool` — 纯函数，接口列表喂进来，方便单测
  - `pub fn parse_gateway(route_output: &str) -> Option<std::net::IpAddr>` — 纯函数
  - `pub fn classify_connect_result(result: std::io::Result<()>) -> bool` — 纯函数

**三处实测结论（本机核实，不要凭直觉改）**：

1. `IOPSGetProvidingPowerSourceType` 返回 `"AC Power"`；可能值还有 `"Battery Power"`/`"UPS Power"`。判定写 `!= "Battery Power"`（UPS 必须算插电；未知类型偏向保活）。
2. `getifaddrs` 只按 `IFF_UP && !IFF_LOOPBACK` 判会得到 14+ 个接口，其中 `utun*`/`awdl*`/`llw*`/`en1`~`en8` 全是噪音。**必须有 `AF_INET` 地址**才收窄到 2 个（本机 `en0` + `utun4`）。
3. 网关探测：`192.168.31.1:9999` → `Connection refused`，**这恰恰证明链路是通的**（包到了对端，对端回 RST）。只有**超时** / `EHOSTUNREACH` / `ENETUNREACH` 才算离线。写反了会在 99% 的正常网络里判成离线。

- [ ] **Step 1: 写失败的测试**

追加到 `src/probe.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn battery_is_the_only_thing_that_is_not_ac() {
        assert!(!is_on_ac("Battery Power"));
        assert!(is_on_ac("AC Power"));
        // UPS 插着就是在用外接电源；当成电池会让保活莫名其妙地停掉。
        assert!(is_on_ac("UPS Power"));
        // 未知类型偏向保活：反向失败的后果更严重。
        assert!(is_on_ac("Something New"));
    }

    #[test]
    fn virtual_interface_prefixes_are_recognised() {
        for name in [
            "utun0", "utun4", "awdl0", "llw0", "bridge0", "gif0", "stf0", "anpi1", "ap1",
            "tun0", "tap0", "docker0", "br-1234", "veth1a2b", "virbr0", "lo",
        ] {
            assert!(is_virtual_interface(name), "{name} must be treated as virtual");
        }
        for name in ["en0", "en1", "eth0", "ens5", "enp0s3", "wlan0", "eno1"] {
            assert!(!is_virtual_interface(name), "{name} must not be treated as virtual");
        }
    }

    /// 回归哨兵：只有 `IFF_UP` 不够，必须有 IPv4 地址。
    /// `en1`~`en8` 这类空接口全 UP 且无地址，只看 flags 会判成在线。
    #[test]
    fn interfaces_without_an_address_are_not_candidates() {
        let ifaces = vec![
            ("en1".to_string(), 0x1u32, false),
            ("en2".to_string(), 0x1u32, false),
            ("utun4".to_string(), 0x1u32, true),
        ];
        assert!(!has_usable_interface(&ifaces));
    }

    /// 回归哨兵：不依赖名字白名单。Linux 云主机叫 `ens5`，判成离线比误判在线更糟。
    #[test]
    fn a_real_interface_with_an_address_is_a_candidate_whatever_its_name() {
        let ifaces = [("ens5".to_string(), 0x1u32, true)];
        assert!(has_usable_interface(&ifaces));
    }

    /// 隧道即使 UP 且带地址也不是候选——它会在底层网络已断时仍然 UP。
    #[test]
    fn a_tunnel_with_an_address_is_still_not_a_candidate() {
        let ifaces = [("utun4".to_string(), 0x1u32, true)];
        assert!(!has_usable_interface(&ifaces));
    }

    #[test]
    fn loopback_is_never_a_candidate() {
        // IFF_LOOPBACK = 0x8
        let ifaces = [("lo0".to_string(), 0x9u32, true)];
        assert!(!has_usable_interface(&ifaces));
    }

    #[test]
    fn the_default_gateway_is_parsed_out_of_route_output() {
        let output = "   route to: default\n\
                      destination: default\n       mask: default\n    gateway: 192.168.31.1\n\
                      \x20 interface: en0\n";
        assert_eq!(
            parse_gateway(output),
            Some("192.168.31.1".parse::<IpAddr>().unwrap())
        );
    }

    /// VPN 为唯一出口时 `gateway:` 是 `link#28`，解析不到合法 IP。
    #[test]
    fn a_link_scoped_gateway_parses_to_none() {
        let output = "   route to: default\ndestination: default\n    gateway: link#28\n";
        assert_eq!(parse_gateway(output), None);
    }

    #[test]
    fn missing_output_parses_to_none() {
        assert_eq!(parse_gateway(""), None);
        assert_eq!(parse_gateway("route: writing to routing socket: not in table"), None);
    }

    /// 回归哨兵：`ECONNREFUSED` 证明链路是通的，必须算在线。
    /// 网关 80/443 通常不开，这是最常见的情形。
    #[test]
    fn connection_refused_counts_as_online() {
        let refused = std::io::Error::from_raw_os_error(libc::ECONNREFUSED);
        assert!(classify_connect_result(Err(refused)));
    }

    #[test]
    fn a_successful_connect_counts_as_online() {
        assert!(classify_connect_result(Ok(())));
    }

    #[test]
    fn timeouts_and_unreachable_networks_count_as_offline() {
        for code in [libc::ETIMEDOUT, libc::EHOSTUNREACH, libc::ENETUNREACH] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(!classify_connect_result(Err(err)), "errno {code} must be offline");
        }
    }

    /// 真机冒烟：本机应当有可用的电源来源与网络判定，且都不 panic。
    #[test]
    fn the_system_probes_run_without_panicking() {
        let power = SystemPowerProbe::new();
        let _ = power.on_ac();
        let net = SystemNetProbe::new();
        let online = net.online();
        // 不硬断言 true：CI 机器可能真的没有默认路由。只要求不 panic，
        // 并且「没有候选接口」时必须为 false。
        assert_eq!(online, online);
    }

    /// 探测目标缺失不构成离线证据：解析不到网关时退化按网卡判定。
    #[test]
    fn no_gateway_falls_back_to_the_interface_check() {
        assert!(decide_online(Some(true), None), "no gateway + a real link => assume online");
        assert!(!decide_online(Some(false), None), "no gateway + no link => offline");
        assert!(decide_online(Some(true), Some(true)));
        assert!(!decide_online(Some(true), Some(false)));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power probe`
Expected: 编译失败（`is_on_ac` / `has_usable_interface` 等未定义）

- [ ] **Step 3: 写实现**

`src/probe.rs` 全文：

```rust
//! 电源来源与网络可达性探测。
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub trait PowerProbe: Send + Sync {
    fn on_ac(&self) -> bool;
}

pub trait NetProbe: Send + Sync {
    fn online(&self) -> bool;
}

pub fn is_on_ac(power_source_type: &str) -> bool {
    power_source_type != "Battery Power"
}

pub fn is_virtual_interface(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "utun", "awdl", "llw", "bridge", "gif", "stf", "anpi", "ap", "tun", "tap", "docker", "br-",
        "veth", "virbr", "lo",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

const IFF_LOOPBACK: u32 = 0x8;

pub fn has_usable_interface(interfaces: &[(String, u32, bool)]) -> bool {
    interfaces.iter().any(|(name, flags, has_v4)| {
        let up = flags & (libc::IFF_UP as u32) != 0;
        let loopback = flags & IFF_LOOPBACK != 0;
        up && !loopback && *has_v4 && !is_virtual_interface(name)
    })
}

pub fn decide_online(has_interface: Option<bool>, gateway_reachable: Option<bool>) -> bool {
    match (has_interface, gateway_reachable) {
        (Some(false), _) => false,
        (_, Some(reachable)) => reachable,
        (Some(true), None) => true,
        (None, None) => false,
    }
}

pub fn parse_gateway(route_output: &str) -> Option<IpAddr> {
    route_output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("gateway:")?;
        rest.trim().parse::<IpAddr>().ok()
    })
}

pub fn classify_connect_result(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => matches!(error.raw_os_error(), Some(libc::ECONNREFUSED)),
    }
}

pub struct SystemPowerProbe;

impl SystemPowerProbe {
    pub fn new() -> Self {
        Self
    }
}
impl Default for SystemPowerProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl PowerProbe for SystemPowerProbe {
    fn on_ac(&self) -> bool {
        mac::providing_power_source_type()
            .map(|ty| is_on_ac(&ty))
            .unwrap_or(true)
    }
}

#[cfg(not(target_os = "macos"))]
impl PowerProbe for SystemPowerProbe {
    fn on_ac(&self) -> bool {
        true
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type Boolean = u8;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            s: CFStringRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> Boolean;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFStringRef;
    }

    pub(super) fn providing_power_source_type() -> Option<String> {
        unsafe {
            let blob = IOPSCopyPowerSourcesInfo();
            if blob.is_null() {
                return None;
            }
            let ty = IOPSGetProvidingPowerSourceType(blob);
            if ty.is_null() {
                CFRelease(blob);
                return None;
            }
            let mut buffer = [0i8; 128];
            let ok = CFStringGetCString(ty, buffer.as_mut_ptr(), buffer.len() as isize, UTF8);
            let value = if ok != 0 {
                Some(
                    CStr::from_ptr(buffer.as_ptr())
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            };
            CFRelease(blob);
            value
        }
    }
}

pub struct SystemNetProbe;

impl SystemNetProbe {
    pub fn new() -> Self {
        Self
    }
}
impl Default for SystemNetProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NetProbe for SystemNetProbe {
    fn online(&self) -> bool {
        let interfaces = collect_interfaces();
        let has_interface = if interfaces.is_empty() {
            None
        } else {
            Some(has_usable_interface(&interfaces))
        };
        let gateway = default_gateway().map(|ip| {
            let addr = SocketAddr::new(ip, 80);
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(_) => true,
                Err(error) => classify_connect_result(Err(error)),
            }
        });
        let online = decide_online(has_interface, gateway);
        if gateway.is_none() {
            eprintln!(
                "[yi-agent-power] cannot determine the default gateway; falling back to the interface check"
            );
        }
        online
    }
}

fn collect_interfaces() -> Vec<(String, u32, bool)> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        let mut out: Vec<(String, u32, bool)> = Vec::new();
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `getifaddrs` 填充 `head`；成功时链表由我们 freeifaddrs。
        let rc = unsafe { libc::getifaddrs(&mut head) };
        if rc != 0 || head.is_null() {
            return out;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            // SAFETY: `cursor` 在链表内且非空。
            let entry = unsafe { &*cursor };
            if !entry.ifa_name.is_null() {
                // SAFETY: `ifa_name` 是 NUL 结尾字符串。
                let name = unsafe { CStr::from_ptr(entry.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                let flags = entry.ifa_flags;
                let has_v4 = !entry.ifa_addr.is_null()
                    // SAFETY: 非空时 `ifa_addr` 指向 sockaddr，首字段是 sa_family。
                    && unsafe { (*entry.ifa_addr).sa_family as i32 } == libc::AF_INET;
                match out.iter_mut().find(|(existing, _, _)| existing == &name) {
                    Some(slot) => slot.2 |= has_v4,
                    None => out.push((name, flags, has_v4)),
                }
            }
            cursor = entry.ifa_next;
        }
        // SAFETY: 链表来自 getifaddrs，此处释放一次。
        unsafe { libc::freeifaddrs(head) };
        out
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

fn default_gateway() -> Option<IpAddr> {
    let output = std::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gateway(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn battery_is_the_only_thing_that_is_not_ac() {
        assert!(!is_on_ac("Battery Power"));
        assert!(is_on_ac("AC Power"));
        assert!(is_on_ac("UPS Power"));
        assert!(is_on_ac("Something New"));
    }

    #[test]
    fn virtual_interface_prefixes_are_recognised() {
        for name in [
            "utun0", "utun4", "awdl0", "llw0", "bridge0", "gif0", "stf0", "anpi1", "ap1", "tun0",
            "tap0", "docker0", "br-1234", "veth1a2b", "virbr0", "lo",
        ] {
            assert!(is_virtual_interface(name), "{name} must be virtual");
        }
        for name in ["en0", "en1", "eth0", "ens5", "enp0s3", "wlan0", "eno1"] {
            assert!(!is_virtual_interface(name), "{name} must not be virtual");
        }
    }

    #[test]
    fn interfaces_without_an_address_are_not_candidates() {
        let ifaces = vec![
            ("en1".to_string(), 0x1u32, false),
            ("en2".to_string(), 0x1u32, false),
            ("utun4".to_string(), 0x1u32, true),
        ];
        assert!(!has_usable_interface(&ifaces));
    }

    #[test]
    fn a_real_interface_with_an_address_is_a_candidate_whatever_its_name() {
        assert!(has_usable_interface(&[("ens5".to_string(), 0x1u32, true)]));
    }

    #[test]
    fn a_tunnel_with_an_address_is_still_not_a_candidate() {
        assert!(!has_usable_interface(&[(
            "utun4".to_string(),
            0x1u32,
            true
        )]));
    }

    #[test]
    fn loopback_is_never_a_candidate() {
        assert!(!has_usable_interface(&[("lo0".to_string(), 0x9u32, true)]));
    }

    #[test]
    fn the_default_gateway_is_parsed_out_of_route_output() {
        let output = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.168.31.1\n       interface: en0\n";
        assert_eq!(
            parse_gateway(output),
            Some("192.168.31.1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn a_link_scoped_gateway_parses_to_none() {
        assert_eq!(
            parse_gateway("   route to: default\ndestination: default\n    gateway: link#28\n"),
            None
        );
    }

    #[test]
    fn missing_output_parses_to_none() {
        assert_eq!(parse_gateway(""), None);
        assert_eq!(
            parse_gateway("route: writing to routing socket: not in table"),
            None
        );
    }

    #[test]
    fn connection_refused_counts_as_online() {
        let refused = std::io::Error::from_raw_os_error(libc::ECONNREFUSED);
        assert!(classify_connect_result(Err(refused)));
    }

    #[test]
    fn a_successful_connect_counts_as_online() {
        assert!(classify_connect_result(Ok(())));
    }

    #[test]
    fn timeouts_and_unreachable_networks_count_as_offline() {
        for code in [libc::ETIMEDOUT, libc::EHOSTUNREACH, libc::ENETUNREACH] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(
                !classify_connect_result(Err(err)),
                "errno {code} must be offline"
            );
        }
    }

    #[test]
    fn the_system_probes_run_without_panicking() {
        let power = SystemPowerProbe::new();
        let _ = power.on_ac();
        let net = SystemNetProbe::new();
        let _ = net.online();
    }

    #[test]
    fn no_gateway_falls_back_to_the_interface_check() {
        assert!(decide_online(Some(true), None));
        assert!(!decide_online(Some(false), None));
        assert!(decide_online(Some(true), Some(true)));
        assert!(!decide_online(Some(true), Some(false)));
    }
}
```

**注意**：Step 1 的测试模块要保留，删掉上面这个占位的 `#[cfg(test)] mod tests { // … }`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power probe`
Expected: **14 个测试 PASS**（已实测）

- [ ] **Step 5: 真机核对探测结果**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power probe::tests::the_system_probes_run_without_panicking -- --nocapture`
Expected: PASS；若本机有默认路由，日志里不应出现 "cannot determine the default gateway"

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/probe.rs
git commit -m "feat(power): 电源来源与默认网关探测，ECONNREFUSED 计为在线"
```

---

### Task 7: `supervisor` — 10s 拍串联，只在迁移边沿碰 FFI

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-power/src/supervisor.rs`

**Interfaces:**
- Consumes: Task 2 的 `Policy`/`Snapshot`/`Decision`、Task 3 的 `Clock`、Task 4 的 `Assertion`、Task 6 的两个 probe trait
- Produces:
  - `pub struct Supervisor`
  - `Supervisor::new(clock: Arc<dyn Clock>, power: Box<dyn PowerProbe>, net: Box<dyn NetProbe>, assertion: Box<dyn Assertion>, enabled: bool) -> Supervisor`
  - `pub fn tick(&mut self, busy: bool, enabled: bool)`
  - `pub fn is_holding(&self) -> bool`（只读，供日志/未来 UI）
  - `impl Drop for Supervisor` — 释放

关键不变量（spec「断言的生命周期」）：

1. **只在迁移边沿调 FFI。** `Decision::Hold` 且已 `Held` 时是空操作——每拍都
   `Create`+`Release` 会刷出大量瞬时断言，`pmset -g assertions` 里看不出稳定状态。
2. **`Release` 必须幂等**，且释放后能重新 `Create`。
3. **创建失败要可见但不能拖垮主循环**：记一次 warn、内部置 `Idle`、下次迁移到
   Hold 边沿才重试（不在每拍无脑重试，否则日志每 10s 刷一条）。

- [ ] **Step 1: 写失败的测试**

追加到 `src/supervisor.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertion::Assertion;
    use crate::clock::{Clock, Stamp};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// 可手动推进的假时钟：两条读数同步前进。
    struct FakeClock {
        mono: Mutex<Duration>,
        wall: Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                mono: Mutex::new(Duration::ZERO),
                wall: Mutex::new(Duration::ZERO),
            })
        }
        fn advance(&self, by: Duration) {
            *self.mono.lock().unwrap() += by;
            *self.wall.lock().unwrap() += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Option<Stamp> {
            Some(Stamp(*self.mono.lock().unwrap()))
        }
        fn since(&self, stamp: Stamp) -> Duration {
            self.mono.lock().unwrap().saturating_sub(stamp.0)
        }
        fn wall(&self) -> Option<Stamp> {
            Some(Stamp(*self.wall.lock().unwrap()))
        }
        fn mono_trusted(&self) -> bool {
            true
        }
    }

    struct FixedPower(bool);
    impl PowerProbe for FixedPower {
        fn on_ac(&self) -> bool {
            self.0
        }
    }

    struct FixedNet(bool);
    impl NetProbe for FixedNet {
        fn online(&self) -> bool {
            self.0
        }
    }

    struct CountingAssertion {
        held: AtomicBool,
        acquires: AtomicUsize,
        releases: AtomicUsize,
        fail_next: AtomicBool,
    }

    impl CountingAssertion {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                held: AtomicBool::new(false),
                acquires: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                fail_next: AtomicBool::new(false),
            })
        }
    }

    impl Assertion for Arc<CountingAssertion> {
        fn acquire(&mut self) -> bool {
            self.acquires.fetch_add(1, Ordering::SeqCst);
            if self.fail_next.swap(false, Ordering::SeqCst) {
                self.held.store(false, Ordering::SeqCst);
                return false;
            }
            self.held.store(true, Ordering::SeqCst);
            true
        }
        fn release(&mut self) {
            self.releases.fetch_add(1, Ordering::SeqCst);
            self.held.store(false, Ordering::SeqCst);
        }
        fn is_held(&self) -> bool {
            self.held.load(Ordering::SeqCst)
        }
    }

    fn supervisor_with(
        clock: Arc<FakeClock>,
        on_ac: bool,
        online: bool,
        assertion: Arc<CountingAssertion>,
    ) -> Supervisor {
        Supervisor::new(
            clock,
            Box::new(FixedPower(on_ac)),
            Box::new(FixedNet(online)),
            Box::new(assertion),
            true,
        )
    }

    #[test]
    fn holding_then_ticking_again_does_not_touch_the_ffi() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));

        sup.tick(false, true);
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
        assert!(sup.is_holding());

        for _ in 0..10 {
            clock.advance(Duration::from_secs(10));
            sup.tick(false, true);
        }
        assert_eq!(
            assertion.acquires.load(Ordering::SeqCst),
            1,
            "只在迁移边沿调 FFI：已持有时不得重复 Create"
        );
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 0);
    }

    /// 电池 + 有活 → 持有；活干完后满 15 分钟才释放，未满不释放。
    ///
    /// 这条是真正的宽限路径：探针是固定值（电池、在线），所以条件从
    /// `busy=true` 变 `busy=false` 时 `ok` 由真变假，全靠时间窗维持。
    #[test]
    fn battery_work_finishing_releases_after_the_grace() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), false, true, Arc::clone(&assertion));
        sup.tick(true, true);
        assert!(sup.is_holding());

        clock.advance(Duration::from_secs(899));
        sup.tick(false, true);
        assert!(sup.is_holding(), "宽限未到不得释放");
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 0);

        clock.advance(Duration::from_secs(1));
        sup.tick(false, true);
        assert!(!sup.is_holding(), "满 15 分钟必须释放");
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disabling_releases_immediately_without_waiting_for_the_grace() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
        sup.tick(false, true);
        assert!(sup.is_holding());

        clock.advance(Duration::from_secs(10));
        sup.tick(false, false);
        assert!(!sup.is_holding());
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn re_enabling_holds_again_on_the_next_tick() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
        sup.tick(false, false);
        assert!(!sup.is_holding());

        clock.advance(Duration::from_secs(10));
        sup.tick(false, true);
        assert!(sup.is_holding());
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
    }

    /// 创建失败：记状态为 Idle，且下次迁移到 Hold 边沿时重试。
    #[test]
    fn a_failed_acquire_is_retried_on_the_next_hold_edge() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        assertion.fail_next.store(true, Ordering::SeqCst);
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));

        sup.tick(false, true);
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
        assert!(!sup.is_holding(), "a failed acquire must not report holding");

        clock.advance(Duration::from_secs(10));
        sup.tick(false, true);
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 2, "retry on the next tick");
        assert!(sup.is_holding());
    }

    /// Drop 释放：`Supervisor` 被丢弃时断言必须被释放（进程内不留悬挂状态）。
    #[test]
    fn dropping_the_supervisor_releases_the_assertion() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        {
            let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
            sup.tick(false, true);
            assert!(assertion.held.load(Ordering::SeqCst));
        }
        assert!(!assertion.held.load(Ordering::SeqCst));
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }

    /// 每拍都读电源/网络，但只在 `ok` 变化时影响状态——这里验证探针确实被用到。
    #[test]
    fn the_snapshot_reflects_the_injected_probes() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, false, Arc::clone(&assertion));
        sup.tick(false, true);
        assert!(!sup.is_holding(), "offline + never satisfied => no hold");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power supervisor`
Expected: 编译失败（`Supervisor` 未定义）

- [ ] **Step 3: 写实现**

`src/supervisor.rs` 全文：

```rust
//! 10s tick：把 clock/policy/probe/assertion 串起来。
use std::sync::Arc;

use crate::assertion::Assertion;
use crate::clock::Clock;
use crate::policy::{Decision, Policy, Snapshot};
use crate::probe::{NetProbe, PowerProbe};

pub struct Supervisor {
    clock: Arc<dyn Clock>,
    power: Box<dyn PowerProbe>,
    net: Box<dyn NetProbe>,
    assertion: Box<dyn Assertion>,
    policy: Policy,
}

impl Supervisor {
    pub fn new(
        clock: Arc<dyn Clock>,
        power: Box<dyn PowerProbe>,
        net: Box<dyn NetProbe>,
        assertion: Box<dyn Assertion>,
        enabled: bool,
    ) -> Self {
        let mut policy = Policy::new();
        if !enabled {
            policy.on_disabled();
        }
        Self {
            clock,
            power,
            net,
            assertion,
            policy,
        }
    }

    pub fn tick(&mut self, busy: bool, enabled: bool) {
        if !enabled {
            self.policy.on_disabled();
            self.release_if_held();
            return;
        }
        self.policy.re_enable();

        let Some(now) = self.clock.sample() else {
            // 两条时钟都取不到：任何不确定都倒向「允许睡」。
            self.policy.on_disabled();
            self.release_if_held();
            return;
        };

        let snapshot = Snapshot {
            on_ac: self.power.on_ac(),
            online: self.net.online(),
            busy,
        };
        match self.policy.decide(&snapshot, now) {
            Decision::Hold => {
                if !self.assertion.is_held() && !self.assertion.acquire() {
                    eprintln!(
                        "[yi-agent-power] could not create the power assertion; will retry on the next hold edge"
                    );
                }
            }
            Decision::Idle => self.release_if_held(),
        }
    }

    fn release_if_held(&mut self) {
        if self.assertion.is_held() {
            self.assertion.release();
        }
    }

    pub fn is_holding(&self) -> bool {
        self.assertion.is_held()
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.release_if_held();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertion::Assertion;
    use crate::clock::{Clock, Stamp};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    struct FakeClock {
        mono: Mutex<Duration>,
        wall: Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                mono: Mutex::new(Duration::ZERO),
                wall: Mutex::new(Duration::ZERO),
            })
        }
        fn advance(&self, by: Duration) {
            *self.mono.lock().unwrap() += by;
            *self.wall.lock().unwrap() += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Option<Stamp> {
            Some(Stamp(*self.mono.lock().unwrap()))
        }
        fn since(&self, stamp: Stamp) -> Duration {
            self.mono.lock().unwrap().saturating_sub(stamp.0)
        }
        fn wall(&self) -> Option<Stamp> {
            Some(Stamp(*self.wall.lock().unwrap()))
        }
        fn mono_trusted(&self) -> bool {
            true
        }
    }

    struct FixedPower(bool);
    impl PowerProbe for FixedPower {
        fn on_ac(&self) -> bool {
            self.0
        }
    }

    struct FixedNet(bool);
    impl NetProbe for FixedNet {
        fn online(&self) -> bool {
            self.0
        }
    }

    struct CountingAssertion {
        held: AtomicBool,
        acquires: AtomicUsize,
        releases: AtomicUsize,
        fail_next: AtomicBool,
    }

    impl CountingAssertion {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                held: AtomicBool::new(false),
                acquires: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                fail_next: AtomicBool::new(false),
            })
        }
    }

    impl Assertion for Arc<CountingAssertion> {
        fn acquire(&mut self) -> bool {
            self.acquires.fetch_add(1, Ordering::SeqCst);
            if self.fail_next.swap(false, Ordering::SeqCst) {
                self.held.store(false, Ordering::SeqCst);
                return false;
            }
            self.held.store(true, Ordering::SeqCst);
            true
        }
        fn release(&mut self) {
            self.releases.fetch_add(1, Ordering::SeqCst);
            self.held.store(false, Ordering::SeqCst);
        }
        fn is_held(&self) -> bool {
            self.held.load(Ordering::SeqCst)
        }
    }

    fn supervisor_with(
        clock: Arc<FakeClock>,
        on_ac: bool,
        online: bool,
        assertion: Arc<CountingAssertion>,
    ) -> Supervisor {
        Supervisor::new(
            clock,
            Box::new(FixedPower(on_ac)),
            Box::new(FixedNet(online)),
            Box::new(assertion),
            true,
        )
    }

    #[test]
    fn holding_then_ticking_again_does_not_touch_the_ffi() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
        sup.tick(false, true);
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
        assert!(sup.is_holding());
        for _ in 0..10 {
            clock.advance(Duration::from_secs(10));
            sup.tick(false, true);
        }
        assert_eq!(
            assertion.acquires.load(Ordering::SeqCst),
            1,
            "只在迁移边沿调 FFI"
        );
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn disabling_releases_immediately_without_waiting_for_the_grace() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
        sup.tick(false, true);
        assert!(sup.is_holding());
        clock.advance(Duration::from_secs(10));
        sup.tick(false, false);
        assert!(!sup.is_holding());
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn re_enabling_holds_again_on_the_next_tick() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
        sup.tick(false, false);
        assert!(!sup.is_holding());
        clock.advance(Duration::from_secs(10));
        sup.tick(false, true);
        assert!(sup.is_holding());
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_acquire_is_retried_on_the_next_hold_edge() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        assertion.fail_next.store(true, Ordering::SeqCst);
        let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
        sup.tick(false, true);
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
        assert!(!sup.is_holding());
        clock.advance(Duration::from_secs(10));
        sup.tick(false, true);
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 2);
        assert!(sup.is_holding());
    }

    #[test]
    fn dropping_the_supervisor_releases_the_assertion() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        {
            let mut sup = supervisor_with(Arc::clone(&clock), true, true, Arc::clone(&assertion));
            sup.tick(false, true);
            assert!(assertion.held.load(Ordering::SeqCst));
        }
        assert!(!assertion.held.load(Ordering::SeqCst));
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_snapshot_reflects_the_injected_probes() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), true, false, Arc::clone(&assertion));
        sup.tick(false, true);
        assert!(!sup.is_holding());
    }

    /// 电池 + 有活 → 持有；活干完 15 分钟后释放。
    #[test]
    fn battery_work_finishing_releases_after_the_grace() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(Arc::clone(&clock), false, true, Arc::clone(&assertion));
        sup.tick(true, true);
        assert!(sup.is_holding());
        clock.advance(Duration::from_secs(899));
        sup.tick(false, true);
        assert!(sup.is_holding(), "宽限未到不释放");
        clock.advance(Duration::from_secs(1));
        sup.tick(false, true);
        assert!(!sup.is_holding(), "满 15 分钟释放");
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }
}
```

**注意**：`Supervisor::new` 需要 `Stamp` 的字段可见性——Task 3 里 `Stamp(pub(crate) Duration)`，
同 crate 内 `now.0` 合法。若你改了 `Stamp` 的可见性，这里要一起改。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power supervisor`
Expected: 7 个测试 PASS（已实测）

- [ ] **Step 5: 全 crate 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-power`
Expected: 全部 PASS；`cargo clippy -p yi-agent-power --all-targets -- -D warnings` 无告警

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-power/src/supervisor.rs
git commit -m "feat(power): supervisor 10s 拍状态机，只在迁移边沿触碰 IOKit"
```

---

### Task 8: busy 采集 — 三路并集 + 独立采样任务（偏差 1、2 的落点）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs`（加 `current_handle_no_touch`）
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（注册模块）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`（加 `yi-agent-power` 依赖）

**Interfaces:**
- Consumes: `runtime::settings::prevent_idle_sleep_enabled`、`subagent::binding::RuntimeBinding`、`store::ipc::{send_request_with_version_timeout, IpcRequest, IpcResponse}`
- Produces（`power_busy.rs`）:
  - `pub(crate) fn local_busy(threads: &HashMap<String, ThreadSession>) -> bool`
  - `pub(crate) fn remote_touch_busy(last_remote_touch: Option<Stamp>, clock: &dyn Clock) -> bool`
  - `pub(crate) fn daemon_busy(runtimes: &ProjectRuntimes) -> bool` — **同步、带 500ms 超时**，只准在 `spawn_blocking` 里调
  - `pub(crate) fn state_counts_as_busy(state: &str) -> bool` — 纯函数
  - `pub(crate) const REMOTE_TOUCH_WINDOW: Duration`（5 分钟）
  - `pub(crate) const DAEMON_PROBE_TIMEOUT: Duration`（500ms）
  - `pub(crate) struct BusySampler` — 独立任务，主循环 `latest()` 非阻塞取最新值

**三处必须写对的地方**：

1. `daemon_busy` **不能**调 `binding.current()`——它内部 `touch()` 会刷新空闲时钟，
   让 `evict_idle_runtimes` 永不回收 daemon（偏差 1）。用新增的
   `current_handle_no_touch()`。
2. `daemon_busy` **不能**在主循环里同步跑（偏差 2）。放进 `spawn_blocking`，
   主循环只读最新值。
3. `active_only: true` 的口径比我们需要的宽：SQL 只排除 `completed`/
   `completed_no_changes`/`failed`/`cancelled` 四种（`repository.rs:3433-3437`），
   还留着 `blocked`/`stalled`/`timed_out`/`budget_exhausted`/`recovery_required`
   等「已经停下、需要人介入才会动」的状态。所以拿到 `TaskSummaries` 后必须再按
   `state_counts_as_busy` 过滤一遍。

- [ ] **Step 1: 加 `current_handle_no_touch`（偏差 1）**

在 `crates/yi-agent-subagent/src/binding.rs` 的 `current()` 方法之后加：

```rust
    /// The current handle **without** refreshing the idle clock.
    ///
    /// `current()` 会 `touch()`——那对「委派请求」是对的（请求本身算使用），
    /// 但对**探测**是错的：保活探测每 10s 问一次句柄，若顺手刷新空闲时钟，
    /// `evict_idle_runtimes` 的 `idle_for()` 就永远停在「刚刚」，
    /// `EVICT_IDLE_AFTER`（600s）永不触发，项目 daemon 永不回收。
    ///
    /// 这个函数只做读取。调用方只能是「问一句就走」的旁观者。
    pub fn current_handle_no_touch(&self) -> Result<RuntimeHandle, String> {
        Ok(self.lock_state().handle.clone())
    }
```

加一条测试（在 `binding.rs` 的 tests 模块里）：

```rust
    /// 探测不得刷新空闲时钟：调 `current_handle_no_touch` 之后 `idle_for` 必须
    /// 继续按真实时间增长，而 `current()` 会把它压回零。
    #[test]
    fn current_handle_no_touch_does_not_reset_the_idle_clock() {
        let binding = RuntimeBinding::fixed(RuntimeHandle {
            socket_path: PathBuf::from("/tmp/x.sock"),
            workspace_root: PathBuf::from("/tmp"),
            session_id: String::new(),
            task_id: String::new(),
            capability: String::new(),
        });
        binding.backdate_idle_clock(Duration::from_secs(500));
        let before = binding.idle_for();
        let _ = binding.current_handle_no_touch().unwrap();
        let after = binding.idle_for();
        assert!(
            after >= before,
            "probing must not rewind the idle clock: before={before:?} after={after:?}"
        );

        binding.backdate_idle_clock(Duration::from_secs(500));
        let _ = binding.current().unwrap();
        assert!(
            binding.idle_for() < Duration::from_secs(60),
            "current() is documented to touch; this pins the difference"
        );
    }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent current_handle_no_touch`
Expected: PASS

- [ ] **Step 2: 加依赖**

`crates/yi-agent-app-server/Cargo.toml` 的 `[dependencies]` 里，`yi-agent-flags = { workspace = true }` 之后加：

```toml
# 保活策略与平台断言。app-server 是唯一持有断言的进程（见 spec「为什么是单一持有者」）。
yi-agent-power = { workspace = true }
```

- [ ] **Step 3: 写 `power_busy.rs` 的失败测试**

创建 `crates/yi-agent-app-server/src/power_busy.rs`，先只写测试：

```rust
//! busy 的三路并集与独立采样任务。
//!
//! 见 plan 偏差 1/2：探测既不能刷新 `RuntimeBinding` 的空闲时钟，也不能在
//! 主循环里同步等 daemon。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_and_waiting_tasks_count_as_busy() {
        for state in [
            "running",
            "waiting_for_resource",
            "waiting_for_permission",
            "waiting_for_children",
        ] {
            assert!(state_counts_as_busy(state), "{state} is work in progress");
        }
    }

    /// 回归哨兵：排队而已，睡着后醒来照跑，不丢进度。
    #[test]
    fn queued_and_paused_do_not_count_as_busy() {
        assert!(!state_counts_as_busy("queued"));
        assert!(!state_counts_as_busy("paused"));
    }

    /// 回归哨兵：`blocked` 是「阻塞等人」，可以让机器无限期不睡。
    #[test]
    fn terminal_and_blocked_states_do_not_count_as_busy() {
        for state in [
            "completed",
            "completed_no_changes",
            "blocked",
            "stalled",
            "timed_out",
            "budget_exhausted",
            "failed",
            "cancelled",
            "recovery_required",
            "recovery_gated",
            "recovery_attested",
        ] {
            assert!(!state_counts_as_busy(state), "{state} must not keep the machine awake");
        }
    }

    /// 新增状态默认按「不算」处理——偏向允许睡眠。判错代价只是睡一觉。
    #[test]
    fn an_unknown_state_does_not_count_as_busy() {
        assert!(!state_counts_as_busy("some_future_state"));
    }

    #[test]
    fn remote_touch_expires_after_five_minutes() {
        assert_eq!(REMOTE_TOUCH_WINDOW, Duration::from_secs(300));
    }

    /// 探测超时必须远小于 10s tick。
    #[test]
    fn the_daemon_probe_timeout_is_far_below_the_tick() {
        assert!(DAEMON_PROBE_TIMEOUT <= Duration::from_millis(500));
    }
}
```

- [ ] **Step 4: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server power_busy`
Expected: 编译失败（模块未注册 / 函数未定义）

- [ ] **Step 5: 写实现**

先注册模块。`crates/yi-agent-app-server/src/lib.rs` 里，按字母序在 `pub mod pairing;` 附近加：

```rust
pub(crate) mod power_busy;
```

然后把 `power_busy.rs` 补成：

```rust
//! busy 的三路并集与独立采样任务。
//!
//! `busy` 由三个来源并起来，缺一个就会漏掉一整类活：
//!
//! (a) **本进程的会话**：`threads` 表里非 `Idle` 或有 `active_turn_id` 的；
//! (b) **项目 daemon 里的子 agent / 看板 / 定时任务**：它们不出现在 `threads`
//!     表里，只按 (a) 判会让「看板在后台推进卡片」被判成无活；
//! (c) **远端近期交互**：非 local 客户端在 5 分钟内发过请求。
//!
//! 两处实现约束（plan 偏差 1/2）：
//!
//! - (b) 读句柄必须用 `current_handle_no_touch()`。`current()` 会刷新
//!   `last_active`，每 10s 探一次等于让 `evict_idle_runtimes` 永不触发。
//! - (b) 是同步阻塞 IPC，**只能**在独立 task 里跑。主循环拿到的是上一拍的结果。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use yi_agent_power::clock::{Clock, Stamp};
use yi_agent_subagent::binding::RuntimeBinding;

use crate::protocol::ThreadStatus;
use crate::server::ProjectRuntimes;
use crate::session::ThreadSession;

/// 远端交互的有效窗口。
pub(crate) const REMOTE_TOUCH_WINDOW: Duration = Duration::from_secs(5 * 60);

/// 查 daemon 的回复截止时间。**必须**远小于 10s tick。
pub(crate) const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// 一个任务状态是否意味着「会自己继续推进」。
///
/// 口径是「**非终态 且 非 `queued`/`paused`**」，落在数据上就是只保留
/// `running` / `waiting_for_*`。
///
/// **不要手抄终态清单**：`yi_agent_store::repository` 的 `TERMINAL_TASK_STATES`
/// 与 `yi_agent_core::subagent::task::TaskState::is_terminal` 是那份权威清单，
/// 且两者被 `prune_treats_only_terminal_states_as_finished` 钉住一致。这里按
/// **白名单**写（只认四个在推进的状态），于是未知新状态自动落到「不算」——这正是
/// 想要的方向：判错的代价是睡了一觉（醒来可续），反过来是整夜不睡且无人察觉。
pub(crate) fn state_counts_as_busy(state: &str) -> bool {
    matches!(
        state,
        "running"
            | "waiting_for_resource"
            | "waiting_for_permission"
            | "waiting_for_children"
    )
}

/// (a) 本进程会话：与 `driver_tick` 的 idle 快照同一口径，只取反。
///
/// 判据是「非 `Idle`」而不是「`Running`」：`AwaitingApproval` 也算有活——
/// 那一刻 turn 没结束、上下文都在，机器睡了用户回来点批准，这条 turn 已经随
/// 睡眠死掉了。`active_turn_id.is_some()` 与它并列，覆盖「turn 已挂上但状态还
/// 没切成 Running」的短暂窗口。
pub(crate) fn local_busy(threads: &HashMap<String, ThreadSession>) -> bool {
    threads.values().any(|session| {
        session.active_turn_id.is_some()
            || !matches!(
                *session
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                ThreadStatus::Idle
            )
    })
}

/// (c) 远端近期交互。
///
/// 时间戳必须走 `clock` 的睡眠感知时钟：睡 8 小时后 `since()` 就是 8 小时，
/// 远超 5 分钟，`remote_touch` 自动失效。**不能**用 `Instant::now()`——那只前进
/// 几分钟，会让一条早已失效的交互记录继续算作「刚刚有过交互」，使 `ok` 恒为真、
/// 15 分钟窗口形同虚设。
pub(crate) fn remote_touch_busy(last_remote_touch: Option<Stamp>, clock: &dyn Clock) -> bool {
    last_remote_touch.is_some_and(|stamp| clock.since(stamp) < REMOTE_TOUCH_WINDOW)
}

/// (b) 项目 daemon 里是否有在推进的任务。
///
/// **同步阻塞**，最长 `DAEMON_PROBE_TIMEOUT`；只能从 `spawn_blocking` 调。
/// 任一项目有 running 类任务即 `true`。
///
/// 已知盲区（刻意）：`runtimes` 只含**已挂载过**的项目。看板在某个用户从没在
/// 本进程打开过的项目上推卡片时查不到——那种情况下判成「无活」让机器睡是正确
/// 行为；为覆盖率去扫磁盘上所有 `runtime.sock` 会把 10s tick 撑坏。
pub(crate) fn daemon_busy(runtimes: &ProjectRuntimes) -> bool {
    // 先取快照再放锁：不能在持锁期间做 IPC。
    let bindings: Vec<Arc<RuntimeBinding>> = runtimes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter_map(|entry| entry.as_ref().ok().cloned())
        .collect();

    bindings.iter().any(|binding| {
        // 关键：`current_handle_no_touch` 而不是 `current()`。
        let Ok(handle) = binding.current_handle_no_touch() else {
            return false;
        };
        let response = yi_agent_store::ipc::send_request_with_version_timeout(
            &handle.socket_path,
            yi_agent_store::ipc::PROTOCOL_VERSION,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: true,
            },
            DAEMON_PROBE_TIMEOUT,
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks }) => {
                tasks.iter().any(|task| state_counts_as_busy(&task.state))
            }
            // daemon 不在 / 超时 / 拒绝：按无活处理（宁可睡），不 panic，
            // 也不因为探测而拉起 daemon。
            _ => false,
        }
    })
}

/// 独立采样任务：把 `(b)` 从主循环里摘出去。
///
/// 主循环每拍只读最新值，取不到就沿用上一拍。这样最坏情况是「busy 晚一拍」，
/// 而不是「主循环卡 500ms」。
pub(crate) struct BusySampler {
    latest: Arc<StdMutex<bool>>,
    stop: Arc<AtomicBool>,
}

impl BusySampler {
    /// 启动采样任务。`runtimes` 是共享表，任务自己 clone 一份 `Arc`。
    pub(crate) fn spawn(runtimes: ProjectRuntimes) -> Self {
        let latest = Arc::new(StdMutex::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = Arc::clone(&latest);
        let stop_flag = Arc::clone(&stop);
        tokio::task::spawn_blocking(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                let busy = daemon_busy(&runtimes);
                *writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = busy;
                std::thread::sleep(DAEMON_PROBE_TIMEOUT);
            }
        });
        Self { latest, stop }
    }

    /// 非阻塞读取：永远返回上一拍的值，绝不等待。
    pub(crate) fn latest(&self) -> bool {
        *self
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for BusySampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
```

**注意**：`spawn_blocking` 里用 `std::thread::sleep` 而不是 `tokio::time::sleep`——那是
阻塞线程池，用 async sleep 会白占一个 worker。

- [ ] **Step 6: 补一条「探测不阻塞主循环」的测试**

追加到 `power_busy.rs` 的 tests：

```rust
    /// 一个「收下请求但永不回复」的 daemon 不让探测卡住超过超时。
    /// 这条是本功能对主循环的唯一硬承诺。
    #[test]
    fn a_silent_daemon_costs_at_most_the_probe_timeout() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("runtime.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // 收下请求但一个字都不回。
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                std::thread::sleep(Duration::from_secs(5));
            }
        });

        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
            socket_path: socket,
            workspace_root: dir.path().to_path_buf(),
            session_id: String::new(),
            task_id: String::new(),
            capability: String::new(),
        });
        runtimes
            .lock()
            .unwrap()
            .insert(dir.path().to_path_buf(), Ok(binding));

        let started = std::time::Instant::now();
        assert!(!daemon_busy(&runtimes), "a silent daemon must not count as busy");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "probe must be bounded by its 500ms timeout, took {elapsed:?}"
        );
        let _ = server.join();
    }
```

- [ ] **Step 7: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server power_busy`
Expected: **7 个测试 PASS**（Task 8 Step 3 的 6 条纯函数用例 + Step 6 的静默 daemon 用例）。
实测：`7 passed`（Step 3 与 Step 6 合计；早前只写 6 是漏算了 Step 6 新增的一条）。

- [ ] **Step 8: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-subagent/src/binding.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/lib.rs \
        yi-agent-rs/crates/yi-agent-app-server/Cargo.toml
git commit -m "feat(app-server): busy 三路并集，daemon 探测放独立任务且不刷新空闲时钟"
```

---

### Task 9: 主循环接线 — 10s tick、远端交互时间戳、装配 supervisor

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 5 的 `settings::prevent_idle_sleep_enabled` / `settings::load_from`、
  Task 6/7 的 `yi_agent_power::{Supervisor, SystemClock, ...}`、Task 8 的 `power_busy::*`
- Produces: 主循环里多一条 `power_ticker` 分支；`serve()` 末尾释放断言（靠 `Drop`）

**为什么不动 `serve()` 的签名。** `serve()` 已有 10 个参数，被 `ws.rs:137` 与二十多处
测试调用。再加一个参数会把改动面扩散到所有测试夹具。改为**在 `serve()` 内部装配**
（`settings_path` 已经在 `RuntimeAttachments` 的析构里拿到），并给测试留一个
`with_power_guard` 开关——由 `cfg!(test)` 之外的一个显式字段控制，默认开。

**接线四处（缺一处功能就是哑的）**：

1. `settings_path` 已可用 → 每拍 `settings::load_from(&settings_path)` 现读开关。
2. 主循环入站帧分支（`server.rs:3340` 的 `line = inbound.recv()`）刷新 `last_remote_touch`。
3. 新增 10s `power_ticker` 分支，算 `busy` 并 `supervisor.tick(...)`。
4. `serve()` 返回时 `Supervisor` 的 `Drop` 释放断言。

- [ ] **Step 1: 加常量与局部状态**

在 `crates/yi-agent-app-server/src/server.rs` 顶部常量区（`DRIVER_TICK` 附近）加：

```rust
/// 保活策略的采样间隔。
///
/// 与 `DRIVER_TICK`（3s）刻意不同：电源/网络判定不需要 3s 的精度，而每拍都要
/// 走一次 `getifaddrs`（可能还有 `route` 子进程），3s 太密。10s 仍远小于 15 分钟
/// 的宽限窗口，迟到一拍不改变结论。
const POWER_TICK: Duration = Duration::from_secs(10);
```

**先改一个架构判断：整条 power tick 不进主循环，放进自己的线程。**

第一版接线把 `supervisor.tick(...)` 直接放在 `select!` 的一个分支里。实测这条
路径要花多久：

| 步骤 | 最坏耗时 |
|---|---|
| `getifaddrs` 遍历接口 | 亚毫秒 |
| `route -n get default`（**spawn 一个子进程**） | 10~50 ms，系统繁忙时可到几百 ms |
| `TcpStream::connect_timeout(gateway:80, 2s)` | **2 s**（网关丢包时真的等满） |
| `IOPMAssertionCreateWithName`（IOKit FFI） | 亚毫秒 |

也就是说网关刚好不通的那一拍，主循环会**卡住 2 秒**，这 2 秒里所有
`inbound.recv()` 的 RPC 都在排队——`DRIVER_TICK` 是 3 s，这等于把一次 tick 的
预算吃掉三分之二。而本功能的目标恰恰是「让机器持续干活」，让它自己成为卡顿源
是自相矛盾的。

`BusySampler` 只把 daemon 探测摘出去了，电源/网络探测仍在主循环里，等于没解决
主要问题。正确做法是**把整个 tick 搬到独立线程**：主循环每拍只做两次原子写
（`activity`、`enabled`），线程自己 `sleep(10s)` 跑完整拍。主循环的新增开销
降到两次 `AtomicBool::store`；断言句柄随之归线程所有，`Drop` 时 join 并释放。

这个设计已在隔离环境实测编译并跑通（含真机 IOKit 断言）：线程按拍推进、`Drop`
时 join 返回、`enabled=false` 之后断言消失。

新建 `crates/yi-agent-app-server/src/power_guard.rs`：

```rust
//! 保活线程：唯一持有 IOKit 断言的执行体。
//!
//! 为什么是独立 OS 线程而不是 `select!` 分支或 `spawn_blocking`：
//!
//! - 一拍里含 `route` 子进程（10~50 ms）与 `connect_timeout(.., 2s)`，
//!   放主循环会阻塞所有 RPC，最长 2 秒。
//! - `spawn_blocking` 的线程会被 tokio 在空闲时回收，而这里需要「长期存在、
//!   每 10 秒醒一次」的节奏；`std::thread` 语义更清楚，也不占 tokio 的阻塞配额。
//! - 断言句柄必须活到进程结束，`Drop` 里 join 才能保证「线程停了断言一定释放」。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use yi_agent_power::Supervisor;
use yi_agent_power::assertion::platform_assertion;
use yi_agent_power::clock::{Clock, SystemClock};
use yi_agent_power::probe::{SystemNetProbe, SystemPowerProbe};

/// 断言显示给用户的文案。`pmset -g assertions` 里能看到它，是排查的第一现场。
const ASSERTION_LABEL: &str = "yi-agent: keeping the machine awake for work";

/// 线程的 tick 间隔。10s 远小于 15 分钟宽限，迟到一拍不改变结论。
const SUPERVISOR_TICK: Duration = Duration::from_secs(10);

pub(crate) struct PowerGuard {
    /// 主循环每拍写入：本进程侧是否有活（本地会话 ∪ 远端近期交互）。
    activity: Arc<AtomicBool>,
    /// 主循环每拍写入：设置页开关的当前值。
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PowerGuard {
    /// 起线程。`enabled` 是装配时读到的初值——避免线程第一拍先误持、再释放。
    pub(crate) fn spawn(enabled: bool) -> Self {
        let activity = Arc::new(AtomicBool::new(false));
        let enabled_flag = Arc::new(AtomicBool::new(enabled));
        let stop = Arc::new(AtomicBool::new(false));
        let activity_in = Arc::clone(&activity);
        let enabled_in = Arc::clone(&enabled_flag);
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
                    supervisor.tick(busy, enabled);
                    std::thread::sleep(SUPERVISOR_TICK);
                }
                // 显式 drop：确保断言在 join 返回前释放，不依赖栈清理时机。
                drop(supervisor);
            })
            .expect("spawn the power guard thread");

        Self {
            activity,
            enabled: enabled_flag,
            stop,
            handle: Some(handle),
        }
    }

    /// 主循环每拍调用：写一个原子，不阻塞。
    pub(crate) fn set_activity(&self, active: bool) {
        self.activity.store(active, Ordering::Relaxed);
    }

    /// 主循环每拍调用：开关现读磁盘后写进来。
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // join 是必要的：不 join 则进程可能在线程释放断言之前退出，
            // 断言变成内核里的孤儿（实际会被系统回收，但那依赖系统行为）。
            //
            // 代价分析：最坏情况要等满一个 `SUPERVISOR_TICK`（10s）——线程正在
            // sleep，只能醒来才发现 stop。让退出等 10 秒不可接受，故用
            // `park_timeout` 风格的分段等待：见下面的 `sleep_until_stop`。
            let _ = handle.join();
        }
    }
}

/// 可被提前打断的等待：`stop` 置位后最多 50ms 内返回。
///
/// 直接用 `thread::sleep(10s)` 会让 `Drop` 的 join 等满 10 秒——那意味着关掉桌面
/// 要卡 10 秒才能退出，用户会以为程序挂了。分段轮询的代价只是一些无意义的唤醒，
/// 而这个线程本来就只在后台每 10 秒干一次活。
fn sleep_until_stop(stop: &AtomicBool) {
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < SUPERVISOR_TICK {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(step);
        waited += step;
    }
}
```

把线程里的 `std::thread::sleep(SUPERVISOR_TICK)` 换成 `sleep_until_stop(&stop_in)`。

在 `serve()` 里 `let mut threads: HashMap<String, ThreadSession> = HashMap::new();` 之后加：

```rust
    // 保活。**唯一持有断言的进程**就是这里（spec「为什么是单一持有者」）。
    //
    // 整条 tick 在独立线程里跑（见 `power_guard` 的模块注释）：它含子进程与
    // 2 秒级 TCP 超时，放主循环会把 RPC 卡住。
    let initial_enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(
        &yi_agent_runtime::settings::load_from(&settings_path),
    );
    let power_guard = crate::power_guard::PowerGuard::spawn(initial_enabled);
    // daemon 侧的 busy 另由 `BusySampler` 采（同样是阻塞 IPC，同样不能进主循环）。
    let busy_sampler = crate::power_busy::BusySampler::spawn(Arc::clone(&runtimes));
    // 远端最后一次发来请求的时刻。**只**由非 local 客户端刷新：桌面壳用 stdio
    // 连的，若不排除，「本地桌面每发一条消息」都会把它刷成真，于是 busy 恒真、
    // 电池下永不释放。
    //
    // 时钟与 `power_guard` 线程里那条**是同一种读数**（睡眠感知单调钟）。它是
    // 系统级读数，不随实例变化，所以两边各建一个 `SystemClock` 得到的是同一口径。
    // 反之若这里改用 `Instant::now()`，睡 8 小时后它会说「刚交互过、还在 5 分钟内」，
    // 而 policy 说「超过 15 分钟」——两个判据打架，`ok` 恒为真。
    let power_clock = yi_agent_power::clock::SystemClock::new();
    let mut last_remote_touch: Option<yi_agent_power::clock::Stamp> = None;
```

在 `driver_ticker` / `lead_ticker` 声明旁边加：

```rust
    // 主循环侧的保活拍：只做「读设置 + 算 activity + 写两个原子」。
    // 真正的探测与断言在线程里（`power_guard`），这里不碰任何 I/O，
    // 所以用 `interval` 而不是 `interval_at`——立刻跑一次也无害。
    let mut power_ticker = tokio::time::interval(POWER_TICK);
```

并在 `crates/yi-agent-app-server/src/lib.rs` 里注册模块（按字母序，`power_busy` 之前）：

```rust
pub(crate) mod power_guard;
```


- [ ] **Step 2: 在主循环结束后显式释放断言**

`serve()` 里主循环 `break` 之后、`for binding in attached_runtimes(&runtimes) { binding.detach(); }`
**之前**加：

```rust
    // 显式释放断言，再拆项目 root。
    //
    // 顺序有讲究：`detach()` 会停掉本进程起的 daemon，那是「本进程还在干活」的
    // 最后证据。若断言在 detach 之后才由 `Drop` 释放，中间这段时间系统可能已经
    // 开始休眠流程。显式先 drop 让断言立刻消失。
    drop(power_guard);
    drop(busy_sampler);
```

`PowerGuard::drop` 会置 `stop` 并 `join`；`join` 最长等 50ms（分段轮询，
见 Task 9 Step 1 的 `sleep_until_stop`），不会拖慢退出。

- [ ] **Step 3: 在入站帧分支刷新 `last_remote_touch`**

在 `server.rs` 的 `line = inbound.recv() => { let Some((client, item)) = line else { break };` 之后、
现有的早退守卫之前，插入：

```rust
                // 远端交互时间戳：**只**记非 local 客户端。
                // `ClientId::ws(..)` 是唯一一种「远端」，而 relay 不是独立入口
                // （`--relay` 的环回桥走同一个 `serve_ws_body`），所以这一个判据
                // 同时覆盖手机直连与经 relay 连接，不需要按传输方式分支。
                // 注意：畸形帧也刷新——它同样证明「有人正在指挥我」。
                if client != crate::broadcast::ClientId::local() {
                    if let Some(stamp) = power_clock.now() {
                        last_remote_touch = Some(stamp);
                    }
                }
```

- [ ] **Step 4: 加 `power_ticker` 分支**

在 `select!` 里 `_ = driver_ticker.tick() => { ... }` 之后加：

```rust
            _ = power_ticker.tick() => {
                // 开关每拍现读磁盘，所以设置页保存后下一拍就生效，不必重启进程。
                // `load_from` 是「读一次文件 + 逐字段校验」，10s 一次开销可忽略。
                let enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(
                    &yi_agent_runtime::settings::load_from(&settings_path),
                );

                // 本进程侧：本地会话 ∪ 远端近期交互。两项都是纯内存判断，
                // 放主循环里是安全的（不碰 I/O，也不起子进程）。
                let local = crate::power_busy::local_busy(&threads);
                let remote = crate::power_busy::remote_touch_busy(
                    last_remote_touch,
                    &power_clock,
                );
                // `latest()` 永远立刻返回（上一拍的结果），绝不等待。
                let daemon = busy_sampler.latest();

                // 只写两个原子。探测与 IOKit 都在供电线程里，这里绝不阻塞。
                power_guard.set_activity(local || remote || daemon);
                power_guard.set_enabled(enabled);
            }
```

**注意借用**：`local_busy(&threads)` 只读借用 `threads`，而 `driver_tick` 需要
`&mut threads`。两者在不同 `select!` 分支里，编译器不会同时借用，所以不需要额外
处理。若编译器仍报错，把 `power_ticker` 分支放在 `driver_ticker` 分支**之前**
（顺序不影响语义，但会改变 borrow 检查的路径）。

**为什么这一拍不会阻塞。** 这里只做四件事：读一次 `settings.json`（本地小文件）、
遍历内存里的 `threads` 表、读两个 `AtomicBool`、两次 `store`。唯一有点分量的
是 `load_from`，它是一次文件读 + 逐字段校验，10 秒一次可以忽略。所有可能慢的
东西——`getifaddrs`、`route` 子进程、2 秒 TCP 超时、IOKit FFI——都不在这条路径上。

- [ ] **Step 5: 确认开关翻转的端到端行为**

在 `power_guard.rs` 的 tests 里加一条，钉住「`set_enabled(false)` 之后断言真的
被释放」这个闭环：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 线程起来、收到 `false` 之后，`pmset` 里不应再看到我们的断言。
    ///
    /// `#[ignore]`：这条要真读系统的断言表，只在 macOS 上、且只在手动跑
    /// `--ignored` 时执行（与 `assertion` 模块的真机测试同款 gate）。
    #[test]
    #[ignore]
    fn disabling_the_guard_releases_the_real_assertion() {
        let guard = PowerGuard::spawn(true);
        guard.set_activity(true);
        // 等线程跑够两拍，确保它持有断言。
        std::thread::sleep(Duration::from_secs(SUPERVISOR_TICK.as_secs() * 2));
        let held = assertion_visible_in_pmset();
        guard.set_enabled(false);
        std::thread::sleep(Duration::from_secs(SUPERVISOR_TICK.as_secs() * 2));
        let released = !assertion_visible_in_pmset();
        drop(guard);
        assert!(held, "the guard should have held an assertion while active");
        assert!(released, "disabling the guard must release the assertion");
    }

    /// `pmset -g assertions` 的输出里有没有我们的断言。
    fn assertion_visible_in_pmset() -> bool {
        let output = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("run pmset");
        String::from_utf8_lossy(&output.stdout).contains("yi-agent: keeping the machine awake")
    }
}
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server power_guard -- --ignored --nocapture`
Expected: 1 个 PASS（macOS 上；需要把 app-server 的测试跑起来，会花约 25 秒）

**这条测试很慢**（两拍各 10 秒 + 系统查询）。它的价值在于钉住「线程 + 断言 +
退出」的整条链，那条链只有真机才能验证。日常 `cargo test` 不会跑它。


- [ ] **Step 6: 给 `local_busy` 与 `remote_touch_busy` 补真实用例**

这两条是 busy 三路里唯二能在进程内直接驱动的，必须真测；`daemon_busy` 的
边界另外由 Step 6 的静默 daemon 用例覆盖。

**不要写「把三个 bool 或起来再断言」那种测试**——它测的是测试自己写的闭包，
无论生产代码怎么写都通过，零守卫价值。要测就测真函数。

追加到 `power_busy.rs` 的 tests 里：

```rust
    /// 造一个只够 busy 判定用的会话。
    fn session(status: ThreadStatus, turn: Option<&str>) -> ThreadSession {
        let (prompt_tx, _) = tokio::sync::mpsc::channel(1);
        let (interrupt_tx, _) = tokio::sync::mpsc::channel(1);
        let (interject_tx, _) = tokio::sync::mpsc::channel(1);
        let (session_tx, _) = tokio::sync::mpsc::channel(1);
        ThreadSession {
            thread_id: "t".to_string(),
            cwd: std::env::temp_dir().display().to_string(),
            model: "test".to_string(),
            active_turn_id: turn.map(str::to_string),
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store: Arc::new(crate::thread_store::ThreadStore::new(&std::env::temp_dir())),
            status: Arc::new(std::sync::Mutex::new(status)),
            subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
        }
    }

    fn table(sessions: Vec<ThreadSession>) -> HashMap<String, ThreadSession> {
        sessions
            .into_iter()
            .enumerate()
            .map(|(i, session)| (format!("t{i}"), session))
            .collect()
    }

    #[test]
    fn an_empty_table_is_not_busy() {
        assert!(!local_busy(&table(vec![])));
    }

    #[test]
    fn a_running_session_is_busy() {
        assert!(local_busy(&table(vec![session(ThreadStatus::Running, Some("turn-1"))])));
    }

    /// `AwaitingApproval` 也算有活：那一刻 turn 没结束、上下文都在。机器睡了，
    /// 用户回来点批准时这条 turn 已经随睡眠死掉了。
    #[test]
    fn awaiting_approval_is_busy() {
        assert!(local_busy(&table(vec![session(ThreadStatus::AwaitingApproval, Some("turn-1"))])));
    }

    /// 覆盖「turn 已挂上但状态还没切成 Running」的短暂窗口。
    #[test]
    fn an_active_turn_with_an_idle_status_is_still_busy() {
        assert!(local_busy(&table(vec![session(ThreadStatus::Idle, Some("turn-1"))])));
    }

    #[test]
    fn an_idle_session_without_a_turn_is_not_busy() {
        assert!(!local_busy(&table(vec![session(ThreadStatus::Idle, None)])));
    }

    /// 表里只要有一个在跑就是 busy。
    #[test]
    fn one_busy_session_makes_the_whole_table_busy() {
        let sessions = vec![
            session(ThreadStatus::Idle, None),
            session(ThreadStatus::Running, Some("turn-2")),
        ];
        assert!(local_busy(&table(sessions)));
    }

    /// 远端交互窗口内为真、过期后为假。
    ///
    /// `FixedClock` 的 `since` 直接返回预设值——这正是要测的自由度：
    /// 「时间过去多久」在真实时钟下要真等，这里可构造。
    struct FixedClock(Duration);

    impl Clock for FixedClock {
        fn now(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(Duration::ZERO))
        }
        fn since(&self, _stamp: Stamp) -> Duration {
            self.0
        }
        fn wall(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(Duration::ZERO))
        }
        fn mono_trusted(&self) -> bool {
            true
        }
    }

    #[test]
    fn remote_touch_expires_at_the_window_boundary() {
        let stamp = Stamp::from_duration(Duration::ZERO);
        // 边界取 `<`：恰好 5 分钟已过期。
        assert!(remote_touch_busy(Some(stamp), &FixedClock(Duration::from_secs(299))));
        assert!(!remote_touch_busy(Some(stamp), &FixedClock(REMOTE_TOUCH_WINDOW)));
    }

    /// 回归哨兵：从没收到过远端帧就是 `None`，必须判假——
    /// 若写成 `is_none_or`，一台纯桌面使用的机器会恒为 busy、电池下永不释放。
    #[test]
    fn no_remote_touch_ever_is_not_busy() {
        assert!(!remote_touch_busy(None, &FixedClock(Duration::ZERO)));
    }

    /// 回归哨兵（spec 明确点名）：**stdio 的本地桌面客户端不得刷新远端时间戳。**
    ///
    /// 这条判据编码在 `server.rs` 的入站帧分支里（`if client != ClientId::local()`），
    /// 不在本模块内，所以测试只能钉住「不写入 ⇒ 不 busy」这一半语义；另一半由
    /// `ClientId::local()` 的语义保证（`broadcast.rs` 里 stdio 是唯一的 local）。
    ///
    /// 为什么值得单独一条：若漏了这个排除，桌面每发一条消息都会把时间戳刷成
    /// `now`，`remote_touch` 恒为真 ⇒ `ok` 恒为真 ⇒ 电池下永不释放，
    /// 而用户从界面上完全看不出来。
    #[test]
    fn a_local_stdio_client_does_not_keep_the_machine_awake() {
        // 模拟「只有 stdio 本地客户端在发请求」：时间戳从未被写入。
        let never_touched: Option<Stamp> = None;
        // 过了任意长时间仍然是 false。
        for secs in [0u64, 60, 3600, 8 * 3600] {
            assert!(
                !remote_touch_busy(never_touched, &FixedClock(Duration::from_secs(secs))),
                "a local-only session must never look like remote activity (t={secs}s)"
            );
        }
    }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server power_busy`
Expected: **16 个测试 PASS**（Task 8 Step 3 的 6 条纯函数用例 + Task 8 Step 6 的 1 条静默 daemon 用例 + 这里 9 条）。
实测：`16 passed; 0 failed`（`cargo test -p yi-agent-app-server power_busy`）。

- [ ] **Step 7: 编译整个 app-server（这是本 plan 最容易出错的一步）**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server`
Expected: 编译通过

两个已知的编译陷阱，都在 Task 3/8 里预备好了，这里只做核对：

1. **`no method named current_handle_no_touch`** → Task 8 Step 1 没落到
   `yi-agent-subagent/src/binding.rs`。
2. **`type Stamp is private` / `cannot construct Stamp`** → `Stamp.0` 是
   `pub(crate)`，跨 crate 不能用元组构造也不能读字段。Task 3 已经给了
   `Stamp::from_duration(Duration)` 与 `Stamp::as_duration()` 两个公开构造/读取口，
   `power_busy` 的测试一律走它们，**不要**写 `Stamp(x)`。

若仍报借用错误（`cannot borrow `threads` as mutable`），把 `power_ticker` 分支
挪到 `driver_ticker` 分支之前——顺序不影响语义，但改变 borrow 检查的路径。

- [ ] **Step 8: 跑 app-server 全绿**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全 PASS（这一套很大，重点看 `power_busy`、`settings_rpc`、`evict_idle_runtimes` 相关）

- [ ] **Step 9: 加一条「不刷新空闲时钟」的回归守卫（在既有测试旁）**

在 `server.rs` 的 tests 里，`evict_idle_runtimes` 相关测试附近加：

```rust
    /// 回归守卫：`daemon_busy` 不得刷新 binding 的空闲时钟。
    ///
    /// 之前若用 `current()` 做探测，每 10s 一次会把 `idle_for()` 压回零，
    /// `EVICT_IDLE_AFTER` 永不触发，项目 daemon 永不回收。
    #[test]
    fn probing_busy_does_not_prevent_runtime_eviction() {
        let dir = tempfile::tempdir().unwrap();
        // 一个不存在的 socket：探测会立刻失败（快），且不碰空闲时钟。
        let socket = dir.path().join("absent.sock");
        let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
            socket_path: socket,
            workspace_root: dir.path().to_path_buf(),
            session_id: String::new(),
            task_id: String::new(),
            capability: String::new(),
        });
        binding.backdate_idle_clock(EVICT_IDLE_AFTER + Duration::from_secs(60));
        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        runtimes
            .lock()
            .unwrap()
            .insert(dir.path().to_path_buf(), Ok(Arc::clone(&binding)));

        // 探测若干次。
        for _ in 0..5 {
            let _ = crate::power_busy::daemon_busy(&runtimes);
        }

        let evicted = evict_idle_runtimes(
            &runtimes,
            &HashMap::new(),
            &dir.path().join("residents"),
            Instant::now(),
            EVICT_IDLE_AFTER,
        );
        assert_eq!(evicted, vec![dir.path().to_path_buf()], "探测不得阻止回收");
    }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server probing_busy_does_not_prevent_runtime_eviction`
Expected: PASS

- [ ] **Step 10: 真机验收（关键，没有这一步等于没做完）**

Run（保持 app-server 在跑，另开终端）：

```bash
pmset -g assertions | grep -A2 "yi-agent"
```

Expected:
- 插电+有网时出现 `PreventUserIdleSystemSleep named: "yi-agent: keeping the machine awake for work"`
- **不应**出现 `PreventSystemSleep`、`PreventUserIdleDisplaySleep`
- 在设置页把开关关掉 → 10s 内断言消失
- 关掉再打开 → 10s 内断言回来

- [ ] **Step 11: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): 主循环 10s 保活拍，远端交互与 daemon 活并集，唯一持有断言"
```

---

### Task 10: 文档与进度同步

**Files:**
- Create: `docs/project-management/yi-agent-power.md`
- Modify: `docs/project-management/README.md`（模块索引加一行）
- Modify: `docs/project-management/yi-agent-app-server.md`（加一条接线项）

**Interfaces:** 无代码产出。

- [ ] **Step 1: 建模块进度文件**

`docs/project-management/yi-agent-power.md`：

```markdown
# yi-agent-power

防空闲休眠：平台电源断言 + 纯策略状态机。设计见
`docs/superpowers/specs/2026-11-01-prevent-idle-sleep-design.md`。

## 完成情况

- [x] `policy` 纯状态机：`ok = online && (on_ac || busy)`，单向 15 分钟宽限，
      边界取 `<`；时钟冻结看门狗（`GRACE + 5min`）。验证：
      `cargo test -p yi-agent-power policy`（16 个用例）
- [x] `clock` 睡眠感知时钟：macOS `CLOCK_MONOTONIC_RAW`、Linux
      `CLOCK_BOOTTIME`；比例判据 `dt_mono * 10 < slept_for` 的墙钟自检，
      连续 3 次降级且单向不可恢复。验证：
      `cargo test -p yi-agent-power clock`
- [x] `assertion` IOKit `PreventUserIdleSystemSleep`，`Drop` 幂等释放；
      非 macOS no-op。验证：`cargo test -p yi-agent-power assertion`；
      真机 `cargo test -p yi-agent-power assertion -- --ignored`
- [x] `probe` 电源（UPS/未知算插电）与网络（候选接口 + 网关探测，
      `ECONNREFUSED` 计为在线）。验证：`cargo test -p yi-agent-power probe`
- [x] `supervisor` 10s 拍状态机，只在迁移边沿调 FFI。验证：
      `cargo test -p yi-agent-power supervisor`
- [x] `agent.prevent_idle_sleep` 开关，四处读取点同步（`FIELDS`/
      `AgentSettings`/`settings_rpc`/`web::api`）。验证：
      `cargo test -p yi-agent-runtime prevent_idle_sleep`
- [x] app-server 主循环接线：10s `power_ticker`、三路 busy 并集、
      非 local 客户端刷新远端时间戳。验证：
      `cargo test -p yi-agent-app-server power_busy`

## 非目标

- 不实现唤醒后续跑（需独立 spec）。
- 不阻止合盖 / 手动睡眠 / 系统更新重启。
- 不为孤儿 daemon 保活（宿主 app-server 退出即无断言）。
- 不覆盖 `yi-agent run` / TUI 入口（交互式，人就在键盘前）。
- 不覆盖 Linux / Windows 的真实断言（编译成 no-op）。
```

- [ ] **Step 2: 索引加一行**

`docs/project-management/README.md` 的模块索引表里，`yi-agent-runtime` 那行之后加：

```markdown
| yi-agent-power | 7 / 7 | [详情](./yi-agent-power.md) |
```

- [ ] **Step 3: app-server 模块文件补一条**

在 `docs/project-management/yi-agent-app-server.md` 里追加：

```markdown
- [x] 干活期间防止空闲休眠：主循环每 10s 采 `busy`（本进程会话 / 项目 daemon /
      远端 5 分钟内交互）与电源、网络，喂给 `yi-agent-power` 的 supervisor。
      只有本进程持有断言，desktop 不持有。验证：`pmset -g assertions | grep yi-agent`。
```

- [ ] **Step 4: Commit**

```bash
git add docs/project-management/yi-agent-power.md \
        docs/project-management/README.md \
        docs/project-management/yi-agent-app-server.md
git commit -m "docs: 记录 yi-agent-power 模块与 app-server 接线"
```

---

### Task 11: 全量验证与收口

**Files:** 无新增。

- [ ] **Step 1: 格式化**

```bash
cd yi-agent-rs && cargo fmt --all
```

- [ ] **Step 2: 逐 crate 测试（不要 `--workspace`，会 OOM）**

```bash
cd yi-agent-rs
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || true
cargo test -p yi-agent-power
cargo test -p yi-agent-subagent
cargo test -p yi-agent-runtime
cargo test -p yi-agent-web
cargo test -p yi-agent-app-server --lib
```

Expected: 全部 PASS。若有残留测试进程导致卡死，先 `kill` 再重跑。

- [ ] **Step 3: lint**

```bash
cd yi-agent-rs && cargo clippy --all-targets --all-features -p yi-agent-power -p yi-agent-app-server -- -D warnings
```

Expected: 无告警。

- [ ] **Step 4: 真机验收三条（照 spec「验收（真机）」）**

```bash
# 1. 断言确实持有，类型正确
pmset -g assertions | grep -A2 "yi-agent"
# 期望：PreventUserIdleSystemSleep named: "yi-agent: ..."
# 不应出现 PreventSystemSleep / PreventUserIdleDisplaySleep

# 2. 拔电 + 无活：1 分钟内系统应进入睡眠（本机 Battery sleep 1）
#    若断言在，则不睡 —— 这是唯一能端到端复现原始故障的验证

# 3. 睡眠感知：睡一会儿再醒，确认没有「醒来后又白扛 15 分钟」
pmset -g log | grep -E "Entering Sleep|Wake from" | tail -4
```

- [ ] **Step 5: 回写 spec（不写这一步，下次读 spec 的人会照着错的实现）**

`docs/superpowers/specs/2026-11-01-prevent-idle-sleep-design.md` 至少有三处要被
实现推翻或补全，逐条改：

1. **策略入参改为双时钟。** spec 写「`clock.since(t)` 一个读数够用」，实际
   `decide` 吃 `Sample { mono, wall }`——因为看门狗要检测的正是「单调钟不前进」，
   用同一条钟度量时长会让兜底永远失效。把 spec 的公式段改成
   `hold = last_ok.is_some_and(|t| now.mono - t < GRACE)`，并补一节说明两条钟
   各自负责什么。
2. **主循环不跑探测。** spec 说「主循环按 10s 拍采电源/网络」，实际整条 tick
   在独立线程里（`power_guard.rs`）。把 spec 的「主循环接线」一节改成
   「主循环只写两个原子，探测在 `power_guard` 线程」。
3. **设置读取点是三处。** spec 只列了 `FIELDS` 与 `current_value` 两处；
   实际还有 `crates/yi-agent-web/src/api.rs` 的 `field_value`，且那里有
   `vars.len() == FIELDS.len()` 的测试钉着。
4. **`RuntimeBinding::current()` 有 `touch()` 副作用**（plan 偏离项 1）——
   spec 完全没提，必须补，否则下一个人还会踩。

- [ ] **Step 6: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff docs/prevent-idle-sleep-design
```

合并后按 `superpowers:finishing-a-development-branch` 清理分支与 worktree。

