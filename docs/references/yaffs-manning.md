---
slug: yaffs-manning
category: history
citation: "Charles Manning, How YAFFS Works, yaffs.net, retrieved revision 2026-06-11 (PDF, 37 pages)"
canonical: https://yaffs.net/sites/default/files/downloads/HowYaffsWorks.pdf
doi: none
archived: https://web.archive.org/web/20260611080450/https://yaffs.net/sites/default/files/downloads/HowYaffsWorks.pdf
archive_date: 2026-06-11
retrieved: 2026-06-11
sha256: 5add1a099e7fae90e5edcaaec77d6ee128682cb9a66ab34fb336e7e7b511fd2d (HowYaffsWorks.pdf)
license: document copyright the author (cited, not vendored)
vendor_status: pointer-only
rot_risk: died-once
consumers:
  - docs/references/design-littlefs.md
provenance: primary
verification: none (historical context, no code derives from it)
---

# How YAFFS Works

YAFFS is the second step in the flash filesystem lineage this registry traces: designed for NAND from the start (where JFFS2 was retrofitted from NOR), log structured per object rather than per device, but still paying a whole device scan at mount unless checkpointing is enabled. Manning's document is the canonical self description.

Rot note: the document path on yaffs.net has already moved at least once; the path recorded in earlier literature (`/documents/how-yaffs-works`) returned 404 at retrieval time and the working path above was found by crawling the site. That is the died once profile: treat the Wayback snapshot as the durable pointer and expect the canonical URL to break again.
