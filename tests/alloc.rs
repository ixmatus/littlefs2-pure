//! Integration tests for the block allocator.

use littlefs2_pure::alloc::{alloc_blocks, scan_used_blocks, Bitmap};
use littlefs2_pure::{BlockAddress, Fs, ROOT_BLOCK_PAIR};

mod common;
use common::MemStorage;

fn formatted_storage() -> MemStorage {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    storage
}

#[test]
fn scan_after_format_marks_only_root_pair() {
    let mut storage = formatted_storage();
    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    scan_used_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b).unwrap();

    // Blocks 0 and 1 are the root pair.
    assert!(used.is_set(0));
    assert!(used.is_set(1));
    // No other blocks used.
    for b in 2..MemStorage::BLOCK_COUNT {
        assert!(!used.is_set(b), "block {b} should be free after format");
    }
}

#[test]
fn alloc_returns_lowest_unused_blocks() {
    let mut storage = formatted_storage();
    let mut out = [BlockAddress::NONE; 3];
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    alloc_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut out, &mut a, &mut b).unwrap();
    // After format, blocks 0 and 1 are used; lowest free are 2, 3, 4.
    assert_eq!(out, [BlockAddress::new(2), BlockAddress::new(3), BlockAddress::new(4)]);
}

#[test]
fn alloc_marks_in_use_blocks_after_ctz_write() {
    // Write a CTZ file (which allocates blocks), then re-scan and
    // verify the new blocks show as used.
    let mut storage = formatted_storage();
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let content: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
        fs.write_to_root(b"f", &content, &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // Re-scan.
    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    scan_used_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b).unwrap();

    assert!(used.is_set(0));
    assert!(used.is_set(1));
    // At least one block in 2..8 should now be used (the CTZ chain).
    let chain_blocks: Vec<u32> = (2..MemStorage::BLOCK_COUNT).filter(|&b| used.is_set(b)).collect();
    assert!(!chain_blocks.is_empty(), "no CTZ chain blocks marked used");
}

#[test]
fn alloc_returns_error_when_disk_full() {
    let mut storage = formatted_storage();
    // MemStorage has 8 blocks total. After format, 2 are used (root
    // pair). Asking for 7 must succeed; asking for 8 must fail.
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    let mut six = [BlockAddress::NONE; 6];
    alloc_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut six, &mut a, &mut b).unwrap();

    let mut seven = [BlockAddress::NONE; 7];
    let err = alloc_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut seven, &mut a, &mut b).unwrap_err();
    assert_eq!(err, littlefs2_pure::Error::OutOfRange);
}
