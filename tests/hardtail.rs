//! Integration tests for HardTail chasing.
//!
//! Builds an image where the root directory is split across two metadata
//! pairs threaded by a HardTail tag. The first pair holds the superblock
//! and a HardTail tag pointing to the second pair; the second pair holds
//! a file. `Fs::resolve` must chase the HardTail to find the file.

use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, Path, Superblock, DISK_VERSION, MAGIC};

mod common;
use common::{build_directory_block, BlockBuilder, DirEntrySpec, MemStorage};

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
fn resolve_chases_hardtail_to_continuation_pair() {
    let mut storage = MemStorage::new();

    // Continuation pair at (2, 3): holds the file "deep.txt".
    let continuation = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"deep.txt",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: b"found via hardtail",
        }],
        MemStorage::BLOCK_SIZE,
    );
    storage.write_block(2, &continuation);

    // Root pair at (0, 1): superblock plus a HardTail pointing at (2, 3).
    // We omit any entries here; the file is only reachable via the tail.
    let sb_bytes = well_formed_sb().to_bytes();
    let tail_bytes = {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&2u32.to_le_bytes());
        t[4..8].copy_from_slice(&3u32.to_le_bytes());
        t
    };
    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::Superblock, 0, 8), MAGIC).unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    builder.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail_bytes).unwrap();
    builder.commit(0).unwrap();
    storage.write_block(0, &builder.finish());

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let resolved = fs.resolve(Path::new("/deep.txt").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(resolved.entry.name, b"deep.txt");
    assert_eq!(resolved.struct_body, b"found via hardtail");
}

#[test]
fn resolve_does_not_chase_softtail() {
    let mut storage = MemStorage::new();

    // Build a pair (2, 3) that DOES have the file (but we should not
    // find it via the root pair's SoftTail).
    let continuation = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"deep.txt",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: b"should not be reached",
        }],
        MemStorage::BLOCK_SIZE,
    );
    storage.write_block(2, &continuation);

    // Root pair: superblock + SoftTail (not HardTail).
    let sb_bytes = well_formed_sb().to_bytes();
    let tail_bytes = {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&2u32.to_le_bytes());
        t[4..8].copy_from_slice(&3u32.to_le_bytes());
        t
    };
    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::Superblock, 0, 8), MAGIC).unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    builder.tag(Tag::new(true, TagType::SoftTail, 0x3FF, 8), &tail_bytes).unwrap();
    builder.commit(0).unwrap();
    storage.write_block(0, &builder.finish());

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    // SoftTail must NOT be followed; the file should be NotFound.
    let err = fs.resolve(Path::new("/deep.txt").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}
