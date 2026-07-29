//! Crash-safety and bound guards for failure-driven metadata relocation
//! (`lfs-23f`). The functional paths are pinned in
//! `tests/pending_badblock_reloc.rs` and `tests/badblock_split_reloc.rs`;
//! this file pins the properties that, if broken, corrupt data silently:
//!
//!   - the retry loop is bounded (a wholly-worn device fails, never hangs);
//!   - the `RelocateState` aggregate balances to zero after a clean run
//!     (no half-done cycle, no double-orphan from the wear+failure overlap);
//!   - wear levelling and failure relocation together stay consistent;
//!   - a relocation is atomic across a power loss at every program boundary
//!     (the fresh-only model mounts as the pre- or post-state, never corrupt).

use littlefs2_pure::block::BlockPair;
use littlefs2_pure::meta::MetadataPair;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::{BlockAddress, Error, Fs, Path};

const BS: usize = 256;
const BC: u32 = 64;

fn buf() -> [u8; BS] {
    [0u8; BS]
}

/// Multi-bad device (default `BLOCK_CYCLES`): `program` fails on any block in
/// `bad`; reads and erases always work.
struct MultiBad {
    data: Vec<u8>,
    bad: Vec<u32>,
}
impl MultiBad {
    fn new(bad: Vec<u32>) -> Self {
        Self { data: vec![0xFFu8; BS * BC as usize], bad }
    }
}
impl Storage for MultiBad {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BS;
    const BLOCK_COUNT: u32 = BC;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    fn read(&mut self, block: u32, off: u32, b: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(b.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        b.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if self.bad.contains(&block) {
            return Err(());
        }
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= BC {
            return Err(());
        }
        let s = (block as usize) * BS;
        self.data[s..s + BS].fill(0xFF);
        Ok(())
    }
}

/// Heavy wear-levelling device (`BLOCK_CYCLES = 1`) with one worn block, to
/// exercise the overlap of scheduled relocations with a failure relocation.
struct WearBad {
    data: Vec<u8>,
    bad: u32,
}
impl WearBad {
    fn new(bad: u32) -> Self {
        Self { data: vec![0xFFu8; BS * BC as usize], bad }
    }
}
impl Storage for WearBad {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BS;
    const BLOCK_COUNT: u32 = BC;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = 1;
    fn read(&mut self, block: u32, off: u32, b: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(b.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        b.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if block == self.bad {
            return Err(());
        }
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= BC {
            return Err(());
        }
        let s = (block as usize) * BS;
        self.data[s..s + BS].fill(0xFF);
        Ok(())
    }
}

/// Worn block plus an Nth-program power-loss counter: the `bad` block always
/// refuses `program` (the hardware fault that forces relocation), and once
/// `program_count` passes `trigger_at` every `program`/`erase` fails (the
/// power loss). Combining the two sweeps a tear across every program boundary
/// of a forced relocation.
struct TornBadBlock {
    data: Vec<u8>,
    bad: u32,
    trigger_at: usize,
    program_count: usize,
}
impl TornBadBlock {
    fn new(bad: u32, trigger_at: usize) -> Self {
        Self { data: vec![0xFFu8; BS * BC as usize], bad, trigger_at, program_count: 0 }
    }
    fn powered(&self) -> bool {
        self.program_count <= self.trigger_at
    }
}
impl Storage for TornBadBlock {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BS;
    const BLOCK_COUNT: u32 = BC;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    fn read(&mut self, block: u32, off: u32, b: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(b.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        b.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        self.program_count += 1;
        if !self.powered() {
            return Err(()); // power lost
        }
        if block == self.bad {
            return Err(()); // worn block
        }
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if !self.powered() {
            return Err(()); // power lost
        }
        if block >= BC {
            return Err(());
        }
        let s = (block as usize) * BS;
        self.data[s..s + BS].fill(0xFF);
        Ok(())
    }
}

/// XOR every committed `RelocateState` body reachable from the root, taking
/// the *latest* `DirStruct` per directory id (latest-wins, matching the
/// kernel's splice-correct walk) so a superseded child pointer from a
/// completed relocation is not followed. A clean filesystem must aggregate to
/// zero.
fn relocate_state_xor<S: Storage>(
    fs: &mut Fs<S>,
    root: BlockPair,
) -> [u8; littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE] {
    use littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE;
    use littlefs2_pure::TagType;

    let mut acc = [0u8; RELOCATE_STATE_BODY_SIZE];
    let mut queue = vec![root];
    let mut visited: Vec<BlockPair> = Vec::new();
    while let Some(pair_addr) = queue.pop() {
        if visited.contains(&pair_addr) {
            continue;
        }
        visited.push(pair_addr);

        let mut a = vec![0u8; S::BLOCK_SIZE];
        let mut b = vec![0u8; S::BLOCK_SIZE];
        fs.storage_mut().read(pair_addr.a.as_u32(), 0, &mut a).unwrap();
        fs.storage_mut().read(pair_addr.b.as_u32(), 0, &mut b).unwrap();
        let pair = MetadataPair::parse(pair_addr.a, &a, pair_addr.b, &b).expect("pair parses");

        for entry in pair.reader.iter_tags() {
            if entry.tag.tag_type() == TagType::RelocateState
                && entry.body.len() == RELOCATE_STATE_BODY_SIZE
            {
                for (acc_b, e) in acc.iter_mut().zip(entry.body.iter()) {
                    *acc_b ^= *e;
                }
            }
        }
        for de in littlefs2_pure::entries(&pair) {
            if de.kind == littlefs2_pure::EntryKind::Directory {
                // Latest DirStruct body wins (a relocation appends a fresh one).
                let mut child = None;
                for t in pair.reader.iter_tags() {
                    if t.tag.tag_type() == TagType::DirStruct
                        && t.tag.id() == de.id
                        && t.body.len() == 8
                    {
                        let ca = u32::from_le_bytes([t.body[0], t.body[1], t.body[2], t.body[3]]);
                        let cb = u32::from_le_bytes([t.body[4], t.body[5], t.body[6], t.body[7]]);
                        child = Some(BlockPair::new(BlockAddress::new(ca), BlockAddress::new(cb)));
                    }
                }
                if let Some(c) = child {
                    queue.push(c);
                }
            }
        }
        if let Some(tail) = pair.reader.tail() {
            queue.push(tail);
        }
    }
    acc
}

fn count_dir(fs: &mut Fs<impl Storage>, a: &mut [u8], b: &mut [u8]) -> usize {
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
    seen
}

/// The retry loop is bounded: when the alternate and every fresh candidate a
/// relocation could reach are worn, a metadata commit fails with `Io`/`OutOfRange`
/// rather than looping forever.
#[test]
fn bounded_retries_metadata_commit_is_io() {
    // Block 3 is /d's alternate; blocks 4..=14 are the fresh candidates the
    // relocation would try. All worn, so the bounded retry exhausts.
    let bad: Vec<u32> = (3..=14).collect();
    let mut storage = MultiBad::new(bad);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let mut err = None;
    for i in 0..12 {
        let name = format!("/d/f{i:02}");
        if let Err(e) = fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b) {
            err = Some(e);
            break;
        }
    }
    assert!(
        matches!(err, Some(Error::Io | Error::OutOfRange)),
        "a wholly-worn relocation target must fail bounded, got {err:?}"
    );
}

/// After the clean reproducer (one plain-compaction relocation past the worn
/// alternate, then a split on good blocks), every reachable `RelocateState`
/// body XORs to zero — no half-done cycle and no double-orphan.
#[test]
fn reproducer_relocate_state_balances() {
    let mut storage = MultiBad::new(vec![3]);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    for i in 0..24usize {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b).unwrap();
    }
    let root = fs.root();
    let acc = relocate_state_xor(&mut fs, root);
    assert_eq!(
        acc,
        [0u8; littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE],
        "RelocateState must balance to zero after a clean failure-driven relocation"
    );
}

/// Heavy wear levelling (`BLOCK_CYCLES = 1`) over a worn block: scheduled
/// relocations and a failure relocation interleave through the same churn.
/// All data survives a remount and the `RelocateState` aggregate stays zero
/// (a double-orphan from the overlap would imbalance it).
#[test]
fn wear_and_failure_stay_consistent() {
    // Block 5 is worn: not /d's initial alternate, so it is hit only once the
    // wear churn cycles it into a pair, overlapping wear with a failure.
    let mut storage = WearBad::new(5);
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let n = 30usize;
    for i in 0..n {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"z", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} should survive wear+failure: {e:?}"));
    }
    assert_eq!(count_dir(&mut fs, &mut a, &mut b), n);

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    assert_eq!(count_dir(&mut fs, &mut a, &mut b), n, "all survive a remount");
    let root = fs.root();
    assert_eq!(
        relocate_state_xor(&mut fs, root),
        [0u8; littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE],
        "wear + failure relocations must still balance"
    );
}

// A ~120-byte inline value forces the first overflow to split directly onto
// the worn alternate (block 3), so the power-loss sweep covers the
// split-relocation crash window (continuation write, sync, lower half to a
// fresh block, parent repoint).
const BIG: [u8; 120] = [0x9D; 120];

fn split_scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = buf();
    let mut b = buf();
    if fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).is_err() {
        return;
    }
    for i in 0..5usize {
        let name = format!("/d/f{i:02}");
        if fs.write_to_path(Path::new(&name).unwrap(), &BIG, &mut a, &mut b).is_err() {
            return;
        }
    }
}

/// A failure-driven split relocation is atomic across a power loss at every
/// program boundary: after a tear at any point, the filesystem remounts and
/// `/d` enumerates a consistent, duplicate-free set whose entries each read
/// back their full payload, and a follow-up write still succeeds. The
/// fresh-only model means each entry is present in full or not at all — never
/// a torn or corrupt entry.
#[test]
fn split_relocation_atomic_across_every_power_loss() {
    // Total program calls with no tear (block 3 worn forces the relocation).
    let total = {
        let mut s = TornBadBlock::new(3, usize::MAX);
        let mut sb = buf();
        Fs::format(&mut s, &mut sb).unwrap();
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(s, &mut ba, &mut bb).unwrap();
        let before = fs.storage().program_count;
        split_scenario(&mut fs);
        fs.storage().program_count - before
    };
    assert!(total > 0);

    for trigger in 1..=total {
        // The format runs untorn; the tear is injected during the scenario.
        let mut s = TornBadBlock::new(3, usize::MAX);
        let mut sb = buf();
        Fs::format(&mut s, &mut sb).unwrap();
        let base = {
            let mut ba = buf();
            let mut bb = buf();
            let mut fs = Fs::mount(s, &mut ba, &mut bb).unwrap();
            let base = fs.storage().program_count;
            // Arm the tear relative to the post-format baseline.
            fs.storage_mut().trigger_at = base + trigger;
            split_scenario(&mut fs);
            fs.into_storage()
        };

        // Re-power and remount twice; recovery must be idempotent.
        let mut s = base;
        s.trigger_at = usize::MAX;
        s.program_count = 0;
        let mut a = buf();
        let mut b = buf();
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(s, &mut ba, &mut bb)
            .unwrap_or_else(|e| panic!("remount after tear@{trigger} failed: {e:?}"));
        assert_dir_consistent(&mut fs, &mut a, &mut b, &format!("tear@{trigger}"));
        // A follow-up write must still land (the FS is not wedged).
        fs.write_to_path(Path::new("/d/zz").unwrap(), b"k", &mut a, &mut b)
            .or_else(|_| {
                // /d may not exist yet if the tear hit before mkdir; create it.
                fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).ok();
                fs.write_to_path(Path::new("/d/zz").unwrap(), b"k", &mut a, &mut b)
            })
            .unwrap_or_else(|e| panic!("post-tear@{trigger} write wedged: {e:?}"));

        let s = fs.into_storage();
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(s, &mut ba, &mut bb)
            .unwrap_or_else(|e| panic!("second remount after tear@{trigger} failed: {e:?}"));
        assert_dir_consistent(&mut fs, &mut a, &mut b, &format!("tear@{trigger}"));
    }
}

/// `/d` (when present) enumerates a duplicate free set, each entry reading
/// back its full payload — the post-tear consistency invariant.
fn assert_dir_consistent<S: Storage>(fs: &mut Fs<S>, a: &mut [u8], b: &mut [u8], ctx: &str) {
    if !fs.exists(Path::new("/d").unwrap(), a, b).unwrap_or(false) {
        return; // tear landed before /d became durable — a valid pre-state
    }
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), a, b)
        .unwrap_or_else(|e| panic!("list_dir after {ctx} failed: {e:?}"));
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "no duplicate entries after {ctx}");
    for name in &names {
        // Every listed entry must read back its full payload (or be the
        // follow-up "zz" sentinel from a prior remount pass).
        let path = format!("/d/{}", core::str::from_utf8(name).unwrap());
        let mut out = [0u8; BIG.len()];
        let got = fs
            .read_at_path(Path::new(&path).unwrap(), 0, &mut out, a, b)
            .unwrap_or_else(|e| panic!("read {path} after {ctx} failed: {e:?}"));
        if name.starts_with(b"zz") {
            continue;
        }
        assert_eq!(got, BIG.len(), "entry {path} length after {ctx}");
        assert_eq!(out, BIG, "entry {path} payload after {ctx}");
    }
}

// ---------------------------------------------------------------------
// Device level composition (review coverage item V4, bead `lfs-hki`)
// ---------------------------------------------------------------------

mod common;

use common::{PartialLandingWitness, PartialProgram, TornPartialStorage, NOR_PARTIAL_LANDINGS};
use littlefs2_pure::NorAlignedStorage;

/// The block whose programs always fail, forcing the relocation.
const WORN: u32 = 3;

/// This file's 64-block geometry as a strict NOR device: programs must
/// be `PROG_SIZE` aligned and `PROG_SIZE` sized, may only clear bits,
/// and always fail on the worn block. The permissive [`TornBadBlock`]
/// above overwrites bytes on a reprogram, which hides the corruption a
/// real chip would produce; this one AND-merges instead, so a kernel
/// that programs a page twice reads back garbage rather than the second
/// write.
struct WornStrictNor {
    data: Vec<u8>,
    bad: u32,
}

impl WornStrictNor {
    fn new(bad: u32) -> Self {
        Self { data: vec![0xFFu8; BS * BC as usize], bad }
    }
}

impl Storage for WornStrictNor {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BS;
    const BLOCK_COUNT: u32 = BC;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, b: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(b.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        b.copy_from_slice(&self.data[s..e]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        assert_eq!(off as usize % 16, 0, "NOR program must be PROG_SIZE-aligned, got off={off}");
        assert_eq!(
            data.len() % 16,
            0,
            "NOR program must be a PROG_SIZE multiple, got len={}",
            data.len()
        );
        if block == self.bad {
            return Err(()); // worn block
        }
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        for (existing, &new) in self.data[s..e].iter().zip(data) {
            assert_eq!(
                *existing & new,
                new,
                "NOR program flipped a 0 bit to 1 (existing={existing:#x}, new={new:#x})"
            );
        }
        for (existing, &new) in self.data[s..e].iter_mut().zip(data) {
            *existing &= new;
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= BC {
            return Err(());
        }
        let s = (block as usize) * BS;
        self.data[s..s + BS].fill(0xFF);
        Ok(())
    }
}

impl PartialProgram for WornStrictNor {
    fn program_partial(&mut self, block: u32, off: u32, data: &[u8]) {
        // A worn block refuses the whole program, so an interrupted
        // program to it lands nothing either.
        if block == self.bad || block >= BC {
            return;
        }
        let s = (block as usize) * BS + off as usize;
        let e = s + data.len();
        assert!(e <= self.data.len(), "partial landing runs past the end of the device");
        for (existing, &new) in self.data[s..e].iter_mut().zip(data) {
            *existing &= new;
        }
    }
}

/// The relocation sweep's device stack: the tear injector inside the
/// alignment adapter, over the worn strict NOR device.
type WornTornStorage = NorAlignedStorage<TornPartialStorage<WornStrictNor>>;

fn worn_torn_storage(trigger: usize, partial: usize) -> WornTornStorage {
    NorAlignedStorage::new(TornPartialStorage::new(WornStrictNor::new(WORN), trigger, partial))
        .expect("the geometry satisfies the alignment adapter's invariants")
}

/// Mount a post tear image on a powered (but still worn) device. The H7
/// line: the image held a valid filesystem, so it must mount.
fn mount_worn_image(image: Vec<u8>, ctx: &str) -> Fs<NorAlignedStorage<WornStrictNor>> {
    let mut device = WornStrictNor::new(WORN);
    assert_eq!(image.len(), device.data.len(), "{ctx}: image is not one whole device image");
    device.data = image;
    let storage = NorAlignedStorage::new(device)
        .expect("the geometry satisfies the alignment adapter's invariants");
    let mut ba = buf();
    let mut bb = buf();
    Fs::mount(storage, &mut ba, &mut bb)
        .unwrap_or_else(|e| panic!("{ctx}: torn write left an unmountable image: {e:?}"))
}

/// The failure driven split relocation sweep at DEVICE program
/// granularity, with partial window landings (review coverage item V4,
/// bead `lfs-hki`).
///
/// `split_relocation_atomic_across_every_power_loss` above tears at the
/// kernel's program calls over a permissive device. This one tears
/// inside the real page programs the alignment adapter issues, over a
/// device that AND-merges like a NOR chip, and can leave the page that
/// carries a relocated pair half programmed. The invariants are the
/// same: the image must remount, `/d` must enumerate a duplicate free
/// set whose entries each read back their full payload, a follow up
/// write must still land, and a second consecutive mount must agree.
///
/// Landing lengths come from `common::NOR_PARTIAL_LANDINGS` (the same 16
/// byte program window as the other sweeps); that constant documents the
/// sampling bound.
#[test]
fn split_relocation_atomic_across_every_nor_program_landing() {
    let (fmt_calls, scenario_calls) = {
        let mut storage = worn_torn_storage(usize::MAX, 0);
        let mut sb = buf();
        Fs::format(&mut storage, &mut sb).expect("untorn format must succeed");
        let fmt = storage.inner().program_count;
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).expect("untorn mount must succeed");
        let pre = fs.storage().inner().program_count;
        split_scenario(&mut fs);
        (fmt, fs.storage().inner().program_count - pre)
    };
    assert!(scenario_calls > 0);

    let mut witness = PartialLandingWitness::new();
    for partial in NOR_PARTIAL_LANDINGS {
        for trigger in 1..=fmt_calls + scenario_calls + 2 {
            let ctx = format!("nor relocation sweep tear@{trigger}, partial landing {partial}");
            let mut storage = worn_torn_storage(trigger, partial);
            let mut sb = buf();
            if Fs::format(&mut storage, &mut sb).is_err() {
                assert!(
                    trigger <= fmt_calls,
                    "{ctx}: format reported torn past its own {fmt_calls} device programs"
                );
                continue;
            }
            let mut ba = buf();
            let mut bb = buf();
            let mut fs = Fs::mount(storage, &mut ba, &mut bb)
                .expect("mount immediately after a completed format must succeed");
            split_scenario(&mut fs);

            // Power off: raw device bytes, adapter cache deliberately lost.
            let image = fs.storage().inner().inner.data.clone();
            witness.observe(partial, trigger, &image);

            let mut a = buf();
            let mut b = buf();
            let mut fs = mount_worn_image(image, &ctx);
            assert_dir_consistent(&mut fs, &mut a, &mut b, &ctx);

            // The filesystem is not wedged: a follow up write lands.
            fs.write_to_path(Path::new("/d/zz").unwrap(), b"k", &mut a, &mut b)
                .or_else(|_| {
                    // /d may not exist if the tear preceded the mkdir.
                    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).ok();
                    fs.write_to_path(Path::new("/d/zz").unwrap(), b"k", &mut a, &mut b)
                })
                .unwrap_or_else(|e| panic!("{ctx}: post tear write wedged: {e:?}"));

            let image = fs.into_storage().into_inner().expect("flush on a powered device").data;
            let ctx2 = format!("{ctx}, second remount");
            let mut fs = mount_worn_image(image, &ctx2);
            assert_dir_consistent(&mut fs, &mut a, &mut b, &ctx2);
        }
    }
    witness.assert_partials_landed("nor relocation sweep");
}
