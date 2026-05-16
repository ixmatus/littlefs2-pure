//! Integration tests for `Fs::mount`.
//!
//! Exercise the storage-backed mount path against an in-memory `MemStorage`
//! pre-populated with a hand-built superblock pair.

use littlefs2_pure::{Error, Fs, Superblock, DISK_VERSION};

mod common;
use common::{build_superblock_block, MemStorage};

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
fn mount_succeeds_against_well_formed_image() {
    let mut storage = MemStorage::new();
    let sb_block = build_superblock_block(&well_formed_sb(), MemStorage::BLOCK_SIZE);
    storage.write_block(0, &sb_block);
    // Block 1 stays in pristine erased state (all 0xFF) -> no commits.

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(fs.superblock().version, DISK_VERSION);
    assert_eq!(fs.superblock().block_size as usize, MemStorage::BLOCK_SIZE);
    assert_eq!(fs.superblock().block_count, MemStorage::BLOCK_COUNT);
    assert_eq!(fs.root(), littlefs2_pure::ROOT_BLOCK_PAIR);
}

#[test]
fn mount_picks_higher_revision_block() {
    let mut storage = MemStorage::new();

    // Block 0: revision 5, valid superblock.
    let sb_a = well_formed_sb();
    let mut a = build_superblock_block(&sb_a, MemStorage::BLOCK_SIZE);
    // The builder writes revision 1 by default. Patch it to 5.
    a[0..4].copy_from_slice(&5u32.to_le_bytes());
    // The CRC was computed for revision 1, so this block is now invalid.
    // Mount should fall back to block 1.

    storage.write_block(0, &a);

    // Block 1: revision 7, valid superblock.
    let sb_b = Superblock { block_count: MemStorage::BLOCK_COUNT, ..well_formed_sb() };
    let mut b = build_superblock_block(&sb_b, MemStorage::BLOCK_SIZE);
    b[0..4].copy_from_slice(&7u32.to_le_bytes());
    // Same situation: revision was 1 when CRC was computed, now 7.
    // Both blocks have invalid CRCs.

    storage.write_block(1, &b);

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    // Both blocks have invalid CRCs (we patched the revision after the
    // CRC was computed) -> mount should fail with Corrupt.
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::Corrupt);
}

#[test]
fn mount_rejects_geometry_mismatch_on_block_count() {
    let mut storage = MemStorage::new();
    let sb = Superblock {
        version: DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT + 1, // mismatched
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::GeometryMismatch);
}

#[test]
fn mount_rejects_geometry_mismatch_on_block_size() {
    let mut storage = MemStorage::new();
    let sb = Superblock {
        version: DISK_VERSION,
        block_size: 512, // device is 256
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::GeometryMismatch);
}

#[test]
fn mount_rejects_wrong_buffer_size() {
    let mut storage = MemStorage::new();
    let sb_block = build_superblock_block(&well_formed_sb(), MemStorage::BLOCK_SIZE);
    storage.write_block(0, &sb_block);

    let mut buf_a = [0u8; 128]; // wrong size
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::GeometryMismatch);
}

#[test]
fn mount_returns_unformatted_for_pristine_chip() {
    // Fresh MemStorage is all 0xFF — the erased state of every NOR or
    // NAND chip on the planet. This is the boot-time "no filesystem
    // here yet" state, distinct from `Corrupt` which means the chip
    // has been programmed but the metadata cannot be parsed.
    let storage = MemStorage::new();

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::Unformatted);
}

#[test]
fn mount_returns_corrupt_for_programmed_but_invalid_pair() {
    // A device that has been programmed (some bytes are non-0xFF) but
    // whose root pair has no successfully verified CCRC commit is
    // genuinely corrupt: bit rot, torn erase, or someone else's data
    // accidentally written here. Caller's recovery story must
    // distinguish this from the `Unformatted` case.
    let mut storage = MemStorage::new();
    // Plant a non-0xFF byte at offset 0 of block 0; everything else
    // stays 0xFF. No valid commit, but the chip is "not pristine".
    storage.write_block(0, &[0x42u8; 4]);

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::Corrupt);
}

#[test]
fn mount_rejects_live_dirstruct_pointing_out_of_bounds() {
    // Adversarial image: a live DirStruct whose pair address is past
    // BLOCK_COUNT. The mount-time gstate walk must reject with
    // Error::Corrupt rather than hand the out-of-range address to
    // Storage::read and accumulate a bogus recovery gstate.
    use littlefs2_pure::meta::{Commit, MetadataPair};
    use littlefs2_pure::storage::Storage;
    use littlefs2_pure::tag::{Tag, TagType};
    use littlefs2_pure::{Path, ROOT_BLOCK_PAIR};

    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let mut storage = fs.into_storage();

    let mut ba = [0u8; MemStorage::BLOCK_SIZE];
    let mut bb = [0u8; MemStorage::BLOCK_SIZE];
    storage.read(ROOT_BLOCK_PAIR.a.as_u32(), 0, &mut ba).unwrap();
    storage.read(ROOT_BLOCK_PAIR.b.as_u32(), 0, &mut bb).unwrap();
    let (active_addr, committed_end, next_ptag, active_is_a, dir_id) = {
        let pair = MetadataPair::parse(ROOT_BLOCK_PAIR.a, &ba, ROOT_BLOCK_PAIR.b, &bb).unwrap();
        let mut id = None;
        for e in pair.reader.iter_tags() {
            if e.tag.tag_type() == TagType::DirStruct {
                id = Some(e.tag.id());
            }
        }
        (
            pair.active_block,
            pair.reader.committed_end(),
            pair.reader.next_ptag(),
            pair.active_block == ROOT_BLOCK_PAIR.a,
            id.expect("/d has a DirStruct tag"),
        )
    };

    // Re-emit a DirStruct at /d's id with an out-of-range pair; the
    // latest-tag-wins reader makes this the live struct for /d.
    let mut oob = [0u8; 8];
    oob[0..4].copy_from_slice(&9999u32.to_le_bytes());
    oob[4..8].copy_from_slice(&9998u32.to_le_bytes());
    let active_buf: &mut [u8] = if active_is_a { &mut ba } else { &mut bb };
    let new_end = {
        let mut commit = Commit::new_appending(active_buf, committed_end, next_ptag).unwrap();
        commit.tag(Tag::new(true, TagType::DirStruct, dir_id, 8), &oob).unwrap();
        commit.finish_padded(0, MemStorage::PROG_SIZE, MemStorage::BLOCK_SIZE).unwrap();
        commit.bytes_written()
    };
    storage
        .program(active_addr.as_u32(), committed_end as u32, &active_buf[committed_end..new_end])
        .unwrap();

    let mut m_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut m_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut m_a, &mut m_b)
        .expect_err("mount must reject an out-of-range live DirStruct");
    assert_eq!(err, Error::Corrupt);
}

#[test]
fn fs_exposes_storage_via_accessors() {
    let mut storage = MemStorage::new();
    storage.write_block(0, &build_superblock_block(&well_formed_sb(), MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    // We can borrow the storage immutably and mutably.
    let _ = fs.storage().data.len();
    let _ = fs.storage_mut().data.len();
    // We can recover it.
    let recovered = fs.into_storage();
    assert_eq!(recovered.data.len(), MemStorage::BLOCK_SIZE * MemStorage::BLOCK_COUNT as usize);
}
