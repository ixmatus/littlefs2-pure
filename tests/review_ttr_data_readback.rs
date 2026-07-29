//! `lfs-ttr`: read back verification for the file data program sites.
//!
//! Review H2 taught the eight metadata commit sites to re read what they
//! just programmed and CRC compare it, so a device that accepts a program
//! and lands corrupted cells takes the worn block path instead of
//! reporting durable success. The file data sites were left out of that
//! commit. The C reference validates every program it issues, file data
//! included (`lfs_bd_prog`'s validate flag, which routes a mismatch into
//! the same `relocate:` label a hard program failure takes), so a lying
//! device silently corrupted file content here while the write returned
//! `Ok`.
//!
//! The five file data program sites and what each must do when the read
//! back disagrees:
//!
//! 1. `try_build_ctz_chain`, the initial CTZ write: name the block worn,
//!    exclude it, rebuild the chain elsewhere.
//! 2. `stream_ctz_extend` step 1, the copy on write rebuild of a dirty
//!    tail: exclude the fresh candidate and allocate another.
//! 3. `stream_ctz_extend` step 2, the overflow blocks: exclude the block
//!    and reallocate the whole new set.
//! 4. `stream_ctz_extend` step 3, the in place fill of the committed
//!    tail: report `Io`, exactly as a program failure does there. The
//!    overflow blocks written in step 2 already carry skip pointers to
//!    the old tail address, so the tail cannot relocate this late; the
//!    committed state is untouched and the next append's dirty check
//!    routes the retry through copy on write.
//! 5. `shrink_ctz_head`, the partial tail relocation: report `Io`,
//!    exactly as a program failure does there. Only a fresh block was
//!    written, so the committed file is untouched.
//!
//! `LyingDev` models the failure: programs to a designated block report
//! success and flip one bit of the region just written.

use littlefs2_pure::{Error, Fs, OpenOptions, Path, Storage};

const BS: usize = 256;
const BC: u32 = 64;

/// Retry bound the kernel applies to worn block exclusion
/// (`fs::MAX_BAD_BLOCK_RETRIES`, private). Mirrored here so the
/// boundedness test states the number it is pinning.
const MAX_BAD_BLOCK_RETRIES: usize = 8;

/// Device whose programs to a designated block report success and land
/// one flipped bit. Reads and erases pass through honestly: this models
/// a cell that no longer holds what was written, not a chip that
/// reports failures.
struct LyingDev {
    data: Vec<u8>,
    /// Blocks that corrupt every program they accept.
    bad: Vec<u32>,
    /// Programs that were actually corrupted. Every test asserts this
    /// is nonzero, so a future change in allocation order turns a test
    /// vacuous loudly rather than silently.
    corruptions: usize,
    /// Total program calls, for the boundedness assertion.
    programs: usize,
}

impl LyingDev {
    fn new() -> Self {
        Self { data: vec![0xFFu8; BS * BC as usize], bad: Vec::new(), corruptions: 0, programs: 0 }
    }
}

impl Storage for LyingDev {
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
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(bytes.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(bytes);
        if self.bad.contains(&block) && !bytes.is_empty() {
            // Flip a bit in the middle of the region rather than at its
            // start. The first bytes of a CTZ block are its skip pointer
            // header, and a corrupted pointer there is not always
            // dereferenced by a read (a higher block often carries a
            // direct pointer that skips over it), which would let a test
            // pass without the kernel noticing anything. The midpoint is
            // file content at every site under test, so a miss is
            // observable in the bytes the file reads back.
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

/// Format an honest device and hand it back. Formatting has to succeed
/// before any block starts lying, otherwise the superblock's own read
/// back (review H2) fails first and the test never reaches the file
/// data path.
fn formatted() -> LyingDev {
    let mut dev = LyingDev::new();
    let mut sb = buf();
    Fs::format(&mut dev, &mut sb).unwrap();
    dev
}

fn mount(dev: LyingDev) -> Fs<LyingDev> {
    let mut a = buf();
    let mut b = buf();
    Fs::mount(dev, &mut a, &mut b).unwrap()
}

/// Site 1, `try_build_ctz_chain`. After format the allocator hands out
/// block 2 first, so a 300 byte file lays its chain across blocks 2 and
/// 3 and block 2 is the one that lies. The read back must name it worn,
/// exclude it, and rebuild the chain on good blocks; the content then
/// reads back byte for byte, here and after a remount.
#[test]
fn initial_ctz_write_relocates_past_a_lying_block() {
    let mut dev = formatted();
    dev.bad.push(2);
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    let content = [0xA5u8; 300];
    fs.write_to_path(p("/f"), &content, &mut a, &mut b)
        .expect("the initial CTZ write must relocate past a lying block");

    let mut out = [0u8; 300];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert_eq!(out, content, "content must survive in the mounted handle");

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert_eq!(out, content, "content must survive a remount");
}

/// Site 3, the overflow blocks of `stream_ctz_extend`. A 200 byte file
/// occupies block 2 alone; appending 500 more bytes fills that block's
/// tail in place and overflows into freshly allocated blocks, the first
/// of which is block 3. Block 3 lies, so the read back must exclude it
/// and reallocate the whole new set.
#[test]
fn append_overflow_relocates_past_a_lying_block() {
    let dev = formatted();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(p("/f"), &[0x11u8; 200], &mut a, &mut b).unwrap();

    let mut dev = fs.into_storage();
    dev.bad.push(3);
    let mut fs = mount(dev);
    let mut scratch = [0u8; 1024];
    fs.append_to_path(p("/f"), &[0x22u8; 500], &mut scratch, &mut a, &mut b)
        .expect("the append must relocate past a lying overflow block");

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    let mut out = [0u8; 700];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 700);
    assert!(out[..200].iter().all(|&x| x == 0x11), "committed prefix intact");
    assert!(out[200..].iter().all(|&x| x == 0x22), "appended bytes intact");
}

/// Site 2, the copy on write rebuild of a dirty tail. Residue in the
/// tail's fill region (what a previously torn append leaves behind, and
/// what no metadata records) routes the append through a rebuild onto a
/// fresh block instead of an in place fill. That fresh block is the
/// first the allocator hands out, block 3, and it lies; the read back
/// must exclude it and take the next candidate.
#[test]
fn dirty_tail_copy_on_write_relocates_past_a_lying_block() {
    let dev = formatted();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(p("/f"), &[0x11u8; 200], &mut a, &mut b).unwrap();

    let mut dev = fs.into_storage();
    // Residue past the committed EOF of the tail block (block 2), the
    // signature of an append torn by power loss.
    dev.data[2 * BS + 200..2 * BS + 204].fill(0x00);
    dev.bad.push(3);
    let mut fs = mount(dev);
    let mut scratch = [0u8; 1024];
    fs.append_to_path(p("/f"), &[0x22u8; 20], &mut scratch, &mut a, &mut b)
        .expect("the copy on write rebuild must relocate past a lying fresh block");

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    let mut out = [0u8; 220];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 220);
    assert!(out[..200].iter().all(|&x| x == 0x11), "committed prefix intact");
    assert!(out[200..].iter().all(|&x| x == 0x22), "appended bytes intact");
}

/// Site 4, the in place fill of the committed tail. The tail block
/// itself lies, and by the time that program runs the append can no
/// longer relocate, so the read back must surface `Io`. What matters is
/// that the failure is reported rather than swallowed: the committed
/// 200 bytes stay readable and the file's size never advances.
#[test]
fn lying_committed_tail_fill_is_an_error_not_silent_corruption() {
    let dev = formatted();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(p("/f"), &[0x11u8; 200], &mut a, &mut b).unwrap();

    let mut dev = fs.into_storage();
    dev.bad.push(2); // the file's one and only (committed) tail block
    let mut fs = mount(dev);
    let mut scratch = [0u8; 1024];
    let err = fs
        .append_to_path(p("/f"), &[0x22u8; 20], &mut scratch, &mut a, &mut b)
        .expect_err("a lying committed tail fill must not report success");
    assert_eq!(err, Error::Io);

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    assert_eq!(fs.size_of(p("/f"), &mut a, &mut b).unwrap(), 200, "the file must not have grown");
    let mut out = [0u8; 220];
    let n = fs.read_at_path(p("/f"), 0, &mut out[..200], &mut a, &mut b).unwrap();
    assert_eq!(n, 200);
    assert!(out[..200].iter().all(|&x| x == 0x11), "the committed bytes must be untouched");

    // The recovery the kernel's comment promises: the cells the failed
    // attempt did land leave the fill region dirty, so retrying the same
    // append routes through the copy on write branch onto a fresh block
    // and succeeds, even though the old tail block still lies.
    fs.append_to_path(p("/f"), &[0x22u8; 20], &mut scratch, &mut a, &mut b)
        .expect("the retry must route through copy on write and succeed");
    let dev = fs.into_storage();
    let mut fs = mount(dev);
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 220);
    assert!(out[..200].iter().all(|&x| x == 0x11), "committed prefix intact after the retry");
    assert!(out[200..].iter().all(|&x| x == 0x22), "appended bytes intact after the retry");
}

/// Site 5, `shrink_ctz_head`. Shrinking a 300 byte file to 100 bytes
/// leaves a partial tail, so the kept prefix relocates onto a freshly
/// allocated block. The chain occupies blocks 2 and 3 and is excluded,
/// so the fresh candidate is block 4, and it lies. Only that fresh
/// block was written, so the read back must surface `Io` and leave the
/// committed file exactly as it was.
#[test]
fn lying_shrink_relocation_is_an_error_not_silent_corruption() {
    let dev = formatted();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(p("/f"), &[0x33u8; 300], &mut a, &mut b).unwrap();

    let mut dev = fs.into_storage();
    dev.bad.push(4);
    let mut fs = mount(dev);
    {
        let opts = OpenOptions::new().write(true);
        let mut f = fs.open(p("/f"), opts, &mut a, &mut b).unwrap();
        let err = f
            .set_len(100, &mut a, &mut b)
            .expect_err("a lying shrink relocation must not report success");
        assert_eq!(err, Error::Io);
    }

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    let mut fs = mount(dev);
    assert_eq!(fs.size_of(p("/f"), &mut a, &mut b).unwrap(), 300, "the file must not have shrunk");
    let mut out = [0u8; 300];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert!(out.iter().all(|&x| x == 0x33), "the committed bytes must be untouched");
}

/// Progressive wear: every free block lies, so no candidate ever
/// verifies. The write must fail bounded rather than exclude and retry
/// forever. The bound is the kernel's `MAX_BAD_BLOCK_RETRIES`, and each
/// attempt programs the first chain block once before the read back
/// rejects it, so the whole write costs at most one program per attempt.
#[test]
fn wholly_lying_device_fails_bounded() {
    let mut dev = formatted();
    dev.bad = (2..BC).collect();
    dev.programs = 0; // count only what the write below costs
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();

    let err = fs
        .write_to_path(p("/f"), &[0xC3u8; 300], &mut a, &mut b)
        .expect_err("a device where nothing verifies must fail, not succeed");
    assert!(matches!(err, Error::Io | Error::OutOfRange), "got {err:?}");

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "no program was corrupted; the test proved nothing");
    // Exactly one program per attempt: the first chain block is written,
    // the read back rejects it, and the block is excluded. Eight
    // exclusions plus the initial attempt is nine. An exact count, not a
    // ceiling, so any change in the retry shape surfaces here.
    assert_eq!(
        dev.programs,
        MAX_BAD_BLOCK_RETRIES + 1,
        "the retry loop must stay bounded at one program per attempt"
    );
}
