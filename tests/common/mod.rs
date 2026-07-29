//! Shared helpers for the property test files. Lives at `tests/common/mod.rs`
//! so each `tests/property_*.rs` file can pull it in via `mod common;` without
//! Cargo treating it as a separate integration test binary.
//!
//! The pattern is borrowed from ferrodec's testing layout. Helpers here are
//! split across consumers, so a blanket `#[allow(dead_code)]` keeps unused
//! warnings off in files that import only one helper.

#![allow(dead_code)]

use littlefs2_pure::ctz::{block_count, content_bytes_in_block, skip_pointers_in_block, CtzStruct};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType, ID_NONE};
use littlefs2_pure::{crc, BlockAddress, Fs, NorAlignedStorage};

/// Test only metadata block builder.
///
/// Constructs valid LittleFS v2 metadata blocks from a list of tags grouped
/// into commits. The output byte layout matches what the [`MetadataReader`]
/// in the crate expects: little endian revision counter at offset 0, then a
/// sequence of XOR encoded big endian tags with CCRC commit terminators,
/// then `0xFF` padding to the end of the buffer.
///
/// The builder mirrors the algorithm in `lfs_dir_commit` (C reference) to
/// the extent needed for read path testing: it does not implement compaction
/// or wear leveling, which are write kernel concerns.
///
/// # Example
///
/// ```ignore
/// let mut builder = BlockBuilder::new(512, 1).unwrap();
/// builder.tag(Tag::new(true, TagType::Superblock, 0, 8), b"littlefs").unwrap();
/// builder.commit(0).unwrap();
/// let block = builder.finish();
/// ```
///
/// [`MetadataReader`]: littlefs2_pure::meta::MetadataReader
pub struct BlockBuilder {
    buf: alloc::vec::Vec<u8>,
    offset: usize,
    /// XOR base for the next tag, i.e. the most recently emitted tag
    /// (possibly with CCRC parity flip applied).
    ptag: u32,
    /// Running CRC accumulator for the in progress commit.
    crc: u32,
}

extern crate alloc;

impl BlockBuilder {
    /// Create a builder with `block_size` capacity. The first 4 bytes hold
    /// `revision` as little endian; the remainder is initialized to `0xFF`
    /// (the erased flash state).
    pub fn new(block_size: usize, revision: u32) -> Result<Self, &'static str> {
        if block_size < 4 {
            return Err("block_size must be at least 4 bytes for the revision header");
        }
        let mut buf = alloc::vec![0xFFu8; block_size];
        buf[0..4].copy_from_slice(&revision.to_le_bytes());
        let crc = crc::update(crc::INIT, &buf[0..4]);
        Ok(Self { buf, offset: 4, ptag: 0xFFFF_FFFF, crc })
    }

    /// Append a non CCRC tag to the current (in progress) commit.
    ///
    /// The body must match `tag.body_len()`. Returns an error if the tag
    /// would overflow the block.
    pub fn tag(&mut self, tag: Tag, body: &[u8]) -> Result<(), &'static str> {
        if body.len() != tag.body_len() {
            return Err("body length does not match tag's length field");
        }
        if tag.is_ccrc() {
            return Err("use commit() to emit a CCRC; tag() is for data tags");
        }
        let dsize = tag.dsize();
        if self.offset + dsize > self.buf.len() {
            return Err("not enough room in block for this tag");
        }

        let raw = tag.into_bits() ^ self.ptag;
        self.buf[self.offset..self.offset + 4].copy_from_slice(&raw.to_be_bytes());
        self.crc = crc::update(self.crc, &raw.to_be_bytes());
        if !body.is_empty() {
            self.buf[self.offset + 4..self.offset + dsize].copy_from_slice(body);
            self.crc = crc::update(self.crc, body);
        }
        self.ptag = tag.into_bits();
        self.offset += dsize;
        Ok(())
    }

    /// Finalize the current commit by emitting a CCRC tag with the given
    /// `chunk` byte. Subsequent calls to `tag()` start a new commit.
    pub fn commit(&mut self, chunk: u8) -> Result<(), &'static str> {
        let ccrc_tag = Tag::new(true, TagType::CommitCrc(chunk), ID_NONE, 4);
        if self.offset + 8 > self.buf.len() {
            return Err("not enough room in block for a CCRC");
        }
        let raw = ccrc_tag.into_bits() ^ self.ptag;
        self.buf[self.offset..self.offset + 4].copy_from_slice(&raw.to_be_bytes());
        self.crc = crc::update(self.crc, &raw.to_be_bytes());
        self.buf[self.offset + 4..self.offset + 8].copy_from_slice(&self.crc.to_le_bytes());
        // After a successful CCRC, ptag's bit 31 is XORed with chunk parity
        // so the next commit's first tag XOR decodes correctly.
        self.ptag = ccrc_tag.into_bits() ^ ((u32::from(chunk) & 1) << 31);
        self.crc = crc::INIT;
        self.offset += 8;
        Ok(())
    }

    /// Bytes used so far (including the revision header).
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Consume the builder and return the underlying buffer.
    pub fn finish(self) -> alloc::vec::Vec<u8> {
        self.buf
    }
}

/// In memory [`Storage`] backing that enforces strict NOR flash
/// semantics: programs may only start at `PROG_SIZE` aligned addresses,
/// must cover a `PROG_SIZE` window, and may only flip bits from `1` to
/// `0`. Any violation panics — tests using this storage assert that
/// the kernel + `NorAlignedStorage` wrapper produce NOR-compliant
/// programs.
#[derive(Debug)]
pub struct StrictNorStorage {
    pub data: alloc::vec::Vec<u8>,
}

impl StrictNorStorage {
    pub const READ_SIZE: usize = 16;
    pub const PROG_SIZE: usize = 16;
    pub const BLOCK_SIZE: usize = 256;
    pub const BLOCK_COUNT: u32 = 8;
    pub const CACHE_SIZE: usize = 64;
    pub const LOOKAHEAD_SIZE: usize = 8;

    pub fn new() -> Self {
        Self { data: alloc::vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}

impl Default for StrictNorStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for StrictNorStorage {
    type Error = ();
    const READ_SIZE: usize = Self::READ_SIZE;
    const PROG_SIZE: usize = Self::PROG_SIZE;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = Self::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = Self::LOOKAHEAD_SIZE;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        assert_eq!(
            off as usize % Self::PROG_SIZE,
            0,
            "NOR program must be PROG_SIZE-aligned, got off={off}"
        );
        assert_eq!(
            data.len() % Self::PROG_SIZE,
            0,
            "NOR program must be a PROG_SIZE multiple, got len={}",
            data.len()
        );
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        // NOR-only semantics: only 1 -> 0 transitions allowed.
        for (existing, &new) in self.data[start..start + data.len()].iter().zip(data) {
            assert_eq!(
                *existing & new,
                new,
                "NOR program flipped a 0 bit to 1 (existing={existing:#x}, new={new:#x})"
            );
        }
        for (existing, &new) in self.data[start..start + data.len()].iter_mut().zip(data) {
            *existing &= new;
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        if end > self.data.len() {
            return Err(());
        }
        for b in &mut self.data[start..end] {
            *b = 0xFF;
        }
        Ok(())
    }
}

/// A device that can also model a program call which lands only part of
/// its window before the power goes away.
///
/// [`Storage::program`] takes a whole aligned window and either lands it
/// or does not; a real NOR device interrupted mid program leaves the
/// window part written. Modelling that needs a byte granular back door
/// that skips the device's alignment precondition while keeping the
/// electrical rule (a program only clears bits), so it lives on this
/// separate trait rather than widening the `Storage` contract that the
/// kernel is written against.
pub trait PartialProgram: Storage {
    /// Land `data` at `(block, off)` with NOR AND semantics, bypassing
    /// the alignment precondition [`Storage::program`] enforces. Cells
    /// outside `data` keep whatever they held: `0xFF` where the block
    /// has not been programmed since its erase, the previously
    /// programmed bits otherwise.
    fn program_partial(&mut self, block: u32, off: u32, data: &[u8]);
}

impl PartialProgram for StrictNorStorage {
    fn program_partial(&mut self, block: u32, off: u32, data: &[u8]) {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        let end = start + data.len();
        assert!(end <= self.data.len(), "partial landing runs past the end of the device");
        // Same NOR rule as the full program path: the bytes that do land
        // may only clear bits. A partial landing is a prefix of a program
        // the kernel asked for, so a violation here is the same kernel bug
        // `program` asserts on, caught one byte earlier.
        for (existing, &new) in self.data[start..end].iter().zip(data) {
            assert_eq!(
                *existing & new,
                new,
                "partial NOR landing flipped a 0 bit to 1 (existing={existing:#x}, new={new:#x})"
            );
        }
        for (existing, &new) in self.data[start..end].iter_mut().zip(data) {
            *existing &= new;
        }
    }
}

/// In memory [`Storage`] backing for the integration tests.
///
/// Holds `BLOCK_SIZE * BLOCK_COUNT` bytes in a `Vec` and implements the
/// read / program / erase contract against that buffer. Geometry constants
/// are baked into the type so the trait's associated consts can refer to
/// them; tests pick a small fixed geometry (256 byte blocks, 8 total
/// blocks) sufficient to host the foundational fixtures.
///
/// The implementation deliberately does not enforce NOR flash semantics
/// (program may only flip `1` to `0`) because the read kernel does not
/// depend on that constraint. The write kernel landing in Phase 2 will
/// One zeroed metadata-block buffer sized to the [`MemStorage`]
/// geometry. The test suite needs a `[u8; MemStorage::BLOCK_SIZE]`
/// scratch buffer for almost every `Fs` call (the `buf_a`/`buf_b`
/// pair, plus assorted `scratch`); spelled out, that literal repeated
/// over 500 times across the suite. Funnel it through one helper so
/// the geometry is named in exactly one place.
#[must_use]
pub fn make_buffer() -> [u8; MemStorage::BLOCK_SIZE] {
    [0u8; MemStorage::BLOCK_SIZE]
}

/// upgrade this to a stricter model.
#[derive(Debug)]
pub struct MemStorage {
    pub data: alloc::vec::Vec<u8>,
}

impl MemStorage {
    pub const READ_SIZE: usize = 16;
    pub const PROG_SIZE: usize = 16;
    pub const BLOCK_SIZE: usize = 256;
    pub const BLOCK_COUNT: u32 = 8;
    pub const CACHE_SIZE: usize = 64;
    pub const LOOKAHEAD_SIZE: usize = 8;

    pub fn new() -> Self {
        Self { data: alloc::vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }

    pub fn write_block(&mut self, block: u32, bytes: &[u8]) {
        let start = (block as usize) * Self::BLOCK_SIZE;
        self.data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

impl Default for MemStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for MemStorage {
    type Error = ();
    const READ_SIZE: usize = Self::READ_SIZE;
    const PROG_SIZE: usize = Self::PROG_SIZE;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = Self::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = Self::LOOKAHEAD_SIZE;

    // Bounds checks mirror the fuzzer's `ImageStorage`: an explicit
    // `block >= BLOCK_COUNT` reject and checked arithmetic so an
    // out-of-range or overflowing block address (an adversarial CTZ
    // skip pointer or pair link) returns `Err(())` rather than
    // wrapping into a valid-looking offset or panicking on a slice
    // index. This honors the `Storage` contract and lets a test
    // observe the kernel's clean reject of such an address.
    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize)
            .checked_mul(Self::BLOCK_SIZE)
            .and_then(|b| b.checked_add(off as usize))
            .ok_or(())?;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize)
            .checked_mul(Self::BLOCK_SIZE)
            .and_then(|b| b.checked_add(off as usize))
            .ok_or(())?;
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
        let start = (block as usize) * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        if end > self.data.len() {
            return Err(());
        }
        for b in &mut self.data[start..end] {
            *b = 0xFF;
        }
        Ok(())
    }
}

/// Storage adapter that simulates a power loss at a configurable
/// program-call boundary. The `trigger_at`-th `program` call (and
/// every later one) fails without writing; `erase` calls after the
/// trigger are also rejected (the bytes already on disk are
/// preserved). Reads pass through unchanged. Use
/// [`TornWriteStorage::new`] to construct.
///
/// Fidelity note (review H7 / M6): the model is *program-call*
/// granularity over a plain [`MemStorage`]. The inner storage does
/// NOT enforce NOR semantics (it permits re-programming without an
/// intervening erase), and because the adapter sits outside any
/// [`littlefs2_pure::NorAlignedStorage`] wrapper, the boundaries it
/// tears at are the kernel's program calls, not device prog-window
/// landings; a partial-program landing (some bytes of one program
/// persisted) is never modeled.
///
/// [`TornPartialStorage`] is the device level counterpart added for
/// review coverage item V4 (bead `lfs-hki`): it sits INSIDE
/// `NorAlignedStorage` over a [`StrictNorStorage`], so it tears at real
/// device program boundaries and can land a prefix of the interrupted
/// window. Both models are kept, and the device one is the coarser
/// model's superset in practice: the kernel programs a whole commit
/// span in a single call, which the alignment adapter then splits into
/// `PROG_SIZE` windows, so on this geometry the device sees about four
/// programs for every one this adapter counts (measured: `Fs::format`
/// is 1 kernel call and 4 device programs; an inline write commit is 1
/// and 3).
pub struct TornWriteStorage {
    pub inner: MemStorage,
    /// Tripping point: after this many `program` calls, power is
    /// lost. The first call is `1`. Set to `usize::MAX` to disable.
    pub trigger_at: usize,
    pub program_count: usize,
}

impl TornWriteStorage {
    pub fn new(inner: MemStorage, trigger_at: usize) -> Self {
        Self { inner, trigger_at, program_count: 0 }
    }

    pub fn powered(&self) -> bool {
        self.program_count < self.trigger_at
    }

    pub fn into_inner(self) -> MemStorage {
        self.inner
    }
}

impl Storage for TornWriteStorage {
    type Error = ();
    const READ_SIZE: usize = MemStorage::READ_SIZE;
    const PROG_SIZE: usize = MemStorage::PROG_SIZE;
    const BLOCK_SIZE: usize = MemStorage::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = MemStorage::BLOCK_COUNT;
    const CACHE_SIZE: usize = MemStorage::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = MemStorage::LOOKAHEAD_SIZE;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        self.inner.read(block, off, buf)
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        self.program_count += 1;
        if !self.powered() {
            return Err(());
        }
        self.inner.program(block, off, data)
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if !self.powered() {
            return Err(());
        }
        self.inner.erase(block)
    }
}

/// One entry passed to [`build_directory_block`].
pub struct DirEntrySpec<'a> {
    pub id: u16,
    pub name: &'a [u8],
    pub name_type: TagType,
    pub struct_type: TagType,
    pub struct_body: &'a [u8],
}

/// Build a metadata block containing a directory listing.
///
/// For each entry the builder emits a NAME tag (with `name_type`
/// controlling RegularFile vs Directory) followed immediately by a
/// STRUCT tag with the given `struct_type` carrying `struct_body`.
pub fn build_directory_block(
    revision: u32,
    entries: &[DirEntrySpec<'_>],
    block_size: usize,
) -> alloc::vec::Vec<u8> {
    let mut builder = BlockBuilder::new(block_size, revision).unwrap();
    for e in entries {
        let name_tag = Tag::new(true, e.name_type, e.id, e.name.len() as u16);
        builder.tag(name_tag, e.name).unwrap();
        let struct_tag = Tag::new(true, e.struct_type, e.id, e.struct_body.len() as u16);
        builder.tag(struct_tag, e.struct_body).unwrap();
    }
    builder.commit(0).unwrap();
    builder.finish()
}

/// Build a CTZ skip list chain in `storage` and return its
/// [`CtzStruct`] header.
///
/// The chain occupies physical blocks `base_block`, `base_block + 1`,
/// ..., one per CTZ-index. Each block at CTZ index `i` carries
/// `ctz(i) + 1` little-endian `u32` skip pointers at its head followed
/// by content bytes. The pointers in block `i` address blocks
/// `i - 2^k` for `k = 0..=ctz(i)`; concretely, the physical address of
/// CTZ-index `j` is `base_block + j`.
///
/// `data` is split across blocks according to each block's content
/// capacity (`block_size - 4 * skip_pointers_in_block(i)`). The last
/// block's content may be partial.
///
/// Returns the [`CtzStruct`] whose `head_block` is the physical block
/// of the last CTZ-index and `size` equals `data.len()`.
pub fn build_ctz_chain(storage: &mut MemStorage, base_block: u32, data: &[u8]) -> CtzStruct {
    let bs = MemStorage::BLOCK_SIZE as u32;
    if data.is_empty() {
        return CtzStruct { head_block: BlockAddress::new(base_block), size: 0 };
    }
    let total = block_count(data.len() as u32, bs);
    let mut data_off = 0usize;
    for i in 0..total {
        let pointer_count = skip_pointers_in_block(i) as usize;
        let content_cap = content_bytes_in_block(i, bs) as usize;
        let phys = base_block + i;

        let mut block_buf = alloc::vec![0xFFu8; bs as usize];
        // Write skip pointers: block i has ctz(i)+1 pointers addressing
        // blocks i - 2^k for k = 0..=ctz(i). Each pointer is the
        // PHYSICAL address (base + index).
        for k in 0..pointer_count {
            let target_idx = i - (1u32 << k);
            let target_phys = base_block + target_idx;
            let off = 4 * k;
            block_buf[off..off + 4].copy_from_slice(&target_phys.to_le_bytes());
        }
        // Append content.
        let header = 4 * pointer_count;
        let content_len = content_cap.min(data.len() - data_off);
        block_buf[header..header + content_len]
            .copy_from_slice(&data[data_off..data_off + content_len]);
        data_off += content_len;

        storage.write_block(phys, &block_buf);
    }
    CtzStruct { head_block: BlockAddress::new(base_block + total - 1), size: data.len() as u32 }
}

/// Construct a single commit metadata block containing a superblock
/// (NAME magic + INLINESTRUCT geometry), matching what
/// [`Superblock::from_pair`] expects for mount.
pub fn build_superblock_block(
    sb: &littlefs2_pure::Superblock,
    block_size: usize,
) -> alloc::vec::Vec<u8> {
    let mut buf = alloc::vec![0xFFu8; block_size];
    buf[0..4].copy_from_slice(&1u32.to_le_bytes());

    let mut ptag: u32 = 0xFFFF_FFFF;
    let mut running_crc = crc::update(crc::INIT, &buf[0..4]);
    let mut off = 4usize;

    let name_tag = Tag::new(true, TagType::Superblock, 0, 8);
    let raw = name_tag.into_bits() ^ ptag;
    buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
    running_crc = crc::update(running_crc, &raw.to_be_bytes());
    buf[off + 4..off + 12].copy_from_slice(littlefs2_pure::MAGIC);
    running_crc = crc::update(running_crc, littlefs2_pure::MAGIC);
    ptag = name_tag.into_bits();
    off += 12;

    let inline_tag =
        Tag::new(true, TagType::InlineStruct, 0, littlefs2_pure::Superblock::SIZE as u16);
    let raw = inline_tag.into_bits() ^ ptag;
    buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
    running_crc = crc::update(running_crc, &raw.to_be_bytes());
    let body = sb.to_bytes();
    buf[off + 4..off + 4 + littlefs2_pure::Superblock::SIZE].copy_from_slice(&body);
    running_crc = crc::update(running_crc, &body);
    ptag = inline_tag.into_bits();
    off += 4 + littlefs2_pure::Superblock::SIZE;

    let ccrc_tag = Tag::new(true, TagType::CommitCrc(0), ID_NONE, 4);
    let raw = ccrc_tag.into_bits() ^ ptag;
    buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
    running_crc = crc::update(running_crc, &raw.to_be_bytes());
    buf[off + 4..off + 8].copy_from_slice(&running_crc.to_le_bytes());

    buf
}

/// Outcome of one torn-write sweep iteration (review H7/V3): either
/// the tear hit `Fs::format` (the device never held a complete
/// filesystem, so nothing about crash atomicity can be asserted), or
/// format completed and the image at power loss is returned.
pub enum TornRun {
    /// The tear landed inside `Fs::format`.
    TornFormat,
    /// Format completed before the tear; the device held a valid
    /// filesystem from that point on, so the returned image MUST
    /// remount. Mount via [`mount_image_strict`].
    Image(alloc::vec::Vec<u8>),
}

/// Run `scenario` against a freshly formatted filesystem with a power
/// loss at the `trigger`-th program call, counted from the first
/// program `Fs::format` issues (so the sweep range must span
/// format's calls plus the scenario's; see [`torn_call_counts`]).
///
/// Strong semantics (review H7): once format has completed, every
/// later failure is the scenario's to absorb. The post-format mount
/// happens before any tear can have landed elsewhere, so it must
/// succeed; a panic here is a harness bug, not a kernel finding.
pub fn run_torn_scenario<F>(trigger: usize, scenario: F) -> TornRun
where
    F: FnOnce(&mut littlefs2_pure::Fs<TornWriteStorage>),
{
    let mut torn = TornWriteStorage::new(MemStorage::new(), trigger);
    let mut scratch = make_buffer();
    if littlefs2_pure::Fs::format(&mut torn, &mut scratch).is_err() {
        return TornRun::TornFormat;
    }
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = littlefs2_pure::Fs::mount(torn, &mut buf_a, &mut buf_b)
        .expect("mount immediately after a completed format must succeed");
    scenario(&mut fs);
    TornRun::Image(fs.into_storage().into_inner().data)
}

/// Mount an image produced by [`run_torn_scenario`]'s `Image` arm.
///
/// This is the load-bearing assertion of every torn sweep: the image
/// held a valid filesystem before the tear, and a torn write must
/// leave it mountable as the pre-state or the post-state — an
/// unmountable image is a bricked device, the exact outcome the
/// sweeps exist to rule out. The pre-H7 sweeps silently `continue`d
/// here, so "torn write bricks the filesystem" passed.
pub fn mount_image_strict(image: alloc::vec::Vec<u8>, ctx: &str) -> littlefs2_pure::Fs<MemStorage> {
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    littlefs2_pure::Fs::mount(MemStorage { data: image }, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{ctx}: torn write left an unmountable image: {e:?}"))
}

/// `(format_calls, scenario_calls)`: how many program calls
/// `Fs::format` issues, and how many `scenario` adds after it, both
/// measured on an untorn device. A sweep covering crash atomicity for
/// the scenario must run triggers through
/// `1..=format_calls + scenario_calls + margin`; arming over the
/// scenario count alone under-covers the tail (review L10).
pub fn torn_call_counts<F>(scenario: F) -> (usize, usize)
where
    F: FnOnce(&mut littlefs2_pure::Fs<TornWriteStorage>),
{
    let mut torn = TornWriteStorage::new(MemStorage::new(), usize::MAX);
    let mut scratch = make_buffer();
    littlefs2_pure::Fs::format(&mut torn, &mut scratch).unwrap();
    let format_calls = torn.program_count;
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = littlefs2_pure::Fs::mount(torn, &mut buf_a, &mut buf_b).unwrap();
    let pre = fs.storage().program_count;
    scenario(&mut fs);
    let post = fs.storage().program_count;
    (format_calls, post - pre)
}

// ---------------------------------------------------------------------
// Device level torn sweeps (review coverage item V4, bead `lfs-hki`)
// ---------------------------------------------------------------------

/// Both test geometries agree on the block size, so one [`make_buffer`]
/// serves either composition and one scenario function can be swept
/// through both. A second geometry (review coverage item V5) must add a
/// second buffer helper rather than let this silently mismatch.
const _: () = assert!(MemStorage::BLOCK_SIZE == StrictNorStorage::BLOCK_SIZE);

/// Torn write adapter at DEVICE program granularity, for use INSIDE a
/// [`NorAlignedStorage`] wrapper (the composition
/// `NorAlignedStorage<TornPartialStorage<StrictNorStorage>>`).
///
/// [`TornWriteStorage`] tears at the kernel's own program calls, which
/// on real NOR hardware are not device operations at all: the kernel
/// hands a whole commit span to `program`, and the alignment adapter
/// splits that span into `PROG_SIZE` windows, one device program each.
/// The power cut lands between those windows, so the device model is
/// the finer of the two (measured on this geometry: `Fs::format` is 1
/// kernel program call and 4 device programs). It also reaches the case
/// a call boundary model cannot express at all: the interrupted program
/// lands only a prefix of its window.
///
/// Semantics, counting the first program as `1`:
///
/// - programs before `trigger_at` land in full;
/// - program `trigger_at` lands its first `partial_bytes` bytes (with
///   NOR AND semantics; the rest of the window keeps its current
///   contents) and then reports failure. `partial_bytes == 0` is the
///   plain boundary tear where nothing of that program lands;
/// - every later `program`, `erase`, and `sync` fails without touching
///   the device, because the power is gone. Reads keep working, as in
///   [`TornWriteStorage`]: the kernel's post tear reads cost nothing to
///   serve and its post tear writes are all refused, so what it does
///   after the tear cannot change the image the next power on sees.
///
/// The failure return models the caller observing a dead device. A real
/// power cut never returns at all, so the kernel's error path is a
/// superset of what hardware does: whatever it attempts after the tear
/// can only read, since every write is refused.
pub struct TornPartialStorage<S: PartialProgram> {
    /// The device underneath the tear injector.
    pub inner: S,
    /// Tripping point: this program call is the one power interrupts.
    /// The first call is `1`. Set to `usize::MAX` to disable.
    pub trigger_at: usize,
    /// How many bytes of the interrupted program land before the power
    /// goes away. `0` is the plain boundary tear.
    pub partial_bytes: usize,
    /// Programs seen so far, including the interrupted one.
    pub program_count: usize,
}

impl<S: PartialProgram> TornPartialStorage<S> {
    pub fn new(inner: S, trigger_at: usize, partial_bytes: usize) -> Self {
        Self { inner, trigger_at, partial_bytes, program_count: 0 }
    }

    /// Whether the device still has power. False from the interrupted
    /// program onward.
    pub fn powered(&self) -> bool {
        self.program_count < self.trigger_at
    }
}

impl<S: PartialProgram> Storage for TornPartialStorage<S>
where
    S::Error: Default,
{
    type Error = S::Error;
    const READ_SIZE: usize = S::READ_SIZE;
    const PROG_SIZE: usize = S::PROG_SIZE;
    const BLOCK_SIZE: usize = S::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = S::BLOCK_COUNT;
    const BLOCK_CYCLES: i32 = S::BLOCK_CYCLES;
    const CACHE_SIZE: usize = S::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = S::LOOKAHEAD_SIZE;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.inner.read(block, off, buf)
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), Self::Error> {
        self.program_count += 1;
        if self.powered() {
            return self.inner.program(block, off, data);
        }
        if self.program_count == self.trigger_at && self.partial_bytes > 0 {
            let landed = self.partial_bytes.min(data.len());
            self.inner.program_partial(block, off, &data[..landed]);
        }
        Err(S::Error::default())
    }

    fn erase(&mut self, block: u32) -> Result<(), Self::Error> {
        if !self.powered() {
            return Err(S::Error::default());
        }
        self.inner.erase(block)
    }

    fn sync(&mut self) -> Result<(), Self::Error> {
        if !self.powered() {
            return Err(S::Error::default());
        }
        self.inner.sync()
    }
}

/// The device stack the V4 sweeps run on: the tear injector sits inside
/// the alignment adapter, so a tear lands in a real device program.
pub type NorTornDevice = TornPartialStorage<StrictNorStorage>;
/// [`NorTornDevice`] behind the alignment adapter the kernel writes to.
pub type NorTornStorage = NorAlignedStorage<NorTornDevice>;
/// A filesystem over [`NorTornStorage`].
pub type NorTornFs = Fs<NorTornStorage>;
/// A filesystem over a powered (never torn) strict NOR device. Post tear
/// remounts use this: recovery may write, and those writes are held to
/// the same NOR rules.
pub type StrictNorFs = Fs<NorAlignedStorage<StrictNorStorage>>;

/// Partial landing lengths swept by the NOR sweeps, in bytes of the
/// `PROG_SIZE` window: nothing lands, the first byte alone, an aligned
/// half window, and all but the last byte.
///
/// Sampling bound, stated so the cap is explicit rather than silent: the
/// full space is every prefix length `0..PROG_SIZE` at every program
/// index, sixteen times the boundary sweep on this geometry. These four
/// points bracket the cases that differ structurally (no landing; a
/// landing too short to carry a tag; a landing that cuts a tag body in
/// half; a landing one byte short of the whole window), and they keep the
/// added runtime in seconds. A regression that only a prefix of, say,
/// seven bytes exposes would escape this sample; widening the sample is
/// a matter of runtime budget, not of harness capability.
pub const NOR_PARTIAL_LANDINGS: [usize; 4] =
    [0, 1, StrictNorStorage::PROG_SIZE / 2, StrictNorStorage::PROG_SIZE - 1];

/// Build the `NorAlignedStorage<TornPartialStorage<StrictNorStorage>>`
/// stack, optionally over an existing device image.
fn nor_torn_storage(
    trigger_at: usize,
    partial_bytes: usize,
    image: Option<&[u8]>,
) -> NorTornStorage {
    let mut device = StrictNorStorage::new();
    if let Some(img) = image {
        assert_eq!(img.len(), device.data.len(), "seed image is not one whole device image");
        device.data.copy_from_slice(img);
    }
    NorAlignedStorage::new(TornPartialStorage::new(device, trigger_at, partial_bytes))
        .expect("the strict NOR geometry satisfies the alignment adapter's invariants")
}

/// Cut the power: the raw device bytes, with the alignment adapter's
/// window cache deliberately NOT flushed.
///
/// The cache is volatile, so a power loss discards it. Flushing here
/// would model the wrong thing and, worse, would push bytes through the
/// already torn injector, which fails; the same discipline as
/// `tests/review_ctz_append_poison.rs`.
fn nor_power_off(fs: NorTornFs) -> alloc::vec::Vec<u8> {
    // `into_storage` consumes the filesystem and the adapter goes out of
    // scope with its window cache still dirty. It has no `Drop` that
    // flushes, so those bytes never reach the device: the power loss.
    fs.into_storage().inner().inner.data.clone()
}

/// `(format_calls, scenario_calls)` measured in DEVICE programs on an
/// untorn run: how many programs `Fs::format` issues through the
/// alignment adapter, and how many `scenario` adds after it.
///
/// These counts are LARGER than [`torn_call_counts`]'s, because the
/// adapter splits each commit span the kernel programs in one call into
/// `PROG_SIZE` windows: the device sweep is the finer one.
pub fn nor_torn_call_counts<F>(scenario: F) -> (usize, usize)
where
    F: FnOnce(&mut NorTornFs),
{
    let mut storage = nor_torn_storage(usize::MAX, 0, None);
    let mut scratch = make_buffer();
    Fs::format(&mut storage, &mut scratch).expect("untorn format must succeed");
    let format_calls = storage.inner().program_count;
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("untorn mount must succeed");
    let pre = fs.storage().inner().program_count;
    scenario(&mut fs);
    let post = fs.storage().inner().program_count;
    (format_calls, post - pre)
}

/// Run `scenario` on a filesystem formatted through the NOR stack, with
/// the power lost at the `trigger`-th DEVICE program (counted from
/// format's first) landing `partial_bytes` of that program's window.
///
/// Same strong semantics as [`run_torn_scenario`]: a tear inside
/// `Fs::format` is the only window in which the device may hold no
/// filesystem at all. Once format has returned, the image must remount.
pub fn run_nor_torn_scenario<F>(trigger: usize, partial_bytes: usize, scenario: F) -> TornRun
where
    F: FnOnce(&mut NorTornFs),
{
    let mut storage = nor_torn_storage(trigger, partial_bytes, None);
    let mut scratch = make_buffer();
    if Fs::format(&mut storage, &mut scratch).is_err() {
        return TornRun::TornFormat;
    }
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect("mount immediately after a completed format must succeed");
    scenario(&mut fs);
    TornRun::Image(nor_power_off(fs))
}

/// Format a strict NOR device, run `seed` on it untorn, and return the
/// raw device image. The seed runs to completion, so the image is a
/// clean filesystem: exactly the pre state a seeded sweep tears from.
pub fn nor_seed_image<F>(seed: F) -> alloc::vec::Vec<u8>
where
    F: FnOnce(&mut StrictNorFs),
{
    let mut storage = NorAlignedStorage::new(StrictNorStorage::new())
        .expect("the strict NOR geometry satisfies the alignment adapter's invariants");
    let mut scratch = make_buffer();
    Fs::format(&mut storage, &mut scratch).expect("seed format must succeed");
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("seed mount must succeed");
    seed(&mut fs);
    fs.into_storage().into_inner().expect("flushing a powered device must succeed").data
}

/// `(mount_calls, scenario_calls)` in DEVICE programs for a scenario run
/// from `seed_image`: how many programs the mount itself issues (mount
/// time recovery can write) and how many the scenario adds.
///
/// Triggers for [`run_nor_torn_from_seed`] are absolute, counted from
/// power on, so a sweep of the scenario runs
/// `mount_calls + 1 ..= mount_calls + scenario_calls + margin`.
pub fn nor_seeded_call_counts<F>(seed_image: &[u8], scenario: F) -> (usize, usize)
where
    F: FnOnce(&mut NorTornFs),
{
    let storage = nor_torn_storage(usize::MAX, 0, Some(seed_image));
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("the seed image must mount");
    let mount_calls = fs.storage().inner().program_count;
    scenario(&mut fs);
    let total = fs.storage().inner().program_count;
    (mount_calls, total - mount_calls)
}

/// Power on a device holding `seed_image`, mount it, and run `scenario`
/// with the power lost at the `trigger`-th DEVICE program since power on
/// (see [`nor_seeded_call_counts`] for the numbering).
///
/// Returns the raw device image at the moment of the power loss.
pub fn run_nor_torn_from_seed<F>(
    seed_image: &[u8],
    trigger: usize,
    partial_bytes: usize,
    scenario: F,
) -> alloc::vec::Vec<u8>
where
    F: FnOnce(&mut NorTornFs),
{
    let storage = nor_torn_storage(trigger, partial_bytes, Some(seed_image));
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_or_else(|e| {
        panic!(
            "trigger {trigger}: mounting a clean seed image failed ({e:?}); the sweep's \
             trigger range must start past the mount's own device programs"
        )
    });
    scenario(&mut fs);
    nor_power_off(fs)
}

/// Mount a post tear NOR image, holding the same H7 line as
/// [`mount_image_strict`]: the device held a valid filesystem before the
/// tear, so an unmountable image is a bricked device and a failure.
///
/// The remount runs through the alignment adapter over a strict NOR
/// device, so any write mount time recovery performs is itself held to
/// NOR rules, including over the half programmed window a partial
/// landing leaves behind.
pub fn mount_nor_image_strict(image: alloc::vec::Vec<u8>, ctx: &str) -> StrictNorFs {
    let mut device = StrictNorStorage::new();
    assert_eq!(image.len(), device.data.len(), "{ctx}: image is not one whole device image");
    device.data = image;
    let storage = NorAlignedStorage::new(device)
        .expect("the strict NOR geometry satisfies the alignment adapter's invariants");
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    Fs::mount(storage, &mut buf_a, &mut buf_b)
        .unwrap_or_else(|e| panic!("{ctx}: torn write left an unmountable image: {e:?}"))
}

/// Non vacuity guard for a partial landing sweep.
///
/// A sweep that runs every trigger and every landing length but where
/// no landing ever changes what reaches the device proves nothing; it
/// is the same shape of silent pass review H7 found in the pre fix
/// sweeps. Feed every image to [`observe`](Self::observe) with the
/// landing length that produced it, iterating the landing lengths in
/// `NOR_PARTIAL_LANDINGS` order (the `0` boundary tear first, so it
/// registers the baseline), then call
/// [`assert_partials_landed`](Self::assert_partials_landed) at the end.
#[derive(Default)]
pub struct PartialLandingWitness {
    baseline: alloc::collections::BTreeMap<usize, alloc::vec::Vec<u8>>,
    partial_runs: usize,
    differing: usize,
}

impl PartialLandingWitness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the image `partial_bytes`/`trigger` produced.
    pub fn observe(&mut self, partial_bytes: usize, trigger: usize, image: &[u8]) {
        if partial_bytes == 0 {
            self.baseline.insert(trigger, image.to_vec());
            return;
        }
        self.partial_runs += 1;
        if self.baseline.get(&trigger).is_some_and(|base| base.as_slice() != image) {
            self.differing += 1;
        }
    }

    /// How many partial landings put different bytes on the device than
    /// the plain boundary tear at the same trigger did.
    pub fn differing(&self) -> usize {
        self.differing
    }

    /// Fail if no partial landing changed anything, and report the
    /// ratio so `cargo test -- --nocapture` shows how much of the sweep
    /// the partial arm actually reached.
    pub fn assert_partials_landed(&self, ctx: &str) {
        println!(
            "{ctx}: {} of {} partial landing runs left a device image differing from the \
             boundary tear at the same trigger",
            self.differing, self.partial_runs
        );
        assert!(
            self.differing > 0,
            "{ctx}: no partial landing changed the device image relative to the \
             boundary tear at the same trigger; the partial arm of the sweep is vacuous"
        );
    }
}

/// Take the device image back out of a powered NOR filesystem, flushing
/// the adapter first (a clean shutdown, not a power loss). Feed the
/// result back to [`mount_nor_image_strict`] for the remount stability
/// check.
pub fn nor_image_of(fs: StrictNorFs) -> alloc::vec::Vec<u8> {
    fs.into_storage().into_inner().expect("flushing a powered device must succeed").data
}
