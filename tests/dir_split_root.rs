//! Root directory growth across HardTail continuation pairs (`lfs-cvh.5`).
//!
//! The root pair `{0, 1}` is the superblock anchor and cannot relocate,
//! but it splits like any other directory when its entries overflow: the
//! superblock entry (id 0) stays in the lower half, so `{0, 1}` remains the
//! mount anchor with a HardTail to a continuation holding the overflow.
//! A superblock-expansion fullness guard keeps the root from growing its
//! (unreclaimable) chain into the last free blocks.

use littlefs2_pure::{Fs, Path, Storage};

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

#[test]
fn root_grows_past_one_pair_via_split() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();

    // Far more small inline files than one 256-byte pair (plus the
    // superblock) can hold, so the root must split across continuations.
    let target = 40usize;
    for i in 0..target {
        let name = format!("/f{i:03}");
        fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("root entry {i} should fit once root splits: {e:?}"));
    }

    // Every entry enumerates (the superblock is not a user entry) and reads
    // back, here and after a fresh remount that re-reads {0,1} and chases
    // the root's HardTail.
    let check = |fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]| {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/").unwrap(), |_e| seen += 1, a, b).unwrap();
        assert_eq!(seen, target, "every root entry must enumerate across the split");
        for i in 0..target {
            let name = format!("/f{i:03}");
            let mut out = [0u8; 1];
            let n = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b).unwrap();
            assert_eq!((n, out[0]), (1, b'v'), "root entry {i} reads back");
        }
    };
    check(&mut fs, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    check(&mut fs, &mut a, &mut b);
}
