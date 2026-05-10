//! Integration tests for the compaction path of `write_inline_to_root`.
//!
//! Writes enough small files to exhaust the active block, forcing the
//! kernel to compact the live state onto the alternate block. Verifies
//! all entries remain readable, and that the metadata pair's active
//! block has flipped after compaction.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Fs, Path};

mod common;
use common::MemStorage;

/// Write a sequence of small files to fill (and overflow) the active
/// block. Returns the names of the files that were successfully
/// written.
fn fill_root(fs: &mut Fs<MemStorage>) -> Vec<String> {
    let mut written = Vec::new();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    for i in 0..30u32 {
        let name = format!("f{i:02}");
        let content = format!("=={i:02}");
        if fs.write_inline_to_root(name.as_bytes(), content.as_bytes(), &mut a, &mut b).is_ok() {
            written.push(name);
        } else {
            break;
        }
    }
    written
}

#[test]
fn compaction_triggers_when_block_fills() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let written = fill_root(&mut fs);
    assert!(written.len() > 5, "should have packed in at least a handful before any limit");

    // Every successfully-written name should still resolve.
    for name in &written {
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let path = format!("/{name}");
        let r = fs.resolve(Path::new(&path).unwrap(), &mut a, &mut b).unwrap();
        let expected = format!("=={}", &name[1..]);
        assert_eq!(r.struct_body, expected.as_bytes(), "content mismatch for {name}");
    }
}

#[test]
fn compaction_survives_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let written = {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let names = fill_root(&mut fs);
        storage = fs.into_storage();
        names
    };

    // Force at least one compaction (sanity: the fill loop should
    // have triggered several given MemStorage::BLOCK_SIZE = 256).
    assert!(written.len() > 8, "test geometry must produce a compaction");

    // Remount fresh and verify everything is still there.
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    for name in &written {
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let path = format!("/{name}");
        let r = fs.resolve(Path::new(&path).unwrap(), &mut a, &mut b).unwrap();
        let expected = format!("=={}", &name[1..]);
        assert_eq!(r.struct_body, expected.as_bytes());
    }
}

#[test]
fn compaction_bumps_revision_counter() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();

    // After format, block 0 has revision 1, block 1 is erased.
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let _written = fill_root(&mut fs);
    storage = fs.into_storage();

    // Check both blocks' revisions on raw storage.
    let rev0 = read_revision(&mut storage, 0);
    let rev1 = read_revision(&mut storage, 1);
    // After compaction, block 1 (or whichever was alternate) holds a
    // higher revision than 1.
    assert!(
        rev0 > 1 || rev1 > 1,
        "after compaction at least one block should have revision > 1; got {rev0}, {rev1}"
    );
}

fn read_revision(storage: &mut MemStorage, block: u32) -> u32 {
    let mut buf = [0u8; 4];
    storage.read(block, 0, &mut buf).unwrap();
    u32::from_le_bytes(buf)
}
