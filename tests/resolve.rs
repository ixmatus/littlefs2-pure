//! Integration tests for `Fs::resolve`.
//!
//! Builds an image with a known directory tree, mounts it, and walks
//! arbitrary absolute paths to leaf entries. Covers:
//!
//! - root-level inline file
//! - root-level directory
//! - nested file inside a subdirectory
//! - missing leaf component
//! - missing intermediate component
//! - intermediate component that is a regular file (not a directory)

use littlefs2_pure::tag::TagType;
use littlefs2_pure::{
    BlockAddress, BlockPair, EntryKind, Error, Fs, Path, Superblock, DISK_VERSION,
};

mod common;
use common::{build_directory_block, DirEntrySpec, MemStorage};

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

/// Build an image with:
///   block 0    -> superblock (also acts as the root pair's "first slot")
///   block 1    -> empty alternate for root
///   block 2    -> subdirectory "logs"
///   block 3    -> empty alternate for "logs"
///
/// Root holds:
///   - config.toml  (regular file, inline)
///   - logs/         (directory pointing at pair (2, 3))
///
/// "logs/" holds:
///   - today.txt (regular file, inline body "hello logs")
fn build_image_with_subdir() -> MemStorage {
    let mut storage = MemStorage::new();

    // Subdirectory at pair (2, 3):
    let logs_block = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"today.txt",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: b"hello logs",
        }],
        MemStorage::BLOCK_SIZE,
    );
    storage.write_block(2, &logs_block);

    // Root pair (0, 1): superblock-plus-children commit.
    // The build_superblock_block helper produces a single-commit pair
    // containing only the superblock magic + INLINESTRUCT. We need to
    // *extend* that block with two more entries (config.toml and logs/).
    // The simplest path: build a custom root block with all four entries
    // (superblock magic, superblock struct, config.toml NAME+STRUCT,
    // logs/ NAME+STRUCT) in one commit.
    let sb_bytes = well_formed_sb().to_bytes();
    let logs_struct = {
        let mut s = [0u8; 8];
        s[0..4].copy_from_slice(&2u32.to_le_bytes());
        s[4..8].copy_from_slice(&3u32.to_le_bytes());
        s
    };
    let mut builder = common::BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    // Superblock NAME (magic) at id 0
    builder
        .tag(littlefs2_pure::Tag::new(true, TagType::Superblock, 0, 8), littlefs2_pure::MAGIC)
        .unwrap();
    // Superblock INLINESTRUCT at id 0
    builder.tag(littlefs2_pure::Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    // config.toml NAME at id 1
    builder
        .tag(
            littlefs2_pure::Tag::new(true, TagType::RegularFile, 1, b"config.toml".len() as u16),
            b"config.toml",
        )
        .unwrap();
    // config.toml InlineStruct (content) at id 1
    builder
        .tag(
            littlefs2_pure::Tag::new(true, TagType::InlineStruct, 1, b"version=2".len() as u16),
            b"version=2",
        )
        .unwrap();
    // logs/ NAME at id 2
    builder
        .tag(littlefs2_pure::Tag::new(true, TagType::Directory, 2, b"logs".len() as u16), b"logs")
        .unwrap();
    // logs/ DirStruct at id 2
    builder.tag(littlefs2_pure::Tag::new(true, TagType::DirStruct, 2, 8), &logs_struct).unwrap();
    builder.commit(0).unwrap();
    storage.write_block(0, &builder.finish());

    storage
}

#[test]
fn resolve_root_level_file() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let resolved = fs.resolve(Path::new("/config.toml").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(resolved.entry.kind, EntryKind::RegularFile);
    assert_eq!(resolved.entry.name, b"config.toml");
    assert_eq!(resolved.struct_type, TagType::InlineStruct);
    assert_eq!(resolved.struct_body, b"version=2");
}

#[test]
fn resolve_root_level_directory() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let resolved = fs.resolve(Path::new("/logs").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(resolved.entry.kind, EntryKind::Directory);
    assert_eq!(resolved.struct_type, TagType::DirStruct);
    // The DirStruct body encodes the pair address (2, 3).
    let a_addr = u32::from_le_bytes(resolved.struct_body[0..4].try_into().unwrap());
    let b_addr = u32::from_le_bytes(resolved.struct_body[4..8].try_into().unwrap());
    assert_eq!((a_addr, b_addr), (2, 3));
}

#[test]
fn resolve_nested_file() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let resolved = fs.resolve(Path::new("/logs/today.txt").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(resolved.entry.kind, EntryKind::RegularFile);
    assert_eq!(resolved.entry.name, b"today.txt");
    assert_eq!(resolved.struct_body, b"hello logs");
    // The pair of the final entry is (2, 3): the subdirectory's pair.
    assert_eq!(resolved.pair.a, BlockAddress::new(2));
}

#[test]
fn resolve_missing_leaf_returns_not_found() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let err = fs.resolve(Path::new("/missing.txt").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn resolve_missing_intermediate_returns_not_found() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let err = fs.resolve(Path::new("/nosuch/foo.txt").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn resolve_intermediate_is_file_returns_not_found() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    // config.toml is a regular file; treating it as a directory must fail.
    let err = fs.resolve(Path::new("/config.toml/foo").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn resolve_root_path_is_invalid() {
    let storage = build_image_with_subdir();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let err = fs.resolve(Path::new("/").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::InvalidPath);
}

// Silence the unused-import warning on this no-features build configuration.
#[allow(dead_code)]
fn _suppress_warnings() {
    let _ = BlockPair::new(BlockAddress::new(0), BlockAddress::new(1));
}
