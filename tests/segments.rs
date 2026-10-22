//! Segment rollover, cross-segment replay, and checkpoint truncation.

mod common;

use common::*;
use rust_wal::{CorruptionKind, Error, Lsn, Wal, WalOptions};

/// Each test record is 16 + 13 = 29 bytes; 8 + 4 * 29 = 124, so a 128-byte
/// limit fits exactly four records per segment.
fn small_segments() -> WalOptions {
    fast_opts().max_segment_bytes(128)
}

#[test]
fn rollover_creates_segments_named_by_base_lsn() {
    let dir = tempfile::tempdir().unwrap();
    let wal = write_records(dir.path(), 10, small_segments());
    assert_eq!(wal.segment_count(), 3);

    let names: Vec<String> = segment_files(dir.path())
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "00000000000000000001.wal",
            "00000000000000000005.wal",
            "00000000000000000009.wal",
        ]
    );
    for seg in segment_files(dir.path()) {
        assert!(file_len(&seg) <= 128);
    }
}

#[test]
fn replay_spans_segments_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), 25, small_segments()));

    let mut wal = Wal::open_with(dir.path(), small_segments()).unwrap();
    assert_eq!(wal.recovery_info().segments, 7);
    assert_eq!(wal.recovery_info().records, 25);
    let lsns: Vec<u64> = read_all(&wal).iter().map(|r| r.lsn.0).collect();
    assert_eq!(lsns, (1..=25).collect::<Vec<_>>());

    // iter_from in a later segment.
    let tail: Vec<u64> = wal
        .iter_from(Lsn(18))
        .unwrap()
        .map(|r| r.unwrap().lsn.0)
        .collect();
    assert_eq!(tail, (18..=25).collect::<Vec<_>>());

    assert_eq!(wal.append(b"more").unwrap(), Lsn(26));
}

#[test]
fn record_larger_than_segment_gets_its_own_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open_with(dir.path(), small_segments()).unwrap();
    wal.append(b"small").unwrap();
    wal.append(&[7u8; 1000]).unwrap();
    wal.append(b"small again").unwrap();
    assert_eq!(wal.segment_count(), 3);
    drop(wal);

    let wal = Wal::open_with(dir.path(), small_segments()).unwrap();
    let records = read_all(&wal);
    assert_eq!(records[1].data, vec![7u8; 1000]);
    assert_eq!(records[2].data, b"small again");
}

#[test]
fn truncate_before_removes_only_fully_obsolete_segments() {
    let dir = tempfile::tempdir().unwrap();
    // Segments: [1..=4] [5..=8] [9..=12] [13..=14]
    let mut wal = write_records(dir.path(), 14, small_segments());
    assert_eq!(wal.segment_count(), 4);

    // LSN 7 is inside segment 2, so only segment 1 can go.
    assert_eq!(wal.truncate_before(Lsn(7)).unwrap(), 1);
    assert_eq!(wal.first_lsn(), Lsn(5));
    // Exactly at a boundary: segment [5..=8] is now obsolete.
    assert_eq!(wal.truncate_before(Lsn(9)).unwrap(), 1);
    assert_eq!(wal.first_lsn(), Lsn(9));
    // The active segment is never removed.
    assert_eq!(wal.truncate_before(Lsn(1_000)).unwrap(), 1);
    assert_eq!(wal.segment_count(), 1);
    assert_eq!(wal.first_lsn(), Lsn(13));

    assert!(matches!(
        wal.iter_from(Lsn(5)),
        Err(Error::LsnOutOfRange(_))
    ));
    let lsns: Vec<u64> = read_all(&wal).iter().map(|r| r.lsn.0).collect();
    assert_eq!(lsns, vec![13, 14]);
    drop(wal);

    // The truncated log reopens with LSNs intact.
    let mut wal = Wal::open_with(dir.path(), small_segments()).unwrap();
    assert_eq!(wal.first_lsn(), Lsn(13));
    assert_eq!(wal.append(b"x").unwrap(), Lsn(15));
}

#[test]
fn torn_tail_in_active_segment_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), 10, small_segments()));
    append_bytes(&last_segment(dir.path()), &[1, 2, 3]);

    let wal = Wal::open_with(dir.path(), small_segments()).unwrap();
    assert_eq!(read_all(&wal).len(), 10);
    assert!(wal.recovery_info().truncated.is_some());
}

#[test]
fn damage_in_sealed_segment_is_an_error_even_at_its_end() {
    // A sealed segment was fsynced before the next one was created, so a
    // short or damaged sealed segment is never a torn write.
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), 10, small_segments()));
    let first = &segment_files(dir.path())[0];
    let len = file_len(first);
    truncate_file(first, len - 1);

    let err = Wal::open_with(dir.path(), small_segments()).unwrap_err();
    assert!(matches!(
        err,
        Error::Corruption {
            kind: CorruptionKind::TruncatedPayload,
            ..
        }
    ));
}

#[test]
fn missing_middle_segment_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), 10, small_segments()));
    std::fs::remove_file(&segment_files(dir.path())[1]).unwrap();

    let err = Wal::open_with(dir.path(), small_segments()).unwrap_err();
    assert!(matches!(
        err,
        Error::Corruption {
            kind: CorruptionKind::SegmentGap {
                expected: Lsn(5),
                found: Lsn(9)
            },
            ..
        }
    ));
}

#[test]
fn reopening_with_a_different_segment_size_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), 10, small_segments()));
    let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();
    for _ in 0..10 {
        wal.append(b"bigger segments now").unwrap();
    }
    assert_eq!(read_all(&wal).len(), 20);
}
