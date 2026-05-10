# ADR-0004: The C reference is invoked offline; vectors are committed

- **Status**: accepted
- **Date**: 2026-05-10

## Context

ADR-0001 forbids C in the dependency graph at runtime. ADR-0002 names the C reference as the bit level tie breaker. Those two together force a choice about *when* the C reference runs.

The options are:

1. Build the C reference in `build.rs` (or a build helper crate) at every `cargo build`. Adds `cc` to the dependency graph, breaks the no C toolchain promise.
2. Build the C reference in `tests/` at every `cargo test`. Same problem at test time; embedded CI without a C compiler cannot run the test suite.
3. Build the C reference offline in `tools/`, emit vectors into `tests/vectors/`, commit the vectors. `cargo test` reads the committed vectors, no C toolchain needed.

ferrodec uses option 3 with the General Decimal Arithmetic dectest vectors: the upstream publishes them once, ferrodec commits them, the runner reads committed text files.

## Decision

The C reference is consulted only via `tools/gen_vectors.sh` (or equivalent). The script:

- Pulls a pinned C reference revision (git submodule or vendored snapshot).
- Builds it with a stable host toolchain.
- Runs a fixed scenario set: format an image, populate a known directory tree, write files of known content, sync, unmount.
- Emits each resulting image into `tests/vectors/<scenario>.bin` along with a metadata sidecar `<scenario>.toml` listing the expected directory entries, file contents, and disk geometry.

The vectors are committed. The conformance runner reads `tests/vectors/`. `cargo test` runs anywhere `rustc` runs.

When the C reference is updated, `tools/gen_vectors.sh` is rerun by a human (or scheduled CI job), the diff is reviewed, and the regenerated vectors are committed. The diff is small in normal cases; a large diff signals either a C reference bug fix (we follow) or a spec clarification (we apply manually and document).

## Consequences

**Wins.**

- `cargo test` works on hosts without `cc`. The embedded CI matrix (cross compile to `thumbv6m-none-eabi`, run unit and property tests on the host) does not need a host C toolchain.
- Vector regeneration is auditable. Each refresh is a commit with a diff over `tests/vectors/`; review catches subtle output changes.
- Reproducibility: a vector committed in 2026 still asserts the same bit pattern in 2030. The crate's correctness claim is grounded in a fixed input, not a moving target.

**Costs.**

- `tests/vectors/` accumulates binary blobs in git history. Each vector is small (a few KB to a few MB depending on geometry); the aggregate stays manageable through Phase 4. If it exceeds tolerance, the migration target is git-lfs, documented in a follow up ADR.
- Vector regeneration is manual. A human runs the script, reviews the diff, and commits. The frequency is low (every few months at most), and the diff is the audit trail.
- The pinned C reference revision is itself a dependency, even if it lives in `tools/` not in `Cargo.toml`. Document the pin in `tools/README.md`.

**Explicitly out of scope.**

- Automatic vector regeneration in CI. The diff review step is intentional; an automated regeneration that auto commits would defeat the audit purpose.

## Related

- ADR-0001 (no FFI), ADR-0002 (spec as oracle), ADR-0003 (verification stacks).
- ferrodec `tests/conformance.rs` for the per file expected counts runner pattern.
