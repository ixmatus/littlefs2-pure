//! Integration tests for `NorAlignedStorage`.
//!
//! Exercise the full format + write + remount path with a backing
//! storage that enforces strict NOR flash semantics
//! (PROG_SIZE-aligned programs, no 0->1 bit flips). The wrapper must
//! satisfy these constraints transparently.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Fs, NorAlignedStorage, Path};

mod common;
use common::StrictNorStorage;

fn make_wrapped() -> NorAlignedStorage<StrictNorStorage> {
    NorAlignedStorage::new(StrictNorStorage::new()).expect("valid PROG_SIZE")
}

#[test]
fn format_then_mount_through_nor_wrapper() {
    let mut storage = make_wrapped();
    let mut scratch = [0u8; StrictNorStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    storage.sync().unwrap();

    let mut buf_a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; StrictNorStorage::BLOCK_SIZE];
    let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(fs.superblock().version, littlefs2_pure::DISK_VERSION);
}

#[test]
fn write_inline_through_nor_wrapper() {
    let mut storage = make_wrapped();
    let mut scratch = [0u8; StrictNorStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    storage.sync().unwrap();

    let mut buf_a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut b = [0u8; StrictNorStorage::BLOCK_SIZE];

    fs.write_inline_to_root(b"k", b"hello", &mut a, &mut b).unwrap();
    fs.storage_mut().sync().unwrap();

    let mut a2 = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut b2 = [0u8; StrictNorStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/k").unwrap(), &mut a2, &mut b2).unwrap();
    assert_eq!(r.struct_body, b"hello");
}

#[test]
fn updates_and_compaction_through_nor_wrapper() {
    // The SMIL workload: hammer updates, force compactions. Strict
    // NOR semantics must hold throughout.
    let mut storage = make_wrapped();
    let mut scratch = [0u8; StrictNorStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    storage.sync().unwrap();

    let final_content;
    {
        let mut buf_a = [0u8; StrictNorStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; StrictNorStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; StrictNorStorage::BLOCK_SIZE];
        let mut b = [0u8; StrictNorStorage::BLOCK_SIZE];
        for i in 0..30u32 {
            let v = format!("v{i:02}");
            fs.write_inline_to_root(b"cfg", v.as_bytes(), &mut a, &mut b).unwrap();
        }
        final_content = b"v29".to_vec();
        fs.storage_mut().sync().unwrap();
        storage = fs.into_storage();
    }

    // Fresh mount on the same device.
    let mut buf_a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut b = [0u8; StrictNorStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, &final_content[..]);
}

#[test]
fn ctz_streaming_append_through_nor_wrapper() {
    // The SMIL audit-logger workload: many small appends to a CTZ
    // file. The streaming path programs sub-block windows of the
    // tail; strict NOR semantics (PROG_SIZE-aligned, 1->0 only) must
    // hold for every program, otherwise StrictNorStorage panics.
    let mut storage = make_wrapped();
    let mut scratch = [0u8; StrictNorStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    storage.sync().unwrap();

    let mut all_bytes: Vec<u8> = Vec::new();
    {
        let mut buf_a = [0u8; StrictNorStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; StrictNorStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; StrictNorStorage::BLOCK_SIZE];
        let mut b = [0u8; StrictNorStorage::BLOCK_SIZE];

        // Seed with enough to be CTZ.
        let seed: Vec<u8> = (0..160).map(|i| (i & 0xff) as u8).collect();
        fs.write_to_path(Path::new("/log").unwrap(), &seed, &mut a, &mut b).unwrap();
        all_bytes.extend_from_slice(&seed);

        // 20 sub-block appends; each is a NOR sub-window program.
        for i in 0..20u32 {
            let entry = format!("e{i:03};");
            fs.append_to_path(
                Path::new("/log").unwrap(),
                entry.as_bytes(),
                &mut [],
                &mut a,
                &mut b,
            )
            .unwrap();
            all_bytes.extend_from_slice(entry.as_bytes());
        }
        fs.storage_mut().sync().unwrap();
        storage = fs.into_storage();
    }

    // Remount and read back.
    let mut buf_a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut b = [0u8; StrictNorStorage::BLOCK_SIZE];
    let mut out = vec![0u8; all_bytes.len()];
    let n = fs.read_at_path(Path::new("/log").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, all_bytes.len());
    assert_eq!(out, all_bytes);
}
