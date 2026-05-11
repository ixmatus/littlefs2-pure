//! Integration tests for splice-aware directory enumeration.
//!
//! Builds metadata blocks with explicit Create/Delete tag sequences and
//! asserts `dir::live_entries` yields the correct surviving entries with
//! the correct renumbered ids.

use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{live_entries, BlockAddress, BlockPair, EntryKind, Error, MetadataPair};

mod common;
use common::{BlockBuilder, MemStorage};

/// Helper: emit a NAME tag at the given id and kind, with the given body.
fn name_tag(builder: &mut BlockBuilder, id: u16, kind: EntryKind, name: &[u8]) {
    let ty = match kind {
        EntryKind::RegularFile => TagType::RegularFile,
        EntryKind::Directory => TagType::Directory,
        _ => unreachable!("EntryKind has only two variants in v2"),
    };
    builder.tag(Tag::new(true, ty, id, name.len() as u16), name).unwrap();
}

/// Helper: emit a Create tag at the given id (no body, zero length).
fn create_tag(builder: &mut BlockBuilder, id: u16) {
    builder.tag(Tag::new(true, TagType::Create, id, 0), &[]).unwrap();
}

/// Helper: emit a Delete tag at the given id (special length sentinel,
/// no body).
fn delete_tag(builder: &mut BlockBuilder, id: u16) {
    builder.tag(Tag::new(true, TagType::Delete, id, 0x3FF), &[]).unwrap();
}

/// Collect live entries from a built block into a Vec<(id, name, kind)>.
fn collect_live(block: &[u8]) -> Vec<(u16, Vec<u8>, EntryKind)> {
    let empty = vec![0xFFu8; block.len()];
    let pair =
        MetadataPair::parse(BlockAddress::new(0), block, BlockAddress::new(1), &empty).unwrap();
    let mut out = Vec::new();
    live_entries(&pair, |e| {
        out.push((e.id, e.name.to_vec(), e.kind));
        Ok::<(), Error>(())
    })
    .unwrap();
    out
}

#[test]
fn create_then_name_yields_entry() {
    let mut b = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    create_tag(&mut b, 0);
    name_tag(&mut b, 0, EntryKind::RegularFile, b"alpha");
    b.commit(0).unwrap();
    let block = b.finish();

    let entries = collect_live(&block);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], (0u16, b"alpha".to_vec(), EntryKind::RegularFile));
}

#[test]
fn create_then_delete_yields_nothing() {
    let mut b = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    create_tag(&mut b, 0);
    name_tag(&mut b, 0, EntryKind::RegularFile, b"alpha");
    delete_tag(&mut b, 0);
    b.commit(0).unwrap();
    let block = b.finish();

    let entries = collect_live(&block);
    assert_eq!(entries.len(), 0);
}

#[test]
fn delete_middle_shifts_subsequent_ids_down() {
    // Create a, b, c with ids 0, 1, 2. Then delete b (id 1).
    // c should renumber from id 2 to id 1.
    let mut bld = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    create_tag(&mut bld, 0);
    name_tag(&mut bld, 0, EntryKind::RegularFile, b"a");
    create_tag(&mut bld, 1);
    name_tag(&mut bld, 1, EntryKind::RegularFile, b"b");
    create_tag(&mut bld, 2);
    name_tag(&mut bld, 2, EntryKind::RegularFile, b"c");
    delete_tag(&mut bld, 1);
    bld.commit(0).unwrap();
    let block = bld.finish();

    let entries = collect_live(&block);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], (0u16, b"a".to_vec(), EntryKind::RegularFile));
    assert_eq!(entries[1], (1u16, b"c".to_vec(), EntryKind::RegularFile));
}

#[test]
fn create_after_delete_reuses_renumbered_slot() {
    // Create a (id 0), b (id 1). Delete a. Create d (id 1, which is one
    // past current count after the delete).
    // After delete: b is at id 0, count = 1.
    // After create d at id 1: b stays at 0, d at 1.
    let mut bld = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    create_tag(&mut bld, 0);
    name_tag(&mut bld, 0, EntryKind::RegularFile, b"a");
    create_tag(&mut bld, 1);
    name_tag(&mut bld, 1, EntryKind::RegularFile, b"b");
    delete_tag(&mut bld, 0);
    create_tag(&mut bld, 1);
    name_tag(&mut bld, 1, EntryKind::RegularFile, b"d");
    bld.commit(0).unwrap();
    let block = bld.finish();

    let entries = collect_live(&block);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], (0u16, b"b".to_vec(), EntryKind::RegularFile));
    assert_eq!(entries[1], (1u16, b"d".to_vec(), EntryKind::RegularFile));
}

#[test]
fn splice_across_commits() {
    // Commit 1: create a, b. Commit 2: delete a.
    // Expected: only b remains, at id 0.
    let mut bld = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    create_tag(&mut bld, 0);
    name_tag(&mut bld, 0, EntryKind::RegularFile, b"a");
    create_tag(&mut bld, 1);
    name_tag(&mut bld, 1, EntryKind::RegularFile, b"b");
    bld.commit(0).unwrap();
    // Commit 2:
    delete_tag(&mut bld, 0);
    bld.commit(1).unwrap();
    let block = bld.finish();

    let entries = collect_live(&block);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], (0u16, b"b".to_vec(), EntryKind::RegularFile));
}

#[test]
fn empty_pair_yields_zero_entries() {
    let mut b = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    b.commit(0).unwrap();
    let block = b.finish();
    let entries = collect_live(&block);
    assert_eq!(entries.len(), 0);
}

#[test]
fn directory_entry_handled() {
    let mut b = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    create_tag(&mut b, 0);
    name_tag(&mut b, 0, EntryKind::Directory, b"sub");
    b.commit(0).unwrap();
    let block = b.finish();
    let entries = collect_live(&block);
    assert_eq!(entries[0].2, EntryKind::Directory);
}

// Silence unused warning for the BlockPair re-export.
#[allow(dead_code)]
fn _suppress() {
    let _ = BlockPair::new(BlockAddress::new(0), BlockAddress::new(1));
}
