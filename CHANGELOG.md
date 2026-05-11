# Changelog

All notable changes to `littlefs2-pure` land here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The 0.x line is explicit about API churn; `KNOWN_ISSUES.md` lists every gap against the v1.0 surface.

## [Unreleased]

## [0.3.1] - 2026-05-11

### Security

- **Fix panic-on-adversarial-input in `MetadataReader::new`.** A
  wire-legal CCRC tag with `body_len < 4` (e.g., the
  special-length sentinel that decodes to `body_len = 0`, or a
  malformed 1..=3 length from a torn or attacker-controlled
  write) made the walker index past `block.len()` when reading
  the 4-byte CCRC body. The line-130 outer-bounds check verified
  `off + dsize() <= block.len()` but `dsize() = 4 + body_len`,
  so a CCRC with declared body_len < 4 slipped through and
  triggered an index-out-of-bounds panic in the LE-u32 decode.
  The fix rejects the commit at the CCRC boundary before
  indexing. Mount on a torn or adversarial pair now cleanly
  returns the previous commit boundary (or `Error::Corrupt` if
  no commit had verified) instead of panicking. Caught by the
  new Kani harness sweep against arbitrary 16- and 32-byte
  blocks; regression-pinned by
  `meta::tests::ccrc_with_short_body_does_not_panic_and_rejects_commit`.

### Added

- **`cargo kani --features kani` job in CI.** All 17
  [`#[kani::proof]`] harnesses under [`src/verify/`] discharge in
  the `kani` workflow job via the official
  `model-checking/kani-github-action@v1` action. Closes the last
  infrastructure item on the v1.0 punch list. Per-harness
  timeout 180s; the slowest harness
  (`commit_proofs::metadata_reader_does_not_panic_on_arbitrary_input`)
  runs ~90s. The kani job is not in the required-checks list
  yet because Kani's toolchain pinning can lag stable Rust;
  flagging regressions here is more useful than blocking
  unrelated PRs on a Kani-toolchain mismatch.

### Changed

- **`commit_proofs::metadata_reader_*` harnesses stub `crc::update`.**
  Without the stub, CBMC unwinds the CRC byte-loop combinatorially
  across every reachable reader path, exhausting the solver
  budget. The harnesses' property is panic-freedom on adversarial
  bytes, not CRC correctness; CRC correctness is verified
  separately in `crc_proofs` against the bit-by-bit reference.
  Stubbing `crc::update` with `kani::any() -> u32` is sound for
  this property because the reader must reject *every* path. The
  three harnesses now also carry explicit `#[kani::unwind(N)]`
  bounds (N = `block.len() + 1`) so CBMC's symbolic unwinding
  terminates.

[`#[kani::proof]`]: https://model-checking.github.io/kani/tutorial-first-steps.html
[`src/verify/`]: src/verify/

## [0.3.0] - 2026-05-11

### Added

- **Mount-time orphan recovery for half-completed wear-levelling
  relocations.** Closes the last v1.2 follow-up item documented in
  ADR-0005. Every compact-time relocation now embeds a balanced
  [`gstate::RelocateState`] tag (16-byte body: `old_pair` LE pair +
  `new_pair` LE pair) on the alternate, on the freshly allocated
  block, and on the parent's `UpdateDirStruct` commit. The three
  contributions XOR to zero once all three land. A crash that lands
  the alternate but not the fresh-block program leaves a non-zero
  filesystem-global RelocateState aggregate; [`Fs::mount`] walks
  every reachable metadata pair via the splice-correct live-entries
  view, XOR-accumulates every committed `RelocateState` body, and if
  the result is non-zero decodes `(old_pair, new_pair)` and emits a
  balancing commit on `old_pair` that cancels the cycle. The
  forfeited fresh block becomes orphan and is reclaimed by the next
  allocator scan. New `TagType::RelocateState` (chunk `0xfe` under
  the Globals abstract type, sharing the slot with `MoveState`); new
  `WriteOp::Noop` (gstate-only commits with no data tag); new
  `Gstate` fields `relocate_old_a/b` and `relocate_new_a/b` plus
  `xor_relocate_body`, `pending_relocation`, `build_relocate_body`.
- **`std` feature for the host-side library.** Renames the internal
  alias of `std`'s `alloc` extern crate to `core_alloc` so it no
  longer collides with this crate's [`crate::alloc`] block-allocator
  module when `std` is enabled. Existing downstream code using
  `littlefs2_pure::alloc::*` is unchanged. Enables `std::eprintln!`
  diagnostics in tests and integration code, and `std::error::Error`
  for [`Error`].

### Changed

- **All compact sites now route through `apply_op_to_pair_inner`.**
  Five previously-duplicated append-or-compact dispatches
  (`write_ctz_to_pair`, `commit_update_ctz`, `mkdir`, `rename_in_dir`,
  and the `remove`/`set_attr` paths) now compose with the new
  RelocateState-aware path, so the existing pair's
  XOR-accumulated `MoveState` and `RelocateState` contributions are
  preserved across every compact regardless of which surface
  triggered it. Eliminates a class of XOR-balance bugs where a
  compaction landing during the recovery window would drop one of
  the three RelocateState contributions and leave a phantom
  pending relocation visible at the next mount.
- **`accumulate_gstate` uses the splice-correct live-entries
  view** to enqueue child pairs during the BFS, not the raw
  `DirStruct` tag stream. Without this, a metadata pair whose
  `UpdateDirStruct` commits have superseded earlier `DirStruct`
  entries would enqueue the stale orphan pair address — visiting a
  block reused for unrelated content — polluting the gstate
  aggregate.

### Tests

- New torn-write atomicity test
  `relocation_atomic_across_every_power_loss` in
  `tests/wear_leveling.rs`: seeds an FS with `/sub/k = "PRE"`, runs
  a relocation-triggering write at every program-call boundary
  through a new `TornWearStorage` adapter, then asserts the
  remounted FS reads back as either pre-state ("PRE") or post-state
  ("POST"). Never corrupt, never a phantom intermediate value.
- Existing 246-test suite passes including the six wear-leveling
  tests from v1.2 (which now exercise the recovery path implicitly
  via repeated mount + remount cycles).

[`gstate::RelocateState`]: crate::gstate
[`Fs::mount`]: crate::Fs::mount
[`Error`]: crate::Error
[`crate::alloc`]: crate::alloc

## [0.2.0] - 2026-05-11

### Added

- **Stateful [`File<'fs, S>`] handle**, opened via
  [`Fs::open`]`(path, OpenOptions, ...)`. Batches a session of many
  small writes into a single `UpdateCtz` commit at
  [`File::sync`] / [`File::close`] time, while each individual
  [`File::write`] still streams bytes onto flash through the same
  NOR-friendly tail-fill + overflow-alloc path as
  [`Fs::append_to_path`]. The headline use case is log-style writers
  (the SMIL audit logger): 16 successive 32-byte writes through
  `File` produce one metadata-pair revision bump instead of 16,
  amortizing the parent-pair touch across the whole session.
- [`OpenOptions`] builder mirroring `std::fs::OpenOptions` shape:
  `read`, `write`, `append`, `truncate`, `create`. Append mode
  forces every write to land at end-of-file regardless of cursor
  position, matching `std::fs::OpenOptions::append`.
- [`SeekFrom`] enum (`Start(u32)` / `Current(i64)` / `End(i64)`) for
  [`File::seek`].
- [`File::set_len`] for shrink (drops trailing blocks; orphaned
  bytes reclaimed by the next allocator scan after sync) and
  extend-with-zero-fill (zero bytes flow through the streaming
  write path).
- [`alloc::alloc_blocks_excluding`]: variant of
  [`alloc::alloc_blocks`] that treats a caller-supplied address
  list as already in use. The [`File`] write path needs it because
  in-flight chain blocks live only in the open handle's memory
  between writes — the metadata-pair entry does not reference them
  until [`File::sync`], so a naive `alloc_blocks` would hand back
  the same physical block on the next write and corrupt the chain.

### Changed

- `Fs::append_to_path`'s internal streaming primitive is now exposed
  as `pub(crate) Fs::stream_ctz_extend`, returning
  `(new_head, new_size)` without committing the metadata-pair
  entry. The path-based `append_to_path` composes
  `stream_ctz_extend` with `commit_update_ctz`; the stateful
  [`File`] composes `stream_ctz_extend` across many writes followed
  by a single `commit_update_ctz` at sync.
- `commit_update_ctz`, `resolve_parent`, `write_inline_to_pair`,
  `apply_op_to_pair`, and the internal `WriteOp` enum are now
  `pub(crate)` so the new `file` module can reach them without
  widening the public surface.

### Scope

- The handle operates on CTZ-backed regular files. Opening an
  existing inline file (content ≤ `Fs::INLINE_MAX`) without
  `truncate(true)` returns [`Error::OutOfRange`]. Inline-style
  upserts of small configuration data stay on the path-based API
  ([`Fs::write_inline_to_root`], [`Fs::write_to_path`]).
- Random in-place writes (cursor `!=` size) return
  [`Error::OutOfRange`]; the streaming primitive does not support
  rewriting bytes in the middle of a chain. Truncate the file and
  rewrite, or use [`Fs::write_to_path`].
- Drop does **not** sync. Uncommitted writes are silently dropped
  on flash; the orphaned chain blocks are reclaimed by the next
  allocator scan, so no corruption results. Always call
  [`File::sync`] or [`File::close`] explicitly.

### Tests

13 new integration tests in `tests/file.rs` covering: write-then-sync
persists; many writes amortize to one metadata-pair revision bump;
drop without sync leaves the pre-open state intact; reads via the
handle match `read_at_path`; seek + read returns offset content;
shrink and extend via `set_len`; truncate-open then rewrite;
missing-without-create returns `NotFound`; inline file rejected;
random in-place write rejected; append mode forces writes to EOF;
the whole session survives remount.

[`File`]: crate::File
[`File<'fs, S>`]: crate::File
[`Fs::open`]: crate::Fs::open
[`File::sync`]: crate::File::sync
[`File::close`]: crate::File::close
[`File::write`]: crate::File::write
[`File::seek`]: crate::File::seek
[`File::set_len`]: crate::File::set_len
[`Fs::append_to_path`]: crate::Fs::append_to_path
[`Fs::write_to_path`]: crate::Fs::write_to_path
[`Fs::write_inline_to_root`]: crate::Fs::write_inline_to_root
[`OpenOptions`]: crate::OpenOptions
[`SeekFrom`]: crate::SeekFrom
[`alloc::alloc_blocks`]: crate::alloc::alloc_blocks
[`alloc::alloc_blocks_excluding`]: crate::alloc::alloc_blocks_excluding
[`Error::OutOfRange`]: crate::Error::OutOfRange

## [0.1.0] - 2026-05-11

First crates.io release. The kernel implements the complete LittleFS v2 surface (mount, format, full path resolution, inline and CTZ read/write, streaming append, mkdir / rmdir / rename, user attributes, atomic cross-directory rename with mount-time gstate recovery, and compact-time inter-pair wear levelling). Bit accuracy against the C reference is verified in both directions.

The version stays in `0.x` because the surface is not yet frozen: a stateful `File<'fs, S>` handle is the one remaining user-visible item before v1.0, plus the API-freeze pass. See `KNOWN_ISSUES.md` for the short list still pending v1.0.

All entries below shipped in this initial release; future releases will appear above this section.

### Added (v1.2 hardening)

- **Inter-pair wear levelling via compact-time pair relocation.**
  When a metadata pair's about-to-be programmed revision lands on
  the configured `BLOCK_CYCLES` boundary, the compactor now
  programs the compacted bytes to both the existing in-pair
  alternate (for durability) and a freshly allocated block, then
  rewrites the parent's `DirStruct` entry to point at the new
  pair address. The modulus matches the C reference exactly
  (`(rev + 1) % ((BLOCK_CYCLES + 1) | 1) == 0`), avoiding the
  documented `block_cycles = 1` non-termination and the
  `block_cycles = 2n` aliasing corner cases. The root pair never
  relocates (its parent is the spec-pinned `(0, 1)` superblock
  location). Parent updates flow through the same
  compact-or-relocate dispatch, so a relocation can cascade up the
  tree until some ancestor commits inline. `BLOCK_CYCLES <= 0`
  disables the predicate cleanly. New `WriteOp::UpdateDirStruct
  { id, new_pair }` carries the parent's rewritten reference; new
  `find_parent_in_tree` BFS-walks from root through `DirStruct`
  references to locate the parent; new
  `alloc::scan_used_with_single_buf` /
  `alloc::alloc_one_block_with_single_buf` let the relocation
  path scan for a fresh block using only the source buffer
  (because the alternate buffer holds the compacted bytes). All
  seven existing compact sites (`write_inline_to_pair`,
  `write_ctz_to_pair`, `commit_update_ctz`, `mkdir`,
  `rename_in_dir`, `remove_from_pair`,
  `apply_op_to_pair_with_movestate`) now dispatch through a
  single `compact_and_program` helper that handles relocation
  uniformly. Six integration tests in `tests/wear_leveling.rs`:
  root pair stays at `(0, 1)` under 200 compactions; subdir pair
  relocates after BLOCK_CYCLES boundary; first relocation
  replaces exactly one block (the alternate); data survives a
  remount after several relocations; nested relocation propagates
  through a grandparent; `BLOCK_CYCLES = -1` disables wear
  levelling. This closes the last documented v1.0 / v1.1 punch
  list item.

### Added (v1.1 hardening)

- **Atomic move state recovery for cross-directory rename.** A
  rename's two commits (Create-in-dst and Delete-in-src) now each
  carry a balanced `MoveState` tag whose 12-byte body XORs to zero
  once both land. A crash between them leaves the filesystem-global
  gstate non-zero; `Fs::mount` walks every reachable metadata pair
  (bounded by `alloc::MAX_QUEUED_PAIRS`), XOR-accumulates every
  committed `MoveState` body, and if the result is non-zero decodes
  the in-flight `(src_pair, src_id)` and emits the missing
  source-side Delete + balancing MoveState before returning the
  `Fs` handle. Callers never observe the duplicate-entry state.
  Compaction also preserves a pair's net gstate contribution: the
  compactor scans the source block for `MoveState` tags, XOR-folds
  them with any new contribution, and emits a single `MoveState`
  tag in the compacted block so a compaction landing during the
  recovery window does not corrupt the gstate. New `gstate` module
  (`src/gstate.rs`) hosts the encoding helpers and `Gstate` type;
  three unit tests pin the body round-trip + decode. Two new
  integration tests in `tests/atomic_move.rs`: the happy path
  (rename runs to completion, mount sees no pending move) and the
  recovery path (rename torn at every program-call boundary,
  mount-time recovery converges, second mount is idempotent).
  This closes the documented v1.0 gap "re-run rename to converge."

### Added (v1.0 finalize)

- **`Fs::sync`.** Routine durability gate; equivalent to
  `self.storage_mut().sync()` but exposed on `Fs` for callers that
  do not want to reach through the storage accessor. Every public
  mutation already syncs as its final step, so `sync()` is only
  needed when the caller mixed direct storage programs with `Fs`
  calls or wants an explicit checkpoint between mutations.
- **User attributes (`Fs::set_attr`, `Fs::get_attr`,
  `Fs::remove_attr`).** LittleFS lets each entry carry up to 256
  arbitrary key-value pairs (`UserAttr(attr_id)` tags). Latest tag
  wins at read time; remove emits a delete-marker tag with the
  length sentinel. Values capped at `0x3FE` bytes. Eight new
  integration tests cover set, get-missing, replace, remove,
  distinct-ids-independent, missing-file rejection, oversize
  rejection, remount survival.
- **Power-loss safety scenarios
  (`tests/power_loss.rs`, `TornWriteStorage`).** New storage
  adapter that simulates power-off at a configurable program-call
  boundary. Two scenarios — inline write and CTZ streaming append
  — assert the v1.0 invariant: a torn write at any program-call
  boundary leaves the FS mountable as either the pre-state or the
  post-state, never a corrupt mid-state. Verified across every
  program-call boundary in each scenario (small bounded sweep).
  Companions the Kani `commit_proofs` and the `fuzz/` parser
  totality work: those cover the spec; this covers the kernel's
  write-ordering contract.
- **Round-trip conformance against the C reference
  (`tests/roundtrip.rs`, `tools/verify_image/`).** New verifier
  binary that mounts an image produced by this crate's writer
  through C littlefs and asserts expected file contents. Three
  scenarios — inline, CTZ, nested dir — pass cleanly. Combined
  with the existing C-to-Rust direction in `tests/conformance.rs`,
  the bit-accuracy claim is now bidirectional: byte for byte, what
  we write the C reference can read, and what the C reference
  writes we can read. The verifier binary is built via
  `make -C tools/verify_image`; tests skip gracefully if the
  binary is missing.
- **GitHub Actions CI (`.github/workflows/ci.yml`).** Workflow
  matrix: rustfmt check, clippy `-D warnings`, host `cargo test`,
  `cargo doc --no-deps`, `--no-default-features` build, three ARM
  cross-compile targets (thumbv6m, thumbv8m, thumbv8m-hf), the
  C-to-Rust conformance suite, and the C-from-Rust round-trip
  suite (which builds the verifier binary before testing). Mirrors
  the local pre-commit gate.
- **LICENSE-MIT and LICENSE-APACHE text files.** The workspace
  already declared `MIT OR Apache-2.0`; this commit ships the
  actual license text so a published crate or downstream
  distribution can include the canonical files.
- **Public API doc-comment pass.** `cargo doc --no-deps` is now
  warning-free: stale intra-doc links (`Fs`, `S::BLOCK_SIZE`,
  `ctz::read_ctz`, `Commit::done`, redundant explicit targets in
  `path` module) all repaired.

### Documented but deferred

- **Wear leveling via pair relocation** stays on the v1.1 punch
  list. Within-pair wear distribution already happens naturally
  via compaction-on-fill (which alternates active and alternate);
  pair-level relocation to fresh blocks requires tracking parent
  references back to the DirStruct/HardTail that points at a pair,
  which is substantial infrastructure. Documented in
  `KNOWN_ISSUES.md`.
- **Atomic move state recovery** stays on the v1.1 punch list.
  Cross-directory rename remains "re-run to converge" after a
  power-loss between the destination Create and the source Delete.
  The proper fix (XOR-accumulated gstate tag across every metadata
  pair, mount-time BFS to recover) is a multi-hour effort; SMIL
  has no cross-dir use case so the current behavior is acceptable
  for v1.0. Documented in `KNOWN_ISSUES.md`.

### Added

- **Kani proof harnesses (Phase 3).** New `src/verify/` module, gated
  by `cfg(kani)`, dischargeable via
  `cargo kani --features=kani`. Four submodules cover the
  load-bearing primitives:
  - `tag_proofs`: `Tag::from_bits` / `into_bits` total over `u32`;
    `TagType::from_bits` / `into_bits` round-trip for every 11-bit
    type field; `AbstractType::from_bits` accepts `0..8` and
    rejects everything else; `dsize() == 4 + body_len()` always;
    `is_ccrc` and `ccrc_chunk` agree.
  - `crc_proofs`: table-based `crc::update` agrees with the bitwise
    reference `crc::update_bitwise` exhaustively over single-byte
    and two-byte inputs at every seed; streaming and one-shot
    agree; empty input is the identity.
  - `meta_proofs`: `rev_scmp` is total, zero iff equal,
    antisymmetric, and an increment-by-one is always "newer" under
    wrap. Matches `lfs_scmp` in the C reference.
  - `commit_proofs`: `MetadataReader::new` does not panic on
    arbitrary 32-byte or 3-byte inputs; rejects short blocks
    cleanly; `committed_end` never exceeds the input length.
  The module is excluded from `cargo build` / `cargo test` /
  cross-compile builds. Running the proofs requires `cargo kani`
  installed locally.
- **`cargo-fuzz` crate (Phase 3).** New `fuzz/` workspace-external
  crate (libFuzzer + nightly only) with five targets:
  `meta_reader_parse`, `tag_decode`, `path_validate`,
  `superblock_parse`, `ctz_struct_decode`. Each target asserts the
  parser's totality and post-conditions on arbitrary bytes. The
  fuzz crate has its own `[workspace]` declaration so it does not
  participate in the main workspace; CI does not run it (unbounded
  runtime by design). Intended for ad-hoc panic-hunting and
  pre-release verification gates. `fuzz/README.md` documents how
  to run.
- **FCRC commit redundancy (Phase 2g.7).** Every metadata commit
  emitted by `Fs` now carries a Forward CRC tag describing the
  expected post-erase content of the next program window, plus a
  prog-aligned CCRC body. Mirrors `lfs_dir_commitcrc` in the C
  reference (`lfs.c:1641`). The combination lets a reader detect
  torn writes that landed past the CCRC even when the partial
  write's bits happened to satisfy a CRC check. Existing commits
  remain readable; FCRC is purely additive on disk. New API:
  `meta::Commit::finish_padded(chunk, prog_size, block_size)`;
  legacy `Commit::finish(chunk)` retained for synthetic-block test
  fixtures and unit tests.
- **Conformance harness against the C reference (Phase 3
  beachhead).** New `tools/gen_vectors/` directory containing a
  vendored copy of the C littlefs source (BSD-3, pinned at
  `LFS_DISK_VERSION = 0x00020001`, matching this crate) and a small
  C driver that produces baseline disk images. The Makefile target
  `make vectors` runs four scenarios (empty format, single inline
  file, single CTZ file, nested directory) and writes the resulting
  binary images to `tests/vectors/`. Five new integration tests in
  `tests/conformance.rs` load each committed vector, mount with our
  reader, and assert the expected `(name, kind, content)` tuples.
  Bit-level cross check against the spec oracle rather than against
  ourselves; previously the bit-accuracy claim was "round-trips
  through our own reader." CI consumes the committed binaries, so
  no host C toolchain is required on builders.
- **`Error::Unformatted`.** New variant distinguishing a pristine
  (every byte `0xFF`) root pair from a programmed-but-unparseable
  one. `Fs::mount` now returns `Unformatted` for a fresh chip and
  reserves `Corrupt` for true bit-rot / torn-erase damage. Lets a
  firmware boot path branch on "format and continue" versus "page
  the on-call". Documented in the [`Fs::mount`] rustdoc and the new
  "Mount error matrix" section of `INTEGRATION.md`.
- **`Fs::tail_room(path)`.** Returns the number of bytes that fit in
  a CTZ file's current tail block without allocating a new one (0
  for inline files). Lets a log writer pack appends so overflow
  arrives on a block boundary; combined with the streaming
  `append_to_path`, this lets a batching log writer (the SMIL audit
  logger) minimize new-block allocations without internal tracking.
- **`INTEGRATION.md` expansions.** New sections: "Where the buffers
  live" (caller-supplied `BLOCK_SIZE` pair, total RAM cost), "Worked
  example: wiring a SPI NOR flash" (full Storage impl + mount with
  error branching), "Mount error matrix" (variant -> action table),
  "Power-loss recovery envelope" (per-stage interrupt behavior, the
  cross-dir-rename two-step transition, what's out of scope).
- **`Fs::rename` (Phase 2g.5): cross-directory move.** Dispatches
  same-parent paths to `rename_in_dir` (single in-place NAME tag).
  Cross-parent paths emit a `Create` in the destination parent
  followed by a `Delete` in the source parent; the source entry's
  struct body is preserved verbatim, so CTZ chains and child
  directory pairs stay in place. Rejects ancestor-cycle moves (`old`
  is a strict ancestor of `new`) with `Error::InvalidPath`. Without
  atomic-move-state recovery (Phase 3), an interrupt between the two
  commits leaves the entry visible in both directories; re-running
  `rename` converges. 8 new integration tests cover the file, dir,
  collision, cycle, and remount cases.
- **`Fs::list_dir` and `Fs::list_root` now chase HardTails.** A
  directory whose entries span multiple metadata pairs (linked by
  HardTail tags, per the LittleFS v2 spec) is enumerated end to end.
  Capped at `MAX_DIR_CHAIN = 32` continuation pairs to bound
  pathological chains. Tested against manually built two-pair root
  and subdirectory chains.
- **`apply_op_to_pair` (private helper).** Centralizes the
  append-or-compact dispatch every write path duplicated. Used by
  `Fs::rename`'s two commits; existing callers (write_inline_to_pair,
  remove_from_pair, mkdir, rename_in_dir, commit_update_ctz) are
  unchanged for now but can migrate to the helper later. Also adds a
  free `op_dsize_of(&WriteOp)` for wire-size calculation.

### Changed

- **`Fs::append_to_path` is now streaming for CTZ files (Phase 2f.2).**
  Previously every append read the entire file into `content_scratch`
  and rewrote a fresh chain (O(file_size) per call). The CTZ-extending
  path now fills the existing tail block in place via NOR sub-window
  programs (the trailing bytes of any chain block are still erased,
  so this is a legal 1-to-0 program), then allocates only the blocks
  needed for any overflow, and finally commits a single `UpdateCtz`
  tag pointing entry at `(new_head, new_size)`. Existing chain blocks
  are never re-erased and never relocated. Write amplification per
  append is bounded by `additional.len() + (one block alloc/erase per
  ~block_size of overflow)`, independent of the file size. The
  inline-only and inline-to-CTZ transition paths still assemble in
  `content_scratch`; for the CTZ-extending path callers may pass an
  empty slice. Verified end-to-end against `StrictNorStorage` (any
  0-to-1 program would panic).
- **`build_compact_commit` now handles `WriteOp::UpdateCtz` correctly
  (bug fix).** The compact path previously copied the old struct body
  even when the in-flight op was an `UpdateCtz`. Streaming appends
  fill the metadata block much faster than the rewrite-style append
  did, so the first compaction would silently drop the new CTZ head
  and size, losing every subsequent extension. The compactor now
  emits the new `CtzStruct` body for the targeted id.

### Added

- **`ctz::collect_chain_blocks`.** Refactor of the read-side backward
  walk into a reusable helper. Reads only skip-pointer headers (4 or
  8 bytes per block), so the streaming append can map an existing
  chain's logical indices to physical addresses without touching the
  content bytes. `read_ctz_at` now delegates to it.
- **`Fs::rename_in_dir` (Phase 2g.2).** Same-parent rename via a new
  `WriteOp::RenameInPlace { id, name_type, new_name }` variant. The
  reader picks the latest NAME for any given id, so appending a new
  NAME at the existing id is sufficient. Useful for SMIL audit log
  rotation (`/audit/log` → `/audit/log.archived`). Cross-directory
  rename is a follow-up (needs Delete-from-source + Create-in-dest
  with proper splice handling).
- **`Fs::rmdir` (Phase 2g.1).** Remove an empty directory at a path.
  Verifies the entry is a Directory and that its metadata pair has no
  live entries before removing it. Returns `Error::NotEmpty` if the
  directory still has contents; `Error::AlreadyExists` if the target
  is a regular file (use `remove_at_path`). After removal, the
  directory's metadata pair becomes unreachable and the allocator
  reclaims its blocks on the next scan.
- **`remove_at_path` rejects directories.** Now returns
  `Error::AlreadyExists` for directory targets, forcing callers to
  use `rmdir` explicitly (avoids orphaning the dir's contents).
- **`Fs::read_at_path` and `Fs::size_of` (Phase 2g.3).**
  `read_at_path(path, offset, &mut out, ...)` reads up to `out.len()`
  bytes starting at `offset` from any file (inline or CTZ); the
  layout is hidden. `size_of` returns the file's byte length.
  Implemented via a new `ctz::read_ctz_at` that handles arbitrary
  start offsets in the chain.
- **`Fs::truncate_path` (Phase 2g.4).** Resize a file to exactly
  `new_size` bytes. Shrinking drops trailing bytes; extending
  zero-pads. Atomic full-rewrite (same model as `append_to_path`).
- **`Error::NotEmpty`** variant.
- **`Fs::append_to_path` and CTZ updates (Phase 2f.1).** Atomic
  full-rewrite append: reads existing content, concatenates with the
  new bytes, writes back via `write_to_path`. Handles all three
  layout transitions automatically:
  - inline-grows-inline: rewrite as inline
  - inline-to-CTZ promotion: write new chain, drop inline body
  - CTZ-to-CTZ extension: write new chain, old chain orphaned
  The caller provides a `content_scratch` buffer big enough to hold
  the combined existing-plus-new content. O(file_size) per append;
  fine for the SMIL audit logger's 80-byte-entry workload. A
  stateful streaming File API (true incremental CTZ chain extension)
  is the Phase 2f.2 follow-up.
- **`WriteOp::UpdateCtz` and CTZ-on-existing-entry updates.**
  `write_ctz_to_pair` now updates an existing regular-file entry
  in-place (via `UpdateCtz`, which emits a `CtzStruct` tag at the
  existing id). The old chain becomes unreachable and the allocator
  reclaims its blocks on the next scan. `write_to_root` /
  `write_to_path` likewise handle the three transitions
  (inline↔CTZ) transparently.
- **Reject overwriting a directory with a file.** Both
  `write_inline_to_pair` and `write_ctz_to_pair` now return
  `Error::AlreadyExists` if the target name resolves to a Directory.
- **`Fs::mkdir`, path-based writes/removes, and `list_dir` (Phase 2e).**
  - `mkdir(path)`: resolves the parent, allocates a fresh metadata
    pair for the new directory, erases + initializes it with one
    empty CCRC commit, then appends a `CreateDir` commit to the
    parent pair pointing at the new dir's blocks.
  - `write_to_path(path, content)`: auto-dispatches inline vs CTZ at
    the parent of the leaf component. The parent directory must
    exist.
  - `remove_at_path(path)`: removes the file at the leaf of `path`.
  - `list_dir(path, callback)`: enumerates entries in a directory at
    `path` (root included). Applies splice renumbering; does not yet
    chase HardTails (Phase 2g).
  - Internal: refactored `write_inline_to_root`,
    `write_ctz_to_root`, and `remove_from_root` to call private
    `*_to_pair` methods, so both root-only and path-based variants
    share the same write logic. Added `resolve_parent` helper.
  - Added `WriteOp::CreateDir { id, name, dir_pair }` and threaded
    through `emit_op` and `build_compact_commit`.
- **CTZ file writes (Phase 2d) via `Fs::write_to_root` and
  `Fs::write_ctz_to_root`.** `write_to_root` auto-dispatches inline vs
  CTZ based on content size (`INLINE_MAX = 128` bytes). The CTZ path
  allocates fresh blocks via the allocator, writes the skip-list
  chain block-by-block (skip pointers at each block's head, then
  content), and appends a metadata commit with `Create` + `RegularFile`
  NAME + `CtzStruct` referencing the chain head. Round-trip verified
  via `resolve` + `read_ctz` over both fresh writes and post-remount.
  Capped at `MAX_CTZ_WRITE_BLOCKS = 256` (~1 MiB at 4 KiB blocks).
- **Block allocator (`src/alloc.rs`, Phase 2c).**
  `scan_used_blocks(storage, root, ...)` BFS-walks the filesystem
  from the root pair, marking every reachable block in a bitmap:
  visits each metadata pair once, follows `DirStruct` and `Tail`
  references into other pairs, walks `CtzStruct` chains backward
  marking each block. `alloc_blocks(out_slice, ...)` returns the
  lowest-numbered unused blocks. `Bitmap` caps device size at 4096
  blocks; deeper traversal limited by a 32-pair BFS queue.
- **`Fs::remove_from_root`, `Fs::list_root`, `Fs::exists` (Phase 2b.4).**
  Closes the CRUD surface needed by SMIL's audit-style consumers.
  `remove_from_root` appends a Delete tag (or skips the slot during
  compaction); `list_root` enumerates user entries (skipping the
  superblock); `exists` is a typed wrapper over `resolve` returning
  `bool`.
- **`dir::lookup` and `dir::live_entries` now apply splice
  renumbering (bug fix).** Prior to this commit, `lookup` would
  return deleted entries (because it scanned for the latest NAME
  match without applying Splice), and `live_entries` errored on a
  Create at id 1 in a freshly formatted pair (because it counted the
  user entries differently from `gather_live_slots`'s write side).
  Both functions now share the same slot-tracking algorithm and
  agree on counts. Superblock NAME tags are counted internally but
  not surfaced through the iterator/lookup.
- **CI workflow** (`.github/workflows/ci.yml`) running on every push:
  host fmt + clippy + test, plus cross-compile against
  `thumbv6m-none-eabi`, `thumbv8m.main-none-eabi`, and
  `thumbv8m.main-none-eabihf` (the SMIL firmware target). All three
  embedded targets check clean today.
- **INTEGRATION.md** for downstream consumers: a one-page rundown of
  what works, what's pending, and the suggested step-by-step
  integration path. SMIL audit-feedback driven.
- **`NorAlignedStorage` wrapper (Phase 2b.3).** Adapter that converts
  byte-granular `program` calls from the kernel into `PROG_SIZE`
  aligned NOR-compliant programs. Caches the active program window in
  a stack-allocated buffer (default `MAX_PROG_SIZE = 512`), flushes on
  window change or `sync`, and enforces 1-to-0 only bit transitions
  internally. Integration tests run the full format + write + remount
  loop through a strict-NOR backing storage that panics on any 0-to-1
  bit flip or misaligned program; the wrapper makes them all pass.
- **Compaction (Phase 2b.2).** `write_inline_to_root` now transparently
  compacts when the active block fills: builds a fresh commit on the
  alternate block containing every live entry plus the new write,
  bumps the revision counter, programs and erases. Subsequent mount
  picks the alternate via the standard revision-based selection.
  The superblock is preserved as id 0 of the root pair.
- **Upsert semantics for `write_inline_to_root` (Phase 2b.1).** Writing
  to an existing name now appends an `InlineStruct` at the existing
  entry's id (later tag wins), instead of returning `AlreadyExists`.
  Update commits are smaller than create commits, so the typical "save
  changed config" workload extends the active block's life
  significantly before compaction triggers.
- **`Fs::write_inline_to_root` (Phase 2b).** Append a small file to
  the root directory. Reads the active block, runs `live_entries` to
  determine the next free id, builds a new commit (Create + NAME +
  InlineStruct) on top of the existing committed region using
  `Commit::new_appending`, and programs only the new bytes to flash.
  Rejects duplicates (`Error::AlreadyExists`); returns
  `Error::OutOfRange` if the commit would overflow the block (Phase
  2e compaction lifts this).
- **`meta::Commit::new_appending`.** Continue a metadata block at a
  given offset with a pre-existing XOR base, supporting the append-
  to-existing-pair pattern.
- **`meta::Commit` slice-based commit builder (Phase 2a foundation).**
  No-alloc, no-std builder for metadata commits. Takes a caller-supplied
  byte slice; writes the revision header at offset 0; appends tags via
  `tag()`; finalizes with `finish(chunk)` which emits the CCRC and
  applies the post-commit parity flip. Decoupled from storage I/O so
  callers can stage commits in memory and program them in one shot.
- **`Fs::format` (Phase 2a).** Initial write-path operation: erases
  blocks 0 and 1, then writes a single commit on block 0 containing the
  superblock NAME magic (`b"littlefs"`) and the 24-byte InlineStruct
  carrying the device geometry. Block 1 is left in pristine erased state
  as the metadata pair's alternate.
  Round-trip verified: `format` then `mount` succeeds and returns a
  superblock matching the device geometry; the operation is idempotent
  (the second format produces byte-identical bytes).
- **Splice handling (`dir::live_entries`, Phase 1i.1).** New
  enumerator that applies Create / Delete renumbering during the walk.
  Maintains a `[Option<DirEntry<'a>>; MAX_LIVE_ENTRIES]` slot array,
  shifting on each splice tag, and emits the final live entries in
  current id order. The existing `dir::entries` is preserved as the
  raw walker. 7 integration tests cover Create/Name, Create then
  Delete, mid-delete renumbering, Create after Delete reusing
  renumbered slot, splice across commits.
- **HardTail chasing (Phase 1i.2).** `MetadataReader` now scans the
  committed region for the latest Tail tag and exposes
  `tail()` and `is_hard_tail()`. `Fs::resolve` chases HardTails at
  every component (both intermediate and final), matching
  `lfs_dir_find`'s inner loop (`lfs.c:1538`). 2 integration tests:
  resolution succeeds through a HardTail; SoftTail correctly does
  not get chased.
- **`Fs::resolve` and `ResolvedPath` (Phase 1h).** Full absolute-path
  resolution: walks from the root metadata pair through every
  intermediate directory by name, returning the final entry plus the
  pair it lives in. Buffers passed in by the caller; after return they
  contain the bytes of the final pair and the returned `ResolvedPath`
  borrows from them.
  Errors: `InvalidPath` for `/`, `NotFound` for missing components
  (leaf or intermediate) and for intermediate components that are
  regular files, `Corrupt` for malformed `DirStruct` bodies.
- **`ctz::read_ctz` storage-backed CTZ file read (Phase 1g full).**
  Walks the skip list chain backward from head using the
  `count = 2 - (index & 1)` rule from `lfs_ctz_traverse` (`lfs.c:2990`),
  collecting block addresses into a stack-allocated array bounded by
  `MAX_CTZ_BLOCKS` (= 256). Then reads each block's content portion
  forward into the output buffer, skipping the `4 * skip_pointers_in_block(i)`
  byte header. `Fs::read_ctz` is the convenience wrapper.
- **`build_ctz_chain` test helper.** Constructs a valid CTZ chain in a
  `MemStorage` from raw bytes: lays out blocks with the right number of
  skip pointers per index, addressing physical blocks `base + i`.
  Independent reimplementation of the write side; pairing it with
  `read_ctz` is a true cross check, not a self-consistency invariant.
- **9 integration tests** (`tests/ctz_read.rs`): zero bytes, fits in
  block 0, exactly fills block 0, spans 2 blocks (touches block 1's
  1-pointer header), spans 3 blocks (odd-index case), spans 5 blocks
  (touches block 4's 3-pointer header — the power-of-two skip case),
  full 6-block chain, partial read into short output, rejects
  undersized scratch.
- **CTZ skip list geometry math (Phase 1g foundations).** New module
  `ctz` carries the algorithms that map a logical file offset to a
  (block_index, absolute_offset_within_block) tuple. Matches
  `lfs_ctz_index` (`lfs.c:2843`) byte-for-byte after the property
  test caught a docs-vs-implementation mismatch about whether the
  returned offset includes the skip pointer header (it does — that's
  what makes it directly usable for a `storage.read` call).
  - `CtzStruct::from_bytes / to_bytes`: 8 byte body codec
    (head_block + size, both LE u32).
  - `skip_pointers_in_block(index)`: `ctz(index) + 1` for `index > 0`,
    else 0.
  - `content_bytes_in_block(index, block_size)`: payload bytes after
    the skip pointer header.
  - `block_count(size, block_size)`: total blocks in a chain.
  - `block_index_at_offset(offset, block_size)`: the central
    `(block, abs_off)` translator.
  - Property test `block_index_matches_brute_force` cross-checks
    against a per-block walk over 200K offsets and 6 block sizes.
- **`dir::lookup` and `dir::Resolved` (Phase 1f sliver).** Single pair
  lookup by name. Walks the tag stream twice: first to find a NAME tag
  whose body matches the requested name, then to pair it with a STRUCT
  tag (InlineStruct, CtzStruct, or DirStruct) at the same id. The
  returned `Resolved { entry, struct_type, struct_body }` carries
  enough to read an inline file directly (`struct_body` is the file
  content) or to follow into a subdirectory (`struct_body` is the
  next pair's two LE u32 block addresses). CTZ-based file content
  reading is Phase 1g.
- **`Fs::read_pair` and the `dir` module (Phase 1e).** `Fs::read_pair`
  fetches an arbitrary metadata pair from storage through two caller
  supplied buffers and runs `MetadataPair::parse` on the result. The
  `dir` module exposes `DirEntry`, `EntryKind`, and `entries(pair) ->
  Entries`, an iterator yielding one entry per NAME tag (RegularFile or
  Directory) in commit order. Phase 1e scope is intentionally narrow:
  splice handling (Delete renumbering), HardTail chasing, and full path
  resolution are deferred and called out in the module docs.
- **Three new integration tests** (`tests/dir.rs`) covering the three
  entry kinds, an empty pair, and a pair containing only a non NAME tag.
- **`Fs::mount` (storage-backed glue).** Composes `MetadataPair::parse` and
  `Superblock::from_pair` against a `Storage` backed device. Caller passes
  in two scratch buffers (each exactly `S::BLOCK_SIZE` bytes); after mount
  returns, the buffers can be reused. The returned `Fs<S>` holds the
  storage, the decoded superblock, and the root pair address.
  Geometry validation: the on disk `block_size` and `block_count` must
  match the `Storage` trait's advertised constants, else
  `Error::GeometryMismatch`.
- **`tests/common::MemStorage`.** In memory `Storage` implementation for
  integration tests. Geometry baked into the type (256 byte blocks, 8
  blocks) so the trait's associated constants resolve. Does not yet
  enforce NOR flash semantics (program-can-only-flip-1-to-0); the read
  kernel does not require it. Phase 2 will tighten this.
- **`tests/mount.rs`.** Seven integration tests: well formed image
  mounts, both blocks corrupt -> Error::Corrupt, geometry mismatch on
  block_size and block_count, wrong buffer size, unformatted device,
  storage accessor round trip.
- **Superblock parser (`superblock::Superblock`).** Decodes the 24 byte
  INLINESTRUCT body (six little endian `u32`s: version, block_size,
  block_count, name_max, file_max, attr_max) and validates the magic NAME
  tag carries `b"littlefs"`. `from_pair` is the mount entry point:
  rejects images with the wrong major version, with a newer minor than
  the crate supports, or missing the magic. Older minor versions parse
  successfully. Layout pinned against `lfs_superblock_fromle32`
  (`lfs.c:474`).
- **Metadata pair selector (`meta::MetadataPair`).** Picks the active
  block of a two block pair using `rev_scmp` (signed revision counter
  comparison, wrap aware, matching `lfs_scmp` at `lfs_util.h:164`). Falls
  back to the alternate if the active has no verified commits; returns
  `Error::Corrupt` if neither has.
- **Metadata block reader (`meta::MetadataReader`).** Walks a metadata block,
  verifies every CCRC, exposes tags from successfully committed regions via
  the `iter_tags` iterator. Bit accuracy verified against the C reference's
  `lfs_dir_fetchmatch` (lfs.c:1095): big endian tag word, little endian
  revision counter and CRC body, XOR encoded against the previous tag,
  CRC reset and `(chunk & 1) << 31` parity flip on each verified commit.
- **Tag helpers.** `dsize`, `body_len`, `is_ccrc`, `ccrc_chunk` for the
  reader and the synthetic builder.
- **Synthetic metadata block builder** (`tests/common::BlockBuilder`) for
  property testing. Independent reimplementation of the commit byte layout;
  the reader + builder agreement is a cross check, not a self consistency
  invariant.
- **Property tests for the metadata reader** (`tests/property_meta.rs`):
  single commit roundtrip, multi commit roundtrip with parity alternation,
  single byte corruption invalidates the committed region.
- Workspace and package scaffold.
- `Storage` trait: read, program, erase, sync, with associated geometry consts.
- `BlockAddress`, `BlockPair` newtypes.
- LittleFS CRC32 implementation (table based, matching the C reference's nibble table) with a bit by bit reference for property cross checking.
- `Tag` type with the 32 bit on disk layout, abstract type and chunk decomposition, valid bit handling, and the full v2 type enumeration.
- `Path` type: fixed capacity (255 byte cap, matching `LFS2_NAME_MAX`), validated on construction.
- `Error` enum.
- Property tests for CRC (table vs bit by bit) and `Tag` (encode then decode is the identity).
- ADRs 0001 through 0004 establishing the pure Rust posture, the SPEC as oracle, the five verification stacks, and the offline C reference vector strategy.
- `docs/PLAN.md` documenting the phased path to v1.0.

### Not yet implemented (see `KNOWN_ISSUES.md` for the full list)

- Mount and superblock detection.
- Directory traversal.
- File read and write.
- Format.
- Conformance harness and golden vectors.
- Kani harnesses (the `kani` feature compiles but contains no harnesses yet).
- Fuzz harnesses.

## Notes on 0.x

Every 0.x release is permitted to break the public API. Pin to an exact version (`= "0.1.0"`) if API stability matters during the read and write kernel implementation; switch to caret ranges (`"^1"`) once 1.0 ships.
