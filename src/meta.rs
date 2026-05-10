//! Metadata pair reader.
//!
//! A LittleFS image stores all metadata (superblock, directories, file
//! handles) in pairs of erase blocks. Each block carries a 32 bit revision
//! counter at offset 0, followed by a log of commits. The block with the
//! higher revision (signed comparison, wrap aware) is the active one; the
//! other is the alternate.
//!
//! Each commit is a sequence of tag prefixed entries terminated by a CCRC
//! (commit CRC) tag whose body contains the CRC of all bytes from the start
//! of the commit through the CCRC tag itself.
//!
//! # Bit accuracy
//!
//! The on disk byte layout (verified against `lfs_dir_fetchmatch` in the C
//! reference at `lfs.c:1095`):
//!
//! - **Revision counter.** 4 bytes at offset 0, little endian `u32`.
//! - **Tag word.** 4 bytes, big endian on disk. XORed against the previous
//!   tag's decoded value before storage; the very first tag's "previous" is
//!   `0xFFFFFFFF`.
//! - **Tag body.** `tag.length()` bytes if the length field is not the
//!   `0x3FF` sentinel, else zero bytes (delete tags carry no body).
//! - **CCRC stored value.** 4 bytes following a CCRC tag, little endian
//!   `u32`. Equals the CRC of all bytes from the start of the commit
//!   (including the revision counter for the first commit) through the
//!   CCRC tag word, computed using the LittleFS CRC variant.
//! - **Post CCRC parity flip.** After a successful CCRC verification, the
//!   running XOR base for the next commit's first tag is XORed with
//!   `(chunk & 1) << 31`. This alternates the valid bit semantics across
//!   commits, so an erased region (all `0xFF`) reads as the end of log
//!   regardless of whether the previous commit's parity was even or odd.
//! - **CRC reset.** Each commit's CRC accumulator starts fresh at
//!   `0xFFFFFFFF`.
//!
//! # The reader's contract
//!
//! [`MetadataReader::new`] walks the entire block once at construction time:
//! reads the revision, walks every tag, verifies every CCRC, and records the
//! offset just past the last successfully verified CCRC as the
//! "committed end". Tags after the committed end are either pristine erased
//! flash or an in flight write that did not durably commit; the reader
//! ignores them.
//!
//! [`MetadataReader::iter_tags`] walks again over only the committed region,
//! emitting one [`TagEntry`] per tag (CCRC tags included, so callers can
//! observe commit boundaries; higher level walkers filter them out).

use crate::block::BlockAddress;
use crate::crc;
use crate::error::Error;
use crate::tag::Tag;

/// One tag plus its body bytes, as emitted by [`MetadataReader::iter_tags`].
#[derive(Clone, Copy, Debug)]
pub struct TagEntry<'a> {
    /// The decoded tag (after XOR against the running base).
    pub tag: Tag,
    /// The tag's body, exactly `tag.body_len()` bytes. May be empty.
    pub body: &'a [u8],
}

/// Reader for a single metadata block.
///
/// Constructed by [`MetadataReader::new`], which walks the block once to
/// identify the committed region. Subsequent calls to
/// [`MetadataReader::iter_tags`] enumerate tags from that region without
/// re verifying CRCs.
#[derive(Clone, Copy, Debug)]
pub struct MetadataReader<'a> {
    block: &'a [u8],
    revision: u32,
    /// Offset just past the last verified CCRC. Zero when no commit verified.
    committed_end: usize,
    /// The running XOR base immediately after the last verified CCRC, with
    /// the parity flip applied. Used by writers (Phase 2) to chain the next
    /// commit. Read only consumers can ignore.
    next_ptag: u32,
}

impl<'a> MetadataReader<'a> {
    /// Parse a metadata block.
    ///
    /// Returns [`Error::Corrupt`] only if the block is shorter than the
    /// 4 byte revision header. A block with no successfully verified
    /// commits parses successfully and reports `committed_end == 0` and
    /// `iter_tags()` yielding nothing.
    pub fn new(block: &'a [u8]) -> Result<Self, Error> {
        if block.len() < 4 {
            return Err(Error::Corrupt);
        }
        let revision = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);

        // Walk to find the last verified CCRC.
        let mut ptag: u32 = 0xFFFF_FFFF;
        let mut running_crc: u32 = crc::update(crc::INIT, &block[0..4]);
        // `off` is advanced by dsize(ptag) BEFORE each read, mirroring
        // the C reference's loop structure. The initial ptag has length
        // field = 0x3FF (no body) and dsize = 4, so the first read lands
        // at offset 4 (just past the revision counter).
        let mut off: usize = 0;
        let mut committed_end: usize = 0;
        let mut next_ptag: u32 = 0xFFFF_FFFF;

        loop {
            off += Tag::from_bits(ptag).dsize();
            if off + 4 > block.len() {
                break;
            }
            let raw_tag =
                u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
            running_crc = crc::update(running_crc, &block[off..off + 4]);
            let decoded = raw_tag ^ ptag;
            let tag = Tag::from_bits(decoded);

            if !tag.is_valid() {
                // End of log.
                break;
            }

            // Bounds check the body before trusting the length.
            if off + tag.dsize() > block.len() {
                break;
            }

            ptag = decoded;

            if tag.is_ccrc() {
                // Stored CRC value is the 4 byte body, little endian.
                let body_start = off + 4;
                let stored = u32::from_le_bytes([
                    block[body_start],
                    block[body_start + 1],
                    block[body_start + 2],
                    block[body_start + 3],
                ]);
                if running_crc != stored {
                    // CRC mismatch: this commit is not durable.
                    break;
                }
                // Commit verified. Advance the committed boundary.
                committed_end = off + tag.dsize();
                // Apply the parity flip to the running XOR base.
                let chunk = tag.ccrc_chunk().unwrap_or(0);
                ptag ^= (u32::from(chunk) & 1) << 31;
                next_ptag = ptag;
                // Reset the CRC accumulator for the next commit.
                running_crc = crc::INIT;
            } else {
                // Normal tag: accumulate its body into the CRC.
                running_crc = crc::update(running_crc, &block[off + 4..off + tag.dsize()]);
            }
        }

        Ok(Self { block, revision, committed_end, next_ptag })
    }

    /// The on disk revision counter.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Offset (in bytes) just past the last verified CCRC. Zero if no
    /// commit verified. New commits would be written starting here.
    #[must_use]
    pub fn committed_end(&self) -> usize {
        self.committed_end
    }

    /// `true` if at least one commit verified.
    #[must_use]
    pub fn has_commits(&self) -> bool {
        self.committed_end > 0
    }

    /// The XOR base for the next (uncommitted) tag, after the post CCRC
    /// parity flip. Phase 2 writers need this; read only consumers can
    /// ignore.
    #[must_use]
    pub fn next_ptag(&self) -> u32 {
        self.next_ptag
    }

    /// Iterate over tags in commit order, from the verified region of the
    /// block. Includes CCRC and FCRC tags so callers can observe commit
    /// structure; higher level walkers filter to data tags.
    #[must_use]
    pub fn iter_tags(&self) -> TagIter<'a> {
        TagIter { block: self.block, offset: 0, end: self.committed_end, ptag: 0xFFFF_FFFF }
    }
}

/// Iterator over tags in a [`MetadataReader`]'s committed region. Returned
/// by [`MetadataReader::iter_tags`].
#[derive(Clone, Debug)]
pub struct TagIter<'a> {
    block: &'a [u8],
    offset: usize,
    end: usize,
    ptag: u32,
}

/// Signed revision comparison.
///
/// Mirrors `lfs_scmp` in the C reference's `lfs_util.h`: returns
/// `(int32_t)(a - b)`, which is positive when `a` is "newer than" `b` under
/// modular ordering of the revision counter, negative when `a` is older,
/// and zero when equal. Wraparound between near-zero and near-`u32::MAX`
/// values stays correct.
#[inline]
#[must_use]
pub fn rev_scmp(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

/// Reader for a metadata *pair*: two erase blocks between which the
/// filesystem rotates for wear leveling. The pair's active block is the
/// one with the higher revision counter (signed comparison, wrap aware);
/// if that block has no successfully verified commits, the alternate is
/// used instead.
#[derive(Clone, Copy, Debug)]
pub struct MetadataPair<'a> {
    /// Address of the active block (the one whose tags `reader` returns).
    pub active_block: BlockAddress,
    /// Address of the alternate (non-active) block.
    pub alternate_block: BlockAddress,
    /// Reader for the active block.
    pub reader: MetadataReader<'a>,
    /// `true` if the alternate block also has at least one verified commit.
    /// Diagnostic only; not load bearing.
    pub alternate_valid: bool,
}

impl<'a> MetadataPair<'a> {
    /// Parse a metadata pair from two block buffers.
    ///
    /// `addr_a` and `addr_b` are the block addresses; `block_a` and
    /// `block_b` are their byte contents. Returns [`Error::Corrupt`] if
    /// neither block has any successfully verified commit.
    pub fn parse(
        addr_a: BlockAddress,
        block_a: &'a [u8],
        addr_b: BlockAddress,
        block_b: &'a [u8],
    ) -> Result<Self, Error> {
        let reader_a = MetadataReader::new(block_a)?;
        let reader_b = MetadataReader::new(block_b)?;

        // Pick the block with the higher revision. On a tie, pair[0] wins,
        // matching the C reference's loop ordering in lfs_dir_fetchmatch
        // (r stays 0 when neither lfs_scmp comparison is strictly positive).
        let a_is_newer = rev_scmp(reader_a.revision(), reader_b.revision()) >= 0;
        let (primary, primary_addr, secondary, secondary_addr) = if a_is_newer {
            (reader_a, addr_a, reader_b, addr_b)
        } else {
            (reader_b, addr_b, reader_a, addr_a)
        };

        if primary.has_commits() {
            return Ok(Self {
                active_block: primary_addr,
                alternate_block: secondary_addr,
                reader: primary,
                alternate_valid: secondary.has_commits(),
            });
        }
        if secondary.has_commits() {
            return Ok(Self {
                active_block: secondary_addr,
                alternate_block: primary_addr,
                reader: secondary,
                alternate_valid: false,
            });
        }
        Err(Error::Corrupt)
    }
}

impl<'a> Iterator for TagIter<'a> {
    type Item = TagEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.offset += Tag::from_bits(self.ptag).dsize();
        if self.offset + 4 > self.end {
            return None;
        }
        let raw_tag = u32::from_be_bytes([
            self.block[self.offset],
            self.block[self.offset + 1],
            self.block[self.offset + 2],
            self.block[self.offset + 3],
        ]);
        let decoded = raw_tag ^ self.ptag;
        let tag = Tag::from_bits(decoded);
        if !tag.is_valid() {
            return None;
        }
        if self.offset + tag.dsize() > self.end {
            return None;
        }

        let body = &self.block[self.offset + 4..self.offset + tag.dsize()];

        // Update ptag for the next iteration, mirroring the parse phase's
        // post CCRC parity flip so chained commits decode correctly.
        let mut next_ptag = decoded;
        if let Some(chunk) = tag.ccrc_chunk() {
            next_ptag ^= (u32::from(chunk) & 1) << 31;
        }
        self.ptag = next_ptag;

        Some(TagEntry { tag, body })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::*;
    use crate::tag::{TagType, ID_NONE};

    /// Build a single commit metadata block in-line for unit testing. The
    /// public test fixture lives in `tests/common/mod.rs`; this version is
    /// the bare minimum the unit tests need.
    fn build_single_commit(revision: u32, tags: &[(Tag, Vec<u8>)], block_size: usize) -> Vec<u8> {
        let mut buf = vec![0xFFu8; block_size];
        buf[0..4].copy_from_slice(&revision.to_le_bytes());

        let mut ptag: u32 = 0xFFFF_FFFF;
        let mut running_crc = crc::update(crc::INIT, &buf[0..4]);
        let mut off = 4usize;

        for (tag, body) in tags {
            let raw = tag.into_bits() ^ ptag;
            buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
            running_crc = crc::update(running_crc, &raw.to_be_bytes());
            let blen = tag.body_len();
            assert_eq!(body.len(), blen, "body length must match tag length field");
            buf[off + 4..off + 4 + blen].copy_from_slice(body);
            running_crc = crc::update(running_crc, body);
            ptag = tag.into_bits();
            off += 4 + blen;
        }

        // Emit the CCRC tag with chunk = 0.
        let ccrc_tag = Tag::new(true, TagType::CommitCrc(0), ID_NONE, 4);
        let raw_ccrc = ccrc_tag.into_bits() ^ ptag;
        buf[off..off + 4].copy_from_slice(&raw_ccrc.to_be_bytes());
        running_crc = crc::update(running_crc, &raw_ccrc.to_be_bytes());
        buf[off + 4..off + 8].copy_from_slice(&running_crc.to_le_bytes());

        buf
    }

    #[test]
    fn empty_block_has_no_commits() {
        let mut buf = vec![0xFFu8; 256];
        buf[0..4].copy_from_slice(&42u32.to_le_bytes());

        let r = MetadataReader::new(&buf).unwrap();
        assert_eq!(r.revision(), 42);
        assert!(!r.has_commits());
        assert_eq!(r.iter_tags().count(), 0);
    }

    #[test]
    fn rejects_block_shorter_than_revision() {
        let buf = [0u8; 3];
        assert_eq!(MetadataReader::new(&buf).unwrap_err(), Error::Corrupt);
    }

    #[test]
    fn single_commit_roundtrips() {
        let tags = vec![
            (Tag::new(true, TagType::Superblock, 0, 8), b"littlefs".to_vec()),
            (Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4]),
        ];
        let block = build_single_commit(7, &tags, 512);

        let r = MetadataReader::new(&block).unwrap();
        assert_eq!(r.revision(), 7);
        assert!(r.has_commits());

        let mut iter = r.iter_tags();
        // First emitted tag: the Superblock NAME entry.
        let e = iter.next().unwrap();
        assert_eq!(e.tag.tag_type(), TagType::Superblock);
        assert_eq!(e.body, b"littlefs");
        // Second: the InlineStruct.
        let e = iter.next().unwrap();
        assert_eq!(e.tag.tag_type(), TagType::InlineStruct);
        assert_eq!(e.body, &[1, 2, 3, 4]);
        // Third: the CCRC.
        let e = iter.next().unwrap();
        assert!(e.tag.is_ccrc());
        assert_eq!(e.body.len(), 4);
        // No more.
        assert!(iter.next().is_none());
    }

    #[test]
    fn delete_tag_has_no_body() {
        let tags = vec![(Tag::new(true, TagType::Delete, 5, 0x3FF), vec![])];
        let block = build_single_commit(1, &tags, 256);
        let r = MetadataReader::new(&block).unwrap();
        let entries: Vec<_> = r.iter_tags().collect();
        assert_eq!(entries.len(), 2); // delete + CCRC
        assert_eq!(entries[0].tag.tag_type(), TagType::Delete);
        assert!(entries[0].body.is_empty());
    }

    #[test]
    fn corrupted_body_byte_invalidates_commit() {
        let tags = vec![(Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4])];
        let mut block = build_single_commit(1, &tags, 256);
        // Corrupt one body byte. The CRC stored in the CCRC body still
        // reflects the original; verification must fail.
        block[10] ^= 0xFF;
        let r = MetadataReader::new(&block).unwrap();
        assert!(!r.has_commits(), "corrupted body must invalidate the commit");
    }

    #[test]
    fn scmp_ordering() {
        // Equal -> zero.
        assert_eq!(rev_scmp(5, 5), 0);
        // Strictly greater small numbers -> positive.
        assert!(rev_scmp(10, 5) > 0);
        // Strictly less -> negative.
        assert!(rev_scmp(5, 10) < 0);
        // Wraparound: 1 is newer than 0xFFFFFFFE.
        assert!(rev_scmp(1, 0xFFFFFFFE) > 0);
        // Wraparound: 0xFFFFFFFE is older than 1.
        assert!(rev_scmp(0xFFFFFFFE, 1) < 0);
        // Far wrap: 0 vs 0x80000000 is ambiguous (max distance).
        // The C reference returns (int32_t)(0 - 0x80000000) = -0x80000000,
        // which is negative, so 0 is "older". This is the documented
        // limit of the wrap-aware comparison.
        assert!(rev_scmp(0, 0x80000000) < 0);
    }

    #[test]
    fn pair_picks_higher_revision() {
        let tags = vec![(Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4])];
        let block_a = build_single_commit(5, &tags, 256);
        let block_b = build_single_commit(7, &tags, 256);

        let pair =
            MetadataPair::parse(BlockAddress::new(10), &block_a, BlockAddress::new(11), &block_b)
                .unwrap();
        assert_eq!(pair.active_block, BlockAddress::new(11));
        assert_eq!(pair.alternate_block, BlockAddress::new(10));
        assert_eq!(pair.reader.revision(), 7);
        assert!(pair.alternate_valid);
    }

    #[test]
    fn pair_picks_a_on_revision_tie() {
        let tags = vec![(Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4])];
        let block_a = build_single_commit(7, &tags, 256);
        let block_b = build_single_commit(7, &tags, 256);
        let pair =
            MetadataPair::parse(BlockAddress::new(10), &block_a, BlockAddress::new(11), &block_b)
                .unwrap();
        assert_eq!(pair.active_block, BlockAddress::new(10), "ties resolve to pair[0]");
    }

    #[test]
    fn pair_handles_revision_wraparound() {
        // a has revision 0xFFFFFFFE; b has revision 0x00000001. Modular
        // ordering says b is newer (3 steps ahead).
        let tags = vec![(Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4])];
        let block_a = build_single_commit(0xFFFFFFFE, &tags, 256);
        let block_b = build_single_commit(0x00000001, &tags, 256);
        let pair =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap();
        assert_eq!(pair.reader.revision(), 0x00000001);
        assert_eq!(pair.active_block, BlockAddress::new(1));
    }

    #[test]
    fn pair_falls_back_to_alternate_when_active_empty() {
        // b has the higher revision but no commits. Active should fall
        // back to a.
        let tags = vec![(Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4])];
        let block_a = build_single_commit(5, &tags, 256);
        let mut block_b = vec![0xFFu8; 256];
        block_b[0..4].copy_from_slice(&100u32.to_le_bytes());

        let pair =
            MetadataPair::parse(BlockAddress::new(10), &block_a, BlockAddress::new(11), &block_b)
                .unwrap();
        assert_eq!(pair.active_block, BlockAddress::new(10), "fall back to a with commits");
        assert_eq!(pair.reader.revision(), 5);
        assert!(!pair.alternate_valid);
    }

    #[test]
    fn pair_errors_when_neither_has_commits() {
        let mut block_a = vec![0xFFu8; 256];
        block_a[0..4].copy_from_slice(&1u32.to_le_bytes());
        let mut block_b = vec![0xFFu8; 256];
        block_b[0..4].copy_from_slice(&2u32.to_le_bytes());

        let err =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap_err();
        assert_eq!(err, Error::Corrupt);
    }

    #[test]
    fn corrupted_revision_invalidates_commit() {
        let tags = vec![(Tag::new(true, TagType::InlineStruct, 0, 4), vec![1, 2, 3, 4])];
        let mut block = build_single_commit(1, &tags, 256);
        block[0] ^= 0xFF;
        // Revision field is part of the CRC, so corrupting it must
        // invalidate the commit.
        let r = MetadataReader::new(&block).unwrap();
        assert!(!r.has_commits());
    }
}
