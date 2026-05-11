//! Mounted filesystem handle.
//!
//! [`Fs::mount`] is the entry point. It reads the root metadata pair
//! (blocks `0` and `1`) through the provided [`Storage`] backed device,
//! picks the active block via [`MetadataPair::parse`], parses the
//! superblock via [`Superblock::from_pair`], and validates the geometry
//! against the storage trait's advertised constants.
//!
//! After mount, the returned [`Fs`] holds:
//!
//! - the storage handle (owned, recoverable via [`Fs::into_storage`]);
//! - the decoded [`Superblock`];
//! - the address of the root metadata pair (always
//!   [`crate::ROOT_BLOCK_PAIR`] for v2).
//!
//! Buffers for the mount probe are passed in by the caller. After mount
//! returns, the buffers can be reused; the `Fs` does not retain a borrow
//! of them. Future directory and file operations (Phases 1e and 1f) take
//! their own scratch buffer per call, so users on `no_std` targets can
//! size a single buffer and reuse it.

use crate::block::BlockPair;
use crate::error::Error;
use crate::meta::MetadataPair;
use crate::storage::Storage;
use crate::superblock::Superblock;
use crate::{BlockAddress, ROOT_BLOCK_PAIR};

/// Maximum number of live entries per metadata pair the write kernel
/// can compact in one operation.
///
/// Matches [`crate::dir::MAX_LIVE_ENTRIES`] so any pair that
/// [`crate::dir::live_entries`] accepts can also be compacted.
const MAX_LIVE_ENTRIES: usize = crate::dir::MAX_LIVE_ENTRIES;

/// Maximum chain length a single CTZ write can produce.
///
/// At 4 KiB blocks this caps a single CTZ file at ~1 MiB; at 256 byte
/// blocks (the test geometry) it caps at 64 KiB. The chain address
/// table is stack-allocated as `[BlockAddress; MAX_CTZ_WRITE_BLOCKS]`
/// (1 KiB at the current cap), so larger files need a streaming CTZ
/// writer (Phase 2f).
const MAX_CTZ_WRITE_BLOCKS: usize = 256;

/// Maximum number of metadata pairs the directory-listing path will
/// chase through HardTails. Bounds the chain length the reader is
/// willing to follow, mirroring `MAX_QUEUED_PAIRS` in the allocator's
/// BFS. A directory with more continuation pairs returns
/// [`Error::OutOfRange`] on enumeration.
const MAX_DIR_CHAIN: usize = 32;

/// Offsets and lengths of a live entry's NAME and STRUCT tags within
/// the source metadata block. Used by the compaction path to copy
/// live entries to the alternate block without owning the source data.
///
/// Encoded enums use sentinel `0xff` to mean "absent".
///
/// **The superblock counts as an entry.** It occupies id `0` in the
/// root pair, with NAME kind `Superblock` and STRUCT kind
/// `InlineStruct` (24 byte geometry body). Compaction preserves it as
/// id `0`, so subsequent file entries start at id `1`.
#[derive(Clone, Copy)]
struct SlotOffsets {
    name_off: u16,
    name_len: u16,
    name_kind: u8, // 0 = RegularFile, 1 = Directory, 2 = Superblock, 0xff = absent
    struct_off: u16,
    struct_len: u16,
    struct_kind: u8, // 0 = InlineStruct, 1 = CtzStruct, 2 = DirStruct, 0xff = absent
}

impl SlotOffsets {
    const EMPTY: Self = Self {
        name_off: 0,
        name_len: 0,
        name_kind: 0xff,
        struct_off: 0,
        struct_len: 0,
        struct_kind: 0xff,
    };
}

/// Walk a metadata pair's tag stream, applying splice (Create/Delete)
/// renumbering, and populate `slots` with offset-and-length pointers
/// into the source buffer for each live entry's latest NAME and STRUCT
/// tags. Returns the number of live entries.
///
/// `active_is_a` selects which of `buf_a`/`buf_b` is the source.
fn gather_live_slots(
    pair: &MetadataPair<'_>,
    active_is_a: bool,
    buf_a: &[u8],
    buf_b: &[u8],
    slots: &mut [SlotOffsets; MAX_LIVE_ENTRIES],
) -> Result<usize, Error> {
    use crate::tag::TagType;

    let source: &[u8] = if active_is_a { buf_a } else { buf_b };
    let base = source.as_ptr() as usize;
    let mut count: usize = 0;

    for entry in pair.reader.iter_tags() {
        let tag = entry.tag;
        let id = tag.id() as usize;
        let body_off = (entry.body.as_ptr() as usize).saturating_sub(base);
        let body_len = entry.body.len();
        match tag.tag_type() {
            TagType::Create => {
                if count >= MAX_LIVE_ENTRIES {
                    return Err(Error::OutOfRange);
                }
                if id > count {
                    return Err(Error::Corrupt);
                }
                let mut i = count;
                while i > id {
                    slots[i] = slots[i - 1];
                    i -= 1;
                }
                slots[id] = SlotOffsets::EMPTY;
                count += 1;
            }
            TagType::Delete => {
                if id >= count {
                    return Err(Error::Corrupt);
                }
                let mut i = id;
                while i + 1 < count {
                    slots[i] = slots[i + 1];
                    i += 1;
                }
                slots[count - 1] = SlotOffsets::EMPTY;
                count -= 1;
            }
            TagType::RegularFile | TagType::Directory | TagType::Superblock => {
                if id >= count {
                    if id == count && count < MAX_LIVE_ENTRIES {
                        slots[id] = SlotOffsets::EMPTY;
                        count += 1;
                    } else {
                        return Err(Error::Corrupt);
                    }
                }
                slots[id].name_off = u16::try_from(body_off).map_err(|_| Error::OutOfRange)?;
                slots[id].name_len = u16::try_from(body_len).map_err(|_| Error::OutOfRange)?;
                slots[id].name_kind = match tag.tag_type() {
                    TagType::RegularFile => 0,
                    TagType::Directory => 1,
                    TagType::Superblock => 2,
                    _ => unreachable!(),
                };
            }
            TagType::InlineStruct | TagType::CtzStruct | TagType::DirStruct if id < count => {
                slots[id].struct_off = u16::try_from(body_off).map_err(|_| Error::OutOfRange)?;
                slots[id].struct_len = u16::try_from(body_len).map_err(|_| Error::OutOfRange)?;
                slots[id].struct_kind = match tag.tag_type() {
                    TagType::InlineStruct => 0,
                    TagType::CtzStruct => 1,
                    TagType::DirStruct => 2,
                    _ => unreachable!(),
                };
            }
            _ => {}
        }
    }
    Ok(count)
}

/// A pending write operation. Used by [`Fs::write_inline_to_root`],
/// [`Fs::remove_from_root`], and the CTZ write path to dispatch
/// through the same append-vs-compact machinery.
#[derive(Clone, Copy)]
enum WriteOp<'a> {
    /// Create a new entry at `id` (the next free id) with NAME `name`
    /// and InlineStruct `content`.
    Create { id: u16, name: &'a [u8], content: &'a [u8] },
    /// Create a new entry at `id` whose content lives in a CTZ chain
    /// at `head_block` (the chain's tail block, per LittleFS
    /// convention). `total_size` is the file's byte length.
    CreateCtz { id: u16, name: &'a [u8], head_block: u32, total_size: u32 },
    /// Update the existing entry at `id` by appending a new
    /// InlineStruct with `content`. The NAME and entry kind are
    /// preserved by the existing tags in the commit log. If the prior
    /// STRUCT was a CtzStruct, the previous chain becomes orphan and
    /// is reclaimed by the next allocator scan.
    Update { id: u16, content: &'a [u8] },
    /// Update the existing entry at `id` to point at a new CTZ chain
    /// at `head_block` with size `total_size`. The NAME and entry kind
    /// are preserved by the existing tags. The previous STRUCT (inline
    /// or CTZ) is replaced; any old CTZ chain becomes orphan.
    UpdateCtz { id: u16, head_block: u32, total_size: u32 },
    /// Remove the entry at `id`. Append path emits a `Delete` tag;
    /// compact path skips the slot and renumbers subsequent ids down.
    Remove { id: u16 },
    /// Create a new subdirectory entry at `id` with NAME `name` and
    /// `DirStruct` body pointing at `dir_pair`.
    CreateDir { id: u16, name: &'a [u8], dir_pair: BlockPair },
    /// Update the NAME tag of the entry at `id` to `new_name`. The
    /// entry's kind (`name_type`) is preserved; the STRUCT tag is
    /// untouched. Used by [`Fs::rename_in_dir`].
    RenameInPlace { id: u16, name_type: crate::tag::TagType, new_name: &'a [u8] },
}

/// Emit the tags for a [`WriteOp`] to an in-progress commit.
fn emit_op(commit: &mut crate::meta::Commit<'_>, op: &WriteOp<'_>) -> Result<(), Error> {
    use crate::tag::{Tag, TagType};
    match *op {
        WriteOp::Create { id, name, content } => {
            commit.tag(Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(Tag::new(true, TagType::RegularFile, id, name.len() as u16), name)?;
            commit.tag(Tag::new(true, TagType::InlineStruct, id, content.len() as u16), content)?;
        }
        WriteOp::CreateCtz { id, name, head_block, total_size } => {
            commit.tag(Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(Tag::new(true, TagType::RegularFile, id, name.len() as u16), name)?;
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&head_block.to_le_bytes());
            body[4..8].copy_from_slice(&total_size.to_le_bytes());
            commit.tag(Tag::new(true, TagType::CtzStruct, id, 8), &body)?;
        }
        WriteOp::Update { id, content } => {
            commit.tag(Tag::new(true, TagType::InlineStruct, id, content.len() as u16), content)?;
        }
        WriteOp::UpdateCtz { id, head_block, total_size } => {
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&head_block.to_le_bytes());
            body[4..8].copy_from_slice(&total_size.to_le_bytes());
            commit.tag(Tag::new(true, TagType::CtzStruct, id, 8), &body)?;
        }
        WriteOp::Remove { id } => {
            // Delete tag's length field is the special sentinel 0x3FF
            // (no body). Subsequent entries with higher ids renumber
            // down at read time via `dir::live_entries`'s splice
            // handling.
            commit.tag(Tag::new(true, TagType::Delete, id, 0x3FF), &[])?;
        }
        WriteOp::CreateDir { id, name, dir_pair } => {
            commit.tag(Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(Tag::new(true, TagType::Directory, id, name.len() as u16), name)?;
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&dir_pair.a.as_u32().to_le_bytes());
            body[4..8].copy_from_slice(&dir_pair.b.as_u32().to_le_bytes());
            commit.tag(Tag::new(true, TagType::DirStruct, id, 8), &body)?;
        }
        WriteOp::RenameInPlace { id, name_type, new_name } => {
            // Append a NAME tag at the existing id with the new
            // bytes. The reader's lookup picks the latest NAME for a
            // given id, so the entry surfaces under `new_name`.
            commit.tag(Tag::new(true, name_type, id, new_name.len() as u16), new_name)?;
        }
    }
    Ok(())
}

/// Build a compacted commit on `alt_buf`: replay every live entry from
/// `slots[..count]` (reading source bytes from `source_buf`) and apply
/// `op` (either creating a new entry at the end or replacing an
/// existing entry's struct body). Returns the total bytes written.
/// `alt_buf` is pre-filled with `0xFF` (erased state).
#[allow(clippy::too_many_arguments)]
fn build_compact_commit(
    alt_buf: &mut [u8],
    source_buf: &[u8],
    new_revision: u32,
    slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
    count: usize,
    op: &WriteOp<'_>,
    prog_size: usize,
    block_size: usize,
) -> Result<usize, Error> {
    use crate::tag::TagType;

    for b in alt_buf.iter_mut() {
        *b = 0xFF;
    }
    let mut commit = crate::meta::Commit::new(alt_buf, new_revision)?;

    // `emit_id` is the id assigned in the compacted output; it diverges
    // from the source slot index after a Remove skip.
    let mut emit_id: u16 = 0;
    for (i, s) in slots.iter().enumerate().take(count) {
        if let WriteOp::Remove { id: remove_id } = *op {
            if (i as u16) == remove_id {
                continue; // drop this entry; do not bump emit_id
            }
        }
        if s.name_kind == 0xff || s.struct_kind == 0xff {
            return Err(Error::Corrupt);
        }
        let name_type = match s.name_kind {
            0 => TagType::RegularFile,
            1 => TagType::Directory,
            2 => TagType::Superblock,
            _ => return Err(Error::Corrupt),
        };
        let struct_type = match s.struct_kind {
            0 => TagType::InlineStruct,
            1 => TagType::CtzStruct,
            2 => TagType::DirStruct,
            _ => return Err(Error::Corrupt),
        };
        let name = &source_buf[s.name_off as usize..s.name_off as usize + s.name_len as usize];
        commit.tag(crate::tag::Tag::new(true, TagType::Create, emit_id, 0), &[])?;
        commit.tag(crate::tag::Tag::new(true, name_type, emit_id, s.name_len), name)?;

        // If this is the target of an Update, substitute the new content
        // for the struct body. Otherwise copy the source's struct as-is.
        if let WriteOp::Update { id: update_id, content } = *op {
            if (i as u16) == update_id {
                commit.tag(
                    crate::tag::Tag::new(
                        true,
                        TagType::InlineStruct,
                        emit_id,
                        content.len() as u16,
                    ),
                    content,
                )?;
                emit_id += 1;
                continue;
            }
        }
        // UpdateCtz overrides the entry's struct body with the new
        // (head_block, total_size). Without this case, an UpdateCtz
        // that triggers compaction copies the prior struct body and
        // silently drops the update.
        if let WriteOp::UpdateCtz { id: update_id, head_block, total_size } = *op {
            if (i as u16) == update_id {
                let mut body = [0u8; 8];
                body[0..4].copy_from_slice(&head_block.to_le_bytes());
                body[4..8].copy_from_slice(&total_size.to_le_bytes());
                commit.tag(crate::tag::Tag::new(true, TagType::CtzStruct, emit_id, 8), &body)?;
                emit_id += 1;
                continue;
            }
        }
        // RenameInPlace overrides the NAME emitted above by emitting a
        // newer NAME after, but for compact we want to emit just one
        // NAME. Backtrack: re-emit the slot from scratch using the new
        // name. We've already emitted Create + NAME(old); emit a
        // newer NAME(new) which shadows the old at read time. The
        // STRUCT below follows normally.
        if let WriteOp::RenameInPlace { id: rename_id, name_type, new_name } = *op {
            if (i as u16) == rename_id {
                commit.tag(
                    crate::tag::Tag::new(true, name_type, emit_id, new_name.len() as u16),
                    new_name,
                )?;
                // Fall through to emit struct body unchanged.
            }
        }
        let struct_body =
            &source_buf[s.struct_off as usize..s.struct_off as usize + s.struct_len as usize];
        commit.tag(crate::tag::Tag::new(true, struct_type, emit_id, s.struct_len), struct_body)?;
        emit_id += 1;
    }

    // Append the new entry at id == emit_id.
    match *op {
        WriteOp::Create { id, name, content } => {
            debug_assert_eq!(id, emit_id, "Create id must equal post-replay emit count");
            commit.tag(crate::tag::Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(
                crate::tag::Tag::new(true, TagType::RegularFile, id, name.len() as u16),
                name,
            )?;
            commit.tag(
                crate::tag::Tag::new(true, TagType::InlineStruct, id, content.len() as u16),
                content,
            )?;
        }
        WriteOp::CreateCtz { id, name, head_block, total_size } => {
            debug_assert_eq!(id, emit_id, "CreateCtz id must equal post-replay emit count");
            commit.tag(crate::tag::Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(
                crate::tag::Tag::new(true, TagType::RegularFile, id, name.len() as u16),
                name,
            )?;
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&head_block.to_le_bytes());
            body[4..8].copy_from_slice(&total_size.to_le_bytes());
            commit.tag(crate::tag::Tag::new(true, TagType::CtzStruct, id, 8), &body)?;
        }
        WriteOp::CreateDir { id, name, dir_pair } => {
            debug_assert_eq!(id, emit_id, "CreateDir id must equal post-replay emit count");
            commit.tag(crate::tag::Tag::new(true, TagType::Create, id, 0), &[])?;
            commit
                .tag(crate::tag::Tag::new(true, TagType::Directory, id, name.len() as u16), name)?;
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&dir_pair.a.as_u32().to_le_bytes());
            body[4..8].copy_from_slice(&dir_pair.b.as_u32().to_le_bytes());
            commit.tag(crate::tag::Tag::new(true, TagType::DirStruct, id, 8), &body)?;
        }
        _ => {}
    }
    commit.finish_padded(0, prog_size, block_size)?;
    Ok(commit.bytes_written())
}

/// Wire-level commit size for a [`WriteOp`], including the trailing
/// CCRC tag (8 bytes). Used by callers to decide whether to append in
/// place or compact onto the alternate.
fn op_dsize_of(op: &WriteOp<'_>) -> usize {
    match *op {
        WriteOp::Update { content, .. } => (4 + content.len()) + 8,
        WriteOp::UpdateCtz { .. } => (4 + 8) + 8,
        WriteOp::Create { name, content, .. } => 4 + (4 + name.len()) + (4 + content.len()) + 8,
        WriteOp::CreateCtz { name, .. } | WriteOp::CreateDir { name, .. } => {
            4 + (4 + name.len()) + (4 + 8) + 8
        }
        WriteOp::Remove { .. } => 4 + 8,
        WriteOp::RenameInPlace { new_name, .. } => (4 + new_name.len()) + 8,
    }
}

/// `true` if every byte of `buf` is `0xFF`. This is the erased state
/// for NOR / NAND flash; both blocks of the root metadata pair in
/// this condition means the device has never been formatted (versus
/// "formatted but corrupted", where at least one bit has been
/// programmed somewhere).
fn is_all_erased(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0xFF)
}

/// `true` if `ancestor` is a *strict* ancestor of `descendant`: every
/// component of `ancestor` appears as a prefix of `descendant`'s
/// component sequence, and `descendant` has at least one further
/// component. `path_is_strict_ancestor(p, p)` is `false`.
fn path_is_strict_ancestor(
    ancestor: crate::path::Path<'_>,
    descendant: crate::path::Path<'_>,
) -> bool {
    let mut a = ancestor.components();
    let mut d = descendant.components();
    loop {
        match (a.next(), d.next()) {
            (Some(ac), Some(dc)) if ac == dc => {}
            (None, Some(_)) => return true,
            _ => return false,
        }
    }
}

/// A path resolution result.
///
/// Returned by [`Fs::resolve`]. The `entry`, `struct_type`, and
/// `struct_body` are the same fields [`crate::dir::Resolved`] carries;
/// `pair` adds the metadata pair address where the entry was found
/// (useful for follow-up reads, e.g., descending into the directory if
/// `entry.kind == Directory`).
#[derive(Clone, Copy, Debug)]
pub struct ResolvedPath<'b> {
    /// Address of the metadata pair holding the resolved entry.
    pub pair: BlockPair,
    /// The directory entry.
    pub entry: crate::dir::DirEntry<'b>,
    /// The type of the STRUCT tag that paired with the NAME.
    pub struct_type: crate::tag::TagType,
    /// The STRUCT tag's body bytes.
    pub struct_body: &'b [u8],
}

/// A mounted LittleFS filesystem.
///
/// Constructed by [`Fs::mount`]. Owns the underlying [`Storage`] and the
/// decoded superblock state. Directory and file APIs land in Phase 1e/1f.
#[derive(Debug)]
pub struct Fs<S: Storage> {
    storage: S,
    superblock: Superblock,
    root: BlockPair,
}

impl<S: Storage> Fs<S> {
    /// Threshold above which [`Self::write_to_root`] switches from
    /// inline storage (the file content lives inside the metadata pair
    /// as an `InlineStruct` body) to CTZ storage (the file content
    /// lives in dedicated blocks linked by a skip list).
    ///
    /// Files at or below this size go inline; larger files go CTZ.
    /// The threshold is conservative; callers wanting more control can
    /// call [`Self::write_inline_to_root`] (always inline, up to 1023
    /// bytes) or [`Self::write_ctz_to_root`] (always CTZ).
    pub const INLINE_MAX: usize = 128;

    /// Write or update a file at the filesystem root, choosing the
    /// storage layout (inline vs CTZ) based on content size.
    ///
    /// Files at or below [`Self::INLINE_MAX`] bytes are stored inline;
    /// larger files are stored as a CTZ skip list. The user-visible
    /// API is the same in both cases: subsequent `resolve` calls
    /// return either an `InlineStruct` whose body is the content, or a
    /// `CtzStruct` whose body parses into a `(head_block, size)` pair
    /// that [`Self::read_ctz`] reassembles.
    ///
    /// **Update semantics are inline-only.** Re-writing an existing
    /// name with a content size above `INLINE_MAX` returns
    /// [`Error::OutOfRange`] for now. The general case requires
    /// freeing the old CTZ chain plus allocating a new one, which is
    /// safe but unimplemented (Phase 2f follow-up).
    pub fn write_to_root(
        &mut self,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if content.len() <= Self::INLINE_MAX {
            self.write_inline_to_root(name, content, buf_a, buf_b)
        } else {
            self.write_ctz_to_root(name, content, buf_a, buf_b)
        }
    }

    /// Write a file at the filesystem root as a CTZ skip list.
    ///
    /// Allocates fresh blocks for the chain, writes them, then appends
    /// a metadata commit with `Create` + `RegularFile` NAME +
    /// `CtzStruct` referencing the head block.
    ///
    /// **Scope.** Root-only, create-only (not update). The name must
    /// not already exist. CTZ-on-CTZ updates (freeing the old chain
    /// and allocating a new one) are a Phase 2f follow-up.
    pub fn write_ctz_to_root(
        &mut self,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.write_ctz_to_pair(self.root, name, content, buf_a, buf_b)
    }

    /// Internal: write a CTZ-backed file to the given metadata pair.
    fn write_ctz_to_pair(
        &mut self,
        pair_addr: BlockPair,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        use crate::ctz::{block_count, skip_pointers_in_block};

        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if name.is_empty() || name.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }
        // Pre-check: detect whether the entry exists, and reject if it
        // exists as a Directory (overwriting a directory with a file
        // is destructive and not supported here; use rmdir + write
        // separately).
        let existing_id: Option<u16> = {
            self.storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
            self.storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            match crate::dir::lookup(&pair, name) {
                Some(r) => {
                    if r.entry.kind != crate::dir::EntryKind::RegularFile {
                        return Err(Error::AlreadyExists);
                    }
                    Some(r.entry.id)
                }
                None => None,
            }
        };

        let bs = S::BLOCK_SIZE as u32;
        let total_blocks_u32 = block_count(content.len() as u32, bs);
        if total_blocks_u32 == 0 {
            // Empty content -> degenerate; CTZ requires at least one block.
            // Fall back to inline (which handles size==0 cleanly).
            return self.write_inline_to_pair(pair_addr, name, content, buf_a, buf_b);
        }
        let total_blocks = total_blocks_u32 as usize;
        if total_blocks > MAX_CTZ_WRITE_BLOCKS {
            return Err(Error::OutOfRange);
        }

        // Allocate physical blocks for the chain.
        let mut chain = [BlockAddress::NONE; MAX_CTZ_WRITE_BLOCKS];
        crate::alloc::alloc_blocks(
            &mut self.storage,
            self.root,
            &mut chain[..total_blocks],
            buf_a,
            buf_b,
        )?;

        // Build each chain block in `buf_a`, erase + program it.
        let mut content_off = 0usize;
        for i in 0..total_blocks {
            let i32 = i as u32;
            let header = 4 * skip_pointers_in_block(i32) as usize;
            // Fill block with 0xFF (erased state).
            for b in buf_a.iter_mut() {
                *b = 0xFF;
            }
            // Skip pointers: block i has ctz(i)+1 pointers addressing
            // blocks i - 2^k for k = 0..=ctz(i). Each pointer is the
            // physical address of that chain index.
            let pointer_count = skip_pointers_in_block(i32) as usize;
            for k in 0..pointer_count {
                let target_idx = i - (1 << k);
                let target_phys = chain[target_idx].as_u32();
                let off = 4 * k;
                buf_a[off..off + 4].copy_from_slice(&target_phys.to_le_bytes());
            }
            // Content slice.
            let block_capacity = S::BLOCK_SIZE - header;
            let take = block_capacity.min(content.len() - content_off);
            buf_a[header..header + take].copy_from_slice(&content[content_off..content_off + take]);
            content_off += take;

            let phys = chain[i].as_u32();
            self.storage.erase(phys).map_err(|_| Error::Io)?;
            self.storage.program(phys, 0, &buf_a[..S::BLOCK_SIZE]).map_err(|_| Error::Io)?;
        }
        self.storage.sync().map_err(|_| Error::Io)?;

        // Append the metadata commit.
        // buf_a was consumed for chain bytes; re-read the target pair.
        self.storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;

        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        {
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == pair_addr.a;
            committed_end = pair.reader.committed_end();
            next_ptag = pair.reader.next_ptag();
            old_revision = pair.reader.revision();
            count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let head_block = chain[total_blocks - 1].as_u32();
        let total_size = content.len() as u32;
        let op = if let Some(id) = existing_id {
            // Replacing an existing file: emit an UpdateCtz that
            // overrides the entry's struct body. The previous chain
            // becomes unreachable and gets reclaimed by the next
            // allocator scan.
            WriteOp::UpdateCtz { id, head_block, total_size }
        } else {
            let new_id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
            if new_id == crate::tag::ID_NONE {
                return Err(Error::OutOfRange);
            }
            WriteOp::CreateCtz { id: new_id, name, head_block, total_size }
        };
        let op_dsize = match op {
            WriteOp::UpdateCtz { .. } => (4 + 8) + 8,
            // Create: Create (4) + NAME (4+name.len) + CtzStruct (4+8) + CCRC (8).
            _ => 4 + (4 + name.len()) + (4 + 8) + 8,
        };

        if committed_end + op_dsize <= S::BLOCK_SIZE {
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, &op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Read up to `out.len()` bytes from the file at `path` starting
    /// at `offset`. Returns the number of bytes copied (may be less
    /// than `out.len()` if `offset + out.len() > file_size`).
    ///
    /// Works for both inline (`InlineStruct`) and CTZ-backed files;
    /// the layout is hidden from callers.
    pub fn read_at_path(
        &mut self,
        path: crate::path::Path<'_>,
        offset: u32,
        out: &mut [u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<usize, Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        // Decode the entry's layout. The body_copy inline buffer caps
        // inline files at 1023 bytes (the LittleFS tag length limit).
        // The Inline variant is much larger than Ctz; the size diff is
        // intentional and bounded by the inline tag length.
        #[allow(clippy::large_enum_variant)]
        enum Layout {
            Inline { size: usize, body_copy: [u8; 1024] },
            Ctz(crate::ctz::CtzStruct),
        }
        let layout = {
            let r = self.resolve(path, buf_a, buf_b)?;
            if r.entry.kind != crate::dir::EntryKind::RegularFile {
                return Err(Error::AlreadyExists);
            }
            match r.struct_type {
                crate::tag::TagType::InlineStruct => {
                    let n = r.struct_body.len();
                    if n > 1024 {
                        return Err(Error::OutOfRange);
                    }
                    let mut body_copy = [0u8; 1024];
                    body_copy[..n].copy_from_slice(r.struct_body);
                    Layout::Inline { size: n, body_copy }
                }
                crate::tag::TagType::CtzStruct => {
                    Layout::Ctz(crate::ctz::CtzStruct::from_bytes(r.struct_body)?)
                }
                _ => return Err(Error::Corrupt),
            }
        };

        match layout {
            Layout::Inline { size, body_copy } => {
                let off = offset as usize;
                if off >= size {
                    return Ok(0);
                }
                let take = (size - off).min(out.len());
                out[..take].copy_from_slice(&body_copy[off..off + take]);
                Ok(take)
            }
            Layout::Ctz(ctz) => {
                crate::ctz::read_ctz_at(&mut self.storage, &ctz, offset, out, buf_a)
            }
        }
    }

    /// Return the size in bytes of the file at `path`.
    pub fn size_of(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<u32, Error> {
        let r = self.resolve(path, buf_a, buf_b)?;
        if r.entry.kind != crate::dir::EntryKind::RegularFile {
            return Err(Error::AlreadyExists);
        }
        match r.struct_type {
            crate::tag::TagType::InlineStruct => Ok(r.struct_body.len() as u32),
            crate::tag::TagType::CtzStruct => {
                Ok(crate::ctz::CtzStruct::from_bytes(r.struct_body)?.size)
            }
            _ => Err(Error::Corrupt),
        }
    }

    /// For a CTZ-backed file, return the number of bytes that can be
    /// appended to the existing tail block without allocating a new
    /// one. Lets a write-heavy caller (a log writer batching entries
    /// at fixed cadence) pack appends so that overflow always arrives
    /// on a block boundary, minimizing the number of new-block
    /// allocations.
    ///
    /// Returns `0` for inline files (no tail block yet); the next
    /// append either grows the inline body or triggers the
    /// inline-to-CTZ transition.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if `path` does not exist.
    /// - [`Error::AlreadyExists`] if `path` resolves to a directory
    ///   (no tail block concept).
    /// - [`Error::Corrupt`] for a malformed entry.
    pub fn tail_room(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<u32, Error> {
        use crate::ctz::{
            block_count, block_index_at_offset, content_bytes_in_block, skip_pointers_in_block,
            CtzStruct,
        };
        let r = self.resolve(path, buf_a, buf_b)?;
        if r.entry.kind != crate::dir::EntryKind::RegularFile {
            return Err(Error::AlreadyExists);
        }
        match r.struct_type {
            crate::tag::TagType::InlineStruct => Ok(0),
            crate::tag::TagType::CtzStruct => {
                let ctz = CtzStruct::from_bytes(r.struct_body)?;
                if ctz.size == 0 {
                    return Ok(0);
                }
                let bs = S::BLOCK_SIZE as u32;
                let n = block_count(ctz.size, bs);
                let tail_idx = n - 1;
                let header = 4 * skip_pointers_in_block(tail_idx);
                let tail_cap = content_bytes_in_block(tail_idx, bs);
                let (idx_check, abs_off) = block_index_at_offset(ctz.size - 1, bs);
                debug_assert_eq!(idx_check, tail_idx);
                let bytes_used = abs_off + 1 - header;
                Ok(tail_cap - bytes_used)
            }
            _ => Err(Error::Corrupt),
        }
    }

    /// Truncate the file at `path` to exactly `new_size` bytes.
    ///
    /// If `new_size < current_size`, the trailing bytes are dropped.
    /// If `new_size > current_size`, the file is zero-extended.
    /// Atomic full-rewrite (same model as [`Self::append_to_path`]).
    pub fn truncate_path(
        &mut self,
        path: crate::path::Path<'_>,
        new_size: u32,
        content_scratch: &mut [u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if (new_size as usize) > content_scratch.len() {
            return Err(Error::OutOfRange);
        }
        let current_size = match self.size_of(path, buf_a, buf_b) {
            Ok(s) => s,
            Err(Error::NotFound) => 0,
            Err(e) => return Err(e),
        };
        let copy_len = new_size.min(current_size);
        if copy_len > 0 {
            let n = self.read_at_path(
                path,
                0,
                &mut content_scratch[..copy_len as usize],
                buf_a,
                buf_b,
            )?;
            debug_assert_eq!(n, copy_len as usize);
        }
        if new_size > current_size {
            for byte in &mut content_scratch[current_size as usize..new_size as usize] {
                *byte = 0;
            }
        }
        self.write_to_path(path, &content_scratch[..new_size as usize], buf_a, buf_b)
    }

    /// Append bytes to the file at `path`. Creates the file if it does
    /// not exist.
    ///
    /// **Storage model.** This is *streaming* for CTZ-backed files: the
    /// existing chain is left intact, the trailing partial block is
    /// filled in place (NOR allows programming bytes that are still
    /// erased), and any overflow allocates exactly enough new blocks to
    /// hold the new tail. Write amplification per append is bounded by
    /// `additional.len() + (one block alloc/erase per ~block_size of
    /// overflow)` rather than scaling with the existing file size.
    ///
    /// The inline-file paths (missing file, inline-to-inline,
    /// inline-to-CTZ transition) still assemble the combined content in
    /// `content_scratch` and dispatch through
    /// [`Self::write_to_path`]. For CTZ-extending appends (the common
    /// case for log-style writers once the file outgrows
    /// [`Self::INLINE_MAX`]) `content_scratch` is **not consulted**;
    /// callers may pass an empty slice. The inline path needs
    /// `content_scratch.len() >= existing_size + additional.len()`,
    /// where `existing_size <= INLINE_MAX` until the transition fires.
    ///
    /// The directory-entry update at the end of the streaming append is
    /// the only commit that becomes visible after a successful return;
    /// crash before that commit lands leaves the file at its pre-append
    /// size and the newly allocated blocks unreferenced (reclaimed by
    /// the next allocator scan).
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfRange`] if the inline-path `content_scratch` is
    ///   too small, or the new total file size would need more than
    ///   `MAX_CTZ_WRITE_BLOCKS` chain blocks.
    /// - [`Error::AlreadyExists`] if the path exists but is a directory.
    /// - Storage and corruption errors propagate from underlying
    ///   reads/writes.
    pub fn append_to_path(
        &mut self,
        path: crate::path::Path<'_>,
        additional: &[u8],
        content_scratch: &mut [u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if additional.is_empty() {
            return Ok(());
        }

        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;
        let name = leaf.as_bytes();
        if name.is_empty() || name.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }

        // Read the parent pair once and classify the existing entry.
        self.storage.read(parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;

        // Inline body is copied out so we can drop the pair borrow.
        // Up to INLINE_MAX + 1 covers a freshly transitioned entry; we
        // size the buffer slightly larger to absorb any future bump.
        let mut inline_copy = [0u8; 256];
        enum Layout {
            Missing,
            Inline { size: usize },
            Ctz { ctz: crate::ctz::CtzStruct, id: u16 },
        }
        let layout = {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            match crate::dir::lookup(&p, name) {
                None => Layout::Missing,
                Some(r) => {
                    if r.entry.kind != crate::dir::EntryKind::RegularFile {
                        return Err(Error::AlreadyExists);
                    }
                    match r.struct_type {
                        crate::tag::TagType::InlineStruct => {
                            let n = r.struct_body.len();
                            if n > inline_copy.len() {
                                return Err(Error::OutOfRange);
                            }
                            inline_copy[..n].copy_from_slice(r.struct_body);
                            Layout::Inline { size: n }
                        }
                        crate::tag::TagType::CtzStruct => {
                            let ctz = crate::ctz::CtzStruct::from_bytes(r.struct_body)?;
                            Layout::Ctz { ctz, id: r.entry.id }
                        }
                        _ => return Err(Error::Corrupt),
                    }
                }
            }
        };

        match layout {
            Layout::Missing => self.write_to_path(path, additional, buf_a, buf_b),
            Layout::Inline { size } => {
                let total = size + additional.len();
                if total > content_scratch.len() {
                    return Err(Error::OutOfRange);
                }
                content_scratch[..size].copy_from_slice(&inline_copy[..size]);
                content_scratch[size..total].copy_from_slice(additional);
                self.write_to_path(path, &content_scratch[..total], buf_a, buf_b)
            }
            Layout::Ctz { ctz, id } => {
                self.append_ctz_streaming(parent, id, ctz, additional, buf_a, buf_b)
            }
        }
    }

    /// Streaming CTZ append: fill the existing tail block in place via
    /// NOR sub-block programs, then allocate and write any overflow
    /// blocks, then commit a single `UpdateCtz` tag pointing at the new
    /// head + size. Existing chain blocks are never re-erased.
    fn append_ctz_streaming(
        &mut self,
        pair_addr: BlockPair,
        entry_id: u16,
        ctz: crate::ctz::CtzStruct,
        data: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        use crate::ctz::{
            block_count, block_index_at_offset, collect_chain_blocks, content_bytes_in_block,
            skip_pointers_in_block,
        };

        let bs = S::BLOCK_SIZE as u32;
        let old_size = ctz.size;
        let n_old = block_count(old_size, bs);
        if (n_old as usize) > MAX_CTZ_WRITE_BLOCKS {
            return Err(Error::OutOfRange);
        }

        // Collect the existing chain's physical addresses. The walk
        // reads only skip-pointer headers (4 or 8 bytes per block), so
        // it touches very little flash and never reads near the tail's
        // free region.
        let mut chain = [BlockAddress::NONE; MAX_CTZ_WRITE_BLOCKS];
        if n_old > 0 {
            collect_chain_blocks(
                &mut self.storage,
                ctz.head_block,
                n_old,
                &mut chain[..n_old as usize],
            )?;
        }

        // Step 1: pack as much of `data` as fits into the existing tail
        // block's unused content region. The bytes there are still
        // 0xFF (never programmed since erase), so the NOR-aligned
        // wrapper can program through this offset safely.
        let mut data_consumed: usize = 0;
        let mut head_phys = ctz.head_block.as_u32();
        if n_old > 0 {
            let tail_idx = n_old - 1;
            let header = 4 * skip_pointers_in_block(tail_idx);
            let tail_cap = content_bytes_in_block(tail_idx, bs);
            let (idx_check, abs_off) = block_index_at_offset(old_size - 1, bs);
            debug_assert_eq!(idx_check, tail_idx);
            let bytes_used = abs_off + 1 - header;
            let room = tail_cap - bytes_used;
            let fill = (room as usize).min(data.len());
            if fill > 0 {
                let tail_phys = chain[tail_idx as usize].as_u32();
                self.storage
                    .program(tail_phys, header + bytes_used, &data[..fill])
                    .map_err(|_| Error::Io)?;
                data_consumed = fill;
                head_phys = tail_phys;
            }
        }

        // Step 2: allocate new chain blocks for the remainder. Each new
        // block stores skip pointers referencing earlier chain entries
        // (existing or newly allocated) followed by content bytes.
        if data_consumed < data.len() {
            let new_total = old_size + data.len() as u32;
            let new_n = block_count(new_total, bs);
            if (new_n as usize) > MAX_CTZ_WRITE_BLOCKS {
                return Err(Error::OutOfRange);
            }
            let new_start = n_old as usize;
            let new_count = (new_n - n_old) as usize;

            crate::alloc::alloc_blocks(
                &mut self.storage,
                self.root,
                &mut chain[new_start..new_start + new_count],
                buf_a,
                buf_b,
            )?;

            let remaining = &data[data_consumed..];
            let mut content_off = 0usize;
            for new_i in n_old..new_n {
                let header_bytes = 4 * skip_pointers_in_block(new_i) as usize;
                for b in buf_a.iter_mut() {
                    *b = 0xFF;
                }
                let ptr_count = skip_pointers_in_block(new_i) as usize;
                for k in 0..ptr_count {
                    let target_idx = (new_i as usize).checked_sub(1 << k).ok_or(Error::Corrupt)?;
                    let target_phys = chain[target_idx].as_u32();
                    let off = 4 * k;
                    buf_a[off..off + 4].copy_from_slice(&target_phys.to_le_bytes());
                }
                let block_capacity = S::BLOCK_SIZE - header_bytes;
                let take = block_capacity.min(remaining.len() - content_off);
                buf_a[header_bytes..header_bytes + take]
                    .copy_from_slice(&remaining[content_off..content_off + take]);
                content_off += take;

                let phys = chain[new_i as usize].as_u32();
                self.storage.erase(phys).map_err(|_| Error::Io)?;
                self.storage.program(phys, 0, &buf_a[..S::BLOCK_SIZE]).map_err(|_| Error::Io)?;
            }
            head_phys = chain[new_n as usize - 1].as_u32();
        }

        self.storage.sync().map_err(|_| Error::Io)?;

        // Commit the directory-entry update. This is the atomic step:
        // anything before the commit lands leaves the file at its
        // pre-append state.
        let new_size = old_size + data.len() as u32;
        self.commit_update_ctz(pair_addr, entry_id, head_phys, new_size, buf_a, buf_b)
    }

    /// Emit an `UpdateCtz` commit on `pair_addr` pointing entry `id` at
    /// `(head_block, total_size)`. Dispatches through the standard
    /// append-or-compact machinery used by every other write op.
    fn commit_update_ctz(
        &mut self,
        pair_addr: BlockPair,
        id: u16,
        head_block: u32,
        total_size: u32,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;

        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        {
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == pair_addr.a;
            committed_end = pair.reader.committed_end();
            next_ptag = pair.reader.next_ptag();
            old_revision = pair.reader.revision();
            count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let op = WriteOp::UpdateCtz { id, head_block, total_size };
        let op_dsize = (4 + 8) + 8;

        if committed_end + op_dsize <= S::BLOCK_SIZE {
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, &op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Resolve a path's parent directory. Returns `(parent_pair,
    /// leaf_name)` where `parent_pair` is the metadata pair of the
    /// directory that should contain the leaf component, and
    /// `leaf_name` is the final path component as a `&str`.
    ///
    /// Returns [`Error::InvalidPath`] for the root path (no parent).
    /// Returns [`Error::NotFound`] if any intermediate component does
    /// not resolve to a Directory.
    fn resolve_parent<'p>(
        &mut self,
        path: crate::path::Path<'p>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(BlockPair, &'p str), Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if path.is_root() {
            return Err(Error::InvalidPath);
        }
        let mut current = self.root;
        let mut components = path.components().peekable();
        loop {
            let name = components.next().ok_or(Error::InvalidPath)?;
            if components.peek().is_none() {
                return Ok((current, name));
            }
            current = self.find_dir_pair(current, name.as_bytes(), buf_a, buf_b)?;
        }
    }

    /// Create a new directory at `path`. The parent directory must
    /// exist; the last component must not exist.
    ///
    /// Allocates a fresh metadata pair for the new directory, erases
    /// and initializes it (with revision 1 and one empty CCRC commit),
    /// then appends a `Create` + `Directory` NAME + `DirStruct` commit
    /// to the parent pair pointing at the new dir's blocks.
    ///
    /// # Errors
    ///
    /// - [`Error::AlreadyExists`] if the leaf component already exists
    ///   in the parent.
    /// - [`Error::NotFound`] if any intermediate component is missing.
    /// - [`Error::OutOfRange`] if the device has no free blocks for
    ///   the new pair.
    /// - Storage and corruption errors propagate from underlying
    ///   reads/programs.
    pub fn mkdir(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (parent, dir_name) = self.resolve_parent(path, buf_a, buf_b)?;
        let dir_name_bytes = dir_name.as_bytes();
        if dir_name_bytes.is_empty() || dir_name_bytes.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }

        // Verify the name doesn't already exist in the parent.
        self.storage.read(parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            if crate::dir::lookup(&p, dir_name_bytes).is_some() {
                return Err(Error::AlreadyExists);
            }
        }

        // Allocate two blocks for the new directory's metadata pair.
        let mut new_blocks = [BlockAddress::NONE; 2];
        crate::alloc::alloc_blocks(&mut self.storage, self.root, &mut new_blocks, buf_a, buf_b)?;
        let new_dir = BlockPair::new(new_blocks[0], new_blocks[1]);

        // Initialize: erase both blocks, then write an empty commit
        // (revision 1 + CCRC, no entries) on block A. Block B remains
        // pristine erased as the alternate.
        self.storage.erase(new_dir.a.as_u32()).map_err(|_| Error::Io)?;
        self.storage.erase(new_dir.b.as_u32()).map_err(|_| Error::Io)?;
        for byte in buf_a.iter_mut() {
            *byte = 0xFF;
        }
        let new_end = {
            let mut commit = crate::meta::Commit::new(&mut buf_a[..S::BLOCK_SIZE], 1)?;
            commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
            commit.bytes_written()
        };
        self.storage.program(new_dir.a.as_u32(), 0, &buf_a[..new_end]).map_err(|_| Error::Io)?;
        self.storage.sync().map_err(|_| Error::Io)?;

        // Append a CreateDir commit to the parent pair.
        self.storage.read(parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            active_addr = p.active_block;
            alternate_addr = p.alternate_block;
            active_is_a = active_addr == parent.a;
            committed_end = p.reader.committed_end();
            next_ptag = p.reader.next_ptag();
            old_revision = p.reader.revision();
            count = gather_live_slots(&p, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let new_id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
        if new_id == crate::tag::ID_NONE {
            return Err(Error::OutOfRange);
        }
        let op = WriteOp::CreateDir { id: new_id, name: dir_name_bytes, dir_pair: new_dir };
        let op_dsize = 4 + (4 + dir_name_bytes.len()) + (4 + 8) + 8;

        if committed_end + op_dsize <= S::BLOCK_SIZE {
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, &op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Write or update a file at `path` (path-based variant of
    /// [`Self::write_to_root`]). Auto-dispatches inline vs CTZ based on
    /// content size. The parent directory must exist.
    pub fn write_to_path(
        &mut self,
        path: crate::path::Path<'_>,
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;
        let name = leaf.as_bytes();
        if content.len() <= Self::INLINE_MAX {
            self.write_inline_to_pair(parent, name, content, buf_a, buf_b)
        } else {
            self.write_ctz_to_pair(parent, name, content, buf_a, buf_b)
        }
    }

    /// Remove a file at `path`. Returns [`Error::NotFound`] if the
    /// file does not exist; [`Error::AlreadyExists`] if the path
    /// resolves to a directory (use [`Self::rmdir`] instead).
    pub fn remove_at_path(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;
        // Reject directories: removing a directory entry without
        // checking emptiness would orphan its content. Use rmdir.
        self.storage.read(parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            match crate::dir::lookup(&p, leaf.as_bytes()) {
                Some(r) if r.entry.kind == crate::dir::EntryKind::Directory => {
                    return Err(Error::AlreadyExists);
                }
                Some(_) => {}
                None => return Err(Error::NotFound),
            }
        }
        self.remove_from_pair(parent, leaf.as_bytes(), buf_a, buf_b)
    }

    /// Rename an entry in place within its current directory.
    ///
    /// Both paths must share the same parent directory. Appends a new
    /// NAME tag at the existing entry's id with `new_name`; the
    /// reader picks the latest NAME for a given id, so the entry now
    /// surfaces under the new name.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if the source path does not exist.
    /// - [`Error::InvalidPath`] if the two paths have different
    ///   parent directories, or if either is the root.
    /// - [`Error::AlreadyExists`] if `new_name` already exists in the
    ///   parent and references a different id.
    pub fn rename_in_dir(
        &mut self,
        old_path: crate::path::Path<'_>,
        new_path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (old_parent, old_leaf) = self.resolve_parent(old_path, buf_a, buf_b)?;
        let (new_parent, new_leaf) = self.resolve_parent(new_path, buf_a, buf_b)?;
        if old_parent != new_parent {
            return Err(Error::InvalidPath);
        }
        let parent = old_parent;
        let new_leaf_bytes = new_leaf.as_bytes();
        let old_leaf_bytes = old_leaf.as_bytes();
        if new_leaf_bytes.is_empty() || new_leaf_bytes.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }
        if old_leaf_bytes == new_leaf_bytes {
            return Ok(()); // no-op
        }

        // Resolve old entry; reject collision with a different entry.
        self.storage.read(parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let (old_id, kind);
        {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            let old_res = crate::dir::lookup(&p, old_leaf_bytes).ok_or(Error::NotFound)?;
            old_id = old_res.entry.id;
            kind = old_res.entry.kind;
            if let Some(collision) = crate::dir::lookup(&p, new_leaf_bytes) {
                if collision.entry.id != old_id {
                    return Err(Error::AlreadyExists);
                }
            }
        }

        // Gather pair state for the append-or-compact dispatch.
        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            active_addr = p.active_block;
            alternate_addr = p.alternate_block;
            active_is_a = active_addr == parent.a;
            committed_end = p.reader.committed_end();
            next_ptag = p.reader.next_ptag();
            old_revision = p.reader.revision();
            count = gather_live_slots(&p, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let name_type = match kind {
            crate::dir::EntryKind::RegularFile => crate::tag::TagType::RegularFile,
            crate::dir::EntryKind::Directory => crate::tag::TagType::Directory,
        };
        let op = WriteOp::RenameInPlace { id: old_id, name_type, new_name: new_leaf_bytes };
        let op_dsize = (4 + new_leaf_bytes.len()) + 8;

        if committed_end + op_dsize <= S::BLOCK_SIZE {
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, &op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Rename an entry across directories (or in place).
    ///
    /// Same-parent paths dispatch to [`Self::rename_in_dir`] (a single
    /// in-place NAME tag). Different-parent paths apply a two-commit
    /// sequence: a `Create` (with the entry's existing struct body) in
    /// the destination parent, followed by a `Delete` (at the source
    /// id) in the source parent. The struct body is preserved exactly,
    /// so CTZ files keep their existing chain and directories keep
    /// their existing metadata pair; cross-directory rename does not
    /// move file content or rehash anything.
    ///
    /// # Cycle prevention
    ///
    /// Rejects with [`Error::InvalidPath`] if `old_path` is a strict
    /// ancestor of `new_path` (would move a directory inside itself).
    ///
    /// # Atomicity gap
    ///
    /// Without atomic-move-state recovery (Phase 3+), an interrupt
    /// between the two commits leaves the entry visible in *both*
    /// directories. On remount the entry resolves under either name;
    /// re-running `rename` converges to the correct state. The C
    /// reference recovers via a `MoveState` tag emitted alongside the
    /// destination Create; that recovery is on the v1.0 punch list.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if the source path does not exist.
    /// - [`Error::AlreadyExists`] if the destination already exists.
    /// - [`Error::InvalidPath`] for empty/oversize names, for the root
    ///   on either side, or for the ancestor-cycle case above.
    /// - [`Error::OutOfRange`] if the destination commit would push
    ///   the live entry count past [`crate::dir::MAX_LIVE_ENTRIES`].
    /// - [`Error::Corrupt`] if the source entry's struct body is
    ///   ill-formed (wrong length for its tag type).
    pub fn rename(
        &mut self,
        old_path: crate::path::Path<'_>,
        new_path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (old_parent, old_leaf) = self.resolve_parent(old_path, buf_a, buf_b)?;
        let (new_parent, new_leaf) = self.resolve_parent(new_path, buf_a, buf_b)?;

        if old_parent == new_parent {
            return self.rename_in_dir(old_path, new_path, buf_a, buf_b);
        }

        let old_leaf_bytes = old_leaf.as_bytes();
        let new_leaf_bytes = new_leaf.as_bytes();
        if new_leaf_bytes.is_empty() || new_leaf_bytes.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }
        if path_is_strict_ancestor(old_path, new_path) {
            return Err(Error::InvalidPath);
        }

        // Look up the source entry; copy its struct body to a stack
        // buffer so the destination commit can borrow it after we
        // release the source pair's parse.
        self.storage.read(old_parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(old_parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let mut src_body = [0u8; 1024];
        let src_id;
        let src_struct_type;
        let src_body_len;
        {
            let p = MetadataPair::parse(old_parent.a, &*buf_a, old_parent.b, &*buf_b)?;
            let r = crate::dir::lookup(&p, old_leaf_bytes).ok_or(Error::NotFound)?;
            let n = r.struct_body.len();
            if n > src_body.len() {
                return Err(Error::OutOfRange);
            }
            src_body[..n].copy_from_slice(r.struct_body);
            src_id = r.entry.id;
            src_struct_type = r.struct_type;
            src_body_len = n;
        }

        // Reject if the destination already exists; compute the new
        // entry id from the destination pair's live count.
        self.storage.read(new_parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(new_parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let new_id: u16;
        {
            let p = MetadataPair::parse(new_parent.a, &*buf_a, new_parent.b, &*buf_b)?;
            if crate::dir::lookup(&p, new_leaf_bytes).is_some() {
                return Err(Error::AlreadyExists);
            }
            let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
            let active_is_a = p.active_block == new_parent.a;
            let count = gather_live_slots(&p, active_is_a, buf_a, buf_b, &mut slots)?;
            let id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
            if id == crate::tag::ID_NONE {
                return Err(Error::OutOfRange);
            }
            new_id = id;
        }

        // Build the Create op for the destination, preserving the
        // source's struct shape.
        let create_op = match src_struct_type {
            crate::tag::TagType::InlineStruct => WriteOp::Create {
                id: new_id,
                name: new_leaf_bytes,
                content: &src_body[..src_body_len],
            },
            crate::tag::TagType::CtzStruct => {
                if src_body_len != 8 {
                    return Err(Error::Corrupt);
                }
                let head_block =
                    u32::from_le_bytes([src_body[0], src_body[1], src_body[2], src_body[3]]);
                let total_size =
                    u32::from_le_bytes([src_body[4], src_body[5], src_body[6], src_body[7]]);
                WriteOp::CreateCtz { id: new_id, name: new_leaf_bytes, head_block, total_size }
            }
            crate::tag::TagType::DirStruct => {
                if src_body_len != 8 {
                    return Err(Error::Corrupt);
                }
                let a = u32::from_le_bytes([src_body[0], src_body[1], src_body[2], src_body[3]]);
                let b = u32::from_le_bytes([src_body[4], src_body[5], src_body[6], src_body[7]]);
                WriteOp::CreateDir {
                    id: new_id,
                    name: new_leaf_bytes,
                    dir_pair: BlockPair::new(BlockAddress::new(a), BlockAddress::new(b)),
                }
            }
            _ => return Err(Error::Corrupt),
        };

        // Step 1: Create in destination. Step 2: Delete from source.
        // Doing Create first keeps the entry reachable through the
        // failure window (duplicate, not loss); doing Delete first
        // would risk a true vanish if Create then failed.
        self.apply_op_to_pair(new_parent, &create_op, buf_a, buf_b)?;
        self.apply_op_to_pair(old_parent, &WriteOp::Remove { id: src_id }, buf_a, buf_b)
    }

    /// Apply a `WriteOp` to a metadata pair through the standard
    /// append-or-compact dispatch (append if there's room in the
    /// active block, otherwise compact onto the alternate and bump
    /// the revision). Reads both blocks of the pair before parsing,
    /// gathers live slots in case a compaction is needed, and syncs
    /// before returning.
    fn apply_op_to_pair(
        &mut self,
        pair_addr: BlockPair,
        op: &WriteOp<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;

        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        {
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == pair_addr.a;
            committed_end = pair.reader.committed_end();
            next_ptag = pair.reader.next_ptag();
            old_revision = pair.reader.revision();
            count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let dsize = op_dsize_of(op);
        if committed_end + dsize <= S::BLOCK_SIZE {
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Remove an empty directory at `path`.
    ///
    /// Verifies the entry is a Directory and its metadata pair has no
    /// live entries before removing it. The directory's metadata pair
    /// becomes unreachable after the parent's entry is removed; the
    /// allocator reclaims its blocks on the next scan.
    pub fn rmdir(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;

        // Resolve the entry; validate it's a Directory; grab its pair.
        self.storage.read(parent.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(parent.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let dir_pair = {
            let p = MetadataPair::parse(parent.a, &*buf_a, parent.b, &*buf_b)?;
            let resolved = crate::dir::lookup(&p, leaf.as_bytes()).ok_or(Error::NotFound)?;
            if resolved.entry.kind != crate::dir::EntryKind::Directory {
                return Err(Error::AlreadyExists);
            }
            if resolved.struct_type != crate::tag::TagType::DirStruct
                || resolved.struct_body.len() != 8
            {
                return Err(Error::Corrupt);
            }
            let a = u32::from_le_bytes([
                resolved.struct_body[0],
                resolved.struct_body[1],
                resolved.struct_body[2],
                resolved.struct_body[3],
            ]);
            let b = u32::from_le_bytes([
                resolved.struct_body[4],
                resolved.struct_body[5],
                resolved.struct_body[6],
                resolved.struct_body[7],
            ]);
            BlockPair::new(BlockAddress::new(a), BlockAddress::new(b))
        };

        // Read the directory's pair and verify it's empty.
        self.storage.read(dir_pair.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(dir_pair.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        {
            let dp = MetadataPair::parse(dir_pair.a, &*buf_a, dir_pair.b, &*buf_b)?;
            let mut live_count = 0usize;
            crate::dir::live_entries(&dp, |_| {
                live_count += 1;
                Ok::<(), Error>(())
            })?;
            if live_count > 0 {
                return Err(Error::NotEmpty);
            }
        }

        self.remove_from_pair(parent, leaf.as_bytes(), buf_a, buf_b)
    }

    /// List the entries in the directory at `path`, calling `f` for
    /// each. Skips the superblock (root only). Applies splice
    /// renumbering. Does **not** chase HardTails (single-pair listing
    /// only — Phase 2g follow-up).
    pub fn list_dir<F>(
        &mut self,
        path: crate::path::Path<'_>,
        f: F,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<usize, Error>
    where
        F: FnMut(&crate::dir::DirEntry<'_>),
    {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        let target_pair = if path.is_root() {
            self.root
        } else {
            // Resolve to the directory entry, then decode its
            // DirStruct body to find its pair address.
            let resolved = self.resolve(path, buf_a, buf_b)?;
            if resolved.entry.kind != crate::dir::EntryKind::Directory {
                return Err(Error::NotFound);
            }
            if resolved.struct_type != crate::tag::TagType::DirStruct
                || resolved.struct_body.len() != 8
            {
                return Err(Error::Corrupt);
            }
            let a = u32::from_le_bytes([
                resolved.struct_body[0],
                resolved.struct_body[1],
                resolved.struct_body[2],
                resolved.struct_body[3],
            ]);
            let b = u32::from_le_bytes([
                resolved.struct_body[4],
                resolved.struct_body[5],
                resolved.struct_body[6],
                resolved.struct_body[7],
            ]);
            BlockPair::new(BlockAddress::new(a), BlockAddress::new(b))
        };

        self.list_pair_chain(target_pair, f, buf_a, buf_b)
    }

    /// Enumerate the directory pair chain starting at `start`, chasing
    /// HardTails through up to [`MAX_DIR_CHAIN`] pairs. Per-pair splice
    /// renumbering is applied; ids are pair-local and reset to 0 at
    /// each pair boundary.
    fn list_pair_chain<F>(
        &mut self,
        start: BlockPair,
        mut f: F,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<usize, Error>
    where
        F: FnMut(&crate::dir::DirEntry<'_>),
    {
        let mut current = start;
        let mut emitted = 0usize;
        for _ in 0..MAX_DIR_CHAIN {
            self.storage.read(current.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
            self.storage.read(current.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
            let next = {
                let pair = MetadataPair::parse(current.a, &*buf_a, current.b, &*buf_b)?;
                let next = if pair.reader.is_hard_tail() { pair.reader.tail() } else { None };
                crate::dir::live_entries(&pair, |e| {
                    f(&e);
                    emitted += 1;
                    Ok::<(), Error>(())
                })?;
                next
            };
            match next {
                Some(p) => current = p,
                None => return Ok(emitted),
            }
        }
        Err(Error::OutOfRange)
    }

    /// Check whether an entry exists at the given absolute path.
    ///
    /// Equivalent to `self.resolve(path, ...).is_ok()` but kinder to
    /// the caller: returns `Ok(true)` for present, `Ok(false)` for a
    /// clean "not found" (missing leaf, missing intermediate, or
    /// intermediate-is-file), and an `Err` for I/O or corruption.
    pub fn exists(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<bool, Error> {
        match self.resolve(path, buf_a, buf_b) {
            Ok(_) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(Error::InvalidPath) if path.is_root() => Ok(true), // root always exists
            Err(other) => Err(other),
        }
    }

    /// Remove an entry from the filesystem root by name.
    ///
    /// If the name resolves, appends a `Delete` tag at the entry's id
    /// (or skips that slot during compaction if the active block is
    /// full). Subsequent entries with higher ids are renumbered down,
    /// matching the splice semantics in [`crate::dir::live_entries`].
    ///
    /// Returns [`Error::NotFound`] if the name does not exist.
    ///
    /// **Scope.** Root-only. Removing non-empty directories is not
    /// validated yet (the API will happily Delete a directory entry
    /// without checking its contents — Phase 2f follow-up). Files are
    /// the safe case and the SMIL calculator's primary need.
    pub fn remove_from_root(
        &mut self,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.remove_from_pair(self.root, name, buf_a, buf_b)
    }

    /// Internal: remove an entry by name from the given metadata pair.
    fn remove_from_pair(
        &mut self,
        pair_addr: BlockPair,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if name.is_empty() {
            return Err(Error::InvalidPath);
        }

        self.storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;

        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        let target_id: u16;
        {
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == pair_addr.a;
            committed_end = pair.reader.committed_end();
            next_ptag = pair.reader.next_ptag();
            old_revision = pair.reader.revision();

            match crate::dir::lookup(&pair, name) {
                Some(r) => target_id = r.entry.id,
                None => return Err(Error::NotFound),
            }
            count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let op = WriteOp::Remove { id: target_id };
        let op_dsize = 4 + 8; // Delete tag (4) + CCRC (4 + 4)

        if committed_end + op_dsize <= S::BLOCK_SIZE {
            // ---- APPEND PATH ----
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, &op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            // ---- COMPACT PATH ----
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }

        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// List the regular file and subdirectory names at the filesystem
    /// root, calling `f` for each.
    ///
    /// Skips the superblock entry. Applies splice (Create/Delete)
    /// renumbering, so deleted entries do not leak through. The names
    /// passed to `f` are raw bytes (LittleFS does not enforce UTF-8);
    /// callers that need a `str` should validate.
    ///
    /// Returns the live entry count.
    pub fn list_root<F>(&mut self, f: F, buf_a: &mut [u8], buf_b: &mut [u8]) -> Result<usize, Error>
    where
        F: FnMut(&crate::dir::DirEntry<'_>),
    {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        self.list_pair_chain(self.root, f, buf_a, buf_b)
    }

    /// Write or update a small inline file at the filesystem root
    /// (upsert semantics).
    ///
    /// If no entry named `name` exists, appends a `Create` + `NAME` +
    /// `InlineStruct` triple at the next free id. If an entry with that
    /// name already exists, appends a single `InlineStruct` tag at the
    /// existing entry's id with the new content (later tags supersede
    /// earlier ones, so reads return the new content).
    ///
    /// If the active block has enough free bytes, the new commit is
    /// appended in place (fast path: one `program` call, no erase).
    /// If the active block is too full, the kernel transparently
    /// **compacts** the live state into a fresh commit on the alternate
    /// block (with revision bumped), applying the create or update in
    /// the same commit. The alternate becomes the new active via the
    /// standard revision-based pair selection on the next mount.
    ///
    /// **Remaining limits.** Inline-only (content must fit alongside
    /// the live state in one block); root-only. CTZ writes (Phase 2d)
    /// and nested paths (Phase 2b.2) lift those.
    pub fn write_inline_to_root(
        &mut self,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.write_inline_to_pair(self.root, name, content, buf_a, buf_b)
    }

    /// Internal: write/update an inline file in the given metadata pair.
    /// Used by both `write_inline_to_root` and `write_inline_to_path`.
    fn write_inline_to_pair(
        &mut self,
        pair_addr: BlockPair,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if name.is_empty() || name.len() > 0x3FF || content.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }

        // Read both blocks of the target pair.
        self.storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;

        // Gather the live state and decide which op (create vs update) to apply.
        let active_addr;
        let alternate_addr;
        let active_is_a;
        let committed_end;
        let next_ptag;
        let old_revision;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        let existing_id: Option<u16>;
        {
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == pair_addr.a;
            committed_end = pair.reader.committed_end();
            next_ptag = pair.reader.next_ptag();
            old_revision = pair.reader.revision();
            existing_id = crate::dir::lookup(&pair, name).map(|r| r.entry.id);
            count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        }

        let op = if let Some(id) = existing_id {
            WriteOp::Update { id, content }
        } else {
            let new_id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
            if new_id == crate::tag::ID_NONE {
                return Err(Error::OutOfRange);
            }
            WriteOp::Create { id: new_id, name, content }
        };

        // Commit size for each WriteOp shape, plus the trailing CCRC (8).
        let op_dsize = match op {
            WriteOp::Update { content, .. } => (4 + content.len()) + 8,
            WriteOp::UpdateCtz { .. } => (4 + 8) + 8,
            WriteOp::Create { name, content, .. } => 4 + (4 + name.len()) + (4 + content.len()) + 8,
            // CreateCtz and CreateDir both have an 8-byte struct body
            // (head_block + size for CTZ; pair_a + pair_b for Dir).
            WriteOp::CreateCtz { name, .. } | WriteOp::CreateDir { name, .. } => {
                4 + (4 + name.len()) + (4 + 8) + 8
            }
            WriteOp::Remove { .. } => 4 + 8,
            WriteOp::RenameInPlace { new_name, .. } => (4 + new_name.len()) + 8,
        };

        if committed_end + op_dsize <= S::BLOCK_SIZE {
            // ---- APPEND PATH ----
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, &op)?;
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            self.storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .map_err(|_| Error::Io)?;
        } else {
            // ---- COMPACT PATH ----
            let new_revision = old_revision.wrapping_add(1);
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    &slots,
                    count,
                    &op,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                )?
            };
            self.storage.erase(alternate_addr.as_u32()).map_err(|_| Error::Io)?;
            let alt_buf: &[u8] = if active_is_a { &*buf_b } else { &*buf_a };
            self.storage
                .program(alternate_addr.as_u32(), 0, &alt_buf[..new_end])
                .map_err(|_| Error::Io)?;
        }

        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Format the storage device with a fresh, empty LittleFS v2
    /// filesystem.
    ///
    /// Erases blocks `0` and `1` (the root metadata pair), then writes
    /// a single commit on block `0` containing:
    ///
    /// 1. Revision counter `1` (4 bytes LE) at offset `0`.
    /// 2. A `Superblock` NAME tag with body `b"littlefs"`.
    /// 3. An `InlineStruct` tag at id `0` whose 24 byte body encodes the
    ///    geometry: version = [`crate::DISK_VERSION`], block_size =
    ///    `S::BLOCK_SIZE`, block_count = `S::BLOCK_COUNT`, name_max /
    ///    file_max / attr_max all zero (defaults).
    /// 4. A CCRC tag with chunk `0`.
    ///
    /// `scratch` must be at least [`S::BLOCK_SIZE`](Storage::BLOCK_SIZE)
    /// bytes; the function pre-fills it with `0xFF` and writes the
    /// commit into it before programming.
    ///
    /// After this call returns, `storage` can be passed to [`Fs::mount`]
    /// to obtain a usable handle. Block `1` is left in pristine erased
    /// state to serve as the metadata pair's alternate.
    pub fn format(storage: &mut S, scratch: &mut [u8]) -> Result<(), Error> {
        if scratch.len() < S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        let scratch = &mut scratch[..S::BLOCK_SIZE];
        // Pre-fill with the erased state so any bytes past the CCRC
        // mirror what a freshly erased flash region would read as.
        for b in scratch.iter_mut() {
            *b = 0xFF;
        }

        let sb = Superblock {
            version: crate::DISK_VERSION,
            block_size: S::BLOCK_SIZE as u32,
            block_count: S::BLOCK_COUNT,
            name_max: 0,
            file_max: 0,
            attr_max: 0,
        };
        let sb_body = sb.to_bytes();

        let mut commit = crate::meta::Commit::new(scratch, 1)?;
        commit
            .tag(crate::tag::Tag::new(true, crate::tag::TagType::Superblock, 0, 8), crate::MAGIC)?;
        commit.tag(
            crate::tag::Tag::new(
                true,
                crate::tag::TagType::InlineStruct,
                0,
                Superblock::SIZE as u16,
            ),
            &sb_body,
        )?;
        commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;

        // Erase + program block 0 with the committed superblock pair.
        storage.erase(0).map_err(|_| Error::Io)?;
        storage.program(0, 0, scratch).map_err(|_| Error::Io)?;
        // Erase block 1 to leave it as a fresh alternate.
        storage.erase(1).map_err(|_| Error::Io)?;
        storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Mount a filesystem.
    ///
    /// Reads blocks `0` and `1` from `storage` into `block_a_buf` and
    /// `block_b_buf`, picks the active block of the resulting pair, and
    /// parses the superblock. Both buffers must be exactly
    /// [`S::BLOCK_SIZE`](Storage::BLOCK_SIZE) bytes; their previous
    /// contents are overwritten.
    ///
    /// # Error matrix
    ///
    /// Mount failures are split into distinct variants so a firmware
    /// boot path can branch by category. The mapping is intended to be
    /// strict enough that callers can match exhaustively without a
    /// catch-all (the [`Error`] type itself is `#[non_exhaustive]`,
    /// but the variants below are the complete mount-time set as of
    /// this release):
    ///
    /// | Variant | Meaning | Suggested action |
    /// |---|---|---|
    /// | [`Error::Io`] | The `storage.read` call failed. | Retry on a transient device fault, or escalate. |
    /// | [`Error::GeometryMismatch`] | Buffers are the wrong size, **or** the on-disk superblock's `block_size` / `block_count` differs from the [`Storage`] trait's advertised values. | Caller bug (wrong buffer length) or wrong-chip-for-image. Do not auto-format. |
    /// | [`Error::Unformatted`] | Both blocks of the root pair are pristine `0xFF`. The device has never been programmed (fresh chip, post-full-erase). | Call [`Fs::format`] and retry, *if* the caller owns the formatting decision. |
    /// | [`Error::Corrupt`] | At least one block has been programmed, but neither block has a successfully verified CCRC commit. Bit rot, torn erase, or third-party-tool damage. | Escalate to a recovery path; do not auto-format. |
    /// | [`Error::NotLittleFs`] | Both blocks parse cleanly (valid CCRC commits) but the [`crate::MAGIC`] NAME tag is absent. The blocks hold someone else's metadata format. | Escalate; this is the "wrong filesystem on this chip" case. |
    /// | [`Error::UnsupportedVersion`] | Magic + superblock present, but the version word is newer than this crate. The contained value is the encoded version. | Escalate; cannot read forward-version data safely. |
    ///
    /// `Error::Unformatted` versus `Error::Corrupt` is the key
    /// distinction for production boot logic: an `Unformatted` device
    /// is the expected first-boot state; a `Corrupt` device is a
    /// "page the on-call engineer" state. The implementation
    /// distinguishes them by checking the literal byte content of
    /// both blocks before parsing.
    pub fn mount(
        mut storage: S,
        block_a_buf: &mut [u8],
        block_b_buf: &mut [u8],
    ) -> Result<Self, Error> {
        if block_a_buf.len() != S::BLOCK_SIZE || block_b_buf.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        storage.read(0, 0, block_a_buf).map_err(|_| Error::Io)?;
        storage.read(1, 0, block_b_buf).map_err(|_| Error::Io)?;

        // Distinguish a fresh / wiped chip from corruption before we
        // even attempt to parse: both blocks of the root pair sit in
        // the erased state (every byte `0xFF`) iff nothing has ever
        // been programmed to them. A real corruption (bit rot, torn
        // erase, mis-formatted image) flips at least one bit somewhere.
        if is_all_erased(block_a_buf) && is_all_erased(block_b_buf) {
            return Err(Error::Unformatted);
        }

        let pair = MetadataPair::parse(
            BlockAddress::new(0),
            block_a_buf,
            BlockAddress::new(1),
            block_b_buf,
        )?;
        let sb = Superblock::from_pair(&pair)?;

        // The on disk superblock advertises the filesystem's geometry.
        // The storage trait advertises the device's geometry. These must
        // agree, otherwise reads beyond the device's actual blocks would
        // wrap or fault, and the kernel cannot safely operate.
        if (sb.block_size as usize) != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if sb.block_count != S::BLOCK_COUNT {
            return Err(Error::GeometryMismatch);
        }

        Ok(Self { storage, superblock: sb, root: ROOT_BLOCK_PAIR })
    }

    /// Decoded superblock for this filesystem.
    #[inline]
    #[must_use]
    pub fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// Address of the root metadata pair. Always
    /// [`crate::ROOT_BLOCK_PAIR`] for LittleFS v2.
    #[inline]
    #[must_use]
    pub fn root(&self) -> BlockPair {
        self.root
    }

    /// Borrow the underlying storage device.
    #[inline]
    #[must_use]
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Borrow the underlying storage device mutably.
    #[inline]
    #[must_use]
    pub fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    /// Consume the `Fs` and return the underlying storage device.
    #[inline]
    #[must_use]
    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Read a CTZ backed file's content via [`ctz::read_ctz`].
    ///
    /// Convenience wrapper that forwards the storage handle.
    pub fn read_ctz(
        &mut self,
        ctz: &crate::ctz::CtzStruct,
        out: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<usize, Error> {
        crate::ctz::read_ctz(&mut self.storage, ctz, out, scratch)
    }

    /// Resolve an absolute path to its directory entry.
    ///
    /// Walks from the root metadata pair through every intermediate
    /// directory by name, ending at the named entry. After this call
    /// returns, `buf_a` and `buf_b` contain the bytes of the metadata
    /// pair holding the final entry, and the returned [`ResolvedPath`]
    /// borrows slices from them.
    ///
    /// # Errors
    ///
    /// - [`Error::GeometryMismatch`] if a buffer is the wrong size.
    /// - [`Error::Io`] for any storage read failure.
    /// - [`Error::NotFound`] if any path component does not exist or if
    ///   a non final component resolves to a regular file.
    /// - [`Error::Corrupt`] if an intermediate directory's `DirStruct`
    ///   body is malformed (wrong length, etc.).
    /// - [`Error::InvalidPath`] if `path` is the root `/` (no entry to
    ///   resolve to).
    pub fn resolve<'b>(
        &mut self,
        path: crate::path::Path<'_>,
        buf_a: &'b mut [u8],
        buf_b: &'b mut [u8],
    ) -> Result<ResolvedPath<'b>, Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if path.is_root() {
            return Err(Error::InvalidPath);
        }

        // Walk intermediate path components, descending through any
        // matched directories and chasing HardTail-threaded
        // continuations as needed. Each intermediate step returns a
        // `BlockPair` (Copy) so the buffer borrows don't escape.
        let mut current = self.root;
        let mut components = path.components().peekable();
        let leaf_name = loop {
            let name = components.next().ok_or(Error::InvalidPath)?;
            if components.peek().is_none() {
                break name;
            }
            current = self.find_dir_pair(current, name.as_bytes(), buf_a, buf_b)?;
        };

        // Final component: look up in the current pair, chasing HardTails.
        // The matching read leaves buf_a/buf_b populated with that pair's
        // bytes; we return a `ResolvedPath<'b>` borrowing from them.
        loop {
            self.storage.read(current.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
            self.storage.read(current.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
            // We need to drop the pair borrow before potentially looping
            // back to re-read. Scope it tightly.
            let tail_to_follow = {
                let pair = MetadataPair::parse(current.a, &*buf_a, current.b, &*buf_b)?;
                if crate::dir::lookup(&pair, leaf_name.as_bytes()).is_some() {
                    None
                } else if pair.reader.is_hard_tail() {
                    pair.reader.tail()
                } else {
                    return Err(Error::NotFound);
                }
            };
            if let Some(next) = tail_to_follow {
                current = next;
                continue;
            }
            // Found here; re-parse to produce the returned 'b-lifetime view.
            let pair = MetadataPair::parse(current.a, buf_a, current.b, buf_b)?;
            let resolved = crate::dir::lookup(&pair, leaf_name.as_bytes()).expect(
                "lookup succeeded once already in this iteration; the data has not changed",
            );
            return Ok(ResolvedPath {
                pair: BlockPair::new(pair.active_block, pair.alternate_block),
                entry: resolved.entry,
                struct_type: resolved.struct_type,
                struct_body: resolved.struct_body,
            });
        }
    }

    /// Locate a Directory entry by name within `dir_pair` (chasing
    /// HardTail-threaded continuations), and return the address of its
    /// metadata pair. Returns [`Error::NotFound`] if the name does not
    /// resolve to a Directory in the chain.
    ///
    /// Used internally by [`Fs::resolve`] for intermediate path
    /// components. Exposed-via-method form because the helper does not
    /// retain a borrow of `buf_a` / `buf_b` past return, decoupling
    /// the caller's lifetime requirements.
    fn find_dir_pair(
        &mut self,
        dir_pair: BlockPair,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<BlockPair, Error> {
        let mut current = dir_pair;
        loop {
            self.storage.read(current.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
            self.storage.read(current.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
            let pair = MetadataPair::parse(current.a, &*buf_a, current.b, &*buf_b)?;
            if let Some(resolved) = crate::dir::lookup(&pair, name) {
                if resolved.entry.kind != crate::dir::EntryKind::Directory {
                    return Err(Error::NotFound);
                }
                if resolved.struct_type != crate::tag::TagType::DirStruct
                    || resolved.struct_body.len() != 8
                {
                    return Err(Error::Corrupt);
                }
                let a = u32::from_le_bytes([
                    resolved.struct_body[0],
                    resolved.struct_body[1],
                    resolved.struct_body[2],
                    resolved.struct_body[3],
                ]);
                let b = u32::from_le_bytes([
                    resolved.struct_body[4],
                    resolved.struct_body[5],
                    resolved.struct_body[6],
                    resolved.struct_body[7],
                ]);
                return Ok(BlockPair::new(BlockAddress::new(a), BlockAddress::new(b)));
            }
            if pair.reader.is_hard_tail() {
                if let Some(tail) = pair.reader.tail() {
                    current = tail;
                    continue;
                }
            }
            return Err(Error::NotFound);
        }
    }

    /// Read a metadata pair from the storage device into the provided
    /// buffers and parse it.
    ///
    /// `addr` is the pair's two block addresses. `buf_a` receives the
    /// bytes of `addr.a`; `buf_b` receives the bytes of `addr.b`. Both
    /// buffers must be exactly [`S::BLOCK_SIZE`](Storage::BLOCK_SIZE)
    /// bytes; their previous contents are overwritten.
    ///
    /// The returned [`MetadataPair`] borrows from `buf_a` and `buf_b`, so
    /// the buffers must outlive the borrow.
    pub fn read_pair<'b>(
        &mut self,
        addr: BlockPair,
        buf_a: &'b mut [u8],
        buf_b: &'b mut [u8],
    ) -> Result<MetadataPair<'b>, Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        self.storage.read(addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        MetadataPair::parse(addr.a, buf_a, addr.b, buf_b)
    }
}
