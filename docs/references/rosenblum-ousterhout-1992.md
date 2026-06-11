---
slug: rosenblum-ousterhout-1992
category: history
citation: "Mendel Rosenblum and John K. Ousterhout, The design and implementation of a log-structured file system, ACM Transactions on Computer Systems 10(1), pages 26 to 52, February 1992"
canonical: https://doi.org/10.1145/146941.146943
doi: 10.1145/146941.146943
archived: none (paywalled publisher page behind a stable DOI; nothing to usefully archive)
archive_date: none
retrieved: 2026-06-11
sha256: none
license: ACM (paywalled article; cited by DOI, not vendored)
vendor_status: pointer-only
rot_risk: stable-publisher
consumers:
  - docs/references/design-littlefs.md
  - docs/references/jffs2-woodhouse-2001.md
provenance: primary
verification: none (intellectual ancestry, no code derives from it)
---

# The log structured filesystem (Sprite LFS, 1992)

The deepest ancestor in this registry's lineage: the paper that established writing all filesystem state as an append only log and reclaiming space by segment cleaning. Every flash filesystem in the chain that follows (JFFS2, YAFFS, SPIFFS, littlefs) is downstream of this idea, because raw flash physics (erase before write, wear) force log structure onto any honest design; Rosenblum and Ousterhout arrived at the same shape a decade early from disk seek economics instead.

littlefs is a deliberate partial heir: metadata pairs are two block logs with in place compaction rather than a device wide log with a cleaner, trading the cleaner's throughput for bounded RAM and bounded mount time. DESIGN.md's "Existing designs?" section makes this contrast explicit. Citation metadata verified against Crossref on 2026-06-11.
