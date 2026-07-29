//! `lfs-6ym`: a read back must be answered by the device, not by the
//! alignment adapter's cache.
//!
//! [`NorAlignedStorage`] splices its cached program window into reads so
//! a caller sees its own not yet flushed bytes. Review H2's read back
//! (and `lfs-ttr`'s file data extension of it) programs a region and
//! immediately re reads it to catch a device that reports success and
//! lands corrupted cells. Behind the adapter those two behaviors
//! collided: the last `PROG_SIZE` window of any programmed region is
//! still sitting dirty in the adapter's cache when the verify read is
//! issued, so the verify compared RAM against RAM and passed whatever
//! the device did.
//!
//! The exposure is not hypothetical and not narrow. Every verify site
//! ends its program in a dirty window, so every one of them had a blind
//! tail; when the verified region fits in a single window the whole
//! verify was blind. Worse, `sync` does not close it: the adapter keeps
//! the window resident after flushing it and keeps splicing, so the
//! device's own bytes stay hidden until the window is switched.
//!
//! ADR-0020 records the decision. The fix is the C reference's shape:
//! `lfs_bd_flush`'s validating compare drops the read cache and passes a
//! null program cache so the comparison reads the device
//! (lfs.c, `lfs_bd_flush` / `lfs_bd_cmp`). Here the verify helpers call
//! [`Storage::read_device`], a defaulted trait method that means "tell
//! me what the device holds"; the adapter overrides it to flush the
//! pending window and read through.
//!
//! Each test below fails on the pre `lfs-6ym` kernel: the corruption
//! lands, the call reports success, and the damage surfaces only at the
//! next mount.

use littlefs2_pure::{CtzStruct, Error, Fs, NorAlignedStorage, Path, Storage};

const BS: usize = 256;
const BC: u32 = 64;
const PS: usize = 16;

/// A device with one worn program page.
///
/// `worn` names a `(block, page offset)` window. A program that touches
/// it lands, then the lowest set bit of the window's first non zero byte
/// fails to hold and reads back as zero. That is a `1 -> 0` flip, so the
/// device never violates NOR semantics; it simply does not hold what it
/// was told, and it says nothing about it. Reads and erases are honest.
///
/// `armed` gates the wear so a test can lay down healthy state first and
/// only then start the page failing.
struct PageLiar {
    data: Vec<u8>,
    worn: Option<(u32, u32)>,
    armed: bool,
    /// Programs that actually lost a bit, so no test can pass vacuously.
    corruptions: usize,
}

impl PageLiar {
    fn new() -> Self {
        Self { data: vec![0xFFu8; BS * BC as usize], worn: None, armed: false, corruptions: 0 }
    }
}

impl Storage for PageLiar {
    type Error = ();
    const READ_SIZE: usize = PS;
    const PROG_SIZE: usize = PS;
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
        let s = (block as usize) * BS + off as usize;
        let e = s.checked_add(bytes.len()).ok_or(())?;
        if block >= BC || e > self.data.len() {
            return Err(());
        }
        // NOR semantics: a program only clears bits.
        for (d, v) in self.data[s..e].iter_mut().zip(bytes) {
            *d &= *v;
        }
        if self.armed && self.worn == Some((block, off)) {
            for i in s..e {
                let v = self.data[i];
                if v != 0 {
                    self.data[i] = v & (v - 1);
                    self.corruptions += 1;
                    break;
                }
            }
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

type Dev = NorAlignedStorage<PageLiar>;

fn buf() -> [u8; BS] {
    [0u8; BS]
}

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// Format an honest device behind the alignment adapter.
fn formatted() -> Dev {
    let mut dev = NorAlignedStorage::new(PageLiar::new()).unwrap();
    let mut sb = buf();
    Fs::format(&mut dev, &mut sb).unwrap();
    dev
}

fn mount(dev: Dev) -> Fs<Dev> {
    let mut a = buf();
    let mut b = buf();
    Fs::mount(dev, &mut a, &mut b).unwrap()
}

/// Arm the worn page. Consumes and returns the device so the adapter's
/// own state is never disturbed mid operation.
fn arm(dev: Dev, block: u32, page: u32) -> Dev {
    let mut inner = dev.into_inner().unwrap();
    inner.worn = Some((block, page));
    inner.armed = true;
    NorAlignedStorage::new(inner).unwrap()
}

/// The blind window of a CTZ chain block. After format the allocator
/// hands out block 4 for a 300 byte file's first chain block (blocks 2
/// and 3 went to the `/d` directory pair the setup makes, keeping the
/// arithmetic explicit). The kernel programs the whole block and then
/// verifies it; the adapter has flushed every window but the last, so
/// only page 240 is answered from RAM. A worn cell there used to survive
/// the verify and corrupt the user's bytes silently.
#[test]
fn worn_page_in_a_chain_block_is_caught_at_the_verify_not_at_the_next_mount() {
    let dev = formatted();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
    let dev = arm(fs.into_storage(), 4, 240);

    let mut fs = mount(dev);
    fs.write_to_path(p("/f"), &[0x33u8; 300], &mut a, &mut b)
        .expect("the healthy blocks left on the device must carry the file");

    let first = {
        let r = fs.resolve(p("/f"), &mut a, &mut b).unwrap();
        let head = CtzStruct::from_bytes(r.struct_body).unwrap().head_block.as_u32();
        let mut raw = buf();
        fs.storage_mut().read(head, 0, &mut raw).unwrap();
        u32::from_le_bytes(raw[0..4].try_into().unwrap())
    };
    assert_ne!(first, 4, "the worn block must not carry committed file content");

    let dev = fs.into_storage();
    assert!(dev.inner().corruptions > 0, "the worn page never fired; the test proved nothing");

    // The verdict that matters: the bytes on the device are the bytes
    // the caller wrote. Before `lfs-6ym` the write reported success and
    // this read came back with bytes 240..256 quietly altered.
    let mut fs = mount(dev);
    let mut out = [0u8; 300];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert!(
        out.iter().all(|&x| x == 0x33),
        "committed file content must equal what was written; first bad byte at {:?}",
        out.iter().position(|&x| x != 0x33)
    );
}

/// The metadata counterpart. `mkdir`'s init commit lands on block 2 and
/// spans two program windows; the verify reads both, but page 16 has not
/// reached the device yet, so a worn cell there used to pass the verify
/// and leave an unreadable directory pair behind.
#[test]
fn worn_page_in_a_metadata_commit_is_caught_at_the_verify() {
    let dev = arm(formatted(), 2, 16);
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(p("/d"), &mut a, &mut b).expect("mkdir must land on a healthy pair");

    let pair = {
        let r = fs.resolve(p("/d"), &mut a, &mut b).unwrap();
        assert_eq!(r.struct_body.len(), 8);
        let lo = u32::from_le_bytes(r.struct_body[0..4].try_into().unwrap());
        let hi = u32::from_le_bytes(r.struct_body[4..8].try_into().unwrap());
        (lo, hi)
    };
    assert_ne!(pair.0, 2, "the worn block must not become the directory's active half");

    let dev = fs.into_storage();
    assert!(dev.inner().corruptions > 0, "the worn page never fired; the test proved nothing");

    // Before `lfs-6ym` the mkdir reported success and the directory was
    // only discovered broken here, one mount later.
    let mut fs = mount(dev);
    fs.write_to_path(p("/d/f"), b"hello", &mut a, &mut b)
        .expect("the committed directory must be usable");
    let dev = fs.into_storage();
    let mut fs = mount(dev);
    let mut out = [0u8; 8];
    let n = fs.read_at_path(p("/d/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"hello");
}

/// The file data tail fill, the one verify site with no relocation left
/// to take (`lfs-ttr` site 4: the overflow blocks already carry skip
/// pointers to this tail's address, so it cannot move). Its verdict is
/// `Io`, and the committed prefix stays readable. That verdict was
/// unreachable behind the adapter: the fill's last window was the dirty
/// one, so the verify passed and the caller's bytes rotted in place.
#[test]
fn worn_page_under_a_tail_fill_reports_io_instead_of_corrupting() {
    let dev = formatted();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
    fs.write_to_path(p("/f"), &[0x11u8; 200], &mut a, &mut b).unwrap();
    // The 200 bytes live in block 4 alone; appending 20 more fills the
    // tail in place across pages 192 and 208, and page 208 is the one
    // the adapter still holds when the verify runs.
    let dev = arm(fs.into_storage(), 4, 208);

    let mut fs = mount(dev);
    let err = fs
        .append_to_path(p("/f"), &[0xAAu8; 20], &mut buf(), &mut a, &mut b)
        .expect_err("a worn cell under the fill must be reported, not committed");
    assert_eq!(err, Error::Io);

    let dev = fs.into_storage();
    assert!(dev.inner().corruptions > 0, "the worn page never fired; the test proved nothing");

    let mut fs = mount(dev);
    assert_eq!(fs.size_of(p("/f"), &mut a, &mut b).unwrap(), 200, "the file must not have grown");
    let mut out = [0u8; 200];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 200);
    assert!(out.iter().all(|&x| x == 0x11), "the committed prefix must be untouched");
}

/// The adapter contract itself, stated without the kernel in the way.
///
/// [`Storage::read`] keeps splicing (callers depend on seeing their own
/// pending bytes, and the kernel's readers must see a commit it has not
/// synced yet). [`Storage::read_device`] answers from the device, after
/// flushing whatever pending bytes overlap the request so the question
/// is a fair one.
#[test]
fn read_device_answers_from_the_device_and_read_still_splices() {
    let mut liar = PageLiar::new();
    liar.worn = Some((3, 0));
    liar.armed = true;
    let mut dev = NorAlignedStorage::new(liar).unwrap();
    dev.erase(3).unwrap();
    dev.program(3, 0, &[0xAAu8; PS]).unwrap();

    // Nothing has reached the device yet, so the splice is the only
    // source of these bytes.
    let mut spliced = [0u8; PS];
    dev.read(3, 0, &mut spliced).unwrap();
    assert_eq!(spliced, [0xAAu8; PS], "read must keep serving pending bytes");

    // The device's answer: the flush happens first, the worn cell eats a
    // bit, and the caller is told.
    let mut truth = [0u8; PS];
    dev.read_device(3, 0, &mut truth).unwrap();
    assert_eq!(truth[0], 0xA8, "read_device must report the device's byte, not the cache's");
    assert_eq!(&truth[1..], &[0xAAu8; PS - 1]);

    // And it stays truthful once the window is clean but still resident,
    // which is where a plain sync leaves it.
    dev.sync().unwrap();
    let mut after_sync = [0u8; PS];
    dev.read_device(3, 0, &mut after_sync).unwrap();
    assert_eq!(after_sync, truth, "a clean resident window must not be spliced either");
    assert_eq!(dev.inner().corruptions, 1, "exactly one flush reached the worn page");
}

/// `read_device` must not disturb a pending window that has nothing to
/// do with the region being asked about: an unrelated block's bytes stay
/// buffered, so the adapter's write batching is untouched.
#[test]
fn read_device_leaves_an_unrelated_pending_window_alone() {
    let mut dev = NorAlignedStorage::new(PageLiar::new()).unwrap();
    dev.erase(3).unwrap();
    dev.erase(4).unwrap();
    dev.program(4, 0, &[0x0Fu8; PS]).unwrap();

    let mut truth = [0u8; PS];
    dev.read_device(3, 0, &mut truth).unwrap();
    assert_eq!(truth, [0xFFu8; PS], "block 3 was never programmed");
    assert_eq!(
        &dev.inner().data[4 * BS..4 * BS + PS],
        &[0xFFu8; PS],
        "block 4's pending window must not have been flushed early"
    );

    dev.sync().unwrap();
    assert_eq!(&dev.inner().data[4 * BS..4 * BS + PS], &[0x0Fu8; PS], "sync still flushes it");
}
