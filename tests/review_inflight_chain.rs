//! Reproducer for the 2026-06 deep review finding C9 (bead lfs-0vy):
//! commit-time relocation and retry allocations must not reallocate
//! the in-flight CTZ chain they are about to publish.
//!
//! `File::write` programs new chain blocks immediately and defers the
//! metadata commit to `sync`. The commit (`commit_update_ctz`) entered
//! `apply_op_to_pair_inner` with an empty inflight set, so a wear
//! relocation, a worn-block retry, or a split continuation allocating
//! INSIDE that commit could rescan the forest (which only sees
//! committed blocks) and hand out the very chain blocks the commit is
//! publishing; erasing them destroys the file's data at the moment it
//! becomes durable. Violates ADR-0010's own exclusion invariant.
//!
//! Trigger shape: a small wear-levelled device where each overwrite
//! orphans the previous chain. The allocator cache over-marks the
//! orphans, so commit-path allocations miss the cache and rescan; the
//! rescan frees the orphans but cannot see the in-flight chain, whose
//! blocks sit lowest and get handed to the relocation.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Fs, OpenOptions, Path};

mod common;

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// 16-block wear-levelled geometry: tight enough that overwrite churn
/// exhausts the cached free view, big enough for root + a subdir + a
/// 3-block chain + slack.
#[derive(Debug)]
struct SmallWearStorage {
    data: Vec<u8>,
    erase_counts: Vec<u32>,
}

impl SmallWearStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 16;
    /// Programs to a block fail once it has been erased this many
    /// times. The tests pre-wear one chosen block (set its count to
    /// the threshold) so exactly that block refuses programs while
    /// every other block stays healthy.
    const WEAR_OUT_ERASES: u32 = 1000;
    fn new() -> Self {
        Self {
            data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize],
            erase_counts: vec![0u32; Self::BLOCK_COUNT as usize],
        }
    }
}

impl Storage for SmallWearStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = 1;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if self.erase_counts[block as usize] >= Self::WEAR_OUT_ERASES {
            return Err(()); // worn out
        }
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        self.erase_counts[block as usize] += 1;
        let start = (block as usize) * Self::BLOCK_SIZE;
        for v in &mut self.data[start..start + Self::BLOCK_SIZE] {
            *v = 0xFF;
        }
        Ok(())
    }
}

/// C9: overwrite a CTZ file in a wear-levelled subdirectory through
/// the stateful `File` handle. One pre-worn block sits in the free
/// pool: when a commit-internal relocation picks it as the fresh
/// candidate, the program fails, the retry clears the allocator cache
/// and rescans, and the rescan (which only sees committed blocks)
/// hands the relocation an in-flight chain block of the very file the
/// commit is publishing; erasing it destroys acknowledged data.
///
/// The pre-worn block's address must coincide with the relocation's
/// candidate pick, which depends on layout, so sweep it across the
/// free pool; healthy runs are vacuously fine, and any acknowledged
/// publish must read back exact in every run.
#[test]
fn c9_sync_relocation_does_not_reallocate_inflight_chain() {
    for worn in 2..SmallWearStorage::BLOCK_COUNT {
        let mut storage = SmallWearStorage::new();
        let mut scratch = vec![0u8; SmallWearStorage::BLOCK_SIZE];
        Fs::format(&mut storage, &mut scratch).unwrap();
        // Pre-wear the chosen block: it reads as free and erases fine,
        // but every program to it fails.
        storage.erase_counts[worn as usize] = SmallWearStorage::WEAR_OUT_ERASES;
        let mut buf_a = vec![0u8; SmallWearStorage::BLOCK_SIZE];
        let mut buf_b = vec![0u8; SmallWearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = vec![0u8; SmallWearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; SmallWearStorage::BLOCK_SIZE];

        // A worn block under the mkdir's own pair allocation fails the
        // setup itself (a separate, documented limitation: the initial
        // pair write has no relocation anchor). Such worn positions do
        // not exercise the scenario under test; skip them.
        if fs.mkdir(p("/d"), &mut a, &mut b).is_err() {
            continue;
        }

        let mut last_published: Option<Vec<u8>> = None;
        for round in 0..40u32 {
            // Distinct content per round so stale blocks are detectable.
            let fill = b'A' + (round % 26) as u8;
            let content = vec![fill; 420 + (round % 32) as usize];

            let published = {
                let Ok(mut f) = fs.open(
                    p("/d/v"),
                    OpenOptions::new().write(true).create(true).truncate(true),
                    &mut a,
                    &mut b,
                ) else {
                    break;
                };
                // The commit under test: the chain below is programmed
                // but unreferenced until close()'s sync publishes it. A
                // worn-block failure may legitimately surface as Err;
                // ACKNOWLEDGED publishes are what must read back exact.
                f.write(&content, &mut a, &mut b).is_ok() && f.close(&mut a, &mut b).is_ok()
            };
            if published {
                last_published = Some(content.clone());
            }

            if let Some(expected) = &last_published {
                let mut out = vec![0u8; 600];
                let n =
                    fs.read_at_path(p("/d/v"), 0, &mut out, &mut a, &mut b).unwrap_or_else(|e| {
                        panic!("worn {worn}, round {round}: read-back failed: {e:?}")
                    });
                assert_eq!(
                    n,
                    expected.len(),
                    "worn {worn}, round {round}: size mismatch (published = {published})"
                );
                assert!(
                    out[..n] == expected[..],
                    "worn {worn}, round {round}: C9 — acknowledged content corrupted \
                     (a commit-path allocation reallocated an in-flight chain \
                     block); got {:02x?}, expected fill {:#04x}",
                    &out[..8.min(n)],
                    expected[0],
                );
            }
        }
        assert!(
            last_published.is_some(),
            "worn {worn}: scenario never published anything; storage too hostile"
        );

        // Remount: consistent with the last acknowledged publish.
        let expected = last_published.unwrap();
        let storage = fs.into_storage();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
            .unwrap_or_else(|e| panic!("worn {worn}: final remount failed: {e:?}"));
        let mut out = vec![0u8; 600];
        let n = fs
            .read_at_path(p("/d/v"), 0, &mut out, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("worn {worn}: final read failed: {e:?}"));
        assert_eq!(n, expected.len(), "worn {worn}: size diverged across remount");
        assert!(out[..n] == expected[..], "worn {worn}: content corrupt after remount");
    }
}

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// Worn-set storage: programs to armed blocks fail; everything else
/// is healthy. The test arms exactly the blocks that steer the commit
/// down the failure-relocation retry path.
#[derive(Debug)]
struct ArmedStorage {
    data: Vec<u8>,
    worn: Vec<u32>,
}

impl ArmedStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 16;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize], worn: vec![] }
    }
}

impl Storage for ArmedStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = -1; // only the failure-driven relocation fires

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if self.worn.contains(&block) {
            return Err(());
        }
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE;
        for v in &mut self.data[start..start + Self::BLOCK_SIZE] {
            *v = 0xFF;
        }
        Ok(())
    }
}

/// C9, deterministic: steer the publish commit down the worn-retry
/// rescan. Arm the directory pair's ALTERNATE (so the compaction's
/// anchor write fails and the commit enters the failure-driven
/// relocation) and the allocator's NEXT free pick (so the first fresh
/// candidate fails, clearing the cache and forcing the authoritative
/// rescan). The rescan sees only committed blocks; without the
/// in-flight exclusion it hands the relocation the lowest free blocks,
/// which are exactly the chain blocks of the file being published, and
/// erasing one destroys acknowledged data.
///
/// `k` (log-filling pad rewrites before the publish) is swept so some
/// run lands the publish commit on a compaction; runs whose publish
/// appends in place (no compaction, no relocation) pass vacuously, and
/// runs whose publish fails honestly keep the previous state.
#[test]
fn c9_failure_relocation_retry_rescan_excludes_inflight_chain() {
    for k in 0..10u32 {
        let mut storage = ArmedStorage::new();
        let mut scratch = vec![0u8; ArmedStorage::BLOCK_SIZE];
        Fs::format(&mut storage, &mut scratch).unwrap();
        let mut buf_a = vec![0u8; ArmedStorage::BLOCK_SIZE];
        let mut buf_b = vec![0u8; ArmedStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = vec![0u8; ArmedStorage::BLOCK_SIZE];
        let mut b = vec![0u8; ArmedStorage::BLOCK_SIZE];

        fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
        // Fill /d's log toward the compaction boundary.
        for i in 0..k {
            let pad = vec![b'p'; 8 + (i % 16) as usize];
            fs.write_to_path(p("/d/pad"), &pad, &mut a, &mut b).unwrap();
        }

        // Locate /d's pair and its alternate block.
        let (d_pair, alternate) = {
            let r = fs.resolve(p("/d"), &mut a, &mut b).unwrap();
            let pa = u32::from_le_bytes([
                r.struct_body[0],
                r.struct_body[1],
                r.struct_body[2],
                r.struct_body[3],
            ]);
            let pb = u32::from_le_bytes([
                r.struct_body[4],
                r.struct_body[5],
                r.struct_body[6],
                r.struct_body[7],
            ]);
            let st = fs.storage_mut();
            let mut ba = vec![0u8; ArmedStorage::BLOCK_SIZE];
            let mut bb = vec![0u8; ArmedStorage::BLOCK_SIZE];
            st.read(pa, 0, &mut ba).unwrap();
            st.read(pb, 0, &mut bb).unwrap();
            let pair = littlefs2_pure::meta::MetadataPair::parse(
                littlefs2_pure::BlockAddress::new(pa),
                &ba,
                littlefs2_pure::BlockAddress::new(pb),
                &bb,
            )
            .unwrap();
            ((pa, pb), pair.alternate_block.as_u32())
        };

        // The publish's chain takes the two lowest free blocks; the
        // relocation's first fresh candidate is the third. Used so far:
        // root {0,1} plus /d's pair (pad is inline; no data blocks).
        let used = [0u32, 1, d_pair.0, d_pair.1];
        let mut free = (0..ArmedStorage::BLOCK_COUNT).filter(|x| !used.contains(x));
        let _chain1 = free.next().unwrap();
        let _chain2 = free.next().unwrap();
        let f1 = free.next().unwrap();

        // Arm: the anchor write fails (enter the failure relocation),
        // and the first fresh candidate fails (force the retry rescan).
        fs.storage_mut().worn = vec![alternate, f1];

        let content = vec![b'Z'; 420];
        let published = {
            match fs.open(
                p("/d/v"),
                OpenOptions::new().write(true).create(true).truncate(true),
                &mut a,
                &mut b,
            ) {
                Ok(mut f) => {
                    f.write(&content, &mut a, &mut b).is_ok() && f.close(&mut a, &mut b).is_ok()
                }
                Err(_) => false,
            }
        };
        fs.storage_mut().worn = vec![];

        if published {
            let mut out = vec![0u8; 600];
            let n = fs
                .read_at_path(p("/d/v"), 0, &mut out, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("k={k}: read-back of acknowledged publish: {e:?}"));
            assert_eq!(n, content.len(), "k={k}: size mismatch on acknowledged publish");
            assert!(
                out[..n] == content[..],
                "k={k}: C9 — acknowledged content corrupted; the failure-relocation \
                 retry rescan handed an in-flight chain block to the relocation; \
                 got {:02x?}",
                &out[..8],
            );
        }
    }
}
