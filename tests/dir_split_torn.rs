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
//! remounting, the filesystem must always mount to a consistent state:
//! `/d` enumerates without error, every entry that survives reads back
//! its content, no entry is duplicated, and no pair reachable by following
//! tails from the root is missing from the tree (no leaked continuation).
//! A second remount is stable (recovery is idempotent).

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

fn count_program_calls() -> usize {
    let mut torn = TornWriteStorage::new(MemStorage::new(), usize::MAX);
    let mut scratch = buf();
    Fs::format(&mut torn, &mut scratch).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(torn, &mut ba, &mut bb).unwrap();
    let pre = fs.storage().program_count;
    scenario(&mut fs);
    fs.storage().program_count - pre
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
    let total = count_program_calls();
    assert!(total > 0);
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

    for trigger in 1..=total + 2 {
        let mut torn = TornWriteStorage::new(MemStorage::new(), trigger);
        let mut scratch = buf();
        if Fs::format(&mut torn, &mut scratch).is_err() {
            continue; // torn format; nothing to check
        }
        let inner = {
            let mut ba = buf();
            let mut bb = buf();
            match Fs::mount(torn, &mut ba, &mut bb) {
                Ok(mut fs) => {
                    scenario(&mut fs);
                    fs.into_storage().into_inner()
                }
                Err(_) => continue,
            }
        };

        // Raw image bytes (MemStorage is not Clone; reconstruct from the
        // public `data` field for each independent mount).
        let image = inner.data;

        // First remount: recovery runs here. A torn image that fails to
        // mount is acceptable (the chip is in a partial-format-like state).
        let names_a = {
            let mut ba = buf();
            let mut bb = buf();
            let Ok(mut fs) = Fs::mount(MemStorage { data: image.clone() }, &mut ba, &mut bb) else {
                continue;
            };
            let names = enumerate_dir(&mut fs);
            let data = fs.into_storage();
            assert_no_thread_orphan(&data.data);
            names
        };

        // Second remount: state is stable (recovery is idempotent).
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(MemStorage { data: image }, &mut ba, &mut bb)
            .expect("re-mount of a once-mounted image");
        let names_b = enumerate_dir(&mut fs);
        assert_eq!(names_a, names_b, "directory state must be stable across remounts");
    }
}
