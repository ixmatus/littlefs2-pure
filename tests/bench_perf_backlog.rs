//! Storage-operation-count harness for the 2026-05-29 performance
//! backlog (`lfs-opt`, `lfs-o72`). For an embedded filesystem the
//! load-bearing cost is flash I/O, so this counts `Storage::read`,
//! `program`, and `erase` calls (deterministic, no wall-clock noise)
//! rather than timing. The numbers decide which patches earn their
//! complexity, per the project's bench-first perf discipline.
//!
//! `#[ignore]` so neither `cargo test` nor CI runs it. Invoke:
//!
//! ```text
//! cargo test --test bench_perf_backlog -- --ignored --nocapture
//! ```

use core::fmt::Write as _;
use littlefs2_pure::{Fs, OpenOptions, Path, Storage};
use std::cell::Cell;

thread_local! {
    static READS: Cell<u64> = const { Cell::new(0) };
    static PROGS: Cell<u64> = const { Cell::new(0) };
    static ERASES: Cell<u64> = const { Cell::new(0) };
}

fn reset_counts() {
    READS.with(|c| c.set(0));
    PROGS.with(|c| c.set(0));
    ERASES.with(|c| c.set(0));
}

fn counts() -> (u64, u64, u64) {
    (READS.with(Cell::get), PROGS.with(Cell::get), ERASES.with(Cell::get))
}

/// RAM device that tallies every storage operation into thread-local
/// counters. 512 blocks of 256 bytes (128 KiB), bounds-checked.
struct CountingStorage {
    data: Vec<u8>,
}

impl CountingStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 512;

    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}

impl Storage for CountingStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        READS.with(|c| c.set(c.get() + 1));
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start + buf.len();
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        PROGS.with(|c| c.set(c.get() + 1));
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start + data.len();
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        ERASES.with(|c| c.set(c.get() + 1));
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let start = (block as usize) * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

fn fresh() -> CountingStorage {
    let mut storage = CountingStorage::new();
    let mut sb = [0u8; CountingStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut sb).expect("format");
    storage
}

/// Bench A (`lfs-opt`): reads spent by the allocator's forest scan on a
/// single block-allocating operation, as the number of reachable
/// directory pairs grows. Reachable pairs grow by *nesting* directories
/// (one entry per parent block, so no single directory block overflows),
/// then a small CTZ file is written to root. The write resolves O(1) but
/// the allocator scan walks the whole nested forest, so the read count
/// isolates the forest-scan cost. Depth is capped under
/// `MAX_QUEUED_PAIRS = 32`.
#[test]
#[ignore = "op-count harness; run with --ignored --nocapture"]
fn bench_a_alloc_scan_vs_population() {
    eprintln!("--- Bench A: allocator scan reads for one shallow CTZ write vs reachable pairs ---");
    for &depth in &[0usize, 4, 8, 16, 28] {
        let storage = fresh();
        let mut mount_a = [0u8; CountingStorage::BLOCK_SIZE];
        let mut mount_b = [0u8; CountingStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut mount_a, &mut mount_b).expect("mount");
        let mut work_a = [0u8; CountingStorage::BLOCK_SIZE];
        let mut work_b = [0u8; CountingStorage::BLOCK_SIZE];

        // Build a nested chain /n0/n1/.../n{depth-1}.
        let mut path = String::new();
        for i in 0..depth {
            write!(path, "/n{i}").unwrap();
            fs.mkdir(Path::new(&path).unwrap(), &mut work_a, &mut work_b).expect("mkdir nest");
        }

        // Probe: write a 2-block CTZ file to root (shallow resolve, one
        // block allocation -> one full forest scan).
        reset_counts();
        let opts = OpenOptions::new().write(true).create(true);
        {
            let mut file =
                fs.open(Path::new("/f").unwrap(), opts, &mut work_a, &mut work_b).expect("open");
            file.write(&[0x33u8; 300], &mut work_a, &mut work_b).expect("write");
            file.close(&mut work_a, &mut work_b).expect("close");
        }
        let (reads, progs, erases) = counts();
        eprintln!(
            "nested_pairs={:>3}  reads={reads:>5}  programs={progs:>4}  erases={erases:>4}",
            depth + 1
        );
    }
}

/// Bench B (`lfs-o72`): reads spent per single-block append as the CTZ
/// chain grows, through one stateful `File` handle. `stream_ctz_extend`
/// re-collects the whole chain backward on every call, so the per-append
/// read count should rise with the current chain length. A log-time head
/// seek (`lfs_ctz_find`) would flatten it.
#[test]
#[ignore = "op-count harness; run with --ignored --nocapture"]
fn bench_b_append_reads_vs_chain_length() {
    eprintln!("--- Bench B: reads per single-block append vs chain length ---");
    let storage = fresh();
    let mut mount_a = [0u8; CountingStorage::BLOCK_SIZE];
    let mut mount_b = [0u8; CountingStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut mount_a, &mut mount_b).expect("mount");
    let mut work_a = [0u8; CountingStorage::BLOCK_SIZE];
    let mut work_b = [0u8; CountingStorage::BLOCK_SIZE];

    let chunk = [0x5Au8; 200]; // ~one content block per append
    let opts = OpenOptions::new().write(true).append(true).create(true);
    let mut file = fs.open(Path::new("/f").unwrap(), opts, &mut work_a, &mut work_b).expect("open");
    for i in 0..200usize {
        reset_counts();
        file.write(&chunk, &mut work_a, &mut work_b).expect("append");
        let (reads, _progs, _erases) = counts();
        if i < 5 || i % 25 == 24 {
            eprintln!("append#={:>3} (chain~{:>3})  reads={reads:>4}", i + 1, i + 1);
        }
    }
}

/// Bench C (`lfs-o72`): total storage ops for a single large `set_len`
/// zero-extend. `set_len` loops `stream_ctz_extend` in fixed-size chunks,
/// so a large extend pays many chain re-walks; widening the chunk cuts
/// the op count proportionally.
#[test]
#[ignore = "op-count harness; run with --ignored --nocapture"]
fn bench_c_set_len_zero_extend() {
    eprintln!("--- Bench C: ops for one set_len zero-extend ---");
    for &target in &[1024u32, 4096, 16384] {
        let storage = fresh();
        let mut mount_a = [0u8; CountingStorage::BLOCK_SIZE];
        let mut mount_b = [0u8; CountingStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut mount_a, &mut mount_b).expect("mount");
        let mut work_a = [0u8; CountingStorage::BLOCK_SIZE];
        let mut work_b = [0u8; CountingStorage::BLOCK_SIZE];

        let opts = OpenOptions::new().write(true).create(true);
        let mut file =
            fs.open(Path::new("/z").unwrap(), opts, &mut work_a, &mut work_b).expect("open");
        // Seed one byte so the file is CTZ-extendable from a known head.
        file.write(&[0u8; 1], &mut work_a, &mut work_b).ok();
        reset_counts();
        file.set_len(target, &mut work_a, &mut work_b).expect("set_len");
        let (reads, progs, erases) = counts();
        eprintln!("target={target:>6}  reads={reads:>6}  programs={progs:>6}  erases={erases:>6}");
    }
}
