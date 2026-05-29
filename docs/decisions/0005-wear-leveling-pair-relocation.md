# ADR-0005: Inter-pair wear levelling via compact-time pair relocation

- **Status**: accepted
- **Date**: 2026-05-10

## Context

Each LittleFS metadata pair is two erase blocks between which the
filesystem rotates on every compact. That alternation distributes
wear within the pair: the active and alternate trade roles every
time the active fills and a fresh commit lands on the alternate.

What it does *not* distribute is inter-pair wear. A workload that
hammers one subdirectory (rewrite the same key, append metadata
attributes, rotate the same audit log) drives compactions in that
subdirectory's pair while every other pair on the device sits idle.
The two blocks of the hot pair burn through their erase budget; the
device dies long before its average erase count would predict.

The C reference solves this by relocating one block of a pair to a
freshly allocated block every `BLOCK_CYCLES` compactions. The
configured modulus is `((BLOCK_CYCLES + 1) | 1)` to avoid two
documented corner cases: `BLOCK_CYCLES = 1` would never terminate,
and any even `BLOCK_CYCLES = 2n` would, due to aliasing, only ever
relocate one of the pair's two blocks. The fresh block replaces the
pair's alternate slot in the compact write; after the standard
post-compact swap, the new block is active and the old active stays
as the new alternate. Over multiple cycles, both halves of the pair
rotate to fresh addresses.

For a non-root pair, the parent's `DirStruct` entry must be
updated to the new pair address. Without that update the parent
still points at the old pair, the freshly programmed block is
orphaned, and the relocation has no effect.

We deferred this work past v1.0 / v1.1 because it threads parent
context through every write path and has a recursive edge case (the
parent commit can itself trigger relocation). At v1.2 the rest of
the kernel is stable enough that the threading is tractable.

## Decision

Wear levelling fires at compact time, controlled by the `Storage`
trait's existing `BLOCK_CYCLES: i32` constant (`-1` disables;
positive values match the C reference modulus exactly). The
predicate

```rust
fn should_relocate(pair, root, new_revision, block_cycles) -> bool {
    pair != root && block_cycles > 0
        && new_revision % (((block_cycles as u32) + 1) | 1) == 0
}
```

runs once per compact. When it fires, the compactor:

1. Programs the compacted bytes to the existing alternate first.
   This is the durability boundary: after this step lands, the
   commit is reachable through the parent's unchanged reference
   because the alternate now has the higher revision.
2. Allocates a fresh block using the single-buffer scan helper
   `alloc::alloc_one_block_with_single_buf` (the alternate buffer
   already holds the compacted bytes, so only the source buffer is
   available as BFS scratch).
3. Programs the same compacted bytes to the fresh block.
4. Returns the new pair address `(active_addr, fresh_addr)` to the
   caller.

The caller (the compact dispatch point) then walks the tree from
root via `find_parent_in_tree` to locate the entry whose
`DirStruct` body matches the old pair, and applies a new
`WriteOp::UpdateDirStruct { id, new_pair }` to that parent. The
parent's own commit goes through the same compact-or-relocate
dispatch and may recurse. Recursion terminates either at a
compact that fits inline (no compaction needed at the parent) or
at the root (excluded by the predicate).

The root pair is excluded because the LittleFS v2 spec pins it to
blocks `(0, 1)` so a cold reader can find the superblock from a
single deterministic starting point. Relocating root would require
either a new superblock-pointer block or a v2 spec extension; both
are out of scope.

The predicate excludes only `pair_addr == root`. Continuations of
a directory's `HardTail` chain are not specifically protected, but
our writer never *creates* tail-threaded pairs (every directory
fits in one pair until `MAX_LIVE_ENTRIES = 256`). Reading
conformance images that contain `HardTail` chains is still
supported; relocating one of those chains is unreachable in
practice and not exercised.

## Consequences

**Wins.**

- Inter-pair wear is now bounded by the same `BLOCK_CYCLES` budget
  as within-pair wear. A workload that hammers one subdirectory no
  longer fixes that pair's two physical blocks; over time the
  blocks rotate through the device's free pool, so erase counts
  even out across the whole filesystem.
- The C reference's `BLOCK_CYCLES` constant on the `Storage` trait
  is now load bearing rather than advisory. Downstream consumers
  that previously copied it over from C littlefs's `lfs_config`
  see the documented behaviour.
- The implementation reuses every existing primitive: BFS
  walk-from-root, slot replay through `build_compact_commit`,
  `WriteOp` dispatch, allocator scan. No new persistent state, no
  new spec extensions, no on-disk format change beyond emitting a
  `DirStruct` tag with a fresh body in the parent's commit log.
- Centralisation: every compact site now dispatches through one
  `compact_and_program` helper, so future write-path changes have
  one place to add behaviour rather than seven.

**Costs.**

- Each relocation costs one extra erase plus one extra program
  (the compacted bytes are written to two blocks: the existing
  alternate and the fresh block). The alternate write is wasted
  in the relocation case (the alternate becomes orphaned once the
  parent update lands). The waste is a per-cycle constant, paid
  every `BLOCK_CYCLES` compactions; the alternative (skip the
  alternate write entirely and rely on the parent update for
  atomicity) creates a crash window between the fresh program and
  the parent update during which the commit is unreachable. We
  pay the constant for the durability guarantee.
- A relocation requires a free block. On a nearly-full device the
  allocator may return `Error::OutOfRange`; the operation then
  fails the same way a regular allocation does. Mitigation: keep
  a reserve of free blocks proportional to the working set, or
  set `BLOCK_CYCLES = -1` to opt out.
- A crash between the fresh-block program and the parent update
  leaves the fresh block orphaned on disk. The next allocator
  scan reclaims it (the BFS walk from root never visits the
  orphan, so it stays unmarked in the bitmap). No corruption, no
  user-visible loss; the relocation just doesn't happen this
  cycle and the predicate will fire again at the next
  `BLOCK_CYCLES` boundary.
- After a *successful* relocation whose parent update took the
  in-place append branch, the orphaned old alternate block is not
  reclaimed promptly. The parent's superseded `DirStruct(id ->
  old_pair)` tag stays in its commit log until the parent next
  compacts, and `alloc::scan_used_blocks` enumerates the parent
  via the raw committed tag stream (not the splice-correct
  latest-wins view that `accumulate_gstate` uses), so it re-marks
  the old alternate as in-use until that compaction. This is a
  benign over-approximation, not a leak that grows unboundedly:
  the allocator stays self-consistent (the block is held, never
  double-handed-out), the read resolver is latest-wins so
  `old_pair` never resolves live, and the worst edge (a stale
  enqueue exhausting `MAX_QUEUED_PAIRS`) degrades to a clean
  `Error::OutOfRange`. Promptly reclaiming the block would require
  teaching the safety-critical reachability scan a latest-wins
  `DirStruct` view; the deferred reclaim is the conservative
  choice and is recorded here rather than changing that scan. See
  review item `lfs-7ts` (2026-05-29).
- `find_parent_in_tree` is O(reachable pairs) per relocation. With
  `MAX_QUEUED_PAIRS = 32` the worst case is a 32-pair walk per
  parent update; for typical workloads (under a dozen
  directories) the walk is much shorter. The constant factor is
  acceptable for an operation that fires once every
  `BLOCK_CYCLES` compactions.
- The C reference aligns a pair's initial revision count to the
  relocation modulus so the first relocation lands a fixed phase
  into the pair's life; this writer starts revisions at 1 and does
  not phase-align. The effect is a benign cadence shift: the same
  number of relocations happen over the device's life, just on a
  different schedule, and an image written by either is read by the
  other (the modulus governs *when* a relocation fires, not the
  on-disk shape). Noted so the difference is not later mistaken for
  a fidelity bug.

**Explicitly out of scope.**

- **Relocating `HardTail`-threaded continuation pairs.** Our
  writer never emits `HardTail` tags. A continuation pair's
  "parent" is the predecessor in the chain, not a `DirStruct`
  entry, so `find_parent_in_tree` would return `None` and
  relocation would fail. Conformance images that contain tail
  chains can be read; mutating them through this kernel does not
  introduce continuations, so the case is unreachable in
  practice. If a future caller needs tail-threaded mutation, the
  fix is to widen `find_parent_in_tree` to also return the
  predecessor's tail tag and add a `WriteOp::UpdateTail` variant.
- **Tuning `BLOCK_CYCLES` per workload.** We expose the constant
  on the `Storage` trait and ship the C reference default of
  `500`. Choosing a value (or the disable sentinel `-1`) is a
  storage-provider concern; the kernel does not impose a policy.

**Mount-time orphan recovery (now shipped in v0.3.0).** The
original draft of this ADR deferred orphan recovery on the
grounds that a torn relocation is a benign missed cycle (the
alternate write is the durability boundary, so user data is
safe; the wear-levelling benefit is forfeited until the next
predicate firing). v0.3.0 took the stricter path. Each
relocation now embeds a balanced `RelocateState` tag (16-byte
body: `old_pair` LE pair + `new_pair` LE pair) on the alternate
program, on the freshly allocated block, and on the parent's
`UpdateDirStruct` commit. The three contributions XOR to zero
once all three land. A crash that lands fewer than three leaves
a non-zero filesystem-global RelocateState aggregate;
`Fs::mount` walks every reachable pair via the splice-correct
live-entries view, XOR-accumulates every committed
`RelocateState` body, and emits a balancing commit on `old_pair`
that cancels the cycle. The forfeited fresh block becomes
orphan and is reclaimed by the next allocator scan. The pattern
mirrors the cross-dir rename pattern in `gstate.rs`; the
`Gstate` struct now carries both the 12-byte `MoveState` triple
and the 16-byte `RelocateState` quadruple side by side. Covered
by `relocation_atomic_across_every_power_loss` in
`tests/wear_leveling.rs`.

## Related

- C reference's `lfs_dir_needsrelocation` (`lfs.c:1911`) and the
  relocation goto in `lfs_dir_compact` (`lfs.c:1946`).
- C reference's `lfs_fs_parent` (`lfs.c:4790`) for the
  parent-finder pattern; we walk tree from root via
  `DirStruct`/`HardTail` references rather than the global tail
  chain (which our writer doesn't maintain).
- ADR-0003 (verification stacks): wear levelling is covered by the
  unit-test and property-test stacks; Kani harnesses for the
  predicate's totality are an open follow-up.
- `tests/wear_leveling.rs` for the seven integration tests (six
  behavioural plus one power-loss sweep).
- `KNOWN_ISSUES.md` entry "Wear leveling via pair relocation"
  now marked complete.
