# yi-agent-power

防空闲休眠：平台电源断言 + 纯策略状态机。设计见
`docs/superpowers/specs/2026-11-01-prevent-idle-sleep-design.md`，实现计划见
`docs/superpowers/plans/2026-11-01-prevent-idle-sleep.md`。

本 crate 只包含纯逻辑层（策略、时钟、探针、平台断言、10s 拍状态机），不含任何
进程装配代码——谁持有断言、多久拍一次、busy 从哪来，都是调用方
（`yi-agent-app-server`）的事。于是这里每一行都能在进程内单测。

## 完成情况

- [x] `policy` 纯状态机：`ok = online && (on_ac || busy)`，单向 15 分钟宽限
      （`GRACE`），边界取 `<`；时钟冻结看门狗 `GRACE + WATCHDOG_SLACK(5min)`。
      入参是双时钟 `Sample { mono, wall }`：宽限用 `mono`，看门狗扛多久用
      `wall`、判「时钟没前进」用 `mono`。验证：
      `cargo test -p yi-agent-power policy`（16 个用例）
- [x] `clock` 睡眠感知时钟：macOS `CLOCK_MONOTONIC_RAW`、Linux
      `CLOCK_BOOTTIME`；比例判据 `dt_mono * 10 < slept_for` 的墙钟自检，
      连续 3 次降级且单向不可恢复。公开 `Stamp::from_duration` /
      `Stamp::as_duration` 供跨 crate 使用。验证：
      `cargo test -p yi-agent-power clock`
- [x] `assertion` IOKit `PreventUserIdleSystemSleep`，`Drop` 幂等释放；
      非 macOS no-op。验证：`cargo test -p yi-agent-power assertion`；
      真机 `cargo test -p yi-agent-power assertion -- --ignored`
- [x] `probe` 电源（UPS/未知算插电）与网络**被动双栈资格**（2026-11-03 起：
      一次 `getifaddrs` 快照，不再 `route`/TCP 80 主动探测）。三态
      `NetworkState::{Online,Offline,Unknown}`：`Unknown`（采样失败，无可信快照）
      也提供资格，成功但无合格接口才是 `Offline`。验证：
      `cargo test -p yi-agent-power probe`
- [x] `supervisor` 10s 拍状态机，只在迁移边沿调 FFI（`Hold` 边沿才
      `acquire`，`Idle` 与 `Drop` 都 `release`）。验证：
      `cargo test -p yi-agent-power supervisor`
- [x] `power.prevent_idle_sleep` 开关，四处读取点同步（`FIELDS` /
      `PowerSettings` / `settings_rpc::current_value` / `web::api::field_value`）。
      默认开，`false`（大小写不敏感）关，未知值与坏文件回落默认开。旧键
      `agent.prevent_idle_sleep` 在 `serde_json::Value` 阶段迁到新组。
      验证：`cargo test -p yi-agent-runtime prevent_idle_sleep`
- [x] app-server 主循环接线：10s `power_ticker`、三路 busy 并集（本地会话 /
      项目 daemon / 远端近期交互）、非 local 客户端刷新远端时间戳；
      整条 power tick 在 `power_guard` 独立线程里跑。验证：
      `cargo test -p yi-agent-app-server power_busy`
- [x] 休眠参数进设置界面：`power` 分组开放「条件消失后再保活多久」
      （1–120 分钟）与「远端交互有效窗口」（1–60 分钟），开关迁到
      `power.prevent_idle_sleep`
  - 字段定义：`yi-agent-rs/crates/yi-agent-runtime/src/settings.rs` 的 `FIELDS`
    （`power.idle_grace_minutes` / `power.remote_touch_minutes`）
  - 窗口参数化：`yi-agent-rs/crates/yi-agent-power/src/policy.rs` 的
    `Policy::with_windows` / `set_windows`，`decide` 每拍现读字段，故缩窗下一拍
    即生效；返回 `Idle` 时同时清 `last_ok`，避免调长窗口让已释放的 hold 复活。
    判据 `cargo test -p yi-agent-power policy`
  - 看门狗余量随宽限缩放：
    `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs` 的
    `slack_for(grace) = max(5min, grace/3)`。判据
    `cargo test -p yi-agent-app-server power_guard`
  - 迁移：同文件 `migrate_legacy_keys`；判据 `cargo test -p yi-agent-runtime migrate`
  - 跨字段拒绝：同文件 `write_to`（远端窗口 > 宽限时返回 `InvalidSetting`，
    零写入）；只在本次 patch 触碰这两个键时拒绝，否则既有倒挂会把用户锁在
    文件外面。判据 `cargo test -p yi-agent-runtime power_windows`
  - 前端分组：`desktop/src/components/SettingsRuntimeTab.tsx`；判据
    `npx vitest run src/components/SettingsRuntimeTab.test.tsx`

## 待实现加固（2026-11-03）

设计：[任务运行防休眠加固 spec](../superpowers/specs/2026-11-03-idle-sleep-hardening-design.md)；
计划：[实施 plan](../superpowers/plans/2026-11-03-idle-sleep-hardening-impl.md)。仅文档审查完成，以下产品需求尚未实现。

- [x] P1 被动双栈链路资格，移除 route/TCP 80 判据。完成判据（自动化已通过）：
      `cargo test -p yi-agent-power sleep_hardening_` 19 项，其中
      `probe::tests::sleep_hardening_ipv6_only_is_online` /
      `_usable_single_addresses_are_online`（v4 公网、v4 私网、v6 global、v6 ULA）/
      `_unusable_addresses_do_not_qualify`（N3 地址）/ `_interfaces_without_a_live_link_are_offline`
      （N4 DOWN、只有 UP、loopback、utun/awdl/llw/bridge）/
      `_successful_empty_snapshot_is_offline`（N5）/ `_sockaddr_in_bytes_round_trip`、
      `_sockaddr_in6_bytes_round_trip`、`_same_interface_keeps_both_families`、
      `_snapshot_holder_releases_exactly_once`（N7）/
      `supervisor::tests::sleep_hardening_unknown_network_still_holds`、
      `_unknown_does_not_bypass_the_other_conditions`、
      `_offline_network_releases_after_the_grace`、
      `_entered_unknown_logs_only_on_the_edge`、`_unknown_edge_after_recovery_logs_again`（N8）。
      保 API 变异证据：把 `network_state` 改成只认 IPv4（旧 `has_v4` 行为）后，
      `sleep_hardening_ipv6_only_is_online`、`_usable_single_addresses_are_online`、
      `_one_qualifying_interface_is_enough` 三条失败（`2001:db8::1234 alone must be online`），
      恢复后 19/19 绿。
      词义边界：资格 = **本机存在可用本地链路**，不表示互联网/provider 可达；局域网有地址、
      上游断开时仍保活。虚拟/旁路前缀（utun/awdl/llw/bridge/anpi/ap/tun/tap/docker/br-/veth/virbr/lo）
      保持原规则——当它们是唯一出口时仍判 Offline，本轮不解决该拓扑。
      真机网络边界（IPv6-only、网关 80 丢包、Wi-Fi 断开）仍待 Task 7 人工验收。
- [ ] P2 RPC/插件/cold lead 启动失败按 turn_id 回滚并收口；完成判据：plan Tasks 3–4 的 T1–T7，`cargo test -p yi-agent-app-server --lib sleep_hardening_`，warm 排队语义不回归。
- [x] P3 一致输入与可通知电源线程、真实已持断言停机测试。完成判据（自动化已通过）：
      `cargo test -p yi-agent-app-server --lib sleep_hardening_` 6 项
      （`power_guard::tests::sleep_hardening_` 前缀）：
      `drop_releases_once_without_waiting_a_tick`（G1：enabled=true 且已 acquire 后 Drop，
      预算 1 秒、release 恰一次）/ `disable_wakes_the_worker_and_skips_the_grace`（G2）/
      `drop_during_a_blocked_tick_waits_then_stops`（G3：Barrier 卡住一次本地调用后
      另线程 Drop，调用返回后不再新拍、释放一次）/
      `repeated_identical_updates_do_not_tick`（G4：100 次相同 update 不额外 tick）/
      `grace_changes_reach_the_policy`（G5）/ `spawn_and_drop_are_bounded`。
      实现：`PowerGuard::spawn(enabled, grace_secs)` + `update(busy, enabled, grace_secs)`
      一次提交一致快照；`Mutex<GuardInputs>` + `Condvar::wait_timeout_while`，
      谓词同时处理「输入换版 / stop / 伪唤醒」，锁不跨任何 probe 或 FFI。
      变异证据：`update` 去掉 `notify_one` → `repeated_identical_updates_do_not_tick` 红；
      等待改回纯 `wait_timeout`（忽略谓词）→ `disable_wakes...`（"disable must release
      immediately"）与 `grace_changes...`（Timeout）红；恢复后 6/6 绿。
      预算语义：1 秒/2 秒是**测试预算**，不是 OS 调度或系统调用永不挂起的硬实时承诺；
      stop 落进一次正在执行的本地 tick 时，join 等该调用返回。
      真机 `#[ignore] disabling_the_guard_releases_the_real_assertion` 仍需手动
      `--ignored` 跑并配合 Task 7 人工验收，本轮不声称 IOKit 实证。
- [ ] P4 显式后台进程意图，默认false、活跃才参与busy；完成判据：plan Tasks 5–6 的 B1–B8，tools/app-server `sleep_hardening_` 与 desktop `npx tsc --noEmit && npx vitest run`。

这四项只在本模块计数，tools/app-server 交叉引用不重复增加需求数。通过 spec/plan 不表示产品修复；原已完成项记录基线行为，实施后再同步替换旧探针说明。

## 非目标

- 不实现唤醒后续跑（需独立 spec）。
- 不阻止合盖 / 手动睡眠 / 系统更新重启。
- 不为孤儿 daemon 保活（宿主 app-server 退出即无断言）。
- 不覆盖 `yi-agent run` / TUI 入口（交互式，人就在键盘前）。
- 不覆盖 Linux / Windows 的真实断言（编译成 no-op）。

## 已知边界

- `daemon_busy` 只看**已挂载过**的项目（`ProjectRuntimes` 里的 binding）。看板在
  用户从没在本进程打开过的项目上推卡片时探测不到；为覆盖率去扫磁盘上所有
  `runtime.sock` 会把 10s tick 撑坏。
- `state_counts_as_busy` 用白名单（只认 `running` / `waiting_for_*`），未知新状态
  一律按「不算」——判错的代价是睡一觉（醒来可续），反过来是整夜不睡且无人察觉。
