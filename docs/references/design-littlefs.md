---
slug: design-littlefs
category: spec, algorithms
citation: "Christopher Haster, The design of littlefs (DESIGN.md), littlefs-project/littlefs, revision d01280e (tag v2.9.3, 2024)"
canonical: https://github.com/littlefs-project/littlefs/blob/d01280e64934a09ba16cac60cf9d3a37e228bb66/DESIGN.md
doi: none
archived: https://web.archive.org/web/20260611082458/https://raw.githubusercontent.com/littlefs-project/littlefs/d01280e64934a09ba16cac60cf9d3a37e228bb66/DESIGN.md
archive_date: 2026-06-11
retrieved: 2026-06-11
sha256: 2a8b3459d74b9fa8bdd947738522c46331739405b1fc3d70f057f636a1dea768 (DESIGN.md at the pinned revision)
license: BSD-3-Clause
vendor_status: vendored-at-path docs/references/vendor/design-littlefs/DESIGN.md
rot_risk: stable-publisher
consumers:
  - src/meta.rs
  - src/ctz.rs
  - src/alloc.rs
  - src/dir.rs
  - src/fs.rs
  - docs/decisions/0005-wear-leveling-pair-relocation.md
  - docs/decisions/0007-ctz-append-no-chain-cache.md
  - docs/decisions/0009-brent-tail-walk.md
  - docs/decisions/0010-allocator-lookahead-cache.md
  - docs/decisions/0011-ctz-append-log-time-seek.md
  - docs/decisions/0012-softtail-global-list-threading.md
  - docs/decisions/0013-directory-splitting.md
  - docs/decisions/0014-failure-driven-pair-relocation.md
provenance: primary
verification: "tests/property_meta.rs, tests/property_ctz.rs, tests/power_loss.rs, tests/wear_leveling.rs, tests/atomic_move.rs"
---

# The design of littlefs (DESIGN.md)

The rationale document behind the format: why metadata pairs, why CTZ skip lists, how the allocator and wear leveling work, and how the move problem is solved with global state. It explains the constraints (power loss resilience, wear awareness, bounded RAM and ROM) that make each structure the shape it is. The same pin discipline applies as for `spec-littlefs-v2`: this entry cites the revision shipped with the vendored C oracle, vendored beside this entry.

DESIGN.md is also, deliberately, a documentation exemplar for this project's own manuals: a single document that walks from problem statement through rejected alternatives to the shipped design, with failure modes treated as first class content.

## Per structure map

Each row names the DESIGN.md section at the pinned revision, the implementing modules, and where this crate's behavior deliberately diverges from the C reference's shape (behavior against the format never diverges; ADRs record shape divergences and the reasoning).

| Structure | DESIGN.md section | Implementation | Divergences and notes |
|---|---|---|---|
| Metadata pairs and commit log | "Metadata pairs" | `src/meta.rs`, `src/tag.rs` | Reader and writer derived from the spec; commit accept or reject totality proven in `src/verify/commit_proofs.rs`. Format bootstrap divergence recorded in ADR-0008. |
| CTZ skip lists | "CTZ skip-lists" | `src/ctz.rs`, `src/file.rs` | No per file chain cache (ADR-0007); block resolution from the head in logarithmic reads via `ctz::seek_block` (ADR-0011). |
| Block allocator and lookahead | "The block allocator" | `src/alloc.rs` | Lookahead cache kept as an over approximation of in use blocks on the `Fs`, refreshed from an authoritative reachability scan (ADR-0010). |
| Wear leveling and relocation | "Wear leveling" | `src/meta.rs`, `src/fs.rs` | Proactive pair relocation per ADR-0005; failure driven relocation on program or erase errors per ADR-0014. |
| Directories and tail threading | "Directories" | `src/dir.rs`, `src/fs.rs` | SOFTTAIL threading of the global directory list (ADR-0012); directory splitting across HARDTAIL continuation pairs (ADR-0013); tail walk cycle defense uses Brent's algorithm instead of the C reference's bounded walk (ADR-0009, see `brent-1980-cycle-detection`). |
| The move problem and gstate | "The move problem" | `src/meta.rs`, `src/fs.rs` | Atomic rename via gstate and MOVESTATE tags; exercised by `tests/atomic_move.rs`. |
