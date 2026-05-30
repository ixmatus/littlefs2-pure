# ADR-0013: write-side directory splitting (HardTail continuation pairs)

- **Status**: proposed (design; implementation is `lfs-cvh`)
- **Date**: 2026-05-29

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

## Related

- `tests/pending_dir_split.rs`: the reproduce-first target.
- Review item `lfs-cvh`.
- C reference: `lfs_dir_split`, `lfs_dir_splittingcompact`,
  `lfs_dir_compact`, `lfs_dir_find_` in
  `tools/gen_vectors/littlefs/lfs.c`.
- ADR-0012 (SoftTail threading: the tail tag and the tree/deorphan
  handling splitting reuses); ADR-0005 (pair relocation, whose
  HardTail-chain case this unblocks); ADR-0009 (Brent's tail walk the
  reader already uses); ADR-0006 (stack budget the per-pair `SlotOffsets`
  array sits in).
