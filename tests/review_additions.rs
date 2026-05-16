//! Regression tests suggested by the post-v1.0 review's
//! test-additions list.
//!
//! - #5 is reworked: `Fs::format` is not byte-identical to the C
//!   reference (see `docs/decisions/0008-format-bootstrap-divergence.md`),
//!   so this pins the semantic invariant it can honestly assert.
//! - #6 pins the revision-wraparound *selection predicate*
//!   (`meta::rev_scmp`) at the u32 wrap from the public surface. The
//!   wrap arithmetic is already discharged by a Kani harness; this is
//!   a fast integration-level guard against the predicate being
//!   rewired the wrong way, which a math proof on the helper alone
//!   would not catch.
//! - #8 pins that the directory entry count is internally consistent:
//!   the count `live_entries` reports survives a compaction round trip
//!   (the compactor walks the same pair through `gather_live_slots`),
//!   exercised end to end through the public API.
//!
//! #6's full mount-orchestration variant and #10 (cross-dir rename
//! under a concurrent torn relocation) are intentionally not added
//! here: the wrap math is Kani-discharged and a faithful mount test
//! needs CRC-correct multi-revision pair fixtures disproportionate to
//! a hardening patch, and #10 is covered in aggregate by
//! `tests/atomic_move.rs` (torn cross-dir rename recovered at every
//! torn point) plus `tests/wear_leveling.rs` (relocation propagation
//! under torn writes). See the v1.0.1 commit body for the rationale.

use littlefs2_pure::meta::rev_scmp;
use littlefs2_pure::{Fs, Path};

mod common;
use common::MemStorage;

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    Fs::mount(storage, &mut a, &mut b).unwrap()
}

/// #5: `Fs::format` produces an image that mounts as a clean empty
/// filesystem with the expected geometry and is stable across a
/// remount. It deliberately does not assert byte equality with the C
/// reference vector; that divergence is accepted and recorded in
/// ADR-0008. Conformance (`vector_01`) and roundtrip cover interop.
#[test]
fn format_produces_a_clean_mountable_empty_filesystem() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    let sb = fs.superblock();
    assert_eq!(sb.block_size, MemStorage::BLOCK_SIZE as u32);
    assert_eq!(sb.block_count, MemStorage::BLOCK_COUNT);
    assert_eq!(sb.version, littlefs2_pure::DISK_VERSION);

    let mut count = 0usize;
    fs.list_root(|_| count += 1, &mut a, &mut b).unwrap();
    assert_eq!(count, 0, "a freshly formatted root has no user entries");

    // Stable across a remount: format then mount then remount sees the
    // same empty filesystem (no spurious recovery, no corruption).
    let storage = fs.into_storage();
    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let mut fs2 = Fs::mount(storage, &mut a2, &mut b2).unwrap();
    let mut count2 = 0usize;
    fs2.list_root(|_| count2 += 1, &mut a2, &mut b2).unwrap();
    assert_eq!(count2, 0);
    assert_eq!(fs2.superblock().block_count, MemStorage::BLOCK_COUNT);
}

/// #6: the active block of a metadata pair is the one with the higher
/// revision under the wrap-aware signed comparison, not the naive
/// `u32` `>`. `rev_scmp(a, b) > 0` iff `a` is newer than `b`. Pin the
/// behaviour exactly across the `0xFFFF_FFFF -> 0` wrap, where naive
/// comparison inverts.
#[test]
fn revision_compare_selects_post_wrap_as_newer() {
    // Naive: 0 < 0xFFFF_FFFF, so a naive picker keeps the pre-wrap
    // block forever. Wrap-aware: 0 is one step newer than
    // 0xFFFF_FFFF.
    assert!(rev_scmp(0x0000_0000, 0xFFFF_FFFF) > 0, "post-wrap 0 must be newer than pre-wrap max");
    assert!(rev_scmp(0xFFFF_FFFF, 0x0000_0000) < 0, "pre-wrap max must be older than post-wrap 0");

    // Adjacent values across the wrap and in the normal range.
    assert!(rev_scmp(0xFFFF_FFFF, 0xFFFF_FFFE) > 0);
    assert!(rev_scmp(0x0000_0001, 0xFFFF_FFFF) > 0, "two steps past the wrap is still newer");
    assert!(rev_scmp(2, 1) > 0);
    assert!(rev_scmp(1, 2) < 0);
    assert_eq!(rev_scmp(42, 42), 0, "equal revisions compare equal");

    // Half-range is the boundary of the signed interpretation; it is
    // its own antisymmetric pair (a - b == i32::MIN both ways).
    let half = 0x8000_0000u32;
    assert!(rev_scmp(half, 0) != 0);
}

/// #8: the live directory-entry count is internally consistent across
/// the compaction path. `list_root`/`list_dir` count via
/// `live_entries`; a compaction walks the same pair through
/// `gather_live_slots` and rewrites it. If the two disagreed on which
/// ids are live, the count would change across a compaction-inducing
/// operation. Build a pair with creates, deletes, and a rename (id
/// renumbering), force compactions, and assert the count the public
/// surface reports never drifts from the independent recount.
#[test]
fn live_entry_count_is_stable_across_compaction() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // Create several entries, delete some, rename one: this exercises
    // splice renumbering, the exact path where gather_live_slots and
    // live_entries must agree on the live id set.
    for i in 0..6 {
        let name = format!("/f{i}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b).unwrap();
    }
    fs.remove_at_path(Path::new("/f1").unwrap(), &mut a, &mut b).unwrap();
    fs.remove_at_path(Path::new("/f4").unwrap(), &mut a, &mut b).unwrap();
    fs.rename(Path::new("/f2").unwrap(), Path::new("/f2r").unwrap(), &mut a, &mut b).unwrap();

    let recount = |fs: &mut Fs<MemStorage>, a: &mut [u8], b: &mut [u8]| {
        let mut n = 0usize;
        fs.list_root(|_| n += 1, a, b).unwrap();
        n
    };

    let expected: Vec<&str> = vec!["/f0", "/f2r", "/f3", "/f5"];
    let baseline = recount(&mut fs, &mut a, &mut b);
    assert_eq!(baseline, expected.len(), "live set after create/delete/rename");
    for p in &expected {
        assert!(fs.exists(Path::new(p).unwrap(), &mut a, &mut b).unwrap(), "{p} must be live");
    }

    // Force many compactions: repeatedly rewrite an entry's content so
    // the pair fills and compacts, each compaction routing the live
    // set through gather_live_slots. The count must never drift.
    for round in 0..40 {
        let body = [u8::try_from(round & 0xff).unwrap(); 3];
        fs.write_to_path(Path::new("/f0").unwrap(), &body, &mut a, &mut b).unwrap();
        assert_eq!(
            recount(&mut fs, &mut a, &mut b),
            baseline,
            "compaction round {round} changed the live entry count; \
             gather_live_slots and live_entries disagreed on the live id set",
        );
    }

    // Survives a remount (the compacted pair re-parses to the same set).
    let storage = fs.into_storage();
    let mut a2 = common::make_buffer();
    let mut b2 = common::make_buffer();
    let mut fs2 = Fs::mount(storage, &mut a2, &mut b2).unwrap();
    let mut after = 0usize;
    fs2.list_root(|_| after += 1, &mut a2, &mut b2).unwrap();
    assert_eq!(after, baseline);
}
