# Implementation plan (retrospective)

This file recorded the phased path from the empty repository to a v1.0 pure Rust LittleFS v2 implementation. Every phase below has shipped on `main`; the live status now lives in [`KNOWN_ISSUES.md`](../KNOWN_ISSUES.md) and [`CHANGELOG.md`](../CHANGELOG.md). Keep this file as a record of the original sequencing.

## Phase 0: foundation (shipped)

**Goal.** The bit-accurate types and the verification scaffolding.

- [x] Workspace and package layout.
- [x] CLAUDE.md, PLAN, ADRs 0001 through 0004.
- [x] `Storage` trait.
- [x] CRC32 (LittleFS variant) with property test against the bit-by-bit reference.
- [x] `Tag` type with bit layout, type enumeration, encode and decode, and a property test for encode-then-decode.
- [x] `BlockAddress` and `BlockPair`.
- [x] `Path` (fixed capacity, validated).
- [x] `Error` enum.

## Phase 1: read path (shipped)

**Goal.** Mount a C-reference-produced disk image and walk directories and files.

- [x] Metadata pair reader.
- [x] Superblock parser.
- [x] Directory traversal (including splice and HardTail chasing).
- [x] File read: inline structs and CTZ skip lists.
- [x] Conformance harness with four committed golden vectors.

## Phase 2: write path (shipped)

**Goal.** Write a file, close it, unmount, remount, and read it back. Round trip equals the C reference's output.

- [x] Commit construction with FCRC redundancy.
- [x] Block allocator (BFS scan).
- [x] Compaction on overflow.
- [x] File write inline and CTZ, with auto-dispatch by size and transparent layout transitions on update.
- [x] `Fs::format`.
- [x] `Fs::sync`.
- [x] Streaming `append_to_path`.
- [x] `mkdir`, `rmdir`, `rename`, `rename_in_dir`, `remove_at_path`, `read_at_path`, `size_of`, `truncate_path`, `list_dir`, `list_root`, `exists`.
- [x] User attribute read and write (`get_attr`, `set_attr`, `remove_attr`).
- [x] Round-trip conformance via a C verifier built from `tools/verify_image/`.

## Phase 3: hardening (shipped)

**Goal.** Power loss survives a kill at every interesting point.

- [x] Power-loss safety verified via `TornWriteStorage` sweep across every program-call boundary in inline write and CTZ streaming append.
- [x] Atomic move state recovery for cross-directory rename (v1.1).
- [x] Inter-pair wear levelling via compact-time pair relocation (v1.2).
- [x] Fuzz harnesses on the parsers and the commit reader (`fuzz/`).
- [x] Kani harnesses on tag totality, CRC equivalence, revision-counter compare, and the commit reader's panic-freedom.

## Phase 4: v1.0

Tracked in [`KNOWN_ISSUES.md`](../KNOWN_ISSUES.md) under "Outstanding before v1.0":

- Stateful `File<'fs, S>` handle (ergonomic, not correctness).
- `cargo kani --features kani` in CI (Kani availability gate on hosted runners).
- Mount-time orphan recovery for half-completed wear-levelling relocations (acceptable miss in current form).

None of the open items are correctness blockers. The v1.0 release waits on stateful `File` (the only user-visible API gap) and a final API-surface freeze pass.

## Non-goals

- LittleFS v1 on-disk format support.
- Asynchronous I/O.
- Multi-threading inside the kernel.
- HardTail-chain pair relocation through this writer (unreachable; documented in ADR-0005).
