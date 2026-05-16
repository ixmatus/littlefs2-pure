//! Integration tests for the pair-relocation wear-levelling path.
//!
//! Wear levelling for the metadata-pair layer redirects compaction
//! from the in-pair alternate to a freshly allocated block every
//! `((BLOCK_CYCLES + 1) | 1)` compactions. The new pair address is
//! propagated to the parent's `DirStruct` reference as part of the
//! same operation.
//!
//! Tests here use a dedicated `Storage` impl with `BLOCK_CYCLES = 1`
//! (so wear levelling fires at `new_revision % 3 == 0`, i.e. roughly
//! every third compaction) and `BLOCK_COUNT = 32` so the device has
//! enough free space to host the freshly allocated blocks across
//! many cycles.

use littlefs2_pure::block::BlockPair;
use littlefs2_pure::meta::MetadataPair;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::{BlockAddress, Fs, Path, ROOT_BLOCK_PAIR};

extern crate alloc as core_alloc;
use core_alloc::vec;

/// 32-block test geometry with aggressive wear levelling
/// (`BLOCK_CYCLES = 1`). The disk is large enough that relocation
/// always finds a free block.
#[derive(Debug)]
struct WearStorage {
    data: core_alloc::vec::Vec<u8>,
}

impl WearStorage {
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 32;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = 1;

    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}

impl Storage for WearStorage {
    type Error = ();
    const READ_SIZE: usize = Self::READ_SIZE;
    const PROG_SIZE: usize = Self::PROG_SIZE;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = Self::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = Self::LOOKAHEAD_SIZE;
    const BLOCK_CYCLES: i32 = Self::BLOCK_CYCLES;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        if end > self.data.len() {
            return Err(());
        }
        for b in &mut self.data[start..end] {
            *b = 0xFF;
        }
        Ok(())
    }
}

/// Same geometry, but with wear levelling disabled.
#[derive(Debug)]
struct NoWearStorage {
    data: core_alloc::vec::Vec<u8>,
}

impl NoWearStorage {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 32;

    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}

impl Storage for NoWearStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = -1;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        if end > self.data.len() {
            return Err(());
        }
        for b in &mut self.data[start..end] {
            *b = 0xFF;
        }
        Ok(())
    }
}

/// Resolve the on-disk `DirStruct` pair address for a subdirectory of
/// the root by name. Returns the pair address the root's `DirStruct`
/// entry currently references.
fn read_subdir_pair_from_root<S: Storage>(fs: &mut Fs<S>, name: &[u8]) -> BlockPair {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    let r = fs
        .resolve(Path::new(core::str::from_utf8(name).unwrap()).unwrap(), &mut a, &mut b)
        .expect("subdir resolves");
    let body = r.struct_body;
    assert_eq!(body.len(), 8, "DirStruct body is 8 bytes");
    let pa = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let pb = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    BlockPair::new(BlockAddress::new(pa), BlockAddress::new(pb))
}

#[test]
fn root_never_relocates_under_heavy_compaction() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();
    assert_eq!(fs.root(), ROOT_BLOCK_PAIR);

    // Hammer the root pair with rewrites of the same key. Each write
    // is an append; eventually appends overflow the block and trigger
    // compactions, which bump the revision counter. With BLOCK_CYCLES
    // = 1 the predicate `(rev + 1) % 3 == 0` fires every ~3
    // compactions on a non-root pair, but the root pair must stay at
    // `(0, 1)` no matter what.
    for i in 0..200u32 {
        let val = vec![b'x'; 16 + (i % 16) as usize];
        fs.write_inline_to_root(b"hot", &val, &mut a, &mut b).unwrap();
    }
    assert_eq!(fs.root(), ROOT_BLOCK_PAIR, "root pair never relocates");

    // Read back the final value to confirm correctness through all
    // those compactions.
    let mut out = [0u8; 64];
    let n = fs.read_at_path(Path::new("/hot").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    let expected_len = 16 + (199u32 % 16) as usize;
    assert_eq!(n, expected_len);
    assert!(out[..n].iter().all(|&b| b == b'x'));
}

#[test]
fn subdir_pair_relocates_to_fresh_blocks() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.mkdir(Path::new("/sub").unwrap(), &mut a, &mut b).unwrap();
    let initial_pair = read_subdir_pair_from_root(&mut fs, b"/sub");

    // Force compactions in /sub by rewriting a single key with
    // growing contents until enough revisions accumulate to cross
    // BLOCK_CYCLES at least once.
    for i in 0..200u32 {
        let val = vec![b'y'; 16 + (i % 32) as usize];
        fs.write_inline_to_pair_for_test_or_path(b"/sub/k", &val, &mut a, &mut b).unwrap();
    }

    let final_pair = read_subdir_pair_from_root(&mut fs, b"/sub");
    assert_ne!(
        initial_pair, final_pair,
        "subdir pair should have relocated after many compactions"
    );

    // Each relocation cycle replaces one block of the pair with a
    // freshly allocated one; after enough cycles every block of the
    // original pair has been rotated out. The wear distribution
    // property is that the pair's address SET migrates over time,
    // which `assert_ne!` above already captures. Verify the new pair
    // addresses are distinct (a real pair, not a degenerate one).
    assert_ne!(final_pair.a, final_pair.b);

    // Read back the latest value through the parent's flipped
    // DirStruct reference; the entry must still be reachable.
    let mut out = vec![0u8; 64];
    let n = fs.read_at_path(Path::new("/sub/k").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    let expected_len = 16 + (199u32 % 32) as usize;
    assert_eq!(n, expected_len);
    assert!(out[..n].iter().all(|&v| v == b'y'));
}

#[test]
fn first_relocation_replaces_exactly_one_block() {
    // After the first relocation cycle, exactly one block of the
    // original pair survives (the one that was active when the
    // relocate fired). Both blocks rotate over subsequent cycles,
    // but the per-cycle invariant is "one block replaced at a time."
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.mkdir(Path::new("/sub").unwrap(), &mut a, &mut b).unwrap();
    let initial_pair = read_subdir_pair_from_root(&mut fs, b"/sub");

    // Drive compactions one at a time, checking after each write
    // whether the pair changed. Stop at the first relocation event.
    let mut prev_pair = initial_pair;
    let mut relocated_after: Option<u32> = None;
    for i in 0..200u32 {
        let val = vec![b'r'; 16 + (i % 16) as usize];
        fs.write_inline_to_pair_for_test_or_path(b"/sub/k", &val, &mut a, &mut b).unwrap();
        let now = read_subdir_pair_from_root(&mut fs, b"/sub");
        if now != prev_pair {
            relocated_after = Some(i);
            prev_pair = now;
            break;
        }
    }
    let i = relocated_after.expect("wear levelling fired within 200 writes");
    assert!(i > 0, "first relocation never on the very first write");
    // After the first relocation: one block of the original pair is
    // preserved, the other has been replaced with a freshly allocated
    // address.
    let original = [initial_pair.a, initial_pair.b];
    let now = [prev_pair.a, prev_pair.b];
    let overlap = original.iter().filter(|x| now.contains(x)).count();
    assert_eq!(
        overlap, 1,
        "first relocation replaces exactly one block (initial={initial_pair:?}, after={prev_pair:?})",
    );
}

#[test]
fn relocated_subdir_data_survives_remount() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.mkdir(Path::new("/sub").unwrap(), &mut a, &mut b).unwrap();
    let initial_pair = read_subdir_pair_from_root(&mut fs, b"/sub");

    // Drive enough compactions to trigger several relocations.
    for i in 0..200u32 {
        let val = vec![b'z'; 16 + (i % 24) as usize];
        fs.write_inline_to_pair_for_test_or_path(b"/sub/k", &val, &mut a, &mut b).unwrap();
    }
    let pair_before_remount = read_subdir_pair_from_root(&mut fs, b"/sub");
    assert_ne!(initial_pair, pair_before_remount);

    let storage = fs.into_storage();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();
    let pair_after_remount = read_subdir_pair_from_root(&mut fs, b"/sub");
    assert_eq!(
        pair_before_remount, pair_after_remount,
        "DirStruct rewrite is durable across remount"
    );

    let mut out = vec![0u8; 64];
    let n = fs.read_at_path(Path::new("/sub/k").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    let expected_len = 16 + (199u32 % 24) as usize;
    assert_eq!(n, expected_len);
    assert!(out[..n].iter().all(|&v| v == b'z'));
}

#[test]
fn negative_block_cycles_disables_wear_levelling() {
    let mut storage = NoWearStorage::new();
    let mut scratch = vec![0u8; NoWearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; NoWearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; NoWearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.mkdir(Path::new("/sub").unwrap(), &mut a, &mut b).unwrap();
    let initial_pair = read_subdir_pair_from_root(&mut fs, b"/sub");

    for i in 0..200u32 {
        let val = vec![b'w'; 16 + (i % 16) as usize];
        fs.write_inline_to_pair_for_test_or_path(b"/sub/k", &val, &mut a, &mut b).unwrap();
    }
    let final_pair = read_subdir_pair_from_root(&mut fs, b"/sub");
    assert_eq!(initial_pair, final_pair, "with BLOCK_CYCLES <= 0 the subdir pair never relocates");
}

#[test]
fn nested_subdir_relocations_propagate_through_grandparent() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.mkdir(Path::new("/g").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/g/p").unwrap(), &mut a, &mut b).unwrap();
    let initial_p = read_subdir_pair_from_root_path(&mut fs, "/g/p");

    for i in 0..200u32 {
        let val = vec![b'q'; 16 + (i % 24) as usize];
        fs.write_inline_to_pair_for_test_or_path(b"/g/p/k", &val, &mut a, &mut b).unwrap();
    }
    let final_p = read_subdir_pair_from_root_path(&mut fs, "/g/p");
    assert_ne!(initial_p, final_p, "grand-child pair should have relocated");

    // The grandparent's reference to /g must still resolve, and /g's
    // reference to /g/p must equal the new pair.
    let mut out = vec![0u8; 64];
    let n = fs.read_at_path(Path::new("/g/p/k").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    let expected_len = 16 + (199u32 % 24) as usize;
    assert_eq!(n, expected_len);
    assert!(out[..n].iter().all(|&v| v == b'q'));
}

/// Read the on-disk `DirStruct` pair for an arbitrary path.
fn read_subdir_pair_from_root_path<S: Storage>(fs: &mut Fs<S>, path: &str) -> BlockPair {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    let r = fs.resolve(Path::new(path).unwrap(), &mut a, &mut b).expect("path resolves");
    let body = r.struct_body;
    assert_eq!(body.len(), 8);
    let pa = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let pb = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    BlockPair::new(BlockAddress::new(pa), BlockAddress::new(pb))
}

/// Convenience: write to a path. Wraps `write_to_path` so the call
/// sites in this file read cleanly regardless of which inline /
/// CTZ path the kernel chooses. The body sizes stay below the
/// inline / CTZ threshold (`Fs::INLINE_MAX = 128`).
trait WritePathExt<S: Storage> {
    fn write_inline_to_pair_for_test_or_path(
        &mut self,
        path: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), littlefs2_pure::Error>;
}

impl<S: Storage> WritePathExt<S> for Fs<S> {
    fn write_inline_to_pair_for_test_or_path(
        &mut self,
        path: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), littlefs2_pure::Error> {
        let s = core::str::from_utf8(path).unwrap();
        self.write_to_path(Path::new(s).unwrap(), content, buf_a, buf_b)
    }
}

/// Read-side cross-check: the pair the root's `DirStruct` points at
/// must actually parse as a valid metadata pair. Useful in tests as
/// a "the address is real" assertion.
#[allow(dead_code)]
fn assert_pair_parses<S: Storage>(fs: &mut Fs<S>, pair: BlockPair) {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    fs.storage_mut().read(pair.a.as_u32(), 0, &mut a).expect("read a");
    fs.storage_mut().read(pair.b.as_u32(), 0, &mut b).expect("read b");
    MetadataPair::parse(pair.a, &a, pair.b, &b).expect("pair parses");
}

/// Torn-write wrapper over [`WearStorage`]. After `trigger_at` program
/// calls, `program` and `erase` return `Err(())` so the FS observes
/// the same I/O failure pattern a real power loss would produce.
struct TornWearStorage {
    inner: WearStorage,
    trigger_at: usize,
    program_count: usize,
}

impl TornWearStorage {
    fn new(inner: WearStorage, trigger_at: usize) -> Self {
        Self { inner, trigger_at, program_count: 0 }
    }

    fn into_inner(self) -> WearStorage {
        self.inner
    }
}

impl Storage for TornWearStorage {
    type Error = ();
    const READ_SIZE: usize = WearStorage::READ_SIZE;
    const PROG_SIZE: usize = WearStorage::PROG_SIZE;
    const BLOCK_SIZE: usize = WearStorage::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = WearStorage::BLOCK_COUNT;
    const CACHE_SIZE: usize = WearStorage::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = WearStorage::LOOKAHEAD_SIZE;
    const BLOCK_CYCLES: i32 = WearStorage::BLOCK_CYCLES;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        self.inner.read(block, off, buf)
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

/// Torn-write atomicity for wear-level pair relocation.
///
/// Seed an FS with `/sub/k = "PRE"`, then run a write that triggers
/// at least one relocation event. For each possible program-call
/// boundary in the post-seed operation, "power off" at that boundary,
/// remount the resulting image, and assert: either `/sub/k` reads
/// back as the pre-state ("PRE") or as the post-state ("POST"). Never
/// corrupt, never a phantom intermediate value.
///
/// The mount path's `recover_pending_relocation` is what cancels a
/// half-completed cycle so the FS observes the pre-state. The
/// alternate-then-fresh program order in `compact_and_program` is
/// what makes the post-state reachable via the parent's unchanged
/// reference once the alternate lands.
#[test]
fn relocation_atomic_across_every_power_loss() {
    // Seed: format + write /sub/k = "PRE" with no torn writes.
    let mut seed = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut seed, &mut scratch).unwrap();
    let seed_data = {
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(seed, &mut a, &mut b).unwrap();
        fs.mkdir(Path::new("/sub").unwrap(), &mut a, &mut b).unwrap();
        // Drive enough rewrites that the NEXT write will hit a
        // compaction (and, with BLOCK_CYCLES = 1, sometimes a
        // relocation). The exact threshold depends on inline tag
        // sizes, so we hammer until the pair is close to full.
        for i in 0..40u32 {
            let val = vec![b'p'; 16 + (i % 16) as usize];
            fs.write_to_path(Path::new("/sub/k").unwrap(), &val, &mut a, &mut b).unwrap();
        }
        // The known pre-state we'll match against on remount.
        fs.write_to_path(Path::new("/sub/k").unwrap(), b"PRE", &mut a, &mut b).unwrap();
        fs.into_storage().data
    };

    // Count program calls for the relocation-triggering scenario.
    let scenario = |fs: &mut Fs<TornWearStorage>| {
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        // A handful of writes; enough to span at least one relocation
        // cycle (BLOCK_CYCLES = 1 fires every ~3 compactions).
        for _ in 0..20 {
            let _ = fs.write_to_path(Path::new("/sub/k").unwrap(), b"POST", &mut a, &mut b);
        }
    };
    let total_calls = {
        let mut s = WearStorage::new();
        s.data = seed_data.clone();
        let torn = TornWearStorage::new(s, usize::MAX);
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(torn, &mut a, &mut b).unwrap();
        let before = fs.storage().program_count;
        scenario(&mut fs);
        let after = fs.storage().program_count;
        after - before
    };
    assert!(total_calls > 0, "scenario should issue program calls");

    for trigger in 1..=total_calls {
        let mut s = WearStorage::new();
        s.data = seed_data.clone();
        let torn = TornWearStorage::new(s, trigger);
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        match Fs::mount(torn, &mut a, &mut b) {
            Ok(mut fs) => {
                scenario(&mut fs);
                let inner = fs.into_storage().into_inner();
                // Remount with a fresh (unlimited) torn wrapper so
                // post-remount work — including any orphan recovery —
                // is allowed to run to completion.
                let recovered = TornWearStorage::new(inner, usize::MAX);
                let mut fs2 = match Fs::mount(recovered, &mut a, &mut b) {
                    Ok(fs) => fs,
                    Err(e) => panic!(
                        "trigger {trigger}: post-torn remount failed: {e:?} \
                         (mount must always recover from a torn relocation)"
                    ),
                };
                let mut out = vec![0u8; 16];
                let n = fs2
                    .read_at_path(Path::new("/sub/k").unwrap(), 0, &mut out, &mut a, &mut b)
                    .unwrap_or_else(|e| {
                        panic!("trigger {trigger}: /sub/k unreadable post-recovery: {e:?}")
                    });
                let content = &out[..n];
                assert!(
                    content == b"PRE" || content == b"POST",
                    "trigger {trigger}: /sub/k read back as {content:?}; \
                     must be pre-state b\"PRE\" or post-state b\"POST\""
                );
            }
            Err(e) => panic!("trigger {trigger}: pre-scenario mount failed: {e:?}"),
        }
    }
}

/// BFS the reachable metadata-pair forest from the root and
/// XOR-accumulate every committed `RelocateState` body, mirroring the
/// kernel's private `accumulate_gstate` walk using only the public API.
///
/// For each visited pair only the active block's tag stream contributes
/// (matching `MetadataPair::parse`, which exposes the higher-revision
/// block's reader). Children are followed via live `DirStruct` tags and
/// the pair's tail, exactly as `accumulate_gstate` does.
fn relocate_state_xor_from_root<S: Storage>(
    fs: &mut Fs<S>,
    root: BlockPair,
) -> [u8; littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE] {
    use littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE;
    use littlefs2_pure::TagType;

    let mut acc = [0u8; RELOCATE_STATE_BODY_SIZE];
    let mut queue: core_alloc::vec::Vec<BlockPair> = core_alloc::vec![root];
    let mut visited: core_alloc::vec::Vec<BlockPair> = core_alloc::vec::Vec::new();

    while let Some(pair_addr) = queue.pop() {
        if visited.contains(&pair_addr) {
            continue;
        }
        visited.push(pair_addr);

        let mut a = vec![0u8; S::BLOCK_SIZE];
        let mut b = vec![0u8; S::BLOCK_SIZE];
        fs.storage_mut().read(pair_addr.a.as_u32(), 0, &mut a).expect("read a");
        fs.storage_mut().read(pair_addr.b.as_u32(), 0, &mut b).expect("read b");
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

        // Enqueue children via live DirStruct entries plus the tail,
        // matching the kernel BFS. `entries` is the raw walker; in the
        // test scenarios below there are no deletions so raw == live.
        for de in littlefs2_pure::entries(&pair) {
            if de.kind == littlefs2_pure::EntryKind::Directory {
                // Find the DirStruct body at this id.
                for tag_entry in pair.reader.iter_tags() {
                    if tag_entry.tag.tag_type() == TagType::DirStruct
                        && tag_entry.tag.id() == de.id
                        && tag_entry.body.len() == 8
                    {
                        let body = tag_entry.body;
                        let ca = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                        let cb = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                        queue.push(BlockPair::new(
                            BlockAddress::new(ca),
                            BlockAddress::new(cb),
                        ));
                    }
                }
            }
        }
        if let Some(tail) = pair.reader.tail() {
            queue.push(tail);
        }
    }
    acc
}

/// Pins the relocation-recovery design contract: after a clean
/// (non-torn) sequence of wear-level relocations, every committed
/// `RelocateState` body reachable from the root must XOR to zero.
///
/// Each relocation writes one `RelocateState` body onto the relocated
/// pair's new active block and a balancing copy into the parent's
/// `propagate_relocation` commit; the two cancel. The post-relocation
/// pair address is `(old_active, fresh)` — the recycled alternate is
/// unreferenced from the root and is correctly NOT walked. If
/// `accumulate_gstate` ever regressed to also fold the orphaned
/// alternate, or dropped the parent's cancel body, this aggregate
/// would be non-zero and a spurious recovery would fire on every
/// mount.
#[test]
fn relocation_xor_aggregate_zeros_on_success() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.mkdir(Path::new("/sub").unwrap(), &mut a, &mut b).unwrap();
    let initial = read_subdir_pair_from_root_path(&mut fs, "/sub");

    // Force /sub to relocate several times (same pattern as the proven
    // subdir_pair_relocates_to_fresh_blocks test).
    for i in 0..200u32 {
        let val = vec![b'x'; 16 + (i % 32) as usize];
        fs.write_to_path(Path::new("/sub/hot").unwrap(), &val, &mut a, &mut b).unwrap();
    }
    let post = read_subdir_pair_from_root_path(&mut fs, "/sub");
    assert_ne!(initial, post, "/sub must have relocated for this test to be meaningful");

    let root = fs.root();
    let acc = relocate_state_xor_from_root(&mut fs, root);
    assert_eq!(
        acc,
        [0u8; littlefs2_pure::gstate::RELOCATE_STATE_BODY_SIZE],
        "RelocateState bodies must XOR to zero after clean relocations; \
         non-zero means accumulate_gstate's reachable set or the \
         parent-cancel pairing has regressed"
    );

    // Steady-state remount must succeed without recovery side effects.
    let storage = fs.into_storage();
    let mut a2 = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b2 = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs2 = Fs::mount(storage, &mut a2, &mut b2).unwrap();
    let mut out = vec![0u8; 64];
    let n = fs2
        .read_at_path(Path::new("/sub/hot").unwrap(), 0, &mut out, &mut a2, &mut b2)
        .unwrap();
    let expected_len = 16 + (199u32 % 32) as usize;
    assert_eq!(n, expected_len);
    assert!(out[..n].iter().all(|&c| c == b'x'));
}
