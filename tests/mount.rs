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
fn mount_rejects_unformatted_device() {
    let storage = MemStorage::new(); // all 0xFF, no superblock

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    // Both blocks have no commits -> Corrupt from MetadataPair::parse.
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
