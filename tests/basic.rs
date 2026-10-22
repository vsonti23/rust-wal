//! Happy-path behaviour: append, replay, reopen, sync policies, limits.

mod common;

use common::*;
use rust_wal::{Error, Lsn, SyncPolicy, Wal, WalOptions};

#[test]
fn round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();

    let a = wal.append(b"alpha").unwrap();
    let b = wal.append(b"beta").unwrap();
    let c = wal.append(b"").unwrap();
    assert_eq!((a, b, c), (Lsn(1), Lsn(2), Lsn(3)));

    let records = read_all(&wal);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].data, b"alpha");
    assert_eq!(records[1].data, b"beta");
    assert_eq!(records[2].data, b"");
    assert_eq!(records[2].lsn, Lsn(3));
}

#[test]
fn empty_log() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open(dir.path()).unwrap();
    assert_eq!(wal.first_lsn(), Lsn(1));
    assert_eq!(wal.next_lsn(), Lsn(1));
    assert_eq!(wal.last_lsn(), None);
    assert!(read_all(&wal).is_empty());
    drop(wal);

    // Reopening an empty log is also clean.
    let wal = Wal::open(dir.path()).unwrap();
    assert_eq!(wal.recovery_info().records, 0);
    assert_eq!(wal.recovery_info().truncated, None);
    assert!(read_all(&wal).is_empty());
}

#[test]
fn replay_after_reopen_and_lsns_continue() {
    let dir = tempfile::tempdir().unwrap();
    drop(write_records(dir.path(), 100, fast_opts()));

    let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();
    assert_eq!(wal.recovery_info().records, 100);
    assert_eq!(wal.recovery_info().truncated, None);
    assert_eq!(wal.last_lsn(), Some(Lsn(100)));
    assert_eq!(wal.append(b"after reopen").unwrap(), Lsn(101));

    let records = read_all(&wal);
    assert_eq!(records.len(), 101);
    for (i, r) in records.iter().take(100).enumerate() {
        assert_eq!(r.lsn, Lsn(i as u64 + 1));
        assert_eq!(r.data, payload(i as u64 + 1));
    }
    assert_eq!(records[100].data, b"after reopen");
}

#[test]
fn iter_from_starts_mid_log() {
    let dir = tempfile::tempdir().unwrap();
    let wal = write_records(dir.path(), 10, fast_opts());

    let lsns: Vec<u64> = wal
        .iter_from(Lsn(7))
        .unwrap()
        .map(|r| r.unwrap().lsn.0)
        .collect();
    assert_eq!(lsns, vec![7, 8, 9, 10]);

    // `next_lsn` is a valid, empty starting point; beyond it is not.
    assert_eq!(wal.iter_from(Lsn(11)).unwrap().count(), 0);
    assert!(matches!(
        wal.iter_from(Lsn(12)),
        Err(Error::LsnOutOfRange(Lsn(12)))
    ));
    assert!(matches!(
        wal.iter_from(Lsn(0)),
        Err(Error::LsnOutOfRange(Lsn(0)))
    ));
}

#[test]
fn large_records() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();

    // 8 MiB of non-repeating bytes, plus small records around it.
    let big: Vec<u8> = (0..8 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    wal.append(b"before").unwrap();
    wal.append(&big).unwrap();
    wal.append(b"after").unwrap();
    drop(wal);

    let wal = Wal::open_with(dir.path(), fast_opts()).unwrap();
    let records = read_all(&wal);
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].data, big);
    assert_eq!(records[2].data, b"after");
}

#[test]
fn oversized_record_is_rejected_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let opts = fast_opts().max_record_bytes(1024);
    let mut wal = Wal::open_with(dir.path(), opts).unwrap();

    wal.append(&[1u8; 1024]).unwrap(); // exactly at the limit is fine
    let err = wal.append(&[1u8; 1025]).unwrap_err();
    assert!(matches!(
        err,
        Error::RecordTooLarge {
            size: 1025,
            max: 1024
        }
    ));

    // The failed append consumed no LSN and the log is still usable.
    assert_eq!(wal.append(b"ok").unwrap(), Lsn(2));
    assert_eq!(read_all(&wal).len(), 2);
}

#[test]
fn sync_policy_always_leaves_nothing_unsynced() {
    let dir = tempfile::tempdir().unwrap();
    let opts = WalOptions::default().sync_policy(SyncPolicy::Always);
    let mut wal = Wal::open_with(dir.path(), opts).unwrap();
    for i in 0..5 {
        wal.append(&payload(i)).unwrap();
        assert_eq!(wal.unsynced_records(), 0);
    }
}

#[test]
fn sync_policy_every_n_batches_fsyncs() {
    let dir = tempfile::tempdir().unwrap();
    let opts = WalOptions::default().sync_policy(SyncPolicy::EveryN(4));
    let mut wal = Wal::open_with(dir.path(), opts).unwrap();

    let mut observed = Vec::new();
    for i in 0..9 {
        wal.append(&payload(i)).unwrap();
        observed.push(wal.unsynced_records());
    }
    // Counter resets to 0 on every 4th append.
    assert_eq!(observed, vec![1, 2, 3, 0, 1, 2, 3, 0, 1]);

    wal.sync().unwrap();
    assert_eq!(wal.unsynced_records(), 0);
}

#[test]
fn sync_policy_never_defers_to_explicit_sync() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();
    for i in 0..10 {
        wal.append(&payload(i)).unwrap();
    }
    assert_eq!(wal.unsynced_records(), 10);
    wal.sync().unwrap();
    assert_eq!(wal.unsynced_records(), 0);
    // Syncing with nothing pending is a cheap no-op.
    wal.sync().unwrap();
}

#[test]
fn invalid_options_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let err = Wal::open_with(
        dir.path(),
        WalOptions::default().sync_policy(SyncPolicy::EveryN(0)),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidConfig(_)));

    let err = Wal::open_with(dir.path(), WalOptions::default().max_segment_bytes(10)).unwrap_err();
    assert!(matches!(err, Error::InvalidConfig(_)));
}

#[test]
fn unrelated_files_in_directory_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.txt"), b"not a segment").unwrap();
    std::fs::write(dir.path().join("123.wal"), b"wrong name format").unwrap();
    let mut wal = Wal::open(dir.path()).unwrap();
    wal.append(b"x").unwrap();
    assert_eq!(read_all(&wal).len(), 1);
}
