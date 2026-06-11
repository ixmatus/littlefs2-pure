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

use crate::meta::{MetadataPair, MetadataReader, TagIter};
use crate::tag::TagType;

/// Outcome of feeding one tag through [`splice_step`].
pub(crate) enum SpliceStep {
    /// The tag was a splice (Create / Delete) and has been applied to
    /// the slot array; the caller has nothing further to do.
    Consumed,
    /// The tag was a NAME (RegularFile / Directory / Superblock) at the
    /// given live id; the count now covers it. The caller stores its
    /// name and kind into the slot, *without* resetting any other slot
    /// fields (a STRUCT tag for this id may already be parked there;
    /// see the id-density note below).
    Name(usize),
    /// Any other tag type; the caller decides (STRUCT storage, etc.).
    Other,
}

/// Shared splice state machine: the one Create / Delete / NAME
/// renumbering core all four live-entry walkers feed tags through
/// ([`live_entries`], [`lookup`], `fs::gather_live_slots`,
/// `alloc::gather_live_structs`). Review finding H1 lived in the
/// divergence risk of four hand-rolled copies; this is the single
/// derivation, checked against `lfs_dir_fetchmatch` (`lfs.c:1233ff`).
///
/// # Id density (review H1)
///
/// The C reference accepts a NAME tag at *any* id and grows the entry
/// count to `max(id + 1, count)` (`lfs.c`, `tempcount` handling): C
/// compaction emits surviving tags in log order, which after a rename
/// is not ascending-id order, and an entry's STRUCT can precede the
/// NAME tags that establish the count. Therefore:
///
/// - a NAME at `id >= count` sets `count = id + 1` (not Corrupt), and
///   does not clear the slots it newly covers: a STRUCT already parked
///   there must survive;
/// - callers must store STRUCT tags for any `id < MAX_LIVE_ENTRIES`,
///   parked until a later NAME covers the id (slots beyond the final
///   count are simply never read out).
///
/// Parked slots cannot be displaced by splice shifts before their NAME
/// arrives: tags reference live ids at write time, so an id can only
/// exceed the running count inside a compacted prefix, and a compacted
/// prefix contains no splice tags.
///
/// Splice tags keep their strict bounds checks (`Create` beyond the
/// count, `Delete` of a nonexistent id): the C reference never writes
/// such logs, so they remain genuine corruption signals.
pub(crate) fn splice_step<T: Copy>(
    slots: &mut [T; MAX_LIVE_ENTRIES],
    count: &mut usize,
    empty: T,
    tag: crate::tag::Tag,
) -> Result<SpliceStep, crate::error::Error> {
    let id = tag.id() as usize;
    match tag.tag_type() {
        TagType::Create => {
            if *count >= MAX_LIVE_ENTRIES {
                return Err(crate::error::Error::OutOfRange);
            }
            if id > *count {
                return Err(crate::error::Error::Corrupt);
            }
            let mut i = *count;
            while i > id {
                slots[i] = slots[i - 1];
                i -= 1;
            }
            slots[id] = empty;
            *count += 1;
            Ok(SpliceStep::Consumed)
        }
        TagType::Delete => {
            if id >= *count {
                return Err(crate::error::Error::Corrupt);
            }
            let mut i = id;
            while i + 1 < *count {
                slots[i] = slots[i + 1];
                i += 1;
            }
            slots[*count - 1] = empty;
            *count -= 1;
            Ok(SpliceStep::Consumed)
        }
        TagType::RegularFile | TagType::Directory | TagType::Superblock => {
            if id >= MAX_LIVE_ENTRIES {
                return Err(crate::error::Error::OutOfRange);
            }
            if id >= *count {
                *count = id + 1;
            }
            Ok(SpliceStep::Name(id))
        }
        _ => Ok(SpliceStep::Other),
    }
}

/// Read the live value of user attribute `attr_id` on the entry whose
/// *current* live id is `live_id`, splice-correct (review C2).
///
/// Walks the committed log newest-to-oldest carrying a splice diff,
/// the exact algorithm of the C reference's `lfs_dir_getslice`
/// (`lfs.c:706-748`): at each step `adj = live_id - gdiff` is the id
/// the target entry had at that point in the log. A Create at `adj`
/// means the walk reached the entry's own creation (no older tag can
/// belong to it); a splice at or below `adj` shifts the diff; the
/// first `UserAttr(attr_id)` tag at `adj` is the live value, with the
/// `0x3FF` length sentinel meaning "removed".
///
/// Returns `None` for absent and removed alike, matching `get_attr`'s
/// `Ok(0)` contract.
pub(crate) fn attr_get<'a>(
    reader: &MetadataReader<'a>,
    live_id: u16,
    attr_id: u8,
) -> Option<&'a [u8]> {
    let mut gdiff: i32 = 0;
    for entry in reader.iter_tags_rev() {
        let tag = entry.tag;
        let id = i32::from(tag.id());
        let adj = i32::from(live_id) - gdiff;
        match tag.tag_type() {
            TagType::Create if id <= adj => {
                if id == adj {
                    // Found where the entry was created; nothing older
                    // can belong to it.
                    return None;
                }
                gdiff += 1;
            }
            TagType::Delete if id <= adj => {
                gdiff -= 1;
            }
            TagType::UserAttr(a) if a == attr_id && id == adj => {
                return if tag.is_special_length() { None } else { Some(entry.body) };
            }
            _ => {}
        }
    }
    None
}

/// Invoke `f(attr_id, body)` for every *live* user attribute of the
/// entry whose current live id is `live_id`, newest-first.
///
/// Same splice-diff walk as [`attr_get`], with a 256-bit seen bitmap
/// so only the latest tag per `attr_id` is considered; delete-marker
/// tags consume their `attr_id` without invoking `f`. This is the
/// compaction replay source (review C1): the C reference's
/// `lfs_dir_compact` replays all unique tags per live id, attributes
/// included (`lfs_dir_traverse` filter, `lfs.c:1988ff`).
pub(crate) fn for_each_live_attr<'a, E, F>(
    reader: &MetadataReader<'a>,
    live_id: u16,
    mut f: F,
) -> Result<(), E>
where
    F: FnMut(u8, &'a [u8]) -> Result<(), E>,
{
    let mut seen = [0u32; 8];
    let mut gdiff: i32 = 0;
    for entry in reader.iter_tags_rev() {
        let tag = entry.tag;
        let id = i32::from(tag.id());
        let adj = i32::from(live_id) - gdiff;
        match tag.tag_type() {
            TagType::Create if id <= adj => {
                if id == adj {
                    break;
                }
                gdiff += 1;
            }
            TagType::Delete if id <= adj => {
                gdiff -= 1;
            }
            TagType::UserAttr(a) if id == adj => {
                let word = (a >> 5) as usize;
                let bit = 1u32 << (a & 31);
                if seen[word] & bit == 0 {
                    seen[word] |= bit;
                    if !tag.is_special_length() {
                        f(a, entry.body)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

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
        match splice_step(&mut slots, &mut count, None, tag)? {
            // STRUCT / CCRC / FCRC / etc. don't affect the roster.
            SpliceStep::Consumed | SpliceStep::Other => {}
            SpliceStep::Name(id) => {
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

// Stack budget guard. `lookup` stacks a `[LookupSlot; MAX_LIVE_ENTRIES]`
// scratch array as a local; at 48 bytes per slot that is the dominant
// contributor to its frame (~12 KiB). `lookup` is called from the
// resolve/rename/rmdir paths in `fs`, so on the Cortex-M0+ ship target
// this peak is a documented, pinned budget. If `LookupSlot` grows, this
// fails to compile until docs/decisions/0006-stack-budget.md is
// revisited.
// Portable budget guard. `LookupSlot` carries pointers, so its byte
// size is pointer-width dependent (24 bytes on the 32-bit
// `thumbv6m-none-eabi` ship target, 48 on a 64-bit host). Pinning an
// absolute count would be wrong on one or the other, so instead bound
// the slot by the sum of its three documented fields plus one word of
// slack: a fourth field cannot be added without tripping this, on any
// target. See docs/decisions/0006-stack-budget.md.
const _: () = assert!(
    core::mem::size_of::<LookupSlot<'static>>()
        <= core::mem::size_of::<Option<&[u8]>>()
            + core::mem::size_of::<Option<(TagType, &[u8])>>()
            + core::mem::size_of::<Option<EntryKind>>()
            + core::mem::size_of::<usize>(),
    "LookupSlot grew past its three documented fields; revisit docs/decisions/0006-stack-budget.md"
);

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
///
/// # Stack budget
///
/// This function stacks a `[LookupSlot; MAX_LIVE_ENTRIES]` local, which
/// dominates its frame. `LookupSlot` carries pointers, so its size is
/// pointer-width dependent: 24 bytes on the 32-bit
/// `thumbv6m-none-eabi` ship target (a 6144-byte / 6 KiB array) and
/// 48 bytes on a 64-bit host (12 KiB). It is called from the resolve,
/// rename, and rmdir paths in [`crate::fs`], so on the Cortex M0+ ship
/// target this 6 KiB peak is a deliberate, pinned budget. The portable
/// static assertion above fails the build if `LookupSlot` gains a
/// field; `docs/decisions/0006-stack-budget.md` records the accounting
/// and why a caller-supplied buffer was not adopted in 1.x.
#[must_use]
pub fn lookup<'a>(pair: &MetadataPair<'a>, name: &[u8]) -> Option<Resolved<'a>> {
    let mut slots: [LookupSlot<'a>; MAX_LIVE_ENTRIES] = [LookupSlot::EMPTY; MAX_LIVE_ENTRIES];
    let mut count: usize = 0;

    for entry in pair.reader.iter_tags() {
        let tag = entry.tag;
        let id = tag.id() as usize;
        match splice_step(&mut slots, &mut count, LookupSlot::EMPTY, tag) {
            Err(_) => return None,
            Ok(SpliceStep::Consumed) => {}
            Ok(SpliceStep::Name(id)) => {
                slots[id].name = Some(entry.body);
                slots[id].kind = match tag.tag_type() {
                    TagType::RegularFile => Some(EntryKind::RegularFile),
                    TagType::Directory => Some(EntryKind::Directory),
                    TagType::Superblock => None,
                    _ => unreachable!(),
                };
            }
            Ok(SpliceStep::Other) => match tag.tag_type() {
                // STRUCT tags may precede the NAME that establishes
                // their id's count in a C-compacted log (review H1);
                // park them and let a later NAME claim the slot.
                ty @ (TagType::InlineStruct | TagType::CtzStruct | TagType::DirStruct)
                    if id < MAX_LIVE_ENTRIES =>
                {
                    slots[id].struct_data = Some((ty, entry.body));
                }
                _ => {}
            },
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
