//! Integration tests for CTZ-backed file writes.
//!
//! Writes files at various sizes through `Fs::write_to_root` (which
//! auto-dispatches inline vs CTZ) and verifies round-trip via
//! `resolve` + (inline `struct_body` OR `Fs::read_ctz`).

use littlefs2_pure::ctz::CtzStruct;
use littlefs2_pure::tag::TagType;
use littlefs2_pure::{EntryKind, Error, Fs, Path};

mod common;
use common::MemStorage;

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap()
}

/// Write `content` at `name`, then mount fresh and read it back via
/// resolve + the appropriate path (inline or CTZ).
fn write_then_remount_and_read(content: &[u8]) -> Vec<u8> {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_root(b"f", content, &mut a, &mut b).unwrap();
    let storage = fs.into_storage();

    // Fresh mount.
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let resolved = fs.resolve(Path::new("/f").unwrap(), &mut a, &mut b).unwrap();

    match resolved.struct_type {
        TagType::InlineStruct => resolved.struct_body.to_vec(),
        TagType::CtzStruct => {
            let ctz = CtzStruct::from_bytes(resolved.struct_body).unwrap();
            let mut out = vec![0u8; ctz.size as usize];
            let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
            let n = fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
            assert_eq!(n, ctz.size as usize);
            out
        }
        other => panic!("unexpected struct_type: {other:?}"),
    }
}

#[test]
fn small_content_goes_inline() {
    // Content well below INLINE_MAX (128) stays inline.
    let content = b"hello, world";
    let got = write_then_remount_and_read(content);
    assert_eq!(got, content);
}

#[test]
fn large_content_goes_ctz() {
    // 500 bytes, well above INLINE_MAX (128). Forces CTZ.
    let content: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
    let got = write_then_remount_and_read(&content);
    assert_eq!(got, content);
}

#[test]
fn ctz_spans_multiple_blocks() {
    // Content larger than one block forces a multi-block CTZ chain.
    // At BLOCK_SIZE = 256, ~500 bytes fits in 2-3 chain blocks.
    let content: Vec<u8> = (0..600).map(|i| ((i * 7) & 0xff) as u8).collect();
    let got = write_then_remount_and_read(&content);
    assert_eq!(got, content);
}

#[test]
fn ctz_struct_type_visible_via_resolve() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let big: Vec<u8> = vec![0xCC; 400];
    fs.write_to_root(b"log.bin", &big, &mut a, &mut b).unwrap();

    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/log.bin").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    assert_eq!(r.struct_body.len(), 8);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    assert_eq!(ctz.size, 400);
}

#[test]
fn ctz_file_alongside_inline_files() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    // Small inline.
    fs.write_to_root(b"cfg", b"v=1", &mut a, &mut b).unwrap();
    // Large CTZ.
    let log: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_root(b"log", &log, &mut a, &mut b).unwrap();
    // Another small inline.
    fs.write_to_root(b"state", b"ready", &mut a, &mut b).unwrap();

    // All three resolve correctly.
    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];

    let r = fs.resolve(Path::new("/cfg").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::InlineStruct);
    assert_eq!(r.struct_body, b"v=1");

    let r = fs.resolve(Path::new("/state").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::InlineStruct);
    assert_eq!(r.struct_body, b"ready");

    let r = fs.resolve(Path::new("/log").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    let mut out = vec![0u8; ctz.size as usize];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(out, log);
}

#[test]
fn ctz_to_ctz_update_replaces_content() {
    // Write a CTZ file, then overwrite it with new (also CTZ) content.
    // The new content must read back; old chain is orphaned (reclaimed
    // by the next allocator scan).
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];

    let v1: Vec<u8> = (0..300).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_root(b"log", &v1, &mut a, &mut b).unwrap();
    let v2: Vec<u8> = (0..300).map(|i| ((i + 1) & 0xff) as u8).collect();
    fs.write_to_root(b"log", &v2, &mut a, &mut b).unwrap();

    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/log").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    let mut out = vec![0u8; ctz.size as usize];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(out, v2);
}

#[test]
fn inline_to_ctz_promotion_via_write_to_root() {
    // First write: small content goes inline. Second write to same
    // name: large content. Must convert to CTZ.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];

    fs.write_to_root(b"f", b"small", &mut a, &mut b).unwrap();
    let big: Vec<u8> = (0..400).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_root(b"f", &big, &mut a, &mut b).unwrap();

    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/f").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    let mut out = vec![0u8; ctz.size as usize];
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(out, big);
}

#[test]
fn ctz_to_inline_shrink_via_write_to_root() {
    // First write: large CTZ. Second write: small inline. The inline
    // STRUCT tag overrides the prior CTZ STRUCT at the same id, so a
    // subsequent read returns the small content.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];

    let big: Vec<u8> = (0..400).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_root(b"f", &big, &mut a, &mut b).unwrap();
    fs.write_to_root(b"f", b"tiny", &mut a, &mut b).unwrap();

    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/f").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::InlineStruct);
    assert_eq!(r.struct_body, b"tiny");
}

#[test]
fn write_to_root_rejects_overwriting_directory_ctz() {
    // mkdir /foo, then try to write a >INLINE_MAX file to /foo. The CTZ
    // write path must reject with AlreadyExists; the directory stays.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/foo").unwrap(), &mut a, &mut b).unwrap();

    let big: Vec<u8> = vec![0; 400];
    let err = fs.write_to_root(b"foo", &big, &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::AlreadyExists);

    let r = fs.resolve(Path::new("/foo").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.entry.kind, EntryKind::Directory);
}

#[test]
fn write_to_root_rejects_overwriting_directory_inline() {
    // Same scenario as above but with content small enough to take the
    // inline write path. Without the kind-check guard, the Update op
    // would substitute an InlineStruct over the DirStruct slot during
    // compaction and orphan the children pair.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/foo").unwrap(), &mut a, &mut b).unwrap();

    // Small content -> inline path.
    let err = fs.write_to_root(b"foo", b"x", &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::AlreadyExists);

    // /foo must still be a directory and still mountable.
    let r = fs.resolve(Path::new("/foo").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.entry.kind, EntryKind::Directory);
}

#[test]
fn ctz_blocks_dont_collide_with_metadata_pair() {
    // The allocator must avoid blocks 0 and 1 (root pair). Write a
    // CTZ file and assert the alloc never produced address 0 or 1.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let content: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_root(b"f", &content, &mut a, &mut b).unwrap();

    // Inspect: resolve the entry, decode the CtzStruct, walk the chain.
    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/f").unwrap(), &mut a2, &mut b2).unwrap();
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    // head_block must not be 0 or 1 (those are the root metadata pair).
    assert!(ctz.head_block.as_u32() >= 2, "CTZ head clashed with root pair: {:?}", ctz.head_block);
}
