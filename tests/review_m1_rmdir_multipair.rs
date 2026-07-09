//! Review M1 (`lfs-tx8`): rmdir of an empty but multi-pair
//! (HardTail-split) directory must remove the whole directory chain
//! without grafting its continuation pairs onto the thread predecessor.
//!
//! An empty directory that has been split across HardTail continuation
//! pairs stays multi-pair (removing its entries does not collapse it), so
//! this state is reachable from the crate's own operations. rmdir must
//! still succeed (the intended contract, pinned by
//! `tests/hardtail.rs::rmdir_accepts_directory_with_empty_hardtail_chain`)
//! but must un-thread every pair of the directory: un-threading only the
//! head re-points the predecessor's tail at the head's HardTail
//! continuation, silently grafting the removed directory's leftover pairs
//! onto the predecessor's chain.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Fs, MetadataPair, Path};

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

/// Whether `path`'s directory head pair has a HardTail continuation. For
/// `/d` this reports whether it is multi-pair; for a predecessor after a
/// buggy rmdir it reports whether a continuation was grafted onto it.
fn dir_head_is_hard_tail(fs: &mut Fs<Dev>, path: &str, a: &mut [u8], b: &mut [u8]) -> Option<bool> {
    let body = fs.resolve(p(path), a, b).ok()?.struct_body;
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

    // A sibling single-pair directory that is /d's thread predecessor; its
    // chain must not gain a grafted continuation.
    fs.mkdir(p("/sibling"), &mut a, &mut b).unwrap();
    fs.write_to_path(p("/sibling/keep"), b"v", &mut a, &mut b).unwrap();

    // Split /d by filling it, then empty it (it stays multi-pair).
    fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
    let n = 24;
    for i in 0..n {
        fs.write_to_path(p(&format!("/d/e{i:02}")), b"x", &mut a, &mut b).unwrap();
    }
    assert_eq!(
        dir_head_is_hard_tail(&mut fs, "/d", &mut a, &mut b),
        Some(true),
        "fill did not split /d (raise n?)"
    );
    for i in 0..n {
        fs.remove_at_path(p(&format!("/d/e{i:02}")), &mut a, &mut b).unwrap();
    }
    assert_eq!(
        dir_head_is_hard_tail(&mut fs, "/d", &mut a, &mut b),
        Some(true),
        "removing entries collapsed /d; the empty multi-pair state is not reached"
    );

    // rmdir must succeed and drop the whole chain.
    fs.rmdir(p("/d"), &mut a, &mut b).expect("rmdir of an empty multi-pair directory must succeed");
    assert!(!fs.exists(p("/d"), &mut a, &mut b).unwrap(), "/d must be gone");

    // The predecessor must not have gained a grafted HardTail continuation.
    // The predecessor is whichever pair's tail pointed at /d's head; check
    // both the sibling and the root pair (blocks {0,1}).
    assert_eq!(
        dir_head_is_hard_tail(&mut fs, "/sibling", &mut a, &mut b),
        Some(false),
        "rmdir grafted /d's continuation onto /sibling's chain"
    );
    {
        let mut r0 = vec![0u8; Dev::BLOCK_SIZE];
        let mut r1 = vec![0u8; Dev::BLOCK_SIZE];
        fs.storage_mut().read(0, 0, &mut r0).unwrap();
        fs.storage_mut().read(1, 0, &mut r1).unwrap();
        let root = MetadataPair::parse(
            littlefs2_pure::BlockAddress::new(0),
            &r0,
            littlefs2_pure::BlockAddress::new(1),
            &r1,
        )
        .unwrap();
        assert!(!root.reader.is_hard_tail(), "rmdir grafted /d's continuation onto the root pair");
    }

    // Remount and confirm consistency: the sibling and its file survived
    // with exactly one entry, and a fresh directory (reusing /d's reclaimed
    // blocks) writes and reads back cleanly.
    let storage = fs.into_storage();
    let mut ba2 = vec![0u8; Dev::BLOCK_SIZE];
    let mut bb2 = vec![0u8; Dev::BLOCK_SIZE];
    let mut fs2 = Fs::mount(storage, &mut ba2, &mut bb2).expect("remount after rmdir");
    let mut a2 = vec![0u8; Dev::BLOCK_SIZE];
    let mut b2 = vec![0u8; Dev::BLOCK_SIZE];
    assert!(fs2.exists(p("/sibling/keep"), &mut a2, &mut b2).unwrap(), "sibling/keep lost");
    let mut count = 0usize;
    fs2.list_dir(p("/sibling"), |_| count += 1, &mut a2, &mut b2).unwrap();
    assert_eq!(count, 1, "sibling gained a grafted entry");
    assert_eq!(
        dir_head_is_hard_tail(&mut fs2, "/sibling", &mut a2, &mut b2),
        Some(false),
        "grafted continuation persisted across remount"
    );

    fs2.mkdir(p("/fresh"), &mut a2, &mut b2).unwrap();
    fs2.write_to_path(p("/fresh/f"), b"ok", &mut a2, &mut b2).unwrap();
    assert!(fs2.exists(p("/fresh/f"), &mut a2, &mut b2).unwrap());
}
