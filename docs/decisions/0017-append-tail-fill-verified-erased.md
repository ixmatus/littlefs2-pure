# ADR-0017: in-place append keeps the tail fill, behind a verified-erased precondition

- **Status**: accepted; **implemented** (review C8, bead lfs-ay4).
- **Date**: 2026-06-11

## Context

Review C8: `stream_ctz_extend` programmed the appended bytes into the
committed tail block's erased region before the overflow allocation
and bounds checks. On an allocation failure (device full) or a power
loss, the metadata still said `old_size` but cells past the committed
EOF were programmed; the next append recomputed the same offsets and
programmed different bytes over them, and NOR AND-semantics silently
corrupted the newly appended, acknowledged data (reproduced: `0x99 &
0x55 = 0x11` read back as committed content).

The C reference never programs a committed data block twice:
`lfs_ctz_extend` copies a partial tail into a freshly erased block on
every extend. ADR-0011's streaming append deliberately departed from
that for its write-amplification win (fill the existing tail in
place), but did so on an unverified assumption, stated in a comment:
"the free region there is still 0xFF (never programmed since erase)".
A torn previous append falsifies exactly that assumption, and no
metadata records it.

## Decision

Keep the in-place tail fill, but make its precondition verified
rather than assumed, and order it after every fallible step.

1. All bounds checks and all fallible device work (overflow
   allocation, overflow block writes) run before any program touches
   the committed tail; the fill is the last device action before
   sync. A failure can then only orphan fresh blocks, never poison
   committed ones.
2. Before filling, the append reads the tail block and verifies the
   fill region is actually erased. A clean region takes the in-place
   fill (the common case; ADR-0011's win is preserved). A dirty
   region, which only a torn fill can produce, routes the tail
   through copy-on-write to a fresh block, the same countermeasure
   `shrink_ctz_head` already uses for the shrink-then-append case.
   Programming `0xFF` bytes over erased cells flips nothing, so a
   torn fill of `0xFF` data is indistinguishable from erased and
   harmless under the check.
3. Crash closure: a power loss during the fill leaves residue that no
   metadata records, and the NEXT append's dirty check converts it
   into a copy-on-write. The residue is never read (it sits past the
   committed size) and never ANDed over.

This is a deliberate, documented divergence from the C reference's
always-copy behavior (the ADR-0008 class): behavior toward the disk
format is unchanged, only the write path's block reuse strategy
differs, and the bidirectional gates are unaffected.

## Consequences

**Wins.** Both C8 halves close with reproducers on strict NOR
semantics (`tests/review_ctz_append_poison.rs`): the device-full
append no longer poisons, and a torn-append sweep at device program
granularity (the torn wrapper sits inside `NorAlignedStorage`,
avoiding the cache-boundary overclaim review H7 flags) reads back
exact bytes at every trigger. The common-case append still fills in
place: one extra block read per append with tail room is the whole
steady-state cost.

**Costs.** The dirty path consumes a fresh block and a whole-block
write where C would have anyway; the orphaned old tail waits for an
allocator scan. The verified-erased check reads the whole tail block
through the storage cache once per append. The check cannot
distinguish "torn fill residue" from any other dirt, which is the
point; but it also means a buggy future writer that programs past
EOF would be silently absorbed as COW rather than surfaced.

**Explicitly out of scope.** A worn tail block discovered by the
in-place fill still fails with `Error::Io` rather than relocating
(the lfs-23f data-path retry covers new blocks only); C9's inflight
threading through the commit entry points is a separate bead
(lfs-0vy).

## Related

- `docs/reviews/2026-06-10-deep-adversarial-review.md` finding C8; bead lfs-ay4.
- ADR-0011 (the in-place append this preserves), ADR-0008 (the
  documented-divergence precedent), the lfs-6o9 shrink fix (the
  copy-on-write pattern reused here).
- Oracle: `lfs_ctz_extend` (vendored lfs.c:2891ff).
