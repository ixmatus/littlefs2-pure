//! CRC32, the LittleFS variant.
//!
//! LittleFS uses CRC32 with the reflected polynomial `0xEDB88320` (the same
//! polynomial as IEEE 802.3, PNG, and zlib's CRC32). The seed is the caller's
//! choice; no final XOR is applied. The C reference uses a 16 entry nibble
//! table; this module mirrors that implementation byte for byte.
//!
//! # Bit accuracy claim
//!
//! [`update`] is required to produce the same `u32` the C reference's
//! `lfs2_crc` produces for any seed and any byte slice. This is anchored to
//! an oracle external to this codebase: littlefs's CRC is the standard
//! reflected CRC-32 (poly `0xEDB88320`), whose published check value is
//! `CRC32("123456789") = 0xCBF43926` (the "CRC-32/ISO-HDLC" entry in the CRC
//! RevEng catalogue and Koopman's CRC reference). littlefs stores the raw
//! running register with no final XOR, so the relation to the catalogue
//! constant is `!update(INIT, b"123456789") == 0xCBF43926`. The
//! `external_crc32_check_value` unit test pins exactly that, so a shared
//! misconception of the variant (which would pass a self-consistency test)
//! cannot pass unnoticed. The property test in `tests/property_crc.rs`
//! checks the table implementation against a bit by bit reference over
//! random inputs; the table/bitwise/associativity tests below guard internal
//! consistency.

/// The nibble table for the reflected polynomial `0xEDB88320`. This matches
/// `rtable` in `lfs2_util.h` of the C reference, byte for byte.
const NIBBLE_TABLE: [u32; 16] = [
    0x00000000, 0x1db71064, 0x3b6e20c8, 0x26d930ac, 0x76dc4190, 0x6b6b51f4, 0x4db26158, 0x5005713c,
    0xedb88320, 0xf00f9344, 0xd6d6a3e8, 0xcb61b38c, 0x9b64c2b0, 0x86d3d2d4, 0xa00ae278, 0xbdbdf21c,
];

/// The seed LittleFS uses for a fresh CRC computation.
///
/// A fresh CRC always starts at `0xFFFFFFFF`. Subsequent chunks of the same
/// commit body feed the previous return value back in as the seed.
pub const INIT: u32 = 0xFFFFFFFF;

/// Update a running CRC with a new byte slice.
///
/// To compute the CRC of a complete buffer, call with `seed = INIT`:
///
/// ```
/// use littlefs2_pure::crc;
///
/// // External anchor: the published CRC-32 check value is the CRC of the
/// // nine ASCII bytes "123456789". littlefs stores the raw register with
/// // no final XOR, so the relation to the catalogue constant 0xCBF43926
/// // ("CRC-32/ISO-HDLC", CRC RevEng catalogue) is a final complement.
/// let raw = crc::update(crc::INIT, b"123456789");
/// assert_eq!(!raw, 0xCBF4_3926);
/// ```
#[must_use]
pub fn update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        // Process low nibble, then high nibble. The XOR with the input byte
        // is masked to four bits inside the index expression; the C reference
        // uses the same idiom.
        crc = (crc >> 4) ^ NIBBLE_TABLE[((crc ^ u32::from(b)) & 0xf) as usize];
        crc = (crc >> 4) ^ NIBBLE_TABLE[((crc ^ u32::from(b >> 4)) & 0xf) as usize];
    }
    crc
}

/// Compute the CRC of a complete buffer in one call, starting from [`INIT`].
///
/// Equivalent to `update(INIT, data)`.
#[inline]
#[must_use]
pub fn compute(data: &[u8]) -> u32 {
    update(INIT, data)
}

/// Reference implementation: bit by bit CRC over the reflected polynomial.
///
/// This is the algorithm the table is *encoding*. It exists for property test
/// cross checking; production code uses [`update`] (the table version, which
/// is roughly 8 times faster). The two implementations agree by construction
/// for every seed and every input.
#[must_use]
#[doc(hidden)]
pub fn update_bitwise(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB88320 & mask);
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// External oracle anchor. The check value of the standard reflected
    /// CRC-32 (poly `0xEDB88320`) is, by definition, `CRC32("123456789")
    /// = 0xCBF43926`, published in the CRC RevEng catalogue and Koopman's
    /// CRC reference as "CRC-32/ISO-HDLC". littlefs stores the raw
    /// register with no final XOR, so the relation is a final complement.
    /// This pins `update` to a constant produced outside this codebase
    /// (and outside littlefs's source): a shared misconception of the
    /// variant would still pass the self-consistency tests below but
    /// fails here.
    #[test]
    fn external_crc32_check_value() {
        let raw = update(INIT, b"123456789");
        assert_eq!(!raw, 0xCBF4_3926, "littlefs CRC must be standard reflected CRC-32");
        // Also pin the raw register littlefs actually stores on disk.
        assert_eq!(raw, 0x340B_C6D9);
    }

    /// CRC of an empty slice is the seed itself, unchanged.
    #[test]
    fn empty_slice_returns_seed() {
        assert_eq!(update(INIT, b""), INIT);
        assert_eq!(update(0xdead_beef, b""), 0xdead_beef);
    }

    /// The table CRC and the bitwise reference produce the same value.
    /// A property test in `tests/property_crc.rs` extends this to random
    /// inputs.
    #[test]
    fn table_matches_bitwise_small_inputs() {
        for seed in [0u32, INIT, 0xdead_beef, 0x1234_5678] {
            for data in [
                &b""[..],
                &b"\x00"[..],
                &b"\xff"[..],
                &b"littlefs"[..],
                &b"The quick brown fox jumps over the lazy dog"[..],
                &b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f"[..],
            ] {
                assert_eq!(
                    update(seed, data),
                    update_bitwise(seed, data),
                    "table vs bitwise mismatch at seed = {seed:#x}, data = {data:?}"
                );
            }
        }
    }

    /// CRC is associative across slice concatenation. `crc(seed, a || b) ==
    /// crc(crc(seed, a), b)`.
    #[test]
    fn associative_over_concatenation() {
        let parts: [&[u8]; 3] = [b"little", b"fs", b"!!!"];
        let mut combined = [0u8; 11];
        combined[..6].copy_from_slice(parts[0]);
        combined[6..8].copy_from_slice(parts[1]);
        combined[8..].copy_from_slice(parts[2]);

        let one_shot = update(INIT, &combined);
        let streamed = parts.iter().fold(INIT, |crc, p| update(crc, p));
        assert_eq!(one_shot, streamed);
    }

    /// The table itself matches its computed values. Each entry is the CRC of
    /// the corresponding 4 bit input when processed by the bitwise reference
    /// with the table's nibble step.
    #[test]
    fn nibble_table_matches_polynomial() {
        for i in 0..16u32 {
            // The table entry `i` is the value of the 32 bit CRC after
            // processing `i` as a low nibble with starting state 0. The
            // equivalent bitwise computation runs four shifts.
            let mut crc = i;
            for _ in 0..4 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB88320 & mask);
            }
            assert_eq!(NIBBLE_TABLE[i as usize], crc, "nibble table entry {i} mismatch");
        }
    }
}
