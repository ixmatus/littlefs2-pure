---
slug: closed-set-registries
category: registries
citation: "Metadata types registry, littlefs technical specification (SPEC.md) section 'Metadata types', revision d01280e (tag v2.9.3, 2024), read together with enum lfs_type and enum lfs_error in lfs.h at the same revision"
canonical: https://github.com/littlefs-project/littlefs/blob/d01280e64934a09ba16cac60cf9d3a37e228bb66/SPEC.md#metadata-types
doi: none
archived: https://web.archive.org/web/20260611082240/https://raw.githubusercontent.com/littlefs-project/littlefs/d01280e64934a09ba16cac60cf9d3a37e228bb66/SPEC.md
archive_date: 2026-06-11
retrieved: 2026-07-27
sha256: none (composes two entries that carry their own hashes; see the body)
license: BSD-3-Clause
vendor_status: pointer-only
rot_risk: stable-publisher
consumers:
  - src/tag.rs
  - src/error.rs
  - src/lib.rs
  - src/dir.rs
  - tests/closed_sets.rs
provenance: secondary
verification: "tests/closed_sets.rs (per bucket census and bidirectional oracle coverage), tests/property_tag.rs, src/verify/tag_proofs.rs"
---

# The closed sets this crate mirrors

Four sets in this crate are closed: they have a definite membership fixed by
the format rather than by this implementation's convenience, and adding to
them is a decision about the format rather than a decision about the code.

| Set | Where it lives | Authority |
|---|---|---|
| Tag types (the 11 bit type field) | `TagType` in `src/tag.rs` | SPEC.md metadata types table; `enum lfs_type` in `lfs.h` |
| Abstract types (the high 3 bits) | `AbstractType` in `src/tag.rs` | The same, at the prefix layer |
| Entry kinds | `EntryKind` in `src/dir.rs` | `LFS_TYPE_REG` and `LFS_TYPE_DIR` |
| Format constants | `DISK_VERSION`, `MAGIC`, `NAME_MAX`, `ROOT_BLOCK_PAIR` in `src/lib.rs` | `LFS_DISK_VERSION`, `LFS_NAME_MAX`, and the superblock layout |

The error set in `src/error.rs` is deliberately absent from that table, for
the reason given below.

## Why this entry has no hash of its own

This entry composes two sources that are each registered and vendored in
their own right: `spec-littlefs-v2` holds the specification at the pin, with
its hash and its vendored copy under `vendor/spec-littlefs-v2/`, and
`c-littlefs-oracle` holds the C reference at the same pin, with per file
hashes over `tools/gen_vectors/littlefs/`. Recording a third copy of those
digests here would create two places to update and one place to forget. The
integrity of what this entry cites is verified through those entries by
`tools/check_references.sh`.

## What checks it

`tests/closed_sets.rs` parses `enum lfs_type`, `enum lfs_error`, and the
`#define` limits straight out of the vendored header at test time, rather
than restating their values in Rust. Bumping the oracle pin therefore runs
every comparison again instead of leaving a stale transcription behind, which
is the failure this arrangement is built to prevent.

Coverage runs in both directions, and both directions are load bearing:

- Every constant the oracle declares must appear in a classification table in
  that suite. A future oracle revision that widens the type space or adds an
  error code fails the suite until someone decides what this crate does about
  it.
- The tag decoder is enumerated over its entire 2048 value domain and the
  resulting census is pinned bucket by bucket, never as a total. A floor on
  the sum would admit a silent compensating regression, where one variant
  widens while another narrows and nothing fails.

A second scan reads variant identifiers out of the Rust sources themselves.
That catches the half the census cannot see: a variant declared but never
wired into the decoder, unreachable from disk yet present in the public API.
The scan exists because these enums are `#[non_exhaustive]`, which is a
deliberate semver commitment and is itself pinned, but which also means an
integration test cannot ask the compiler to check completeness for it.

## What is not mechanically checked, and why

**The error set is not a mirror.** `Error` in `src/error.rs` has no numeric
relationship to `enum lfs_error`. The C reference returns negative errno
values through an `int` channel, a C idiom this crate has no reason to
inherit, and the two sets do not align one to one in either direction. This
crate is finer grained where a caller can act on the difference:
`Unformatted`, `Corrupt`, and `NotLittleFs` are three distinct answers to
"this device did not mount", all of which are `LFS_ERR_CORRUPT` upstream, and
the distinction is what lets a firmware boot path format a fresh chip without
also reformatting a damaged one. It is coarser where a caller cannot:
`OutOfRange` absorbs both `LFS_ERR_NOSPC` and `LFS_ERR_FBIG`. Asserting a
numeric equivalence would assert something false. What the suite pins instead
is that a mapping decision exists and is recorded for every upstream code,
with a stated reason for each widening and each condition judged unreachable.

**Two oracle limits have no constant here.** `LFS_FILE_MAX` and
`LFS_ATTR_MAX` are superblock fields rather than compile time constants:
`Fs::format` writes zero into `name_max`, `file_max`, and `attr_max`, which
is the encoding for "the driver's default" and is what the C reference writes
too. There is nothing on this side to compare. The suite asserts only that
the oracle still declares them, so the decision resurfaces if that changes.

**One tag type is this crate's own.** `TagType::RelocateState` occupies
`0x7fe`, an unused slot in the Globals abstract type, and carries the
mount time recovery state for a half completed metadata pair relocation
(ADR-0005, ADR-0014). It has no counterpart in the oracle and is excluded
from the oracle table by name rather than by omission. C littlefs does not
recognize the slot, so interoperation with a torn `0x7fe` aggregate is
asserted by neither the specification nor a conformance vector. If a future
upstream revision claims `0x7fe`, the forward coverage check fails, which is
the intended alarm.
