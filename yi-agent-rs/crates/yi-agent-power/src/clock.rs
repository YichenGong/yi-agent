//! 睡眠感知时钟。
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp(pub(crate) Duration);

impl Stamp {
    /// 由一个读数构造。只给测试与跨 crate 的夹具用——生产代码一律从
    /// `Clock::now()` / `Clock::wall()` 取戳，不要凭空造时间点。
    pub fn from_duration(duration: Duration) -> Self {
        Self(duration)
    }

    pub fn as_duration(&self) -> Duration {
        self.0
    }
}

pub fn sample_monotonic() -> Option<Duration> {
    platform_monotonic()
}

#[cfg(unix)]
fn platform_monotonic() -> Option<Duration> {
    #[cfg(target_os = "macos")]
    let clock_id = libc::CLOCK_MONOTONIC_RAW;
    #[cfg(target_os = "linux")]
    let clock_id = libc::CLOCK_BOOTTIME;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` 是本地可写的 `timespec`，`clock_id` 是合法常量。
        let rc = unsafe { libc::clock_gettime(clock_id, &mut ts) };
        if rc != 0 || ts.tv_sec < 0 {
            return None;
        }
        Some(
            Duration::from_secs(ts.tv_sec as u64)
                + Duration::from_nanos(ts.tv_nsec.clamp(0, 999_999_999) as u64),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(not(unix))]
fn platform_monotonic() -> Option<Duration> {
    None
}

fn wall_now() -> Option<Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

pub(crate) fn is_suspicious(dt_mono: Duration, dt_wall: Duration) -> bool {
    let slept_for = dt_wall.saturating_sub(dt_mono);
    slept_for > Duration::from_secs(300) && dt_mono * 10 < slept_for
}

pub(crate) struct TrustGuard {
    suspects: u8,
    trusted: bool,
}

impl TrustGuard {
    pub(crate) fn new() -> Self {
        Self {
            suspects: 0,
            trusted: true,
        }
    }

    pub(crate) fn observe(&mut self, dt_mono: Duration, dt_wall: Duration) {
        if !self.trusted {
            return;
        }
        if is_suspicious(dt_mono, dt_wall) {
            self.suspects = self.suspects.saturating_add(1);
            if self.suspects >= 3 {
                self.trusted = false;
            }
        } else {
            self.suspects = 0;
        }
    }

    pub(crate) fn trusted(&self) -> bool {
        self.trusted
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Option<Stamp>;
    fn since(&self, stamp: Stamp) -> Duration;
    /// 墙钟读数。只用于看门狗度量「扛了多久」。
    fn wall(&self) -> Option<Stamp>;
    fn mono_trusted(&self) -> bool;

    /// 一拍所需的双时钟读数。任一取不到即返回 `None`（调用方据此放弃保活）。
    fn sample(&self) -> Option<crate::policy::Sample> {
        Some(crate::policy::Sample {
            mono: self.now()?.0,
            wall: self.wall()?.0,
        })
    }
}

struct ClockState {
    last_mono: Option<Duration>,
    last_wall: Option<Duration>,
    guard: TrustGuard,
    platform_ok: bool,
}

pub struct SystemClock {
    state: Mutex<ClockState>,
    downgrades: AtomicU64,
}

impl SystemClock {
    pub fn new() -> Self {
        let platform_ok = sample_monotonic().is_some();
        if !platform_ok {
            tracing_warn_once("platform monotonic clock is unavailable; using wall clock");
        }
        Self {
            state: Mutex::new(ClockState {
                last_mono: None,
                last_wall: None,
                guard: TrustGuard::new(),
                platform_ok,
            }),
            downgrades: AtomicU64::new(0),
        }
    }

    pub fn self_check(&self) {
        let Some(mono) = sample_monotonic() else {
            return;
        };
        let Some(wall) = wall_now() else { return };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let (Some(prev_mono), Some(prev_wall)) = (state.last_mono, state.last_wall) {
            let dt_mono = mono.saturating_sub(prev_mono);
            let dt_wall = wall.saturating_sub(prev_wall);
            let was_trusted = state.guard.trusted();
            state.guard.observe(dt_mono, dt_wall);
            if was_trusted && !state.guard.trusted() {
                self.downgrades.fetch_add(1, Ordering::Relaxed);
                tracing_warn_once(
                    "monotonic clock stopped tracking wall time; downgraded to wall clock",
                );
            }
        }
        state.last_mono = Some(mono);
        state.last_wall = Some(wall);
    }

    fn read(&self) -> Option<Duration> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.platform_ok
            && state.guard.trusted()
            && let Some(mono) = sample_monotonic()
        {
            return Some(mono);
        }
        wall_now()
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Option<Stamp> {
        self.self_check();
        self.read().map(Stamp)
    }
    fn since(&self, stamp: Stamp) -> Duration {
        match self.read() {
            Some(now) => now.saturating_sub(stamp.0),
            None => Duration::ZERO,
        }
    }
    fn wall(&self) -> Option<Stamp> {
        wall_now().map(Stamp)
    }
    fn mono_trusted(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.platform_ok && state.guard.trusted()
    }
}

fn tracing_warn_once(message: &str) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| eprintln!("[yi-agent-power] {message}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_reads_are_non_decreasing() {
        let a = sample_monotonic().expect("platform clock must work");
        let b = sample_monotonic().expect("platform clock must work");
        assert!(b >= a);
    }

    #[test]
    fn system_clock_reports_a_stamp_and_a_non_negative_elapsed() {
        let clock = SystemClock::new();
        let start = clock.now().expect("clock must be readable");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let elapsed = clock.since(start);
        assert!(
            elapsed >= std::time::Duration::from_millis(5),
            "{elapsed:?}"
        );
    }

    #[test]
    fn since_saturates_instead_of_going_negative() {
        let clock = SystemClock::new();
        let now = clock.now().unwrap();
        let future = Stamp(now.0 + std::time::Duration::from_secs(3600));
        assert_eq!(clock.since(future), std::time::Duration::ZERO);
    }

    #[test]
    fn a_stalled_monotonic_clock_is_suspicious_even_when_not_frozen_solid() {
        assert!(is_suspicious(
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(8 * 3600)
        ));
    }

    #[test]
    fn normal_sleep_is_not_suspicious() {
        assert!(!is_suspicious(
            std::time::Duration::from_secs(8 * 3600),
            std::time::Duration::from_secs(8 * 3600)
        ));
    }

    #[test]
    fn a_late_tick_is_not_suspicious() {
        assert!(!is_suspicious(
            std::time::Duration::from_secs(50),
            std::time::Duration::from_secs(50)
        ));
    }

    #[test]
    fn three_consecutive_suspects_flip_mono_trusted() {
        let mut guard = TrustGuard::new();
        let mono = std::time::Duration::from_secs(20);
        let wall = std::time::Duration::from_secs(3600);
        guard.observe(mono, wall);
        assert!(guard.trusted());
        guard.observe(mono, wall);
        assert!(guard.trusted());
        guard.observe(mono, wall);
        assert!(!guard.trusted());
    }

    #[test]
    fn trust_never_returns_after_a_flip() {
        let mut guard = TrustGuard::new();
        let mono = std::time::Duration::from_secs(20);
        let wall = std::time::Duration::from_secs(3600);
        for _ in 0..3 {
            guard.observe(mono, wall);
        }
        assert!(!guard.trusted());
        guard.observe(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        );
        assert!(!guard.trusted());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn raw_monotonic_differs_from_uptime_raw_on_macos() {
        fn read(id: libc::clockid_t) -> Option<Duration> {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: 本地可写 timespec + 合法 clock id。
            let rc = unsafe { libc::clock_gettime(id, &mut ts) };
            (rc == 0).then(|| {
                Duration::from_secs(ts.tv_sec as u64) + Duration::from_nanos(ts.tv_nsec as u64)
            })
        }
        let raw = read(libc::CLOCK_MONOTONIC_RAW).expect("CLOCK_MONOTONIC_RAW must work");
        let uptime = read(libc::CLOCK_UPTIME_RAW).expect("CLOCK_UPTIME_RAW must work");
        assert!(raw > uptime, "raw={raw:?} uptime={uptime:?}");
        assert_eq!(read(7), None, "CLOCK_BOOTTIME must not exist on macOS");
    }
}
