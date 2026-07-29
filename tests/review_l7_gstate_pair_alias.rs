//! Review L7 (`lfs-a8j`): the mount time gstate accumulation walk must
//! dedup its visited set on the unordered physical block set of each
//! metadata pair, not on the ordered `(a, b)` tuple.
//!
//! Pair order carries no meaning on disk. The active half is chosen by
//! revision counter, not by position, so `{2, 3}` and `{3, 2}` address
//! exactly the same metadata pair and read back exactly the same
//! committed state. The C reference encodes that fact directly: its only
//! two pair comparison primitives are `lfs_pair_cmp` (true when the pairs
//! share any block) and `lfs_pair_issync` (true when they name the same
//! set in either order), and `lfs_dir_fetchmatch` re orders every fetched
//! pair by revision before use.
//!
//! The accumulation walk XOR folds every pair's `MoveState` body into one
//! gstate. XOR is self inverse, so visiting one pair twice folds its
//! contribution twice and cancels it to zero. An image that names one
//! physical pair under both orders therefore erases that pair's pending
//! move from the accumulated gstate, and mount recovery never fires.
//!
//! The three cases below hold everything constant except the order of the
//! second reference, so the differential isolates the ordering as the sole
//! variable:
//!
//! 1. `single_reference` names the carrier pair once as `{2, 3}`.
//! 2. `duplicate_reference_same_order` names it twice, both as `{2, 3}`.
//! 3. `aliased_reference_swapped_order` names it as `{2, 3}` and `{3, 2}`.
//!
//! The carrier pair holds a `MoveState` body naming an out of bounds
//! source pair, so a fired recovery is observable as the `Error::Corrupt`
//! that the review M3 bounds check raises. All three cases must reject the
//! image. Before the fix case 3 mounted successfully, because the second
//! visit cancelled the first.

use littlefs2_pure::gstate::build_move_body;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{BlockAddress, BlockPair, Error, Fs};

mod common;
use common::{BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// The pair that carries the pending move state. Both blocks are inside
/// the eight block device, so the bounds check never rejects the carrier
/// itself; only the move body it holds is out of range.
const CARRIER_A: u32 = 2;
const CARRIER_B: u32 = 3;

/// Lift a valid superblock NAME body and geometry body out of a freshly
/// formatted image so these tests never hardcode the disk version or the
/// geometry encoding.
fn superblock_bodies() -> (Vec<u8>, Vec<u8>) {
    let mut donor = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut donor, &mut scratch).unwrap();
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    donor.read(0, 0, &mut block0).unwrap();
    let reader = littlefs2_pure::MetadataReader::new(&block0).unwrap();
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
    (
        name.expect("formatted image carries a superblock NAME"),
        geom.expect("formatted image carries the geometry struct"),
    )
}

/// Program `bytes` into `block`, honoring the harness's NOR program
/// granularity.
fn program_block(storage: &mut MemStorage, block: u32, bytes: &[u8]) {
    for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
        storage.program(block, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
    }
}

/// Build an image whose root pair carries a valid superblock plus one
/// directory entry per element of `child_refs`, each entry's `DirStruct`
/// naming that pair. Block `CARRIER_A` holds a commit whose `MoveState`
/// body names the out of bounds source pair `{200, 201}`; block
/// `CARRIER_B` stays erased, so the carrier pair reads back identically
/// under either address order.
fn image_with_child_refs(child_refs: &[BlockPair]) -> MemStorage {
    let (sb_name, sb_geom) = superblock_bodies();

    let mut root = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    root.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    root.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    for (i, child) in child_refs.iter().enumerate() {
        // Entry ids start at 1; id 0 is the superblock's own slot.
        let id = (i + 1) as u16;
        let name = [b'a' + i as u8];
        root.tag(Tag::new(true, TagType::Directory, id, 1), &name).unwrap();
        let mut body = [0u8; 8];
        body[0..4].copy_from_slice(&child.a.as_u32().to_le_bytes());
        body[4..8].copy_from_slice(&child.b.as_u32().to_le_bytes());
        root.tag(Tag::new(true, TagType::DirStruct, id, 8), &body).unwrap();
    }
    root.commit(0).unwrap();
    let root_bytes = root.finish();

    // The carrier pair's committed state: one MoveState tag naming an out
    // of bounds source pair. The id mirrors the real writer's ID_NONE.
    let bad_src = BlockPair::new(BlockAddress::new(200), BlockAddress::new(201));
    let move_body = build_move_body(bad_src, 0);
    let mut carrier = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    carrier
        .tag(Tag::new(true, TagType::MoveState, 0x3FF, move_body.len() as u16), &move_body)
        .unwrap();
    carrier.commit(0).unwrap();
    let carrier_bytes = carrier.finish();

    let mut storage = MemStorage::new();
    // Block 0 = root; block 1 stays erased so block 0 is the active half.
    program_block(&mut storage, 0, &root_bytes);
    // Block CARRIER_A holds the commit; block CARRIER_B stays erased.
    program_block(&mut storage, CARRIER_A, &carrier_bytes);
    storage
}

fn mount_result(storage: MemStorage) -> Result<(), Error> {
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    Fs::mount(storage, &mut buf_a, &mut buf_b).map(|_| ())
}

#[test]
fn single_reference_fires_recovery() {
    // Baseline: one reference to the carrier pair, so its contribution is
    // folded exactly once and the pending move survives accumulation.
    let carrier = BlockPair::new(BlockAddress::new(CARRIER_A), BlockAddress::new(CARRIER_B));
    let err = mount_result(image_with_child_refs(&[carrier]))
        .expect_err("a single reference must leave the pending move visible");
    assert_eq!(
        err,
        Error::Corrupt,
        "the out of bounds move source must be rejected once recovery fires"
    );
}

#[test]
fn duplicate_reference_same_order_fires_recovery() {
    // Control: the same pair named twice in the SAME order. The ordered
    // tuple dedup already collapses this, so the contribution is folded
    // once and recovery fires. Isolating this case proves the failure in
    // `aliased_reference_swapped_order` comes from the ORDER, not from the
    // presence of a second reference.
    let carrier = BlockPair::new(BlockAddress::new(CARRIER_A), BlockAddress::new(CARRIER_B));
    let err = mount_result(image_with_child_refs(&[carrier, carrier]))
        .expect_err("a same order duplicate must still leave the pending move visible");
    assert_eq!(err, Error::Corrupt, "the ordered dedup already collapses a same order duplicate");
}

#[test]
fn aliased_reference_swapped_order_fires_recovery() {
    // The reproducer. The same physical pair is named as {2, 3} and as
    // {3, 2}. Both references address one pair and read back one committed
    // MoveState body, so a correct accumulation folds it once. Under the
    // ordered tuple dedup the walk visited the pair twice, XOR cancelled
    // the body to zero, and mount returned Ok with the pending move
    // silently dropped.
    let forward = BlockPair::new(BlockAddress::new(CARRIER_A), BlockAddress::new(CARRIER_B));
    let swapped = BlockPair::new(BlockAddress::new(CARRIER_B), BlockAddress::new(CARRIER_A));
    let err = mount_result(image_with_child_refs(&[forward, swapped])).expect_err(
        "a swapped order alias must not cancel the pending move: \
         the accumulation walk has to dedup on the physical block set",
    );
    assert_eq!(
        err,
        Error::Corrupt,
        "the out of bounds move source must be rejected under aliasing too"
    );
}

#[test]
fn swapped_order_reads_the_same_active_block() {
    // The load bearing premise of the fix: canonicalizing the dedup key
    // does not change what a walk reads, because the reader selects the
    // active half by revision counter and not by position. Parsing the
    // carrier pair under both orders must yield the same active block.
    let storage = image_with_child_refs(&[BlockPair::new(
        BlockAddress::new(CARRIER_A),
        BlockAddress::new(CARRIER_B),
    )]);
    let mut buf_a = vec![0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = vec![0u8; MemStorage::BLOCK_SIZE];
    let mut storage = storage;
    storage.read(CARRIER_A, 0, &mut buf_a).unwrap();
    storage.read(CARRIER_B, 0, &mut buf_b).unwrap();

    let forward = littlefs2_pure::MetadataPair::parse(
        BlockAddress::new(CARRIER_A),
        &buf_a,
        BlockAddress::new(CARRIER_B),
        &buf_b,
    )
    .unwrap();
    let swapped = littlefs2_pure::MetadataPair::parse(
        BlockAddress::new(CARRIER_B),
        &buf_b,
        BlockAddress::new(CARRIER_A),
        &buf_a,
    )
    .unwrap();
    assert_eq!(
        forward.active_block.as_u32(),
        swapped.active_block.as_u32(),
        "revision selection must make the active block independent of pair order"
    );
    assert_eq!(forward.active_block.as_u32(), CARRIER_A);
}
