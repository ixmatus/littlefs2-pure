//! CTZ skip list layout for non inline files.
//!
//! Files larger than the inline threshold are stored as a chain of blocks
//! linked by a skip list. Each block in the chain (except possibly block
//! `0`) reserves a header of one or more little endian `u32` "skip
//! pointers" at offset `0`; the remaining bytes hold file content.
//!
//! The number of skip pointers in block index `N` (for `N > 0`) is
//! `ctz(N) + 1`, where `ctz` is the count of trailing zero bits. Block
//! `0` has zero pointers and is entirely content. The `k`-th pointer
//! (at byte offset `4*k`) of block `N` addresses block `N - 2^k`. This
//! is the same trick standard CTZ skip lists use: from any block,
//! larger skips are available without extra storage cost on the small
//! blocks.
//!
//! The "head" stored in a [`crate::TagType::CtzStruct`] body is the
//! address of the **last** block in the chain (the one whose content
//! ends the file). Traversal walks backward from there toward block
//! `0`.
//!
//! # Bit accuracy
//!
//! The algorithms in this module mirror `lfs_ctz_index` and
//! `lfs_ctz_find` in the C reference (`lfs.c:2843` and `lfs.c:2856`).
//! Property tests in `tests/property_ctz.rs` cross check against an
//! independent reimplementation.
//!
//! # Scope of this commit
//!
//! Phase 1g deliverables:
//!
//! - [`CtzStruct`]: decode the 8 byte body (head block + size).
//! - [`block_count`]: total number of blocks in a chain holding `size`
//!   bytes.
//! - [`skip_pointers_in_block`]: `ctz(index) + 1` for `index > 0`, else
//!   `0`.
//! - [`content_bytes_in_block`]: payload bytes after the skip pointer
//!   header.
//! - [`block_index_at_offset`]: the C reference's `lfs_ctz_index`
//!   translated to Rust, returning `(block_index, offset_within_block)`.
//!
//! The storage-backed `read_ctz` (walk the chain from head, read each
//! block's content portion, concatenate) is Phase 1h.

use crate::block::BlockAddress;
use crate::error::Error;

/// Decoded CTZ struct, as carried in the body of a
/// [`crate::TagType::CtzStruct`] tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtzStruct {
    /// Address of the **last** block in the chain (whose content ends
    /// the file). Traversal walks backward from here.
    pub head_block: BlockAddress,
    /// Total file size in bytes.
    pub size: u32,
}

impl CtzStruct {
    /// Wire format size: two LE `u32`s = 8 bytes.
    pub const SIZE: usize = 8;

    /// Decode an 8 byte CtzStruct body.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != Self::SIZE {
            return Err(Error::OutOfRange);
        }
        Ok(Self {
            head_block: BlockAddress::new(u32::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ])),
            size: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    /// Encode to 8 little endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.head_block.as_u32().to_le_bytes());
        out[4..8].copy_from_slice(&self.size.to_le_bytes());
        out
    }
}

/// Number of skip pointers stored at the head of block `index`.
///
/// - `index == 0`: zero pointers (all `block_size` bytes are content).
/// - `index > 0`: `ctz(index) + 1` pointers, at byte offsets
///   `0, 4, ..., 4*(ctz(index))`. The `k`-th pointer addresses block
///   `index - 2^k`.
#[inline]
#[must_use]
pub fn skip_pointers_in_block(index: u32) -> u32 {
    if index == 0 {
        0
    } else {
        index.trailing_zeros() + 1
    }
}

/// Content bytes (excluding the skip pointer header) in block `index`
/// for a device with the given `block_size`.
#[inline]
#[must_use]
pub fn content_bytes_in_block(index: u32, block_size: u32) -> u32 {
    block_size - 4 * skip_pointers_in_block(index)
}

/// Total number of blocks (`0..count`) required to hold `size` bytes.
///
/// Returns `0` when `size == 0` (empty file uses no blocks).
#[must_use]
pub fn block_count(size: u32, block_size: u32) -> u32 {
    if size == 0 {
        return 0;
    }
    block_index_at_offset(size - 1, block_size).0 + 1
}

/// Translate a logical byte offset within the file to a `(block_index,
/// absolute_offset_within_block)` pair.
///
/// Mirrors `lfs_ctz_index` in the C reference (`lfs.c:2843`). The
/// returned offset is the **absolute** byte position within the block
/// as it sits on disk: the skip pointer header lives at the start of
/// the block (bytes `0..4*skip_pointers_in_block(index)`) and the
/// content follows. The offset already accounts for the header, so
/// `storage.read(block, returned_offset, ..)` lands on the right byte.
#[must_use]
pub fn block_index_at_offset(offset: u32, block_size: u32) -> (u32, u32) {
    // `b` is the maximum content per block, ignoring the per block skip
    // pointer overhead. The C code uses `b = block_size - 2*4` because
    // every block past index 0 has at least 2 skip pointers, and the
    // formula corrects for the variable count using popcount.
    let b: u32 = block_size - 2 * 4;
    let i: u32 = offset / b;
    if i == 0 {
        // Block 0 has no skip pointer header, so absolute and content
        // offsets coincide.
        return (0, offset);
    }
    // Correction step: account for the additional bytes consumed by the
    // skip pointer headers in blocks 1..i-1. The resulting offset is
    // the absolute byte position within block `i`, including its skip
    // pointer header.
    let i = (offset - 4 * (popcount(i - 1) + 2)) / b;
    let abs_off = offset - b * i - 4 * popcount(i);
    (i, abs_off)
}

#[inline]
fn popcount(a: u32) -> u32 {
    a.count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctz_struct_roundtrip() {
        let s = CtzStruct { head_block: BlockAddress::new(0xDEAD_BEEF), size: 0x1234_5678 };
        let b = s.to_bytes();
        let recovered = CtzStruct::from_bytes(&b).unwrap();
        assert_eq!(recovered, s);
    }

    #[test]
    fn ctz_struct_rejects_wrong_size() {
        assert_eq!(CtzStruct::from_bytes(&[0u8; 4]).unwrap_err(), Error::OutOfRange);
        assert_eq!(CtzStruct::from_bytes(&[0u8; 12]).unwrap_err(), Error::OutOfRange);
    }

    #[test]
    fn skip_pointers_table() {
        // ctz(N) + 1 for N > 0; 0 for N == 0.
        assert_eq!(skip_pointers_in_block(0), 0);
        assert_eq!(skip_pointers_in_block(1), 1); // ctz(0b001) = 0
        assert_eq!(skip_pointers_in_block(2), 2); // ctz(0b010) = 1
        assert_eq!(skip_pointers_in_block(3), 1); // ctz(0b011) = 0
        assert_eq!(skip_pointers_in_block(4), 3); // ctz(0b100) = 2
        assert_eq!(skip_pointers_in_block(5), 1); // ctz(0b101) = 0
        assert_eq!(skip_pointers_in_block(6), 2); // ctz(0b110) = 1
        assert_eq!(skip_pointers_in_block(7), 1); // ctz(0b111) = 0
        assert_eq!(skip_pointers_in_block(8), 4); // ctz(0b1000) = 3
        assert_eq!(skip_pointers_in_block(16), 5); // ctz(0b10000) = 4
    }

    #[test]
    fn content_bytes_uses_skip_count() {
        let bs = 4096;
        // Block 0: all content.
        assert_eq!(content_bytes_in_block(0, bs), 4096);
        // Block 1: 1 pointer = 4 bytes overhead.
        assert_eq!(content_bytes_in_block(1, bs), 4096 - 4);
        // Block 2: 2 pointers = 8 bytes overhead.
        assert_eq!(content_bytes_in_block(2, bs), 4096 - 8);
        // Block 4: 3 pointers = 12 bytes overhead.
        assert_eq!(content_bytes_in_block(4, bs), 4096 - 12);
    }

    #[test]
    fn block_index_at_zero_is_zero() {
        let (i, off) = block_index_at_offset(0, 4096);
        assert_eq!((i, off), (0, 0));
    }

    #[test]
    fn block_index_within_first_block() {
        let bs = 4096;
        // Block 0 holds bytes 0..4096 (no skip pointers).
        let (i, off) = block_index_at_offset(100, bs);
        assert_eq!((i, off), (0, 100));
        // The first b = block_size - 8 = 4088 bytes also live in block 0
        // per the C code's i = size/b check; offsets in [0, 4088) map
        // to block 0.
        let (i, off) = block_index_at_offset(4087, bs);
        assert_eq!((i, off), (0, 4087));
    }

    /// Reference implementation of block_count via brute force walking
    /// blocks until cumulative content exceeds size. Used to cross
    /// check `block_count` and `block_index_at_offset`.
    fn block_count_brute(size: u32, block_size: u32) -> u32 {
        if size == 0 {
            return 0;
        }
        let mut consumed = 0u32;
        let mut idx = 0u32;
        loop {
            let cap = content_bytes_in_block(idx, block_size);
            consumed = consumed.saturating_add(cap);
            if consumed >= size {
                return idx + 1;
            }
            idx += 1;
        }
    }

    #[test]
    fn block_index_offset_includes_skip_header() {
        // Block 4 has 3 skip pointers = 12 bytes header. File offset
        // 496 is the first byte of block 4's *content*. The absolute
        // offset within the block is 12 (header) + 0 (content) = 12.
        let bs = 128;
        let (i, abs_off) = block_index_at_offset(496, bs);
        assert_eq!((i, abs_off), (4, 12));
    }

    #[test]
    fn block_count_matches_brute_force() {
        let bs = 256;
        for size in [0, 1, 10, 100, 247, 248, 249, 500, 1000, 5000, 50000] {
            assert_eq!(
                block_count(size, bs),
                block_count_brute(size, bs),
                "size = {size}: block_count disagrees with brute force"
            );
        }
    }
}
