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

use crate::block::{BlockAddress, BlockPair};
use crate::crc;
use crate::error::Error;
use crate::tag::{Tag, TagType};

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
    /// the parity flip applied. Used by the writer to chain the next
    /// commit. Read only consumers can ignore.
    next_ptag: u32,
    /// The most recent Tail tag's pair address, decoded from the 8 byte
    /// body, or `None` if no Tail tag was present in any committed region.
    tail: Option<BlockPair>,
    /// `true` if the last Tail tag was a HardTail (entries continue in
    /// the threaded pair); `false` if it was a SoftTail (just a global
    /// directory thread, no entries in the next pair). Meaningless when
    /// `tail` is `None`.
    is_hard_tail: bool,
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

        // Second pass to extract Tail info from the committed region.
        // Latest Tail tag wins (commits in order; later supersedes).
        let (tail, is_hard_tail) = scan_for_tail(block, committed_end);

        Ok(Self { block, revision, committed_end, next_ptag, tail, is_hard_tail })
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
    /// parity flip. The writer reads this to chain the next commit
    /// onto the existing log; read-only consumers can ignore.
    #[must_use]
    pub fn next_ptag(&self) -> u32 {
        self.next_ptag
    }

    /// The most recent Tail tag's pair address, if any committed Tail
    /// tag was seen.
    ///
    /// LittleFS uses Tail tags for two purposes:
    /// - **HardTail** ([`TagType::HardTail`]): the directory's entries
    ///   continue in the threaded pair. Lookups must chase the chain.
    /// - **SoftTail** ([`TagType::SoftTail`]): a thread for the global
    ///   filesystem-wide directory list. Lookups do *not* descend.
    ///
    /// Use [`Self::is_hard_tail`] to discriminate.
    #[must_use]
    pub fn tail(&self) -> Option<BlockPair> {
        self.tail
    }

    /// `true` if the last Tail tag in the committed region was a HardTail
    /// (entries continue in the threaded pair). Returns `false` if no
    /// Tail tag was present or if the last was a SoftTail.
    #[must_use]
    pub fn is_hard_tail(&self) -> bool {
        self.is_hard_tail
    }

    /// Iterate over tags in commit order, from the verified region of the
    /// block. Includes CCRC and FCRC tags so callers can observe commit
    /// structure; higher level walkers filter to data tags.
    #[must_use]
    pub fn iter_tags(&self) -> TagIter<'a> {
        TagIter { block: self.block, offset: 0, end: self.committed_end, ptag: 0xFFFF_FFFF }
    }
}

/// Scan the committed region for the latest Tail tag (Soft or Hard)
/// and decode its 8 byte body as a `BlockPair`. Returns
/// `(Some(pair), is_hard)` if a Tail was found, `(None, false)`
/// otherwise.
fn scan_for_tail(block: &[u8], committed_end: usize) -> (Option<BlockPair>, bool) {
    let mut ptag: u32 = 0xFFFF_FFFF;
    let mut off: usize = 0;
    let mut latest: Option<BlockPair> = None;
    let mut is_hard = false;
    loop {
        off += Tag::from_bits(ptag).dsize();
        if off + 4 > committed_end {
            break;
        }
        let raw_tag =
            u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        let decoded = raw_tag ^ ptag;
        let tag = Tag::from_bits(decoded);
        if !tag.is_valid() {
            break;
        }
        if off + tag.dsize() > committed_end {
            break;
        }

        if matches!(tag.tag_type(), TagType::HardTail | TagType::SoftTail) && tag.body_len() == 8 {
            let body_start = off + 4;
            let a = u32::from_le_bytes([
                block[body_start],
                block[body_start + 1],
                block[body_start + 2],
                block[body_start + 3],
            ]);
            let b = u32::from_le_bytes([
                block[body_start + 4],
                block[body_start + 5],
                block[body_start + 6],
                block[body_start + 7],
            ]);
            latest = Some(BlockPair::new(BlockAddress::new(a), BlockAddress::new(b)));
            is_hard = matches!(tag.tag_type(), TagType::HardTail);
        }

        // CCRC tags carry the parity flip on bit 31 of the next ptag.
        ptag = decoded;
        if let Some(chunk) = tag.ccrc_chunk() {
            ptag ^= (u32::from(chunk) & 1) << 31;
        }
    }
    (latest, is_hard)
}

/// Builder for one or more commits on a fresh (erased) metadata block.
///
/// Takes a caller-supplied byte slice of at least the block size, writes
/// the revision header at offset 0, then appends tags via [`Commit::tag`].
/// Each call to [`Commit::finish`] emits a CCRC and resets the running
/// CRC; further [`Commit::tag`] calls start a new commit.
/// [`Commit::bytes_written`] returns the number of bytes written.
///
/// The builder does **not** touch the storage device. It produces the
/// on-disk byte layout in the caller's buffer; the caller is responsible
/// for erasing the block and programming the buffer's contents. This
/// separation keeps the builder no_std and no_alloc, and lets callers
/// stage multiple commits in memory before committing to flash.
///
/// The byte layout matches what [`MetadataReader`] consumes (verified
/// against `lfs_dir_commit` in the C reference).
#[derive(Debug)]
pub struct Commit<'a> {
    buf: &'a mut [u8],
    offset: usize,
    ptag: u32,
    crc: u32,
}

impl<'a> Commit<'a> {
    /// Begin a metadata block by writing `revision` (LE `u32`) at offset
    /// `0` and initializing the running XOR base and CRC accumulator.
    ///
    /// `buf` must be at least 8 bytes (4 for revision + 4 for a future
    /// tag word) but is typically the full block size. Bytes past the
    /// last commit are left untouched; callers usually pre-fill the
    /// buffer with `0xFF` so it reads as erased.
    pub fn new(buf: &'a mut [u8], revision: u32) -> Result<Self, Error> {
        if buf.len() < 8 {
            return Err(Error::OutOfRange);
        }
        buf[0..4].copy_from_slice(&revision.to_le_bytes());
        let crc = crc::update(crc::INIT, &buf[0..4]);
        Ok(Self { buf, offset: 4, ptag: 0xFFFF_FFFF, crc })
    }

    /// Continue an existing metadata block by appending a new commit at
    /// `offset` with the given pre-existing XOR base `ptag`.
    ///
    /// Typically `offset` is [`MetadataReader::committed_end`] and
    /// `ptag` is [`MetadataReader::next_ptag`] for the pair being
    /// extended. The CRC accumulator starts fresh at [`crc::INIT`]
    /// because every commit's CRC is independent.
    ///
    /// Bytes before `offset` are left untouched. Bytes from `offset`
    /// onward are overwritten by `tag` and `finish` calls.
    pub fn new_appending(buf: &'a mut [u8], offset: usize, ptag: u32) -> Result<Self, Error> {
        if buf.len() < offset + 8 {
            return Err(Error::OutOfRange);
        }
        Ok(Self { buf, offset, ptag, crc: crc::INIT })
    }

    /// Append a non-CCRC tag and its body to the in-progress commit.
    ///
    /// `body.len()` must equal `tag.body_len()`. Returns
    /// [`Error::OutOfRange`] if the tag would overflow the buffer.
    pub fn tag(&mut self, tag: Tag, body: &[u8]) -> Result<(), Error> {
        if tag.is_ccrc() {
            return Err(Error::InvalidTag);
        }
        if body.len() != tag.body_len() {
            return Err(Error::InvalidTag);
        }
        let dsize = tag.dsize();
        if self.offset + dsize > self.buf.len() {
            return Err(Error::OutOfRange);
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

    /// Finalize the current commit by emitting a CCRC with `chunk`.
    /// The running CRC is included as the 4-byte LE body and is
    /// reset for any subsequent commits. The XOR base is updated
    /// for the next commit per the parity-flip rule.
    ///
    /// Emits a plain CCRC: no FCRC, no prog-alignment padding. Use
    /// [`Self::finish_padded`] for the C-reference-compatible variant
    /// that adds FCRC for torn-write detection and pads the CCRC body
    /// so the next commit starts on a prog-aligned boundary.
    pub fn finish(&mut self, chunk: u8) -> Result<(), Error> {
        let ccrc_tag = Tag::new(true, crate::tag::TagType::CommitCrc(chunk), 0x3FF, 4);
        if self.offset + 8 > self.buf.len() {
            return Err(Error::OutOfRange);
        }
        let raw = ccrc_tag.into_bits() ^ self.ptag;
        self.buf[self.offset..self.offset + 4].copy_from_slice(&raw.to_be_bytes());
        self.crc = crc::update(self.crc, &raw.to_be_bytes());
        self.buf[self.offset + 4..self.offset + 8].copy_from_slice(&self.crc.to_le_bytes());
        self.ptag = ccrc_tag.into_bits() ^ ((u32::from(chunk) & 1) << 31);
        self.crc = crc::INIT;
        self.offset += 8;
        Ok(())
    }

    /// Finalize the commit with a forward CRC (FCRC) tag and a
    /// prog-aligned CCRC body, matching `lfs_dir_commitcrc` in the C
    /// reference (`lfs.c:1641`).
    ///
    /// `prog_size` is the device's program-unit size; `block_size` is
    /// the metadata block size. Both are typically pulled from the
    /// [`crate::Storage`] trait's associated constants.
    ///
    /// # Behavior
    ///
    /// Computes `end = align_up(min(off + 20, block_size), prog_size)`
    /// where `off` is the current commit cursor. The CCRC's body
    /// length is set to `end - ccrc_off - 4`, padding out to `end`
    /// with bytes that are not included in the CRC (the reader
    /// ignores them). The bytes from CCRC body's first 4 bytes hold
    /// the actual CRC value; the rest is padding the caller's pre-fill
    /// is expected to leave as `0xFF`.
    ///
    /// If `end + prog_size <= block_size` and there is room for the
    /// FCRC tag (12 bytes) ahead of the CCRC, an FCRC tag is emitted
    /// before the CCRC. The FCRC body advertises the expected CRC of
    /// the next `prog_size` bytes after `end`, assuming they remain in
    /// the post-erase state (`0xFF`). A reader (the C reference, or a
    /// future enhanced version of this crate's reader) uses the FCRC
    /// to detect torn writes that landed past this commit's CCRC.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfRange`] if `end` exceeds the buffer or the
    ///   computed CCRC padding would overflow the 10-bit tag length
    ///   field (`0x3FE` max). The latter is unreachable for any
    ///   sane `prog_size` (≤ 1018 bytes).
    pub fn finish_padded(
        &mut self,
        chunk: u8,
        prog_size: usize,
        block_size: usize,
    ) -> Result<(), Error> {
        let off = self.offset;
        if prog_size == 0 {
            return self.finish(chunk);
        }
        let target = (off + 20).min(block_size);
        let end = target.div_ceil(prog_size) * prog_size;
        if end > block_size {
            return Err(Error::OutOfRange);
        }
        if end < off + 8 {
            // Not enough room for even a minimal CCRC. Caller bug.
            return Err(Error::OutOfRange);
        }

        // Will we emit an FCRC? Need (a) room for the FCRC tag + body
        // (12 bytes) ahead of the CCRC, i.e. `end >= off + 20`, and
        // (b) a prog window past `end` that the FCRC can describe,
        // i.e. `end + prog_size <= block_size`.
        let emit_fcrc = end >= off + 20 && end + prog_size <= block_size;

        if emit_fcrc {
            // FCRC body: (size_LE: u32, crc_LE: u32). The CRC is the
            // CRC32 of `prog_size` 0xFF bytes from `crc::INIT` (the
            // post-erase state of the next prog window).
            let mut fcrc_value = crc::INIT;
            // Run the bytes one at a time; small (prog_size ≤ 512 in
            // practice) so the loop cost is irrelevant.
            for _ in 0..prog_size {
                fcrc_value = crc::update(fcrc_value, &[0xFF]);
            }
            let mut fcrc_body = [0u8; 8];
            fcrc_body[0..4].copy_from_slice(&(prog_size as u32).to_le_bytes());
            fcrc_body[4..8].copy_from_slice(&fcrc_value.to_le_bytes());
            let fcrc_tag = Tag::new(true, crate::tag::TagType::ForwardCrc, 0x3FF, 8);
            self.tag(fcrc_tag, &fcrc_body)?;
        }

        // Emit padded CCRC: body length = end - ccrc_off - 4. First
        // 4 bytes of body are the running CRC; the rest is padding
        // (not CRCed, ignored by the reader).
        let ccrc_off = self.offset;
        let body_len = end - ccrc_off - 4;
        if body_len > 0x3FE {
            return Err(Error::OutOfRange);
        }
        if ccrc_off + 4 + body_len > self.buf.len() {
            return Err(Error::OutOfRange);
        }
        let ccrc_tag =
            Tag::new(true, crate::tag::TagType::CommitCrc(chunk), 0x3FF, body_len as u16);
        let raw = ccrc_tag.into_bits() ^ self.ptag;
        self.buf[ccrc_off..ccrc_off + 4].copy_from_slice(&raw.to_be_bytes());
        // Only the tag word feeds the CRC; the body (CRC + padding)
        // does not, per the C reference's `lfs_dir_commitcrc`.
        self.crc = crc::update(self.crc, &raw.to_be_bytes());
        self.buf[ccrc_off + 4..ccrc_off + 8].copy_from_slice(&self.crc.to_le_bytes());
        // Padding bytes (body_len - 4): leave as whatever the buffer
        // pre-fill held. Callers pre-fill with 0xFF before construction.
        self.ptag = ccrc_tag.into_bits() ^ ((u32::from(chunk) & 1) << 31);
        self.crc = crc::INIT;
        self.offset = end;
        Ok(())
    }

    /// Total bytes written so far, including the revision header.
    #[must_use]
    pub fn bytes_written(&self) -> usize {
        self.offset
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
    fn commit_builder_roundtrips_via_reader() {
        let mut buf = vec![0xFFu8; 256];
        let mut c = Commit::new(&mut buf, 42).unwrap();
        c.tag(Tag::new(true, TagType::Superblock, 0, 8), b"littlefs").unwrap();
        c.tag(Tag::new(true, TagType::InlineStruct, 0, 4), &[1, 2, 3, 4]).unwrap();
        c.finish(0).unwrap();

        let reader = MetadataReader::new(&buf).unwrap();
        assert_eq!(reader.revision(), 42);
        assert!(reader.has_commits());

        let entries: Vec<_> = reader.iter_tags().collect();
        assert_eq!(entries.len(), 3); // superblock NAME + InlineStruct + CCRC
        assert_eq!(entries[0].tag.tag_type(), TagType::Superblock);
        assert_eq!(entries[0].body, b"littlefs");
        assert_eq!(entries[1].tag.tag_type(), TagType::InlineStruct);
        assert_eq!(entries[1].body, &[1, 2, 3, 4]);
        assert!(entries[2].tag.is_ccrc());
    }

    #[test]
    fn commit_builder_rejects_ccrc_via_tag() {
        let mut buf = vec![0xFFu8; 64];
        let mut c = Commit::new(&mut buf, 1).unwrap();
        let ccrc = Tag::new(true, TagType::CommitCrc(0), ID_NONE, 4);
        let err = c.tag(ccrc, &[0u8; 4]).unwrap_err();
        assert_eq!(err, Error::InvalidTag);
    }

    #[test]
    fn commit_builder_rejects_body_length_mismatch() {
        let mut buf = vec![0xFFu8; 64];
        let mut c = Commit::new(&mut buf, 1).unwrap();
        let t = Tag::new(true, TagType::InlineStruct, 0, 8);
        // Tag says length 8 but body is only 4.
        let err = c.tag(t, &[0u8; 4]).unwrap_err();
        assert_eq!(err, Error::InvalidTag);
    }

    #[test]
    fn commit_builder_rejects_overflow() {
        let mut buf = [0xFFu8; 16];
        let mut c = Commit::new(&mut buf, 1).unwrap();
        // Tag with a body that doesn't fit (16 - 4 rev - 4 tag = 8 left, request 16).
        let t = Tag::new(true, TagType::InlineStruct, 0, 16);
        let err = c.tag(t, &[0u8; 16]).unwrap_err();
        assert_eq!(err, Error::OutOfRange);
    }

    #[test]
    fn commit_builder_short_buffer_rejected() {
        let mut tiny = [0u8; 4]; // < 8
        assert_eq!(Commit::new(&mut tiny, 0).unwrap_err(), Error::OutOfRange);
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

    /// Build a commit using `finish_padded`, then read it back: the
    /// reader must verify the CCRC, see the FCRC tag in the
    /// committed region, and report the committed end at the
    /// prog-aligned `end`.
    #[test]
    fn finish_padded_roundtrips_through_reader() {
        let mut buf = [0xFFu8; 256];
        {
            let mut c = Commit::new(&mut buf, 7).unwrap();
            c.tag(Tag::new(true, TagType::InlineStruct, 0, 5), b"hello").unwrap();
            c.finish_padded(0, 16, 256).unwrap();
        }
        let r = MetadataReader::new(&buf).unwrap();
        assert!(r.has_commits());
        assert_eq!(r.revision(), 7);
        // FCRC should appear in the committed tag stream.
        let mut saw_fcrc = false;
        let mut saw_inline = false;
        for entry in r.iter_tags() {
            match entry.tag.tag_type() {
                TagType::InlineStruct => {
                    saw_inline = true;
                    assert_eq!(entry.body, b"hello");
                }
                TagType::ForwardCrc => {
                    saw_fcrc = true;
                    assert_eq!(entry.body.len(), 8);
                    // FCRC body: (size LE, crc LE).
                    let size = u32::from_le_bytes([
                        entry.body[0],
                        entry.body[1],
                        entry.body[2],
                        entry.body[3],
                    ]);
                    assert_eq!(size, 16);
                }
                _ => {}
            }
        }
        assert!(saw_inline);
        assert!(saw_fcrc);
        // committed_end must be prog-aligned (multiple of 16).
        assert_eq!(r.committed_end() % 16, 0);
    }

    /// When the commit fills the block to where no FCRC window
    /// remains, finish_padded must still emit a valid CCRC (just
    /// without FCRC).
    #[test]
    fn finish_padded_omits_fcrc_when_no_room_for_next_prog() {
        // Tiny block: just enough for revision + one tag + CCRC, no
        // FCRC space afterwards. block_size = 32, prog_size = 16.
        let mut buf = [0xFFu8; 32];
        {
            let mut c = Commit::new(&mut buf, 1).unwrap();
            c.tag(Tag::new(true, TagType::InlineStruct, 0, 4), &[1, 2, 3, 4]).unwrap();
            c.finish_padded(0, 16, 32).unwrap();
        }
        let r = MetadataReader::new(&buf).unwrap();
        assert!(r.has_commits());
        // No FCRC because end + prog_size > block_size.
        for entry in r.iter_tags() {
            assert_ne!(
                entry.tag.tag_type(),
                TagType::ForwardCrc,
                "FCRC should not be emitted when there is no next-prog window"
            );
        }
    }

    /// Two successive finish_padded calls in the same block: the
    /// second commit starts at the prog-aligned offset the first
    /// padded out to. Both commits round-trip.
    #[test]
    fn two_padded_commits_chain_correctly() {
        let mut buf = [0xFFu8; 256];
        let first_end;
        {
            let mut c = Commit::new(&mut buf, 1).unwrap();
            c.tag(Tag::new(true, TagType::InlineStruct, 0, 2), b"AA").unwrap();
            c.finish_padded(0, 16, 256).unwrap();
            first_end = c.bytes_written();
        }
        // Read state needed to extend.
        let next_ptag = MetadataReader::new(&buf).unwrap().next_ptag();
        {
            let mut c = Commit::new_appending(&mut buf, first_end, next_ptag).unwrap();
            c.tag(Tag::new(true, TagType::InlineStruct, 1, 2), b"BB").unwrap();
            c.finish_padded(0, 16, 256).unwrap();
        }
        let r = MetadataReader::new(&buf).unwrap();
        assert!(r.has_commits());
        let mut inline_bodies: Vec<Vec<u8>> = Vec::new();
        for entry in r.iter_tags() {
            if entry.tag.tag_type() == TagType::InlineStruct {
                inline_bodies.push(entry.body.to_vec());
            }
        }
        assert_eq!(inline_bodies, vec![b"AA".to_vec(), b"BB".to_vec()]);
    }
}
