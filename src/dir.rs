//! Directory entry enumeration.
//!
//! A directory in LittleFS is materialized as a metadata pair whose tag
//! stream carries NAME and STRUCT tags for each entry. The NAME tag's type
//! field discriminates regular files from sub directories
//! ([`TagType::RegularFile`] vs [`TagType::Directory`]); the NAME tag's
//! body holds the entry's name as raw bytes; the matching STRUCT tag at
//! the same id holds the entry's storage layout (inline data, CTZ skip
//! list head, or sub directory pair address).
//!
//! # Scope of this module
//!
//! Phase 1e exposes a forward iterator over NAME tags in a single
//! [`MetadataPair`]. Each yielded [`DirEntry`] carries the id, the name
//! bytes, and the kind (file or directory).
//!
//! What this module does **not** yet do:
//!
//! - **Splice handling.** A [`TagType::Delete`] tag for id `N` removes the
//!   entry at `N` and renumbers entries with id `> N` down by one. The
//!   current iterator ignores Splice tags and may yield deleted entries.
//!   Acceptable for read only mounts of freshly written filesystems; will
//!   be tightened before Phase 2.
//! - **HardTail chasing.** A directory whose entries overflow one metadata
//!   pair is split across pairs threaded via [`TagType::HardTail`] tags.
//!   The current iterator covers one pair only.
//! - **Path resolution.** Walking from the root pair down to an arbitrary
//!   subdirectory by name requires composing this iterator with a follow
//!   step on each [`DirEntry::kind`] of `Directory`. Phase 1f deliverable.

use crate::meta::{MetadataPair, TagIter};
use crate::tag::TagType;

/// One directory entry: an id, a name, and a kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntry<'a> {
    /// Per metadata pair identifier. Together with the pair address it
    /// uniquely locates the entry.
    pub id: u16,
    /// Entry name, as stored on disk. LittleFS does not enforce UTF-8;
    /// callers that want a string should validate.
    pub name: &'a [u8],
    /// Whether this entry is a regular file or a sub directory.
    pub kind: EntryKind,
}

/// The two kinds of entries a directory can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file. Its content layout (inline or CTZ skip list) is
    /// described by a [`TagType::InlineStruct`] or
    /// [`TagType::CtzStruct`] tag at the same id.
    RegularFile,
    /// A sub directory. Its metadata pair address is in the
    /// [`TagType::DirStruct`] tag at the same id (8 byte body, two LE
    /// `u32` block addresses).
    Directory,
}

/// Iterator over the directory entries of a [`MetadataPair`].
///
/// Returned by [`entries`]. Yields one [`DirEntry`] per NAME tag, in
/// commit order. See the module documentation for the limitations of the
/// Phase 1e scope.
#[derive(Clone, Debug)]
pub struct Entries<'a> {
    inner: TagIter<'a>,
}

impl<'a> Iterator for Entries<'a> {
    type Item = DirEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.inner.next()?;
            let kind = match entry.tag.tag_type() {
                TagType::RegularFile => EntryKind::RegularFile,
                TagType::Directory => EntryKind::Directory,
                _ => continue,
            };
            return Some(DirEntry { id: entry.tag.id(), name: entry.body, kind });
        }
    }
}

/// Construct an entry iterator over a metadata pair.
#[must_use]
pub fn entries<'a>(pair: &MetadataPair<'a>) -> Entries<'a> {
    Entries { inner: pair.reader.iter_tags() }
}

/// A resolved entry: the directory entry plus its STRUCT body.
///
/// Returned by [`lookup`]. The `struct_body` slice's interpretation
/// depends on `entry.kind`:
///
/// - [`EntryKind::RegularFile`] with an [`crate::TagType::InlineStruct`]
///   STRUCT tag: `struct_body` *is* the file content (inline small file).
/// - [`EntryKind::RegularFile`] with an [`crate::TagType::CtzStruct`]
///   STRUCT tag: `struct_body` is 8 bytes encoding the CTZ skip list head
///   block (LE `u32`) followed by the file size (LE `u32`). Phase 1g
///   adds the helper to follow the skip list.
/// - [`EntryKind::Directory`] with a [`crate::TagType::DirStruct`] STRUCT
///   tag: `struct_body` is 8 bytes, two LE `u32`s addressing the
///   subdirectory's metadata pair.
///
/// The `struct_type` field disambiguates without re-parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved<'a> {
    /// The directory entry.
    pub entry: DirEntry<'a>,
    /// The type of the STRUCT tag that paired with the NAME.
    pub struct_type: crate::tag::TagType,
    /// The STRUCT tag's body bytes.
    pub struct_body: &'a [u8],
}

/// Look up a directory entry by name within a single metadata pair.
///
/// Walks the tag stream looking for:
///
/// 1. A NAME tag (RegularFile or Directory) whose body equals `name`.
/// 2. A STRUCT tag at the same id (a [`crate::TagType::InlineStruct`],
///    [`crate::TagType::CtzStruct`], or [`crate::TagType::DirStruct`]).
///
/// Returns `None` if no NAME matches, or if the matching NAME's id has
/// no STRUCT tag in the same pair. The "no STRUCT" case can happen in
/// partially-written commits and is treated by this read path as
/// "entry incomplete, do not yield".
///
/// # Scope
///
/// Single pair only. Does not chase HardTail tags into adjacent pairs.
/// Does not apply splice / Delete renumbering. See module docs for the
/// Phase 1e scope.
#[must_use]
pub fn lookup<'a>(pair: &MetadataPair<'a>, name: &[u8]) -> Option<Resolved<'a>> {
    // First pass: find the NAME tag with the matching body, record its
    // id and kind.
    let mut found: Option<(u16, EntryKind, &'a [u8])> = None;
    for entry in pair.reader.iter_tags() {
        match entry.tag.tag_type() {
            TagType::RegularFile if entry.body == name => {
                found = Some((entry.tag.id(), EntryKind::RegularFile, entry.body));
            }
            TagType::Directory if entry.body == name => {
                found = Some((entry.tag.id(), EntryKind::Directory, entry.body));
            }
            _ => {}
        }
    }
    let (id, kind, name_slice) = found?;

    // Second pass: find a STRUCT tag at the same id. Latest wins
    // (mirroring the "later commits supersede earlier" rule).
    let mut struct_body: Option<(crate::tag::TagType, &'a [u8])> = None;
    for entry in pair.reader.iter_tags() {
        if entry.tag.id() != id {
            continue;
        }
        if let ty @ (TagType::InlineStruct | TagType::CtzStruct | TagType::DirStruct) =
            entry.tag.tag_type()
        {
            struct_body = Some((ty, entry.body));
        }
    }
    let (struct_type, body) = struct_body?;
    Some(Resolved {
        entry: DirEntry { id, name: name_slice, kind },
        struct_type,
        struct_body: body,
    })
}
