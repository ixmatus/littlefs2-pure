---
slug: jffs2-woodhouse-2001
category: history
citation: "David Woodhouse, JFFS: The Journalling Flash File System, Ottawa Linux Symposium, 2001"
canonical: https://sourceware.org/jffs2/jffs2.pdf
doi: none
archived: https://web.archive.org/web/20260611083247/https://sourceware.org/jffs2/jffs2.pdf
archive_date: 2026-06-11
retrieved: 2026-06-11
sha256: b57206ba3e3390b373520082f891237cd614a82511dee1bb40871786c98fcc7e (jffs2.pdf)
license: paper copyright the author (cited, not vendored)
vendor_status: pointer-only
rot_risk: community-run
consumers:
  - docs/references/design-littlefs.md
provenance: primary
verification: none (historical context, no code derives from it)
---

# JFFS2: the journalling flash filesystem

The first widely deployed purpose built flash filesystem for Linux, and the opening move in the lineage that leads to littlefs: JFFS2 (2001) treats the whole device as a circular log with garbage collection, which buys power loss robustness at the cost of full device scans at mount and RAM proportional to the file count. littlefs's DESIGN.md positions itself explicitly against this trade off (bounded RAM, constant mount cost), so the paper is the reference point for understanding what littlefs is a reaction to.

Lineage as the registry tells it: JFFS2 (log structured, scan heavy) to YAFFS (NAND native, still scan at mount) to SPIFFS (tiny NOR targets, no directories) to littlefs (bounded everything, directories, power loss safety by construction). See `yaffs-manning` and `spiffs`; the log structuring idea itself goes back to `rosenblum-ousterhout-1992`.
