//! Parity tests: confirm that `OsInterval` produces tick instants
//! comparable to `tokio::time::Interval` for each `MissedTickBehavior`,
//! within a generous jitter tolerance.
//!
//! These run against the active platform backend (kqueue on macOS/BSD,
//! timerfd on Linux, threadpool timer on Windows, fallback elsewhere).
//! They use real time — `OsInterval` bypasses tokio's pauseable clock by
//! design, so `tokio::time::pause()` is not applicable.
//!
//! All tests are `#[ignore]` because they measure wall-clock scheduling
//! and are flaky on shared CI runners (notably macOS GitHub Actions),
//! where backend setup latency alone can exceed the parity tolerance.
//! Run locally with `cargo test --test parity -- --ignored`.

#![cfg(feature = "interval")]

use std::time::Duration;

use tokio::time::Instant;
use tokio_osinterval::MissedTickBehavior as OsBehavior;

/// Period used by all parity tests. Long enough that ~ms-scale scheduler
/// jitter doesn't dominate, short enough to keep tests fast.
const PERIOD: Duration = Duration::from_millis(50);

/// Tolerance applied to "tick N of `OsInterval` should be near tick N of
/// `tokio::time::Interval`". Generous to absorb scheduler latency on busy
/// CI runners.
const TOL: Duration = Duration::from_millis(40);

fn close(a: Duration, b: Duration, tol: Duration) -> bool {
    a.abs_diff(b) <= tol
}

fn assert_offsets_close(label: &str, os: &[Duration], tk: &[Duration]) {
    assert_eq!(os.len(), tk.len(), "{label}: tick count mismatch");
    for (i, (a, b)) in os.iter().zip(tk.iter()).enumerate() {
        assert!(
            close(*a, *b, TOL),
            "{label}: tick {i} differs: os={a:?} tokio={b:?} tol={TOL:?}",
        );
    }
}

async fn collect_os(behavior: OsBehavior, pre_sleep: Duration, n: usize) -> Vec<Duration> {
    let start = Instant::now();
    let mut iv = tokio_osinterval::interval(PERIOD);
    iv.set_missed_tick_behavior(behavior);
    iv.tick().await; // consume the immediate first tick
    if !pre_sleep.is_zero() {
        tokio::time::sleep(pre_sleep).await;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        iv.tick().await;
        out.push(start.elapsed());
    }
    out
}

async fn collect_tokio(
    behavior: tokio::time::MissedTickBehavior,
    pre_sleep: Duration,
    n: usize,
) -> Vec<Duration> {
    let start = Instant::now();
    let mut iv = tokio::time::interval(PERIOD);
    iv.set_missed_tick_behavior(behavior);
    iv.tick().await; // consume the immediate first tick
    if !pre_sleep.is_zero() {
        tokio::time::sleep(pre_sleep).await;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        iv.tick().await;
        out.push(start.elapsed());
    }
    out
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "real-time parity test; flaky on shared CI runners"]
async fn burst_no_miss() {
    let os = collect_os(OsBehavior::Burst, Duration::ZERO, 4).await;
    let tk = collect_tokio(tokio::time::MissedTickBehavior::Burst, Duration::ZERO, 4).await;
    assert_offsets_close("burst/no-miss", &os, &tk);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "real-time parity test; flaky on shared CI runners"]
async fn burst_with_missed_ticks() {
    // Sleep through ~3 periods. Both impls should "burst" catch-up ticks
    // immediately, then resume the original cadence.
    let pre = Duration::from_millis(160); // ~3.2 * PERIOD
    let os = collect_os(OsBehavior::Burst, pre, 5).await;
    let tk = collect_tokio(tokio::time::MissedTickBehavior::Burst, pre, 5).await;
    assert_offsets_close("burst/missed", &os, &tk);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "real-time parity test; flaky on shared CI runners"]
async fn delay_with_missed_ticks() {
    // After missed ticks, both should reset cadence to "now + period"
    // rather than catching up.
    let pre = Duration::from_millis(160);
    let os = collect_os(OsBehavior::Delay, pre, 4).await;
    let tk = collect_tokio(tokio::time::MissedTickBehavior::Delay, pre, 4).await;
    assert_offsets_close("delay/missed", &os, &tk);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "real-time parity test; flaky on shared CI runners"]
async fn skip_with_missed_ticks() {
    // After missed ticks, both should snap forward to the next aligned
    // multiple of `period` from the original schedule.
    let pre = Duration::from_millis(160);
    let os = collect_os(OsBehavior::Skip, pre, 4).await;
    let tk = collect_tokio(tokio::time::MissedTickBehavior::Skip, pre, 4).await;
    assert_offsets_close("skip/missed", &os, &tk);
}
