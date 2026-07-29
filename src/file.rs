//! Stateful file handle.
//!
//! [`File`] is the batched-write counterpart to the stateless
//! [`crate::Fs::write_to_path`] / [`crate::Fs::append_to_path`] /
//! [`crate::Fs::read_at_path`] surface. The path-based methods commit
//! one metadata-pair tag per call; the [`File`] handle defers the
//! metadata commit until [`File::sync`] (or [`File::close`]) so a
//! session of many small writes lands as a single metadata-pair
//! commit. The CTZ chain itself is written incrementally as each
//! [`File::write`] call runs: bytes that fit in the existing tail
//! block go through NOR sub-window programs; overflow blocks come
//! from fresh allocations and are programmed in order. Only the
//! parent-directory entry's `CtzStruct` body is held back until sync.
//!
//! # Atomicity
//!
//! The metadata commit is the visibility boundary. Until [`File::sync`]
//! lands, the file's parent entry still points at the pre-open
//! `(head_block, size)`; a remount sees the file unchanged regardless
//! of how many [`File::write`] calls preceded the crash. The new
//! chain blocks programmed during the session are unreachable from
//! the metadata, so the next [`crate::alloc::scan_used_blocks`] sweep
//! reclaims them.
//!
//! Once [`File::sync`] commits, the new chain is visible and the old
//! chain (if any) becomes orphan, reclaimed by the next allocator
//! scan.
//!
//! # Scope
//!
//! The handle operates on CTZ-backed regular files (and on missing or
//! truncated-to-zero entries, which become CTZ on the first write).
//! Inline-style upserts of small configuration data are still served
//! by the path-based API ([`crate::Fs::write_inline_to_root`],
//! [`crate::Fs::write_to_path`] for content at or below
//! [`crate::Fs::INLINE_MAX`]). Opening an existing inline file for
//! buffered access is not supported by this handle; use
//! [`crate::Fs::read_at_path`] / [`crate::Fs::write_to_path`] for
//! that case. The error path is [`Error::OutOfRange`] with a clear
//! intent rather than silent promotion.
//!
//! # Buffer convention
//!
//! [`File`] borrows the underlying [`crate::Fs`] mutably for its
//! lifetime, so while a handle is open no other [`crate::Fs`]
//! operation can run on the same handle. Each `File` method takes
//! two `&mut [u8]` scratch buffers (each
//! [`crate::Storage::BLOCK_SIZE`] bytes), matching the rest of the
//! crate's no-`alloc` convention.
//!
//! # Drop
//!
//! Drop does **not** sync. Uncommitted writes are silently dropped
//! on flash: the new chain blocks remain reclaimable by the next
//! allocator scan, so no corruption occurs, but the file remains at
//! its pre-open state. Always call [`File::sync`] or
//! [`File::close`] explicitly to commit. The `#[must_use]` attribute
//! on [`Fs::open`](crate::Fs::open) is the only compile-time nudge
//! toward an explicit sync; the Drop impl deliberately stays silent
//! (it has no way to surface a diagnostic and the dropped blocks are
//! reclaimable, so there is nothing safe or useful for it to do).

use crate::block::BlockPair;
use crate::ctz::CtzStruct;
use crate::dir;
use crate::error::Error;
use crate::meta::MetadataPair;
use crate::path::Path;
use crate::storage::Storage;
use crate::tag::TagType;
use crate::{BlockAddress, Fs};

/// Open-mode flags for [`Fs::open`](crate::Fs::open).
///
/// Mirrors `std::fs::OpenOptions`'s shape:
///
/// - [`OpenOptions::read`] — open for reading. Required to call
///   [`File::read`].
/// - [`OpenOptions::write`] — open for writing. Required to call
///   [`File::write`] or [`File::set_len`].
/// - [`OpenOptions::append`] — implies `write`; positions the cursor
///   at end of file at open time and forces every [`File::write`] to
///   land at end of file regardless of subsequent [`File::seek`]
///   calls.
/// - [`OpenOptions::truncate`] — drop existing content on open.
///   Requires `write`. Has no effect on missing files.
/// - [`OpenOptions::create`] — create the file if missing. The parent
///   directory must already exist. Has no effect when the file is
///   already present.
// Five bool fields mirror `std::fs::OpenOptions` semantics one-for-one
// (read / write / append / truncate / create). The shape is the
// standard library's, so the same lint allowance applies here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
}

impl OpenOptions {
    /// Default options: read-only, no creation.
    #[must_use]
    pub const fn new() -> Self {
        Self { read: false, write: false, append: false, truncate: false, create: false }
    }

    /// Set the read flag.
    #[must_use]
    pub const fn read(mut self, on: bool) -> Self {
        self.read = on;
        self
    }

    /// Set the write flag. Required for [`File::write`].
    #[must_use]
    pub const fn write(mut self, on: bool) -> Self {
        self.write = on;
        self
    }

    /// Set the append flag. Implies [`Self::write`]; writes are forced
    /// to end of file regardless of cursor position.
    #[must_use]
    pub const fn append(mut self, on: bool) -> Self {
        self.append = on;
        if on {
            self.write = true;
        }
        self
    }

    /// Set the truncate flag. Drops existing content to zero bytes on
    /// open. Requires [`Self::write`].
    #[must_use]
    pub const fn truncate(mut self, on: bool) -> Self {
        self.truncate = on;
        self
    }

    /// Set the create flag. Creates the file if it does not exist.
    #[must_use]
    pub const fn create(mut self, on: bool) -> Self {
        self.create = on;
        self
    }
}

/// Seek anchor for [`File::seek`].
///
/// Mirrors `std::io::SeekFrom` over `u32` offsets.
#[derive(Clone, Copy, Debug)]
pub enum SeekFrom {
    /// Offset from the start of the file.
    Start(u32),
    /// Offset from the current cursor position. Negative values move
    /// backward; the resulting position is clamped at 0.
    Current(i64),
    /// Offset from the end of the file. Negative values move backward;
    /// the resulting position is clamped at 0.
    End(i64),
}

/// Stateful file handle.
///
/// Constructed by [`Fs::open`](crate::Fs::open). Holds the parent
/// metadata-pair address and entry id, the cursor position, and the
/// current view of `(head_block, size)` for the CTZ chain being
/// extended. [`File::sync`] commits the staged metadata.
///
/// The handle borrows the [`crate::Fs`] mutably for its lifetime, so
/// concurrent operations on the same [`crate::Fs`] are not possible
/// while a [`File`] is open.
#[must_use = "files do not auto-sync on drop; call `sync()` or `close()` to commit pending writes"]
pub struct File<'fs, S: Storage> {
    fs: &'fs mut Fs<S>,
    /// The directory metadata pair holding the entry.
    parent: BlockPair,
    /// The entry's id within `parent`.
    id: u16,
    /// Current logical cursor position.
    pos: u32,
    /// Current CTZ head block. `NONE` for empty / not-yet-allocated
    /// chains. Tracks the head as new chain blocks are appended
    /// during this session; only committed at [`Self::sync`].
    head_block: BlockAddress,
    /// Current file size in bytes (in-memory view, possibly past the
    /// committed size).
    size: u32,
    /// `true` if [`Self::sync`] needs to emit a commit.
    dirty: bool,
    /// The open mode flags.
    options: OpenOptions,
}

impl<S: Storage> Fs<S> {
    /// Open a regular file at `path` for buffered read/write through a
    /// stateful [`File`] handle. The handle batches writes so a
    /// session of many [`File::write`] calls touches the metadata
    /// pair exactly once at [`File::sync`] time.
    ///
    /// # Modes
    ///
    /// - **Read-only** (`OpenOptions::new().read(true)`): the file
    ///   must exist; [`File::read`] returns committed bytes; writes
    ///   are rejected.
    /// - **Write**: the file must exist unless `create` is also set;
    ///   [`File::write`] streams bytes into the chain and updates the
    ///   in-memory size/head without committing the metadata-pair
    ///   entry until [`File::sync`].
    /// - **Append**: write at end of file. Combine with `truncate`
    ///   to start from empty.
    /// - **Create**: allocate a fresh empty file when missing.
    ///
    /// Opening an existing **inline** file (one whose content lives
    /// directly in an `InlineStruct` body inside the metadata pair)
    /// is supported only when `truncate(true)` is also set (which
    /// discards the inline body and starts a fresh CTZ chain).
    /// Non-truncating opens of inline files return
    /// [`Error::OutOfRange`]; route those through
    /// [`Self::read_at_path`] / [`Self::write_to_path`] instead.
    ///
    /// # Errors
    ///
    /// - [`Error::GeometryMismatch`] if either buffer is the wrong size.
    /// - [`Error::InvalidPath`] for the root path, empty names, or
    ///   conflicting mode flags (e.g. `truncate` without `write`,
    ///   neither `read` nor `write`).
    /// - [`Error::NotFound`] if the parent directory or, for
    ///   non-creating modes, the file itself does not exist.
    /// - [`Error::AlreadyExists`] if `path` resolves to a directory.
    /// - [`Error::OutOfRange`] when an existing inline file is opened
    ///   without `truncate`.
    pub fn open<'fs>(
        &'fs mut self,
        path: Path<'_>,
        options: OpenOptions,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<File<'fs, S>, Error> {
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if !options.read && !options.write {
            return Err(Error::InvalidPath);
        }
        if options.truncate && !options.write {
            return Err(Error::InvalidPath);
        }

        let (parent, leaf) = self.resolve_parent_for_file(path, buf_a, buf_b)?;
        let name = leaf.as_bytes();
        if name.is_empty() || name.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }

        // Classify the existing entry. The file's entry may live in any
        // pair of the parent's HardTail chain; `owner` is that pair (the
        // chain's last pair when the entry is absent, where a create
        // lands). The file handle caches `owner` so write-backs target
        // the entry's actual pair, not the chain's first pair.
        let owner = match self.seek_entry_in_chain(parent, name, buf_a, buf_b)? {
            crate::fs::ChainSeek::Found { pair, .. } => pair,
            crate::fs::ChainSeek::Absent { last_pair, .. } => last_pair,
        };
        #[derive(Clone, Copy)]
        enum Existing {
            Missing,
            Inline { id: u16, size: usize },
            Ctz { id: u16, ctz: CtzStruct },
        }
        let existing: Existing = {
            let p = MetadataPair::parse(owner.a, &*buf_a, owner.b, &*buf_b)?;
            match dir::lookup_checked(&p, name)? {
                None => Existing::Missing,
                Some(r) => {
                    if r.entry.kind != dir::EntryKind::RegularFile {
                        return Err(Error::AlreadyExists);
                    }
                    match r.struct_type {
                        TagType::InlineStruct => {
                            Existing::Inline { id: r.entry.id, size: r.struct_body.len() }
                        }
                        TagType::CtzStruct => Existing::Ctz {
                            id: r.entry.id,
                            ctz: CtzStruct::from_bytes(r.struct_body)?,
                        },
                        _ => return Err(Error::Corrupt),
                    }
                }
            }
        };

        let (file_parent, id, mut head_block, mut size) = match existing {
            Existing::Missing => {
                if !options.create {
                    return Err(Error::NotFound);
                }
                // Materialize an empty inline entry; the first write
                // promotes it to CTZ via sync. The create resolves which
                // pair of the chain the entry landed in.
                let (entry_pair, new_id) =
                    self.create_empty_entry_for_file(parent, name, buf_a, buf_b)?;
                (entry_pair, new_id, BlockAddress::NONE, 0u32)
            }
            Existing::Inline { id, size } => {
                // Inline files only work through File when truncating
                // away the inline body.
                if !options.truncate {
                    return Err(Error::OutOfRange);
                }
                (owner, id, BlockAddress::NONE, size as u32)
            }
            Existing::Ctz { id, ctz } => (owner, id, ctz.head_block, ctz.size),
        };

        // Apply truncate. The orphaned blocks (if any) are reclaimed
        // by the next allocator scan once sync lands.
        let mut dirty = false;
        if options.truncate {
            head_block = BlockAddress::NONE;
            size = 0;
            dirty = true;
        }

        // Initial cursor: end-of-file for append; otherwise start.
        let pos = if options.append { size } else { 0 };

        Ok(File { fs: self, parent: file_parent, id, pos, head_block, size, dirty, options })
    }
}

impl<S: Storage> File<'_, S> {
    /// Current cursor position.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.pos
    }

    /// Current in-memory file size. May exceed the committed size
    /// until [`Self::sync`] lands.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// `true` if pending writes are waiting on [`Self::sync`].
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Move the cursor. Returns the new absolute position.
    ///
    /// Seeking past the end of file is permitted; the file size is
    /// not extended until a write actually happens.
    pub fn seek(&mut self, from: SeekFrom) -> Result<u32, Error> {
        let new_pos: i64 = match from {
            SeekFrom::Start(p) => i64::from(p),
            SeekFrom::Current(d) => i64::from(self.pos) + d,
            SeekFrom::End(d) => i64::from(self.size) + d,
        };
        if !(0..=i64::from(u32::MAX)).contains(&new_pos) {
            return Err(Error::OutOfRange);
        }
        self.pos = new_pos as u32;
        Ok(self.pos)
    }

    /// Read up to `out.len()` bytes from the file starting at the
    /// current cursor, advancing the cursor. Returns the number of
    /// bytes copied (may be less than `out.len()` if the cursor was
    /// already at or past EOF).
    ///
    /// Requires the file to have been opened with [`OpenOptions::read`].
    pub fn read(
        &mut self,
        out: &mut [u8],
        buf_a: &mut [u8],
        _buf_b: &mut [u8],
    ) -> Result<usize, Error> {
        if !self.options.read {
            return Err(Error::InvalidPath);
        }
        if buf_a.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if out.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        if self.head_block.is_none() {
            // The only way to have size > 0 with head_block == NONE
            // here is the "inline opened with truncate" path, which
            // sets size = 0. The `pos >= size` guard above covered
            // that. Anything else is a logic bug.
            return Ok(0);
        }
        let avail = self.size - self.pos;
        let take = (out.len() as u64).min(u64::from(avail)) as usize;
        let ctz = CtzStruct { head_block: self.head_block, size: self.size };
        let n = crate::ctz::read_ctz_at(
            self.fs.storage_mut(),
            &ctz,
            self.pos,
            &mut out[..take],
            buf_a,
        )?;
        debug_assert_eq!(n, take);
        self.pos += take as u32;
        Ok(take)
    }

    /// Write `data` to the file at the current cursor position.
    ///
    /// **Append mode.** When the file was opened with
    /// [`OpenOptions::append`], the cursor is forced to end-of-file
    /// before every write — pure-append semantics, matching
    /// `std::fs::OpenOptions::append`.
    ///
    /// **Position constraint.** Writes that do not extend the file
    /// (random in-place rewrites at an arbitrary offset) are not
    /// supported: this method requires `self.position() == self.size()`
    /// at write time. To rewrite content in the middle of a file,
    /// use [`crate::Fs::truncate_path`] + [`Self::write`] or rewrite
    /// the whole file via [`crate::Fs::write_to_path`].
    ///
    /// Returns the number of bytes accepted (always `data.len()`
    /// unless the file would exceed the maximum CTZ chain length the
    /// kernel can describe).
    pub fn write(
        &mut self,
        data: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<usize, Error> {
        if !self.options.write {
            return Err(Error::InvalidPath);
        }
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if data.is_empty() {
            return Ok(0);
        }
        if self.options.append {
            self.pos = self.size;
        }
        if self.pos != self.size {
            return Err(Error::OutOfRange);
        }
        let ctz = CtzStruct { head_block: self.head_block, size: self.size };
        let (new_head, new_size) = self.fs.stream_ctz_extend(ctz, data, buf_a, buf_b)?;
        self.head_block = BlockAddress::new(new_head);
        self.size = new_size;
        self.pos = new_size;
        self.dirty = true;
        Ok(data.len())
    }

    /// Truncate the file to `new_size` bytes.
    ///
    /// Shrinking discards trailing bytes (orphaned blocks reclaimed
    /// by the next allocator scan after sync). Extending past the
    /// current size zero-fills the new bytes.
    ///
    /// Requires [`OpenOptions::write`]. The change is staged in
    /// memory until [`Self::sync`].
    pub fn set_len(
        &mut self,
        new_size: u32,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if !self.options.write {
            return Err(Error::InvalidPath);
        }
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        match new_size.cmp(&self.size) {
            core::cmp::Ordering::Equal => Ok(()),
            core::cmp::Ordering::Less => {
                if new_size == 0 {
                    self.head_block = BlockAddress::NONE;
                    self.size = 0;
                    self.pos = self.pos.min(self.size);
                    self.dirty = true;
                    return Ok(());
                }
                let new_head =
                    self.fs.shrink_ctz_head(self.head_block, self.size, new_size, buf_a, buf_b)?;
                self.head_block = new_head;
                self.size = new_size;
                self.pos = self.pos.min(self.size);
                self.dirty = true;
                Ok(())
            }
            core::cmp::Ordering::Greater => {
                let saved_pos = self.pos;
                self.pos = self.size;
                let mut remaining = new_size - self.size;
                // Zero-fill is a sequence of streaming writes from a
                // shared read-only zero buffer (`static`, so it costs no
                // stack). The loop iteration boundary closes the buf_a /
                // buf_b borrows between calls so we can hand them back to
                // stream_ctz_extend each time. The buffer is sized well
                // past a typical block so each `stream_ctz_extend` call
                // advances multiple blocks per chain re-walk rather than
                // one ~64-byte slice, cutting the call count (and the
                // per-call chain walks) on large zero-extends by an order
                // of magnitude; see `tests/bench_perf_backlog.rs` Bench C.
                static ZERO_CHUNK: [u8; 1024] = [0u8; 1024];
                while remaining > 0 {
                    let chunk = (remaining as usize).min(ZERO_CHUNK.len());
                    let ctz = CtzStruct { head_block: self.head_block, size: self.size };
                    let (new_head, new_size_local) =
                        self.fs.stream_ctz_extend(ctz, &ZERO_CHUNK[..chunk], buf_a, buf_b)?;
                    self.head_block = BlockAddress::new(new_head);
                    self.size = new_size_local;
                    remaining -= chunk as u32;
                }
                self.pos = saved_pos.min(self.size);
                self.dirty = true;
                Ok(())
            }
        }
    }

    /// Commit any pending writes to the metadata pair. After this
    /// returns, a remount sees the file at its in-memory state. If
    /// the file is not dirty, this is a cheap no-op.
    pub fn sync(&mut self, buf_a: &mut [u8], buf_b: &mut [u8]) -> Result<(), Error> {
        if !self.dirty {
            return Ok(());
        }
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if self.head_block.is_none() {
            // Logical empty file: replace the entry's STRUCT with an
            // empty InlineStruct, dropping any prior CTZ chain.
            self.fs.update_inline_at_id(self.parent, self.id, &[], buf_a, buf_b)?;
        } else {
            self.fs.commit_update_ctz(
                self.parent,
                self.id,
                self.head_block.as_u32(),
                self.size,
                buf_a,
                buf_b,
            )?;
        }
        // This commit may have superseded a prior CTZ chain (overwrite,
        // truncate-to-zero, or set_len shrink), orphaning its blocks.
        // Drop the allocator lookahead so they are reclaimed promptly.
        self.fs.invalidate_alloc_cache();
        self.dirty = false;
        Ok(())
    }

    /// Sync any pending writes and consume the handle, releasing the
    /// borrow on the parent [`Fs`].
    pub fn close(mut self, buf_a: &mut [u8], buf_b: &mut [u8]) -> Result<(), Error> {
        self.sync(buf_a, buf_b)
    }
}

impl<S: Storage> Drop for File<'_, S> {
    fn drop(&mut self) {
        // Drop discards pending writes by design (the docs say so;
        // sync() must run explicitly to commit). We don't even
        // log/warn here because Drop has no way to surface diagnostics
        // and the silently-dropped chain blocks are reclaimable by
        // the next allocator scan, so no corruption occurs.
    }
}

// ---- Internal bridges from File into Fs ----------------------------------
//
// `File` needs three Fs internals: resolve_parent (path walking),
// write_inline_to_pair (to materialize a new entry on `create`), and
// an inline update for a specific id (to replace a CtzStruct with
// an empty InlineStruct on truncate-to-zero). Each is exposed as a
// thin `pub(crate)` wrapper here to avoid widening Fs's public API.

impl<S: Storage> Fs<S> {
    pub(crate) fn resolve_parent_for_file<'p>(
        &mut self,
        path: Path<'p>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(BlockPair, &'p str), Error> {
        self.resolve_parent(path, buf_a, buf_b)
    }

    pub(crate) fn create_empty_entry_for_file(
        &mut self,
        parent: BlockPair,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(BlockPair, u16), Error> {
        // Materialize a new entry with an empty inline body. The first
        // sync replaces it with a CtzStruct. The entry is committed to
        // the last pair of the parent's HardTail chain; re-seek the chain
        // to learn which pair it landed in and its local id, since the
        // file handle must cache the entry's actual owning pair.
        self.write_inline_to_pair_for_file(parent, name, &[], buf_a, buf_b)?;
        match self.seek_entry_in_chain(parent, name, buf_a, buf_b)? {
            crate::fs::ChainSeek::Found { pair, id, .. } => Ok((pair, id)),
            crate::fs::ChainSeek::Absent { .. } => Err(Error::Corrupt),
        }
    }

    pub(crate) fn write_inline_to_pair_for_file(
        &mut self,
        parent: BlockPair,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.write_inline_to_pair(parent, name, content, buf_a, buf_b)
    }
}
