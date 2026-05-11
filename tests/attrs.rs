//! Integration tests for `Fs::set_attr` / `get_attr` / `remove_attr`.

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
fn get_attr_on_entry_without_attribute_returns_zero() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    let mut out = [0u8; 16];
    let n = fs.get_attr(Path::new("/f").unwrap(), 7, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn set_then_get_roundtrips() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f").unwrap(), 9, b"value!", &mut a, &mut b).unwrap();
    let mut out = [0u8; 16];
    let n = fs.get_attr(Path::new("/f").unwrap(), 9, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 6);
    assert_eq!(&out[..n], b"value!");
}

#[test]
fn set_attr_replaces_previous() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f").unwrap(), 1, b"first", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f").unwrap(), 1, b"second", &mut a, &mut b).unwrap();
    let mut out = [0u8; 16];
    let n = fs.get_attr(Path::new("/f").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"second");
}

#[test]
fn remove_attr_makes_get_return_zero() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f").unwrap(), 3, b"present", &mut a, &mut b).unwrap();
    fs.remove_attr(Path::new("/f").unwrap(), 3, &mut a, &mut b).unwrap();
    let mut out = [0u8; 16];
    let n = fs.get_attr(Path::new("/f").unwrap(), 3, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn distinct_attr_ids_are_independent() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f").unwrap(), 1, b"one", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f").unwrap(), 2, b"two", &mut a, &mut b).unwrap();
    fs.remove_attr(Path::new("/f").unwrap(), 1, &mut a, &mut b).unwrap();
    let mut out = [0u8; 16];
    let n1 = fs.get_attr(Path::new("/f").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n1, 0);
    let n2 = fs.get_attr(Path::new("/f").unwrap(), 2, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n2], b"two");
}

#[test]
fn set_attr_on_missing_file_returns_not_found() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let err = fs.set_attr(Path::new("/nope").unwrap(), 0, b"v", &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn set_attr_rejects_oversize_value() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    let big = vec![0xab_u8; 0x3FF];
    let err = fs.set_attr(Path::new("/f").unwrap(), 0, &big, &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::OutOfRange);
}

#[test]
fn attrs_survive_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.write_to_path(Path::new("/cfg").unwrap(), b"payload", &mut a, &mut b).unwrap();
        fs.set_attr(Path::new("/cfg").unwrap(), 5, b"persisted", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut out = [0u8; 32];
    let n = fs.get_attr(Path::new("/cfg").unwrap(), 5, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"persisted");
}
