//! Crash safety for `lfs-cvh` write-side directory splitting.
//!
//! When a directory pair overflows, the writer allocates a HardTail
//! continuation, programs it FIRST, then commits the original's lower
//! half with a HardTail to it (`split_directory_pair`). The crash window
//! is the gap between those two: a power loss after the continuation is
//! written but before the original's commit lands leaves the continuation
//! referenced by nothing — an unreferenced orphan the allocator reclaims,
//! exactly the mkdir-create window.
//!
//! Scenario: `mkdir /d` then write enough small files into `/d` to force
//! at least one split. Tearing at every program-call boundary and
//! remounting, the filesystem must always mount (strong semantics,
//! review H7: once format completed, an unmountable post-tear image is
//! a bricked device and an immediate failure) to an exact prefix of
//! the operation sequence: `/d` absent, or `/d` holding exactly
//! `f00..f(k-1)` for some `k`, each entry reading back its content,
//! no entry duplicated, and no pair reachable by following tails from
//! the root missing from the tree (no leaked continuation). A second
//! remount is stable (recovery is idempotent).
//!
//! `torn_multi_cut_directory_split_is_atomic_across_every_power_loss`
//! sweeps the same window for a split that has to cut twice (review L1):
//! a `set_attr` grows an entry the first cut would leave behind, so the
//! writer programs two continuations before the single linearizing
//! commit. The crash window is wider by one continuation, and the
//! invariant is the same: nothing is reachable until the lower commit
//! lands, so every tear reads as the pre-state or the post-state.

use littlefs2_pure::meta::MetadataReader;
use littlefs2_pure::{BlockPair, Fs, Path};

mod common;
use common::{MemStorage, TornWriteStorage};

const BS: usize = MemStorage::BLOCK_SIZE;

fn buf() -> [u8; BS] {
    [0u8; BS]
}

/// Number of small files written into `/d`. On the 256-byte / 8-block
/// device this overflows one metadata pair (~13 small entries) and forces
/// a single split into a HardTail continuation, while leaving free blocks
/// for that continuation.
const ENTRIES: u32 = 16;

fn scenario(fs: &mut Fs<TornWriteStorage>) {
    let mut a = buf();
    let mut b = buf();
    let _ = fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b);
    for i in 0..ENTRIES {
        let s = format!("/d/f{i:02}");
        let _ = fs.write_to_path(Path::new(&s).unwrap(), b"x", &mut a, &mut b);
    }
}

/// Parse the active block of `pair` from a raw image, returning its live
/// DirStruct children, its tail, and whether that tail is a HardTail
/// (this directory's own continuation) versus a SoftTail (the global
/// thread to the next directory).
fn read_pair(data: &[u8], pair: BlockPair) -> (Vec<BlockPair>, Option<BlockPair>, bool) {
    let rd = |blk: u32| {
        let s = (blk as usize) * BS;
        MetadataReader::new(&data[s..s + BS]).ok()
    };
    let ra = rd(pair.a.as_u32());
    let rb = rd(pair.b.as_u32());
    let active = match (&ra, &rb) {
        (Some(x), Some(y)) => {
            if (x.revision().wrapping_sub(y.revision()) as i32) >= 0 {
                ra
            } else {
                rb
            }
        }
        (Some(_), None) => ra,
        (None, Some(_)) => rb,
        (None, None) => None,
    };
    let mut children = Vec::new();
    let mut tail = None;
    let mut is_hard = false;
    if let Some(r) = active {
        tail = r.tail();
        is_hard = r.is_hard_tail();
        for e in r.iter_tags() {
            if e.tag.tag_type() == littlefs2_pure::tag::TagType::DirStruct && e.body.len() == 8 {
                let a = u32::from_le_bytes([e.body[0], e.body[1], e.body[2], e.body[3]]);
                let b = u32::from_le_bytes([e.body[4], e.body[5], e.body[6], e.body[7]]);
                children.push(BlockPair::new(
                    littlefs2_pure::BlockAddress::new(a),
                    littlefs2_pure::BlockAddress::new(b),
                ));
            }
        }
    }
    (children, tail, is_hard)
}

/// Assert every pair reachable by following tails from the root is also a
/// live tree pair (a DirStruct child, or a HardTail continuation of one).
/// A split leaves a HardTail continuation, so the tree set must follow
/// HardTails as well as DirStruct references — otherwise a legitimate
/// continuation would read as an orphan.
fn assert_no_thread_orphan(data: &[u8]) {
    let root =
        BlockPair::new(littlefs2_pure::BlockAddress::new(0), littlefs2_pure::BlockAddress::new(1));

    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() {
        let (children, tail, is_hard) = read_pair(data, tree[i]);
        for c in children {
            if !tree.contains(&c) {
                tree.push(c);
            }
        }
        // A HardTail continues this directory's own pair chain; include it
        // in the tree set.
        if is_hard {
            if let Some(cont) = tail {
                if !tree.contains(&cont) {
                    tree.push(cont);
                }
            }
        }
        i += 1;
    }

    let mut cur = root;
    let mut seen = vec![root];
    for _ in 0..64 {
        let (_children, tail, _is_hard) = read_pair(data, cur);
        match tail {
            None => return,
            Some(next) => {
                assert!(
                    tree.contains(&next),
                    "thread reaches {next:?} which is not a live tree pair (leaked continuation)",
                );
                if seen.contains(&next) {
                    return; // cycle in a corrupt image; not this test's concern
                }
                seen.push(next);
                cur = next;
            }
        }
    }
}

/// Enumerate `/d` on a mounted image: return the sorted list of entry
/// names, asserting each reads back its one-byte content and that no name
/// repeats. Returns `None` if the directory does not resolve (a valid
/// pre-operation state where `/d` was not yet created).
fn enumerate_dir(fs: &mut Fs<MemStorage>) -> Option<Vec<Vec<u8>>> {
    let mut a = buf();
    let mut b = buf();
    if !fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).ok()? {
        return None;
    }
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).ok()?;
    // No duplicate names (a split that mis-renumbered ids could surface
    // the same entry twice).
    let mut sorted = names.clone();
    sorted.sort();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "a split must not duplicate a directory entry");
    // Every surviving entry reads back its content.
    for name in &names {
        let s = core::str::from_utf8(name).unwrap();
        let path = format!("/d/{s}");
        let mut out = [0u8; 1];
        let n = fs.read_at_path(Path::new(&path).unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!((n, out[0]), (1, b'x'), "entry {s} content survived the split");
    }
    Some(sorted)
}

#[test]
fn torn_directory_split_is_atomic_across_every_power_loss() {
    let (fmt_calls, scenario_calls) = common::torn_call_counts(scenario);
    assert!(scenario_calls > 0);
    // The scenario must actually split: a single 256-byte pair cannot hold
    // 16 entries, so a clean (untorn) run produces a multi-pair directory.
    {
        let mut storage = MemStorage::new();
        let mut scratch = buf();
        Fs::format(&mut storage, &mut scratch).unwrap();
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = buf();
        let mut b = buf();
        fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
        let mut ok = 0u32;
        for i in 0..ENTRIES {
            let s = format!("/d/f{i:02}");
            if fs.write_to_path(Path::new(&s).unwrap(), b"x", &mut a, &mut b).is_ok() {
                ok += 1;
            }
        }
        let data = fs.into_storage();
        assert_no_thread_orphan(&data.data);
        // A single pair holds far fewer than 16 of these entries; reaching
        // this many proves the directory grew across a continuation.
        assert!(ok >= 14, "scenario should fill past one pair (got {ok})");
    }

    for trigger in 1..=fmt_calls + scenario_calls + 2 {
        let image = match common::run_torn_scenario(trigger, scenario) {
            common::TornRun::TornFormat => {
                assert!(
                    trigger <= fmt_calls,
                    "trigger {trigger}: format reported torn past its own \
                     {fmt_calls} program calls"
                );
                continue;
            }
            common::TornRun::Image(image) => image,
        };

        // First remount: recovery runs here. Strong semantics (review
        // H7): the image held a valid filesystem before the tear, so
        // it MUST mount; the pre-H7 sweep silently `continue`d past
        // unmountable images, accepting bricked devices.
        let names_a = {
            let mut fs = common::mount_image_strict(
                image.clone(),
                &format!("split sweep trigger {trigger}, first remount"),
            );
            let names = enumerate_dir(&mut fs);
            // The surviving directory must be an exact prefix of the
            // write sequence: each commit is atomic and the writes are
            // sequential, so any other set means a write half-landed.
            if let Some(names) = &names {
                for (i, name) in names.iter().enumerate() {
                    assert_eq!(
                        name,
                        format!("f{i:02}").as_bytes(),
                        "trigger {trigger}: surviving entries must be an exact \
                         prefix of the write sequence, got {names:?}"
                    );
                }
            }
            let data = fs.into_storage();
            assert_no_thread_orphan(&data.data);
            names
        };

        // Second remount: state is stable (recovery is idempotent).
        let mut fs = common::mount_image_strict(
            image,
            &format!("split sweep trigger {trigger}, second remount"),
        );
        let names_b = enumerate_dir(&mut fs);
        assert_eq!(names_a, names_b, "directory state must be stable across remounts");
    }
}

// --- multi-cut split (review L1) -------------------------------------

/// Attribute sizes that drive the pair into a state one cut cannot
/// place. `MC_A0` and `MC_A1` grow entries 0 and 1 by log append, then
/// `MC_TRIGGER` forces a compaction whose combined range is 304 bytes:
/// the first cut leaves a 278-byte lower portion, which no 256-byte
/// block holds, so a second cut has to follow. The byte accounting is
/// spelled out in `tests/review_l1_split_recheck.rs`.
const MC_A0: usize = 60;
const MC_A1: usize = 60;
const MC_TRIGGER: usize = 120;

/// Names of the four entries the multi-cut scenario creates, in
/// creation order.
const MC_NAMES: [&str; 4] = ["0", "1", "2", "3"];

fn multi_cut_scenario(fs: &mut Fs<TornWriteStorage>) {
    let mut a = buf();
    let mut b = buf();
    let _ = fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b);
    for name in MC_NAMES {
        let p = format!("/d/{name}");
        let _ = fs.write_to_path(Path::new(&p).unwrap(), b"", &mut a, &mut b);
    }
    let _ = fs.set_attr(Path::new("/d/0").unwrap(), 1, &[0xA0; MC_A0], &mut a, &mut b);
    let _ = fs.set_attr(Path::new("/d/1").unwrap(), 1, &[0xA1; MC_A1], &mut a, &mut b);
    let _ = fs.set_attr(Path::new("/d/0").unwrap(), 2, &[0xB0; MC_TRIGGER], &mut a, &mut b);
}

/// Read attribute `id` on `/d/<name>` and assert it is either absent
/// (`Ok(0)`, the pre-state of that `set_attr`) or exactly `len` bytes of
/// `fill` (its post-state). A partial value would mean a `set_attr`
/// half-landed, which no split may allow. A tear before the entry was
/// created leaves nothing to check.
fn assert_attr_all_or_nothing(
    fs: &mut Fs<MemStorage>,
    name: &str,
    id: u8,
    len: usize,
    fill: u8,
    ctx: &str,
) {
    let mut a = buf();
    let mut b = buf();
    let mut out = [0u8; BS];
    let path = format!("/d/{name}");
    if !fs.exists(Path::new(&path).unwrap(), &mut a, &mut b).unwrap() {
        return;
    }
    let n = fs.get_attr(Path::new(&path).unwrap(), id, &mut out, &mut a, &mut b).unwrap();
    if n == 0 {
        return;
    }
    assert_eq!(n, len, "{ctx}: attr {id} on {name} landed partially ({n} bytes)");
    assert!(
        out[..len].iter().all(|&x| x == fill),
        "{ctx}: attr {id} on {name} landed with wrong content"
    );
}

/// Enumerate `/d` for the multi-cut scenario: the surviving names must
/// be an exact prefix of the creation order, with no duplicates.
/// Returns `None` when `/d` does not resolve.
fn enumerate_multi_cut_dir(fs: &mut Fs<MemStorage>, ctx: &str) -> Option<Vec<Vec<u8>>> {
    let mut a = buf();
    let mut b = buf();
    if !fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).ok()? {
        return None;
    }
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).ok()?;
    let mut sorted = names.clone();
    sorted.sort();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "{ctx}: a multi-cut split must not duplicate an entry");
    for (i, name) in names.iter().enumerate() {
        assert_eq!(
            name.as_slice(),
            MC_NAMES[i].as_bytes(),
            "{ctx}: surviving entries must be an exact prefix of the write sequence, got {names:?}"
        );
    }
    Some(names)
}

/// Length of `/d`'s HardTail chain in a raw image, counting the first
/// pair. Three means the split cut twice.
fn multi_cut_chain_len(data: &[u8]) -> u32 {
    let root =
        BlockPair::new(littlefs2_pure::BlockAddress::new(0), littlefs2_pure::BlockAddress::new(1));
    let (children, _, _) = read_pair(data, root);
    let mut cur = *children.first().expect("root must reference /d");
    let mut chain = 1;
    for _ in 0..8 {
        let (_, tail, is_hard) = read_pair(data, cur);
        match (is_hard, tail) {
            (true, Some(next)) => {
                chain += 1;
                cur = next;
            }
            _ => break,
        }
    }
    chain
}

#[test]
fn torn_multi_cut_directory_split_is_atomic_across_every_power_loss() {
    let (fmt_calls, scenario_calls) = common::torn_call_counts(multi_cut_scenario);
    assert!(scenario_calls > 0);

    // The scenario must actually cut twice on an untorn run, otherwise
    // the sweep below would be covering the ordinary single-cut window
    // again. Three pairs in the chain means two continuations.
    {
        let mut storage = MemStorage::new();
        let mut scratch = buf();
        Fs::format(&mut storage, &mut scratch).unwrap();
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = buf();
        let mut b = buf();
        fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
        for name in MC_NAMES {
            let p = format!("/d/{name}");
            fs.write_to_path(Path::new(&p).unwrap(), b"", &mut a, &mut b).unwrap();
        }
        fs.set_attr(Path::new("/d/0").unwrap(), 1, &[0xA0; MC_A0], &mut a, &mut b).unwrap();
        fs.set_attr(Path::new("/d/1").unwrap(), 1, &[0xA1; MC_A1], &mut a, &mut b).unwrap();
        fs.set_attr(Path::new("/d/0").unwrap(), 2, &[0xB0; MC_TRIGGER], &mut a, &mut b)
            .expect("the growing set_attr must re-split rather than fail");
        let data = fs.into_storage();
        assert_no_thread_orphan(&data.data);
        assert_eq!(
            multi_cut_chain_len(&data.data),
            3,
            "the scenario must produce a two-cut split (three pairs)"
        );
    }

    for trigger in 1..=fmt_calls + scenario_calls + 2 {
        let image = match common::run_torn_scenario(trigger, multi_cut_scenario) {
            common::TornRun::TornFormat => {
                assert!(
                    trigger <= fmt_calls,
                    "trigger {trigger}: format reported torn past its own \
                     {fmt_calls} program calls"
                );
                continue;
            }
            common::TornRun::Image(image) => image,
        };

        let ctx = format!("multi-cut sweep trigger {trigger}");
        let names_a = {
            let mut fs =
                common::mount_image_strict(image.clone(), &format!("{ctx}, first remount"));
            let names = enumerate_multi_cut_dir(&mut fs, &ctx);
            if names.is_some() {
                assert_attr_all_or_nothing(&mut fs, "0", 1, MC_A0, 0xA0, &ctx);
                assert_attr_all_or_nothing(&mut fs, "0", 2, MC_TRIGGER, 0xB0, &ctx);
                assert_attr_all_or_nothing(&mut fs, "1", 1, MC_A1, 0xA1, &ctx);
            }
            let data = fs.into_storage();
            assert_no_thread_orphan(&data.data);
            names
        };

        let mut fs = common::mount_image_strict(image, &format!("{ctx}, second remount"));
        let names_b = enumerate_multi_cut_dir(&mut fs, &ctx);
        assert_eq!(names_a, names_b, "{ctx}: directory state must be stable across remounts");
    }
}
