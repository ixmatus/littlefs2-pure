//! Directory entry enumeration.
//!
//! A directory in LittleFS is materialized as a metadata pair whose tag
//! stream carries NAME and STRUCT tags for each entry. The NAME tag's
//! type field discriminates regular files from sub-directories
//! ([`TagType::RegularFile`] vs [`TagType::Directory`]); the NAME tag's
//! body holds the entry's name as raw bytes; the matching STRUCT tag at
//! the same id holds the entry's storage layout (inline data, CTZ skip
//! list head, or sub-directory pair address).
//!
//! # The three views
//!
//! - [`entries`] is the raw walker: one entry per NAME tag in commit
//!   order, no splice handling. Useful when every committed tag must be
//!   observable (e.g. conformance debugging).
//! - [`live_entries`] is the splice-correct walker: a `Delete` tag at
//!   id `N` removes the entry at `N` and renumbers entries with
//!   `id > N` down by one. This is what user-visible APIs walk.
//! - [`lookup`] is the single-pair name lookup, splice-correct, and
//!   returns the entry plus its paired STRUCT body (so an inline file's
//!   contents are reachable in one walk).
//!
//! All three operate on a single [`MetadataPair`]. Crossing
//! HardTail-threaded continuation pairs is the caller's responsibility;
//! [`crate::Fs::resolve`] handles it for path walks and
//! [`crate::Fs::list_dir`] handles it for enumeration.

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

/// The kinds of entries a directory can hold.
///
/// The LittleFS v2 on-disk format defines two kinds today (regular file,
/// directory); the enum is `#[non_exhaustive]` so a future spec revision
/// or fork extension can grow this without a major version bump. Callers
/// pattern matching on [`EntryKind`] must include a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
/// commit order. This walker does **not** apply splice (Create/Delete)
/// renumbering and may yield entries that a later `Delete` removed; use
/// [`live_entries`] for the splice-correct view.
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
///
/// **Raw walker.** Yields every NAME tag in commit order, including
/// names of entries that were subsequently deleted by a [`TagType::Delete`]
/// splice tag. Use [`live_entries`] for splice-correct enumeration.
#[must_use]
pub fn entries<'a>(pair: &MetadataPair<'a>) -> Entries<'a> {
    Entries { inner: pair.reader.iter_tags() }
}

/// Maximum number of live entries per metadata pair supported by
/// [`live_entries`].
///
/// The pair's tag stream is bounded by block_size, and each entry needs
/// at minimum a 4 byte NAME tag plus a 12 byte STRUCT tag, so a 4 KiB
/// block tops out around 250 entries; 256 covers that with margin. The
/// state array is `MAX_LIVE_ENTRIES * sizeof(EntrySlot)` bytes on the
/// stack, so a smaller cap may be appropriate for tight embedded
/// targets (this is a future tunable).
pub const MAX_LIVE_ENTRIES: usize = 256;

/// Walk a metadata pair's tag stream, applying splice (Create / Delete)
/// renumbering, and invoke `f` for each live entry in current id order.
///
/// "Live" means the entry has a NAME tag and has not been removed by a
/// subsequent [`TagType::Delete`] tag in the same commit log. Ids are
/// assigned by walk position after renumbering, not by the on disk id
/// field of the originating NAME tag.
///
/// Returns the total number of live entries on success. Returns
/// [`crate::error::Error::OutOfRange`] if the directory at any point
/// exceeds [`MAX_LIVE_ENTRIES`], or [`crate::error::Error::Corrupt`] if
/// a splice tag references an id outside the current count.
///
/// # Algorithm
///
/// Mirrors the splice handling in `lfs_dir_fetchmatch` (`lfs.c:1095`).
/// Walking forward in commit order, the iterator maintains an array of
/// "slots" indexed by current id. A Create at id `i` shifts slots up;
/// a Delete at id `i` shifts them down. NAME tags at id `i` populate
/// the slot's name and kind. At the end, slots `0..count` hold the
/// final state.
pub fn live_entries<'a, F, E>(
    pair: &MetadataPair<'a>,
    mut f: F,
) -> Result<usize, crate::error::Error>
where
    F: FnMut(DirEntry<'a>) -> Result<(), E>,
    E: Into<crate::error::Error>,
{
    let mut slots: [Option<DirEntry<'a>>; MAX_LIVE_ENTRIES] = [None; MAX_LIVE_ENTRIES];
    let mut count: usize = 0;

    for entry in pair.reader.iter_tags() {
        let tag = entry.tag;
        let id = tag.id() as usize;
        match tag.tag_type() {
            TagType::Create => {
                if count >= MAX_LIVE_ENTRIES {
                    return Err(crate::error::Error::OutOfRange);
                }
                if id > count {
                    return Err(crate::error::Error::Corrupt);
                }
                let mut i = count;
                while i > id {
                    slots[i] = slots[i - 1];
                    i -= 1;
                }
                slots[id] = None;
                count += 1;
            }
            TagType::Delete => {
                if id >= count {
                    return Err(crate::error::Error::Corrupt);
                }
                let mut i = id;
                while i + 1 < count {
                    slots[i] = slots[i + 1];
                    i += 1;
                }
                slots[count - 1] = None;
                count -= 1;
            }
            TagType::RegularFile | TagType::Directory | TagType::Superblock => {
                if id >= count {
                    // A NAME tag without a prior Create is allowed at the
                    // boundary `id == count` (legacy commits without an
                    // explicit Create implicitly bump the count). Reject
                    // anything beyond.
                    if id == count && count < MAX_LIVE_ENTRIES {
                        count += 1;
                    } else {
                        return Err(crate::error::Error::Corrupt);
                    }
                }
                // The Superblock NAME counts toward the entry count
                // (id 0 of the root pair) but is not emitted to the
                // caller's callback. Track it as a `None` slot so the
                // slot count stays in sync with `gather_live_slots` in
                // the write path.
                let kind = match tag.tag_type() {
                    TagType::RegularFile => Some(EntryKind::RegularFile),
                    TagType::Directory => Some(EntryKind::Directory),
                    TagType::Superblock => None,
                    _ => unreachable!(),
                };
                slots[id] = kind.map(|k| DirEntry { id: id as u16, name: entry.body, kind: k });
            }
            _ => {} // STRUCT / CCRC / FCRC / etc. don't affect the entry roster.
        }
    }

    for (i, slot) in slots.iter().enumerate().take(count) {
        if let Some(mut e) = *slot {
            e.id = i as u16;
            f(e).map_err(Into::into)?;
        }
    }
    Ok(count)
}

/// A resolved entry: the directory entry plus its STRUCT body.
///
/// Returned by [`lookup`]. The `struct_body` slice's interpretation
/// depends on `entry.kind`:
///
/// - [`EntryKind::RegularFile`] with an [`crate::TagType::InlineStruct`]
///   STRUCT tag: `struct_body` *is* the file content (inline small file).
/// - [`EntryKind::RegularFile`] with an [`crate::TagType::CtzStruct`]
///   STRUCT tag: `struct_body` is 8 bytes encoding the CTZ skip list
///   head block (LE `u32`) followed by the file size (LE `u32`). Decode
///   via [`crate::ctz::CtzStruct::from_bytes`] and walk the chain with
///   [`crate::ctz::read_ctz`] (or [`crate::Fs::read_ctz`]).
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

/// Internal slot used by [`lookup`] to track each live entry's
/// latest NAME and STRUCT tag bodies, indexed by current id.
#[derive(Clone, Copy)]
struct LookupSlot<'a> {
    /// `Some(name_bytes)` once a NAME has been seen at this id.
    name: Option<&'a [u8]>,
    /// `Some(kind)` for `RegularFile` / `Directory`; `None` for the
    /// Superblock NAME (counted but not user visible).
    kind: Option<EntryKind>,
    /// `Some((struct_type, struct_body))` once a STRUCT has been seen.
    struct_data: Option<(TagType, &'a [u8])>,
}

impl LookupSlot<'_> {
    const EMPTY: Self = Self { name: None, kind: None, struct_data: None };
}

/// Look up a directory entry by name within a single metadata pair.
///
/// Walks the tag stream applying splice (Create / Delete) renumbering,
/// and looks for a slot whose NAME body equals `name` and whose entry
/// kind is a user visible one (RegularFile or Directory). Returns
/// `None` if the entry is missing, has been deleted by a subsequent
/// splice, or has no corresponding STRUCT tag in the pair.
///
/// # Scope
///
/// Single pair only. Does not chase HardTail tags into adjacent pairs;
/// callers wanting that should walk the chain themselves (or use
/// [`crate::Fs::resolve`] which does).
#[must_use]
pub fn lookup<'a>(pair: &MetadataPair<'a>, name: &[u8]) -> Option<Resolved<'a>> {
    let mut slots: [LookupSlot<'a>; MAX_LIVE_ENTRIES] = [LookupSlot::EMPTY; MAX_LIVE_ENTRIES];
    let mut count: usize = 0;

    for entry in pair.reader.iter_tags() {
        let tag = entry.tag;
        let id = tag.id() as usize;
        match tag.tag_type() {
            TagType::Create => {
                if count >= MAX_LIVE_ENTRIES || id > count {
                    return None;
                }
                let mut i = count;
                while i > id {
                    slots[i] = slots[i - 1];
                    i -= 1;
                }
                slots[id] = LookupSlot::EMPTY;
                count += 1;
            }
            TagType::Delete => {
                if id >= count {
                    return None;
                }
                let mut i = id;
                while i + 1 < count {
                    slots[i] = slots[i + 1];
                    i += 1;
                }
                slots[count - 1] = LookupSlot::EMPTY;
                count -= 1;
            }
            TagType::RegularFile | TagType::Directory | TagType::Superblock => {
                if id >= count {
                    if id == count && count < MAX_LIVE_ENTRIES {
                        slots[id] = LookupSlot::EMPTY;
                        count += 1;
                    } else {
                        return None;
                    }
                }
                slots[id].name = Some(entry.body);
                slots[id].kind = match tag.tag_type() {
                    TagType::RegularFile => Some(EntryKind::RegularFile),
                    TagType::Directory => Some(EntryKind::Directory),
                    TagType::Superblock => None,
                    _ => unreachable!(),
                };
            }
            ty @ (TagType::InlineStruct | TagType::CtzStruct | TagType::DirStruct)
                if id < count =>
            {
                slots[id].struct_data = Some((ty, entry.body));
            }
            _ => {}
        }
    }

    // Find a slot whose name matches and whose kind is user visible
    // (i.e., not the Superblock).
    for (i, slot) in slots.iter().enumerate().take(count) {
        let (Some(slot_name), Some(kind), Some((struct_type, struct_body))) =
            (slot.name, slot.kind, slot.struct_data)
        else {
            continue;
        };
        if slot_name == name {
            return Some(Resolved {
                entry: DirEntry { id: i as u16, name: slot_name, kind },
                struct_type,
                struct_body,
            });
        }
    }
    None
}
