---
slug: failure-fcrc-rollback
category: failure-museum
citation: "littlefs2-pure post mortem: FCRC reader rolled back a durable commit; introduced in v1.0.2 (R2 remediation), fixed in v1.1.0 (tracker id lfs-3q9 at the time)"
canonical: CHANGELOG.md, v1.1.0 Fixed section (this repository)
doi: none
archived: none (internal artifact, committed in tree)
archive_date: none
retrieved: 2026-06-11
sha256: none
license: project license
vendor_status: vendored-at-path CHANGELOG.md
rot_risk: stable-publisher
consumers:
  - tests/review_r2_fcrc.rs
  - src/meta.rs
provenance: internal
verification: "tests/review_r2_fcrc.rs pins the corrected behavior at the reader level and end to end through the public API"
---

# Post mortem: the FCRC rollback that discarded durable commits

A crash consistency near miss, first class in this museum because the defect sat exactly on the property the filesystem exists to provide.

**What happened.** The v1.0.2 review remediation (R2) added a check that, after a commit's CCRC verified, recomputed the following program window's CRC and rolled the commit back one level on mismatch. The justifying description ("C rejects the commit") was wrong about the oracle: in the C reference, `lfs_dir_fetchmatch` fixes the commit offset once a CCRC verifies and never moves it again; the FCRC governs only whether the following window counts as erased. The rollback therefore discarded durably committed metadata on the precise scenario the FCRC exists to detect (power loss inside the program that follows a CCRC valid commit), presenting stale state where the oracle reads the latest commit.

**Why it got in.** A remediation written against a paraphrase of oracle behavior instead of the oracle itself. The review finding was plausible, the fix was plausible, and the torn write sweeps of the day did not generate the intra program tear that distinguishes the two readings. Map versus territory: the paraphrase was the map.

**The fix.** The reader keeps the commit and reports the block as not erased (an `erased()` flag on `MetadataReader`); the writer's append in place path requires that flag and otherwise compacts onto a freshly erased block. The clean path stayed byte identical, so conformance and roundtrip vectors were unchanged.

**Lessons the registry keeps.** Verify review findings against the primary source before remediating; a fix to crash handling needs a reproducer that actually contains the crash; and a sweep that does not generate a tear class proves nothing about it (see `pillai-2014-crash-consistency` and the VERIFICATION-MAP).
