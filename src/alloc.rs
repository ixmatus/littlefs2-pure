//! Block allocator.
//!
//! Identifies in-use blocks by walking the filesystem from the root
//! pair, then surfaces unused blocks to callers. Used by [`crate::Fs`]
//! for CTZ file writes and `mkdir`.
//!
//! # Algorithm
//!
//! Maintains a bitmap of in-use blocks. Walks pairs in a fixed-size
//! queue (BFS), marking each pair's two blocks and traversing
//! references to other pairs ([`crate::TagType::DirStruct`],
//! [`crate::TagType::HardTail`], [`crate::TagType::SoftTail`]) and to
//! CTZ chains ([`crate::TagType::CtzStruct`], walking the chain
//! backward from the head and marking each block).
//!
//! Cycles are detected via the bitmap itself: a pair whose blocks
//! are already marked is not enqueued again. The CTZ chain walk uses
//! [`crate::ctz::skip_pointers_in_block`] and the
//! `count = 2 - (index & 1)` rule from `lfs_ctz_traverse` so it stays
//! O(N) in the chain length, with two skip-pointer reads per even
//! index step.
//!
//! # Bounds
//!
//! - `MAX_TRACKED_BLOCKS = 4096`: the bitmap caps device size at
//!   roughly 16 MiB at 4 KiB blocks, or 1 MiB at 256-byte blocks.
//!   Devices beyond that need a streaming allocator (forward-looking
//!   enhancement; not currently exercised).
//! - `MAX_QUEUED_PAIRS = 32`: caps directory tree depth and HardTail
//!   chain length the scan can traverse in one pass. A directory tree
//!   with more than 32 leaves visited concurrently returns
//!   `Error::OutOfRange`.

use crate::block::{BlockAddress, BlockPair};
use crate::ctz;
use crate::error::Error;
use crate::meta::MetadataPair;
use crate::storage::Storage;
use crate::tag::TagType;

/// Maximum number of blocks the scan-based allocator can track.
pub const MAX_TRACKED_BLOCKS: usize = 4096;

/// Maximum number of pairs the BFS queue holds at one time.
pub const MAX_QUEUED_PAIRS: usize = 32;

/// Bitmap of in-use blocks, indexed by block address.
///
/// Set bits indicate blocks that are reachable from the filesystem
/// root and therefore must not be reused. Unset bits are free for
/// allocation.
#[derive(Clone, Copy)]
pub struct Bitmap {
    bits: [u8; MAX_TRACKED_BLOCKS / 8],
}

impl Bitmap {
    /// Empty bitmap.
    pub const EMPTY: Self = Self { bits: [0u8; MAX_TRACKED_BLOCKS / 8] };

    /// Mark `block` as in-use. Returns `Error::OutOfRange` if `block`
    /// is beyond [`MAX_TRACKED_BLOCKS`].
    pub fn set(&mut self, block: u32) -> Result<(), Error> {
        let idx = block as usize;
        if idx >= MAX_TRACKED_BLOCKS {
            return Err(Error::OutOfRange);
        }
        self.bits[idx / 8] |= 1 << (idx % 8);
        Ok(())
    }

    /// `true` if `block` is marked in-use. `false` if unset or beyond
    /// the bitmap cap (out-of-range blocks can never be free, so
    /// callers iterating in `0..BLOCK_COUNT` get the right answer).
    pub fn is_set(&self, block: u32) -> bool {
        let idx = block as usize;
        if idx >= MAX_TRACKED_BLOCKS {
            return true;
        }
        (self.bits[idx / 8] & (1 << (idx % 8))) != 0
    }
}

/// Walk the filesystem from `root` and mark every reachable block in
/// `used`. Visits each metadata pair once; follows DirStruct and Tail
/// references into other pairs; walks each CTZ chain.
///
/// `buf_a` and `buf_b` are scratch buffers of `S::BLOCK_SIZE` each;
/// they are reused for every pair read.
pub fn scan_used_blocks<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    used: &mut Bitmap,
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<(), Error> {
    if buf_a.len() < S::BLOCK_SIZE || buf_b.len() < S::BLOCK_SIZE {
        return Err(Error::GeometryMismatch);
    }
    let buf_a = &mut buf_a[..S::BLOCK_SIZE];
    let buf_b = &mut buf_b[..S::BLOCK_SIZE];

    let mut queue: [BlockPair; MAX_QUEUED_PAIRS] =
        [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE); MAX_QUEUED_PAIRS];
    queue[0] = root;
    let mut tail = 1usize;
    let mut head = 0usize;

    while head < tail {
        let pair = queue[head];
        head += 1;

        // Mark this pair's blocks.
        used.set(pair.a.as_u32())?;
        used.set(pair.b.as_u32())?;

        // Read both blocks and parse.
        storage.read(pair.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        storage.read(pair.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let parsed = MetadataPair::parse(pair.a, &*buf_a, pair.b, &*buf_b)?;

        // Walk tags, enqueueing referenced pairs and marking CTZ blocks.
        // We need to drop the iterator borrow before we can read storage
        // for CTZ walks, so collect references first.
        let mut child_pairs: [BlockPair; MAX_QUEUED_PAIRS] =
            [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE); MAX_QUEUED_PAIRS];
        let mut child_count = 0usize;
        let mut ctz_chains: [(u32, u32); MAX_QUEUED_PAIRS] = [(0, 0); MAX_QUEUED_PAIRS];
        let mut ctz_count = 0usize;

        for entry in parsed.reader.iter_tags() {
            match entry.tag.tag_type() {
                TagType::DirStruct | TagType::HardTail | TagType::SoftTail
                    if entry.body.len() == 8 =>
                {
                    let a = u32::from_le_bytes([
                        entry.body[0],
                        entry.body[1],
                        entry.body[2],
                        entry.body[3],
                    ]);
                    let b = u32::from_le_bytes([
                        entry.body[4],
                        entry.body[5],
                        entry.body[6],
                        entry.body[7],
                    ]);
                    // Skip pairs already marked (visited or about-to-be).
                    if !used.is_set(a) || !used.is_set(b) {
                        if child_count >= MAX_QUEUED_PAIRS {
                            return Err(Error::OutOfRange);
                        }
                        child_pairs[child_count] =
                            BlockPair::new(BlockAddress::new(a), BlockAddress::new(b));
                        child_count += 1;
                    }
                }
                TagType::CtzStruct if entry.body.len() == 8 => {
                    let head_block = u32::from_le_bytes([
                        entry.body[0],
                        entry.body[1],
                        entry.body[2],
                        entry.body[3],
                    ]);
                    let size = u32::from_le_bytes([
                        entry.body[4],
                        entry.body[5],
                        entry.body[6],
                        entry.body[7],
                    ]);
                    if ctz_count >= MAX_QUEUED_PAIRS {
                        return Err(Error::OutOfRange);
                    }
                    ctz_chains[ctz_count] = (head_block, size);
                    ctz_count += 1;
                }
                _ => {}
            }
        }
        let _ = parsed; // pair borrow ends here; iter_tags is finished

        // Walk CTZ chains (storage reads, so the pair borrow must be done).
        for &(head_block, size) in &ctz_chains[..ctz_count] {
            walk_ctz_chain(storage, head_block, size, used)?;
        }

        // Enqueue child pairs for BFS.
        for child in &child_pairs[..child_count] {
            if tail >= MAX_QUEUED_PAIRS {
                return Err(Error::OutOfRange);
            }
            queue[tail] = *child;
            tail += 1;
        }
    }

    Ok(())
}

/// Walk a CTZ chain from `head_block` (the chain's last physical
/// block), marking every block as used. Uses a tiny stack buffer for
/// the skip pointer reads; does not need a full block buffer.
fn walk_ctz_chain<S: Storage>(
    storage: &mut S,
    head_block: u32,
    size: u32,
    used: &mut Bitmap,
) -> Result<(), Error> {
    if size == 0 {
        return Ok(());
    }
    let bs = S::BLOCK_SIZE as u32;
    let total = ctz::block_count(size, bs);
    let mut index = total - 1;
    let mut head = head_block;
    let mut sp_buf = [0u8; 8];

    loop {
        used.set(head)?;
        if index == 0 {
            break;
        }
        let count = 2 - (index & 1);
        storage.read(head, 0, &mut sp_buf[..4 * count as usize]).map_err(|_| Error::Io)?;
        let ptr0 = u32::from_le_bytes([sp_buf[0], sp_buf[1], sp_buf[2], sp_buf[3]]);
        if count == 2 {
            let ptr1 = u32::from_le_bytes([sp_buf[4], sp_buf[5], sp_buf[6], sp_buf[7]]);
            used.set(ptr0)?;
            head = ptr1;
            index -= 2;
        } else {
            head = ptr0;
            index -= 1;
        }
    }
    Ok(())
}

/// Allocate `out.len()` unused blocks.
///
/// Scans the filesystem to determine in-use blocks, then fills `out`
/// with the first `out.len()` unused block addresses (in ascending
/// order). Returns [`Error::OutOfRange`] if the device does not have
/// enough free blocks.
///
/// The allocated blocks are not erased; callers issuing programs to
/// them must erase first.
pub fn alloc_blocks<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    out: &mut [BlockAddress],
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<(), Error> {
    let mut used = Bitmap::EMPTY;
    scan_used_blocks(storage, root, &mut used, buf_a, buf_b)?;

    let mut filled = 0usize;
    for b in 0..S::BLOCK_COUNT {
        if filled >= out.len() {
            return Ok(());
        }
        if !used.is_set(b) {
            out[filled] = BlockAddress::new(b);
            filled += 1;
            // Mark to prevent re-allocation within this call.
            used.set(b)?;
        }
    }
    if filled < out.len() {
        return Err(Error::OutOfRange);
    }
    Ok(())
}

/// Walk the filesystem from `root` and mark every reachable block,
/// using a single block-sized buffer for the scan.
///
/// The two-buffer [`scan_used_blocks`] is sharper because
/// [`MetadataPair::parse`] needs both blocks at once to pick the active
/// one. The single-buffer variant trades sharpness for the freedom to
/// run a scan while the second buffer holds something else (notably
/// the freshly compacted bytes during a wear-levelling relocation):
/// it reads each block of each pair sequentially, walks whatever
/// commits parse there, and unions every referenced pair / CTZ chain.
/// Blocks reachable from EITHER half of a pair are marked, so the
/// result is a safe over-approximation of "in use".
pub fn scan_used_with_single_buf<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    used: &mut Bitmap,
    buf: &mut [u8],
) -> Result<(), Error> {
    if buf.len() < S::BLOCK_SIZE {
        return Err(Error::GeometryMismatch);
    }
    let buf = &mut buf[..S::BLOCK_SIZE];

    let mut queue: [BlockPair; MAX_QUEUED_PAIRS] =
        [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE); MAX_QUEUED_PAIRS];
    queue[0] = root;
    let mut tail = 1usize;
    let mut head = 0usize;

    while head < tail {
        let pair = queue[head];
        head += 1;

        used.set(pair.a.as_u32())?;
        used.set(pair.b.as_u32())?;

        let mut child_pairs: [BlockPair; MAX_QUEUED_PAIRS] =
            [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE); MAX_QUEUED_PAIRS];
        let mut child_count = 0usize;
        let mut ctz_chains: [(u32, u32); MAX_QUEUED_PAIRS] = [(0, 0); MAX_QUEUED_PAIRS];
        let mut ctz_count = 0usize;

        for &block in &[pair.a.as_u32(), pair.b.as_u32()] {
            storage.read(block, 0, buf).map_err(|_| Error::Io)?;
            let Ok(reader) = crate::meta::MetadataReader::new(&*buf) else {
                continue;
            };
            if !reader.has_commits() {
                continue;
            }
            for entry in reader.iter_tags() {
                match entry.tag.tag_type() {
                    TagType::DirStruct | TagType::HardTail | TagType::SoftTail
                        if entry.body.len() == 8 =>
                    {
                        let a = u32::from_le_bytes([
                            entry.body[0],
                            entry.body[1],
                            entry.body[2],
                            entry.body[3],
                        ]);
                        let b = u32::from_le_bytes([
                            entry.body[4],
                            entry.body[5],
                            entry.body[6],
                            entry.body[7],
                        ]);
                        if !used.is_set(a) || !used.is_set(b) {
                            let child = BlockPair::new(BlockAddress::new(a), BlockAddress::new(b));
                            // Dedup against the in-flight enqueue list.
                            if !child_pairs[..child_count].contains(&child) {
                                if child_count >= MAX_QUEUED_PAIRS {
                                    return Err(Error::OutOfRange);
                                }
                                child_pairs[child_count] = child;
                                child_count += 1;
                            }
                        }
                    }
                    TagType::CtzStruct if entry.body.len() == 8 => {
                        let head_block = u32::from_le_bytes([
                            entry.body[0],
                            entry.body[1],
                            entry.body[2],
                            entry.body[3],
                        ]);
                        let size = u32::from_le_bytes([
                            entry.body[4],
                            entry.body[5],
                            entry.body[6],
                            entry.body[7],
                        ]);
                        if ctz_count >= MAX_QUEUED_PAIRS {
                            return Err(Error::OutOfRange);
                        }
                        ctz_chains[ctz_count] = (head_block, size);
                        ctz_count += 1;
                    }
                    _ => {}
                }
            }
        }

        for &(head_block, size) in &ctz_chains[..ctz_count] {
            walk_ctz_chain(storage, head_block, size, used)?;
        }

        for child in &child_pairs[..child_count] {
            if tail >= MAX_QUEUED_PAIRS {
                return Err(Error::OutOfRange);
            }
            queue[tail] = *child;
            tail += 1;
        }
    }

    Ok(())
}

/// Allocate one unused block using a single-buffer scan.
///
/// Companion to [`scan_used_with_single_buf`] for the wear-levelling
/// relocation path: the compact buffer holding the new commit bytes
/// must be preserved across the scan, so only the other buffer is
/// available as scratch.
///
/// `excluded` blocks are treated as already-used even if the scan
/// thinks they are free. The relocation path passes the pair's own
/// alternate block here so the fresh allocation never collides with
/// the very block about to be orphaned.
pub fn alloc_one_block_with_single_buf<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    excluded: &[BlockAddress],
    buf: &mut [u8],
) -> Result<BlockAddress, Error> {
    let mut used = Bitmap::EMPTY;
    scan_used_with_single_buf(storage, root, &mut used, buf)?;
    for ex in excluded {
        used.set(ex.as_u32())?;
    }
    for b in 0..S::BLOCK_COUNT {
        if !used.is_set(b) {
            return Ok(BlockAddress::new(b));
        }
    }
    Err(Error::OutOfRange)
}
