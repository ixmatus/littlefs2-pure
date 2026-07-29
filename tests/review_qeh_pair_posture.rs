//! Review `lfs-qeh`: one posture for pair addresses decoded from disk,
//! pinned at every walker.
//!
//! The posture itself is written down once, on `fs::pair_in_bounds`:
//!
//! 1. The all ones sentinel is thread end, never an address. It is
//!    resolved at the tail decode, so it never reaches a bounds check.
//! 2. A genuine out of range pair is [`Error::Corrupt`], not a skip.
//!
//! Rule 2's oracle is `lfs_dir_fetchmatch` (`lfs.c:1103`), which returns
//! `LFS_ERR_CORRUPT` when either half of a pair is `>= lfs->block_count`.
//! Before this change the rule was split: the gstate accumulation walk
//! and both allocator walkers rejected, while `collect_live_tree_pairs`'s
//! `HardTail` and the parent lookup BFS silently skipped. A skip lets a
//! walk return a confidently incomplete answer rather than an error.
//!
//! Rules 1 and 2 have to be pinned together, because the cheap way to
//! satisfy either one alone breaks the other: a walker that rejects every
//! address it cannot fetch also rejects the sentinel (the `lfs-yl6`
//! regression), and a walker that tolerates the sentinel by tolerating
//! unfetchable addresses swallows real corruption.
//!
//! # Scope
//!
//! These tests cover the walkers. The `HardTail` *chase* sites once
//! surfaced an out of range address as [`Error::Io`]; review `lfs-w3o`
//! closed that gap by routing every pair-block read in `src/fs.rs`
//! through `fs::read_pair_blocks`, so the posture is now one rule at
//! every fetch, exactly as the C reference has it. The test named
//! `out_of_range_hard_tail_chase_is_corrupt` (formerly
//! `..._is_still_io`) pins the unified answer here; the chase sites get
//! their own coverage in `tests/review_w3o_chase_pair_bounds.rs`.

use littlefs2_pure::alloc::{scan_used_blocks, scan_used_with_single_buf, Bitmap};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, MetadataReader, Path, ROOT_BLOCK_PAIR};

mod common;
use common::{BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// Inside the allocator's 4096 entry tracking bitmap, past the device's
/// 8 block count.
const NEAR_OOR: (u32, u32) = (200, 201);
/// Past the tracking bitmap too, where `Bitmap::is_set` answers `true`
/// and an "already marked" guard would swallow the address.
const FAR_OOR: (u32, u32) = (5000, 5001);
/// The all ones sentinel: not an address at all.
const SENTINEL: (u32, u32) = (u32::MAX, u32::MAX);

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

fn pair_body(pair: (u32, u32)) -> [u8; 8] {
    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&pair.0.to_le_bytes());
    body[4..8].copy_from_slice(&pair.1.to_le_bytes());
    body
}

fn program_block(storage: &mut MemStorage, block: u32, bytes: &[u8]) {
    for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
        storage.program(block, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
    }
}

/// How the crafted root refers to `target`.
#[derive(Clone, Copy, Debug)]
enum Via {
    SoftTail,
    HardTail,
    DirStruct,
}

/// Root pair that is superblock valid and refers to `target` via `how`.
fn image(target: (u32, u32), how: Via) -> MemStorage {
    let (sb_name, sb_geom) = superblock_bodies();
    let body = pair_body(target);
    let mut root = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    root.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    root.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    match how {
        Via::SoftTail => {
            root.tag(Tag::new(true, TagType::SoftTail, 0x3FF, 8), &body).unwrap();
        }
        Via::HardTail => {
            root.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &body).unwrap();
        }
        Via::DirStruct => {
            root.tag(Tag::new(true, TagType::Directory, 1, 1), b"d").unwrap();
            root.tag(Tag::new(true, TagType::DirStruct, 1, 8), &body).unwrap();
        }
    }
    root.commit(0).unwrap();
    let bytes = root.finish();
    let mut storage = MemStorage::new();
    program_block(&mut storage, 0, &bytes);
    storage
}

fn mount_result(storage: MemStorage) -> Result<(), Error> {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    Fs::mount(storage, &mut a, &mut b).map(|_| ())
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

// ---- Rule 2: out of range rejects, at every walker ----

#[test]
fn mount_rejects_out_of_range_pairs_however_referenced() {
    // The gstate accumulation walk runs first at mount and follows both
    // tail flavors plus live DirStruct children, so every reference shape
    // reaches a bounds check here.
    for how in [Via::SoftTail, Via::DirStruct] {
        for target in [NEAR_OOR, FAR_OOR] {
            let err = mount_result(image(target, how))
                .expect_err("an out of range pair must not mount cleanly");
            assert_eq!(
                err,
                Error::Corrupt,
                "an out of range pair address is image corruption, not a device \
                 fault or a thing to skip ({how:?} -> {target:?})"
            );
        }
    }
}

#[test]
fn allocator_walkers_reject_out_of_range_pairs() {
    for how in [Via::SoftTail, Via::HardTail, Via::DirStruct] {
        for target in [NEAR_OOR, FAR_OOR] {
            let mut storage = image(target, how);
            assert_eq!(
                two_buffer_scan(&mut storage).expect_err("two buffer walker must reject"),
                Error::Corrupt,
                "{how:?} -> {target:?}"
            );
            let mut storage = image(target, how);
            assert_eq!(
                single_buffer_scan(&mut storage).expect_err("single buffer walker must reject"),
                Error::Corrupt,
                "{how:?} -> {target:?}"
            );
        }
    }
}

#[test]
fn post_mount_write_path_rejects_an_out_of_range_thread_link() {
    // A device that develops corruption AFTER mount still meets the
    // posture: the write path re-walks the forest for its allocation and
    // budget checks, and those walkers reject rather than skip. Mount is
    // not re-run here, so this exercises the walkers directly rather than
    // mount's own accumulation pass.
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    // Overwrite the alternate root block with a higher revision commit
    // whose SoftTail points off the device.
    let (sb_name, sb_geom) = superblock_bodies();
    let mut blk = BlockBuilder::new(MemStorage::BLOCK_SIZE, 99).unwrap();
    blk.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    blk.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    blk.tag(Tag::new(true, TagType::SoftTail, 0x3FF, 8), &pair_body(NEAR_OOR)).unwrap();
    blk.commit(0).unwrap();
    let bytes = blk.finish();
    {
        let s = fs.storage_mut();
        s.erase(1).unwrap();
        for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
            s.program(1, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
        }
    }

    let err = fs
        .mkdir(Path::new("/x").unwrap(), &mut a, &mut b)
        .expect_err("a write over a corrupt thread link must not succeed");
    assert_eq!(err, Error::Corrupt, "the write path's walkers owe the same rejection");
}

// ---- Rule 1: the sentinel is not an out of range address ----

#[test]
fn the_sentinel_is_accepted_as_thread_end_at_every_walker() {
    // The counterweight to rule 2. Satisfying rule 2 by rejecting
    // everything unfetchable would reject this, which is the `lfs-yl6`
    // regression: a conforming C written image made unmountable.
    for how in [Via::SoftTail, Via::HardTail] {
        mount_result(image(SENTINEL, how))
            .unwrap_or_else(|e| panic!("mount must accept a null {how:?}: {e:?}"));

        let mut storage = image(SENTINEL, how);
        two_buffer_scan(&mut storage)
            .unwrap_or_else(|e| panic!("two buffer walker must accept a null {how:?}: {e:?}"));

        let mut storage = image(SENTINEL, how);
        single_buffer_scan(&mut storage)
            .unwrap_or_else(|e| panic!("single buffer walker must accept a null {how:?}: {e:?}"));
    }
}

#[test]
fn the_sentinel_in_a_dir_struct_is_still_corruption() {
    // Rule 1 is about TAIL bodies. The sentinel means "thread ends here",
    // and a `DirStruct` has no thread to end: the C writer never emits an
    // all ones `DirStruct`, so there is nothing to be lenient toward and
    // it falls through to rule 2.
    let err = mount_result(image(SENTINEL, Via::DirStruct))
        .expect_err("an all ones DirStruct is not thread end");
    assert_eq!(err, Error::Corrupt);

    let mut storage = image(SENTINEL, Via::DirStruct);
    assert_eq!(
        two_buffer_scan(&mut storage).expect_err("walker must reject an all ones DirStruct"),
        Error::Corrupt
    );
    let mut storage = image(SENTINEL, Via::DirStruct);
    assert_eq!(
        single_buffer_scan(&mut storage).expect_err("walker must reject an all ones DirStruct"),
        Error::Corrupt
    );
}

// ---- The former gap, now closed ----

#[test]
fn out_of_range_hard_tail_chase_is_corrupt() {
    // Once the documented residual, now the posture. `resolve` and
    // `list_dir` chase a HardTail; both used to hand the address straight
    // to `Storage::read` with no bounds check, so out of range surfaced
    // as `Io` rather than `Corrupt`. The C reference never had that
    // split, because its bounds check lives inside `lfs_dir_fetchmatch`
    // and so covers chases too. Review `lfs-w3o` routed every pair-block
    // read through `fs::read_pair_blocks`, which applies the same rule
    // in the same place the C reference does.
    let storage = image(NEAR_OOR, Via::HardTail);
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    // Mount rejects it as Corrupt (the accumulation walk sees the tail
    // before any chase does), so reach the chase from a mounted handle
    // over a device that changes underneath.
    assert_eq!(Fs::mount(storage, &mut a, &mut b).map(|_| ()).unwrap_err(), Error::Corrupt);

    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();
    let (sb_name, sb_geom) = superblock_bodies();
    let mut blk = BlockBuilder::new(MemStorage::BLOCK_SIZE, 99).unwrap();
    blk.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    blk.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    blk.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &pair_body(NEAR_OOR)).unwrap();
    blk.commit(0).unwrap();
    let bytes = blk.finish();
    {
        let s = fs.storage_mut();
        s.erase(1).unwrap();
        for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
            s.program(1, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
        }
    }
    assert_eq!(
        fs.list_root(|_| {}, &mut a, &mut b).expect_err("the chase must reject it"),
        Error::Corrupt,
        "the chase sites now answer with the same posture as the walkers"
    );
}
