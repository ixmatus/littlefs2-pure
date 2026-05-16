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

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let resolved = fs.resolve(Path::new("/deep.txt").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(resolved.entry.name, b"deep.txt");
    assert_eq!(resolved.struct_body, b"found via hardtail");
}

#[test]
fn list_root_enumerates_across_hardtail() {
    // Root pair (0,1) holds the superblock + one file "head.txt" + a
    // HardTail pointing at (2,3). Continuation pair (2,3) holds
    // "tail.txt". list_root must emit both entries in chain order.
    let mut storage = MemStorage::new();

    let continuation = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"tail.txt",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: b"in continuation",
        }],
        MemStorage::BLOCK_SIZE,
    );
    storage.write_block(2, &continuation);

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
    // One regular file living in the head pair.
    builder.tag(Tag::new(true, TagType::Create, 1, 0), &[]).unwrap();
    builder.tag(Tag::new(true, TagType::RegularFile, 1, 8), b"head.txt").unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 1, 7), b"in head").unwrap();
    builder.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail_bytes).unwrap();
    builder.commit(0).unwrap();
    storage.write_block(0, &builder.finish());

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut names: Vec<Vec<u8>> = Vec::new();
    let n = fs
        .list_root(
            |e| {
                names.push(e.name.to_vec());
            },
            &mut a,
            &mut b,
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(names[0], b"head.txt");
    assert_eq!(names[1], b"tail.txt");
}

#[test]
fn list_dir_enumerates_subdir_across_hardtail() {
    // Root pair holds a DirStruct for "/audit" pointing at the audit
    // head pair (2,3). The audit head pair holds one file plus a
    // HardTail pointing at (4,5), which holds a second file. list_dir
    // on "/audit" must surface both files.
    let mut storage = MemStorage::new();

    // Audit continuation pair at (4,5): one file.
    let cont = build_directory_block(
        1,
        &[DirEntrySpec {
            id: 0,
            name: b"entry-002",
            name_type: TagType::RegularFile,
            struct_type: TagType::InlineStruct,
            struct_body: b"two",
        }],
        MemStorage::BLOCK_SIZE,
    );
    storage.write_block(4, &cont);

    // Audit head pair at (2,3): one file + HardTail to (4,5).
    let tail_to_cont = {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&4u32.to_le_bytes());
        t[4..8].copy_from_slice(&5u32.to_le_bytes());
        t
    };
    let mut audit_head = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    audit_head.tag(Tag::new(true, TagType::Create, 0, 0), &[]).unwrap();
    audit_head.tag(Tag::new(true, TagType::RegularFile, 0, 9), b"entry-001").unwrap();
    audit_head.tag(Tag::new(true, TagType::InlineStruct, 0, 3), b"one").unwrap();
    audit_head.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail_to_cont).unwrap();
    audit_head.commit(0).unwrap();
    storage.write_block(2, &audit_head.finish());

    // Root pair at (0,1): superblock + DirStruct for "audit" -> (2,3).
    let sb_bytes = well_formed_sb().to_bytes();
    let dir_to_audit = {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&2u32.to_le_bytes());
        t[4..8].copy_from_slice(&3u32.to_le_bytes());
        t
    };
    let mut root = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    root.tag(Tag::new(true, TagType::Superblock, 0, 8), MAGIC).unwrap();
    root.tag(Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    root.tag(Tag::new(true, TagType::Create, 1, 0), &[]).unwrap();
    root.tag(Tag::new(true, TagType::Directory, 1, 5), b"audit").unwrap();
    root.tag(Tag::new(true, TagType::DirStruct, 1, 8), &dir_to_audit).unwrap();
    root.commit(0).unwrap();
    storage.write_block(0, &root.finish());

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut names: Vec<Vec<u8>> = Vec::new();
    let n = fs
        .list_dir(
            Path::new("/audit").unwrap(),
            |e| {
                names.push(e.name.to_vec());
            },
            &mut a,
            &mut b,
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(names[0], b"entry-001");
    assert_eq!(names[1], b"entry-002");
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

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    // SoftTail must NOT be followed; the file should be NotFound.
    let err = fs.resolve(Path::new("/deep.txt").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

/// Build an image where `/d` is a directory whose head pair (2,3) is
/// empty but HardTail-threads to a continuation pair (4,5). The
/// continuation holds `cont_entries`.
fn build_hardtail_dir_image(cont_entries: &[DirEntrySpec<'_>]) -> MemStorage {
    let mut storage = MemStorage::new();

    // Continuation pair (4,5).
    let cont = build_directory_block(1, cont_entries, MemStorage::BLOCK_SIZE);
    storage.write_block(4, &cont);

    // Head pair (2,3): no entries, just a HardTail to (4,5).
    let tail_to_cont = {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&4u32.to_le_bytes());
        t[4..8].copy_from_slice(&5u32.to_le_bytes());
        t
    };
    let mut head = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    head.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &tail_to_cont).unwrap();
    head.commit(0).unwrap();
    storage.write_block(2, &head.finish());

    // Root pair (0,1): superblock + DirStruct "d" -> (2,3).
    let sb_bytes = well_formed_sb().to_bytes();
    let dir_to_head = {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&2u32.to_le_bytes());
        t[4..8].copy_from_slice(&3u32.to_le_bytes());
        t
    };
    let mut root = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    root.tag(Tag::new(true, TagType::Superblock, 0, 8), MAGIC).unwrap();
    root.tag(Tag::new(true, TagType::InlineStruct, 0, 24), &sb_bytes).unwrap();
    root.tag(Tag::new(true, TagType::Create, 1, 0), &[]).unwrap();
    root.tag(Tag::new(true, TagType::Directory, 1, 1), b"d").unwrap();
    root.tag(Tag::new(true, TagType::DirStruct, 1, 8), &dir_to_head).unwrap();
    root.commit(0).unwrap();
    storage.write_block(0, &root.finish());
    storage
}

#[test]
fn rmdir_rejects_directory_with_entries_in_hardtail_continuation() {
    // /d's head pair is empty, but a HardTail continuation holds a
    // live file. rmdir must count across the chain and reject with
    // NotEmpty rather than orphan the continuation pair.
    let storage = build_hardtail_dir_image(&[DirEntrySpec {
        id: 0,
        name: b"buried.txt",
        name_type: TagType::RegularFile,
        struct_type: TagType::InlineStruct,
        struct_body: b"still here",
    }]);
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let err = fs.rmdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotEmpty);
}

#[test]
fn rmdir_accepts_directory_with_empty_hardtail_chain() {
    // /d's head pair is empty and its HardTail continuation is also
    // empty. The directory is genuinely empty across the whole chain,
    // so rmdir must succeed.
    let storage = build_hardtail_dir_image(&[]);
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.rmdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    assert!(!fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).unwrap());
}
