//! R3a regression: cycle-safe directory enumeration (Brent's, no
//! arbitrary cap) on the `list_pair_chain` path.
//!
//! Source: `docs/reviews/2026-05-15-six-agent-correctness-review.md`
//! High finding #3, ADR-0009.
//!
//! `list_pair_chain` previously walked at most `MAX_DIR_CHAIN = 32`
//! pairs, emitting each pair's entries inside the loop and then
//! returning `OutOfRange`, with no cycle detection. A cyclic HardTail
//! therefore spammed up to 32x duplicate entries before erroring. It
//! now uses the same Brent's walker as `Fs::resolve`/`find_dir_pair`:
//! a cyclic chain is rejected with `Error::Corrupt` and there is no
//! arbitrary length cap at this layer.
//!
//! The end-to-end reachable-pair-set limit is a separate, documented
//! concern: `Fs::mount`'s `accumulate_gstate` sweep is bounded by
//! `MAX_QUEUED_PAIRS = 32` and rejects a larger reachable set before
//! `list_pair_chain` is reached (KNOWN_ISSUES.md / ADR-0009). This
//! test therefore pins cycle-correctness, not >32-pair enumeration.

use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, Superblock, DISK_VERSION, MAGIC};

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

#[test]
fn list_root_self_cycle_is_corrupt_not_dup_spam() {
    // Root pair (0,1): superblock + one file + a HardTail pointing at
    // the root pair itself. Mount succeeds (accumulate_gstate dedupes
    // the self-reference); list_root walks list_pair_chain, which must
    // detect the cycle and return Error::Corrupt instead of emitting
    // the file entry unboundedly / 32x then OutOfRange.
    let mut storage = MemStorage::new();
    let sb_bytes = well_formed_sb().to_bytes();
    let mut tail = [0u8; 8];
    tail[0..4].copy_from_slice(&0u32.to_le_bytes());
    tail[4..8].copy_from_slice(&1u32.to_le_bytes());

    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::Superblock, 0, 8), MAGIC).unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    builder.tag(Tag::new(true, TagType::RegularFile, 1, 5), b"a.txt").unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 1, 2), b"hi").unwrap();
    builder.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail).unwrap();
    builder.commit(0).unwrap();
    storage.write_block(0, &builder.finish());

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("mount is cycle-safe");

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut count = 0usize;
    let res = fs.list_root(
        |_e| {
            count += 1;
            // Guard the test itself: a regression that re-broke the
            // cycle detection must fail fast here, not hang the suite.
            assert!(count < 1000, "list_root emitted unboundedly: cycle not detected");
        },
        &mut a,
        &mut b,
    );
    assert_eq!(res.unwrap_err(), Error::Corrupt, "cyclic chain must be Corrupt");
}
