//! Property tests for the CTZ skip list geometry math.
//!
//! Cross checks the production [`block_index_at_offset`] against a brute
//! force reimplementation that walks blocks one at a time, summing the
//! content capacity until the requested offset falls within a block.
//! The two must agree at every offset for every block size.

use littlefs2_pure::ctz::{
    block_count, block_index_at_offset, content_bytes_in_block, skip_pointers_in_block,
};
use proptest::prelude::*;

mod common;

/// Walk blocks from index `0` summing their content capacity until the
/// cumulative total exceeds `offset`. Returns `(index,
/// absolute_offset_within_block)`. The absolute offset accounts for
/// the skip pointer header at the start of each block, matching the
/// semantics of [`block_index_at_offset`].
fn brute_force(offset: u32, block_size: u32) -> (u32, u32) {
    let mut consumed = 0u32;
    let mut idx = 0u32;
    loop {
        let cap = content_bytes_in_block(idx, block_size);
        if offset < consumed + cap {
            let content_off = offset - consumed;
            let header = 4 * skip_pointers_in_block(idx);
            return (idx, header + content_off);
        }
        consumed = consumed.saturating_add(cap);
        idx += 1;
        // Sanity bound to avoid runaway loops on insane inputs.
        assert!(idx <= 1_000_000, "brute_force exceeded 1M iterations");
    }
}

proptest! {
    /// `block_index_at_offset` agrees with the brute force walk for
    /// every offset within a reasonable file size range, across a
    /// spread of plausible block sizes.
    #[test]
    fn block_index_matches_brute_force(
        offset in 0u32..=200_000,
        bs_log2 in 7u32..=12, // 128..=4096
    ) {
        let bs = 1u32 << bs_log2;
        let (i_fast, off_fast) = block_index_at_offset(offset, bs);
        let (i_brute, off_brute) = brute_force(offset, bs);
        prop_assert_eq!((i_fast, off_fast), (i_brute, off_brute));
    }

    /// `block_count(size)` equals the index of the block containing
    /// `size - 1` plus one, for non zero sizes.
    #[test]
    fn block_count_consistency(
        size in 1u32..=200_000,
        bs_log2 in 7u32..=12,
    ) {
        let bs = 1u32 << bs_log2;
        let (last_idx, _) = block_index_at_offset(size - 1, bs);
        prop_assert_eq!(block_count(size, bs), last_idx + 1);
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
