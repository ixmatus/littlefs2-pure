//! Reproduce-first target for `lfs-23f`: failure-driven block relocation.
//!
//! The commit path maps every `Storage::program`/`erase` failure to
//! `Error::Io` and propagates it; the only relocation is wear-leveling
//! (the `BLOCK_CYCLES` predicate), never failure-driven. The C reference
//! relocates a block when a commit fails (a worn/bad block), so a single
//! bad block is recoverable rather than fatal. This pins the gap.
//!
//! Progress: the CTZ initial-write path (`write_survives_*`) and the CTZ
//! append path (`append_survives_*`) relocate past worn data blocks. The
//! remaining gap is failure-driven relocation of a **metadata pair** on a
//! worn commit block, pinned by the `#[ignore]`d
//! `metadata_commit_survives_a_bad_block_via_relocation` (lfs-23f).

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

/// A worn block in a CTZ write is relocated past, not fatal: the write
/// completes onto good blocks and the content reads back intact.
#[test]
fn write_survives_a_bad_block_via_relocation() {
    let mut storage = BadBlockDev::new(BAD);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(Path::new("/f").unwrap(), &[0xAA; 300], &mut a, &mut b)
        .expect("write should survive a single bad block via relocation");
    let mut out = [0u8; 300];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert!(out.iter().all(|&x| x == 0xAA));
}

/// A worn block hit while a CTZ *append* allocates an overflow block is
/// relocated past, not fatal. Exercises `stream_ctz_extend` (the append /
/// `File`-write new-block path), distinct from the initial-write path
/// above.
#[test]
fn append_survives_a_bad_block_via_relocation() {
    // A 200-byte initial file is a one-block CTZ (above INLINE_MAX = 128),
    // so the append takes the streaming `stream_ctz_extend` path rather
    // than re-writing inline. The append overflows into freshly allocated
    // blocks; the first the allocator hands out is block 3, marked worn.
    let mut storage = BadBlockDev::new(3);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    let mut scratch = [0u8; 1024];

    fs.write_to_path(Path::new("/f").unwrap(), &[0x11; 200], &mut a, &mut b).unwrap();
    fs.append_to_path(Path::new("/f").unwrap(), &[0x22; 500], &mut scratch, &mut a, &mut b)
        .expect("append should survive a worn overflow block via relocation");

    let mut out = [0u8; 700];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 700);
    assert!(out[..200].iter().all(|&x| x == 0x11), "original bytes intact");
    assert!(out[200..].iter().all(|&x| x == 0x22), "appended bytes intact");
}

/// Storage with several designated worn blocks.
struct MultiBadDev {
    data: Vec<u8>,
    bad: Vec<u32>,
}
impl MultiBadDev {
    fn new(bad: Vec<u32>) -> Self {
        Self {
            data: vec![0xFFu8; BadBlockDev::BLOCK_SIZE * BadBlockDev::BLOCK_COUNT as usize],
            bad,
        }
    }
}
impl Storage for MultiBadDev {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BadBlockDev::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = BadBlockDev::BLOCK_COUNT;
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
            return Err(());
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

/// Sub-case 1 in isolation: a worn block hit by a *plain compaction* (the
/// live set still fits one block, so no split). After `mkdir /d` at `{2,3}`
/// with block 3 (the alternate) worn, six entries overflow block 2 once: the
/// sixth write compacts the pair, and the compaction's write lands on the
/// worn alternate. The pair must relocate onto a fresh block. Smaller and
/// split-free, so it pins the plain-compact relocation path on its own
/// before the full reproducer exercises the split path too.
#[test]
fn metadata_plain_compact_survives_worn_alternate() {
    let mut storage = BadBlockDev::new(3);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // Six entries: five append into block 2, the sixth overflows and
    // compacts onto the worn alternate (block 3), which must relocate. Six
    // keeps the live set under half a block, so the kernel compacts rather
    // than splits.
    let n = 6usize;
    for i in 0..n {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} should survive the worn alternate: {e:?}"));
    }

    let check = |fs: &mut Fs<BadBlockDev>, a: &mut [u8], b: &mut [u8]| {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
        assert_eq!(seen, n, "all entries survive the plain-compact relocation");
    };
    check(&mut fs, &mut a, &mut b);
    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    check(&mut fs, &mut a, &mut b);
}

/// A worn block hit while a metadata pair *commits* (compacts onto its
/// alternate, or splits onto it) is relocated past: the pair migrates to a
/// fresh block, the parent's `DirStruct` is updated, and the operation
/// completes. Distinct from the CTZ data/append paths above, which relocate
/// file blocks, not metadata pairs.
///
/// Twenty-four entries overflow a 256-byte block, so `/d` must split across
/// a `HardTail` continuation. The first overflow (write 5) is a plain
/// compaction onto the worn alternate (block 3), which relocates `/d` onto a
/// fresh block and drops block 3 from the pair. The later split therefore
/// lands on good blocks: once relocated, block 3 is no longer one of `/d`'s
/// blocks, and `scan_used_blocks`' raw-tag over-approximation keeps the
/// parent's superseded `DirStruct(/d -> {2,3})` marking block 3 as used
/// until the root next compacts, so the allocator does not re-hand it out as
/// a continuation block. A split that lands *directly* onto a worn block
/// (larger entries, so the first overflow splits before any plain
/// compaction can evict the worn half) is covered separately.
#[test]
fn metadata_commit_survives_a_bad_block_via_relocation() {
    // After format the root is {0,1}; `mkdir /d` takes the next two free
    // blocks {2,3}, writing its init commit to block 2 (the active half)
    // and leaving block 3 as the erased alternate. Marking block 3 worn
    // makes /d's first compaction (or split) onto its alternate hit it.
    let mut storage = BadBlockDev::new(3);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // Fill /d until its active block overflows and the kernel must commit
    // onto the (worn) alternate. Every entry must land via relocation.
    let n = 24usize;
    for i in 0..n {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} should survive the worn metadata block: {e:?}"));
    }

    // All entries enumerate and read back, here and after a remount (the
    // relocation's gstate must balance — no half-applied cycle).
    let check = |fs: &mut Fs<BadBlockDev>, a: &mut [u8], b: &mut [u8]| {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
        assert_eq!(seen, n, "all entries survive the metadata relocation");
    };
    check(&mut fs, &mut a, &mut b);
    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    check(&mut fs, &mut a, &mut b);
}

/// Several consecutive worn blocks are all relocated past.
#[test]
fn write_survives_several_bad_blocks() {
    // Blocks 2,3,4 (the first ones the allocator hands out) are bad.
    let mut storage = MultiBadDev::new(vec![2, 3, 4]);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(Path::new("/f").unwrap(), &[0x5A; 300], &mut a, &mut b)
        .expect("write should relocate past several bad blocks");
    let mut out = [0u8; 300];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert!(out.iter().all(|&x| x == 0x5A));
}

/// A device with too many bad blocks (beyond the retry bound) fails the
/// write with Io rather than looping forever.
#[test]
fn too_many_bad_blocks_is_bounded_io() {
    let bad: Vec<u32> = (2..BadBlockDev::BLOCK_COUNT).collect(); // every free block bad
    let mut storage = MultiBadDev::new(bad);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    let r = fs.write_to_path(Path::new("/f").unwrap(), &[0xAA; 300], &mut a, &mut b);
    assert!(matches!(r, Err(Error::Io | Error::OutOfRange)), "got {r:?}");
}

/// Device whose `program` fails on one designated block only for writes at a
/// non-zero offset, modelling a block worn out for in-place appends but
/// still accepting a full (offset-zero) compaction write. Reads and erases
/// always work.
struct AppendFailDev {
    data: Vec<u8>,
    bad: u32,
}
impl AppendFailDev {
    fn new(bad: u32) -> Self {
        Self {
            data: vec![0xFFu8; BadBlockDev::BLOCK_SIZE * BadBlockDev::BLOCK_COUNT as usize],
            bad,
        }
    }
}
impl Storage for AppendFailDev {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BadBlockDev::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = BadBlockDev::BLOCK_COUNT;
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
        if block == self.bad && off != 0 {
            return Err(()); // worn for in-place appends, fine for a fresh compaction
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

/// Sub-case 2: an in-place append hits a worn *active* block. The append
/// must not be fatal; the commit falls back to a relocating compaction that
/// rebuilds the readable active block's live set onto a fresh block and
/// evicts the worn block (eager eviction). `mkdir /d` lands at `{2,3}` with
/// block 2 the active half; marking block 2 worn-for-appends makes the first
/// in-place append to `/d` fail and fall back.
#[test]
fn append_fallback_survives_worn_active() {
    let mut storage = AppendFailDev::new(2);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    // mkdir writes block 2's init commit at offset 0 (allowed) and appends
    // /d's entry to the root at a non-zero offset on block 0 (allowed).
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // The first write appends f00 to block 2 at a non-zero offset, which
    // fails; the fallback relocates /d off the worn block. Subsequent writes
    // land on the relocated (good) active half.
    let n = 6usize;
    for i in 0..n {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"y", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} should survive the worn active block: {e:?}"));
    }

    let check = |fs: &mut Fs<AppendFailDev>, a: &mut [u8], b: &mut [u8]| {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
        assert_eq!(seen, n, "all entries survive the append-fallback relocation");
    };
    check(&mut fs, &mut a, &mut b);
    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    check(&mut fs, &mut a, &mut b);
}
