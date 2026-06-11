# Deep review of littlefs2-pure at v1.2.0 (2026-06-09/10)

Multi-agent adversarial review of the tree at commit `3ea7c85` (v1.2.0, clean, all tests green, clippy clean at `-D warnings`).

**Method.** Eight review dimensions (mutation kernel, crash-safety state machines, gstate algebra, adversarial read path, spec conformance against the vendored C reference, type design, allocator and bounds, the verification stack itself) ran as independent finder agents, ~150 agents total. Every finding was routed through two to three adversarial refuter agents with distinct lenses (code reachability, oracle fidelity, reproduction) before acceptance. 70 raw findings reduced to the deduplicated set below: 9 Critical and 8 High root causes plus Medium and Low tails. Several Criticals were reproduced live against the crate; C3 was validated end to end against the vendored C reference binary. Three findings were killed by refuters; four remain contested. The review's seed lead (a suspected mount-time panic at `gstate.rs:225`) was settled as a non-issue: the `expect` is test-only.

**Status of this document.** Findings snapshot at review time; the live queue is beads (`lfs-` prefix). This file is the archival record, not a tracker.

## Verdict

The foundational layers hold: the tag codec, CRC, metadata reader, path validation, and the read path survived adversarial scrutiny with essentially nothing above Low. The serious findings cluster in two places. First, the user-attribute subsystem is broken end to end and survives only because no test exercises attrs past the append fast path. Second, the v2 write arc (splitting, relocation, gstate) independently re-derived C's mechanisms but missed four load-bearing countermeasures the C reference carries for exactly the cross-products this review targeted: `lfs_dir_drop`'s "steal state", the `fix pending move ... is in fact _required_` relocation patch, splice-corrected parent lookup, and latest-tag-wins gstate reads. Each missing countermeasure is a confirmed Critical.

Three findings break the bidirectional interop claim outright (C3, H1, C4). The verification-stack review explains why the conformance gates never caught them: the golden vectors contain no C-compacted multi-entry block, no delete-then-C-mount scenario, and the roundtrip gate is read-only (C never writes into a Rust image), while two torn-write sweeps silently skip exactly the outcomes that would have failed.

## Critical findings

### C1. Compaction destroys all user attributes; `set_attr` on a full block returns `Ok` without persisting
`src/fs.rs:262`, `src/fs.rs:596`. Confirmed 12c/0r across three independent dimensions; reproduced live.
`gather_live_slots` (fs.rs:198-276) records only NAME and STRUCT offsets per slot; UserAttr tags fall into the `_ => {}` arm at fs.rs:272. `build_compact_commit` therefore re-emits each entry as Create+NAME+STRUCT only: every compaction strips every attribute on every entry in the pair. Additionally `WriteOp::SetAttr`/`RemoveAttr` match no arm in `build_compact_commit` (wildcard at fs.rs:596), so a `set_attr` that lands when the active block is full takes the compact path, persists nothing, and returns `Ok(())`.
Reproducer: format; `write_to_path("/f")`; `set_attr("/f", 7, b"secret")`; fill the active block with small writes until compaction fires; `get_attr` now returns 0. With the block already full, the `set_attr` itself returns Ok without persisting.
Oracle: `lfs_dir_compact` (vendored lfs.c:1988-1994) replays all unique tags per live id, attributes included, and merges the in-flight attrs of the triggering commit.
Remediation direction: record attr tag offsets in the slot-gathering pass (or replay the full tag stream during compaction) and give SetAttr/RemoveAttr explicit arms; ban the wildcard (see D4).

### C2. `get_attr` is not splice-aware: attributes vanish and leak across entries
`src/fs.rs:2359`. Confirmed 9c/0r across variants; reproduced live (append path only, no compaction involved).
`get_attr` iterates the raw committed tag stream comparing stored `tag.id()` against the current post-splice live id, with no adjustment for intervening Create/Delete splice tags. Delete a lower-id entry and the attribute apparently disappears; create a new entry that reuses the raw id and it reads the previous entry's attribute (cross-entry data leak).
Oracle: C's `lfs_dir_getslice` (lfs.c:706-748) carries a splice diff (`gdiff`) across every SPLICE tag and invalidates deleted matches.
Remediation direction: route attribute reads through the same splice-replayed view the rest of the read path uses (one shared splice walker; see D2).

### C3. Delete tags written with length 0x3FF; C resolves the deleted name to the wrong file and destroys it
`src/fs.rs:376`. Confirmed 3c/0r; validated end to end against the vendored C reference binary.
`emit_op` writes entry deletes as `Tag::new(true, Delete, id, 0x3FF)`. C writes every entry-delete with size 0 (`lfs_remove` lfs.c:3898, `lfs_rename`, `lfs_fs_demove`, `lfs_dir_drop`); 0x3FF is a reserved sentinel, and the exact 32-bit compare in `lfs_dir_fetchmatch`'s besttag invalidation (lfs.c:1244) never matches the Rust tag. Demonstrated: after Rust `remove("/bb")`, a C mount resolves `/bb` to neighbor `/aa`, serves its content, and `lfs_remove("/bb")` permanently deletes `/aa`.
Remediation direction: emit Delete with length 0; add a delete-then-C-mount conformance vector to pin it.

### C4. Per-pair gstate contribution decoded as XOR-of-all-tags; C semantics is latest-tag-wins
`src/fs.rs:815`. Confirmed 3c/0r.
`scan_pair_move_state` XOR-accumulates every MoveState tag in a pair's log. C reads a pair's contribution as the single latest matching tag (`lfs_dir_getslice` + type-masked `lfs_dir_getgstate`), and C writers fold the pair's existing contribution into each new tag before committing. A valid, crash-free C image holding two MOVESTATE tags in one log (two renames into the same directory, no intervening compaction) therefore mis-accumulates under Rust; mount recovery decodes a phantom pending move and deletes a live entry.
Remediation direction: per pair, take the latest MoveState/RelocateState tag, not the XOR of all; pin with a C-written multi-move vector.

### C5. `find_parent_in_tree` returns raw unspliced ids; relocation repoints the wrong parent entry
`src/fs.rs:1136`. Confirmed 3c/0r; reproduced with `BLOCK_CYCLES = 1`.
The parent search iterates raw tags and returns the id at tag-write time; every consumer (UpdateDirStruct emit and compact substitution) interprets it as the current live id. Any Delete committed to the parent's log after the child's DirStruct tag and before the next compaction makes them diverge: `propagate_relocation` then repoints a sibling entry's struct body, silently corrupting it.
Oracle: C's `lfs_fs_parent` resolves via `lfs_dir_fetchmatch`, which splice-corrects the matched tag (lfs.c:1241ff).
Remediation direction: splice-correct the id in the parent walk (or have it return a live id by construction; see D1).

### C6. Cross-directory rename captures the source pair address before commit 1; a relocation cascade outdates it
`src/fs.rs:2591`. Confirmed 3c/0r; reproduced via a relocation grid harness.
`move_body = build_move_body(P_src, src_id)` is computed before the destination commit. If that commit relocates the destination pair, `propagate_relocation` commits to the destination's parent, which can be (or cascade into) the source pair, relocating it. Commit 2 and every future mount recovery then target the orphaned old address: permanent duplicate entry, and the stale MoveState can never be cancelled, eventually an unmountable image.
Oracle: C patches the pending move inside the relocation walk at both the parent-commit and pred-commit sites, commented "this looks like an optimization but is in fact _required_ since relocating may outdate the move" (lfs.c:2484-2485, 2536-2537).
Remediation direction: thread relocation outcomes back to in-flight move coordinates (a bounded remap channel as the no_std `fixmlist`; see D6), or re-resolve the source after commit 1.

### C7. `rmdir` drops a pair from the reachable set without stealing its gstate contribution
`src/fs.rs:3865`. Finder-reproduced out of tree; independently re-verified by direct code read during synthesis (the limit killed this finding's refuters).
`rmdir` removes the parent entry and re-threads the predecessor with `WriteOp::Noop`, `extra_move_state = None`. A directory pair that carries a non-zero net MoveState contribution (left there by a completed rename out of that directory: balanced globally, non-zero per pair) leaves the reachable aggregate permanently non-zero once dropped. Every subsequent mount decodes a pending move against a dead pair and emits a futile balancing commit, forever.
Oracle: C's `lfs_dir_drop` (lfs.c:1831-1840) is commented `// steal state` and XORs the dropped pair's contribution into the delta committed on the survivor; called from `lfs_remove` and `lfs_rename`.
Remediation direction: scan the dropped pair's net contribution and fold it into the predecessor's re-thread commit (make pair-drop steal gstate by construction; see D5).

### C8. Failed or torn streaming append poisons the committed tail block
`src/fs.rs:1837`. 2c/0r (verified on the sibling variant; remaining refuters killed at the session limit).
`stream_ctz_extend` programs the appended bytes into the committed tail block's erased region before the overflow allocation and before `MAX_CTZ_WRITE_BLOCKS` is checked. If allocation fails (device full, retries exhausted) or power is lost, the metadata still says `old_size` but the cells past committed EOF are programmed; the next append recomputes the same offsets and programs different bytes over them, and NOR AND-semantics commit silently corrupted content.
Oracle: C never programs a committed data block twice; `lfs_ctz_extend` copies a partial tail into a freshly erased block on every extend (lfs.c:2891ff).
Remediation direction: perform all fallible steps (allocation, bounds) before the in-place fill, or adopt C's copy-out-the-tail approach for the failure paths.

### C9. Commit-time relocation/split allocations can reallocate in-flight CTZ chain blocks
`src/fs.rs:2047`. Confirmed 3c/0r.
`commit_update_ctz` and `update_inline_at_id` enter `apply_op_to_pair_inner` with `inflight = &[]`; these are the commit paths for `File::sync` and streaming append, exactly the commits whose CTZ blocks are programmed but not yet referenced by committed metadata. Inside such a commit, the wear relocation, the worn-block retry loop (which clears `used_cache` and rescans), and the split-continuation loop all allocate; the authoritative rescan cannot see the un-referenced chain and can hand its blocks out, so the commit that publishes the file destroys its data.
Oracle: ADR-0010's own invariant requires callers on the rescan path to name the in-flight chain via the exclusion parameter; these call sites pass none.
Remediation direction: thread the in-flight chain through every commit entry point (make the reserved-block set a required parameter by type; see D7).

## High findings

### H1. Reader rejects routine C-compacted directories
`src/fs.rs:249`. 3c/0r. The slot-gathering pass requires NAME tags id-dense in log order; C compaction emits orders that violate this, so valid C images fail `Fs::mount` with `Error::Corrupt`. Interop-breaking C-to-Rust. Pin with a C-compacted multi-entry vector.

### H2. No read-back verification after programming a commit
`src/fs.rs:2788`. 3c/0r. C re-reads and CRC-checks every commit (and the can_append erased verdict is only sound in combination with it). Silently corrupted programs are reported as durable success.

### H3. Abandoned wear-relocation `RelocateState` goes stale on re-relocation
`src/fs.rs:3208`. 3c/0r. If a pair whose relocation was deferred relocates again before remount, the durable RelocateState body references dead addresses; mount recovery then commits to a dead pair on every mount.

### H4. `deorphan_sweep` lacks C's half-orphan repoint
`src/fs.rs:4362`. 3c/0r. A crash between `propagate_relocation`'s parent and predecessor commits leaves the relocated pair reachable through the tree but absent from the global thread (or vice versa); the sweep reclaims rather than repoints, permanently dropping a live pair from the thread.

### H5. `get_attr` does not chase HardTail continuations while `set_attr` does
`src/fs.rs:2352`. 4c/0r. Attributes on entries in split directories are set successfully and then unreadable.

### H6. Cross-directory rename drops the moved entry's user attributes
`src/fs.rs:2549`. 6c/0r. The rename preserves NAME and STRUCT but no attrs; C preserves all unique tags of the moved id.

### H7. The torn-write sweeps overclaim
`tests/power_loss.rs:130`, `tests/dir_split_torn.rs:240`, `tests/common/mod.rs:330`. 3c/0r each.
`inline_write_atomic_across_every_power_loss` accepts `Corrupt`/`Unformatted` remounts at every trigger, so "torn write bricks the filesystem" would pass. `dir_split_torn.rs` (and `pending_softtail_torn.rs`) silently `continue` past triggers whose post-tear image fails to mount. TornWriteStorage also wraps outside `NorAlignedStorage`, so "program boundaries" are cache-flush boundaries, not device program boundaries, and no partial-program landing is ever modeled.

### H8 (contested 5c/1r). Allocator CTZ chain walk lacks the `MAX_CTZ_BLOCKS` guard
`src/alloc.rs:335`. The read and write paths cap chain walks; the allocator scan does not, so an adversarial `CtzStruct.size` drives ~`size/block_size` storage reads during every allocator rescan. The dissenting refuter rates it Medium (bounded by u32 arithmetic, no panic); the majority confirms High as a mount-adjacent DoS.

## Medium findings

- **M1.** `rmdir` of an empty multi-pair directory grafts its HardTail continuation onto the thread predecessor, silently extending an unrelated chain; C refuses with NOTEMPTY. `src/fs.rs:3871`. 2c/0r.
- **M2.** Raw-name write APIs accept names up to 1023 bytes, exceeding NAME_MAX = 255; such entries are unreachable or wrongly resolved under C. `src/fs.rs:4125`. 2c/0r.
- **M3.** Mount-time recovery dereferences (and writes through) gstate-decoded pair addresses with no `pair_in_bounds` validation, unlike every other on-disk pair pointer. `src/fs.rs:4425`, `src/fs.rs:4457`. 4c/0r merged.
- **M4.** `roundtrip.rs` converts a missing C verifier binary into a silent pass; the bidirectional bit-accuracy gate can be skipped without failing CI. `tests/roundtrip.rs:39`. 2c/0r.
- **M5.** `pending_softtail_torn.rs` discards its own load-bearing assertion with `let _` and skips unmountable images. `tests/pending_softtail_torn.rs:164`. 2c/0r.
- **M6.** TornWriteStorage's doc claims the inner storage enforces NOR semantics; the MemStorage used does not. `tests/common/mod.rs:330`. 2c/0r.
- **M7 (partial verification).** `walk_ctz_chain` issues 4/8-byte reads, violating the Storage trait's own READ_SIZE alignment precondition. `src/alloc.rs:346`.
- **M8 (partial).** NorAlignedStorage keeps a dirty window after a flush failure and defers program failures to a later flush, defeating the kernel's synchronous error handling. `src/nor.rs:100`.
- **M9 (partial).** Inflight exclusion arrays are smaller than `MAX_CTZ_WRITE_BLOCKS`; CTZ writes of 33+ blocks fail the exclusion contract. `src/fs.rs:3051`.
- **M10 (partial).** Allocator BFS dedups on mark-at-pop instead of whole-queue membership; double-enqueue can overflow the bounded queue spuriously. `src/alloc.rs:281`.
- **M11 (partial).** The roundtrip gate is read-only: C never writes into a Rust-formatted image, so the FCRC/erased-window handshake is unproven in that direction. `tools/verify_image/main.c:53`.

## Low findings

- **L1.** Single-cut split never re-checks the lower half, and the forced-victim/worn-alternate relocation paths never split at all: spurious `OutOfRange` where C succeeds. `src/fs.rs:965`. 2c/0r.
- **L2.** CCRC recognition narrower than the C reader: chunks 0x04..0x7F parse as Unknown and break the commit chain. `src/tag.rs:216`. 2c/0r.
- **L3.** `src/verify/mod.rs:23` harness inventory misdescribes `commit_proofs` coverage. 2c/0r.
- **L4 (contested).** `dir::lookup` masks corrupt splice streams as `NotFound` where its three sibling walkers return `Corrupt`. `src/dir.rs:316`.
- **L5 (contested).** C gstate orphan-count and needssuperblock bits are not modeled; residual non-zero gstate persists indefinitely under Rust. `src/gstate.rs:149`.
- **L6 (contested).** `OpenOptions` permits create without write: a nominally read-only open mutates the filesystem. Judged std-consistent by one refuter. `src/file.rs:296`.
- **L7 (partial).** gstate accumulation dedups by ordered BlockPair tuple, not physical block set. `src/fs.rs:705`.
- **L8 (partial).** `property_ctz`'s "brute force" oracle shares the per-block capacity formula with the implementation. `tests/property_ctz.rs:24`.
- **L9 (partial).** Forest walkers in `alloc.rs` skip the out-of-range pair rejection the trait docs promise. `src/alloc.rs:279`.
- **L10 (partial).** Torn sweeps arm triggers with fixed margins over a count that excludes format's program calls. `tests/power_loss.rs:117`.

## Killed findings (checked and dismissed)

- Multi-window whole-block torn-program variant of H7 (0c/3r: the failure mode is subsumed by the boundary model).
- "Undecodable non-zero gstate silently treated as no-move poisons recovery" (0c/2r: decode is total over the body length actually committed).
- Duplicate of the atomic_move content-check claim (0c/2r: the content check exists on the adjacent path).

## Design observations (selected from 52)

- **D1. Newtype the live-id versus raw-tag-id distinction.** Two `u16` meanings cross module boundaries; C2 and C5 are both instances of confusing them. A `LiveId`/`RawId` pair with explicit splice conversion makes the bug class unrepresentable. 1.x-compatible (internal).
- **D2. Unify the four splice-replay state machines.** `dir::live_entries`, `dir::lookup`, `alloc::gather_live_structs`, and `get_attr`'s walk re-implement Create/Delete replay; C1/C2 hid in the copies. One no-alloc walker with a sink trait serves all four. 1.x-compatible.
- **D3. Derive the size estimate and the emitter from one tag stream.** `compact_range_size` and `build_compact_commit` are parallel constructions that must agree byte-for-byte; generate both from a single per-entry tag iterator so they cannot drift. 1.x-compatible.
- **D4. Split `WriteOp` into compactor-replayable and log-only ops; ban the wildcard arm in `build_compact_commit`.** C1's silent drop came through `_ => {}`. An exhaustive match over a closed sub-enum turns the next forgotten variant into a compile error. 1.x-compatible.
- **D5. Make pair-drop steal gstate by construction.** A single `drop_pair(pred, dropped)` operation that scans and folds the dropped pair's contribution; `rmdir` (C7) and any future drop site cannot forget it. 1.x-compatible.
- **D6. Adopt an Fs-resident gstate (the C `lfs->gstate`/`gdelta` model) with a bounded relocation-remap channel as the no_std `fixmlist`.** The structural fix for C4/C6/C7's family: per-pair contributions become an in-RAM aggregate maintained through commits, and relocation outcomes patch in-flight coordinates. Recommended as the spine of the remediation arc.
- **D7. Make the reserved-block set a type.** Commit entry points take `Inflight<'_>` non-optionally so C9's `inflight = &[]` requires an explicit `Inflight::none()` with a justification. 1.x-compatible.
- **D8. Parse-don't-validate gstate.** Decode once into `{Balanced, PendingMove(..), PendingRelocate(..), Garbage}` instead of boolean predicates plus Option getters; mount policy for Garbage becomes an explicit match arm. 1.x-compatible.
- **D9. Storage geometry invariants at compile time.** Post-monomorphization const assertions (power-of-two, divisibility) in `Fs::mount`/`format` paths; misuse becomes a compile error, zero runtime cost. 1.x-compatible.
- **D10. Split `fs.rs` (4,918 lines) along the CommitPlan seam.** Extract a pure planning step from `compact_and_program` (it currently does five jobs) and a parameter object for `apply_op_to_pair_inner`'s sprawl; type the buffer roles. Internal refactor.
- Also noted: `Error::Io` erases `S::Error` (keep in 1.x, plan `Error<E>` for 2.x); lowest-free-ascending allocation concentrates wear on low addresses; entry creation at `id = count` diverges from C's sorted insertion (enumeration order differs, benign but worth pinning); `Tag::new(valid=false, ..)` constructible but meaningless on the write path.

## Coverage debt (prioritized)

1. C-written conformance vectors for the image classes that hid the top findings: a C-compacted multi-entry directory (H1), delete-then-C-mount (C3), a multi-move gstate log (C4), attrs, deep CTZ (3+ blocks).
2. Make the roundtrip gate read-write: C mounts a Rust image and writes into it (FCRC/erased handshake, M11), then Rust remounts.
3. Fix the three overclaiming torn sweeps (H7, M5): assert pre-state-or-post-state, never skip unmountable images, fail on `Corrupt`/`Unformatted`.
4. Run the torn sweeps through `NorAlignedStorage` and model partial-program landings.
5. Second test geometry for every integration suite (break the 256/16 monoculture).
6. Kani: CTZ geometry totality on arbitrary u32; a writer-side `Commit` harness.
7. Make `roundtrip.rs` fail (not pass) when the C verifier is missing (M4).
8. An attr suite covering compaction, splice, HardTail, and rename paths (pins C1/C2/H5/H6).

## Open questions (41 logged; notable)

Format crash window can mount the old filesystem rolled back by one commit; ADR-0006 stack headroom for the v2 relocation cascade (~1.3-1.8 KiB new depth); `BLOCK_CYCLES == 0` doc/code divergence ("negative disables" vs `<= 0`); whether mount-time recovery writing on adversarial gstate is inside the stated threat model; the 64 KiB metadata-block ceiling implied by `SlotOffsets` u16 offsets; CCRC perturb-bit handling when appending into a non-erased window; whether the spec obligates a rewriter to preserve unrecognized forward-compat tags across compaction (Rust drops them; C preserves).

## Scope

Reviewed: all of `src/` across eight dimensions; the test suite, Kani harnesses, fuzz targets, conformance and roundtrip gates as code; spec and C-reference comparison for tag encoding, FCRC, CTZ geometry, tail threading, gstate, splice, and revision discipline. Not reviewed: `tools/` C harness code quality beyond gate semantics; CI infrastructure beyond gate semantics; performance.

Evidence chains (finder reasoning plus per-lens refuter verdicts) for every finding are preserved in the session's workflow journal; structured data snapshot in `/tmp/lfs_review_synthesis.json` at review time.
