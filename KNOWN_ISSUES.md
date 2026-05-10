# Known issues

Everything missing for v1.0. The list shrinks as phases land; v1.0 ships when the list is empty.

## Read path (Phase 1)

- [x] Metadata block reader: walk commits, verify CRC, surface tag stream. (`src/meta.rs`, Phase 1a)
- [x] Metadata *pair* reader: pick the active block of a pair by revision counter (wrap aware signed comparison), fall back to the alternate if the active fails. (`src/meta.rs::MetadataPair`, Phase 1b)
- [x] Superblock parser: detect the LittleFS magic, parse version, geometry, name_max, file_max, attr_max. (`src/superblock.rs`, Phase 1c)
- [ ] Storage-backed mount (Fs::mount). Reads blocks 0 and 1 via the Storage trait, runs MetadataPair::parse, then Superblock::from_pair. Glue layer; the heavy lifting is now in place.
- [ ] Directory traversal: open root, list children, resolve absolute paths.
- [ ] File read: inline structs (small files stored inside metadata) and CTZ skip list (block chained files).
- [ ] User attribute read.
- [ ] Mount level error reporting: distinguish "not a LittleFS v2 image" from "geometry mismatch" from "corrupt metadata".

## Write path (Phase 2)

- [ ] Commit construction: tag stream encoding, CCRC tail, FCRC for redundancy.
- [ ] Block allocator with the lookahead buffer.
- [ ] Compaction on full metadata pair.
- [ ] File write: inline up to a threshold, CTZ extension above it.
- [ ] `Fs::format` producing a superblock the C reference can mount.
- [ ] Sync semantics (`Fs::sync`, drop on close).
- [ ] mkdir, rmdir, rename, remove.
- [ ] User attribute write.
- [ ] Atomic move state recovery.

## Hardening (Phase 3)

- [ ] Power loss safety: a torn write at any page boundary leaves the filesystem mountable as either the pre commit or post commit state.
- [ ] Wear leveling: `block_cycles` rotation across each metadata pair.
- [ ] Fuzz harnesses on the parsers and the commit reader.
- [ ] Kani harness: revision counter comparison totality under wrap.
- [ ] Kani harness: commit accept or reject dispatch totality.

## Conformance (Phase 3, parallel)

- [ ] `tools/gen_vectors.sh`: pinned C reference, fixed scenario set, emit images + metadata sidecars.
- [ ] `tests/vectors/`: committed golden images for every scenario.
- [ ] `tests/conformance.rs`: per file expected `(passes, skips)` table, aggregate `FAIL_CEILING = 0`.
- [ ] Round trip vectors: a Rust written image mounts in C and reads what we wrote, byte for byte.

## Infrastructure

- [ ] CI workflow: matrix of host targets plus `thumbv6m-none-eabi` cross compile.
- [ ] `cargo kani --features kani` job in CI (gated on Kani availability).
- [ ] Documentation pass on the public API: every public item has a doc comment with at least one example.
- [ ] LICENSE-MIT and LICENSE-APACHE files. (The workspace package declares `MIT OR Apache-2.0` already; the text files for distribution are still missing.)

## Non issues (recorded for clarity)

- LittleFS v1 on disk format support. Out of scope per `docs/PLAN.md`.
- Async API. Out of scope; the Storage trait is synchronous. A future async wrapper crate is allowed but does not block v1.0.
