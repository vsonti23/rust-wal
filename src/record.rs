//! On-disk record encoding.
//!
//! Every record is a fixed 16-byte header followed by the payload:
//!
//! ```text
//! offset  size  field
//! 0       4     payload length (u32, little-endian)
//! 4       4     CRC32 of bytes [0..4] ++ [8..16] ++ payload (u32, LE)
//! 8       8     LSN (u64, little-endian)
//! 16      len   payload
//! ```
//!
//! The checksum covers the length and the LSN as well as the payload, so a
//! flipped bit anywhere in the record (except inside the CRC field itself,
//! which also produces a mismatch) is detected.

use crate::Lsn;

/// Size of the fixed record header in bytes.
pub(crate) const HEADER_LEN: usize = 16;

/// A decoded record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub len: u32,
    pub crc: u32,
    pub lsn: Lsn,
}

impl Header {
    /// Decode a header from exactly `HEADER_LEN` bytes.
    ///
    /// Taking `&[u8; HEADER_LEN]` instead of `&[u8]` moves the length check
    /// into the type system: a caller cannot pass a short slice.
    pub fn decode(buf: &[u8; HEADER_LEN]) -> Header {
        Header {
            len: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            crc: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            lsn: Lsn(u64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ])),
        }
    }

    /// Total on-disk size of the record this header describes.
    pub fn record_len(&self) -> u64 {
        HEADER_LEN as u64 + u64::from(self.len)
    }

    /// Check the stored CRC against the header fields and `payload`.
    pub fn verify(&self, payload: &[u8]) -> bool {
        checksum(self.len, self.lsn, payload) == self.crc
    }
}

/// CRC32 (IEEE) over the length, LSN, and payload.
pub(crate) fn checksum(len: u32, lsn: Lsn, payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&len.to_le_bytes());
    hasher.update(&lsn.0.to_le_bytes());
    hasher.update(payload);
    hasher.finalize()
}

/// Encode a full record (header + payload) into `buf`, replacing its contents.
///
/// The caller owns `buf` and passes it in by `&mut` so the same allocation is
/// reused for every append instead of allocating a fresh `Vec` each time.
/// Building the whole record in memory also means it reaches the kernel in a
/// single `write` call.
pub(crate) fn encode_into(buf: &mut Vec<u8>, lsn: Lsn, payload: &[u8]) {
    let len = u32::try_from(payload.len()).expect("payload length checked by caller");
    buf.clear();
    buf.reserve(HEADER_LEN + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&checksum(len, lsn, payload).to_le_bytes());
    buf.extend_from_slice(&lsn.0.to_le_bytes());
    buf.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_of(buf: &[u8]) -> Header {
        Header::decode(buf[..HEADER_LEN].try_into().unwrap())
    }

    #[test]
    fn encode_decode_round_trip() {
        let mut buf = Vec::new();
        encode_into(&mut buf, Lsn(42), b"hello");
        assert_eq!(buf.len(), HEADER_LEN + 5);

        let header = header_of(&buf);
        assert_eq!(header.len, 5);
        assert_eq!(header.lsn, Lsn(42));
        assert_eq!(header.record_len(), 21);
        assert!(header.verify(&buf[HEADER_LEN..]));
    }

    #[test]
    fn byte_layout_is_stable() {
        // Golden test: the on-disk format is a compatibility contract, so pin it.
        let mut buf = Vec::new();
        encode_into(&mut buf, Lsn(1), b"ab");
        assert_eq!(&buf[0..4], &[2, 0, 0, 0], "length, little-endian");
        assert_eq!(&buf[8..16], &[1, 0, 0, 0, 0, 0, 0, 0], "LSN, little-endian");
        assert_eq!(&buf[16..], b"ab");
        let crc = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(crc, checksum(2, Lsn(1), b"ab"));
    }

    #[test]
    fn every_single_bit_flip_is_detected() {
        let mut buf = Vec::new();
        encode_into(&mut buf, Lsn(7), b"write-ahead log");
        for byte in 0..buf.len() {
            for bit in 0..8 {
                let mut damaged = buf.clone();
                damaged[byte] ^= 1 << bit;
                let header = header_of(&damaged);
                let payload_ok = header.len as usize == damaged.len() - HEADER_LEN;
                assert!(
                    !payload_ok || !header.verify(&damaged[HEADER_LEN..]),
                    "flip of byte {byte} bit {bit} went undetected"
                );
            }
        }
    }

    #[test]
    fn checksum_binds_the_lsn() {
        // The same payload at a different position must not validate.
        assert_ne!(checksum(3, Lsn(1), b"abc"), checksum(3, Lsn(2), b"abc"));
    }

    #[test]
    fn empty_payload_is_valid() {
        let mut buf = Vec::new();
        encode_into(&mut buf, Lsn(9), b"");
        assert_eq!(buf.len(), HEADER_LEN);
        assert!(header_of(&buf).verify(b""));
    }

    #[test]
    fn all_zero_header_does_not_validate() {
        // Zero-filled tails (common after a crash on some filesystems) must be
        // rejected rather than parsed as an empty record with LSN 0.
        let header = Header::decode(&[0u8; HEADER_LEN]);
        assert!(!header.verify(b""));
    }
}
