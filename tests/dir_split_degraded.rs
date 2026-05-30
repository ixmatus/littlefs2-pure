//! Degraded-split fallback for `lfs-cvh` directory splitting.
//!
//! `compute_split_index` targets half a block, so any compaction of a pair
//! holding more than half a block's worth of live entries wants to split.
//! A pair fills past half a block through in-place appends, so a later
//! removal or update that triggers a compaction wants to split too — even
//! though the remaining entries still fit one block. On a full device that
//! split cannot allocate a continuation. Rather than fail, the compaction
//! degrades to a single-block commit (the C reference's "unable to split"
//! fallback): the operation lands, and only a genuine over-one-block
//! overflow returns `OutOfRange`.
//!
//! Without the fallback, deleting an entry from a split directory on a full
//! device returned `OutOfRange` and the entry could not be removed —
//! wedging the filesystem at capacity.

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

#[test]
fn delete_from_split_dir_on_full_device_succeeds() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // Fill `/d` with 300-byte CTZ files until the device is full. The
    // directory splits across several pairs; the data blocks pack the rest
    // of the device.
    let content = |s: u8| [s; 300];
    let mut created: Vec<u32> = Vec::new();
    for i in 0..200u32 {
        let name = format!("/d/f{i:03}");
        match fs.write_to_path(Path::new(&name).unwrap(), &content(i as u8), &mut a, &mut b) {
            Ok(()) => created.push(i),
            Err(Error::OutOfRange) => break,
            Err(e) => panic!("create f{i:03}: {e:?}"),
        }
    }
    assert!(created.len() >= 12, "device should hold a split directory of files");

    // Delete every other file. On a full device the owning pair's
    // compaction wants to split but cannot allocate; each delete must still
    // succeed via the degraded single-block fallback.
    let mut survivors: Vec<u32> = Vec::new();
    for (k, &i) in created.iter().enumerate() {
        let name = format!("/d/f{i:03}");
        if k % 2 == 0 {
            fs.remove_at_path(Path::new(&name).unwrap(), &mut a, &mut b)
                .unwrap_or_else(|e| panic!("delete f{i:03} on full device failed: {e:?}"));
        } else {
            survivors.push(i);
        }
    }

    // The freed data blocks are reclaimable: at least one new file lands.
    let mut recreated = 0;
    for j in 500..600u32 {
        let name = format!("/d/f{j:03}");
        match fs.write_to_path(
            Path::new(&name).unwrap(),
            &content((j & 0xff) as u8),
            &mut a,
            &mut b,
        ) {
            Ok(()) => recreated += 1,
            Err(Error::OutOfRange) => break,
            Err(e) => panic!("recreate f{j:03}: {e:?}"),
        }
    }
    assert!(recreated > 0, "freed blocks must be reclaimable after deletes");

    // Every survivor still reads back its content exactly (no block was
    // handed to two owners), on this handle and after a fresh remount.
    let verify = |fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]| {
        for &i in &survivors {
            let name = format!("/d/f{i:03}");
            let mut out = [0u8; 300];
            let n = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b).unwrap();
            assert_eq!(n, 300, "f{i:03} size");
            assert!(out.iter().all(|&x| x == i as u8), "f{i:03} content corrupted");
        }
    };
    verify(&mut fs, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    verify(&mut fs, &mut a, &mut b);
}
