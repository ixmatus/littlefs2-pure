# Known issues

What's missing for v1.0. v1.0 ships when every entry below is either checked or moved to "explicitly out of scope."

The v1.0 / v1.1 / v1.2 punch list against the LittleFS v2 spec is complete. What remains is ergonomic polish and CI plumbing; see "Outstanding before v1.0" at the bottom.

## Read path

- [x] Metadata block reader: walk commits, verify CRC, surface tag stream. (`src/meta.rs`)
- [x] Metadata *pair* reader: pick the active block of a pair by revision counter (wrap-aware signed comparison), fall back to the alternate if the active fails. (`src/meta.rs::MetadataPair`)
- [x] Superblock parser: detect the LittleFS magic, parse version, geometry, name_max, file_max, attr_max. (`src/superblock.rs`)
- [x] Storage-backed mount (`Fs::mount`). Reads blocks `0` and `1` via the `Storage` trait, runs `MetadataPair::parse`, then `Superblock::from_pair`. Atomic-move-state recovery completes any in-flight cross-directory rename before returning. (`src/fs.rs`)
- [x] Splice handling: Delete tags renumber entries with higher ids. (`src/dir.rs`)
- [x] HardTail chasing: directories split across multiple metadata pairs. `Fs::resolve`, `Fs::list_dir`, and `Fs::list_root` follow HardTails. (`src/fs.rs`)
- [x] Full path resolution: walk from root by name, descending into subdirectories. (`src/fs.rs::resolve`)
- [x] File read for inline structs: the InlineStruct body *is* the file content; `dir::lookup` returns it directly.
- [x] CTZ struct codec and geometry math: 8-byte body decode/encode, skip-pointer count per block, content bytes per block, file offset → (block, abs_offset) translation. (`src/ctz.rs`)
- [x] CTZ storage-backed read: walk the chain backward from head, fetch each block's content portion, reassemble. (`src/ctz.rs::read_ctz` / `read_ctz_at`)
- [x] User attribute read: `Fs::get_attr(path, attr_id, &mut out, ...)` returns the most recent committed value (`0` on absent or delete-marker).
- [x] Mount error reporting: `Fs::mount` returns distinct variants for `Io`, `GeometryMismatch`, `Unformatted`, `Corrupt`, `NotLittleFs`, and `UnsupportedVersion(v)`. Matrix and recommended actions in `INTEGRATION.md`.

## Write path

- [x] Slice-based commit builder (`meta::Commit`): tag stream encoding + CCRC tail.
- [x] Commit construction with FCRC redundancy. `meta::Commit::finish_padded` emits an FCRC tag describing the next prog window's post-erase CRC and pads the CCRC body so the next commit starts at a prog-aligned offset.
- [x] Block allocator: scan-based BFS walk from root, tracks used blocks via bitmap. (`src/alloc.rs`)
- [x] Compaction on overflow: when the active block fills, GC live state plus the new write into a fresh commit on the alternate, bump revision.
- [x] NOR-aligned program wrapper: `NorAlignedStorage` caches programs to `PROG_SIZE`-aligned windows. (`src/nor.rs`)
- [x] File write inline (root and arbitrary path) with upsert semantics.
- [x] File write CTZ (root and arbitrary path), including CTZ-on-CTZ updates, CTZ-on-inline / inline-on-CTZ transitions handled transparently.
- [x] `append_to_path`: streaming append for CTZ files. Fills the existing tail block in place via NOR sub-window programs and allocates only the blocks needed for overflow. Write amplification per append is bounded by `additional.len() + one block per ~block_size of overflow`.
- [x] `Fs::format` producing a superblock the C reference can mount, verified bidirectionally via `tests/conformance.rs` and `tests/roundtrip.rs`.
- [x] Sync semantics. `Fs::sync` exposes the storage layer's sync; every public mutation already syncs as its final step.
- [x] `remove_from_root`, `remove_at_path`: delete a file by name, splice-correct.
- [x] `list_root`, `list_dir`: enumerate entries; splice-correct; chase HardTails through up to 32 continuation pairs.
- [x] `exists`: typed wrapper over `resolve`.
- [x] `mkdir`, `rmdir` at arbitrary paths.
- [x] `read_at_path`, `size_of`: offset-aware random read (inline + CTZ).
- [x] `truncate_path`: shrink or zero-extend a file via atomic rewrite.
- [x] `rename_in_dir`: same-parent rename.
- [x] `rename`: cross-directory rename. Preserves the entry's struct body so CTZ chains and child directory pairs stay in place; rejects ancestor-cycle moves.
- [x] User attribute write: `Fs::set_attr` and `Fs::remove_attr`. Values capped at `0x3FE` bytes (LittleFS length-field non-sentinel max).
- [x] Atomic move state recovery (v1.1). Cross-directory rename emits balanced `MoveState` tags in both commits; `Fs::mount` BFS-walks every reachable metadata pair, XOR-accumulates every `MoveState` body, and if the result is non-zero decodes the in-flight `(src_pair, src_id)` and emits the missing source-side Delete + balancing MoveState. Compaction preserves a pair's net gstate contribution. Verified by `tests/atomic_move.rs` across every program-call boundary.
- [x] Inter-pair wear levelling via pair relocation (v1.2). Compact-time predicate `(rev + 1) % ((BLOCK_CYCLES + 1) | 1) == 0` redirects the compact to a freshly allocated block; the parent's `DirStruct` flips via a new `WriteOp::UpdateDirStruct`. Atomic at the alternate-program boundary; `BLOCK_CYCLES <= 0` disables. Verified by `tests/wear_leveling.rs`. Design in `docs/decisions/0005-wear-leveling-pair-relocation.md`.
- [x] Mount-time orphan recovery for half-completed wear-levelling relocations (v1.2). A balanced `RelocateState` tag rides every relocation commit; a non-zero XOR-aggregate at mount time decodes the in-flight `(old_pair, new_pair)` and emits a balancing commit on `old_pair` that cancels the cycle. The forfeited fresh block is reclaimed by the next allocator scan. Verified by `relocation_atomic_across_every_power_loss` in `tests/wear_leveling.rs`.

## Hardening

- [x] Power-loss safety: torn-write scenarios across every program-call boundary land the FS as either the pre-state or post-state. Verified by `tests/power_loss.rs` using `TornWriteStorage` for inline-write and CTZ-streaming-append scenarios.
- [x] Fuzz harnesses on the parsers and the commit reader. `fuzz/` (libFuzzer, nightly-only, outside the main workspace) covers `MetadataReader::new`, `Tag::from_bits`, `Path::new`, `Superblock::from_bytes`, and `CtzStruct::from_bytes`.
- [x] Kani harness: revision counter comparison totality under wrap.
- [x] Kani harness: commit accept-or-reject dispatch totality.
- [x] Kani harness: tag dispatch + CRC equivalence.

## Conformance

- [x] `tools/gen_vectors/`: vendored C reference + Makefile + `main.c` producing baseline images.
- [x] `tests/vectors/`: four committed golden images (empty format, single inline, single CTZ, nested dir).
- [x] `tests/conformance.rs`: per-vector test, mount + assert expected `(name, kind, content)` tuples.
- [x] `tests/roundtrip.rs` + `tools/verify_image/`: a C verifier that mounts images Rust wrote and validates expected content. Combined with `tests/conformance.rs`, the bit-accuracy claim is bidirectional.

## Infrastructure

- [x] CI workflow: `.github/workflows/ci.yml` runs rustfmt, clippy `-D warnings`, host test, `cargo doc` (warning-free), `--no-default-features` build, three ARM cross-compile targets, the C-to-Rust conformance suite, and the C-from-Rust round-trip suite.
- [x] Documentation pass on the public API: `cargo doc --no-deps` is warning-free under `RUSTDOCFLAGS=-D warnings`; every public item has a doc comment.
- [x] LICENSE-MIT and LICENSE-APACHE text files ship at the repo root.
- [x] `README.md` with quick-start, status, and pointers into `INTEGRATION.md`.

## Outstanding before v1.0

None of these are correctness blockers.

- [x] **Stateful `File<'fs, S>` handle** with `open / read / write / seek / sync / set_len`. Batches many writes into a single `UpdateCtz` commit at sync time. CTZ-backed regular files (and missing-with-`create` and truncated-to-empty files); inline files are rejected with a typed error so the path-based API stays the right tool for small upserts. Random in-place writes (cursor `!=` size) are out of scope and return [`Error::OutOfRange`]. Drop discards uncommitted writes (the chain blocks become orphan and are reclaimed by the next allocator scan; no corruption).
- [ ] **`cargo kani --features kani` job in CI** (gated on Kani availability on GitHub Actions hosted runners). Harnesses are in place under `src/verify/`; CI integration is a future enhancement.

## Out of scope

- **LittleFS v1 on-disk format support.** The crate name is `littlefs2-pure` for a reason.
- **HardTail-chain pair relocation.** Our writer never emits `HardTail` tags (directories cap at `MAX_LIVE_ENTRIES = 256` per pair without splitting). Continuation-pair relocation is unreachable through this kernel; supporting it would require first adding directory splitting and a corresponding `UpdateHardTail` write op. The path is documented in ADR-0005.
- **Async I/O.** The `Storage` trait is synchronous. A future async wrapper crate is allowed but does not block v1.0.
- **Multi-threading.** The kernel is single-threaded by construction; users serialize at the boundary.
