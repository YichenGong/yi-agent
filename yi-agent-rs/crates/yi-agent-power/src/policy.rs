//! 纯策略状态机：零 I/O，全部可单测。
use std::time::Duration;

/// 默认宽限：条件消失后再扛 15 分钟。
pub const DEFAULT_GRACE: Duration = Duration::from_secs(15 * 60);
/// 默认看门狗余量：见 `Policy::decide` 的看门狗分支。
pub const DEFAULT_WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);

/// 历史别名，保留是为了让既有的 `GRACE + WATCHDOG_SLACK` 计算（测试内两处）
/// 与语义引用不必改动。值必须与 `DEFAULT_*` 完全一致，有测试钉住。
pub const GRACE: Duration = DEFAULT_GRACE;
pub const WATCHDOG_SLACK: Duration = DEFAULT_WATCHDOG_SLACK;

/// 一拍的时间读数：两条时钟同时给出。
///
/// **为什么必须两条。** 宽限窗口要用「睡眠期间照常前进」的单调钟；而看门狗要
/// 检测的正是「这条单调钟不前进」。若看门狗的时长也用它来度量，单调钟冻结时
/// 差值恒为 0，判据永远不成立——兜底在最该生效的场景下失效。所以：
/// 时长由墙钟度量，增长由单调钟判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// 睡眠感知单调读数。宽限窗口用它。
    pub mono: Duration,
    /// 墙钟读数（自 `UNIX_EPOCH`）。只用于看门狗度量「扛了多久」。
    pub wall: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub on_ac: bool,
    pub online: bool,
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold,
    Idle,
}

impl Snapshot {
    fn keeps_awake(&self) -> bool {
        self.online && (self.on_ac || self.busy)
    }
}

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

    pub fn on_disabled(&mut self) -> Decision {
        self.disabled = true;
        self.last_ok = None;
        self.hold_since = None;
        Decision::Idle
    }

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
}

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

    /// 电池 + 有网：`keeps_awake == busy`。测「条件消失」只能用这个——
    /// `ac_online` 的 `on_ac` 恒为真，`busy` 参不参与判定，永远 Hold。
    fn battery(busy: bool) -> Snapshot {
        Snapshot {
            on_ac: false,
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

    /// 单调钟**完全冻结**（读数恒为 100）时，墙钟照样前进。
    /// 看门狗的时长必须由墙钟度量，否则 `now - hold_since` 恒为 0，永不释放。
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

    /// 单调钟在增长（时钟正常）时看门狗不该触发，哪怕 hold 持续远超 GRACE+SLACK。
    #[test]
    fn the_watchdog_stays_quiet_when_the_clock_advances() {
        let mut p = Policy::new();
        for t in (0..=3600).step_by(600) {
            assert_eq!(p.decide(&ac_online(true), at(t)), Decision::Hold, "t={t}");
        }
    }

    /// 单调钟增长过一点点就算「没冻结」：判据是「有增长」，不是「增长得够快」。
    /// 后者会把高负载下被调度变慢的正常时钟误判成坏钟。
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
}
