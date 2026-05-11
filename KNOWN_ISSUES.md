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
- [x] User attribute read. `Fs::get_attr(path, attr_id, &mut out, ...)` walks the entry's committed `UserAttr(attr_id)` tags and returns the most recent value (`0` on absent or delete-marker).
- [x] Mount level error reporting: `Fs::mount` returns distinct variants for `Io`, `GeometryMismatch`, `Unformatted` (new), `Corrupt`, `NotLittleFs`, and `UnsupportedVersion(v)`. Matrix and recommended actions documented in `INTEGRATION.md`.

## Write path (Phase 2)

- [x] Slice-based commit builder (`meta::Commit`): tag stream encoding + CCRC tail. (Phase 2a)
- [x] Commit construction with FCRC redundancy. `meta::Commit::finish_padded(chunk, prog_size, block_size)` emits an FCRC tag describing the next prog window's post-erase CRC and pads the CCRC body so the next commit starts at a prog-aligned offset. All `Fs` write paths use it. (Phase 2g.7)
- [ ] Block allocator with the lookahead buffer.
- [ ] Compaction on full metadata pair.
- [x] File write (inline, root-only) with upsert semantics: `Fs::write_inline_to_root` appends a Create+NAME+InlineStruct or just an InlineStruct (if updating). (Phase 2b)
- [x] Compaction on overflow: when the active block fills, GC live state plus the new write into a fresh commit on the alternate, bump revision. (Phase 2b.2)
- [x] NOR-aligned program wrapper: `NorAlignedStorage` caches programs to `PROG_SIZE` aligned windows. (Phase 2b.3)
- [ ] File write at arbitrary paths (not just root): requires path resolution to a directory + append. (Phase 2b.4)
- [x] Block allocator: scan-based, BFS walk from root, tracks used blocks via bitmap. (`src/alloc.rs`, Phase 2c)
- [x] File write with CTZ extension when content exceeds inline threshold: `Fs::write_to_root` / `Fs::write_ctz_to_root` allocate blocks and lay out the skip-list chain. (Phase 2d)
- [x] CTZ-on-CTZ updates: `write_to_root` and `write_to_path` rewrite the chain; the old chain becomes unreachable and is reclaimed by the next allocator scan. (Phase 2f.1)
- [x] CTZ-on-inline / inline-on-CTZ transitions: handled transparently via `Update` / `UpdateCtz` overrides. (Phase 2f.1)
- [x] `append_to_path`: atomic full-rewrite append. (Phase 2f.1)
- [x] **Streaming append for large CTZ files**: `append_to_path` now fills the existing tail block in place via NOR sub-window programs and allocates only the blocks needed for overflow. Existing chain blocks are never re-erased; write amplification is bounded by `additional.len() + one block per ~block_size of overflow`, independent of file size. (Phase 2f.2)
- [ ] **Stateful `File<'fs, S>` handle** with `open / read / write / seek / sync / set_len`. Useful for batching multiple writes into one `UpdateCtz` commit (amortizing the metadata-pair touch over a session of writes). The streaming `append_to_path` already covers the write-amplification side; this is purely about reducing metadata-commit pressure for write-heavy sessions. (Phase 2f.2 remaining)
- [x] `Fs::format` producing a superblock the C reference can mount. (Phase 2a; bit accuracy verified against `meta::MetadataReader` round-trip; C-reference cross-check pending the conformance harness.)
- [x] Sync semantics. `Fs::sync` exposes the storage layer's sync from the `Fs` handle; every public mutation already syncs as its final step. Drop on close intentionally not implemented — Drop cannot return errors, and callers needing explicit sync error handling should call `sync()` before dropping.
- [x] `remove_from_root`: delete a file by name from the root, splice-correct. (Phase 2b.4)
- [x] `list_root`: enumerate root entries, splice-correct, skipping the superblock. (Phase 2b.4)
- [x] `exists`: typed wrapper over `resolve`. (Phase 2b.4)
- [x] `mkdir`: create a directory at an arbitrary path. (`src/fs.rs::mkdir`, Phase 2e)
- [x] `write_to_path` / `remove_at_path` / `list_dir`: path-based file ops on arbitrary directories. (Phase 2e)
- [x] `rmdir`: remove an empty directory with emptiness check. (`src/fs.rs::rmdir`, Phase 2g.1)
- [x] `remove_at_path` rejects directory targets (must use `rmdir`). (Phase 2g.1)
- [x] `read_at_path` / `size_of` (offset-aware random read; works for inline + CTZ). (Phase 2g.3)
- [x] `truncate_path` (shrink or zero-extend a file via atomic rewrite). (Phase 2g.4)
- [x] `rename` within the same directory. (`src/fs.rs::rename_in_dir`, Phase 2g.2)
- [x] Cross-directory rename (`Fs::rename`). Issues a `Create` in the destination parent followed by a `Delete` in the source parent; preserves the entry's struct body so CTZ chains and child directory pairs stay in place. Rejects ancestor-cycle moves (`old` is a strict ancestor of `new`). (Phase 2g.5)
- [x] User attribute write. `Fs::set_attr(path, attr_id, value, ...)` and `Fs::remove_attr(path, attr_id, ...)`. Values capped at `0x3FE` bytes (the LittleFS length-field non-sentinel max).
- [x] Atomic move state recovery. Cross-directory rename emits balanced `MoveState` tags in both commits; `Fs::mount` BFS-walks every reachable metadata pair, XOR-accumulates every `MoveState` body, and if the result is non-zero decodes the in-flight `(src_pair, src_id)` and emits the missing source-side Delete before returning the `Fs` handle. Compaction preserves a pair's net gstate contribution. Verified by `tests/atomic_move.rs` across every program-call boundary in the rename. (v1.1)

## Hardening (Phase 3)

- [x] Power loss safety: torn-write scenarios across every program-call boundary land the FS as either the pre-state or post-state. Verified by `tests/power_loss.rs` using `TornWriteStorage` for inline-write and CTZ-streaming-append scenarios. (Phase 3)
- [ ] Wear leveling (v1.1). Within-pair wear distribution happens naturally via compaction-on-fill, which alternates active and alternate blocks of each pair. Pair-level relocation (BLOCK_CYCLES rotation to fresh blocks) requires tracking parent pair references to update DirStruct/HardTail pointers when a pair moves; non-trivial infrastructure deferred to v1.1.
- [x] Fuzz harnesses on the parsers and the commit reader. `fuzz/` (libFuzzer, nightly-only, outside the main workspace) covers `MetadataReader::new`, `Tag::from_bits`, `Path::new`, `Superblock::from_bytes`, and `CtzStruct::from_bytes`. Each target asserts totality + post-conditions; runtime is unbounded by design, so they are run ad-hoc rather than in CI. (Phase 3)
- [x] Kani harness: revision counter comparison totality under wrap. `verify::meta_proofs` proves `rev_scmp` total, reflexive, antisymmetric, and that an increment-by-one is always "newer." (Phase 3)
- [x] Kani harness: commit accept or reject dispatch totality. `verify::commit_proofs` proves `MetadataReader::new` does not panic on arbitrary inputs, rejects short blocks, and keeps `committed_end <= block.len()`. Bounded at 32-byte blocks; longer adversarial inputs are covered by the fuzz target. (Phase 3)
- [x] Kani harness: tag dispatch + CRC equivalence. `verify::tag_proofs` and `verify::crc_proofs` discharge the remaining totality / agreement obligations on the bit-level primitives. (Phase 3)

## Conformance (Phase 3, parallel)

- [x] `tools/gen_vectors/`: vendored C reference + Makefile + main.c producing baseline images. (Phase 2g.7)
- [x] `tests/vectors/`: four committed golden images (empty format, single inline, single CTZ, nested dir). Scenario set grows as new edge cases surface. (Phase 2g.7)
- [x] `tests/conformance.rs`: per-vector test, mount + assert expected `(name, kind, content)` tuples. (Phase 2g.7)
- [x] Round trip vectors: `tools/verify_image/` builds a C verifier that mounts Rust-produced images via C littlefs and validates expected content. `tests/roundtrip.rs` exercises inline, CTZ, and nested-dir scenarios. Combined with `tests/conformance.rs` (C-to-Rust), the bit-accuracy claim is now bidirectional.

## Infrastructure

- [x] CI workflow: `.github/workflows/ci.yml` runs rustfmt, clippy `-D warnings`, host test, `cargo doc`, no-default-features build, three ARM cross-compile targets, the C-to-Rust conformance suite, and the C-from-Rust round-trip suite (which builds the verifier first).
- [ ] `cargo kani --features kani` job in CI (gated on Kani availability). Harnesses are in place under `src/verify/`; CI integration is a future enhancement once Kani is reliably available on hosted runners.
- [x] Documentation pass on the public API: `cargo doc --no-deps` is warning-free; every public item has a doc comment.
- [x] LICENSE-MIT and LICENSE-APACHE text files now ship at the repo root.

## Non issues (recorded for clarity)

- LittleFS v1 on disk format support. Out of scope per `docs/PLAN.md`.
- Async API. Out of scope; the Storage trait is synchronous. A future async wrapper crate is allowed but does not block v1.0.
