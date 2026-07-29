//! Review L1: a compacting split must re-check the portion it keeps.
//!
//! `compute_split_index` bounds only the *upper* portion of a cut (it
//! shrinks it to about half a block). The lower portion is whatever
//! remains, and a write op that grows an entry already in that portion
//! (a `set_attr`, an inline rewrite, a rename to a longer name) can push
//! the remainder past one block. Before the fix the writer cut once and
//! committed, so the lower commit overflowed and the whole operation
//! failed with `Error::OutOfRange`; the C reference loops
//! (`lfs_dir_splittingcompact` sets `end = split` and measures again) and
//! cuts until every piece fits.
//!
//! The oracle was run directly: the identical sequence against the
//! vendored C reference at `tools/gen_vectors/littlefs` on the same
//! geometry returns 0 from every call, reads all three attributes back
//! after a remount, and enumerates all four entries.
//!
//! The sequence is tuned to the 256-byte geometry and is deliberately
//! fragile in one direction only: every step below asserts, so a change
//! in commit layout that stops producing an over-one-block lower portion
//! fails loudly here rather than silently retiring the coverage.

use littlefs2_pure::{Fs, Path, Storage};

/// 64-block device: room for the continuation pairs a multi-cut split
/// allocates, with the same 256-byte / 16-byte geometry the rest of the
/// suite uses.
struct Dev {
    data: Vec<u8>,
    /// Blocks whose `program` fails, modelling worn cells. Empty unless a
    /// test wires specific blocks bad.
    bad: Vec<u32>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize], bad: Vec::new() }
    }
}
impl Storage for Dev {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        if self.bad.contains(&block) {
            return Err(());
        }
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let s = (block as usize) * Self::BLOCK_SIZE;
        self.data[s..s + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

fn buf() -> [u8; Dev::BLOCK_SIZE] {
    [0u8; Dev::BLOCK_SIZE]
}

/// Attribute values sized so the pair reaches the state the finding
/// needs. With four entries of 13 wire bytes each, `A0` and `A1` grow
/// entries 0 and 1 to 77 bytes apiece by log append (no compaction), and
/// `TRIGGER` then forces a compaction whose combined range is 304 bytes.
/// The single cut lands at index 2, leaving a 278-byte lower portion that
/// no 256-byte block can hold.
const A0: usize = 60;
const A1: usize = 60;
const TRIGGER: usize = 120;

/// Build the pre-trigger state: `/d` holding four one-byte-named empty
/// files, with attribute id 1 set on the first two.
fn setup(fs: &mut Fs<Dev>) {
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    for name in ["/d/0", "/d/1", "/d/2", "/d/3"] {
        fs.write_to_path(Path::new(name).unwrap(), b"", &mut a, &mut b).unwrap();
    }
    fs.set_attr(Path::new("/d/0").unwrap(), 1, &[0xA0; A0], &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/d/1").unwrap(), 1, &[0xA1; A1], &mut a, &mut b).unwrap();
}

fn mount(storage: Dev, ba: &mut [u8; Dev::BLOCK_SIZE], bb: &mut [u8; Dev::BLOCK_SIZE]) -> Fs<Dev> {
    Fs::mount(storage, ba, bb).unwrap()
}

#[test]
fn growing_an_entry_re_splits_until_the_lower_half_fits() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = mount(storage, &mut ba, &mut bb);
    setup(&mut fs);

    let mut a = buf();
    let mut b = buf();
    // The op the single-cut writer rejected with `OutOfRange`: it grows
    // entry 0, which sits in the lower portion of the cut the size
    // estimate picks, past what one block can hold.
    fs.set_attr(Path::new("/d/0").unwrap(), 2, &[0xB0; TRIGGER], &mut a, &mut b)
        .expect("a growing set_attr must re-split, not fail with OutOfRange");

    // Every attribute reads back, including the two written before the
    // split moved entries into continuations.
    let mut out = [0u8; TRIGGER];
    let n = fs.get_attr(Path::new("/d/0").unwrap(), 2, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, TRIGGER);
    assert!(out.iter().all(|&x| x == 0xB0));
    let n = fs.get_attr(Path::new("/d/0").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, A0);
    assert!(out[..A0].iter().all(|&x| x == 0xA0));
    let n = fs.get_attr(Path::new("/d/1").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, A1);
    assert!(out[..A1].iter().all(|&x| x == 0xA1));

    // Every entry survives, in order, exactly once.
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"0".to_vec(), b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);

    // The image remounts and reads back the same state, so the chain the
    // multi-cut split built is durable and not merely in-memory.
    let storage = fs.into_storage();
    let mut fs = mount(storage, &mut ba, &mut bb);
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"0".to_vec(), b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
    let n = fs.get_attr(Path::new("/d/0").unwrap(), 2, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, TRIGGER);
}

#[test]
fn the_multi_cut_split_produces_three_pairs() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = mount(storage, &mut ba, &mut bb);
    setup(&mut fs);

    let mut a = buf();
    let mut b = buf();
    let before = chain_len(&mut fs, &mut a, &mut b);
    assert_eq!(before, 1, "the pre-trigger directory is a single pair");

    fs.set_attr(Path::new("/d/0").unwrap(), 2, &[0xB0; TRIGGER], &mut a, &mut b).unwrap();

    // Two cuts: one continuation for entries 2 and 3, one for entry 1,
    // entry 0 alone left in the original pair. A single cut cannot place
    // this range, which is the whole finding.
    let after = chain_len(&mut fs, &mut a, &mut b);
    assert_eq!(after, 3, "the growing op needs two cuts, not one");
}

#[test]
fn a_multi_cut_split_relocates_past_worn_continuation_blocks() {
    // Wear four of the low free blocks so both cuts hit a worn candidate
    // and have to reallocate. The retry budget is shared across cuts, so
    // this also pins that a second cut does not re-offer a block the
    // first cut already found worn.
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = mount(storage, &mut ba, &mut bb);
    setup(&mut fs);

    let mut a = buf();
    let mut b = buf();
    fs.storage_mut().bad.extend_from_slice(&[4, 5, 6, 7]);
    fs.set_attr(Path::new("/d/0").unwrap(), 2, &[0xB0; TRIGGER], &mut a, &mut b)
        .expect("worn continuation blocks must be relocated past, not fatal");

    let pairs = chain_pairs(&mut fs, &mut a, &mut b);
    assert_eq!(pairs.len(), 3, "the split still needs two cuts on a worn device");
    for p in &pairs {
        for blk in [p.a.as_u32(), p.b.as_u32()] {
            assert!(!(4..=7).contains(&blk), "a continuation landed on the worn block {blk}");
        }
    }

    let mut out = [0u8; TRIGGER];
    let n = fs.get_attr(Path::new("/d/0").unwrap(), 2, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, TRIGGER);
    assert!(out.iter().all(|&x| x == 0xB0));

    // The image remounts and no continuation landed on a worn block.
    let storage = fs.into_storage();
    let mut fs = mount(storage, &mut ba, &mut bb);
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"0".to_vec(), b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
}

/// Length of `/d`'s HardTail chain, counting the first pair.
fn chain_len(
    fs: &mut Fs<Dev>,
    a: &mut [u8; Dev::BLOCK_SIZE],
    b: &mut [u8; Dev::BLOCK_SIZE],
) -> u32 {
    chain_pairs(fs, a, b).len() as u32
}

/// Every metadata pair in `/d`'s HardTail chain, first pair first.
fn chain_pairs(
    fs: &mut Fs<Dev>,
    a: &mut [u8; Dev::BLOCK_SIZE],
    b: &mut [u8; Dev::BLOCK_SIZE],
) -> Vec<littlefs2_pure::BlockPair> {
    let pair = {
        let resolved = fs.resolve(Path::new("/d").unwrap(), a, b).unwrap();
        assert_eq!(resolved.struct_body.len(), 8, "/d must resolve to a directory");
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
