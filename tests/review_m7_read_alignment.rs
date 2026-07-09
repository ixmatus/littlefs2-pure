//! Review M7 (`lfs-mqz`): the allocator's CTZ chain walk must issue only
//! `READ_SIZE`-aligned reads.
//!
//! `walk_ctz_chain` read a chain block's skip-pointer header with a 4- or
//! 8-byte read at offset 0. The `Storage` contract requires reads aligned
//! to and sized as a multiple of `READ_SIZE` (16 here), so those reads
//! violated the precondition and would fault on hardware that enforces it
//! (`MemStorage` happens to tolerate them, which is why the existing
//! suites never caught it). The fix fetches the header as a
//! `READ_SIZE`-aligned window through a block buffer.
//!
//! This storage asserts alignment on every read. The allocator walks a
//! committed CTZ chain when it rebuilds its used-set, so remounting and
//! writing a second CTZ file drives `walk_ctz_chain` over the first
//! file's chain. Before the fix the skip-pointer read panics here.
//!
//! Scope: this pins the allocator walk only (the finding's location). The
//! CTZ *read* path (`ctz::collect_chain_blocks`) issues the same
//! sub-`READ_SIZE` skip-pointer reads and is tracked as a separate
//! discovered follow-up, so this test deliberately does not read file
//! content back (it checks existence via aligned full-block metadata
//! reads).

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Fs, Path};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

struct AlignCheckStorage {
    data: Vec<u8>,
}
impl AlignCheckStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 16;
    const READ_SIZE: usize = 16;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}
impl Storage for AlignCheckStorage {
    type Error = ();
    const READ_SIZE: usize = Self::READ_SIZE;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        // The contract every real read-aligned device enforces.
        assert_eq!(off as usize % Self::READ_SIZE, 0, "read offset {off} not READ_SIZE-aligned");
        assert_eq!(buf.len() % Self::READ_SIZE, 0, "read len {} not READ_SIZE-aligned", buf.len());
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let start = block as usize * Self::BLOCK_SIZE;
        for v in &mut self.data[start..start + Self::BLOCK_SIZE] {
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

#[test]
fn allocator_ctz_walk_reads_are_aligned() {
    let body: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();

    let mut storage = AlignCheckStorage::new();
    let mut scratch = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut ba = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
        let mut bb = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
        let mut b = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
        // A multi-block CTZ file: its chain carries skip-pointer headers.
        fs.write_to_path(p("/a.bin"), &body, &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // Remount with an empty allocator cache, then write a second CTZ file.
    // Its allocation rebuilds the used-set, walking /a.bin's committed
    // chain and reading each block's skip-pointer header. Every such read
    // must be READ_SIZE-aligned or the storage above panics.
    let mut ba = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
    let mut bb = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
    let mut b = vec![0u8; AlignCheckStorage::BLOCK_SIZE];
    fs.write_to_path(p("/b.bin"), &body, &mut a, &mut b).unwrap();

    // Both files are present (checked via aligned full-block metadata
    // reads). The point of the test is that the allocation scan above
    // walked /a.bin's chain without a misaligned skip-pointer read.
    assert!(fs.exists(p("/a.bin"), &mut a, &mut b).unwrap());
    assert!(fs.exists(p("/b.bin"), &mut a, &mut b).unwrap());
}
