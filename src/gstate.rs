//! Filesystem-global state, XOR-accumulated across the per-pair
//! contributions of every reachable metadata pair.
//!
//! LittleFS tracks at most one in-flight cross-directory move at a
//! time via a 12-byte "gstate" field encoded as a triple of LE `u32`s:
//!
//! 1. The deletion tag that would complete the move (a regular
//!    `Delete` tag at the source id).
//! 2. The source pair's first block address.
//! 3. The source pair's second block address.
//!
//! # The two accumulation levels (review C4)
//!
//! The convention is the C reference's, and the two levels differ:
//!
//! - **Within one pair's log**, every committed gstate tag is the
//!   pair's new TOTAL contribution: writers fold the pair's existing
//!   contribution into each tag they commit, and the reader takes the
//!   single latest tag (`fs::scan_pair_move_state`). XOR-of-all-tags
//!   is WRONG here: a valid C log holding two MOVESTATE tags (two
//!   moves into the same directory, no intervening compaction)
//!   mis-accumulates into a phantom move under that reading.
//!   An explicit all-zero body is a real total ("returned to zero")
//!   and shadows earlier non-zero tags.
//! - **Across pairs**, the global gstate is the XOR of the per-pair
//!   contributions (`fs::accumulate_gstate`).
//!
//! Each cross-directory rename's two commits (Create-in-dst,
//! Delete-in-src) fold the SAME delta body into their pair's total,
//! so the global XOR returns to zero after both land. Crash between
//! them: the aggregate is non-zero, and mount-time recovery decodes
//! the in-flight move and completes it.
//!
//! Compaction preserves a pair's contribution by re-emitting its net
//! total as the compacted block's single gstate tag (an all-zero net
//! is simply omitted: absence reads as zero). Dropping a pair from
//! the reachable set (rmdir's un-thread, the deorphan reclaim) STEALS
//! its contribution into the survivor's commit
//! (`fs::unthread_and_steal`, the C reference's `lfs_dir_drop`);
//! without the steal, a globally balanced but per-pair non-zero
//! contribution would leave the aggregate permanently non-zero
//! (review C7).

use crate::block::{BlockAddress, BlockPair};
use crate::tag::{Tag, TagType};

/// Wire-format size of one `MoveState` tag body.
pub const MOVE_STATE_BODY_SIZE: usize = 12;

/// Wire-format size of one `RelocateState` tag body (two pair
/// addresses encoded as 4 LE `u32`s).
pub const RELOCATE_STATE_BODY_SIZE: usize = 16;

/// Accumulated filesystem-global state. Initialized at mount time
/// by XOR-accumulating every committed `MoveState` and `RelocateState`
/// body across every reachable metadata pair. Zero gstate means no
/// in-flight cross-directory move and no half-completed wear-levelling
/// relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Gstate {
    /// XOR-accumulated tag word. When non-zero, decodes as the
    /// `Delete` tag that would complete the in-flight move.
    pub move_tag: u32,
    /// XOR-accumulated source-pair first block.
    pub move_pair_a: u32,
    /// XOR-accumulated source-pair second block.
    pub move_pair_b: u32,
    /// XOR-accumulated relocation source-pair first block.
    pub relocate_old_a: u32,
    /// XOR-accumulated relocation source-pair second block.
    pub relocate_old_b: u32,
    /// XOR-accumulated relocation destination-pair first block.
    pub relocate_new_a: u32,
    /// XOR-accumulated relocation destination-pair second block.
    pub relocate_new_b: u32,
}

impl Gstate {
    /// Zero state: no in-flight move and no half-completed relocation.
    pub const ZERO: Self = Self {
        move_tag: 0,
        move_pair_a: 0,
        move_pair_b: 0,
        relocate_old_a: 0,
        relocate_old_b: 0,
        relocate_new_a: 0,
        relocate_new_b: 0,
    };

    /// True if the gstate represents no in-flight operation.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.move_tag == 0
            && self.move_pair_a == 0
            && self.move_pair_b == 0
            && self.relocate_old_a == 0
            && self.relocate_old_b == 0
            && self.relocate_new_a == 0
            && self.relocate_new_b == 0
    }

    /// True if a cross-directory move is in flight (gstate carries
    /// the deletion the source pair still owes).
    #[must_use]
    pub fn has_pending_move(&self) -> bool {
        self.move_tag != 0 || self.move_pair_a != 0 || self.move_pair_b != 0
    }

    /// True if a wear-levelling pair relocation is half-completed.
    #[must_use]
    pub fn has_pending_relocation(&self) -> bool {
        self.relocate_old_a != 0
            || self.relocate_old_b != 0
            || self.relocate_new_a != 0
            || self.relocate_new_b != 0
    }

    /// XOR the bytes of one on-disk `RelocateState` body into the
    /// running gstate.
    pub fn xor_relocate_body(&mut self, body: &[u8; RELOCATE_STATE_BODY_SIZE]) {
        self.relocate_old_a ^= u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        self.relocate_old_b ^= u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        self.relocate_new_a ^= u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        self.relocate_new_b ^= u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
    }

    /// Decode the in-flight pair relocation, if any.
    #[must_use]
    pub fn pending_relocation(&self) -> Option<(BlockPair, BlockPair)> {
        if !self.has_pending_relocation() {
            return None;
        }
        Some((
            BlockPair::new(
                BlockAddress::new(self.relocate_old_a),
                BlockAddress::new(self.relocate_old_b),
            ),
            BlockPair::new(
                BlockAddress::new(self.relocate_new_a),
                BlockAddress::new(self.relocate_new_b),
            ),
        ))
    }

    /// XOR the bytes of one on-disk `MoveState` body into the running
    /// gstate. Called once per committed `MoveState` tag during
    /// mount-time accumulation; called again during compaction to
    /// roll up a pair's net contribution.
    ///
    /// # Divergence: C's orphan and needssuperblock bits (review L5)
    ///
    /// The C reference packs more than a move into the same 32 bit tag
    /// word. Bits 8 to 0 hold an orphan count
    /// (`lfs_gstate_getorphans`, `lfs.c:411`), bit 9 holds a
    /// needssuperblock flag (`lfs_gstate_needssuperblock`,
    /// `lfs.c:420`), and bit 31 holds a summary of the low ten bits
    /// that `lfs_fs_preporphans` maintains (`lfs.c:4838`). This crate
    /// models only the move: the whole word lands in
    /// [`Self::move_tag`], and [`Self::pending_move`] classifies it as
    /// a tag, so anything that is not a `Delete` decodes as no move.
    ///
    /// Of those fields only bit 31 ever reaches a disk a C writer
    /// produced. C strips the entire ten bit size field from every
    /// gstate delta before committing it (`delta.tag &= ~LFS_MKTAG(0,
    /// 0, 0x3ff)` at `lfs.c:2024` and `lfs.c:2275`), and the comment
    /// at `lfs.c:4482` calls the superblock bit reserved on disk. C
    /// reconstitutes a count of one from bit 31 on its next mount
    /// (`lfs.c:4556`).
    ///
    /// **Consequence.** A C image carrying orphans presents this
    /// reader with `move_tag == 0x8000_0000`, so
    /// [`Self::has_pending_move`] reports true while
    /// [`Self::pending_move`] returns `None`. Mount therefore fires no
    /// recovery and does not loop; the residue persists across every
    /// Rust mount until a C mount runs its deorphan pass and clears
    /// it. Reads, writes, and move recovery are all unaffected: bit 31
    /// sits in the tag's valid bit position, which
    /// [`crate::tag::Tag::tag_type`] ignores, so an image carrying an
    /// orphan AND an in flight move still decodes and completes the
    /// move.
    ///
    /// The behavior above is pinned by
    /// `tests/review_l5_gstate_orphan_bits.rs`. Modeling the orphan
    /// bits properly belongs with the gstate aggregate resident in `Fs`;
    /// see the explicitly out of scope section of
    /// `docs/decisions/0016-gstate-totals-and-relocation-cascade.md`
    /// and the parse don't validate follow up (review D8).
    pub fn xor_body(&mut self, body: &[u8; MOVE_STATE_BODY_SIZE]) {
        self.move_tag ^= u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        self.move_pair_a ^= u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        self.move_pair_b ^= u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
    }

    /// Encode the gstate as a 12-byte LE-triple ready to be emitted
    /// as a `MoveState` tag body.
    #[must_use]
    pub fn to_body(self) -> [u8; MOVE_STATE_BODY_SIZE] {
        let mut out = [0u8; MOVE_STATE_BODY_SIZE];
        out[0..4].copy_from_slice(&self.move_tag.to_le_bytes());
        out[4..8].copy_from_slice(&self.move_pair_a.to_le_bytes());
        out[8..12].copy_from_slice(&self.move_pair_b.to_le_bytes());
        out
    }

    /// Decode the in-flight move, if any. Returns `(source_pair,
    /// source_id)` when `has_pending_move()` is true.
    #[must_use]
    pub fn pending_move(&self) -> Option<(BlockPair, u16)> {
        if !self.has_pending_move() {
            return None;
        }
        let tag = Tag::from_bits(self.move_tag);
        if tag.tag_type() != TagType::Delete {
            // The encoded tag is malformed; treat as no move (the
            // alternative is a panic, which is worse for a recovery
            // path). The Kani harness in `verify::commit_proofs` and
            // the fuzz parser cover the input-side; this guards the
            // semantic decode.
            return None;
        }
        let src_pair = BlockPair::new(
            BlockAddress::new(self.move_pair_a),
            BlockAddress::new(self.move_pair_b),
        );
        Some((src_pair, tag.id()))
    }
}

/// Build a `RelocateState` body recording the relocation of
/// `old_pair` to `new_pair`. The same body is emitted on the source
/// pair's alternate AND the freshly allocated block AND the parent
/// commit's `UpdateDirStruct`; once all three land the XOR-aggregate
/// across reachable pairs cancels to zero. A non-zero aggregate at
/// mount time indicates a half-completed relocation that
/// [`crate::Fs::mount`] cancels via a balancing recovery commit on
/// `old_pair`.
#[must_use]
pub fn build_relocate_body(
    old_pair: BlockPair,
    new_pair: BlockPair,
) -> [u8; RELOCATE_STATE_BODY_SIZE] {
    let mut out = [0u8; RELOCATE_STATE_BODY_SIZE];
    out[0..4].copy_from_slice(&old_pair.a.as_u32().to_le_bytes());
    out[4..8].copy_from_slice(&old_pair.b.as_u32().to_le_bytes());
    out[8..12].copy_from_slice(&new_pair.a.as_u32().to_le_bytes());
    out[12..16].copy_from_slice(&new_pair.b.as_u32().to_le_bytes());
    out
}

/// Build a `MoveState` body that records (or balances) an in-flight
/// move from `src_pair`, deleting entry `src_id` at the source.
/// The same body is emitted on both sides of the cross-directory
/// rename so they XOR to zero once both commits land.
#[must_use]
pub fn build_move_body(src_pair: BlockPair, src_id: u16) -> [u8; MOVE_STATE_BODY_SIZE] {
    let tag = Tag::new(true, TagType::Delete, src_id, 0).into_bits();
    let mut out = [0u8; MOVE_STATE_BODY_SIZE];
    out[0..4].copy_from_slice(&tag.to_le_bytes());
    out[4..8].copy_from_slice(&src_pair.a.as_u32().to_le_bytes());
    out[8..12].copy_from_slice(&src_pair.b.as_u32().to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_roundtrip_through_xor() {
        let pair = BlockPair::new(BlockAddress::new(4), BlockAddress::new(5));
        let body = build_move_body(pair, 7);
        let mut g = Gstate::ZERO;
        g.xor_body(&body);
        g.xor_body(&body);
        assert!(g.is_zero(), "double-XOR of the same body must zero out");
    }

    #[test]
    fn pending_move_decodes_after_single_xor() {
        let pair = BlockPair::new(BlockAddress::new(4), BlockAddress::new(5));
        let body = build_move_body(pair, 7);
        let mut g = Gstate::ZERO;
        g.xor_body(&body);
        let (decoded_pair, id) = g.pending_move().expect("non-zero gstate has a pending move");
        assert_eq!(decoded_pair, pair);
        assert_eq!(id, 7);
    }

    #[test]
    fn zero_gstate_has_no_pending_move() {
        assert_eq!(Gstate::ZERO.pending_move(), None);
        assert!(!Gstate::ZERO.has_pending_move());
    }
}
