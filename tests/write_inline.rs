//! Integration tests for `Fs::write_inline_to_root`.
//!
//! Format an image, write inline files at the root, then read them
//! back via the existing resolve + struct_body path. End-to-end
//! write+read round-trip on a Storage device.

use littlefs2_pure::tag::TagType;
use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::MemStorage;

#[test]
fn write_then_read_single_file() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    fs.write_inline_to_root(b"hello.txt", b"hello, world!", &mut a, &mut b).unwrap();

    // Read it back.
    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let resolved = fs.resolve(Path::new("/hello.txt").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(resolved.entry.name, b"hello.txt");
    assert_eq!(resolved.struct_type, TagType::InlineStruct);
    assert_eq!(resolved.struct_body, b"hello, world!");
}

#[test]
fn write_multiple_files_each_at_next_id() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    fs.write_inline_to_root(b"a.txt", b"AAA", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"b.txt", b"BBBB", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"c.txt", b"CCCCC", &mut a, &mut b).unwrap();

    for (name, expected) in
        [(b"a.txt", &b"AAA"[..]), (b"b.txt", &b"BBBB"[..]), (b"c.txt", &b"CCCCC"[..])]
    {
        let mut a2 = common::make_buffer();
        let mut b2 = common::make_buffer();
        let path_str = std::str::from_utf8(name).unwrap();
        let path_with_slash = format!("/{path_str}");
        let r = fs.resolve(Path::new(&path_with_slash).unwrap(), &mut a2, &mut b2).unwrap();
        assert_eq!(r.struct_body, expected, "content mismatch for {path_str}");
    }
}

#[test]
fn write_duplicate_name_updates_content() {
    // write_inline_to_root has upsert semantics: writing to an existing
    // name replaces the content. Verifies the second write succeeds and
    // a read returns the second content.
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_inline_to_root(b"cfg", b"v1", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"cfg", b"v2-updated", &mut a, &mut b).unwrap();

    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let r = fs.resolve(Path::new("/cfg").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_body, b"v2-updated");
}

#[test]
fn many_updates_to_same_name_survive_remount() {
    // Hammer the same name with many updates. Each update appends a
    // new InlineStruct (small commit); eventually the block fills and
    // compaction GCs to the alternate. After every update, the latest
    // content must be readable. Then a fresh mount must agree.
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    let final_content;
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        for i in 0..40u32 {
            let v = format!("rev{i:03}");
            fs.write_inline_to_root(b"state", v.as_bytes(), &mut a, &mut b).unwrap();
        }
        final_content = b"rev039".to_vec();

        // Confirm latest content visible to the live Fs.
        let mut a2 = common::make_buffer();
        let mut b2 = common::make_buffer();
        let r = fs.resolve(Path::new("/state").unwrap(), &mut a2, &mut b2).unwrap();
        assert_eq!(r.struct_body, &final_content[..]);
        storage = fs.into_storage();
    }

    // Fresh mount sees the final content too (durability check).
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs.resolve(Path::new("/state").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, &final_content[..]);
}

#[test]
fn write_overflowing_block_rejected() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // Pack writes until one overflows.
    let mut wrote = 0;
    for i in 0..100u32 {
        let name = format!("file_{i:03}.txt");
        let content = vec![b'x'; 64]; // 64 bytes of content
        let r = fs.write_inline_to_root(name.as_bytes(), &content, &mut a, &mut b);
        match r {
            Ok(()) => {
                wrote += 1;
            }
            Err(Error::OutOfRange) => break,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(wrote >= 1, "should have written at least one before overflow");
    assert!(wrote < 100, "should have hit OutOfRange before 100 files");
}

#[test]
fn writes_survive_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        fs.write_inline_to_root(b"persist.cfg", b"alive", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // Fresh mount on the same device.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs.resolve(Path::new("/persist.cfg").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"alive");
}
