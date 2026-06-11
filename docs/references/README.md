# Reference registry

This directory is the citation registry for `littlefs2-pure`: one markdown file per external source the crate's design, code, tests, or documentation relies on. The crate implements an external on disk format, so the registry is part of the deliverable, not an afterthought. The directory is self contained by design: a downstream project may copy it wholesale into its own tree, and every entry must remain legible without this repository's CLAUDE.md, issue tracker, or git history.

## The accretion ritual

When a slice of work cites or relies on an external source, its registry entry is appended or updated in the same slice, never deferred to a future documentation pass. Every load bearing URL is saved to the Wayback Machine at citation time and the archived URL is recorded. A vendor document is treated as rotting from the day it is cited.

The upstream littlefs `SPEC.md` and `DESIGN.md` are moving git documents: every citation pins the exact upstream commit hash, and the cited revision is archived and vendored. The rot mode there is silent content drift, not disappearance.

## Entry schema

Each entry is a markdown file named `<slug>.md` with YAML frontmatter followed by a short prose body (why this source, what it grounds, alternatives considered). The frontmatter keys:

| Key | Meaning |
|---|---|
| `slug` | Matches the filename stem. |
| `category` | One or more of the categories below, comma separated. |
| `citation` | Author, title, venue, year, edition or revision. |
| `canonical` | Canonical URL or document number. |
| `doi` | DOI when one exists, else `none`. |
| `archived` | Wayback Machine URL saved at citation time, else `none` with a reason. |
| `archive_date` | Date of the Wayback snapshot. |
| `retrieved` | Date the source was last retrieved and verified. |
| `sha256` | Hash of retrieved binaries or vendored files, else `none`. |
| `license` | License of the source material, recorded before any vendoring. |
| `vendor_status` | `vendored-at-path <path>`, `pointer-only`, `legally-cannot`, or `paper-copy-owned`. |
| `rot_risk` | `died-once`, `single-maintainer`, `community-run`, `academic-personal`, `stable-publisher`, or `ephemeral`. |
| `consumers` | Repository paths (code, tests, ADRs, docs) that lean on this source. |
| `provenance` | `primary` or `secondary`; for internal artifacts, `internal`. |
| `verification` | Tests, vectors, or proofs derived from or anchored to this source. |

Vendored copies live under `vendor/<slug>/`. A vendored copy always travels with its license file, and its sha256 is recorded in the entry frontmatter.

## Categories

| Category | Scope |
|---|---|
| `spec` | The on disk format specification and design documents. |
| `oracle` | The C reference implementation used as a behavioral oracle, never as a code template. |
| `conformance` | Parity vector sets and the evidence behind the bit accuracy claim, including coverage gaps. |
| `algorithms` | Published algorithm sources behind specific code paths. |
| `registries` | Closed enumerable sets (tag types, error codes, format constants) and their authoritative sources. |
| `history` | The lineage and intellectual frame: flash filesystems, log structured filesystems, crash consistency literature. |
| `failure-museum` | Post mortems of shipped defects, written at fix time; crash consistency near misses are first class. |

Shared philosophy entries (conviviality, permacomputing, maintenance culture) are deliberately absent: the master registry for those lives in the downstream project that copies this directory, and they are copied in that direction, not authored here.

## Companion documents

`INDEX.md` holds one line per entry and never carries content. `GLOSSARY.md` defines the LittleFS v2 vocabulary the entries assume. `VERIFICATION-MAP.md` maps each correctness claim to the artifact that checks it, including an honest statement of what each artifact cannot see.

`tests/registry.rs` in the host repository checks the structural invariants (schema keys present, INDEX coverage, consumer paths exist); it does not verify hashes or live URLs.
