//! Regression for `lfs-opt` (2026-05-29 review): the block-allocator
//! lookahead cache must never hand out a live block, and freed blocks
//! must remain allocatable (reclaimed by the rescan-on-exhaustion path
//! even though the cache over-marks them until then).
//!
//! The cache is an over-approximation of in-use blocks: it can mark a
//! freed block as still-used (benign) but never a live block as free.
//! This test drives create / delete / re-create churn on a small device
//! so that re-creation can only succeed by reclaiming freed blocks, and
//! checks content integrity throughout. A double-allocation bug would
//! corrupt a surviving file; a failure to reclaim would surface as a
//! spurious `OutOfRange`.

use littlefs2_pure::{Error, Fs, Path, Storage};

/// 64-block, 256-byte RAM device, bounds-checked.
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
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let start = (block as usize) * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

fn buf() -> [u8; Dev::BLOCK_SIZE] {
    [0u8; Dev::BLOCK_SIZE]
}

#[test]
fn create_delete_recreate_churn_reclaims_and_preserves_integrity() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();

    // Each file is a 2-block CTZ file (300 bytes) so creation consumes
    // real blocks and the device fills quickly.
    let content = |seed: u8| [seed; 300];

    // Phase 1: fill until OutOfRange.
    let mut created: Vec<u32> = Vec::new();
    for i in 0..64u32 {
        let name = format!("/f{i:02}");
        match fs.write_to_path(Path::new(&name).unwrap(), &content(i as u8), &mut a, &mut b) {
            Ok(()) => created.push(i),
            Err(Error::OutOfRange) => break,
            Err(e) => panic!("unexpected error creating f{i:02}: {e:?}"),
        }
    }
    assert!(created.len() >= 4, "device should hold a few files, got {}", created.len());

    // Phase 2: delete every other created file (frees their CTZ blocks).
    let mut survivors: Vec<u32> = Vec::new();
    for (k, &i) in created.iter().enumerate() {
        let name = format!("/f{i:02}");
        if k % 2 == 0 {
            fs.remove_at_path(Path::new(&name).unwrap(), &mut a, &mut b).unwrap();
        } else {
            survivors.push(i);
        }
    }

    // Phase 3: create new files. This can only succeed by reclaiming the
    // blocks freed in phase 2 (the device was full). A failure to reclaim
    // would be a spurious OutOfRange; a double-allocation would later
    // corrupt a survivor.
    let mut recreated: Vec<u32> = Vec::new();
    for j in 100..164u32 {
        let name = format!("/f{j:02}");
        match fs.write_to_path(Path::new(&name).unwrap(), &content(j as u8), &mut a, &mut b) {
            Ok(()) => recreated.push(j),
            Err(Error::OutOfRange) => break,
            Err(e) => panic!("unexpected error recreating f{j:02}: {e:?}"),
        }
    }
    assert!(!recreated.is_empty(), "freed blocks must be reclaimable for new files");

    // Phase 4: every survivor and every recreated file reads back exactly
    // (no block was handed to two owners).
    let verify = |fs: &mut Fs<Dev>, ids: &[u32], a: &mut [u8], b: &mut [u8]| {
        for &i in ids {
            let name = format!("/f{i:02}");
            let mut out = [0u8; 300];
            let n = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b).unwrap();
            assert_eq!(n, 300, "f{i:02} size");
            assert!(out.iter().all(|&x| x == i as u8), "f{i:02} content corrupted");
        }
    };
    verify(&mut fs, &survivors, &mut a, &mut b);
    verify(&mut fs, &recreated, &mut a, &mut b);

    // Phase 5: a fresh remount sees the same consistent state.
    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    verify(&mut fs, &survivors, &mut a, &mut b);
    verify(&mut fs, &recreated, &mut a, &mut b);
}
