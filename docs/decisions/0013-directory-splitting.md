# ADR-0013: write-side directory splitting (HardTail continuation pairs)

- **Status**: accepted; implemented for subdirectories (`lfs-cvh.1`..`.4`).
  Root growth (`lfs-cvh.5`) and HardTail-chain relocation (`lfs-cvh.6`)
  follow.
- **Date**: 2026-05-29 (design); revised 2026-05-30 (implementation notes)

## Context

The LittleFS v2 format lets a directory span several metadata pairs
linked by `HardTail` tags (`SoftTail` threads the global list; the split
bit on the tail tag distinguishes them, see ADR-0012). This crate's
**reader** already follows that chain: `list_pair_chain` chases HardTails
with a Brent's cycle-safe walk and concatenates each pair's live entries,
and `collect_live_tree_pairs` (the deorphan tree set) follows HardTail
continuations as part of the tree. So a C-written split directory is read
faithfully.

The **writer** does not split. When a directory's entries no longer fit
in one metadata pair, the compacting commit overflows and the operation
fails with `Error::OutOfRange` (`tests/pending_dir_split.rs`
`confirm_overflow_is_the_current_limit`). Directories are therefore capped
at one pair's worth of entries. This is the root of two other documented
limits: the `MAX_QUEUED_PAIRS = 32` reachable-pair reject and the absent
HardTail-chain pair relocation both exist only because the writer never
produces HardTail chains.

## Decision

Implement write-side splitting so a directory grows across HardTail
continuation pairs, matching the C reference (`lfs_dir_split`,
`lfs_dir_splittingcompact`). Two architectural changes plus the split
itself:

### 1. Directory writes target the last pair of the chain

A new entry (or an entry update/remove) is committed to the pair that
holds the relevant id. New entries go to the **last** pair of the
directory's HardTail chain (the one with room), not the first. The write
path (`apply_op` / `write_inline_to_pair` / the `mkdir` parent target and
`remove_from_pair`) must walk HardTails from the directory's first pair to
the pair that owns the target id (for update/remove) or the last pair (for
create), then commit there. Lookup by name already needs the same chase.
This is the larger, more pervasive change; every directory write currently
assumes a single pair.

### 2. Overflow triggers a split, not an error

When a compacting commit on a pair would exceed the block, split instead
of erroring (`compact_and_program` / `build_compact_commit`). The C
reference's `lfs_dir_splittingcompact` halves the entry range until the
lower half fits about half a block (reserving ~40 bytes for the tail,
gstate, a move delete, and the CCRC), then allocates a continuation pair,
compacts the upper range into it, and writes the lower range into the
original. The crate has the inputs it needs: `gather_live_slots` yields a
`SlotOffsets` per live entry whose `name_len` + `struct_len` give the
entry's wire size (`12 + name_len + struct_len`: a Create tag, a NAME tag,
a STRUCT tag), so the split point is computed without extra reads.

### 3. Cascade and ordering

Splitting cascades: if the continuation also overflows it splits again
(the reproducer's 60 entries on a 256-byte block need several pairs). The
continuation is allocated and written **first**, then the original's
commit lands with a `HardTail` to it; a crash before the original's commit
leaves the continuation an unreferenced orphan reclaimed by the allocator,
exactly like the mkdir-create window. The continuation inherits the
original's prior tail (so the global thread and any further continuation
stay linked); the original's new tail becomes a HardTail to the
continuation (`split = true`).

### 4. IDs

Entry ids are local per pair (`gather_live_slots` numbers from 0 within a
pair); the reader concatenates across the chain in order, which is how
`list_pair_chain` already presents a split directory. The split assigns
the upper entries ids 0.. in the continuation and keeps the lower entries
0.. in the original. Lookup/update/remove resolve a name to a
`(pair, local_id)` by chasing HardTails.

### 5. Special cases (root and superblock)

`lfs_dir_splittingcompact` has a superblock-expansion path: when the root
pair (`{0,1}`) would overflow, it duplicates the superblock into a new
pair so the root can grow, guarded so a nearly-full device does not
expand. The root cannot relocate (it is fixed at `{0,1}`), so root growth
is the only way to add many root entries. This is the subtlest case and
should land last, behind a guard, with its own conformance vector.

## Consequences

**Wins.** Directories grow without bound (up to the reachable-pair
budget). Lifts the one-pair cap and, with the continuation pairs now
produced by this writer, makes HardTail-chain relocation reachable and
removes the rationale for the `MAX_QUEUED_PAIRS` reject being a *hard*
limit. SoftTail threading (ADR-0012) already accounts for HardTail
continuations in the tree set and the deorphan sweep, so splitting and
threading compose.

**Costs.** This is the most invasive write-side change in the crate: every
directory operation gains a HardTail chase to find the owning pair, and
the compact path gains split-point computation, continuation allocation,
and cascade, all crash-gated and conformance/round-trip verified. It is
comparable in scope to the SoftTail subsystem (ADR-0012) and warrants the
same gated, reproduce-first, one-commit-per-step treatment.

**Implementation order (proposed).** (1) HardTail chase in the directory
write/lookup/remove paths (target the owning / last pair; a no-op for
single-pair directories, so it lands green first); (2) split-point
computation from `SlotOffsets` sizes; (3) the split in
`compact_and_program` (continuation-first, then the original's HardTail
commit), single level; (4) cascade; (5) the root/superblock-expansion
special case; (6) HardTail-chain relocation (now reachable). Each
reproduce-first and conformance/round-trip gated; `pending_dir_split.rs`'s
`directory_grows_past_one_pair_via_split` target comes off `#[ignore]` at
step 4.

## Revision after implementation (2026-05-30)

Three findings refined the plan during `lfs-cvh.1`..`.4`.

**A single split suffices per op; the within-compaction cascade is
unreachable here.** The C reference cascades because `lfs_dir_commit`
batches many attrs into one commit, so a single compaction can add many
entries and overflow by more than one pair. This crate commits one
`WriteOp` at a time, adding at most one entry, and every metadata pair
already fits one block (the append path rejects anything larger). So the
combined sequence a compaction must place is at most one block plus one
entry, and one split always cuts it into two sub-block pairs: the upper
portion is bounded to half a block by `compute_split_index`, and the
lower portion is the pre-existing entries, themselves at most one block.
The outer cascade loop of `lfs_dir_splittingcompact` therefore never runs
a second iteration in this writer. Repeatedly splitting the *last* pair as
it fills builds the multi-pair chain, so the reproduce-first target
(`directory_grows_past_one_pair_via_split`, 60 entries) passed at the
single-split step (`lfs-cvh.3`), not a later cascade step. No speculative
cascade code was written: an unreachable second iteration would be
untestable through the public API, and the design favors total functions
over dead branches. Should a future feature batch multiple creates into
one commit, the cascade becomes reachable and must be added then, with a
test that exercises it.

**The reachable-pair budget needed a pre-split guard.** The mount-time
walks (allocator scan, gstate accumulation, deorphan) enumerate the
reachable forest into fixed `MAX_QUEUED_PAIRS` arrays, so a forest larger
than that bound is unmountable. A split's continuation allocation scans
the forest *before* the new pair exists, so at exactly `MAX_QUEUED_PAIRS`
reachable pairs the scan fits and the split lands, producing an
unmountable `MAX_QUEUED_PAIRS + 1` forest. The split branch now counts the
live tree first and refuses with `OutOfRange` at the budget, keeping the
image mountable; the directory caps cleanly one pair below the bound. The
same off-by-one affects `mkdir` creating the budget-th directory and is
filed separately (`lfs-43o`); it is pre-existing and orthogonal to
splitting. Lifting `MAX_QUEUED_PAIRS` from a hard limit (the Consequences
above) is thus still future work and gated on the stack budget (ADR-0006),
not delivered by splitting alone.

**The root is excluded until `lfs-cvh.5`, and step 5 needs a fullness
guard.** The split branch skips the root pair `{0, 1}`: it cannot relocate,
so a root overflow keeps its prior `OutOfRange` behavior. Enabling the
generic split for the root works correctly in isolation (the superblock
entry at id 0 stays in the lower half, `{0, 1}` remains the mount anchor
with a HardTail to the continuation, every entry reads back and the image
remounts), but a root continuation chain is *permanent*: unlike a freed
subdirectory, the root cannot be un-split, so its continuation blocks are
never reclaimed. On a small device the root chain then consumes enough
blocks that nothing else fits — exactly the case the C reference guards
with `lfs_dir_splittingcompact`'s superblock-expansion check (`do not
expand when the filesystem is more than ~88% full`). Step 5 therefore
needs that fullness guard (a free-block count before a root split) plus a
redesign of the root-fill reclaim regression (`tests/review_lookahead.rs`,
tuned for a single-pair root), not just removing the `pair_addr !=
self.root` guard.

The deeper blocker, though, is the **allocator's used-block scan**
(`lfs-fvw`). `scan_used_blocks` iterates raw `iter_tags` and marks every
`CtzStruct`/`DirStruct` chain, including entries a later Create/Delete
splice removed but whose struct tag is still physically present — a delete
or struct-update that appended rather than compacted. This is benign for
single-pair directories (they fill and compact, erasing the stale tags),
but a split directory keeps its pairs at half a block, so a delete has
room to append a Delete tag instead of compacting, and the freed entry's
`CtzStruct` chain stays over-marked until that pair next compacts. Under
heavy delete churn on a near-full split directory the over-marked blocks
are not reclaimed in time and a recreate fails with `OutOfRange` even
though space was freed. The fullness guard does not address this; the fix
is to make the scan splice-correct (mark only live entries' chains, as
`gather_live_slots` does). It is crash-safe — the scan reflects the
durable state on every mount and the in-memory freeing never persists — but
it is a core allocator change whose under-marking failure mode is a
double-allocation, so it warrants its own carefully-verified increment
(`lfs-fvw`) and gates step 5. This is the subtlest case, as this ADR
anticipated, and is deferred.

**A degraded-split fallback was required (`lfs-cvh`, post-`.4`).** Because
`compute_split_index` targets half a block, any compaction of a pair
holding more than half a block of live entries wants to split — and a pair
fills past half a block through in-place appends, so a removal or update
that triggers a compaction wants to split even though the remaining
entries still fit one block. On a full device that split cannot allocate a
continuation. Returning `OutOfRange` there wedged the filesystem: a user
could neither add nor remove files at capacity. The fix mirrors the C
reference's "unable to split" path: when a split is wanted but cannot
proceed (the reachable-pair budget is reached, or no free pair is
available), the compaction degrades to a single-block commit; only a
genuine over-one-block overflow still returns `OutOfRange`. The
reachable-pair budget guard (above) folds into the same fallback.
Pinned by `tests/dir_split_degraded.rs`.

## Related

- `tests/pending_dir_split.rs`: the reproduce-first target.
- `tests/dir_split_torn.rs`: the split crash-window power-loss pin.
- `tests/dir_split_budget.rs`: clean failure at the reachable-pair budget.
- `tools/verify_image` `split_dir` scenario + `roundtrip_split_dir`: the C
  reference reads a crate-written split directory.
- Review item `lfs-cvh`.
- C reference: `lfs_dir_split`, `lfs_dir_splittingcompact`,
  `lfs_dir_compact`, `lfs_dir_find_` in
  `tools/gen_vectors/littlefs/lfs.c`.
- ADR-0012 (SoftTail threading: the tail tag and the tree/deorphan
  handling splitting reuses); ADR-0005 (pair relocation, whose
  HardTail-chain case this unblocks); ADR-0009 (Brent's tail walk the
  reader already uses); ADR-0006 (stack budget the per-pair `SlotOffsets`
  array sits in).
