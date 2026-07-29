//! The third test geometry: 4096 byte blocks with a 256 byte read and
//! program window (review coverage item V5, bead `lfs-4s3`).
//!
//! This is the shape of a real SPI NOR part: a 4 KiB erase block and a
//! 256 byte page. `tests/geometry2.rs` carries the broad second geometry
//! band at 512/32; this file covers only what 4 KiB blocks say that a
//! doubling of 256 cannot, and stops there, because every commit buffer
//! here is sixteen times the default suite's and whole crash sweeps at
//! that size buy little for the runtime they cost.
//!
//! What 4096/256 reaches that neither 256/16 nor 512/32 does:
//!
//! - A block far larger than the inline threshold (a fixed 128 bytes, not
//!   a geometry derived one), so the inline and CTZ split sits at a very
//!   different fraction of a block.
//! - A CTZ chain whose per block skip pointer header is a negligible
//!   fraction of the block, where at 256 it is a sixteenth of it.
//! - A program window wide enough that a whole small commit lands in one
//!   device program, which is the opposite regime from the default
//!   geometry's several windows per commit.
//! - A split budget of 2048 bytes, so a pair holds many more entries
//!   before it overflows.
//!
//! The crash sweeps at this geometry are deliberately limited to the
//! cheapest one (an inline write, kernel program granularity). The wide
//! sweeps live at 512/32 and at the default geometry.

use littlefs2_pure::ctz::{block_count, content_bytes_in_block};
use littlefs2_pure::{Fs, NorAlignedStorage, Path, Storage};

mod common;
use common::{MemStorage4K, StrictNorStorageG, TornWriteStorage};

/// The strict NOR double at the third geometry. Only this file needs it,
/// so it stays here rather than in the shared helpers.
type StrictNor4K = StrictNorStorageG<4096, 256, 16>;

fn buf() -> [u8; MemStorage4K::BLOCK_SIZE] {
    [0u8; MemStorage4K::BLOCK_SIZE]
}

fn make_fs() -> Fs<MemStorage4K> {
    let mut storage = MemStorage4K::new();
    let mut scratch = buf();
    Fs::format(&mut storage, &mut scratch).expect("format at 4096/256 must succeed");
    let mut buf_a = buf();
    let mut buf_b = buf();
    Fs::mount(storage, &mut buf_a, &mut buf_b).expect("mount at 4096/256 must succeed")
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + i / 251) & 0xFF) as u8).collect()
}

#[test]
fn format_records_the_third_geometry() {
    let fs = make_fs();
    assert_eq!(fs.superblock().block_size, 4096);
    assert_eq!(fs.superblock().block_count, MemStorage4K::BLOCK_COUNT);
    assert_eq!(MemStorage4K::PROG_SIZE, 256);
    assert_eq!(MemStorage4K::READ_SIZE, 256);
}

#[test]
fn content_across_the_4k_chain_boundaries_roundtrips() {
    let bs = MemStorage4K::BLOCK_SIZE as u32;
    // One block holds the whole 4096; the second spends 4 bytes on its
    // single skip pointer, so two blocks hold 8188.
    assert_eq!(content_bytes_in_block(0, bs), 4096);
    assert_eq!(content_bytes_in_block(1, bs), 4092);
    assert_eq!(block_count(4096, bs), 1);
    assert_eq!(block_count(4097, bs), 2);
    assert_eq!(block_count(8188, bs), 2);
    assert_eq!(block_count(8189, bs), 3);

    for n in [0usize, 1, 128, 129, 255, 256, 257, 4095, 4096, 4097, 8187, 8188, 8189, 12000] {
        let content = pattern(n);
        let mut fs = make_fs();
        let mut a = buf();
        let mut b = buf();
        fs.write_to_path(Path::new("/f").unwrap(), &content, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("writing {n} bytes at 4096/256 failed: {e:?}"));

        let storage = fs.into_storage();
        let mut buf_a = buf();
        let mut buf_b = buf();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = buf();
        let mut b = buf();
        assert_eq!(
            fs.size_of(Path::new("/f").unwrap(), &mut a, &mut b).unwrap() as usize,
            n,
            "size of a {n} byte file after remount"
        );
        let mut out = vec![0u8; n];
        fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(out, content, "roundtrip of {n} bytes at 4096/256");
    }
}

#[test]
fn reads_off_the_256_byte_grid_are_widened_correctly() {
    let content = pattern(10000);
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(Path::new("/big").unwrap(), &content, &mut a, &mut b).unwrap();

    // Offsets on and around the window grid (256) and the chain block
    // boundaries (4096, then 8188), with lengths that are never a
    // multiple of the window.
    for off in [0u32, 1, 255, 256, 257, 4095, 4096, 4097, 8187, 8188, 8189, 9999] {
        for len in [1usize, 3, 255, 257, 1000] {
            let end = (off as usize + len).min(content.len());
            if end <= off as usize {
                continue;
            }
            let want = &content[off as usize..end];
            let mut out = vec![0u8; want.len()];
            let n = fs
                .read_at_path(Path::new("/big").unwrap(), off, &mut out, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("read at offset {off} length {len} failed: {e:?}"));
            assert_eq!(n, want.len());
            assert_eq!(out, want, "wrong bytes at offset {off} length {len}");
        }
    }
}

#[test]
fn a_directory_splits_at_a_2048_byte_budget() {
    // The split budget here is `min(4096 - 40, 2048) = 2048`, so a pair
    // holds far more than at the smaller geometries. Long names make each
    // entry cost about 77 wire bytes (Create, a 64 byte NAME, a one byte
    // inline STRUCT), so 40 entries is comfortably past 2048 and forces a
    // split without writing hundreds of entries.
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let target = 40usize;
    let long = "n".repeat(59);
    for i in 0..target {
        let name = format!("/d/{long}{i:03}");
        fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} must fit once the pair splits: {e:?}"));
    }

    // The chain really did grow past one pair.
    let pair = {
        let resolved = fs.resolve(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
        let body = resolved.struct_body;
        littlefs2_pure::BlockPair::new(
            littlefs2_pure::BlockAddress::new(u32::from_le_bytes([
                body[0], body[1], body[2], body[3],
            ])),
            littlefs2_pure::BlockAddress::new(u32::from_le_bytes([
                body[4], body[5], body[6], body[7],
            ])),
        )
    };
    let view = fs.read_pair(pair, &mut a, &mut b).unwrap();
    assert!(
        view.reader.is_hard_tail(),
        "{target} long named entries must overflow one 4096 byte pair"
    );

    let storage = fs.into_storage();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, target, "every entry must enumerate across the split after a remount");
    for i in 0..target {
        let name = format!("/d/{long}{i:03}");
        let mut out = [0u8; 1];
        fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(&out, b"v");
    }
}

#[test]
fn strict_nor_programs_stay_window_aligned_at_4k() {
    // `StrictNor4K` panics on any program that is not a whole aligned 256
    // byte window or that tries to set a bit back to one. At this window
    // size a small commit is one device program, the opposite regime from
    // the default geometry.
    let mut storage = NorAlignedStorage::new(StrictNor4K::new())
        .expect("4096/256 satisfies the alignment adapter's invariants");
    let mut scratch = [0u8; 4096];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; 4096];
    let mut buf_b = [0u8; 4096];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; 4096];
    let mut b = [0u8; 4096];

    fs.write_to_path(Path::new("/inline").unwrap(), b"tiny", &mut a, &mut b).unwrap();
    let big = pattern(9000);
    fs.write_to_path(Path::new("/big").unwrap(), &big, &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d/e").unwrap(), b"x", &mut a, &mut b).unwrap();
    fs.sync().unwrap();

    let image = common::nor_image_of_on::<StrictNor4K>(fs);
    let mut fs =
        common::mount_nor_image_strict_on::<StrictNor4K>(image, "clean 4096/256 NOR shutdown");
    let mut a = [0u8; 4096];
    let mut b = [0u8; 4096];
    let mut out = vec![0u8; big.len()];
    fs.read_at_path(Path::new("/big").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(out, big);
    let mut small = [0u8; 4];
    fs.read_at_path(Path::new("/inline").unwrap(), 0, &mut small, &mut a, &mut b).unwrap();
    assert_eq!(&small, b"tiny");
}

/// The one crash sweep at this geometry: an inline write, torn at every
/// kernel program boundary. Wide sweeps stay at the smaller geometries;
/// this pins that the commit and recovery paths are not somehow specific
/// to a block size where a commit spans several program windows.
#[test]
fn inline_write_is_atomic_at_4k_across_every_kernel_program_boundary() {
    fn scenario<S: Storage>(fs: &mut Fs<S>) {
        let mut a = vec![0u8; S::BLOCK_SIZE];
        let mut b = vec![0u8; S::BLOCK_SIZE];
        let _ = fs.write_to_path(Path::new("/log").unwrap(), b"ONE", &mut a, &mut b);
    }
    let scenario = scenario::<TornWriteStorage<MemStorage4K>>;
    let (fmt_calls, scenario_calls) = common::torn_call_counts_on::<MemStorage4K, _>(scenario);
    assert!(scenario_calls > 0);

    for trigger in 1..=fmt_calls + scenario_calls + 3 {
        match common::run_torn_scenario_on::<MemStorage4K, _>(trigger, scenario) {
            common::TornRun::TornFormat => assert!(
                trigger <= fmt_calls,
                "trigger {trigger}: format reported torn past its own {fmt_calls} calls"
            ),
            common::TornRun::Image(image) => {
                let mut fs = common::mount_image_strict_on::<MemStorage4K>(
                    image,
                    &format!("4096/256 inline sweep trigger {trigger}"),
                );
                let mut a = buf();
                let mut b = buf();
                let p = Path::new("/log").unwrap();
                let content = if fs.exists(p, &mut a, &mut b).unwrap() {
                    let size = fs.size_of(p, &mut a, &mut b).unwrap();
                    let mut out = vec![0u8; size as usize];
                    fs.read_at_path(p, 0, &mut out, &mut a, &mut b).unwrap();
                    out
                } else {
                    Vec::new()
                };
                assert!(
                    content.is_empty() || content == b"ONE",
                    "trigger {trigger}: content {content:?} is neither the pre state \
                     nor the post state"
                );
            }
        }
    }
}
