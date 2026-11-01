# 干活期间防止系统空闲休眠

日期：2026-11-01
状态：已批准（方案：单一持有者 + 纯策略 crate + 尊重系统休眠意图）

## 问题

长任务跑到一半，机器睡了，活就断了。这是三条独立的真实故障，不是同一个问题：

1. **进程被冻结**。macOS 空闲睡眠会暂停全部用户态进程，正在跑的 turn 卡在原地。
2. **醒来不会自愈**。provider 流靠空闲 60s 判 Stalled（`yi-agent-core/src/agent.rs:155`）、
   连接/读超时 300s（`yi-agent-llm/src/openai/client.rs:126-148`）。睡眠远超这两个预算，
   醒来后这条 HTTP 流必然已死。而 `agent.rs:126-133` 明确写着：

   > Deliberately *not* retried: ... and `Stream` (a connection reset mid-stream is not an
   > idle transport hiccup).

   流中断归为 `Stream`，**不可重试**，于是这一轮直接 `Error` 收场，不会自己续跑。
3. **手机连不上**。这条链路是唯一已经对睡眠做过设计的部分：relay 每 30s ping
   （`yi-agent-relay/src/client.rs:32-33,104-120`，注释写着「应对电脑睡眠唤醒」），
   断线走指数退避（`backoff.rs:41-52`）。它能自愈，但前提是电脑侧进程还活着
   且系统已经醒来。

本机 `pmset -g custom` 实测正好解释了「干着干着就睡了」：

```
AC Power:      sleep 0        (永不睡)
Battery Power: sleep 1        (空闲 1 分钟就睡), displaysleep 5
```

插电其实不会睡；真正打断工作的是**用电池的时候**。这恰好对应需求里「有网且上电」
的分档，不是巧合。

全仓库目前没有任何防休眠实现：搜 `caffeinate` / `IOPMAssertion` / `powerSaveBlocker`
零命中，`docs/ideas.md` 也没登记过。

## 用户需求（原话拆解）与逐条落点

| 原话 | 落在设计里的哪一条 | 备注 |
|---|---|---|
| 有网且上电 → 保持工作，不睡 | `ok = online && on_ac` | 插电时不看 `busy` |
| 电池供电 → 只在确实有活在跑时保持工作 | `ok = online && busy` | 用户选了「C」档 |
| 手机要保持连着 | 唯一持有者就是承载 relay 的 app-server；断言在，链路的 30s ping 才有机会跑 | 见「为什么是单一持有者」 |
| 检测不到「有网 + 该保活」持续 15 分钟才允许睡 | 单向宽限窗口 `last_ok.map_or(false, \|t\| clock.since(t) < GRACE)` | 与字面读法的差异及理由已单独论证 |
| 系统没有休眠意图时不得主动休眠 | 只持 `PreventUserIdleSystemSleep` | 不拦合盖 / 手动睡眠 / 系统更新 |

「手机要保持连着」这一条需要补一句诚实的边界：本设计保的是**电脑不睡**，
而不是**链路永不中断**。睡眠期间链路必断（relay 的 30s ping 也停了）；唤醒后的
重连由既有的指数退避负责（`backoff.rs:41-52`）。所以这一条的实际承诺是
「只要机器本该醒着，链路就一直是通的」。

## 目标与非目标

目标（本 spec 覆盖）：

- 满足保活条件时持有系统断言，阻止**空闲自动睡眠**。
- 条件消失后经过 15 分钟宽限才释放断言。
- 插电有网时全程保活；电池供电时只在有活时保活。
- 有开关可以关掉整个行为。

非目标（明确不做）：

- **不实现唤醒后的续跑**。那是独立的一条腿，工作量比防睡大，验证方式也完全不同
  （需要真实的流中断注入）。单独出 spec，见本文末尾「后续」。
- 不阻止合盖、手动睡眠、系统更新重启。任何用户态断言都拦不住这些，这是设计前提
  而不是缺陷（见「断言怎么下（macOS）」）。
- 不改 `pmset` 配置、不替系统接管电源策略。机器上必须有 yi-agent 进程在跑才会保活。
- 不覆盖 Linux / Windows（编译成 no-op）。
- 不做「保活历史」「耗电统计」之类的观察面板。
- 不为孤儿 daemon 保活：宿主 app-server 已退出的情况下，用户的意图通常就是关机或
  合盖，此时不该由遗留进程替整机扛住不睡。

## 设计

### 一句话

**一个进程持有断言，一条纯函数决定持不持，15 分钟只作为一个方向上的宽限窗口。**

### 分层

```
yi-agent-power (新 crate)
├── policy      纯状态机,零 I/O,全部可单测
├── assertion   平台层:macOS 持 IOPMAssertion,其它 no-op
├── probe       电源来源 + 网络可达性,经 trait 注入以便测试
└── supervisor  10s tick,把上面三者串起来

yi-agent-runtime
└── settings 里加开关字段(走 settings::FIELDS,设置页自动渲染)

yi-agent-app-server
└── 主循环每拍: 采集 busy/电源/网络 → 喂给 policy → 迁移断言状态
```

**`supervisor` 不挂在 `yi-agent-runtime` 的 supervise 上**，尽管那个 crate 里确实有
一个 500ms 的监督循环（`runtime/src/supervise.rs:24`）。那个循环管的是**插件子进程**
的起停转发表，只有 daemon 进程跑它（`yi-agent/src/main.rs:768`），app-server 根本不
调用（`yi-agent-app-server` 从不引用 `supervise`）。把电源策略塞进去等于给一条与它
无关的循环加职责。

正确位置就是已经存在、每拍驱动所有会话的 app-server 主循环——`driver_ticker`
（`server.rs:3276`，3s）已经在那儿按拍采 `threads` 快照，10s 一次的电源拍挂在
它旁边最自然。

`yi-agent-app-server` 已依赖 `yi-agent-runtime`（`crates/yi-agent-app-server/Cargo.toml:20`），
所以开关读 `settings::FIELDS` 不需要新增依赖边；`yi-agent-power` 是新 crate，
app-server 直接依赖它。

### 为什么是单一持有者，而不是每个进程各持一个

为什么不采用「每个进程各持一个断言、各自判定」的自治拓扑：它有一个绕不开的
拓扑漏洞——desktop 拿不到「活干完了」这个事实。若 desktop 无条件持有断言，那么
「活干完 15 分钟后允许睡」**永远不成立**——desktop 一直持有，系统一直不睡。
要修就得让 desktop 知道 sidecar 的 busy 状态，那就回到跨进程上报，等于绕一圈
还是最复杂的方案。

所以：

- **持有者只有 `yi-agent app-server`**。它是所有「活」的唯一汇聚点：活跃 turn、
  驱动 tick 都在它的 `threads` 表里（`server.rs:6165-6190` 已经在按拍
  采同一份快照），子 agent / 看板 / 定时任务跑在项目 daemon 里、经既有 IPC 可查
  （见「接线点」的 (b)）。三种传输模式（`--listen stdio://`、`--listen ws://`、
  `--listen relay://` 或等价的 `--relay`）共用 `server.rs` 同一个主循环，
  一份实现覆盖全部。

  需要说明的边界：`yi-agent run`（headless，`main.rs:1704`）和 TUI
  （`run_tui_agent`，`main.rs:1977`）**不经过** app-server 主循环，是独立入口。
  本 spec 不为这两个入口保活——它们是交互式的，人就在键盘前面，不存在「人不在
  却被睡打断」的场景，而给它们各自加一套断言只会扩大 `unsafe` 面积。若以后需要，
  复用同一个 crate 即可。
- **desktop 不持有断言，也不显示状态**（本 spec 不做状态 UI，理由见「接线点 3」）。
  desktop 的唯一落点是设置页里那个开关——它由 `settings::FIELDS` 驱动渲染，
  不需要桌面端写任何专门代码。
- 多个 assert 本来由 macOS 原生聚合（`pmset -g assertions` 每个 pid 一行），所以
  不需要跨进程协调；但也正因为会做并集，多余的持有者只会让释放条件失效，不能
  靠「系统会合并」来掩盖这个问题。

代价是一个已知盲区，而且机制比"app 关掉就没保活"更确定：

`CommandChild`（`tauri-plugin-shell-2.3.6/src/process/mod.rs:65-70`）既没有 `Drop`
实现也没有 `kill_on_drop`，所以 desktop 退出时**不会主动杀** sidecar。但它的
stdin 管道写端随之关闭，`app-server` 的 stdio 读端读到 EOF → 主循环 `break`
（`server.rs:3326-3333` 的注释写明"桌面关闭即整进程优雅退出"）→ 进程退出。

因此：**desktop 一退出，保活就消失，手机那条连接也同时断掉**。这是既有行为，
不是本 spec 引入的。真要在 desktop 关闭后继续保活（例如手机远程指挥），必须用
headless 模式单独跑一个 app-server（`yi-agent app-server --listen ws://...`
或 `--relay <url>`），那个进程自己持有断言，本设计对它同样生效。

这个边界必须在文档里写清：用户以为"关掉窗口（隐藏，`window.rs:118-135`
是隐藏不是退出）还会继续干活"，但**按 Cmd+Q 真退出**就什么都没有了。两者在
UI 上看起来都是"窗口不见了"，行为却完全不同。

### 断言的生命周期

持有/释放不是两个动作，而是三个状态之间的迁移，**必须显式建模**，否则会出现
「条件恢复了但断言没回来」这类静默失效：

```
Idle ──hold 变真──> Held
Held ──hold 变假──> Idle
Held ──进程退出──> (系统自动回收)
```

两条不变量：

1. **只在迁移边沿调 FFI**。每拍都 `Create` + `Release` 会产生大量瞬时断言，
   `pmset -g assertions` 里看不出稳定状态，也让「到底持着没有」难以判断。
   supervisor 记住当前状态，`Decide::Hold` 且已 `Held` 时是空操作。
2. **`Release` 必须幂等**。释放后立刻又满足条件（例如用户在 15 分钟窗口内重新
   有活）时，必须能重新 `Create`。所以不变量 2 的实质是：状态机自己记
   `Held(bool)`，`Drop` 只在 `Held == true` 时释放，重复调用不产生第二次
   `IOPMAssertionRelease`（对已释放的 id 调用会返回错误，虽然不崩溃但会污染日志）。

**失败要可见，但不能拖垮主循环**：`IOPMAssertionCreateWithName` 返回非
`kIOReturnSuccess` 时（例如权限被 MDM 策略拒绝）记一次 `warn`，把内部状态设为
`Idle`，并在**下次迁移到 Hold 边沿时重试**——不在每拍无脑重试，否则日志每 10s
刷一条。

### 策略：一条公式 + 一个窗口

```
GRACE = 15min                            // 单向宽限窗口

ok   = online && (on_ac || busy)         // 本次 tick 是否满足保活条件
if ok { last_ok = Some(now) }             // 只在 ok 时写入;初值 None
hold = last_ok.map_or(false, |t| clock.since(t) < GRACE)   // 单向时间窗
```

`busy` 与 `on_ac` 是两个独立输入，公式把它们合成一个布尔，所以不需要写
`if on_ac { ... } else { ... }` 这样的分支表：

| 电源 | 网络 | 有活 | ok |
|---|---|---|---|
| AC | 在线 | 任意 | 是 |
| AC | 离线 | 任意 | 否 |
| 电池 | 在线 | 有活 | 是 |
| 电池 | 在线 | 无活 | 否 |
| 电池 | 离线 | 任意 | 否 |

**启动时的初值：`Option<Stamp>`，初始为 `None`。** 不能初始化为 `now`，否则每次
app-server 启动都会无条件 hold 15 分钟，即使开机时明明没有活——那样"关掉再开
一次 app 就能续 15 分钟"会变成绕过手段，也不符合"没活就该睡"的意图。

也不能写成「"很久以前"的 `Stamp`」：`Stamp` 是不透明类型，既没有"很久以前"这种
构造，用 `now() - Duration` 求它还会有下溢风险（机器刚开机时单调钟读数本来就很
小）。用 `Option` 表达"从未满足过"才是干净的：

```rust
let hold = last_ok.map_or(false, |t| clock.since(t) < GRACE);
// ...
if ok { last_ok = Some(now); }   // 只在 ok 时写入
```

`None` 天然不 hold，`Some(t)` 才有得比。全程没有哨兵值、没有减法、没有下溢可能。

**为什么用「最近一次满足距今不到 15 分钟」，而不是「连续 15 分钟不满足才释放」。**

这两种读法听起来只是措辞差别，我按拍模拟对比过，**结论是它们在抖动场景下
完全等价**：字面读法里「连续」的计数器一旦遇到一次满足就清零，单向读法里
`last_ok` 也会在满足时刷新，两者对抖动的反应一模一样。所以"单向读法在抖动上
更好"这个说法是错的，不能拿它当理由。

真正的差异只有一处，而且很重要：**进程刚启动、条件从未满足过时**。

- 字面读法：计数器从 0 开始累加，要连续 900 拍（15 分钟）才释放。于是
  「开机后前 15 分钟」是无条件 hold 的——哪怕当时没网、没活，机器也不许睡。
- 单向读法：`last_ok` 初值为 `None`，第一拍就不 hold，立即不保活。

单向读法在「条件消失满 15 分钟」这个稳态上与字面读法完全一致（都是 15 分钟），
在多提的启动期和别处都不更差，少一个「无条件 hold 窗口」这个副作用，实现也更
简单（一个 `Option<Stamp>` 而不是一个计数器）。这是选它的真实理由。

用户已确认采用这一读法，并把 15 分钟定为宽限窗口。

**为什么用时间戳而不是计数。** 计数依赖 tick 被准时调度；系统负载、App Nap、
时钟调整都会让 tick 迟到，计数会误判。时间戳只比较 `clock.since(last_ok)`，
迟到一次只是晚一点刷新，不会误释放。

**时间源的选择（这是本设计最容易做错的一处）。**

不能直接用 `std::time::Instant`。Rust std 在 Apple 平台把 `Instant` 绑到
`CLOCK_UPTIME_RAW`（`library/std/src/sys/time/unix.rs:55-67`），注释明写：

> `CLOCK_UPTIME_RAW` … is identical to the result of `mach_absolute_time()` …
> We use `CLOCK_UPTIME_RAW` instead of `CLOCK_MONOTONIC` since `CLOCK_UPTIME_RAW`
> is based on `mach_absolute_time`…

而 `mach_absolute_time` / `CLOCK_UPTIME_RAW` **在睡眠期间不前进**。后果是：「断网
→ 机器睡了 8 小时 → 醒来」读出的耗时仍然只有几分钟，于是**一个早已
失效的 `last_ok` 会被当成新的宽限期**，醒来后系统又白扛 15 分钟。这正好是本功能
最该避免的失效形态：条件已经不成立，却还拦着不能睡。

所以：

- **macOS**：计时基准用 `CLOCK_MONOTONIC_RAW`（`libc` 里 `= 4`）。它与
  `CLOCK_UPTIME_RAW` 的**唯一差别**就是睡眠期间照常前进。本机实测两者差
  672831 秒（约 7.8 天累计睡眠），可作证两者的语义差异是真实且量级巨大的。
- **Linux**：必须用 **`CLOCK_BOOTTIME`**，**不能用 `CLOCK_MONOTONIC`**。内核文档
  与系统调用手册都写明 `CLOCK_MONOTONIC` 在 suspend 期间不前进，而
  `CLOCK_BOOTTIME` 与 `CLOCK_MONOTONIC` 相同但**包含 suspend 时间**。这一条很容易
  被顺手写成 `CLOCK_MONOTONIC`（它名字看起来最"正统"），结果在 Linux 笔记本上
  复制出与 macOS 完全相同的 bug。
- **不要用 `tokio::time::Instant`**：它只是 `std::time::Instant` 的薄包装
  （`tokio-1.53.2/src/time/instant.rs:34-35` 的字段就是 `std: std::time::Instant`），
  继承同一个时钟，换成它不解决任何问题。
- 另加一道**墙钟自检**。注意它不是"睡眠检测"，判据也不是「墙钟差超过某个固定
  阈值」——那样在系统高负载（tick 被延迟 25 秒）时会误判。正确做法是拿两条时钟
  的**增量做差**，细节与实测结论见下一节。

  这里有个**必须想清楚的问题**：既然选对了时钟，睡眠已被 `since(last_ok)` 自然
  计入，为什么还要墙钟校验？

  因为在选对时钟的前提下，睡眠**本来就该**消耗宽限，不需要任何特殊处理。真正会
  出问题的是**时钟选错**。而这带来一个反直觉的结论，我实测确认过：

  | 场景 | `dt_mono` | `dt_wall` | `slept_for` |
  |---|---|---|---|
  | 正确时钟，正常一拍 | 10s | 10s | 0 |
  | 正确时钟，高负载 tick 迟到 50s | 50s | 50s | 0 |
  | **正确时钟，睡 8 小时** | 28800s | 28800s | **0** |
  | 错误时钟（`Instant`/`UPTIME_RAW`），睡 8 小时 | 20s | 28800s | **28780s** |
  | 时钟完全冻结，睡 8 小时 | 0 | 28800s | 28800s |

  **关键：用对时钟时，睡再久 `slept_for` 也是 0。** 两个钟都含睡眠时间，一起前进，
  差值恒为零。所以 `slept_for > 5min && dt_mono*10 < slept_for` **不是"检测到
  睡眠"，而是"时钟不含睡眠时间"的告警**——它只在时钟选错时才会触发。把它的含义理解成"检测睡眠"会让整段
  逻辑失去意义（正常睡眠永远不触发），后续所有"睡醒后要做 XX"的推理都会建立
  在错误前提上。

  所以这条校验的唯一用途是**自证时钟选得对不对**，绝不用来修正宽限：

  ```rust
  let dt_mono = clock.since(last_sample);            // 期望:睡眠期间照常前进
  let dt_wall = wall_since(last_sample_wall);
  let slept_for = dt_wall.saturating_sub(dt_mono);   // 正确时钟下恒≈0
  // 用比例而不是绝对阈值:见风险一节,绝对阈值在 dt_mono=20s 时会漏判
  if slept_for > Duration::from_secs(300) && dt_mono * 10 < slept_for {
      clock.suspect();          // 只计数。连续 3 次 → mono_trusted = false
      // 注意:不改 last_ok,不改宽限,不改任何判据
  }
  if ok { last_ok = Some(now); }   // now 取自睡眠感知时钟;初值 None
  hold = last_ok.map_or(false, |t| clock.since(t) < GRACE);
  ```

  验证过的行为（假时钟模拟）：

  - 正确时钟 + 睡前 `ok=true` + 睡 8h 后 `ok=false` → 醒来 `since(last_ok)≈8h15m`
    → **立即释放**。没有任何"睡眠特殊处理"，纯靠时钟正确。
  - 正确时钟 + 睡 8h 后 `ok=true` → `last_ok` 刷成此刻 → 重新起算 15 分钟。
  - 错误时钟 + 同样场景 → `slept_for` 立刻告警，连续 3 次后切墙钟；若模拟里不给
    这条兜底，`since(last_ok)` 只读到几分钟，会 hold 住不放（我已复现）。

  两条"睡眠后行为"都不需要 `credited_sleep` 这类"扣减睡眠时长"的补偿项。
  **那种机制有害**：`credited_sleep` 若只增不减，在条件长期不 ok 时会把 `awake`
  永久压成 0，释放路径彻底不可达。我模拟过这个发散（每 50 拍睡一次的场景，`credited`
  累到 2400s 而 `awake` 恒为 0，宽限永不触发）。**宽限只能单向流逝。**

  `last_remote_touch` 同理不需要在睡眠时显式作废：用对时钟后
  `clock.since(last_remote_touch)` 自然大于 5 分钟，`remote_touch` 自动为 false。

  边界：`d_mono == 0` 时 `slept_for == d_wall`，此时 `0 < slept_for` 恒成立，
  只要墙钟前进超过 5 分钟就告警——这正是时钟冻结时该有的反应（连续触发 →
  判 `mono_trusted=false`）。全程只有减法、乘法与比较，没有除零。
  `d_wall` 为负（墙钟被往回调）时 `saturating_sub` 给出 0，不会告警。

两道保险并存：即使某一平台的单调时钟选错（Linux 上误用 `CLOCK_MONOTONIC`），
墙钟差值这一条也能兜住；即使墙钟被人为大幅调整（NTP 跳变），单调钟也不会给出
负数。这套判据同时覆盖「进程被 SIGSTOP / 调试器挂起 / 内存换出」这些非睡眠冻结：
它们同样只让墙钟前进。

实现上必须按平台分支，因为**`CLOCK_BOOTTIME` 在 macOS 上不存在**：我在本机直接
调用 `clock_gettime(7, …)` 返回 `-1`；`CLOCK_MONOTONIC_RAW`(4) 与
`CLOCK_UPTIME_RAW`(8) 都返回 0。所以「一个时钟跨平台通用」是假的，**失败分支是
真实可达的**，不是防御性编程。`libc` 已在 workspace 里被 5 个 crate 使用
（`libc = "0.2"`），不需要新增依赖。

`clock_gettime` 失败时**不要退化为 `std::time::Instant::now()`**——那正是全文论证
过的坏时钟（睡眠期间不前进），退化到它等于把已知缺陷当成兜底。正确做法是置
`mono_trusted = false`、**退化为墙钟 `SystemTime`**：它虽然会被 NTP 跳变干扰，但
睡眠期间照常前进，恰好是本功能需要的那一半性质。绝不 panic（电源策略的辅助功能
不该让 app-server 崩掉）。连续失败说明平台不支持所选时钟，应在**启动时**打一条
明确日志，而不是每 10s 刷屏。

若墙钟也取不到（`SystemTime::now()` 在 `UNIX_EPOCH` 之前，理论上不可能），
`clock` 返回 `Option`，调用方直接放弃保活（不 hold）——**任何不确定都倒向"允许睡"**，
因为永不睡是用户无法察觉的失败。

**切换是单向的，不尝试"修复回单调钟"**：退化到墙钟后 `dt_mono` 与 `dt_wall`
同源，`slept_for` 恒为 0，`suspect` 不再累积，因此永远不会有"自愈"的机会。这是
刻意的，不是缺陷——墙钟已经是可用基准（含睡眠时间），而反复试探坏时钟只会让
行为摇摆。实现时不要试图加"定期重试单调钟"的逻辑，那会让 `hold` 在切换瞬间抖动。

**边界取 `<`**：恰好 15 分 00 秒整时释放。这条写进测试，避免以后有人把 `<=` 改回去
而没人发现。

### 探测

- **电源**：`IOPSGetProvidingPowerSourceType(IOPSCopyPowerSourcesInfo())`。
  需要两个精确事实（都在 `IOPowerSources.h` 里核实过）：

  1. 它返回的是 **`kIOPMACPowerKey` = `"AC Power"`**（`IOPowerSources.h:215`），
     不是 `kIOPSCopyPowerSourcesInfo` 那套描述字典里用的 `kIOPSACPowerValue`
     （`IOPSKeys.h:762`）。两个宏的字符串值相同（都是 `"AC Power"`），但**常量名
     分属两套 API**，混用会让人以为需要去解析字典。
  2. 可能的返回值有三个：`"AC Power"` / `"Battery Power"` / **`"UPS Power"`**
     （`IOPowerSources.h:195-215`）。**UPS 必须判为 on_ac**——UPS 插着的 Mac
     正在用外接电源，把它当电池会莫名其妙地停止保活。判定写成
     `!= "Battery Power"`（含未知值）比 `== "AC Power"` 更稳：未知电源类型时
     偏向保活，而「偏保守不保活」在这里没有安全收益。

  内存：`IOPSCopyPowerSourcesInfo` 是 `Copy` 规则（返回 +1 引用），必须
  `CFRelease`；`IOPSGetProvidingPowerSourceType` 返回的 CFString 归前者持有，
  **不要再 release**（`IOPowerSources.h:297-303` 明确写了 caller should NOT release）。
  这两个所有权方向反了就是一次过度释放崩溃。

  这条也实测过（本机插电）：`IOPSGetProvidingPowerSourceType` 返回
  `"AC Power"`，与 `"Battery Power"` 比较为不等。所以判定逻辑可以直接照此写，
  不需要事先解析 `IOPSGetPowerSourceDescription` 的字典。本地调用，每次 tick 都做，
  开销可忽略。
- **网络（这是本 spec 最容易做错的一处：不要把「有网」等同于「provider 可达」）**。

  一个很自然但错误的做法是「对 provider `base_url` 与 relay 的 host 做 TCP
  connect」。它的问题在于：provider 服务故障、DNS 污染、或被墙时 TCP connect 会
  失败，于是 `online=false` → 不许保活 → **该干活的时候反而睡了**。而「有网」的
  语义是*本机有没有可用的网络出口*，跟某个具体远端此刻是否健康无关。两者混淆会
  让本功能在「代理/网关正好抽了一下」这种最常见的场景里失效。

  正确做法（**不依赖任何外部服务**）：

  1. **先看链路层**：本机是否有**真正承载互联网流量**的、已 up 且非 loopback 的
     接口。用 `getifaddrs(3)`（`libc` 已暴露）遍历，但判据不能只是
     `IFF_UP && !IFF_LOOPBACK` —— 我在本机实测该判据返回 **14 个**"可用"接口，
     其中绝大多数是噪音：

     ```
     utun3 / utun4 / utun5 / utun6   ← 隧道（VPN / 系统代理），Wi-Fi 断了它们仍 UP
     awdl0 / llw0                    ← AirDrop / Apple Wireless Direct，永远 UP
     en0  ×5                         ← 真实网卡（多个地址，正常）
     ```

     **只按 flags 判定会让 `online` 恒为 true，整条网络判据失去意义。** 而且
     「有地址」这个条件不能省：本机第二次实测（只打印 `IFF_UP && !IFF_LOOPBACK`
     的接口）发现 `en1`~`en8` 这 7 个**根本没配地址**的空接口也全都被算成候选。
     加上「必须有 IPv4 地址」之后，候选立刻收缩到两个：

     ```
     en0      192.168.31.88     ← 真实 Wi-Fi
     utun4    10.72.248.102     ← VPN 隧道
     ```

     正确的判据是三层的，缺一层都会被噪音骗：

     - **必要条件 1**：`IFF_UP` 且非 `IFF_LOOPBACK`；
     - **必要条件 2**：**至少有一个 `AF_INET` 地址**。没有地址的接口（`en1`~`en8`
       这种占位接口）不可能承载流量，只看 flags 会把它们全放进来；
     - **排除**已知的虚拟/旁路接口前缀：`utun` `awdl` `llw` `bridge` `gif` `stf`
       `anpi` `ap`（macOS）；`tun` `tap` `docker` `br-` `veth` `virbr` `lo`（Linux）。
       这些接口在底层网络已断时仍可能 `UP`，且（如 `utun4`）还带着地址。
     - **不依赖名字白名单**。Linux 上真实网卡可能叫 `ens5` / `enp0s3` / `wlan0` /
       `eth0`（云主机甚至叫 `eno1`），macOS 上叫 `en0`。「必须以 `en`/`eth`/`wl`
       开头」在 Linux 上会**把所有真实网卡都判成离线**，那是比误判在线更糟的方向。
     - **最终确认交给第 2 步（网关探测）**：上面三条只是筛出候选接口，`online`
       由「有候选接口」**且**「网关可达」共同决定。这样既不需要维护名字白名单，
       也不会被隧道接口骗过——隧道即使 UP 且带地址，网关探测仍能反映真相。

     注意离线判定不止「没网卡」一种：接口存在但网关不可达同样是离线。

  2. **再确认能出网**：对**网关**或 DNS 服务器做一次探测，而不是对 provider。
     网关来源我实测过三种，只有一种可用：

     | 方式 | 结果 |
     |---|---|
     | `route -n get default` | 输出含 `gateway: 192.168.31.1` 与 `interface: en0`，普通用户可跑，**可用** |
     | `sysctl -n net.route.0.0` | **不存在**（`sysctl: unknown oid`），不可用 |
     | 解析 `/etc/resolv.conf` | macOS 上常是桩文件，不可靠 |

     所以用 `route -n get default`，解析 `gateway:` 行。注意它是**子进程**：
     `yi-agent-app-server` 已有 `std::process::Command` 用法（`git_diff.rs:213`），
     不算引入新模式；但要给 1s 超时，且失败一律走下面的兜底。网关地址可缓存 30s，
     不必每拍都跑子进程。

     **VPN 场景必须写清楚，否则实现会踩坑。** 本机 `netstat -rn -f inet` 显示
     **两条默认路由**：

     ```
     default   192.168.31.1   UGScg    en0      ← 物理出口
     default   link#28        UCSIg    utun4    ← VPN 隧道
     ```

     而 `route -n get default` 只返回 **en0 那条**（`gateway: 192.168.31.1`、
     `interface: en0`）。这对本设计恰好是**想要的**结果：

     - 我们问的是「本机还连得上局域网/上游吗」，物理出口的网关正是这个问题的答案；
     - VPN 隧道的 gateway 是 `link#28` 这种**点对点链接层标识，不是可探测的 IP**，
       对它做 TCP connect 无意义；
     - 所以「取 `route -n get default` 的第一条」在 VPN 开着的机器上也得到可用网关，
       不需要额外排除 utun。

     代价是：若系统把 **VPN 设为唯一出口**（物理链路已断、只剩隧道），`route -n get
     default` 可能只返回 utun 的 `link#...`。那时 `gateway:` 解析不到合法 IP，
     走下面的「拿不到网关」兜底（偏向 `online=true`），不会误判离线。这个方向的
     失败是安全的，符合「反向失败后果更严重」的判断。

     我在本机实测了这条探测的实际分辨力，结论是**可用但要理解它的边界**：

     ```
     192.168.31.1:80   -> connected      (正常,网关可达)
     192.168.31.1:9999 -> Connection refused (端口关闭,但链路是通的)
     192.168.31.99:80  -> timeout        (不存在的内网地址 = 真断网的样子)
     1.1.1.1:443       -> timeout        (公网被拦/走代理时也会超时)
     ```

     两个由此得出的实现约束：

     - **端口不能用「一定开着」的假设**。网关 80/443 通常不开，会返回
       `Connection refused`。而 **`ECONNREFUSED` 恰恰证明链路是通的**（包到了对端，
       对端回了 RST）。所以判定必须是「`connect` 成功 **或** `ECONNREFUSED`」= 在线，
       只有**超时**或 `EHOSTUNREACH`/`ENETUNREACH` 才算离线。这一点写反就会在
       99% 的正常网络里判成离线。
     - **不要探测公网地址**（如 `1.1.1.1:443` 作健康检查）。本机实证它会超时——那
       可能是有意拦截或走了代理，与「本机有没有网」是两回事。只用**默认网关**，
       它就在同一链路上，语义最干净。

  3. **两者都拿不到时**：`online = 无网卡 ? false : true`（保守偏向「有网」），
     并在日志里记一次"无法确定出口"。

  relay host 是**可选**的第二来源，不是必须：relay 是自家基础设施，它的可达性比
  第三方 provider 更能代表「这台机器能不能出网」，但**没有配 relay 时完全不影响
  判定**——绝大多数本地用法都不配 relay。

  **不要用 `SCNetworkReachability`**：它自 macOS 14.4 起整体标记废弃
  （`SCNetworkReachability.h` 里每个函数都带 `API_DEPRECATED_WITH_REPLACEMENT`），
  替代品 `NWPathMonitor` 是异步推送、需要 EventLoop，与本设计的 10s 同步判定
  不匹配。引入一个已废弃且语义为「某个 host 是否可达」的 API，正好会重犯上面
  那个错误。

  探测本身是 TCP connect + 2s 超时。
- 网络探测最小间隔 30s（tick 是 10s，避免每拍都连一次）。失败后按 30s 重试，不指数
  退避——这是一次廉价的本地探测，不是远程调用。
- 探测目标缺失**不构成离线证据**：未配置 relay 时用默认网关，解析不到网关时用
  `getifaddrs` 的网卡状态兜底。只有在**确实没有任何可用出口**（无网卡、也无默认
  路由、也无 relay）时才判 `false`。这与「宁可让它睡」并不冲突——「没证据」和
  「有反面证据」是两件事，前者偏向保活，后者才允许睡。

### 断言怎么下（macOS）

- 类型是 **`PreventUserIdleSystemSleep`**。它只挡「空闲自动睡眠」：显示器照常熄灭、
  合盖照常睡、菜单里手动睡眠照常睡、系统更新重启照常重启。SDK 原话是
  「The display may dim and idle sleep while this assertion is enabled, but the
  system may not idle sleep. The system may still sleep for lid close, Apple menu,
  low battery, or other sleep reasons.」（`IOPMLib.h:285-287`）。这正是需求里
  「系统全局没有进入休眠的意图时不得主动休眠」和「不越过系统」的落点。
- **不用** `PreventSystemSleep`：它会连合盖都挡住，属于越权。
- **也不用 `NetworkClientActive`**，尽管它看起来更贴切（SDK 里它的定位就是
  「Keeps the system awake while OS X serves active network clients」，
  `IOPMLib.h:344`）。三个原因：

  1. 它的语义是「**正在服务远端客户端**」。而我们要保活的场景大部分是本地开工
     （终端里跑 turn、看板推卡片），根本没有远端客户端。用它会让断言名与真实
     原因对不上，`pmset -g assertions` 里看的人会误解。
  2. 它的行为**依赖系统对"网络是否还可用"的判断**：SDK 明写
     「If the network is no longer available, this assertion may stop working and
     allow the system to go to sleep」（`IOPMLib.h:365-366`）。我们的策略要求
     「在线 + 上电才保活」，判据必须由我们自己掌握，交给不透明的系统判断会让
     本机网卡状态与断言状态脱节。
  3. 配套 API `IOPMDeclareNetworkClientActivity` 的有效期是「system sleep timer」，
     与我们的 15 分钟窗口是两套时间语义，混用会让测试无法用假时钟驱动。

  一句话：它是「服务远端客户端」的专用工具，我们做的是「本机在干活就别睡」，
  类型用 `PreventUserIdleSystemSleep` 更准确。**这不影响 (c) `remote_touch` 那条
  判据**——那条是判断"有没有人还在指挥我"，与用哪个断言类型无关。
- **不持** `PreventUserIdleDisplaySleep`：会让屏幕一直亮，纯浪费电。
- **不起 `caffeinate` 子进程**。子进程方案有三个真实问题：多一个可被误杀的对象；
  崩溃后可能残留（残留的 caffeinate 会让整机永不睡且没人知道原因）；无法按状态精确
  持有/释放。`IOPMAssertion` 在**进程退出时由系统自动回收**，崩溃不留持久断言。

**实现陷阱（已在 SDK 里核实，并已端到端实测）**：`PreventUserIdleSystemSleep` 是宏
`kIOPMAssert_PreventUserIdleSystemSleep`，定义在 `IOPMLib.h:292`，展开为
`CFSTR("PreventUserIdleSystemSleep")`，**不是导出符号**。Rust 侧不能
`extern { static kIOPMAssert_... }` 引用它（链接会失败），必须自己构造等值 CFString。
`IOPMAssertionCreateWithName`（`IOPMLib.h:776`）与 `IOPMAssertionRelease`
（`IOPMLib.h:662`）是真实导出符号，可以直接 extern。

这条路径我在本机用一段等价的 C 程序验证过，**不是纸面推导**：

```c
IOPMAssertionCreateWithName(CFSTR("PreventUserIdleSystemSleep"),
    kIOPMAssertionLevelOn, CFSTR("yi-agent: waiting for work"), &id);
```

- 返回值 `0x0`（`kIOReturnSuccess`），拿到有效的断言 id；
- 存活期间 `pmset -g assertions` 实际输出：
  `pid 88489(...): PreventUserIdleSystemSleep named: "yi-agent: waiting for work"`；
- `IOPMAssertionRelease` 返回 `0x0`，断言随即消失。

所以「类型名、断言名字、能否被 `pmset` 看见」这三件事都已确认。断言名带上标识，
便于 `pmset -g assertions` 里一眼认出来。

**一个必须写明的能力边界：该断言在 Dark Wake 下无效。** SDK 原文两句
（`IOPMLib.h:289`、`:312`）都是同一句：

> This assertion has no effect if the system is in Dark Wake.

本机 `pmset -g` 显示 `powernap 1`、`standby 1`，也就是 Power Nap 是开着的——
系统在睡眠中会定期进入 Dark Wake 去做后台维护。在这个窗口里断言不生效，
但这**不是本 spec 能修的问题**，也不构成缺陷：

- Dark Wake 是系统自己安排的短暂醒来，不是用户要的那种"睡过去"；
- 它结束时系统按原有 sleep timer 继续，我们的断言依旧在场，出 Dark Wake 后立即恢复效力；
- 真要把 Dark Wake 也挡住，需要 `PreventSystemSleep`，那是越权（见上）。

所以验收时不要用「睡眠中后台维护窗口里断言是否有效」来质疑功能；判据只看
**醒着（full wake）期间**断言是否正确持有与释放。

### 组件边界

新增 crate `yi-agent-rs/crates/yi-agent-power`：

| 模块 | 职责 | 依赖 | 可测性 |
|---|---|---|---|
| `policy` | `Policy::decide(&Snapshot, now) -> Decision` | 仅 `std::time::{Duration, Instant}` | 纯函数,全单测 |
| `clock` | 单调时钟（睡眠期间前进）+ 墙钟自检（`suspect()` → `mono_trusted`） | `libc::clock_gettime`（仅 unix） | 经 trait 注入 |
| `assertion` | `Assertion` 句柄,`Drop` 释放 | IOKit（仅 macOS） | 经 trait 注入 |
| `probe` | `PowerProbe` / `NetProbe` trait + 真实实现 | IOKit / `tokio::net` | 经 trait 注入 |
| `supervisor` | 10s tick,串联三者,记状态供查询 | 以上三者 | 用假实现驱动 |

`policy` 必须不依赖 `assertion`/`probe` 的具体类型，只吃
`Snapshot { on_ac: bool, online: bool, busy: bool }` 与调用方给的 `now`。
这样 15 分钟窗口的全部行为可以在无 IOKit、无网络的机器上（也就是 Linux CI）测完。

`clock` 对外只有三个方法，语义必须钉死，否则实现时很容易混用：

| 方法 | 返回 | 时钟来源 |
|---|---|---|
| `clock.now()` | 当前读数（不透明类型 `Stamp`） | 睡眠期间**照常前进**的单调钟 |
| `clock.since(stamp)` | `Duration`，恒非负 | 同上 |
| `clock.suspect()` / `mono_trusted()` | 计数 / 是否可信 | 由墙钟差值自检 |

**`Stamp` 是 `clock` 自己的类型，不是 `std::time::Instant`**——这是刻意的，
让「误用 `Instant::now()`」在编译期就报错，而不是等到睡着之后才发现。
凡是要判「过了多久」的地方（`last_ok`、`last_remote_touch`、看门狗）一律用
`clock.since()`，全文没有第二个时间来源。

**`clock` 单独成模块的理由**：把「睡眠感知时间」这一件事收在一处。`policy` 只接受
外部传入的 `now`，因此它对时钟的选择是零知识——时钟错了，只有 `clock` 一个文件
错，而且 13 行动机写在文件头。若让它散落在 supervisor 里，"为什么不用
`Instant::now()`"这个问题以后一定会有人踩。

### 接线点

**1. `yi-agent-app-server/src/server.rs` 主循环**

已有的 `driver_ticker`（`server.rs:3276`，3s，分支在 `3822`）旁边再加一个 10s ticker。
`busy` 由**三个来源**并起来，缺一个就会漏掉一整类活：

```rust
// (a) 本进程的会话:与 server.rs:6165-6173 的 idle 快照同一口径,只取反
//     ThreadStatus 只有 Idle/Running/AwaitingApproval 三个变体
//     (protocol.rs:781-784),后两个都算有活
let local = threads.values().any(|s| {
    s.active_turn_id.is_some()
        || !matches!(*s.status.lock().unwrap_or_else(|p| p.into_inner()), ThreadStatus::Idle)
});

// (b) 子 agent / 看板 / 定时任务:跑在项目 daemon 里,不在上面这张表。
//     关键:socket 是**按项目**的(server.rs:829-845 的 socket_for_thread 用
//     project_key(cwd) 查 runtimes),不是一个全局 socket。必须遍历所有
//     已挂载的项目 runtime,漏掉任何一个都会漏掉那个项目里的活。
let remote_busy = runtimes
    .lock().unwrap_or_else(|p| p.into_inner())
    .values()
    .filter_map(|entry| entry.as_ref().ok()?.current().ok())
    .any(|handle| {
        yi_agent_store::ipc::send_request_with_version_timeout(
            &handle.socket_path,
            yi_agent_store::ipc::PROTOCOL_VERSION,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: true,
            },
            std::time::Duration::from_millis(500),   // tick 是 10s,必须远小于它
        )
        .map(|r| match r {
            yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } => tasks.iter().any(|t| {
                matches!(t.state.as_str(),
                    "running" | "waiting_for_resource"
                        | "waiting_for_permission" | "waiting_for_children")
            }),
            _ => false,
        })
        .unwrap_or(false)
    });

// (c) 远端近期有交互。注意 elapsed 必须走 clock 模块的睡眠感知时钟,
//     不能用 Instant::now():睡 8 小时后 Instant 只前进几分钟,
//     会让一条早已失效的交互记录继续算作"刚刚有过交互"
let remote_touch = last_remote_touch
    .map_or(false, |t| clock.since(t) < std::time::Duration::from_secs(5 * 60));

let busy = local || remote_busy || remote_touch;
```

**(c) 的赋值点**：主循环 `inbound.recv()` 分支（`server.rs:3340`）拿到每一帧时，
若该帧来自**非本地**客户端就刷新 `last_remote_touch`。必须排除本地客户端，
否则判据形同虚设：`ClientId::local()`（`broadcast.rs:52-54`）是 stdio 传输的唯一
客户端，而桌面壳正是用 stdio 连的——如果不排除，"本地桌面每发一条消息"都会把
`remote_touch` 刷新成真，于是 `busy` 恒为真，电池下永不释放。
只有 `ClientId::ws(..)`（`broadcast.rs:57-59`）才算"远端"。

这条覆盖面是完整的：`ClientId::ws` 只在 `ws.rs:284` 一处注册，而 relay 也不是
独立入口——`--relay` 的环回桥走的是同一个 `serve_ws_body`（`ws.rs:53-54` 的注释
说明 ws 是"第三条生产入口……与 `--relay` 的环回桥同构"）。所以「不是 local 就是
远端」这一个判据同时覆盖手机直连与经 relay 连接，不需要按传输方式分支。

**(c) 的时钟选择是这条规则唯一的坑**：`last_remote_touch` 是「远端最后一次发来
消息」的时间戳。若它和 `last_ok` 用了不同的时钟，就会出现「宽限期正确释放了，但
`remote_touch` 仍为真，于是 `ok` 依旧成立」——两个判据互相抵消，15 分钟窗口形同
虚设。所以 `last_remote_touch` 必须与 `last_ok` 一样用 `clock` 模块的
睡眠感知时钟——用对时钟后它**自动**失效（睡 8 小时后 `since()` 就是 8 小时，
远超 5 分钟），不需要额外的"睡醒后清空"逻辑。

**(a) 的判据为什么是「非 Idle」而不是「Running」。** `ThreadStatus` 只有三个变体
（`protocol.rs:781-784`）：`Idle` / `Running` / `AwaitingApproval`。用 `!= Idle`
把「等人审批」也算成有活是对的：那一刻 turn 没结束、上下文都在，机器睡了用户回来
点一下批准，这条 turn 已经随睡眠死掉了。若写成 `== Running`，只要有一个工具在等
批准，机器就会在电池下睡着。这与 `active_turn_id.is_some()` 并列，是因为存在
「turn 已挂上但状态还没切成 Running」的短暂窗口，两个条件取或覆盖得更全。

**(b) 容易被漏掉，但必须覆盖。** 子 agent、看板卡片、定时任务都由项目
runtime daemon 执行（`server.rs:1267` 的 `attach::ensure_owned_runtime`），
它们**不出现在 app-server 的 `threads` 表里**——只按 `threads` 判 busy，会让
「看板在后台推进卡片」这种情况被判成无活，然后在电池供电下睡着，正是本功能
最该覆盖的场景。

好消息是不需要新增任何协议：`IpcRequest::ListTaskSummaries { session_id, active_only }`
（`yi-agent-store/src/ipc.rs:387-392`）**已经支持 `active_only`**，而且 app-server
本来就在每个会话上用 500ms 周期轮询它（`server.rs:992-1012` 的 `watch_children`），
所以这条调用路径、错误处理、daemon 不在时的降级都已有先例。

**但超时必须缩短，这是实现时最容易踩的一脚。** `ipc::send_request` 的默认回复
超时是 `CLIENT_REQUEST_TIMEOUT = 30s`（`ipc.rs:57`），而本功能的 tick 只有 10s。
若直接调 `send_request`，一个读请求后不答的 daemon 会让主循环一次卡住 30 秒——
比它想解决的问题更严重（主循环卡住 = 所有会话停摆）。

因此：

- 必须用 `send_request_with_version_timeout(..., Duration::from_millis(500))`
  （`ipc.rs:1311`，这个变体就是为「明确指定回复截止时间」而抽出来的）。
- 或者把这步放到 `spawn_blocking` / 独立任务里，**绝不能在主循环的同一线程同步等**。
  本设计采用前者 + 同步短超时：500ms 远小于 10s tick，最坏情况只是这一拍慢半秒。
- daemon 不在（socket 不存在）时 `connect` 会立刻失败，不需要等超时；
  这条路本来就是快的。

这条约束的实质是：**保活是辅助功能，它在任何情况下都不得让主循环变慢。**
漏掉这条，一个卡死的 daemon 就能让整个 app-server 无响应——比让机器睡一觉糟得多。

**一个已知盲区，必须承认。** `runtimes` 只包含**已经挂载过**的项目 runtime
（键是 `project_key(cwd)`，由 `ensure_owned_runtime` 在会话建立时填入）。
所以：

- 用户在这个 app-server 里打开过某项目的会话 → 它在 `runtimes` 里 → 能查到；
- 看板在某个项目上推卡片，但用户从没在这个 app-server 里打开过那个项目 →
  **查不到**，那份活会在电池供电下被判成"无活"。

这是可接受的，理由是**不放大失败后果**：那种情况下用户根本没在这个 app-server
里工作，判成"无活"、让机器睡，正是"没有活要保"的正确行为；醒来后看板会自己续。
反过来说，若为了覆盖率去枚举磁盘上所有 `.yi-agent/runtime.sock` 并逐个探测，就要
引入目录扫描 + N 次可能各等 500ms 的 IPC，会把 10s tick 撑坏——代价远大于收益。

结论：**不做全局 daemon 扫描**。判据只覆盖本 app-server 已知的项目，这在
"辅助功能不得拖慢主循环"和"覆盖率"之间是刻意的取舍。

但它的「活跃」口径比我们想要的宽，必须写清楚：`active_only=true` 的 SQL 是
`state_json NOT IN ('completed','completed_no_changes','failed','cancelled')`
（`repository.rs:3433-3437`）——**只排除了 4 个**，而 `TaskState`
（`yi-agent-core/src/subagent/task.rs:145-161`）共 15 个变体，`is_terminal`
（`task.rs:261-274`）认定其中 **9 个**是终态。差额里还留着
`blocked` / `stalled` / `timed_out` / `budget_exhausted` / `recovery_required`
这五种「已经停下、需要人或外部介入才会动」的状态，外加 `queued` / `paused`
这两种「不会自己推进」的状态。

正确的 busy 口径因此是「**非终态 且 非 queued/paused**」，等价于只保留
`running` / `waiting_for_resource` / `waiting_for_permission` / `waiting_for_children`：

| 状态 | 算「在干活」 | 原因 |
|---|---|---|
| `running` | 是 | 正在执行 |
| `waiting_for_*`（三种） | 是 | 卡在等资源/审批/子任务，进度仍会推进 |
| `queued` | 否 | 排队而已；睡着后醒来照跑，不丢进度 |
| `paused` | 否 | 人主动暂停的 |
| 其余 9 个终态 | 否 | `is_terminal` 已认定它们不会再动 |

实现上**不要手抄状态清单**：仓库已有 `TERMINAL_TASK_STATES`（`repository.rs:53-69`，
11 项，含 `recovery_gated`/`recovery_attested` 两个落盘别名）与
`TaskState::is_terminal`（`task.rs:261`），且两者被
`prune_treats_only_terminal_states_as_finished` 钉住一致。在 app-server 侧我们只有
序列化字符串，所以判据写成「不在终态集合里，且不是 `queued`/`paused`」；
若需要字符串版终态集合，应把 `TERMINAL_TASK_STATES` 提升为 `pub` 复用，
而不是在 `yi-agent-power` 里再抄一份——两份清单迟早会分叉。

新增状态时默认按「不算」处理并记一次未知状态日志——**偏向允许睡眠**：判错的代价
是睡了一觉（醒来可续），而反过来判错的代价是整夜不睡且无人察觉。

这条也顺带说明了一个原则：本功能的 busy 判据要的是「**是否会自己继续推进**」，
而不是「记录里是否还有未终结的行」。`paused` 就是反例，`blocked` 更是——
一个阻塞等待人工介入的任务可以让机器无限期不睡。

代价与边界：这是 10s 一次的 socket 往返，不是热路径。daemon 不在或超时时按
`false` 处理（宁可睡）。注意 (b) 只对**已 attach 的项目**有 socket；`runtimes`
表为空时跳过，不主动拉起 daemon——为探测是否该保活而去启动一个 daemon，是
副作用反过来驱动策略，不能这么做。

远端交互时间戳的来源：主循环处理 inbound 帧处（`server.rs:3340` 的
`line = inbound.recv()` 分支）记一个时间戳，非 local client 才刷新。
`hub.client_count()` 只用来判断「有没有远端连接」，**不用它判 busy**：
纯挂着不动的配对连接不算有活，否则手机放桌上连着，笔记本整晚不睡，正好违背
「电池下该睡就睡」。

**2. 开关放 `settings.json`，不是 `preferences.json`**

不能写进 `<workdir>/.yi-agent/preferences.json` 的 `prevent_idle_sleep`，
两处实证：

1. **UI 不会渲染它**。桌面与 web 的设置页都由 `settings::FIELDS`
   （`yi-agent-runtime/src/settings.rs:167`）驱动渲染，`settings/read`
   （`rpc/ui.rs:93`）只吐这张表里的字段。写进 `preferences.json` 的键在设置页
   根本不会出现，等于开关只能靠手改文件。
2. **`preferences.json` 是按进程 workdir 的**，而 `settings.json` 走
   `yi_agent_store::paths::home_join`（`settings.rs:359-362`），是**全局单份**。
   本功能是整机电源策略，全局语义才对；按项目各存一份会出现「A 项目开着、
   B 项目关了，到底保不保活」的无解状态。

正确落点：

- `FIELDS`（`settings.rs:167` 起的数组）加一条，四个字段要一起给：
  `key: "agent.prevent_idle_sleep"`、**`group: "agent"`**、
  `field_type: FieldType::Select`、`options: &["true", "false"]`、
  `default_display: "true"`。`group` 是显式字段（`settings.rs:154`），
  决定它落在设置页哪一栏；`GROUP_ORDER = ["agent","compact","startup"]`
  （`SettingsRuntimeTab.tsx:15`）里已有 `agent`，所以不需要动前端分组顺序
  （对比 `tui.*` 那几个字段：`tui` 不在 `GROUP_ORDER` 里，走的是"其余分组
  排在最后"的兜底分支，所以它们显示在页面末尾）。
  **不能加 `FieldType::Bool`**：枚举只有 `Number` / `Text` / `Select`
  （`settings.rs:142-146`），且布尔在该文件里已有既定表达方式——见
  `tui.notifications`（`settings.rs:271-280`）用 `Select` + `["true","false"]`
  + 存字符串。`settings.rs:125-131` 解释了为什么必须存字符串而不是 `Option<bool>`。
- 写入走现成的 `settings/write`（`rpc/ui.rs:97`），它已有 scope 校验
  （`settings_rpc::write_scope()` 返回 `Scope::Control`）与整表校验，不需要新 RPC。
  `FieldType::Select` 的校验是「值必须出现在 `options` 里」
  （`settings.rs:658-670`），写 `"yes"` 之类会被明确拒绝，这正是选 `Select`
  而不是 `Text` 的第二个好处。
- **新键要在四个地方同步落地，漏一处就会静默失效**：
  1. `FIELDS` 数组加 `FieldSpec`（决定渲染与校验）；
  2. `AgentSettings`（`settings.rs:85`）加 `Option<String>` 字段 +
     `deserialize_with = "de_optional_string"`，否则 `settings.json` 里的值读不回来；
  3. `settings_rpc.rs:46-66` 的 `current_value` match 加一个分支——这个函数
     漏键会静默返回 `null`，前端显示"未设置"而文件里其实有值；
     已有测试 `every_field_in_fields_can_be_read_back`（`settings_rpc.rs:116`）
     遍历 `FIELDS` 断言每个键都能读回，加了 (1) 不加 (3) 会直接红；
  4. `apply_op`（`settings.rs:675-680`）用 `split_once('.')` 分 `group`/`field`，
     所以键名必须恰好是 `agent.prevent_idle_sleep`，不能多一层点。
- 默认 `"true"`；缺文件 / 空串 / 未知值按默认开处理（与 `tui.notifications`
  的兜底一致）。
- **改动要立即生效，不能只读启动时那一次**。`read_value` 是每次 RPC 现读磁盘，
  但 supervisor 跑在自己 10s tick 里，不会因为设置页保存就重读。做法照抄
  `CatalogWatcher::current()`（`models.rs:411-437`）的「指纹变了才重读、解析失败
  保留 last-known-good 且不推进指纹」模式——同一个文件已有两处用这套，
  不引入新概念。关闭时立即释放断言，不等 15 分钟窗口（显式用户意图不需要宽限）。

**3. `desktop` 显示状态**

目前**没有**任何 `app/status` 或健康检查类 RPC（`rpc/` 下只有
`config/read`、`settings/read|write` 等具体域方法）。所以状态显示要么新增一个
只读 RPC，要么不做。建议**本 spec 不做**：状态是辅助信息，为它新增协议会扩大
改动面和测试面。

验收不依赖 UI——`pmset -g assertions` 本身就是权威视图（见「验收」一节），
它比 UI 更直接（UI 只能显示进程自己认为的状态，断言列表是系统实际看到的）。
若后续确实要显示，再加 `agent/powerStatus` 只读 RPC。

## 测试

全部可确定性测试，不需要真睡眠、不需要真断网：

| 用例 | 断言 |
|---|---|
| 插电+有网+无活 | 一直 hold |
| 进程刚启动,条件不成立 | **不 hold**（`last_ok` 初值为 `None`,不是 `Some(now)`;回归哨兵） |
| 插电+有网,第 14:59 断网 | 仍 hold |
| 插电+有网,断网满 15:00 | 释放（边界取 `<`） |
| 断网满 15 分钟后恢复 | 已释放,恢复后重新持有（不是「从未释放」;**证明宽限真的会到期**） |
| 断网 3 分钟恢复 | 全程未释放（抖动不闪断;两种读法此处等价） |
| 启动后一直不满足条件(从没 ok 过) | **第一拍就不 hold**（唯一能区分两种读法的用例，回归哨兵） |
| 电池+有活+在线 | hold |
| 电池+无活+在线 | 不 hold |
| 电池+有活,活干完后 15 分钟 | 释放 |
| 未配置 provider 也无 relay，但有网卡+默认路由 | `online=true`,照常按电源/活判定 |
| 无任何非 loopback 网卡 | `online=false`,不 hold |
| 只有 `utun*` / `awdl*` / `llw*` 接口 UP，网关不可达 | `online=false`（**不被隧道/空投接口误导**，回归哨兵） |
| 有 `en0`/`ens5`/`wlan0` 且 UP 且有地址,网关可达 | `online=true`（**不依赖接口名白名单**） |
| provider 服务不可达但默认网关可达 | `online=true`（**不被 provider 故障误导**，回归哨兵） |
| 网关 `connect` 成功 | `online=true` |
| 网关返回 `ECONNREFUSED`（端口关闭） | `online=true`（**链路通,这是最常见的情形**，回归哨兵） |
| 网关探测超时 / `ENETUNREACH` | `online=false` |
| 默认路由解析失败 | 退化为按网卡判定,不 panic |
| 同时有 en0 与 utun 两条默认路由 | 取 `route -n get default` 的 en0 网关,探测可用 |
| VPN 为唯一出口(网关是 `link#28`) | 解析不到 IP → 兜底偏 `online=true`,**不误判离线** |
| 只有 `en1`~`en8` 这类无地址接口 UP | 不算候选（**`IFF_UP` 不够,必须有 `AF_INET` 地址**,回归哨兵） |
| 网络探测 2s 超时 | 计为离线,不 panic |
| 开关关闭 | 立即释放,不等窗口 |
| 开关从关改开（指纹变化） | 下一拍重新持有,不需要重启进程 |
| settings.json 损坏 | 保留 last-known-good,不因坏文件丢掉用户的选择 |
| tick 迟到 5 分钟 | 仍按时间戳判定,不因「少了几拍」误释放 |
| **睡眠 8 小时后醒来,条件已不成立** | **不 hold**（睡眠感知时钟让 `since(last_ok)=8h15m`;若误用 `Instant::now()` 会读成几分钟而 hold,这是回归哨兵） |
| 睡眠后条件仍成立 | hold,且 `last_ok` 从醒来的时刻重新起算 |
| 睡眠 8 小时后 `last_remote_touch` 是睡前 1 分钟 | `remote_touch = false`(睡眠感知时钟让 `since()` 远超 5 分钟;**不能凭 Instant 认为"刚刚有交互"**) |
| 睡眠中条件仍成立(插电+有网) | `hold = true`,醒来后 `since(last_ok)` 由下一次 `ok` 刷新归零(**不因睡得久而误释放**) |
| 连续多次短睡(每次 3 分钟) | 每次睡眠都计入宽限,**累计后照常释放;用对时钟时 `slept_for` 恒为 0,不会被误判成时钟故障** |
| **单调钟冻结**(`since()` 恒为 0)且条件恒为 `ok=false` | (a) `slept_for > 5min && dt_mono*10 < slept_for` 连续 3 次即判 `mono_trusted=false` 改走墙钟;(b) 若仍未恢复,超过 `GRACE + 5min` **强制 Release**。**这是"永不睡"的唯一挡板**,回归哨兵 |
| 坏钟但 `dt_mono=20s`(非 0, 进程还被调度过) | 仍判 `mono_trusted=false`（**绝对阈值会漏判,必须用比例**,回归哨兵） |
| 正确钟睡 8 小时(`dt_mono=28800`) | **不**告警(`slept_for=0`;**别把正常睡眠误判成坏钟**,回归哨兵) |
| `clock_gettime` 启动即失败 | `mono_trusted=false`,退化为墙钟单基准,不 panic |
| 墙钟被 NTP 向后跳 | 不产生负数差值,不 panic |
| 电源为 UPS | `on_ac = true`（不被当成电池） |
| 电源为未知类型 | `on_ac = true`（偏向保活） |
| 处于 Dark Wake 期间 | 断言无效果（SDK 已声明）,**不视为失败**;出 Dark Wake 后恢复 |
| 子 agent 在 daemon 里跑（`running`） | `busy = true`（走 `ListTaskSummaries{active_only:true}` 再按 state 过滤） |
| daemon 里只有 `queued` 任务 | `busy = false`（**排队不算在跑**，回归哨兵） |
| daemon 里只有 `paused` 任务 | `busy = false`（**暂停不算在跑**，回归哨兵） |
| daemon 里只有 `waiting_for_permission` | `busy = true`（等审批也算在推进） |
| 会话状态是 `AwaitingApproval` | `busy = true`（**等人批准不等于做完了**，回归哨兵） |
| daemon 里只有 `blocked` 任务 | `busy = false`（**阻塞等人不算在跑**，回归哨兵） |
| daemon 里出现未知新状态 | 按 `busy = false` 处理并记日志,不 panic |
| 状态清单与 `TERMINAL_TASK_STATES` 不一致 | 复用同一个常量,不抄第二份 |
| daemon 不在 / socket 超时 | `busy = false`,不 panic,不因探测而拉起 daemon |
| daemon 收下请求但永不回复 | 500ms 超时后 `busy = false`,**主循环不被拖住**(回归哨兵) |
| 某项目从没在本进程打开过 | 不出现在 `runtimes` 里,不影响判定,**不扫描磁盘找 socket** |
| 多个项目各有 daemon | 逐个查,**任一**有 running 类任务即 `busy = true` |
| 远端连接挂着但 6 分钟无请求 | `remote_touch = false` |
| stdio 本地客户端持续发请求 | **不刷新** `remote_touch`（**桌面自己发的不算远端交互**,回归哨兵） |
| WS 客户端 1 分钟前发过请求 | `remote_touch = true` |
| 断言 `Drop` | 释放被调用一次且仅一次 |
| 非 macOS | 编译通过,`hold` 路径为空操作 |

`clock`、`assertion`、两个 probe 都通过 trait 注入假实现，因此上面每一行都能在
Linux CI 上跑。macOS 真实 FFI 只保留一个 `#[ignore]` 的 smoke test。

「子 agent 在 daemon 里跑」那条**不需要真 daemon**：`server.rs:7908` 已有
`fake_daemon` 夹具（现被 `watch_children` 等 6 处测试复用），它按请求脚本回
`IpcResponse`，喂 `TaskSummaries` 一条非空任务即可。这条用例的价值正是把
「只按 `threads` 判 busy」的回归钉死。

**「睡眠 8 小时后醒来」那一条是本设计最关键的回归哨兵**：它必须能在假时钟下
构造（给 `policy` 喂一个「跳了 8 小时」的 `now`），无需真的睡。若实现里有人把
单调时钟换回 `Instant::now()`，这条会立刻变红——这是唯一能挡住那类回归的测试。
「provider 不可达但网关可达」那条同理，是防止网络判定退化的哨兵。

## 验收（真机）

```bash
# 1. 断言确实持有,且类型正确
pmset -g assertions | grep -A2 "yi-agent"
# 期望: PreventUserIdleSystemSleep named: "yi-agent: ..."
# 不应出现 PreventSystemSleep / PreventUserIdleDisplaySleep

# 2. 断网 15 分钟内仍持有,之后消失
#    (关闭 Wi-Fi,分别在 5 分钟和 20 分钟时各跑一次上面那条)

# 3. 拔电 + 无活 15 分钟后消失;拔电 + 有活在跑时新增

# 4. 睡眠感知:睡一会儿再醒,确认没有"醒来后又白扛 15 分钟"
#    睡前记下 pmset -g assertions 的时间戳,醒来后跑:
pmset -g log | grep -E "Entering Sleep|Wake from" | tail -4
# 若醒来后断言仍在且条件已不成立 → 时钟基准错了(见「时间源的选择」)
```

**验收前必须知道的一个本机事实**：本机 `pmset -g custom` 是
`Battery Power: sleep 1`（电池下空闲 1 分钟就睡）、`AC Power: sleep 0`（插电
永不睡）。这带来一个验证上的陷阱：

- **插电时系统本来就不睡**，所以「持着断言但机器没睡」无法证明功能有效。
  真正能证明的只有断言本身的出现与消失，因此验收以 `pmset -g assertions`
  为准，不以「机器有没有睡」为准。
- **电池场景验证最直接**：拔电、不碰键盘，1 分钟内系统就该进入睡眠——除非
  我们的断言在。这是唯一能端到端复现原始故障的验收方式。
- 本 spec **不改** `pmset` 配置。所以「插电时 sleep 0」是用户自己的设置，
  不是本功能的效果；别把已有的 `sleep 0` 误当成验收通过。

## 风险

- **断言不释放会让整机不睡**。这是本设计最严重的失败模式，且用户往往毫无察觉。
  缓解：15 分钟窗口本身是兜底；`IOPMAssertion` 随进程退出回收，不会跨进程残留；
  开关可立即释放；`pmset -g assertions` 可肉眼检查。**不采用**「长期持有，只在
  退出时释放」的简化实现。
- **反向失败：该保活时没保活**。因为判据里多了一道 `online`，任何网络判定错误都会
  直接表现为「本该干活却睡了」。这是本设计里**后果更严重**的方向（用户丢的是活，
  不是电），所以判据一律偏向"有网"：UPS 算插电、未知电源算插电、无网卡才算离线、
  provider 不可达不影响判定。代价是可能在一个其实没网的环境里多扛 15 分钟。
- **IOKit FFI 的 `unsafe` 面**。仓库已有先例（`yi-agent-tools/src/process_group.rs`、
  `yi-agent-store/src/ipc.rs`、`yi-agent-supervisors/src/daemon.rs`），不算破例。
  但 FFI 只在 `clock` / `assertion` / `probe` 三个薄模块里，`policy` 完全不碰，
  把不可测面积压到最小。
- **时间基准选错是静默失效**。用错时钟不会报错、不会 panic，只会让 15 分钟窗口
  在睡眠后失效，而症状（"醒来后多扛 15 分钟"）几乎无法从日志看出来。缓解：测试表里
  那条「睡眠 8 小时后醒来」的哨兵用例；`clock` 单独成模块并有 13 行动机注释。

- **单调钟冻结 → 宽限永不到期 → 整机永不睡**。这是比上一条更坏的失效方向，且
  很容易被"偏向保活"的直觉掩盖：若 `clock.since()` 恒返回 0（平台不支持所选时钟、
  FFI 持续失败后退化成了一个不前进的基准），`since(last_ok)` 永远是 0，
  `hold` 永远是 `true`，
  **释放路径彻底不可达**。我在模拟里验过这条路径确实存在（`d_mono` 恒为 0 且条件
  恒为 `ok=false` 时，`since(last_ok)` 恒为 0，`hold` 恒为真）。

  两层兜底，缺一不可：

  1. **`clock` 必须能自证失效**。启动时调一次 `clock_gettime`，失败即置
     `mono_trusted = false`，此后一律退化为墙钟（`SystemTime`）单基准。墙钟在睡眠
     时照常前进，是这条兜底的正确退化项——它的缺点（NTP 跳变）比"永不睡"轻得多。

     运行期自检的判据要小心，**不能写成"墙钟前进了很大一段就判时钟坏了"**：
     时钟选对时，睡 8 小时醒来 `dt_mono` 也是 28800s，`slept_for` 为 0（见上面
     策略一节的对比表）。真正说明选错的特征恰恰相反——**墙钟前进了一整段时长，
     而 `dt_mono` 几乎没动**。

     判据写成比例，不要写成绝对阈值。我先写的是 `slept_for > 5min && dt_mono < 2×tick`，
     用模拟一验就有边界漏判：坏钟场景 `dt_mono = 20s`（不是 0，因为进程还被调度过
     几次）配 tick = 10s 时，`20 < 20` 为假，**坏钟被漏掉**。改成比例后没有这个
     刻度依赖：

     ```
     suspect = slept_for > 5min && dt_mono * 10 < slept_for
     // std 的 Duration 实现了 Mul<u32>, dt_mono * 10 可直接写, 无需转数值
     ```

     含义是「单调钟前进量不到墙钟超额的十分之一」，等价于"它基本没跟着走"。
     用模拟逐场景验证过：正确钟（含睡 8 小时、高负载 50s tick）全部不告警；
     坏钟在 `dt_mono = 0 / 20 / 25` 三种取值下全部告警。连续 3 次才置
     `mono_trusted = false`，避免单次抖动就切换。

     已知代价：墙钟被 NTP 向前跳 1 小时（而进程没睡）会命中这条，被误判成坏钟。
     后果只是切到墙钟基准，而墙钟本来就是可用基准，行为不变——所以这个误报无害，
     不需要为它加特判。

  2. **`hold` 必须有硬上限**。给一个独立于 `policy` 的看门狗：若 `hold` 已连续为
     `true` 超过 `GRACE + 5min` **且**期间单调钟读数没有增长过一次，说明时钟坏了，
     强制 `Release` 并打一条 `error` 日志。宁可睡一觉（醒来可续），也不要静默地让
     整机永不睡——后者用户完全无法察觉。

     这里看「单调钟原始读数没增长」而不是「`since(last_ok)` 没增长」：后者本身就是
     由时钟算出来的，时钟坏了两者都不会增长，判据等价；但直接看原始读数更不容易
     被上层逻辑错误伪装。

  这两条都写进了测试表（「单调钟冻结」那一行）。
- **多个 yi-agent 实例**（正式版 + 调试实例）。各自持各自的断言，系统做并集；
  任一实例判定 hold 就整体不睡。这是可接受的行为，但排查时要记得看所有 pid。
- **每 10s 一次 daemon socket 往返**用于 `ListTaskSummaries`。daemon 卡住时的超时
  必须小于 tick，否则 tick 被拖慢、`last_ok` 刷新变稀疏。探测是纯辅助信息，
  超时按 `false` 处理即可，**绝不能阻塞主循环**。

## 后续（不在本 spec 内）

「醒来后自动续跑」需要单独一份 spec：

- 现在的 `Stream` 不可重试是一条**刻意的**设计（`agent.rs:126-133`）,改成可重试
  会把「连接重置」也纳入重试,必须先把「流中断」和「连接重置」分成两类。
- 需要 turn 级 checkpoint：已完成的工具往返已在 session 里,缺的是「续跑时如何
  重新发起 provider 请求并保持工具调用配对合法」。
- 验证需要真实的流中断注入,与防睡的验证方式完全不同。
