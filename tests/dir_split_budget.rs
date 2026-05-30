//! Graceful degradation at the reachable-pair budget for `lfs-cvh`
//! directory splitting.
//!
//! A directory grows across HardTail continuation pairs without bound in
//! principle, but the kernel's pair-enumeration walks (the allocator scan,
//! the deorphan tree set, the gstate accumulation) are bounded at
//! `MAX_QUEUED_PAIRS` reachable pairs — a fixed stack budget (ADR-0006).
//! When a single directory's chain would push the reachable set past that
//! bound, the next split's allocator scan returns `OutOfRange` *before*
//! programming anything, so the failure is clean: the write is rejected,
//! the directory keeps every entry that already landed, and the image
//! still mounts and reads back consistently.
//!
//! This uses a 128-block device so the 32-pair budget is reached while
//! free blocks remain — isolating the budget cap from plain block
//! exhaustion (which a 64-block device would hit first, since 32 pairs is
//! 64 blocks).

use littlefs2_pure::{Error, Fs, Path, Storage};

struct Dev {
    data: Vec<u8>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 128;
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
    const LOOKAHEAD_SIZE: usize = 16;
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
fn deep_chain_fails_cleanly_at_the_pair_budget() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // Grow `/d` until a write is rejected. With 128 blocks free, the
    // reject must come from the reachable-pair budget, not block
    // exhaustion. The only acceptable error is OutOfRange — never Corrupt.
    let mut created = 0usize;
    let mut hit_limit = false;
    for i in 0..1000u32 {
        let name = format!("/d/f{i:04}");
        match fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b) {
            Ok(()) => created += 1,
            Err(Error::OutOfRange) => {
                hit_limit = true;
                break;
            }
            Err(e) => panic!("entry {i} failed with {e:?}, expected clean OutOfRange"),
        }
    }
    assert!(hit_limit, "the pair budget should bound the directory before 1000 entries");
    assert!(created > 100, "many entries should land before the budget bites (got {created})");

    // The rejected write changed nothing: every entry that landed still
    // enumerates and reads back, on this handle and after a fresh mount.
    let verify = |fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]| {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
        assert_eq!(seen, created, "every landed entry must still enumerate");
        // Spot-check the first, a middle, and the last landed entry.
        for &i in &[0u32, (created as u32) / 2, created as u32 - 1] {
            let name = format!("/d/f{i:04}");
            let mut out = [0u8; 1];
            let n = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b).unwrap();
            assert_eq!((n, out[0]), (1, b'x'), "entry f{i:04} content");
        }
    };
    verify(&mut fs, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    verify(&mut fs, &mut a, &mut b);
}
