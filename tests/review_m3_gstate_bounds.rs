//! Review M3 (`lfs-fr8`): mount-time gstate recovery must validate the
//! bounds of a pair address it decodes from gstate before dereferencing
//! or writing through it.
//!
//! A crashed cross-directory rename leaves a `MoveState` tag whose body
//! names the move's source pair. `Fs::mount` accumulates that gstate and,
//! if non-zero, runs `recover_pending_move`, which reads the named pair
//! and commits a balancing delete into it. The decoded pair is an on-disk
//! pointer like any other, but unlike every tail or `DirStruct` pointer it
//! was not bounds-checked, so a corrupt or adversarial `MoveState` body
//! naming an out-of-range pair drove mount recovery to dereference (and
//! attempt to write through) an out-of-bounds block address.
//!
//! This crafts a superblock-valid root whose log carries one `MoveState`
//! tag naming the pair `{200, 201}` on an 8-block device. Mount must
//! reject it with `Error::Corrupt` (the bounds check) rather than
//! dereferencing block 200.

use littlefs2_pure::gstate::build_move_body;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{BlockAddress, BlockPair, Error, Fs};

mod common;
use common::{BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;

#[test]
fn mount_rejects_out_of_bounds_gstate_move_pair() {
    // Pull a valid superblock NAME + geometry body from a fresh format so
    // this test does not hardcode the disk version or geometry encoding.
    let mut donor = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut donor, &mut scratch).unwrap();
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    donor.read(0, 0, &mut block0).unwrap();
    let reader = littlefs2_pure::MetadataReader::new(&block0).unwrap();
    let mut sb_name: Option<alloc::vec::Vec<u8>> = None;
    let mut sb_geom: Option<alloc::vec::Vec<u8>> = None;
    for entry in reader.iter_tags() {
        match entry.tag.tag_type() {
            TagType::Superblock => sb_name = Some(entry.body.to_vec()),
            TagType::InlineStruct if entry.tag.id() == 0 && sb_geom.is_none() => {
                sb_geom = Some(entry.body.to_vec());
            }
            _ => {}
        }
    }
    let sb_name = sb_name.expect("formatted image carries a superblock NAME");
    let sb_geom = sb_geom.expect("formatted image carries the geometry struct");

    // Craft the root block: valid superblock plus one MoveState tag whose
    // body names an out-of-bounds source pair {200, 201} (the device holds
    // 8 blocks). The `id` mirrors the real writer's ID_NONE (0x3FF).
    let bad_pair = BlockPair::new(BlockAddress::new(200), BlockAddress::new(201));
    let move_body = build_move_body(bad_pair, 0);

    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    builder
        .tag(Tag::new(true, TagType::MoveState, 0x3FF, move_body.len() as u16), &move_body)
        .unwrap();
    builder.commit(0).unwrap();
    let crafted = builder.finish();

    let mut storage = MemStorage::new();
    // Block 0 = crafted; block 1 stays erased so block 0 is the active half.
    for (i, chunk) in crafted.chunks(MemStorage::PROG_SIZE).enumerate() {
        storage.program(0, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
    }

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect_err("mount must reject an out-of-bounds gstate move pair");
    assert_eq!(
        err,
        Error::Corrupt,
        "the out-of-bounds pair must be rejected as Corrupt, not dereferenced"
    );
}
