//! Review M1 (`lfs-tx8`): rmdir of an empty but multi-pair
//! (HardTail-split) directory must not graft the directory's continuation
//! pairs onto the thread predecessor.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Error, Fs, MetadataPair, Path};

extern crate alloc;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

struct Dev {
    data: Vec<u8>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 32;
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
        let s = block as usize * Self::BLOCK_SIZE + off as usize;
        if s + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[s..s + buf.len()]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let s = block as usize * Self::BLOCK_SIZE + off as usize;
        if s + data.len() > self.data.len() {
            return Err(());
        }
        self.data[s..s + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let s = block as usize * Self::BLOCK_SIZE;
        for v in &mut self.data[s..s + Self::BLOCK_SIZE] {
            *v = 0xFF;
        }
        Ok(())
    }
    fn sync(&mut self) -> Result<(), ()> {
        Ok(())
    }
}

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// Read `/d`'s metadata pair and report whether its head is HardTail-split
/// (multi-pair). Returns None if `/d` is absent.
fn dir_is_multipair(fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]) -> Option<bool> {
    let r = fs.resolve(p("/d"), a, b).ok()?;
    let body = r.struct_body;
    if body.len() != 8 {
        return None;
    }
    let da = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let db = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let mut ba = vec![0u8; Dev::BLOCK_SIZE];
    let mut bb = vec![0u8; Dev::BLOCK_SIZE];
    fs.storage_mut().read(da, 0, &mut ba).ok()?;
    fs.storage_mut().read(db, 0, &mut bb).ok()?;
    let pair = MetadataPair::parse(
        littlefs2_pure::BlockAddress::new(da),
        &ba,
        littlefs2_pure::BlockAddress::new(db),
        &bb,
    )
    .ok()?;
    Some(pair.reader.is_hard_tail())
}

#[test]
fn rmdir_empty_multipair_directory_does_not_graft_continuation() {
    let mut storage = Dev::new();
    let mut scratch = vec![0u8; Dev::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut ba = vec![0u8; Dev::BLOCK_SIZE];
    let mut bb = vec![0u8; Dev::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = vec![0u8; Dev::BLOCK_SIZE];
    let mut b = vec![0u8; Dev::BLOCK_SIZE];

    // A sibling single-pair directory whose thread must stay intact.
    fs.mkdir(p("/sibling"), &mut a, &mut b).unwrap();
    fs.write_to_path(p("/sibling/keep"), b"v", &mut a, &mut b).unwrap();

    // Split /d by filling it, then empty it.
    fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
    let n = 24;
    for i in 0..n {
        fs.write_to_path(p(&format!("/d/e{i:02}")), b"x", &mut a, &mut b).unwrap();
    }
    let split = dir_is_multipair(&mut fs, &mut a, &mut b);
    for i in 0..n {
        fs.remove_at_path(p(&format!("/d/e{i:02}")), &mut a, &mut b).unwrap();
    }
    let empty_multipair = dir_is_multipair(&mut fs, &mut a, &mut b);
    // Diagnostic surfaced on failure so we know whether crate ops even
    // reach the M1 state.
    assert_eq!(split, Some(true), "fill did not split /d (raise n?)");
    assert_eq!(empty_multipair, Some(true), "removing entries collapsed /d; M1 state not reached");

    let rmdir_result = fs.rmdir(p("/d"), &mut a, &mut b);

    if empty_multipair == Some(true) {
        // The M1 case: rmdir must refuse (NotEmpty), matching C, rather
        // than graft /d's continuation onto the thread.
        assert_eq!(
            rmdir_result,
            Err(Error::NotEmpty),
            "rmdir of an empty multi-pair directory must refuse, not graft"
        );
    }

    // Regardless of the outcome, the filesystem must remain consistent:
    // remount and confirm the sibling and its file survived intact.
    let storage = fs.into_storage();
    let mut ba2 = vec![0u8; Dev::BLOCK_SIZE];
    let mut bb2 = vec![0u8; Dev::BLOCK_SIZE];
    let mut fs2 = Fs::mount(storage, &mut ba2, &mut bb2).expect("remount after rmdir");
    let mut a2 = vec![0u8; Dev::BLOCK_SIZE];
    let mut b2 = vec![0u8; Dev::BLOCK_SIZE];
    assert!(fs2.exists(p("/sibling"), &mut a2, &mut b2).unwrap(), "sibling lost");
    assert!(fs2.exists(p("/sibling/keep"), &mut a2, &mut b2).unwrap(), "sibling/keep lost");
    let mut count = 0usize;
    fs2.list_dir(p("/sibling"), |_| count += 1, &mut a2, &mut b2).unwrap();
    assert_eq!(count, 1, "sibling gained a grafted entry");
}
