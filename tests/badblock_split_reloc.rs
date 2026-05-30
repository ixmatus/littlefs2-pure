//! Failure-driven relocation during a directory *split* (`lfs-23f`).
//!
//! The plain-compaction relocation is pinned in
//! `tests/pending_badblock_reloc.rs`. A split is the other commit shape that
//! writes a worn block: it programs a fresh `HardTail` continuation (two
//! blocks) and then the original's lower half onto its alternate. Two worn
//! placements matter, and each is relocated past differently:
//!
//!   - **3a — worn alternate:** the lower-half write lands on the worn
//!     alternate. The original pair relocates onto a fresh block (the lower
//!     half, still carrying the `HardTail` to the continuation), and the
//!     parent's `DirStruct` is repointed.
//!   - **3b — worn continuation block:** a continuation block is worn. The
//!     continuation is brand new and unreferenced until the lower-half
//!     commit, so a failed attempt is a clean blank orphan: exclude it and
//!     reallocate the continuation pair.
//!
//! Larger entries force the *first* overflow to split (the live set exceeds
//! half a block before the active block fills), so the split lands on the
//! worn block directly, before any plain compaction can evict it.

use littlefs2_pure::{Fs, Path, Storage};

/// Device whose `program` fails on a configured set of (worn) blocks.
/// Reads and erases still work, modelling blocks that no longer accept
/// writes. NOR-style `program` (bitwise AND into the cell).
struct BadBlocksDev {
    data: Vec<u8>,
    bad: Vec<u32>,
}
impl BadBlocksDev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    fn new(bad: Vec<u32>) -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize], bad }
    }
}
impl Storage for BadBlocksDev {
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
        if self.bad.contains(&block) {
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
fn buf() -> [u8; BadBlocksDev::BLOCK_SIZE] {
    [0u8; BadBlocksDev::BLOCK_SIZE]
}

// A ~120-byte inline value: two entries already exceed a 256-byte block, so
// the first overflow compacts with a live set over half a block and splits
// rather than plain-compacting. This lands the split directly on the worn
// block before any plain compaction can evict it.
const BIG: [u8; 120] = [0xC3; 120];

fn write_n(fs: &mut Fs<BadBlocksDev>, n: usize, a: &mut [u8], b: &mut [u8]) {
    for i in 0..n {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), &BIG, a, b)
            .unwrap_or_else(|e| panic!("entry {i} should survive the worn split block: {e:?}"));
    }
}

fn check_n(fs: &mut Fs<BadBlocksDev>, n: usize, a: &mut [u8], b: &mut [u8]) {
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
    assert_eq!(seen, n, "all entries survive the split relocation");
    // Every entry reads back its full payload.
    for i in 0..n {
        let name = format!("/d/f{i:02}");
        let mut out = [0u8; BIG.len()];
        let got = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b).unwrap();
        assert_eq!(got, BIG.len(), "entry {i} length");
        assert_eq!(out, BIG, "entry {i} payload");
    }
}

/// 3a: the split's lower-half write lands on the worn alternate (block 3).
/// The original pair relocates onto a fresh block; the continuation and the
/// parent repoint complete the split.
#[test]
fn split_lower_half_survives_worn_alternate() {
    // mkdir /d -> {2,3}, active 2, alternate 3; mark the alternate worn.
    let mut storage = BadBlocksDev::new(vec![3]);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let n = 6usize;
    write_n(&mut fs, n, &mut a, &mut b);
    check_n(&mut fs, n, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    check_n(&mut fs, n, &mut a, &mut b);
}

/// 3b: a continuation block (block 4, the first one the allocator hands out
/// for the continuation) is worn; the alternate (block 3) is good. The
/// continuation is reallocated past the worn block and the lower half lands
/// on the good alternate.
#[test]
fn split_continuation_survives_worn_block() {
    // mkdir /d -> {2,3}; block 4 is the first free block the split's
    // continuation allocation reaches (the pair's own {2,3} are excluded).
    let mut storage = BadBlocksDev::new(vec![4]);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let n = 6usize;
    write_n(&mut fs, n, &mut a, &mut b);
    check_n(&mut fs, n, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    check_n(&mut fs, n, &mut a, &mut b);
}
