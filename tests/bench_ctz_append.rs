//! Repeatable timing harness for the many-small-appends path.
//!
//! `File::write` in append mode calls `Fs::stream_ctz_extend` once per
//! write, and that function walks the existing CTZ chain
//! (`collect_chain_blocks`, ~n/2 small reads) every time. For N
//! sequential single-block appends the walk cost is the arithmetic
//! series, i.e. O(N^2). This harness measures the wall time of N
//! appends so the cost can be quantified before deciding whether a
//! per-`File` chain cache earns its complexity and its ~1 KiB struct
//! growth (see docs/decisions/0007-*; the decision is bench-gated).
//!
//! Not a microbenchmark framework: zero dependencies, `std::time`,
//! `#[ignore]` so neither `cargo test` nor CI runs it. Invoke
//! explicitly:
//!
//! ```text
//! cargo test --test bench_ctz_append -- --ignored --nocapture
//! ```

use littlefs2_pure::{Fs, OpenOptions, Path, Storage};
use std::time::Instant;

/// A generous RAM device: 512 blocks of 256 bytes (128 KiB). Large
/// enough to grow a CTZ file to the kernel's 256-block cap with room
/// for the superblock and metadata. Bounds-checked, like the real
/// adapters.
struct BigStorage {
    data: Vec<u8>,
}

impl BigStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 512;

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
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start + buf.len();
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start + data.len();
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let start = (block as usize) * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

/// Append `chunk` to `/f` exactly `appends` times through a single
/// stateful `File` handle (the path the chain re-walk hits), returning
/// the elapsed wall time.
fn time_appends(appends: usize, chunk: &[u8]) -> std::time::Duration {
    let mut storage = BigStorage::new();
    let mut sb = [0u8; BigStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut sb).expect("format");

    let mut a = [0u8; BigStorage::BLOCK_SIZE];
    let mut b = [0u8; BigStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).expect("mount");

    let opts = OpenOptions::new().write(true).append(true).create(true);
    let mut buf_a = [0u8; BigStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; BigStorage::BLOCK_SIZE];

    let start = Instant::now();
    {
        let mut f = fs.open(Path::new("/f").unwrap(), opts, &mut buf_a, &mut buf_b).expect("open");
        for _ in 0..appends {
            f.write(chunk, &mut buf_a, &mut buf_b).expect("append");
        }
    }
    start.elapsed()
}

#[test]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
fn bench_many_small_appends() {
    // 200 byte chunk: with the per-block skip-pointer header this is
    // about one content block per append, so N appends build an
    // N-block chain and the final appends pay the full walk.
    let chunk = [0x5Au8; 200];
    // 250 is close to the kernel's 256-block CTZ cap: if the O(N^2)
    // walk were observable anywhere it would show by here as a rising
    // per-append figure.
    for &n in &[50usize, 100, 200, 250] {
        // Three trials per size; report each so noise is visible
        // rather than averaged away.
        let mut trials = [std::time::Duration::ZERO; 3];
        for t in &mut trials {
            *t = time_appends(n, &chunk);
        }
        let n_f = f64::from(u32::try_from(n).unwrap());
        let per = |d: std::time::Duration| d.as_secs_f64() * 1e6 / n_f;
        eprintln!(
            "appends={n:>4}  trials_us=[{:.0}, {:.0}, {:.0}]  per_append_us=[{:.1}, {:.1}, {:.1}]",
            trials[0].as_secs_f64() * 1e6,
            trials[1].as_secs_f64() * 1e6,
            trials[2].as_secs_f64() * 1e6,
            per(trials[0]),
            per(trials[1]),
            per(trials[2]),
        );
    }
}
