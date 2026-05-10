//! Integration tests for `Fs::format`.
//!
//! Formats an empty `MemStorage` and then mounts the result, verifying
//! the round-trip succeeds and the superblock fields match the device
//! geometry.

use littlefs2_pure::{Fs, Superblock, DISK_VERSION};

mod common;
use common::MemStorage;

#[test]
fn format_then_mount_roundtrips() {
    let mut storage = MemStorage::new();
    // Before format: blocks are all 0xFF; mount must fail.
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let res = Fs::mount(MemStorage::new(), &mut buf_a, &mut buf_b);
        assert!(res.is_err(), "pristine storage must not mount");
    }

    // Format.
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();

    // Mount the formatted storage.
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let expected = Superblock {
        version: DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    assert_eq!(fs.superblock(), &expected);
    assert_eq!(fs.root(), littlefs2_pure::ROOT_BLOCK_PAIR);
}

#[test]
fn format_leaves_block_one_erased() {
    let mut storage = MemStorage::new();
    // Pre-dirty block 1 with garbage to confirm format erases it.
    storage.write_block(1, &[0x42u8; MemStorage::BLOCK_SIZE]);

    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();

    // Block 1's bytes should now all be 0xFF (erased).
    let start = MemStorage::BLOCK_SIZE;
    let end = start + MemStorage::BLOCK_SIZE;
    assert!(
        storage.data[start..end].iter().all(|&b| b == 0xFF),
        "format should leave block 1 in erased state"
    );
}

#[test]
fn format_rejects_undersized_scratch() {
    let mut storage = MemStorage::new();
    let mut tiny = [0u8; 32];
    let err = Fs::format(&mut storage, &mut tiny).unwrap_err();
    assert_eq!(err, littlefs2_pure::Error::GeometryMismatch);
}

#[test]
fn format_twice_is_idempotent() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let snapshot1 = storage.data.clone();

    Fs::format(&mut storage, &mut scratch).unwrap();
    let snapshot2 = storage.data.clone();

    assert_eq!(snapshot1, snapshot2, "format must produce a deterministic image");
}
