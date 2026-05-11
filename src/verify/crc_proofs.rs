//! CRC table-vs-bitwise agreement.
//!
//! `crc::update` uses a 16-entry nibble table; `crc::update_bitwise`
//! is the textbook bit-by-bit reference. The property test in
//! `tests/property_crc.rs` shows agreement for random byte slices up
//! to a few hundred bytes; these Kani proofs nail it down for
//! exhaustive 1-byte and 2-byte inputs across every seed.

use crate::crc;

/// For every seed and every single-byte input, the table CRC matches
/// the bit-by-bit reference. Exhaustive over `(seed, byte)` pairs.
#[kani::proof]
fn crc_table_matches_bitwise_one_byte() {
    let seed: u32 = kani::any();
    let byte: u8 = kani::any();
    let table = crc::update(seed, &[byte]);
    let bitwise = crc::update_bitwise(seed, &[byte]);
    assert_eq!(table, bitwise);
}

/// Same for two-byte inputs. Doubles the symbolic state space; still
/// well within Kani's budget at the geometry the kernel cares about.
#[kani::proof]
fn crc_table_matches_bitwise_two_bytes() {
    let seed: u32 = kani::any();
    let b0: u8 = kani::any();
    let b1: u8 = kani::any();
    let table = crc::update(seed, &[b0, b1]);
    let bitwise = crc::update_bitwise(seed, &[b0, b1]);
    assert_eq!(table, bitwise);
}

/// Empty input is the identity: `update(seed, &[]) == seed`. Trivial
/// but worth pinning as a regression guard.
#[kani::proof]
fn crc_empty_input_is_identity() {
    let seed: u32 = kani::any();
    assert_eq!(crc::update(seed, &[]), seed);
    assert_eq!(crc::update_bitwise(seed, &[]), seed);
}

/// Streaming and one-shot agree: `update(update(seed, a), b)` equals
/// `update(seed, concat(a, b))` for one-byte chunks. The
/// `tests::property_crc::associative_over_concatenation` unit test
/// covers a few example inputs; this Kani proof covers every pair.
#[kani::proof]
fn crc_associative_over_one_byte_concatenation() {
    let seed: u32 = kani::any();
    let a: u8 = kani::any();
    let b: u8 = kani::any();
    let streamed = crc::update(crc::update(seed, &[a]), &[b]);
    let one_shot = crc::update(seed, &[a, b]);
    assert_eq!(streamed, one_shot);
}
