//! Property tests for the LittleFS CRC32 implementation.
//!
//! The claim under test: [`littlefs2_pure::crc::update`] (the nibble table
//! implementation, used at runtime) produces the same `u32` as
//! `update_bitwise` (a bit by bit reference that encodes the polynomial
//! directly) for every seed and every input.
//!
//! The bitwise reference is itself unit tested in `src/crc.rs` against the
//! polynomial definition, and against a small set of hand crafted inputs.
//! Treating it as an oracle in this property test gives us "table CRC matches
//! polynomial" over a much wider input distribution than unit tests alone.

use littlefs2_pure::crc;
use proptest::prelude::*;

mod common;

proptest! {
    /// The table implementation and the bitwise reference agree at every
    /// seed and every input.
    #[test]
    fn table_matches_bitwise(
        seed: u32,
        data in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let table = crc::update(seed, &data);
        let bitwise = crc::update_bitwise(seed, &data);
        prop_assert_eq!(table, bitwise);
    }

    /// `update` is associative across concatenation: streaming a buffer
    /// chunk by chunk yields the same CRC as feeding it in one shot.
    #[test]
    fn associative_across_arbitrary_splits(
        seed: u32,
        data in proptest::collection::vec(any::<u8>(), 0..256),
        split in 0usize..256,
    ) {
        let split = split.min(data.len());
        let one_shot = crc::update(seed, &data);
        let streamed = crc::update(crc::update(seed, &data[..split]), &data[split..]);
        prop_assert_eq!(one_shot, streamed);
    }

    /// `compute` is a shorthand for `update(INIT, data)`.
    #[test]
    fn compute_is_update_from_init(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        prop_assert_eq!(crc::compute(&data), crc::update(crc::INIT, &data));
    }
}
