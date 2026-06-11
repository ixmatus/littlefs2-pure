---
slug: brent-1980-cycle-detection
category: algorithms
citation: "Richard P. Brent, An improved Monte Carlo factorization algorithm, BIT Numerical Mathematics 20(2), pages 176 to 184, June 1980"
canonical: https://doi.org/10.1007/BF01933190
doi: 10.1007/BF01933190
archived: https://web.archive.org/web/20231102171046/https://link.springer.com/article/10.1007/BF01933190
archive_date: 2023-11-02
retrieved: 2026-06-11
sha256: none
license: Springer (paywalled article; cited by DOI, not vendored)
vendor_status: pointer-only
rot_risk: stable-publisher
consumers:
  - docs/decisions/0009-brent-tail-walk.md
  - src/dir.rs
provenance: primary
verification: "tests/review_r1_tail_cycle.rs pins cycle termination on adversarial tail loops"
---

# Brent's cycle detection (1980)

The cycle detection algorithm this crate uses to defend directory tail walks against corrupted or adversarial HARDTAIL chains that loop. Brent published it inside a Pollard rho factorization improvement; section 3 of the paper gives the cycle finding method itself (a power of two teleporting tortoise), which finds a cycle in at most twice the cycle's period plus its tail using two pointers and no memory. Citation metadata verified against Crossref on 2026-06-11.

Why this source: ADR-0009 chose Brent's walk over the C reference's bounded walk so that arbitrarily long valid chains terminate without a hard cap; the C reference is the behavioral oracle for what a valid chain means, but the cycle defense is an algorithmic choice made fresh, so it gets a primary literature citation rather than an oracle pointer. Floyd's tortoise and hare was the alternative; Brent's variant does the same job with fewer pointer advances per step.
