//! H2 regression: read-back verification after programming a commit.
//!
//! Source: `docs/reviews/2026-06-10-deep-adversarial-review.md` High
//! finding H2.
//!
//! The C reference re-reads and CRC-checks every commit it programs
//! (`lfs_dir_commitcrc` via `lfs_bd_crc`); a device that accepts a
//! program and lands corrupted cells is treated exactly like one that
//! reports the failure, taking the worn-block path. Before the fix
//! this crate trusted `program`'s `Ok` and reported durable success
//! for commits whose bytes never landed; the corruption surfaced only
//! at the next mount of that pair, as silently missing state.
//!
//! `SilentCorruptStorage` models the failure: programs to a sticky
//! bad block report `Ok` but flip one bit of the written region.

use littlefs2_pure::{Error, Fs, Path, Storage};

mod common;
use common::MemStorage;

/// Wraps [`MemStorage`]; programs to blocks marked bad report success
/// but corrupt one bit of the just-written region. Reads and erases
/// pass through honestly (erase on real worn NOR can also lie, but
/// the erase verdict is FCRC-checked by the reader; H2 is about the
/// program path).
struct SilentCorruptStorage {
    inner: MemStorage,
    bad: [bool; MemStorage::BLOCK_COUNT as usize],
}

impl SilentCorruptStorage {
    fn new(inner: MemStorage) -> Self {
        Self { inner, bad: [false; MemStorage::BLOCK_COUNT as usize] }
    }

    fn set_bad(&mut self, block: u32) {
        self.bad[block as usize] = true;
    }
}

impl Storage for SilentCorruptStorage {
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
        self.inner.program(block, off, data)?;
        if self.bad[block as usize] && !data.is_empty() {
            let idx = (block as usize) * Self::BLOCK_SIZE + off as usize;
            self.inner.data[idx] ^= 0x40;
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        self.inner.erase(block)
    }
}

/// Format + mount on clean storage, mkdir `/d`, and hand back the
/// storage. After format the root pair is `{0, 1}`; mkdir's pair is
/// allocated lowest-free-ascending, so `/d` lives at `{2, 3}` with
/// its init commit on block 2.
fn storage_with_subdir() -> SilentCorruptStorage {
    let mut storage = SilentCorruptStorage::new(MemStorage::new());
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.into_storage()
}

#[test]
fn corrupted_append_relocates_pair_and_data_survives() {
    // The subdir pair's active block (2) silently corrupts every
    // program. The append's read-back must fail, divert to the
    // worn-block eviction, and land the commit on a fresh block; the
    // file then survives a remount byte-for-byte. Before the fix the
    // write reported success and the remount lost the file.
    let mut storage = storage_with_subdir();
    storage.set_bad(2);

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let content = b"survives a lying program";
    fs.write_to_path(Path::new("/d/f").unwrap(), content, &mut a, &mut b).unwrap();

    let storage = fs.into_storage();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut out = [0u8; 64];
    let n = fs.read_at_path(Path::new("/d/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], content);
}

#[test]
fn corrupted_compact_alternate_relocates_and_all_files_survive() {
    // The subdir's alternate (3) silently corrupts. Small writes
    // append to block 2 until it fills; the compaction then programs
    // block 3, whose read-back must fail and divert to the fresh-block
    // relocation. Every file written must survive a remount. Before
    // the fix the compaction reported success and the remount saw the
    // pair's last CCRC-valid prefix: files silently gone.
    let mut storage = storage_with_subdir();
    storage.set_bad(3);

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let names = ["/d/f0", "/d/f1", "/d/f2", "/d/f3", "/d/f4", "/d/f5"];
    for (i, name) in names.iter().enumerate() {
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let content = [i as u8; 24];
        fs.write_to_path(Path::new(name).unwrap(), &content, &mut a, &mut b).unwrap();
    }

    let storage = fs.into_storage();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    for (i, name) in names.iter().enumerate() {
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let mut out = [0u8; 64];
        let n = fs.read_at_path(Path::new(name).unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(&out[..n], &[i as u8; 24], "{name} lost or corrupted");
    }
}

#[test]
fn corrupted_root_append_is_an_error_not_silent_success() {
    // The root pair cannot relocate, so when both its blocks corrupt
    // silently the write must surface an error; the pre-fix behavior
    // was `Ok` with the commit lost. The previously committed state
    // must remain intact on remount.
    let mut storage = SilentCorruptStorage::new(MemStorage::new());
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    // A pre-existing file, committed while the device was honest.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_root(b"keep", b"old state", &mut a, &mut b).unwrap();

    let mut storage = fs.into_storage();
    storage.set_bad(0);
    storage.set_bad(1);

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let err = fs.write_to_root(b"new", b"never lands", &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::Io);

    // The failed write must not have damaged the durable state.
    let storage = fs.into_storage();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut out = [0u8; 32];
    let n = fs.read_at_path(Path::new("/keep").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"old state");
}
