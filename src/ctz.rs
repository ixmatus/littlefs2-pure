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
//! # Module surface
//!
//! - [`CtzStruct`]: decode / encode the 8-byte body (head block + size).
//! - [`block_count`]: total number of blocks in a chain holding `size`
//!   bytes.
//! - [`skip_pointers_in_block`]: `ctz(index) + 1` for `index > 0`, else
//!   `0`.
//! - [`content_bytes_in_block`]: payload bytes after the skip-pointer
//!   header.
//! - [`block_index_at_offset`]: the C reference's `lfs_ctz_index`
//!   translated to Rust, returning `(block_index, offset_within_block)`.
//! - [`read_ctz`] and [`read_ctz_at`]: walk the chain backward from the
//!   head and assemble file bytes; [`collect_chain_blocks`] is the
//!   reusable backward-walk helper used by the streaming append in
//!   [`crate::Fs::append_to_path`].

use crate::block::BlockAddress;
use crate::error::Error;
use crate::storage::Storage;

/// Reject a CTZ block address decoded from on-disk bytes that lies
/// outside the device.
///
/// CTZ skip pointers are attacker-controlled in a corrupt or
/// adversarial image (the `Storage` threat model names them
/// explicitly). The kernel already pre-checks metadata pair addresses
/// (`fs::pair_in_bounds`); this is the symmetric guard for the CTZ
/// path, classifying an out-of-range pointer as [`Error::Corrupt`]
/// (the on-disk structure is malformed) rather than letting it reach
/// `Storage::read` and surface as the indistinguishable [`Error::Io`],
/// or, with a non-conforming adapter, as memory unsafety. Defense in
/// depth: a conforming `Storage` impl also rejects the access, but the
/// kernel does not depend on that for a correct error classification.
#[inline]
fn require_in_bounds<S: Storage>(b: BlockAddress) -> Result<BlockAddress, Error> {
    if b.as_u32() < S::BLOCK_COUNT {
        Ok(b)
    } else {
        Err(Error::Corrupt)
    }
}

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
/// a read at `returned_offset` (through
/// [`crate::storage::read_range`], which puts it on the `READ_SIZE`
/// grid) lands on the right byte.
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

/// Upper bound on the number of blocks in a single CTZ file this read
/// path supports.
///
/// At 4 KiB blocks this caps file size at about 1 MiB; at 256 byte
/// blocks (the test geometry) it caps at about 64 KiB. The cap exists
/// so the block address scratch buffer fits on the stack without
/// requiring `alloc`. Larger files need a streaming read path or a
/// caller supplied address buffer; both are future enhancements.
pub const MAX_CTZ_BLOCKS: usize = 256;

/// Read a CTZ backed file's content into `out`.
///
/// `ctz` describes the file's layout (head block and total size).
/// `scratch` must be at least [`S::BLOCK_SIZE`](Storage::BLOCK_SIZE)
/// bytes; it is the `READ_SIZE` aligned staging window for the skip
/// pointer headers and for any content span whose ends do not fall on
/// the read grid (see [`crate::storage::read_range`]). A span already
/// on the grid, which is what a full block of content is whenever the
/// block carries no pointer header, is read straight into `out`. Its
/// contents after the call are unspecified.
///
/// The function reads `min(out.len(), ctz.size)` bytes and returns the
/// count actually read.
///
/// Algorithm: walks backward from `ctz.head_block` using the same
/// `count = 2 - (index & 1)` rule as `lfs_ctz_traverse`
/// (`lfs.c:2990`), collecting the chain's block addresses into a
/// stack array. Then reads each block's content portion (skipping
/// the `4 * skip_pointers_in_block(i)` byte header) in forward order
/// into `out`. The chain is bounded by [`MAX_CTZ_BLOCKS`]; oversized
/// files return [`Error::OutOfRange`].
pub fn read_ctz<S: Storage>(
    storage: &mut S,
    ctz: &CtzStruct,
    out: &mut [u8],
    scratch: &mut [u8],
) -> Result<usize, Error> {
    read_ctz_at(storage, ctz, 0, out, scratch)
}

/// Largest `READ_SIZE` the window free convenience wrappers
/// ([`collect_chain_blocks`] and [`seek_block`]) support.
///
/// A skip pointer header is 4 or 8 bytes, but the [`Storage`] contract
/// admits no read smaller than `READ_SIZE`, so fetching one needs a
/// staging window of at least `READ_SIZE` bytes. The wrappers take no
/// buffer from the caller, so they stack one of this fixed size and
/// report [`Error::GeometryMismatch`] on a device whose `READ_SIZE`
/// exceeds it. The value matches [`crate::nor::MAX_PROG_SIZE`], and
/// `PROG_SIZE` is at least `READ_SIZE` on any conforming device, so the
/// two ceilings agree.
///
/// The buffered forms ([`collect_chain_blocks_buffered`] and
/// [`seek_block_buffered`]) take the window from the caller and carry
/// no such ceiling; so does the whole file read path
/// ([`read_ctz`] and [`read_ctz_at`]), which stages through the block
/// sized `scratch` it already requires. The kernel uses the buffered
/// forms everywhere.
pub const MAX_UNBUFFERED_READ_SIZE: usize = 512;

/// Walk a CTZ chain backward from `head` (the physical address of the
/// chain's last block) and fill `out[0..total_blocks]` with the chain's
/// block addresses indexed by chain position.
///
/// Reads only skip pointer headers, one aligned read per visited
/// block, so the caller does not need a per block scratch buffer. The
/// traversal uses the same `count = 2 - (index & 1)` rule as
/// `lfs_ctz_traverse` (`lfs.c:2990`).
///
/// This form stages the header through a stacked
/// [`MAX_UNBUFFERED_READ_SIZE`] byte window; use
/// [`collect_chain_blocks_buffered`] to supply the window (and to lift
/// the `READ_SIZE` ceiling) when the caller already holds a block
/// buffer.
///
/// # Errors
///
/// - [`Error::OutOfRange`] if `out.len() < total_blocks as usize`.
/// - [`Error::GeometryMismatch`] if `S::READ_SIZE` exceeds
///   [`MAX_UNBUFFERED_READ_SIZE`].
/// - I/O errors propagate from `storage.read`.
pub fn collect_chain_blocks<S: Storage>(
    storage: &mut S,
    head: BlockAddress,
    total_blocks: u32,
    out: &mut [BlockAddress],
) -> Result<(), Error> {
    if S::READ_SIZE > MAX_UNBUFFERED_READ_SIZE {
        return Err(Error::GeometryMismatch);
    }
    let mut window = [0u8; MAX_UNBUFFERED_READ_SIZE];
    collect_chain_blocks_buffered(storage, head, total_blocks, out, &mut window)
}

/// [`collect_chain_blocks`] with a caller supplied read window.
///
/// `window` stages the `READ_SIZE` aligned fetch of each skip pointer
/// header (see [`crate::storage::read_range`]); it must hold at least
/// `S::READ_SIZE` bytes, which any block sized buffer does. Its
/// contents after the call are unspecified.
///
/// # Errors
///
/// - [`Error::OutOfRange`] if `out.len() < total_blocks as usize`.
/// - [`Error::GeometryMismatch`] if `window` is shorter than
///   `S::READ_SIZE`.
/// - [`Error::Corrupt`] if a skip pointer addresses a block outside the
///   device.
/// - I/O errors propagate from `storage.read`.
pub fn collect_chain_blocks_buffered<S: Storage>(
    storage: &mut S,
    head: BlockAddress,
    total_blocks: u32,
    out: &mut [BlockAddress],
    window: &mut [u8],
) -> Result<(), Error> {
    if total_blocks == 0 {
        return Ok(());
    }
    if (out.len() as u32) < total_blocks {
        return Err(Error::OutOfRange);
    }
    let mut head = head;
    let mut index = total_blocks - 1;
    let mut sp_buf = [0u8; 8];
    loop {
        // Every address written to `out` is bounds-checked here, so
        // both this walk's read of `head` and the caller's later
        // forward read of `out[i]` only ever dereference an in-device
        // block; an out-of-range skip pointer is rejected as
        // `Error::Corrupt`.
        out[index as usize] = require_in_bounds::<S>(head)?;
        if index == 0 {
            break;
        }
        let count = 2 - (index & 1);
        // The header is `4 * count` bytes at offset 0, a length that is
        // not in general a `READ_SIZE` multiple, so the fetch goes
        // through the aligned window (review `lfs-8e6`, the read path
        // twin of M7).
        crate::storage::read_range(
            storage,
            head.as_u32(),
            0,
            &mut sp_buf[..4 * count as usize],
            window,
        )?;
        let ptr0 = u32::from_le_bytes([sp_buf[0], sp_buf[1], sp_buf[2], sp_buf[3]]);
        if count == 2 {
            let ptr1 = u32::from_le_bytes([sp_buf[4], sp_buf[5], sp_buf[6], sp_buf[7]]);
            out[(index - 1) as usize] = require_in_bounds::<S>(BlockAddress::new(ptr0))?;
            head = BlockAddress::new(ptr1);
            index -= 2;
        } else {
            head = BlockAddress::new(ptr0);
            index -= 1;
        }
    }
    Ok(())
}

/// Seek from a chain head (the physical address `head` of the block at
/// logical index `from_index`) to the physical address of the block at a
/// smaller-or-equal logical `target_index`, following skip pointers.
///
/// Reads only skip-pointer headers, in `O(log(from_index - target_index))`
/// reads, so a caller that needs one specific block's address need not
/// walk the whole chain with [`collect_chain_blocks`].
///
/// This is the descent half of the CTZ skip-list: block `i` stores a
/// pointer to block `i - 2^k` at header offset `4*k`, for each
/// `k in 0..=ctz(i)`. From the current block at index `i > target`, the
/// walk follows the largest available jump `2^k` that does not undershoot
/// `target` (the largest `k <= ctz(i)` with `2^k <= i - target`); each
/// hop strictly decreases `i` without passing `target`, so it lands
/// exactly on `target` after `O(log)` hops. Every dereferenced address is
/// bounds-checked, so a corrupt or out-of-range pointer is rejected as
/// [`Error::Corrupt`].
///
/// This form stages each pointer fetch through a stacked
/// [`MAX_UNBUFFERED_READ_SIZE`] byte window; use
/// [`seek_block_buffered`] to supply the window (and to lift the
/// `READ_SIZE` ceiling) when the caller already holds a block buffer.
///
/// # Errors
///
/// - [`Error::OutOfRange`] if `target_index > from_index`.
/// - [`Error::Corrupt`] if a skip pointer is out of range.
/// - [`Error::GeometryMismatch`] if `S::READ_SIZE` exceeds
///   [`MAX_UNBUFFERED_READ_SIZE`].
/// - I/O errors propagate from `storage.read`.
pub fn seek_block<S: Storage>(
    storage: &mut S,
    head: BlockAddress,
    from_index: u32,
    target_index: u32,
) -> Result<BlockAddress, Error> {
    if S::READ_SIZE > MAX_UNBUFFERED_READ_SIZE {
        return Err(Error::GeometryMismatch);
    }
    let mut window = [0u8; MAX_UNBUFFERED_READ_SIZE];
    seek_block_buffered(storage, head, from_index, target_index, &mut window)
}

/// [`seek_block`] with a caller supplied read window.
///
/// `window` stages the `READ_SIZE` aligned fetch of each skip pointer
/// (see [`crate::storage::read_range`]); it must hold at least
/// `S::READ_SIZE` bytes, which any block sized buffer does. Its
/// contents after the call are unspecified.
///
/// # Errors
///
/// - [`Error::OutOfRange`] if `target_index > from_index`.
/// - [`Error::Corrupt`] if a skip pointer is out of range.
/// - [`Error::GeometryMismatch`] if `window` is shorter than
///   `S::READ_SIZE`.
/// - I/O errors propagate from `storage.read`.
pub fn seek_block_buffered<S: Storage>(
    storage: &mut S,
    head: BlockAddress,
    from_index: u32,
    target_index: u32,
    window: &mut [u8],
) -> Result<BlockAddress, Error> {
    if target_index > from_index {
        return Err(Error::OutOfRange);
    }
    let mut cur = require_in_bounds::<S>(head)?;
    let mut idx = from_index;
    let mut sp = [0u8; 4];
    while idx > target_index {
        let gap = idx - target_index;
        // Available jumps at block `idx` are `2^k` for `k in 0..=ctz(idx)`
        // (idx > 0 inside this loop). Choose the largest `k` bounded by
        // both the gap and the block's pointer count.
        let max_k = idx.trailing_zeros();
        let gap_k = gap.ilog2(); // floor(log2(gap)); gap >= 1
        let k = gap_k.min(max_k);
        // The pointer is 4 bytes at offset `4*k`: neither the offset nor
        // the length sits on the `READ_SIZE` grid in general, so the
        // fetch goes through the aligned window (review `lfs-8e6`).
        crate::storage::read_range(storage, cur.as_u32(), 4 * k, &mut sp, window)?;
        let ptr = u32::from_le_bytes([sp[0], sp[1], sp[2], sp[3]]);
        cur = require_in_bounds::<S>(BlockAddress::new(ptr))?;
        idx -= 1 << k;
    }
    Ok(cur)
}

/// Read up to `out.len()` bytes of a CTZ file starting at byte offset
/// `start_off`. Returns the number of bytes copied (may be less than
/// `out.len()` if `start_off + out.len() > ctz.size`).
///
/// Like [`read_ctz`] but seek-aware. The implementation still walks
/// the whole chain backward to collect block addresses (same
/// `MAX_CTZ_BLOCKS` cap); only the read step is offset-aware. A
/// log-time seek (using skip pointers from the head, à la
/// `lfs_ctz_find`) is a future optimization.
///
/// Every device read this issues sits on the `READ_SIZE` grid, staged
/// through `scratch` where the on disk extent does not (review
/// `lfs-8e6`).
pub fn read_ctz_at<S: Storage>(
    storage: &mut S,
    ctz: &CtzStruct,
    start_off: u32,
    out: &mut [u8],
    scratch: &mut [u8],
) -> Result<usize, Error> {
    if scratch.len() < S::BLOCK_SIZE {
        return Err(Error::GeometryMismatch);
    }
    if ctz.size == 0 || out.is_empty() || start_off >= ctz.size {
        return Ok(0);
    }

    let bs = S::BLOCK_SIZE as u32;
    let total_blocks = block_count(ctz.size, bs);
    if total_blocks as usize > MAX_CTZ_BLOCKS {
        return Err(Error::OutOfRange);
    }

    let mut blocks = [BlockAddress::NONE; MAX_CTZ_BLOCKS];
    collect_chain_blocks_buffered(storage, ctz.head_block, total_blocks, &mut blocks, scratch)?;

    // Forward read: pull each block's content portion into `out`,
    // skipping logical bytes before `start_off`.
    let end_off = ctz.size.min(start_off.saturating_add(out.len() as u32));
    let target_bytes = (end_off - start_off) as usize;
    let mut out_off = 0usize;
    let mut logical_off = 0u32;
    for i in 0..total_blocks {
        if out_off >= target_bytes {
            break;
        }
        let header = 4 * skip_pointers_in_block(i) as usize;
        let block_content_max = (bs as usize) - header;
        let block_logical_start = logical_off;
        let block_logical_end = logical_off + block_content_max as u32;
        // Skip blocks entirely before start_off.
        if block_logical_end <= start_off {
            logical_off = block_logical_end;
            continue;
        }
        // Determine the slice of this block's content to copy.
        let skip_in_block = (start_off.saturating_sub(block_logical_start)) as usize;
        let take = (block_content_max - skip_in_block).min(target_bytes - out_off);
        if take == 0 {
            logical_off = block_logical_end;
            continue;
        }
        // The content span starts just past the skip pointer header and
        // runs for exactly the bytes the caller wants, so neither end
        // need sit on the `READ_SIZE` grid; `read_range` stages the
        // ragged case through `scratch` (review `lfs-8e6`).
        crate::storage::read_range(
            storage,
            blocks[i as usize].as_u32(),
            (header + skip_in_block) as u32,
            &mut out[out_off..out_off + take],
            scratch,
        )?;
        out_off += take;
        logical_off = block_logical_end;
    }
    Ok(out_off)
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
