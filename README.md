# littlefs2-pure

A pure Rust, `no_std`, no-allocator implementation of the [LittleFS v2 on-disk format](https://github.com/littlefs-project/littlefs/blob/master/SPEC.md). No FFI, no `cc` build dependency, no global allocator inside the kernel. Bit-accurate against the C reference in both directions: images this crate writes mount cleanly through C littlefs, and images C littlefs writes mount cleanly through this crate.

```toml
[dependencies]
littlefs2-pure = "1"
```

License: MIT OR Apache-2.0. MSRV: Rust 1.84.

## How littlefs2-pure is developed

This is an open disclosure of the development process so users can judge for themselves whether the resulting code meets their bar.

**Authorship and collaboration.** Parnell Springmeyer is the author of record. littlefs2-pure is developed in collaboration with Claude, an AI coding agent from Anthropic. Parnell owns
architecture, acceptance criteria, test and verification strategy, and release boundaries. Claude drafts the implementation, writes and runs tests and verification harnesses, and produces
analysis under that direction. **Parnell does not review the generated code line by line.** Human oversight operates at the level of design, strategy, and outcomes: does the architecture make
sense, are the right invariants being checked, does the verification strategy cover the risk surface, do the tests and proofs pass. Merges to main are GPG signed by Parnell to attest to that
level of review, not to an audit of every line.

**Provenance.** Implementations derive from primary sources: the littlefs on disk format specification and the published design notes on power loss safety, dynamic wear leveling, and copy on
write metadata. The agent is instructed to cite recalled sources rather than reproduce verbatim, to surface provenance uncertainty rather than hide it, and to choose surface forms (identifiers,
helper decomposition, file layout) fresh for idiomatic Rust rather than copying from the upstream C reference (`BSD-3-Clause`), which serves as the oracle for on disk byte for byte compatibility.
These are instructions to the agent, not guarantees about every line of output. A verbatim reproduction or an unflagged derivation could slip through. The project's defense against that is the
instruction discipline above plus the human reviewer's ability to notice architectural smells that suggest a problem upstream, not a clean room audit. Where derivation from licensed source did
occur, attribution and license text are preserved. If you spot a passage that reads like a copy from a source it should not be copied from, please open an issue.

**Verification.** The verification posture places correctness in the type system where it can (typestate for mount and handle lifecycle, capability separation between read only and read write
paths, newtypes that prevent address and offset confusion at the boundary), in property tests over the file operation algebra, in example tests for known on disk images and edge cases, and in
crash simulation that cuts writes at block boundaries to check that the filesystem mounts and recovers. CI runs the usual lints and the full test suite; specific test counts and on disk
compatibility coverage change as the project evolves. Significant decisions are recorded as ADRs in the repo. `unsafe` blocks carry a written justification at the call site.

**Scope and threat model.** littlefs2-pure is a personal project. The intended use is flash backed storage on microcontrollers; durability and quality are goals, but this is not a funded library
with a maintenance team behind it. The threat model assumes an honest block device but a hostile power supply: every write can be interrupted, every block can be partially programmed, and a mount
after a crash is designed to converge on a consistent filesystem or report failure cleanly. The published versions on crates.io are yanked; the repository remains public for users who want to
read or fork the work.

**What this does not promise.** AI collaboration does not transfer responsibility. The author is accountable for what ships under his name. The disciplines above narrow the failure surface; they
do not eliminate it. In particular, this process is most exposed to subtle bugs that a careful human reading of the code would catch but tests, types, and crash simulation would not. For a
power-loss-safe filesystem that specifically includes crash sequences the simulation did not generate, or on-disk format drift from the upstream C reference that byte-level tests did not catch.
Issues are welcome and will be triaged as time allows; no SLA is offered. This README describes the project's development process and is not a warranty; see the LICENSE file for the legal terms
governing use.

## Status

**v1.2.0: API stable; the LittleFS v2 write surface is complete.** The kernel reads and writes the full LittleFS v2 on disk format, and the public API is covered by the semver contract. The v1.2.0 milestone completed the writer: directories grow across `HardTail` continuation pairs ([ADR-0013](docs/decisions/0013-directory-splitting.md)), every pair is threaded into the global list the C reference walks ([ADR-0012](docs/decisions/0012-softtail-global-list-threading.md)), and a single worn block under a metadata or file commit is relocated past rather than fatal ([ADR-0014](docs/decisions/0014-failure-driven-pair-relocation.md)). `#[non_exhaustive]` is applied to every spec tracking enum (`Error`, `EntryKind`, `AbstractType`, `TagType`) so future variants ship in 1.x minor releases without a major bump. See [`CHANGELOG.md`](CHANGELOG.md) for the per-release record and [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) for the design rationale and the remaining constraints.

Verification posture (see [ADR-0003](docs/decisions/0003-verification-stacks.md)):

| Stack | Coverage |
|---|---|
| Unit tests | Tight per-module invariants. |
| Property tests (`proptest`) | CRC, tag bit layout, CTZ geometry, metadata commit round-trip. |
| Conformance (C → Rust) | Mount C-littlefs-written images; assert expected entries. |
| Conformance (Rust → C) | Mount Rust-written images through a C verifier; assert expected content. |
| Power-loss sweep | `TornWriteStorage` / `TornWearStorage` / `TornBadBlock` across every program-call boundary in inline write, CTZ streaming append, cross-dir rename, wear-level pair relocation, directory splitting, and failure-driven bad-block relocation. |
| Kani harnesses | `Tag::from_bits` totality, `crc::update` vs bitwise reference, `rev_scmp` wrap-aware compare, `MetadataReader::new` panic-freedom. |
| libFuzzer (`fuzz/`) | Parser totality on adversarial bytes for tag, path, superblock, CTZ struct, metadata reader. |

The full host and `no_std` suites pass, clippy `-D warnings` is clean, `cargo doc` is warning-free under `RUSTDOCFLAGS=-D warnings`, and the three ARM cross-compile targets build. CI's whole matrix is green, including the advisory Kani proofs.

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
- [ADR-0012](docs/decisions/0012-softtail-global-list-threading.md): SoftTail global directory-list threading.
- [ADR-0013](docs/decisions/0013-directory-splitting.md): Write-side directory splitting across HardTail continuation pairs.
- [ADR-0014](docs/decisions/0014-failure-driven-pair-relocation.md): Failure-driven relocation of a metadata pair past a worn block.

ADRs 0006 through 0011 cover the Cortex-M0+ scratch budget, the cycle-safe tail walk, and the allocation and append performance work; the full series lives under [`docs/decisions/`](docs/decisions/).

### What the kernel does NOT do

- **Allocate.** The kernel takes two `&mut [u8]` buffers (each `S::BLOCK_SIZE` bytes) from the caller for every operation. There is no internal cache; `CACHE_SIZE` and `LOOKAHEAD_SIZE` on the `Storage` trait are advisory.
- **Use unsafe.** The `unsafe_code` lint is set to `forbid` in `[workspace.lints]`; the crate contains no `unsafe` blocks.
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
├── KNOWN_ISSUES.md             feature checklist and design constraints
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
│   ├── dir_split_*.rs          directory-split crash + relocation sweeps
│   ├── badblock_*.rs           failure-driven bad-block relocation + crash sweep
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

CI runs every gate above on every push and pull request to `main`, plus the Kani proofs and a fuzz smoke run as advisory (non-gating) jobs. See [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

## Versioning

This crate follows [Semantic Versioning](https://semver.org/). v1.x is the current line; depend on it with a caret range (`"1"`). Additive features ship as minor bumps (the LittleFS v2 write surface landed across the 1.x minors), and `#[non_exhaustive]` on the spec tracking enums keeps a new variant out of the breaking set, so a 2.0 would mean a deliberate API break.

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/).

## License

Dual licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

The vendored C reference under `tools/gen_vectors/littlefs/` is BSD-3-Clause (upstream license preserved); it ships only as a host-side test fixture and is not part of the runtime crate.
