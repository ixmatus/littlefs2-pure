# Six-Agent Correctness and Spec-Fidelity Review

**Date:** 2026-05-15
**Subject revision:** `main` @ `381e7ff` (v1.0.1 + the post-tag Kani CI fix)
**Method:** read-only. Six agents, no repo files modified; every High claim
independently re-verified against the cited source (`SPEC.md`, the C
reference `lfs.c`, and the crate source).

> This is a point-in-time review. The `file:line` citations were accurate on
> `381e7ff`; re-verify each one against current source before landing a fix,
> since later commits shift line numbers. The findings (not the line numbers)
> are the durable content.

A companion remediation plan tracks the fixes as beads issues `lfs` R1 through
R4; see the project task tracker. R1 (the tail-walk hang) lands first as a
patch-release candidate.

---

## Summary

The core on-disk codec is faithful to the spec and the C reference bit for
bit: tag layout, CRC algorithm, CCRC byte range, parity flip, `rev_scmp`
wraparound, superblock, splice renumbering, gstate XOR cancellation, and the
ADR-0008 format bootstrap. The defects cluster in two places:

1. Robustness against corrupt or adversarial images. The read paths are
   weaker than the C oracle.
2. A verification posture broader than its external-oracle coverage. Several
   "complete" claims rest on self-consistency rather than an external anchor.

None is a silent miscompute of well-formed data. All bite on untrusted input
or overstate what is proven.

## High — verified, fix-worthy

### 1. Unbounded tail-follow with no cycle detection (behavioral divergence)

`Fs::resolve` (fs.rs:3087) and `find_dir_pair` (fs.rs:3137) chase
`pair.reader.tail()` in a bare `loop {}` with neither a count cap nor cycle
detection. A corrupt or adversarial `HardTail` that points back into the
chain hangs `mount`/`mkdir`/`rmdir`/`rename`/`exists`/`read`/`get_attr`
forever, issuing storage reads, never returning an error. The C reference
guards every tail walk with Brent's algorithm and returns `LFS_ERR_CORRUPT`
(lfs.c:4407-4423). The crate already has the fix pattern (the
`MAX_DIR_CHAIN = 32` bound) but applies it only on the enumeration path
(fs.rs:2550), not the resolution path: inconsistent within the crate, weaker
than the oracle, undocumented. Top priority. A real liveness failure on the
exact input class the threat model names.

### 2. FCRC written but never validated by the reader (behavioral divergence + overstated completeness)

Independently found by three agents (CRC, power-loss, oracle-meta).
`Commit::finish_padded` emits a spec-shaped FCRC; `MetadataReader::new`
(meta.rs:108-192, verified) checks only the CCRC and never reads the
`ForwardCrc` tag. The C reference, after a CCRC verifies, recomputes the next
prog-window CRC and rejects the commit if the FCRC does not match the
erased-state expectation (lfs.c:1264-1340): the precise mechanism littlefs
added to close the intra-program torn-write hole. The crate's mount is
therefore safe only against whole-program-boundary tears. `KNOWN_ISSUES.md`
marks both "FCRC redundancy" and "Power-loss safety" complete `[x]` with no
caveat, and `tests/power_loss.rs` exercises only all-or-nothing program cuts
(exhaustive at every boundary, but the intra-program class FCRC exists to
catch is untested).

### 3. `MAX_DIR_CHAIN = 32` truncates long C-written directories (behavioral divergence)

`list_pair_chain` (fs.rs:2550, verified) emits each pair's entries via
`live_entries` inside the loop, then returns `Err(OutOfRange)` after 32
pairs. A directory the C writer legitimately split into more than 32
continuation pairs (many entries, small block) yields a truncated partial
entry set followed by an error: non-enumerable, with entries already
reported. The 32 bound is justified in ADR-0005 only for the writer (which
never emits `HardTail`); reusing it for the reader of external images is
unjustified and undocumented.

### 4. CTZ block pointers have no kernel-side bounds check; the `Storage` doc over-promises (defense-in-depth / doc-fidelity gap)

Verified: storage.rs:34-35 claims "The kernel defensively rejects
out-of-range pair addresses before dereferencing them", scoped to pair
addresses, yet line 32-33 explicitly names CTZ skip pointers as adversarial.
`collect_chain_blocks` (ctz.rs:228) and `read_ctz_at` (ctz.rs:302) pass an
on-disk-decoded block straight to `storage.read` with no `< BLOCK_COUNT`
guard, unlike the pair path's `pair_in_bounds`. Mitigation: the loop is
index-governed so it cannot itself spin or write out of bounds, and the
trait contract requires impls to reject out-of-range, so a conforming
`Storage` impl is safe. The defect is that the doc claims a kernel pre-check
for CTZ that does not exist, and the primary test adapter `MemStorage::read`
uses unchecked multiplication with no `block >= BLOCK_COUNT` guard, so no
test exercises a clean reject of an out-of-range CTZ pointer (the fuzzer's
`ImageStorage` does it correctly).

### 5. Verification-posture overstatement (claims not pinned by an external oracle)

Convergent across the CRC, commit, and oracle-meta agents:

- The CRC stack is table == bitwise == polynomial, all in-repo; no test
  anywhere pins `crc::update` to a C-produced value. The doctest at
  crc.rs:41 is an admitted tautology. A shared misconception of the LittleFS
  CRC32 variant would pass every property test and Kani harness; the only
  true anchor is transitive through `conformance.rs` mount.
- `KNOWN_ISSUES.md` lists "Kani harness: commit accept-or-reject dispatch
  totality" as discharged, but `commit_proofs.rs` stubs `crc::update` to
  nondeterministic, so only panic-freedom is proven (sound, and actually
  strengthened by the stub), not accept/reject correctness. The claim
  overstates what is verified.
- `tag_proofs.rs` Kani harnesses are all self-referential; bit-position
  fidelity is pinned only by a 6-row hand-computed example table.

## Medium

- **RelocateState doc cites a nonexistent spec rule (claim not pinned).**
  tag.rs:173-174 (verified verbatim): "C littlefs readers see this as an
  unknown tag and skip it per the spec's forward-compat rule." The fetched
  `SPEC.md` contains no general forward-compat skip rule, and C's
  `lfs_dir_getgstate` does not fold a 0x7fe slot, so a torn RelocateState
  aggregate's interop behavior is asserted, not verified against C. ADR-0005
  documents the crate-side recovery well but does not substantiate the
  C-interop claim. Recommend rewording to "C littlefs does not recognize
  this slot; interop with a torn 0x7fe aggregate is not verified against the
  C reference."
- **path.rs doc states a false claim about the C reference.** "LittleFS does
  not interpret `./..` specially and would create literal entries": the C
  `lfs_dir_find` (lfs.c:1483-1512) explicitly skips `.` and applies `..`
  parent-cancellation. The crate's behavior (rejecting at the boundary) is a
  sound stricter posture; only the stated rationale is wrong.
- **UTF-8-only path surface is an undocumented interop gap.** A conformant C
  filesystem with non-UTF-8 entry names is partially unreadable through
  every public path API (lower layers are byte-clean; `Path` is the
  chokepoint). Not in `KNOWN_ISSUES.md`.
- **`INLINE_MAX = 128` hardcoded** vs the C reference's dynamic policy
  (benign, interop preserved, not in `KNOWN_ISSUES.md`), and `File::write`
  is append/extend-only vs C's random in-place overwrite (documented in the
  method doc, not listed as a gap).
- **Coverage gaps:** no C-produced vector forces CTZ skip-pointer counts at
  or above 3 (deepest skip levels untested against C); the Rust to C
  roundtrip is only 3 simple scenarios (attributes, rename gstate,
  multi-level CTZ never read back by C); the allocator silently caps the
  device at 4096 blocks with no mount-time rejection of an oversized
  `BLOCK_COUNT`.

## Low / Notes

No `NotADirectory`/ENOTDIR distinction (collapsed into `NotFound`);
`InvalidPath` doc enumerates 2 of roughly 6 triggers; the tag.rs module doc
has one backwards sentence on the valid bit; ADR-0006's no-recursion-cap is
honest but the implicit 32-level by roughly 2.5 KiB-frame stack ceiling
(roughly 80 KiB worst case on thumbv6m) is unquantified; C aligns the
initial pair revision to the relocation modulus and the Rust writer does not
(benign cadence shift, worth a one-line ADR-0005 note).

## Per-spec-area verdict

| Area | Verdict |
|---|---|
| Tag bit layout, XOR log decode, parity flip | Faithful (bit for bit vs `LFS_MKTAG`/`lfs_dir_fetchmatch`) |
| CRC algorithm / CCRC byte range / rev_scmp | Faithful behavior; CRC value not pinned by any external oracle |
| FCRC | Behavioral divergence: written per spec, never validated by the reader; completeness claim overstated |
| CTZ skip-list geometry | Faithful (`lfs_ctz_index`/traverse), exhaustively property-verified within range |
| CTZ pointer dereference safety | Defense-in-depth gap: no kernel bounds check; trait doc over-promises |
| Directory splice / hard-soft tail distinction | Faithful, internally consistent, stricter than C by intent |
| Tail-chain resolution (corrupt input) | Behavioral divergence: no cycle guard, hangs; weaker than C's Brent detection |
| Long C directory enumeration | Behavioral divergence: 32-pair cap truncates then errors after emitting partial |
| Superblock / version compat / format bootstrap | Faithful; documented divergence (ADR-0008), interop gated bidirectionally |
| Allocator in-use determination | Faithful and interop-safe; silent 4096-block ceiling |
| gstate move/rename + mount-time recovery | Faithful; resolves the same way as C move-fixup |
| RelocateState interop claim | Claim not pinned: doc cites a nonexistent spec rule |
| no_std / no_alloc / forbid(unsafe_code) | Faithful; clean, additive layering |

## Suggested fix order

1. Cap and cycle-detect the resolution-path tail walk. A real hang on
   untrusted input.
2. Either validate FCRC in the reader or downgrade the
   `KNOWN_ISSUES.md`/ADR completeness claims to match reality.
3. Bound or document the `MAX_DIR_CHAIN` reader limit and add a kernel-side
   CTZ pointer bounds check.
4. Add one direct C-produced CRC test vector and correct the overstated
   Kani, RelocateState, and path.rs claims.

Items 1 through 3 are behavioral. Item 4 closes the gap between the stated
bit-accuracy posture and its actual external-oracle coverage: squarely the
frugality concern in the engineering values, since a half-pinned claim gets
rediscovered by every future reviewer.
