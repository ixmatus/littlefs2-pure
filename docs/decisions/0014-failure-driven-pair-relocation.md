# ADR-0014: failure driven metadata pair relocation

- **Status**: accepted; implemented (`lfs-23f` Part 2)
- **Date**: 2026-05-30

## Context

ADR-0005 added wear levelling: every `BLOCK_CYCLES` compactions a metadata
pair relocates one of its blocks to a freshly allocated block, evening out
erase counts across the device. That relocation is scheduled by a revision
predicate; it never fires because a write failed. Everywhere else the commit
path mapped each `Storage::program` and `Storage::erase` error to
`Error::Io` and propagated it, so a single worn block underneath a metadata
commit was fatal: the directory operation died even though the rest of the
device was healthy.

The C reference instead relocates the pair when a commit fails on a worn
block (`lfs_dir_relocatingcommit`, `lfs_dir_compact`), so one bad block is
recoverable rather than terminal. Part 1 of `lfs-23f` brought that property
to CTZ data blocks (the initial write and the streaming append paths). This
ADR records Part 2: the same property for the metadata pair itself.

The wear path's crash safety does not carry over. Wear writes the compacted
bytes to the pair's existing alternate first. That program is the durability
anchor: once it lands, the new commit is reachable through the parent's
unchanged `DirStruct`, because the alternate now outranks the active by
revision. A worn alternate is precisely the block that program cannot write,
so the anchor step is the one that fails. A failure driven relocation must
land its commit somewhere the worn block is not, under a different crash
safety argument.

## Decision

A failure driven relocation writes the compacted commit only to a freshly
allocated block, with no in place anchor; the parent's `DirStruct` repoint is
the sole linearization point.

The new pair keeps the pair's good block and replaces the worn (victim) block
with the fresh one. A single helper, `relocate_compact_to_fresh`, serves every
failure site. It allocates a fresh block (excluding the pair's two blocks, any
inflight blocks from a relocation cascade, and every fresh candidate already
found worn), rebuilds the compacted commit carrying the `RelocateState` body
that encodes the `(pair, new_pair)` migration, programs it to the fresh block,
and returns the new pair address so the existing `propagate_relocation` repoints
the parent exactly as the wear path does. A worn fresh candidate is excluded and
retried, bounded by `MAX_BAD_BLOCK_RETRIES`; exhaustion returns `Error::Io`,
never an unbounded loop. The on disk shape is identical to a wear relocation, so
`build_relocate_body`, `propagate_relocation`, `accumulate_gstate`, and
`recover_pending_relocation` are reused unchanged.

Which block is the victim depends on the failure site:

| Failure site | Worn block | Kept | New pair |
|---|---|---|---|
| Plain compaction (the alternate program fails) | alternate | active | `{active, fresh}` |
| Append fallback (the in place append program fails) | active | alternate | `{alternate, fresh}` |
| Directory split (the lower half program fails) | alternate | active | `{active, fresh}`, lower half still carrying the `HardTail` to the continuation |

For the append case the active block is only read during a compaction, so a
block worn for writes is still rebuilt onto a fresh block from its readable
content; the kernel eagerly evicts it rather than deferring (see the append
fallback note below). A directory split additionally writes the continuation:
a worn continuation block is relocated past in place (the continuation is
unreferenced until the lower half commits, so a failed attempt is a clean blank
orphan, excluded and reallocated). The root pair `{0, 1}` is the fixed
superblock anchor and cannot relocate, so a worn root commit stays `Error::Io`.

## Crash safety

The fresh only model needs no mount time recovery, and that is the load
bearing property. Because the commit lands only on the fresh block, which is
unreferenced until the parent repoints at it, the number of `RelocateState`
bodies reachable from the root is always zero (before the repoint) or two
(the fresh block and the parent, after it), and they cancel under XOR. It is
never exactly one, the imbalance that `recover_pending_relocation` exists to
cancel. So that recovery never fires for this path, which is essential:
firing it would commit a balancing tag onto the old pair, whose worn half
cannot accept a write, and the mount would fail or loop.

The linchpin is `MetadataPair::parse`: it selects the active block as the
higher revision block that also carries a verified CCRC. A blank, worn, or
torn block has no verified CCRC, so it is invisible both to the read resolver
and to `accumulate_gstate` (which folds only the active block's tag stream).
Therefore, before the parent repoint, the pair reads as its pre commit state
(the kept good block outranks the blank or worn victim); after the repoint,
the fresh block wins and the pair reads as the post commit state. A crash
before the repoint mounts as the pre state with the fresh block reclaimed as
an orphan; a crash after it mounts as the post state. Both are valid, never a
partial commit. Atomicity is unchanged from the caller's view, since
`apply_op_to_pair_inner` only syncs and returns after the parent repoint.

This contrasts with the wear path, which keeps its alternate anchor: there a
crash before the parent repoint mounts as the post state (the commit is
durable on the anchor), leaving a single reachable body that recovery cancels.
The failure path trades that earlier durability, which the caller cannot
observe anyway, for never touching the worn block.

## Consequences

**Wins.**

- A single worn block under a metadata commit is recoverable rather than
  fatal, completing the fault tolerance Part 1 began for CTZ data blocks. A
  directory operation survives a worn alternate, a worn continuation, and a
  worn active block.
- The on disk encoding and the gstate machinery are reused verbatim, so the
  feature adds no new tag type, no new recovery path, and no format change.
  The wear path and the failure path differ only in whether the alternate
  anchor is written.
- The wear and failure paths compose: when a wear scheduled relocation's
  alternate is itself worn, the already allocated wear fresh is reused as the
  failure path's first candidate, so the overlap never allocates or orphans
  two blocks.

**Costs.**

- Each failure relocation consumes one fresh block (a split that relocates
  also consumes the two continuation blocks). On a nearly full device the
  allocation can return `Error::OutOfRange`, the same way a wear relocation
  can.
- The worn block stays marked in use until the root next compacts, through
  the same superseded `DirStruct` over approximation ADR-0005 documents
  (`lfs-7ts`). This is benign: the block is held, never double handed out, and
  is not re marked as a child once the parent points at the new pair.
- A device whose worn blocks exceed `MAX_BAD_BLOCK_RETRIES` fresh candidates
  fails the commit with `Error::Io`. The bound is a backstop against a wholly
  failing device, not a tuned capacity.
- A crash before the parent repoint discards the in flight operation (the pre
  state). This is the correct atomicity contract (the operation had not
  returned success), but it differs from the wear path, which preserves the
  commit on a crash because it had already anchored it.

**Explicitly out of scope.**

- **A worn root pair.** The root is pinned to blocks `{0, 1}` by the spec and
  has no parent to repoint, so a worn root commit is `Error::Io`. Surviving it
  would need a superblock pointer indirection the v2 format does not provide.
- **Eager eviction when a worn append also overflows.** The append fallback
  evicts the worn active onto a fresh block in the common case where the live
  set fits one block (which it always does for a single pair, since pairs split
  at half a block during growth). The rare case where the same commit would
  also split is left to a normal split, which writes the good alternate and
  defers the worn block's eviction to the next compaction that targets it; that
  is correct, just one cycle later.

## Related

- ADR-0005 (wear levelling pair relocation and `RelocateState` orphan
  recovery): the design this extends and reuses.
- ADR-0013 (write side directory splitting): the split path whose lower half
  write this teaches to relocate.
- `lfs-23f` Part 1 (CTZ data and append bad block relocation): the sibling
  feature for file data blocks.
- C reference `lfs_dir_relocatingcommit` and the relocate path in
  `lfs_dir_compact` (`tools/gen_vectors/littlefs/lfs.c`): the behavioural
  oracle.
- Tests: `tests/pending_badblock_reloc.rs` (plain compaction and append
  fallback), `tests/badblock_split_reloc.rs` (split relocation),
  `tests/badblock_reloc_crash.rs` (bounds, gstate balance, and the power loss
  sweep).
