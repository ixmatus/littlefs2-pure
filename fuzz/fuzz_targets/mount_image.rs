#![no_main]
//! Fuzz the whole mount + traversal path on an adversarial disk image.
//!
//! The other targets fuzz a single codec in isolation (a tag, a
//! superblock body, a metadata block). This one feeds arbitrary bytes
//! as a complete device image and drives `Fs::mount` followed by a
//! bounded root listing, so the metadata-pair parser, the revision
//! comparison, the tail-chain walk, and the directory iterator all run
//! against bytes no writer produced.
//!
//! Property: mounting and listing an arbitrary image terminates with
//! either a mounted filesystem or a typed `Error` and never panics,
//! never indexes the backing store out of bounds, and never loops
//! unboundedly. The storage adapter below range-checks every access,
//! so a malformed pair pointer surfaces as `Err`, exactly as the
//! `Storage` trait contract requires of a real adapter.
//!
//! Companions the Kani proof
//! `verify::commit_proofs::metadata_reader_does_not_panic_on_arbitrary_input`
//! (single block, symbolic) by exercising the multi-block orchestration
//! Kani's loop budget cannot reach.

use libfuzzer_sys::fuzz_target;
use littlefs2_pure::{Fs, Storage};

/// Geometry mirrors `tests/common::MemStorage` and the conformance
/// vectors: 8 blocks of 256 bytes, 16-byte read/prog units.
const BLOCK_SIZE: usize = 256;
const BLOCK_COUNT: u32 = 8;
const IMAGE_BYTES: usize = BLOCK_SIZE * BLOCK_COUNT as usize;

/// A fixed-size RAM image with a bounds-checked `Storage` impl. Out-of-
/// range accesses return `Err(())` rather than panicking, honoring the
/// trait's "never read out-of-bounds, never panic" contract so the
/// fuzzer measures the kernel, not an unchecked adapter.
struct ImageStorage {
    data: [u8; IMAGE_BYTES],
}

impl Storage for ImageStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BLOCK_SIZE;
    const BLOCK_COUNT: u32 = BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize)
            .checked_mul(BLOCK_SIZE)
            .and_then(|b| b.checked_add(off as usize))
            .ok_or(())?;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= BLOCK_COUNT || end > IMAGE_BYTES {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize)
            .checked_mul(BLOCK_SIZE)
            .and_then(|b| b.checked_add(off as usize))
            .ok_or(())?;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= BLOCK_COUNT || end > IMAGE_BYTES {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= BLOCK_COUNT {
            return Err(());
        }
        let start = (block as usize) * BLOCK_SIZE;
        self.data[start..start + BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    // Erased flash is all 0xFF; overlay the fuzz bytes over an erased
    // image (truncating long inputs, leaving the tail erased for short
    // ones) so a short input still describes a plausible device.
    let mut image = [0xFFu8; IMAGE_BYTES];
    let n = data.len().min(IMAGE_BYTES);
    image[..n].copy_from_slice(&data[..n]);

    let storage = ImageStorage { data: image };
    let mut buf_a = [0u8; BLOCK_SIZE];
    let mut buf_b = [0u8; BLOCK_SIZE];

    if let Ok(mut fs) = Fs::mount(storage, &mut buf_a, &mut buf_b) {
        // A successful mount means the root pair parsed; now drive the
        // directory iterator and tail-chain walk. Bound the callback so
        // a crafted cyclic-looking listing cannot wedge the fuzzer
        // (the kernel is expected to bound this itself; the cap turns a
        // hang into a fast, visible failure if it ever does not).
        let mut count = 0usize;
        let mut a = [0u8; BLOCK_SIZE];
        let mut b = [0u8; BLOCK_SIZE];
        let _ = fs.list_root(
            |_entry| {
                count += 1;
                assert!(count <= 4096, "list_root yielded an implausible entry count");
            },
            &mut a,
            &mut b,
        );
    }
});
