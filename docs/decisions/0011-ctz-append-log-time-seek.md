# ADR-0011: CTZ append via log-time skip-list seek

- **Status**: accepted
- **Date**: 2026-05-29

## Context

ADR-0007 measured the per-append chain re-walk in `stream_ctz_extend`
and declined to fix it, because the only fix considered, a per-`File`
chain cache, would add about 1 KiB to every `File` handle and work
against the ADR-0006 stack budget. The walk was judged a bounded,
sub-dominant cost on a 128 KiB device holding a single file.

The 2026-05-29 review re-measured with a storage-operation-count
harness (`tests/bench_perf_backlog.rs`, Bench B) rather than wall-clock,
and the cost was sharper than the timing harness implied: reads per
single-block append rise linearly with chain length, from 2 at chain
length 1 to 82 at chain length 200, i.e. `O(n)` per append and `O(n^2)`
to build a file. `stream_ctz_extend` called `collect_chain_blocks` (a
full backward walk) on every invocation. That walk served three
purposes: the tail block address for the in-place fill, the existing
block addresses for the new blocks' skip pointers, and the in-flight
chain as the allocator's exclusion list.

A `set_len` zero-extend amplified this further: it looped
`stream_ctz_extend` in 64-byte chunks, so a 16 KiB extend re-walked a
growing chain 256 times for 4334 reads (Bench C).

## Decision

Eliminate the full chain walk from the append hot path, keeping
ADR-0007's no-per-`File`-cache decision intact (no `File` growth). Three
coordinated changes:

1. **Tail address.** The in-place tail fill targets the chain head,
   whose address is already in the `CtzStruct`, so step one needs no
   walk.

2. **Skip-pointer targets.** A new `ctz::seek_block` descends the
   skip-list from the head to any earlier index in `O(log n)` reads
   (follow the largest available jump `2^k` at the current block that
   does not undershoot the target). The new blocks' pointers to existing
   blocks are resolved by `seek_block`; pointers to other new blocks
   come from the freshly allocated set. `seek_block` is verified against
   the full-walk oracle `collect_chain_blocks` for every index of a
   200-block chain (`tests/review_seek.rs`).

3. **In-flight exclusion.** The allocator (ADR-0010) takes over excluding
   the uncommitted chain. On a cache hit the chain's blocks are already
   marked (they were marked when handed out), so no walk occurs. Only on
   a rescan does `alloc_blocks_cached` walk the named `exclude_chain`
   once, which is the same order as the rescan it accompanies.

A `set_len` zero-extend also now fills from a 1 KiB shared `static` zero
buffer instead of a 64-byte stack buffer, cutting the number of
`stream_ctz_extend` calls (and therefore chain operations) on a large
extend by an order of magnitude.

## Consequences

**Wins.** Append is no longer quadratic. Bench B after the change:
reads per single-block append fall to 0 to 10 across chain lengths 1 to
200 (most appends 0 to 3; the head and recently allocated blocks need no
seek, and the allocator hits the cache), versus 2 to 82 before. Bench C
(16 KiB `set_len` zero-extend): 4334 reads before, 48 after, combining
the wider chunk, the seek, and the lookahead. No `File` struct growth:
the transient `[BlockAddress; MAX_CTZ_WRITE_BLOCKS]` work array in
`stream_ctz_extend` is unchanged in size (the old collected chain is
simply replaced by the newly-allocated set), so the ADR-0006 budget and
ADR-0007's decision both hold.

**Costs.** `stream_ctz_extend` now issues `O(log n)` seek reads per new
block whose skip pointers reference existing blocks (chiefly the first
new block of an append), instead of one amortized share of a single
`O(n)` walk. For appending many blocks at once this is more reads than
the single walk would have been; the win is in the many-small-appends
pattern the benchmark targets and ADR-0007 flagged. `seek_block` is new
surface on the read side of the skip-list and is pinned by the oracle
test. The `set_len` shrink path (`shrink_ctz_head`) still uses
`collect_chain_blocks`; truncate is not a hot path and was left
unchanged.

**Relationship to ADR-0007.** ADR-0007's negative result (do not add a
per-`File` chain cache) is preserved, not reversed: this change reaches
`O(log n)` appends without any per-handle cache, via a stateless seek
plus the single shared allocator lookahead. ADR-0007's benchmark
(`tests/bench_ctz_append.rs`) remains valid as a wall-clock companion to
the op-count harness here.

## Related

- `src/ctz.rs`: `seek_block` (and the `collect_chain_blocks` oracle).
- `src/fs.rs`: `stream_ctz_extend`; `src/file.rs`: `set_len` zero-extend
  chunk.
- `tests/review_seek.rs`: the seek-versus-oracle property test.
- `tests/bench_perf_backlog.rs` (Bench B, Bench C): the measurements.
- ADR-0007 (no per-`File` chain cache, preserved); ADR-0010 (lookahead
  cache, which supplies the in-flight exclusion); ADR-0006 (stack
  budget).
- 2026-05-29 review item `lfs-o72`.
