//! Block reclaim for split directories (`lfs-fvw`).
//!
//! The allocator's used-block scan must be splice-correct: when a file is
//! deleted from a split directory, its CTZ data blocks must become
//! reclaimable even though the delete appended a Delete tag rather than
//! compacting the owning pair (split directories hold their pairs at half
//! a block, so deletes have room to append). A raw tag scan would keep
//! marking the freed chain — via the deleted entry's stale `CtzStruct`
//! tag — until the pair next compacts, so recreates would spuriously fail
//! with `OutOfRange` even though space was freed.
//!
//! This fills a split `/d` with CTZ files until the device is full,
//! deletes most of them, and asserts nearly all the freed space comes
//! back as new files.

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
fn deleting_files_from_a_split_dir_reclaims_their_blocks() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // Fill `/d` with two-block CTZ files until the device is full; the
    // directory splits across several pairs along the way.
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
    assert!(created.len() >= 12, "device should hold a split directory of CTZ files");

    // Delete all but the last two. Each freed file is a two-block CTZ
    // chain whose entry was removed by an appended Delete tag, not a
    // compaction — so a raw allocator scan would keep its blocks marked.
    let keep = 2;
    let deleted = created.len() - keep;
    for &i in &created[..deleted] {
        let name = format!("/d/f{i:03}");
        fs.remove_at_path(Path::new(&name).unwrap(), &mut a, &mut b).unwrap();
    }

    // Recreate: most of the freed two-block chains must come back (a few
    // blocks go to the new files' own continuation pairs). With the
    // over-marking bug the freed blocks stay marked and only a handful
    // recreate; splice-correct scanning reclaims the rest. The threshold
    // sits well between the two regimes (measured: ~5 buggy, ~19 fixed of
    // 23 deleted).
    let mut recreated = 0usize;
    for j in 500..(500 + deleted as u32 + 8) {
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
    assert!(
        recreated >= deleted * 2 / 3,
        "freed CTZ blocks must be reclaimable: deleted {deleted}, recreated only {recreated}",
    );

    // The kept files still read back exactly, here and after a remount.
    let verify = |fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]| {
        for &i in &created[deleted..] {
            let name = format!("/d/f{i:03}");
            let mut out = [0u8; 300];
            let n = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b).unwrap();
            assert_eq!(n, 300);
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
