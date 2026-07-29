//! Review follow up `lfs-8e6` (discovered from M7, `lfs-mqz`): the CTZ
//! **read** path must issue only `READ_SIZE` aligned reads.
//!
//! The `Storage` contract requires every read to start at a `READ_SIZE`
//! aligned offset and to cover a `READ_SIZE` multiple of bytes.
//! `ctz::collect_chain_blocks` fetched a chain block's skip pointer
//! header with a bare 4 or 8 byte read at offset 0, `ctz::seek_block`
//! read 4 bytes at offset `4*k`, and `ctz::read_ctz_at` read a block's
//! content span at offset `header + skip` for exactly the number of
//! bytes the caller wanted. None of the three honored the precondition:
//! they fault on hardware that enforces read alignment. `MemStorage`
//! tolerates any offset and length, which is why the existing suites
//! never caught it (the same blind spot M7 had on the allocator walk).
//!
//! Every test here drives a real filesystem over a storage that asserts
//! both alignment conditions on every read, so any surviving sub
//! `READ_SIZE` read in the CTZ read path panics with the offending
//! block, offset, and length.

use littlefs2_pure::ctz::{block_count, collect_chain_blocks, seek_block, MAX_CTZ_BLOCKS};
use littlefs2_pure::{BlockAddress, Fs, OpenOptions, Path, SeekFrom, Storage};

/// A storage that enforces the read alignment precondition the trait
/// documents. Programs are deliberately *not* checked: the kernel emits
/// byte granular programs on purpose and `NorAlignedStorage` is the
/// documented adapter for devices that cannot take them.
struct AlignedOnly {
    data: Vec<u8>,
}

impl AlignedOnly {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    const READ_SIZE: usize = 16;

    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}

impl Storage for AlignedOnly {
    type Error = ();
    const READ_SIZE: usize = Self::READ_SIZE;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
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

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// Deterministic content with no run of repeated bytes, so a read that
/// lands on the wrong offset shows up as a value mismatch.
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// A freshly formatted filesystem holding `/f` with `len` bytes of
/// [`body`] content, returned already remounted (so nothing is cached
/// from the write).
fn fs_with_file(len: usize) -> (Fs<AlignedOnly>, Vec<u8>) {
    let content = body(len);
    let mut storage = AlignedOnly::new();
    let mut scratch = [0u8; AlignedOnly::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut ba = [0u8; AlignedOnly::BLOCK_SIZE];
        let mut bb = [0u8; AlignedOnly::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
        let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
        fs.write_to_path(p("/f"), &content, &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    let mut ba = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut bb = [0u8; AlignedOnly::BLOCK_SIZE];
    let fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    (fs, content)
}

/// `Fs::read_at_path` from offset zero: exercises `collect_chain_blocks`
/// (the skip pointer headers) and the whole content reassembly loop in
/// `read_ctz_at`.
#[test]
fn whole_file_read_is_aligned() {
    let (mut fs, content) = fs_with_file(700);
    let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut out = vec![0u8; content.len()];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, content.len());
    assert_eq!(out, content, "read back content differs from what was written");
}

/// Reads whose start offset, end offset, or both fall inside a
/// `READ_SIZE` window: the head fragment, the tail fragment, and the
/// fully interior case must all be assembled through aligned device
/// reads.
#[test]
fn offset_reads_are_aligned() {
    let (mut fs, content) = fs_with_file(700);
    let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
    for &(off, len) in
        &[(1usize, 1usize), (3, 5), (13, 100), (255, 2), (250, 300), (0, 699), (699, 1)]
    {
        let mut out = vec![0u8; len];
        let n = fs.read_at_path(p("/f"), off as u32, &mut out, &mut a, &mut b).unwrap();
        let want = &content[off..(off + len).min(content.len())];
        assert_eq!(n, want.len(), "short read at offset {off} length {len}");
        assert_eq!(&out[..n], want, "wrong bytes at offset {off} length {len}");
    }
}

/// The stateful handle read path (`File::read` after a seek) reaches
/// `read_ctz_at` with a non zero start offset.
#[test]
fn file_handle_read_after_seek_is_aligned() {
    let (mut fs, content) = fs_with_file(700);
    let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
    let opts = OpenOptions::new().read(true);
    let mut f = fs.open(p("/f"), opts, &mut a, &mut b).unwrap();
    f.seek(SeekFrom::Start(37)).unwrap();
    let mut out = [0u8; 300];
    let n = f.read(&mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 300);
    assert_eq!(&out[..], &content[37..337]);
    f.close(&mut a, &mut b).unwrap();
}

/// A streaming append that overflows the tail block allocates new chain
/// blocks whose skip pointers reference existing ones, resolved by
/// `ctz::seek_block`'s skip list descent.
#[test]
fn append_seek_descent_is_aligned() {
    let (mut fs, content) = fs_with_file(700);
    let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
    let extra = body(1500);
    {
        let opts = OpenOptions::new().read(true).write(true).append(true);
        let mut f = fs.open(p("/f"), opts, &mut a, &mut b).unwrap();
        f.write(&extra, &mut a, &mut b).unwrap();
        f.close(&mut a, &mut b).unwrap();
    }
    let mut out = vec![0u8; content.len() + extra.len()];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, out.len());
    assert_eq!(&out[..content.len()], &content[..]);
    assert_eq!(&out[content.len()..], &extra[..]);
}

/// `File::set_len` shrinking to a mid block size walks the chain with
/// `collect_chain_blocks` (through `shrink_ctz_head`).
#[test]
fn set_len_chain_walk_is_aligned() {
    let (mut fs, content) = fs_with_file(700);
    let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
    {
        let opts = OpenOptions::new().read(true).write(true);
        let mut f = fs.open(p("/f"), opts, &mut a, &mut b).unwrap();
        f.set_len(400, &mut a, &mut b).unwrap();
        f.close(&mut a, &mut b).unwrap();
    }
    let mut out = vec![0u8; 400];
    let n = fs.read_at_path(p("/f"), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 400);
    assert_eq!(&out[..], &content[..400]);
}

/// The two low level chain primitives called directly, the way an
/// embedder walking a chain would call them: every read they issue must
/// honor the precondition on its own, not only when reached through
/// `read_ctz_at`.
#[test]
fn low_level_chain_primitives_are_aligned() {
    let (fs, content) = fs_with_file(2000);
    let mut a = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut b = [0u8; AlignedOnly::BLOCK_SIZE];
    let mut fs = fs;
    let (head, size) = {
        let r = fs.resolve(p("/f"), &mut a, &mut b).unwrap();
        let body = r.struct_body;
        (
            u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        )
    };
    assert_eq!(size as usize, content.len());
    let mut storage = fs.into_storage();

    let n = block_count(size, AlignedOnly::BLOCK_SIZE as u32);
    assert!(n >= 8, "chain too short to exercise multi level skips: {n}");

    let mut chain = [BlockAddress::NONE; MAX_CTZ_BLOCKS];
    collect_chain_blocks(&mut storage, BlockAddress::new(head), n, &mut chain[..n as usize])
        .unwrap();

    for target in 0..n {
        let got = seek_block(&mut storage, BlockAddress::new(head), n - 1, target).unwrap();
        assert_eq!(got, chain[target as usize], "seek to index {target} disagreed with the walk");
    }
}
