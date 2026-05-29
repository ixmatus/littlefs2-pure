//! Regression for `lfs-6o9` (2026-05-29 review): `File::set_len` shrink
//! followed by an extending write must not corrupt data on NOR flash.
//!
//! Before the fix, a shrink that left the new tail block partially full
//! reused that already-programmed block as the new tail. A subsequent
//! `File::write` (append) filled the tail in place at `header +
//! bytes_used`, a region still holding the stale content from before the
//! shrink. On NOR the program ANDs with those bytes, so the appended
//! bytes read back wrong (e.g. `0xAA & 0x55 == 0x00`). The fix relocates
//! a partial tail copy-on-write to a freshly erased block before the
//! append, so the fill lands on `0xFF` cells.
//!
//! The bug is invisible on a permissive RAM backing (which lets a
//! program overwrite); it only shows up under strict NOR semantics, so
//! this test uses `NorAlignedStorage<StrictNorStorage>`.

mod common;
use common::StrictNorStorage;
use littlefs2_pure::{Fs, NorAlignedStorage, OpenOptions, Path};

const BS: usize = StrictNorStorage::BLOCK_SIZE;

fn buf() -> [u8; BS] {
    [0u8; BS]
}

#[test]
fn shrink_then_append_preserves_appended_bytes_on_nor() {
    let mut storage = NorAlignedStorage::new(StrictNorStorage::new()).unwrap();
    let mut scratch = buf();
    Fs::format(&mut storage, &mut scratch).unwrap();

    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = buf();
    let mut b = buf();

    let path = Path::new("/log").unwrap();

    // Write 300 bytes of 0xAA: a two-block CTZ file.
    {
        let mut f = fs
            .open(path, OpenOptions::new().write(true).create(true).append(true), &mut a, &mut b)
            .unwrap();
        assert_eq!(f.write(&[0xAA; 300], &mut a, &mut b).unwrap(), 300);
        f.close(&mut a, &mut b).unwrap();
    }

    // Shrink to 260 (a partial tail block) then append 20 bytes of 0x55.
    {
        let mut f =
            fs.open(path, OpenOptions::new().write(true).append(true), &mut a, &mut b).unwrap();
        f.set_len(260, &mut a, &mut b).unwrap();
        assert_eq!(f.write(&[0x55; 20], &mut a, &mut b).unwrap(), 20);
        f.close(&mut a, &mut b).unwrap();
    }

    // Read back: bytes 0..260 stay 0xAA, bytes 260..280 must be 0x55.
    let mut out = [0u8; 280];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 280, "logical size after shrink+append");
    assert!(out[..260].iter().all(|&x| x == 0xAA), "kept prefix must stay intact");
    assert!(
        out[260..280].iter().all(|&x| x == 0x55),
        "appended bytes must read back as written, not ANDed with stale content: {:?}",
        &out[260..280],
    );
}
