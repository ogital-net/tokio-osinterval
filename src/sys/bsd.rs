//! macOS / *BSD backend using `kqueue` + `EVFILT_TIMER`, integrated with
//! the tokio reactor through [`tokio::io::unix::AsyncFd`].
//!
//! One `kqueue` is created per `OsInterval`. We arm a one-shot timer
//! (`EV_ONESHOT`) and re-arm on every tick so that the userspace
//! `MissedTickBehavior` policy in [`crate::interval`] stays authoritative.
//!
//! ## Relative vs. absolute arming
//!
//! Unlike Linux's `timerfd` (which supports `TFD_TIMER_ABSTIME` portably),
//! `EVFILT_TIMER`'s absolute-time flag is spelled differently on every
//! platform that has it (`NOTE_ABSOLUTE` on Apple, `NOTE_ABSTIME` on
//! FreeBSD, absent elsewhere) and requires using the OS's own clock
//! reference rather than [`tokio::time::Instant`]. We therefore always
//! arm with a relative duration computed from `Instant::now()`.
//!
//! The practical consequence is that any latency between reading the
//! clock and the kernel installing the timer is added to the achieved
//! wakeup time. For typical periods (≥1 ms) this is negligible.
//!
//! ## Time units
//!
//! `EVFILT_TIMER` accepts a `data` value whose unit is selected via
//! `fflags`. Where `NOTE_NSECONDS` is available (Apple, `FreeBSD`,
//! `DragonFly`) we use nanosecond precision. `NetBSD`/`OpenBSD` fall
//! back to the kqueue default of milliseconds, rounded up so we never
//! wake earlier than requested.

// FFI conversions to/from libc integer types (`isize`/`usize`/`c_int`)
// are pervasive in this module and the values involved are bounded by
// kernel-side invariants (small buffer lengths, expiration counts >= 1,
// nanosecond/millisecond durations clamped via `clamp_i64`). The
// truncation/sign lints are noise here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// `if rc < 0 / == 0 / else` is more readable than `match rc.cmp(&0)`
// for the syscall-return-code idiom used throughout this file.
#![allow(clippy::comparison_chain)]

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::time::Instant;

/// Arbitrary identifier for our single timer event on each per-interval
/// kqueue. The kqueue is private to this `Timer`, so any constant works.
const TIMER_IDENT: usize = 1;

// On Apple platforms, `EVFILT_TIMER` participates in the kernel's
// power-aware timer coalescing by default, which can add several
// milliseconds of slack to every expiration. `NOTE_CRITICAL` opts out
// of that and asks XNU to fire each tick as close to the scheduled
// time as it can. The flag does not exist on the BSDs.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const TIMER_FFLAGS_EXTRA: u32 = libc::NOTE_CRITICAL;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const TIMER_FFLAGS_EXTRA: u32 = 0;

pub(super) struct Timer {
    fd: AsyncFd<OwnedFd>,
}

impl Timer {
    pub(super) fn new() -> Self {
        // `kqueue(2)` takes no flags on the platforms we support; some
        // BSDs offer `kqueue1(KQUEUE_CLOEXEC)` but it isn't portable
        // (notably absent on macOS), so we set FD_CLOEXEC by hand.
        let raw = unsafe { libc::kqueue() };
        assert!(raw >= 0, "kqueue() failed: {}", io::Error::last_os_error());
        // SAFETY: `raw` is a freshly-created fd we own.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        set_cloexec(owned.as_raw_fd());

        let fd = AsyncFd::with_interest(owned, Interest::READABLE)
            .expect("registering kqueue fd with the tokio reactor (is a runtime active?)");
        Self { fd }
    }

    pub(super) fn arm(&mut self, deadline: Instant) {
        let delta = deadline.saturating_duration_since(Instant::now());
        let (data, fflags) = duration_to_timer(delta);

        // SAFETY: `kevent` is plain-data; zeroed is a valid initial state.
        let mut kev: libc::kevent = unsafe { mem::zeroed() };
        kev.ident = TIMER_IDENT as _;
        kev.filter = libc::EVFILT_TIMER;
        // EV_ADD implicitly enables the event, so EV_ENABLE is redundant.
        // Re-issuing EV_ADD on an existing identifier modifies it in place,
        // which is exactly the semantics we want for re-arming each tick.
        kev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        kev.fflags = fflags | TIMER_FFLAGS_EXTRA;
        kev.data = data as _;

        let rc = unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                &kev,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };
        assert!(
            rc >= 0,
            "kevent(arm) failed: {}",
            io::Error::last_os_error()
        );
    }

    pub(super) fn disarm(&mut self) {
        let mut kev: libc::kevent = unsafe { mem::zeroed() };
        kev.ident = TIMER_IDENT as _;
        kev.filter = libc::EVFILT_TIMER;
        kev.flags = libc::EV_DELETE;
        // Best-effort: ENOENT here just means the timer wasn't currently
        // registered, which is fine.
        unsafe {
            libc::kevent(
                self.fd.as_raw_fd(),
                &kev,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            );
        }
        self.drain();
    }

    /// Drain any already-queued expirations sitting on the kqueue so the
    /// next `poll_expired` doesn't immediately return `Ready` for stale
    /// data.
    ///
    /// Because we register a single `EV_ONESHOT` timer, at most one event
    /// can ever be pending here, but we loop defensively in case of
    /// `EINTR` and to absorb any future change to the registration model.
    fn drain(&self) {
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `kevent` is plain-data; zeroed is a valid initial state.
        let mut buf: [libc::kevent; 4] = unsafe { mem::zeroed() };
        loop {
            let rc = unsafe {
                libc::kevent(
                    self.fd.as_raw_fd(),
                    ptr::null(),
                    0,
                    buf.as_mut_ptr(),
                    buf.len() as libc::c_int,
                    &zero,
                )
            };
            if rc < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if (rc as usize) < buf.len() {
                // Drained everything (possibly zero events).
                break;
            }
            // Buffer was filled exactly; loop in case more events remain.
        }
    }

    pub(super) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            let mut guard = match self.fd.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => panic!("kqueue poll_read_ready failed: {e}"),
                Poll::Pending => return Poll::Pending,
            };

            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `kevent` is plain-data; zeroed is a valid initial state.
            let mut buf: [libc::kevent; 4] = unsafe { mem::zeroed() };

            let result = guard.try_io(|inner| {
                let rc = unsafe {
                    libc::kevent(
                        inner.as_raw_fd(),
                        ptr::null(),
                        0,
                        buf.as_mut_ptr(),
                        buf.len() as libc::c_int,
                        &zero,
                    )
                };
                if rc < 0 {
                    Err(io::Error::last_os_error())
                } else if rc == 0 {
                    // Spurious wakeup; tell try_io it's would-block so the
                    // readiness flag is cleared and we re-poll.
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                } else {
                    Ok(())
                }
            });

            match result {
                Ok(Ok(())) => return Poll::Ready(()),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => panic!("kevent(poll) failed: {e}"),
                Err(_would_block) => {}
            }
        }
    }
}

fn set_cloexec(fd: libc::c_int) {
    // SAFETY: `fd` is a valid open file descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            // Best-effort: a failure here only widens the FD beyond exec
            // boundaries, it doesn't affect correctness of the timer.
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// Convert a `Duration` into the `(data, fflags)` pair expected by
/// `EVFILT_TIMER`.
///
/// Uses `NOTE_NSECONDS` (nanosecond precision) where available;
/// otherwise falls back to the kqueue default of milliseconds, rounded
/// *up* so we never fire earlier than requested. A zero-or-near-zero
/// delta is mapped to the smallest representable positive value (1 unit)
/// — `EVFILT_TIMER` treats `data == 0` as "fire immediately" on most
/// platforms, but spelling it as `1` is portable and indistinguishable
/// in practice.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
))]
fn duration_to_timer(d: Duration) -> (i64, u32) {
    let ns = d.as_nanos().max(1);
    (clamp_i64(ns), libc::NOTE_NSECONDS)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
)))]
fn duration_to_timer(d: Duration) -> (i64, u32) {
    // Round nanoseconds up to the next whole millisecond.
    let ms = d.as_nanos().div_ceil(1_000_000).max(1);
    (clamp_i64(ms), 0)
}

fn clamp_i64(value: u128) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_i64_within_range() {
        assert_eq!(clamp_i64(0), 0);
        assert_eq!(clamp_i64(1_000_000), 1_000_000);
        assert_eq!(clamp_i64(i64::MAX as u128), i64::MAX);
    }

    #[test]
    fn clamp_i64_saturates() {
        assert_eq!(clamp_i64(i64::MAX as u128 + 1), i64::MAX);
        assert_eq!(clamp_i64(u128::MAX), i64::MAX);
    }

    #[test]
    fn duration_to_timer_zero_is_one_unit() {
        let (data, _) = duration_to_timer(Duration::ZERO);
        assert_eq!(data, 1);
    }

    #[test]
    fn duration_to_timer_uses_expected_unit() {
        // 2.5 ms → 2_500_000 ns (NOTE_NSECONDS) or 3 ms (rounded up).
        let (data, fflags) = duration_to_timer(Duration::from_micros(2_500));
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
        ))]
        {
            assert_eq!(fflags, libc::NOTE_NSECONDS);
            assert_eq!(data, 2_500_000);
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
        )))]
        {
            assert_eq!(fflags, 0);
            assert_eq!(data, 3);
        }
    }

    #[test]
    fn duration_to_timer_rounds_ms_up() {
        // Force the millisecond branch to be exercised in logic terms by
        // checking the helper directly: 1 ns of input must round to 1
        // unit regardless of which unit is in use.
        let (data, _) = duration_to_timer(Duration::from_nanos(1));
        assert_eq!(data, 1);
    }
}
