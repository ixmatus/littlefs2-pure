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
