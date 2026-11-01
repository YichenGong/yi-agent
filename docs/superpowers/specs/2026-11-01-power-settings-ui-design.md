# 休眠配置进设置界面 设计

**日期**: 2026-11-01
**状态**: 待实现（无前置阻塞；原「被排版重做阻塞」的判断在核实后已解除，见 §0）
**相关代码**:
- `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（`FIELDS` / `AgentSettings` / `normalized` / `load_from`）
- `yi-agent-rs/crates/yi-agent-power/src/policy.rs`（`GRACE` / `WATCHDOG_SLACK` / `Policy`）
- `yi-agent-rs/crates/yi-agent-power/src/supervisor.rs`（`Supervisor`）
- `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs`（供断线程）
- `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs`（`REMOTE_TOUCH_WINDOW`）
- `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs`（`current_value`）
- `yi-agent-rs/crates/yi-agent-web/src/api.rs`（`field_value`）
- `desktop/src/components/SettingsRuntimeTab.tsx`（分组常量与渲染）

---

## 0. 前端现状（原「被排版重做阻塞」的约束已解除）

**结论：没有阻塞，本设计可以直接落地。**

初稿写这份 spec 时，「运行参数」页正被另一份设计重写（`2026-11-01-runtime-settings-layout`），当时判断必须等它先合并。**该重做现在已经完成并合入 main**（已核实）：

- 实现提交 `60eccea9` / `a4ceab0c` / `dd2ab871` / `dd1b1ff5`，全部是当前 HEAD 的祖先；
- `docs/project-management/desktop.md` 已记 `- [x] 「运行参数」页排版重做`；
- `docs/project-management/README.md` 的 desktop 计数已更新为 `106 / 117`。

所以 §0 原本的「执行顺序固定为：排版重做 → 本设计」**作废**，本设计不需要等待任何前置。

### 0.1 落地时面对的真实结构（已核实）

`desktop/src/components/SettingsRuntimeTab.tsx` 现在是：

| 常量 / 函数 | 现状 | 本设计要做什么 |
|---|---|---|
| `GROUP_LABELS`（`:10`） | 4 组中文名 | 加 `power: "电源与休眠"` |
| `GROUP_HINTS`（`:18`） | 4 组一句话说明 | 加 `power` 那句 |
| `GROUP_ORDER`（`:26`） | `["agent", "compact", "tui", "startup"]` | 加 `"power"` |
| `VALUE_LABELS`（`:34`） | 枚举中文映射 | **要加一条**：`"power.prevent_idle_sleep": { true: "开", false: "关" }`（与 `tui.notifications` 完全同构，见下） |
| `labelFor`（`:68`） | 纯函数，查不到原样返回 | **不用改** |

### 0.2 必须改的那条测试

`SettingsRuntimeTab.test.tsx:188` 有精确断言：

```ts
expect(GROUP_ORDER).toEqual(["agent", "compact", "tui", "startup"]);
```

加 `power` 后它会红。**但这不能靠把 `power` 塞进 `toEqual` 数组了事**——这张表会随功能增长，每加一组就要改一次断言，而该测试真正要守的是「没有组漏出英文名」（`GROUP_LABELS[group] ?? group` 那行）。

改成对 `FIELDS` 的实际分组做覆盖断言：

```ts
// 后端每多一个组，前端必须给出中文名与说明——否则页面上直接露出英文组名。
for (const group of GROUP_ORDER) {
  expect(GROUP_LABELS[group]).toBeTruthy();
  expect(GROUP_HINTS[group]).toBeTruthy();
}
expect(GROUP_ORDER).toContain("power");
```

理由：前两条循环断言已覆盖「漏出英文名」，`toContain` 钉住顺序表里确实登记了新组。若仍保留精确 `toEqual`，至少要把期望数组同步成 `["agent", "compact", "tui", "startup", "power"]`——**不推荐**，理由同上。

### 0.3 `power` 组的排序位置

放**最后**（`agent → compact → tui → startup → power`）。理由：前四组都影响单次对话的执行行为，`power` 影响的是整机行为，影响面更大但改动频率最低，放末尾符合 `GROUP_ORDER` 注释里「按影响面从大到小」之外的另一条隐含规则——按「离一次对话的距离」排。若将来觉得它该更靠前，只动 `GROUP_ORDER` 一处即可。

**`VALUE_LABELS` 必须加，不能省**：`power.prevent_idle_sleep` 是 `Select`（`options: ["true","false"]`），与已有的 `tui.notifications`（`{ true: "开", false: "关" }`）是同一种形态。不加这一条，下拉框里会显示英文 `true` / `false`，而旁边「终端通知」那一组的同类开关显示的是「开 / 关」——同一页里两种风格。

注意 `labelFor` 的查表键是 `field.key`（完整点路径），且映射表的键是裸 `true` / `false`（JS 对象字面量里 `true:` 会被转成字符串 `"true"`）。抄 `tui.notifications` 那一行即可，不要写成 `"power.prevent_idle_sleep": { "true": "开" }` 之外的其它形态——功能等价，但保持全表一致更易读。

### 0.4 为什么不能靠 `extra` 回退兜底

`SettingsRuntimeTab.tsx:112-119` 会把不在 `GROUP_ORDER` 里的组追加到末尾，页面能渲染出来，但组标题走 `GROUP_LABELS[group] ?? group`，会直接漏出英文 `power`。必须显式登记在两张表里。

---

## 1. 问题

「干活期间防止空闲休眠」已经实现（`docs/superpowers/specs/2026-11-01-prevent-idle-sleep-design.md`），开关 `agent.prevent_idle_sleep` 也已进设置界面——它由 `settings::FIELDS` 驱动渲染，自动出现在「运行参数 → Agent」组。

但用户拿不到真正想调的东西：

1. **找不到**。开关埋在一串 agent 执行参数（最大轮次、子 agent 上限、系统提示词、技能目录预算）的最后一项，与「休眠」这个心智模型毫无关联。
2. **调不了**。三个真正影响行为的量全是编译期常量：

| 常量 | 位置 | 当前值 | 用户会想调它的真实场景 |
|---|---|---|---|
| `GRACE` | `policy.rs:4` | 15 分钟 | 「15 分钟太钝了，我拔了电就想让它睡」 |
| `REMOTE_TOUCH_WINDOW` | `power_busy.rs:30` | 5 分钟 | 「手机连着的窗口该多长」 |
| `WATCHDOG_SLACK` | `policy.rs:5` | 5 分钟 | 安全兜底，**不开放**但随宽限自动缩放（见 §4.2） |

所以需求是：**把休眠相关配置集中成独立一组，并把两个用户真会调的时长开放出来。**

---

## 2. 目标与非目标

**目标**

- 设置界面出现独立的「电源与休眠」组，三个休眠配置集中一处。
- `power.idle_grace_minutes`（条件消失后再保活多久）与 `power.remote_touch_minutes`（远端交互有效窗口）可配。
- 老用户的 `agent.prevent_idle_sleep` 设置**不丢失**（一次性迁移）。
- 改设置后不重启进程即生效（最坏 20 秒，见 §5.4）。
- 坏配置一律回落默认，**绝不让配置错误导致不保活**。

**非目标**

- **不把** `WATCHDOG_SLACK`、`SUPERVISOR_TICK`（10s）、`CONNECT_TIMEOUT`（2s）开放成用户字段。注意区分：「不开放」不等于「不动」——`WATCHDOG_SLACK` 会改成由宽限推导（§4.2），但它不出现在设置界面；另两个完全不动（§4.3）。
- 不加「电池策略」开关（改变 `ok = online && (on_ac || busy)` 的语义，会动 16 条已有 `policy` 测试，单独一轮）。
- 不做诊断面板（「当前是否持有断言」「上次判定原因」需新 RPC 与状态回传，单独一轮）。
- 不改断言类型（仍是 `PreventUserIdleSystemSleep`）。
- 不覆盖 `yi-agent run` / TUI 入口。

---

## 3. 字段设计

### 3.1 新分组 `power`

三个字段全部进 `power` 组：

| key | 标签 | 类型 | 默认 | 范围 | 说明 |
|---|---|---|---|---|---|
| `power.prevent_idle_sleep` | 干活期间防止休眠 | `Select` | `"true"` | `"true"` / `"false"` | 总开关（由 `agent.` 迁移而来） |
| `power.idle_grace_minutes` | 条件消失后再保活多久（分钟） | `Number` | `"15"` | 1–120 | 对应 `GRACE` |
| `power.remote_touch_minutes` | 远端交互有效窗口（分钟） | `Number` | `"5"` | 1–60 | 对应 `REMOTE_TOUCH_WINDOW` |

分组中文名：**「电源与休眠」**。组说明：「有网且（插电 或 有活在跑）时阻止系统空闲休眠」。

**新增 `power` 组必须同步改的硬编码列表**（漏掉会留下 `{"power":{}}` 空壳噪音，或让字段读回 `null`）：

| 位置 | 现状 | 改法 |
|---|---|---|
| `settings.rs:636` `for group in ["agent", "compact", "startup", "tui"]` | 建删空组壳的白名单 | 加 `"power"` |
| `settings.rs` `Settings` 结构体 | 只有 `agent/compact/startup/tui` 四个字段 | 加 `pub power: PowerSettings` |
| `settings_rpc.rs:46` `current_value` 的 `match spec.key` | 逐键列举 | 加三个分支（`_ => Value::Null` 兜底会让漏改静默返回 null） |
| `yi-agent-rs/crates/yi-agent-web/src/api.rs` `field_value` 的 `match spec.key` | 逐键列举 | 加三个分支 |
| `SettingsRuntimeTab.tsx` 三张分组表 | 见 §0 | 见 §0 |

前两条是 `settings.rs` 内部、漏了会直接编译错或落盘出脏数据（`PowerSettings` 缺字段则 `FIELDS` 引用的 `self.power.*` 编译不过；空组白名单漏了只是留下 `{"power":{}}` 噪音）。

后三条是「漏了**不报错**、只是功能静默不生效」：

- `settings_rpc::current_value` 有守卫测试（`every_field_in_fields_can_be_read_back`，见 §7.4），漏改会红；
- `web::api::field_value` **没有任何测试**，漏改静默返回 `null`——只能靠人眼或 §7.4 给的 `grep -c` 自查；
- 前端两张表（`GROUP_LABELS` / `GROUP_HINTS`）与 `GROUP_ORDER` 见 §0（无阻塞，直接改）。

### 3.2 键名为什么是二层的

`settings.rs` 的 `apply_op` 用 `key.split_once('.')` 分 `group` / `field`（`settings.rs` 内 `apply_op`）。键名多一层点（`power.a.b`）会让 `field` 变成 `a.b`，写入路径与读取路径不一致。所以键必须恰好是 `power.<field>`。

### 3.3 为什么时长用「分钟」而不是「秒」

`FieldType::Number` 的校验是 `u64` 且有 `min`/`max`（`validate_value`）。用分钟：

- 用户可以直接填 15，不必算 900；
- 上下限（1–120）落在人类可读量级，错误提示「must be at most 120」有实际意义；
- 秒级精度对本功能无意义——策略每 10 秒才判一次，秒级差异会被采样粒度吃掉。

内部仍按 `Duration` 使用，只在读写边界乘 60。

### 3.4 类型选择

- 两个新字段存 `Option<u64>`，`deserialize_with = "de_optional_u64"`。该 helper **已存在**（`settings.rs:343`，`de_optional_u32` / `u16` / `usize` 都由它派生），直接复用，不要新写。
- 开关沿用 `Option<String>` + `de_optional_string`（`FieldType::Select` 只接受 JSON 字符串，用 `Option<bool>` 会写进去读不回来——这是原设计的既有结论）。

**`default_display` 必须与常量对上，且有两条既有测试盯着它**（已核实）：

- `settings.rs:932` `default_display_matches_the_runtime_constants`
- `config.rs:713` 同一形状的断言（`"default_display for {key} drifted from the runtime constant"`）

两条都是「`cases` 数组逐项比对 `find_spec(key).default_display == 常量.to_string()`」。**不是遍历 `FIELDS` 的强约束，而是逐项列举**——所以新增字段**不加进 `cases` 就不会被检查**，写了 `default_display: "15"` 但常量其实是 900 秒 / 15 分钟也不会红。

本设计的处理：**不复用 `config.rs` 的既有常量**（那些是 agent/compact 的执行参数常量，语义不同），而是在 `settings.rs` 内为两个新字段各定义一对常量并在测试里比对：

```rust
// 单位是分钟——与字段语义一致，转换只在读取时乘 60。
pub const POWER_IDLE_GRACE_MINUTES_DEFAULT: u64 = 15;
pub const POWER_REMOTE_TOUCH_MINUTES_DEFAULT: u64 = 5;
```

然后在这两条测试的 `cases` 各加两行。这样「默认值只有一处」的回归网对 `power` 组同样成立。

**`power.prevent_idle_sleep` 的 `description` 要改**：现有那条写死了「条件消失 15 分钟后恢复」（`settings.rs:252`）。15 分钟变成可配后，这句话会与实际行为不符。改成不写具体数字：

```
"有网且（插电 或 有活在跑）时阻止系统空闲休眠；条件消失后按「保活窗口」再等一段时间"
```

`label`（`干活期间防止休眠`）与 `options`（`["true", "false"]`）不变。

---

## 4. 配置如何流到策略层

这是本次改动技术上最实质的部分。四个层面依次改。

### 4.1 `Policy` 参数化

现在 `Policy::decide` 直接引用 `GRACE` 常量，`Policy` 结构体没有时长字段。

改成：

```rust
pub const DEFAULT_GRACE: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);

pub struct Policy {
    grace: Duration,
    watchdog_slack: Duration,
    last_ok: Option<Duration>,
    hold_since: Option<Sample>,
    disabled: bool,
}

impl Policy {
    /// 默认窗口。已有 16 条测试走这条路，零改动。
    pub fn new() -> Self { Self::with_windows(DEFAULT_GRACE, DEFAULT_WATCHDOG_SLACK) }

    pub fn with_windows(grace: Duration, watchdog_slack: Duration) -> Self { .. }

    pub fn set_windows(&mut self, grace: Duration, watchdog_slack: Duration) { .. }
}
```

`decide` 里两处替换：`held_for > GRACE + WATCHDOG_SLACK` → `held_for > self.grace + self.watchdog_slack`；`now.mono.saturating_sub(last) < GRACE` → `< self.grace`。

**必须同时处理的一个陷阱**：`Policy` 现在派生 `#[derive(Debug, Default)]`（`policy.rs:40`）。加了两个 `Duration` 字段后，`Default` 会把它们置为 **0 秒**，而不是 15 分钟——`Policy::default()` 就变成一个「立刻放弃保活」的策略，而且**不报错**。

处理方式：手写 `impl Default for Policy`，转调 `Self::new()`，并从 `#[derive(..)]` 中删掉 `Default`。若漏了这步，所有走 `Default` 的路径（包括将来有人写测试时顺手用 `..Default::default()`）都会静默退化。

**保留 `GRACE` 常量名做别名**（`pub const GRACE: Duration = DEFAULT_GRACE;`）可以让已有 16 条测试零改动。已确认（`grep -n "GRACE" policy.rs`）：测试里只在两处以 `GRACE + WATCHDOG_SLACK` 的形式算 deadline（`policy.rs:292`、`policy.rs:329`），两处都在 `#[cfg(test)]` 内部。所以「保留别名」还是「改测试」都可以，成本都极低；**建议保留别名**，让 diff 只落在生产代码上。

### 4.2 `watchdog_slack` 跟着 `grace` 缩放，且不开放给用户

**先把看门狗的真实触发条件说准**（已核实 `policy.rs:66-77` 与 `clock.rs`）：它要求**两个条件同时成立**——

```rust
let held_for = now.wall - since.wall;     // 墙钟：扛了多久
let mono_grew = now.mono > since.mono;    // 单调钟：自 hold 开始有没有走过一步
if !mono_grew && held_for > GRACE + WATCHDOG_SLACK { 强制释放 }
```

`mono` 用的是 `CLOCK_MONOTONIC_RAW`（macOS）/ `CLOCK_BOOTTIME`（Linux）。`TrustGuard` 的判据（`clock.rs:63-66`，已核实）是：

```rust
let slept_for = dt_wall - dt_mono;
slept_for > Duration::from_secs(300) && dt_mono * 10 < slept_for
```

即「墙钟比单调钟多走了 5 分钟以上，且单调钟只走了不到它的 1/10」，连续 3 拍成立就**永久**降级到墙钟。

所以 `!mono_grew` 的成因分两种，它们的性质不同：

- **信任期**：单调钟连续多拍读取**完全相同**（严格不变，不是「走得慢」）。注意 `is_suspicious` 只要求 mono 慢到 1/10 以下，一个「慢慢走但没停」的钟可以触发降级、却**不**触发看门狗（`mono_grew` 为真）。这两条判据的松紧不同，不能混为一谈。
- **降级后**：`now()` 返回的是墙钟，于是 `mono` 其实是墙钟——`mono_grew` 恒为真，看门狗**永不触发**。

也就是说看门狗不是「比宽限再多等一会儿的兜底」，而是「mono 这条钟坏了、宽限判据不可信时，按墙钟兜底放睡」。

**那为什么还要让 slack 跟随 grace 缩放？** 因为在**信任期且 mono 恰好冻结**的那条路径上，`held_for`（墙钟）会持续增长，而 `GRACE` 本来也是用 mono 度量的——此时用 `wall > grace + slack` 来近似 `mono > grace` 才有意义。若用户把宽限调到 120 分钟而 slack 固定 5 分钟，那么只要 mono 冻结过一次、墙钟走过 125 分钟，就会在「按用户设定还该继续扛」的时候强制释放。

这个差异只在**时钟异常**的路径上体现，正常路径（mono 正常前进）不受影响——但正因为是异常路径，用户不可能自己排查，所以参数关系必须在设计上保持自洽。

推导规则固定为：

```
watchdog_slack = max(DEFAULT_WATCHDOG_SLACK, grace / 3)
```

实测输出（已用 rustc 验证 `(grace / 3).max(Duration::from_secs(300))`）：

| 宽限 | slack | 看门狗触发点（墙钟） |
|---|---|---|
| 1 分钟 | 5 分钟 | 6 分钟 |
| 5 分钟 | 5 分钟 | 10 分钟 |
| 15 分钟 | 5 分钟 | 20 分钟（与现状一致） |
| 60 分钟 | 20 分钟 | 80 分钟 |
| 120 分钟 | 40 分钟 | 160 分钟 |

下限 5 分钟的理由：宽限设成 1 分钟时，`1/3 = 20 秒` 的 slack 会让看门狗过于敏感——一次短暂的 `sample()` 抖动就可能被判成时钟故障。

这个推导放在 `power_guard.rs` 内的私有函数 `slack_for(grace)` 里（它已依赖 `yi-agent-power`），而不是 `Policy` 内部：`Policy` 保持「给我两个窗口值」的纯函数语义，便于测试；推导规则单点定义，有独立测试钉住。

### 4.3 为什么不开放 `SUPERVISOR_TICK` 与 `CONNECT_TIMEOUT`

- **`SUPERVISOR_TICK`（10s）**：一拍里含 `route` 子进程（10–50ms）与 `connect_timeout(2s)`。用户若设成 1 秒，等于每秒起一个子进程并可能等 2 秒 TCP 超时——功能自己变成机器负载源。
- **`CONNECT_TIMEOUT`（2s）**：它是「判断没网」的容忍时间，调大让每拍最坏耗时线性增长；调小会在网关稍慢时误判离线 → 不保活（后果更严重的方向）。

两个都属于「调错会让功能自伤」的参数，不给用户。

### 4.4 `Supervisor` 与 `PowerGuard` 的数据通路

真实结构（已核实）：

- `PowerGuard::spawn(enabled: bool)`（`power_guard.rs`）持有三个 `Arc<AtomicBool>`：`activity` / `enabled` / `stop`。主循环调 `set_activity` / `set_enabled` 写入，线程每拍读。
- 线程内建 `Supervisor::new(clock, power, net, assertion, enabled)`，循环体是「读两个原子 → `supervisor.tick(busy, enabled)` → `sleep_until_stop`」。
- 主循环 `power_ticker`（`server.rs:3868`，间隔 `POWER_TICK = 10s`）每拍 `load_from(settings_path)`、算 `enabled` / `local` / `remote` / `daemon`，只写两个原子。

改造：新增 `Arc<AtomicU64>` 两个（存**秒数**）：`grace_secs`、`remote_secs`。`PowerGuard::spawn` 增加两个初值参数（装配时读一次设置，与现有 `enabled` 的做法一致——避免线程第一拍用错窗口）。

**`spawn` 签名的连带改动**（已核实全部 3 个调用点）：

| 位置 | 现状 | 改法 |
|---|---|---|
| `server.rs:3274` | `PowerGuard::spawn(initial_enabled)` | 加两个窗口初值 |
| `power_guard.rs:131`（测试，`#[ignore]`） | `PowerGuard::spawn(true)` | 传默认窗口 |
| `power_guard.rs:159`（测试） | `PowerGuard::spawn(false)` | 传默认窗口 |

另需新增一个 `set_windows(&self, grace_secs: u64, remote_secs: u64)` 方法（与 `set_enabled` 同形状：只 `store`，不阻塞），由主循环每拍调用。

线程每拍：

```
let busy    = activity.load();
let enabled = enabled_in.load();
let grace   = Duration::from_secs(grace_secs_in.load());
// 每拍重算并下发，成本是两次 store，远低于一次时钟采样。
supervisor.set_windows(grace, slack_for(grace));
supervisor.tick(busy, enabled);
```

`slack_for(grace)` 即 §4.2 的推导。**判定与下发都放在线程内、每拍无条件执行**，不做「值变了才下发」的优化：省下的两次 atomic load 微不足道，而「只在变化时下发」需要额外保存上一拍的值，多一处可能与真实状态不同步的状态。

### 4.4.1 `power_windows_minutes()` 的签名与语义

`settings.rs` 里新增一个纯函数，把「可选字段 → 实际生效窗口」这一层收口，供主循环、`write_to` 的跨字段校验、以及测试共同使用：

```rust
/// 两个窗口的实际生效值（分钟）。`None` 回落到默认。
/// **只兜 `None`，不兜越界**——越界值由 `normalized()` 的 `clamp!` 先行清空，
/// 调用方必须先取得经过 `normalized()` 的 `Settings`（见下方第 3 条）。
/// 不做 slack 推导，也不做单位换算。
pub fn power_windows_minutes(settings: &Settings) -> (u64, u64) {
    let grace = settings.power.idle_grace_minutes
        .unwrap_or(POWER_IDLE_GRACE_MINUTES_DEFAULT);
    let remote = settings.power.remote_touch_minutes
        .unwrap_or(POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    (grace, remote)
}
```

三个刻意的取舍：

1. **返回分钟而不是 `Duration`**：名字里带单位，调用方看到 `(15, 5)` 不会误当秒用。换算成 `Duration::from_secs(m * 60)` 由调用点做，一处一次。
2. **不做 slack 推导**：`watchdog_slack` 是策略层概念。已核实依赖方向——`yi-agent-runtime` 与 `yi-agent-power` **互不依赖**（各自 Cargo.toml 里都没有对方），`yi-agent-app-server` 同时依赖两者。把 slack 推导放进 `settings` 会迫使 `runtime` 认知策略层概念，虽不必新增 crate 依赖（纯算术），但会把「策略参数怎么定」这个知识错放到「配置怎么存」的模块里。所以推导放在 `power_guard.rs` 的私有函数 `slack_for(grace)`——那里本来就 `use yi_agent_power::Supervisor`，放这儿零成本。
3. **调用前必须已经过 `normalized()`**。这个函数只兜 `None`，**不兜越界值**——`Some(0)` 会原样返回 `0`。所有调用方拿到的 `Settings` 都来自 `load_from`（已 normalized）；`write_to` 里的跨字段校验则必须自己先 `.normalized()`（§5.2 注意事项第 2 条）。

`grace_secs` / `remote_secs` 两个原子存的就是这个函数的返回值乘 60。

主循环侧每拍：

```rust
let settings = load_from(&settings_path);            // 已经在这里读，不新增读盘
let (grace_min, remote_min) = power_windows_minutes(&settings);
let (grace_secs, remote_secs) = (grace_min * 60, remote_min * 60);

power_guard.set_enabled(prevent_idle_sleep_enabled(&settings));
power_guard.set_windows(grace_secs, remote_secs);     // 一次写入两个原子

// 远端窗口用秒数构造 Duration，与原子里的单位保持同一口径。
let remote_window = Duration::from_secs(remote_secs);
let remote = remote_touch_busy(last_remote_touch, &power_clock, remote_window);
```

**同一拍内只读一次盘**：`settings` 这一个变量同时喂给开关、两个窗口与远端判定，四处不会读到不同版本。这是本设计「单一读取点」原则的具体落点。

**为什么用原子传值而不让线程自己读 `settings.json`**：主循环那一拍已经 `load_from` 过一次（`power_ticker` 分支），线程重复读同一文件既浪费又可能与主循环读到不同版本。**单一读取点**：主循环读盘 → 写原子 → 线程消费。这样「设置页保存后多久生效」只有一个可推理的链路。

`Supervisor::set_windows(grace, slack)` 内部转给 `Policy::set_windows`。

**`remote_touch_busy` 的签名要加第三个参数**，而 `power_ticker` 里它被调用时必须在同一个分支内先算出 `remote_window`（来自同一次 `load_from` 的结果），不能另读一次盘。

### 4.5 `REMOTE_TOUCH_WINDOW` 参数化

`power_busy::remote_touch_busy(last_remote_touch, clock)` 加第三个参数 `window: Duration`。调用点只有主循环一处（`server.rs` 的 `power_ticker` 分支）。

`REMOTE_TOUCH_WINDOW` 常量保留为默认值，供测试引用。**设置里的默认值走另一对常量**（`POWER_REMOTE_TOUCH_MINUTES_DEFAULT`，见 §3.4），二者数值相同（300 秒 / 5 分钟）但单位不同（`Duration` vs 分钟数）——测试要钉住这一点，避免将来改一处忘另一处。

---

## 5. 边界、校验与降级

### 5.1 单字段校验（后端强制）

| 字段 | 范围 | 拒绝理由示例 |
|---|---|---|
| `power.idle_grace_minutes` | 1–120 | `must be at least 1` / `must be at most 120` |
| `power.remote_touch_minutes` | 1–60 | 同上 |

下限 1 的理由：0 会让「满足条件 → 立刻放弃」变成逐拍抖动，宽限失去意义。
上限的理由：防止「设成 10000 分钟」把机器变成事实上的永不睡。

校验由现有 `validate_value` 按 `FieldSpec.min` / `max` 自动完成，不需要新代码。

### 5.2 跨字段校验：远端窗口不得大于宽限窗口

先把两个窗口各自在管什么说准（`policy.rs` 的 `keeps_awake` 与 `remote_touch_busy` 已核实）：

```rust
keeps_awake = online && (on_ac || busy)   // policy.rs:35-37
busy = local || remote || daemon          // 主循环三路取或
remote = 最近一次远端交互距今 < remote_window
grace = 最近一次 keeps_awake 为真之后，还能再扛多久
```

`busy` 只在一个象限里起作用：

| 有网 | 插电 | `busy` 是否影响 `keeps_awake` |
|---|---|---|
| 是 | 是 | **不影响**（`on_ac` 已经让它为真） |
| 是 | 否（电池） | **影响** |
| 否 | 是/否 | 不影响（`online` 为假，整体为假） |

所以 `remote_touch_minutes` 唯一有意义的场景是**「有网 + 电池」**：手机连着、机器没插电时，靠「最近 5 分钟内有远端交互」继续算作有活。

**倒挂（远端窗口 > 宽限）的实际后果**：在电池场景下，远端交互消失后 `busy` 变假，`grace` 从那一刻起算。若远端窗口比宽限长，那么在「远端窗口还没过、但宽限已经过」的那段时间里 `busy` 仍为真——**它其实仍然起作用，而不是「完全失效」**。

但这仍是一个应当拒绝的配置，理由换成了更准确的两条：

1. **它让「宽限」在电池场景下失去意义**。用户以为「没活 15 分钟就放睡」，实际会因为远端窗口 30 分钟而一直算作有活，机器要扛满 30 分钟。两个窗口互相掩盖，用户调哪个都看不出效果。
2. **它没有一致的语义**。宽限的语义是「活停了之后再等多久」；远端窗口的语义是「多久之内算还有活」。后者长于前者，等于说「我认定的活，比我自己允许的等待还长」——配置自相矛盾。

**所以仍然拒绝，但理由不是「不起作用」，而是「语义自相矛盾并让宽限失效」。** 这一点必须在错误文案里体现，否则用户会看到「must not exceed」却想不明白为什么。

**处理方式：写入时拒绝**，而不是运行时静默取小值。理由：这是配置错误，用户应当场知道；静默取小值会让「我明明设了 30 分钟」变成无法解释的行为。

实现位置：`validate_value` 是逐字段无状态的，做不了跨字段判断，只能放在 `write_to` 里。**现有 `write_to` 已是「校验失败零写入」语义，可直接复用这个形状。**

**具体接法**（`write_to` 现有形状：解析 patch → 逐项 `validate_value` → 读原文件成 object → 逐个 `apply_op` → 删空壳组 → 序列化写回）：
跨字段检查必须插在 `apply_op` 之后、`write_atomically` 之前，因为「新值与旧值比较」只有在合并后的完整对象上才成立。此时把合并后的 object 反序列化成 `Settings`，读两个字段的**生效值**，比较后决定是否 `return Err`。**放在这里的好处：那一刻还没有写盘，天然满足零写入。**

必须注意的三点：

1. **必须用「合并后的完整对象」判断**，不能只比较 patch 内部。用户可能只 patch 一个键（例如只把宽限从 60 改成 5），此时远端窗口的生效值来自**文件里的旧值**。
2. **必须对合并结果跑一遍 `normalized()` 再取生效值**（`settings.normalized()` 是消费式返回，写 `let merged = merged.normalized();`；随后用 `power_windows_minutes(&merged)` 取两个生效值，见 §4.4.1）。`validate_value` 只校验 patch 里的新值；文件里的旧值可能是手改的越界值（例如 `0`），`from_value` 不会自动钳制。不归一化就会拿 `Some(0)` 参与比较，得出无意义的结论。`normalized()` 是同模块的私有方法（`fn normalized(mut self) -> Settings`，消费式返回），`write_to` 直接调用即可。
3. **`from_value` 失败要转成 `Corrupt`**。合并后的对象可能是 `{"agent": 5}` 这种结构性坏（`write_to` 现在只校验「顶层是对象」，不校验内层），此时应 `SettingsError::Corrupt` 拒绝写入，而不是 `unwrap` 或静默跳过校验。这与现有「文件存在却读不出来 → 拒绝覆盖」的守卫同向。

**`None` 的语义**：两个字段任一为 `None` 时视为默认值（宽限 15、远端 5），比较 `5 > 15` 为假，放行。所以「只设宽限 5、远端用默认 5」这种组合是合法的（相等，见下）。

错误形式沿用 `SettingsError::InvalidSetting { key, reason }`，`key` 取 `power.remote_touch_minutes`，`reason` 写清「must not exceed power.idle_grace_minutes」。

**边界取舍**：允许**相等**（5 分钟远端窗口 + 5 分钟宽限），只有严格大于才拒。相等时远端窗口在宽限到期的同一刻失效，语义是自洽的。

### 5.3 降级

| 情形 | 行为 |
|---|---|
| 字段缺失 / `null` / 空串 | 回落默认（15 / 5），不视为错误 |
| 非数字（`"abc"`） | `de_optional_u64` 吸收成 `None`，回落默认 |
| 越界（`0` / `200`） | `normalized()` 的 `clamp!` 清成 `None`，回落默认，记 `tracing::warn` |
| 跨字段倒挂（手改文件造成） | 回落默认（15 / 5），记 `tracing::warn`，**不拒绝启动** |
| 整份 JSON 损坏 | `load_from` 返回默认值，即「默认开 + 15 + 5」 |

**`clamp!` 必须逐字段显式加**。已核实 `normalized()` 里的越界回落是逐字段列举的（`settings.rs:473-491` 共 7 处 `clamp!`，只覆盖 `Number` 字段；`Select` / `Text` 走各自的 `matches!` 校验）。新增两个数值字段必须自己写：

```rust
clamp!("power.idle_grace_minutes", self.power.idle_grace_minutes);
clamp!("power.remote_touch_minutes", self.power.remote_touch_minutes);
```

**漏了不会报错**：`clamp!` 未覆盖的字段会带着越界值一路进策略层，`Duration::from_secs(0)` 或 200 分钟都能编译能运行，只是行为错。这正是 §7.3 第 5 条测试存在的理由。

**跨字段倒挂的回落是独立的一步**，`clamp!` 管不到：即使两个值各自都在范围内（例如宽限 5 + 远端 30），倒挂仍然成立。这一步要在 `normalized()` 里单独写：

```rust
if let (Some(remote), Some(grace)) = (self.power.remote_touch_minutes, self.power.idle_grace_minutes) {
    if remote > grace {
        tracing::warn!(.., "power.remote_touch_minutes exceeds power.idle_grace_minutes; falling back to the defaults");
        self.power.remote_touch_minutes = None;
        self.power.idle_grace_minutes = None;
    }
}
```

**两个都清空（而不是只清远端）** 的理由：只清远端的活儿会让「宽限 5」单独生效，用户看到的仍是自己设置的 5 分钟——但文件里那对矛盾值会一直在，每次启动都 warn 一次、每次都只修一半。两个一起回默认，行为单调且可解释。

方向是一致的：**任何坏配置都倒向「按默认继续保活」**。原始故障是「夜里跑着跑着睡了」，让配置解析失败关掉保活会重现它。

### 5.4 改动生效时机

改设置 → 下一次 `power_ticker`（≤10 秒）读到 → 写原子 → 线程下一拍（≤10 秒）生效。**最坏 20 秒**。

### 5.5 缩短宽限时，已在跑的那次 hold 立即按新窗口重算

**精确语义**（已用逐拍推演验证）：`decide` 每拍都用**当前** `self.grace` 现算 `now.mono - last_ok < grace`，所以新窗口在下一拍就生效，不需要额外代码。具体到三种情形：

| 情形 | 结果 |
|---|---|
| 条件**已消失**、旧窗口还没到期、用户缩窄窗口到「已超出」 | 下一拍立即 idle（推演：`last_ok=0`、`now=1201`、新宽限 300 → `1201 < 300` 为假 → Idle） |
| 条件**仍然成立**（`keeps_awake` 为真） | 继续 hold——每拍都会把 `last_ok` 刷成当前时刻，与窗口大小无关。这是对的：有活就该扛 |
| 用户**调长**窗口（当前仍在 hold） | `last_ok` 是绝对时刻，窗口变大后判据更容易成立，自动延长 |

**必须有一条测试钉住第一行**，否则将来有人「优化」成缓存窗口值（或把 `grace` 在 `with_windows` 时快照进 `Deadline`）就会破坏它，而且破坏后只表现为「设置改了不起作用」，很难发现。

**不要**把它写成「缩短宽限后一定立刻释放」——条件仍在跑时不会释放，也不该释放。

#### 一个必须处理的副作用：调长窗口会让「已释放」的 hold 复活

`decide` 在返回 `Idle` 时只清 `hold_since`、**不清 `last_ok`**（已核实 `policy.rs:88-93`）。于是：

```rust
// 逐拍推演（已用脚本验证）
t=0     busy=True   grace=300s   → Hold   // last_ok = 0
t=400   busy=False  grace=300s   → Idle   // 400 - 0 >= 300，但 last_ok 仍 = 0
// 用户把宽限从 5 分钟改成 60 分钟
t=401   busy=False  grace=3600s  → Hold   // 401 - 0 < 3600，复活
```

**结论：要修，在 `Idle` 分支把 `last_ok` 一并清成 `None`。**

先排除了「不修」这条路，因为最初给它的理由是错的。逐拍推演（含两个分支对照）结果：

| 场景 | 不清 `last_ok` | 清 `last_ok` |
|---|---|---|
| `releasing_then_re_holding_after_the_grace_expires`（既有测试） | Hold / Idle / Hold | Hold / Idle / **Hold** |
| 调长窗口（上面那条时序） | **Hold（复活）** | **Idle（不复活）** |

也就是说：**清 `last_ok` 不影响任何既有行为**——因为重新有活时 `keeps_awake` 那行会立刻把 `last_ok` 刷成当前时刻，与它之前是 `None` 还是旧值无关。唯一被改变的正是「没有活、只是把窗口调长」这一条，而它恰恰是我们要消除的。

**为什么复活是错的**：用户在**已经放过一次睡眠之后**调长窗口，期望是「以后多扛一会儿」，而不是「现在就重新占住睡眠」。而且这个复活没有任何用户可见的提示——`pmset -g assertions` 里会突然重新出现一条断言，用户会以为是设置没生效、或者机器自己乱占了电源。配置变更应当只影响「下一次判定」，不该追溯性地改变「已经作出的判定」。

**改动位置**（`policy.rs` 的 `decide` 尾部）：

```rust
match (decision, self.hold_since) {
    (Decision::Hold, None) => self.hold_since = Some(now),
    (Decision::Idle, _) => { self.hold_since = None; self.last_ok = None; }
    (Decision::Hold, Some(_)) => {}
}
```

**必须写一条测试钉住它**：`lengthening_the_window_does_not_revive_a_released_hold` —— 按上面的时序断言 `t=401` 是 `Idle`。没有这条测试，将来有人删掉那行 `self.last_ok = None;`（它看起来像冗余的清理）时不会有任何测试报警。

**注意不要顺手改看门狗分支**：它已经清了 `last_ok`（`policy.rs:74-75`），那是刻意的——时钟故障时必须彻底重置，不能留着旧 `last_ok` 让下一拍又 Hold。

---

## 6. 迁移：`agent.prevent_idle_sleep` → `power.prevent_idle_sleep`

### 6.1 规则

**迁移必须做在原始 JSON 上，不能做在反序列化后的结构体上。** 这是本设计最容易写错的一步，理由见下。

`load_from` 的真实形状（已核实，`settings.rs:404`）：

```rust
let text = 读文件;                      // NotFound / 读失败 → 返回 default
let mut value: serde_json::Value = 解析; // 失败 → 返回 default
if !value.is_object() { 返回 default }

migrate_legacy_keys(&mut value);         // ← 新增，插在这里

let settings = match from_value::<Settings>(value) {   // 失败 → 返回 default
    Ok(settings) => settings,
    Err(error) => return default,
};
settings.normalized()                    // 真实代码就是这样收尾的（尾表达式，不是链式）
```

**注意 `value` 要改成 `mut`**：迁移就地修改它。真实代码里它目前是 `let value`，加迁移后必须 `let mut value`。

**在 `if !value.is_object()` 之后、`from_value` 之前插入迁移**：

```
若 value["agent"]["prevent_idle_sleep"] 存在
   且 value["power"]["prevent_idle_sleep"] 不存在
    → value["power"]["prevent_idle_sleep"] = 该值
    → 从 value["agent"] 删掉该键（agent 变空则删掉整个 agent 键）
```

**为什么不能等反序列化之后再搬**：一旦把 `AgentSettings.prevent_idle_sleep` 字段删掉（§6.3 要求删，否则设置页会出现两个开关的字段定义），`serde` 就会**静默忽略**文件里的 `agent.prevent_idle_sleep`——`Settings` 上没有任何 `deny_unknown_fields`（已核实全仓无此属性），未知键直接丢弃，不报错。于是反序列化之后旧值**已经不存在了**，无处可搬。

换言之：迁移的输入只存在于 `serde_json::Value` 阶段。只有两种可行做法：

- **(A) 在 `Value` 上搬键**（本设计采用）：结构体里彻底删掉旧字段，干净。
- **(B) 保留旧字段**（`AgentSettings.prevent_idle_sleep` 留作 legacy、不进 `FIELDS`），反序列化后搬。

**选 A**。B 会让旧字段长期活在结构体里，每次 `Settings` 相等性比较、每条测试夹具都要考虑它；而它的存在理由只是「迁移的中间载体」——这个载体 `Value` 阶段就能提供。

三条约束：

1. **只在新键缺失时迁移**。否则用户显式设了 `power.prevent_idle_sleep = "true"`，每次启动都被旧的 `"false"` 覆盖。
2. **迁移后不写盘**（理由见 §6.2）。
3. **幂等**：迁移把旧键从 `agent` 里删掉了，同一份 `Value` 上再跑一遍是空操作。这一点对「同一进程内多次 `load_from`」也成立（每拍都重新读文件、重新迁移，结果一致）。

**迁移代码应是一个独立的小函数**（例如 `fn migrate_legacy_keys(value: &mut serde_json::Value)`），不从 `load_from` 里内联展开。它纯操作 JSON、零 I/O、无副作用，可以直接单测，不必绕着 `load_from` 造临时文件。

### 6.2 迁移要不要落盘

- **不落盘（只在内存里搬）**：每次启动都搬一次，`settings.json` 里旧键永远存在。用户若用旧版 app 打开，仍能读到旧键——回滚兼容性最好。
- **落盘（启动时重写文件）**：文件立刻干净，但旧版 app 回滚后读不到自己的键，开关回落默认（开）。

**选不落盘。** 理由：迁移逻辑本身是幂等的、成本可忽略（一次字段搬运），而「不擅自改用户文件」是更强的原则。

**必须澄清一个容易想错的地方**：旧键**不会**因为「用户在设置页保存了别的字段」而自动消失。`write_to` 的形状是「读原对象 → 只对 patch 里出现过的键调 `apply_op` → 序列化写回」，未出现在 patch 里的键（包括残留的 `agent.prevent_idle_sleep`）会原样保留。只有对**该键本身**写入 `null` 才会删掉它，而它已不在 `FIELDS` 里，设置页永远不会对它发 `null`。

所以「旧键残留」是**长期存在**的，不是暂态。这不影响正确性（`load_from` 每次都做一遍幂等迁移），但两件事必须写进测试：

1. `write_to` 保存其它字段后，文件里的 `agent.prevent_idle_sleep` **仍然在**（这是预期行为，不是 bug）；
2. 该旧键在文件里存在时，`load_from` 的结果始终与迁移后一致（幂等，见 §7.3 第 3 条）。

如果将来希望文件真的干净，唯一办法是启动时主动落盘重写——本设计明确不做（理由见上）。

### 6.3 旧键不进 `FIELDS`，且必须改的既有代码

迁移完成后 `agent.prevent_idle_sleep` 不再是可编辑字段，从 `FIELDS` 中移除。否则设置页会同时显示两个开关，用户不知道哪个生效。

**键名迁移的连带改动清单**（已核实的真实引用点，`grep -rn prevent_idle_sleep`）：

| 位置 | 现状 | 改法 |
|---|---|---|
| `settings.rs:102` `AgentSettings.prevent_idle_sleep` | `Option<String>` 字段 | **删除**，字段移到新的 `PowerSettings` |
| `settings.rs:174` `PREVENT_IDLE_SLEEP_KEY` | `= "agent.prevent_idle_sleep"` | 改成 `"power.prevent_idle_sleep"` |
| `settings.rs:178` `prevent_idle_sleep_enabled` | 读 `settings.agent.prevent_idle_sleep` | 改读 `settings.power.prevent_idle_sleep` |
| `settings.rs:530` `normalized()` 里的枚举校验 | 校验并清空 `self.agent.…` | 改为 `self.power.…`，warn 文案同步改键名 |
| `settings.rs:246` `FIELDS` 里的 `FieldSpec` | `group: "agent"` | 改 `group: "power"` |
| `settings_rpc.rs:46` / `crates/yi-agent-web/src/api.rs` `match` | 用 `PREVENT_IDLE_SLEEP_KEY` 分支读 `current.agent.…` | 改读 `current.power.…` |
| `server.rs:3271` / `server.rs:3871` | 调 `prevent_idle_sleep_enabled(&settings)` | **调用点不用改**（函数名与签名不变，只是内部换字段） |

**这 6 条既有测试会因字段移动而编译失败**，必须一起改（都是机械替换 `settings.agent.prevent_idle_sleep` → `settings.power.prevent_idle_sleep`）：

- `prevent_idle_sleep_defaults_to_enabled`（`settings.rs:1405`）
- `prevent_idle_sleep_honours_false`（`:1411`）
- `an_unknown_prevent_idle_sleep_falls_back_to_enabled`（`:1419`）
- `an_empty_prevent_idle_sleep_is_unset`（`:1430`）
- `a_corrupt_settings_file_still_enables_prevention`（`:1445`）
- `prevent_idle_sleep_is_case_insensitive`（`:1457`）

后四条造的是 `{"agent":{"prevent_idle_sleep":...}}` 形式的**真实文件**。迁移落地后它们的语义会变：这些文件现在会先被 §6.1 的迁移搬到 `power`，再被 `normalized()` 校验。**测试要按新语义重写，不是简单改字段名**：

- `an_unknown_prevent_idle_sleep_falls_back_to_enabled`：文件里写 `{"agent":{"prevent_idle_sleep":"yes"}}`，迁移后 `power` 侧是 `"yes"`，被 `normalized()` 清成 `None`，断言 `settings.power.prevent_idle_sleep == None` 且 `prevent_idle_sleep_enabled` 为真。
- 其余三条同理：断言点从 `agent` 侧换到 `power` 侧，**但输入文件的键名要保留旧的 `agent.`**——那正是迁移的输入端，改成 `power.` 就测不到迁移了。

**建议保留其中至少两条用旧键名做输入**，这样它们同时充当迁移的回归测试。

#### `PREVENT_IDLE_SLEEP_KEY` 常量怎么办

它被 4 处引用（`settings.rs:174` 定义、`:246` 进 `FIELDS`、`web/src/api.rs:87`、`settings_rpc.rs:53`）。**直接把它的值改成 `"power.prevent_idle_sleep"` 即可**，四处自动跟随，不必改名或删除。

但要清楚它的新语义：**它从此只表示「新键」**，迁移函数里的旧键字符串 `"agent.prevent_idle_sleep"` 必须写成**字面量**，不能引用这个常量——一旦将来有人再改常量值，迁移就会跟着漂移、静默失效。

同理，`AgentSettings` 里删掉字段后，旧键的字符串只应出现在两处：`migrate_legacy_keys` 内部，以及 §6.3 那 6 条测试的文件内容里。**全仓除此之外不应再出现 `"agent.prevent_idle_sleep"`**，这条可以用 `grep -rn 'agent\.prevent_idle_sleep' crates/` 自查（预期只剩迁移函数与测试）。

#### 旧键的 `label` / `description` 怎么处理

`FieldSpec` 是 `Copy` 结构体、`FIELDS` 是 `static`，不能有条件地拼接字符串。所以**不改 `label`，只改 `description`**（去掉写死的「15 分钟」，见 §3.4）。`label`（「干活期间防止休眠」）与语义无关，原样搬到 `power` 组即可。

---

## 7. 测试

### 7.1 `yi-agent-power`（`policy`）

- **已有 16 条零改动**（走 `Policy::new()` 默认路径；`GRACE` 常量保留为别名，`policy.rs:292`、`:329` 两处 `deadline` 计算不必动）。
- 新增 6 条：

1. `a_custom_grace_releases_at_its_own_boundary` —— `with_windows(300s, 300s)` 下，用**正常时钟**（`at()`）走到第 299 秒 hold、第 300 秒 idle。这条钉住「宽限真的被参数化」，而不是还在读常量。
2. `the_watchdog_scales_with_the_grace_window` —— 用 `frozen_mono()` 构造「mono 冻结、墙钟照走」，`with_windows(3600s, 1200s)`：墙钟走到 4800 秒仍 hold，4801 秒才 idle。**必须用 `frozen_mono`**：用 `at()` 时 `mono_grew` 恒为真，看门狗分支根本不执行，测试会假通过。
3. `shrinking_the_grace_releases_an_in_flight_hold_immediately` —— 构造「条件已消失但仍在宽限内持有」，再缩窄窗口：
   ```
   with_windows(3600s, 1200s)
   decide(keeps_awake=true,  at(0))    → Hold   // 刷新 last_ok = 0
   decide(keeps_awake=false, at(1200)) → Hold   // 空跑 20 分钟，仍在 3600 宽限内
   set_windows(300s, 300s)
   decide(keeps_awake=false, at(1201)) → Idle   // 1201 - 0 = 1201 >= 300，立即释放
   ```
   **`at(1200)` 那步必须传 `keeps_awake=false`**：若一直传 true，每拍都会刷新 `last_ok`，缩窄窗口后仍会 Hold（那是正确行为，但测不到本条的意图）。这条钉住「窗口缩短立即生效、不被缓存」。附带建议再加一条对照断言：`keeps_awake` 仍为 true 时缩窄窗口后**应继续 Hold**，防止有人为了「让缩窄立刻生效」而在忙时错误释放。
4. `the_grace_window_accepts_its_documented_limits` —— 1 分钟与 120 分钟两端各一条断言，确认极值下判定仍自洽（1 分钟：60 秒 hold、61 秒 idle）。
5. `default_windows_match_the_historical_constants` —— `Policy::new()` 的宽限是 15 分钟、slack 是 5 分钟；同时断言 `GRACE == DEFAULT_GRACE` 别名没漂移。这条是给「手写 `Default` 后忘了转调 `new()`」兜底的。**断言要写成「第 899 秒仍 Hold、第 900 秒才 Idle」**：若 `Default` 把宽限置 0，那么 `at(0)` busy 后 `at(0)` 同拍就会 idle（`0 - 0 < 0` 为假），测试立刻红。不要写成「第 1 秒就 idle」——`last_ok` 初始是 `None`，必须先有一拍 busy 才可能进入 hold 判定，措辞不准确容易写出测不到东西的断言。

6. `lengthening_the_window_does_not_revive_a_released_hold` —— §5.5 末段那条时序：
   ```
   with_windows(300s, 300s)
   decide(keeps_awake=true,  at(0))   → Hold
   decide(keeps_awake=false, at(400)) → Idle     // 400 >= 300
   set_windows(3600s, 1200s)
   decide(keeps_awake=false, at(401)) → Idle     // 401 - 0 < 3600，但要断言仍是 Idle
   ```
   这条钉住 `Idle` 分支里 `self.last_ok = None;` 那行。**没有它，删掉那行不会有任何测试报警**，而行为会变成「调长窗口就复活占住睡眠」。

**关于第 2 条的一个重要澄清**：看门狗在正常路径上**永远不会**与宽限竞争（`mono_grew` 为真就跳过）。所以「slack 缩放」只在时钟异常路径有意义，这条测试测的正是那条路径。不要把它的断言写成「宽限 60 分钟时 20 分钟就释放」——那是错的，正确的是 80 分钟（60 + 20）。

### 7.2 `power_busy`

加第三参数 `window: Duration` 后，**4 处测试调用点要补参数**（已核实位置）：

- `power_busy.rs:391` / `:395`（`remote_touch_expires_at_the_window_boundary`）
- `power_busy.rs:405`（`no_remote_touch_ever_is_not_busy`）
- `power_busy.rs:424`（`a_local_stdio_client_does_not_keep_the_machine_awake`）
- `power_busy.rs:397` 的 `FixedClock(REMOTE_TOUCH_WINDOW)` 是**时钟读数**，不是窗口参数，不要误改

前三处传 `REMOTE_TOUCH_WINDOW` 即可保持原语义（机械改动，行为不变）。

新增 1 条：`a_custom_remote_window_expires_at_its_own_boundary` —— 窗口 120 秒时第 119 秒真、第 120 秒假。这条钉住「窗口真的被参数化」，而不是还在读常量。

生产调用点只有一处：`server.rs:3878`。

### 7.3 `settings`

9 条：

1. `power_windows_default_to_fifteen_and_five` —— 空 `Settings::default()` 的生效值为 15 / 5。
2. `migrate_the_old_agent_key_into_the_power_group` —— **造真实旧格式文件**，`load_from` 后断言 `power.prevent_idle_sleep == Some("false")`。
3. `migrate_twice_is_a_no_op` —— 同一份文件 `load_from` 两次，结果相等（§6.1 约束 3）。
4. `migrate_keeps_an_explicit_power_key` —— 两个键都在且冲突时，`power` 的值生效；且此时**旧键不应被覆盖**成 `power` 的值。
5. `power_windows_out_of_range_fall_back_to_defaults` —— 文件里写 `{"power":{"idle_grace_minutes":0}}` 与 `{"power":{"remote_touch_minutes":200}}`，各自 `load_from` 后生效值均为默认（§5.3 的 `clamp!`）。
6. `power_windows_inverted_pair_falls_back_to_both_defaults` —— 文件里写 `{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}`（两值各自合法但倒挂），`load_from` 后**两个**生效值都是默认（§5.3 末段）。
7. `power_windows_reject_a_remote_window_longer_than_the_grace` —— `write_to` 返回 `InvalidSetting`，且**读回的原始字节与写入前逐字节相同**。**不要断言 mtime**：同一秒内的两次写入 mtime 可能一样，APFS 的粗粒度时间戳会让这条断言假通过。字节比对才是「零写入」的真正判据。
8. `migrate_legacy_key_stays_on_disk_after_other_writes` —— 造带 `agent.prevent_idle_sleep` 的文件，`write_to` 改 `power.idle_grace_minutes`，再读文件原文断言旧键**仍在**（§6.2 澄清的预期行为）。
9. `migrate_value_survives_a_fresh_write` —— 旧格式文件（`agent` 侧为 `"false"`）→ `load_from` 得 A → 用 `serde_json::to_value(&A)` **整份写回**同一路径（这会产出新格式、无旧键） → 再 `load_from` 得 B → 断言 `A == B`。这条钉住「迁移结果可重放」：若迁移只在内存搬、而值本身落不进新键，第二次读就会退回默认（开），A 与 B 不等会立刻暴露。

**迁移测试必须造真实文件**：只有 `load_from` 走完整「读 → 解析 → 迁移 → 反序列化 → normalized」路径才能验证迁移真的接在了正确位置；纯构造 `Settings` 结构体测不到它。

**第 2 条要断言 `agent` 侧为空**：迁移的「删旧键」是 §6.1 明确要求的（否则同一份 `Value` 上重复迁移的幂等性靠的是「新键已存在」这个间接条件，而不是「旧键已删」）。但注意——`load_from` 返回的 `Settings` 里已经没有 `agent.prevent_idle_sleep` 字段了（§6.3 要删），所以这条断言只能落第 9 条的往返形式上，或者直接单测 `migrate_legacy_keys` 那个纯 JSON 函数。**推荐后者**：给 `migrate_legacy_keys` 单独写 3 条纯 JSON 单测（搬键 / 新键优先 / 幂等），比绕 `load_from` 断言「结构体里没有某字段」清晰得多。

### 7.4 `settings_rpc` / `web::api`

两处 `match` 各加三个分支。**两边的守卫强度不一样，已核实**：

- **`settings_rpc` 有实测守卫**：`every_field_in_fields_can_be_read_back`（`settings_rpc.rs:119`）遍历 `FIELDS` 断言每键都能读回非 `null`。漏加分支会红。
- **`web::api` 没有测试守卫**。`crates/yi-agent-web/src/api.rs` 里**没有任何 `#[test]`**（已核实：`grep -n "#\[cfg(test)\]" api.rs` 无输出）。`get_config` 遍历 `FIELDS` 生成响应，但 `field_value` 的 `_ => Value::Null` 兜底会让漏改的分支**静默返回 `null`**，接口照样 200、页面照样渲染，只是那两个字段永远显示「未设置」。

因此 web 侧的三个分支**必须靠人眼保证**，不能指望测试。做法：改完后用下面的命令自查 `field_value` 与 `current_value` 的 match 分支数是否一致：

```bash
cd yi-agent-rs
grep -c '=> opt(' crates/yi-agent-web/src/api.rs
grep -c '=> option_to_json(' crates/yi-agent-app-server/src/settings_rpc.rs
```

两个数字必须相等（都等于 `FIELDS.len()`）。这是本设计里唯一「测试兜不住」的点，值得单独写进验收清单。

**建议顺手补一条 web 侧测试**（不在本设计范围内，但成本极低）：对 `FIELDS` 里每个 `Number` 字段，构造一个非 `None` 的 `Settings`，断言 `field_value` 返回的不是 `Value::Null`。缺少这条，`field_value` 就会一直是「漏改不报错」的区域。

### 7.5 前端

- `GROUP_LABELS` 加 `power: "电源与休眠"`、`GROUP_HINTS` 加对应一句、`GROUP_ORDER` 末尾加 `"power"`（§0.1）。
- `SettingsRuntimeTab.test.tsx:188` 的精确 `toEqual` 断言改成覆盖断言（§0.2），否则加组即红。
- `VALUE_LABELS` 加 `"power.prevent_idle_sleep": { true: "开", false: "关" }`（§0.1）。`labelFor` 本身不用改——它是纯查表函数，表里有了就生效。
- **必须有一条测试**：`labelFor("power.prevent_idle_sleep", "false")` 返回 `"关"`。否则漏加映射不会报错，只会让下拉框显示英文 `true`/`false`，与相邻的「终端通知」组风格不一致。

**两个数值字段的单位已经在标签里**（`条件消失后再保活多久（分钟）`、`远端交互有效窗口（分钟）`），不要依赖 `labelFor` 加后缀：`Number` 字段走的是 `<input type="number">`，`labelFor` 只用于 `Select` 的选项文案（`SettingsRuntimeTab.tsx:204`、`:208`）与状态行（`:238`、`:239`），给它塞单位既不走那条路径、也会让「已设为 15」变成「已设为 15 分钟」以外的意外形态。

---

## 8. 验收

### 8.1 自动化

`cargo test -p <crate> <filter>` 的 `<filter>` 是**测试名子串匹配**，所以过滤词必须真的出现在测试名里。下面的过滤词已按本设计新增/改动的测试名对过：

```bash
cd yi-agent-rs

# policy：既有 16 条 + 新增 6 条（全部在 policy 模块内）
cargo test -p yi-agent-power policy

# power_busy：窗口参数化的 4 处改动 + 新增 1 条
cargo test -p yi-agent-app-server power_busy

# settings：§7.3 的 9 条 + migrate_legacy_keys 的 3 条纯 JSON 单测（§7.3 末段）
cargo test -p yi-agent-runtime power_windows
cargo test -p yi-agent-runtime migrate
cargo test -p yi-agent-runtime default_display

# 读回守卫（漏加 match 分支会红）
cargo test -p yi-agent-app-server every_field_in_fields_can_be_read_back

cd ../desktop
npx vitest run src/components/SettingsRuntimeTab.test.tsx
npx tsc --noEmit
```

**四个容易踩的过滤坑**：

- §7.3 的 9 条测试名全部以 `power_windows_` 或 `migrate_` 开头，加 `migrate_legacy_keys` 的 3 条纯 JSON 单测也以 `migrate_` 开头，因此 `cargo test -p yi-agent-runtime power_windows` 与 `cargo test -p yi-agent-runtime migrate` 两条命令即可覆盖全部 12 条。
- **测试名必须保持这个前缀**。cargo 对 0 匹配的过滤词照样 exit 0、不报错，所以一条被遗漏的测试在 CI 里完全静默——这比测试失败更危险。写 plan 时要把「测试名带前缀」当成硬约束写进每个 Step。
- `cargo test -p yi-agent-web get_config`：**`yi-agent-web` 里没有任何测试**（§7.4 已核实），这条命令会 0 匹配通过。**不要把它当作「web 侧也验过了」**。
- `npm test -- SettingsRuntimeTab` 能用，但 `npm test` 是 `vitest run`，追加参数走的是位置过滤；用 `npx vitest run <path>` 更明确，也与 `docs/project-management/desktop.md` 里既有的判据写法一致。

### 8.1.1 提交前必跑

```bash
cd yi-agent-rs && cargo fmt --all && just ci   # 12 道门禁（fmt/lint/test/build/bindings/secret…）
```

这是项目规定的唯一 CI 入口（`.github` 与 GitLab 都只跑它）。`just ci` 里的 `lint` 是 `clippy --all-targets`，会连带检查本设计新增的测试代码。

### 8.2 真机

1. **生效时延**：设置页把宽限从 15 改成 1 并保存 → `pmset -g assertions` 在 20 秒内反映新窗口语义。
2. **立即释放**：在断言持有期间把宽限从 15 改到 1，平复后 1 分钟内断言应消失。
3. **迁移不丢设置**：手工把 `settings.json` 写成旧格式（`agent.prevent_idle_sleep: "false"`），启动后设置页「干活期间防止休眠」应显示「关」。
4. **跨字段拒绝**：把远端窗口设成 30、宽限设成 5 → 保存被拒并在界面上看到原因。

---

## 9. 风险

- **迁移写错会丢用户设置**。缓解：三条约束（§6.1）+ 三组测试（§7.3 第 2/3/4 条）。
- **`Policy` 参数化会碰已有 16 条测试**。缓解：保留 `Policy::new()` 默认路径（既有 16 条走它，源码零改动）；`GRACE` 常量保留为别名，`policy.rs:292` / `:329` 两处 `deadline` 计算不动。
- **`last_ok` 在 Idle 时清空会改动既有语义**（§5.5 末段）。这是本设计唯一一处**主动改变既有行为**的地方，必须明确其影响面：已用脚本扫过全部 16 条测试，「Idle 之后再出现 Hold」的只有 `releasing_then_re_holding_after_the_grace_expires`（`policy.rs:180`），而它靠 `keeps_awake` 那行刷新 `last_ok`，与清不清无关——逐拍对照推演已确认结果不变。缓解：新增第 6 条测试钉住「调长窗口不复活」。
- **跨字段校验破坏了「字段独立」的简单性**。这是必要的复杂度：不校验就会产生「设了但不起作用」的静默配置。
- **前端精确断言**。`GROUP_ORDER` 那条 `toEqual` 在加组时必红。缓解：按 §0.2 改成覆盖断言，而不是每次往期望数组里追加一项。
- **本设计不构成对排版重做的回退**。排版重做已合并（§0），本设计只在它的新结构上做加法：两张表各加一行、`GROUP_ORDER` 末尾加一项，不动渲染骨架。
