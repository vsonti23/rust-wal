//! Rough append throughput for each sync policy.
//!
//! Run with: `cargo run --release --example throughput [records] [payload_bytes]`
//!
//! The numbers depend heavily on the device and filesystem. On an SSD,
//! `Always` is typically limited by fsync latency (hundreds to a few
//! thousand appends per second), while `Never` runs at memory-copy speed
//! because it only writes into the page cache.

use std::time::Instant;

use rust_wal::{Result, SyncPolicy, Wal, WalOptions};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let records: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let payload_bytes: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(256);
    let payload = vec![0xA5u8; payload_bytes];

    println!("{records} records x {payload_bytes} bytes\n");
    println!(
        "{:<14} {:>12} {:>14} {:>10}",
        "policy", "elapsed (ms)", "appends/s", "MiB/s"
    );

    let policies = [
        SyncPolicy::Always,
        SyncPolicy::EveryN(10),
        SyncPolicy::EveryN(100),
        SyncPolicy::Never,
    ];
    for policy in policies {
        let dir = tempfile::tempdir()?;
        let mut wal = Wal::open_with(dir.path(), WalOptions::default().sync_policy(policy))?;

        let start = Instant::now();
        for _ in 0..records {
            wal.append(&payload)?;
        }
        // Count the final fsync so every policy ends with the same durability.
        wal.sync()?;
        let elapsed = start.elapsed().as_secs_f64();

        let rate = records as f64 / elapsed;
        let mib = rate * payload_bytes as f64 / (1024.0 * 1024.0);
        println!(
            "{:<14} {:>12.1} {:>14.0} {:>10.1}",
            format!("{policy:?}"),
            elapsed * 1000.0,
            rate,
            mib
        );
    }
    Ok(())
}
