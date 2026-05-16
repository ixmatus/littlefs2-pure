//! Integration tests for `Fs::remove_from_root`, `Fs::list_root`,
//! and `Fs::exists`. Covers the CRUD surface SMIL's audit logger
//! relies on (modulo path-nested writes which are not yet supported).

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
fn remove_then_resolve_returns_not_found() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_inline_to_root(b"doomed", b"contents", &mut a, &mut b).unwrap();

    // Confirm it exists, then remove.
    assert!(fs.exists(Path::new("/doomed").unwrap(), &mut a, &mut b).unwrap());
    fs.remove_from_root(b"doomed", &mut a, &mut b).unwrap();
    assert!(!fs.exists(Path::new("/doomed").unwrap(), &mut a, &mut b).unwrap());

    let err = fs.resolve(Path::new("/doomed").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn remove_missing_returns_not_found() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let err = fs.remove_from_root(b"nope", &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound);
}

#[test]
fn remove_renumbers_subsequent_entries() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_inline_to_root(b"a", b"AAA", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"b", b"BBB", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"c", b"CCC", &mut a, &mut b).unwrap();

    // Remove the middle one.
    fs.remove_from_root(b"b", &mut a, &mut b).unwrap();

    // a and c still resolve with the same content.
    let r = fs.resolve(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"AAA");
    let r = fs.resolve(Path::new("/c").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, b"CCC");
    // b is gone.
    assert!(!fs.exists(Path::new("/b").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn remove_with_compaction_survives_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    let final_state: Vec<(Vec<u8>, Vec<u8>)>;
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();

        // Write enough files to drive at least one compaction. With
        // MemStorage's 256-byte blocks, 8 small entries already force
        // compaction. Pick a count that exercises both append and
        // compact paths without exceeding the post-GC capacity.
        const N: u32 = 6;
        for i in 0..N {
            let name = format!("f{i}");
            let content = format!("c{i}");
            fs.write_inline_to_root(name.as_bytes(), content.as_bytes(), &mut a, &mut b).unwrap();
        }
        // Remove every other entry, exercising the Remove path through
        // both append and compact.
        for i in (0..N).step_by(2) {
            let name = format!("f{i}");
            fs.remove_from_root(name.as_bytes(), &mut a, &mut b).unwrap();
        }

        // Capture the expected final state.
        final_state = (0..N)
            .filter(|i| i % 2 != 0)
            .map(|i| (format!("f{i}").into_bytes(), format!("c{i}").into_bytes()))
            .collect();
        storage = fs.into_storage();
    }

    // Fresh mount: every survivor still readable, every removed one gone.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    for (name, expected) in &final_state {
        let mut path_buf = vec![b'/'];
        path_buf.extend_from_slice(name);
        let path_str = std::str::from_utf8(&path_buf).unwrap();
        let r = fs.resolve(Path::new(path_str).unwrap(), &mut a, &mut b).unwrap();
        assert_eq!(r.struct_body, expected.as_slice());
    }
    for i in (0..6u32).step_by(2) {
        let path_str = format!("/f{i}");
        assert!(!fs.exists(Path::new(&path_str).unwrap(), &mut a, &mut b).unwrap());
    }
}

#[test]
fn list_root_enumerates_only_user_entries() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_inline_to_root(b"alpha", b"1", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"beta", b"2", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"gamma", b"3", &mut a, &mut b).unwrap();

    let mut names: Vec<Vec<u8>> = Vec::new();
    let count = fs
        .list_root(
            |e| {
                assert_eq!(e.kind, EntryKind::RegularFile);
                names.push(e.name.to_vec());
            },
            &mut a,
            &mut b,
        )
        .unwrap();
    assert_eq!(count, 3);
    assert!(names.contains(&b"alpha".to_vec()));
    assert!(names.contains(&b"beta".to_vec()));
    assert!(names.contains(&b"gamma".to_vec()));
}

#[test]
fn list_root_skips_removed_entries() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_inline_to_root(b"keep", b"k", &mut a, &mut b).unwrap();
    fs.write_inline_to_root(b"drop", b"d", &mut a, &mut b).unwrap();
    fs.remove_from_root(b"drop", &mut a, &mut b).unwrap();

    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_root(|e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"keep".to_vec()]);
}

#[test]
fn exists_handles_root() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    assert!(fs.exists(Path::new("/").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn exists_returns_false_for_missing() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    assert!(!fs.exists(Path::new("/not-here").unwrap(), &mut a, &mut b).unwrap());
}
