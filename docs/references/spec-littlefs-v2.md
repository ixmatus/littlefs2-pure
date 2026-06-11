---
slug: spec-littlefs-v2
category: spec
citation: "Christopher Haster et al., littlefs technical specification (SPEC.md), littlefs-project/littlefs, revision d01280e (tag v2.9.3, 2024)"
canonical: https://github.com/littlefs-project/littlefs/blob/d01280e64934a09ba16cac60cf9d3a37e228bb66/SPEC.md
doi: none
archived: https://web.archive.org/web/20260611082240/https://raw.githubusercontent.com/littlefs-project/littlefs/d01280e64934a09ba16cac60cf9d3a37e228bb66/SPEC.md
archive_date: 2026-06-11
retrieved: 2026-06-11
sha256: 6dd74dfc58ac93589f8e002bb73c0830a59206912cfe8c951c07eb2bd5fccad0 (SPEC.md at the pinned revision)
license: BSD-3-Clause
vendor_status: vendored-at-path docs/references/vendor/spec-littlefs-v2/SPEC.md
rot_risk: stable-publisher
consumers:
  - src/tag.rs
  - src/meta.rs
  - src/superblock.rs
  - src/dir.rs
  - src/ctz.rs
  - src/lib.rs
  - docs/decisions/0002-spec-as-oracle.md
  - docs/decisions/0008-format-bootstrap-divergence.md
provenance: primary
verification: "tests/conformance.rs (C written vectors), tests/roundtrip.rs (C reads our images), tests/property_tag.rs, src/verify/tag_proofs.rs"
---

# littlefs technical specification (SPEC.md)

The normative description of the LittleFS v2 on disk format: metadata pairs and their commit log, the 32 bit XOR encoded tag (valid bit, type, id, length), the metadata type registry (`0x0xx` NAME through `0x5ff` FCRC), inline and CTZ file structures, SOFTTAIL and HARDTAIL threading, gstate and MOVESTATE, and the CCRC commit boundary rules. ADR-0002 makes this document the oracle for everything it pins down; the C reference fills in only what the spec leaves open.

## Why the pin

SPEC.md is a moving document inside the upstream git repository; its content drifts silently as the format gains minor revisions. Every citation in this repository therefore refers to the exact revision above, which is the revision the vendored C oracle (see `c-littlefs-oracle`) was released with. The pinned revision is vendored beside this entry and archived; a future reader can diff a later upstream SPEC.md against the vendored copy to see precisely what changed since this crate's citations were written.

## The format version story

The on disk version is a 32 bit value, major in the high half, minor in the low half.

- This crate writes disk version 2.1: `DISK_VERSION = 0x0002_0001` (`src/lib.rs`).
- The reader accepts major exactly 2 and minor at most 1, so both lfs2.0 and lfs2.1 images mount; newer minors are rejected with `Error::UnsupportedVersion` (`src/superblock.rs`).
- Upstream introduced disk version 2.1 in release v2.6.0. Verified against the pinned trees, not recalled: `lfs.h` at tag v2.5.1 defines `LFS_DISK_VERSION 0x00020000` and at v2.6.0 defines `0x00020001` (retrieved 2026-06-11). The 2.1 minor carries the FCRC erase state checksum (`0x5ff` LFS_TYPE_FCRC in the pinned SPEC.md) used to decide whether a commit's following program window is genuinely erased.

## Known divergence

`Fs::format` initializes only block A of the root pair at revision 1 and omits the tail area tag, where the C reference also pre writes block B at revision 2. Both are valid and interoperate; they are not byte identical. The divergence is deliberate and recorded in ADR-0008. Structure encoding fidelity is otherwise non negotiable: every tag, CRC, and revision counter this crate writes must match what the C reference would produce for the same structure.
