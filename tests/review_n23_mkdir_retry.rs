//! `lfs-n23`: `mkdir`'s pair init commit joins the fresh block retry
//! discipline.
//!
//! ADR-0014 gave every other fresh block path the same shape: when the
//! freshly allocated block refuses the write (or, since review H2, when
//! the read back disagrees with what was sent), exclude that candidate
//! from the allocator and try another, bounded by
//! `MAX_BAD_BLOCK_RETRIES`. `mkdir` predates that machinery. It
//! allocated a pair, programmed the empty init commit onto block A, and
//! mapped both a program failure and a read back mismatch straight to
//! `Error::Io`, so a single worn block sitting where the allocator
//! happened to point made `mkdir` fail on an otherwise healthy device.
//!
//! The retry is purely additive because the pair is unreferenced until
//! the parent's `CreateDir` commit lands: an abandoned candidate is a
//! blank orphan the next allocator scan reclaims, exactly like the
//! continuation blocks a failed directory split leaves behind.
//!
//! The crash window between the init commit and the parent commit is
//! already swept at every program call boundary by
//! `tests/dir_split_torn.rs` (its scenario opens with `mkdir /d`, and it
//! asserts the post tear image mounts and leaves no pair reachable from
//! the root that is missing from the tree) and by
//! `tests/pending_softtail_torn.rs` (`mkdir /a; mkdir /b; rmdir /a`).
//! This file therefore covers only what those do not: the retry itself
//! and its bound.

use littlefs2_pure::{Error, Fs, Path, Storage};

const BS: usize = 256;
const BC: u32 = 64;

/// Retry bound the kernel applies to worn block exclusion
/// (`fs::MAX_BAD_BLOCK_RETRIES`, private). Mirrored here so the
/// boundedness test states the number it is pinning.
const MAX_BAD_BLOCK_RETRIES: usize = 8;

/// Device with two independent failure modes per block: `refuse`
/// rejects every program (a block that reports its own wear), and `lie`
/// accepts the program, reports success, and lands one flipped bit (a
/// block that does not). `mkdir` has to survive both the same way.
struct WornDev {
    data: Vec<u8>,
    refuse: Vec<u32>,
    lie: Vec<u32>,
    /// Programs actually rejected, so a test cannot pass vacuously.
    refusals: usize,
    /// Programs actually corrupted, likewise.
    corruptions: usize,
    /// Total program calls, for the boundedness assertion.
    programs: usize,
}

impl WornDev {
    fn new() -> Self {
        Self {
            data: vec![0xFFu8; BS * BC as usize],
            refuse: Vec::new(),
            lie: Vec::new(),
            refusals: 0,
            corruptions: 0,
            programs: 0,
        }
    }
}

impl Storage for WornDev {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = BS;
    const BLOCK_COUNT: u32 = BC;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(buf.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[s..e]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, bytes: &[u8]) -> Result<(), ()> {
        self.programs += 1;
        if self.refuse.contains(&block) {
            self.refusals += 1;
            return Err(());
        }
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(bytes.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(bytes);
        if self.lie.contains(&block) && !bytes.is_empty() {
            self.data[s + bytes.len() / 2] ^= 0x40;
            self.corruptions += 1;
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= BC {
            return Err(());
        }
        let s = (block as usize) * BS;
        self.data[s..s + BS].fill(0xFF);
        Ok(())
    }
}

fn buf() -> [u8; BS] {
    [0u8; BS]
}

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// Format an honest device and hand it back, so the superblock is
/// written before any block starts failing.
fn formatted() -> WornDev {
    let mut dev = WornDev::new();
    let mut sb = buf();
    Fs::format(&mut dev, &mut sb).unwrap();
    dev
}

fn mount(dev: WornDev) -> Fs<WornDev> {
    let mut a = buf();
    let mut b = buf();
    Fs::mount(dev, &mut a, &mut b).unwrap()
}

/// Read `/d`'s `DirStruct` body and return the pair it names.
fn dir_pair(fs: &mut Fs<WornDev>) -> (u32, u32) {
    let mut a = buf();
    let mut b = buf();
    let r = fs.resolve(p("/d"), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body.len(), 8);
    let lo = u32::from_le_bytes(r.struct_body[0..4].try_into().unwrap());
    let hi = u32::from_le_bytes(r.struct_body[4..8].try_into().unwrap());
    (lo, hi)
}

/// After format the root pair is `{0, 1}` and the allocator hands out
/// block 2 next, so `mkdir` aims its init commit at block 2. Block 2
/// refuses every program. The allocation must exclude it and retry, and
/// the directory must land on a later candidate rather than failing the
/// whole call.
#[test]
fn mkdir_retries_past_a_worn_first_candidate() {
    let mut dev = formatted();
    dev.refuse.push(2);
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(p("/d"), &mut a, &mut b).expect("mkdir must retry past a worn first candidate");

    let (lo, hi) = dir_pair(&mut fs);
    assert!(lo != 2 && hi != 2, "the worn block must not be part of the pair, got {lo:?}/{hi:?}");

    // The directory is usable and survives a remount.
    fs.write_to_path(p("/d/f"), b"hello", &mut a, &mut b).unwrap();
    let dev = fs.into_storage();
    assert!(dev.refusals > 0, "the worn block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    let mut out = [0u8; 8];
    let n = fs.read_at_path(p("/d/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"hello");
}

/// The same, for a block that accepts the program and lands corrupted
/// cells. Review H2 already made the init commit read back what it
/// wrote; before this change that read back reported `Io` instead of
/// retrying, so a silently lying block was fatal where a loud one now
/// is not.
#[test]
fn mkdir_retries_past_a_lying_first_candidate() {
    let mut dev = formatted();
    dev.lie.push(2);
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(p("/d"), &mut a, &mut b).expect("mkdir must retry past a lying first candidate");

    let (lo, hi) = dir_pair(&mut fs);
    assert!(lo != 2 && hi != 2, "the lying block must not be part of the pair, got {lo:?}/{hi:?}");

    fs.write_to_path(p("/d/f"), b"hello", &mut a, &mut b).unwrap();
    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    let mut out = [0u8; 8];
    let n = fs.read_at_path(p("/d/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"hello");
}

/// Two worn candidates in a row are both excluded. This pins that the
/// exclusion accumulates across attempts rather than being reset each
/// time, which a loop that forgot to grow its exclusion list would not
/// do (it would retry the same block forever).
#[test]
fn mkdir_retries_past_several_worn_candidates() {
    let mut dev = formatted();
    dev.refuse.extend_from_slice(&[2, 3, 4]);
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(p("/d"), &mut a, &mut b).expect("mkdir must retry past several worn candidates");

    let (lo, hi) = dir_pair(&mut fs);
    for bad in [2u32, 3, 4] {
        assert!(lo != bad && hi != bad, "worn block {bad} must not be in the pair {lo}/{hi}");
    }
}

/// A device where nothing accepts a write must fail bounded. The retry
/// stops after `MAX_BAD_BLOCK_RETRIES` exclusions, so the whole call
/// costs at most one init commit program per attempt; it never spins.
#[test]
fn mkdir_on_a_wholly_worn_device_fails_bounded() {
    let mut dev = formatted();
    dev.refuse = (2..BC).collect();
    dev.programs = 0; // count only what the mkdir below costs
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    let err = fs.mkdir(p("/d"), &mut a, &mut b).expect_err("a wholly worn device must fail mkdir");
    assert!(matches!(err, Error::Io | Error::OutOfRange), "got {err:?}");

    let dev = fs.into_storage();
    assert!(dev.refusals > 0, "no program was refused; the test proved nothing");
    // Exactly one init commit program per attempt: eight exclusions plus
    // the initial attempt is nine. An exact count, not a ceiling, so any
    // change in the retry shape surfaces here.
    assert_eq!(
        dev.programs,
        MAX_BAD_BLOCK_RETRIES + 1,
        "the retry loop must stay bounded at one program per attempt"
    );

    // The failed mkdir left nothing behind: the image still mounts and
    // holds no `/d`.
    let mut fs = mount(dev);
    assert!(!fs.exists(p("/d"), &mut a, &mut b).unwrap(), "a failed mkdir must not leave an entry");
}
