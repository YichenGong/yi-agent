//! 10s tick：把 clock/policy/probe/assertion 串起来。
use std::sync::Arc;
use std::time::Duration;

use crate::assertion::Assertion;
use crate::clock::Clock;
use crate::policy::{Decision, Policy, Snapshot};
use crate::probe::{NetProbe, NetworkState, PowerProbe};

/// 是否「刚进入未知」。只在状态边沿记日志：持续 Unknown 不该每 10 秒刷屏，
/// 恢复后再未知才算一次新的边沿。
fn entered_unknown(previous: Option<NetworkState>, next: NetworkState) -> bool {
    next == NetworkState::Unknown && previous != Some(NetworkState::Unknown)
}

pub struct Supervisor {
    clock: Arc<dyn Clock>,
    power: Box<dyn PowerProbe>,
    net: Box<dyn NetProbe>,
    assertion: Box<dyn Assertion>,
    policy: Policy,
    /// 上一拍采到的网络状态，仅用于 Unknown 边沿日志。
    last_network_state: Option<NetworkState>,
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
            last_network_state: None,
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

        // 每拍只采一次网络状态：没有主动探测，这次调用只是本地 FFI 快照。
        let state = self.net.state();
        if entered_unknown(self.last_network_state, state) {
            eprintln!(
                "[yi-agent-power] network state is unknown (no trustworthy interface snapshot); \
                 keeping the sleep assertion eligible while other conditions hold"
            );
        }
        self.last_network_state = Some(state);

        let snapshot = Snapshot {
            on_ac: self.power.on_ac(),
            // 只有确认离线才不给资格；Unknown 仍是资格。策略本身不知道采样错误。
            online: state.eligible(),
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

    /// 可切换的三态假探针：测试要在同一 supervisor 上跨过状态边沿。
    struct FixedNet(std::sync::Mutex<NetworkState>);

    impl FixedNet {
        fn new(state: NetworkState) -> Self {
            Self(std::sync::Mutex::new(state))
        }
        fn set(&self, state: NetworkState) {
            *self.0.lock().unwrap() = state;
        }
    }

    // 与测试里 `impl Assertion for Arc<CountingAssertion>` 同一手法：
    // 让 `Arc` 本身成为探针，测试才能在外面切换状态。
    impl NetProbe for Arc<FixedNet> {
        fn state(&self) -> NetworkState {
            *self.0.lock().unwrap()
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
        state: NetworkState,
        assertion: Arc<CountingAssertion>,
    ) -> Supervisor {
        Supervisor::new(
            clock,
            Box::new(FixedPower(on_ac)),
            Box::new(Arc::new(FixedNet::new(state))),
            Box::new(assertion),
            true,
        )
    }

    /// 需要跨网络状态边沿测试的夹具：网络探针是可切换的 `Arc`。
    fn supervisor_with_switchable_net(
        clock: Arc<FakeClock>,
        on_ac: bool,
        state: NetworkState,
        assertion: Arc<CountingAssertion>,
    ) -> (Supervisor, Arc<FixedNet>) {
        let net = Arc::new(FixedNet::new(state));
        let sup = Supervisor::new(
            clock,
            Box::new(FixedPower(on_ac)),
            Box::new(net.clone()),
            Box::new(assertion),
            true,
        );
        (sup, net)
    }

    #[test]
    fn holding_then_ticking_again_does_not_touch_the_ffi() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
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
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
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
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
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
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
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
            let mut sup = supervisor_with(
                Arc::clone(&clock),
                true,
                NetworkState::Online,
                Arc::clone(&assertion),
            );
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
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Offline,
            Arc::clone(&assertion),
        );
        sup.tick(false, true);
        assert!(!sup.is_holding());
    }

    /// 电池 + 有活 → 持有；活干完 15 分钟后释放。
    #[test]
    fn battery_work_finishing_releases_after_the_grace() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            false,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
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
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            false,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
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

    // ---- P1：三态网络资格在 Supervisor 里的行为 ---------------------------

    /// Unknown 与 Online 一样提供资格：busy + 电池下两拍都持有（N5）。
    #[test]
    fn sleep_hardening_unknown_network_still_holds() {
        for state in [NetworkState::Online, NetworkState::Unknown] {
            let clock = FakeClock::new();
            let assertion = CountingAssertion::new();
            let mut sup = supervisor_with(Arc::clone(&clock), false, state, Arc::clone(&assertion));
            sup.tick(true, true);
            assert!(sup.is_holding(), "{state:?} with busy must hold");
            clock.advance(Duration::from_secs(10));
            sup.tick(true, true);
            assert!(
                sup.is_holding(),
                "{state:?} must keep holding without a new edge"
            );
            assert_eq!(
                assertion.acquires.load(Ordering::SeqCst),
                1,
                "{state:?}: only the hold edge touches the FFI"
            );
        }
    }

    /// 确认离线（Offline）后进入宽限，满 15 分钟释放。
    #[test]
    fn sleep_hardening_offline_network_releases_after_the_grace() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let (mut sup, net) = supervisor_with_switchable_net(
            Arc::clone(&clock),
            false,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
        sup.tick(true, true);
        assert!(sup.is_holding());

        net.set(NetworkState::Offline);
        clock.advance(Duration::from_secs(899));
        sup.tick(true, true);
        assert!(sup.is_holding(), "grace not yet elapsed");
        clock.advance(Duration::from_secs(1));
        sup.tick(true, true);
        assert!(!sup.is_holding(), "offline past the grace must release");
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);
    }

    /// Unknown 不是无条件保活：开关关、无活、电池，三条任一都仍能阻止持有。
    #[test]
    fn sleep_hardening_unknown_does_not_bypass_the_other_conditions() {
        // enabled = false
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Unknown,
            Arc::clone(&assertion),
        );
        sup.tick(true, false);
        assert!(!sup.is_holding(), "disabled must release even when unknown");

        // busy = false 且电池：没有活可保，插电才算 ok
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            false,
            NetworkState::Unknown,
            Arc::clone(&assertion),
        );
        sup.tick(false, true);
        assert!(!sup.is_holding(), "no work on battery must not hold");

        // on_ac = false 但 busy = true 才是该持有的那一组（对照）
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            false,
            NetworkState::Unknown,
            Arc::clone(&assertion),
        );
        sup.tick(true, true);
        assert!(
            sup.is_holding(),
            "unknown + busy on battery is the eligible case"
        );

        // 插电：即使无活也持有（策略非回归，不是 Unknown 的特权）
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            true,
            NetworkState::Unknown,
            Arc::clone(&assertion),
        );
        sup.tick(false, true);
        assert!(sup.is_holding(), "on AC holds regardless of busy");
    }

    /// N6：网络采样每拍一次即可——线缆恢复后的下一拍就能重新获得资格。
    #[test]
    fn sleep_hardening_network_recovery_regains_eligibility_on_next_tick() {
        let clock = FakeClock::new();
        let assertion = CountingAssertion::new();
        let (mut sup, net) = supervisor_with_switchable_net(
            Arc::clone(&clock),
            false,
            NetworkState::Offline,
            Arc::clone(&assertion),
        );
        sup.tick(true, true);
        assert!(!sup.is_holding(), "offline never acquires");

        net.set(NetworkState::Online);
        clock.advance(Duration::from_secs(10));
        sup.tick(true, true);
        assert!(sup.is_holding(), "recovered link holds on the next tick");
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);
    }

    // ---- N8：Unknown 只在边沿记日志 -------------------------------------

    #[test]
    fn sleep_hardening_entered_unknown_logs_only_on_the_edge() {
        assert!(
            entered_unknown(None, NetworkState::Unknown),
            "first sample unknown is an edge"
        );
        assert!(
            !entered_unknown(Some(NetworkState::Unknown), NetworkState::Unknown),
            "persistent unknown must not log every tick"
        );
        assert!(
            entered_unknown(Some(NetworkState::Online), NetworkState::Unknown),
            "online -> unknown is an edge"
        );
        assert!(
            entered_unknown(Some(NetworkState::Offline), NetworkState::Unknown),
            "offline -> unknown is an edge"
        );
        assert!(
            !entered_unknown(Some(NetworkState::Unknown), NetworkState::Online),
            "leaving unknown is not an entering edge"
        );
        assert!(
            !entered_unknown(Some(NetworkState::Online), NetworkState::Offline),
            "known -> known is not an edge"
        );
    }

    /// 边沿之后恢复、再次未知要重新记一次：状态本身是「上一次采样」，
    /// 不是「曾经未知过」。
    #[test]
    fn sleep_hardening_unknown_edge_after_recovery_logs_again() {
        let mut previous = None;
        let mut edges = 0;
        for state in [
            NetworkState::Unknown,
            NetworkState::Unknown,
            NetworkState::Online,
            NetworkState::Unknown,
        ] {
            if entered_unknown(previous, state) {
                edges += 1;
            }
            previous = Some(state);
        }
        assert_eq!(edges, 2, "unknown, unknown, online, unknown => two edges");
    }

    /// B6：后台 busy 是**普通 busy 源**——它不为 Supervisor 引入任何新语义。
    ///
    /// 只要 `busy=true` 且网络合格、在电池上，两拍都持有；后台工作到终态后
    /// `busy=false`，宽限未到仍持有、等于宽限释放；若另有 busy 源仍为 true，
    /// 则继续持有（不因后台结束而释放）。
    ///
    /// 这里用 FakeClock 推进，**不**真实等待 15 分钟。
    #[test]
    fn sleep_hardening_background_busy_follows_the_grace_boundary() {
        // 两拍 Hold（后台 busy 与 online+battery 同形）。
        let clock = Arc::new(FakeClock::new());
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            false,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
        sup.tick(true, true);
        assert!(sup.is_holding(), "background busy on battery must hold");
        clock.advance(Duration::from_secs(10));
        sup.tick(true, true);
        assert!(sup.is_holding(), "still busy => keep holding");
        assert_eq!(assertion.acquires.load(Ordering::SeqCst), 1);

        // 后台终态：busy=false，宽限未到仍持有。
        clock.advance(Duration::from_secs(899));
        sup.tick(false, true);
        assert!(sup.is_holding(), "grace has not elapsed yet");
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 0);

        // 恰好等于宽限 => 释放（判据是 `<`）。
        clock.advance(Duration::from_secs(1));
        sup.tick(false, true);
        assert!(
            !sup.is_holding(),
            "busy gone and grace elapsed must release"
        );
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 1);

        // 另一路 busy 仍为 true：不因后台结束而释放。
        let clock = Arc::new(FakeClock::new());
        let assertion = CountingAssertion::new();
        let mut sup = supervisor_with(
            Arc::clone(&clock),
            false,
            NetworkState::Online,
            Arc::clone(&assertion),
        );
        sup.tick(true, true);
        assert!(sup.is_holding());
        clock.advance(Duration::from_secs(10));
        // 后台结束，但会话仍在跑 turn：聚合后的 busy 仍是 true。
        sup.tick(true, true);
        clock.advance(Duration::from_secs(3600));
        sup.tick(true, true);
        assert!(sup.is_holding(), "another busy source keeps it holding");
        assert_eq!(assertion.releases.load(Ordering::SeqCst), 0);
    }
}
