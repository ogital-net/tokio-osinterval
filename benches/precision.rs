//! Criterion benches comparing the precision/drift of [`OsInterval`]
//! against [`tokio::time::Interval`].
//!
//! Each benchmark runs a fixed number of ticks at a small period and
//! reports the total elapsed time per iteration. Lower mean = less drift
//! per tick; lower variance across samples = less jitter.
//!
//! Run with:
//!
//! ```sh
//! cargo bench --bench precision
//! ```
//!
//! Note: results depend heavily on platform timer resolution, scheduler
//! load, and (on Windows) whether the system timer is at "high
//! resolution". On Linux without `PREEMPT_RT`, both impls bottom out
//! around ~1 ms; on macOS, kqueue can deliver sub-ms; on Windows, the
//! threadpool timer is typically ~1 ms with a 0.5 ms floor.

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;

/// Period used by the benches. Small enough that timer mechanics dominate
/// over loop overhead, large enough to actually require kernel waits on
/// every platform we target.
const PERIOD: Duration = Duration::from_millis(2);

/// Number of ticks measured per iteration (in addition to the immediate
/// first tick, which we discard).
const TICKS: usize = 50;

fn bench_steady_state(c: &mut Criterion) {
    let rt = Runtime::new().expect("build tokio runtime");

    let mut group = c.benchmark_group("steady_state_50_ticks_2ms");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(8));

    group.bench_function("os_interval", |b| {
        b.to_async(&rt).iter(|| async {
            let mut iv = tokio_osinterval::interval(PERIOD);
            iv.tick().await; // discard the immediate first tick
            for _ in 0..TICKS {
                black_box(iv.tick().await);
            }
        });
    });

    group.bench_function("tokio_interval", |b| {
        b.to_async(&rt).iter(|| async {
            let mut iv = tokio::time::interval(PERIOD);
            iv.tick().await;
            for _ in 0..TICKS {
                black_box(iv.tick().await);
            }
        });
    });

    group.finish();
}

fn bench_short_period(c: &mut Criterion) {
    // Sub-period stress: at 500us we should see the OS-native backends
    // pull ahead of the tokio wheel on platforms that support sub-ms.
    const SHORT: Duration = Duration::from_micros(500);
    const SHORT_TICKS: usize = 100;

    let rt = Runtime::new().expect("build tokio runtime");

    let mut group = c.benchmark_group("steady_state_100_ticks_500us");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(8));

    group.bench_function("os_interval", |b| {
        b.to_async(&rt).iter(|| async {
            let mut iv = tokio_osinterval::interval(SHORT);
            iv.tick().await;
            for _ in 0..SHORT_TICKS {
                black_box(iv.tick().await);
            }
        });
    });

    group.bench_function("tokio_interval", |b| {
        b.to_async(&rt).iter(|| async {
            let mut iv = tokio::time::interval(SHORT);
            iv.tick().await;
            for _ in 0..SHORT_TICKS {
                black_box(iv.tick().await);
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_steady_state, bench_short_period);
criterion_main!(benches);
