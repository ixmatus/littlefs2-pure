//! Review `lfs-0ph`: the allocator's CTZ chain walk owes the same
//! address posture as the CTZ read paths.
//!
//! `ctz::require_in_bounds` already governs the read side: review R3b
//! routed `collect_chain_blocks_buffered` and `seek_block_buffered`
//! through it, so `read_ctz` and `read_ctz_at` reject an out of range
//! skip pointer as [`Error::Corrupt`](littlefs2_pure::Error::Corrupt).
//! `tests/review_r3b_ctz_bounds.rs` pins that half.
//!
//! `alloc::walk_ctz_chain`, the marking walk the allocator runs over
//! every live chain, did not. The H8 guard bounds the chain's total
//! block *count* against the device, but an individual skip pointer is
//! a separate claim: on an 8 block device a pointer naming block 200
//! passes the count guard, gets marked in the (4096 entry) tracking
//! bitmap, and is then handed to `Storage::read`, surfacing as
//! [`Error::Io`](littlefs2_pure::Error::Io). A pointer past the bitmap
//! surfaced instead as
//! [`Error::OutOfRange`](littlefs2_pure::Error::OutOfRange) from
//! `Bitmap::set`, a capacity verdict on what is really a malformed
//! structure. Neither names the fault; the answer is `Corrupt`.
//!
//! Both ranges are covered below, because they failed differently and a
//! fix that only moves the near one is only half a fix.
//!
//! # Why the chain is four blocks
//!
//! The walk consumes both skip pointers of a block in one read, so a
//! two or three block chain never dereferences a pointer at all: it
//! reads the head's header and lands on index 0. Four blocks is the
//! shortest chain in which the walk takes a second hop and therefore
//! dereferences a decoded pointer.

use littlefs2_pure::alloc::{scan_used_blocks, scan_used_with_single_buf, Bitmap};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, MetadataReader, Path, ROOT_BLOCK_PAIR};

mod common;
use common::{build_ctz_chain, BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// Inside the allocator's 4096 entry tracking bitmap, past the device's
/// 8 block count: `Bitmap::set` accepted it and the read then failed.
const NEAR_OOR: u32 = 200;
/// Past the tracking bitmap as well: `Bitmap::set` rejected it with
/// `OutOfRange` before any read.
const FAR_OOR: u32 = 5000;

/// First block of the crafted chain; the root pair owns 0 and 1.
const CHAIN_BASE: u32 = 2;
/// Four blocks at 256 bytes hold 1008 content bytes; 900 lands in the
/// fourth (see the module note on why four).
const FILE_SIZE: usize = 900;

fn superblock_bodies() -> (Vec<u8>, Vec<u8>) {
    let mut donor = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut donor, &mut scratch).unwrap();
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    donor.read(0, 0, &mut block0).unwrap();
    let reader = MetadataReader::new(&block0).unwrap();
    let mut name: Option<Vec<u8>> = None;
    let mut geom: Option<Vec<u8>> = None;
    for entry in reader.iter_tags() {
        match entry.tag.tag_type() {
            TagType::Superblock => name = Some(entry.body.to_vec()),
            TagType::InlineStruct if entry.tag.id() == 0 && geom.is_none() => {
                geom = Some(entry.body.to_vec());
            }
            _ => {}
        }
    }
    (name.expect("superblock NAME"), geom.expect("geometry struct"))
}

/// An image whose root block 0 is superblock valid and carries one
/// `RegularFile` entry backed by a `CtzStruct` naming `head`, with a
/// real four block chain written at [`CHAIN_BASE`].
///
/// `corrupt_head_pointer` optionally rewrites the chain head block's
/// skip pointer, which is the pointer the walk dereferences on its
/// second hop.
fn image(head: u32, corrupt_head_pointer: Option<u32>) -> MemStorage {
    let mut storage = MemStorage::new();
    let data = vec![0xA5u8; FILE_SIZE];
    let ctz = build_ctz_chain(&mut storage, CHAIN_BASE, &data);
    assert_eq!(
        ctz.head_block.as_u32(),
        CHAIN_BASE + 3,
        "the fixture must build a four block chain"
    );

    if let Some(bad) = corrupt_head_pointer {
        let mut blk = vec![0u8; MemStorage::BLOCK_SIZE];
        storage.read(ctz.head_block.as_u32(), 0, &mut blk).unwrap();
        blk[0..4].copy_from_slice(&bad.to_le_bytes());
        storage.write_block(ctz.head_block.as_u32(), &blk);
    }

    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&head.to_le_bytes());
    body[4..8].copy_from_slice(&(FILE_SIZE as u32).to_le_bytes());

    let (sb_name, sb_geom) = superblock_bodies();
    let mut root = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    root.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    root.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    root.tag(Tag::new(true, TagType::RegularFile, 1, 3), b"big").unwrap();
    root.tag(Tag::new(true, TagType::CtzStruct, 1, 8), &body).unwrap();
    root.commit(0).unwrap();
    let bytes = root.finish();
    for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
        storage.program(0, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
    }
    storage
}

/// A sound chain reached through a sound `CtzStruct`, with one skip
/// pointer rewritten to `bad`.
fn corrupt_pointer_image(bad: u32) -> MemStorage {
    image(CHAIN_BASE + 3, Some(bad))
}

/// A sound chain reached through a `CtzStruct` whose head is `bad`.
fn corrupt_head_image(bad: u32) -> MemStorage {
    image(bad, None)
}

fn two_buffer_scan(storage: &mut MemStorage) -> Result<(), Error> {
    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    scan_used_blocks(storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b)
}

fn single_buffer_scan(storage: &mut MemStorage) -> Result<(), Error> {
    let mut used = Bitmap::EMPTY;
    let mut buf = common::make_buffer();
    scan_used_with_single_buf(storage, ROOT_BLOCK_PAIR, &mut used, &mut buf)
}

// ---- The walk: an out of range skip pointer ----

#[test]
fn both_walkers_reject_an_out_of_range_skip_pointer() {
    for bad in [NEAR_OOR, FAR_OOR] {
        let mut storage = corrupt_pointer_image(bad);
        assert_eq!(
            two_buffer_scan(&mut storage).expect_err("two buffer walker must reject"),
            Error::Corrupt,
            "an out of range CTZ skip pointer is image corruption, not a \
             device fault and not a capacity limit (block {bad})"
        );
        let mut storage = corrupt_pointer_image(bad);
        assert_eq!(
            single_buffer_scan(&mut storage).expect_err("single buffer walker must reject"),
            Error::Corrupt,
            "block {bad}"
        );
    }
}

// ---- The walk: an out of range chain head ----

#[test]
fn both_walkers_reject_an_out_of_range_chain_head() {
    for bad in [NEAR_OOR, FAR_OOR] {
        let mut storage = corrupt_head_image(bad);
        assert_eq!(
            two_buffer_scan(&mut storage).expect_err("two buffer walker must reject"),
            Error::Corrupt,
            "block {bad}"
        );
        let mut storage = corrupt_head_image(bad);
        assert_eq!(
            single_buffer_scan(&mut storage).expect_err("single buffer walker must reject"),
            Error::Corrupt,
            "block {bad}"
        );
    }
}

// ---- Through the public surface ----

#[test]
fn a_write_over_a_corrupt_chain_reports_corrupt() {
    // Any write needing an allocation re-walks the forest, so the
    // corruption must surface as `Corrupt` rather than as a phantom
    // hardware fault on an unrelated operation.
    for bad in [NEAR_OOR, FAR_OOR] {
        for storage in [corrupt_pointer_image(bad), corrupt_head_image(bad)] {
            let mut a = common::make_buffer();
            let mut b = common::make_buffer();
            let mut fs = Fs::mount(storage, &mut a, &mut b).expect("the metadata itself is sound");
            let err = fs
                .mkdir(Path::new("/d").unwrap(), &mut a, &mut b)
                .expect_err("allocating over a corrupt chain must not succeed");
            assert_eq!(err, Error::Corrupt, "block {bad}");
        }
    }
}

#[test]
fn the_read_path_agrees_with_the_walk() {
    // The R3b half, re-checked through the whole file surface so both
    // halves are pinned to one answer in one place.
    for bad in [NEAR_OOR, FAR_OOR] {
        for storage in [corrupt_pointer_image(bad), corrupt_head_image(bad)] {
            let mut a = common::make_buffer();
            let mut b = common::make_buffer();
            let mut fs = Fs::mount(storage, &mut a, &mut b).expect("the metadata itself is sound");
            let mut out = [0u8; FILE_SIZE];
            let err = fs
                .read_at_path(Path::new("/big").unwrap(), 0, &mut out, &mut a, &mut b)
                .expect_err("an out of range address must not be dereferenced");
            assert_eq!(err, Error::Corrupt, "block {bad}");
        }
    }
}

// ---- The counterweight ----

#[test]
fn a_healthy_chain_still_walks_and_reads() {
    // Rejecting bad addresses must not reject good ones. The same
    // fixture with no corruption walks, marks and reads cleanly.
    let mut storage = image(CHAIN_BASE + 3, None);
    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    scan_used_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b)
        .expect("a healthy chain must walk cleanly");
    for blk in CHAIN_BASE..CHAIN_BASE + 4 {
        assert!(used.is_set(blk), "chain block {blk} must be marked used");
    }

    let mut fs = Fs::mount(image(CHAIN_BASE + 3, None), &mut a, &mut b).unwrap();
    let mut out = [0u8; FILE_SIZE];
    let n = fs.read_at_path(Path::new("/big").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, FILE_SIZE);
    assert!(out.iter().all(|&v| v == 0xA5), "the content must round trip");
}
