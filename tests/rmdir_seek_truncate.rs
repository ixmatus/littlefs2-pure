//! Integration tests for `Fs::rmdir`, `Fs::read_at_path` (seek-aware
//! read), and `Fs::truncate_path` (set_len semantics).

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

#[test]
fn rmdir_empty_directory_succeeds() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/empty").unwrap(), &mut a, &mut b).unwrap();
    assert!(fs.exists(Path::new("/empty").unwrap(), &mut a, &mut b).unwrap());
    fs.rmdir(Path::new("/empty").unwrap(), &mut a, &mut b).unwrap();
    assert!(!fs.exists(Path::new("/empty").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn rmdir_non_empty_returns_not_empty() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d/file").unwrap(), b"contents", &mut a, &mut b).unwrap();
    let err = fs.rmdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotEmpty);
    // Directory and its file should both still exist.
    assert!(fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/d/file").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn rmdir_on_regular_file_returns_already_exists() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/file").unwrap(), b"x", &mut a, &mut b).unwrap();
    let err = fs.rmdir(Path::new("/file").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::AlreadyExists);
}

#[test]
fn remove_at_path_on_directory_returns_already_exists() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let err = fs.remove_at_path(Path::new("/d").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::AlreadyExists);
}

#[test]
fn rmdir_then_recreate_directory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    fs.rmdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    // Recreate it; the allocator should have reclaimed the blocks.
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    assert!(fs.exists(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn read_at_path_inline_with_offset() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/cfg").unwrap(), b"abcdefghij", &mut a, &mut b).unwrap();

    let mut buf = [0u8; 4];
    let n = fs.read_at_path(Path::new("/cfg").unwrap(), 3, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf, b"defg");

    // Read past the end -> 0 bytes copied.
    let n = fs.read_at_path(Path::new("/cfg").unwrap(), 100, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn read_at_path_ctz_with_offset() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let content: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_path(Path::new("/log").unwrap(), &content, &mut a, &mut b).unwrap();

    // Read 50 bytes starting at offset 200.
    let mut buf = [0u8; 50];
    let n = fs.read_at_path(Path::new("/log").unwrap(), 200, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(n, 50);
    assert_eq!(buf.as_slice(), &content[200..250]);

    // Read past end returns short count.
    let mut buf = [0u8; 100];
    let n = fs.read_at_path(Path::new("/log").unwrap(), 450, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(n, 50); // bytes 450..500
    assert_eq!(&buf[..n], &content[450..500]);
}

#[test]
fn size_of_reports_correct_size() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/small").unwrap(), b"abc", &mut a, &mut b).unwrap();
    assert_eq!(fs.size_of(Path::new("/small").unwrap(), &mut a, &mut b).unwrap(), 3);

    let big: Vec<u8> = vec![0; 400];
    fs.write_to_path(Path::new("/big").unwrap(), &big, &mut a, &mut b).unwrap();
    assert_eq!(fs.size_of(Path::new("/big").unwrap(), &mut a, &mut b).unwrap(), 400);
}

#[test]
fn truncate_path_shrinks_file() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut cs = [0u8; 1024];
    fs.write_to_path(Path::new("/f").unwrap(), b"abcdefghij", &mut a, &mut b).unwrap();

    fs.truncate_path(Path::new("/f").unwrap(), 5, &mut cs, &mut a, &mut b).unwrap();
    assert_eq!(fs.size_of(Path::new("/f").unwrap(), &mut a, &mut b).unwrap(), 5);

    let mut buf = [0u8; 10];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf[..5], b"abcde");
}

#[test]
fn truncate_path_extends_with_zeros() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut cs = [0u8; 1024];
    fs.write_to_path(Path::new("/f").unwrap(), b"abc", &mut a, &mut b).unwrap();

    fs.truncate_path(Path::new("/f").unwrap(), 8, &mut cs, &mut a, &mut b).unwrap();
    assert_eq!(fs.size_of(Path::new("/f").unwrap(), &mut a, &mut b).unwrap(), 8);

    let mut buf = [0u8; 10];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(n, 8);
    assert_eq!(&buf[..8], b"abc\0\0\0\0\0");
}

#[test]
fn truncate_path_to_zero() {
    // truncate(0) effectively clears the file. Useful for "open with
    // truncate" semantics.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut cs = [0u8; 1024];
    fs.write_to_path(Path::new("/log").unwrap(), b"contents", &mut a, &mut b).unwrap();
    fs.truncate_path(Path::new("/log").unwrap(), 0, &mut cs, &mut a, &mut b).unwrap();
    assert_eq!(fs.size_of(Path::new("/log").unwrap(), &mut a, &mut b).unwrap(), 0);
}
