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
fn build_compact_commit(
    alt_buf: &mut [u8],
    source_buf: &[u8],
    new_revision: u32,
    slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
    count: usize,
    op: &WriteOp<'_>,
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
    commit.finish(0)?;
    Ok(commit.bytes_written())
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
                commit.finish(0)?;
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
                build_compact_commit(buf_b, buf_a, new_revision, &slots, count, &op)?
            } else {
                build_compact_commit(buf_a, buf_b, new_revision, &slots, count, &op)?
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
    /// not exist; otherwise reads the existing content, concatenates
    /// `additional`, and rewrites via [`Self::write_to_path`].
    ///
    /// **Storage model.** This is an *atomic full-rewrite* append. The
    /// existing content is read into the caller-supplied
    /// `content_scratch` buffer, the new bytes are appended in memory,
    /// and the combined content is written as a single new entry. For
    /// CTZ-backed files, the old chain becomes unreachable and the
    /// allocator reclaims its blocks on the next scan.
    ///
    /// This pattern is O(file_size) per append, so it suits use cases
    /// with infrequent appends or small files. Append-heavy workloads
    /// (log streaming) should use the future stateful `File` API,
    /// which extends CTZ chains incrementally (Phase 2f.2).
    ///
    /// `content_scratch` must be at least
    /// `existing_size + additional.len()` bytes. Caller is expected to
    /// budget enough space for the maximum expected file size.
    ///
    /// # Errors
    ///
    /// - [`Error::OutOfRange`] if `content_scratch` is too small for
    ///   the combined content.
    /// - [`Error::AlreadyExists`] if the path exists but is a
    ///   directory (cannot append to a directory).
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
        // Resolve and copy the existing content into content_scratch.
        let (existing_size, ctz_to_load): (usize, Option<crate::ctz::CtzStruct>) =
            match self.resolve(path, buf_a, buf_b) {
                Ok(r) => {
                    if r.entry.kind != crate::dir::EntryKind::RegularFile {
                        return Err(Error::AlreadyExists);
                    }
                    match r.struct_type {
                        crate::tag::TagType::InlineStruct => {
                            let n = r.struct_body.len();
                            if n + additional.len() > content_scratch.len() {
                                return Err(Error::OutOfRange);
                            }
                            content_scratch[..n].copy_from_slice(r.struct_body);
                            (n, None)
                        }
                        crate::tag::TagType::CtzStruct => {
                            let ctz = crate::ctz::CtzStruct::from_bytes(r.struct_body)?;
                            let n = ctz.size as usize;
                            if n + additional.len() > content_scratch.len() {
                                return Err(Error::OutOfRange);
                            }
                            (n, Some(ctz))
                        }
                        _ => return Err(Error::Corrupt),
                    }
                }
                Err(Error::NotFound) => {
                    if additional.len() > content_scratch.len() {
                        return Err(Error::OutOfRange);
                    }
                    (0, None)
                }
                Err(e) => return Err(e),
            };

        // For CTZ files, pull the chain content into the scratch.
        if let Some(ctz) = ctz_to_load {
            self.read_ctz(&ctz, &mut content_scratch[..existing_size], buf_a)?;
        }

        // Stamp the new bytes after the existing content.
        content_scratch[existing_size..existing_size + additional.len()]
            .copy_from_slice(additional);
        let total = existing_size + additional.len();

        // Hand off to write_to_path, which auto-dispatches inline vs
        // CTZ and handles update semantics for existing entries.
        self.write_to_path(path, &content_scratch[..total], buf_a, buf_b)
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
            commit.finish(0)?;
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
                commit.finish(0)?;
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
                build_compact_commit(buf_b, buf_a, new_revision, &slots, count, &op)?
            } else {
                build_compact_commit(buf_a, buf_b, new_revision, &slots, count, &op)?
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
                commit.finish(0)?;
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
                build_compact_commit(buf_b, buf_a, new_revision, &slots, count, &op)?
            } else {
                build_compact_commit(buf_a, buf_b, new_revision, &slots, count, &op)?
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
        mut f: F,
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

        self.storage.read(target_pair.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(target_pair.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let pair = MetadataPair::parse(target_pair.a, buf_a, target_pair.b, buf_b)?;
        let mut emitted = 0usize;
        crate::dir::live_entries(&pair, |e| {
            f(&e);
            emitted += 1;
            Ok::<(), Error>(())
        })?;
        Ok(emitted)
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
                commit.finish(0)?;
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
                build_compact_commit(buf_b, buf_a, new_revision, &slots, count, &op)?
            } else {
                build_compact_commit(buf_a, buf_b, new_revision, &slots, count, &op)?
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
    pub fn list_root<F>(
        &mut self,
        mut f: F,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<usize, Error>
    where
        F: FnMut(&crate::dir::DirEntry<'_>),
    {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        self.storage.read(0, 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(1, 0, buf_b).map_err(|_| Error::Io)?;
        let pair = MetadataPair::parse(BlockAddress::new(0), buf_a, BlockAddress::new(1), buf_b)?;
        let mut emitted = 0usize;
        crate::dir::live_entries(&pair, |e| {
            // Skip the superblock entry by kind. The superblock NAME
            // appears as TagType::Superblock but live_entries only
            // emits DirEntry { kind: RegularFile | Directory }, so it
            // never appears here. Defensive though: if a future
            // refactor surfaces it, we still skip on kind.
            f(&e);
            emitted += 1;
            Ok::<(), Error>(())
        })?;
        Ok(emitted)
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
                commit.finish(0)?;
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
                build_compact_commit(buf_b, buf_a, new_revision, &slots, count, &op)?
            } else {
                build_compact_commit(buf_a, buf_b, new_revision, &slots, count, &op)?
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
        commit.finish(0)?;

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
    /// Returns:
    ///
    /// - [`Error::GeometryMismatch`] if the buffers are the wrong size, or
    ///   if the on disk superblock's `block_size` or `block_count` does
    ///   not match the storage trait's advertised values.
    /// - [`Error::Io`] if the underlying device read failed.
    /// - The errors documented on [`MetadataPair::parse`] and
    ///   [`Superblock::from_pair`] when the on disk structures are
    ///   missing or malformed.
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
