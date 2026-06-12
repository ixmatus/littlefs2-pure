# ADR-0018: staged attribute move for cross-directory rename; CRC read-back after every commit

- **Status**: accepted; **implemented** (review arc lfs-inm / H6 and
  lfs-efe / H2 from the 2026-06 deep review).
- **Date**: 2026-06-11

## Context

Two High findings from the 2026-06 deep adversarial review
(`docs/reviews/2026-06-10-deep-adversarial-review.md`) touch the commit machinery from
opposite sides.

**H6.** Cross-directory rename rebuilt the destination entry from
NAME and STRUCT alone; every user attribute on the moved entry died
silently. The C reference's `lfs_rename` commits the destination with
`LFS_FROM_MOVE`, a traversal that replays **all** unique tags of the
moved id, attributes included, inside the same commit that carries
the create and the MoveState tag, so the move (entry plus attributes)
is atomic.

**H2.** No commit path re-read what it programmed. The C reference
re-reads and CRC-checks every commit (`lfs_dir_commitcrc` via
`lfs_bd_crc`) and treats a mismatch exactly like a program failure
(relocate). A device that accepts a program and lands corrupted cells
("silent corruption", a real worn-NOR failure mode) made this crate
report durable success and surface the loss only at the next mount.

The shared constraint: the kernel owns exactly two block-sized
buffers (`buf_a`, `buf_b`), both holding the destination pair during
a commit, and no allocator. The C reference solves both problems by
streaming through its `cache_size` block caches; this kernel has no
third buffer to stream through.

## Decision

**H6: stage, then emit.** `Fs::rename` captures the source entry's
live attributes (via `dir::for_each_live_attr`, the splice-correct
enumeration ADR-0015 built for compaction replay) into a fixed 1 KiB
stack pool (`RENAME_ATTR_STAGE`), serialized as
`[attr_id, len_lo, len_hi, value...]` records wrapped in the typed
`StagedAttrs` view. The Create-family `WriteOp`s carry a
`StagedAttrs<'a>`; both emission paths (`emit_op` on the append path,
`emit_compact_range`'s new-entry arm on the compact path) re-emit the
records after the STRUCT tag, and `op_dsize_of` counts them, so the
attributes ride the destination commit atomically with the MoveState
tag. Every non-rename constructor passes the visible
`StagedAttrs::EMPTY`.

Alternatives rejected:

- *Stream from the source pair at emit time* (the C shape): the
  enumeration needs a backward tag walk over the source block, which
  is in neither buffer by then; reading tag-sized windows from
  storage violates the `READ_SIZE` alignment contract (the exact
  defect review M7 flags in `walk_ctz_chain`). A commit-time
  storage-backed attr stream needs a reserve-and-fill `Commit` API
  plus an aligned window reader; deferred as the 2.x path to lifting
  the bound.
- *Move attributes in follow-up commits*: a crash between the create
  commit and the attr commits completes the move via gstate recovery
  with the attributes silently gone, which is H6 again, narrowed.
- *A third caller buffer*: an API break on the 1.x line for a corner
  case.

The bound is explicit and fail-loud: an entry whose live attribute
payload exceeds the stage fails the rename with `Error::OutOfRange`,
attributes intact, nothing moved, where C would succeed. Documented
on `Fs::rename`.

**H2: verify by CRC into the just-programmed buffer.** A single
helper, `verify_programmed`, re-reads the programmed region and
compares CRC32 against the bytes that were sent, the C mechanism. The
read-back destination is the region of the build buffer that was just
programmed: on a match the buffer's contents are unchanged by
construction; on a mismatch the caller treats buffer and block as
worn together, and every caller's existing worn-block fallback
(forced-victim eviction, fresh-block relocation, bad-block retry,
ADR-0014) already rebuilds the buffer before reuse. All eight commit
program sites verify: in-place append, compact-to-alternate, the
wear-relocation fresh copy, the bad-block fresh relocation, both
split programs, mkdir's pair init, and `format`'s superblock (the
last two report `Io`, matching their program-failure behavior; the
root anchor cannot relocate).

## Consequences

**Wins.** Attribute loss on rename closes with reproducers across all
three Create arms and both emission paths (`tests/attr_suite.rs`,
which also pins H5's chain-aware `get_attr`). Silent program
corruption now lands on the same tested relocation machinery as
reported program failure (`tests/review_h2_readback.rs`: a lying
block diverts the commit and data survives remount; a lying root
surfaces `Io` with prior state intact).

**Costs.** Every commit pays one read-back of its programmed bytes
(append: the appended region; compact paths: the rebuilt block
prefix) plus a CRC pass over RAM; against the erase that precedes
every compact program, noise. Rename grows a 1 KiB stack stage; the
documented `OutOfRange` divergence from C appears only above 1 KiB
of per-entry attribute payload, unreachable on geometries up to the
stage size and shouted, not swallowed, beyond it. CRC32 equality, not
byte equality, is the verification predicate, exactly as in C.

## Related

- Review findings H2, H6 (also pinned: H5); ADR-0014 (the worn-block
  fallbacks the verify failures divert into), ADR-0015
  (`for_each_live_attr`, the splice-correct enumeration the staging
  reuses; the splice/attr family).
- Oracle: vendored `lfs.c` (`lfs_rename` `LFS_FROM_MOVE`,
  `lfs_dir_commitcrc`, `lfs_bd_crc`) at `tools/gen_vectors/littlefs/`.
- Beads: lfs-inm (H6), lfs-efe (H2), lfs-h1p (V8 suite), lfs-e7i (H5
  closure).
