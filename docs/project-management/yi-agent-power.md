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
- [x] `probe` 电源（UPS/未知算插电）与网络（候选接口 + 网关探测，
      `ECONNREFUSED` 计为在线）。验证：`cargo test -p yi-agent-power probe`
- [x] `supervisor` 10s 拍状态机，只在迁移边沿调 FFI（`Hold` 边沿才
      `acquire`，`Idle` 与 `Drop` 都 `release`）。验证：
      `cargo test -p yi-agent-power supervisor`
- [x] `agent.prevent_idle_sleep` 开关，四处读取点同步（`FIELDS` /
      `AgentSettings` / `settings_rpc::current_value` / `web::api::field_value`）。
      默认开，`false`（大小写不敏感）关，未知值与坏文件回落默认开。验证：
      `cargo test -p yi-agent-runtime prevent_idle_sleep`
- [x] app-server 主循环接线：10s `power_ticker`、三路 busy 并集（本地会话 /
      项目 daemon / 远端 5 分钟内交互）、非 local 客户端刷新远端时间戳；
      整条 power tick 在 `power_guard` 独立线程里跑。验证：
      `cargo test -p yi-agent-app-server power_busy`

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
