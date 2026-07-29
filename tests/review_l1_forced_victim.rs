//! Review L1, second half: does the forced-victim relocation path need
//! the split fallback the normal compaction path has?
//!
//! When an in-place append fails on a worn active block, the kernel
//! eagerly relocates the pair onto a fresh block
//! (`compact_and_program`'s `forced_victim` branch), rebuilding the live
//! set there. That branch runs *before* the split-point computation, so
//! it never splits. The finding reads that as a spurious `OutOfRange`
//! waiting to happen: an overfull pair that hits relocation would error
//! where the C reference splits.
//!
//! It cannot happen, and the guard is arithmetic rather than a check.
//! The branch is reachable only when `can_append` held, which requires
//! `committed_end + dsize <= BLOCK_SIZE`. The active block's log already
//! contains every tag the compaction re-emits (the crate writes its own
//! compacted blocks, so each live entry's Create, NAME, STRUCT, and
//! attribute tags are physically present), plus the revision header, the
//! tail tag the compaction re-emits, and at least one CCRC and FCRC. So
//! `committed_end >= 4 + live + tail + 16`, which is the compacted size
//! of the same set. The op contributes `dsize` bytes to the log but
//! `dsize - 8` to a compaction (a compaction shares the one trailing
//! CCRC the op's `dsize` budgets for). The compacted image is therefore
//! at most `BLOCK_SIZE - 8` bytes and always fits.
//!
//! This file is the empirical half of that argument: it sweeps a
//! single-entry pair (the shape that can carry the most live bytes,
//! because a one-entry range never splits) across every attribute size
//! the geometry admits, wears the active block, and issues the smallest
//! op that still takes the append path. Every case that actually
//! reaches the forced-victim branch must relocate and succeed.
//!
//! Verdict recorded here so a future reader does not re-litigate it:
//! the error half of the finding is refuted for this writer. The
//! behavioural half stands and is deliberate: the forced-victim path
//! also skips the *half-block* split the ordinary path would perform,
//! so a relocating pair can carry more than half a block until its next
//! ordinary compaction redistributes it.

use littlefs2_pure::{Fs, Path, Storage};

struct Dev {
    data: Vec<u8>,
    /// Blocks whose `program` fails, modelling worn cells.
    bad: Vec<u32>,
    /// Blocks a failed `program` was attempted on.
    prog_failures: Vec<u32>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    fn new() -> Self {
        Self {
            data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize],
            bad: Vec::new(),
            prog_failures: Vec::new(),
        }
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
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if self.bad.contains(&block) {
            self.prog_failures.push(block);
            return Err(());
        }
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let s = (block as usize) * Self::BLOCK_SIZE;
        self.data[s..s + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

fn buf() -> [u8; Dev::BLOCK_SIZE] {
    [0u8; Dev::BLOCK_SIZE]
}

/// Address of `/d`'s first metadata pair.
fn d_pair(fs: &mut Fs<Dev>) -> littlefs2_pure::BlockPair {
    let mut a = buf();
    let mut b = buf();
    let r = fs.resolve(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let body = r.struct_body;
    assert_eq!(body.len(), 8);
    littlefs2_pure::BlockPair::new(
        littlefs2_pure::BlockAddress::new(u32::from_le_bytes([body[0], body[1], body[2], body[3]])),
        littlefs2_pure::BlockAddress::new(u32::from_le_bytes([body[4], body[5], body[6], body[7]])),
    )
}

fn active_block(fs: &mut Fs<Dev>, pair: littlefs2_pure::BlockPair) -> u32 {
    let mut a = buf();
    let mut b = buf();
    fs.read_pair(pair, &mut a, &mut b).unwrap().active_block.as_u32()
}

#[test]
fn the_forced_victim_relocation_never_needs_a_split() {
    // Attribute sizes that keep a one-entry pair inside a block: the
    // entry is 13 wire bytes and the compaction adds 32 of header, tail,
    // CCRC, and FCRC, so 4 + (13 + 4 + v) + 32 must clear 256.
    let mut exercised = 0usize;
    let mut relocated = 0usize;
    for v in (0..=200).step_by(4) {
        for w in [0usize, 4, 8, 16, 32] {
            let mut storage = Dev::new();
            let mut sb = buf();
            Fs::format(&mut storage, &mut sb).unwrap();
            let mut ba = buf();
            let mut bb = buf();
            let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
            let mut a = buf();
            let mut b = buf();

            fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
            fs.write_to_path(Path::new("/d/0").unwrap(), b"", &mut a, &mut b).unwrap();
            if fs.set_attr(Path::new("/d/0").unwrap(), 1, &vec![0xA0; v], &mut a, &mut b).is_err() {
                // `v` alone does not fit a block; nothing to probe.
                continue;
            }

            let pair = d_pair(&mut fs);
            let active = active_block(&mut fs, pair);
            fs.storage_mut().bad.push(active);
            fs.storage_mut().prog_failures.clear();

            let res = fs.set_attr(Path::new("/d/0").unwrap(), 2, &vec![0xCC; w], &mut a, &mut b);
            let hit_worn = fs.storage().prog_failures.contains(&active);
            if !hit_worn {
                // The op did not take the append path, so the worn block
                // was never programmed and the forced-victim branch was
                // not the one under test.
                continue;
            }
            exercised += 1;
            assert!(
                res.is_ok(),
                "v={v} w={w}: the forced-victim relocation must not fail: {res:?}"
            );

            // The pair moved off the worn block, which is the branch's
            // whole job, and every byte still reads back.
            let after = d_pair(&mut fs);
            assert_ne!(after, pair, "v={v} w={w}: the worn pair must relocate");
            assert!(after.a.as_u32() != active && after.b.as_u32() != active);
            relocated += 1;

            let mut out = [0u8; Dev::BLOCK_SIZE];
            let n = fs.get_attr(Path::new("/d/0").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
            assert_eq!(n, v, "v={v} w={w}: the pre-existing attribute survived");
            assert!(out[..v].iter().all(|&x| x == 0xA0));
            let n = fs.get_attr(Path::new("/d/0").unwrap(), 2, &mut out, &mut a, &mut b).unwrap();
            assert_eq!(n, w, "v={v} w={w}: the new attribute landed whole");
            assert!(out[..w].iter().all(|&x| x == 0xCC));

            // The relocation is durable, not just in memory.
            let storage = fs.into_storage();
            let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
            let mut names: Vec<Vec<u8>> = Vec::new();
            fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b)
                .unwrap();
            assert_eq!(names, vec![b"0".to_vec()], "v={v} w={w}: the entry survived the remount");
        }
    }
    // A sweep that never reached the branch would prove nothing. The
    // grid above reaches it 183 times on this geometry; the floor here
    // is loose enough to survive incidental layout changes and tight
    // enough that a change which stops reaching the branch fails.
    assert!(exercised >= 100, "the sweep must exercise the forced-victim branch (got {exercised})");
    assert_eq!(exercised, relocated);
}
