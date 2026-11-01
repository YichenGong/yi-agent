//! 纯策略状态机：零 I/O，全部可单测。
use std::time::Duration;

pub const GRACE: Duration = Duration::from_secs(15 * 60);
pub const WATCHDOG_SLACK: Duration = Duration::from_secs(5 * 60);

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
            if !mono_grew && held_for > GRACE + WATCHDOG_SLACK {
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
}
