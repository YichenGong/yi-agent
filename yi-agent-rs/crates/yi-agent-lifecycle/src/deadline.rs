//! Monotonic total-budget accounting.
//!
//! Every deadline in this protocol is a *total* budget for a whole operation, not
//! a per-syscall timeout. That distinction is load-bearing: the spec forbids
//! reusing the business IPC's 30-second request timeout as if it were the 10-second
//! teardown guarantee, and it forbids per-object budgets that would multiply by
//! the number of clients. One `Deadline` object is threaded through every hop of
//! an operation instead.
//!
//! Time is read from a [`Clock`] rather than directly from `Instant::now()`, so a
//! test can drive a whole teardown sequence without sleeping, and a suspend/resume
//! adapter can substitute a suspend-aware clock where one exists. `Clock` itself
//! has no Tokio or business dependency: this crate is linked by the desktop shell.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// A monotonic time source, expressed as time since an arbitrary origin.
///
/// Implementations must be monotonic. `Duration` (not `Instant`) is the unit so
/// the trait stays object-safe, serializable in handover state, and usable from a
/// fake clock in tests.
pub trait Clock: Send + Sync {
    /// Time elapsed since this clock's origin.
    fn now(&self) -> Duration;
}

/// The production clock: process-monotonic, immune to wall-clock jumps and NTP.
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

#[derive(Clone)]
struct DeadlineInner {
    clock: Arc<dyn Clock>,
    /// `clock.now()` at which the budget is exhausted.
    expires_at: Duration,
}

/// A total budget for one operation, shared by every I/O hop inside it.
#[derive(Clone)]
pub struct Deadline(DeadlineInner);

impl Deadline {
    /// A budget of `budget` starting now, on the production monotonic clock.
    pub fn after(budget: Duration) -> Self {
        Self::with_clock(budget, Arc::new(MonotonicClock::new()))
    }

    /// A budget driven by an injected clock. Tests supply a fake; a
    /// suspend-aware adapter supplies a platform clock.
    pub fn with_clock(budget: Duration, clock: Arc<dyn Clock>) -> Self {
        let expires_at = clock.now().saturating_add(budget);
        Self(DeadlineInner { clock, expires_at })
    }

    /// Budget left, saturating at zero. Never negative, never panics on overflow.
    pub fn remaining(&self) -> Duration {
        self.0.expires_at.saturating_sub(self.0.clock.now())
    }

    /// `ask` capped by what is left. Callers pass their own per-hop timeout
    /// (the control channel's 2 seconds) and get whichever is smaller.
    pub fn clamp(&self, ask: Duration) -> Duration {
        let remaining = self.remaining();
        if ask < remaining { ask } else { remaining }
    }

    /// True when no budget remains, i.e. no further I/O may start.
    pub fn is_expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

impl std::fmt::Debug for Deadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deadline")
            .field("remaining", &self.remaining())
            .finish()
    }
}
