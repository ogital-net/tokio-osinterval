//! macOS / *BSD `PeriodicInterval` backend.
//!
//! Single `kqueue` with one `EVFILT_TIMER` registration in
//! periodic mode (no `EV_ONESHOT`). The kernel keeps firing on its
//! own; `kev.data` on each delivery is the number of times the timer
//! expired since the previous delivery, which is exactly the
//! coalesce-count we want.

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

pub(super) struct PeriodicTimer {
    fd: AsyncFd<OwnedFd>,
}

impl PeriodicTimer {
    pub(super) fn new(period: Duration) -> io::Result<Self> {
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a freshly-created fd we own.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        set_cloexec(owned.as_raw_fd());

        let (data, fflags) = duration_to_timer(period);
        // SAFETY: `kevent` is plain-data; zeroed is a valid initial state.
        let mut kev: libc::kevent = unsafe { mem::zeroed() };
        kev.ident = TIMER_IDENT as _;
        kev.filter = libc::EVFILT_TIMER;
        // No EV_ONESHOT: the kernel keeps re-firing every `period`.
        kev.flags = libc::EV_ADD;
        kev.fflags = fflags | TIMER_FFLAGS_EXTRA;
        kev.data = data as _;

        let rc =
            unsafe { libc::kevent(owned.as_raw_fd(), &kev, 1, ptr::null_mut(), 0, ptr::null()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = AsyncFd::with_interest(owned, Interest::READABLE)?;
        Ok(Self { fd })
    }

    pub(super) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        loop {
            let mut guard = match self.fd.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `kevent` is plain-data; zeroed is a valid initial state.
            let mut buf: [libc::kevent; 1] = unsafe { mem::zeroed() };

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
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                } else {
                    // `buf` is sized 1 because exactly one
                    // `(ident, EVFILT_TIMER)` pair is registered on this
                    // private kqueue, so `kevent` cannot return more
                    // than one event per call.
                    let ev = &buf[0];
                    // The kernel surfaces asynchronous registration
                    // failures as an event with `EV_ERROR` set and the
                    // errno in `data`; without this check we would
                    // mis-report the errno as an expiration count.
                    if (u32::from(ev.flags) & u32::from(libc::EV_ERROR)) != 0 {
                        return Err(io::Error::from_raw_os_error(ev.data as i32));
                    }
                    // `data` is the number of expirations since the last
                    // delivery; documented as `>= 1` when delivered.
                    debug_assert!(ev.data >= 1);
                    Ok(ev.data as u64)
                }
            });

            match result {
                Ok(Ok(n)) => return Poll::Ready(Ok(n)),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Poll::Ready(Err(e)),
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
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// Convert a [`Duration`] into the `(data, fflags)` pair expected by
/// `EVFILT_TIMER`. Mirrors the helper in [`crate::sys::bsd`].
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
    let ms = d.as_nanos().div_ceil(1_000_000).max(1);
    (clamp_i64(ms), 0)
}

fn clamp_i64(value: u128) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
