//! Property tests for the CTZ skip list geometry math.
//!
//! # Why the oracle is built from the format rules, not from the crate
//!
//! Review finding L8 (`lfs-9z5`) killed the previous version of this
//! file. Its "brute force" walk summed per block capacities obtained by
//! calling [`content_bytes_in_block`] and [`skip_pointers_in_block`],
//! the very functions under test. A wrong pointer count would have
//! shifted both sides of every comparison by the same amount, so the
//! suite was self consistency wearing an oracle's clothes.
//!
//! The oracle in this file therefore reaches for the format description
//! instead of the crate. Its derivation is:
//!
//! 1. `DESIGN.md` at the pinned oracle revision `d01280e` (tag
//!    `v2.9.3`, vendored at
//!    `docs/references/vendor/design-littlefs/DESIGN.md`, section on
//!    CTZ skip lists, the rules paragraph at lines 622 to 630) states
//!    the structural rule: for every block `n` that is divisible by
//!    `2^x`, that block stores a pointer to block `n - 2^x`.
//! 2. [`oracle_pointer_count`] implements that sentence literally. It
//!    counts the values of `x` for which `2^x` divides `n` and
//!    `n - 2^x` is a block that exists. Block `0` falls out with zero
//!    pointers because `0 - 2^x` is negative for every `x`, so the
//!    special case is derived rather than borrowed.
//! 3. Each block spends four bytes per pointer on a little endian
//!    `u32` at the head of the block, and every remaining byte holds
//!    file content, so capacity is `block_size - 4 * pointers`.
//! 4. [`oracle_locate`] lays the file out one block at a time from
//!    block `0`, accumulating capacities until the requested offset
//!    lands inside a block.
//!
//! Nothing on the oracle side calls into `littlefs2_pure::ctz`, and the
//! shapes differ on purpose: the oracle iterates over candidate
//! divisors and accumulates, while the implementation uses
//! `trailing_zeros` and the closed form population count identity that
//! `lfs_ctz_index` uses (`lfs.c:2843` at the same pinned revision).
//! An off by one in either the pointer count or the capacity now moves
//! exactly one side of the comparison.
//!
//! [`PINNED_VECTORS`] adds a second, even more independent tier: hand
//! computed `(offset, block, offset_in_block)` triples across three
//! geometries, checked against numbers written down by walking the
//! layout on paper rather than by running any code.

use littlefs2_pure::ctz::{
    block_count, block_index_at_offset, content_bytes_in_block, skip_pointers_in_block,
};
use proptest::prelude::*;

mod common;

/// Block indices biased toward the low end.
///
/// A uniform draw from `0..=200_000` reaches block `0` about once every
/// two hundred thousand cases, so a fault confined to the head of the
/// chain survives a whole proptest run. Block `0` is exactly where the
/// format's pointer rule has its only special case, so the strategy
/// spends a third of its cases on the first few blocks and another
/// third on the first few hundred.
fn block_index_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![0u32..=8, 0u32..=512, 0u32..=200_000]
}

/// File offsets biased toward the low end, for the same reason.
fn offset_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![0u32..=16, 0u32..=4_096, 0u32..=200_000]
}

/// Number of skip pointers stored at the head of block `n`, counted
/// straight from the format rule rather than from a trailing zero
/// count.
///
/// The rule: block `n` stores a pointer to block `n - 2^x` for every
/// `x` such that `2^x` divides `n`. A pointer only exists when the
/// target block exists, that is when `2^x <= n`.
///
/// The loop deliberately does not assume that divisibility by
/// successive powers of two fails monotonically. It tests every
/// candidate `x` from `0` up to the largest power of two that still
/// fits inside `n`, so a wrong assumption about the shape of the
/// sequence cannot hide here.
fn oracle_pointer_count(n: u32) -> u32 {
    let mut count = 0u32;
    for x in 0u32..32 {
        let step = 1u32 << x;
        if step > n {
            // Block `n - 2^x` would sit before the head of the chain,
            // so no pointer is stored. For `n == 0` this rejects every
            // candidate on the first iteration, which is exactly why
            // block 0 is pure content.
            break;
        }
        if n % step == 0 {
            count += 1;
        }
    }
    count
}

/// Content bytes in block `n`, from the pointer count the format rule
/// dictates and the four bytes each pointer occupies.
fn oracle_capacity(n: u32, block_size: u32) -> u32 {
    block_size - 4 * oracle_pointer_count(n)
}

/// Lay the file out from block `0` one block at a time and report the
/// block that holds byte `offset`, together with the byte position of
/// that byte inside the block as it sits on disk.
///
/// The on disk position counts the skip pointer header, because the
/// header occupies the front of the block and the content follows it.
/// This matches what [`block_index_at_offset`] promises its callers, so
/// a read issued at the returned position lands on the requested byte.
fn oracle_locate(offset: u32, block_size: u32) -> (u32, u32) {
    let mut consumed = 0u32;
    let mut index = 0u32;
    loop {
        let capacity = oracle_capacity(index, block_size);
        if offset < consumed + capacity {
            let header = 4 * oracle_pointer_count(index);
            return (index, header + (offset - consumed));
        }
        consumed = consumed.saturating_add(capacity);
        index += 1;
        // Sanity bound so a broken oracle fails loudly instead of
        // hanging the suite.
        assert!(index <= 1_000_000, "oracle_locate exceeded 1M iterations");
    }
}

/// Number of blocks a file of `size` bytes occupies, accumulated the
/// same independent way.
fn oracle_block_count(size: u32, block_size: u32) -> u32 {
    if size == 0 {
        return 0;
    }
    let mut consumed = 0u32;
    let mut index = 0u32;
    loop {
        consumed = consumed.saturating_add(oracle_capacity(index, block_size));
        if consumed >= size {
            return index + 1;
        }
        index += 1;
        assert!(index <= 1_000_000, "oracle_block_count exceeded 1M iterations");
    }
}

/// Hand computed layout vectors: `(block_size, offset, block_index,
/// offset_within_block)`.
///
/// Every row was worked out on paper from the pointer counts
/// `0, 1, 2, 1, 3, 1, 2, 1, 4` for blocks `0` through `8`. For block
/// size 256 that gives capacities `256, 252, 248, 252, 244, 252, 248,
/// 252, 240` and cumulative content boundaries at `0, 256, 508, 756,
/// 1008, 1252, 1504, 1752, 2004`. The rows pin the last byte of each
/// block and the first byte of the next, because those are the offsets
/// an off by one moves first.
///
/// Three geometries appear so the suite does not inherit the 256 byte
/// block monoculture the review flagged as coverage debt.
const PINNED_VECTORS: &[(u32, u32, u32, u32)] = &[
    // Block size 128. Capacities 128, 124, 120, 124, 116, 124, 120,
    // 124, 112; boundaries 0, 128, 252, 372, 496, 612, 736, 856, 980.
    (128, 0, 0, 0),
    (128, 127, 0, 127),
    (128, 128, 1, 4),
    (128, 251, 1, 127),
    (128, 252, 2, 8),
    (128, 371, 2, 127),
    (128, 372, 3, 4),
    (128, 495, 3, 127),
    (128, 496, 4, 12),
    (128, 611, 4, 127),
    (128, 612, 5, 4),
    (128, 980, 8, 16),
    // Block size 256.
    (256, 0, 0, 0),
    (256, 255, 0, 255),
    (256, 256, 1, 4),
    (256, 507, 1, 255),
    (256, 508, 2, 8),
    (256, 755, 2, 255),
    (256, 756, 3, 4),
    (256, 1007, 3, 255),
    (256, 1008, 4, 12),
    (256, 1251, 4, 255),
    (256, 1252, 5, 4),
    (256, 2004, 8, 16),
    // Block size 512. Capacities 512, 508, 504, 508, 500; boundaries
    // 0, 512, 1020, 1524, 2032.
    (512, 0, 0, 0),
    (512, 511, 0, 511),
    (512, 512, 1, 4),
    (512, 1019, 1, 511),
    (512, 1020, 2, 8),
    (512, 1523, 2, 511),
    (512, 1524, 3, 4),
    (512, 2031, 3, 511),
    (512, 2032, 4, 12),
];

/// Hand computed `(block_size, size, block_count)` triples, read off
/// the same cumulative boundaries as [`PINNED_VECTORS`]. A size that
/// exactly fills a block and a size one byte past it both appear,
/// because that pair is where a fencepost error in the rounding shows
/// up.
const PINNED_BLOCK_COUNTS: &[(u32, u32, u32)] = &[
    (256, 0, 0),
    (256, 1, 1),
    (256, 256, 1),
    (256, 257, 2),
    (256, 508, 2),
    (256, 509, 3),
    (256, 756, 3),
    (256, 757, 4),
    (256, 2004, 8),
    (256, 2005, 9),
    (128, 128, 1),
    (128, 129, 2),
    (128, 252, 2),
    (128, 253, 3),
    (128, 980, 8),
    (128, 981, 9),
    (512, 512, 1),
    (512, 513, 2),
    (512, 1020, 2),
    (512, 1021, 3),
];

#[test]
fn pinned_layout_vectors() {
    for &(block_size, offset, expect_index, expect_off) in PINNED_VECTORS {
        assert_eq!(
            block_index_at_offset(offset, block_size),
            (expect_index, expect_off),
            "block_index_at_offset({offset}, {block_size}) disagrees with the hand computed layout"
        );
        // The oracle must reproduce the same hand computed numbers,
        // otherwise the oracle itself has drifted and the property
        // tests below prove nothing.
        assert_eq!(
            oracle_locate(offset, block_size),
            (expect_index, expect_off),
            "the independent oracle disagrees with the hand computed layout at offset {offset}"
        );
    }
}

#[test]
fn pinned_block_counts() {
    for &(block_size, size, expect) in PINNED_BLOCK_COUNTS {
        assert_eq!(
            block_count(size, block_size),
            expect,
            "block_count({size}, {block_size}) disagrees with the hand computed count"
        );
        assert_eq!(
            oracle_block_count(size, block_size),
            expect,
            "the independent oracle disagrees with the hand computed count at size {size}"
        );
    }
}

#[test]
fn pinned_pointer_counts() {
    // Hand computed from the format rule, block 0 through block 16.
    const EXPECTED: [u32; 17] = [0, 1, 2, 1, 3, 1, 2, 1, 4, 1, 2, 1, 3, 1, 2, 1, 5];
    for (index, &expect) in EXPECTED.iter().enumerate() {
        let index = index as u32;
        assert_eq!(
            skip_pointers_in_block(index),
            expect,
            "skip_pointers_in_block({index}) disagrees with the hand computed table"
        );
        assert_eq!(
            oracle_pointer_count(index),
            expect,
            "the independent oracle disagrees with the hand computed table at block {index}"
        );
    }
}

proptest! {
    /// The pointer count the crate reports matches the count the format
    /// rule dictates, for every block index.
    ///
    /// This is the property the old suite could not have: the previous
    /// oracle called [`skip_pointers_in_block`] to build its own answer.
    #[test]
    fn skip_pointers_match_format_rule(index in block_index_strategy()) {
        prop_assert_eq!(skip_pointers_in_block(index), oracle_pointer_count(index));
    }

    /// The per block capacity the crate reports matches `block_size`
    /// minus four bytes for each pointer the format rule places in that
    /// block.
    #[test]
    fn capacity_matches_format_rule(
        index in block_index_strategy(),
        bs_log2 in 7u32..=12, // 128..=4096
    ) {
        let bs = 1u32 << bs_log2;
        prop_assert_eq!(content_bytes_in_block(index, bs), oracle_capacity(index, bs));
    }

    /// `block_index_at_offset` agrees with an independent layout walk
    /// for every offset within a reasonable file size range, across a
    /// spread of plausible block sizes.
    #[test]
    fn block_index_matches_independent_walk(
        offset in offset_strategy(),
        bs_log2 in 7u32..=12,
    ) {
        let bs = 1u32 << bs_log2;
        prop_assert_eq!(block_index_at_offset(offset, bs), oracle_locate(offset, bs));
    }

    /// `block_count` agrees with an independent layout walk, rather
    /// than merely agreeing with `block_index_at_offset`.
    #[test]
    fn block_count_matches_independent_walk(
        size in offset_strategy(),
        bs_log2 in 7u32..=12,
    ) {
        let bs = 1u32 << bs_log2;
        prop_assert_eq!(block_count(size, bs), oracle_block_count(size, bs));
    }

    /// Every byte of a file is reachable, and consecutive offsets never
    /// step backward through the chain. A structural invariant of the
    /// layout that the oracle comparison alone does not state.
    #[test]
    fn locations_advance_monotonically(
        offset in offset_strategy(),
        bs_log2 in 7u32..=12,
    ) {
        let bs = 1u32 << bs_log2;
        let (index_a, _) = block_index_at_offset(offset, bs);
        let (index_b, off_b) = block_index_at_offset(offset + 1, bs);
        prop_assert!(index_b == index_a || index_b == index_a + 1);
        prop_assert!(off_b < bs, "offset within block must stay inside the block");
    }

    /// `skip_pointers_in_block` is monotonically non decreasing along
    /// powers of two: block `2^k` always has more pointers than block
    /// `2^(k-1)`.
    #[test]
    fn skip_pointers_monotone_at_powers_of_two(k in 1u32..=20) {
        let prev = skip_pointers_in_block(1u32 << (k - 1));
        let curr = skip_pointers_in_block(1u32 << k);
        prop_assert!(curr > prev);
    }
}
