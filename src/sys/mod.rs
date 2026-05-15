//! Platform-specific timer backends.
//!
//! Each backend implements a small interface: arm a one-shot wakeup at an
//! absolute [`tokio::time::Instant`], and poll for that wakeup. All
//! interval scheduling policy lives in [`crate::interval`].
//!
//! Backend selection is done at compile time via `cfg` attributes.
//! When the `os-native` feature is disabled, every target falls back
//! to the `tokio::time::sleep_until`-based backend.

use std::task::{Context, Poll};

use tokio::time::Instant;

// Always compiled so that platform stubs (e.g. bsd.rs, windows.rs in early
// milestones) can re-export it; suppress dead-code warnings when a native
// backend on the active target doesn't use it.
#[allow(dead_code)]
mod fallback;

// Native backend selection. `target_os` values are mutually exclusive,
// so these `cfg` arms can never overlap.
#[cfg(all(feature = "os-native", any(target_os = "linux", target_os = "android")))]
mod linux;
#[cfg(all(feature = "os-native", any(target_os = "linux", target_os = "android")))]
use linux as imp;

#[cfg(all(
    feature = "os-native",
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ),
))]
mod bsd;
#[cfg(all(
    feature = "os-native",
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ),
))]
use bsd as imp;

#[cfg(all(feature = "os-native", windows))]
mod windows;
#[cfg(all(feature = "os-native", windows))]
use windows as imp;

#[cfg(any(
    not(feature = "os-native"),
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        windows,
    )),
))]
use fallback as imp;

/// One-shot, re-armable timer backed by an OS facility (or the portable
/// fallback). Constructed in a disarmed state.
pub(crate) struct Timer(imp::Timer);

impl Timer {
    pub(crate) fn new() -> Self {
        Timer(imp::Timer::new())
    }

    /// Arm the timer to wake the current task at `deadline`. Replaces any
    /// previously armed deadline.
    pub(crate) fn arm(&mut self, deadline: Instant) {
        self.0.arm(deadline);
    }

    /// Cancel any pending wakeup. Safe to call when not armed.
    pub(crate) fn disarm(&mut self) {
        self.0.disarm();
    }

    /// Poll for the armed deadline to elapse.
    ///
    /// Must only be called after [`Timer::arm`]; otherwise returns
    /// `Poll::Pending` indefinitely.
    pub(crate) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.0.poll_expired(cx)
    }
}

impl std::fmt::Debug for Timer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timer").finish_non_exhaustive()
    }
}
