# ADR-0012: SoftTail global directory-list threading (design)

- **Status**: proposed (design only; implementation is `lfs-xmx`, not yet landed)
- **Date**: 2026-05-29

## Context

The LittleFS v2 format threads every metadata pair into a filesystem-wide
linked list via tail pointers. A pair's tail tag carries a split bit:
`SoftTail` (split = 0) points to the next *directory* in the global list;
`HardTail` (split = 1) points to *this* directory's own continuation pair
(see ADR for directory splitting, `lfs-cvh`). The list is rooted at the
superblock pair `{0, 1}` and walked by following `dir.tail` until null.

This crate's writer emits **no tail tag at all** (no `WriteOp` does). It
finds reachable pairs by a parent to child `DirStruct` BFS, which is
self-consistent for crate-only use. But the C reference's pair
enumeration walks the tail thread, not the tree:

- `lfs_mount_` (lfs.c:4406) iterates `dir.tail` from `{0, 1}`.
- `lfs_fs_traverse_` (lfs.c:4617) iterates `dir.tail` and, per pair,
  marks the pair blocks and each entry's CTZ blocks. **There is no
  `DirStruct`-tree recursion.**
- `lfs_alloc_scan` (lfs.c:642) builds the free-block lookahead from
  `lfs_fs_traverse_`.

**Confirmed consequence (review item `lfs-l3f`).** A crate-created image
with subdirectories has no tail thread, so the C reference's allocator
and traverse never visit the subdirectory pairs or the file blocks
inside them. The C reference can still *read* such an image (path
resolution descends `DirStruct`, which is why the round-trip read suite
passes), but if it *writes* into the image it will allocate blocks it
believes are free and corrupt the subdirectories. So the missing thread
is a read-write **interop correctness defect**, not merely a missing
feature. The crate's own operation is unaffected (it uses the tree, not
the thread).

## Decision

The crate's writer will thread every directory pair into the global list,
matching the C reference's structure so crate-written images are safe for
any conformant littlefs to read *and* write.

### Threading on create (crash-safe for this crate)

`lfs_mkdir_` (lfs.c:2595) inserts the new directory immediately after the
parent in the list:

1. The new dir's initial commit carries `SoftTail -> parent's current
   tail` (null if the parent had none, making the new dir the list end).
2. The parent's create commit adds `SoftTail -> new dir pair` alongside
   the `Name` + `DirStruct`.

Because this crate's directories never split (one pair each; see
`lfs-cvh`), the parent update is a single atomic commit, so mkdir
threading has **no crash window** here: a crash before the parent commit
leaves the new dir an unreferenced orphan reclaimed by the allocator
scan, exactly as today.

### Compaction must preserve the tail

`lfs_dir_compact` re-emits `LFS_TYPE_TAIL + split` on every compaction
(lfs.c:2003). The crate's `compact_and_program` rebuilds a pair from its
live entry slots and currently drops everything else; it must read the
pair's current tail before compacting and re-emit it, or compaction
would silently break the thread. (Append-in-place commits need no change:
the latest tail tag persists as the newest until a new one supersedes
it.)

### Un-threading on remove requires the orphan subsystem

This is the load-bearing complication. `lfs_remove_` (lfs.c:3849) deletes
the entry from the parent, then finds the thread **predecessor**
(`lfs_fs_pred`) and re-links it (`lfs_dir_drop`: predecessor tail ->
removed dir's tail). Because mkdir inserts each new dir right after the
parent, a directory's predecessor is frequently a **sibling, not the
parent**, so the entry-delete and the predecessor re-link land on **two
different pairs** and cannot be one atomic commit.

The crash window between them leaves the filesystem inconsistent in
either ordering: delete-then-relink leaves a pair in the thread with no
parent entry; relink-then-delete leaves a pair in the tree but out of the
thread (whose now-thread-free blocks the allocator could reuse). The C
reference survives this only with its **orphan subsystem**:
`lfs_fs_preporphans(+1/-1)` brackets the desync with an orphan-count in
global state, and mount-time `lfs_fs_forceconsistency` /
`lfs_fs_deorphan` walks the thread, detects pairs in the thread with no
parent `DirStruct` (or stale references), and completes the drop.

Therefore correct threading **requires adding an orphan-count gstate and
a mount-time deorphan sweep**, structurally analogous to the crate's
existing `MoveState` (atomic-move recovery, ADR-0009 era) and
`RelocateState` (wear-level orphan recovery, ADR-0005) mechanisms: a
balanced gstate contribution that a mount-time sweep reconciles.

### Rename

Cross-directory rename moves an entry between parents but does not move
the directory's pair, and the global list order is independent of the
tree hierarchy, so the directory's list membership is unchanged. Rename
needs no thread maintenance (to be confirmed by an interop test).

## Consequences

**Wins.** Crate-written images become read-write interoperable with the C
reference: `lfs_fs_traverse`, `lfs_fs_gc`, `lfs_fs_size`, and the C
allocator all see every directory. Closes the `lfs-l3f` defect. Couples
naturally with directory splitting (`lfs-cvh`): both emit the same tail
tag, distinguished by the split bit, in the same `compact_and_program`
path.

**Costs.** A new orphan-count gstate plus a mount-time deorphan sweep is a
subsystem, not a localized change; it is the bulk of the work and the
reason this is its own slice rather than folded into the `mkdir`/`compact`
edits. The on-disk bytes the writer produces change (tail tags appear),
so the round-trip and conformance gates must be re-run, and a new C
`verify_image` `traverse` scenario should be added as the regression pin
that would have caught the original defect.

**Implementation order (proposed).** (1) tail-tag `WriteOp` + emission
and `compact_and_program` preservation; (2) `mkdir` threading (atomic,
no orphan window); (3) the orphan-count gstate + mount-time deorphan; (4)
`rmdir` un-threading using (3); (5) the C `verify_image` traverse
regression test; (6) confirm rename needs no change. Each step
reproduce-first and conformance/round-trip gated.

**Revision after attempting step 2 (2026-05-29).** Step 1 (tail emission
+ compaction preservation) landed as a verified no-op. Attempting step 2
(`mkdir` threading) in isolation surfaced a coupling the original order
missed: **wear-levelling relocation also breaks the thread.** When a
threaded directory pair relocates, its address changes; the relocation
updates the parent's `DirStruct` (via `propagate_relocation` /
`UpdateDirStruct`) but **not** the thread predecessor's tail, so that
predecessor's `SoftTail` is left pointing at the stale old pair. The
existing `tests/wear_leveling.rs::relocation_xor_aggregate_zeros_on_success`
caught this (the stale link makes the gstate walk double-count a
`RelocateState` body). The crate's own allocator over-approximates and is
not corrupted, but C traverse would follow the stale link, defeating the
threading. Relocation happens during ordinary writes, so this is not
deferrable to the `rmdir` slice. Step 2 was reverted; step 1 remains.

**Progress (2026-05-29, continued).** The steady-state feature landed and
is green across the full suite (power-loss + relocation crash tests
included), conformance, and round-trip, in three further increments:
mkdir threading; `find_thread_predecessor` + relocation thread-update (the
predecessor-tail update folds atomically into the parent's DirStruct
commit when the predecessor is the parent, the common case); and `rmdir`
un-threading with a tail-clear capability (a NONE-sentinel `new_tail`
forces a compaction so the rebuilt block drops the sticky tail tag).
Pinned by `tests/pending_softtail.rs`
(`directories_are_threaded_into_the_global_list`,
`mkdir_rmdir_churn_reclaims_blocks`).

What remains is the **mount-time deorphan sweep** for the crash windows
(`rmdir`'s two-pair un-thread; the sibling-predecessor relocation case). A
crash there leaves a pair in the thread but not the tree. Analysis: this
is a recoverable **space leak, not corruption** — both the crate's and the
C reference's allocators over-approximate by following the thread, so the
orphan's blocks are marked used (never reused) until reclaimed. Deorphan
reclaims them by walking the thread and dropping any threaded pair absent
from the **live, splice-correct** tree set (latest-wins `DirStruct`
reachability, modelled on `accumulate_gstate`, *not* raw `iter_tags`: a
crashed `rmdir` has already deleted the parent's `DirStruct`, so an
over-approximating tree walk would miss the orphan). The sweep is
idempotent and re-runnable, so it needs no orphan-count gstate gate for
correctness (only for the efficiency of skipping it on healthy mounts).
A torn-`rmdir` power-loss test pins it.

Consequence: the **thread-predecessor maintenance machinery**
(`lfs_fs_pred`-style walk to find the pair whose tail references a given
pair, plus a thread-update commit, plus the crash-window recovery for the
separate-pair update) is a **prerequisite shared by relocation and
`rmdir`**, not a later step. The revised order is: (1) tail infra
[done]; (2) the predecessor walk + the orphan/relocation crash-window
recovery (the orphan-count gstate + mount-time deorphan, generalized to
cover a stale thread link from a relocated *or* removed pair); (3)
relocation thread-update wired into `propagate_relocation`; (4) `mkdir`
threading; (5) `rmdir` un-threading; (6) the C traverse regression test;
(7) confirm rename. Threading cannot be enabled (step 4) until the
predecessor maintenance (steps 2 and 3) exists, because every write can
trigger a relocation.

## Related

- `tests/pending_softtail.rs`: the crate-side reproduce-first target.
- Review items `lfs-xmx` (this work) and `lfs-l3f` (the confirmed
  interop finding).
- C reference: `lfs_mkdir_`, `lfs_remove_`, `lfs_fs_pred`,
  `lfs_dir_drop`, `lfs_fs_preporphans`, `lfs_fs_forceconsistency`,
  `lfs_fs_traverse_`, `lfs_mount_` in `tools/gen_vectors/littlefs/lfs.c`.
- ADR-0005 (`RelocateState` orphan recovery) and the `MoveState`
  atomic-move recovery: the existing gstate-sweep patterns the orphan
  subsystem mirrors.
- The directory-splitting design (`lfs-cvh`), which shares the tail tag.
