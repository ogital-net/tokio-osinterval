//! Linux/Android backend using `timerfd_create(CLOCK_MONOTONIC, ...)`
//! integrated with the tokio reactor through [`tokio::io::unix::AsyncFd`].
//!
//! We arm the timerfd as a one-shot **absolute** timer
//! (`TFD_TIMER_ABSTIME`) against `CLOCK_MONOTONIC`. Doing so eliminates
//! the latency between the userspace clock read and the kernel installing
//! the timer from the achieved wakeup time — with a relative arming, the
//! kernel adds the delta to *its* clock at the moment of `timerfd_settime`,
//! so any time spent in the syscall path pushes the wakeup later. With an
//! absolute deadline the kernel wakes at exactly the time we ask for.
//!
//! Tokio's [`tokio::time::Instant`] wraps [`std::time::Instant`], which on
//! Linux is `CLOCK_MONOTONIC`, so the two clocks tick at the same rate and
//! a single sample of each at `Timer::new()` is enough to convert any
//! future tokio `Instant` to an absolute `CLOCK_MONOTONIC` timespec.
//!
//! The kernel's `it_interval` is left at zero — we re-arm on every tick so
//! that the userspace `MissedTickBehavior` policy in
//! [`crate::interval`] stays authoritative.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::time::Instant;

pub(super) struct Timer {
    fd: AsyncFd<OwnedFd>,
    /// Tokio-side reference instant captured at construction. Paired with
    /// [`Timer::epoch_monotonic`] to convert future `Instant` deadlines
    /// into absolute `CLOCK_MONOTONIC` timespecs without an extra syscall
    /// per `arm()`.
    epoch_tokio: Instant,
    /// `CLOCK_MONOTONIC` timestamp captured at construction.
    epoch_monotonic: libc::timespec,
}

impl Timer {
    pub(super) fn new() -> Self {
        let raw = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        assert!(
            raw >= 0,
            "timerfd_create failed: {}",
            io::Error::last_os_error()
        );
        // SAFETY: `raw` is a freshly-created fd we own.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        let fd = AsyncFd::with_interest(owned, Interest::READABLE)
            .expect("registering timerfd with the tokio reactor (is a runtime active?)");

        // Sample both clocks back-to-back. The two reads aren't truly
        // simultaneous, but both are `CLOCK_MONOTONIC` underneath and tick
        // at the same rate, so any small offset between the samples is a
        // constant that cancels out for every later `arm()`.
        let epoch_monotonic = clock_gettime_monotonic();
        let epoch_tokio = Instant::now();

        Self {
            fd,
            epoch_tokio,
            epoch_monotonic,
        }
    }

    /// Arm the timerfd as a one-shot absolute timer expiring at `deadline`.
    ///
    /// `timerfd_settime` resets the kernel's expiration counter as part of
    /// re-arming, so any unread expirations from the previous arming are
    /// implicitly discarded — that is what makes it safe for
    /// [`crate::interval::OsInterval`] to call `arm` between ticks without
    /// an explicit `disarm`/drain in between.
    pub(super) fn arm(&mut self, deadline: Instant) {
        // Absolute `CLOCK_MONOTONIC` time corresponding to `deadline`.
        // If the deadline is before our epoch (e.g. an immediate tick),
        // `saturating_duration_since` clamps the delta to zero and we
        // arm at `epoch_monotonic`, a point already in the past — the
        // kernel honors that as a fire-immediately request.
        let delta = deadline.saturating_duration_since(self.epoch_tokio);
        let it_value = add_duration(self.epoch_monotonic, delta);

        let new_value = libc::itimerspec {
            it_interval: ZERO_TIMESPEC,
            it_value,
        };
        let rc = unsafe {
            libc::timerfd_settime(
                self.fd.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &new_value,
                ptr::null_mut(),
            )
        };
        assert!(
            rc >= 0,
            "timerfd_settime failed: {}",
            io::Error::last_os_error()
        );
    }

    pub(super) fn disarm(&mut self) {
        let zero = libc::itimerspec {
            it_interval: ZERO_TIMESPEC,
            it_value: ZERO_TIMESPEC,
        };
        // Best-effort: failure here just means we'd see a stray expiration,
        // which the next `arm()` will overwrite anyway.
        unsafe {
            libc::timerfd_settime(self.fd.as_raw_fd(), 0, &zero, ptr::null_mut());
        }
        self.drain();
    }

    /// Drain any pending expirations sitting in the fd so the next
    /// `poll_expired` doesn't return `Ready` immediately for stale data.
    ///
    /// A single 8-byte read suffices: a timerfd's expiration counter is
    /// reset to zero atomically by the kernel on read, so we only loop to
    /// retry `EINTR`.
    fn drain(&self) {
        let mut buf = [0u8; 8];
        loop {
            let rc = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if rc < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // Either we drained 8 bytes, or `read` returned EAGAIN (nothing
            // to drain) / some other error we have no way to surface here.
            // In all of those cases the fd is now in the desired state.
            break;
        }
    }

    pub(super) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            let mut guard = match self.fd.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => panic!("timerfd poll_read_ready failed: {e}"),
                Poll::Pending => return Poll::Pending,
            };

            let read_result = guard.try_io(|inner| {
                let mut buf = [0u8; 8];
                let rc = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                    )
                };
                if rc == 8 {
                    Ok(())
                } else if rc < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    // Short read on timerfd "shouldn't happen"; treat it as
                    // would-block so we re-register interest.
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
            });

            match read_result {
                // Successful 8-byte read: timer expired at least once.
                Ok(Ok(())) => return Poll::Ready(()),
                // Real error from `read(2)` other than WouldBlock.
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => panic!("timerfd read failed: {e}"),
                // try_io saw WouldBlock and cleared readiness; re-poll.
                Err(_would_block) => {}
            }
        }
    }
}

const ZERO_TIMESPEC: libc::timespec = libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// Read the current `CLOCK_MONOTONIC` time, panicking on failure.
///
/// `clock_gettime(CLOCK_MONOTONIC, ...)` only fails for programmer errors
/// (bad pointer / unsupported clock), neither of which can happen here.
fn clock_gettime_monotonic() -> libc::timespec {
    let mut ts = MaybeUninit::<libc::timespec>::uninit();
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()) };
    assert!(
        rc >= 0,
        "clock_gettime(CLOCK_MONOTONIC) failed: {}",
        io::Error::last_os_error()
    );
    // SAFETY: `clock_gettime` returned success, so `ts` is initialized.
    unsafe { ts.assume_init() }
}

/// Add a [`Duration`] to a [`libc::timespec`], saturating on overflow of
/// the seconds component (relevant on 32-bit `time_t` platforms).
#[allow(clippy::similar_names)]
fn add_duration(ts: libc::timespec, d: Duration) -> libc::timespec {
    let secs = libc::time_t::try_from(d.as_secs()).unwrap_or(libc::time_t::MAX);
    let mut tv_sec = ts.tv_sec.saturating_add(secs);
    // Both `tv_nsec` (always in `0..1_000_000_000`) and `subsec_nanos()`
    // (always `< 1_000_000_000`) fit comfortably in `i64`, so the sum
    // can't overflow.
    let mut tv_nsec = ts.tv_nsec + i64::from(d.subsec_nanos());
    if tv_nsec >= 1_000_000_000 {
        tv_nsec -= 1_000_000_000;
        tv_sec = tv_sec.saturating_add(1);
    }
    libc::timespec { tv_sec, tv_nsec }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_duration_no_carry() {
        let base = libc::timespec {
            tv_sec: 10,
            tv_nsec: 100,
        };
        let out = add_duration(base, Duration::new(2, 50));
        assert_eq!(out.tv_sec, 12);
        assert_eq!(out.tv_nsec, 150);
    }

    #[test]
    fn add_duration_with_carry() {
        let base = libc::timespec {
            tv_sec: 10,
            tv_nsec: 999_999_900,
        };
        let out = add_duration(base, Duration::new(0, 200));
        assert_eq!(out.tv_sec, 11);
        assert_eq!(out.tv_nsec, 100);
    }

    #[test]
    fn add_duration_zero_is_identity() {
        let base = libc::timespec {
            tv_sec: 42,
            tv_nsec: 1234,
        };
        let out = add_duration(base, Duration::ZERO);
        assert_eq!(out.tv_sec, base.tv_sec);
        assert_eq!(out.tv_nsec, base.tv_nsec);
    }
}
