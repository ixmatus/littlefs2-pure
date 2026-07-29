//! Review L5 (`lfs-1nh`): C's orphan count and needssuperblock gstate
//! bits are not modeled, and this file pins the safety claim that makes
//! deferring the modeling acceptable.
//!
//! # The C encoding, derived from the vendored oracle
//!
//! C packs three unrelated things into the 32 bit tag word of its
//! `lfs_gstate_t` (`lfs.c` at the pinned revision `d01280e`, tag
//! `v2.9.3`, vendored under `tools/gen_vectors/littlefs/`):
//!
//! | bits | meaning | oracle |
//! |---|---|---|
//! | 31 | set when the low ten bits are non zero | `lfs_fs_preporphans`, `lfs.c:4838` |
//! | 30 to 20 | tag type; a non zero `type1` means a move is in flight | `lfs_gstate_hasmove`, `lfs.c:415` |
//! | 19 to 10 | the move's source id | `lfs_tag_id`, `lfs.c:363` |
//! | 9 | needssuperblock | `lfs_gstate_needssuperblock`, `lfs.c:420` |
//! | 8 to 0 | orphan count, zero to `0x1ff` | `lfs_gstate_getorphans`, `lfs.c:411` |
//!
//! Two facts about that layout decide what a real C image can carry.
//!
//! First, C strips the whole ten bit size field out of every gstate
//! delta before committing it: `delta.tag &= ~LFS_MKTAG(0, 0, 0x3ff)`
//! at `lfs.c:2024` and `lfs.c:2275`. The orphan count and the
//! needssuperblock bit therefore never reach the disk. The comment at
//! `lfs.c:4482` says so outright for the superblock bit, calling it
//! reserved on disk.
//!
//! Second, bit 31 does survive, because `lfs_fs_preporphans`
//! (`lfs.c:4833`) writes it as a summary of the count. So the only
//! orphan evidence a C image carries is the single bit 31, and C
//! reconstitutes a count of one from it on the next mount:
//! `lfs->gstate.tag += !lfs_tag_isvalid(lfs->gstate.tag)` at
//! `lfs.c:4556`.
//!
//! The disk pattern to pin is therefore a `MoveState` body whose tag
//! word is `0x8000_0000` with a zero source pair.
//!
//! # What this crate does with it
//!
//! [`Gstate::has_pending_move`] tests the three words for non zero, so
//! the orphan marker makes it report true. [`Gstate::pending_move`]
//! then classifies the tag word and finds type `0x000`, not `Delete`,
//! so it returns `None` and mount fires no recovery. The residue sits
//! on the disk until a C mount clears it.
//!
//! That is the divergence. The tests below pin the four claims the
//! deferral rests on: mount succeeds, mount writes nothing, entries
//! stay readable, and later writes still work. If any of them breaks,
//! the deferral is wrong and the modeling is not optional.
//!
//! See `docs/decisions/0016-gstate-totals-and-relocation-cascade.md`,
//! the explicitly out of scope section, for why the full model waits on
//! the `Fs` resident gstate work.

use littlefs2_pure::gstate::Gstate;
use littlefs2_pure::meta::{rev_scmp, Commit, MetadataReader};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType, ID_NONE};
use littlefs2_pure::{BlockAddress, BlockPair, Fs, Path};

mod common;
use common::MemStorage;

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// The gstate tag word a C image carries after `lfs_fs_preporphans`
/// raised the orphan summary bit and the commit path stripped the
/// count. This is the only orphan pattern a real C writer produces.
const C_ORPHAN_MARKER: u32 = 0x8000_0000;

/// The in RAM pattern for one orphan before the commit path strips the
/// count. Unreachable from a conforming C writer, reachable from any
/// other writer of the format, so the reader is pinned against it too.
const RAW_ORPHAN_COUNT_ONE: u32 = 0x8000_0001;

/// The needssuperblock bit, `1 << 9`. C sets it only in RAM and the
/// same strip keeps it off the disk, so this is a foreign writer's
/// pattern rather than C's.
const NEEDS_SUPERBLOCK: u32 = 0x0000_0200;

/// Snapshot every byte of the device.
fn device_bytes(storage: &mut MemStorage) -> Vec<u8> {
    let mut out = Vec::new();
    let mut block = vec![0u8; MemStorage::BLOCK_SIZE];
    for i in 0..MemStorage::BLOCK_COUNT {
        storage.read(i, 0, &mut block).unwrap();
        out.extend_from_slice(&block);
    }
    out
}

/// Format a device and populate it with entries the assertions can
/// look for afterwards: an inline file at the root, a subdirectory,
/// and a file inside that subdirectory.
fn format_and_populate() -> MemStorage {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/keep"), b"payload", &mut a, &mut b).unwrap();
    fs.mkdir(p("/sub"), &mut a, &mut b).unwrap();
    fs.write_to_path(p("/sub/nested"), b"inner", &mut a, &mut b).unwrap();
    fs.into_storage()
}

/// Which half of the root pair `{0, 1}` currently holds the newer
/// commit.
fn active_root_block(storage: &mut MemStorage) -> u32 {
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    let mut block1 = vec![0u8; MemStorage::BLOCK_SIZE];
    storage.read(0, 0, &mut block0).unwrap();
    storage.read(1, 0, &mut block1).unwrap();
    let r0 = MetadataReader::new(&block0).unwrap();
    let r1 = MetadataReader::new(&block1).unwrap();
    u32::from(r1.has_commits() && (!r0.has_commits() || rev_scmp(r1.revision(), r0.revision()) > 0))
}

/// Append one commit to the root pair's active half carrying
/// `tag_word` as the pair's new total gstate contribution, with a zero
/// source pair.
///
/// This is the C convention: a committed `MoveState` tag is the pair's
/// whole contribution, and the reachable aggregate is the XOR across
/// pairs. Only the root carries a contribution here, so the aggregate
/// equals `tag_word`.
fn inject_gstate_total(storage: &mut MemStorage, tag_word: u32, pair: BlockPair) {
    let active = active_root_block(storage);
    let mut buf = vec![0u8; MemStorage::BLOCK_SIZE];
    storage.read(active, 0, &mut buf).unwrap();
    let (end, ptag) = {
        let reader = MetadataReader::new(&buf).unwrap();
        (reader.committed_end(), reader.next_ptag())
    };

    let mut body = [0u8; 12];
    body[0..4].copy_from_slice(&tag_word.to_le_bytes());
    body[4..8].copy_from_slice(&pair.a.as_u32().to_le_bytes());
    body[8..12].copy_from_slice(&pair.b.as_u32().to_le_bytes());

    let new_end = {
        let mut c = Commit::new_appending(&mut buf, end, ptag).unwrap();
        c.tag(Tag::new(true, TagType::MoveState, ID_NONE, 12), &body).unwrap();
        c.finish_padded(0, MemStorage::PROG_SIZE, MemStorage::BLOCK_SIZE).unwrap();
        c.bytes_written()
    };
    for off in (end..new_end).step_by(MemStorage::PROG_SIZE) {
        storage.program(active, off as u32, &buf[off..off + MemStorage::PROG_SIZE]).unwrap();
    }
}

/// Every assertion the adjudication rests on, run against one injected
/// gstate pattern.
///
/// 1. Mount succeeds.
/// 2. Mount emits no recovery commit: the image is byte stable across
///    two consecutive mounts.
/// 3. Entries written before the injection stay readable.
/// 4. A normal write afterwards succeeds and survives a remount.
fn assert_inert(tag_word: u32, label: &str) {
    let mut storage = format_and_populate();
    inject_gstate_total(
        &mut storage,
        tag_word,
        BlockPair::new(BlockAddress::new(0), BlockAddress::new(0)),
    );
    let injected = device_bytes(&mut storage);

    // 1 and 2: mount succeeds and writes nothing.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{label}: mount must succeed on residual gstate, got {e:?}"));

    // 3: entries survive.
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let keep = fs
        .resolve(p("/keep"), &mut a, &mut b)
        .unwrap_or_else(|e| panic!("{label}: /keep must stay readable, got {e:?}"));
    assert_eq!(keep.struct_body, b"payload", "{label}: /keep content changed");
    let nested = fs
        .resolve(p("/sub/nested"), &mut a, &mut b)
        .unwrap_or_else(|e| panic!("{label}: /sub/nested must stay readable, got {e:?}"));
    assert_eq!(nested.struct_body, b"inner", "{label}: /sub/nested content changed");

    let mut storage = fs.into_storage();
    let after_first_mount = device_bytes(&mut storage);
    assert_eq!(
        injected, after_first_mount,
        "{label}: mount emitted a commit; the image is not byte stable across a mount"
    );

    // The second mount must be identical too, which rules out a
    // recovery that converges after one pass rather than firing none.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{label}: the second mount must succeed, got {e:?}"));
    let mut storage = fs.into_storage();
    let after_second_mount = device_bytes(&mut storage);
    assert_eq!(
        injected, after_second_mount,
        "{label}: the second mount emitted a commit; residual gstate is not inert"
    );

    // 4: normal writes still work and survive a remount.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/after"), b"written", &mut a, &mut b).unwrap_or_else(|e| {
        panic!("{label}: a normal write after the residue must succeed, got {e:?}")
    });
    let storage = fs.into_storage();

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{label}: remount after the write must succeed, got {e:?}"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let after = fs
        .resolve(p("/after"), &mut a, &mut b)
        .unwrap_or_else(|e| panic!("{label}: /after must be readable after a remount, got {e:?}"));
    assert_eq!(after.struct_body, b"written", "{label}: /after content changed");
    let keep = fs.resolve(p("/keep"), &mut a, &mut b).expect("/keep must still resolve");
    assert_eq!(keep.struct_body, b"payload", "{label}: /keep content changed after the write");
}

/// Negative control for [`assert_inert`].
///
/// Every claim in this file is of the form "nothing happens", so the
/// whole file would pass just as happily if [`inject_gstate_total`]
/// wrote somewhere `accumulate_gstate` never looks. This test injects a
/// live pending move through the identical path and asserts that mount
/// DOES rewrite the device. That is what proves the injected tag
/// reaches the reachable aggregate, and therefore that the inert
/// results above are about the bit pattern rather than about a test
/// harness that misses.
#[test]
fn control_a_live_move_total_does_reach_mount_recovery() {
    let mut storage = format_and_populate();
    let root = BlockPair::new(BlockAddress::new(0), BlockAddress::new(1));
    let move_word = Tag::new(true, TagType::Delete, 0, 0).into_bits();
    inject_gstate_total(&mut storage, move_word, root);
    let injected = device_bytes(&mut storage);

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect("control: a live pending move must still mount");
    let mut storage = fs.into_storage();
    let after = device_bytes(&mut storage);
    assert_ne!(
        injected, after,
        "control: mount must rewrite the device when the injected gstate names a real move; \
         if this passes unchanged, inject_gstate_total is not reaching the aggregate and \
         every inert assertion in this file is vacuous"
    );
}

/// The pattern a real crashed C image carries: bit 31 alone.
#[test]
fn c_orphan_marker_is_inert_under_our_reader() {
    assert_inert(C_ORPHAN_MARKER, "C orphan marker 0x80000000");
}

/// The in RAM orphan count, which a foreign writer could persist even
/// though C strips it.
#[test]
fn raw_orphan_count_is_inert_under_our_reader() {
    assert_inert(RAW_ORPHAN_COUNT_ONE, "raw orphan count 0x80000001");
    assert_inert(0x8000_0003, "raw orphan count 0x80000003");
    assert_inert(0x8000_01ff, "raw orphan count at the 0x1ff ceiling");
}

/// The needssuperblock bit, alone and combined with an orphan count.
#[test]
fn needssuperblock_bit_is_inert_under_our_reader() {
    assert_inert(NEEDS_SUPERBLOCK, "needssuperblock alone");
    assert_inert(C_ORPHAN_MARKER | NEEDS_SUPERBLOCK | 0x0000_0003, "needssuperblock plus orphans");
}

/// The decode level statement of the adjudication, in the exact terms
/// it was written in: the orphan patterns make `has_pending_move`
/// report true while `pending_move` yields nothing, so mount's only
/// gstate trigger never fires.
#[test]
fn orphan_patterns_decode_as_pending_move_without_a_move() {
    for tag_word in [
        C_ORPHAN_MARKER,
        RAW_ORPHAN_COUNT_ONE,
        NEEDS_SUPERBLOCK,
        C_ORPHAN_MARKER | NEEDS_SUPERBLOCK,
    ] {
        let mut body = [0u8; 12];
        body[0..4].copy_from_slice(&tag_word.to_le_bytes());
        let mut g = Gstate::ZERO;
        g.xor_body(&body);
        assert!(
            g.has_pending_move(),
            "0x{tag_word:08x}: a non zero tag word reports a pending move"
        );
        assert_eq!(
            g.pending_move(),
            None,
            "0x{tag_word:08x}: the tag word is not a Delete tag, so no move is decoded"
        );
        assert!(
            !g.has_pending_relocation(),
            "0x{tag_word:08x}: the move word must not leak into the relocation words"
        );
    }
}

/// The other half of the safety claim: the orphan summary bit sits in
/// the tag's valid bit position, which this crate's tag decoder
/// ignores when classifying a type. So an image that carries an orphan
/// AND a genuine in flight move still decodes the move and still gets
/// recovered.
///
/// C builds that word as `0x8000_0000 | LFS_MKTAG(LFS_TYPE_DELETE, id,
/// 0)` (`lfs_fs_prepmove` at `lfs.c:4845` sets the type and id;
/// `lfs_fs_preporphans` at `lfs.c:4833` raises bit 31 independently).
/// If the orphan bit masked the move, a crashed cross directory rename
/// in an image that also has an orphan would never be completed, and
/// that would be a correctness bug rather than a deferrable divergence.
#[test]
fn orphan_marker_does_not_mask_a_real_pending_move() {
    let src = BlockPair::new(BlockAddress::new(4), BlockAddress::new(5));
    let move_word = Tag::new(true, TagType::Delete, 7, 0).into_bits();
    assert_eq!(move_word & C_ORPHAN_MARKER, 0, "a valid Delete tag has bit 31 clear");

    for extra in [0u32, C_ORPHAN_MARKER, C_ORPHAN_MARKER | NEEDS_SUPERBLOCK | 0x0000_0002] {
        let mut body = [0u8; 12];
        body[0..4].copy_from_slice(&(move_word | extra).to_le_bytes());
        body[4..8].copy_from_slice(&src.a.as_u32().to_le_bytes());
        body[8..12].copy_from_slice(&src.b.as_u32().to_le_bytes());
        let mut g = Gstate::ZERO;
        g.xor_body(&body);
        assert_eq!(
            g.pending_move(),
            Some((src, 7)),
            "extra bits 0x{extra:08x} must not hide the in flight move"
        );
    }
}
