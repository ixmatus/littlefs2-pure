//! R3b regression: kernel-side CTZ skip-pointer bounds check.
//!
//! Source: `docs/reviews/2026-05-15-six-agent-correctness-review.md`
//! High finding #4.
//!
//! CTZ skip pointers decoded from disk are attacker-controlled in a
//! corrupt or adversarial image (the `Storage` threat model names them
//! explicitly). `collect_chain_blocks` passed an on-disk-decoded block
//! straight to `Storage::read` with no `< BLOCK_COUNT` guard, unlike
//! the metadata-pair path. The kernel now bounds-checks every CTZ
//! block address (`ctz::require_in_bounds`) and classifies an
//! out-of-range pointer as `Error::Corrupt` (malformed on-disk
//! structure) rather than letting it reach `Storage::read` and surface
//! as the indistinguishable `Error::Io`, or, with a non-conforming
//! adapter, as memory unsafety.
//!
//! The `MemStorage` test adapter is hardened (explicit
//! `block >= BLOCK_COUNT` reject, checked arithmetic) so these tests
//! observe the kernel's clean reject rather than an adapter panic.
//!
//! # Companion suite
//!
//! Review `lfs-0ph` audited the rest of the CTZ surface against this
//! rule and found the *allocator's* marking walk still dereferencing
//! its addresses; `tests/review_0ph_ctz_walk_bounds.rs` covers that
//! half. The entry points named below are the half that already
//! rejected, pinned here directly rather than only through `read_ctz`,
//! so the audit's finding is checkable rather than asserted.

use littlefs2_pure::ctz::{collect_chain_blocks, read_ctz, seek_block, CtzStruct};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::{BlockAddress, Error};

mod common;
use common::{build_ctz_chain, MemStorage};

extern crate alloc;

#[test]
fn ctz_head_block_out_of_range_is_corrupt() {
    // A CtzStruct whose head block address is past the device. The
    // kernel must reject it as Corrupt (malformed on-disk structure),
    // not pass it to Storage::read (which would surface as Io) and not
    // panic.
    let mut storage = MemStorage::new();
    let ctz = CtzStruct { head_block: BlockAddress::new(MemStorage::BLOCK_COUNT), size: 10 };
    let mut out = [0u8; 64];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    assert_eq!(read_ctz(&mut storage, &ctz, &mut out, &mut scratch).unwrap_err(), Error::Corrupt,);
}

#[test]
fn ctz_skip_pointer_out_of_range_is_corrupt() {
    // Build a valid two-block CTZ chain, then corrupt the head block's
    // single skip pointer to an out-of-device address. Walking the
    // chain backward must reject the decoded pointer as Corrupt.
    let mut storage = MemStorage::new();
    let data = alloc::vec![0xABu8; 300]; // > one block of content -> 2 blocks
    let ctz = build_ctz_chain(&mut storage, 2, &data);
    assert_eq!(ctz.head_block.as_u32(), 3, "two-block chain heads at base+1");

    // Overwrite the head block's first skip pointer (offset 0..4) with
    // a block number well past BLOCK_COUNT.
    let mut head_blk = alloc::vec![0u8; MemStorage::BLOCK_SIZE];
    head_blk[0..4].copy_from_slice(&9999u32.to_le_bytes());
    storage.write_block(3, &head_blk);

    let mut out = alloc::vec![0u8; 512];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    assert_eq!(read_ctz(&mut storage, &ctz, &mut out, &mut scratch).unwrap_err(), Error::Corrupt,);
}

#[test]
fn collect_chain_blocks_rejects_an_out_of_range_head_and_pointer() {
    // The backward walk `read_ctz` delegates to, exercised directly so
    // the guard is pinned at the entry point rather than only through
    // its caller.
    let mut storage = MemStorage::new();
    let data = alloc::vec![0xABu8; 300];
    let ctz = build_ctz_chain(&mut storage, 2, &data);
    let mut out = [BlockAddress::NONE; 8];

    assert_eq!(
        collect_chain_blocks(&mut storage, BlockAddress::new(9999), 2, &mut out).unwrap_err(),
        Error::Corrupt,
        "an out of range chain head is Corrupt before the first read"
    );

    let mut head_blk = alloc::vec![0u8; MemStorage::BLOCK_SIZE];
    storage.read(ctz.head_block.as_u32(), 0, &mut head_blk).unwrap();
    head_blk[0..4].copy_from_slice(&9999u32.to_le_bytes());
    storage.write_block(ctz.head_block.as_u32(), &head_blk);
    assert_eq!(
        collect_chain_blocks(&mut storage, ctz.head_block, 2, &mut out).unwrap_err(),
        Error::Corrupt,
        "an out of range skip pointer is Corrupt even though it is only recorded, not read"
    );
}

#[test]
fn seek_block_rejects_an_out_of_range_head_and_pointer() {
    // The log-time descent used by the seek-aware read and append
    // paths. It follows skip pointers just as the collect walk does and
    // owes the same classification.
    let mut storage = MemStorage::new();
    let data = alloc::vec![0xABu8; 300];
    let ctz = build_ctz_chain(&mut storage, 2, &data);

    assert_eq!(
        seek_block(&mut storage, BlockAddress::new(9999), 1, 0).unwrap_err(),
        Error::Corrupt,
        "an out of range start address is Corrupt before the first read"
    );

    let mut head_blk = alloc::vec![0u8; MemStorage::BLOCK_SIZE];
    storage.read(ctz.head_block.as_u32(), 0, &mut head_blk).unwrap();
    head_blk[0..4].copy_from_slice(&9999u32.to_le_bytes());
    storage.write_block(ctz.head_block.as_u32(), &head_blk);
    assert_eq!(
        seek_block(&mut storage, ctz.head_block, 1, 0).unwrap_err(),
        Error::Corrupt,
        "an out of range skip pointer is Corrupt at the hop that decodes it"
    );
}

#[test]
fn an_in_range_chain_still_collects_and_seeks() {
    // The counterweight: the guards must reject bad addresses without
    // rejecting good ones.
    let mut storage = MemStorage::new();
    let data = alloc::vec![0xABu8; 300];
    let ctz = build_ctz_chain(&mut storage, 2, &data);

    let mut out = [BlockAddress::NONE; 8];
    collect_chain_blocks(&mut storage, ctz.head_block, 2, &mut out).unwrap();
    assert_eq!(out[0].as_u32(), 2);
    assert_eq!(out[1].as_u32(), 3);
    assert_eq!(seek_block(&mut storage, ctz.head_block, 1, 0).unwrap().as_u32(), 2);
}
