# Design notes

This document explains how `rust-wal` works and, more importantly, *why* each
decision was made. It is written to be read top to bottom by someone who is
comfortable with systems concepts but new to this codebase (and possibly to
Rust). Every section ends with the place in the code where the idea lives.

Contents:

1. [Why write-ahead logs exist](#1-why-write-ahead-logs-exist)
2. [The record format](#2-the-record-format)
3. [Write vs. flush vs. fsync vs. fdatasync](#3-write-vs-flush-vs-fsync-vs-fdatasync)
4. [Making new files durable: fsync the directory](#4-making-new-files-durable-fsync-the-directory)
5. [Sync policies and group commit](#5-sync-policies-and-group-commit)
6. [Torn writes and how they are detected](#6-torn-writes-and-how-they-are-detected)
7. [Tail corruption vs. mid-log corruption](#7-tail-corruption-vs-mid-log-corruption)
8. [When fsync fails](#8-when-fsync-fails)
9. [Segments and checkpointing](#9-segments-and-checkpointing)
10. [How real systems use WALs](#10-how-real-systems-use-wals)
11. [What this implementation deliberately does not do](#11-what-this-implementation-deliberately-does-not-do)
12. [Rust concepts used here](#12-rust-concepts-used-here)
13. [Interview questions and model answers](#13-interview-questions-and-model-answers)

---

## 1. Why write-ahead logs exist

A database keeps its real data in structures that are expensive to update
safely: B-tree pages, hash tables, SSTables, an in-memory map. Updating one of
these in place usually means changing several places on disk (a leaf page, a
parent page, a free-space map). If the machine loses power halfway through,
you are left with a structure that is neither the old version nor the new
one.

The write-ahead log solves this with one rule: **before changing the data
structure, append a description of the change to a log and make that log
entry durable.** Appending to the end of a file is the simplest, fastest
thing a disk can do, and an append is easy to validate after a crash. Once
the log entry is durable, the change is "committed": even if the process dies
a microsecond later, restart can read the log and redo the change.

This gives three things:

- **Atomicity and durability.** A change either has a complete, valid log
  record (it happened) or it does not (it did not happen). There is no
  in-between.
- **Performance.** The expensive random writes to the main data structure can
  be delayed and batched. Only the cheap sequential log append is on the
  critical path of a commit.
- **Replication for free.** The log is an ordered stream of every change.
  Shipping it to another machine and replaying it there is how most databases
  build replicas (PostgreSQL streaming replication, Raft logs in etcd, Kafka
  topics themselves).

The rest of this document is about doing that append and that replay
correctly when the disk, the OS, and the power supply are working against
you.

Code: the whole crate; the public entry point is `src/wal.rs`.

## 2. The record format

Each record is a fixed 16-byte header followed by the payload. All integers
are little-endian.

```text
 0        4        8                16
 +--------+--------+----------------+---------------------------+
 | length |  crc32 |      lsn       |  payload (length bytes)   |
 +--------+--------+----------------+---------------------------+
```

### Why a length prefix

A log needs a way to find where one record ends and the next begins. The two
options are delimiters (like newlines) and length prefixes.

Delimiters require escaping any occurrence of the delimiter inside the
payload, which costs CPU, makes the size on disk unpredictable, and is easy to
get wrong. A length prefix lets the reader jump straight to the next record
and lets the payload be arbitrary bytes.

The length also gives recovery a crucial piece of information: *how many bytes
should be here*. If the header says 500 bytes but the file ends after 200,
the record is incomplete, which is the signature of a crash during the write.

The length is a `u32`, so a record can be up to 4 GiB in principle. In
practice `max_record_bytes` (default 16 MiB) caps it. That cap also protects
recovery: a corrupted length field could claim 4 GiB, and a naive reader
would try to allocate that much memory. Recovery refuses any length above the
cap.

### Why a checksum, and why CRC32

A length prefix tells you whether bytes are *present*. It cannot tell you
whether they are the *right* bytes. After a crash, a file can have the right
size but contain old or garbage data in some sectors (section 6). Disks and
controllers also, rarely, return bits that are simply wrong ("bit rot").

The checksum is what turns "some bytes" into "the exact bytes the writer
wrote". The options:

| Checksum | Size | Speed | Notes |
| --- | --- | --- | --- |
| CRC32 / CRC32C | 4 B | very fast; hardware instructions on x86 and ARM | Detects all single-bit and burst errors up to 32 bits; designed for exactly this job |
| xxHash / CityHash | 8 B | very fast in software | Good general-purpose hashes; RocksDB supports xxHash for block checksums |
| SHA-256 | 32 B | much slower | Cryptographic; protects against deliberate tampering, which is not the threat model here |

CRC32 is the standard choice for log records (LevelDB, RocksDB, Kafka, and
etcd all use a CRC32 variant) because it is designed to catch exactly the
accidental errors storage produces, it is 4 bytes, and `crc32fast` uses the
CPU's hardware acceleration when available. (CRC32C, the "Castagnoli"
polynomial, has slightly better error detection and has a dedicated x86
instruction; it would be an equally good choice.)

**What the CRC covers matters.** Here it covers the length and the LSN as
well as the payload. If it only covered the payload, a flipped bit in the
length field would make the reader take the wrong number of bytes, and a
flipped bit in the LSN would go unnoticed. The unit test
`every_single_bit_flip_is_detected` in `src/record.rs` flips every bit of an
encoded record and checks that each one is caught.

### Why an LSN in every record

The log sequence number is the record's position in the log: 1, 2, 3, and so
on with no gaps. Other parts of a database use it as a pointer: "the data
page reflects everything up to LSN 812", "replica B has applied up to LSN
900", "the checkpoint covers LSN 1 through 5,000".

Storing the LSN inside each record (rather than only computing it by
counting) gives two benefits:

- **Self-describing records.** A record read in isolation, for example by a
  replica, says where it belongs.
- **An extra integrity check.** Recovery checks that each record's LSN is
  exactly one more than the previous one. A record with a valid checksum but
  the wrong LSN cannot be a torn write (the checksum proves the bytes are
  exactly what some writer wrote), so it indicates a misplaced file, a
  duplicated segment, or a bug. Recovery reports this as corruption.

Because the CRC includes the LSN, the same payload at a different position
produces a different checksum (unit test `checksum_binds_the_lsn`).

### Why a segment header

Each segment file starts with 8 bytes: the magic `RWAL` and a format version.
The magic catches "this is not a WAL file at all". The version leaves room to
change the record format later while still reading old files.

### Why little-endian, fixed-width fields

Fixed widths mean the header can be decoded with no parsing logic, and
little-endian matches every mainstream CPU today so encoding is essentially a
copy. The format is pinned by the golden test `byte_layout_is_stable`,
because the on-disk format is a compatibility contract: changing it silently
would make existing logs unreadable.

Code: `src/record.rs` (`encode_into`, `Header::decode`, `checksum`).

## 3. Write vs. flush vs. fsync vs. fdatasync

This is the single most important concept for durability, and the most
commonly misunderstood. There are several layers between your program and
the physical storage, and data can be lost at each one:

```text
 your Vec<u8>          (process memory)        lost if the process crashes
     |  write()
     v
 OS page cache         (kernel memory)         lost if the kernel panics or power fails
     |  fsync() / fdatasync()
     v
 device write cache    (drive's RAM)           lost on power failure unless the drive
     |  (FUA / cache flush, issued by fsync)   has power-loss protection
     v
 stable media          (NAND flash / platter)  survives
```

- **User-space buffering and `flush()`.** A `BufWriter` in Rust (or `FILE*`
  in C) collects writes in your process's memory. `flush()` hands them to the
  kernel with `write()`. This WAL does not use a user-space buffer for
  appends: each record is encoded into one buffer and passed to a single
  `write()` call, so there is no "forgot to flush" failure mode.
- **`write()`** copies bytes into the kernel's page cache and returns. The
  data is now visible to every other process reading the file, and it
  survives your process crashing. It does **not** survive power loss or a
  kernel panic. The kernel writes dirty pages to disk later, on its own
  schedule (on Linux, typically within about 30 seconds).
- **`fsync(fd)`** (Rust: `File::sync_all`) blocks until the file's data
  *and* all of its metadata (size, modification time, and so on) have been
  written to the device, and asks the device to flush its own write cache.
  When it returns successfully, the data survives power loss.
- **`fdatasync(fd)`** (Rust: `File::sync_data`) is the same, except it skips
  metadata that is not needed to read the data back. File size *is* needed,
  so after an append `fdatasync` still persists the new size; the
  modification time is not, so it can skip that inode update. This often
  saves an extra write per sync, which is why WALs (PostgreSQL defaults to
  `fdatasync` on Linux) prefer it. This crate uses `sync_data` for appends.

A note on macOS: there, a plain `fsync(2)` call does not force the drive to
flush its own write cache; the stronger `fcntl(F_FULLFSYNC)` does. Rust's
standard library already uses `F_FULLFSYNC` for `sync_all` and `sync_data` on
Apple platforms, so this crate gets the strong behaviour automatically, but C
code (and some databases) calling `fsync` directly does not.

Code: `Wal::append` and `Wal::sync` in `src/wal.rs`.

## 4. Making new files durable: fsync the directory

On POSIX file systems a file's *name* is not stored in the file. It is an
entry in the parent directory, and the directory is itself a file with its
own data blocks. `fsync` on a new file makes its contents and inode durable,
but not necessarily the directory entry that points to it. After a power
cut, the file's blocks can be on disk with no name referring to them, and
the file appears to have never existed.

So every time this WAL creates a segment, it:

1. creates the file and writes the segment header,
2. calls `sync_all` on the file,
3. opens the directory and calls `sync_all` on it.

The same applies to deletion: `truncate_before` fsyncs the directory after
removing segment files, so a deleted segment does not reappear after a crash.
(Reappearing would be mostly harmless here, but it would waste space and
could confuse a system that has already reused those LSNs elsewhere.)

This step is easy to forget and is a classic interview follow-up. The
well-known 2014 paper "All File Systems Are Not Created Equal" (Pillai et al.,
OSDI) found missing directory fsyncs and similar ordering bugs in many
popular applications.

On non-Unix platforms, directories cannot be opened as files, so
`sync_dir` is a no-op there.

Code: `create_segment` and `sync_dir` in `src/segment.rs`.

## 5. Sync policies and group commit

`fsync` is slow: it waits for the device. On a good NVMe drive it takes tens
to hundreds of microseconds; on a cloud network disk or a spinning disk it can
take milliseconds. If every append waits for its own `fsync`, throughput is
capped at roughly `1 / fsync_latency` appends per second, no matter how small
the records are.

The `SyncPolicy` enum exposes the three standard points on the tradeoff:

- **`Always`**: fsync after every append. `Ok(lsn)` means the record
  survives power loss. This is what a database does when a client asks for a
  synchronous commit.
- **`EveryN(n)`**: fsync after every `n` appends. This is a simple form of
  **group commit**: the cost of one fsync is shared by `n` records. A crash
  can lose up to `n - 1` appends that `append` already returned `Ok` for, so
  callers that need per-record durability should not acknowledge those
  records to *their* clients until a sync covers them.
- **`Never`**: the WAL never fsyncs automatically; the OS writes pages back
  eventually. This survives process crashes (the data is in the kernel) but
  not power loss. It is appropriate when durability comes from somewhere else
  (for example replication to other machines, which is Kafka's default
  model) or when the caller calls `sync()` at its own commit points.

The `throughput` example shows the difference concretely. On the development
machine `Always` managed about 7,000 appends per second and `Never` about one
million, a 150x gap that is entirely the cost of waiting for the device.

Real group commit in a database is usually *concurrent*: many client threads
append, one of them performs the fsync, and all of them are acknowledged
together when it completes. That gives `Always`-level guarantees with
`EveryN`-level throughput under load. PostgreSQL (`commit_delay`), MySQL
InnoDB, and RocksDB all do this. Building it on top of this crate means
wrapping the `Wal` in a mutex and having waiting writers block on a
condition variable until `sync` covers their LSN.

Regardless of policy, the WAL always fsyncs a segment before sealing it and
creating the next one (section 7 explains why that matters for recovery).

Code: `SyncPolicy` in `src/options.rs`; the `match` at the end of
`Wal::append`.

## 6. Torn writes and how they are detected

A **torn write** is a write that was only partially applied when the system
crashed. It happens because no layer of the stack promises that a multi-byte
write is atomic:

- The kernel splits a write into pages (4 KiB) and may write them back in any
  order.
- The device writes in sectors (512 bytes or 4 KiB). A crash can land
  between sectors of the same write.
- File systems such as ext4 in some modes can persist a file's new size
  before its new data blocks, so after a crash the end of the file reads back
  as zeros.

After a crash during an append, the end of the last segment can therefore
look like any of these:

| What is on disk | How it is detected | Code path |
| --- | --- | --- |
| Part of a header (fewer than 16 bytes) | Not enough bytes for a header | `TruncatedHeader` |
| A full header, part of the payload | Header length points past end of file | `TruncatedPayload` |
| All bytes present, some are stale or garbage | CRC mismatch | `ChecksumMismatch` |
| A header with a nonsense length | Length larger than `max_record_bytes` | `LengthTooLarge` |
| Zeros where the record should be | CRC mismatch (an all-zero header never validates) | `ChecksumMismatch` |

The length prefix catches the first two, and the checksum catches the rest.
Neither alone is enough: without the length you cannot tell "short" from
"complete", and without the checksum you cannot tell "complete" from
"correct".

The test `truncation_at_every_byte_of_the_file` cuts a log at every possible
byte offset and checks that recovery always yields exactly the records that
were complete, and that the log then accepts new appends at the right LSN.

Code: `scan_segment` in `src/segment.rs`.

## 7. Tail corruption vs. mid-log corruption

Detecting a bad record is the easy part. The hard part is deciding **what to
do about it**, because the same bytes can mean two very different things:

- **A torn tail.** The process crashed during an append. The damaged record
  was never acknowledged as durable (or, with relaxed sync policies, the
  caller accepted that it might be lost). The correct response is to cut it
  off and continue. Refusing to start would turn every crash into an outage.
- **Mid-log corruption.** A record in the middle of the log is bad, and valid
  records come after it. A crash during an append only ever damages the
  *end* of the log, so this cannot be a torn write. It is bit rot, a firmware
  bug, an operator editing files, or a bug in the WAL. The records after the
  damage were acknowledged, possibly fsynced, and possibly already acted on.
  Silently truncating would destroy them. The correct response is to stop and
  report the problem so a human or a higher-level system (restore from a
  replica, a backup, or a snapshot) can fix it.

This crate's rules, applied by `Wal::open_with`:

1. **Invalid data in the final segment is a torn tail if nothing valid can
   follow it**: the invalid record extends to (or past) the end of the file,
   or every byte after it is zero. Recovery truncates the file to the end of
   the last valid record, fsyncs the truncation, and reports what it did in
   `recovery_info()`.
2. **Invalid data followed by more non-zero data is corruption.** `open`
   returns `Error::Corruption { path, offset, kind }` and does not modify the
   file.
3. **Sealed segments are always strict.** A segment is fsynced before the
   next one is created, so by the time any later segment exists, every byte
   of the earlier one is durable. Any damage there, even at its very end, is
   corruption. Missing segments (LSN gaps between file names) are corruption.
4. **A checksum-valid record with the wrong LSN is always corruption**, even
   at the tail, for the reason given in section 2.

Two honest caveats, both of which are good interview discussion points:

- **The last record is ambiguous.** A bit flip in the final record of the log
  looks exactly like a torn write, so it is truncated, even if that record
  had been fsynced. No single-copy log can distinguish these cases; systems
  that care keep multiple replicas and compare.
- **Out-of-order persistence of unsynced data.** With `EveryN` or `Never`,
  several unsynced records can be in the page cache at the moment of the
  crash, and the kernel may have persisted a later one but not an earlier
  one. Rule 2 would then report corruption for data that was never promised
  to be durable. For that situation the crate offers
  `RecoveryMode::PointInTime`, which truncates the final segment at the
  *first* invalid record regardless of what follows, trading the
  corruption-detection of rule 2 for availability. RocksDB exposes the same
  choice through its `WALRecoveryMode` option (`kTolerateCorruptedTailRecords`,
  `kAbsoluteConsistency`, `kPointInTimeRecovery`, `kSkipAnyCorruptedRecords`),
  and the names here are borrowed from it.

Code: the recovery loop in `Wal::open_with` (`src/wal.rs`); the
classification in `scan_segment` (`src/segment.rs`); tests in
`tests/recovery.rs` and `tests/segments.rs`.

## 8. When fsync fails

In 2018 PostgreSQL developers discovered ("fsyncgate") that on Linux, when
`fsync` reports an I/O error, the kernel may already have **dropped the dirty
pages and marked them clean**. A second `fsync` then returns success, even
though the data never reached the disk. Software that retried `fsync` after a
failure could silently lose data. PostgreSQL's fix was to treat an fsync
failure as fatal (PANIC and recover from the WAL on restart).

This crate follows the same principle. If a `write` or `fsync` fails, the
`Wal` is **poisoned**: every subsequent call returns `Error::Poisoned`. A
failed `write` may also have left a partial record in the file. The only
correct recovery is to reopen the log, which re-reads what is actually on
disk and truncates any partial tail.

Code: the `poisoned` field and `check_poisoned` in `src/wal.rs`.

## 9. Segments and checkpointing

If a log only grows, it eventually fills the disk, and replay on startup gets
slower forever. The fix has two parts.

**Checkpointing** (owned by the application, not the WAL): periodically
persist the full state, for example by flushing dirty pages or writing a
snapshot, and remember the LSN it covers. After a checkpoint at LSN `c`,
recovery only needs to load the checkpoint and replay records after `c`
(`iter_from(c + 1)`). Records before `c` are no longer needed.

**Segmenting** (owned by the WAL) makes discarding old records cheap. Instead
of one huge file, the log is a sequence of files. When the active segment
would exceed `max_segment_bytes`, the WAL fsyncs it (sealing it) and starts a
new file named after the next LSN. Discarding old records is then just
deleting whole files with `truncate_before(lsn)`, rather than rewriting the
front of a file. File names encode the base LSN, zero-padded to 20 digits so
that sorting the names sorts the segments, and so that `iter_from` can find
the right file without opening the others.

Details worth knowing:

- `truncate_before` only deletes segments whose *every* record is below
  `lsn`, so a few records older than `lsn` may remain. That is always safe.
- It never deletes the active segment.
- It deletes the oldest segment first. A crash part-way through leaves a
  valid log that simply begins a bit earlier, never a log with a hole.
- A record larger than `max_segment_bytes` gets a segment to itself instead of
  being rejected.
- Segment boundaries are also what makes "sealed segments are strict"
  possible (section 7): they give recovery a point before which everything is
  known to have been fsynced.

Code: `roll_segment` and `truncate_before` in `src/wal.rs`; `src/segment.rs`;
tests in `tests/segments.rs`.

## 10. How real systems use WALs

**PostgreSQL** writes every change to the WAL (`pg_wal/`) in 16 MiB segment
files before modifying the corresponding data pages in shared buffers.
Records carry a CRC32C and an LSN, which is a byte position in the WAL
stream. Every data page stores the LSN of the last record that modified it,
and the buffer manager will not write a page to disk until the WAL is
durable up to that LSN; this ordering is the "write-ahead" rule. Checkpoints
flush dirty pages and let old segments be recycled. The same WAL stream is
shipped to standbys for streaming replication. Because a page write can be
torn, PostgreSQL also logs a full copy of each page the first time it is
modified after a checkpoint (`full_page_writes`).

**RocksDB / LevelDB** (LSM trees) append each write batch to a WAL and then
insert it into an in-memory memtable. When the memtable fills, it is written
out as an immutable SSTable file, after which the WAL segments it covers can
be deleted. The log format splits records into 32 KiB blocks with a 7-byte
header (CRC32C, length, type) so a reader can resynchronize at the next block
after damage. Durability is per-write (`WriteOptions::sync`), and recovery
behaviour is configurable (`WALRecoveryMode`, see section 7).

**Kafka** is, in a sense, nothing but a WAL exposed as a product. Each topic
partition is an append-only log split into segment files named by their base
offset (the same naming scheme as this crate), with a sparse offset index next
to each segment. Records are batched, and each batch carries a CRC32C. By
default Kafka does not fsync on every write; it relies on replication across
brokers (`acks=all`, `min.insync.replicas`) for durability and lets the OS
flush the page cache. Retention deletes whole old segments, just like
`truncate_before`.

**etcd** (and Raft implementations in general) persist the Raft log in a
segmented WAL before acknowledging entries to the leader. Records are
length-prefixed protobufs with a rolling CRC32C, and segment files are
preallocated to 64 MB and fsynced together with the directory. A Raft node
must fsync its log before replying to `AppendEntries`, because the protocol's
safety proof assumes that an acknowledged entry is durable. Periodic
snapshots let etcd discard old WAL segments.

## 11. What this implementation deliberately does not do

These are left out to keep the core small and easy to reason about. Each is
a reasonable interview "how would you extend this?" topic.

- **Concurrent group commit.** See section 5.
- **Preallocation.** etcd and others preallocate segment files (with
  `fallocate`) so appends do not change the file size, which makes
  `fdatasync` cheaper because no size metadata changes. Recovery then needs a
  way to find the logical end; this crate's "zeros after the last valid
  record are a torn tail" rule is the same idea.
- **`O_DIRECT` / `O_DSYNC`.** Bypassing the page cache or making every write
  synchronous changes the performance profile; both need aligned buffers and
  careful benchmarking.
- **Block framing for resynchronization.** LevelDB's 32 KiB blocks let a
  reader skip a damaged region and continue. This crate stops at the first
  damage instead, which is simpler and is the right default when the goal is
  to never return data after a gap.
- **Inter-process locking.** Two processes appending to the same directory
  would corrupt it. A lock file would prevent that.
- **Async I/O.** The API is synchronous. An async wrapper would move the
  blocking `fsync` to a dedicated thread.

## 12. Rust concepts used here

If you are new to Rust, these are the places in the code that show each
concept in a realistic setting.

**Ownership and borrowing.**

- `Wal::append(&mut self, payload: &[u8])` borrows the payload instead of
  taking ownership of a `Vec<u8>`, so callers can pass a string literal
  (`b"..."`), part of a buffer, or a `Vec` without copying it first.
- `record::encode_into(buf: &mut Vec<u8>, ...)` takes a mutable borrow of a
  buffer that the `Wal` owns, so one allocation is reused for every append.
- `Wal::iter(&self) -> Iter<'_>` returns an iterator that *borrows* the WAL
  (`src/iter.rs`, field `segments: &'a [Segment]`). While the iterator is
  alive the compiler will not let you call `append` or `truncate_before`,
  both of which need `&mut self`. That is a whole class of bugs ("deleted the
  file I was reading", "the segment list changed under me") ruled out at
  compile time with no runtime lock.
- `Option::as_mut()` in `Iter::read_next` borrows the reader inside an
  `Option` in place rather than moving it out.

**`Result` and the `?` operator.**

- Every fallible function returns `crate::Result<T>`, an alias for
  `std::result::Result<T, Error>` (`src/error.rs`).
- `?` returns early with the error if there is one. Because `Error` has
  `Io(#[from] io::Error)`, `?` on any `std::io` call converts the
  `io::Error` into `Error::Io` automatically. See almost any line of
  `Wal::open_with`.
- `iter().collect::<Result<Vec<_>>>()` (used in tests and the doc example)
  turns an iterator of `Result`s into a `Result` of a `Vec`, stopping at the
  first error.

**Traits and iterators.**

- `impl Iterator for Iter<'_>` (`src/iter.rs`) is all it takes to make the
  WAL work with `for` loops, `map`, `filter`, `take`, `count`, and `collect`.
- `#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]` on
  `Lsn` (`src/lib.rs`) generates common trait implementations. `Ord` is what
  lets the code write `lsn < self.first_lsn()` and `sort_by_key`.
- `impl fmt::Display for Lsn` and for `CorruptionKind` controls how they print
  with `{}`.
- `impl Default for WalOptions` plus builder methods that take and return
  `self` give the `WalOptions::default().sync_policy(...)` style.
- Generic functions with trait bounds, for example
  `fn rest_is_zero<R: Read + Seek>(reader: &mut R, ...)` in `src/segment.rs`,
  work with any reader, including the in-memory ones you could use in tests.
- `impl AsRef<Path>` in `Wal::open` accepts `&str`, `String`, `&Path`, or
  `PathBuf`.

**Enums and `match`.**

- `SyncPolicy`, `RecoveryMode`, and `CorruptionKind` are enums whose variants
  can carry data (`EveryN(u32)`, `UnexpectedLsn { expected, found }`).
- The `match self.options.sync_policy { ... }` at the end of `Wal::append`
  uses a guard (`SyncPolicy::EveryN(n) if self.unsynced >= n`). `match` must
  be exhaustive, so adding a new policy will not compile until every `match`
  handles it.
- `matches!(...)` in `CorruptionKind::is_truncation` and throughout the tests
  is a compact way to check which variant you have.

**Error types.**

- `src/error.rs` defines one public `Error` enum with `thiserror`, which
  generates `Display` and `std::error::Error`. Callers can `match` on the
  variant (for example, treat `Error::Corruption` differently from
  `Error::Io`).
- `#[non_exhaustive]` on the enums means adding a variant later is not a
  breaking change for users.
- The newtype `Lsn(u64)` is a typed wrapper around a number, so an LSN cannot
  be accidentally passed where a byte offset is expected.

**Other details.**

- Fixed-size arrays such as `&[u8; HEADER_LEN]` in `Header::decode` put the
  length check in the type system.
- `#[cfg(unix)]` in `sync_dir` compiles different code per platform.
- `#[cfg(test)] mod tests` keeps unit tests next to the code; `tests/` holds
  integration tests that use only the public API.

## 13. Interview questions and model answers

**Q: What is a write-ahead log and why do databases use one?**
An append-only file where every change is recorded and made durable before
it is applied to the main data structures. It makes commits atomic and
durable (a change exists if and only if its log record is complete and
valid), turns the commit path into cheap sequential I/O, and provides an
ordered change stream for recovery and replication.

**Q: What does it mean for a write to be durable? Is `write()` enough?**
Durable means it survives power loss. `write()` only puts data in the OS
page cache, which survives a process crash but not a kernel crash or power
loss. Durability requires `fsync`/`fdatasync` to return successfully (and a
device that honours cache flushes).

**Q: `fsync` vs. `fdatasync`?**
`fsync` flushes data and all metadata. `fdatasync` flushes data and only the
metadata needed to read it back (such as file size), skipping things like
mtime, so it often avoids an extra inode write. WALs usually use
`fdatasync`; preallocated files make it cheaper still because the size does
not change.

**Q: You fsynced the file. Why might it be gone after a crash?**
If it was newly created, its directory entry lives in the parent directory,
which also needs an fsync. Without it the data can be on disk with no name.

**Q: How do you detect a torn or partial write?**
The length prefix tells you how many bytes the record should have; if the
file ends early, the write was cut off. The checksum, covering header and
payload, tells you whether the bytes that are present are the bytes that were
written. You need both.

**Q: Why CRC32 rather than SHA-256?**
The threat is accidental corruption, not an adversary. CRC32 is designed to
detect exactly the burst and bit errors storage produces, is 4 bytes, and is
hardware-accelerated. A cryptographic hash is slower and larger and adds
nothing for this threat model.

**Q: What should recovery do when it finds a bad record?**
It depends on where it is. If nothing valid follows it (end of file, or only
zeros), it is a torn tail from a crash: truncate to the last valid record and
continue. If valid data follows it, it cannot be a crash artifact; truncating
would destroy acknowledged writes, so fail loudly and let the operator
restore from a replica or backup. Optionally offer a "point-in-time" mode that
truncates at the first error when losing unsynced data is acceptable.

**Q: Can you tell a torn final record from a bit flip in the final record?**
No, not from a single copy. Both look like a final record with a bad
checksum. This WAL truncates it. Systems that must not lose that record rely
on replication.

**Q: Tradeoffs between syncing every write and batching?**
Per-write fsync gives the strongest guarantee but caps throughput at about
one commit per fsync latency. Batching (group commit) amortizes one fsync over
many records: much higher throughput, and either a window of possible loss or
higher latency per commit if acknowledgements wait for the batch's fsync.
Systems like Kafka skip fsync entirely and get durability from replication.

**Q: What is group commit?**
Many concurrent transactions share one fsync. Writers append their records,
one thread issues the fsync, and all writers whose records it covered are
acknowledged together. It keeps per-commit durability while raising
throughput under concurrency.

**Q: What happens if `fsync` returns an error? Can you retry?**
Not safely on Linux: the kernel may have dropped the dirty pages and marked
them clean, so a retry can report success without the data being on disk
(PostgreSQL's "fsyncgate", 2018). Treat it as fatal, stop using the handle,
and recover from what is actually on disk. This WAL poisons itself.

**Q: Why include the LSN in each record?**
It makes records self-describing (useful for replication and for pointers
from data pages and checkpoints), and it adds an integrity check: a
checksum-valid record with an unexpected LSN reveals misplaced or duplicated
files that a CRC alone would accept.

**Q: How do you keep the log from growing forever?**
Checkpoint the state (flush pages or write a snapshot) and record the LSN it
covers, then delete log data before it. Segmenting the log into files makes
that a cheap file deletion. Delete oldest first so a crash never leaves a gap.

**Q: Why can recovery be strict about sealed segments but lenient about the
active one?**
Because the WAL fsyncs a segment before creating the next. Once a later
segment exists, every byte of the earlier one was durable, so damage there is
real corruption. Only the active segment can contain an interrupted append.

**Q: How does PostgreSQL guarantee the WAL is written before the data page?**
Each data page records the LSN of the last WAL record that changed it, and
the buffer manager flushes the WAL up to that LSN before it is allowed to
write the page.

**Q: How does Kafka's log differ from a database WAL?**
It is the primary data, not a side structure. It uses the same segmented,
offset-named, append-only design, but by default relies on replication rather
than per-write fsync for durability, and deletes whole segments by retention
time or size.

**Q: What would you change to make this production-ready?**
Concurrent group commit, an inter-process lock on the directory, segment
preallocation, a time-based sync policy, metrics (fsync latency, bytes per
second), and fault-injection tests that simulate the
page cache reordering writes (tools like ALICE or dm-flakey/dm-log-writes on
Linux).

**Q: How did you test crash recovery?**
By constructing crash images directly: cutting the file at every byte,
appending partial and garbage records, zero-filling the tail, flipping bits in
every field, deleting segments. Each case asserts recovery yields exactly the
complete prefix or a corruption error. A seeded randomized test generates 300
more cases with the invariant "never return wrong data". Unit tests check that
every single-bit flip in a record is detected.
