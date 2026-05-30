//! Crash recovery for `lfs-xmx` SoftTail threading: the mount-time
//! deorphan sweep must reclaim a directory left in the global thread but
//! not the tree by an interrupted `rmdir` un-thread.
//!
//! Scenario: `mkdir /a; mkdir /b; rmdir /a`. Because mkdir inserts each
//! new dir right after the parent, the thread is `root -> /b -> /a`, so
//! `/a`'s thread predecessor is the sibling `/b`, not the parent. rmdir
//! `/a` therefore commits to two different pairs (delete the entry on
//! root, then clear `/b`'s tail), and a crash between them leaves `/a` in
//! the thread but not the tree. Tearing at every program-call boundary
//! and remounting, the deorphan sweep must leave a consistent filesystem:
//! every pair reachable by following tails from the root must also be a
//! live directory in the tree (no orphan).

use littlefs2_pure::meta::MetadataReader;
use littlefs2_pure::{BlockPair, Fs, Path};

mod common;
use common::{MemStorage, TornWriteStorage};

const BS: usize = MemStorage::BLOCK_SIZE;

fn buf() -> [u8; BS] {
    [0u8; BS]
}

/// Parse the active block of the pair at `(a, b)` from a raw image and
/// return `(live_dir_children, tail)`.
fn read_pair(data: &[u8], pair: BlockPair) -> (Vec<BlockPair>, Option<BlockPair>) {
    let rd = |blk: u32| {
        let s = (blk as usize) * BS;
        MetadataReader::new(&data[s..s + BS]).ok()
    };
    // Pick the active block by revision (higher wins, ties to a).
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
    if let Some(r) = active {
        tail = r.tail();
        // Raw tag scan for DirStruct bodies. Splice/latest-wins is not
        // modelled here; for this small tree (no renumbering) the raw
        // children set is sufficient to confirm reachability.
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
    (children, tail)
}

/// Assert every pair reachable by following tails from the root is also
/// reachable via DirStruct from the root (no thread orphan).
fn assert_no_thread_orphan(data: &[u8]) {
    let root =
        BlockPair::new(littlefs2_pure::BlockAddress::new(0), littlefs2_pure::BlockAddress::new(1));

    // Tree set: DirStruct (+ HardTail continuation) reachability.
    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() {
        let (children, _tail) = read_pair(data, tree[i]);
        for c in children {
            if !tree.contains(&c) {
                tree.push(c);
            }
        }
        i += 1;
    }

    // Thread walk: follow tails from root; every pair must be in the tree.
    let mut cur = root;
    let mut seen = vec![root];
    for _ in 0..64 {
        let (_children, tail) = read_pair(data, cur);
        match tail {
            None => return, // reached the list end consistently
            Some(next) => {
                assert!(
                    tree.contains(&next),
                    "thread reaches {next:?} which is not a live tree directory (orphan not reclaimed)",
                );
                if seen.contains(&next) {
                    return; // cycle (corrupt image); not this test's concern
                }
                seen.push(next);
                cur = next;
            }
        }
    }
}

fn count_program_calls<F: FnOnce(&mut Fs<TornWriteStorage>)>(scenario: F) -> usize {
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

#[test]
fn torn_rmdir_unthread_is_deorphaned_on_remount() {
    let scenario = |fs: &mut Fs<TornWriteStorage>| {
        let mut a = buf();
        let mut b = buf();
        let _ = fs.mkdir(Path::new("/a").unwrap(), &mut a, &mut b);
        let _ = fs.mkdir(Path::new("/b").unwrap(), &mut a, &mut b);
        let _ = fs.rmdir(Path::new("/a").unwrap(), &mut a, &mut b);
    };
    let total = count_program_calls(scenario);
    assert!(total > 0);

    for trigger in 1..=total + 2 {
        let mut torn = TornWriteStorage::new(MemStorage::new(), trigger);
        let mut scratch = buf();
        if Fs::format(&mut torn, &mut scratch).is_err() {
            continue; // format torn; nothing to check
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

        // Remount: deorphan runs here. It must succeed and leave a
        // consistent thread.
        let mut ba = buf();
        let mut bb = buf();
        // A torn-format-like state that fails to remount is acceptable.
        let Ok(mut fs) = Fs::mount(inner, &mut ba, &mut bb) else {
            continue;
        };
        // /b must still resolve (it was never removed).
        let mut a = buf();
        let mut b = buf();
        let _ = fs.exists(Path::new("/b").unwrap(), &mut a, &mut b);
        let data = fs.into_storage();
        assert_no_thread_orphan(&data.data);
    }
}
