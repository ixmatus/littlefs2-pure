---
slug: c-littlefs-oracle
category: oracle
citation: "Christopher Haster et al., littlefs (C reference implementation), littlefs-project/littlefs, tag v2.9.3, commit d01280e64934a09ba16cac60cf9d3a37e228bb66 (2024); library version 2.9, disk version 2.1"
canonical: https://github.com/littlefs-project/littlefs/tree/d01280e64934a09ba16cac60cf9d3a37e228bb66
doi: none
archived: https://web.archive.org/web/20260214173935/https://github.com/littlefs-project/littlefs/releases/tag/v2.9.3
archive_date: 2026-02-14
retrieved: 2026-06-11
sha256: "lfs.c 4a221443c1936be4d41d8f400cd2fd055695196acadef35fcb227eb7e90f6db5, lfs.h 37cefbc6983f19ada481bda913199d917cbb8c82d53a43dca369d62f669c64e6, lfs_util.c f2fbde533670560434bd9f5a547174cc7c5a4670a02c47b4bd85180dced8b2ec, lfs_util.h 03e912a6e9894c9d10c61f5da22b89ebe0bb778af67972d7b67a5f160731bf72"
license: BSD-3-Clause (preserved at tools/gen_vectors/littlefs/LICENSE.md)
vendor_status: vendored-at-path tools/gen_vectors/littlefs
rot_risk: stable-publisher
consumers:
  - tools/gen_vectors
  - tools/verify_image
  - docs/decisions/0002-spec-as-oracle.md
  - docs/decisions/0004-c-reference-as-golden.md
  - docs/decisions/0019-storage-geometry-gate.md
  - src/geometry.rs
  - src/meta.rs
  - src/ctz.rs
  - src/dir.rs
  - src/superblock.rs
  - src/alloc.rs
  - src/fs.rs
  - src/block.rs
provenance: primary
verification: "tests/conformance.rs (vectors the oracle wrote), tests/roundtrip.rs (the oracle reads our images via tools/verify_image)"
---

# C littlefs as behavioral oracle

The upstream C implementation is this crate's behavioral oracle for everything the specification does not pin down to bit level, and never a code template. The doctrine is recorded in ADR-0002 and ADR-0004: cross check outputs, not internals. The Rust implementation takes its shape from idiomatic Rust; only behavior is inherited. Inline comments throughout `src/` cite `lfs.c` line numbers (for example `lfs_dir_fetchmatch` at `lfs.c:1095`, `lfs_ctz_find` at `lfs.c:2856`); those line numbers resolve against exactly the pinned revision above, which is what keeps them stable.

## The pin, verified two ways

The vendored copy at `tools/gen_vectors/littlefs/` arrived via the `littlefs2-sys` crate, version 0.3.2. Two independent checks (run 2026-06-11) agree on the upstream identity:

1. The `littlefs2-sys` repository (trussed-dev/littlefs2-sys, formerly nickray/littlefs2-sys) pins its `littlefs` git submodule at commit `d01280e64934a09ba16cac60cf9d3a37e228bb66` at its `0.3.2` tag.
2. All four vendored files (`lfs.c`, `lfs.h`, `lfs_util.c`, `lfs_util.h`) are byte identical (sha256 above) to the same files at upstream tag `v2.9.3`, which resolves to that same commit. No patches were carried.

`lfs.h` at the pin defines `LFS_VERSION 0x00020009` (library 2.9) and `LFS_DISK_VERSION 0x00020001` (disk format 2.1).

## What the oracle grounds

Every parity vector in `tests/vectors/` was written by this oracle revision (see `conformance-vector-corpus`), and the roundtrip gate compiles this same source into `tools/verify_image` so the oracle can mount and read images this crate writes. A clean oracle result speaks only to what the oracle exercises; the corpus entry records the coverage gaps.

The oracle is also what settles questions the specification leaves open about which encodings a conforming reader must accept. The null tail sentinel is the worked example (`lfs-yl6`, 2026-07-28): `SPEC.md` describes the tail tag without saying whether thread end is spelled as an absent tag or as an all ones body, and the oracle writes *both*, choosing by which commit path it takes. `lfs_dir_drop` (`lfs.c:1831`) commits the tail body unguarded, so dropping the last directory in the thread writes the sentinel literally, while `lfs_dir_compact` (`lfs.c:2003`) guards on `!lfs_pair_isnull` and omits the tag. The oracle reads the two identically because every walk over `dir->tail` is gated on `lfs_pair_isnull` (`lfs.c:292`) before the pair is fetched. Reading the oracle's *writer* alone would have missed half the accepted input language; the rule lives in its reader.
