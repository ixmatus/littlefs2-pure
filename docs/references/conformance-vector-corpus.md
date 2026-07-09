---
slug: conformance-vector-corpus
category: conformance
citation: "littlefs2-pure parity vector corpus, generated 2026-05 by the pinned C oracle (littlefs v2.9.3, see c-littlefs-oracle), extended 2026-07 with the review-coverage classes 08-12; 12 disk images committed at tests/vectors/"
canonical: tests/vectors/ (this repository)
doi: none
archived: none (internal artifact, committed in tree)
archive_date: none
retrieved: 2026-07-08
sha256: "01_empty_format.bin 774ac2275e681c7c815b9554769ce4c3d26087dc59ffa4dfdc6812284e8495c6, 02_single_inline.bin dca2c46c15d1fbc72c4fbb1211c9a074e61def2375d4acaa2e809eaf791d0b3e, 03_single_ctz.bin 97f098394a1c5b5c026a9d7666e7bc3647cad272a097f04d860d6769ceb7c00f, 04_nested_dir.bin b70b5a914e10add668bfa02ccefe321553031b3c0e6f9d42f8a696e61f54aeb0, 05_hardtail_dir.bin 8d44ed0637ff1c52c6bcdb739e6457e749226a99d50efabb8e8cf642ff7505e4, 06_inline_ctz_boundary.bin 78343fdf0a0cd6bbbcdc41e7ebeb5b39722358b835668587ae78999529402e0d, 07_deleted_recreated.bin 57640b4f0c1c2c0dc82fcaca908ac51ccd4ef4fbe9f63e87aaa5dc40a02497b9, 08_user_attrs.bin 6fc50f8966bde31dbe9cfd1c3aef5d34ee85297b8973476219d2d5d39396da37, 09_deep_ctz.bin 441f26a13d56c3cd795e99db481974e2cce9f1a9a4ce96f93ed1f264cdfcec04, 10_delete_tombstone.bin 7273c497047f1b737fc2b6fea490c0879dbfc1f67ef15c1509f1a8edbef2d012, 11_compacted_rename.bin 77c8fb2fcda038c1b30d779a11dab555e90c48ee4436c0fba9dac5c3d5676832, 12_multimove_gstate.bin 58b5c1a223b112accba2631bc5e1e157fd0729cb3693a23248751a2b9bf91d1c"
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

The evidence behind the bidirectional bit accuracy claim: images the pinned C oracle wrote must mount and read correctly under this crate (conformance direction), and images this crate writes must mount and read correctly under the oracle (roundtrip direction). Twelve golden images are committed at `tests/vectors/`, 2048 bytes each, all generated at one geometry: `read_size = 16`, `prog_size = 16`, `block_size = 256`, `block_count = 8`, `cache_size = 64`, `lookahead_size = 8` (matching the test suite's `MemStorage`).

| Image | Scenario |
|---|---|
| `01_empty_format.bin` | Freshly formatted empty filesystem |
| `02_single_inline.bin` | One inline file (`/cfg`) |
| `03_single_ctz.bin` | One CTZ file (`/payload.bin`, 500 bytes) |
| `04_nested_dir.bin` | Nested directory with a file (`/audit/log`) |
| `05_hardtail_dir.bin` | Directory split across HARDTAIL continuation pairs |
| `06_inline_ctz_boundary.bin` | File at the inline versus CTZ boundary |
| `07_deleted_recreated.bin` | File deleted and recreated under the same name |
| `08_user_attrs.bin` | Three entries with user attributes, the middle removed (splice; review C1/C2) |
| `09_deep_ctz.bin` | A 900 byte, four block CTZ chain needing multi word skip pointers |
| `10_delete_tombstone.bin` | A bare delete tombstone beside a live neighbor (review C3) |
| `11_compacted_rename.bin` | An oracle compacted directory with non id dense NAME order (review H1) |
| `12_multimove_gstate.bin` | Two moves into one directory: two MOVESTATE tags in one log (review C4) |

Generation method: `tools/gen_vectors/main.c` compiled against the vendored oracle (`make vectors` regenerates the set). The oracle pin and its two way verification are recorded in `c-littlefs-oracle`. `tests/conformance.rs` additionally pins each image's CRC32 so an accidental regeneration cannot silently change what the assertions exercise.

## Coverage gaps, as of 2026-07-08

The corpus proves what it exercises and nothing more. The deep review of v1.2.0 (docs/reviews, 2026-06-10) named these gaps; the tracker IDs in parentheses are this repository's issue handles at the time of writing and will go stale faster than this prose.

- CLOSED (lfs-761, 2026-07-08). The image classes that hid the top findings are now C written vectors 08-12: an oracle compacted directory with non id dense NAME order (H1), a bare delete tombstone read across a fresh mount (C3), a two step move gstate log (C4), user attributes across a splice (C1/C2), and a four block CTZ chain. Each pins the read direction of its fix in `tests/conformance.rs`.
- CLOSED (lfs-jdk, 2026-07-08). The roundtrip gate is now read-write: the `mutate` scenario in `tools/verify_image` has the oracle write an inline file and a CTZ file into a Rust formatted image, which Rust then remounts and verifies, exercising the FCRC / erased-window handshake in the C-writes-into-Rust direction (M11). Deeper mixed writer histories (interleaved Rust and C commits) remain unexercised (lfs-4b8).
- The torn write sweeps overclaim in places: some assertions accept a bricked filesystem, some sweeps skip silently, and tears at cache boundaries inside a program window are not modeled the way NOR hardware would land them (lfs-hki).
- Every image uses the single geometry above; no second geometry is exercised end to end (lfs-4s3).

These gaps are exactly what the README disclosure's named failure mode points at: crash sequences the simulation did not generate, and format drift the byte level tests did not catch. When the gaps close, this entry must be updated in the same slice.
