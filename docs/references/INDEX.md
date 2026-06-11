# Reference registry index

One line per entry; content lives in the entries, never here. Schema and conventions: [README.md](README.md). Companions: [GLOSSARY.md](GLOSSARY.md), [VERIFICATION-MAP.md](VERIFICATION-MAP.md).

## Spec and conformance

- [spec-littlefs-v2](spec-littlefs-v2.md) — the on disk format specification at pinned revision d01280e (v2.9.3); the format version story (writes 2.1, reads 2.0 and 2.1)
- [design-littlefs](design-littlefs.md) — Haster's design rationale at the same pin; per structure map with ADR divergences; documentation exemplar

## Oracle


## Algorithms and registries


## History and frame


## Failure museum

Post mortems are written at fix time, in the same slice as the fix; crash consistency near misses are first class entries.


## Named gaps

- The closed set registries (tag types in `src/tag.rs`, error kinds in `src/error.rs`, format constants in `src/lib.rs`) are hand maintained against the pinned SPEC.md; generators that emit them from a single table are preferred and deferred (tracker id lfs-432 at the time of writing).
- Registry hashes and pointers are checked structurally by `tests/registry.rs` only; CI verification that recorded sha256 values still match and archived URLs still resolve is deferred (lfs-lrs).
- No flash vendor app notes are cited yet; when the allocator or wear discussion first leans on one, it enters this registry in the same slice.
- Philosophy entries (conviviality, permacomputing, maintenance culture) are absent by design; they are copied from the downstream master registry, not authored here.
