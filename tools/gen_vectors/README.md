# gen_vectors

Generates the C-reference-produced LittleFS images that
`tests/conformance.rs` mounts and validates against this crate's
reader. The images are the bit-level oracle of "this is what a valid
LittleFS v2 image looks like" beyond what our own readers can verify
about themselves.

## Vendored upstream

`littlefs/` contains a pinned copy of `littlefs-project/littlefs` taken
from `littlefs2-sys 0.3.2`. The relevant identifiers:

- `LFS_VERSION = 0x00020009` (library version 2.9)
- `LFS_DISK_VERSION = 0x00020001` (on-disk format 2.1)
- License: BSD-3 (see `littlefs/LICENSE.md`)

The on-disk format version matches this crate's `DISK_VERSION`
constant, so vectors produced here are guaranteed format-compatible.
The library version is allowed to advance; what we depend on is the
on-disk byte layout.

## Building

```sh
make
```

Produces `build/gen_vectors` (a host binary, no cross-compile). Needs
only `cc` and `make`.

## Regenerating the vectors

```sh
make vectors
```

Writes the committed images to `../../tests/vectors/`:

| File | Scenario |
|---|---|
| `01_empty_format.bin` | `lfs_format` with no entries |
| `02_single_inline.bin` | one inline file `/cfg` ("hello, littlefs") |
| `03_single_ctz.bin` | one CTZ file `/payload.bin` (500 bytes, `i & 0xff`) |
| `04_nested_dir.bin` | `/audit/` containing `/audit/log` ("entry-0001;") |
| `05_hardtail_dir.bin` | `/d` filled `a`..`z`, dense enough to span a HardTail |
| `06_inline_ctz_boundary.bin` | `/b128` and `/b129`, straddling the inline/CTZ region |
| `07_deleted_recreated.bin` | `/x` created, removed, recreated with a new body |
| `08_user_attrs.bin` | `/aa`,`/bb`,`/cc` with attrs, `/bb` removed (splice; review C1/C2) |
| `09_deep_ctz.bin` | `/big.bin`, 900 bytes, a four-block CTZ chain |
| `10_delete_tombstone.bin` | `/aa` kept beside a bare `/bb` tombstone (review C3) |
| `11_compacted_rename.bin` | rename then compaction: non-id-dense NAME order (review H1) |
| `12_multimove_gstate.bin` | two renames into `/dst`: two `MOVESTATE` tags in one log (review C4) |

Vectors 08-12 are the C-written image classes that hid the top
findings of the 2026-06 deep review; each pins the read direction of a
fix (see the per-vector comments in `main.c` and `tests/conformance.rs`).

CI does not invoke this Makefile; the binaries are committed at rest.
Regenerate locally when the upstream pinning changes or you add a
scenario. `tests/conformance.rs` pins each vector's CRC32, so a
regeneration that changes any byte fails the suite until the pin is
updated in the same commit.

## Geometry

Matches `tests/common::MemStorage` so the images load directly into
`MemStorage::data` without any translation step:

- `read_size = 16`
- `prog_size = 16`
- `block_size = 256`
- `block_count = 8` (2 KiB total device)
- `cache_size = 64`
- `lookahead_size = 8`

## Adding a scenario

1. Add a new `scenario_*` function to `main.c` driving `lfs_*` calls,
   then `dump(path)` at the end.
2. Wire it into `main()` with a new output filename.
3. `make vectors`.
4. Commit the new `.bin` file.
5. Add a matching test in `tests/conformance.rs` asserting the
   expected entries via the crate's read surface.

## Round-trip (Rust -> C)

Wired via the sibling `tools/verify_image/`, which mounts a
Rust-written image under the C reference. It runs both read-only
scenarios (C reads what Rust wrote) and a `mutate` scenario (C writes
into a Rust-formatted image and Rust remounts the result, review M11).
See `tools/verify_image/README.md` and `tests/roundtrip.rs`.
