//! 保活线程：唯一持有 IOKit 断言的执行体。
//!
//! 为什么是独立 OS 线程而不是 `select!` 分支或 `spawn_blocking`：
//!
//! - 一拍里含 `route` 子进程（10~50 ms）与 `connect_timeout(.., 2s)`，
//!   放主循环会阻塞所有 RPC，最长 2 秒。
//! - `spawn_blocking` 的线程会被 tokio 在空闲时回收，而这里需要「长期存在、
//!   每 10 秒醒一次」的节奏；`std::thread` 语义更清楚，也不占 tokio 的阻塞配额。
//! - 断言句柄必须活到进程结束，`Drop` 里 join 才能保证「线程停了断言一定释放」。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

pub(crate) struct PowerGuard {
    /// 主循环每拍写入：本进程侧是否有活（本地会话 ∪ 远端近期交互）。
    activity: Arc<AtomicBool>,
    /// 主循环每拍写入：设置页开关的当前值。
    enabled: Arc<AtomicBool>,
    /// 主循环每拍写入：条件消失后的宽限，单位秒。
    grace_secs: Arc<AtomicU64>,
    /// 主循环每拍写入：远端交互有效窗口，单位秒。
    ///
    /// 保活线程本身不读它——远端窗口由主循环消费（传给 `remote_touch_busy`）。
    /// 存下来是为了 `set_windows` 的签名对称，以及让「主循环确实下发过」可被断言。
    #[allow(dead_code)]
    remote_secs: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PowerGuard {
    /// 起线程。三个初值都是装配时读到的——避免线程第一拍先误持、再释放。
    pub(crate) fn spawn(enabled: bool, grace_secs: u64, remote_secs: u64) -> Self {
        let activity = Arc::new(AtomicBool::new(false));
        let enabled_flag = Arc::new(AtomicBool::new(enabled));
        let grace_flag = Arc::new(AtomicU64::new(grace_secs));
        let remote_flag = Arc::new(AtomicU64::new(remote_secs));
        let stop = Arc::new(AtomicBool::new(false));
        let activity_in = Arc::clone(&activity);
        let enabled_in = Arc::clone(&enabled_flag);
        let grace_in = Arc::clone(&grace_flag);
        let stop_in = Arc::clone(&stop);

        let handle = std::thread::Builder::new()
            .name("yi-agent-power".to_string())
            .spawn(move || {
                // 时钟在线程里建：`SystemClock` 自己持有「单调钟是否可信」的状态，
                // 每拍读一次就是它的全部用途，跨线程共享没有意义。
                let clock = Arc::new(SystemClock::new()) as Arc<dyn Clock>;
                let mut supervisor = Supervisor::new(
                    clock,
                    Box::new(SystemPowerProbe::new()),
                    Box::new(SystemNetProbe::new()),
                    platform_assertion(ASSERTION_LABEL),
                    enabled_in.load(Ordering::Relaxed),
                );
                while !stop_in.load(Ordering::Relaxed) {
                    let busy = activity_in.load(Ordering::Relaxed);
                    let enabled = enabled_in.load(Ordering::Relaxed);
                    // 每拍重算并下发：成本是两次 load + 两次 store，远低于
                    // 一次时钟采样。不做「值变了才下发」的优化——那要多保存
                    // 一份可能失同步的状态。
                    let grace = Duration::from_secs(grace_in.load(Ordering::Relaxed));
                    supervisor.set_windows(grace, slack_for(grace));
                    supervisor.tick(busy, enabled);
                    sleep_until_stop(&stop_in);
                }
                // 显式 drop：确保断言在 join 返回前释放，不依赖栈清理时机。
                drop(supervisor);
            })
            .expect("spawn the power guard thread");

        Self {
            activity,
            enabled: enabled_flag,
            grace_secs: grace_flag,
            remote_secs: remote_flag,
            stop,
            handle: Some(handle),
        }
    }

    /// 主循环每拍调用：写一个原子，不阻塞。
    pub(crate) fn set_activity(&self, active: bool) {
        self.activity.store(active, Ordering::Relaxed);
    }

    /// 主循环每拍调用：开关现读磁盘后写进来。
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 主循环每拍调用：把设置里的两个窗口写进来。只 `store`，不阻塞。
    ///
    /// 与 `set_enabled` 同形状：主循环绝不在这里做 I/O，窗口的读取与
    /// 换算都在调用方那一拍完成。
    pub(crate) fn set_windows(&self, grace_secs: u64, remote_secs: u64) {
        self.grace_secs.store(grace_secs, Ordering::Relaxed);
        // 远端窗口由主循环自己消费（传给 `remote_touch_busy`），线程不需要它；
        // 但存下来让「装配值与运行值来自同一处」这件事可被测试断言。
        self.remote_secs.store(remote_secs, Ordering::Relaxed);
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // join 是必要的：不 join 则进程可能在线程释放断言之前退出，
            // 断言变成内核里的孤儿（实际会被系统回收，但那依赖系统行为）。
            //
            // 代价分析：最坏情况要等满一个 `SUPERVISOR_TICK`（10s）——线程正在
            // sleep，只能醒来才发现 stop。让退出等 10 秒不可接受，故等待走
            // `sleep_until_stop` 的分段轮询：`stop` 置位后最多 50ms 返回。
            let _ = handle.join();
        }
    }
}

/// 可被提前打断的等待：`stop` 置位后最多 50ms 内返回。
///
/// 直接用 `thread::sleep(10s)` 会让 `Drop` 的 join 等满 10 秒——那意味着关掉桌面
/// 要卡 10 秒才能退出，用户会以为程序挂了。分段轮询的代价只是一些无意义的唤醒，
/// 而这个线程本来就只在后台每 10 秒干一次活。
fn sleep_until_stop(stop: &AtomicBool) {
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < SUPERVISOR_TICK {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(step);
        waited += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线程起来、收到 `false` 之后，`pmset` 里不应再看到我们的断言。
    ///
    /// `#[ignore]`：这条要真读系统的断言表，只在 macOS 上、且只在手动跑
    /// `--ignored` 时执行（与 `assertion` 模块的真机测试同款 gate）。
    #[test]
    #[ignore]
    fn disabling_the_guard_releases_the_real_assertion() {
        let guard = PowerGuard::spawn(true, 15 * 60, 5 * 60);
        guard.set_activity(true);
        // 等线程跑够两拍，确保它持有断言。
        std::thread::sleep(Duration::from_secs(SUPERVISOR_TICK.as_secs() * 2));
        let held = assertion_visible_in_pmset();
        guard.set_enabled(false);
        std::thread::sleep(Duration::from_secs(SUPERVISOR_TICK.as_secs() * 2));
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

    /// 退出延迟必须远小于一拍：`Drop` 不该让关闭桌面卡 10 秒。
    ///
    /// 用真实的 `PowerGuard`（因此会真的碰 IOKit 一次）来量，是因为延迟来自
    /// `sleep_until_stop` 与线程调度，`mock` 量不到。
    #[test]
    fn dropping_the_guard_does_not_wait_a_whole_tick() {
        let guard = PowerGuard::spawn(false, 15 * 60, 5 * 60);
        let started = std::time::Instant::now();
        drop(guard);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drop must be bounded by the 50ms poll step, took {elapsed:?}"
        );
    }

    /// `watchdog_slack = max(5min, grace/3)`。
    /// 下限 5 分钟的理由：宽限设成 1 分钟时，20 秒的 slack 会让看门狗
    /// 过于敏感——一次短暂的采样抖动就可能被判成时钟故障。
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
}
