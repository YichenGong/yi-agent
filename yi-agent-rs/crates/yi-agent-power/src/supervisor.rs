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
