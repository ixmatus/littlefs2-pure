//! Reachable-pair budget guard for `mkdir` (`lfs-43o`).
//!
//! Each subdirectory is a new reachable metadata pair. The mount-time
//! walks (allocator scan, gstate accumulation, deorphan) enumerate the
//! reachable forest into fixed `MAX_QUEUED_PAIRS` arrays, so a forest
//! larger than the budget is unmountable. `mkdir` must refuse before
//! creating the pair that would push past the budget — otherwise it
//! produces an image its own `Fs::mount` rejects with `OutOfRange`, losing
//! every directory created up to that point on the next remount.
//!
//! A 128-block device so the pair budget bites well before block
//! exhaustion.

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
fn mkdir_stops_at_the_pair_budget_with_a_mountable_image() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();

    // Create sibling directories until the budget rejects the next one.
    // With 128 blocks free the reject must be the pair budget, not block
    // exhaustion, and it must be a clean OutOfRange (never Corrupt).
    let mut made = 0u32;
    let mut hit_limit = false;
    for i in 0..200u32 {
        let name = format!("/d{i:03}");
        match fs.mkdir(Path::new(&name).unwrap(), &mut a, &mut b) {
            Ok(()) => made += 1,
            Err(Error::OutOfRange) => {
                hit_limit = true;
                break;
            }
            Err(e) => panic!("mkdir d{i:03} failed unexpectedly: {e:?}"),
        }
    }
    assert!(hit_limit, "the pair budget should bound sibling directories");
    assert!(made >= 4, "a few directories should be creatable (got {made})");

    // The rejected mkdir changed nothing: every directory that was created
    // still resolves, on this handle and after a fresh mount — the mount
    // walks must be able to enumerate the whole forest.
    let check = |fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]| {
        for i in 0..made {
            let name = format!("/d{i:03}");
            assert!(
                fs.exists(Path::new(&name).unwrap(), a, b).unwrap(),
                "directory d{i:03} must survive",
            );
        }
    };
    check(&mut fs, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).expect("image must remain mountable");
    check(&mut fs, &mut a, &mut b);
}
