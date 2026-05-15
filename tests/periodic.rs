#![cfg(all(
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

use std::time::Duration;

use tokio::time::Instant;
use tokio_osinterval::PeriodicInterval;

#[tokio::test(flavor = "current_thread")]
async fn ticks_at_period() {
    let period = Duration::from_millis(20);
    let mut iv = PeriodicInterval::new(period).expect("create periodic");

    let start = Instant::now();
    for _ in 0..5 {
        let n = iv.tick().await.expect("tick");
        assert!(n >= 1, "expiration count should be at least 1, got {n}");
    }
    let elapsed = start.elapsed();
    // First tick fires `period` after construction (no immediate fire),
    // so 5 ticks ≥ 5 * period. Allow generous slack for CI scheduling.
    assert!(
        elapsed >= period * 5,
        "5 ticks took {elapsed:?}, expected ≥ {:?}",
        period * 5
    );
    assert!(
        elapsed < period * 5 + Duration::from_millis(250),
        "5 ticks took {elapsed:?}, expected < {:?}",
        period * 5 + Duration::from_millis(250)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn coalesces_missed_ticks() {
    let period = Duration::from_millis(10);
    let mut iv = PeriodicInterval::new(period).expect("create periodic");

    // Block the task long enough that several ticks should accumulate
    // in the kernel before we read.
    std::thread::sleep(Duration::from_millis(60));

    let n = iv.tick().await.expect("tick");
    assert!(
        n >= 3,
        "expected coalesced count of at least 3 after 60ms sleep at 10ms period, got {n}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn period_accessor() {
    let period = Duration::from_millis(5);
    let iv = PeriodicInterval::new(period).expect("create periodic");
    assert_eq!(iv.period(), period);
}

#[test]
#[should_panic(expected = "must be non-zero")]
fn zero_period_panics() {
    // Construction outside a runtime is fine for the panic check
    // because the assert fires before any tokio call.
    let _ = PeriodicInterval::new(Duration::ZERO);
}
