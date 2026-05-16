//! Integration tests for `mkdir`, path-based writes/removes, and
//! `list_dir`. Covers the API surface SMIL's audit logger needs for
//! `create_dir("/audit")` + writes into the subdirectory.

use littlefs2_pure::ctz::CtzStruct;
use littlefs2_pure::tag::TagType;
use littlefs2_pure::{EntryKind, Error, Fs, Path};

mod common;
use common::MemStorage;

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap()
}

#[test]
fn mkdir_then_resolve_finds_directory() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();

    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let resolved = fs.resolve(Path::new("/audit").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(resolved.entry.kind, EntryKind::Directory);
    assert_eq!(resolved.entry.name, b"audit");
    assert_eq!(resolved.struct_type, TagType::DirStruct);
    assert_eq!(resolved.struct_body.len(), 8);
}

#[test]
fn mkdir_duplicate_returns_already_exists() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap();
    let err = fs.mkdir(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::AlreadyExists);
}

#[test]
fn mkdir_missing_parent_returns_not_found() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let err = fs.mkdir(Path::new("/nope/sub").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn mkdir_root_returns_invalid_path() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let err = fs.mkdir(Path::new("/").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::InvalidPath);
}

#[test]
fn write_to_path_inline_file_in_subdirectory() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/audit/entry0").unwrap(), b"hello", &mut a, &mut b).unwrap();

    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let r = fs.resolve(Path::new("/audit/entry0").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::InlineStruct);
    assert_eq!(r.struct_body, b"hello");
}

#[test]
fn write_to_path_ctz_file_in_subdirectory() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    let content: Vec<u8> = (0..400).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_path(Path::new("/audit/log.bin").unwrap(), &content, &mut a, &mut b).unwrap();

    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let r = fs.resolve(Path::new("/audit/log.bin").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    let mut out = vec![0u8; ctz.size as usize];
    let mut scratch = common::make_buffer();
    fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
    assert_eq!(out, content);
}

#[test]
fn list_dir_enumerates_subdirectory_contents() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/cfg/one").unwrap(), b"1", &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/cfg/two").unwrap(), b"2", &mut a, &mut b).unwrap();

    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let mut names: Vec<Vec<u8>> = Vec::new();
    let count = fs
        .list_dir(
            Path::new("/cfg").unwrap(),
            |e| {
                names.push(e.name.to_vec());
            },
            &mut a2,
            &mut b2,
        )
        .unwrap();
    assert_eq!(count, 2);
    assert!(names.contains(&b"one".to_vec()));
    assert!(names.contains(&b"two".to_vec()));
}

#[test]
fn remove_at_path_in_subdirectory() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d/keep").unwrap(), b"k", &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d/drop").unwrap(), b"d", &mut a, &mut b).unwrap();
    fs.remove_at_path(Path::new("/d/drop").unwrap(), &mut a, &mut b).unwrap();

    assert!(fs.exists(Path::new("/d/keep").unwrap(), &mut a, &mut b).unwrap());
    assert!(!fs.exists(Path::new("/d/drop").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn nested_writes_survive_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
        fs.write_to_path(Path::new("/audit/00").unwrap(), b"first", &mut a, &mut b).unwrap();
        fs.write_to_path(Path::new("/audit/01").unwrap(), b"second", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    // Fresh mount.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs.resolve(Path::new("/audit/00").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"first");
    let r = fs.resolve(Path::new("/audit/01").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"second");
}

#[test]
fn mkdir_creates_isolated_namespaces() {
    // Two subdirectories with the same file names; each should be
    // independently navigable.
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/b").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/a/file").unwrap(), b"AAA", &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/b/file").unwrap(), b"BBB", &mut a, &mut b).unwrap();

    let r = fs.resolve(Path::new("/a/file").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"AAA");
    let r = fs.resolve(Path::new("/b/file").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"BBB");
}
