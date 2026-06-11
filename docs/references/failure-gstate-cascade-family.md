---
slug: failure-gstate-cascade-family
category: failure-museum
citation: "littlefs2-pure post mortem: the gstate convention and relocation-cascade family (2026-06 deep review C4, C6, C7, C8, H3, H4); shipped v1.0.0 through v1.2.0, fixed in the ADR-0016/ADR-0017 arc (tracker ids lfs-w7w, lfs-njj, lfs-bkq, lfs-gfm, lfs-les, lfs-ay4 at the time)"
canonical: docs/decisions/0016-gstate-totals-and-relocation-cascade.md and docs/decisions/0017-append-tail-fill-verified-erased.md (this repository)
doi: none
archived: none (internal artifact, committed in tree)
archive_date: none
retrieved: 2026-06-11
sha256: none
license: project license
vendor_status: vendored-at-path docs/decisions/0016-gstate-totals-and-relocation-cascade.md
rot_risk: stable-publisher
consumers:
  - tests/review_gstate.rs
  - tests/review_reloc_cascade.rs
  - tests/review_ctz_append_poison.rs
  - src/fs.rs
  - src/gstate.rs
provenance: internal
verification: "every finding pinned by a pre-fix-failing reproducer; the C6 remap additionally verified load bearing by disabling it (the grid reproduces at round 36); power-loss sweeps run at device program granularity"
---

# Post mortem: re-derived mechanisms, missing countermeasures

The v2 write arc independently re-derived the C reference's gstate and
relocation machinery and got every happy path right; what it missed
was the countermeasures, the parts of the C code that exist only for
cross-products of features (a rename DURING a relocation, an rmdir
AFTER a rename, a relocation OF a relocation, an append AFTER a torn
append). Five Criticals and Highs shipped that way (review C4, C6,
C7, H3, H4), plus the committed-tail poisoning (C8) whose sibling had
already been fixed once for shrink (`failure-setlen-nor`) without the
append-side instance being recognized.

**What happened.** Per-pair gstate was read as XOR-of-all-tags where
C's convention is each-tag-is-the-new-total, latest wins; a valid C
log decoded a phantom move and recovery deleted a live entry (C4).
Pair drops never stole the dropped pair's contribution; rmdir after a
rename out of the directory made the image unmountable (C7). The
rename captured source coordinates that a relocation cascade could
outdate (C6). An abandoned relocation deferred its unbalanced
RelocateState across a window where a second relocation killed both
addresses (H3). The deorphan sweep reclaimed half-orphans instead of
repointing the thread at the tree's twin (H4). And reproducing C6
surfaced a sixth defect none of the ~150 review agents found: the
parent walk enqueued superseded DirStruct bodies and could commit the
relocation repoint onto a STALE pre-relocation pair, erasing freed
blocks while the live parent kept the outdated reference.

**Why the gates missed it.** Every mechanism was tested in isolation;
no test composed them. The torn sweeps never combined a rename with a
relocation; the rmdir tests never rmdir'd a rename source; the gstate
tests only ever saw this writer's own delta convention, which is
internally consistent and interop-wrong. The stale-parent defect
needed block-address REUSE across relocations, which only a
long-running grid produces.

**The lesson.** When behavior is inherited from a reference, the
comments the reference marks as load-bearing ("this looks like an
optimization but is in fact _required_") are the specification of the
cross-products; a re-derivation that skips them has silently narrowed
the spec to the happy paths. And reproducers must COMPOSE features:
grids and sweeps over interleaved operations found in hours what
dimension-at-a-time review verified as sound. The structural records
are ADR-0016 (gstate totals, stealing drops, twin resolution,
live-state-only walks) and ADR-0017 (verified-erased preconditions
over assumed ones).
