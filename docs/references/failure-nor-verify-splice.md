---
slug: failure-nor-verify-splice
category: failure-museum
citation: "littlefs2-pure post mortem: the read back verification validated the alignment adapter's cache instead of the device (tracker id lfs-6ym); decided and fixed in ADR-0020"
canonical: docs/decisions/0020-nor-verify-fidelity.md (this repository)
doi: none
archived: none (internal artifact, committed in tree)
archive_date: none
retrieved: 2026-07-28
sha256: none
license: project license
vendor_status: vendored-at-path docs/decisions/0020-nor-verify-fidelity.md
rot_risk: stable-publisher
consumers:
  - tests/review_6ym_nor_verify_fidelity.rs
  - src/nor.rs
  - src/storage.rs
  - src/fs.rs
provenance: internal
verification: "tests/review_6ym_nor_verify_fidelity.rs pins the three kernel sites and the adapter contract; the H7 and V4 torn sweeps pin the crash window unchanged"
---

# Post mortem: a read back that read the cache

**What happened.** Review H2 and `lfs-ttr` gave every program site a read back: write the region, re read it, compare, and treat a mismatch as a worn block. `NorAlignedStorage`, the adapter real NOR flash needs, buffers writes into one `PROG_SIZE` window and splices that window into `read` so a caller sees its own not yet flushed bytes. The two behaviors met on the write path and cancelled. The adapter flushes a window only when the next one loads, so the last window of any programmed region was still sitting in RAM when the verify read arrived, and the verify compared the caller's own bytes against themselves. Behind the adapter, one worn program page produced a `write_to_path` that returned `Ok` over user data the device had altered, a `mkdir` that returned `Ok` and left a directory pair that read back `Corrupt` at the next mount, and an `append_to_path` that returned `Ok` where the design specifies `Io`.

**Why it got in.** The verify was written and tested against bare devices, where `read` and "what the device holds" are the same thing. The adapter was written and tested against honest devices, where the splice is invisible. Neither test diet contained the composition that mattered: a lying device *underneath* the adapter. Each layer was individually right about the world it was checked in.

**The fix.** `Storage` grows a defaulted `read_device` method meaning "tell me what the device holds"; `verify_programmed` and `verify_programmed_bytes` use it, and the adapter overrides it to flush an overlapping dirty window and read through, forwarding to the inner `read_device` so a stack of adapters bypasses all of them. This is the C reference's own shape from the other side: `lfs_bd_flush`'s validating comparison drops the read cache and passes a null program cache precisely so the comparison reaches the device (`lfs_bd_cmp`, `lfs.c`). ADR-0020 records the exposure map, the two rejected designs, and the crash window argument.

**Lessons the registry keeps.** A cache that is correct for reads is not automatically correct for verification reads; verification is the one caller whose whole purpose is to distrust the layer above the device, so it must be able to say so. Composition is where layered test diets go blind: the `StrictNorStorage` and `NorAlignedStorage` discipline the `failure-setlen-nor` post mortem installed still ran honest devices under the adapter, so it could not see this. When a countermeasure exists, test that it fires through every stack the crate ships, not only through the shortest one. And note the direction of the near miss: `sync` looked like the obvious fix and was measured to not work, because the adapter keeps the window resident and keeps splicing after flushing it. The measurement, not the intuition, chose the design.
