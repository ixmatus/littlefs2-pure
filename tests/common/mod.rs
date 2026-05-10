//! Shared helpers for the property test files. Lives at `tests/common/mod.rs`
//! so each `tests/property_*.rs` file can pull it in via `mod common;` without
//! Cargo treating it as a separate integration test binary.
//!
//! The pattern is borrowed from ferrodec's testing layout. Helpers here are
//! split across consumers, so a blanket `#[allow(dead_code)]` keeps unused
//! warnings off in files that import only one helper.

#![allow(dead_code)]

use littlefs2_pure::crc;
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
