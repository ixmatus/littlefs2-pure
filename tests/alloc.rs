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
fn adversarial_ctz_size_is_corrupt_not_a_scan_dos() {
    // Review H8: a committed CtzStruct whose size implies more blocks
    // than the device holds must be rejected as Corrupt before the
    // chain walk starts. Without the guard, the walk performs
    // ~size/block_size skip-pointer reads per allocator rescan; the
    // in-bounds self-pointing blocks below keep the unguarded walk
    // alive for all ~16.8M iterations, so the pre-fix behavior is a
    // multi-second scan that ends Ok, not an error.
    let mut storage = MemStorage::new();

    // Self-pointing skip pointers: every word in blocks 2..8 is the
    // valid block address 2, so an unguarded walk never leaves the
    // device and never errors.
    let mut self_ptr = [0u8; MemStorage::BLOCK_SIZE];
    for w in self_ptr.chunks_exact_mut(4) {
        w.copy_from_slice(&2u32.to_le_bytes());
    }
    for b in 2..MemStorage::BLOCK_COUNT {
        storage.write_block(b, &self_ptr);
    }

    // Root pair: block 0 carries one file entry whose CtzStruct claims
    // size u32::MAX with its head inside the device; block 1 is erased.
    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&2u32.to_le_bytes());
    body[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let entries = [common::DirEntrySpec {
        id: 0,
        name: b"big",
        name_type: littlefs2_pure::TagType::RegularFile,
        struct_type: littlefs2_pure::TagType::CtzStruct,
        struct_body: &body,
    }];
    let block = common::build_directory_block(1, &entries, MemStorage::BLOCK_SIZE);
    storage.write_block(0, &block);

    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let err =
        scan_used_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b).unwrap_err();
    assert_eq!(err, littlefs2_pure::Error::Corrupt);
}

/// 512-block in-RAM storage for the over-`MAX_CTZ_BLOCKS` chain test.
/// `MemStorage` has 8 blocks, far below the 257-block chain the test
/// needs.
struct BigStorage {
    data: Vec<u8>,
}

impl BigStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 512;

    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }

    fn write_block(&mut self, block: u32, bytes: &[u8]) {
        let start = (block as usize) * Self::BLOCK_SIZE;
        self.data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

impl littlefs2_pure::Storage for BigStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize)
            .checked_mul(Self::BLOCK_SIZE)
            .and_then(|b| b.checked_add(off as usize))
            .ok_or(())?;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= <Self as littlefs2_pure::Storage>::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize)
            .checked_mul(Self::BLOCK_SIZE)
            .and_then(|b| b.checked_add(off as usize))
            .ok_or(())?;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= <Self as littlefs2_pure::Storage>::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= <Self as littlefs2_pure::Storage>::BLOCK_COUNT {
            return Err(());
        }
        let start = (block as usize) * Self::BLOCK_SIZE;
        for b in &mut self.data[start..start + Self::BLOCK_SIZE] {
            *b = 0xFF;
        }
        Ok(())
    }
}

#[test]
fn chain_longer_than_read_cap_still_marks_used() {
    // The H8 guard bounds the walk at the device size, NOT at the read
    // path's MAX_CTZ_BLOCKS stack cap. A C-written file larger than
    // the read cap is unreadable by this crate but its blocks must
    // still be marked used, or the allocator would hand them out and
    // destroy the file on the next write.
    use littlefs2_pure::ctz::{content_bytes_in_block, skip_pointers_in_block, MAX_CTZ_BLOCKS};

    let bs = BigStorage::BLOCK_SIZE as u32;
    let total: u32 = MAX_CTZ_BLOCKS as u32 + 44; // 300 blocks, > read cap, < device
    let size: u32 = (0..total).map(|i| content_bytes_in_block(i, bs)).sum();
    assert_eq!(littlefs2_pure::ctz::block_count(size, bs), total);

    let mut storage = BigStorage::new();
    let base: u32 = 2;
    // Build the chain's skip-pointer headers (content bytes stay 0xFF;
    // the scan never reads them).
    for i in 0..total {
        let mut block_buf = [0xFFu8; BigStorage::BLOCK_SIZE];
        for k in 0..skip_pointers_in_block(i) as usize {
            let target_phys = base + i - (1u32 << k);
            block_buf[4 * k..4 * k + 4].copy_from_slice(&target_phys.to_le_bytes());
        }
        storage.write_block(base + i, &block_buf);
    }

    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&(base + total - 1).to_le_bytes());
    body[4..8].copy_from_slice(&size.to_le_bytes());
    let entries = [common::DirEntrySpec {
        id: 0,
        name: b"big",
        name_type: littlefs2_pure::TagType::RegularFile,
        struct_type: littlefs2_pure::TagType::CtzStruct,
        struct_body: &body,
    }];
    let block = common::build_directory_block(1, &entries, BigStorage::BLOCK_SIZE);
    storage.write_block(0, &block);

    let mut used = Bitmap::EMPTY;
    let mut a = [0u8; BigStorage::BLOCK_SIZE];
    let mut b = [0u8; BigStorage::BLOCK_SIZE];
    scan_used_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b).unwrap();
    for i in 0..total {
        assert!(used.is_set(base + i), "chain block {} unmarked", base + i);
    }
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
