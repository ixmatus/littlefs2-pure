# littlefs2-pure-fuzz

`cargo-fuzz` (libFuzzer) harnesses exercising the kernel's parser
surface on adversarial byte inputs. The companion to the Kani proofs
in `src/verify/`: Kani exhausts small symbolic inputs, fuzz extends
coverage to the long-tail.

Lives outside the parent workspace because libFuzzer requires a
nightly compiler and ASan/UBSan instrumentation that does not mix
with the library crate's stable / no_std posture.

## Targets

| Target | Property |
|---|---|
| `meta_reader_parse` | `MetadataReader::new` on arbitrary bytes never panics, never reads past the input, `committed_end <= len`, `iter_tags` terminates |
| `tag_decode` | `Tag::from_bits` + every accessor on arbitrary 32-bit inputs is total; `TagType::from_bits` round-trips via `into_bits` |
| `path_validate` | `Path::new` rejects every disallowed UTF-8 input cleanly; accepted paths satisfy every documented invariant |
| `superblock_parse` | `Superblock::from_bytes` on arbitrary slices is total |
| `ctz_struct_decode` | `CtzStruct::from_bytes`/`to_bytes` are inverses on 8-byte inputs; non-8-byte inputs error |

## Running

```sh
cargo install cargo-fuzz   # one-time
cd fuzz
cargo +nightly fuzz run meta_reader_parse
```

Each run loops indefinitely; press Ctrl-C to stop. A corpus and
artifact directory are populated under `fuzz/corpus/<target>/` and
`fuzz/artifacts/<target>/`; both are gitignored.

To run a quick smoke check (bounded steps):

```sh
cargo +nightly fuzz run meta_reader_parse -- -runs=10000
```

## Adding a target

1. Drop a new `fuzz_targets/<name>.rs` file using the
   `#![no_main]` + `fuzz_target!` form.
2. Add the matching `[[bin]]` entry to `Cargo.toml`.
3. Run it; commit the seed corpus you find useful.

## CI

Fuzz targets are NOT run by the per-commit CI matrix — their
runtime is unbounded by design. They are intended for ad-hoc
"discover what panics" sessions and for the longer-running
verification gate before each minor release.
