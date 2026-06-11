---
slug: crc32-iso-hdlc
category: registries, algorithms
citation: "Greg Cook, Catalogue of parametrised CRC algorithms (CRC RevEng), entry CRC-32/ISO-HDLC; corroborated by Philip Koopman, CRC polynomial evaluation pages, Carnegie Mellon University"
canonical: https://reveng.sourceforge.io/crc-catalogue/all.htm
doi: none
archived: https://web.archive.org/web/20260611082803/https://reveng.sourceforge.io/crc-catalogue/all.htm
archive_date: 2026-06-11
retrieved: 2026-06-11
sha256: none
license: catalogue text copyright Greg Cook (cited, not vendored)
vendor_status: pointer-only
rot_risk: single-maintainer
consumers:
  - src/crc.rs
  - tests/property_crc.rs
  - src/verify/crc_proofs.rs
provenance: primary
verification: "external_crc32_check_value unit test in src/crc.rs pins the published check value; tests/property_crc.rs checks the nibble table against a bit by bit reference; src/verify/crc_proofs.rs proves bounded slice agreement under Kani"
---

# CRC-32/ISO-HDLC (the littlefs CRC variant)

LittleFS uses the standard reflected CRC-32: polynomial `0xEDB88320`, the same as IEEE 802.3, PNG, and zlib. The CRC RevEng catalogue is the authoritative published registry of CRC parametrisations; its CRC-32/ISO-HDLC entry supplies the external anchor `CRC32("123456789") = 0xCBF43926`. Koopman's CRC pages at Carnegie Mellon corroborate the polynomial's identity and provide the error detection analysis; they are folded into this entry as a corroborating source rather than given their own file.

The littlefs twist worth recording: the format stores the raw running register with no final XOR, so the relation to the catalogue check value is a final complement, `!update(INIT, b"123456789") == 0xCBF43926`. The `external_crc32_check_value` test pins exactly that relation, so a shared misconception of the variant (which a self consistency test would happily pass) cannot survive unnoticed.

Rot note: both sources are personal pages of single maintainers (a SourceForge site and an academic page), the classic rot profile. Archived at citation time; Koopman's page resolves through the Wayback snapshot of 2026-05-05 (`https://web.archive.org/web/20260505184632/https://users.ece.cmu.edu/~koopman/crc/`, the save endpoint deduplicated to the recent capture). The check value itself is also pinned in code and in the IEEE 802.3 lineage, so loss of the pages would not orphan the claim.
