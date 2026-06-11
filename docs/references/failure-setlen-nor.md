---
slug: failure-setlen-nor
category: failure-museum
citation: "littlefs2-pure post mortem: set_len shrink then extend corrupted data on NOR flash; fixed in v1.1.0 (tracker id lfs-6o9 at the time)"
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
  - tests/review_shrink_append.rs
  - tests/common
  - src/file.rs
provenance: internal
verification: "tests/review_shrink_append.rs pins the fix under NorAlignedStorage<StrictNorStorage>"
---

# Post mortem: shrink then extend ANDed bytes into stale NOR cells

**What happened.** `File::set_len` shrinking a file could leave the new tail block partially full and keep using it. The next extending write filled the tail region in place at an offset still holding stale content. NOR flash programs cells one to zero only, so the appended bytes were ANDed with the leftovers (`0xAA & 0x55 == 0x00`): silent data corruption, no error returned.

**Why it got in.** The bug is invisible on a permissive RAM backed test storage, which happily overwrites. Every functional test passed; the defect lived in the gap between the storage model the tests used and the physics the trait contract promises. The checker's blind spot was the storage model itself.

**The fix.** A partial tail is relocated copy on write to a freshly erased block before becoming the new head, so the later in place fill lands on `0xFF` cells. The old chain and committed metadata stay untouched until sync, preserving power loss atomicity; the orphaned block is reclaimed by the next allocator scan.

**Lessons the registry keeps.** A storage abstraction permissive beyond the hardware contract launders bugs; strict NOR semantics (`StrictNorStorage`, `NorAlignedStorage`) belong in the default test diet, not just in dedicated suites. Weight a green suite by what its storage model could not have seen. The VERIFICATION-MAP records which suites run under which storage models for exactly this reason.
