#![cfg(feature = "interval")]

use std::time::Duration;

use tokio::time::Instant;
use tokio_osinterval::{interval, interval_at, MissedTickBehavior};

#[tokio::test(flavor = "current_thread")]
async fn first_tick_is_immediate() {
    let start = Instant::now();
    let mut iv = interval(Duration::from_millis(50));
    iv.tick().await;
    assert!(start.elapsed() < Duration::from_millis(20));
}

#[tokio::test(flavor = "current_thread")]
async fn ticks_at_period() {
    let mut iv = interval(Duration::from_millis(50));
    iv.tick().await; // immediate
    let t0 = Instant::now();
    iv.tick().await;
    iv.tick().await;
    let elapsed = t0.elapsed();
    // Two periods: ~100ms. Allow generous slack for CI.
    assert!(
        elapsed >= Duration::from_millis(90),
        "elapsed was {elapsed:?}",
    );
    assert!(
        elapsed < Duration::from_millis(300),
        "elapsed was {elapsed:?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn interval_at_in_future_delays_first_tick() {
    let start = Instant::now();
    let when = start + Duration::from_millis(60);
    let mut iv = interval_at(when, Duration::from_millis(50));
    iv.tick().await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50),
        "elapsed was {elapsed:?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reset_immediately_fires_now() {
    let mut iv = interval(Duration::from_secs(60));
    iv.tick().await; // consume the immediate first tick
    iv.reset_immediately();
    let t0 = Instant::now();
    iv.tick().await;
    assert!(t0.elapsed() < Duration::from_millis(50));
}

#[tokio::test(flavor = "current_thread")]
async fn missed_tick_behavior_roundtrip() {
    let mut iv = interval(Duration::from_millis(10));
    assert_eq!(iv.missed_tick_behavior(), MissedTickBehavior::Burst);
    iv.set_missed_tick_behavior(MissedTickBehavior::Skip);
    assert_eq!(iv.missed_tick_behavior(), MissedTickBehavior::Skip);
    assert_eq!(iv.period(), Duration::from_millis(10));
}

#[tokio::test(flavor = "current_thread")]
async fn sub_millisecond_period_ticks() {
    // Smoke test that sub-ms periods are honored on platforms with
    // high-resolution backends (timerfd / kqueue / Win HR timer).
    // We only assert that 50 * 500us ticks complete within a generous
    // upper bound, not on per-tick precision.
    let mut iv = interval(Duration::from_micros(500));
    iv.tick().await; // immediate
    let t0 = Instant::now();
    for _ in 0..50 {
        iv.tick().await;
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_micros(500 * 50 - 100),
        "elapsed was {elapsed:?} (expected ~25ms)",
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "elapsed was {elapsed:?} (expected ~25ms)",
    );
}
