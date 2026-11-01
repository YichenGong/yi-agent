//! 10s tick：把 clock/policy/probe/assertion 串起来。
use std::sync::Arc;
use std::time::Duration;

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

    /// 运行时换窗口。由 `PowerGuard` 的线程每拍下发，值与设置保持一致。
    ///
    /// 只改 `Policy` 的字段，不触发任何 I/O；断言是否释放仍由下一拍的
    /// `tick` 决定——所以「缩短宽限立即生效」不需要在这里补逻辑。
    pub fn set_windows(&mut self, grace: Duration, watchdog_slack: Duration) {
        self.policy.set_windows(grace, watchdog_slack);
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
}
