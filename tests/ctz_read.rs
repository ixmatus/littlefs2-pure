//! Integration tests for storage-backed CTZ file read.

use littlefs2_pure::ctz::{block_count, content_bytes_in_block, read_ctz};

mod common;
use common::{build_ctz_chain, MemStorage};

/// Read the entire chain into `out` and assert it matches `expected`.
fn assert_read(data: &[u8]) {
    let mut storage = MemStorage::new();
    // Use blocks starting at index 2 (blocks 0/1 reserved for the
    // root metadata pair in real images; not relevant here, just a
    // convention).
    let ctz = build_ctz_chain(&mut storage, 2, data);

    let mut out = vec![0u8; data.len()];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    let n = read_ctz(&mut storage, &ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(out, data);
}

#[test]
fn read_zero_bytes() {
    let mut storage = MemStorage::new();
    let ctz = build_ctz_chain(&mut storage, 2, &[]);
    let mut out = [0u8; 4];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    let n = read_ctz(&mut storage, &ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn read_fits_in_block_zero() {
    // Block 0 has no skip pointers, so its content capacity is the full
    // block size.
    let bs = MemStorage::BLOCK_SIZE;
    let data: Vec<u8> = (0..bs as u8).cycle().take(bs / 2).collect();
    assert_read(&data);
}

#[test]
fn read_exactly_block_zero_capacity() {
    let bs = MemStorage::BLOCK_SIZE;
    let data: Vec<u8> = (0..255).cycle().take(bs).collect();
    // This is the boundary: data.len() == content_bytes_in_block(0).
    // block_count returns 1, single block, no chain.
    assert_eq!(block_count(data.len() as u32, bs as u32), 1);
    assert_read(&data);
}

#[test]
fn read_spans_two_blocks() {
    let bs = MemStorage::BLOCK_SIZE as u32;
    // Just past block 0's capacity: 1 byte into block 1.
    let size = content_bytes_in_block(0, bs) + 1;
    let data: Vec<u8> = (0..255).cycle().take(size as usize).collect();
    assert_eq!(block_count(size, bs), 2);
    assert_read(&data);
}

#[test]
fn read_spans_three_blocks_odd_index() {
    let bs = MemStorage::BLOCK_SIZE as u32;
    // Block 0 + block 1 + 1 byte of block 2. block 2 has 2 skip
    // pointers, so its content starts at offset 8.
    let size = content_bytes_in_block(0, bs) + content_bytes_in_block(1, bs) + 5;
    let data: Vec<u8> = (0..255).cycle().take(size as usize).collect();
    assert_eq!(block_count(size, bs), 3);
    assert_read(&data);
}

#[test]
fn read_spans_five_blocks_hits_power_of_two_index() {
    let bs = MemStorage::BLOCK_SIZE as u32;
    // Block 4 is the first power-of-two index with 3 skip pointers;
    // hitting it exercises the count=2 branch of the reverse walk.
    let size = content_bytes_in_block(0, bs)
        + content_bytes_in_block(1, bs)
        + content_bytes_in_block(2, bs)
        + content_bytes_in_block(3, bs)
        + 17;
    let data: Vec<u8> = (0..255).cycle().take(size as usize).collect();
    assert_eq!(block_count(size, bs), 5);
    assert_read(&data);
}

#[test]
fn read_full_8_block_chain() {
    let bs = MemStorage::BLOCK_SIZE as u32;
    // Largest file that fits in the MemStorage device (8 blocks total,
    // but 2 reserved for fictional root pair so we use 6 here).
    let mut size = 0u32;
    for i in 0..6 {
        size += content_bytes_in_block(i, bs);
    }
    let data: Vec<u8> = (0..255).cycle().take(size as usize).collect();
    assert_eq!(block_count(size, bs), 6);
    assert_read(&data);
}

#[test]
fn read_partial_into_short_output() {
    // Build a chain holding 200 bytes, request only 100 bytes via a
    // shorter output slice. Asserts the function honors `out.len()`.
    let data: Vec<u8> = (0..200).map(|i| i as u8).collect();
    let mut storage = MemStorage::new();
    let ctz = build_ctz_chain(&mut storage, 2, &data);
    let mut out = [0u8; 100];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    let n = read_ctz(&mut storage, &ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(n, 100);
    assert_eq!(&out[..], &data[..100]);
}

#[test]
fn read_rejects_undersized_scratch() {
    let data = vec![0u8; 100];
    let mut storage = MemStorage::new();
    let ctz = build_ctz_chain(&mut storage, 2, &data);
    let mut out = [0u8; 100];
    let mut scratch = [0u8; 16]; // < BLOCK_SIZE
    let err = read_ctz(&mut storage, &ctz, &mut out, &mut scratch).unwrap_err();
    assert_eq!(err, littlefs2_pure::Error::GeometryMismatch);
}
