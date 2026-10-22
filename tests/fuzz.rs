//! Randomized crash testing without external dependencies.
//!
//! Property: after any random damage to the final segment (truncation,
//! garbage append, or a bit flip), `Wal::open` either
//!   * succeeds and the log is an exact *prefix* of what was written, or
//!   * fails with `Error::Corruption`.
//!
//! It must never return wrong data, reorder records, or panic.

mod common;

use common::*;
use rust_wal::{Error, Lsn, Wal};

/// Tiny deterministic PRNG (xorshift64*), so failures are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_damage_never_yields_wrong_data() {
    let mut rng = Rng(0x5EED_CAFE_F00D_BEEF);
    let mut recovered = 0;
    let mut rejected = 0;

    for case in 0..300 {
        let dir = tempfile::tempdir().unwrap();
        let n = 1 + rng.below(20);
        let written: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let len = rng.below(200) as usize;
                (0..len).map(|j| (i as usize * 31 + j) as u8).collect()
            })
            .collect();
        {
            let mut wal = Wal::open_with(dir.path(), fast_opts()).unwrap();
            for data in &written {
                wal.append(data).unwrap();
            }
        }

        let seg = last_segment(dir.path());
        let len = file_len(&seg);
        match rng.below(3) {
            0 => truncate_file(&seg, rng.below(len + 1)),
            1 => {
                let junk: Vec<u8> = (0..1 + rng.below(64)).map(|_| rng.next() as u8).collect();
                append_bytes(&seg, &junk);
            }
            _ => flip_bit(&seg, rng.below(len), rng.below(8) as u8),
        }

        match Wal::open_with(dir.path(), fast_opts()) {
            Ok(wal) => {
                let records = read_all(&wal);
                assert!(records.len() <= written.len(), "case {case}: extra records");
                for (i, r) in records.iter().enumerate() {
                    assert_eq!(r.lsn, Lsn(i as u64 + 1), "case {case}");
                    assert_eq!(r.data, written[i], "case {case}: wrong data");
                }
                recovered += 1;
            }
            Err(Error::Corruption { .. }) => rejected += 1,
            Err(e) => panic!("case {case}: unexpected error {e}"),
        }
    }
    // Make sure both outcomes were actually exercised.
    assert!(recovered > 0 && rejected > 0, "{recovered} / {rejected}");
}
