//! Public `OsInterval` type and the userspace policy logic
//! (next-deadline arithmetic and `MissedTickBehavior`).
//!
//! Backends in [`crate::sys`] only need to satisfy [`crate::sys::Timer`]:
//! "wake me at this absolute deadline". All scheduling decisions live here
//! so that every platform behaves identically with respect to missed-tick
//! handling.

use std::future::poll_fn;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::Instant;

use crate::sys;

/// Defines the behavior of an [`OsInterval`] when it misses a tick.
///
/// Semantically identical to [`tokio::time::MissedTickBehavior`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MissedTickBehavior {
    /// Tick as fast as possible until caught up. This is the default and
    /// matches `tokio::time::Interval`'s default.
    #[default]
    Burst,
    /// After a missed tick, the next tick fires `period` after the moment
    /// the missed tick was observed (i.e. ticks slip).
    Delay,
    /// Skip missed ticks and keep the original schedule, snapping the next
    /// deadline forward to the next multiple of `period`.
    Skip,
}

impl MissedTickBehavior {
    /// Compute the next deadline given the previous scheduled deadline,
    /// the current time, and the period.
    fn next_deadline(self, prev: Instant, now: Instant, period: Duration) -> Instant {
        match self {
            MissedTickBehavior::Burst => prev + period,
            MissedTickBehavior::Delay => now + period,
            MissedTickBehavior::Skip => {
                // next = prev + period * ceil((now - prev) / period)
                if now <= prev + period {
                    prev + period
                } else {
                    let elapsed_ns = now.saturating_duration_since(prev).as_nanos();
                    let period_ns = period.as_nanos().max(1);
                    // `Duration::mul` takes `u32`. If we have fallen behind by
                    // more than ~4 billion ticks we can't honestly represent the
                    // catch-up anyway, so saturate the multiplier rather than
                    // silently truncating it.
                    let n = u32::try_from(elapsed_ns.div_ceil(period_ns)).unwrap_or(u32::MAX);
                    prev + period * n
                }
            }
        }
    }
}

/// Creates an interval that yields its first tick immediately and then once
/// every `period` thereafter.
///
/// Mirrors [`tokio::time::interval`].
///
/// # Panics
/// Panics if `period` is zero.
#[must_use]
pub fn interval(period: Duration) -> OsInterval {
    interval_at(Instant::now(), period)
}

/// Creates an interval that yields its first tick at `start` and then once
/// every `period` thereafter.
///
/// Mirrors [`tokio::time::interval_at`].
///
/// # Panics
/// Panics if `period` is zero.
#[must_use]
pub fn interval_at(start: Instant, period: Duration) -> OsInterval {
    assert!(period > Duration::ZERO, "`period` must be non-zero");
    OsInterval {
        period,
        behavior: MissedTickBehavior::default(),
        next_deadline: start,
        armed_for: None,
        timer: sys::Timer::new(),
    }
}

/// Periodic timer driven by the operating system's native async timer.
///
/// See the crate-level docs for the precise platform mapping.
///
/// # Example
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_osinterval::{interval, MissedTickBehavior};
///
/// # async fn run() {
/// let mut iv = interval(Duration::from_millis(50));
/// iv.set_missed_tick_behavior(MissedTickBehavior::Skip);
///
/// for _ in 0..10 {
///     let scheduled = iv.tick().await;
///     // `scheduled` is the deadline this tick was scheduled for.
///     let _ = scheduled;
/// }
/// # }
/// ```
pub struct OsInterval {
    period: Duration,
    behavior: MissedTickBehavior,
    /// The deadline of the *next* tick to deliver.
    next_deadline: Instant,
    /// The deadline the underlying kernel timer is currently armed for, if
    /// any. Used to avoid redundant `arm()` syscalls.
    armed_for: Option<Instant>,
    timer: sys::Timer,
}

impl OsInterval {
    /// Returns the period of this interval.
    #[must_use]
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Returns the current [`MissedTickBehavior`].
    #[must_use]
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.behavior
    }

    /// Sets the [`MissedTickBehavior`].
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.behavior = behavior;
    }

    /// Resets the interval so the next tick fires `period` after now.
    ///
    /// Equivalent to [`tokio::time::Interval::reset`].
    pub fn reset(&mut self) {
        self.set_next_deadline(Instant::now() + self.period);
    }

    /// Resets the interval so the next tick fires immediately.
    pub fn reset_immediately(&mut self) {
        self.set_next_deadline(Instant::now());
    }

    /// Resets the interval so the next tick fires `after` from now.
    pub fn reset_after(&mut self, after: Duration) {
        self.set_next_deadline(Instant::now() + after);
    }

    /// Resets the interval so the next tick fires at the given `deadline`.
    pub fn reset_at(&mut self, deadline: Instant) {
        self.set_next_deadline(deadline);
    }

    fn set_next_deadline(&mut self, deadline: Instant) {
        self.next_deadline = deadline;
        // Force the backend to be re-armed on the next poll.
        self.armed_for = None;
        self.timer.disarm();
    }

    /// Completes when the next tick has elapsed and returns the
    /// deadline that was scheduled for that tick.
    ///
    /// Cancellation safety: dropping the returned future before it
    /// completes does not lose progress; the next call to [`tick`] will
    /// still fire at the same scheduled deadline.
    ///
    /// [`tick`]: Self::tick
    pub async fn tick(&mut self) -> Instant {
        poll_fn(|cx| self.poll_tick(cx)).await
    }

    /// Poll-based variant of [`tick`](Self::tick).
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        let target = self.next_deadline;

        // Fast path: the deadline is already in the past. Skip the
        // syscall and deliver immediately.
        let now = Instant::now();
        if now >= target {
            self.advance_after_tick(target, now);
            return Poll::Ready(target);
        }

        // Make sure the kernel timer is armed for the right deadline.
        if self.armed_for != Some(target) {
            self.timer.arm(target);
            self.armed_for = Some(target);
        }

        match self.timer.poll_expired(cx) {
            Poll::Ready(()) => {
                let now = Instant::now();
                self.advance_after_tick(target, now);
                Poll::Ready(target)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn advance_after_tick(&mut self, fired: Instant, now: Instant) {
        self.next_deadline = self.behavior.next_deadline(fired, now, self.period);
        self.armed_for = None;
    }
}

impl std::fmt::Debug for OsInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OsInterval")
            .field("period", &self.period)
            .field("missed_tick_behavior", &self.behavior)
            .field("next_deadline", &self.next_deadline)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_advances_to_next_multiple() {
        let prev = Instant::now();
        let period = Duration::from_millis(100);
        // Suppose 350ms have elapsed since prev: ceil(350/100) = 4.
        let now = prev + Duration::from_millis(350);
        let next = MissedTickBehavior::Skip.next_deadline(prev, now, period);
        assert_eq!(next, prev + Duration::from_millis(400));
    }

    #[test]
    fn burst_uses_period_only() {
        let prev = Instant::now();
        let period = Duration::from_millis(100);
        let now = prev + Duration::from_millis(500);
        let next = MissedTickBehavior::Burst.next_deadline(prev, now, period);
        assert_eq!(next, prev + period);
    }

    #[test]
    fn delay_uses_now_plus_period() {
        let prev = Instant::now();
        let period = Duration::from_millis(100);
        let now = prev + Duration::from_millis(500);
        let next = MissedTickBehavior::Delay.next_deadline(prev, now, period);
        assert_eq!(next, now + period);
    }
}
