# Changelog

All notable changes to `littlefs2-pure` land here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The 0.x line is explicit about API churn; `KNOWN_ISSUES.md` lists every gap against the v1.0 surface.

## [Unreleased]

### Added

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
