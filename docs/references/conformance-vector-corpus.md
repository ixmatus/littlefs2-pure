---
slug: conformance-vector-corpus
category: conformance
citation: "littlefs2-pure parity vector corpus, generated 2026-05 by the pinned C oracle (littlefs v2.9.3, see c-littlefs-oracle); 7 disk images committed at tests/vectors/"
canonical: tests/vectors/ (this repository)
doi: none
archived: none (internal artifact, committed in tree)
archive_date: none
retrieved: 2026-06-11
sha256: "01_empty_format.bin 774ac2275e681c7c815b9554769ce4c3d26087dc59ffa4dfdc6812284e8495c6, 02_single_inline.bin dca2c46c15d1fbc72c4fbb1211c9a074e61def2375d4acaa2e809eaf791d0b3e, 03_single_ctz.bin 97f098394a1c5b5c026a9d7666e7bc3647cad272a097f04d860d6769ceb7c00f, 04_nested_dir.bin b70b5a914e10add668bfa02ccefe321553031b3c0e6f9d42f8a696e61f54aeb0, 05_hardtail_dir.bin 8d44ed0637ff1c52c6bcdb739e6457e749226a99d50efabb8e8cf642ff7505e4, 06_inline_ctz_boundary.bin 78343fdf0a0cd6bbbcdc41e7ebeb5b39722358b835668587ae78999529402e0d, 07_deleted_recreated.bin 57640b4f0c1c2c0dc82fcaca908ac51ccd4ef4fbe9f63e87aaa5dc40a02497b9"
license: generated artifact of BSD-3-Clause littlefs; same terms
vendor_status: vendored-at-path tests/vectors
rot_risk: stable-publisher
consumers:
  - tests/conformance.rs
  - tests/roundtrip.rs
  - tools/gen_vectors
  - tools/verify_image
  - KNOWN_ISSUES.md
provenance: internal
verification: "tests/conformance.rs mounts and reads every image; tests/roundtrip.rs hands images this crate writes to the C verifier"
---

# Parity vector corpus

The evidence behind the bidirectional bit accuracy claim: images the pinned C oracle wrote must mount and read correctly under this crate (conformance direction), and images this crate writes must mount and read correctly under the oracle (roundtrip direction). Seven golden images are committed at `tests/vectors/`, 2048 bytes each, all generated at one geometry: `read_size = 16`, `prog_size = 16`, `block_size = 256`, `block_count = 8`, `cache_size = 64`, `lookahead_size = 8` (matching the test suite's `MemStorage`).

| Image | Scenario |
|---|---|
| `01_empty_format.bin` | Freshly formatted empty filesystem |
| `02_single_inline.bin` | One inline file (`/cfg`) |
| `03_single_ctz.bin` | One CTZ file (`/payload.bin`, 500 bytes) |
| `04_nested_dir.bin` | Nested directory with a file (`/audit/log`) |
| `05_hardtail_dir.bin` | Directory split across HARDTAIL continuation pairs |
| `06_inline_ctz_boundary.bin` | File at the inline versus CTZ boundary |
| `07_deleted_recreated.bin` | File deleted and recreated under the same name |

Generation method: `tools/gen_vectors/main.c` compiled against the vendored oracle (`make vectors` regenerates the set). The oracle pin and its two way verification are recorded in `c-littlefs-oracle`.

## Coverage gaps, as of 2026-06-11

The corpus proves what it exercises and nothing more. The deep review of v1.2.0 (docs/reviews, 2026-06-10) named these gaps; the tracker IDs in parentheses are this repository's issue handles at the time of writing and will go stale faster than this prose.

- No C written vector exercises a directory the oracle itself compacted (multi entry, post compaction tag order), a delete observed across a fresh mount, multi step move gstate, user attributes, or a CTZ chain deep enough to need multi word skip pointers (lfs-761).
- The roundtrip gate is read only: the oracle mounts and reads our images, but never writes into an image this crate then reads back. Mixed writer histories are unexercised (lfs-jdk, lfs-4b8).
- The torn write sweeps overclaim in places: some assertions accept a bricked filesystem, some sweeps skip silently, and tears at cache boundaries inside a program window are not modeled the way NOR hardware would land them (lfs-e2a, lfs-xzt, lfs-hki).
- Every image uses the single geometry above; no second geometry is exercised end to end (lfs-4s3).

These gaps are exactly what the README disclosure's named failure mode points at: crash sequences the simulation did not generate, and format drift the byte level tests did not catch. When the gaps close, this entry must be updated in the same slice.
