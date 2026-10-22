//! Configuration: durability policy, recovery mode, and size limits.

use crate::error::{Error, Result};
use crate::record::HEADER_LEN;
use crate::segment::SEGMENT_HEADER_LEN;

/// When the WAL forces written data from the OS page cache to stable storage.
///
/// This is the central durability/throughput tradeoff of any log. See
/// `docs/DESIGN.md` for a longer discussion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// `fdatasync` after every append. An `Ok(lsn)` from
    /// [`Wal::append`](crate::Wal::append) means the record survives power
    /// loss. Slowest: every append waits for the device.
    Always,
    /// `fdatasync` after every `n` appends (group commit). A crash can lose
    /// at most the last `n - 1` acknowledged appends. `n` must be at least 1.
    EveryN(u32),
    /// Never fsync automatically; the OS writes dirty pages back on its own
    /// schedule (typically within ~30 seconds on Linux). Survives a process
    /// crash but not a kernel panic or power loss. Call
    /// [`Wal::sync`](crate::Wal::sync) yourself at commit points.
    Never,
}

/// What to do when recovery finds an invalid record in the final segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryMode {
    /// Truncate the final segment only when the damage looks like an
    /// interrupted write: the invalid record runs to the end of the file, or
    /// everything after it is zero bytes. Any other invalid record (one that
    /// is followed by more non-zero data) is reported as
    /// [`Error::Corruption`]. This is the default.
    TolerateTornTail,
    /// Truncate the final segment at the first invalid record, discarding
    /// everything after it. Appropriate when unsynced writes may have reached
    /// disk out of order (e.g. with [`SyncPolicy::Never`]) and losing the
    /// unsynced suffix is acceptable. Sealed segments are still verified
    /// strictly.
    PointInTime,
}

/// Options for [`Wal::open_with`](crate::Wal::open_with).
///
/// Construct with `WalOptions::default()` and adjust with the builder
/// methods:
///
/// ```
/// use rust_wal::{RecoveryMode, SyncPolicy, WalOptions};
///
/// let opts = WalOptions::default()
///     .sync_policy(SyncPolicy::EveryN(32))
///     .max_segment_bytes(16 * 1024 * 1024)
///     .recovery_mode(RecoveryMode::TolerateTornTail);
/// assert_eq!(opts.sync_policy, SyncPolicy::EveryN(32));
/// ```
#[derive(Debug, Clone)]
pub struct WalOptions {
    /// Durability policy. Default: [`SyncPolicy::Always`].
    pub sync_policy: SyncPolicy,
    /// Roll over to a new segment file once the active one would exceed this
    /// size. A single record larger than this gets a segment of its own.
    /// Default: 64 MiB.
    pub max_segment_bytes: u64,
    /// Largest payload accepted by `append`, and the largest length recovery
    /// will believe when reading a header. Bounding this stops a corrupted
    /// length field from triggering a multi-gigabyte allocation.
    /// Default: 16 MiB.
    pub max_record_bytes: u32,
    /// Recovery behaviour for the final segment. Default:
    /// [`RecoveryMode::TolerateTornTail`].
    pub recovery_mode: RecoveryMode,
}

impl Default for WalOptions {
    fn default() -> Self {
        WalOptions {
            sync_policy: SyncPolicy::Always,
            max_segment_bytes: 64 * 1024 * 1024,
            max_record_bytes: 16 * 1024 * 1024,
            recovery_mode: RecoveryMode::TolerateTornTail,
        }
    }
}

impl WalOptions {
    /// Set the [`SyncPolicy`].
    pub fn sync_policy(mut self, policy: SyncPolicy) -> Self {
        self.sync_policy = policy;
        self
    }

    /// Set the segment rollover size in bytes.
    pub fn max_segment_bytes(mut self, bytes: u64) -> Self {
        self.max_segment_bytes = bytes;
        self
    }

    /// Set the maximum payload size in bytes.
    pub fn max_record_bytes(mut self, bytes: u32) -> Self {
        self.max_record_bytes = bytes;
        self
    }

    /// Set the [`RecoveryMode`].
    pub fn recovery_mode(mut self, mode: RecoveryMode) -> Self {
        self.recovery_mode = mode;
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.sync_policy == SyncPolicy::EveryN(0) {
            return Err(Error::InvalidConfig("SyncPolicy::EveryN requires n >= 1"));
        }
        if self.max_segment_bytes < (SEGMENT_HEADER_LEN + HEADER_LEN) as u64 {
            return Err(Error::InvalidConfig(
                "max_segment_bytes must fit a segment header and one record header",
            ));
        }
        Ok(())
    }
}
