//! Shared helpers for integration tests.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rust_wal::{Record, SyncPolicy, Wal, WalOptions};

/// Size of a record header on disk (length + CRC32 + LSN).
pub const HEADER_LEN: u64 = 16;
/// Size of the per-segment file header.
pub const SEGMENT_HEADER_LEN: u64 = 8;

/// Options that keep tests fast: no fsync on every append.
pub fn fast_opts() -> WalOptions {
    WalOptions::default().sync_policy(SyncPolicy::Never)
}

pub fn payload(i: u64) -> Vec<u8> {
    format!("record-{i:06}").into_bytes()
}

/// Open a fresh log in `dir` and append `n` records.
pub fn write_records(dir: &Path, n: u64, opts: WalOptions) -> Wal {
    let mut wal = Wal::open_with(dir, opts).unwrap();
    for i in 1..=n {
        wal.append(&payload(i)).unwrap();
    }
    wal
}

pub fn read_all(wal: &Wal) -> Vec<Record> {
    wal.iter()
        .unwrap()
        .collect::<rust_wal::Result<Vec<_>>>()
        .unwrap()
}

/// Segment files in `dir`, sorted (file names sort by base LSN).
pub fn segment_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    files.sort();
    files
}

/// The (only or last) segment file.
pub fn last_segment(dir: &Path) -> PathBuf {
    segment_files(dir).pop().expect("at least one segment")
}

pub fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

pub fn truncate_file(path: &Path, len: u64) {
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

pub fn append_bytes(path: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

pub fn flip_bit(path: &Path, offset: u64, bit: u8) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset as usize] ^= 1 << bit;
    fs::write(path, bytes).unwrap();
}

/// Byte offset of the record with 1-based index `i` in a single-segment log
/// written with `write_records` (all payloads have the same length).
pub fn record_offset(i: u64) -> u64 {
    SEGMENT_HEADER_LEN + (i - 1) * (HEADER_LEN + payload(1).len() as u64)
}

/// Encode a record exactly as the library does, for crafting crash images.
pub fn encode_record(lsn: u64, data: &[u8]) -> Vec<u8> {
    let len = data.len() as u32;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&len.to_le_bytes());
    hasher.update(&lsn.to_le_bytes());
    hasher.update(data);
    let crc = hasher.finalize();
    let mut out = Vec::new();
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&lsn.to_le_bytes());
    out.extend_from_slice(data);
    out
}
