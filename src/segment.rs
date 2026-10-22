//! Segment files: naming, creation, and the recovery scan.
//!
//! The log is a directory of segment files. Each file is named after the LSN
//! of its first record, zero-padded so lexical order equals numeric order:
//!
//! ```text
//! wal/
//!   00000000000000000001.wal   records 1..=812
//!   00000000000000000813.wal   records 813..   (active, append-only)
//! ```
//!
//! Each file starts with an 8-byte segment header (`b"RWAL"` + format version
//! as a little-endian u32) followed by records.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::Lsn;
use crate::error::{CorruptionKind, Error, Result};
use crate::record::{HEADER_LEN, Header};

/// Current on-disk format version.
pub(crate) const FORMAT_VERSION: u32 = 1;
/// Size of the per-file header.
pub(crate) const SEGMENT_HEADER_LEN: usize = 8;
const SEGMENT_EXTENSION: &str = "wal";

pub(crate) fn segment_header() -> [u8; SEGMENT_HEADER_LEN] {
    let v = FORMAT_VERSION.to_le_bytes();
    [b'R', b'W', b'A', b'L', v[0], v[1], v[2], v[3]]
}

/// A segment file and the LSN of its first record.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub base_lsn: Lsn,
    pub path: PathBuf,
}

pub(crate) fn segment_path(dir: &Path, base_lsn: Lsn) -> PathBuf {
    dir.join(format!("{:020}.{SEGMENT_EXTENSION}", base_lsn.0))
}

/// Parse `00000000000000000042.wal` into `Lsn(42)`. Anything else is ignored,
/// so unrelated files in the directory do not confuse recovery.
fn parse_segment_name(name: &str) -> Option<Lsn> {
    let stem = name.strip_suffix(".wal")?;
    if stem.len() != 20 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok().map(Lsn)
}

/// List segment files in `dir`, sorted by base LSN.
pub(crate) fn list_segments(dir: &Path) -> io::Result<Vec<Segment>> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        // `file_name()` is an `OsString`; non-UTF-8 names cannot be ours.
        if let Some(base_lsn) = name.to_str().and_then(parse_segment_name) {
            segments.push(Segment {
                base_lsn,
                path: entry.path(),
            });
        }
    }
    segments.sort_by_key(|s| s.base_lsn);
    Ok(segments)
}

/// Create a new, empty segment and make its existence durable.
///
/// Two fsyncs are needed: one for the file contents (the header) and one
/// for the *directory*, because a new file's directory entry lives in the
/// directory, not in the file. Without the second fsync a power cut can
/// leave the data blocks on disk with no name pointing at them.
pub(crate) fn create_segment(dir: &Path, base_lsn: Lsn) -> Result<(Segment, File)> {
    let path = segment_path(dir, base_lsn);
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&segment_header())?;
    file.sync_all()?;
    sync_dir(dir)?;
    Ok((Segment { base_lsn, path }, file))
}

/// fsync a directory so that file creations, deletions, and renames in it
/// are durable. A no-op on platforms that cannot open directories as files.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The first invalid region found while scanning a segment.
#[derive(Debug)]
pub(crate) struct Invalid {
    pub offset: u64,
    pub kind: CorruptionKind,
    /// True when the damage is consistent with an interrupted write: nothing
    /// that could be a valid record follows it.
    pub looks_torn: bool,
}

/// Result of validating one segment from start to end.
#[derive(Debug)]
pub(crate) struct Scan {
    /// Number of valid records.
    pub records: u64,
    /// LSN that the next record (or next segment) must have.
    pub next_lsn: Lsn,
    /// Length of the file on disk.
    pub file_len: u64,
    /// `None` if the whole file is valid.
    pub invalid: Option<Invalid>,
}

/// Read and validate every record in `segment`.
///
/// This function only *classifies* what it finds; the caller (`Wal::open`)
/// decides whether an invalid region is truncated or reported, because that
/// depends on whether this is the last segment and on the recovery mode.
///
/// A valid record with the wrong LSN is always returned as an error: a
/// correct checksum means the bytes are exactly what some writer wrote, so a
/// sequencing problem is a bug or a misplaced file, never a torn write.
pub(crate) fn scan_segment(segment: &Segment, max_record_bytes: u32) -> Result<Scan> {
    let file = File::open(&segment.path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    let mut scan = Scan {
        records: 0,
        next_lsn: segment.base_lsn,
        file_len,
        invalid: None,
    };

    // Segment header.
    if file_len < SEGMENT_HEADER_LEN as u64 {
        scan.invalid = Some(Invalid {
            offset: 0,
            kind: CorruptionKind::TruncatedSegmentHeader,
            looks_torn: true,
        });
        return Ok(scan);
    }
    let mut seg_header = [0u8; SEGMENT_HEADER_LEN];
    reader.read_exact(&mut seg_header)?;
    if seg_header != segment_header() {
        let looks_torn = rest_is_zero(&mut reader, 0)?;
        scan.invalid = Some(Invalid {
            offset: 0,
            kind: CorruptionKind::BadSegmentHeader,
            looks_torn,
        });
        return Ok(scan);
    }

    let mut offset = SEGMENT_HEADER_LEN as u64;
    let mut header_buf = [0u8; HEADER_LEN];
    let mut payload = Vec::new();

    loop {
        let remaining = file_len - offset;
        if remaining == 0 {
            return Ok(scan); // clean end of segment
        }

        // Crash case 1: the process died after writing only part of a header.
        if remaining < HEADER_LEN as u64 {
            scan.invalid = Some(Invalid {
                offset,
                kind: CorruptionKind::TruncatedHeader,
                looks_torn: true,
            });
            return Ok(scan);
        }

        reader.read_exact(&mut header_buf)?;
        let header = Header::decode(&header_buf);
        let record_end = offset + header.record_len();

        // A length we would never have written. Do not trust it enough to
        // allocate. It is a torn tail only if nothing valid can follow it.
        if header.len > max_record_bytes {
            let looks_torn =
                record_end >= file_len || rest_is_zero(&mut reader, offset + HEADER_LEN as u64)?;
            scan.invalid = Some(Invalid {
                offset,
                kind: CorruptionKind::LengthTooLarge(header.len),
                looks_torn,
            });
            return Ok(scan);
        }

        // Crash case 2: header made it to disk, payload only partially.
        if record_end > file_len {
            scan.invalid = Some(Invalid {
                offset,
                kind: CorruptionKind::TruncatedPayload,
                looks_torn: true,
            });
            return Ok(scan);
        }

        payload.resize(header.len as usize, 0);
        reader.read_exact(&mut payload)?;

        // Crash case 3: all bytes are present but some sectors hold old or
        // garbage data (a torn write). If this record is the last thing in
        // the file, or only zeros follow, it is indistinguishable from an
        // interrupted append. If real data follows, something in the middle
        // of the log rotted, and silently dropping the records after it would
        // lose acknowledged writes.
        if !header.verify(&payload) {
            let looks_torn = record_end == file_len || rest_is_zero(&mut reader, record_end)?;
            scan.invalid = Some(Invalid {
                offset,
                kind: CorruptionKind::ChecksumMismatch,
                looks_torn,
            });
            return Ok(scan);
        }

        if header.lsn != scan.next_lsn {
            return Err(Error::Corruption {
                path: segment.path.clone(),
                offset,
                kind: CorruptionKind::UnexpectedLsn {
                    expected: scan.next_lsn,
                    found: header.lsn,
                },
            });
        }

        scan.records += 1;
        scan.next_lsn = scan.next_lsn.next();
        offset = record_end;
    }
}

/// True if every byte from `offset` to end of file is zero (or there are none).
///
/// Some filesystems (ext4, XFS) can, after a crash, show a file whose size
/// was extended but whose new blocks were never written, which reads back as
/// zeros. Such a tail is an interrupted write, not corruption.
fn rest_is_zero<R: Read + Seek>(reader: &mut R, offset: u64) -> io::Result<bool> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(true);
        }
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
    }
}

/// Cut `segment` back to `offset` bytes and make the cut durable. If the
/// segment header itself was damaged, rewrite it so the file is a valid,
/// empty segment.
pub(crate) fn truncate_segment(segment: &Segment, offset: u64) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(&segment.path)?;
    if offset < SEGMENT_HEADER_LEN as u64 {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&segment_header())?;
    } else {
        file.set_len(offset)?;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_round_trip() {
        let path = segment_path(Path::new("/tmp"), Lsn(813));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "00000000000000000813.wal");
        assert_eq!(parse_segment_name(name), Some(Lsn(813)));
    }

    #[test]
    fn foreign_files_are_ignored() {
        assert_eq!(parse_segment_name("notes.txt"), None);
        assert_eq!(parse_segment_name("813.wal"), None);
        assert_eq!(parse_segment_name("0000000000000000081x.wal"), None);
    }
}
