//! Conformance against C-reference-generated vectors.
//!
//! Each `tests/vectors/NN_*.bin` file is a 2 KiB image produced by
//! `tools/gen_vectors/main.c` running the C littlefs reference at a
//! pinned version (see `tools/gen_vectors/littlefs/lfs.h`'s
//! `LFS_DISK_VERSION`). The scenarios cover:
//!
//! - `01_empty_format`: fresh `lfs_format` with no entries.
//! - `02_single_inline`: one tiny file at root (`/cfg`, 15 bytes).
//! - `03_single_ctz`: one CTZ-backed file at root (`/payload.bin`,
//!   500 bytes, content `i & 0xff` for `i = 0..500`).
//! - `04_nested_dir`: `/audit/` directory containing
//!   `/audit/log` with the body `entry-0001;`.
//!
//! The runner loads each vector into a `MemStorage`, mounts with our
//! reader, and asserts the expected (name, kind, content) tuples.
//!
//! # Regenerating
//!
//! ```text
//! make -C tools/gen_vectors vectors
//! ```
//!
//! requires a host C toolchain (`cc`). The bin files are committed at
//! rest; CI does not invoke the C compiler.
//!
//! Bit-level cross check rather than self-consistency: if our writer
//! and reader both have a coordinated bug, the property tests miss
//! it. The C-reference images keep us honest at the byte boundary.

use littlefs2_pure::ctz::CtzStruct;
use littlefs2_pure::tag::TagType;
use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::MemStorage;

/// Load a vector file's bytes into a fresh `MemStorage`. Panics on
/// I/O error (these files are committed; a missing one is a setup
/// bug, not a flake).
fn load(vector_name: &str) -> MemStorage {
    let path = format!("tests/vectors/{vector_name}");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert_eq!(
        bytes.len(),
        MemStorage::BLOCK_SIZE * MemStorage::BLOCK_COUNT as usize,
        "vector {vector_name} is the wrong size; regenerate with `make -C tools/gen_vectors vectors`"
    );
    let mut s = MemStorage::new();
    s.data.copy_from_slice(&bytes);
    s
}

fn mount(s: MemStorage) -> Fs<MemStorage> {
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    Fs::mount(s, &mut buf_a, &mut buf_b).expect("mount conformance vector")
}

fn read_inline<'a>(fs: &mut Fs<MemStorage>, p: &str, scratch: &'a mut [u8]) -> &'a [u8] {
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new(p).unwrap(), &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(r.struct_type, TagType::InlineStruct);
    let n = r.struct_body.len();
    scratch[..n].copy_from_slice(r.struct_body);
    &scratch[..n]
}

fn read_ctz_all(fs: &mut Fs<MemStorage>, p: &str) -> Vec<u8> {
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let size = fs.size_of(Path::new(p).unwrap(), &mut buf_a, &mut buf_b).unwrap();
    let mut out = vec![0u8; size as usize];
    let n = fs.read_at_path(Path::new(p).unwrap(), 0, &mut out, &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(n, size as usize);
    out
}

#[test]
fn vector_01_empty_format_mounts_clean() {
    let mut fs = mount(load("01_empty_format.bin"));
    // Superblock geometry matches MemStorage's constants.
    let sb = fs.superblock();
    assert_eq!(sb.block_size, MemStorage::BLOCK_SIZE as u32);
    assert_eq!(sb.block_count, MemStorage::BLOCK_COUNT);
    assert_eq!(sb.version, littlefs2_pure::DISK_VERSION);

    // Root is empty (no user entries).
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut count = 0usize;
    fs.list_root(|_| count += 1, &mut a, &mut b).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn vector_02_single_inline_resolves_and_reads() {
    let mut fs = mount(load("02_single_inline.bin"));
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];

    // /cfg exists, is a regular file, and resolves to InlineStruct
    // body == "hello, littlefs" (15 bytes).
    assert!(fs.exists(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap());
    let mut scratch = [0u8; 32];
    let body = read_inline(&mut fs, "/cfg", &mut scratch);
    assert_eq!(body, b"hello, littlefs");
    assert_eq!(body.len(), 15);

    // Root has exactly one user entry.
    let mut count = 0usize;
    fs.list_root(|_| count += 1, &mut a, &mut b).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn vector_03_single_ctz_resolves_and_reads() {
    let mut fs = mount(load("03_single_ctz.bin"));
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];

    // /payload.bin exists as a CTZ-backed regular file with 500 bytes.
    let r = fs.resolve(Path::new("/payload.bin").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    assert_eq!(ctz.size, 500);

    let bytes = read_ctz_all(&mut fs, "/payload.bin");
    let expected: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
    assert_eq!(bytes, expected);
}

#[test]
fn vector_04_nested_dir_resolves_through_dirstruct() {
    let mut fs = mount(load("04_nested_dir.bin"));
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];

    // /audit is a Directory; /audit/log is a regular file with the
    // expected inline body.
    assert!(fs.exists(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap());
    let mut scratch = [0u8; 32];
    let body = read_inline(&mut fs, "/audit/log", &mut scratch);
    assert_eq!(body, b"entry-0001;");

    // Listing /audit yields exactly one entry named "log".
    let mut entries: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/audit").unwrap(), |e| entries.push(e.name.to_vec()), &mut a, &mut b)
        .unwrap();
    assert_eq!(entries, vec![b"log".to_vec()]);
}

#[test]
fn unformatted_buffer_returns_unformatted() {
    // Sanity gate: a vector path that goes through the same code as
    // the conformance loader. Confirms the conformance harness's
    // mount path correctly distinguishes Unformatted (all-0xFF) from
    // a real vector.
    let s = MemStorage::new();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(s, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::Unformatted);
}
