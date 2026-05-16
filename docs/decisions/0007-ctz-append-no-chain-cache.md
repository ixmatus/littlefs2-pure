# ADR-0007: no per-File CTZ chain cache (bench-gated, negative result)

- **Status**: accepted
- **Date**: 2026-05-15

## Context

A post v1.0 review observed that `File::write` in append mode calls
`Fs::stream_ctz_extend` once per write, and that function calls
`collect_chain_blocks`, which walks the existing CTZ chain (about `n/2`
small skip-pointer reads for an `n`-block file) every time. For N
sequential single-block appends the walk cost is the arithmetic series:
O(N^2) small reads. The review proposed caching the chain in the
stateful `File` handle so repeated appends do not re-walk, turning the
append sequence into O(N).

The proposed cache is not free. The chain is up to
`MAX_CTZ_WRITE_BLOCKS = 256` block addresses; caching it in every
`File` adds roughly 1 KiB to the handle, which lives on the caller's
stack. ADR-0006 had just pinned the kernel's stack budget on the
`thumbv6m-none-eabi` ship target; a 1 KiB `File` growth works directly
against that. The cache would also need correct invalidation across
seek, truncate, splice, and wear-levelling relocation, each already
covered by power-loss tests that assume the current shapes.

Per the project's performance discipline, a performance patch lands
only on a reproducible win against a targeted benchmark; a neutral
measurement reverts and the ADR entry is the deliverable. So the work
was gated on a measurement taken before any kernel change.

## Decision

Do not add a per-`File` CTZ chain cache in the 1.x line. Keep the
benchmark; record the negative result here.

`tests/bench_ctz_append.rs` (zero dependency, `std::time`, `#[ignore]`)
times N appends through one stateful `File` handle on a 128 KiB RAM
device, at N = 50, 100, 200, 250 (250 is near the 256-block cap), three
trials each. Per-append time stays essentially flat as N grows: on a
quiet machine about 0.6 us at N = 50 rising only to about 0.9 us at
N = 250, and under load about 2.0 to 3.3 us with no upward trend in N
at all. A genuinely O(N^2) per-append cost would scale with N: a 5x
larger N would cost roughly 5x per append. The observed rise is about
1.5x for a 5x N, so the chain walk is a small sub-dominant term, not
the cost driver; total time scales close to linearly with N.

The O(N^2) term is therefore real in the abstract but does not drive
cost in practice. Two reasons. First, the constant per-append cost
(erasing and programming a fresh ~256 byte block, plus `alloc_blocks`'
filesystem scan) dwarfs a walk of a few hundred 4 to 8 byte reads.
Second, the kernel caps a writable CTZ chain at 256 blocks, so the
sub-dominant term is bounded and can never overtake the constant.
Implementing the cache would trade a small, bounded, measured
sub-dominant cost for a ~1 KiB stack regression on the constrained
ship target and real cache-invalidation risk. The discipline says a
neutral measurement reverts; nothing was implemented, so there is
nothing to revert and the benchmark plus this ADR are the deliverable.

## Consequences

**Wins.** The append path keeps its current, test-covered shape; no
`File` growth, no new invalidation surface. The benchmark stays as a
durable, runnable artifact: the O(N) claim is now checkable, not
folklore, and a future change that makes appends quadratic in an
observable way would show as a rising per-append figure.

**Costs.** The theoretical O(N^2) walk remains in the code. If a
future change lifts the 256-block cap, or a backing store makes small
reads far more expensive relative to block programming than RAM does,
this decision must be revisited; the ADR and the benchmark are the
trigger and the instrument for that revisit.

**Explicitly out of scope.** This ADR does not change
`stream_ctz_extend`, `collect_chain_blocks`, the `File` struct, or
`MAX_CTZ_WRITE_BLOCKS`. It does not claim the walk is free, only that
it is dominated and bounded on every workload the 1.x kernel can
reach. A 2.x revision that raises the cap should re-run the benchmark
first.

## Related

- `tests/bench_ctz_append.rs`: the harness and the measurement.
- `src/fs.rs`: `stream_ctz_extend`; `src/ctz.rs`:
  `collect_chain_blocks`; `src/file.rs`: `File::write`.
- ADR-0006 (stack budget), whose constraint this decision protects.
- Post v1.0 review item M3.
