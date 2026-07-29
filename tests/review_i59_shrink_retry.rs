//! `lfs-i59`: `shrink_ctz_head`'s partial tail relocation joins the
//! fresh block retry discipline.
//!
//! ADR-0014 gave every other fresh block path the same shape: when the
//! freshly allocated block refuses the write (or, since review H2, when
//! the read back disagrees with what was sent), exclude that candidate
//! from the allocator and try another, bounded by
//! `MAX_BAD_BLOCK_RETRIES`. `relocate_compact_to_fresh`, the split
//! continuation, the CTZ chain builder, the copy on write tail rebuild,
//! and (since `lfs-n23`) `mkdir` all follow it.
//!
//! `shrink_ctz_head` did not. It allocated one fresh block for the
//! partial tail copy on write and mapped an erase failure, a program
//! failure, or a read back mismatch straight to `Error::Io`, so a single
//! worn block sitting where the allocator happened to point made
//! `File::set_len` fail on an otherwise healthy device.
//!
//! The retry is purely additive here. Only the fresh block is written
//! before the failure: the old chain, the old tail, and the committed
//! metadata are all untouched, and the new head is not published until
//! the caller's sync commits it. An abandoned candidate is therefore an
//! unreferenced orphan the next allocator scan reclaims, exactly like
//! the continuation blocks a failed directory split leaves behind.
//!
//! The crash window is unchanged by the retry (nothing committed before,
//! nothing committed after), so this file covers only what the existing
//! shrink suites do not: the retry itself, its exclusion bookkeeping, and
//! its bound. `tests/review_shrink_append.rs` keeps the NOR correctness
//! property the relocation exists to serve.

use littlefs2_pure::{CtzStruct, Error, Fs, OpenOptions, Path, Storage};

const BS: usize = 256;
const BC: u32 = 64;

/// Retry bound the kernel applies to worn block exclusion
/// (`fs::MAX_BAD_BLOCK_RETRIES`, private). Mirrored here so the
/// boundedness test states the number it is pinning.
const MAX_BAD_BLOCK_RETRIES: usize = 8;

/// Device with two independent failure modes per block: `refuse` rejects
/// every program (a block that reports its own wear), and `lie` accepts
/// the program, reports success, and lands one flipped bit (a block that
/// does not). The relocation has to survive both the same way, because
/// review H2's read back routes them into one path.
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

fn mount(dev: WornDev) -> Fs<WornDev> {
    let mut a = buf();
    let mut b = buf();
    Fs::mount(dev, &mut a, &mut b).unwrap()
}

/// Format an honest device and lay a 300 byte file across it, so every
/// block that later starts failing fails only during the shrink under
/// test. After format the root pair is `{0, 1}`, so the file's chain
/// takes blocks 2 and 3 and the next fresh candidate is block 4.
fn with_file() -> WornDev {
    let mut dev = WornDev::new();
    let mut sb = buf();
    Fs::format(&mut dev, &mut sb).unwrap();
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.write_to_path(p("/f"), &[0x33u8; 300], &mut a, &mut b).unwrap();
    let dev = fs.into_storage();
    let chain = chain_blocks(dev);
    assert_eq!(
        chain.0,
        [2, 3],
        "test premise: the chain occupies blocks 2 and 3, got {:?}",
        chain.0
    );
    chain.1
}

/// Return the file's two chain block addresses and hand the device back.
fn chain_blocks(dev: WornDev) -> ([u32; 2], WornDev) {
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    let head = {
        let r = fs.resolve(p("/f"), &mut a, &mut b).unwrap();
        CtzStruct::from_bytes(r.struct_body).unwrap().head_block.as_u32()
    };
    // Block index 1 carries one skip pointer, at offset 0, naming block
    // index 0. Reading it directly avoids depending on a walk helper.
    let mut raw = buf();
    fs.storage_mut().read(head, 0, &mut raw).unwrap();
    let first = u32::from_le_bytes(raw[0..4].try_into().unwrap());
    ([first, head], fs.into_storage())
}

/// Read `/f`'s head block address out of its committed `CtzStruct`.
fn head_of(fs: &mut Fs<WornDev>) -> u32 {
    let mut a = buf();
    let mut b = buf();
    let r = fs.resolve(p("/f"), &mut a, &mut b).unwrap();
    CtzStruct::from_bytes(r.struct_body).unwrap().head_block.as_u32()
}

/// Shrink `/f` to 100 bytes and commit. 100 lands mid block, so the kept
/// prefix relocates copy on write onto a fresh block.
fn shrink_to_100(fs: &mut Fs<WornDev>) -> Result<(), Error> {
    let mut a = buf();
    let mut b = buf();
    let opts = OpenOptions::new().write(true);
    let mut f = fs.open(p("/f"), opts, &mut a, &mut b)?;
    f.set_len(100, &mut a, &mut b)?;
    f.close(&mut a, &mut b)
}

/// Assert the committed file is exactly `len` bytes of `byte`.
fn assert_content(dev: WornDev, len: usize, byte: u8) -> WornDev {
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    assert_eq!(fs.size_of(p("/f"), &mut a, &mut b).unwrap(), len as u32, "committed size");
    let mut out = vec![0u8; len];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, len, "bytes read back");
    assert!(out.iter().all(|&x| x == byte), "content must be {len} bytes of {byte:#04x}");
    fs.into_storage()
}

/// The fresh candidate refuses every program. The relocation must
/// exclude it and allocate another rather than failing the whole
/// `set_len`.
#[test]
fn shrink_retries_past_a_worn_first_candidate() {
    let mut dev = with_file();
    dev.refuse.push(4);
    let mut fs = mount(dev);
    shrink_to_100(&mut fs).expect("shrink must retry past a worn first candidate");

    let head = head_of(&mut fs);
    assert_ne!(head, 4, "the worn block must not become the new head");

    let dev = fs.into_storage();
    assert!(dev.refusals > 0, "the worn block was never programmed; the test proved nothing");
    let dev = assert_content(dev, 100, 0x33);

    // The relocation's whole purpose survives the retry: appending onto
    // the relocated tail lands on erased cells.
    let mut fs = mount(dev);
    let mut a = buf();
    let mut b = buf();
    fs.append_to_path(p("/f"), &[0x55u8; 20], &mut buf(), &mut a, &mut b).unwrap();
    let mut out = [0u8; 120];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 120);
    assert!(out[..100].iter().all(|&x| x == 0x33), "kept prefix");
    assert!(out[100..].iter().all(|&x| x == 0x55), "appended bytes");
}

/// The same, for a candidate that accepts the program and lands
/// corrupted cells. Review H2's read back already caught this; before
/// `lfs-i59` it reported `Io` instead of retrying, so a silently lying
/// block was fatal where a loud one now is not.
#[test]
fn shrink_retries_past_a_lying_first_candidate() {
    let mut dev = with_file();
    dev.lie.push(4);
    let mut fs = mount(dev);
    shrink_to_100(&mut fs).expect("shrink must retry past a lying first candidate");

    let head = head_of(&mut fs);
    assert_ne!(head, 4, "the lying block must not become the new head");

    let dev = fs.into_storage();
    assert!(dev.corruptions > 0, "the lying block was never programmed; the test proved nothing");
    assert_content(dev, 100, 0x33);
}

/// A candidate whose erase fails is worn too, and takes the same path.
#[test]
fn shrink_retries_past_several_worn_candidates() {
    let mut dev = with_file();
    dev.refuse.extend_from_slice(&[4, 5, 6]);
    dev.lie.push(7);
    let mut fs = mount(dev);
    shrink_to_100(&mut fs).expect("shrink must retry past several worn candidates");

    let head = head_of(&mut fs);
    for bad in [4u32, 5, 6, 7] {
        assert_ne!(head, bad, "worn block {bad} must not become the new head");
    }
    let dev = fs.into_storage();
    assert!(dev.refusals > 0 && dev.corruptions > 0, "both failure modes must have fired");
    assert_content(dev, 100, 0x33);
}

/// A device where no fresh block accepts a write must fail bounded. The
/// retry stops after `MAX_BAD_BLOCK_RETRIES` exclusions, so the whole
/// call costs exactly one relocation program per attempt; it never
/// spins. An exact count, not a ceiling, so any change in the retry
/// shape surfaces here.
#[test]
fn shrink_on_a_wholly_worn_device_fails_bounded() {
    let mut dev = with_file();
    dev.refuse = (4..BC).collect();
    dev.programs = 0; // count only what the shrink below costs
    let mut fs = mount(dev);
    let err = shrink_to_100(&mut fs).expect_err("a wholly worn device must fail the shrink");
    assert!(matches!(err, Error::Io | Error::OutOfRange), "got {err:?}");

    let dev = fs.into_storage();
    assert!(dev.refusals > 0, "no program was refused; the test proved nothing");
    assert_eq!(
        dev.programs,
        MAX_BAD_BLOCK_RETRIES + 1,
        "the retry loop must stay bounded at one program per attempt"
    );

    // The failed shrink left the committed file exactly as it was.
    assert_content(dev, 300, 0x33);
}
