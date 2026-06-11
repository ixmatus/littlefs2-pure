//! Reproducers for the 2026-06 deep review findings H4, H3, and C6
//! (beads lfs-les, lfs-gfm, lfs-bkq): the relocation-cascade family.
//!
//! Oracle: the C reference repoints half-orphans during deorphan
//! (pass 0 of `lfs_fs_deorphan`: the tree's `DirStruct` is
//! authoritative and the thread tail is re-synced to it), and patches
//! the pending move at every relocation-cascade commit site
//! ("this looks like an optimization but is in fact _required_ since
//! relocating may outdate the move", lfs.c:2484, 2536).

use littlefs2_pure::meta::MetadataReader;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::{BlockAddress, BlockPair, Fs, Path};

mod common;

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// 32-block geometry with aggressive wear levelling, as in
/// `tests/wear_leveling.rs`.
#[derive(Debug)]
struct WearStorage {
    data: Vec<u8>,
}

impl WearStorage {
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 32;

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
        for v in &mut self.data[start..end] {
            *v = 0xFF;
        }
        Ok(())
    }
}

/// Torn-write wrapper: program and erase fail after `trigger_at`
/// program calls, modelling power loss at a program boundary.
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
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = 1;

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

/// Collect the global thread (every pair reachable from the root by
/// following tail tags, hard and soft) from the raw device.
fn thread_pairs(storage: &mut WearStorage) -> Vec<BlockPair> {
    let mut out = vec![BlockPair::new(BlockAddress::new(0), BlockAddress::new(1))];
    let mut idx = 0;
    while idx < out.len() && out.len() <= WearStorage::BLOCK_COUNT as usize {
        let pair = out[idx];
        idx += 1;
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        storage.read(pair.a.as_u32(), 0, &mut a).unwrap();
        storage.read(pair.b.as_u32(), 0, &mut b).unwrap();
        let parsed =
            littlefs2_pure::meta::MetadataPair::parse(pair.a, &a, pair.b, &b).expect("parses");
        if let Some(t) = parsed.reader.tail() {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// The metadata pair a directory entry resolves to through the tree.
fn dir_pair_of(fs: &mut Fs<WearStorage>, path: &str) -> BlockPair {
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
    let r = fs.resolve(p(path), &mut a, &mut b).expect("dir resolves");
    assert_eq!(r.struct_body.len(), 8);
    let lo = u32::from_le_bytes([
        r.struct_body[0],
        r.struct_body[1],
        r.struct_body[2],
        r.struct_body[3],
    ]);
    let hi = u32::from_le_bytes([
        r.struct_body[4],
        r.struct_body[5],
        r.struct_body[6],
        r.struct_body[7],
    ]);
    BlockPair::new(BlockAddress::new(lo), BlockAddress::new(hi))
}

/// H4: a crash between `propagate_relocation`'s parent commit and
/// predecessor commit leaves the tree holding the relocated pair while
/// the thread still points at the outdated twin. The deorphan sweep
/// must REPOINT the thread at the tree's pair (the C half-orphan fix),
/// not reclaim the stale link, which permanently drops the live pair
/// from the thread.
///
/// Sweep every program-call boundary over a scenario that relocates
/// subdirectory pairs under wear. After each torn run: remount must
/// succeed, both files must read back as a known pre or post state,
/// and EVERY directory's tree pair must be reachable through the
/// global thread (the thread/tree sync invariant the C reference's
/// allocator and traverse depend on).
#[test]
fn h4_thread_follows_tree_across_every_power_loss() {
    // Seed: two dirs, each with a file, so the relocating pair's
    // thread predecessor can differ from its tree parent.
    let mut seed = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut seed, &mut scratch).unwrap();
    let seed_data = {
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(seed, &mut a, &mut b).unwrap();
        fs.mkdir(p("/d1"), &mut a, &mut b).unwrap();
        fs.mkdir(p("/d2"), &mut a, &mut b).unwrap();
        fs.write_to_path(p("/d1/k"), b"PRE", &mut a, &mut b).unwrap();
        fs.write_to_path(p("/d2/k"), b"PRE", &mut a, &mut b).unwrap();
        fs.into_storage().data
    };

    // Scenario: hammer both dirs so each pair compacts and (with
    // BLOCK_CYCLES = 1) relocates repeatedly.
    let scenario = |fs: &mut Fs<TornWearStorage>| {
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        for i in 0..14u32 {
            let val = vec![b'q'; 16 + (i % 16) as usize];
            let _ = fs.write_to_path(p("/d1/k"), &val, &mut a, &mut b);
            let _ = fs.write_to_path(p("/d2/k"), &val, &mut a, &mut b);
        }
        let _ = fs.write_to_path(p("/d1/k"), b"POST", &mut a, &mut b);
        let _ = fs.write_to_path(p("/d2/k"), b"POST", &mut a, &mut b);
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
        fs.storage().program_count - before
    };
    assert!(total_calls > 0);

    for trigger in 1..=total_calls {
        let mut s = WearStorage::new();
        s.data = seed_data.clone();
        let torn = TornWearStorage::new(s, trigger);
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(torn, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: pre-scenario mount failed: {e:?}"));
        scenario(&mut fs);
        let inner = fs.into_storage().into_inner();

        // Remount on the untorn device: recovery must complete.
        let mut fs2 = Fs::mount(inner, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: post-torn remount failed: {e:?}"));

        // Content invariant: both files at a known state.
        for path in ["/d1/k", "/d2/k"] {
            let mut out = vec![0u8; 64];
            let n = fs2
                .read_at_path(p(path), 0, &mut out, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("trigger {trigger}: {path} unreadable: {e:?}"));
            let content = &out[..n];
            assert!(
                content == b"PRE" || content == b"POST" || content.iter().all(|&c| c == b'q'),
                "trigger {trigger}: {path} read back as {content:?}"
            );
        }

        // Thread/tree sync invariant (H4): every directory's tree pair
        // is reachable through the global thread.
        let d1 = dir_pair_of(&mut fs2, "/d1");
        let d2 = dir_pair_of(&mut fs2, "/d2");
        let mut storage = fs2.into_storage();
        let thread = thread_pairs(&mut storage);
        for (name, pair) in [("/d1", d1), ("/d2", d2)] {
            assert!(
                thread.contains(&pair),
                "trigger {trigger}: {name}'s tree pair {pair:?} is missing from the \
                 global thread {thread:?}; the deorphan sweep reclaimed a half-orphan \
                 instead of repointing the thread at the relocated pair (H4)"
            );
        }
    }
}

/// C6: a cross-directory rename whose destination commit cascades a
/// relocation into the SOURCE pair outdates the captured source
/// coordinates; the source delete (and a crashed-rename recovery)
/// then target the orphaned old address: duplicate entry, stale
/// MoveState, eventually an unmountable image.
///
/// Grid: the destination directory is nested inside the source
/// directory, so every relocation of the destination pair commits
/// into the source pair while the move is pending; with
/// `BLOCK_CYCLES = 1` those commits periodically relocate the source
/// pair mid-rename. Each iteration asserts the rename's
/// post-conditions; the final double-mount checks quiescence.
#[test]
fn c6_rename_survives_relocation_cascade_into_source() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut buf_b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];

    fs.mkdir(p("/src"), &mut a, &mut b).unwrap();
    fs.mkdir(p("/src/dst"), &mut a, &mut b).unwrap();

    for round in 0..60u32 {
        // A fresh file in the source dir, with filler so both pairs
        // advance toward compaction (and relocation) on different
        // rounds.
        let content = vec![b'A' + (round % 26) as u8; 8 + (round % 24) as usize];
        fs.write_to_path(p("/src/f"), &content, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("round {round}: write /src/f failed: {e:?}"));
        let filler = vec![b'z'; 8 + ((round * 7) % 24) as usize];
        fs.write_to_path(p("/src/dst/pad"), &filler, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("round {round}: write pad failed: {e:?}"));

        // The cross-directory rename under test.
        fs.rename(p("/src/f"), p("/src/dst/f"), &mut a, &mut b)
            .unwrap_or_else(|e| panic!("round {round}: rename failed: {e:?}"));

        // Post-conditions: the source entry is gone, the destination
        // holds the content, exactly once.
        let r = fs
            .resolve(p("/src/dst/f"), &mut a, &mut b)
            .unwrap_or_else(|e| panic!("round {round}: /src/dst/f missing after rename: {e:?}"));
        assert_eq!(r.struct_body, &content[..], "round {round}: moved content diverged");
        assert!(
            fs.resolve(p("/src/f"), &mut a, &mut b).is_err(),
            "round {round}: duplicate entry: /src/f still resolves after the rename \
             (the source delete targeted an outdated pair address, C6)"
        );

        // Clean up for the next round.
        fs.remove_at_path(p("/src/dst/f"), &mut a, &mut b)
            .unwrap_or_else(|e| panic!("round {round}: cleanup remove failed: {e:?}"));
    }

    // Quiescence: a remount succeeds and a second mount writes nothing.
    let mut storage = fs.into_storage();
    let before: Vec<u8> = storage.data.clone();
    {
        let fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
            .expect("C6: image unmountable after the rename grid");
        storage = fs.into_storage();
    }
    // Mount may quiesce residual state once; the SECOND mount must be
    // byte-stable.
    let _ = before;
    let after_first: Vec<u8> = storage.data.clone();
    {
        let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("second mount");
        storage = fs.into_storage();
    }
    assert_eq!(
        after_first, storage.data,
        "C6: consecutive mounts keep writing; a stale MoveState can never cancel"
    );
}

/// H3: an abandoned wear relocation (the fresh block refuses the
/// program) leaves an unbalanced `RelocateState` on the pair. If the
/// pair relocates again before remount, the stale body references
/// dead addresses and mount recovery commits to a dead pair on every
/// mount. The abandonment must self-cancel instead of deferring to
/// mount.
///
/// Storage: programs to one designated block fail (the worn fresh
/// candidate); everything else succeeds. Drive a relocation that
/// lands on the worn block (abandoned), heal the block, keep writing
/// until a later relocation succeeds, then remount twice: both must
/// succeed and the second must be byte-stable.
#[test]
fn h3_abandoned_relocation_self_cancels() {
    /// WearStorage with one block whose programs fail while `worn` is
    /// set.
    struct OneWornStorage {
        inner: WearStorage,
        worn_block: u32,
        worn: bool,
    }
    impl Storage for OneWornStorage {
        type Error = ();
        const READ_SIZE: usize = WearStorage::READ_SIZE;
        const PROG_SIZE: usize = WearStorage::PROG_SIZE;
        const BLOCK_SIZE: usize = WearStorage::BLOCK_SIZE;
        const BLOCK_COUNT: u32 = WearStorage::BLOCK_COUNT;
        const CACHE_SIZE: usize = 64;
        const LOOKAHEAD_SIZE: usize = 8;
        const BLOCK_CYCLES: i32 = 1;

        fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
            self.inner.read(block, off, buf)
        }
        fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
            if self.worn && block == self.worn_block {
                return Err(());
            }
            self.inner.program(block, off, data)
        }
        fn erase(&mut self, block: u32) -> Result<(), ()> {
            self.inner.erase(block)
        }
    }

    // Try every plausible fresh-candidate block as the worn one; the
    // allocator's choice depends on layout, so sweep until some run
    // exhibits an abandoned relocation (worn fresh) and still must
    // end in a quiescent, mountable image. Runs where the worn block
    // is never chosen are vacuously fine.
    for worn_block in 4..WearStorage::BLOCK_COUNT {
        let mut seed = WearStorage::new();
        let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
        Fs::format(&mut seed, &mut scratch).unwrap();
        let mut buf_a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut buf_b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = {
            let mut f = Fs::mount(seed, &mut buf_a, &mut buf_b).unwrap();
            let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
            let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
            f.mkdir(p("/sub"), &mut a, &mut b).unwrap();
            f.write_to_path(p("/sub/k"), b"seed", &mut a, &mut b).unwrap();
            Fs::mount(
                OneWornStorage { inner: f.into_storage(), worn_block, worn: true },
                &mut buf_a,
                &mut buf_b,
            )
            .unwrap()
        };

        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        // Phase 1: relocations under the worn block; any that pick it
        // are abandoned (or rerouted by the failure-driven path).
        for i in 0..30u32 {
            let val = vec![b'w'; 8 + (i % 24) as usize];
            fs.write_to_path(p("/sub/k"), &val, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("worn {worn_block}, write {i}: {e:?}"));
        }
        // Phase 2: heal the block; later relocations succeed.
        fs.storage_mut().worn = false;
        for i in 0..30u32 {
            let val = vec![b'h'; 8 + (i % 24) as usize];
            fs.write_to_path(p("/sub/k"), &val, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("healed {worn_block}, write {i}: {e:?}"));
        }

        // Remount twice; the image must be mountable and quiescent.
        let inner = fs.into_storage().inner;
        let mut storage = inner;
        {
            let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_or_else(|e| {
                panic!("worn {worn_block}: remount failed: {e:?} (stale RelocateState, H3)")
            });
            storage = fs.into_storage();
        }
        let after_first = storage.data.clone();
        {
            let fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
                .unwrap_or_else(|e| panic!("worn {worn_block}: second mount failed: {e:?}"));
            storage = fs.into_storage();
        }
        assert_eq!(
            after_first, storage.data,
            "worn {worn_block}: consecutive mounts keep writing; an abandoned \
             relocation's RelocateState never cancelled (H3)"
        );
    }
}

// Quiet the unused-import lint when individual tests are filtered.
#[allow(unused_imports)]
use MetadataReader as _;

/// C6 crash window: power loss at every program boundary across a
/// cross-directory rename whose commits relocate pairs under wear.
/// After each torn run the remount must recover to a consistent
/// state: exactly one of source and destination holds the file (the
/// pre state, or the post state via mount-time move recovery), the
/// content is intact, and a second mount is byte-stable (no futile
/// recovery loop). This exercises `recover_pending_move`'s resolution
/// of a source pair that relocated after the destination commit
/// (the stale-coordinate decode), including the relocated-twin path.
#[test]
fn c6_rename_recovers_across_every_power_loss() {
    // Seed: the nested topology, advanced until pairs are compaction-
    // prone, with the file staged at the source.
    let mut seed = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut seed, &mut scratch).unwrap();
    let seed_data = {
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(seed, &mut a, &mut b).unwrap();
        fs.mkdir(p("/src"), &mut a, &mut b).unwrap();
        fs.mkdir(p("/src/dst"), &mut a, &mut b).unwrap();
        for i in 0..10u32 {
            let filler = vec![b'y'; 8 + (i % 24) as usize];
            fs.write_to_path(p("/src/dst/pad"), &filler, &mut a, &mut b).unwrap();
            fs.write_to_path(p("/src/g"), &filler, &mut a, &mut b).unwrap();
        }
        fs.write_to_path(p("/src/f"), b"MOVED", &mut a, &mut b).unwrap();
        fs.into_storage().data
    };

    let scenario = |fs: &mut Fs<TornWearStorage>| {
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let _ = fs.rename(p("/src/f"), p("/src/dst/f"), &mut a, &mut b);
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
        fs.storage().program_count - before
    };
    assert!(total_calls > 0);

    for trigger in 1..=total_calls {
        let mut s = WearStorage::new();
        s.data = seed_data.clone();
        let torn = TornWearStorage::new(s, trigger);
        let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut b = vec![0u8; WearStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(torn, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: pre-scenario mount failed: {e:?}"));
        scenario(&mut fs);
        let inner = fs.into_storage().into_inner();

        let mut fs2 = Fs::mount(inner, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("trigger {trigger}: post-torn remount failed: {e:?}"));
        let src_has = fs2.resolve(p("/src/f"), &mut a, &mut b).is_ok();
        let dst_r = fs2.resolve(p("/src/dst/f"), &mut a, &mut b);
        match (src_has, &dst_r) {
            (true, Err(_)) => {} // pre state
            (false, Ok(r)) => {
                assert_eq!(r.struct_body, b"MOVED", "trigger {trigger}: moved content corrupted");
            }
            (true, Ok(_)) => {
                panic!("trigger {trigger}: duplicate entry after recovery (move never completed)")
            }
            (false, Err(e)) => {
                panic!("trigger {trigger}: file vanished from both directories: {e:?}")
            }
        }
        // Quiescence: a second mount writes nothing.
        let mut storage = fs2.into_storage();
        let snap = storage.data.clone();
        {
            let fs3 = Fs::mount(storage, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("trigger {trigger}: second mount failed: {e:?}"));
            storage = fs3.into_storage();
        }
        assert_eq!(
            snap, storage.data,
            "trigger {trigger}: consecutive mounts keep writing (unbalanced gstate residue)"
        );
    }
}
