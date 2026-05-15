//! Minimal heartbeat example.
//!
//! ```sh
//! cargo run --example heartbeat
//! ```

use std::time::Duration;

use tokio::time::Instant;
use tokio_osinterval::{interval, MissedTickBehavior};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut ticker = interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let start = Instant::now();
    for i in 0..10 {
        let scheduled = ticker.tick().await;
        let actual_offset = scheduled.saturating_duration_since(start);
        let drift = scheduled.elapsed();
        println!("tick {i:>2}: scheduled at +{actual_offset:?} (observed drift {drift:?})");
    }
}
