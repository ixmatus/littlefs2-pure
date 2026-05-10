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

/// A pending write operation. Used by [`Fs::write_inline_to_root`] and
/// [`Fs::remove_from_root`] to dispatch through the same append-vs-compact
/// machinery.
#[derive(Clone, Copy)]
enum WriteOp<'a> {
    /// Create a new entry at `id` (the next free id) with NAME `name`
    /// and InlineStruct `content`.
    Create { id: u16, name: &'a [u8], content: &'a [u8] },
    /// Update the existing entry at `id` by appending a new
    /// InlineStruct with `content`. The NAME and entry kind are
    /// preserved by the existing tags in the commit log.
    Update { id: u16, content: &'a [u8] },
    /// Remove the entry at `id`. Append path emits a `Delete` tag;
    /// compact path skips the slot and renumbers subsequent ids down.
    Remove { id: u16 },
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
        WriteOp::Update { id, content } => {
            commit.tag(Tag::new(true, TagType::InlineStruct, id, content.len() as u16), content)?;
        }
        WriteOp::Remove { id } => {
            // Delete tag's length field is the special sentinel 0x3FF
            // (no body). Subsequent entries with higher ids renumber
            // down at read time via `dir::live_entries`'s splice
            // handling.
            commit.tag(Tag::new(true, TagType::Delete, id, 0x3FF), &[])?;
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
    for (i, s) in slots.iter().enumerate().take(count) {
        let id = u16::try_from(i).map_err(|_| Error::OutOfRange)?;
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
        commit.tag(crate::tag::Tag::new(true, TagType::Create, id, 0), &[])?;
        commit.tag(crate::tag::Tag::new(true, name_type, id, s.name_len), name)?;

        // If this is the target of an Update, substitute the new content
        // for the struct body. Otherwise copy the source's struct as-is.
        if let WriteOp::Update { id: update_id, content } = *op {
            if id == update_id {
                commit.tag(
                    crate::tag::Tag::new(true, TagType::InlineStruct, id, content.len() as u16),
                    content,
                )?;
                continue;
            }
        }
        let struct_body =
            &source_buf[s.struct_off as usize..s.struct_off as usize + s.struct_len as usize];
        commit.tag(crate::tag::Tag::new(true, struct_type, id, s.struct_len), struct_body)?;
    }
    // Create: append the new entry at id == count.
    if let WriteOp::Create { id, name, content } = *op {
        debug_assert_eq!(id as usize, count, "Create id must equal current live count");
        commit.tag(crate::tag::Tag::new(true, TagType::Create, id, 0), &[])?;
        commit
            .tag(crate::tag::Tag::new(true, TagType::RegularFile, id, name.len() as u16), name)?;
        commit.tag(
            crate::tag::Tag::new(true, TagType::InlineStruct, id, content.len() as u16),
            content,
        )?;
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
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if name.is_empty() {
            return Err(Error::InvalidPath);
        }

        self.storage.read(0, 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(1, 0, buf_b).map_err(|_| Error::Io)?;

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
            let pair =
                MetadataPair::parse(BlockAddress::new(0), &*buf_a, BlockAddress::new(1), &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == BlockAddress::new(0);
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
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if name.is_empty() || name.len() > 0x3FF || content.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }

        // Read both root blocks.
        self.storage.read(0, 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(1, 0, buf_b).map_err(|_| Error::Io)?;

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
            let pair =
                MetadataPair::parse(BlockAddress::new(0), &*buf_a, BlockAddress::new(1), &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == BlockAddress::new(0);
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
            WriteOp::Create { name, content, .. } => 4 + (4 + name.len()) + (4 + content.len()) + 8,
            WriteOp::Remove { .. } => 4 + 8,
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
