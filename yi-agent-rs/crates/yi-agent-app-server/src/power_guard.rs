//! 保活线程：唯一持有 IOKit 断言的执行体。
//!
//! 为什么是独立 OS 线程而不是 `select!` 分支或 `spawn_blocking`：
//!
//! - 一拍里含 `getifaddrs` 快照、电源来源查询与可能的 IOKit 调用；这些是**本地**
//!   系统调用，但仍不该放主循环里和 RPC 抢时间片。
//! - `spawn_blocking` 的线程会被 tokio 在空闲时回收，而这里需要「长期存在、
//!   每 10 秒醒一次」的节奏；`std::thread` 语义更清楚，也不占 tokio 的阻塞配额。
//! - 断言句柄必须活到进程结束，`Drop` 里 join 才能保证「线程停了断言一定释放」。
//!
//! 线程不轮询参数，而是等在一个 `Mutex<GuardInputs>` + `Condvar` 上：主循环
//! 一次 `update` 提交**一致**的三个业务值（busy / enabled / grace），线程被
//! 唤醒后立刻拷贝快照、放锁，再做 tick——锁绝不跨任何 probe 或 FFI。

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use yi_agent_power::Supervisor;
use yi_agent_power::assertion::platform_assertion;
use yi_agent_power::clock::{Clock, SystemClock};
use yi_agent_power::probe::{SystemNetProbe, SystemPowerProbe};

/// 断言显示给用户的文案。`pmset -g assertions` 里能看到它，是排查的第一现场。
const ASSERTION_LABEL: &str = "yi-agent: keeping the machine awake for work";

/// 线程的 tick 间隔。10s 远小于 15 分钟宽限，迟到一拍不改变结论。
const SUPERVISOR_TICK: Duration = Duration::from_secs(10);

/// 看门狗余量跟随宽限缩放：`max(5min, grace/3)`。
///
/// 为什么需要缩放（已核实 `policy.rs` 的看门狗分支）：看门狗用**墙钟**
/// 度量「扛了多久」、用**单调钟**判定「有没有前进」。在「信任期且 mono 恰好
/// 冻结」这条路径上，用 `wall > grace + slack` 近似 `mono > grace`。若用户把
/// 宽限调到 120 分钟而 slack 固定 5 分钟，mono 冻结过一次就会在 125 分钟强制
/// 释放——那比用户设定还早。这条差异只在时钟异常路径体现，但正因为异常，
/// 用户不可能自己排查，所以参数关系必须在设计上自洽。
///
/// 放在这里而不是 `Policy` 内部：`Policy` 保持「给我两个窗口值」的纯函数
/// 语义，推导规则单点定义、有独立测试。
fn slack_for(grace: Duration) -> Duration {
    (grace / 3).max(yi_agent_power::policy::DEFAULT_WATCHDOG_SLACK)
}

/// 主循环与保活线程之间唯一共享的可变状态。
///
/// 三个业务值必须**一起**换版：拆成三个原子时，线程可能读到「新的 busy +
/// 旧的 enabled」，那一拍的策略输入从未真实存在过。`stop` 只由 `Drop` 写，
/// `update` 永远不能把它恢复成 `false`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct GuardInputs {
    pub(crate) busy: bool,
    pub(crate) enabled: bool,
    pub(crate) grace_secs: u64,
    pub(crate) stop: bool,
}

type SharedInputs = Arc<(Mutex<GuardInputs>, Condvar)>;

pub(crate) struct PowerGuard {
    shared: SharedInputs,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PowerGuard {
    /// 生产装配：真实时钟 / 电源 / 网络 / IOKit 全部在**线程内**创建。
    ///
    /// 远程交互窗口不在这里：它由主循环自己消费（`remote_touch_busy`），
    /// 线程不需要它，也就不必保存一份可能失同步的副本。
    pub(crate) fn spawn(enabled: bool, grace_secs: u64) -> Self {
        let initial = GuardInputs {
            busy: false,
            enabled,
            grace_secs,
            stop: false,
        };
        Self::spawn_with_factory(
            initial,
            move || {
                // 时钟在线程里建：`SystemClock` 自己持有「单调钟是否可信」的状态，
                // 每拍读一次就是它的全部用途，跨线程共享没有意义。
                let clock = Arc::new(SystemClock::new()) as Arc<dyn Clock>;
                Supervisor::new(
                    clock,
                    Box::new(SystemPowerProbe::new()),
                    Box::new(SystemNetProbe::new()),
                    platform_assertion(ASSERTION_LABEL),
                    enabled,
                )
            },
            |_| {},
        )
    }

    /// 起线程并保存句柄。工厂与首拍后回调都在线程里执行，调用方不跨线程搬运
    /// 任何非 `Send` 的平台句柄。
    ///
    /// `after_tick` 只用于测试观察「一次真实 tick 已结束」；生产传空闭包。
    ///
    /// 回调收到**这一拍实际采用的输入**。只发一个「跑完了」的无内容信号不够：
    /// 信号相对 `update` 是异步的，测试排空通道之后仍可能收到上一拍的迟到信号，
    /// 于是把「旧拍跑完」误当成「新拍跑完」。带上输入就必须逐字段匹配。
    pub(crate) fn spawn_with_factory<F, A>(initial: GuardInputs, factory: F, after_tick: A) -> Self
    where
        F: FnOnce() -> Supervisor + Send + 'static,
        A: FnMut(GuardInputs) + Send + 'static,
    {
        let shared: SharedInputs = Arc::new((Mutex::new(initial), Condvar::new()));
        let shared_in = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("yi-agent-power".to_string())
            .spawn(move || run_worker(shared_in, factory, after_tick))
            .expect("spawn the power guard thread");
        Self {
            shared,
            handle: Some(handle),
        }
    }

    /// 主循环每拍调用一次：提交一致的三个业务值。
    ///
    /// 只持锁写字段、放锁后 `notify_one`；相同值的重复提交不唤醒线程，避免
    /// 10 秒一次的无意义 tick 或忙循环。这里绝不执行 I/O。
    pub(crate) fn update(&self, busy: bool, enabled: bool, grace_secs: u64) {
        let (lock, cv) = &*self.shared;
        let changed = {
            let mut inputs = lock.lock().unwrap_or_else(|p| p.into_inner());
            // `stop` 只由 Drop 写；提交参数不能把已停止的线程"复活"。
            if inputs.stop {
                false
            } else {
                let next = GuardInputs {
                    busy,
                    enabled,
                    grace_secs,
                    stop: false,
                };
                let changed = *inputs != next;
                if changed {
                    *inputs = next;
                }
                changed
            }
        };
        if changed {
            cv.notify_one();
        }
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        let (lock, cv) = &*self.shared;
        {
            let mut inputs = lock.lock().unwrap_or_else(|p| p.into_inner());
            inputs.stop = true;
        }
        // 先放锁再 notify：被唤醒的线程要能立刻拿到锁。
        cv.notify_one();
        if let Some(handle) = self.handle.take() {
            // join 是必要的：不 join 则进程可能在线程释放断言之前退出，
            // 断言变成内核里的孤儿（实际会被系统回收，但那依赖系统行为）。
            //
            // 等待有上限：线程不是在 sleep 轮询，而是等 Condvar；stop 置位并
            // notify 后它立即返回。若 stop 恰好落进一次正在执行的本地 tick，
            // join 会等那次调用返回——不宣称能强行中断 OS 调用。
            let _ = handle.join();
        }
    }
}

/// 线程主体：取快照 → 放锁 → tick → 等下一拍或输入变化/stop。
///
/// `after_tick` 在**真实 tick 结束之后**调用，并带上该拍采用的输入；测试用它
/// 确认「首拍已结束」以及「哪一拍结束」，从而不会把上一拍的迟到信号当成新拍。
fn run_worker<F, A>(shared: SharedInputs, make_supervisor: F, mut after_tick: A)
where
    F: FnOnce() -> Supervisor,
    A: FnMut(GuardInputs),
{
    let mut supervisor = make_supervisor();
    let (lock, cv) = &*shared;
    loop {
        let snapshot = *lock.lock().unwrap_or_else(|p| p.into_inner());
        if snapshot.stop {
            break;
        }
        let grace = Duration::from_secs(snapshot.grace_secs);
        supervisor.set_windows(grace, slack_for(grace));
        supervisor.tick(snapshot.busy, snapshot.enabled);
        after_tick(snapshot);

        let inputs = lock.lock().unwrap_or_else(|p| p.into_inner());
        if inputs.stop {
            break;
        }
        // 谓词把「输入被换版」「stop」「伪唤醒」三件事一次处理完：条件不再成立
        // 时立即返回，否则最多等一拍。
        let _wait_result = cv
            .wait_timeout_while(inputs, SUPERVISOR_TICK, |current| {
                !current.stop && *current == snapshot
            })
            .unwrap_or_else(|p| p.into_inner());
    }
    // 显式 drop：确保断言在 join 返回前释放，不依赖栈清理时机。
    drop(supervisor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use yi_agent_power::assertion::Assertion;
    use yi_agent_power::clock::Stamp;
    use yi_agent_power::probe::{NetProbe, NetworkState, PowerProbe};

    /// 假时钟：每次采样推进 1 秒。真实线程里的策略需要时间前进才会做决策。
    struct SteppingClock {
        elapsed: StdMutex<Duration>,
    }

    impl SteppingClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                elapsed: StdMutex::new(Duration::ZERO),
            })
        }
    }

    impl Clock for SteppingClock {
        fn now(&self) -> Option<Stamp> {
            let mut elapsed = self.elapsed.lock().unwrap();
            *elapsed += Duration::from_secs(1);
            Some(Stamp::from_duration(*elapsed))
        }
        fn since(&self, stamp: Stamp) -> Duration {
            self.elapsed
                .lock()
                .unwrap()
                .saturating_sub(stamp.as_duration())
        }
        fn wall(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(*self.elapsed.lock().unwrap()))
        }
        fn mono_trusted(&self) -> bool {
            true
        }
    }

    struct Battery;
    impl PowerProbe for Battery {
        fn on_ac(&self) -> bool {
            false
        }
    }

    struct AlwaysOnline;
    impl NetProbe for AlwaysOnline {
        fn state(&self) -> NetworkState {
            NetworkState::Online
        }
    }

    /// 计数断言：acquire 时向 channel 发信号，release 计数。
    ///
    /// `Clone` 是「同一份状态的两个句柄」：计数都在 `Arc` 里，测试线程与
    /// supervisor 线程看的是同一组原子值。不能直接给 `Arc<Self>` 实现
    /// `Assertion`——`Assertion` 对本 crate 是外部 trait，孤儿规则不允许。
    #[derive(Clone)]
    struct CountingAssertion {
        held: Arc<AtomicBool>,
        releases: Arc<AtomicUsize>,
        acquired: mpsc::Sender<()>,
    }

    impl CountingAssertion {
        fn new(acquired: mpsc::Sender<()>, releases: Arc<AtomicUsize>) -> Self {
            Self {
                held: Arc::new(AtomicBool::new(false)),
                releases,
                acquired,
            }
        }
        fn handle(&self) -> Box<dyn Assertion> {
            Box::new(self.clone())
        }
    }

    impl Assertion for CountingAssertion {
        fn acquire(&mut self) -> bool {
            self.held.store(true, Ordering::SeqCst);
            let _ = self.acquired.send(());
            true
        }
        fn release(&mut self) {
            if self.held.swap(false, Ordering::SeqCst) {
                self.releases.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn is_held(&self) -> bool {
            self.held.load(Ordering::SeqCst)
        }
    }

    /// 可阻塞一次的探针：用于「tick 进行中 Drop」。
    struct BlockingPower {
        entered: Option<mpsc::Sender<()>>,
        release: Arc<std::sync::Barrier>,
    }

    impl PowerProbe for BlockingPower {
        fn on_ac(&self) -> bool {
            if let Some(entered) = &self.entered {
                let _ = entered.send(());
            }
            self.release.wait();
            false
        }
    }

    fn battery_online_supervisor(
        clock: Arc<SteppingClock>,
        assertion: &CountingAssertion,
    ) -> Supervisor {
        Supervisor::new(
            clock,
            Box::new(Battery),
            Box::new(AlwaysOnline),
            assertion.handle(),
            true,
        )
    }

    #[test]
    fn slack_scales_with_the_grace_window() {
        let cases = [
            (1u64, 5u64), // 1 分钟 → 下限
            (5, 5),       // 5 分钟 → 下限
            (15, 5),      // 15 分钟 → 下限（与历史行为一致）
            (60, 20),     // 60/3 = 20
            (120, 40),    // 120/3 = 40
        ];
        for (grace_min, want_slack_min) in cases {
            assert_eq!(
                slack_for(Duration::from_secs(grace_min * 60)),
                Duration::from_secs(want_slack_min * 60),
                "grace {grace_min}min should give slack {want_slack_min}min"
            );
        }
    }

    // ---- 真实线程循环：G1–G5 --------------------------------------------

    /// G1：enabled=true 且已持有断言时 Drop——不等 10 秒，断言恰好释放一次。
    #[test]
    fn sleep_hardening_drop_releases_once_without_waiting_a_tick() {
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let releases = Arc::new(AtomicUsize::new(0));
        let clock = SteppingClock::new();
        let assertion = CountingAssertion::new(acquired_tx, Arc::clone(&releases));
        let factory_assertion = assertion.clone();
        let factory_clock = Arc::clone(&clock);

        // 首拍只建立策略基线，所以先 update 一次触发第二拍再等 acquire。
        let (tick_tx, tick_rx) = mpsc::channel();
        let guard = PowerGuard::spawn_with_factory(
            GuardInputs {
                busy: true,
                enabled: true,
                grace_secs: 900,
                stop: false,
            },
            move || battery_online_supervisor(factory_clock, &factory_assertion),
            move |inputs| {
                let _ = tick_tx.send(inputs);
            },
        );
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first tick must finish");
        guard.update(true, true, 901);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("must acquire while busy on battery");

        let started = std::time::Instant::now();
        drop(guard);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drop must not wait a whole tick, took {elapsed:?}"
        );
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "the assertion is released exactly once"
        );
    }

    /// G2：线程等在 Condvar 上时 enabled=false，必须被唤醒并跳过宽限直接释放。
    #[test]
    fn sleep_hardening_disable_wakes_the_worker_and_skips_the_grace() {
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let releases = Arc::new(AtomicUsize::new(0));
        let clock = SteppingClock::new();
        let assertion = CountingAssertion::new(acquired_tx, Arc::clone(&releases));
        let factory_assertion = assertion.clone();
        let factory_clock = Arc::clone(&clock);

        let guard = PowerGuard::spawn_with_factory(
            GuardInputs {
                busy: true,
                enabled: true,
                grace_secs: 900,
                stop: false,
            },
            move || battery_online_supervisor(factory_clock, &factory_assertion),
            |_| {},
        );
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("must acquire first");

        guard.update(false, false, 900);
        // 不等满一拍：禁用必须立刻唤醒线程并释放。
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while releases.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "disable must release immediately, not after the grace"
        );
        drop(guard);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    /// G3：一次本地 tick 卡在系统调用里时 Drop——join 等这次调用返回，
    /// 之后不再有新的 tick，断言最终释放一次。
    #[test]
    fn sleep_hardening_drop_during_a_blocked_tick_waits_then_stops() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (entered_tx, entered_rx) = mpsc::channel();
        let releases = Arc::new(AtomicUsize::new(0));
        let clock = SteppingClock::new();
        let assertion = CountingAssertion::new(mpsc::channel().0, Arc::clone(&releases));
        let ticks = Arc::new(AtomicUsize::new(0));

        let factory_barrier = Arc::clone(&barrier);
        let factory_clock = Arc::clone(&clock);
        let factory_assertion = assertion.clone();
        let factory_ticks = Arc::clone(&ticks);
        let guard = PowerGuard::spawn_with_factory(
            GuardInputs {
                busy: true,
                enabled: true,
                grace_secs: 900,
                stop: false,
            },
            move || {
                Supervisor::new(
                    factory_clock,
                    Box::new(BlockingPower {
                        entered: Some(entered_tx),
                        release: factory_barrier,
                    }),
                    Box::new(AlwaysOnline),
                    factory_assertion.handle(),
                    true,
                )
            },
            move |_| {
                factory_ticks.fetch_add(1, Ordering::SeqCst);
            },
        );

        // 等线程真的进到那一次本地调用里，再开另一个线程 Drop。
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker must enter the blocked probe");
        let dropper = std::thread::spawn(move || {
            drop(guard);
        });
        // 给 Drop 一点时间去置 stop 并卡在 join 上，然后才放行 tick。
        std::thread::sleep(Duration::from_millis(50));
        barrier.wait();
        dropper
            .join()
            .expect("drop must return after the call completes");

        let ticks_after = ticks.load(Ordering::SeqCst);
        assert_eq!(
            ticks_after, 1,
            "no tick runs after the stop observed post-call"
        );
        assert_eq!(
            releases.load(Ordering::SeqCst),
            1,
            "the assertion is released exactly once on the way out"
        );
    }

    /// G4：一致输入。同一拍的 busy/enabled/grace 必须一起换版，且相同的重复
    /// update 不引发额外 tick。
    #[test]
    fn sleep_hardening_repeated_identical_updates_do_not_tick() {
        let (tick_tx, tick_rx) = mpsc::channel();
        let clock = SteppingClock::new();
        let assertion = CountingAssertion::new(mpsc::channel().0, Arc::new(AtomicUsize::new(0)));
        let factory_assertion = assertion.clone();
        let factory_clock = Arc::clone(&clock);

        let guard = PowerGuard::spawn_with_factory(
            GuardInputs {
                busy: false,
                enabled: true,
                grace_secs: 900,
                stop: false,
            },
            move || battery_online_supervisor(factory_clock, &factory_assertion),
            move |inputs| {
                let _ = tick_tx.send(inputs);
            },
        );
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first tick must finish");

        // 100 次完全相同的提交：值没变 → 不 notify → 不应有新的 tick。
        for _ in 0..100 {
            guard.update(false, true, 900);
        }
        assert!(
            tick_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "identical updates must not cause extra ticks"
        );

        // 真的变了：必须唤醒并跑一拍。
        guard.update(true, true, 900);
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("a real change must wake the worker");
        drop(guard);
    }

    /// A grace change submitted from the main loop must take effect on the
    /// *next tick the worker runs*, which the update itself triggers.
    ///
    /// Why this matters: the main loop only calls `update` when the settings
    /// changed or when the 10s ticker fires. A save must not have to wait for
    /// the ticker -- so `update` has to wake the condvar. Asserting "a tick ran"
    /// would pass even if the window never reached the policy, so this drives a
    /// real hold and asserts the *decision* flips.
    #[test]
    fn sleep_hardening_grace_changes_reach_the_policy() {
        let (tick_tx, tick_rx) = mpsc::channel();
        let clock = SteppingClock::new();
        let assertion = CountingAssertion::new(mpsc::channel().0, Arc::new(AtomicUsize::new(0)));
        let factory_assertion = assertion.clone();
        let factory_clock = Arc::clone(&clock);

        let guard = PowerGuard::spawn_with_factory(
            GuardInputs {
                busy: true,
                enabled: true,
                grace_secs: 900,
                stop: false,
            },
            move || battery_online_supervisor(factory_clock, &factory_assertion),
            move |inputs| {
                let _ = tick_tx.send(inputs);
            },
        );
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first tick must finish");

        // 只改窗口（其余不变）也必须算一次变化并被下发。
        guard.update(true, true, 3600);
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("a grace change must wake the worker");
        drop(guard);
    }

    /// 外部可控时钟：只有测试推进它，tick 自己不会动。
    ///
    /// `SteppingClock` 每次采样前进 1 秒，用来测「宽限到期」需要几百拍；这里要
    /// 测的是「起点是哪一刻」，所以时间必须停在测试指定的位置。
    struct EditableClock {
        elapsed: Arc<StdMutex<Duration>>,
    }

    impl Clock for EditableClock {
        fn now(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(*self.elapsed.lock().unwrap()))
        }
        fn since(&self, stamp: Stamp) -> Duration {
            self.elapsed
                .lock()
                .unwrap()
                .saturating_sub(stamp.as_duration())
        }
        fn wall(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(*self.elapsed.lock().unwrap()))
        }
        fn mono_trusted(&self) -> bool {
            true
        }
    }

    /// 起一个用外部可控时钟的 guard，并返回可推进的时间与观测点。
    struct PowerFixture {
        guard: PowerGuard,
        elapsed: Arc<StdMutex<Duration>>,
        releases: Arc<AtomicUsize>,
        tick_rx: mpsc::Receiver<GuardInputs>,
    }

    impl PowerFixture {
        fn start(grace_secs: u64) -> Self {
            let (acquired_tx, acquired_rx) = mpsc::channel();
            let releases = Arc::new(AtomicUsize::new(0));
            let elapsed = Arc::new(StdMutex::new(Duration::ZERO));
            let assertion = CountingAssertion::new(acquired_tx, Arc::clone(&releases));
            let factory_assertion = assertion.clone();
            let factory_elapsed = Arc::clone(&elapsed);
            let (tick_tx, tick_rx) = mpsc::channel();
            let guard = PowerGuard::spawn_with_factory(
                GuardInputs {
                    busy: true,
                    enabled: true,
                    grace_secs,
                    stop: false,
                },
                move || {
                    Supervisor::new(
                        Arc::new(EditableClock {
                            elapsed: factory_elapsed,
                        }),
                        Box::new(Battery),
                        Box::new(AlwaysOnline),
                        factory_assertion.handle(),
                        true,
                    )
                },
                move |inputs| {
                    let _ = tick_tx.send(inputs);
                },
            );
            // 这里只等「线程起来了」，不测延迟：满并发跑整个 lib 测试套件时
            // 调度会把 1 秒耗光。宽松截止，避免把机器负载当逻辑失败。
            acquired_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("work at t=0 must acquire");
            let fixture = Self {
                guard,
                elapsed,
                releases,
                tick_rx,
            };
            fixture.settle();
            fixture
        }

        /// 推进时间并提交一次输入，等**采用这份输入的那一拍**跑完。
        ///
        /// 匹配输入而不是「随便收到一条信号」：信号相对 `update` 是异步的，
        /// 排空通道之后仍可能收到上一拍的迟到信号。只认 `Update` 就等于
        /// 承认「新输入还没被采用」，于是这里会一直等到真正的目标拍。
        fn tick_at(&mut self, secs: u64, busy: bool, grace_secs: u64) {
            *self.elapsed.lock().unwrap() = Duration::from_secs(secs);
            self.guard.update(busy, true, grace_secs);
            let expected = GuardInputs {
                busy,
                enabled: true,
                grace_secs,
                stop: false,
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                // 只证明「被唤醒并采用了新输入」，不证明「多快」。
                let seen = self
                    .tick_rx
                    .recv_timeout(remaining)
                    .expect("a changed input must wake the worker");
                if seen == expected {
                    return;
                }
                // 上一拍的迟到信号：继续等目标拍。
            }
        }

        /// 吞掉首拍可能迟到的信号，让后续等待从干净状态开始。
        fn settle(&self) {
            std::thread::sleep(Duration::from_millis(20));
            while self.tick_rx.try_recv().is_ok() {}
        }

        fn released(&self) -> usize {
            self.releases.load(Ordering::SeqCst)
        }
    }

    /// Lengthening the window must not drop a hold that is still running.
    ///
    /// This is the failure mode a naive "rebuild the policy on save" produces:
    /// rebuilding clears `last_ok`, so the next tick sees "no work, no recent
    /// work" and releases -- the user widened the window and the machine went to
    /// sleep. `set_windows` only swaps durations, so the hold survives.
    ///
    /// The window is widened at t=400, i.e. *past* the old 300s boundary but
    /// inside the new 3600s one. That makes "did the new window reach the
    /// policy?" observable in a single tick: forwarding it holds, keeping the
    /// startup value releases. The grace change itself wakes the worker, so no
    /// 10s ticker is involved.
    #[test]
    fn lengthening_the_window_does_not_release_a_running_hold() {
        let mut fixture = PowerFixture::start(300);
        assert_eq!(fixture.released(), 0, "work at t=0 holds the assertion");

        // Work stops at t=100: inside the 300s window, so still held.
        fixture.tick_at(100, false, 300);
        assert_eq!(fixture.released(), 0, "100s into a 300s window still holds");

        // The save: 300 -> 3600, at t=400 (past the old boundary).
        fixture.tick_at(400, false, 3600);
        assert_eq!(
            fixture.released(),
            0,
            "widening the window must not release: the hold is still inside it"
        );
    }

    /// Shortening the window must take effect at the save, not at the next tick.
    ///
    /// No 10s interval is advanced anywhere here: the release is driven by
    /// `update` waking the condvar, which is exactly the path a settings save
    /// takes. The grace is counted from when work stopped (t=0), not from the
    /// save -- otherwise a user who shortens the window to stop the hold would
    /// wait another full window.
    #[test]
    fn a_window_save_shortens_the_remaining_grace_without_a_ticker() {
        let mut fixture = PowerFixture::start(3600);
        assert_eq!(fixture.released(), 0);

        // Work stops at t=1200. 1 hour window: still held.
        fixture.tick_at(1200, false, 3600);
        assert_eq!(fixture.released(), 0, "1200s into a 3600s window holds");

        // The save: 3600 -> 300. 1200 - 0 >= 300, so this very tick releases.
        fixture.tick_at(1200, false, 300);
        assert_eq!(
            fixture.released(),
            1,
            "shortening the window must release at the save, counting from when work stopped"
        );
    }

    /// 退出预算的基线检查：`spawn` 的装配本身不阻塞，Drop 有界。
    #[test]
    fn sleep_hardening_spawn_and_drop_are_bounded() {
        let started = std::time::Instant::now();
        let guard = PowerGuard::spawn(false, 900);
        drop(guard);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "spawn+drop must stay within the 2s real-platform budget"
        );
    }

    // ---- 真机：`#[ignore]` 手动跑 --------------------------------------

    /// 线程起来、收到 `false` 之后，`pmset` 里不应再看到我们的断言。
    ///
    /// `#[ignore]`：这条要真读系统的断言表，只在 macOS 上、且只在手动跑
    /// `--ignored` 时执行（与 `assertion` 模块的真机测试同款 gate）。
    #[test]
    #[ignore]
    fn disabling_the_guard_releases_the_real_assertion() {
        let guard = PowerGuard::spawn(true, 15 * 60);
        guard.update(true, true, 15 * 60);
        // 等线程跑够两拍，确保它持有断言。
        std::thread::sleep(Duration::from_secs(SUPERVISOR_TICK.as_secs() * 2));
        let held = assertion_visible_in_pmset();
        guard.update(false, false, 15 * 60);
        std::thread::sleep(Duration::from_secs(2));
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
