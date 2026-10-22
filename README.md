# rust-wal

[![CI](https://github.com/vsonti23/rust-wal/actions/workflows/ci.yml/badge.svg)](https://github.com/vsonti23/rust-wal/actions/workflows/ci.yml)

A segmented, append-only write-ahead log (WAL) in Rust, with length-prefixed
checksummed records, sequential replay, crash recovery, and configurable
durability.

A WAL is the component that lets a database promise "once I said yes, your
write survives a crash". Every change is appended to the log and made durable
*before* it is applied anywhere else; after a crash, the log is replayed to
rebuild state. This crate implements that log with the safety properties a
production system needs, in about 1,400 lines of heavily commented, dependency-light Rust
(`crc32fast` and `thiserror`).

## Features

- **Length-prefixed binary records** with a 16-byte header: payload length,
  CRC32, and a monotonically increasing log sequence number (LSN).
- **Per-record checksum validation** covering the length, LSN, and payload.
- **Configurable durability**: fsync on every append, every N appends
  (group commit), or never (OS-managed write-back), plus explicit `sync()`.
- **Crash recovery on open**: a torn or truncated tail is cut back to the
  last valid record; corruption in the middle of the log is reported as an
  error instead of silently discarding acknowledged data.
- **Segment files** with size-based rollover and `truncate_before(lsn)` to
  delete segments that a checkpoint has made obsolete.
- **Sequential replay** through a standard Rust iterator, from the start or
  from any LSN.
- **Fail-stop on I/O errors**: a failed write or fsync poisons the handle,
  because retrying a failed fsync is not safe on Linux.
- Exhaustive and randomized crash tests (every truncation point, every bit
  flip, zero-filled tails, out-of-order damage).

## Quick start

```rust
use rust_wal::{Lsn, SyncPolicy, Wal, WalOptions};

fn main() -> rust_wal::Result<()> {
    let opts = WalOptions::default().sync_policy(SyncPolicy::Always);
    let mut wal = Wal::open_with("data/wal", opts)?;

    // Append returns the record's LSN. With SyncPolicy::Always, Ok means durable.
    let lsn: Lsn = wal.append(b"SET user:1 alice")?;
    wal.append(b"SET user:2 bob")?;

    // Replay everything (e.g. on startup), oldest first.
    for record in wal.iter()? {
        let record = record?;
        println!("{}: {}", record.lsn, String::from_utf8_lossy(&record.data));
    }

    // Replay only what came after a checkpoint, then drop obsolete segments.
    for record in wal.iter_from(lsn)? {
        let _ = record?;
    }
    wal.truncate_before(lsn)?;
    Ok(())
}
```

Add to `Cargo.toml`:

```toml
[dependencies]
rust-wal = { git = "https://github.com/vsonti23/rust-wal" }
```

## API overview

| Item | Purpose |
| --- | --- |
| `Wal::open(dir)` / `Wal::open_with(dir, opts)` | Create or open a log and run recovery |
| `wal.append(&[u8]) -> Result<Lsn>` | Append one record |
| `wal.sync()` | Force appended records to stable storage (`fdatasync`) |
| `wal.iter()` / `wal.iter_from(lsn)` | Sequential replay, yields `Result<Record>` |
| `wal.truncate_before(lsn)` | Delete whole segments older than `lsn` |
| `wal.recovery_info()` | What recovery found and truncated |
| `WalOptions` | `sync_policy`, `max_segment_bytes`, `max_record_bytes`, `recovery_mode` |

## On-disk format

A log is a directory of segment files. Each file is named after the LSN of
its first record, zero-padded to 20 digits so that lexical order equals LSN
order:

```text
data/wal/
  00000000000000000001.wal    records 1 .. 812     (sealed, fsynced)
  00000000000000000813.wal    records 813 ..       (active, append-only)
```

Each segment starts with an 8-byte header, followed by records:

```text
Segment file
+----------------+----------+----------+-----+----------+
| segment header | record 1 | record 2 | ... | record n |
|    8 bytes     |          |          |     |          |
+----------------+----------+----------+-----+----------+

Segment header
+---------------------+----------------------------+
| magic "RWAL" (4 B)  | format version u32 LE (4 B)|
+---------------------+----------------------------+

Record (all integers little-endian)
 0        4        8                16
 +--------+--------+----------------+---------------------------+
 | length |  crc32 |      lsn       |  payload (length bytes)   |
 |  u32   |  u32   |      u64       |                           |
 +--------+--------+----------------+---------------------------+
           crc32 = CRC32(length ++ lsn ++ payload)
```

- **length** lets the reader find the next record without any delimiter or
  escaping, and lets recovery tell a short payload from a complete one.
- **crc32** (IEEE, via `crc32fast`, hardware-accelerated) detects torn writes
  and bit rot. It covers the length and LSN, not just the payload, so a
  damaged header cannot be mistaken for a valid record.
- **lsn** is the record's position in the log: it starts at 1 and increases
  by exactly one per record. Storing it in every record lets recovery detect
  misplaced, duplicated, or missing data even when checksums pass.

Payloads are capped by `max_record_bytes` (default 16 MiB). Recovery refuses
to trust a larger length field, which prevents a corrupted header from
causing a huge allocation.

## Durability and sync policies

`write()` only copies bytes into the operating system's page cache. Data is
on stable storage only after `fsync`/`fdatasync` returns. The sync policy
decides when that happens:

| Policy | fsync | Survives process crash | Survives power loss / kernel panic | Relative cost |
| --- | --- | --- | --- | --- |
| `Always` | after every append | yes | yes, every acknowledged append | highest latency per append |
| `EveryN(n)` | after every n appends | yes | all but at most the last n - 1 appends | amortized |
| `Never` | only on explicit `sync()` | yes | only what the OS flushed or you synced | cheapest |

Additional guarantees, independent of policy:

- New segment files are fsynced, and so is the directory, so the file's
  existence is durable, not just its contents.
- A segment is always fsynced before the next one is created ("sealed").
- `truncate_before` fsyncs the directory after deleting files.
- After a failed write or fsync, the `Wal` returns `Error::Poisoned` for every
  further call; reopen it to recover from what is really on disk.

Sample output of `cargo run --release --example throughput` (5,000 x 256-byte
records, Linux VM with SSD-backed storage; your numbers will differ):

```text
policy         elapsed (ms)      appends/s      MiB/s
Always                695.7           7187        1.8
EveryN(10)             86.8          57632       14.1
EveryN(100)            13.3         375897       91.8
Never                   4.6        1088532      265.8
```

## Recovery semantics

`Wal::open` reads and validates every record in every segment before
returning.

**Torn tail: truncated.** In the final segment, an invalid record is treated
as an interrupted append, and the file is truncated back to the end of the
last valid record, if:

- fewer bytes than a header remain (truncated header),
- the header announces more payload than the file contains (truncated payload),
- the checksum fails and the record is the last thing in the file, or
- the checksum or length is invalid and only zero bytes follow it (some
  filesystems expose zero-filled blocks after a crash).

The truncation is fsynced, and `recovery_info()` reports the offset, bytes
discarded, and reason. Subsequent appends continue from the next LSN.

**Mid-log corruption: error.** If an invalid record is followed by more
non-zero data, the damage cannot be explained by a crash during an append.
Opening fails with `Error::Corruption { path, offset, kind }` and the file is
left untouched. Truncating there would silently delete records that were
acknowledged, possibly fsynced, and possibly already acted on by clients.
Surfacing the error lets an operator restore from a replica or backup.

**Sealed segments: strict.** Non-final segments were fsynced before the next
segment was created, so any damage there, including a short file, is
corruption. Missing segments (an LSN gap between files) are also an error.

**Checksum-valid records with the wrong LSN: error.** A valid CRC means the
bytes are exactly what a writer produced, so a sequencing break is a bug or
a misplaced file, never a torn write.

**`RecoveryMode::PointInTime`.** With `SyncPolicy::EveryN` or `Never`,
several unsynced records can be in flight and the kernel may persist them out
of order, so a crash can damage an earlier unsynced record while a later one
survives. The default (`TolerateTornTail`) reports that as corruption.
`PointInTime` instead truncates the final segment at the first invalid record,
accepting the loss of the unsynced suffix. Sealed segments remain strict.

The final record of the log is inherently ambiguous: a bit flip in it looks
exactly like a torn write, so it is truncated in either mode. `docs/DESIGN.md`
discusses this tradeoff.

## Testing

```sh
cargo test                                    # unit, integration, and doc tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run --example demo                      # append, simulated crash, recovery
cargo run --release --example throughput      # compare sync policies
```

The suite (45 tests) includes:

- Round trip, empty log, reopen and replay, `iter_from`, large (8 MiB) and
  empty records, record size limits, option validation.
- Crash images: a truncated header at every length, a truncated payload at
  every length, and **the file cut at every single byte**, each checked to
  recover exactly the complete records and accept new appends.
- Bit flips in the tail (truncated) and mid-log (rejected, file untouched),
  zero-filled tails, garbage length fields, torn segment headers, wrong LSNs.
- Segment rollover, cross-segment replay, `truncate_before`, damaged sealed
  segments, and missing segments.
- A unit test that every single-bit flip in a record is detected.
- A deterministic randomized test: 300 random crash images, each of which
  must either recover an exact prefix of the written records or fail with
  `Error::Corruption`, never return wrong data.

CI runs formatting, clippy, tests, and the examples on Linux and macOS.

## Design notes

[`docs/DESIGN.md`](docs/DESIGN.md) explains every design decision in plain
language: why WALs exist, the record format, `write` vs `fsync` vs
`fdatasync`, the page cache, torn writes, tail vs mid-log corruption,
segmenting and checkpointing, how PostgreSQL, RocksDB, Kafka, and etcd use
WALs, and a set of interview-style questions with answers.

## Non-goals and possible extensions

- **Single writer.** One `Wal` per directory; there is no inter-process lock
  (a lock file via `File::lock`, stabilized in Rust 1.89, would add one).
- **No background sync thread.** A time-based policy ("fsync every 10 ms")
  would need one; `EveryN` plus explicit `sync()` covers group commit.
- **No preallocation or `O_DIRECT`.** Both are common optimizations in
  production WALs and are discussed in the design notes.
- **No compression or encryption** of payloads.

## License

MIT. See [LICENSE](LICENSE).
