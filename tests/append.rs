//! Integration tests for `Fs::append_to_path`.
//!
//! Covers the SMIL audit logger's "append to /audit/log.bin" workflow
//! at various sizes (inline-only, inline-then-CTZ, CTZ-only).

use littlefs2_pure::ctz::CtzStruct;
use littlefs2_pure::tag::TagType;
use littlefs2_pure::{Error, Fs, Path};

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

fn read_content(fs: &mut Fs<MemStorage>, path: &str) -> Vec<u8> {
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new(path).unwrap(), &mut buf_a, &mut buf_b).unwrap();
    match r.struct_type {
        TagType::InlineStruct => r.struct_body.to_vec(),
        TagType::CtzStruct => {
            let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
            let mut out = vec![0u8; ctz.size as usize];
            let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
            fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
            out
        }
        _ => panic!("unexpected struct_type"),
    }
}

#[test]
fn append_creates_file_if_missing() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];
    fs.append_to_path(
        Path::new("/log").unwrap(),
        b"first entry",
        &mut content_scratch,
        &mut a,
        &mut b,
    )
    .unwrap();
    assert_eq!(read_content(&mut fs, "/log"), b"first entry");
}

#[test]
fn append_inline_grows_inline() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];

    fs.append_to_path(Path::new("/log").unwrap(), b"AAA", &mut content_scratch, &mut a, &mut b)
        .unwrap();
    fs.append_to_path(Path::new("/log").unwrap(), b"BBB", &mut content_scratch, &mut a, &mut b)
        .unwrap();
    fs.append_to_path(Path::new("/log").unwrap(), b"CCC", &mut content_scratch, &mut a, &mut b)
        .unwrap();

    assert_eq!(read_content(&mut fs, "/log"), b"AAABBBCCC");
}

#[test]
fn append_promotes_inline_to_ctz() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];

    fs.append_to_path(Path::new("/log").unwrap(), b"head_", &mut content_scratch, &mut a, &mut b)
        .unwrap();

    // Add 200 bytes; total ~205 > INLINE_MAX (128) so the file
    // promotes to CTZ on this append.
    let chunk: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &chunk, &mut content_scratch, &mut a, &mut b)
        .unwrap();

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/log").unwrap(), &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);

    let mut expected = Vec::new();
    expected.extend_from_slice(b"head_");
    expected.extend_from_slice(&chunk);
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn append_ctz_to_ctz_extends_content() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];

    let v1: Vec<u8> = (0..300).map(|i| (i & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &v1, &mut content_scratch, &mut a, &mut b)
        .unwrap();
    let v2: Vec<u8> = (300..500).map(|i| (i & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &v2, &mut content_scratch, &mut a, &mut b)
        .unwrap();

    let mut expected = v1.clone();
    expected.extend_from_slice(&v2);
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn append_rejects_directory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let err = fs
        .append_to_path(Path::new("/d").unwrap(), b"x", &mut content_scratch, &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::AlreadyExists);
}

#[test]
fn append_rejects_undersized_content_scratch() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut tiny = [0u8; 4];
    let err = fs
        .append_to_path(Path::new("/log").unwrap(), b"hello world", &mut tiny, &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::OutOfRange);
}

#[test]
fn append_into_subdirectory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    fs.append_to_path(
        Path::new("/audit/log").unwrap(),
        b"entry0\n",
        &mut content_scratch,
        &mut a,
        &mut b,
    )
    .unwrap();
    fs.append_to_path(
        Path::new("/audit/log").unwrap(),
        b"entry1\n",
        &mut content_scratch,
        &mut a,
        &mut b,
    )
    .unwrap();
    assert_eq!(read_content(&mut fs, "/audit/log"), b"entry0\nentry1\n");
}

#[test]
fn appends_survive_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let mut cs = [0u8; 1024];
        for i in 0..5u32 {
            let entry = format!("e{i};");
            fs.append_to_path(
                Path::new("/log").unwrap(),
                entry.as_bytes(),
                &mut cs,
                &mut a,
                &mut b,
            )
            .unwrap();
        }
        storage = fs.into_storage();
    }
    // Remount, read back.
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(read_content(&mut fs, "/log"), b"e0;e1;e2;e3;e4;");
}
