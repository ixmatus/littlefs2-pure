# ADR-0015: one splice core, splice-diff attr reads, and a single compaction emission stream

- **Status**: accepted; **implemented** (review arc lfs-2dg, lfs-3z8,
  lfs-fb2, lfs-r88 from the 2026-06 deep review).
- **Date**: 2026-06-11

## Context

The 2026-06 deep adversarial review of v1.2.0
(`REVIEW-v1.2.0-2026-06-10.md`) found that four of its nine Critical
findings shared one root: the live-id versus raw-tag-id distinction was
re-derived independently at five places, and each copy missed a
different part of the C reference's countermeasures.

- **C1.** Compaction re-emitted each live entry as Create + NAME +
  STRUCT only; every user attribute on every entry died at every
  compaction, and `SetAttr` / `RemoveAttr` fell through a `_ => {}`
  wildcard, so a `set_attr` landing on a full block compacted,
  persisted nothing, and returned `Ok(())`.
- **C2.** `get_attr` compared raw committed tag ids against the
  current post-splice live id. Deleting a lower-id entry made an
  attribute vanish; creating an entry that reused the raw id leaked
  the previous entry's attribute across entries.
- **C5.** `find_parent_in_tree` returned the id a `DirStruct` tag
  carried at write time; relocation consumed it as a live id and
  repointed a sibling's struct body when any Delete intervened.
- **H1.** All four forward walkers (`dir::live_entries`,
  `dir::lookup`, `fs::gather_live_slots`,
  `alloc::gather_live_structs`) required NAME tags id-dense in log
  order. C compaction emits surviving tags in log order, which after a
  rename is not ascending-id order, so valid C images failed
  `Fs::mount` with `Error::Corrupt`.

Separately, the split-point size estimate (`compact_range_size`) and
the compaction emitter (`build_compact_commit`) were parallel
constructions whose byte-for-byte agreement was enforced by a comment
("the two are edited together"), exactly the drift class that produced
C1.

The C reference's mechanisms, re-derived from the vendored oracle
(`tools/gen_vectors/littlefs/lfs.c`):

- `lfs_dir_fetchmatch` grows the entry count to `max(id + 1, count)`
  on any NAME tag (lfs.c `tempcount`), and carries a candidate match
  (`tempbesttag`) forward across splices: a splice at or below the
  candidate shifts it, an exact Delete kills it, an identical tag with
  different contents invalidates it (lfs.c:1241ff, 1280ff).
- `lfs_dir_getslice` (lfs.c:706-748) answers point queries by walking
  the log *backward* with a splice diff (`gdiff`): at each step the
  queried live id is mapped to the id the entry had at that point;
  reaching the entry's own Create terminates the query.
- `lfs_dir_compact` replays all unique tags per live id, attributes
  included, merging the in-flight commit's attrs (`lfs_dir_traverse`
  filter, lfs.c:1988ff).

## Decision

Adopt one splice core, one backward query primitive, and one compaction
emission stream; derive all consumers from those three.

1. **`dir::splice_step`** is the single forward Create / Delete / NAME
   renumbering state machine; all four walkers feed tags through it.
   It implements C's count semantics: a NAME at any id grows the count
   to `id + 1`, and slots it newly covers are not cleared, so a STRUCT
   tag that precedes its NAME in a C-compacted log parks in its slot
   until the NAME claims it. Splice tags keep strict bounds checks (C
   never writes a Create beyond the count or a Delete of a nonexistent
   id; those remain corruption signals).
2. **`meta::TagIterRev`** walks a committed log newest-to-oldest by
   XOR back-stepping (`decoded[i-1] = raw[i] ^ decoded[i]`, bit 31
   masked), the exact mechanism of `lfs_dir_getslice`.
   **`dir::attr_get`** (point query) and **`dir::for_each_live_attr`**
   (enumeration with a 256-bit seen bitmap) are gdiff walks over it.
   `Fs::get_attr` routes through `seek_entry_in_chain` (so attributes
   on split-directory continuation entries are readable; review H5)
   and then `attr_get`.
3. **`fs::emit_compact_range`** is the one function that emits a
   compaction range: Create + NAME + STRUCT per slot, with the
   in-flight op's substitutions derived by the exhaustive
   **`slot_plan`** match (no wildcard arm: a new `WriteOp` variant
   fails to compile until its compaction effect is declared), followed
   by the entry's live attributes replayed via `for_each_live_attr`,
   with `SetAttr` / `RemoveAttr` merged in. It writes to a `TagSink`:
   either the real `Commit` (in `build_compact_commit`) or a byte
   counter (`compact_range_size`), so the split-point estimate and the
   written bytes are the same computation.
4. **`find_parent_in_tree`** carries C's candidate discipline per
   scanned pair: latest content match wins, splices at or below shift
   the candidate, an exact Delete or an identical-id `DirStruct` with
   different contents kills it. The returned id is live by
   construction.

## Consequences

**Wins.** The four findings close with reproducers pinned in
`tests/review_splice_attrs.rs` (attrs survive compaction and full-block
`set_attr`; attr reads track renumbering and never leak across raw-id
reuse; relocation repoints the live entry; a hand-built C-compaction-
shaped image mounts and resolves). The splice semantics exist in one
place each (forward core, backward query), so the next walker cannot
re-derive them wrong. The estimate/emitter drift class is structurally
gone.

**Costs.** Compaction now re-parses the source block once per
`emit_compact_range` call and walks it once per emitted entry for attr
replay: O(entries x log) instead of O(log), acceptable against the
erase cost that dominates compaction, but it is new work in the hot
path. The split-point estimate includes attr bytes, so attr-heavy
directories split slightly earlier (correct, previously undercounted).
Relaxed NAME density weakens one corruption signal the strict reader
had; C parity requires that (the strictness was rejecting valid
images), and splice tags keep their checks.

**Explicitly out of scope.** The full `LiveId` / `RawId` newtype
separation (review D1) is deferred: `find_parent_in_tree` now returns
a live id by construction, but ids still cross module boundaries as
bare `u16`. The gstate and relocation family (review C4, C6, C7, H3,
H4; design D5, D6, D8) is the next arc and is not touched here. C
emission *order* inside a compacted block is not byte-matched to C
(both readers accept either order; same class of divergence as
ADR-0008).

## Related

- `REVIEW-v1.2.0-2026-06-10.md` findings C1, C2, C5, H1, H5; design
  observations D1, D2, D3, D4.
- Beads: lfs-2dg, lfs-3z8, lfs-fb2, lfs-r88 (this arc); lfs-e7i (H5,
  the chain-aware `get_attr` half landed here, the attr test suite is
  lfs-h1p).
- ADR-0004 (C reference as behavioral oracle), ADR-0006 (stack
  budgets; the seen bitmap is 32 bytes, no new slot arrays),
  ADR-0008 (precedent for documented non-byte-identical divergence).
- Oracle: vendored `lfs.c` at `tools/gen_vectors/littlefs/`
  (`lfs_dir_getslice`, `lfs_dir_fetchmatch`, `lfs_dir_compact`,
  `lfs_fs_parent`); registry entry `docs/references/`.
