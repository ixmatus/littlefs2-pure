//! Reproducers for the 2026-06 deep review finding C8 (bead lfs-ay4):
//! a failed or torn streaming append must not poison the committed
//! CTZ tail block.
//!
//! `stream_ctz_extend` filled the committed tail block's erased region
//! BEFORE the overflow allocation and bounds checks. On an allocation
//! failure (device full) or a power loss, the metadata still said
//! `old_size` but the cells past the committed EOF were programmed;
//! the next append recomputed the same offsets and programmed
//! different bytes over them, and NOR AND-semantics silently
//! corrupted the newly appended (and acknowledged) content. Oracle:
//! the C reference never programs a committed data block twice
//! (`lfs_ctz_extend` copies a partial tail to a fresh block,
//! lfs.c:2891ff).
//!
//! The bug is invisible on a permissive RAM backing; these tests run
//! on `NorAlignedStorage<StrictNorStorage>` (AND-merge programs,
//! panic on a `0 -> 1` flip), with the torn wrapper INSIDE the NOR
//! adapter so tears land at device program granularity.

mod common;
use common::StrictNorStorage;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Fs, NorAlignedStorage, Path};

extern crate alloc;
use alloc::vec;

const BS: usize = StrictNorStorage::BLOCK_SIZE;

fn buf() -> [u8; BS] {
    [0u8; BS]
}

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// C8, the allocation-failure half: an append whose overflow
/// allocation fails (device nearly full) must leave the committed
/// tail block untouched, so a subsequent smaller append lands
/// cleanly.
///
/// Geometry: 8 blocks. Root pair takes 2, `/log` takes 2, `/filler`
/// takes 3, leaving exactly one free block. The big append needs two
/// overflow blocks and must fail; the small append fits the tail's
/// free region (with one block available for a copy-on-write if the
/// implementation needs one).
#[test]
fn c8_failed_overflow_alloc_does_not_poison_tail() {
    let mut storage = NorAlignedStorage::new(StrictNorStorage::new()).unwrap();
    let mut scratch = buf();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = buf();
    let mut b = buf();

    // /log: 300 bytes of 0xAA -> blocks {idx0: 256, idx1: 44 of 252}.
    fs.write_to_path(p("/log"), &[0xAA; 300], &mut a, &mut b).unwrap();
    // /filler: 600 bytes -> 3 blocks. One block now remains free.
    fs.write_to_path(p("/filler"), &[0xEE; 600], &mut a, &mut b).unwrap();

    // Big append: fills the tail's 208 free bytes and needs two more
    // blocks; only one is free, so the allocation must fail without
    // programming the committed tail.
    let big = [0x99u8; 600];
    let mut scratch2 = vec![0u8; 2048];
    let err = fs.append_to_path(p("/log"), &big, &mut scratch2, &mut a, &mut b);
    assert!(err.is_err(), "C8: the oversized append must fail (device nearly full)");

    // The file must still read back exactly as committed.
    let mut out = vec![0u8; 1024];
    let n = fs.read_at_path(p("/log"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300, "size must be unchanged after the failed append");
    assert!(out[..300].iter().all(|&x| x == 0xAA));

    // A small append must land cleanly. Before the fix this ANDed
    // 0x55 with the failed append's 0x99 residue (0x99 & 0x55 = 0x11)
    // or panicked the strict NOR model on a 0 -> 1 flip.
    fs.append_to_path(p("/log"), &[0x55; 50], &mut scratch2, &mut a, &mut b)
        .expect("C8: a small append after a failed one must succeed");
    let n = fs.read_at_path(p("/log"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 350);
    assert!(out[..300].iter().all(|&x| x == 0xAA), "committed prefix corrupted");
    assert!(
        out[300..350].iter().all(|&x| x == 0x55),
        "C8: appended bytes ANDed with a failed append's residue: {:02x?}",
        &out[300..310],
    );
}

/// Torn wrapper at DEVICE program granularity: sits INSIDE the
/// NOR-aligned adapter, so a tear models a power loss inside a real
/// device program, not at a cache-flush boundary.
struct TornStrictNor {
    inner: StrictNorStorage,
    trigger_at: usize,
    program_count: usize,
}

impl Storage for TornStrictNor {
    type Error = ();
    const READ_SIZE: usize = StrictNorStorage::READ_SIZE;
    const PROG_SIZE: usize = StrictNorStorage::PROG_SIZE;
    const BLOCK_SIZE: usize = StrictNorStorage::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = StrictNorStorage::BLOCK_COUNT;
    const CACHE_SIZE: usize = StrictNorStorage::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = StrictNorStorage::LOOKAHEAD_SIZE;

    fn read(&mut self, block: u32, off: u32, out: &mut [u8]) -> Result<(), ()> {
        self.inner.read(block, off, out)
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        self.program_count += 1;
        if self.program_count > self.trigger_at {
            return Err(());
        }
        self.inner.program(block, off, data)
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if self.program_count > self.trigger_at {
            return Err(());
        }
        self.inner.erase(block)
    }
}

/// C8, the power-loss half: sweep a tear across every device program
/// boundary of an append. After each tear: remount, then append
/// different bytes; the result must be exactly the committed prefix
/// plus the new bytes, never an AND of the torn append's residue.
#[test]
fn c8_torn_append_then_new_append_reads_back_exact() {
    // Seed: /log = 300 bytes of 0xAA, flushed to raw bytes.
    let seed_data = {
        let mut storage = NorAlignedStorage::new(StrictNorStorage::new()).unwrap();
        let mut scratch = buf();
        Fs::format(&mut storage, &mut scratch).unwrap();
        let mut buf_a = buf();
        let mut buf_b = buf();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = buf();
        let mut b = buf();
        fs.write_to_path(p("/log"), &[0xAA; 300], &mut a, &mut b).unwrap();
        fs.into_storage().into_inner().unwrap().data
    };

    // Count device programs for the torn scenario (one append).
    let total_calls = {
        let mut inner = StrictNorStorage::new();
        inner.data = seed_data.clone();
        let torn = TornStrictNor { inner, trigger_at: usize::MAX, program_count: 0 };
        let storage = NorAlignedStorage::new(torn).unwrap();
        let mut buf_a = buf();
        let mut buf_b = buf();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("seed mount");
        let mut a = buf();
        let mut b = buf();
        let before = fs.storage().inner().program_count;
        let mut scratch2 = vec![0u8; 2048];
        let _ = fs.append_to_path(p("/log"), &[0x55; 100], &mut scratch2, &mut a, &mut b);
        fs.storage().inner().program_count - before
    };
    assert!(total_calls > 0);

    for trigger in 1..=total_calls {
        let mut inner = StrictNorStorage::new();
        inner.data = seed_data.clone();
        let torn = TornStrictNor { inner, trigger_at: trigger, program_count: 0 };
        let storage = NorAlignedStorage::new(torn).unwrap();
        let mut buf_a = buf();
        let mut buf_b = buf();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: pre-scenario mount failed: {e:?}"));
        let mut a = buf();
        let mut b = buf();
        let mut scratch2 = vec![0u8; 2048];
        let _ = fs.append_to_path(p("/log"), &[0x55; 100], &mut scratch2, &mut a, &mut b);

        // Power off: take the raw device bytes WITHOUT flushing the
        // NOR adapter's cache. A power loss discards volatile cache
        // contents; flushing here would both model the wrong thing
        // and error through the still-torn wrapper, silently skipping
        // every interesting trigger (the overclaim pattern review H7
        // flags).
        let inner_data = fs.storage().inner().inner.data.clone();
        let mut recovered = StrictNorStorage::new();
        recovered.data = inner_data;
        let storage = NorAlignedStorage::new(recovered).unwrap();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: post-torn remount failed: {e:?}"));

        // The committed state is pre (300) or post (400).
        let mut out = vec![0u8; 1024];
        let n = fs.read_at_path(p("/log"), 0, &mut out, &mut a, &mut b).unwrap();
        assert!(n == 300 || n == 400, "trigger {trigger}: size {n} is neither pre nor post");
        assert!(out[..300].iter().all(|&x| x == 0xAA), "trigger {trigger}: prefix corrupted");
        if n == 400 {
            assert!(out[300..400].iter().all(|&x| x == 0x55));
        }

        // Append different bytes; they must read back exactly (the
        // pre-fix failure: ANDed with the torn fill's 0x55 residue,
        // or a strict-NOR panic on the 0 -> 1 flip).
        fs.append_to_path(p("/log"), &[0x33; 80], &mut scratch2, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: follow-up append failed: {e:?}"));
        let n2 = fs.read_at_path(p("/log"), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(n2, n + 80);
        assert!(
            out[n..n + 80].iter().all(|&x| x == 0x33),
            "trigger {trigger}: C8 — appended bytes corrupted by torn-fill residue: {:02x?}",
            &out[n..n + 8],
        );
    }
}
