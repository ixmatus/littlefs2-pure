//! Review M8 (`lfs-ru2`): a failed flush in `NorAlignedStorage` must not
//! leave a dirty window that every subsequent flush retries. One worn
//! block would otherwise poison every later flush with the same failing
//! program, so a healthy write after the failure could never land.
//!
//! Drive the wrapper directly: buffer a program into a block whose backing
//! program fails, sync (the flush fails), then program and sync a healthy
//! block. Post-fix the healthy write lands; before the fix the poisoned
//! dirty window retries the failing program on the next window load and
//! blocks it.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::NorAlignedStorage;

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// Backing storage that fails `program` on one chosen block; everything
/// else behaves normally.
struct FailBlockStorage {
    data: Vec<u8>,
    fail_block: u32,
}
impl FailBlockStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 8;
    fn new(fail_block: u32) -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize], fail_block }
    }
}
impl Storage for FailBlockStorage {
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
        if block == self.fail_block {
            return Err(()); // worn block: programs fail
        }
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

#[test]
fn failed_flush_does_not_poison_later_flushes() {
    let worn = 2u32;
    let healthy = 3u32;
    let mut nor = NorAlignedStorage::new(FailBlockStorage::new(worn)).unwrap();

    nor.erase(worn).unwrap();
    nor.erase(healthy).unwrap();

    let payload = [0u8; FailBlockStorage::PROG_SIZE];

    // Buffer a program into the worn block, then flush: the backing
    // program fails and the error surfaces here.
    nor.program(worn, 0, &payload).unwrap();
    assert!(nor.sync().is_err(), "flush of the worn block must surface the program failure");

    // A subsequent write to a healthy block must land. Before the fix, the
    // worn block's dirty window was kept, so this write's window load
    // re-flushes (and re-fails) the worn program and never reaches the
    // healthy block.
    nor.program(healthy, 0, &payload)
        .expect("healthy write must not be poisoned by the earlier failed flush");
    nor.sync().expect("healthy flush must succeed");

    // The healthy block holds the programmed bytes.
    let mut back = [0xFFu8; FailBlockStorage::PROG_SIZE];
    nor.read(healthy, 0, &mut back).unwrap();
    assert_eq!(back, payload);
}
