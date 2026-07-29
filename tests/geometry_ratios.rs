//! Geometries with a different `BLOCK_SIZE / PROG_SIZE` ratio (review
//! coverage item V5, bead `lfs-4s3`).
//!
//! `tests/geometry2.rs` (512/32) and `tests/geometry_4k.rs` (4096/256)
//! break the default geometry's absolute sizes, but all three share a
//! ratio of sixteen program windows per block, because each is the
//! default scaled on both axes at once. Several kernel quantities depend
//! on that ratio rather than on either size alone: how many device
//! programs the alignment adapter issues per commit, how much of a block
//! one window covers when a partial landing cuts a commit, and the
//! rounding in `(BLOCK_SIZE / 2).next_multiple_of(PROG_SIZE)` that sets
//! the split budget. A bug that cancels at a ratio of sixteen would
//! survive all three.
//!
//! This file runs one generic exercise at ratios of 4, 8, and 32, which
//! only reads as economical because the doubles in `tests/common/mod.rs`
//! are generic over geometry: each geometry costs a type alias and one
//! line in the table below, not a copy of the suite.
//!
//! The exercise stays deliberately narrow, since the deep scenarios live
//! in the two files above: format and mount, content across the chain
//! block boundaries this geometry implies, reads off the window grid, a
//! directory split at the budget this geometry implies, and (for the
//! strict NOR arm) that every program the alignment adapter issues is a
//! whole legal window.

use littlefs2_pure::ctz::{block_count, content_bytes_in_block};
use littlefs2_pure::{Fs, NorAlignedStorage, Path, Storage};

mod common;
use common::{MemStorageG, StrictNorStorageG, TestDevice};

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + i / 251) & 0xFF) as u8).collect()
}

/// Cumulative content capacity of the first `n` CTZ chain blocks, from
/// the per block capacities the geometry implies. The sizes the exercise
/// probes are derived from this rather than written down, so the same body
/// is correct at every geometry.
fn chain_capacity(block_size: u32, n: u32) -> u32 {
    (0..n).map(|i| content_bytes_in_block(i, block_size)).sum()
}

/// The split budget the kernel computes for this geometry:
/// `min(BLOCK_SIZE - SPLIT_RESERVE, (BLOCK_SIZE / 2)` rounded up to the
/// program window`)`, where `SPLIT_RESERVE` is 40 bytes of tail, gstate,
/// move delete, and CCRC.
fn split_budget<D: Storage>() -> usize {
    core::cmp::min(D::BLOCK_SIZE - 40, (D::BLOCK_SIZE / 2).next_multiple_of(D::PROG_SIZE))
}

/// True when `/d`'s first metadata pair carries a HardTail, meaning the
/// directory has grown past one pair.
fn first_pair_is_split<S: Storage>(fs: &mut Fs<S>, a: &mut [u8], b: &mut [u8]) -> bool {
    let pair = {
        let resolved = fs.resolve(Path::new("/d").unwrap(), a, b).unwrap();
        let body = resolved.struct_body;
        assert_eq!(body.len(), 8, "/d must resolve to a directory");
        littlefs2_pure::BlockPair::new(
            littlefs2_pure::BlockAddress::new(u32::from_le_bytes([
                body[0], body[1], body[2], body[3],
            ])),
            littlefs2_pure::BlockAddress::new(u32::from_le_bytes([
                body[4], body[5], body[6], body[7],
            ])),
        )
    };
    fs.read_pair(pair, a, b).unwrap().reader.is_hard_tail()
}

/// Format, mount, and exercise the geometry sensitive paths on `D`.
///
/// `label` names the geometry in every failure message, so a red run says
/// which ratio broke rather than only which assertion did.
fn exercise<D: TestDevice>(label: &str) {
    let bs = D::BLOCK_SIZE as u32;
    let mut storage = D::fresh();
    let mut scratch = vec![0u8; D::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap_or_else(|e| panic!("{label}: format: {e:?}"));
    let mut buf_a = vec![0u8; D::BLOCK_SIZE];
    let mut buf_b = vec![0u8; D::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{label}: mount: {e:?}"));
    assert_eq!(fs.superblock().block_size, bs, "{label}: superblock block size");
    assert_eq!(fs.superblock().block_count, D::BLOCK_COUNT, "{label}: superblock block count");

    let mut a = vec![0u8; D::BLOCK_SIZE];
    let mut b = vec![0u8; D::BLOCK_SIZE];

    // Content sizes: one byte, the inline threshold either side, and each
    // of the first three chain capacities either side. All derived, so a
    // geometry with 1024 byte blocks probes 1024 and 2044 rather than the
    // numbers some other geometry happens to care about.
    let mut sizes = vec![1usize, 128, 129];
    for n in 1..=3u32 {
        let cap = chain_capacity(bs, n) as usize;
        sizes.push(cap - 1);
        sizes.push(cap);
        sizes.push(cap + 1);
    }
    for (i, n) in sizes.iter().copied().enumerate() {
        let content = pattern(n);
        let name = format!("/f{i}");
        let p = Path::new(&name).unwrap();
        fs.write_to_path(p, &content, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("{label}: writing {n} bytes: {e:?}"));
        assert_eq!(
            fs.size_of(p, &mut a, &mut b).unwrap() as usize,
            n,
            "{label}: size of a {n} byte file"
        );
        let mut out = vec![0u8; n];
        fs.read_at_path(p, 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(out, content, "{label}: content of a {n} byte file");
        // Each of these fits in one metadata pair's worth of entries only
        // because they are removed again; the point of the loop is the
        // chain arithmetic, not directory capacity.
        fs.remove_at_path(p, &mut a, &mut b).unwrap();
    }

    // A three block file read at offsets and lengths that are never a
    // multiple of the window, including across every block boundary.
    let three = chain_capacity(bs, 3) as usize;
    assert_eq!(block_count(three as u32, bs), 3, "{label}: three block capacity");
    let content = pattern(three);
    let p = Path::new("/big").unwrap();
    fs.write_to_path(p, &content, &mut a, &mut b).unwrap();
    let boundaries = [
        0u32,
        1,
        D::PROG_SIZE as u32 - 1,
        D::PROG_SIZE as u32,
        chain_capacity(bs, 1) - 1,
        chain_capacity(bs, 1),
        chain_capacity(bs, 1) + 1,
        chain_capacity(bs, 2) - 1,
        chain_capacity(bs, 2),
        chain_capacity(bs, 2) + 1,
    ];
    for off in boundaries {
        for len in [1usize, 3, D::READ_SIZE - 1, D::READ_SIZE + 1, D::BLOCK_SIZE / 3] {
            let end = (off as usize + len).min(three);
            if end <= off as usize {
                continue;
            }
            let want = &content[off as usize..end];
            let mut out = vec![0u8; want.len()];
            fs.read_at_path(p, off, &mut out, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("{label}: read at {off} length {len}: {e:?}"));
            assert_eq!(out, want, "{label}: bytes at offset {off} length {len}");
        }
    }
    fs.remove_at_path(p, &mut a, &mut b).unwrap();

    // A directory grown until it splits. The entry count that triggers a
    // split is not `budget / entry_size`: a split happens only when a
    // COMPACTION runs and finds the live set past the budget, and a
    // compaction runs only when the append log fills, where each commit is
    // padded to a whole program window. At 2048/64 that schedule leaves a
    // window of entry counts whose compacted size already exceeds the
    // budget while no compaction has yet been due, so a fixed count is the
    // wrong criterion. Grow until the first pair carries a HardTail
    // instead, capped generously, and report the count the geometry
    // actually needed rather than freezing a tally.
    let cap = 4 * split_budget::<D>() / 16 + 16;
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let mut entries = 0usize;
    let mut split_at: Option<usize> = None;
    while entries < cap {
        let name = format!("/d/f{entries:03}");
        fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("{label}: entry {entries}: {e:?}"));
        entries += 1;
        if split_at.is_none() && first_pair_is_split(&mut fs, &mut a, &mut b) {
            split_at = Some(entries);
        }
        // Keep writing a little past the split so the continuation is
        // itself written to and later enumerated across.
        if split_at.is_some_and(|at| entries >= at + 3) {
            break;
        }
    }
    let split_at = split_at.unwrap_or_else(|| {
        panic!(
            "{label}: {entries} single byte entries did not overflow one pair at a split \
             budget of {}",
            split_budget::<D>()
        )
    });
    println!("{label}: the directory split after {split_at} entries, grown to {entries}");

    // Everything survives a remount that reads the tree from disk again.
    let storage = fs.into_storage();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{label}: remount: {e:?}"));
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, entries, "{label}: every entry must enumerate after the split");
    assert!(split_at <= entries, "{label}: the split must have happened within the run");
    for i in 0..entries {
        let name = format!("/d/f{i:03}");
        let mut out = [0u8; 1];
        fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("{label}: entry {i} after remount: {e:?}"));
        assert_eq!(&out, b"v", "{label}: entry {i} content");
    }
}

#[test]
fn ratio_four_1024_over_256() {
    // Four windows per block: a small commit is most of one window, and
    // the split budget is exactly half a block with no rounding.
    assert_eq!(split_budget::<MemStorageG<1024, 256, 32>>(), 512);
    exercise::<MemStorageG<1024, 256, 32>>("1024/256 (ratio 4)");
}

#[test]
fn ratio_eight_512_over_64() {
    assert_eq!(split_budget::<MemStorageG<512, 64, 64>>(), 256);
    exercise::<MemStorageG<512, 64, 64>>("512/64 (ratio 8)");
}

#[test]
fn ratio_thirtytwo_512_over_16() {
    // Same block size as the second geometry, a window half the default's:
    // twice as many device programs per commit as 512/32.
    assert_eq!(split_budget::<MemStorageG<512, 16, 64>>(), 256);
    exercise::<MemStorageG<512, 16, 64>>("512/16 (ratio 32)");
}

#[test]
fn ratio_thirtytwo_2048_over_64() {
    assert_eq!(split_budget::<MemStorageG<2048, 64, 32>>(), 1024);
    exercise::<MemStorageG<2048, 64, 32>>("2048/64 (ratio 32)");
}

/// The strict NOR arm at two ratios: every program the kernel issues
/// through the alignment adapter must be one whole aligned window that
/// only clears bits, which the double asserts on each call. Ratio 4 is
/// the case where a small commit is a single device program, the opposite
/// regime from the default geometry's several.
#[test]
fn strict_nor_windows_hold_at_ratios_four_and_thirtytwo() {
    fn nor_exercise<D: TestDevice>(label: &str) {
        let mut storage = NorAlignedStorage::new(D::fresh())
            .unwrap_or_else(|| panic!("{label}: the geometry must satisfy the adapter"));
        let mut scratch = vec![0u8; D::BLOCK_SIZE];
        Fs::format(&mut storage, &mut scratch).unwrap();
        let mut buf_a = vec![0u8; D::BLOCK_SIZE];
        let mut buf_b = vec![0u8; D::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = vec![0u8; D::BLOCK_SIZE];
        let mut b = vec![0u8; D::BLOCK_SIZE];

        let big = pattern(chain_capacity(D::BLOCK_SIZE as u32, 3) as usize - 7);
        fs.write_to_path(Path::new("/big").unwrap(), &big, &mut a, &mut b).unwrap();
        fs.write_to_path(Path::new("/small").unwrap(), b"tiny", &mut a, &mut b).unwrap();
        fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
        for i in 0..10 {
            let name = format!("/d/e{i:02}");
            fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b).unwrap();
        }
        fs.sync().unwrap();

        let image = common::nor_image_of_on::<D>(fs);
        let mut fs = common::mount_nor_image_strict_on::<D>(image, label);
        let mut a = vec![0u8; D::BLOCK_SIZE];
        let mut b = vec![0u8; D::BLOCK_SIZE];
        let mut out = vec![0u8; big.len()];
        fs.read_at_path(Path::new("/big").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(out, big, "{label}: CTZ content after a clean NOR shutdown");
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
        assert_eq!(seen, 10, "{label}: directory after a clean NOR shutdown");
    }

    nor_exercise::<StrictNorStorageG<1024, 256, 32>>("strict NOR 1024/256 (ratio 4)");
    nor_exercise::<StrictNorStorageG<512, 16, 64>>("strict NOR 512/16 (ratio 32)");
}
