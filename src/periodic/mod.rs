//! [`PeriodicInterval`], a thin wrapper around a single kernel-side
//! periodic timer.
//!
//! Unlike [`crate::OsInterval`], the kernel owns the schedule: we arm
//! once with `it_value = it_interval = period` (Linux) or an
//! always-rearming `EVFILT_TIMER` (BSD/macOS) and just drain
//! expirations. There is no `MissedTickBehavior`, no `reset_*`, no
//! deadline arithmetic in userspace — by design.
//!
//! Use this when:
//!
//! * You want the lowest possible per-tick overhead.
//! * Cron/heartbeat-style scheduling where coarse resolution is
//!   acceptable and missed ticks should *coalesce* (Burst-equivalent).
//! * You're fine with the platform restriction (Linux/Android/macOS/iOS
//!   and the BSDs only).
//!
//! Use [`crate::OsInterval`] otherwise — it has richer missed-tick
//! semantics, supports reset, and works on Windows and non-Unix
//! fallbacks.

use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
use linux as imp;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
mod bsd;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
use bsd as imp;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
)))]
compile_error!(
    "the `periodic` feature is only supported on Linux, Android, \
     macOS, iOS, FreeBSD, NetBSD, OpenBSD, and DragonFly BSD"
);

/// Number of times a [`PeriodicInterval`] has expired since the previous
/// successful tick.
///
/// Always at least `1` when returned from [`PeriodicInterval::tick`]; a
/// value greater than `1` means the consumer fell behind the kernel
/// schedule and the kernel coalesced the missed expirations.
pub type Expirations = u64;

/// A periodic ticker driven entirely by an OS timer.
///
/// The kernel maintains the schedule; `tick().await` simply waits for
/// the next expiration (or batch of expirations) and reports how many
/// fires were coalesced.
///
/// # Example
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_osinterval::PeriodicInterval;
///
/// # async fn run() -> std::io::Result<()> {
/// let mut iv = PeriodicInterval::new(Duration::from_secs(60))?;
/// loop {
///     let n = iv.tick().await?;
///     if n > 1 {
///         eprintln!("fell behind by {} ticks", n - 1);
///     }
///     // run cron job
/// }
/// # }
/// ```
///
/// # Construction
///
/// `new` requires an active tokio runtime to register the timer fd with
/// the reactor. It returns an [`io::Error`](std::io::Error) if the
/// kernel rejects the timer setup; once constructed, `tick`/`poll_tick`
/// return errors only on truly unexpected I/O failures.
///
/// # Differences from [`OsInterval`](crate::OsInterval)
///
/// * No `MissedTickBehavior` — coalescing is the only mode.
/// * No `reset`, `reset_after`, `reset_at`, or `reset_immediately`.
/// * No `interval_at`-style phase control. The first tick fires
///   `period` after construction (not immediately).
/// * Returns the OS-reported expiration count from each `tick`.
/// * Linux/BSD only.
pub struct PeriodicInterval {
    inner: imp::PeriodicTimer,
    period: Duration,
}

impl PeriodicInterval {
    /// Create a new periodic ticker with the given `period`.
    ///
    /// Must be called from within a tokio runtime.
    ///
    /// # Errors
    /// Returns an error if the underlying timer object cannot be
    /// created or registered with the reactor.
    ///
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(period: Duration) -> std::io::Result<Self> {
        assert!(period > Duration::ZERO, "`period` must be non-zero");
        let inner = imp::PeriodicTimer::new(period)?;
        Ok(Self { inner, period })
    }

    /// Returns the period of this ticker.
    #[must_use]
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Wait for the next expiration. Returns the number of times the
    /// kernel timer has fired since the previous successful `tick`
    /// (always ≥ 1).
    ///
    /// Cancellation safety: dropping the returned future before it
    /// completes does not lose progress; the kernel keeps counting and
    /// the next `tick` will report the accumulated total.
    ///
    /// # Errors
    /// Returns an error if the underlying OS timer object reports an
    /// I/O failure while waiting for or draining expirations.
    pub async fn tick(&mut self) -> std::io::Result<Expirations> {
        std::future::poll_fn(|cx| self.poll_tick(cx)).await
    }

    /// Poll-based variant of [`tick`](Self::tick).
    ///
    /// # Errors
    /// See [`tick`](Self::tick).
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<Expirations>> {
        self.inner.poll_tick(cx)
    }
}

impl std::fmt::Debug for PeriodicInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeriodicInterval")
            .field("period", &self.period)
            .finish_non_exhaustive()
    }
}
