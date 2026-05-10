//! Shared helpers for the property test files. Lives at `tests/common/mod.rs`
//! so each `tests/property_*.rs` file can pull it in via `mod common;` without
//! Cargo treating it as a separate integration test binary.
//!
//! The pattern is borrowed from ferrodec's testing layout. Helpers here are
//! split across consumers, so a blanket `#[allow(dead_code)]` keeps unused
//! warnings off in files that import only one helper.

#![allow(dead_code)]

use littlefs2_pure::crc;
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType, ID_NONE};

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

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
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
