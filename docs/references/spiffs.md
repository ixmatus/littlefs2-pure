---
slug: spiffs
category: history
citation: "Peter Andersson (pellepl), SPIFFS: SPI Flash File System, github.com/pellepl/spiffs, retrieved 2026-06-11"
canonical: https://github.com/pellepl/spiffs
doi: none
archived: https://web.archive.org/web/20260522020756/https://github.com/pellepl/spiffs
archive_date: 2026-05-22
retrieved: 2026-06-11
sha256: none
license: MIT
vendor_status: pointer-only
rot_risk: single-maintainer
consumers:
  - docs/references/design-littlefs.md
provenance: primary
verification: none (historical context, no code derives from it)
---

# SPIFFS

The third step in the lineage: a filesystem for small NOR flash on microcontrollers, the niche littlefs now occupies. SPIFFS proved the demand (it shipped in early ESP8266/ESP32 SDKs) while exhibiting the limits littlefs was designed past: no directories, no power loss guarantees by construction, and a maintenance profile typical of a single maintainer hobby project. Espressif's own SDKs later replaced it with littlefs, which is the cleanest historical evidence of the succession.

The repository wiki carries the design documentation; the repo has been quiet for years, so the single maintainer rot class applies to both code and wiki.

Archive note: the Wayback save endpoint was rate limited on the citation date (2026-06-11); the recorded snapshot is the closest existing capture, three weeks prior.
