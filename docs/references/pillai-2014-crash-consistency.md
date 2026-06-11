---
slug: pillai-2014-crash-consistency
category: history, conformance
citation: "Thanumalayan Sankaranarayana Pillai, Vijay Chidambaram, Ramnatthan Alagappan, Samer Al-Kiswany, Andrea C. Arpaci-Dusseau, and Remzi H. Arpaci-Dusseau, All File Systems Are Not Created Equal: On the Complexity of Crafting Crash-Consistent Applications, 11th USENIX Symposium on Operating Systems Design and Implementation (OSDI 14), 2014"
canonical: https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai
doi: none
archived: https://web.archive.org/web/20260506212252/https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai
archive_date: 2026-05-06
retrieved: 2026-06-11
sha256: none
license: USENIX open access
vendor_status: pointer-only
rot_risk: stable-publisher
consumers:
  - docs/references/VERIFICATION-MAP.md
  - docs/references/conformance-vector-corpus.md
provenance: primary
verification: none (methodological frame; no code derives from it)
---

# Crash consistency as a coverage problem

The intellectual frame for being honest about what this crate's power loss simulation proves. Pillai and coauthors systematically enumerated the crash states real filesystems can expose and showed that applications believed crash safe fail under states their authors never generated in testing: the space of post crash images is much larger than the space any test harness explores, and the gap is where the bugs live.

That is precisely the posture this registry takes toward the torn write sweeps in `tests/power_loss.rs` and friends: a sweep enumerates a model of interruption points, and its verdict is only as strong as the model. The VERIFICATION-MAP states which tear classes the sweeps generate and which they cannot; the README disclosure names "crash sequences the simulation did not generate" as the project's primary residual risk, and this paper is the citation for why that residual risk is the one to name. Title and author list verified against the USENIX page on 2026-06-11.

Archive note: the Wayback save endpoint was rate limited on the citation date (2026-06-11); the recorded snapshot is the closest existing capture, five weeks prior.
