//! Integration tests for `Fs::rename_in_dir` (same-parent rename).

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
fn rename_file_in_root() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/old").unwrap(), b"contents", &mut a, &mut b).unwrap();
    fs.rename_in_dir(Path::new("/old").unwrap(), Path::new("/new").unwrap(), &mut a, &mut b)
        .unwrap();

    assert!(!fs.exists(Path::new("/old").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/new").unwrap(), &mut a, &mut b).unwrap());

    // Content survives.
    let mut buf = [0u8; 32];
    let n = fs.read_at_path(Path::new("/new").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"contents");
}

#[test]
fn rename_directory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/d_old").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d_old/inside").unwrap(), b"x", &mut a, &mut b).unwrap();

    fs.rename_in_dir(Path::new("/d_old").unwrap(), Path::new("/d_new").unwrap(), &mut a, &mut b)
        .unwrap();

    assert!(!fs.exists(Path::new("/d_old").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/d_new").unwrap(), &mut a, &mut b).unwrap());
    // Contents of the directory still accessible under the new name.
    assert!(fs.exists(Path::new("/d_new/inside").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn rename_collision_rejected() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/a").unwrap(), b"AAA", &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/b").unwrap(), b"BBB", &mut a, &mut b).unwrap();

    let err = fs
        .rename_in_dir(Path::new("/a").unwrap(), Path::new("/b").unwrap(), &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::AlreadyExists);

    // Both should still be intact.
    let mut buf = [0u8; 8];
    let n = fs.read_at_path(Path::new("/a").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"AAA");
    let n = fs.read_at_path(Path::new("/b").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"BBB");
}

#[test]
fn rename_missing_source_returns_not_found() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let err = fs
        .rename_in_dir(Path::new("/nope").unwrap(), Path::new("/dst").unwrap(), &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn rename_to_same_name_is_noop() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/x").unwrap(), b"v1", &mut a, &mut b).unwrap();
    fs.rename_in_dir(Path::new("/x").unwrap(), Path::new("/x").unwrap(), &mut a, &mut b).unwrap();
    // Still exists with original content.
    assert!(fs.exists(Path::new("/x").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn rename_cross_parent_returns_invalid_path() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/file").unwrap(), b"x", &mut a, &mut b).unwrap();

    let err = fs
        .rename_in_dir(Path::new("/file").unwrap(), Path::new("/d/file").unwrap(), &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::InvalidPath);
}

#[test]
fn cross_dir_rename_moves_file() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/file").unwrap(), b"contents", &mut a, &mut b).unwrap();

    fs.rename(Path::new("/file").unwrap(), Path::new("/dst/file").unwrap(), &mut a, &mut b)
        .unwrap();

    assert!(!fs.exists(Path::new("/file").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/dst/file").unwrap(), &mut a, &mut b).unwrap());

    // Content travels with the entry.
    let mut buf = [0u8; 32];
    let n = fs.read_at_path(Path::new("/dst/file").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"contents");
}

#[test]
fn cross_dir_rename_with_new_name() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/old").unwrap(), b"AAA", &mut a, &mut b).unwrap();

    fs.rename(Path::new("/old").unwrap(), Path::new("/dst/new").unwrap(), &mut a, &mut b).unwrap();

    assert!(!fs.exists(Path::new("/old").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/dst/new").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn cross_dir_rename_preserves_ctz_chain_in_place() {
    // The CTZ chain blocks are shared; only the directory entry moves.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/archive").unwrap(), &mut a, &mut b).unwrap();
    let payload: Vec<u8> = (0..300).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_path(Path::new("/log").unwrap(), &payload, &mut a, &mut b).unwrap();

    fs.rename(Path::new("/log").unwrap(), Path::new("/archive/log").unwrap(), &mut a, &mut b)
        .unwrap();

    // Read full content back through the new path.
    let mut out = vec![0u8; payload.len()];
    let n =
        fs.read_at_path(Path::new("/archive/log").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, payload.len());
    assert_eq!(out, payload);
}

#[test]
fn cross_dir_rename_moves_directory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/src").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/src/inner").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/src/inner/x").unwrap(), b"x", &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();

    fs.rename(Path::new("/src/inner").unwrap(), Path::new("/dst/inner").unwrap(), &mut a, &mut b)
        .unwrap();

    assert!(!fs.exists(Path::new("/src/inner").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/dst/inner").unwrap(), &mut a, &mut b).unwrap());
    // Contents survive the move.
    assert!(fs.exists(Path::new("/dst/inner/x").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn cross_dir_rename_rejects_existing_destination() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/file").unwrap(), b"A", &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/dst/file").unwrap(), b"B", &mut a, &mut b).unwrap();

    let err = fs
        .rename(Path::new("/file").unwrap(), Path::new("/dst/file").unwrap(), &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::AlreadyExists);

    // Both entries remain intact.
    let mut buf = [0u8; 4];
    let n = fs.read_at_path(Path::new("/file").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"A");
    let n = fs.read_at_path(Path::new("/dst/file").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"B");
}

#[test]
fn cross_dir_rename_rejects_ancestor_cycle() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let err = fs
        .rename(Path::new("/d").unwrap(), Path::new("/d/inside").unwrap(), &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::InvalidPath);
}

#[test]
fn cross_dir_rename_survives_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.mkdir(Path::new("/archive").unwrap(), &mut a, &mut b).unwrap();
        fs.write_to_path(Path::new("/log").unwrap(), b"persistent", &mut a, &mut b).unwrap();
        fs.rename(Path::new("/log").unwrap(), Path::new("/archive/log").unwrap(), &mut a, &mut b)
            .unwrap();
        storage = fs.into_storage();
    }
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    assert!(!fs.exists(Path::new("/log").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/archive/log").unwrap(), &mut a, &mut b).unwrap());
    let mut buf = [0u8; 32];
    let n =
        fs.read_at_path(Path::new("/archive/log").unwrap(), 0, &mut buf, &mut a, &mut b).unwrap();
    assert_eq!(&buf[..n], b"persistent");
}

#[test]
fn rename_dispatches_to_in_dir_for_same_parent() {
    // The dispatcher must hand same-parent renames to rename_in_dir;
    // the cross-dir path's Create-then-Delete would change the entry's
    // id within the same pair, which is observable.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/a").unwrap(), b"AAA", &mut a, &mut b).unwrap();
    fs.rename(Path::new("/a").unwrap(), Path::new("/b").unwrap(), &mut a, &mut b).unwrap();
    assert!(!fs.exists(Path::new("/a").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/b").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn rename_in_subdirectory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/audit/log").unwrap(), b"current", &mut a, &mut b).unwrap();
    fs.rename_in_dir(
        Path::new("/audit/log").unwrap(),
        Path::new("/audit/log.archived").unwrap(),
        &mut a,
        &mut b,
    )
    .unwrap();

    assert!(!fs.exists(Path::new("/audit/log").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/audit/log.archived").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn rename_survives_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.write_to_path(Path::new("/old").unwrap(), b"persistent", &mut a, &mut b).unwrap();
        fs.rename_in_dir(Path::new("/old").unwrap(), Path::new("/new").unwrap(), &mut a, &mut b)
            .unwrap();
        storage = fs.into_storage();
    }
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    assert!(!fs.exists(Path::new("/old").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/new").unwrap(), &mut a, &mut b).unwrap());
}
