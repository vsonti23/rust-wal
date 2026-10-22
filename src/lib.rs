//! # rust-wal
//!
//! A segmented, checksummed, append-only write-ahead log with crash recovery
//! and configurable durability.
//!
//! * **Record format.** Each record is a 16-byte header (payload length,
//!   CRC32, LSN) followed by the payload. See the README for the layout.
//! * **Durability.** [`SyncPolicy`] selects fsync after every append, after
//!   every N appends (group commit), or never (OS-managed write-back).
//! * **Recovery.** [`Wal::open`] validates the whole log. A torn or truncated
//!   tail is cut back to the last valid record; corruption in the middle of
//!   the log is returned as [`Error::Corruption`] rather than silently
//!   dropping acknowledged data.
//! * **Segments.** The log rolls over to a new file at a configurable size,
//!   and [`Wal::truncate_before`] deletes segments after a checkpoint.
//!
//! ```
//! use rust_wal::{SyncPolicy, Wal, WalOptions};
//!
//! # fn main() -> rust_wal::Result<()> {
//! # let dir = tempfile::tempdir()?;
//! let opts = WalOptions::default().sync_policy(SyncPolicy::EveryN(64));
//! let mut wal = Wal::open_with(dir.path(), opts.clone())?;
//! wal.append(b"put k1 v1")?;
//! wal.append(b"put k2 v2")?;
//! wal.sync()?; // make both durable now
//! drop(wal);
//!
//! // After a restart (or a crash), reopening replays what is on disk.
//! let wal = Wal::open_with(dir.path(), opts)?;
//! for record in wal.iter()? {
//!     let record = record?;
//!     println!("{}: {:?}", record.lsn, record.data);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! For a plain-language walkthrough of every design decision, see
//! `docs/DESIGN.md` in the repository.

#![warn(missing_docs)]

mod error;
mod iter;
mod options;
mod record;
mod segment;
mod wal;

use std::fmt;

pub use error::{CorruptionKind, Error, Result};
pub use iter::{Iter, Record};
pub use options::{RecoveryMode, SyncPolicy, WalOptions};
pub use wal::{RecoveryInfo, TailTruncation, Wal};

/// Log sequence number: the position of a record in the log.
///
/// LSNs start at 1 and increase by exactly 1 per record, with no gaps. A
/// newtype instead of a bare `u64` means the compiler will not let you mix an
/// LSN up with a byte offset or a length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The LSN immediately after this one.
    pub fn next(self) -> Lsn {
        Lsn(self.0 + 1)
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for Lsn {
    fn from(value: u64) -> Self {
        Lsn(value)
    }
}
