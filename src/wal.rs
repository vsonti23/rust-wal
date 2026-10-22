//! The [`Wal`] handle: open/recover, append, sync, replay, and segment cleanup.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Lsn;
use crate::error::{CorruptionKind, Error, Result};
use crate::iter::Iter;
use crate::options::{RecoveryMode, SyncPolicy, WalOptions};
use crate::record::{self, HEADER_LEN};
use crate::segment::{self, SEGMENT_HEADER_LEN, Segment};

/// What [`Wal::open`] found and repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryInfo {
    /// Number of segment files in the log after recovery.
    pub segments: usize,
    /// Number of valid records found.
    pub records: u64,
    /// Set if an invalid tail was cut off the final segment.
    pub truncated: Option<TailTruncation>,
}

/// Details of a tail truncation performed during recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailTruncation {
    /// Segment that was truncated.
    pub path: PathBuf,
    /// New length of the file (offset of the first invalid record).
    pub offset: u64,
    /// Number of bytes discarded.
    pub bytes_discarded: u64,
    /// Why the first discarded record was rejected.
    pub reason: CorruptionKind,
}

/// An append-only, segmented, checksummed write-ahead log.
///
/// A `Wal` owns its directory: only one `Wal` (in one process) may have a
/// directory open at a time. Appends take `&mut self`, so Rust's borrow
/// checker already prevents two threads from appending concurrently without
/// a lock; wrap it in a `Mutex` to share it.
///
/// ```
/// use rust_wal::Wal;
///
/// # fn main() -> rust_wal::Result<()> {
/// # let dir = tempfile::tempdir()?;
/// let mut wal = Wal::open(dir.path())?;
/// let lsn = wal.append(b"set x = 1")?;
/// wal.append(b"set y = 2")?;
///
/// let records: Vec<_> = wal.iter()?.collect::<rust_wal::Result<_>>()?;
/// assert_eq!(records[0].lsn, lsn);
/// assert_eq!(records[1].data, b"set y = 2");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Wal {
    dir: PathBuf,
    options: WalOptions,
    /// All segments, oldest first. The last one is the active segment.
    segments: Vec<Segment>,
    /// Handle to the active segment, opened in append mode.
    active: File,
    /// Current size of the active segment in bytes.
    active_len: u64,
    /// Number of records in the active segment.
    active_records: u64,
    /// LSN the next append will receive.
    next_lsn: Lsn,
    /// Appends written since the last fsync.
    unsynced: u32,
    /// Set after a failed write or fsync; see [`Error::Poisoned`].
    poisoned: bool,
    /// Reusable encode buffer.
    buf: Vec<u8>,
    recovery: RecoveryInfo,
}

impl Wal {
    /// Open (or create) a log in `dir` with default options.
    pub fn open(dir: impl AsRef<Path>) -> Result<Wal> {
        Wal::open_with(dir, WalOptions::default())
    }

    /// Open (or create) a log in `dir`, running crash recovery.
    ///
    /// Recovery validates every record in every segment:
    ///
    /// * Sealed (non-final) segments must be fully valid and contiguous.
    ///   They were fsynced before the next segment was created, so any damage
    ///   there is real corruption and is returned as [`Error::Corruption`].
    /// * In the final segment, an invalid tail that looks like an interrupted
    ///   write is truncated back to the last valid record (see
    ///   [`RecoveryMode`]). Invalid data followed by more data is reported as
    ///   corruption instead of being silently discarded.
    pub fn open_with(dir: impl AsRef<Path>, options: WalOptions) -> Result<Wal> {
        options.validate()?;
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        let mut segments = segment::list_segments(&dir)?;
        if segments.is_empty() {
            // Fresh log. LSNs start at 1 so that 0 can mean "nothing yet".
            let (seg, _file) = segment::create_segment(&dir, Lsn(1))?;
            segments.push(seg);
        }

        let mut recovery = RecoveryInfo {
            segments: segments.len(),
            ..RecoveryInfo::default()
        };
        let mut expected_base = segments[0].base_lsn;
        let mut active_records = 0;
        let last_index = segments.len() - 1;

        for (i, seg) in segments.iter().enumerate() {
            let is_last = i == last_index;

            if seg.base_lsn != expected_base {
                return Err(Error::Corruption {
                    path: seg.path.clone(),
                    offset: 0,
                    kind: CorruptionKind::SegmentGap {
                        expected: expected_base,
                        found: seg.base_lsn,
                    },
                });
            }

            let scan = segment::scan_segment(seg, options.max_record_bytes)?;

            if let Some(invalid) = scan.invalid {
                let truncate = is_last
                    && (invalid.looks_torn || options.recovery_mode == RecoveryMode::PointInTime);
                if !truncate {
                    return Err(Error::Corruption {
                        path: seg.path.clone(),
                        offset: invalid.offset,
                        kind: invalid.kind,
                    });
                }
                segment::truncate_segment(seg, invalid.offset)?;
                let new_len = invalid.offset.max(SEGMENT_HEADER_LEN as u64);
                recovery.truncated = Some(TailTruncation {
                    path: seg.path.clone(),
                    offset: new_len,
                    bytes_discarded: scan.file_len.saturating_sub(invalid.offset),
                    reason: invalid.kind,
                });
            }

            if !is_last && scan.records == 0 {
                return Err(Error::Corruption {
                    path: seg.path.clone(),
                    offset: SEGMENT_HEADER_LEN as u64,
                    kind: CorruptionKind::EmptySealedSegment,
                });
            }

            recovery.records += scan.records;
            expected_base = scan.next_lsn;
            if is_last {
                active_records = scan.records;
            }
        }

        let active_path = &segments[last_index].path;
        let active = OpenOptions::new()
            .read(true)
            .append(true)
            .open(active_path)?;
        let active_len = active.metadata()?.len();

        Ok(Wal {
            dir,
            options,
            segments,
            active,
            active_len,
            active_records,
            next_lsn: expected_base,
            unsynced: 0,
            poisoned: false,
            buf: Vec::new(),
            recovery,
        })
    }

    /// Append one record and return its LSN.
    ///
    /// Whether the record is durable when this returns depends on the
    /// [`SyncPolicy`]. With `SyncPolicy::Always`, `Ok` means it is on stable
    /// storage.
    ///
    /// If the write or the fsync fails, the WAL becomes poisoned and every
    /// later call returns [`Error::Poisoned`]: the file may now hold a partial
    /// record, and after a failed fsync the kernel may have discarded the
    /// dirty pages while marking them clean, so a retry could falsely report
    /// success. Reopening runs recovery against what is really on disk.
    pub fn append(&mut self, payload: &[u8]) -> Result<Lsn> {
        self.check_poisoned()?;
        if payload.len() > self.options.max_record_bytes as usize {
            return Err(Error::RecordTooLarge {
                size: payload.len(),
                max: self.options.max_record_bytes,
            });
        }

        let record_len = (HEADER_LEN + payload.len()) as u64;
        if self.active_records > 0 && self.active_len + record_len > self.options.max_segment_bytes
        {
            self.roll_segment()?;
        }

        let lsn = self.next_lsn;
        record::encode_into(&mut self.buf, lsn, payload);

        // One write call per record. This lands in the OS page cache, not on
        // the disk; durability comes from the fsync below (or later).
        if let Err(e) = self.active.write_all(&self.buf) {
            self.poisoned = true;
            return Err(e.into());
        }
        self.active_len += record_len;
        self.active_records += 1;
        self.next_lsn = lsn.next();
        self.unsynced += 1;

        // `match` on the enum must handle every variant; adding a new policy
        // later will not compile until this is updated.
        match self.options.sync_policy {
            SyncPolicy::Always => self.sync()?,
            SyncPolicy::EveryN(n) if self.unsynced >= n => self.sync()?,
            SyncPolicy::EveryN(_) | SyncPolicy::Never => {}
        }
        Ok(lsn)
    }

    /// Force all appended records to stable storage (`fdatasync`).
    ///
    /// `fdatasync` (Rust's `File::sync_data`) flushes the file's data and the
    /// metadata needed to read it back (such as its size), but skips
    /// metadata like modification time, which makes it cheaper than a full
    /// `fsync` (`sync_all`).
    pub fn sync(&mut self) -> Result<()> {
        self.check_poisoned()?;
        if self.unsynced == 0 {
            return Ok(());
        }
        if let Err(e) = self.active.sync_data() {
            self.poisoned = true;
            return Err(e.into());
        }
        self.unsynced = 0;
        Ok(())
    }

    /// Iterate over every record in the log, oldest first.
    ///
    /// The iterator borrows the `Wal` immutably, so the compiler rejects any
    /// attempt to `append` or `truncate_before` while it is alive. Records
    /// appended before the call are included.
    pub fn iter(&self) -> Result<Iter<'_>> {
        self.iter_from(self.first_lsn())
    }

    /// Iterate starting at `lsn` (inclusive). Typical use: replay everything
    /// after the last checkpoint.
    ///
    /// Returns [`Error::LsnOutOfRange`] if `lsn` was removed by
    /// [`truncate_before`](Wal::truncate_before) or is beyond the end of the
    /// log. Passing [`next_lsn`](Wal::next_lsn) yields an empty iterator.
    pub fn iter_from(&self, lsn: Lsn) -> Result<Iter<'_>> {
        if lsn < self.first_lsn() || lsn > self.next_lsn {
            return Err(Error::LsnOutOfRange(lsn));
        }
        // Start at the last segment whose base LSN is <= `lsn`.
        let start_segment = self
            .segments
            .partition_point(|s| s.base_lsn <= lsn)
            .saturating_sub(1);
        Ok(Iter::new(
            &self.segments[start_segment..],
            lsn,
            self.next_lsn,
            self.options.max_record_bytes,
        ))
    }

    /// Delete whole segments whose records all have an LSN below `lsn`.
    ///
    /// Call this after a checkpoint: once the state up to `lsn` is safely
    /// persisted elsewhere, the log before it is no longer needed. The active
    /// segment is never removed, and segments are only removed whole, so
    /// records slightly older than `lsn` may remain. Returns the number of
    /// segments deleted.
    ///
    /// Deletion is oldest-first and followed by a directory fsync, so a crash
    /// part-way through leaves a valid log that simply starts a little
    /// earlier.
    pub fn truncate_before(&mut self, lsn: Lsn) -> Result<usize> {
        self.check_poisoned()?;
        let mut removed = 0;
        while self.segments.len() > 1 && self.segments[1].base_lsn <= lsn {
            fs::remove_file(&self.segments[0].path)?;
            self.segments.remove(0);
            removed += 1;
        }
        if removed > 0 {
            segment::sync_dir(&self.dir)?;
        }
        Ok(removed)
    }

    /// LSN of the oldest record still in the log (or of the next append, if
    /// the log is empty).
    pub fn first_lsn(&self) -> Lsn {
        self.segments[0].base_lsn
    }

    /// LSN of the newest record, or `None` if the log holds no records.
    pub fn last_lsn(&self) -> Option<Lsn> {
        (self.next_lsn > self.first_lsn()).then(|| Lsn(self.next_lsn.0 - 1))
    }

    /// LSN that the next [`append`](Wal::append) will return.
    pub fn next_lsn(&self) -> Lsn {
        self.next_lsn
    }

    /// Number of records written but not yet fsynced.
    pub fn unsynced_records(&self) -> u32 {
        self.unsynced
    }

    /// Number of segment files currently in the log.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Directory this log lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The options this log was opened with.
    pub fn options(&self) -> &WalOptions {
        &self.options
    }

    /// What recovery found when this log was opened.
    pub fn recovery_info(&self) -> &RecoveryInfo {
        &self.recovery
    }

    /// Seal the active segment and start a new one.
    ///
    /// The old segment is always fsynced first, whatever the sync policy.
    /// That invariant is what lets recovery treat any damage in a sealed
    /// segment as real corruption rather than a torn write.
    fn roll_segment(&mut self) -> Result<()> {
        if let Err(e) = self.active.sync_data() {
            self.poisoned = true;
            return Err(e.into());
        }
        self.unsynced = 0;
        let (seg, file) = segment::create_segment(&self.dir, self.next_lsn)?;
        self.segments.push(seg);
        self.active = file;
        self.active_len = SEGMENT_HEADER_LEN as u64;
        self.active_records = 0;
        Ok(())
    }

    fn check_poisoned(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }
}
