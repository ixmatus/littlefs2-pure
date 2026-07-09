//! Review M9 (`lfs-yka`): a CTZ write whose freshly-programmed chain is
//! longer than the commit-internal exclusion arrays can hold must still
//! publish correctly.
//!
//! When the metadata commit that publishes a CTZ file relocates or
//! splits, `apply_op_to_pair_inner` copied the write's whole in-flight
//! chain into a fixed exclusion array so the internal allocation could
//! not reuse a chain block. Those arrays are bounded (the largest, the
//! split-continuation array, holds `2 + MAX_QUEUED_PAIRS +
//! MAX_BAD_BLOCK_RETRIES + 1 = 43` entries), so a chain longer than that
//! overflowed the bound check and returned `OutOfRange`. The fix carries
//! the chain as `(head, size)` coordinates walked on demand (the review C9
//! mechanism) instead of a materialized block list, so no exclusion array
//! ever receives the chain and the publish succeeds for any chain length,
//! uniform with the streaming-append path.
//!
//! Verification note: the overflow's user impact is narrower than the
//! finding suggested. `split_directory_pair`'s `OutOfRange` is caught by
//! its caller (`src/fs.rs`, the "unable to split" degrade-to-compaction
//! fallback), so the overflow surfaces as a hard write failure only when
//! the big-chain write is *precisely* the commit forcing a genuine split
//! whose one-block fallback also cannot fit. That timing was not
//! reproducible at test scale; this test instead pins the positive
//! guarantee the fix delivers: a CTZ file whose chain (here ~45 blocks)
//! exceeds every exclusion array writes and reads back exact across a
//! sweep of directory fill levels, including runs whose publish commit
//! splits the root and reaches the split-continuation array. It is a
//! coverage guard, not a pre/post regression reproducer.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{ctz, Fs, Path};

extern crate alloc;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

/// 96-block device: room for the root pair, a split continuation, and a
/// ~44-block CTZ chain with generous slack. Plain copy-on-program (no NOR
/// AND); this test is about exclusion-array capacity, not torn writes.
struct BigStorage {
    data: Vec<u8>,
}
impl BigStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 96;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}
impl Storage for BigStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
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
fn ctz_write_with_large_chain_survives_commit_internal_alloc() {
    let content_len = 11_000usize;
    // The chain must exceed the largest commit-internal exclusion array
    // (43 slots, so an in-flight chain of 42+ blocks overflows it).
    let chain_blocks = ctz::block_count(content_len as u32, BigStorage::BLOCK_SIZE as u32);
    assert!(chain_blocks >= 42, "chain must exceed the largest array (got {chain_blocks})");
    let content: Vec<u8> = (0..content_len).map(|i| (i & 0xff) as u8).collect();

    // Sweep the number of tiny root fillers so at least one run makes the
    // big CTZ entry the commit that forces the root pair to split, copying
    // the whole in-flight chain into the split-continuation array.
    for n_fill in 0..24u32 {
        let mut storage = BigStorage::new();
        let mut scratch = vec![0u8; BigStorage::BLOCK_SIZE];
        Fs::format(&mut storage, &mut scratch).unwrap();
        let mut ba = vec![0u8; BigStorage::BLOCK_SIZE];
        let mut bb = vec![0u8; BigStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = vec![0u8; BigStorage::BLOCK_SIZE];
        let mut b = vec![0u8; BigStorage::BLOCK_SIZE];

        for i in 0..n_fill {
            let name = format!("/f{i:02}");
            fs.write_to_path(p(&name), b"x", &mut a, &mut b).unwrap();
        }

        // The write under test: its chain is programmed, then the metadata
        // commit into the (possibly full) root may split, copying the whole
        // in-flight chain into the bounded split-continuation array. It must
        // not fail with a spurious OutOfRange.
        fs.write_to_path(p("/big"), &content, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("n_fill={n_fill}: big CTZ write failed with {e:?}"));

        // Read it back exact.
        let size = fs.size_of(p("/big"), &mut a, &mut b).unwrap();
        assert_eq!(size as usize, content_len, "n_fill={n_fill}: size");
        let mut out = vec![0u8; content_len];
        let n = fs.read_at_path(p("/big"), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(n, content_len, "n_fill={n_fill}: read count");
        assert_eq!(out, content, "n_fill={n_fill}: /big content mismatch");

        // The tiny fillers survive the split too.
        for i in 0..n_fill {
            let name = format!("/f{i:02}");
            assert!(fs.exists(p(&name), &mut a, &mut b).unwrap(), "n_fill={n_fill}: {name} lost");
        }
    }
}
