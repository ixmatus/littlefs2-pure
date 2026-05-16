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
| `mount_image` | `Fs::mount` + a bounded root listing on an arbitrary whole-device image is total: a mounted filesystem or a typed `Error`, never a panic, never an out-of-bounds store access, never an unbounded loop |

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

A `fuzz_smoke` job runs every target for a short bounded budget
(`-runs`, a few seconds each) on every push. It is advisory: like the
Kani job it stays out of the required checks, because a libFuzzer
toolchain or sanitizer regression should flag here rather than block
unrelated changes. The smoke budget catches a target that panics on
its seed or fails to build; it is not a substitute for a real
campaign.

The exhaustive runs stay manual. Their runtime is unbounded by design;
they are for ad hoc "discover what panics" sessions and for the
longer running verification gate before each minor release.
