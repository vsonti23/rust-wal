//! Sequential replay.

use std::fs::File;
use std::io::{self, BufReader, Read};

use crate::Lsn;
use crate::error::{CorruptionKind, Error, Result};
use crate::record::{HEADER_LEN, Header};
use crate::segment::{SEGMENT_HEADER_LEN, Segment, segment_header};

/// One record read back from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Sequence number assigned by [`Wal::append`](crate::Wal::append).
    pub lsn: Lsn,
    /// The payload exactly as it was appended.
    pub data: Vec<u8>,
}

/// Iterator over records, created by [`Wal::iter`](crate::Wal::iter) or
/// [`Wal::iter_from`](crate::Wal::iter_from).
///
/// Yields `Result<Record>`: reading can fail (I/O error, or corruption that
/// appeared after the log was opened). After the first error the iterator is
/// fused and returns `None`.
///
/// The `'a` lifetime ties the iterator to the `Wal` it came from: it holds a
/// borrowed slice of the WAL's segment list rather than a copy, and the
/// borrow checker guarantees the segment list (and the files) cannot change
/// while that borrow is alive.
pub struct Iter<'a> {
    segments: &'a [Segment],
    segment_index: usize,
    reader: Option<BufReader<File>>,
    /// Byte offset of the next record within the current segment.
    offset: u64,
    /// LSN of the next record on disk.
    expected: Lsn,
    /// First LSN to yield; earlier records are skipped.
    start: Lsn,
    /// Stop before this LSN (the WAL's `next_lsn` when the iterator was made).
    end: Lsn,
    max_record_bytes: u32,
    done: bool,
}

impl<'a> Iter<'a> {
    pub(crate) fn new(
        segments: &'a [Segment],
        start: Lsn,
        end: Lsn,
        max_record_bytes: u32,
    ) -> Self {
        Iter {
            segments,
            segment_index: 0,
            reader: None,
            offset: 0,
            expected: segments.first().map_or(start, |s| s.base_lsn),
            start,
            end,
            max_record_bytes,
            done: false,
        }
    }

    fn corruption(&self, kind: CorruptionKind) -> Error {
        Error::Corruption {
            path: self.segments[self.segment_index].path.clone(),
            offset: self.offset,
            kind,
        }
    }

    fn open_current(&mut self) -> Result<()> {
        let seg = &self.segments[self.segment_index];
        let mut reader = BufReader::new(File::open(&seg.path)?);
        let mut header = [0u8; SEGMENT_HEADER_LEN];
        self.offset = 0;
        if read_full(&mut reader, &mut header)? != SEGMENT_HEADER_LEN {
            return Err(self.corruption(CorruptionKind::TruncatedSegmentHeader));
        }
        if header != segment_header() {
            return Err(self.corruption(CorruptionKind::BadSegmentHeader));
        }
        if seg.base_lsn != self.expected {
            return Err(self.corruption(CorruptionKind::SegmentGap {
                expected: self.expected,
                found: seg.base_lsn,
            }));
        }
        self.offset = SEGMENT_HEADER_LEN as u64;
        self.reader = Some(reader);
        Ok(())
    }

    /// Read the next record at or after `start`, or `None` once `end` is reached.
    fn read_next(&mut self) -> Result<Option<Record>> {
        loop {
            if self.expected >= self.end {
                return Ok(None);
            }
            if self.reader.is_none() {
                self.open_current()?;
            }
            // `as_mut()` turns `&mut Option<T>` into `Option<&mut T>`, so we
            // borrow the reader in place instead of moving it out.
            let reader = self.reader.as_mut().expect("reader opened above");

            let mut header_buf = [0u8; HEADER_LEN];
            match read_full(reader, &mut header_buf)? {
                0 => {
                    // Clean end of this segment; move on to the next one.
                    self.segment_index += 1;
                    self.reader = None;
                    if self.segment_index >= self.segments.len() {
                        self.segment_index -= 1;
                        return Err(self.corruption(CorruptionKind::TruncatedHeader));
                    }
                    continue;
                }
                HEADER_LEN => {}
                _ => return Err(self.corruption(CorruptionKind::TruncatedHeader)),
            }

            let header = Header::decode(&header_buf);
            if header.len > self.max_record_bytes {
                return Err(self.corruption(CorruptionKind::LengthTooLarge(header.len)));
            }
            let mut data = vec![0u8; header.len as usize];
            if read_full(reader, &mut data)? != data.len() {
                return Err(self.corruption(CorruptionKind::TruncatedPayload));
            }
            if !header.verify(&data) {
                return Err(self.corruption(CorruptionKind::ChecksumMismatch));
            }
            if header.lsn != self.expected {
                return Err(self.corruption(CorruptionKind::UnexpectedLsn {
                    expected: self.expected,
                    found: header.lsn,
                }));
            }

            self.offset += header.record_len();
            self.expected = self.expected.next();
            if header.lsn >= self.start {
                return Ok(Some(Record {
                    lsn: header.lsn,
                    data,
                }));
            }
        }
    }
}

impl Iterator for Iter<'_> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // `Result<Option<T>>` -> `Option<Result<T>>` is exactly `transpose`,
        // but we also want to fuse on error, so spell it out with `match`.
        match self.read_next() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Like `read_exact`, but returns how many bytes were read instead of
/// failing on EOF, so the caller can tell "clean end" (0) from "torn" (some).
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}
