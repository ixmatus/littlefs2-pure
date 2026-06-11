---
slug: failure-splice-id-family
category: failure-museum
citation: "littlefs2-pure post mortem: the raw-id versus live-id confusion family (2026-06 deep review C1, C2, C5, H1); shipped v1.0.0 through v1.2.0, fixed in the ADR-0015 arc (tracker ids lfs-2dg, lfs-3z8, lfs-fb2, lfs-r88 at the time)"
canonical: docs/reviews/2026-06-10-deep-adversarial-review.md and docs/decisions/0015-shared-splice-core-and-attr-replay.md (this repository)
doi: none
archived: none (internal artifact, committed in tree)
archive_date: none
retrieved: 2026-06-11
sha256: none
license: project license
vendor_status: vendored-at-path docs/decisions/0015-shared-splice-core-and-attr-replay.md
rot_risk: stable-publisher
consumers:
  - tests/review_splice_attrs.rs
  - src/dir.rs
  - src/fs.rs
  - src/alloc.rs
  - src/meta.rs
provenance: internal
verification: "tests/review_splice_attrs.rs pins all four findings with pre-fix-failing reproducers; the conformance corpus gap (no C-compacted multi-entry vector) that let H1 ship is recorded in conformance-vector-corpus and closes under lfs-761"
---

# Post mortem: one id confusion, five copies, four Criticals

A correctness family, first class in this museum because it shows how a
sound mechanism re-derived independently at several sites fails at each
site differently, and how a test corpus shaped by the writer's own
output cannot see any of it.

**What happened.** LittleFS renumbers directory entries through splice
tags: a committed tag carries the id its entry had at write time, and
every later Create or Delete at or below that id shifts the entry's
live id. Five code sites needed that mapping; each had its own copy of
the logic, and four were wrong in distinct ways. Compaction rebuilt
entries from a slot table that never recorded attribute tags, so every
compaction silently destroyed every user attribute, and a `set_attr`
arriving on a full block compacted, persisted nothing, and returned
`Ok` (C1). `get_attr` compared raw ids against live ids directly:
attributes vanished after a lower-id delete and leaked across entries
when a new entry reused the raw id (C2). The relocation parent walk
returned a raw id that its consumer used as a live id, repointing a
sibling's struct body into a directory address and destroying the
sibling (C5, reproduced: file content replaced by a pair address). All
four forward walkers also assumed NAME tags arrive id-dense in log
order, which is true of this crate's own output and false of C
compaction after a rename, so valid C images failed mount (H1).

**Why the gates missed it.** The attribute surface was tested only on
the append fast path; no test compacted, renumbered, or remounted
around an attribute. The conformance corpus contained no C-compacted
multi-entry directory, so the id-density assumption was only ever
exercised against images this crate wrote itself: the writer's
conventions had quietly become the reader's grammar. The relocation
grid existed but never combined a relocation with a prior delete in
the parent.

**The lesson.** When a semantic invariant (here: how an id maps
through splices) lives in more than one implementation, each copy is
an independent chance to be wrong, and self-written test images
verify the copies against each other rather than against the format.
The fix is structural, not local: one forward splice core
(`dir::splice_step`), one backward splice-diff query
(`dir::attr_get` over `meta::TagIterRev`), one compaction emission
stream feeding both the writer and the size estimate, each derived
once from the oracle (`lfs_dir_fetchmatch`, `lfs_dir_getslice`,
`lfs_dir_compact`). ADR-0015 records the design; the full
`LiveId`/`RawId` newtype separation remains open as the review's D1.
