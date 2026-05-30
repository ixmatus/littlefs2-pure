//! Reproduce-first target for `lfs-23f`: failure-driven block relocation.
//!
//! The commit path maps every `Storage::program`/`erase` failure to
//! `Error::Io` and propagates it; the only relocation is wear-leveling
//! (the `BLOCK_CYCLES` predicate), never failure-driven. The C reference
//! relocates a block when a commit fails (a worn/bad block), so a single
//! bad block is recoverable rather than fatal. This pins the gap.
//!
//! `confirm_bad_block_is_currently_fatal` documents today's behavior.
//! `write_survives_a_bad_block_via_relocation` is the target, `#[ignore]`d
//! until relocation-on-failure lands (lfs-23f).

use littlefs2_pure::{Error, Fs, Path, Storage};

/// Device whose `program` fails on one designated (worn) block, modelling
/// a block that no longer accepts writes. Reads and erases still work.
struct BadBlockDev {
    data: Vec<u8>,
    bad: u32,
}
impl BadBlockDev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    fn new(bad: u32) -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize], bad }
    }
}
impl Storage for BadBlockDev {
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
        if block == self.bad {
            return Err(()); // worn block: refuses writes
        }
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
fn buf() -> [u8; BadBlockDev::BLOCK_SIZE] {
    [0u8; BadBlockDev::BLOCK_SIZE]
}

// After format (block 0 used, block 1 the erased alternate) the allocator
// hands out the lowest free block, 2, for the first CTZ file. Marking 2
// bad makes the first allocating write hit it deterministically.
const BAD: u32 = 2;

#[test]
fn confirm_bad_block_is_currently_fatal() {
    let mut storage = BadBlockDev::new(BAD);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    // A 300-byte CTZ file allocates blocks 2 and 3; programming block 2
    // fails. Today that surfaces as a fatal Io error, not a relocation.
    let r = fs.write_to_path(Path::new("/f").unwrap(), &[0xAA; 300], &mut a, &mut b);
    assert!(
        matches!(r, Err(Error::Io)),
        "a worn block currently fails the whole write (got {r:?})",
    );
}

#[test]
#[ignore = "target for lfs-23f: remove ignore when failure-driven relocation lands"]
fn write_survives_a_bad_block_via_relocation() {
    let mut storage = BadBlockDev::new(BAD);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    // After relocation lands, the worn block is skipped and the write
    // completes onto good blocks; the content reads back intact.
    fs.write_to_path(Path::new("/f").unwrap(), &[0xAA; 300], &mut a, &mut b)
        .expect("write should survive a single bad block via relocation");
    let mut out = [0u8; 300];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert!(out.iter().all(|&x| x == 0xAA));
}
