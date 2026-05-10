# ADR-0001: Pure Rust, no FFI to the C reference

- **Status**: accepted
- **Date**: 2026-05-10

## Context

The `littlefs2` ecosystem on crates.io is dominated by FFI wrappers around the [C reference](https://github.com/littlefs-project/littlefs). The `littlefs2` and `littlefs2-sys` crates are bindgen layers; the dependent ecosystem inherits the C build (cc crate, cross compilation toolchain mismatches, opaque pointer arithmetic, unsafe at every call). Several pure Rust starts exist on personal forks but none are complete or maintained.

LittleFS v2 is a load bearing piece of infrastructure for embedded Rust projects (Trussed, Tock, the SMIL calculator that motivates this work). The format is stable; the spec is fixed at version 2.x. A pure Rust implementation is feasible and pays back for every downstream consumer in build time, target portability, audit surface, and the absence of `unsafe`.

## Decision

`littlefs2-pure` is implemented entirely in safe Rust. No FFI, no `cc` build dependency, no `bindgen`. The C reference is consulted for behavior; bytes on disk match what the C reference would write. The C reference is invoked only at vector generation time in `tools/`, never at runtime or in the dependency graph of the crate.

The workspace lint config sets `unsafe_code = "forbid"`.

## Consequences

**Wins.**

- No C toolchain in the dependency graph. `cargo build` is the only command needed; cross compilation to any Rust target works without extra setup.
- Every line of the implementation is auditable in Rust. No translation tax when reading. No `unsafe` block to argue about.
- The crate compiles on Cortex M0+ (`thumbv6m-none-eabi`) without `cc` or a host C compiler, which removes a class of build failures on embedded CI.
- Optimizer convergence over time: the gap to the C reference closes as `rustc` and LLVM improve. The FFI tax is permanent until removed.

**Costs.**

- Performance against the C reference is unknown at v0.1 and may lag in the short term. The plan defers the perf pass until after correctness lands; ferrodec's ADR-0006 establishes the precedent.
- Vector generation requires the C reference toolchain installed wherever vectors are regenerated. That cost lives in `tools/`, not in the crate's dependency graph.
- Some idioms from the C reference (pointer arithmetic, struct overlays) translate to enums and pattern matches in Rust. The translation takes engineering effort that an FFI wrapper avoids.

**Explicitly out of scope.**

- Performance parity with the C reference at v1.0. The target is correctness and complete spec coverage. Performance is a follow up phase with its own ADR.
- Optional unsafe acceleration paths. If a v2.x of the crate needs them, that is a future ADR; v1.0 forbids unsafe.

## Related

- Parnell's global CLAUDE.md: "Pure Rust beats FFI even at a performance cost: the Rust optimizer narrows the gap over time, the build system tax of FFI is permanent until removed."
- ferrodec's posture: pure Rust IEEE 754 decimal, no FFI to IBM decNumber or Intel BID-XML.
