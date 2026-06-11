# ADR-0016: gstate totals, stealing drops, and relocation-cascade coordinate integrity

- **Status**: accepted; **implemented** (review arc lfs-w7w, lfs-njj,
  lfs-bkq, lfs-gfm, lfs-les from the 2026-06 deep review).
- **Date**: 2026-06-11

## Context

The 2026-06 deep review's second Critical cluster was the gstate and
relocation family: the v2 write arc re-derived C's mechanisms but
missed four countermeasures the C reference carries for exactly the
cross-products the review targeted. Every finding reproduced:

- **C4.** Per-pair gstate was decoded as XOR-of-all-tags. The C
  convention has two levels: within one pair's log every committed
  gstate tag is the pair's new TOTAL (writers fold the pair's
  existing contribution into each tag; readers take the single latest
  tag), and only across pairs does XOR accumulate. A valid C log with
  two MOVESTATE tags decoded a phantom move under XOR-of-all and
  mount recovery deleted a live entry.
- **C7.** Dropping a pair from the reachable set (rmdir's un-thread,
  the deorphan reclaim) never removed its contribution from the
  aggregate. A completed rename leaves equal non-zero totals on
  source and destination; rmdir of the emptied source made the
  aggregate permanently non-zero, and once the dead pair's content no
  longer covered the decoded id, the image was unmountable.
- **C6.** A cross-directory rename captured the source pair address
  before the destination commit; a relocation cascade triggered by
  that commit could relocate the source, so the source delete (and a
  crashed-rename recovery) targeted the orphaned old address:
  duplicate entry, uncancellable MoveState.
- **H3.** An abandoned wear relocation (worn fresh block) deferred
  its unbalanced `RelocateState` to mount-time cancellation; a second
  relocation before remount outdated both addresses and recovery
  committed through a dead pair on every mount.
- **H4.** A crash between `propagate_relocation`'s parent and
  predecessor commits leaves the tree holding the relocated pair
  while the thread points at the outdated twin; the deorphan sweep
  reclaimed the stale link, permanently dropping a live pair from
  the thread the C allocator and traverse depend on.

Reproducing C6 surfaced a sixth, deeper defect this ADR also covers:
`find_parent_in_tree` BFS-enqueued every raw `DirStruct` body,
superseded ones included. Once relocation reuses freed blocks, a
stale pre-relocation pair's old log still matches the target, and the
parent repoint then committed onto a DEAD pair (erasing freed or
reallocated blocks) while the live parent kept the outdated
reference. This is the same class `accumulate_gstate` and
`collect_live_tree_pairs` already guard against (authoritative
children only) and the C reference avoids by construction
(`lfs_fs_parent` walks the thread and matches through the
splice-corrected fetch).

## Decision

Adopt the C reference's gstate conventions and coordinate-integrity
countermeasures, shaped for this crate's two-commit architecture.

1. **Latest-total-wins per pair** (C4). `scan_pair_move_state` /
   `scan_pair_relocate_state` return the latest committed tag's body
   (an explicit all-zero body is a real total and shadows earlier
   tags). The append path folds the pair's existing total into every
   gstate tag it emits; the compact path re-emits the net total as
   the rebuilt block's single tag (an all-zero net is omitted:
   absence reads as zero). Cross-pair accumulation stays XOR.
2. **Drops steal by construction** (C7, review D5).
   `unthread_and_steal` is the single thread-drop primitive: it
   re-points the predecessor's tail past the dropped pair and folds
   the dropped pair's `MoveState` and `RelocateState` totals into
   that same commit (`lfs_dir_drop`'s `// steal state`). rmdir and
   the deorphan reclaim both route through it.
3. **An Fs-resident pending move with relocation remap** (C6). The
   rename window stores `(cur_pair, cur_id, delta)` on the `Fs`;
   `propagate_relocation` remaps `cur_pair` when the source
   relocates, and the source commit targets the remapped address
   while folding the ORIGINAL delta (the bodies cancel by XOR
   regardless of address). This is the architectural seat of the C
   reference's required pending-move patch, adapted: C folds the
   completion into the cascade commit because its gstate is
   RAM-authoritative per commit; our two-commit rename only needs the
   coordinates to stay live.
4. **Crash-window twin resolution** (C6's recovery half, H4).
   Relocation replaces exactly one block of a pair, so a stale
   address's live twin is the unique tree pair sharing a block where
   the shared block is the twin's INACTIVE half (`relocated_twin_in`;
   the active-half sharer is a block reuser). `recover_pending_move`
   resolves the decoded source through it before committing, and the
   deorphan sweep re-points the thread at the twin instead of
   reclaiming (C's half-orphan pass: the tree is authoritative).
5. **Abandoned relocations self-cancel** (H3). When the fresh program
   fails, the abandonment immediately folds the cancelling
   `RelocateState` delta back out of the pair's total in a follow-up
   commit on the unchanged pair address, instead of deferring to
   mount-time recovery across a window where the addresses can die.
6. **The parent walk consumes live state only.** `find_parent_in_tree`
   enqueues children from splice-corrected latest-wins `DirStruct`
   bodies (via `gather_live_slots`) and the reader's latest tail, and
   matches over live bodies, so the returned id is live by
   construction. This supersedes ADR-0015's per-pair candidate
   discipline for C5 (the live-slot walk subsumes it) and closes the
   stale-pair parent-commit defect above.

## Consequences

**Wins.** All five findings close with reproducers pinned
(`tests/review_gstate.rs`, `tests/review_reloc_cascade.rs`),
including two power-loss sweeps over relocating renames and a
thread/tree sync invariant, plus consecutive-mount byte-stability
checks that fail on any futile recovery loop. The remap was verified
load-bearing by disabling it (the C6 grid then reproduces the
duplicate entry at round 36). Interop: a C image whose logs hold
multiple gstate tags now decodes correctly, and our writer emits the
totals C's reader expects.

**Costs.** Migration: a v1.2.0-written image crashed mid-rename with
several unfolded delta tags in one log decodes differently under the
latest-total reader; quiescent images are unaffected. The H3
self-cancel adds one small commit per abandoned relocation and rides
the standard commit path (its compaction can nest a cascade; bounded
like any commit, but it deepens the ADR-0006 recursion budget by one
frame in the abandonment corner). `recover_pending_move` now collects
the live tree before committing: one BFS per crashed-rename recovery,
mount-time only. The twin rule's disambiguation assumes the
mid-cascade window contains only cascade allocations (fresh halves
become active); a future write path that allocates erased-half pairs
mid-cascade would need to revisit `relocated_twin_in`.

**Explicitly out of scope.** The full Fs-resident gstate aggregate
(review D6's complete form: an in-RAM `lfs->gstate`/`gdelta` model
maintained through every commit) is not adopted; the pending-move
field is its minimal load-bearing slice. C's orphan-count and
needssuperblock gstate bits stay unmodeled (review L5, contested).
C8/C9 (CTZ write-path families) are separate arcs. Parse-don't-
validate gstate decoding (review D8) remains open.

## Related

- `docs/reviews/2026-06-10-deep-adversarial-review.md` findings C4, C6, C7, H3, H4; design
  observations D5, D6, D8.
- Beads: lfs-w7w, lfs-njj (closed in the first slice), lfs-bkq,
  lfs-gfm, lfs-les (this slice).
- ADR-0015 (the splice core this builds on; its C5 candidate walk is
  superseded by item 6), ADR-0005 / ADR-0014 (the two relocation
  flavors whose one-block-kept invariant the twin rule derives from),
  ADR-0012 (the thread the half-orphan repoint preserves).
- Oracle: vendored `lfs.c` (`lfs_dir_getgstate`, `lfs_dir_drop`,
  `lfs_fs_relocate`'s pending-move patches at 2484/2536,
  `lfs_fs_deorphan`'s half-orphan pass, `lfs_fs_parent`).
