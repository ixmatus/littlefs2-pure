# littlefs2-pure

A pure Rust, `no_std`, no-allocator implementation of the [LittleFS v2 on-disk format](https://github.com/littlefs-project/littlefs/blob/master/SPEC.md). No FFI, no `cc` build dependency, no global allocator inside the kernel. Bit-accurate against the C reference in both directions: images this crate writes mount cleanly through C littlefs, and images C littlefs writes mount cleanly through this crate.

```toml
[dependencies]
littlefs2-pure = "0.3"
```

License: MIT OR Apache-2.0. MSRV: Rust 1.84.

## Status

The kernel implements the complete v2 spec surface. The v1.0 / v1.1 / v1.2 / `File` / Kani-in-CI punch list is closed; every item in [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md)'s "outstanding before v1.0" section is shipped. v1.0 is on deck pending an API-freeze pass and a soak interval. The Kani sweep landing in v0.3.1 caught a real panic-on-adversarial-input bug in `MetadataReader::new`; details in [`CHANGELOG.md`](CHANGELOG.md).

Verification posture (see [ADR-0003](docs/decisions/0003-verification-stacks.md)):

| Stack | Coverage |
|---|---|
| Unit tests | Tight per-module invariants. |
| Property tests (`proptest`) | CRC, tag bit layout, CTZ geometry, metadata commit round-trip. |
| Conformance (C → Rust) | Mount C-littlefs-written images; assert expected entries. |
| Conformance (Rust → C) | Mount Rust-written images through a C verifier; assert expected content. |
| Power-loss sweep | `TornWriteStorage` / `TornWearStorage` across every program-call boundary in inline write, CTZ streaming append, cross-dir rename, and wear-level pair relocation. |
| Kani harnesses | `Tag::from_bits` totality, `crc::update` vs bitwise reference, `rev_scmp` wrap-aware compare, `MetadataReader::new` panic-freedom. |
| libFuzzer (`fuzz/`) | Parser totality on adversarial bytes for tag, path, superblock, CTZ struct, metadata reader. |

246 tests pass, clippy `-D warnings` clean, `cargo doc` warning-free under `RUSTDOCFLAGS=-D warnings`, three ARM cross-compile targets clean.

## Quick start

Implement [`Storage`](src/storage.rs) against your flash chip, then mount or format:

```rust
use littlefs2_pure::{Fs, Path, Storage, NorAlignedStorage};

let mut buf_a = [0u8; <YourStorage as Storage>::BLOCK_SIZE];
let mut buf_b = [0u8; <YourStorage as Storage>::BLOCK_SIZE];

let storage = NorAlignedStorage::new(your_storage).expect("PROG_SIZE divides BLOCK_SIZE");

let mut fs = match Fs::mount(storage, &mut buf_a, &mut buf_b) {
    Ok(fs) => fs,
    Err(littlefs2_pure::Error::Unformatted) => {
        // Fresh chip: format and retry.
        let (mut storage, mut scratch) = (your_storage_again, [0u8; <YourStorage as Storage>::BLOCK_SIZE]);
        Fs::format(&mut storage, &mut scratch)?;
        Fs::mount(NorAlignedStorage::new(storage)?, &mut buf_a, &mut buf_b)?
    }
    Err(e) => return Err(e.into()),
};

fs.mkdir(Path::new("/log")?, &mut buf_a, &mut buf_b)?;
fs.write_to_path(Path::new("/log/v1")?, b"hello", &mut buf_a, &mut buf_b)?;

let mut out = [0u8; 32];
let n = fs.read_at_path(Path::new("/log/v1")?, 0, &mut out, &mut buf_a, &mut buf_b)?;
assert_eq!(&out[..n], b"hello");
```

For a session of many small writes against the same file, the stateful [`File`] handle batches the metadata-pair commit:

```rust
use littlefs2_pure::OpenOptions;

let mut file = fs.open(
    Path::new("/log/audit")?,
    OpenOptions::new().write(true).create(true).append(true),
    &mut buf_a, &mut buf_b,
)?;
for entry in audit_log_entries() {
    file.write(entry.as_bytes(), &mut buf_a, &mut buf_b)?;
}
file.close(&mut buf_a, &mut buf_b)?;
```

Each `write` streams onto flash through the same NOR-friendly tail-fill + overflow-alloc path as `Fs::append_to_path`, but the metadata-pair entry is held back until `close` (or an explicit `sync`) so the whole session lands as a single revision bump on the parent pair.

[`INTEGRATION.md`](INTEGRATION.md) walks through a full SPI-NOR adapter, the mount-error matrix, the power-loss recovery envelope, and the verification matrix in detail.

## Design

### Why pure Rust

The existing `littlefs2` crates on crates.io are FFI wrappers around the C reference. This crate exists because the LittleFS v2 spec is durable, the artifact is load-bearing for years, the consequence of corruption is high, and pulling in a C toolchain through a Rust dependency violates the no-FFI invariant downstream embedded consumers want to keep. The [ADR series under `docs/decisions/`](docs/decisions/) records the design choices:

- [ADR-0001](docs/decisions/0001-pure-rust-no-ffi.md): No FFI in the dependency graph.
- [ADR-0002](docs/decisions/0002-spec-as-oracle.md): The spec is the oracle; the C reference is the bit-level tie-breaker.
- [ADR-0003](docs/decisions/0003-verification-stacks.md): Five complementary verification stacks.
- [ADR-0004](docs/decisions/0004-c-reference-as-golden.md): C-reference vectors are produced offline and committed.
- [ADR-0005](docs/decisions/0005-wear-leveling-pair-relocation.md): Inter-pair wear levelling via compact-time pair relocation.

### What the kernel does NOT do

- **Allocate.** The kernel takes two `&mut [u8]` buffers (each `S::BLOCK_SIZE` bytes) from the caller for every operation. There is no internal cache; `CACHE_SIZE` and `LOOKAHEAD_SIZE` on the `Storage` trait are advisory.
- **Use unsafe.** `#![forbid(unsafe_code)]` workspace-wide.
- **Pull in `std` or `alloc` by default.** The default feature set is empty; `alloc` and `std` are opt-in for richer hosts.
- **Use FFI.** Zero C in the dependency graph.

### Crate features

| Feature | Default | What it enables |
|---|---|---|
| `alloc` | off | `Vec`-backed read buffers on hosts with a global allocator. |
| `std` | off | `std::error::Error` for `Error`, `std::io` adapters around `Storage` (implies `alloc`). |
| `kani` | off | Compiles the formal verification harnesses under `src/verify/`. |

### Storage trait

```rust
pub trait Storage {
    type Error: core::fmt::Debug;
    const READ_SIZE: usize;
    const PROG_SIZE: usize;
    const BLOCK_SIZE: usize;
    const BLOCK_COUNT: u32;
    const BLOCK_CYCLES: i32 = 500;     // <= 0 disables wear levelling
    const CACHE_SIZE: usize;           // advisory
    const LOOKAHEAD_SIZE: usize;       // advisory

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), Self::Error>;
    fn erase(&mut self, block: u32) -> Result<(), Self::Error>;
    fn sync(&mut self) -> Result<(), Self::Error> { Ok(()) }
}
```

`NorAlignedStorage` wraps any `Storage` to convert byte-granular programs from the kernel into `PROG_SIZE`-aligned NOR-compliant programs.

## Repository layout

```
littlefs2-pure/
├── Cargo.toml                  workspace root, package metadata
├── README.md                   this file
├── INTEGRATION.md              integration walkthrough, mount-error matrix, recovery envelope
├── CHANGELOG.md                per-release notes (Keep a Changelog format)
├── KNOWN_ISSUES.md             punch list against v1.0
├── LICENSE-MIT / LICENSE-APACHE
├── docs/
│   ├── PLAN.md                 phase retrospective
│   └── decisions/              architecture decision records
├── src/                        the crate
├── tests/
│   ├── property_*.rs           proptest suites
│   ├── conformance.rs          mount C-reference vectors
│   ├── roundtrip.rs            mount our writes through the C verifier
│   ├── power_loss.rs           torn-write sweep
│   ├── atomic_move.rs          cross-dir rename torn-write sweep
│   ├── wear_leveling.rs        compact-time relocation sweep
│   └── vectors/                committed disk images from the C reference
├── tools/
│   ├── gen_vectors/            vendored C reference + driver producing baseline images
│   └── verify_image/           C verifier that mounts our images
└── fuzz/                       cargo-fuzz crate (libFuzzer, nightly-only, separate workspace)
```

## Building and testing

```sh
cargo test                      # full host test suite
cargo test --no-default-features  # no_std no_alloc floor
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Embedded targets
cargo build --target thumbv6m-none-eabi --no-default-features
cargo build --target thumbv8m.main-none-eabi --no-default-features
cargo build --target thumbv8m.main-none-eabihf --no-default-features

# Conformance against the C reference (vectors already committed)
cargo test --test conformance
cargo test --test roundtrip     # builds the C verifier via tools/verify_image/Makefile

# Optional: regenerate the C-reference vectors (requires a host C toolchain)
make -C tools/gen_vectors vectors

# Optional: Kani proofs
cargo kani --features kani

# Optional: fuzz (nightly only; see fuzz/README.md)
cd fuzz && cargo +nightly fuzz run meta_reader_parse
```

CI runs every gate above (except Kani and fuzz, which are local-only) on every push and pull request to `main`. See [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

## Versioning

This crate follows [Semantic Versioning](https://semver.org/). The `0.x` line is explicit about API churn; pin to an exact version (`= "0.1.0"`) if you need stability during the run-up to v1.0. Switch to caret ranges (`"^1"`) once v1.0 ships.

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/).

## License

Dual licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

The vendored C reference under `tools/gen_vectors/littlefs/` is BSD-3-Clause (upstream license preserved); it ships only as a host-side test fixture and is not part of the runtime crate.
