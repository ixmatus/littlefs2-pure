//! Reproduce-first target for `lfs-cvh`: write-side directory splitting.
//!
//! The writer never emits a `HardTail` tag, so a directory's entries must
//! all fit in one metadata pair (one block). Past that the kernel returns
//! `Error::OutOfRange` instead of splitting the directory across a
//! HardTail-threaded continuation pair the way the C reference does. The
//! crate already *reads* such split directories; this pins the write-side
//! gap.
//!
//! `confirm_overflow_is_the_current_limit` documents today's behavior and
//! runs in CI. `directory_grows_past_one_pair_via_split` is the target:
//! it is `#[ignore]`d until splitting lands (lfs-cvh), at which point the
//! ignore is removed and it must pass.

use littlefs2_pure::{Error, Fs, Path, Storage};

struct Dev {
    data: Vec<u8>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}
impl Storage for Dev {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let s = (block as usize) * Self::BLOCK_SIZE;
        self.data[s..s + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}
fn buf() -> [u8; Dev::BLOCK_SIZE] {
    [0u8; Dev::BLOCK_SIZE]
}

/// How many small entries fit in a subdirectory before the kernel rejects
/// the next one. Returns the count successfully created.
fn fill_subdir_until_full() -> usize {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let mut created = 0usize;
    for i in 0..200u32 {
        let name = format!("/d/f{i:03}");
        match fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b) {
            Ok(()) => created += 1,
            Err(Error::OutOfRange) => break,
            Err(e) => panic!("unexpected error at entry {i}: {e:?}"),
        }
    }
    created
}

#[test]
fn confirm_overflow_is_the_current_limit() {
    // With HardTail splitting (lfs-cvh) a directory grows across
    // continuation pairs, so it is no longer capped at one pair. Overflow
    // is now device-bound: continuation pairs consume free blocks until
    // this 64-block device is exhausted, at which point the allocator
    // returns OutOfRange. The bound still falls below the 200-entry probe
    // ceiling, so this documents that a finite device still terminates.
    let n = fill_subdir_until_full();
    assert!(n >= 1, "should fit at least one entry");
    assert!(n < 200, "a 64-block device must run out of blocks before 200 entries (got {n})");
}

#[test]
fn directory_grows_past_one_pair_via_split() {
    // After splitting lands, a directory holds far more entries than one
    // metadata pair can, and every entry reads back.
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let target = 60usize; // comfortably more than one 256-byte pair holds
    for i in 0..target {
        let name = format!("/d/f{i:03}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} should succeed once splitting lands: {e:?}"));
    }
    // All entries enumerate and read back.
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, target, "all split-directory entries must enumerate");
}
