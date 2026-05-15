//! Linux/Android `PeriodicInterval` backend.
//!
//! Single `timerfd` armed once with `it_value = it_interval = period`.
//! The kernel maintains the schedule; reads return the number of
//! expirations that have occurred since the last read (coalescing
//! missed ticks naturally).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

pub(super) struct PeriodicTimer {
    fd: AsyncFd<OwnedFd>,
}

impl PeriodicTimer {
    pub(super) fn new(period: Duration) -> io::Result<Self> {
        let raw = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a freshly-created fd we own.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };

        let ts = duration_to_timespec(period);
        let new_value = libc::itimerspec {
            it_interval: ts,
            it_value: ts,
        };
        let rc = unsafe {
            libc::timerfd_settime(
                owned.as_raw_fd(),
                0, // relative
                &new_value,
                ptr::null_mut(),
            )
        };
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

            let result = guard.try_io(|inner| {
                let mut buf = [0u8; 8];
                let rc = unsafe {
                    libc::read(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                    )
                };
                if rc == 8 {
                    Ok(u64::from_ne_bytes(buf))
                } else if rc < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
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

/// Convert a [`Duration`] to a [`libc::timespec`], saturating on
/// overflow of the seconds component (relevant on 32-bit `time_t`
/// platforms).
fn duration_to_timespec(d: Duration) -> libc::timespec {
    let tv_sec = libc::time_t::try_from(d.as_secs()).unwrap_or(libc::time_t::MAX);
    // `subsec_nanos()` is always `< 1_000_000_000` and so fits in i64.
    let tv_nsec = i64::from(d.subsec_nanos());
    libc::timespec { tv_sec, tv_nsec }
}
