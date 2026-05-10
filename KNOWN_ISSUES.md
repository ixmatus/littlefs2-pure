# Known issues

Everything missing for v1.0. The list shrinks as phases land; v1.0 ships when the list is empty.

## Read path (Phase 1)

- [x] Metadata block reader: walk commits, verify CRC, surface tag stream. (`src/meta.rs`, Phase 1a)
- [x] Metadata *pair* reader: pick the active block of a pair by revision counter (wrap aware signed comparison), fall back to the alternate if the active fails. (`src/meta.rs::MetadataPair`, Phase 1b)
- [x] Superblock parser: detect the LittleFS magic, parse version, geometry, name_max, file_max, attr_max. (`src/superblock.rs`, Phase 1c)
- [x] Storage-backed mount (`Fs::mount`). Reads blocks 0 and 1 via the `Storage` trait, runs `MetadataPair::parse`, then `Superblock::from_pair`. (`src/fs.rs`, Phase 1d)
- [x] Single-pair directory entry iteration. (`src/dir.rs`, Phase 1e)
- [x] Single-pair name lookup with STRUCT pairing. (`src/dir.rs::lookup`, Phase 1f sliver)
- [x] Splice handling: Delete tags renumber entries with higher ids; `dir::live_entries` applies the rules during walk. (`src/dir.rs`, Phase 1i.1)
- [x] HardTail chasing: directories split across multiple metadata pairs. `Fs::resolve` chases HardTails at each path component. (`src/fs.rs`, Phase 1i.2)
- [x] Full path resolution: walk from root by name, descending into subdirectories. (`src/fs.rs::resolve`, Phase 1h)
- [x] File read for inline structs: the InlineStruct body *is* the file content; `dir::lookup` returns it directly. (Phase 1f sliver)
- [x] CTZ struct codec and geometry math: 8 byte body decode/encode, skip pointer count per block, content bytes per block, file offset -> (block, abs_offset) translation. (`src/ctz.rs`, Phase 1g foundations)
- [x] CTZ storage-backed read: walk the chain backward from head, fetch each block's content portion, reassemble. (`src/ctz.rs::read_ctz`, Phase 1g full)
- [ ] User attribute read.
- [ ] Mount level error reporting: distinguish "not a LittleFS v2 image" from "geometry mismatch" from "corrupt metadata".

## Write path (Phase 2)

- [x] Slice-based commit builder (`meta::Commit`): tag stream encoding + CCRC tail. (Phase 2a)
- [ ] Commit construction with FCRC redundancy. The current builder emits CCRC only; FCRC for next-prog erase detection is not yet integrated. (Phase 2 follow-up)
- [ ] Block allocator with the lookahead buffer.
- [ ] Compaction on full metadata pair.
- [x] File write (inline, root-only) with upsert semantics: `Fs::write_inline_to_root` appends a Create+NAME+InlineStruct or just an InlineStruct (if updating). (Phase 2b)
- [x] Compaction on overflow: when the active block fills, GC live state plus the new write into a fresh commit on the alternate, bump revision. (Phase 2b.2)
- [x] NOR-aligned program wrapper: `NorAlignedStorage` caches programs to `PROG_SIZE` aligned windows. (Phase 2b.3)
- [ ] File write at arbitrary paths (not just root): requires path resolution to a directory + append. (Phase 2b.4)
- [ ] File write with CTZ extension when content exceeds inline threshold: requires block allocator. (Phase 2d)
- [x] `Fs::format` producing a superblock the C reference can mount. (Phase 2a; bit accuracy verified against `meta::MetadataReader` round-trip; C-reference cross-check pending the conformance harness.)
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
