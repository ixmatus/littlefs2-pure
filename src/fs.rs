//! Mounted filesystem handle.
//!
//! [`Fs::mount`] is the entry point. It reads the root metadata pair
//! (blocks `0` and `1`) through the provided [`Storage`]-backed device,
//! picks the active block via [`MetadataPair::parse`], parses the
//! superblock via [`Superblock::from_pair`], validates the geometry
//! against the storage trait's advertised constants, and finishes any
//! in-flight cross-directory rename via the gstate recovery walk (see
//! [`crate::gstate`]).
//!
//! After mount, the returned [`Fs`] holds:
//!
//! - the storage handle (owned, recoverable via [`Fs::into_storage`]);
//! - the decoded [`Superblock`];
//! - the address of the root metadata pair (always
//!   [`crate::ROOT_BLOCK_PAIR`] for v2).
//!
//! Every public mutation re-reads the affected metadata pair through
//! two caller-supplied scratch buffers (each exactly
//! [`S::BLOCK_SIZE`](Storage::BLOCK_SIZE) bytes) and syncs the storage
//! before returning. The buffers are reusable across calls; the `Fs`
//! does not retain a borrow of them, so a `no_std` consumer can
//! size one pair of buffers and reuse them for the life of the mount.
//!
//! # Compaction and wear levelling
//!
//! Every commit dispatch checks whether the active block has room for
//! the new tags. If yes, the tags are appended in place (one program
//! call, no erase). If not, the live state is GC'd and rewritten to
//! the alternate block in a fresh commit with an incremented revision
//! counter (the standard LittleFS rotate). When `S::BLOCK_CYCLES` is
//! positive and the rewritten pair is not the root, the compact-time
//! wear-levelling predicate (matching the C reference's
//! `(rev + 1) % ((BLOCK_CYCLES + 1) | 1) == 0`) may further redirect
//! the new commit onto a freshly allocated block, with the parent's
//! `DirStruct` entry flipped to the new pair address through a single
//! follow-up commit. See `docs/decisions/0005-wear-leveling-pair-relocation.md`
//! for the atomicity model.

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

// Stack budget guard. `gather_live_slots` callers stack a
// `[SlotOffsets; MAX_LIVE_ENTRIES]` scratch array, and that array is on
// the frame of `apply_op_to_pair_inner` while it recurses through
// `propagate_relocation`. The Cortex-M0+ ship target has a small stack,
// so the per-frame cost of this array is a documented, pinned budget,
// not an incidental detail. If `SlotOffsets` grows, this fails to
// compile until docs/decisions/0006-stack-budget.md is revisited.
const _: () = assert!(
    core::mem::size_of::<SlotOffsets>() == 10
        && core::mem::size_of::<[SlotOffsets; MAX_LIVE_ENTRIES]>() == 2560,
    "SlotOffsets stack budget changed; revisit docs/decisions/0006-stack-budget.md"
);

/// Maximum chain length a single CTZ write can produce.
///
/// At 4 KiB blocks this caps a single full-rewrite CTZ write at ~1 MiB;
/// at 256 byte blocks (the test geometry) it caps at 64 KiB. The chain
/// address table is stack-allocated as
/// `[BlockAddress; MAX_CTZ_WRITE_BLOCKS]` (1 KiB at the current cap).
/// The streaming [`Fs::append_to_path`] is not bounded by this cap; it
/// only allocates the blocks needed for the appended overflow and
/// reuses the existing chain blocks in place.
const MAX_CTZ_WRITE_BLOCKS: usize = 256;

/// Maximum distinct bad blocks a single CTZ write will relocate past
/// before giving up with [`Error::Io`]. A worn block is excluded from the
/// write's allocation and the chain is rebuilt; this bounds the retries so
/// a wholly-failing device terminates rather than looping.
const MAX_BAD_BLOCK_RETRIES: usize = 8;

/// Cycle-safe walker for a HardTail chain, in O(1) memory and with no
/// arbitrary length cap (Brent's algorithm).
///
/// Every reader tail-walk (`Fs::resolve`'s final-component loop, the
/// internal `find_dir_pair`, and `list_pair_chain`) chases
/// `pair.reader.tail()` until a lookup hits or a pair has no tail. On a
/// well-formed image the chain is a finite acyclic list; the C
/// reference legitimately splits a large directory across many
/// continuation pairs. On a corrupt or adversarial image the tail can
/// point back into the chain (a self-cycle, or an A -> B -> A loop);
/// without a guard the walk reads storage forever and never returns.
///
/// A fixed visited array would either cap the legitimate chain length
/// (rejecting valid long C-written directories, the original defect) or
/// allocate. Brent's algorithm detects a cycle with three scalars and
/// no allocation, and imposes no length ceiling: a valid chain of any
/// length enumerates, a cyclic chain is rejected with
/// [`Error::Corrupt`]. This is exactly what the C reference does
/// (`lfs.c` `lfs_dir_fetchmatch` guards every tail walk and returns
/// `LFS_ERR_CORRUPT`). See ADR-0009.
///
/// `Error::Corrupt` (not `Error::OutOfRange`) is the right code: the
/// on-disk chain is malformed, matching the C oracle's classification.
///
/// Usage: construct with the chain start, process `current`, then for
/// each non-terminal step call [`advance`](Self::advance) with the next
/// pair before moving to it. A cycle is reported as the moving pointer
/// catches the periodically-teleported reference; a corrupt cyclic
/// chain may therefore be processed for O(mu + lambda) steps (bounded
/// by the device's finite block count) before the error, and any
/// entries a streaming caller already saw must be discarded on `Err`,
/// as for every other `Error` return.
struct BrentTailWalk {
    /// The reference ("tortoise") the moving pointer is compared
    /// against; teleported to the current node every `power` steps.
    saved: BlockPair,
    /// Steps remaining in the current power-of-two stride.
    power: u32,
    /// Steps taken since the last teleport.
    steps: u32,
}

impl BrentTailWalk {
    fn new(start: BlockPair) -> Self {
        Self { saved: start, power: 1, steps: 0 }
    }

    /// Advance to `next` (the tail of the pair just processed). Returns
    /// [`Error::Corrupt`] if `next` equals the saved reference, which
    /// for Brent's algorithm means the chain has a cycle.
    fn advance(&mut self, next: BlockPair) -> Result<(), Error> {
        if next == self.saved {
            return Err(Error::Corrupt);
        }
        self.steps += 1;
        if self.steps == self.power {
            self.saved = next;
            self.power = self.power.saturating_mul(2);
            self.steps = 0;
        }
        Ok(())
    }
}

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
///
/// # Stack budget
///
/// `slots` is a `[SlotOffsets; MAX_LIVE_ENTRIES]` (256 * 10 = 2560
/// bytes). It is not allocated here but every caller stacks it as a
/// local before the call, and one such caller is
/// [`Fs::apply_op_to_pair_inner`], which holds that 2.5 KiB frame while
/// it recurses through [`Fs::propagate_relocation`] back into itself
/// during wear-levelling relocation. On the Cortex-M0+ ship target this
/// is a deliberate, pinned budget rather than an accident; the static
/// assertion near [`MAX_LIVE_ENTRIES`] fails the build if `SlotOffsets`
/// grows, and `docs/decisions/0006-stack-budget.md` records the full
/// accounting and the recursion-depth bound.
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
        match crate::dir::splice_step(slots, &mut count, SlotOffsets::EMPTY, tag)? {
            crate::dir::SpliceStep::Consumed => {}
            crate::dir::SpliceStep::Name(id) => {
                slots[id].name_off = u16::try_from(body_off).map_err(|_| Error::OutOfRange)?;
                slots[id].name_len = u16::try_from(body_len).map_err(|_| Error::OutOfRange)?;
                slots[id].name_kind = match tag.tag_type() {
                    TagType::RegularFile => 0,
                    TagType::Directory => 1,
                    TagType::Superblock => 2,
                    _ => unreachable!(),
                };
            }
            crate::dir::SpliceStep::Other => match tag.tag_type() {
                // STRUCT tags may precede the NAME that establishes
                // their id's count in a C-compacted log (review H1);
                // park them and let a later NAME claim the slot.
                TagType::InlineStruct | TagType::CtzStruct | TagType::DirStruct
                    if id < MAX_LIVE_ENTRIES =>
                {
                    slots[id].struct_off =
                        u16::try_from(body_off).map_err(|_| Error::OutOfRange)?;
                    slots[id].struct_len =
                        u16::try_from(body_len).map_err(|_| Error::OutOfRange)?;
                    slots[id].struct_kind = match tag.tag_type() {
                        TagType::InlineStruct => 0,
                        TagType::CtzStruct => 1,
                        TagType::DirStruct => 2,
                        _ => unreachable!(),
                    };
                }
                _ => {}
            },
        }
    }
    Ok(count)
}

/// Live user attributes captured from a source pair, serialized into
/// a caller-owned stack buffer as consecutive
/// `[attr_id, len_lo, len_hi, value...]` records. Built by
/// [`stage_live_attrs`]; carried by the Create-family [`WriteOp`]s so
/// a cross-directory rename's destination commit re-emits the moved
/// entry's attributes atomically with its NAME and STRUCT (review H6,
/// the C reference's `LFS_FROM_MOVE` traversal, which replays all
/// unique tags of the moved id). Serialized rather than a slice of
/// slices so one stack pool bounds the whole capture without a second
/// fixed-size pointer array.
#[derive(Clone, Copy)]
pub(crate) struct StagedAttrs<'a> {
    /// Well-formed by construction in `stage_live_attrs`: each record
    /// header's length field never overruns the buffer.
    records: &'a [u8],
}

impl StagedAttrs<'_> {
    /// No attributes; what every caller other than the rename
    /// destination commit passes.
    pub(crate) const EMPTY: StagedAttrs<'static> = StagedAttrs { records: &[] };
}

impl<'a> StagedAttrs<'a> {
    fn iter(&self) -> StagedAttrIter<'a> {
        StagedAttrIter { rest: self.records }
    }

    /// On-disk dsize of the staged attrs: one 4-byte tag plus the
    /// value per record.
    fn dsize(&self) -> usize {
        self.iter().map(|(_, v)| 4 + v.len()).sum()
    }
}

struct StagedAttrIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for StagedAttrIter<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        if self.rest.len() < 3 {
            return None;
        }
        let attr_id = self.rest[0];
        let len = u16::from_le_bytes([self.rest[1], self.rest[2]]) as usize;
        if self.rest.len() < 3 + len {
            return None;
        }
        let (value, tail) = self.rest[3..].split_at(len);
        self.rest = tail;
        Some((attr_id, value))
    }
}

/// Stack budget for the attribute payload a cross-directory rename
/// stages, in bytes (each attribute costs its value length plus a
/// 3-byte record header). The same order as `rename`'s existing
/// 1 KiB struct-body stage; entries carrying more attribute bytes
/// than this rename with an explicit [`Error::OutOfRange`] rather
/// than silently dropping attributes (the pre-H6 behavior). The C
/// reference streams the source pair through its block caches during
/// the destination commit and has no such bound; lifting it here
/// needs a commit-time storage-backed attr stream, a 2.x candidate.
const RENAME_ATTR_STAGE: usize = 1024;

/// Serialize the live user attributes of `live_id` from `reader` into
/// `stage` (the [`StagedAttrs`] record format), returning the staged
/// view. Returns [`Error::OutOfRange`] when the attributes exceed the
/// stage; see [`Fs::rename`] for the documented bound.
fn stage_live_attrs<'s>(
    reader: &crate::meta::MetadataReader<'_>,
    live_id: u16,
    stage: &'s mut [u8],
) -> Result<StagedAttrs<'s>, Error> {
    let mut used = 0usize;
    crate::dir::for_each_live_attr(reader, live_id, |attr_id, body| {
        let need = 3 + body.len();
        if stage.len() - used < need {
            return Err(Error::OutOfRange);
        }
        stage[used] = attr_id;
        stage[used + 1..used + 3].copy_from_slice(&(body.len() as u16).to_le_bytes());
        stage[used + 3..used + need].copy_from_slice(body);
        used += need;
        Ok(())
    })?;
    Ok(StagedAttrs { records: &stage[..used] })
}

/// A pending write operation. Used by [`Fs::write_inline_to_root`],
/// [`Fs::remove_from_root`], and the CTZ write path to dispatch
/// through the same append-vs-compact machinery.
#[derive(Clone, Copy)]
pub(crate) enum WriteOp<'a> {
    /// Create a new entry at `id` (the next free id) with NAME `name`
    /// and InlineStruct `content`. `moved_attrs` carries the entry's
    /// user attributes when the create is the destination half of a
    /// cross-directory rename (review H6); plain creates pass
    /// [`StagedAttrs::EMPTY`].
    Create { id: u16, name: &'a [u8], content: &'a [u8], moved_attrs: StagedAttrs<'a> },
    /// Create a new entry at `id` whose content lives in a CTZ chain
    /// at `head_block` (the chain's tail block, per LittleFS
    /// convention). `total_size` is the file's byte length.
    /// `moved_attrs` as in [`WriteOp::Create`].
    CreateCtz {
        id: u16,
        name: &'a [u8],
        head_block: u32,
        total_size: u32,
        moved_attrs: StagedAttrs<'a>,
    },
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
    /// Remove the entry at `id`. Append path emits a `Delete` tag
    /// (length 0, the C reference's entry-delete encoding; review
    /// C3); compact path skips the slot and renumbers subsequent ids
    /// down.
    Remove { id: u16 },
    /// Create a new subdirectory entry at `id` with NAME `name` and
    /// `DirStruct` body pointing at `dir_pair`. `moved_attrs` as in
    /// [`WriteOp::Create`].
    CreateDir { id: u16, name: &'a [u8], dir_pair: BlockPair, moved_attrs: StagedAttrs<'a> },
    /// Update the NAME tag of the entry at `id` to `new_name`. The
    /// entry's kind (`name_type`) is preserved; the STRUCT tag is
    /// untouched. Used by [`Fs::rename_in_dir`].
    RenameInPlace { id: u16, name_type: crate::tag::TagType, new_name: &'a [u8] },
    /// Set / replace a user attribute on the entry at `id`. Latest
    /// `UserAttr(attr_id)` tag wins at read time.
    SetAttr { id: u16, attr_id: u8, value: &'a [u8] },
    /// Remove a user attribute from the entry at `id` by emitting a
    /// `UserAttr(attr_id)` tag with the delete-marker length sentinel
    /// (`0x3FF`).
    RemoveAttr { id: u16, attr_id: u8 },
    /// Rewrite the `DirStruct` body of the entry at `id` to point at
    /// `new_pair`. Used by wear-levelling pair relocation: when a
    /// directory's metadata pair migrates to fresh blocks, its parent's
    /// `DirStruct` reference is flipped to the new address through
    /// this op so the swap is atomic at the parent's commit boundary.
    UpdateDirStruct { id: u16, new_pair: BlockPair },
    /// No user-visible change. Used by mount-time recovery to emit a
    /// commit carrying only gstate tags (a balancing `MoveState` or
    /// `RelocateState`) plus the CCRC.
    Noop,
}

/// Blocks the allocator must treat as in-use during a commit even
/// though no committed metadata references them yet (review C9,
/// design D7). Every commit entry point names its in-flight state
/// explicitly; passing nothing requires the visible
/// [`Inflight::NONE`], so a new commit path cannot forget the
/// question.
///
/// Two shapes, matching how callers hold the information:
///
/// - `blocks`: individually named blocks (a relocation cascade's
///   fresh halves, a freshly written chain the caller has as a list).
/// - `chain`: an un-committed CTZ chain `(head, size)` whose blocks
///   are programmed but unreferenced (the stateful `File`'s batched
///   writes and the streaming append between extend and publish).
///   Carried as coordinates, not a list, so the hot path never walks
///   the chain (ADR-0011); the allocator walks it only on an
///   authoritative rescan, which is exactly the path that cannot see
///   un-committed blocks (ADR-0010's exclusion invariant).
///
/// Commit-internal allocations (the wear-relocation fresh block, the
/// worn-block retry rescan, the split continuation) consume both:
/// before review C9 they received an empty list from the publish
/// paths, so a retry rescan could hand the relocation the very chain
/// blocks the commit was publishing, destroying the file's data at
/// the moment it became durable (reproduced).
#[derive(Clone, Copy)]
pub(crate) struct Inflight<'a> {
    /// Individually named in-flight blocks.
    pub(crate) blocks: &'a [BlockAddress],
    /// An un-committed CTZ chain `(head_block, total_size)`.
    pub(crate) chain: Option<(u32, u32)>,
}

impl Inflight<'static> {
    /// No in-flight state: the commit publishes nothing whose blocks
    /// a rescan could miss.
    pub(crate) const NONE: Inflight<'static> = Inflight { blocks: &[], chain: None };
}

/// Outcome of walking a directory's HardTail chain looking for an entry
/// by name (see [`Fs::seek_entry_in_chain`]). A directory may span
/// several metadata pairs linked by `HardTail` tags; a name lives in one
/// of them, and a new entry is appended to the last pair of the chain.
pub(crate) enum ChainSeek {
    /// `name` lives in `pair` at local id `id` with kind `kind`. The
    /// scratch buffers hold `pair` on return, so the caller can re-parse
    /// it (e.g. to copy a STRUCT body) without another read.
    Found { pair: BlockPair, id: u16, kind: crate::dir::EntryKind },
    /// `name` is absent from the whole chain. A create targets
    /// `last_pair`, whose live-entry count is `count` (the local id a new
    /// entry would take). The scratch buffers hold `last_pair` on return.
    Absent { last_pair: BlockPair, count: usize },
}

/// Emit the tags for a [`WriteOp`] to an in-progress commit.
fn emit_op(commit: &mut crate::meta::Commit<'_>, op: &WriteOp<'_>) -> Result<(), Error> {
    use crate::tag::{Tag, TagType};
    match *op {
        WriteOp::Create { id, name, content, moved_attrs } => {
            commit.tag(Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(Tag::new(true, TagType::RegularFile, id, name.len() as u16), name)?;
            commit.tag(Tag::new(true, TagType::InlineStruct, id, content.len() as u16), content)?;
            for (a, v) in moved_attrs.iter() {
                commit.tag(Tag::new(true, TagType::UserAttr(a), id, v.len() as u16), v)?;
            }
        }
        WriteOp::CreateCtz { id, name, head_block, total_size, moved_attrs } => {
            commit.tag(Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(Tag::new(true, TagType::RegularFile, id, name.len() as u16), name)?;
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&head_block.to_le_bytes());
            body[4..8].copy_from_slice(&total_size.to_le_bytes());
            commit.tag(Tag::new(true, TagType::CtzStruct, id, 8), &body)?;
            for (a, v) in moved_attrs.iter() {
                commit.tag(Tag::new(true, TagType::UserAttr(a), id, v.len() as u16), v)?;
            }
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
            // Entry deletes carry length 0, matching every delete the
            // C reference writes (`lfs_remove`, `lfs_rename`,
            // `lfs_fs_demove`, `lfs_dir_drop`: all
            // `LFS_MKTAG(LFS_TYPE_DELETE, id, 0)`). The reserved
            // sentinel 0x3FF used here before review C3 was invisible
            // to `lfs_dir_fetchmatch`'s exact-compare besttag
            // invalidation (lfs.c:1244), so a C mount resolved the
            // deleted name to its NEIGHBOR entry and a C-side remove
            // destroyed that neighbor. This crate's own reader
            // dispatches deletes on the id alone and accepts both
            // encodings, so pre-fix images stay readable. Pinned by
            // the `remove` roundtrip scenario. Subsequent entries
            // with higher ids renumber down at read time via
            // `dir::live_entries`'s splice handling.
            commit.tag(Tag::new(true, TagType::Delete, id, 0), &[])?;
        }
        WriteOp::CreateDir { id, name, dir_pair, moved_attrs } => {
            commit.tag(Tag::new(true, TagType::Create, id, 0), &[])?;
            commit.tag(Tag::new(true, TagType::Directory, id, name.len() as u16), name)?;
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&dir_pair.a.as_u32().to_le_bytes());
            body[4..8].copy_from_slice(&dir_pair.b.as_u32().to_le_bytes());
            commit.tag(Tag::new(true, TagType::DirStruct, id, 8), &body)?;
            for (a, v) in moved_attrs.iter() {
                commit.tag(Tag::new(true, TagType::UserAttr(a), id, v.len() as u16), v)?;
            }
        }
        WriteOp::RenameInPlace { id, name_type, new_name } => {
            // Append a NAME tag at the existing id with the new
            // bytes. The reader's lookup picks the latest NAME for a
            // given id, so the entry surfaces under `new_name`.
            commit.tag(Tag::new(true, name_type, id, new_name.len() as u16), new_name)?;
        }
        WriteOp::SetAttr { id, attr_id, value } => {
            commit
                .tag(Tag::new(true, TagType::UserAttr(attr_id), id, value.len() as u16), value)?;
        }
        WriteOp::RemoveAttr { id, attr_id } => {
            // Length-sentinel 0x3FF = no body, delete marker. The
            // reader sees this and treats the attribute as removed.
            commit.tag(Tag::new(true, TagType::UserAttr(attr_id), id, 0x3FF), &[])?;
        }
        WriteOp::UpdateDirStruct { id, new_pair } => {
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&new_pair.a.as_u32().to_le_bytes());
            body[4..8].copy_from_slice(&new_pair.b.as_u32().to_le_bytes());
            commit.tag(Tag::new(true, TagType::DirStruct, id, 8), &body)?;
        }
        WriteOp::Noop => {}
    }
    Ok(())
}

/// How a [`WriteOp`] manifests on one emitted slot during a compaction
/// rebuild. [`slot_plan`] is the single, exhaustive derivation site
/// (review D4): a future `WriteOp` variant fails to compile there
/// until its compaction effect is declared, which closes the
/// silent-drop class behind review C1 (`SetAttr` / `RemoveAttr` fell
/// through a `_ => {}` wildcard, so a `set_attr` landing on a full
/// block compacted, persisted nothing, and returned `Ok`).
#[derive(Clone, Copy)]
struct SlotPlan<'a> {
    /// Skip this entry entirely (it is `Remove`'s target).
    drop_entry: bool,
    /// Emit a second NAME tag after the copied one
    /// (`RenameInPlace`'s new name); the latest NAME shadows at read
    /// time.
    extra_name: Option<(crate::tag::TagType, &'a [u8])>,
    /// Replace the slot's STRUCT tag type and body.
    struct_override: Option<(crate::tag::TagType, OverrideBody<'a>)>,
    /// Suppress this attr id during the source-log attr replay (the
    /// in-flight `SetAttr` / `RemoveAttr` supersedes the stored
    /// value).
    attr_suppress: Option<u8>,
    /// Emit this attr value after the replay (`SetAttr`'s new value;
    /// the C reference merges the triggering commit's attrs the same
    /// way in `lfs_dir_compact`).
    attr_append: Option<(u8, &'a [u8])>,
}

/// A STRUCT body override: either caller-provided bytes (`Update`'s
/// inline content) or two little-endian words materialized at emission
/// (`UpdateCtz`'s `(head, size)`, `UpdateDirStruct`'s pair address).
#[derive(Clone, Copy)]
enum OverrideBody<'a> {
    Slice(&'a [u8]),
    Words(u32, u32),
}

impl SlotPlan<'static> {
    const NONE: Self = Self {
        drop_entry: false,
        extra_name: None,
        struct_override: None,
        attr_suppress: None,
        attr_append: None,
    };
}

/// Derive `op`'s effect on the emitted slot at combined index `i`.
/// Exhaustive over [`WriteOp`] with no wildcard arm by design; see
/// [`SlotPlan`].
fn slot_plan<'a>(op: &WriteOp<'a>, i: u16) -> SlotPlan<'a> {
    use crate::tag::TagType;
    match *op {
        WriteOp::Update { id, content } if id == i => SlotPlan {
            struct_override: Some((TagType::InlineStruct, OverrideBody::Slice(content))),
            ..SlotPlan::NONE
        },
        WriteOp::UpdateCtz { id, head_block, total_size } if id == i => SlotPlan {
            struct_override: Some((
                TagType::CtzStruct,
                OverrideBody::Words(head_block, total_size),
            )),
            ..SlotPlan::NONE
        },
        WriteOp::UpdateDirStruct { id, new_pair } if id == i => SlotPlan {
            struct_override: Some((
                TagType::DirStruct,
                OverrideBody::Words(new_pair.a.as_u32(), new_pair.b.as_u32()),
            )),
            ..SlotPlan::NONE
        },
        WriteOp::Remove { id } if id == i => SlotPlan { drop_entry: true, ..SlotPlan::NONE },
        WriteOp::RenameInPlace { id, name_type, new_name } if id == i => {
            SlotPlan { extra_name: Some((name_type, new_name)), ..SlotPlan::NONE }
        }
        WriteOp::SetAttr { id, attr_id, value } if id == i => SlotPlan {
            attr_suppress: Some(attr_id),
            attr_append: Some((attr_id, value)),
            ..SlotPlan::NONE
        },
        WriteOp::RemoveAttr { id, attr_id } if id == i => {
            // The rebuilt block simply omits the attr; no delete
            // marker is needed because there is no older tag left to
            // shadow.
            SlotPlan { attr_suppress: Some(attr_id), ..SlotPlan::NONE }
        }
        // Create-family ops and Noop touch no existing slot; the
        // guarded arms above fall through here when `i` is not their
        // target. Listed exhaustively so a new variant cannot compile
        // without declaring its compaction effect.
        WriteOp::Create { .. }
        | WriteOp::CreateCtz { .. }
        | WriteOp::CreateDir { .. }
        | WriteOp::Noop
        | WriteOp::Update { .. }
        | WriteOp::UpdateCtz { .. }
        | WriteOp::UpdateDirStruct { .. }
        | WriteOp::Remove { .. }
        | WriteOp::RenameInPlace { .. }
        | WriteOp::SetAttr { .. }
        | WriteOp::RemoveAttr { .. } => SlotPlan::NONE,
    }
}

/// Where [`emit_compact_range`] sends its tags: a real [`crate::meta::Commit`]
/// (the compaction write path) or a byte counter (the split-point size
/// estimate). One emission function feeding both is what guarantees
/// the estimate and the writer cannot drift (review D3); they were
/// previously parallel constructions required to agree byte-for-byte
/// by comment alone.
enum TagSink<'x, 'b> {
    Commit(&'x mut crate::meta::Commit<'b>),
    Count(&'x mut usize),
}

impl TagSink<'_, '_> {
    fn tag(&mut self, tag: crate::tag::Tag, body: &[u8]) -> Result<(), Error> {
        match self {
            TagSink::Commit(c) => c.tag(tag, body),
            TagSink::Count(n) => {
                **n += tag.dsize();
                Ok(())
            }
        }
    }
}

/// Emit the combined-sequence sub-range `[lo, hi)` of a compaction
/// rebuild: the live slots `slots[lo..min(hi, count)]` followed by the
/// op's virtual new entry at combined index `count` when it falls in
/// range. Per entry: a `Create` tag, the NAME, the STRUCT (with `op`'s
/// substitutions applied), and the entry's live user attributes
/// replayed from the source log (review C1; the C reference's
/// `lfs_dir_compact` replays all unique tags per live id, attributes
/// included). `emit_id` restarts at 0 for every range, so a split
/// continuation's entries get local ids 0.. (the reader concatenates
/// them across the HardTail chain).
///
/// `source_buf` is the full source (active) block; the attr replay
/// re-parses it, which is cheap relative to the compaction itself.
fn emit_compact_range(
    sink: &mut TagSink<'_, '_>,
    source_buf: &[u8],
    slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
    count: usize,
    op: &WriteOp<'_>,
    lo: usize,
    hi: usize,
) -> Result<(), Error> {
    use crate::tag::{Tag, TagType};

    let reader = crate::meta::MetadataReader::new(source_buf)?;
    let mut emit_id: u16 = 0;
    for (i, s) in slots.iter().enumerate().take(hi.min(count)).skip(lo) {
        let plan = slot_plan(op, i as u16);
        if plan.drop_entry {
            continue; // drop this entry; do not bump emit_id
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
        sink.tag(Tag::new(true, TagType::Create, emit_id, 0), &[])?;
        sink.tag(Tag::new(true, name_type, emit_id, s.name_len), name)?;
        if let Some((nt, new_name)) = plan.extra_name {
            // RenameInPlace: a second NAME at the same id; the newer
            // one shadows at read time.
            sink.tag(Tag::new(true, nt, emit_id, new_name.len() as u16), new_name)?;
        }
        match plan.struct_override {
            Some((st, OverrideBody::Slice(body))) => {
                sink.tag(Tag::new(true, st, emit_id, body.len() as u16), body)?;
            }
            Some((st, OverrideBody::Words(w0, w1))) => {
                let mut body = [0u8; 8];
                body[0..4].copy_from_slice(&w0.to_le_bytes());
                body[4..8].copy_from_slice(&w1.to_le_bytes());
                sink.tag(Tag::new(true, st, emit_id, 8), &body)?;
            }
            None => {
                let struct_body = &source_buf
                    [s.struct_off as usize..s.struct_off as usize + s.struct_len as usize];
                sink.tag(Tag::new(true, struct_type, emit_id, s.struct_len), struct_body)?;
            }
        }
        // Replay the entry's live attributes (latest value per attr
        // id, splice-correct, delete markers consume their id), minus
        // the one the in-flight op supersedes.
        crate::dir::for_each_live_attr(&reader, i as u16, |a, body| {
            if plan.attr_suppress == Some(a) {
                return Ok(());
            }
            sink.tag(Tag::new(true, TagType::UserAttr(a), emit_id, body.len() as u16), body)
        })?;
        if let Some((a, v)) = plan.attr_append {
            sink.tag(Tag::new(true, TagType::UserAttr(a), emit_id, v.len() as u16), v)?;
        }
        emit_id += 1;
    }

    // Append the new entry, but only when its combined index `count`
    // falls in `[lo, hi)`. The new entry has the highest index, so a
    // split always places it in the upper-half continuation; the lower
    // half emits no new entry. Its local id is `emit_id` (the entries
    // emitted in this range), which equals the op's `id` for the
    // single-pair `[0, total)` case the debug-asserts check. The match
    // is exhaustive with no wildcard (review D4).
    if lo <= count && count < hi {
        match *op {
            WriteOp::Create { id, name, content, moved_attrs } => {
                debug_assert!(
                    lo > 0 || id == emit_id,
                    "single-pair Create id must equal emit count"
                );
                sink.tag(Tag::new(true, TagType::Create, emit_id, 0), &[])?;
                sink.tag(Tag::new(true, TagType::RegularFile, emit_id, name.len() as u16), name)?;
                sink.tag(
                    Tag::new(true, TagType::InlineStruct, emit_id, content.len() as u16),
                    content,
                )?;
                for (a, v) in moved_attrs.iter() {
                    sink.tag(Tag::new(true, TagType::UserAttr(a), emit_id, v.len() as u16), v)?;
                }
            }
            WriteOp::CreateCtz { id, name, head_block, total_size, moved_attrs } => {
                debug_assert!(
                    lo > 0 || id == emit_id,
                    "single-pair CreateCtz id must equal emit count"
                );
                sink.tag(Tag::new(true, TagType::Create, emit_id, 0), &[])?;
                sink.tag(Tag::new(true, TagType::RegularFile, emit_id, name.len() as u16), name)?;
                let mut body = [0u8; 8];
                body[0..4].copy_from_slice(&head_block.to_le_bytes());
                body[4..8].copy_from_slice(&total_size.to_le_bytes());
                sink.tag(Tag::new(true, TagType::CtzStruct, emit_id, 8), &body)?;
                for (a, v) in moved_attrs.iter() {
                    sink.tag(Tag::new(true, TagType::UserAttr(a), emit_id, v.len() as u16), v)?;
                }
            }
            WriteOp::CreateDir { id, name, dir_pair, moved_attrs } => {
                debug_assert!(
                    lo > 0 || id == emit_id,
                    "single-pair CreateDir id must equal emit count"
                );
                sink.tag(Tag::new(true, TagType::Create, emit_id, 0), &[])?;
                sink.tag(Tag::new(true, TagType::Directory, emit_id, name.len() as u16), name)?;
                let mut body = [0u8; 8];
                body[0..4].copy_from_slice(&dir_pair.a.as_u32().to_le_bytes());
                body[4..8].copy_from_slice(&dir_pair.b.as_u32().to_le_bytes());
                sink.tag(Tag::new(true, TagType::DirStruct, emit_id, 8), &body)?;
                for (a, v) in moved_attrs.iter() {
                    sink.tag(Tag::new(true, TagType::UserAttr(a), emit_id, v.len() as u16), v)?;
                }
            }
            WriteOp::Update { .. }
            | WriteOp::UpdateCtz { .. }
            | WriteOp::UpdateDirStruct { .. }
            | WriteOp::Remove { .. }
            | WriteOp::RenameInPlace { .. }
            | WriteOp::SetAttr { .. }
            | WriteOp::RemoveAttr { .. }
            | WriteOp::Noop => {}
        }
    }
    Ok(())
}

/// Build a compacted commit on `alt_buf`: replay every live entry from
/// `slots[..count]` (reading source bytes from `source_buf`, user
/// attributes included) and apply `op` (creating a new entry at the
/// end, replacing an existing entry's struct body, or merging an
/// in-flight attr change). Returns the total bytes written.
/// `alt_buf` is pre-filled with `0xFF` (erased state).
#[allow(clippy::too_many_arguments)]
fn build_compact_commit(
    alt_buf: &mut [u8],
    source_buf: &[u8],
    new_revision: u32,
    slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
    count: usize,
    op: &WriteOp<'_>,
    lo: usize,
    hi: usize,
    prog_size: usize,
    block_size: usize,
    move_state: Option<[u8; crate::gstate::MOVE_STATE_BODY_SIZE]>,
    relocate_state: Option<[u8; crate::gstate::RELOCATE_STATE_BODY_SIZE]>,
    tail: Option<(BlockPair, bool)>,
) -> Result<usize, Error> {
    use crate::tag::TagType;

    for b in alt_buf.iter_mut() {
        *b = 0xFF;
    }
    let mut commit = crate::meta::Commit::new(alt_buf, new_revision)?;

    // The live-entry emission (entries, structs, attrs, the op's new
    // entry) is shared with the split-point size estimate; see
    // `emit_compact_range` (review D3).
    emit_compact_range(&mut TagSink::Commit(&mut commit), source_buf, slots, count, op, lo, hi)?;

    // If the caller passed a MoveState body (typically the pair's
    // pre-compaction accumulated gstate XOR any new contribution),
    // emit it as a single tag so the compacted block carries the
    // pair's net gstate contribution. Skip the all-zero body: an
    // empty contribution is the default and the compactor should
    // not waste bytes on it.
    if let Some(body) = move_state {
        if body != [0u8; crate::gstate::MOVE_STATE_BODY_SIZE] {
            commit.tag(
                crate::tag::Tag::new(
                    true,
                    TagType::MoveState,
                    crate::tag::ID_NONE,
                    crate::gstate::MOVE_STATE_BODY_SIZE as u16,
                ),
                &body,
            )?;
        }
    }
    if let Some(body) = relocate_state {
        if body != [0u8; crate::gstate::RELOCATE_STATE_BODY_SIZE] {
            commit.tag(
                crate::tag::Tag::new(
                    true,
                    TagType::RelocateState,
                    crate::tag::ID_NONE,
                    crate::gstate::RELOCATE_STATE_BODY_SIZE as u16,
                ),
                &body,
            )?;
        }
    }
    // Re-emit the pair's tail tag. Compaction rebuilds the block from
    // live entries, so without this the metadata-pair global thread link
    // (SoftTail = next directory in the filesystem list, HardTail = this
    // directory's continuation) would be silently dropped. `None` means
    // the pair has no tail (the common case until threading lands).
    // A NONE-sentinel pair means "no tail" (the thread end after an
    // un-thread); emit nothing so the rebuilt block drops the link.
    if let Some((pair, is_hard)) = tail {
        if !pair.a.is_none() {
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&pair.a.as_u32().to_le_bytes());
            body[4..8].copy_from_slice(&pair.b.as_u32().to_le_bytes());
            let tag_type = if is_hard { TagType::HardTail } else { TagType::SoftTail };
            commit.tag(crate::tag::Tag::new(true, tag_type, crate::tag::ID_NONE, 8), &body)?;
        }
    }
    commit.finish_padded(0, prog_size, block_size)?;
    Ok(commit.bytes_written())
}

/// Wire-level commit size for a [`WriteOp`], including the trailing
/// Walk the metadata-pair forest from `root` and accumulate every
/// committed `MoveState` body into a single [`crate::gstate::Gstate`].
/// Used by mount-time atomic-move-state recovery: if the resulting
/// gstate is non-zero, a cross-directory rename was crashed between
/// its two commits and the source `Delete` must still be emitted.
///
/// Bounded at [`crate::alloc::MAX_QUEUED_PAIRS`] visited pairs to
/// match the allocator's BFS budget; deeper directory trees return
/// `Error::OutOfRange`.
fn accumulate_gstate<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<crate::gstate::Gstate, Error> {
    let mut queue: [BlockPair; crate::alloc::MAX_QUEUED_PAIRS] =
        [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE); crate::alloc::MAX_QUEUED_PAIRS];
    queue[0] = root;
    let mut tail = 1usize;
    let mut head = 0usize;
    let mut gstate = crate::gstate::Gstate::ZERO;

    while head < tail {
        let pair_addr = queue[head];
        head += 1;

        storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;

        gstate.xor_body(&scan_pair_move_state(&pair));
        gstate.xor_relocate_body(&scan_pair_relocate_state(&pair));

        // Walk live entries (splice-correct, latest-tag-wins) so we
        // only enqueue the AUTHORITATIVE child pair for each id, not
        // every superseded `UpdateDirStruct` tag in the log. Visiting
        // stale historical pairs would read garbage (their blocks
        // have been reused for other purposes) and pollute the
        // gstate accumulation.
        let active_is_a = pair.active_block == pair_addr.a;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        let source_buf: &[u8] = if active_is_a { &*buf_a } else { &*buf_b };
        // Tail tags (HardTail, SoftTail) follow the pair's own thread
        // and don't multiplex into the live-entries view; enqueue the
        // latest tail just like the read-side resolver does.
        if let Some(tail_pair) = pair.reader.tail() {
            // A live tail pointing outside the device is genuine
            // corruption; proceeding would yield an incomplete gstate
            // and silently mis-recover.
            if !pair_in_bounds::<S>(tail_pair) {
                return Err(Error::Corrupt);
            }
            if !queue[..tail].contains(&tail_pair) {
                if tail >= crate::alloc::MAX_QUEUED_PAIRS {
                    return Err(Error::OutOfRange);
                }
                queue[tail] = tail_pair;
                tail += 1;
            }
        }
        for slot in slots.iter().take(count) {
            // struct_kind 2 == DirStruct (per gather_live_slots's
            // encoding); body is 8 bytes encoding the child pair.
            if slot.struct_kind == 2 && slot.struct_len == 8 {
                let start = slot.struct_off as usize;
                let body = &source_buf[start..start + 8];
                let a = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let b = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let child = BlockPair::new(BlockAddress::new(a), BlockAddress::new(b));
                // A live DirStruct pointing outside the device is
                // genuine corruption; reject rather than read garbage
                // and mis-accumulate the recovery gstate.
                if !pair_in_bounds::<S>(child) {
                    return Err(Error::Corrupt);
                }
                if !queue[..tail].contains(&child) {
                    if tail >= crate::alloc::MAX_QUEUED_PAIRS {
                        return Err(Error::OutOfRange);
                    }
                    queue[tail] = child;
                    tail += 1;
                }
            }
        }
    }

    Ok(gstate)
}

/// BFS the directory tree from `root`, following only live
/// (splice-correct) `DirStruct` children and `HardTail` continuations,
/// and collect every reachable metadata pair into `out`. Returns the
/// count.
///
/// This is the *tree*, deliberately distinct from the global metadata
/// thread: `SoftTail` links (the global list) are NOT followed, so a pair
/// reachable only via the thread (a crash-orphaned half-removed directory)
/// is absent from the result. The mount-time deorphan sweep uses that
/// distinction. `HardTail` continuations ARE part of the tree (the same
/// directory's entries continue there), so they are followed; the live
/// `DirStruct` view (not raw `iter_tags`) ensures a directory whose entry
/// has already been deleted is not counted as live.
fn collect_live_tree_pairs<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    out: &mut [BlockPair; crate::alloc::MAX_QUEUED_PAIRS],
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<usize, Error> {
    out[0] = root;
    let mut tail = 1usize;
    let mut head = 0usize;
    while head < tail {
        let pair_addr = out[head];
        head += 1;
        storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
        let active_is_a = pair.active_block == pair_addr.a;
        // A HardTail continues this directory's own pair chain (part of
        // the tree); a SoftTail is the global list (not the tree).
        if pair.reader.is_hard_tail() {
            if let Some(cont) = pair.reader.tail() {
                if pair_in_bounds::<S>(cont) && !out[..tail].contains(&cont) {
                    if tail >= crate::alloc::MAX_QUEUED_PAIRS {
                        return Err(Error::OutOfRange);
                    }
                    out[tail] = cont;
                    tail += 1;
                }
            }
        }
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        let source_buf: &[u8] = if active_is_a { &*buf_a } else { &*buf_b };
        for slot in slots.iter().take(count) {
            if slot.struct_kind == 2 && slot.struct_len == 8 {
                let start = slot.struct_off as usize;
                let body = &source_buf[start..start + 8];
                let a = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let b = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let child = BlockPair::new(BlockAddress::new(a), BlockAddress::new(b));
                if !pair_in_bounds::<S>(child) {
                    return Err(Error::Corrupt);
                }
                if !out[..tail].contains(&child) {
                    if tail >= crate::alloc::MAX_QUEUED_PAIRS {
                        return Err(Error::OutOfRange);
                    }
                    out[tail] = child;
                    tail += 1;
                }
            }
        }
    }
    Ok(tail)
}

/// Find the live tree pair that is the relocated TWIN of `stale`, if
/// any: the pair the filesystem now reaches where `stale` used to be.
///
/// Both relocation flavors replace exactly one block of a pair and
/// keep the other (wear levelling keeps the old active block and
/// replaces the alternate with the fresh; the failure-driven path
/// keeps the good block and replaces the worn victim), so the twin
/// shares exactly one block with `stale`. Sharing alone is not
/// sufficient: a block `stale` no longer needs (its anchor, freed by
/// the relocation) can be reallocated as the FRESH half of a later
/// relocation in the same cascade. The disambiguator is which half
/// the shared block is *in the candidate*: a relocation's kept block
/// carries the pre-relocation revision while the fresh block carries
/// the new commit, so in the true twin the shared block is the
/// INACTIVE half; in a reuser the recycled block is the fresh,
/// ACTIVE half. Block ownership is otherwise exclusive, so at most
/// one tree pair passes the test.
///
/// Used by the mount-recovery paths whose durable coordinates can be
/// outdated by a relocation cascade (reviews C6 and H4); within those
/// windows only cascade commits run, so no other pair shapes (e.g. a
/// fresh mkdir pair with an erased half) can alias the rule.
fn relocated_twin_in<S: Storage>(
    storage: &mut S,
    tree: &[BlockPair],
    stale: BlockPair,
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<Option<BlockPair>, Error> {
    for &t in tree {
        if t == stale {
            continue;
        }
        let shared = if t.a == stale.a || t.a == stale.b {
            t.a
        } else if t.b == stale.a || t.b == stale.b {
            t.b
        } else {
            continue;
        };
        storage.read(t.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        storage.read(t.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let p = MetadataPair::parse(t.a, &*buf_a, t.b, &*buf_b)?;
        if p.active_block != shared {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

/// Read a metadata pair's `MoveState` contribution: the body of the
/// *latest* committed `MoveState` tag, or all-zero when none exists.
///
/// Latest-tag-wins is the C reference's semantics (review C4): a
/// reader takes the single newest matching tag (`lfs_dir_getgstate`
/// over `lfs_dir_getslice`), and every writer folds the pair's
/// existing contribution into each tag it commits, so each committed
/// tag is the pair's new TOTAL contribution, not a delta. A valid
/// C-written log can hold several MoveState tags (two moves into the
/// same directory with no intervening compaction); XOR-accumulating
/// them, as this function did before the C4 fix, decodes a phantom
/// move and mount recovery destroys a live entry. The write-side half
/// of the convention lives in `apply_op_to_pair_inner` (append path
/// folds `pair_existing` into the emitted tag) and
/// `build_compact_commit` (the compacted block re-emits the net total
/// as its single tag). An explicit all-zero body is a real
/// contribution ("returned to zero") and must win over earlier
/// non-zero tags, which the newest-first scan gives for free.
fn scan_pair_move_state(pair: &MetadataPair<'_>) -> [u8; crate::gstate::MOVE_STATE_BODY_SIZE] {
    let mut out = [0u8; crate::gstate::MOVE_STATE_BODY_SIZE];
    for entry in pair.reader.iter_tags_rev() {
        if entry.tag.tag_type() == crate::tag::TagType::MoveState
            && entry.body.len() == crate::gstate::MOVE_STATE_BODY_SIZE
        {
            out.copy_from_slice(entry.body);
            break;
        }
    }
    out
}

/// Read a metadata pair's `RelocateState` contribution: the body of
/// the latest committed `RelocateState` tag, or all-zero when none
/// exists. Same latest-total-wins convention as
/// [`scan_pair_move_state`]; see there for the derivation (review C4).
fn scan_pair_relocate_state(
    pair: &MetadataPair<'_>,
) -> [u8; crate::gstate::RELOCATE_STATE_BODY_SIZE] {
    let mut out = [0u8; crate::gstate::RELOCATE_STATE_BODY_SIZE];
    for entry in pair.reader.iter_tags_rev() {
        if entry.tag.tag_type() == crate::tag::TagType::RelocateState
            && entry.body.len() == crate::gstate::RELOCATE_STATE_BODY_SIZE
        {
            out.copy_from_slice(entry.body);
            break;
        }
    }
    out
}

/// CCRC tag (8 bytes). Used by callers to decide whether to append in
/// place or compact onto the alternate.
fn op_dsize_of(op: &WriteOp<'_>) -> usize {
    match *op {
        WriteOp::Update { content, .. } => (4 + content.len()) + 8,
        WriteOp::UpdateCtz { .. } | WriteOp::UpdateDirStruct { .. } => (4 + 8) + 8,
        WriteOp::Create { name, content, moved_attrs, .. } => {
            4 + (4 + name.len()) + (4 + content.len()) + moved_attrs.dsize() + 8
        }
        WriteOp::CreateCtz { name, moved_attrs, .. }
        | WriteOp::CreateDir { name, moved_attrs, .. } => {
            4 + (4 + name.len()) + (4 + 8) + moved_attrs.dsize() + 8
        }
        WriteOp::Remove { .. } | WriteOp::RemoveAttr { .. } => 4 + 8,
        WriteOp::RenameInPlace { new_name, .. } => (4 + new_name.len()) + 8,
        WriteOp::SetAttr { value, .. } => (4 + value.len()) + 8,
        WriteOp::Noop => 8,
    }
}

/// Re-read a just-programmed region and CRC-compare it against the
/// bytes that were sent, the C reference's post-commit validation
/// (`lfs_dir_commitcrc` re-reads every commit through `lfs_bd_crc`;
/// review H2). Without it, a program that silently corrupted cells
/// reports durable success and the corruption surfaces only at the
/// next mount of that pair.
///
/// `region` doubles as the read-back destination (the kernel has no
/// third block buffer): on a match the buffer holds the on-disk bytes,
/// which equal what it held before the call; on a mismatch the caller
/// must treat the buffer as clobbered and fall into its worn-block
/// path, every one of which rebuilds the buffer before reuse.
///
/// Alignment: every caller verifies a region it just programmed, so
/// `off`/`len` are `PROG_SIZE`-aligned, and `BLOCK_SIZE` being a
/// multiple of `PROG_SIZE` (itself at least `READ_SIZE`) makes the
/// read legal under the [`Storage`] geometry contract.
fn verify_programmed<S: Storage>(
    storage: &mut S,
    block: BlockAddress,
    off: usize,
    region: &mut [u8],
) -> bool {
    let expected = crate::crc::update(crate::crc::INIT, region);
    storage.read(block.as_u32(), off as u32, region).is_ok()
        && crate::crc::update(crate::crc::INIT, region) == expected
}

/// True when `op` adds a new entry, so the combined entry sequence has
/// one more element (at index `count`) than the live entry count.
/// Create-family ops append an entry; every other op transforms or
/// removes an existing one.
fn op_adds_entry(op: &WriteOp<'_>) -> bool {
    matches!(op, WriteOp::Create { .. } | WriteOp::CreateCtz { .. } | WriteOp::CreateDir { .. })
}

/// Bytes a compacting split reserves in each pair beyond the live
/// entries: the tail tag (`4 + 2*4 = 12`), the gstate tags
/// (`4 + 3*4 = 16`), a move-delete tag (`4`), and the CCRC (`4 + 4 = 8`),
/// totalling 40. Matches the C reference's `lfs_dir_splittingcompact`
/// (lfs.c, "space is complicated").
const SPLIT_RESERVE: usize = 40;

/// Wire size of the live entries in the combined-sequence range
/// `[lo, hi)` exactly as [`build_compact_commit`] would emit them
/// (user attributes included), plus the virtual new entry (combined
/// index `count`) when it falls in the range for a Create-family op.
/// Excludes the revision header, gstate, tail, and CCRC — those are
/// covered by [`SPLIT_RESERVE`].
///
/// The estimate and the emitter are the same function over different
/// [`TagSink`]s (review D3), so they cannot drift.
fn compact_range_size(
    source_buf: &[u8],
    slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
    count: usize,
    op: &WriteOp<'_>,
    lo: usize,
    hi: usize,
) -> Result<usize, Error> {
    let mut size = 0usize;
    emit_compact_range(&mut TagSink::Count(&mut size), source_buf, slots, count, op, lo, hi)?;
    Ok(size)
}

/// Pick the split index over the combined-sequence range `[begin, end)`,
/// mirroring the C reference's `lfs_dir_splittingcompact` inner loop: the
/// upper portion `[split, end)` is shrunk (by increasing `split`) until it
/// fits the per-pair budget — capped at half a block (prog-aligned) to
/// avoid degenerate nearly-full pairs, and reserving [`SPLIT_RESERVE`]
/// bytes for the tail, gstate, move-delete, and CCRC.
///
/// This is a monotone shrink, not a binary search: `split` only
/// increases, bounding the loop to ~log2 iterations and matching the
/// oracle's metadata distribution (which matters for byte-equal
/// conformance once split directories are emitted). The `< 0xff` guard is
/// the oracle's id-fits-in-a-byte cap on entries per pair.
///
/// Returns `begin` when the whole range already fits (no split needed);
/// otherwise the index where the upper portion `[split, end)` moves to a
/// continuation pair and the lower portion `[begin, split)` stays.
fn compute_split_index<S: Storage>(
    source_buf: &[u8],
    slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
    count: usize,
    op: &WriteOp<'_>,
    begin: usize,
    end: usize,
) -> Result<usize, Error> {
    let budget = core::cmp::min(
        S::BLOCK_SIZE.saturating_sub(SPLIT_RESERVE),
        (S::BLOCK_SIZE / 2).next_multiple_of(S::PROG_SIZE),
    );
    let mut split = begin;
    while end - split > 1 {
        let size = compact_range_size(source_buf, slots, count, op, split, end)?;
        if (end - split) < 0xff && size <= budget {
            break;
        }
        split += (end - split) / 2;
    }
    Ok(split)
}

/// `true` if every byte of `buf` is `0xFF`. This is the erased state
/// for NOR / NAND flash; both blocks of the root metadata pair in
/// this condition means the device has never been formatted (versus
/// "formatted but corrupted", where at least one bit has been
/// programmed somewhere).
fn is_all_erased(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0xFF)
}

/// Wear-levelling predicate. Returns `true` when a pair's about-to-be
/// programmed `new_revision` lands on the `BLOCK_CYCLES` boundary, so
/// the compact should re-target a freshly allocated block instead of
/// the in-pair alternate. The root pair is excluded: it lives at
/// fixed addresses `(0, 1)` (the spec's first-block readability
/// requirement) and has no parent to update.
///
/// The modulus matches the C reference: `((block_cycles + 1) | 1)`
/// avoids two corner cases noted upstream — `block_cycles == 1` would
/// prevent relocations from terminating, and any even
/// `block_cycles == 2n` would alias to relocating only one block of
/// the pair, defeating wear distribution.
///
/// `BLOCK_CYCLES <= 0` (or the literal default of `-1` documented on
/// the `Storage` trait) disables wear levelling entirely.
fn should_relocate(
    pair_addr: BlockPair,
    root: BlockPair,
    new_revision: u32,
    block_cycles: i32,
) -> bool {
    if pair_addr == root {
        return false;
    }
    if block_cycles <= 0 {
        return false;
    }
    // (block_cycles + 1) | 1, computed safely against the i32-to-u32
    // narrowing. block_cycles is positive here.
    let m = ((block_cycles as u32).wrapping_add(1)) | 1;
    new_revision % m == 0
}

/// True when both blocks of a metadata-pair address decoded from disk
/// fall inside the device geometry. A `DirStruct` / tail body in a
/// corrupt or adversarial image can encode any 32-bit value; the
/// kernel must validate before handing the address to
/// [`Storage::read`] (whose contract requires it to error on
/// out-of-range, but which the kernel does not depend on for its own
/// correctness). Out-of-range pair addresses are never legitimately
/// reachable, so callers skip or reject them.
#[inline]
fn pair_in_bounds<S: Storage>(pair: BlockPair) -> bool {
    pair.a.as_u32() < S::BLOCK_COUNT && pair.b.as_u32() < S::BLOCK_COUNT
}

/// Walk the global metadata-pair thread from `root` (following each
/// pair's tail) and return the pair whose tail references `target`, with
/// whether that tail is a HardTail. Returns `Ok(None)` if `target` is the
/// root or no pair's tail references it.
///
/// This is the thread-predecessor lookup the C reference calls
/// `lfs_fs_pred`. It is needed when a threaded pair changes address
/// (wear-levelling relocation) or is removed: the predecessor's tail must
/// be re-pointed so the global list stays consistent for the C
/// reference's allocator and traverse. Bounded and cycle-safe at
/// [`crate::alloc::MAX_QUEUED_PAIRS`] hops.
fn find_thread_predecessor<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    target: BlockPair,
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<Option<(BlockPair, bool)>, Error> {
    let mut cur = root;
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps > crate::alloc::MAX_QUEUED_PAIRS {
            return Err(Error::OutOfRange);
        }
        storage.read(cur.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        storage.read(cur.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let (tail, is_hard) = {
            let p = MetadataPair::parse(cur.a, &*buf_a, cur.b, &*buf_b)?;
            (p.reader.tail(), p.reader.is_hard_tail())
        };
        match tail {
            Some(t) if t == target => return Ok(Some((cur, is_hard))),
            Some(t) => cur = t,
            None => return Ok(None),
        }
    }
}

/// BFS-walk the metadata-pair forest from `root` and return the
/// `(parent_pair, id)` of the entry whose `DirStruct` body matches
/// `target`. Returns `Ok(None)` if `target` is not referenced by any
/// reachable `DirStruct` tag.
///
/// Used by the wear-levelling relocation chain to find the parent
/// whose `DirStruct` entry must be flipped to the new pair address
/// after a child pair migrates to fresh blocks.
///
/// Only `DirStruct` references are matched: this finds the *tree*
/// parent whose entry must be flipped to the new address. The relocated
/// pair's *thread* predecessor (whose tail also references it) is found
/// separately by [`find_thread_predecessor`] and updated by
/// `propagate_relocation`. `HardTail` continuations are walked and ARE
/// DirStruct candidates (a split directory's child entries live there).
///
/// # Live state only (reviews C5 and C6)
///
/// The walk consumes exclusively LIVE state: children are enqueued
/// from each pair's splice-corrected latest-wins `DirStruct` bodies
/// (via `gather_live_slots`), the tail from the reader's latest tail
/// tag, and the match runs over the live bodies, so the returned id is
/// a live id by construction (review C5) and the returned pair is the
/// live parent. Iterating raw tags here was a confirmed corruption
/// source twice over: a raw match returned write-time ids that
/// relocation consumed as live ids (C5), and raw enqueueing followed
/// superseded `DirStruct` bodies into STALE pre-relocation pairs whose
/// old logs can still match the target, committing the parent repoint
/// onto a dead pair (erasing freed or reallocated blocks) while the
/// real parent kept the outdated reference (found while reproducing
/// C6). `accumulate_gstate` and `collect_live_tree_pairs` follow the
/// same authoritative-children rule for the same reason. The C
/// reference's `lfs_fs_parent` only ever fetches live pairs (it walks
/// the thread) and matches through `lfs_dir_fetchmatch`'s
/// splice-corrected view.
///
/// Bounded at [`crate::alloc::MAX_QUEUED_PAIRS`] visited pairs, the
/// same budget as the allocator's BFS. Deeper trees return
/// [`Error::OutOfRange`].
fn find_parent_in_tree<S: Storage>(
    storage: &mut S,
    root: BlockPair,
    target: BlockPair,
    buf_a: &mut [u8],
    buf_b: &mut [u8],
) -> Result<Option<(BlockPair, u16)>, Error> {
    if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
        return Err(Error::GeometryMismatch);
    }
    let mut queue: [BlockPair; crate::alloc::MAX_QUEUED_PAIRS] =
        [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE); crate::alloc::MAX_QUEUED_PAIRS];
    queue[0] = root;
    let mut tail = 1usize;
    let mut head = 0usize;

    while head < tail {
        let pair_addr = queue[head];
        head += 1;

        storage.read(pair_addr.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        storage.read(pair_addr.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
        let active_is_a = pair.active_block == pair_addr.a;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        let source_buf: &[u8] = if active_is_a { &*buf_a } else { &*buf_b };

        // Latest tail only; every raw tail tag would re-enqueue
        // superseded thread links.
        if let Some(t) = pair.reader.tail() {
            if pair_in_bounds::<S>(t) && !queue[..tail].contains(&t) {
                if tail >= crate::alloc::MAX_QUEUED_PAIRS {
                    return Err(Error::OutOfRange);
                }
                queue[tail] = t;
                tail += 1;
            }
        }

        for (i, slot) in slots.iter().enumerate().take(count) {
            // struct_kind 2 == DirStruct (gather_live_slots encoding).
            if slot.struct_kind != 2 || slot.struct_len != 8 {
                continue;
            }
            let start = slot.struct_off as usize;
            let body = &source_buf[start..start + 8];
            let a = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let b = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let child = BlockPair::new(BlockAddress::new(a), BlockAddress::new(b));
            if child == target {
                // `i` is the entry's live id by construction.
                return Ok(Some((pair_addr, i as u16)));
            }
            // An out-of-range DirStruct body cannot be the target (a
            // real allocated pair) and must never be dereferenced;
            // skip it rather than enqueue.
            if pair_in_bounds::<S>(child) && !queue[..tail].contains(&child) {
                if tail >= crate::alloc::MAX_QUEUED_PAIRS {
                    return Err(Error::OutOfRange);
                }
                queue[tail] = child;
                tail += 1;
            }
        }
    }

    Ok(None)
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
/// decoded superblock state.
#[derive(Debug)]
pub struct Fs<S: Storage> {
    storage: S,
    superblock: Superblock,
    root: BlockPair,
    /// Lookahead cache of in-use blocks (over-approximation) so
    /// steady-state allocation serves from RAM instead of re-walking the
    /// reachable forest on every block-allocating write. `None` forces a
    /// fresh authoritative scan on the next allocation; freed-block churn
    /// invalidates it for promptness, but correctness never depends on
    /// invalidation (see `crate::alloc::alloc_blocks_cached`).
    used_cache: Option<crate::alloc::Bitmap>,
    /// In-flight cross-directory rename coordinates (review C6): set
    /// before the destination commit, consumed by the source commit.
    /// The destination commit can trigger a relocation cascade that
    /// relocates the SOURCE pair before the source commit runs;
    /// [`Fs::propagate_relocation`] remaps these coordinates so the
    /// source delete targets the live address. The C reference patches
    /// its in-RAM gstate at the same sites: "this looks like an
    /// optimization but is in fact _required_ since relocating may
    /// outdate the move" (lfs.c:2484, 2536). `None` outside the rename
    /// window.
    pending_move: Option<PendingMove>,
}

/// See [`Fs::pending_move`]. `delta` is the original MoveState body
/// committed with the destination create; the balancing source commit
/// must fold the SAME delta (the bodies cancel by XOR regardless of
/// which address the source pair lives at by then), while `cur_pair`
/// tracks the source pair's current address through relocations.
#[derive(Clone, Copy, Debug)]
struct PendingMove {
    cur_pair: BlockPair,
    cur_id: u16,
    delta: [u8; crate::gstate::MOVE_STATE_BODY_SIZE],
}

impl<S: Storage> Fs<S> {
    /// Drop the block-allocator lookahead cache, forcing the next
    /// allocation to rescan the reachable forest from disk. Called after
    /// an operation that frees blocks so they are reclaimed promptly
    /// rather than lingering as over-marked in the cache. Never required
    /// for correctness (a stale cache is only an over-approximation; see
    /// [`crate::alloc::alloc_blocks_cached`]); purely a promptness hook.
    pub(crate) fn invalidate_alloc_cache(&mut self) {
        self.used_cache = None;
    }

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
    /// Updates flip layouts transparently: an existing inline entry
    /// promoted past `INLINE_MAX` is rewritten as CTZ (the old inline
    /// body is replaced); an existing CTZ entry shrunk below
    /// `INLINE_MAX` is rewritten inline (the old chain becomes
    /// unreachable and is reclaimed by the next allocator scan).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if `name` is empty or longer than
    /// [`crate::NAME_MAX`] bytes; a longer name would be unreachable or
    /// wrongly resolved under the C reference.
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
    /// If an entry of the same name already exists as a regular file,
    /// this emits an `UpdateCtz` against the existing id; the old
    /// chain (inline body or prior CTZ chain) becomes unreachable and
    /// is reclaimed by the next allocator scan. Rejects with
    /// [`Error::AlreadyExists`] if the existing entry is a directory
    /// (use `rmdir` first).
    pub fn write_ctz_to_root(
        &mut self,
        name: &[u8],
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.write_ctz_to_pair(self.root, name, content, buf_a, buf_b)
    }

    /// Build the CTZ chain blocks for `content` into the physical blocks
    /// in `chain`: for each block write its skip pointers and content
    /// slice, then erase + program it. On the first block whose erase or
    /// program fails (a worn/bad block), returns `Err(phys)` with that
    /// block's physical address so the caller can exclude it and rebuild
    /// past it. `chain.len()` is the block count.
    fn try_build_ctz_chain(
        &mut self,
        chain: &[BlockAddress],
        content: &[u8],
        buf_a: &mut [u8],
    ) -> Result<(), u32> {
        use crate::ctz::skip_pointers_in_block;
        let mut content_off = 0usize;
        for (i, block) in chain.iter().enumerate() {
            let i32 = i as u32;
            let header = 4 * skip_pointers_in_block(i32) as usize;
            for b in buf_a.iter_mut() {
                *b = 0xFF;
            }
            let pointer_count = skip_pointers_in_block(i32) as usize;
            for k in 0..pointer_count {
                let target_idx = i - (1 << k);
                let target_phys = chain[target_idx].as_u32();
                let off = 4 * k;
                buf_a[off..off + 4].copy_from_slice(&target_phys.to_le_bytes());
            }
            let block_capacity = S::BLOCK_SIZE - header;
            let take = block_capacity.min(content.len() - content_off);
            buf_a[header..header + take].copy_from_slice(&content[content_off..content_off + take]);
            content_off += take;

            let phys = block.as_u32();
            self.storage.erase(phys).map_err(|_| phys)?;
            self.storage.program(phys, 0, &buf_a[..S::BLOCK_SIZE]).map_err(|_| phys)?;
        }
        Ok(())
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
        use crate::ctz::block_count;

        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        // Names are capped at NAME_MAX (255), not the tag length field's
        // 0x3FF ceiling: a longer entry name is unreachable or wrongly
        // resolved under the C reference (review M2, `lfs-ax2`). Path-
        // derived names are already within NAME_MAX; this is the guard
        // for the raw-name write APIs that bypass `Path` validation.
        if name.is_empty() || name.len() > crate::NAME_MAX {
            return Err(Error::InvalidPath);
        }
        // Pre-check: detect whether the entry exists anywhere in the
        // directory's HardTail chain, and reject if it exists as a
        // Directory (overwriting a directory with a file is destructive
        // and not supported here; use rmdir + write separately). `target`
        // is the pair to commit to: the owning pair for an update, the
        // chain's last pair (with room) for a create.
        let (target, existing_id): (BlockPair, Option<u16>) =
            match self.seek_entry_in_chain(pair_addr, name, buf_a, buf_b)? {
                ChainSeek::Found { pair, id, kind } => {
                    if kind != crate::dir::EntryKind::RegularFile {
                        return Err(Error::AlreadyExists);
                    }
                    (pair, Some(id))
                }
                ChainSeek::Absent { last_pair, .. } => (last_pair, None),
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

        // Allocate and build the chain, relocating past any block whose
        // erase/program fails (a worn/bad block). The C reference handles
        // bad blocks on demand rather than with a persistent bad-block
        // list, so a failed block is simply excluded from this write's
        // allocation and the chain is rebuilt; the block stays free and is
        // re-relocated if a later write hits it again. Bounded retries
        // guard against a wholly-bad device.
        let mut chain = [BlockAddress::NONE; MAX_CTZ_WRITE_BLOCKS];
        let mut excluded = [BlockAddress::NONE; MAX_BAD_BLOCK_RETRIES];
        let mut ex_len = 0usize;
        loop {
            crate::alloc::alloc_blocks_cached(
                &mut self.storage,
                self.root,
                &mut self.used_cache,
                &excluded[..ex_len],
                None,
                &mut chain[..total_blocks],
                buf_a,
                buf_b,
            )?;
            match self.try_build_ctz_chain(&chain[..total_blocks], content, buf_a) {
                Ok(()) => break,
                Err(bad_phys) => {
                    if ex_len >= MAX_BAD_BLOCK_RETRIES {
                        return Err(Error::Io);
                    }
                    // Exclude the bad block and re-scan so it is not handed
                    // back, then rebuild the whole chain.
                    excluded[ex_len] = BlockAddress::new(bad_phys);
                    ex_len += 1;
                    self.used_cache = None;
                }
            }
        }
        self.storage.sync().map_err(|_| Error::Io)?;

        // Append the metadata commit.
        // buf_a was consumed for chain bytes; re-read the target pair
        // just to learn the live-entry count for the new id.
        self.storage.read(target.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(target.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let count: usize = {
            let pair = MetadataPair::parse(target.a, &*buf_a, target.b, &*buf_b)?;
            let active_is_a = pair.active_block == target.a;
            let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
            gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?
        };

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
            WriteOp::CreateCtz {
                id: new_id,
                name,
                head_block,
                total_size,
                moved_attrs: StagedAttrs::EMPTY,
            }
        };

        // Pass the just-allocated chain as inflight so a commit-internal
        // wear relocation, worn-block retry, or split (if it fires) will
        // not reallocate a chain block. The chain is carried as `(head,
        // size)` coordinates, not a materialized block list: the list form
        // capped the honored exclusion at the small `blocks` arrays in the
        // commit-internal allocation sites (each `2 + MAX_QUEUED_PAIRS`),
        // so a chain of 33+ blocks overflowed them and failed the write
        // with `OutOfRange` (review M9). The chain is fully programmed and
        // synced above, so the allocator's on-demand walk (ADR-0010/0011,
        // the review C9 mechanism) excludes every chain block regardless of
        // length, exactly as the streaming-append publish path does.
        self.apply_op_to_pair_inner(
            target,
            &op,
            None,
            None,
            None,
            Inflight { blocks: &[], chain: Some((head_block, total_size)) },
            buf_a,
            buf_b,
        )
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
        // A truncate rewrites the file onto fresh blocks, orphaning the
        // old chain; invalidate the lookahead cache so it is reclaimed.
        self.used_cache = None;
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

        // Position on the pair that owns the entry within the parent's
        // HardTail chain (the last pair when the name is absent), and
        // classify the existing entry. `owner` carries the owning pair so
        // the CTZ-append commit targets it rather than the chain's first
        // pair.
        let owner = match self.seek_entry_in_chain(parent, name, buf_a, buf_b)? {
            ChainSeek::Found { pair, .. } => pair,
            ChainSeek::Absent { last_pair, .. } => last_pair,
        };

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
            let p = MetadataPair::parse(owner.a, &*buf_a, owner.b, &*buf_b)?;
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
                self.append_ctz_streaming(owner, id, ctz, additional, buf_a, buf_b)
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
        let (head_phys, new_size) = self.stream_ctz_extend(ctz, data, buf_a, buf_b)?;
        self.commit_update_ctz(pair_addr, entry_id, head_phys, new_size, buf_a, buf_b)
    }

    /// Program `data` to the end of an existing CTZ chain, returning
    /// the new `(head_block, total_size)` WITHOUT committing the
    /// metadata-pair entry update. Used by both [`Self::append_to_path`]
    /// (which immediately follows up with `commit_update_ctz`) and the
    /// stateful [`crate::file::File`] handle (which defers the commit
    /// until [`crate::file::File::sync`] / `close`, amortizing the
    /// metadata-pair touch over a session of writes).
    ///
    /// Existing chain blocks are never re-erased.
    ///
    /// # Committed-tail discipline (review C8)
    ///
    /// The committed tail block is programmed at most once per append,
    /// as the LAST device action before sync, after every fallible
    /// step (bounds checks, allocation, overflow-block writes) has
    /// succeeded — a failure must never leave programmed cells past
    /// the committed EOF, because the metadata still says `old_size`
    /// and the next append would AND different bytes over them on
    /// NOR, corrupting acknowledged data. And the fill region is
    /// verified actually erased first: a previous append torn by
    /// power loss leaves residue there that no metadata records. A
    /// dirty region routes the tail through copy-on-write to a fresh
    /// block instead (the same countermeasure `shrink_ctz_head` uses;
    /// the C reference never programs a committed data block twice,
    /// `lfs_ctz_extend`, lfs.c:2891ff). The clean in-place fill is
    /// kept as the common case for ADR-0011's write-amplification
    /// win.
    pub(crate) fn stream_ctz_extend(
        &mut self,
        ctz: crate::ctz::CtzStruct,
        data: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(u32, u32), Error> {
        use crate::ctz::{
            block_count, block_index_at_offset, content_bytes_in_block, seek_block,
            skip_pointers_in_block,
        };

        let bs = S::BLOCK_SIZE as u32;
        let old_size = ctz.size;
        let n_old = block_count(old_size, bs);
        if (n_old as usize) > MAX_CTZ_WRITE_BLOCKS {
            return Err(Error::OutOfRange);
        }
        // All bounds checks up front (review C8): nothing may program
        // before every fallible step is known to be reachable.
        let new_total = old_size + data.len() as u32;
        let new_n = block_count(new_total, bs);
        if (new_n as usize) > MAX_CTZ_WRITE_BLOCKS {
            return Err(Error::OutOfRange);
        }

        // Step 1: plan the tail fill. The tail block *is* the chain
        // head, so its address is known without walking the chain.
        // `deferred_fill` carries a clean in-place fill to execute
        // after the overflow writes; a dirty region is rebuilt
        // copy-on-write right here (fresh blocks are safe to write
        // early: a failure merely orphans them).
        let mut data_consumed: usize = 0;
        let mut tail_addr = ctz.head_block;
        let mut deferred_fill: Option<(u32, u32, usize)> = None;
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
                let tail_phys = ctz.head_block.as_u32();
                let off = (header + bytes_used) as usize;
                // Dirty check: is the fill region actually erased?
                let clean = {
                    self.storage.read(tail_phys, 0, buf_a).map_err(|_| Error::Io)?;
                    buf_a[off..off + fill].iter().all(|&x| x == 0xFF)
                };
                if clean {
                    deferred_fill = Some((tail_phys, off as u32, fill));
                } else {
                    // Copy-on-write: rebuild the committed tail plus
                    // the fill on a fresh block. The un-committed
                    // chain is named so a rescan cannot hand out its
                    // blocks; worn candidates are excluded and
                    // retried, bounded.
                    let exclude_chain = Some((ctz.head_block.as_u32(), old_size));
                    let mut excluded = [BlockAddress::NONE; MAX_BAD_BLOCK_RETRIES];
                    let mut ex_len = 0usize;
                    loop {
                        let mut one = [BlockAddress::NONE; 1];
                        crate::alloc::alloc_blocks_cached(
                            &mut self.storage,
                            self.root,
                            &mut self.used_cache,
                            &excluded[..ex_len],
                            exclude_chain,
                            &mut one,
                            buf_a,
                            buf_b,
                        )?;
                        let fresh = one[0];
                        // Rebuild the image: committed bytes verbatim
                        // (the skip-pointer header references the
                        // unchanged earlier chain), residue cleared,
                        // fill appended.
                        self.storage
                            .read(ctz.head_block.as_u32(), 0, buf_a)
                            .map_err(|_| Error::Io)?;
                        for x in &mut buf_a[off..] {
                            *x = 0xFF;
                        }
                        buf_a[off..off + fill].copy_from_slice(&data[..fill]);
                        if self.storage.erase(fresh.as_u32()).is_ok()
                            && self
                                .storage
                                .program(fresh.as_u32(), 0, &buf_a[..S::BLOCK_SIZE])
                                .is_ok()
                        {
                            tail_addr = fresh;
                            break;
                        }
                        if ex_len >= MAX_BAD_BLOCK_RETRIES {
                            return Err(Error::Io);
                        }
                        excluded[ex_len] = fresh;
                        ex_len += 1;
                        self.used_cache = None;
                    }
                }
                data_consumed = fill;
            }
        }
        let mut head_phys = tail_addr.as_u32();

        // Step 2: allocate new chain blocks for the remainder. Each new
        // block stores skip pointers to earlier blocks: newly allocated
        // ones come from `new_blocks`; existing ones are resolved by an
        // O(log n) `seek_block` from the (possibly copy-on-written) tail
        // rather than a full chain walk.
        if data_consumed < data.len() {
            let new_count = (new_n - n_old) as usize;

            // The existing chain may not be committed to its metadata
            // entry yet (the stateful `File` write path batches writes),
            // so an authoritative allocator rescan would not see it. Name
            // it as the in-flight chain to exclude. On the hot cache-hit
            // path the allocator skips the walk entirely (those blocks
            // were marked when handed out), so the append no longer pays
            // an O(n) chain collect; the walk happens only on a rescan.
            let exclude_chain = if n_old > 0 { Some((tail_addr.as_u32(), old_size)) } else { None };
            let remaining = &data[data_consumed..];

            // Bad-block retry. A worn overflow block (its erase or program
            // returns `Err`) is excluded and the whole new-block set
            // re-allocated and rewritten, mirroring the initial-write
            // path's `try_build_ctz_chain`. Re-allocation changes the new
            // blocks' addresses, so the skip pointers among them are
            // rebuilt each attempt; the existing chain (and the tail bytes
            // already packed in step 1) are untouched. Bounded by
            // `MAX_BAD_BLOCK_RETRIES` (lfs-23f).
            let mut excluded = [BlockAddress::NONE; MAX_BAD_BLOCK_RETRIES];
            let mut ex_len = 0usize;
            'retry: loop {
                let mut new_blocks = [BlockAddress::NONE; MAX_CTZ_WRITE_BLOCKS];
                crate::alloc::alloc_blocks_cached(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..ex_len],
                    exclude_chain,
                    &mut new_blocks[..new_count],
                    buf_a,
                    buf_b,
                )?;

                let mut content_off = 0usize;
                for new_i in n_old..new_n {
                    let header_bytes = 4 * skip_pointers_in_block(new_i) as usize;
                    for b in buf_a.iter_mut() {
                        *b = 0xFF;
                    }
                    let ptr_count = skip_pointers_in_block(new_i) as usize;
                    for k in 0..ptr_count {
                        let target_idx =
                            (new_i as usize).checked_sub(1 << k).ok_or(Error::Corrupt)?;
                        let target_phys = if target_idx >= n_old as usize {
                            new_blocks[target_idx - n_old as usize].as_u32()
                        } else {
                            // Seek from `tail_addr`: when the tail was
                            // copy-on-written, index n_old - 1 must
                            // resolve to the NEW tail; its copied
                            // header reaches the unchanged earlier
                            // blocks.
                            seek_block(&mut self.storage, tail_addr, n_old - 1, target_idx as u32)?
                                .as_u32()
                        };
                        let off = 4 * k;
                        buf_a[off..off + 4].copy_from_slice(&target_phys.to_le_bytes());
                    }
                    let block_capacity = S::BLOCK_SIZE - header_bytes;
                    let take = block_capacity.min(remaining.len() - content_off);
                    buf_a[header_bytes..header_bytes + take]
                        .copy_from_slice(&remaining[content_off..content_off + take]);
                    content_off += take;

                    let phys = new_blocks[(new_i - n_old) as usize].as_u32();
                    if self.storage.erase(phys).is_err()
                        || self.storage.program(phys, 0, &buf_a[..S::BLOCK_SIZE]).is_err()
                    {
                        // Worn block: exclude it and rebuild the new set.
                        if ex_len >= MAX_BAD_BLOCK_RETRIES {
                            return Err(Error::Io);
                        }
                        excluded[ex_len] = BlockAddress::new(phys);
                        ex_len += 1;
                        self.used_cache = None;
                        continue 'retry;
                    }
                }
                head_phys = new_blocks[new_count - 1].as_u32();
                break;
            }
        }

        // Step 3: the clean in-place fill of the COMMITTED tail block,
        // last (review C8): every fallible step above has succeeded, so
        // the only way these cells get programmed without the metadata
        // update following is a power loss, which the next append's
        // dirty check turns into a copy-on-write.
        if let Some((tail_phys, off, fill)) = deferred_fill {
            self.storage.program(tail_phys, off, &data[..fill]).map_err(|_| Error::Io)?;
        }

        self.storage.sync().map_err(|_| Error::Io)?;

        let new_size = old_size + data.len() as u32;
        Ok((head_phys, new_size))
    }

    /// Compute the new head block for a CTZ file shrunk to `new_size`
    /// (`0 < new_size <= old_size`), relocating a partial tail block when
    /// necessary so a later in-place append stays NOR-correct.
    ///
    /// A shrink whose new tail block is left exactly full has no stale
    /// content past `new_size`, so the existing block is reused. A shrink
    /// that lands mid-block leaves content beyond `new_size` programmed
    /// (not `0xFF`); reusing that block would make the next
    /// [`Self::stream_ctz_extend`] fill its tail region in place over
    /// dirty NOR cells (a `1 -> 0`-only device ANDs the appended bytes
    /// with the stale content, corrupting them). To prevent that, the
    /// kept prefix is relocated copy-on-write to a freshly erased block,
    /// which becomes the new head. The skip-pointer header is copied
    /// verbatim because it references the unchanged earlier chain blocks.
    ///
    /// **Atomicity.** Only a fresh block is written; the old chain and
    /// the committed metadata are untouched until the caller commits the
    /// new head at sync. A power loss here leaves the previous file state
    /// fully readable, and the orphaned fresh block is reclaimed by the
    /// next allocator scan.
    pub(crate) fn shrink_ctz_head(
        &mut self,
        head_block: BlockAddress,
        old_size: u32,
        new_size: u32,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<BlockAddress, Error> {
        use crate::ctz::{
            block_count, block_index_at_offset, collect_chain_blocks, content_bytes_in_block,
            skip_pointers_in_block, MAX_CTZ_BLOCKS,
        };
        debug_assert!(new_size > 0 && new_size <= old_size);
        let bs = S::BLOCK_SIZE as u32;
        let n_old = block_count(old_size, bs);
        if n_old as usize > MAX_CTZ_BLOCKS {
            return Err(Error::OutOfRange);
        }
        let mut chain = [BlockAddress::NONE; MAX_CTZ_BLOCKS];
        collect_chain_blocks(&mut self.storage, head_block, n_old, &mut chain[..n_old as usize])?;

        let (new_tail_idx, abs_off) = block_index_at_offset(new_size - 1, bs);
        let header = 4 * skip_pointers_in_block(new_tail_idx);
        let tail_cap = content_bytes_in_block(new_tail_idx, bs);
        let bytes_used = abs_off + 1 - header;
        let old_tail = chain[new_tail_idx as usize];
        if bytes_used == tail_cap {
            // New tail is exactly full: no stale content past new_size,
            // so the next append allocates fresh blocks and the existing
            // block can be reused unchanged.
            return Ok(old_tail);
        }

        // Partial tail: relocate the kept prefix onto a freshly erased
        // block. Exclude the whole existing chain so the allocator never
        // hands back a block this (possibly not-yet-committed) file still
        // uses.
        let mut fresh = [BlockAddress::NONE; 1];
        crate::alloc::alloc_blocks_cached(
            &mut self.storage,
            self.root,
            &mut self.used_cache,
            &chain[..n_old as usize],
            None,
            &mut fresh,
            buf_a,
            buf_b,
        )?;
        let fresh_addr = fresh[0];

        // Build the relocated block: header + kept content from the old
        // tail, the remainder of the block left erased.
        self.storage
            .read(old_tail.as_u32(), 0, &mut buf_a[..S::BLOCK_SIZE])
            .map_err(|_| Error::Io)?;
        let keep = (header + bytes_used) as usize;
        for byte in &mut buf_a[keep..S::BLOCK_SIZE] {
            *byte = 0xFF;
        }
        self.storage.erase(fresh_addr.as_u32()).map_err(|_| Error::Io)?;
        self.storage
            .program(fresh_addr.as_u32(), 0, &buf_a[..S::BLOCK_SIZE])
            .map_err(|_| Error::Io)?;
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(fresh_addr)
    }

    /// Emit an `UpdateCtz` commit on `pair_addr` pointing entry `id` at
    /// `(head_block, total_size)`. Dispatches through the standard
    /// append-or-compact machinery used by every other write op.
    pub(crate) fn commit_update_ctz(
        &mut self,
        pair_addr: BlockPair,
        id: u16,
        head_block: u32,
        total_size: u32,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let op = WriteOp::UpdateCtz { id, head_block, total_size };
        // The chain being published is programmed but unreferenced
        // until this commit lands; name it so commit-internal
        // allocations (wear relocation, worn-block retry rescans,
        // split continuations) cannot hand out its blocks (review C9).
        self.apply_op_to_pair_inner(
            pair_addr,
            &op,
            None,
            None,
            None,
            Inflight { blocks: &[], chain: Some((head_block, total_size)) },
            buf_a,
            buf_b,
        )
    }

    /// Resolve a path's parent directory. Returns `(parent_pair,
    /// leaf_name)` where `parent_pair` is the metadata pair of the
    /// directory that should contain the leaf component, and
    /// `leaf_name` is the final path component as a `&str`.
    ///
    /// Returns [`Error::InvalidPath`] for the root path (no parent).
    /// Returns [`Error::NotFound`] if any intermediate component does
    /// not resolve to a Directory.
    pub(crate) fn resolve_parent<'p>(
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

        // Find the pair to commit the new entry to: the last pair of the
        // parent's HardTail chain (the one with room). Reject a name that
        // already exists anywhere in the chain. The new directory is
        // inserted into the global metadata-pair list immediately after
        // that pair: it inherits the pair's current tail (a SoftTail to
        // the next directory, since the last pair of a chain has no
        // HardTail), then the pair's tail is re-pointed at the new dir
        // (matches `lfs_mkdir_`). For a single-pair directory this pair
        // is the parent itself.
        let target = match self.seek_entry_in_chain(parent, dir_name_bytes, buf_a, buf_b)? {
            ChainSeek::Found { .. } => return Err(Error::AlreadyExists),
            ChainSeek::Absent { last_pair, .. } => last_pair,
        };
        let parent_tail: Option<(BlockPair, bool)> = {
            let p = MetadataPair::parse(target.a, &*buf_a, target.b, &*buf_b)?;
            p.reader.tail().map(|t| (t, p.reader.is_hard_tail()))
        };

        // Reachable-pair budget. The mount-time walks (allocator scan,
        // gstate accumulation, deorphan) enumerate the forest into fixed
        // `MAX_QUEUED_PAIRS` arrays (ADR-0006), so a forest larger than the
        // budget is unmountable. mkdir adds the new directory pair, and the
        // `CreateDir` commit may split the parent's last pair into a
        // continuation — up to two new reachable pairs, and neither the new
        // dir (not yet referenced) nor the split (its own check sees the
        // pre-split tree) accounts for the other. Reserve two slots here,
        // else the writer could produce an image its own mount cannot read
        // (lfs-43o, the mkdir analogue of the split-path guard).
        // `collect_live_tree_pairs` clobbers both buffers, but `parent_tail`
        // is already captured and the allocation re-reads what it needs.
        {
            let mut tree = [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE);
                crate::alloc::MAX_QUEUED_PAIRS];
            let reachable =
                collect_live_tree_pairs(&mut self.storage, self.root, &mut tree, buf_a, buf_b)?;
            if reachable + 2 > crate::alloc::MAX_QUEUED_PAIRS {
                return Err(Error::OutOfRange);
            }
        }

        // Allocate two blocks for the new directory's metadata pair.
        let mut new_blocks = [BlockAddress::NONE; 2];
        crate::alloc::alloc_blocks_cached(
            &mut self.storage,
            self.root,
            &mut self.used_cache,
            &[],
            None,
            &mut new_blocks,
            buf_a,
            buf_b,
        )?;
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
            // The new directory takes the parent's old place in the global
            // list: its tail points where the parent's tail did (null if
            // the parent was the list end). Crash-safe: until the parent's
            // commit below lands, the new dir is referenced by nothing and
            // is reclaimed as an orphan, so this link is never reachable.
            if let Some((tail_pair, is_hard)) = parent_tail {
                let mut body = [0u8; 8];
                body[0..4].copy_from_slice(&tail_pair.a.as_u32().to_le_bytes());
                body[4..8].copy_from_slice(&tail_pair.b.as_u32().to_le_bytes());
                let tag_type = if is_hard {
                    crate::tag::TagType::HardTail
                } else {
                    crate::tag::TagType::SoftTail
                };
                commit.tag(crate::tag::Tag::new(true, tag_type, crate::tag::ID_NONE, 8), &body)?;
            }
            commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
            commit.bytes_written()
        };
        self.storage.program(new_dir.a.as_u32(), 0, &buf_a[..new_end]).map_err(|_| Error::Io)?;
        // Review H2: the new directory's init commit gets the same
        // read-back every other commit gets. A verify failure reports
        // `Io` exactly like a program failure on this path; the
        // allocated pair stays an unreferenced orphan. `buf_a` is
        // re-read below before its next use.
        if !verify_programmed(&mut self.storage, new_dir.a, 0, &mut buf_a[..new_end]) {
            return Err(Error::Io);
        }
        self.storage.sync().map_err(|_| Error::Io)?;

        // Re-read the target pair to compute the new id from the live
        // count, then append the CreateDir commit. Pass the just-allocated
        // new_dir blocks as inflight so wear-level relocation (if it
        // fires) won't reallocate them.
        self.storage.read(target.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(target.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let count: usize = {
            let p = MetadataPair::parse(target.a, &*buf_a, target.b, &*buf_b)?;
            let active_is_a = p.active_block == target.a;
            let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
            gather_live_slots(&p, active_is_a, buf_a, buf_b, &mut slots)?
        };
        let new_id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
        if new_id == crate::tag::ID_NONE {
            return Err(Error::OutOfRange);
        }
        let op = WriteOp::CreateDir {
            id: new_id,
            name: dir_name_bytes,
            dir_pair: new_dir,
            moved_attrs: StagedAttrs::EMPTY,
        };
        // Re-point the target pair's tail at the new dir (SoftTail),
        // inserting it into the global list right after the parent. Rides
        // the same atomic commit as the DirStruct.
        self.apply_op_to_pair_inner(
            target,
            &op,
            None,
            None,
            Some((new_dir, false)),
            Inflight { blocks: &new_blocks, chain: None },
            buf_a,
            buf_b,
        )
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
        // checking emptiness would orphan its content. Use rmdir. The
        // entry may live in any pair of the parent's HardTail chain.
        match self.seek_entry_in_chain(parent, leaf.as_bytes(), buf_a, buf_b)? {
            ChainSeek::Found { kind: crate::dir::EntryKind::Directory, .. } => {
                return Err(Error::AlreadyExists);
            }
            ChainSeek::Found { .. } => {}
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        }
        self.remove_from_pair(parent, leaf.as_bytes(), buf_a, buf_b)
    }

    /// Set (or replace) a user attribute on the entry at `path`.
    ///
    /// LittleFS lets each entry carry a small set of arbitrary
    /// key-value pairs (the keys are byte ids in `0..=255`, the
    /// values are byte slices up to `0x3FE` bytes). Attributes are
    /// stored as `UserAttr(attr_id)` tags at the entry's id inside
    /// its directory pair; later tags supersede earlier ones, so a
    /// repeated `set_attr` simply appends a new tag and the reader
    /// returns the most recent value.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if `path` does not exist.
    /// - [`Error::InvalidPath`] for the root path (root has no
    ///   directory entry to attach attributes to).
    /// - [`Error::OutOfRange`] if `value.len() >= 0x3FF` (the length
    ///   sentinel is reserved for the delete marker).
    pub fn set_attr(
        &mut self,
        path: crate::path::Path<'_>,
        attr_id: u8,
        value: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if value.len() >= 0x3FF {
            return Err(Error::OutOfRange);
        }
        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;
        let (target, id) = match self.seek_entry_in_chain(parent, leaf.as_bytes(), buf_a, buf_b)? {
            ChainSeek::Found { pair, id, .. } => (pair, id),
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        };
        let op = WriteOp::SetAttr { id, attr_id, value };
        self.apply_op_to_pair(target, &op, buf_a, buf_b)
    }

    /// Remove a user attribute from the entry at `path`. Idempotent:
    /// removing a non-existent attribute is not an error; the
    /// delete-marker tag lands all the same and the reader treats the
    /// attribute as absent.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if `path` does not exist.
    pub fn remove_attr(
        &mut self,
        path: crate::path::Path<'_>,
        attr_id: u8,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;
        let (target, id) = match self.seek_entry_in_chain(parent, leaf.as_bytes(), buf_a, buf_b)? {
            ChainSeek::Found { pair, id, .. } => (pair, id),
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        };
        let op = WriteOp::RemoveAttr { id, attr_id };
        self.apply_op_to_pair(target, &op, buf_a, buf_b)
    }

    /// Read the most recently committed value of user attribute
    /// `attr_id` on the entry at `path` into `out`. Returns the
    /// number of bytes copied (clamped at `out.len()`).
    ///
    /// Returns `Ok(0)` if the entry has no `UserAttr(attr_id)` tag,
    /// or if the latest such tag is a delete marker.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if `path` does not exist.
    pub fn get_attr(
        &mut self,
        path: crate::path::Path<'_>,
        attr_id: u8,
        out: &mut [u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<usize, Error> {
        let (parent, leaf) = self.resolve_parent(path, buf_a, buf_b)?;
        // Chase the parent's HardTail chain, exactly as `set_attr`
        // does: an entry in a split directory's continuation pair has
        // readable attributes too (review H5). On return the scratch
        // buffers hold the owning pair.
        let (target, id) = match self.seek_entry_in_chain(parent, leaf.as_bytes(), buf_a, buf_b)? {
            ChainSeek::Found { pair, id, .. } => (pair, id),
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        };
        let pair = MetadataPair::parse(target.a, &*buf_a, target.b, &*buf_b)?;
        // Splice-diff query (review C2): `id` is the entry's *current*
        // live id; the committed tags carry the raw ids of their write
        // time. `attr_get` walks the log backward adjusting for every
        // intervening Create/Delete, the C reference's
        // `lfs_dir_getslice` algorithm.
        match crate::dir::attr_get(&pair.reader, id, attr_id) {
            None => Ok(0),
            Some(body) => {
                let n = body.len().min(out.len());
                out[..n].copy_from_slice(&body[..n]);
                Ok(n)
            }
        }
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
        // Rename may overwrite an existing target (freeing its blocks);
        // invalidate the lookahead so those blocks are reclaimable.
        self.used_cache = None;
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

        // Resolve the old entry across the parent's HardTail chain; reject
        // if the new name already exists anywhere in the chain. Distinct
        // names (guaranteed above) resolve to distinct entries, so any
        // existing new name is a genuine collision.
        let (old_pair, old_id, kind) =
            match self.seek_entry_in_chain(parent, old_leaf_bytes, buf_a, buf_b)? {
                ChainSeek::Found { pair, id, kind } => (pair, id, kind),
                ChainSeek::Absent { .. } => return Err(Error::NotFound),
            };
        if let ChainSeek::Found { .. } =
            self.seek_entry_in_chain(parent, new_leaf_bytes, buf_a, buf_b)?
        {
            return Err(Error::AlreadyExists);
        }

        let name_type = match kind {
            crate::dir::EntryKind::RegularFile => crate::tag::TagType::RegularFile,
            crate::dir::EntryKind::Directory => crate::tag::TagType::Directory,
        };
        let op = WriteOp::RenameInPlace { id: old_id, name_type, new_name: new_leaf_bytes };
        self.apply_op_to_pair(old_pair, &op, buf_a, buf_b)
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
    /// The entry's user attributes move with it, riding the
    /// destination `Create` commit atomically (review H6; the C
    /// reference's `LFS_FROM_MOVE` replays all unique tags of the
    /// moved id). One documented divergence: the attributes stage
    /// through a fixed 1 KiB stack pool, so an entry whose live
    /// attribute payload exceeds [`RENAME_ATTR_STAGE`] fails the
    /// rename with [`Error::OutOfRange`] (attributes intact, nothing
    /// moved) where the C reference, which streams the source pair
    /// through its block caches, would succeed.
    ///
    /// # Cycle prevention
    ///
    /// Rejects with [`Error::InvalidPath`] if `old_path` is a strict
    /// ancestor of `new_path` (would move a directory inside itself).
    ///
    /// # Atomicity
    ///
    /// Both commits carry a balanced `MoveState` tag whose 12-byte body
    /// XORs to zero once both land. A crash between them leaves the
    /// filesystem-global gstate non-zero; [`Self::mount`] BFS-walks
    /// every reachable metadata pair, XOR-accumulates every committed
    /// `MoveState` body, and if the result is non-zero decodes the
    /// in-flight `(src_pair, src_id)` and emits the missing source
    /// Delete + balancing MoveState before returning the `Fs` handle.
    /// Callers never observe the duplicate-entry state.
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
        // Rename may overwrite an existing target (freeing its blocks) and
        // moves struct bodies between pairs; invalidate the lookahead.
        self.used_cache = None;
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

        // Position on the source entry's owning pair within the source
        // parent's HardTail chain, then copy its struct body to a stack
        // buffer so the destination commit can borrow it after we release
        // the source pair's parse. `src_owner` is the pair the source
        // Delete (and its MoveState body) must target.
        let src_owner = match self.seek_entry_in_chain(old_parent, old_leaf_bytes, buf_a, buf_b)? {
            ChainSeek::Found { pair, .. } => pair,
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        };
        let mut src_body = [0u8; 1024];
        // The moved entry's live user attributes ride the destination
        // Create commit (review H6; the C reference's `LFS_FROM_MOVE`
        // replays all unique tags of the moved id, so attributes move
        // atomically with the entry). Staged through a stack pool like
        // `src_body`: the destination commit cannot read the source
        // pair (both scratch buffers hold the destination by then),
        // and the kernel owns no third block buffer.
        let mut attr_stage = [0u8; RENAME_ATTR_STAGE];
        let src_id;
        let src_struct_type;
        let src_body_len;
        let staged_attr_len;
        {
            let p = MetadataPair::parse(src_owner.a, &*buf_a, src_owner.b, &*buf_b)?;
            let r = crate::dir::lookup(&p, old_leaf_bytes).ok_or(Error::NotFound)?;
            let n = r.struct_body.len();
            if n > src_body.len() {
                return Err(Error::OutOfRange);
            }
            src_body[..n].copy_from_slice(r.struct_body);
            src_id = r.entry.id;
            src_struct_type = r.struct_type;
            src_body_len = n;
            staged_attr_len = stage_live_attrs(&p.reader, src_id, &mut attr_stage)?.records.len();
        }
        let moved_attrs = StagedAttrs { records: &attr_stage[..staged_attr_len] };

        // Reject if the destination already exists anywhere in the
        // destination parent's chain; compute the new entry id from the
        // destination chain's last pair (the one the Create targets).
        let (dst_last, new_id): (BlockPair, u16) =
            match self.seek_entry_in_chain(new_parent, new_leaf_bytes, buf_a, buf_b)? {
                ChainSeek::Found { .. } => return Err(Error::AlreadyExists),
                ChainSeek::Absent { last_pair, count } => {
                    let id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
                    if id == crate::tag::ID_NONE {
                        return Err(Error::OutOfRange);
                    }
                    (last_pair, id)
                }
            };

        // Build the Create op for the destination, preserving the
        // source's struct shape.
        let create_op = match src_struct_type {
            crate::tag::TagType::InlineStruct => WriteOp::Create {
                id: new_id,
                name: new_leaf_bytes,
                content: &src_body[..src_body_len],
                moved_attrs,
            },
            crate::tag::TagType::CtzStruct => {
                if src_body_len != 8 {
                    return Err(Error::Corrupt);
                }
                let head_block =
                    u32::from_le_bytes([src_body[0], src_body[1], src_body[2], src_body[3]]);
                let total_size =
                    u32::from_le_bytes([src_body[4], src_body[5], src_body[6], src_body[7]]);
                WriteOp::CreateCtz {
                    id: new_id,
                    name: new_leaf_bytes,
                    head_block,
                    total_size,
                    moved_attrs,
                }
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
                    moved_attrs,
                }
            }
            _ => return Err(Error::Corrupt),
        };

        // Step 1: Create in destination. Step 2: Delete from source.
        // Doing Create first keeps the entry reachable through the
        // failure window (duplicate, not loss); doing Delete first
        // would risk a true vanish if Create then failed.
        // Atomic-move-state encoding: both commits carry the same
        // MoveState body so they XOR to zero once both land. A crash
        // between step 1 and step 2 leaves the gstate non-zero, which
        // mount-time recovery decodes and completes.
        // The MoveState body identifies the source entry by its owning
        // pair so mount-time recovery completes the Delete at the right
        // pair when the source spans a HardTail chain.
        let move_body = crate::gstate::build_move_body(src_owner, src_id);
        // Track the in-flight source coordinates so a relocation
        // cascade triggered by the destination commit can remap them
        // (review C6; see `Fs::pending_move`). Cleared on every exit
        // from this window, including the destination commit failing.
        self.pending_move =
            Some(PendingMove { cur_pair: src_owner, cur_id: src_id, delta: move_body });
        let dst_result = self.apply_op_to_pair_with_movestate(
            dst_last,
            &create_op,
            Some(move_body),
            buf_a,
            buf_b,
        );
        let pm = self.pending_move.take();
        dst_result?;
        // The delta must be the ORIGINAL body (it cancels the
        // destination commit's by XOR); the target address is the
        // source pair's CURRENT location, remapped through any cascade.
        let pm =
            pm.unwrap_or(PendingMove { cur_pair: src_owner, cur_id: src_id, delta: move_body });
        self.apply_op_to_pair_with_movestate(
            pm.cur_pair,
            &WriteOp::Remove { id: pm.cur_id },
            Some(pm.delta),
            buf_a,
            buf_b,
        )
    }

    /// Apply a `WriteOp` to a metadata pair through the standard
    /// append-or-compact dispatch. Convenience wrapper around
    /// [`Self::apply_op_to_pair_with_movestate`] with no MoveState
    /// piggyback (the common case).
    /// Convenience wrapper for ops with NO in-flight blocks: it
    /// passes [`Inflight::NONE`]. A caller publishing blocks that no
    /// committed metadata references yet (a CTZ chain, a fresh pair)
    /// must call [`Self::apply_op_to_pair_inner`] and name them
    /// (review C9).
    pub(crate) fn apply_op_to_pair(
        &mut self,
        pair_addr: BlockPair,
        op: &WriteOp<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.apply_op_to_pair_with_movestate(pair_addr, op, None, buf_a, buf_b)
    }

    /// Replace the STRUCT body of entry `id` in `parent` with an
    /// inline body equal to `content`. Used by [`crate::file::File::sync`]
    /// to commit a truncated-to-empty file (the prior `CtzStruct`
    /// chain becomes orphan and is reclaimed by the next allocator
    /// scan).
    pub(crate) fn update_inline_at_id(
        &mut self,
        parent: BlockPair,
        id: u16,
        content: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let op = WriteOp::Update { id, content };
        self.apply_op_to_pair(parent, &op, buf_a, buf_b)
    }

    /// Apply a `WriteOp` to a metadata pair, optionally folding a
    /// `MoveState` delta into the pair's contribution on the same
    /// commit. Used by cross-directory rename to encode in-flight
    /// gstate atomically with the user-visible change.
    ///
    /// `extra_move_state` is a DELTA. Both paths fold it with the
    /// pair's existing contribution and emit the pair's new TOTAL
    /// (the C convention; review C4, see [`crate::gstate`]): the
    /// append path emits the folded total as a new tag (shadowing the
    /// previous total under the reader's latest-tag-wins scan, even
    /// when the new total is all-zero), and the compact path emits
    /// the net total as the rebuilt block's single gstate tag
    /// (omitting an all-zero net: absence reads as zero).
    fn apply_op_to_pair_with_movestate(
        &mut self,
        pair_addr: BlockPair,
        op: &WriteOp<'_>,
        extra_move_state: Option<[u8; crate::gstate::MOVE_STATE_BODY_SIZE]>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.apply_op_to_pair_inner(
            pair_addr,
            op,
            extra_move_state,
            None,
            None,
            Inflight::NONE,
            buf_a,
            buf_b,
        )
    }

    /// Inner form of [`Self::apply_op_to_pair_with_movestate`] that
    /// also carries an optional `extra_relocate_state` body (used by
    /// the wear-levelling parent-update cascade) and an `inflight`
    /// list of block addresses the allocator must treat as in-use
    /// even though they are not yet reachable from `root` (each
    /// in-flight child relocation's fresh block).
    #[allow(clippy::too_many_arguments)]
    fn apply_op_to_pair_inner(
        &mut self,
        pair_addr: BlockPair,
        op: &WriteOp<'_>,
        extra_move_state: Option<[u8; crate::gstate::MOVE_STATE_BODY_SIZE]>,
        extra_relocate_state: Option<[u8; crate::gstate::RELOCATE_STATE_BODY_SIZE]>,
        new_tail: Option<(BlockPair, bool)>,
        inflight: Inflight<'_>,
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
        let active_erased;
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        let count: usize;
        let pair_existing_ms: [u8; crate::gstate::MOVE_STATE_BODY_SIZE];
        let pair_existing_rs: [u8; crate::gstate::RELOCATE_STATE_BODY_SIZE];
        let current_tail: Option<(BlockPair, bool)>;
        {
            let pair = MetadataPair::parse(pair_addr.a, &*buf_a, pair_addr.b, &*buf_b)?;
            active_addr = pair.active_block;
            alternate_addr = pair.alternate_block;
            active_is_a = active_addr == pair_addr.a;
            committed_end = pair.reader.committed_end();
            next_ptag = pair.reader.next_ptag();
            old_revision = pair.reader.revision();
            active_erased = pair.reader.erased();
            pair_existing_ms = scan_pair_move_state(&pair);
            pair_existing_rs = scan_pair_relocate_state(&pair);
            current_tail = pair.reader.tail().map(|t| (t, pair.reader.is_hard_tail()));
            count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
        }
        // The effective tail for this commit: an explicit `new_tail`
        // override (mkdir threading, rmdir un-threading) wins; otherwise
        // preserve whatever the pair already threads. Compaction must
        // re-emit it; an in-place append keeps the existing tail tag and
        // only emits when the tail actually changes.
        let effective_tail = new_tail.or(current_tail);
        // A `new_tail` whose pair is the NONE sentinel means "clear the
        // tail" (un-threading the global-list end so the predecessor
        // becomes the new end). The tail tag is sticky, so an in-place
        // append cannot drop it; force a compaction, which rebuilds the
        // block without re-emitting any tail tag.
        let clear_tail = matches!(new_tail, Some((p, _)) if p.a.is_none());

        let extra_ms_dsize =
            extra_move_state.map_or(0, |_| 4 + crate::gstate::MOVE_STATE_BODY_SIZE);
        let extra_rs_dsize =
            extra_relocate_state.map_or(0, |_| 4 + crate::gstate::RELOCATE_STATE_BODY_SIZE);
        // A tail change on the append path emits one 4-byte tag + 8-byte
        // pair body; reserve that so `can_append` does not overflow.
        let extra_tail_dsize = new_tail.map_or(0, |_| 4 + 8);
        let dsize = op_dsize_of(op) + extra_ms_dsize + extra_rs_dsize + extra_tail_dsize;
        // Append in place only when the active block reports its next
        // window as erased (the FCRC matched the on-disk bytes) and that
        // window is prog-aligned. A non-erased window means a torn write
        // contaminated the cells past `committed_end`; appending there
        // would program over dirty NOR cells, so fall through to the
        // compact path, which rewrites onto a freshly erased block.
        let can_append = active_erased
            && committed_end % S::PROG_SIZE == 0
            && committed_end + dsize <= S::BLOCK_SIZE
            && !clear_tail;
        let mut did_append = false;
        if can_append {
            let active_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            let new_end = {
                let mut commit =
                    crate::meta::Commit::new_appending(active_buf, committed_end, next_ptag)?;
                emit_op(&mut commit, op)?;
                // Gstate tags carry the pair's new TOTAL contribution
                // (the C convention; review C4): fold the existing
                // total with the caller's delta. An all-zero result
                // is still emitted; it must shadow the prior non-zero
                // total under the reader's latest-tag-wins scan.
                if let Some(extra) = extra_move_state {
                    let mut body = pair_existing_ms;
                    for (n, e) in body.iter_mut().zip(extra.iter()) {
                        *n ^= *e;
                    }
                    commit.tag(
                        crate::tag::Tag::new(
                            true,
                            crate::tag::TagType::MoveState,
                            crate::tag::ID_NONE,
                            crate::gstate::MOVE_STATE_BODY_SIZE as u16,
                        ),
                        &body,
                    )?;
                }
                if let Some(extra) = extra_relocate_state {
                    let mut body = pair_existing_rs;
                    for (n, e) in body.iter_mut().zip(extra.iter()) {
                        *n ^= *e;
                    }
                    commit.tag(
                        crate::tag::Tag::new(
                            true,
                            crate::tag::TagType::RelocateState,
                            crate::tag::ID_NONE,
                            crate::gstate::RELOCATE_STATE_BODY_SIZE as u16,
                        ),
                        &body,
                    )?;
                }
                // Only an explicit tail change needs emitting on the
                // append path; the pair's prior tail tag persists in the
                // block (latest-wins) when `new_tail` is None.
                if let Some((tail_pair, is_hard)) = new_tail {
                    let mut body = [0u8; 8];
                    body[0..4].copy_from_slice(&tail_pair.a.as_u32().to_le_bytes());
                    body[4..8].copy_from_slice(&tail_pair.b.as_u32().to_le_bytes());
                    let tag_type = if is_hard {
                        crate::tag::TagType::HardTail
                    } else {
                        crate::tag::TagType::SoftTail
                    };
                    commit
                        .tag(crate::tag::Tag::new(true, tag_type, crate::tag::ID_NONE, 8), &body)?;
                }
                commit.finish_padded(0, S::PROG_SIZE, S::BLOCK_SIZE)?;
                commit.bytes_written()
            };
            // A failed in-place append means the active block is worn for
            // writes. Do not give up: fall through to the compact path, which
            // only reads the active block and rebuilds its live set elsewhere
            // (eagerly onto a fresh block, evicting the worn active — see the
            // `forced_victim` branch in `compact_and_program`). The live
            // entries `slots` reference sit below `committed_end`, which the
            // append never overwrites, so they stay valid for the compaction.
            // A program that returns Ok but lands corrupted bytes is
            // indistinguishable from success without the read-back
            // (review H2); a verify failure falls through to the same
            // worn-block eviction as a program failure. The verify
            // clobbers only `[committed_end..new_end)`, and the live
            // entries `slots` reference sit below `committed_end`.
            did_append = self
                .storage
                .program(
                    active_addr.as_u32(),
                    committed_end as u32,
                    &active_buf[committed_end..new_end],
                )
                .is_ok()
                && verify_programmed(
                    &mut self.storage,
                    active_addr,
                    committed_end,
                    &mut active_buf[committed_end..new_end],
                );
        }
        if !did_append {
            let new_revision = old_revision.wrapping_add(1);
            let mut net_ms = pair_existing_ms;
            if let Some(extra) = extra_move_state {
                for (n, e) in net_ms.iter_mut().zip(extra.iter()) {
                    *n ^= *e;
                }
            }
            let ms_arg = if net_ms == [0u8; crate::gstate::MOVE_STATE_BODY_SIZE] {
                None
            } else {
                Some(net_ms)
            };
            let mut net_rs = pair_existing_rs;
            if let Some(extra) = extra_relocate_state {
                for (n, e) in net_rs.iter_mut().zip(extra.iter()) {
                    *n ^= *e;
                }
            }
            let rs_arg = if net_rs == [0u8; crate::gstate::RELOCATE_STATE_BODY_SIZE] {
                None
            } else {
                Some(net_rs)
            };
            // When the append above failed, the active block is worn:
            // instruct the compaction to eagerly relocate the pair, evicting
            // the active block onto a fresh one. Otherwise a normal compaction.
            let forced_victim = if can_append { Some(active_addr) } else { None };
            let relocated = self.compact_and_program(
                pair_addr,
                active_addr,
                alternate_addr,
                active_is_a,
                new_revision,
                &slots,
                count,
                op,
                ms_arg,
                rs_arg,
                effective_tail,
                inflight,
                forced_victim,
                buf_a,
                buf_b,
            )?;
            if let Some(new_pair) = relocated {
                self.propagate_relocation(pair_addr, new_pair, inflight, buf_a, buf_b)?;
            }
        }
        self.storage.sync().map_err(|_| Error::Io)?;
        Ok(())
    }

    /// Build a compacted commit and program it durably, automatically
    /// relocating the pair to a freshly allocated block when the
    /// wear-levelling predicate fires.
    ///
    /// **Atomicity.** The compact bytes are always written to the
    /// existing alternate first. After that program completes, the
    /// caller's commit is reachable from the parent's unchanged
    /// reference (the alternate is now the highest-revision block of
    /// the pair). If the predicate then triggers a relocation, the
    /// same bytes are copied to a fresh block and the new pair address
    /// is returned. A crash after the alternate program but before the
    /// fresh program lands a non-relocated successful commit; a crash
    /// after the fresh program but before the caller's parent update
    /// leaves the new pair orphaned (reclaimed by the next allocator
    /// scan) and the FS still observes the new state via the old
    /// pair's freshly-programmed alternate.
    ///
    /// Returns `Ok(Some(new_pair))` when wear-levelling fired and the
    /// caller must propagate the new address to the parent;
    /// `Ok(None)` for the regular alternate-in-place compact.
    #[allow(clippy::too_many_arguments)]
    fn compact_and_program(
        &mut self,
        pair_addr: BlockPair,
        active_addr: BlockAddress,
        alternate_addr: BlockAddress,
        active_is_a: bool,
        new_revision: u32,
        slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
        count: usize,
        op: &WriteOp<'_>,
        ms_arg: Option<[u8; crate::gstate::MOVE_STATE_BODY_SIZE]>,
        rs_arg: Option<[u8; crate::gstate::RELOCATE_STATE_BODY_SIZE]>,
        tail: Option<(BlockPair, bool)>,
        inflight: Inflight<'_>,
        forced_victim: Option<BlockAddress>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<Option<BlockPair>, Error> {
        let total = count + usize::from(op_adds_entry(op));

        // `forced_victim` is set when an in-place append failed on a worn
        // active block: eagerly relocate the pair onto a fresh block,
        // rebuilding the (readable) active block's live set there and dropping
        // the worn block, exactly like the alternate-worn pivot but evicting
        // the active half. A single pair's live set always fits one block (it
        // splits at half-block during growth), so no split is needed. The
        // root pair is the fixed superblock anchor and cannot relocate.
        if let Some(victim) = forced_victim {
            if pair_addr == self.root {
                return Err(Error::Io);
            }
            let new_pair = self.relocate_compact_to_fresh(
                pair_addr,
                victim,
                active_is_a,
                new_revision,
                slots,
                count,
                op,
                0,
                total,
                ms_arg,
                rs_arg,
                tail,
                inflight,
                None,
                buf_a,
                buf_b,
            )?;
            return Ok(Some(new_pair));
        }

        // Would the compacted pair overflow the block? If so, split it
        // across a HardTail continuation instead of erroring. The split
        // index is computed from the live-entry sizes without any read.
        // The root pair `{0, 1}` is excluded here: it cannot relocate and
        // its growth needs the superblock-expansion guard, which lands in
        // a later increment (lfs-cvh.5); until then a root overflow keeps
        // its prior `Error::OutOfRange` behavior via the normal path.
        // The compacted live set exceeds half a block, so a split would
        // distribute it. The root pair `{0, 1}` splits like any other
        // directory — its superblock entry at id 0 always falls in the
        // lower half, so `{0, 1}` stays the mount anchor with a HardTail to
        // the continuation, and it never relocates (the lower-half commit
        // keeps its address) — but with an extra fullness guard below,
        // because a root continuation chain cannot be reclaimed. Splitting
        // degrades gracefully when it cannot proceed.
        let split = {
            let source_buf: &[u8] = if active_is_a { &*buf_a } else { &*buf_b };
            compute_split_index::<S>(source_buf, slots, count, op, 0, total)?
        };
        let mut do_split = split > 0;
        if do_split {
            // Bound the reachable-pair count before splitting. The split
            // adds exactly one reachable pair (the continuation); the
            // mount-time walks (allocator scan, gstate accumulation,
            // deorphan) enumerate the forest into fixed `MAX_QUEUED_PAIRS`
            // arrays (ADR-0006), so a forest of more than `MAX_QUEUED_PAIRS`
            // reachable pairs is unmountable. At the budget, do not split:
            // fall through to a single-block compaction (degraded), which
            // still fits when the live set is at most one block and only
            // a genuine overflow turns into `OutOfRange` below.
            //
            // `collect_live_tree_pairs` clobbers both scratch buffers, so
            // re-read the active block afterward to restore the bytes
            // `slots` reference.
            let mut tree = [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE);
                crate::alloc::MAX_QUEUED_PAIRS];
            let reachable =
                collect_live_tree_pairs(&mut self.storage, self.root, &mut tree, buf_a, buf_b)?;
            let source_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            self.storage.read(active_addr.as_u32(), 0, source_buf).map_err(|_| Error::Io)?;
            if reachable >= crate::alloc::MAX_QUEUED_PAIRS {
                do_split = false;
            }
        }
        if do_split && pair_addr == self.root {
            // Superblock-expansion fullness guard (the C reference's
            // `lfs_dir_splittingcompact` check). The root cannot be
            // un-split, so a root continuation pair is dedicated forever;
            // growing the chain when the device is nearly full would
            // permanently consume the last free blocks and starve future
            // writes. Do not split the root once free space drops to an
            // eighth of the device — degrade to a single-block compaction
            // (a root overflow then keeps its prior `OutOfRange`).
            // `scan_used_blocks` clobbers both buffers; restore the source.
            let mut used = crate::alloc::Bitmap::EMPTY;
            crate::alloc::scan_used_blocks(&mut self.storage, self.root, &mut used, buf_a, buf_b)?;
            let source_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
            self.storage.read(active_addr.as_u32(), 0, source_buf).map_err(|_| Error::Io)?;
            let free = (0..S::BLOCK_COUNT).filter(|&blk| !used.is_set(blk)).count();
            if free <= (S::BLOCK_COUNT as usize) / 8 {
                do_split = false;
            }
        }
        if do_split {
            // Split. Wear-levelling relocation is a hint, not a correctness
            // requirement; defer it to the next commit rather than fold a
            // relocation's fresh allocation and parent update into the same
            // crash window as the split. The original keeps its address
            // (lower half + a HardTail to the continuation), so there is
            // nothing to propagate to the parent — `Ok(None)`.
            //
            // If no free pair is available for the continuation, degrade to
            // a single-block compaction (the C reference's "unable to
            // split" fallback): a removal or update that shrunk the live
            // set back within one block still lands, while a true overflow
            // surfaces as `OutOfRange` from the compaction below. The
            // continuation allocation clobbers only the build buffer, but
            // re-read the source to be safe before the fallback.
            match self.split_directory_pair(
                pair_addr,
                active_addr,
                alternate_addr,
                active_is_a,
                new_revision,
                slots,
                count,
                op,
                split,
                total,
                tail,
                ms_arg,
                rs_arg,
                inflight,
                buf_a,
                buf_b,
            ) {
                // `Some(new_pair)` when the split had to relocate the original
                // (its lower-half write hit a worn alternate); the caller
                // propagates that to the parent like any pair relocation.
                Ok(opt) => return Ok(opt),
                Err(Error::OutOfRange) => {
                    let source_buf: &mut [u8] = if active_is_a { buf_a } else { buf_b };
                    self.storage
                        .read(active_addr.as_u32(), 0, source_buf)
                        .map_err(|_| Error::Io)?;
                }
                Err(e) => return Err(e),
            }
        }

        let relocating = should_relocate(pair_addr, self.root, new_revision, S::BLOCK_CYCLES);

        // When relocating, allocate the fresh destination block FIRST
        // so the relocation's `RelocateState` body — which encodes
        // `(pair_addr, new_pair)` — can be embedded in both the
        // alternate commit AND the fresh-block commit. A crash with
        // the alternate programmed but the fresh not yet leaves a
        // `RelocateState` reachable through the parent's (unchanged)
        // reference, so mount-time recovery can detect the half-done
        // cycle and cancel it.
        //
        // Buffer hygiene: the allocator's single-buffer BFS scan
        // clobbers whatever buffer we hand it. We pass the
        // alt-buffer (the one we're about to overwrite with the new
        // compact bytes anyway), keeping the source-buffer intact
        // so `slots[..count]`'s offsets remain valid.
        let (fresh_opt, relocate_event_body) = if relocating {
            let mut excluded = [BlockAddress::NONE; 2 + crate::alloc::MAX_QUEUED_PAIRS];
            excluded[0] = active_addr;
            excluded[1] = alternate_addr;
            let mut ex_len = 2;
            for &b in inflight.blocks {
                if ex_len >= excluded.len() {
                    return Err(Error::OutOfRange);
                }
                excluded[ex_len] = b;
                ex_len += 1;
            }
            let fresh = if active_is_a {
                crate::alloc::alloc_one_block_cached_single_buf(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..ex_len],
                    inflight.chain,
                    buf_b,
                )?
            } else {
                crate::alloc::alloc_one_block_cached_single_buf(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..ex_len],
                    inflight.chain,
                    buf_a,
                )?
            };
            let new_pair = BlockPair::new(
                if pair_addr.a == alternate_addr { fresh } else { pair_addr.a },
                if pair_addr.b == alternate_addr { fresh } else { pair_addr.b },
            );
            let body = crate::gstate::build_relocate_body(pair_addr, new_pair);
            (Some((fresh, new_pair)), Some(body))
        } else {
            (None, None)
        };

        // Combine the caller's pre-existing rs contribution with the
        // current relocation's body (if any) into a single net body
        // for the compact commit.
        let combined_rs = match (rs_arg, relocate_event_body) {
            (None, None) => None,
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (Some(a), Some(b)) => {
                let mut out = a;
                for (o, e) in out.iter_mut().zip(b.iter()) {
                    *o ^= *e;
                }
                if out == [0u8; crate::gstate::RELOCATE_STATE_BODY_SIZE] {
                    None
                } else {
                    Some(out)
                }
            }
        };

        // The whole live set plus the new entry fits one block (`split`
        // was 0): compact the full combined range `[0, total)`.
        let new_end = if active_is_a {
            build_compact_commit(
                buf_b,
                buf_a,
                new_revision,
                slots,
                count,
                op,
                0,
                total,
                S::PROG_SIZE,
                S::BLOCK_SIZE,
                ms_arg,
                combined_rs,
                tail,
            )?
        } else {
            build_compact_commit(
                buf_a,
                buf_b,
                new_revision,
                slots,
                count,
                op,
                0,
                total,
                S::PROG_SIZE,
                S::BLOCK_SIZE,
                ms_arg,
                combined_rs,
                tail,
            )?
        };

        let alt_bytes_len = new_end;
        let alt_write_ok = {
            let alt_buf: &mut [u8] = if active_is_a { buf_b } else { buf_a };
            self.storage.erase(alternate_addr.as_u32()).is_ok()
                && self
                    .storage
                    .program(alternate_addr.as_u32(), 0, &alt_buf[..alt_bytes_len])
                    .is_ok()
                // Review H2: a silently corrupted program must take the
                // worn-alternate relocation below, not report success.
                // On mismatch the build buffer is clobbered with disk
                // bytes; `relocate_compact_to_fresh` rebuilds it from
                // the untouched source buffer.
                && verify_programmed(
                    &mut self.storage,
                    alternate_addr,
                    0,
                    &mut alt_buf[..alt_bytes_len],
                )
        };

        if !alt_write_ok {
            // The alternate is a worn block (sub-case 1, or a wear-scheduled
            // relocation whose alternate happens to be worn). There is no
            // in-place anchor to fall back on, so relocate the pair directly
            // onto a fresh block: the new commit lands only on the fresh
            // block (unreachable until the parent's `DirStruct` repoints at
            // it), so a crash before that repoint mounts as the pre-commit
            // state (the good active half out-revisions the blank/worn
            // alternate) and the fresh block is reclaimed as an orphan. The
            // root pair `{0, 1}` is the fixed superblock anchor and cannot
            // relocate, so a worn root commit stays fatal.
            if pair_addr == self.root {
                return Err(Error::Io);
            }
            // Reuse a wear-allocated fresh block as the first candidate so a
            // wear relocation whose alternate is worn does not allocate (and
            // orphan) two fresh blocks.
            let seed = fresh_opt.map(|(f, _)| f);
            let new_pair = self.relocate_compact_to_fresh(
                pair_addr,
                alternate_addr,
                active_is_a,
                new_revision,
                slots,
                count,
                op,
                0,
                total,
                ms_arg,
                rs_arg,
                tail,
                inflight,
                seed,
                buf_a,
                buf_b,
            )?;
            return Ok(Some(new_pair));
        }

        let Some((fresh, new_pair)) = fresh_opt else {
            return Ok(None);
        };

        // Wear relocation: copy the same bytes (already carrying the relocate
        // body) to the fresh block. If the fresh block is itself worn, the
        // anchor on the alternate already holds a durable commit, so abandon
        // the relocation this cycle. Wear levelling is opportunistic and
        // re-fires at the next cycle.
        let fresh_write_ok = {
            let alt_buf: &mut [u8] = if active_is_a { buf_b } else { buf_a };
            self.storage.erase(fresh.as_u32()).is_ok()
                && self.storage.program(fresh.as_u32(), 0, &alt_buf[..alt_bytes_len]).is_ok()
                // Review H2: a corrupted fresh copy must abandon the
                // relocation (the alternate already anchors a durable,
                // verified commit), not propagate a bad pair address.
                // On mismatch the buffer is clobbered; the cancel path
                // below re-reads the pair from storage before use.
                && verify_programmed(&mut self.storage, fresh, 0, &mut alt_buf[..alt_bytes_len])
        };
        if fresh_write_ok {
            Ok(Some(new_pair))
        } else {
            // Cancel the abandoned relocation's `RelocateState`
            // immediately (review H3). The anchor commit durably
            // carries the unbalanced `(pair_addr, new_pair)` body;
            // deferring the cancel to mount-time recovery leaves a
            // window where a LATER successful relocation of this pair
            // outdates both addresses, and recovery then commits
            // through a dead address on every mount, forever. The
            // cancelling delta folds the body back out of the pair's
            // total in a follow-up commit on the same (unchanged) pair
            // address; a crash before it lands keeps today's recovery
            // path (the pair has not moved, so the decoded addresses
            // are live). The recursion through `apply_op_to_pair_inner`
            // is the standard commit path (it may compact, and that
            // compact may itself relocate, which is the normal nested
            // cascade bounded like any other commit).
            let body =
                relocate_event_body.expect("a wear relocation attempt always has a relocate body");
            self.apply_op_to_pair_inner(
                pair_addr,
                &WriteOp::Noop,
                None,
                Some(body),
                None,
                inflight,
                buf_a,
                buf_b,
            )?;
            Ok(None)
        }
    }

    /// Relocate a metadata pair onto a freshly allocated block when a commit
    /// cannot write one of the pair's own blocks (a worn / bad block). The
    /// compacted bytes (carrying a balanced `RelocateState` body for the
    /// `pair_addr -> new_pair` migration) are programmed ONLY to the fresh
    /// block, replacing `victim_addr` (the worn block) in the pair while
    /// keeping the other (good) block. The caller must then propagate the
    /// returned `new_pair` to the parent via [`Self::propagate_relocation`].
    ///
    /// **Crash-safety (fresh-only, no in-place anchor).** Unlike the
    /// wear-levelling path, the commit is never written to one of the pair's
    /// current blocks, so until the parent repoints at `new_pair` the fresh
    /// block is unreferenced and the pair reads as its pre-commit state (the
    /// kept good block out-revisions the blank/worn victim, which has no
    /// verified CCRC and so loses the active-block selection). The parent
    /// repoint is the sole linearization point: a crash before it mounts as
    /// the pre-state with the fresh block reclaimed as an orphan; a crash
    /// after it mounts as the post-state. The reachable `RelocateState`
    /// aggregate is therefore always zero (no body reachable before the
    /// repoint) or balanced (fresh + parent after it) — never the single
    /// unbalanced body that would trip mount-time relocation recovery, which
    /// must not fire here because it would try to commit onto the worn pair.
    ///
    /// `range_start..range_end` selects the live-entry range to compact (the
    /// full `0..total` for a plain compaction, or `0..split` for the lower
    /// half of a directory split, where `tail` carries the `HardTail` to the
    /// continuation). `seed_fresh`, when `Some`, is tried as the first
    /// destination before allocating (used to reuse a wear-allocated fresh).
    /// Bounded by [`MAX_BAD_BLOCK_RETRIES`]; exhaustion returns
    /// [`Error::Io`], never an infinite loop.
    #[allow(clippy::too_many_arguments)]
    fn relocate_compact_to_fresh(
        &mut self,
        pair_addr: BlockPair,
        victim_addr: BlockAddress,
        active_is_a: bool,
        new_revision: u32,
        slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
        count: usize,
        op: &WriteOp<'_>,
        range_start: usize,
        range_end: usize,
        ms_arg: Option<[u8; crate::gstate::MOVE_STATE_BODY_SIZE]>,
        rs_arg: Option<[u8; crate::gstate::RELOCATE_STATE_BODY_SIZE]>,
        tail: Option<(BlockPair, bool)>,
        inflight: Inflight<'_>,
        seed_fresh: Option<BlockAddress>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<BlockPair, Error> {
        // Excluded set: the pair's own two blocks, every inflight block from a
        // parent relocation cascade, and every worn fresh candidate tried so
        // far. Sized for the worst case with bound checks → `OutOfRange`.
        let mut excluded =
            [BlockAddress::NONE; 2 + 2 + crate::alloc::MAX_QUEUED_PAIRS + MAX_BAD_BLOCK_RETRIES];
        excluded[0] = pair_addr.a;
        excluded[1] = pair_addr.b;
        let mut base_len = 2;
        for &blk in inflight.blocks {
            if base_len >= excluded.len() {
                return Err(Error::OutOfRange);
            }
            excluded[base_len] = blk;
            base_len += 1;
        }
        let mut ex_len = base_len;
        let mut seed = seed_fresh;
        let mut tries = 0usize;
        loop {
            let fresh = match seed.take() {
                Some(s) => s,
                None => {
                    // The allocator's single-buffer BFS scan clobbers the
                    // build buffer (the one we are about to rebuild the commit
                    // into); the source buffer holding the live entries stays
                    // intact so `slots` remain valid.
                    if active_is_a {
                        crate::alloc::alloc_one_block_cached_single_buf(
                            &mut self.storage,
                            self.root,
                            &mut self.used_cache,
                            &excluded[..ex_len],
                            inflight.chain,
                            buf_b,
                        )?
                    } else {
                        crate::alloc::alloc_one_block_cached_single_buf(
                            &mut self.storage,
                            self.root,
                            &mut self.used_cache,
                            &excluded[..ex_len],
                            inflight.chain,
                            buf_a,
                        )?
                    }
                }
            };
            let new_pair = BlockPair::new(
                if pair_addr.a == victim_addr { fresh } else { pair_addr.a },
                if pair_addr.b == victim_addr { fresh } else { pair_addr.b },
            );
            // Fold the relocation's `RelocateState` body into the caller's
            // pre-existing contribution. The body is non-zero (it encodes two
            // distinct pairs), so the result is `Some` unless `rs_arg`
            // exactly cancels it.
            let relocate_body = crate::gstate::build_relocate_body(pair_addr, new_pair);
            let combined_rs = match rs_arg {
                None => Some(relocate_body),
                Some(a) => {
                    let mut out = a;
                    for (o, e) in out.iter_mut().zip(relocate_body.iter()) {
                        *o ^= *e;
                    }
                    if out == [0u8; crate::gstate::RELOCATE_STATE_BODY_SIZE] {
                        None
                    } else {
                        Some(out)
                    }
                }
            };
            // Rebuild the commit AFTER the alloc (which clobbered the build
            // buffer) so the bytes reflect this candidate's relocate body.
            let new_end = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    new_revision,
                    slots,
                    count,
                    op,
                    range_start,
                    range_end,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                    ms_arg,
                    combined_rs,
                    tail,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    new_revision,
                    slots,
                    count,
                    op,
                    range_start,
                    range_end,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                    ms_arg,
                    combined_rs,
                    tail,
                )?
            };
            let write_ok = {
                let build_buf: &mut [u8] = if active_is_a { buf_b } else { buf_a };
                self.storage.erase(fresh.as_u32()).is_ok()
                    && self.storage.program(fresh.as_u32(), 0, &build_buf[..new_end]).is_ok()
                    // Review H2: a fresh candidate that corrupts its
                    // bytes silently is as worn as one that errors;
                    // the retry below excludes it and rebuilds the
                    // (now clobbered) build buffer for the next one.
                    && verify_programmed(&mut self.storage, fresh, 0, &mut build_buf[..new_end])
            };
            if write_ok {
                // The fresh block is now in use; drop the stale lookahead.
                self.used_cache = None;
                return Ok(new_pair);
            }
            // The fresh candidate is itself worn: exclude it and try another,
            // bounded so a wholly-failing device cannot loop forever.
            self.used_cache = None;
            tries += 1;
            if tries >= MAX_BAD_BLOCK_RETRIES || ex_len >= excluded.len() {
                return Err(Error::Io);
            }
            excluded[ex_len] = fresh;
            ex_len += 1;
        }
    }

    /// Split an overflowing directory pair across a freshly allocated
    /// `HardTail` continuation, matching the C reference's `lfs_dir_split`.
    /// The combined entry sequence is cut at `split` (computed by
    /// [`compute_split_index`]): the upper portion `[split, total)` moves
    /// to the continuation, the lower portion `[0, split)` stays in the
    /// original pair, now ending in a `HardTail` to the continuation.
    ///
    /// **Ordering and crash-safety.** The continuation is allocated and
    /// fully programmed *before* the original's lower-half commit lands.
    /// Until that commit, the continuation is referenced by nothing (the
    /// original's active block still holds its pre-split tags with no
    /// HardTail), so a crash leaves it an unreferenced orphan reclaimed by
    /// the next allocator scan — exactly the `mkdir` create window. After
    /// the lower-half commit lands on the original's alternate (higher
    /// revision, CCRC-valid), the directory reads as the post-split state:
    /// the original's lower entries followed by the continuation's upper
    /// entries (which include the new entry for a Create), concatenated by
    /// `list_pair_chain` across the HardTail.
    ///
    /// **gstate** stays on the original (lower) pair; the continuation is
    /// brand new and carries a zero contribution. The continuation
    /// inherits the original's prior tail so the global thread (and any
    /// further continuation) stays linked.
    ///
    /// **Worn blocks.** A split writes a fresh continuation block and the
    /// original's alternate, either of which may be worn. A worn continuation
    /// block is relocated past in place: the continuation is unreferenced
    /// until the lower-half commit, so a failed write is a clean blank orphan
    /// — exclude it and reallocate the pair (bounded by
    /// [`MAX_BAD_BLOCK_RETRIES`]). A worn *alternate* on the lower-half write
    /// relocates the original onto a fresh block via
    /// [`Self::relocate_compact_to_fresh`], with the lower half still
    /// carrying the `HardTail` to the continuation; that returns the new pair
    /// address (`Ok(Some(new_pair))`) so the caller repoints the parent. The
    /// root pair cannot relocate, so a worn root alternate stays
    /// [`Error::Io`]. A normal split returns `Ok(None)`.
    ///
    /// One split always suffices in this writer: each metadata pair fits
    /// one block and each `WriteOp` adds at most one entry, so the combined
    /// sequence is at most one block plus one entry, which a single cut
    /// splits into two sub-block pairs (the upper bounded to half a block
    /// by `compute_split_index`, the lower being the pre-existing entries).
    /// A multi-pair directory grows by repeatedly splitting the last pair
    /// as it fills. The within-compaction cascade of the C reference's
    /// `lfs_dir_splittingcompact` (reachable only when one commit batches
    /// several creates) is therefore unreachable here; see ADR-0013. A
    /// lower portion that somehow still exceeded the block would surface as
    /// the pre-existing `Error::OutOfRange` from `build_compact_commit`.
    #[allow(clippy::too_many_arguments)]
    fn split_directory_pair(
        &mut self,
        pair_addr: BlockPair,
        active_addr: BlockAddress,
        alternate_addr: BlockAddress,
        active_is_a: bool,
        new_revision: u32,
        slots: &[SlotOffsets; MAX_LIVE_ENTRIES],
        count: usize,
        op: &WriteOp<'_>,
        split: usize,
        total: usize,
        inherited_tail: Option<(BlockPair, bool)>,
        ms_arg: Option<[u8; crate::gstate::MOVE_STATE_BODY_SIZE]>,
        rs_arg: Option<[u8; crate::gstate::RELOCATE_STATE_BODY_SIZE]>,
        inflight: Inflight<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<Option<BlockPair>, Error> {
        // Excluded set for the continuation allocation: the pair's own blocks,
        // any inflight blocks, and every worn continuation candidate tried.
        // The build buffer is the allocator's single-buffer scan scratch, so
        // the source buffer (holding the bytes `slots` point into) is never
        // clobbered. One extra slot holds the first-allocated block tentatively
        // while the second is allocated.
        let mut excluded =
            [BlockAddress::NONE; 2 + crate::alloc::MAX_QUEUED_PAIRS + MAX_BAD_BLOCK_RETRIES + 1];
        excluded[0] = active_addr;
        excluded[1] = alternate_addr;
        let mut base = 2;
        for &b in inflight.blocks {
            if base >= excluded.len() {
                return Err(Error::OutOfRange);
            }
            excluded[base] = b;
            base += 1;
        }

        // Allocate + program the continuation, relocating past worn
        // continuation blocks. The continuation is unreferenced until the
        // lower-half commit, so an attempt that hits a worn block leaves a
        // blank/unreferenced orphan: exclude the worn block and retry.
        let mut ex = base;
        let mut tries = 0usize;
        let cont = loop {
            let ca = if active_is_a {
                crate::alloc::alloc_one_block_cached_single_buf(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..ex],
                    inflight.chain,
                    buf_b,
                )?
            } else {
                crate::alloc::alloc_one_block_cached_single_buf(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..ex],
                    inflight.chain,
                    buf_a,
                )?
            };
            // Hold `ca` in the scratch slot at `ex` so the second allocation
            // does not pick it; the slot is overwritten (with the worn block)
            // only if this attempt fails.
            if ex >= excluded.len() {
                return Err(Error::OutOfRange);
            }
            excluded[ex] = ca;
            let cb = if active_is_a {
                crate::alloc::alloc_one_block_cached_single_buf(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..=ex],
                    inflight.chain,
                    buf_b,
                )?
            } else {
                crate::alloc::alloc_one_block_cached_single_buf(
                    &mut self.storage,
                    self.root,
                    &mut self.used_cache,
                    &excluded[..=ex],
                    inflight.chain,
                    buf_a,
                )?
            };
            let cont = BlockPair::new(ca, cb);

            // Build + program the continuation: the upper portion at revision
            // 1, inheriting the original's prior tail, carrying no gstate.
            // Rebuilt each attempt (the alloc clobbered the build buffer);
            // the bytes are identical since the continuation's tail does not
            // reference its own address.
            let cont_len = if active_is_a {
                build_compact_commit(
                    buf_b,
                    buf_a,
                    1,
                    slots,
                    count,
                    op,
                    split,
                    total,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                    None,
                    None,
                    inherited_tail,
                )?
            } else {
                build_compact_commit(
                    buf_a,
                    buf_b,
                    1,
                    slots,
                    count,
                    op,
                    split,
                    total,
                    S::PROG_SIZE,
                    S::BLOCK_SIZE,
                    None,
                    None,
                    inherited_tail,
                )?
            };
            let a_ok = {
                let build_buf: &mut [u8] = if active_is_a { buf_b } else { buf_a };
                self.storage.erase(ca.as_u32()).is_ok()
                    && self.storage.program(ca.as_u32(), 0, &build_buf[..cont_len]).is_ok()
                    // Review H2: an unverified continuation would be
                    // linearized into the chain by the lower-half
                    // commit below. The retry rebuilds the buffer.
                    && verify_programmed(&mut self.storage, ca, 0, &mut build_buf[..cont_len])
            };
            // Block B stays erased as the continuation's alternate so a stale
            // image there cannot masquerade as a newer revision.
            let b_ok = a_ok && self.storage.erase(cb.as_u32()).is_ok();
            if a_ok && b_ok {
                break cont;
            }
            // A continuation block is worn: exclude it and retry. The other
            // (good, possibly already-written) block is abandoned as a
            // blank/unreferenced orphan and reclaimed by the next scan.
            self.used_cache = None;
            tries += 1;
            if tries >= MAX_BAD_BLOCK_RETRIES {
                return Err(Error::Io);
            }
            let worn = if a_ok { cb } else { ca };
            excluded[ex] = worn;
            ex += 1;
        };

        // Make the continuation durable before the linearizing commit, so
        // a reordering device cannot expose a HardTail to a not-yet-written
        // continuation (mirrors mkdir's sync between the new dir and the
        // parent commit).
        self.storage.sync().map_err(|_| Error::Io)?;

        // Build + program the original's lower half: at `new_revision`,
        // ending in a HardTail (split bit set) to the continuation, carrying
        // the original's gstate. Programming this block (higher revision,
        // CCRC-valid) is the split's linearization point — the moment the
        // continuation becomes reachable.
        let low_len = if active_is_a {
            build_compact_commit(
                buf_b,
                buf_a,
                new_revision,
                slots,
                count,
                op,
                0,
                split,
                S::PROG_SIZE,
                S::BLOCK_SIZE,
                ms_arg,
                rs_arg,
                Some((cont, true)),
            )?
        } else {
            build_compact_commit(
                buf_a,
                buf_b,
                new_revision,
                slots,
                count,
                op,
                0,
                split,
                S::PROG_SIZE,
                S::BLOCK_SIZE,
                ms_arg,
                rs_arg,
                Some((cont, true)),
            )?
        };
        let low_ok = {
            let build_buf: &mut [u8] = if active_is_a { buf_b } else { buf_a };
            self.storage.erase(alternate_addr.as_u32()).is_ok()
                && self.storage.program(alternate_addr.as_u32(), 0, &build_buf[..low_len]).is_ok()
                // Review H2: this program is the split's linearization
                // point; a silent corruption here must divert to the
                // worn-alternate relocation, which rebuilds the buffer.
                && verify_programmed(
                    &mut self.storage,
                    alternate_addr,
                    0,
                    &mut build_buf[..low_len],
                )
        };
        if low_ok {
            // A continuation born here is a new reachable pair; drop the
            // lookahead so its blocks are not handed out again.
            self.used_cache = None;
            return Ok(None);
        }

        // The alternate is worn: relocate the original onto a fresh block.
        // The lower half (still carrying the HardTail to the continuation) is
        // written only to the fresh block; the parent repoint linearizes the
        // split, exactly like the plain-compaction relocation. The root pair
        // is the fixed superblock anchor and cannot relocate.
        if pair_addr == self.root {
            return Err(Error::Io);
        }
        // The continuation blocks are allocated but not yet reachable; keep
        // the relocation's fresh allocation off them.
        let mut reloc_inflight = [BlockAddress::NONE; 2 + crate::alloc::MAX_QUEUED_PAIRS];
        reloc_inflight[0] = cont.a;
        reloc_inflight[1] = cont.b;
        let mut rl = 2;
        for &b in inflight.blocks {
            if rl >= reloc_inflight.len() {
                return Err(Error::OutOfRange);
            }
            reloc_inflight[rl] = b;
            rl += 1;
        }
        let new_pair = self.relocate_compact_to_fresh(
            pair_addr,
            alternate_addr,
            active_is_a,
            new_revision,
            slots,
            count,
            op,
            0,
            split,
            ms_arg,
            rs_arg,
            Some((cont, true)),
            Inflight { blocks: &reloc_inflight[..rl], chain: inflight.chain },
            None,
            buf_a,
            buf_b,
        )?;
        self.used_cache = None;
        Ok(Some(new_pair))
    }

    /// Propagate a pair relocation up the tree: find the parent that
    /// references `old_pair` via a `DirStruct` tag and rewrite that
    /// tag's body to point at `new_pair`. The parent commit itself
    /// flows through the standard compact-or-append dispatch and may
    /// recursively trigger another relocation; recursion terminates
    /// at the root pair (which never relocates) or when a parent
    /// commit fits inline.
    fn propagate_relocation(
        &mut self,
        old_pair: BlockPair,
        new_pair: BlockPair,
        inflight: Inflight<'_>,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        if old_pair == self.root {
            return Ok(());
        }
        // Remap the in-flight rename's source coordinates (review C6):
        // if the pair that just relocated is the pending move's source,
        // the source delete (and anything else consuming these
        // coordinates) must target the new address. Without this, the
        // delete lands on the orphaned old address: a permanent
        // duplicate entry whose MoveState can never cancel.
        if let Some(pm) = self.pending_move.as_mut() {
            if pm.cur_pair == old_pair {
                pm.cur_pair = new_pair;
            }
        }
        let relocate_body = crate::gstate::build_relocate_body(old_pair, new_pair);
        // Add the fresh half of new_pair to inflight so a cascading
        // relocation of the pair we are about to commit to does not
        // reallocate the block we just programmed.
        let fresh = if old_pair.a == new_pair.a { new_pair.b } else { new_pair.a };
        let mut next_inflight = [BlockAddress::NONE; crate::alloc::MAX_QUEUED_PAIRS];
        let mut next_len = 0;
        for &b in inflight.blocks {
            if next_len >= next_inflight.len() {
                return Err(Error::OutOfRange);
            }
            next_inflight[next_len] = b;
            next_len += 1;
        }
        if next_len >= next_inflight.len() {
            return Err(Error::OutOfRange);
        }
        next_inflight[next_len] = fresh;
        next_len += 1;
        // The relocated pair changed address, so its thread predecessor's
        // tail must be re-pointed at `new_pair` to keep the global
        // metadata-pair list consistent (the C reference's allocator and
        // traverse follow it).
        let pred = find_thread_predecessor(&mut self.storage, self.root, old_pair, buf_a, buf_b)?;
        if let Some((parent_pair, parent_id)) =
            find_parent_in_tree(&mut self.storage, self.root, old_pair, buf_a, buf_b)?
        {
            // Tree node: a parent holds a `DirStruct` to `old_pair`.
            // Rewrite it to `new_pair`. When the thread predecessor IS the
            // parent (the common case for a directory's first/only child),
            // fold the tail update into the same commit so it lands
            // atomically; otherwise re-point the predecessor's tail in a
            // separate commit.
            let op = WriteOp::UpdateDirStruct { id: parent_id, new_pair };
            let parent_new_tail = match pred {
                Some((p, is_hard)) if p == parent_pair => Some((new_pair, is_hard)),
                _ => None,
            };
            self.apply_op_to_pair_inner(
                parent_pair,
                &op,
                None,
                Some(relocate_body),
                parent_new_tail,
                Inflight { blocks: &next_inflight[..next_len], chain: inflight.chain },
                buf_a,
                buf_b,
            )?;
            if let Some((pred_pair, is_hard)) = pred {
                if pred_pair != parent_pair {
                    self.apply_op_to_pair_inner(
                        pred_pair,
                        &WriteOp::Noop,
                        None,
                        None,
                        Some((new_pair, is_hard)),
                        Inflight::NONE,
                        buf_a,
                        buf_b,
                    )?;
                }
            }
        } else {
            // HardTail continuation: no parent holds a `DirStruct` to it —
            // it is reached only through its thread predecessor's HardTail.
            // Re-point that HardTail at `new_pair` and carry the
            // relocation's gstate on the same commit, so mount-time
            // recovery balances it. A continuation with no predecessor is
            // genuinely orphaned (corrupt or already dropped).
            let (pred_pair, is_hard) = pred.ok_or(Error::Corrupt)?;
            self.apply_op_to_pair_inner(
                pred_pair,
                &WriteOp::Noop,
                None,
                Some(relocate_body),
                Some((new_pair, is_hard)),
                Inflight { blocks: &next_inflight[..next_len], chain: inflight.chain },
                buf_a,
                buf_b,
            )?;
        }
        Ok(())
    }

    /// Drop `dropped` from the global metadata-pair thread: re-point
    /// `pred_pair`'s tail past it (to the dropped pair's own tail, or
    /// clear the tail at the list end), **stealing** the dropped
    /// pair's gstate contributions into the same commit.
    ///
    /// This is the C reference's `lfs_dir_drop` (`lfs.c:1831`,
    /// commented `// steal state`), and the steal is load-bearing
    /// (review C7): a pair can carry a non-zero `MoveState` total that
    /// is balanced globally (a completed rename out of the directory
    /// leaves equal totals on source and destination). Dropping the
    /// pair without folding its total into the survivor leaves the
    /// reachable aggregate permanently non-zero; every subsequent
    /// mount then decodes a pending move against the dead pair, and
    /// once that pair's content no longer covers the decoded id the
    /// image fails to mount at all. Routing every thread-drop through
    /// this helper makes the steal structural (review D5): a future
    /// drop site cannot forget it.
    ///
    /// The steal rides the un-thread commit, which is the moment the
    /// pair leaves the reachable set; a crash before it leaves the
    /// pair thread-reachable (its contribution still counted), a
    /// crash after it has the contribution already folded into
    /// `pred_pair`. Both gstate kinds are stolen; the same argument
    /// applies to `RelocateState`.
    fn unthread_and_steal(
        &mut self,
        pred_pair: BlockPair,
        dropped: BlockPair,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.storage.read(dropped.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(dropped.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        let (dropped_tail, stolen_ms, stolen_rs) = {
            let p = MetadataPair::parse(dropped.a, &*buf_a, dropped.b, &*buf_b)?;
            (
                p.reader.tail().map(|t| (t, p.reader.is_hard_tail())),
                scan_pair_move_state(&p),
                scan_pair_relocate_state(&p),
            )
        };
        let new_tail =
            dropped_tail.or(Some((BlockPair::new(BlockAddress::NONE, BlockAddress::NONE), false)));
        let ms = (stolen_ms != [0u8; crate::gstate::MOVE_STATE_BODY_SIZE]).then_some(stolen_ms);
        let rs = (stolen_rs != [0u8; crate::gstate::RELOCATE_STATE_BODY_SIZE]).then_some(stolen_rs);
        self.apply_op_to_pair_inner(
            pred_pair,
            &WriteOp::Noop,
            ms,
            rs,
            new_tail,
            Inflight::NONE,
            buf_a,
            buf_b,
        )
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

        // Resolve the entry across the parent's HardTail chain; validate
        // it's a Directory; grab its pair. `owner` is the pair that holds
        // the subdirectory entry (the parent's first pair for a
        // single-pair directory).
        let owner = match self.seek_entry_in_chain(parent, leaf.as_bytes(), buf_a, buf_b)? {
            ChainSeek::Found { pair, kind, .. } => {
                if kind != crate::dir::EntryKind::Directory {
                    return Err(Error::AlreadyExists);
                }
                pair
            }
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        };
        let dir_pair = {
            let p = MetadataPair::parse(owner.a, &*buf_a, owner.b, &*buf_b)?;
            let resolved = crate::dir::lookup(&p, leaf.as_bytes()).ok_or(Error::NotFound)?;
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

        // Verify the directory is empty. A directory may be threaded
        // across HardTail continuation pairs; counting only the first
        // pair would wrongly accept rmdir of a directory whose entries
        // live in a continuation pair. This writer never emits
        // HardTail tags, so single-pair directories are the common
        // case, but an image written by the C reference (or a future
        // chaining writer) can have them. Walk the whole chain.
        let live_count = self.list_pair_chain(dir_pair, |_| {}, buf_a, buf_b)?;
        if live_count > 0 {
            return Err(Error::NotEmpty);
        }

        // Remove the entry from the parent (drops the directory from the
        // tree). Then un-thread it: re-point the thread predecessor's tail
        // past the removed pair so the global list stays consistent for
        // the C reference, stealing the dropped pair's gstate
        // contribution into the same commit (review C7; the C
        // reference's `lfs_dir_drop` "steal state"). A crash between
        // the two leaves the pair in the thread but not the tree (an
        // orphan), which mount-time deorphan recovery reconciles with
        // the same stealing drop.
        self.remove_from_pair(parent, leaf.as_bytes(), buf_a, buf_b)?;
        if let Some((pred_pair, _is_hard)) =
            find_thread_predecessor(&mut self.storage, self.root, dir_pair, buf_a, buf_b)?
        {
            self.unthread_and_steal(pred_pair, dir_pair, buf_a, buf_b)?;
        }
        Ok(())
    }

    /// List the entries in the directory at `path`, calling `f` for
    /// each. Skips the superblock (root only). Applies splice
    /// renumbering. Chases HardTail-threaded continuation pairs for the
    /// full length of the chain with a Brent's cycle-safe walk (no
    /// arbitrary cap; a cyclic chain is rejected with [`Error::Corrupt`],
    /// see ADR-0009). The end-to-end reachable-pair limit
    /// (`MAX_QUEUED_PAIRS = 32`) lives in the mount-time gstate sweep,
    /// not here.
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
    /// HardTails for the full length of the chain (Brent's walk, no
    /// arbitrary cap; a cyclic chain is rejected with
    /// [`Error::Corrupt`], see [`BrentTailWalk`] and ADR-0009).
    /// Per-pair splice renumbering is applied; ids are pair-local and
    /// reset to 0 at each pair boundary.
    ///
    /// On `Err` (including a corrupt cyclic chain) the caller must
    /// discard any entries already passed to `f`: a cycle is detected
    /// only after the moving pointer catches the reference, so a
    /// streaming caller may have seen a bounded prefix, possibly with
    /// repeats, before the error.
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
        let mut walk = BrentTailWalk::new(current);
        loop {
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
                Some(p) => {
                    walk.advance(p)?;
                    current = p;
                }
                None => return Ok(emitted),
            }
        }
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
    /// **Scope.** Root-only and does not enforce a "directory must be
    /// empty" check before issuing the Delete tag. Use [`Self::rmdir`]
    /// for directories so the emptiness check fires; this method is
    /// intended for regular file removal at the root and is the path
    /// SMIL audit-style consumers exercise.
    pub fn remove_from_root(
        &mut self,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        self.remove_from_pair(self.root, name, buf_a, buf_b)
    }

    /// Internal: remove an entry by name from the directory whose first
    /// metadata pair is `first_pair`, chasing HardTail continuation pairs
    /// to the pair that owns the entry.
    fn remove_from_pair(
        &mut self,
        first_pair: BlockPair,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        // A removal frees the entry's CTZ chain or child pair; drop the
        // lookahead cache so those blocks are reclaimed on the next
        // allocation rather than lingering as over-marked.
        self.used_cache = None;
        if buf_a.len() != S::BLOCK_SIZE || buf_b.len() != S::BLOCK_SIZE {
            return Err(Error::GeometryMismatch);
        }
        if name.is_empty() {
            return Err(Error::InvalidPath);
        }

        // Resolve the entry across the directory's chain, then dispatch
        // the Remove to its owning pair through the standard apply path so
        // the compact-or-append decision (and any wear-levelling that
        // fires) flows through one code path.
        let (target, target_id) = match self.seek_entry_in_chain(first_pair, name, buf_a, buf_b)? {
            ChainSeek::Found { pair, id, .. } => (pair, id),
            ChainSeek::Absent { .. } => return Err(Error::NotFound),
        };
        self.apply_op_to_pair(target, &WriteOp::Remove { id: target_id }, buf_a, buf_b)
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
    /// This method is the inline-only, root-only fast path. For arbitrary
    /// paths or content past `INLINE_MAX` use [`Self::write_to_path`],
    /// which auto-dispatches inline vs CTZ.
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
    pub(crate) fn write_inline_to_pair(
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
        // Names are capped at NAME_MAX (255); the inline content keeps
        // the tag length field's 0x3FF ceiling (review M2, `lfs-ax2`).
        if name.is_empty() || name.len() > crate::NAME_MAX || content.len() > 0x3FF {
            return Err(Error::InvalidPath);
        }

        // Look up the existing entry (if any) across the directory's
        // HardTail chain, or find the last pair (with room) for a create.
        // Reject overwrite of a Directory: the Update path would
        // substitute an InlineStruct over the existing DirStruct slot
        // during compaction, orphaning the directory's children pair.
        // Mirrors the matching check in `write_ctz_to_pair`.
        let (target, op) = match self.seek_entry_in_chain(pair_addr, name, buf_a, buf_b)? {
            ChainSeek::Found { pair, id, kind } => {
                if kind != crate::dir::EntryKind::RegularFile {
                    return Err(Error::AlreadyExists);
                }
                (pair, WriteOp::Update { id, content })
            }
            ChainSeek::Absent { last_pair, count } => {
                let new_id = u16::try_from(count).map_err(|_| Error::OutOfRange)?;
                if new_id == crate::tag::ID_NONE {
                    return Err(Error::OutOfRange);
                }
                (
                    last_pair,
                    WriteOp::Create { id: new_id, name, content, moved_attrs: StagedAttrs::EMPTY },
                )
            }
        };
        self.apply_op_to_pair(target, &op, buf_a, buf_b)
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

        let new_end = {
            let mut commit = crate::meta::Commit::new(scratch, 1)?;
            commit.tag(
                crate::tag::Tag::new(true, crate::tag::TagType::Superblock, 0, 8),
                crate::MAGIC,
            )?;
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
            commit.bytes_written()
        };

        // Erase + program block 0 with the committed superblock pair.
        // Program only the committed prefix, matching every other commit
        // path (`&buf[..new_end]`); the erase already left the trailing
        // bytes at 0xFF, so the on-disk result is identical and no prog
        // cycles are spent on the all-0xFF tail.
        storage.erase(0).map_err(|_| Error::Io)?;
        storage.program(0, 0, &scratch[..new_end]).map_err(|_| Error::Io)?;
        // Review H2: verify the superblock commit like every other
        // commit. Block 0 is the fixed mount anchor and cannot
        // relocate, so a silent corruption here is a hard `Io` error,
        // not a candidate for the worn-block fallback.
        if !verify_programmed(storage, BlockAddress::new(0), 0, &mut scratch[..new_end]) {
            return Err(Error::Io);
        }
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

        // pair was a Copy struct over the buffer slices; letting it
        // go out of scope releases the borrow. Reuse `block_a_buf`
        // and `block_b_buf` for the gstate sweep below.
        let _ = pair;
        let mut fs = Self {
            storage,
            superblock: sb,
            root: ROOT_BLOCK_PAIR,
            used_cache: None,
            pending_move: None,
        };

        // Atomic-move-state recovery: walk every reachable metadata
        // pair, XOR-accumulate `MoveState` tag bodies into a single
        // gstate. If non-zero, a cross-directory rename was crashed
        // between its destination Create and source Delete; complete
        // the move before returning the Fs handle so the user never
        // observes the duplicate state.
        let gstate = accumulate_gstate(&mut fs.storage, fs.root, block_a_buf, block_b_buf)?;
        if let Some((src_pair, src_id)) = gstate.pending_move() {
            fs.recover_pending_move(src_pair, src_id, block_a_buf, block_b_buf)?;
        }
        if let Some((old_pair, new_pair)) = gstate.pending_relocation() {
            fs.recover_pending_relocation(old_pair, new_pair, block_a_buf, block_b_buf)?;
        }
        // Deorphan sweep: drop any pair left in the global thread but not
        // the live tree by an interrupted rmdir un-thread or
        // sibling-predecessor relocation. A no-op on a healthy filesystem.
        fs.deorphan_sweep(block_a_buf, block_b_buf)?;
        Ok(fs)
    }

    /// Walk the global metadata-pair thread and drop any pair that is in
    /// the thread but not a live tree directory (a crash orphan from an
    /// interrupted `rmdir` un-thread or sibling-predecessor relocation).
    /// Re-points the orphan's predecessor past it so the C reference's
    /// allocator/traverse stop following a stale link and the orphan's
    /// blocks are reclaimed. Idempotent; a no-op when every threaded pair
    /// is a live directory (the healthy case, including C-written images).
    fn deorphan_sweep(&mut self, buf_a: &mut [u8], buf_b: &mut [u8]) -> Result<(), Error> {
        let mut tree = [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE);
            crate::alloc::MAX_QUEUED_PAIRS];
        let tree_count =
            collect_live_tree_pairs(&mut self.storage, self.root, &mut tree, buf_a, buf_b)?;

        // Track the pairs advanced through so a cyclic thread (a corrupt
        // image) terminates the sweep without error: mount must stay
        // cycle-safe (the resolution path rejects the cycle as Corrupt
        // later), matching `accumulate_gstate`'s deduped walk. The `steps`
        // backstop also breaks rather than erroring.
        let mut cur = self.root;
        let mut visited = [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE);
            crate::alloc::MAX_QUEUED_PAIRS];
        visited[0] = self.root;
        let mut visited_count = 1usize;
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 4 * crate::alloc::MAX_QUEUED_PAIRS {
                break;
            }
            self.storage.read(cur.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
            self.storage.read(cur.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
            let (next, cur_is_hard) = {
                let p = MetadataPair::parse(cur.a, &*buf_a, cur.b, &*buf_b)?;
                (p.reader.tail(), p.reader.is_hard_tail())
            };
            let Some(next) = next else {
                break;
            };
            if tree[..tree_count].contains(&next) {
                // A live threaded directory; advance, but stop on a cycle
                // (leave the corrupt thread as-is for the resolution path
                // to reject).
                if visited[..visited_count].contains(&next) {
                    break;
                }
                if visited_count < crate::alloc::MAX_QUEUED_PAIRS {
                    visited[visited_count] = next;
                    visited_count += 1;
                }
                cur = next;
                continue;
            }
            // `next` is threaded but not a live tree pair. Two cases,
            // distinguished exactly as the C reference's deorphan does
            // (lfs.c `lfs_fs_deorphan`, the half-orphan pass):
            //
            // 1. Half-orphan (review H4): a crash between
            //    `propagate_relocation`'s parent commit and predecessor
            //    commit left the TREE holding the relocated twin while
            //    the thread still points at the outdated address. The
            //    tree is authoritative: re-point the thread AT the
            //    twin. Reclaiming instead would permanently drop a
            //    live pair from the thread, which the C reference's
            //    allocator and traverse depend on. No gstate steal:
            //    the twin carries the pair's (shared) log, so its
            //    contribution stays counted through the tree address.
            //
            // 2. Full orphan: a crash mid-rmdir. Re-point `cur` past
            //    it (to the orphan's own successor, or clear at the
            //    list end), stealing the orphan's gstate contribution
            //    (review C7/D5).
            //
            // Either way, re-check `cur` without advancing.
            if let Some(twin) =
                relocated_twin_in(&mut self.storage, &tree[..tree_count], next, buf_a, buf_b)?
            {
                self.apply_op_to_pair_inner(
                    cur,
                    &WriteOp::Noop,
                    None,
                    None,
                    Some((twin, cur_is_hard)),
                    Inflight::NONE,
                    buf_a,
                    buf_b,
                )?;
                self.used_cache = None; // the stale twin's exclusive block frees
                continue;
            }
            self.unthread_and_steal(cur, next, buf_a, buf_b)?;
            self.used_cache = None; // the orphan's blocks are now free
        }
        Ok(())
    }

    /// Complete a cross-directory rename whose destination commit
    /// landed but whose source Delete did not. Emits a Delete at
    /// `src_id` in `src_pair` along with a balancing `MoveState` tag
    /// so the filesystem's gstate returns to zero.
    ///
    /// After a genuine crashed rename `src_id` is always the live id
    /// the source entry held at rename time (recovery runs at mount,
    /// before any user operation can renumber it). The defensive guard
    /// here is for a corrupted or adversarial `MoveState` body that
    /// decodes to an out-of-range id: emitting a `Delete` past the
    /// live count would corrupt the splice state of `src_pair`, and
    /// the balancing `MoveState` would permanently mask the
    /// inconsistency by zeroing the gstate. Surface it as
    /// [`Error::Corrupt`] at mount instead.
    fn recover_pending_move(
        &mut self,
        src_pair: BlockPair,
        src_id: u16,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        // The durable MoveState body names the source pair at the
        // address it had when the destination commit landed; the crash
        // window spans the destination commit's relocation cascade,
        // which can have relocated the source (review C6). Resolve to
        // the pair's live address: the decoded address when it is
        // still in the live tree, otherwise its relocated twin. The
        // commit must land on the LIVE address: committing through the
        // stale one can rotate onto a block the live pair does not
        // contain, making the delete (and the gstate cancel) invisible
        // to every future mount.
        let mut tree = [BlockPair::new(BlockAddress::NONE, BlockAddress::NONE);
            crate::alloc::MAX_QUEUED_PAIRS];
        let tree_count =
            collect_live_tree_pairs(&mut self.storage, self.root, &mut tree, buf_a, buf_b)?;
        let target = if tree[..tree_count].contains(&src_pair) {
            src_pair
        } else {
            relocated_twin_in(&mut self.storage, &tree[..tree_count], src_pair, buf_a, buf_b)?
                .unwrap_or(src_pair)
        };

        // Validate src_id against the live entry count of the target.
        self.storage.read(target.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
        self.storage.read(target.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
        {
            let pair = MetadataPair::parse(target.a, &*buf_a, target.b, &*buf_b)?;
            let active_is_a = pair.active_block == target.a;
            let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
            let count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
            if (src_id as usize) >= count {
                return Err(Error::Corrupt);
            }
        }

        // The balancing delta uses the ORIGINAL coordinates: it must
        // XOR-cancel the destination commit's body byte for byte.
        let balance = crate::gstate::build_move_body(src_pair, src_id);
        self.apply_op_to_pair_with_movestate(
            target,
            &WriteOp::Remove { id: src_id },
            Some(balance),
            buf_a,
            buf_b,
        )
    }

    /// Cancel a half-completed wear-levelling pair relocation by
    /// emitting a balancing `RelocateState` commit on `old_pair`.
    fn recover_pending_relocation(
        &mut self,
        old_pair: BlockPair,
        new_pair: BlockPair,
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<(), Error> {
        let balance = crate::gstate::build_relocate_body(old_pair, new_pair);
        self.apply_op_to_pair_inner(
            old_pair,
            &WriteOp::Noop,
            None,
            Some(balance),
            None,
            Inflight::NONE,
            buf_a,
            buf_b,
        )
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
    ///
    /// Pending bytes in any wrapping cache (notably
    /// [`crate::NorAlignedStorage`]'s program-window cache) are NOT
    /// flushed here; every public mutation on `Fs` already calls
    /// [`Storage::sync`] before returning, so callers who only mutate
    /// through `Fs` have a durable image at every API boundary.
    /// Callers that bypass `Fs` and program the underlying device
    /// directly are responsible for their own sync sequencing.
    #[inline]
    #[must_use]
    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Flush any pending bytes in the storage layer's caches to the
    /// device. Equivalent to `self.storage_mut().sync()` but exposed
    /// on `Fs` so callers do not have to reach through the storage
    /// accessor for a routine durability gate.
    ///
    /// Every public mutation on `Fs` already syncs as its final step,
    /// so explicit `sync()` calls are only needed when the caller
    /// mixed direct storage programs with `Fs` calls, or when the
    /// caller wants a fresh durability point between mutations.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the underlying device's `sync` fails.
    pub fn sync(&mut self) -> Result<(), Error> {
        self.storage.sync().map_err(|_| Error::Io)
    }

    /// Read a CTZ backed file's content via [`crate::ctz::read_ctz`].
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
        // bytes; we return a `ResolvedPath<'b>` borrowing from them. The
        // Brent's walk: a cyclic tail is rejected with `Error::Corrupt`
        // (review item R1) and a valid chain of any length resolves
        // with no arbitrary cap (review item R3, ADR-0009).
        let mut walk = BrentTailWalk::new(current);
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
                walk.advance(next)?;
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

    /// Resolve `name` within the directory whose first metadata pair is
    /// `first`, chasing `HardTail` continuation pairs. Mirrors the C
    /// reference's `lfs_dir_find_`: a split directory's entries span
    /// several pairs, so the name may live in any pair of the chain and a
    /// new entry is appended to the last pair (the one with room).
    ///
    /// Returns [`ChainSeek::Found`] with the owning pair, the entry's
    /// local id, and its kind when the name resolves; otherwise
    /// [`ChainSeek::Absent`] with the chain's last pair and its live
    /// entry count. On return the scratch buffers hold the pair named in
    /// the result (the owning pair for `Found`, the last pair for
    /// `Absent`), so the caller can re-parse it without another read.
    ///
    /// Cycle-safe: a malformed self-referential HardTail chain is
    /// rejected with [`Error::Corrupt`] via the same Brent's walk the
    /// reader uses (ADR-0009). For a single-pair directory (every
    /// directory this writer produced before `lfs-cvh`) the chain is one
    /// pair, so this reads `first`, looks it up, and returns — identical
    /// to the prior single-pair behavior.
    pub(crate) fn seek_entry_in_chain(
        &mut self,
        first: BlockPair,
        name: &[u8],
        buf_a: &mut [u8],
        buf_b: &mut [u8],
    ) -> Result<ChainSeek, Error> {
        let mut current = first;
        let mut walk = BrentTailWalk::new(current);
        loop {
            self.storage.read(current.a.as_u32(), 0, buf_a).map_err(|_| Error::Io)?;
            self.storage.read(current.b.as_u32(), 0, buf_b).map_err(|_| Error::Io)?;
            let next: Option<BlockPair>;
            let hit: Option<(u16, crate::dir::EntryKind)>;
            let count: usize;
            {
                let pair = MetadataPair::parse(current.a, &*buf_a, current.b, &*buf_b)?;
                hit = crate::dir::lookup(&pair, name).map(|r| (r.entry.id, r.entry.kind));
                // Only a HardTail continues this directory; a SoftTail is
                // the next directory in the global thread, not part of
                // this chain.
                next = if pair.reader.is_hard_tail() { pair.reader.tail() } else { None };
                // Live count of the (potential) last pair, needed only
                // for the Absent result; cheap to compute regardless.
                let active_is_a = pair.active_block == current.a;
                let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
                count = gather_live_slots(&pair, active_is_a, buf_a, buf_b, &mut slots)?;
            }
            if let Some((id, kind)) = hit {
                // Buffers already hold `current` (the owning pair).
                return Ok(ChainSeek::Found { pair: current, id, kind });
            }
            match next {
                Some(t) => {
                    walk.advance(t)?;
                    current = t;
                    // Loop re-reads the next pair on the next iteration.
                }
                None => {
                    // Buffers hold `current` (the last pair of the chain).
                    return Ok(ChainSeek::Absent { last_pair: current, count });
                }
            }
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
        // Brent's walk: a cyclic HardTail is rejected with
        // `Error::Corrupt` (review item R1); a valid chain of any length
        // is descended with no arbitrary cap (review item R3, ADR-0009).
        let mut walk = BrentTailWalk::new(current);
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
                    walk.advance(tail)?;
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
    ///
    /// **Low-level internal.** Exposed for the conformance and
    /// adversarial test harnesses. It stays semver-covered for the 1.x
    /// line but is not part of the recommended surface and is a
    /// candidate to move to `pub(crate)` in 2.0; depend on it only with
    /// that in mind.
    #[doc(hidden)]
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

#[cfg(test)]
mod split_point_tests {
    //! Unit tests for the pure split-point math (`compact_range_size`,
    //! `compute_split_index`). These run with no I/O: a zero-sized
    //! `Storage` impl supplies only the `BLOCK_SIZE` / `PROG_SIZE`
    //! geometry the budget reads. Geometry matches `tests/pending_dir_split.rs`
    //! (256-byte blocks, 16-byte prog), the reproduce-first target.

    use super::{
        compact_range_size, compute_split_index, SlotOffsets, StagedAttrs, WriteOp,
        MAX_LIVE_ENTRIES,
    };
    use crate::storage::Storage;

    /// An empty committed source block (revision header only, rest
    /// erased): parses cleanly, carries no tags, so the attr replay
    /// contributes nothing and slot name/struct slices read 0xFF
    /// filler. The size math under test is the per-entry tag layout,
    /// which does not depend on body contents.
    fn empty_source() -> [u8; 256] {
        let mut buf = [0xFFu8; 256];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf
    }

    struct Dev256;
    impl Storage for Dev256 {
        type Error = ();
        const READ_SIZE: usize = 16;
        const PROG_SIZE: usize = 16;
        const BLOCK_SIZE: usize = 256;
        const BLOCK_COUNT: u32 = 64;
        const CACHE_SIZE: usize = 64;
        const LOOKAHEAD_SIZE: usize = 8;
        fn read(&mut self, _: u32, _: u32, _: &mut [u8]) -> Result<(), ()> {
            unreachable!("split-point math performs no I/O")
        }
        fn program(&mut self, _: u32, _: u32, _: &[u8]) -> Result<(), ()> {
            unreachable!("split-point math performs no I/O")
        }
        fn erase(&mut self, _: u32) -> Result<(), ()> {
            unreachable!("split-point math performs no I/O")
        }
    }

    /// A live regular-file slot with the given NAME and STRUCT byte
    /// lengths (offsets are irrelevant to the size math).
    fn slot(name_len: u16, struct_len: u16) -> SlotOffsets {
        SlotOffsets {
            name_off: 0,
            name_len,
            name_kind: 0, // RegularFile
            struct_off: 0,
            struct_len,
            struct_kind: 0, // InlineStruct
        }
    }

    fn filled(n: usize, name_len: u16, struct_len: u16) -> [SlotOffsets; MAX_LIVE_ENTRIES] {
        let mut slots = [SlotOffsets::EMPTY; MAX_LIVE_ENTRIES];
        for s in slots.iter_mut().take(n) {
            *s = slot(name_len, struct_len);
        }
        slots
    }

    // The 256/16 budget: min(256 - 40, alignup(128, 16)) = min(216, 128) = 128.
    const BUDGET: usize = 128;

    #[test]
    fn budget_matches_oracle_geometry() {
        // Empty range fits trivially; confirm the budget the split obeys
        // by probing a single oversized entry that must not be split off
        // alone (end - split == 1 stops the loop).
        let slots = filled(1, 200, 200);
        // A 2-entry combined sequence whose single existing slot is huge:
        // the loop can split at most down to one entry per side.
        let n = compute_split_index::<Dev256>(&empty_source(), &slots, 1, &WriteOp::Noop, 0, 1)
            .unwrap();
        assert_eq!(n, 0, "a single-entry range never splits");
    }

    #[test]
    fn range_size_create_family() {
        // name "fNNN" = 4 bytes, inline content "x" = 1 byte.
        let slots = filled(3, 4, 1);
        let op = WriteOp::Create {
            id: 3,
            name: b"f003",
            content: b"x",
            moved_attrs: StagedAttrs::EMPTY,
        };
        // Each live slot: 12 + 4 + 1 = 17. New entry (op_dsize_of - 8):
        // 4 + (4+4) + (4+1) = 17. Range [0,4) over count=3 includes all
        // three live slots plus the virtual new entry.
        assert_eq!(compact_range_size(&empty_source(), &slots, 3, &op, 0, 4).unwrap(), 3 * 17 + 17);
        // Upper sub-range [2,4): one live slot + the new entry.
        assert_eq!(compact_range_size(&empty_source(), &slots, 3, &op, 2, 4).unwrap(), 17 + 17);
        // The new entry is excluded from a lower range that stops before
        // its index.
        assert_eq!(compact_range_size(&empty_source(), &slots, 3, &op, 0, 3).unwrap(), 3 * 17);
    }

    #[test]
    fn small_directory_does_not_split() {
        // 3 entries plus a create = 68 bytes < 128 budget: no split.
        let slots = filled(3, 4, 1);
        let op = WriteOp::Create {
            id: 3,
            name: b"f003",
            content: b"x",
            moved_attrs: StagedAttrs::EMPTY,
        };
        assert_eq!(
            compute_split_index::<Dev256>(&empty_source(), &slots, 3, &op, 0, 4).unwrap(),
            0
        );
    }

    #[test]
    fn overflowing_directory_splits_at_half() {
        // 11 existing 17-byte entries + a 12th (create) = 204 bytes > 128.
        // Inner loop: split 0 -> 6 (upper [6,12) = 5*17 + 17 = 102 <= 128).
        let slots = filled(11, 4, 1);
        let op = WriteOp::Create {
            id: 11,
            name: b"f011",
            content: b"x",
            moved_attrs: StagedAttrs::EMPTY,
        };
        let split = compute_split_index::<Dev256>(&empty_source(), &slots, 11, &op, 0, 12).unwrap();
        assert_eq!(split, 6);
        // Both halves must fit the budget after the chosen split.
        assert!(compact_range_size(&empty_source(), &slots, 11, &op, 0, split).unwrap() <= BUDGET);
        assert!(compact_range_size(&empty_source(), &slots, 11, &op, split, 12).unwrap() <= BUDGET);
    }

    #[test]
    fn remove_target_costs_nothing_in_range() {
        let slots = filled(4, 4, 1);
        let op = WriteOp::Remove { id: 1 };
        // Four 17-byte slots, one removed -> three remain: 51 bytes.
        assert_eq!(compact_range_size(&empty_source(), &slots, 4, &op, 0, 4).unwrap(), 3 * 17);
    }

    #[test]
    fn rename_in_place_counts_both_names() {
        let slots = filled(2, 4, 1);
        let op = WriteOp::RenameInPlace {
            id: 0,
            name_type: crate::tag::TagType::RegularFile,
            new_name: b"renamed-longer",
        };
        // Slot 0 carries Create(4) + NAME(4+4) + NAME(4+14) + STRUCT(4+1)
        // = 35; slot 1 is a plain 17-byte entry.
        assert_eq!(compact_range_size(&empty_source(), &slots, 2, &op, 0, 2).unwrap(), 35 + 17);
    }

    #[test]
    fn cascade_subrange_reuses_begin() {
        // Emulate the cascade's second pass over a shrunk lower range:
        // begin stays 0, end is the prior split. A range that now fits
        // returns begin (no further split).
        let slots = filled(6, 4, 1);
        // 6 * 17 = 102 <= 128: the lower half fits, no further split.
        assert_eq!(
            compute_split_index::<Dev256>(&empty_source(), &slots, 6, &WriteOp::Noop, 0, 6)
                .unwrap(),
            0
        );
    }
}
