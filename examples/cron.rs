//! Minimal cron-style scheduler driven by [`PeriodicInterval`].
//!
//! Demonstrates the typical "wake every N seconds, do work, log if we
//! fell behind" loop. Build with:
//!
//! ```sh
//! cargo run --example cron --features periodic
//! ```

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
)))]
fn main() {
    eprintln!("the `cron` example requires a platform with a PeriodicInterval backend");
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
use std::time::Duration;

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
use tokio_osinterval::PeriodicInterval;

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    // In a real cron-style job this would be 60s or longer; we use 500ms
    // here so the example finishes quickly.
    let mut iv = PeriodicInterval::new(Duration::from_millis(500))?;

    for tick in 1..=5 {
        let n = iv.tick().await?;
        if n > 1 {
            eprintln!("tick {tick}: fell behind by {} fires", n - 1);
        } else {
            println!("tick {tick}: on schedule");
        }

        // Simulate occasional slow work that overruns the period.
        if tick == 2 {
            std::thread::sleep(Duration::from_millis(1_200));
        }
    }

    Ok(())
}
