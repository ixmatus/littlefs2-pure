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
//!
//! The scenario is swept under two tear models: at the kernel's program
//! calls over a permissive RAM device, and at the device's own program
//! boundaries with the injector inside `NorAlignedStorage` over a
//! strict NOR device, where the interrupted page may also land half
//! programmed (review coverage item V4, bead `lfs-hki`). The second
//! model is the finer one, since the alignment adapter splits each
//! commit span the kernel programs in one call into `PROG_SIZE`
//! windows; on this geometry that is roughly three device programs per
//! kernel program call for this scenario. Both counts are measured at
//! run time by the respective `*_call_counts` helper, so neither sweep
//! carries a frozen tally.

use littlefs2_pure::meta::MetadataReader;
use littlefs2_pure::storage::Storage;
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

/// `mkdir /a; mkdir /b; rmdir /a`. Generic over the storage so the same
/// sequence runs under both tear models.
fn scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = buf();
    let mut b = buf();
    let _ = fs.mkdir(Path::new("/a").unwrap(), &mut a, &mut b);
    let _ = fs.mkdir(Path::new("/b").unwrap(), &mut a, &mut b);
    let _ = fs.rmdir(Path::new("/a").unwrap(), &mut a, &mut b);
}

#[test]
fn torn_rmdir_unthread_is_deorphaned_on_remount() {
    let scenario = scenario::<TornWriteStorage>;
    let (fmt_calls, scenario_calls) = common::torn_call_counts(scenario);
    assert!(scenario_calls > 0);

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

        // Remount: deorphan recovery runs here. Strong semantics
        // (review H7): the image held a valid filesystem before the
        // tear, so it MUST mount; the pre-H7 sweep silently
        // `continue`d past unmountable images.
        let mut fs =
            common::mount_image_strict(image, &format!("rmdir unthread sweep trigger {trigger}"));
        let mut a = buf();
        let mut b = buf();
        // Load-bearing existence checks (review M5: the pre-fix sweep
        // discarded this result with `let _`). Both queries must
        // answer cleanly; any (a, b) combination is a legitimate
        // prefix state of mkdir/mkdir/rmdir, so the assertions are
        // that the answers are coherent and stable, and that the
        // thread holds no orphan.
        let a_exists = fs.exists(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
        let b_exists = fs.exists(Path::new("/b").unwrap(), &mut a, &mut b).unwrap();
        let data = fs.into_storage();
        assert_no_thread_orphan(&data.data);

        // Recovery is idempotent: a second mount answers the same.
        let mut fs = common::mount_image_strict(
            data.data,
            &format!("rmdir unthread sweep trigger {trigger}, second remount"),
        );
        let a2 = fs.exists(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
        let b2 = fs.exists(Path::new("/b").unwrap(), &mut a, &mut b).unwrap();
        assert_eq!(
            (a_exists, b_exists),
            (a2, b2),
            "trigger {trigger}: directory state must be stable across remounts"
        );
    }
}

/// The rmdir unthread sweep at DEVICE program granularity, with
/// partial window landings (review coverage item V4, bead `lfs-hki`).
///
/// Same invariants as the kernel boundary sweep above: the image must
/// mount, both existence queries must answer cleanly and identically on
/// a second consecutive mount, and the tail thread must reach no pair
/// that is not a live tree directory. What changes is where the power
/// cut lands: inside one of the two real page programs that carry the
/// entry delete and the predecessor's tail clear, and possibly with
/// that page left half programmed.
///
/// Landing lengths come from `common::NOR_PARTIAL_LANDINGS`; that
/// constant documents the sampling bound.
#[test]
fn torn_rmdir_unthread_is_deorphaned_across_every_nor_program_landing() {
    let scenario = scenario::<common::NorTornStorage>;
    let (fmt_calls, scenario_calls) = common::nor_torn_call_counts(scenario);
    assert!(scenario_calls > 0);

    let mut witness = common::PartialLandingWitness::new();
    for partial in common::NOR_PARTIAL_LANDINGS {
        for trigger in 1..=fmt_calls + scenario_calls + 2 {
            let ctx = format!("nor rmdir unthread sweep trigger {trigger}, partial {partial}");
            let image = match common::run_nor_torn_scenario(trigger, partial, scenario) {
                common::TornRun::TornFormat => {
                    assert!(
                        trigger <= fmt_calls,
                        "{ctx}: format reported torn past its own {fmt_calls} device programs"
                    );
                    continue;
                }
                common::TornRun::Image(image) => image,
            };
            witness.observe(partial, trigger, &image);

            let mut fs = common::mount_nor_image_strict(image, &ctx);
            let mut a = buf();
            let mut b = buf();
            let a_exists = fs.exists(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
            let b_exists = fs.exists(Path::new("/b").unwrap(), &mut a, &mut b).unwrap();
            let recovered = common::nor_image_of(fs);
            assert_no_thread_orphan(&recovered);

            let mut fs =
                common::mount_nor_image_strict(recovered, &format!("{ctx}, second remount"));
            let a2 = fs.exists(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
            let b2 = fs.exists(Path::new("/b").unwrap(), &mut a, &mut b).unwrap();
            assert_eq!(
                (a_exists, b_exists),
                (a2, b2),
                "{ctx}: directory state must be stable across remounts"
            );
        }
    }
    witness.assert_partials_landed("nor rmdir unthread sweep");
}
