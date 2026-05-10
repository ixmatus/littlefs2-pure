# Implementation plan

A phased path from the empty repository to a v1.0 pure Rust LittleFS v2 implementation. Each phase has an exit condition; the next phase does not start until the exit condition holds.

## Phase 0: foundation (this phase)

**Goal.** The bit accurate types and the verification scaffolding.

- [x] Workspace and package layout.
- [x] CLAUDE.md, PLAN, ADRs 0001 through 0004.
- [x] `Storage` trait.
- [x] CRC32 (LittleFS variant) with property test against the bit by bit reference.
- [x] `Tag` type with bit layout, type enumeration, encode and decode, and a property test for encode then decode.
- [x] `BlockAddress` and `BlockPair`.
- [x] `Path` (fixed capacity, validated).
- [x] `Error` enum.

**Exit.** `cargo test` and `cargo test --no-default-features` both green. The bit layouts in `crc.rs` and `tag.rs` are double checked against the C reference's source. No mounting or reading yet; the kernel is types and primitives only.

## Phase 1: read path

**Goal.** Mount a C reference produced disk image and walk directories and files.

- Metadata pair reader: identify the active block by revision, walk commits, verify CRCs, surface tags in order.
- Superblock parser: detect magic, version, sizes; reject incompatible layouts with a clear error.
- Directory traversal: list children, resolve a path to a metadata location.
- File read: inline structs and CTZ skip lists.
- The conformance harness gets its first vectors here: a small image with a known directory tree and known file content. The runner walks ours against the C image and asserts byte equality on every read.

**Exit.** A read only `Fs::mount` returns a usable handle on every C produced vector in `tests/vectors/`. Property tests cover walk after walk idempotence and seek correctness. KNOWN_ISSUES lists exactly what is missing for write.

## Phase 2: write path

**Goal.** Write a file, close it, unmount, remount, and read it back. Round trip equals the C reference's output.

- Commit construction: tag stream encoding, CRC tail, FCRC for redundancy.
- Block allocator with the lookahead buffer.
- Compaction: when a metadata block fills, garbage collect to the alternate block in the pair.
- File write: inline up to a threshold, CTZ extension above it.
- `Fs::format` produces a valid superblock that the C reference can mount.
- `Fs::sync` and the implicit sync on drop.

**Exit.** Round trip vectors: a Rust written image mounts in C and reads what we wrote, byte for byte. Conformance vectors expand to cover write, truncate, remove, rename, mkdir, rmdir.

## Phase 3: hardening

**Goal.** Power loss survives a kill at every interesting point.

- Atomic commit semantics: a torn write at any byte offset leaves the filesystem in a state mountable as either the pre commit or post commit value, never something in between.
- Wear leveling: `block_cycles` rotation across the pair.
- Move state recovery: the global state log handles in flight directory moves.
- Fuzz harnesses on the parsers and the commit reader.
- Kani harnesses on the commit accept/reject dispatch and the revision counter comparison.

**Exit.** A torn write fuzzer kills the writer at every page boundary and the resulting image always mounts to one of the two expected states. Property tests assert this.

## Phase 4: v1.0

- KNOWN_ISSUES.md is empty.
- CHANGELOG.md reads as a stable release.
- API surface frozen. No `#[doc(hidden)]` escape hatches.
- `cargo build --target thumbv6m-none-eabi --no-default-features` succeeds.
- The conformance harness ingests every dq*.decTest equivalent for littlefs (the C reference's `tests/` directory translated into committed golden vectors).
- Documentation pass.

## Non goals

- LittleFS v1 on disk format support. Out of scope. The crate name is `littlefs2-pure` for a reason.
- Asynchronous I/O. The Storage trait is synchronous. An async wrapper is a follow up crate.
- Multi threading. The kernel is single threaded by construction; users serialize at the boundary.

## Estimated effort

The C reference is roughly five thousand lines of dense bit fiddling, plus a comparable volume of tests. A faithful Rust port at the same quality target is similar. Phase 0 fits in a session; Phase 1 spans several; Phase 2 is the largest chunk; Phase 3 dominates calendar time because hardening is iterative. Total: weeks of focused work, not days.
