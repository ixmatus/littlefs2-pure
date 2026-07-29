//! The second test geometry: 512 byte blocks, 32 byte read and program
//! windows (review coverage item V5, bead `lfs-4s3`).
//!
//! Nearly every integration suite in this crate runs one geometry, 256
//! byte blocks with a 16 byte read and program granularity. That is a
//! monoculture, and a monoculture hides a specific class of kernel bug:
//! arithmetic that happens to cancel at one geometry. Split points round
//! to `(BLOCK_SIZE / 2).next_multiple_of(PROG_SIZE)`, a CTZ chain block
//! spends `4 * (ctz(i) + 1)` bytes of its `BLOCK_SIZE` on skip pointers,
//! the alignment adapter buffers one `PROG_SIZE` window at a time, and
//! every read the kernel issues has to land on the `READ_SIZE` grid. An
//! off by one in any of those can be invisible at 256/16 and fatal at
//! 512/32.
//!
//! This file drives the scenarios whose logic is geometry sensitive at
//! 512/32: file content across the inline and CTZ boundary, CTZ reads at
//! offsets that straddle both block and window boundaries, the file
//! handle append and truncate paths, directory growth through a split, a
//! multi cut split with its byte accounting derived again for 512 byte
//! blocks, read alignment on a 32 byte grid, strict NOR programs through
//! the alignment adapter, and both torn write sweeps (kernel program
//! granularity, and device granularity with partial window landings).
//!
//! What deliberately stays at one geometry, so a later reader does not
//! read the omission as an oversight:
//!
//! - The conformance and roundtrip vectors. They are images the C
//!   reference produced at a pinned geometry, and the C verifier in
//!   `tools/verify_image` is compiled for it. A second geometry there
//!   means regenerating artifacts, not adding a test.
//! - `tests/review_l1_split_recheck.rs` and the other byte tuned
//!   reproducers. Their sizes are chosen against 256 byte arithmetic and
//!   documented as such; forcing them through a second geometry would
//!   retire the coverage they exist for. The multi cut scenario worth
//!   having at 512 is derived again here instead, with its own arithmetic.
//! - Pinned vector tables and the property suites, which are geometry
//!   independent already (they test encode and decode, not layout).
//!
//! Every device double here comes from `tests/common/mod.rs`, which is
//! generic over geometry: `MemStorage512` and `StrictNorStorage512` are
//! the 512/32 aliases, and the torn write harness helpers take the device
//! type as a parameter.

use littlefs2_pure::ctz::{block_count, content_bytes_in_block, skip_pointers_in_block};
use littlefs2_pure::{Fs, NorAlignedStorage, OpenOptions, Path, SeekFrom, Storage};

mod common;
use common::{MemStorage512, StrictNorStorage512, TornWriteStorage};

/// One block sized scratch buffer at the second geometry. Every `Fs` call
/// takes two of them.
fn buf() -> [u8; MemStorage512::BLOCK_SIZE] {
    [0u8; MemStorage512::BLOCK_SIZE]
}

/// A formatted and mounted filesystem at the second geometry.
fn make_fs() -> Fs<MemStorage512> {
    let mut storage = MemStorage512::new();
    let mut scratch = buf();
    Fs::format(&mut storage, &mut scratch).expect("format at 512/32 must succeed");
    let mut buf_a = buf();
    let mut buf_b = buf();
    Fs::mount(storage, &mut buf_a, &mut buf_b).expect("mount at 512/32 must succeed")
}

/// Deterministic pseudo random content of `n` bytes. A byte pattern with
/// a period longer than the block size, so a block sized copy landing at
/// the wrong offset shows up as a mismatch rather than as identical
/// bytes.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + i / 251) & 0xFF) as u8).collect()
}

// ---------------------------------------------------------------------
// The geometry itself
// ---------------------------------------------------------------------

#[test]
fn format_records_the_second_geometry_in_the_superblock() {
    let fs = make_fs();
    let sb = fs.superblock();
    assert_eq!(sb.block_size, MemStorage512::BLOCK_SIZE as u32);
    assert_eq!(sb.block_count, MemStorage512::BLOCK_COUNT);
    // The mount already enforced agreement between the on disk geometry
    // and the device's, so a superblock echoing 256 here would have
    // failed the mount. Asserting it anyway pins that this suite is
    // actually running the geometry it claims.
    assert_eq!(MemStorage512::BLOCK_SIZE, 512);
    assert_eq!(MemStorage512::PROG_SIZE, 32);
    assert_eq!(MemStorage512::READ_SIZE, 32);
}

/// The CTZ block capacity table at 512, spelled out. A chain block at
/// index `i` spends `4 * skip_pointers_in_block(i)` bytes on its skip
/// pointer header, where the count is `ctz(i) + 1` for every index but
/// the chain's first (index 0 has no predecessor and so no pointers), and
/// what remains is content. At 256 the same table reads 256, 252, 248,
/// 252, 244; writing the 512 numbers down here means the sizes the
/// roundtrip test picks are chosen against the geometry rather than
/// copied from the 256 byte suite.
#[test]
fn ctz_block_capacities_at_512() {
    let bs = MemStorage512::BLOCK_SIZE as u32;
    let expected: [(u32, u32, u32); 9] = [
        // (index, skip pointers, content bytes)
        (0, 0, 512),
        (1, 1, 508),
        (2, 2, 504),
        (3, 1, 508),
        (4, 3, 500),
        (5, 1, 508),
        (6, 2, 504),
        (7, 1, 508),
        (8, 4, 496),
    ];
    for (i, ptrs, content) in expected {
        assert_eq!(skip_pointers_in_block(i), ptrs, "pointer count at CTZ index {i}");
        assert_eq!(content_bytes_in_block(i, bs), content, "content bytes at CTZ index {i}");
    }
    // One block holds a whole 512 bytes; the 513th byte forces a second
    // block, whose own pointer header leaves it 508. So the two block
    // capacity is 1020 and the three block capacity is 1524.
    assert_eq!(block_count(512, bs), 1);
    assert_eq!(block_count(513, bs), 2);
    assert_eq!(block_count(1020, bs), 2);
    assert_eq!(block_count(1021, bs), 3);
    assert_eq!(block_count(1524, bs), 3);
    assert_eq!(block_count(1525, bs), 4);
}

// ---------------------------------------------------------------------
// File content: inline, CTZ, and the sizes where the geometry matters
// ---------------------------------------------------------------------

/// Write `content` at `/f`, remount, and read it back through the path
/// API. The remount is the load bearing part: it reads the metadata pair
/// afresh and walks the chain from disk again rather than trusting anything
/// cached in the handle.
fn write_remount_read(content: &[u8]) -> Vec<u8> {
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(Path::new("/f").unwrap(), content, &mut a, &mut b)
        .unwrap_or_else(|e| panic!("writing {} bytes at 512/32 failed: {e:?}", content.len()));
    let storage = fs.into_storage();

    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("remount must succeed");
    let mut a = buf();
    let mut b = buf();
    let size = fs.size_of(Path::new("/f").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(size as usize, content.len(), "size after remount");
    let mut out = vec![0u8; content.len()];
    fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    out
}

#[test]
fn content_sizes_bracketing_the_geometry_roundtrip() {
    // The sizes bracket every boundary the geometry introduces: the read
    // and program window (32), the inline threshold (128, a fixed crate
    // constant and so NOT scaled by geometry), and the cumulative CTZ
    // chain capacities from `ctz_block_capacities_at_512` (512 for one
    // block, 1020 for two, 1524 for three), each probed one byte below,
    // at, and one byte above. The multi block lengths in between leave a
    // partial last block.
    let sizes = [
        0usize, 1, 31, 32, 33, 63, 64, 127, 128, 129, 255, 256, 511, 512, 513, 1019, 1020, 1021,
        1523, 1524, 1525, 2048, 3000,
    ];
    for n in sizes {
        let content = pattern(n);
        let got = write_remount_read(&content);
        assert_eq!(got, content, "roundtrip of {n} bytes at 512/32");
    }
}

#[test]
fn ctz_reads_at_every_window_and_block_boundary() {
    // One multi block file, read at offsets and lengths that straddle
    // the 32 byte read grid and the block content boundaries at once.
    // `read_at_path` goes through `read_range`, whose window arithmetic
    // is exactly what a single geometry cannot exercise.
    let content = pattern(2600);
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(Path::new("/big").unwrap(), &content, &mut a, &mut b).unwrap();

    // Offsets on and around the window grid (32) and the chain block
    // boundaries (512, then 1020, then 1524).
    let interesting: [u32; 16] =
        [0, 1, 31, 32, 33, 511, 512, 513, 1019, 1020, 1021, 1523, 1524, 1525, 2047, 2599];
    let lengths: [usize; 8] = [1, 2, 31, 32, 33, 64, 511, 600];
    for off in interesting {
        for len in lengths {
            let end = (off as usize + len).min(content.len());
            if end <= off as usize {
                continue;
            }
            let want = &content[off as usize..end];
            let mut out = vec![0u8; want.len()];
            let n = fs
                .read_at_path(Path::new("/big").unwrap(), off, &mut out, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("read at offset {off} length {len} failed: {e:?}"));
            assert_eq!(n, want.len(), "short read at offset {off} length {len}");
            assert_eq!(out, want, "wrong bytes at offset {off} length {len}");
        }
    }
}

#[test]
fn file_handle_append_seek_and_truncate() {
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();

    // Append in chunks that are not a multiple of the program window and
    // not a divisor of the block content capacity, so every chunk lands
    // at a different phase within its block.
    let chunk = pattern(97);
    let rounds = 40usize;
    {
        let opts = OpenOptions::new().write(true).append(true).create(true);
        let mut f = fs.open(Path::new("/log").unwrap(), opts, &mut a, &mut b).unwrap();
        for _ in 0..rounds {
            assert_eq!(f.write(&chunk, &mut a, &mut b).unwrap(), chunk.len());
        }
        f.close(&mut a, &mut b).unwrap();
    }
    let mut expected = Vec::new();
    for _ in 0..rounds {
        expected.extend_from_slice(&chunk);
    }

    // Read the whole file back through a handle, in reads whose length
    // is coprime with both the block size and the window size.
    {
        let opts = OpenOptions::new().read(true);
        let mut f = fs.open(Path::new("/log").unwrap(), opts, &mut a, &mut b).unwrap();
        assert_eq!(f.size() as usize, expected.len());
        let mut got = Vec::new();
        let mut chunk_buf = [0u8; 37];
        loop {
            let n = f.read(&mut chunk_buf, &mut a, &mut b).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&chunk_buf[..n]);
        }
        assert_eq!(got, expected, "handle read of the appended file");
    }

    // Seek to a block interior offset and read across the boundary.
    {
        let opts = OpenOptions::new().read(true);
        let mut f = fs.open(Path::new("/log").unwrap(), opts, &mut a, &mut b).unwrap();
        let off = 1000u32;
        assert_eq!(f.seek(SeekFrom::Start(off)).unwrap(), off);
        let mut out = [0u8; 200];
        let n = f.read(&mut out, &mut a, &mut b).unwrap();
        assert_eq!(n, 200);
        assert_eq!(&out[..], &expected[off as usize..off as usize + 200]);
        // Seeking from the end lands on the same absolute offset.
        let from_end = f.seek(SeekFrom::End(-200)).unwrap();
        assert_eq!(from_end as usize, expected.len() - 200);
    }

    // Shrink to a size that is neither block nor window aligned, then
    // grow it back with a zero fill, and check both through a remount.
    let shrunk = 1237usize;
    {
        let opts = OpenOptions::new().read(true).write(true);
        let mut f = fs.open(Path::new("/log").unwrap(), opts, &mut a, &mut b).unwrap();
        f.set_len(shrunk as u32, &mut a, &mut b).unwrap();
        f.close(&mut a, &mut b).unwrap();
    }
    let storage = fs.into_storage();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = buf();
    let mut b = buf();
    let mut out = vec![0u8; shrunk];
    fs.read_at_path(Path::new("/log").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(out, expected[..shrunk], "truncated content after remount");

    let grown = 1600usize;
    {
        let opts = OpenOptions::new().read(true).write(true);
        let mut f = fs.open(Path::new("/log").unwrap(), opts, &mut a, &mut b).unwrap();
        f.set_len(grown as u32, &mut a, &mut b).unwrap();
        f.close(&mut a, &mut b).unwrap();
    }
    let mut out = vec![0xAAu8; grown];
    fs.read_at_path(Path::new("/log").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..shrunk], &expected[..shrunk], "kept prefix after growing");
    assert!(out[shrunk..].iter().all(|&x| x == 0), "grown region must read as zero fill");
}

#[test]
fn path_level_append_and_truncate_cross_the_inline_boundary_at_512() {
    // `append_to_path` has two regimes: an inline file is reassembled in
    // `content_scratch` and rewritten, and a CTZ file is extended in
    // place by filling the tail block and allocating past it. The
    // transition sits at the fixed 128 byte inline threshold, but the
    // tail fill arithmetic on the far side is pure geometry: how much
    // room the last chain block has left is `content_bytes_in_block` of
    // its index minus what is used.
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();
    let mut scratch = vec![0u8; 4096];

    let mut expected: Vec<u8> = Vec::new();
    // Appends chosen to walk from inline, across the threshold, and then
    // over the first and second chain block boundaries (512, then 1020).
    for step in [40usize, 40, 40, 40, 200, 300, 100, 400, 5] {
        let addition = pattern(step);
        fs.append_to_path(Path::new("/j").unwrap(), &addition, &mut scratch, &mut a, &mut b)
            .unwrap_or_else(|e| {
                panic!("appending {step} bytes at total {} failed: {e:?}", expected.len())
            });
        expected.extend_from_slice(&addition);

        let size = fs.size_of(Path::new("/j").unwrap(), &mut a, &mut b).unwrap();
        assert_eq!(size as usize, expected.len(), "size after appending {step} bytes");
        let mut out = vec![0u8; expected.len()];
        fs.read_at_path(Path::new("/j").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(out, expected, "content after appending {step} bytes");
    }

    // `tail_room` is the geometry derived free space in the chain's last
    // block; appending exactly that much must not allocate a new block,
    // and one more byte must.
    let room = fs.tail_room(Path::new("/j").unwrap(), &mut a, &mut b).unwrap();
    assert!(room < MemStorage512::BLOCK_SIZE as u32, "tail room cannot exceed a block");
    let filler = pattern(room as usize);
    fs.append_to_path(Path::new("/j").unwrap(), &filler, &mut scratch, &mut a, &mut b).unwrap();
    expected.extend_from_slice(&filler);
    assert_eq!(fs.tail_room(Path::new("/j").unwrap(), &mut a, &mut b).unwrap(), 0);
    fs.append_to_path(Path::new("/j").unwrap(), b"!", &mut scratch, &mut a, &mut b).unwrap();
    expected.push(b'!');

    // Truncate back below a block boundary and then zero extend past one.
    fs.truncate_path(Path::new("/j").unwrap(), 700, &mut scratch, &mut a, &mut b).unwrap();
    expected.truncate(700);
    fs.truncate_path(Path::new("/j").unwrap(), 1100, &mut scratch, &mut a, &mut b).unwrap();
    expected.resize(1100, 0);

    let storage = fs.into_storage();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = buf();
    let mut b = buf();
    let mut out = vec![0xEEu8; expected.len()];
    fs.read_at_path(Path::new("/j").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(out, expected, "content after truncate, extend, and remount");
}

// ---------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------

#[test]
fn directory_tree_operations_at_512() {
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();

    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/d/sub").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d/sub/inline").unwrap(), b"small", &mut a, &mut b).unwrap();
    let big = pattern(1400);
    fs.write_to_path(Path::new("/d/sub/big").unwrap(), &big, &mut a, &mut b).unwrap();

    let storage = fs.into_storage();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = buf();
    let mut b = buf();

    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d/sub").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b)
        .unwrap();
    names.sort();
    assert_eq!(names, vec![b"big".to_vec(), b"inline".to_vec()]);

    let mut out = vec![0u8; big.len()];
    fs.read_at_path(Path::new("/d/sub/big").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(out, big);

    // Rename across directories, then remove and rmdir the emptied tree.
    fs.rename(Path::new("/d/sub/inline").unwrap(), Path::new("/d/moved").unwrap(), &mut a, &mut b)
        .unwrap();
    assert!(fs.exists(Path::new("/d/moved").unwrap(), &mut a, &mut b).unwrap());
    assert!(!fs.exists(Path::new("/d/sub/inline").unwrap(), &mut a, &mut b).unwrap());

    fs.remove_at_path(Path::new("/d/sub/big").unwrap(), &mut a, &mut b).unwrap();
    fs.rmdir(Path::new("/d/sub").unwrap(), &mut a, &mut b).unwrap();
    fs.remove_at_path(Path::new("/d/moved").unwrap(), &mut a, &mut b).unwrap();
    fs.rmdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let mut seen = 0usize;
    fs.list_dir(Path::new("/").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, 0, "the tree must be empty again");
}

#[test]
fn a_directory_grows_past_one_pair_and_every_entry_survives() {
    // At 512 bytes a metadata pair holds roughly twice the entries a 256
    // byte pair does, so the entry count that forces a split is itself a
    // geometry fact. 90 entries is comfortably past one pair at 512 and
    // exercises the split, the HardTail chain, and the chain walk on
    // enumeration.
    let target = 90usize;
    let mut fs = make_fs();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    for i in 0..target {
        let name = format!("/d/f{i:03}");
        fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("entry {i} must fit once the pair splits: {e:?}"));
    }

    let check = |fs: &mut Fs<MemStorage512>, a: &mut [u8], b: &mut [u8]| {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
        assert_eq!(seen, target, "every entry must enumerate across the split");
        for i in 0..target {
            let name = format!("/d/f{i:03}");
            let mut out = [0u8; 1];
            fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, a, b)
                .unwrap_or_else(|e| panic!("entry {i} must read back: {e:?}"));
            assert_eq!(&out, b"v");
        }
    };
    check(&mut fs, &mut a, &mut b);

    // The chain is durable, not an in memory artifact.
    let storage = fs.into_storage();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    check(&mut fs, &mut a, &mut b);
}

/// Every metadata pair in `path`'s HardTail chain, first pair first.
fn chain_pairs<S: Storage>(
    fs: &mut Fs<S>,
    path: &str,
    a: &mut [u8],
    b: &mut [u8],
) -> Vec<littlefs2_pure::BlockPair> {
    let pair = {
        let resolved = fs.resolve(Path::new(path).unwrap(), a, b).unwrap();
        let body = resolved.struct_body;
        assert_eq!(body.len(), 8, "{path} must resolve to a directory");
        littlefs2_pure::BlockPair::new(
            littlefs2_pure::BlockAddress::new(u32::from_le_bytes([
                body[0], body[1], body[2], body[3],
            ])),
            littlefs2_pure::BlockAddress::new(u32::from_le_bytes([
                body[4], body[5], body[6], body[7],
            ])),
        )
    };
    let mut out = vec![pair];
    let mut cur = pair;
    for _ in 0..64 {
        let view = fs.read_pair(cur, a, b).unwrap();
        if !view.reader.is_hard_tail() {
            break;
        }
        match view.reader.tail() {
            Some(next) => {
                cur = next;
                out.push(next);
            }
            None => break,
        }
    }
    out
}

/// Attribute sizes that drive a multi cut split at 512 byte blocks,
/// derived again for this geometry rather than carried over from the 256
/// byte reproducer in `tests/review_l1_split_recheck.rs`.
///
/// The accounting, in the same terms that file uses. Four entries with one
/// byte names cost 13 wire bytes each (Create, NAME, and an empty inline
/// STRUCT), so the entry sequence alone is 52 bytes, and an attribute of
/// `n` bytes costs `n + 4`. The split point estimate shrinks the upper
/// portion of a cut until it fits `min(BLOCK_SIZE - SPLIT_RESERVE,
/// (BLOCK_SIZE / 2).next_multiple_of(PROG_SIZE))`, which at 512/32 is
/// `min(472, 256) = 256`: half a block, where the 256 byte geometry's
/// same formula yields 128. A piece the loop cannot cut further (one
/// entry) is instead held only to the full `BLOCK_SIZE - SPLIT_RESERVE`,
/// so 472 here.
///
/// With `A0 = A1 = 80` on entries 0 and 1, the pre trigger range is
/// `52 + 84 + 84 = 220 <= 256`, so the directory is still a single pair.
/// The trigger adds a 300 byte attribute to entry 0, taking the combined
/// range to `220 + 304 = 524`. The first cut lands at index 2: the upper
/// portion, entries 2 and 3, is 26 bytes, and the lower portion is
/// `(13 + 84 + 304) + (13 + 84) = 401 + 97 = 498`, still past 256, so a
/// single cut cannot place this range. The second cut moves entry 1 (97
/// bytes) out and leaves entry 0 alone at 401 bytes: over the half block
/// budget, under the 472 a single uncuttable entry is allowed, so it
/// commits and the loop terminates at three pairs.
const MC_A0: usize = 80;
const MC_A1: usize = 80;
const MC_TRIGGER: usize = 300;

/// Build the pre trigger directory: `/d` with four one byte named empty
/// files, attribute 1 set on the first two.
fn multi_cut_setup<S: Storage>(fs: &mut Fs<S>) {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    for name in ["/d/0", "/d/1", "/d/2", "/d/3"] {
        fs.write_to_path(Path::new(name).unwrap(), b"", &mut a, &mut b).unwrap();
    }
    fs.set_attr(Path::new("/d/0").unwrap(), 1, &[0xA0; MC_A0], &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/d/1").unwrap(), 1, &[0xA1; MC_A1], &mut a, &mut b).unwrap();
}

#[test]
fn a_growing_entry_forces_a_multi_cut_split_at_512() {
    let mut fs = make_fs();
    multi_cut_setup(&mut fs);
    let mut a = buf();
    let mut b = buf();

    assert_eq!(
        chain_pairs(&mut fs, "/d", &mut a, &mut b).len(),
        1,
        "the pre trigger directory must still be a single pair, otherwise the \
         attribute sizes above no longer set up the multi cut case"
    );

    fs.set_attr(Path::new("/d/0").unwrap(), 2, &[0xB0; MC_TRIGGER], &mut a, &mut b)
        .expect("a growing set_attr must re-split rather than fail with OutOfRange");

    assert_eq!(
        chain_pairs(&mut fs, "/d", &mut a, &mut b).len(),
        3,
        "at 512 bytes this growing op needs two cuts, not one; if the writer places \
         it in fewer pairs the sizes above have stopped exercising multi cut splitting"
    );

    // Every attribute and every entry survives the multi cut, here and
    // after a remount that walks the chain from disk again.
    let check = |fs: &mut Fs<MemStorage512>, a: &mut [u8], b: &mut [u8]| {
        let mut out = vec![0u8; MC_TRIGGER];
        let n = fs.get_attr(Path::new("/d/0").unwrap(), 2, &mut out, a, b).unwrap();
        assert_eq!(n, MC_TRIGGER);
        assert!(out.iter().all(|&x| x == 0xB0));
        let n = fs.get_attr(Path::new("/d/0").unwrap(), 1, &mut out, a, b).unwrap();
        assert_eq!(n, MC_A0);
        assert!(out[..MC_A0].iter().all(|&x| x == 0xA0));
        let n = fs.get_attr(Path::new("/d/1").unwrap(), 1, &mut out, a, b).unwrap();
        assert_eq!(n, MC_A1);
        assert!(out[..MC_A1].iter().all(|&x| x == 0xA1));
        let mut names: Vec<Vec<u8>> = Vec::new();
        fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), a, b).unwrap();
        assert_eq!(names, vec![b"0".to_vec(), b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
    };
    check(&mut fs, &mut a, &mut b);

    let storage = fs.into_storage();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    check(&mut fs, &mut a, &mut b);
}

// ---------------------------------------------------------------------
// Read alignment on a 32 byte grid
// ---------------------------------------------------------------------

/// A 512/32 device that asserts the read alignment precondition the
/// [`Storage`] contract states: every read starts on the `READ_SIZE` grid
/// and covers a whole multiple of it. Programs are deliberately not
/// checked, matching `tests/review_ctz_read_alignment.rs`: the kernel
/// emits byte granular programs on purpose and `NorAlignedStorage` is the
/// documented adapter for devices that cannot take them.
///
/// The default geometry already has such a device (reviews M7 and
/// `lfs-8e6`). Running it at a 32 byte grid matters because the read
/// window arithmetic in `read_range` rounds offsets down and lengths up to
/// `READ_SIZE`; a rounding bug that lands inside a 16 byte window can
/// escape a 16 byte grid and still cross a 32 byte one.
struct AlignedOnly512 {
    data: Vec<u8>,
    /// Reads served, so the test can show the checked path ran rather
    /// than passing because nothing ever read.
    reads: usize,
    /// Reads whose length was not the whole block, i.e. the windowed
    /// reads the CTZ and file paths issue rather than a full block
    /// metadata fetch.
    sub_block_reads: usize,
}

impl AlignedOnly512 {
    const BLOCK_SIZE: usize = 512;
    const BLOCK_COUNT: u32 = 64;
    const READ_SIZE: usize = 32;

    fn new() -> Self {
        Self {
            data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize],
            reads: 0,
            sub_block_reads: 0,
        }
    }
}

impl Storage for AlignedOnly512 {
    type Error = ();
    const READ_SIZE: usize = Self::READ_SIZE;
    const PROG_SIZE: usize = 32;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 128;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        self.reads += 1;
        if buf.len() != Self::BLOCK_SIZE {
            self.sub_block_reads += 1;
        }
        assert_eq!(
            off as usize % Self::READ_SIZE,
            0,
            "read offset {off} on block {block} is not READ_SIZE ({}) aligned",
            Self::READ_SIZE
        );
        assert_eq!(
            buf.len() % Self::READ_SIZE,
            0,
            "read length {} on block {block} offset {off} is not a READ_SIZE ({}) multiple",
            buf.len(),
            Self::READ_SIZE
        );
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let start = block as usize * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

#[test]
fn every_read_lands_on_the_32_byte_grid() {
    // Reaching the end of this test IS the assertion: any read the
    // kernel issues off the 32 byte grid panics inside the device.
    let mut storage = AlignedOnly512::new();
    let mut scratch = [0u8; AlignedOnly512::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; AlignedOnly512::BLOCK_SIZE];
    let mut buf_b = [0u8; AlignedOnly512::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; AlignedOnly512::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly512::BLOCK_SIZE];

    // A multi block CTZ file: writing it walks the chain, and writing a
    // SECOND one makes the allocator walk the first one's chain when it
    // rebuilds the used set (the review M7 path).
    let first = pattern(2100);
    fs.write_to_path(Path::new("/one").unwrap(), &first, &mut a, &mut b).unwrap();
    let second = pattern(1300);
    fs.write_to_path(Path::new("/two").unwrap(), &second, &mut a, &mut b).unwrap();

    // Reads at offsets and lengths that are not multiples of 32, so the
    // read path has to widen them to the grid rather than pass them
    // through.
    for (off, len) in [(0u32, 7usize), (1, 7), (17, 45), (500, 60), (1019, 5), (1521, 200)] {
        let end = (off as usize + len).min(first.len());
        let mut out = vec![0u8; end - off as usize];
        fs.read_at_path(Path::new("/one").unwrap(), off, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(out, first[off as usize..end], "content at offset {off} length {len}");
    }

    // The append path reads the chain tail again, and the file handle read
    // path is a different caller of the same window helper.
    {
        let opts = OpenOptions::new().read(true).write(true).append(true);
        let mut f = fs.open(Path::new("/one").unwrap(), opts, &mut a, &mut b).unwrap();
        f.write(&pattern(41), &mut a, &mut b).unwrap();
        f.close(&mut a, &mut b).unwrap();
    }
    let mut out = vec![0u8; second.len()];
    fs.read_at_path(Path::new("/two").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(out, second);

    // A directory split drives the metadata walk, and a remount reads every
    // pair from scratch.
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    for i in 0..34 {
        let name = format!("/d/e{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b).unwrap();
    }
    let storage = fs.into_storage();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, 34);

    // Non vacuity: the alignment assertions only mean something if the
    // kernel actually read, and specifically if it issued reads narrower
    // than a whole block, which are the ones `read_range` has to widen to
    // the grid. A refactor that stopped exercising the windowed read path
    // would make this test pass for the wrong reason.
    let dev = fs.storage();
    assert!(dev.reads > 100, "the alignment checks saw only {} reads", dev.reads);
    assert!(
        dev.sub_block_reads > 20,
        "only {} of {} reads were narrower than a block, so the windowed read path \
         is barely covered here",
        dev.sub_block_reads,
        dev.reads
    );
    println!(
        "512/32 alignment: {} reads, {} narrower than a block",
        dev.reads, dev.sub_block_reads
    );
}

// ---------------------------------------------------------------------
// Worn blocks at 512
// ---------------------------------------------------------------------

/// A 512/32 device where nominated blocks refuse every program, modelling
/// worn cells. The counterpart of the `Dev` in
/// `tests/review_l1_forced_victim.rs`, at the second geometry.
struct Worn512 {
    data: Vec<u8>,
    bad: Vec<u32>,
    attempted_bad: Vec<u32>,
}

impl Worn512 {
    const BLOCK_SIZE: usize = 512;
    const BLOCK_COUNT: u32 = 64;

    fn new() -> Self {
        Self {
            data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize],
            bad: Vec::new(),
            attempted_bad: Vec::new(),
        }
    }
}

impl Storage for Worn512 {
    type Error = ();
    const READ_SIZE: usize = 32;
    const PROG_SIZE: usize = 32;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 128;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if self.bad.contains(&block) {
            self.attempted_bad.push(block);
            return Err(());
        }
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let start = block as usize * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

#[test]
fn a_worn_active_block_relocates_the_pair_at_512() {
    // The forced victim path: an in place append fails on a worn active
    // block, so the writer relocates the whole pair onto fresh blocks and
    // rebuilds the live set there. `tests/review_l1_forced_victim.rs`
    // sweeps this at 256 with attribute sizes tuned to that block size;
    // the same path at 512 carries up to twice the live bytes through the
    // relocation, which is the geometry sensitive part.
    let mut storage = Worn512::new();
    let mut scratch = [0u8; Worn512::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; Worn512::BLOCK_SIZE];
    let mut buf_b = [0u8; Worn512::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; Worn512::BLOCK_SIZE];
    let mut b = [0u8; Worn512::BLOCK_SIZE];

    // One entry carrying a large attribute: a single entry range never
    // splits, so the pair holds the most live bytes a 512 byte block can
    // carry into a relocation.
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/d/x").unwrap(), b"", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/d/x").unwrap(), 1, &[0xA5; 300], &mut a, &mut b).unwrap();

    let before = chain_pairs(&mut fs, "/d", &mut a, &mut b)[0];
    // Wear both blocks of the pair the directory currently lives on, so
    // the next commit cannot land in place or in the pair's alternate.
    fs.storage_mut().bad.extend_from_slice(&[before.a.as_u32(), before.b.as_u32()]);

    fs.set_attr(Path::new("/d/x").unwrap(), 2, &[0x5A; 40], &mut a, &mut b)
        .expect("a worn pair must relocate rather than fail the commit");

    assert!(
        !fs.storage().attempted_bad.is_empty(),
        "the test must actually have driven a program onto a worn block"
    );
    let after = chain_pairs(&mut fs, "/d", &mut a, &mut b)[0];
    assert_ne!(
        (before.a.as_u32(), before.b.as_u32()),
        (after.a.as_u32(), after.b.as_u32()),
        "the pair must have moved off the worn blocks"
    );

    // Both attributes and the entry survive the relocation, and the new
    // pair address is durable.
    let storage = fs.into_storage();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut out = [0u8; 300];
    let n = fs.get_attr(Path::new("/d/x").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert!(out.iter().all(|&x| x == 0xA5));
    let n = fs.get_attr(Path::new("/d/x").unwrap(), 2, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 40);
    assert!(out[..40].iter().all(|&x| x == 0x5A));
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"x".to_vec()]);
}

// ---------------------------------------------------------------------
// Wear leveling at 512
// ---------------------------------------------------------------------

/// A 512/32 device with a short wear rotation interval, so metadata
/// commits relocate their pair every few commits instead of every 500.
struct Wear512 {
    data: Vec<u8>,
    erases: Vec<u32>,
}

impl Wear512 {
    const BLOCK_SIZE: usize = 512;
    const BLOCK_COUNT: u32 = 48;

    fn new() -> Self {
        Self {
            data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize],
            erases: Vec::new(),
        }
    }
}

impl Storage for Wear512 {
    type Error = ();
    const READ_SIZE: usize = 32;
    const PROG_SIZE: usize = 32;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    /// Rotate after every second commit, so a handful of writes exercises
    /// several relocations.
    const BLOCK_CYCLES: i32 = 2;
    const CACHE_SIZE: usize = 128;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = block as usize * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        self.erases.push(block);
        let start = block as usize * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

#[test]
fn wear_rotation_relocates_pairs_at_512() {
    let mut storage = Wear512::new();
    let mut scratch = [0u8; Wear512::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; Wear512::BLOCK_SIZE];
    let mut buf_b = [0u8; Wear512::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = [0u8; Wear512::BLOCK_SIZE];
    let mut b = [0u8; Wear512::BLOCK_SIZE];

    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    let pair_before = chain_pairs(&mut fs, "/d", &mut a, &mut b)[0];

    // Enough commits into `/d` to trip the rotation interval several
    // times over, each one a metadata commit on `/d`'s pair.
    for i in 0..20 {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b)
            .unwrap_or_else(|e| panic!("write {i} under wear rotation failed: {e:?}"));
    }
    let pair_after = chain_pairs(&mut fs, "/d", &mut a, &mut b)[0];
    assert_ne!(
        (pair_before.a.as_u32(), pair_before.b.as_u32()),
        (pair_after.a.as_u32(), pair_after.b.as_u32()),
        "a BLOCK_CYCLES of 2 must have relocated the directory pair at least once"
    );
    assert!(!fs.storage().erases.is_empty(), "relocation must erase its fresh blocks");

    // Every entry still reads back, and the relocated chain is durable.
    let storage = fs.into_storage();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, 20, "every entry must survive the relocations");
    for i in 0..20 {
        let name = format!("/d/f{i:02}");
        let mut out = [0u8; 1];
        fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(&out, b"v");
    }
}

// ---------------------------------------------------------------------
// Strict NOR programs at a 32 byte window
// ---------------------------------------------------------------------

#[test]
fn strict_nor_programs_stay_window_aligned_at_512() {
    // `StrictNorStorage512` panics on any program that is not a whole
    // aligned 32 byte window or that tries to set a bit back to one, so
    // reaching the end of this test IS the assertion: every program the
    // kernel issued through the alignment adapter was NOR legal at a
    // window size no other suite uses.
    let mut storage = NorAlignedStorage::new(StrictNorStorage512::new())
        .expect("512/32 satisfies the alignment adapter's invariants");
    let mut scratch = buf();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = buf();
    let mut buf_b = buf();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = buf();
    let mut b = buf();

    fs.write_to_path(Path::new("/inline").unwrap(), b"tiny", &mut a, &mut b).unwrap();
    let big = pattern(1700);
    fs.write_to_path(Path::new("/big").unwrap(), &big, &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    for i in 0..12 {
        let name = format!("/d/e{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b).unwrap();
    }
    fs.sync().unwrap();

    // Remount off the flushed device and read everything back, so the
    // NOR legal programs are also the correct bytes.
    let image = common::nor_image_of_on::<StrictNorStorage512>(fs);
    let mut fs = common::mount_nor_image_strict_on::<StrictNorStorage512>(
        image,
        "clean 512/32 NOR shutdown",
    );
    let mut a = buf();
    let mut b = buf();
    let mut out = vec![0u8; big.len()];
    fs.read_at_path(Path::new("/big").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(out, big);
    let mut small = [0u8; 4];
    fs.read_at_path(Path::new("/inline").unwrap(), 0, &mut small, &mut a, &mut b).unwrap();
    assert_eq!(&small, b"tiny");
    let mut seen = 0usize;
    fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, 12);
}

// ---------------------------------------------------------------------
// Torn writes at the second geometry
// ---------------------------------------------------------------------

/// Create `/log` as an inline file. Generic over the storage so the same
/// sequence drives both tear models.
fn inline_scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    let _ = fs.write_to_path(Path::new("/log").unwrap(), b"ONE", &mut a, &mut b);
}

/// Write a CTZ backed `/big` spanning three chain blocks at 512 bytes.
fn ctz_scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    let _ = fs.write_to_path(Path::new("/big").unwrap(), &pattern(1300), &mut a, &mut b);
}

/// `/log`'s content on a mounted post tear image, or empty when the file
/// is absent. Every error is a regression: the image mounted, so its read
/// surface must be coherent.
fn read_path<S: Storage>(fs: &mut Fs<S>, path: &str) -> Vec<u8> {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    let p = Path::new(path).unwrap();
    if !fs.exists(p, &mut a, &mut b).unwrap() {
        return Vec::new();
    }
    let size = fs.size_of(p, &mut a, &mut b).unwrap();
    let mut out = vec![0u8; size as usize];
    fs.read_at_path(p, 0, &mut out, &mut a, &mut b).unwrap();
    out
}

#[test]
fn inline_write_is_atomic_at_512_across_every_kernel_program_boundary() {
    let scenario = inline_scenario::<TornWriteStorage<MemStorage512>>;
    let (fmt_calls, scenario_calls) = common::torn_call_counts_on::<MemStorage512, _>(scenario);
    assert!(scenario_calls > 0, "the scenario must perform at least one program call");

    for trigger in 1..=fmt_calls + scenario_calls + 5 {
        match common::run_torn_scenario_on::<MemStorage512, _>(trigger, scenario) {
            common::TornRun::TornFormat => assert!(
                trigger <= fmt_calls,
                "trigger {trigger}: format reported torn past its own {fmt_calls} calls"
            ),
            common::TornRun::Image(image) => {
                let mut fs = common::mount_image_strict_on::<MemStorage512>(
                    image,
                    &format!("512/32 inline sweep trigger {trigger}"),
                );
                let content = read_path(&mut fs, "/log");
                assert!(
                    content.is_empty() || content == b"ONE",
                    "trigger {trigger}: content {content:?} is neither the pre state nor the post state"
                );
            }
        }
    }
}

#[test]
fn ctz_write_is_atomic_at_512_across_every_kernel_program_boundary() {
    let want = pattern(1300);
    let scenario = ctz_scenario::<TornWriteStorage<MemStorage512>>;
    let (fmt_calls, scenario_calls) = common::torn_call_counts_on::<MemStorage512, _>(scenario);
    assert!(scenario_calls > 1, "a multi block CTZ write must take several program calls");

    for trigger in 1..=fmt_calls + scenario_calls + 5 {
        match common::run_torn_scenario_on::<MemStorage512, _>(trigger, scenario) {
            common::TornRun::TornFormat => assert!(
                trigger <= fmt_calls,
                "trigger {trigger}: format reported torn past its own {fmt_calls} calls"
            ),
            common::TornRun::Image(image) => {
                let mut fs = common::mount_image_strict_on::<MemStorage512>(
                    image,
                    &format!("512/32 CTZ sweep trigger {trigger}"),
                );
                let content = read_path(&mut fs, "/big");
                assert!(
                    content.is_empty() || content == want,
                    "trigger {trigger}: /big holds {} bytes, neither the pre state (absent) \
                     nor the post state ({} bytes)",
                    content.len(),
                    want.len()
                );
            }
        }
    }
}

/// Entries written into `/d` by the split scenario. At 512 bytes a pair
/// holds about sixteen of these (each costs 16 wire bytes: Create, a three
/// byte NAME, and a one byte inline STRUCT, against a half block split
/// budget of 256), so 24 forces one split and leaves free blocks for the
/// continuation. The untorn run below asserts the split really
/// happened, so a layout change cannot silently retire this coverage.
const SPLIT_ENTRIES: usize = 24;

fn split_scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    if fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).is_err() {
        return;
    }
    for i in 0..SPLIT_ENTRIES {
        let name = format!("/d/f{i:02}");
        if fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b).is_err() {
            return;
        }
    }
}

/// The names in `/d`, or `None` when `/d` itself is absent. Every other
/// error is a regression: the image mounted, so its directory surface
/// must be readable.
fn enumerate_split_dir<S: Storage>(fs: &mut Fs<S>, ctx: &str) -> Option<Vec<Vec<u8>>> {
    let mut a = vec![0u8; S::BLOCK_SIZE];
    let mut b = vec![0u8; S::BLOCK_SIZE];
    if !fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).unwrap() {
        return None;
    }
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b)
        .unwrap_or_else(|e| panic!("{ctx}: /d exists but does not enumerate: {e:?}"));
    // The surviving entries must be an exact prefix of the write order,
    // each still holding its content: a torn write may lose the tail of
    // the sequence, never reorder it, duplicate it, or corrupt an entry
    // written before the tear.
    for (i, name) in names.iter().enumerate() {
        assert_eq!(
            name,
            format!("f{i:02}").as_bytes(),
            "{ctx}: surviving entries must be an exact prefix of the write order, got {names:?}"
        );
        let path = format!("/d/f{i:02}");
        let mut out = [0u8; 1];
        fs.read_at_path(Path::new(&path).unwrap(), 0, &mut out, &mut a, &mut b)
            .unwrap_or_else(|e| panic!("{ctx}: entry {i} survived but does not read: {e:?}"));
        assert_eq!(&out, b"v", "{ctx}: entry {i} holds the wrong content");
    }
    Some(names)
}

#[test]
fn directory_split_is_atomic_at_512_across_every_kernel_program_boundary() {
    // The untorn run first: it pins that the scenario actually splits at
    // this geometry, which is what makes the sweep meaningful.
    {
        let mut fs = make_fs();
        split_scenario(&mut fs);
        let mut a = buf();
        let mut b = buf();
        assert!(
            chain_pairs(&mut fs, "/d", &mut a, &mut b).len() >= 2,
            "{SPLIT_ENTRIES} entries must overflow one 512 byte pair; if they no longer do, \
             this sweep is no longer covering the split crash window"
        );
    }

    let scenario = split_scenario::<TornWriteStorage<MemStorage512>>;
    let (fmt_calls, scenario_calls) = common::torn_call_counts_on::<MemStorage512, _>(scenario);
    assert!(scenario_calls > 1, "a splitting write sequence must take several program calls");

    for trigger in 1..=fmt_calls + scenario_calls + 2 {
        let image = match common::run_torn_scenario_on::<MemStorage512, _>(trigger, scenario) {
            common::TornRun::TornFormat => {
                assert!(
                    trigger <= fmt_calls,
                    "trigger {trigger}: format reported torn past its own {fmt_calls} calls"
                );
                continue;
            }
            common::TornRun::Image(image) => image,
        };
        let ctx = format!("512/32 split sweep trigger {trigger}");
        let names_a = {
            let mut fs = common::mount_image_strict_on::<MemStorage512>(
                image.clone(),
                &format!("{ctx}, first remount"),
            );
            enumerate_split_dir(&mut fs, &ctx)
        };
        let mut fs = common::mount_image_strict_on::<MemStorage512>(
            image,
            &format!("{ctx}, second remount"),
        );
        assert_eq!(
            names_a,
            enumerate_split_dir(&mut fs, &ctx),
            "{ctx}: the directory state must be stable across consecutive remounts"
        );
    }
}

#[test]
fn directory_split_is_atomic_at_512_across_every_nor_program_landing() {
    let scenario =
        split_scenario::<NorAlignedStorage<common::TornPartialStorage<StrictNorStorage512>>>;
    let (fmt_calls, scenario_calls) =
        common::nor_torn_call_counts_on::<StrictNorStorage512, _>(scenario);
    assert!(scenario_calls > 1, "the splitting sequence must issue several device programs");

    let mut witness = common::PartialLandingWitness::new();
    for partial in common::NOR_PARTIAL_LANDINGS_512 {
        for trigger in 1..=fmt_calls + scenario_calls + 2 {
            let ctx =
                format!("512/32 NOR split sweep trigger {trigger}, partial landing {partial}");
            let image = match common::run_nor_torn_scenario_on::<StrictNorStorage512, _>(
                trigger, partial, scenario,
            ) {
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
            let names_a = {
                let mut fs =
                    common::mount_nor_image_strict_on::<StrictNorStorage512>(image.clone(), &ctx);
                enumerate_split_dir(&mut fs, &ctx)
            };
            let mut fs = common::mount_nor_image_strict_on::<StrictNorStorage512>(
                image,
                &format!("{ctx}, second remount"),
            );
            assert_eq!(
                names_a,
                enumerate_split_dir(&mut fs, &ctx),
                "{ctx}: the directory state must be stable across consecutive remounts"
            );
        }
    }
    witness.assert_partials_landed("512/32 NOR split sweep");
}

#[test]
fn nor_device_tears_at_512_leave_a_mountable_image() {
    // The finer model: the tear lands inside a device program window, and
    // a prefix of that window may have reached the flash. At 512/32 the
    // window is twice the default's, so a partial landing can cut a tag
    // body in places the 16 byte window cannot express.
    let scenario = |fs: &mut common::NorTornFs512| {
        let mut a = buf();
        let mut b = buf();
        let _ = fs.write_to_path(Path::new("/log").unwrap(), b"ONE", &mut a, &mut b);
    };
    let (fmt_calls, scenario_calls) =
        common::nor_torn_call_counts_on::<StrictNorStorage512, _>(scenario);
    assert!(scenario_calls > 0, "the scenario must issue at least one device program");

    let mut witness = common::PartialLandingWitness::new();
    for partial in common::NOR_PARTIAL_LANDINGS_512 {
        for trigger in 1..=fmt_calls + scenario_calls + 4 {
            match common::run_nor_torn_scenario_on::<StrictNorStorage512, _>(
                trigger, partial, scenario,
            ) {
                common::TornRun::TornFormat => assert!(
                    trigger <= fmt_calls,
                    "trigger {trigger}: format reported torn past its own {fmt_calls} \
                     device programs"
                ),
                common::TornRun::Image(image) => {
                    witness.observe(partial, trigger, &image);
                    let ctx = format!("512/32 NOR sweep trigger {trigger} landing {partial}");
                    let mut fs =
                        common::mount_nor_image_strict_on::<StrictNorStorage512>(image, &ctx);
                    let content = read_path(&mut fs, "/log");
                    assert!(
                        content.is_empty() || content == b"ONE",
                        "{ctx}: content {content:?} is neither the pre state nor the post state"
                    );
                }
            }
        }
    }
    witness.assert_partials_landed("512/32 NOR inline sweep");
}
