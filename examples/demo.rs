//! End-to-end walkthrough: append, reopen, replay, simulate a crash, recover.
//!
//! Run with: `cargo run --example demo`

use std::fs::{self, OpenOptions};
use std::io::Write;

use rust_wal::{Result, SyncPolicy, Wal, WalOptions};

fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let opts = WalOptions::default().sync_policy(SyncPolicy::Always);
    println!("WAL directory: {}\n", dir.path().display());

    // 1. Append a few commands, as a key-value store would before applying them.
    {
        let mut wal = Wal::open_with(dir.path(), opts.clone())?;
        for cmd in ["SET user:1 alice", "SET user:2 bob", "DEL user:1"] {
            let lsn = wal.append(cmd.as_bytes())?;
            println!("appended LSN {lsn}: {cmd}");
        }
    } // `wal` is dropped here, closing the file.

    // 2. Simulate a crash in the middle of the next append: write only part
    //    of a record directly into the segment file.
    let segment = fs::read_dir(dir.path())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .expect("segment file exists");
    let size_before = fs::metadata(&segment)?.len();
    OpenOptions::new()
        .append(true)
        .open(&segment)?
        .write_all(&[0x2A, 0x00, 0x00, 0x00, 0xDE, 0xAD])?;
    println!(
        "\nsimulated crash: {} bytes -> {} bytes (6-byte torn header)",
        size_before,
        fs::metadata(&segment)?.len()
    );

    // 3. Reopen. Recovery truncates the torn tail back to the last valid record.
    let mut wal = Wal::open_with(dir.path(), opts)?;
    let info = wal.recovery_info();
    println!(
        "\nrecovered {} records from {} segment(s)",
        info.records, info.segments
    );
    if let Some(t) = &info.truncated {
        println!(
            "truncated {} bytes at offset {} ({})",
            t.bytes_discarded, t.offset, t.reason
        );
    }

    // 4. Replay, then keep appending: LSNs continue where the valid log ended.
    println!("\nreplay:");
    for record in wal.iter()? {
        let record = record?;
        println!(
            "  LSN {}: {}",
            record.lsn,
            String::from_utf8_lossy(&record.data)
        );
    }
    let lsn = wal.append(b"SET user:3 carol")?;
    println!("\nappended LSN {lsn} after recovery");
    Ok(())
}
