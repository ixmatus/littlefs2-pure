# Known issues

**v1.0 — the punch list is closed.** Every spec-driven and infrastructure item that gated v1.0 shipped before the freeze; the checklist below is the archival record of how we got there. The "Out of scope" section at the bottom enumerates the deliberate v1.x non-goals — items we will not add without a major version bump.

Open issues against the v1.x line (regressions, ergonomic gaps that surface in real use) belong in the GitHub issue tracker, not in this file.

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
- [x] FCRC, written **and** validated. `meta::Commit::finish_padded` emits an FCRC tag describing the next prog window's post-erase CRC and pads the CCRC body so the next commit starts at a prog-aligned offset. `MetadataReader::new`, after a commit's CCRC verifies, recomputes that window's CRC from disk; on a mismatch (a torn write inside the program that followed the commit) it reports the block as **not erased** so the next writer compacts onto a fresh block, while keeping the durable commit (it never rolls back a CCRC-valid commit). This mirrors `lfs_dir_fetchmatch`, where the FCRC governs `dir->erased` and never `dir->off` (review items R2 and `lfs-7ts`/`lfs-3q9`; the original R2 remediation rolled the commit back, which lost durable data and is corrected here). Verified by `tests/review_r2_fcrc.rs` and the C-written conformance vectors.
- [x] Block allocator: scan-based BFS walk from root, tracks used blocks via bitmap. (`src/alloc.rs`)
- [x] Compaction on overflow: when the active block fills, GC live state plus the new write into a fresh commit on the alternate, bump revision.
- [x] NOR-aligned program wrapper: `NorAlignedStorage` caches programs to `PROG_SIZE`-aligned windows. (`src/nor.rs`)
- [x] File write inline (root and arbitrary path) with upsert semantics.
- [x] File write CTZ (root and arbitrary path), including CTZ-on-CTZ updates, CTZ-on-inline / inline-on-CTZ transitions handled transparently.
- [x] `append_to_path`: streaming append for CTZ files. Fills the existing tail block in place via NOR sub-window programs and allocates only the blocks needed for overflow. Write amplification per append is bounded by `additional.len() + one block per ~block_size of overflow`.
- [x] `Fs::format` producing a superblock the C reference can mount, verified bidirectionally via `tests/conformance.rs` and `tests/roundtrip.rs`.
- [x] Sync semantics. `Fs::sync` exposes the storage layer's sync; every public mutation already syncs as its final step.
- [x] `remove_from_root`, `remove_at_path`: delete a file by name, splice-correct.
- [x] `list_root`, `list_dir`: enumerate entries; splice-correct; chase HardTails with a Brent's cycle-safe walk (no arbitrary length cap at this layer; a cyclic chain is rejected `Corrupt`, see ADR-0009). End-to-end the reachable metadata-pair set is bounded by `MAX_QUEUED_PAIRS = 32` at mount; see the limitation note below.
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

- [x] Power-loss safety, both torn-write classes. Program-call-boundary tears land the FS as either the pre-state or post-state, verified by `tests/power_loss.rs` (`TornWriteStorage`, inline-write and CTZ-streaming-append scenarios). Intra-program tears (a power loss inside the program that follows a CCRC-valid commit) are handled reader-side by FCRC validation: the durable commit is kept (its own CCRC verified, so it is not affected by a tear in the *following* window), and the block is reported as not erased so the next write compacts onto a fresh block rather than appending into the dirty window, verified by `tests/review_r2_fcrc.rs` (review items R2, `lfs-3q9`). The CCRC alone could not distinguish a clean from a torn following window.
- [x] Fuzz harnesses on the parsers and the commit reader. `fuzz/` (libFuzzer, nightly-only, outside the main workspace) covers `MetadataReader::new`, `Tag::from_bits`, `Path::new`, `Superblock::from_bytes`, and `CtzStruct::from_bytes`.
- [x] Kani harness: revision counter comparison totality under wrap.
- [x] Kani harness: commit accept-or-reject dispatch **panic-freedom**. `commit_proofs.rs` stubs `crc::update` to a nondeterministic value, so the harness proves the dispatch never panics on arbitrary input (the stub strengthens that result); it does **not** prove accept/reject *correctness*, which is pinned instead by the conformance and roundtrip vectors against the C reference.
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

## v1.0 freeze record

Every item that gated v1.0 shipped before the freeze. The list is preserved here for archaeology.

- [x] **Stateful `File<'fs, S>` handle** (v0.2.0).
- [x] **Atomic move state recovery for cross-directory rename** (v1.1, shipped in v0.1.0).
- [x] **Inter-pair wear levelling via pair relocation** (v1.2, shipped in v0.1.0).
- [x] **Mount-time orphan recovery for half-completed wear-levelling relocations** (v0.3.0). XOR-balanced `RelocateState` gstate tag rides every relocation; mount-time BFS decodes non-zero aggregates and emits a balancing commit on the source pair.
- [x] **`cargo kani --features kani` in CI** (v0.3.1). 17 of 17 harnesses discharge via `model-checking/kani-github-action@v1`; the sweep caught a real panic-on-adversarial-input bug in `MetadataReader::new` (CCRC with `body_len < 4`), fixed and regression-pinned in the same release.
- [x] **API freeze pass** (v1.0.0). `#[non_exhaustive]` applied to `Error`, `EntryKind`, `AbstractType`, `TagType`; dead `ReadOnlyStorage` trait pruned; lib preamble and README rewritten to commit to the v1.x semver contract.

## Out of scope

- **LittleFS v1 on-disk format support.** The crate name is `littlefs2-pure` for a reason.
- **HardTail-chain pair relocation.** Our writer never emits `HardTail` tags (directories cap at `MAX_LIVE_ENTRIES = 256` per pair without splitting). Continuation-pair relocation is unreachable through this kernel; supporting it would require first adding directory splitting and a corresponding `UpdateHardTail` write op. The path is documented in ADR-0005.
- **Reachable metadata-pair set larger than `MAX_QUEUED_PAIRS = 32`.** `Fs::mount` runs the `accumulate_gstate` move/relocation-recovery sweep, a deduplicating BFS over the directory forest bounded by `MAX_QUEUED_PAIRS = 32` (`src/alloc.rs`). An image whose reachable pair set exceeds 32, including a single directory the C reference split across more than 32 continuation pairs, is rejected at mount with `Error::OutOfRange` before enumeration is reached. The tail-walk layer itself is cycle-safe and cap-free (Brent's, ADR-0009); lifting this end-to-end limit would require an unbounded dedup set in a no-alloc kernel and would enlarge the ADR-0006-pinned Cortex-M0+ stack arrays, so it is a deliberate v1.x constraint, not a defect. Documented in ADR-0009.
- **Non-UTF-8 entry names through the public path API.** LittleFS entry names are arbitrary bytes; the crate's lower layers are byte-clean, but [`Path`] is the public chokepoint and is UTF-8. A conformant C filesystem containing a non-UTF-8 entry name is therefore partially unreachable through the `Path`-taking API. The bytes are intact on disk; only the safe path surface declines them. Widening `Path` to bytes is a 2.x API question.
- **`INLINE_MAX = 128` is a fixed writer-side inline/CTZ threshold.** The C reference chooses the inline cap dynamically from geometry; `Fs::INLINE_MAX` hardcodes 128. This affects only this crate's *writer* (where it promotes a file from inline to a CTZ chain). The reader dispatches on the on-disk struct tag type, not on size, so a C-written file is read back faithfully whether or not C would have kept it inline; read interop is fully preserved. The only divergence is the boundary at which this crate's writer promotes to CTZ. Benign, documented here so it is not rediscovered as a divergence.
- **Async I/O.** The `Storage` trait is synchronous. A future async wrapper crate is allowed but does not block v1.0.
- **Multi-threading.** The kernel is single-threaded by construction; users serialize at the boundary.
