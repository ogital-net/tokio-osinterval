//! `tokio-osinterval` provides [`OsInterval`], an alternative to
//! [`tokio::time::Interval`] that drives its periodic ticks from the
//! operating system's native async-capable timer facility instead of
//! tokio's userspace timer wheel.
//!
//! Backends:
//!
//! | Platform                         | Backend                                  |
//! |----------------------------------|------------------------------------------|
//! | Linux / Android                  | `timerfd_create(CLOCK_MONOTONIC, …)`     |
//! | macOS / iOS / *BSD               | `kqueue` + `EVFILT_TIMER`                |
//! | Windows                          | `CreateThreadpoolTimer`                  |
//! | Other / `--no-default-features`  | `tokio::time::sleep_until` fallback      |
//!
//! The public API mirrors [`tokio::time::Interval`] closely so swapping
//! is mostly an import change.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_osinterval::interval;
//!
//! # async fn run() {
//! let mut ticker = interval(Duration::from_millis(100));
//! for _ in 0..5 {
//!     ticker.tick().await;
//!     // do periodic work
//! }
//! # }
//! ```
//!
//! # Differences from [`tokio::time::Interval`]
//!
//! * Each `OsInterval` owns one kernel timer object (an fd or
//!   `PTP_TIMER`); creating it requires an active tokio runtime.
//! * The native backends bypass `tokio::time::pause()` — for tests that
//!   need a paused clock, build with `default-features = false`.
//! * Sub-millisecond periods are honored on platforms whose timers
//!   support them (kqueue with `NOTE_NSECONDS`, timerfd, Windows
//!   high-resolution timers).
//!
//! # Cargo features
//!
//! * **`interval`** *(default)* — enables [`OsInterval`] and the
//!   `interval` / `interval_at` constructors.
//! * **`os-native`** *(default)* — selects the platform-native backend
//!   for `OsInterval`. Disable to force the portable
//!   `tokio::time::sleep_until` fallback. Has no effect unless
//!   `interval` is also enabled.
//! * **`periodic`** — enables [`PeriodicInterval`], a stripped-down
//!   ticker driven by a single kernel-side periodic timer (Linux/BSD
//!   only).
//!
//! `interval` and `periodic` are independent: enable either, both, or
//! (with `default-features = false`) neither.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "interval")]
mod interval;
#[cfg(feature = "interval")]
mod sys;

#[cfg(all(
    feature = "periodic",
    any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    )
))]
mod periodic;

#[cfg(feature = "interval")]
#[cfg_attr(docsrs, doc(cfg(feature = "interval")))]
pub use interval::{interval, interval_at, MissedTickBehavior, OsInterval};

#[cfg(all(
    feature = "periodic",
    any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    )
))]
#[cfg_attr(docsrs, doc(cfg(feature = "periodic")))]
pub use periodic::{Expirations, PeriodicInterval};
