//! Error types.
//!
//! The crate exposes a single [`Error`] enum so callers can `match` on the
//! failure they care about (for example, corruption vs. plain I/O) while
//! still using `?` everywhere. `thiserror` generates the `Display` and
//! `std::error::Error` impls, and `#[from]` lets `?` convert an
//! `io::Error` into `Error::Io` automatically.

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::Lsn;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong when using a [`Wal`](crate::Wal).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An underlying I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The log contains data that cannot be explained by a crash during a
    /// write (for example, a checksum failure in the middle of the log).
    /// The WAL refuses to open rather than silently dropping records.
    #[error("corruption in {} at byte offset {offset}: {kind}", path.display())]
    Corruption {
        /// Segment file in which the problem was found.
        path: PathBuf,
        /// Byte offset of the first bad record (or header) in that file.
        offset: u64,
        /// What exactly was wrong.
        kind: CorruptionKind,
    },

    /// `append` was called with a payload larger than
    /// [`WalOptions::max_record_bytes`](crate::WalOptions).
    #[error("record of {size} bytes exceeds the configured maximum of {max} bytes")]
    RecordTooLarge {
        /// Size of the rejected payload.
        size: usize,
        /// Configured maximum.
        max: u32,
    },

    /// The options passed to [`Wal::open_with`](crate::Wal::open_with) are invalid.
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),

    /// The requested LSN is not in the log (it was removed by
    /// [`Wal::truncate_before`](crate::Wal::truncate_before) or has not been
    /// written yet).
    #[error("LSN {0} is not available in this log")]
    LsnOutOfRange(Lsn),

    /// A previous write or fsync failed. After a failed fsync the kernel may
    /// have already dropped the dirty pages, so retrying is unsafe; the only
    /// correct recovery is to reopen the log, which re-validates what is
    /// actually on disk.
    #[error("the WAL is poisoned by an earlier write or sync failure; reopen it to recover")]
    Poisoned,
}

/// The specific reason a region of the log was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptionKind {
    /// Fewer bytes than a segment header remain in the file.
    TruncatedSegmentHeader,
    /// The segment header does not carry the expected magic/version.
    BadSegmentHeader,
    /// Fewer bytes than a record header remain in the file.
    TruncatedHeader,
    /// The header announces more payload bytes than the file contains.
    TruncatedPayload,
    /// The header announces a payload larger than `max_record_bytes`.
    LengthTooLarge(u32),
    /// The stored CRC32 does not match the header and payload.
    ChecksumMismatch,
    /// A record with a valid checksum carries the wrong sequence number.
    UnexpectedLsn {
        /// The LSN the log should contain at this position.
        expected: Lsn,
        /// The LSN that was found.
        found: Lsn,
    },
    /// Segment files are not contiguous (a file is missing or misnamed).
    SegmentGap {
        /// The base LSN the next segment should have.
        expected: Lsn,
        /// The base LSN encoded in the file name.
        found: Lsn,
    },
    /// A sealed (non-final) segment contains no records.
    EmptySealedSegment,
}

impl CorruptionKind {
    /// Whether this kind of damage is what a crash in the middle of an
    /// append looks like (as opposed to bit rot or a logic bug).
    pub fn is_truncation(&self) -> bool {
        matches!(
            self,
            CorruptionKind::TruncatedSegmentHeader
                | CorruptionKind::TruncatedHeader
                | CorruptionKind::TruncatedPayload
        )
    }
}

impl fmt::Display for CorruptionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorruptionKind::TruncatedSegmentHeader => write!(f, "truncated segment header"),
            CorruptionKind::BadSegmentHeader => write!(f, "bad segment header (magic/version)"),
            CorruptionKind::TruncatedHeader => write!(f, "truncated record header"),
            CorruptionKind::TruncatedPayload => write!(f, "truncated record payload"),
            CorruptionKind::LengthTooLarge(len) => {
                write!(f, "record length {len} exceeds max_record_bytes")
            }
            CorruptionKind::ChecksumMismatch => write!(f, "CRC32 checksum mismatch"),
            CorruptionKind::UnexpectedLsn { expected, found } => {
                write!(f, "expected LSN {expected}, found {found}")
            }
            CorruptionKind::SegmentGap { expected, found } => {
                write!(
                    f,
                    "expected segment starting at LSN {expected}, found {found}"
                )
            }
            CorruptionKind::EmptySealedSegment => write!(f, "sealed segment contains no records"),
        }
    }
}
