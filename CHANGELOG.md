# Changelog

All notable changes to `littlefs2-pure` land here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The 0.x line is explicit about API churn; `KNOWN_ISSUES.md` lists every gap against the v1.0 surface.

## [Unreleased]

### Added

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
