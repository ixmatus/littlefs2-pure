# Changelog

All notable changes to `littlefs2-pure` land here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The 0.x line is explicit about API churn; `KNOWN_ISSUES.md` lists every gap against the v1.0 surface.

## [Unreleased]

## [1.2.0] - 2026-05-30

The v2 write-completeness milestone: the crate's writer now produces the full LittleFS v2 on-disk surface it could already read. Three additive features land together, so the public API only grows and this stays a minor bump (the crate version in Cargo.toml remains 1.0.0, as with prior tags). A crate-written image with subdirectories is now safe for any conformant littlefs to read *and* write, directories grow without a one-pair cap, and a single worn block under a metadata or file commit is recoverable rather than fatal. The conformance (8/8) and round-trip (now 5/5) gates against the C reference hold throughout.

### Added

- Write-side directory splitting across `HardTail` continuation pairs (`lfs-cvh`, ADR-0013), lifting the cap of one metadata pair per directory. A directory that overflowed its pair previously returned `Error::OutOfRange`; the writer now splits, growing the directory across a chain of continuation pairs up to the reachable-pair budget. Every directory write (create, update, remove, and name lookup) chases the `HardTail` chain to the pair owning the target id, and a compacting commit that would exceed the block splits instead of erroring: the continuation (the upper half of the entries) is allocated and written first, then the original's lower half commits with a `HardTail` to it, so a crash before that commit leaves the continuation an unreferenced orphan the next allocator scan reclaims. The root pair `{0, 1}` grows the same way, behind a superblock-expansion fullness guard that declines to split once free space drops below an eighth of the device (a root continuation chain cannot be relocated or reclaimed). Continuation pairs are bidirectionally interoperable with the C reference (`roundtrip_split_dir`, `roundtrip_split_root`). The allocator's reachable-block scan was made splice correct as part of this (`lfs-fvw`): it now walks the latest-wins live entries rather than the raw tag stream, so a freed CTZ chain on a half-full split pair is reclaimable. Crash safety is pinned across every program boundary by `tests/dir_split_*.rs`.
- Global directory-list threading via `SoftTail` tail pointers (`lfs-xmx`, ADR-0012), closing a read/write interop defect with the C reference (`lfs-l3f`). The C reference enumerates metadata pairs by walking the tail thread from the superblock pair, not the `DirStruct` tree, so a crate-written image with subdirectories (which emitted no tail tag at all) had pairs the C allocator and `lfs_fs_traverse` never visited: C could read such an image but writing into it would allocate blocks it believed free and corrupt the subdirectories. The writer now threads every directory into the global list on `mkdir` (atomic with the parent commit), repoints the thread predecessor when a pair relocates, unthreads on `rmdir`, and reclaims crash orphans at mount via a deorphan sweep (a pair left in the thread but not the live tree). Crate-written images are now safe for any conformant littlefs to traverse and allocate against. Verified across every power-loss boundary by the threading and deorphan suites.
- Failure driven relocation of a metadata pair past a worn block (`lfs-23f`), completing the fault tolerance that an earlier part brought to CTZ data blocks. A metadata commit that cannot write one of the pair's blocks (a worn block that refuses programs) now relocates the pair onto a freshly allocated block instead of returning `Error::Io`, so a single bad block under a directory operation is recoverable rather than fatal. Wear levelling writes the compacted bytes to the pair's alternate first as an in place durability anchor; the failure path cannot use that anchor when the alternate is the worn block, so it writes the compacted commit only to the fresh block and lets the parent's `DirStruct` repoint linearize it. The reachable `RelocateState` aggregate is therefore always balanced (zero before the repoint, a cancelling pair after it), so the mount-time relocation recovery never fires on the worn pair it could not write; the active block selection ignores blocks without a verified CCRC, so a crash before the repoint mounts as the pre-state with the fresh block reclaimed as an orphan, and a crash after it mounts as the post-state, never a partial commit. The helper relocates past a worn alternate on a plain compaction or a directory split, a worn continuation block (excluded and reallocated), and a worn active block hit by an append (the append falls back to a relocating compaction that eagerly evicts the worn block onto a fresh one); the root pair cannot relocate, so a worn root commit stays `Error::Io`, and a wholly worn device fails bounded by `MAX_BAD_BLOCK_RETRIES` rather than looping. The on-disk shape matches a wear relocation, so `build_relocate_body`, `propagate_relocation`, `accumulate_gstate`, and `recover_pending_relocation` are reused unchanged. Crash safety across a power loss at every program boundary, the gstate balance, and the retry bound are pinned by `tests/badblock_reloc_crash.rs`; the functional paths by `tests/pending_badblock_reloc.rs` and `tests/badblock_split_reloc.rs`. Design in `docs/decisions/0014-failure-driven-pair-relocation.md`.

## [1.1.0] - 2026-05-29

Remediation of the 2026-05-29 multi-agent review. Additive public API only (new `ctz::seek_block`, `alloc::alloc_blocks_cached` / `alloc::alloc_one_block_cached_single_buf`, `MetadataReader::erased`, and a `Debug` impl on `alloc::Bitmap`); no breaking changes, so the 1.x semver contract holds and this is a minor bump. Two genuine correctness bugs on the power-loss and NOR write paths are fixed, plus one latent storage-adapter bug, a set of documentation-fidelity corrections, and the allocation/append performance backlog. The FCRC fix corrects a prior remediation (v1.0.2 item R2) that itself introduced a data-loss divergence from the C reference.

### Fixed

- The FCRC reader no longer rolls back a durable commit; it keeps the commit and reports the block as not erased, matching the C reference (`lfs-3q9`). The v1.0.2 R2 remediation, after a commit's CCRC verified, recomputed the following prog window's CRC and rolled the commit back one level on mismatch. That description ("C rejects the commit") was wrong: `lfs_dir_fetchmatch` fixes `dir->off` once a CCRC verifies and never moves it again, and the FCRC governs only `dir->erased`. The rollback therefore discarded durably committed metadata on the exact intra-program torn-write case the filesystem exists to survive (a power loss inside the program that follows a CCRC-valid commit), presenting stale state where the C reference reads the latest commit. `MetadataReader` now exposes an `erased()` flag (the FCRC matched the on-disk window and the boundary is prog-aligned); the writer's append-in-place path requires it and otherwise compacts onto a freshly erased block, so a torn following window forces a compact rather than an append into dirty cells. The corrected behaviour is pinned at the reader level and end-to-end through the public API by `tests/review_r2_fcrc.rs`; the clean path is byte-identical to before, so the conformance (8/8) and roundtrip vectors are unchanged.
- `File::set_len` shrink followed by an extending write no longer corrupts data on NOR flash (`lfs-6o9`). A shrink that left the new tail block partially full reused that already-programmed block; the next `File::write` filled its tail region in place at an offset still holding stale content, and on a `1 -> 0`-only device the appended bytes were ANDed with that content (for example `0xAA & 0x55 == 0x00`). A partial tail is now relocated copy-on-write to a freshly erased block before it becomes the new head, so the later in-place fill lands on `0xFF` cells. The old chain and the committed metadata are untouched until sync, so the operation stays power-loss atomic and the orphaned block is reclaimed by the next allocator scan. Pinned by `tests/review_shrink_append.rs` under `NorAlignedStorage<StrictNorStorage>` (the bug is invisible on a permissive RAM backing).
- `NorAlignedStorage` no longer drops the first of two sub-window programs to the same prog window when no sync intervenes (`lfs-8o1`). `load_window` re-read the window from the device when the cache was dirty for that same window, clobbering the pending bytes; it now keeps a dirty same-window cache so both `1 -> 0` programs survive the flush. Latent (unreachable through the kernel, which syncs between sub-window programs) but reachable for a direct caller of the public adapter. Pinned by `tests/nor.rs`.

### Performance

- The block allocator now keeps a lookahead cache of in-use blocks on the `Fs`, so steady-state allocation serves from RAM instead of re-walking the reachable forest on every block-allocating write (`lfs-opt`, ADR-0010). Previously each `File::write`/`append_to_path` overflow, `mkdir`, and CTZ create paid `O(reachable blocks)` of flash reads for `scan_used_blocks`; a small CTZ write rose from 12 reads at one reachable pair to 68 at 29 pairs. The cache is an over-approximation of in-use blocks (it can mark a freed block as still-used but never a live block as free), refreshed from an authoritative scan on a miss and after a free, so it never hands out a live block; under-marking is structurally impossible. After the change the allocating write is flat at about 10 reads regardless of forest size (the cold-cache first allocation still pays one scan). The `Fs` grows by one 513-byte `Option<Bitmap>` (one per mount, within the ADR-0006 budget). The previously-declared but unconsumed `Storage::LOOKAHEAD_SIZE` intent is now honored. Verified by the full suite (the wear-levelling / power-loss / atomic-move churn would expose a double allocation) plus a new create/delete/re-create reclaim stress test (`tests/review_lookahead.rs`).
- CTZ append no longer re-walks the whole chain on every write (`lfs-o72`, ADR-0011). `stream_ctz_extend` previously called `collect_chain_blocks` (a full backward walk) per call, so reads per single-block append rose linearly with chain length (2 at length 1 to 82 at length 200), i.e. O(n) per append and O(n²) to build a file. A new `ctz::seek_block` resolves any block address from the head in O(log n) reads (verified against the full-walk oracle for every index of a 200-block chain, `tests/review_seek.rs`); the in-flight chain exclusion moves into the allocator's rescan path so the hot path does no walk. Reads per append now fall to 0 to 10 across chain lengths 1 to 200. ADR-0007's no-per-`File`-cache decision is preserved (no `File` growth). Separately, `File::set_len` zero-extend fills from a 1 KiB shared `static` buffer instead of a 64-byte stack buffer, cutting a 16 KiB zero-extend from 4334 reads to 48 (combined with the seek and lookahead). A new op-count harness `tests/bench_perf_backlog.rs` records all three baselines.

### Changed

- Documentation-fidelity corrections, no behaviour change except as noted: the `Fs::list_dir` doc no longer claims a 32-pair HardTail cap with `Error::OutOfRange` (it uses the cap-free Brent's walk, ADR-0009; the 32-pair limit is the mount-time gstate sweep); the `alloc` module comment now describes `MAX_QUEUED_PAIRS = 32` as a reachable-pair-set breadth bound rather than a tree-depth bound; the `read_ctz`/`read_ctz_at` doc states the `scratch` parameter is currently unread (a 2.0 removal candidate) instead of claiming "only the first 8 bytes are touched"; ADR-0005 records that the old alternate block orphaned by a successful relocation is reclaimed lazily at the parent's next compaction rather than promptly (`lfs-7ts`, a benign over-approximation, left as-is to avoid changing the safety-critical reachability scan). `Fs::format` now programs only the committed prefix `&scratch[..new_end]` instead of the whole block, matching every other commit path; the erase already left the tail at `0xFF`, so the on-disk result is identical and no prog cycles are spent on the all-`0xFF` tail (`lfs-nqy`).

## [1.0.2] - 2026-05-16

Six-agent correctness review remediation, plus the post-`v1.0.1` advisory-Kani CI repair. No public API changes; the 1.x semver contract is intact. The behavioural changes are on the adversarial-input and power-loss paths (R1/R2/R3); R4 is verification-posture and documentation only. The full review is archived at `docs/reviews/2026-05-15-six-agent-correctness-review.md`.

### Security

- Every reader `HardTail` walk is now cycle safe with no arbitrary length cap (Brent's algorithm, `BrentTailWalk`, ADR-0009). `Fs::resolve`'s final component loop and the internal `find_dir_pair` previously chased `pair.reader.tail()` with no count cap and no revisit check, so a corrupt or adversarial image whose tail points back into its own chain (a self cycle, or an A to B to A loop) made path resolution, `exists`, and the operations layered on them issue storage reads forever and never return: a liveness failure on the exact untrusted input class the threat model names (review item R1). The directory enumeration path `list_pair_chain` had a separate defect: a fixed 32 pair cap that emitted each pair's entries then returned `OutOfRange`, with no cycle detection, so a cyclic chain spammed duplicate entries before erroring (review item R3). All three call sites now share one O(1) memory Brent's walker: a valid chain of any length is followed, a cyclic chain is rejected with `Error::Corrupt`, matching the C reference. `Fs::mount`'s gstate sweep was already deduped and is unaffected; the end to end reachable pair set is still bounded by `MAX_QUEUED_PAIRS = 32` at mount, a deliberate documented limit (`KNOWN_ISSUES.md`, ADR-0009). Regression reproducers in `tests/`; the `mount_image` fuzz target now also drives the resolution path. From the 2026-05-15 six agent correctness review (items R1 and R3).
- The reader now validates the forward CRC (FCRC), closing an intra-program torn-write hole in the power-loss-safety guarantee (review item R2). `Commit::finish_padded` already emitted a spec-shaped FCRC tag recording the CRC the next prog-aligned window should have while erased, but `MetadataReader::new` checked only the CCRC and never read it. A commit whose CCRC was valid but whose following prog window had been contaminated by a torn write (a power loss *inside* a program, not at a program boundary) was therefore accepted: the reader returned a metadata state the writer never atomically committed. The reader now, after a commit's CCRC verifies, recomputes that window's CRC from disk and rolls the commit back exactly one level on mismatch (an out-of-range window is treated as a mismatch); only the last verified commit needs the check, since any earlier commit is followed by a CCRC-valid commit that already proves the writer programmed past it. Behaviour change on the safety-critical commit-accept path, anchored against the C reference by the existing C-written conformance vectors (still 8/8) and the roundtrip and boundary power-loss suites (unchanged); intra-program torn writes are pinned by `tests/review_r2_fcrc.rs`. `tests/power_loss.rs` only tore at program-call boundaries and its own header documented this class as the FCRC's reader-side responsibility. From the 2026-05-15 six agent correctness review (item R2, High #2).
- CTZ skip pointers decoded from disk are bounds checked by the kernel before dereference, symmetric with the existing metadata pair check. `collect_chain_blocks` passed an on disk block address straight to `Storage::read` with no `< BLOCK_COUNT` guard, so an out of range skip pointer in a corrupt or adversarial image surfaced as the indistinguishable `Error::Io` (or, with a non conforming adapter, as memory unsafety) rather than `Error::Corrupt`. The kernel now classifies an out of range CTZ pointer as `Error::Corrupt`; the `Storage` trait doc is updated to state the kernel pre checks CTZ pointers as well as pair addresses, and the `MemStorage` test adapter is hardened (explicit `block >= BLOCK_COUNT` reject, checked arithmetic) so a test observes the clean reject. From the review (item R3, High #4).

### Fixed

- The advisory `cargo kani` CI job is repaired. It had failed at compile on every `main` push since it was introduced in v0.3.1: `crc_update_stub` is referenced only through `#[kani::stub]`, which rustc's dead code pass does not count as a use, so the false positive `dead_code` lint became a hard error under CI's `RUSTFLAGS=-D warnings` before any harness ran. `#[allow(dead_code)]` on the stub fixes the compile; all 17 harnesses now discharge in CI. No change to any non `kani` build (the item is `#[cfg(kani)]`). This is a CI only fix landed after the `v1.0.1` tag; it is not part of that release.

### Changed

- The Kani CI job pins `model-checking/kani-github-action@v1.1` and `kani-version: 0.67.0` instead of tracking floating `@v1` / `kani-version: latest`, so a Kani bump is an explicit reviewed change rather than silent drift (post-review item M9). The job remains non blocking by deliberate design.
- Verification posture corrected to match what is actually proven (review item R4, no behaviour change). `crc::update` is now pinned to an oracle external to this codebase: the published CRC-32 check value `CRC32("123456789") = 0xCBF43926` ("CRC-32/ISO-HDLC", CRC RevEng catalogue), via `!update(INIT, b"123456789") == 0xCBF43926`; the prior `crc.rs` doctest was an admitted tautology and the only anchor was transitive through conformance mount. Overstated or inaccurate claims were corrected in place: the `KNOWN_ISSUES.md` Kani commit-dispatch entry now says panic-freedom (not accept/reject correctness, which the `crc::update` stub does not prove); the `RelocateState` tag doc no longer asserts a nonexistent spec forward-compat rule; the `Path` doc no longer claims the C reference treats `.`/`..` literally (it does interpret them; the crate is deliberately stricter); the tag module valid-bit sentence is de-garbled; ADR-0006 now quantifies the worst-case stack ceiling; ADR-0005 notes the benign initial-revision modulus cadence shift; and `KNOWN_ISSUES.md` records the UTF-8 `Path` and fixed `INLINE_MAX` interop notes.

## [1.0.1] - 2026-05-15

Post v1.0 deep review remediation. No public API changes; the 1.x semver contract is intact. This release closes the review's correctness and adversarial input findings and hardens the verification surface.

### Security

- `recover_pending_move` now rejects a `MoveState` whose decoded `src_id` is past the live entry count, surfacing `Error::Corrupt` instead of committing a bogus `Delete` plus a balancing `MoveState` that would permanently mask the inconsistency. A corrupt or adversarial image can supply such a body.
- Pair addresses decoded from disk (directory pair pointers, tail links) are bounds checked before dereference: an out of range live `DirStruct` or tail is rejected as `Error::Corrupt`, and the `Storage` trait contract now states explicitly that an implementation must reject out of range accesses rather than index its backing buffer, so a malformed image cannot turn into memory unsafety in the adapter.

### Fixed

- An inline write that would overwrite an existing directory entry is now rejected, mirroring the existing guard on the CTZ write path.
- `rmdir` emptiness is counted across the whole `HardTail` chain, not just the first pair, so a directory whose entries spill into a continuation pair is no longer wrongly treated as empty.

### Added

- Per vector content pin in the conformance harness: each committed `tests/vectors/*.bin` is checked against its LittleFS CRC32, so a silent regeneration that changes the bytes is caught instead of slipping past the size only check.
- Three conformance vectors: a dense directory that spans a `HardTail` chain, files straddling the inline/CTZ size region, and a delete then recreate of the same name.
- Hand encoded spec oracle for the tag layout, plus `RelocateState` and `Unknown` coverage in the tag property generator.
- `mount_image` fuzz target exercising `Fs::mount` plus a bounded listing on an arbitrary whole device image, and an advisory `fuzz_smoke` CI job that builds and briefly runs every target.
- `cargo test --no-default-features --lib` now runs in CI, so the no_std no_alloc kernel is tested at its floor, not just built.
- `tests/bench_ctz_append.rs`, a zero dependency, ignored timing harness for the many small appends path.
- Review suggested regression tests: `Fs::format` produces a clean, stable, mountable empty filesystem; the wrap aware revision predicate is pinned at the `u32` wrap; the live entry count is stable across compaction.

### Changed

- The metadata plumbing (`MetadataPair`, `MetadataReader`, `TagEntry`, `meta::Commit`, `Fs::read_pair`) is marked `#[doc(hidden)]` with a note that it stays semver covered in 1.x but is a candidate to move to `pub(crate)` in 2.0. Nothing is removed; the tag types stay documented as a deliberate public inspection contract.
- The test suite funnels block buffer allocation through one `common::make_buffer` helper instead of repeating the geometry literal in hundreds of places.
- `CLAUDE.md`'s bit accuracy note is narrowed to what is verified: structure encoding is byte faithful and conformance pinned; the format bootstrap may differ from the C reference while remaining semantically conformant.

### Documentation

- ADR-0006 pins the per call scratch stack budget for the Cortex M0+ ship target with portable compile time guards rather than restructuring the buffers in 1.x.
- ADR-0007 records the bench gated decision not to add a per `File` CTZ chain cache: the O(N^2) chain walk is a bounded sub dominant cost the 256 block cap keeps unobservable, so the cache would trade a measured neutral gain for a stack regression and invalidation risk.
- ADR-0008 records that `Fs::format` is not byte identical to the C reference's empty format image (both are valid and interoperate) and the decision to treat that bootstrap divergence as accepted for 1.x.

## [1.0.0] - 2026-05-11

**API frozen.** Every public item ships with the v1.x semver contract. Future additive changes ship as 1.x minor releases; removing or renaming any public item requires a 2.0.

### Added

- `#[non_exhaustive]` on [`AbstractType`] and [`EntryKind`] (matching the existing annotation on [`Error`] and [`TagType`]) so a future LittleFS spec revision or fork can introduce a new variant in a 1.x minor release without forcing a major bump on every downstream pattern match.
- Crate-level [`Status`](crate#status) section in `src/lib.rs` documenting the freeze and the `#[non_exhaustive]` posture.

### Removed

- Dead `storage::ReadOnlyStorage` trait. Defined but never used anywhere in the crate or tests; pruned before v1.0 lockdown so the public surface only commits to items with proven utility.

### Documentation

- [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md): the "outstanding before v1.0" section is now the v1.0 freeze record, archived inline. The preamble points new bug reports at the GitHub issue tracker rather than the file.
- [`README.md`](README.md): status block rewritten to commit to v1.x. The dependency snippet upgraded from `"0.3"` to `"1"`.
- [`src/error.rs`](src/error.rs): module preamble corrected (was contradicting the `#[non_exhaustive]` annotation by claiming variants stay exhaustive).

### Breaking

This is the first non-0.x release; nothing in this changelog entry breaks a 1.x consumer (there is no 1.x consumer yet). The single 0.x→1.0 break is the removal of `storage::ReadOnlyStorage`, which had zero in-tree or known external users.

[`AbstractType`]: crate::AbstractType
[`EntryKind`]: crate::EntryKind
[`Error`]: crate::Error
[`TagType`]: crate::TagType

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

## Notes on versioning

`littlefs2-pure` follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html) starting at v1.0.0. Caret ranges (`"^1"`) are safe: any 1.x release will read images written by any earlier 1.x release and accept any code that compiled against any earlier 1.x.

Historical 0.x releases (0.1, 0.2, 0.3) were each permitted to break the public API; the 1.0 release froze the surface after the API-freeze audit recorded in [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md).
