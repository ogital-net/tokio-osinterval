# tokio-osinterval

[![Crates.io](https://img.shields.io/crates/v/tokio-osinterval.svg)](https://crates.io/crates/tokio-osinterval)
[![Documentation](https://docs.rs/tokio-osinterval/badge.svg)](https://docs.rs/tokio-osinterval)
[![License](https://img.shields.io/crates/l/tokio-osinterval.svg)](https://github.com/ogital-net/tokio-osinterval/blob/main/LICENSE)


An alternative to [`tokio::time::Interval`] that drives its periodic ticks
from the operating system's native async-capable timer facility instead of
tokio's userspace timer wheel.

The goal is more accurate, lower-jitter periodic ticks (especially for
sub-ms to low-ms periods) while keeping a familiar `Interval`-shaped API.

## Why

`tokio::time::Interval` runs on top of tokio's coarse timer wheel
(default ~1 ms resolution). For most uses that is fine; for tight
heartbeats, audio/MIDI clocks, periodic polling under jitter budgets, or
sub-ms work, the wheel's slot size becomes the dominant source of error.

`OsInterval` instead asks the kernel:

| Platform                     | Backend                                        | tokio integration              |
|------------------------------|------------------------------------------------|--------------------------------|
| Linux / Android              | `timerfd_create(CLOCK_MONOTONIC, …)`           | `tokio::io::unix::AsyncFd`     |
| macOS / iOS / *BSD           | `kqueue` + `EVFILT_TIMER` (`NOTE_NSECONDS`)    | `tokio::io::unix::AsyncFd`     |
| Windows 10 1803+ / Server 2019+ | `CreateWaitableTimerExW(HIGH_RESOLUTION)`   | `CreateThreadpoolWait` callback → atomic + `Waker` |
| Windows (older)              | `CreateThreadpoolTimer` (auto-fallback)        | callback → atomic + `Waker`    |
| Other                        | `tokio::time::sleep_until` fallback            | n/a                            |

Disabling the `os-native` feature also forces the `sleep_until`
fallback on platforms that would otherwise use a native backend.

On Windows, the high-resolution waitable timer (Win10 1803+ /
Server 2019+) is detected once per process via a runtime probe; older
Windows versions transparently use the threadpool-timer path. Either
way, `OsInterval` owns one kernel object per instance — no shared
global reactor beyond what tokio already provides.

## Quick example

```rust
use std::time::Duration;
use tokio_osinterval::interval;

#[tokio::main]
async fn main() {
    let mut ticker = interval(Duration::from_millis(10));
    for _ in 0..100 {
        ticker.tick().await;
        // do periodic work
    }
}
```

The API mirrors `tokio::time::Interval` closely:

```rust
use std::time::Duration;
use tokio_osinterval::{interval_at, MissedTickBehavior};
use tokio::time::Instant;

# async fn run() {
let start = Instant::now() + Duration::from_secs(1);
let mut iv = interval_at(start, Duration::from_millis(50));
iv.set_missed_tick_behavior(MissedTickBehavior::Skip);

loop {
    let scheduled = iv.tick().await;
    // `scheduled` is the deadline this tick was scheduled for
}
# }
```

Available methods: `tick`, `poll_tick`, `period`, `missed_tick_behavior`,
`set_missed_tick_behavior`, `reset`, `reset_immediately`, `reset_after`,
`reset_at`. See the [API docs](https://docs.rs/tokio-osinterval) for
details.

## `MissedTickBehavior`

Identical semantics to `tokio::time::MissedTickBehavior`:

- **`Burst`** *(default)* — fire as fast as possible until caught up.
- **`Delay`** — slip the schedule: next tick is `period` after the missed
  tick was observed.
- **`Skip`** — keep the original schedule, snapping the next deadline to
  the next aligned multiple of `period`.

The userspace policy is shared by every backend; the kernel timer is
re-armed each tick rather than running in periodic mode, so behavior is
identical across platforms. See the [Design](#design-one-shot-re-arm-not-kernel-periodic)
section below for the rationale.

## Design: one-shot re-arm, not kernel-periodic

Every supported backend (`timerfd`, `EVFILT_TIMER`, waitable timer,
threadpool timer) *can* be configured as a true periodic timer that the
kernel re-fires on its own. `OsInterval` deliberately doesn't do that.
Each tick is a fresh one-shot arming computed in userspace from the
current `MissedTickBehavior` and the previous deadline.

This costs one extra syscall per tick (a few microseconds on Linux/BSD,
negligible above ~1 ms periods). In exchange:

- **Uniform `MissedTickBehavior` across platforms.** Only `Burst` maps
  cleanly to a kernel-periodic timer — and even then, Windows timers
  don't expose an overrun count, so a periodic-mode implementation
  would still hand-roll the count there. `Delay` and `Skip` *require* a
  re-arm at every tick to slip or snap the schedule, so periodic mode
  would mean two divergent code paths per backend. With one-shot re-arm,
  one userspace policy module drives all three behaviors identically on
  every target.

- **`interval_at(start, period)` works portably.** `EVFILT_TIMER` has no
  cross-platform "fire once at T, then every P" mode (Apple's
  `NOTE_ABSOLUTE` and FreeBSD's `NOTE_ABSTIME` are spelled differently
  and use OS-specific clock references). Computing each deadline in
  userspace side-steps that entirely.

- **No background wakeups while idle.** If the consumer holds an
  `OsInterval` but doesn't call `tick()` for a while (awaiting something
  else), nothing fires in the kernel. A periodic timer would keep
  queuing expirations and producing reactor wakeups for ticks no one is
  observing.

- **Per-tick rounding correction.** On platforms where the kernel timer
  has coarser resolution than `Duration` (NetBSD/OpenBSD round to whole
  milliseconds), each re-arm re-anchors against the original schedule
  (`prev + period`) so rounding error doesn't accumulate over thousands
  of ticks.

- **Simple cancel-safety and `reset_*`.** `tick()` only advances state
  on a successful expiration read, so dropping the future preserves the
  next deadline. `reset`, `reset_after`, `reset_at`, and
  `reset_immediately` are just userspace deadline updates plus a lazy
  re-arm on the next poll — no special cases for "the kernel is in
  mid-period".

The headline downside — an extra `timerfd_settime` / `kevent` /
`SetWaitableTimer` per tick — is the dominant cost only at sub-100 µs
periods, which is below the realistic precision floor of every supported
OS scheduler anyway. For the periods this crate is designed for
(sub-millisecond up through low-millisecond), kernel jitter dwarfs the
re-arm cost.

## Comparing precision vs `tokio::time::Interval`

The included criterion bench measures total elapsed time for N ticks at a
small period. Lower mean = less drift; tighter samples = less jitter.

```sh
cargo bench --bench precision
```

Sample run on macOS (Apple Silicon, kqueue backend, single-threaded
runtime):

| Bench                        | `OsInterval` | `tokio::time::Interval` | Ideal  |
|------------------------------|--------------|-------------------------|--------|
| 50 ticks @ 2 ms              | 100.4 ms     | 100.9 ms                | 100 ms |
| 100 ticks @ 500 µs           | 50.1 ms      | 51.2 ms                 | 50 ms  |

Numbers vary with platform and scheduler load. On Windows 10 1803+ the
high-resolution waitable timer typically delivers per-tick drift in the
300–600 µs range; on older Windows versions the threadpool-timer
fallback is bounded by the system tick (~15.6 ms by default).

## Cargo features

| Feature      | Default | Effect                                                 |
|--------------|:-------:|--------------------------------------------------------|
| `interval`   | ✅      | Enables `OsInterval` and `interval` / `interval_at`.   |
| `os-native`  | ✅      | Use the platform-native backend for `OsInterval`. No effect unless `interval` is also enabled. |
| `periodic`   |         | Enables [`PeriodicInterval`](#periodicinterval) (Linux/BSD only). |

`interval` and `periodic` are independent. Disable `interval` if you only
need the cron-style `PeriodicInterval`; disable `periodic` (the default)
if you only need the full-featured `OsInterval`. Disabling `os-native`
forces `OsInterval` onto the portable `tokio::time::sleep_until`
fallback everywhere, which is useful for parity testing or for keeping
the timer entirely inside tokio.

## `PeriodicInterval`

Behind the `periodic` feature flag, the crate also exposes
`PeriodicInterval`: a stripped-down ticker driven by a *single*,
kernel-side periodic timer.

```rust
use std::time::Duration;
use tokio_osinterval::PeriodicInterval;

# async fn run() -> std::io::Result<()> {
let mut iv = PeriodicInterval::new(Duration::from_secs(60))?;
loop {
    let n = iv.tick().await?;
    if n > 1 {
        eprintln!("fell behind by {} ticks", n - 1);
    }
    // run cron job
}
# }
```

Differences from `OsInterval`:

| Aspect                    | `OsInterval`                              | `PeriodicInterval`                  |
|---------------------------|-------------------------------------------|-------------------------------------|
| Kernel arming             | One-shot, re-armed each tick              | Periodic, armed once at construction|
| `MissedTickBehavior`      | Burst / Delay / Skip                      | Always coalesces (Burst-equivalent) |
| `reset*` methods          | ✅                                        | ❌                                  |
| First-tick semantics      | Fires immediately (or `interval_at`)      | Fires `period` after construction   |
| Returns from `tick()`     | `Instant` (scheduled deadline)            | `io::Result<u64>` (expiration count)|
| Platforms                 | Linux, *BSD, macOS, iOS, Windows, fallback| Linux, Android, *BSD, macOS, iOS    |
| Per-tick syscalls         | One re-arm per tick                       | Zero (just an fd read)              |

It exists for the cron / heartbeat case where:

- you want the lowest possible per-tick overhead,
- coarse resolution is fine,
- coalescing (Burst) is the only behavior you need,
- you don't need `reset_*` or phase control,
- and you're OK with Linux-or-BSD-only.

For everything else, prefer `OsInterval`.

## Caveats

- `OsInterval` deliberately bypasses tokio's pauseable test clock. If your
  tests rely on `tokio::time::pause()`, either use `tokio::time::Interval`
  directly or pull this crate in with the native backend disabled:
  ```toml
  tokio-osinterval = { version = "1", default-features = false, features = ["interval"] }
  ```
  (`default-features = false` alone removes `OsInterval` entirely — you
  must opt back in to `interval`.)
- `tick()` is `async` and requires an active tokio runtime, just like
  `tokio::time::interval`.
- The first `tick()` returns immediately (matching tokio); use
  `interval_at` to defer the first tick.
- `reset*` methods take effect on the next `poll_tick`/`tick` call: the
  kernel timer is re-armed lazily.

## MSRV

Rust 1.81

## License

Licensed under the [BSD 2-Clause License](LICENSE).

Copyright (c) 2026, Latigo LLC.

[`tokio::time::Interval`]: https://docs.rs/tokio/latest/tokio/time/struct.Interval.html
