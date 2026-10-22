//! Crash recovery: torn tails are truncated, mid-log corruption is an error.
//!
//! These tests build "crash images" by editing segment files directly, which
//! is what the disk could look like if power failed during an append.

mod common;

use common::*;
use rust_wal::{CorruptionKind, Error, Lsn, RecoveryMode, Wal};

const N: u64 = 5;

/// Reopen and assert the log recovered to exactly `expected` records.
fn assert_recovers_to(dir: &std::path::Path, expected: u64) -> Wal {
    let wal = Wal::open_with(dir, fast_opts()).unwrap();
    let records = read_all(&wal);
    assert_eq!(records.len() as u64, expected);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r.lsn, Lsn(i as u64 + 1));
        assert_eq!(r.data, payload(i as u64 + 1));
    }
    assert_eq!(wal.next_lsn(), Lsn(expected + 1));
    wal
}

#[test]
fn truncated_header_at_every_length() {
    // Simulate a crash after writing 1..15 bytes of the 6th record's header.
    let full = encode_record(N + 1, &payload(N + 1));
    for cut in 1..HEADER_LEN as usize {
        let dir = tempfile::tempdir().unwrap();
        drop(write_records(dir.path(), N, fast_opts()));
        let seg = last_segment(dir.path());
        let clean_len = file_len(&seg);
        append_bytes(&seg, &full[..cut]);

        let wal = assert_recovers_to(dir.path(), N);
        let t = wal
            .recovery_info()
            .truncated
            .clone()
            .expect("tail truncated");
        assert_eq!(t.reason, CorruptionKind::TruncatedHeader);
        assert_eq!(t.offset, clean_len);
        assert_eq!(t.bytes_discarded, cut as u64);
        assert_eq!(file_len(&seg), clean_len, "file physically truncated");
    }
}

#[test]
fn truncated_payload_at_every_length() {
    // Header fully written, payload cut short at every possible point.
    let full = encode_record(N + 1, &payload(N + 1));
    for cut in HEADER_LEN as usize..full.len() {
        let dir = tempfile::tempdir().unwrap();
        drop(write_records(dir.path(), N, fast_opts()));
        append_bytes(&last_segment(dir.path()), &full[..cut]);

        let wal = assert_recovers_to(dir.path(), N);
        let reason = wal.recovery_info().truncated.clone().unwrap().reason;
        assert_eq!(reason, CorruptionKind::TruncatedPayload);
    }
}

#[test]
fn truncation_at_every_byte_of_the_file() {
    // The strongest form of the crash test: cut the file at every byte and
    // check that recovery always yields exactly the complete records.
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    let seg = last_segment(dir.path());
    let original = std::fs::read(&seg).unwrap();
    let record_len = HEADER_LEN + payload(1).len() as u64;

    for len in 0..=original.len() as u64 {
        std::fs::write(&seg, &original[..len as usize]).unwrap();
        let complete = len.saturating_sub(SEGMENT_HEADER_LEN) / record_len;
        let mut wal = assert_recovers_to(dir.path(), complete);
        // The recovered log must accept new appends at the right LSN.
        assert_eq!(wal.append(b"next").unwrap(), Lsn(complete + 1));
    }
}

#[test]
fn flipped_bit_in_last_record_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    let seg = last_segment(dir.path());
    // Flip a payload bit in the final record.
    flip_bit(&seg, record_offset(N) + HEADER_LEN + 3, 2);

    let wal = assert_recovers_to(dir.path(), N - 1);
    let t = wal.recovery_info().truncated.clone().unwrap();
    assert_eq!(t.reason, CorruptionKind::ChecksumMismatch);
    assert_eq!(t.offset, record_offset(N));
}

#[test]
fn flipped_bit_mid_log_is_an_error() {
    // Each byte region of a middle record: length, CRC, LSN, payload.
    for byte in [0, 5, 9, HEADER_LEN + 1] {
        let dir = tempfile::tempdir().unwrap();
        drop(write_records(dir.path(), N, fast_opts()));
        let seg = last_segment(dir.path());
        let before = std::fs::read(&seg).unwrap();
        flip_bit(&seg, record_offset(3) + byte, 0);

        match Wal::open_with(dir.path(), fast_opts()) {
            Err(Error::Corruption { offset, path, .. }) => {
                assert_eq!(offset, record_offset(3), "byte {byte}");
                assert_eq!(path, seg);
            }
            other => panic!("byte {byte}: expected corruption error, got {other:?}"),
        }
        // Refusing to open must not modify the file.
        let mut after = std::fs::read(&seg).unwrap();
        after[(record_offset(3) + byte) as usize] ^= 1;
        assert_eq!(before, after, "open must not truncate on corruption");
    }
}

#[test]
fn point_in_time_mode_truncates_at_first_bad_record() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    flip_bit(&last_segment(dir.path()), record_offset(3) + HEADER_LEN, 0);

    let opts = fast_opts().recovery_mode(RecoveryMode::PointInTime);
    let wal = Wal::open_with(dir.path(), opts).unwrap();
    assert_eq!(read_all(&wal).len(), 2);
    let t = wal.recovery_info().truncated.clone().unwrap();
    assert_eq!(t.reason, CorruptionKind::ChecksumMismatch);
    assert_eq!(t.offset, record_offset(3));
}

#[test]
fn zero_filled_tail_is_treated_as_torn() {
    // After a crash, some filesystems expose an extended file size whose new
    // blocks were never written and read back as zeros.
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    let seg = last_segment(dir.path());
    let clean_len = file_len(&seg);
    append_bytes(&seg, &[0u8; 4096]);

    let wal = assert_recovers_to(dir.path(), N);
    assert_eq!(
        wal.recovery_info().truncated.clone().unwrap().offset,
        clean_len
    );
}

#[test]
fn torn_record_followed_by_zeros_is_treated_as_torn() {
    // A record whose payload sector never hit the disk, then zero fill.
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    let seg = last_segment(dir.path());
    let mut torn = encode_record(N + 1, &payload(N + 1));
    let last = torn.len() - 1;
    torn[last] ^= 0xFF;
    torn.extend_from_slice(&[0u8; 512]);
    append_bytes(&seg, &torn);

    assert_recovers_to(dir.path(), N);
}

#[test]
fn garbage_length_reaching_past_eof_is_treated_as_torn() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    let seg = last_segment(dir.path());
    let mut header = encode_record(N + 1, b"x");
    header[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    append_bytes(&seg, &header);

    let wal = assert_recovers_to(dir.path(), N);
    let reason = wal.recovery_info().truncated.clone().unwrap().reason;
    assert_eq!(reason, CorruptionKind::LengthTooLarge(u32::MAX));
}

#[test]
fn crash_simulation_partial_append_then_reopen_and_continue() {
    let dir = tempfile::tempdir().unwrap();
    let opts = fast_opts();

    // Round 1: write, then "crash" in the middle of appending record 6.
    drop(write_records(dir.path(), N, opts.clone()));
    let partial = encode_record(N + 1, b"this append never finished");
    append_bytes(&last_segment(dir.path()), &partial[..partial.len() / 2]);

    // Round 2: recovery drops the half record; new appends reuse LSN 6.
    let mut wal = Wal::open_with(dir.path(), opts.clone()).unwrap();
    assert!(wal.recovery_info().truncated.is_some());
    assert_eq!(wal.append(b"first after crash").unwrap(), Lsn(N + 1));
    assert_eq!(wal.append(b"second after crash").unwrap(), Lsn(N + 2));
    drop(wal);

    // Round 3: a clean reopen sees all records and no further truncation.
    let wal = Wal::open_with(dir.path(), opts).unwrap();
    assert_eq!(wal.recovery_info().truncated, None);
    let records = read_all(&wal);
    assert_eq!(records.len() as u64, N + 2);
    assert_eq!(records[N as usize].data, b"first after crash");
    assert_eq!(records[N as usize + 1].data, b"second after crash");
}

#[test]
fn valid_record_with_wrong_lsn_is_an_error() {
    // A checksum-valid record cannot be a torn write, so a sequencing break
    // is reported even at the tail.
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    append_bytes(&last_segment(dir.path()), &encode_record(N + 5, b"skipped"));

    let err = Wal::open_with(dir.path(), fast_opts()).unwrap_err();
    assert!(matches!(
        err,
        Error::Corruption {
            kind: CorruptionKind::UnexpectedLsn {
                expected: Lsn(6),
                found: Lsn(10)
            },
            ..
        }
    ));
}

#[test]
fn torn_segment_header_is_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    drop(Wal::open(dir.path()).unwrap());
    let seg = last_segment(dir.path());
    truncate_file(&seg, 3);

    let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();
    assert_eq!(
        wal.recovery_info().truncated.clone().unwrap().reason,
        CorruptionKind::TruncatedSegmentHeader
    );
    assert_eq!(file_len(&seg), SEGMENT_HEADER_LEN);
    wal.append(b"works").unwrap();
    drop(wal);
    assert_eq!(read_all(&Wal::open(dir.path()).unwrap()).len(), 1);
}

#[test]
fn bad_segment_header_magic_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    flip_bit(&last_segment(dir.path()), 0, 1);
    let err = Wal::open_with(dir.path(), fast_opts()).unwrap_err();
    assert!(matches!(
        err,
        Error::Corruption {
            kind: CorruptionKind::BadSegmentHeader,
            ..
        }
    ));
}

#[test]
fn recovery_is_idempotent() {
    // Crashing during (or right after) recovery and recovering again must
    // give the same answer.
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), N, fast_opts()));
    append_bytes(&last_segment(dir.path()), &[0xAB; 7]);

    drop(assert_recovers_to(dir.path(), N));
    let wal = assert_recovers_to(dir.path(), N);
    assert_eq!(wal.recovery_info().truncated, None);
}
