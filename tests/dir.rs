//! Integration tests for `Fs::read_pair` and the directory entry iterator.

use littlefs2_pure::tag::TagType;
use littlefs2_pure::{entries, lookup, BlockAddress, BlockPair, EntryKind, Fs};

mod common;
use common::{build_directory_block, DirEntrySpec, MemStorage};

#[test]
fn dir_iter_yields_each_name_tag() {
    // Build a directory block with three entries:
    //   id 0: regular file "config.toml" (inline struct body)
    //   id 1: regular file "scratch.bin" (CTZ struct body, 12 bytes)
    //   id 2: directory   "logs"        (DirStruct body, 8 bytes)
    let dir_struct = vec![10u8, 0, 0, 0, 11, 0, 0, 0]; // pair (10, 11)
    let ctz_struct = vec![0u8; 12];
    let inline_struct = vec![0xABu8; 4];
    let dir_block = build_directory_block(
        1,
        &[
            DirEntrySpec {
                id: 0,
                name: b"config.toml",
                name_type: TagType::RegularFile,
                struct_type: TagType::InlineStruct,
                struct_body: &inline_struct,
            },
            DirEntrySpec {
                id: 1,
                name: b"scratch.bin",
                name_type: TagType::RegularFile,
                struct_type: TagType::CtzStruct,
                struct_body: &ctz_struct,
            },
            DirEntrySpec {
                id: 2,
                name: b"logs",
                name_type: TagType::Directory,
                struct_type: TagType::DirStruct,
                struct_body: &dir_struct,
            },
        ],
        MemStorage::BLOCK_SIZE,
    );

    let mut storage = MemStorage::new();
    storage.write_block(2, &dir_block);

    // No superblock; we mount via a contrived superblock at blocks 0/1
    // pointing at the directory at blocks 2/3.
    let sb = littlefs2_pure::Superblock {
        version: littlefs2_pure::DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &common::build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    // Read the directory at (2, 3).
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let pair = fs
        .read_pair(BlockPair::new(BlockAddress::new(2), BlockAddress::new(3)), &mut a, &mut b)
        .unwrap();

    let collected: Vec<_> = entries(&pair).collect();
    assert_eq!(collected.len(), 3);

    assert_eq!(collected[0].id, 0);
    assert_eq!(collected[0].name, b"config.toml");
    assert_eq!(collected[0].kind, EntryKind::RegularFile);

    assert_eq!(collected[1].id, 1);
    assert_eq!(collected[1].name, b"scratch.bin");
    assert_eq!(collected[1].kind, EntryKind::RegularFile);

    assert_eq!(collected[2].id, 2);
    assert_eq!(collected[2].name, b"logs");
    assert_eq!(collected[2].kind, EntryKind::Directory);
}

#[test]
fn dir_iter_empty_pair_yields_nothing() {
    let dir_block = {
        let mut b = common::BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
        b.commit(0).unwrap();
        b.finish()
    };

    let mut storage = MemStorage::new();
    storage.write_block(2, &dir_block);

    let sb = littlefs2_pure::Superblock {
        version: littlefs2_pure::DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &common::build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let pair = fs
        .read_pair(BlockPair::new(BlockAddress::new(2), BlockAddress::new(3)), &mut a, &mut b)
        .unwrap();
    assert_eq!(entries(&pair).count(), 0);
}

#[test]
fn lookup_finds_inline_file() {
    let inline_body = b"hello world".to_vec();
    let dir_block = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"greet.txt",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: &inline_body,
        }],
        MemStorage::BLOCK_SIZE,
    );

    let mut storage = MemStorage::new();
    storage.write_block(2, &dir_block);
    let sb = littlefs2_pure::Superblock {
        version: littlefs2_pure::DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &common::build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let pair = fs
        .read_pair(BlockPair::new(BlockAddress::new(2), BlockAddress::new(3)), &mut a, &mut b)
        .unwrap();

    let resolved = lookup(&pair, b"greet.txt").unwrap();
    assert_eq!(resolved.entry.id, 0);
    assert_eq!(resolved.entry.kind, EntryKind::RegularFile);
    assert_eq!(resolved.struct_type, TagType::InlineStruct);
    // For an inline file, the struct body *is* the file content.
    assert_eq!(resolved.struct_body, b"hello world");
}

#[test]
fn lookup_finds_directory() {
    let dir_struct = vec![10u8, 0, 0, 0, 11, 0, 0, 0];
    let dir_block = build_directory_block(
        1,
        &[DirEntrySpec {
            // Sole entry in this pair, so it must occupy id 0
            // (splice-aware lookup expects contiguous ids).
            id: 0,
            name: b"subdir",
            name_type: TagType::Directory,
            struct_type: TagType::DirStruct,
            struct_body: &dir_struct,
        }],
        MemStorage::BLOCK_SIZE,
    );

    let mut storage = MemStorage::new();
    storage.write_block(2, &dir_block);
    let sb = littlefs2_pure::Superblock {
        version: littlefs2_pure::DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &common::build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let pair = fs
        .read_pair(BlockPair::new(BlockAddress::new(2), BlockAddress::new(3)), &mut a, &mut b)
        .unwrap();

    let resolved = lookup(&pair, b"subdir").unwrap();
    assert_eq!(resolved.entry.id, 0);
    assert_eq!(resolved.entry.kind, EntryKind::Directory);
    assert_eq!(resolved.struct_type, TagType::DirStruct);
    // Decode the pair address from the DirStruct body.
    let a_addr = u32::from_le_bytes(resolved.struct_body[0..4].try_into().unwrap());
    let b_addr = u32::from_le_bytes(resolved.struct_body[4..8].try_into().unwrap());
    assert_eq!((a_addr, b_addr), (10, 11));
}

#[test]
fn lookup_missing_name_returns_none() {
    let inline = b"x".to_vec();
    let block = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"present.txt",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: &inline,
        }],
        MemStorage::BLOCK_SIZE,
    );

    let mut storage = MemStorage::new();
    storage.write_block(2, &block);
    let sb = littlefs2_pure::Superblock {
        version: littlefs2_pure::DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &common::build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let pair = fs
        .read_pair(BlockPair::new(BlockAddress::new(2), BlockAddress::new(3)), &mut a, &mut b)
        .unwrap();
    assert!(lookup(&pair, b"absent.txt").is_none());
}

#[test]
fn dir_iter_skips_non_name_tags() {
    // A block with only a CCRC and no NAME tags should yield no entries.
    let block = {
        let mut b = common::BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
        // Emit a single non-NAME tag (an InlineStruct at id 0 with junk
        // body), then commit.
        let body = vec![1u8, 2, 3, 4];
        let tag = littlefs2_pure::tag::Tag::new(true, TagType::InlineStruct, 0, 4);
        b.tag(tag, &body).unwrap();
        b.commit(0).unwrap();
        b.finish()
    };

    let mut storage = MemStorage::new();
    storage.write_block(2, &block);
    let sb = littlefs2_pure::Superblock {
        version: littlefs2_pure::DISK_VERSION,
        block_size: MemStorage::BLOCK_SIZE as u32,
        block_count: MemStorage::BLOCK_COUNT,
        name_max: 0,
        file_max: 0,
        attr_max: 0,
    };
    storage.write_block(0, &common::build_superblock_block(&sb, MemStorage::BLOCK_SIZE));

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let pair = fs
        .read_pair(BlockPair::new(BlockAddress::new(2), BlockAddress::new(3)), &mut a, &mut b)
        .unwrap();
    assert_eq!(entries(&pair).count(), 0);
}
