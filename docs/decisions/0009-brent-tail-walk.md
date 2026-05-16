# ADR-0009: Brent's cycle detection for reader tail walks, no length cap

- **Status**: accepted
- **Date**: 2026-05-15

## Context

The 2026-05-15 six-agent correctness review found two related defects in
how the reader follows a directory's HardTail chain.

`Fs::resolve`'s final-component loop and the internal `find_dir_pair`
chased `pair.reader.tail()` in a bare `loop {}` with no count cap and no
cycle check (review item R1): a corrupt or adversarial HardTail that
points back into its own chain made path resolution issue storage reads
forever and never return. The interim R1 fix added a fixed
`[Option<BlockPair>; 32]` visited array (`TailGuard`).

Separately, `list_pair_chain` (the directory enumeration path) walked at
most `MAX_DIR_CHAIN = 32` pairs, emitting each pair's entries inside the
loop and then returning `OutOfRange`, with no cycle detection at all
(review item R3): a cyclic chain spammed up to 32 duplicate entries
before erroring, and a directory the C reference legitimately split
across more than 32 continuation pairs was truncated then errored.

A fixed visited array forces a choice between capping the legitimate
chain length and allocating, and it leaves the resolution path (32 cap)
and the enumeration path (32 cap) on two different mechanisms. The C
reference solves the same problem with Brent's algorithm: O(1) memory,
no length ceiling, `LFS_ERR_CORRUPT` on a cycle.

## Decision

All reader tail walks use one shared Brent's cycle detector
(`BrentTailWalk`), with no arbitrary length cap; a cyclic chain is
rejected with `Error::Corrupt`.

`Fs::resolve`, `find_dir_pair`, and `list_pair_chain` each construct a
`BrentTailWalk` at the chain start and call `advance(next)` before
moving to the next pair. The detector holds three scalars (a teleporting
reference pair, a power-of-two stride, a step counter); no allocation,
`no_std`/no-alloc safe. `MAX_DIR_CHAIN` and `TailGuard` are removed.

## Consequences

**Wins.** A valid HardTail chain of any length is followed at the
resolution and enumeration layers with no arbitrary cap. A cyclic or
self-referential chain is rejected with `Error::Corrupt` (the C oracle's
classification) instead of hanging (old resolve path) or emitting
duplicate entries then `OutOfRange` (old enumeration path). One
mechanism across all three call sites removes the prior "list succeeds
but open fails" asymmetry within the mountable range. O(1) memory, no
stack-array growth, so the ADR-0006 Cortex-M0+ stack budget is
unchanged.

**Costs.** Brent's detects a cycle only when the moving pointer catches
the periodically-teleported reference, not on first revisit. A corrupt
cyclic chain is therefore processed for O(mu + lambda) steps (bounded by
the device's finite block count) before the error, and a streaming
caller of `list_pair_chain` may observe a bounded prefix, possibly with
repeats, before the `Err`. This is acceptable and is the C reference's
own behavior: entries seen before any `Error` return must be discarded,
which was already the contract.

**Explicitly out of scope.** This decision does not lift the end-to-end
reachable-pair-set limit. `Fs::mount` runs `accumulate_gstate`, a BFS
bounded by `MAX_QUEUED_PAIRS = 32` (`src/alloc.rs`), which rejects an
image whose reachable metadata-pair set exceeds 32 pairs, including a
single directory whose continuation chain exceeds 32 pairs, with
`Error::OutOfRange` before `list_pair_chain` is ever reached. That BFS
deduplicates a branching directory forest and so fundamentally needs a
visited set, not a linear-chain cycle detector; removing its cap would
require an unbounded allocation in a no-alloc kernel and would enlarge
the `[BlockPair; MAX_QUEUED_PAIRS]` stack arrays, perturbing the
ADR-0006-pinned stack budget. The 32-pair reachable-set limit is a
deliberate, documented v1.x constraint recorded in `KNOWN_ISSUES.md`;
this ADR only makes the tail-walk layer cycle-safe and cap-free so it is
correct within that limit and ready if the limit is ever revisited.

## Related

- `docs/reviews/2026-05-15-six-agent-correctness-review.md`, items R1
  (High #1) and R3 (High #3).
- ADR-0005 (wear-leveling pair relocation; `MAX_QUEUED_PAIRS` rationale).
- ADR-0006 (Cortex-M0+ scratch stack budget; why the constant is pinned).
- C reference `lfs.c` `lfs_dir_fetchmatch` (Brent-guarded tail walk).
- `src/fs.rs` `BrentTailWalk`; the R1 cycle reproducer in
  `tests/review_r1_tail_cycle.rs` and the R3 enumeration cycle test.
