//! Review L6: `create` without write access must not mutate flash.
//!
//! `OpenOptions::create(true)` asks the kernel to materialize an entry
//! when the name is missing, which programs the parent metadata pair.
//! Combining it with an access mode that grants neither `write` nor
//! `append` therefore turns a nominally read only open into a flash
//! mutation, which is exactly what the standard library forbids:
//! `std::fs::OpenOptions` returns `InvalidInput` when `create` or
//! `create_new` is set and the access mode lacks both `write` and
//! `append`.
//!
//! The tests here pin both halves of the fix. The rejection is typed
//! and happens before any device call, and the storage counters prove
//! the reject is total: zero programs and zero erases, with the raw
//! image bytes unchanged.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Error, Fs, OpenOptions, Path};

mod common;
use common::MemStorage;

/// Storage decorator that counts device mutations.
///
/// Program and erase calls pass through to the inner [`MemStorage`]
/// after bumping a counter, so a test can assert that an operation
/// touched no flash at all. Reads are not counted; a rejected open is
/// allowed to read (path resolution runs before the mode check in some
/// orderings), but it must never program or erase.
struct CountingStorage {
    inner: MemStorage,
    programs: usize,
    erases: usize,
}

impl CountingStorage {
    fn new(inner: MemStorage) -> Self {
        Self { inner, programs: 0, erases: 0 }
    }
}

impl Storage for CountingStorage {
    type Error = ();
    const READ_SIZE: usize = MemStorage::READ_SIZE;
    const PROG_SIZE: usize = MemStorage::PROG_SIZE;
    const BLOCK_SIZE: usize = MemStorage::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = MemStorage::BLOCK_COUNT;
    const CACHE_SIZE: usize = MemStorage::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = MemStorage::LOOKAHEAD_SIZE;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        self.inner.read(block, off, buf)
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        self.programs += 1;
        self.inner.program(block, off, data)
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        self.erases += 1;
        self.inner.erase(block)
    }
}

fn fresh_counting_fs() -> Fs<CountingStorage> {
    let mut storage = CountingStorage::new(MemStorage::new());
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    storage.programs = 0;
    storage.erases = 0;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    Fs::mount(storage, &mut a, &mut b).unwrap()
}

/// The headline case: `read(true).create(true)` names no write access,
/// so the open must be rejected and the device must be untouched.
#[test]
fn create_without_write_access_is_rejected_and_touches_no_flash() {
    let mut fs = fresh_counting_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/ghost").unwrap();

    let before = fs.storage().inner.data.clone();

    let r = fs.open(path, OpenOptions::new().read(true).create(true), &mut a, &mut b);
    assert_eq!(r.err(), Some(Error::InvalidPath));

    assert_eq!(fs.storage().programs, 0, "a rejected open must not program flash");
    assert_eq!(fs.storage().erases, 0, "a rejected open must not erase flash");
    assert_eq!(fs.storage().inner.data, before, "a rejected open must not change the image");

    // The name was never materialized, so a later honest open still
    // reports the file as missing.
    let r = fs.open(path, OpenOptions::new().read(true), &mut a, &mut b);
    assert_eq!(r.err(), Some(Error::NotFound));
}

/// `create(true)` alone (no access mode at all) is rejected by the
/// existing "neither read nor write" arm. Pinned here so the
/// widened predicate keeps the same typed answer.
#[test]
fn create_with_no_access_mode_is_rejected() {
    let mut fs = fresh_counting_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/ghost").unwrap();

    let r = fs.open(path, OpenOptions::new().create(true), &mut a, &mut b);
    assert_eq!(r.err(), Some(Error::InvalidPath));
    assert_eq!(fs.storage().programs, 0);
    assert_eq!(fs.storage().erases, 0);
}

/// `truncate` without write access is the sibling hole of the same
/// class and was already rejected. Pinned so the widened predicate
/// does not lose it, including on an existing file where truncate
/// would otherwise destroy content.
#[test]
fn truncate_without_write_access_is_rejected_and_touches_no_flash() {
    let mut fs = fresh_counting_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/victim").unwrap();
    fs.write_to_path(path, &[b'v'; 32], &mut a, &mut b).unwrap();

    let programs = fs.storage().programs;
    let erases = fs.storage().erases;
    let before = fs.storage().inner.data.clone();

    let r = fs.open(path, OpenOptions::new().read(true).truncate(true), &mut a, &mut b);
    assert_eq!(r.err(), Some(Error::InvalidPath));
    assert_eq!(fs.storage().programs, programs);
    assert_eq!(fs.storage().erases, erases);
    assert_eq!(fs.storage().inner.data, before);

    // The content survived.
    let mut out = [0u8; 64];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 32);
    assert!(out[..n].iter().all(|&v| v == b'v'));
}

/// The legitimate creating opens keep working. `write` and `append`
/// each grant enough access for `create`, matching the standard
/// library's predicate.
#[test]
fn create_with_write_or_append_still_opens() {
    for options in [
        OpenOptions::new().write(true).create(true),
        OpenOptions::new().append(true).create(true),
        OpenOptions::new().read(true).write(true).create(true),
    ] {
        let mut fs = fresh_counting_fs();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let path = Path::new("/real").unwrap();
        let file = fs.open(path, options, &mut a, &mut b).expect("creating open must succeed");
        drop(file);
    }
}
