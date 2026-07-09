# Reference registry index

One line per entry; content lives in the entries, never here. Schema and conventions: [README.md](README.md). Companions: [GLOSSARY.md](GLOSSARY.md), [VERIFICATION-MAP.md](VERIFICATION-MAP.md).

## Spec and conformance

- [spec-littlefs-v2](spec-littlefs-v2.md) — the on disk format specification at pinned revision d01280e (v2.9.3); the format version story (writes 2.1, reads 2.0 and 2.1)
- [design-littlefs](design-littlefs.md) — Haster's design rationale at the same pin; per structure map with ADR divergences; documentation exemplar
- [conformance-vector-corpus](conformance-vector-corpus.md) — the twelve golden images (08-12 added for the 2026-06 review classes), generation method, oracle pin, and coverage gaps as of 2026-07-08

## Oracle

- [c-littlefs-oracle](c-littlefs-oracle.md) — C littlefs v2.9.3 as behavioral oracle, never code template; the pin verified two ways

## Algorithms and registries

- [crc32-iso-hdlc](crc32-iso-hdlc.md) — the littlefs CRC variant anchored to the CRC RevEng catalogue check value
- [brent-1980-cycle-detection](brent-1980-cycle-detection.md) — Brent's cycle detection behind the ADR-0009 tail walk

## History and frame

- [rosenblum-ousterhout-1992](rosenblum-ousterhout-1992.md) — the log structured filesystem paper; the deepest ancestor
- [jffs2-woodhouse-2001](jffs2-woodhouse-2001.md) — JFFS2, the first step of the flash filesystem lineage
- [yaffs-manning](yaffs-manning.md) — How YAFFS Works; second step, NAND native
- [spiffs](spiffs.md) — SPIFFS, the small NOR predecessor littlefs displaced
- [pillai-2014-crash-consistency](pillai-2014-crash-consistency.md) — crash states as a coverage problem; the frame for the simulation's limits

## Failure museum

Post mortems are written at fix time, in the same slice as the fix; crash consistency near misses are first class entries.

- [failure-fcrc-rollback](failure-fcrc-rollback.md) — the FCRC reader rollback that discarded durable commits (v1.0.2 to v1.1.0)
- [failure-setlen-nor](failure-setlen-nor.md) — shrink then extend ANDed bytes into stale NOR cells (fixed v1.1.0)
- [failure-splice-id-family](failure-splice-id-family.md) — one raw-id versus live-id confusion, five copies, four Criticals (2026-06 review C1/C2/C5/H1; fixed in the ADR-0015 arc)
- [failure-gstate-cascade-family](failure-gstate-cascade-family.md) — re-derived mechanisms, missing countermeasures: the cross-product defects (2026-06 review C4/C6/C7/C8/H3/H4; fixed in the ADR-0016/0017 arc)

## Named gaps

- The closed set registries (tag types in `src/tag.rs`, error kinds in `src/error.rs`, format constants in `src/lib.rs`) are hand maintained against the pinned SPEC.md; generators that emit them from a single table are preferred and deferred (tracker id lfs-432 at the time of writing).
- Registry hashes and pointers are checked structurally by `tests/registry.rs` only; CI verification that recorded sha256 values still match and archived URLs still resolve is deferred (lfs-lrs).
- No flash vendor app notes are cited yet; when the allocator or wear discussion first leans on one, it enters this registry in the same slice.
- Philosophy entries (conviviality, permacomputing, maintenance culture) are absent by design; they are copied from the downstream master registry, not authored here.
