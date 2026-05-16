//! R1 regression: a corrupt or adversarial `HardTail` that cycles must
//! not hang the resolver.
//!
//! Source: `docs/reviews/2026-05-15-six-agent-correctness-review.md`
//! High finding #1. `Fs::resolve` and the internal `find_dir_pair`
//! chased `pair.reader.tail()` in a bare `loop {}` with no count cap and
//! no cycle check. A `HardTail` that points back into its own chain
//! (self-cycle, or an A -> B -> A two-pair cycle) made
//! resolve/exists/mkdir/rmdir/rename/get_attr issue storage reads
//! forever and never return. The C reference guards every tail walk
//! with Brent's algorithm and returns `LFS_ERR_CORRUPT`
//! (lfs.c:4407-4423).
//!
//! `Fs::mount`'s gstate sweep (`accumulate_gstate`) was already
//! bounded and cycle-safe (it dedupes the BFS queue), so mount
//! succeeds here; the hang was strictly on the resolution path. These
//! cases pin every public entry point that routes through `resolve` or
//! `find_dir_pair` to terminate with `Error::Corrupt`.
//!
//! Note on running pre-fix: without the guard these calls do not fail,
//! they hang, which would wedge `cargo test`. The fix and this guard
//! land in the same change; the assertions below pin the post-fix
//! contract and stand as the regression guard thereafter.

use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, Path, Superblock, DISK_VERSION, MAGIC};

mod common;
use common::{BlockBuilder, MemStorage};

fn well_formed_sb() -> Superblock {
    Superblock {
        version: DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    }
}

/// Two little-endian `u32` block addresses, the 8-byte `HardTail` body.
fn tail_body(a: u32, b: u32) -> [u8; 8] {
    let mut t = [0u8; 8];
    t[0..4].copy_from_slice(&a.to_le_bytes());
    t[4..8].copy_from_slice(&b.to_le_bytes());
    t
}

/// Root pair block: superblock magic + geometry + a `HardTail`
/// pointing at `(tail_a, tail_b)`. No directory entries, so every
/// lookup misses and the resolver follows the tail.
fn root_block_with_hardtail(tail_a: u32, tail_b: u32) -> alloc::vec::Vec<u8> {
    let sb_bytes = well_formed_sb().to_bytes();
    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::Superblock, 0, 8), MAGIC).unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    builder.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail_body(tail_a, tail_b)).unwrap();
    builder.commit(0).unwrap();
    builder.finish()
}

extern crate alloc;

/// A plain continuation pair carrying only a `HardTail` back to
/// `(tail_a, tail_b)` and no entries.
fn continuation_block_with_hardtail(tail_a: u32, tail_b: u32) -> alloc::vec::Vec<u8> {
    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail_body(tail_a, tail_b)).unwrap();
    builder.commit(0).unwrap();
    builder.finish()
}

/// Assert the entry points that provably route through the two
/// formerly-unbounded loops (`Fs::resolve`'s final-component `loop {}`
/// and the internal `find_dir_pair` `loop {}`) reject a corrupt cyclic
/// chain with `Error::Corrupt` instead of looping forever.
///
/// `mkdir`/`rmdir`/`rename` are deliberately not asserted here: they
/// descend via `resolve_parent` and `list_pair_chain`, which were
/// already bounded (R3 covers that path's separate truncation defect).
/// This guard is scoped to exactly the loops R1 fixes.
fn assert_resolution_path_rejects(storage: MemStorage) {
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    // Mount must still succeed: its gstate sweep is BFS-deduped and
    // cycle-safe; the hang was strictly on the resolution path.
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("mount is cycle-safe");

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // resolve, final-component loop (the `loop {}` in `Fs::resolve`):
    // single-component path, lookup misses every pair, tail is chased.
    assert_eq!(
        fs.resolve(Path::new("/missing.txt").unwrap(), &mut a, &mut b).unwrap_err(),
        Error::Corrupt,
        "resolve's final-component loop must reject a cyclic tail chain"
    );

    // resolve, intermediate component -> `find_dir_pair`'s `loop {}`:
    // a non-final component forces the directory-descent walk.
    assert_eq!(
        fs.resolve(Path::new("/sub/leaf.txt").unwrap(), &mut a, &mut b).unwrap_err(),
        Error::Corrupt,
        "find_dir_pair must reject a cyclic tail chain"
    );

    // exists wraps resolve and propagates non-NotFound errors; it must
    // surface the corrupt chain rather than masking it as Ok(false).
    assert_eq!(
        fs.exists(Path::new("/missing.txt").unwrap(), &mut a, &mut b).unwrap_err(),
        Error::Corrupt,
        "exists must surface the corrupt chain, not Ok(false)"
    );
}

#[test]
fn hardtail_self_cycle_is_rejected_not_hung() {
    // Root pair (0,1): superblock + a HardTail that points at the root
    // pair itself. The tightest possible cycle.
    let mut storage = MemStorage::new();
    storage.write_block(0, &root_block_with_hardtail(0, 1));
    assert_resolution_path_rejects(storage);
}

#[test]
fn hardtail_two_pair_cycle_is_rejected_not_hung() {
    // Root (0,1) -> tail (2,3) -> tail (0,1): an A -> B -> A cycle the
    // C reference's Brent guard catches and we previously did not.
    let mut storage = MemStorage::new();
    storage.write_block(0, &root_block_with_hardtail(2, 3));
    storage.write_block(2, &continuation_block_with_hardtail(0, 1));
    assert_resolution_path_rejects(storage);
}
