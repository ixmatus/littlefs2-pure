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

use littlefs2_pure::ctz::{read_ctz, CtzStruct};
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
