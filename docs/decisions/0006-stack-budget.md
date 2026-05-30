# ADR-0006: pin the per-call scratch stack budget rather than restructure it

- **Status**: accepted
- **Date**: 2026-05-15

## Context

A post v1.0 review flagged two large fixed scratch arrays the kernel
places on the stack:

- `dir::lookup` stacks a `[LookupSlot; MAX_LIVE_ENTRIES]` local.
  `LookupSlot` carries pointers, so its size is pointer width
  dependent: 24 bytes on the 32-bit `thumbv6m-none-eabi` ship target
  and 48 on a 64-bit host. With `MAX_LIVE_ENTRIES = 256` that is a
  6144 byte (6 KiB) array on the ship target, 12 KiB on a host, on the
  frame of every name lookup.
- `fs::gather_live_slots` operates on a `[SlotOffsets; MAX_LIVE_ENTRIES]`
  passed by reference. The array is not allocated in that function, but
  every caller stacks it as a local first. At
  `size_of::<SlotOffsets>() = 10` that is 2560 bytes (2.5 KiB) per
  caller frame.

The `SlotOffsets` array is the load bearing case. One caller is
`Fs::apply_op_to_pair_inner`, which holds its 2.5 KiB array while it
calls `Fs::propagate_relocation`, which calls `apply_op_to_pair_inner`
again to rewrite the `DirStruct` tag in the parent during a wear
levelling relocation. The arrays are therefore not merely large, they
are stacked concurrently down a recursion whose depth is bounded by the
directory tree, on a target (`thumbv6m-none-eabi`, Cortex M0+) whose
total stack is small.

The obvious alternative, hoisting both arrays into caller supplied
buffers threaded through the public API, is a behaviour preserving
refactor in principle but a wide one in practice: it touches the
resolve, rename, rmdir, compaction, and relocation paths, all of which
are already covered by power loss and torn write tests that assume the
current call shapes. Doing that mid 1.x, against a stack overflow that
has not been demonstrated on the ship target, trades a real regression
risk for a speculative gain.

## Decision

Pin the scratch budget with compile time guards and document it at the
call sites and here, rather than restructure the buffers in the 1.x
line.

A `const` assertion next to `MAX_LIVE_ENTRIES` (in `fs`) pins
`SlotOffsets` at its exact 10 byte size; that struct has only integer
fields, so the count is the same on every target. A second assertion
next to `LookupSlot` (in `dir`) bounds it by the sum of its three
documented fields plus one machine word of slack, rather than an
absolute byte count: `LookupSlot` carries pointers, so an absolute
count would be wrong on either the 32-bit ship target or a 64-bit
host, while the compositional bound holds on both and still trips if a
fourth field is added. The `gather_live_slots` and `lookup` doc
comments state the budget and the recursion interaction and point
here. The figures are read from `core::mem::size_of` on both targets,
not estimated.

## Consequences

**Wins.** The budget is now a checked fact, one tier below a type and
above prose: a future change that grows `SlotOffsets` or `LookupSlot`
cannot silently inflate the worst case stack, it fails CI. A maintainer
reading either function sees the budget and the recursion without
reconstructing it. No behaviour changes, so the existing power loss and
relocation tests remain valid evidence.

**Costs.** The worst case stack is still proportional to directory tree
depth: each `apply_op_to_pair_inner` level on a cascading relocation
adds roughly its 2.5 KiB `SlotOffsets` frame plus other locals, and the
`lookup` peak (6 KiB on the ship target) is transient but real. There
is no hard recursion
depth cap in code; termination relies on the recursion ending at the
root pair (which never relocates) or at the first parent whose commit
fits inline without itself relocating. The common case is depth 1 or 2;
a deeply nested tree with cascading relocations is the unmitigated worst
case. A consumer on a severely stack constrained target must size the
stack against tree depth, not against a constant.

Quantified: the recursion depth is bounded above by the number of
reachable metadata pairs, which mount caps at `MAX_QUEUED_PAIRS = 32`
(see ADR-0009; an image exceeding it is rejected at mount). The
unmitigated worst case is therefore on the order of `32 * 2.5 KiB`,
roughly 80 KiB of concurrent `SlotOffsets` frames, plus the transient
6 KiB `lookup` peak, on the `thumbv6m-none-eabi` ship target. This is
an upper bound, not a typical figure (real depth is 1 or 2); it exists
so a constrained consumer has a concrete number to size against rather
than an open-ended "tree depth."

**Explicitly out of scope.** This ADR does not introduce a recursion
depth limit, does not move the scratch arrays to caller supplied
buffers, and does not change `MAX_LIVE_ENTRIES`. A 2.x revision is free
to adopt caller supplied scratch (the doc notes already flag it); that
is a breaking API change and is deliberately deferred.

## Revision after directory splitting (2026-05-30, lfs-cvh / lfs-fvw)

The write-side directory-splitting arc added two scratch users to the
write path, both pinned by `const` assertions in the same spirit as the
originals.

- `alloc::scan_used_blocks` (the used-block scan) was made splice-correct
  (lfs-fvw) and now stacks a `[LiveStruct; MAX_LIVE_ENTRIES]` local. At
  `size_of::<LiveStruct>() = 12` that is 3072 bytes (3 KiB), pinned by a
  guard next to `LiveStruct`. Crucially it is **not** multiplied down the
  relocation recursion: the scan is an iterative BFS that runs to
  completion and returns before `propagate_relocation` recurses, so at most
  one such array is live at a time. It adds 3 KiB once to the worst-case
  peak, alongside the transient 6 KiB `lookup` array, not `32 * 3 KiB`.

- The split path and `mkdir` gained reachable-pair budget checks
  (`collect_live_tree_pairs`, lfs-cvh.4 / .5 / lfs-43o) which stack a
  `[SlotOffsets; MAX_LIVE_ENTRIES]` (the existing 2.5 KiB array) for the
  duration of the walk. These run before the split / new-pair commit and
  do not recurse into `apply_op_to_pair_inner`, so they too are not
  multiplied by depth; they add at most one extra 2.5 KiB frame transiently
  at the level performing the split.

The recursion-bounded `32 * 2.5 KiB` ≈ 80 KiB `SlotOffsets` worst case from
the relocation cascade still dominates; the splitting arc raises the
transient peak by roughly 3 + 2.5 KiB, not by a factor of depth. The
out-of-scope note stands: a 2.x revision adopting caller-supplied scratch
would subsume all of these arrays.

## Related

- `src/fs.rs`: `gather_live_slots`, `apply_op_to_pair_inner`,
  `propagate_relocation`, and the `SlotOffsets` size guard.
- `src/alloc.rs`: `scan_used_blocks`, `gather_live_structs`, and the
  `LiveStruct` size guard (lfs-fvw).
- `src/dir.rs`: `lookup` and the `LookupSlot` size guard.
- ADR-0005 (wear levelling pair relocation), which introduced the
  `propagate_relocation` recursion this budget accounts for.
- Post v1.0 review item M2.
