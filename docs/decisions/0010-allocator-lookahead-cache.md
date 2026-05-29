# ADR-0010: block-allocator lookahead cache

- **Status**: accepted
- **Date**: 2026-05-29

## Context

Every block-allocating write re-derived the in-use set from scratch.
`alloc_blocks` / `alloc_blocks_excluding` called `scan_used_blocks`, a
BFS from the root that reads both blocks of every reachable metadata
pair and walks every CTZ chain, then picked free blocks from the
resulting bitmap. The cost is `O(reachable blocks)` of flash I/O per
allocation, paid on every `File::write` overflow, `append_to_path`
overflow, `mkdir`, and CTZ file create, regardless of how few blocks the
operation actually needs.

The 2026-05-29 review measured this with a storage-operation-count
harness (`tests/bench_perf_backlog.rs`, Bench A): a single small CTZ
write costs `reads = 2 * reachable_pairs` for the scan alone, rising
from 12 reads at one reachable pair to 68 at 29 pairs, with the actual
write work constant at four programs. The `Storage::LOOKAHEAD_SIZE`
constant existed in the trait but the allocator ignored it, so the
declared lookahead facility was a half-implementation.

The C reference avoids the re-scan with a persistent lookahead window
advanced across allocations. The risk in adopting any such cache on a
no-alloc kernel is correctness: a cache that ever reports a live block
as free hands the same block to two owners and corrupts the filesystem.

## Decision

Keep an over-approximating used-block bitmap on the `Fs` and serve
allocations from it, refreshing from an authoritative scan only on a
miss. Implemented as `alloc::alloc_blocks_cached` and
`alloc::alloc_one_block_cached_single_buf`, backed by a new
`Fs::used_cache: Option<Bitmap>` field.

The cache is safe **by construction** because it is only ever an
over-approximation of in-use blocks: it may mark a freed block as still
used (benign: that block is simply not reused until the next rescan) but
it can never mark a live block as free. Three properties hold that
invariant:

1. Every block handed out is marked used in the cache before return
   (`take_free_blocks`), so one allocation can never return a block a
   prior allocation already took.
2. A cache miss, or a request the cached over-approximation cannot
   satisfy, rescans from the authoritative on-disk state with
   `scan_used_blocks` and re-applies the caller's exclusions, the exact
   basis the uncached path used.
3. The cache is RAM-only and starts at `None` at every mount, so it can
   never carry stale state across a power cycle.

Staleness (freed blocks lingering as used) is the only failure mode, and
it is self-correcting: when the cache cannot satisfy a request it
rescans, reclaiming everything freed since. Invalidating the cache after
an operation that frees blocks (`remove`, `rmdir`, `truncate`, `rename`,
and a `File` sync that supersedes a chain) is a promptness optimization
only; a missed invalidation costs an extra rescan, never correctness.

In-flight chains (a stateful `File`'s newly allocated but not-yet-
committed blocks) are handled without an explicit exclusion list on the
hot path: those blocks were marked when this same allocator handed them
out, so a cache hit already excludes them. On the rescan path, which an
authoritative scan would not see them in, the caller names the chain via
the `exclude_chain` parameter and the allocator walks it once (the scan
is already `O(reachable)`, so the walk is the same order). See ADR-0011.

## Consequences

**Wins.** Steady-state allocation drops from `O(reachable blocks)` of
flash I/O to an in-RAM bitmap scan. Bench A after the change: the
allocating write is flat at about 10 reads from one reachable pair to
29 (the cold-cache first allocation still pays one scan), versus 12 to
68 before. The declared `LOOKAHEAD_SIZE` intent is now honored in
spirit (a persistent free-block view), closing the half-implementation.

**Costs.** The `Fs` grows by one `Option<Bitmap>`, 513 bytes
(`MAX_TRACKED_BLOCKS / 8` plus the discriminant). Unlike a per-`File`
chain cache (ADR-0007), there is exactly one `Fs` per mount, so this is
a single fixed cost, not a per-handle one, and it sits within the
ADR-0006 stack budget. Churny delete-then-create workloads that outrun
the invalidation hooks fall back to rescan-on-exhaustion, i.e. to the
pre-change behavior, never worse.

**Verification.** The full suite (including the wear-levelling,
power-loss, and atomic-move churn that would expose a double
allocation) passes unchanged. `tests/review_lookahead.rs` adds a
create / delete / re-create churn stress test that can only succeed by
reclaiming freed blocks, with content integrity checked through a
remount.

## Related

- `src/alloc.rs`: `Bitmap`, `take_free_blocks`, `alloc_blocks_cached`,
  `alloc_one_block_cached_single_buf`, `scan_used_blocks`.
- `src/fs.rs`: `Fs::used_cache`, `Fs::invalidate_alloc_cache`, the five
  allocation sites and the free-site invalidations.
- `tests/bench_perf_backlog.rs` (Bench A): the measurement.
- `tests/review_lookahead.rs`: the churn / reclaim integrity test.
- ADR-0006 (stack budget); ADR-0011 (CTZ append seek, which relies on
  this cache for in-flight chain exclusion).
- 2026-05-29 review item `lfs-opt`.
